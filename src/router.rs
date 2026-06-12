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
    /// 创建一个空状态路由器。
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

    /// 移除所有已经到达过期时间的状态。
    ///
    /// 过期判断只依赖状态自己的 `expires_at`，不关心来源类型。
    pub fn prune_expired(&mut self, now: DateTime<Utc>) {
        // 过期清理统一集中在这里，避免调用方自己重复写 TTL 判断逻辑。
        self.states
            .retain(|_, state| state.expires_at.is_none_or(|expires_at| expires_at > now));
    }

    /// 计算当前全局应展示的最终模式。
    ///
    /// 算法分两步：
    /// 1. 过滤掉已过期状态；
    /// 2. 在剩余状态中按“优先级更高、更新时间更近”选出一条。
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

    /// 生成 `status --verbose` 所需的来源状态快照。
    ///
    /// 返回值是面向输出协议的稳定结构，不直接暴露内部 `SourceState`。
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

    /// 判断同一个 `(source, session)` 的新状态是否应覆盖旧状态。
    ///
    /// 当前策略整体偏“最后写入优先”，只保留少量针对 AI 生成态的保护规则。
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

        // 现场里“AI 生成内容/长任务处理中”最容易被一类笼统 continuation 事件冲掉：
        // 例如 Claude 的 `PostToolBatch` 往往只说明“这一轮工具链还在继续推进”，
        // 但并没有明确表明当前已经切回命令执行态。
        //
        // 如果上一条明确状态已经是 `ai`，而新事件只是这种缺少工具细节的泛 busy，
        // 就保留当前 `ai`，让用户能真实看到生成态持续存在。
        //
        // 这里仍然只在“同一 source + session”内生效；
        // 对于真正明确的命令执行事件（例如 Bash / Shell），仍然允许 busy 正常覆盖 ai。
        if should_preserve_ai_generation_state(current, candidate) {
            return false;
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

/// 将 `std::time::Duration` 转换为 `chrono::Duration`。
fn duration_to_chrono(duration: std::time::Duration) -> ChronoDuration {
    // `chrono` 与 `std::time` 分属两个世界，这里统一做一次桥接转换。
    ChronoDuration::seconds(duration.as_secs() as i64)
}

/// 判断是否应保留当前 `ai` 生成态，而不是被泛化的 `busy` 覆盖。
///
/// 这是针对某些宿主只上报“工具链继续推进”而不提供明确工具语义时的体验保护。
fn should_preserve_ai_generation_state(current: &SourceState, candidate: &SourceState) -> bool {
    if current.mode != Mode::Ai || candidate.mode != Mode::Busy {
        return false;
    }

    // 只有“没有明确工具语义的泛 continuation busy”才不应冲掉 ai。
    // 明确的 shell/工具执行仍应展示为 busy。
    if matches!(
        candidate.raw_tool.as_deref(),
        Some("Bash" | "Shell" | "Read" | "MCP" | "MultiEdit" | "Edit" | "Write" | "apply_patch")
    ) {
        return false;
    }

    matches!(
        candidate.raw_event.as_deref(),
        Some("PostToolBatch" | "afterAgentThought")
    )
}

// 测试实现拆到独立目录，避免与状态路由主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../tests/core/router_tests.rs"]
mod tests;
