use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// 项目内部统一错误类型。
///
/// 技术方案要求 adapter 错误不要把第三方库类型泄漏到核心层，
/// 因此这里统一收敛成稳定的 `code + message` 结构，便于：
/// 1. IPC 返回稳定错误码。
/// 2. CLI 以 JSON 形式输出失败原因。
/// 3. 单元测试直接断言错误语义，而不是断言某个库的错误文本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppError {
    /// 稳定错误码。
    ///
    /// 这个字段给 CLI、IPC 调用方和测试断言使用，
    /// 要尽量保持稳定，不要直接暴露第三方库原始错误分类。
    pub code: String,
    /// 面向人的错误说明。
    ///
    /// 它可以包含上下文细节，例如具体文件路径、系统错误文本或失败阶段，
    /// 便于用户在日志和命令输出里直接理解问题。
    pub message: String,
}

impl AppError {
    /// 直接构造项目内标准错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 基于 IO 错误构造统一错误对象。
    ///
    /// `context` 负责描述“是在做什么时失败”，
    /// 原始错误文本则作为补充细节拼进 `message`。
    pub fn io(context: &str, err: impl Display) -> Self {
        Self::new("io_error", format!("{context}: {err}"))
    }

    /// 基于输入解析或格式校验错误构造统一错误对象。
    pub fn invalid(context: &str, err: impl Display) -> Self {
        Self::new("invalid_input", format!("{context}: {err}"))
    }

    /// 构造“不支持当前操作或环境”的统一错误。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message.into())
    }
}

impl std::error::Error for AppError {}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// 项目内部统一结果别名。
///
/// 让核心层、端口层和适配器层都围绕同一错误模型协作，
/// 避免每层各自定义 `Result<T, XxxError>` 造成心智负担。
pub type AppResult<T> = Result<T, AppError>;

/// 电脑端和固件之间约定的模式字符串。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// 默认展示态；没有任何高优先级状态时用于演示灯效。
    Demo,
    /// AI 正在思考、规划、分析。
    Thinking,
    /// AI 正在读写文件、生成内容、处理上下文。
    Ai,
    /// AI 正在执行命令、外部工具或其它“忙碌但非内容生成”的工作。
    Busy,
    /// 当前轮次或会话成功结束。
    Success,
    /// 当前轮次或会话进入失败态。
    Error,
    /// 当前流程阻塞，等待用户输入、授权或确认。
    Alarm,
    /// 固件的交通灯展示模式。
    Traffic,
    /// 全部灯灭。
    Off,
    /// 手动红灯常亮。
    Red,
    /// 手动黄灯常亮。
    Yellow,
    /// 手动绿灯常亮。
    Green,
}

impl Mode {
    /// 返回与固件协议一致的稳定字符串值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Demo => "demo",
            Self::Thinking => "thinking",
            Self::Ai => "ai",
            Self::Busy => "busy",
            Self::Success => "success",
            Self::Error => "error",
            Self::Alarm => "alarm",
            Self::Traffic => "traffic",
            Self::Off => "off",
            Self::Red => "red",
            Self::Yellow => "yellow",
            Self::Green => "green",
        }
    }

    /// 返回该模式在全局路由中的优先级。
    ///
    /// 数字越大，表示当多个来源同时存在时，这个模式越应该成为最终展示态。
    pub fn priority(self) -> u8 {
        match self {
            Self::Alarm => 110,
            Self::Error => 100,
            Self::Yellow => 90,
            Self::Busy => 80,
            Self::Ai => 70,
            Self::Thinking => 60,
            Self::Success => 50,
            Self::Red => 40,
            Self::Green => 30,
            Self::Demo => 20,
            Self::Traffic => 10,
            Self::Off => 0,
        }
    }

    /// 返回该模式的默认存活时间。
    ///
    /// TTL 规则收敛在核心层，目的是：
    /// 1. 不让不同 adapter 私自决定状态能保留多久；
    /// 2. 让 router 对所有来源使用同一套过期规则；
    /// 3. 让测试可以直接围绕模式语义断言过期行为。
    pub fn default_ttl(self) -> Option<Duration> {
        let secs = match self {
            Self::Alarm => 30 * 60,
            Self::Busy | Self::Ai | Self::Thinking => 15 * 60,
            Self::Error => 10 * 60,
            Self::Success => 30,
            Self::Demo | Self::Traffic => 20 * 60,
            Self::Red | Self::Yellow | Self::Green => 5 * 60,
            Self::Off => return None,
        };
        Some(Duration::from_secs(secs))
    }
}

