//! 低级键盘钩子（对应 `win/keyboard_hook.py`；设计文档 6.2.2 / 8.2）。
//!
//! 结构与 Python 版逐条对齐：
//! - **专用命名线程**安装 `WH_KEYBOARD_LL` 并跑 `GetMessage` 消息循环；钩子必须在有消息泵的线程上
//!   安装，跨线程重装只能 `PostThreadMessage` 回钩子线程自己做。
//! - 回调只做三件事：构造 [`KeyEvent`]、交给 `on_event`、返回 `1` 或 `CallNextHookEx`。
//! - 看门狗每 `HOOK_WATCHDOG_S`（30s）检查一次“系统有输入但回调计数不增长”。
//! - import 本模块不安装任何钩子；只有 [`KeyboardHook::install`] 有副作用。
//!
//! # 设计约束（PITFALLS 2.5）
//!
//! **回调内绝对不能阻塞、不能做 I/O、不能拿锁。** 低级钩子回调里等锁会把输入事件链
//! 卡住——实测表现是鼠标一起卡顿；一旦回调超时，Windows 会**静默摘掉**钩子且不报错。
//! 所以回调路径上只有原子计数 + 一次 `on_event` 调用，重活（遮罩、转发、日志）由
//! `on_event` 背后的路由线程承担。
//!
//! # 分层（对应 Python 的可注入后端）
//!
//! Python 直接用 ctypes 调 Win32，测试只能靠 skipif 跳过。Rust 侧把 Win32 调用收进
//! [`KeyboardHookApi`]：真实实现是 [`Win32KeyboardHookApi`]，测试注入 `tests::FakeApi`，
//! 于是 install / uninstall 的生命周期用例不装系统钩子也能默认跑。
//!
//! # 与 Python 的已知差异
//!
//! 低级钩子回调没有 user data 参数，所以进程内用 [`ACTIVE`] 全局槽位把回调路由到
//! [`HookContext`]。产品只用**一个**钩子实例；同进程装第二个会让回调都落到最后一个
//! 实例上（Python 用绑定方法，天然支持多个）。

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GetLastError, HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG,
    PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::constants::HOOK_WATCHDOG_S;
use crate::tablet::keymap::KeyEvent;

// ---------------------------------------------------------------------------
// 常量（对应 Python 模块级常量）
// ---------------------------------------------------------------------------

/// `HC_ACTION`：回调只处理这个 nCode。
pub const HC_ACTION: i32 = 0;

/// 扫描码扩展位（0xE000），拼进 [`KeyEvent::scan`]。
pub const SCAN_EXTENDED: i32 = 0xE000;

/// `LLKHF_EXTENDED`（WinUser.h；与 windows crate 的 `LLKHF_EXTENDED.0` 同值）。
pub const LLKHF_EXTENDED: u32 = 0x01;

/// 看门狗请求“原地重装”的自定义消息（`WM_APP + 1`）。
pub const WM_APP_REINSTALL: u32 = 0x8000 + 1;

/// 钩子线程名（Python `padlend-kbd-hook`）。
pub const HOOK_THREAD_NAME: &str = "padlend-kbd-hook";

/// 看门狗线程名（Python `padlend-kbd-watchdog`）。
pub const WATCHDOG_THREAD_NAME: &str = "padlend-kbd-watchdog";

/// 等钩子线程就绪的上限（Python `_INSTALL_TIMEOUT_S = 5.0`）。
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(5);

/// 等钩子线程退出的上限（Python `_UNINSTALL_TIMEOUT_S = 3.0`）。
pub const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(3);

/// 看门狗轮询粒度（Python `_WATCHDOG_TICK_S = 0.5`）。
pub const WATCHDOG_TICK: Duration = Duration::from_millis(500);

/// 失败路径上 join 线程的上限（Python `_JOIN_TIMEOUT_S = 1.0`）。
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 低级键盘钩子的错误；对应 Python 的 `PadLendError(ErrorCode.HOOK_FAILED, …)`。
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// 非 Windows 平台（Python 在 sys.platform != "win32" 时抛这个）。
    #[error("低级键盘钩子仅支持 Windows")]
    Unsupported,
    /// 钩子线程 5s 内没有给出结果（Python：键盘钩子线程启动超时）。
    #[error("键盘钩子线程启动超时")]
    InstallTimeout,
    /// `SetWindowsHookExW` 返回 NULL；错误文本前缀与 Python 一致。
    #[error("SetWindowsHookExW 失败: {source}")]
    SetHook {
        /// `SetWindowsHookExW` 的底层错误（Python 侧是 OSError）。
        #[from]
        source: windows::core::Error,
    },
    /// 钩子线程创建失败（Rust 独有：Python 由 threading.Thread 抛 RuntimeError）。
    #[error("键盘钩子线程创建失败: {source}")]
    Spawn {
        /// `thread::Builder::spawn` 的 I/O 错误。
        #[source]
        source: io::Error,
    },
}

