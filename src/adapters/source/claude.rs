//! Claude Hook stdin 解析器。

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{AgentCapability, AgentEvent, AppError, AppResult, HookParseContext, Mode};
use crate::ports::source::SourceAdapter;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ClaudeHookInput {
    // 保留完整字段形状，便于后续扩展和输入结构校验。
    session_id: Option<String>,
    transcript_path: Option<String>,
    cwd: Option<String>,
    hook_event_name: Option<String>,
    reason: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_response: Option<Value>,
}

pub struct ClaudeAdapter;

impl SourceAdapter for ClaudeAdapter {
    fn source(&self) -> &'static str {
        "claude"
    }

    fn parse(&self, input: Value, ctx: &HookParseContext) -> AppResult<AgentEvent> {
        // 先做宿主 JSON 解析，再进入统一映射。
        let raw: ClaudeHookInput = serde_json::from_value(input.clone())
            .map_err(|err| AppError::invalid("failed to parse claude hook stdin", err))?;

        let (capability, suggested_mode) = map_claude_mode(
            raw.hook_event_name.as_deref(),
            raw.tool_name.as_deref(),
            raw.reason.as_deref(),
            ctx.explicit_mode,
        );

        Ok(build_event(ctx, &input, capability, suggested_mode))
    }
}

fn map_claude_mode(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    reason: Option<&str>,
    fallback_mode: Mode,
) -> (AgentCapability, Option<Mode>) {
    // Claude 的 Notification/PermissionDenied 等事件语义与其他宿主不完全相同，
    // 必须在 adapter 层单独编码，保证核心层看到的是稳定能力枚举。
    match raw_event.unwrap_or_default() {
        "SessionStart" => (AgentCapability::Idle, Some(Mode::Green)),
        "UserPromptSubmit" | "SubagentStart" | "PreCompact" | "PostCompact" => {
            (AgentCapability::Thinking, Some(Mode::Thinking))
        }
        "PermissionRequest" | "Notification" => {
            (AgentCapability::WaitingForUser, Some(Mode::Alarm))
        }
        "PermissionDenied" | "PostToolUseFailure" | "StopFailure" => {
            (AgentCapability::Failed, Some(Mode::Error))
        }
        // Claude 的 PostToolUse/PostToolBatch 在第一阶段同样不从 tool_response 里猜结果，
        // 直接保留安装器配置的兜底 mode，让编辑类工具能稳定维持 ai，
        // 同时也能让 PostToolBatch 把 alarm 及时推出到 busy。
        "PostToolUse" | "PostToolBatch" => (AgentCapability::RunningCommand, Some(fallback_mode)),
        // 在真实使用中，Claude 结束会话时不一定总会先触发 Stop。
        // 因此正常 SessionEnd 也应当给出 success，避免任务已完成但灯直接回落到 demo。
        // 如果明确带有中止/关闭类 reason，则仍回落为 demo，让失败态继续由更早的 error 事件主导。
        "SessionEnd" => match reason.unwrap_or_default() {
            "aborted" | "error" | "window_close" | "user_close" => {
                (AgentCapability::Idle, Some(Mode::Demo))
            }
            _ => (AgentCapability::Succeeded, Some(Mode::Success)),
        },
        "SubagentStop" | "Stop" => (AgentCapability::Succeeded, Some(Mode::Success)),
        "PreToolUse" => match tool_name.unwrap_or_default() {
            "Edit" | "MultiEdit" | "Write" => (AgentCapability::Generating, Some(Mode::Ai)),
            "Bash" => (AgentCapability::RunningCommand, Some(Mode::Busy)),
            _ => (AgentCapability::RunningCommand, Some(Mode::Busy)),
        },
        _ => (AgentCapability::Unknown, None),
    }
}

// 测试实现拆到独立目录，避免与 Claude Hook 事件解析主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/source/claude_tests.rs"]
mod tests;
