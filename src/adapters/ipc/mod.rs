//! 各类 IPC 传输实现集合。
//!
//! 这一层的职责很纯粹：
//! 1. 负责把 `IpcRequestEnvelope` 编码后发到 daemon；
//! 2. 负责从 daemon 收到 `IpcResponseEnvelope`；
//! 3. 尽量让上层完全不关心 Unix socket / named pipe / TCP 的细节差异。

pub mod named_pipe;
pub mod unix_socket;
