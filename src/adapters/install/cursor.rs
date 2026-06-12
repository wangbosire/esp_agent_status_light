//! Cursor Hook 安装器。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::adapters::install::{cursor_uninstall, install_cursor_like_hooks};
use crate::adapters::platform::user_home_dir;
use crate::model::{AppResult, HookCommand, HookSpec, InstallScope, Mode};
use crate::ports::hook_install::HookInstallAdapter;
use crate::ports::platform::PlatformAdapter;

/// Cursor Hook 安装器。
pub struct CursorInstallAdapter;

impl HookInstallAdapter for CursorInstallAdapter {
    fn target(&self) -> &'static str {
        "cursor"
    }

    fn config_path(&self, scope: &InstallScope) -> AppResult<PathBuf> {
        // Cursor 的项目级配置也放在仓库内 `.cursor/hooks.json`，与全局路径平行。
        Ok(match scope {
            InstallScope::Global => user_home_dir()?.join(".cursor").join("hooks.json"),
            InstallScope::Project(root) => root.join(".cursor").join("hooks.json"),
        })
    }

    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec> {
        // Cursor 的 Hook schema 与 Codex/Claude 不同，因此需要独立的事件清单。
        vec![
            // sessionStart 表示会话刚建立，先亮绿色表示“就绪”。
            spec(exe, "sessionStart", None, Mode::Green, 900),
            spec(exe, "beforeSubmitPrompt", None, Mode::Thinking, 900),
            spec(exe, "preToolUse", Some("Shell"), Mode::Busy, 1800),
            // Cursor 在真正落地文件写入前，会先经过通用 preToolUse。
            // 把编辑类工具单独挂成 ai，才能覆盖“正在生成/改写内容”的长过程。
            spec(exe, "preToolUse", Some("Write"), Mode::Ai, 900),
            spec(exe, "preToolUse", Some("Edit"), Mode::Ai, 900),
            spec(exe, "preToolUse", Some("MultiEdit"), Mode::Ai, 900),
            spec(exe, "beforeShellExecution", None, Mode::Busy, 1800),
            // Cursor 的读文件官方事件也应该直接进入 `ai`，
            // 这样“读上下文 -> 生成/修改内容”会保持同一条视觉状态链。
            spec(exe, "beforeReadFile", None, Mode::Ai, 900),
            spec(exe, "beforeTabFileRead", None, Mode::Ai, 900),
            // Cursor 回复正文时会触发 afterAgentResponse。
            // 这个事件正是“AI 正在生成内容”最稳定的官方信号之一。
            spec(exe, "afterAgentResponse", None, Mode::Ai, 900),
            spec(exe, "afterFileEdit", None, Mode::Ai, 900),
            spec(exe, "afterTabFileEdit", None, Mode::Ai, 900),
            spec(exe, "postToolUseFailure", None, Mode::Error, 600),
            spec(exe, "stop", None, Mode::Success, 30),
            spec(exe, "subagentStop", None, Mode::Success, 30),
        ]
    }

    fn install(
        &self,
        config: Value,
        specs: &[HookSpec],
        hook_id: &str,
        platform: &dyn PlatformAdapter,
    ) -> AppResult<Value> {
        // Cursor hooks 结构比 Codex/Claude 更扁平，因此走专用公共 helper。
        install_cursor_like_hooks(config, specs, hook_id, platform)
    }

    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value> {
        Ok(cursor_uninstall(config, hook_id))
    }
}

/// 构造一条 Cursor Hook 规则定义。
fn spec(exe: &Path, event: &str, matcher: Option<&str>, mode: Mode, ttl: u64) -> HookSpec {
    // `fallback_mode` 和 `ttl` 都在这里显式固化，
    // 这样 source adapter 与 install adapter 看到的是同一套规则基线。
    HookSpec {
        target: "cursor".into(),
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
                "cursor".into(),
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

// 测试实现拆到独立目录，避免与 Cursor Hook 安装逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/cursor_tests.rs"]
mod tests;
