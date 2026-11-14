//! Router：跨越状态机、发送队列与 supervisor（对应 Python 的 core/router.py）。
//!
//! 这是 B2 迁移 M3 阶段的核心模块：把 Python 版 Router（2206 行）逐条移植为 Rust，
//! 行为 1:1——状态迁移原因字符串（golden trace 会逐条比对）、溢出测量、角落裁决、
//! 入口 warp burst、心跳、按键防粘、pointer_speed 轮询、supervisor tick、健康检查与
//! 重连、旋转与桌面拓扑变化、剪贴板注入。
//!
//! # 线程与锁（DESIGN 10）
//!
//! - Router 自身是 Sync：全部可变状态放在 Mutex<Inner>，公开方法一律借 &self。
//!   Python 用 RLock 允许同线程重入，Rust 的 Mutex 不可重入，所以 Python 的每个
//!   _xxx_locked 都变成 fn xxx(&self, inner: &mut Inner, ...)，由最外层方法一次性取锁。
//! - 四个公开回调（state / metrics / orientation / clipboard）在锁外统一派发
//!   （RouterEvent 缓冲），既避免"锁内回调 → 回调再进 Router"的死锁，也让 GUI 可以
//!   安全地在 on_state_changed 里调用 Router::set_placement。
//! - 发送队列是独立的 Mutex<VecDeque<SendMsg>>（对应 Python 的 _send_lock）。锁序固定
//!   为 Inner → Queue，任何路径都不反向加锁。
//! - 两个后台线程（padlend-sender / padlend-supervisor）只按节奏调用
//!   Router::sender_frame 与 Router::tick_supervisor；测试直接调用这两个方法即可，
//!   既不需要线程，也不需要真实时间（时钟由 Clock 注入）。
//!
//! # 性能（PITFALLS 2.5）
//!
//! 鼠标热路径（on_raw → handle_captured → queue_mouse）只改状态、只入队，不碰 socket；
//! 真正的写操作全部在 sender 线程的 Router::sender_frame 里、持锁之外完成。与 Python
//! 一致的两处例外仍然保留：enter_captured 的 mask.show()，以及 begin_return / fault 的
//! "补发全松"（send_keyboard_zero / send_mouse_release）发生在锁内，因为它们的顺序语义
//! 必须与状态迁移严格一致。
//!
//! # 与 Python 的已知差异
//!
//! 1. Config 在 Rust 侧是不可变借用：connect_device 无法再改写 cfg.device.serial。
//!    目标序列号记录在 Router::requested_serial，真正切换设备由 main 层交给会话实现。
//! 2. 回调在锁外派发（Python 在锁内调用，靠 RLock 重入）。transitions 的顺序不变。
//! 3. Python 的 _normalize_report（bytes / tuple / 对象三种形态）收敛成
//!    KeymapPort::on_key 的 Option<(u8, Vec<u8>)> 契约。
//! 4. 会话回调（facts / clipboard）无法在构造函数里注册（拿不到 &Arc<Self>），改为
//!    Router::register_session_callbacks，并由 Router::start 幂等补调。
//! 5. 日志：crate 没有 log 依赖，沿用现有模块的 eprintln!("[router] ...") 风格；debug 级
//!    日志用环境变量 PADLEND_ROUTER_DEBUG 门控，避免 1000Hz 热路径上做格式化。
//! 6. timeBeginPeriod(1)（crate 未启用 Win32_Media feature）改为从 winmm.dll 动态解析，
//!    失败即降级为 no-op。

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::Config;
use crate::constants as C;
use crate::logic::events::{KeyEvent, RawDelta};
use crate::logic::geometry::{
    segment_contacts_tablet, tablet_entry_point, tablet_exit_point, tablet_rect, Align, Edge,
    EdgeSegment, MonitorInfo, Placement, Rect, TabletRect,
};
use crate::logic::metrics::{Metrics, NullProcessUsage, Snapshot};
use crate::logic::state::{can_transition, State};
use crate::tablet::keymap::{Keymap, VK_NAME_TO_CODE};

// ---------------------------------------------------------------------------
// 错误码与异常（core/errors.py）
// ---------------------------------------------------------------------------

/// 冻结错误码（core/errors.py，DESIGN 8.4）。
///
/// 码值是契约：日志、诊断包、fault:<码> 状态迁移原因都用它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// E100：找不到 adb。
    AdbNotFound,
    /// E101：平板离线。
    DeviceOffline,
    /// E102：/dev/uhid 不可写。
    UhidUnavailable,
    /// E103：平板端服务启动失败。
    ServerStartFailed,
    /// E104：adb forward 失败。
    ForwardFailed,
    /// E105：握手失败。
    HandshakeFailed,
    /// E106：数据通道断开。
    SocketBroken,
    /// E200：键盘钩子失败。
    HookFailed,
    /// E201：Raw Input 注册失败。
    RawInputFailed,
    /// E202：ClipCursor 泄漏。
    ClipCursorLeak,
    /// E300：配置非法。
    ConfigInvalid,
    /// E301：摆位失效。
    PlacementInvalid,
    /// E900：未预期的错误。
    Unexpected,
}

impl ErrorCode {
    /// 冻结的对外码值（E101 这类）。
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::AdbNotFound => "E100",
            ErrorCode::DeviceOffline => "E101",
            ErrorCode::UhidUnavailable => "E102",
            ErrorCode::ServerStartFailed => "E103",
            ErrorCode::ForwardFailed => "E104",
            ErrorCode::HandshakeFailed => "E105",
            ErrorCode::SocketBroken => "E106",
            ErrorCode::HookFailed => "E200",
            ErrorCode::RawInputFailed => "E201",
            ErrorCode::ClipCursorLeak => "E202",
            ErrorCode::ConfigInvalid => "E300",
            ErrorCode::PlacementInvalid => "E301",
            ErrorCode::Unexpected => "E900",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Router 会话层错误（对应 Python PadLendError(code, detail)）。
///
/// 状态机只需要"码 + 描述"：码用于 fault:<码> 迁移原因与重连分支，描述用于日志与
/// state.fault 事件。
#[derive(Debug, Clone, Error)]
pub enum RouterError {
    /// 业务错误：带冻结码。
    #[error("{code}: {detail}")]
    Coded {
        /// 冻结错误码。
        code: ErrorCode,
        /// 人类可读的补充说明。
        detail: String,
    },
}

impl RouterError {
    /// 用码与描述构造。
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        RouterError::Coded {
            code,
            detail: detail.into(),
        }
    }

    /// 设备离线（E101）。
    pub fn device_offline(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::DeviceOffline, detail)
    }

    /// 摆位失效（E301）。
    pub fn placement_invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::PlacementInvalid, detail)
    }

    /// 握手失败（E105）。
    pub fn handshake_failed(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::HandshakeFailed, detail)
    }

    /// 未预期的错误（E900）。
    pub fn unexpected(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unexpected, detail)
    }

    /// 冻结错误码。
    pub fn code(&self) -> ErrorCode {
        match self {
            RouterError::Coded { code, .. } => *code,
        }
    }

    /// 补充说明（可能为空串）。
    pub fn detail(&self) -> &str {
        match self {
            RouterError::Coded { detail, .. } => detail.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// 设备事实 / 健康快照（tablet/session.py，DESIGN 8.2 冻结字段）
// ---------------------------------------------------------------------------

/// start() 返回的设备事实（字段与 Python DeviceFacts 一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceFacts {
    /// adb 序列号。
    pub serial: String,
    /// 机型名。
    pub model: String,
    /// 逻辑宽（像素）。
    pub width: i32,
    /// 逻辑高（像素）。
    pub height: i32,
    /// 屏幕方向 0..3（Surface.ROTATION_*）。
    pub orientation: i32,
    /// 平板指针的 XScale。
    pub xscale: f64,
    /// 系统指针速度档位（0 = 归零，1 计数 = XScale 像素）。
    pub pointer_speed: i32,
    /// ro.serialno；读不到时为空串。
    pub hardware_serial: String,
}

impl DeviceFacts {
    /// 忽略 hardware_serial 的便捷构造。
    pub fn new(
        serial: impl Into<String>,
        model: impl Into<String>,
        width: i32,
        height: i32,
        orientation: i32,
        xscale: f64,
        pointer_speed: i32,
    ) -> Self {
        Self {
            serial: serial.into(),
            model: model.into(),
            width,
            height,
            orientation,
            xscale,
            pointer_speed,
            hardware_serial: String::new(),
        }
    }

    /// 等价于 Python 的 float(getattr(facts, "xscale", 1.0) or 1.0)。
    ///
    /// 只把 0.0（以及非有限值）换成 1.0；负值原样保留，由 CursorEstimator::new 决定是否
    /// 退化为 1.0（与 Python 相同）。
    pub fn xscale_or_one(&self) -> f64 {
        if self.xscale == 0.0 || !self.xscale.is_finite() {
            1.0
        } else {
            self.xscale
        }
    }
}

/// 会话健康快照（对应 Python HealthReport）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HealthReport {
    /// 服务端进程存活。
    pub alive: bool,
    /// 控制通道可写。
    pub socket_ok: bool,
    /// UHID 设备已创建。
    pub uhid_created: bool,
    /// scrcpy-server 进程存活。
    pub server_alive: bool,
    /// 连续失败次数（会话侧统计）。
    pub consecutive_failures: i32,
    /// 最近一次错误描述。
    pub last_error: String,
    /// 已运行秒数。
    pub uptime_s: f64,
    /// 已发送报文数。
    pub sent: i64,
}

impl HealthReport {
    /// 健康 = 服务端存活且控制通道可写（对应 Python 的 alive and socket_ok）。
    pub fn is_healthy(&self) -> bool {
        self.alive && self.socket_ok
    }
}

// ---------------------------------------------------------------------------
// 时钟与外部端口（trait 化，便于测试注入）
// ---------------------------------------------------------------------------

/// 单调时钟（对应 Python 注入的 clock()，默认 time.perf_counter）。
pub trait Clock: Send + Sync + fmt::Debug {
    /// 当前时刻（秒，单调递增，不要求从 0 开始）。
    fn now(&self) -> f64;
}

/// 默认时钟：进程内单调计时。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        use std::sync::OnceLock;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
    }
}

/// 桌面绝对光标查询的三态结果。
///
/// Python 用 callable(getattr(desktop, "cursor_pos", None)) 区分三种情况：有 getter 且拿到
/// 坐标 / 有 getter 但返回 None / 根本没有 getter（退回累加 Raw Input 相对位移）。Rust 的
/// trait 方法一定存在，因此用本枚举表达后两种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorQuery {
    /// 绝对坐标（屏幕物理像素）。
    Position(i32, i32),
    /// 有查询能力但本次读不到（保持上次位置，不累加相对位移）。
    Unavailable,
    /// 桌面层没有绝对光标源（退回累加相对位移，仅测试替身会出现）。
    #[allow(dead_code)] // 由 DesktopPort 实现方按需返回，模块内没有构造点。
    Unsupported,
}

/// 会话端口（tablet/session.py 的实际调用面）。
///
/// 约定：
/// - 返回 Option<...> 的方法是可扩展点，None 表示"会话没有这个方法"（对应 Python 的
///   callable(getattr(...)) 检查）；
/// - 返回 Result<...> 表示会失败（对应 Python 的 try/except）；
/// - 所有方法都借 &self，实现内部自行加锁——Router 会在 RawInput 线程、键盘钩子线程、
///   sender 线程、supervisor 线程上调用它们。
pub trait SessionPort: Send + Sync + fmt::Debug {
    /// 写一条鼠标 HID 报告（对应 TabletSession.send_mouse）。
    fn send_mouse(&self, buttons: i32, dx: i32, dy: i32, wheel: i32) -> Result<(), RouterError>;

    /// 写一条键盘报告（modifier + 最多 6 个 usage）。
    fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), RouterError>;

    /// 当前设备事实；未连接时为 None。
    fn facts(&self) -> Option<DeviceFacts>;

    /// 健康快照。Ok(None) = 会话不提供 health()（视为健康）。
    fn health(&self) -> Result<Option<HealthReport>, RouterError> {
        Ok(None)
    }

    /// 把文本写进平板剪贴板（SET_CLIPBOARD）。
    fn set_clipboard(&self, _text: &str) -> Result<(), RouterError> {
        Ok(())
    }

    /// 停止当前会话（设备切换/断开时调用）。
    fn stop(&self) {}

    /// 建立会话并返回设备事实。
    fn start(&self) -> Result<DeviceFacts, RouterError> {
        Err(RouterError::device_offline(
            "session 没有 DeviceFacts 且不支持 start()",
        ))
    }

    /// 在已有会话上重连；None = 会话不支持 reconnect()。
    fn reconnect(&self) -> Option<Result<Option<DeviceFacts>, RouterError>> {
        None
    }

    /// 请求会话把 pointer_speed 归零；None = 不支持。
    fn ensure_pointer_speed(&self) -> Option<Result<(), RouterError>> {
        None
    }

    /// 设置 pointer_speed；None = 不支持。
    fn set_pointer_speed(&self, _speed: i32) -> Option<Result<(), RouterError>> {
        None
    }

    /// 把系统光标移到绝对坐标；false = 不支持。
    fn set_cursor_pos(&self, _x: i32, _y: i32) -> bool {
        false
    }

    /// 请求取消正在进行的首连（对应 cancel_start() / abort_start()）。
    ///
    /// 返回 true 表示会话支持取消并已收到请求。
    fn cancel_start(&self) -> bool {
        false
    }

    /// 直接发一个 VK（诊断/扩展面；Router 核心路径不调用）。
    fn send_keycode(&self, vk: i32) -> Result<(), RouterError> {
        let usage = vk.clamp(0, 255) as u8;
        self.send_key(0, &[usage])
    }

    /// 注入文本（扩展面；Router 核心路径不调用）。
    fn inject_text(&self, _text: &str) -> Result<(), RouterError> {
        Ok(())
    }

    /// 会话是否已启动（扩展面；Router 核心路径不调用）。
    fn started(&self) -> bool {
        false
    }

    /// 注册设备事实回调（可选扩展）；返回 true 表示已接受。
    fn set_facts_callback(&self, _callback: Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>) -> bool {
        false
    }

    /// 注册剪贴板回调（可选扩展）；返回 true 表示已接受。
    fn set_clipboard_callback(&self, _callback: Option<Arc<dyn Fn(String) + Send + Sync>>) -> bool {
        false
    }
}

/// 光标遮罩端口（win/mask.py）。
pub trait MaskPort: Send + Sync + fmt::Debug {
    /// 显示遮罩并锁定光标到 clip（1 像素条，None = 不锁）。
    fn show(&self, clip: Option<Rect>) -> Result<(), RouterError>;

    /// 释放遮罩与 ClipCursor；必须可重复调用。
    fn release(&self);

    /// 隐藏遮罩（等价于 release，仅供调用方语义清晰）。
    fn hide(&self) {
        self.release();
    }

    /// 直接设置"光标是否隐藏"（扩展面；Router 用 show / release 表达）。
    fn set_cursor_hidden(&self, _hidden: bool) {}

    /// 诊断字符串（cap.cursor 日志用）。
    fn diagnose(&self) -> String {
        String::new()
    }
}

/// 虚拟桌面端口（win/desktop.py）。
pub trait DesktopPort: Send + Sync + fmt::Debug {
    /// 当前显示器列表。
    fn monitors(&self) -> Vec<MonitorInfo>;

    /// 虚拟桌面并集（可为负原点；空桌面返回空矩形）。
    fn bounding_box(&self) -> Rect;

    /// 系统光标绝对坐标。
    fn cursor_pos(&self) -> CursorQuery {
        CursorQuery::Unsupported
    }

    /// band 像素范围内命中的外轮廓边段。
    fn segments_at(&self, _x: i32, _y: i32, _band: i32) -> Vec<EdgeSegment> {
        Vec::new()
    }

    /// 角落裁决（DESIGN 6.3.6）；None 时 Router 退回"唯一候选"规则。
    fn resolve_corner(
        &self,
        _segments: &[EdgeSegment],
        _delta: (i32, i32),
        _priority: Option<Edge>,
    ) -> Option<EdgeSegment> {
        None
    }

    /// 当前拓扑的全部外轮廓边段。
    fn exterior_segments(&self) -> Vec<EdgeSegment> {
        Vec::new()
    }

    /// 某边段是否仍存在于当前拓扑；None = 桌面层没有该方法。
    fn segment_exists(&self, _segment: &EdgeSegment) -> Option<bool> {
        None
    }

    /// 重新枚举显示器（拓扑变化时调用）。
    fn refresh(&self) -> Result<(), RouterError> {
        Ok(())
    }

    /// 设置系统光标绝对坐标；false = 不支持。
    fn set_cursor_pos(&self, _x: i32, _y: i32) -> bool {
        false
    }
}

/// 键位表端口（tablet/keymap.py）。
///
/// Python 的 _keymap_report 会依次尝试 on_key / feed / handle / translate 并归一化返回值；
/// Rust 侧把这份契约收敛成单个 on_key，None 表示"本次没有可转发报告"。
pub trait KeymapPort: Send + Sync + fmt::Debug {
    /// 喂一个按键事件，返回 (modifier, usages)。
    fn on_key(&self, event: &KeyEvent) -> Option<(u8, Vec<u8>)>;

    /// 该键是否原样透传给 Windows。
    fn is_passthrough(&self, event: &KeyEvent) -> bool;

    /// 清空按键状态（交还/急停时调用）。
    fn reset(&self);
}

/// 内置键位表适配器：把 tablet::keymap::Keymap（需要 &mut self）包成 KeymapPort（&self），
/// 对应 Python _ensure_keymap_locked 的惰性构造。
struct BuiltinKeymap {
    /// 内部键位表。
    inner: Mutex<Keymap>,
}

impl BuiltinKeymap {
    /// 用透传键名构造（名字解析失败由 Keymap 自己记 warning）。
    fn new(names: Vec<String>) -> Self {
        Self {
            inner: Mutex::new(Keymap::with_passthrough(names)),
        }
    }

    /// 把逻辑层事件转成键位表事件（两份同形结构，字段名一致）。
    fn convert(event: &KeyEvent) -> crate::tablet::keymap::KeyEvent {
        crate::tablet::keymap::KeyEvent::new(event.vk, event.scan, event.is_extended, event.down)
    }
}

impl fmt::Debug for BuiltinKeymap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BuiltinKeymap")
    }
}

impl KeymapPort for BuiltinKeymap {
    fn on_key(&self, event: &KeyEvent) -> Option<(u8, Vec<u8>)> {
        let converted = Self::convert(event);
        let mut keymap = lock_mutex(&self.inner);
        Some(keymap.on_key(&converted))
    }

    fn is_passthrough(&self, event: &KeyEvent) -> bool {
        let converted = Self::convert(event);
        lock_mutex(&self.inner).is_passthrough(&converted)
    }

    fn reset(&self) {
        lock_mutex(&self.inner).reset();
    }
}

/// 取锁；被毒化时继续用内部状态（清理路径绝不能再 panic）。
fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Python int(x) 的截断语义（NaN/Inf 取 0，避免 panic）。
fn trunc_i32(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let truncated = value.trunc();
    if truncated > f64::from(i32::MAX) {
        i32::MAX
    } else if truncated < f64::from(i32::MIN) {
        i32::MIN
    } else {
        truncated as i32
    }
}

/// Python 内建 round()（银行家舍入）后再取整。
fn round_ties_even_i32(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let rounded = value.round_ties_even();
    if rounded > f64::from(i32::MAX) {
        i32::MAX
    } else if rounded < f64::from(i32::MIN) {
        i32::MIN
    } else {
        rounded as i32
    }
}

/// 是否打开 debug 级日志（PADLEND_ROUTER_DEBUG 非空即视为开启）。
fn router_debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PADLEND_ROUTER_DEBUG") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    })
}

