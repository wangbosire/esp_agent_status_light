//! macOS 平台适配器。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapters::ipc::unix_socket::UnixSocketTransport;
use crate::adapters::platform::{shell_quote, spawn_background, unix_runtime_root};
use crate::model::{AppResult, HookCommand};
use crate::ports::ipc::IpcTransport;
use crate::ports::platform::PlatformAdapter;

pub struct MacosAdapter;

impl PlatformAdapter for MacosAdapter {
    fn runtime_root(&self) -> PathBuf {
        unix_runtime_root()
    }

    fn default_ipc_adapter(&self, ipc_path: &Path) -> Box<dyn IpcTransport> {
        Box::new(UnixSocketTransport::new(ipc_path.to_path_buf()))
    }

    fn quote_hook_command(&self, command: &HookCommand) -> String {
        shell_quote(command)
    }

    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
        object["command"] = json!(self.quote_hook_command(command));
    }

    fn spawn_background_daemon(&self, exe: &Path) -> AppResult<()> {
        spawn_background(exe)
    }
}