impl HookError {
    /// 对应 Python 的错误码（`core.errors.ErrorCode.HOOK_FAILED` = "E200"）。
    pub fn code(&self) -> &'static str {
        "E200"
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑：事件解析 / 健康判定（默认用例直接跑这些）
// ---------------------------------------------------------------------------

/// 是否是低级键盘钩子关心的按键消息。
///
/// 只有这四种消息的 lParam 才指向 `KBDLLHOOKSTRUCT`；集合与 Python 的
/// `w_param in (WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP)` 一致。
pub fn is_key_message(w_param: u32) -> bool {
    w_param == WM_KEYDOWN
        || w_param == WM_KEYUP
        || w_param == WM_SYSKEYDOWN
        || w_param == WM_SYSKEYUP
}

/// 从 `KBDLLHOOKSTRUCT` 的字段构造 [`KeyEvent`]；不是按键消息时返回 `None`。
///
/// 与 Python `_hook_proc` 逐条对齐：
/// - `flags & LLKHF_EXTENDED` 时把 0xE000 或进扫描码（用于区分左右 Ctrl/Alt）；
/// - `is_extended` 由**拼过的**扫描码决定，不是直接取 flags；
/// - `WM_SYSKEYDOWN` 算按下、`WM_SYSKEYUP` 算抬起。
pub fn event_from_raw(w_param: u32, vk: u32, scan: u32, flags: u32, ts: f64) -> Option<KeyEvent> {
    let down = if w_param == WM_KEYDOWN || w_param == WM_SYSKEYDOWN {
        true
    } else if w_param == WM_KEYUP || w_param == WM_SYSKEYUP {
        false
    } else {
        return None;
    };
    let mut scan = scan as i32;
    if flags & LLKHF_EXTENDED != 0 {
        scan |= SCAN_EXTENDED;
    }
    Some(KeyEvent {
        vk: vk as i32,
        scan,
        is_extended: scan & SCAN_EXTENDED != 0,
        down,
        ts,
    })
}

/// 看门狗判定：系统最后输入时间变了（说明有输入），但钩子回调计数没增长。
///
/// 与 Python `_watchdog_loop` 里那个 stalled 表达式逐条对应；任一取值为 `None` 都不算。
/// 注意这只能说明“可能有输入却没进键盘回调”——鼠标独动也会命中，所以 Python 只记
/// debug 日志、不自动重装（每 30s 重装一次会在正常使用中刷屏并抖动）。
pub fn watchdog_stalled(
    last_calls: u64,
    calls: u64,
    last_tick: Option<u32>,
    tick: Option<u32>,
) -> bool {
    match (last_tick, tick) {
        (Some(previous), Some(current)) => current != previous && calls == last_calls,
        _ => false,
    }
}

/// 钩子健康的三条件（Python `healthy` 属性）：已安装 + 启动无错 + 线程存活。
pub fn is_healthy(installed: bool, startup_error: bool, thread_alive: bool) -> bool {
    installed && !startup_error && thread_alive
}

// ---------------------------------------------------------------------------
// 健康 / 计数
// ---------------------------------------------------------------------------

/// 钩子的锁无关统计（Python 的 `_installed/_calls/_callback_errors/_reinstalls`）。
///
/// 回调路径只碰原子量：**不能在这里加锁**（PITFALLS 2.5：锁内 I/O 会阻塞鼠标）。
#[derive(Debug, Default)]
pub struct HookHealth {
    installed: AtomicBool,
    startup_error: AtomicBool,
    thread_alive: AtomicBool,
    calls: AtomicU64,
    callback_errors: AtomicU64,
    reinstalls: AtomicU64,
}

impl HookHealth {
    /// 新建一份全零统计。
    pub fn new() -> Self {
        Self::default()
    }

    /// 还原到 install() 之前的初值（Python 里那几个字段的复位）。
    pub fn reset(&self) {
        self.installed.store(false, Ordering::Release);
        self.startup_error.store(false, Ordering::Release);
        self.thread_alive.store(false, Ordering::Release);
        self.calls.store(0, Ordering::Relaxed);
        self.callback_errors.store(0, Ordering::Relaxed);
        self.reinstalls.store(0, Ordering::Relaxed);
    }

    /// 钩子已安装 + 线程存活 + 启动无错（对应 Python `healthy`）。
    pub fn healthy(&self) -> bool {
        is_healthy(
            self.installed.load(Ordering::Acquire),
            self.startup_error.load(Ordering::Acquire),
            self.thread_alive.load(Ordering::Acquire),
        )
    }

    /// 当前是否挂着钩子。
    pub fn installed(&self) -> bool {
        self.installed.load(Ordering::Acquire)
    }

    /// 消息循环线程是否还在跑。
    pub fn thread_alive(&self) -> bool {
        self.thread_alive.load(Ordering::Acquire)
    }

    /// 启动阶段是否失败过。
    pub fn startup_failed(&self) -> bool {
        self.startup_error.load(Ordering::Acquire)
    }

    /// 回调总次数（看门狗用）。
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// `on_event` 抛异常（Rust 里是 panic）的次数。
    pub fn callback_errors(&self) -> u64 {
        self.callback_errors.load(Ordering::Relaxed)
    }

    /// 看门狗原地重装的次数（诊断用）。
    pub fn reinstalls(&self) -> u64 {
        self.reinstalls.load(Ordering::Relaxed)
    }

    fn record_call(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }

