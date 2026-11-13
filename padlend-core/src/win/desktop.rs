//! 虚拟桌面几何与多显示器外轮廓（对应 `win/desktop.py`；设计文档 6.3 / 8.1 / 8.2 / 13）。
//!
//! 几何数据类型定义在 [`crate::logic::geometry`]（对应 Python 的 `core/geometry.py`，
//! 避免 `logic` 反向依赖 `win`），本模块按契约重新导出，保证
//! `use padlend_core::win::desktop::{Edge, Rect, DesktopMap};` 等写法成立。
//!
//! ## 核心概念
//!
//! 虚拟桌面坐标：主屏左上角为原点，单位物理像素，允许负值（13.3）。
//!
//! 外轮廓边段：平板只能吸附到整个虚拟桌面并集的**外轮廓**，不能吸附到显示器之间的
//! 内缝（6.3.3 / 13.2）。算法对每台显示器的每条边，在边外侧 1px 的扫描线上按 1px
//! 单位找出「不被任何显示器占据」的连续区间（6.3.3），然后合并共线且 span 相接/重叠
//! 的段（6.3.4 的「虚拟段」）。
//!
//! **合并必须同时满足三个条件**：`edge` 相同、边线坐标相同（垂直边是 x，水平边是 y）、
//! span 相接或重叠。仅检查 edge+span 会把左右双屏中两块屏各自的 left 边（x 不同但
//! span 相同）错误地并成一条，落点会落在两屏之间 —— 移植时不要「简化」这个判断。
//!
//! ## 可注入后端
//!
//! 枚举显示器、读/写光标都是 Win32 调用，统一收在 [`DesktopApi`] 后面：生产代码用
//! [`Win32DesktopApi`]，测试注入假快照走纯几何路径（对应 Python 的
//! `DesktopMap(monitors=...)`）。用 [`DesktopMap::from_monitors`] 构造**不触碰
//! Win32**，只有 [`DesktopMap::new`] / [`DesktopMap::refresh`] 才枚举显示器。
//!
//! ## 刷新语义
//!
//! 刷新是整体替换：先算出新的不可变快照，再一次性赋值，读线程不会看到半更新的组合；
//! 失败时保留上一次成功的快照，避免一次瞬时失败把可用几何清空（13.4）。

use std::cmp::Ordering;
use std::mem::size_of;
use std::sync::Arc;

use windows::core::BOOL;
use windows::Win32::Foundation::{GetLastError, SetLastError, ERROR_SUCCESS, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use crate::constants::{BOUNDARY_INSET_PX, CORNER_DIRECTION_RATIO, ENTRY_MARGIN_PX};
use crate::logic::geometry::ALL_EDGES;
use crate::win::dpi;

pub use crate::logic::geometry::{Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect};

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 桌面几何层的错误：对应 Python 里抛 `PadLendError` 的两处。
///
/// 文案逐字对齐 Python，便于两侧日志 diff；[`DesktopError::code`] 给出
/// `core/errors.py` 的错误码（枚举失败 `E900`，摆位失效 `E301`）。
#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    /// `EnumDisplayMonitors` 返回失败且系统给出了错误码。
    #[error("EnumDisplayMonitors 失败 (GetLastError={last_error})")]
    EnumFailed {
        /// `GetLastError()`。
        last_error: u32,
    },
    /// 回调内部失败（`GetMonitorInfoW` 等）拼接成的明细。
    #[error("{0}")]
    EnumDetail(String),
    /// 旧 `Placement.segment` 在当前快照里已不存在（13.4）。
    #[error("段 {edge} [{span_start}, {span_end}) 已不存在")]
    SegmentGone {
        /// 边名（`left` / `right` / `top` / `bottom`）。
        edge: &'static str,
        /// 沿边起点。
        span_start: i32,
        /// 沿边终点。
        span_end: i32,
    },
    /// 段指向的显示器已经从快照里消失。
    #[error("段引用的显示器 handle=0x{handle:X} 已不存在")]
    MonitorGone {
        /// `MonitorInfo.handle`。
        handle: i64,
    },
    /// 显示器矩形为空（宽或高 `<= 0`），无法计算落点。
    #[error("显示器 {device} 的矩形为空")]
    EmptyMonitorRect {
        /// 设备名；没有设备名时是 handle 的十进制字符串（Python `device or handle`）。
        device: String,
    },
}

impl DesktopError {
    /// `core/errors.py` 的错误码：枚举失败是 `E900`，摆位失效是 `E301`。
    pub fn code(&self) -> &'static str {
        match self {
            DesktopError::EnumFailed { .. } | DesktopError::EnumDetail(_) => "E900",
            DesktopError::SegmentGone { .. }
            | DesktopError::MonitorGone { .. }
            | DesktopError::EmptyMonitorRect { .. } => "E301",
        }
    }
}

// ---------------------------------------------------------------------------
// 可注入后端
// ---------------------------------------------------------------------------

/// 桌面几何依赖的 Win32 后端。
///
/// 三个方法分别对应 Python 里 `_enum_display_monitors`、`dpi.try_get_cursor_pos` 与
/// `dpi.set_cursor_pos` 三处外部依赖；测试用假实现替换它们，几何计算本身保持纯函数。
pub trait DesktopApi: Send + Sync {
    /// 枚举显示器（`EnumDisplayMonitors` + `GetMonitorInfoW` + `GetDpiForMonitor`）。
    ///
    /// 返回的列表由调用方再排序/建快照；失败时返回 [`DesktopError`]。
    fn enum_monitors(&self) -> Result<Vec<MonitorInfo>, DesktopError>;

    /// 当前系统光标的物理像素坐标；查询失败返回 `None`（不抛）。
    fn try_get_cursor_pos(&self) -> Option<(i32, i32)>;

    /// 把系统光标移到物理像素坐标；成功返回 `true`。
    fn set_cursor_pos(&self, x: i32, y: i32) -> bool;
}

/// 真实的 Win32 实现（枚举用本模块的 `EnumDisplayMonitors` 封装，光标交给 `win::dpi`）。
pub struct Win32DesktopApi;

impl DesktopApi for Win32DesktopApi {
    fn enum_monitors(&self) -> Result<Vec<MonitorInfo>, DesktopError> {
        enum_display_monitors()
    }

    fn try_get_cursor_pos(&self) -> Option<(i32, i32)> {
        dpi::try_get_cursor_pos()
    }

    fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
        dpi::set_cursor_pos(x, y)
    }
}

// ---------------------------------------------------------------------------
// 纯几何辅助
// ---------------------------------------------------------------------------