impl Display for Mode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = AppError;

    /// 将外部字符串解析为稳定的模式枚举。
    ///
    /// CLI、测试和未来潜在的 JSON 输入都复用这套解析规则，
    /// 保证大小写、空白处理和错误码保持一致。
    fn from_str(s: &str) -> AppResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "demo" => Ok(Self::Demo),
            "thinking" => Ok(Self::Thinking),
            "ai" => Ok(Self::Ai),
            "busy" => Ok(Self::Busy),
            "success" => Ok(Self::Success),
            "error" => Ok(Self::Error),
            "alarm" => Ok(Self::Alarm),
            "traffic" => Ok(Self::Traffic),
            "off" => Ok(Self::Off),
            "red" => Ok(Self::Red),
            "yellow" => Ok(Self::Yellow),
            "green" => Ok(Self::Green),
            other => Err(AppError::new(
                "invalid_mode",
                format!("unsupported mode: {other}"),
            )),
        }
    }
}

/// 统一能力枚举是整个方案的稳定核心。
/// 新增 Agent 时只需要把原始 Hook 事件归一到这个枚举，不需要改 router/daemon。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentCapability {
    /// Agent 正在思考、规划、压缩上下文或组织后续动作。
    Thinking,
    /// Agent 正在读取文件、编辑文件、生成文本或处理内容。
    Generating,
    /// Agent 正在执行命令、调用外部工具或运行系统操作。
    RunningCommand,
    /// Agent 暂停在需要用户介入的节点。
    WaitingForUser,
    /// 当前动作或会话成功结束。
    Succeeded,
    /// 当前动作或会话失败。
    Failed,
    /// Agent 存在但当前不处于活跃处理态。
    Idle,
    /// 当前来源无法被稳定识别时的兜底能力。
    Unknown,
}

/// 统一来源标识。
///
/// 这里单独包一层是为了把“任意字符串”与“已经归一后的来源名”区分开，
/// 后续如果要扩展更多来源元信息，也可以在不破坏主流程的前提下演进。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSource(pub String);

impl AgentSource {
    /// 以任意可转字符串值构造来源对象。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl Display for AgentSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct HookParseContext {
    /// 当前 `esp send` 指定的来源名。
    ///
    /// source adapter registry 会用它选择具体解析器。
    pub source: String,
    /// 命令行显式传入的 mode。
    ///
    /// 对 Hook 场景来说它通常只是兜底值，最终模式还要经过 `resolve_mode`。
    pub explicit_mode: Mode,
    /// 当前 CLI 进程工作目录。
    ///
    /// 它会参与 session fallback 生成，也会作为 `cwd` 的兜底来源。
    pub current_dir: PathBuf,
}

/// `SourceAdapter` 的输出结构。
/// 这里面只保留后续路由和排障需要的稳定字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    /// 归一后的来源信息。
    pub source: AgentSource,
    /// 当前事件所属会话标识。
    pub session: String,
    /// 归一后的能力语义。
    pub capability: AgentCapability,
    /// source adapter 给出的建议 mode。
    ///
    /// 如果是 `None`，说明该来源事件没有足够语义，需要后续走核心映射或命令兜底。
    pub suggested_mode: Option<Mode>,
    /// 事件关联的工作目录。
    pub cwd: Option<PathBuf>,
    /// 宿主工具原始 Hook 事件名。
    pub raw_event: Option<String>,
    /// 宿主工具原始工具名。
    pub raw_tool: Option<String>,
    /// 当前轮次 / 工具调用 / generation 的稳定标识。
    pub turn: Option<String>,
    /// 由 source adapter 提炼出的稳定语义，供核心路由层决策。
    pub semantics: EventSemantics,
}

/// 从宿主 Hook 事件中提炼出的稳定语义。
///
/// router 只关心这类语义，不应再直接依赖宿主私有字符串。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventSemantics {
    /// 泛 continuation 事件，没有明确工具边界。
    Continuation,
    /// 明确的 shell / tool 执行事件。
    ExplicitToolExecution,
    /// 明确的文件读取事件。
    FileRead,
    /// 明确的文件编辑 / 写入事件。
    FileWrite,
    /// 结束或成功收尾事件。
    Completion,
    /// 失败事件。
    Failure,
    /// 需要用户介入的事件。
    UserAttention,
    /// 无法稳定归类的兜底语义。
    Unknown,
}

