use crate::model::{AppResult, LogEvent};

pub trait EventLog: Send + Sync {
    /// 追加一条事件日志。
    fn append(&self, event: LogEvent) -> AppResult<()>;
    /// 追加一条仅写入 runtime.log 的运行链路日志。
    ///
    /// 这类日志用于记录“程序当前走到了哪个处理节点”，
    /// 方便排查 send -> IPC -> daemon -> router -> BLE 过程中的具体卡点，
    /// 不应污染面向事件观察的 events.log。
    fn append_runtime(&self, event: LogEvent) -> AppResult<()>;
    /// 读取最近 N 条日志。
    fn tail(&self, limit: usize) -> AppResult<Vec<LogEvent>>;
}