/// 打一行 router debug 日志（门控；热路径不会格式化）。
macro_rules! rdebug {
    ($($arg:tt)*) => {
        if router_debug_enabled() {
            eprintln!("[router] {}", format_args!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// 平板指针位置估计器（tablet/cursor.py，DESIGN 8.2 冻结契约）
// ---------------------------------------------------------------------------

/// 平板指针位置的唯一真相源（只在 Router 锁内访问）。
///
/// 换算链（DESIGN 6.4.1）：PC 物理像素位移 Δp --×gain--> HID 相对计数 c（clamp ±127）
/// --×XScale--> 平板像素位移 Δx。关键不变量 I3：每次 feed 之后立刻 clamp。
///
/// TODO(B2-M4)：tablet/cursor.rs 落地后本类型应迁出去复用，避免两份实现。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorEstimator {
    /// 平板宽（浮点，与 Python 的 self.W 一致）。
    width: f64,
    /// 平板高。
    height: f64,
    /// 1 计数对应的平板像素（xscale，<= 0 时退化为 1.0）。
    scale: f64,
    /// 估计的平板指针 X。
    pub x: f64,
    /// 估计的平板指针 Y。
    pub y: f64,
    /// 小数残差（防慢速拖动丢步）。
    residual: [f64; 2],
    /// 最近一次实际发出的 HID 计数。
    last_report: (i32, i32),
}

impl CursorEstimator {
    /// 构造；尺寸非法时返回 E105（Python 抛 ValueError）。
    pub fn new(width: i32, height: i32, xscale: f64) -> Result<Self, RouterError> {
        if width <= 0 || height <= 0 {
            return Err(RouterError::handshake_failed(format!(
                "invalid tablet size {width}x{height}"
            )));
        }
        Ok(Self {
            width: f64::from(width),
            height: f64::from(height),
            scale: if xscale > 0.0 { xscale } else { 1.0 },
            x: 0.0,
            y: 0.0,
            residual: [0.0, 0.0],
            last_report: (0, 0),
        })
    }

    /// 平板宽（int(W)）。
    pub fn width(&self) -> i32 {
        trunc_i32(self.width)
    }

    /// 平板高。
    pub fn height(&self) -> i32 {
        trunc_i32(self.height)
    }

    /// 1 计数对应的平板像素。
    pub fn xscale(&self) -> f64 {
        self.scale
    }

    /// 当前位置。
    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// 最近一次发出的 HID 计数。
    pub fn last_report(&self) -> (i32, i32) {
        self.last_report
    }

    /// 重置位置；缺省归中（旋转/重启后指针位置未知，DESIGN 6.4.3）。
    pub fn reset(&mut self, x: Option<f64>, y: Option<f64>) {
        let target_x = x.unwrap_or((self.width - 1.0) / 2.0);
        let target_y = y.unwrap_or((self.height - 1.0) / 2.0);
        self.x = target_x;
        self.y = target_y;
        self.residual = [0.0, 0.0];
        self.clamp();
    }

    /// 与系统行为对齐：PointerController 也会夹紧，我们不夹就会越走越偏。
    pub fn clamp(&mut self) {
        self.x = self.x.clamp(0.0, self.width - 1.0);
        self.y = self.y.clamp(0.0, self.height - 1.0);
    }

    /// 估计指针是否已抵达平板边界；多条边同时命中时按 DESIGN 6.4.2 的优先级。
    pub fn hit(&self) -> Option<Edge> {
        if self.x <= 0.0 {
            return Some(Edge::Left);
        }
        if self.x >= self.width - 1.0 {
            return Some(Edge::Right);
        }
        if self.y <= 0.0 {
            return Some(Edge::Top);
        }
        if self.y >= self.height - 1.0 {
            return Some(Edge::Bottom);
        }
        None
    }

    /// PC 物理像素位移 → 一帧 HID 相对计数（±clamp）。
    pub fn feed(&mut self, dpx: f64, dpy: f64, gain: f64, clamp: i32) -> (i32, i32) {
        let limit = clamp.clamp(1, C::HID_CLAMP);
        self.residual[0] += dpx * gain;
        self.residual[1] += dpy * gain;

        let mut cx = trunc_i32(self.residual[0]);
        let mut cy = trunc_i32(self.residual[1]);
        self.residual[0] -= f64::from(cx);
        self.residual[1] -= f64::from(cy);

        cx = cx.clamp(-limit, limit);
        cy = cy.clamp(-limit, limit);

        self.x += f64::from(cx) * self.scale;
        self.y += f64::from(cy) * self.scale;
        self.clamp();
        self.last_report = (cx, cy);
        (cx, cy)
    }

    /// 相对设备做"绝对定位"：穿屏时把平板指针搬到入口点（DESIGN 6.4.2）。
    ///
    /// 返回需要一次性 flush 的 HID 报告列表（warp 的 26 个报文 ≈ 3ms）。
    pub fn warp_to(&mut self, target_x: f64, target_y: f64) -> Vec<(i32, i32)> {
        let mut out: Vec<(i32, i32)> = Vec::new();
        let tx = target_x.clamp(0.0, self.width - 1.0);
        let ty = target_y.clamp(0.0, self.height - 1.0);
        for _ in 0..64 {
            let dx = tx - self.x;
            let dy = ty - self.y;
            if dx.abs() < self.scale && dy.abs() < self.scale {
                break;
            }
            let cx = trunc_i32(dx / self.scale).clamp(-C::HID_CLAMP, C::HID_CLAMP);
            let cy = trunc_i32(dy / self.scale).clamp(-C::HID_CLAMP, C::HID_CLAMP);
            if cx == 0 && cy == 0 {
                // scale > 1 时残余小于一个整数步长，无法表达；交给 clamp 收敛。
                break;
            }
            self.x += f64::from(cx) * self.scale;
            self.y += f64::from(cy) * self.scale;
            out.push((cx, cy));
        }
        self.clamp();
        if let Some(last) = out.last().copied() {
            self.last_report = last;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 发送报文 / 事件 / 线程基础设施
// ---------------------------------------------------------------------------

/// 一条鼠标 HID 报告（对应 Python 的 _SendMsg）。
///
/// 全零的"真实松键报告"与空闲心跳必须区分：前者走队列、算延迟；后者是保活。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SendMsg {
    /// 按键位图。
    pub buttons: i32,
    /// X 相对位移（HID 计数）。
    pub dx: i32,
    /// Y 相对位移。
    pub dy: i32,
    /// 滚轮 detent。
    pub wheel: i32,
    /// 是否属于 warp/pin 的 burst 批次（可突破单帧 8 条上限）。
    pub burst: bool,
    /// 是否心跳（不记延迟、可被背压丢弃）。
    pub heartbeat: bool,
    /// 入队时刻（clock()）；写出成功后用它算 DESIGN 7.4 的 rtt_ms。
    pub stamp: f64,
}

impl SendMsg {
    /// 普通报告。
    fn report(buttons: i32, dx: i32, dy: i32, wheel: i32, stamp: f64) -> Self {
        Self {
            buttons,
            dx,
            dy,
            wheel,
            burst: false,
            heartbeat: false,
            stamp,
        }
    }

    /// burst 报告（批量 warp / pin，buttons 恒为 0）。
    fn burst(dx: i32, dy: i32, stamp: f64) -> Self {
        Self {
            buttons: 0,
            dx,
            dy,
            wheel: 0,
            burst: true,
            heartbeat: false,
            stamp,
        }
    }

    /// 心跳报告（必须携带当前有效按键位图，PITFALLS 2.2）。
    fn heartbeat(buttons: i32) -> Self {
        Self {
            buttons,
            dx: 0,
            dy: 0,
            wheel: 0,
            burst: false,
            heartbeat: true,
            stamp: 0.0,
        }
    }

    /// 是否心跳。
    fn is_heartbeat(&self) -> bool {
        self.heartbeat
    }
}

/// 锁外派发的事件（回调缓冲）。
#[derive(Debug)]
enum RouterEvent {
    /// 状态迁移 (old, new, reason)。
    State(State, State, String),
    /// 指标快照。
    Metrics(Snapshot),
    /// 朝向/尺寸变化后的设备事实。
    Orientation(DeviceFacts),
    /// 收到的平板剪贴板文本。
    Clipboard(String),
}

/// 回调集合（独立锁；派发时先克隆 Arc 再放开锁，回调里可以安全重入 Router）。
#[derive(Default)]
struct Callbacks {
    /// 状态迁移回调。
    state: Option<Arc<dyn Fn(State, State, &str) + Send + Sync>>,
    /// 指标回调。
    metrics: Option<Arc<dyn Fn(&Snapshot) + Send + Sync>>,
    /// 朝向回调。
    orientation: Option<Arc<dyn Fn(&DeviceFacts) + Send + Sync>>,
    /// 剪贴板回调。
    clipboard: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

/// 线程句柄与线程标识（stop 里 join；从自己线程调用时跳过）。
#[derive(Default)]
struct Threads {
    /// sender 线程。
    sender: Option<JoinHandle<()>>,
    /// sender 线程 id。
    sender_id: Option<ThreadId>,
    /// supervisor 线程。
    supervisor: Option<JoinHandle<()>>,
    /// supervisor 线程 id。
    supervisor_id: Option<ThreadId>,
}

/// 停止信号（对应 Python 的 threading.Event）。
struct StopFlag {
    /// 无锁快速判定。
    flag: AtomicBool,
    /// 与条件变量配对的状态。
    guard: Mutex<bool>,
    /// 唤醒等待者。
    signal: Condvar,
}

impl StopFlag {
    /// 新建（未置位）。
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            guard: Mutex::new(false),
            signal: Condvar::new(),
        }
    }

    /// 置位并唤醒所有等待者。
    fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        *lock_mutex(&self.guard) = true;
        self.signal.notify_all();
    }

    /// 清位（start 时对应 Python 的 stop_event.clear()）。
    fn clear(&self) {
        *lock_mutex(&self.guard) = false;
        self.flag.store(false, Ordering::SeqCst);
    }

    /// 是否已置位。
    fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 等待置位或超时（对应 stop_event.wait(timeout)）。
    fn wait_timeout(&self, timeout: Duration) {
        let guard = lock_mutex(&self.guard);
        if *guard {
            return;
        }
        let _unused = self.signal.wait_timeout(guard, timeout);
    }
}

/// 全部可变状态（对应 Python Router 的实例字段，除发送队列/线程/回调）。
struct Inner {
    /// start 是否调用过。
    started: bool,
    /// 是否已停止。
    stopped: bool,
    /// 当前状态。
    state: State,
    /// 平板摆位。
    placement: Option<Placement>,
    /// 平板指针估计器。
    estimator: Option<CursorEstimator>,
    /// 设备事实。
    facts: Option<DeviceFacts>,
    /// 是否手动接管。
    manual: bool,
    /// 最近一次已知的 PC 绝对光标。
    pc_cursor: Option<(i32, i32)>,
    /// 当前物理按键位图。
    buttons: i32,
    /// 滚轮余数（120 单位/格）。
    wheel_accum: i32,

    /// ARMING 观察窗截止时刻。
    arm_deadline: f64,
    /// ARMING 累计外推像素。
    arm_accum: f64,
    /// 本次接管压住的外轮廓边段。
    arm_segment: Option<EdgeSegment>,
    /// 该边段所属显示器。
    arm_monitor: Option<MonitorInfo>,

    /// 入口边（= 吸附边的对边）。
    entry_edge: Option<Edge>,
    /// 出口溢出累计。
    overflow: f64,
    /// 入口前已按住的键：保持中性直到物理松键。
    neutral_until_release: bool,
    /// 是否已记录 cap.hit。
    hit_logged: bool,
    /// RETURNING 去抖截止时刻。
    return_deadline: f64,

    /// 连续健康检查失败次数。
    health_failures: u32,
    /// 连续写失败次数。
    write_failures: u32,
    /// 重连尝试次数（决定退避档位）。
    reconnect_attempt: u32,
    /// 下次允许重连的时刻。
    next_reconnect_at: f64,
    /// supervisor 是否正在连接。
    connect_in_progress: bool,
    /// 用户主动断开后禁止自动重连。
    device_suspended: bool,
    /// 上次 pointer_speed 复查时刻。
    last_pointer_poll: Option<f64>,
    /// 上次显示器签名。
    desktop_sig: Option<Vec<(i64, Rect)>>,

    /// 键位表（惰性构造；注入时 keymap_loaded 直接为真）。
    keymap: Option<Arc<dyn KeymapPort>>,
    /// 是否已尝试过键位表初始化。
    keymap_loaded: bool,
    /// 透传键的 VK 集合（由配置解析）。
    passthrough_vks: HashSet<i32>,
    /// 按下的 Ctrl 侧（0 左 / 1 右）。
    ctrl_sides: HashSet<i32>,
    /// 按下的 Alt 侧。
    alt_sides: HashSet<i32>,
    /// CAPTURED 期间已吞掉的 Win 键。
    swallowed_win: HashSet<i32>,
    /// 是否已记录 kbd.suppress.on。
    suppress_logged: bool,
}

impl Inner {
    /// 全新状态（与 Python __init__ 的字段初值一致）。
    fn new(config: &Config, keymap: Option<Arc<dyn KeymapPort>>) -> Self {
        let passthrough_vks = resolve_passthrough(config);
        Self {
            started: false,
            stopped: false,
            state: State::Idle,
            placement: None,
            estimator: None,
            facts: None,
            manual: false,
            pc_cursor: None,
            buttons: 0,
            wheel_accum: 0,
            arm_deadline: 0.0,
            arm_accum: 0.0,
            arm_segment: None,
            arm_monitor: None,
            entry_edge: None,
            overflow: 0.0,
            neutral_until_release: false,
            hit_logged: false,
            return_deadline: 0.0,
            health_failures: 0,
            write_failures: 0,
            reconnect_attempt: 0,
            next_reconnect_at: 0.0,
            connect_in_progress: false,
            device_suspended: false,
            last_pointer_poll: None,
            desktop_sig: None,
            keymap_loaded: keymap.is_some(),
            keymap,
            passthrough_vks,
            ctrl_sides: HashSet::new(),
            alt_sides: HashSet::new(),
            swallowed_win: HashSet::new(),
            suppress_logged: false,
        }
    }

    /// 心跳/捕获时用的有效按键位图（中性期强制 0）。
    fn effective_buttons(&self) -> i32 {
        if self.neutral_until_release {
            0
        } else {
            self.buttons
        }
    }

    /// 取滚轮 detent（余数留到下次，PITFALLS 2.3）。
    fn take_wheel(&mut self, units: i32) -> i32 {
        if units != 0 {
            self.wheel_accum += units;
        }
        let detents = self.wheel_accum / C::WHEEL_UNITS_PER_DETENT;
        if detents != 0 {
            self.wheel_accum -= detents * C::WHEEL_UNITS_PER_DETENT;
        }
        detents
    }
}

/// 键位表透传名字解析（对应 Python _resolve_passthrough）。
fn resolve_passthrough(config: &Config) -> HashSet<i32> {
    let mut resolved: HashSet<i32> = HashSet::new();
    for raw_name in &config.keyboard.passthrough_keys {
        let name = raw_name.trim().to_uppercase();
        if name.is_empty() {
            continue;
        }
        match lookup_vk_name(&name) {
            Some(vk) => {
                resolved.insert(vk);
            }
            None => eprintln!("[router] router.passthrough_unknown name={raw_name}"),
        }
    }
    resolved
}

/// 名字 → VK；优先用 tablet::keymap::VK_NAME_TO_CODE，缺失时退回内置表。
fn lookup_vk_name(name: &str) -> Option<i32> {
    if let Some(vk) = VK_NAME_TO_CODE.get(name).copied() {
        return Some(vk);
    }
    Some(match name {
        "VK_LWIN" => 0x5B,
        "VK_RWIN" => 0x5C,
        "VK_APPS" => 0x5D,
        "VK_LCONTROL" => 0xA2,
        "VK_RCONTROL" => 0xA3,
        "VK_LSHIFT" => 0xA0,
        "VK_RSHIFT" => 0xA1,
        "VK_LMENU" => 0xA4,
        "VK_RMENU" => 0xA5,
        "VK_CONTROL" => 0x11,
        "VK_SHIFT" => 0x10,
        "VK_MENU" => 0x12,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// PadLend 的核心状态机（对应 Python Router，DESIGN 8.2 冻结接口 + 测试注入点）。
pub struct Router {
    /// 配置（只读；设备序列号见 requested_serial）。
    cfg: Config,
    /// 会话端口。
    session: Arc<dyn SessionPort>,
    /// 桌面端口。
    desktop: Arc<dyn DesktopPort>,
    /// 遮罩端口。
    mask: Arc<dyn MaskPort>,
    /// 时���。
    clock: Arc<dyn Clock>,
    /// 指标聚合器（与 Router 共用注入时钟）。
    metrics: Metrics,
    /// 全部可变状态。
    inner: Mutex<Inner>,
    /// 发送队列（独立锁，见模块头）。
    queue: Mutex<VecDeque<SendMsg>>,
    /// 停止信号。
    stop_flag: StopFlag,
    /// 后台线程。
    threads: Mutex<Threads>,
    /// 回调集合。
    callbacks: Mutex<Callbacks>,
    /// 设备切换串行锁（对应 Python _connect_lock）。
    connect_lock: Mutex<()>,
    /// 用户最近一次请求连接的序列号（替代 Python 改写 cfg.device.serial）。
    requested_serial: Mutex<Option<String>>,
}

impl fmt::Debug for Router {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock_inner();
        formatter
            .debug_struct("Router")
            .field("state", &inner.state)
            .field("manual", &inner.manual)
            .field("connected", &inner.facts.is_some())
            .finish()
    }
}

impl Router {
    /// 用配置与四个外部端口构造（时钟默认 SystemClock、键位表默认惰性构造）。
    pub fn new(
        cfg: Config,
        session: Arc<dyn SessionPort>,
        desktop: Arc<dyn DesktopPort>,
        mask: Arc<dyn MaskPort>,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let metrics = Self::build_metrics(&clock);
        let requested_serial = if cfg.device.serial.is_empty() {
            None
        } else {
            Some(cfg.device.serial.clone())
        };
        Self {
            inner: Mutex::new(Inner::new(&cfg, None)),
            cfg,
            session,
            desktop,
            mask,
            clock,
            metrics,
            queue: Mutex::new(VecDeque::new()),
            stop_flag: StopFlag::new(),
            threads: Mutex::new(Threads::default()),
            callbacks: Mutex::new(Callbacks::default()),
            connect_lock: Mutex::new(()),
            requested_serial: Mutex::new(requested_serial),
        }
    }

    /// 注入可推进的时钟（测试用；对应 Python 构造函数的 clock 参数）。
    /// 注入进程 CPU/内存采样器（真实运行时用 win::process::Win32ProcessUsage）。
    pub fn with_process_usage(mut self, usage: Box<dyn crate::logic::metrics::ProcessUsage>) -> Self {
        self.metrics.set_process_usage(usage);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.metrics = Self::build_metrics(&clock);
        self.clock = clock;
        self
    }

    /// 注入键位表（对应 Python 构造函数的 keymap 参数；注入后不再惰性构造）。
    pub fn with_keymap(self, keymap: Arc<dyn KeymapPort>) -> Self {
        {
            let mut inner = self.lock_inner();
            inner.keymap = Some(keymap);
            inner.keymap_loaded = true;
        }
        self
    }

    /// 构造指标聚合器（窗口 1s、与 Router 共用时钟、无 CPU/内存采样）。
    fn build_metrics(clock: &Arc<dyn Clock>) -> Metrics {
        let handle = Arc::clone(clock);
        match Metrics::with_parts(1.0, move || handle.now(), NullProcessUsage) {
            Ok(metrics) => metrics,
            Err(_) => Metrics::with_defaults(),
        }
    }

    /// 取内部状态锁（被毒化时继续用内部状态）。
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        lock_mutex(&self.inner)
    }

    /// 取发送队列锁。
    fn lock_queue(&self) -> MutexGuard<'_, VecDeque<SendMsg>> {
        lock_mutex(&self.queue)
    }

    /// 取回调锁。
    fn lock_callbacks(&self) -> MutexGuard<'_, Callbacks> {
        lock_mutex(&self.callbacks)
    }

    /// 取线程锁。
    fn lock_threads(&self) -> MutexGuard<'_, Threads> {
        lock_mutex(&self.threads)
    }

    /// 取连接串行锁。
    fn lock_connect(&self) -> MutexGuard<'_, ()> {
        lock_mutex(&self.connect_lock)
    }

    // -- 只读属性 ----------------------------------------------------------

    /// 当前状态。
    pub fn state(&self) -> State {
        self.lock_inner().state
    }

    /// 平板摆位。
    pub fn placement(&self) -> Option<Placement> {
        self.lock_inner().placement.clone()
    }

    /// 是否手动接管。
    pub fn manual(&self) -> bool {
        self.lock_inner().manual
    }

    /// 平板指针估计器快照。
    pub fn estimator(&self) -> Option<CursorEstimator> {
        self.lock_inner().estimator
    }

    /// 当前设备事实；未连接时为 None。
    pub fn facts(&self) -> Option<DeviceFacts> {
        self.lock_inner().facts.clone()
    }

    /// 是否已拿到设备事实（GUI 设备页/画布据此显示真实尺寸）。
    pub fn connected(&self) -> bool {
        let inner = self.lock_inner();
        inner.facts.is_some() && inner.state != State::Fault
    }

    /// 用户最近一次请求连接的序列号（对应 Python 里被改写的 cfg.device.serial）。
    pub fn requested_serial(&self) -> Option<String> {
        lock_mutex(&self.requested_serial).clone()
    }

    /// DESIGN 7.4 指标快照（GUI 也可等 supervisor 的 on_metrics 推送）。
    pub fn metrics_snapshot(&self) -> Snapshot {
        self.metrics.snapshot()
    }

    // -- 回调注入（对应 Python 的属性赋值） --------------------------------

    /// 注入状态迁移回调（old, new, reason）。
    pub fn set_on_state_changed<F>(&self, callback: F)
    where
        F: Fn(State, State, &str) + Send + Sync + 'static,
    {
        self.lock_callbacks().state = Some(Arc::new(callback));
    }

    /// 移除状态迁移回调。
    pub fn clear_on_state_changed(&self) {
        self.lock_callbacks().state = None;
    }

    /// 注入指标回调。
    pub fn set_on_metrics<F>(&self, callback: F)
    where
        F: Fn(&Snapshot) + Send + Sync + 'static,
    {
        self.lock_callbacks().metrics = Some(Arc::new(callback));
    }

    /// 注入朝向回调。
    pub fn set_on_orientation_changed<F>(&self, callback: F)
    where
        F: Fn(&DeviceFacts) + Send + Sync + 'static,
    {
        self.lock_callbacks().orientation = Some(Arc::new(callback));
    }

    /// 注入剪贴板回调。
    pub fn set_on_clipboard_received<F>(&self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.lock_callbacks().clipboard = Some(Arc::new(callback));
    }

    /// 统一派发事件（锁外；回调 panic 只记录，不影响状态机）。
    fn dispatch(&self, events: Vec<RouterEvent>) {
        for event in events {
            match event {
                RouterEvent::State(old, new, reason) => {
                    let callback = match &self.lock_callbacks().state {
                        Some(callback) => Arc::clone(callback),
                        None => continue,
                    };
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        callback(old, new, reason.as_str());
                    }));
                    if outcome.is_err() {
                        eprintln!("[router] router.state_callback_failed");
                    }
                }
                RouterEvent::Metrics(snapshot) => {
                    let callback = match &self.lock_callbacks().metrics {
                        Some(callback) => Arc::clone(callback),
                        None => continue,
                    };
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        callback(&snapshot);
                    }));
                    if outcome.is_err() {
                        eprintln!("[router] router.metrics_callback_failed");
                    }
                }
                RouterEvent::Orientation(facts) => {
                    let callback = match &self.lock_callbacks().orientation {
                        Some(callback) => Arc::clone(callback),
                        None => continue,
                    };
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        callback(&facts);
                    }));
                    if outcome.is_err() {
                        eprintln!("[router] router.orientation_callback_failed");
                    }
                }
                RouterEvent::Clipboard(text) => {
                    let callback = match &self.lock_callbacks().clipboard {
                        Some(callback) => Arc::clone(callback),
                        None => continue,
                    };
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        callback(text.as_str());
                    }));
                    if outcome.is_err() {
                        eprintln!("[router] router.clipboard_callback_failed");
                    }
                }
            }
        }
    }

    // -- 会话回调注册 ------------------------------------------------------

    /// 把 Router 的 facts / clipboard 入口注册进会话（对应 Python __init__ 的注册）。
    ///
    /// Rust 拿不到构造期的 &Arc<Self>，因此单独暴露；start() 会幂等补调一次。
    /// 用 Weak 打破 Router 与 Session 之间的引用环。
    pub fn register_session_callbacks(self: &Arc<Self>) {
        let weak_facts = Arc::downgrade(self);
        let facts_callback: Arc<dyn Fn(DeviceFacts) + Send + Sync> = Arc::new(move |facts| {
            if let Some(router) = weak_facts.upgrade() {
                router.on_session_facts(facts);
            }
        });
        if !self.session.set_facts_callback(Some(facts_callback)) {
            rdebug!("router.facts_callback_register_skipped");
        }
        let weak_clipboard = Arc::downgrade(self);
        let clipboard_callback: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |text| {
            if let Some(router) = weak_clipboard.upgrade() {
                router.on_session_clipboard(text.as_str());
            }
        });
        if !self.session.set_clipboard_callback(Some(clipboard_callback)) {
            rdebug!("router.clipboard_callback_register_skipped");
        }
    }

    // -- 生命周期 ----------------------------------------------------------

    /// 启动 sender / supervisor 线程；可重复调用（第二次起为空操作）。
    ///
    /// **非阻塞**：设备连接（可能耗时数十秒的 session.start()）交给 supervisor 线程执行。
    /// 这里只读取已有的 DeviceFacts；没有设备时立即 IDLE→FAULT 并让 supervisor 马上发起
    /// 首连，GUI 线程不会被 ADB 卡住。
    pub fn start(self: &Arc<Self>) {
        {
            let mut inner = self.lock_inner();
            if inner.started {
                return;
            }
            inner.started = true;
            inner.stopped = false;
        }
        self.stop_flag.clear();
        self.register_session_callbacks();
        let auto_connect = self.cfg.device.auto_connect;
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if !auto_connect {
                inner.device_suspended = true;
                inner.next_reconnect_at = f64::INFINITY;
                inner.facts = None;
                inner.estimator = None;
                eprintln!("[router] router.start auto_connect=false，等待用户连接设备");
            } else {
                if let Some(facts) = self.read_facts() {
                    inner.facts = Some(facts);
                }
                let outcome = self.start_prepare(&mut inner);
                match outcome {
                    Ok(()) => {}
                    Err(err) => {
                        eprintln!(
                            "[router] router.start_waiting code={} detail={}",
                            err.code(),
                            err.detail()
                        );
                        self.fault(
                            &mut inner,
                            &mut events,
                            err.code(),
                            &format!("启动等待设备: {}", err.detail()),
                        );
                        inner.next_reconnect_at = self.clock.now();
                    }
                }
            }
            rdebug!("router.start state={}", inner.state.as_str());
        }
        self.dispatch(events);
        self.spawn_threads();
    }

    /// start() 的锁内准备：估计器、显示器签名、初始光标。
    fn start_prepare(&self, inner: &mut Inner) -> Result<(), RouterError> {
        self.ensure_estimator(inner)?;
        inner.desktop_sig = Some(self.monitor_signature());
        if inner.pc_cursor.is_none() {
            inner.pc_cursor = Some(self.query_cursor_pos().unwrap_or_else(|| self.default_cursor()));
        }
        Ok(())
    }

    /// 起两个后台线程（对应 Python 的两条 Thread）。
    fn spawn_threads(self: &Arc<Self>) {
        let sender_router = Arc::clone(self);
        match thread::Builder::new()
            .name("padlend-sender".to_string())
            .spawn(move || sender_router.sender_loop())
        {
            Ok(handle) => {
                let id = handle.thread().id();
                let mut threads = self.lock_threads();
                threads.sender = Some(handle);
                threads.sender_id = Some(id);
            }
            Err(err) => eprintln!("[router] sender.spawn_failed err={err}"),
        }
        let supervisor_router = Arc::clone(self);
        match thread::Builder::new()
            .name("padlend-supervisor".to_string())
            .spawn(move || supervisor_router.supervisor_loop())
        {
            Ok(handle) => {
                let id = handle.thread().id();
                let mut threads = self.lock_threads();
                threads.supervisor = Some(handle);
                threads.supervisor_id = Some(id);
            }
            Err(err) => eprintln!("[router] supervisor.spawn_failed err={err}"),
        }
    }

    /// sender 线程主体：1000Hz 出帧（对应 Python _sender_loop）。
    ///
    /// 节奏用真实 Instant（Python 这里也用 perf_counter 而不是注入时钟）。
    fn sender_loop(self: Arc<Self>) {
        let hz = self.report_hz();
        rdebug!("sender.start hz={hz}");
        let period = 1.0 / f64::from(hz);
        let period_duration = Duration::from_secs_f64(period);
        let timer_active = timer_period_begin(C::TIMER_PERIOD_MS);
        let mut next_at = Instant::now();
        while !self.stop_flag.is_set() {
            self.sender_frame();
            next_at += period_duration;
            let now = Instant::now();
            if next_at + period_duration < now {
                next_at = now;
            } else if next_at > now {
                self.stop_flag.wait_timeout(next_at - now);
            }
        }
        if timer_active {
            timer_period_end(C::TIMER_PERIOD_MS);
        }
        rdebug!("sender.stop");
    }

    /// supervisor 线程主体：每 2s 一次看护（对应 Python _supervisor_loop）。
    fn supervisor_loop(self: Arc<Self>) {
        while !self.stop_flag.is_set() {
            self.tick_supervisor();
            self.stop_flag
                .wait_timeout(Duration::from_secs_f64(C::HEALTHCHECK_INTERVAL_S));
        }
    }

    /// 停止所有线程并**保证**释放遮罩；幂等，不调用 session.stop()。
    ///
    /// 注意：Rust 没有带超时的 join。若 supervisor 正卡在 session.start()（ADB 最长 40s），
    /// 这里会等到它返回；晚到的结果由 stopped 检查丢弃。
    pub fn stop(&self) {
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            inner.stopped = true;
        }
        self.stop_flag.set();
        self.cancel_connection_best_effort();
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            self.abort_capture(&mut inner, &mut events, "stop");
        }
        self.dispatch(events);
        self.join_threads();
        self.release_mask("stop");
        rdebug!("router.stop");
    }

    /// join 两个后台线程（跳过当前线程，避免自锁）。
    fn join_threads(&self) {
        let (sender, sender_id, supervisor, supervisor_id) = {
            let mut threads = self.lock_threads();
            (
                threads.sender.take(),
                threads.sender_id.take(),
                threads.supervisor.take(),
                threads.supervisor_id.take(),
            )
        };
        let current = thread::current().id();
        if let Some(handle) = sender {
            if sender_id != Some(current) {
                let _joined = handle.join();
            }
        }
        if let Some(handle) = supervisor {
            if supervisor_id != Some(current) {
                let _joined = handle.join();
            }
        }
    }

    /// 请求会话取消正在进行的首连（可选扩展，不阻塞、绝不抛）。
    fn cancel_connection_best_effort(&self) {
        let active = self.lock_inner().connect_in_progress;
        if !active {
            return;
        }
        if self.session.cancel_start() {
            rdebug!("router.connect_cancel");
        }
    }

    /// 串行地停掉当前会话，避免与 supervisor 的连接尝试并发。
    fn stop_session_for_device(&self) {
        self.cancel_connection_best_effort();
        let _guard = self.lock_connect();
        self.session.stop();
    }

    /// 用户从设备页选择一台设备并连接（可重复调用）。
    pub fn connect_device(&self, serial: Option<&str>) {
        let requested = serial.map(str::to_string);
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            // 同一台设备的连接已经在进行（supervisor 自动首连 / 上一次点击）：
            // 直接复用它，**不能**走下面的 stop_session_for_device()——那会
            // cancel_start() 掉正在握手的会话，服务端随即被 teardown 杀掉，
            // 显式连接最终拿到一条死通道（真机日志里的 E105「启动已取消」）。
            let in_flight = lock_mutex(&self.requested_serial).clone();
            if inner.connect_in_progress && in_flight.as_deref() == requested.as_deref() {
                inner.device_suspended = false;
                inner.reconnect_attempt = 0;
                inner.next_reconnect_at = self.clock.now();
                rdebug!(
                    "router.device.connect serial={} (in-flight, reuse)",
                    serial.unwrap_or("")
                );
                return;
            }
            inner.device_suspended = false;
            {
                let mut current = lock_mutex(&self.requested_serial);
                if current.as_deref() != requested.as_deref() {
                    *current = requested;
                }
            }
            inner.reconnect_attempt = 0;
            inner.next_reconnect_at = self.clock.now();
            self.abort_capture(&mut inner, &mut events, "device.connect");
            inner.facts = None;
            inner.estimator = None;
        }
        self.dispatch(events);
        self.stop_session_for_device();
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            inner.device_suspended = false;
            inner.reconnect_attempt = 0;
            inner.next_reconnect_at = self.clock.now();
            self.change_state(&mut inner, &mut events, State::Fault, "device.connect");
        }
        eprintln!("[router] router.device.connect serial={}", serial.unwrap_or(""));
        self.dispatch(events);
    }

    /// 用户主动断开设备；停止会话并禁止 supervisor 自动重连。
    pub fn disconnect_device(&self) {
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            inner.device_suspended = true;
            inner.next_reconnect_at = f64::INFINITY;
            inner.reconnect_attempt = 0;
            self.abort_capture(&mut inner, &mut events, "device.disconnect");
            inner.facts = None;
            inner.estimator = None;
        }
        self.dispatch(events);
        self.stop_session_for_device();
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            inner.facts = None;
            inner.estimator = None;
            self.change_state(&mut inner, &mut events, State::Idle, "device.disconnect");
        }
        eprintln!("[router] router.device.disconnect");
        self.dispatch(events);
    }

    /// 急停（Ctrl+Alt+Q）：立即交还输入，但保持 Router 线程可用。
    pub fn emergency_stop(&self) {
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            if inner.state == State::Captured {
                self.begin_return(&mut inner, &mut events, "panic", false);
            }
            if inner.state != State::Idle {
                self.change_state(&mut inner, &mut events, State::Idle, "panic");
            }
            self.release_mask("panic");
            self.send_keyboard_zero();
            self.send_mouse_release();
            self.reset_keymap(&mut inner);
            inner.arm_accum = 0.0;
            inner.overflow = 0.0;
        }
        self.dispatch(events);
    }

    /// 更新平板摆位；CAPTURED 期间调用只影响下一次接管。
    pub fn set_placement(&self, placement: Placement) {
        {
            let mut inner = self.lock_inner();
            inner.placement = Some(placement.clone());
            inner.arm_segment = None;
            inner.arm_monitor = None;
        }
        eprintln!(
            "[router] router.placement edge={} span={}..{} offset={:.3}",
            placement.segment.edge.as_str(),
            placement.segment.span_start,
            placement.segment.span_end,
            placement.offset
        );
    }

    /// 手动接管/交还（设置页"立即连接"按钮与托盘菜单共用）。
    pub fn toggle_manual(&self) {
        let mut events: Vec<RouterEvent> = Vec::new();
        let outcome = {
            let mut inner = self.lock_inner();
            self.toggle_manual_locked(&mut inner, &mut events)
        };
        if let Err(err) = outcome {
            eprintln!(
                "[router] router.manual_failed code={} detail={}",
                err.code(),
                err.detail()
            );
            let mut inner = self.lock_inner();
            self.fault(
                &mut inner,
                &mut events,
                err.code(),
                &format!("手动接管失败: {}", err.detail()),
            );
        }
        self.dispatch(events);
    }

    /// toggle_manual 的锁内部分。
    fn toggle_manual_locked(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
    ) -> Result<(), RouterError> {
        if inner.stopped {
            return Ok(());
        }
        match inner.state {
            State::Returning => return Ok(()),
            State::Captured => {
                inner.manual = false;
                self.begin_return(inner, events, "manual", true);
                return Ok(());
            }
            State::Fault => {
                self.change_state(inner, events, State::Idle, "manual");
            }
            State::Idle | State::Arming => {}
        }
        let placement = match inner.placement.clone() {
            Some(placement) => placement,
            None => {
                eprintln!("[router] router.manual.no_placement");
                return Ok(());
            }
        };
        let monitor = match self.monitor_for_handle(placement.segment.monitor_handle) {
            Some(monitor) => monitor,
            None => {
                eprintln!("[router] router.manual.no_monitor");
                return Ok(());
            }
        };
        inner.manual = true;
        inner.arm_segment = Some(placement.segment);
        inner.arm_monitor = Some(monitor);
        self.enter_captured(inner, events, "manual")
    }

    /// 把平板指针重新归中（GUI 诊断按钮）；CAPTURED 时用 warp 连发。
    pub fn resync_pointer(&self) {
        let mut events: Vec<RouterEvent> = Vec::new();
        let outcome = {
            let mut inner = self.lock_inner();
            self.resync_pointer_locked(&mut inner)
        };
        if let Err(err) = outcome {
            eprintln!(
                "[router] router.resync_failed code={} detail={}",
                err.code(),
                err.detail()
            );
            let mut inner = self.lock_inner();
            self.fault(
                &mut inner,
                &mut events,
                err.code(),
                &format!("归中失败: {}", err.detail()),
            );
        }
        self.dispatch(events);
    }

    /// resync_pointer 的锁内部分。
    fn resync_pointer_locked(&self, inner: &mut Inner) -> Result<(), RouterError> {
        if inner.stopped {
            return Ok(());
        }
        self.ensure_estimator(inner)?;
        let mut estimator = match inner.estimator {
            Some(estimator) => estimator,
            None => return Ok(()),
        };
        if inner.state == State::Captured {
            let (target_x, target_y) = estimator.center();
            let reports = estimator.warp_to(target_x, target_y);
            // warp 只能发整数步，会留下 <1 的残差；内部坐标对齐精确中心。
            estimator.reset(None, None);
            inner.estimator = Some(estimator);
            self.enqueue_burst(&reports);
            rdebug!("router.resync reports={}", reports.len());
        } else {
            estimator.reset(None, None);
            inner.estimator = Some(estimator);
        }
        Ok(())
    }

    // -- RawInput ----------------------------------------------------------

    /// RawInput 线程入口；绝不向调用方抛异常。
    pub fn on_raw(&self, ev: &RawDelta) {
        let mut events: Vec<RouterEvent> = Vec::new();
        let outcome = {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            self.metrics.record_raw(1);
            self.on_raw_locked(&mut inner, &mut events, ev)
        };
        if let Err(err) = outcome {
            eprintln!(
                "[router] router.raw_failed code={} detail={}",
                err.code(),
                err.detail()
            );
            let mut inner = self.lock_inner();
            self.fault(
                &mut inner,
                &mut events,
                err.code(),
                &format!("处理鼠标输入失败: {}", err.detail()),
            );
        }
        self.dispatch(events);
    }

    /// on_raw 的锁内部分（对应 Python _on_raw_locked）。
    fn on_raw_locked(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        ev: &RawDelta,
    ) -> Result<(), RouterError> {
        let previous_buttons = inner.buttons;
        inner.buttons = ev.buttons;
        if inner.buttons != previous_buttons {
            rdebug!("cap.btn buttons=0x{:02x}", inner.buttons);
            if inner.state == State::Captured {
                let diagnosis =
                    std::panic::catch_unwind(AssertUnwindSafe(|| self.mask.diagnose()));
                match diagnosis {
                    Ok(text) => rdebug!("cap.cursor {text}"),
                    Err(_) => rdebug!("cap.cursor failed"),
                }
            }
        }
        self.track_pc_cursor(inner, ev);
        match inner.state {
            State::Fault => Ok(()),
            State::Returning => {
                if self.clock.now() >= inner.return_deadline {
                    self.change_state(inner, events, State::Idle, "debounce");
                    if let Some(position) = self.query_cursor_pos() {
                        inner.pc_cursor = Some(position);
                    }
                }
                Ok(())
            }
            State::Idle => self.maybe_arm(inner, events, ev),
            State::Arming => self.update_arm(inner, events, ev),
            State::Captured => self.handle_captured(inner, events, ev),
        }
    }

    /// 跟踪 PC 绝对光标（对应 Python _track_pc_cursor）。
    fn track_pc_cursor(&self, inner: &mut Inner, ev: &RawDelta) {
        match self.desktop.cursor_pos() {
            CursorQuery::Position(x, y) => {
                inner.pc_cursor = Some((x, y));
                return;
            }
            CursorQuery::Unavailable => {
                if inner.pc_cursor.is_none() {
                    inner.pc_cursor = Some(self.default_cursor());
                }
                return;
            }
            CursorQuery::Unsupported => {}
        }
        if inner.pc_cursor.is_none() {
            inner.pc_cursor = Some(self.default_cursor());
        }
        let (x, y) = inner.pc_cursor.unwrap_or((0, 0));
        inner.pc_cursor = Some(self.clamp_to_desktop(x + ev.dx, y + ev.dy));
    }

    /// 查询 PC 光标绝对坐标；无查询能力时返回 None。
    fn query_cursor_pos(&self) -> Option<(i32, i32)> {
        match self.desktop.cursor_pos() {
            CursorQuery::Position(x, y) => Some((x, y)),
            CursorQuery::Unavailable | CursorQuery::Unsupported => None,
        }
    }

    /// 虚拟桌面中心（Python _default_cursor）。
    fn default_cursor(&self) -> (i32, i32) {
        let rect = self.bounding_box();
        let (center_x, center_y) = rect.center();
        (trunc_i32(center_x), trunc_i32(center_y))
    }

    /// 把坐标夹到虚拟桌面内（Python _clamp_to_desktop）。
    fn clamp_to_desktop(&self, x: i32, y: i32) -> (i32, i32) {
        let rect = self.bounding_box();
        if rect.empty() {
            return (x, y);
        }
        (
            x.clamp(rect.left, rect.right - 1),
            y.clamp(rect.top, rect.bottom - 1),
        )
    }

    /// 桌面并集（Python _bounding_box）。
    fn bounding_box(&self) -> Rect {
        self.desktop.bounding_box()
    }

    // -- 边缘 / ARMING -----------------------------------------------------

    /// IDLE 边缘监测（对应 Python _maybe_arm_locked）。
    fn maybe_arm(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        ev: &RawDelta,
    ) -> Result<(), RouterError> {
        let placement = match inner.placement.clone() {
            Some(placement) => placement,
            None => {
                rdebug!("arm.skip placement=none");
                return Ok(());
            }
        };
        let (x, y) = inner.pc_cursor.unwrap_or_else(|| self.default_cursor());
        let segments = self.segments_at(x, y);
        if segments.is_empty() {
            return Ok(());
        }
        if let Err(err) = self.ensure_estimator(inner) {
            rdebug!("arm.skip estimator code={}", err.code());
            return Ok(());
        }
        let estimator = match inner.estimator {
            Some(estimator) => estimator,
            None => return Ok(()),
        };
        let anchor = match self.monitor_for_handle(placement.segment.monitor_handle) {
            Some(monitor) => monitor,
            None => {
                rdebug!("arm.skip anchor");
                return Ok(());
            }
        };
        let tablet = tablet_rect(
            &placement.segment,
            &anchor,
            placement.offset,
            estimator.width(),
            estimator.height(),
        );
        let mut contacts: Vec<EdgeSegment> = Vec::new();
        for segment in &segments {
            let monitor = match self.monitor_for_handle(segment.monitor_handle) {
                Some(monitor) => monitor,
                None => continue,
            };
            let along = if segment.edge.is_vertical() {
                f64::from(y)
            } else {
                f64::from(x)
            };
            if segment_contacts_tablet(segment, &monitor, &tablet, along) {
                contacts.push(*segment);
            }
        }
        if contacts.is_empty() {
            rdebug!("arm.skip contacts=0");
            return Ok(());
        }
        let chosen = match self.resolve_corner(inner, &contacts, (ev.dx, ev.dy)) {
            Some(chosen) => chosen,
            None => return Ok(()),
        };
        let (nx, ny) = chosen.edge.outward_normal();
        if ev.dx * nx + ev.dy * ny <= 0 {
            return Ok(());
        }
        let monitor = match self.monitor_for_handle(chosen.monitor_handle) {
            Some(monitor) => monitor,
            None => return Ok(()),
        };
        inner.arm_segment = Some(chosen);
        inner.arm_monitor = Some(monitor);
        inner.arm_deadline = self.clock.now() + self.cfg.motion.arm_window_ms.max(0) as f64 / 1000.0;
        inner.arm_accum = 0.0;
        inner.manual = false;
        if self.change_state(inner, events, State::Arming, "arm") {
            rdebug!("arm.start edge={}", chosen.edge.as_str());
        }
        Ok(())
    }

    /// ARMING 观察窗（对应 Python _update_arm_locked）。
    fn update_arm(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        ev: &RawDelta,
    ) -> Result<(), RouterError> {
        let placement = match inner.placement.clone() {
            Some(placement) => placement,
            None => {
                self.arm_fail(inner, events, "placement_lost");
                return Ok(());
            }
        };
        let active = match inner.arm_segment {
            Some(segment) => segment,
            None => {
                self.arm_fail(inner, events, "placement_lost");
                return Ok(());
            }
        };
        let now = self.clock.now();
        if now > inner.arm_deadline {
            let arm_px = self.cfg.motion.arm_px;
            let slow_px = (C::ARM_SLOW_PX as i64).min((arm_px / 2).max(1));
            if inner.arm_accum >= slow_px as f64 {
                inner.arm_accum = 0.0;
                return self.enter_captured(inner, events, "arm_slow");
            }
            self.arm_fail(inner, events, "timeout");
            return Ok(());
        }
        let (x, y) = inner.pc_cursor.unwrap_or_else(|| self.default_cursor());
        let segments = self.segments_at(x, y);
        let mut still_touching = false;
        let monitor = self.monitor_for_handle(active.monitor_handle);
        let anchor = self.monitor_for_handle(placement.segment.monitor_handle);
        if let (Some(monitor), Some(estimator), Some(anchor)) = (monitor, inner.estimator, anchor) {
            let tablet = tablet_rect(
                &placement.segment,
                &anchor,
                placement.offset,
                estimator.width(),
                estimator.height(),
            );
            let along = if active.edge.is_vertical() {
                f64::from(y)
            } else {
                f64::from(x)
            };
            still_touching = segments.iter().any(|segment| {
                !Self::segment_differs(segment, &active)
                    && segment_contacts_tablet(segment, &monitor, &tablet, along)
            });
        }
        if !still_touching {
            self.arm_fail(inner, events, "left_edge");
            return Ok(());
        }
        let (nx, ny) = active.edge.outward_normal();
        if ev.dx * nx + ev.dy * ny < 0 {
            self.arm_fail(inner, events, "reverse");
            return Ok(());
        }
        inner.arm_accum += f64::from(ev.dx.abs() + ev.dy.abs());
        if inner.arm_accum >= self.cfg.motion.arm_px.max(1) as f64 {
            inner.arm_accum = 0.0;
            return self.enter_captured(inner, events, "arm");
        }
        Ok(())
    }

    /// 两个外轮廓边段是否指同一条边（Python _segment_differs 取反）。
    fn segment_differs(left: &EdgeSegment, right: &EdgeSegment) -> bool {
        left.monitor_handle != right.monitor_handle
            || left.span_start != right.span_start
            || left.span_end != right.span_end
    }

    /// ARMING 失败：回 IDLE 并带上 arm:<原因>（对应 Python _arm_fail_locked）。
    fn arm_fail(&self, inner: &mut Inner, events: &mut Vec<RouterEvent>, reason: &str) {
        eprintln!(
            "[router] arm.fail reason={reason} accum={:.1}",
            inner.arm_accum
        );
        inner.arm_accum = 0.0;
        inner.arm_segment = None;
        inner.arm_monitor = None;
        self.change_state(inner, events, State::Idle, &format!("arm:{reason}"));
    }

    /// 命中当前光标位置的外轮廓边段（Python _segments_at）。
    fn segments_at(&self, x: i32, y: i32) -> Vec<EdgeSegment> {
        let band = self.cfg.motion.edge_band_px.max(0);
        self.desktop.segments_at(x, y, band as i32)
    }

    /// 角落裁决（对应 Python _resolve_corner_locked）。
    fn resolve_corner(
        &self,
        inner: &Inner,
        segments: &[EdgeSegment],
        delta: (i32, i32),
    ) -> Option<EdgeSegment> {
        if segments.is_empty() {
            return None;
        }
        if let Some(chosen) = self
            .desktop
            .resolve_corner(segments, delta, self.corner_priority(inner))
        {
            return Some(chosen);
        }
        if segments.len() == 1 {
            Some(segments[0])
        } else {
            None
        }
    }

    /// 角落优先边：摆位优先，其次配置（Python _corner_priority）。
    fn corner_priority(&self, inner: &Inner) -> Option<Edge> {
        if let Some(placement) = inner.placement.as_ref() {
            if let Some(edge) = placement.corner_priority {
                return Some(edge);
            }
        }
        self.config_edge_opt()
    }

    /// 配置里的吸附边（解析失败返回 None）。
    fn config_edge_opt(&self) -> Option<Edge> {
        self.cfg.placement.edge.parse::<Edge>().ok()
    }

    /// 配置里的吸附边（解析失败回退 RIGHT，对应 Python _config_edge）。
    fn config_edge_or_right(&self) -> Edge {
        self.config_edge_opt().unwrap_or(Edge::Right)
    }

    // -- 显示器查询 --------------------------------------------------------

    /// 当前显示器列表（Python _monitors）。
    fn monitors(&self) -> Vec<MonitorInfo> {
        let monitors = self.desktop.monitors();
        if monitors.is_empty() {
            return Vec::new();
        }
        monitors
    }

    /// 按句柄查显示器（Python _monitor_for_handle）。
    fn monitor_for_handle(&self, handle: i64) -> Option<MonitorInfo> {
        self.monitors()
            .into_iter()
            .find(|monitor| monitor.handle == handle)
    }

    /// 显示器签名（用于发现拓扑变化；Python _monitor_signature）。
    fn monitor_signature(&self) -> Vec<(i64, Rect)> {
        self.monitors()
            .into_iter()
            .map(|monitor| (monitor.handle, monitor.rect))
            .collect()
    }

    /// 主显示器；没有标记 primary 时取第一个（Python _primary_monitor）。
    fn primary_monitor(&self) -> Option<MonitorInfo> {
        let monitors = self.monitors();
        if let Some(primary) = monitors.iter().find(|monitor| monitor.primary) {
            return Some(primary.clone());
        }
        monitors.first().cloned()
    }

    /// 读取会话当前设备事实（Python _read_facts）。
    fn read_facts(&self) -> Option<DeviceFacts> {
        self.session.facts()
    }

    /// 惰性创建估计器（对应 Python _ensure_estimator_locked）。
    fn ensure_estimator(&self, inner: &mut Inner) -> Result<(), RouterError> {
        if inner.estimator.is_some() {
            return Ok(());
        }
        let facts = match inner.facts.clone().or_else(|| self.read_facts()) {
            Some(facts) => facts,
            None => {
                return Err(RouterError::device_offline(
                    "尚无设备信息；连接由 supervisor 线程异步完成",
                ))
            }
        };
        let (width, height) = (facts.width, facts.height);
        if width <= 0 || height <= 0 {
            return Err(RouterError::handshake_failed(format!(
                "非法设备尺寸 {width}x{height}"
            )));
        }
        let mut estimator = CursorEstimator::new(width, height, facts.xscale_or_one())?;
        estimator.reset(None, None);
        inner.facts = Some(facts);
        inner.estimator = Some(estimator);
        Ok(())
    }

    /// 发一行键盘全零报告（交还/急停）。
    fn send_keyboard_zero(&self) {
        if let Err(err) = self.session.send_key(0, &[]) {
            rdebug!("router.keyboard_zero_skipped code={}", err.code());
        }
    }

    /// 补一条"全松"鼠标报告（绕过发送队列，与 Python 一致）。
    fn send_mouse_release(&self) {
        if let Err(err) = self.session.send_mouse(0, 0, 0, 0) {
            rdebug!("router.mouse_release_skipped code={}", err.code());
        }
    }

    /// 释放遮罩（异常只记录，对应 Python _release_mask）。
    fn release_mask(&self, reason: &str) {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| self.mask.release()));
        if outcome.is_err() {
            eprintln!("[router] router.mask_release_failed reason={reason}");
        }
    }

    /// 清空键位表状态（对应 Python _reset_keymap_locked）。
    fn reset_keymap(&self, inner: &mut Inner) {
        let keymap = match inner.keymap.clone() {
            Some(keymap) => keymap,
            None => return,
        };
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| keymap.reset()));
        if outcome.is_err() {
            eprintln!("[router] keymap.reset 失败");
        }
    }

    /// 设置系统光标绝对坐标（session → desktop → Win32 回退）。
    fn set_pc_cursor(&self, position: (i32, i32)) {
        if self.session.set_cursor_pos(position.0, position.1) {
            return;
        }
        if self.desktop.set_cursor_pos(position.0, position.1) {
            return;
        }
        fallback_set_cursor_pos(position.0, position.1);
    }

    /// 状态迁移的唯一写入口（对应 Python _change_state）。
    fn change_state(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        destination: State,
        reason: &str,
    ) -> bool {
        let old = inner.state;
        if destination == old {
            return false;
        }
        if !can_transition(old, destination) {
            eprintln!(
                "[router] router.illegal_transition {}->{} reason={}",
                old.as_str(),
                destination.as_str(),
                reason
            );
            if destination != State::Fault {
                self.fault(
                    inner,
                    events,
                    ErrorCode::Unexpected,
                    &format!(
                        "非法状态迁移 {}->{} ({})",
                        old.as_str(),
                        destination.as_str(),
                        reason
                    ),
                );
            }
            return false;
        }
        inner.state = destination;
        if destination != State::Captured {
            inner.arm_accum = 0.0;
        }
        if destination == State::Idle {
            inner.arm_segment = None;
            inner.arm_monitor = None;
        }
        rdebug!(
            "router.state {}->{} reason={}",
            old.as_str(),
            destination.as_str(),
            reason
        );
        events.push(RouterEvent::State(old, destination, reason.to_string()));
        true
    }

    /// 进入 FAULT：释放遮罩、停键盘、保留输入回流（I3）。
    fn fault(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        code: ErrorCode,
        detail: &str,
    ) {
        let was_captured = inner.state == State::Captured;
        if inner.state != State::Fault {
            self.change_state(
                inner,
                events,
                State::Fault,
                &format!("fault:{}", code.as_str()),
            );
        }
        inner.arm_accum = 0.0;
        inner.overflow = 0.0;
        self.release_mask("fault");
        if was_captured {
            self.send_keyboard_zero();
            self.send_mouse_release();
            self.reset_keymap(inner);
        }
        inner.reconnect_attempt = 0;
        inner.write_failures = 0;
        inner.next_reconnect_at = self.clock.now() + C::RECONNECT_BACKOFF_S[0];
        eprintln!(
            "[router] state.fault code={} detail={detail}",
            code.as_str()
        );
    }

    /// stop()/急停/设备切换的公共清理（对应 Python _abort_capture_locked）。
    fn abort_capture(&self, inner: &mut Inner, events: &mut Vec<RouterEvent>, reason: &str) {
        match inner.state {
            State::Captured => {
                self.begin_return(inner, events, reason, false);
            }
            State::Idle => {}
            _ => {
                self.change_state(inner, events, State::Idle, reason);
            }
        }
        self.release_mask(reason);
        self.send_keyboard_zero();
        self.send_mouse_release();
        self.reset_keymap(inner);
        inner.arm_accum = 0.0;
        inner.overflow = 0.0;
    }

    /// 指标快照入队（锁外派发）。
    fn emit_metrics(&self, events: &mut Vec<RouterEvent>) {
        events.push(RouterEvent::Metrics(self.metrics.snapshot()));
    }

    // -- CAPTURED / RETURNING ----------------------------------------------

    /// 进入 CAPTURED（对应 Python _enter_captured_locked）。
    ///
    /// 顺序不可换：先 warp 到入口点、再 queue 全零松键、最后把 warp + pin 作为 burst 入队
    /// （PITFALLS 2.4：进入前先补发全松）。
    fn enter_captured(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        reason: &str,
    ) -> Result<(), RouterError> {
        if inner.state == State::Captured {
            return Ok(());
        }
        if inner.state == State::Idle {
            self.change_state(inner, events, State::Arming, reason);
        }
        if inner.state != State::Arming {
            return Err(RouterError::unexpected(format!(
                "无法从 {} 进入 CAPTURED",
                inner.state.as_str()
            )));
        }
        let placement = match inner.placement.clone() {
            Some(placement) => placement,
            None => return Err(RouterError::placement_invalid("未设置平板摆位")),
        };
        let segment = inner.arm_segment.unwrap_or(placement.segment);
        let anchor = match self.monitor_for_handle(placement.segment.monitor_handle) {
            Some(monitor) => monitor,
            None => {
                return Err(RouterError::placement_invalid(
                    "摆位引用的显示器已不存在",
                ))
            }
        };
        let monitor = match inner
            .arm_monitor
            .clone()
            .or_else(|| self.monitor_for_handle(segment.monitor_handle))
        {
            Some(monitor) => monitor,
            None => {
                return Err(RouterError::placement_invalid(
                    "摆位引用的显示器已不存在",
                ))
            }
        };
        self.ensure_estimator(inner)?;
        let mut estimator = match inner.estimator {
            Some(estimator) => estimator,
            None => return Err(RouterError::unexpected("进入 CAPTURED 时缺少 estimator")),
        };
        let tablet = tablet_rect(
            &placement.segment,
            &anchor,
            placement.offset,
            estimator.width(),
            estimator.height(),
        );
        let (mut cursor_x, mut cursor_y) =
            inner.pc_cursor.unwrap_or_else(|| self.default_cursor());
        if reason == "manual" {
            if segment.edge.is_vertical() {
                cursor_x = round_ties_even_i32((tablet.left + tablet.right) / 2.0);
            } else {
                cursor_y = round_ties_even_i32((tablet.top + tablet.bottom) / 2.0);
            }
        }
        let (entry_x, entry_y) = tablet_entry_point(
            &segment,
            &tablet,
            f64::from(cursor_x),
            f64::from(cursor_y),
            estimator.width(),
            estimator.height(),
        );
        let mut reports = Self::warp_to_entry(&mut estimator, entry_x, entry_y);
        let clip = self.clip_rect(&*inner, &segment, &monitor, &tablet);
        inner.estimator = Some(estimator);
        let shown = std::panic::catch_unwind(AssertUnwindSafe(|| self.mask.show(clip)));
        let show_error = match shown {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(format!("{err}")),
            Err(_) => Some("mask.show panic".to_string()),
        };
        if let Some(detail) = show_error {
            self.release_mask("capture_show_failed");
            return Err(RouterError::unexpected(format!("mask.show: {detail}")));
        }
        if !self.change_state(inner, events, State::Captured, reason) {
            self.release_mask("capture_transition_failed");
            return Ok(());
        }
        let entry_edge = segment.edge.opposite();
        inner.entry_edge = Some(entry_edge);
        inner.overflow = 0.0;
        inner.neutral_until_release = inner.buttons != 0;
        inner.wheel_accum = 0;
        inner.hit_logged = false;
        inner.suppress_logged = false;
        self.queue_mouse(0, 0, 0, 0);
        let pin = Self::pin_to_edge_reports(&estimator, entry_edge);
        let pin_count = pin.len();
        let report_count = reports.len();
        reports.extend(pin);
        self.enqueue_burst(&reports);
        eprintln!(
            "[router] cap.start edge={} manual={} reports={} pin={} point=({:.0},{:.0}) buttons=0x{:02x}",
            segment.edge.as_str(),
            reason == "manual",
            report_count,
            pin_count,
            entry_x,
            entry_y,
            inner.buttons
        );
        rdebug!("cap.mask {}", self.mask.diagnose());
        Ok(())
    }

    /// warp 到入口点并补一步"贴边"残差（对应 Python _warp_to_entry）。
    fn warp_to_entry(
        estimator: &mut CursorEstimator,
        target_x: f64,
        target_y: f64,
    ) -> Vec<(i32, i32)> {
        let mut reports = estimator.warp_to(target_x, target_y);
        let dx = Self::edge_settle_delta(estimator.x, target_x, estimator.raw_width());
        let dy = Self::edge_settle_delta(estimator.y, target_y, estimator.raw_height());
        if dx != 0 || dy != 0 {
            reports.push(estimator.feed(f64::from(dx), f64::from(dy), 1.0, C::HID_CLAMP));
        }
        reports
    }

    /// 目标点贴在平板边界上时，返回还要补的 1 个计数（对应 Python _edge_settle_delta）。
    fn edge_settle_delta(current: f64, target: f64, size: f64) -> i32 {
        if size <= 1.0 {
            return 0;
        }
        let last = size - 1.0;
        let on_edge = target.abs() < 1e-9 || (target - last).abs() < 1e-9;
        if !on_edge {
            return 0;
        }
        let gap = target - current;
        if gap.abs() < 1e-9 {
            return 0;
        }
        if gap > 0.0 {
            1
        } else {
            -1
        }
    }

    /// 入口边"顶满"报告（对应 Python _pin_to_edge_reports）。
    fn pin_to_edge_reports(estimator: &CursorEstimator, edge: Edge) -> Vec<(i32, i32)> {
        let (nx, ny) = edge.outward_normal();
        let axis = if edge.is_vertical() {
            estimator.raw_width()
        } else {
            estimator.raw_height()
        };
        let scale = if estimator.xscale() == 0.0 {
            1.0
        } else {
            estimator.xscale()
        };
        let mut remaining = trunc_i32(axis / scale) + 1;
        let mut out: Vec<(i32, i32)> = Vec::new();
        while remaining > 0 {
            let step = C::HID_CLAMP.min(remaining);
            remaining -= step;
            out.push((step * nx, step * ny));
        }
        out
    }

    /// 计算 1 像素宽的剪贴条（对应 Python _clip_rect）。
    fn clip_rect(
        &self,
        inner: &Inner,
        segment: &EdgeSegment,
        monitor: &MonitorInfo,
        tablet: &TabletRect,
    ) -> Option<Rect> {
        let (cursor_x, cursor_y) = inner.pc_cursor.unwrap_or((0, 0));
        let vertical = segment.edge.is_vertical();
        let along = if vertical { cursor_y } else { cursor_x };
        let expected_line = Self::edge_line(segment.edge, &monitor.rect);
        let mut target = monitor.clone();
        for candidate in self.monitors() {
            let rect = candidate.rect;
            if Self::edge_line(segment.edge, &rect) != expected_line {
                continue;
            }
            let hit = if vertical {
                rect.top <= along && along < rect.bottom
            } else {
                rect.left <= along && along < rect.right
            };
            if hit {
                target = candidate;
                break;
            }
        }
        let rect = target.rect;
        if vertical {
            let left = if segment.edge == Edge::Right {
                rect.right - 1
            } else {
                rect.left
            };
            if left < rect.left || left >= rect.right {
                return None;
            }
            let top = rect
                .top
                .max(segment.span_start)
                .max(trunc_i32(tablet.top));
            let bottom = rect
                .bottom
                .min(segment.span_end)
                .min(trunc_i32(tablet.bottom));
            if bottom <= top {
                return None;
            }
            return Some(Rect::new(left, top, left + 1, bottom));
        }
        let top = if segment.edge == Edge::Bottom {
            rect.bottom - 1
        } else {
            rect.top
        };
        if top < rect.top || top >= rect.bottom {
            return None;
        }
        let left = rect
            .left
            .max(segment.span_start)
            .max(trunc_i32(tablet.left));
        let right = rect
            .right
            .min(segment.span_end)
            .min(trunc_i32(tablet.right));
        if right <= left {
            return None;
        }
        Some(Rect::new(left, top, right, top + 1))
    }

    /// 某条边在给定矩形上的坐标（对应 Python 里按 edge 取 rect 字段）。
    fn edge_line(edge: Edge, rect: &Rect) -> i32 {
        match edge {
            Edge::Left => rect.left,
            Edge::Right => rect.right,
            Edge::Top => rect.top,
            Edge::Bottom => rect.bottom,
        }
    }

    /// CAPTURED 状态的鼠标事件（对应 Python _handle_captured_locked）。
    fn handle_captured(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        ev: &RawDelta,
    ) -> Result<(), RouterError> {
        let mut estimator = match inner.estimator {
            Some(estimator) => estimator,
            None => {
                return Err(RouterError::unexpected("CAPTURED 状态缺少 estimator"));
            }
        };
        if inner.buttons == 0 {
            inner.neutral_until_release = false;
        }
        let buttons = inner.effective_buttons();
        let gain = self.cfg.motion.gain;
        let clamp = self
            .cfg
            .motion
            .hid_clamp
            .clamp(1, i64::from(C::HID_CLAMP)) as i32;
        let (dx, dy) = estimator.feed(f64::from(ev.dx), f64::from(ev.dy), gain, clamp);
        let wheel = inner.take_wheel(ev.wheel);
        inner.estimator = Some(estimator);
        self.queue_mouse(buttons, dx, dy, wheel);
        let entry = match inner.entry_edge {
            Some(edge) => edge,
            None => {
                inner.overflow = 0.0;
                return Ok(());
            }
        };
        if inner.buttons != 0 || !Self::at_edge(&estimator, entry) {
            inner.overflow = 0.0;
            return Ok(());
        }
        let (nx, ny) = entry.outward_normal();
        let push = ev.dx * nx + ev.dy * ny;
        if push <= 0 {
            inner.overflow = 0.0;
            return Ok(());
        }
        if !inner.hit_logged {
            inner.hit_logged = true;
            rdebug!("cap.hit edge={}", entry.as_str());
        }
        inner.overflow += f64::from(push);
        let threshold = self.cfg.motion.exit_overflow_px.max(1) as f64;
        if inner.overflow >= threshold {
            rdebug!("cap.exit overflow={:.0}", inner.overflow);
            self.begin_return(inner, events, "exit", true);
        }
        Ok(())
    }

    /// 估计器是否贴在某条边上（对应 Python _at_edge）。
    fn at_edge(estimator: &CursorEstimator, edge: Edge) -> bool {
        match edge {
            Edge::Left => estimator.x <= 0.0,
            Edge::Right => estimator.x >= estimator.raw_width() - 1.0,
            Edge::Top => estimator.y <= 0.0,
            Edge::Bottom => estimator.y >= estimator.raw_height() - 1.0,
        }
    }

    /// 交还控制（对应 Python _begin_return_locked）。
    fn begin_return(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        reason: &str,
        debounce: bool,
    ) {
        if inner.state != State::Captured {
            return;
        }
        let mut exit_position: Option<(i32, i32)> = None;
        if let (Some(estimator), Some(placement)) = (inner.estimator, inner.placement.clone()) {
            let segment = inner.arm_segment.unwrap_or(placement.segment);
            let anchor = self.monitor_for_handle(placement.segment.monitor_handle);
            let monitor = inner
                .arm_monitor
                .clone()
                .or_else(|| self.monitor_for_handle(segment.monitor_handle));
            if let (Some(anchor), Some(monitor)) = (anchor, monitor) {
                let tablet = tablet_rect(
                    &placement.segment,
                    &anchor,
                    placement.offset,
                    estimator.width(),
                    estimator.height(),
                );
                let along = if segment.edge.is_vertical() {
                    estimator.y
                } else {
                    estimator.x
                };
                exit_position = Some(tablet_exit_point(
                    &segment,
                    &monitor,
                    &tablet,
                    along,
                    C::ENTRY_MARGIN_PX,
                ));
            }
        }
        if let Some(position) = exit_position {
            self.set_pc_cursor(position);
            inner.pc_cursor = Some(position);
        }
        self.release_mask(reason);
        self.send_keyboard_zero();
        self.reset_keymap(inner);
        inner.overflow = 0.0;
        inner.neutral_until_release = false;
        inner.wheel_accum = 0;
        self.send_mouse_release();
        if !self.change_state(inner, events, State::Returning, reason) {
            return;
        }
        let debounce_ms = if debounce {
            self.cfg.motion.debounce_ms.max(0)
        } else {
            0
        };
        inner.return_deadline = self.clock.now() + debounce_ms as f64 / 1000.0;
        rdebug!(
            "cap.end reason={} debounce={} buttons=0x{:02x}",
            reason,
            debounce_ms,
            inner.buttons
        );
    }

    // -- 发送队列 ----------------------------------------------------------

    /// 入队一条普通鼠标报告（对应 Python _queue_mouse_locked）。
    fn queue_mouse(&self, buttons: i32, dx: i32, dy: i32, wheel: i32) {
        let message = SendMsg::report(buttons, dx, dy, wheel, self.clock.now());
        self.append_send(message);
    }

    /// 入队一批 burst 报告（对应 Python _enqueue_burst_locked）。
    fn enqueue_burst(&self, reports: &[(i32, i32)]) {
        let stamp = self.clock.now();
        let mut queue = self.lock_queue();
        for (dx, dy) in reports {
            let message = SendMsg::burst(*dx, *dy, stamp);
            self.append_send_locked(&mut queue, message);
        }
    }

    /// 入队并加队列锁。
    fn append_send(&self, message: SendMsg) {
        let mut queue = self.lock_queue();
        self.append_send_locked(&mut queue, message);
    }

    /// 超过软阈值时丢弃心跳；真实报告始终保留且保持原始顺序（对应 Python _append_send_locked）。
    fn append_send_locked(&self, queue: &mut VecDeque<SendMsg>, message: SendMsg) {
        if queue.len() >= C::SEND_QUEUE_MAXLEN {
            if message.is_heartbeat() {
                self.metrics.record_drop(1);
                return;
            }
            if let Some(index) = queue.iter().position(SendMsg::is_heartbeat) {
                queue.remove(index);
            }
        }
        queue.push_back(message);
    }

    /// 取一批待发报文（对应 Python _drain）。
    fn drain(&self) -> Vec<SendMsg> {
        let captured = self.lock_inner().state == State::Captured;
        let mut out: Vec<SendMsg> = Vec::new();
        let mut queue = self.lock_queue();
        while !queue.is_empty() && out.len() < C::MAX_BATCH {
            let head_is_burst = queue.front().map(|message| message.burst).unwrap_or(false);
            if head_is_burst {
                while queue.front().map(|message| message.burst).unwrap_or(false) {
                    if let Some(message) = queue.pop_front() {
                        if captured {
                            out.push(message);
                        }
                    }
                }
                break;
            }
            if let Some(message) = queue.pop_front() {
                if (message.dx != 0 || message.dy != 0) && !captured {
                    continue;
                }
                out.push(message);
            }
        }
        out
    }

    /// 出一帧（对应 Python _sender_frame）；sender 线程与测试共用。
    pub fn sender_frame(&self) {
        let inactive = {
            let inner = self.lock_inner();
            inner.stopped || inner.device_suspended
        };
        if inactive {
            self.lock_queue().clear();
            self.metrics.set_backlog(0);
            return;
        }
        let messages = self.drain();
        if messages.is_empty() {
            self.send_heartbeat();
        } else {
            for message in &messages {
                self.send_mouse_message(message);
            }
        }
        let depth = self.lock_queue().len();
        self.metrics.set_backlog(depth as i64);
    }

    /// 发心跳（必须带当前按键位图，PITFALLS 2.2）。
    fn send_heartbeat(&self) {
        let buttons = {
            let inner = self.lock_inner();
            if inner.state == State::Captured {
                inner.effective_buttons()
            } else {
                0
            }
        };
        let message = SendMsg::heartbeat(buttons);
        self.send_mouse_message(&message);
    }

    /// 写一条报文并记账（对应 Python _send_mouse_message）。
    fn send_mouse_message(&self, message: &SendMsg) {
        match self
            .session
            .send_mouse(message.buttons, message.dx, message.dy, message.wheel)
        {
            Ok(()) => self.note_write_success(Some(message)),
            Err(err) => self.note_write_failure(&err),
        }
    }

    /// 写成功：清失败计数、记速率与（非心跳的）rtt 样本。
    fn note_write_success(&self, message: Option<&SendMsg>) {
        {
            let mut inner = self.lock_inner();
            inner.write_failures = 0;
        }
        self.metrics.record_send(1);
        if let Some(message) = message {
            if message.stamp != 0.0 && !message.heartbeat {
                let elapsed_ms = (self.clock.now() - message.stamp) * 1000.0;
                self.metrics.record_latency(elapsed_ms.max(0.0));
            }
        }
    }

    /// 写失败：连续 3 次才进 FAULT（对应 Python _note_write_failure）。
    fn note_write_failure(&self, err: &RouterError) {
        let mut events: Vec<RouterEvent> = Vec::new();
        let failures;
        {
            let mut inner = self.lock_inner();
            if inner.state == State::Fault {
                return;
            }
            // 还没有设备信息 = 会话没连上：此刻的写失败只说明"还没起来"，
            // 不该累计成链路故障（真机上启动 1 秒就被打成 E106）。
            if inner.facts.is_none() {
                return;
            }
            inner.write_failures += 1;
            failures = inner.write_failures;
            if failures < C::CONSECUTIVE_WRITE_FAILURES {
                rdebug!("sender.write_failed failures={failures} err={err}");
                return;
            }
            self.fault(
                &mut inner,
                &mut events,
                err.code(),
                &format!("连续 {failures} 次写失败: {err}"),
            );
        }
        eprintln!("[router] sender.write_failed failures={failures} err={err}");
        self.dispatch(events);
    }

    /// 出帧频率（clamp 到 [125, 1000]，对应 Python _report_hz）。
    fn report_hz(&self) -> i32 {
        self.cfg
            .motion
            .report_hz
            .clamp(i64::from(C::MIN_REPORT_HZ), i64::from(C::MAX_REPORT_HZ)) as i32
    }

    // -- supervisor --------------------------------------------------------

    /// 一次看护周期（对应 Python _supervisor_tick，注入当前时刻便于测试）。
    pub fn tick_supervisor(&self) {
        self.tick_supervisor_at(None);
    }

    /// 带显式时刻的看护周期。
    pub fn tick_supervisor_at(&self, now: Option<f64>) {
        let current = now.unwrap_or_else(|| self.clock.now());
        let mut events: Vec<RouterEvent> = Vec::new();
        let connect_due = {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            let due = inner.state == State::Fault
                && current >= inner.next_reconnect_at
                && !inner.connect_in_progress
                && !inner.device_suspended;
            if due {
                inner.connect_in_progress = true;
            }
            due
        };
        if connect_due {
            self.connect_once();
            return;
        }
        let mut rotation_facts: Option<DeviceFacts> = None;
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            if inner.device_suspended {
                if inner.state == State::Fault {
                    self.change_state(&mut inner, &mut events, State::Idle, "device.suspended");
                }
                if inner.state != State::Fault {
                    self.emit_metrics(&mut events);
                }
            } else {
                if inner.state != State::Fault {
                    self.check_health(&mut inner, &mut events);
                    rotation_facts = self.check_rotation(&mut inner, &mut events);
                    self.check_desktop(&mut inner, &mut events);
                    self.poll_pointer_speed(&mut inner, current);
                    if inner.state == State::Returning && current >= inner.return_deadline {
                        self.change_state(&mut inner, &mut events, State::Idle, "debounce");
                    }
                }
                self.emit_metrics(&mut events);
            }
        }
        if let Some(facts) = rotation_facts {
            events.push(RouterEvent::Orientation(facts));
        }
        self.dispatch(events);
    }

    /// 执行一次（可能阻塞的）连接/重连（对应 Python _connect_once）。
    fn connect_once(&self) {
        let mut facts: Option<DeviceFacts> = None;
        let mut error: Option<RouterError> = None;
        let mut healthy = false;
        {
            let _guard = self.lock_connect();
            if self.stop_flag.is_set() {
                self.lock_inner().connect_in_progress = false;
                return;
            }
            match self.connect_attempt(&mut facts) {
                Ok(value) => healthy = value,
                Err(err) => error = Some(err),
            }
        }
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            inner.connect_in_progress = false;
            if inner.stopped {
                return;
            }
            if inner.device_suspended {
                // 用户在连接过程中点了"断开"：丢弃晚到的 facts，保持 IDLE。
                inner.facts = None;
                inner.estimator = None;
                if inner.state != State::Fault {
                    self.change_state(&mut inner, &mut events, State::Idle, "device.suspended");
                }
                self.emit_metrics(&mut events);
            } else if healthy && facts.is_some() {
                inner.facts = facts;
                self.recover(&mut inner, &mut events);
                self.emit_metrics(&mut events);
            } else {
                inner.reconnect_attempt += 1;
                let attempt = inner.reconnect_attempt;
                let backoff = C::RECONNECT_BACKOFF_S;
                let index = (attempt as usize).min(backoff.len()) - 1;
                inner.next_reconnect_at = self.clock.now() + backoff[index];
                let (code, detail) = match &error {
                    Some(err) => {
                        let detail = if err.detail().is_empty() {
                            err.code().as_str().to_string()
                        } else {
                            err.detail().to_string()
                        };
                        (err.code().as_str().to_string(), detail)
                    }
                    None => (
                        ErrorCode::DeviceOffline.as_str().to_string(),
                        "连接后健康检查失败".to_string(),
                    ),
                };
                eprintln!(
                    "[router] router.connect_failed attempt={attempt} code={code} detail={detail}"
                );
                eprintln!("[router] tablet.reconnect attempt={attempt} result=fail");
                self.emit_metrics(&mut events);
            }
        }
        self.dispatch(events);
    }

    /// 连接尝试本体（持连接串行锁、不持状态锁）。
    fn connect_attempt(&self, facts: &mut Option<DeviceFacts>) -> Result<bool, RouterError> {
        *facts = self.read_facts();
        if facts.is_none() {
            *facts = Some(self.session.start()?);
        } else {
            match self.session.reconnect() {
                Some(result) => {
                    if let Some(candidate) = result? {
                        *facts = Some(candidate);
                    }
                }
                None => {
                    *facts = Some(self.session.start()?);
                }
            }
        }
        if facts.is_none() {
            return Err(RouterError::device_offline("连接未返回 DeviceFacts"));
        }
        Ok(self.session_healthy())
    }

    /// 健康检查（连续 3 次失败才 FAULT，对应 Python _check_health_locked）。
    fn check_health(&self, inner: &mut Inner, events: &mut Vec<RouterEvent>) {
        if self.session_healthy() {
            inner.health_failures = 0;
            return;
        }
        inner.health_failures += 1;
        if inner.health_failures >= C::CONSECUTIVE_WRITE_FAILURES {
            let failures = inner.health_failures;
            self.fault(
                inner,
                events,
                ErrorCode::SocketBroken,
                &format!("健康检查连续失败 {failures} 次"),
            );
        }
    }

    /// 会话健康判定（对应 Python _session_healthy）。
    fn session_healthy(&self) -> bool {
        match self.session.health() {
            Ok(None) => true,
            Ok(Some(report)) => report.is_healthy(),
            Err(err) => {
                rdebug!("session.health 失败 err={err}");
                false
            }
        }
    }

    /// 轮询会话缓存的设备事实（对应 Python _check_rotation_locked）。
    fn check_rotation(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
    ) -> Option<DeviceFacts> {
        let facts = self.read_facts()?;
        if self.apply_facts(inner, events, facts.clone()) {
            Some(facts)
        } else {
            None
        }
    }

    /// 用新的设备事实重建估计器；返回尺寸/朝向是否真的变化（对应 Python _apply_facts_locked）。
    fn apply_facts(
        &self,
        inner: &mut Inner,
        events: &mut Vec<RouterEvent>,
        facts: DeviceFacts,
    ) -> bool {
        let old = match inner.facts.clone() {
            Some(old) => old,
            None => {
                inner.facts = Some(facts);
                return false;
            }
        };
        let changed = facts.width != old.width
            || facts.height != old.height
            || facts.orientation != old.orientation;
        if !changed {
            return false;
        }
        let mut estimator =
            match CursorEstimator::new(facts.width, facts.height, facts.xscale_or_one()) {
                Ok(estimator) => estimator,
                Err(err) => {
                    self.fault(
                        inner,
                        events,
                        err.code(),
                        &format!("旋转重建失败: {}", err.detail()),
                    );
                    return false;
                }
            };
        estimator.reset(None, None);
        inner.estimator = Some(estimator);
        inner.facts = Some(facts.clone());
        inner.overflow = 0.0;
        eprintln!(
            "[router] router.rotation width={} height={} orientation={}",
            facts.width, facts.height, facts.orientation
        );
        true
    }

    /// 会话朝向监听入口（对应 Python _on_session_facts）。
    pub fn on_session_facts(&self, facts: DeviceFacts) {
        let mut events: Vec<RouterEvent> = Vec::new();
        {
            let mut inner = self.lock_inner();
            if inner.stopped {
                return;
            }
            if self.apply_facts(&mut inner, &mut events, facts.clone()) {
                events.push(RouterEvent::Orientation(facts));
            }
        }
        self.dispatch(events);
    }

    /// 显示器拓扑变化检测（对应 Python _check_desktop_locked）。
    fn check_desktop(&self, inner: &mut Inner, events: &mut Vec<RouterEvent>) {
        let signature = self.monitor_signature();
        match inner.desktop_sig.as_ref() {
            None => {
                inner.desktop_sig = Some(signature);
                return;
            }
            Some(previous) if *previous == signature => return,
            Some(_) => {}
        }
        inner.desktop_sig = Some(signature.clone());
        if let Err(err) = self.desktop.refresh() {
            eprintln!("[router] router.desktop_refresh_failed err={err}");
            return;
        }
        eprintln!(
            "[router] router.desktop_changed monitors={}",
            signature.len()
        );
        if inner.state == State::Captured {
            self.begin_return(inner, events, "display", true);
        }
        self.validate_placement(inner);
    }

    /// 校验摆位是否仍然有效（对应 Python _validate_placement_locked）。
    fn validate_placement(&self, inner: &mut Inner) {
        let placement = match inner.placement.clone() {
            Some(placement) => placement,
            None => return,
        };
        let valid = match self.desktop.segment_exists(&placement.segment) {
            Some(value) => value,
            None => self.segment_in_current_geometry(&placement.segment),
        };
        if !valid {
            self.reanchor_placement(inner);
        }
    }

    /// 边段是否仍在当前拓扑里（对应 Python _segment_in_current_geometry）。
    fn segment_in_current_geometry(&self, segment: &EdgeSegment) -> bool {
        self.desktop
            .exterior_segments()
            .iter()
            .any(|item| item == segment)
    }

    /// 摆位失效后重新锚定到主屏同侧（对应 Python _reanchor_placement_locked）。
    fn reanchor_placement(&self, inner: &mut Inner) {
        let segments = self.desktop.exterior_segments();
        if segments.is_empty() {
            return;
        }
        let old = inner.placement.clone();
        let target_edge = match &old {
            Some(placement) => placement.segment.edge,
            None => self.config_edge_or_right(),
        };
        let mut candidates: Vec<EdgeSegment> = segments
            .iter()
            .filter(|segment| segment.edge == target_edge)
            .copied()
            .collect();
        if let Some(primary) = self.primary_monitor() {
            let same_monitor: Vec<EdgeSegment> = candidates
                .iter()
                .filter(|segment| segment.monitor_handle == primary.handle)
                .copied()
                .collect();
            if !same_monitor.is_empty() {
                candidates = same_monitor;
            }
        }
        if candidates.is_empty() {
            candidates = segments;
        }
        let mut best = match candidates.first() {
            Some(first) => *first,
            None => return,
        };
        for candidate in &candidates {
            if candidate.length() > best.length() {
                best = *candidate;
            }
        }
        let corner_priority = match &old {
            Some(placement) => placement.corner_priority,
            None => self.config_edge_opt(),
        };
        match Placement::new(best, 0.5, Align::Proportional, corner_priority) {
            Ok(placement) => {
                inner.placement = Some(placement);
                eprintln!(
                    "[router] router.placement_reanchored edge={} span={}..{}",
                    best.edge.as_str(),
                    best.span_start,
                    best.span_end
                );
            }
            Err(err) => eprintln!("[router] router.reanchor_failed err={err}"),
        }
    }

    /// 每 10s 复查一次 pointer_speed（对应 Python _poll_pointer_speed_locked）。
    fn poll_pointer_speed(&self, inner: &mut Inner, now: f64) {
        if let Some(last) = inner.last_pointer_poll {
            if now - last < C::POINTER_SPEED_POLL_S {
                return;
            }
        }
        inner.last_pointer_poll = Some(now);
        self.restore_pointer_speed(inner);
    }

    /// 恢复 pointer_speed（对应 Python _restore_pointer_speed）。
    fn restore_pointer_speed(&self, inner: &Inner) {
        if let Some(result) = self.session.ensure_pointer_speed() {
            match result {
                Ok(()) => return,
                Err(err) => eprintln!("[router] session.ensure_pointer_speed 失败 err={err}"),
            }
        }
        if let Some(facts) = inner.facts.as_ref() {
            if facts.pointer_speed == 0 {
                return;
            }
        }
        let speed = self.cfg.misc.restore_pointer_speed as i32;
        if let Some(result) = self.session.set_pointer_speed(speed) {
            if let Err(err) = result {
                eprintln!("[router] session.set_pointer_speed 失败 err={err}");
            }
        }
    }

    /// 重连成功后的恢复（对应 Python _recover_locked）。
    fn recover(&self, inner: &mut Inner, events: &mut Vec<RouterEvent>) {
        if !self.change_state(inner, events, State::Idle, "reconnect") {
            return;
        }
        inner.health_failures = 0;
        inner.write_failures = 0;
        inner.reconnect_attempt = 0;
        inner.next_reconnect_at = 0.0;
        self.metrics.set_backlog(0);
        let mut failure: Option<RouterError> = None;
        if let Some(facts) = self.read_facts() {
            inner.facts = Some(facts);
        }
        match self.ensure_estimator(inner) {
            Err(err) => failure = Some(err),
            Ok(()) => {
                if let Some(mut estimator) = inner.estimator {
                    estimator.reset(None, None);
                    inner.estimator = Some(estimator);
                }
                if let Some(candidate) = self.read_facts() {
                    let current = inner.estimator;
                    let (width, height) = match current {
                        Some(estimator) => (estimator.width(), estimator.height()),
                        None => (0, 0),
                    };
                    if candidate.width != width || candidate.height != height {
                        match CursorEstimator::new(
                            candidate.width,
                            candidate.height,
                            candidate.xscale_or_one(),
                        ) {
                            Ok(mut estimator) => {
                                estimator.reset(None, None);
                                inner.estimator = Some(estimator);
                            }
                            Err(err) => failure = Some(err),
                        }
                    }
                    inner.facts = Some(candidate);
                }
            }
        }
        if let Some(err) = failure {
            self.fault(
                inner,
                events,
                err.code(),
                &format!("重连恢复失败: {}", err.detail()),
            );
            return;
        }
        eprintln!("[router] tablet.reconnect result=ok");
    }

    // -- 剪贴板 ------------------------------------------------------------

    /// 接收线程入口：按开关把平板文本转交给 GUI，绝不直接碰 GUI（对应 Python _on_session_clipboard）。
    pub fn on_session_clipboard(&self, text: &str) {
        if !self.cfg.misc.clipboard_sync {
            return;
        }
        if self.lock_inner().stopped {
            return;
        }
        let mut events: Vec<RouterEvent> = Vec::new();
        events.push(RouterEvent::Clipboard(text.to_string()));
        self.dispatch(events);
        rdebug!(
            "router.clipboard_received chars={}",
            text.chars().count()
        );
    }

    /// 把 PC 剪贴板文本写进平板剪贴板（UTF-8，最多 4KB；对应 Python inject_clipboard）。
    pub fn inject_clipboard(&self, text: &str) -> bool {
        if !self.cfg.misc.clipboard_sync || text.is_empty() {
            return false;
        }
        let payload = truncate_utf8(text, 4096);
        if payload.is_empty() {
            return false;
        }
        match self.session.set_clipboard(&payload) {
            Ok(()) => {
                rdebug!("router.clipboard_sent chars={}", payload.chars().count());
                true
            }
            Err(err) => {
                eprintln!(
                    "[router] router.clipboard_failed code={} detail={}",
                    err.code(),
                    err.detail()
                );
                false
            }
        }
    }

    // -- 键盘 --------------------------------------------------------------

    /// 键盘入口；返回 true 表示本机应吞掉该键（对应 Python on_key）。
    pub fn on_key(&self, event: &KeyEvent) -> bool {
        if event.down {
            // 键盘操作可能伴随窗口切换；趁 IDLE/RETURNING 时刷新一次绝对光标。
            let refresh = self.lock_inner().state != State::Captured;
            if refresh {
                if let Some(position) = self.query_cursor_pos() {
                    self.lock_inner().pc_cursor = Some(position);
                }
            }
        }
        let prepared = {
            let mut inner = self.lock_inner();
            self.prepare_key(&mut inner, event)
        };
        let (suppress, report) = prepared;
        let (modifier, usages) = match report {
            Some(report) => report,
            None => return suppress,
        };
        // 发送在 Router 锁外：socket 写阻塞绝不能拖住鼠标 Raw Input（PITFALLS 2.5）。
        if let Err(err) = self.session.send_key(modifier, &usages) {
            self.note_write_failure(&err);
            return false;
        }
        self.note_write_success(None);
        suppress
    }

    /// 锁内完成状态判定与键位表更新（对应 Python _prepare_key_locked）。
    fn prepare_key(&self, inner: &mut Inner, event: &KeyEvent) -> (bool, Option<(u8, Vec<u8>)>) {
        self.track_safety(inner, event);
        if let Some(win_vk) = Self::win_vk(event) {
            // Windows 键在接管期间必须整键消失：放行 keydown 会打开开始菜单，
            // 而 topmost 遮罩会拦住它的鼠标交互，用户看到的就是"整屏卡死"。
            if inner.state == State::Captured || inner.swallowed_win.contains(&win_vk) {
                if event.down {
                    inner.swallowed_win.insert(win_vk);
                } else {
                    inner.swallowed_win.remove(&win_vk);
                }
                return (true, None);
            }
            inner.swallowed_win.remove(&win_vk);
            return (false, None);
        }
        if self.is_passthrough(inner, event) {
            return (false, None);
        }
        if inner.state != State::Captured {
            return (false, None);
        }
        let keyboard = &self.cfg.keyboard;
        if !keyboard.forward {
            return (false, None);
        }
        if is_safety_letter(event.vk) && !inner.ctrl_sides.is_empty() && !inner.alt_sides.is_empty() {
            rdebug!("router.safety_hotkey vk=0x{:02X}", event.vk);
            return (false, None);
        }
        let keymap = match self.ensure_keymap(inner) {
            Some(keymap) => keymap,
            None => return (false, None),
        };
        let report = match self.keymap_report(&keymap, event) {
            Some(report) => report,
            None => return (false, None),
        };
        let suppress = keyboard.suppress_pc;
        if suppress && !inner.suppress_logged {
            inner.suppress_logged = true;
            eprintln!(
                "[router] kbd.suppress.on layout={}",
                keyboard.layout
            );
        }
        (suppress, Some(report))
    }

    /// 维护 Ctrl/Alt 左右侧集合（对应 Python _track_safety）。
    fn track_safety(&self, inner: &mut Inner, event: &KeyEvent) {
        if is_ctrl_vk(event.vk) {
            let side = Self::modifier_side(event, 0xA3, 0x11);
            if event.down {
                inner.ctrl_sides.insert(side);
            } else {
                inner.ctrl_sides.remove(&side);
            }
        } else if is_alt_vk(event.vk) {
            let side = Self::modifier_side(event, 0xA5, 0x12);
            if event.down {
                inner.alt_sides.insert(side);
            } else {
                inner.alt_sides.remove(&side);
            }
        }
    }

    /// 识别左/右 Win；兼容只给扫描码的事件源（对应 Python _win_vk）。
    fn win_vk(event: &KeyEvent) -> Option<i32> {
        if is_win_vk(event.vk) {
            return Some(event.vk);
        }
        if event.vk == 0 && is_win_vk(event.scan & 0xFF) {
            return Some(event.scan & 0xFF);
        }
        None
    }

    /// 区分修饰键左右侧（对应 Python _modifier_side）。
    fn modifier_side(event: &KeyEvent, right_vk: i32, generic_vk: i32) -> i32 {
        if event.vk == right_vk {
            return 1;
        }
        if event.vk == generic_vk {
            let extended = event.is_extended || (event.scan & 0xE000) != 0;
            return if extended { 1 } else { 0 };
        }
        0
    }

    /// 是否原样透传（对应 Python _is_passthrough：只看已加载的 keymap）。
    fn is_passthrough(&self, inner: &Inner, event: &KeyEvent) -> bool {
        if inner.passthrough_vks.contains(&event.vk) {
            return true;
        }
        let keymap = match inner.keymap.as_ref() {
            Some(keymap) => Arc::clone(keymap),
            None => return false,
        };
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| keymap.is_passthrough(event)));
        match outcome {
            Ok(value) => value,
            Err(_) => {
                rdebug!("keymap.is_passthrough 失败");
                false
            }
        }
    }

    /// 惰性构造内置键位表（对应 Python _ensure_keymap_locked）。
    fn ensure_keymap(&self, inner: &mut Inner) -> Option<Arc<dyn KeymapPort>> {
        if inner.keymap_loaded {
            return inner.keymap.clone();
        }
        inner.keymap_loaded = true;
        let names = self.cfg.keyboard.passthrough_keys.clone();
        let keymap: Arc<dyn KeymapPort> = Arc::new(BuiltinKeymap::new(names));
        inner.keymap = Some(Arc::clone(&keymap));
        Some(keymap)
    }

    /// 喂键位表（对应 Python _keymap_report）。
    fn keymap_report(
        &self,
        keymap: &Arc<dyn KeymapPort>,
        event: &KeyEvent,
    ) -> Option<(u8, Vec<u8>)> {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| keymap.on_key(event)));
        match outcome {
            Ok(report) => report,
            Err(_) => {
                eprintln!("[router] router.keymap_failed");
                None
            }
        }
    }
}

