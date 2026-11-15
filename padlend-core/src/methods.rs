//! RPC 方法表与分派（对应 Python service/api.py 的 call()）。
//!
//! 29 个方法的实现在 crate::service::api；运行期通过全局 OnceLock 拿到 Engine。

use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use crate::service::api;
use crate::service::engine::Engine;

/// RPC 方法名白名单（Python 的 25 个 + 1.6.0 的 devices.cancel + 2.0.0 的三个指针校准）。
pub const METHODS: [&str; 29] = [
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
    // 2.0.0 指针校准（设置页 → 鼠标 → 指针校准）。
    "pointer.calibrate_begin",
    "pointer.calibrate_state",
    "pointer.calibrate_end",
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
        assert_eq!(METHODS.len(), 29);
        let mut sorted = METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 29, "方法名不得重复");
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

    /// 白名单必须覆盖 api::dispatch 里的方法：2.0.0 漏过一次（实现写在 api.rs，
    /// 白名单没加 → 运行期「未知方法：pointer.calibrate_begin」，单测也发现不了）。
    #[test]
    fn whitelist_covers_pointer_calibration() {
        for method in [
            "pointer.calibrate_begin",
            "pointer.calibrate_state",
            "pointer.calibrate_end",
        ] {
            assert!(METHODS.contains(&method), "{method} 不在白名单里");
            if let Reply::Error(message) = dispatch(method, &json!({})) {
                assert!(!message.starts_with("未知方法"), "{method}: {message}");
            }
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
