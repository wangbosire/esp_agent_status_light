//! Codex Hook 安装器。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::adapters::install::{codex_like_uninstall, decorate_command_fields};
use crate::model::{AppResult, HookCommand, HookSpec, InstallScope, Mode};
use crate::ports::hook_install::HookInstallAdapter;
use crate::ports::platform::PlatformAdapter;

pub struct CodexInstallAdapter;

impl HookInstallAdapter for CodexInstallAdapter {
    fn target(&self) -> &'static str {
        "codex"
    }

    fn config_path(&self, scope: &InstallScope) -> PathBuf {
        match scope {
            InstallScope::Global => dirs_home().join(".codex").join("hooks.json"),
            InstallScope::Project(root) => root.join(".codex").join("hooks.json"),
        }
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
        // 先卸载旧版本托管 Hook，再重新写入，保证 install 幂等。
        let mut config = codex_like_uninstall(config, hook_id);
        let root = config
            .as_object_mut()
            .expect("codex config should be object");
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks_map = hooks.as_object_mut().expect("hooks should be object");

        for spec in specs {
            let entry = hooks_map
                .entry(spec.event.clone())
                .or_insert_with(|| json!([]));
            let items = entry.as_array_mut().expect("event entries should be array");
            let mut group = json!({
                "hooks": [{
                    "type": "command",
                    "timeout": 10,
                    "statusMessage": "Updating AgentStatusLight"
                }]
            });
            decorate_command_fields(platform, &mut group["hooks"][0], &spec.command);
            if let Some(matcher) = &spec.matcher {
                group["matcher"] = json!(matcher);
            }
            items.push(group);
        }

        Ok(config)
    }

    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value> {
        Ok(codex_like_uninstall(config, hook_id))
    }
}

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

fn dirs_home() -> PathBuf {
    // 保持实现简单：第一阶段直接依赖 HOME，后续如需更复杂目录策略再扩展平台层。
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// 测试实现拆到独立目录，避免与 Codex Hook 安装逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/codex_tests.rs"]
mod tests;
