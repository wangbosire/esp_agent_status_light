//! CLI 命令分发与应用装配模块。
//!
//! 这一层负责：
//! 1. 解析 CLI 参数后选择正确命令路径；
//! 2. 装配 platform/runtime/ipc/install/source 等 adapter；
//! 3. 把 Hook stdin、安装配置、daemon 自启动等边缘逻辑串起来。

use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::adapters;
use crate::cli::{Cli, Commands};
use crate::daemon::Daemon;
use crate::model::{
    AgentCapability, AgentEvent, AgentSource, AppError, AppResult, HookCommand, HookParseContext,
    HookSpec, InstallManifest, InstallScope, IpcRequestEnvelope, IpcRequestPayload, Mode,
    SendPayload, StatusResponse,
};
use crate::ports::device::LightDevice;
use crate::ports::hook_install::HookInstallRegistry;
use crate::ports::ipc::{IpcServer, IpcTransport};
use crate::ports::log::EventLog;
use crate::ports::platform::PlatformAdapter;
use crate::ports::runtime::RuntimeStore;
use crate::ports::source::SourceAdapterRegistry;
use crate::router::resolve_mode;

pub enum CommandOutput {
    /// 输出 JSON，供脚本调用与人类查看共用。
    Json(Value),
    /// 输出普通文本。
    Text(String),
    /// 不输出任何内容，常用于 Hook 静默执行路径。
    Silent,
}

/// 每次 CLI 调用时临时构建的应用上下文。
/// 它只持有轻量级适配器和注册表，不在这里保存业务状态。
struct AppContext {
    source_registry: SourceAdapterRegistry,
    install_registry: HookInstallRegistry,
    runtime: Arc<dyn RuntimeStore>,
    log: Arc<dyn EventLog>,
    platform: Box<dyn PlatformAdapter>,
}

impl AppContext {
    fn build() -> Self {
        // 所有默认 adapter 装配集中在这里，避免散落在各个命令实现中。
        let platform = adapters::platform::current_platform();
        let runtime: Arc<dyn RuntimeStore> = Arc::new(
            adapters::runtime::fs::FsRuntimeAdapter::new(platform.runtime_root()),
        );
        let log: Arc<dyn EventLog> =
            Arc::new(adapters::log::jsonl::JsonlLogAdapter::new(runtime.clone()));
        Self {
            source_registry: adapters::source::registry(),
            install_registry: adapters::install::registry(),
            runtime,
            log,
            platform,
        }
    }

    fn ipc_client(&self) -> Box<dyn IpcTransport> {
        // IPC client 每次按平台动态创建，避免跨平台时把传输细节写死。
        self.platform
            .default_ipc_adapter(&self.runtime.default_ipc_path())
    }

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

    fn device(&self) -> Box<dyn LightDevice> {
        // 当前正式设备只有 BLE 实现；测试场景会直接绕过这里注入 mock。
        Box::new(adapters::device::btleplug_ble::BtleplugBleAdapter::default())
    }
}

pub async fn run(cli: Cli) -> AppResult<CommandOutput> {
    // 每次命令调用都重新构建上下文即可，避免在 CLI 进程内维护多余全局状态。
    let ctx = AppContext::build();

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
        } => run_send(ctx, mode, source, session, ttl, quiet, strict, hook_id).await,
        Commands::Status { verbose } => run_status(ctx, verbose).await,
        Commands::Logs { limit } => run_logs(ctx, limit).await,
        Commands::Stop { force } => run_stop(ctx, force).await,
        Commands::Install { target, dir } => run_install(ctx, target, dir).await,
        Commands::Uninstall { target, dir } => run_uninstall(ctx, target, dir).await,
    }
}

fn append_runtime_log(
    log: &dyn EventLog,
    kind: &str,
    message: &str,
    code: Option<&str>,
    source: Option<&str>,
    session: Option<&str>,
    mode: Option<Mode>,
) {
    // runtime 日志只用于排查链路问题，因此这里采用“尽力写入”策略：
    // 即使日志失败，也绝不反过来阻塞主功能。
    let _ = log.append_runtime(crate::model::LogEvent {
        timestamp: Utc::now(),
        level: if code.is_some() {
            "warn".into()
        } else {
            "info".into()
        },
        kind: kind.into(),
        message: message.into(),
        code: code.map(ToOwned::to_owned),
        source: source.map(ToOwned::to_owned),
        session: session.map(ToOwned::to_owned),
        mode,
    });
}

