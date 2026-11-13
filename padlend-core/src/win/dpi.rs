//! DPI 感知、光标位置与显示器基础查询（对应 `win/dpi.py`；设计文档 6.3.1 / 13.1）。
//!
//! 本模块只封装 Windows 的少量查询接口：
//!
//! - [`set_dpi_awareness`]：在创建任何窗口前声明 `PER_MONITOR_AWARE_V2`；
//!   新系统失败时回退 `SetProcessDPIAware`。
//! - [`get_monitor_dpi`]：`GetDpiForMonitor(MDT_EFFECTIVE_DPI)`。
//! - [`get_cursor_pos`] / [`try_get_cursor_pos`]：`GetCursorPos`（DPI 感知下是物理像素）。
//! - [`get_monitor_rect`]：`GetMonitorInfoW` 的 `rcMonitor`。
//!
//! ## 与 `win/desktop.rs` 的约定差异
//!
//! 本模块的查询在失败时**只做安全回退，不抛异常**（返回 `None` / `(0, 0)` / 96），
//! 避免 GUI 的 DPI 探测把整个进程拖垮。需要严格错误处理的多显示器枚举在
//! [`crate::win::desktop`] 里完成。
//!
//! `use` 本模块没有副作用：不加载 DLL、不创建窗口、不改进程 DPI 状态；
//! DLL 句柄与导出函数都在函数首次调用时解析并缓存（对应 Python 的 `lru_cache`）。
//!
//! ## 可注入后端
//!
//! 所有 Win32 调用都收在 [`DpiApi`] 后面，生产实现是 [`Win32DpiApi`]，测试注入假后端
//! （`#[cfg(test)]` 里的 `FakeDpiApi`）即可验证回退与回退顺序，不碰真实系统状态。
//! 对应 Python 用 `lru_cache` 包住的模块级函数 + `monkeypatch` 注入。
//!
//! ## feature 说明
//!
//! `windows` crate 的 `Win32_UI_HiDpi` feature 没有开（本模块不允许改 `Cargo.toml`），
//! 所以 `SetProcessDpiAwarenessContext` 与 `GetDpiForMonitor` 走
//! `LoadLibraryW` + `GetProcAddress` 动态解析。这与 Python 用 ctypes 按名字取函数
//! 的行为一致，也顺带保住了「旧系统上没有 shcore.dll → 回退 96」的路径。

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use thiserror::Error;
use windows::core::{w, PCSTR};
use windows::Win32::Foundation::{GetLastError, FARPROC, HMODULE, POINT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos, SetProcessDPIAware};

use crate::constants::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
use crate::logic::geometry::Rect;

/// `MONITOR_DPI_TYPE`（shcore.h）：有效 DPI。
pub const MDT_EFFECTIVE_DPI: i32 = 0;

/// 查询失败时的 DPI 回退值（96 DPI = 100% 缩放）。
pub const FALLBACK_DPI: u32 = 96;

/// DPI 接口的错误：只在本模块内部流转，对外一律折算成安全回退值。
///
/// Python 版把这些失败写成 `log.debug`；Rust 侧保留结构化错误，方便需要时在
/// 调用点打日志或做诊断，同时不改变「查询不抛异常」的对外约定。
#[derive(Debug, Error)]
pub enum DpiError {
    /// 动态库加载失败（例如 Windows 7 没有 shcore.dll）。
    #[error("加载 {module} 失败")]
    LoadModule {
        /// 模块名（`user32.dll` / `shcore.dll`）。
        module: &'static str,
    },
    /// 模块加载成功但缺少导出符号（旧系统上的新 API）。
    #[error("{module} 缺少导出 {symbol}")]
    MissingExport {
        /// 模块名。
        module: &'static str,
        /// 导出符号名。
        symbol: &'static str,
    },
    /// `GetCursorPos` 失败。
    #[error("GetCursorPos 失败: {code}")]
    GetCursorPos {
        /// `GetLastError()`。
        code: u32,
    },
    /// `SetCursorPos` 失败。
    #[error("SetCursorPos 失败: {code}")]
    SetCursorPos {
        /// `GetLastError()`。
        code: u32,
    },
    /// `GetMonitorInfoW` 失败。
    #[error("GetMonitorInfoW(0x{handle:X}) 失败: {code}")]
    MonitorInfo {
        /// `HMONITOR` 的整数值。
        handle: i64,
        /// `GetLastError()`。
        code: u32,
    },
    /// `GetDpiForMonitor` 失败（HRESULT 非 0）。
    #[error("GetDpiForMonitor(0x{handle:X}) 失败: HRESULT=0x{hresult:08X}")]
    MonitorDpi {
        /// `HMONITOR` 的整数值。
        handle: i64,
        /// HRESULT。
        hresult: i32,
    },
    /// `SetProcessDpiAwarenessContext(-4)` 失败。
    #[error("SetProcessDpiAwarenessContext(-4) 失败: {code}")]
    AwarenessContext {
        /// `GetLastError()`（进程 DPI 已经锁定过时是 `ERROR_ACCESS_DENIED`）。
        code: u32,
    },
    /// `SetProcessDPIAware` 回退也失败。
    #[error("SetProcessDPIAware 失败: {code}")]
    LegacyAware {
        /// `GetLastError()`。
        code: u32,
    },
}

