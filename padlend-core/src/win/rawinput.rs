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
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostThreadMessageW, RegisterClassExW, SetWindowLongPtrW, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_INPUT, WM_QUIT, WNDCLASSEXW,
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

/// `stop()` 等消息线程退出的上限。
///
/// 正常路径上 `PostThreadMessageW(WM_QUIT)` 会立刻唤醒 `GetMessageW`（线程内的
/// 窗口已经建好，队列必然存在），所以这里只兜底“线程失联”的病态情况——上限的意义
/// 是保证 `stop()` / `Drop` 永不永久阻塞。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// `RegisterClassExW` 遇到“类已存在”时的 Win32 错误码。
///
/// 窗口类属于**进程**而不是线程：`stop()` 之后再 `start()` 会命中它。
const ERROR_CLASS_ALREADY_EXISTS: i32 = 1410;

/// 一个已注册的 Raw Input 源；drop 时会向消息线程投递 `WM_QUIT` 并有界等待它退出。
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
            // 失败路径上线程已经发完错误并返回；join 只是回收线程栈，不会阻塞。
            Ok(Err(err)) => {
                let _ = join.join();
                return Err(err);
            }
            Err(_) => {
                let _ = join.join();
                return Err("Raw Input 线程在启动前退出".to_string());
            }
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
        let Some(join) = self.join.take() else {
            return;
        };
        // 必须是 WM_QUIT：GetMessageW 收到它返回 0，消息循环才会 break。
        // 之前投的是 WM_CLOSE——它只是普通线程消息（hwnd == NULL），会被
        // DispatchMessageW 直接丢弃，wndproc 的 WM_CLOSE 分支永远收不到，
        // 于是消息循环永不退出、join 永久阻塞（--debug-raw 退出即挂死）。
        //
        // 这里的投递对“start() 刚拿到 thread_id 就立刻 stop()”同样安全：
        // 线程在发出 thread_id 之前已经建好窗口（消息队列随之存在），
        // PostThreadMessageW 只把消息排队，线程回到消息泵时必然取到。
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Err(err) = posted {
            // 线程已经退出（或队列被销毁）时投递会失败；此时它多半马上就能 join 回来。
            eprintln!("[rawinput] PostThreadMessageW(WM_QUIT) 失败：{err}");
        }
        // 有界等待：join() 本身没有超时，线程一旦卡住 Drop 就会挂死整个进程。
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !join.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if join.is_finished() {
            let _ = join.join();
        } else {
            // 极端病态：消息线程没在期限内退出。只能 detach（drop JoinHandle 不阻塞）；
            // 线程仍持有 stats 的 Arc，等它自己退出时回收，不会有人从这里解引用裸指针。
            eprintln!("[rawinput] stop_timeout");
            drop(join);
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

/// 设备名（`RIDI_DEVICENAME`）。
///
/// 排障用：一台机器上往往同时挂着物理鼠标、触摸板、虚拟鼠标（VHF / ROOT\HIDCLASS）。
/// 粘性按键位图是**所有设备共用**的，只有分清是谁发的 DOWN，才知道该不该怀疑它。
/// 失败返回空串——只影响日志，不影响链路。
pub fn device_name(hdevice: u64) -> String {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::UI::Input::{GetRawInputDeviceInfoW, RIDI_DEVICENAME};
    let handle = HANDLE(hdevice as usize as *mut std::ffi::c_void);
    let mut size: u32 = 0;
    let probe = unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, None, &mut size) };
    if probe == u32::MAX || size == 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; size as usize];
    let written = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_DEVICENAME,
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
        )
    };
    if written == u32::MAX {
        return String::new();
    }
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

