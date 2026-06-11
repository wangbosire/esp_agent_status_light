//! Claude Hook 安装器。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::adapters::install::{codex_like_uninstall, decorate_command_fields};
use crate::model::{AppResult, HookCommand, HookSpec, InstallScope, Mode};
use crate::ports::hook_install::HookInstallAdapter;
use crate::ports::platform::PlatformAdapter;

pub struct ClaudeInstallAdapter;

impl HookInstallAdapter for ClaudeInstallAdapter {
    fn target(&self) -> &'static str {
        "claude"
    }

    fn config_path(&self, scope: &InstallScope) -> PathBuf {
        match scope {
            InstallScope::Global => dirs_home().join(".claude").join("settings.json"),
            InstallScope::Project(root) => root.join(".claude").join("settings.json"),
        }
    }

    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec> {
        // Claude 支持的事件相对更丰富，因此这里显式覆盖通知、失败、会话结束等场景。
        vec![
            // 会话刚打开时先显示绿色，表示 Agent 会话已建立、当前处于待命状态。
            spec(exe, "SessionStart", None, Mode::Green, 900),
            spec(exe, "UserPromptSubmit", None, Mode::Thinking, 900),
            spec(exe, "PreToolUse", Some("Bash"), Mode::Busy, 1800),
            // 文件读取按最新规则也属于 AI 内容处理态。
            spec(exe, "PreToolUse", Some("Read"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("Edit"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("MultiEdit"), Mode::Ai, 900),
            spec(exe, "PreToolUse", Some("Write"), Mode::Ai, 900),
            spec(exe, "PermissionRequest", None, Mode::Alarm, 1800),
            spec(exe, "Notification", None, Mode::Alarm, 1800),
            // Claude 用户确认后，通常会继续触发 PostToolUse。
            // 这里补上对应 Hook，确保 alarm 能及时回到 busy/ai。
            spec(exe, "PostToolUse", Some("Bash"), Mode::Busy, 1800),
            spec(exe, "PostToolUse", Some("Read"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("Edit"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("MultiEdit"), Mode::Ai, 900),
            spec(exe, "PostToolUse", Some("Write"), Mode::Ai, 900),
            // Claude 在某些连续工具场景下，会在单次工具完成后继续发出 PostToolBatch。
            // 这个事件没有稳定的 tool matcher，但它明确表示“工具阶段正在继续推进”，
            // 因此至少要把 alarm 及时推出，恢复到运行态 busy。
            spec(exe, "PostToolBatch", None, Mode::Busy, 1800),
            spec(exe, "PermissionDenied", None, Mode::Error, 600),
            spec(exe, "PostToolUseFailure", None, Mode::Error, 600),
            spec(exe, "StopFailure", None, Mode::Error, 600),
            spec(exe, "Stop", None, Mode::Success, 30),
            spec(exe, "SubagentStop", None, Mode::Success, 30),
            // 实际使用里 SessionEnd 往往就是“本轮任务完成”的唯一结束事件，
            // 因此这里也提供 success 兜底。若 stdin 中带有异常结束 reason，
            // SourceAdapter 会把它改写回 demo，不会误报成功。
            spec(exe, "SessionEnd", None, Mode::Success, 30),
        ]
    }

    fn install(
        &self,
        config: Value,
        specs: &[HookSpec],
        hook_id: &str,
        platform: &dyn PlatformAdapter,
    ) -> AppResult<Value> {
        // Claude 的 hooks 结构与 Codex 相近，因此复用相同的 uninstall 策略。
        let mut config = codex_like_uninstall(config, hook_id);
        let root = config
            .as_object_mut()
            .expect("claude config should be object");
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks_map = hooks.as_object_mut().expect("hooks should be object");

        for spec in specs {
            let entry = hooks_map
                .entry(spec.event.clone())
                .or_insert_with(|| json!([]));
            let items = entry.as_array_mut().expect("event hooks should be array");
            let mut group = json!({
                "hooks": [{
                    "type": "command",
                    "timeout": 10
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
    HookSpec {
        target: "claude".into(),
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
                "claude".into(),
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
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// 测试实现拆到独立目录，避免与 Claude Hook 安装逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/claude_tests.rs"]
mod tests;
