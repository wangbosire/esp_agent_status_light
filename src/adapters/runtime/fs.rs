//! 文件系统版 runtime 存储实现。
//!
//! 当前项目的 pid、ipc 信息、日志和安装清单都直接落在本地文件系统，
//! 这是最简单、最稳妥、也最便于用户手动排查的一种实现。

use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{
    AppError, AppResult, BleDeviceConfig, InstallManifest, InstallManifestIndex, IpcInfo,
};
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

    /// 以“写临时文件再替换”的方式原子更新小型 runtime 文件。
    ///
    /// 这些文件通常会被 CLI 和 daemon 并发读写，因此尽量避免留下半写状态。
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

    fn ble_config_path(&self) -> PathBuf {
        self.root.join("ble.json")
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
        // install manifest 记录“这个 target 当前已知装到了哪些配置文件”，不作为运行时真相来源。
        let mut index = self
            .read_install_manifest(&manifest.target)?
            .unwrap_or_else(|| InstallManifestIndex {
                target: manifest.target.clone(),
                installations: Vec::new(),
            });
        if let Some(existing) = index
            .installations
            .iter_mut()
            .find(|item| item.config_path == manifest.config_path)
        {
            *existing = manifest.clone();
        } else {
            index.installations.push(manifest.clone());
        }
        let raw = serde_json::to_string_pretty(&index)
            .map_err(|err| AppError::invalid("serialize install manifest", err))?;
        self.write_atomic(
            self.install_manifest_path(&manifest.target),
            raw,
            "install manifest",
        )
    }

    fn remove_install_manifest(&self, target: &str, config_path: &str) -> AppResult<()> {
        let Some(mut index) = self.read_install_manifest(target)? else {
            return Ok(());
        };
        index
            .installations
            .retain(|item| item.config_path != config_path);

        let path = self.install_manifest_path(target);
        if index.installations.is_empty() {
            if path.exists() {
                fs::remove_file(path)
                    .map_err(|err| AppError::io("remove install manifest", err))?;
            }
            return Ok(());
        }

        let raw = serde_json::to_string_pretty(&index)
            .map_err(|err| AppError::invalid("serialize install manifest", err))?;
        self.write_atomic(path, raw, "install manifest")
    }

    fn read_install_manifest(&self, target: &str) -> AppResult<Option<InstallManifestIndex>> {
        let path = self.install_manifest_path(target);
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(path).map_err(|err| AppError::io("read install manifest", err))?;
        parse_install_manifest_index(target, &raw).map(Some)
    }

    fn read_ble_config(&self) -> AppResult<BleDeviceConfig> {
        let path = self.ble_config_path();
        if !path.exists() {
            return Ok(BleDeviceConfig::default());
        }
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read ble config", err))?;
        serde_json::from_str(&raw).map_err(|err| AppError::invalid("parse ble config", err))
    }

    fn write_ble_config(&self, config: &BleDeviceConfig) -> AppResult<()> {
        self.ensure_layout()?;
        let raw = serde_json::to_string_pretty(config)
            .map_err(|err| AppError::invalid("serialize ble config", err))?;
        self.write_atomic(self.ble_config_path(), raw, "ble config")
    }
}

fn parse_install_manifest_index(target: &str, raw: &str) -> AppResult<InstallManifestIndex> {
    if let Ok(index) = serde_json::from_str::<InstallManifestIndex>(raw) {
        return Ok(index);
    }
    let legacy = serde_json::from_str::<InstallManifest>(raw)
        .map_err(|err| AppError::invalid("parse install manifest", err))?;
    Ok(InstallManifestIndex {
        target: target.into(),
        installations: vec![legacy],
    })
}