// RAWINPUT 缓冲：热路径上每个事件都需要一块 8 字节对齐的缓冲，
// 用 thread_local 复用避免每事件一次堆分配。
thread_local! {
    static RAW_BUFFER: std::cell::RefCell<Vec<u64>> =
        std::cell::RefCell::new(Vec::new());
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

    // RAWINPUT 需要 8 字节对齐，用 u64 缓冲而不是 Vec<u8>；缓冲 thread_local 复用。
    let words = (size as usize).div_ceil(size_of::<u64>());
    let packet = RAW_BUFFER.with(|cell| {
        let mut buffer = cell.borrow_mut();
        if buffer.len() < words {
            buffer.resize(words, 0);
        }
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
            return None;
        }
        // 解码只有一处实现（decode.rs），M0 的对拍向量走的也是它。
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, size as usize) };
        crate::decode::decode_raw_input(bytes)
    });
    let Some(packet) = packet else {
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
            // 窗口类属于进程：stop() 之后再次 start() 会命中“类已存在”。
            // 类名、hInstance、wndproc 都是本进程自己的，复用它是预期行为；
            // 只有其它注册失败才是真错误。
            if err.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS) {
                let _ = ready.send(Err(format!("RegisterClassExW 失败：{err}")));
                return Err("RegisterClassExW 失败".to_string());
            }
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
            // 0 = WM_QUIT（stop() 用 PostThreadMessageW 投递），-1 = 取消息失败。
            // 线程消息（hwnd == NULL）也会被 GetMessageW 取出，所以 WM_QUIT
            // 一定能结束这个循环；DispatchMessageW 对 hwnd == NULL 的消息是空操作。
            let ret = GetMessageW(msg.as_mut_ptr(), None, 0, 0).0;
            if ret == -1 || ret == 0 {
                break;
            }
            let message = msg.assume_init();
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        // 先注销设备再销毁窗口：注销之后不会再有新的 WM_INPUT，窗口属于本线程，
        // 由本线程销毁（stop() 不再依赖 wndproc 收到 WM_CLOSE）。
        let remove = [RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        }];
        let _ = RegisterRawInputDevices(&remove, size_of::<RAWINPUTDEVICE>() as u32);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    }

    stopped.store(true, Ordering::SeqCst);
    Ok(())
}

// ---------------------------------------------------------------------------
// 回归测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_NOREMOVE};

    /// 钉住 P0-2 的 Win32 语义：`WM_CLOSE` 只是 hwnd == NULL 的普通线程消息。
    ///
    /// 旧实现的 stop() 投 `WM_CLOSE`，消息循环只在 `GetMessageW` 返回 0/-1 时退出，
    /// 而 `DispatchMessageW` 对 NULL 窗口的消息不调用任何 wndproc（wndproc 里的
    /// `WM_CLOSE → DestroyWindow` 分支永远收不到）→ join 永久阻塞。本用例不需要建窗，
    /// 在任何有消息队列的 Windows 会话上都能跑。
    #[test]
    fn wm_quit_not_wm_close_terminates_thread_message_loop() {
        unsafe {
            // 先建立线程消息队列：没有队列时 GetMessageW 会直接返回 -1。
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            let thread_id = GetCurrentThreadId();

            assert!(
                PostThreadMessageW(thread_id, WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok(),
                "向本线程投递 WM_CLOSE 失败"
            );
            let ret = GetMessageW(&mut msg, None, 0, 0).0;
            assert!(
                ret > 0,
                "WM_CLOSE 是普通消息：GetMessageW 返回非 0（取到消息），只有 WM_QUIT 才返回 0"
            );
            assert_eq!(msg.message, WM_CLOSE, "取到的消息必须是 WM_CLOSE");
            assert!(msg.hwnd.0.is_null(), "线程消息的 hwnd 必须是 NULL");
            // 这一步在旧实现里被当作“唤醒”，实际什么都分发不到。
            let _ = DispatchMessageW(&msg);

            assert!(PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok());
            assert_eq!(
                GetMessageW(&mut msg, None, 0, 0).0,
                0,
                "WM_QUIT 才会让 GetMessageW 返回 0、结束消息循环"
            );
        }
    }

    /// 回归（真实路径）：start() 之后立刻 stop() 必须在 STOP_TIMEOUT 内 join 回来。
    ///
    /// 这条覆盖“thread_id 刚拿到就 stop()”的竞态：WM_QUIT 只需要排队，线程回到
    /// 消息泵时必然取到，不会漏掉唤醒。没有交互式桌面（建窗/注册 Raw Input 失败）
    /// 的环境会跳过——那种环境下真实路径本来就无法验证，纯 Win32 语义由上一个用例兜住。
    #[test]
    fn stop_after_start_returns_within_timeout() {
        let mut source = match RawInputSource::start() {
            Ok(source) => source,
            Err(err) => {
                eprintln!("[rawinput][test] 跳过真实路径：无法启动 Raw Input（{err}）");
                return;
            }
        };
        let started = Instant::now();
        source.stop();
        let elapsed = started.elapsed();
        assert!(
            elapsed < STOP_TIMEOUT,
            "stop() 用了 {elapsed:?}，超过上限 {STOP_TIMEOUT:?}（消息循环没有被 WM_QUIT 唤醒）"
        );
        // 幂等：第二次 stop 立即返回。
        source.stop();
    }
}
