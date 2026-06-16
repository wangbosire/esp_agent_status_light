//! CLI 命令分发与应用装配模块。
//!
//! 这一层负责：
//! 1. 解析 CLI 参数后选择正确命令路径；
//! 2. 装配 platform/runtime/ipc/install/source 等 adapter；
//! 3. 把 Hook stdin、安装配置、daemon 自启动等边缘逻辑串起来。

mod boot;
mod install;
mod io;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};

use crate::adapters;
use crate::cli::{BleCommands, Cli, Commands};
use crate::daemon::Daemon;
use crate::model::{
    AgentCapability, AgentEvent, AgentSource, AppError, AppResult, BleDeviceConfig, EventSemantics,
    HookParseContext, InstallManifest, InstallScope, IpcRequestEnvelope, IpcRequestPayload, Mode,
    RuntimeLogEvent, SendPayload, StatusResponse,
};
use crate::ports::device::LightDevice;
use crate::ports::hook_install::HookInstallRegistry;
use crate::ports::ipc::{IpcServer, IpcTransport};
use crate::ports::log::EventLog;
use crate::ports::platform::PlatformAdapter;
use crate::ports::runtime::RuntimeStore;
use crate::ports::source::{FallbackReason, SourceAdapterRegistry};
use crate::router::resolve_mode;
use boot::{ensure_daemon_running, force_stop_by_pid, request_with_auto_start};
use install::{backup_if_exists, read_json_or_empty, resolve_install_command, write_json};
#[cfg(test)]
use install::{build_cargo_run_hook_command, should_use_cargo_run_hooks};
use io::read_stdin_json;

pub enum CommandOutput {
    /// 输出 JSON，供脚本调用与人类查看共用。
    Json(Value),
    /// 输出普通文本。
    Text(String),
    /// 不输出任何内容，常用于 Hook 静默执行路径。
    Silent,
}

/// `send` 命令在进入核心逻辑前归一后的参数视图。
///
/// 单独抽出这个结构而不是在 `run_send()` 里直接传一长串参数，有两个目的：
/// 1. 让命令分发层到实现层的边界更清晰；
/// 2. 后续扩展 `send` 选项时，不必持续膨胀函数签名。
struct SendCommandArgs {
    /// CLI 上显式传入的 mode。
    /// 对手动调用来说通常就是最终模式；对 Hook 调用来说，它只是 source adapter 无法识别事件时的兜底值。
    explicit_mode: Mode,
    /// 事件来源名，例如 `manual` / `codex` / `cursor` / `claude`。
    /// 命令层用它选择对应的 SourceAdapter，并最终写入 SendPayload 供 daemon 路由。
    source: String,
    /// CLI 上传入的 session 参数。
    /// 值为 `auto` 时，后续会替换成 SourceAdapter 从 Hook stdin 中解析出的真实 session。
    session: String,
    /// 显式覆盖状态存活时间，单位秒。
    /// 为空时由 router 使用 mode 默认 TTL，避免每个 source adapter 自己决定过期策略。
    ttl: Option<u64>,
    /// Hook 静默模式。
    /// 非 strict 失败时不输出 warning，保证灯效链路不会打扰宿主 Agent 主流程。
    quiet: bool,
    /// 严格失败模式。
    /// 只有开启后，send/IPC/BLE 链路错误才会以非零退出码返回给调用方。
    strict: bool,
    /// 安装器写入的稳定 Hook 标识。
    /// daemon 会把它记录进 payload/log，安装与卸载也依赖同一标识做去重和匹配。
    hook_id: String,
}

/// 每次 CLI 调用时临时构建的应用上下文。
/// 它只持有轻量级适配器和注册表，不在这里保存业务状态。
struct AppContext {
    /// Hook 来源解析器注册表。
    source_registry: SourceAdapterRegistry,
    /// Hook 安装器注册表。
    install_registry: HookInstallRegistry,
    /// runtime 文件存储适配器。
    runtime: Arc<dyn RuntimeStore>,
    /// 事件与运行链路日志适配器。
    log: Arc<dyn EventLog>,
    /// 当前平台差异适配器。
    platform: Box<dyn PlatformAdapter>,
}

