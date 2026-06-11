//! `adapters::source::cursor` 模块测试。

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
