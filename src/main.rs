//! 程序主入口模块。
//!
//! 这里刻意保持很薄，只负责把 CLI、日志和命令分发串起来，
//! 避免把任何业务规则写进入口文件，符合技术方案里“入口只做装配”的要求。

mod adapters;
mod cli;
mod command;
mod daemon;
mod model;
mod ports;
mod router;
mod runtime_lock;

use clap::Parser;

use crate::command::{CommandOutput, run};

/// 程序入口只负责三件事：
/// 1. 初始化结构化日志。
/// 2. 解析 CLI 参数。
/// 3. 把控制权交给命令分发层。
#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    match run(cli).await {
        Ok(CommandOutput::Json(value)) => {
            if let Ok(text) = serde_json::to_string_pretty(&value) {
                println!("{text}");
            }
        }
        Ok(CommandOutput::Text(text)) => {
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Ok(CommandOutput::Silent) => {}
        Err(err) => {
            let payload = serde_json::json!({
                "ok": false,
                "code": err.code,
                "message": err.message,
            });
            if let Ok(text) = serde_json::to_string_pretty(&payload) {
                eprintln!("{text}");
            }
            std::process::exit(1);
        }
    }
}
