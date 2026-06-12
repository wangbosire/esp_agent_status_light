use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Value, json};

use crate::model::{AppError, AppResult, HookCommand, HookSpec};
use crate::ports::runtime::RuntimeStore;
use crate::runtime_lock::FileLock;

pub(super) enum InstallCommandTarget {
    StableBinary { path: PathBuf },
    CargoRun { manifest_path: PathBuf },
}

impl InstallCommandTarget {
    pub(super) fn spec_exe(&self) -> &Path {
        match self {
            Self::StableBinary { path } => path.as_path(),
            Self::CargoRun { .. } => Path::new("cargo"),
        }
    }

    pub(super) fn apply_to_specs(&self, specs: &mut [HookSpec]) {
        if let Self::CargoRun { manifest_path } = self {
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
    fs::copy(&current, &tmp_path).map_err(|err| AppError::io("copy stable binary", err))?;
    replace_file(&tmp_path, &target, "replace stable binary")?;
    Ok(target)
}

pub(super) fn read_json_or_empty(path: &Path) -> AppResult<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = fs::read_to_string(path).map_err(|err| AppError::io("read config json", err))?;
    serde_json::from_str(&raw).map_err(|err| AppError::invalid("parse config json", err))
}

pub(super) fn write_json(path: &Path, value: &Value) -> AppResult<()> {
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
                fs::remove_file(to).map_err(|err| AppError::io(context, err))?;
                fs::rename(from, to).map_err(|err| AppError::io(context, err))
            } else {
                Err(AppError::io(context, rename_err))
            }
        }
    }
}