    fn record_callback_error(&self) {
        self.callback_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn set_installed(&self, value: bool) {
        self.installed.store(value, Ordering::Release);
    }

    fn set_thread_alive(&self, value: bool) {
        self.thread_alive.store(value, Ordering::Release);
    }

    fn mark_startup_error(&self) {
        self.startup_error.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// 消息泵抽象（可注入后端）
// ---------------------------------------------------------------------------

/// `GetMessageW` 的三种结果（对应 Python 里对返回值的 0 / -1 / 其他 判断）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageOutcome {
    /// 返回 0：收到 `WM_QUIT`，退出消息循环。
    Quit,
    /// 返回 -1：取消息失败（附 `GetLastError`）。
    Failed(i32),
    /// 一条普通消息。
    Message(MSG),
}

/// 低级键盘钩子的 Win32 调用面。
///
/// 对应 Python 里 `_load_user32() / _load_kernel32()` 拿到的那几个函数；真实实现是
/// [`Win32KeyboardHookApi`]，测试用 `tests::FakeApi` 注入。
pub trait KeyboardHookApi: Send + Sync + 'static {
    /// 建立线程消息队列并返回当前线程 id（`PeekMessageW(PM_NOREMOVE)` + `GetCurrentThreadId`）。
    fn prepare_message_queue(&self) -> u32;
    /// `SetWindowsHookExW` 的 hMod；取不到时返回 `None`（低级钩子允许 NULL）。
    fn module_handle(&self) -> Option<isize>;
    /// 安装钩子并返回 HHOOK 的 `isize` 形态。
    fn set_hook(&self, module: Option<isize>) -> Result<isize, HookError>;
    /// 卸载钩子；失败只记日志（对应 Python 里的 suppress）。
    fn unhook(&self, hook: isize);
    /// `CallNextHookEx`。
    fn call_next(&self, n_code: i32, w_param: usize, l_param: isize) -> isize;
    /// 向钩子线程投递消息；失败返回 `false`。
    fn post_thread_message(&self, thread_id: u32, msg: u32) -> bool;
    /// 取下一条消息（`GetMessageW`）。
    fn next_message(&self) -> MessageOutcome;
    /// `TranslateMessage` + `DispatchMessageW`。
    fn dispatch(&self, msg: &MSG);
    /// 系统最后一次输入的时间戳（`GetLastInputInfo`）；不可用时 `None`。
    fn last_input_tick(&self) -> Option<u32>;
}

/// 真实的 Win32 实现（对应 Python 的 `_load_user32()/_load_kernel32()` 惰性加载）。
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32KeyboardHookApi;

impl KeyboardHookApi for Win32KeyboardHookApi {
    fn prepare_message_queue(&self) -> u32 {
        // 先创建线程消息队列：PostThreadMessage / 看门狗依赖它。
        let mut msg = MSG::default();
        // 返回��只表示“队列里有没有消息”，这里只关心副作用（建立队列）。
        let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
        unsafe { GetCurrentThreadId() }
    }

    fn module_handle(&self) -> Option<isize> {
        // 失败时退回 NULL：低级钩子允许 hMod 为空。
        match unsafe { GetModuleHandleW(None) } {
            Ok(module) => Some(module.0 as isize),
            Err(_) => None,
        }
    }

    fn set_hook(&self, module: Option<isize>) -> Result<isize, HookError> {
        let instance = module.map(|value| HINSTANCE(value as *mut core::ffi::c_void));
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), instance, 0) }
            .map_err(|source| HookError::SetHook { source })?;
        Ok(hook.0 as isize)
    }

    fn unhook(&self, hook: isize) {
        if let Err(err) = unsafe { UnhookWindowsHookEx(HHOOK(hook as *mut core::ffi::c_void)) } {
            eprintln!("[keyboard_hook] UnhookWindowsHookEx 失败：{err}");
        }
    }

    fn call_next(&self, n_code: i32, w_param: usize, l_param: isize) -> isize {
        unsafe { CallNextHookEx(None, n_code, WPARAM(w_param), LPARAM(l_param)) }.0 as isize
    }

    fn post_thread_message(&self, thread_id: u32, msg: u32) -> bool {
        unsafe { PostThreadMessageW(thread_id, msg, WPARAM(0), LPARAM(0)) }.is_ok()
    }

    fn next_message(&self) -> MessageOutcome {
        let mut msg = MSG::default();
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        match ret {
            0 => MessageOutcome::Quit,
            -1 => MessageOutcome::Failed(unsafe { GetLastError() }.0 as i32),
            _ => MessageOutcome::Message(msg),
        }
    }

    fn dispatch(&self, msg: &MSG) {
        unsafe {
            let _ = TranslateMessage(msg);
            let _ = DispatchMessageW(msg);
        }
    }

    fn last_input_tick(&self) -> Option<u32> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            Some(info.dwTime)
        } else {
            None
        }
    }
}
// ---------------------------------------------------------------------------
// 回调上下文（全局槽位）
// ---------------------------------------------------------------------------

/// 回调路由槽位。
///
/// `WH_KEYBOARD_LL` 的回调是自由函数、没有 user data，只能靠进程级静态量找回实例——
/// 这与 Python 用绑定方法不同，因此**同一进程只支持一个钩子实例**（产品也只用 1 个）。
static ACTIVE: AtomicPtr<HookContext> = AtomicPtr::new(ptr::null_mut());

/// 回调需要的全部状态：`on_event` 加上没有锁的计数器。
struct HookContext {
    health: Arc<HookHealth>,
    on_event: Box<dyn Fn(&KeyEvent) -> bool + Send + Sync>,
}

/// 把事件交给 `on_event`；返回 `true` 表示吞键。
///
/// 与 Python 的 try/except 对齐：用户回调 panic 只计数、**绝不**上抛给系统。
fn deliver(ctx: &HookContext, event: &KeyEvent) -> bool {
    match catch_unwind(AssertUnwindSafe(|| (ctx.on_event)(event))) {
        Ok(swallow) => swallow,
        Err(_) => {
            ctx.health.record_callback_error();
            false
        }
    }
}