// ---------------------------------------------------------------------------
// 可注入后端
// ---------------------------------------------------------------------------

/// DPI 查询 / 光标读写的可注入后端。
///
/// 每个方法对应一条 Win32 调用：返回 `Err` 表示该条调用失败，由本模块的自由函数
/// 折算成安全回退值。测试用它替换真实系统状态。
pub trait DpiApi: Send + Sync {
    /// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`。
    fn set_awareness_context_v2(&self) -> Result<(), DpiError>;

    /// `SetProcessDPIAware()`（Windows 7 兼容回退）。
    fn set_legacy_aware(&self) -> Result<(), DpiError>;

    /// `GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI)` 的 x 方向 DPI。
    fn monitor_dpi(&self, hmon: i64) -> Result<u32, DpiError>;

    /// `GetCursorPos`，物理像素。
    fn cursor_pos(&self) -> Result<(i32, i32), DpiError>;

    /// `SetCursorPos`。
    fn set_cursor_pos(&self, x: i32, y: i32) -> Result<(), DpiError>;

    /// `GetMonitorInfoW(hmon).rcMonitor`，物理像素。
    fn monitor_rect(&self, hmon: i64) -> Result<Rect, DpiError>;
}

/// 真实的 Win32 实现。
///
/// `GetCursorPos` / `SetCursorPos` / `GetMonitorInfoW` / `SetProcessDPIAware` 走
/// `windows` crate 的静态绑定；`SetProcessDpiAwarenessContext` 与 `GetDpiForMonitor`
/// 走动态解析（见模块文档的 feature 说明）。模块句柄进程内只加载一次、永不释放，
/// 与 Python `lru_cache(maxsize=1)` 的生命周期一致。
pub struct Win32DpiApi;

impl DpiApi for Win32DpiApi {
    fn set_awareness_context_v2(&self) -> Result<(), DpiError> {
        let set_context = set_awareness_context_fn()?;
        let context = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 as isize;
        // SAFETY: 函数签名来自 user32.dll 的真实导出；context 是文档规定的 -4。
        let ok = unsafe { set_context(context) };
        if ok != 0 {
            Ok(())
        } else {
            Err(DpiError::AwarenessContext {
                code: last_error_code(),
            })
        }
    }