impl CursorEstimator {
    /// 平板宽（浮点原值，对应 Python 的 self.W）。
    pub fn raw_width(&self) -> f64 {
        self.width
    }

    /// 平板高（浮点原值）。
    pub fn raw_height(&self) -> f64 {
        self.height
    }

    /// 精确中心目标 ((W-1)/2, (H-1)/2)（对应 Python 里 (estimator.W - 1) / 2）。
    pub fn center(&self) -> (f64, f64) {
        ((self.width - 1.0) / 2.0, (self.height - 1.0) / 2.0)
    }
}

impl Drop for Router {
    /// 对应 Python __del__：只释放遮罩，异常一律吞掉。
    fn drop(&mut self) {
        let _outcome = std::panic::catch_unwind(AssertUnwindSafe(|| self.mask.release()));
    }
}

/// 把 UTF-8 文本截到最多 max_bytes 字节，且不切断多字节字符（对应 Python bytes[:4096].decode(ignore)）。
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 是否 Ctrl 家族的 VK（Python _CTRL_VKS）。
fn is_ctrl_vk(vk: i32) -> bool {
    matches!(vk, 0x11 | 0xA2 | 0xA3)
}

/// 是否 Alt 家族的 VK（Python _ALT_VKS）。
fn is_alt_vk(vk: i32) -> bool {
    matches!(vk, 0x12 | 0xA4 | 0xA5)
}

