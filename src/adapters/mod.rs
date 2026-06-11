//! 所有外部适配器实现的汇总入口。
//!
//! 这些模块分别承接不同外部差异：BLE、Hook 配置格式、IPC、文件系统、平台等。

pub mod device;
pub mod install;
pub mod ipc;
pub mod log;
pub mod platform;
pub mod runtime;
pub mod source;
