//! 系统级保命热键（对应 `win/hotkeys.py`；设计文档 6.2.3 / 6.7.5）。
//!
//! `RegisterHotKey` 注册 `Ctrl+Alt+S`（切换）与 `Ctrl+Alt+Q`（急停），专用命名线程跑消息循环，
//! `WM_HOTKEY` 按 id 分发到回调。**不依赖键盘钩子**：钩子被系统静默摘掉时保命键仍然可用。
//!
//! `import` 本模块不注册任何热键；只有 [`HotkeyManager::start`] 有副作用。
//!
//! # 设计约束（PITFALLS 2.1 / 2.5）
//!
//! - 热键回调在**热键线程**上执行，必须非阻塞（PITFALLS 2.5：回调里做 I/O 会连带卡住输入链）。
//!   回调只做“置标志 / 投递到通道”，真正的工作交给路由线程。
//! - 注册失败**不阻断主程序**（Python 只 `log.error`）：被别的程序占用的热键很常见。
//!
//! # 分层（对应 Python 的可注入后端）
//!
//! Python 把 Win32 调用藏在 `_load_user32()/_load_kernel32()` 后面，测试只能 `skipif`。
//! Rust 侧把它们收进 [`HotkeyApi`]：真实实现是 [`Win32HotkeyApi`]，测试注入 `tests::FakeApi`，
//! 于是 start/stop 生命周期与 `WM_HOTKEY` 分发都能默认跑。

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    PM_NOREMOVE, WM_QUIT,
};

// 消息泵结果类型与键盘钩子共用一份定义（两处逻辑完全同构，没必要各写一份）。
use crate::win::keyboard_hook::MessageOutcome;

// ---------------------------------------------------------------------------
// 常量（对应 Python 模块级常量）
// ---------------------------------------------------------------------------

/// `MOD_ALT`。
pub const MOD_ALT: u32 = 0x0001;

/// `MOD_CONTROL`。
pub const MOD_CONTROL: u32 = 0x0002;

/// `_CTRL_ALT = MOD_CONTROL | MOD_ALT`：两条热键都带这个修饰位。
pub const CTRL_ALT: u32 = MOD_CONTROL | MOD_ALT;

/// `WM_HOTKEY`。
pub const WM_HOTKEY: u32 = 0x0312;

/// 切换热键 id（`Ctrl+Alt+S`）。
pub const HOTKEY_TOGGLE_ID: i32 = 0x1001;

/// 急停热键 id（`Ctrl+Alt+Q`）。
pub const HOTKEY_PANIC_ID: i32 = 0x1002;

/// `VK_Q`。
pub const VK_Q: u32 = 0x51;

/// `VK_S`。
pub const VK_S: u32 = 0x53;

/// 热键线程名（Python `padlend-hotkeys`）。
pub const HOTKEY_THREAD_NAME: &str = "padlend-hotkeys";

/// 等热键线程就绪的上限（Python `_START_TIMEOUT_S = 2.0`）。
pub const START_TIMEOUT: Duration = Duration::from_secs(2);

/// 等热键线程退出的上限（Python `_STOP_TIMEOUT_S = 3.0`）。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// 失败路径上 join 线程的上限（Python `_JOIN_TIMEOUT_S = 1.0`）。
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 热键管理器的错误；对应 Python 的 `PadLendError(ErrorCode.UNEXPECTED, …)`。
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// 非 Windows 平台（Python 在 `sys.platform != "win32"` 时抛）。
    #[error("RegisterHotKey 仅支持 Windows")]
    Unsupported,
    /// 热键线程创建失败（Rust 独有：Python 由 threading.Thread 抛 RuntimeError）。
    #[error("热键线程创建失败: {source}")]
    Spawn {
        /// `thread::Builder::spawn` 的 I/O 错误。
        #[source]
        source: io::Error,
    },
}

impl HotkeyError {
    /// 对应 Python 的错误码（`core.errors.ErrorCode.UNEXPECTED` = "E900"）。
    pub fn code(&self) -> &'static str {
        "E900"
    }
}

// ---------------------------------------------------------------------------
// 分发（纯逻辑，默认用例直接跑）
// ---------------------------------------------------------------------------

