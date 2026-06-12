//! 测试用假设备实现。

use async_trait::async_trait;

use crate::model::{AppResult, DeviceHealth, DeviceInfo, Mode};
use crate::ports::device::LightDevice;

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct MockLightDevice {
    /// 是否已经过 connect。
    connected: bool,
    /// 最近一次写入的模式。
    mode: Option<Mode>,
}

#[async_trait]
impl LightDevice for MockLightDevice {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        // 测试假设备不做任何真实 IO，只记录“已连接”状态。
        self.connected = true;
        Ok(DeviceInfo {
            name: "MockLightDevice".into(),
            id: "mock".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        // 直接把最后一次写入缓存下来，供测试断言。
        self.mode = Some(mode);
        Ok(())
    }

    async fn read_mode(&mut self) -> AppResult<Option<Mode>> {
        // 读取直接返回本地缓存，足以覆盖单元测试对“最近一次写入”的断言需求。
        Ok(self.mode)
    }

    async fn health(&self) -> DeviceHealth {
        DeviceHealth {
            connected: self.connected,
            device_name: Some("MockLightDevice".into()),
            last_error: None,
            last_write_at: None,
            last_mode: self.mode,
        }
    }
}
