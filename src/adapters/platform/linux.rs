//! Linux 平台适配器。
//!
//! 第一阶段 Linux 不是主目标平台，但仍尽量复用 Unix 路径，方便本地开发和测试。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapters::ipc::unix_socket::UnixSocketTransport;
use crate::adapters::platform::{shell_quote, spawn_background, unix_runtime_root};
use crate::model::{AppResult, HookCommand};
use crate::ports::ipc::IpcTransport;
use crate::ports::platform::PlatformAdapter;

#[allow(dead_code)]
/// Linux 平台适配器。
pub struct LinuxAdapter;

impl PlatformAdapter for LinuxAdapter {
    fn runtime_root(&self) -> AppResult<PathBuf> {
        // Linux 目前直接复用 Unix 目录布局，保持与 macOS 的使用习惯一致。
        unix_runtime_root()
    }

    fn default_ipc_adapter(&self, ipc_path: &Path) -> Box<dyn IpcTransport> {
        // Linux 默认优先使用 Unix Domain Socket，简单且无需额外端口管理。
        Box::new(UnixSocketTransport::new(ipc_path.to_path_buf()))
    }

    fn quote_hook_command(&self, command: &HookCommand) -> String {
        shell_quote(command)
    }

    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand) {
        // Linux/macOS 宿主普遍只认标准 `command` 字段，因此这里只写这一份。
        object["command"] = json!(self.quote_hook_command(command));
    }

    fn spawn_background_daemon(&self, exe: &Path) -> AppResult<()> {
        // 后台拉起策略复用平台公共实现，避免每个 Unix 平台各写一套子进程细节。
        spawn_background(exe)
    }
}
