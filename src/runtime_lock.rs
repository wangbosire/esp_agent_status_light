//! 运行时文件锁辅助。
//!
//! 用于串行化 daemon 启动、安装过程中的稳定二进制写入，以及日志热路径写入。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::model::{AppError, AppResult};

/// 进程间共享的轻量级文件锁。
///
/// 这里不依赖平台专属 flock / named mutex，而是使用“创建新文件即持锁”的方式：
/// - 简单；
/// - 跨平台；
/// - 足以覆盖本项目里短时间串行化的小临界区。
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref().to_path_buf();
        // 重试窗口保持很短，因为调用点本身都是启动/安装/日志这种小临界区。
        for _ in 0..20 {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    // 锁文件内容记录持有者 pid，便于下次启动时判断是否为僵尸锁。
                    writeln!(file, "{}", std::process::id())
                        .map_err(|err| AppError::io("write file lock pid", err))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    match stale_lock_owner(&path) {
                        Ok(true) => {
                            // 明确确认旧持有者已死时，直接清理僵尸锁再重试。
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                        Ok(false) => {}
                        Err(_) => {
                            // 锁文件损坏时也按“不可恢复的旧锁”处理，
                            // 避免因为一份坏文件永久卡死启动/日志链路。
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                    }
                    // 当前确实有活着的持有者时，短暂等待后重试。
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
        // Drop 阶段只做尽力清理，不能因为删锁失败影响主流程收尾。
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn process_is_alive(pid: u32) -> AppResult<bool> {
    #[cfg(unix)]
    {
        // `kill -0` 不发送信号，只做进程存在性探测。
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map_err(|err| AppError::io("check pid with kill -0", err))?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        // Windows 没有 `kill -0`，这里退回 tasklist 过滤 PID 的方式做近似探测。
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
    // 锁文件的唯一语义就是“里面记录了持锁 pid”，因此这里只解析这一项。
    let raw = fs::read_to_string(path).map_err(|err| AppError::io("read file lock pid", err))?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .map_err(|err| AppError::invalid("parse file lock pid", err))?;
    Ok(!process_is_alive(pid)?)
}
