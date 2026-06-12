//! runtime 存储实现集合。
//!
//! 当前只有文件系统实现，但这里单独保留模块边界，目的是：
//! 1. 让 daemon/command 只依赖 `RuntimeStore` trait；
//! 2. 未来如果需要把部分运行态迁到别的介质，不必回改核心逻辑。

pub mod fs;
