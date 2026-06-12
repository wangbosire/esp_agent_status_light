//! 兜底来源解析器。
//!
//! 当指定来源解析失败或未注册时，fallback 会尽最大努力保留 session/cwd/raw_event
//! 等信息，确保 Hook 不会因为 JSON 细节变化而完全失效。

use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{AgentCapability, AgentEvent, AppResult, EventSemantics, HookParseContext};
use crate::ports::source::SourceAdapter;

pub struct FallbackAdapter;

impl FallbackAdapter {
    /// 以“永不失败”的方式做最保守的事件归一。
    ///
    /// 它不会尝试从宿主私有字段里推断业务语义，
    /// 只负责把还能拿到的通用上下文尽量保留下来。
    pub fn parse_lossy(&self, input: Value, ctx: &HookParseContext) -> AgentEvent {
        // 兜底解析永远不假设具体语义，只保留最保守的 Unknown 能力。
        // 这样即使宿主升级了 Hook JSON 结构，系统至少还能保住 session/cwd 等排障上下文。
        build_event(
            ctx,
            &input,
            AgentCapability::Unknown,
            None,
            EventSemantics::Unknown,
        )
    }
}

impl SourceAdapter for FallbackAdapter {
    fn source(&self) -> &'static str {
        "*"
    }

    fn parse(&self, input: Value, ctx: &HookParseContext) -> AppResult<AgentEvent> {
        Ok(self.parse_lossy(input, ctx))
    }
}
