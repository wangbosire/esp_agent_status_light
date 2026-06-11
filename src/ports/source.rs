use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::model::{AgentEvent, AppResult, HookParseContext};

/// SourceAdapter 负责把不同 Agent 的 Hook stdin 归一成统一事件。
pub trait SourceAdapter: Send + Sync {
    /// 返回该 adapter 负责的来源名，例如 `codex` / `cursor` / `claude`。
    fn source(&self) -> &'static str;
    /// 将宿主工具原始 JSON 转换为核心层可理解的 `AgentEvent`。
    fn parse(&self, input: Value, ctx: &HookParseContext) -> AppResult<AgentEvent>;
}

#[derive(Default, Clone)]
pub struct SourceAdapterRegistry {
    /// 按来源名注册所有可用 adapter。
    adapters: HashMap<String, Arc<dyn SourceAdapter>>,
}

impl SourceAdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with<A>(mut self, adapter: A) -> Self
    where
        A: SourceAdapter + 'static,
    {
        // 注册表采用覆盖式插入，便于测试中替换默认实现。
        self.adapters
            .insert(adapter.source().to_string(), Arc::new(adapter));
        self
    }

    pub fn get(&self, source: &str) -> Option<Arc<dyn SourceAdapter>> {
        self.adapters.get(source).cloned()
    }

    pub fn parse_or_fallback(&self, input: Value, ctx: &HookParseContext) -> AgentEvent {
        let fallback = self.get("*").expect("fallback adapter must be registered");

        // 指定来源解析失败时不会让 Hook 整体失败，而是回退到 lossy 解析，
        // 这与“Hook 失败不得阻塞 Agent 主流程”的设计目标一致。
        self.get(&ctx.source)
            .and_then(|adapter| adapter.parse(input.clone(), ctx).ok())
            .unwrap_or_else(|| {
                fallback
                    .parse(input, ctx)
                    .expect("fallback adapter should never fail")
            })
    }
}
