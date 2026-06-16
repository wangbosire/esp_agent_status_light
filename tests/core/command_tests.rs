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
