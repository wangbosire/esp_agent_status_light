//! `adapters::ipc::named_pipe` 模块测试。

use std::path::PathBuf;

use super::pipe_name_from_path;

#[test]
fn keeps_existing_named_pipe_path() {
    let path = PathBuf::from(r"\\.\pipe\custom-agent-light");
    assert_eq!(pipe_name_from_path(&path), r"\\.\pipe\custom-agent-light");
}

#[test]
fn derives_stable_named_pipe_name_from_regular_path() {
    let path = PathBuf::from(r"C:\Users\alice\AppData\Local\AgentStatusLight\runtime\daemon.sock");
    let pipe_name = pipe_name_from_path(&path);
    assert!(pipe_name.starts_with(r"\\.\pipe\esp-agent-status-light-daemon-"));
    assert_eq!(pipe_name, pipe_name_from_path(&path));
}
