//! 平台差异抽象端口。
//!
//! 这里只处理路径、命令引用和后台拉起等平台相关行为，
//! 不负责任何业务状态判断。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{AppResult, HookCommand};
use crate::ports::ipc::IpcTransport;

/// 平台适配层只处理“平台差异”，不负责状态路由。
pub trait PlatformAdapter: Send + Sync {
    /// 返回当前平台默认 runtime 根目录。
    fn runtime_root(&self) -> PathBuf;
    /// 返回当前平台默认 IPC 客户端实现。
    ///
    /// 例如 Unix 下可能是 unix socket，Windows 下则可能是 named pipe。
    fn default_ipc_adapter(&self, ipc_path: &Path) -> Box<dyn IpcTransport>;
    /// 将命令渲染为当前平台可直接执行的 shell 字符串。
    ///
    /// 安装器写配置时通常需要字符串命令，而不是 `exe + args` 结构，
    /// 因此引用规则必须由平台层统一负责。
    fn quote_hook_command(&self, command: &HookCommand) -> String;
    /// 安装器最终写入的 hook JSON 在不同平台可能需要不同字段。
    /// Unix/macOS 只写标准 `command`；
    /// Windows 则按技术方案在需要时补 `commandWindows` / `command_windows`。
    fn decorate_hook_command(&self, object: &mut Value, command: &HookCommand);
    /// 以平台惯例方式启动后台 daemon。
    ///
    /// 例如是否需要脱离终端、重定向输出或使用不同的进程创建标志。
    fn spawn_background_daemon(&self, exe: &Path) -> AppResult<()>;
}