/// 热键 id → 回调的分发表（Python `_callbacks + _dispatch`）。
///
/// 回调在热键线程上执行：**调用方必须保证它非阻塞**（PITFALLS 2.5）。
/// 回调 panic 只计数并记日志，绝不让消息循环退出（Python 用 `except Exception` 兜住）。
pub struct HotkeyRouter {
    on_toggle: Box<dyn Fn() + Send + Sync>,
    on_panic: Box<dyn Fn() + Send + Sync>,
    toggle_calls: AtomicU64,
    panic_calls: AtomicU64,
    unknown_ids: AtomicU64,
    callback_errors: AtomicU64,
}

impl HotkeyRouter {
    /// 建立分发表（对应 Python `_callbacks = {TOGGLE: on_toggle, PANIC: on_panic}`）。
    pub fn new<F, G>(on_toggle: F, on_panic: G) -> Self
    where
        F: Fn() + Send + Sync + 'static,
        G: Fn() + Send + Sync + 'static,
    {
        Self {
            on_toggle: Box::new(on_toggle),
            on_panic: Box::new(on_panic),
            toggle_calls: AtomicU64::new(0),
            panic_calls: AtomicU64::new(0),
            unknown_ids: AtomicU64::new(0),
            callback_errors: AtomicU64::new(0),
        }
    }

    /// 按 id 调回调；未知 id 只记日志（Python `hotkeys.unknown_id`）。
    pub fn dispatch(&self, hotkey_id: i32) {
        let callback: &(dyn Fn() + Send + Sync) = match hotkey_id {
            HOTKEY_TOGGLE_ID => self.on_toggle.as_ref(),
            HOTKEY_PANIC_ID => self.on_panic.as_ref(),
            _ => {
                self.unknown_ids.fetch_add(1, Ordering::Relaxed);
                eprintln!("[hotkeys] hotkeys.unknown_id id=0x{hotkey_id:04X}");
                return;
            }
        };

        match catch_unwind(AssertUnwindSafe(callback)) {
            Ok(()) => {
                if hotkey_id == HOTKEY_TOGGLE_ID {
                    self.toggle_calls.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.panic_calls.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                self.callback_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[hotkeys] hotkeys.callback_failed id=0x{hotkey_id:04X}");
            }
        }
    }

    /// 切换热键成功回调的次数。
    pub fn toggle_calls(&self) -> u64 {
        self.toggle_calls.load(Ordering::Relaxed)
    }

    /// 急停热键成功回调的次数。
    pub fn panic_calls(&self) -> u64 {
        self.panic_calls.load(Ordering::Relaxed)
    }

    /// 未知 id 的 `WM_HOTKEY` 次数。
    pub fn unknown_ids(&self) -> u64 {
        self.unknown_ids.load(Ordering::Relaxed)
    }

    /// 回调 panic 的次数。
    pub fn callback_errors(&self) -> u64 {
        self.callback_errors.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for HotkeyRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeyRouter")
            .field("toggle_calls", &self.toggle_calls())
            .field("panic_calls", &self.panic_calls())
            .field("unknown_ids", &self.unknown_ids())
            .field("callback_errors", &self.callback_errors())
            .finish()
    }
}

/// 取锁；中毒时取回内层值（非测试代码不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 按顺序注册 `Ctrl+Alt+S` 与 `Ctrl+Alt+Q`；单个失败只记日志并继续（Python `_register_all`）。
fn register_all(api: &dyn HotkeyApi) -> Vec<i32> {
    let entries = [(HOTKEY_TOGGLE_ID, VK_S), (HOTKEY_PANIC_ID, VK_Q)];
    let mut registered = Vec::new();
    for (hotkey_id, vk) in entries {
        if api.register(hotkey_id, CTRL_ALT, vk) {
            registered.push(hotkey_id);
        } else {
            // 失败细节（LastError）由后端实现记日志，这里只标出 id/vk。
            eprintln!("[hotkeys] hotkeys.register_failed id=0x{hotkey_id:04X} vk=0x{vk:02X}");
        }
    }
    eprintln!(
        "[hotkeys] hotkeys.registered ids={:?}",
        registered.iter().map(|id| format!("0x{id:04X}")).collect::<Vec<_>>()
    );
    registered
}

// ---------------------------------------------------------------------------
// Win32 调用面（可注入后端）
// ---------------------------------------------------------------------------

/// 热键管理器的 Win32 调用面。
///
/// 对应 Python `_load_user32() / _load_kernel32()` 拿到的那几个函数；真实实现是
/// [`Win32HotkeyApi`]，测试用 `tests::FakeApi` 注入。
pub trait HotkeyApi: Send + Sync + 'static {
    /// 建立线程消息队列并返回当前线程 id（Python 在 `_run` 里做同样两件事）。
    fn prepare_message_queue(&self) -> u32;
    /// `RegisterHotKey(None, id, modifiers, vk)`；失败返回 `false`。
    fn register(&self, hotkey_id: i32, modifiers: u32, vk: u32) -> bool;
    /// `UnregisterHotKey(None, id)`；失败只记日志。
    fn unregister(&self, hotkey_id: i32);
    /// 向热键线程投递消息；失败返回 `false`。
    fn post_thread_message(&self, thread_id: u32, msg: u32) -> bool;
    /// 取下一条消息（`GetMessageW`）。
    fn next_message(&self) -> MessageOutcome;
    /// `TranslateMessage` + `DispatchMessageW`。
    fn dispatch(&self, msg: &MSG);
}

