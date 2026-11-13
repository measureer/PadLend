//! 单实例互斥量（对应 `win/single_instance.py`；设计文档 7.5）。
//!
//! 第一个进程 `acquire()` 建立 `Global\PadLend.SingleInstance` 并返回 `true`；后启动的进程会看到
//! `ERROR_ALREADY_EXISTS`，返回 `false`，由调用方唤醒已有实例（`Global\PadLend.Activate` 命名事件）后退出。
//!
//! 互斥量**只用于“是否存在”**，不做 `WaitForSingleObject` 等待，所以句柄持有到 [`release`]
//! 或进程结束即可——这也意味着不会出现“第一个实例崩溃后第二个实例永久卡住”的死锁。
//!
//! 导入本模块不创建任何内核对象；所有内核调用都在 [`SingleInstance`] 的方法里发生。
//!
//! # 分层（对应 Python 的可注入后端）
//!
//! Python 用 `_bind()` 把 kernel32 原型集中绑定，测试用临时互斥量验证。Rust 侧把内核调用收进
//! [`SingleInstanceApi`]：真实实现是 [`Win32SingleInstanceApi`]，测试注入 `tests::FakeApi`，
//! 于是 acquire / release / already_running / 激活事件全链路都能默认跑。
//!
//! # 与 Python 的已知差异（动态解析 CreateMutexW / CreateEventW）
//!
//! 本 crate 的 `windows` 特性里没有 `Win32_Security`，而 `CreateMutexW` / `CreateEventW` 的签名
//! 需要 `Option<*const SECURITY_ATTRIBUTES>`，因此被 feature 门禁掉了。这里改用
//! `GetModuleHandleW` + `GetProcAddress` 动态解析这两个函数——效果与 Python 的 `ctypes.WinDLL` 完全等价。
//! 一旦 Cargo.toml 打开 `Win32_Security`，可以把它们换回 typed API（见 `Win32SingleInstanceApi` 注释）。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{s, w, PCSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, SetLastError, HANDLE, WIN32_ERROR};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{
    OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
};

use crate::constants::{SINGLE_INSTANCE_EVENT, SINGLE_INSTANCE_MUTEX};

// ---------------------------------------------------------------------------
// 常量（对应 Python 模块级常量）
// ---------------------------------------------------------------------------

/// `ERROR_ALREADY_EXISTS`：`CreateMutexW` 成功但同名对象已存在。
pub const ERROR_ALREADY_EXISTS: u32 = 183;

/// `ERROR_FILE_NOT_FOUND`：`OpenMutexW` / `OpenEventW` 找不到对象。
pub const ERROR_FILE_NOT_FOUND: u32 = 2;

/// `SYNCHRONIZE`：`OpenMutexW` 只需要同步权限（crate 里该常量在 Storage::FileSystem，故自建）。
pub const SYNCHRONIZE: u32 = 0x0010_0000;

/// `EVENT_MODIFY_STATE`：`SetEvent` 需要的权限。
pub const EVENT_MODIFY_STATE: u32 = 0x0002;

/// `WAIT_OBJECT_0` 的值（Python `_WAIT_OBJECT_0`）。
pub const WAIT_OBJECT_0_VALUE: u32 = 0;

/// `WAIT_TIMEOUT` 的值（Python `_WAIT_TIMEOUT`）。
pub const WAIT_TIMEOUT_VALUE: u32 = 258;

/// 激活监听的单次等待上限（Python `WaitForSingleObject(handle, 250)`）。
pub const WATCH_TICK_MS: u32 = 250;

/// 激活监听线程名（Python `padlend-activate`）。
pub const ACTIVATION_THREAD_NAME: &str = "padlend-activate";

/// 唤醒首实例的默认超时（Python `timeout=2.0`）。
pub const DEFAULT_SIGNAL_TIMEOUT: Duration = Duration::from_secs(2);

/// 唤醒重试的下限（Python `max(0.1, timeout)`）。
pub const MIN_SIGNAL_TIMEOUT: Duration = Duration::from_millis(100);

/// 唤醒重试间隔（Python `time.sleep(0.05)`）。
pub const SIGNAL_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// 停止监听时 join 线程的上限（Python `thread.join(1.5)`）。
pub const WATCHER_JOIN_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 后端调用失败：要么解析不到 kernel32 符号，要么 Win32 返回 `GetLastError`。
#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    /// 动态加载 kernel32 失败（Python：`无法加载 kernel32.dll: {exc}`）。
    #[error("无法加载 kernel32.dll: {message}")]
    Kernel32 {
        /// 失败原因。
        message: String,
    },
    /// Win32 调用失败并返回错误码。
    #[error("GetLastError={code}")]
    Win32 {
        /// `GetLastError()` 的值。
        code: u32,
    },
}

/// 单实例模块的错误；对应 Python 的 `PadLendError(ErrorCode.UNEXPECTED, …)`。
#[derive(Debug, thiserror::Error)]
pub enum SingleInstanceError {
    /// 本进程已持有**另一个**名字的互斥量（Python：`本进程已持有互斥量 …`）。
    #[error("本进程已持有互斥量 '{held}'，不能再获取 '{name}'")]
    MutexAlreadyHeld {
        /// 当前持有的名字。
        held: String,
        /// 想要再获取的名字。
        name: String,
    },
    /// `CreateMutexW` 失败。
    #[error("CreateMutexW('{name}') 失败: GetLastError={code}")]
    CreateMutex {
        /// 互斥量名。
        name: String,
        /// `GetLastError()` 的值。
        code: u32,
    },
    /// `OpenMutexW` 失败（不是“不存在”，那种情况返回 `false`）。
    #[error("OpenMutexW('{name}') 失败: GetLastError={code}")]
    OpenMutex {
        /// 互斥量名。
        name: String,
        /// `GetLastError()` 的值。
        code: u32,
    },
    /// 动态加载 kernel32 失败。
    #[error("无法加载 kernel32.dll: {message}")]
    Kernel32 {
        /// 失败原因。
        message: String,
    },
}