/// 是否左/右 Win（Python _WIN_VKS）。
fn is_win_vk(vk: i32) -> bool {
    matches!(vk, 0x5B | 0x5C)
}

/// 保命热键的字母（Ctrl+Alt+S / Ctrl+Alt+Q，Python _SAFETY_LETTERS）。
fn is_safety_letter(vk: i32) -> bool {
    matches!(vk, 0x53 | 0x51)
}

// ---------------------------------------------------------------------------
// Windows 回退与计时精度（Python 里分别是 ctypes SetCursorPos 与
// winmm.timeBeginPeriod；这里尽量用已启用的 windows feature，缺 feature 时降级为 no-op）
// ---------------------------------------------------------------------------

/// 设置系统光标的最后回退（对应 Python ctypes.windll.user32.SetCursorPos）。
#[cfg(windows)]
fn fallback_set_cursor_pos(x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    let _result = unsafe { SetCursorPos(x, y) };
}

/// 非 Windows 平台的空实现（本 crate 仅支持 Windows，保留以便类型检查）。
#[cfg(not(windows))]
fn fallback_set_cursor_pos(_x: i32, _y: i32) {}

/// 提高系统计时精度（对应 Python _timer_begin）；返回是否成功。
#[cfg(windows)]
fn timer_period_begin(period_ms: u32) -> bool {
    match load_winmm_timer("timeBeginPeriod") {
        Some(function) => (unsafe { function(period_ms) }) == 0,
        None => false,
    }
}

