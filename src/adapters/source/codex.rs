//! Codex Hook stdin 解析器。

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{AgentCapability, AgentEvent, AppError, AppResult, HookParseContext, Mode};
use crate::ports::source::SourceAdapter;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CodexHookInput {
    // 这些字段不一定都会在当前逻辑里使用，但保留结构定义有两个价值：
    // 1. 能在 JSON 结构变化时尽早暴露解析异常；
    // 2. 方便后续扩展更多映射规则时直接取用。
    session_id: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
    hook_event_name: Option<String>,
    model: Option<String>,
    turn_id: Option<String>,
    permission_mode: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_response: Option<Value>,
}

pub struct CodexAdapter;

impl SourceAdapter for CodexAdapter {
    fn source(&self) -> &'static str {
        "codex"
    }

    fn parse(&self, input: Value, ctx: &HookParseContext) -> AppResult<AgentEvent> {
        // 先按 Codex 官方 Hook JSON 做结构化解析，再映射成统一能力模型。
        let raw: CodexHookInput = serde_json::from_value(input.clone())
            .map_err(|err| AppError::invalid("failed to parse codex hook stdin", err))?;

        let (capability, suggested_mode) = map_codex_mode(
            raw.hook_event_name.as_deref(),
            raw.tool_name.as_deref(),
            ctx.explicit_mode,
        );

        Ok(build_event(ctx, &input, capability, suggested_mode))
    }
}

fn map_codex_mode(
    raw_event: Option<&str>,
    tool_name: Option<&str>,
    fallback_mode: Mode,
) -> (AgentCapability, Option<Mode>) {
    // 这里严格编码 Codex 事件到统一能力/模式的映射，
    // 不把这些判断散落到 daemon 或 router 中。
    match raw_event.unwrap_or_default() {
        "SessionStart" => (AgentCapability::Idle, Some(Mode::Green)),
        "UserPromptSubmit" | "PreCompact" | "PostCompact" | "SubagentStart" => {
            (AgentCapability::Thinking, Some(Mode::Thinking))
        }
        "PermissionRequest" => (AgentCapability::WaitingForUser, Some(Mode::Alarm)),
        "SubagentStop" | "Stop" => (AgentCapability::Succeeded, Some(Mode::Success)),
        // `PostToolUse` 官方并不稳定提供可供第一阶段统一解析的成功/失败语义，
        // 因此这里必须保留安装器写入的兜底 mode：
        // Bash 继续保持 busy，Edit/Write/apply_patch 则继续保持 ai。
        "PostToolUse" => (AgentCapability::RunningCommand, Some(fallback_mode)),
        "PreToolUse" => match tool_name.unwrap_or_default() {
            "Bash" => (AgentCapability::RunningCommand, Some(Mode::Busy)),
            // 按最新产品规则，文件读写都属于“AI 正在处理内容”：
            // - `Read` 表示正在读取文件上下文；
            // - `apply_patch` / `Edit` / `Write` 表示正在改写文件。
            // 这些都统一展示为 `ai`，让用户能看到“内容处理态”而不是泛 busy。
            "Read" | "apply_patch" | "Edit" | "Write" => {
                (AgentCapability::Generating, Some(Mode::Ai))
            }
            _ => (AgentCapability::RunningCommand, Some(Mode::Busy)),
        },
        _ => (AgentCapability::Unknown, None),
    }
}

// 测试实现拆到独立目录，避免与 Codex Hook 事件解析主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/source/codex_tests.rs"]
mod tests;
