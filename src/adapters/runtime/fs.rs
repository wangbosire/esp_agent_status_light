//! 文件系统版 runtime 存储实现。

use std::fs;
use std::path::PathBuf;

use crate::model::{AppError, AppResult, InstallManifest, IpcInfo};
use crate::ports::runtime::RuntimeStore;

#[derive(Debug, Clone)]
pub struct FsRuntimeAdapter {
    /// runtime 根目录，例如 `~/.esp-agent-status-light` 或 `%LOCALAPPDATA%/...`。
    root: PathBuf,
}

impl FsRuntimeAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn pid_path(&self) -> PathBuf {
        // pid 文件只服务于“已有 daemon 健康检查”和 `stop --force`。
        self.runtime_dir().join("daemon.pid")
    }

    fn ipc_info_path(&self) -> PathBuf {
        // 保存当前 IPC 类型和地址，便于 status 输出和命令侧诊断。
        self.runtime_dir().join("ipc.json")
    }
}

impl RuntimeStore for FsRuntimeAdapter {
    fn runtime_root(&self) -> PathBuf {
        self.root.clone()
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn events_log_path(&self) -> PathBuf {
        self.runtime_dir().join("events.log")
    }

    fn runtime_log_path(&self) -> PathBuf {
        self.runtime_dir().join("runtime.log")
    }

    fn install_manifest_path(&self, target: &str) -> PathBuf {
        self.root.join(format!("config.{target}.json"))
    }

    fn default_ipc_path(&self) -> PathBuf {
        self.runtime_dir().join("daemon.sock")
    }

    fn ensure_layout(&self) -> AppResult<()> {
        // bin/runtime 目录统一由这里创建，调用方不再分别处理。
        fs::create_dir_all(self.bin_dir()).map_err(|err| AppError::io("create bin dir", err))?;
        fs::create_dir_all(self.runtime_dir())
            .map_err(|err| AppError::io("create runtime dir", err))?;
        Ok(())
    }

    fn read_pid(&self) -> AppResult<Option<u32>> {
        let path = self.pid_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read pid file", err))?;
        raw.trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|err| AppError::invalid("parse pid file", err))
    }

    fn write_pid(&self, pid: u32) -> AppResult<()> {
        fs::write(self.pid_path(), pid.to_string())
            .map_err(|err| AppError::io("write pid file", err))
    }

    fn clear_pid(&self) -> AppResult<()> {
        let path = self.pid_path();
        if path.exists() {
            fs::remove_file(path).map_err(|err| AppError::io("remove pid file", err))?;
        }
        Ok(())
    }

    fn read_ipc_info(&self) -> AppResult<Option<IpcInfo>> {
        let path = self.ipc_info_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read ipc info", err))?;
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|err| AppError::invalid("parse ipc info", err))
    }

    fn write_ipc_info(&self, info: &IpcInfo) -> AppResult<()> {
        let raw = serde_json::to_string_pretty(info)
            .map_err(|err| AppError::invalid("serialize ipc info", err))?;
        fs::write(self.ipc_info_path(), raw).map_err(|err| AppError::io("write ipc info", err))
    }

    fn clear_ipc_info(&self) -> AppResult<()> {
        let path = self.ipc_info_path();
        if path.exists() {
            fs::remove_file(path).map_err(|err| AppError::io("remove ipc info", err))?;
        }
        Ok(())
    }

    fn write_install_manifest(&self, manifest: &InstallManifest) -> AppResult<()> {
        // install manifest 只记录“这次安装把什么写到了哪里”，不作为运行时真相来源。
        let raw = serde_json::to_string_pretty(manifest)
            .map_err(|err| AppError::invalid("serialize install manifest", err))?;
        fs::write(self.install_manifest_path(&manifest.target), raw)
            .map_err(|err| AppError::io("write install manifest", err))
    }
}
