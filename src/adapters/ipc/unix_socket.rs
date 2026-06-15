//! Unix Domain Socket IPC 实现。
//!
//! 这是 Unix/macOS 默认使用的本地 IPC 方案，特点是：
//! 1. 不暴露网络端口；
//! 2. 请求-响应模型简单；
//! 3. 与 runtime 目录里的 socket 文件天然绑定，便于排障。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::{Duration, timeout};

use crate::model::{AppError, AppResult, IpcInfo, IpcRequestEnvelope, IpcResponseEnvelope};
use crate::ports::ipc::{IpcRequestHandler, IpcServer, IpcTransport};

#[derive(Debug, Clone)]
pub struct UnixSocketTransport {
    /// Unix socket 文件路径。
    path: PathBuf,
}

impl UnixSocketTransport {
    /// 使用指定 socket 路径创建 IPC 客户端。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl IpcTransport for UnixSocketTransport {
    async fn request(&self, req: IpcRequestEnvelope) -> AppResult<IpcResponseEnvelope> {
        #[cfg(not(unix))]
        {
            let _ = req;
            return Err(AppError::unsupported(
                "unix socket is not supported on this platform",
            ));
        }

        #[cfg(unix)]
        {
            // 客户端请求采用短超时，避免 Hook 卡在 daemon 不可用的场景中。
            // 这里分开限制 connect/read 两段超时，是为了让错误定位更清楚：
            // 到底是连不上 daemon，还是 daemon 收到了但没及时响应。
            let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(&self.path))
                .await
                .map_err(|_| AppError::new("ipc_timeout", "connect daemon ipc timed out"))?
                .map_err(|err| AppError::io("connect daemon ipc", err))?;

            let raw = serde_json::to_string(&req)
                .map_err(|err| AppError::invalid("serialize ipc request", err))?;
            stream
                .write_all(raw.as_bytes())
                .await
                .map_err(|err| AppError::io("write ipc request", err))?;
            stream
                .write_all(b"\n")
                .await
                .map_err(|err| AppError::io("write ipc newline", err))?;
            stream
                .flush()
                .await
                .map_err(|err| AppError::io("flush ipc request", err))?;

            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            // 协议约定服务端一行返回一个完整 JSON 响应，因此读到换行即可完成一次 RPC。
            timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .map_err(|_| AppError::new("ipc_timeout", "read daemon ipc response timed out"))?
                .map_err(|err| AppError::io("read ipc response", err))?;

            serde_json::from_str(line.trim())
                .map_err(|err| AppError::invalid("parse ipc response", err))
        }
    }
}

pub struct UnixSocketServer {
    /// Unix socket 文件路径。
    path: PathBuf,
}

impl UnixSocketServer {
    /// 使用指定 socket 路径创建 IPC 服务端。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl IpcServer for UnixSocketServer {
    fn info(&self) -> IpcInfo {
        IpcInfo {
            kind: "unix_socket".into(),
            address: self.path.to_string_lossy().to_string(),
            version: 1,
            started_at: Utc::now(),
        }
    }

    async fn serve(
        &self,
        handler: Arc<dyn IpcRequestHandler>,
        mut shutdown: watch::Receiver<bool>,
    ) -> AppResult<()> {
        #[cfg(not(unix))]
        {
            let _ = handler;
            let _ = shutdown;
            return Err(AppError::unsupported(
                "unix socket server is not supported on this platform",
            ));
        }

        #[cfg(unix)]
        {
            // 遗留 socket 文件不应导致下次启动失败，因此先尽力清理。
            // 这里不做更复杂的“验证它是不是自己创建的 socket”判断，
            // 因为 runtime 目录本身已经是本项目私有空间。
            if self.path.exists() {
                let _ = std::fs::remove_file(&self.path);
            }

            let listener = UnixListener::bind(&self.path)
                .map_err(|err| AppError::io("bind unix socket", err))?;

            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_ok() && *shutdown.borrow() {
                            break;
                        }
                    }
                    accept_result = listener.accept() => {
                        // 每个连接独立起任务处理，避免单个慢请求阻塞后续 Hook。
                        // 单连接任务内部仍然只处理一条请求，协议保持最简单的 request-response 模式。
                        let (stream, _) = accept_result.map_err(|err| AppError::io("accept unix socket", err))?;
                        let handler = handler.clone();
                        tokio::spawn(async move {
                            let _ = handle_stream(stream, handler).await;
                        });
                    }
                }
            }

            let _ = std::fs::remove_file(&self.path);
            Ok(())
        }
    }
}

#[cfg(unix)]
/// 处理单个 Unix socket 连接上的一条请求。
async fn handle_stream(
    mut stream: UnixStream,
    handler: Arc<dyn IpcRequestHandler>,
) -> AppResult<()> {
    // 协议采用“一行一个 JSON 包”的简单 framing，便于不同传输层复用。
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .await
            .map_err(|err| AppError::io("read unix socket request", err))?;
    }

    let request: IpcRequestEnvelope = serde_json::from_str(line.trim())
        .map_err(|err| AppError::invalid("parse ipc request", err))?;
    let response = handler.handle(request).await;
    let raw = serde_json::to_string(&response)
        .map_err(|err| AppError::invalid("serialize ipc response", err))?;
    stream
        .write_all(raw.as_bytes())
        .await
        .map_err(|err| AppError::io("write unix socket response", err))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|err| AppError::io("write unix socket newline", err))?;
    stream
        .flush()
        .await
        .map_err(|err| AppError::io("flush unix socket response", err))?;
    Ok(())
}
