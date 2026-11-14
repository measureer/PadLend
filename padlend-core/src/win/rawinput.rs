//! Raw Input 捕获（对应 `win/rawinput.py`）。
//!
//! 结构与 Python 版一致：**专用线程**创建 message-only 窗口（`HWND_MESSAGE`）并跑
//! `GetMessage` 循环，用 `RIDEV_INPUTSINK` 注册鼠标，窗口没有焦点也能收到输入。
//! 计数走 `AtomicU64`；窗口与统计对象的绑定走 `GWLP_USERDATA`（不依赖全局变量，
//! 多实例/测试互不干扰）。

use std::io;
use std::mem::{size_of, MaybeUninit};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, RegisterClassExW, SetWindowLongPtrW, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_INPUT,
    WNDCLASSEXW,
};

/// Raw Input 事件计数（与 Python `--debug-raw` 的口径一致）。
#[derive(Debug, Default)]
pub struct Stats {
    pub events: AtomicU64,
    pub moves: AtomicU64,
    pub wheel: AtomicU64,
    pub buttons: AtomicU64,
    pub absolute: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub events: u64,
    pub moves: u64,
    pub wheel: u64,
    pub buttons: u64,
    pub absolute: u64,
}

const CLASS_NAME: &str = "PadLendRawInput";

/// 服务模式的事件回调：每个解码后的鼠标包都交给它。
///
/// 用全局槽而不是窗口用户数据，是为了不动既有的 GWLP_USERDATA 统计绑定；
/// 进程内只会有一个 Raw Input 源（与 Python 的单实例假设一致）。
static HANDLER: std::sync::Mutex<
    Option<std::sync::Arc<dyn Fn(crate::decode::MousePacket) + Send + Sync>>,
> = std::sync::Mutex::new(None);

/// 一个已注册的 Raw Input 源；drop 时会向消息线程投递 `WM_CLOSE` 并 join。
pub struct RawInputSource {
    stats: Arc<Stats>,
    thread_id: u32,
    stopped: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RawInputSource {
    /// 在专用线程上建窗 + 注册 + 泵消息。
    /// 带回调启动：除计数外，把每个解码后的鼠标包交给 handler（服务模式用）。
    pub fn start_with(
        handler: std::sync::Arc<dyn Fn(crate::decode::MousePacket) + Send + Sync>,
    ) -> Result<Self, String> {
        {
            let mut slot = HANDLER.lock().unwrap_or_else(|err| err.into_inner());
            *slot = Some(handler);
        }
        Self::start()
    }

    pub fn start() -> Result<Self, String> {
        let stats = Arc::new(Stats::default());
        let stopped = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<u32, String>>();

        let thread_stats = Arc::clone(&stats);
        let thread_stopped = Arc::clone(&stopped);
        let join = std::thread::Builder::new()
            .name("padlend-rawinput".to_string())
            .spawn(move || {
                if let Err(err) = run_message_loop(thread_stats, thread_stopped, tx) {
                    eprintln!("[rawinput] 消息循环退出：{err}");
                }
            })
            .map_err(|err| format!("无法创建 Raw Input 线程：{err}"))?;

        let thread_id = match rx.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Raw Input 线程在启动前退出".to_string()),
        };

        Ok(Self {
            stats,
            thread_id,
            stopped,
            join: Some(join),
        })
    }

    pub fn count(&self) -> u64 {
        self.stats.events.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            events: self.stats.events.load(Ordering::Relaxed),
            moves: self.stats.moves.load(Ordering::Relaxed),
            wheel: self.stats.wheel.load(Ordering::Relaxed),
            buttons: self.stats.buttons.load(Ordering::Relaxed),
            absolute: self.stats.absolute.load(Ordering::Relaxed),
        }
    }

    pub fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(join) = self.join.take() {
            // 窗口属于消息线程，只能由它自己结束泵。
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread_id,
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            let _ = join.join();
        }
    }
}

