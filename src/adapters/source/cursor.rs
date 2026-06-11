//! Cursor Hook stdin 解析器。

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{AgentCapability, AgentEvent, AppError, AppResult, HookParseContext, Mode};
use crate::ports::source::SourceAdapter;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorHookInput {
    // Cursor 官方事件字段大量使用 camelCase，但也可能混入 snake_case；
    // 结构体负责主路径解析，通用辅助函数再补充兼容字段读取。
    conversation_id: Option<String>,
    generation_id: Option<String>,
    hook_event_name: Option<String>,
    model: Option<String>,
    cursor_version: Option<String>,
    workspace_roots: Option<Vec<String>>,
    transcript_path: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    command: Option<String>,
    status: Option<String>,
    failure_type: Option<String>,
    error_message: Option<String>,
    reason: Option<String>,
    duration: Option<u64>,
    cwd: Option<String>,
}

pub struct CursorAdapter;

impl SourceAdapter for CursorAdapter {
    fn source(&self) -> &'static str {
        "cursor"
    }

    fn parse(&self, input: Value, ctx: &HookParseContext) -> AppResult<AgentEvent> {
        // 先把已知字段按 Cursor 结构解码，再组合成统一事件。
        let raw: CursorHookInput = serde_json::from_value(input.clone())
            .map_err(|err| AppError::invalid("failed to parse cursor hook stdin", err))?;

        let (capability, suggested_mode) = map_cursor_mode(
            raw.hook_event_name.as_deref(),
            raw.tool_name.as_deref(),
            raw.status.as_deref(),
            raw.reason.as_deref(),
        );

        Ok(build_event(ctx, &input, capability, suggested_mode))
    }
}

fn map_cursor_mode(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    status: Option<&str>,
    reason: Option<&str>,
) -> (AgentCapability, Option<Mode>) {
    // Cursor 事件更细，既有 beforeShellExecution 这种显式命令执行事件，
    // 也有 afterFileEdit 这类文件编辑事件，这里统一翻译为能力与建议模式。
    match raw_event.unwrap_or_default() {
        "sessionStart" => (AgentCapability::Idle, Some(Mode::Green)),
        "beforeSubmitPrompt" | "afterAgentThought" | "subagentStart" | "preCompact" => {
            (AgentCapability::Thinking, Some(Mode::Thinking))
        }
        "afterAgentResponse" | "afterFileEdit" | "afterTabFileEdit" => {
            (AgentCapability::Generating, Some(Mode::Ai))
        }
        "postToolUseFailure" => (AgentCapability::Failed, Some(Mode::Error)),
        "beforeShellExecution" | "beforeMCPExecution" | "beforeReadFile" | "beforeTabFileRead" => {
            (AgentCapability::RunningCommand, Some(Mode::Busy))
        }
        "afterShellExecution" | "afterMCPExecution" => (AgentCapability::RunningCommand, None),
        "preToolUse" => match tool_name.unwrap_or_default() {
            "Write" | "Edit" | "MultiEdit" => (AgentCapability::Generating, Some(Mode::Ai)),
            "Shell" => (AgentCapability::RunningCommand, Some(Mode::Busy)),
            _ => (AgentCapability::RunningCommand, Some(Mode::Busy)),
        },
        "subagentStop" => match status.unwrap_or_default() {
            "error" | "aborted" => (AgentCapability::Failed, Some(Mode::Error)),
            _ => (AgentCapability::Succeeded, Some(Mode::Success)),
        },
        "stop" => match status.unwrap_or_default() {
            "error" | "aborted" => (AgentCapability::Failed, Some(Mode::Error)),
            _ => (AgentCapability::Succeeded, Some(Mode::Success)),
        },
        "sessionEnd" => match reason.unwrap_or_default() {
            _ => (AgentCapability::Idle, Some(Mode::Demo)),
        },
        "workspaceOpen" => (AgentCapability::Idle, Some(Mode::Demo)),
        _ => (AgentCapability::Unknown, None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cursor_session_start_maps_to_green() {
        let ctx = HookParseContext {
            source: "cursor".into(),
            explicit_mode: Mode::Thinking,
            current_dir: ".".into(),
            ttl: None,
        };
        let event = CursorAdapter
            .parse(
                json!({
                    "conversationId": "abc",
                    "hookEventName": "sessionStart",
                }),
                &ctx,
            )
            .expect("cursor parse should succeed");
        assert_eq!(event.capability, AgentCapability::Idle);
        assert_eq!(event.suggested_mode, Some(Mode::Green));
    }

    #[test]
    fn cursor_failure_maps_to_error() {
        let ctx = HookParseContext {
            source: "cursor".into(),
            explicit_mode: Mode::Busy,
            current_dir: ".".into(),
            ttl: None,
        };
        let event = CursorAdapter
            .parse(
                json!({
                    "conversationId": "abc",
                    "hookEventName": "postToolUseFailure",
                }),
                &ctx,
            )
            .expect("cursor parse should succeed");
        assert_eq!(event.capability, AgentCapability::Failed);
        assert_eq!(event.suggested_mode, Some(Mode::Error));
    }

    #[test]
    fn cursor_session_and_turn_use_camel_case_fields() {
        let ctx = HookParseContext {
            source: "cursor".into(),
            explicit_mode: Mode::Busy,
            current_dir: "/tmp/project".into(),
            ttl: None,
        };
        let event = CursorAdapter
            .parse(
                json!({
                    "conversationId": "conv-1",
                    "generationId": "gen-1",
                    "hookEventName": "beforeShellExecution",
                    "toolUseId": "tool-1",
                    "tool_name": "Shell"
                }),
                &ctx,
            )
            .expect("cursor parse should succeed");
        assert_eq!(event.session, "conv-1");
        assert_eq!(event.turn.as_deref(), Some("tool-1"));
        assert_eq!(event.raw_event.as_deref(), Some("beforeShellExecution"));
    }
}
