//! Windows 平台适配器。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapters::ipc::named_pipe::NamedPipeTransport;
use crate::adapters::platform::{spawn_background, windows_runtime_root, windows_shell_quote};
use crate::model::{AppResult, HookCommand};
use crate::ports::ipc::IpcTransport;
use crate::ports::platform::PlatformAdapter;

#[allow(dead_code)]
/// Windows 平台适配器。
pub struct WindowsAdapter;

impl PlatformAdapter for WindowsAdapter {
    fn runtime_root(&self) -> AppResult<PathBuf> {
        // Windows 运行时目录优先落在 LocalAppData，避免污染用户文档目录。
        windows_runtime_root()
    }

    fn default_ipc_adapter(&self, ipc_path: &Path) -> Box<dyn IpcTransport> {
        // Windows 默认使用 named pipe，而不是伪造 Unix 风格路径。
        Box::new(NamedPipeTransport::new(ipc_path.to_path_buf()))
    }

    fn quote_hook_command(&self, command: &HookCommand) -> String {
        windows_shell_quote(command)
    }

    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
        // hooks.json / settings.json 在 Windows 上可能需要显式覆盖字段，
        // 避免默认 command 使用 POSIX 风格引用后被宿主进程错误解释。
        let command_text = self.quote_hook_command(command);
        object["command"] = json!(command_text);
        object["commandWindows"] = json!(command_text);
        object["command_windows"] = json!(command_text);
    }

    fn spawn_background_daemon(&self, exe: &Path) -> AppResult<()> {
        // 暂时仍复用公共 spawn 策略，保持平台间的后台启动行为一致。
        spawn_background(exe)
    }
}
