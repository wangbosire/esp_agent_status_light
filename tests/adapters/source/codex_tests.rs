//! `adapters::source::codex` 模块测试。

use serde_json::json;

use super::*;

#[test]
fn codex_session_start_maps_to_green() {
    // 会话初始化应该稳定给出“已就绪”的绿色态，而不是沿用外部 fallback mode。
    let ctx = HookParseContext {
        source: "codex".into(),
        explicit_mode: Mode::Thinking,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = CodexAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "SessionStart"
            }),
            &ctx,
        )
        .expect("codex parse should succeed");
    assert_eq!(event.capability, AgentCapability::Idle);
    assert_eq!(event.suggested_mode, Some(Mode::Green));
}

#[test]
fn codex_bash_maps_to_busy() {
    // Bash 明确属于工具执行态，应映射为 busy。
    let ctx = HookParseContext {
        source: "codex".into(),
        explicit_mode: Mode::Busy,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = CodexAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
            }),
            &ctx,
        )
        .expect("codex parse should succeed");
    assert_eq!(event.capability, AgentCapability::RunningCommand);
    assert_eq!(event.suggested_mode, Some(Mode::Busy));
}

#[test]
fn codex_read_maps_to_ai() {
    // 读文件按最新规则属于 AI 处理内容的一部分，应直接映射为 ai。
    let ctx = HookParseContext {
        source: "codex".into(),
        explicit_mode: Mode::Ai,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = CodexAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
            }),
            &ctx,
        )
        .expect("codex parse should succeed");
    assert_eq!(event.capability, AgentCapability::Generating);
    assert_eq!(event.suggested_mode, Some(Mode::Ai));
}

#[test]
fn codex_post_tool_use_preserves_ai_fallback_mode() {
    // PostToolUse 本身缺少稳定细粒度语义，因此要保留安装器预先写入的 fallback mode。
    let ctx = HookParseContext {
        source: "codex".into(),
        explicit_mode: Mode::Ai,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = CodexAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "PostToolUse",
                "tool_name": "apply_patch",
            }),
            &ctx,
        )
        .expect("codex parse should succeed");
    assert_eq!(event.capability, AgentCapability::RunningCommand);
    assert_eq!(event.suggested_mode, Some(Mode::Ai));
}
