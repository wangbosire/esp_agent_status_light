//! `adapters::install::claude` 模块测试。

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
fn claude_install_generates_post_tool_use_hooks_to_clear_alarm() {
    let adapter = ClaudeInstallAdapter;
    let specs = adapter.hook_specs(Path::new("/tmp/esp"));
    let installed = adapter
        .install(json!({}), &specs, "agent-status-light", &TestPlatform)
        .expect("install should succeed");

    let session_start_hooks = installed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart hooks should exist");
    assert_eq!(
        session_start_hooks[0]["hooks"][0]["command"],
        json!(
            "/tmp/esp send --mode green --source claude --session auto --ttl 900 --quiet --hook-id agent-status-light"
        )
    );

    let hooks = installed["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse hooks should exist");
    assert_eq!(hooks.len(), 5);
    assert!(
        hooks.iter().any(|group| {
            group["matcher"] == json!("Read")
                && group["hooks"][0]["command"]
                    == json!(
                        "/tmp/esp send --mode ai --source claude --session auto --ttl 900 --quiet --hook-id agent-status-light"
                    )
        }),
        "PostToolUse should contain Read -> ai hook"
    );

    let pre_hooks = installed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse hooks should exist");
    assert!(
        pre_hooks.iter().any(|group| {
            group["matcher"] == json!("Read")
                && group["hooks"][0]["command"]
                    == json!(
                        "/tmp/esp send --mode ai --source claude --session auto --ttl 900 --quiet --hook-id agent-status-light"
                    )
        }),
        "PreToolUse should contain Read -> ai hook"
    );

    let batch_hooks = installed["hooks"]["PostToolBatch"]
        .as_array()
        .expect("PostToolBatch hooks should exist");
    assert_eq!(batch_hooks.len(), 1);
}
