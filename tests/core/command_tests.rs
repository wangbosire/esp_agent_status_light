//! `command` 模块测试。
//!
//! 这里主要验证 `cargo run` 调试模式下 Hook 命令的拼装规则，
//! 确保开发态与 release 安装态继续保持既定差异。

use super::*;
use std::path::Path;

#[test]
fn debug_target_binary_uses_cargo_run_hooks() {
    assert!(should_use_cargo_run_hooks(Path::new(
        "/tmp/esp/target/debug/esp"
    )));
}

#[test]
fn release_target_binary_keeps_stable_binary_mode() {
    assert!(!should_use_cargo_run_hooks(Path::new(
        "/tmp/esp/target/release/esp"
    )));
}

#[test]
fn cargo_run_hook_command_wraps_send_args() {
    let command = build_cargo_run_hook_command(
        Path::new("/tmp/esp/Cargo.toml"),
        &[
            "send".into(),
            "--mode".into(),
            "busy".into(),
            "--source".into(),
            "cursor".into(),
        ],
    );

    assert_eq!(command.exe, PathBuf::from("cargo"));
    assert_eq!(
        command.args,
        vec![
            "run",
            "--manifest-path",
            "/tmp/esp/Cargo.toml",
            "--",
            "send",
            "--mode",
            "busy",
            "--source",
            "cursor",
        ]
    );
}

#[test]
fn ble_scan_duration_must_be_in_supported_range() {
    assert!(validate_ble_scan_duration(1).is_ok());
    assert!(validate_ble_scan_duration(60).is_ok());

    let err = validate_ble_scan_duration(0).expect_err("zero duration should be rejected");
    assert_eq!(err.code, "invalid_ble_scan_duration");

    let err = validate_ble_scan_duration(61).expect_err("long duration should be rejected");
    assert_eq!(err.code, "invalid_ble_scan_duration");
}

#[test]
fn hook_input_keeps_original_stdin_for_runtime_logs() {
    let raw = "{\n  \"hook_event_name\": \"PreToolUse\",\n  \"tool_name\": \"Read\"\n}\n";
    let input = io::hook_input_from_raw(raw.into());

    assert_eq!(input.raw_input.as_deref(), Some(raw));
    assert_eq!(
        input.parsed_json.as_ref().and_then(|value| {
            value
                .get("hook_event_name")
                .and_then(serde_json::Value::as_str)
        }),
        Some("PreToolUse")
    );
    assert!(input.parse_error.is_none());
}

#[test]
fn hook_input_keeps_original_stdin_when_json_is_invalid() {
    let raw = "{not-json";
    let input = io::hook_input_from_raw(raw.into());

    assert_eq!(input.raw_input.as_deref(), Some(raw));
    assert_eq!(input.parsed_json, Some(serde_json::json!({})));
    assert!(input.parse_error.is_some());
}
