//! 运行时文件存储端口。
//!
//! 该抽象把 pid、ipc 元信息、日志与安装清单的路径规则集中起来，
//! 让上层逻辑不需要自己拼路径或关心平台目录差异。

use std::path::PathBuf;

use crate::model::{AppResult, BleDeviceConfig, InstallManifest, InstallManifestIndex, IpcInfo};

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
    /// JSONL 运行日志文件路径。
    ///
    /// 与 events log 不同，这份日志用于保留 daemon 的最近运行轨迹，
    /// 便于排查“当前这轮运行里发生了什么”，实现层会负责数量裁剪。
    fn runtime_log_path(&self) -> PathBuf;
    /// 某个安装目标对应的 manifest 路径。
    fn install_manifest_path(&self, target: &str) -> PathBuf;
    /// 平台默认 IPC 地址或路径。
    fn default_ipc_path(&self) -> PathBuf;
    /// BLE 设备配置文件路径。
    fn ble_config_path(&self) -> PathBuf;
    /// 确保 runtime 所需目录存在。
    fn ensure_layout(&self) -> AppResult<()>;
    /// 读取 daemon pid 文件。
    ///
    /// 返回 `None` 表示当前没有已知 pid 标记，并不等价于“进程一定不存在”。
    fn read_pid(&self) -> AppResult<Option<u32>>;
    /// 写入当前 daemon pid。
    fn write_pid(&self, pid: u32) -> AppResult<()>;
    /// 清理 daemon pid 文件。
    fn clear_pid(&self) -> AppResult<()>;
    /// 读取当前 daemon 暴露的 IPC 元信息。
    fn read_ipc_info(&self) -> AppResult<Option<IpcInfo>>;
    /// 写入当前 daemon 暴露的 IPC 元信息。
    fn write_ipc_info(&self, info: &IpcInfo) -> AppResult<()>;
    /// 清理 IPC 元信息文件。
    fn clear_ipc_info(&self) -> AppResult<()>;
    /// 记录一次安装动作的最终落盘结果。
    ///
    /// 这不是 Hook 真相来源，而是一个便于用户排查的“安装摘要”。
    fn write_install_manifest(&self, manifest: &InstallManifest) -> AppResult<()>;
    /// 从安装摘要中移除指定配置路径。
    fn remove_install_manifest(&self, target: &str, config_path: &str) -> AppResult<()>;
    /// 读取指定 target 的安装摘要。
    fn read_install_manifest(&self, target: &str) -> AppResult<Option<InstallManifestIndex>>;
    /// 读取 BLE 设备配置；未配置时返回默认配置。
    fn read_ble_config(&self) -> AppResult<BleDeviceConfig>;
    /// 写入 BLE 设备配置。
    fn write_ble_config(&self, config: &BleDeviceConfig) -> AppResult<()>;
}
