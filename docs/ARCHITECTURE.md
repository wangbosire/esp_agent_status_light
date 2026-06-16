# AgentStatusLight Architecture

基于当前仓库代码实现整理的系统架构说明，覆盖：

- `esp send` 的 Hook 归一化与 IPC 投递链路
- `esp daemon` 的状态路由、TTL 清理、BLE 重连与设备写入链路
- `install` / `uninstall` 的配置写入、去重、备份与安装清单落盘关系

这份文档描述的是“当前实现”，不是未来态草图。对应核心代码主要位于：

- `src/command.rs`
- `src/router.rs`
- `src/daemon.rs`
- `src/adapters/source/*.rs`
- `src/adapters/install/*.rs`
- `src/adapters/runtime/fs.rs`

***

## 1. 总览

### 1.1 核心模块职责图

| 模块 | 当前职责 | 关键输入 | 关键输出 |
| --- | --- | --- | --- |
| `src/cli.rs` | 暴露 `daemon`、`send`、`status`、`logs`、`stop`、`ble`、`install`、`uninstall` 命令面 | 用户命令行参数 | 结构化 CLI 命令 |
| `src/command.rs` | 装配 runtime / log / platform / source registry / install registry；处理自动拉起 daemon；记录 Hook 原始 stdin；把 Hook stdin 归一后发往 IPC | CLI 参数、Hook stdin JSON | `SendPayload`、IPC 请求、状态输出 |
| `src/adapters/source/*.rs` | 按 `source` 解析 Codex / Cursor / Claude Hook JSON，抽取 `session`、`turn`、`semantics`、`capability`、`suggested_mode` | 宿主工具 Hook JSON | `AgentEvent` |
| `src/router.rs` | `resolve_mode` 与 `StateRouter`；按 `(source, session)` 维护状态池，处理 TTL、优先级、AI 保留规则 | `AgentEvent`、`SendPayload` | 当前 session 状态、全局 `effective mode` |
| `src/daemon.rs` | 独占 `LightDevice`；接收 IPC；调用 router；派发后台 BLE 同步；写前刷新最新 effective；维护 TTL 清理、断线重连和空闲自动停止后台任务 | IPC 请求、router 决策结果 | BLE 写入、`StatusResponse`、日志 |
| `src/adapters/install/*.rs` | 把统一 `HookSpec` 翻译成 Codex / Cursor / Claude 的官方配置结构 | 安装目标、配置 JSON、平台差异 | 更新后的 hooks/settings JSON |
| `src/adapters/runtime/fs.rs` / `src/runtime_lock.rs` | 统一管理 runtime 根目录、日志、IPC 信息、安装清单、稳定二进制副本路径和跨进程文件锁 | runtime root、锁 owner | `runtime/`、`bin/`、`config.<target>.json`、token 化 lock 文件 |

### 1.2 当前系统架构图

```mermaid
flowchart LR
    subgraph AgentSide["Agent / Hook 侧"]
        Agent["Codex / Cursor / Claude"]
        Hook["已安装 Hook 配置"]
        Agent --> Hook
    end

    subgraph SendProcess["单次 esp send 进程"]
        CLI["esp send"]
        Stdin["Hook stdin JSON"]
        HookInput["HookInput<br/>raw_input + parsed_json"]
        Registry["SourceAdapterRegistry"]
        Adapter["Codex / Cursor / Claude / Fallback Adapter"]
        Event["AgentEvent"]
        Resolver["resolve_mode()"]
        Payload["SendPayload"]
        IPCClient["IpcTransport Client"]

        Stdin --> HookInput
        HookInput --> Registry
        Registry --> Adapter
        Adapter --> Event
        Event --> Resolver
        Resolver --> Payload
        Payload --> IPCClient
    end

    subgraph DaemonProcess["常驻 esp daemon 进程"]
        IPCServer["IpcServer"]
        Daemon["Daemon"]
        Router["StateRouter"]
        TTL["expiry_loop()"]
        Reconnect["reconnect_loop()"]
        Idle["idle_shutdown_loop()"]
        Runtime["RuntimeStore"]
        EventLog["EventLog / JSONL"]
        DevicePort["LightDevice"]

        IPCServer --> Daemon
        Daemon --> Router
        TTL --> Router
        Reconnect --> DevicePort
        Idle --> Daemon
        Daemon --> Runtime
        Daemon --> EventLog
        Daemon --> DevicePort
    end

    subgraph Hardware["物理设备"]
        BLE["Btleplug BLE Adapter"]
        ESP32["ESP32-C3 BLE GATT"]
        Lamp["红 / 黄 / 绿灯效"]

        DevicePort --> BLE
        BLE --> ESP32
        ESP32 --> Lamp
    end

    Hook --> CLI
    Hook -. 注入 .-> Stdin
    HookInput -. raw_input / input_json .-> EventLog
    IPCClient --> IPCServer
    Runtime --> BleConfig["ble.json<br/>设备名 / UUID"]
    BleConfig --> BLE
```

