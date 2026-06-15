use async_trait::async_trait;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Utc;
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

use crate::model::{AppError, AppResult, DeviceHealth, DeviceInfo, Mode};
use crate::ports::device::LightDevice;

const BLE_SCAN_WINDOW: Duration = Duration::from_secs(6);
const BLE_SCAN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 真实 BLE adapter 尽量保持“失败可恢复”：
/// 1. daemon 可以先接受 IPC，再慢慢等待蓝牙恢复。
/// 2. 即便当前没有连上设备，也要把 health 信息暴露给 `status --verbose`。
///
/// 这里不做任何“状态路由”判断，只负责：
/// - 找到目标设备；
/// - 建立 GATT 连接；
/// - 把 mode 字符串写入固件暴露的特征值。
pub struct BtleplugBleAdapter {
    /// 目标 BLE 设备名称。
    device_name: String,
    /// 固件暴露的 GATT 服务 UUID。
    service_uuid: Uuid,
    /// 固件暴露的 mode 特征 UUID。
    mode_char_uuid: Uuid,
    /// 当前已连接的外围设备。
    peripheral: Option<Peripheral>,
    /// 已发现的 mode 特征。
    characteristic: Option<Characteristic>,
    /// 当前缓存的设备健康状态。
    health: DeviceHealth,
}

impl Default for BtleplugBleAdapter {
    fn default() -> Self {
        Self {
            // UUID 必须与固件中的 GATT 服务保持严格一致。
            // 这里用 `from_u128` 直接构造，避免在默认构造路径里引入 fallible parse。
            device_name: "AgentStatusLight".into(),
            service_uuid: Uuid::from_u128(0xb8b7e0017a6b4f4f9a8b11c0ffee0001),
            mode_char_uuid: Uuid::from_u128(0xb8b7e0027a6b4f4f9a8b11c0ffee0001),
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
        //
        // 代价是每次连接都会重新扫描，但当前 daemon 的连接频率很低，
        // 更重要的是保证“掉线后能自己恢复”。
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
            // 某些平台扫描后返回的 peripheral 只是“发现了设备”，并不代表已连接。
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
        // 连接成功后，把最后一次“看见的设备名”缓存到 health 里，
        // 即使后面短暂掉线，`status --verbose` 也还能告诉用户刚刚连接的是哪台设备。
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

    async fn disconnect(&mut self) -> AppResult<()> {
        if let Some(peripheral) = self.peripheral.as_ref()
            && peripheral.is_connected().await.unwrap_or(false)
        {
            peripheral
                .disconnect()
                .await
                .map_err(|err| AppError::new("ble_disconnect_failed", err.to_string()))?;
        }
        self.peripheral = None;
        self.characteristic = None;
        self.health.connected = false;
        Ok(())
    }

    async fn health(&self) -> DeviceHealth {
        // status 路径需要尽量反映真实连接状态，因此这里做一次轻量的
        // `is_connected()` 探测；其它诸如最近写入模式、设备名仍沿用缓存快照。
        let mut health = self.health.clone();
        match &self.peripheral {
            Some(peripheral) => match peripheral.is_connected().await {
                Ok(connected) => {
                    health.connected = connected && self.characteristic.is_some();
                    if connected {
                        health.last_error = None;
                    } else if health.last_error.is_none() {
                        health.last_error =
                            Some("ble_disconnected: peripheral is not connected".into());
                    }
                }
                Err(err) => {
                    health.connected = false;
                    health.last_error = Some(format!("ble_health_failed: {err}"));
                }
            },
            None => {
                health.connected = false;
            }
        }
        health
    }
}

impl BtleplugBleAdapter {
    /// 扫描附近 BLE 设备并选择最佳候选目标。
    ///
    /// 当前选择策略很直接：
    /// 1. 名称或服务 UUID 匹配；
    /// 2. 若有多个候选，选择 RSSI 最强的设备。
    async fn scan_target(&mut self, adapter: Adapter) -> AppResult<Peripheral> {
        // 第一阶段采用简单全量扫描 + 最佳候选选择策略：
        // 满足“名称匹配或服务 UUID 匹配”即可，再从中选择 RSSI 最强者。
        //
        // 部分平台蓝牙栈需要一点时间才能把 advertisement/service 信息补全。
        // 因此这里采用“启动扫描 + 多轮轮询”的方式，找到目标后尽快返回；
        // 没找到时也能在错误信息里带上扫描摘要，方便定位是设备未广播还是名称/UUID 不匹配。
        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|err| AppError::new("ble_scan_failed", err.to_string()))?;

        let scan_started = Instant::now();
        let mut best: Option<(i16, Peripheral)> = None;
        let mut max_seen_peripherals = 0usize;
        let mut nearby_names = Vec::<String>::new();

        while scan_started.elapsed() < BLE_SCAN_WINDOW {
            sleep(BLE_SCAN_POLL_INTERVAL).await;
            let peripherals = adapter
                .peripherals()
                .await
                .map_err(|err| AppError::new("ble_peripheral_list_failed", err.to_string()))?;
            max_seen_peripherals = max_seen_peripherals.max(peripherals.len());

            for peripheral in peripherals {
                let Ok(properties) = peripheral.properties().await else {
                    continue;
                };
                let Some(properties) = properties else {
                    continue;
                };

                if let Some(name) = properties.local_name.as_deref()
                    && nearby_names.len() < 8
                    && !nearby_names.iter().any(|seen| seen == name)
                {
                    nearby_names.push(name.to_string());
                }

                let name_matches = properties
                    .local_name
                    .as_deref()
                    .is_some_and(|name| name == self.device_name);
                let service_matches = properties.services.contains(&self.service_uuid);
                // 名称匹配便于开发阶段人工识别，服务 UUID 匹配则能覆盖重命名设备等情况。
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

            if best.is_some() {
                break;
            }
        }

        let _ = adapter.stop_scan().await;

        best.map(|(_, peripheral)| peripheral).ok_or_else(|| {
            AppError::new(
                "ble_device_not_found",
                format!(
                    "{} device not found after {}ms scan; scanned up to {} peripherals; nearby names: {}",
                    self.device_name,
                    BLE_SCAN_WINDOW.as_millis(),
                    max_seen_peripherals,
                    if nearby_names.is_empty() {
                        "<none>".into()
                    } else {
                        nearby_names.join(", ")
                    }
                ),
            )
        })
    }
}
