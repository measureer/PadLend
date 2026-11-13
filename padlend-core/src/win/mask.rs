//! 光标遮罩与 `ClipCursor`（对应 `win/mask.py`，设计文档 6.6 / 8.2 / 12.3）。
//!
//! 接管期把 PC 光标"藏起来"：`ClipCursor` 把它钉在屏幕边缘 1px 条上，同时每个显示器盖一个
//! 几乎全透明（alpha=1）的 `WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`
//! 窗口。遮罩窗口：
//!
//! - 不抢焦点（`WS_EX_NOACTIVATE` + `SW_SHOWNOACTIVATE` + `SWP_NOACTIVATE`）；
//! - 不进 Alt-Tab（`WS_EX_TOOLWINDOW`）；
//! - **永远在最顶层**（`WS_EX_TOPMOST` + 接管期看门狗）：任务栏、输入法面板等置顶窗口一旦
//!   盖到遮罩上面，光标就会重新显示 —— 用户看到的"电脑和平板同时出现鼠标指针"正是这个
//!   （1.1.1 回归修复）；
//! - 接管期**接收**鼠标消息（不再加 `WS_EX_TRANSPARENT`），以便 `WM_SETCURSOR` 把系统光标
//!   置空；此时 PC 点击本就应被忽略，不会影响平板输入；
//! - 永远不调用 `SetForegroundWindow`。
//!
//! 光标隐藏只依赖遮罩窗口：窗口过程处理 `WM_SETCURSOR` → `SetCursor(NULL)` 并返回 1。
//! **绝不使用进程级 `ShowCursor` 计数**——计数跨线程不平衡时会把光标永久压成隐藏。
//!
//! **遮罩窗口只属于一条专用线程**（1.1.2）：创建、消息泵、销毁都跑在 `padlend-mask-win`
//! 线程上。1.1.1 的跨线程释放靠 `PostMessageW(WM_CLOSE)` 交给窗口所属线程处理，但 UI 按钮经
//! RPC 线程池进入接管时，那个线程没有消息泵，`WM_CLOSE` 永远不被处理 → 遮罩窗口泄漏，
//! 整块屏幕没有光标而且吞掉所有点击。现在由窗口线程自己泵消息，释放时把 `DestroyWindow`
//! 直接投递到该线程执行；万一投递超时，退路是先 `ShowWindow(SW_HIDE)` 把屏幕让出来，再补发
//! `WM_CLOSE` 让窗口线程稍后收尾。**绝不能让遮罩继续盖着屏幕。**
//!
//! **`ClipCursor` 泄漏是灾难级故障**（6.6 的教训）：用户光标会被锁死在 1px 条里。
//! 因此 `release()` 幂等、无异常、`ClipCursor(None)` 走无条件路径，并在首次 `show()`
//! 时登记 atexit 兜底（12.3 第一层）。Rust 没有 `atexit`，对位物是 [`unclip_on_exit`]：
//! 主程序退出前必须显式调用一次（见模块末尾说明）。
//!
//! 导入本模块不创建窗口、不锁光标（实施说明 2.6）：所有 Win32 调用都发生在
//! [`CursorMask::show`] 之后，静态注册表是惰性的。
//!
//! # 与 `win/rawinput.rs` 的结构差异
//!
//! rawinput 的窗口是 `HWND_MESSAGE`（不可见）+ `GWLP_USERDATA` 绑定统计对象；遮罩窗口
//! 与此**必须**不同，且不是移植遗漏：
//!
//! - 遮罩要盖在每个显示器上，"只收消息"的 `HWND_MESSAGE` 窗口不参与绘制，不能用；
//! - 窗口过程不需要任何状态（`WM_SETCURSOR` / `WM_CLOSE` 都不读对象），所以没有
//!   `GWLP_USERDATA`；Python 版同样如此。
//!
//! 相同的是"专用线程 + 消息泵 + Drop 里停止"这一层：任务队列、建窗代次、超时与退路
//! 与 Python 逐行对齐（含错误文案）。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use thiserror::Error;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClipCursor,
    GetCursorInfo, GetSystemMetrics, GetWindowLongPtrW, LoadCursorW, PeekMessageW, PostMessageW,
    FindWindowW, GetWindowThreadProcessId, RegisterClassExW, SetCursor, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, TranslateMessage, CURSORINFO, CURSOR_SHOWING, GWL_EXSTYLE, HCURSOR,
    HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE, SET_WINDOW_POS_FLAGS, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WNDCLASSEXW, WS_POPUP,
};

use crate::logic::geometry::{Edge, EdgeSegment, Rect};

// ---------------------------------------------------------------------------
// 常量（与 Python 模块级常量逐字对齐）
// ---------------------------------------------------------------------------

/// 遮罩扩展样式：分层窗口（alpha=1）。
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
/// 仅保留常量供旧测试/文档引用，当前不再使用（遮罩必须接收 `WM_SETCURSOR`）。
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
/// 不抢焦点。
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
/// 不进 Alt-Tab。
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// 永远置顶。1.1.1 回归：非 topmost 的遮罩会被任务栏 / 其它置顶窗口盖住，光标落到
/// 它们上面就重新显示（"电脑和平板同时出现鼠标指针"）。因此从创建起就带
/// `WS_EX_TOPMOST`，并在接管期由看门狗持续回读确认。
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;

/// 遮罩窗口创建时使用的扩展样式（6.6 的 EX_STYLE）。
pub const MASK_EX_STYLE: u32 = WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;

/// 光标移到窗口上时由窗口过程处理的消息。
pub const WM_SETCURSOR: u32 = 0x0020;
/// 关闭窗口（跨线程释放的唯一入口）。
pub const WM_CLOSE: u32 = 0x0010;
/// 鼠标移动（只作为 `WM_SETCURSOR` 的 lParam 命中码使用）。
pub const WM_MOUSEMOVE: u32 = 0x0200;
/// `WM_SETCURSOR` 的命中测试码：落在客户区。
pub const HTCLIENT: u32 = 1;
/// 箭头光标资源 ID（`LoadCursorW(NULL, IDC_ARROW)`）。
pub const IDC_ARROW: u32 = 32512;

/// `GetWindowLongPtrW` 的扩展样式索引（`GWL_EXSTYLE`）。
pub const GWL_EXSTYLE_INDEX: i32 = -20;

/// `HWND_TOPMOST` 的整数值（-1）。
pub const HWND_TOPMOST_VALUE: isize = -1;

/// `RegisterClassExW` 遇到"类已存在"时的 Win32 错误码。
pub const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

/// 接管期看门狗（1.1.1）：每秒回读一次遮罩是否仍置顶、光标是否仍隐藏。
pub const WATCHDOG_INTERVAL_S: f64 = 1.0;
/// 修复动作的日志限流间隔（避免每秒一条淹没日志）。
pub const REPAIR_LOG_INTERVAL_S: f64 = 5.0;

/// 遮罩专用线程（1.1.2）：投递任务的等待上限。
pub const MASK_THREAD_TIMEOUT_S: f64 = 5.0;
/// 遮罩线程消息泵的空转间隔（PeekMessage 不是阻塞的）。
pub const MASK_PUMP_SLEEP_S: f64 = 0.005;
/// 孤儿遮罩清理时等待窗口消失的轮询次数与间隔。
pub const ORPHAN_POLL_TRIES: usize = 20;
/// 孤儿遮罩清理的轮询间隔（毫秒）。
pub const ORPHAN_POLL_INTERVAL_MS: u64 = 50;

/// 遮罩窗口类名（PadLend 私有常量，不会命中其它应用的窗口）。
pub const WINDOW_CLASS_NAME: &str = "PadLendCursorMaskWindow";
/// 遮罩窗口标题（孤儿清理按类名 + 标题查找）。
pub const WINDOW_TITLE: &str = "PadLend Cursor Mask";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 遮罩层的错误。
///
/// 文本逐字对齐 Python 侧抛出的 `OSError` / `TimeoutError` 消息，便于两侧日志 diff。
#[derive(Debug, Error)]
pub enum MaskError {
    /// `CreateWindowExW` 返回空句柄（Python `OSError(f"CreateWindowExW 失败 err={err}")`）。
    #[error("CreateWindowExW 失败 err={err}")]
    CreateWindow {
        /// `GetLastError` 的原始值。
        err: i64,
    },
    /// 窗口类注册失败（Python `OSError(f"RegisterClassExW 失败 err={err}")`）。
    #[error("RegisterClassExW 失败 err={err}")]
    RegisterClass {
        /// `GetLastError` 的原始值。
        err: i64,
    },
    /// 任务已被遮罩线程取走却迟迟不返回：任务可能卡在 `CreateWindowExW`/`SetWindowPos` 里。
    #[error("遮罩线程任务超时（仍在执行）")]
    ThreadTimeoutRunning,
    /// 任务还没被遮罩线程取走就超时：可以安全地认为它没有执行。
    #[error("遮罩线程任务超时（未执行）")]
    ThreadTimeoutNotRun,
    /// 遮罩线程已经退出（对应 Python `_fail_pending_tasks` 的 `RuntimeError`）。
    #[error("遮罩线程已退出，任务未执行")]
    ThreadExited,
    /// 遮罩线程任务 panic（Python 侧是把原异常回抛）。
    #[error("遮罩线程任务异常：{message}")]
    TaskFailed {
        /// panic payload 的可读文本。
        message: String,
    },
    /// 平台后端自己报告的错误（真实 Win32 错误或测试替身注入的故障）。
    #[error("{message}")]
    Backend {
        /// 原始错误文本。
        message: String,
    },
}

