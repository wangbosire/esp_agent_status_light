//! 核心状态路由模块。
//!
//! 这里实现技术方案中最关键的规则：多来源状态合并、TTL 过期、优先级比较、
//! 同一轮 turn 的失败态保护，以及新的思考轮次如何覆盖旧失败态。

use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::model::{
    AgentCapability, AgentEvent, HookParseContext, Mode, SendPayload, SourceState,
    StatusSourceEntry,
};

/// `resolve_mode` 是 Hook 场景中最关键的决策函数之一。
/// 它必须稳定遵守技术方案给出的优先级顺序，避免不同 adapter 自己“拍脑袋”决定最终灯效。
pub fn resolve_mode(ctx: &HookParseContext, event: &AgentEvent) -> Mode {
    if ctx.source == "manual" {
        return ctx.explicit_mode;
    }

    if ctx.explicit_mode == Mode::Off {
        return Mode::Off;
    }

    if let Some(mode) = event.suggested_mode {
        return mode;
    }

    match event.capability {
        AgentCapability::Thinking => Mode::Thinking,
        AgentCapability::Generating => Mode::Ai,
        AgentCapability::RunningCommand => Mode::Busy,
        AgentCapability::WaitingForUser => Mode::Alarm,
        AgentCapability::Succeeded => Mode::Success,
        AgentCapability::Failed => Mode::Error,
        AgentCapability::Idle => Mode::Demo,
        AgentCapability::Unknown => ctx.explicit_mode,
    }
}

#[derive(Debug, Default)]
pub struct StateRouter {
    /// 以 `(source, session)` 为键保存每个来源当前的有效状态。
    states: HashMap<(String, String), SourceState>,
    /// `manual off` 触发后，需要在“没有任何状态”时保持熄灯，而不是退回 demo。
    /// 一旦后续收到新的非 off 状态，这个开关就会被清掉。
    manual_hold_off: bool,
}

