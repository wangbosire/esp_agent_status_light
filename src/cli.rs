use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::model::Mode;

/// CLI 只表达用户意图，不放业务规则。
/// 具体逻辑在 `command.rs` 中完成。
#[derive(Debug, Parser)]
#[command(name = "esp", version, about = "AgentStatusLight CLI")]
pub struct Cli {
    /// 具体子命令。
    /// `clap` 只负责把命令行解析成结构体，后续解释交给命令分发层。
    #[command(subcommand)]
    pub command: Commands,
}

/// 所有对外暴露的命令面。
/// 这里严格对齐技术方案中的命令设计，避免在 CLI 层提前做“智能推断”。
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// 启动后台 daemon。
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    /// 发送一个状态事件给 daemon。
    Send {
        /// 用户显式传入的 mode。
        /// 对 Hook 模式来说，它只是兜底值；是否真的采用，要再经过 `resolve_mode`。
        #[arg(long)]
        mode: Mode,
        /// 事件来源，例如 `codex` / `cursor` / `claude` / `manual`。
        #[arg(long, default_value = "manual")]
        source: String,
        /// 会话标识。
        /// 默认 `manual`，Hook 场景下通常会被 `auto` 替换为 adapter 解析出的真实 session。
        #[arg(long, default_value = "manual")]
        session: String,
        /// 覆盖默认 TTL，单位秒。
        #[arg(long)]
        ttl: Option<u64>,
        /// 失败时静默降级，不在标准输出打印 warning。
        #[arg(long)]
        quiet: bool,
        /// 失败时返回非零退出码，用于需要“严格失败”的调用方。
        #[arg(long)]
        strict: bool,
        /// 安装器写入的稳定标识，用于重复安装去重与卸载。
        #[arg(long, default_value = "agent-status-light")]
        hook_id: String,
    },
    /// 查询 daemon 当前状态。
    Status {
        #[arg(long)]
        verbose: bool,
    },
    /// 查看最近日志。
    Logs {
        /// 最多返回多少条日志，命令层还会做范围裁剪。
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 优雅停止 daemon。
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// 为指定 Agent 安装 Hook。
    Install {
        /// 安装目标，当前支持 codex / cursor / claude。
        /// 这里故意保持为字符串，新增 Agent 时无需回头改 CLI 枚举。
        target: String,
        /// 指定项目目录时安装为项目级 Hook；不指定则安装为全局 Hook。
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// 为指定 Agent 卸载 Hook。
    Uninstall {
        target: String,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}