impl AppContext {
    /// 构建一套默认 CLI 运行所需的适配器集合。
    fn build() -> AppResult<Self> {
        // 所有默认 adapter 装配集中在这里，避免散落在各个命令实现中。
        let platform = adapters::platform::current_platform();
        let runtime_root = platform.runtime_root()?;
        let runtime: Arc<dyn RuntimeStore> =
            Arc::new(adapters::runtime::fs::FsRuntimeAdapter::new(runtime_root));
        let log: Arc<dyn EventLog> =
            Arc::new(adapters::log::jsonl::JsonlLogAdapter::new(runtime.clone()));
        Ok(Self {
            source_registry: adapters::source::registry(),
            install_registry: adapters::install::registry(),
            runtime,
            log,
            platform,
        })
    }

    /// 创建一个面向 daemon 的 IPC 客户端。
    fn ipc_client(&self) -> Box<dyn IpcTransport> {
        // IPC client 每次按平台动态创建，避免跨平台时把传输细节写死。
        self.platform
            .default_ipc_adapter(&self.runtime.default_ipc_path())
    }

    /// 创建当前平台默认的 IPC server。
    ///
    /// daemon 进程启动时使用；CLI 普通命令不会直接监听服务。
    fn ipc_server(&self) -> Arc<dyn IpcServer> {
        #[cfg(unix)]
        {
            Arc::new(adapters::ipc::unix_socket::UnixSocketServer::new(
                self.runtime.default_ipc_path(),
            ))
        }
        #[cfg(not(unix))]
        {
            Arc::new(adapters::ipc::named_pipe::NamedPipeServer::new(
                self.runtime.default_ipc_path(),
            ))
        }
    }

    /// 创建默认物理设备适配器。
    fn device(&self) -> AppResult<Box<dyn LightDevice>> {
        // 当前正式设备只有 BLE 实现；测试场景会直接绕过这里注入 mock。
        Ok(Box::new(
            adapters::device::btleplug_ble::BtleplugBleAdapter::with_config(
                self.runtime.read_ble_config()?,
            ),
        ))
    }
}

/// CLI 总入口。
///
/// 该函数负责把解析后的命令枚举分派到具体处理函数，
/// 并为每次命令调用重新装配一套轻量上下文。
pub async fn run(cli: Cli) -> AppResult<CommandOutput> {
    // 每次命令调用都重新构建上下文即可，避免在 CLI 进程内维护多余全局状态。
    let ctx = AppContext::build()?;

    match cli.command {
        Commands::Daemon { foreground } => run_daemon(ctx, foreground).await,
        Commands::Send {
            mode,
            source,
            session,
            ttl,
            quiet,
            strict,
            hook_id,
        } => {
            run_send(
                ctx,
                SendCommandArgs {
                    explicit_mode: mode,
                    source,
                    session,
                    ttl,
                    quiet,
                    strict,
                    hook_id,
                },
            )
            .await
        }
        Commands::Status { verbose } => run_status(ctx, verbose).await,
        Commands::Logs { limit } => run_logs(ctx, limit).await,
        Commands::Installations { target } => run_installations(ctx, target).await,
        Commands::Stop { force } => run_stop(ctx, force).await,
        Commands::Ble { command } => run_ble(ctx, command).await,
        Commands::Install { target, dir } => run_install(ctx, target, dir).await,
        Commands::Uninstall { target, dir } => run_uninstall(ctx, target, dir).await,
    }
}

/// 以“尽力写入”方式追加一条运行链路日志。
///
/// 运行链路日志用于定位内部处理路径，不应反过来影响主命令行为。
fn append_runtime_log(log: &dyn EventLog, event: RuntimeLogEvent<'_>) {
    // runtime 日志只用于排查链路问题，因此这里采用“尽力写入”策略：
    // 即使日志失败，也绝不反过来阻塞主功能。
    let _ = log.append_runtime(event.into_log_event());
}

