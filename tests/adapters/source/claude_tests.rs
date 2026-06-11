//! `adapters::source::claude` 模块测试。

use serde_json::json;

use super::*;

#[test]
fn claude_session_start_maps_to_green() {
    let ctx = HookParseContext {
        source: "claude".into(),
        explicit_mode: Mode::Thinking,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = ClaudeAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "SessionStart",
            }),
            &ctx,
        )
        .expect("claude parse should succeed");
    assert_eq!(event.capability, AgentCapability::Idle);
    assert_eq!(event.suggested_mode, Some(Mode::Green));
}

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

#[test]
fn claude_read_maps_to_ai() {
    let ctx = HookParseContext {
        source: "claude".into(),
        explicit_mode: Mode::Ai,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = ClaudeAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
            }),
            &ctx,
        )
        .expect("claude parse should succeed");
    assert_eq!(event.capability, AgentCapability::Generating);
    assert_eq!(event.suggested_mode, Some(Mode::Ai));
}

#[test]
fn claude_post_tool_use_preserves_ai_fallback_mode() {
    let ctx = HookParseContext {
        source: "claude".into(),
        explicit_mode: Mode::Ai,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = ClaudeAdapter
        .parse(
            json!({
                "session_id": "abc",
                "hook_event_name": "PostToolUse",
                "tool_name": "Write",
            }),
            &ctx,
        )
        .expect("claude parse should succeed");
    assert_eq!(event.capability, AgentCapability::RunningCommand);
    assert_eq!(event.suggested_mode, Some(Mode::Ai));
}
