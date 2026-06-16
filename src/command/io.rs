use std::io::{IsTerminal, Read};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::model::{AppError, AppResult};

/// Hook stdin 的一次读取结果。
///
/// `raw_input` 保留宿主工具传进来的原始 stdin 文本，专门用于 runtime 日志排障；
/// `parsed_json` 继续服务后续 source adapter 归一逻辑，保持旧行为不变。
#[derive(Debug, Clone, Default)]
pub(super) struct HookInput {
    pub raw_input: Option<String>,
    pub parsed_json: Option<Value>,
    pub parse_error: Option<String>,
    pub timed_out: bool,
}

/// 从标准输入中读取 Hook JSON。
///
/// 这个函数的目标不是“永远成功读出完整 JSON”，而是：
/// - 有输入时尽量拿到有用上下文；
/// - 没有输入时快速返回；
/// - 输入损坏时也不要阻塞 Hook 主流程。
pub(super) fn read_hook_input() -> AppResult<HookInput> {
    let stdin = std::io::stdin();
    // 手动命令通常没有 stdin payload，Hook 场景才会进到这里。
    if stdin.is_terminal() {
        return Ok(HookInput::default());
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
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Ok(HookInput {
                timed_out: true,
                ..HookInput::default()
            });
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(HookInput::default()),
    };

    Ok(hook_input_from_raw(raw))
}

pub(super) fn hook_input_from_raw(raw: String) -> HookInput {
    // 空输入直接视作“没有 hook 上下文”，让后续 fallback 和 explicit mode 接管。
    if raw.trim().is_empty() {
        return HookInput {
            raw_input: Some(raw),
            ..HookInput::default()
        };
    }

    // JSON 解析失败时也不要让 Hook 整体失败；这里退回一个空对象，
    // 让上游至少还能走 session/cwd 的兜底逻辑。
    match serde_json::from_str(raw.trim()) {
        Ok(value) => HookInput {
            raw_input: Some(raw),
            parsed_json: Some(value),
            parse_error: None,
            timed_out: false,
        },
        Err(err) => HookInput {
            raw_input: Some(raw),
            parsed_json: Some(json!({})),
            parse_error: Some(err.to_string()),
            timed_out: false,
        },
    }
}