/// 还原系统计时精度（对应 Python _timer_end）。
#[cfg(windows)]
fn timer_period_end(period_ms: u32) {
    if let Some(function) = load_winmm_timer("timeEndPeriod") {
        let _result = unsafe { function(period_ms) };
    }
}

/// 非 Windows 平台的空实现。
#[cfg(not(windows))]
fn timer_period_begin(_period_ms: u32) -> bool {
    false
}

/// 非 Windows 平台的空实现。
#[cfg(not(windows))]
fn timer_period_end(_period_ms: u32) {}

/// 从 winmm.dll 动态解析 timeBeginPeriod / timeEndPeriod。
///
/// crate 没有启用 Win32_Media feature（不允许改 Cargo.toml），因此用 LibraryLoader 动态取地址；
/// winmm 常驻不卸载（进程级计时精度，句柄泄漏一次可忽略）。
#[cfg(windows)]
fn load_winmm_timer(name: &str) -> Option<unsafe extern "system" fn(u32) -> u32> {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let module_name: Vec<u16> = "winmm.dll\0".encode_utf16().collect();
    let module = unsafe { LoadLibraryW(PCWSTR(module_name.as_ptr())) }.ok()?;
    let symbol: Vec<u8> = name.bytes().chain(std::iter::once(0u8)).collect();
    let address = unsafe { GetProcAddress(module, PCSTR(symbol.as_ptr())) }?;
    let raw: unsafe extern "system" fn() -> isize = address;
    Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(u32) -> u32>(raw) })
}