/// router 内部保存的“单个 source + session 当前状态”。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceState {
    /// 来源工具名。
    pub source: String,
    /// 会话标识。
    pub session: String,
    /// 当前状态模式。
    pub mode: Mode,
    /// 产生该状态的原始事件名。
    pub raw_event: Option<String>,
    /// 产生该状态的原始工具名。
    pub raw_tool: Option<String>,
    /// 产生该状态的轮次标识。
    pub turn: Option<String>,
    /// 该状态对应的统一能力。
    pub capability: Option<AgentCapability>,
    /// source adapter 曾建议的 mode。
    pub suggested_mode: Option<Mode>,
    /// 该模式在全局路由中的优先级缓存值。
    pub priority: u8,
    /// 最近一次写入这条状态的时间。
    pub updated_at: DateTime<Utc>,
    /// 这条状态的过期时间；为空表示不过期。
    pub expires_at: Option<DateTime<Utc>>,
    /// 该状态对应事件的稳定语义。
    pub semantics: EventSemantics,
}

/// `status --verbose` 输出中的单条来源明细。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSourceEntry {
    /// 来源工具名。
    pub source: String,
    /// 会话标识。
    pub session: String,
    /// 当前保存的模式。
    pub mode: Mode,
    /// 原始事件名。
    pub raw_event: Option<String>,
    /// 原始工具名。
    pub raw_tool: Option<String>,
    /// 原始 turn 标识。
    pub turn: Option<String>,
    /// 统一能力语义。
    pub capability: Option<AgentCapability>,
    /// source adapter 给出的建议模式。
    pub suggested_mode: Option<Mode>,
    /// 该状态对应事件的稳定语义。
    pub semantics: EventSemantics,
    /// 该来源状态在全局路由中的优先级。
    pub priority: u8,
    /// 距离过期还剩多少秒；为空表示不过期。
    pub expires_in: Option<i64>,
}

/// `esp status` 命令返回的标准状态快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    /// daemon 运行状态，例如 `running` / `stopped`。
    pub daemon: String,
    /// BLE 连接状态，例如 `connected` / `disconnected`。
    pub ble: String,
    /// 当前连接设备名称。
    pub device: Option<String>,
    /// 设备最近一次写入或缓存的模式。
    pub mode: Mode,
    /// router 当前计算出的全局有效模式。
    pub effective: Mode,
    /// verbose 模式下返回所有来源明细；普通模式可为空。
    pub sources: Option<Vec<StatusSourceEntry>>,
    /// runtime 根目录路径，便于用户排障。
    pub runtime_dir: Option<String>,
    /// 当前 IPC 传输类型，例如 `unix_socket` / `named_pipe`。
    pub ipc: Option<String>,
    /// 最近一次成功写入 BLE 的时间。
    pub last_ble_write_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 设备的人类可读名称，优先取 BLE 广播名。
    pub name: String,
    /// 设备的稳定标识。
    /// 当前由 BLE peripheral id 序列化得到，主要用于状态输出和排障。
    pub id: String,
}

/// `status --verbose` 中用于展示 BLE 连接健康度的快照。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceHealth {
    /// 当前是否已建立可用连接。
    pub connected: bool,
    /// 最近一次识别出的设备名。
    pub device_name: Option<String>,
    /// 最近一次 BLE 层错误，便于用户快速排障。
    pub last_error: Option<String>,
    /// 最近一次成功写入模式的时间。
    pub last_write_at: Option<DateTime<Utc>>,
    /// 最近一次成功写入的模式。
    pub last_mode: Option<Mode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    /// 日志写入时间。
    ///
    /// 内部一律继续使用 `Utc` 保存，保证状态计算、TTL 和测试比较逻辑不受宿主机时区影响；
    /// 但对外序列化到 JSONL 时会统一转换为北京时间字符串，降低人工排障时的换算成本。
    #[serde(with = "crate::model::beijing_time")]
    pub timestamp: DateTime<Utc>,
    /// 日志等级，目前主要是 `info` / `warn`。
    pub level: String,
    /// 日志类别，例如 daemon / ble / ipc_send。
    pub kind: String,
    /// 面向人的简短描述。
    pub message: String,
    /// 链路阶段，便于快速定位“当前走到哪一步”。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// 稳定错误码，方便脚本或测试断言。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 如果日志与某个 source 相关，则记录来源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 如果日志与某个会话相关，则记录 session。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 如果日志与某个最终 mode 相关，则记录 mode。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// 结构化上下文，承载 hook、tool、turn、request_id、流程节点等关键排障信息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

