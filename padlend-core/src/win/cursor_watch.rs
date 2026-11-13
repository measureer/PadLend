//! Raw Input 停摆时的光标轮询兜底。
//!
//! Windows 在某些焦点/桌面切换后会停止向 `RIDEV_INPUTSINK` 窗口投递
//! `WM_INPUT`（日志中表现为最后一次按键事件后彻底静默，但 `GetCursorPos`
//! 仍在变化）。这里以低频率轮询绝对光标位置，仅在 Raw Input 超过 `stale_s` 没有
//! 事件时合成 `RawDelta`，**注入回 rawinput 线程**交由 Router 处理；Raw Input 恢复后
//! 立即让位，避免重复计数。
//!
//! 注入必须走 rawinput 线程：接管时创建的遮罩窗口归属该线程，只有该线程有消息泵。
//! 若在轮询线程直接调用 Router，遮罩窗口会变成无响应窗口，点击后整个应用会挂起
//! （PITFALLS 3.1 / 6.6）。所以 [`RawInputLike::inject`] 的实现必须把事件投递到
//! rawinput 线程，本模块的轮询线程只负责判定。
//!
//! 分层与 Python 一致：Python 用 `CursorWatch(router, rawinput, *, get_cursor=..., get_buttons=...)`
//! 注入后端，Rust 侧对应三个 trait（[`RouterLike`] / [`RawInputLike`] /
//! [`CursorProbe`]）+ 真实实现 [`Win32CursorProbe`]，测试用替身驱动。

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::logic::state::State;

/// 默认轮询间隔 20ms（对应 Python `interval_s=0.02`）。
pub const DEFAULT_INTERVAL_S: f64 = 0.02;
/// Raw Input 超过该时长没有事件才算"停摆"（对应 Python `stale_s=0.25`）。
pub const DEFAULT_STALE_S: f64 = 0.25;
/// 兜底日志的限流窗口（Python 里硬编码为 2.0 秒，避免 20ms 一条刷屏）。
pub const FALLBACK_LOG_INTERVAL_S: f64 = 2.0;
/// `stop()` 等待轮询线程退出的上限（对应 Python `join(timeout=1.5)`）。
pub const STOP_TIMEOUT_S: f64 = 1.5;

/// `VK_LBUTTON`。
pub const VK_LBUTTON: i32 = 0x01;
/// `VK_RBUTTON`。
pub const VK_RBUTTON: i32 = 0x02;
/// `VK_MBUTTON`。
pub const VK_MBUTTON: i32 = 0x04;
/// `VK_XBUTTON1`。
pub const VK_XBUTTON1: i32 = 0x05;
/// `VK_XBUTTON2`。
pub const VK_XBUTTON2: i32 = 0x06;

/// 虚拟键 → HID 按钮位（顺序与 Python `_BUTTON_MAP` 完全一致）。
pub const BUTTON_MAP: [(i32, i32); 5] = [
    (VK_LBUTTON, 0x01),
    (VK_RBUTTON, 0x02),
    (VK_MBUTTON, 0x04),
    (VK_XBUTTON1, 0x08),
    (VK_XBUTTON2, 0x10),
];

/// 合成的原始输入事件（`core/events.py::RawDelta` 的镜像）。
///
/// **接线点**：`core::events` 模块移植出真正的 `RawDelta` 之后，本模块应改成
/// `use crate::core::events::RawDelta;` 并删除本定义。字段名/顺序/默认值已与 Python
/// 逐项对齐，所以接线时只需换 import（`buttons` 用 `i32`、`ts` 用秒为单位的 `f64`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawDelta {
    /// 相对位移 X。
    pub dx: i32,
    /// 相对位移 Y。
    pub dy: i32,
    /// HID 按钮位（见 [`BUTTON_MAP`]）。
    pub buttons: i32,
    /// 滚轮增量。
    pub wheel: i32,
    /// 是否绝对坐标。
    pub absolute: bool,
    /// 事件时间戳（`time.time()` 秒）。
    pub ts: f64,
}

