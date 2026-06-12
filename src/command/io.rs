use std::io::{IsTerminal, Read};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::model::{AppError, AppResult};

/// 从标准输入中读取 Hook JSON。
///
/// 这个函数的目标不是“永远成功读出完整 JSON”，而是：
/// - 有输入时尽量拿到有用上下文；
/// - 没有输入时快速返回；
/// - 输入损坏时也不要阻塞 Hook 主流程。
pub(super) fn read_stdin_json() -> AppResult<Option<Value>> {
    let stdin = std::io::stdin();
    // 手动命令通常没有 stdin payload，Hook 场景才会进到这里。
    if stdin.is_terminal() {
        return Ok(None);
    }

    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        // 把阻塞式 stdin 读取放到单独线程里，避免主线程被异常输入卡住。
        let mut raw = String::new();
        let result = stdin.lock().read_to_string(&mut raw).map(|_| raw);
        let _ = tx.send(result);
    });

    // 给 stdin 一个很短的读取窗口，避免某些宿主没有及时关闭管道时把命令卡死。
    let raw = match rx.recv_timeout(Duration::from_millis(75)) {
        Ok(Ok(raw)) => raw,
        Ok(Err(err)) => return Err(AppError::io("read hook stdin", err)),
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
    };

    // 空输入直接视作“没有 hook 上下文”，让后续 fallback 和 explicit mode 接管。
    if raw.trim().is_empty() {
        return Ok(None);
    }

    // JSON 解析失败时也不要让 Hook 整体失败；这里退回一个空对象，
    // 让上游至少还能走 session/cwd 的兜底逻辑。
    match serde_json::from_str(raw.trim()) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(Some(json!({}))),
    }
}
