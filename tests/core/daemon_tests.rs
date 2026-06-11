//! `daemon` 模块测试。
//!
//! 这些测试覆盖 daemon 的状态流转、日志落盘与 Hook 仿真链路。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;

use super::*;
use crate::adapters::device::mock::MockLightDevice;
use crate::adapters::log::jsonl::JsonlLogAdapter;
use crate::adapters::runtime::fs::FsRuntimeAdapter;
use crate::model::{AppError, DeviceInfo, InstallManifest, IpcInfo};
use crate::ports::log::EventLog;
use crate::ports::runtime::RuntimeStore;
use crate::ports::source::SourceAdapterRegistry;
use crate::router::resolve_mode;
use crate::{adapters, model::HookParseContext};

#[derive(Debug, Default)]
struct DeviceState {
    connected: bool,
    writes: Vec<Mode>,
    fail_write: bool,
}

struct TestLightDevice {
    state: Arc<Mutex<DeviceState>>,
}

#[async_trait]
impl LightDevice for TestLightDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        let mut state = self.state.lock().await;
        state.connected = true;
        Ok(DeviceInfo {
            name: "test-device".into(),
            id: "test".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        let mut state = self.state.lock().await;
        if state.fail_write {
            return Err(AppError::new("ble_write_failed", "simulated write failure"));
        }
        state.writes.push(mode);
        Ok(())
    }

    async fn read_mode(&mut self) -> AppResult<Option<Mode>> {
        let state = self.state.lock().await;
        Ok(state.writes.last().copied())
    }

    async fn health(&self) -> DeviceHealth {
        let state = self.state.lock().await;
        DeviceHealth {
            connected: state.connected,
            device_name: Some("test-device".into()),
            last_error: None,
            last_write_at: None,
            last_mode: state.writes.last().copied(),
        }
    }
}

struct TestRuntimeStore;

impl RuntimeStore for TestRuntimeStore {
    fn runtime_root(&self) -> PathBuf {
        PathBuf::from("/tmp/esp-test")
    }

    fn runtime_dir(&self) -> PathBuf {
        self.runtime_root().join("runtime")
    }

    fn bin_dir(&self) -> PathBuf {
        self.runtime_root().join("bin")
    }

    fn events_log_path(&self) -> PathBuf {
        self.runtime_dir().join("events.log")
    }

    fn runtime_log_path(&self) -> PathBuf {
        self.runtime_dir().join("runtime.log")
    }

    fn install_manifest_path(&self, target: &str) -> PathBuf {
        self.runtime_root().join(format!("config.{target}.json"))
    }

    fn default_ipc_path(&self) -> PathBuf {
        self.runtime_dir().join("daemon.sock")
    }

    fn ensure_layout(&self) -> AppResult<()> {
        Ok(())
    }

    fn read_pid(&self) -> AppResult<Option<u32>> {
        Ok(None)
    }

    fn write_pid(&self, _pid: u32) -> AppResult<()> {
        Ok(())
    }

    fn clear_pid(&self) -> AppResult<()> {
        Ok(())
    }

    fn read_ipc_info(&self) -> AppResult<Option<IpcInfo>> {
        Ok(None)
    }

    fn write_ipc_info(&self, _info: &IpcInfo) -> AppResult<()> {
        Ok(())
    }

    fn clear_ipc_info(&self) -> AppResult<()> {
        Ok(())
    }

    fn write_install_manifest(&self, _manifest: &InstallManifest) -> AppResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct TestEventLog;

impl EventLog for TestEventLog {
    fn append(&self, _event: LogEvent) -> AppResult<()> {
        Ok(())
    }

    fn append_runtime(&self, _event: LogEvent) -> AppResult<()> {
        Ok(())
    }

