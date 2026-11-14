//! RPC 方法表与分派（对应 Python service/api.py 的 call()）。
//!
//! 25 个方法的实现在 crate::service::api；运行期通过全局 OnceLock 拿到 Engine。

use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use crate::service::api;
use crate::service::engine::Engine;

/// 26 个 RPC 方法名（Python 的 25 个 + 1.6.0 新增的 devices.cancel）。
pub const METHODS: [&str; 26] = [
    "app.bootstrap",
    "app.shutdown",
    "config.get",
    "config.save",
    "devices.scan",
    "devices.locate",
    "devices.connect",
    "devices.disconnect",
    "devices.cancel",
    "devices.add",
    "devices.remove",
    "layout.desktop",
    "layout.initial",
    "layout.save",
    "engine.toggle",
    "engine.panic",
    "engine.reconnect",
    "engine.inject_clipboard",
    "engine.state",
    "engine.metrics",
    "diagnostics.run",
    "diagnostics.export",
    "diagnostics.static",
    "system.paths",
    "wizard.check",
    "logs.recent",
];

/// 一次分派的结果（rpc 层据此写响应）。
#[derive(Debug)]
pub enum Reply {
    /// 成功：写 result。
    Ok(Value),
    /// 失败：写 error 文本。
    Error(String),
    /// 关闭：写 result 后退出循环。
    Shutdown(Value),
}

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

/// 安装引擎（只允许一次）；已安装返回 false。
pub fn install(engine: Arc<Engine>) -> bool {
    ENGINE.set(engine).is_ok()
}

/// 当前引擎（未接线时为 None）。
pub fn engine() -> Option<Arc<Engine>> {
    ENGINE.get().cloned()
}

/// 是否已经接线。
pub fn installed() -> bool {
    ENGINE.get().is_some()
}

/// 分派一个方法：未知方法 / 未接线都有明确中文错误。
pub fn dispatch(method: &str, params: &Value) -> Reply {
    if !METHODS.contains(&method) {
        return Reply::Error(format!("未知方法：{method}"));
    }
    let Some(engine) = engine() else {
        return Reply::Error("引擎未启动".to_string());
    };
    match method {
        "app.shutdown" => {
            engine.shutdown();
            Reply::Shutdown(json!({ "ok": true }))
        }
        _ => api::call(&engine, method, params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table_matches_python_contract() {
        assert_eq!(METHODS.len(), 26);
        let mut sorted = METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 26, "方法名不得重复");
        for required in [
            "app.bootstrap",
            "app.shutdown",
            "layout.save",
            "engine.state",
            "diagnostics.export",
            "wizard.check",
            "logs.recent",
        ] {
            assert!(METHODS.contains(&required), "缺少方法 {required}");
        }
    }

    #[test]
    fn unknown_method_is_reported_as_unknown() {
        match dispatch("no.such.method", &json!({})) {
            Reply::Error(message) => assert_eq!(message, "未知方法：no.such.method"),
            other => panic!("应报未知方法：{other:?}"),
        }
    }

    #[test]
    fn dispatch_requires_engine_installation() {
        // 该方法测试运行期间全局 ENGINE 必须未被安装（其它测试不 install）。
        assert!(!installed(), "测试进程不应安装全局引擎");
        match dispatch("devices.scan", &json!({})) {
            Reply::Error(message) => assert_eq!(message, "引擎未启动"),
            other => panic!("应报引擎未启动：{other:?}"),
        }
    }

    #[test]
    fn method_list_covers_every_python_handler() {
        // Python 分派表的 25 个键（用于防止移植时漏项）。
        let python = [
            "app.bootstrap",
            "app.shutdown",
            "config.get",
            "config.save",
            "devices.scan",
            "devices.locate",
            "devices.connect",
            "devices.disconnect",
            "devices.add",
            "devices.remove",
            "layout.desktop",
            "layout.initial",
            "layout.save",
            "engine.toggle",
            "engine.panic",
            "engine.reconnect",
            "engine.inject_clipboard",
            "engine.state",
            "engine.metrics",
            "diagnostics.run",
            "diagnostics.export",
            "diagnostics.static",
            "system.paths",
            "wizard.check",
            "logs.recent",
        ];
        for method in python {
            assert!(METHODS.contains(&method), "缺少 {method}");
        }
    }
}
