//! Codex Hook 安装器。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::adapters::install::{codex_like_uninstall, install_codex_like_hooks};
use crate::adapters::platform::user_home_dir;
use crate::model::{AppResult, HookCommand, HookSpec, InstallScope, Mode};
use crate::ports::hook_install::HookInstallAdapter;
use crate::ports::platform::PlatformAdapter;

/// Codex Hook 安装器。
pub struct CodexInstallAdapter;

impl HookInstallAdapter for CodexInstallAdapter {
    fn target(&self) -> &'static str {
        "codex"
    }

    fn config_path(&self, scope: &InstallScope) -> AppResult<PathBuf> {
        Ok(match scope {
            InstallScope::Global => user_home_dir()?.join(".codex").join("hooks.json"),
            InstallScope::Project(root) => root.join(".codex").join("hooks.json"),
        })
    }

    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec> {
        // 事件与 matcher 组合严格按技术方案实现，不在这里做额外扩展。
        vec![
            // 会话刚打开时先显示绿色，表示“已连通、已就绪，但尚未进入思考/执行”。
            spec(exe, "SessionStart", None, Mode::Green, 900),
            spec(exe, "UserPromptSubmit", None, Mode::Thinking, 900),
            spec(exe, "PreToolUse", Some("Bash"), Mode::Busy, 1800),
            // 文件读取也属于 AI 处理内容的一部分，因此这里同样挂到 `ai`。
            spec(exe, "PreToolUse", Some("Read"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("apply_patch"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("Edit"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("Write"), Mode::Ai, 900),
            spec(exe, "PermissionRequest", None, Mode::Alarm, 1800),
            // 用户完成授权后，Codex 往往通过 PostToolUse 继续推进流程。
            // 如果这里没有对应 Hook，alarm 会一直挂着，直到 Stop 才被覆盖。
            spec(exe, "PostToolUse", Some("Bash"), Mode::Busy, 1800),
            // 读取文件后的续流程仍应保持在 `ai`，避免刚读上下文就闪回 busy。
            spec(exe, "PostToolUse", Some("Read"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("apply_patch"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("Edit"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("Write"), Mode::Ai, 900),
            spec(exe, "Stop", None, Mode::Success, 30),
            spec(exe, "SubagentStop", None, Mode::Success, 30),
        ]
    }

    fn install(
        &self,
        config: Value,
        specs: &[HookSpec],
        hook_id: &str,
        platform: &dyn PlatformAdapter,
    ) -> AppResult<Value> {
        install_codex_like_hooks(
            config,
            specs,
            hook_id,
            platform,
            Some(10),
            Some("Updating AgentStatusLight"),
        )
    }

    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value> {
        Ok(codex_like_uninstall(config, hook_id))
    }
}

/// 构造一条 Codex Hook 规则定义。
fn spec(exe: &Path, event: &str, matcher: Option<&str>, mode: Mode, ttl: u64) -> HookSpec {
    // 每条 Hook 都以 `esp send ... --quiet --hook-id agent-status-light` 的稳定形式生成，
    // 便于后续卸载和排障。
    HookSpec {
        target: "codex".into(),
        event: event.into(),
        matcher: matcher.map(ToOwned::to_owned),
        fallback_mode: mode,
        ttl: Duration::from_secs(ttl),
        command: HookCommand {
            exe: exe.to_path_buf(),
            args: vec![
                "send".into(),
                "--mode".into(),
                mode.as_str().into(),
                "--source".into(),
                "codex".into(),
                "--session".into(),
                "auto".into(),
                "--ttl".into(),
                ttl.to_string(),
                "--quiet".into(),
                "--hook-id".into(),
                "agent-status-light".into(),
            ],
        },
    }
}

// 测试实现拆到独立目录，避免与 Codex Hook 安装逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/codex_tests.rs"]
mod tests;
