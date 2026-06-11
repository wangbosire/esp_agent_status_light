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
        );

        Ok(build_event(ctx, &input, capability, suggested_mode))
    }
}

fn map_claude_mode(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    reason: Option<&str>,
) -> (AgentCapability, Option<Mode>) {
    // Claude 的 Notification/PermissionDenied 等事件语义与其他宿主不完全相同，
    // 必须在 adapter 层单独编码，保证核心层看到的是稳定能力枚举。
    match raw_event.unwrap_or_default() {
        "SessionStart" | "UserPromptSubmit" | "SubagentStart" | "PreCompact" | "PostCompact" => {
            (AgentCapability::Thinking, Some(Mode::Thinking))
        }
        "PermissionRequest" | "Notification" => {
            (AgentCapability::WaitingForUser, Some(Mode::Alarm))
        }
        "PermissionDenied" | "PostToolUseFailure" | "StopFailure" => {
            (AgentCapability::Failed, Some(Mode::Error))
        }
        "PostToolUse" | "PostToolBatch" => (AgentCapability::RunningCommand, None),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn claude_notification_maps_to_alarm() {
        let ctx = HookParseContext {
            source: "claude".into(),
            explicit_mode: Mode::Busy,
            current_dir: ".".into(),
            ttl: None,
        };
        let event = ClaudeAdapter
            .parse(
                json!({
                    "session_id": "abc",
                    "hook_event_name": "Notification",
                }),
                &ctx,
            )
            .expect("claude parse should succeed");
        assert_eq!(event.capability, AgentCapability::WaitingForUser);
        assert_eq!(event.suggested_mode, Some(Mode::Alarm));
    }

    #[test]
    fn claude_session_end_defaults_to_success() {
        let ctx = HookParseContext {
            source: "claude".into(),
            explicit_mode: Mode::Demo,
            current_dir: ".".into(),
            ttl: None,
        };
        let event = ClaudeAdapter
            .parse(
                json!({
                    "session_id": "abc",
                    "hook_event_name": "SessionEnd",
                }),
                &ctx,
            )
            .expect("claude parse should succeed");
        assert_eq!(event.capability, AgentCapability::Succeeded);
        assert_eq!(event.suggested_mode, Some(Mode::Success));
    }

    #[test]
    fn claude_session_end_aborted_keeps_demo() {
        let ctx = HookParseContext {
            source: "claude".into(),
            explicit_mode: Mode::Demo,
            current_dir: ".".into(),
            ttl: None,
        };
        let event = ClaudeAdapter
            .parse(
                json!({
                    "session_id": "abc",
                    "hook_event_name": "SessionEnd",
                    "reason": "aborted",
                }),
                &ctx,
            )
            .expect("claude parse should succeed");
        assert_eq!(event.capability, AgentCapability::Idle);
        assert_eq!(event.suggested_mode, Some(Mode::Demo));
    }
}
