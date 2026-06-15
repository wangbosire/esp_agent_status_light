//! Hook 安装端口。
//!
//! 每个宿主工具都有自己的配置格式，这一层负责把统一 Hook 规则翻译进去。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::model::{AppError, AppResult, HookSpec, InstallScope};
use crate::ports::platform::PlatformAdapter;

/// Hook 安装器要把“本工具的规则”翻译成各家官方配置格式。
pub trait HookInstallAdapter: Send + Sync {
    /// 返回目标工具名。
    fn target(&self) -> &'static str;
    /// 根据全局 / 项目级安装范围计算配置文件路径。
    fn config_path(&self, scope: &InstallScope) -> AppResult<std::path::PathBuf>;
    /// 生成该工具所需的全部 Hook 规则。
    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec>;
    /// 将 Hook 规则写入宿主工具配置格式。
    ///
    /// 输入 `config` 表示用户当前已有配置，实现必须在尽量保留原配置的前提下
    /// 注入由本工具管理的 Hook 条目。
    fn install(
        &self,
        config: Value,
        specs: &[HookSpec],
        hook_id: &str,
        platform: &dyn PlatformAdapter,
    ) -> AppResult<Value>;
    /// 从宿主工具配置中移除本工具写入的 Hook。
    ///
    /// 实现应只删除由当前 `hook_id` 标识的托管条目，避免误伤用户自定义配置。
    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value>;
}

#[derive(Default, Clone)]
pub struct HookInstallRegistry {
    /// 以目标名索引不同安装器。
    adapters: HashMap<String, Arc<dyn HookInstallAdapter>>,
}

impl HookInstallRegistry {
    /// 创建空安装器注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个安装器实现。
    ///
    /// 使用链式 builder 写法，既方便默认注册表在一处集中声明，
    /// 也方便测试里临时替换某个宿主的安装器实现。
    pub fn with<A>(mut self, adapter: A) -> Self
    where
        A: HookInstallAdapter + 'static,
    {
        self.adapters
            .insert(adapter.target().to_string(), Arc::new(adapter));
        self
    }

    /// 按目标名获取安装器。
    ///
    /// 找不到时在这里统一返回稳定错误，命令层无需再自己拼错误文案。
    pub fn get(&self, target: &str) -> AppResult<Arc<dyn HookInstallAdapter>> {
        // 未知目标在这里统一报错，避免命令层了解各家实现细节。
        // 这样命令层只需要处理“找得到安装器/找不到安装器”两种结果。
        self.adapters.get(target).cloned().ok_or_else(|| {
            AppError::new(
                "unknown_install_target",
                format!("unknown install target: {target}"),
            )
        })
    }
}
