use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Value, json};

use crate::model::{AppError, AppResult, HookCommand, HookSpec};
use crate::ports::runtime::RuntimeStore;
use crate::runtime_lock::FileLock;

/// install 命令最终写入宿主配置时采用的命令来源。
///
/// 这个枚举的存在，是为了把“开发态走 cargo run”与“发布态走稳定二进制副本”
/// 这两个策略在类型层面明确区分开。
pub(super) enum InstallCommandTarget {
    StableBinary { path: PathBuf },
    CargoRun { manifest_path: PathBuf },
}

impl InstallCommandTarget {
    pub(super) fn spec_exe(&self) -> &Path {
        match self {
            Self::StableBinary { path } => path.as_path(),
            // 先让安装器按 `cargo` 生成命令骨架，后面再统一补 `run --manifest-path --` 前缀。
            Self::CargoRun { .. } => Path::new("cargo"),
        }
    }

    pub(super) fn apply_to_specs(&self, specs: &mut [HookSpec]) {
        if let Self::CargoRun { manifest_path } = self {
            // 所有 HookSpec 最初都只关心 `send ...` 这段业务参数。
            // 这里统一把它们重写成 `cargo run -- ...`，避免安装器各自处理开发态分支。
            for spec in specs {
                spec.command = build_cargo_run_hook_command(manifest_path, &spec.command.args);
            }
        }
    }

    pub(super) fn display_command(&self) -> String {
        match self {
            Self::StableBinary { path } => path.to_string_lossy().to_string(),
            Self::CargoRun { manifest_path } => format!(
                "cargo run --manifest-path {} --",
                manifest_path.to_string_lossy()
            ),
        }
    }
}

pub(super) fn resolve_install_command(
    runtime: &dyn RuntimeStore,
) -> AppResult<InstallCommandTarget> {
    let current =
        std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;

    // 开发态从 `target/debug` 运行时，优先回写 `cargo run`，让已安装 Hook 跟随当前源码。
    if should_use_cargo_run_hooks(&current) {
        return Ok(InstallCommandTarget::CargoRun {
            manifest_path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        });
    }

    Ok(InstallCommandTarget::StableBinary {
        path: install_stable_binary(runtime)?,
    })
}

pub(super) fn should_use_cargo_run_hooks(current_exe: &Path) -> bool {
    // release 构建和正式分发一律不走这条分支，避免线上 Hook 依赖 cargo toolchain。
    if !cfg!(debug_assertions) {
        return false;
    }

    let Some(parent) = current_exe.parent() else {
        return false;
    };
    let Some(grand_parent) = parent.parent() else {
        return false;
    };

    parent.file_name().is_some_and(|name| name == "debug")
        && grand_parent
            .file_name()
            .is_some_and(|name| name == "target")
}

pub(super) fn build_cargo_run_hook_command(
    manifest_path: &Path,
    send_args: &[String],
) -> HookCommand {
    // 这里保留 send 原始参数顺序，确保开发态和稳定二进制态行为尽量一致。
    let mut args = vec![
        "run".into(),
        "--manifest-path".into(),
        manifest_path.to_string_lossy().to_string(),
        "--".into(),
    ];
    args.extend(send_args.iter().cloned());
    HookCommand {
        exe: PathBuf::from("cargo"),
        args,
    }
}

fn install_stable_binary(runtime: &dyn RuntimeStore) -> AppResult<PathBuf> {
    runtime.ensure_layout()?;
    let current =
        std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;
    let file_name = if cfg!(windows) { "esp.exe" } else { "esp" };
    let target = runtime.bin_dir().join(file_name);
    let lock_path = runtime.bin_dir().join("stable-binary.lock");
    // 多次 install 并发执行时，只允许一个进程更新 runtime/bin 下的稳定副本。
    let _guard = FileLock::acquire(lock_path)?;

    let tmp_name = format!(
        "{}.tmp.{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("esp"),
        std::process::id()
    );
    let tmp_path = runtime.bin_dir().join(tmp_name);
    // 先写临时文件，再 replace，避免目标文件在 copy 过程中被其它进程观察到半成品。
    fs::copy(&current, &tmp_path).map_err(|err| AppError::io("copy stable binary", err))?;
    replace_file(&tmp_path, &target, "replace stable binary")?;
    Ok(target)
}

pub(super) fn read_json_or_empty(path: &Path) -> AppResult<Value> {
    // install/uninstall 都允许“配置文件第一次创建”的路径，因此不存在时返回空对象。
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = fs::read_to_string(path).map_err(|err| AppError::io("read config json", err))?;
    serde_json::from_str(&raw).map_err(|err| AppError::invalid("parse config json", err))
}

pub(super) fn write_json(path: &Path, value: &Value) -> AppResult<()> {
    // 配置写入统一走 pretty JSON，方便用户自己排查和版本管理 diff。
    let raw = serde_json::to_string_pretty(value)
        .map_err(|err| AppError::invalid("serialize json file", err))?;
    let tmp_path = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json"),
        std::process::id()
    ));
    fs::write(&tmp_path, raw).map_err(|err| AppError::io("write json temp file", err))?;
    replace_file(&tmp_path, path, "replace json file")
}

pub(super) fn backup_if_exists(path: &Path) -> AppResult<()> {
    if !path.exists() {
        return Ok(());
    }
    // install/uninstall 都保留覆盖前备份，避免用户自定义 hook 被误改后无法回退。
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let backup = path.with_extension(format!(
        "{}.bak.{timestamp}",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    fs::copy(path, backup).map_err(|err| AppError::io("backup config file", err))?;
    Ok(())
}

fn replace_file(from: &Path, to: &Path, context: &str) -> AppResult<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if to.exists() {
                // 某些平台上 rename 到已存在目标会失败，这里手动 remove 后再替换。
                fs::remove_file(to).map_err(|err| AppError::io(context, err))?;
                fs::rename(from, to).map_err(|err| AppError::io(context, err))
            } else {
                Err(AppError::io(context, rename_err))
            }
        }
    }
}
