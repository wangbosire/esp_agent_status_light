//! JSONL 事件日志实现。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::model::{AppError, AppResult, LogEvent};
use crate::ports::log::EventLog;
use crate::ports::runtime::RuntimeStore;
use crate::runtime_lock::FileLock;

/// 运行日志只保留最近 3000 条，避免长期运行后单文件无限膨胀。
const MAX_RUNTIME_LOG_ENTRIES: usize = 3000;
const RUNTIME_LOG_TRIM_INTERVAL: usize = 128;

#[derive(Clone)]
pub struct JsonlLogAdapter {
    /// 通过 runtime store 获取日志路径，避免日志实现自己关心目录布局。
    runtime: Arc<dyn RuntimeStore>,
    /// 运行日志不需要每条 append 都全量读文件裁剪；用计数器做低频修剪。
    runtime_writes_since_trim: Arc<Mutex<usize>>,
}

impl JsonlLogAdapter {
    /// 使用给定 runtime store 创建 JSONL 日志实现。
    pub fn new(runtime: Arc<dyn RuntimeStore>) -> Self {
        Self {
            runtime,
            runtime_writes_since_trim: Arc::new(Mutex::new(0)),
        }
    }
}

impl EventLog for JsonlLogAdapter {
    fn append(&self, event: LogEvent) -> AppResult<()> {
        self.runtime.ensure_layout()?;
        let _guard = LogWriteGuard::acquire(&self.runtime.runtime_log_path())?;
        self.append_event_and_runtime_locked(
            &self.runtime.events_log_path(),
            &self.runtime.runtime_log_path(),
            event,
        )
    }

    fn append_runtime(&self, event: LogEvent) -> AppResult<()> {
        self.runtime.ensure_layout()?;
        let _guard = LogWriteGuard::acquire(&self.runtime.runtime_log_path())?;
        self.append_runtime_locked(&self.runtime.runtime_log_path(), event)
    }

    fn tail(&self, limit: usize) -> AppResult<Vec<LogEvent>> {
        // 做一层数量裁剪，避免用户一次读取过多日志。
        let limit = limit.clamp(1, 1000);
        let path = self.runtime.events_log_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = fs::read_to_string(path).map_err(|err| AppError::io("read events log", err))?;
        let lines: Vec<&str> = raw.lines().collect();
        let start = lines.len().saturating_sub(limit);
        let mut items = Vec::new();
        for line in &lines[start..] {
            match serde_json::from_str(line) {
                Ok(event) => items.push(event),
                Err(_) => {
                    // 日志是排障工具，不能因为一条半写/手改坏行导致整次 tail 失败。
                    // 坏行会被跳过；后续新日志仍可正常读取。
                    continue;
                }
            }
        }
        Ok(items)
    }
}

impl JsonlLogAdapter {
    fn append_event_and_runtime_locked(
        &self,
        events_path: &Path,
        runtime_path: &Path,
        event: LogEvent,
    ) -> AppResult<()> {
        // 采用 JSONL 追加写入：简单、稳定，而且适合按行 tail。
        let line = serde_json::to_string(&event)
            .map_err(|err| AppError::invalid("serialize log event", err))?;
        append_jsonl_line(events_path, &line, "events")?;
        append_jsonl_line(runtime_path, &line, "runtime")?;
        self.maybe_trim_runtime_log(runtime_path)
    }

    fn append_runtime_locked(&self, runtime_path: &Path, event: LogEvent) -> AppResult<()> {
        let line = serde_json::to_string(&event)
            .map_err(|err| AppError::invalid("serialize runtime log event", err))?;
        append_jsonl_line(runtime_path, &line, "runtime")?;
        self.maybe_trim_runtime_log(runtime_path)
    }

    fn maybe_trim_runtime_log(&self, runtime_path: &Path) -> AppResult<()> {
        let should_trim = {
            let mut writes = self.runtime_writes_since_trim.lock().map_err(|_| {
                AppError::new("lock_poisoned", "runtime log trim counter lock poisoned")
            })?;
            *writes += 1;
            if *writes < RUNTIME_LOG_TRIM_INTERVAL {
                false
            } else {
                *writes = 0;
                true
            }
        };
        if should_trim {
            trim_jsonl_to_last_n(runtime_path, MAX_RUNTIME_LOG_ENTRIES, "runtime")
        } else {
            Ok(())
        }
    }
}

/// 追加一行 JSONL 到指定日志文件。
fn append_jsonl_line(path: &Path, line: &str, label: &str) -> AppResult<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| AppError::io(&format!("open {label} log"), err))?;
    writeln!(file, "{line}").map_err(|err| AppError::io(&format!("append {label} log"), err))
}

/// 将 JSONL 日志裁剪到最近 N 条。
///
/// runtime 日志更偏排障用途，因此允许按条数截断来控制文件体积。
fn trim_jsonl_to_last_n(path: &Path, max_entries: usize, label: &str) -> AppResult<()> {
    let raw =
        fs::read_to_string(path).map_err(|err| AppError::io(&format!("read {label} log"), err))?;
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= max_entries {
        return Ok(());
    }

    let start = lines.len().saturating_sub(max_entries);
    let mut trimmed = lines[start..].join("\n");
    trimmed.push('\n');
    let tmp_path = path.with_extension(format!("{}.tmp", label));
    fs::write(&tmp_path, trimmed).map_err(|err| AppError::io(&format!("trim {label} log"), err))?;
    fs::rename(&tmp_path, path).map_err(|err| AppError::io(&format!("replace {label} log"), err))
}

struct LogWriteGuard {
    _lock: FileLock,
}

impl LogWriteGuard {
    fn acquire(runtime_log_path: &Path) -> AppResult<Self> {
        let lock_path = runtime_log_path.with_extension("lock");
        Ok(Self {
            _lock: FileLock::acquire(lock_path)?,
        })
    }
}

// 测试实现拆到独立目录，避免与 JSONL 日志写入主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/log/jsonl_tests.rs"]
mod tests;
