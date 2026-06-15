//! `adapters::log::jsonl` 模块测试。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;

use super::*;
use crate::adapters::runtime::fs::FsRuntimeAdapter;
use crate::model::Mode;

fn temp_runtime_root(name: &str) -> PathBuf {
    // 用带 pid 的临时目录避免并发测试之间互相污染同一份日志文件。
    let root = std::env::temp_dir().join(format!("esp-jsonl-log-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn sample_event(index: usize) -> LogEvent {
    // 生成结构完整的样例事件，便于同时覆盖北京时间序列化和上下文字段落盘。
    LogEvent {
        timestamp: Utc::now(),
        level: "info".into(),
        kind: "test".into(),
        message: format!("message-{index}"),
        phase: Some("test.phase".into()),
        code: None,
        source: Some("test".into()),
        session: Some("session-1".into()),
        mode: Some(Mode::Busy),
        context: Some(json!({
            "index": index,
            "hook_event": "PreToolUse",
        })),
    }
}

#[test]
fn append_writes_both_events_and_runtime_logs() {
    let runtime = Arc::new(FsRuntimeAdapter::new(temp_runtime_root("double-write")));
    let adapter = JsonlLogAdapter::new(runtime.clone());

    adapter
        .append(sample_event(1))
        .expect("append should succeed");

    assert!(runtime.events_log_path().exists());
    assert!(runtime.runtime_log_path().exists());

    let events_raw = std::fs::read_to_string(runtime.events_log_path()).expect("read events log");
    let runtime_raw =
        std::fs::read_to_string(runtime.runtime_log_path()).expect("read runtime log");
    assert!(events_raw.contains("message-1"));
    assert!(events_raw.contains("+08:00"));
    assert!(runtime_raw.contains("message-1"));
    assert!(runtime_raw.contains("+08:00"));
    assert!(runtime_raw.contains("\"phase\":\"test.phase\""));
    assert!(runtime_raw.contains("\"hook_event\":\"PreToolUse\""));

    let _ = std::fs::remove_dir_all(runtime.runtime_root());
}

#[test]
fn append_runtime_writes_only_runtime_log() {
    let runtime = Arc::new(FsRuntimeAdapter::new(temp_runtime_root("runtime-only")));
    let adapter = JsonlLogAdapter::new(runtime.clone());

    adapter
        .append_runtime(sample_event(7))
        .expect("append_runtime should succeed");

    assert!(!runtime.events_log_path().exists());
    assert!(runtime.runtime_log_path().exists());

    let runtime_raw =
        std::fs::read_to_string(runtime.runtime_log_path()).expect("read runtime log");
    assert!(runtime_raw.contains("message-7"));

    let _ = std::fs::remove_dir_all(runtime.runtime_root());
}

#[test]
fn runtime_log_keeps_only_last_3000_entries() {
    let runtime = Arc::new(FsRuntimeAdapter::new(temp_runtime_root("trim")));
    let adapter = JsonlLogAdapter::new(runtime.clone());

    for index in 0..3005 {
        adapter
            .append(sample_event(index))
            .expect("append should succeed");
    }

    let runtime_raw =
        std::fs::read_to_string(runtime.runtime_log_path()).expect("read runtime log");
    let runtime_lines: Vec<&str> = runtime_raw.lines().collect();
    assert_eq!(runtime_lines.len(), MAX_RUNTIME_LOG_ENTRIES);
    let runtime_items: Vec<LogEvent> = runtime_lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("parse runtime log event"))
        .collect();
    assert_eq!(runtime_items[0].message, "message-5");
    assert_eq!(
        runtime_items
            .last()
            .expect("runtime log should not be empty")
            .message,
        "message-3004"
    );

    let events_raw = std::fs::read_to_string(runtime.events_log_path()).expect("read events log");
    assert_eq!(events_raw.lines().count(), 3005);

    let _ = std::fs::remove_dir_all(runtime.runtime_root());
}

#[test]
fn corrupted_lock_file_is_recovered_on_next_append() {
    let runtime = Arc::new(FsRuntimeAdapter::new(temp_runtime_root("corrupt-lock")));
    runtime.ensure_layout().expect("layout should succeed");
    let lock_path = runtime.runtime_log_path().with_extension("lock");
    std::fs::write(&lock_path, "not-a-pid").expect("write broken lock");

    let adapter = JsonlLogAdapter::new(runtime.clone());
    adapter
        .append_runtime(sample_event(1))
        .expect("append_runtime should recover from broken lock file");

    let runtime_raw =
        std::fs::read_to_string(runtime.runtime_log_path()).expect("read runtime log");
    assert!(runtime_raw.contains("message-1"));

    let _ = std::fs::remove_dir_all(runtime.runtime_root());
}