/// 把 `windows` crate 的错误转成后端错误（保留原文）。
impl From<windows::core::Error> for MaskError {
    fn from(err: windows::core::Error) -> Self {
        MaskError::Backend {
            message: err.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 平台后端接口（测试注入假实现）
// ---------------------------------------------------------------------------

/// 遮罩线程消息泵：收到一个 `idle` 回调，循环调用它取走投递进来的建窗/销窗任务。
pub type MaskMessageLoop = Arc<dyn Fn(&mut dyn FnMut()) -> Result<(), MaskError> + Send + Sync>;

/// 遮罩用到的全部平台调用（Python `win.mask.MaskApi` Protocol 的 1:1 移植）。
///
/// 单测用 `FakeMaskApi` 替换。Python 用 `getattr` + `callable` 探测"后端有没有这个
/// 能力"，Rust 侧改用**默认方法**表达同一件事：默认实现即"后端不支持，按 Python 的
/// 缺省分支处理"。
pub trait MaskApi: Send + Sync {
    /// 每个显示器的物理矩形；至少一个（枚举失败时后端自己退回虚拟桌面包围盒）。
    fn monitor_rects(&self) -> Result<Vec<Rect>, MaskError>;

    /// 只创建窗口并返回 hwnd（0 视为失败）；alpha/置顶等配置放到
    /// [`MaskApi::configure_mask_window`]。
    ///
    /// 1.2.0：创建与配置拆开，调用方在 `CreateWindowExW` 返回后立刻登记 hwnd。1.1.2 把
    /// 四步塞在一个函数里，配置阶段一旦被 DWM/开始菜单卡住，hwnd 还没返回、调用方也没
    /// 登记，超时重试就会再建一个窗口并把第一个漏掉。
    fn create_mask_window(&self, rect: Rect) -> Result<isize, MaskError>;

    /// 配置已创建的窗口（alpha=1 + 非激活显示 + 置顶并回读确认）。默认空实现 = 后端不支持。
    fn configure_mask_window(&self, hwnd: isize, rect: Rect) -> Result<(), MaskError> {
        let _ = (hwnd, rect);
        Ok(())
    }

    /// 销毁窗口。**必须由窗口所属线程执行**（见模块文档 1.1.2）。
    fn destroy_window(&self, hwnd: isize) -> Result<(), MaskError>;

    /// 设置/解除光标裁剪；返回 Win32 是否报告成功（`None` = 解除）。
    fn clip_cursor(&self, rect: Option<Rect>) -> Result<bool, MaskError>;

    /// 当前生效的光标裁剪矩形；查询失败返回 `None`（诊断用）。
    fn clip_rect(&self) -> Result<Option<Rect>, MaskError> {
        Ok(None)
    }

    /// 把当前线程的光标设为空/箭头（**不碰 `ShowCursor` 计数**）。
    fn set_cursor_hidden(&self, hidden: bool) -> Result<(), MaskError>;

    /// 请求窗口所属线程关闭并销毁窗口；返回 `PostMessage` 是否成功。
    fn post_close_window(&self, hwnd: isize) -> Result<bool, MaskError> {
        let _ = hwnd;
        Ok(false)
    }

    /// 把遮罩重新放到最顶层并回读确认。
    fn ensure_topmost(&self, hwnd: isize) -> Result<bool, MaskError> {
        let _ = hwnd;
        Ok(true)
    }

    /// 遮罩当前是否真的带 `WS_EX_TOPMOST`；读不到返回 `None`。
    fn window_topmost(&self, hwnd: isize) -> Result<Option<bool>, MaskError> {
        let _ = hwnd;
        Ok(None)
    }

    /// 让遮罩立刻重新决定光标（投递一条 `WM_SETCURSOR`），不必等用户移动鼠标。
    fn refresh_cursor(&self, hwnd: isize) -> Result<(), MaskError> {
        let _ = hwnd;
        Ok(())
    }

    /// 立刻把窗口藏起来（跨线程安全）；窗口本身仍等待销毁。
    fn hide_window(&self, hwnd: isize) -> Result<(), MaskError> {
        let _ = hwnd;
        Ok(())
    }

    /// 当前光标是否对用户可见；查询失败返回 `None`。
    ///
    /// 接管期这个值必须是 `false`：一旦为 `true`，用户就会在电脑屏幕上看见鼠标
    /// （穿透时"点一下鼠标冒出来"的现场证据）。
    fn cursor_visible(&self) -> Result<Option<bool>, MaskError> {
        Ok(None)
    }

    /// 遮罩专用线程的消息泵；返回 `None` 表示后端没有消息泵（测试假实现 / 非 Windows），
    /// 此时所有窗口操作退回调用线程执行，保持 1.1.1 之前的行为。
    fn mask_message_loop(&self) -> Option<MaskMessageLoop> {
        None
    }
}

// ---------------------------------------------------------------------------
// set_cursor_hidden 的注入缝（Python 测试 monkeypatch 了 `_user32`）
// ---------------------------------------------------------------------------

/// [`set_cursor_hidden_with`] 用到的两个 Win32 调用。
///
/// Python 侧的单测用 `monkeypatch.setattr(mask_module, "_user32", ...)` 换掉整个
/// user32，这里把它收窄成恰好这两个调用，测试注入替身后即可断言"绝不碰 `ShowCursor`"。
pub trait CursorOps: Send + Sync {
    /// `SetCursor(handle)`；`None` 表示空光标。
    fn set_cursor(&self, handle: Option<isize>) -> Result<(), MaskError>;
    /// `LoadCursorW(NULL, IDC_ARROW)`，返回光标句柄。
    fn load_arrow(&self) -> Result<isize, MaskError>;
}

/// 真实的 user32 实现。
pub struct Win32CursorOps;

impl CursorOps for Win32CursorOps {
    fn set_cursor(&self, handle: Option<isize>) -> Result<(), MaskError> {
        let cursor = handle.map(|raw| HCURSOR(raw as *mut c_void));
        let _ = unsafe { SetCursor(cursor) };
        Ok(())
    }

    fn load_arrow(&self) -> Result<isize, MaskError> {
        let handle = unsafe {
            LoadCursorW(
                None,
                PCWSTR(IDC_ARROW as usize as *const u16),
            )
        }?;
        Ok(handle.0 as isize)
    }
}

/// 把当前线程的光标设为空/箭头（`WM_SETCURSOR` 之外唯一的隐藏入口）。
///
/// 真正的隐藏发生在遮罩窗口的 `WM_SETCURSOR`；这里**不碰 `ShowCursor`**，避免
/// 进程级计数跨线程失衡后光标永久消失。
pub fn set_cursor_hidden_with(ops: &dyn CursorOps, hidden: bool) -> Result<(), MaskError> {
    if hidden {
        ops.set_cursor(None)
    } else {
        let handle = ops.load_arrow()?;
        ops.set_cursor(Some(handle))
    }
}

// ---------------------------------------------------------------------------
// 真实 Win32 实现
// ---------------------------------------------------------------------------

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// 遮罩窗口类是否已注册：`OnceLock` 里放注册结果（0 = 成功，非 0 = Win32 错误码）。
static WINDOW_CLASS: OnceLock<Result<(), i64>> = OnceLock::new();

/// 真实的 Win32 实现。
///
/// 需要进程处于 per-monitor DPI aware（`win/dpi.py` 在启动早期设置 PMv2），否则
/// `GetMonitorInfoW` / `GetSystemMetrics` 返回的是被虚拟化的逻辑坐标，遮罩会错位。
pub struct Win32MaskApi;

impl MaskApi for Win32MaskApi {
    fn monitor_rects(&self) -> Result<Vec<Rect>, MaskError> {
        let mut rects: Vec<Rect> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(monitor_enum_proc),
                LPARAM(&mut rects as *mut Vec<Rect> as isize),
            );
        }
        if rects.is_empty() {
            eprintln!("[mask] EnumDisplayMonitors 未返回显示器，退回虚拟桌面包围盒");
            rects.push(virtual_desktop_rect());
        }
        Ok(rects)
    }

    fn create_mask_window(&self, rect: Rect) -> Result<isize, MaskError> {
        ensure_window_class()?;
        let instance = unsafe { GetModuleHandleW(None) }?;
        let class = wide(WINDOW_CLASS_NAME);
        let title = wide(WINDOW_TITLE);
        let width = rect.width().max(1);
        let height = rect.height().max(1);
        let created = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(MASK_EX_STYLE),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                rect.left,
                rect.top,
                width,
                height,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        };
        match created {
            Ok(hwnd) => Ok(hwnd.0 as isize),
            Err(_) => Err(MaskError::CreateWindow {
                err: unsafe { GetLastError() }.0 as i64,
            }),
        }
    }

    fn configure_mask_window(&self, hwnd: isize, rect: Rect) -> Result<(), MaskError> {
        let handle = hwnd_of(hwnd);
        let width = rect.width().max(1);
        let height = rect.height().max(1);
        unsafe {
            // alpha=1：近乎全透明，用户不应看到任何遮挡（6.6）。
            if let Err(err) = SetLayeredWindowAttributes(handle, COLORREF(0), 1, LWA_ALPHA) {
                eprintln!("[mask] SetLayeredWindowAttributes 失败 hwnd={hwnd}：{err}");
            }
            let _ = ShowWindow(handle, SW_SHOWNOACTIVATE);
            // 用物理矩形重排一次，确保跨显示器 / DPI 变化后位置正确。
            if let Err(err) = SetWindowPos(
                handle,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                width,
                height,
                SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0),
            ) {
                eprintln!("[mask] SetWindowPos 失败 hwnd={hwnd}：{err}");
            }
        }
        // 1.1.1 现场：这一步会"返回 TRUE 但不生效"，遮罩被任务栏压住、PC 光标重新
        // 显示。样式里已带 WS_EX_TOPMOST，但必须回读确认，不能相信返回值。
        if self.window_topmost(hwnd)? != Some(true) {
            eprintln!("[mask] 遮罩创建后未置顶 hwnd={hwnd}，重试一次");
            let _ = self.ensure_topmost(hwnd)?;
            if self.window_topmost(hwnd)? != Some(true) {
                eprintln!("[mask] 遮罩无法置顶 hwnd={hwnd}：接管期电脑光标可能重新显示");
            }
        }
        Ok(())
    }

    fn destroy_window(&self, hwnd: isize) -> Result<(), MaskError> {
        unsafe { DestroyWindow(hwnd_of(hwnd)) }?;
        Ok(())
    }

    fn clip_cursor(&self, rect: Option<Rect>) -> Result<bool, MaskError> {
        match rect {
            None => Ok(unsafe { ClipCursor(None) }.is_ok()),
            Some(rect) => {
                let native = RECT {
                    left: rect.left,
                    top: rect.top,
                    right: rect.right,
                    bottom: rect.bottom,
                };
                Ok(unsafe { ClipCursor(Some(&native)) }.is_ok())
            }
        }
    }

    fn clip_rect(&self) -> Result<Option<Rect>, MaskError> {
        let mut native = RECT::default();
        if unsafe { GetClipCursor(&mut native) }.is_err() {
            return Ok(None);
        }
        Ok(Some(Rect::new(
            native.left,
            native.top,
            native.right,
            native.bottom,
        )))
    }

    fn set_cursor_hidden(&self, hidden: bool) -> Result<(), MaskError> {
        set_cursor_hidden_with(&Win32CursorOps, hidden)
    }

    fn post_close_window(&self, hwnd: isize) -> Result<bool, MaskError> {
        let posted = unsafe { PostMessageW(Some(hwnd_of(hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        Ok(posted.is_ok())
    }

    fn ensure_topmost(&self, hwnd: isize) -> Result<bool, MaskError> {
        // SetWindowPos(HWND_TOPMOST) 在真实故障现场会返回成功却没真的置顶（遮罩仍在
        // 任务栏下面），所以这里以回读结果为准。
        unsafe {
            let _ = SetWindowPos(
                hwnd_of(hwnd),
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SET_WINDOW_POS_FLAGS(SWP_NOMOVE.0 | SWP_NOSIZE.0 | SWP_NOACTIVATE.0 | SWP_NOOWNERZORDER.0),
            );
        }
        Ok(self.window_topmost(hwnd)? == Some(true))
    }

    fn window_topmost(&self, hwnd: isize) -> Result<Option<bool>, MaskError> {
        let style = unsafe { GetWindowLongPtrW(hwnd_of(hwnd), GWL_EXSTYLE) };
        Ok(Some((style as u32 & WS_EX_TOPMOST) != 0))
    }

    fn refresh_cursor(&self, hwnd: isize) -> Result<(), MaskError> {
        // 向窗口过程投递一条 WM_SETCURSOR（命中测试 HTCLIENT、鼠标消息 WM_MOUSEMOVE）：
        // 窗口过程收到后会 SetCursor(NULL)，把光标重新藏起来。
        let lparam = ((HTCLIENT << 16) | WM_MOUSEMOVE) as isize;
        let _ = unsafe {
            PostMessageW(
                Some(hwnd_of(hwnd)),
                WM_SETCURSOR,
                WPARAM(hwnd as usize),
                LPARAM(lparam),
            )
        };
        Ok(())
    }

    fn hide_window(&self, hwnd: isize) -> Result<(), MaskError> {
        let _ = unsafe { ShowWindow(hwnd_of(hwnd), SW_HIDE) };
        Ok(())
    }

    fn cursor_visible(&self) -> Result<Option<bool>, MaskError> {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return Ok(None);
        }
        Ok(Some(info.flags.0 & CURSOR_SHOWING.0 != 0))
    }

    fn mask_message_loop(&self) -> Option<MaskMessageLoop> {
        Some(Arc::new(|idle: &mut dyn FnMut()| {
            run_mask_message_loop(idle);
            Ok(())
        }))
    }
}

/// `EnumDisplayMonitors` 的回调：把每个显示器的物理矩形追加进 `data` 指向的 Vec。
unsafe extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let rects = unsafe { &mut *(data.0 as *mut Vec<Rect>) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() {
        rects.push(Rect::new(
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right,
            info.rcMonitor.bottom,
        ));
    }
    BOOL(1)
}

/// 虚拟桌面包围盒（`EnumDisplayMonitors` 什么都没返回时的退路）。
fn virtual_desktop_rect() -> Rect {
    let mut left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let mut top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let mut width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let mut height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
        height = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
        left = 0;
        top = 0;
    }
    Rect::new(left, top, left + width, top + height)
}

/// 遮罩窗口过程：`WM_SETCURSOR` 置空光标并返回 1；`WM_CLOSE` 在本线程销毁窗口。
unsafe extern "system" fn mask_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            // 光标移到遮罩上时立即置空；返回 1 阻止 DefWindowProc 重新设置箭头。
            let _ = unsafe { SetCursor(None) };
            LRESULT(1)
        }
        WM_CLOSE => {
            // 跨线程释放只能 PostMessage(WM_CLOSE)；DestroyWindow 必须由本线程执行。
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 注册（一次）遮罩窗口类；用 `OnceLock` 代替 Python 的 `_class_lock` + `_window_proc`。
fn ensure_window_class() -> Result<(), MaskError> {
    let outcome = WINDOW_CLASS.get_or_init(|| unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(module) => HINSTANCE(module.0),
            Err(_) => return Err(GetLastError().0 as i64),
        };
        let class = wide(WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(mask_window_proc),
            hInstance: instance,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let error = GetLastError().0;
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(error as i64);
            }
        }
        Ok(())
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(err) => Err(MaskError::RegisterClass { err: *err }),
    }
}

