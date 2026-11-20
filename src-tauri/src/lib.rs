//! PadLend Tauri 2 外壳。
//!
//! 职责边界（迁移的核心）：
//! * Rust 外壳：进程/窗口/托盘/单实例/clipboard/路径打开 + 引擎生命周期；
//! * padlend-core（Rust 引擎，独立进程）：Router 状态机、ADB、UHID、几何、配置、诊断；
//! * React/TS（`src/`）：纯展示与交互，不直接触碰任何业务对象。

mod audio;
mod bridge;
mod commands;
mod shell_log;
mod tray;
mod updates;

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 窗口程序没有控制台：先接上文件日志，后面所有 eprintln 才有地方可查。
    shell_log::init(env!("CARGO_PKG_VERSION"));
    // release 是 panic = "abort"，但 Hook 仍会先跑：把 panic 位置写进日志，
    // 避免 GUI 进程静默消失后什么都查不到。
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        shell_log::write_line(&format!("PANIC {info}"));
        previous_hook(info);
    }));
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二实例：把已有窗口还原并置前，与旧版命名事件语义一致。
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                // show() 不会还原最小化状态：窗口若收在任务栏，要补 unminimize()
                // 才能真正回到用户面前（与 tray.rs 菜单动作的处理一致）。
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            let _ = app.emit("app:present", json!({}));
        }))
        .setup(|app| {
            let handle = app.handle().clone();
            // 音频 sink 先起来，把随机端口通过环境变量交给 sidecar；绑定失败
            // 只降级为无声，不应阻止 PadLend 启动。
            let audio_port = match audio::AudioServer::start() {
                Ok(server) => {
                    let port = server.port();
                    app.manage(server);
                    port
                }
                Err(e) => {
                    shell_log::write_line(&format!("音频接收器启动失败（本次不会有声音）：{e}"));
                    0
                }
            };
            // setup 闭包返回 Box<dyn Error>；String 不是 Error，显式转成 io::Error。
            // 启动失败必须让用户看见（见 run() 末尾的错误框）：release 是 panic = "abort"，
            // 在这里 unwrap/expect 会让进程静默消失，用户只看到「双击没反应」。
            let bridge = match bridge::Bridge::start(handle.clone(), audio_port) {
                Ok(bridge) => bridge,
                Err(e) => {
                    shell_log::write_line(&format!("业务引擎启动失败，无法继续：{e}"));
                    return Err(std::io::Error::other(format!(
                        "业务引擎（padlend-core.exe）未能启动：{e}"
                    ))
                    .into());
                }
            };
            app.manage(bridge);
            app.manage(Arc::new(AtomicBool::new(false)));
            if let Err(e) = tray::install(&handle) {
                // 托盘注册失败不该让整个应用起不来（旧版也是降级为顶栏菜单）。
                shell_log::write_line(&format!("托盘注册失败：{e}"));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::rpc,
            commands::set_close_to_tray,
            commands::set_tray_state,
            commands::open_path,
            commands::open_url,
            commands::check_update,
            commands::service_shutdown,
            commands::quit_app,
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
        .run(tauri::generate_context!());
    // 不 panic：panic = "abort" 下进程直接消失，用户拿不到任何线索。错误写 shell.log，
    // 再用系统错误框说明「缺 padlend-core.exe / 重新安装」，然后正常退出。
    if let Err(err) = result {
        let detail = format!("{err}");
        shell_log::write_line(&format!("外壳启动失败，程序即将退出：{detail}"));
        show_startup_error(&detail);
    }
}

/// 启动失败时的用户可见提示。
///
/// 只声明 user32!MessageBoxW，不引入新依赖：GUI 进程没有控制台，eprintln! 等于扔进虚空。
#[cfg(windows)]
fn show_startup_error(detail: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            u_type: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x0000_0010;
    let message = format!(
        "PadLend 无法启动。\n\n{detail}\n\n请确认安装包完整：PadLend.exe 同目录应有 padlend-core.exe（免安装包还需 vendor 目录），或重新安装 PadLend。\n\n日志：{}",
        shell_log::log_path().display()
    );
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "PadLend 启动失败"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 两个字符串都以 NUL 结尾且在调用期间存活；空句柄表示没有父窗口。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_startup_error(detail: &str) {
    eprintln!("PadLend 无法启动：{detail}");
}