impl RawDelta {
    /// 只有位移的事件，其余字段取 Python 默认值（`buttons=0, wheel=0, absolute=False, ts=0.0`）。
    pub fn movement(dx: i32, dy: i32) -> Self {
        Self {
            dx,
            dy,
            buttons: 0,
            wheel: 0,
            absolute: false,
            ts: 0.0,
        }
    }
}

impl Default for RawDelta {
    fn default() -> Self {
        Self::movement(0, 0)
    }
}

/// 光标轮询兜底的错误。
///
/// 轮询路径本身与 Python 一样"永不致命"：单次判定失败只是返回 `false`；本枚举用于
/// [`CursorWatch::try_start`] 以及后端实现上报无法降级的原因。
#[derive(Debug, thiserror::Error)]
pub enum CursorWatchError {
    /// 无法创建轮询线程。
    #[error("创建光标轮询线程失败：{source}")]
    Spawn {
        /// 底层错误。
        #[source]
        source: std::io::Error,
    },
    /// Win32 / 后端调用失败。
    #[error("{message}")]
    Backend {
        /// 人类可读的失败原因。
        message: String,
    },
}

/// 路由状态源（对应 Python `router.state`）。
pub trait RouterLike: Send + Sync {
    /// 当前路由状态；读失败按"未接管"处理（Python 里 `except Exception: captured = False`）。
    fn state(&self) -> Result<State, CursorWatchError>;
}

/// Raw Input 后端（对应 Python `rawinput.last_event_age` / `rawinput.inject`）。
///
/// **`inject` 必须把事件投递到 rawinput 线程**：遮罩窗口归属该线程，只有它有消息泵；
/// 在调用方线程直接分发会让遮罩窗口无响应，点击后整个应用挂起（PITFALLS 6.6）。
pub trait RawInputLike: Send + Sync {
    /// 距上一个真实 Raw Input 事件过去了多少秒（无事件时返回 `f64::INFINITY`）。
    fn last_event_age(&self) -> Result<f64, CursorWatchError>;
    /// 把合成事件投递给 Router；返回是否被接受。
    fn inject(&self, delta: RawDelta) -> Result<bool, CursorWatchError>;
}

/// 绝对光标 + 系统按键状态（对应 Python 的 `get_cursor` / `get_buttons` 两个可注入函数）。
pub trait CursorProbe: Send + Sync {
    /// 当前光标屏幕坐标；读不到返回 `Ok(None)`（Python `try_get_cursor_pos`）。
    fn cursor_pos(&self) -> Result<Option<(i32, i32)>, CursorWatchError>;
    /// 当前按键位（Python `_query_buttons`）。
    fn buttons(&self) -> Result<i32, CursorWatchError>;
}

/// 真实 Win32 实现：`GetCursorPos` + `GetAsyncKeyState`。
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32CursorProbe;

impl CursorProbe for Win32CursorProbe {
    fn cursor_pos(&self) -> Result<Option<(i32, i32)>, CursorWatchError> {
        let mut point = POINT::default();
        // SAFETY: point 是有效的可写 POINT，GetCursorPos 只写它。
        match unsafe { GetCursorPos(&mut point) } {
            // 与 Python 的 try_get_cursor_pos 一致：失败不当异常，直接当"读不到"。
            Ok(()) => Ok(Some((point.x, point.y))),
            Err(_) => Ok(None),
        }
    }

    fn buttons(&self) -> Result<i32, CursorWatchError> {
        Ok(query_buttons())
    }
}

/// 读系统按键状态（对应 Python `_query_buttons`：任何失败都退化成 0）。
///
/// 用 `GetAsyncKeyState` 而不是 Raw Input，是为了在 Raw Input 停摆时仍能补齐松键，
/// 否则出口会一直被禁用。
pub fn query_buttons() -> i32 {
    let mut value = 0;
    for (vk, bit) in BUTTON_MAP {
        // SAFETY: GetAsyncKeyState 只读系统状态，没有前置条件。
        let state = unsafe { GetAsyncKeyState(vk) };
        if (state as u16) & 0x8000 != 0 {
            value |= bit;
        }
    }
    value
}

