//! daemon 核心执行模块。
//!
//! 这个模块负责把来自 Hook 的 IPC 请求变成稳定的内部状态，并且作为唯一持有
//! `LightDevice` 的进程，统一管理 BLE 连接、重连、TTL 过期和日志记录。

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, sleep};

use crate::model::{
    AppError, AppResult, DeviceHealth, IpcRequestEnvelope, IpcRequestPayload, IpcResponseEnvelope,
    LogEvent, Mode, SendPayload, StatusResponse,
};
use crate::ports::device::LightDevice;
use crate::ports::ipc::{IpcRequestHandler, IpcServer};
use crate::ports::log::EventLog;
use crate::ports::runtime::RuntimeStore;
use crate::router::StateRouter;

/// daemon 是唯一持有 LightDevice 的进程。
/// 所有 Hook 事件都要先经过 IPC 进入这里，再由这里统一做优先级路由和 BLE 写入。
pub struct Daemon {
    /// 核心状态路由器，负责多来源状态合并与优先级决策。
    router: Mutex<StateRouter>,
    /// BLE 设备句柄只能由 daemon 独占，避免多个进程同时写灯。
    device: Mutex<Box<dyn LightDevice>>,
    /// runtime 文件管理器，负责 pid / ipc / manifest 等落盘。
    runtime: Arc<dyn RuntimeStore>,
    /// 结构化事件日志输出。
    log: Arc<dyn EventLog>,
    /// 用于后台任务和 server 协调退出。
    shutdown_tx: watch::Sender<bool>,
    /// 最近一次成功写 BLE 的时间，提供给 `status --verbose`。
    last_ble_write_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    /// 最近一次真正写到设备上的模式，用于去重和重连补写。
    last_applied_mode: Mutex<Option<Mode>>,
}

