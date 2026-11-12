//! Tauri 命令层：只做转发与窗口状态，业务规则一律留在 Python 服务里。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, State};

use crate::bridge::Bridge;
use crate::tray;

/// 前端唯一需要的通用业务调用：`invoke("rpc", { method, params })`。
/// 具体方法名由 `src/api/client.ts` 里的强类型封装集中定义。
#[tauri::command]
pub async fn rpc(
    method: String,
    params: Option<Value>,
    bridge: State<'_, Arc<Bridge>>,
) -> Result<Value, String> {
    bridge
        .call(&method, params.unwrap_or_else(|| json!({})))
        .await
}

#[tauri::command]
pub fn set_close_to_tray(enabled: bool, flag: State<'_, Arc<AtomicBool>>) {
    flag.store(enabled, Ordering::Relaxed);
}

#[tauri::command]
pub fn set_tray_state(captured: bool, connected: bool, device: String, app: AppHandle) {
    let suffix = if device.is_empty() {
        "未连接".to_string()
    } else {
        let state = if captured { "接管中" } else { "已连接" };
        format!("{device} · {state}")
    };
    tray::update(&app, captured, connected, &format!("PadLend · {suffix}"));
}

#[tauri::command]
pub fn service_shutdown(bridge: State<'_, Arc<Bridge>>) {
    bridge.shutdown();
}

/// 真正的退出：先等 sidecar 完成 teardown，再让 Tauri 退出。
///
/// 不能复用 `window.close()`：`close_to_tray=true` 时它会被 `prevent_close()`
/// 拦下，只隐藏窗口，进程和托盘图标都还在（1.2.0 缺陷 B）。
#[tauri::command]
pub fn quit_app(app: AppHandle, bridge: State<'_, Arc<Bridge>>) {
    bridge.shutdown();
    app.exit(0);
}

#[tauri::command]
pub fn app_info() -> Value {
    json!({
        "name": "PadLend",
        "version": env!("CARGO_PKG_VERSION"),
        "shell": "Tauri 2",
    })
}