/// 测试专用：探测 Router 状态锁是否空闲（用于验证键盘发送在锁外，PITFALLS 2.5）。
#[cfg(test)]
impl Router {
    fn try_lock_inner_for_test(&self) -> bool {
        match self.inner.try_lock() {
            Ok(guard) => {
                drop(guard);
                true
            }
            Err(_) => false,
        }
    }
}

// ===========================================================================
// 测试替身与门槛测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;
    use std::sync::Weak;

    use crate::logic::geometry::{Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect};

    // -----------------------------------------------------------------------
    // 假时钟
    // -----------------------------------------------------------------------

    /// 可精确推进的假时钟（对应 tests/fakes.py::FakeClock）。
    #[derive(Debug, Default)]
    struct FakeClock {
        now: Mutex<f64>,
    }

    impl FakeClock {
        /// 从给定时刻开始（make_rig 用 1000.0）。
        fn new(now: f64) -> Self {
            let clock = Self::default();
            *lock_mutex(&clock.now) = now;
            clock
        }

        /// 当前时刻。
        fn value(&self) -> f64 {
            *lock_mutex(&self.now)
        }

        /// 前进 seconds 秒。
        fn advance(&self, seconds: f64) {
            *lock_mutex(&self.now) += seconds;
        }

        /// 前进 ms 毫秒。
        fn advance_ms(&self, ms: i64) {
            self.advance(ms as f64 / 1000.0);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> f64 {
            self.value()
        }
    }

    // -----------------------------------------------------------------------
    // 假会话
    // -----------------------------------------------------------------------

    /// 会话替身（对应 tests/fakes.py::FakeSession）。
    #[derive(Default)]
    struct FakeSession {
        facts: Mutex<Option<DeviceFacts>>,
        alive: Mutex<bool>,
        socket_ok: Mutex<bool>,
        has_facts: AtomicBool,
        start_error: Mutex<Option<ErrorCode>>,
        reconnect_error: Mutex<Option<ErrorCode>>,
        fail_sends: AtomicI32,
        mouse_reports: Mutex<Vec<(i32, i32, i32, i32)>>,
        key_reports: Mutex<Vec<(u8, Vec<u8>)>>,
        cursor_sets: Mutex<Vec<(i32, i32)>>,
        pointer_speed_sets: Mutex<Vec<i32>>,
        clipboards: Mutex<Vec<String>>,
        start_calls: AtomicI32,
        stop_calls: AtomicI32,
        reconnect_calls: AtomicI32,
        health_calls: AtomicI32,
        ensure_pointer_speed_calls: AtomicI32,
        cancel_calls: AtomicI32,
        facts_callback: Mutex<Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>>,
        clipboard_callback: Mutex<Option<Arc<dyn Fn(String) + Send + Sync>>>,
        lock_probe: Mutex<Option<Weak<Router>>>,
    }

    impl FakeSession {
        /// 默认连接状态的会话（facts = 1600x2560）。
        fn new() -> Self {
            let session = Self::default();
            *lock_mutex(&session.facts) = Some(default_facts());
            *lock_mutex(&session.alive) = true;
            *lock_mutex(&session.socket_ok) = true;
            session.has_facts.store(true, Ordering::SeqCst);
            session
        }

        /// 已记录的鼠标报文。
        fn reports(&self) -> Vec<(i32, i32, i32, i32)> {
            lock_mutex(&self.mouse_reports).clone()
        }

        /// 清空已记录的鼠标报文。
        fn clear_reports(&self) {
            lock_mutex(&self.mouse_reports).clear();
        }

        /// 已记录的键盘报文。
        fn keys(&self) -> Vec<(u8, Vec<u8>)> {
            lock_mutex(&self.key_reports).clone()
        }

        /// 清空已记录的键盘报文。
        fn clear_keys(&self) {
            lock_mutex(&self.key_reports).clear();
        }

        /// 设置设备事实（None = 未连接）。
        fn set_facts(&self, facts: Option<DeviceFacts>) {
            self.has_facts.store(facts.is_some(), Ordering::SeqCst);
            *lock_mutex(&self.facts) = facts;
        }

        /// 模拟写失败次数。
        fn set_fail_sends(&self, count: i32) {
            self.fail_sends.store(count, Ordering::SeqCst);
        }

        /// 模拟健康状态。
        fn set_health(&self, alive: bool, socket_ok: bool) {
            *lock_mutex(&self.alive) = alive;
            *lock_mutex(&self.socket_ok) = socket_ok;
        }

        /// 计数器快照。
        fn count(&self, counter: &AtomicI32) -> i32 {
            counter.load(Ordering::SeqCst)
        }
    }

    impl fmt::Debug for FakeSession {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("FakeSession")
        }
    }

    impl SessionPort for FakeSession {
        fn send_mouse(&self, buttons: i32, dx: i32, dy: i32, wheel: i32) -> Result<(), RouterError> {
            let remaining = self.fail_sends.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_sends.store(remaining - 1, Ordering::SeqCst);
                return Err(RouterError::new(ErrorCode::SocketBroken, "模拟写失败"));
            }
            lock_mutex(&self.mouse_reports).push((buttons, dx, dy, wheel));
            Ok(())
        }

        fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), RouterError> {
            // 转发路径必须在 Router 锁外（PITFALLS 2.5）；send_keyboard_zero 走空 usages，
            // 按 PITFALLS 2.4 它本来就发生在锁内，因此只检查真正转发按键的调用。
            let probe = lock_mutex(&self.lock_probe).clone();
            if !usages.is_empty() {
                if let Some(weak) = probe {
                if let Some(router) = weak.upgrade() {
                    assert!(
                            router.try_lock_inner_for_test(),
                            "键盘发送发生在 Router 锁内（PITFALLS 2.5）"
                        );
                    }
                }
            }
            lock_mutex(&self.key_reports).push((modifier, usages.to_vec()));
            Ok(())
        }

        fn facts(&self) -> Option<DeviceFacts> {
            if !self.has_facts.load(Ordering::SeqCst) {
                return None;
            }
            lock_mutex(&self.facts).clone()
        }

        fn health(&self) -> Result<Option<HealthReport>, RouterError> {
            self.health_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(HealthReport {
                alive: *lock_mutex(&self.alive),
                socket_ok: *lock_mutex(&self.socket_ok),
                uhid_created: true,
                server_alive: true,
                consecutive_failures: 0,
                last_error: String::new(),
                uptime_s: 1.0,
                sent: 0,
            }))
        }

        fn set_clipboard(&self, text: &str) -> Result<(), RouterError> {
            lock_mutex(&self.clipboards).push(text.to_string());
            Ok(())
        }

        fn stop(&self) {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn start(&self) -> Result<DeviceFacts, RouterError> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock_mutex(&self.start_error) {
                return Err(RouterError::new(code, "模拟 start 失败"));
            }
            if !self.has_facts.load(Ordering::SeqCst) {
                return Err(RouterError::device_offline("会话没有 DeviceFacts"));
            }
            let facts = default_facts();
            *lock_mutex(&self.facts) = Some(facts.clone());
            Ok(facts)
        }

        fn reconnect(&self) -> Option<Result<Option<DeviceFacts>, RouterError>> {
            self.reconnect_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock_mutex(&self.reconnect_error) {
                return Some(Err(RouterError::new(code, "模拟 reconnect 失败")));
            }
            Some(Ok(self.facts()))
        }

        fn ensure_pointer_speed(&self) -> Option<Result<(), RouterError>> {
            self.ensure_pointer_speed_calls.fetch_add(1, Ordering::SeqCst);
            Some(Ok(()))
        }

        fn set_pointer_speed(&self, speed: i32) -> Option<Result<(), RouterError>> {
            lock_mutex(&self.pointer_speed_sets).push(speed);
            Some(Ok(()))
        }

        fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
            lock_mutex(&self.cursor_sets).push((x, y));
            true
        }

        fn cancel_start(&self) -> bool {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            true
        }

        fn set_facts_callback(
            &self,
            callback: Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>,
        ) -> bool {
            *lock_mutex(&self.facts_callback) = callback;
            true
        }

        fn set_clipboard_callback(
            &self,
            callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
        ) -> bool {
            *lock_mutex(&self.clipboard_callback) = callback;
            true
        }
    }

    /// 默认设备事实（对应 fakes.default_facts）。
    fn default_facts() -> DeviceFacts {
        DeviceFacts::new("FAKE0001", "FAKE", 1600, 2560, 0, 1.0, 0)
    }

    // -----------------------------------------------------------------------
    // 假桌面
    // -----------------------------------------------------------------------

    /// 光标查询状态。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeCursor {
        /// 有坐标。
        Position(i32, i32),
        /// 有 getter 但读不到。
        Missing,
        /// 没有绝对光标源。
        Unsupported,
    }

    /// 桌面替身（对应 tests/fakes.py::FakeDesktop）。
    #[derive(Debug)]
    struct FakeDesktop {
        monitors: Mutex<Vec<MonitorInfo>>,
        segments: Mutex<Vec<EdgeSegment>>,
        cursor: Mutex<FakeCursor>,
        refresh_calls: AtomicI32,
        corner_calls: Mutex<Vec<((i32, i32), Option<Edge>)>>,
    }

    impl FakeDesktop {
        /// 单显示器 1920x1080 + 右边缘整段（make_rig 默认）。
        fn new() -> Self {
            Self {
                monitors: Mutex::new(vec![monitor_one()]),
                segments: Mutex::new(vec![right_segment()]),
                cursor: Mutex::new(FakeCursor::Position(960, 540)),
                refresh_calls: AtomicI32::new(0),
                corner_calls: Mutex::new(Vec::new()),
            }
        }

        /// 替换显示器列表。
        fn set_monitors(&self, monitors: Vec<MonitorInfo>) {
            *lock_mutex(&self.monitors) = monitors;
        }

        /// 替换外轮廓段。
        fn set_segments(&self, segments: Vec<EdgeSegment>) {
            *lock_mutex(&self.segments) = segments;
        }

        /// 设置光标位置。
        fn set_cursor(&self, position: Option<(i32, i32)>) {
            *lock_mutex(&self.cursor) = match position {
                Some((x, y)) => FakeCursor::Position(x, y),
                None => FakeCursor::Missing,
            };
        }

        /// 记录到的角落裁决调用。
        fn corners(&self) -> Vec<((i32, i32), Option<Edge>)> {
            lock_mutex(&self.corner_calls).clone()
        }

        /// 刷新次数。
        fn refreshes(&self) -> i32 {
            self.refresh_calls.load(Ordering::SeqCst)
        }
    }

    impl DesktopPort for FakeDesktop {
        fn monitors(&self) -> Vec<MonitorInfo> {
            lock_mutex(&self.monitors).clone()
        }

        fn bounding_box(&self) -> Rect {
            let monitors = lock_mutex(&self.monitors);
            let mut iter = monitors.iter();
            let mut bounds = match iter.next() {
                Some(first) => first.rect,
                None => return Rect::new(0, 0, 0, 0),
            };
            for monitor in iter {
                bounds = bounds.union(&monitor.rect);
            }
            bounds
        }

        fn cursor_pos(&self) -> CursorQuery {
            match *lock_mutex(&self.cursor) {
                FakeCursor::Position(x, y) => CursorQuery::Position(x, y),
                FakeCursor::Missing => CursorQuery::Unavailable,
                FakeCursor::Unsupported => CursorQuery::Unsupported,
            }
        }

        fn segments_at(&self, x: i32, y: i32, band: i32) -> Vec<EdgeSegment> {
            let monitors = lock_mutex(&self.monitors);
            let mut out = Vec::new();
            for segment in lock_mutex(&self.segments).iter() {
                let monitor = match monitors
                    .iter()
                    .find(|monitor| monitor.handle == segment.monitor_handle)
                {
                    Some(monitor) => monitor,
                    None => continue,
                };
                let rect = monitor.rect;
                let (line, coordinate, along) = match segment.edge {
                    Edge::Right => (rect.right, x, y),
                    Edge::Left => (rect.left - 1, x, y),
                    Edge::Top => (rect.top - 1, y, x),
                    Edge::Bottom => (rect.bottom, y, x),
                };
                if line - band <= coordinate
                    && coordinate <= line + band
                    && segment.contains_along(along)
                {
                    out.push(*segment);
                }
            }
            out
        }

        fn resolve_corner(
            &self,
            segments: &[EdgeSegment],
            delta: (i32, i32),
            priority: Option<Edge>,
        ) -> Option<EdgeSegment> {
            lock_mutex(&self.corner_calls).push((delta, priority));
            if segments.is_empty() {
                return None;
            }
            if segments.len() == 1 {
                return Some(segments[0]);
            }
            let mut scored: Vec<(i32, EdgeSegment)> = segments
                .iter()
                .map(|segment| {
                    let (nx, ny) = segment.edge.outward_normal();
                    (delta.0 * nx + delta.1 * ny, *segment)
                })
                .collect();
            scored.sort_by(|left, right| right.0.cmp(&left.0));
            let top = scored[0];
            let second = scored.get(1).map(|item| item.0).unwrap_or(0);
            let top_score = top.0;
            let ratio = f64::from(top_score - second) / f64::from(top_score.max(1));
            if top_score > 0 && ratio > C::CORNER_DIRECTION_RATIO {
                return Some(top.1);
            }
            if let Some(edge) = priority {
                if let Some(found) = segments.iter().find(|segment| segment.edge == edge) {
                    return Some(*found);
                }
            }
            Some(top.1)
        }

        fn exterior_segments(&self) -> Vec<EdgeSegment> {
            lock_mutex(&self.segments).clone()
        }

        fn segment_exists(&self, segment: &EdgeSegment) -> Option<bool> {
            Some(
                lock_mutex(&self.segments)
                    .iter()
                    .any(|item| item == segment),
            )
        }

        fn refresh(&self) -> Result<(), RouterError> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
            *lock_mutex(&self.cursor) = FakeCursor::Position(x, y);
            true
        }
    }

    /// make_rig 的显示器 1：1920x1080 主屏。
    fn monitor_one() -> MonitorInfo {
        MonitorInfo::new(1, "\\\\.\\DISPLAY1", Rect::new(0, 0, 1920, 1080), 96, true)
    }

    /// 右边缘整段。
    fn right_segment() -> EdgeSegment {
        EdgeSegment::new(1, Edge::Right, 0, 1080).expect("右边缘段合法")
    }

    // -----------------------------------------------------------------------
    // 假遮罩
    // -----------------------------------------------------------------------

    /// 遮罩替身（对应 tests/fakes.py::FakeMask）。
    #[derive(Debug, Default)]
    struct FakeMask {
        shown: Mutex<Vec<Option<Rect>>>,
        release_calls: AtomicI32,
        fail_show: AtomicBool,
    }

    impl FakeMask {
        /// 曾经 show 过的 clip。
        fn shown(&self) -> Vec<Option<Rect>> {
            lock_mutex(&self.shown).clone()
        }

        /// release 调用次数。
        fn releases(&self) -> i32 {
            self.release_calls.load(Ordering::SeqCst)
        }
    }

    impl MaskPort for FakeMask {
        fn show(&self, clip: Option<Rect>) -> Result<(), RouterError> {
            if self.fail_show.load(Ordering::SeqCst) {
                return Err(RouterError::new(ErrorCode::ClipCursorLeak, "模拟 show 失败"));
            }
            lock_mutex(&self.shown).push(clip);
            Ok(())
        }

        fn release(&self) {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn diagnose(&self) -> String {
            let shown = lock_mutex(&self.shown);
            let clip = shown.last().copied().flatten();
            format!(
                "visible=False clip={clip:?} windows={}",
                if clip.is_some() { 1 } else { 0 }
            )
        }
    }

    // -----------------------------------------------------------------------
    // 假键位表
    // -----------------------------------------------------------------------

    /// 键位表替身（对应 tests/fakes.py::FakeKeymap：固定报告 (0, [4])）。
    #[derive(Debug)]
    struct FakeKeymap {
        report: Mutex<(u8, Vec<u8>)>,
        events: Mutex<Vec<KeyEvent>>,
        passthrough: HashSet<i32>,
        reset_calls: AtomicI32,
    }

    impl FakeKeymap {
        /// 默认报告 (0, [4])。
        fn new() -> Self {
            Self {
                report: Mutex::new((0, vec![4])),
                events: Mutex::new(Vec::new()),
                passthrough: HashSet::new(),
                reset_calls: AtomicI32::new(0),
            }
        }

        /// 修改固定报告。
        fn set_report(&self, modifier: u8, usages: Vec<u8>) {
            *lock_mutex(&self.report) = (modifier, usages);
        }

        /// 收到的按键事件数。
        fn event_count(&self) -> usize {
            lock_mutex(&self.events).len()
        }

        /// reset 调用次数。
        fn resets(&self) -> i32 {
            self.reset_calls.load(Ordering::SeqCst)
        }
    }

    impl KeymapPort for FakeKeymap {
        fn on_key(&self, event: &KeyEvent) -> Option<(u8, Vec<u8>)> {
            lock_mutex(&self.events).push(*event);
            Some(lock_mutex(&self.report).clone())
        }

        fn is_passthrough(&self, event: &KeyEvent) -> bool {
            self.passthrough.contains(&event.vk)
        }

        fn reset(&self) {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    // -----------------------------------------------------------------------
    // 测试装置（对应 tests/test_router.py::make_rig）
    // -----------------------------------------------------------------------

    /// 一套测试装置：Router + 全部替身 + 迁移记录。
    struct Rig {
        router: Arc<Router>,
        session: Arc<FakeSession>,
        desktop: Arc<FakeDesktop>,
        mask: Arc<FakeMask>,
        keymap: Arc<FakeKeymap>,
        clock: Arc<FakeClock>,
        transitions: Arc<Mutex<Vec<(State, State, String)>>>,
    }

    /// 与 tests/fakes.py::FakeConfig 对齐的配置。
    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.motion.arm_px = i64::from(C::ARM_PX);
        cfg.motion.arm_window_ms = i64::from(C::ARM_WINDOW_MS);
        cfg.motion.exit_overflow_px = i64::from(C::EXIT_OVERFLOW_PX);
        cfg.motion.edge_band_px = i64::from(C::EDGE_BAND_PX);
        cfg.motion.debounce_ms = i64::from(C::DEBOUNCE_MS);
        cfg.motion.entry_margin_px = i64::from(C::ENTRY_MARGIN_PX);
        cfg.motion.gain = 1.0;
        cfg.motion.report_hz = 1000;
        cfg.motion.hid_clamp = i64::from(C::HID_CLAMP);
        cfg.keyboard.forward = true;
        cfg.keyboard.suppress_pc = true;
        cfg.keyboard.passthrough_keys = vec!["VK_LWIN".to_string(), "VK_RWIN".to_string()];
        cfg.misc.clipboard_sync = true;
        cfg.misc.restore_pointer_speed = -3;
        cfg.placement.corner_priority = "right".to_string();
        cfg.device.auto_connect = true;
        cfg
    }

    /// 默认摆位：显示器 1 的右边缘（等价 make_placement）。
    fn default_placement() -> Placement {
        Placement::new(right_segment(), 0.5, Align::Proportional, None).expect("默认摆位合法")
    }

    /// 构造一套装置（不调用 start，避免真起线程）。
    fn make_rig(cfg: Config) -> Rig {
        let clock = Arc::new(FakeClock::new(1000.0));
        let session = Arc::new(FakeSession::new());
        let desktop = Arc::new(FakeDesktop::new());
        let mask = Arc::new(FakeMask::default());
        let keymap = Arc::new(FakeKeymap::new());
        let transitions: Arc<Mutex<Vec<(State, State, String)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let session_port: Arc<dyn SessionPort> = session.clone();
        let desktop_port: Arc<dyn DesktopPort> = desktop.clone();
        let mask_port: Arc<dyn MaskPort> = mask.clone();
        let clock_port: Arc<dyn Clock> = clock.clone();
        let keymap_port: Arc<dyn KeymapPort> = keymap.clone();
        let router = Arc::new(
            Router::new(cfg, session_port, desktop_port, mask_port)
                .with_clock(clock_port)
                .with_keymap(keymap_port),
        );
        *lock_mutex(&session.lock_probe) = Some(Arc::downgrade(&router));
        {
            let sink = Arc::clone(&transitions);
            router.set_on_state_changed(move |old, new, reason| {
                lock_mutex(&sink).push((old, new, reason.to_string()));
            });
        }
        router.set_placement(default_placement());
        Rig {
            router,
            session,
            desktop,
            mask,
            keymap,
            clock,
            transitions,
        }
    }

    impl Rig {
        /// 发一个鼠标事件（无按键、无滚轮）。
        fn raw(&self, dx: i32, dy: i32) {
            self.raw_full(dx, dy, 0, 0);
        }

        /// 发一个带按键位图的鼠标事件。
        fn raw_buttons(&self, dx: i32, dy: i32, buttons: i32) {
            self.raw_full(dx, dy, buttons, 0);
        }

        /// 发一个全字段鼠标事件。
        fn raw_full(&self, dx: i32, dy: i32, buttons: i32, wheel: i32) {
            self.router
                .on_raw(&RawDelta::new(dx, dy, buttons, wheel, false, 0.0));
        }

        /// 出帧。
        fn frame(&self) {
            self.router.sender_frame();
        }

        /// 看护一次。
        fn tick(&self) {
            self.router.tick_supervisor();
        }

        /// 记录的鼠标报文。
        fn reports(&self) -> Vec<(i32, i32, i32, i32)> {
            self.session.reports()
        }

        /// 清空鼠标报文记录。
        fn clear_reports(&self) {
            self.session.clear_reports();
        }

        /// 迁移三元组。
        fn transitions(&self) -> Vec<(State, State, String)> {
            lock_mutex(&self.transitions).clone()
        }

        /// 把光标放到右边缘中点。
        fn at_right_edge(&self) {
            self.desktop.set_cursor(Some((1919, 500)));
        }

        /// 边缘外推一小步，进入 ARMING。
        fn arm(&self) {
            self.at_right_edge();
            self.raw(5, 0);
            assert_eq!(self.router.state(), State::Arming, "应当进入 ARMING");
        }

        /// 手动接管（toggle_manual）。
        fn capture_manually(&self) {
            self.router.toggle_manual();
            assert_eq!(self.router.state(), State::Captured, "手动接管应进入 CAPTURED");
        }

        /// 把队列发空（一帧可突破批次上限发完 burst）。
        fn flush(&self) {
            self.frame();
            self.clear_reports();
        }

        /// 锁内状态访问。
        fn inner<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
            let mut guard = lock_mutex(&self.router.inner);
            f(&mut guard)
        }
    }

    /// 提取 payload 里的整数（缺省 0）。
    fn payload_i32(payload: &serde_json::Value, key: &str) -> i32 {
        payload.get(key).and_then(|value| value.as_i64()).unwrap_or(0) as i32
    }

    /// 逐条回放 fixture 的 script。
    fn apply_step(rig: &Rig, step: &Step) {
        match step.op.as_str() {
            "capture" | "release" => rig.router.toggle_manual(),
            "raw" => rig.raw_full(
                payload_i32(&step.payload, "dx"),
                payload_i32(&step.payload, "dy"),
                payload_i32(&step.payload, "buttons"),
                payload_i32(&step.payload, "wheel"),
            ),
            "key" => {
                let extended = step
                    .payload
                    .get("extended")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let down = step
                    .payload
                    .get("down")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true);
                let _suppressed = rig.router.on_key(&KeyEvent::new(
                    payload_i32(&step.payload, "vk"),
                    payload_i32(&step.payload, "scan"),
                    extended,
                    down,
                ));
            }
            "frame" => {
                let times = step
                    .payload
                    .get("times")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1);
                for _ in 0..times.max(0) {
                    rig.frame();
                }
            }
            other => panic!("fixture 里有未知 op: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // 门槛测试：回放 tests/fixtures/router_scenarios.json
    // -----------------------------------------------------------------------

    /// fixture 里的一个场景。
    #[derive(serde::Deserialize)]
    struct Scenario {
        name: String,
        script: Vec<Step>,
        expect: Expect,
    }

    /// fixture 里的一个脚本步骤。
    #[derive(serde::Deserialize)]
    struct Step {
        op: String,
        #[serde(default)]
        payload: serde_json::Value,
    }

    /// fixture 里记录的期望结果。
    #[derive(serde::Deserialize)]
    struct Expect {
        state: String,
        transitions: Vec<(String, String, String)>,
        mouse_reports: Vec<(i32, i32, i32, i32)>,
        key_reports: Vec<(u8, Vec<u8>)>,
        clip: Option<(i32, i32, i32, i32)>,
        estimator: Option<EstimatorExpect>,
    }

    /// fixture 里记录的估计器快照。
    #[derive(serde::Deserialize)]
    struct EstimatorExpect {
        x: f64,
        y: f64,
        width: i32,
        height: i32,
    }

    /// 门槛测试：5 个场景逐条回放，state/transitions/mouse_reports/key_reports/clip/estimator 全比。
    #[test]
    fn fixture_scenarios_replay_identically() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/router_scenarios.json"
        );
        let text = std::fs::read_to_string(path).expect("读取 router_scenarios.json");
        let scenarios: Vec<Scenario> =
            serde_json::from_str(&text).expect("解析 router_scenarios.json");
        assert_eq!(scenarios.len(), 5, "门槛 fixture 应有 5 个场景");
        for scenario in &scenarios {
            let rig = make_rig(test_config());
            for step in &scenario.script {
                apply_step(&rig, step);
            }
            assert_scenario(&rig, scenario);
        }
    }

    /// 比对单个场景的全部期望。
    fn assert_scenario(rig: &Rig, scenario: &Scenario) {
        let name = scenario.name.as_str();
        let expected = &scenario.expect;

        // (1) 状态
        assert_eq!(
            rig.router.state().as_str(),
            expected.state.as_str(),
            "{name}: 最终状态不一致"
        );

        // (2) 迁移三元组（字符串逐条相等）
        let actual_transitions: Vec<(String, String, String)> = rig
            .transitions()
            .into_iter()
            .map(|(old, new, reason)| {
                (
                    old.as_str().to_string(),
                    new.as_str().to_string(),
                    reason,
                )
            })
            .collect();
        assert_eq!(
            actual_transitions, expected.transitions,
            "{name}: 状态迁移不一致"
        );

        // (3) 鼠标报文。fixture 由真线程录制，开头可能多 1 条纯心跳 [0,0,0,0]。
        let actual_reports = rig.reports();
        if expected.mouse_reports.len() == actual_reports.len() {
            assert_eq!(
                actual_reports, expected.mouse_reports,
                "{name}: 鼠标报文不一致"
            );
        } else {
            assert_eq!(
                expected.mouse_reports.len(),
                actual_reports.len() + 1,
                "{name}: 鼠标报文数量差 {}（只容忍 fixture 开头多 1 条心跳）",
                expected.mouse_reports.len() as i64 - actual_reports.len() as i64
            );
            assert_eq!(
                expected.mouse_reports[0],
                (0, 0, 0, 0),
                "{name}: 多出的首条必须是心跳"
            );
            assert_eq!(
                actual_reports,
                expected.mouse_reports[1..],
                "{name}: 鼠标报文不一致（已跳过开头心跳）"
            );
        }

        // (4) 键盘报文
        assert_eq!(
            rig.session.keys(),
            expected.key_reports,
            "{name}: 键盘报文不一致"
        );

        // (5) 剪贴条
        let actual_clip = rig.mask.shown().first().copied().flatten();
        let actual_clip = actual_clip.map(|rect| rect.as_tuple());
        assert_eq!(actual_clip, expected.clip, "{name}: clip 不一致");

        // (6) 估计器
        match (&expected.estimator, rig.router.estimator()) {
            (None, None) => {}
            (Some(expected_estimator), Some(estimator)) => {
                assert!(
                    (estimator.x - expected_estimator.x).abs() < 1e-6,
                    "{name}: estimator.x 期望 {} 实际 {}",
                    expected_estimator.x,
                    estimator.x
                );
                assert!(
                    (estimator.y - expected_estimator.y).abs() < 1e-6,
                    "{name}: estimator.y 期望 {} 实际 {}",
                    expected_estimator.y,
                    estimator.y
                );
                assert_eq!(
                    (estimator.width(), estimator.height()),
                    (expected_estimator.width, expected_estimator.height),
                    "{name}: estimator 尺寸不一致"
                );
            }
            (expected_estimator, estimator) => panic!(
                "{name}: estimator 存在性不一致（期望 {:?}，实际 {:?}）",
                expected_estimator.is_some(),
                estimator.map(|value| (value.width(), value.height()))
            ),
        }
    }

    // -----------------------------------------------------------------------
    // 关键单测
    // -----------------------------------------------------------------------

    /// 进入 CAPTURED：先补发全松、再 warp burst（PITFALLS 2.4），并顶满入口边。
    #[test]
    fn manual_entry_queues_all_zero_release_before_warp_burst() {
        let rig = make_rig(test_config());
        rig.capture_manually();

        assert_eq!(rig.mask.shown().len(), 1, "应显示一次遮罩");
        assert_eq!(
            rig.mask.shown()[0].map(|rect| rect.as_tuple()),
            Some((1919, 0, 1920, 1080)),
            "CLIPCURSOR 条应为右边缘 1 像素宽"
        );
        assert!(rig.reports().is_empty(), "出帧前不应有任何报文");
        assert_eq!(
            rig.router.estimator().and_then(|estimator| estimator.hit()),
            Some(Edge::Left),
            "入口应贴在 LEFT 边"
        );

        rig.frame();
        let reports = rig.reports();
        assert_eq!(reports[0], (0, 0, 0, 0), "首条必须是全零松键");
        assert!(reports.iter().all(|report| report.0 == 0), "接管瞬间所有按键位必须为 0");
        let total: i32 = reports.iter().map(|report| report.1).sum();
        assert!(
            -total >= 1600,
            "入口 pinning 至少覆盖一个平板宽度，实际 {}",
            -total
        );
        assert_eq!(rig.router.estimator().map(|est| est.x), Some(0.0));

        let transitions = rig.transitions();
        assert!(transitions.contains(&(State::Idle, State::Arming, "manual".to_string())));
        assert!(transitions.contains(&(State::Arming, State::Captured, "manual".to_string())));
    }

    /// 心跳必须携带当前按键位图，且不能把按住状态冲掉（PITFALLS 2.2）。
    #[test]
    fn heartbeat_keeps_held_buttons() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();

        rig.raw_buttons(0, 0, 1);
        rig.frame();
        let reports = rig.reports();
        assert_eq!(reports.last().copied(), Some((1, 0, 0, 0)), "按住左键应转发");

        rig.clear_reports();
        rig.frame(); // 队列空 → 心跳
        let heartbeat = rig.reports();
        assert_eq!(heartbeat.last().copied(), Some((1, 0, 0, 0)), "心跳必须带按键位");
        assert_eq!((heartbeat[0].1, heartbeat[0].2), (0, 0), "心跳不带位移");

        rig.clear_reports();
        rig.raw_buttons(3, 0, 0);
        rig.frame();
        assert_eq!(
            rig.reports().last().copied(),
            Some((0, 3, 0, 0)),
            "物理松开后新按键事件应正常转发"
        );
    }

    /// 进入前已按住的键在物理松开前保持中性。
    #[test]
    fn press_before_entry_stays_neutral_until_release() {
        let rig = make_rig(test_config());
        rig.at_right_edge();
        rig.raw_buttons(5, 0, 1); // 按住左键外推 → ARMING
        assert_eq!(rig.router.state(), State::Arming);
        rig.raw_buttons(10, 0, 1);
        rig.raw_buttons(10, 0, 1);
        rig.raw_buttons(10, 0, 1);
        assert_eq!(rig.router.state(), State::Captured);

        rig.flush();
        rig.raw_buttons(5, 0, 1);
        rig.frame();
        assert_eq!(
            rig.reports().last().copied(),
            Some((0, 5, 0, 0)),
            "入口前按住的键必须保持中性"
        );

        rig.clear_reports();
        rig.raw_buttons(2, 0, 0);
        rig.frame();
        assert_eq!(
            rig.reports().last().copied(),
            Some((0, 2, 0, 0)),
            "松键后仍保持中性，直到新按键"
        );

        rig.clear_reports();
        rig.raw_buttons(2, 0, 1);
        rig.frame();
        assert_eq!(
            rig.reports().last().copied(),
            Some((1, 2, 0, 0)),
            "重新按下后应转发按键位"
        );
    }

    /// 滚轮按 120 单位累加/取整，余数留到下次。
    #[test]
    fn wheel_accumulates_120_units_per_detent() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();

        rig.raw_full(0, 0, 0, 120);
        rig.frame();
        assert_eq!(rig.reports().last().copied(), Some((0, 0, 0, 1)));

        rig.clear_reports();
        rig.raw_full(0, 0, 0, 60);
        rig.frame();
        assert_eq!(rig.reports().last().copied(), Some((0, 0, 0, 0)), "60 不足一格");

        rig.clear_reports();
        rig.raw_full(0, 0, 0, 60);
        rig.frame();
        assert_eq!(rig.reports().last().copied(), Some((0, 0, 0, 1)), "余数累加成 1 格");

        rig.clear_reports();
        rig.raw_full(0, 0, 0, -120);
        rig.frame();
        assert_eq!(rig.reports().last().copied(), Some((0, 0, 0, -1)), "反向一格");
    }

    /// Windows 键必须整键吞掉并锁存到 keyup（PITFALLS 2.1）。
    #[test]
    fn win_key_swallowed_and_latched() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.session.clear_keys();

        assert!(rig.router.on_key(&KeyEvent::new(0x5B, 0, false, true)));
        assert!(rig.router.on_key(&KeyEvent::new(0x5B, 0, false, false)));
        assert!(rig.router.on_key(&KeyEvent::new(0x5C, 0, true, true)));
        assert!(rig.router.on_key(&KeyEvent::new(0, 0x5B, false, true)));
        assert!(rig.session.keys().is_empty(), "Win 键不得转发");
        assert_eq!(rig.keymap.event_count(), 0, "Win 键不得进键位表");

        rig.router.emergency_stop();
        assert!(
            rig.router.on_key(&KeyEvent::new(0x5B, 0, false, false)),
            "急停后迟到的 keyup 仍要吞掉"
        );
        assert!(
            !rig.router.on_key(&KeyEvent::new(0x5B, 0, false, false)),
            "锁存清除后再按应放行"
        );
    }

    /// 键盘报文：CAPTURED 时转发并抑制本机，透传/未接管/关闭转发时放行。
    #[test]
    fn keyboard_reports_and_suppress() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.session.clear_keys();

        assert!(
            rig.router.on_key(&KeyEvent::new(0x41, 0, false, true)),
            "suppress_pc 时应吞键"
        );
        assert_eq!(rig.session.keys().last().cloned(), Some((0, vec![4])));

        // Ctrl+Alt+S 保命热键不转发
        rig.router.on_key(&KeyEvent::new(0xA2, 0, false, true));
        rig.router.on_key(&KeyEvent::new(0xA4, 0, false, true));
        rig.session.clear_keys();
        assert!(!rig.router.on_key(&KeyEvent::new(0x53, 0, false, true)));
        assert!(rig.session.keys().is_empty(), "保命热键不得转发");

        // 6KRO：最多 6 个 usage 原样转发
        rig.keymap.set_report(2, vec![4, 5, 6, 7, 8, 9]);
        rig.router.on_key(&KeyEvent::new(0x42, 0, false, true));
        assert_eq!(
            rig.session.keys().last().cloned(),
            Some((2, vec![4, 5, 6, 7, 8, 9]))
        );
    }

    /// forward=false 时不转发但也不吞键。
    #[test]
    fn keyboard_forward_disabled() {
        let mut cfg = test_config();
        cfg.keyboard.forward = false;
        let rig = make_rig(cfg);
        rig.capture_manually();
        rig.session.clear_keys();
        assert!(!rig.router.on_key(&KeyEvent::new(0x41, 0, false, true)));
        assert!(rig.session.keys().is_empty());
    }

    /// 非 CAPTURED 时不转发键盘。
    #[test]
    fn keyboard_ignored_when_not_captured() {
        let rig = make_rig(test_config());
        assert!(!rig.router.on_key(&KeyEvent::new(0x41, 0, false, true)));
        assert!(rig.session.keys().is_empty());
        assert!(!rig.router.on_key(&KeyEvent::new(0x5B, 0, false, true)));
        assert!(!rig.router.on_key(&KeyEvent::new(0x5B, 0, false, false)));
    }

    /// 按键会刷新 PC 绝对光标（防止接管瞬间从旧位置 warp）。
    #[test]
    fn keyboard_refreshes_pc_cursor() {
        let rig = make_rig(test_config());
        rig.desktop.set_cursor(Some((432, 123)));
        rig.router.on_key(&KeyEvent::new(0x41, 0, false, true));
        assert_eq!(rig.inner(|inner| inner.pc_cursor), Some((432, 123)));
    }

    /// ARMING 三种失败原因与慢推兜底。
    #[test]
    fn arm_timeout_reverse_and_left_edge() {
        // timeout：过了观察窗但累计不足 slow_px
        let rig = make_rig(test_config());
        rig.at_right_edge();
        rig.raw(2, 0);
        rig.clock.advance_ms(90);
        rig.raw(2, 0);
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.transitions().contains(&(
            State::Arming,
            State::Idle,
            "arm:timeout".to_string()
        )));

        // arm_slow：过了观察窗但从入口累计已够 slow_px
        let rig = make_rig(test_config());
        rig.arm();
        rig.raw(3, 0); // 累计 3
        rig.raw(3, 0); // 累计 6（< arm_px，未触发）
        rig.clock.advance_ms(90);
        rig.raw(3, 0); // 观察窗已过，按 slow_px=5 兜底
        assert_eq!(rig.router.state(), State::Captured);
        assert!(rig.transitions().contains(&(
            State::Arming,
            State::Captured,
            "arm_slow".to_string()
        )));

        // reverse：反向推回
        let rig = make_rig(test_config());
        rig.arm();
        rig.raw(-5, 0);
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig
            .transitions()
            .contains(&(State::Arming, State::Idle, "arm:reverse".to_string())));

        // left_edge：光标离开边缘带
        let rig = make_rig(test_config());
        rig.arm();
        rig.desktop.set_cursor(Some((1700, 500)));
        rig.raw(5, 0);
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.transitions().contains(&(
            State::Arming,
            State::Idle,
            "arm:left_edge".to_string()
        )));
    }

    /// 角落裁决：方向明确时按方向，方向打平时按 corner_priority。
    #[test]
    fn corner_resolution_prefers_direction_then_priority() {
        let rig = make_rig(test_config());
        rig.desktop.set_segments(vec![
            right_segment(),
            EdgeSegment::new(1, Edge::Top, 0, 1921).expect("顶边段合法"),
        ]);
        rig.desktop.set_cursor(Some((1920, 0)));
        rig.raw(2, -6);
        assert_eq!(rig.router.state(), State::Arming, "角落应能触发 ARMING");
        assert!(!rig.desktop.corners().is_empty(), "应调用 desktop.resolve_corner");
        assert_eq!(
            rig.inner(|inner| inner.arm_segment.map(|segment| segment.edge)),
            Some(Edge::Top),
            "方向更明确的一侧（TOP）应胜出"
        );

        let rig = make_rig(test_config());
        rig.desktop.set_segments(vec![
            right_segment(),
            EdgeSegment::new(1, Edge::Top, 0, 1921).expect("顶边段合法"),
        ]);
        rig.desktop.set_cursor(Some((1920, 0)));
        rig.raw(5, -5);
        assert_eq!(rig.router.state(), State::Arming);
        assert_eq!(
            rig.inner(|inner| inner.arm_segment.map(|segment| segment.edge)),
            Some(Edge::Right),
            "方向打平时按 corner_priority=right"
        );
        assert_eq!(
            rig.router.placement().map(|placement| placement.segment.edge),
            Some(Edge::Right),
            "角落裁决不得改写已保存的摆位"
        );
    }

    /// 溢出出口：只有贴入口边向外推才累计，达到阈值后交还并去抖。
    #[test]
    fn overflow_exits_after_hit_and_debounces() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();

        rig.raw(30, 0); // 反向（向内）
        assert_eq!(rig.inner(|inner| inner.overflow), 0.0, "向内推不累计");
        assert_eq!(rig.router.state(), State::Captured);

        rig.raw(-30, 0); // 回到入口边并外推
        assert_eq!(rig.inner(|inner| inner.overflow), 30.0);
        assert_eq!(rig.router.state(), State::Captured);

        rig.raw(-50, 0);
        assert_eq!(rig.router.state(), State::Returning);
        assert!(rig.transitions().contains(&(
            State::Captured,
            State::Returning,
            "exit".to_string()
        )));
        assert!(rig.mask.releases() >= 1);

        rig.clock.advance_ms(200);
        rig.tick();
        assert_eq!(rig.router.state(), State::Returning, "去抖未到不得回 IDLE");
        rig.clock.advance_ms(60);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle);
    }

    /// 按住键时不得因溢出退出。
    #[test]
    fn held_button_blocks_exit() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();
        rig.raw_buttons(-300, 0, 1);
        assert_eq!(rig.router.state(), State::Captured, "按住期间不得交还");
        rig.raw_buttons(-90, 0, 0);
        assert_eq!(rig.router.state(), State::Returning);
    }

    /// 交还：补发全松、清键位表、恢复光标，并在去抖后回 IDLE。
    #[test]
    fn manual_release_sends_all_zeros_and_restores_cursor() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();
        rig.raw_buttons(6, 0, 1);
        rig.frame();
        rig.clear_reports();

        rig.router.toggle_manual();
        assert_eq!(rig.router.state(), State::Returning);
        assert_eq!(
            rig.reports().last().copied(),
            Some((0, 0, 0, 0)),
            "交还必须补发全松"
        );
        assert_eq!(rig.session.keys().last().cloned(), Some((0, Vec::new())));
        assert_eq!(rig.keymap.resets(), 1, "交还应重置键位表");
        assert!(rig.mask.releases() >= 1);
        let cursor = lock_mutex(&rig.session.cursor_sets)
            .last()
            .copied()
            .expect("应恢复 PC 光标");
        assert!(
            (0..1920).contains(&cursor.0) && (0..1080).contains(&cursor.1),
            "恢复的光标必须在主屏内，实际 {cursor:?}"
        );

        rig.clock.advance_ms(200);
        rig.tick();
        assert_eq!(rig.router.state(), State::Returning);
        rig.clock.advance_ms(60);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle);
    }

    /// 急停：立即交还、补发全松、清键位表，之后仍可再次接管。
    #[test]
    fn emergency_stop_releases_and_allows_reentry() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.session.clear_keys();
        rig.router.emergency_stop();
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.mask.releases() >= 1);
        assert_eq!(rig.session.keys().last().cloned(), Some((0, Vec::new())));
        assert!(rig.keymap.resets() >= 1);
        assert_eq!(
            rig.transitions().last().cloned(),
            Some((State::Returning, State::Idle, "panic".to_string()))
        );

        rig.capture_manually();
        assert_eq!(rig.router.state(), State::Captured);
    }

    /// 没有摆位时手动接管必须安全空操作。
    #[test]
    fn manual_without_placement_is_safe() {
        let clock = Arc::new(FakeClock::new(1000.0));
        let session = Arc::new(FakeSession::new());
        let desktop = Arc::new(FakeDesktop::new());
        let mask = Arc::new(FakeMask::default());
        let sessions: Arc<dyn SessionPort> = session.clone();
        let desktops: Arc<dyn DesktopPort> = desktop.clone();
        let masks: Arc<dyn MaskPort> = mask.clone();
        let clocks: Arc<dyn Clock> = clock.clone();
        let router = Arc::new(Router::new(test_config(), sessions, desktops, masks).with_clock(clocks));
        router.toggle_manual();
        assert_eq!(router.state(), State::Idle);
        assert!(mask.shown().is_empty());
    }

    /// 一批最多 8 条；burst 不受批次上限约束。
    #[test]
    fn drain_batch_limit_is_eight() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();
        for _ in 0..10 {
            rig.router.queue_mouse(1, 1, 0, 0);
        }
        rig.frame();
        assert_eq!(rig.reports().len(), C::MAX_BATCH, "单帧最多 8 条");
        rig.clear_reports();
        rig.frame();
        assert_eq!(rig.reports().len(), 2, "剩余 2 条");
        rig.clear_reports();
        rig.frame();
        let tail = rig.reports();
        assert_eq!(tail.len(), 1, "队列空时发心跳");
        assert_eq!((tail[0].1, tail[0].2), (0, 0));
    }

    /// 队列饱和时丢弃心跳、保留真实报文。
    #[test]
    fn saturated_queue_drops_heartbeats_keeps_reports() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.flush();
        for _ in 0..C::SEND_QUEUE_MAXLEN {
            rig.router.queue_mouse(1, 0, 0, 0);
        }
        rig.router.queue_mouse(0, 0, 0, 0);
        rig.router.append_send(SendMsg::heartbeat(1));

        let mut all: Vec<(i32, i32, i32, i32)> = Vec::new();
        while all.len() < C::SEND_QUEUE_MAXLEN + 1 {
            rig.clear_reports();
            rig.frame();
            all.extend(rig.reports());
        }
        assert_eq!(all.len(), C::SEND_QUEUE_MAXLEN + 1, "真实报文一条都不能丢");
        assert_eq!(
            all.last().copied(),
            Some((0, 0, 0, 0)),
            "最后一条应是全零松键而不是心跳"
        );
    }

    /// 模拟写失败连续 3 次进入 FAULT。
    #[test]
    fn write_failures_fault_after_three() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.session.set_fail_sends(3);
        rig.frame();
        assert_eq!(rig.router.state(), State::Fault);
        assert!(rig.mask.releases() >= 1);
        assert!(rig.transitions().contains(&(
            State::Captured,
            State::Fault,
            "fault:E106".to_string()
        )));
    }

    /// mask.show 失败 → 包裹成 E900 并进 FAULT。
    #[test]
    fn mask_show_failure_faults() {
        let rig = make_rig(test_config());
        rig.mask.fail_show.store(true, Ordering::SeqCst);
        rig.router.toggle_manual();
        assert_eq!(rig.router.state(), State::Fault);
        assert!(rig.mask.releases() >= 1);
        assert!(rig
            .transitions()
            .contains(&(State::Arming, State::Fault, "fault:E900".to_string())));
    }

    /// 非法迁移不得改状态，并把自己送进 FAULT(E900)。
    #[test]
    fn illegal_transition_faults() {
        let rig = make_rig(test_config());
        let mut events: Vec<RouterEvent> = Vec::new();
        let changed = {
            let mut inner = lock_mutex(&rig.router.inner);
            let changed =
                rig.router
                    .change_state(&mut inner, &mut events, State::Returning, "bogus");
            assert_eq!(inner.state, State::Fault);
            changed
        };
        assert!(!changed);
        rig.router.dispatch(events);
        assert_eq!(
            rig.transitions().last().cloned(),
            Some((State::Idle, State::Fault, "fault:E900".to_string()))
        );
    }

    /// 健康检查连续 3 次失败才降级；恢复后自动重连。
    #[test]
    fn health_degrades_then_reconnects() {
        let rig = make_rig(test_config());
        rig.session.set_health(false, false);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle, "第 1 次失败不降级");
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle, "第 2 次失败不降级");
        rig.tick();
        assert_eq!(rig.router.state(), State::Fault, "第 3 次失败降级");
        assert!(rig
            .transitions()
            .contains(&(State::Idle, State::Fault, "fault:E106".to_string())));

        rig.session.set_health(true, true);
        rig.clock.advance_ms(1010);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle, "退避到点后应重连回 IDLE");
        assert!(rig.session.count(&rig.session.reconnect_calls) >= 1);
        assert!(rig.router.estimator().is_some());
        assert!(rig.router.facts().is_some());
    }

    /// 首连失败 → FAULT，退避到点后 supervisor 自动连上。
    #[test]
    fn initial_connect_failure_then_supervisor_recovers() {
        let rig = make_rig(test_config());
        rig.session.set_facts(None);
        *lock_mutex(&rig.session.start_error) = Some(ErrorCode::DeviceOffline);
        rig.router.start();
        assert_eq!(rig.router.state(), State::Fault);
        assert!(rig.router.estimator().is_none());

        rig.session.set_facts(Some(default_facts()));
        rig.session.start_calls.store(0, Ordering::SeqCst);
        rig.clock.advance_ms(1010);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.router.estimator().is_some());
        rig.router.stop();
    }

    /// 旋转：会话事实变化后重建估计器并复位到中心，同时通知 GUI。
    #[test]
    fn rotation_rebuilds_estimator_and_notifies() {
        let rig = make_rig(test_config());
        let seen: Arc<Mutex<Vec<DeviceFacts>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&seen);
            rig.router.set_on_orientation_changed(move |facts| {
                lock_mutex(&sink).push(facts.clone());
            });
        }
        rig.capture_manually();
        let mut rotated = default_facts();
        rotated.width = 2560;
        rotated.height = 1600;
        rotated.orientation = 1;
        rig.session.set_facts(Some(rotated.clone()));

        rig.tick();
        let estimator = rig.router.estimator().expect("旋转后应有估计器");
        assert_eq!((estimator.width(), estimator.height()), (2560, 1600));
        assert_eq!(estimator.position(), (1279.5, 799.5));
        assert_eq!(
            lock_mutex(&seen).last().cloned(),
            Some(rotated),
            "GUI 应收到朝向通知"
        );
    }

    /// 显示器拓扑变化：refresh 一次并按主屏同侧重新锚定。
    #[test]
    fn desktop_change_refreshes_and_reanchors() {
        let rig = make_rig(test_config());
        rig.tick(); // 先建立 desktop_sig
        let second = MonitorInfo::new(
            2,
            "\\\\.\\DISPLAY2",
            Rect::new(1920, 0, 3840, 1080),
            96,
            true,
        );
        rig.desktop.set_monitors(vec![second]);
        rig.desktop.set_segments(vec![
            EdgeSegment::new(2, Edge::Top, 0, 1920).expect("顶边段合法"),
        ]);

        rig.tick();
        assert_eq!(rig.desktop.refreshes(), 1, "拓扑变化应 refresh 一次");
        let placement = rig.router.placement().expect("应保留摆位");
        assert_eq!(placement.segment.monitor_handle, 2);
        assert_eq!(placement.segment.edge, Edge::Top);
        assert!((placement.offset - 0.5).abs() < 1e-9);
        assert_eq!(placement.align, Align::Proportional);
    }

    /// CAPTURED 期间拓扑变化 → 先交还再校验摆位。
    #[test]
    fn desktop_change_while_captured_returns() {
        let rig = make_rig(test_config());
        rig.tick();
        rig.capture_manually();
        rig.desktop.set_monitors(vec![MonitorInfo::new(
            2,
            "\\\\.\\DISPLAY2",
            Rect::new(0, 0, 1280, 720),
            96,
            true,
        )]);
        rig.desktop.set_segments(vec![
            EdgeSegment::new(2, Edge::Right, 0, 720).expect("右边缘段合法"),
        ]);
        rig.tick();
        assert_eq!(rig.router.state(), State::Returning);
        assert!(rig.transitions().contains(&(
            State::Captured,
            State::Returning,
            "display".to_string()
        )));
    }

    /// resync_pointer：回到平板中心。
    #[test]
    fn resync_pointer_recentres_estimator() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.router.resync_pointer();
        let estimator = rig.router.estimator().expect("应有估计器");
        assert_eq!(estimator.position(), (799.5, 1279.5));
    }

    /// 断开设备：不发送、不重连，状态保持 IDLE。
    #[test]
    fn disconnect_stops_sending_and_reconnecting() {
        let rig = make_rig(test_config());
        rig.router.disconnect_device();
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.router.facts().is_none());
        assert!(!rig.router.connected());
        assert!(rig.session.count(&rig.session.stop_calls) >= 1);

        rig.clear_reports();
        rig.frame();
        assert!(rig.reports().is_empty(), "断开后不得再发送");
        rig.clock.advance(120.0);
        rig.tick();
        assert_eq!(rig.router.state(), State::Idle);
        assert_eq!(
            rig.session.count(&rig.session.start_calls),
            0,
            "用户断开后不得自动重连"
        );
    }

    /// 切换设备：先 FAULT，supervisor 再连上新设备。
    #[test]
    fn connect_device_reuses_an_in_flight_start_for_the_same_serial() {
        let rig = make_rig(test_config());
        *lock_mutex(&rig.router.requested_serial) = Some("S1".to_string());
        {
            let mut inner = rig.router.lock_inner();
            inner.connect_in_progress = true;
        }
        let stop_before = rig.session.count(&rig.session.stop_calls);
        let cancel_before = rig.session.count(&rig.session.cancel_calls);

        rig.router.connect_device(Some("S1"));

        assert_eq!(
            rig.session.count(&rig.session.stop_calls),
            stop_before,
            "同一台设备的 in-flight start 不能被 stop 掉"
        );
        assert_eq!(
            rig.session.count(&rig.session.cancel_calls),
            cancel_before,
            "同一台设备的 in-flight start 不能被 cancel_start 掉"
        );
    }

    #[test]
    fn connect_device_still_switches_when_another_serial_is_in_flight() {
        let rig = make_rig(test_config());
        *lock_mutex(&rig.router.requested_serial) = Some("S1".to_string());
        {
            let mut inner = rig.router.lock_inner();
            inner.connect_in_progress = true;
        }
        let stop_before = rig.session.count(&rig.session.stop_calls);

        rig.router.connect_device(Some("S2"));

        assert!(
            rig.session.count(&rig.session.stop_calls) > stop_before,
            "切换设备必须停掉旧会话"
        );
        assert_eq!(
            lock_mutex(&rig.router.requested_serial).as_deref(),
            Some("S2")
        );
    }

    #[test]
    fn write_failures_before_any_device_facts_do_not_fault() {
        let rig = make_rig(test_config());
        {
            let mut inner = rig.router.lock_inner();
            inner.facts = None;
        }
        for _ in 0..5 {
            rig.router.note_write_failure(&RouterError::new(
                ErrorCode::SocketBroken,
                "socket not ready",
            ));
        }
        assert_ne!(
            rig.router.state(),
            State::Fault,
            "会话未连上时写失败不能把状态打成 FAULT"
        );
    }

    #[test]
    fn connect_device_faults_then_recovers() {
        let rig = make_rig(test_config());
        rig.router.connect_device(Some("serial-2"));
        assert_eq!(rig.router.state(), State::Fault);
        assert!(rig.router.facts().is_none());
        assert_eq!(rig.router.requested_serial().as_deref(), Some("serial-2"));
        assert!(rig.session.count(&rig.session.stop_calls) >= 1);

        rig.tick();
        assert_eq!(rig.router.state(), State::Idle);
        assert!(rig.router.facts().is_some());
        assert!(rig.router.connected());
    }

    /// auto_connect=false：start 视为挂起，不调 session.start。
    #[test]
    fn auto_connect_disabled_waits_for_user() {
        let mut cfg = test_config();
        cfg.device.auto_connect = false;
        let rig = make_rig(cfg);
        rig.router.start();
        assert_eq!(rig.router.state(), State::Idle);
        assert_eq!(rig.session.count(&rig.session.start_calls), 0);
        assert!(rig.router.facts().is_none());
        rig.clock.advance(120.0);
        rig.tick();
        assert_eq!(rig.session.count(&rig.session.start_calls), 0);
        rig.router.stop();
    }

    /// 光标查询不可用时绝不累加 Raw Input 相对计数。
    #[test]
    fn unavailable_cursor_does_not_accumulate() {
        let rig = make_rig(test_config());
        rig.desktop.set_cursor(Some((300, 300)));
        rig.raw(1, 0); // 一次正常采样，建立 pc_cursor
        assert_eq!(rig.inner(|inner| inner.pc_cursor), Some((300, 300)));
        *lock_mutex(&rig.desktop.cursor) = FakeCursor::Missing;
        rig.raw(5, 5);
        assert_eq!(
            rig.inner(|inner| inner.pc_cursor),
            Some((300, 300)),
            "有 getter 但读不到时必须保持绝对位置"
        );
    }

    /// stop 幂等，且不调用 session.stop()。
    #[test]
    fn stop_is_idempotent() {
        let rig = make_rig(test_config());
        rig.capture_manually();
        rig.router.stop();
        let releases = rig.mask.releases();
        assert!(releases >= 1);
        assert_eq!(rig.session.count(&rig.session.stop_calls), 0);
        rig.router.stop();
        assert_eq!(rig.mask.releases(), releases, "重复 stop 不得重复释放");
    }

    /// 剪贴板：接收回调、注入（UTF-8 截断到 4KB）。
    #[test]
    fn clipboard_roundtrip_and_truncation() {
        let rig = make_rig(test_config());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&seen);
            rig.router
                .set_on_clipboard_received(move |text| lock_mutex(&sink).push(text.to_string()));
        }
        rig.router.on_session_clipboard("你好");
        assert_eq!(lock_mutex(&seen).last().cloned(), Some("你好".to_string()));

        assert!(rig.router.inject_clipboard("hello"));
        assert_eq!(
            lock_mutex(&rig.session.clipboards).last().cloned(),
            Some("hello".to_string())
        );
        assert!(!rig.router.inject_clipboard(""), "空文本不注入");

        let long = "a".repeat(5000);
        assert!(rig.router.inject_clipboard(&long));
        let payload = lock_mutex(&rig.session.clipboards)
            .last()
            .cloned()
            .expect("应有注入");
        assert!(payload.len() <= 4096, "剪贴板负载不得超过 4KB");
    }

    /// clipboard_sync=false 时既不接收也不注入。
    #[test]
    fn clipboard_disabled() {
        let mut cfg = test_config();
        cfg.misc.clipboard_sync = false;
        let rig = make_rig(cfg);
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&seen);
            rig.router
                .set_on_clipboard_received(move |text| lock_mutex(&sink).push(text.to_string()));
        }
        rig.router.on_session_clipboard("hello");
        assert!(lock_mutex(&seen).is_empty());
        assert!(!rig.router.inject_clipboard("hello"));
        assert!(lock_mutex(&rig.session.clipboards).is_empty());
    }

    /// set_placement 只改摆位，不影响已接管状态。
    #[test]
    fn set_placement_updates_but_keeps_state() {
        let rig = make_rig(test_config());
        let placement = Placement::new(
            EdgeSegment::new(1, Edge::Right, 100, 900).expect("段合法"),
            0.25,
            Align::Center,
            Some(Edge::Right),
        )
        .expect("摆位合法");
        rig.router.set_placement(placement.clone());
        assert_eq!(rig.router.placement().map(|item| item.segment.span_start), Some(100));
        assert_eq!(rig.router.state(), State::Idle);
        assert!(!rig.router.manual());
    }
}
