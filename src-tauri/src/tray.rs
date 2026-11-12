//! 系统托盘：菜单结构与旧版 Qt 版一致，动作统一以事件交给前端编排。

use serde_json::json;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

fn build_menu(app: &AppHandle, captured: bool, connected: bool) -> Result<Menu<Wry>, String> {
    let toggle = MenuItem::with_id(
        app,
        "toggle",
        if captured { "交还控制权" } else { "手动接管" },
        connected,
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    let reconnect = MenuItem::with_id(app, "reconnect", "重连", connected, None::<&str>)
        .map_err(|e| e.to_string())?;
    let open_config = MenuItem::with_id(app, "open-config", "打开配置…", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let copy_diagnostics = MenuItem::with_id(app, "copy-diagnostics", "复制诊断信息", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let open_logs = MenuItem::with_id(app, "open-logs", "打开日志目录…", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let about = MenuItem::with_id(app, "about", "关于 PadLend…", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let run_diagnostics = MenuItem::with_id(app, "run-diagnostics", "运行诊断…", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let wizard = MenuItem::with_id(app, "wizard", "首次设置向导…", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let quit = MenuItem::with_id(app, "quit", "退出 PadLend", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let s1 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let s2 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    let s3 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
    // 显式声明 trait object 数组：MenuItem 与 PredefinedMenuItem 是不同类型，
    // 只有统一成 &dyn IsMenuItem 才能放进同一个切片。
    let items: [&dyn tauri::menu::IsMenuItem<Wry>; 12] = [
        &toggle,
        &reconnect,
        &s1,
        &open_config,
        &copy_diagnostics,
        &open_logs,
        &about,
        &s2,
        &run_diagnostics,
        &wizard,
        &s3,
        &quit,
    ];
    Menu::with_items(app, &items).map_err(|e| e.to_string())
}

pub fn install(app: &AppHandle) -> Result<(), String> {
    let menu = build_menu(app, false, false)?;
    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("PadLend · 未连接");
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder
        .on_menu_event(|app, event| {
            let action = event.id.as_ref().to_string();
            let _ = app.emit("tray:action", json!({ "action": action }));
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.hide();
                    } else {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        })
        .build(app)
        .map_err(|e| format!("托盘创建失败：{e}"))?;
    Ok(())
}

/// 状态变化时只替换菜单，不重建托盘图标（避免图标闪动 / 资源抖动）。
pub fn update(app: &AppHandle, captured: bool, connected: bool, tooltip: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_menu(app, captured, connected) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(tooltip));
    }
}