async fn run_daemon(ctx: AppContext, foreground: bool) -> AppResult<CommandOutput> {
    // `esp daemon` 同时承担“启动后台服务”和“真正运行 daemon 主循环”两种职责；
    // 是否进入主循环完全由 `foreground` 决定。
    if !foreground {
        // 非前台模式只负责拉起后台 daemon，自身立即退出。
        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "command",
                phase: "daemon.background_requested",
                message: "daemon command requested background startup",
                code: None,
                source: None,
                session: None,
                mode: None,
                context: None,
            },
        );
        let exe =
            std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;
        ctx.platform.spawn_background_daemon(&exe)?;
        return Ok(CommandOutput::Json(json!({
            "ok": true,
            "daemon": "starting",
            "runtime_dir": ctx.runtime.runtime_root(),
        })));
    }

    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "command",
            phase: "daemon.foreground_requested",
            message: "daemon command entering foreground run loop",
            code: None,
            source: None,
            session: None,
            mode: None,
            context: None,
        },
    );
    let daemon = Daemon::new(ctx.runtime.clone(), ctx.log.clone(), ctx.device()?);
    daemon.run(ctx.ipc_server()).await?;
    Ok(CommandOutput::Silent)
}

async fn run_send(ctx: AppContext, args: SendCommandArgs) -> AppResult<CommandOutput> {
    // 当前工作目录既用于 fallback session 生成，也用于状态排障输出。
    let current_dir =
        std::env::current_dir().map_err(|err| AppError::io("read current dir", err))?;
    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "send",
            phase: "send.received",
            message: "send command started",
            code: None,
            source: Some(&args.source),
            session: None,
            mode: Some(args.explicit_mode),
            context: Some(json!({
                "session_arg": args.session,
                "explicit_mode": args.explicit_mode,
                "ttl_secs": args.ttl,
                "strict": args.strict,
                "quiet": args.quiet,
                "hook_id": args.hook_id,
                "cwd": current_dir.clone(),
            })),
        },
    );
    let ctx_parse = HookParseContext {
        source: args.source.clone(),
        explicit_mode: args.explicit_mode,
        current_dir: current_dir.clone(),
    };

    // 手动模式不依赖 Hook stdin。
    // Hook 模式才会读取 stdin 并交给 SourceAdapterRegistry 归一。
    let event = if args.source == "manual" {
        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "send",
                phase: "send.manual_bypass",
                message: "manual source bypassed hook stdin parsing",
                code: None,
                source: Some("manual"),
                session: Some("manual"),
                mode: Some(args.explicit_mode),
                context: Some(json!({
                    "source": "manual",
                    "reason": "manual_source",
                })),
            },
        );
        AgentEvent {
            source: AgentSource::new("manual"),
            session: "manual".into(),
            capability: AgentCapability::Unknown,
            suggested_mode: None,
            cwd: Some(current_dir.clone()),
            raw_event: None,
            raw_tool: None,
            turn: None,
            semantics: EventSemantics::Unknown,
        }
    } else {
        let stdin_json = read_stdin_json()?.unwrap_or_else(|| json!({}));
        append_runtime_log(
            ctx.log.as_ref(),
            RuntimeLogEvent {
                kind: "send",
                phase: "send.stdin_loaded",
                message: "hook stdin loaded",
                code: None,
                source: Some(&args.source),
                session: None,
                mode: Some(args.explicit_mode),
                context: Some(json!({
                    "has_hook_event": stdin_json
                        .get("hook_event_name")
                        .or_else(|| stdin_json.get("hookEventName"))
                        .is_some(),
                    "hook_event": stdin_json
                        .get("hook_event_name")
                        .or_else(|| stdin_json.get("hookEventName")),
                    "tool_name": stdin_json
                        .get("tool_name")
                        .or_else(|| stdin_json.get("toolName"))
                        .or_else(|| stdin_json.get("tool")),
                    "turn": stdin_json
                        .get("turn")
                        .or_else(|| stdin_json.get("turn_id"))
                        .or_else(|| stdin_json.get("turnId")),
                })),
            },
        );
        let parsed = ctx
            .source_registry
            .parse_or_fallback_with_reason(stdin_json, &ctx_parse);
        if let Some(reason) = &parsed.fallback_reason {
            let (code, message, context) = match reason {
                FallbackReason::SourceMissing(source) => (
                    "source_adapter_missing",
                    "source adapter missing, using fallback parser",
                    json!({ "source": source }),
                ),
                FallbackReason::ParseFailed(err) => (
                    err.code.as_str(),
                    "source adapter parse failed, using fallback parser",
                    json!({
                        "error_code": err.code,
                        "error_message": err.message,
                    }),
                ),
            };
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "send",
                    phase: "send.hook_parse_fallback",
                    message,
                    code: Some(code),
                    source: Some(&args.source),
                    session: None,
                    mode: Some(args.explicit_mode),
                    context: Some(context),
                },
            );
        }
        parsed.event
    };
    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "send",
            phase: "send.hook_normalized",
            message: "hook event normalized",
            code: None,
            source: Some(&args.source),
            session: Some(&event.session),
            mode: event.suggested_mode,
            context: Some(json!({
                "normalized_source": event.source.0,
                "raw_event": event.raw_event,
                "raw_tool": event.raw_tool,
                "capability": format!("{:?}", event.capability),
                "suggested_mode": event.suggested_mode,
                "turn": event.turn,
                "event_session": event.session,
                "event_cwd": event.cwd,
            })),
        },
    );

    // 最终 mode 的决策顺序必须固定：
    // manual -> explicit off -> suggested_mode -> capability 映射 -> explicit_mode 兜底。
    // 这样来源 adapter 只负责“尽量识别语义”，而不是各自决定最终灯效。
    let resolved_mode = resolve_mode(&ctx_parse, &event);
    let resolved_session = if args.session == "auto" {
        event.session.clone()
    } else {
        args.session
    };
    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "send",
            phase: "send.mode_resolved",
            message: "mode resolved",
            code: None,
            source: Some(&args.source),
            session: Some(&resolved_session),
            mode: Some(resolved_mode),
            context: Some(json!({
                "resolved_mode": resolved_mode,
                "resolved_session": resolved_session,
                "explicit_mode": args.explicit_mode,
                "suggested_mode": event.suggested_mode,
                "capability": format!("{:?}", event.capability),
                "raw_event": event.raw_event,
            })),
        },
    );

    let payload = SendPayload {
        mode: resolved_mode,
        source: args.source.clone(),
        session: resolved_session.clone(),
        ttl: args.ttl,
        hook_id: Some(args.hook_id),
        raw_event: event.raw_event.clone(),
        raw_tool: event.raw_tool.clone(),
        capability: Some(event.capability.clone()),
        suggested_mode: event.suggested_mode,
        cwd: event
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().to_string()),
        turn: event.turn.clone(),
        semantics: event.semantics,
    };

    let request = IpcRequestEnvelope::new(IpcRequestPayload::Send(payload));
    append_runtime_log(
        ctx.log.as_ref(),
        RuntimeLogEvent {
            kind: "send",
            phase: "send.ipc_dispatch",
            message: "dispatching ipc send request",
            code: None,
            source: Some(&args.source),
            session: Some(&resolved_session),
            mode: Some(resolved_mode),
            context: Some(json!({
                "request_id": request.request_id,
                "payload": request.payload,
            })),
        },
    );
    match request_with_auto_start(&ctx, request).await {
        Ok(response) if response.ok => {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "send",
                    phase: "send.completed",
                    message: "ipc send request completed successfully",
                    code: None,
                    source: Some(&args.source),
                    session: Some(&resolved_session),
                    mode: Some(resolved_mode),
                    context: Some(json!({
                        "response_message": response.message,
                        "response_data": response.data,
                    })),
                },
            );
            Ok(CommandOutput::Silent)
        }
        Ok(response) => {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "send",
                    phase: "send.application_error",
                    message: "ipc send request returned application error",
                    code: response.code.as_deref(),
                    source: Some(&args.source),
                    session: Some(&resolved_session),
                    mode: Some(resolved_mode),
                    context: Some(json!({
                        "response_code": response.code,
                        "response_message": response.message,
                        "response_data": response.data,
                    })),
                },
            );
            let err = AppError::new(
                response.code.unwrap_or_else(|| "ipc_request_failed".into()),
                response.message,
            );
            handle_send_failure(err, args.quiet, args.strict)
        }
        Err(err) => {
            append_runtime_log(
                ctx.log.as_ref(),
                RuntimeLogEvent {
                    kind: "send",
                    phase: "send.transport_error",
                    message: "ipc transport failed",
                    code: Some(&err.code),
                    source: Some(&args.source),
                    session: Some(&resolved_session),
                    mode: Some(resolved_mode),
                    context: Some(json!({
                        "error_code": err.code,
                        "error_message": err.message,
                    })),
                },
            );
            handle_send_failure(err, args.quiet, args.strict)
        }
    }
}

