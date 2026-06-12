use std::time::Duration;

use serde_json::json;
use tokio::time::sleep;

use super::{AppContext, append_runtime_log};
use crate::model::{
    AppError, AppResult, IpcRequestEnvelope, IpcRequestPayload, IpcResponseEnvelope,
    RuntimeLogEvent,
};
use crate::ports::runtime::RuntimeStore;
use crate::runtime_lock::{FileLock, process_is_alive};

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
    let _guard = FileLock::acquire(lock_path)?;

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
        let _ = ctx.runtime.clear_pid();
        let _ = ctx.runtime.clear_ipc_info();
    }

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
