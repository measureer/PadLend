//! 平板连接会话（对应 tablet/session.py，DESIGN 6.1 / 8.2 / 12.3、PITFALLS 1.1~1.7 与 4.2）。
//!
//! 这一层把 adb、scrcpy-server、控制 socket、UHID 与音频通道串成一条会话，
//! 并实现 logic/router 的 SessionPort 供 Router 调用。分块说明：
//!
//! 1. 纯函数：wm size / 朝向 / XScale / pointer_speed / serial 的解析，全部可单测；
//! 2. 依赖注入面：SocketIo + SocketFactory（真实现 TcpSocket/TcpSocketFactory）、
//!    Sleeper（退避睡眠）、StateStore（pointer_speed 与熄屏超时的持久化）；
//! 3. TabletSession：握手、UHID 创建、发送队列 + 1000Hz 批量 flush、健康检查、
//!    重连、朝向监听、六步清理（teardown）。
//!
//! 线程模型（DESIGN 6.1.4）：send_mouse/send_key 只把编码后的报文压进有界队列，
//! 真正的 socket 写由 sender_frame() 在锁外完成；这样 Raw Input 钩子线程永远不会
//! 被网络 IO 阻塞，也不会在锁内做 IO（PITFALLS 4.2）。Router 若自己驱动发送，
//! 必须周期性调用 sender_frame()；默认 start() 会拉起一个 1000Hz 的 flusher 线程。
//!
//! 顺序约束：
//! * 音频通道先于控制通道握手（PITFALLS 2.6）；
//! * 清理顺序固定为 朝向监听 → UHID destroy → socket → server kill/forward remove →
//!   pointer_speed 还原 → screen_off_timeout 还原 → 校验（缺一不可，F7 幽灵设备）；
//! * 每次（重）连接都要重建 UHID：旧的 UHID 已随 app_process 一起消失。

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::constants as C;
use crate::logic::router::{
    DeviceFacts, ErrorCode, HealthReport, RouterError, SessionPort,
};
use crate::tablet::adb::{AdbApi, AdbError, AdbErrorCode};
use crate::tablet::audio::{AudioRelay, AUDIO_SOCKET_POLL_S};
use crate::tablet::server::{ScrcpyServer, KILL_PATTERN};
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// 会话错误
// ---------------------------------------------------------------------------

/// 会话层错误（对应 Python 的 PadLendError(code, detail)）。
///
/// 码值与 logic/router 的 ErrorCode 一一对应，日志里仍然打印 E1xx/E9xx 原文，
/// 保证与 Python 版本的日志/事件可对照。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// 找不到 adb 或设备（E100/E101 由具体分支决定）。
    #[error("E100: {0}")]
    AdbNotFound(String),
    /// 设备不在线/候选地址不可达。
    #[error("E101: {0}")]
    DeviceOffline(String),
    /// UHID 创建失败。
    #[error("E102: {0}")]
    UhidUnavailable(String),
    /// 服务端 jar 推送或 app_process 启动失败。
    #[error("E103: {0}")]
    ServerStartFailed(String),
    /// 端口转发失败。
    #[error("E104: {0}")]
    ForwardFailed(String),
    /// 控制/音频通道握手失败。
    #[error("E105: {0}")]
    HandshakeFailed(String),
    /// 控制通道已断开或连续写失败。
    #[error("E106: {0}")]
    SocketBroken(String),
    /// 其余未预期错误。
    #[error("E900: {0}")]
    Unexpected(String),
}

impl SessionError {
    /// 构造设备离线错误。
    pub fn offline(detail: impl Into<String>) -> Self {
        SessionError::DeviceOffline(detail.into())
    }

    /// 构造握手错误。
    pub fn handshake(detail: impl Into<String>) -> Self {
        SessionError::HandshakeFailed(detail.into())
    }

    /// 冻结错误码。
    pub fn code(&self) -> ErrorCode {
        match self {
            SessionError::AdbNotFound(_) => ErrorCode::AdbNotFound,
            SessionError::DeviceOffline(_) => ErrorCode::DeviceOffline,
            SessionError::UhidUnavailable(_) => ErrorCode::UhidUnavailable,
            SessionError::ServerStartFailed(_) => ErrorCode::ServerStartFailed,
            SessionError::ForwardFailed(_) => ErrorCode::ForwardFailed,
            SessionError::HandshakeFailed(_) => ErrorCode::HandshakeFailed,
            SessionError::SocketBroken(_) => ErrorCode::SocketBroken,
            SessionError::Unexpected(_) => ErrorCode::Unexpected,
        }
    }

    /// 补充说明。
    pub fn detail(&self) -> &str {
        match self {
            SessionError::AdbNotFound(detail)
            | SessionError::DeviceOffline(detail)
            | SessionError::UhidUnavailable(detail)
            | SessionError::ServerStartFailed(detail)
            | SessionError::ForwardFailed(detail)
            | SessionError::HandshakeFailed(detail)
            | SessionError::SocketBroken(detail)
            | SessionError::Unexpected(detail) => detail,
        }
    }

    /// 转成 Router 的错误（保留码与描述）。
    pub fn to_router(&self) -> RouterError {
        RouterError::new(self.code(), self.detail())
    }
}

impl From<AdbError> for SessionError {
    fn from(error: AdbError) -> Self {
        let detail = error.detail.clone();
        match error.code {
            AdbErrorCode::AdbNotFound => SessionError::AdbNotFound(detail),
            AdbErrorCode::DeviceOffline => SessionError::DeviceOffline(detail),
            AdbErrorCode::ServerStartFailed => SessionError::ServerStartFailed(detail),
            AdbErrorCode::ForwardFailed => SessionError::ForwardFailed(detail),
            AdbErrorCode::Unexpected => SessionError::Unexpected(detail),
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数：设备事实解析（逐条对应 session.py 顶部函数）
// ---------------------------------------------------------------------------

/// 常量：settings 里表示"没有设置"的字符串。
const NULL_SETTING: &str = "null";
/// Android 出厂默认 pointer_speed 档位。
const FACTORY_POINTER_SPEED: i32 = -3;
/// 熄屏超时调大到 5 小时（毫秒）。
const SCREEN_TIMEOUT_KEEP_MS: i64 = 5 * 60 * 60 * 1000;
/// 2.0.4：adb 心跳间隔（秒）。后台线程跑，路由线程只读结果。
const LINK_PROBE_INTERVAL_S: f64 = 2.0;
/// 2.0.4：adb 心跳单次超时（秒）。只跑一条 shell 内建命令，2.5s 足够。
const LINK_PROBE_TIMEOUT_S: f64 = 2.5;
/// 朝向轮询周期（秒）。
///
/// 2.0.2：1.0 → 2.0，当时以为「只问显示旋转」很便宜。
/// 2.0.5：2.0 → 15.0。实测同一台平板（HONOR CGA-W00，3200x2136）：
/// `dumpsys window displays` 851ms 起步，忙时 4.7s / 7.7s / 13.4s；而
/// `dumpsys input` 只要 65–150ms。2s 一轮根本追不上 dump 本身，本地 5s 超时
/// 只杀掉 adb 客户端、平板上的 dump 还在跑，下一轮又发一条 —— 几轮就堆成
/// 好几份 window dump 抢 WindowManager 锁，system_server 越拖越慢。
/// 旋转是低频事件，15s 一次足够，连续失败还会翻倍退避。
const ORIENTATION_POLL_S: f64 = 15.0;
/// 2.0.6：接管期间也要偶尔看一眼，否则用户拿着鼠标在平板上转屏时永远发现不了。
const ORIENTATION_CAPTURE_POLL_S: f64 = 60.0;
/// 朝向轮询周期：接管期间放宽到 ORIENTATION_CAPTURE_POLL_S。
fn orientation_interval(capturing: bool) -> f64 {
    if capturing {
        ORIENTATION_CAPTURE_POLL_S
    } else {
        ORIENTATION_POLL_S
    }
}

/// 朝向监听重启等待（秒）。
const ORIENTATION_RESTART_S: f64 = 2.0;
/// 候选地址 TCP 探测超时（秒）。
const CANDIDATE_PROBE_TIMEOUT_S: f64 = 0.6;
/// 候选地址 adb connect 超时（秒）。
const CANDIDATE_CONNECT_TIMEOUT_S: f64 = 4.0;
/// 旧版朝向监听脚本（1.x 时代在平板上常驻一条 adb shell 循环）。
///
/// 2.0.2 起不再使用：改成 PC 侧按 ORIENTATION_POLL_S 跑一次 ORIENTATION_VIEWPORT_SH。
pub const ORIENTATION_SCRIPT: &str = "while true; do dumpsys input 2>/dev/null | grep -m1 'Viewport.*orientation='; sleep 1; done";

/// 朝向查询：跑一次 `dumpsys input`，从 InputDispatcher 的 Viewport 行里取 orientation=。
///
/// 2.0.2 用的是 `dumpsys window displays | grep mDisplayRotation`，看着「输出只有
/// 几十字节」，代价其实全在那个 window dump 上：HONOR CGA-W00 实测 851ms 起步、
/// 忙时 4.7–13.4s，而且全程拿 WindowManager 的锁；`dumpsys input` 同样信息只要
/// 65–150ms。所以 2.0.5 换成了 `dumpsys input`。
///
/// 2.0.6 揪出真正的坑：2.0.5 把它写成
/// `device.shell(&["sh", "-c", "dumpsys input | grep ..."])`，而 adb 客户端是把
/// `shell` 之后的 argv **用空格拼成一个字符串**再交给设备端 shell 的 —— `sh -c`
/// 只拿到第一个词，实际执行的是不带参数的 `dumpsys`（又慢又没有 Viewport 行），
/// 每轮都 5s 超时。朝向检测此前一直靠「三次失败后退回完整 dump」的兜底在工作，
/// 所以时灵时不灵。现在直接执行 `dumpsys input`（单条命令，不经过 `sh -c`），
/// 输出在本地解析。
const ORIENTATION_QUERY_TIMEOUT_S: f64 = 8.0;

/// 从 "Override size: 1200x2000" 这类行里解析宽高。
fn parse_size_value(text: &str) -> Option<(i32, i32)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && !bytes[index].is_ascii_digit() {
        index += 1;
    }
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == start || index >= bytes.len() || bytes[index] != b'x' {
        return None;
    }
    let width: i32 = text[start..index].parse().ok()?;
    index += 1;
    let height_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == height_start {
        return None;
    }
    let height: i32 = text[height_start..index].parse().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((width, height))
}

/// 解析 wm size：优先 Override size，其次 Physical size（对应 parse_wm_size）。
pub fn parse_wm_size(text: &str) -> Result<(i32, i32), SessionError> {
    for key in ["Override size:", "Physical size:"] {
        for line in text.lines() {
            if let Some(rest) = line.split_once(key).map(|(_, rest)| rest) {
                if let Some(size) = parse_size_value(rest) {
                    return Ok(size);
                }
            }
        }
    }
    Err(SessionError::offline(format!(
        "无法解析 wm size 输出: {text:?}"
    )))
}

/// 在文本里找 orientation=(数字)，返回值已 & 0x03。
fn find_orientation(text: &str) -> Option<i32> {
    let key = "orientation=";
    let mut rest = text;
    while let Some(position) = rest.find(key) {
        let after = &rest[position + key.len()..];
        let digits: String = after
            .chars()
            .take_while(|value| value.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            if let Ok(value) = digits.parse::<i32>() {
                return Some(value & 0x03);
            }
        }
        // 2.0.6：after 可能以多字节字符开头（adb shell 的输出会被系统本地化），
        // 以前按字节切 `&after[1..]` 会 panic（byte index 1 is not a char boundary）。
        // 这里按字符边界推进，语义不变：继续往后找下一个 orientation=。
        let step = after.chars().next().map(char::len_utf8).unwrap_or(0);
        rest = &after[step..];
    }
    None
}

/// 解析屏幕朝向（对应 parse_orientation）。
///
/// 优先 Viewport 行的 orientation=；退化到任意 orientation=；再退化按宽高判断。
pub fn parse_orientation(text: &str) -> i32 {
    for line in text.lines() {
        if line.contains("Viewport") && line.contains("orientation=") {
            if let Some(value) = find_orientation(line) {
                return value;
            }
        }
    }
    if let Some(value) = find_orientation(text) {
        return value;
    }
    if let Some((width, height)) = text
        .lines()
        .find_map(|line| line.split_once("Physical size:").and_then(|(_, rest)| parse_size_value(rest)))
    {
        return if width > height { 1 } else { 0 };
    }
    0
}


/// 朝向为奇数时交换宽高（对应 oriented_size）。
pub fn oriented_size(width: i32, height: i32, orientation: i32) -> (i32, i32) {
    if orientation & 1 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

/// 解析 dumpsys input 里 Cursor Input Mapper 段的 XScale（对应 parse_xscale）。
pub fn parse_xscale(text: &str) -> f64 {
    let mut in_section = false;
    let mut found: Option<f64> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if in_section && trimmed.is_empty() {
            break;
        }
        if trimmed.contains("Cursor Input Mapper") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((_, rest)) = trimmed.split_once("XScale:") {
            found = parse_leading_float(rest);
            if found.is_some() {
                break;
            }
        }
    }
    match found {
        Some(value) if value > 0.0 && value.is_finite() => value,
        Some(_) => 1.0,
        None => {
            eprintln!("[tablet.session] dumpsys input 中找不到 XScale，暂用 1.0（A1 前必须核实）");
            1.0
        }
    }
}

/// 解析 " 1.5xxx" 这种前导浮点。
fn parse_leading_float(text: &str) -> Option<f64> {
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices().peekable();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while let Some((index, value)) = chars.next() {
        if value.is_ascii_digit() {
            seen_digit = true;
            end = index + 1;
        } else if value == '.' && !seen_dot {
            seen_dot = true;
            end = index + 1;
        } else {
            let _ = index;
            break;
        }
    }
    if !seen_digit {
        return None;
    }
    trimmed[..end].parse::<f64>().ok()
}

/// 解析 pointer_speed 输出（对应 parse_pointer_speed）：取第一个整数，失败为 0。
pub fn parse_pointer_speed(text: &str) -> i32 {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] == b'-' || bytes[start].is_ascii_digit() {
            let mut index = start;
            if bytes[index] == b'-' {
                index += 1;
            }
            let digits_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index > digits_start {
                if let Ok(value) = text[start..index].parse::<i32>() {
                    return value;
                }
            }
        }
    }
    0
}

/// 清洗 ro.serialno（对应 clean_serial）：取最后一行；unknown/null/0/none 视为空。
pub fn clean_serial(text: &str) -> String {
    let value = text
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or("");
    if matches!(
        value.to_lowercase().as_str(),
        "unknown" | NULL_SETTING | "0" | "none"
    ) {
        return String::new();
    }
    value.to_string()
}

// ---------------------------------------------------------------------------
// 依赖注入面：socket / 睡眠 / 状态文件
// ---------------------------------------------------------------------------

/// 一条已建立的双向字节通道（控制 socket 或音频 socket）。
///
/// 实现的 read 在超时时必须返回 WouldBlock/TimedOut 而不是 0（0 表示对端关闭），
/// 这样 receiver 线程才能区分"暂时没数据"和"对端关闭"。
pub trait SocketIo: Send + Sync + fmt::Debug {
    /// 读入至多 buffer.len() 字节；超时返回 WouldBlock/TimedOut。
    fn read(&self, buffer: &mut [u8]) -> io::Result<usize>;

    /// 写完全部字节。
    fn write_all(&self, data: &[u8]) -> io::Result<()>;

    /// 设置读超时（None = 阻塞）。
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// 设置写超时。
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// 关闭（幂等）。
    fn shutdown(&self) -> io::Result<()>;
}

/// 真实 TCP 通道。
#[derive(Debug)]
pub struct TcpSocket {
    stream: TcpStream,
}

impl TcpSocket {
    /// 从已连接的 TcpStream 构造。
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }
}

impl SocketIo for TcpSocket {
    fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut reference = &self.stream;
        reference.read(buffer)
    }

    fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut reference = &self.stream;
        reference.write_all(data)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(timeout)
    }

    fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}

/// 建立 socket 的工厂（测试注入替身，禁止真连）。
pub trait SocketFactory: Send + Sync + fmt::Debug {
    /// 在 timeout 内连到 addr（形如 "127.0.0.1:1234"）。
    fn connect(&self, addr: &str, timeout: Duration) -> Result<Arc<dyn SocketIo>, String>;
}

/// 真实 TCP 工厂。
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpSocketFactory;

impl SocketFactory for TcpSocketFactory {
    fn connect(&self, addr: &str, timeout: Duration) -> Result<Arc<dyn SocketIo>, String> {
        let mut last_error = String::new();
        let targets = addr
            .to_socket_addrs()
            .map_err(|error| format!("解析 {addr} 失败: {error}"))?;
        for target in targets {
            match TcpStream::connect_timeout(&target, timeout) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(Arc::new(TcpSocket::new(stream)));
                }
                Err(error) => last_error = error.to_string(),
            }
        }
        if last_error.is_empty() {
            last_error = "没有可用地址".to_string();
        }
        Err(last_error)
    }
}

/// 睡眠注入面（重连退避在测试里要瞬时完成）。
pub trait Sleeper: Send + Sync + fmt::Debug {
    /// 睡眠 seconds 秒（<= 0 直接返回）。
    fn sleep(&self, seconds: f64);
}

/// 真实睡眠。
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, seconds: f64) {
        if seconds > 0.0 && seconds.is_finite() {
            thread::sleep(Duration::from_secs_f64(seconds));
        }
    }
}

/// pointer_speed 与 screen_off_timeout 的状态文件读写（对应 session.py 尾部）。
///
/// 文件格式与 Python 一致：{"serial": "...", "value": 0, "saved_at": 1234.5}。
/// 写入走 tmp + rename 的原子替换，避免崩溃时留下半个文件。
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: PathBuf,
}

/// pointer_speed 状态文件名。
pub const POINTER_SPEED_STATE: &str = "pointer_speed_state.json";
/// 熄屏超时状态文件名。
pub const SCREEN_TIMEOUT_STATE: &str = "screen_timeout_state.json";

impl StateStore {
    /// 指定目录（测试用临时目录）。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 用户配置目录（生产用）。
    pub fn with_config_dir() -> Self {
        let dir = C::config_dir().unwrap_or_else(|_| std::env::temp_dir());
        Self { dir }
    }

