//! `adapters::install` 公共逻辑测试。

use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::ports::ipc::IpcTransport;
use crate::ports::platform::PlatformAdapter;

#[test]
fn cursor_uninstall_removes_managed_entries_only() {
    let config = json!({
        "version": 1,
        "hooks": {
            "beforeShellExecution": [
                {"command": "/tmp/esp send --mode busy --hook-id agent-status-light"},
                {"command": "echo keep-me"}
            ]
        }
    });
    let updated = cursor_uninstall(config, "agent-status-light");
    let items = updated["hooks"]["beforeShellExecution"]
        .as_array()
        .expect("hooks array should exist");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["command"], "echo keep-me");
}

#[test]
fn codex_like_uninstall_keeps_user_hooks_in_same_group() {
    let config = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "/tmp/esp send --mode busy --hook-id agent-status-light"},
                        {"type": "command", "command": "echo keep-me"}
                    ]
                }
            ]
        }
    });
    let updated = codex_like_uninstall(config, "agent-status-light");
    let hooks = updated["hooks"]["PreToolUse"][0]["hooks"]
        .as_array()
        .expect("hooks array should exist");
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["command"], "echo keep-me");
}

struct TestPlatform {
    windows: bool,
}

impl PlatformAdapter for TestPlatform {
    fn runtime_root(&self) -> PathBuf {
        PathBuf::from(".")
    }

    fn default_ipc_adapter(&self, _ipc_path: &Path) -> Box<dyn IpcTransport> {
        panic!("not used in tests");
    }

    fn quote_hook_command(&self, command: &HookCommand) -> String {
        if self.windows {
            format!("WIN:{}", command.exe.display())
        } else {
            format!("POSIX:{}", command.exe.display())
        }
    }

    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
        let rendered = self.quote_hook_command(command);
        object["command"] = json!(rendered);
        if self.windows {
            object["commandWindows"] = json!(rendered);
            object["command_windows"] = json!(rendered);
        }
    }

    fn spawn_background_daemon(&self, _exe: &Path) -> crate::model::AppResult<()> {
        panic!("not used in tests");
    }
}

#[test]
fn decorate_command_fields_adds_windows_overrides_only_when_needed() {
    let mut value = json!({});
    let command = HookCommand {
        exe: PathBuf::from("/tmp/esp"),
        args: Vec::new(),
    };

    decorate_command_fields(&TestPlatform { windows: false }, &mut value, &command);
    assert_eq!(value["command"], "POSIX:/tmp/esp");
    assert!(value.get("commandWindows").is_none());
    assert!(value.get("command_windows").is_none());

    let mut windows_value = json!({});
    decorate_command_fields(
        &TestPlatform { windows: true },
        &mut windows_value,
        &command,
    );
    assert_eq!(windows_value["command"], "WIN:/tmp/esp");
    assert_eq!(windows_value["commandWindows"], "WIN:/tmp/esp");
    assert_eq!(windows_value["command_windows"], "WIN:/tmp/esp");
}
