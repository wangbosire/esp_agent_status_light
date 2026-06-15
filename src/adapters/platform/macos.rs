//! macOS 平台适配器。
//!
//! 当前 macOS 是主要目标平台之一，因此默认走：
//! 1. `~/.esp-agent-status-light` runtime 根目录；
//! 2. Unix socket IPC；
//! 3. POSIX shell 风格 hook 命令拼装。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapters::ipc::unix_socket::UnixSocketTransport;
use crate::adapters::platform::{shell_quote, spawn_background, unix_runtime_root};
use crate::model::{AppResult, HookCommand};
use crate::ports::ipc::IpcTransport;
use crate::ports::platform::PlatformAdapter;

/// macOS 平台适配器。
pub struct MacosAdapter;

impl PlatformAdapter for MacosAdapter {
    fn runtime_root(&self) -> AppResult<PathBuf> {
        // 当前 macOS 与 Linux 共享同一类用户级 runtime 目录策略。
        unix_runtime_root()
    }

    fn default_ipc_adapter(&self, ipc_path: &Path) -> Box<dyn IpcTransport> {
        // macOS 同样优先走 Unix socket，避免引入不必要的传输复杂度。
        Box::new(UnixSocketTransport::new(ipc_path.to_path_buf()))
    }

    fn quote_hook_command(&self, command: &HookCommand) -> String {
        shell_quote(command)
    }

    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
        // macOS 宿主目前也统一消费 POSIX 风格 `command` 字段。
        object["command"] = json!(self.quote_hook_command(command));
    }

    fn spawn_background_daemon(&self, exe: &Path) -> AppResult<()> {
        // 第一阶段不接入 launchd，而是保持和其它平台一致的轻量后台进程模型。
        spawn_background(exe)
    }
}
