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
        self.append_runtime_log(
            "runtime_ble",
            "attempting device connect",
            None,
            None,
            None,
            None,
        );
        let mut device = self.device.lock().await;
        match device.connect().await {
            Ok(_) => {
                self.append_runtime_log(
                    "runtime_ble",
                    "device connect succeeded",
                    None,
                    None,
                    None,
                    None,
                );
                Ok(())
            }
            Err(err) => {
                self.append_runtime_log(
                    "runtime_ble",
                    &format!(
                        "device connect failed: code={}, message={}",
                        err.code, err.message
                    ),
                    Some(&err.code),
                    None,
                    None,
                    None,
                );
                Err(err)
            }
        }
    }

    async fn sync_effective_mode(&self, force_write: bool) -> AppResult<()> {
        let effective = {
            let router = self.router.lock().await;
            router.effective_mode(Utc::now())
        };
        self.append_runtime_log(
            "runtime_ble",
            &format!(
                "sync_effective_mode started: effective={}, force_write={force_write}",
                effective.as_str()
            ),
            None,
            None,
            None,
            Some(effective),
        );

        let mut device = self.device.lock().await;
        let health = device.health().await;
        if !health.connected {
            self.append_runtime_log(
                "runtime_ble",
                "device was disconnected during sync, reconnecting before write",
                None,
                None,
                None,
                Some(effective),
            );
            device.connect().await?;
        }

        // BLE 写入要做节流：只有 effective mode 真正变化时才写设备。
        // 但如果设备刚刚重连，即使 mode 没变化，也必须强制补写一次当前 effective mode。
        let mut last_applied = self.last_applied_mode.lock().await;
        if !force_write && health.connected && last_applied.is_some_and(|mode| mode == effective) {
            self.append_runtime_log(
                "runtime_ble",
                "skipped BLE write because effective mode did not change",
                None,
                None,
                None,
                Some(effective),
            );
            return Ok(());
        }

        device.write_mode(effective).await?;
        *last_applied = Some(effective);
        *self.last_ble_write_at.lock().await = Some(Utc::now());
        self.append_runtime_log(
            "runtime_ble",
            "BLE write succeeded and last_applied_mode updated",
            None,
            None,
            None,
            Some(effective),
        );
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

    fn append_runtime_log(
        &self,
        kind: &str,
        message: &str,
        code: Option<&str>,
        source: Option<&str>,
        session: Option<&str>,
        mode: Option<Mode>,
    ) {
        // runtime 日志用于记录链路节点，采用“尽力写入”策略；
        // 即使日志失败，也不能影响 daemon 对外处理状态请求。
        let _ = self.log.append_runtime(LogEvent {
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
        });
    }

    async fn handle_send(&self, request_id: &str, payload: SendPayload) -> IpcResponseEnvelope {
        self.append_runtime_log(
            "runtime_ipc_send",
            &format!(
                "daemon received send request: request_id={request_id}, raw_event={:?}, raw_tool={:?}, turn={:?}",
                payload.raw_event, payload.raw_tool, payload.turn
            ),
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(payload.mode),
        );
        let now = Utc::now();
        let effective = {
            let mut router = self.router.lock().await;
            router.apply_send(&payload, now)
        };
        self.append_runtime_log(
            "runtime_router",
            &format!(
                "router applied state and resolved effective mode={}",
                effective.as_str()
            ),
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(effective),
        );

        let _ = self.append_log(
            "ipc_send",
            "accepted state update",
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(payload.mode),
        );

        if let Err(err) = self.sync_effective_mode(false).await {
            self.append_runtime_log(
                "runtime_ble",
                &format!(
                    "sync_effective_mode failed: code={}, message={}",
                    err.code, err.message
                ),
                Some(&err.code),
                Some(&payload.source),
                Some(&payload.session),
                Some(effective),
            );
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

        self.append_runtime_log(
            "runtime_ipc_send",
            "daemon completed send request successfully",
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(effective),
        );

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

// 测试实现拆到独立目录，避免与 daemon 状态机和设备协同主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../tests/core/daemon_tests.rs"]
mod tests;