    fn tail(&self, _limit: usize) -> AppResult<Vec<LogEvent>> {
        Ok(Vec::new())
    }
}

fn build_daemon(device_state: Arc<Mutex<DeviceState>>) -> Arc<Daemon> {
    Daemon::new(
        Arc::new(TestRuntimeStore),
        Arc::new(TestEventLog),
        Box::new(TestLightDevice {
            state: device_state,
        }),
    )
}

fn send_payload(mode: Mode) -> SendPayload {
    SendPayload {
        mode,
        source: "codex".into(),
        session: "session-1".into(),
        ttl: Some(30),
        hook_id: Some("agent-status-light".into()),
        raw_event: None,
        raw_tool: None,
        capability: None,
        suggested_mode: None,
        cwd: None,
        turn: None,
    }
}

#[tokio::test]
async fn handle_send_returns_error_when_ble_write_fails() {
    let device_state = Arc::new(Mutex::new(DeviceState {
        connected: true,
        writes: Vec::new(),
        fail_write: true,
    }));
    let daemon = build_daemon(device_state);

    let response = daemon.handle_send("req-1", send_payload(Mode::Busy)).await;

    assert!(!response.ok);
    assert_eq!(response.code.as_deref(), Some("ble_write_failed"));
    assert_eq!(
        response.data.as_ref().and_then(|data| data.get("accepted")),
        Some(&json!(true))
    );
    assert_eq!(
        response.data.as_ref().and_then(|data| data.get("queued")),
        Some(&json!(true))
    );
}

#[tokio::test]
async fn sync_effective_mode_force_write_reapplies_same_mode() {
    let device_state = Arc::new(Mutex::new(DeviceState {
        connected: true,
        writes: Vec::new(),
        fail_write: false,
    }));
    let daemon = build_daemon(device_state.clone());

    let response = daemon.handle_send("req-2", send_payload(Mode::Ai)).await;
    assert!(response.ok);

    daemon
        .sync_effective_mode(true)
        .await
        .expect("force write should succeed");

    let state = device_state.lock().await;
    assert_eq!(state.writes, vec![Mode::Ai, Mode::Ai]);
}

#[tokio::test]
async fn hook_state_flow_uses_latest_state_within_same_session() {
    let device_state = Arc::new(Mutex::new(DeviceState {
        connected: true,
        writes: Vec::new(),
        fail_write: false,
    }));
    let daemon = build_daemon(device_state.clone());

    let busy = SendPayload {
        mode: Mode::Busy,
        source: "cursor".into(),
        session: "conv-1".into(),
        ttl: Some(1800),
        hook_id: Some("agent-status-light".into()),
        raw_event: Some("beforeShellExecution".into()),
        raw_tool: Some("Shell".into()),
        capability: Some(crate::model::AgentCapability::RunningCommand),
        suggested_mode: Some(Mode::Busy),
        cwd: Some("/tmp/project".into()),
        turn: Some("turn-1".into()),
    };
    let response = daemon.handle_send("flow-1", busy).await;
    assert!(response.ok);

    let status = daemon.handle_status("status-1", true).await;
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("busy"));
    assert_eq!(
        status_json["sources"][0]["raw_event"],
        json!("beforeShellExecution")
    );
    assert_eq!(status_json["sources"][0]["turn"], json!("turn-1"));

    let error = SendPayload {
        mode: Mode::Error,
        source: "cursor".into(),
        session: "conv-1".into(),
        ttl: Some(600),
        hook_id: Some("agent-status-light".into()),
        raw_event: Some("postToolUseFailure".into()),
        raw_tool: Some("Shell".into()),
        capability: Some(crate::model::AgentCapability::Failed),
        suggested_mode: Some(Mode::Error),
        cwd: Some("/tmp/project".into()),
        turn: Some("turn-1".into()),
    };
    let response = daemon.handle_send("flow-2", error).await;
    assert!(response.ok);

    let same_turn_success = SendPayload {
        mode: Mode::Success,
        source: "cursor".into(),
        session: "conv-1".into(),
        ttl: Some(30),
        hook_id: Some("agent-status-light".into()),
        raw_event: Some("stop".into()),
        raw_tool: None,
        capability: Some(crate::model::AgentCapability::Succeeded),
        suggested_mode: Some(Mode::Success),
        cwd: Some("/tmp/project".into()),
        turn: Some("turn-1".into()),
    };
    let response = daemon.handle_send("flow-3", same_turn_success).await;
    assert!(response.ok);