/// 真实的 Win32 实现（对应 Python 的 `_load_user32()/_load_kernel32()` 惰性加载）。
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32HotkeyApi;

impl HotkeyApi for Win32HotkeyApi {
    fn prepare_message_queue(&self) -> u32 {
        let mut msg = MSG::default();
        // 只关心副作用：给本线程建立消息队列（PostThreadMessage 依赖它）。
        let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
        unsafe { GetCurrentThreadId() }
    }

    fn register(&self, hotkey_id: i32, modifiers: u32, vk: u32) -> bool {
        match unsafe { RegisterHotKey(None, hotkey_id, HOT_KEY_MODIFIERS(modifiers), vk) } {
            Ok(()) => true,
            Err(err) => {
                eprintln!(
                    "[hotkeys] hotkeys.register_failed id=0x{hotkey_id:04X} vk=0x{vk:02X} err={err}"
                );
                false
            }
        }
    }

    fn unregister(&self, hotkey_id: i32) {
        if let Err(err) = unsafe { UnregisterHotKey(None, hotkey_id) } {
            eprintln!("[hotkeys] hotkeys.unregister_failed id=0x{hotkey_id:04X} err={err}");
        }
    }

    fn post_thread_message(&self, thread_id: u32, msg: u32) -> bool {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        unsafe { PostThreadMessageW(thread_id, msg, WPARAM(0), LPARAM(0)) }.is_ok()
    }

    fn next_message(&self) -> MessageOutcome {
        let mut msg = MSG::default();
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        match ret {
            0 => MessageOutcome::Quit,
            -1 => MessageOutcome::Failed(unsafe { windows::Win32::Foundation::GetLastError() }.0 as i32),
            _ => MessageOutcome::Message(msg),
        }
    }