/// 运行链路日志事件。
///
/// 与直接面向用户的 `LogEvent` 不同，这个结构用于调用点以借用字段的方式
/// 描述一次 runtime 节点记录，再统一转换成可落盘的 `LogEvent`。
#[derive(Debug, Clone)]
pub struct RuntimeLogEvent<'a> {
    /// 日志事件类型，用于分类和筛选。
    pub kind: &'a str,
    /// 日志事件阶段，用于定位处理路径。
    pub phase: &'a str,
    /// 日志事件消息，用于面向人类排障。
    pub message: &'a str,
    /// 稳定错误码；存在时会把日志级别提升为 `warn`。
    pub code: Option<&'a str>,
    /// 日志事件来源。
    pub source: Option<&'a str>,
    /// 日志事件会话。
    pub session: Option<&'a str>,
    /// 日志事件模式。
    pub mode: Option<Mode>,
    /// 结构化补充信息，会写入最终 `LogEvent.context` 字段。
    pub context: Option<Value>,
}

impl RuntimeLogEvent<'_> {
    /// 转换成用于实际落盘的拥有型 `LogEvent`。
    pub fn into_log_event(self) -> LogEvent {
        LogEvent {
            timestamp: Utc::now(),
            level: if self.code.is_some() {
                "warn".into()
            } else {
                "info".into()
            },
            kind: self.kind.into(),
            message: self.message.into(),
            phase: Some(self.phase.into()),
            code: self.code.map(ToOwned::to_owned),
            source: self.source.map(ToOwned::to_owned),
            session: self.session.map(ToOwned::to_owned),
            mode: self.mode,
            context: self.context,
        }
    }
}

/// `DateTime<Utc>` 与“日志对外展示用北京时间字符串”之间的桥接序列化模块。
///
/// 设计上我们刻意不把核心模型字段直接改成 `FixedOffset` 或 `Local`，原因是：
/// 1. router / daemon / TTL 计算更适合围绕 UTC 保持稳定；
/// 2. 测试环境、CI 机器和用户机器的本地时区可能不同，不适合把业务逻辑绑到宿主时区；
/// 3. 真正需要北京时间的主要是 JSONL 日志的人类可读输出，而不是内部计算。
///
/// 因此这里采用“内部 UTC，序列化时转 +08:00，反序列化再转回 UTC”的折中方案。
pub(crate) mod beijing_time {
    use chrono::{DateTime, FixedOffset, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// 北京时间固定 UTC+8 偏移量，单位为秒。
    const BEIJING_OFFSET: i32 = 8 * 3600;

    /// 返回北京时间的固定时区对象。
    ///
    /// 使用固定偏移而非 `Local`，是为了避免受宿主机器时区设置影响。
    fn beijing_offset() -> FixedOffset {
        FixedOffset::east_opt(BEIJING_OFFSET).expect("valid beijing offset")
    }

    /// 将内部 UTC 时间编码成带 `+08:00` 的 RFC3339 字符串。
    pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .with_timezone(&beijing_offset())
            .to_rfc3339()
            .serialize(serializer)
    }

    /// 将日志里的 RFC3339 时间字符串重新解析回 UTC。
    ///
    /// 这样 `EventLog::tail()` 读回来的依旧是 UTC 语义，
    /// 上层调用方不需要为“日志文件是北京时间”再额外做时区分支。
    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&value)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

/// daemon 启动后写入 runtime 目录的 IPC 元信息。
/// 命令层通过它了解当前使用的传输类型和地址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcInfo {
    /// IPC 类型，例如 `unix_socket` / `named_pipe`。
    pub kind: String,
    /// 当前 IPC 地址或路径。
    pub address: String,
    /// IPC 协议版本。
    pub version: u8,
    /// daemon 启动时间。
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCommand {
    /// 被写入 Hook 配置中的稳定可执行文件路径。
    pub exe: PathBuf,
    /// 可执行文件参数列表。
    pub args: Vec<String>,
}

/// 安装器内部用于描述“一条需要写入宿主工具的 Hook 规则”。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// 目标工具名称。
    pub target: String,
    /// Hook 事件名，保持宿主工具原生命名。
    pub event: String,
    /// 可选 matcher，例如 Bash / Edit / Write。
    pub matcher: Option<String>,
    /// 宿主工具未提供足够上下文时，命令行侧的兜底 mode。
    pub fallback_mode: Mode,
    /// 该 Hook 对应状态的 TTL。
    pub ttl: Duration,
    /// 最终要写进配置文件的命令。
    pub command: HookCommand,
}