/// 统一处理 `send` 命令失败时的降级策略。
///
/// 默认 Hook 语义是“失败不阻断主流程”，因此除非显式启用 `--strict`，
/// 否则这里会把错误降级为静默或 warning 文本。
fn handle_send_failure(err: AppError, quiet: bool, strict: bool) -> AppResult<CommandOutput> {
    // Hook 默认是“失败不阻塞主流程”，只有 `--strict` 才把错误向上抛出。
    // 这样灯效链路的问题不会轻易反向影响用户真正要执行的任务。
    if strict {
        return Err(err);
    }

    if quiet {
        Ok(CommandOutput::Silent)
    } else {
        Ok(CommandOutput::Text(format!(
            "warning: {}: {}",
            err.code, err.message
        )))
    }
}

async fn run_status(ctx: AppContext, verbose: bool) -> AppResult<CommandOutput> {
    // status 优先读取 daemon 实时状态；
    // 如果 daemon 不在，则退回一个稳定的“stopped”响应结构，方便脚本和人类统一消费。
    let request = IpcRequestEnvelope::new(IpcRequestPayload::Status { verbose });
    match ctx.ipc_client().request(request).await {
        Ok(response) if response.ok => Ok(CommandOutput::Json(
            response.data.unwrap_or_else(|| json!({})),
        )),
        // daemon 不可用时返回一个稳定的“stopped”结构，而不是直接报错，
        // 这样脚本和用户都能用统一格式感知状态。
        _ => Ok(CommandOutput::Json(json!(StatusResponse {
            daemon: "stopped".into(),
            ble: "disconnected".into(),
            device: None,
            mode: Mode::Off,
            effective: Mode::Off,
            sources: None,
            runtime_dir: Some(ctx.runtime.runtime_root().to_string_lossy().to_string()),
            ipc: None,
            last_ble_write_at: None,
        }))),
    }
}

