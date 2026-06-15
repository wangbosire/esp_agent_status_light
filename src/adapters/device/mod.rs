//! 设备适配器集合。
//!
//! 这里把真实 BLE 设备实现与测试假设备并列放置。

pub mod btleplug_ble;
#[cfg(test)]
pub mod mock;