    /// 状态目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 文件路径。
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// 原子写入一个 JSON 对象（自动补 saved_at）；返回是否成功。
    pub fn save(&self, name: &str, payload: serde_json::Value) -> bool {
        let mut object = match payload {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };
        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs_f64())
            .unwrap_or(0.0);
        object.insert("saved_at".to_string(), serde_json::json!(saved_at));
        let text = serde_json::Value::Object(object).to_string();
        if std::fs::create_dir_all(&self.dir).is_err() {
            eprintln!("[tablet.session] 保存 {name} 状态失败: 无法创建目录");
            return false;
        }
        let path = self.path(name);
        let tmp = self.dir.join(format!("{name}.tmp"));
        let write = (|| -> io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        match write {
            Ok(()) => true,
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                eprintln!("[tablet.session] 保存 {name} 状态失败: {error}");
                false
            }
        }
    }

    /// 读回 JSON 对象；文件不存在或解析失败返回 None。
    pub fn load(&self, name: &str) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.path(name)).ok()?;
        serde_json::from_str::<serde_json::Value>(&text).ok()
    }

    /// 删除状态文件（不存在也算成功）。
    pub fn clear(&self, name: &str) {
        let _ = std::fs::remove_file(self.path(name));
    }

    /// 读 {"serial", "value"}；缺字段或类型不对返回 None。
    pub fn load_entry(&self, name: &str) -> Option<(String, i64)> {
        let value = self.load(name)?;
        let serial = value
            .get("serial")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string();
        let number = value.get("value")?.as_i64()?;
        Some((serial, number))
    }
}

// ---------------------------------------------------------------------------
// 配置与构造
// ---------------------------------------------------------------------------

/// 会话的配置快照（对应 Python 的 cfg 里用到的字段）。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// 配置里的设备 serial（空串 = 自动选择）。
    pub serial: String,
    /// USB 优先。
    pub prefer_usb: bool,
    /// 固定转发端口（Python 默认 C.DEFAULT_SERVER_PORT）。
    pub server_port: u16,
    /// 自定义 jar 路径（None = vendor 下的默认 jar）。
    pub jar: Option<PathBuf>,
    /// 连接期间临时调大熄屏超时。
    pub keep_screen_on: bool,
    /// 开启音频通道。
    pub forward_audio: bool,
    /// start() 成功后自动拉起 1000Hz 发送线程（测试关掉，改由 sender_frame 驱动）。
    pub auto_flush: bool,
    /// start() 成功后是否启动朝向监听（测试可关掉，避免忙等）。
    pub watch_orientation: bool,
    /// 握手重试窗口（秒）；测试调小以免等满 8s。
    pub handshake_retry_timeout: f64,
    /// 退出时把 pointer_speed 还原到出厂值（force_cleanup 用）。
    pub restore_pointer_speed: i32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            serial: String::new(),
            prefer_usb: true,
            server_port: C::DEFAULT_SERVER_PORT,
            jar: None,
            keep_screen_on: true,
            forward_audio: false,
            auto_flush: true,
            watch_orientation: true,
            handshake_retry_timeout: C::HANDSHAKE_RETRY_TIMEOUT_S,
            restore_pointer_speed: FACTORY_POINTER_SPEED,
        }
    }
}

impl SessionConfig {
    /// 从 crate::config::Config 取会话相关字段。
    pub fn from_config(config: &crate::config::Config) -> Self {
        let port = if (1..=65535).contains(&config.device.server_port) {
            config.device.server_port as u16
        } else {
            C::DEFAULT_SERVER_PORT
        };
        Self {
            serial: config.device.serial.clone(),
            prefer_usb: config.device.prefer_usb,
            server_port: port,
            jar: None,
            keep_screen_on: config.misc.keep_screen_on,
            forward_audio: config.misc.forward_audio,
            auto_flush: true,
            watch_orientation: true,
            handshake_retry_timeout: C::HANDSHAKE_RETRY_TIMEOUT_S,
            restore_pointer_speed: config.misc.restore_pointer_speed as i32,
        }
    }
}

/// 从环境变量 PADLEND_AUDIO_PORT 取本机播放器端口（对应 _audio_sink_port）。
pub fn audio_sink_port_from_env() -> Option<u16> {
    let raw = std::env::var("PADLEND_AUDIO_PORT").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<i64>() {
        Ok(port) if (1..65536).contains(&port) => Some(port as u16),
        _ => {
            eprintln!("[tablet.session] PADLEND_AUDIO_PORT 不是整数: {raw:?}");
            None
        }
    }
}

/// 判断 serial 是否像无线地址（与 DeviceEntry::is_wireless 同义）。
/// 读设备的 Android API 等级（getprop ro.build.version.sdk）。
///
/// 读不到（模拟设备、命令超时）时返回 None：调用方保持原设置，不因为一次
/// 查询失败就把音频关掉。
fn android_api_level(adb: &Arc<dyn AdbApi>) -> Option<i32> {
    let (code, text) = adb.shell_result(&["getprop", "ro.build.version.sdk"], 5.0);
    if code != 0 {
        return None;
    }
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|line| line.parse::<i32>().ok())
}

fn looks_wireless(serial: &str) -> bool {
    serial.contains(':') || serial.contains("_adb-tls-connect") || serial.contains(".tcp")
}

/// 会话构造器（依赖全部可注入，测试不碰真 adb / 真 socket / 真时钟）。
pub struct SessionBuilder {
    adb: Arc<dyn AdbApi>,
    config: SessionConfig,
    socket_factory: Arc<dyn SocketFactory>,
    sleeper: Arc<dyn Sleeper>,
    store: Arc<StateStore>,
    candidates: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    verify_facts: Option<Arc<dyn Fn(&str, &DeviceFacts) -> bool + Send + Sync>>,
    audio_sink_port: Option<u16>,
}

impl SessionBuilder {
    /// 用给定 adb 构造（其余依赖用生产实现）。
    pub fn new(adb: Arc<dyn AdbApi>) -> Self {
        Self {
            adb,
            config: SessionConfig::default(),
            socket_factory: Arc::new(TcpSocketFactory),
            sleeper: Arc::new(ThreadSleeper),
            store: Arc::new(StateStore::with_config_dir()),
            candidates: None,
            verify_facts: None,
            audio_sink_port: audio_sink_port_from_env(),
        }
    }

    /// 从应用配置构造。
    pub fn from_config(adb: Arc<dyn AdbApi>, config: &crate::config::Config) -> Self {
        let mut builder = Self::new(adb);
        builder.config = SessionConfig::from_config(config);
        builder
    }

    /// 覆盖配置快照。
    pub fn config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }

    /// 注入 socket 工厂。
    pub fn socket_factory(mut self, factory: Arc<dyn SocketFactory>) -> Self {
        self.socket_factory = factory;
        self
    }

    /// 注入睡眠实现。
    pub fn sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// 注入状态文件目录。
    pub fn store(mut self, store: Arc<StateStore>) -> Self {
        self.store = store;
        self
    }

    /// 注入候选地址 provider（设备注册表）。
    pub fn candidates(mut self, candidates: Arc<dyn Fn() -> Vec<String> + Send + Sync>) -> Self {
        self.candidates = Some(candidates);
        self
    }

    /// 注入身份校验器（serial, facts -> 是否同一台设备）。
    pub fn verify_facts(
        mut self,
        verify: Arc<dyn Fn(&str, &DeviceFacts) -> bool + Send + Sync>,
    ) -> Self {
        self.verify_facts = Some(verify);
        self
    }

    /// 注入本机播放器端口。
    pub fn audio_sink_port(mut self, port: Option<u16>) -> Self {
        self.audio_sink_port = port;
        self
    }

    /// 构造会话（构造零副作用：不碰 adb、不建 socket）。
    pub fn build(self) -> Arc<TabletSession> {
        let auto_flush = self.config.auto_flush;
        let core = Arc::new(SessionCore {
            adb: self.adb,
            socket_factory: self.socket_factory,
            sleeper: self.sleeper,
            store: self.store,
            config: Mutex::new(self.config),
            candidates: self.candidates,
            verify_facts: Mutex::new(self.verify_facts),
            facts_callback: Mutex::new(None),
            clipboard_callback: Mutex::new(None),
            audio_sink_port: self.audio_sink_port,
            lifecycle: Mutex::new(()),
            started: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            sock_broken: AtomicBool::new(false),
            uhid_created: AtomicBool::new(false),
            audio_active: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            orientation_wake: AtomicBool::new(false),
            auto_flush: AtomicBool::new(auto_flush),
            facts: Mutex::new(None),
            last_error: Mutex::new(String::new()),
            started_at: Mutex::new(None),
            failures: AtomicU32::new(0),
            sent: AtomicI64::new(0),
            dev_adb: Mutex::new(None),
            server: Mutex::new(None),
            sock: Mutex::new(None),
            audio_sock: Mutex::new(None),
            audio_relay: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            queue_cv: Condvar::new(),
            send_buffer: Mutex::new(Vec::new()),
            last_mouse_buttons: Mutex::new(None),
            receiver: Mutex::new(None),
            receiver_stop: Mutex::new(None),
            orientation: Mutex::new(None),
            orientation_stop: Mutex::new(None),
            flusher: Mutex::new(None),
            flusher_stop: Arc::new(AtomicBool::new(false)),
            link_ok: AtomicBool::new(true),
            pointer_ok: AtomicBool::new(true),
            link: Mutex::new(None),
            link_stop: Mutex::new(None),
            pointer_original: Mutex::new(None),
            screen_original: Mutex::new(None),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = core.self_ref.set(Arc::downgrade(&core));
        Arc::new(TabletSession { core })
    }
}

// ---------------------------------------------------------------------------
// 会话门面（对 Router 暴露的稳定接口）
// ---------------------------------------------------------------------------

/// 平板连接会话门面（对应 TabletSession）。
///
/// 所有方法都借 &self，内部自行加锁：Router 会在 Raw Input 线程、键盘钩子线程、
/// sender 线程与 supervisor 线程上同时调用。
#[derive(Clone)]
pub struct TabletSession {
    core: Arc<SessionCore>,
}

impl TabletSession {
    /// 用默认依赖构造（生产路径）。
    pub fn new(adb: Arc<dyn AdbApi>) -> Arc<Self> {
        SessionBuilder::new(adb).build()
    }

    /// 开始会话并返回设备事实（DDL 见 DESIGN 6.1.2）。
    pub fn connect(&self) -> Result<DeviceFacts, SessionError> {
        self.core.start()
    }

    /// 停止会话（六步清理，幂等）。
    pub fn disconnect(&self) {
        self.core.stop();
    }

    /// 重连（退避 1/2/4/8/16s）；全部失败返回 Ok(None)。
    pub fn reconnect_attempts(
        &self,
        attempts: Option<usize>,
    ) -> Result<Option<DeviceFacts>, SessionError> {
        self.core.reconnect(attempts)
    }

    /// 刷新设备事实（旋转后调用）。
    pub fn refresh(&self) -> Result<DeviceFacts, SessionError> {
        self.core.refresh_facts()
    }

    /// 当前设备事实。
    pub fn current_facts(&self) -> Option<DeviceFacts> {
        self.core.facts_clone()
    }

    /// 会话是否已启动。
    pub fn is_started(&self) -> bool {
        self.core.started.load(Ordering::SeqCst)
    }

    /// 当前转发端口。
    pub fn port(&self) -> Option<u16> {
        self.core.current_port()
    }

    /// 健康快照。
    pub fn health_report(&self) -> HealthReport {
        self.core.health()
    }

    /// 把配置改动同步进会话。
    ///
    /// Python 的 config.save 直接改 Router/会话共用的 Config 对象，所以「开开关 → 重连」
    /// 立即生效；Rust 的会话配置是构造期快照，必须显式推送（音频、熄屏、端口、指针速度）。
    pub fn update_config(&self, config: &crate::config::Config) {
        self.core.apply_config(config);
    }

    /// 写鼠标报告（进发送队列，由 sender_frame 批量写出）。
    pub fn send_mouse(
        &self,
        buttons: i32,
        dx: i32,
        dy: i32,
        wheel: i32,
    ) -> Result<(), SessionError> {
        self.core.send_mouse(buttons, dx, dy, wheel)
    }

    /// 写键盘全量报告（进发送队列）。
    pub fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), SessionError> {
        self.core.send_key(modifier, usages)
    }

    /// 降级路径：INJECT_KEYCODE（Android keycode，不是 VK）。
    pub fn send_android_keycode(&self, keycode: i64, down: bool) -> Result<(), SessionError> {
        self.core.send_android_keycode(keycode, down)
    }

    /// 操作平板：后退 / 主页 / 最近任务 / 通知栏 / 电源 / 音量（快捷键与 RPC 共用）。
    pub fn android_action(
        &self,
        action: crate::tablet::protocol::AndroidAction,
    ) -> Result<(), SessionError> {
        self.core.android_action(action)
    }

    /// 直接发一个 VK（等价 SessionPort 默认实现：VK 当 usage 用）。
    pub fn send_vk(&self, vk: i32) -> Result<(), SessionError> {
        let usage = vk.clamp(0, 255) as u8;
        self.core.send_key(0, &[usage])
    }

    /// 注入文本（空串忽略）。
    pub fn inject_text(&self, text: &str) -> Result<(), SessionError> {
        self.core.inject_text(text)
    }

    /// 写平板剪贴板（空串忽略）。
    pub fn set_clipboard(&self, text: &str) -> Result<(), SessionError> {
        self.core.set_clipboard(text, 0)
    }

    /// 写平板剪贴板并带序列号。
    pub fn set_clipboard_with_sequence(
        &self,
        text: &str,
        sequence: u64,
    ) -> Result<(), SessionError> {
        self.core.set_clipboard(text, sequence)
    }

    /// 请求取消进行中的 start()（只置取消标志，清理交给 stop）。
    pub fn request_cancel(&self) -> bool {
        self.core.closing.store(true, Ordering::SeqCst);
        true
    }

    /// pointer_speed 复查（supervisor 每 10s 调）；true = 当前为 0。
    pub fn ensure_pointer_speed(&self) -> Result<bool, SessionError> {
        self.core.ensure_pointer_speed()
    }

    /// 注册朝向/分辨率变化回调。
    pub fn set_facts_callback(
        &self,
        callback: Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>,
    ) -> bool {
        match self.core.facts_callback.lock() {
            Ok(mut guard) => {
                *guard = callback;
                true
            }
            Err(_) => false,
        }
    }

    /// 注册平板剪贴板回调。
    pub fn set_clipboard_callback(
        &self,
        callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> bool {
        match self.core.clipboard_callback.lock() {
            Ok(mut guard) => {
                *guard = callback;
                true
            }
            Err(_) => false,
        }
    }

    /// 启动时恢复崩溃遗留的 pointer_speed（对应 restore_pending_pointer_speed）。
    pub fn restore_pending_pointer_speed(&self) -> Option<i32> {
        self.core.restore_pending_pointer_speed()
    }

    /// 启动时恢复崩溃遗留的熄屏超时。
    pub fn restore_pending_screen_timeout(&self) -> Option<i64> {
        self.core.restore_pending_screen_timeout()
    }

    /// 退出兜底清理（对应 force_cleanup）；返回是否做成了尽力清理。
    pub fn force_cleanup(&self, port: Option<u16>) -> bool {
        self.core.force_cleanup(port)
    }

    /// 发送队列里待写的报文数。
    pub fn pending_len(&self) -> usize {
        self.core.queue.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// 批量写出当前队列（最多 MAX_BATCH 条）；返回写成功的条数。
    ///
    /// Router 若自己驱动发送节拍，必须周期性调用本方法；否则报文会一直留在队列里。
    pub fn sender_frame(&self) -> Result<usize, SessionError> {
        self.core.sender_frame()
    }

    /// 状态文件目录（诊断用）。
    pub fn state_dir(&self) -> PathBuf {
        self.core.store.dir().to_path_buf()
    }

    /// 附录 C.2 的诊断项（全部 best-effort；历史：1.x 由 tools/probe.py 消费）。
    pub fn diagnostics(&self) -> Vec<(String, String)> {
        let adb = self.core.dev_adb_clone().unwrap_or_else(|| self.core.adb.clone());
        self.core.probe_diagnostics(&adb)
    }
}

impl fmt::Debug for TabletSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TabletSession")
            .field("started", &self.core.started.load(Ordering::SeqCst))
            .field("port", &self.core.current_port())
            .field("serial", &self.core.facts_clone().map(|facts| facts.serial))
            .finish()
    }
}

impl fmt::Display for TabletSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let serial = self
            .core
            .facts_clone()
            .map(|facts| facts.serial)
            .unwrap_or_else(|| "-".to_string());
        write!(
            formatter,
            "TabletSession(started={}, port={:?}, serial={serial})",
            self.core.started.load(Ordering::SeqCst),
            self.core.current_port()
        )
    }
}

/// 让 Router 直接使用会话（16 个 trait 方法逐条映射 Python 的同名方法）。
impl SessionPort for TabletSession {
    fn send_mouse(&self, buttons: i32, dx: i32, dy: i32, wheel: i32) -> Result<(), RouterError> {
        TabletSession::send_mouse(self, buttons, dx, dy, wheel).map_err(|error| error.to_router())
    }

    fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), RouterError> {
        TabletSession::send_key(self, modifier, usages).map_err(|error| error.to_router())
    }

    fn facts(&self) -> Option<DeviceFacts> {
        self.core.facts_clone()
    }

    fn health(&self) -> Result<Option<HealthReport>, RouterError> {
        Ok(Some(self.core.health()))
    }

    fn set_clipboard(&self, text: &str) -> Result<(), RouterError> {
        TabletSession::set_clipboard(self, text).map_err(|error| error.to_router())
    }

    fn stop(&self) {
        self.core.stop();
    }

    fn start(&self) -> Result<DeviceFacts, RouterError> {
        self.core.start().map_err(|error| error.to_router())
    }

    fn reconnect(&self) -> Option<Result<Option<DeviceFacts>, RouterError>> {
        Some(self.core.reconnect(None).map_err(|error| error.to_router()))
    }

    /// pointer_speed 复查（2.0.4：只读后台线程的最近结果）。
    ///
    /// 真正的 adb 读写挪到了 `link_loop` 后台线程。路由线程每 10s 调一次这里，
    /// 如果在这一层直接跑 `adb shell settings get`，平板 system_server 一忙就会
    /// 卡满 5s 超时 —— 那段时间路由线程不 drain 队列，平板上的指针直接冻住。
    /// 2.0.3 真机实测：注入 600 次移动，平板 /dev/input 侧收到 0 个事件。
    fn ensure_pointer_speed(&self) -> Option<Result<(), RouterError>> {
        if self.core.dev_adb_clone().is_none() {
            return None;
        }
        if self.core.pointer_ok.load(Ordering::SeqCst) {
            Some(Ok(()))
        } else {
            Some(Err(RouterError::device_offline(
                "pointer_speed 复查未通过（后台线程重试中）",
            )))
        }
    }

    /// 链路探针（2.0.3）：同样只读缓存值，真正的 adb 探测在 `link_loop` 里。
    ///
    /// 不能用 ensure_pointer_speed 代劳——那个把「值不是 0」和「adb 超时」
    /// 都折成 Ok(false)，拿它当心跳会把用户手动改过的 pointer_speed 误判成断线。
    fn link_probe(&self) -> Option<Result<(), RouterError>> {
        if self.core.dev_adb_clone().is_none() {
            // 还没 start：没链路可探，按健康处理，别把启动期算成掉线。
            return Some(Ok(()));
        }
        if self.core.link_ok.load(Ordering::SeqCst) {
            Some(Ok(()))
        } else {
            Some(Err(RouterError::device_offline(
                "adb 链路探针失败（后台心跳超时）",
            )))
        }
    }

    fn set_cursor_pos(&self, _x: i32, _y: i32) -> bool {
        false
    }

    fn cancel_start(&self) -> bool {
        self.request_cancel()
    }

    fn send_keycode(&self, vk: i32) -> Result<(), RouterError> {
        TabletSession::send_vk(self, vk).map_err(|error| error.to_router())
    }

    fn android_action(
        &self,
        action: crate::tablet::protocol::AndroidAction,
    ) -> Option<Result<(), RouterError>> {
        Some(TabletSession::android_action(self, action).map_err(|error| error.to_router()))
    }

    fn inject_text(&self, text: &str) -> Result<(), RouterError> {
        self.core.inject_text(text).map_err(|error| error.to_router())
    }

    fn started(&self) -> bool {
        self.core.started.load(Ordering::SeqCst)
    }

    fn set_facts_callback(
        &self,
        callback: Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>,
    ) -> bool {
        TabletSession::set_facts_callback(self, callback)
    }

    fn set_clipboard_callback(
        &self,
        callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> bool {
        TabletSession::set_clipboard_callback(self, callback)
    }

    fn sync_config(&self, config: &crate::config::Config) {
        TabletSession::update_config(self, config);
    }

    fn set_capturing(&self, capturing: bool) {
        self.core.set_capturing(capturing);
    }
}