### 1.3 用户使用总流程

```mermaid
flowchart TD
    User["用户拿到已烧录设备"] --> Power["USB 供电"]
    Power --> Esp32["ESP32-C3 广播 BLE<br/>名称：AgentStatusLight"]

    User --> Package["获取电脑端工具包<br/>macOS / Windows"]
    Package --> BleDiag["BLE 配置与排障<br/>ble config / scan / test"]
    BleDiag --> Test["手动测试<br/>send --mode demo / off"]
    Test --> Daemon["后台 daemon 自动启动"]
    Daemon --> Ble["保持 BLE 连接"]
    Ble --> Esp32

    Package --> Install["安装 Hook<br/>install codex / cursor / claude"]
    Install --> AgentConfig["写入目标工具配置<br/>Codex hooks.json<br/>Cursor hooks.json<br/>Claude settings.json"]
    AgentConfig --> Agent["Agent 工作中触发 Hook"]
    Agent --> Send["Hook 调用 send<br/>--source + --session auto<br/>--ttl + --quiet"]
    Send --> Ipc["本地 IPC 发送给 daemon"]
    Send --> RuntimeLog["runtime.log<br/>记录 Hook raw_input"]
    Ipc --> Router["状态优先级路由<br/>按 source/session 合并"]
    Router --> Mode["选择最高优先级 mode"]
    Mode --> Ble
    Daemon --> IdleStop["1 小时无事件<br/>自动停止"]
    IdleStop --> Send
    Esp32 --> Light["红 / 黄 / 绿状态灯展示"]

    User --> Status["排障命令<br/>ble scan/test<br/>status --verbose<br/>logs --limit 100"]
    Status --> Daemon
```

***

## 2. Hook 上报与状态路由

### 2.1 Hook 自动上报时序图

```mermaid
sequenceDiagram
    autonumber
    participant Agent as Agent / IDE
    participant Hook as Hook Command
    participant Send as esp send
    participant Registry as SourceAdapterRegistry
    participant Adapter as SourceAdapter
    participant Resolve as resolve_mode
    participant IPC as IPC Client
    participant Daemon as esp daemon
    participant Router as StateRouter
    participant Device as LightDevice / BLE

    Agent->>Hook: 触发 Hook 事件并传入 stdin JSON
    Hook->>Send: 执行 esp send --source X --session auto --ttl ... --quiet
    Send->>Send: runtime.log 记录 raw_input / input_json
    Send->>Registry: parse_or_fallback(stdin_json, HookParseContext)
    Registry->>Adapter: 按 source 选择 adapter
    Adapter-->>Registry: AgentEvent(session, turn, semantics, capability, suggested_mode)
    Registry-->>Send: AgentEvent
    Send->>Resolve: resolve_mode(ctx, event)
    Resolve-->>Send: resolved_mode
    Send->>Send: 组装 SendPayload(source, session, ttl, raw_event, turn, semantics)
    Send->>IPC: request(send)
    IPC->>Daemon: IpcRequestPayload::Send
    Daemon->>Router: apply_send(payload, now)
    Router-->>Daemon: effective_mode
    Daemon-->>IPC: accepted(queued=true)
    Daemon->>Device: 后台 sync_effective_mode(false)
    Daemon->>Device: health()
    alt disconnected 或 idle stale
        Daemon->>Device: connect()
    end
    Daemon->>Router: 写入前重新读取 latest effective
    Router-->>Daemon: latest effective_mode
    alt latest effective 变化或需要补写
        Device-->>Daemon: write_mode(latest effective)
    else effective mode 未变化
        Device-->>Daemon: skip write
    end
    IPC-->>Send: IpcResponseEnvelope
    Send-->>Hook: quiet 成功或按 strict/warning 降级
```