async fn run_daemon(ctx: AppContext, foreground: bool) -> AppResult<CommandOutput> {
    if !foreground {
        // 非前台模式只负责拉起后台 daemon，自身立即退出。
        append_runtime_log(
            ctx.log.as_ref(),
            "runtime_command",
            "daemon command requested background startup",
            None,
            None,
            None,
            None,
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
        "runtime_command",
        "daemon command entering foreground run loop",
        None,
        None,
        None,
        None,
    );
    let daemon = Daemon::new(ctx.runtime.clone(), ctx.log.clone(), ctx.device());
    daemon.run(ctx.ipc_server()).await?;
    Ok(CommandOutput::Silent)
}

async fn run_send(
    ctx: AppContext,
    explicit_mode: Mode,
    source: String,
    session: String,
    ttl: Option<u64>,
    quiet: bool,
    strict: bool,
    hook_id: String,
) -> AppResult<CommandOutput> {
    // 当前工作目录既用于 fallback session 生成，也用于状态排障输出。
    let current_dir =
        std::env::current_dir().map_err(|err| AppError::io("read current dir", err))?;
    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_send",
        &format!(
            "send command started: source={source}, session_arg={session}, explicit_mode={}",
            explicit_mode.as_str()
        ),
        None,
        Some(&source),
        None,
        Some(explicit_mode),
    );
    let ctx_parse = HookParseContext {
        source: source.clone(),
        explicit_mode,
        current_dir: current_dir.clone(),
        ttl: ttl.map(Duration::from_secs),
    };

    // 手动模式不依赖 Hook stdin。
    // Hook 模式才会读取 stdin 并交给 SourceAdapterRegistry 归一。
    let event = if source == "manual" {
        append_runtime_log(
            ctx.log.as_ref(),
            "runtime_send",
            "manual source bypassed hook stdin parsing",
            None,
            Some("manual"),
            Some("manual"),
            Some(explicit_mode),
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
        }
    } else {
        let stdin_json = read_stdin_json()?.unwrap_or_else(|| json!({}));
        append_runtime_log(
            ctx.log.as_ref(),
            "runtime_send",
            &format!(
                "hook stdin loaded for source={source}, has_hook_event={}",
                stdin_json
                    .get("hook_event_name")
                    .or_else(|| stdin_json.get("hookEventName"))
                    .is_some()
            ),
            None,
            Some(&source),
            None,
            Some(explicit_mode),
        );
        ctx.source_registry
            .parse_or_fallback(stdin_json, &ctx_parse)
    };
    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_send",
        &format!(
            "hook event normalized: raw_event={:?}, raw_tool={:?}, capability={:?}, suggested_mode={:?}, turn={:?}",
            event.raw_event, event.raw_tool, event.capability, event.suggested_mode, event.turn
        ),
        None,
        Some(&source),
        Some(&event.session),
        event.suggested_mode,
    );

    // 最终 mode 的决策顺序必须固定：
    // manual -> explicit off -> suggested_mode -> capability 映射 -> explicit_mode 兜底。
    let resolved_mode = resolve_mode(&ctx_parse, &event);
    let resolved_session = if session == "auto" {
        event.session.clone()
    } else {
        session
    };
    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_send",
        &format!(
            "mode resolved: session={resolved_session}, mode={}, raw_event={:?}",
            resolved_mode.as_str(),
            event.raw_event
        ),
        None,
        Some(&source),
        Some(&resolved_session),
        Some(resolved_mode),
    );

    let payload = SendPayload {
        mode: resolved_mode,
        source: source.clone(),
        session: resolved_session.clone(),
        ttl,
        hook_id: Some(hook_id),
        raw_event: event.raw_event.clone(),
        raw_tool: event.raw_tool.clone(),
        capability: Some(event.capability.clone()),
        suggested_mode: event.suggested_mode,
        cwd: event
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().to_string()),
        turn: event.turn.clone(),
    };

    let request = IpcRequestEnvelope::new(IpcRequestPayload::Send(payload));
    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_send",
        "dispatching ipc send request",
        None,
        Some(&source),
        Some(&resolved_session),
        Some(resolved_mode),
    );
    match request_with_auto_start(&ctx, request).await {
        Ok(response) if response.ok => {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_send",
                "ipc send request completed successfully",
                None,
                Some(&source),
                Some(&resolved_session),
                Some(resolved_mode),
            );
            Ok(CommandOutput::Silent)
        }
        Ok(response) => {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_send",
                &format!(
                    "ipc send request returned application error: code={:?}, message={}",
                    response.code, response.message
                ),
                response.code.as_deref(),
                Some(&source),
                Some(&resolved_session),
                Some(resolved_mode),
            );
            let err = AppError::new(
                response.code.unwrap_or_else(|| "ipc_request_failed".into()),
                response.message,
            );
            handle_send_failure(err, quiet, strict)
        }
        Err(err) => {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_send",
                &format!(
                    "ipc transport failed: code={}, message={}",
                    err.code, err.message
                ),
                Some(&err.code),
                Some(&source),
                Some(&resolved_session),
                Some(resolved_mode),
            );
            handle_send_failure(err, quiet, strict)
        }
    }
}