/// 轮询兜底的参数（Python 的两个关键字参数）。
#[derive(Debug, Clone, Copy)]
pub struct CursorWatchConfig {
    /// 轮询间隔（秒）。
    pub interval_s: f64,
    /// Raw Input 静默多久才算停摆（秒）。
    pub stale_s: f64,
}

impl Default for CursorWatchConfig {
    fn default() -> Self {
        Self {
            interval_s: DEFAULT_INTERVAL_S,
            stale_s: DEFAULT_STALE_S,
        }
    }
}

/// 在 Raw Input 静默时轮询绝对光标并合成相对位移。
pub struct CursorWatch {
    shared: Arc<Shared>,
    interval_s: f64,
    stop: Arc<Gate>,
    thread: Mutex<Option<WatchThread>>,
}

/// 轮询线程与它的存活标志（Python `_thread`）。
struct WatchThread {
    handle: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

/// 轮询判定需要的全部状态；轮询线程与 `poll_once` 共享同一份。
struct Shared {
    router: Arc<dyn RouterLike>,
    rawinput: Arc<dyn RawInputLike>,
    probe: Arc<dyn CursorProbe>,
    stale_s: f64,
    poll: Mutex<PollState>,
}

/// Python 的 `_last` / `_last_buttons` / `_last_log` 三个字段。
#[derive(Debug, Default)]
struct PollState {
    last: Option<(i32, i32)>,
    last_buttons: Option<i32>,
    last_log: f64,
}

impl CursorWatch {
    /// 用真实 Win32 后端构造（生产路径）。
    pub fn new(router: Arc<dyn RouterLike>, rawinput: Arc<dyn RawInputLike>) -> Self {
        Self::with_probe(
            router,
            rawinput,
            Arc::new(Win32CursorProbe),
            CursorWatchConfig::default(),
        )
    }

    /// 注入后端与参数构造（测试替身、以及上层自定义光标来源时用）。
    pub fn with_probe(
        router: Arc<dyn RouterLike>,
        rawinput: Arc<dyn RawInputLike>,
        probe: Arc<dyn CursorProbe>,
        config: CursorWatchConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                router,
                rawinput,
                probe,
                stale_s: config.stale_s,
                poll: Mutex::new(PollState::default()),
            }),
            interval_s: config.interval_s,
            stop: Arc::new(Gate::new()),
            thread: Mutex::new(None),
        }
    }

    /// 轮询间隔（秒）。
    pub fn interval_s(&self) -> f64 {
        self.interval_s
    }

    /// Raw Input 静默阈值（秒）。
    pub fn stale_s(&self) -> f64 {
        self.shared.stale_s
    }

    /// 轮询线程是否在跑（对应 Python `running`）。
    pub fn running(&self) -> bool {
        let slot = lock(&self.thread);
        match slot.as_ref() {
            Some(watch) => watch.alive.load(Ordering::SeqCst),
            None => false,
        }
    }

    /// 启动轮询线程；已在跑则直接返回（Python `start()`）。
    ///
    /// 线程创建失败只记日志（Python 会抛异常，但调用方并不处理），需要错误时用
    /// [`CursorWatch::try_start`]。
    pub fn start(&self) {
        if let Err(err) = self.try_start() {
            eprintln!("[cursorwatch] 无法启动光标轮询：{err}");
        }
    }

    /// 启动轮询线程并返回失败原因。
    pub fn try_start(&self) -> Result<(), CursorWatchError> {
        let mut slot = lock(&self.thread);
        if let Some(watch) = slot.as_ref() {
            if watch.alive.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        // Python：stop.clear()、last/last_buttons 归零，再起线程。
        self.stop.clear();
        {
            let mut poll = lock(&self.shared.poll);
            poll.last = None;
            poll.last_buttons = None;
        }
        let alive = Arc::new(AtomicBool::new(true));
        let alive_in_thread = Arc::clone(&alive);
        let stop = Arc::clone(&self.stop);
        let shared = Arc::clone(&self.shared);
        let interval = Duration::from_secs_f64(self.interval_s.max(0.001));
        let spawned = thread::Builder::new()
            .name("padlend-cursorwatch".to_string())
            .spawn(move || {
                while !stop.wait(interval) {
                    // 轮询线程永不致命：单次判定 panic 只丢这一次采样。
                    let now = monotonic_now();
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| poll_once(&shared, now)));
                }
                alive_in_thread.store(false, Ordering::SeqCst);
            });
        match spawned {
            Ok(handle) => {
                *slot = Some(WatchThread {
                    handle: Some(handle),
                    alive,
                });
                Ok(())
            }
            Err(err) => {
                self.stop.clear();
                Err(CursorWatchError::Spawn { source: err })
            }
        }
    }

    /// 停止轮询线程（Python `stop()`）。
    ///
    /// 有界等待：`stop` 最多等 [`STOP_TIMEOUT_S`] 秒，超时就 detach（Python 里
    /// `join(timeout=1.5)` 之后同样把 `_thread` 置 None）。停止位一旦置上，残留线程
    /// 最多再空转一个 interval 就退出，不会再注入事件。
    pub fn stop(&self) {
        self.stop.set();
        let taken = {
            let mut slot = lock(&self.thread);
            slot.take()
        };
        if let Some(mut watch) = taken {
            let deadline = Instant::now() + Duration::from_secs_f64(STOP_TIMEOUT_S);
            while watch.alive.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            // 仍然活着就 detach（drop JoinHandle 不会阻塞）。
            let _ = watch.handle.take();
        }
        let mut poll = lock(&self.shared.poll);
        poll.last = None;
        poll.last_buttons = None;
    }

    /// 执行一次判定；返回是否合成了输入事件（对应 Python `poll_once(now=None)`）。
    pub fn poll_once(&self) -> bool {
        self.poll_once_at(monotonic_now())
    }

    /// 用给定时间戳执行一次判定（测试用；`now` 是单调钟秒数）。
    pub fn poll_once_at(&self, now: f64) -> bool {
        poll_once(&self.shared, now)
    }
}