impl SingleInstanceError {
    /// 对应 Python 的错误码（`core.errors.ErrorCode.UNEXPECTED` = "E900"）。
    pub fn code(&self) -> &'static str {
        "E900"
    }
}

/// `CreateMutexW` 的结果：句柄 + 是否“已经存在同名对象”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutexHandle {
    /// 内核句柄（Python 的 `int(handle)`）。
    pub handle: isize,
    /// `GetLastError() == ERROR_ALREADY_EXISTS`。
    pub already_exists: bool,
}

// ---------------------------------------------------------------------------
// 内核调用面（可注入后端）
// ---------------------------------------------------------------------------

/// 单实例模块的 Win32 调用面。
///
/// 对应 Python `_bind()` 绑定的那几个函数；真实实现是 [`Win32SingleInstanceApi`]，
/// 测试用 `tests::FakeApi` 注入。
pub trait SingleInstanceApi: Send + Sync + 'static {
    /// `CreateMutexW(NULL, FALSE, name)`。
    fn create_mutex(&self, name: &str) -> Result<MutexHandle, ApiError>;
    /// `CreateEventW(NULL, FALSE, FALSE, name)`：**自动复位、初始无信号**。
    fn create_event(&self, name: &str) -> Result<isize, ApiError>;
    /// `OpenMutexW(SYNCHRONIZE, FALSE, name)`；不存在时返回 `Err(Win32{ERROR_FILE_NOT_FOUND})`。
    fn open_mutex(&self, name: &str) -> Result<isize, ApiError>;
    /// `OpenEventW(EVENT_MODIFY_STATE, FALSE, name)`；不存在时返回 `Err(Win32{ERROR_FILE_NOT_FOUND})`。
    fn open_event(&self, name: &str) -> Result<isize, ApiError>;
    /// `SetEvent`。
    fn set_event(&self, handle: isize) -> bool;
    /// `CloseHandle`；失败只记日志。
    fn close_handle(&self, handle: isize);
    /// `WaitForSingleObject(handle, milliseconds)`，返回 `WAIT_EVENT` 的原始值。
    fn wait_for_single_object(&self, handle: isize, milliseconds: u32) -> u32;
    /// `AllowSetForegroundWindow(-1)`；失败不影响事件通知（Python 用 suppress 包住）。
    fn allow_set_foreground_window(&self);
}

/// 真实的 Win32 实现。
///
/// `CreateMutexW` / `CreateEventW` 走 `GetProcAddress` 动态解析（原因见模块头）；其余用 typed API。
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32SingleInstanceApi;

type CreateMutexWFn =
    unsafe extern "system" fn(*mut core::ffi::c_void, i32, *const u16) -> *mut core::ffi::c_void;
type CreateEventWFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    i32,
    i32,
    *const u16,
) -> *mut core::ffi::c_void;

/// 把 Rust 字符串转成以 0 结尾的 UTF-16（`ctypes.c_wchar_p` 在做什么）。
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从 kernel32 解析一个导出函数。
///
/// # Safety
///
/// 调用方必须保证 `name` 对应的函数签名与 `T` 一致（C ABI、参数个数一致）。
unsafe fn resolve<T: Copy>(name: PCSTR) -> Result<T, ApiError> {
    let module = unsafe { GetModuleHandleW(w!("kernel32.dll")) }.map_err(|err| {
        ApiError::Kernel32 {
            message: format!("GetModuleHandleW 失败: {err}"),
        }
    })?;
    let proc = unsafe { GetProcAddress(module, name) }.ok_or_else(|| ApiError::Kernel32 {
        message: format!("找不到导出函数 {name:?}"),
    })?;
    // SAFETY: 调用方承诺签名一致；两者都是函数指针（宽度相同）。
    Ok(unsafe { std::mem::transmute_copy(&proc) })
}

fn create_mutex_w() -> Result<CreateMutexWFn, ApiError> {
    static PROC: OnceLock<CreateMutexWFn> = OnceLock::new();
    if let Some(proc) = PROC.get() {
        return Ok(*proc);
    }
    let proc: CreateMutexWFn = unsafe { resolve(s!("CreateMutexW"))? };
    let _ = PROC.set(proc);
    Ok(proc)
}

fn create_event_w() -> Result<CreateEventWFn, ApiError> {
    static PROC: OnceLock<CreateEventWFn> = OnceLock::new();
    if let Some(proc) = PROC.get() {
        return Ok(*proc);
    }
    let proc: CreateEventWFn = unsafe { resolve(s!("CreateEventW"))? };
    let _ = PROC.set(proc);
    Ok(proc)
}

impl SingleInstanceApi for Win32SingleInstanceApi {
    fn create_mutex(&self, name: &str) -> Result<MutexHandle, ApiError> {
        let function = create_mutex_w()?;
        let name_w = wide(name);
        let handle = unsafe {
            // CreateMutexW 只在“已存在”时设置 LastError，所以先清零（Python 同）。
            SetLastError(WIN32_ERROR(0));
            let handle = function(std::ptr::null_mut(), 0, name_w.as_ptr());
            let last_error = GetLastError().0;
            if handle.is_null() {
                return Err(ApiError::Win32 { code: last_error });
            }
            MutexHandle {
                handle: handle as isize,
                already_exists: last_error == ERROR_ALREADY_EXISTS,
            }
        };
        Ok(handle)
    }