/// 低级钩子回调：**只构造事件 + 调 `on_event`，禁止任何 I/O 与加锁**（PITFALLS 2.5）。
///
/// # Safety
///
/// 由 `SetWindowsHookExW` 调用；`nCode == HC_ACTION` 且 wParam 是按键消息时，
/// lParam 指向有效的 `KBDLLHOOKSTRUCT`。
unsafe extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let current = ACTIVE.load(Ordering::Acquire);
    if current.is_null() {
        return unsafe { CallNextHookEx(None, n_code, w_param, l_param) };
    }
    // SAFETY: 槽位只在钩子线程存活期内由它写入/清空（见 ThreadCleanup）。
    let ctx = unsafe { &*current };
    ctx.health.record_call();

    if n_code == HC_ACTION && is_key_message(w_param.0 as u32) {
        // SAFETY: 低级键盘钩子在 HC_ACTION + 按键消息时保证 lParam 是 KBDLLHOOKSTRUCT。
        let info = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
        if let Some(event) = event_from_raw(
            w_param.0 as u32,
            info.vkCode,
            info.scanCode,
            info.flags.0,
            ts_now(),
        ) {
            if deliver(ctx, &event) {
                return LRESULT(1);
            }
        }
    }

    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

/// 相对进程启动的单调时间戳（Python 用 `time.perf_counter()`）。
///
/// 只用于延迟统计：绝对值与 Python 不同（两边零点不同），不能跨进程比较。
fn ts_now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

/// 取锁；中毒时取回内层值（非测试代码不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 钩子线程与 uninstall() 共享的句柄槽（Python 的 `_hook/_watchdog/_thread_id`）。
#[derive(Debug, Default)]
struct SharedHook {
    hook: Mutex<Option<isize>>,
    watchdog: Mutex<Option<JoinHandle<()>>>,
    thread_id: AtomicU32,
}

impl SharedHook {
    fn hook(&self) -> Option<isize> {
        *lock(&self.hook)
    }

    fn set_hook(&self, hook: isize) {
        *lock(&self.hook) = Some(hook);
    }

    fn take_hook(&self) -> Option<isize> {
        lock(&self.hook).take()
    }

    fn set_watchdog(&self, handle: JoinHandle<()>) {
        let old = lock(&self.watchdog).replace(handle);
        drop(old);
    }

    fn take_watchdog(&self) -> Option<JoinHandle<()>> {
        lock(&self.watchdog).take()
    }

    fn thread_id(&self) -> u32 {
        self.thread_id.load(Ordering::Acquire)
    }
}

/// 停止信号（Python `threading.Event`）；看门狗按 `WATCHDOG_TICK` 粒度等它。
#[derive(Debug, Default)]
struct StopFlag {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl StopFlag {
    fn set(&self) {
        *lock(&self.flag) = true;
        self.cv.notify_all();
    }

    fn clear(&self) {
        *lock(&self.flag) = false;
    }

    /// 等一个 tick；返回 `true` 表示收到停止信号。
    fn wait_tick(&self, tick: Duration) -> bool {
        let guard = lock(&self.flag);
        let (guard, _timeout) = self
            .cv
            .wait_timeout(guard, tick)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard
    }
}

/// 线程退出清理（对应 Python `_run` 的 finally）。
///
/// 用 Drop 而不是手写收尾，保证 panic 展开时也会卸载钩子、清空回调槽——Python 的
/// finally 在 BaseException 下同样会执行。
struct ThreadCleanup {
    api: Arc<dyn KeyboardHookApi>,
    health: Arc<HookHealth>,
    shared: Arc<SharedHook>,
    ctx: usize,
}

impl Drop for ThreadCleanup {
    fn drop(&mut self) {
        let hook = self.shared.take_hook();
        self.health.set_installed(false);
        self.health.set_thread_alive(false);
        if let Some(hook) = hook {
            self.api.unhook(hook);
        }
        // 只清自己占的槽：多实例时不要踩别人的回调。
        let _ = ACTIVE.compare_exchange(
            self.ctx as *mut HookContext,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// 等线程结束，最多 `timeout`；超时后放弃 join（JoinHandle 被 detach）。
///
/// 对应 Python 的 `thread.join(timeout)` + `is_alive()`。标准库没有带超时的 join，
/// 只能轮询 `is_finished()`；5ms 粒度对退出路径足够细。
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let started = Instant::now();
    while !handle.is_finished() {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
    true
}

// ---------------------------------------------------------------------------
// 钩子
// ---------------------------------------------------------------------------

/// 键盘低级钩子（设计文档 6.2.2）。
///
/// `on_event` **必须非阻塞**，返回 `true` 表示吞键（例如 Win 键拦截）。
pub struct KeyboardHook {
    api: Arc<dyn KeyboardHookApi>,
    ctx: Box<HookContext>,
    health: Arc<HookHealth>,
    shared: Arc<SharedHook>,
    stop: Arc<StopFlag>,
    thread: Option<JoinHandle<()>>,
}

impl KeyboardHook {
    /// 用真实 Win32 后端创建钩子（不安装；install() 才有副作用）。
    pub fn new<F>(on_event: F) -> Self
    where
        F: Fn(&KeyEvent) -> bool + Send + Sync + 'static,
    {
        Self::with_api(Arc::new(Win32KeyboardHookApi), on_event)
    }

    /// 注入自定义后端（测试替身用）。
    pub fn with_api<F>(api: Arc<dyn KeyboardHookApi>, on_event: F) -> Self
    where
        F: Fn(&KeyEvent) -> bool + Send + Sync + 'static,
    {
        let health = Arc::new(HookHealth::new());
        Self {
            api,
            ctx: Box::new(HookContext {
                health: Arc::clone(&health),
                on_event: Box::new(on_event),
            }),
            health,
            shared: Arc::new(SharedHook::default()),
            stop: Arc::new(StopFlag::default()),
            thread: None,
        }
    }

    /// 启动钩子线程并安装钩子；失败返回 `E200`。
    ///
    /// 幂等：线程还活着时直接返回 `Ok(())`（Python 里是 `if alive: return`）。
    pub fn install(&mut self) -> Result<(), HookError> {
        if self.thread_alive() {
            return Ok(());
        }

        self.stop.clear();
        self.health.reset();
        *lock(&self.shared.hook) = None;

        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), HookError>>();
        let api = Arc::clone(&self.api);
        let health = Arc::clone(&self.health);
        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop);
        let ctx = self.ctx.as_ref() as *const HookContext as usize;

        let handle = std::thread::Builder::new()
            .name(HOOK_THREAD_NAME.to_string())
            .spawn(move || hook_thread_main(api, ctx, health, shared, stop, ready_tx))
            .map_err(|source| HookError::Spawn { source })?;
        self.thread = Some(handle);

        match ready_rx.recv_timeout(INSTALL_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                // Python 在 startup_error 分支里先 join 再抛，保持同样的收尾顺序。
                if let Some(handle) = self.thread.take() {
                    let _ = join_with_timeout(handle, JOIN_TIMEOUT);
                }
                Err(err)
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                Err(HookError::InstallTimeout)
            }
        }
    }