// ---------------------------------------------------------------------------
// 会话内核（所有状态都在这里；TabletSession 只是薄门面）
// ---------------------------------------------------------------------------

/// 握手尝试的中间结果：区分"协议错误"（立即失败）与"还没就绪"（重试）。
enum HandshakeFailure {
    /// dummy 字节不符：真正的协议错误。
    Protocol(SessionError),
    /// 连接/读取失败：服务端可能还没开始监听。
    Retry(String),
}

/// 会话内核：字段与 Python TabletSession 一一对应。
struct SessionCore {
    // 注入面
    adb: Arc<dyn AdbApi>,
    socket_factory: Arc<dyn SocketFactory>,
    sleeper: Arc<dyn Sleeper>,
    store: Arc<StateStore>,
    /// 配置快照（Python 共享同一个 Config 对象；Rust 是构造期拷贝，运行期改动靠
    /// `update_config` 推送，见 service::api 的 config.save）。
    config: Mutex<SessionConfig>,
    candidates: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    verify_facts: Mutex<Option<Arc<dyn Fn(&str, &DeviceFacts) -> bool + Send + Sync>>>,
    facts_callback: Mutex<Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>>,
    clipboard_callback: Mutex<Option<Arc<dyn Fn(String) + Send + Sync>>>,
    audio_sink_port: Option<u16>,
    self_ref: std::sync::OnceLock<std::sync::Weak<SessionCore>>,
    // 生命周期
    lifecycle: Mutex<()>,
    started: AtomicBool,
    closing: AtomicBool,
    sock_broken: AtomicBool,
    uhid_created: AtomicBool,
    audio_active: AtomicBool,
    /// 2.0.2：Router 是否正在接管（接管期间降低朝向轮询频率，见 orientation_loop）。
    capturing: AtomicBool,
    /// 2.0.6：交还控制权时叫醒朝向线程立刻补一次查询，不用等满一个轮询周期。
    orientation_wake: AtomicBool,
    auto_flush: AtomicBool,
    // 事实与统计
    facts: Mutex<Option<DeviceFacts>>,
    last_error: Mutex<String>,
    started_at: Mutex<Option<Instant>>,
    failures: AtomicU32,
    sent: AtomicI64,
    // 设备与服务端
    dev_adb: Mutex<Option<Arc<dyn AdbApi>>>,
    server: Mutex<Option<Arc<ScrcpyServer>>>,
    // 通道
    sock: Mutex<Option<Arc<dyn SocketIo>>>,
    audio_sock: Mutex<Option<Arc<dyn SocketIo>>>,
    audio_relay: Mutex<Option<Arc<AudioRelay>>>,
    // 发送队列（锁内只搬指针，不做 IO）
    queue: Mutex<std::collections::VecDeque<Vec<u8>>>,
    /// 发送线程在队列空时阻塞等待（2.2.0：替代 1ms 轮询）。
    queue_cv: Condvar,
    /// 发送线程复用的一块拼包缓冲（2.2.0：替代每帧 new Vec<Vec<u8>>）。
    send_buffer: Mutex<Vec<u8>>,
    last_mouse_buttons: Mutex<Option<i32>>,
    // 线程
    receiver: Mutex<Option<JoinHandle<()>>>,
    receiver_stop: Mutex<Option<Arc<AtomicBool>>>,
    orientation: Mutex<Option<JoinHandle<()>>>,
    orientation_stop: Mutex<Option<Arc<AtomicBool>>>,
    flusher: Mutex<Option<JoinHandle<()>>>,
    flusher_stop: Arc<AtomicBool>,
    /// 2.0.4：最近一次 adb 心跳是否通（true=通）。路由线程只读这个值。
    link_ok: AtomicBool,
    /// 2.0.4：最近一次 pointer_speed 复查是否通过。
    pointer_ok: AtomicBool,
    link: Mutex<Option<JoinHandle<()>>>,
    link_stop: Mutex<Option<Arc<AtomicBool>>>,
    // 待还原的原值
    pointer_original: Mutex<Option<i32>>,
    screen_original: Mutex<Option<i64>>,
}

impl fmt::Debug for SessionCore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCore")
            .field("started", &self.started.load(Ordering::SeqCst))
            .field("port", &self.current_port())
            .field("uhid_created", &self.uhid_created.load(Ordering::SeqCst))
            .field("serial", &self.facts_clone().map(|facts| facts.serial))
            .finish()
    }
}