    fn create_event(&self, name: &str) -> Result<isize, ApiError> {
        let function = create_event_w()?;
        let name_w = wide(name);
        let handle = unsafe {
            SetLastError(WIN32_ERROR(0));
            // bManualReset = 0（自动复位）、bInitialState = 0（初始无信号）。
            let handle = function(std::ptr::null_mut(), 0, 0, name_w.as_ptr());
            if handle.is_null() {
                return Err(ApiError::Win32 {
                    code: GetLastError().0,
                });
            }
            handle as isize
        };
        Ok(handle)
    }

    fn open_mutex(&self, name: &str) -> Result<isize, ApiError> {
        let name_w = wide(name);
        match unsafe {
            OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(SYNCHRONIZE),
                false,
                windows::core::PCWSTR(name_w.as_ptr()),
            )
        } {
            Ok(handle) => Ok(handle.0 as isize),
            Err(_) => Err(ApiError::Win32 {
                code: unsafe { GetLastError() }.0,
            }),
        }
    }

    fn open_event(&self, name: &str) -> Result<isize, ApiError> {
        let name_w = wide(name);
        match unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE),
                false,
                windows::core::PCWSTR(name_w.as_ptr()),
            )
        } {
            Ok(handle) => Ok(handle.0 as isize),
            Err(_) => Err(ApiError::Win32 {
                code: unsafe { GetLastError() }.0,
            }),
        }
    }

    fn set_event(&self, handle: isize) -> bool {
        unsafe { SetEvent(HANDLE(handle as *mut core::ffi::c_void)) }.is_ok()
    }

    fn close_handle(&self, handle: isize) {
        if let Err(err) = unsafe { CloseHandle(HANDLE(handle as *mut core::ffi::c_void)) } {
            eprintln!("[single_instance] 关闭句柄失败：{err}");
        }
    }

    fn wait_for_single_object(&self, handle: isize, milliseconds: u32) -> u32 {
        unsafe { WaitForSingleObject(HANDLE(handle as *mut core::ffi::c_void), milliseconds) }.0
    }

    fn allow_set_foreground_window(&self) {
        // ASFW_ANY = -1：允许任意进程抢前台（Python 同样 suppress 失败）。
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX) };
    }
}

// ---------------------------------------------------------------------------
// 管理器
// ---------------------------------------------------------------------------

/// 取锁；中毒时取回内层值（非测试代码不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 本进程持有的互斥量（Python 的 `_handle + _held_name`）。
#[derive(Debug)]
struct HeldMutex {
    handle: isize,
    name: String,
}

/// 模块级状态（Python 那几个模块级全局变量）。
#[derive(Debug, Default)]
struct State {
    mutex: Option<HeldMutex>,
    activation_handle: Option<isize>,
    activation_stop: Option<Arc<AtomicBool>>,
    activation_thread: Option<JoinHandle<()>>,
}

/// 等待线程结束，最多 `timeout`；超时后放弃 join（JoinHandle 被 detach）。
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

/// 激活等待循环（Python `_activation_loop`）。
///
/// 250ms 一次等待，既能让 `stop` 及时生效，也不会空转烧 CPU。
fn activation_loop(
    api: &dyn SingleInstanceApi,
    handle: isize,
    stop: &AtomicBool,
    callback: &(dyn Fn() + Send + Sync),
) {
    while !stop.load(Ordering::Acquire) {
        let result = api.wait_for_single_object(handle, WATCH_TICK_MS);
        if result == WAIT_OBJECT_0_VALUE {
            // 回调异常只记日志，绝不让后台线程退出（Python 的 except Exception）。
            if catch_unwind(AssertUnwindSafe(callback)).is_err() {
                eprintln!("[single_instance] 处理单实例唤起事件失败");
            }
        } else if result == WAIT_TIMEOUT_VALUE {
            continue;
        } else {
            eprintln!("[single_instance] 等待单实例唤起事件失败: {result}");
            return;
        }
    }
}

/// 单实例管理器（设计文档 7.5）。
///
/// 对应 Python 的模块级函数集合，但状态放进结构体：产品用进程级单例
/// （[`acquire`] 等自由函数），测试各自建一个实例注入 [`FakeApi`]，互不干扰。
pub struct SingleInstance {
    api: Arc<dyn SingleInstanceApi>,
    state: Mutex<State>,
}

impl SingleInstance {
    /// 用真实 Win32 后端创建（不创建任何内核对象；acquire() 才有副作用）。
    pub fn new() -> Self {
        Self::with_api(Arc::new(Win32SingleInstanceApi))
    }

    /// 注入自定义后端（测试替身用）。
    pub fn with_api(api: Arc<dyn SingleInstanceApi>) -> Self {
        Self {
            api,
            state: Mutex::new(State::default()),
        }
    }

    /// 尝试成为唯一实例（Python `acquire`）。
    ///
    /// 返回 `true` 表示本进程持有互斥量；`false` 表示已有实例在运行。
    /// 重复调用是幂等的：持有同一名称时再次调用仍返回 `true`（Python 文档明确要求）。
    pub fn acquire(&self, name: &str) -> Result<bool, SingleInstanceError> {
        let mut state = lock(&self.state);
        if let Some(held) = &state.mutex {
            if held.name == name {
                return Ok(true);
            }
            return Err(SingleInstanceError::MutexAlreadyHeld {
                held: held.name.clone(),
                name: name.to_string(),
            });
        }

        match self.api.create_mutex(name) {
            Ok(handle) if handle.already_exists => {
                // 已有实例：立刻关掉这个多余的句柄，不改变本进程状态。
                self.api.close_handle(handle.handle);
                eprintln!("[single_instance] 检测到已有实例（互斥量 {name}）");
                Ok(false)
            }
            Ok(handle) => {
                state.mutex = Some(HeldMutex {
                    handle: handle.handle,
                    name: name.to_string(),
                });
                Ok(true)
            }
            Err(ApiError::Kernel32 { message }) => Err(SingleInstanceError::Kernel32 { message }),
            Err(ApiError::Win32 { code }) => Err(SingleInstanceError::CreateMutex {
                name: name.to_string(),
                code,
            }),
        }
    }

