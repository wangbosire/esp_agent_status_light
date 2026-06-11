# AgentStatusLight 用户操作手册

本文档面向最终用户，介绍 AgentStatusLight 的开箱使用、日常操作、Hook 安装、状态说明和常见问题排查。

---

## 1. 产品简介

AgentStatusLight 是一款通过蓝牙连接电脑的桌面状态灯。

它可以和 Codex、Cursor、Claude 等 AI 编程工具联动，用灯光显示当前工作状态，例如：

- 思考中
- 正在生成内容
- 正在执行命令
- 成功完成
- 执行失败
- 等待你确认或授权

你不需要了解固件、蓝牙协议或内部实现，只需要按本文步骤完成连接和安装即可。

---

## 2. 使用前准备

请先确认以下条件：

- 设备已经正确供电
- 状态灯固件已经烧录完成
- 电脑支持蓝牙
- 电脑已安装本产品附带的 `esp` 工具

当前建议平台：

- macOS
- Windows

设备默认蓝牙名称：

```text
AgentStatusLight
```

设备上电后，默认会进入 `demo` 展示模式。

---

## 3. 首次使用

### 3.1 连接电源

用 USB 线给设备供电。

上电后，状态灯会自动进入演示模式。如果灯光正常循环变化，说明固件已经在运行。

### 3.2 打开终端

macOS：

- 打开“终端”应用

Windows：

- 打开 PowerShell 或 Windows Terminal

### 3.3 确认命令可用

输入：

```bash
esp status
```

如果已经正确安装工具，你会看到一段 JSON 状态输出。

如果提示找不到 `esp` 命令，请先安装电脑端工具包，或联系卖家获取安装包。

---

## 4. 日常使用流程

建议按下面顺序完成首次配置：

1. 给状态灯上电
2. 手动测试灯效
3. 安装目标 AI 工具的 Hook
4. 正常使用 Codex / Cursor / Claude
5. 需要排障时查看状态和日志

---

## 5. 手动测试灯效

在正式联动前，建议先手动测试设备和蓝牙连接是否正常。

### 5.1 发送测试模式

例如：

```bash
esp send --mode demo
```

或者：

```bash
esp send --mode green
esp send --mode yellow
esp send --mode red
```

### 5.2 关闭灯光

```bash
esp send --mode off
```

### 5.3 常用测试模式

| 命令 | 含义 |
| --- | --- |
| `esp send --mode demo` | 演示模式 |
| `esp send --mode thinking` | 思考中 |
| `esp send --mode ai` | AI 生成中 |
| `esp send --mode busy` | 执行命令中 |
| `esp send --mode success` | 成功 |
| `esp send --mode error` | 失败 |
| `esp send --mode alarm` | 需要用户处理 |
| `esp send --mode off` | 熄灯 |

如果手动测试正常，说明设备、蓝牙和后台服务基本可用。

---

## 6. 安装 Hook 联动 AI 工具

安装 Hook 后，AI 工具在工作时会自动调用本产品，不需要你手动发送命令。

### 6.1 安装 Codex Hook

```bash
esp install codex
```

### 6.2 安装 Cursor Hook

```bash
esp install cursor
```

### 6.3 安装 Claude Hook

```bash
esp install claude
```

安装成功后，命令会输出配置文件位置和命令路径。

### 6.4 项目级安装

如果你只想在某一个项目里启用 Hook，可以进入项目目录后执行：

```bash
esp install cursor --dir /你的项目目录
```

`codex` 和 `claude` 也支持同样的 `--dir` 方式。

---

## 7. Hook 安装后会发生什么

安装完成后：

- 系统会把对应 Hook 写入目标工具配置文件
- 工具会在关键事件发生时自动调用 `esp send`
- 后台 daemon 会接收这些事件
- daemon 会通过蓝牙把最终状态写入状态灯

你正常使用 AI 工具即可，不需要每次手动操作。

---

## 8. 灯光状态说明

| 灯效模式 | 说明 |
| --- | --- |
| `demo` | 开机演示或空闲展示 |
| `thinking` | AI 正在思考、分析、规划 |
| `ai` | AI 正在写代码或生成内容 |
| `busy` | 正在执行命令、构建、测试、读取工具结果 |
| `success` | 当前任务成功完成 |
| `error` | 当前任务失败 |
| `alarm` | 需要用户介入，例如等待确认、授权、处理阻塞 |
| `traffic` | 自动降级展示模式 |
| `off` | 熄灯 |
| `red` / `yellow` / `green` | 单灯测试模式 |

---

