//! Hook 安装器公共逻辑。
//!
//! 这里负责：
//! 1. 统一注册不同宿主工具安装器；
//! 2. 判断一条 Hook 是否由本工具写入；
//! 3. 复用安装/卸载辅助逻辑，尽量在保留用户配置的前提下完成更新。

pub mod claude;
pub mod codex;
pub mod cursor;

use serde_json::{Map, Value, json};

use crate::model::{AppError, AppResult, HookCommand};
use crate::ports::hook_install::HookInstallRegistry;
use crate::ports::platform::PlatformAdapter;

/// 构建默认 Hook 安装器注册表。
///
/// 这里集中声明当前支持的宿主工具，命令层无需了解具体安装器实现细节。
pub fn registry() -> HookInstallRegistry {
    // 是否支持某个宿主，只由这里决定。
    // 这样新增/下线宿主时，不需要去命令层和测试层到处找字符串分支。
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
fn ensure_object(value: &mut Value) -> AppResult<&mut Map<String, Value>> {
    // 某些宿主配置文件可能不存在或被用户写成非对象，
    // 这里统一强制转成空对象，后续逻辑才能稳定写入。
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .ok_or_else(|| AppError::new("invalid_config_shape", "config value must be an object"))
}

fn ensure_array<'a>(value: &'a mut Value, context: &str) -> AppResult<&'a mut Vec<Value>> {
    // 安装路径比卸载路径更“主动”：
    // 如果用户把配置某段写成了错误类型，我们优先把它拉回可写形态，而不是直接失败。
    if !value.is_array() {
        *value = Value::Array(Vec::new());
    }
    value.as_array_mut().ok_or_else(|| {
        AppError::new(
            "invalid_config_shape",
            format!("{context} must be an array"),
        )
    })
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
    // 卸载路径采用“尽力恢复再删除”的策略：
    // 即使 hooks 根结构有局部损坏，也尽量不要影响其它还能保留的用户配置。
    let root = ensure_object(&mut config).expect("config should be object after normalization");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks).expect("hooks should be object after normalization");
    // Codex / Claude 的结构都是 “事件 -> matcher group -> hooks[]” 三层，
    // 卸载时只删掉带 hook_id 的命令，其它用户自定义 Hook 必须完整保留。
    for entries in hooks_map.values_mut() {
        // 如果某个事件条目不是数组，说明用户手改坏了格式；
        // 这里直接跳过，避免卸载逻辑进一步破坏未知结构。
        let Some(items) = entries.as_array_mut() else {
            continue;
        };
        for entry in items.iter_mut() {
            // 同理，matcher group 里如果没有 hooks 数组，就把它当成“非本工具可管理项”跳过。
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
    let root = ensure_object(&mut config).expect("config should be object after normalization");
    root.entry("version").or_insert_with(|| json!(1));
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_map = ensure_object(hooks).expect("hooks should be object after normalization");
    // Cursor 的结构更扁平，是 “事件 -> command[]”；
    // 因此这里按 command 字段直接筛掉本工具写入的条目。
    for entries in hooks_map.values_mut() {
        // Cursor 结构更扁平，但仍然允许某个事件项损坏时局部跳过。
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

/// 把一组 Hook 规则写入 Codex / Claude 共用的三层 hooks 结构。
///
/// 该 helper 只关注公共 JSON 形状，不关心具体事件语义；
/// “每个宿主要注册哪些事件”仍由各自安装器定义。
pub(crate) fn install_codex_like_hooks(
    mut config: Value,
    specs: &[crate::model::HookSpec],
    hook_id: &str,
    platform: &dyn PlatformAdapter,
    command_timeout: Option<u64>,
    status_message: Option<&str>,
) -> AppResult<Value> {
    // install 先复用 uninstall 做幂等清理，避免重复安装把同一条 Hook 越堆越多。
    config = codex_like_uninstall(config, hook_id);
    let root = ensure_object(&mut config)?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks_map = ensure_object(hooks)?;

    for spec in specs {
        // 每个事件下都保持“数组 of matcher-group”的宿主原生结构，
        // 这样用户手工查看配置时能直接对应回官方文档语义。
        let entry = hooks_map
            .entry(spec.event.clone())
            .or_insert_with(|| json!([]));
        let items = ensure_array(entry, &format!("hooks.{}", spec.event))?;
        let mut group = json!({
            "hooks": [{
                "type": "command"
            }]
        });
        if let Some(timeout) = command_timeout {
            group["hooks"][0]["timeout"] = json!(timeout);
        }
        if let Some(message) = status_message {
            group["hooks"][0]["statusMessage"] = json!(message);
        }
        let mut hook_value = json!({});
        // 真正的命令字段完全交给平台层决定，
        // 安装器只负责“什么时候写什么 Hook”，不关心引用转义细节。
        decorate_command_fields(platform, &mut hook_value, &spec.command);
        // Claude / Codex 这类宿主要求每个 hook 明确声明类型，
        // 否则配置文件虽然能写出来，但宿主不会把它当成可执行命令 hook。
        hook_value["type"] = json!("command");
        group["hooks"][0] = hook_value;
        if let Some(matcher) = &spec.matcher {
            group["matcher"] = json!(matcher);
        }
        items.push(group);
    }

    Ok(config)
}

/// 把一组 Hook 规则写入 Cursor 风格的扁平 hooks 结构。
pub(crate) fn install_cursor_like_hooks(
    mut config: Value,
    specs: &[crate::model::HookSpec],
    hook_id: &str,
    platform: &dyn PlatformAdapter,
) -> AppResult<Value> {
    // Cursor 的 install 规则和 codex/claude 不同，但“先清理旧托管条目”这条策略保持一致。
    config = cursor_uninstall(config, hook_id);
    let root = ensure_object(&mut config)?;
    root.entry("version").or_insert_with(|| json!(1));
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks_map = ensure_object(hooks)?;

    for spec in specs {
        let entry = hooks_map
            .entry(spec.event.clone())
            .or_insert_with(|| json!([]));
        let items = ensure_array(entry, &format!("hooks.{}", spec.event))?;
        let mut item = json!({});
        decorate_command_fields(platform, &mut item, &spec.command);
        if let Some(matcher) = &spec.matcher {
            item["matcher"] = json!(matcher);
        }
        items.push(item);
    }

    Ok(config)
}

// 测试实现拆到独立目录，避免与 Hook 安装/卸载公共逻辑混写在同一个文件里。
#[cfg(test)]
#[path = "../../../tests/adapters/install/mod_tests.rs"]
mod tests;