/// 可超时地等待线程结束；返回是否在超时前结束。
fn join_thread(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// 从 socket 精确读满 buffer；EOF 与超时都返回 Err（调用方按"未就绪"处理）。
fn recv_exactly(socket: &Arc<dyn SocketIo>, buffer: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        match socket.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "对端在 dummy 字节前关闭",
                ))
            }
            Ok(count) => filled += count,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl SessionCore {
    // -- 小工具 ------------------------------------------------------------

    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.lifecycle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn shared(&self) -> Option<Arc<SessionCore>> {
        self.self_ref.get().and_then(|weak| weak.upgrade())
    }

    /// 当前配置快照。
    fn config_snapshot(&self) -> SessionConfig {
        match self.config.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 把配置改动同步进会话。
    ///
    /// Python 的 config.save 直接改 Router/会话共用的 Config 对象，所以「开音频开关 →
    /// 重连」立刻生效；Rust 的会话配置是构造期快照，必须显式推送。只覆盖下次连接才读的
    /// 字段，`auto_flush` / `watch_orientation` / 握手超时这些运行时与测试旋钮保持原值。
    fn apply_config(&self, config: &crate::config::Config) {
        let next = SessionConfig::from_config(config);
        let mut guard = match self.config.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.serial = next.serial;
        guard.prefer_usb = next.prefer_usb;
        guard.server_port = next.server_port;
        guard.keep_screen_on = next.keep_screen_on;
        guard.forward_audio = next.forward_audio;
        guard.restore_pointer_speed = next.restore_pointer_speed;
    }

    fn set_facts(&self, facts: Option<DeviceFacts>) {
        if let Ok(mut guard) = self.facts.lock() {
            *guard = facts;
        }
    }

    fn facts_clone(&self) -> Option<DeviceFacts> {
        self.facts.lock().ok().and_then(|guard| guard.clone())
    }

    fn set_last_error(&self, detail: &str) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = detail.to_string();
        }
    }

    fn last_error(&self) -> String {
        self.last_error
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn set_dev_adb(&self, adb: Option<Arc<dyn AdbApi>>) {
        if let Ok(mut guard) = self.dev_adb.lock() {
            *guard = adb;
        }
    }

    fn dev_adb_clone(&self) -> Option<Arc<dyn AdbApi>> {
        self.dev_adb.lock().ok().and_then(|guard| guard.clone())
    }

    fn set_server(&self, server: Option<Arc<ScrcpyServer>>) {
        if let Ok(mut guard) = self.server.lock() {
            *guard = server;
        }
    }

    fn server_clone(&self) -> Option<Arc<ScrcpyServer>> {
        self.server.lock().ok().and_then(|guard| guard.clone())
    }

    fn current_socket(&self) -> Option<Arc<dyn SocketIo>> {
        self.sock.lock().ok().and_then(|guard| guard.clone())
    }

    fn current_port(&self) -> Option<u16> {
        self.server_clone().and_then(|server| server.port())
    }

    fn socket_ok(&self) -> bool {
        self.current_socket().is_some()
            && !self.sock_broken.load(Ordering::SeqCst)
            && self.failures.load(Ordering::SeqCst) < C::CONSECUTIVE_WRITE_FAILURES
    }

    fn set_pointer_original(&self, value: Option<i32>) {
        if let Ok(mut guard) = self.pointer_original.lock() {
            *guard = value;
        }
    }

    fn set_screen_original(&self, value: Option<i64>) {
        if let Ok(mut guard) = self.screen_original.lock() {
            *guard = value;
        }
    }

    /// 等待 seconds 秒；返回 true 表示被 closing 打断（对应 Event.wait）。
    fn closing_wait(&self, seconds: f64) -> bool {
        if seconds <= 0.0 {
            return self.closing.load(Ordering::SeqCst);
        }
        let slices = ((seconds / 0.05).ceil() as u32).max(1);
        for _ in 0..slices {
            if self.closing.load(Ordering::SeqCst) {
                return true;
            }
            self.sleeper.sleep(0.05);
        }
        self.closing.load(Ordering::SeqCst)
    }

    /// 记录 Router 是否正在接管，并在交还控制权时叫醒朝向线程。
    ///
    /// 2.0.6：用户把鼠标滑回电脑（交还）时立刻看一下朝向，否则刚转过屏也要
    /// 等满一个轮询周期（接管期间是 60s）才更新。
    fn set_capturing(&self, capturing: bool) {
        self.capturing.store(capturing, Ordering::SeqCst);
        if !capturing {
            self.orientation_wake.store(true, Ordering::SeqCst);
        }
    }

    /// 可中断睡眠一个朝向轮询周期；返回 true 表示要退出。
    ///
    /// 2.0.6：交还控制权时 `set_capturing(false)` 会抬起 `orientation_wake`，
    /// 这里看到就立刻返回，让下一轮查询马上跑（最多等 50ms），不用等满周期。
    ///
    /// 唤醒旗标必须由调用方按线程各传各的：朝向线程传 `Some(&orientation_wake)`，
    /// adb 心跳线程传 `None`。2.0.6 初版两个线程共用同一个旗标，而心跳线程 2s 周期
    /// 里几乎全程在这段 50ms 分片里睡，`swap(false)` 大概率把给朝向线程的唤醒吃掉，
    /// 交还控制权后实际仍要等满 60s（P0 回归）。谁睡谁消费，别共用。
    fn sleep_interruptible(
        &self,
        seconds: f64,
        stop: &AtomicBool,
        wake: Option<&AtomicBool>,
    ) -> bool {
        let slices = ((seconds / 0.05).ceil() as u32).max(1);
        for _ in 0..slices {
            if stop.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
                return true;
            }
            // 只有带旗标的睡眠才消费它；心跳传 None，永远吃不到朝向唤醒。
            if let Some(flag) = wake {
                if flag.swap(false, Ordering::SeqCst) {
                    return false;
                }
            }
            self.sleeper.sleep(0.05);
        }
        false
    }

    // -- 状态文件 ----------------------------------------------------------

    fn save_pointer_state(&self, serial: &str, value: i64) {
        let payload = serde_json::json!({"serial": serial, "value": value});
        let _ = self.store.save(POINTER_SPEED_STATE, payload);
    }

    fn save_screen_state(&self, serial: &str, value: i64) {
        let payload = serde_json::json!({"serial": serial, "value": value});
        let _ = self.store.save(SCREEN_TIMEOUT_STATE, payload);
    }

    /// 崩溃兜底：启动时恢复遗留的 pointer_speed（对应 restore_pending_pointer_speed）。
    fn restore_pending_pointer_speed(&self) -> Option<i32> {
        let (serial, value) = self.store.load_entry(POINTER_SPEED_STATE)?;
        let dev = if serial.trim().is_empty() {
            self.adb.clone()
        } else {
            self.adb.with_serial(&serial)
        };
        if value != 0
            && !dev.shell_ok(
                &["settings", "put", "system", "pointer_speed", &value.to_string()],
                C::ADB_SHELL_TIMEOUT,
            )
        {
            eprintln!(
                "[tablet.session] 启动恢复 pointer_speed={value} 失败（设备未连接？），保留状态"
            );
            return None;
        }
        self.store.clear(POINTER_SPEED_STATE);
        let label = if serial.trim().is_empty() {
            "(默认)".to_string()
        } else {
            serial.clone()
        };
        eprintln!("[tablet.session] 启动恢复 pointer_speed={value} serial={label}");
        Some(value as i32)
    }

    /// 崩溃兜底：启动时恢复遗留的熄屏超时。
    fn restore_pending_screen_timeout(&self) -> Option<i64> {
        let (serial, value) = self.store.load_entry(SCREEN_TIMEOUT_STATE)?;
        let dev = if serial.trim().is_empty() {
            self.adb.clone()
        } else {
            self.adb.with_serial(&serial)
        };
        if !dev.shell_ok(
            &["settings", "put", "system", "screen_off_timeout", &value.to_string()],
            C::ADB_SHELL_TIMEOUT,
        ) {
            eprintln!(
                "[tablet.session] 启动恢复 screen_off_timeout={value} 失败（设备未连接？），保留状态"
            );
            return None;
        }
        self.store.clear(SCREEN_TIMEOUT_STATE);
        eprintln!("[tablet.session] 启动恢复 screen_off_timeout={value}");
        Some(value)
    }

    /// 退出兜底清理（对应 force_cleanup）：杀残留、摘转发、还原 pointer_speed。
    fn force_cleanup(&self, port: Option<u16>) -> bool {
        let pending = self.store.load_entry(POINTER_SPEED_STATE);
        let config = self.config_snapshot();
        let serial = config.serial.trim().to_string();
        let restore = match &pending {
            Some((state_serial, value)) if serial.is_empty() || *state_serial == serial => *value,
            _ => config.restore_pointer_speed as i64,
        };
        let dev: Arc<dyn AdbApi> = if serial.is_empty() {
            self.adb.clone()
        } else {
            self.adb.with_serial(&serial)
        };
        let _ = dev.shell_ok(&["pkill", "-9", "-f", KILL_PATTERN], C::ADB_SHELL_TIMEOUT);
        if let Some(port) = port {
            dev.forward_remove(port);
        }
        if !serial.is_empty() {
            if dev.shell_ok(
                &["settings", "put", "system", "pointer_speed", &restore.to_string()],
                C::ADB_SHELL_TIMEOUT,
            ) {
                self.store.clear(POINTER_SPEED_STATE);
            }
            let _ = dev.shell_ok(
                &["settings", "put", "system", "pointer_location", "0"],
                C::ADB_SHELL_TIMEOUT,
            );
        }
        true
    }

    // -- 启动 / 停止 / 重连 ------------------------------------------------

    fn start(&self) -> Result<DeviceFacts, SessionError> {
        let _guard = self.lock_lifecycle();
        if self.started.load(Ordering::SeqCst) && self.socket_ok() {
            if let Some(facts) = self.facts_clone() {
                return Ok(facts);
            }
        }
        if self.started.load(Ordering::SeqCst) || self.current_socket().is_some() {
            eprintln!("[tablet.session] 检测到半启动状态，先清理再重启");
            self.teardown();
            self.set_facts(None);
        }
        self.closing.store(false, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        match self.start_locked() {
            Ok(facts) => {
                self.set_facts(Some(facts.clone()));
                if let Ok(mut guard) = self.started_at.lock() {
                    *guard = Some(Instant::now());
                }
                self.sock_broken.store(false, Ordering::SeqCst);
                self.failures.store(0, Ordering::SeqCst);
                self.start_flusher();
                self.start_orientation_watch();
                self.start_link_watch();
                Ok(facts)
            }
            Err(error) => {
                self.set_last_error(error.detail());
                eprintln!(
                    "[tablet.session] 启动失败: {} ({})",
                    error.detail(),
                    error.code().as_str()
                );
                self.teardown();
                self.started.store(false, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    /// 启动各阶段的取消检查点。
    ///
    /// devices.cancel / 断开 / 换设备都会先置 closing，阻塞在 adb 子进程或
    /// 握手里的 start 只会在下一个检查点退出——所以要塞在每一个可能耗时的
    /// 阶段之间，而不是只在 handshake 内部。
    fn ensure_starting(&self, stage: &str) -> Result<(), SessionError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(SessionError::handshake(format!("启动已取消（{stage}）")));
        }
        Ok(())
    }

    fn start_locked(&self) -> Result<DeviceFacts, SessionError> {
        let started = Instant::now();
        let mut stage_at = Instant::now();
        let mut mark = |stage: &str| {
            eprintln!(
                "[tablet.session] 启动阶段 {stage} 完成（本段 {}ms，累计 {}ms）",
                stage_at.elapsed().as_millis(),
                started.elapsed().as_millis()
            );
            stage_at = Instant::now();
        };
        self.ensure_starting("准备")?;
        let facts = self.connect_candidate()?;
        mark("选设备");
        self.ensure_starting("选设备后")?;
        let config = self.config_snapshot();
        let device = self
            .dev_adb_clone()
            .ok_or_else(|| SessionError::offline("尚未选定设备"))?;
        // Android 11（API 30）以下没有系统音频回采，强行 audio=true 只会让
        // 服务端起不来、握手白等一轮再回退（用户看到的就是"开声音转发就卡"）。
        let audio_active = match config.forward_audio {
            false => false,
            true => match android_api_level(&device) {
                Some(level) if level < 30 => {
                    eprintln!(
                        "[tablet.session] 系统 Android API {level} 不支持音频回采（需要 11/API 30），本次只开控制通道"
                    );
                    false
                }
                _ => true,
            },
        };
        if audio_active != config.forward_audio {
            eprintln!("[tablet.session] 平板系统版本过低，已跳过音频转发（只共享控制）");
        }
        self.audio_active.store(audio_active, Ordering::SeqCst);
        let server = Arc::new(ScrcpyServer::with_parts(
            device.clone(),
            Some(config.server_port),
            config.jar.clone(),
            None,
            None,
            audio_active,
        ));
        self.set_server(Some(server.clone()));
        eprintln!(
            "[tablet.session] 使用设备 {}（{}）",
            facts.serial,
            if facts.model.trim().is_empty() {
                "unknown"
            } else {
                facts.model.as_str()
            }
        );
        // 先放好 facts：保存 pointer_speed 状态要用 serial。
        self.set_facts(Some(facts.clone()));
        self.normalize_pointer_speed(&device)?;
        mark("指针速度");
        self.ensure_starting("推 jar 前")?;
        server.push_if_needed()?;
        mark("推 jar");
        let _ = server.kill(5.0);
        server.spawn()?;
        let _port = server.forward()?;
        mark("启服务端+转发");
        self.ensure_starting("握手前")?;
        if let Err(error) = self.connect_socket() {
            if !audio_active {
                return Err(error);
            }
            eprintln!(
                "[tablet.session] 音频通道不可用，回退为仅控制通道（{}ms）: {}",
                started.elapsed().as_millis(),
                error.detail()
            );
            mark("音频握手（失败）");
            self.restart_without_audio()?;
            self.connect_socket()?;
        }
        mark(if audio_active { "音频+控制握手" } else { "控制握手" });
        self.ensure_starting("建 UHID 前")?;
        self.create_uhid()?;
        mark("UHID");
        self.start_receiver();
        self.keep_screen_on(&device);
        mark("熄屏设置");
        eprintln!(
            "[tablet.session] {} serial={} size={}x{} orientation={} xscale={} pointer_speed={}",
            C::EVENT_TABLET_CONNECT,
            facts.serial,
            facts.width,
            facts.height,
            facts.orientation,
            facts.xscale,
            facts.pointer_speed
        );
        Ok(facts)
    }

    /// 丢弃音频服务端，按 audio=false 重启（服务端每实例只接受一次通道）。
    fn restart_without_audio(&self) -> Result<(), SessionError> {
        self.close_socket();
        let port = self.current_port();
        if let Some(old) = self.server_clone() {
            let _ = old.kill(5.0);
            old.forward_remove();
        }
        self.audio_active.store(false, Ordering::SeqCst);
        let device = self
            .dev_adb_clone()
            .ok_or_else(|| SessionError::offline("尚未选定设备"))?;
        let server = Arc::new(ScrcpyServer::with_parts(
            device,
            port,
            self.config_snapshot().jar.clone(),
            None,
            None,
            false,
        ));
        // 旧服务端带 cleanup=true，退出时会自删远端 jar（真机实测：杀进程后
        // /data/local/tmp/scrcpy-server-manual.jar 立刻不在了）。回退重启前必须重新
        // 确认/推送，否则 app_process 找不到 class，握手会一直"对端在 dummy 字节前关闭"，
        // 用户只能干等到 30s 连接超时。
        server.push_if_needed()?;
        server.spawn()?;
        server.forward()?;
        self.set_server(Some(server));
        Ok(())
    }

    fn stop(&self) {
        // 先置取消标志再拿锁：卡在 40s 首连等待里的 start() 会很快看到。
        self.closing.store(true, Ordering::SeqCst);
        self.stop_orientation_watch();
        self.stop_link_watch();
        let _guard = self.lock_lifecycle();
        if !(self.started.load(Ordering::SeqCst)
            || self.current_socket().is_some()
            || self.uhid_created.load(Ordering::SeqCst))
        {
            eprintln!("[tablet.session] stop(): 未启动，忽略");
            self.closing.store(false, Ordering::SeqCst);
            return;
        }
        self.teardown();
        self.started.store(false, Ordering::SeqCst);
        self.set_facts(None);
        self.closing.store(false, Ordering::SeqCst);
    }

    fn reconnect(&self, attempts: Option<usize>) -> Result<Option<DeviceFacts>, SessionError> {
        if !self.started.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let backoff = C::RECONNECT_BACKOFF_S;
        let total = attempts.unwrap_or(backoff.len()).max(1);
        for index in 0..total {
            let delay = backoff[index.min(backoff.len() - 1)];
            if self.closing_wait(delay) {
                return Ok(None);
            }
            let _guard = self.lock_lifecycle();
            if !self.started.load(Ordering::SeqCst) {
                return Ok(None);
            }
            match self.restart_transport() {
                Ok(()) => {
                    eprintln!(
                        "[tablet.session] {} attempt={}",
                        C::EVENT_TABLET_RECONNECT,
                        index + 1
                    );
                    return Ok(self.facts_clone());
                }
                Err(error) => {
                    self.set_last_error(error.detail());
                    eprintln!(
                        "[tablet.session] 重连第 {} 次失败: {}",
                        index + 1,
                        error.detail()
                    );
                }
            }
        }
        eprintln!("[tablet.session] 重连 {total} 次均失败");
        Ok(None)
    }

    /// 只重建服务端 + socket + UHID，不动 pointer_speed / 熄屏设置。
    fn restart_transport(&self) -> Result<(), SessionError> {
        self.close_socket();
        let server = self
            .server_clone()
            .ok_or_else(|| SessionError::ServerStartFailed("服务端尚未创建".to_string()))?;
        if let Err(error) = server.kill(5.0) {
            eprintln!(
                "[tablet.session] 重连时 kill 失败（忽略）: {}",
                error.detail
            );
        }
        server.forward_remove();
        server.push_if_needed()?;
        server.spawn()?;
        server.forward()?;
        self.connect_socket()?;
        self.create_uhid()?;
        self.start_receiver();
        Ok(())
    }

    // -- 设备选择（1.2.2 候选端点） ---------------------------------------

    fn candidate_serials(&self) -> Vec<Option<String>> {
        let current = self.config_snapshot().serial.trim().to_string();
        let current_entry = if current.is_empty() {
            None
        } else {
            Some(current)
        };
        let Some(provider) = self.candidates.clone() else {
            return vec![current_entry];
        };
        let raw = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| provider())) {
            Ok(values) => values,
            Err(_) => {
                eprintln!("[tablet.session] 读取设备候选地址失败，回退当前地址");
                Vec::new()
            }
        };
        let mut result: Vec<Option<String>> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for value in raw {
            let text = value.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let key = text.to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            result.push(Some(text));
        }
        if result.is_empty() {
            vec![current_entry]
        } else {
            result.truncate(8);
            result
        }
    }

    fn select_device(&self) -> Result<Arc<dyn AdbApi>, SessionError> {
        self.ensure_starting("选默认设备")?;
        let base = self.adb.clone();
        base.version()?;
        let config = self.config_snapshot();
        let serial = config.serial.trim().to_string();
        let serial_ref = if serial.is_empty() {
            None
        } else {
            Some(serial.as_str())
        };
        match base.select_device(serial_ref, config.prefer_usb) {
            Ok(entry) => Ok(base.with_serial(&entry.serial)),
            Err(error) if error.code == AdbErrorCode::DeviceOffline => {
                eprintln!(
                    "[tablet.session] 暂无可用设备，{}s 内每 {}s 重试",
                    C::INITIAL_RETRY_WINDOW_S,
                    C::INITIAL_RETRY_INTERVAL_S
                );
                let cancel = || self.closing.load(Ordering::SeqCst);
                let entry = base.wait_for_device(C::INITIAL_RETRY_WINDOW_S, &cancel)?;
                Ok(base.with_serial(&entry.serial))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// 只认注册表给出的地址；不再接受"唯一在线设备"兜底。
    fn select_candidate(&self, serial: &Option<String>) -> Result<Arc<dyn AdbApi>, SessionError> {
        let Some(serial) = serial.as_deref() else {
            return self.select_device();
        };
        if self.candidates.is_none() {
            return self.select_device();
        }
        self.adb.version()?;
        let entries = self.adb.devices()?;
        let found = entries.iter().find(|entry| entry.serial == serial);
        if let Some(entry) = found {
            if entry.online() {
                return Ok(self.adb.with_serial(&entry.serial));
            }
        }
        if !looks_wireless(serial) {
            let detail = match found {
                Some(entry) => format!("USB 设备 {serial} 当前状态是 {}", entry.state),
                None => format!("USB 设备 {serial} 未出现在 adb devices"),
            };
            return Err(SessionError::offline(detail));
        }
        let (host, port) = crate::tablet::locator::split_address(serial);
        if !host.is_empty()
            && !crate::tablet::locator::tcp_open(&host, port, CANDIDATE_PROBE_TIMEOUT_S)
        {
            return Err(SessionError::offline(format!(
                "{serial} 的 TCP 端口不可达（已跳过，避免等待 adb connect 超时）"
            )));
        }
        if let Err(error) = self.adb.connect(serial, CANDIDATE_CONNECT_TIMEOUT_S) {
            return Err(SessionError::offline(format!(
                "候选地址 {serial} 连接失败：{}",
                error.detail
            )));
        }
        let entries = self.adb.devices()?;
        let found = entries.iter().find(|entry| entry.serial == serial);
        if let Some(entry) = found {
            if entry.online() {
                return Ok(self.adb.with_serial(&entry.serial));
            }
        }
        let state = match found {
            Some(entry) => entry.state.clone(),
            None => "未出现在 adb devices".to_string(),
        };
        Err(SessionError::offline(format!(
            "候选地址 {serial} 连接后状态为 {state}（若为 unauthorized，请在平板上允许 USB 调试）"
        )))
    }

    fn connect_candidate(&self) -> Result<DeviceFacts, SessionError> {
        let serials = self.candidate_serials();
        let mut errors: Vec<SessionError> = Vec::new();
        for serial in &serials {
            // 每个候选地址都要单独看取消标志：一台上限 8 个候选，每个候选
            // 最坏要花 probe+connect 秒级，用户点了取消不能还往下试。
            self.ensure_starting("候选探测")?;
            let selected = match self.select_candidate(serial) {
                Ok(adb) => adb,
                Err(error) => {
                    if error.code() != ErrorCode::DeviceOffline {
                        return Err(error);
                    }
                    if serials.len() > 1 {
                        eprintln!(
                            "[tablet.session] 候选 {} 不可用：{}",
                            serial.as_deref().unwrap_or("默认设备"),
                            error.detail()
                        );
                    }
                    errors.push(error);
                    self.set_dev_adb(None);
                    continue;
                }
            };
            self.set_dev_adb(Some(selected.clone()));
            let facts = match self.read_facts(&selected) {
                Ok(facts) => facts,
                Err(error) => {
                    if error.code() != ErrorCode::DeviceOffline {
                        return Err(error);
                    }
                    errors.push(error);
                    self.set_dev_adb(None);
                    continue;
                }
            };
            let mut verified = true;
            let verify = self
                .verify_facts
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            if let Some(verify) = verify {
                let serial_text = selected.serial();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    verify(&serial_text, &facts)
                }));
                match outcome {
                    Ok(true) => {}
                    Ok(false) => {
                        verified = false;
                        if errors.is_empty() {
                            errors.push(SessionError::offline(format!(
                                "{serial_text} 的连接结果不是注册表里的同一台设备"
                            )));
                        }
                    }
                    Err(_) => {
                        verified = false;
                        errors.push(SessionError::offline(format!(
                            "{serial_text} 身份校验异常：校验器 panic"
                        )));
                    }
                }
            }
            if !verified {
                self.set_dev_adb(None);
                continue;
            }
            return Ok(facts);
        }
        if let Some(last) = errors.last() {
            let detail = if errors.len() > 1 {
                format!("{}（已尝试 {} 个地址）", last.detail(), errors.len())
            } else {
                last.detail().to_string()
            };
            return Err(SessionError::offline(detail));
        }
        Err(SessionError::offline("没有可用的设备地址"))
    }

    /// 读取 DeviceFacts（附录 C.2 的命令集合；adb 必须已绑定 serial）。
    fn read_facts(&self, adb: &Arc<dyn AdbApi>) -> Result<DeviceFacts, SessionError> {
        let serial = adb.serial();
        if serial.trim().is_empty() {
            return Err(SessionError::offline("read_facts 需要一个已绑定的 serial"));
        }
        let model = adb.shell(&["getprop", "ro.product.model"], C::ADB_SHELL_TIMEOUT)?;
        let hardware_serial = clean_serial(&adb.shell(
            &["getprop", "ro.serialno"],
            C::ADB_SHELL_TIMEOUT,
        )?);
        let hardware_serial = if hardware_serial.is_empty() {
            serial.clone()
        } else {
            hardware_serial
        };
        let size_text = adb.shell(&["wm", "size"], C::ADB_SHELL_TIMEOUT)?;
        let (natural_w, natural_h) = parse_wm_size(&size_text)?;
        let dumpsys = adb.shell(&["dumpsys", "input"], C::ADB_SHELL_TIMEOUT)?;
        let pointer_speed = parse_pointer_speed(&adb.shell(
            &["settings", "get", "system", "pointer_speed"],
            C::ADB_SHELL_TIMEOUT,
        )?);
        let orientation = parse_orientation(&dumpsys);
        let (width, height) = oriented_size(natural_w, natural_h, orientation);
        let model = model.trim();
        Ok(DeviceFacts {
            serial,
            model: if model.is_empty() {
                "unknown".to_string()
            } else {
                model.to_string()
            },
            width,
            height,
            orientation,
            xscale: parse_xscale(&dumpsys),
            pointer_speed,
            hardware_serial,
        })
    }

    /// 附录 C.2 的诊断项（全部 best-effort；历史：1.x 由 tools/probe.py 消费）。
    pub fn probe_diagnostics(&self, adb: &Arc<dyn AdbApi>) -> Vec<(String, String)> {
        let checks: [(&str, &[&str]); 5] = [
            ("density", &["wm", "density"]),
            ("pointer_speed", &["settings", "get", "system", "pointer_speed"]),
            ("uhid_groups", &["id"]),
            ("uhid_open", &["exec 3<>/dev/uhid && echo OPEN_OK"]),
            (
                "input_scales",
                &["dumpsys input | grep -E 'Pointer Speed:|XScale:|YScale:'"],
            ),
        ];
        let mut out = Vec::new();
        for (key, args) in checks {
            let value = match adb.shell(args, C::ADB_SHELL_TIMEOUT) {
                Ok(text) => text.trim().to_string(),
                Err(error) => format!("<失败: {}>", error.detail),
            };
            out.push((key.to_string(), value));
        }
        out
    }

    // -- pointer_speed / 熄屏超时 -----------------------------------------

    fn normalize_pointer_speed(&self, adb: &Arc<dyn AdbApi>) -> Result<(), SessionError> {
        let original = parse_pointer_speed(&adb.shell(
            &["settings", "get", "system", "pointer_speed"],
            C::ADB_SHELL_TIMEOUT,
        )?);
        self.set_pointer_original(Some(original));
        let serial = self.facts_clone().map(|facts| facts.serial).unwrap_or_default();
        self.save_pointer_state(&serial, original as i64);
        if !adb.shell_ok(
            &["settings", "put", "system", "pointer_speed", "0"],
            C::ADB_SHELL_TIMEOUT,
        ) {
            return Err(SessionError::offline(
                "无法把 pointer_speed 归零（settings put 失败）",
            ));
        }
        let actual = parse_pointer_speed(&adb.shell(
            &["settings", "get", "system", "pointer_speed"],
            C::ADB_SHELL_TIMEOUT,
        )?);
        if actual != 0 {
            return Err(SessionError::offline(format!(
                "pointer_speed 归零后仍为 {actual}"
            )));
        }
        eprintln!("[tablet.session] pointer_speed: {original} → 0（退出时还原）");
        Ok(())
    }

    fn restore_pointer_speed(&self) {
        let target = self.pointer_original.lock().ok().and_then(|guard| *guard);
        let Some(target) = target else {
            return;
        };
        let Some(adb) = self.dev_adb_clone() else {
            return;
        };
        if target == 0 {
            self.store.clear(POINTER_SPEED_STATE);
            return;
        }
        if adb.shell_ok(
            &["settings", "put", "system", "pointer_speed", &target.to_string()],
            C::ADB_SHELL_TIMEOUT,
        ) {
            eprintln!("[tablet.session] pointer_speed 已还原为 {target}");
            self.store.clear(POINTER_SPEED_STATE);
        } else {
            eprintln!(
                "[tablet.session] pointer_speed 还原失败（原值 {target}），保留状态待下次启动"
            );
        }
    }

    fn keep_screen_on(&self, adb: &Arc<dyn AdbApi>) {
        if !self.config_snapshot().keep_screen_on {
            return;
        }
        let raw = match adb.shell(&["settings", "get", "system", "screen_off_timeout"], C::ADB_SHELL_TIMEOUT) {
            Ok(text) => text.trim().to_string(),
            Err(error) => {
                eprintln!(
                    "[tablet.session] 读取 screen_off_timeout 失败（不致命）：{}",
                    error.detail
                );
                return;
            }
        };
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            eprintln!(
                "[tablet.session] screen_off_timeout 原值不可读（{raw:?}），保持系统设置不动"
            );
            return;
        }
        let original = match raw.parse::<i64>() {
            Ok(value) => value,
            Err(_) => return,
        };
        if original >= SCREEN_TIMEOUT_KEEP_MS {
            return;
        }
        let serial = self.facts_clone().map(|facts| facts.serial).unwrap_or_default();
        // 先落盘再修改：put 之后崩溃时下次启动仍能还原。
        self.save_screen_state(&serial, original);
        if !adb.shell_ok(
            &[
                "settings",
                "put",
                "system",
                "screen_off_timeout",
                &SCREEN_TIMEOUT_KEEP_MS.to_string(),
            ],
            C::ADB_SHELL_TIMEOUT,
        ) {
            self.store.clear(SCREEN_TIMEOUT_STATE);
            eprintln!("[tablet.session] screen_off_timeout 调大失败（不致命）");
            return;
        }
        self.set_screen_original(Some(original));
        eprintln!(
            "[tablet.session] screen_off_timeout: {original} → {SCREEN_TIMEOUT_KEEP_MS}（退出时还原）"
        );
    }

    fn restore_keep_screen_on(&self) {
        let original = self.screen_original.lock().ok().and_then(|guard| *guard);
        let Some(original) = original else {
            return;
        };
        let Some(adb) = self.dev_adb_clone() else {
            return;
        };
        if adb.shell_ok(
            &["settings", "put", "system", "screen_off_timeout", &original.to_string()],
            C::ADB_SHELL_TIMEOUT,
        ) {
            self.store.clear(SCREEN_TIMEOUT_STATE);
            self.set_screen_original(None);
            eprintln!("[tablet.session] screen_off_timeout 已还原为 {original}");
        } else {
            eprintln!(
                "[tablet.session] screen_off_timeout 还原失败（原值 {original}），保留状态待下次启动"
            );
        }
    }

    /// pointer_speed 复查：不是 0 就重新归零（F4）。
    fn ensure_pointer_speed(&self) -> Result<bool, SessionError> {
        let Some(adb) = self.dev_adb_clone() else {
            return Ok(false);
        };
        let outcome = (|| -> Result<bool, SessionError> {
            let current = parse_pointer_speed(&adb.shell(
                &["settings", "get", "system", "pointer_speed"],
                C::ADB_SHELL_TIMEOUT,
            )?);
            if current == 0 {
                return Ok(true);
            }
            eprintln!("[tablet.session] pointer_speed 被改为 {current}，重新归零");
            let _ = adb.shell_ok(
                &["settings", "put", "system", "pointer_speed", "0"],
                C::ADB_SHELL_TIMEOUT,
            );
            let actual = parse_pointer_speed(&adb.shell(
                &["settings", "get", "system", "pointer_speed"],
                C::ADB_SHELL_TIMEOUT,
            )?);
            Ok(actual == 0)
        })();
        match outcome {
            Ok(value) => Ok(value),
            Err(error) => {
                eprintln!(
                    "[tablet.session] pointer_speed 复查失败: {}",
                    error.detail()
                );
                Ok(false)
            }
        }
    }

    fn refresh_facts(&self) -> Result<DeviceFacts, SessionError> {
        let _guard = self.lock_lifecycle();
        let Some(adb) = self.dev_adb_clone() else {
            return Err(SessionError::offline("尚未 start，没有可刷新的设备事实"));
        };
        let facts = self.read_facts(&adb)?;
        self.set_facts(Some(facts.clone()));
        eprintln!(
            "[tablet.session] 设备事实已刷新: {}x{} orientation={}",
            facts.width, facts.height, facts.orientation
        );
        Ok(facts)
    }

    // -- 握手与通道 --------------------------------------------------------

    /// 握手一条 scrcpy 通道。
    ///
    /// `expect_dummy` 只在**第一条**被接受的连接上为 true：scrcpy 5.0.1 的
    /// `send_dummy_byte` 只发给第一个 accept 的 socket（真机实测：audio=true 时
    /// 音频通道先拿到 `00`，随后的控制通道没有任何前缀字节，直接就是控制消息流）。
    fn handshake_socket(
        &self,
        label: &str,
        expect_dummy: bool,
    ) -> Result<Arc<dyn SocketIo>, SessionError> {
        let port = self
            .current_port()
            .ok_or_else(|| SessionError::ForwardFailed("尚未分配转发端口".to_string()))?;
        let addr = format!("127.0.0.1:{port}");
        let deadline =
            Instant::now() + Duration::from_secs_f64(self.config_snapshot().handshake_retry_timeout.max(0.0));
        loop {
            if self.closing.load(Ordering::SeqCst) {
                return Err(SessionError::handshake("启动已取消"));
            }
            let attempt = self.try_handshake(&addr, label, port, expect_dummy);
            match attempt {
                Ok(socket) => {
                    eprintln!("[tablet.session] {label}通道已建立（tcp:{port}）");
                    return Ok(socket);
                }
                Err(HandshakeFailure::Protocol(error)) => return Err(error),
                Err(HandshakeFailure::Retry(message)) => {
                    if Instant::now() >= deadline {
                        return Err(SessionError::handshake(format!(
                            "连接 {label}通道 127.0.0.1:{port} 失败（已重试 {}s）: {message}",
                            self.config_snapshot().handshake_retry_timeout
                        )));
                    }
                    eprintln!("[tablet.session] 服务端尚未就绪，重试{label}握手: {message}");
                    self.sleeper.sleep(C::HANDSHAKE_RETRY_INTERVAL_S);
                }
            }
        }
    }

    fn try_handshake(
        &self,
        addr: &str,
        label: &str,
        _port: u16,
        expect_dummy: bool,
    ) -> Result<Arc<dyn SocketIo>, HandshakeFailure> {
        let socket = self
            .socket_factory
            .connect(addr, Duration::from_secs_f64(C::SOCKET_CONNECT_TIMEOUT))
            .map_err(HandshakeFailure::Retry)?;
        if let Err(error) = socket
            .set_read_timeout(Some(Duration::from_secs_f64(C::SOCKET_HANDSHAKE_TIMEOUT)))
        {
            let _ = socket.shutdown();
            return Err(HandshakeFailure::Retry(error.to_string()));
        }
        if expect_dummy {
            let mut dummy = [0u8; 1];
            if let Err(error) = recv_exactly(&socket, &mut dummy) {
                let _ = socket.shutdown();
                return Err(HandshakeFailure::Retry(error.to_string()));
            }
            if dummy[..] != C::HANDSHAKE_DUMMY[..] {
                let _ = socket.shutdown();
                return Err(HandshakeFailure::Protocol(SessionError::handshake(format!(
                    "{label}通道 dummy 字节不符: {:02x}（期望 {}）",
                    dummy[0],
                    C::HANDSHAKE_DUMMY[0]
                ))));
            }
        }
        let send_timeout = Some(Duration::from_secs_f64(C::SOCKET_SEND_TIMEOUT));
        let _ = socket.set_read_timeout(send_timeout);
        let _ = socket.set_write_timeout(send_timeout);
        Ok(socket)
    }

    fn connect_socket(&self) -> Result<(), SessionError> {
        // dummy 字节只出现在第一条连接上：音频模式下第一条是音频通道，纯控制模式下
        // 第一条就是控制通道。把控制通道也按 dummy 握手，音频模式会白等一整轮重试，
        // 最终误判"音频通道不可用"。
        let audio_active = self.audio_active.load(Ordering::SeqCst);
        let mut audio: Option<Arc<dyn SocketIo>> = None;
        if audio_active {
            audio = Some(self.handshake_socket("音频", true)?);
        }
        let control = match self.handshake_socket("控制", !audio_active) {
            Ok(socket) => socket,
            Err(error) => {
                if let Some(socket) = &audio {
                    let _ = socket.shutdown();
                }
                return Err(error);
            }
        };
        if let Ok(mut guard) = self.sock.lock() {
            *guard = Some(control);
        }
        self.sock_broken.store(false, Ordering::SeqCst);
        if let Some(audio_socket) = audio {
            let _ = audio_socket
                .set_read_timeout(Some(Duration::from_secs_f64(AUDIO_SOCKET_POLL_S)));
            let relay = AudioRelay::new(
                audio_socket.clone(),
                self.audio_sink_port,
                self.socket_factory.clone(),
            );
            relay.start();
            if let Ok(mut guard) = self.audio_sock.lock() {
                *guard = Some(audio_socket);
            }
            if let Ok(mut guard) = self.audio_relay.lock() {
                *guard = Some(relay.clone());
            }
            if relay.degraded() {
                eprintln!("[tablet.session] 音频播放器不可用，本次连接只保留控制通道");
            }
        }
        eprintln!(
            "[tablet.session] 控制通道已建立（tcp:{}）",
            self.current_port().unwrap_or(0)
        );
        Ok(())
    }

    fn close_socket(&self) {
        let audio = self.audio_sock.lock().ok().and_then(|mut guard| guard.take());
        if let Some(socket) = &audio {
            let _ = socket.shutdown();
        }
        let relay = self
            .audio_relay
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(relay) = relay {
            relay.stop();
        }
        let stop = self
            .receiver_stop
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(stop) = &stop {
            stop.store(true, Ordering::SeqCst);
        }
        let socket = self.sock.lock().ok().and_then(|mut guard| guard.take());
        if let Some(socket) = &socket {
            if let Err(error) = socket.shutdown() {
                eprintln!("[tablet.session] socket 关闭时已失效: {error}");
            }
        }
        let handle = self.receiver.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            if handle.thread().id() != thread::current().id() {
                join_thread(handle, Duration::from_secs(2));
            }
        }
    }

    fn start_receiver(&self) {
        if self
            .receiver
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let Some(socket) = self.current_socket() else {
            return;
        };
        let Some(shared) = self.shared() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.receiver_stop.lock() {
            *guard = Some(stop.clone());
        }
        let thread_socket = socket.clone();
        let spawn = thread::Builder::new()
            .name("padlend-receiver".to_string())
            .spawn(move || shared.receiver_loop(thread_socket, stop));
        match spawn {
            Ok(handle) => {
                if let Ok(mut guard) = self.receiver.lock() {
                    *guard = Some(handle);
                }
                eprintln!("[tablet.session] 控制通道接收监听已启动");
            }
            Err(error) => eprintln!("[tablet.session] 接收线程启动失败: {error}"),
        }
    }

    fn receiver_loop(&self, socket: Arc<dyn SocketIo>, stop: Arc<AtomicBool>) {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 65_536];
        while !stop.load(Ordering::SeqCst) {
            let read = socket.read(&mut chunk);
            match read {
                Ok(0) => {
                    self.mark_receiver_broken(&socket, "对端关闭了控制通道");
                    return;
                }
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(error) => {
                    self.mark_receiver_broken(&socket, &format!("socket 读失败: {error}"));
                    return;
                }
            }
            // 2.2.0：用 offset 游标解析，循环内不再 drain 整个缓冲区（原实现
            // 每条消息都把剩余字节前移，连续到达时最坏 O(n²)）。
            let mut offset = 0usize;
            loop {
                if offset >= buffer.len() || stop.load(Ordering::SeqCst) {
                    break;
                }
                match crate::tablet::protocol::deserialize_device_message(&buffer[offset..]) {
                    Ok((None, _)) => break,
                    Ok((Some(message), consumed)) => {
                        let consumed = consumed.min(buffer.len() - offset);
                        if consumed == 0 {
                            break;
                        }
                        offset += consumed;
                        self.handle_device_message(&message);
                    }
                    Err(error) => {
                        self.mark_receiver_broken(
                            &socket,
                            &format!("设备消息解析失败: {error}"),
                        );
                        return;
                    }
                }
            }
            if offset > 0 {
                if offset >= buffer.len() {
                    buffer.clear();
                } else {
                    buffer.drain(..offset);
                }
            }
        }
        eprintln!("[tablet.session] 控制通道接收监听已停止");
    }

    fn mark_receiver_broken(&self, socket: &Arc<dyn SocketIo>, detail: &str) {
        let stop_flag = self
            .receiver_stop
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(stop) = stop_flag {
            if stop.load(Ordering::SeqCst) {
                return;
            }
        }
        let is_current = self
            .current_socket()
            .map(|current| Arc::ptr_eq(&current, socket))
            .unwrap_or(false);
        if !is_current {
            return;
        }
        self.sock_broken.store(true, Ordering::SeqCst);
        self.set_last_error(detail);
        eprintln!("[tablet.session] 控制通道接收中断: {detail}");
    }

    fn handle_device_message(&self, message: &crate::tablet::protocol::DeviceMessage) {
        use crate::tablet::protocol::{
            DEVICE_MSG_ACK_CLIPBOARD, DEVICE_MSG_CLIPBOARD, DEVICE_MSG_UHID_OUTPUT,
        };
        match message.msg_type {
            DEVICE_MSG_CLIPBOARD => {
                let callback = self
                    .clipboard_callback
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone());
                if let Some(callback) = callback {
                    let text = message.text.clone();
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(text)))
                        .is_err()
                    {
                        eprintln!("[tablet.session] 平板剪贴板回调失败（panic）");
                    }
                }
            }
            DEVICE_MSG_ACK_CLIPBOARD => {
                eprintln!(
                    "[tablet.session] 平板剪贴板 ACK sequence={}",
                    message.sequence
                );
            }
            DEVICE_MSG_UHID_OUTPUT => {
                eprintln!(
                    "[tablet.session] 忽略 UHID 输出 id={} size={}",
                    message.uhid_id,
                    message.data.len()
                );
            }
            _ => {}
        }
    }

    // -- UHID --------------------------------------------------------------

    fn create_uhid(&self) -> Result<(), SessionError> {
        let mouse = crate::tablet::protocol::uhid_create(
            C::UHID_MOUSE_ID as i64,
            C::HID_VENDOR_ID as i64,
            C::HID_MOUSE_PRODUCT_ID as i64,
            C::HID_MOUSE_NAME,
            C::HID_MOUSE_DESCRIPTOR,
        )
        .map_err(|error| SessionError::UhidUnavailable(format!("鼠标 UHID_CREATE 编码失败: {error}")))?;
        let keyboard = crate::tablet::protocol::uhid_create(
            C::UHID_KEYBOARD_ID as i64,
            C::HID_VENDOR_ID as i64,
            C::HID_KEYBOARD_PRODUCT_ID as i64,
            C::HID_KEYBOARD_NAME,
            C::HID_KEYBOARD_DESCRIPTOR,
        )
        .map_err(|error| {
            SessionError::UhidUnavailable(format!("键盘 UHID_CREATE 编码失败: {error}"))
        })?;
        self.send_quiet(&mouse, true)?;
        self.send_quiet(&keyboard, true)?;
        self.uhid_created.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.last_mouse_buttons.lock() {
            *guard = None;
        }
        eprintln!(
            "[tablet.session] UHID 已创建：鼠标 id=0x{:04X}、键盘 id=0x{:04X}",
            C::UHID_MOUSE_ID, C::UHID_KEYBOARD_ID
        );
        Ok(())
    }

    fn destroy_uhid(&self) {
        if !self.uhid_created.load(Ordering::SeqCst) || self.current_socket().is_none() {
            self.uhid_created.store(false, Ordering::SeqCst);
            return;
        }
        for uhid_id in [C::UHID_KEYBOARD_ID, C::UHID_MOUSE_ID] {
            let payload = match crate::tablet::protocol::uhid_destroy(uhid_id as i64) {
                Ok(payload) => payload,
                Err(error) => {
                    eprintln!("[tablet.session] UHID_DESTROY 0x{uhid_id:04X} 编码失败: {error}");
                    continue;
                }
            };
            if let Err(error) = self.send_quiet(&payload, false) {
                eprintln!(
                    "[tablet.session] UHID_DESTROY 0x{uhid_id:04X} 失败: {}",
                    error.detail()
                );
            }
        }
        self.uhid_created.store(false, Ordering::SeqCst);
    }

    // -- 发送 --------------------------------------------------------------

    fn ensure_connected(&self) -> Result<(), SessionError> {
        if self.socket_ok() {
            Ok(())
        } else {
            Err(SessionError::SocketBroken(
                "控制通道未建立或已断开".to_string(),
            ))
        }
    }

    /// 直接写一条报文（UHID 创建/销毁用；不在队列里排队）。
    fn send_quiet(&self, payload: &[u8], raise_on_broken: bool) -> Result<bool, SessionError> {
        let Some(socket) = self.current_socket() else {
            if raise_on_broken {
                return Err(SessionError::SocketBroken("socket 已关闭".to_string()));
            }
            return Ok(false);
        };
        match socket.write_all(payload) {
            Ok(()) => {
                self.failures.store(0, Ordering::SeqCst);
                self.sent.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            }
            Err(error) => {
                let count = self.failures.fetch_add(1, Ordering::SeqCst) + 1;
                self.set_last_error(&format!("socket 写失败: {error}"));
                eprintln!("[tablet.session] socket 写失败（连续 {count} 次）: {error}");
                if count >= C::CONSECUTIVE_WRITE_FAILURES {
                    self.sock_broken.store(true, Ordering::SeqCst);
                    if raise_on_broken {
                        return Err(SessionError::SocketBroken(format!(
                            "连续 {count} 次写失败"
                        )));
                    }
                }
                Ok(false)
            }
        }
    }

    /// 把报文压进有界队列（锁内只做搬指针；满了丢最旧一条）。
    fn enqueue(&self, payload: Vec<u8>) {
        if let Ok(mut guard) = self.queue.lock() {
            if guard.len() >= C::SEND_QUEUE_MAXLEN {
                guard.pop_front();
            }
            guard.push_back(payload);
            drop(guard);
            // 发送线程在 queue_cv 上等空队列；入队后叫醒它，不再靠 1ms 轮询。
            self.queue_cv.notify_one();
        }
    }

    /// 发送线程的空队列等待（2.2.0）。
    ///
    /// 调用前 sender_frame 已把队列取空；这里在锁内再确认一次，避免
    /// “取空 → 入队 → notify” 之间丢唤醒。stop 由 stop_flusher 置位并
    /// notify_all，最长 1s 的 wait_timeout 只是兜底。
    fn wait_for_sender_work(&self, stop: &AtomicBool) {
        let guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if stop.load(Ordering::SeqCst) || !guard.is_empty() {
            return;
        }
        let _ = self.queue_cv.wait_timeout(guard, Duration::from_secs(1));
    }

    /// 在锁外批量写出队列（最多 MAX_BATCH 条）；返回写成功条数。
    ///
    /// 2.2.0：同一帧的所有报文拼进复用缓冲后一次 write_all，避免每帧
    /// new 一个 Vec<Vec<u8>> 并为每条报文各做一次系统调用。
    fn sender_frame(&self) -> Result<usize, SessionError> {
        let mut batch: [Option<Vec<u8>>; C::MAX_BATCH] = std::array::from_fn(|_| None);
        let mut count = 0usize;
        if let Ok(mut guard) = self.queue.lock() {
            while count < C::MAX_BATCH {
                match guard.pop_front() {
                    Some(item) => {
                        batch[count] = Some(item);
                        count += 1;
                    }
                    None => break,
                }
            }
        }
        if count == 0 {
            return Ok(0);
        }
        let Some(socket) = self.current_socket() else {
            // 与 Python 的 _send_quiet 一致：socket 已关闭时丢弃报文，不算失败。
            return Ok(0);
        };
        let mut buffer = self.send_buffer.lock().unwrap_or_else(|error| error.into_inner());
        buffer.clear();
        for item in batch.iter().take(count).flatten() {
            buffer.extend_from_slice(item);
        }
        match socket.write_all(&buffer) {
            Ok(()) => {
                self.failures.store(0, Ordering::SeqCst);
                self.sent.fetch_add(count as i64, Ordering::SeqCst);
                Ok(count)
            }
            Err(error) => {
                let failures = self.failures.fetch_add(1, Ordering::SeqCst) + 1;
                self.set_last_error(&format!("socket 写失败: {error}"));
                eprintln!("[tablet.session] socket 写失败（连续 {failures} 次）: {error}");
                if failures >= C::CONSECUTIVE_WRITE_FAILURES {
                    self.sock_broken.store(true, Ordering::SeqCst);
                    return Err(SessionError::SocketBroken(format!(
                        "连续 {failures} 次写失败"
                    )));
                }
                Ok(0)
            }
        }
    }

    fn start_flusher(&self) {
        if !self.auto_flush.load(Ordering::SeqCst) {
            return;
        }
        if self
            .flusher
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let Some(shared) = self.shared() else {
            return;
        };
        let stop = Arc::clone(&self.flusher_stop);
        stop.store(false, Ordering::SeqCst);
        let spawn = thread::Builder::new()
            .name("padlend-sender".to_string())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = shared.sender_frame();
                    shared.wait_for_sender_work(&stop);
                }
            });
        match spawn {
            Ok(handle) => {
                if let Ok(mut guard) = self.flusher.lock() {
                    *guard = Some(handle);
                }
            }
            Err(error) => eprintln!("[tablet.session] 发送线程启动失败: {error}"),
        }
    }

    fn stop_flusher(&self) {
        self.flusher_stop.store(true, Ordering::SeqCst);
        // 发送线程可能正阻塞在 queue_cv 上，必须叫醒它才能及时 join。
        self.queue_cv.notify_all();
        let handle = self.flusher.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            if handle.thread().id() != thread::current().id() {
                join_thread(handle, Duration::from_secs(1));
            }
        }
    }

    // -- 朝向监听 ----------------------------------------------------------

    fn start_orientation_watch(&self) {
        if !self.config_snapshot().watch_orientation {
            return;
        }
        if self
            .orientation
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let Some(device) = self.dev_adb_clone() else {
            return;
        };
        let Some(shared) = self.shared() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.orientation_stop.lock() {
            *guard = Some(stop.clone());
        }
        let spawn = thread::Builder::new()
            .name("padlend-orientation".to_string())
            .spawn(move || shared.orientation_loop(device, stop));
        match spawn {
            Ok(handle) => {
                if let Ok(mut guard) = self.orientation.lock() {
                    *guard = Some(handle);
                }
                let serial = self
                    .facts_clone()
                    .map(|facts| facts.serial)
                    .unwrap_or_else(|| "?".to_string());
                eprintln!("[tablet.session] 朝向监听已启动 serial={serial}");
            }
            Err(error) => eprintln!("[tablet.session] 朝向监听启动失败: {error}"),
        }
    }

    fn stop_orientation_watch(&self) {
        let stop = self
            .orientation_stop
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(stop) = &stop {
            stop.store(true, Ordering::SeqCst);
        }
        let handle = self.orientation.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            if handle.thread().id() != thread::current().id() {
                join_thread(handle, Duration::from_secs(2));
            }
        }
    }

    // -- adb 心跳 ----------------------------------------------------------

    /// 启动 adb 心跳线程（2.0.4）。
    ///
    /// 路由线程原先每 10s 直接跑一次 `adb shell settings get system pointer_speed`。
    /// 平板的 system_server 一忙（实测 5s 超时、`settings get` 偶尔要 17s），路由线程
    /// 就被卡住，那段时间不 drain 发送队列 —— 平板上的指针冻住再跳一段。把 adb IO
    /// 全部挪到这里，路由线程只读 `link_ok` / `pointer_ok`。
    fn start_link_watch(&self) {
        if self.link.lock().map(|guard| guard.is_some()).unwrap_or(false) {
            return;
        }
        let Some(device) = self.dev_adb_clone() else {
            return;
        };
        let Some(shared) = self.shared() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.link_stop.lock() {
            *guard = Some(stop.clone());
        }
        let spawn = thread::Builder::new()
            .name("padlend-adb-link".to_string())
            .spawn(move || shared.link_loop(device, stop));
        match spawn {
            Ok(handle) => {
                if let Ok(mut guard) = self.link.lock() {
                    *guard = Some(handle);
                }
            }
            Err(error) => eprintln!("[tablet.session] adb 心跳线程启动失败: {error}"),
        }
    }

    fn stop_link_watch(&self) {
        let stop = self.link_stop.lock().ok().and_then(|mut guard| guard.take());
        if let Some(stop) = &stop {
            stop.store(true, Ordering::SeqCst);
        }
        let handle = self.link.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            if handle.thread().id() != thread::current().id() {
                join_thread(handle, Duration::from_secs(2));
            }
        }
    }

    /// adb 心跳线程：一条 `echo` 探活，另外每 10s 复查一次 pointer_speed。
    fn link_loop(&self, device: Arc<dyn AdbApi>, stop: Arc<AtomicBool>) {
        let mut last_pointer = Instant::now() - Duration::from_secs(60);
        while !stop.load(Ordering::SeqCst) {
            // 心跳只是等下一轮探活，不参与朝向唤醒（None）。
            if self.sleep_interruptible(LINK_PROBE_INTERVAL_S, &stop, None) {
                break;
            }
            if self.closing.load(Ordering::SeqCst) {
                continue;
            }
            let ok = device
                .shell(&["echo", "link-probe"], LINK_PROBE_TIMEOUT_S)
                .is_ok();
            self.link_ok.store(ok, Ordering::SeqCst);
            if !ok {
                continue;
            }
            if last_pointer.elapsed().as_secs_f64() < C::POINTER_SPEED_POLL_S {
                continue;
            }
            last_pointer = Instant::now();
            let restored = self.ensure_pointer_speed().unwrap_or(false);
            self.pointer_ok.store(restored, Ordering::SeqCst);
        }
    }

    /// 朝向轮询线程。
    ///
    /// 2.0.2：从「每 1s 跑一次完整 dumpsys input」改成「每 2s 只问显示旋转」。
    /// 2.0.5：那一步问错了地方 —— `dumpsys window displays` 实测 851ms 起步、
    /// 忙时 4.7–13.4s，而 `dumpsys input` 同样信息只要 65–150ms。周期也从 2s 拉到
    /// 15s，拿不到结果时翻倍退避（15 → 30 → 60s），避免平板忙的时候把查询堆成一串。
    /// 2.0.6：查询改成直接跑 `dumpsys input`（不再经过 `sh -c`，见 ORIENTATION_QUERY_TIMEOUT_S），
    /// 并且接管期间也查（60s 一次）；交还时由 orientation_wake 立刻叫醒补一次。
    fn orientation_loop(&self, device: Arc<dyn AdbApi>, stop: Arc<AtomicBool>) {
        let mut interval = ORIENTATION_POLL_S;
        let mut was_capturing = false;
        while !stop.load(Ordering::SeqCst) {
            // 交还控制权时由 set_capturing(false) 抬旗标，这里只消费自己这一份。
            if self.sleep_interruptible(interval, &stop, Some(&self.orientation_wake)) {
                break;
            }
            // 2.0.2 曾经在接管期间完全不查询（dumpsys 会在 system_server 主线程上排队，
            // 输入派发跟着卡几十毫秒）。但那样用户拿着鼠标在平板上转屏就永远发现不了 ——
            // 2.0.6 改成接管期间每 60s 才问一次，交还时由 orientation_wake 立刻补一次。
            let capturing = self.capturing.load(Ordering::SeqCst);
            if capturing != was_capturing {
                was_capturing = capturing;
                interval = orientation_interval(capturing);
            }
            match device.shell(&["dumpsys", "input"], ORIENTATION_QUERY_TIMEOUT_S) {
                Ok(text) if text.contains("orientation=") => {
                    self.apply_orientation(&text);
                    interval = orientation_interval(self.capturing.load(Ordering::SeqCst));
                }
                Ok(_) => {
                    eprintln!("[tablet.session] 朝向查询里没有 Viewport 行");
                    interval = (interval * 2.0)
                        .min(orientation_interval(self.capturing.load(Ordering::SeqCst)) * 4.0);
                }
                Err(error) => {
                    if !stop.load(Ordering::SeqCst) {
                        eprintln!("[tablet.session] 朝向查询失败: {}", error.detail);
                    }
                    interval = (interval * 2.0)
                        .min(orientation_interval(self.capturing.load(Ordering::SeqCst)) * 4.0);
                }
            }
        }
        let _ = ORIENTATION_RESTART_S;
        eprintln!("[tablet.session] 朝向监听已停止");
    }

    /// 应用一段 dumpsys input 输出里的朝向（对应 _apply_orientation）。
    fn apply_orientation(&self, text: &str) {
        self.apply_orientation_value(parse_orientation(text));
    }

    /// 应用一个朝向值（0..3）：宽高按朝向重算后广播。
    fn apply_orientation_value(&self, orientation: i32) {
        let Some(old) = self.facts_clone() else {
            return;
        };
        if old.orientation == orientation {
            return;
        }
        let (natural_w, natural_h) = if old.orientation & 1 == 1 {
            (old.height, old.width)
        } else {
            (old.width, old.height)
        };
        let (width, height) = oriented_size(natural_w, natural_h, orientation);
        let new_facts = DeviceFacts {
            width,
            height,
            orientation,
            ..old
        };
        if self.closing.load(Ordering::SeqCst) || !self.started.load(Ordering::SeqCst) {
            return;
        }
        self.set_facts(Some(new_facts.clone()));
        eprintln!(
            "[tablet.session] 设备朝向变化: {}x{} orientation={}",
            new_facts.width, new_facts.height, new_facts.orientation
        );
        let callback = self
            .facts_callback
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(callback) = callback {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(new_facts)
            }))
            .is_err()
            {
                eprintln!("[tablet.session] 朝向回调失败（panic）");
            }
        }
    }

    // -- 健康 / 清理 -------------------------------------------------------

    fn health(&self) -> HealthReport {
        let server_alive = self
            .server_clone()
            .map(|server| server.proc_alive())
            .unwrap_or(false);
        let socket_ok = self.socket_ok();
        let uptime = self
            .started_at
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|instant| instant.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        HealthReport {
            alive: self.started.load(Ordering::SeqCst)
                && socket_ok
                && !self.sock_broken.load(Ordering::SeqCst),
            socket_ok,
            uhid_created: self.uhid_created.load(Ordering::SeqCst),
            server_alive,
            consecutive_failures: self.failures.load(Ordering::SeqCst) as i32,
            last_error: self.last_error(),
            uptime_s: uptime,
            sent: self.sent.load(Ordering::SeqCst),
        }
    }

    /// 退出序列六步（6.1.3）：任何一步失败都不阻断后续步骤。
    fn teardown(&self) {
        self.stop_flusher();
        self.stop_orientation_watch();
        self.destroy_uhid();
        self.close_socket();
        let server = self.server_clone();
        if let Some(server) = &server {
            let _ = server.kill(2.0);
            server.forward_remove();
        }
        self.restore_pointer_speed();
        self.restore_keep_screen_on();
        if let Some(server) = &server {
            if !server.verify_clean(2.0) {
                eprintln!(
                    "[tablet.session] 清理校验失败：{}",
                    server.verify_detail()
                );
                eprintln!(
                    "[tablet.session] {} detail={}",
                    C::EVENT_TABLET_CLEANUP_GHOST,
                    server.verify_detail()
                );
                let adb = self.dev_adb_clone().unwrap_or_else(|| self.adb.clone());
                let _ = adb.shell_ok(&["pkill", "-9", "-f", KILL_PATTERN], C::ADB_SHELL_TIMEOUT);
                if !server.verify_clean(2.0) {
                    eprintln!(
                        "[tablet.session] 幽灵设备仍在：{}",
                        server.verify_detail()
                    );
                }
            }
        }
        if let Ok(mut guard) = self.queue.lock() {
            guard.clear();
        }
        self.uhid_created.store(false, Ordering::SeqCst);
        self.sock_broken.store(false, Ordering::SeqCst);
        self.audio_active.store(false, Ordering::SeqCst);
        self.failures.store(0, Ordering::SeqCst);
        eprintln!("[tablet.session] 连接层已清理");
    }

    // -- 发送入口（公开面走的都是这几个） ---------------------------------

    fn send_mouse(
        &self,
        buttons: i32,
        dx: i32,
        dy: i32,
        wheel: i32,
    ) -> Result<(), SessionError> {
        self.ensure_connected()?;
        let value = buttons & 0x1F;
        let changed = if let Ok(mut guard) = self.last_mouse_buttons.lock() {
            if *guard != Some(value) {
                *guard = Some(value);
                true
            } else {
                false
            }
        } else {
            false
        };
        if changed {
            eprintln!(
                "[tablet.session] tablet.mouse.btn 0x{value:02x} dx={dx} dy={dy}"
            );
        }
        let report = crate::tablet::uhid::mouse_report(buttons, dx, dy, wheel);
        let payload = crate::tablet::protocol::uhid_report(C::UHID_MOUSE_ID as i64, &report)
            .map_err(|error| SessionError::Unexpected(format!("鼠标报告编码失败: {error}")))?;
        self.enqueue(payload);
        Ok(())
    }

    fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), SessionError> {
        self.ensure_connected()?;
        let report = crate::tablet::uhid::keyboard_report(modifier as i32, usages)
            .map_err(|error| SessionError::Unexpected(format!("键盘报告编码失败: {error}")))?;
        let payload = crate::tablet::protocol::uhid_report(C::UHID_KEYBOARD_ID as i64, &report)
            .map_err(|error| SessionError::Unexpected(format!("键盘报告编码失败: {error}")))?;
        self.enqueue(payload);
        Ok(())
    }

    fn send_android_keycode(&self, keycode: i64, down: bool) -> Result<(), SessionError> {
        self.ensure_connected()?;
        let action = if down {
            crate::tablet::protocol::ACTION_DOWN
        } else {
            crate::tablet::protocol::ACTION_UP
        };
        let payload = crate::tablet::protocol::inject_keycode(action, keycode, 0, 0)
            .map_err(|error| SessionError::Unexpected(format!("keycode 编码失败: {error}")))?;
        self.enqueue(payload);
        Ok(())
    }

    /// 操作平板：后退 / 主页 / 最近任务 / 通知栏 / 电源 / 音量。
    ///
    /// 按键走 INJECT_KEYCODE 的 DOWN+UP 对；通知栏走 scrcpy 的 EXPAND_NOTIFICATION_PANEL。
    /// 都不经过键盘转发，所以「转发按键到平板」关掉时快捷键仍然可用。
    fn android_action(
        &self,
        action: crate::tablet::protocol::AndroidAction,
    ) -> Result<(), SessionError> {
        self.ensure_connected()?;
        use crate::tablet::protocol as p;
        let payloads: Vec<Vec<u8>> = match action.keycode() {
            Some(keycode) => vec![
                p::inject_keycode(p::ACTION_DOWN, keycode, 0, 0)
                    .map_err(|error| SessionError::Unexpected(format!("keycode 编码失败: {error}")))?,
                p::inject_keycode(p::ACTION_UP, keycode, 0, 0)
                    .map_err(|error| SessionError::Unexpected(format!("keycode 编码失败: {error}")))?,
            ],
            None => vec![p::expand_notification_panel()],
        };
        for payload in payloads {
            self.enqueue(payload);
        }
        Ok(())
    }

    fn inject_text(&self, text: &str) -> Result<(), SessionError> {
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_connected()?;
        let payload = crate::tablet::protocol::inject_text(text)
            .map_err(|error| SessionError::Unexpected(format!("文本编码失败: {error}")))?;
        self.enqueue(payload);
        Ok(())
    }

    fn set_clipboard(&self, text: &str, sequence: u64) -> Result<(), SessionError> {
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_connected()?;
        let payload = crate::tablet::protocol::set_clipboard(text, sequence)
            .map_err(|error| SessionError::Unexpected(format!("剪贴板编码失败: {error}")))?;
        self.enqueue(payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::adb::{select_device_from, DeviceEntry};
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    // -- 临时目录 ----------------------------------------------------------

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let serial = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "padlend-session-{}-{}-{}",
                std::process::id(),
                tag,
                serial
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // -- 假 adb ------------------------------------------------------------

    #[derive(Debug)]
    struct FakeAdbState {
        devices: Vec<DeviceEntry>,
        shell_map: HashMap<String, String>,
        shell_queue: VecDeque<(String, Result<String, AdbError>)>,
        shell_ok_map: HashMap<String, bool>,
        shell_ok_queue: VecDeque<(String, bool)>,
        forwards: VecDeque<bool>,
        forward_calls: Vec<u16>,
        forward_removes: Vec<u16>,
        pushes: usize,
        popen: Vec<String>,
        calls: Vec<String>,
        md5: Option<String>,
        version_ok: bool,
        wait_error: Option<String>,
        /// `getprop ro.build.version.sdk` 的应答（默认按 Android 14 上报）。
        sdk: String,
    }

    #[derive(Debug, Clone)]
    struct FakeAdb {
        state: Arc<Mutex<FakeAdbState>>,
        serial: String,
    }

    impl FakeAdb {
        fn new(devices: Vec<DeviceEntry>) -> Self {
            let mut shell_map = HashMap::new();
            shell_map.insert(
                "getprop ro.product.model".to_string(),
                "Pad 7".to_string(),
            );
            shell_map.insert("getprop ro.serialno".to_string(), "HW-SERIAL".to_string());
            shell_map.insert(
                "wm size".to_string(),
                "Physical size: 1600x2560".to_string(),
            );
            shell_map.insert(
                "dumpsys input".to_string(),
                "  Viewport INTERNAL: displayId=0, orientation=1\n  Cursor Input Mapper:\n    XScale: 2.0\n".to_string(),
            );
            shell_map.insert(
                "settings get system screen_off_timeout".to_string(),
                "60000".to_string(),
            );
            let mut shell_queue = VecDeque::new();
            for value in ["-3", "-3", "0"] {
                shell_queue.push_back((
                    "settings get system pointer_speed".to_string(),
                    Ok(value.to_string()),
                ));
            }
            Self {
                state: Arc::new(Mutex::new(FakeAdbState {
                    devices,
                    shell_map,
                    shell_queue,
                    shell_ok_map: HashMap::new(),
                    shell_ok_queue: VecDeque::new(),
                    forwards: VecDeque::new(),
                    forward_calls: Vec::new(),
                    forward_removes: Vec::new(),
                    pushes: 0,
                    popen: Vec::new(),
                    calls: Vec::new(),
                    md5: None,
                    version_ok: true,
                    wait_error: None,
                    sdk: "34\n".to_string(),
                })),
                serial: String::new(),
            }
        }

        fn with_serial_raw(mut self, serial: &str) -> Self {
            self.serial = serial.to_string();
            self
        }

        fn snapshot(&self) -> std::sync::MutexGuard<'_, FakeAdbState> {
            self.state.lock().expect("fake adb 状态锁")
        }

        fn set_shell(&self, key: &str, value: &str) {
            if let Ok(mut guard) = self.state.lock() {
                guard.shell_map.insert(key.to_string(), value.to_string());
            }
        }

        fn set_sdk(&self, value: &str) {
            if let Ok(mut guard) = self.state.lock() {
                guard.sdk = value.to_string();
            }
        }

        fn set_shell_ok(&self, key: &str, value: bool) {
            if let Ok(mut guard) = self.state.lock() {
                guard.shell_ok_map.insert(key.to_string(), value);
            }
        }

        fn calls(&self) -> Vec<String> {
            self.snapshot().calls.clone()
        }
    }

    impl AdbApi for FakeAdb {
        fn serial(&self) -> String {
            self.serial.clone()
        }

        fn with_serial(&self, serial: &str) -> Arc<dyn AdbApi> {
            Arc::new(FakeAdb {
                state: self.state.clone(),
                serial: serial.to_string(),
            })
        }

        fn version(&self) -> Result<String, AdbError> {
            let guard = self.snapshot();
            if guard.version_ok {
                Ok("Android Debug Bridge version 1.0.41".to_string())
            } else {
                Err(AdbError::new(AdbErrorCode::AdbNotFound, "adb 不存在"))
            }
        }

        fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
            Ok(self.snapshot().devices.clone())
        }

        fn select_device(
            &self,
            serial: Option<&str>,
            prefer_usb: bool,
        ) -> Result<DeviceEntry, AdbError> {
            let entries = self.snapshot().devices.clone();
            select_device_from(&entries, serial, prefer_usb)
        }

        fn wait_for_device(
            &self,
            _timeout: f64,
            cancel: &(dyn Fn() -> bool + Sync),
        ) -> Result<DeviceEntry, AdbError> {
            if cancel() {
                return Err(AdbError::offline("等待设备已取消"));
            }
            let guard = self.snapshot();
            if let Some(detail) = guard.wait_error.clone() {
                return Err(AdbError::offline(detail));
            }
            match guard.devices.iter().find(|entry| entry.online()) {
                Some(entry) => Ok(entry.clone()),
                None => Err(AdbError::offline("没有在线的平板（adb devices 为空）")),
            }
        }

        fn shell(&self, args: &[&str], _timeout: f64) -> Result<String, AdbError> {
            let key = args.join(" ");
            let mut guard = self.snapshot();
            guard.calls.push(format!("shell {key}"));
            if let Some(position) = guard
                .shell_queue
                .iter()
                .position(|(queued, _)| *queued == key)
            {
                if let Some((_, result)) = guard.shell_queue.remove(position) {
                    return result;
                }
            }
            Ok(guard.shell_map.get(&key).cloned().unwrap_or_default())
        }

        fn shell_ok(&self, args: &[&str], _timeout: f64) -> bool {
            let key = args.join(" ");
            let mut guard = self.snapshot();
            guard.calls.push(format!("shell_ok {key}"));
            if let Some(position) = guard
                .shell_ok_queue
                .iter()
                .position(|(queued, _)| *queued == key)
            {
                if let Some((_, value)) = guard.shell_ok_queue.remove(position) {
                    return value;
                }
            }
            guard.shell_ok_map.get(&key).copied().unwrap_or(true)
        }

        fn shell_result(&self, args: &[&str], _timeout: f64) -> (i32, String) {
            let key = args.join(" ");
            let mut guard = self.snapshot();
            guard.calls.push(format!("shell_result {key}"));
            if key == "getprop ro.build.version.sdk" {
                return (0, guard.sdk.clone());
            }
            (0, "0\n".to_string())
        }

        fn push(&self, _local: &Path, _remote: &str) -> Result<(), AdbError> {
            let mut guard = self.snapshot();
            guard.pushes += 1;
            Ok(())
        }

        fn md5(&self, _remote: &str) -> Option<String> {
            self.snapshot().md5.clone()
        }

        fn forward(&self, port: u16, _remote: &str) -> Result<(), AdbError> {
            let mut guard = self.snapshot();
            guard.forward_calls.push(port);
            if guard.forwards.pop_front().unwrap_or(true) {
                Ok(())
            } else {
                Err(AdbError::new(
                    AdbErrorCode::ForwardFailed,
                    format!("tcp:{port} 被占用"),
                ))
            }
        }

        fn forward_remove(&self, port: u16) {
            if let Ok(mut guard) = self.state.lock() {
                guard.forward_removes.push(port);
            }
        }

        fn popen_shell(&self, command: &str) -> Result<Box<dyn crate::tablet::adb::ChildHandle>, AdbError> {
            let mut guard = self.snapshot();
            guard.popen.push(command.to_string());
            Ok(Box::new(FakeChild::alive()))
        }
    }

    #[derive(Debug)]
    struct FakeChild {
        alive: std::sync::atomic::AtomicBool,
    }

    impl FakeChild {
        fn alive() -> Self {
            Self {
                alive: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }

    impl crate::tablet::adb::ChildHandle for FakeChild {
        fn try_wait(&mut self) -> Option<i32> {
            if self.alive.load(Ordering::SeqCst) {
                None
            } else {
                Some(0)
            }
        }

        fn terminate(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
        }

        fn kill(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
        }

        fn wait_timeout(&mut self, _timeout: Duration) -> bool {
            !self.alive.load(Ordering::SeqCst)
        }

        fn pid(&self) -> Option<u32> {
            Some(4321)
        }
    }

    // -- 假 socket ---------------------------------------------------------

    #[derive(Debug)]
    struct FakeSocket {
        reads: Mutex<VecDeque<Vec<u8>>>,
        written: Mutex<Vec<u8>>,
        shutdown: AtomicBool,
        write_fail: AtomicBool,
        fail_after: Mutex<Option<usize>>,
        write_calls: AtomicUsize,
    }

    impl FakeSocket {
        fn new(initial: Vec<u8>) -> Arc<Self> {
            let mut reads = VecDeque::new();
            if !initial.is_empty() {
                reads.push_back(initial);
            }
            Arc::new(Self {
                reads: Mutex::new(reads),
                written: Mutex::new(Vec::new()),
                shutdown: AtomicBool::new(false),
                write_fail: AtomicBool::new(false),
                fail_after: Mutex::new(None),
                write_calls: AtomicUsize::new(0),
            })
        }

        fn push_read(&self, bytes: Vec<u8>) {
            if let Ok(mut guard) = self.reads.lock() {
                guard.push_back(bytes);
            }
        }

        fn written(&self) -> Vec<u8> {
            self.written
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }

        fn write_calls(&self) -> usize {
            self.write_calls.load(Ordering::SeqCst)
        }

        fn clear_written(&self) {
            if let Ok(mut guard) = self.written.lock() {
                guard.clear();
            }
        }

        fn set_write_fail(&self, fail: bool) {
            self.write_fail.store(fail, Ordering::SeqCst);
        }
    }

    impl SocketIo for FakeSocket {
        fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut guard = match self.reads.lock() {
                Ok(guard) => guard,
                Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "锁损坏")),
            };
            match guard.pop_front() {
                Some(chunk) if chunk.is_empty() => Ok(0),
                Some(chunk) => {
                    let count = chunk.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&chunk[..count]);
                    Ok(count)
                }
                None => {
                    drop(guard);
                    // 避免接收线程忙等；真实 socket 这里会被读超时唤醒。
                    thread::sleep(Duration::from_millis(5));
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "暂时没有数据"))
                }
            }
        }

        fn write_all(&self, data: &[u8]) -> io::Result<()> {
            self.write_calls.fetch_add(1, Ordering::SeqCst);
            if self.write_fail.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "已断开"));
            }
            let mut guard = match self.written.lock() {
                Ok(guard) => guard,
                Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "锁损坏")),
            };
            if let Ok(fail_after) = self.fail_after.lock() {
                if let Some(limit) = *fail_after {
                    if guard.len() >= limit {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "写满后断开"));
                    }
                }
            }
            guard.extend_from_slice(data);
            Ok(())
        }

        fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> io::Result<()> {
            self.shutdown.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeFactory {
        dummy: u8,
        fail: AtomicBool,
        /// 只让第一次 connect 的 dummy 字节错误（用于音频握手失败后回退的用例）。
        first_dummy_bad: AtomicBool,
        /// 模拟 scrcpy 5.0.1：只有第一条连接带 dummy，之后建立的通道没有。
        first_only_dummy: AtomicBool,
        sockets: Mutex<Vec<Arc<FakeSocket>>>,
    }

    impl FakeFactory {
        fn new(dummy: u8) -> Arc<Self> {
            Arc::new(Self {
                dummy,
                fail: AtomicBool::new(false),
                first_dummy_bad: AtomicBool::new(false),
                first_only_dummy: AtomicBool::new(false),
                sockets: Mutex::new(Vec::new()),
            })
        }

        /// 让第一条通道的 dummy 字节错误（模拟"音频通道握手失败"）。
        fn fail_first_dummy(&self) {
            self.first_dummy_bad.store(true, Ordering::SeqCst);
        }

        /// 只有第一条连接带 dummy（对应真机 scrcpy 5.0.1 的行为）。
        fn dummy_first_only(&self) {
            self.first_only_dummy.store(true, Ordering::SeqCst);
        }

        fn last(&self) -> Option<Arc<FakeSocket>> {
            self.sockets.lock().ok().and_then(|guard| guard.last().cloned())
        }

        fn count(&self) -> usize {
            self.sockets.lock().map(|guard| guard.len()).unwrap_or(0)
        }
    }

    impl SocketFactory for FakeFactory {
        fn connect(&self, addr: &str, _timeout: Duration) -> Result<Arc<dyn SocketIo>, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(format!("{addr} 连接被拒绝"));
            }
            let dummy = if self.first_dummy_bad.swap(false, Ordering::SeqCst) {
                0x42
            } else {
                self.dummy
            };
            let first = self.count() == 0;
            let initial = if first || !self.first_only_dummy.load(Ordering::SeqCst) {
                vec![dummy]
            } else {
                Vec::new()
            };
            let socket = FakeSocket::new(initial);
            if let Ok(mut guard) = self.sockets.lock() {
                guard.push(socket.clone());
            }
            Ok(socket)
        }
    }

    // -- 无操作睡眠 --------------------------------------------------------

    #[derive(Debug, Default)]
    struct NoopSleeper {
        total: Mutex<f64>,
    }

    impl Sleeper for NoopSleeper {
        fn sleep(&self, seconds: f64) {
            if let Ok(mut guard) = self.total.lock() {
                *guard += seconds;
            }
            // 记录时长但只睡 1ms：既让退避逻辑瞬间完成，又不让轮询线程忙等。
            if seconds > 0.0 {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    // -- 脚手架 ------------------------------------------------------------

    struct Harness {
        session: Arc<TabletSession>,
        adb: FakeAdb,
        factory: Arc<FakeFactory>,
        sleeper: Arc<NoopSleeper>,
        dir: TempDir,
    }

    fn entry(serial: &str, state: &str) -> DeviceEntry {
        let mut entry = DeviceEntry::new(serial, state);
        entry.model = "Pad 7".to_string();
        entry.device = "pad".to_string();
        entry.transport_id = "3".to_string();
        entry
    }

    fn harness_with(factory: Arc<FakeFactory>, config: SessionConfig) -> Harness {
        let dir = TempDir::new("session");
        let jar = dir.path.join("test.jar");
        std::fs::write(&jar, b"jar-bytes").expect("写 jar");
        let mut config = config;
        config.jar = Some(jar);
        config.auto_flush = false;
        config.watch_orientation = false;
        config.handshake_retry_timeout = 0.2;
        let adb = FakeAdb::new(vec![entry("S1", "device")]).with_serial_raw("");
        let sleeper = Arc::new(NoopSleeper::default());
        let session = SessionBuilder::new(Arc::new(adb.clone()))
            .config(config)
            .socket_factory(factory.clone())
            .sleeper(sleeper.clone())
            .store(Arc::new(StateStore::new(&dir.path)))
            .build();
        Harness {
            session,
            adb,
            factory,
            sleeper,
            dir,
        }
    }

    fn harness() -> Harness {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        harness_with(FakeFactory::new(0), config)
    }

    fn wait_until<F: Fn() -> bool>(condition: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    // -- 纯函数 ------------------------------------------------------------

    #[test]
    fn parse_wm_size_prefers_override() {
        let text = "Physical size: 1600x2560\nOverride size: 2560x1600\n";
        assert_eq!(parse_wm_size(text).expect("应当解析"), (2560, 1600));
        assert_eq!(
            parse_wm_size("Physical size: 1080x1920\n").expect("应当解析"),
            (1080, 1920)
        );
    }

    #[test]
    fn parse_wm_size_reports_e101_on_garbage() {
        let error = parse_wm_size("no size here").expect_err("应当失败");
        assert_eq!(error.code(), ErrorCode::DeviceOffline);
        assert!(error.detail().contains("无法解析 wm size 输出"));
    }

    #[test]
    fn parse_orientation_prefers_viewport_line() {
        let text = "  Viewport INTERNAL: displayId=0, orientation=1\n  orientation=3\n";
        assert_eq!(parse_orientation(text), 1);
        assert_eq!(parse_orientation("orientation=6"), 2);
    }

    #[test]
    fn parse_orientation_reads_the_viewport_line_in_a_full_dump() {
        // 2.0.6：整份 dumpsys input 的输出里挑 Viewport 行（不再在设备端 grep）。
        let dump = [
            "INPUT MANAGER (dumpsys input)",
            "",
            "Input Dispatcher State:",
            "  DispatchEnabled: true",
            "  Viewport INTERNAL: displayId=0, uniqueId=local:1, port=131, orientation=1, densityDpi=420",
            "  FocusedApplications:",
            "    displayId=0, name='com.android.launcher3'",
        ]
        .join("\n");
        assert_eq!(parse_orientation(&dump), 1);
        assert_eq!(oriented_size(2136, 3200, parse_orientation(&dump)), (3200, 2136));
    }

    #[test]
    fn orientation_interval_backs_off_while_capturing() {
        // 2.0.6：接管期间不能停摆（否则转屏发现不了），但要拉长到 60s。
        assert_eq!(orientation_interval(false), ORIENTATION_POLL_S);
        assert_eq!(orientation_interval(true), ORIENTATION_CAPTURE_POLL_S);
        assert!(ORIENTATION_CAPTURE_POLL_S > ORIENTATION_POLL_S);
    }

    #[test]
    fn parse_orientation_handles_multibyte_text_after_the_key() {
        // P0-5：after 以多字节字符开头时，&after[1..] 会 panic
        // （byte index 1 is not a char boundary）。解析对象是 adb shell 输出，
        // 本地化文案随时可能出现在 orientation= 后面。
        assert_eq!(parse_orientation("orientation=横向"), 0);
        assert_eq!(parse_orientation("orientation=横 1"), 0);
        // “继续找下一个 orientation=” 的语义必须保留。
        assert_eq!(parse_orientation("orientation=横orientation=2"), 2);
        assert_eq!(parse_orientation("orientation=xorientation=3"), 3);
        assert_eq!(parse_orientation("orientation=横, orientation=1"), 1);
        // ASCII 路径与空值不变。
        assert_eq!(parse_orientation("orientation=5"), 1);
        assert_eq!(parse_orientation("orientation="), 0);
        assert_eq!(parse_orientation("orientation"), 0);
        assert_eq!(parse_orientation("orientation=横"), 0);
    }

    #[test]
    fn releasing_control_then_waking_does_not_starve_the_orientation_loop() {
        // P0-6：心跳线程（None）不许消费朝向唤醒；朝向线程（Some）第一片就醒。
        let harness = harness();
        let core = harness.session.core.as_ref();
        let stop = AtomicBool::new(false);

        core.orientation_wake.store(true, Ordering::SeqCst);
        let before = *harness.sleeper.total.lock().expect("锁");
        assert!(!core.sleep_interruptible(LINK_PROBE_INTERVAL_S, &stop, None));
        let after = *harness.sleeper.total.lock().expect("锁");
        assert!(after > before, "心跳应当照常睡满：{before} -> {after}");
        assert!(
            core.orientation_wake.load(Ordering::SeqCst),
            "adb 心跳把朝向唤醒吃掉了（P0-6 回归）"
        );

        let before = *harness.sleeper.total.lock().expect("锁");
        assert!(!core.sleep_interruptible(
            ORIENTATION_CAPTURE_POLL_S,
            &stop,
            Some(&core.orientation_wake)
        ));
        let after = *harness.sleeper.total.lock().expect("锁");
        assert_eq!(after, before, "有唤醒时应当第一片就返回，不进睡眠");
        assert!(!core.orientation_wake.load(Ordering::SeqCst), "唤醒只消费一次");
    }

    #[test]
    fn releasing_control_raises_the_orientation_wake() {
        let harness = harness();
        let core = harness.session.core.as_ref();
        core.orientation_wake.store(false, Ordering::SeqCst);
        core.set_capturing(true);
        assert!(!core.orientation_wake.load(Ordering::SeqCst), "接管时不该叫醒");
        core.set_capturing(false);
        assert!(core.orientation_wake.load(Ordering::SeqCst), "交还时要叫醒");
    }

    #[test]
    fn parse_orientation_falls_back_to_size() {
        assert_eq!(parse_orientation("Physical size: 2560x1600\n"), 1);
        assert_eq!(parse_orientation("Physical size: 1600x2560\n"), 0);
        assert_eq!(parse_orientation("nothing"), 0);
    }

    #[test]
    fn oriented_size_swaps_only_on_odd_orientation() {
        assert_eq!(oriented_size(1600, 2560, 0), (1600, 2560));
        assert_eq!(oriented_size(1600, 2560, 1), (2560, 1600));
        assert_eq!(oriented_size(1600, 2560, 2), (1600, 2560));
        assert_eq!(oriented_size(1600, 2560, 3), (2560, 1600));
    }

    #[test]
    fn parse_xscale_reads_cursor_input_mapper_only() {
        let text = "  Cursor Input Mapper:\n    XScale: 1.5\n    YScale: 1.5\n\n  XScale: 9.0\n";
        assert!((parse_xscale(text) - 1.5).abs() < 1e-9);
        assert!((parse_xscale("  Cursor Input Mapper:\n    XScale: 0\n") - 1.0).abs() < 1e-9);
        assert!((parse_xscale("no mapper") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_pointer_speed_variants() {
        assert_eq!(parse_pointer_speed("-3\n"), -3);
        assert_eq!(parse_pointer_speed("null\n"), 0);
        assert_eq!(parse_pointer_speed(" 1 \n"), 1);
        assert_eq!(parse_pointer_speed("abc"), 0);
    }

    #[test]
    fn clean_serial_variants() {
        assert_eq!(clean_serial("unknown\n"), "");
        assert_eq!(clean_serial("null"), "");
        assert_eq!(clean_serial("0"), "");
        assert_eq!(clean_serial("none"), "");
        assert_eq!(clean_serial("abc\nXYZ\n"), "XYZ");
        assert_eq!(clean_serial(""), "");
    }

    #[test]
    fn state_store_roundtrip_is_atomic_and_clearable() {
        let dir = TempDir::new("state");
        let store = StateStore::new(&dir.path);
        assert!(store.save(
            POINTER_SPEED_STATE,
            serde_json::json!({"serial": "S1", "value": -3})
        ));
        assert_eq!(
            store.load_entry(POINTER_SPEED_STATE),
            Some(("S1".to_string(), -3))
        );
        let raw = store.load(POINTER_SPEED_STATE).expect("应当能读回");
        assert!(raw.get("saved_at").is_some());
        assert!(store.save(
            POINTER_SPEED_STATE,
            serde_json::json!({"serial": "S2", "value": 5})
        ));
        assert_eq!(
            store.load_entry(POINTER_SPEED_STATE),
            Some(("S2".to_string(), 5))
        );
        store.clear(POINTER_SPEED_STATE);
        assert!(store.load_entry(POINTER_SPEED_STATE).is_none());
    }

    // -- 启动 --------------------------------------------------------------

    #[test]
    fn start_reads_facts_and_creates_both_uhid_devices() {
        let harness = harness();
        let facts = harness.session.connect().expect("应当启动");
        assert_eq!(facts.serial, "S1");
        assert_eq!(facts.model, "Pad 7");
        assert_eq!(facts.width, 2560);
        assert_eq!(facts.height, 1600);
        assert_eq!(facts.orientation, 1);
        assert!((facts.xscale - 2.0).abs() < 1e-9);
        assert_eq!(facts.pointer_speed, -3);
        assert_eq!(facts.hardware_serial, "HW-SERIAL");
        assert!(harness.session.is_started());
        assert_eq!(harness.session.port(), Some(SessionConfig::default().server_port));
        let socket = harness.factory.last().expect("应当建立控制通道");
        let written = socket.written();
        assert_eq!(written[0], crate::tablet::protocol::MSG_UHID_CREATE);
        // 第二条 UHID_CREATE 在鼠标之后：直接扫描报文头。
        let mut offset = 0;
        let mut ids = Vec::new();
        while offset < written.len() {
            if written[offset] != crate::tablet::protocol::MSG_UHID_CREATE {
                break;
            }
            let id = u16::from_be_bytes([written[offset + 1], written[offset + 2]]);
            ids.push(id);
            let name_len = written[offset + 7] as usize;
            let desc_len =
                u16::from_be_bytes([written[offset + 8 + name_len], written[offset + 9 + name_len]])
                    as usize;
            offset += 8 + name_len + 2 + desc_len;
        }
        assert_eq!(ids, vec![C::UHID_MOUSE_ID, C::UHID_KEYBOARD_ID]);
        let health = harness.session.health_report();
        assert!(health.alive && health.socket_ok && health.uhid_created);
    }

    #[test]
    fn start_reports_bad_handshake_dummy() {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        let harness = harness_with(FakeFactory::new(0x42), config);
        let error = harness.session.connect().expect_err("dummy 不对必须失败");
        assert_eq!(error.code(), ErrorCode::HandshakeFailed);
        assert!(error.detail().contains("dummy 字节不符"));
        assert!(!harness.session.is_started());
    }

    #[test]
    fn start_fails_when_pointer_speed_cannot_be_zeroed() {
        let harness = harness();
        harness
            .adb
            .set_shell_ok("settings put system pointer_speed 0", false);
        let error = harness.session.connect().expect_err("归零失败必须报错");
        assert_eq!(error.code(), ErrorCode::DeviceOffline);
        assert!(error.detail().contains("无法把 pointer_speed 归零"));
        assert!(!harness.session.is_started());
    }

    #[test]
    fn start_reports_offline_device_after_wait() {
        let adb = FakeAdb::new(vec![entry("S1", "offline")]).with_serial_raw("");
        if let Ok(mut guard) = adb.state.lock() {
            guard.wait_error = Some("没有在线的平板（adb devices 为空）".to_string());
        }
        let dir = TempDir::new("offline");
        let jar = dir.path.join("test.jar");
        std::fs::write(&jar, b"jar").expect("写 jar");
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        config.jar = Some(jar);
        let session = SessionBuilder::new(Arc::new(adb))
            .config(config)
            .socket_factory(FakeFactory::new(0))
            .sleeper(Arc::new(NoopSleeper::default()))
            .store(Arc::new(StateStore::new(&dir.path)))
            .build();
        let error = session.connect().expect_err("离线必须失败");
        assert_eq!(error.code(), ErrorCode::DeviceOffline);
        assert!(error.detail().contains("没有在线的平板"));
    }

    // -- 发送 --------------------------------------------------------------

    #[test]
    fn send_queue_batches_at_most_eight_messages() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let socket = harness.factory.last().expect("应当有控制通道");
        socket.clear_written();
        for index in 0..10 {
            harness
                .session
                .send_mouse(0, index, 0, 0)
                .expect("应当入队");
        }
        assert_eq!(harness.session.pending_len(), 10);
        let written = harness.session.sender_frame().expect("应当写出");
        assert_eq!(written, C::MAX_BATCH);
        assert_eq!(harness.session.pending_len(), 2);
        // 每条鼠标报告是 UHID_INPUT：1 + 2 + 2 + 4 = 9 字节。
        assert_eq!(socket.written().len(), 9 * C::MAX_BATCH);
    }

    #[test]
    fn send_queue_drops_oldest_when_full() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        for _ in 0..(C::SEND_QUEUE_MAXLEN + 5) {
            harness.session.send_mouse(0, 1, 1, 0).expect("应当入队");
        }
        assert_eq!(harness.session.pending_len(), C::SEND_QUEUE_MAXLEN);
    }

    #[test]
    fn enqueue_and_stop_wake_blocked_sender() {
        let harness = harness();
        let stop = Arc::new(AtomicBool::new(false));
        let core = Arc::clone(&harness.session.core);
        let worker_stop = Arc::clone(&stop);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            core.wait_for_sender_work(&worker_stop);
            let _ = tx.send(());
        });
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        harness.session.core.enqueue(vec![1, 2, 3]);
        rx.recv_timeout(Duration::from_millis(500))
            .expect("入队应唤醒发送线程");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "入队到唤醒耗时 {:?}",
            started.elapsed()
        );
        worker.join().expect("等待线程应退出");

        // stop_flusher 的 notify_all 也必须能叫醒空闲等待，不能等满 1s 兜底。
        let stop = Arc::new(AtomicBool::new(false));
        let core = Arc::clone(&harness.session.core);
        let worker_stop = Arc::clone(&stop);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            core.wait_for_sender_work(&worker_stop);
            let _ = tx.send(());
        });
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        stop.store(true, Ordering::SeqCst);
        harness.session.core.queue_cv.notify_all();
        rx.recv_timeout(Duration::from_millis(500))
            .expect("stop 应唤醒发送线程");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "stop 到唤醒耗时 {:?}",
            started.elapsed()
        );
        worker.join().expect("等待线程应退出");
    }
    #[test]
    fn receiver_loop_handles_messages_split_across_reads() {
        fn clipboard_packet(text: &str) -> Vec<u8> {
            let mut packet = vec![crate::tablet::protocol::DEVICE_MSG_CLIPBOARD];
            packet.extend_from_slice(&(text.len() as u32).to_be_bytes());
            packet.extend_from_slice(text.as_bytes());
            packet
        }

        let harness = harness();
        let (tx, rx) = std::sync::mpsc::channel();
        harness.session.set_clipboard_callback(Some(Arc::new(move |text| {
            let _ = tx.send(text);
        })));
        let first = clipboard_packet("一号");
        let second = clipboard_packet("second");
        let third = clipboard_packet("三号");
        let socket = FakeSocket::new(Vec::new());
        // 第一条完整 + 第二条前 3 字节；第二条剩余 + 第三条前 2 字节；第三条剩余；EOF。
        let mut chunk = first.clone();
        chunk.extend_from_slice(&second[..3]);
        socket.push_read(chunk);
        let mut chunk = second[3..].to_vec();
        chunk.extend_from_slice(&third[..2]);
        socket.push_read(chunk);
        socket.push_read(third[2..].to_vec());
        socket.push_read(Vec::new());
        let core = Arc::clone(&harness.session.core);
        let socket: Arc<dyn SocketIo> = socket;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || core.receiver_loop(socket, stop));
        let received = vec![
            rx.recv_timeout(Duration::from_secs(1)).expect("第一条"),
            rx.recv_timeout(Duration::from_secs(1)).expect("第二条"),
            rx.recv_timeout(Duration::from_secs(1)).expect("第三条"),
        ];
        worker.join().expect("接收线程应退出");
        assert_eq!(received, vec!["一号", "second", "三号"]);
        assert!(harness.session.pending_len() == 0);
    }
    #[test]
    fn sender_frame_writes_each_batch_in_one_call() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let socket = harness.factory.last().expect("应当有控制通道");
        socket.clear_written();
        for _ in 0..3 {
            harness.session.send_mouse(0, 1, 1, 0).expect("应当入队");
        }
        let before = socket.write_calls();
        let written = harness.session.sender_frame().expect("应当写出");
        assert_eq!(written, 3);
        assert_eq!(socket.write_calls() - before, 1, "一帧只应一次 write_all");
        // 每条鼠标报告是 UHID_INPUT：1 + 2 + 2 + 4 = 9 字节。
        assert_eq!(socket.written().len(), 9 * 3);
    }
    #[test]
    fn sender_frame_flags_broken_after_three_write_failures() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let socket = harness.factory.last().expect("应当有控制通道");
        socket.set_write_fail(true);
        let mut last_error = None;
        for _ in 0..3 {
            harness.session.send_mouse(0, 1, 1, 0).expect("应当入队");
            if let Err(error) = harness.session.sender_frame() {
                last_error = Some(error);
                break;
            }
        }
        let error = last_error.expect("第三次写失败必须报错");
        assert_eq!(error.code(), ErrorCode::SocketBroken);
        assert!(error.detail().contains("连续 3 次写失败"));
        assert!(!harness.session.health_report().socket_ok);
    }

    #[test]
    fn keyboard_clipboard_and_text_are_encoded() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let socket = harness.factory.last().expect("应当有控制通道");
        socket.clear_written();
        harness.session.send_key(0x02, &[0x04, 0x05]).expect("应当入队");
        harness.session.set_clipboard("你好").expect("应当入队");
        harness.session.inject_text("").expect("空串忽略");
        assert_eq!(harness.session.pending_len(), 2);
        harness.session.sender_frame().expect("应当写出");
        let written = socket.written();
        // 键盘：1 + 2 + 2 + 8 = 13；剪贴板：1 + 8 + 1 + 4 + 6 = 20。
        assert_eq!(written.len(), 13 + 20);
        assert_eq!(written[0], crate::tablet::protocol::MSG_UHID_INPUT);
        assert_eq!(
            written[13],
            crate::tablet::protocol::MSG_SET_CLIPBOARD
        );
    }

    #[test]
    fn send_before_start_is_e106() {
        let harness = harness();
        let error = harness.session.send_mouse(0, 1, 1, 0).expect_err("未连接必须报错");
        assert_eq!(error.code(), ErrorCode::SocketBroken);
        assert!(error.detail().contains("控制通道未建立或已断开"));
    }

    // -- 清理 --------------------------------------------------------------

    #[test]
    fn teardown_restores_pointer_speed_and_screen_timeout() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        harness.session.disconnect();
        assert!(!harness.session.is_started());
        let calls = harness.adb.calls();
        assert!(calls
            .iter()
            .any(|call| call == "shell_ok settings put system pointer_speed -3"));
        assert!(calls
            .iter()
            .any(|call| call == "shell_ok settings put system screen_off_timeout 60000"));
        let pkill_calls = calls
            .iter()
            .filter(|call| call.starts_with("shell_ok pkill"))
            .count();
        assert!(pkill_calls >= 3, "calls={calls:?}");
        assert!(harness
            .adb
            .snapshot()
            .forward_removes
            .contains(&SessionConfig::default().server_port));
        let state = StateStore::new(&harness.dir.path);
        assert!(state.load_entry(POINTER_SPEED_STATE).is_none());
        assert!(state.load_entry(SCREEN_TIMEOUT_STATE).is_none());
    }

    #[test]
    fn stop_without_start_is_ignored() {
        let harness = harness();
        harness.session.disconnect();
        harness.session.disconnect();
        assert!(!harness.session.is_started());
        assert!(harness.adb.calls().is_empty());
    }

    // -- 重连 --------------------------------------------------------------

    #[test]
    fn reconnect_requires_started_session() {
        let harness = harness();
        assert!(harness
            .session
            .reconnect_attempts(Some(1))
            .expect("未启动应当返回 Ok(None)")
            .is_none());
    }

    #[test]
    fn reconnect_rebuilds_transport_and_uhid() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let before = harness.factory.count();
        let facts = harness
            .session
            .reconnect_attempts(Some(1))
            .expect("应当成功")
            .expect("应当有事实");
        assert_eq!(facts.serial, "S1");
        assert!(harness.factory.count() > before, "应当新建控制通道");
        let socket = harness.factory.last().expect("应当有新的控制通道");
        assert_eq!(
            socket.written()[0],
            crate::tablet::protocol::MSG_UHID_CREATE
        );
    }

    #[test]
    fn reconnect_returns_none_when_every_attempt_fails() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        harness.factory.fail.store(true, Ordering::SeqCst);
        let result = harness
            .session
            .reconnect_attempts(Some(2))
            .expect("重连失败不抛");
        assert!(result.is_none());
        assert!(harness.sleeper.total.lock().map(|g| *g).unwrap_or(0.0) >= 3.0);
    }

    // -- 朝向 / 事实 / 健康 -----------------------------------------------

    #[test]
    fn apply_orientation_updates_facts_and_invokes_callback() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let seen: Arc<Mutex<Vec<DeviceFacts>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        assert!(harness.session.set_facts_callback(Some(Arc::new(
            move |facts: DeviceFacts| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push(facts);
                }
            }
        ))));
        harness
            .session
            .core
            .apply_orientation("Viewport INTERNAL: displayId=0, orientation=0");
        let facts = harness.session.current_facts().expect("应当有事实");
        assert_eq!(facts.orientation, 0);
        assert_eq!(facts.width, 1600);
        assert_eq!(facts.height, 2560);
        let calls = seen.lock().map(|guard| guard.clone()).unwrap_or_default();
        assert_eq!(calls.len(), 1);
        // 相同朝向不应再触发回调。
        harness
            .session
            .core
            .apply_orientation("Viewport INTERNAL: displayId=0, orientation=0");
        assert_eq!(seen.lock().map(|g| g.len()).unwrap_or(0), 1);
    }

    #[test]
    fn refresh_facts_reloads_screen_size() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        harness.adb.set_shell("wm size", "Physical size: 800x600");
        let facts = harness.session.refresh().expect("应当刷新");
        assert_eq!(facts.width, 600);
        assert_eq!(facts.height, 800);
    }

    #[test]
    fn health_marks_socket_broken_after_receiver_eof() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let socket = harness.factory.last().expect("应当有控制通道");
        socket.push_read(Vec::new());
        assert!(wait_until(|| !harness.session.health_report().socket_ok));
        let health = harness.session.health_report();
        assert!(!health.alive);
        assert!(health.last_error.contains("对端关闭了控制通道"));
    }

    #[test]
    fn clipboard_device_message_reaches_callback() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        assert!(harness.session.set_clipboard_callback(Some(Arc::new(
            move |text: String| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push(text);
                }
            }
        ))));
        let socket = harness.factory.last().expect("应当有控制通道");
        let text = "平板剪贴板";
        let mut payload = Vec::new();
        payload.push(crate::tablet::protocol::DEVICE_MSG_CLIPBOARD);
        payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
        payload.extend_from_slice(text.as_bytes());
        socket.push_read(payload);
        assert!(wait_until(|| !seen
            .lock()
            .map(|guard| guard.is_empty())
            .unwrap_or(true)));
        let values = seen.lock().map(|guard| guard.clone()).unwrap_or_default();
        assert_eq!(values, vec![text.to_string()]);
    }

    #[test]
    fn ensure_pointer_speed_zeroes_again() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        harness.adb.set_shell("settings get system pointer_speed", "7");
        let ok = harness.session.ensure_pointer_speed().expect("应当复查");
        assert!(!ok, "复查时读回 7，归零后仍应是 7（fake 未改值）");
        let calls = harness.adb.calls();
        assert!(calls
            .iter()
            .any(|call| call == "shell_ok settings put system pointer_speed 0"));
    }

    #[test]
    fn force_cleanup_writes_pointer_speed_and_removes_forward() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        assert!(harness.session.force_cleanup(Some(1234)));
        let calls = harness.adb.calls();
        assert!(calls.iter().any(|call| call == "shell_ok pkill -9 -f com.genymobile.scrcpy.Server"));
        assert!(harness.adb.snapshot().forward_removes.contains(&1234));
        assert!(calls
            .iter()
            .any(|call| call == "shell_ok settings put system pointer_location 0"));
    }

    #[test]
    fn audio_failure_falls_back_to_control_only() {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        config.forward_audio = true;
        let factory = FakeFactory::new(0);
        // 音频通道先握手：让第一条通道 dummy 错误，触发"回退为仅控制通道"分支。
        factory.fail_first_dummy();
        let harness = harness_with(factory.clone(), config);
        let facts = harness.session.connect().expect("回退后应当启动");
        assert_eq!(facts.serial, "S1");
        // 回退会丢弃旧 server 并重新 spawn 一次。
        assert_eq!(harness.adb.snapshot().popen.len(), 2, "应当重启服务端");
        // 旧服务端带 cleanup=true，退出时会自删远端 jar：回退重启前必须重新推送，
        // 否则新服务端起不来，用户会一直卡在连接中直到 30s 超时。
        assert_eq!(
            harness.adb.snapshot().pushes,
            2,
            "回退重启前必须重新推送 jar"
        );
        assert!(harness.session.is_started());
        harness.session.disconnect();
    }

    #[test]
    fn audio_mode_does_not_wait_for_dummy_on_control_channel() {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        config.forward_audio = true;
        let factory = FakeFactory::new(0);
        // 真机 scrcpy 5.0.1：只有第一条被 accept 的连接带 dummy，控制通道没有。
        factory.dummy_first_only();
        let harness = harness_with(factory.clone(), config);
        let facts = harness
            .session
            .connect()
            .expect("控制通道没有 dummy 也应当连上");
        assert_eq!(facts.serial, "S1");
        assert_eq!(factory.count(), 2, "音频 + 控制两条通道都要建立");
        assert_eq!(
            harness.adb.snapshot().pushes,
            1,
            "不该因为假失败触发回退重推"
        );
        assert!(harness.session.is_started());
        harness.session.disconnect();
    }

    #[test]
    fn audio_is_skipped_on_android_below_eleven() {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        config.forward_audio = true;
        let harness = harness_with(FakeFactory::new(0), config);
        // Android 10 = API 29，没有系统音频回采，audio=true 只会让服务端起不来。
        harness.adb.set_sdk("29\n");
        harness.session.connect().expect("应当启动");
        let command = harness
            .adb
            .snapshot()
            .popen
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(
            command.contains("audio=false"),
            "Android 10 应当只开控制通道：{command}"
        );
        assert!(
            harness
                .adb
                .calls()
                .iter()
                .any(|call| call.contains("ro.build.version.sdk")),
            "应当先查系统 API 级别"
        );
        harness.session.disconnect();
    }

    #[test]
    fn saved_config_reaches_the_session_snapshot() {
        // Python 的会话共享同一个 Config 对象，config.save 之后下次连接就能看到新值；
        // Rust 会话是构造期快照，必须靠 config_save 推送（service::api::sync_session_config）。
        let harness = harness(); // forward_audio=false、auto_flush=false、watch_orientation=false
        let mut config = crate::config::Config::default();
        config.misc.forward_audio = true;
        config.device.server_port = 27999;
        config.device.serial = "S1".to_string();
        harness.session.update_config(&config);
        let snapshot = harness.session.core.config_snapshot();
        assert!(snapshot.forward_audio, "音频开关必须进到会话快照");
        assert_eq!(snapshot.server_port, 27999);
        assert_eq!(snapshot.serial, "S1");
        assert!(!snapshot.auto_flush, "运行时旋钮不能被打回默认值");
        assert!(!snapshot.watch_orientation);
    }

    #[test]
    fn audio_switch_saved_while_connected_takes_effect_on_reconnect() {
        let mut config = SessionConfig::default();
        config.serial = "S1".to_string();
        let harness = harness_with(FakeFactory::new(0), config);
        harness.session.connect().expect("首次连接");
        let first = harness
            .adb
            .snapshot()
            .popen
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(!first.contains("audio=true"), "默认不带音频通道：{first}");

        // 设置里打开「转发平板声音」→ 保存配置 → 重连（用户实际操作顺序）。
        let mut app = crate::config::Config::default();
        app.misc.forward_audio = true;
        app.device.serial = "S1".to_string();
        harness.session.update_config(&app);
        harness.session.disconnect();
        harness.session.connect().expect("重连");
        let second = harness
            .adb
            .snapshot()
            .popen
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(second.contains("audio=true"), "重连后应当带音频通道：{second}");
        harness.session.disconnect();
    }

    #[test]
    fn session_port_reports_health_and_facts() {
        let harness = harness();
        harness.session.connect().expect("应当启动");
        let port: &dyn SessionPort = harness.session.as_ref();
        assert!(port.started());
        assert!(port.facts().is_some());
        let health = port.health().expect("应当有健康").expect("应当有");
        assert!(health.socket_ok);
        assert!(port.ensure_pointer_speed().is_some());
        assert!(!port.set_cursor_pos(1, 1));
    }
}



