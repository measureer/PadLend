//! 业务服务层（对应 Python 的 service/）：把引擎组装起来并实现 25 个 RPC 方法。
//!
//! * engine：运行时组装（配置、注册表、Router、日志环、事实镜像）；
//! * api：25 个方法的实现（devices/engine/layout/diagnostics/...）；
//! * serializers：与 Python 逐字段一致的 DTO；
//! * registry：设备记录 / 端点 / 去重 / 持久化；
//! * diagnostics：诊断采集、脱敏与导出；
//! * clockfmt：UTC 时间格式化（无 chrono 依赖）。

use thiserror::Error;

pub mod api;
pub mod clockfmt;
pub mod diagnostics;
pub mod engine;
pub mod registry;
pub mod serializers;

/// 服务层错误（RPC 边界统一转成中文字符串，文案与 Python 对齐）。
#[derive(Debug, Error)]
pub enum ServiceError {
    /// 配置路径解析失败。
    #[error("配置路径不可用：{0}")]
    ConfigPath(String),
    /// 配置写盘失败。
    #[error("配置保存失败：{0}")]
    ConfigSave(String),
    /// 引擎初始化失败（会话 / Router 组装）。
    #[error("引擎初始化失败：{0}")]
    Init(String),
}

/// RPC 边界把服务层错误转成纯文本（Python 的 ValueError/RuntimeError 都是字符串）。
impl From<ServiceError> for String {
    fn from(error: ServiceError) -> Self {
        error.to_string()
    }
}