    fn set_legacy_aware(&self) -> Result<(), DpiError> {
        // SAFETY: 无参数、无副作用的进程状态调用。
        let ok = unsafe { SetProcessDPIAware() };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(DpiError::LegacyAware {
                code: last_error_code(),
            })
        }
    }

    fn monitor_dpi(&self, hmon: i64) -> Result<u32, DpiError> {
        let get_dpi = get_dpi_for_monitor_fn()?;
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let handle = HMONITOR(hmon as usize as *mut c_void);
        // SAFETY: 两个输出指针都指向本栈帧里有效的 u32。
        let hresult = unsafe { get_dpi(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        if hresult == 0 && dpi_x > 0 {
            Ok(dpi_x)
        } else {
            Err(DpiError::MonitorDpi {
                handle: hmon,
                hresult,
            })
        }
    }

    fn cursor_pos(&self) -> Result<(i32, i32), DpiError> {
        let mut point = POINT::default();
        // SAFETY: 输出指针指向本栈帧里有效的 POINT。
        unsafe { GetCursorPos(&mut point) }.map_err(|_| DpiError::GetCursorPos {
            code: last_error_code(),
        })?;
        Ok((point.x, point.y))
    }

    fn set_cursor_pos(&self, x: i32, y: i32) -> Result<(), DpiError> {
        // SAFETY: 纯 Win32 调用，参数是普通整数。
        unsafe { SetCursorPos(x, y) }.map_err(|_| DpiError::SetCursorPos {
            code: last_error_code(),
        })
    }

    fn monitor_rect(&self, hmon: i64) -> Result<Rect, DpiError> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let handle = HMONITOR(hmon as usize as *mut c_void);
        // SAFETY: cbSize 已按要求填好，输出指针指向本栈帧里有效的 MONITORINFO。
        let ok = unsafe { GetMonitorInfoW(handle, &mut info) };
        if !ok.as_bool() {
            return Err(DpiError::MonitorInfo {
                handle: hmon,
                code: last_error_code(),
            });
        }
        Ok(Rect::new(
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right,
            info.rcMonitor.bottom,
        ))
    }
}

// ---------------------------------------------------------------------------
// 动态库与导出符号（进程内缓存）
// ---------------------------------------------------------------------------

/// 已加载的 DLL 句柄。
///
/// `HMODULE` 里是裸指针，因此显式声明 `Send` / `Sync`：句柄只在 `GetProcAddress`
/// 里被读，加载后不再变化，多线程共享是安全的。
struct Module(HMODULE);

// SAFETY: 见上；HMODULE 是进程级不透明句柄，跨线程只读使用。
unsafe impl Send for Module {}
// SAFETY: 同上。
unsafe impl Sync for Module {}

/// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT) -> BOOL`。
type SetAwarenessContextFn = unsafe extern "system" fn(isize) -> i32;
/// `GetDpiForMonitor(HMONITOR, MONITOR_DPI_TYPE, UINT*, UINT*) -> HRESULT`。
type GetDpiForMonitorFn = unsafe extern "system" fn(HMONITOR, i32, *mut u32, *mut u32) -> i32;

/// `user32.dll` 句柄（加载失败也缓存 None，避免每次重试）。
static USER32: OnceLock<Option<Module>> = OnceLock::new();
/// `shcore.dll` 句柄（Windows 8.1+；缺失时永久回退 96）。
static SHCORE: OnceLock<Option<Module>> = OnceLock::new();

/// `SetProcessDpiAwarenessContext` 的导出名（NUL 结尾，`GetProcAddress` 要求）。
const SZ_SET_DPI_AWARENESS_CONTEXT: &[u8] = b"SetProcessDpiAwarenessContext\0";
/// `GetDpiForMonitor` 的导出名。
const SZ_GET_DPI_FOR_MONITOR: &[u8] = b"GetDpiForMonitor\0";

/// 延迟加载 `user32.dll`；失败返回 `None`（按本模块约定不抛）。
fn user32() -> Option<&'static Module> {
    USER32
        .get_or_init(|| {
            // SAFETY: w! 产出的是以 NUL 结尾的静态宽字符串。
            unsafe { LoadLibraryW(w!("user32.dll")) }.ok().map(Module)
        })
        .as_ref()
}

/// 延迟加载 `shcore.dll`（Windows 8.1+ 的 DPI 接口）；失败返回 `None`。
fn shcore() -> Option<&'static Module> {
    SHCORE
        .get_or_init(|| {
            // SAFETY: 同上。
            unsafe { LoadLibraryW(w!("shcore.dll")) }.ok().map(Module)
        })
        .as_ref()
}

