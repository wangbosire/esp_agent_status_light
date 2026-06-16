use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::model::Mode;

const CLI_ABOUT: &str = "AgentStatusLight CLI";
const CLI_LONG_ABOUT: &str = "\
AgentStatusLight 的命令行工具。

用于启动后台 daemon、手动发送灯效状态、查看当前运行状态、读取日志，
以及为 Codex / Cursor / Claude 安装或卸载 Hook。

如果你是首次使用，建议按下面顺序操作：
1. `esp daemon --foreground` 或直接执行一次 `esp send --mode demo`
2. `esp status --verbose` 确认 daemon 与设备状态
3. `esp install codex` / `esp install cursor` / `esp install claude`
4. 用 `esp logs --limit 100` 排查 Hook 与蓝牙链路问题";
const CLI_AFTER_HELP: &str = "\
示例:
  esp send --mode demo
  esp send --mode busy --source cursor --session auto --quiet
  esp status --verbose
  esp logs --limit 50
  esp install codex
  esp installations
  esp installations cursor
  esp install cursor --dir /path/to/project
  esp uninstall claude";
const SEND_AFTER_HELP: &str = "\
模式说明:
  demo       演示模式
  thinking   AI 正在思考、分析、规划
  ai         AI 正在生成内容或读写上下文
  busy       AI 正在执行命令或外部工具
  success    当前任务成功完成
  error      当前任务失败
  alarm      等待用户确认、授权或介入
  traffic    交通灯展示模式
  off        关闭灯光
  red        红灯常亮测试
  yellow     黄灯常亮测试
  green      绿灯常亮测试

示例:
  esp send --mode demo
  esp send --mode off
  esp send --mode busy --source cursor --session auto --quiet
  esp send --mode error --source claude --session auto --strict
  printf '{\"hook_event_name\":\"session.completed\"}' | esp send --mode success --source codex --session auto";
const INSTALL_AFTER_HELP: &str = "\
支持目标:
  codex      安装 Codex Hook
  cursor     安装 Cursor Hook
  claude     安装 Claude Hook

示例:
  esp install codex
  esp install cursor --dir /path/to/project
  esp install claude --dir .";
const UNINSTALL_AFTER_HELP: &str = "\
支持目标:
  codex      卸载 Codex Hook
  cursor     卸载 Cursor Hook
  claude     卸载 Claude Hook

示例:
  esp uninstall codex
  esp uninstall cursor --dir /path/to/project
  esp uninstall claude --dir .";
const BLE_AFTER_HELP: &str = "\
示例:
  esp ble config
  esp ble config --name AgentStatusLight --service-uuid b8b7e001-7a6b-4f4f-9a8b-11c0ffee0001 --mode-char-uuid b8b7e002-7a6b-4f4f-9a8b-11c0ffee0001
  esp ble scan
  esp ble scan --duration 10
  esp ble test
  esp ble test --mode green";

/// CLI 只表达用户意图，不放业务规则。
/// 具体逻辑在 `command.rs` 中完成。
#[derive(Debug, Parser)]
#[command(
    name = "esp",
    version,
    about = CLI_ABOUT,
    long_about = CLI_LONG_ABOUT,
    after_help = CLI_AFTER_HELP
)]
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
    #[command(
        about = "启动后台 daemon",
        long_about = "\
启动 AgentStatusLight 后台服务。

daemon 负责维护 IPC 服务、管理状态路由、持有 BLE 设备连接，并把最终灯效写入硬件。
大多数 `send` 命令在发现 daemon 未运行时，会自动尝试拉起它。

默认行为是请求平台适配器在后台启动并立即返回；如果你正在排障，
可以使用 `--foreground` 让 daemon 挂在当前终端，直接观察日志输出。"
    )]
    Daemon {
        /// 以前台方式运行 daemon，适合排障或开发调试。
        #[arg(long)]
        foreground: bool,
    },
    /// 发送一个状态事件给 daemon。
    #[command(
        about = "发送灯效状态给 daemon",
        long_about = "\
向本地 daemon 发送一个状态事件，让状态灯切换到指定模式。

这个命令既可以手动调用，也可以被 Codex / Cursor / Claude 的 Hook 自动调用。
手动调用时通常只需要 `--mode`；Hook 场景下则会配合 `--source`、`--session auto`
以及标准输入中的事件 JSON 一起决定最终模式和会话归属。

