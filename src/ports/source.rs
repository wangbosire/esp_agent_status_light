//! Hook 来源解析端口。
//!
//! 不同宿主工具的 Hook JSON 形状不同，这一层负责把它们收敛到统一事件模型。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::adapters::source::fallback::FallbackAdapter;
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
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个来源解析器并返回更新后的注册表。
    ///
    /// 采用链式 `with()` 写法，便于在默认注册和测试替换时保持装配代码紧凑。
    pub fn with<A>(mut self, adapter: A) -> Self
    where
        A: SourceAdapter + 'static,
    {
        // 注册表采用覆盖式插入，便于测试中替换默认实现。
        self.adapters
            .insert(adapter.source().to_string(), Arc::new(adapter));
        self
    }

    /// 根据来源名获取对应解析器。
    pub fn get(&self, source: &str) -> Option<Arc<dyn SourceAdapter>> {
        self.adapters.get(source).cloned()
    }

    /// 先按指定来源尝试解析，失败时回退到兜底解析器。
    ///
    /// 这样可以满足“Hook 失败不阻断主流程”的要求：
    /// 即使某个宿主升级了字段结构，系统也至少还能退回显式 mode 或默认逻辑。
    pub fn parse_or_fallback(&self, input: Value, ctx: &HookParseContext) -> AgentEvent {
        // 先尝试来源专属解析，再落到 fallback。
        // 这样即使某个宿主字段升级了，也不会让整个 Hook 入口直接失效。
        // 指定来源解析失败时不会让 Hook 整体失败，而是回退到 lossy 解析，
        // 这与“Hook 失败不得阻塞 Agent 主流程”的设计目标一致。
        self.get(&ctx.source)
            .and_then(|adapter| adapter.parse(input.clone(), ctx).ok())
            .unwrap_or_else(|| FallbackAdapter.parse_lossy(input, ctx))
    }
}