impl Daemon {
    pub fn new(
        runtime: Arc<dyn RuntimeStore>,
        log: Arc<dyn EventLog>,
        device: Box<dyn LightDevice>,
    ) -> Arc<Self> {
        // daemon 以 `Arc<Self>` 形式返回，便于同时交给 IPC server、
        // 过期清理任务和重连任务共享。
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            router: Mutex::new(StateRouter::new()),
            device: Mutex::new(device),
            runtime,
            log,
            shutdown_tx,
            last_ble_write_at: Mutex::new(None),
            last_applied_mode: Mutex::new(None),
        })
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub async fn run(self: Arc<Self>, server: Arc<dyn IpcServer>) -> AppResult<()> {
        // daemon 启动时先写入运行时元信息，便于命令层自动发现与自恢复。
        self.runtime.ensure_layout()?;
        self.runtime.write_pid(std::process::id())?;
        self.runtime.write_ipc_info(&server.info())?;
        self.append_log("daemon", "daemon started", None, None, None, None)?;

        if let Err(err) = self.try_connect_device().await {
            self.append_log(
                "ble",
                "initial device connect failed",
                Some(&err.code),
                None,
                None,
                None,
            )?;
        }

        let expiry_task = {
            let daemon = self.clone();
            tokio::spawn(async move {
                daemon.expiry_loop().await;
            })
        };
        let reconnect_task = {
            let daemon = self.clone();
            tokio::spawn(async move {
                daemon.reconnect_loop().await;
            })
        };

        let serve_result = server.serve(self.clone(), self.shutdown_receiver()).await;

        // 一旦 server 退出，无论是正常 stop 还是异常退出，都要通知后台任务收尾。
        let _ = self.shutdown_tx.send(true);
        let _ = expiry_task.await;
        let _ = reconnect_task.await;

        {
            let mut device = self.device.lock().await;
            // 退出时尽量把灯灭掉；如果失败，也不阻止进程退出。
            let _ = device.write_mode(Mode::Off).await;
        }

        let _ = self.runtime.clear_pid();
        let _ = self.runtime.clear_ipc_info();
        let _ = self.append_log("daemon", "daemon stopped", None, None, None, None);

        serve_result
    }

    async fn expiry_loop(self: Arc<Self>) {
        let mut shutdown = self.shutdown_receiver();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        break;
                    }
                }
                _ = sleep(Duration::from_secs(1)) => {
                    let now = Utc::now();
                    let before = {
                        let router = self.router.lock().await;
                        router.effective_mode(now)
                    };
                    {
                        let mut router = self.router.lock().await;
                        router.prune_expired(now);
                    }
                    let after = {
                        let router = self.router.lock().await;
                        router.effective_mode(now)
                    };
                    if before != after {
                        // 这里只是普通过期切换，不是重连场景；
                        // 因此保持去重写入，避免每次 TTL 清理都重复打 BLE。
                        let _ = self.sync_effective_mode(false).await;
                    }
                }
            }
        }
    }

    async fn reconnect_loop(self: Arc<Self>) {
        let mut shutdown = self.shutdown_receiver();
        // 退避序列从短到长，尽快恢复，也避免长时间断连时疯狂扫描蓝牙。
        let backoff = [1_u64, 2, 5, 10, 30];
        let mut index = 0_usize;

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        break;
                    }
                }
                _ = sleep(Duration::from_secs(backoff[index])) => {
                    let health = {
                        let device = self.device.lock().await;
                        device.health().await
                    };
                    if health.connected {
                        index = 0;
                        continue;
                    }

                    match self.try_connect_device().await {
                        Ok(()) => {
                            index = 0;
                            let _ = self.sync_effective_mode(true).await;
                        }
                        Err(err) => {
                            let _ = self.append_log("ble", "device reconnect failed", Some(&err.code), None, None, None);
                            index = (index + 1).min(backoff.len() - 1);
                        }
                    }
                }
            }
        }
    }

    async fn try_connect_device(&self) -> AppResult<()> {
        // 连接能力完全委托给 device adapter，本层只关心成功或失败。
        let mut device = self.device.lock().await;
        device.connect().await.map(|_| ())
    }

    async fn sync_effective_mode(&self, force_write: bool) -> AppResult<()> {
        let effective = {
            let router = self.router.lock().await;
            router.effective_mode(Utc::now())
        };

        let mut device = self.device.lock().await;
        let health = device.health().await;
        if !health.connected {
            device.connect().await?;
        }

        // BLE 写入要做节流：只有 effective mode 真正变化时才写设备。
        // 但如果设备刚刚重连，即使 mode 没变化，也必须强制补写一次当前 effective mode。
        let mut last_applied = self.last_applied_mode.lock().await;
        if !force_write && health.connected && last_applied.is_some_and(|mode| mode == effective) {
            return Ok(());
        }

        device.write_mode(effective).await?;
        *last_applied = Some(effective);
        *self.last_ble_write_at.lock().await = Some(Utc::now());
        Ok(())
    }

    fn append_log(
        &self,
        kind: &str,
        message: &str,
        code: Option<&str>,
        source: Option<&str>,
        session: Option<&str>,
        mode: Option<Mode>,
    ) -> AppResult<()> {
        // 统一封装日志构造，避免不同调用点随手写出结构不一致的日志。
        self.log.append(LogEvent {
            timestamp: Utc::now(),
            level: if code.is_some() {
                "warn".into()
            } else {
                "info".into()
            },
            kind: kind.into(),
            message: message.into(),
            code: code.map(ToOwned::to_owned),
            source: source.map(ToOwned::to_owned),
            session: session.map(ToOwned::to_owned),
            mode,
        })
    }

    async fn handle_send(&self, request_id: &str, payload: SendPayload) -> IpcResponseEnvelope {
        let now = Utc::now();
        let effective = {
            let mut router = self.router.lock().await;
            router.apply_send(&payload, now)
        };

        let _ = self.append_log(
            "ipc_send",
            "accepted state update",
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(payload.mode),
        );

        if let Err(err) = self.sync_effective_mode(false).await {
            // 路由状态已经接受成功，但 BLE 临时不可用时仍要把“已接受”告诉调用方，
            // 同时附带明确错误码，供 `--strict` 场景感知失败。
            let _ = self.append_log(
                "ble",
                "failed to sync effective mode",
                Some(&err.code),
                Some(&payload.source),
                Some(&payload.session),
                Some(effective),
            );
            return IpcResponseEnvelope::error(request_id.to_string(), &err).with_data(json!({
                "accepted": true,
                "effective": effective,
                "queued": true,
            }));
        }

        IpcResponseEnvelope::ok(request_id.to_string(), "accepted").with_data(json!({
            "effective": effective,
        }))
    }

    async fn handle_status(&self, request_id: &str, verbose: bool) -> IpcResponseEnvelope {
        let now = Utc::now();
        let sources = {
            let mut router = self.router.lock().await;
            router.prune_expired(now);
            // verbose 模式才返回每个来源明细，避免普通 `status` 输出过大。
            if verbose {
                Some(router.snapshot(now))
            } else {
                None
            }
        };
        let effective = {
            let router = self.router.lock().await;
            router.effective_mode(now)
        };
        let health: DeviceHealth = {
            let device = self.device.lock().await;
            device.health().await
        };

        let response = StatusResponse {
            daemon: "running".into(),
            ble: if health.connected {
                "connected".into()
            } else {
                "disconnected".into()
            },
            device: health.device_name.clone(),
            mode: effective,
            effective,
            sources,
            runtime_dir: Some(self.runtime.runtime_root().to_string_lossy().to_string()),
            ipc: self
                .runtime
                .read_ipc_info()
                .ok()
                .flatten()
                .map(|info| info.kind),
            last_ble_write_at: *self.last_ble_write_at.lock().await,
        };

        match serde_json::to_value(response) {
            Ok(data) => IpcResponseEnvelope::ok(request_id.to_string(), "ok").with_data(data),
            Err(err) => IpcResponseEnvelope::error(
                request_id.to_string(),
                &AppError::invalid("serialize status response", err),
            ),
        }
    }

    async fn handle_stop(&self, request_id: &str) -> IpcResponseEnvelope {
        // stop 采用异步通知模式：先返回 stopping，再由 run() 收尾并真正退出。
        let _ = self.append_log("daemon", "stop requested", None, None, None, None);
        let _ = self.shutdown_tx.send(true);
        IpcResponseEnvelope::ok(request_id.to_string(), "stopping")
    }
}