默认语义偏向“尽量不打断主流程”：
- 普通失败会降级成 warning 或静默
- `--quiet` 可完全静默
- `--strict` 才会在失败时返回非零退出码",
        after_help = SEND_AFTER_HELP
    )]
    Send {
        /// 用户显式传入的 mode。
        /// 对 Hook 模式来说，它只是兜底值；是否真的采用，要再经过 `resolve_mode`。
        #[arg(long, help = "要发送的灯效模式，例如 demo / busy / success / off")]
        mode: Mode,
        /// 事件来源，例如 `codex` / `cursor` / `claude` / `manual`。
        #[arg(
            long,
            default_value = "manual",
            help = "事件来源；手动执行时通常保持 manual，Hook 调用时填 codex / cursor / claude"
        )]
        source: String,
        /// 会话标识。
        /// 默认 `manual`，Hook 场景下通常会被 `auto` 替换为 adapter 解析出的真实 session。
        #[arg(
            long,
            default_value = "manual",
            help = "会话标识；Hook 场景建议使用 auto，让适配器自动解析真实 session"
        )]
        session: String,
        /// 覆盖默认 TTL，单位秒。
        #[arg(long, help = "覆盖默认状态存活时间，单位秒")]
        ttl: Option<u64>,
        /// 失败时静默降级，不在标准输出打印 warning。
        #[arg(long, help = "失败时静默退出，不输出 warning 文本")]
        quiet: bool,
        /// 失败时返回非零退出码，用于需要“严格失败”的调用方。
        #[arg(long, help = "失败时返回非零退出码，而不是降级为 warning 或静默")]
        strict: bool,
        /// 安装器写入的稳定标识，用于重复安装去重与卸载。
        #[arg(
            long,
            default_value = "agent-status-light",
            help = "Hook 稳定标识，用于安装去重和卸载匹配"
        )]
        hook_id: String,
    },
    /// 查询 daemon 当前状态。
    #[command(
        about = "查询 daemon 当前状态",
        long_about = "\
查询后台 daemon 的当前状态。

当 daemon 正常运行时，会返回当前蓝牙连接、有效模式、IPC 信息等；
如果 daemon 不可用，则返回一个稳定的 stopped 结构，方便脚本和人工统一判断。

排障时建议优先使用 `esp status --verbose`。"
    )]
    Status {
        /// 输出更完整的状态字段，适合排障。
        #[arg(long)]
        verbose: bool,
    },
    /// 查看最近日志。
    #[command(
        about = "查看最近日志",
        long_about = "\
读取本地运行日志。

日志用于排查 Hook 是否触发、daemon 是否收到事件、状态路由是否正确、
以及 BLE 写入过程中是否出现错误。该命令直接读取本地 JSONL 日志文件，
不依赖 daemon 在线。"
    )]
    Logs {
        /// 最多返回多少条日志，命令层还会做范围裁剪。
        #[arg(long, default_value_t = 100, help = "最多返回多少条日志记录")]
        limit: usize,
    },
    /// 查看 Hook 安装信息。
    #[command(
        about = "查看 Hook 安装信息",
        long_about = "\
查看 AgentStatusLight 当前记录的 Hook 安装信息。

不指定目标时默认列出全部已支持 Agent；指定目标时只查询该 Agent。
该命令读取 runtime 安装清单，不依赖 daemon 在线。",
        after_help = "\
示例:
  esp installations
  esp installations cursor"
    )]
    Installations {
        /// 可选安装目标；不传则查询全部目标。
        #[arg(help = "安装目标：codex / cursor / claude；不传则查询全部")]
        target: Option<String>,
    },
    /// 优雅停止 daemon。
    #[command(
        about = "停止后台 daemon",
        long_about = "\
停止 AgentStatusLight 后台 daemon。

默认会优先通过 IPC 请求 daemon 自行优雅退出。
如果 daemon 已失联但 pid 仍残留，可以额外传入 `--force`，
尝试按 runtime 中记录的 pid 做兜底终止。"
    )]
    Stop {
        /// 当 IPC 不可用时，按 pid 尝试强制终止 daemon。
        #[arg(long)]
        force: bool,
    },
    /// 配置、扫描与测试 BLE 设备。
    #[command(
        about = "配置、扫描与测试 BLE 设备",
        long_about = "\
管理 AgentStatusLight 使用的 BLE 设备配置，并直接排查蓝牙链路。

