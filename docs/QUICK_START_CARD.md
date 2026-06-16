# AgentStatusLight 快速开始

适用于首次拿到设备的用户。

---

## 1. 接通电源

用 USB 线给状态灯供电。  
上电后，灯会自动进入演示模式。

如果灯完全不亮，请先检查：

- USB 线是否正常
- 供电口是否稳定
- 设备是否已烧录固件

---

## 2. 打开终端

macOS：

- 打开“终端”

Windows：

- 打开 PowerShell 或 Windows Terminal

---

## 3. 先做一次手动测试

输入：

```bash
esp send --mode demo
```

如果灯效正常变化，说明设备、蓝牙和工具基本可用。

你也可以继续测试：

```bash
esp ble scan --duration 10
esp ble test --mode green
esp send --mode green
esp send --mode yellow
esp send --mode red
esp send --mode off
```

---

## 4. 安装 AI 工具联动

按你使用的工具执行其中一个：

### Codex

```bash
esp install codex
```

### Cursor

```bash
esp install cursor
```

### Claude

```bash
esp install claude
```

安装完成后，AI 工具在工作时会自动驱动状态灯。

---

## 5. 检查当前状态

输入：

```bash
esp status --verbose
```

重点看这几项：

- `daemon` 是否为 `running`
- `ble` 是否为 `connected`
- `mode` 是否正常变化

---

## 6. 常用命令

```bash
esp send --mode demo
esp send --mode off
esp ble scan --duration 10
esp ble test --mode green
esp status
esp status --verbose
esp logs --limit 50
esp stop
```

---

## 7. 灯光含义

| 灯效 | 含义 |
| --- | --- |
| `thinking` | AI 正在思考 |
| `ai` | AI 正在生成内容 |
| `busy` | 正在执行命令 |
| `success` | 任务成功 |
| `error` | 任务失败 |
| `alarm` | 等待你处理 |
| `off` | 熄灯 |

---

## 8. 常见问题

### 执行命令后灯没反应

先执行：

```bash
esp status --verbose
esp logs --limit 50
```

再检查：

- 设备是否通电
- 电脑蓝牙是否开启
- `esp ble scan --duration 10` 是否能看到匹配设备
- `esp ble test --mode green` 是否能独立连上设备
- 是否已经安装对应 Hook

### daemon 显示未运行

如果长时间没有使用，后台 daemon 会在 1 小时空闲后自动停止。再次执行 `esp send --mode demo` 或下一次 Hook 触发时会自动拉起。

### Hook 没生效

重新执行安装命令，例如：

```bash
esp install cursor
```

---

## 9. 完整手册

完整操作说明见：

[docs/USER_MANUAL.md](/Users/bytedance/Desktop/workspace/repo/esp/docs/USER_MANUAL.md)