#[async_trait::async_trait]
impl IpcRequestHandler for Daemon {
    async fn handle(&self, req: IpcRequestEnvelope) -> IpcResponseEnvelope {
        // IPC server 完全不理解业务，只把请求透传到这里做统一分发。
        let request_id = req.request_id.clone();
        match req.payload {
            IpcRequestPayload::Send(payload) => self.handle_send(&request_id, payload).await,
            IpcRequestPayload::Status { verbose } => self.handle_status(&request_id, verbose).await,
            IpcRequestPayload::Stop => self.handle_stop(&request_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
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
    async fn hook_state_flow_matches_expected_priority_and_turn_rules() {
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
        // 同一 turn 的 success 不应覆盖尚未过期的 error。
        assert_eq!(status_json["effective"], json!("error"));
        assert_eq!(status_json["sources"][0]["mode"], json!("error"));

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
        // 新一轮 thinking 允许覆盖旧失败态，表示任务重新开始。
        assert_eq!(status_json["effective"], json!("thinking"));
        assert_eq!(status_json["sources"][0]["turn"], json!("turn-2"));

        let state = device_state.lock().await;
        assert_eq!(state.writes, vec![Mode::Busy, Mode::Error, Mode::Thinking]);
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

        // 第一步先模拟 Claude 发起权限请求，状态应立即进入 alarm。
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

        // 第二步模拟用户完成选择后，Claude 继续发出 PostToolBatch。
        // 这正是现场里最容易漏装 Hook 的恢复事件；若恢复正常，状态必须尽快离开 alarm。
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
}
