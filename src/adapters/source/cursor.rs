//! Cursor Hook stdin 解析器。

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{
    AgentCapability, AgentEvent, AppError, AppResult, EventSemantics, HookParseContext, Mode,
};
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

/// Cursor Hook 解析器。
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

        Ok(build_event(
            ctx,
            &input,
            capability,
            suggested_mode,
            semantics_for_cursor(
                raw.hook_event_name.as_deref(),
                raw.tool_name.as_deref(),
                raw.status.as_deref(),
                raw.reason.as_deref(),
            ),
        ))
    }
}

/// 将 Cursor 原始 Hook 事件映射为统一能力与建议模式。
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
        "beforeReadFile" | "beforeTabFileRead" => {
            // 文件读取本质上也是 AI 在处理上下文的一部分。
            // 按最新规则，这类读文件动作不再展示为泛 busy，而是直接进入 `ai`。
            (AgentCapability::Generating, Some(Mode::Ai))
        }
        "beforeShellExecution" | "beforeMCPExecution" => {
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
        "sessionEnd" => {
            let _ = reason;
            (AgentCapability::Idle, Some(Mode::Demo))
        }
        "workspaceOpen" => (AgentCapability::Idle, Some(Mode::Demo)),
        _ => (AgentCapability::Unknown, None),
    }
}

fn semantics_for_cursor(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    status: Option<&str>,
    reason: Option<&str>,
) -> EventSemantics {
    match raw_event.unwrap_or_default() {
        "sessionStart" | "workspaceOpen" => EventSemantics::Continuation,
        "beforeSubmitPrompt" | "afterAgentThought" | "subagentStart" | "preCompact" => {
            EventSemantics::Continuation
        }
        "afterAgentResponse" | "afterFileEdit" | "afterTabFileEdit" => EventSemantics::FileWrite,
        "postToolUseFailure" => EventSemantics::Failure,
        "beforeReadFile" | "beforeTabFileRead" => EventSemantics::FileRead,
        "beforeShellExecution" | "beforeMCPExecution" => EventSemantics::ExplicitToolExecution,
        "afterShellExecution" | "afterMCPExecution" => EventSemantics::Completion,
        "preToolUse" => match tool_name.unwrap_or_default() {
            "Write" | "Edit" | "MultiEdit" => EventSemantics::FileWrite,
            "Shell" => EventSemantics::ExplicitToolExecution,
            _ => EventSemantics::ExplicitToolExecution,
        },
        "subagentStop" | "stop" => {
            if matches!(status, Some("error" | "aborted")) {
                EventSemantics::Failure
            } else {
                EventSemantics::Completion
            }
        }
        "sessionEnd" => {
            let _ = reason;
            EventSemantics::Completion
        }
        _ => EventSemantics::Unknown,
    }
}

// 测试实现拆到独立目录，避免与 Cursor Hook 事件解析主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/source/cursor_tests.rs"]
mod tests;
