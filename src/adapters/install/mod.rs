pub mod claude;
pub mod codex;
pub mod cursor;

// Hook 安装器公共逻辑。
//
// 这里负责：
// 1. 统一注册不同宿主工具安装器；
// 2. 判断一条 Hook 是否由本工具写入；
// 3. 复用卸载逻辑，确保只删除托管条目，不误伤用户自定义配置。

use serde_json::{Map, Value, json};

use crate::model::HookCommand;
use crate::ports::hook_install::HookInstallRegistry;
use crate::ports::platform::PlatformAdapter;

pub fn registry() -> HookInstallRegistry {
    HookInstallRegistry::new()
        .with(codex::CodexInstallAdapter)
        .with(cursor::CursorInstallAdapter)
        .with(claude::ClaudeInstallAdapter)
}

fn is_managed_command(command: &str, hook_id: &str) -> bool {
    // 双重判断是为了兼容历史安装结果：
    // 新版本优先用 `--hook-id` 精确识别，旧版本则退回命令特征匹配。
    command.contains(&format!("--hook-id {hook_id}"))
        || (command.contains("esp send --mode") && command.contains("agent-status-light"))
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    // 某些宿主配置文件可能不存在或被用户写成非对象，
    // 这里统一强制转成空对象，后续逻辑才能稳定写入。
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("value should be object")
}

fn decorate_command_fields(
    platform: &dyn PlatformAdapter,
    object: &mut Value,
    command: &HookCommand,
) {
    // 具体写哪些字段交给平台层决定，安装器本身不关心 Windows / POSIX 差异。
    platform.decorate_hook_command(object, command);
}

fn codex_like_uninstall(mut config: Value, hook_id: &str) -> Value {
    let root = ensure_object(&mut config);
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks);
    // Codex / Claude 的结构都是 “事件 -> matcher group -> hooks[]” 三层，
    // 卸载时只删掉带 hook_id 的命令，其它用户自定义 Hook 必须完整保留。
    for entries in hooks_map.values_mut() {
        let Some(items) = entries.as_array_mut() else {
            continue;
        };
        for entry in items.iter_mut() {
            let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| is_managed_command(command, hook_id))
            });
        }
        items.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
    }
    config
}

fn cursor_uninstall(mut config: Value, hook_id: &str) -> Value {
    let root = ensure_object(&mut config);
    root.entry("version").or_insert_with(|| json!(1));
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks);
    // Cursor 的结构更扁平，是 “事件 -> command[]”；
    // 因此这里按 command 字段直接筛掉本工具写入的条目。
    for entries in hooks_map.values_mut() {
        let Some(items) = entries.as_array_mut() else {
            continue;
        };
        items.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| is_managed_command(command, hook_id))
        });
    }
    config
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ports::ipc::IpcTransport;
    use crate::ports::platform::PlatformAdapter;
    use std::path::{Path, PathBuf};

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
}