async fn run_logs(ctx: AppContext, limit: usize) -> AppResult<CommandOutput> {
    // logs 不走 daemon，直接读本地 JSONL 文件即可。
    // 这样即便 daemon 已退出，用户仍能查看最近一轮事件事实。
    let items = ctx.log.tail(limit)?;
    Ok(CommandOutput::Json(serde_json::to_value(items).map_err(
        |err| AppError::invalid("serialize logs output", err),
    )?))
}

async fn run_installations(ctx: AppContext, target: Option<String>) -> AppResult<CommandOutput> {
    ctx.runtime.ensure_layout()?;
    let targets = if let Some(target) = target {
        // 复用 registry 的目标校验，保证未知 target 的错误码和 install/uninstall 一致。
        ctx.install_registry.get(&target)?;
        vec![target]
    } else {
        ctx.install_registry.targets()
    };

    let mut items = Vec::with_capacity(targets.len());
    for target in targets {
        let manifest_path = ctx.runtime.install_manifest_path(&target);
        let manifest = ctx.runtime.read_install_manifest(&target)?;
        let installations = manifest
            .as_ref()
            .map(|index| index.installations.clone())
            .unwrap_or_default();
        items.push(json!({
            "target": target,
            "installed": !installations.is_empty(),
            "manifest_path": manifest_path,
            "installations": installations,
        }));
    }

    Ok(CommandOutput::Json(json!({
        "ok": true,
        "runtime_root": ctx.runtime.runtime_root(),
        "targets": items,
    })))
}