    /// 释放互斥量与唤起事件；未持有时是空操作（幂等）。
    pub fn release(&self) {
        self.stop_activation_watcher();

        let held = lock(&self.state).mutex.take();
        if let Some(held) = held {
            self.api.close_handle(held.handle);
        }
    }

    /// 查询是否已有**其他**实例持有该互斥量（Python `already_running`）。
    ///
    /// 本进程自己持有时返回 `false`（否则调用方会把自身当成“已有实例”）。
    /// 互斥量不存在返回 `false`；其他查询错误抛 `E900`。
    pub fn already_running(&self, name: &str) -> Result<bool, SingleInstanceError> {
        {
            let state = lock(&self.state);
            if state
                .mutex
                .as_ref()
                .is_some_and(|held| held.name == name)
            {
                return Ok(false);
            }
        }

        match self.api.open_mutex(name) {
            Ok(handle) => {
                self.api.close_handle(handle);
                Ok(true)
            }
            Err(ApiError::Win32 { code }) if code == ERROR_FILE_NOT_FOUND => Ok(false),
            Err(ApiError::Win32 { code }) => Err(SingleInstanceError::OpenMutex {
                name: name.to_string(),
                code,
            }),
            Err(ApiError::Kernel32 { message }) => Err(SingleInstanceError::Kernel32 { message }),
        }
    }

    /// 创建“唤到前台”命名事件；重复调用幂等（Python `create_activation_event`）。
    ///
    /// 事件是**自动复位**的：第二实例 `SetEvent` 后首实例的等待线程唤醒一次即回到无信号。
    pub fn create_activation_event(&self, name: &str) -> bool {
        let mut state = lock(&self.state);
        if state.activation_handle.is_some() {
            return true;
        }
        match self.api.create_event(name) {
            Ok(handle) => {
                state.activation_handle = Some(handle);
                eprintln!("[single_instance] 已创建单实例唤起事件 {name}");
                true
            }
            Err(err) => {
                eprintln!("[single_instance] CreateEventW('{name}') 失败: {err}");
                false
            }
        }
    }

    /// 第二实例：通知首实例显示窗口（Python `signal_running_instance`）。
    ///
    /// 首实例可能刚启动、事件还没建好，所以在 `max(0.1s, timeout)` 内重试 `OpenEventW`；
    /// 旧版本没有这个事件时返回 `false`，调用方再退回按窗口标题查找。
    pub fn signal_running_instance(&self, name: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout.max(MIN_SIGNAL_TIMEOUT);
        loop {
            match self.api.open_event(name) {
                Ok(handle) => {
                    self.api.allow_set_foreground_window();
                    let signaled = self.api.set_event(handle);
                    self.api.close_handle(handle);
                    if !signaled {
                        eprintln!("[single_instance] SetEvent('{name}') 失败");
                        return false;
                    }
                    return true;
                }
                Err(ApiError::Win32 { code }) => {
                    if code != ERROR_FILE_NOT_FOUND {
                        eprintln!("[single_instance] OpenEventW('{name}') 失败: GetLastError={code}");
                    }
                }
                Err(ApiError::Kernel32 { message }) => {
                    eprintln!("[single_instance] OpenEventW('{name}') 失败: {message}");
                }
            }

            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(SIGNAL_RETRY_INTERVAL);
        }
    }

    /// 首实例：在后台线程等待第二实例的唤起事件（Python `start_activation_watcher`）。
    pub fn start_activation_watcher<F>(&self, callback: F, name: &str) -> bool
    where
        F: Fn() + Send + Sync + 'static,
    {
        if !self.create_activation_event(name) {
            return false;
        }

        let mut state = lock(&self.state);
        if state
            .activation_thread
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return true;
        }
        let Some(handle) = state.activation_handle else {
            return false;
        };

        let stop = Arc::new(AtomicBool::new(false));
        let api = Arc::clone(&self.api);
        let stop_flag = Arc::clone(&stop);
        let callback = Arc::new(callback);
        let spawned = std::thread::Builder::new()
            .name(ACTIVATION_THREAD_NAME.to_string())
            .spawn(move || activation_loop(api.as_ref(), handle, stop_flag.as_ref(), callback.as_ref()));

        match spawned {
            Ok(thread) => {
                state.activation_stop = Some(stop);
                state.activation_thread = Some(thread);
                true
            }
            Err(err) => {
                eprintln!("[single_instance] 唤起监听线程创建失败：{err}");
                false
            }
        }
    }

    /// 停止唤起监听并关闭事件句柄；未启动时是空操作（Python `stop_activation_watcher`）。
    pub fn stop_activation_watcher(&self) {
        let (stop, thread, handle) = {
            let mut state = lock(&self.state);
            (
                state.activation_stop.take(),
                state.activation_thread.take(),
                state.activation_handle,
            )
        };

        if let Some(stop) = stop {
            stop.store(true, Ordering::Release);
        }
        if let Some(thread) = thread {
            if !join_with_timeout(thread, WATCHER_JOIN_TIMEOUT) {
                eprintln!("[single_instance] 唤起监听线程 join 超时");
            }
        }
        if let Some(handle) = handle {
            let mut state = lock(&self.state);
            // 只有句柄还是刚才那个才关（Python: if _activation_handle == handle）。
            if state.activation_handle == Some(handle) {
                self.api.close_handle(handle);
                state.activation_handle = None;
            }
        }
    }
}

