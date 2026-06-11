//! `router` 模块测试。
//!
//! 这些测试虽然迁移到了仓库根目录的 `tests/` 下，但仍作为 `router` 的子模块编译，
//! 因而可以继续验证私有状态替换规则，不改变现有行为。

use chrono::Utc;

use super::*;
use crate::model::{AgentEvent, AgentSource};

#[test]
fn resolve_mode_prefers_suggested_mode() {
    let ctx = HookParseContext {
        source: "codex".into(),
        explicit_mode: Mode::Busy,
        current_dir: ".".into(),
        ttl: None,
    };
    let event = AgentEvent {
        source: AgentSource::new("codex"),
        session: "a".into(),
        capability: AgentCapability::Thinking,
        suggested_mode: Some(Mode::Alarm),
        cwd: None,
        raw_event: None,
        raw_tool: None,
        turn: None,
    };
    assert_eq!(resolve_mode(&ctx, &event), Mode::Alarm);
}

#[test]
fn manual_off_clears_all_states() {
    let now = Utc::now();
    let mut router = StateRouter::new();
    router.apply_send(
        &SendPayload {
            mode: Mode::Busy,
            source: "codex".into(),
            session: "abc".into(),
            ttl: None,
            hook_id: None,
            raw_event: None,
            raw_tool: None,
            capability: None,
            suggested_mode: None,
            cwd: None,
            turn: None,
        },
        now,
    );
    assert_eq!(router.effective_mode(now), Mode::Busy);

    router.apply_send(
        &SendPayload {
            mode: Mode::Off,
            source: "manual".into(),
            session: "manual".into(),
            ttl: None,
            hook_id: None,
            raw_event: None,
            raw_tool: None,
            capability: None,
            suggested_mode: None,
            cwd: None,
            turn: None,
        },
        now,
    );
    assert_eq!(router.effective_mode(now), Mode::Off);
    assert!(router.snapshot(now).is_empty());
}

#[test]
fn latest_state_overrides_error_in_same_turn() {
    let now = Utc::now();
    let mut router = StateRouter::new();
    let error = SendPayload {
        mode: Mode::Error,
        source: "codex".into(),
        session: "abc".into(),
        ttl: Some(600),
        hook_id: None,
        raw_event: Some("PostToolUseFailure".into()),
        raw_tool: Some("Bash".into()),
        capability: Some(AgentCapability::Failed),
        suggested_mode: Some(Mode::Error),
        cwd: None,
        turn: Some("turn-1".into()),
    };
    router.apply_send(&error, now);

    let success = SendPayload {
        mode: Mode::Success,
        source: "codex".into(),
        session: "abc".into(),
        ttl: Some(30),
        hook_id: None,
        raw_event: Some("Stop".into()),
        raw_tool: None,
        capability: Some(AgentCapability::Succeeded),
        suggested_mode: Some(Mode::Success),
        cwd: None,
        turn: Some("turn-1".into()),
    };
    assert_eq!(router.apply_send(&success, now), Mode::Success);
    assert_eq!(router.effective_mode(now), Mode::Success);
}

#[test]
fn latest_state_overrides_alarm_in_same_session() {
    let now = Utc::now();
    let mut router = StateRouter::new();
    let alarm = SendPayload {
        mode: Mode::Alarm,
        source: "claude".into(),
        session: "abc".into(),
        ttl: Some(1800),
        hook_id: None,
        raw_event: Some("PermissionRequest".into()),
        raw_tool: None,
        capability: Some(AgentCapability::WaitingForUser),
        suggested_mode: Some(Mode::Alarm),
        cwd: None,
        turn: None,
    };
    router.apply_send(&alarm, now);

    let success = SendPayload {
        mode: Mode::Success,
        source: "claude".into(),
        session: "abc".into(),
        ttl: Some(30),
        hook_id: None,
        raw_event: Some("SessionEnd".into()),
        raw_tool: None,
        capability: Some(AgentCapability::Succeeded),
        suggested_mode: Some(Mode::Success),
        cwd: None,
        turn: None,
    };
    let later = now + ChronoDuration::seconds(1);
    assert_eq!(router.apply_send(&success, later), Mode::Success);
    assert_eq!(router.effective_mode(later), Mode::Success);
}

