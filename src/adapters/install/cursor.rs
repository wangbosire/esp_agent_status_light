//! Cursor Hook 安装器。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::adapters::install::{cursor_uninstall, decorate_command_fields};
use crate::model::{AppResult, HookCommand, HookSpec, InstallScope, Mode};
use crate::ports::hook_install::HookInstallAdapter;
use crate::ports::platform::PlatformAdapter;

pub struct CursorInstallAdapter;

impl HookInstallAdapter for CursorInstallAdapter {
    fn target(&self) -> &'static str {
        "cursor"
    }

    fn config_path(&self, scope: &InstallScope) -> PathBuf {
        match scope {
            InstallScope::Global => dirs_home().join(".cursor").join("hooks.json"),
            InstallScope::Project(root) => root.join(".cursor").join("hooks.json"),
        }
    }

    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec> {
        // Cursor 的 Hook schema 与 Codex/Claude 不同，因此需要独立的事件清单。
        vec![
            spec(exe, "sessionStart", None, Mode::Thinking, 900),
            spec(exe, "beforeSubmitPrompt", None, Mode::Thinking, 900),
            spec(exe, "preToolUse", Some("Shell"), Mode::Busy, 1800),
            spec(exe, "beforeShellExecution", None, Mode::Busy, 1800),
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
        // 先清理旧条目，避免重复安装后同一事件触发多次。
        let mut config = cursor_uninstall(config, hook_id);
        let root = config
            .as_object_mut()
            .expect("cursor config should be object");
        root.entry("version").or_insert_with(|| json!(1));
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks_map = hooks.as_object_mut().expect("hooks should be object");

        for spec in specs {
            let entry = hooks_map
                .entry(spec.event.clone())
                .or_insert_with(|| json!([]));
            let items = entry.as_array_mut().expect("hook event should be array");
            let mut item = json!({});
            decorate_command_fields(platform, &mut item, &spec.command);
            if let Some(matcher) = &spec.matcher {
                item["matcher"] = json!(matcher);
            }
            items.push(item);
        }

        Ok(config)
    }

    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value> {
        Ok(cursor_uninstall(config, hook_id))
    }
}

fn spec(exe: &Path, event: &str, matcher: Option<&str>, mode: Mode, ttl: u64) -> HookSpec {
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

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ports::ipc::IpcTransport;

    struct TestPlatform;

    impl PlatformAdapter for TestPlatform {
        fn runtime_root(&self) -> PathBuf {
            PathBuf::from(".")
        }

        fn default_ipc_adapter(&self, _ipc_path: &Path) -> Box<dyn IpcTransport> {
            panic!("not used in tests");
        }

        fn quote_hook_command(&self, command: &HookCommand) -> String {
            format!("{} {}", command.exe.display(), command.args.join(" "))
        }

        fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
            object["command"] = json!(self.quote_hook_command(command));
        }

        fn spawn_background_daemon(&self, _exe: &Path) -> AppResult<()> {
            panic!("not used in tests");
        }
    }

    #[test]
    fn cursor_install_generates_expected_shell_hook() {
        let adapter = CursorInstallAdapter;
        let specs = adapter.hook_specs(Path::new("/tmp/esp"));
        let installed = adapter
            .install(json!({}), &specs, "agent-status-light", &TestPlatform)
            .expect("install should succeed");

        let shell_hooks = installed["hooks"]["beforeShellExecution"]
            .as_array()
            .expect("beforeShellExecution hooks should exist");
        assert_eq!(shell_hooks.len(), 1);
        assert_eq!(
            shell_hooks[0]["command"],
            json!(
                "/tmp/esp send --mode busy --source cursor --session auto --ttl 1800 --quiet --hook-id agent-status-light"
            )
        );
    }
}