impl Default for SingleInstance {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------------------
// 进程级单例 + 自由函数（对齐 Python 的模块级 API）
// ---------------------------------------------------------------------------

/// 进程级单例（Python 的模块级全局量）。
fn global() -> &'static SingleInstance {
    static GLOBAL: OnceLock<SingleInstance> = OnceLock::new();
    GLOBAL.get_or_init(SingleInstance::new)
}

/// 尝试成为唯一实例（默认互斥量 [`SINGLE_INSTANCE_MUTEX`]）。
pub fn acquire_default() -> Result<bool, SingleInstanceError> {
    global().acquire(SINGLE_INSTANCE_MUTEX)
}

/// 尝试成为唯一实例（指定互斥量名）。
pub fn acquire(name: &str) -> Result<bool, SingleInstanceError> {
    global().acquire(name)
}

/// 释放互斥量与唤起事件；幂等。
pub fn release() {
    global().release();
}

/// 查询是否已有其他实例（默认互斥量）。
pub fn already_running_default() -> Result<bool, SingleInstanceError> {
    global().already_running(SINGLE_INSTANCE_MUTEX)
}

/// 查询是否已有其他实例（指定互斥量名）。
pub fn already_running(name: &str) -> Result<bool, SingleInstanceError> {
    global().already_running(name)
}

/// 创建唤起事件（默认事件名 [`SINGLE_INSTANCE_EVENT`]）。
pub fn create_activation_event_default() -> bool {
    global().create_activation_event(SINGLE_INSTANCE_EVENT)
}

/// 创建唤起事件（指定名字）。
pub fn create_activation_event(name: &str) -> bool {
    global().create_activation_event(name)
}

/// 唤醒已有实例（默认事件名、默认 2s 超时）。
pub fn signal_running_instance_default() -> bool {
    global().signal_running_instance(SINGLE_INSTANCE_EVENT, DEFAULT_SIGNAL_TIMEOUT)
}

/// 唤醒已有实例（指定事件名与超时）。
pub fn signal_running_instance(name: &str, timeout: Duration) -> bool {
    global().signal_running_instance(name, timeout)
}

/// 启动唤起监听（默认事件名）。
pub fn start_activation_watcher_default<F>(callback: F) -> bool
where
    F: Fn() + Send + Sync + 'static,
{
    global().start_activation_watcher(callback, SINGLE_INSTANCE_EVENT)
}

/// 启动唤起监听（指定事件名）。
pub fn start_activation_watcher<F>(callback: F, name: &str) -> bool
where
    F: Fn() + Send + Sync + 'static,
{
    global().start_activation_watcher(callback, name)
}

/// 停止唤起监听。
pub fn stop_activation_watcher() {
    global().stop_activation_watcher();
}