#[test]
fn latest_state_replaces_same_session_even_when_priority_is_lower() {
    let now = Utc::now();
    let mut router = StateRouter::new();

    let thinking = SendPayload {
        mode: Mode::Thinking,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(900),
        hook_id: None,
        raw_event: Some("UserPromptSubmit".into()),
        raw_tool: None,
        capability: Some(AgentCapability::Thinking),
        suggested_mode: Some(Mode::Thinking),
        cwd: None,
        turn: None,
    };
    assert_eq!(router.apply_send(&thinking, now), Mode::Thinking);

    let success = SendPayload {
        mode: Mode::Success,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(30),
        hook_id: None,
        raw_event: Some("SessionEnd".into()),
        raw_tool: None,
        capability: Some(AgentCapability::Succeeded),
        suggested_mode: Some(Mode::Success),
        cwd: None,
        turn: None,
    };
    let later = now + ChronoDuration::seconds(1);
    assert_eq!(router.apply_send(&success, later), Mode::Success);

    let snapshot = router.snapshot(later);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].mode, Mode::Success);
    assert_eq!(router.effective_mode(later), Mode::Success);
}

#[test]
fn generic_busy_continuation_does_not_override_ai_in_same_session() {
    let now = Utc::now();
    let mut router = StateRouter::new();

    let ai = SendPayload {
        mode: Mode::Ai,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(900),
        hook_id: None,
        raw_event: Some("PreToolUse".into()),
        raw_tool: Some("Write".into()),
        capability: Some(AgentCapability::Generating),
        suggested_mode: Some(Mode::Ai),
        cwd: None,
        turn: Some("turn-1".into()),
    };
    assert_eq!(router.apply_send(&ai, now), Mode::Ai);

    let generic_busy = SendPayload {
        mode: Mode::Busy,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(1800),
        hook_id: None,
        raw_event: Some("PostToolBatch".into()),
        raw_tool: None,
        capability: Some(AgentCapability::RunningCommand),
        suggested_mode: Some(Mode::Busy),
        cwd: None,
        turn: None,
    };
    let later = now + ChronoDuration::seconds(1);
    assert_eq!(router.apply_send(&generic_busy, later), Mode::Ai);

    let snapshot = router.snapshot(later);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].mode, Mode::Ai);
    assert_eq!(router.effective_mode(later), Mode::Ai);
}

#[test]
fn explicit_shell_busy_still_overrides_ai_in_same_session() {
    let now = Utc::now();
    let mut router = StateRouter::new();

    let ai = SendPayload {
        mode: Mode::Ai,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(900),
        hook_id: None,
        raw_event: Some("PreToolUse".into()),
        raw_tool: Some("Write".into()),
        capability: Some(AgentCapability::Generating),
        suggested_mode: Some(Mode::Ai),
        cwd: None,
        turn: Some("turn-1".into()),
    };
    assert_eq!(router.apply_send(&ai, now), Mode::Ai);

    let shell_busy = SendPayload {
        mode: Mode::Busy,
        source: "claude".into(),
        session: "session-1".into(),
        ttl: Some(1800),
        hook_id: None,
        raw_event: Some("PreToolUse".into()),
        raw_tool: Some("Bash".into()),
        capability: Some(AgentCapability::RunningCommand),
        suggested_mode: Some(Mode::Busy),
        cwd: None,
        turn: Some("turn-2".into()),
    };
    let later = now + ChronoDuration::seconds(1);
    assert_eq!(router.apply_send(&shell_busy, later), Mode::Busy);

    let snapshot = router.snapshot(later);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].mode, Mode::Busy);
    assert_eq!(router.effective_mode(later), Mode::Busy);
}