/// 在当前线程泵消息，直到进程退出（遮罩专用线程里调用）。
///
/// 遮罩窗口的 `WM_SETCURSOR` / `WM_CLOSE` 都只能在本线程处理，所以 `idle` 被反复调用
/// 来取走投递进来的建窗 / 销窗任务。
fn run_mask_message_loop(idle: &mut dyn FnMut()) {
    // 注意：Python 在这里还会 SetThreadDpiAwarenessContext(PMv2)（可选 API）。
    // 当前 Cargo.toml 没有 Win32_UI_HiDpi feature，见移植报告；遮罩线程继承进程
    // 默认的 DPI 感知，PMv2 由 win/dpi.py 在启动早期统一设置。
    let mut msg = MSG::default();
    loop {
        idle();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        thread::sleep(Duration::from_secs_f64(MASK_PUMP_SLEEP_S));
    }
}

// ---------------------------------------------------------------------------
// 孤儿遮罩清理
// ---------------------------------------------------------------------------

/// 孤儿清理用到的平台调用（Python 测试 monkeypatch 了 `_user32` / `_kernel32`）。
pub trait OrphanApi {
    /// 按类名 + 标题查找窗口。
    fn find_window(&self, class: &str, title: &str) -> Option<isize>;
    /// 窗口所属进程 PID。
    fn window_process_id(&self, hwnd: isize) -> u32;
    /// `ShowWindow(SW_HIDE)`。
    fn hide_window(&self, hwnd: isize);
    /// `PostMessage(WM_CLOSE)`；返回是否投递成功。
    fn post_close(&self, hwnd: isize) -> bool;
    /// `ClipCursor(None)`（无条件）。
    fn unclip(&self);
    /// `OpenProcess(PROCESS_TERMINATE)` + TerminateProcess + CloseHandle`；成功返回 true。
    fn terminate_process(&self, pid: u32) -> bool;
    /// 轮询等待（真实实现睡 50ms；测试替身直接返回，对应 pytest 里被 monkeypatch 的
    /// `time.sleep`）。
    fn poll_delay(&self) {
        thread::sleep(Duration::from_millis(ORPHAN_POLL_INTERVAL_MS));
    }
}

/// 真实 Win32 的孤儿清理调用。
pub struct Win32OrphanApi;