/// 解析 `SetProcessDpiAwarenessContext`。
fn set_awareness_context_fn() -> Result<SetAwarenessContextFn, DpiError> {
    let module = user32().ok_or(DpiError::LoadModule {
        module: "user32.dll",
    })?;
    // SAFETY: 模块句柄有效；符号名以 NUL 结尾。
    let proc = unsafe { GetProcAddress(module.0, PCSTR(SZ_SET_DPI_AWARENESS_CONTEXT.as_ptr())) }
        .ok_or(DpiError::MissingExport {
            module: "user32.dll",
            symbol: "SetProcessDpiAwarenessContext",
        })?;
    // SAFETY: user32.dll 里该符号的签名就是 BOOL WINAPI (DPI_AWARENESS_CONTEXT)；
    // FARPROC 与函数指针同宽（编译器会检查尺寸），转换后只在上面按该签名调用。
    Ok(unsafe { std::mem::transmute::<FARPROC, SetAwarenessContextFn>(Some(proc)) })
}

/// 解析 `GetDpiForMonitor`。
fn get_dpi_for_monitor_fn() -> Result<GetDpiForMonitorFn, DpiError> {
    let module = shcore().ok_or(DpiError::LoadModule {
        module: "shcore.dll",
    })?;
    // SAFETY: 模块句柄有效；符号名以 NUL 结尾。
    let proc = unsafe { GetProcAddress(module.0, PCSTR(SZ_GET_DPI_FOR_MONITOR.as_ptr())) }.ok_or(
        DpiError::MissingExport {
            module: "shcore.dll",
            symbol: "GetDpiForMonitor",
        },
    )?;
    // SAFETY: shcore.dll 里该符号是 HRESULT WINAPI GetDpiForMonitor(HMONITOR,
    // MONITOR_DPI_TYPE, UINT*, UINT*)；调用点参数与签名一致。
    Ok(unsafe { std::mem::transmute::<FARPROC, GetDpiForMonitorFn>(Some(proc)) })
}

/// 最近一次 Win32 错误码（`GetLastError` 的数值部分）。
fn last_error_code() -> u32 {
    // SAFETY: 无参数、不访问外部内存。
    unsafe { GetLastError() }.0
}

// ---------------------------------------------------------------------------
// 对外函数（失败只回退，不抛）
// ---------------------------------------------------------------------------

/// 进程级「DPI 感知已声明」缓存。
///
/// Python 每次都重新调用：Windows 在已经锁定 DPI 感知后会让两个 API 都返回失败，
/// 于是第二次调用返回 `False`。Rust 侧把它做成**幂等**：首次成功后直接返回
/// `true`，重复调用不 panic、不再骚扰系统（这是与 Python 的有意差异，调用方拿到的
/// 语义「本进程现在是 Per-Monitor V2」反而更准确）；失败不缓存，下次仍会重试，
/// 与 Python 一致。
static AWARENESS_SET: AtomicBool = AtomicBool::new(false);

/// 声明进程 DPI 感知（必须在创建任何窗口之前调用）。
///
/// 优先 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`；旧系统或系统已锁定
/// DPI 感知导致失败时回退 `SetProcessDPIAware()`。返回是否至少有一个调用成功；
/// 任何失败都不抛异常。重复调用幂等。
pub fn set_dpi_awareness() -> bool {
    if AWARENESS_SET.load(Ordering::Relaxed) {
        return true;
    }
    let ok = set_dpi_awareness_with(&Win32DpiApi);
    if ok {
        AWARENESS_SET.store(true, Ordering::Relaxed);
    }
    ok
}

/// [`set_dpi_awareness`] 的可注入版本：按 V2 → 旧接口的顺序尝试。
pub fn set_dpi_awareness_with(api: &dyn DpiApi) -> bool {
    if api.set_awareness_context_v2().is_ok() {
        return true;
    }
    api.set_legacy_aware().is_ok()
}

/// 返回某台显示器的有效 DPI（96 / 120 / 144 ...）。
///
/// 查询失败（含 Windows 7 没有 shcore）时回退 [`FALLBACK_DPI`]，不抛异常。
pub fn get_monitor_dpi(hmon: i64) -> u32 {
    get_monitor_dpi_with(&Win32DpiApi, hmon)
}

/// [`get_monitor_dpi`] 的可注入版本。
pub fn get_monitor_dpi_with(api: &dyn DpiApi, hmon: i64) -> u32 {
    match api.monitor_dpi(hmon) {
        Ok(dpi) if dpi > 0 => dpi,
        _ => FALLBACK_DPI,
    }
}

