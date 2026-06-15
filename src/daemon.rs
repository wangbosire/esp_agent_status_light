//! daemon 核心执行模块。
//!
//! 这个模块负责把来自 Hook 的 IPC 请求变成稳定的内部状态，并且作为唯一持有
//! `LightDevice` 的进程，统一管理 BLE 连接、重连、TTL 过期和日志记录。

use std::sync::{Arc, Weak};

use chrono::Utc;
use serde_json::json;
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, sleep, timeout};

use crate::model::{
    AppError, AppResult, DeviceHealth, IpcRequestEnvelope, IpcRequestPayload, IpcResponseEnvelope,
    LogEvent, Mode, RuntimeLogEvent, SendPayload, StatusResponse,
};
use crate::ports::device::LightDevice;
use crate::ports::ipc::{IpcRequestHandler, IpcServer};
use crate::ports::log::EventLog;
use crate::ports::runtime::RuntimeStore;
use crate::router::StateRouter;
use crate::runtime_lock::FileLock;

#[cfg(test)]
const DEVICE_HEALTH_TIMEOUT: Duration = Duration::from_millis(20);
#[cfg(not(test))]
const DEVICE_HEALTH_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(test)]
const DEVICE_CONNECT_TIMEOUT: Duration = Duration::from_millis(80);
#[cfg(not(test))]
const DEVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(test)]
const DEVICE_WRITE_TIMEOUT: Duration = Duration::from_millis(80);
#[cfg(not(test))]
const DEVICE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 判定“连接已经闲置太久、下次写前应主动刷新”的阈值。
///
/// 这个阈值的目标不是替代底层蓝牙栈的 keepalive，而是规避一种常见现场问题：
/// 设备长时间待机后，`health()` 看起来仍是 connected，但第一次真正 `write_mode()`
/// 会在底层卡住很久，最后才以 `ble_write_timeout` 失败。
///
/// 因此这里选择在“上次成功写入已经过去较久”时，主动做一次 reconnect，
/// 把“陈旧连接”问题前移到写之前处理。
#[cfg(test)]
const DEVICE_IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const DEVICE_IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(120);

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
    /// 设备健康快照缓存。
    ///
    /// 这份缓存让 `status`、重连判断和日志可以在不抢占设备互斥锁的前提下
    /// 继续工作；即使某次底层 BLE 写入卡住，daemon 仍能对外报告“我当前卡在什么状态”。
    device_health: Mutex<DeviceHealth>,
    /// 最近一次成功写 BLE 的时间，提供给 `status --verbose`。
    last_ble_write_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    /// 最近一次真正写到设备上的模式，用于去重和重连补写。
    last_applied_mode: Mutex<Option<Mode>>,
    /// 进程级启动锁，避免并发 spawn 出多个 daemon。
    startup_lock: Mutex<Option<FileLock>>,
    /// 允许 `&self` 路径安全升级回 `Arc<Self>`，用于后台任务派发。
    ///
    /// `IpcRequestHandler::handle()` 只能拿到 `&self`，但某些工作我们希望丢到后台异步执行，
    /// 例如：路由状态已接受后，再慢慢做 BLE 同步。
    ///
    /// 这里保存一个 `Weak<Self>`，让这类路径可以在需要时安全升级成 `Arc<Self>`；
    /// 若升级失败，说明 daemon 生命周期已经走到尾部，此时请求应该直接返回“当前不可用”。
    self_ref: Weak<Daemon>,
}

impl Daemon {
    /// 构造 daemon 实例。
    ///
    /// 这里把 runtime/log/device 等外部依赖全部注入进来，
    /// 让 daemon 本身只负责“协调”和“状态决策”，不直接创建具体 adapter。
    pub fn new(
        runtime: Arc<dyn RuntimeStore>,
        log: Arc<dyn EventLog>,
        device: Box<dyn LightDevice>,
    ) -> Arc<Self> {
        // daemon 以 `Arc<Self>` 形式返回，便于同时交给 IPC server、
        // 过期清理任务和重连任务共享。
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new_cyclic(|self_ref| Self {
            router: Mutex::new(StateRouter::new()),
            device: Mutex::new(device),
            runtime,
            log,
            shutdown_tx,
            device_health: Mutex::new(DeviceHealth::default()),
            last_ble_write_at: Mutex::new(None),
            last_applied_mode: Mutex::new(None),
            startup_lock: Mutex::new(None),
            self_ref: self_ref.clone(),
        })
    }

