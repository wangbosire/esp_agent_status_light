use async_trait::async_trait;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Utc;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::model::{AppError, AppResult, DeviceHealth, DeviceInfo, Mode};
use crate::ports::device::LightDevice;

/// 真实 BLE adapter 尽量保持“失败可恢复”：
/// 1. daemon 可以先接受 IPC，再慢慢等待蓝牙恢复。
/// 2. 即便当前没有连上设备，也要把 health 信息暴露给 `status --verbose`。
pub struct BtleplugBleAdapter {
    device_name: String,
    service_uuid: Uuid,
    mode_char_uuid: Uuid,
    peripheral: Option<Peripheral>,
    characteristic: Option<Characteristic>,
    health: DeviceHealth,
}

impl Default for BtleplugBleAdapter {
    fn default() -> Self {
        Self {
            // UUID 必须与固件中的 GATT 服务保持严格一致。
            device_name: "AgentStatusLight".into(),
            service_uuid: Uuid::parse_str("b8b7e001-7a6b-4f4f-9a8b-11c0ffee0001")
                .expect("service uuid should be valid"),
            mode_char_uuid: Uuid::parse_str("b8b7e002-7a6b-4f4f-9a8b-11c0ffee0001")
                .expect("char uuid should be valid"),
            peripheral: None,
            characteristic: None,
            health: DeviceHealth::default(),
        }
    }
}

#[async_trait]
impl LightDevice for BtleplugBleAdapter {
    async fn connect(&mut self) -> AppResult<DeviceInfo> {
        // 每次 connect 都重新拿系统蓝牙 adapter，
        // 这样在蓝牙子系统重置后更容易恢复。
        let manager = Manager::new()
            .await
            .map_err(|err| AppError::new("ble_manager_init_failed", err.to_string()))?;
        let adapters = manager
            .adapters()
            .await
            .map_err(|err| AppError::new("ble_adapter_list_failed", err.to_string()))?;
        let adapter = adapters.into_iter().next().ok_or_else(|| {
            AppError::new("ble_adapter_missing", "no bluetooth adapter available")
        })?;

        let peripheral = self.scan_target(adapter).await?;
        let properties = peripheral
            .properties()
            .await
            .map_err(|err| AppError::new("ble_properties_failed", err.to_string()))?
            .ok_or_else(|| {
                AppError::new(
                    "ble_properties_missing",
                    "peripheral properties unavailable",
                )
            })?;

        if !peripheral.is_connected().await.unwrap_or(false) {
            peripheral
                .connect()
                .await
                .map_err(|err| AppError::new("ble_connect_failed", err.to_string()))?;
        }
        peripheral
            .discover_services()
            .await
            .map_err(|err| AppError::new("ble_discover_services_failed", err.to_string()))?;

        let characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|candidate| candidate.uuid == self.mode_char_uuid)
            .ok_or_else(|| {
                AppError::new(
                    "ble_characteristic_missing",
                    "mode characteristic not found",
                )
            })?;

        self.health.connected = true;
        self.health.device_name = properties
            .local_name
            .clone()
            .or(Some(self.device_name.clone()));
        self.health.last_error = None;
        self.peripheral = Some(peripheral.clone());
        self.characteristic = Some(characteristic);

        Ok(DeviceInfo {
            name: properties
                .local_name
                .unwrap_or_else(|| self.device_name.clone()),
            id: format!("{:?}", peripheral.id()),
        })
    }

    async fn write_mode(&mut self, mode: Mode) -> AppResult<()> {
        // 电脑端只发送短字符串 mode，固件无需解析复杂 JSON。
        let peripheral = self
            .peripheral
            .as_ref()
            .ok_or_else(|| AppError::new("ble_not_connected", "device is not connected"))?;
        let characteristic = self.characteristic.as_ref().ok_or_else(|| {
            AppError::new(
                "ble_characteristic_missing",
                "mode characteristic not discovered",
            )
        })?;

        peripheral
            .write(
                characteristic,
                mode.as_str().as_bytes(),
                WriteType::WithResponse,
            )
            .await
            .map_err(|err| {
                // 写失败时立即把连接状态标脏，促使 daemon 的重连循环介入。
                self.health.connected = false;
                self.health.last_error = Some(err.to_string());
                AppError::new("ble_write_failed", err.to_string())
            })?;

        self.health.connected = true;
        self.health.last_mode = Some(mode);
        self.health.last_write_at = Some(Utc::now());
        Ok(())
    }

    async fn read_mode(&mut self) -> AppResult<Option<Mode>> {
        // 当前主要用于调试验证；正常运行链路以 write 为主。
        let peripheral = self
            .peripheral
            .as_ref()
            .ok_or_else(|| AppError::new("ble_not_connected", "device is not connected"))?;
        let characteristic = self.characteristic.as_ref().ok_or_else(|| {
            AppError::new(
                "ble_characteristic_missing",
                "mode characteristic not discovered",
            )
        })?;

        let bytes = peripheral
            .read(characteristic)
            .await
            .map_err(|err| AppError::new("ble_read_failed", err.to_string()))?;
        let text = String::from_utf8(bytes)
            .map_err(|err| AppError::invalid("device returned non-utf8 mode", err))?;
        Ok(Some(text.parse()?))
    }

    async fn health(&self) -> DeviceHealth {
        self.health.clone()
    }
}

impl BtleplugBleAdapter {
    async fn scan_target(&mut self, adapter: Adapter) -> AppResult<Peripheral> {
        // 第一阶段采用简单全量扫描 + 最佳候选选择策略：
        // 满足“名称匹配或服务 UUID 匹配”即可，再从中选择 RSSI 最强者。
        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|err| AppError::new("ble_scan_failed", err.to_string()))?;
        sleep(Duration::from_secs(2)).await;

        let peripherals = adapter
            .peripherals()
            .await
            .map_err(|err| AppError::new("ble_peripheral_list_failed", err.to_string()))?;

        let mut best: Option<(i16, Peripheral)> = None;
        for peripheral in peripherals {
            let Some(properties) = peripheral
                .properties()
                .await
                .map_err(|err| AppError::new("ble_properties_failed", err.to_string()))?
            else {
                continue;
            };

            let name_matches = properties
                .local_name
                .as_deref()
                .is_some_and(|name| name == self.device_name);
            let service_matches = properties.services.contains(&self.service_uuid);
            if !name_matches && !service_matches {
                continue;
            }

            // 多个设备同时满足条件时，优先选择信号最强的那个。
            let rssi = properties.rssi.unwrap_or(i16::MIN);
            match &best {
                Some((best_rssi, _)) if *best_rssi >= rssi => {}
                _ => {
                    best = Some((rssi, peripheral.clone()));
                }
            }
        }

        best.map(|(_, peripheral)| peripheral).ok_or_else(|| {
            AppError::new("ble_device_not_found", "AgentStatusLight device not found")
        })
    }
}