/// Hook 配置的安装范围。
#[derive(Debug, Clone)]
pub enum InstallScope {
    /// 安装到当前用户的全局配置目录。
    Global,
    /// 安装到某个项目目录下的本地配置。
    Project(PathBuf),
}

/// 安装完成后写入 runtime 的清单文件。
/// 用于用户查看实际落盘位置，也为后续排障预留依据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallManifest {
    /// 安装目标名，例如 `codex` / `cursor` / `claude`。
    pub target: String,
    /// 本次安装完成时间。
    pub installed_at: DateTime<Utc>,
    /// 实际写入的配置文件路径。
    pub config_path: String,
    /// Hook 命令最终引用的可执行路径。
    pub command_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPayload {
    /// 本次请求希望生效的最终模式。
    pub mode: Mode,
    /// 来源工具名。
    pub source: String,
    /// 来源会话。
    pub session: String,
    /// TTL 秒数；为空则由 mode 默认 TTL 决定。
    pub ttl: Option<u64>,
    /// 安装器写入的稳定 Hook 标识。
    pub hook_id: Option<String>,
    /// 原始 Hook 事件名。
    pub raw_event: Option<String>,
    /// 原始工具名，例如 Bash / Edit / Write。
    pub raw_tool: Option<String>,
    /// 统一后的能力枚举，供路由和排障使用。
    pub capability: Option<AgentCapability>,
    /// source adapter 建议的 mode。
    pub suggested_mode: Option<Mode>,
    /// 当前工作目录，字符串化后便于 IPC 传输。
    pub cwd: Option<String>,
    /// 当前 turn 标识。
    pub turn: Option<String>,
    /// 由 source adapter 提炼出的稳定语义。
    pub semantics: EventSemantics,
}

/// IPC 请求体。
/// 目前只暴露发送状态、查询状态和停止 daemon 三类能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequestPayload {
    /// 发送一条状态更新请求。
    Send(SendPayload),
    /// 查询 daemon 当前状态。
    Status { verbose: bool },
    /// 请求 daemon 优雅停止。
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequestEnvelope {
    /// IPC 协议版本，方便以后兼容扩展。
    pub version: u8,
    /// 请求唯一标识，用于关联响应。
    pub request_id: String,
    /// 预留鉴权字段，当前阶段未启用。
    pub auth: Option<String>,
    /// 具体请求体。
    pub payload: IpcRequestPayload,
}

impl IpcRequestEnvelope {
    /// 基于给定 payload 创建标准 IPC 请求 envelope。
    pub fn new(payload: IpcRequestPayload) -> Self {
        // 每次请求都生成全局唯一 request_id，
        // 这样客户端、daemon 和日志将来都能串联同一次请求。
        Self {
            version: 1,
            request_id: Uuid::new_v4().to_string(),
            auth: None,
            payload,
        }
    }
}

/// IPC 响应 envelope。
///
/// 它与请求 envelope 成对出现，负责把 daemon 的处理结果包装成稳定协议，
/// 供 CLI、测试和潜在外部调用方统一消费。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponseEnvelope {
    /// IPC 协议版本。
    pub version: u8,
    /// 与请求对应的 request_id。
    pub request_id: String,
    /// 是否成功。
    pub ok: bool,
    /// 面向人的简短响应信息。
    pub message: String,
    /// 失败时返回的稳定错误码。
    ///
    /// 成功响应通常为空；失败响应应尽量提供可稳定断言的 code。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 附加结构化响应数据。
    ///
    /// 例如 `status` 返回的完整状态快照，或 `send` 返回的 effective mode。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl IpcResponseEnvelope {
    /// 构造一个成功响应。
    pub fn ok(request_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: 1,
            request_id: request_id.into(),
            ok: true,
            message: message.into(),
            code: None,
            data: None,
        }
    }

    /// 为成功响应追加结构化数据。
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        // 为响应追加结构化数据。
        // 这里采用 builder 风格，避免创建多个近似构造函数。
        self.data = Some(data);
        self
    }

    /// 基于统一错误对象构造失败响应。
    pub fn error(request_id: impl Into<String>, err: &AppError) -> Self {
        Self {
            version: 1,
            request_id: request_id.into(),
            ok: false,
            message: err.message.clone(),
            code: Some(err.code.clone()),
            data: None,
        }
    }
}