    /// 为后台任务或 IPC server 生成一个新的 shutdown 接收端。
    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// 启动 daemon 主循环。
    ///
    /// 这个函数会完成：
    /// 1. runtime 初始化与 pid/ipc 元信息落盘；
    /// 2. 首次设备连接；
    /// 3. 过期清理与自动重连后台任务启动；
    /// 4. IPC server 监听；
    /// 5. 退出时的收尾、灭灯与 runtime 清理。
    pub async fn run(self: Arc<Self>, server: Arc<dyn IpcServer>) -> AppResult<()> {
        // daemon 启动时先写入运行时元信息，便于命令层自动发现与自恢复。
        self.runtime.ensure_layout()?;
        self.acquire_startup_lock().await?;
        self.runtime.write_pid(std::process::id())?;
        self.runtime.write_ipc_info(&server.info())?;
        self.append_log("daemon", "daemon started", None, None, None, None)?;

        // 首次 BLE 连接不应阻塞 daemon 对外进入 ready：
        // 自动拉起链路只依赖 IPC `status` 探活，设备慢连/超时应该由后台重连链路兜底。
        let initial_connect_task = {
            let daemon = self.clone();
            tokio::spawn(async move {
                if let Err(err) = daemon.try_connect_device().await {
                    let _ = daemon.append_log(
                        "ble",
                        "initial device connect failed",
                        Some(&err.code),
                        None,
                        None,
                        None,
                    );
                }
            })
        };

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
        initial_connect_task.abort();
        let _ = initial_connect_task.await;
        let _ = expiry_task.await;
        let _ = reconnect_task.await;

        {
            let mut device = self.device.lock().await;
            // 退出时尽量把灯灭掉；如果失败，也不阻止进程退出。
            let _ = device.write_mode(Mode::Off).await;
        }

        let _ = self.runtime.clear_pid();
        let _ = self.runtime.clear_ipc_info();
        let _ = self.release_startup_lock().await;
        let _ = self.append_log("daemon", "daemon stopped", None, None, None, None);

        serve_result
    }

