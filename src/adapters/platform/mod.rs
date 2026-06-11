pub mod linux;
pub mod macos;
pub mod windows;

// 平台差异适配层公共实现。
//
// 主要负责 runtime 根目录、shell 引号规则以及后台 daemon 拉起方式等差异。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, path::PathBuf as StdPathBuf};

use crate::model::{AppError, AppResult, HookCommand};
use crate::ports::platform::PlatformAdapter;

pub fn current_platform() -> Box<dyn PlatformAdapter> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosAdapter)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsAdapter)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Box::new(linux::LinuxAdapter)
    }
}

pub(crate) fn unix_runtime_root() -> PathBuf {
    // Unix/macOS 当前共用同一 runtime 目录策略。
    env::var("HOME")
        .map(StdPathBuf::from)
        .unwrap_or_else(|_| StdPathBuf::from("."))
        .join(".esp-agent-status-light")
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_runtime_root() -> PathBuf {
    env::var("LOCALAPPDATA")
        .map(StdPathBuf::from)
        .unwrap_or_else(|_| StdPathBuf::from("."))
        .join("AgentStatusLight")
}

pub(crate) fn shell_quote(command: &HookCommand) -> String {
    // POSIX shell quoting：尽量直出安全字符，必要时再用单引号包裹。
    let exe = quote_shell_token(command.exe.to_string_lossy().as_ref());
    let args = command
        .args
        .iter()
        .map(|arg| quote_shell_token(arg))
        .collect::<Vec<_>>()
        .join(" ");
    if args.is_empty() {
        exe
    } else {
        format!("{exe} {args}")
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_shell_quote(command: &HookCommand) -> String {
    // Windows 下不能复用 POSIX 的单引号规则，这里单独实现双引号转义。
    let exe = quote_windows_token(command.exe.to_string_lossy().as_ref());
    let args = command
        .args
        .iter()
        .map(|arg| quote_windows_token(arg))
        .collect::<Vec<_>>()
        .join(" ");
    if args.is_empty() {
        exe
    } else {
        format!("{exe} {args}")
    }
}

pub(crate) fn quote_shell_token(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn quote_windows_token(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '\\' | '.' | ':'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

pub(crate) fn spawn_background(exe: &Path) -> AppResult<()> {
    // 第一阶段使用最朴素的子进程 detach 方式即可，避免引入平台专属后台服务机制。
    Command::new(exe)
        .arg("daemon")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| AppError::io("spawn background daemon", err))?;
    Ok(())
}
