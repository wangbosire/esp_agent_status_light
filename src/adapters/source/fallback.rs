//! 兜底来源解析器。
//!
//! 当指定来源解析失败或未注册时，fallback 会尽最大努力保留 session/cwd/raw_event
//! 等信息，确保 Hook 不会因为 JSON 细节变化而完全失效。

use serde_json::Value;

use crate::adapters::source::build_event;
use crate::model::{AgentCapability, AgentEvent, AppResult, HookParseContext};
use crate::ports::source::SourceAdapter;

pub struct FallbackAdapter;

impl FallbackAdapter {
    pub fn parse_lossy(&self, input: Value, ctx: &HookParseContext) -> AgentEvent {
        // 兜底解析永远不假设具体语义，只保留最保守的 Unknown 能力。
        build_event(ctx, &input, AgentCapability::Unknown, None)
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
