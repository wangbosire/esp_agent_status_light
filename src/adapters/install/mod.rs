pub mod claude;
pub mod codex;
pub mod cursor;

// Hook 安装器公共逻辑。
//
// 这里负责：
// 1. 统一注册不同宿主工具安装器；
// 2. 判断一条 Hook 是否由本工具写入；
// 3. 复用卸载逻辑，确保只删除托管条目，不误伤用户自定义配置。

use serde_json::{Map, Value, json};

use crate::model::HookCommand;
use crate::ports::hook_install::HookInstallRegistry;
use crate::ports::platform::PlatformAdapter;

/// 构建默认 Hook 安装器注册表。
///
/// 这里集中声明当前支持的宿主工具，命令层无需了解具体安装器实现细节。
pub fn registry() -> HookInstallRegistry {
    HookInstallRegistry::new()
        .with(codex::CodexInstallAdapter)
        .with(cursor::CursorInstallAdapter)
        .with(claude::ClaudeInstallAdapter)
}

/// 判断一条命令是否由本工具托管生成。
///
/// 卸载时必须只删除自身写入的 Hook，因此需要一个尽量稳定的识别策略。
fn is_managed_command(command: &str, hook_id: &str) -> bool {
    // 双重判断是为了兼容历史安装结果：
    // 新版本优先用 `--hook-id` 精确识别，旧版本则退回命令特征匹配。
    command.contains(&format!("--hook-id {hook_id}"))
        || (command.contains("esp send --mode") && command.contains("agent-status-light"))
}

/// 确保给定 JSON 值是对象，并返回其可变引用。
fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    // 某些宿主配置文件可能不存在或被用户写成非对象，
    // 这里统一强制转成空对象，后续逻辑才能稳定写入。
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("value should be object")
}

/// 将跨平台命令描述写入宿主配置对象。
///
/// 具体字段形式交给平台层决定，例如 Windows 可能需要额外字段覆盖。
fn decorate_command_fields(
    platform: &dyn PlatformAdapter,
    object: &mut Value,
    command: &HookCommand,
) {
    // 具体写哪些字段交给平台层决定，安装器本身不关心 Windows / POSIX 差异。
    platform.decorate_hook_command(object, command);
}

/// 从 Codex/Claude 风格的 hooks 结构中移除本工具托管条目。
fn codex_like_uninstall(mut config: Value, hook_id: &str) -> Value {
    let root = ensure_object(&mut config);
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks);
    // Codex / Claude 的结构都是 “事件 -> matcher group -> hooks[]” 三层，
    // 卸载时只删掉带 hook_id 的命令，其它用户自定义 Hook 必须完整保留。
    for entries in hooks_map.values_mut() {
        let Some(items) = entries.as_array_mut() else {
            continue;
        };
        for entry in items.iter_mut() {
            let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| is_managed_command(command, hook_id))
            });
        }
        items.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
    }
    config
}

/// 从 Cursor 风格的 hooks 结构中移除本工具托管条目。
fn cursor_uninstall(mut config: Value, hook_id: &str) -> Value {
    let root = ensure_object(&mut config);
    root.entry("version").or_insert_with(|| json!(1));
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks);
    // Cursor 的结构更扁平，是 “事件 -> command[]”；
    // 因此这里按 command 字段直接筛掉本工具写入的条目。
    for entries in hooks_map.values_mut() {
        let Some(items) = entries.as_array_mut() else {
            continue;
        };
        items.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| is_managed_command(command, hook_id))
        });
    }
    config
}

// 测试实现拆到独立目录，避免与 Hook 安装/卸载公共逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/mod_tests.rs"]
mod tests;