/// 四条边的固定顺序（Python `_EDGE_INDEX`）：保证 `exterior_segments` 结果可复现，
/// 也是 GUI 绘制的顺序。
fn edge_index(edge: Edge) -> u8 {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

/// 与 Python `_clamp` 同语义的浮点夹取（NaN 会原样传播，因为两个比较都不成立）。
fn clamp(value: f64, low: f64, high: f64) -> f64 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// 与 Python `_clamp_int` 同语义：`high < low` 时返回 `low`。
fn clamp_int(value: i64, low: i64, high: i64) -> i64 {
    if high < low {
        return low;
    }
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// 等价于 Python `int(round(value))`：四舍六入五成双（banker's rounding）。
///
/// Python 的 `round(float)` 是「就近取偶」，与 Rust `f64::round`（远离零）在 `x.5`
/// 上不同，所以必须用 `round_ties_even`。NaN / inf 在 Python 会抛，这里退化为 0。
fn round_to_i32(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let rounded = value.round_ties_even();
    if rounded >= i32::MAX as f64 {
        i32::MAX
    } else if rounded <= i32::MIN as f64 {
        i32::MIN
    } else {
        rounded as i32
    }
}

/// 返回边线坐标：外侧 1px 扫描线的 x（垂直边）或 y（水平边）。
fn edge_line(monitor: &MonitorInfo, edge: Edge) -> i32 {
    let rect = monitor.rect;
    match edge {
        Edge::Left => rect.left.saturating_sub(1),
        Edge::Right => rect.right,
        Edge::Top => rect.top.saturating_sub(1),
        Edge::Bottom => rect.bottom,
    }
}

/// 在 `monitor` 某条边外侧 1px 的扫描线上，返回未被占据的连续区间。
///
/// 逐像素扫描（宽度 1px）是设计文档 6.3.3 的原始口径：只要扫描线被**任何**显示器
/// 覆盖就算被占据，因此显示器之间的内缝不会产出边段。
fn free_runs(monitor: &MonitorInfo, edge: Edge, monitors: &[MonitorInfo]) -> Vec<(i32, i32)> {
    let rect = monitor.rect;
    if rect.empty() {
        return Vec::new();
    }

    let vertical = edge.is_vertical();
    let line = edge_line(monitor, edge);
    let (along_start, along_end) = if vertical {
        (rect.top, rect.bottom)
    } else {
        (rect.left, rect.right)
    };

    let mut runs: Vec<(i32, i32)> = Vec::new();
    let mut run_start: Option<i32> = None;
    let mut along = along_start;
    while along < along_end {
        let blocked = monitors.iter().any(|other| {
            if vertical {
                other.rect.contains(line, along)
            } else {
                other.rect.contains(along, line)
            }
        });
        if blocked {
            if let Some(start) = run_start.take() {
                runs.push((start, along));
            }
        } else if run_start.is_none() {
            run_start = Some(along);
        }
        along += 1;
    }
    if let Some(start) = run_start {
        runs.push((start, along_end));
    }
    runs
}

/// 计算虚拟桌面外轮廓边段（纯函数，不依赖 win32）。
///
/// 对每台显示器每条边，在边外侧 1px 的扫描线上按 1px 找出未被任何显示器占据的连续
/// 区间；随后把 `edge` 相同、边线坐标相同且 span 相接/重叠的段合并成虚拟段
/// （6.3.3 / 6.3.4）。合并后的段保留起始 monitor 的 handle（共线且相接，落点计算
/// 不受影响）。
pub fn compute_exterior_segments(monitors: &[MonitorInfo]) -> Vec<EdgeSegment> {
    // 记录 (edge 序号, 边线坐标, span_start, span_end, monitor_handle)
    let mut records: Vec<(u8, i32, i32, i32, i64)> = Vec::new();
    for monitor in monitors {
        for edge in ALL_EDGES {
            let line = edge_line(monitor, edge);
            for (span_start, span_end) in free_runs(monitor, edge, monitors) {
                records.push((edge_index(edge), line, span_start, span_end, monitor.handle));
            }
        }
    }
    // 稳定排序：完全相同的记录（重复显示器）保持输入顺序，与 Python 的 list.sort 一致。
    records.sort();

    let mut merged: Vec<(u8, i32, i32, i32, i64)> = Vec::new();
    for record in records {
        if let Some(last) = merged.last_mut() {
            if last.0 == record.0 && last.1 == record.1 && record.2 <= last.3 {
                last.3 = last.3.max(record.3);
                continue;
            }
        }
        merged.push(record);
    }

    // span_start < span_end 由 free_runs 与上面的合并规则保证（合并只延长末尾），
    // 所以这里直接构造而不走 EdgeSegment::new 的 Result —— Python 版此路径同样不会失败。
    merged
        .into_iter()
        .map(|(edge_index, _line, span_start, span_end, handle)| {
            // span_start < span_end 由 free_runs（只在未占据区间闭合）与合并规则（只延长
            // 末尾）保证，与 Python 一样这里不可能失败，因此不经 EdgeSegment::new 的校验。
            EdgeSegment {
                monitor_handle: handle,
                edge: ALL_EDGES[edge_index as usize],
                span_start,
                span_end,
            }
        })
        .collect()
}

/// 所有显示器并集的包围盒；无显示器时为 `Rect(0, 0, 0, 0)`。
fn bounding_box(monitors: &[MonitorInfo]) -> Rect {
    let mut iter = monitors.iter();
    let Some(first) = iter.next() else {
        return Rect::new(0, 0, 0, 0);
    };
    let mut rect = first.rect;
    for monitor in iter {
        rect = rect.union(&monitor.rect);
    }
    rect
}

/// PC 沿边坐标 → 平板沿边坐标（6.3.5，返回 `0 .. t_size-1` 的浮点值）。
pub fn pc_to_tablet_along(pc_along: i32, seg: &EdgeSegment, t_size: i32) -> f64 {
    let span = (pc_along as i64 - seg.span_start as i64) as f64;
    let t = span / seg.length().max(1) as f64;
    clamp(t, 0.0, 1.0) * t_size.saturating_sub(1).max(0) as f64
}

/// 平板沿边坐标 → PC 沿边坐标（6.3.5，返回虚拟桌面整数坐标）。
pub fn tablet_to_pc_along(t_along: f64, seg: &EdgeSegment, t_size: i32) -> i32 {
    let t = clamp(t_along / t_size.saturating_sub(1).max(1) as f64, 0.0, 1.0);
    let span = seg.span_start as f64 + t * seg.length().saturating_sub(1).max(0) as f64;
    round_to_i32(span)
}

// ---------------------------------------------------------------------------
// Win32 显示器枚举
// ---------------------------------------------------------------------------

/// 枚举过程中的累积状态（`EnumDisplayMonitors` 的回调没有返回值可用，只能靠它收集）。
#[derive(Default)]
struct EnumContext {
    /// 成功取到的显示器。
    monitors: Vec<MonitorInfo>,
    /// 失败明细；非空时整次枚举报错（与 Python 的 `failures` 一致）。
    failures: Vec<String>,
}

/// 最近一次 Win32 错误码。
fn last_error_code() -> u32 {
    // SAFETY: 无参数、不访问外部内存。
    unsafe { GetLastError() }.0
}

/// 按 NUL 截断的宽字符串转 Rust 字符串（`szDevice` 是定长数组）。
fn wide_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

/// `EnumDisplayMonitors` 的回调。
///
/// 回调不能向外抛异常，所有失败都记进 [`EnumContext::failures`]，枚举结束后统一转成
/// 错误（对应 Python 回调里的 `try/except`）。
unsafe extern "system" fn monitor_enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    if data.0 == 0 {
        return BOOL::from(true);
    }
    let context = unsafe { &mut *(data.0 as *mut EnumContext) };
    let handle = hmon.0 as isize as i64;

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: 输出指针指向本栈帧里有效的 MONITORINFOEXW，cbSize 已填成结构体大小。
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo as *mut MONITORINFO) };
    if !ok.as_bool() {
        context.failures.push(format!(
            "GetMonitorInfoW(0x{handle:X}) 失败: {}",
            last_error_code()
        ));
        return BOOL::from(true);
    }

    let rc = info.monitorInfo.rcMonitor;
    context.monitors.push(MonitorInfo::new(
        handle,
        wide_to_string(&info.szDevice),
        Rect::new(rc.left, rc.top, rc.right, rc.bottom),
        dpi::get_monitor_dpi(handle),
        info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    ));
    BOOL::from(true)
}