impl OrphanApi for Win32OrphanApi {
    fn find_window(&self, class: &str, title: &str) -> Option<isize> {
        let class = wide(class);
        let title = wide(title);
        let hwnd = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) }.ok()?;
        if hwnd.0.is_null() {
            None
        } else {
            Some(hwnd.0 as isize)
        }
    }

    fn window_process_id(&self, hwnd: isize) -> u32 {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd_of(hwnd), Some(&mut pid));
        }
        pid
    }

    fn hide_window(&self, hwnd: isize) {
        let _ = unsafe { ShowWindow(hwnd_of(hwnd), SW_HIDE) };
    }

    fn post_close(&self, hwnd: isize) -> bool {
        unsafe { PostMessageW(Some(hwnd_of(hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok()
    }

    fn unclip(&self) {
        let _ = unsafe { ClipCursor(None) };
    }

    fn terminate_process(&self, pid: u32) -> bool {
        match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => {
                let terminated = unsafe { TerminateProcess(handle, 1) }.is_ok();
                let _ = unsafe { CloseHandle(handle) };
                terminated
            }
            Err(_) => false,
        }
    }
}

/// 收掉上一个 sidecar 留下的遮罩窗口，返回其 PID（没有则 `None`）。
///
/// 进程真正退出时 Windows 会销毁窗口；能留在这里的只可能是"sidecar 还活着但主线程/
/// 消息泵卡死"或"外壳已崩溃、sidecar 变成孤儿"。启动时先按窗口类名找到它，隐藏、
/// 补发 `WM_CLOSE`，仍不消失就直接终止那个进程，最后解除 `ClipCursor`。窗口类名是
/// PadLend 私有常量，不会命中其它应用的窗口。
///
/// 与 Python 的差异：`_require_windows()` 没有对位物（本 crate 只在 Windows 构建），
/// 所以返回类型是 `Option<u32>` 而不是 `int | None` + 异常。
pub fn cleanup_orphan_mask() -> Option<u32> {
    cleanup_orphan_mask_with(&Win32OrphanApi)
}

/// [`cleanup_orphan_mask`] 的可注入版本（单测走这里）。
pub fn cleanup_orphan_mask_with(api: &dyn OrphanApi) -> Option<u32> {
    let hwnd = api.find_window(WINDOW_CLASS_NAME, WINDOW_TITLE)?;
    let pid = api.window_process_id(hwnd);
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    eprintln!("[mask] 发现孤儿遮罩窗口 hwnd={hwnd} pid={pid}，执行清理");
    api.hide_window(hwnd);
    api.post_close(hwnd);
    let mut gone = false;
    for _ in 0..ORPHAN_POLL_TRIES {
        api.poll_delay();
        if api.find_window(WINDOW_CLASS_NAME, WINDOW_TITLE).is_none() {
            gone = true;
            break;
        }
    }
    if !gone && !api.terminate_process(pid) {
        eprintln!("[mask] 无法终止孤儿 sidecar pid={pid}（可能已退出）");
    }
    api.unclip();
    Some(pid)
}

// ---------------------------------------------------------------------------
// 几何：边缘 1px 条
// ---------------------------------------------------------------------------

/// 把外轮廓边段换算成 `ClipCursor` 用的边缘条（6.6：边缘 1px 条）。
///
/// 条带沿边方向被 `span_start`/`span_end` 限制在显示器范围内；如果边段完全落在显示器
/// 之外（摆位过期），退回整条边 —— 宁可锁住整条边，也不要产生非法矩形。
pub fn clip_rect_for_segment(
    monitor_rect: Rect,
    segment: &EdgeSegment,
    thickness: Option<i32>,
) -> Rect {
    let band = thickness.unwrap_or(crate::constants::EDGE_BAND_PX).max(1);
    match segment.edge {
        Edge::Left => {
            let mut top = monitor_rect.top.max(segment.span_start);
            let mut bottom = monitor_rect.bottom.min(segment.span_end);
            if bottom <= top {
                top = monitor_rect.top;
                bottom = monitor_rect.bottom;
            }
            Rect::new(
                monitor_rect.left,
                top,
                (monitor_rect.left + band).min(monitor_rect.right),
                bottom,
            )
        }
        Edge::Right => {
            let mut top = monitor_rect.top.max(segment.span_start);
            let mut bottom = monitor_rect.bottom.min(segment.span_end);
            if bottom <= top {
                top = monitor_rect.top;
                bottom = monitor_rect.bottom;
            }
            Rect::new(
                (monitor_rect.right - band).max(monitor_rect.left),
                top,
                monitor_rect.right,
                bottom,
            )
        }
        Edge::Top => {
            let mut left = monitor_rect.left.max(segment.span_start);
            let mut right = monitor_rect.right.min(segment.span_end);
            if right <= left {
                left = monitor_rect.left;
                right = monitor_rect.right;
            }
            Rect::new(
                left,
                monitor_rect.top,
                right,
                (monitor_rect.top + band).min(monitor_rect.bottom),
            )
        }
        Edge::Bottom => {
            let mut left = monitor_rect.left.max(segment.span_start);
            let mut right = monitor_rect.right.min(segment.span_end);
            if right <= left {
                left = monitor_rect.left;
                right = monitor_rect.right;
            }
            Rect::new(
                left,
                (monitor_rect.bottom - band).max(monitor_rect.top),
                right,
                monitor_rect.bottom,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// atexit 兜底（12.3 第一层）
// ---------------------------------------------------------------------------

static ATEXIT_GUARD: AtomicBool = AtomicBool::new(false);

/// 进程退出前最后一道保险：永远不要让 `ClipCursor` 泄漏。
///
/// Python 用 `atexit.register(_atexit_unclip)`；Rust 没有进程退出钩子，对位物是
/// 本函数：**主程序（main.rs / 服务宿主）必须在退出前显式调用一次**。遮罩对象的
/// [`CursorMask::release`]（以及 `Drop`）已经覆盖常规路径，本函数只兜"没走到 Drop"
/// 的异常路径。
pub fn unclip_on_exit() {
    let _ = unsafe { ClipCursor(None) };
}

/// 首次 `show()` 时登记兜底（Python `_register_atexit_guard`）。幂等。
fn register_atexit_guard() {
    ATEXIT_GUARD.store(true, Ordering::SeqCst);
}

/// 兜底是否已登记（对应 pytest 里的 `mask_module._guard_registered`）。
pub fn atexit_guard_registered() -> bool {
    ATEXIT_GUARD.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

/// 加锁；锁中毒（持锁线程 panic）时取回内部值继续，绝不让遮罩路径再 panic 一次。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 投递给遮罩线程执行的一项工作（建窗 / 销窗）。
type TaskFn = Box<dyn FnOnce() -> Result<(), MaskError> + Send>;

struct Task {
    func: TaskFn,
    done: Sender<Result<(), MaskError>>,
    /// 任务是否已被遮罩线程取走（超时后据此区分"仍在执行"与"未执行"）。
    taken: Arc<AtomicBool>,
}

/// 对应 Python `_lock` 保护的公开状态（`show`/`hide`/`release` 串行化）。
struct Public {
    active: bool,
    clip_owned: bool,
    clip_rect: Option<Rect>,
    cursor_hidden: bool,
    /// 建窗线程（Python 侧也是只写字段，保留语义供诊断扩展）。
    #[allow(dead_code)]
    owner_thread: Option<ThreadId>,
    /// 上次修复日志的时间（`None` = 还没记过，对应 Python 的 0.0）。
    repair_log_at: Option<Instant>,
}

/// 对应 Python `_work_lock` 保护的状态：窗口列表 + 建窗代次 + 投递队列。
///
/// 遮罩线程只碰这一把锁（0.5 秒级的高频路径），`Public` 那把锁由调用线程与看门狗竞争，
/// 两者不会互相等待，避免"调用方持锁等遮罩线程、遮罩线程又要那把锁"的死锁。
struct Work {
    windows: Vec<isize>,
    epoch: u64,
    queue: VecDeque<Task>,
}

/// 遮罩专用线程的簿记。
struct ThreadSlot {
    handle: Option<JoinHandle<()>>,
    id: Option<ThreadId>,
    exited: Arc<AtomicBool>,
    failed: bool,
}

struct Shared {
    api: Arc<dyn MaskApi>,
    config: MaskConfig,
    public: Mutex<Public>,
    work: Mutex<Work>,
    thread: Mutex<ThreadSlot>,
}

/// 看门狗线程句柄：丢弃 `stop` 里的 Sender 即可让它立刻醒来退出。
struct Watchdog {
    stop: Option<Sender<()>>,
    alive: Arc<AtomicBool>,
}

/// 遮罩的可调参数（Python 侧是同名模块常量，单测用 monkeypatch 改）。
#[derive(Debug, Clone)]
pub struct MaskConfig {
    /// 投递任务到遮罩线程的等待上限（Python `_MASK_THREAD_TIMEOUT_S`）。
    pub thread_timeout: Duration,
    /// 接管期看门狗的复查间隔（Python `_WATCHDOG_INTERVAL_S`）。
    pub watchdog_interval: Duration,
    /// 修复日志的限流间隔（Python `_REPAIR_LOG_INTERVAL_S`）。
    pub repair_log_interval: Duration,
}

impl Default for MaskConfig {
    fn default() -> Self {
        Self {
            thread_timeout: Duration::from_secs_f64(MASK_THREAD_TIMEOUT_S),
            watchdog_interval: Duration::from_secs_f64(WATCHDOG_INTERVAL_S),
            repair_log_interval: Duration::from_secs_f64(REPAIR_LOG_INTERVAL_S),
        }
    }
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 接管期藏掉 PC 光标；不抢焦点、不接鼠标、不进 Alt-Tab（6.6 / 8.2）。
///
/// 语义：
///
/// - `show(Some(rect))`：显示遮罩并把光标锁到 `rect`（Router 传边缘 1px 条）；
/// - `show(None)`：只显示遮罩，**不改动**当前 `ClipCursor`；
/// - `hide()`：销毁遮罩，并解除自己持有的锁；
/// - `release()`：幂等、无异常的兜底释放 —— 无条件 `ClipCursor(None)` 后销毁窗口。
pub struct CursorMask {
    shared: Arc<Shared>,
    watchdog: Mutex<Option<Watchdog>>,
}

impl CursorMask {
    /// 遮罩扩展样式（对应 `CursorMask.EX_STYLE`）。
    pub const EX_STYLE: u32 = MASK_EX_STYLE;

    /// 用真实 Win32 后端构造。
    pub fn new(api: Arc<dyn MaskApi>) -> Self {
        Self::with_config(api, MaskConfig::default())
    }

    /// 用指定参数构造（单测用来缩短遮罩线程超时 / 看门狗间隔）。
    pub fn with_config(api: Arc<dyn MaskApi>, config: MaskConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                api,
                config,
                public: Mutex::new(Public {
                    active: false,
                    clip_owned: false,
                    clip_rect: None,
                    cursor_hidden: false,
                    owner_thread: None,
                    repair_log_at: None,
                }),
                work: Mutex::new(Work {
                    windows: Vec::new(),
                    epoch: 0,
                    queue: VecDeque::new(),
                }),
                thread: Mutex::new(ThreadSlot {
                    handle: None,
                    id: None,
                    exited: Arc::new(AtomicBool::new(true)),
                    failed: false,
                }),
            }),
            watchdog: Mutex::new(None),
        }
    }

    // -- 状态 -------------------------------------------------------------

    /// 当前是否处于接管状态（遮罩已显示）。
    pub fn active(&self) -> bool {
        lock(&self.shared.public).active
    }

    /// 当前的光标锁定是否由本对象持有（诊断 / I2 一致性检查用）。
    pub fn clip_owned(&self) -> bool {
        lock(&self.shared.public).clip_owned
    }

    /// 本对象最后一次成功设置的光标裁剪矩形。
    pub fn clip_rect(&self) -> Option<Rect> {
        lock(&self.shared.public).clip_rect
    }

    /// 一行现场状态：`visible` / `clip` / 遮罩窗口数 / 每个遮罩的 topmost 状态。
    ///
    /// 接管期（尤其是点击前后）写进日志，用来判断"电脑上又冒出鼠标"是遮罩被拆了、
    /// 锁被系统回收了，还是别的进程把光标显示回来了。文本格式与 Python 的 f-string
    /// 完全一致（`True`/`False`/`None` 与 `Rect(left=..)` 都是 Python repr）。
    pub fn diagnose(&self) -> String {
        let visible = self.shared.api.cursor_visible().ok().flatten();
        let clip = self.shared.api.clip_rect().ok().flatten();
        let windows = self.windows_snapshot().len();
        let active = self.active();
        format!(
            "visible={} clip={} windows={} topmost={} active={}",
            py_opt_bool(visible),
            py_opt_rect(clip),
            windows,
            py_topmost_list(&self.topmost_state()),
            active
        )
    }

    // -- 生命周期 ---------------------------------------------------------

    /// 显示遮罩（每显示器一个窗口），可选地把光标锁到 `clip_rect`。
    pub fn show(&self, clip_rect: Option<Rect>) -> Result<(), MaskError> {
        let mut public = lock(&self.shared.public);
        register_atexit_guard();
        let outcome = self.show_locked(&mut public, clip_rect);
        if outcome.is_err() {
            self.cleanup_quietly(&mut public, true);
        }
        outcome
    }

    fn show_locked(&self, public: &mut Public, clip_rect: Option<Rect>) -> Result<(), MaskError> {
        self.create_windows(public)?;
        public.active = true;
        self.hide_cursor(public);
        self.refresh_all_cursors();
        match clip_rect {
            None => {
                if public.clip_owned {
                    self.apply_clip(public, None);
                }
            }
            Some(rect) => self.apply_clip(public, Some(rect)),
        }
        self.start_watchdog();
        Ok(())
    }

    /// 销毁遮罩窗口；只解除本对象持有的光标锁。幂等。
    pub fn hide(&self) {
        let mut public = lock(&self.shared.public);
        self.stop_watchdog();
        if public.clip_owned {
            self.apply_clip(&mut public, None);
        }
        self.show_cursor(&mut public);
        self.destroy_windows(&mut public);
        public.active = false;
    }

    /// 兜底释放：无条件 `ClipCursor(None)`，再销毁窗口。幂等且绝不抛异常。
    pub fn release(&self) {
        let mut public = lock(&self.shared.public);
        self.stop_watchdog();
        public.clip_owned = false;
        public.clip_rect = None;
        if let Err(err) = self.shared.api.clip_cursor(None) {
            eprintln!("[mask] release: ClipCursor(None) 失败：{err}");
        }
        self.show_cursor(&mut public);
        self.destroy_windows(&mut public);
        public.active = false;
    }

    // -- 内部：窗口 -------------------------------------------------------

    fn create_windows(&self, public: &mut Public) -> Result<(), MaskError> {
        if !self.windows_snapshot().is_empty() {
            // 复用上一轮的窗口：系统可能在两次接管之间把 topmost 抢走（任务栏重建、
            // 显示设置变更……），所以每次接管都重新置顶一次（1.1.1）。
            self.reassert_topmost();
            return Ok(());
        }
        let rects = self.shared.api.monitor_rects()?;
        let epoch = {
            let mut work = lock(&self.shared.work);
            work.epoch = work.epoch.wrapping_add(1);
            work.epoch
        };

        let api = Arc::clone(&self.shared.api);
        let shared = Arc::clone(&self.shared);
        let created = Arc::new(AtomicBool::new(false));
        let created_in_task = Arc::clone(&created);
        let outcome = self.run_on_mask_thread(move || {
            for rect in rects {
                if epoch != lock(&shared.work).epoch {
                    return Ok(());
                }
                let hwnd = api.create_mask_window(rect)?;
                if hwnd == 0 {
                    continue;
                }
                // 先登记再配置：配置阶段（ShowWindow/SetWindowPos）在真实故障现场可能
                // 被 DWM/开始菜单卡住；只要 hwnd 已登记，释放路径就能找到它，不会出现
                // 无人认领的 topmost 窗口。
                let stale = {
                    let mut work = lock(&shared.work);
                    let stale = epoch != work.epoch;
                    if !stale {
                        work.windows.push(hwnd);
                    }
                    stale
                };
                if stale {
                    if let Err(err) = api.destroy_window(hwnd) {
                        eprintln!("[mask] 销毁未登记的遮罩窗口失败 hwnd={hwnd}：{err}");
                    }
                    return Ok(());
                }
                created_in_task.store(true, Ordering::SeqCst);
                api.configure_mask_window(hwnd, rect)?;
            }
            Ok(())
        });

        match outcome {
            Ok(on_worker) => {
                if created.load(Ordering::SeqCst) {
                    public.owner_thread = if on_worker {
                        lock(&self.shared.thread).id
                    } else {
                        Some(thread::current().id())
                    };
                }
                Ok(())
            }
            Err(err) => {
                // 超时/异常时旧任务可能还在跑：代次失效 + 清理已登记的窗口。
                self.destroy_windows(public);
                Err(err)
            }
        }
    }

    fn destroy_windows(&self, public: &mut Public) {
        let windows = {
            let mut work = lock(&self.shared.work);
            // 先失效在建任务，再取走窗口：两者在同一把锁内，晚到的建窗任务要么把
            // hwnd 放进列表（被这里取走），要么看到过期代次自己销毁。
            work.epoch = work.epoch.wrapping_add(1);
            std::mem::take(&mut work.windows)
        };
        for hwnd in windows {
            self.destroy_one(hwnd);
        }
        public.owner_thread = None;
    }

    /// 销毁一个遮罩窗口；必须落在建窗的那条线程上（1.1.2）。
    fn destroy_one(&self, hwnd: isize) {
        let api = Arc::clone(&self.shared.api);
        let delivered = match self.run_on_mask_thread(move || api.destroy_window(hwnd)) {
            Ok(on_worker) => on_worker,
            Err(err) => {
                eprintln!("[mask] 销毁遮罩窗口失败 hwnd={hwnd}：{err}");
                false
            }
        };
        if delivered || !self.mask_thread_started() {
            // 线程内销毁成功；或平台没有遮罩线程，窗口本就在调用线程创建。
            return;
        }
        // 线程没接住任务：先让窗口隐藏（跨线程安全），把屏幕立刻让出来，再补发
        // WM_CLOSE 让窗口线程稍后收尾。绝不能让遮罩继续盖着屏幕。
        if let Err(err) = self.shared.api.hide_window(hwnd) {
            eprintln!("[mask] 隐藏遮罩窗口失败 hwnd={hwnd}：{err}");
        }
        if let Err(err) = self.shared.api.post_close_window(hwnd) {
            eprintln!("[mask] 补发 WM_CLOSE 失败 hwnd={hwnd}：{err}");
        }
    }

    fn mask_thread_started(&self) -> bool {
        lock(&self.shared.thread).handle.is_some()
    }

    // -- 内部：遮罩线程 ---------------------------------------------------

    /// 确保遮罩专用线程已启动；平台后端没有消息泵时返回 `false`。
    fn ensure_mask_thread(&self) -> bool {
        let Some(loop_fn) = self.shared.api.mask_message_loop() else {
            return false;
        };
        let mut slot = lock(&self.shared.thread);
        if slot.handle.is_some() && !slot.exited.load(Ordering::SeqCst) {
            return true;
        }
        if slot.failed {
            return false;
        }
        let exited = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&self.shared);
        let exited_in_thread = Arc::clone(&exited);
        let spawned = thread::Builder::new()
            .name("padlend-mask-win".to_string())
            .spawn(move || mask_thread_main(shared, loop_fn, exited_in_thread));
        match spawned {
            Ok(handle) => {
                slot.handle = Some(handle);
                slot.exited = exited;
                true
            }
            Err(err) => {
                eprintln!("[mask] 无法创建遮罩线程：{err}");
                slot.failed = true;
                false
            }
        }
    }

    /// 在遮罩线程上执行 `func`，返回"是否由该线程执行"。
    ///
    /// 平台后端没有消息泵（测试假实现 / 非 Windows）时直接在调用线程执行，保持 1.1.1
    /// 之前的行为。
    fn run_on_mask_thread<F>(&self, func: F) -> Result<bool, MaskError>
    where
        F: FnOnce() -> Result<(), MaskError> + Send + 'static,
    {
        if !self.ensure_mask_thread() {
            func()?;
            return Ok(false);
        }
        let current = thread::current().id();
        let on_thread = lock(&self.shared.thread).id == Some(current);
        if on_thread {
            func()?;
            return Ok(true);
        }

        let (tx, rx) = mpsc::channel();
        let taken = Arc::new(AtomicBool::new(false));
        {
            let mut work = lock(&self.shared.work);
            work.queue.push_back(Task {
                func: Box::new(func),
                done: tx,
                taken: Arc::clone(&taken),
            });
        }

        match rx.recv_timeout(self.shared.config.thread_timeout) {
            Ok(Ok(())) => Ok(true),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Disconnected) => Err(MaskError::ThreadExited),
            Err(RecvTimeoutError::Timeout) => {
                let still_queued = {
                    let mut work = lock(&self.shared.work);
                    match work
                        .queue
                        .iter()
                        .position(|task| Arc::ptr_eq(&task.taken, &taken))
                    {
                        Some(index) => {
                            work.queue.remove(index);
                            true
                        }
                        None => false,
                    }
                };
                if still_queued {
                    // 任务没被取走：可以确定它没有执行。
                    eprintln!(
                        "[mask] 遮罩线程 {:.1}s 未响应，任务未被执行",
                        self.shared.config.thread_timeout.as_secs_f64()
                    );
                    Err(MaskError::ThreadTimeoutNotRun)
                } else {
                    // 任务已经被线程取走，可能正卡在 CreateWindowExW/SetWindowPos 里。
                    // 绝不能再在调用线程执行一次：那会建出第二个无人跟踪的窗口。
                    eprintln!(
                        "[mask] 遮罩线程 {:.1}s 未完成，任务仍在执行；不再重复执行",
                        self.shared.config.thread_timeout.as_secs_f64()
                    );
                    Err(MaskError::ThreadTimeoutRunning)
                }
            }
        }
    }

    // -- 内部：剪辑 / 光标 -------------------------------------------------

    fn apply_clip(&self, public: &mut Public, rect: Option<Rect>) {
        let ok = match self.shared.api.clip_cursor(rect) {
            Ok(ok) => ok,
            Err(err) => {
                eprintln!("[mask] ClipCursor 调用失败：{err}");
                false
            }
        };
        let Some(rect) = rect else {
            public.clip_owned = false;
            public.clip_rect = None;
            return;
        };
        if ok {
            public.clip_owned = true;
            public.clip_rect = Some(rect);
        } else {
            // 锁不住不是致命错误：遮罩仍然显示，但要留下诊断线索。
            eprintln!("[mask] ClipCursor({rect:?}) 失败，遮罩已显示但光标未锁定");
            public.clip_owned = false;
            public.clip_rect = None;
        }
    }

    fn hide_cursor(&self, public: &mut Public) {
        if public.cursor_hidden {
            return;
        }
        match self.shared.api.set_cursor_hidden(true) {
            Ok(()) => public.cursor_hidden = true,
            // 隐藏失败不阻断接管：遮罩 + ClipCursor 仍生效，只是可能看到 PC 光标。
            Err(err) => eprintln!("[mask] 隐藏系统光标失败：{err}"),
        }
    }

    fn show_cursor(&self, public: &mut Public) {
        if !public.cursor_hidden {
            return;
        }
        if let Err(err) = self.shared.api.set_cursor_hidden(false) {
            eprintln!("[mask] 恢复系统光标失败：{err}");
        }
        public.cursor_hidden = false;
    }

    fn windows_snapshot(&self) -> Vec<isize> {
        lock(&self.shared.work).windows.clone()
    }

    /// 把已在使用的遮罩重新置顶；平台后端不支持时视为成功。
    fn reassert_topmost(&self) -> bool {
        let mut ok = true;
        for hwnd in self.windows_snapshot() {
            match self.shared.api.ensure_topmost(hwnd) {
                Ok(true) => {}
                Ok(false) => {
                    ok = false;
                    eprintln!("[mask] 遮罩重新置顶失败 hwnd={hwnd}");
                }
                Err(err) => {
                    ok = false;
                    eprintln!("[mask] 遮罩重新置顶异常 hwnd={hwnd}：{err}");
                }
            }
        }
        ok
    }

    /// 让所有遮罩立刻重设一次光标（不依赖用户移动鼠标）。
    fn refresh_all_cursors(&self) {
        for hwnd in self.windows_snapshot() {
            if let Err(err) = self.shared.api.refresh_cursor(hwnd) {
                eprintln!("[mask] 刷新遮罩光标失败 hwnd={hwnd}：{err}");
            }
        }
    }

    /// 每个遮罩的 topmost 状态；读不到为 `None`（诊断用）。
    fn topmost_state(&self) -> Vec<Option<bool>> {
        self.windows_snapshot()
            .into_iter()
            .map(|hwnd| self.shared.api.window_topmost(hwnd).ok().flatten())
            .collect()
    }

    // -- 内部：看门狗 -----------------------------------------------------

    /// 接管期间每秒复查遮罩是否仍置顶、光标是否仍藏住（1.1.1）。
    ///
    /// 1.1.1 的现场故障是：遮罩偶尔拿不到 / 丢失 `WS_EX_TOPMOST`（`SetWindowPos` 返回成功
    /// 却没生效），任务栏压在遮罩上面，PC 光标就重新显示出来。遮罩一旦丢了顶层位置，
    /// 只能靠定期回读 + 重设兜底，不能指望某一次调用一定成功。
    fn start_watchdog(&self) {
        let mut slot = lock(&self.watchdog);
        if let Some(watchdog) = slot.as_ref() {
            if watchdog.alive.load(Ordering::SeqCst) {
                return;
            }
        }
        let (tx, rx) = mpsc::channel::<()>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_in_thread = Arc::clone(&alive);
        let shared = Arc::clone(&self.shared);
        let interval = self.shared.config.watchdog_interval;
        let spawned = thread::Builder::new()
            .name("padlend-mask-watch".to_string())
            .spawn(move || {
                loop {
                    match rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                watchdog_tick(&shared);
                            }))
                            .is_err()
                            {
                                // 看门狗自身永不致命。
                                eprintln!("[mask] 遮罩看门狗异常");
                            }
                        }
                        _ => break,
                    }
                }
                alive_in_thread.store(false, Ordering::SeqCst);
            });
        match spawned {
            Ok(_handle) => {
                *slot = Some(Watchdog { stop: Some(tx), alive });
            }
            Err(err) => {
                eprintln!("[mask] 无法创建遮罩看门狗线程：{err}");
            }
        }
    }

    fn stop_watchdog(&self) {
        // 不 join：看门狗 tick 要抢 `public` 锁，而调用方正持有它（Python 同样只置停止位）。
        // 丢弃 Sender 后线程从 recv_timeout 立刻返回并退出。
        let mut slot = lock(&self.watchdog);
        if let Some(mut watchdog) = slot.take() {
            watchdog.stop.take();
        }
    }

    /// 单次复查（对应 Python `_watchdog_tick`）：诊断与单测可直接调用。
    pub fn watchdog_tick(&self) {
        watchdog_tick(&self.shared);
    }

    // -- 内部：失败回滚 ---------------------------------------------------

    fn cleanup_quietly(&self, public: &mut Public, unclip: bool) {
        self.stop_watchdog();
        self.show_cursor(public);
        if unclip && public.clip_owned {
            if let Err(err) = self.shared.api.clip_cursor(None) {
                eprintln!("[mask] 回滚 ClipCursor(None) 失败：{err}");
            }
            public.clip_owned = false;
            public.clip_rect = None;
        }
        self.destroy_windows(public);
        public.active = false;
    }
}

