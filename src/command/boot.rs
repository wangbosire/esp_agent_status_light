use std::fs;
use std::time::Duration;

use serde_json::json;
use tokio::time::sleep;

use super::{AppContext, append_runtime_log};
use crate::model::{
    AppError, AppResult, IpcRequestEnvelope, IpcRequestPayload, IpcResponseEnvelope,
    RuntimeLogEvent,
};
use crate::ports::runtime::RuntimeStore;
use crate::runtime_lock::{FileLock, lock_owner_is_stale, process_is_alive, read_lock_owner};

/// daemon 自启动锁的最大重试次数。
///
/// 600 * 10ms ≈ 6s。
/// 这个窗口比 daemon ready 探测的 3s 略长，目的是让并发 Hook 在“已经有人负责启动”的情况下
/// 更倾向于排队等待同一轮启动完成，而不是因为只晚到几百毫秒就直接报 `lock_timeout`。
const DAEMON_AUTOSTART_LOCK_RETRY_ATTEMPTS: usize = 600;
/// daemon 自启动锁的单次重试间隔。
///
/// 保持 10ms 能让等待方足够快地观察到锁释放，又不至于在高并发下疯狂忙轮询。
const DAEMON_AUTOSTART_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

/// 先尝试直接请求 daemon；失败时自动拉起并重试一次。
///
/// 这样用户第一次执行 `esp send` 时不必先手动启动 daemon，
/// 也让 Hook 场景具备“开箱即用”的体验。
pub(super) async fn request_with_auto_start(
    ctx: &AppContext,
    request: IpcRequestEnvelope,
) -> AppResult<IpcResponseEnvelope> {
    match ctx.ipc_client().request(request.clone()).await {
        Ok(response) => {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "runtime_ipc",
                    phase: "ipc.initial_success",
                    message: "initial ipc request succeeded",
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: Some(json!({
                        "request_id": request.request_id,
                    })),
                },
            );
            Ok(response)
        }
        Err(_) => {
            // 第一次请求失败并不立刻判定为业务错误：
            // 更常见的情况只是 daemon 还没启动或者刚好在重启窗口里。
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "runtime_ipc",
                    phase: "ipc.initial_failed",
                    message: "initial ipc request failed, attempting daemon auto-start",
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: Some(json!({
                        "request_id": request.request_id,
                    })),
                },
            );
            ensure_daemon_running(ctx).await?;
            let retried = ctx.ipc_client().request(request).await;
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "runtime_ipc",
                    phase: if retried.is_ok() {
                        "ipc.retry_success"
                    } else {
                        "ipc.retry_failed"
                    },
                    message: if retried.is_ok() {
                        "retry ipc request after auto-start succeeded"
                    } else {
                        "retry ipc request after auto-start failed"
                    },
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: None,
                },
            );
            retried
        }
    }
}

pub(super) async fn ensure_daemon_running(ctx: &AppContext) -> AppResult<()> {
    ctx.runtime.ensure_layout()?;
    let lock_path = ctx.runtime.runtime_dir().join("daemon-autostart.lock");
    // 用跨进程锁串行化“检查 -> 清理 stale pid -> spawn”的整段流程，
    // 避免多个 CLI/Hook 同时认为 daemon 不在，然后各自拉起一份后台进程。
    // Hook 高并发风暴里会同时有很多 send 落到这里：
    // 比起让后到请求在 200ms 内直接报 lock_timeout，更合理的是排队等待
    // 已有的 daemon 启动流程完成，再复用它。
    let _guard = FileLock::acquire_with_retry(
        lock_path,
        DAEMON_AUTOSTART_LOCK_RETRY_ATTEMPTS,
        DAEMON_AUTOSTART_LOCK_RETRY_DELAY,
    )?;

    // 拿到串行化锁之后，第一件事永远是再探测一次 daemon 是否已经 ready。
    // 这样可以覆盖“我在等待锁时，前一个请求已经把 daemon 启好了”的情况，
    // 避免无意义地继续走 stale pid 清理和重复 spawn。
    if daemon_ipc_ready(ctx).await {
        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "runtime_daemon_boot",
                phase: "daemon_boot.already_ready",
                message: "daemon already ready during auto-start check",
                code: None,
                source: None,
                session: None,
                mode: None,
                context: None,
            },
        );
        return Ok(());
    }

    if let Some(pid) = ctx.runtime.read_pid()? {
        let alive = process_is_alive(pid)?;
        if alive && daemon_ipc_ready(ctx).await {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "runtime_daemon_boot",
                    phase: "daemon_boot.healthcheck_alive",
                    message: "daemon health check found alive pid with ready ipc",
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: Some(json!({ "pid": pid })),
                },
            );
            return Ok(());
        }

        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "runtime_daemon_boot",
                phase: "daemon_boot.healthcheck_stale",
                message: "daemon health check found stale or unreachable pid, clearing runtime markers",
                code: None,
                source: None,
                session: None,
                mode: None,
                context: Some(json!({
                    "pid": pid,
                    "alive": alive,
                })),
            },
        );
        // 只要 pid 还在但 IPC 不通，就把它视为 stale marker：
        // 可能是旧 daemon 异常退出，也可能是 pid 文件和 ipc 信息残留了。
        clear_stale_daemon_runtime_markers(ctx, Some((pid, alive)));
    }

    // 走到这里说明当前没有可用 IPC；即便 pid 文件缺失，也可能还残留 daemon.lock。
    // 这类锁会让新拉起的 daemon 在进入主循环前直接退出，从而表现为 hook 偶发“触发了但灯不变”。
    clear_stale_daemon_startup_lock(ctx, None);

    let exe = std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;
    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "runtime_daemon_boot",
            phase: "daemon_boot.spawn",
            message: "spawning background daemon",
            code: None,
            source: None,
            session: None,
            mode: None,
            context: Some(json!({
                "exe": exe,
            })),
        },
    );
    ctx.platform.spawn_background_daemon(&exe)?;

    // 后台进程已 fork/spawn 出去后，再主动等一个短窗口确认 IPC ready。
    // 这样调用方拿到的错误更稳定，不会把“启动中”误当成永久失败。
    for _ in 0..20 {
        sleep(Duration::from_millis(150)).await;
        if daemon_ipc_ready(ctx).await {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "runtime_daemon_boot",
                    phase: "daemon_boot.ready",
                    message: "daemon became ready after auto-start",
                    code: None,
                    source: None,
                    session: None,
                    mode: None,
                    context: None,
                },
            );
            return Ok(());
        }
    }

    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "runtime_daemon_boot",
            phase: "daemon_boot.timeout",
            message: "daemon did not become ready after auto-start timeout window",
            code: Some("ipc_unavailable"),
            source: None,
            session: None,
            mode: None,
            context: Some(json!({
                "timeout_ms": 3000,
            })),
        },
    );
    Err(AppError::new(
        "ipc_unavailable",
        "daemon did not become ready after auto-start",
    ))
}