/// `EnumDisplayMonitors` + `GetMonitorInfoW` + `GetDpiForMonitor`。
///
/// 主屏优先、其余按位置排序，保证 GUI 与 Router 看到稳定顺序。没有任何显示器时返回
/// 空列表（Python 只记一条 warning），因为「无头会话」不是错误。
fn enum_display_monitors() -> Result<Vec<MonitorInfo>, DesktopError> {
    let mut context = EnumContext::default();
    let data = LPARAM(&mut context as *mut EnumContext as isize);
    // 与 Python 的 ctypes.set_last_error(0) 一致：先清零，失败路径读到的才是本次调用的错误码。
    // SAFETY: 仅写线程本地的 last error，无内存访问。
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: 回调只在本次调用期间被调用，context 的指针在整个调用期间有效。
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(monitor_enum_proc), data) };
    let last_error = last_error_code();
    let mut monitors = context.monitors;

    if !ok.as_bool() && (!monitors.is_empty() || last_error != 0) {
        return Err(DesktopError::EnumFailed { last_error });
    }
    if !context.failures.is_empty() {
        let detail = context
            .failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<String>>()
            .join("; ");
        return Err(DesktopError::EnumDetail(detail));
    }
    if monitors.is_empty() {
        return Ok(monitors);
    }

    monitors.sort_by(|a, b| {
        (!a.primary, a.rect.left, a.rect.top, a.device.as_str()).cmp(&(
            !b.primary,
            b.rect.left,
            b.rect.top,
            b.device.as_str(),
        ))
    });
    Ok(monitors)
}

// ---------------------------------------------------------------------------
// DesktopMap
// ---------------------------------------------------------------------------

/// 虚拟桌面几何快照（6.3）。
///
/// `DesktopMap::new()` 立即枚举真实显示器；[`DesktopMap::from_monitors`] 注入已知
/// 快照（单元测试 / 诊断用），不触碰 win32。刷新是整体替换：构造新的不可变快照后
/// 一次性赋值，读线程不会看到半更新的组合。
pub struct DesktopMap {
    /// Win32 后端（光标读写与后续刷新）。
    api: Arc<dyn DesktopApi>,
    /// 显示器快照。
    monitors: Vec<MonitorInfo>,
    /// 外轮廓段（按 edge/span 稳定排序）。
    segments: Vec<EdgeSegment>,
    /// 所有显示器并集的包围盒。
    bounding_box: Rect,
}

impl DesktopMap {
    /// `DesktopMap()`：立即枚举真实显示器，失败返回错误。
    pub fn new() -> Result<Self, DesktopError> {
        Self::with_api(Arc::new(Win32DesktopApi))
    }

    /// 指定后端的构造器；构造时立即枚举一次。
    pub fn with_api(api: Arc<dyn DesktopApi>) -> Result<Self, DesktopError> {
        let monitors = api.enum_monitors()?;
        Ok(Self::install(api, monitors))
    }

    /// `DesktopMap(monitors=...)`：注入已知快照，不触碰 win32。
    pub fn from_monitors(monitors: Vec<MonitorInfo>) -> Self {
        Self::from_api_and_monitors(Arc::new(Win32DesktopApi), monitors)
    }

    /// 指定后端 + 注入快照（测试同时验证光标委托与几何）。
    pub fn from_api_and_monitors(api: Arc<dyn DesktopApi>, monitors: Vec<MonitorInfo>) -> Self {
        Self::install(api, monitors)
    }

    /// 建快照：外轮廓与包围盒都由显示器列表纯计算得出。
    fn install(api: Arc<dyn DesktopApi>, monitors: Vec<MonitorInfo>) -> Self {
        let segments = compute_exterior_segments(&monitors);
        let bounding_box = bounding_box(&monitors);
        Self {
            api,
            monitors,
            segments,
            bounding_box,
        }
    }

    /// 重新枚举显示器并重建外轮廓（13.4）。
    ///
    /// 失败时保留上一次成功的快照，避免一次瞬时失败把可用几何清空。
    pub fn refresh(&mut self) -> Result<(), DesktopError> {
        let monitors = self.api.enum_monitors()?;
        let segments = compute_exterior_segments(&monitors);
        let bounding_box = bounding_box(&monitors);
        self.monitors = monitors;
        self.segments = segments;
        self.bounding_box = bounding_box;
        Ok(())
    }

    // -- 快照 ---------------------------------------------------------------

    /// 当前显示器快照（主屏在前）。
    pub fn monitors(&self) -> &[MonitorInfo] {
        &self.monitors
    }

    /// 所有显示器并集的包围盒；无显示器时为 `Rect(0, 0, 0, 0)`。
    pub fn bounding_box(&self) -> Rect {
        self.bounding_box
    }

    /// 当前外轮廓段（按 edge/span 稳定排序）。
    pub fn exterior_segments(&self) -> &[EdgeSegment] {
        &self.segments
    }

    /// 当前系统光标的物理像素坐标；查询失败返回 `None`。
    ///
    /// Router 用它把 `RawDelta` 换算成绝对光标位置。若这里缺失，Router 只能把原始
    /// 鼠标计数当像素累加；在开启指针加速时两者会迅速漂移，导致边缘判定落在错误位置。
    /// `None` 表示本次查询失败，Router 应保持上一次已知位置而不是自行累计。
    pub fn cursor_pos(&self) -> Option<(i32, i32)> {
        self.api.try_get_cursor_pos()
    }