    /// TTL 过期清理后台任务。
    ///
    /// 它每秒检查一次状态池，把过期状态移除；
    /// 如果过期前后的 effective mode 发生变化，则同步更新设备。
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
                    let (before, after) = {
                        let mut router = self.router.lock().await;
                        let before = router.effective_mode(now);
                        router.prune_expired(now);
                        let after = router.effective_mode(now);
                        (before, after)
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

    /// BLE 自动重连后台任务。
    ///
    /// 当设备断开时，daemon 不会立刻失败退出，而是按退避序列持续尝试重连，
    /// 并在重连成功后把当前 effective mode 强制补写回设备。
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
                    let health = self.cached_device_health().await;
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

    /// 尝试建立一次设备连接。
    ///
    /// 该函数只负责连接本身，不负责状态同步；
    /// 状态补写由调用方在适当时机通过 `sync_effective_mode(true)` 完成。
    async fn try_connect_device(&self) -> AppResult<()> {
        // 连接能力完全委托给 device adapter，本层只关心成功或失败。
        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_ble",
            phase: "ble.connect_attempt",
            message: "attempting device connect",
            code: None,
            source: None,
            session: None,
            mode: None,
            context: None,
        });
        let mut device = self.device.lock().await;
        match timeout(DEVICE_CONNECT_TIMEOUT, device.connect()).await {
            Ok(Ok(info)) => {
                self.update_cached_device_health(|health| {
                    health.connected = true;
                    health.device_name = Some(info.name.clone());
                    health.last_error = None;
                })
                .await;
                self.append_runtime_log(RuntimeLogEvent {
                    kind: "runtime_ble",
                    phase: "ble.connect_success",
                    message: "device connect succeeded",
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: None,
                });
                Ok(())
            }
            Ok(Err(err)) => {
                self.update_cached_device_health(|health| {
                    health.connected = false;
                    health.last_error = Some(format!("{}: {}", err.code, err.message));
                })
                .await;
                self.append_runtime_log(RuntimeLogEvent {
                    kind: "runtime_ble",
                    phase: "ble.connect_failed",
                    message: "device connect failed",
                    code: Some(&err.code),
                    source: None,
                    session: None,
                    mode: None,
                    context: Some(json!({
                        "error_code": err.code,
                        "error_message": err.message,
                    })),
                });
                Err(err)
            }
            Err(_) => {
                self.update_cached_device_health(|health| {
                    health.connected = false;
                    health.last_error =
                        Some("ble_connect_timeout: device connect timed out".into());
                })
                .await;
                let err = AppError::new("ble_connect_timeout", "device connect timed out");
                self.append_runtime_log(RuntimeLogEvent {
                    kind: "runtime_ble",
                    phase: "ble.connect_failed",
                    message: "device connect timed out",
                    code: Some(&err.code),
                    source: None,
                    session: None,
                    mode: None,
                    context: Some(json!({
                        "error_code": err.code,
                        "error_message": err.message,
                        "timeout_ms": DEVICE_CONNECT_TIMEOUT.as_millis(),
                    })),
                });
                Err(err)
            }
        }
    }

    /// 把 router 当前计算出的 effective mode 同步到物理设备。
    ///
    /// `force_write=true` 常用于“刚重连完成后补写当前状态”，
    /// 即使模式没变化也要重新写一次设备，保证灯效和内存状态重新对齐。
    async fn sync_effective_mode(&self, force_write: bool) -> AppResult<()> {
        let now = Utc::now();
        let effective = {
            let router = self.router.lock().await;
            router.effective_mode(now)
        };
        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_ble",
            phase: "ble.sync_started",
            message: "sync_effective_mode started",
            code: None,
            source: None,
            session: None,
            mode: Some(effective),
            context: Some(json!({
                "effective": effective,
                "force_write": force_write,
            })),
        });

        let mut device = self.device.lock().await;
        let health = match timeout(DEVICE_HEALTH_TIMEOUT, device.health()).await {
            Ok(health) => {
                self.replace_cached_device_health(health.clone()).await;
                health
            }
            Err(_) => {
                self.append_runtime_log(RuntimeLogEvent {
                    kind: "runtime_ble",
                    phase: "ble.health_timeout",
                    message: "device health probe timed out before write",
                    code: Some("ble_health_timeout"),
                    source: None,
                    session: None,
                    mode: Some(effective),
                    context: Some(json!({
                        "effective": effective,
                        "timeout_ms": DEVICE_HEALTH_TIMEOUT.as_millis(),
                    })),
                });
                self.cached_device_health().await
            }
        };
        if !health.connected {
            self.append_runtime_log(RuntimeLogEvent {
                kind: "runtime_ble",
                phase: "ble.reconnect_before_write",
                message: "device was disconnected during sync, reconnecting before write",
                code: None,
                source: None,
                session: None,
                mode: Some(effective),
                context: Some(json!({
                    "effective": effective,
                })),
            });
            match timeout(DEVICE_CONNECT_TIMEOUT, device.connect()).await {
                Ok(Ok(info)) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = true;
                        cached.device_name = Some(info.name.clone());
                        cached.last_error = None;
                    })
                    .await;
                }
                Ok(Err(err)) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = false;
                        cached.last_error = Some(format!("{}: {}", err.code, err.message));
                    })
                    .await;
                    return Err(err);
                }
                Err(_) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = false;
                        cached.last_error =
                            Some("ble_connect_timeout: reconnect before write timed out".into());
                    })
                    .await;
                    return Err(AppError::new(
                        "ble_connect_timeout",
                        "reconnect before write timed out",
                    ));
                }
            }
        } else if self.is_ble_connection_stale(now).await {
            self.append_runtime_log(RuntimeLogEvent {
                kind: "runtime_ble",
                phase: "ble.reconnect_before_write",
                message: "device connection looked stale after idle, reconnecting before write",
                code: None,
                source: None,
                session: None,
                mode: Some(effective),
                context: Some(json!({
                    "effective": effective,
                    "reason": "idle_stale",
                    "idle_refresh_interval_secs": DEVICE_IDLE_REFRESH_INTERVAL.as_secs(),
                })),
            });
            match timeout(DEVICE_CONNECT_TIMEOUT, device.connect()).await {
                Ok(Ok(info)) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = true;
                        cached.device_name = Some(info.name.clone());
                        cached.last_error = None;
                    })
                    .await;
                }
                Ok(Err(err)) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = false;
                        cached.last_error = Some(format!("{}: {}", err.code, err.message));
                    })
                    .await;
                    return Err(err);
                }
                Err(_) => {
                    self.update_cached_device_health(|cached| {
                        cached.connected = false;
                        cached.last_error =
                            Some("ble_connect_timeout: reconnect after idle timed out".into());
                    })
                    .await;
                    return Err(AppError::new(
                        "ble_connect_timeout",
                        "reconnect after idle timed out",
                    ));
                }
            }
        }

        // BLE 写入要做节流：只有 effective mode 真正变化时才写设备。
        // 但如果设备刚刚重连，即使 mode 没变化，也必须强制补写一次当前 effective mode。
        let mut last_applied = self.last_applied_mode.lock().await;
        if !force_write && health.connected && last_applied.is_some_and(|mode| mode == effective) {
            self.append_runtime_log(RuntimeLogEvent {
                kind: "runtime_ble",
                phase: "ble.write_skipped_unchanged",
                message: "skipped BLE write because effective mode did not change",
                code: None,
                source: None,
                session: None,
                mode: Some(effective),
                context: Some(json!({
                    "effective": effective,
                    "force_write": force_write,
                })),
            });
            return Ok(());
        }

        match timeout(DEVICE_WRITE_TIMEOUT, device.write_mode(effective)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                self.update_cached_device_health(|health| {
                    health.connected = false;
                    health.last_error = Some(format!("{}: {}", err.code, err.message));
                })
                .await;
                return Err(err);
            }
            Err(_) => {
                self.update_cached_device_health(|health| {
                    health.connected = false;
                    health.last_error = Some("ble_write_timeout: device write timed out".into());
                })
                .await;
                return Err(AppError::new("ble_write_timeout", "device write timed out"));
            }
        }

        *last_applied = Some(effective);
        let now = Utc::now();
        *self.last_ble_write_at.lock().await = Some(now);
        self.update_cached_device_health(|health| {
            health.connected = true;
            health.last_error = None;
            health.last_mode = Some(effective);
            health.last_write_at = Some(now);
        })
        .await;
        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_ble",
            phase: "ble.write_success",
            message: "BLE write succeeded and last_applied_mode updated",
            code: None,
            source: None,
            session: None,
            mode: Some(effective),
            context: Some(json!({
                "effective": effective,
            })),
        });
        Ok(())
    }

    /// 追加一条面向事件事实的结构化日志。
    fn append_log(
        &self,
        kind: &str,
        message: &str,
        code: Option<&str>,
        source: Option<&str>,
        session: Option<&str>,
        mode: Option<Mode>,
    ) -> AppResult<()> {
        // 统一封装事件日志构造，避免不同调用点随手写出结构不一致的日志。
        // 这类日志会同时进入 events.log，属于对外“发生过什么”的事实记录。
        self.log.append(LogEvent {
            timestamp: Utc::now(),
            level: if code.is_some() {
                "warn".into()
            } else {
                "info".into()
            },
            kind: kind.into(),
            message: message.into(),
            phase: None,
            code: code.map(ToOwned::to_owned),
            source: source.map(ToOwned::to_owned),
            session: session.map(ToOwned::to_owned),
            mode,
            context: None,
        })
    }

    fn append_runtime_log(&self, event: RuntimeLogEvent<'_>) {
        // runtime 日志用于记录链路节点，采用“尽力写入”策略；
        // 即使日志失败，也不能影响 daemon 对外处理状态请求。
        let _ = self.log.append_runtime(event.into_log_event());
    }

    /// 处理 `send` IPC 请求。
    ///
    /// 整个流程是：
    /// 1. 记录收到的原始 payload；
    /// 2. 交给 router 更新会话状态池；
    /// 3. 记录“accepted state update”事件日志；
    /// 4. 尝试把 effective mode 写入 BLE；
    /// 5. 根据 BLE 写入结果返回成功或“已接受但设备暂不可用”的响应。
    async fn handle_send(&self, request_id: &str, payload: SendPayload) -> IpcResponseEnvelope {
        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_ipc_send",
            phase: "ipc_send.received",
            message: "daemon received send request",
            code: None,
            source: Some(&payload.source),
            session: Some(&payload.session),
            mode: Some(payload.mode),
            context: Some(json!({
                "request_id": request_id,
                "hook_id": payload.hook_id,
                "raw_event": payload.raw_event,
                "raw_tool": payload.raw_tool,
                "turn": payload.turn,
                "ttl_secs": payload.ttl,
                "capability": payload.capability.as_ref().map(|value| format!("{value:?}")),
                "suggested_mode": payload.suggested_mode,
                "cwd": payload.cwd,
            })),
        });
        let now = Utc::now();
        let effective = {
            let mut router = self.router.lock().await;
            router.apply_send(&payload, now)
        };
        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_router",
            phase: "router.state_applied",
            message: "router applied state and resolved effective mode",
            code: None,
            source: Some(&payload.source),
            session: Some(&payload.session),
            mode: Some(effective),
            context: Some(json!({
                "request_id": request_id,
                "effective_mode": effective,
                "input_mode": payload.mode,
                "raw_event": payload.raw_event,
                "turn": payload.turn,
            })),
        });

        let _ = self.append_log(
            "ipc_send",
            "accepted state update",
            None,
            Some(&payload.source),
            Some(&payload.session),
            Some(payload.mode),
        );

        let Some(sync_daemon) = self.self_ref.upgrade() else {
            return IpcResponseEnvelope::error(
                request_id.to_string(),
                &AppError::new("daemon_unavailable", "daemon self reference is unavailable"),
            );
        };
        let sync_daemon = Arc::new(SendSyncContext {
            daemon: sync_daemon,
            request_id: request_id.to_string(),
            source: payload.source.clone(),
            session: payload.session.clone(),
            effective,
        });
        tokio::spawn(async move {
            sync_daemon.run().await;
        });

        self.append_runtime_log(RuntimeLogEvent {
            kind: "runtime_ipc_send",
            phase: "ipc_send.completed",
            message: "daemon completed send request successfully",
            code: None,
            source: Some(&payload.source),
            session: Some(&payload.session),
            mode: Some(effective),
            context: Some(json!({
                "request_id": request_id,
                "effective_mode": effective,
            })),
        });

        IpcResponseEnvelope::ok(request_id.to_string(), "accepted").with_data(json!({
            "effective": effective,
            "queued": true,
        }))
    }

    /// 处理 `status` IPC 请求。
    ///
    /// verbose 模式下会额外返回当前状态池中的所有来源明细，
    /// 便于排查“为什么现在显示的是这个状态”。
    async fn handle_status(&self, request_id: &str, verbose: bool) -> IpcResponseEnvelope {
        let now = Utc::now();
        let (effective, sources) = {
            let mut router = self.router.lock().await;
            router.snapshot_status(now, verbose)
        };
        let health = self.cached_device_health().await;

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

    /// 处理 `stop` IPC 请求。
    ///
    /// 这里只负责广播 shutdown 信号并立即返回，
    /// 真正的停止和清理在 `run()` 的收尾阶段完成。
    async fn handle_stop(&self, request_id: &str) -> IpcResponseEnvelope {
        // stop 采用异步通知模式：先返回 stopping，再由 run() 收尾并真正退出。
        let _ = self.append_log("daemon", "stop requested", None, None, None, None);
        let _ = self.shutdown_tx.send(true);
        IpcResponseEnvelope::ok(request_id.to_string(), "stopping")
    }

    async fn acquire_startup_lock(&self) -> AppResult<()> {
        let lock_path = self.runtime.runtime_dir().join("daemon.lock");
        let lock = FileLock::acquire(lock_path)?;
        // `run()` 自身运行在 Tokio runtime 内，这里如果使用 `blocking_lock()`，
        // 会因为在 runtime 线程上做阻塞等待而直接 panic。
        let mut guard = self.startup_lock.lock().await;
        *guard = Some(lock);
        Ok(())
    }

    async fn release_startup_lock(&self) -> AppResult<()> {
        let mut guard = self.startup_lock.lock().await;
        let _ = guard.take();
        Ok(())
    }

    async fn cached_device_health(&self) -> DeviceHealth {
        self.device_health.lock().await.clone()
    }

    async fn replace_cached_device_health(&self, health: DeviceHealth) {
        *self.device_health.lock().await = health;
    }

    async fn update_cached_device_health(&self, update: impl FnOnce(&mut DeviceHealth)) {
        let mut health = self.device_health.lock().await;
        update(&mut health);
    }

    /// 判断当前 BLE 连接是否已经“闲置过久，值得在写前主动刷新”。
    ///
    /// 这里并不试图证明连接“必然坏了”，而是做一个启发式判断：
    /// - 如果从未成功写过 BLE，则不做 idle refresh；
    /// - 如果最近刚写成功，也不额外 reconnect；
    /// - 只有“连接看起来还在，但距离上次成功写已经很久”时，才在写前多做一步 connect。
    ///
    /// 这么做的好处是把“空闲太久后的第一次写超时”前移为一次更可恢复的 reconnect。
    async fn is_ble_connection_stale(&self, now: chrono::DateTime<Utc>) -> bool {
        let Some(last_write_at) = *self.last_ble_write_at.lock().await else {
            return false;
        };
        now.signed_duration_since(last_write_at)
            .to_std()
            .ok()
            .is_some_and(|elapsed| elapsed >= DEVICE_IDLE_REFRESH_INTERVAL)
    }
}

