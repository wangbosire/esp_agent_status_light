//! 文件系统版 runtime 存储实现。
//!
//! 当前项目的 pid、ipc 信息、日志和安装清单都直接落在本地文件系统，
//! 这是最简单、最稳妥、也最便于用户手动排查的一种实现。

use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{AppError, AppResult, InstallManifest, IpcInfo};
use crate::ports::runtime::RuntimeStore;

#[derive(Debug, Clone)]
pub struct FsRuntimeAdapter {
    /// runtime 根目录，例如 `~/.esp-agent-status-light` 或 `%LOCALAPPDATA%/...`。
    root: PathBuf,
}

impl FsRuntimeAdapter {
    /// 使用给定根目录创建一个文件系统 runtime 存储。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 返回 pid 文件路径。
    fn pid_path(&self) -> PathBuf {
        // pid 文件只服务于“已有 daemon 健康检查”和 `stop --force`。
        self.runtime_dir().join("daemon.pid")
    }

    /// 返回 IPC 元信息文件路径。
    fn ipc_info_path(&self) -> PathBuf {
        // 保存当前 IPC 类型和地址，便于 status 输出和命令侧诊断。
        self.runtime_dir().join("ipc.json")
    }

    fn write_atomic(&self, path: PathBuf, raw: String, context: &str) -> AppResult<()> {
        // 先写临时文件，再替换目标文件，避免异常中断时留下半写内容。
        let tmp_path = path.with_extension(format!(
            "{}.tmp.{}",
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("tmp"),
            std::process::id()
        ));
        fs::write(&tmp_path, raw)
            .map_err(|err| AppError::io(&format!("write {context} temp"), err))?;
        replace_file(&tmp_path, &path, context)
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
        // pid 文件只保存一个数字，不需要复杂格式。
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read pid file", err))?;
        raw.trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|err| AppError::invalid("parse pid file", err))
    }

    fn write_pid(&self, pid: u32) -> AppResult<()> {
        self.write_atomic(self.pid_path(), pid.to_string(), "pid file")
    }

    fn clear_pid(&self) -> AppResult<()> {
        let path = self.pid_path();
        if path.exists() {
            // pid 文件只是一个状态提示，清理失败不应影响主流程继续收尾。
            fs::remove_file(path).map_err(|err| AppError::io("remove pid file", err))?;
        }
        Ok(())
    }

    fn read_ipc_info(&self) -> AppResult<Option<IpcInfo>> {
        let path = self.ipc_info_path();
        if !path.exists() {
            return Ok(None);
        }
        // IPC 元信息采用 pretty JSON 写入，但读取时仍按普通 JSON 解析。
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read ipc info", err))?;
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|err| AppError::invalid("parse ipc info", err))
    }

    fn write_ipc_info(&self, info: &IpcInfo) -> AppResult<()> {
        let raw = serde_json::to_string_pretty(info)
            .map_err(|err| AppError::invalid("serialize ipc info", err))?;
        self.write_atomic(self.ipc_info_path(), raw, "ipc info")
    }

    fn clear_ipc_info(&self) -> AppResult<()> {
        let path = self.ipc_info_path();
        if path.exists() {
            // 与 pid 文件一致，ipc 元信息也是“尽力清理”语义。
            fs::remove_file(path).map_err(|err| AppError::io("remove ipc info", err))?;
        }
        Ok(())
    }

    fn write_install_manifest(&self, manifest: &InstallManifest) -> AppResult<()> {
        // install manifest 只记录“这次安装把什么写到了哪里”，不作为运行时真相来源。
        let raw = serde_json::to_string_pretty(manifest)
            .map_err(|err| AppError::invalid("serialize install manifest", err))?;
        self.write_atomic(
            self.install_manifest_path(&manifest.target),
            raw,
            "install manifest",
        )
    }
}

fn replace_file(from: &Path, to: &Path, context: &str) -> AppResult<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if to.exists() {
                fs::remove_file(to).map_err(|err| AppError::io(context, err))?;
                // 再试一次 rename，覆盖大多数平台上“目标已存在”导致的失败。
                fs::rename(from, to).map_err(|err| AppError::io(context, err))
            } else {
                Err(AppError::io(context, rename_err))
            }
        }
    }
}
