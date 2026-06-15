//! `daemon` 模块测试。
//!
//! 这些测试覆盖 daemon 的状态流转、日志落盘与 Hook 仿真链路。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::task::yield_now;
use tokio::time::{sleep, timeout};

use super::*;
use crate::adapters::device::mock::MockLightDevice;
use crate::adapters::log::jsonl::JsonlLogAdapter;
use crate::adapters::runtime::fs::FsRuntimeAdapter;
use crate::model::{AppError, DeviceInfo, EventSemantics, InstallManifest, IpcInfo};
use crate::ports::ipc::{IpcRequestHandler, IpcServer};
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

#[derive(Debug, Default)]
struct HangingDeviceState {
    connected: bool,
    writes: Vec<Mode>,
}

/// 用来模拟“底层 BLE future 一直不返回”的设备。
///
/// 真实线上问题不是普通错误返回，而是某次底层调用长时间挂住，
/// 导致 daemon 看起来像是失联或退出。
/// 这个测试设备专门把 `write_mode` 卡住，验证 daemon 侧的超时与缓存逻辑。
struct HangingWriteDevice {
    state: Arc<Mutex<HangingDeviceState>>,
    write_started_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl LightDevice for HangingWriteDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        let mut state = self.state.lock().await;
        state.connected = true;
        Ok(DeviceInfo {
            name: "hanging-device".into(),
            id: "hanging".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        {
            let mut state = self.state.lock().await;
            state.writes.push(mode);
        }
        if let Some(sender) = self.write_started_tx.lock().await.take() {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn health(&self) -> DeviceHealth {
        let state = self.state.lock().await;
        DeviceHealth {
            connected: state.connected,
            device_name: Some("hanging-device".into()),
            last_error: None,
            last_write_at: None,
            last_mode: state.writes.last().copied(),
        }
    }
}

#[derive(Debug, Default)]
struct SlowConnectDeviceState {
    connect_calls: usize,
}

struct SlowConnectDevice {
    state: Arc<Mutex<SlowConnectDeviceState>>,
    connect_started_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl LightDevice for SlowConnectDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        {
            let mut state = self.state.lock().await;
            state.connect_calls += 1;
        }
        if let Some(sender) = self.connect_started_tx.lock().await.take() {
            let _ = sender.send(());
        }
        sleep(Duration::from_millis(150)).await;
        Ok(DeviceInfo {
            name: "slow-connect-device".into(),
            id: "slow-connect".into(),
        })
    }

    async fn write_mode(&mut self, _mode: Mode) -> AppResult<()> {
        Ok(())
    }

    async fn health(&self) -> DeviceHealth {
        DeviceHealth {
            connected: false,
            device_name: Some("slow-connect-device".into()),
            last_error: None,
            last_write_at: None,
            last_mode: None,
        }
    }
}

#[derive(Debug, Default)]
struct IdleRefreshDeviceState {
    connect_calls: usize,
    health_connected: bool,
    writes: Vec<Mode>,
}

struct IdleRefreshDevice {
    state: Arc<Mutex<IdleRefreshDeviceState>>,
}

#[async_trait]
impl LightDevice for IdleRefreshDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        let mut state = self.state.lock().await;
        state.connect_calls += 1;
        state.health_connected = true;
        Ok(DeviceInfo {
            name: "idle-refresh-device".into(),
            id: "idle-refresh".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        let mut state = self.state.lock().await;
        state.writes.push(mode);
        Ok(())
    }

    async fn health(&self) -> DeviceHealth {
        let state = self.state.lock().await;
        DeviceHealth {
            connected: state.health_connected,
            device_name: Some("idle-refresh-device".into()),
            last_error: None,
            last_write_at: None,
            last_mode: state.writes.last().copied(),
        }
    }
}

#[derive(Debug, Default)]
struct RefreshBeforeWriteDeviceState {
    health_calls: usize,
    writes: Vec<Mode>,
}

struct RefreshBeforeWriteDevice {
    state: Arc<Mutex<RefreshBeforeWriteDeviceState>>,
    health_started_tx: Mutex<Option<oneshot::Sender<()>>>,
    allow_health_return_rx: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl LightDevice for RefreshBeforeWriteDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        Ok(DeviceInfo {
            name: "refresh-before-write-device".into(),
            id: "refresh-before-write".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        let mut state = self.state.lock().await;
        state.writes.push(mode);
        Ok(())
    }

    async fn health(&self) -> DeviceHealth {
        {
            let mut state = self.state.lock().await;
            state.health_calls += 1;
        }
        if let Some(sender) = self.health_started_tx.lock().await.take() {
            let _ = sender.send(());
        }
        if let Some(receiver) = self.allow_health_return_rx.lock().await.take() {
            let _ = receiver.await;
        }
        DeviceHealth {
            connected: true,
            device_name: Some("refresh-before-write-device".into()),
            last_error: None,
            last_write_at: None,
            last_mode: None,
        }
    }
}

struct ReadySignalServer {
    serve_started_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl IpcServer for ReadySignalServer {
    fn info(&self) -> IpcInfo {
        IpcInfo {
            kind: "test".into(),
            address: "ready-signal".into(),
            version: 1,
            started_at: Utc::now(),
        }
    }

    async fn serve(
        &self,
        _handler: Arc<dyn IpcRequestHandler>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()> {
        if let Some(sender) = self.serve_started_tx.lock().await.take() {
            let _ = sender.send(());
        }
        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                break;
            }
        }
        Ok(())
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
    // 大多数测试只关心 daemon 状态流转，因此统一复用这套最小可观测 fake 依赖装配。
    Daemon::new(
        Arc::new(TestRuntimeStore),
        Arc::new(TestEventLog),
        Box::new(TestLightDevice {
            state: device_state,
        }),
    )
}

fn build_hanging_write_daemon(
    device_state: Arc<Mutex<HangingDeviceState>>,
    write_started_tx: oneshot::Sender<()>,
) -> Arc<Daemon> {
    Daemon::new(
        Arc::new(TestRuntimeStore),
        Arc::new(TestEventLog),
        Box::new(HangingWriteDevice {
            state: device_state,
            write_started_tx: Mutex::new(Some(write_started_tx)),
        }),
    )
}

fn send_payload(mode: Mode) -> SendPayload {
    // 统一 helper 能避免各测试在 source/session/hook_id 这些样板字段上重复展开。
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
        semantics: EventSemantics::Unknown,
    }
}

async fn wait_for_writes(state: &Arc<Mutex<DeviceState>>, expected_len: usize) {
    for _ in 0..20 {
        if state.lock().await.writes.len() >= expected_len {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn handle_send_accepts_even_when_ble_write_fails_later() {
    let device_state = Arc::new(Mutex::new(DeviceState {
        connected: true,
        writes: Vec::new(),
        fail_write: true,
    }));
    let daemon = build_daemon(device_state);

    let response = daemon.handle_send("req-1", send_payload(Mode::Busy)).await;

    assert!(response.ok);
    assert_eq!(
        response.data.as_ref().and_then(|data| data.get("queued")),
        Some(&json!(true))
    );

    for _ in 0..10 {
        if daemon.cached_device_health().await.last_error.is_some() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let health = daemon.cached_device_health().await;
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("ble_write_failed"))
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
    wait_for_writes(&device_state, 1).await;

    daemon
        .sync_effective_mode(true)
        .await
        .expect("force write should succeed");

    let state = device_state.lock().await;
    assert_eq!(state.writes, vec![Mode::Ai, Mode::Ai]);
}

#[tokio::test]
async fn sync_effective_mode_refreshes_mode_before_ble_write() {
    let runtime_root = temp_runtime_root("refresh-before-write");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    runtime.ensure_layout().expect("layout should succeed");

    let device_state = Arc::new(Mutex::new(RefreshBeforeWriteDeviceState::default()));
    let (health_started_tx, health_started_rx) = oneshot::channel();
    let (allow_health_return_tx, allow_health_return_rx) = oneshot::channel();
    let daemon = Daemon::new(
        runtime.clone(),
        Arc::new(TestEventLog),
        Box::new(RefreshBeforeWriteDevice {
            state: device_state.clone(),
            health_started_tx: Mutex::new(Some(health_started_tx)),
            allow_health_return_rx: Mutex::new(Some(allow_health_return_rx)),
        }),
    );

    {
        let mut router = daemon.router.lock().await;
        router
            .apply_send(&send_payload(Mode::Busy), Utc::now())
            .expect("busy should apply");
    }

    let sync_daemon = daemon.clone();
    let sync_task = tokio::spawn(async move { sync_daemon.sync_effective_mode(false).await });
    health_started_rx
        .await
        .expect("sync should reach health probe before write");

    {
        let mut router = daemon.router.lock().await;
        router
            .apply_send(&send_payload(Mode::Ai), Utc::now())
            .expect("ai should apply");
    }

    let _ = allow_health_return_tx.send(());
    sync_task
        .await
        .expect("sync task should finish")
        .expect("sync should succeed");

    let state = device_state.lock().await;
    assert_eq!(state.writes, vec![Mode::Ai]);

    let _ = std::fs::remove_dir_all(runtime_root);
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
        semantics: EventSemantics::ExplicitToolExecution,
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
        semantics: EventSemantics::Failure,
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
        semantics: EventSemantics::Completion,
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
        semantics: EventSemantics::Continuation,
    };
    let response = daemon.handle_send("flow-4", new_round_thinking).await;
    assert!(response.ok);
    wait_for_writes(&device_state, 1).await;

    let status = daemon.handle_status("status-3", true).await;
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("thinking"));
    assert_eq!(status_json["sources"][0]["turn"], json!("turn-2"));

    let state = device_state.lock().await;
    assert_eq!(state.writes.last(), Some(&Mode::Thinking));
}

fn temp_runtime_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("esp-sim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[tokio::test]
async fn startup_lock_can_be_acquired_inside_tokio_runtime() {
    let runtime_root = temp_runtime_root("startup-lock");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    runtime.ensure_layout().expect("layout should succeed");

    let daemon = Daemon::new(
        runtime.clone(),
        Arc::new(TestEventLog),
        Box::new(MockLightDevice::default()),
    );

    daemon
        .acquire_startup_lock()
        .await
        .expect("startup lock should be acquired inside async runtime");
    assert!(runtime.runtime_dir().join("daemon.lock").exists());

    daemon
        .release_startup_lock()
        .await
        .expect("startup lock should be released inside async runtime");
    assert!(!runtime.runtime_dir().join("daemon.lock").exists());

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[test]
fn file_lock_owner_round_trips_json_and_legacy_pid_format() {
    let root = temp_runtime_root("lock-owner");
    std::fs::create_dir_all(&root).expect("create temp root");

    let json_lock = root.join("json.lock");
    let owner = crate::runtime_lock::LockOwner {
        pid: std::process::id(),
        token: Some("token-1".into()),
        start_signature: Some("started".into()),
    };
    std::fs::write(
        &json_lock,
        serde_json::to_string(&owner).expect("serialize lock owner"),
    )
    .expect("write json lock");

    let parsed = crate::runtime_lock::read_lock_owner(&json_lock).expect("parse json lock");
    assert_eq!(parsed, owner);

    let legacy_lock = root.join("legacy.lock");
    std::fs::write(&legacy_lock, format!("{}\n", std::process::id())).expect("write legacy lock");
    let parsed_legacy = crate::runtime_lock::read_lock_owner(&legacy_lock).expect("parse legacy");
    assert_eq!(parsed_legacy.pid, std::process::id());
    assert!(parsed_legacy.token.is_none());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn handle_send_returns_before_hanging_ble_write_finishes() {
    let device_state = Arc::new(Mutex::new(HangingDeviceState::default()));
    let (write_started_tx, write_started_rx) = oneshot::channel();
    let daemon = build_hanging_write_daemon(device_state.clone(), write_started_tx);

    daemon
        .try_connect_device()
        .await
        .expect("connect should initialize cached device health");

    let started = std::time::Instant::now();
    let response = daemon.handle_send("hang-1", send_payload(Mode::Busy)).await;

    assert!(
        started.elapsed() < Duration::from_millis(40),
        "send should return before waiting on the slow BLE write path"
    );
    assert!(response.ok);
    assert_eq!(
        response.data.as_ref().and_then(|data| data.get("queued")),
        Some(&json!(true))
    );

    write_started_rx
        .await
        .expect("background BLE write should still be attempted");

    let status = daemon.handle_status("hang-status", true).await;
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("busy"));
    assert_eq!(status_json["ble"], json!("disconnected"));

    let state = device_state.lock().await;
    assert_eq!(state.writes, vec![Mode::Busy]);
}

#[tokio::test]
async fn status_remains_responsive_while_ble_write_is_stuck() {
    let device_state = Arc::new(Mutex::new(HangingDeviceState::default()));
    let (write_started_tx, write_started_rx) = oneshot::channel();
    let daemon = build_hanging_write_daemon(device_state, write_started_tx);

    daemon
        .try_connect_device()
        .await
        .expect("connect should initialize cached device health");

    let send_daemon = daemon.clone();
    let send_response = send_daemon
        .handle_send("hang-2", send_payload(Mode::Ai))
        .await;
    assert!(send_response.ok);

    write_started_rx
        .await
        .expect("write should begin before probing status");
    yield_now().await;
    sleep(Duration::from_millis(5)).await;

    let status = timeout(
        Duration::from_millis(40),
        daemon.handle_status("hang-status-live", true),
    )
    .await
    .expect("status should not block on the device mutex while BLE write is hung");

    assert!(status.ok);
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["effective"], json!("ai"));
    assert_eq!(status_json["ble"], json!("disconnected"));

    let still_responsive = timeout(
        Duration::from_millis(40),
        daemon.handle_status("hang-status-final", true),
    )
    .await
    .expect("status should remain responsive while background BLE write is hung");
    assert!(still_responsive.ok);
}

#[tokio::test]
async fn status_reports_live_ble_health_instead_of_cached_snapshot() {
    let device_state = Arc::new(Mutex::new(DeviceState {
        connected: false,
        writes: Vec::new(),
        fail_write: false,
    }));
    let daemon = build_daemon(device_state.clone());

    daemon
        .try_connect_device()
        .await
        .expect("connect should initialize cached connected state");
    {
        let mut state = device_state.lock().await;
        state.connected = false;
    }

    let status = daemon.handle_status("live-health", false).await;
    assert!(status.ok);
    let status_json = status.data.expect("status should contain data");
    assert_eq!(status_json["ble"], json!("disconnected"));
}

#[tokio::test]
async fn daemon_status_is_ready_while_initial_connect_is_still_running() {
    let runtime_root = temp_runtime_root("slow-connect-startup");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let device_state = Arc::new(Mutex::new(SlowConnectDeviceState::default()));
    let (connect_started_tx, connect_started_rx) = oneshot::channel();
    let daemon = Daemon::new(
        runtime.clone(),
        Arc::new(TestEventLog),
        Box::new(SlowConnectDevice {
            state: device_state.clone(),
            connect_started_tx: Mutex::new(Some(connect_started_tx)),
        }),
    );
    let (serve_started_tx, serve_started_rx) = oneshot::channel();
    let server = Arc::new(ReadySignalServer {
        serve_started_tx: Mutex::new(Some(serve_started_tx)),
    });
    let daemon_for_server = daemon.clone();

    let serve_task = tokio::spawn(async move { daemon_for_server.run(server.clone()).await });

    serve_started_rx
        .await
        .expect("server should start before probing status");
    connect_started_rx
        .await
        .expect("initial connect should start in background");

    let status = timeout(
        Duration::from_millis(80),
        daemon.handle_status("slow-start-status", false),
    )
    .await
    .expect("status should be available before initial connect finishes");

    assert!(status.ok);
    let data = status.data.expect("status should contain data");
    assert_eq!(data["daemon"], json!("running"));
    assert_eq!(data["ble"], json!("disconnected"));

    let state = device_state.lock().await;
    assert_eq!(state.connect_calls, 1);
    drop(state);

    let stop = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Stop))
        .await;
    assert!(stop.ok);

    serve_task
        .await
        .expect("daemon task should join")
        .expect("daemon should stop cleanly");

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[tokio::test]
async fn idle_connection_refreshes_before_next_write() {
    let runtime_root = temp_runtime_root("idle-refresh");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let device_state = Arc::new(Mutex::new(IdleRefreshDeviceState::default()));
    let daemon = Daemon::new(
        runtime.clone(),
        Arc::new(TestEventLog),
        Box::new(IdleRefreshDevice {
            state: device_state.clone(),
        }),
    );

    daemon
        .sync_effective_mode(true)
        .await
        .expect("first write should succeed");

    {
        let mut state = device_state.lock().await;
        state.connect_calls = 0;
        state.health_connected = true;
    }
    {
        let mut last = daemon.last_ble_write_at.lock().await;
        if let Some(old) = *last {
            *last = Some(old - chrono::Duration::seconds(300));
        }
    }

    daemon
        .sync_effective_mode(false)
        .await
        .expect("idle refresh should reconnect before writing");

    let state = device_state.lock().await;
    assert_eq!(state.connect_calls, 1);
    assert_eq!(state.writes, vec![Mode::Demo, Mode::Demo]);

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[tokio::test]
async fn daemon_autostart_lock_waits_long_enough_for_existing_owner() {
    let runtime_root = temp_runtime_root("autostart-lock-wait");
    let lock_path = runtime_root.join("runtime").join("daemon-autostart.lock");
    std::fs::create_dir_all(lock_path.parent().expect("runtime dir")).expect("create runtime dir");
    let (lock_acquired_tx, lock_acquired_rx) = std::sync::mpsc::channel();

    let holder = std::thread::spawn({
        let lock_path = lock_path.clone();
        move || {
            let _guard = crate::runtime_lock::FileLock::acquire(&lock_path)
                .expect("holder should acquire lock");
            lock_acquired_tx
                .send(())
                .expect("notify holder acquired lock");
            std::thread::sleep(Duration::from_millis(250));
        }
    });

    lock_acquired_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("holder should acquire lock before main thread waits");
    let started = std::time::Instant::now();
    let acquired = crate::runtime_lock::FileLock::acquire_with_retry(
        &lock_path,
        60,
        Duration::from_millis(10),
    )
    .expect("lock should be acquired after holder releases");

    assert!(started.elapsed() >= Duration::from_millis(200));
    drop(acquired);
    holder.join().expect("holder should finish");

    let _ = std::fs::remove_dir_all(runtime_root);
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
        semantics: event.semantics,
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
async fn simulated_ai_generation_flow_survives_generic_busy_continuation() {
    let runtime_root = temp_runtime_root("ai-flow");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let log: Arc<dyn EventLog> = Arc::new(JsonlLogAdapter::new(runtime.clone()));
    let daemon = Daemon::new(
        runtime.clone(),
        log.clone(),
        Box::new(MockLightDevice::default()),
    );
    let registry = adapters::source::registry();

    // 第一步模拟 Claude 进入文件生成/写入阶段。
    // 这类事件应该明确落到 `ai`，对应“AI 正在生成内容/长任务处理中”的灯效。
    let ai = build_hook_request(
        &registry,
        "claude",
        Mode::Ai,
        serde_json::json!({
            "session_id": "session-ai-1",
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "cwd": "/tmp/project",
            "tool_input": {
                "file_path": "/tmp/project/src/main.rs"
            },
            "tool_use_id": "turn-ai-1"
        }),
    );
    let response = daemon.handle(ai).await;
    assert!(response.ok);

    // 第二步模拟真实现场里常见的 continuation 事件：
    // Claude 在单次工具完成后可能继续发出 `PostToolBatch`，它只表示“流程还在继续”，
    // 并不等价于“已经回到明确的命令执行态 busy”。
    // 因此这里期望 router 继续保留同会话内更有语义的 `ai`。
    let continuation = build_hook_request(
        &registry,
        "claude",
        Mode::Busy,
        serde_json::json!({
            "session_id": "session-ai-1",
            "hook_event_name": "PostToolBatch",
            "cwd": "/tmp/project"
        }),
    );
    let response = daemon.handle(continuation).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data after ai continuation");
    assert_eq!(data["effective"], serde_json::json!("ai"));
    assert_eq!(data["sources"][0]["mode"], serde_json::json!("ai"));
    assert_eq!(data["sources"][0]["raw_tool"], serde_json::json!("Write"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("PreToolUse")
    );

    // 运行日志里两类事件都应该能看到，方便后续排查为什么最终展示仍是 `ai`。
    let logs = log.tail(20).expect("read logs");
    assert!(
        logs.iter()
            .any(|item| { item.message == "accepted state update" && item.mode == Some(Mode::Ai) })
    );
    assert!(
        logs.iter().any(|item| {
            item.message == "accepted state update" && item.mode == Some(Mode::Busy)
        })
    );

    let _ = std::fs::remove_dir_all(runtime_root);
}

#[tokio::test]
async fn simulated_file_read_flow_maps_to_ai() {
    let runtime_root = temp_runtime_root("file-read-ai");
    let runtime: Arc<dyn RuntimeStore> = Arc::new(FsRuntimeAdapter::new(runtime_root.clone()));
    let log: Arc<dyn EventLog> = Arc::new(JsonlLogAdapter::new(runtime.clone()));
    let daemon = Daemon::new(
        runtime.clone(),
        log.clone(),
        Box::new(MockLightDevice::default()),
    );
    let registry = adapters::source::registry();

    // 仿真 Cursor 读取文件上下文。
    // 按最新产品规则，文件读取也属于 AI 内容处理流程，因此应直接展示为 `ai`。
    let read = build_hook_request(
        &registry,
        "cursor",
        Mode::Ai,
        serde_json::json!({
            "conversationId": "conv-read-1",
            "hookEventName": "beforeReadFile",
            "cwd": "/tmp/project",
            "toolUseId": "turn-read-1"
        }),
    );
    let response = daemon.handle(read).await;
    assert!(response.ok);

    let status = daemon
        .handle(IpcRequestEnvelope::new(IpcRequestPayload::Status {
            verbose: true,
        }))
        .await;
    assert!(status.ok);
    let data = status.data.expect("status data after beforeReadFile");
    assert_eq!(data["effective"], serde_json::json!("ai"));
    assert_eq!(data["sources"][0]["mode"], serde_json::json!("ai"));
    assert_eq!(
        data["sources"][0]["raw_event"],
        serde_json::json!("beforeReadFile")
    );
    assert_eq!(data["sources"][0]["turn"], serde_json::json!("turn-read-1"));

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
    assert!(items[0].timestamp <= Utc::now());
    assert_eq!(items[0].phase, None);
    assert_eq!(items[0].context, None);
}
