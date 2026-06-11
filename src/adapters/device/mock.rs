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
        self.connected = true;
        Ok(DeviceInfo {
            name: "MockLightDevice".into(),
            id: "mock".into(),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        self.mode = Some(mode);
        Ok(())
    }

    async fn read_mode(&mut self) -> AppResult<Option<Mode>> {
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
