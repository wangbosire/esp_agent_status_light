//! `adapters::install::cursor` 模块测试。

use serde_json::json;

use super::*;
use crate::ports::ipc::IpcTransport;

struct TestPlatform;

impl PlatformAdapter for TestPlatform {
    fn runtime_root(&self) -> crate::model::AppResult<PathBuf> {
        Ok(PathBuf::from("."))
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

    let session_start_hooks = installed["hooks"]["sessionStart"]
        .as_array()
        .expect("sessionStart hooks should exist");
    assert_eq!(
        session_start_hooks[0]["command"],
        json!(
            "/tmp/esp send --mode green --source cursor --session auto --ttl 900 --quiet --hook-id agent-status-light"
        )
    );

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

    let response_hooks = installed["hooks"]["afterAgentResponse"]
        .as_array()
        .expect("afterAgentResponse hooks should exist");
    assert_eq!(response_hooks.len(), 1);
    assert_eq!(
        response_hooks[0]["command"],
        json!(
            "/tmp/esp send --mode ai --source cursor --session auto --ttl 900 --quiet --hook-id agent-status-light"
        )
    );

    let read_hooks = installed["hooks"]["beforeReadFile"]
        .as_array()
        .expect("beforeReadFile hooks should exist");
    assert_eq!(read_hooks.len(), 1);
    assert_eq!(
        read_hooks[0]["command"],
        json!(
            "/tmp/esp send --mode ai --source cursor --session auto --ttl 900 --quiet --hook-id agent-status-light"
        )
    );

    let tab_read_hooks = installed["hooks"]["beforeTabFileRead"]
        .as_array()
        .expect("beforeTabFileRead hooks should exist");
    assert_eq!(tab_read_hooks.len(), 1);
    assert_eq!(
        tab_read_hooks[0]["command"],
        json!(
            "/tmp/esp send --mode ai --source cursor --session auto --ttl 900 --quiet --hook-id agent-status-light"
        )
    );

    let edit_hooks = installed["hooks"]["preToolUse"]
        .as_array()
        .expect("preToolUse hooks should exist");
    assert!(
        edit_hooks.iter().any(|item| {
            item["matcher"] == json!("Write")
                && item["command"]
                    == json!(
                        "/tmp/esp send --mode ai --source cursor --session auto --ttl 900 --quiet --hook-id agent-status-light"
                    )
        }),
        "preToolUse should contain Write -> ai hook"
    );
}
