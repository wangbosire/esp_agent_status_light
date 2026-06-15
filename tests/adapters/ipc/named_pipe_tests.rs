//! `adapters::ipc::named_pipe` 模块测试。

use std::path::PathBuf;

use super::pipe_name_from_path;

#[test]
fn keeps_existing_named_pipe_path() {
    // 已经是合法 pipe 名称时不应再次 hash/重写，避免外部显式注入路径失效。
    let path = PathBuf::from(r"\\.\pipe\custom-agent-light");
    assert_eq!(pipe_name_from_path(&path), r"\\.\pipe\custom-agent-light");
}

#[test]
fn derives_stable_named_pipe_name_from_regular_path() {
    // 普通文件路径会被稳定映射成“可读前缀 + hash”的 pipe 名称，保证跨进程一致。
    let path = PathBuf::from(r"C:\Users\alice\AppData\Local\AgentStatusLight\runtime\daemon.sock");
    let pipe_name = pipe_name_from_path(&path);
    assert!(pipe_name.starts_with(r"\\.\pipe\esp-agent-status-light-daemon-"));
    assert_eq!(pipe_name, pipe_name_from_path(&path));
}