说明：`send` 在 router 接受状态后立即返回 `queued=true`，BLE 写入作为后台副作用执行。后台任务在真正写设备前会重新读取 router 的最新 effective mode，避免旧同步任务在 device lock、health 或 reconnect 上排队后，把设备写回已经过期的旧状态。

### 2.2 daemon 启动、自恢复与重连时序图

```mermaid
sequenceDiagram
    autonumber
    participant User as 用户 / Hook
    participant Send as esp send
    participant Boot as Auto-start Helper
    participant Platform as PlatformAdapter
    participant Daemon as esp daemon
    participant Runtime as RuntimeStore
    participant Lock as FileLock
    participant Server as IpcServer
    participant Device as LightDevice / BLE

    User->>Send: 首次执行 send / daemon / status
    Send->>Boot: request_with_auto_start(...)
    alt daemon 未运行
        Boot->>Lock: acquire daemon-autostart.lock<br/>owner = pid + token + start_signature
        Boot->>Server: status 探活
        Boot->>Lock: 清理确认 stale 的 daemon.lock / pid / ipc marker
        Boot->>Platform: spawn_background_daemon(current_exe)
        Platform-->>Daemon: 启动后台进程
        Daemon->>Lock: acquire daemon.lock
        Daemon->>Runtime: ensure_layout / write_pid / write_ipc_info
        Daemon-->>Device: 后台 try_connect_device()
        Daemon->>Server: serve(handler)
        Boot->>Server: 重试发送原始 IPC 请求
    else daemon 已运行
        Boot->>Server: 直接发送 IPC 请求
    end
    Server->>Daemon: handle(request)
    Daemon-->>Server: response
    Server-->>Send: IPC 响应

    loop 每 1 秒
        Daemon->>Daemon: expiry_loop()
        Daemon->>Daemon: prune_expired()
        alt effective mode 改变
            Daemon->>Device: sync_effective_mode(false)
            Daemon->>Daemon: 写入前刷新 latest effective
        end
    end

    loop 退避重连 1s / 2s / 5s / 10s / 30s
        Daemon->>Device: health()
        alt BLE 已断开
            Daemon->>Device: connect()
            alt 重连成功
                Daemon->>Device: sync_effective_mode(true)
                Daemon->>Daemon: 强制补写 latest effective
            end
        end
    end

    loop 每 30 秒
        Daemon->>Daemon: idle_shutdown_loop()
        alt 连续 1 小时无 send / Hook 事件
            Daemon->>Server: shutdown
            Daemon->>Runtime: clear_pid / clear_ipc_info
        end
    end
```

说明：`FileLock` 的 owner 使用 JSON 保存 `pid`、随机 `token` 和可选 `start_signature`。`pid` 用于判断进程是否存活，`start_signature` 用于在平台允许时识别 PID 复用，`token` 用于确保 Drop 只删除自己持有的锁文件。若启动签名不可读取，逻辑会保守地等待活跃 PID，而不是误删可能仍被持有的锁。

### 2.3 路由与覆盖规则判定图

```mermaid
flowchart TD
    Start["daemon 收到 SendPayload"] --> Key["按 (source, session) 定位当前状态"]
    Key --> Off{"payload.mode == off ?"}
    Off -- yes, manual/manual --> ClearAll["清空全部状态并开启 manual_hold_off"]
    Off -- yes, normal source --> RemoveOne["只删除当前 (source, session) 状态"]
    Off -- no --> Candidate["构造 candidate SourceState<br/>写入 raw_event / raw_tool / turn / semantics / ttl"]

    Candidate --> Expired{"旧状态已过期?"}
    Expired -- yes --> Replace["直接替换"]
    Expired -- no --> Preserve{"保留 AI 生成态?"}

    Preserve -- yes --> Keep["当前是 ai，候选是 busy，且 semantics=Continuation<br/>保留现有 ai"]
    Preserve -- no --> Latest{"candidate.updated_at >= current.updated_at ?"}
    Latest -- yes --> Replace
    Latest -- no --> Ignore["忽略更旧状态"]

    ClearAll --> Effective["重新计算 effective mode"]
    RemoveOne --> Effective
    Replace --> Effective
    Keep --> Effective
    Ignore --> Effective

    Effective --> Pick["在所有未过期状态中按 priority，再按 updated_at 选择最高者"]
    Pick --> Queue["返回 accepted，并派发后台 sync_effective_mode()"]
    Queue --> Refresh["写 BLE 前重新读取 latest effective"]
    Refresh --> Sync["写入 latest effective 或跳过 BLE"]
```