fn handle_send_failure(err: AppError, quiet: bool, strict: bool) -> AppResult<CommandOutput> {
    // Hook 默认是“失败不阻塞主流程”，只有 `--strict` 才把错误向上抛出。
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
    let items = ctx.log.tail(limit)?;
    Ok(CommandOutput::Json(serde_json::to_value(items).map_err(
        |err| AppError::invalid("serialize logs output", err),
    )?))
}

async fn run_stop(ctx: AppContext, force: bool) -> AppResult<CommandOutput> {
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
    let config_path = adapter.config_path(&scope);
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

    ctx.runtime.write_install_manifest(&InstallManifest {
        target: target.clone(),
        installed_at: Utc::now(),
        config_path: config_path.to_string_lossy().to_string(),
        command_path: install_command.display_command(),
    })?;

    let _ = ensure_daemon_running(&ctx).await;

    Ok(CommandOutput::Json(json!({
        "ok": true,
        "target": target,
        "config_path": config_path,
        "command_path": install_command.display_command(),
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
    let config_path = adapter.config_path(&scope);
    let config = read_json_or_empty(&config_path)?;
    if config_path.exists() {
        backup_if_exists(&config_path)?;
    }
    let updated = adapter.uninstall(config, "agent-status-light")?;
    write_json(&config_path, &updated)?;
    Ok(CommandOutput::Json(json!({
        "ok": true,
        "target": target,
        "config_path": config_path,
    })))
}

async fn request_with_auto_start(
    ctx: &AppContext,
    request: IpcRequestEnvelope,
) -> AppResult<crate::model::IpcResponseEnvelope> {
    match ctx.ipc_client().request(request.clone()).await {
        Ok(response) => {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_ipc",
                "initial ipc request succeeded",
                None,
                None,
                None,
                None,
            );
            Ok(response)
        }
        Err(_) => {
            // 首次请求失败时尝试自动拉起 daemon，符合“普通用户开箱即用”的目标。
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_ipc",
                "initial ipc request failed, attempting daemon auto-start",
                None,
                None,
                None,
                None,
            );
            ensure_daemon_running(ctx).await?;
            let retried = ctx.ipc_client().request(request).await;
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_ipc",
                if retried.is_ok() {
                    "retry ipc request after auto-start succeeded"
                } else {
                    "retry ipc request after auto-start failed"
                },
                None,
                None,
                None,
                None,
            );
            retried
        }
    }
}

async fn ensure_daemon_running(ctx: &AppContext) -> AppResult<()> {
    // 先做 pid 健康检查：如果已有 daemon 进程仍然活着，就不要重复拉起新实例。
    // 这能避免 IPC 短暂失败或启动窗口期导致多个 daemon 争抢设备与 runtime 文件。
    if let Some(pid) = ctx.runtime.read_pid()? {
        if process_is_alive(pid)? {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_daemon_boot",
                &format!("daemon health check found alive pid={pid}"),
                None,
                None,
                None,
                None,
            );
            return Ok(());
        }
        append_runtime_log(
            ctx.log.as_ref(),
            "runtime_daemon_boot",
            &format!("daemon health check found stale pid={pid}, clearing runtime markers"),
            None,
            None,
            None,
            None,
        );
        let _ = ctx.runtime.clear_pid();
        let _ = ctx.runtime.clear_ipc_info();
    }

    let exe = std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;
    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_daemon_boot",
        "spawning background daemon",
        None,
        None,
        None,
        None,
    );
    ctx.platform.spawn_background_daemon(&exe)?;

    for _ in 0..20 {
        sleep(Duration::from_millis(150)).await;
        let probe = IpcRequestEnvelope::new(IpcRequestPayload::Status { verbose: false });
        if ctx.ipc_client().request(probe).await.is_ok() {
            append_runtime_log(
                ctx.log.as_ref(),
                "runtime_daemon_boot",
                "daemon became ready after auto-start",
                None,
                None,
                None,
                None,
            );
            return Ok(());
        }
    }

    append_runtime_log(
        ctx.log.as_ref(),
        "runtime_daemon_boot",
        "daemon did not become ready after auto-start timeout window",
        Some("ipc_unavailable"),
        None,
        None,
        None,
    );
    Err(AppError::new(
        "ipc_unavailable",
        "daemon did not become ready after auto-start",
    ))
}