async fn run_stop(ctx: AppContext, force: bool) -> AppResult<CommandOutput> {
    // stop 先尝试优雅的 daemon IPC；
    // 只有显式 `--force` 时才允许按 pid 做最后兜底。
    let request = IpcRequestEnvelope::new(IpcRequestPayload::Stop);
    match ctx.ipc_client().request(request).await {
        Ok(response) if response.ok => Ok(CommandOutput::Json(json!({
            "ok": true,
            "message": response.message,
        }))),
        Ok(response) => Err(AppError::new(
            response.code.unwrap_or_else(|| "daemon_stop_failed".into()),
            response.message,
        )),
        Err(_err) if force => {
            // `--force` 仅在 IPC 失联时作为最后兜底手段，直接按 pid 发终止信号。
            force_stop_by_pid(ctx.runtime.as_ref())?;
            Ok(CommandOutput::Json(json!({
                "ok": true,
                "message": "force stop signal sent",
            })))
        }
        Err(err) => Err(err),
    }
}

async fn run_ble(ctx: AppContext, command: BleCommands) -> AppResult<CommandOutput> {
    match command {
        BleCommands::Config {
            name,
            service_uuid,
            mode_char_uuid,
            reset,
        } => run_ble_config(ctx, name, service_uuid, mode_char_uuid, reset).await,
        BleCommands::Scan { duration } => run_ble_scan(ctx, duration).await,
        BleCommands::Test { mode } => run_ble_test(ctx, mode).await,
    }
}

async fn run_ble_config(
    ctx: AppContext,
    name: Option<String>,
    service_uuid: Option<String>,
    mode_char_uuid: Option<String>,
    reset: bool,
) -> AppResult<CommandOutput> {
    let has_update = reset || name.is_some() || service_uuid.is_some() || mode_char_uuid.is_some();
    let config = if reset {
        BleDeviceConfig::default()
    } else {
        ctx.runtime
            .read_ble_config()?
            .with_updates(name, service_uuid, mode_char_uuid)?
    };

    if has_update {
        ctx.runtime.write_ble_config(&config)?;
    }

    Ok(CommandOutput::Json(json!({
        "ok": true,
        "updated": has_update,
        "config_path": ctx.runtime.ble_config_path(),
        "config": config,
    })))
}

async fn run_ble_scan(ctx: AppContext, duration: u64) -> AppResult<CommandOutput> {
    let duration = validate_ble_scan_duration(duration)?;
    let config = ctx.runtime.read_ble_config()?;
    let devices =
        adapters::device::btleplug_ble::BtleplugBleAdapter::scan_nearby(&config, duration).await?;
    Ok(CommandOutput::Json(json!({
        "ok": true,
        "duration_secs": duration.as_secs(),
        "config": config,
        "count": devices.len(),
        "devices": devices,
    })))
}

async fn run_ble_test(ctx: AppContext, mode: Option<Mode>) -> AppResult<CommandOutput> {
    let config = ctx.runtime.read_ble_config()?;
    let mut device =
        adapters::device::btleplug_ble::BtleplugBleAdapter::with_config(config.clone());
    let info = device.connect().await?;
    if let Some(mode) = mode {
        device.write_mode(mode).await?;
    }
    let health = device.health().await;
    device.disconnect().await?;

    Ok(CommandOutput::Json(json!({
        "ok": true,
        "config": config,
        "device": info,
        "tested_mode": mode,
        "health_before_disconnect": health,
    })))
}

