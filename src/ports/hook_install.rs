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
    fn config_path(&self, scope: &InstallScope) -> std::path::PathBuf;
    /// 生成该工具所需的全部 Hook 规则。
    fn hook_specs(&self, exe: &Path) -> Vec<HookSpec>;
    /// 将 Hook 规则写入宿主工具配置格式。
    fn install(
        &self,
        config: Value,
        specs: &[HookSpec],
        hook_id: &str,
        platform: &dyn PlatformAdapter,
    ) -> AppResult<Value>;
    /// 从宿主工具配置中移除本工具写入的 Hook。
    fn uninstall(&self, config: Value, hook_id: &str) -> AppResult<Value>;
}

#[derive(Default, Clone)]
pub struct HookInstallRegistry {
    /// 以目标名索引不同安装器。
    adapters: HashMap<String, Arc<dyn HookInstallAdapter>>,
}

impl HookInstallRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with<A>(mut self, adapter: A) -> Self
    where
        A: HookInstallAdapter + 'static,
    {
        self.adapters
            .insert(adapter.target().to_string(), Arc::new(adapter));
        self
    }

    pub fn get(&self, target: &str) -> AppResult<Arc<dyn HookInstallAdapter>> {
        // 未知目标在这里统一报错，避免命令层了解各家实现细节。
        self.adapters.get(target).cloned().ok_or_else(|| {
            AppError::new(
                "unknown_install_target",
                format!("unknown install target: {target}"),
            )
        })
    }
}