fn is_process_alive(pid: u32) -> AppResult<bool> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map_err(|err| AppError::io("check daemon pid with kill -0", err))?;
        return Ok(status.success());
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .map_err(|err| AppError::io("check daemon pid with tasklist", err))?;
        if !output.status.success() {
            return Err(AppError::new(
                "pid_check_failed",
                format!("tasklist returned status {}", output.status),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().any(|line| {
            line.contains(&format!(",\"{pid}\"")) || line.contains(&format!(",{pid},"))
        }));
    }
}

fn process_is_alive(pid: u32) -> AppResult<bool> {
    is_process_alive(pid)
}

/// install 命令最终要写入 Hook 配置中的命令来源。
///
/// - Release / 普通分发场景：继续使用 runtime/bin 下的稳定二进制副本。
/// - 本地开发 `cargo run` 场景：写成 `cargo run --manifest-path ... -- send ...`，
///   让 Hook 始终回到当前工程执行。
enum InstallCommandTarget {
    StableBinary { path: PathBuf },
    CargoRun { manifest_path: PathBuf },
}

impl InstallCommandTarget {
    fn spec_exe(&self) -> &Path {
        match self {
            Self::StableBinary { path } => path.as_path(),
            // 先让各安装器生成默认 `send ...` 参数，后面再统一改写为 cargo 前缀命令。
            Self::CargoRun { .. } => Path::new("cargo"),
        }
    }

    fn apply_to_specs(&self, specs: &mut [HookSpec]) {
        if let Self::CargoRun { manifest_path } = self {
            for spec in specs {
                spec.command = build_cargo_run_hook_command(manifest_path, &spec.command.args);
            }
        }
    }

    fn display_command(&self) -> String {
        match self {
            Self::StableBinary { path } => path.to_string_lossy().to_string(),
            Self::CargoRun { manifest_path } => format!(
                "cargo run --manifest-path {} --",
                manifest_path.to_string_lossy()
            ),
        }
    }
}

fn resolve_install_command(runtime: &dyn RuntimeStore) -> AppResult<InstallCommandTarget> {
    let current =
        std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;

    if should_use_cargo_run_hooks(&current) {
        return Ok(InstallCommandTarget::CargoRun {
            manifest_path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        });
    }

    Ok(InstallCommandTarget::StableBinary {
        path: install_stable_binary(runtime)?,
    })
}

fn should_use_cargo_run_hooks(current_exe: &Path) -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }

    let Some(parent) = current_exe.parent() else {
        return false;
    };
    let Some(grand_parent) = parent.parent() else {
        return false;
    };

    parent.file_name().is_some_and(|name| name == "debug")
        && grand_parent
            .file_name()
            .is_some_and(|name| name == "target")
}