fn validate_ble_scan_duration(duration: u64) -> AppResult<Duration> {
    if !(1..=60).contains(&duration) {
        return Err(AppError::new(
            "invalid_ble_scan_duration",
            "scan duration must be between 1 and 60 seconds",
        ));
    }
    Ok(Duration::from_secs(duration))
}

async fn run_install(
    ctx: AppContext,
    target: String,
    dir: Option<PathBuf>,
) -> AppResult<CommandOutput> {
    ctx.runtime.ensure_layout()?;
    let adapter = ctx.install_registry.get(&target)?;
    let scope = dir
        .map(InstallScope::Project)
        .unwrap_or(InstallScope::Global);
    let config_path = adapter.config_path(&scope)?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|err| AppError::io("create target config dir", err))?;
    }

    // 正式 release 版本继续写入 runtime/bin 下的稳定副本；
    // 但开发阶段如果是通过 `cargo run` 运行，希望 Hook 也继续走 `cargo run -- ...`，
    // 这样改完源码重新编译后，安装好的 Hook 会自动跟随当前工程，而不是绑定旧二进制副本。
    let install_command = resolve_install_command(ctx.runtime.as_ref())?;
    let mut specs = adapter.hook_specs(install_command.spec_exe());
    install_command.apply_to_specs(&mut specs);
    let config = read_json_or_empty(&config_path)?;
    // 写入前先备份用户配置，避免 JSON 合并异常时没有回退点。
    backup_if_exists(&config_path)?;
    let updated = adapter.install(config, &specs, "agent-status-light", ctx.platform.as_ref())?;
    write_json(&config_path, &updated)?;

    let manifest = InstallManifest {
        target: target.clone(),
        installed_at: Utc::now(),
        config_path: config_path.to_string_lossy().to_string(),
        command_path: install_command.display_command(),
    };
    ctx.runtime.write_install_manifest(&manifest)?;
    let manifest_index = ctx.runtime.read_install_manifest(&target)?;
    let installation_count = manifest_index
        .as_ref()
        .map(|index| index.installations.len())
        .unwrap_or(0);

    let _ = ensure_daemon_running(&ctx).await;

    Ok(CommandOutput::Json(json!({
        "ok": true,
        "target": target,
        "config_path": config_path,
        "command_path": install_command.display_command(),
        "manifest_path": ctx.runtime.install_manifest_path(&manifest.target),
        "manifest_installations": installation_count,
        "runtime_root": ctx.runtime.runtime_root(),
    })))
}

async fn run_uninstall(
    ctx: AppContext,
    target: String,
    dir: Option<PathBuf>,
) -> AppResult<CommandOutput> {
    // 卸载也保留备份，避免误删后用户无法恢复自己的配置。
    let adapter = ctx.install_registry.get(&target)?;
    let scope = dir
        .map(InstallScope::Project)
        .unwrap_or(InstallScope::Global);
    let config_path = adapter.config_path(&scope)?;
    if !config_path.exists() {
        return Ok(CommandOutput::Json(json!({
            "ok": true,
            "target": target,
            "config_path": config_path,
            "changed": false,
        })));
    }
    let config = read_json_or_empty(&config_path)?;
    backup_if_exists(&config_path)?;
    let updated = adapter.uninstall(config, "agent-status-light")?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|err| AppError::io("create target config dir", err))?;
    }
    write_json(&config_path, &updated)?;
    let config_path_raw = config_path.to_string_lossy().to_string();
    ctx.runtime
        .remove_install_manifest(&target, &config_path_raw)?;
    Ok(CommandOutput::Json(json!({
        "ok": true,
        "target": target,
        "config_path": config_path,
        "manifest_path": ctx.runtime.install_manifest_path(&target),
    })))
}

// 测试实现拆到独立目录，避免与 CLI 主流程装配逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../tests/core/command_tests.rs"]
mod tests;