    let status = daemon.handle_status("status-2", true).await;
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("success"));
    assert_eq!(status_json["sources"][0]["mode"], json!("success"));

    let new_round_thinking = SendPayload {
        mode: Mode::Thinking,
        source: "cursor".into(),
        session: "conv-1".into(),
        ttl: Some(900),
        hook_id: Some("agent-status-light".into()),
        raw_event: Some("beforeSubmitPrompt".into()),
        raw_tool: None,
        capability: Some(crate::model::AgentCapability::Thinking),
        suggested_mode: Some(Mode::Thinking),
        cwd: Some("/tmp/project".into()),
        turn: Some("turn-2".into()),
    };
    let response = daemon.handle_send("flow-4", new_round_thinking).await;
    assert!(response.ok);

    let status = daemon.handle_status("status-3", true).await;
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("thinking"));
    assert_eq!(status_json["sources"][0]["turn"], json!("turn-2"));

    let state = device_state.lock().await;
    assert_eq!(
        state.writes,
        vec![Mode::Busy, Mode::Error, Mode::Success, Mode::Thinking]
    );
}

fn temp_runtime_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("esp-sim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn build_hook_request(
    registry: &SourceAdapterRegistry,
    source: &str,
    explicit_mode: Mode,
    input: Value,
) -> IpcRequestEnvelope {
    let current_dir = PathBuf::from("/tmp/project");
    let ctx = HookParseContext {
        source: source.into(),
        explicit_mode,
        current_dir: current_dir.clone(),
        ttl: None,
    };
    let event = registry.parse_or_fallback(input, &ctx);
    let resolved_mode = resolve_mode(&ctx, &event);
    let payload = SendPayload {
        mode: resolved_mode,
        source: source.into(),
        session: event.session.clone(),
        ttl: explicit_mode.default_ttl().map(|ttl| ttl.as_secs()),
        hook_id: Some("agent-status-light".into()),
        raw_event: event.raw_event.clone(),
        raw_tool: event.raw_tool.clone(),
        capability: Some(event.capability.clone()),
        suggested_mode: event.suggested_mode,
        cwd: event
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().to_string()),
        turn: event.turn.clone(),
    };
    IpcRequestEnvelope::new(IpcRequestPayload::Send(payload))
}