### 2.4 mode 决策优先顺序图

`esp send` 在命令侧确定最终 `mode` 的优先顺序是固定的，这一点和 daemon 里的“优先级比较”不是同一层概念：

```mermaid
flowchart TD
    A["收到 send 参数 + Hook stdin"] --> B{"source == manual ?"}
    B -- yes --> M1["直接使用 explicit_mode"]
    B -- no --> C{"explicit_mode == off ?"}
    C -- yes --> M2["直接使用 off"]
    C -- no --> D{"event.suggested_mode 存在?"}
    D -- yes --> M3["使用 suggested_mode"]
    D -- no --> E{"capability 可映射?"}
    E -- yes --> M4["Thinking/Generating/RunningCommand/... -> Mode"]
    E -- no --> M5["退回 explicit_mode"]
```

### 2.5 BLE 后台同步与写前刷新图

`send` 的成功语义是“daemon 已接受状态并排队同步设备”，不是“BLE 已完成写入”。真实 BLE 写入由后台任务尽力执行：

```mermaid
flowchart TD
    Accepted["handle_send: router.apply_send 成功"] --> Spawn["spawn SendSyncContext"]
    Spawn --> Return["立即返回 accepted<br/>queued=true"]
    Spawn --> Sync["后台 sync_effective_mode(force_write)"]

    Sync --> Initial["读取 initial effective<br/>用于日志和本轮同步上下文"]
    Initial --> DeviceLock["获取 LightDevice mutex"]
    DeviceLock --> Health["health() 探测"]
    Health --> Connected{"connected?"}
    Connected -- no --> Reconnect["connect() 重连"]
    Connected -- yes --> Stale{"距离上次成功写入过久?"}
    Stale -- yes --> Reconnect
    Stale -- no --> Refresh
    Reconnect --> Refresh["写 BLE 前重新读取 latest effective"]

    Refresh --> Changed{"latest effective != last_applied<br/>或 force/reconnect?"}
    Changed -- yes --> Write["write_mode(latest effective)"]
    Changed -- no --> Skip["skip unchanged write"]
    Write --> Cache["更新 last_applied_mode<br/>last_ble_write_at<br/>DeviceHealth"]
    Skip --> Done["同步完成"]
    Cache --> Done
```

这张图的关键点是 `Refresh`：旧同步任务即使已经在 `health()`、`connect()` 或 device mutex 上等待了一段时间，也必须在真正写入前重新读取 router 的最新 effective mode。

***

## 3. 核心数据模型

### 3.1 数据模型关系图

```mermaid
classDiagram
    class AgentEvent {
        +source: AgentSource
        +session: String
        +capability: AgentCapability
        +suggested_mode: Option<Mode>
        +cwd: Option<PathBuf>
        +raw_event: Option<String>
        +raw_tool: Option<String>
        +turn: Option<String>
        +semantics: EventSemantics
    }

    class SendPayload {
        +mode: Mode
        +source: String
        +session: String
        +ttl: Option<u64>
        +hook_id: Option<String>
        +raw_event: Option<String>
        +raw_tool: Option<String>
        +capability: Option<AgentCapability>
        +suggested_mode: Option<Mode>
        +cwd: Option<String>
        +turn: Option<String>
        +semantics: EventSemantics
    }

    class SourceState {
        +source: String
        +session: String
        +mode: Mode
        +turn: Option<String>
        +priority: u8
        +updated_at: DateTime
        +expires_at: Option<DateTime>
        +semantics: EventSemantics
    }

    class StatusResponse {
        +daemon: String
        +ble: String
        +device: Option<String>
        +mode: Mode
        +effective: Mode
        +sources: Option
        +runtime_dir: Option<String>
        +ipc: Option<String>
        +last_ble_write_at: Option<DateTime>
    }

    class EventSemantics {
        <<enumeration>>
        Continuation
        ExplicitToolExecution
        FileRead
        FileWrite
        Completion
        Failure
        UserAttention
        Unknown
    }

    class StateRouter {
        +apply_send(payload, now) Mode
        +prune_expired(now)
        +effective_mode(now) Mode
        +snapshot_status(now, verbose)
    }

    AgentEvent --> SendPayload : 派生并补足 resolved mode
    SendPayload --> SourceState : 写入状态池
    SourceState --> EventSemantics
    SendPayload --> EventSemantics
    StateRouter --> SourceState : 管理多个
    StateRouter --> StatusResponse : 生成快照
```

