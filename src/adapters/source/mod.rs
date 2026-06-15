//! Hook 来源解析公共辅助。
//!
//! 每个宿主工具的 source adapter 只负责识别“这条事件表示什么”，
//! 这里则集中处理 session/cwd/raw_event/turn 等通用字段提取和事件组装。

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod fallback;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use serde_json::Value;

use crate::model::{
    AgentCapability, AgentEvent, AgentSource, EventSemantics, HookParseContext, Mode,
};
use crate::ports::source::SourceAdapterRegistry;

/// 构建默认来源解析器注册表。
///
/// 这里集中声明项目当前支持的 Hook 来源，新增来源时通常只需要：
/// 1. 新增一个 adapter；
/// 2. 在这里注册；
/// 3. 保持 fallback 适配器继续存在。
pub fn registry() -> SourceAdapterRegistry {
    // 注册顺序本身不影响解析，但 fallback 必须始终存在。
    // 如果某个具体 adapter 解析失败，registry 会自动回退到 fallback，保证 Hook 不阻断主流程。
    SourceAdapterRegistry::new()
        .with(codex::CodexAdapter)
        .with(cursor::CursorAdapter)
        .with(claude::ClaudeAdapter)
        .with(fallback::FallbackAdapter)
}

/// 所有 source adapter 共用的 session 提取兜底逻辑。
/// 字段顺序完全按技术方案编码，避免不同 adapter 私下改优先级。
pub fn extract_session_or_hash(input: &Value, ctx: &HookParseContext) -> String {
    // 字段优先级严格按照技术方案：
    // session_id -> conversation_id -> generation_id -> cwd -> workspace_roots[0] -> transcript_path -> hash。
    // 这样做的目的，是尽量让同一轮任务在不同宿主里落到同一个稳定 session 上。
    string_field(
        input,
        &[
            "session_id",
            "conversation_id",
            "conversationId",
            "generation_id",
            "generationId",
        ],
    )
    .or_else(|| string_field(input, &["cwd"]))
    .or_else(|| {
        input
            .get("workspace_roots")
            .or_else(|| input.get("workspaceRoots"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
    .or_else(|| string_field(input, &["transcript_path"]))
    .unwrap_or_else(|| {
        // 如果所有显式字段都没有，就用 source + cwd 做一个稳定 hash，
        // 至少保证同一次运行不会每次都生成不同 session。
        let mut hasher = DefaultHasher::new();
        ctx.source.hash(&mut hasher);
        ctx.current_dir.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    })
}

/// 从原始 Hook JSON 中提取工作目录。
///
/// 返回的 cwd 主要用于：
/// 1. 补充 `AgentEvent.cwd`；
/// 2. 在缺少显式 session 时参与会话标识推断；
/// 3. 给日志和排障提供上下文。
pub fn extract_cwd(input: &Value) -> Option<PathBuf> {
    // 优先使用显式 cwd；没有时再退回 workspaceRoots 的第一个根目录。
    // 这个函数只负责“尽量提取一个合理 cwd”，不做任何路径有效性判断。
    string_field(input, &["cwd"])
        .map(PathBuf::from)
        .or_else(|| {
            input
                .get("workspace_roots")
                .or_else(|| input.get("workspaceRoots"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
}

/// 提取宿主工具的原始事件名。
///
/// 这个字段主要用于日志与排障展示，不应该被 daemon/router 当成稳定业务语义直接依赖。
pub fn extract_raw_event(input: &Value) -> Option<String> {
    string_field(input, &["hook_event_name", "hookEventName"])
}

/// 提取宿主工具的原始工具名。
///
/// 与 `raw_event` 一样，它更多是面向排障上下文，而不是核心决策输入。
pub fn extract_raw_tool(input: &Value) -> Option<String> {
    string_field(input, &["tool_name", "toolName"])
}

/// 提取当前事件的轮次或工具调用标识。
///
/// 这个字段用于帮助排查“当前状态对应的是哪一轮动作”，
/// 以及未来如果需要引入更精细的会话内覆盖规则时作为稳定依据。
pub fn extract_turn(input: &Value) -> Option<String> {
    string_field(
        input,
        &[
            "turn_id",
            "tool_use_id",
            "toolUseId",
            "generation_id",
            "generationId",
        ],
    )
}

/// 依次尝试多个字段名并返回第一个字符串值。
///
/// 它主要解决不同宿主工具在 `snake_case` / `camelCase` 上的不一致问题。
pub fn string_field(input: &Value, names: &[&str]) -> Option<String> {
    // 依次尝试多个可能字段名，兼容 snake_case / camelCase 差异。
    names.iter().find_map(|name| {
        input
            .get(name)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

/// 将来源 adapter 的解析结果组装为统一 `AgentEvent`。
///
/// 各 adapter 只需要专注于“如何识别能力与建议模式”，
/// 通用字段提取与兜底逻辑则在这里统一完成。
pub fn build_event(
    ctx: &HookParseContext,
    input: &Value,
    capability: AgentCapability,
    suggested_mode: Option<Mode>,
    semantics: EventSemantics,
) -> AgentEvent {
    // 所有来源最终都收敛到统一 `AgentEvent`，
    // 这正是 Adapter 模式在这里的核心价值。
    // 上层 router 只看这个统一结构，不再关心宿主私有字段名。
    AgentEvent {
        source: AgentSource::new(ctx.source.clone()),
        session: extract_session_or_hash(input, ctx),
        capability,
        suggested_mode,
        cwd: extract_cwd(input).or(Some(ctx.current_dir.clone())),
        raw_event: extract_raw_event(input),
        raw_tool: extract_raw_tool(input),
        turn: extract_turn(input),
        semantics,
    }
}
