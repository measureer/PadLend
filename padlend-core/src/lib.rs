//! PadLend Rust sidecar 的库入口（`main.rs` 只是薄壳，逻辑放在这里便于集成测试）。

pub mod config;
pub mod constants;
pub mod decode;
pub mod logic;
pub mod methods;
pub mod rpc;
pub mod service;
pub mod tablet;
pub mod win;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