fn build_cargo_run_hook_command(manifest_path: &Path, send_args: &[String]) -> HookCommand {
    let mut args = vec![
        "run".into(),
        "--manifest-path".into(),
        manifest_path.to_string_lossy().to_string(),
        "--".into(),
    ];
    args.extend(send_args.iter().cloned());
    HookCommand {
        exe: PathBuf::from("cargo"),
        args,
    }
}

fn install_stable_binary(runtime: &dyn RuntimeStore) -> AppResult<PathBuf> {
    runtime.ensure_layout()?;
    let current =
        std::env::current_exe().map_err(|err| AppError::io("resolve current exe", err))?;
    let file_name = if cfg!(windows) { "esp.exe" } else { "esp" };
    let target = runtime.bin_dir().join(file_name);
    // Hook 一律指向 runtime/bin 中的稳定副本，
    // 这样即使用户从源码目录切走，已安装的 Hook 仍然可用。
    fs::copy(&current, &target).map_err(|err| AppError::io("copy stable binary", err))?;
    Ok(target)
}

fn read_json_or_empty(path: &Path) -> AppResult<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = fs::read_to_string(path).map_err(|err| AppError::io("read config json", err))?;
    serde_json::from_str(&raw).map_err(|err| AppError::invalid("parse config json", err))
}

fn write_json(path: &Path, value: &Value) -> AppResult<()> {
    // 所有配置文件统一格式化写出，便于用户手动检查和 diff。
    let raw = serde_json::to_string_pretty(value)
        .map_err(|err| AppError::invalid("serialize json file", err))?;
    fs::write(path, raw).map_err(|err| AppError::io("write json file", err))
}

fn backup_if_exists(path: &Path) -> AppResult<()> {
    if !path.exists() {
        return Ok(());
    }
    // 备份文件名带时间戳，便于连续多次 install/uninstall 后追踪历史。
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let backup = path.with_extension(format!(
        "{}.bak.{timestamp}",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    fs::copy(path, backup).map_err(|err| AppError::io("backup config file", err))?;
    Ok(())
}

fn read_stdin_json() -> AppResult<Option<Value>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }

    // Hook stdin 只做短超时读取，避免某些 Agent 在没有可靠 EOF 时阻塞主流程。
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut raw = String::new();
        let result = stdin.lock().read_to_string(&mut raw).map(|_| raw);
        let _ = tx.send(result);
    });

    let raw = match rx.recv_timeout(Duration::from_millis(75)) {
        Ok(Ok(raw)) => raw,
        Ok(Err(err)) => return Err(AppError::io("read hook stdin", err)),
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
    };

    // 某些 Hook 在特定事件下可能不会写 stdin；
    // 这里返回 None，让 Fallback / explicit_mode 继续兜底。
    if raw.trim().is_empty() {
        return Ok(None);
    }

    // malformed JSON 也不应该阻断 Hook；这里降级为空对象，继续走 fallback 和显式 mode。
    match serde_json::from_str(raw.trim()) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(Some(json!({}))),
    }
}

fn force_stop_by_pid(runtime: &dyn RuntimeStore) -> AppResult<()> {
    let pid = runtime
        .read_pid()?
        .ok_or_else(|| AppError::new("pid_missing", "no daemon pid file found"))?;

    #[cfg(unix)]
    {
        // Unix 上优先发送 TERM，让 daemon 有机会正常收尾并灭灯。
        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .map_err(|err| AppError::io("execute kill", err))?;
        if !status.success() {
            return Err(AppError::new(
                "kill_failed",
                format!("kill returned status {status}"),
            ));
        }
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .map_err(|err| AppError::io("execute taskkill", err))?;
        if !status.success() {
            return Err(AppError::new(
                "kill_failed",
                format!("taskkill returned status {status}"),
            ));
        }
    }

    let _ = runtime.clear_pid();
    let _ = runtime.clear_ipc_info();
    Ok(())
}

// 测试实现拆到独立目录，避免与 CLI 主流程装配逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../tests/core/command_tests.rs"]
mod tests;