    /// 把系统光标移到物理像素坐标；成功返回 `true`。
    pub fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
        self.api.set_cursor_pos(x, y)
    }

    // -- 查询 ---------------------------------------------------------------

    /// 按 handle 找显示器。
    fn monitor_by_handle(&self, handle: i64) -> Option<&MonitorInfo> {
        self.monitors
            .iter()
            .find(|monitor| monitor.handle == handle)
    }

    /// 点所在显示器（右/下开区间，与 `Rect::contains` 一致）。
    pub fn monitor_at(&self, x: i32, y: i32) -> Option<&MonitorInfo> {
        self.monitors
            .iter()
            .find(|monitor| monitor.rect.contains(x, y))
    }

    /// 光标 `(x, y)` 在 1px 带内可命中的外轮廓段（Python 默认 `band=1`）。
    pub fn segments_at(&self, x: i32, y: i32) -> Vec<EdgeSegment> {
        self.segments_at_with_band(x, y, 1)
    }

    /// 返回光标 `(x, y)` 在 `band` 像素内可命中的外轮廓段。
    ///
    /// 垂直边比较 x 与边线（left 边线 = `left-1`，right 边线 = `right`）；水平边比较
    /// y。沿边方向必须落在 `[span_start, span_end)` 内，因此屏幕角落会同时命中两条边，
    /// 交给 [`DesktopMap::resolve_corner`] 裁决。
    pub fn segments_at_with_band(&self, x: i32, y: i32, band: i32) -> Vec<EdgeSegment> {
        let band = band.max(0) as i64;
        let mut found: Vec<EdgeSegment> = Vec::new();
        for seg in &self.segments {
            let Some(monitor) = self.monitor_by_handle(seg.monitor_handle) else {
                continue;
            };
            let line = edge_line(monitor, seg.edge) as i64;
            let (coordinate, along) = if seg.edge.is_vertical() {
                (x as i64, y)
            } else {
                (y as i64, x)
            };
            if (coordinate - line).abs() > band || !seg.contains_along(along) {
                continue;
            }
            found.push(*seg);
        }
        found
    }

    /// `refresh` 后校验旧 `Placement.segment` 是否仍然存在（13.4）。
    pub fn segment_exists(&self, seg: &EdgeSegment) -> bool {
        self.segments.contains(seg)
    }

    /// 摆位是否仍然有效（底层就是段是否还在快照里）。
    pub fn placement_valid(&self, pl: &Placement) -> bool {
        self.segment_exists(&pl.segment)
    }

    // -- 角落裁决 -----------------------------------------------------------

    /// 按 6.3.6 裁决角落同时命中的两条边。
    ///
    /// 方向足够明确（最高分 > 0 且比次高分高 15% 以上）时取最高分；否则取 `priority`
    /// 指定的边，没有偏好则取最高分（分数相同保持输入顺序）。
    pub fn resolve_corner(
        &self,
        segments: &[EdgeSegment],
        delta: (i32, i32),
        priority: Option<Edge>,
    ) -> Option<EdgeSegment> {
        let first = segments.first()?;
        if segments.len() == 1 {
            return Some(*first);
        }

        let (dx, dy) = delta;
        let mut scored: Vec<(EdgeSegment, f64)> = segments
            .iter()
            .map(|seg| {
                let (nx, ny) = seg.edge.outward_normal();
                let score = dx as i64 * nx as i64 + dy as i64 * ny as i64;
                (*seg, score as f64)
            })
            .collect();
        // 稳定排序：分数相同时保持输入顺序（Python 的 sorted 同样是稳定排序）。
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

        let (best, best_score) = scored[0];
        let second_score = scored.get(1).map(|(_, score)| *score).unwrap_or(0.0);
        if best_score > 0.0
            && (best_score - second_score) / best_score.max(1.0) > CORNER_DIRECTION_RATIO
        {
            return Some(best);
        }
        if let Some(priority) = priority {
            if let Some((seg, _)) = scored.iter().find(|(seg, _)| seg.edge == priority) {
                return Some(*seg);
            }
        }
        Some(best)
    }

    // -- 沿边映射 -----------------------------------------------------------

    /// 平板贴边那条边的长度：垂直 PC 边用平板高，水平 PC 边用平板宽。
    fn tablet_along_size(edge: Edge, tablet_w: i32, tablet_h: i32) -> i32 {
        if edge.is_vertical() {
            tablet_h
        } else {
            tablet_w
        }
    }

    /// PC 侧沿边坐标 → 平板入口点（t4/t5 的 `P0`，6.3.5）。
    ///
    /// 平板贴边方向（用户给定）：PC 右边 → 平板左缘 `x=0`；PC 左边 → 平板右缘
    /// `x=W-1`；PC 上边 → 平板下缘 `y=H-1`；PC 下边 → 平板上缘 `y=0`。返回平板
    /// 像素坐标（浮点，供 `warp_to` 使用）。
    pub fn entry_point(
        &self,
        pl: &Placement,
        pc_along: i32,
        tablet_w: i32,
        tablet_h: i32,
    ) -> (f64, f64) {
        let seg = pl.segment;
        let t_size = Self::tablet_along_size(seg.edge, tablet_w, tablet_h);
        let along = pc_to_tablet_along(pc_along, &seg, t_size);
        match seg.edge {
            Edge::Right => (0.0, along),
            Edge::Left => (tablet_w.saturating_sub(1).max(0) as f64, along),
            Edge::Top => (along, tablet_h.saturating_sub(1).max(0) as f64),
            Edge::Bottom => (along, 0.0),
        }
    }

    /// 平板侧沿边坐标 → PC 虚拟桌面落点（t9 的 `SetCursorPos`，6.3.5）。
    ///
    /// `entry_margin_px` / `boundary_inset_px` 缺省取 `ENTRY_MARGIN_PX` /
    /// `BOUNDARY_INSET_PX`。落点距边线 `max(1, entry_margin) + boundary_inset` 像素，
    /// 位于桌面**内部**（Windows 不允许把光标留在所有显示器之外，1px 是边界像素）；
    /// 例如默认值在 right 边得到 `x=right-2`，left 边得到 `x=left+1`。落点还会夹到
    /// 显示器与段范围内，避免极端布局越界。
    pub fn exit_position(
        &self,
        pl: &Placement,
        tablet_along: f64,
        tablet_w: i32,
        tablet_h: i32,
        entry_margin_px: Option<i32>,
        boundary_inset_px: Option<i32>,
    ) -> Result<(i32, i32), DesktopError> {
        let seg = pl.segment;
        if !self.segment_exists(&seg) {
            return Err(DesktopError::SegmentGone {
                edge: seg.edge.as_str(),
                span_start: seg.span_start,
                span_end: seg.span_end,
            });
        }
        let monitor = self
            .monitor_by_handle(seg.monitor_handle)
            .ok_or(DesktopError::MonitorGone {
                handle: seg.monitor_handle,
            })?;
        if monitor.rect.empty() {
            let device = if monitor.device.is_empty() {
                monitor.handle.to_string()
            } else {
                monitor.device.clone()
            };
            return Err(DesktopError::EmptyMonitorRect { device });
        }

        let t_size = Self::tablet_along_size(seg.edge, tablet_w, tablet_h);
        let along = clamp_int(
            tablet_to_pc_along(tablet_along, &seg, t_size) as i64,
            seg.span_start as i64,
            seg.span_start.max(seg.span_end.saturating_sub(1)) as i64,
        ) as i32;

        let margin = entry_margin_px.unwrap_or(ENTRY_MARGIN_PX);
        let inset = boundary_inset_px.unwrap_or(BOUNDARY_INSET_PX);
        let offset = (margin.max(1) + inset.max(0)) as i64;

        let rect = monitor.rect;
        let (left, right) = (rect.left as i64, rect.right as i64);
        let (top, bottom) = (rect.top as i64, rect.bottom as i64);
        let (x, y) = match seg.edge {
            Edge::Right => (clamp_int(right - offset, left, right - 1) as i32, along),
            Edge::Left => (clamp_int(left + offset - 1, left, right - 1) as i32, along),
            Edge::Top => (along, clamp_int(top + offset - 1, top, bottom - 1) as i32),
            Edge::Bottom => (along, clamp_int(bottom - offset, top, bottom - 1) as i32),
        };
        Ok((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const TABLET_W: i32 = 3200;
    const TABLET_H: i32 = 2136;

    // -- 夹具（与 tests/fixtures/desktop_*.json 逐字段一致）------------------

    fn monitor(
        handle: i64,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
        dpi: u32,
        primary: bool,
    ) -> MonitorInfo {
        MonitorInfo::new(
            handle,
            format!(r"\\.\DISPLAY{handle}"),
            Rect::new(left, top, left + width, top + height),
            dpi,
            primary,
        )
    }

    fn fixtures() -> Vec<(&'static str, Vec<MonitorInfo>)> {
        vec![
            ("single", vec![monitor(1, 0, 0, 1920, 1080, 96, true)]),
            (
                "dual_horizontal",
                vec![
                    monitor(1, 0, 0, 1920, 1080, 96, true),
                    monitor(2, 1920, 0, 1920, 1080, 144, false),
                ],
            ),
            (
                "dual_vertical",
                vec![
                    monitor(1, 0, 0, 1920, 1080, 96, true),
                    monitor(2, 0, 1080, 1920, 1080, 96, false),
                ],
            ),
            (
                "l_shape",
                vec![
                    monitor(1, 0, 0, 1920, 1080, 96, true),
                    monitor(2, 0, 1080, 1280, 1024, 96, false),
                ],
            ),
            (
                "t_shape",
                vec![
                    monitor(1, 0, 0, 1280, 1024, 96, true),
                    monitor(2, 1280, 0, 1280, 1024, 96, false),
                    monitor(3, 640, 1024, 1280, 1024, 96, false),
                ],
            ),
            (
                "negative",
                vec![
                    monitor(1, 0, 0, 1920, 1080, 96, true),
                    monitor(2, -1920, 0, 1920, 1080, 96, false),
                ],
            ),
        ]
    }

    fn layout(name: &str) -> Vec<MonitorInfo> {
        fixtures()
            .into_iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, monitors)| monitors)
            .expect("夹具名必须存在")
    }

    fn map_of(name: &str) -> DesktopMap {
        DesktopMap::from_monitors(layout(name))
    }

    fn spans_for(segments: &[EdgeSegment], edge: Edge) -> Vec<(i32, i32)> {
        let mut spans: Vec<(i32, i32)> = segments
            .iter()
            .filter(|seg| seg.edge == edge)
            .map(|seg| (seg.span_start, seg.span_end))
            .collect();
        spans.sort_unstable();
        spans
    }

    fn segments_by_edge(map: &DesktopMap) -> HashMap<Edge, EdgeSegment> {
        map.exterior_segments()
            .iter()
            .map(|seg| (seg.edge, *seg))
            .collect()
    }

    fn placement(seg: EdgeSegment) -> Placement {
        Placement::new(seg, 0.5, Align::Proportional, None).expect("默认摆位合法")
    }

    fn entry(map: &DesktopMap, pl: &Placement, pc_along: i32) -> (f64, f64) {
        map.entry_point(pl, pc_along, TABLET_W, TABLET_H)
    }

    fn exit(map: &DesktopMap, pl: &Placement, tablet_along: f64) -> (i32, i32) {
        map.exit_position(pl, tablet_along, TABLET_W, TABLET_H, None, None)
            .expect("落点应有效")
    }

    fn approx(value: f64, expected: f64) {
        assert!(
            (value - expected).abs() < 1e-9,
            "{value} != {expected}（允许 1e-9 误差）"
        );
    }

    /// 可编程的假后端：枚举结果可在测试中切换，光标读写全程记录。
    struct FakeDesktopApi {
        monitors: Mutex<Result<Vec<MonitorInfo>, ()>>,
        cursor: Option<(i32, i32)>,
        moves: Mutex<Vec<(i32, i32)>>,
    }

    impl FakeDesktopApi {
        fn new(monitors: Result<Vec<MonitorInfo>, ()>) -> Self {
            Self {
                monitors: Mutex::new(monitors),
                cursor: None,
                moves: Mutex::new(Vec::new()),
            }
        }

        fn with_cursor(cursor: Option<(i32, i32)>) -> Self {
            Self {
                monitors: Mutex::new(Ok(Vec::new())),
                cursor,
                moves: Mutex::new(Vec::new()),
            }
        }

        fn set_monitors(&self, monitors: Result<Vec<MonitorInfo>, ()>) {
            if let Ok(mut slot) = self.monitors.lock() {
                *slot = monitors;
            }
        }

        fn moves(&self) -> Vec<(i32, i32)> {
            self.moves
                .lock()
                .map(|moves| moves.clone())
                .unwrap_or_default()
        }
    }

    impl DesktopApi for FakeDesktopApi {
        fn enum_monitors(&self) -> Result<Vec<MonitorInfo>, DesktopError> {
            match self.monitors.lock() {
                Ok(slot) => slot.clone().map_err(|_| DesktopError::EnumFailed { last_error: 5 }),
                Err(_) => Err(DesktopError::EnumFailed { last_error: 5 }),
            }
        }

        fn try_get_cursor_pos(&self) -> Option<(i32, i32)> {
            self.cursor
        }

        fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
            if let Ok(mut moves) = self.moves.lock() {
                moves.push((x, y));
            }
            true
        }
    }

    // -- 类型契约与纯函数 ---------------------------------------------------

    #[test]
    fn geometry_types_are_reexported_not_redefined() {
        // 编译期证明 desktop 重新导出的就是 logic/geometry 的类型（Python 用的是 is 断言）。
        let edge: crate::logic::geometry::Edge = Edge::Left;
        let align: crate::logic::geometry::Align = Align::Center;
        let rect: crate::logic::geometry::Rect = Rect::new(0, 0, 1920, 1080);
        let info: crate::logic::geometry::MonitorInfo =
            MonitorInfo::new(1, r"\\.\DISPLAY1", rect, 96, true);
        let segment: crate::logic::geometry::EdgeSegment =
            EdgeSegment::new(info.handle, edge, 0, 1080).expect("合法段");
        let placed: crate::logic::geometry::Placement =
            Placement::new(segment, 0.5, align, None).expect("合法摆位");
        assert_eq!(placed.segment, segment);
        assert_eq!(rect.width(), 1920);
    }

    #[test]
    fn compute_exterior_segments_is_pure_and_order_independent() {
        for (name, monitors) in fixtures() {
            let map = DesktopMap::from_monitors(monitors.clone());
            assert_eq!(
                compute_exterior_segments(&monitors),
                map.exterior_segments(),
                "{name}"
            );
            let mut reversed = monitors;
            reversed.reverse();
            assert_eq!(
                compute_exterior_segments(&reversed),
                map.exterior_segments(),
                "{name}（逆序显示器）"
            );
        }
    }

    #[test]
    fn empty_desktop_map() {
        let map = DesktopMap::from_monitors(Vec::new());
        assert!(map.monitors().is_empty());
        assert!(map.exterior_segments().is_empty());
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 0, 0));
        assert!(map.monitor_at(0, 0).is_none());
        assert!(map.segments_at(0, 0).is_empty());
        assert!(map.resolve_corner(&[], (1, 1), Some(Edge::Top)).is_none());
    }

    // -- 布局：单屏 / 左右双屏 / 上下双屏 -----------------------------------

    #[test]
    fn single_screen_exterior_segments() {
        let map = map_of("single");
        let expected = vec![
            EdgeSegment::new(1, Edge::Left, 0, 1080).expect("合法段"),
            EdgeSegment::new(1, Edge::Right, 0, 1080).expect("合法段"),
            EdgeSegment::new(1, Edge::Top, 0, 1920).expect("合法段"),
            EdgeSegment::new(1, Edge::Bottom, 0, 1920).expect("合法段"),
        ];
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 4);
        assert_eq!(segments, expected.as_slice());
        assert_eq!(
            segments.iter().map(|seg| seg.edge).collect::<Vec<_>>(),
            vec![Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
        );
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 1920, 1080));
        assert_eq!(map.monitor_at(0, 0), Some(&map.monitors()[0]));
        assert_eq!(map.monitor_at(1919, 1079), Some(&map.monitors()[0]));
        assert!(map.monitor_at(1920, 0).is_none());
        assert!(map.monitor_at(-1, 0).is_none());
    }

    #[test]
    fn dual_horizontal_merges_outer_edges() {
        let map = map_of("dual_horizontal");
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 4);
        assert_eq!(spans_for(segments, Edge::Left), vec![(0, 1080)]);
        assert_eq!(spans_for(segments, Edge::Right), vec![(0, 1080)]);
        assert_eq!(spans_for(segments, Edge::Top), vec![(0, 3840)]);
        assert_eq!(spans_for(segments, Edge::Bottom), vec![(0, 3840)]);
        for seg in segments {
            if seg.edge == Edge::Top || seg.edge == Edge::Bottom {
                assert_eq!(seg.length(), 3840);
            }
        }
        // 显示器之间的竖直内缝不能成为吸附边。
        assert!(map.segments_at(1920, 500).is_empty());
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 3840, 1080));
        assert_eq!(map.monitor_at(1919, 0), Some(&map.monitors()[0]));
        assert_eq!(map.monitor_at(1920, 0), Some(&map.monitors()[1]));
        assert_eq!(map.monitor_at(2000, 0), Some(&map.monitors()[1]));
        assert_eq!(map.monitor_at(2000, 0).map(|m| m.dpi), Some(144));
        approx(map.monitors()[1].scale(), 1.5);
        // 内缝两侧的 left 边 x 不同，不能被合并成一条（PITFALLS：仅比 edge+span 会出错）。
        let right = segments_by_edge(&map)[&Edge::Right];
        assert_eq!(right.monitor_handle, 2, "右侧外轮廓必须属于副屏");
    }

    #[test]
    fn dual_vertical_merges_side_edges() {
        let map = map_of("dual_vertical");
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 4);
        assert_eq!(spans_for(segments, Edge::Left), vec![(0, 2160)]);
        assert_eq!(spans_for(segments, Edge::Right), vec![(0, 2160)]);
        assert_eq!(spans_for(segments, Edge::Top), vec![(0, 1920)]);
        assert_eq!(spans_for(segments, Edge::Bottom), vec![(0, 1920)]);
        // 上下双屏之间的水平内缝同样不可吸附。
        assert!(map.segments_at(500, 1080).is_empty());
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 1920, 2160));
        assert_eq!(map.monitor_at(0, 1079), Some(&map.monitors()[0]));
        assert_eq!(map.monitor_at(0, 1080), Some(&map.monitors()[1]));
    }

    // -- 布局：L 形 / T 形 / 负坐标 -----------------------------------------

    #[test]
    fn l_shape_exterior_segments() {
        let map = map_of("l_shape");
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 6);
        assert_eq!(spans_for(segments, Edge::Left), vec![(0, 2104)]);
        assert_eq!(spans_for(segments, Edge::Right), vec![(0, 1080), (1080, 2104)]);
        assert_eq!(spans_for(segments, Edge::Top), vec![(0, 1920)]);
        assert_eq!(
            spans_for(segments, Edge::Bottom),
            vec![(0, 1280), (1280, 1920)]
        );
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 1920, 2104));
        assert_eq!(map.monitor_at(1279, 2103), Some(&map.monitors()[1]));
        assert!(map.monitor_at(1280, 1080).is_none());
        assert!(map.segments_at(1280, 500).is_empty());
    }

    #[test]
    fn t_shape_three_monitors_exterior_segments() {
        let map = map_of("t_shape");
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 8);
        assert_eq!(spans_for(segments, Edge::Top), vec![(0, 2560)]);
        assert_eq!(
            spans_for(segments, Edge::Bottom),
            vec![(0, 640), (640, 1920), (1920, 2560)]
        );
        assert_eq!(spans_for(segments, Edge::Left), vec![(0, 1024), (1024, 2048)]);
        assert_eq!(
            spans_for(segments, Edge::Right),
            vec![(0, 1024), (1024, 2048)]
        );
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 2560, 2048));
        assert!(map.segments_at(1280, 500).is_empty());
        assert!(map.segments_at(1000, 1024).is_empty());
        assert!(map.monitor_at(639, 1024).is_none());
        assert_eq!(map.monitor_at(640, 1024), Some(&map.monitors()[2]));
    }

    #[test]
    fn negative_coordinates_exterior_segments() {
        let map = map_of("negative");
        let segments = map.exterior_segments();
        assert_eq!(segments.len(), 4);
        assert_eq!(spans_for(segments, Edge::Left), vec![(0, 1080)]);
        assert_eq!(spans_for(segments, Edge::Right), vec![(0, 1080)]);
        assert_eq!(spans_for(segments, Edge::Top), vec![(-1920, 1920)]);
        assert_eq!(spans_for(segments, Edge::Bottom), vec![(-1920, 1920)]);
        assert_eq!(map.bounding_box(), Rect::new(-1920, 0, 1920, 1080));
        assert_eq!(map.monitor_at(-1920, 0), Some(&map.monitors()[1]));
        assert_eq!(map.monitor_at(-1, 50), Some(&map.monitors()[1]));
        assert_eq!(map.monitor_at(0, 0), Some(&map.monitors()[0]));
        assert!(map.monitor_at(1920, 0).is_none());
        // x=0 是两屏之间的内缝，不是外轮廓。
        assert!(map.segments_at(0, 500).is_empty());
        assert_eq!(
            spans_for(&map.segments_at_with_band(-1921, 500, 1), Edge::Left),
            vec![(0, 1080)]
        );
    }

    #[test]
    fn merge_requires_same_edge_line() {
        let first = MonitorInfo::new(1, r"\\.\DISPLAY1", Rect::new(0, 0, 100, 100), 96, true);
        let second = MonitorInfo::new(2, r"\\.\DISPLAY2", Rect::new(200, 100, 300, 200), 96, false);
        let segments = compute_exterior_segments(&[first, second]);
        let mut lefts: Vec<(i32, i32)> = segments
            .iter()
            .filter(|seg| seg.edge == Edge::Left)
            .map(|seg| (seg.span_start, seg.span_end))
            .collect();
        lefts.sort_unstable();
        // 两条 left 边 x 不同（-1 与 199），不能合并成 0..200。
        assert_eq!(lefts, vec![(0, 100), (100, 200)]);
    }

    // -- 命中 / 角落裁决 / 沿边映射 -----------------------------------------

    #[test]
    fn segments_at_band_and_corners() {
        let map = map_of("single");
        let by_edge = segments_by_edge(&map);
        let left = by_edge[&Edge::Left];
        let top = by_edge[&Edge::Top];
        assert_eq!(map.segments_at(-1, 500), vec![left]);
        assert!(map.segments_at(-3, 500).is_empty());
        assert_eq!(map.segments_at_with_band(-3, 500, 2), vec![left]);
        assert_eq!(map.segments_at_with_band(-1, 500, 0), vec![left]);
        assert_eq!(map.segments_at_with_band(-1, 500, -5), vec![left]);

        let mut corner = map.segments_at(0, 0);
        corner.sort_by_key(|seg| edge_index(seg.edge));
        assert_eq!(corner, vec![left, top]);

        let mut bottom_right = map
            .segments_at(1919, 1079)
            .iter()
            .map(|seg| seg.edge)
            .collect::<Vec<_>>();
        bottom_right.sort_by_key(|edge| edge_index(*edge));
        assert_eq!(bottom_right, vec![Edge::Right, Edge::Bottom]);
        assert_eq!(
            map.segments_at(0, 1080)
                .iter()
                .map(|seg| seg.edge)
                .collect::<Vec<_>>(),
            vec![Edge::Bottom]
        );
        assert!(map.segments_at(960, 540).is_empty());
    }

    #[test]
    fn resolve_corner_rules() {
        let map = map_of("single");
        let by_edge = segments_by_edge(&map);
        let left = by_edge[&Edge::Left];
        let top = by_edge[&Edge::Top];
        let corner = [left, top];

        assert!(map.resolve_corner(&[], (1, 1), Some(Edge::Top)).is_none());
        assert_eq!(
            map.resolve_corner(&[left], (1, 1), Some(Edge::Top)),
            Some(left)
        );
        assert_eq!(
            map.resolve_corner(&corner, (-1, 0), Some(Edge::Top)),
            Some(left)
        );
        assert_eq!(
            map.resolve_corner(&corner, (0, -1), Some(Edge::Left)),
            Some(top)
        );
        assert_eq!(map.resolve_corner(&corner, (-1, -1), Some(Edge::Top)), Some(top));
        assert_eq!(map.resolve_corner(&corner, (-1, -1), None), Some(left));
        assert_eq!(map.resolve_corner(&corner, (0, 0), Some(Edge::Top)), Some(top));
        // 分数均负，走 priority。
        assert_eq!(map.resolve_corner(&corner, (1, 1), Some(Edge::Top)), Some(top));
        // 方向差 > 15%。
        assert_eq!(
            map.resolve_corner(&corner, (-100, -1), Some(Edge::Top)),
            Some(left)
        );
        // 方向差 <= 15%，走 priority。
        assert_eq!(
            map.resolve_corner(&corner, (-10, -9), Some(Edge::Top)),
            Some(top)
        );
        // 无匹配 priority → 最高分。
        assert_eq!(
            map.resolve_corner(&corner, (1, 1), Some(Edge::Right)),
            Some(left)
        );
    }

    #[test]
    fn along_mapping_formulas() {
        let map = map_of("single");
        let by_edge = segments_by_edge(&map);
        let right = by_edge[&Edge::Right];
        let top = by_edge[&Edge::Top];

        approx(pc_to_tablet_along(0, &right, TABLET_H), 0.0);
        approx(pc_to_tablet_along(540, &right, TABLET_H), 1067.5);
        approx(
            pc_to_tablet_along(1079, &right, TABLET_H),
            1079.0 / 1080.0 * 2135.0,
        );
        approx(pc_to_tablet_along(1080, &right, TABLET_H), 2135.0);
        approx(pc_to_tablet_along(-50, &right, TABLET_H), 0.0);

        assert_eq!(tablet_to_pc_along(0.0, &right, TABLET_H), 0);
        assert_eq!(tablet_to_pc_along(1067.5, &right, TABLET_H), 540);
        assert_eq!(tablet_to_pc_along(2135.0, &right, TABLET_H), 1079);
        assert_eq!(tablet_to_pc_along(-10.0, &right, TABLET_H), 0);
        assert_eq!(tablet_to_pc_along(9999.0, &right, TABLET_H), 1079);

        // 水平边用平板宽；t_size=1 时避免除零。
        approx(pc_to_tablet_along(960, &top, TABLET_W), 1599.5);
        assert_eq!(tablet_to_pc_along(1599.5, &top, TABLET_W), 960);
        approx(pc_to_tablet_along(500, &right, 1), 0.0);
        assert_eq!(tablet_to_pc_along(0.0, &right, 1), 0);
        // t_size=1 时非零 t 夹到 1。
        assert_eq!(tablet_to_pc_along(50.0, &right, 1), 1079);
    }

    // -- 入口 / 出口坐标 -----------------------------------------------------

    #[test]
    fn entry_and_exit_single_screen() {
        let map = map_of("single");
        let by_edge = segments_by_edge(&map);

        let right = placement(by_edge[&Edge::Right]);
        assert_eq!(entry(&map, &right, 540), (0.0, 1067.5));
        assert_eq!(exit(&map, &right, 1067.5), (1918, 540));
        assert_eq!(exit(&map, &right, -1.0), (1918, 0));
        assert_eq!(exit(&map, &right, 10_000.0), (1918, 1079));

        let left = placement(by_edge[&Edge::Left]);
        assert_eq!(entry(&map, &left, 540), (3199.0, 1067.5));
        assert_eq!(exit(&map, &left, 1067.5), (1, 540));

        let top = placement(by_edge[&Edge::Top]);
        assert_eq!(entry(&map, &top, 960), (1599.5, 2135.0));
        assert_eq!(exit(&map, &top, 1599.5), (960, 1));

        let bottom = placement(by_edge[&Edge::Bottom]);
        assert_eq!(entry(&map, &bottom, 960), (1599.5, 0.0));
        assert_eq!(exit(&map, &bottom, 1599.5), (960, 1078));
    }

    #[test]
    fn exit_position_margin_and_inset_overrides() {
        let map = map_of("single");
        let right = placement(segments_by_edge(&map)[&Edge::Right]);
        let case = |margin: Option<i32>, inset: Option<i32>| {
            map.exit_position(&right, 1067.5, TABLET_W, TABLET_H, margin, inset)
                .expect("落点应有效")
        };
        assert_eq!(case(Some(1), None), (1919, 540));
        assert_eq!(case(Some(0), None), (1919, 540));
        assert_eq!(case(Some(-5), None), (1919, 540));
        assert_eq!(case(None, Some(8)), (1910, 540));

        let left = placement(segments_by_edge(&map)[&Edge::Left]);
        let case_left = |margin: Option<i32>, inset: Option<i32>| {
            map.exit_position(&left, 1067.5, TABLET_W, TABLET_H, margin, inset)
                .expect("落点应有效")
        };
        assert_eq!(case_left(Some(1), None), (0, 540));
        assert_eq!(case_left(None, Some(8)), (9, 540));
    }

    #[test]
    fn entry_and_exit_on_merged_multi_monitor_segments() {
        let map = map_of("dual_horizontal");
        let by_edge = segments_by_edge(&map);

        let top = placement(by_edge[&Edge::Top]);
        assert_eq!((top.segment.span_start, top.segment.span_end), (0, 3840));
        assert_eq!(entry(&map, &top, 960), (799.75, 2135.0));
        assert_eq!(exit(&map, &top, 799.75), (960, 1));
        assert_eq!(exit(&map, &top, 3199.0), (3839, 1));

        let right = placement(by_edge[&Edge::Right]);
        assert_eq!(right.segment.span_end, 1080);
        assert_eq!(entry(&map, &right, 540), (0.0, 1067.5));
        // 右侧外轮廓属于副屏（x=3840），落点必须跑到 3838 而不是 1918。
        assert_eq!(exit(&map, &right, 1067.5), (3838, 540));
    }

    #[test]
    fn exit_position_negative_coordinates() {
        let map = map_of("negative");
        let by_edge = segments_by_edge(&map);
        let right = placement(by_edge[&Edge::Right]);
        let left = placement(by_edge[&Edge::Left]);
        assert_eq!(exit(&map, &right, 1067.5), (1918, 540));
        assert_eq!(exit(&map, &left, 1067.5), (-1919, 540));
    }

    #[test]
    fn exit_position_rejects_stale_monitor() {
        let map = map_of("single");
        let stale = placement(EdgeSegment::new(99, Edge::Right, 0, 1080).expect("合法段"));
        let err = map
            .exit_position(&stale, 0.0, TABLET_W, TABLET_H, None, None)
            .expect_err("陈旧段必须报错");
        assert_eq!(err.code(), "E301");
        assert_eq!(err.to_string(), "段 right [0, 1080) 已不存在");

        // 另外两条防御分支的文案（状态由显示器列表构建，正常构造下走不到）。
        assert_eq!(
            DesktopError::MonitorGone {
                handle: 0x1_0000_0001
            }
            .to_string(),
            "段引用的显示器 handle=0x100000001 已不存在"
        );
        assert_eq!(
            DesktopError::EmptyMonitorRect {
                device: "1".to_string()
            }
            .to_string(),
            "显示器 1 的矩形为空"
        );
        assert_eq!(
            DesktopError::EnumFailed { last_error: 87 }.to_string(),
            "EnumDisplayMonitors 失败 (GetLastError=87)"
        );
        assert_eq!(DesktopError::EnumFailed { last_error: 87 }.code(), "E900");
    }

    #[test]
    fn segment_exists_and_placement_valid() {
        let map = map_of("single");
        let seg = map.exterior_segments()[0];
        assert!(map.segment_exists(&seg));
        assert!(map.placement_valid(&placement(seg)));
        let stale = EdgeSegment::new(seg.monitor_handle, seg.edge, seg.span_start + 1, seg.span_end)
            .expect("合法段");
        assert!(!map.segment_exists(&stale));
        assert!(!map.placement_valid(&placement(stale)));
        assert!(!map.segment_exists(&EdgeSegment::new(99, Edge::Left, 0, 1).expect("合法段")));
    }

    // -- 刷新 / 光标委托 -----------------------------------------------------

    #[test]
    fn refresh_keeps_snapshot_on_failure() {
        let api = Arc::new(FakeDesktopApi::new(Ok(layout("single"))));
        let mut map = DesktopMap::with_api(api.clone()).expect("首次枚举成功");
        assert_eq!(map.monitors().len(), 1);
        assert_eq!(map.exterior_segments().len(), 4);

        // 枚举失败：错误上抛，旧快照（含外轮廓）必须原样保留。
        api.set_monitors(Err(()));
        let err = map.refresh().expect_err("枚举失败必须报错");
        assert_eq!(err.code(), "E900");
        assert_eq!(map.monitors().len(), 1);
        assert_eq!(map.exterior_segments().len(), 4);

        // 成功后整体替换。
        api.set_monitors(Ok(layout("dual_horizontal")));
        map.refresh().expect("第二次刷新成功");
        assert_eq!(map.monitors().len(), 2);
        assert_eq!(map.bounding_box(), Rect::new(0, 0, 3840, 1080));
        assert_eq!(map.exterior_segments().len(), 4);
    }

    #[test]
    fn desktop_cursor_helpers_delegate_to_dpi() {
        let api = Arc::new({
            let fake = FakeDesktopApi::with_cursor(Some((321, 654)));
            fake.set_monitors(Ok(layout("single")));
            fake
        });
        let map = DesktopMap::from_api_and_monitors(api.clone(), layout("single"));
        assert_eq!(map.cursor_pos(), Some((321, 654)));
        assert!(map.set_cursor_pos(10, 20));
        assert_eq!(api.moves(), vec![(10, 20)]);
    }

    #[test]
    fn desktop_cursor_helpers_return_none_on_failure() {
        let api = Arc::new(FakeDesktopApi::with_cursor(None));
        let map = DesktopMap::from_api_and_monitors(api, layout("single"));
        assert!(map.cursor_pos().is_none());
    }

    #[test]
    fn constants_used_by_geometry() {
        assert_eq!(ENTRY_MARGIN_PX, 2);
        assert_eq!(BOUNDARY_INSET_PX, 0);
        approx(CORNER_DIRECTION_RATIO, 0.15);
    }

    /// 真实枚举的冒烟用例：只读，不改任何系统状态（不建窗口、不动光标）。
    ///
    /// 与下面的严格用例不同，无头 / 服务会话（枚举为空或失败）也算通过，只校验失败
    /// 码是 E900，因此可以默认运行；严格断言留给
    /// `real_enumeration_returns_usable_monitors`（#[ignore]）。
    #[test]
    fn real_enumeration_smoke_is_read_only() {
        match DesktopMap::new() {
            Ok(map) => {
                assert!(
                    map.monitors().iter().filter(|m| m.primary).count() <= 1,
                    "主屏最多一块"
                );
                for monitor in map.monitors() {
                    assert!(monitor.rect.width() > 0, "{monitor:?}");
                    assert!(monitor.rect.height() > 0, "{monitor:?}");
                    assert!(monitor.dpi > 0, "{monitor:?}");
                }
                for seg in map.exterior_segments() {
                    assert!(seg.length() > 0, "{seg:?}");
                    assert!(map.monitors().iter().any(|m| m.handle == seg.monitor_handle));
                }
            }
            // 无头 / 服务会话下 EnumDisplayMonitors 可能失败；这不影响本模块的几何正确性。
            Err(err) => assert_eq!(err.code(), "E900"),
        }
    }

    /// 真实枚举显示器：不改任何系统状态，但会读取用户当前的显示器布局。
    ///
    /// 默认不跑（CI / 远程会话可能是无头环境，枚举结果为空会让断言失败），
    /// 桌面会话里用 cargo test -- --ignored 手工验证。
    #[test]
    #[ignore = "会枚举真实显示器；CI 无头/远程会话下可能为空，需在桌面会话里手工运行"]
    fn real_enumeration_returns_usable_monitors() {
        let map = DesktopMap::new().expect("真实枚举应成功");
        // cargo test -- --ignored --nocapture 时打印实际布局，便于人工核对换行/负坐标。
        for monitor in map.monitors() {
            println!(
                "monitor handle=0x{:X} device={} rect=({}, {}, {}, {}) dpi={} primary={}",
                monitor.handle,
                monitor.device,
                monitor.rect.left,
                monitor.rect.top,
                monitor.rect.right,
                monitor.rect.bottom,
                monitor.dpi,
                monitor.primary
            );
        }
        for seg in map.exterior_segments() {
            println!(
                "segment edge={} span=[{}, {}) handle=0x{:X}",
                seg.edge.as_str(),
                seg.span_start,
                seg.span_end,
                seg.monitor_handle
            );
        }
        assert!(!map.monitors().is_empty(), "至少一块显示器");
        for monitor in map.monitors() {
            assert!(monitor.rect.width() > 0);
            assert!(monitor.rect.height() > 0);
            assert!(monitor.dpi > 0);
            assert!(!monitor.device.is_empty());
        }
        assert!(map.monitors().iter().filter(|m| m.primary).count() <= 1);
        assert!(!map.exterior_segments().is_empty());

        // 主屏排在最前；顺序只由 (not primary, left, top, device) 决定。
        let flags: Vec<bool> = map.monitors().iter().map(|m| m.primary).collect();
        let mut sorted = flags.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(flags, sorted, "主屏必须排在所有副屏之前");

        // 真实枚举出来的每个段都必须能映射回自己的显示器。
        for seg in map.exterior_segments() {
            assert!(seg.length() > 0, "外轮廓段长度必须为正：{seg:?}");
            assert!(map.monitors().iter().any(|m| m.handle == seg.monitor_handle));
        }
    }
}
