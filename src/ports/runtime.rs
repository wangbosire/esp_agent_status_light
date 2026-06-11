use std::path::PathBuf;

use crate::model::{AppResult, InstallManifest, IpcInfo};

/// RuntimeStore 把 pid/socket/log/manifest 等运行态文件集中管理，
/// 这样 daemon 和 command 层都不需要散落拼路径。
pub trait RuntimeStore: Send + Sync {
    /// runtime 根目录，例如 `~/.esp-agent-status-light`。
    fn runtime_root(&self) -> PathBuf;
    /// 运行时目录，通常保存 pid / socket / ipc 信息。
    fn runtime_dir(&self) -> PathBuf;
    /// 稳定二进制目录，安装后的 Hook 会指向这里。
    fn bin_dir(&self) -> PathBuf;
    /// JSONL 事件日志文件路径。
    fn events_log_path(&self) -> PathBuf;
    /// 某个安装目标对应的 manifest 路径。
    fn install_manifest_path(&self, target: &str) -> PathBuf;
    /// 平台默认 IPC 地址或路径。
    fn default_ipc_path(&self) -> PathBuf;
    /// 确保 runtime 所需目录存在。
    fn ensure_layout(&self) -> AppResult<()>;
    fn read_pid(&self) -> AppResult<Option<u32>>;
    fn write_pid(&self, pid: u32) -> AppResult<()>;
    fn clear_pid(&self) -> AppResult<()>;
    fn read_ipc_info(&self) -> AppResult<Option<IpcInfo>>;
    fn write_ipc_info(&self, info: &IpcInfo) -> AppResult<()>;
    fn clear_ipc_info(&self) -> AppResult<()>;
    fn write_install_manifest(&self, manifest: &InstallManifest) -> AppResult<()>;
}