/// 返回光标的物理像素坐标；查询失败返回 `None`（不抛异常）。
pub fn try_get_cursor_pos() -> Option<(i32, i32)> {
    try_get_cursor_pos_with(&Win32DpiApi)
}

/// [`try_get_cursor_pos`] 的可注入版本。
pub fn try_get_cursor_pos_with(api: &dyn DpiApi) -> Option<(i32, i32)> {
    api.cursor_pos().ok()
}

/// 返回光标的物理像素坐标；失败回退 `(0, 0)`（不抛异常）。
pub fn get_cursor_pos() -> (i32, i32) {
    get_cursor_pos_with(&Win32DpiApi)
}

/// [`get_cursor_pos`] 的可注入版本。
pub fn get_cursor_pos_with(api: &dyn DpiApi) -> (i32, i32) {
    try_get_cursor_pos_with(api).unwrap_or((0, 0))
}

/// 把光标移到物理像素坐标；成功返回 `true`（不抛异常）。
pub fn set_cursor_pos(x: i32, y: i32) -> bool {
    set_cursor_pos_with(&Win32DpiApi, x, y)
}

/// [`set_cursor_pos`] 的可注入版本。
pub fn set_cursor_pos_with(api: &dyn DpiApi, x: i32, y: i32) -> bool {
    api.set_cursor_pos(x, y).is_ok()
}

/// 返回某台显示器的物理矩形（`rcMonitor`）；失败返回 `None`。
///
/// `hmon == 0` 直接返回 `None`（Python 里的 not hmon 短路），不查询系统。
pub fn get_monitor_rect(hmon: i64) -> Option<Rect> {
    get_monitor_rect_with(&Win32DpiApi, hmon)
}

