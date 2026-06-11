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

        // 按用户最新要求：同一个 `(source, session)` 始终以最后一条状态为准。
        // 这里不再对 error/alarm/success/thinking 做任何额外保护或特判，
        // 只保留“过期状态可被接管”与“新状态写回状态池”这两个基础规则。
        //
        // 全局 effective mode 的优先级比较，仍然只发生在不同 source/session 之间；
        // 但单个会话自己的状态快照，必须始终反映该会话最新一次上报的 mode。
        candidate.updated_at >= current.updated_at
    }
}

fn duration_to_chrono(duration: std::time::Duration) -> ChronoDuration {
    // `chrono` 与 `std::time` 分属两个世界，这里统一做一次桥接转换。
    ChronoDuration::seconds(duration.as_secs() as i64)
}

// 测试实现拆到独立目录，避免与状态路由主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../tests/core/router_tests.rs"]
mod tests;