impl StateRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 所有 daemon 内部状态变更最终都要经过这里。
    /// 这样“失败态不被成功态误覆盖”“manual off 清空全局”等规则只存在一份实现。
    pub fn apply_send(&mut self, payload: &SendPayload, now: DateTime<Utc>) -> Mode {
        self.prune_expired(now);

        let key = (payload.source.clone(), payload.session.clone());

        if payload.mode == Mode::Off {
            if payload.source == "manual" && payload.session == "manual" {
                self.states.clear();
                self.manual_hold_off = true;
            } else {
                self.states.remove(&key);
            }
            return self.effective_mode(now);
        }

        self.manual_hold_off = false;

        let ttl = payload
            .ttl
            .map(|ttl| ChronoDuration::seconds(ttl as i64))
            .or_else(|| payload.mode.default_ttl().map(duration_to_chrono));

        let candidate = SourceState {
            source: payload.source.clone(),
            session: payload.session.clone(),
            mode: payload.mode,
            raw_event: payload.raw_event.clone(),
            raw_tool: payload.raw_tool.clone(),
            turn: payload.turn.clone(),
            capability: payload.capability.clone(),
            suggested_mode: payload.suggested_mode,
            priority: payload.mode.priority(),
            updated_at: now,
            expires_at: ttl.map(|ttl| now + ttl),
        };

        match self.states.get(&key) {
            Some(current) if !self.should_replace(current, &candidate, now) => {}
            _ => {
                self.states.insert(key, candidate);
            }
        }

        self.effective_mode(now)
    }

    pub fn prune_expired(&mut self, now: DateTime<Utc>) {
        // 过期清理统一集中在这里，避免调用方自己重复写 TTL 判断逻辑。
        self.states
            .retain(|_, state| state.expires_at.is_none_or(|expires_at| expires_at > now));
    }

    pub fn effective_mode(&self, now: DateTime<Utc>) -> Mode {
        // 先按优先级选最高，再按更新时间选最近。
        // 这与技术方案中“展示最重要且最新的状态”保持一致。
        self.states
            .values()
            .filter(|state| state.expires_at.is_none_or(|expires_at| expires_at > now))
            .max_by(|left, right| {
                left.priority
                    .cmp(&right.priority)
                    .then(left.updated_at.cmp(&right.updated_at))
            })
            .map(|state| state.mode)
            .unwrap_or_else(|| {
                if self.manual_hold_off {
                    Mode::Off
                } else {
                    Mode::Demo
                }
            })
    }

    pub fn snapshot(&self, now: DateTime<Utc>) -> Vec<StatusSourceEntry> {
        // `status --verbose` 需要查看所有来源明细，因此这里把内部状态转成稳定输出结构。
        let mut items: Vec<_> = self
            .states
            .values()
            .map(|state| StatusSourceEntry {
                source: state.source.clone(),
                session: state.session.clone(),
                mode: state.mode,
                raw_event: state.raw_event.clone(),
                raw_tool: state.raw_tool.clone(),
                turn: state.turn.clone(),
                capability: state.capability.clone(),
                suggested_mode: state.suggested_mode,
                priority: state.priority,
                expires_in: state
                    .expires_at
                    .map(|expires_at| (expires_at - now).num_seconds()),
            })
            .collect();

        items.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| right.expires_in.cmp(&left.expires_in))
        });
        items
    }

    fn should_replace(
        &self,
        current: &SourceState,
        candidate: &SourceState,
        now: DateTime<Utc>,
    ) -> bool {
        // 旧状态已经过期时，任何新状态都应该直接接管。
        if current
            .expires_at
            .is_some_and(|expires_at| expires_at <= now)
        {
            return true;
        }

        // `error` 代表已经确认的失败结果，同一轮里的 success/demo 往往只是生命周期结束事件，
        // 不能把真实失败误刷成成功。
        //
        // 但 `alarm` 的语义不同：它表示“当前正在等待用户操作或授权”，并不是最终失败。
        // 一旦用户完成选择，后续同 session 的 busy/ai/thinking/success 都应该能够及时接管；
        // 否则就会出现日志里已经收到了 Stop/SessionEnd success，
        // `status` 却仍然卡在 alarm 的真实现场问题。
        if current.mode == Mode::Error
            && matches!(candidate.mode, Mode::Success | Mode::Demo)
            && same_turn_or_missing(&current.turn, &candidate.turn)
        {
            return false;
        }

        // 新一轮 prompt/session start 到来时，允许 thinking 把旧 error/alarm 冲掉，表示任务已重新开始。
        if matches!(current.mode, Mode::Error | Mode::Alarm)
            && candidate.mode == Mode::Thinking
            && (turn_changed(&current.turn, &candidate.turn)
                || candidate
                    .raw_event
                    .as_deref()
                    .is_some_and(is_new_round_event))
        {
            return true;
        }

        // 到这里说明：
        // 1. 旧状态没过期；
        // 2. 不是“失败态保护”场景；
        // 3. 也不是“新一轮 thinking 覆盖旧失败态”的特殊恢复场景。
        //
        // 对于同一个 `(source, session)`，状态池里保存的应当是“该会话的最新状态”，
        // 而不是“该会话历史上优先级最高的状态”。
        // 全局 effective mode 的优先级比较，应该只发生在不同 source/session 之间。
        //
        // 如果这里继续按 priority 决定是否替换，就会出现：
        // thinking(60) 收到后，后续 success(50) 无法写回状态池，
        // 结果 `status --verbose` 永远停在 thinking，这正是当前线上看到的 bug。
        candidate.updated_at >= current.updated_at
    }
}

fn same_turn_or_missing(current: &Option<String>, next: &Option<String>) -> bool {
    // turn 缺失时无法严格判断是否同轮，只能保守视为“可能同轮”。
    current.is_none() || next.is_none() || current == next
}

fn turn_changed(current: &Option<String>, next: &Option<String>) -> bool {
    matches!((current, next), (Some(left), Some(right)) if left != right)
}

fn is_new_round_event(raw_event: &str) -> bool {
    // 这些事件都意味着“新的工作轮次已经开始”，允许它们冲掉旧失败态。
    matches!(
        raw_event,
        "SessionStart" | "sessionStart" | "UserPromptSubmit" | "beforeSubmitPrompt"
    )
}

fn duration_to_chrono(duration: std::time::Duration) -> ChronoDuration {
    // `chrono` 与 `std::time` 分属两个世界，这里统一做一次桥接转换。
    ChronoDuration::seconds(duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
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
    fn success_does_not_override_error_in_same_turn() {
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
        assert_eq!(router.apply_send(&success, now), Mode::Error);
    }

    #[test]
    fn success_overrides_alarm_in_same_session() {
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
            // 真实 Claude 结束事件经常不带稳定 turn，这里显式覆盖现场场景。
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
}
