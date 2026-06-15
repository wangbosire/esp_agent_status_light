//! 平台差异适配层公共实现。
//!
//! 主要负责 runtime 根目录、shell 引号规则以及后台 daemon 拉起方式等差异，
//! 让命令层和安装层尽量围绕稳定 trait 协作，而不是到处散落 `cfg` 分支。

pub mod linux;
pub mod macos;
pub mod windows;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, path::PathBuf as StdPathBuf};

use crate::model::{AppError, AppResult, HookCommand};
use crate::ports::platform::PlatformAdapter;

/// 根据当前目标平台选择默认平台适配器。
pub fn current_platform() -> Box<dyn PlatformAdapter> {
    // 平台选择集中在这里，避免上层到处写 `cfg` 分支。
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

/// 解析当前用户 home 目录。
pub(crate) fn user_home_dir() -> AppResult<PathBuf> {
    // 优先读取各平台最常见的 home 环境变量。
    // 这里不直接依赖第三方目录库，是为了减少安装工具链/交叉平台时的依赖面。
    if let Ok(home) = env::var("HOME") {
        return Ok(StdPathBuf::from(home));
    }
    if let Ok(profile) = env::var("USERPROFILE") {
        return Ok(StdPathBuf::from(profile));
    }
    if let (Ok(drive), Ok(path)) = (env::var("HOMEDRIVE"), env::var("HOMEPATH")) {
        return Ok(StdPathBuf::from(format!("{drive}{path}")));
    }
    Err(AppError::new(
        "missing_home_dir",
        "HOME, USERPROFILE, or HOMEDRIVE/HOMEPATH is not set",
    ))
}

/// 计算 Unix/macOS 平台默认 runtime 根目录。
pub(crate) fn unix_runtime_root() -> AppResult<PathBuf> {
    // Unix/macOS 当前共用同一 runtime 目录策略。
    let home = user_home_dir()?;
    Ok(home.join(".esp-agent-status-light"))
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_runtime_root() -> AppResult<PathBuf> {
    // Windows 目录优先级按常见程度排序，
    // 保证用户环境变量不完整时仍有尽可能高的可恢复概率。
    let appdata = env::var("LOCALAPPDATA")
        .map(StdPathBuf::from)
        .or_else(|_| env::var("APPDATA").map(StdPathBuf::from))
        .or_else(|_| {
            env::var("USERPROFILE")
                .map(|value| StdPathBuf::from(value).join("AppData").join("Local"))
        })
        .map_err(|_| {
            AppError::new(
                "missing_appdata_dir",
                "LOCALAPPDATA, APPDATA, or USERPROFILE is not set",
            )
        })?;
    Ok(appdata.join("AgentStatusLight"))
}

/// 将命令渲染为 POSIX shell 可直接执行的字符串。
pub(crate) fn shell_quote(command: &HookCommand) -> String {
    // POSIX shell quoting：尽量直出安全字符，必要时再用单引号包裹。
    // 这里不直接拼 `Command`，因为安装器最终需要写入的是宿主工具配置里的字符串命令。
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

/// 对单个 POSIX shell token 做安全引用。
pub(crate) fn quote_shell_token(value: &str) -> String {
    // 安全字符集合尽量宽一点，避免常见路径/参数被过度引用，影响可读性。
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
    // Windows token 规则和 POSIX 完全不同，因此必须单独维护引用逻辑。
    // 这里采用偏保守策略：能直出就直出，剩余情况统一走双引号包裹。
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '\\' | '.' | ':'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

/// 以最小依赖方式拉起后台 daemon 进程。
///
/// 当前实现统一通过 `esp daemon --foreground` 子进程承载真正服务逻辑，
/// 外层命令只负责把它从当前终端交互中脱离。
pub(crate) fn spawn_background(exe: &Path) -> AppResult<()> {
    // 第一阶段使用最朴素的子进程 detach 方式即可，避免引入平台专属后台服务机制。
    // 真正的服务逻辑仍然运行在 `esp daemon --foreground` 这条路径上，
    // 这样前台和后台模式共享同一套 daemon 主流程。
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
