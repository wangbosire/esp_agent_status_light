use crate::model::{AppResult, LogEvent};

pub trait EventLog: Send + Sync {
    /// 追加一条事件日志。
    fn append(&self, event: LogEvent) -> AppResult<()>;
    /// 读取最近 N 条日志。
    fn tail(&self, limit: usize) -> AppResult<Vec<LogEvent>>;
}