    fn dispatch(&self, msg: &MSG) {
        unsafe {
            let _ = TranslateMessage(msg);
            let _ = DispatchMessageW(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// 管理器
// ---------------------------------------------------------------------------

/// 退出时的注销守卫（对应 Python `_run` 的 finally 里 `_unregister_all`）。
///
/// 用 Drop 保证消息循环 panic 退出时也会注销热键，避免把 `Ctrl+Alt+S/Q` 永久占住。
struct HotkeyCleanup {
    api: Arc<dyn HotkeyApi>,
    registered: Arc<Mutex<Vec<i32>>>,
}

impl Drop for HotkeyCleanup {
    fn drop(&mut self) {
        let ids: Vec<i32> = std::mem::take(&mut *lock(&self.registered));
        for hotkey_id in ids {
            self.api.unregister(hotkey_id);
        }
    }
}

/// 保命热键管理器（6.2.3 / 6.7.5）。
///
/// 回调在热键线程执行，**必须非阻塞**。对应 Python `HotkeyManager`。
pub struct HotkeyManager {
    api: Arc<dyn HotkeyApi>,
    router: Arc<HotkeyRouter>,
    thread: Option<JoinHandle<()>>,
    thread_id: Arc<AtomicU32>,
    registered: Arc<Mutex<Vec<i32>>>,
    stop: Arc<AtomicBool>,
}

impl HotkeyManager {
    /// 用真实 Win32 后端创建管理器（不注册任何热键；start() 才有副作用）。
    pub fn new<F, G>(on_toggle: F, on_panic: G) -> Self
    where
        F: Fn() + Send + Sync + 'static,
        G: Fn() + Send + Sync + 'static,
    {
        Self::with_api(Arc::new(Win32HotkeyApi), on_toggle, on_panic)
    }

    /// 注入自定义后端（测试替身用）。
    pub fn with_api<F, G>(api: Arc<dyn HotkeyApi>, on_toggle: F, on_panic: G) -> Self
    where
        F: Fn() + Send + Sync + 'static,
        G: Fn() + Send + Sync + 'static,
    {
        Self {
            api,
            router: Arc::new(HotkeyRouter::new(on_toggle, on_panic)),
            thread: None,
            thread_id: Arc::new(AtomicU32::new(0)),
            registered: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 启动热键线程并尽力注册；注册失败只记日志，不阻断主程序（Python `start`）。
    ///
    /// 幂等：线程还活着时直接返回 `Ok(())`。
    pub fn start(&mut self) -> Result<(), HotkeyError> {
        if !cfg!(windows) {
            return Err(HotkeyError::Unsupported);
        }
        if self.active() {
            return Ok(());
        }

        self.stop.store(false, Ordering::Release);
        *lock(&self.registered) = Vec::new();
        self.thread_id.store(0, Ordering::Release);

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let api = Arc::clone(&self.api);
        let router = Arc::clone(&self.router);
        let thread_id = Arc::clone(&self.thread_id);
        let registered = Arc::clone(&self.registered);

        let handle = std::thread::Builder::new()
            .name(HOTKEY_THREAD_NAME.to_string())
            .spawn(move || hotkey_thread_main(api, router, thread_id, registered, ready_tx))
            .map_err(|source| HotkeyError::Spawn { source })?;
        self.thread = Some(handle);

        // Python: ready.wait(2s) 超时只 log.warning；startup_error 只 log.error。
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => eprintln!("[hotkeys] hotkeys.start_timeout"),
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[hotkeys] hotkeys.start_failed err=热键线程在就绪前退出")
            }
        }
        Ok(())
    }

    /// 退出消息循环并注销热键；幂等（Python `stop`）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.thread.take() else {
            return;
        };

        if !handle.is_finished() {
            let thread_id = self.thread_id.load(Ordering::Acquire);
            if thread_id != 0 && !self.api.post_thread_message(thread_id, WM_QUIT) {
                eprintln!("[hotkeys] hotkeys.post_quit_failed");
            }
        }
        if !join_with_timeout(handle, STOP_TIMEOUT) {
            eprintln!("[hotkeys] hotkeys.stop_timeout");
        }
    }

    /// 热键线程仍存活（Python `active`）。
    pub fn active(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// 当前成功注册的热键 id（Python `registered`；诊断/测试用）。
    pub fn registered(&self) -> Vec<i32> {
        lock(&self.registered).clone()
    }

    /// 是否已收到停止请求。
    ///
    /// 与 Python 一致：这个标志在消息循环里**没有人读**（Python 的 `_stop` 同样只 set/clear），
    /// 保留只为语义对齐与外部诊断。
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// 分发表（测试与诊断用；生产代码通过 `WM_HOTKEY` 触发）。
    pub fn router(&self) -> &HotkeyRouter {
        self.router.as_ref()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 热键线程主体（Python `_run`）：建队列 → 注册 → 就绪 → 消息循环；退出时必注销。
fn hotkey_thread_main(
    api: Arc<dyn HotkeyApi>,
    router: Arc<HotkeyRouter>,
    thread_id: Arc<AtomicU32>,
    registered: Arc<Mutex<Vec<i32>>>,
    ready: mpsc::Sender<()>,
) {
    let _cleanup = HotkeyCleanup {
        api: Arc::clone(&api),
        registered: Arc::clone(&registered),
    };

    let current = api.prepare_message_queue();
    thread_id.store(current, Ordering::Release);

    let ids = register_all(api.as_ref());
    *lock(&registered) = ids;
    let _ = ready.send(());

    message_loop(api.as_ref(), router.as_ref());
}

/// 消息循环（Python `_message_loop`）：`WM_QUIT` 退出，`WM_HOTKEY` 分发，其余走默认派发。
fn message_loop(api: &dyn HotkeyApi, router: &HotkeyRouter) {
    loop {
        match api.next_message() {
            MessageOutcome::Quit => return,
            MessageOutcome::Failed(code) => {
                eprintln!("[hotkeys] hotkeys.get_message_failed err={code}");
                return;
            }
            MessageOutcome::Message(msg) => {
                if msg.message == WM_HOTKEY {
                    router.dispatch(msg.wParam.0 as i32);
                    continue;
                }
                api.dispatch(&msg);
            }
        }
    }
}

/// 等线程结束，最多 [`STOP_TIMEOUT`]；超时后放弃 join（JoinHandle 被 detach）。
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
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Condvar;

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

    // ---- 纯逻辑：分发（Python _dispatch）------------------------------------

    #[test]
    fn dispatch_routes_toggle_and_panic() {
        let toggle = Arc::new(AtomicU64::new(0));
        let panic = Arc::new(AtomicU64::new(0));
        let router = HotkeyRouter::new(
            {
                let toggle = Arc::clone(&toggle);
                move || {
                    toggle.fetch_add(1, Ordering::SeqCst);
                }
            },
            {
                let panic = Arc::clone(&panic);
                move || {
                    panic.fetch_add(1, Ordering::SeqCst);
                }
            },
        );

        router.dispatch(HOTKEY_TOGGLE_ID);
        router.dispatch(HOTKEY_TOGGLE_ID);
        router.dispatch(HOTKEY_PANIC_ID);

        assert_eq!(router.toggle_calls(), 2);
        assert_eq!(router.panic_calls(), 1);
        assert_eq!(toggle.load(Ordering::SeqCst), 2);
        assert_eq!(panic.load(Ordering::SeqCst), 1);
        assert_eq!(router.unknown_ids(), 0);
        assert_eq!(router.callback_errors(), 0);
    }

    #[test]
    fn unknown_hotkey_id_is_ignored() {
        // 对应 Python hotkeys.unknown_id 分支。
        let router = HotkeyRouter::new(|| {}, || {});
        router.dispatch(0x9999);

        assert_eq!(router.toggle_calls(), 0);
        assert_eq!(router.panic_calls(), 0);
        assert_eq!(router.unknown_ids(), 1);
    }

    #[test]
    fn panicking_callback_is_counted_and_does_not_propagate() {
        // 对应 Python hotkeys.callback_failed：回调异常只记日志，消息循环继续。
        let router = HotkeyRouter::new(|| panic!("急停回调炸了"), || {});
        router.dispatch(HOTKEY_TOGGLE_ID);

        assert_eq!(router.callback_errors(), 1);
        assert_eq!(router.toggle_calls(), 0, "panic 的回调不该算成功");

        // 之后仍然能正常分发别的热键。
        router.dispatch(HOTKEY_PANIC_ID);
        assert_eq!(router.panic_calls(), 1);
    }

    #[test]
    fn callback_table_uses_python_ids() {
        assert_eq!(HOTKEY_TOGGLE_ID, 0x1001);
        assert_eq!(HOTKEY_PANIC_ID, 0x1002);
        assert_eq!(CTRL_ALT, MOD_CONTROL | MOD_ALT);
        assert_eq!(WM_HOTKEY, 0x0312);
        assert_eq!(VK_S, 0x53);
        assert_eq!(VK_Q, 0x51);
    }

    // ---- 替身驱动：注册 / 生命周期 -----------------------------------------

    /// 替身队列里的消息。
    ///
    /// 不能直接存 [`MessageOutcome`]：里面的 [`MSG`] 含 `HWND(*mut c_void)`，不是 `Send`，
    /// 会让替身不满足 trait 的 `Sync` 约束。
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum FakeMessage {
        Quit,
        Hotkey(i32),
        Other(u32),
    }

    /// 测试替身：脚本化消息泵 + 注册记录，完全不碰 Win32。
    #[derive(Debug, Default)]
    struct FakeApi {
        messages: Mutex<VecDeque<FakeMessage>>,
        cv: Condvar,
        thread_id: AtomicU32,
        fail_ids: Mutex<Vec<i32>>,
        register_calls: AtomicU32,
        unregister_calls: AtomicU32,
        registered_ids: Mutex<Vec<i32>>,
        unregistered_ids: Mutex<Vec<i32>>,
        dispatched: AtomicU32,
    }

    impl FakeApi {
        fn fail(&self, hotkey_id: i32) {
            lock(&self.fail_ids).push(hotkey_id);
        }

        fn push(&self, message: FakeMessage) {
            lock(&self.messages).push_back(message);
            self.cv.notify_all();
        }

        fn register_calls(&self) -> u32 {
            self.register_calls.load(Ordering::SeqCst)
        }

        fn unregister_calls(&self) -> u32 {
            self.unregister_calls.load(Ordering::SeqCst)
        }

        fn registered_ids(&self) -> Vec<i32> {
            lock(&self.registered_ids).clone()
        }

        fn unregistered_ids(&self) -> Vec<i32> {
            lock(&self.unregistered_ids).clone()
        }

        fn dispatched(&self) -> u32 {
            self.dispatched.load(Ordering::SeqCst)
        }
    }

    impl HotkeyApi for FakeApi {
        fn prepare_message_queue(&self) -> u32 {
            self.thread_id.store(0x7777, Ordering::SeqCst);
            0x7777
        }

        fn register(&self, hotkey_id: i32, _modifiers: u32, _vk: u32) -> bool {
            self.register_calls.fetch_add(1, Ordering::SeqCst);
            if lock(&self.fail_ids).contains(&hotkey_id) {
                return false;
            }
            lock(&self.registered_ids).push(hotkey_id);
            true
        }

        fn unregister(&self, hotkey_id: i32) {
            self.unregister_calls.fetch_add(1, Ordering::SeqCst);
            lock(&self.unregistered_ids).push(hotkey_id);
        }

        fn post_thread_message(&self, _thread_id: u32, msg: u32) -> bool {
            if msg == WM_QUIT {
                self.push(FakeMessage::Quit);
            } else {
                self.push(FakeMessage::Other(msg));
            }
            true
        }

        fn next_message(&self) -> MessageOutcome {
            let mut queue = lock(&self.messages);
            loop {
                if let Some(message) = queue.pop_front() {
                    return match message {
                        FakeMessage::Quit => MessageOutcome::Quit,
                        FakeMessage::Hotkey(hotkey_id) => MessageOutcome::Message(MSG {
                            message: WM_HOTKEY,
                            wParam: windows::Win32::Foundation::WPARAM(hotkey_id as usize),
                            ..Default::default()
                        }),
                        FakeMessage::Other(message) => MessageOutcome::Message(MSG {
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
    }

    fn fake_manager(api: &Arc<FakeApi>) -> HotkeyManager {
        let concrete: Arc<FakeApi> = Arc::clone(api);
        let backend: Arc<dyn HotkeyApi> = concrete;
        HotkeyManager::with_api(backend, || {}, || {})
    }

    #[test]
    fn register_all_keeps_going_after_failure() {
        // 对应 Python _register_all：单个失败只 log.error，继续注册下一条。
        let api = Arc::new(FakeApi::default());
        api.fail(HOTKEY_PANIC_ID);
        let backend: Arc<dyn HotkeyApi> = Arc::clone(&api) as Arc<dyn HotkeyApi>;

        let registered = register_all(backend.as_ref());

        assert_eq!(registered, vec![HOTKEY_TOGGLE_ID], "急停键失败不影响切换键");
        assert_eq!(api.register_calls(), 2, "两条热键都必须尝试注册");
        assert_eq!(api.registered_ids(), vec![HOTKEY_TOGGLE_ID], "失败的 id 不该进成功列表");
    }

    #[test]
    fn start_then_stop_registers_and_unregisters() {
        let api = Arc::new(FakeApi::default());
        let mut manager = fake_manager(&api);

        manager.start().expect("替身后端不该失败");
        assert!(manager.active(), "start 之后线程必须存活");
        assert_eq!(manager.registered(), vec![HOTKEY_TOGGLE_ID, HOTKEY_PANIC_ID]);

        manager.stop();
        assert!(!manager.active(), "stop 之后线程必须结束");
        assert!(manager.registered().is_empty(), "退出时必须清空 registered");
        assert_eq!(api.unregister_calls(), 2, "退出时必须注销全部热键");
        assert_eq!(
            api.unregistered_ids(),
            vec![HOTKEY_TOGGLE_ID, HOTKEY_PANIC_ID],
            "注销顺序与注册顺序一致"
        );
    }

    #[test]
    fn stop_is_idempotent() {
        let api = Arc::new(FakeApi::default());
        let mut manager = fake_manager(&api);

        manager.stop();
        manager.stop();

        assert!(!manager.active());
        assert_eq!(api.register_calls(), 0, "没启动就不该注册");
        assert_eq!(api.unregister_calls(), 0);
    }

    #[test]
    fn start_twice_registers_once() {
        let api = Arc::new(FakeApi::default());
        let mut manager = fake_manager(&api);

        manager.start().expect("第一次启动");
        manager.start().expect("重复启动必须是空操作");
        assert_eq!(api.register_calls(), 2, "只有第一次启动注册了两条热键");

        manager.stop();
    }

    #[test]
    fn wm_hotkey_message_reaches_callback() {
        let api = Arc::new(FakeApi::default());
        let toggle = Arc::new(AtomicU64::new(0));
        {
            let counter = Arc::clone(&toggle);
            let concrete: Arc<FakeApi> = Arc::clone(&api);
            let backend: Arc<dyn HotkeyApi> = concrete;
            let mut manager = HotkeyManager::with_api(
                backend,
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                },
                || {},
            );
            manager.start().expect("启动");

            api.push(FakeMessage::Hotkey(HOTKEY_TOGGLE_ID));
            assert!(
                wait_until(|| toggle.load(Ordering::SeqCst) == 1, Duration::from_secs(1)),
                "WM_HOTKEY 没有分发到切换回调"
            );

            api.push(FakeMessage::Hotkey(0x9999));
            assert!(
                wait_until(|| manager.router().unknown_ids() == 1, Duration::from_secs(1)),
                "未知 id 必须被计数"
            );

            api.push(FakeMessage::Other(0x0200));
            assert!(
                wait_until(|| api.dispatched() == 1, Duration::from_secs(1)),
                "非热键消息要交给 TranslateMessage/DispatchMessageW"
            );

            manager.stop();
        }
        assert_eq!(api.register_calls(), 2);
    }

    #[test]
    fn drop_stops_the_thread() {
        let api = Arc::new(FakeApi::default());
        {
            let mut manager = fake_manager(&api);
            manager.start().expect("启动");
            assert!(manager.active());
        }
        assert_eq!(api.unregister_calls(), 2, "Drop 必须注销热键");
    }

    // ---- 真实 Win32：只碰线程队列，不建窗口（默认跑）------------------------

    #[test]
    fn real_prepare_message_queue_returns_thread_id() {
        // 对应 Python tests/test_hotkeys.py：GetCurrentThreadId 来自 kernel32 且非 0。
        // 只建本线程的消息队列，不建窗口、不动光标，锁屏下也安全。
        let thread_id = Win32HotkeyApi.prepare_message_queue();
        assert!(thread_id > 0, "GetCurrentThreadId 必须返回非 0 值");
    }

    // ---- 真实热键注册：默认不跑 ---------------------------------------------

    #[test]
    #[ignore = "会真实 RegisterHotKey 抢占 Ctrl+Alt+S/Q 系统热键：需要交互式桌面，手工执行 cargo test -- --ignored"]
    fn real_start_and_stop_registers_hotkeys() {
        let mut manager = HotkeyManager::new(|| {}, || {});
        manager.start().expect("启动热键线程失败");
        assert!(manager.active());
        assert_eq!(
            manager.registered(),
            vec![HOTKEY_TOGGLE_ID, HOTKEY_PANIC_ID],
            "锁屏/被占用时可能只注册成功一个，此时手工放行 Ctrl+Alt+S/Q"
        );
        manager.stop();
        assert!(!manager.active());
        assert!(manager.registered().is_empty());
    }
}