impl Drop for CursorMask {
    fn drop(&mut self) {
        self.release();
        // 遮罩线程是 daemon（Python 同样不 join，也没有停止信号）：只把句柄 detach，
        // 绝不在这里 join —— 消息泵是死循环，join 会挂住析构。
        let mut slot = lock(&self.shared.thread);
        let _ = slot.handle.take();
    }
}

// ---------------------------------------------------------------------------
// 看门狗与遮罩线程的裸函数（只依赖 Arc<Shared>，不借用 CursorMask）
// ---------------------------------------------------------------------------

fn drain_mask_tasks(shared: &Shared) {
    loop {
        let task = {
            let mut work = lock(&shared.work);
            work.queue.pop_front()
        };
        let Some(task) = task else { break };
        task.taken.store(true, Ordering::SeqCst);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task.func));
        let result = match outcome {
            Ok(result) => result,
            Err(payload) => Err(MaskError::TaskFailed {
                message: panic_message(payload),
            }),
        };
        let _ = task.done.send(result);
    }
}

fn mask_thread_main(shared: Arc<Shared>, loop_fn: MaskMessageLoop, exited: Arc<AtomicBool>) {
    {
        let mut slot = lock(&shared.thread);
        slot.id = Some(thread::current().id());
    }
    let idle_shared = Arc::clone(&shared);
    let mut idle = move || drain_mask_tasks(&idle_shared);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop_fn(&mut idle)));
    {
        let mut slot = lock(&shared.thread);
        slot.id = None;
        if outcome.is_err() {
            slot.failed = true;
        }
    }
    if let Err(payload) = outcome {
        eprintln!("[mask] 遮罩线程消息泵异常退出：{}", panic_message(payload));
    }
    exited.store(true, Ordering::SeqCst);
    // 对应 `_fail_pending_tasks`：丢弃队列里的任务，发送端随之销毁，
    // 等待中的调用方立刻收到 Disconnected（映射为 ThreadExited）。
    let pending: Vec<Task> = {
        let mut work = lock(&shared.work);
        work.queue.drain(..).collect()
    };
    drop(pending);
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知 panic".to_string()
    }
}

fn watchdog_tick(shared: &Shared) {
    {
        let public = lock(&shared.public);
        if !public.active || lock(&shared.work).windows.is_empty() {
            return;
        }
    }
    let mut repaired = false;
    // 快照必须先取到局部变量：`for .. in lock(..)` 会把 MutexGuard 的生命周期延长到
    // 整个循环体，循环里再取同一把锁就是重入死锁（std 的 Mutex 不可重入）。
    let windows = lock(&shared.work).windows.clone();
    for hwnd in windows {
        match shared.api.window_topmost(hwnd) {
            Ok(Some(false)) => {
                reassert_topmost(shared);
                repaired = true;
                break;
            }
            _ => {}
        }
    }
    let visible = shared.api.cursor_visible().ok().flatten();
    if visible == Some(true) {
        // 光标又露出来了：让遮罩立刻重设一次光标（顶层被抢是最常见原因，上面已经修过；
        // 这里兜住其它把光标显示回来的进程）。
        refresh_all_cursors(shared);
        repaired = true;
    }
    if repaired {
        log_repair(shared, visible);
    }
}

