//! PadLend Tauri 2 外壳。
//!
//! 职责边界（迁移的核心）：
//! * Rust：进程/窗口/托盘/单实例/clipboard/路径打开 + sidecar 生命周期；
//! * Python（`service/`）：Router 状态机、ADB、UHID、几何、配置、诊断；
//! * React/TS（`src/`）：纯展示与交互，不直接触碰任何业务对象。

mod bridge;
mod commands;
mod tray;

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二实例：把已有窗口还原并置前，与旧版命名事件语义一致。
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            let _ = app.emit("app:present", json!({}));
        }))
        .setup(|app| {
            let handle = app.handle().clone();
            // setup 闭包返回 Box<dyn Error>；String 不是 Error，显式转成 io::Error。
            let bridge = bridge::Bridge::start(handle.clone())
                .map_err(std::io::Error::other)?;
            app.manage(bridge);
            app.manage(Arc::new(AtomicBool::new(false)));
            if let Err(e) = tray::install(&handle) {
                // 托盘注册失败不该让整个应用起不来（旧版也是降级为顶栏菜单）。
                eprintln!("tray unavailable: {e}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::rpc,
            commands::set_close_to_tray,
            commands::set_tray_state,
            commands::service_shutdown,
            commands::app_info,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let close_to_tray = app
                    .state::<Arc<AtomicBool>>()
                    .load(Ordering::Relaxed);
                if close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                    let _ = app.emit("tray:minimized", json!({}));
                } else {
                    // setup 中一定已 manage，窗口关闭事件发生时状态可用。
                    app.state::<Arc<bridge::Bridge>>().shutdown();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("启动 PadLend 外壳失败");
}