    /// 退出消息循环并卸载钩子；幂等（Python 里第二次调用是空操作）。
    pub fn uninstall(&mut self) {
        self.stop.set();
        let Some(handle) = self.thread.take() else {
            return;
        };

        if !handle.is_finished() {
            let thread_id = self.shared.thread_id();
            if thread_id != 0 && !self.api.post_thread_message(thread_id, WM_QUIT) {
                eprintln!("[keyboard_hook] PostThreadMessageW(WM_QUIT) 失败");
            }
        }
        if !join_with_timeout(handle, UNINSTALL_TIMEOUT) {
            eprintln!("[keyboard_hook] uninstall_timeout");
        }

        if let Some(watchdog) = self.shared.take_watchdog() {
            let _ = join_with_timeout(watchdog, JOIN_TIMEOUT);
        }

        let hook = self.shared.take_hook();
        self.health.set_installed(false);
        // 线程若异常退出，钩子可能仍挂在句柄上；尽力从当前线程卸载（与 Python 同）。
        if let Some(hook) = hook {
            if !self.health.thread_alive() {
                self.api.unhook(hook);
            }
        }
    }

    /// 钩子已安装且消息循环线程仍存活。
    pub fn healthy(&self) -> bool {
        self.health.healthy()
    }

    /// 看门狗重装次数（诊断用）。
    pub fn reinstalls(&self) -> u64 {
        self.health.reinstalls()
    }

    /// `on_event` 抛异常（panic）的次数（诊断用）。
    pub fn callback_errors(&self) -> u64 {
        self.health.callback_errors()
    }

    /// 回调总次数（诊断用）。
    pub fn calls(&self) -> u64 {
        self.health.calls()
    }

    /// 请求钩子线程原地重装；返回是否成功投递。
    ///
    /// 跨线程不能直接 `SetWindowsHookExW`，只能发消息让钩子线程自己重装
    /// （Python `_request_reinstall` 同）。默认看门狗**不**调用它，只记日志。
    pub fn request_reinstall(&self) -> bool {
        let thread_id = self.shared.thread_id();
        if thread_id == 0 {
            return false;
        }
        let ok = self.api.post_thread_message(thread_id, WM_APP_REINSTALL);
        if !ok {
            eprintln!("[keyboard_hook] PostThreadMessageW(WM_APP_REINSTALL) 失败");
        }
        ok
    }

