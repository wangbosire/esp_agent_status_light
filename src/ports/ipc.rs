//! IPC 传输与服务端口。
//!
//! 它把“怎么收发请求”与“请求背后的业务含义”分离开，
//! 这样 daemon 可以复用不同传输层实现。

use std::sync::Arc;

use async_trait::async_trait;

use crate::model::{AppResult, IpcInfo, IpcRequestEnvelope, IpcResponseEnvelope};

#[async_trait]
pub trait IpcTransport: Send + Sync {
    /// 发起一次同步请求-响应式 IPC 调用。
    ///
    /// CLI 层只需要这个简单能力，不需要感知底层 socket / pipe / TCP 细节。
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
    ///
    /// server 本身不应理解业务；它只负责解码请求、调用 handler、再编码响应。
    async fn serve(
        &self,
        handler: Arc<dyn IpcRequestHandler>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()>;
}
