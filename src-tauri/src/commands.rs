//! Tauri 命令层：只做转发与窗口状态，业务规则一律留在 `padlend-core` 引擎里。

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

/// 用系统默认程序打开文件或目录（配置、日志目录、第三方许可）。
///
/// 1.7.5 起改由外壳直接调用插件 Rust API：前端走 `plugin:opener|open_path` 时会先过
/// capability 的 path scope，当时清单里只有字符串 `opener:allow-open-path`（无 scope 条目，
/// 见 Tauri 的 ACL："without any pre-configured scope"），于是任何路径都被拒，前端只收到
/// 一个裸字符串错误，最终提示成「无法打开：undefined」——「打开配置…」「打开日志目录…」
/// 两个托盘菜单项因此完全不可用。该权限现已从清单删除，路径由本命令自行校验。
#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    let target = std::path::PathBuf::from(&path);
    // Windows 默认不给 .yaml/.yml/.md 这类扩展名登记关联，ShellExecute **不报错**却也什么都
    // 不启动（Start-Process config.yaml → 无异常、无窗口），所以「靠系统关联兜底」不可行。
    // 配置、日志、第三方许可是我们自己的纯文本，直接交给记事本，行为可预期。
    let is_text_file = target.is_file()
        && target
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "yaml" | "yml" | "json" | "log" | "txt" | "ini" | "md" | "conf" | "csv"
                )
            })
            .unwrap_or(false);
    if is_text_file {
        std::process::Command::new("notepad.exe")
            .arg(&target)
            .spawn()
            .map_err(|e| {
                crate::shell_log::write_line(&format!("用记事本打开 {path} 失败：{e}"));
                format!("用记事本打开失败：{e}")
            })?;
        return Ok(());
    }
    tauri_plugin_opener::open_path(&target, None::<&str>).map_err(|e| {
        crate::shell_log::write_line(&format!("打开 {path} 失败：{e}"));
        e.to_string()
    })
}

/// 检查更新：返回最新发布的 tag，能拿到详情时一并返回 GitHub 的 release JSON。
///
/// 网络请求在 Rust 侧做（见 updates.rs）：前端 fetch 读不了 github.com（无 CORS），
/// 而 WinHTTP 还能用上系统代理。
#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    // 网络请求放到阻塞线程：同步命令会占着主线程，界面会卡住。
    tauri::async_runtime::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let tag = crate::updates::latest_tag()?;
        let release = crate::updates::release_details();
        Ok(serde_json::json!({ "tag": tag, "release": release }))
    })
    .await
    .map_err(|e| format!("检查更新任务失败：{e}"))?
}

/// 用系统默认浏览器打开网页（检查更新里的下载页 / Release 页）。
///
/// 和 `open_path` 一样直连插件 Rust API：前端那条 `plugin:opener|open_url` 同样要过
/// capability 的 URL 白名单，而清单里没有条目。
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 http(s) 链接".into());
    }
    tauri_plugin_opener::open_url(&url, None::<&str>).map_err(|e| {
        crate::shell_log::write_line(&format!("打开链接 {url} 失败：{e}"));
        e.to_string()
    })
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