impl Drop for RawInputSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            on_raw_input(hwnd, lparam);
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn stats_of(hwnd: HWND) -> Option<&'static Stats> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as usize;
    if raw == 0 {
        return None;
    }
    Some(unsafe { &*(raw as *const Stats) })
}

fn on_raw_input(hwnd: HWND, lparam: LPARAM) {
    let Some(stats) = stats_of(hwnd) else {
        return;
    };
    let handle = HRAWINPUT(lparam.0 as *mut std::ffi::c_void);
    let mut size: u32 = 0;
    let header_size = size_of::<RAWINPUTHEADER>() as u32;
    let probe = unsafe { GetRawInputData(handle, RID_INPUT, None, &mut size, header_size) };
    if probe == u32::MAX || size == 0 {
        return;
    }

    // RAWINPUT 需要 8 字节对齐，用 u64 缓冲而不是 Vec<u8>。
    let words = (size as usize).div_ceil(size_of::<u64>());
    let mut buffer = vec![0u64; words];
    let mut written = size;
    let got = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some(buffer.as_mut_ptr().cast()),
            &mut written,
            header_size,
        )
    };
    if got == u32::MAX {
        return;
    }

    // 解码只有一处实现（decode.rs），M0 的对拍向量走的也是它。
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, size as usize) };
    let Some(packet) = crate::decode::decode_raw_input(bytes) else {
        return;
    };

    // 先喂给服务层回调（锁要立刻释放，回调里会进 router 的锁）。
    let handler = {
        let slot = HANDLER.lock().unwrap_or_else(|err| err.into_inner());
        slot.clone()
    };
    if let Some(handler) = handler {
        static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !FIRST.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[rawinput] 首个事件 dx={} dy={} buttons={:#04x} abs={}",
                packet.x,
                packet.y,
                packet.button_flags,
                packet.is_absolute()
            );
        }
        handler(packet);
    }

    stats.events.fetch_add(1, Ordering::Relaxed);
    if packet.x != 0 || packet.y != 0 {
        stats.moves.fetch_add(1, Ordering::Relaxed);
    }
    if packet.has_wheel() {
        stats.wheel.fetch_add(1, Ordering::Relaxed);
    }
    if packet.has_button_change() {
        stats.buttons.fetch_add(1, Ordering::Relaxed);
    }
    if packet.is_absolute() {
        stats.absolute.fetch_add(1, Ordering::Relaxed);
    }
}

fn run_message_loop(
    stats: Arc<Stats>,
    stopped: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<u32, String>>,
) -> Result<(), String> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)
            .map(|module| HINSTANCE(module.0))
            .map_err(|err| format!("GetModuleHandleW 失败：{err}"))?;

        let class_name = wide(CLASS_NAME);
        let window_name = wide("padlend-rawinput");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hIconSm: Default::default(),
        };
        if RegisterClassExW(&wc) == 0 {
            let err = io::Error::last_os_error();
            let _ = ready.send(Err(format!("RegisterClassExW 失败：{err}")));
            return Err("RegisterClassExW 失败".to_string());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .map_err(|err| format!("CreateWindowExW 失败：{err}"))?;

        // 窗口与统计对象一对一绑定：wndproc 从 GWLP_USERDATA 取回指针。
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&stats) as isize);

        let devices = [RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        }];
        if let Err(err) = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) {
            let _ = ready.send(Err(format!("RegisterRawInputDevices 失败：{err}")));
            return Err("RegisterRawInputDevices 失败".to_string());
        }

        let thread_id = windows::Win32::System::Threading::GetCurrentThreadId();
        let _ = ready.send(Ok(thread_id));

        let mut msg = MaybeUninit::<MSG>::zeroed();
        loop {
            let ret = GetMessageW(msg.as_mut_ptr(), None, 0, 0).0;
            if ret == -1 || ret == 0 {
                break;
            }
            let message = msg.assume_init();
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        let remove = [RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        }];
        let _ = RegisterRawInputDevices(&remove, size_of::<RAWINPUTDEVICE>() as u32);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }

    stopped.store(true, Ordering::SeqCst);
    Ok(())
}
