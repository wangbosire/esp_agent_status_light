//! `cli` 模块测试。
//!
//! 这里主要验证帮助文案是否包含关键说明和常用示例，
//! 避免 CLI 文本在后续迭代中被意外裁剪。

use clap::CommandFactory;

use super::*;

#[test]
fn root_help_includes_quick_start_examples() {
    let help = Cli::command().render_long_help().to_string();

    assert!(help.contains("AgentStatusLight 的命令行工具"));
    assert!(help.contains("esp send --mode demo"));
    assert!(help.contains("esp installations"));
    assert!(help.contains("esp install cursor --dir /path/to/project"));
}

#[test]
fn send_help_includes_mode_reference_and_hook_examples() {
    let mut cmd = Cli::command();
    let help = cmd
        .find_subcommand_mut("send")
        .expect("send subcommand should exist")
        .render_long_help()
        .to_string();

    assert!(help.contains("向本地 daemon 发送一个状态事件"));
    assert!(help.contains("thinking   AI 正在思考、分析、规划"));
    assert!(help.contains("--session auto"));
}

#[test]
fn install_help_lists_supported_targets() {
    let mut cmd = Cli::command();
    let help = cmd
        .find_subcommand_mut("install")
        .expect("install subcommand should exist")
        .render_long_help()
        .to_string();

    assert!(help.contains("当前支持 `codex`、`cursor`、`claude`"));
    assert!(help.contains("esp install claude --dir ."));
}

#[test]
fn installations_help_mentions_default_all_targets() {
    let mut cmd = Cli::command();
    let help = cmd
        .find_subcommand_mut("installations")
        .expect("installations subcommand should exist")
        .render_long_help()
        .to_string();

    assert!(help.contains("不指定目标时默认列出全部已支持 Agent"));
    assert!(help.contains("esp installations cursor"));
}
