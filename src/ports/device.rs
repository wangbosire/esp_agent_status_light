//! 物理灯设备抽象端口。
//!
//! daemon 只依赖这组稳定接口，不关心底层到底是 BLE、串口还是测试 mock。

use async_trait::async_trait;

use crate::model::{AppResult, DeviceHealth, DeviceInfo, Mode};

#[async_trait]
pub trait LightDevice: Send + Sync {
    /// 建立与物理设备的连接，并返回设备信息。
    async fn connect(&mut self) -> AppResult<DeviceInfo>;
    /// 向设备写入新的 mode。
    ///
    /// 这是整个系统最关键的副作用接口之一：
    /// router 算出的最终 mode，都会在 daemon 中通过它落到真实硬件。
    async fn write_mode(&mut self, mode: Mode) -> AppResult<()>;
    /// 返回连接健康状态快照。
    ///
    /// 这不是实时订阅接口，而是一次“当前我看起来是否健康”的轮询结果，
    /// 供 `status`、重连循环和测试检查使用。
    async fn health(&self) -> DeviceHealth;
}