fn reassert_topmost(shared: &Shared) {
    let windows = lock(&shared.work).windows.clone();
    for hwnd in windows {
        match shared.api.ensure_topmost(hwnd) {
            Ok(true) => {}
            Ok(false) => eprintln!("[mask] 遮罩重新置顶失败 hwnd={hwnd}"),
            Err(err) => eprintln!("[mask] 遮罩重新置顶异常 hwnd={hwnd}：{err}"),
        }
    }
}

fn refresh_all_cursors(shared: &Shared) {
    let windows = lock(&shared.work).windows.clone();
    for hwnd in windows {
        if let Err(err) = shared.api.refresh_cursor(hwnd) {
            eprintln!("[mask] 刷新遮罩光标失败 hwnd={hwnd}：{err}");
        }
    }
}

fn log_repair(shared: &Shared, visible: Option<bool>) {
    let mut public = lock(&shared.public);
    let now = Instant::now();
    if let Some(last) = public.repair_log_at {
        if now.saturating_duration_since(last) < shared.config.repair_log_interval {
            return;
        }
    }
    public.repair_log_at = Some(now);
    let topmost = topmost_state(shared);
    let active = public.active;
    drop(public);
    eprintln!(
        "[mask] mask.repair topmost={} visible={} active={}",
        py_topmost_list(&topmost),
        py_opt_bool(visible),
        active
    );
}

fn topmost_state(shared: &Shared) -> Vec<Option<bool>> {
    lock(&shared.work)
        .windows
        .iter()
        .map(|hwnd| shared.api.window_topmost(*hwnd).ok().flatten())
        .collect()
}

// ---------------------------------------------------------------------------
// Python repr 兼容的小工具（诊断文本要能直接和 Python 日志 diff）
// ---------------------------------------------------------------------------

fn py_opt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "True",
        Some(false) => "False",
        None => "None",
    }
}

fn py_opt_rect(value: Option<Rect>) -> String {
    match value {
        None => "None".to_string(),
        Some(rect) => format!(
            "Rect(left={}, top={}, right={}, bottom={})",
            rect.left, rect.top, rect.right, rect.bottom
        ),
    }
}

fn py_topmost_list(values: &[Option<bool>]) -> String {
    let inner: Vec<&str> = values.iter().map(|value| py_opt_bool(*value)).collect();
    format!("[{}]", inner.join(", "))
}