`config` 会读写 runtime 中的 BLE 配置；daemon、scan 和 test 都共享同一份配置。
`scan` 用当前配置标记匹配到的设备，便于确认名称或服务 UUID 是否正确。
`test` 会独立连接设备，不依赖 daemon 在线。",
        after_help = BLE_AFTER_HELP
    )]
    Ble {
        /// BLE 子命令。
        #[command(subcommand)]
        command: BleCommands,
    },
    /// 为指定 Agent 安装 Hook。
    #[command(
        about = "为指定 Agent 安装 Hook",
        long_about = "\
向目标 AI 工具安装 Hook，使其在关键事件发生时自动调用 `esp send`。

当前支持 `codex`、`cursor`、`claude`。
不指定 `--dir` 时执行全局安装；指定 `--dir` 后执行项目级安装，
适合只在某个仓库内启用联动。",
        after_help = INSTALL_AFTER_HELP
    )]
    Install {
        /// 安装目标，当前支持 codex / cursor / claude。
        /// 这里故意保持为字符串，新增 Agent 时无需回头改 CLI 枚举。
        #[arg(help = "安装目标：codex / cursor / claude")]
        target: String,
        /// 指定项目目录时安装为项目级 Hook；不指定则安装为全局 Hook。
        #[arg(long, help = "项目目录；传入后安装为项目级 Hook，不传则安装到全局配置")]
        dir: Option<PathBuf>,
    },
    /// 为指定 Agent 卸载 Hook。
    #[command(
        about = "为指定 Agent 卸载 Hook",
        long_about = "\
从目标 AI 工具中移除由 AgentStatusLight 安装的 Hook。

卸载逻辑会优先使用稳定的 `--hook-id` 标识定位本工具写入的命令，
避免误删用户自己的其它 Hook。`--dir` 的语义与安装命令一致：
不传时处理全局配置，传入时处理项目级配置。",
        after_help = UNINSTALL_AFTER_HELP
    )]
    Uninstall {
        #[arg(help = "卸载目标：codex / cursor / claude")]
        target: String,
        #[arg(long, help = "项目目录；传入后卸载项目级 Hook，不传则卸载全局配置")]
        dir: Option<PathBuf>,
    },
}

/// BLE 设备管理子命令。
#[derive(Debug, Subcommand)]
pub enum BleCommands {
    /// 查看或更新 BLE 设备配置。
    #[command(
        alias = "configure",
        about = "查看或更新 BLE 设备配置",
        long_about = "\
查看或更新 BLE 设备配置。

不传任何参数时只输出当前配置；传入任一参数时会保存更新后的配置。
设备名用于按广播名称匹配，服务 UUID 用于按 GATT 服务匹配，mode 特征 UUID 用于写入灯效模式。"
    )]
    Config {
        /// 目标 BLE 设备广播名。
        #[arg(long, alias = "device-name", help = "目标 BLE 设备广播名")]
        name: Option<String>,
        /// 固件暴露的 GATT 服务 UUID。
        #[arg(long, help = "固件暴露的 GATT 服务 UUID")]
        service_uuid: Option<String>,
        /// 固件暴露的 mode 特征 UUID。
        #[arg(long, help = "固件暴露的 mode 特征 UUID")]
        mode_char_uuid: Option<String>,
        /// 重置为默认 BLE 配置。
        #[arg(long, help = "重置为默认 BLE 配置")]
        reset: bool,
    },
    /// 扫描附近 BLE 设备。
    #[command(about = "扫描附近 BLE 设备")]
    Scan {
        /// 扫描时长，单位秒。
        #[arg(long, default_value_t = 6, help = "扫描时长，单位秒")]
        duration: u64,
    },
    /// 独立测试 BLE 连接。
    #[command(about = "独立测试 BLE 连接")]
    Test {
        /// 连接成功后可选写入一个测试模式。
        #[arg(long, help = "连接成功后可选写入一个测试模式，例如 green / off")]
        mode: Option<Mode>,
    },
}

// CLI 测试放在 tests 目录，避免帮助文案校验与命令定义混写在一起。
#[cfg(test)]
#[path = "../tests/core/cli_tests.rs"]
mod tests;
