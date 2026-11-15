//! 外壳日志（1.7.2）。
//!
//! Tauri 外壳是 `windows_subsystem = "windows"` 的图形进程：没有控制台，
//! `eprintln!` 等于扔进虚空。1.7.0/1.7.1 排查「开着转发但没有声音」时就卡在
//! 这里——引擎的 service.log 只能看到「播放器把连接关了」，播放器自己为什么
//! 退出却没有任何记录。
//!
//! 所以外壳也落一份文件：`%LOCALAPPDATA%\PadLend\shell.log`，与引擎的
//! `service.log` 同目录，用户回传日志时一并取走即可。
//!
//! 写入永远 best-effort：日志失败绝不影响业务，也不 panic。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// 单文件上限 1 MiB，超过后轮转成 `shell.log.1`（只保留一代）。
const MAX_BYTES: u64 = 1_048_576;

/// 进程启动时刻，用于在每行打出相对时间（跨日志对照毫秒级顺序）。
fn started() -> &'static Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now)
}

/// 外壳日志路径（与 service.log 同目录）。
pub fn log_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("PadLend").join("shell.log")
}

/// 日志目录（shell.log 与引擎 service.log 所在）。托盘「打开日志目录…」打开它。
pub fn log_dir() -> PathBuf {
    log_path()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// 启动时调用一次：建目录、必要时轮转、写一行版本头。
pub fn init(version: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = fs::rename(&path, path.with_extension("log.1"));
        }
    }
    write_line(&format!(
        "===== PadLend {version} 外壳启动 pid={} =====",
        std::process::id()
    ));
}

/// 写一行日志（自动加本地时间戳与相对毫秒）。
pub fn write_line(message: &str) {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let elapsed = started().elapsed().as_millis();
    let path = log_path();
    let line = format!("[{stamp} +{elapsed}ms] {message}\n");
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

/// `shell_log!(...)`：与 `write_line(&format!(...))` 等价，少写一层括号。
#[macro_export]
macro_rules! shell_log {
    ($($arg:tt)*) => {
        $crate::shell_log::write_line(&format!($($arg)*))
    };
}