/// [`get_monitor_rect`] 的可注入版本。
pub fn get_monitor_rect_with(api: &dyn DpiApi, hmon: i64) -> Option<Rect> {
    if hmon == 0 {
        return None;
    }
    api.monitor_rect(hmon).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 可编程的假后端：每个方法一个开关，并记录调用次数。
    ///
    /// 对应 pytest 里 monkeypatch 掉 user32 / shcore 的玩法。
    #[derive(Default)]
    struct FakeDpiApi {
        context_ok: bool,
        legacy_ok: bool,
        dpi: Option<u32>,
        cursor: Option<(i32, i32)>,
        set_cursor_ok: bool,
        rect: Option<Rect>,
        legacy_calls: AtomicUsize,
        rect_calls: AtomicUsize,
    }

    impl DpiApi for FakeDpiApi {
        fn set_awareness_context_v2(&self) -> Result<(), DpiError> {
            if self.context_ok {
                Ok(())
            } else {
                Err(DpiError::AwarenessContext { code: 5 })
            }
        }

        fn set_legacy_aware(&self) -> Result<(), DpiError> {
            self.legacy_calls.fetch_add(1, Ordering::Relaxed);
            if self.legacy_ok {
                Ok(())
            } else {
                Err(DpiError::LegacyAware { code: 5 })
            }
        }

        fn monitor_dpi(&self, hmon: i64) -> Result<u32, DpiError> {
            match self.dpi {
                Some(dpi) => Ok(dpi),
                None => Err(DpiError::MonitorDpi {
                    handle: hmon,
                    hresult: 0x8007_0057u32 as i32,
                }),
            }
        }

        fn cursor_pos(&self) -> Result<(i32, i32), DpiError> {
            self.cursor.ok_or(DpiError::GetCursorPos { code: 0 })
        }

        fn set_cursor_pos(&self, _x: i32, _y: i32) -> Result<(), DpiError> {
            if self.set_cursor_ok {
                Ok(())
            } else {
                Err(DpiError::SetCursorPos { code: 0 })
            }
        }

        fn monitor_rect(&self, hmon: i64) -> Result<Rect, DpiError> {
            self.rect_calls.fetch_add(1, Ordering::Relaxed);
            self.rect.ok_or(DpiError::MonitorInfo {
                handle: hmon,
                code: 87,
            })
        }
    }

    #[test]
    fn constants_match_python() {
        assert_eq!(MDT_EFFECTIVE_DPI, 0);
        assert_eq!(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, -4);
        assert_eq!(FALLBACK_DPI, 96);
    }

    #[test]
    fn awareness_falls_back_to_legacy() {
        // V2 失败 → 旧接口兜底
        let api = FakeDpiApi {
            context_ok: false,
            legacy_ok: true,
            ..Default::default()
        };
        assert!(set_dpi_awareness_with(&api));
        assert_eq!(api.legacy_calls.load(Ordering::Relaxed), 1);

        // V2 成功时不再调旧接口
        let api = FakeDpiApi {
            context_ok: true,
            legacy_ok: false,
            ..Default::default()
        };
        assert!(set_dpi_awareness_with(&api));
        assert_eq!(
            api.legacy_calls.load(Ordering::Relaxed),
            0,
            "V2 成功后不应再调旧接口"
        );

        // 两个都失败 → false，且不 panic
        let api = FakeDpiApi::default();
        assert!(!set_dpi_awareness_with(&api));
        assert_eq!(api.legacy_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queries_fall_back_on_backend_failure() {
        let api = FakeDpiApi::default();
        assert_eq!(get_monitor_dpi_with(&api, 7), FALLBACK_DPI);
        assert_eq!(try_get_cursor_pos_with(&api), None);
        assert_eq!(get_cursor_pos_with(&api), (0, 0));
        assert!(!set_cursor_pos_with(&api, 3, 4));
        assert_eq!(get_monitor_rect_with(&api, 7), None);
    }

    #[test]
    fn queries_pass_backend_values_through() {
        let api = FakeDpiApi {
            dpi: Some(144),
            cursor: Some((321, 654)),
            set_cursor_ok: true,
            rect: Some(Rect::new(-1920, 0, 0, 1080)),
            ..Default::default()
        };
        assert_eq!(get_monitor_dpi_with(&api, 7), 144);
        assert_eq!(try_get_cursor_pos_with(&api), Some((321, 654)));
        assert_eq!(get_cursor_pos_with(&api), (321, 654));
        assert!(set_cursor_pos_with(&api, 3, 4));
        assert_eq!(
            get_monitor_rect_with(&api, 7),
            Some(Rect::new(-1920, 0, 0, 1080))
        );

        // handle=0 直接返回 None，不查询后端（Python 的 not hmon 短路）。
        assert_eq!(get_monitor_rect_with(&api, 0), None);
        assert_eq!(api.rect_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn zero_dpi_is_rejected_like_python() {
        // Python: if hresult == 0 and dpi_x.value > 0 —— 0 也算失败。
        let api = FakeDpiApi {
            dpi: Some(0),
            ..Default::default()
        };
        assert_eq!(get_monitor_dpi_with(&api, 7), FALLBACK_DPI);
    }

    /// pytest test_dpi_helpers_are_safe_on_invalid_handles（去掉会改进程 DPI 状态
    /// 的那一行，见下面的 #[ignore] 版本）。
    #[test]
    fn dpi_helpers_are_safe_on_invalid_handles() {
        assert_eq!(get_monitor_dpi(0), FALLBACK_DPI);
        assert!(get_monitor_rect(0).is_none());
        // 返回值类型始终是 (i32, i32)；查询本身失败也只回退。
        let (x, y): (i32, i32) = get_cursor_pos();
        let _ = (x, y);
        let _ = try_get_cursor_pos();
    }

    #[test]
    #[ignore = "会改进程 DPI 感知状态（SetProcessDpiAwarenessContext），只在桌面会话里手工运行"]
    fn real_set_dpi_awareness_is_idempotent() {
        assert!(set_dpi_awareness(), "首次调用应成功（V2 或回退）");
        assert!(set_dpi_awareness(), "重复调用必须幂等且不 panic");
        assert!(set_dpi_awareness(), "第三次调用同样返回 true");
    }

    #[test]
    #[ignore = "会真实移动系统光标（SetCursorPos），只在桌面会话里手工运行"]
    fn real_set_cursor_pos_round_trips() {
        let Some(origin) = try_get_cursor_pos() else {
            return;
        };
        assert!(set_cursor_pos(10, 20));
        assert_eq!(try_get_cursor_pos(), Some((10, 20)));
        assert!(set_cursor_pos(origin.0, origin.1), "恢复到原位置");
    }
}