#[tokio::test]
async fn simulated_hook_trigger_updates_status_and_logs() {
    let runtime_root = temp_runtime_root("hook-flow");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let log: Arc<dyn EventLog> = Arc::new(JsonlLogAdapter::new(runtime.clone()));
    let daemon = Daemon::new(
        runtime.clone(),
        log.clone(),
        Box::new(MockLightDevice::default()),
    );
    let registry = adapters::source::registry();

    let thinking = build_hook_request(
        &registry,
        "cursor",
        Mode::Thinking,
        serde_json::json!({
            "conversationId": "conv-1",
            "hookEventName": "beforeSubmitPrompt",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(thinking).await;
    assert!(response.ok);

    let busy = build_hook_request(
        &registry,
        "cursor",
        Mode::Busy,
        serde_json::json!({
            "conversationId": "conv-1",
            "hookEventName": "beforeShellExecution",
            "command": "npm test",
            "cwd": "/tmp/project",
            "toolUseId": "turn-1"
        }),
    );
    let response = daemon.handle(busy).await;
    assert!(response.ok);

    let error = build_hook_request(
        &registry,
        "cursor",
        Mode::Error,
        serde_json::json!({
            "conversationId": "conv-1",
            "hookEventName": "postToolUseFailure",
            "failureType": "command_error",
            "cwd": "/tmp/project",
            "toolUseId": "turn-1"
        }),
    );
    let response = daemon.handle(error).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data");
    assert_eq!(data["effective"], serde_json::json!("error"));
    assert_eq!(data["sources"][0]["source"], serde_json::json!("cursor"));
    assert_eq!(data["sources"][0]["session"], serde_json::json!("conv-1"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("postToolUseFailure")
    );
    assert_eq!(data["sources"][0]["turn"], serde_json::json!("turn-1"));

    let logs = log.tail(20).expect("read logs");
    let kinds = logs
        .iter()
        .map(|item| item.kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(kinds.iter().filter(|kind| **kind == "ipc_send").count(), 3);

    let stop = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Stop))
        .await;
    assert!(stop.ok);

    let final_logs = log.tail(50).expect("read final logs");
    assert!(
        final_logs
            .iter()
            .any(|item| item.message == "stop requested")
    );
    assert!(
        final_logs
            .iter()
            .any(|item| item.message == "accepted state update")
    );

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[tokio::test]
async fn simulated_alarm_flow_recovers_after_follow_up_hook() {
    let runtime_root = temp_runtime_root("alarm-flow");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let log: Arc<dyn EventLog> = Arc::new(JsonlLogAdapter::new(runtime.clone()));
    let daemon = Daemon::new(
        runtime.clone(),
        log.clone(),
        Box::new(MockLightDevice::default()),
    );
    let registry = adapters::source::registry();

    let alarm = build_hook_request(
        &registry,
        "claude",
        Mode::Alarm,
        serde_json::json!({
            "session_id": "session-alarm-1",
            "hook_event_name": "PermissionRequest",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(alarm).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data after alarm");
    assert_eq!(data["effective"], serde_json::json!("alarm"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("PermissionRequest")
    );

    let resume = build_hook_request(
        &registry,
        "claude",
        Mode::Busy,
        serde_json::json!({
            "session_id": "session-alarm-1",
            "hook_event_name": "PostToolBatch",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(resume).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data after resume");
    assert_eq!(data["effective"], serde_json::json!("busy"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("PostToolBatch")
    );
    assert_eq!(data["sources"][0]["mode"], serde_json::json!("busy"));

    let logs = log.tail(20).expect("read logs");
    assert_eq!(
        logs.iter().filter(|item| item.kind == "ipc_send").count(),
        2
    );

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[tokio::test]
async fn simulated_alarm_then_success_updates_effective_mode() {
    let runtime_root = temp_runtime_root("alarm-success");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let log: Arc<dyn EventLog> = Arc::new(JsonlLogAdapter::new(runtime.clone()));
    let daemon = Daemon::new(runtime.clone(), log, Box::new(MockLightDevice::default()));
    let registry = adapters::source::registry();

    let alarm = build_hook_request(
        &registry,
        "claude",
        Mode::Alarm,
        serde_json::json!({
            "session_id": "session-alarm-success-1",
            "hook_event_name": "PermissionRequest",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(alarm).await;
    assert!(response.ok);

    let success = build_hook_request(
        &registry,
        "claude",
        Mode::Success,
        serde_json::json!({
            "session_id": "session-alarm-success-1",
            "hook_event_name": "SessionEnd",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(success).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data after success");
    assert_eq!(data["effective"], serde_json::json!("success"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("SessionEnd")
    );
    assert_eq!(data["sources"][0]["mode"], serde_json::json!("success"));

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[test]
fn append_log_uses_warn_level_when_error_code_present() {
    let runtime = Arc::new(TestRuntimeStore);
    let captured = Arc::new(Mutex::new(Vec::<LogEvent>::new()));

    struct CapturingLog {
        captured: Arc<Mutex<Vec<LogEvent>>>,
    }

    impl EventLog for CapturingLog {
        fn append(&self, event: LogEvent) -> AppResult<()> {
            self.captured.blocking_lock().push(event);
            Ok(())
        }

        fn append_runtime(&self, event: LogEvent) -> AppResult<()> {
            self.captured.blocking_lock().push(event);
            Ok(())
        }

        fn tail(&self, _limit: usize) -> AppResult<Vec<LogEvent>> {
            Ok(Vec::new())
        }
    }

    let daemon = Daemon::new(
        runtime,
        Arc::new(CapturingLog {
            captured: captured.clone(),
        }),
        Box::new(TestLightDevice {
            state: Arc::new(Mutex::new(DeviceState::default())),
        }),
    );

    daemon
        .append_log(
            "ble",
            "write failed",
            Some("ble_write_failed"),
            Some("codex"),
            Some("session-1"),
            Some(Mode::Error),
        )
        .expect("append log should succeed");

    let items = captured.blocking_lock();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].level, "warn");
    assert_eq!(items[0].timestamp <= Utc::now(), true);
}