    /// 消息循环线程是否还活着（Python `thread.is_alive()`）。
    fn thread_alive(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        self.uninstall();
    }
}

/// 钩子线程主体：建队列 → 装钩子 → 起看门狗 → 泵消息。
fn hook_thread_main(
    api: Arc<dyn KeyboardHookApi>,
    ctx: usize,
    health: Arc<HookHealth>,
    shared: Arc<SharedHook>,
    stop: Arc<StopFlag>,
    ready: mpsc::Sender<Result<(), HookError>>,
) {
    let _cleanup = ThreadCleanup {
        api: Arc::clone(&api),
        health: Arc::clone(&health),
        shared: Arc::clone(&shared),
        ctx,
    };

    let thread_id = api.prepare_message_queue();
    shared.thread_id.store(thread_id, Ordering::Release);
    health.set_thread_alive(true);

    // 回调槽必须在 SetWindowsHookExW 之前写好：钩子一挂上就可能立刻回调。
    ACTIVE.store(ctx as *mut HookContext, Ordering::Release);

    let hook = match api.set_hook(api.module_handle()) {
        Ok(hook) => hook,
        Err(err) => {
            health.mark_startup_error();
            let _ = ready.send(Err(err));
            return;
        }
    };
    shared.set_hook(hook);
    health.set_installed(true);
    let _ = ready.send(Ok(()));

    // 看门狗：与 Python 一样在钩子线程里启动，失败只记日志。
    let watchdog = std::thread::Builder::new()
        .name(WATCHDOG_THREAD_NAME.to_string())
        .spawn({
            let api = Arc::clone(&api);
            let health = Arc::clone(&health);
            let stop = Arc::clone(&stop);
            move || watchdog_loop(api.as_ref(), &health, &stop)
        });
    match watchdog {
        Ok(handle) => shared.set_watchdog(handle),
        Err(err) => eprintln!("[keyboard_hook] 看门狗线程创建失败：{err}"),
    }

    message_loop(api.as_ref(), shared.as_ref(), health.as_ref());
}

/// 看门狗主循环（Python `_watchdog_loop`）。
///
/// 只做判定与日志：判定卡住也**不**自动重装（Python 注释说明：每 30s 重装一次会在
/// 正常使用中刷屏并抖动，真正失效由钩子线程/健康标记与用户按键验证）。
fn watchdog_loop(api: &dyn KeyboardHookApi, health: &HookHealth, stop: &StopFlag) {
    // Python: interval = max(1.0, float(C.HOOK_WATCHDOG_S))
    let interval = Duration::from_secs_f64(HOOK_WATCHDOG_S.max(1.0));
    let mut last_calls = health.calls();
    let mut last_tick = api.last_input_tick();
    let mut deadline = Instant::now() + interval;

    while !stop.wait_tick(WATCHDOG_TICK) {
        let now = Instant::now();
        if now < deadline {
            continue;
        }
        deadline = now + interval;

        let calls = health.calls();
        let tick = api.last_input_tick();
        if watchdog_stalled(last_calls, calls, last_tick, tick) {
            eprintln!(
                "[keyboard_hook] kbd.watchdog.idle_or_mouse_only calls={calls} last_input_tick={tick:?}"
            );
        }
        last_calls = calls;
        last_tick = tick;
    }
}

/// 消息循环（Python `_message_loop`）：`WM_QUIT` 退出，`WM_APP_REINSTALL` 原地重装。
fn message_loop(api: &dyn KeyboardHookApi, shared: &SharedHook, health: &HookHealth) {
    loop {
        match api.next_message() {
            MessageOutcome::Quit => return,
            MessageOutcome::Failed(code) => {
                eprintln!("[keyboard_hook] kbd.hook.get_message_failed err={code}");
                return;
            }
            MessageOutcome::Message(msg) => {
                if msg.message == WM_APP_REINSTALL {
                    reinstall_hook(api, shared, health);
                    continue;
                }
                api.dispatch(&msg);
            }
        }
    }
}

/// 在钩子线程原地重装（跨线程只能发消息触发，见 [`KeyboardHook::request_reinstall`]）。
fn reinstall_hook(api: &dyn KeyboardHookApi, shared: &SharedHook, health: &HookHealth) {
    if let Some(old) = shared.hook() {
        api.unhook(old);
    }
    match api.set_hook(api.module_handle()) {
        Ok(hook) => {
            shared.set_hook(hook);
            health.set_installed(true);
            health.reinstalls.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[keyboard_hook] kbd.hook.reinstalled count={}",
                health.reinstalls()
            );
        }
        Err(err) => {
            shared.take_hook();
            health.set_installed(false);
            eprintln!("[keyboard_hook] kbd.hook.reinstall_failed err={err}");
        }
    }
}
// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicIsize;

    /// 轮询等待条件成立（替身后端的异步回调没有别的同步手段）。
    fn wait_until<F>(mut predicate: F, timeout: Duration) -> bool
    where
        F: FnMut() -> bool,
    {
        let started = Instant::now();
        while !predicate() {
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    // ---- 纯逻辑：事件解析（Python _hook_proc 里的构造规则）-------------------

    #[test]
    fn decodes_keydown_without_extended_bit() {
        let event = event_from_raw(WM_KEYDOWN, 0x41, 0x1E, 0, 1.5).expect("WM_KEYDOWN 必须产出事件");
        assert_eq!(event.vk, 0x41);
        assert_eq!(event.scan, 0x1E, "非扩展键的扫描码要原样保留");
        assert!(!event.is_extended);
        assert!(event.down);
        assert_eq!(event.ts, 1.5);
    }

    #[test]
    fn extended_flag_ors_into_scan_code() {
        let event =
            event_from_raw(WM_KEYDOWN, 0x11, 0x1D, LLKHF_EXTENDED, 0.0).expect("扩展键也必须产出事件");
        assert_eq!(event.scan, SCAN_EXTENDED | 0x1D, "扩展位要或进扫描码");
        assert!(event.is_extended, "is_extended 取拼过的扫描码，而不是 flags");
    }

    #[test]
    fn syskeyup_is_a_release() {
        let event = event_from_raw(WM_SYSKEYUP, 0x12, 0x38, 0, 0.0).expect("SYSKEYUP 要产出抬起事件");
        assert!(!event.down, "WM_SYSKEYUP 是抬起");
        assert_eq!(event.scan, 0x38);
        assert!(!event.is_extended);
    }

    #[test]
    fn non_key_messages_are_ignored() {
        assert!(
            event_from_raw(0x0200, 0x41, 0x1E, 0, 0.0).is_none(),
            "WM_MOUSEMOVE 不是键盘消息"
        );
        assert!(
            event_from_raw(0x0312, 0x41, 0x1E, 0, 0.0).is_none(),
            "WM_HOTKEY 不是键盘消息"
        );
        assert!(
            event_from_raw(WM_QUIT, 0x41, 0x1E, 0, 0.0).is_none(),
            "WM_QUIT 不是键盘消息"
        );
    }

    #[test]
    fn is_key_message_matches_python_tuple() {
        for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
            assert!(is_key_message(message), "0x{message:04X} 应当被处理");
        }
        for message in [0u32, 0x0200, 0x0312, WM_QUIT] {
            assert!(!is_key_message(message), "0x{message:04X} 不该被处理");
        }
    }

    // ---- 纯逻辑：看门狗 / 健康 ----------------------------------------------

    #[test]
    fn watchdog_requires_new_input_and_frozen_callbacks() {
        assert!(
            watchdog_stalled(10, 10, Some(100), Some(200)),
            "系统有新输入但回调计数没涨 = 可疑"
        );
        assert!(
            !watchdog_stalled(10, 11, Some(100), Some(200)),
            "回调有增长说明钩子活着"
        );
        assert!(
            !watchdog_stalled(10, 10, Some(200), Some(200)),
            "系统没有新输入，不判定"
        );
        assert!(
            !watchdog_stalled(10, 10, None, Some(200)),
            "取不到上次输入时间就不判定"
        );
        assert!(
            !watchdog_stalled(10, 10, Some(100), None),
            "取不到本次输入时间就不判定"
        );
    }

    #[test]
    fn health_needs_install_live_thread_and_no_startup_error() {
        assert!(is_healthy(true, false, true));
        assert!(!is_healthy(false, false, true), "没装不算健康");
        assert!(!is_healthy(true, true, true), "启动失败不算健康");
        assert!(!is_healthy(true, false, false), "线程没了不算健康");
    }

    #[test]
    fn hook_health_starts_unhealthy_and_resets_counters() {
        let health = HookHealth::new();
        assert!(!health.healthy());
        health.set_installed(true);
        health.set_thread_alive(true);
        health.record_call();
        health.record_callback_error();
        assert!(health.healthy());
        assert_eq!(health.calls(), 1);
        assert_eq!(health.callback_errors(), 1);

        health.reset();
        assert!(!health.healthy(), "reset 之后必须回到未安装状态");
        assert_eq!(health.calls(), 0);
        assert_eq!(health.callback_errors(), 0);
    }

    #[test]
    fn panicking_callback_is_counted_and_never_swallows() {
        let panicking = HookContext {
            health: Arc::new(HookHealth::new()),
            on_event: Box::new(|_event| panic!("回调炸了")),
        };
        assert!(
            !deliver(&panicking, &KeyEvent::key_down(0x41)),
            "回调 panic 的返回值必须是「不吞键」"
        );
        assert_eq!(panicking.health.callback_errors(), 1);

        let swallowing = HookContext {
            health: Arc::new(HookHealth::new()),
            on_event: Box::new(|_event| true),
        };
        assert!(deliver(&swallowing, &KeyEvent::key_down(0x41)));
        assert_eq!(swallowing.health.callback_errors(), 0);
    }

    // ---- 替身驱动：生命周期（对应 pytest 里那两个用例）-----------------------

    /// 替身队列里的消息。
    ///
    /// 不能直接存 [`MessageOutcome`]：它里面的 [`MSG`] 含 `HWND(*mut c_void)`，
    /// 不是 `Send`，塞进 `Mutex` 后整个替身就不满足 trait 的 `Sync` 约束。
    #[derive(Debug, Clone, Copy)]
    enum FakeMessage {
        Quit,
        Message(u32),
    }

    /// 测试替身：脚本化消息泵 + 计数，完全不碰 Win32。
    ///
    /// `next_message` 在脚本队列空时阻塞（与真实 `GetMessageW` 一样），
    /// `post_thread_message(WM_QUIT)` 往队列里塞 `Quit` 把它唤醒。
    #[derive(Debug, Default)]
    struct FakeApi {
        messages: Mutex<VecDeque<FakeMessage>>,
        cv: Condvar,
        thread_id: AtomicU32,
        next_handle: AtomicIsize,
        set_hook_fail: Mutex<Option<i32>>,
        set_hook_calls: AtomicU32,
        unhook_calls: AtomicU32,
        dispatched: AtomicU32,
        last_tick: Mutex<Option<u32>>,
    }

    impl FakeApi {
        fn set_hook_calls(&self) -> u32 {
            self.set_hook_calls.load(Ordering::SeqCst)
        }

        fn unhook_calls(&self) -> u32 {
            self.unhook_calls.load(Ordering::SeqCst)
        }

        fn dispatched(&self) -> u32 {
            self.dispatched.load(Ordering::SeqCst)
        }

        fn fail_set_hook(&self, code: i32) {
            *lock(&self.set_hook_fail) = Some(code);
        }

        fn push(&self, outcome: FakeMessage) {
            lock(&self.messages).push_back(outcome);
            self.cv.notify_all();
        }
    }

    impl KeyboardHookApi for FakeApi {
        fn prepare_message_queue(&self) -> u32 {
            self.thread_id.store(0x1234, Ordering::SeqCst);
            0x1234
        }

        fn module_handle(&self) -> Option<isize> {
            None
        }

        fn set_hook(&self, _module: Option<isize>) -> Result<isize, HookError> {
            self.set_hook_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock(&self.set_hook_fail) {
                return Err(HookError::SetHook {
                    source: windows::core::Error::from_hresult(windows::core::HRESULT(code)),
                });
            }
            Ok(self.next_handle.fetch_add(1, Ordering::SeqCst) + 1)
        }

        fn unhook(&self, _hook: isize) {
            self.unhook_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn call_next(&self, _n_code: i32, _w_param: usize, _l_param: isize) -> isize {
            0
        }

        fn post_thread_message(&self, _thread_id: u32, msg: u32) -> bool {
            if msg == WM_QUIT {
                self.push(FakeMessage::Quit);
            } else {
                self.push(FakeMessage::Message(msg));
            }
            true
        }

        fn next_message(&self) -> MessageOutcome {
            let mut queue = lock(&self.messages);
            loop {
                if let Some(outcome) = queue.pop_front() {
                    return match outcome {
                        FakeMessage::Quit => MessageOutcome::Quit,
                        FakeMessage::Message(message) => MessageOutcome::Message(MSG {
                            message,
                            ..Default::default()
                        }),
                    };
                }
                queue = self
                    .cv
                    .wait(queue)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }

        fn dispatch(&self, _msg: &MSG) {
            self.dispatched.fetch_add(1, Ordering::SeqCst);
        }

        fn last_input_tick(&self) -> Option<u32> {
            *lock(&self.last_tick)
        }
    }

    fn fake_hook(api: &Arc<FakeApi>) -> KeyboardHook {
        let concrete: Arc<FakeApi> = Arc::clone(api);
        let backend: Arc<dyn KeyboardHookApi> = concrete;
        KeyboardHook::with_api(backend, |_event| false)
    }

    #[test]
    fn install_then_uninstall_reports_health() {
        // 对应 Python test_install_and_uninstall（换成替身，默认跑）。
        let api = Arc::new(FakeApi::default());
        let mut hook = fake_hook(&api);

        hook.install().expect("替身后端不该失败");
        assert!(hook.healthy(), "装好后必须 healthy");

        hook.uninstall();
        assert!(!hook.healthy(), "卸载后必须 not healthy");
        assert_eq!(api.set_hook_calls(), 1, "只该 SetWindowsHookExW 一次");
        assert_eq!(api.unhook_calls(), 1, "卸载要真正 UnhookWindowsHookEx");
    }

    #[test]
    fn uninstall_is_idempotent() {
        // 对应 Python test_uninstall_is_idempotent。
        let api = Arc::new(FakeApi::default());
        let mut hook = fake_hook(&api);

        hook.uninstall();
        hook.uninstall();

        assert!(!hook.healthy());
        assert_eq!(api.set_hook_calls(), 0, "没装过就不该 SetWindowsHookExW");
        assert_eq!(api.unhook_calls(), 0);
    }

    #[test]
    fn install_twice_installs_only_once() {
        let api = Arc::new(FakeApi::default());
        let mut hook = fake_hook(&api);

        hook.install().expect("第一次安装");
        hook.install().expect("重复安装必须是空操作");
        assert_eq!(api.set_hook_calls(), 1);

        hook.uninstall();
    }

    #[test]
    fn set_hook_failure_matches_python_message() {
        let api = Arc::new(FakeApi::default());
        api.fail_set_hook(0x8007_0057u32 as i32); // ERROR_INVALID_PARAMETER
        let mut hook = fake_hook(&api);

        let err = hook.install().expect_err("SetWindowsHookExW 失败必须上抛");
        assert!(
            err.to_string().starts_with("SetWindowsHookExW 失败: "),
            "错误文案前缀要与 Python 一致，实际：{err}"
        );
        assert_eq!(err.code(), "E200");
        assert!(!hook.healthy());
    }

    #[test]
    fn request_reinstall_reinstalls_on_hook_thread() {
        let api = Arc::new(FakeApi::default());
        let mut hook = fake_hook(&api);
        hook.install().expect("安装");

        api.push(FakeMessage::Message(0x0200));
        assert!(
            wait_until(|| api.dispatched() == 1, Duration::from_secs(1)),
            "普通消息要交给 TranslateMessage/DispatchMessageW"
        );

        assert!(hook.request_reinstall(), "重装请求应当投递成功");
        assert!(
            wait_until(|| hook.reinstalls() == 1, Duration::from_secs(1)),
            "重装请求没有被钩子线程执行"
        );
        assert_eq!(api.set_hook_calls(), 2);
        assert_eq!(api.unhook_calls(), 1, "重装前必须先卸掉旧钩子");

        hook.uninstall();
    }

    #[test]
    fn drop_uninstalls() {
        let api = Arc::new(FakeApi::default());
        {
            let mut hook = fake_hook(&api);
            hook.install().expect("安装");
            assert!(hook.healthy());
        }
        assert_eq!(api.unhook_calls(), 1, "Drop 必须卸载钩子");
    }

    // ---- 真实系统钩子：默认不跑 ----------------------------------------------

    #[test]
    #[ignore = "会真实安装 WH_KEYBOARD_LL 系统钩子：需要交互式桌面，手工执行 cargo test -- --ignored"]
    fn real_install_and_uninstall() {
        // 对应 Python test_install_and_uninstall。
        let mut hook = KeyboardHook::new(|_event| false);
        hook.install().expect("安装低级键盘钩子失败");
        assert!(hook.healthy());
        hook.uninstall();
        assert!(!hook.healthy());
    }

    #[test]
    #[ignore = "会真实安装 WH_KEYBOARD_LL 系统钩子：需要交互式桌面，手工执行 cargo test -- --ignored"]
    fn real_uninstall_is_idempotent() {
        // 对应 Python test_uninstall_is_idempotent。
        let mut hook = KeyboardHook::new(|_event| false);
        hook.uninstall();
        hook.uninstall();
        assert!(!hook.healthy());
    }
}
