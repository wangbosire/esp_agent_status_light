//! 运行时文件锁辅助。
//!
//! 用于串行化 daemon 启动、安装过程中的稳定二进制写入，以及日志热路径写入。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::model::{AppError, AppResult};

pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref().to_path_buf();
        for _ in 0..20 {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .map_err(|err| AppError::io("write file lock pid", err))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    match stale_lock_owner(&path) {
                        Ok(true) => {
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                        Ok(false) => {}
                        Err(_) => {
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(AppError::io("acquire file lock", err)),
            }
        }
        Err(AppError::new(
            "lock_timeout",
            format!("timed out waiting for lock: {}", path.display()),
        ))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn process_is_alive(pid: u32) -> AppResult<bool> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map_err(|err| AppError::io("check pid with kill -0", err))?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .map_err(|err| AppError::io("check pid with tasklist", err))?;
        if !output.status.success() {
            return Err(AppError::new(
                "pid_check_failed",
                format!("tasklist returned status {}", output.status),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().any(|line| {
            line.contains(&format!(",\"{pid}\"")) || line.contains(&format!(",{pid},"))
        }))
    }
}

fn stale_lock_owner(path: &Path) -> AppResult<bool> {
    let raw = fs::read_to_string(path).map_err(|err| AppError::io("read file lock pid", err))?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .map_err(|err| AppError::invalid("parse file lock pid", err))?;
    Ok(!process_is_alive(pid)?)
}
