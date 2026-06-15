//! TCP loopback IPC 实现。
//!
//! 主要用于调试或受限环境下的降级测试，不作为正式默认传输。

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::{Duration, timeout};

const IPC_SERVER_READ_TIMEOUT: Duration = Duration::from_secs(2);

use crate::model::{AppError, AppResult, IpcInfo, IpcRequestEnvelope, IpcResponseEnvelope};
use crate::ports::ipc::{IpcRequestHandler, IpcServer, IpcTransport};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TcpLoopbackTransport {
    /// 目标回环地址，例如 `127.0.0.1:NNNN`。
    addr: SocketAddr,
}

#[allow(dead_code)]
impl TcpLoopbackTransport {
    /// 使用指定回环地址创建 TCP IPC 客户端。
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

#[async_trait]
impl IpcTransport for TcpLoopbackTransport {
    async fn request(&self, req: IpcRequestEnvelope) -> AppResult<IpcResponseEnvelope> {
        // 逻辑与 Unix/named pipe 保持一致，同样使用单行 JSON 请求与响应。
        // TCP 版本主要用于调试，因此这里不做额外复杂优化，只追求行为一致。
        let mut stream = timeout(Duration::from_secs(2), TcpStream::connect(self.addr))
            .await
            .map_err(|_| AppError::new("ipc_timeout", "connect tcp loopback timed out"))?
            .map_err(|err| AppError::io("connect tcp loopback", err))?;
        let raw = serde_json::to_string(&req)
            .map_err(|err| AppError::invalid("serialize ipc request", err))?;
        stream
            .write_all(raw.as_bytes())
            .await
            .map_err(|err| AppError::io("write tcp request", err))?;
        stream
            .write_all(b"\n")
            .await
            .map_err(|err| AppError::io("write tcp newline", err))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|err| AppError::io("read tcp response", err))?;
        serde_json::from_str(line.trim())
            .map_err(|err| AppError::invalid("parse tcp response", err))
    }
}

#[allow(dead_code)]
pub struct TcpLoopbackServer {
    /// 服务端监听地址。
    addr: SocketAddr,
}

#[allow(dead_code)]
impl TcpLoopbackServer {
    /// 使用指定回环地址创建 TCP IPC 服务端。
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

#[async_trait]
impl IpcServer for TcpLoopbackServer {
    fn info(&self) -> IpcInfo {
        IpcInfo {
            kind: "tcp_loopback".into(),
            address: self.addr.to_string(),
            version: 1,
            started_at: Utc::now(),
        }
    }

    async fn serve(
        &self,
        handler: Arc<dyn IpcRequestHandler>,
        mut shutdown: watch::Receiver<bool>,
    ) -> AppResult<()> {
        // 调试 server 不要求多实例/抢占式自恢复，只要能在本机回环口稳定收发即可。
        let listener = TcpListener::bind(self.addr)
            .await
            .map_err(|err| AppError::io("bind tcp loopback", err))?;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        break;
                    }
                }
                    accept_result = listener.accept() => {
                        // 调试 server 允许“尽力而为”处理：解析失败直接丢弃，不影响整体循环。
                        let (mut stream, _) = accept_result.map_err(|err| AppError::io("accept tcp loopback", err))?;
                        let handler = handler.clone();
                    tokio::spawn(async move {
                        let mut line = String::new();
                        {
                            let mut reader = BufReader::new(&mut stream);
                            if timeout(IPC_SERVER_READ_TIMEOUT, reader.read_line(&mut line)).await.is_err() {
                                return;
                            }
                        }
                        let Ok(request) = serde_json::from_str::<IpcRequestEnvelope>(line.trim()) else {
                            return;
                        };
                        let response = handler.handle(request).await;
                        if let Ok(raw) = serde_json::to_string(&response) {
                            let _ = stream.write_all(raw.as_bytes()).await;
                            let _ = stream.write_all(b"\n").await;
                            let _ = stream.flush().await;
                        }
                    });
                }
            }
        }
        Ok(())
    }
}