impl Drop for CursorWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 一次判定（对应 Python `CursorWatch.poll_once`）。
fn poll_once(shared: &Shared, now: f64) -> bool {
    // 路由状态读失败按"未接管"处理（Python `except Exception: captured = False`）。
    let captured = matches!(shared.router.state(), Ok(State::Captured));
    // 读不到事件年龄就当成"非常久没有事件"，让兜底继续工作。
    let age = shared
        .rawinput
        .last_event_age()
        .unwrap_or(f64::INFINITY);
    if age < shared.stale_s {
        // Raw Input 还活着：立刻让位，并把基线清空，避免恢复后补一段假位移。
        let mut poll = lock(&shared.poll);
        poll.last = None;
        poll.last_buttons = None;
        return false;
    }

    // CAPTURED 时 PC 光标被 ClipCursor 钉住，不能从绝对坐标伪造位移；但系统按键
    // 状态仍可用于补齐 Raw Input 漏掉的松键，避免出口被永久禁用。
    if captured {
        lock(&shared.poll).last = None;
        let Ok(buttons) = shared.probe.buttons() else {
            return false;
        };
        {
            let mut poll = lock(&shared.poll);
            if poll.last_buttons == Some(buttons) {
                return false;
            }
            poll.last_buttons = Some(buttons);
        }
        // 这条日志是"平板上按钮被按了一下"的关键旁证：它说明按下来自
        // GetAsyncKeyState（Raw Input 已经停摆），而不是真实的 WM_INPUT。
        eprintln!("[cursorwatch] cursorwatch.btn 0x{buttons:02x} age={age:.2}s");
        return inject(
            shared,
            RawDelta {
                dx: 0,
                dy: 0,
                buttons,
                wheel: 0,
                absolute: false,
                ts: wall_now(),
            },
        );
    }

    let Some(position) = shared.probe.cursor_pos().unwrap_or(None) else {
        return false;
    };
    let previous = lock(&shared.poll).last;
    let Some(previous) = previous else {
        lock(&shared.poll).last = Some(position);
        return false;
    };
    let dx = position.0 - previous.0;
    let dy = position.1 - previous.1;
    lock(&shared.poll).last = Some(position);
    if dx == 0 && dy == 0 {
        return false;
    }
    {
        let mut poll = lock(&shared.poll);
        if now - poll.last_log >= FALLBACK_LOG_INTERVAL_S {
            poll.last_log = now;
            eprintln!("[cursorwatch] cursorwatch.fallback age={age:.2}s move=({dx},{dy}) raw=stalled");
        }
    }
    // 按键读不到就按"没有按键"处理（Python `except Exception: buttons = 0`）。
    let buttons = shared.probe.buttons().unwrap_or(0);
    lock(&shared.poll).last_buttons = Some(buttons);
    inject(
        shared,
        RawDelta {
            dx,
            dy,
            buttons,
            wheel: 0,
            absolute: false,
            ts: wall_now(),
        },
    )
}

