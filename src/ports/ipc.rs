use std::sync::Arc;

use async_trait::async_trait;

use crate::model::{AppResult, IpcInfo, IpcRequestEnvelope, IpcResponseEnvelope};

#[async_trait]
pub trait IpcTransport: Send + Sync {
    /// 发起一次同步请求-响应式 IPC 调用。
    async fn request(&self, req: IpcRequestEnvelope) -> AppResult<IpcResponseEnvelope>;
}

/// handler 独立出来后，server adapter 就可以复用同一套传输层，
/// 不需要知道 daemon 内部状态结构。
#[async_trait]
pub trait IpcRequestHandler: Send + Sync {
    /// 处理一条已经解码完成的 IPC 请求。
    async fn handle(&self, req: IpcRequestEnvelope) -> IpcResponseEnvelope;
}

#[async_trait]
pub trait IpcServer: Send + Sync {
    /// 返回当前 server 的元信息，供 runtime 落盘。
    fn info(&self) -> IpcInfo;
    /// 启动监听循环，直到收到 shutdown 信号。
    async fn serve(
        &self,
        handler: Arc<dyn IpcRequestHandler>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()>;
}
