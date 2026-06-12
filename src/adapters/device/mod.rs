//! 设备适配器集合。
//!
//! 这里把真实 BLE 设备实现与测试假设备并列放置，
//! 让 daemon 在生产和测试场景下都能复用同一套 `LightDevice` 抽象。

pub mod btleplug_ble;
pub mod mock;