fn clear_stale_daemon_runtime_markers(ctx: &AppContext, stale_pid: Option<(u32, bool)>) {
    // pid/ipc/daemon.lock 是同一轮 daemon 生命周期的三类标记。
    // 一旦 pid 已被判定为 stale，必须成组清理，避免只清掉 pid/ipc 后留下启动锁卡死下一轮自启动。
    let _ = ctx.runtime.clear_pid();
    let _ = ctx.runtime.clear_ipc_info();
    clear_stale_daemon_startup_lock(ctx, stale_pid);
}

fn clear_stale_daemon_startup_lock(ctx: &AppContext, stale_pid: Option<(u32, bool)>) {
    let lock_path = ctx.runtime.runtime_dir().join("daemon.lock");
    let Some(reason) = stale_startup_lock_reason(&lock_path, stale_pid) else {
        return;
    };

    let removed = match fs::remove_file(&lock_path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    };

    if removed {
        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "runtime_daemon_boot",
                phase: "daemon_boot.cleared_stale_startup_lock",
                message: "cleared stale daemon startup lock before spawning daemon",
                code: None,
                source: None,
                session: None,
                mode: None,
                context: Some(json!({
                    "lock_path": lock_path,
                    "reason": reason,
                    "stale_pid": stale_pid.map(|(pid, alive)| json!({
                        "pid": pid,
                        "alive": alive,
                    })),
                })),
            },
        );
    }
}

fn stale_startup_lock_reason(
    lock_path: &std::path::Path,
    stale_pid: Option<(u32, bool)>,
) -> Option<&'static str> {
    if !lock_path.exists() {
        return None;
    }
    let owner = match read_lock_owner(lock_path) {
        Ok(owner) => owner,
        Err(_) => return Some("startup_lock_unreadable"),
    };
    let owner_pid = owner.pid;
    if matches!(stale_pid, Some((pid, false)) if pid == owner_pid) {
        return Some("startup_lock_matches_dead_daemon_pid");
    }

    match lock_owner_is_stale(lock_path) {
        Ok(true) => Some("startup_lock_owner_dead"),
        Ok(false) => None,
        Err(_) => None,
    }
}

/// 使用统一的 `status` RPC 判断 daemon IPC 是否已经 ready。
///
/// 这里刻意不引入额外探活协议，而是直接复用最轻量的业务无副作用请求：
/// - ready 的定义与真实调用方保持一致；
/// - 不需要再维护一套“启动中但不一定可用”的旁路状态；
/// - 若 transport / socket / handler 任一环节没就绪，都会自然返回 false。
async fn daemon_ipc_ready(ctx: &AppContext) -> bool {
    // 就绪探测统一走最轻量的 `status` 请求，避免引入额外探活协议。
    let probe = IpcRequestEnvelope::new(IpcRequestPayload::Status { verbose: false });
    ctx.ipc_client().request(probe).await.is_ok()
}

/// 当 daemon 常规 IPC stop 失败时，按 pid 做最后兜底终止。
///
/// 这里只负责“尽量杀掉记录中的进程并清理 runtime 标记”，
/// 不试图保证它一定是正确的 daemon 实例，因此只在 `--force` 下使用。
pub(super) fn force_stop_by_pid(runtime: &dyn RuntimeStore) -> AppResult<()> {
    let pid = runtime
        .read_pid()?
        .ok_or_else(|| AppError::new("pid_missing", "no daemon pid file found"))?;

    #[cfg(unix)]
    {
        // 先发 TERM，让 daemon 有机会执行自己的清理路径。
        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .map_err(|err| AppError::io("execute kill", err))?;
        if !status.success() {
            return Err(AppError::new(
                "kill_failed",
                format!("kill returned status {status}"),
            ));
        }
    }

    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .map_err(|err| AppError::io("execute taskkill", err))?;
        if !status.success() {
            return Err(AppError::new(
                "kill_failed",
                format!("taskkill returned status {status}"),
            ));
        }
    }

    let _ = runtime.clear_pid();
    let _ = runtime.clear_ipc_info();
    Ok(())
}
