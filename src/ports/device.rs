use async_trait::async_trait;

use crate::model::{AppResult, DeviceHealth, DeviceInfo, Mode};

#[allow(dead_code)]
#[async_trait]
pub trait LightDevice: Send + Sync {
    /// 建立与物理设备的连接，并返回设备信息。
    async fn connect(&mut self) -> AppResult<DeviceInfo>;
    /// 向设备写入新的 mode。
    async fn write_mode(&mut self, mode: Mode) -> AppResult<()>;
    /// 从设备读取当前 mode；主要用于调试和测试。
    async fn read_mode(&mut self) -> AppResult<Option<Mode>>;
    /// 返回连接健康状态快照。
    async fn health(&self) -> DeviceHealth;
}