### 3.2 `turn` 与 `semantics` 的定位

| 字段 | 作用 | 来自哪里 | 当前主要用途 |
| --- | --- | --- | --- |
| `turn` | 标识“这是哪一轮 / 哪次工具调用 / 哪个 generation” | 各 source adapter 从 `turn_id`、`tool_use_id`、`generation_id` 等字段提取 | 排障、状态快照、为后续更细粒度覆盖规则保留锚点 |
| `semantics` | 标识“这条事件应该被核心层理解成什么业务语义” | 各 source adapter 把宿主私有事件名映射成统一 `EventSemantics` | 路由层判定覆盖关系，例如保护 `ai` 不被泛化 `Continuation` 的 `busy` 冲掉 |

***

## 4. Install / Uninstall 架构

### 4.1 目标配置文件落点

| 目标 | 全局配置 | 项目级配置 |
| --- | --- | --- |
| Codex | `~/.codex/hooks.json` | `<dir>/.codex/hooks.json` |
| Cursor | `~/.cursor/hooks.json` | `<dir>/.cursor/hooks.json` |
| Claude | `~/.claude/settings.json` | `<dir>/.claude/settings.json` |

补充落点：

| 类型 | 路径规则 | 作用 |
| --- | --- | --- |
| runtime 根目录 | 平台适配器决定，例如 `~/.esp-agent-status-light` | 统一保存安装清单、稳定二进制、副作用运行文件 |
| 稳定二进制副本 | `<runtime_root>/bin/esp` 或 `esp.exe` | release / 分发场景下 Hook 实际引用的命令路径 |
| BLE 配置 | `<runtime_root>/ble.json` | 保存设备名、Service UUID 和 mode characteristic UUID，供 daemon、scan、test 共用 |
| 安装清单 | `<runtime_root>/config.<target>.json` | 记录该 `target` 的多条安装记录，按 `config_path` 去重/upsert |
| daemon 运行信息 | `<runtime_root>/runtime/daemon.pid`、`ipc.json`、`daemon.lock`、`daemon-autostart.lock` | daemon 自恢复、启动串行化、排障与 `status` 查询 |
| 日志文件 | `<runtime_root>/runtime/events.log`、`runtime.log`、`runtime.lock` | `logs` 读取用户事件；runtime 链路日志用于排查；日志写入通过 token 化文件锁串行化 |

文件锁 owner 当前采用 JSON 结构，兼容旧版本 pid-only 锁文件：

```json
{
  "pid": 12345,
  "token": "random-uuid",
  "start_signature": "Mon Jun 15 12:34:56 2026"
}
```

`pid` 用于存活检查，`start_signature` 在平台允许读取时用于识别 PID 复用，`token` 用于防止旧 owner Drop 时误删新 owner 的锁。

### 4.2 install / uninstall 时序图

```mermaid
sequenceDiagram
    autonumber
    participant User as 用户
    participant CLI as esp install / uninstall
    participant Registry as HookInstallRegistry
    participant Adapter as Target Install Adapter
    participant Runtime as RuntimeStore
    participant FS as Config File
    participant Platform as PlatformAdapter
    participant Boot as Auto-start Helper
    participant Daemon as esp daemon

    User->>CLI: esp install codex|cursor|claude [--dir]
    CLI->>Runtime: ensure_layout()
    CLI->>Registry: get(target)
    Registry-->>CLI: HookInstallAdapter
    CLI->>Adapter: config_path(scope)
    Adapter-->>CLI: 目标配置路径
    CLI->>CLI: resolve_install_command()
    CLI->>Adapter: hook_specs(spec_exe)
    CLI->>FS: read_json_or_empty(config_path)
    CLI->>FS: backup_if_exists(config_path)
    CLI->>Adapter: install(config, specs, hook_id, platform)
    Adapter->>Platform: decorate_hook_command(...)
    Adapter-->>CLI: updated JSON
    CLI->>FS: write_json(config_path, updated)
    CLI->>Runtime: write_install_manifest(target, config_path, command_path)
    CLI->>Boot: ensure_daemon_running()
    Boot-->>Daemon: 如未运行则尝试后台拉起

    User->>CLI: esp uninstall codex|cursor|claude [--dir]
    CLI->>Registry: get(target)
    CLI->>Adapter: config_path(scope)
    CLI->>FS: read_json_or_empty(config_path)
    CLI->>FS: backup_if_exists(config_path) if exists
    CLI->>Adapter: uninstall(config, hook_id)
    Adapter-->>CLI: updated JSON
    CLI->>FS: write_json(config_path, updated)
```