/// 测试辅助：把进程级单例的状态清空（等价 Python 测试里的 `release()` teardown）。
pub fn reset_for_tests() {
    global().release();
}
// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::atomic::{AtomicIsize, AtomicU32};

    /// 轮询等待条件成立。
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

    /// 测试替身：内存里的“内核对象表”，完全不碰 Win32。
    ///
    /// 用“对象表 + 句柄表”模拟真实内核语义：最后一个句柄关闭时对象消失，
    /// 于是“release 之后还能重新 acquire”与 Windows 行为一致。
    #[derive(Debug)]
    struct FakeApi {
        mutexes: Mutex<HashSet<String>>,
        events: Mutex<HashSet<String>>,
        handles: Mutex<HashMap<isize, HandleKind>>,
        external: Mutex<HashSet<String>>,
        next_handle: AtomicIsize,
        create_mutex_calls: AtomicU32,
        open_mutex_calls: AtomicU32,
        create_event_calls: AtomicU32,
        set_event_calls: AtomicU32,
        close_calls: AtomicU32,
        allow_foreground_calls: AtomicU32,
        wait_calls: AtomicU32,
        fail_create_mutex: Mutex<Option<u32>>,
        fail_open_mutex: Mutex<Option<u32>>,
        fail_create_event: Mutex<Option<u32>>,
        fail_set_event: AtomicBool,
        wait_results: Mutex<VecDeque<u32>>,
        wait_default: Mutex<u32>,
    }

    /// 替身句柄指向的内核对象。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HandleKind {
        Mutex(String),
        Event(String),
    }

    impl Default for FakeApi {
        fn default() -> Self {
            Self {
                mutexes: Mutex::new(HashSet::new()),
                events: Mutex::new(HashSet::new()),
                handles: Mutex::new(HashMap::new()),
                external: Mutex::new(HashSet::new()),
                next_handle: AtomicIsize::new(0x100),
                create_mutex_calls: AtomicU32::new(0),
                open_mutex_calls: AtomicU32::new(0),
                create_event_calls: AtomicU32::new(0),
                set_event_calls: AtomicU32::new(0),
                close_calls: AtomicU32::new(0),
                allow_foreground_calls: AtomicU32::new(0),
                wait_calls: AtomicU32::new(0),
                fail_create_mutex: Mutex::new(None),
                fail_open_mutex: Mutex::new(None),
                fail_create_event: Mutex::new(None),
                fail_set_event: AtomicBool::new(false),
                wait_results: Mutex::new(VecDeque::new()),
                wait_default: Mutex::new(WAIT_TIMEOUT_VALUE),
            }
        }
    }

    impl FakeApi {
        fn next_handle(&self) -> isize {
            self.next_handle.fetch_add(1, Ordering::SeqCst)
        }

        /// 模拟“另一个进程已经持有同名互斥量”：外部对象不会被本替身关闭。
        fn mark_mutex_existing(&self, name: &str) {
            lock_(&self.external).insert(name.to_string());
            lock_(&self.mutexes).insert(name.to_string());
        }

        fn set_wait_default(&self, code: u32) {
            *lock_(&self.wait_default) = code;
        }

        fn push_wait(&self, code: u32) {
            lock_(&self.wait_results).push_back(code);
        }

        fn close_calls(&self) -> u32 {
            self.close_calls.load(Ordering::SeqCst)
        }

        fn wait_calls(&self) -> u32 {
            self.wait_calls.load(Ordering::SeqCst)
        }

        fn open_mutex_calls(&self) -> u32 {
            self.open_mutex_calls.load(Ordering::SeqCst)
        }

        fn create_event_calls(&self) -> u32 {
            self.create_event_calls.load(Ordering::SeqCst)
        }

        fn set_event_calls(&self) -> u32 {
            self.set_event_calls.load(Ordering::SeqCst)
        }

        fn allow_foreground_calls(&self) -> u32 {
            self.allow_foreground_calls.load(Ordering::SeqCst)
        }
    }

    /// 替身内部的取锁（用独立名字避免与模块级 lock 混淆）。
    fn lock_<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    impl SingleInstanceApi for FakeApi {
        fn create_mutex(&self, name: &str) -> Result<MutexHandle, ApiError> {
            self.create_mutex_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock_(&self.fail_create_mutex) {
                return Err(ApiError::Win32 { code });
            }
            let mut mutexes = lock_(&self.mutexes);
            let already_exists = mutexes.contains(name);
            mutexes.insert(name.to_string());
            let handle = self.next_handle();
            lock_(&self.handles).insert(handle, HandleKind::Mutex(name.to_string()));
            Ok(MutexHandle {
                handle,
                already_exists,
            })
        }

        fn create_event(&self, name: &str) -> Result<isize, ApiError> {
            self.create_event_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock_(&self.fail_create_event) {
                return Err(ApiError::Win32 { code });
            }
            let handle = self.next_handle();
            lock_(&self.events).insert(name.to_string());
            lock_(&self.handles).insert(handle, HandleKind::Event(name.to_string()));
            Ok(handle)
        }

        fn open_mutex(&self, name: &str) -> Result<isize, ApiError> {
            self.open_mutex_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = *lock_(&self.fail_open_mutex) {
                return Err(ApiError::Win32 { code });
            }
            if lock_(&self.mutexes).contains(name) {
                let handle = self.next_handle();
                lock_(&self.handles).insert(handle, HandleKind::Mutex(name.to_string()));
                Ok(handle)
            } else {
                Err(ApiError::Win32 {
                    code: ERROR_FILE_NOT_FOUND,
                })
            }
        }

        fn open_event(&self, name: &str) -> Result<isize, ApiError> {
            if lock_(&self.events).contains(name) {
                let handle = self.next_handle();
                lock_(&self.handles).insert(handle, HandleKind::Event(name.to_string()));
                Ok(handle)
            } else {
                Err(ApiError::Win32 {
                    code: ERROR_FILE_NOT_FOUND,
                })
            }
        }

        fn set_event(&self, _handle: isize) -> bool {
            self.set_event_calls.fetch_add(1, Ordering::SeqCst);
            !self.fail_set_event.load(Ordering::SeqCst)
        }

        fn close_handle(&self, handle: isize) {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            // 句柄全部关闭后对象消失（外部对象除外）——真实内核语义。
            // 注意：必须先把 remove 的结果落到局部变量，否则 if let 的作用域会一直
            // 持有 handles 的 MutexGuard，下面再取同一把锁就死锁了。
            let removed = lock_(&self.handles).remove(&handle);
            if let Some(HandleKind::Mutex(name)) = removed {
                let still_open = lock_(&self.handles)
                    .values()
                    .any(|kind| *kind == HandleKind::Mutex(name.clone()));
                let external = lock_(&self.external).contains(&name);
                if !still_open && !external {
                    lock_(&self.mutexes).remove(&name);
                }
            }
        }

        fn wait_for_single_object(&self, _handle: isize, _milliseconds: u32) -> u32 {
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            // 防止 wait 默认值不是超时时消息循环空转烧 CPU。
            std::thread::sleep(Duration::from_millis(1));
            if let Some(code) = lock_(&self.wait_results).pop_front() {
                return code;
            }
            *lock_(&self.wait_default)
        }

        fn allow_set_foreground_window(&self) {
            self.allow_foreground_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fake(api: &Arc<FakeApi>) -> SingleInstance {
        let concrete: Arc<FakeApi> = Arc::clone(api);
        let backend: Arc<dyn SingleInstanceApi> = concrete;
        SingleInstance::with_api(backend)
    }

    const MUTEX_A: &str = r"Global\PadLend.Test.MutexA";
    const MUTEX_B: &str = r"Global\PadLend.Test.MutexB";
    const EVENT_A: &str = r"Global\PadLend.Test.Activate";

    // ---- 互斥量 -------------------------------------------------------------

    #[test]
    fn acquire_is_idempotent() {
        // 对应 Python test_geometry 里的 acquire 幂等用例。
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        assert!(instance.acquire(MUTEX_A).expect("首次 acquire"));
        assert!(instance.acquire(MUTEX_A).expect("重复 acquire 必须幂等"));
        assert_eq!(api.close_calls(), 0, "持有时不该关句柄");
    }

    #[test]
    fn acquire_returns_false_when_another_instance_exists() {
        let api = Arc::new(FakeApi::default());
        api.mark_mutex_existing(MUTEX_A);
        let instance = fake(&api);

        assert!(!instance.acquire(MUTEX_A).expect("已有实例不算错误"));
        assert_eq!(api.close_calls(), 1, "检测到已有实例必须立刻关掉多余句柄");
        // 失败的 acquire 不能把状态记成“已持有”，所以换个名字还应当成功。
        assert!(
            instance.acquire(MUTEX_B).expect("失败的 acquire 不该留下状态"),
            "失败的 acquire 之后本进程仍然是空的"
        );
    }

    #[test]
    fn acquire_with_second_name_reports_e900() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);
        assert!(instance.acquire(MUTEX_A).expect("先持有 A"));

        let err = instance.acquire(MUTEX_B).expect_err("持有 A 时不能再获取 B");
        assert_eq!(err.code(), "E900");
        let message = err.to_string();
        assert!(
            message.contains("本进程已持有互斥量") && message.contains(MUTEX_B),
            "错误文案要与 Python 对齐，实际：{message}"
        );
    }

    #[test]
    fn create_mutex_failure_matches_python_message() {
        let api = Arc::new(FakeApi::default());
        *lock_(&api.fail_create_mutex) = Some(5);
        let instance = fake(&api);

        let err = instance.acquire(MUTEX_A).expect_err("CreateMutexW 失败必须上抛");
        assert_eq!(err.code(), "E900");
        assert_eq!(
            err.to_string(),
            format!("CreateMutexW('{MUTEX_A}') 失败: GetLastError=5")
        );
    }

    #[test]
    fn release_is_idempotent() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        instance.release();
        instance.release();
        assert_eq!(api.close_calls(), 0, "没持有时 release 是空操作");

        assert!(instance.acquire(MUTEX_A).expect("acquire"));
        instance.release();
        assert_eq!(api.close_calls(), 1, "释放必须关掉互斥量句柄");
        instance.release();
        assert_eq!(api.close_calls(), 1, "第二次 release 不能再关");
        assert!(instance.acquire(MUTEX_A).expect("释放后可以重新获取"));
    }

    #[test]
    fn already_running_is_false_for_self_without_querying() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);
        assert!(instance.acquire(MUTEX_A).expect("acquire"));

        assert!(
            !instance.already_running(MUTEX_A).expect("自己持有时必须 false"),
            "否则调用方会把自身当成“已有实例”"
        );
        assert_eq!(api.open_mutex_calls(), 0, "自己持有时不该再 OpenMutexW");
    }

    #[test]
    fn already_running_sees_external_instance() {
        let api = Arc::new(FakeApi::default());
        api.mark_mutex_existing(MUTEX_A);
        let instance = fake(&api);

        assert!(instance.already_running(MUTEX_A).expect("查询"), "外部实例必须被检测到");
        assert_eq!(api.close_calls(), 1, "查询成功要关掉打开得到的句柄");
    }

    #[test]
    fn already_running_is_false_when_mutex_missing() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        assert!(!instance.already_running(MUTEX_A).expect("不存在不是错误"));
        assert_eq!(api.close_calls(), 0);
    }

    #[test]
    fn already_running_propagates_other_errors() {
        let api = Arc::new(FakeApi::default());
        *lock_(&api.fail_open_mutex) = Some(5);
        let instance = fake(&api);

        let err = instance.already_running(MUTEX_A).expect_err("非 FILE_NOT_FOUND 必须上抛");
        assert_eq!(
            err.to_string(),
            format!("OpenMutexW('{MUTEX_A}') 失败: GetLastError=5")
        );
    }

    // ---- 唤起事件 -----------------------------------------------------------

    #[test]
    fn create_activation_event_is_idempotent() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        assert!(instance.create_activation_event(EVENT_A));
        assert!(instance.create_activation_event(EVENT_A), "重复调用必须返回 true");
        assert_eq!(api.create_event_calls(), 1, "只该 CreateEventW 一次");
    }

    #[test]
    fn create_activation_event_failure_returns_false() {
        let api = Arc::new(FakeApi::default());
        *lock_(&api.fail_create_event) = Some(5);
        let instance = fake(&api);

        assert!(!instance.create_activation_event(EVENT_A), "失败只返回 false，不上抛");
    }

    #[test]
    fn signal_returns_false_when_event_missing() {
        // 对应 Python：旧版本没有事件时返回 False，调用方退回按标题查找窗口。
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        let started = Instant::now();
        let signaled = instance.signal_running_instance(EVENT_A, Duration::from_millis(100));
        let elapsed = started.elapsed();

        assert!(!signaled);
        assert!(
            elapsed >= MIN_SIGNAL_TIMEOUT - Duration::from_millis(20),
            "至少要重试到下限超时，实际 {elapsed:?}"
        );
        assert_eq!(api.set_event_calls(), 0);
    }

    #[test]
    fn signal_wakes_existing_event() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);
        assert!(instance.create_activation_event(EVENT_A));

        assert!(instance.signal_running_instance(EVENT_A, Duration::from_millis(100)));
        assert_eq!(api.set_event_calls(), 1);
        assert_eq!(api.allow_foreground_calls(), 1, "必须先允许抢前台再 SetEvent");
    }

    #[test]
    fn signal_returns_false_when_set_event_fails() {
        let api = Arc::new(FakeApi::default());
        api.fail_set_event.store(true, Ordering::SeqCst);
        let instance = fake(&api);
        assert!(instance.create_activation_event(EVENT_A));

        assert!(!instance.signal_running_instance(EVENT_A, Duration::from_millis(100)));
        assert_eq!(api.close_calls(), 1, "失败路径也要关掉事件句柄");
    }

    // ---- 唤起监听 -----------------------------------------------------------

    #[test]
    fn watcher_invokes_callback_and_stop_closes_handle() {
        // 对应 Python test_geometry：激活事件唤醒 watcher。
        let api = Arc::new(FakeApi::default());
        api.set_wait_default(WAIT_OBJECT_0_VALUE);
        let instance = fake(&api);
        let hits = Arc::new(AtomicU32::new(0));

        let counter = Arc::clone(&hits);
        assert!(
            instance.start_activation_watcher(
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                },
                EVENT_A
            ),
            "启动监听失败"
        );
        assert!(
            wait_until(|| hits.load(Ordering::SeqCst) >= 2, Duration::from_secs(2)),
            "唤起事件没有触发回调"
        );

        instance.stop_activation_watcher();
        assert_eq!(api.close_calls(), 1, "停止监听必须关闭事件句柄");
        assert!(api.wait_calls() >= 2);
    }

    #[test]
    fn watcher_survives_callback_panic() {
        let api = Arc::new(FakeApi::default());
        api.set_wait_default(WAIT_OBJECT_0_VALUE);
        let instance = fake(&api);

        assert!(instance.start_activation_watcher(|| panic!("唤起回调炸了"), EVENT_A));
        assert!(
            wait_until(|| api.wait_calls() >= 5, Duration::from_secs(2)),
            "回调 panic 不能终止监听线程"
        );
        instance.stop_activation_watcher();
    }

    #[test]
    fn watcher_stops_on_unexpected_wait_result() {
        let api = Arc::new(FakeApi::default());
        // 第一次等待返回一个既不是 OBJECT_0 也不是 TIMEOUT 的值 → 监听线程应退出。
        api.push_wait(0xFFFF_FFFF);
        let instance = fake(&api);

        assert!(instance.start_activation_watcher(|| {}, EVENT_A));
        assert!(
            wait_until(|| api.wait_calls() >= 1, Duration::from_secs(2)),
            "至少等一次"
        );
        instance.stop_activation_watcher();
        assert_eq!(api.close_calls(), 1);
    }

    #[test]
    fn start_watcher_twice_starts_one_thread() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        assert!(instance.start_activation_watcher(|| {}, EVENT_A));
        assert!(instance.start_activation_watcher(|| {}, EVENT_A), "重复启动幂等");
        instance.stop_activation_watcher();
        assert_eq!(api.create_event_calls(), 1, "事件只建一次");
    }

    #[test]
    fn stop_watcher_is_idempotent() {
        let api = Arc::new(FakeApi::default());
        let instance = fake(&api);

        instance.stop_activation_watcher();
        instance.stop_activation_watcher();
        assert_eq!(api.close_calls(), 0);
    }

    #[test]
    fn release_also_stops_watcher() {
        // Python release() 第一件事就是 stop_activation_watcher()。
        let api = Arc::new(FakeApi::default());
        api.set_wait_default(WAIT_TIMEOUT_VALUE);
        let instance = fake(&api);
        assert!(instance.acquire(MUTEX_A).expect("acquire"));
        assert!(instance.start_activation_watcher(|| {}, EVENT_A));

        instance.release();
        assert_eq!(api.close_calls(), 2, "互斥量 + 事件两个句柄都要关");
    }

    // ---- 真实内核对象：默认不跑 ---------------------------------------------

    const TEST_MUTEX: &str = r"Global\PadLend.SingleInstance.Test";
    const TEST_EVENT: &str = r"Global\PadLend.Activate.Test";

    #[test]
    #[ignore = "会创建真实内核互斥量 Global\\PadLend.*：手工执行 cargo test -- --ignored"]
    fn real_acquire_release_is_idempotent() {
        // 对应 Python test_geometry.py:390 附近的 acquire/release 幂等用例。
        let instance = SingleInstance::new();
        assert!(instance.acquire(TEST_MUTEX).expect("首次 acquire"));
        assert!(instance.acquire(TEST_MUTEX).expect("重复 acquire 幂等"));
        assert!(!instance.already_running(TEST_MUTEX).expect("自己持有时 false"));

        instance.release();
        instance.release();
        assert!(instance.acquire(TEST_MUTEX).expect("释放后可重新获取"));
        instance.release();
    }

    #[test]
    #[ignore = "会创建真实内核互斥量 Global\\PadLend.*：手工执行 cargo test -- --ignored"]
    fn real_already_running_detects_other_instance() {
        let held = SingleInstance::new();
        let observer = SingleInstance::new();

        assert!(held.acquire(TEST_MUTEX).expect("acquire"));
        assert!(observer.already_running(TEST_MUTEX).expect("查询"), "外部实例必须被检测到");

        held.release();
        assert!(!observer.already_running(TEST_MUTEX).expect("释放后查询"));
    }

    #[test]
    #[ignore = "会创建真实内核事件 Global\\PadLend.*：手工执行 cargo test -- --ignored"]
    fn real_activation_event_roundtrip() {
        // 对应 Python test_geometry：激活事件唤醒 watcher、缺失事件 signal 返回 False。
        let first = SingleInstance::new();
        let second = SingleInstance::new();

        assert!(
            !second.signal_running_instance(TEST_EVENT, Duration::from_millis(100)),
            "事件不存在时必须返回 false"
        );

        assert!(first.create_activation_event(TEST_EVENT));
        let hits = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&hits);
        assert!(first.start_activation_watcher(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            TEST_EVENT
        ));

        assert!(second.signal_running_instance(TEST_EVENT, Duration::from_millis(500)));
        assert!(
            wait_until(|| hits.load(Ordering::SeqCst) >= 1, Duration::from_secs(2)),
            "首实例没有收到唤起事件"
        );
        first.stop_activation_watcher();
    }
}
