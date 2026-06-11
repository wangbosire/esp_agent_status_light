use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
    pub code: String,
    pub message: String,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn io(context: &str, err: impl Display) -> Self {
        Self::new("io_error", format!("{context}: {err}"))
    }

    pub fn invalid(context: &str, err: impl Display) -> Self {
        Self::new("invalid_input", format!("{context}: {err}"))
    }

    #[allow(dead_code)]
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

pub type AppResult<T> = Result<T, AppError>;

/// 电脑端和固件之间约定的模式字符串。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Demo,
    Thinking,
    Ai,
    Busy,
    Success,
    Error,
    Alarm,
    Traffic,
    Off,
    Red,
    Yellow,
    Green,
}

impl Mode {
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

    /// 这里直接编码技术方案中的优先级表。
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

    /// 默认 TTL 也必须稳定落在核心层，不能散在 adapter 中。
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
    Thinking,
    Generating,
    RunningCommand,
    WaitingForUser,
    Succeeded,
    Failed,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSource(pub String);

impl AgentSource {
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
    pub source: String,
    pub explicit_mode: Mode,
    pub current_dir: PathBuf,
    #[allow(dead_code)]
    pub ttl: Option<Duration>,
}

/// `SourceAdapter` 的输出结构。
/// 这里面只保留后续路由和排障需要的稳定字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub source: AgentSource,
    pub session: String,
    pub capability: AgentCapability,
    pub suggested_mode: Option<Mode>,
    pub cwd: Option<PathBuf>,
    pub raw_event: Option<String>,
    pub raw_tool: Option<String>,
    pub turn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceState {
    pub source: String,
    pub session: String,
    pub mode: Mode,
    pub raw_event: Option<String>,
    pub raw_tool: Option<String>,
    pub turn: Option<String>,
    pub capability: Option<AgentCapability>,
    pub suggested_mode: Option<Mode>,
    pub priority: u8,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSourceEntry {
    pub source: String,
    pub session: String,
    pub mode: Mode,
    pub raw_event: Option<String>,
    pub raw_tool: Option<String>,
    pub turn: Option<String>,
    pub capability: Option<AgentCapability>,
    pub suggested_mode: Option<Mode>,
    pub priority: u8,
    pub expires_in: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub daemon: String,
    pub ble: String,
    pub device: Option<String>,
    pub mode: Mode,
    pub effective: Mode,
    pub sources: Option<Vec<StatusSourceEntry>>,
    pub runtime_dir: Option<String>,
    pub ipc: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for DeviceHealth {
    fn default() -> Self {
        Self {
            connected: false,
            device_name: None,
            last_error: None,
            last_write_at: None,
            last_mode: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    /// 日志写入时间。
    pub timestamp: DateTime<Utc>,
    /// 日志等级，目前主要是 `info` / `warn`。
    pub level: String,
    /// 日志类别，例如 daemon / ble / ipc_send。
    pub kind: String,
    /// 面向人的简短描述。
    pub message: String,
    /// 稳定错误码，方便脚本或测试断言。
    pub code: Option<String>,
    /// 如果日志与某个 source 相关，则记录来源。
    pub source: Option<String>,
    /// 如果日志与某个会话相关，则记录 session。
    pub session: Option<String>,
    /// 如果日志与某个最终 mode 相关，则记录 mode。
    pub mode: Option<Mode>,
}

/// daemon 启动后写入 runtime 目录的 IPC 元信息。
/// 命令层通过它了解当前使用的传输类型和地址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcInfo {
    pub kind: String,
    pub address: String,
    pub version: u8,
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
    Global,
    Project(PathBuf),
}

/// 安装完成后写入 runtime 的清单文件。
/// 用于用户查看实际落盘位置，也为后续排障预留依据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallManifest {
    pub target: String,
    pub installed_at: DateTime<Utc>,
    pub config_path: String,
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
}

/// IPC 请求体。
/// 目前只暴露发送状态、查询状态和停止 daemon 三类能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequestPayload {
    Send(SendPayload),
    Status { verbose: bool },
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
    pub payload: IpcRequestPayload,
}

impl IpcRequestEnvelope {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponseEnvelope {
    pub version: u8,
    pub request_id: String,
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl IpcResponseEnvelope {
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

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        // 为响应追加结构化数据。
        // 这里采用 builder 风格，避免创建多个近似构造函数。
        self.data = Some(data);
        self
    }

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
