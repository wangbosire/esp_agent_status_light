//! JSONL 事件日志实现。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;

use crate::model::{AppError, AppResult, LogEvent};
use crate::ports::log::EventLog;
use crate::ports::runtime::RuntimeStore;

#[derive(Clone)]
pub struct JsonlLogAdapter {
    /// 通过 runtime store 获取日志路径，避免日志实现自己关心目录布局。
    runtime: Arc<dyn RuntimeStore>,
}

impl JsonlLogAdapter {
    pub fn new(runtime: Arc<dyn RuntimeStore>) -> Self {
        Self { runtime }
    }
}

impl EventLog for JsonlLogAdapter {
    fn append(&self, event: LogEvent) -> AppResult<()> {
        self.runtime.ensure_layout()?;
        let path = self.runtime.events_log_path();
        // 采用 JSONL 追加写入：简单、稳定，而且适合按行 tail。
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|err| AppError::io("open events log", err))?;
        let line = serde_json::to_string(&event)
            .map_err(|err| AppError::invalid("serialize log event", err))?;
        writeln!(file, "{line}").map_err(|err| AppError::io("append events log", err))
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
            let event: LogEvent = serde_json::from_str(line)
                .map_err(|err| AppError::invalid("parse log event", err))?;
            items.push(event);
        }
        Ok(items)
    }
}