## 9. 查看当前状态

### 9.1 简要状态

```bash
esp status
```

这个命令用于查看：

- daemon 是否运行
- 蓝牙是否已连接
- 当前生效模式是什么

### 9.2 详细状态

```bash
esp status --verbose
```

这个命令会返回更多信息，例如：

- 当前有效状态
- 各个来源的状态明细
- IPC 类型
- runtime 目录
- 最近一次 BLE 写入时间

当你安装了多个来源的 Hook，这个命令特别有用。

---

## 10. 查看日志

如果状态不符合预期，可以查看最近日志：

```bash
esp logs --limit 100
```

你也可以只看最近 20 条：

```bash
esp logs --limit 20
```

日志常用于排查：

- Hook 是否触发
- daemon 是否收到事件
- 蓝牙写入是否失败
- 状态是否被成功接收

---

## 11. 停止后台服务

### 11.1 正常停止

```bash
esp stop
```

### 11.2 强制停止

如果后台服务没有正常响应，可以使用：

```bash
esp stop --force
```

通常只在排障时使用强制停止。

---

## 12. 卸载 Hook

如果你不再需要联动某个工具，可以卸载对应 Hook。

### 12.1 卸载 Codex Hook

```bash
esp uninstall codex
```

### 12.2 卸载 Cursor Hook

```bash
esp uninstall cursor
```

### 12.3 卸载 Claude Hook

```bash
esp uninstall claude
```

如果当初是项目级安装，请加上 `--dir` 指向对应项目目录。

---

## 13. 自动超时说明

为了避免灯长时间高亮，设备内置了自动超时：

- 普通模式最长运行 15 分钟
- 超时后自动切换到 `traffic`
- `traffic` 最长运行 20 分钟
- 然后自动切换到 `off`

这属于正常保护行为。

---

## 14. 常见问题

### 14.1 输入 `esp status` 显示 daemon 未运行

可先尝试：

```bash
esp send --mode demo
```

这个命令通常会自动拉起后台 daemon。

然后再执行：

```bash
esp status --verbose
```

### 14.2 灯不亮，但命令没有报错

请依次检查：

1. 设备是否通电
2. 蓝牙是否开启
3. 是否能执行 `esp send --mode green`
4. `esp status --verbose` 中 `ble` 是否为 `connected`
5. `esp logs --limit 50` 中是否有 BLE 相关错误

### 14.3 手动命令可用，但 AI 工具联动没反应

通常请检查：

1. 是否已经执行 `esp install codex` / `cursor` / `claude`
2. 是否装到了正确的用户目录或项目目录
3. AI 工具本身是否真的触发了对应 Hook
4. 查看 `esp logs --limit 100`

### 14.4 多个 AI 工具同时使用时，为什么灯光不是最后一个触发的状态

这是正常设计。

系统会按优先级选择“当前最重要”的状态，而不是简单按最后一次事件覆盖。例如：

- `error` 会比 `success` 更优先
- `alarm` 会比普通执行状态更优先

这样更符合真实提醒需求。

### 14.5 为什么灯光会自己从 `thinking` 变成 `traffic` 或 `off`

这是固件自动超时保护，不是故障。

如果电脑长时间没有继续发送新状态，设备会自动降级，避免一直高亮。

---

## 15. 建议的日常使用方式

推荐习惯：

1. 开始工作前先给状态灯上电
2. 打开 AI 工具前确认蓝牙正常
3. 第一次使用新电脑时先手动执行一次 `esp send --mode demo`
4. 完成 Hook 安装后，平时无需手动操作
5. 出现异常时优先用 `esp status --verbose` 和 `esp logs` 排查

---

## 16. 推荐命令速查

```bash
esp send --mode demo
esp send --mode off
esp status
esp status --verbose
esp logs --limit 50
esp stop
esp install codex
esp install cursor
esp install claude
esp uninstall codex
esp uninstall cursor
esp uninstall claude
```

---

## 17. 交付建议

如果你把本产品交付给终端用户，建议同时提供以下内容：

- 已烧录好的硬件设备
- 电脑端可执行文件 `esp`
- 本文档
- 一页“快速开始卡片”

快速开始卡片建议只保留 4 步：

1. 连接设备电源
2. 执行 `esp send --mode demo`
3. 执行 `esp install cursor`（或对应目标）
4. 用 `esp status --verbose` 检查连接

---

如需进一步面向售卖场景，我下一步可以继续给你补两份配套文档：

- 《产品包装内的 1 页快速开始说明》
- 《售后排障 FAQ / 客服答复模板》
