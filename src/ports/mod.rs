//! 核心层依赖的所有稳定 port 定义。
//!
//! 这些 trait 把“业务语义”与“外部实现”隔开，使新增 Agent、平台或传输方式时，
//! 只需要新增 adapter，而不必改动路由与 daemon 核心逻辑。

pub mod device;
pub mod hook_install;
pub mod ipc;
pub mod log;
pub mod platform;
pub mod runtime;
pub mod source;