// ---------------------------------------------------------------------------
// 单测（逐条对应 tests/test_mask.py，共 40 个用例；仅 real_* 两个 #[ignore]）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicIsize, AtomicUsize};
    use std::sync::Condvar;

    const MON_A: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    const MON_B: Rect = Rect {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1080,
    };
    const BAND_LEFT: Rect = Rect {
        left: 0,
        top: 0,
        right: 1,
        bottom: 1080,
    };

    // -- 假后端 -----------------------------------------------------------

    /// 记录调用的假后端；可注入故障，用来验证失败路径与幂等性。
    struct FakeState {
        monitors: Vec<Rect>,
        /// 打开消息泵（对应 pytest 的 `ThreadedFakeMaskApi`）。
        threaded: AtomicBool,
        /// 消息泵卡死：不再取任务（对应 `_StalledApi`）。
        park_pump: AtomicBool,
        pump_stop: Arc<AtomicBool>,
        pump_thread_id: Mutex<Option<ThreadId>>,
        /// 建窗闸门：模拟 `CreateWindowExW` 被卡住（对应 `_SlowCreateApi`）。
        create_gate: Mutex<Option<Arc<Gate>>>,
        fail_create_at: Mutex<Option<usize>>,
        fail_destroy: AtomicBool,
        fail_configure: AtomicBool,
        fail_clip: AtomicBool,
        clip_ok: AtomicBool,
        topmost_repair_ok: AtomicBool,
        created: Mutex<Vec<Rect>>,
        destroyed: Mutex<Vec<isize>>,
        close_posted: Mutex<Vec<isize>>,
        clip_calls: Mutex<Vec<Option<Rect>>>,
        cursor_hidden_calls: Mutex<Vec<bool>>,
        cursor_refreshes: Mutex<Vec<isize>>,
        ensure_topmost_calls: Mutex<Vec<isize>>,
        topmost: Mutex<HashMap<isize, bool>>,
        cursor_shown: AtomicBool,
        hidden: Mutex<Vec<isize>>,
        alive: Mutex<HashSet<isize>>,
        create_threads: Mutex<Vec<ThreadId>>,
        destroy_threads: Mutex<Vec<ThreadId>>,
        next_handle: AtomicIsize,
    }

    /// 可等待的闸门。
    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }

        fn wait(&self, timeout: Duration) {
            let guard = self.open.lock().unwrap();
            let _ = self
                .cv
                .wait_timeout_while(guard, timeout, |open| !*open)
                .unwrap();
        }
    }

    #[derive(Clone)]
    struct FakeMaskApi {
        st: Arc<FakeState>,
    }

    impl FakeMaskApi {
        fn new(monitors: &[Rect]) -> Self {
            Self {
                st: Arc::new(FakeState {
                    monitors: monitors.to_vec(),
                    threaded: AtomicBool::new(false),
                    park_pump: AtomicBool::new(false),
                    pump_stop: Arc::new(AtomicBool::new(false)),
                    pump_thread_id: Mutex::new(None),
                    create_gate: Mutex::new(None),
                    fail_create_at: Mutex::new(None),
                    fail_destroy: AtomicBool::new(false),
                    fail_configure: AtomicBool::new(false),
                    fail_clip: AtomicBool::new(false),
                    clip_ok: AtomicBool::new(true),
                    topmost_repair_ok: AtomicBool::new(true),
                    created: Mutex::new(Vec::new()),
                    destroyed: Mutex::new(Vec::new()),
                    close_posted: Mutex::new(Vec::new()),
                    clip_calls: Mutex::new(Vec::new()),
                    cursor_hidden_calls: Mutex::new(Vec::new()),
                    cursor_refreshes: Mutex::new(Vec::new()),
                    ensure_topmost_calls: Mutex::new(Vec::new()),
                    topmost: Mutex::new(HashMap::new()),
                    cursor_shown: AtomicBool::new(false),
                    hidden: Mutex::new(Vec::new()),
                    alive: Mutex::new(HashSet::new()),
                    create_threads: Mutex::new(Vec::new()),
                    destroy_threads: Mutex::new(Vec::new()),
                    next_handle: AtomicIsize::new(100),
                }),
            }
        }

        fn created(&self) -> Vec<Rect> {
            self.st.created.lock().unwrap().clone()
        }

        fn destroyed(&self) -> Vec<isize> {
            self.st.destroyed.lock().unwrap().clone()
        }

        fn close_posted(&self) -> Vec<isize> {
            self.st.close_posted.lock().unwrap().clone()
        }

        fn clip_calls(&self) -> Vec<Option<Rect>> {
            self.st.clip_calls.lock().unwrap().clone()
        }

        fn cursor_hidden_calls(&self) -> Vec<bool> {
            self.st.cursor_hidden_calls.lock().unwrap().clone()
        }

        fn cursor_refreshes(&self) -> Vec<isize> {
            self.st.cursor_refreshes.lock().unwrap().clone()
        }

        fn ensure_topmost_calls(&self) -> Vec<isize> {
            self.st.ensure_topmost_calls.lock().unwrap().clone()
        }

        fn hidden(&self) -> Vec<isize> {
            self.st.hidden.lock().unwrap().clone()
        }

        fn alive(&self) -> HashSet<isize> {
            self.st.alive.lock().unwrap().clone()
        }

        fn create_threads(&self) -> Vec<ThreadId> {
            self.st.create_threads.lock().unwrap().clone()
        }

        fn destroy_threads(&self) -> Vec<ThreadId> {
            self.st.destroy_threads.lock().unwrap().clone()
        }

        fn pump_thread(&self) -> Option<ThreadId> {
            *self.st.pump_thread_id.lock().unwrap()
        }

        fn topmost_of(&self, hwnd: isize) -> Option<bool> {
            self.st.topmost.lock().unwrap().get(&hwnd).copied()
        }

        /// 假后端最后一次成功设置的光标裁剪（对应 FakeMaskApi.clip_rect）。
        fn current_clip(&self) -> Option<Rect> {
            self.st
                .clip_calls
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|rect| *rect)
        }
    }

    impl MaskApi for FakeMaskApi {
        fn monitor_rects(&self) -> Result<Vec<Rect>, MaskError> {
            Ok(self.st.monitors.clone())
        }

        fn create_mask_window(&self, rect: Rect) -> Result<isize, MaskError> {
            self.st
                .create_threads
                .lock()
                .unwrap()
                .push(thread::current().id());
            let fail_at = *self.st.fail_create_at.lock().unwrap();
            if let Some(at) = fail_at {
                if self.st.created.lock().unwrap().len() == at {
                    return Err(MaskError::Backend {
                        message: "CreateWindowExW 失败（测试注入）".to_string(),
                    });
                }
            }
            let handle = self.st.next_handle.fetch_add(1, Ordering::SeqCst);
            self.st.created.lock().unwrap().push(rect);
            self.st.topmost.lock().unwrap().insert(handle, true);
            self.st.alive.lock().unwrap().insert(handle);
            let gate = self.st.create_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait(Duration::from_secs(2));
            }
            Ok(handle)
        }

        fn configure_mask_window(&self, hwnd: isize, rect: Rect) -> Result<(), MaskError> {
            let _ = rect;
            assert!(
                self.st.alive.lock().unwrap().contains(&hwnd),
                "配置前窗口必须已登记: {hwnd}"
            );
            if self.st.fail_configure.load(Ordering::SeqCst) {
                return Err(MaskError::Backend {
                    message: "SetWindowPos 失败（测试注入）".to_string(),
                });
            }
            self.st.topmost.lock().unwrap().insert(hwnd, true);
            Ok(())
        }

        fn destroy_window(&self, hwnd: isize) -> Result<(), MaskError> {
            self.st
                .destroy_threads
                .lock()
                .unwrap()
                .push(thread::current().id());
            if self.st.fail_destroy.load(Ordering::SeqCst) {
                return Err(MaskError::Backend {
                    message: "DestroyWindow 失败（测试注入）".to_string(),
                });
            }
            self.st.destroyed.lock().unwrap().push(hwnd);
            self.st.alive.lock().unwrap().remove(&hwnd);
            Ok(())
        }

        fn clip_cursor(&self, rect: Option<Rect>) -> Result<bool, MaskError> {
            self.st.clip_calls.lock().unwrap().push(rect);
            if self.st.fail_clip.load(Ordering::SeqCst) {
                return Err(MaskError::Backend {
                    message: "ClipCursor 失败（测试注入）".to_string(),
                });
            }
            Ok(self.st.clip_ok.load(Ordering::SeqCst))
        }

        fn clip_rect(&self) -> Result<Option<Rect>, MaskError> {
            Ok(self.current_clip())
        }

        fn set_cursor_hidden(&self, hidden: bool) -> Result<(), MaskError> {
            self.st.cursor_hidden_calls.lock().unwrap().push(hidden);
            self.st.cursor_shown.store(!hidden, Ordering::SeqCst);
            Ok(())
        }

        fn cursor_visible(&self) -> Result<Option<bool>, MaskError> {
            Ok(Some(self.st.cursor_shown.load(Ordering::SeqCst)))
        }

        fn ensure_topmost(&self, hwnd: isize) -> Result<bool, MaskError> {
            self.st
                .ensure_topmost_calls
                .lock()
                .unwrap()
                .push(hwnd);
            if !self.st.topmost_repair_ok.load(Ordering::SeqCst) {
                return Ok(false);
            }
            self.st.topmost.lock().unwrap().insert(hwnd, true);
            Ok(true)
        }

        fn window_topmost(&self, hwnd: isize) -> Result<Option<bool>, MaskError> {
            Ok(self.topmost_of(hwnd))
        }

        fn refresh_cursor(&self, hwnd: isize) -> Result<(), MaskError> {
            self.st.cursor_refreshes.lock().unwrap().push(hwnd);
            Ok(())
        }

        fn post_close_window(&self, hwnd: isize) -> Result<bool, MaskError> {
            self.st.close_posted.lock().unwrap().push(hwnd);
            Ok(true)
        }

        fn hide_window(&self, hwnd: isize) -> Result<(), MaskError> {
            self.st.hidden.lock().unwrap().push(hwnd);
            Ok(())
        }

        fn mask_message_loop(&self) -> Option<MaskMessageLoop> {
            if !self.st.threaded.load(Ordering::SeqCst) {
                return None;
            }
            let st = Arc::clone(&self.st);
            Some(Arc::new(move |idle: &mut dyn FnMut()| {
                *st.pump_thread_id.lock().unwrap() = Some(thread::current().id());
                while !st.pump_stop.load(Ordering::SeqCst) {
                    if st.park_pump.load(Ordering::SeqCst) {
                        // 消息泵卡死：永远不再取任务（pytest `_StalledApi`）。
                        loop {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    idle();
                    thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            }))
        }
    }

    fn make_mask(api: &FakeMaskApi) -> CursorMask {
        CursorMask::new(Arc::new(api.clone()))
    }

    // -----------------------------------------------------------------------
    // show / hide
    // -----------------------------------------------------------------------

    #[test]
    fn show_creates_one_window_per_monitor() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();

        assert!(mask.active());
        assert_eq!(api.created(), vec![MON_A, MON_B]);
        // None 语义 = 只显示遮罩，不改动当前 ClipCursor（6.6）
        assert_eq!(api.clip_calls(), Vec::<Option<Rect>>::new());
        assert!(!mask.clip_owned());
        assert_eq!(mask.clip_rect(), None);
    }

    #[test]
    fn show_with_clip_rect_locks_cursor() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();

        assert_eq!(api.clip_calls(), vec![Some(BAND_LEFT)]);
        assert!(mask.clip_owned());
        assert_eq!(mask.clip_rect(), Some(BAND_LEFT));
        assert!(mask.active());
    }

    #[test]
    fn repeated_show_does_not_rebuild_windows() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();
        mask.show(None).unwrap();
        mask.show(Some(BAND_LEFT)).unwrap();

        assert_eq!(api.created().len(), 2);
        assert!(api.destroyed().is_empty());
    }

    #[test]
    fn show_none_after_clip_releases_clip() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();
        mask.show(None).unwrap();

        assert_eq!(api.clip_calls(), vec![Some(BAND_LEFT), None]);
        assert!(!mask.clip_owned());
        assert!(mask.active());
    }

    #[test]
    fn hide_destroys_windows_and_unclips() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();
        mask.hide();

        assert_eq!(api.destroyed(), vec![100, 101]);
        assert_eq!(api.clip_calls(), vec![Some(BAND_LEFT), None]);
        assert!(!mask.active());
        assert!(!mask.clip_owned());
        assert_eq!(mask.clip_rect(), None);
    }

    #[test]
    fn hide_without_show_is_noop() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.hide();

        assert!(api.created().is_empty());
        assert!(api.destroyed().is_empty());
        assert!(api.clip_calls().is_empty());
        assert!(!mask.active());
    }

    #[test]
    fn show_after_hide_creates_windows_again() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();
        mask.hide();
        mask.show(None).unwrap();

        assert_eq!(api.created().len(), 2);
        assert!(mask.active());
    }

    // -----------------------------------------------------------------------
    // 失败与兜底
    // -----------------------------------------------------------------------

    #[test]
    fn show_rolls_back_when_window_creation_fails() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        *api.st.fail_create_at.lock().unwrap() = Some(1);
        let mask = make_mask(&api);

        assert!(mask.show(Some(BAND_LEFT)).is_err());

        assert!(!mask.active());
        assert!(!mask.clip_owned());
        assert_eq!(api.clip_calls(), Vec::<Option<Rect>>::new());
        // 已创建的第一个窗口被回滚
        assert_eq!(api.destroyed(), vec![100]);
        // 回滚后再 show 仍然可用
        *api.st.fail_create_at.lock().unwrap() = None;
        mask.show(None).unwrap();
        assert!(mask.active());
        assert_eq!(api.created().len(), 3);
    }

    #[test]
    fn show_keeps_mask_when_clip_fails() {
        let api = FakeMaskApi::new(&[MON_A]);
        api.st.clip_ok.store(false, Ordering::SeqCst);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();

        assert!(mask.active());
        assert!(!mask.clip_owned());
        assert_eq!(mask.clip_rect(), None);
        assert_eq!(api.clip_calls(), vec![Some(BAND_LEFT)]);
    }

    #[test]
    fn show_keeps_mask_when_clip_raises() {
        let api = FakeMaskApi::new(&[MON_A]);
        api.st.fail_clip.store(true, Ordering::SeqCst);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();

        assert!(mask.active());
        assert!(!mask.clip_owned());
    }

    #[test]
    fn release_is_idempotent() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();
        mask.release();
        mask.release();
        mask.release();

        assert!(!mask.active());
        assert!(!mask.clip_owned());
        assert_eq!(mask.clip_rect(), None);
        assert_eq!(api.destroyed(), vec![100, 101]);
        // 每次 release 都无条件 ClipCursor(None)：宁可多解一次，绝不泄漏锁定
        assert_eq!(
            api.clip_calls(),
            vec![Some(BAND_LEFT), None, None, None]
        );
    }

    #[test]
    fn release_without_show_still_unclips() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.release();

        assert_eq!(api.clip_calls(), vec![None]);
        assert!(api.destroyed().is_empty());
        assert!(!mask.active());
    }

    #[test]
    fn release_survives_api_failures() {
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        api.st.fail_clip.store(true, Ordering::SeqCst);
        api.st.fail_destroy.store(true, Ordering::SeqCst);
        let mask = make_mask(&api);

        mask.show(None).unwrap();
        mask.release(); // 不抛
        mask.release(); // 仍然不抛，且内部状态已清空

        assert!(!mask.active());
        assert!(!mask.clip_owned());
        assert_eq!(api.clip_calls().len(), 2);
        assert!(api.destroyed().is_empty());
    }

    #[test]
    fn release_then_show_works_again() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();
        mask.release();
        mask.show(Some(BAND_LEFT)).unwrap();

        assert!(mask.active());
        assert!(mask.clip_owned());
        assert_eq!(api.current_clip(), Some(BAND_LEFT));
    }

    #[test]
    fn show_registers_atexit_guard() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();

        assert!(atexit_guard_registered());
    }

    #[test]
    fn ex_style_matches_design() {
        // 接管期遮罩必须接收 WM_SETCURSOR，所以不再使用 WS_EX_TRANSPARENT。
        // 1.1.1：样式里必须带 WS_EX_TOPMOST，否则任务栏会盖住遮罩、PC 光标重新显示。
        let expected = WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
        assert_eq!(expected, CursorMask::EX_STYLE);
        assert!(CursorMask::EX_STYLE & WS_EX_TRANSPARENT == 0);
    }

    #[test]
    fn show_refreshes_cursor_without_mouse_move() {
        // 1.1.1：遮罩创建后不能等用户动鼠标才执行 WM_SETCURSOR，必须立即投递一次。
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(Some(BAND_LEFT)).unwrap();

        assert_eq!(api.cursor_refreshes(), vec![100]);
        mask.hide();
    }

    #[test]
    fn repeated_show_reasserts_topmost() {
        // 两次接管之间系统可能把 topmost 抢走；复用窗口时必须重新置顶。
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();
        assert_eq!(api.ensure_topmost_calls(), Vec::<isize>::new());

        api.st.topmost.lock().unwrap().insert(100, false);
        mask.show(None).unwrap();

        assert_eq!(api.ensure_topmost_calls(), vec![100]);
        assert_eq!(api.topmost_of(100), Some(true));
        mask.hide();
    }

    #[test]
    fn watchdog_repairs_topmost_and_cursor() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);
        mask.show(Some(BAND_LEFT)).unwrap();
        api.st.cursor_refreshes.lock().unwrap().clear();

        api.st.topmost.lock().unwrap().insert(100, false);
        api.st.cursor_shown.store(true, Ordering::SeqCst); // 模拟别的进程把系统光标显示回来
        mask.watchdog_tick();

        assert_eq!(api.ensure_topmost_calls(), vec![100]);
        assert_eq!(api.topmost_of(100), Some(true));
        assert_eq!(api.cursor_refreshes(), vec![100]);
        mask.hide();
    }

    #[test]
    fn watchdog_tick_is_noop_before_show() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.watchdog_tick();

        assert_eq!(api.ensure_topmost_calls(), Vec::<isize>::new());
        assert_eq!(api.cursor_refreshes(), Vec::<isize>::new());
    }

    #[test]
    fn watchdog_stops_on_hide() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);
        mask.show(None).unwrap();
        // Python 直接 join 线程；Rust 侧 JoinHandle 在停止时被丢弃（不 join，避免与
        // 看门狗抢同一把锁死锁），改用 alive 标志等它退出。
        let alive = {
            let slot = mask.watchdog.lock().unwrap();
            Arc::clone(&slot.as_ref().expect("看门狗应已启动").alive)
        };
        assert!(alive.load(Ordering::SeqCst));

        mask.hide();

        assert!(mask.watchdog.lock().unwrap().is_none());
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!alive.load(Ordering::SeqCst));
    }

    #[test]
    fn diagnose_reports_topmost() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);

        mask.show(None).unwrap();

        assert!(
            mask.diagnose().contains("topmost=[True]"),
            "{}",
            mask.diagnose()
        );
        mask.hide();
    }

    // -----------------------------------------------------------------------
    // 边缘 1px 条几何（show 的 clip_rect 由 Router 用同名纯函数算出）
    // -----------------------------------------------------------------------

    #[test]
    fn clip_rect_for_full_edge() {
        let cases = [
            (Edge::Left, 1080, Rect::new(0, 0, 1, 1080)),
            (Edge::Right, 1080, Rect::new(1919, 0, 1920, 1080)),
            (Edge::Top, 1920, Rect::new(0, 0, 1920, 1)),
            (Edge::Bottom, 1920, Rect::new(0, 1079, 1920, 1080)),
        ];
        for (edge, span_end, expected) in cases {
            let segment = EdgeSegment::new(1, edge, 0, span_end).unwrap();
            assert_eq!(
                clip_rect_for_segment(MON_A, &segment, None),
                expected,
                "{edge}"
            );
        }
    }

    #[test]
    fn clip_rect_respects_span() {
        let segment = EdgeSegment::new(1, Edge::Left, 100, 300).unwrap();
        assert_eq!(
            clip_rect_for_segment(MON_A, &segment, None),
            Rect::new(0, 100, 1, 300)
        );
    }

    #[test]
    fn clip_rect_clips_span_to_monitor() {
        let segment = EdgeSegment::new(1, Edge::Right, -50, 200).unwrap();
        assert_eq!(
            clip_rect_for_segment(MON_B, &segment, None),
            Rect::new(3839, 0, 3840, 200)
        );
    }

    #[test]
    fn clip_rect_falls_back_for_stale_span() {
        let segment = EdgeSegment::new(1, Edge::Left, 5000, 6000).unwrap();
        assert_eq!(
            clip_rect_for_segment(MON_A, &segment, None),
            Rect::new(0, 0, 1, 1080)
        );
    }

    #[test]
    fn clip_rect_uses_thickness_override() {
        let segment = EdgeSegment::new(1, Edge::Left, 0, 1080).unwrap();
        assert_eq!(
            clip_rect_for_segment(MON_A, &segment, Some(3)),
            Rect::new(0, 0, 3, 1080)
        );
    }

    #[test]
    fn clip_rect_handles_negative_origin_monitor() {
        let monitor = Rect::new(-1920, 0, 0, 1080);
        let left = EdgeSegment::new(1, Edge::Left, 0, 1080).unwrap();
        let right = EdgeSegment::new(1, Edge::Right, 0, 1080).unwrap();
        assert_eq!(
            clip_rect_for_segment(monitor, &left, None),
            Rect::new(-1920, 0, -1919, 1080)
        );
        assert_eq!(
            clip_rect_for_segment(monitor, &right, None),
            Rect::new(-1, 0, 0, 1080)
        );
    }

    // -----------------------------------------------------------------------
    // 真实 Win32（默认跳过：会建窗 / 改系统光标状态）
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "会调用真实 ClipCursor（锁屏环境需手工验证）"]
    fn real_mask_release_is_safe() {
        let mask = CursorMask::new(Arc::new(Win32MaskApi));
        mask.release();
        mask.release();
        assert!(!mask.active());
        assert!(!mask.clip_owned());
    }

    #[test]
    #[ignore = "会真实建窗（需交互式桌面，PADLEND_UI_TESTS=1 的手工验证项）"]
    fn real_mask_show_and_hide() {
        let api: Arc<dyn MaskApi> = Arc::new(Win32MaskApi);
        let mask = CursorMask::new(Arc::clone(&api));
        let shown = mask.show(None);
        assert!(shown.is_ok(), "{shown:?}");
        assert!(mask.active());
        assert!(!api.monitor_rects().unwrap().is_empty());
        mask.release();
        assert!(!mask.active());
        assert!(!mask.clip_owned());
    }

    #[test]
    fn cursor_hidden_while_mask_active_and_restored() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);
        mask.show(None).unwrap();
        mask.show(None).unwrap(); // 重复 show 只隐藏一次，hide 只恢复一次
        assert_eq!(api.cursor_hidden_calls(), vec![true]);
        mask.hide();
        assert_eq!(api.cursor_hidden_calls(), vec![true, false]);
        mask.hide(); // 幂等
        assert_eq!(api.cursor_hidden_calls(), vec![true, false]);
    }

    #[test]
    fn release_restores_cursor() {
        let api = FakeMaskApi::new(&[MON_A]);
        let mask = make_mask(&api);
        mask.show(Some(BAND_LEFT)).unwrap();
        mask.release();
        assert_eq!(api.cursor_hidden_calls(), vec![true, false]);
    }

    /// `ShowCursor` 是进程级计数；任何一次跨线程失衡都会让光标永久消失。
    ///
    /// Python 用 monkeypatch 换掉整个 `_user32`；这里换掉 [`CursorOps`]，断言调用
    /// 顺序恰好是"置空 → 取箭头 → 设箭头"，中间没有任何 `ShowCursor`。
    #[test]
    fn set_cursor_hidden_never_touches_show_cursor() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        enum CursorCall {
            SetCursor(Option<isize>),
            LoadArrow,
        }

        #[derive(Default)]
        struct FakeCursorOps {
            calls: Mutex<Vec<CursorCall>>,
        }

        impl CursorOps for FakeCursorOps {
            fn set_cursor(&self, handle: Option<isize>) -> Result<(), MaskError> {
                self.calls.lock().unwrap().push(CursorCall::SetCursor(handle));
                Ok(())
            }

            fn load_arrow(&self) -> Result<isize, MaskError> {
                self.calls.lock().unwrap().push(CursorCall::LoadArrow);
                Ok(123)
            }
        }

        let ops = FakeCursorOps::default();
        set_cursor_hidden_with(&ops, true).unwrap();
        set_cursor_hidden_with(&ops, false).unwrap();

        assert_eq!(
            ops.calls.lock().unwrap().clone(),
            vec![
                CursorCall::SetCursor(None),
                CursorCall::LoadArrow,
                CursorCall::SetCursor(Some(123)),
            ]
        );
        // LoadCursorW 的实参（箭头资源 ID）与 Python 的 IDC_ARROW 一致。
        assert_eq!(IDC_ARROW, 32512);
    }

    #[test]
    fn cross_thread_release_destroys_on_mask_thread() {
        // 1.1.2：UI/RPC 线程进入、rawinput 线程退出时，销窗必须落在遮罩线程。
        // 1.1.1 走 PostMessage(WM_CLOSE)，而 RPC 线程没有消息泵 → WM_CLOSE 永远不被
        // 处理，遮罩窗口泄漏：整块屏幕没有光标，而且吞掉所有点击。
        let api = FakeMaskApi::new(&[MON_A, MON_B]);
        api.st.threaded.store(true, Ordering::SeqCst);
        let mask = Arc::new(CursorMask::new(Arc::new(api.clone())));
        mask.show(None).unwrap();

        let pump = api.pump_thread().expect("消息泵应已启动");
        assert_eq!(api.create_threads(), vec![pump, pump]);
        assert_ne!(thread::current().id(), pump);

        let worker = {
            let mask = Arc::clone(&mask);
            thread::spawn(move || mask.release())
        };
        worker.join().unwrap();

        assert_eq!(api.destroyed(), vec![100, 101]);
        assert_eq!(api.destroy_threads(), vec![pump, pump]);
        // 不再依赖跨线程 PostMessage(WM_CLOSE)，也就不会漏窗。
        assert_eq!(api.close_posted(), Vec::<isize>::new());
        assert!(api.alive().is_empty());
        api.st.pump_stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn configure_failure_tears_down_registered_window() {
        // 配置阶段失败时，已登记的窗口必须被销毁，不能留下 topmost 孤儿。
        let api = FakeMaskApi::new(&[MON_A]);
        api.st.fail_configure.store(true, Ordering::SeqCst);
        let mask = make_mask(&api);

        assert!(mask.show(None).is_err());

        assert_eq!(api.created(), vec![MON_A]);
        assert_eq!(api.destroyed(), vec![100]);
        assert!(api.alive().is_empty());
        assert!(!mask.active());
    }

    #[test]
    fn create_timeout_does_not_build_a_second_window() {
        // 建窗任务已在线程上执行时，调用线程绝不能超时重跑、建出第二个窗口。
        let api = FakeMaskApi::new(&[MON_A]);
        api.st.threaded.store(true, Ordering::SeqCst);
        let gate = Gate::new();
        *api.st.create_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let mask = CursorMask::with_config(
            Arc::new(api.clone()),
            MaskConfig {
                thread_timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );

        assert!(mask.show(None).is_err());
        gate.open();
        mask.release();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !api.alive().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        api.st.pump_stop.store(true, Ordering::SeqCst);

        // 只有一个窗口被创建；迟到的建窗任务看到过期代次后自行销毁。
        assert_eq!(api.created(), vec![MON_A]);
        assert_eq!(api.destroyed(), vec![100]);
        assert!(api.alive().is_empty());
        assert!(!mask.active());
    }

    #[test]
    fn destroy_falls_back_to_hiding_when_mask_thread_stalls() {
        // 遮罩线程卡死时先 ShowWindow(SW_HIDE)，绝不让遮罩继续盖住屏幕。
        let api = FakeMaskApi::new(&[MON_A]);
        api.st.threaded.store(true, Ordering::SeqCst);
        let mask = CursorMask::with_config(
            Arc::new(api.clone()),
            MaskConfig {
                thread_timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );
        mask.show(None).unwrap();
        api.st.park_pump.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(10));
        mask.release();

        assert_eq!(api.hidden(), vec![100]);
        assert_eq!(api.close_posted(), vec![100]);
        assert!(!mask.active());
        api.st.pump_stop.store(true, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // 孤儿遮罩清理（对应用 fake user32/kernel32 的三个用例）
    // -----------------------------------------------------------------------

    /// 孤儿清理用的假 user32/kernel32。
    ///
    /// 与 Python 的差异：`OpenProcess`/`TerminateProcess`/`CloseHandle` 三步在
    /// [`OrphanApi::terminate_process`] 里合并成一个调用，所以这里只能断言"终止了哪个
    /// pid"，不能逐条断言 `(0x0001, False, 4242)` / `(77, 1)` / `77`。
    #[derive(Default)]
    struct FakeOrphanApi {
        exists: AtomicBool,
        /// `PostMessage` 后窗口是否消失（`_StalledApi` 场景设为 false）。
        post_clears: AtomicBool,
        hwnd: isize,
        pid: u32,
        hides: AtomicUsize,
        posts: AtomicUsize,
        unclips: AtomicUsize,
        terminated: Mutex<Vec<u32>>,
    }

    impl FakeOrphanApi {
        fn new(pid: u32) -> Self {
            Self {
                exists: AtomicBool::new(true),
                post_clears: AtomicBool::new(true),
                hwnd: 123,
                pid,
                ..Default::default()
            }
        }
    }

    impl OrphanApi for FakeOrphanApi {
        fn find_window(&self, _class: &str, _title: &str) -> Option<isize> {
            if self.exists.load(Ordering::SeqCst) {
                Some(self.hwnd)
            } else {
                None
            }
        }

        fn window_process_id(&self, _hwnd: isize) -> u32 {
            self.pid
        }

        fn hide_window(&self, _hwnd: isize) {
            self.hides.fetch_add(1, Ordering::SeqCst);
        }

        fn post_close(&self, _hwnd: isize) -> bool {
            self.posts.fetch_add(1, Ordering::SeqCst);
            if self.post_clears.load(Ordering::SeqCst) {
                self.exists.store(false, Ordering::SeqCst);
            }
            true
        }

        fn unclip(&self) {
            self.unclips.fetch_add(1, Ordering::SeqCst);
        }

        fn terminate_process(&self, pid: u32) -> bool {
            self.terminated.lock().unwrap().push(pid);
            true
        }

        /// 对应 pytest 里被 monkeypatch 成空操作的 `time.sleep`。
        fn poll_delay(&self) {}
    }

    #[test]
    fn cleanup_orphan_mask_hides_and_closes_window() {
        let api = FakeOrphanApi::new(4242);

        assert_eq!(cleanup_orphan_mask_with(&api), Some(4242));
        assert_eq!(api.hides.load(Ordering::SeqCst), 1);
        assert_eq!(api.posts.load(Ordering::SeqCst), 1);
        assert!(api.terminated.lock().unwrap().is_empty());
        assert_eq!(api.unclips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_orphan_mask_terminates_stuck_sidecar() {
        let api = FakeOrphanApi::new(4242);
        api.post_clears.store(false, Ordering::SeqCst);

        assert_eq!(cleanup_orphan_mask_with(&api), Some(4242));
        assert_eq!(api.terminated.lock().unwrap().clone(), vec![4242]);
        assert_eq!(api.unclips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_orphan_mask_ignores_own_process() {
        let api = FakeOrphanApi::new(std::process::id());

        assert_eq!(cleanup_orphan_mask_with(&api), None);
        assert_eq!(api.hides.load(Ordering::SeqCst), 0);
        assert_eq!(api.unclips.load(Ordering::SeqCst), 0);
    }
}
