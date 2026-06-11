//! Windows Named Pipe IPC 实现。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
#[cfg(windows)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::model::{AppError, AppResult, IpcInfo, IpcRequestEnvelope, IpcResponseEnvelope};
use crate::ports::ipc::{IpcRequestHandler, IpcServer, IpcTransport};

#[cfg(windows)]
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeServer as TokioNamedPipeServer, ServerOptions,
};
#[cfg(windows)]
use tokio::time::{Duration, sleep, timeout};

#[derive(Debug, Clone)]
pub struct NamedPipeTransport {
    /// 命令层传入的“逻辑 IPC 路径”。
    /// Windows 实际会被映射成稳定的 named pipe 名称。
    #[cfg_attr(not(windows), allow(dead_code))]
    path: PathBuf,
}

impl NamedPipeTransport {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl IpcTransport for NamedPipeTransport {
    async fn request(&self, req: IpcRequestEnvelope) -> AppResult<IpcResponseEnvelope> {
        #[cfg(not(windows))]
        {
            let _ = req;
            return Err(AppError::unsupported(
                "named pipe transport is only available on Windows",
            ));
        }

        #[cfg(windows)]
        {
            let pipe_name = pipe_name_from_path(&self.path);
            // 命名管道在服务端尚未 ready 的启动窗口期，可能出现短暂 NotFound/busy，
            // 所以这里单独做重试等待。
            let mut client = timeout(Duration::from_secs(2), open_client_with_retry(&pipe_name))
                .await
                .map_err(|_| {
                    AppError::new("ipc_timeout", "connect daemon named pipe timed out")
                })??;

            let raw = serde_json::to_string(&req)
                .map_err(|err| AppError::invalid("serialize ipc request", err))?;
            client
                .write_all(raw.as_bytes())
                .await
                .map_err(|err| AppError::io("write named pipe request", err))?;
            client
                .write_all(b"\n")
                .await
                .map_err(|err| AppError::io("write named pipe newline", err))?;
            client
                .flush()
                .await
                .map_err(|err| AppError::io("flush named pipe request", err))?;

            let mut reader = BufReader::new(client);
            let mut line = String::new();
            timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    AppError::new("ipc_timeout", "read daemon named pipe response timed out")
                })?
                .map_err(|err| AppError::io("read named pipe response", err))?;

            serde_json::from_str(line.trim())
                .map_err(|err| AppError::invalid("parse ipc response", err))
        }
    }
}

pub struct NamedPipeServer {
    path: PathBuf,
}

impl NamedPipeServer {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl IpcServer for NamedPipeServer {
    fn info(&self) -> IpcInfo {
        IpcInfo {
            kind: "named_pipe".into(),
            address: pipe_name_from_path(&self.path),
            version: 1,
            started_at: Utc::now(),
        }
    }

    async fn serve(
        &self,
        handler: Arc<dyn IpcRequestHandler>,
        shutdown: watch::Receiver<bool>,
    ) -> AppResult<()> {
        #[cfg(not(windows))]
        {
            let _ = handler;
            let _ = shutdown;
            return Err(AppError::unsupported(
                "named pipe server is only available on Windows",
            ));
        }

        #[cfg(windows)]
        {
            let mut shutdown = shutdown;
            let pipe_name = pipe_name_from_path(&self.path);
            // Windows Named Pipe 不是像 Unix socket 那样“bind 一次就能 accept 多次”；
            // 必须始终至少保留一个尚未连接的 server instance，客户端连接才不会随机报 NotFound。
            let mut listener = create_server_instance(&pipe_name, true)?;

            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_ok() && *shutdown.borrow() {
                            break;
                        }
                    }
                    connect_result = listener.connect() => {
                        connect_result.map_err(|err| AppError::io("connect named pipe client", err))?;
                        let connected = listener;
                        listener = create_server_instance(&pipe_name, false)?;
                        let handler = handler.clone();
                        tokio::spawn(async move {
                            let _ = handle_pipe_stream(connected, handler).await;
                        });
                    }
                }
            }

            Ok(())
        }
    }
}

fn pipe_name_from_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.starts_with(r"\\.\pipe\") {
        return raw.into_owned();
    }

    // RuntimeStore 在所有平台都产出 “default_ipc_path”，但在 Windows 上真正使用的是
    // Named Pipe 名称而不是文件路径。这里把路径稳定映射成 pipe name：
    // 1. 文件名作为人类可读前缀，便于排障；
    // 2. 完整路径做稳定 hash，避免不同 runtime root 发生重名碰撞。
    let readable = raw
        .rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.split('.').next())
        .map(sanitize_pipe_component)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "daemon".into());
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    let hash = hasher.finish();
    format!(r"\\.\pipe\esp-agent-status-light-{readable}-{hash:016x}")
}

fn sanitize_pipe_component(value: &str) -> String {
    // pipe 名称只保留安全字符，其余统一替换成 `-`，避免宿主路径包含特殊字符。
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(windows)]
const ERROR_PIPE_BUSY_CODE: i32 = 231;

#[cfg(windows)]
async fn open_client_with_retry(
    pipe_name: &str,
) -> AppResult<tokio::net::windows::named_pipe::NamedPipeClient> {
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    || err.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE) =>
            {
                // 服务端可能还没来得及创建下一个空闲实例，稍等片刻再试。
                sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(AppError::io("open named pipe client", err)),
        }
    }
}

#[cfg(windows)]
fn create_server_instance(
    pipe_name: &str,
    first_instance: bool,
) -> AppResult<TokioNamedPipeServer> {
    // `first_pipe_instance(true)` 能帮助系统在重复启动时尽早暴露冲突。
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first_instance);
    options
        .create(pipe_name)
        .map_err(|err| AppError::io("create named pipe server", err))
}

#[cfg(windows)]
async fn handle_pipe_stream(
    mut stream: TokioNamedPipeServer,
    handler: Arc<dyn IpcRequestHandler>,
) -> AppResult<()> {
    // 与 Unix/TCP 传输保持完全一致的按行 JSON 协议，减少上层分支。
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .await
            .map_err(|err| AppError::io("read named pipe request", err))?;
    }

    let request: IpcRequestEnvelope = serde_json::from_str(line.trim())
        .map_err(|err| AppError::invalid("parse ipc request", err))?;
    let response = handler.handle(request).await;
    let raw = serde_json::to_string(&response)
        .map_err(|err| AppError::invalid("serialize ipc response", err))?;
    stream
        .write_all(raw.as_bytes())
        .await
        .map_err(|err| AppError::io("write named pipe response", err))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|err| AppError::io("write named pipe newline", err))?;
    stream
        .flush()
        .await
        .map_err(|err| AppError::io("flush named pipe response", err))?;
    let _ = stream.disconnect();
    Ok(())
}

// 测试实现拆到独立目录，避免与 Windows named pipe 传输主逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/ipc/named_pipe_tests.rs"]
mod tests;
