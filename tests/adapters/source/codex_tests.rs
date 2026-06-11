//! `adapters::source::codex` 模块测试。

use serde_json::json;

use super::*;

#[test]
fn codex_session_start_maps_to_green() {
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