### 4.3 配置写入关系图

```mermaid
flowchart TD
    Cmd["esp install <target> [--dir]"] --> Scope["计算 InstallScope<br/>Global / Project(dir)"]
    Scope --> Registry["HookInstallRegistry.get(target)"]
    Registry --> Target["CodexInstallAdapter<br/>CursorInstallAdapter<br/>ClaudeInstallAdapter"]

    Target --> Path["adapter.config_path(scope)"]
    Path --> ConfigFile{"目标配置文件"}
    ConfigFile -- codex --> CodexPath["~/.codex/hooks.json<br/><dir>/.codex/hooks.json"]
    ConfigFile -- cursor --> CursorPath["~/.cursor/hooks.json<br/><dir>/.cursor/hooks.json"]
    ConfigFile -- claude --> ClaudePath["~/.claude/settings.json<br/><dir>/.claude/settings.json"]

    Target --> Specs["adapter.hook_specs(exe)"]
    Specs --> RemoveOld["install 前先按 hook_id 清理旧托管条目"]
    RemoveOld --> Merge["把 HookSpec 合并进宿主原生 JSON 结构"]
    Merge --> Backup["覆盖前备份原文件<br/>*.bak.<timestamp>"]
    Backup --> WriteConfig["write_json(config_path)"]

    Cmd --> ResolveCmd["resolve_install_command()"]
    ResolveCmd --> Stable{"当前是否开发态 cargo run?"}
    Stable -- yes --> CargoRun["command_path = cargo run --manifest-path ... --"]
    Stable -- no --> StableBin["复制到 <runtime_root>/bin/esp(.exe)"]

    CargoRun --> Manifest["write_install_manifest"]
    StableBin --> Manifest
    WriteConfig --> Manifest["<runtime_root>/config.<target>.json"]

    Manifest --> ManifestBody["记录 target / installed_at / config_path / command_path"]
    ManifestBody --> RuntimeRoot["<runtime_root>"]
```

### 4.4 不同目标的配置结构差异图

```mermaid
flowchart LR
    HookSpec["统一 HookSpec 列表"] --> Codex["Codex / Claude 风格<br/>hooks -> event -> matcher-group[] -> hooks[]"]
    HookSpec --> Cursor["Cursor 风格<br/>hooks -> event -> command[]"]

    Codex --> CodexFields["可带 matcher<br/>hook 内含 command / timeout / statusMessage"]
    Cursor --> CursorFields["可带 matcher<br/>事件项直接是 command 对象"]

    CodexFields --> Managed["所有托管命令都带 --hook-id agent-status-light"]
    CursorFields --> Managed
    Managed --> Uninstall["uninstall 时按 hook_id 精确删除本工具写入的命令"]
```

### 4.5 install / uninstall 当前实现要点

- `install` 会先执行一次逻辑级“卸载旧托管条目”，保证重复安装幂等，不会堆叠重复 Hook。
- `uninstall` 只删除命令中带 `--hook-id agent-status-light` 的托管条目，尽量不碰用户手写 Hook。
- `install` 和 `uninstall` 都会在覆盖前创建备份文件。
- `install` 会把该 `target` 的安装记录追加/更新到 `<runtime_root>/config.<target>.json`；`uninstall` 会删除对应 `config_path` 的记录，若清空则删除整个清单文件。
- `install` 在成功写入配置后，会顺手尝试确保 daemon 已启动。

***

## 5. 阅读建议

如果你是第一次读这个项目，建议顺序：

1. 先看本文件第 1 节和第 2 节，建立“Hook -> send -> IPC -> daemon -> router -> BLE”的主链路。
2. 再看第 4 节，理解为什么 install/uninstall 需要独立 adapter。
3. 对着代码阅读：
   - `src/command.rs`
   - `src/router.rs`
   - `src/daemon.rs`
   - `src/adapters/source/*.rs`
   - `src/adapters/install/*.rs`
