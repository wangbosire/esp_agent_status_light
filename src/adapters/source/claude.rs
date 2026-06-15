//! Claude Hook stdin 解析器。

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{
    AgentCapability, AgentEvent, AppError, AppResult, EventSemantics, HookParseContext, Mode,
};
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

/// Claude Hook 解析器。
///
/// Claude 的结束类/通知类事件和其它宿主差异较大，
/// 因此这里保留独立映射，而不是完全复用别家的规则。
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

        Ok(build_event(
            ctx,
            &input,
            capability,
            suggested_mode,
            semantics_for_claude(
                raw.hook_event_name.as_deref(),
                raw.tool_name.as_deref(),
                raw.reason.as_deref(),
            ),
        ))
    }
}

/// 将 Claude Hook 事件映射为统一能力与建议模式。
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
            // 按最新规则，文件读取和文件改写都归到 `ai`：
            // 只要 Claude 正在围绕文件内容取上下文或落地修改，
            // 用户看到的都应该是“AI 内容处理态”。
            "Read" | "Edit" | "MultiEdit" | "Write" => {
                (AgentCapability::Generating, Some(Mode::Ai))
            }
            "Bash" => (AgentCapability::RunningCommand, Some(Mode::Busy)),
            _ => (AgentCapability::RunningCommand, Some(Mode::Busy)),
        },
        _ => (AgentCapability::Unknown, None),
    }
}

/// 将 Claude 原始事件映射为统一流程语义。
///
/// 这里和 mode 映射分开维护，避免以后调整视觉状态时意外影响状态覆盖规则。
fn semantics_for_claude(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    reason: Option<&str>,
) -> EventSemantics {
    // Claude 的 `reason` 字段在结束类事件里语义很关键：
    // 没有它时，SessionEnd 和 PostToolBatch 都不足以区分“自然结束”还是“异常中止”。
    match raw_event.unwrap_or_default() {
        "SessionStart" => EventSemantics::Continuation,
        "UserPromptSubmit" | "SubagentStart" | "PreCompact" | "PostCompact" => {
            EventSemantics::Continuation
        }
        "PermissionRequest" | "Notification" => EventSemantics::UserAttention,
        "PermissionDenied" | "PostToolUseFailure" | "StopFailure" => EventSemantics::Failure,
        "PostToolUse" => EventSemantics::Continuation,
        "PostToolBatch" => {
            if matches!(
                reason,
                Some("aborted" | "error" | "window_close" | "user_close")
            ) {
                EventSemantics::Failure
            } else {
                EventSemantics::Continuation
            }
        }
        "SessionEnd" => {
            if matches!(
                reason,
                Some("aborted" | "error" | "window_close" | "user_close")
            ) {
                EventSemantics::Failure
            } else {
                EventSemantics::Completion
            }
        }
        "SubagentStop" | "Stop" => EventSemantics::Completion,
        "PreToolUse" => match tool_name.unwrap_or_default() {
            "Read" => EventSemantics::FileRead,
            "Edit" | "MultiEdit" | "Write" => EventSemantics::FileWrite,
            "Bash" => EventSemantics::ExplicitToolExecution,
            _ => EventSemantics::ExplicitToolExecution,
        },
        _ => EventSemantics::Unknown,
    }
}

// 测试实现拆到独立目录，避免与 Claude Hook 事件解析主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/source/claude_tests.rs"]
mod tests;