struct SendSyncContext {
    /// 真正执行后台 BLE 同步的 daemon 共享引用。
    daemon: Arc<Daemon>,
    /// 原始 IPC 请求 ID，便于把后台同步结果和前台 accepted 请求关联起来。
    request_id: String,
    /// 触发这次同步的来源名。
    source: String,
    /// 触发这次同步的会话 ID。
    session: String,
    /// 这次后台同步想要落到设备上的 effective mode。
    effective: Mode,
}

impl SendSyncContext {
    /// 在后台执行一次“尽力同步 BLE”。
    ///
    /// 语义上，IPC `send` 在路由状态被 daemon 接受后就已经算成功；
    /// 这里负责的是随后的物理设备副作用，因此即使失败也只写日志和 health，
    /// 不再反向阻塞或回滚前台请求。
    async fn run(&self) {
        if let Err(err) = self.daemon.sync_effective_mode(false).await {
            self.daemon.append_runtime_log(RuntimeLogEvent {
                kind: "runtime_ble",
                phase: "ble.sync_failed",
                message: "sync_effective_mode failed",
                code: Some(&err.code),
                source: Some(&self.source),
                session: Some(&self.session),
                mode: Some(self.effective),
                context: Some(json!({
                    "request_id": self.request_id,
                    "effective_mode": self.effective,
                    "error_code": err.code,
                    "error_message": err.message,
                })),
            });
            let _ = self.daemon.append_log(
                "ble",
                "failed to sync effective mode",
                Some(&err.code),
                Some(&self.source),
                Some(&self.session),
                Some(self.effective),
            );
        }
    }
}

/// daemon 作为 IPC handler 的实现入口。
///
/// server adapter 只需要把解码后的请求交给这里，
/// 后续业务分发统一由 daemon 内部完成。
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