/// 在不同平台上尽量稳定地用 `from` 覆盖 `to`。
///
/// 某些平台 `rename` 到已存在目标时会直接失败，因此这里在必要时手动删除旧文件重试。
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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn temp_runtime_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("esp-fs-runtime-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn manifest(target: &str, config_path: &str, command_path: &str) -> InstallManifest {
        InstallManifest {
            target: target.into(),
            installed_at: Utc::now(),
            config_path: config_path.into(),
            command_path: command_path.into(),
        }
    }

    #[test]
    fn install_manifest_keeps_multiple_config_paths_per_target() {
        let root = temp_runtime_root("multi-manifest");
        let runtime = FsRuntimeAdapter::new(root.clone());
        runtime.ensure_layout().expect("layout should succeed");

        runtime
            .write_install_manifest(&manifest(
                "cursor",
                "/repo/a/.cursor/hooks.json",
                "/bin/esp",
            ))
            .expect("first manifest write should succeed");
        runtime
            .write_install_manifest(&manifest(
                "cursor",
                "/repo/b/.cursor/hooks.json",
                "/bin/esp",
            ))
            .expect("second manifest write should succeed");

        let index = runtime
            .read_install_manifest("cursor")
            .expect("read manifest should succeed")
            .expect("manifest should exist");
        assert_eq!(index.target, "cursor");
        assert_eq!(index.installations.len(), 2);
        assert!(
            index
                .installations
                .iter()
                .any(|item| item.config_path == "/repo/a/.cursor/hooks.json")
        );
        assert!(
            index
                .installations
                .iter()
                .any(|item| item.config_path == "/repo/b/.cursor/hooks.json")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_manifest_updates_existing_config_path() {
        let root = temp_runtime_root("upsert-manifest");
        let runtime = FsRuntimeAdapter::new(root.clone());
        runtime.ensure_layout().expect("layout should succeed");

        runtime
            .write_install_manifest(&manifest("codex", "/repo/.codex/hooks.json", "/old/esp"))
            .expect("first manifest write should succeed");
        runtime
            .write_install_manifest(&manifest("codex", "/repo/.codex/hooks.json", "/new/esp"))
            .expect("second manifest write should succeed");

        let index = runtime
            .read_install_manifest("codex")
            .expect("read manifest should succeed")
            .expect("manifest should exist");
        assert_eq!(index.installations.len(), 1);
        assert_eq!(index.installations[0].command_path, "/new/esp");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_install_manifest_deletes_only_matching_config_path() {
        let root = temp_runtime_root("remove-manifest");
        let runtime = FsRuntimeAdapter::new(root.clone());
        runtime.ensure_layout().expect("layout should succeed");

        runtime
            .write_install_manifest(&manifest(
                "claude",
                "/repo/a/.claude/settings.json",
                "/bin/esp",
            ))
            .expect("first manifest write should succeed");
        runtime
            .write_install_manifest(&manifest(
                "claude",
                "/repo/b/.claude/settings.json",
                "/bin/esp",
            ))
            .expect("second manifest write should succeed");

        runtime
            .remove_install_manifest("claude", "/repo/a/.claude/settings.json")
            .expect("remove manifest should succeed");
        let index = runtime
            .read_install_manifest("claude")
            .expect("read manifest should succeed")
            .expect("manifest should still exist");
        assert_eq!(index.installations.len(), 1);
        assert_eq!(
            index.installations[0].config_path,
            "/repo/b/.claude/settings.json"
        );

        runtime
            .remove_install_manifest("claude", "/repo/b/.claude/settings.json")
            .expect("remove final manifest should succeed");
        assert!(
            runtime
                .read_install_manifest("claude")
                .expect("read manifest should succeed")
                .is_none()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_install_manifest_accepts_legacy_single_record_shape() {
        let root = temp_runtime_root("legacy-manifest");
        let runtime = FsRuntimeAdapter::new(root.clone());
        runtime.ensure_layout().expect("layout should succeed");
        let legacy = manifest("cursor", "/legacy/.cursor/hooks.json", "/bin/esp");
        let raw = serde_json::to_string_pretty(&legacy).expect("serialize legacy manifest");
        fs::write(runtime.install_manifest_path("cursor"), raw).expect("write legacy manifest");

        let index = runtime
            .read_install_manifest("cursor")
            .expect("read manifest should succeed")
            .expect("manifest should exist");
        assert_eq!(index.target, "cursor");
        assert_eq!(index.installations.len(), 1);
        assert_eq!(
            index.installations[0].config_path,
            "/legacy/.cursor/hooks.json"
        );

        let _ = fs::remove_dir_all(root);
    }
}