/// 投递给 rawinput 线程（Python `_inject`）：注入失败只记日志，不抛。
fn inject(shared: &Shared, delta: RawDelta) -> bool {
    match shared.rawinput.inject(delta) {
        Ok(accepted) => accepted,
        Err(err) => {
            eprintln!("[cursorwatch] cursorwatch.router_failed: {err}");
            false
        }
    }
}

/// 可复用的置位/等待开关（对应 Python `threading.Event`）。
struct Gate {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            flag: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn set(&self) {
        *lock(&self.flag) = true;
        self.cv.notify_all();
    }

    fn clear(&self) {
        *lock(&self.flag) = false;
        self.cv.notify_all();
    }

    /// 等 `timeout` 或直到置位；返回是否已置位（Python `Event.wait`）。
    fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.flag);
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |flag| !*flag)
            .unwrap_or_else(|err| err.into_inner());
        *guard
    }
}

/// 取锁并忽略 poison（轮询线程里的 panic 不该传染给调用方）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

/// 单调钟秒数（对应 Python `time.monotonic()`）。
fn monotonic_now() -> f64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// 墙钟秒数（对应 Python `time.time()`）。
fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// 单测（逐条对应 tests/test_cursor_watch.py 的 3 个用例，全部默认执行：
// 替身驱动，不建窗、不锁光标、不动鼠标）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// 路由替身（`_Router`）。
    struct FakeRouter {
        state: Mutex<Result<State, ()>>,
    }

    impl FakeRouter {
        fn new(state: State) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(Ok(state)),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(Err(())),
            })
        }
    }

    impl RouterLike for FakeRouter {
        fn state(&self) -> Result<State, CursorWatchError> {
            lock(&self.state).clone().map_err(|()| CursorWatchError::Backend {
                message: "router.state 不可用（测试注入）".to_string(),
            })
        }
    }

    /// Raw Input 替身（`_Raw`）：固定事件年龄，记录注入。
    struct FakeRaw {
        age: Result<f64, ()>,
        inject_ok: bool,
        injected: Mutex<Vec<RawDelta>>,
    }

    impl FakeRaw {
        fn new(age: f64) -> Arc<Self> {
            Arc::new(Self {
                age: Ok(age),
                inject_ok: true,
                injected: Mutex::new(Vec::new()),
            })
        }

        fn failing_inject(age: f64) -> Arc<Self> {
            Arc::new(Self {
                age: Ok(age),
                inject_ok: false,
                injected: Mutex::new(Vec::new()),
            })
        }

        fn failing_age() -> Arc<Self> {
            Arc::new(Self {
                age: Err(()),
                inject_ok: true,
                injected: Mutex::new(Vec::new()),
            })
        }

        fn injected(&self) -> Vec<RawDelta> {
            lock(&self.injected).clone()
        }
    }

    impl RawInputLike for FakeRaw {
        fn last_event_age(&self) -> Result<f64, CursorWatchError> {
            self.age.map_err(|()| CursorWatchError::Backend {
                message: "last_event_age 不可用（测试注入）".to_string(),
            })
        }

        fn inject(&self, delta: RawDelta) -> Result<bool, CursorWatchError> {
            lock(&self.injected).push(delta);
            if self.inject_ok {
                Ok(true)
            } else {
                Err(CursorWatchError::Backend {
                    message: "inject 失败（测试注入）".to_string(),
                })
            }
        }
    }

    /// 光标/按键替身：按队列依次返回，队列空了就报错（对应 Python 的迭代器耗尽）。
    #[derive(Default)]
    struct FakeProbe {
        positions: Mutex<VecDeque<Option<(i32, i32)>>>,
        buttons: Mutex<VecDeque<Result<i32, ()>>>,
    }

    impl FakeProbe {
        fn new(positions: Vec<Option<(i32, i32)>>, buttons: Vec<i32>) -> Arc<Self> {
            Arc::new(Self {
                positions: Mutex::new(positions.into()),
                buttons: Mutex::new(buttons.into_iter().map(Ok).collect()),
            })
        }

        fn fixed_buttons(buttons: i32) -> Arc<Self> {
            let mut queue = VecDeque::new();
            for _ in 0..32 {
                queue.push_back(Ok(buttons));
            }
            Arc::new(Self {
                positions: Mutex::new(VecDeque::new()),
                buttons: Mutex::new(queue),
            })
        }
    }

    impl CursorProbe for FakeProbe {
        fn cursor_pos(&self) -> Result<Option<(i32, i32)>, CursorWatchError> {
            match lock(&self.positions).pop_front() {
                Some(position) => Ok(position),
                None => Err(CursorWatchError::Backend {
                    message: "cursor_pos 队列已空（测试注入）".to_string(),
                }),
            }
        }

        fn buttons(&self) -> Result<i32, CursorWatchError> {
            match lock(&self.buttons).pop_front() {
                Some(Ok(buttons)) => Ok(buttons),
                _ => Err(CursorWatchError::Backend {
                    message: "buttons 队列已空（测试注入）".to_string(),
                }),
            }
        }
    }

    fn watch(
        router: Arc<FakeRouter>,
        raw: Arc<FakeRaw>,
        probe: Arc<FakeProbe>,
        stale_s: f64,
    ) -> CursorWatch {
        // 具体类型在这里隐式 unsize 成 Arc<dyn ...>，测试里就不用到处写 as。
        CursorWatch::with_probe(
            router,
            raw,
            probe,
            CursorWatchConfig {
                interval_s: DEFAULT_INTERVAL_S,
                stale_s,
            },
        )
    }

    #[test]
    fn stale_raw_emits_synthetic_delta() {
        // Raw Input 静默 1.0s > stale 0.25s：用绝对坐标差合成一次位移。
        let router = FakeRouter::new(State::Idle);
        let raw = FakeRaw::new(1.0);
        let probe = FakeProbe::new(vec![Some((100, 200)), Some((110, 190))], vec![0x01, 0x01]);
        let watch = watch(router, Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(10.0)); // 记录基线
        assert!(watch.poll_once_at(10.1));

        let injected = raw.injected();
        assert_eq!(injected.len(), 1);
        assert_eq!((injected[0].dx, injected[0].dy, injected[0].buttons), (10, -10, 0x01));
        assert_eq!(injected[0].wheel, 0);
        assert!(!injected[0].absolute);
        assert!(injected[0].ts > 0.0);
    }

    #[test]
    fn fresh_raw_yields_no_synthetic_event() {
        let router = FakeRouter::new(State::Idle);
        let raw = FakeRaw::new(0.05);
        let probe = FakeProbe::new(vec![Some((100, 200))], vec![0x01]);
        let watch = watch(router, Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(1.0));

        assert!(raw.injected().is_empty());
    }

    #[test]
    fn captured_state_reconciles_buttons_without_motion() {
        // 接管期 PC 光标被钉住：只按 GetAsyncKeyState 补 Raw Input 漏掉的按键变化。
        let router = FakeRouter::new(State::Captured);
        let raw = FakeRaw::new(5.0);
        let probe = FakeProbe::new(
            vec![Some((100, 200))],
            vec![0x01, 0x01, 0x00],
        );
        let watch = watch(router, Arc::clone(&raw), probe, 0.25);

        assert!(watch.poll_once_at(1.0));
        assert!(!watch.poll_once_at(1.1)); // 状态未变，不重复注入
        assert!(watch.poll_once_at(1.2)); // Raw Input 漏掉的松键由系统状态补齐

        let injected = raw.injected();
        let triples: Vec<(i32, i32, i32)> = injected
            .iter()
            .map(|delta| (delta.dx, delta.dy, delta.buttons))
            .collect();
        assert_eq!(triples, vec![(0, 0, 0x01), (0, 0, 0x00)]);
    }

    #[test]
    fn button_map_matches_hid_bits() {
        assert_eq!(
            BUTTON_MAP,
            [
                (0x01, 0x01),
                (0x02, 0x02),
                (0x04, 0x04),
                (0x05, 0x08),
                (0x06, 0x10),
            ]
        );
    }

    #[test]
    fn loop_exits_on_stop() {
        // 纯替身：线程只轮询 FakeProbe，不碰 Win32。
        let router = FakeRouter::new(State::Idle);
        let raw = FakeRaw::new(0.05); // 永远"新鲜"→ 不会注入
        let probe = FakeProbe::fixed_buttons(0);
        let watch = CursorWatch::with_probe(
            router,
            Arc::clone(&raw) as Arc<dyn RawInputLike>,
            probe,
            CursorWatchConfig {
                interval_s: 0.005,
                stale_s: 0.25,
            },
        );

        watch.start();
        assert!(watch.running());
        watch.start(); // 幂等：不会起第二个线程
        thread::sleep(Duration::from_millis(30));
        watch.stop();
        assert!(!watch.running());
        assert!(raw.injected().is_empty());
    }

    #[test]
    fn stop_resets_baseline() {
        let router = FakeRouter::new(State::Idle);
        let raw = FakeRaw::new(1.0);
        let probe = FakeProbe::new(
            vec![Some((100, 200)), Some((110, 190)), Some((120, 180))],
            vec![0x01, 0x01, 0x01],
        );
        let watch = watch(router, Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(10.0)); // 基线
        watch.stop(); // 清空 last/last_buttons
        assert!(!watch.poll_once_at(10.1)); // 重新记基线，不注入
        assert!(raw.injected().is_empty());
    }

    #[test]
    fn router_failure_means_not_captured() {
        // Python: router.state 抛异常 → captured=False → 走绝对坐标分支。
        let raw = FakeRaw::new(1.0);
        let probe = FakeProbe::new(vec![Some((100, 200)), Some((100, 200))], vec![0x01]);
        let watch = watch(FakeRouter::failing(), Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(10.0)); // 基线
        assert!(!watch.poll_once_at(10.1)); // 位移为 0，不注入
        assert!(raw.injected().is_empty());
    }

    #[test]
    fn age_failure_is_treated_as_stale() {
        // 读不到事件年龄 → 当成"非常久"→ 兜底继续工作。
        let raw = FakeRaw::failing_age();
        let probe = FakeProbe::new(vec![Some((0, 0)), Some((5, 7))], vec![0x00]);
        let watch = watch(FakeRouter::new(State::Idle), Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(1.0));
        assert!(watch.poll_once_at(1.1));
        let injected = raw.injected();
        assert_eq!((injected[0].dx, injected[0].dy), (5, 7));
    }

    #[test]
    fn inject_failure_returns_false() {
        let raw = FakeRaw::failing_inject(1.0);
        let probe = FakeProbe::new(vec![Some((0, 0)), Some((5, 7))], vec![0x00]);
        let watch = watch(FakeRouter::new(State::Idle), Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(1.0));
        assert!(!watch.poll_once_at(1.1)); // 注入失败只记日志，判定返回 false
        assert_eq!(raw.injected().len(), 1);
    }

    #[test]
    fn cursor_unavailable_is_noop() {
        let raw = FakeRaw::new(1.0);
        let probe = FakeProbe::new(vec![None, None], vec![0x00]);
        let watch = watch(FakeRouter::new(State::Idle), Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(1.0));
        assert!(!watch.poll_once_at(1.1));
        assert!(raw.injected().is_empty());
    }

    #[test]
    fn captured_buttons_failure_returns_false() {
        let raw = FakeRaw::new(5.0);
        let probe = Arc::new(FakeProbe::default()); // 两个队列都空 → 按键读取失败
        let watch = watch(FakeRouter::new(State::Captured), Arc::clone(&raw), probe, 0.25);

        assert!(!watch.poll_once_at(1.0));
        assert!(raw.injected().is_empty());
    }
}
