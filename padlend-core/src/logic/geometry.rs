//! 纯几何数据类型（对应 `core/geometry.py`，设计文档 6.3 / 8.1）。
//!
//! Python 版把类型定义放在 `core/geometry.py`、由 `win/desktop.py` 按契约重新
//! 导出，目的是让 `core` / `tablet` 层不必 import win32 平台代码。Rust 侧理由
//! 相同：本模块只依赖标准库与 serde，不 import Win32；`win` 层、Router 与画布
//! 都从这里取类型。
//!
//! 坐标契约与 Python 完全一致：虚拟桌面用**物理像素**，主屏左上角为原点，
//! 副屏在主屏左侧/上方时坐标为负（13.3）；矩形的 `right` / `bottom` 是开区间。
//!
//! 序列化形状与 `config.py` 的 `placement` / `layouts.devices.*.<orientation>`
//! 一致，可以直接落进 config.yaml：
//!
//! ```yaml
//! monitor_handle: 1
//! edge: right
//! span_start: 0
//! span_end: 1600
//! offset: 0.5
//! align: proportional
//! corner_priority: right
//! ```
//!
//! 字段名与枚举取值都对齐 Python：`edge` / `align` / `corner_priority` 是下划线
//! 枚举的小写字符串（`align` 的 `one_to_one` 带下划线）；`corner_priority` 为
//! `None` 时写空串，与 `PlacementCfg.corner_priority = ""` 一致。
//!
//! 与配置文件唯一的差异是显示器身份：文件里 `monitor` 是设备名
//! （`\"\\\\\\\\.\\\\DISPLAY1\"`，见 `config.py:PlacementCfg.monitor`），运行时类型只能
//! 拿到 `monitor_handle`，因此字段名是 `monitor_handle`；设备名 ↔ handle 的换算
//! 属于 config / win 层职责（Python 在 `service/api.py` 里做）。

use std::fmt;
use std::str::FromStr;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// 几何层的错误：对应 Python 里抛 `ValueError` 的地方。
///
/// 消息文本逐字对齐 Python，便于两侧日志 diff。
#[derive(Debug, Error, PartialEq)]
pub enum GeometryError {
    /// `EdgeSegment.span_end < span_start`（Python `__post_init__` 的 ValueError）。
    #[error("EdgeSegment span_end({span_end}) < span_start({span_start})")]
    InvertedSpan { span_start: i32, span_end: i32 },
    /// `Placement.offset` 超出 `[0.0, 1.0]`（Python `__post_init__` 的 ValueError）。
    #[error("Placement.offset out of range: {offset:?}")]
    OffsetOutOfRange { offset: f64 },
    /// 非法的边名（Python `Edge("x")` 的 ValueError）。
    #[error("'{value}' is not a valid Edge")]
    UnknownEdge { value: String },
    /// 非法的对齐名（Python `Align("x")` 的 ValueError）。
    #[error("'{value}' is not a valid Align")]
    UnknownAlign { value: String },
}

// ---------------------------------------------------------------------------
// Edge / Align
// ---------------------------------------------------------------------------

/// 虚拟桌面的四条外轮廓边（Python `Edge(StrEnum)`）。
///
/// YAML/JSON 里是小写字符串 `left` / `right` / `top` / `bottom`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    /// 左边缘：span 沿 y 轴。
    Left,
    /// 右边缘：span 沿 y 轴。
    Right,
    /// 上边缘：span 沿 x 轴。
    Top,
    /// 下边缘：span 沿 x 轴。
    Bottom,
}

/// 四条边的固定顺序（对应 `win/desktop.py:_EDGES`，GUI 绘制与段排序都依赖它）。
pub const ALL_EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

impl Edge {
    /// 枚举的小写字符串取值（等价于 Python StrEnum 的 `.value`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    /// 指向屏幕外侧的单位向量（6.3.6 角落裁决用）。
    pub fn outward_normal(&self) -> (i32, i32) {
        match self {
            Edge::Left => (-1, 0),
            Edge::Right => (1, 0),
            Edge::Top => (0, -1),
            Edge::Bottom => (0, 1),
        }
    }

    /// `left` / `right` 的 span 沿 y 轴；`top` / `bottom` 沿 x 轴。
    pub fn is_vertical(&self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// 对边（穿屏后“入口边”就是吸附边的对边）。
    pub fn opposite(&self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Edge {
    type Err = GeometryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "left" => Ok(Edge::Left),
            "right" => Ok(Edge::Right),
            "top" => Ok(Edge::Top),
            "bottom" => Ok(Edge::Bottom),
            other => Err(GeometryError::UnknownEdge {
                value: other.to_string(),
            }),
        }
    }
}

impl TryFrom<&str> for Edge {
    type Error = GeometryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// 平板在边段内的对齐方式（Python `Align(StrEnum)`）。
///
/// 默认 `Proportional`（`proportional`）：按段内相对位置摆放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    /// 按比例（默认）。
    #[default]
    Proportional,
    /// 居中。
    Center,
    /// 1:1。
    OneToOne,
}

impl Align {
    /// 枚举的小写字符串取值（等价于 Python StrEnum 的 `.value`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Align::Proportional => "proportional",
            Align::Center => "center",
            Align::OneToOne => "one_to_one",
        }
    }
}

impl fmt::Display for Align {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Align {
    type Err = GeometryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "proportional" => Ok(Align::Proportional),
            "center" => Ok(Align::Center),
            "one_to_one" => Ok(Align::OneToOne),
            other => Err(GeometryError::UnknownAlign {
                value: other.to_string(),
            }),
        }
    }
}

impl TryFrom<&str> for Align {
    type Error = GeometryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

// ---------------------------------------------------------------------------
// Rect
// ---------------------------------------------------------------------------

/// 虚拟桌面矩形，物理像素，`right` / `bottom` 为开区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    /// 左边界（含）。
    pub left: i32,
    /// 上边界（含）。
    pub top: i32,
    /// 右边界（不含）。
    pub right: i32,
    /// 下边界（不含）。
    pub bottom: i32,
}

impl Rect {
    /// 与 Python 位置参数同序的构造器：`Rect(left, top, right, bottom)`。
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// 宽度；空矩形可能是 0 或负数。
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// 高度；空矩形可能是 0 或负数。
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// 宽或高 `<= 0` 即视为空。
    pub fn empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    /// 点是否在矩形内（右/下开区间）。
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.left <= x && x < self.right && self.top <= y && y < self.bottom
    }

    /// 两矩形的并集（逐分量 min/max，不要求相交）。
    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// 中心点（浮点，虚拟桌面物理像素）。
    pub fn center(&self) -> (f64, f64) {
        (
            self.left as f64 + self.width() as f64 / 2.0,
            self.top as f64 + self.height() as f64 / 2.0,
        )
    }

    /// 与 Python `as_tuple()` 相同顺序的四元组。
    pub fn as_tuple(&self) -> (i32, i32, i32, i32) {
        (self.left, self.top, self.right, self.bottom)
    }
}

// ---------------------------------------------------------------------------
// MonitorInfo
// ---------------------------------------------------------------------------

/// 一台显示器。`rect` 为物理像素，副屏在主屏左侧时可为负坐标（13.3）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Win32 的 `HMONITOR`（在 `EnumDisplayMonitors` 回调里取到的整数）。
    pub handle: i64,
    /// 设备名，例如 `\"\\\\\\\\.\\\\DISPLAY1\"`。
    pub device: String,
    /// 物理像素矩形（可含负坐标）。
    pub rect: Rect,
    /// 该显示器的 DPI（96 / 144 ...）。
    pub dpi: u32,
    /// 是否主屏。
    pub primary: bool,
}

impl MonitorInfo {
    /// 与 Python 位置参数同序的构造器：`MonitorInfo(handle, device, rect, dpi, primary)`。
    pub fn new(
        handle: i64,
        device: impl Into<String>,
        rect: Rect,
        dpi: u32,
        primary: bool,
    ) -> Self {
        Self {
            handle,
            device: device.into(),
            rect,
            dpi,
            primary,
        }
    }

    /// DPI 缩放比（96 DPI = 1.0）。
    pub fn scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }

    /// 设备名最后一段（`\"\\\\\\\\.\\\\DISPLAY1\"` → `DISPLAY1`），空则回退整串。
    pub fn name(&self) -> &str {
        match self.device.rsplit('\\').next() {
            Some(tail) if !tail.is_empty() => tail,
            _ => self.device.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// EdgeSegment
// ---------------------------------------------------------------------------

/// 虚拟桌面外轮廓上的一段可吸附边。
///
/// `span_start` 含、`span_end` 不含，单位是沿边方向的虚拟桌面坐标：
///
/// * left/right 边：沿 y 轴，坐标是虚拟桌面的 y；
/// * top/bottom 边：沿 x 轴，坐标是虚拟桌面的 x。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "EdgeSegmentRaw")]
pub struct EdgeSegment {
    /// 段所属显示器的 `MonitorInfo.handle`。
    pub monitor_handle: i64,
    /// 段所在的边。
    pub edge: Edge,
    /// 沿边起点（含）。
    pub span_start: i32,
    /// 沿边终点（不含）。
    pub span_end: i32,
}

/// 反序列化中间层：先收原始字段，再走 `try_from` 复刻 `__post_init__` 的校验。
#[derive(Deserialize)]
struct EdgeSegmentRaw {
    monitor_handle: i64,
    edge: Edge,
    span_start: i32,
    span_end: i32,
}

impl EdgeSegment {
    /// 构造并校验 `span_end >= span_start`
    /// （Python `__post_init__` 抛 `ValueError("EdgeSegment span_end(..) < span_start(..)")`）。
    ///
    /// 字段是公开的（对齐 Python dataclass 的读取语义），构造时请走这里或
    /// `validate()`；直接写字面量会绕过校验。
    pub fn new(
        monitor_handle: i64,
        edge: Edge,
        span_start: i32,
        span_end: i32,
    ) -> Result<Self, GeometryError> {
        let segment = Self {
            monitor_handle,
            edge,
            span_start,
            span_end,
        };
        segment.validate()?;
        Ok(segment)
    }

    /// 复刻 Python `__post_init__` 的校验；合法返回 `Ok(())`。
    pub fn validate(&self) -> Result<(), GeometryError> {
        if self.span_end < self.span_start {
            return Err(GeometryError::InvertedSpan {
                span_start: self.span_start,
                span_end: self.span_end,
            });
        }
        Ok(())
    }

    /// 段长（`span_end - span_start`，最小 0）。
    pub fn length(&self) -> i32 {
        (self.span_end - self.span_start).max(0)
    }

    /// 沿边坐标是否落在 `[span_start, span_end)` 内。
    pub fn contains_along(&self, along: i32) -> bool {
        self.span_start <= along && along < self.span_end
    }
}

impl TryFrom<EdgeSegmentRaw> for EdgeSegment {
    type Error = GeometryError;

    fn try_from(raw: EdgeSegmentRaw) -> Result<Self, Self::Error> {
        EdgeSegment::new(raw.monitor_handle, raw.edge, raw.span_start, raw.span_end)
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// 平板摆位（6.3.4）：吸附到某个外轮廓段 + 段内相对位置。
///
/// 序列化是**扁平**的 config.yaml 形状（见模块文档），不是嵌套 `segment`。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "PlacementRaw")]
pub struct Placement {
    /// 吸附到的外轮廓段。
    pub segment: EdgeSegment,
    /// 段内相对位置，`0.0 .. 1.0`（0 靠 `span_start`，1 靠 `span_end`）。
    pub offset: f64,
    /// 对齐方式。
    pub align: Align,
    /// 角落优先边；`None` 表示没有偏好（配置里写空串）。
    pub corner_priority: Option<Edge>,
}

/// 反序列化中间层（扁平字段 = config.yaml 的形状）。
#[derive(Deserialize)]
struct PlacementRaw {
    monitor_handle: i64,
    edge: Edge,
    span_start: i32,
    span_end: i32,
    #[serde(default = "default_offset")]
    offset: f64,
    #[serde(default)]
    align: Align,
    #[serde(default, deserialize_with = "deserialize_corner_priority")]
    corner_priority: Option<Edge>,
}

/// `Placement.offset` 的 Python 默认值。
fn default_offset() -> f64 {
    0.5
}

/// `corner_priority` 既接受边名，也接受空串 / null / 缺失（配置里表示“无偏好”）。
fn deserialize_corner_priority<'de, D>(deserializer: D) -> Result<Option<Edge>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(text) => Edge::from_str(text)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

impl Placement {
    /// 构造并校验 `offset ∈ [0.0, 1.0]`
    /// （Python `__post_init__` 抛 `ValueError("Placement.offset out of range: ..")`）。
    pub fn new(
        segment: EdgeSegment,
        offset: f64,
        align: Align,
        corner_priority: Option<Edge>,
    ) -> Result<Self, GeometryError> {
        let placement = Self {
            segment,
            offset,
            align,
            corner_priority,
        };
        placement.validate()?;
        Ok(placement)
    }

    /// 复刻 Python `__post_init__` 的校验（NaN 也会被拒，与 Python 相同）。
    pub fn validate(&self) -> Result<(), GeometryError> {
        if !(0.0..=1.0).contains(&self.offset) {
            return Err(GeometryError::OffsetOutOfRange {
                offset: self.offset,
            });
        }
        Ok(())
    }

    /// `corner_priority` 的配置写法：`None` 写成空串。
    pub fn corner_priority_str(&self) -> &'static str {
        match self.corner_priority {
            Some(edge) => edge.as_str(),
            None => "",
        }
    }
}

impl Serialize for Placement {
    /// 按 config.yaml 的扁平形状输出（字段顺序与 `PlacementCfg` 一致）。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Placement", 7)?;
        state.serialize_field("monitor_handle", &self.segment.monitor_handle)?;
        state.serialize_field("edge", &self.segment.edge)?;
        state.serialize_field("span_start", &self.segment.span_start)?;
        state.serialize_field("span_end", &self.segment.span_end)?;
        state.serialize_field("offset", &self.offset)?;
        state.serialize_field("align", &self.align)?;
        state.serialize_field("corner_priority", &self.corner_priority_str())?;
        state.end()
    }
}

impl TryFrom<PlacementRaw> for Placement {
    type Error = GeometryError;

    fn try_from(raw: PlacementRaw) -> Result<Self, Self::Error> {
        let segment = EdgeSegment::new(raw.monitor_handle, raw.edge, raw.span_start, raw.span_end)?;
        Placement::new(segment, raw.offset, raw.align, raw.corner_priority)
    }
}

// ---------------------------------------------------------------------------
// 摆位几何
// ---------------------------------------------------------------------------

/// 平板矩形 `(left, top, right, bottom)`，单位是虚拟桌面物理像素。
///
/// 对应 Python `tablet_rect()` 的 4 元组；独立成类型是为了避免
/// `(f64, f64, f64, f64)` 顺序写错——画布与 Router 共用同一份几何。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TabletRect {
    /// 左边界。
    pub left: f64,
    /// 上边界。
    pub top: f64,
    /// 右边界。
    pub right: f64,
    /// 下边界。
    pub bottom: f64,
}

impl TabletRect {
    /// 与 Python 元组同序的四元组。
    pub fn as_tuple(&self) -> (f64, f64, f64, f64) {
        (self.left, self.top, self.right, self.bottom)
    }

    /// 平板宽。
    pub fn width(&self) -> f64 {
        self.right - self.left
    }

    /// 平板高。
    pub fn height(&self) -> f64 {
        self.bottom - self.top
    }
}

impl From<(f64, f64, f64, f64)> for TabletRect {
    fn from(value: (f64, f64, f64, f64)) -> Self {
        let (left, top, right, bottom) = value;
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
}

impl From<TabletRect> for (f64, f64, f64, f64) {
    fn from(value: TabletRect) -> Self {
        value.as_tuple()
    }
}

/// Python 内置 `max(a, b)` 的浮点语义：`b > a` 才取 `b`。
///
/// 与 `f64::max` 的关键区别是 NaN：Python 的 `max(nan, 0.0)` 返回
/// `nan`，Rust 的 `f64::max` 会返回 `0.0`。几何函数必须逐位一致。
fn py_max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Python 内置 `min(a, b)` 的浮点语义（NaN 规则同 `py_max`）。
fn py_min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// 把值夹到 `[0.0, 1.0]`（对应 Python `clamp01`）。
pub fn clamp01(value: f64) -> f64 {
    py_min(py_max(value, 0.0), 1.0)
}

/// 等价于 Python `int(float)`：向零截断。
///
/// Python 对 NaN / ±inf 抛 `ValueError` / `OverflowError`；几何调用方只会
/// 传有限坐标，这里退化为 `0`（NaN 一律视为“不命中”）。
fn py_int(value: f64) -> i32 {
    if value.is_finite() {
        value.trunc() as i32
    } else {
        0
    }
}

/// 等价于 Python `int(round(value))`：四舍六入五成双（banker's rounding）。
///
/// Python 的 `round(float)` 是“就近取偶”，与 Rust `f64::round`（远离零）
/// 在 `x.5` 上不同，所以必须用 `round_ties_even`。
fn py_round_int(value: f64) -> i64 {
    if value.is_finite() {
        value.round_ties_even() as i64
    } else {
        0
    }
}

/// 把 i64 中间结果夹回 i32（Python 的 int 无宽度上限，这里按显示器坐标的物理范围收敛）。
fn clamp_i32(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// 平板矩形 `(left, top, right, bottom)`，单位是虚拟桌面物理像素。
///
/// 平板贴在外轮廓段外侧，段上的投影点对准平板中心。贴边一侧的长度是
/// 平板的高（left/right 边）或宽（top/bottom 边）。画布和 Router 共用这
/// 一份几何，否则拖动预览与实际穿越范围会不一致。
pub fn tablet_rect(
    segment: &EdgeSegment,
    monitor: &MonitorInfo,
    offset: f64,
    tablet_w: i32,
    tablet_h: i32,
) -> TabletRect {
    let area = monitor.rect;
    let along = segment.span_start as f64 + clamp01(offset) * segment.length().max(1) as f64;
    if segment.edge.is_vertical() {
        let x = if segment.edge == Edge::Left {
            area.left as i64 - tablet_w as i64
        } else {
            area.right as i64
        };
        TabletRect {
            left: x as f64,
            top: along - tablet_h as f64 / 2.0,
            right: (x + tablet_w as i64) as f64,
            bottom: along + tablet_h as f64 / 2.0,
        }
    } else {
        let y = if segment.edge == Edge::Top {
            area.top as i64 - tablet_h as i64
        } else {
            area.bottom as i64
        };
        TabletRect {
            left: along - tablet_w as f64 / 2.0,
            top: y as f64,
            right: along + tablet_w as f64 / 2.0,
            bottom: (y + tablet_h as i64) as f64,
        }
    }
}

/// 鼠标在 `along` 处压线时，这条外轮廓段是否真的贴着平板。
///
/// 只比较段所在的那条 1px 扫描线：线必须落在平板范围内，且光标沿边坐标
/// 也必须落在平板范围内。这样同一台显示器的同一条边不会因为显示器本身
/// 够长就被整段当成可穿透——平板只贴住其中一小段时，只有那一小段能触发。
pub fn segment_contacts_tablet(
    segment: &EdgeSegment,
    monitor: &MonitorInfo,
    tablet: &TabletRect,
    along: f64,
) -> bool {
    if !segment.contains_along(py_int(along)) {
        return false;
    }
    let area = monitor.rect;
    if segment.edge.is_vertical() {
        let line = if segment.edge == Edge::Left {
            area.left as i64 - 1
        } else {
            area.right as i64
        } as f64;
        tablet.left <= line && line < tablet.right && tablet.top <= along && along < tablet.bottom
    } else {
        let line = if segment.edge == Edge::Top {
            area.top as i64 - 1
        } else {
            area.bottom as i64
        } as f64;
        tablet.top <= line && line < tablet.bottom && tablet.left <= along && along < tablet.right
    }
}

/// PC 压线点 → 平板入口点（1:1 几何）。
///
/// 画布按真实像素摆放平板，因此鼠标从哪一点穿过去，就落在平板对应的那一点。
/// 不再把整段 PC 边归一化到整条平板边；平板跨在两块显示器之间时，这样才
/// 不会发生落点跳变。
pub fn tablet_entry_point(
    segment: &EdgeSegment,
    tablet: &TabletRect,
    pc_x: f64,
    pc_y: f64,
    tablet_w: i32,
    tablet_h: i32,
) -> (f64, f64) {
    let max_x = (tablet_w as i64 - 1).max(0) as f64;
    let max_y = (tablet_h as i64 - 1).max(0) as f64;
    match segment.edge {
        Edge::Left => (max_x, py_min(py_max(pc_y - tablet.top, 0.0), max_y)),
        Edge::Right => (0.0, py_min(py_max(pc_y - tablet.top, 0.0), max_y)),
        Edge::Top => {
            let x = py_min(py_max(pc_x - tablet.left, 0.0), max_x);
            (x, max_y)
        }
        Edge::Bottom => {
            let x = py_min(py_max(pc_x - tablet.left, 0.0), max_x);
            (x, 0.0)
        }
    }
}

/// 平板入口边上的点 → PC 落点（1:1 几何，已夹到显示器与段范围内）。
///
/// `offset_px` 是落点距边线的像素数（Python 由
/// `max(1, ENTRY_MARGIN_PX) + BOUNDARY_INSET_PX` 算好后传进来）；落点永远在
/// 桌面内部，因为 Windows 不允许把光标留在所有显示器之外。
pub fn tablet_exit_point(
    segment: &EdgeSegment,
    monitor: &MonitorInfo,
    tablet: &TabletRect,
    tablet_along: f64,
    offset_px: i32,
) -> (i32, i32) {
    let area = monitor.rect;
    let margin = (offset_px as i64).max(1);
    if segment.edge.is_vertical() {
        let area_top = area.top as i64;
        let area_bottom = area.bottom as i64;
        let mut lo = area_top.max(segment.span_start as i64);
        let mut hi = area_bottom.min(segment.span_end as i64);
        if hi <= lo {
            lo = area_top;
            hi = area_bottom;
        }
        let y = py_round_int(tablet.top + tablet_along);
        let y = y.max(lo).min(lo.max(hi - 1));
        let x = if segment.edge == Edge::Left {
            area.left as i64 + margin - 1
        } else {
            area.right as i64 - margin
        };
        let x = x.max(area.left as i64).min(area.right as i64 - 1);
        (clamp_i32(x), clamp_i32(y))
    } else {
        let area_left = area.left as i64;
        let area_right = area.right as i64;
        let mut lo = area_left.max(segment.span_start as i64);
        let mut hi = area_right.min(segment.span_end as i64);
        if hi <= lo {
            lo = area_left;
            hi = area_right;
        }
        let x = py_round_int(tablet.left + tablet_along);
        let x = x.max(lo).min(lo.max(hi - 1));
        let y = if segment.edge == Edge::Top {
            area.top as i64 + margin - 1
        } else {
            area.bottom as i64 - margin
        };
        let y = y.max(area.top as i64).min(area.bottom as i64 - 1);
        (clamp_i32(x), clamp_i32(y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLET_W: i32 = 3200;
    const TABLET_H: i32 = 2136;

    fn display1() -> MonitorInfo {
        MonitorInfo::new(1, r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), 96, true)
    }

    /// `core/geometry.py` 里 single 布局的四条段（与 pytest test_geometry.py 一致）。
    fn single_segment(edge: Edge) -> EdgeSegment {
        let span_end = if edge.is_vertical() { 1080 } else { 1920 };
        EdgeSegment::new(1, edge, 0, span_end).unwrap()
    }

    // -- 类型契约 --------------------------------------------------------

    #[test]
    fn rect_contract() {
        let rect = Rect::new(0, 0, 1920, 1080);
        assert_eq!(rect.width(), 1920);
        assert_eq!(rect.height(), 1080);
        assert!(!rect.empty());
        assert!(Rect::new(0, 0, 0, 5).empty());
        assert!(Rect::new(0, 0, 5, 0).empty());
        assert!(rect.contains(0, 0));
        assert!(rect.contains(1919, 1079));
        assert!(!rect.contains(1920, 0));
        assert!(!rect.contains(-1, 0));
        assert_eq!(rect.center(), (960.0, 540.0));
        assert_eq!(rect.as_tuple(), (0, 0, 1920, 1080));

        let other = Rect::new(-1920, 0, 0, 1080);
        assert_eq!(other.union(&rect), Rect::new(-1920, 0, 1920, 1080));
        // 并集只看四个分量，不要求相交。
        assert_eq!(
            Rect::new(0, 0, 10, 10).union(&Rect::new(50, 50, 60, 60)),
            Rect::new(0, 0, 60, 60)
        );
        // 序列化字段名与 Python dataclass 一致。
        let yaml = serde_yaml_ng::to_string(&rect).unwrap();
        assert_eq!(
            yaml,
            "left: 0\ntop: 0\nright: 1920\nbottom: 1080\n"
        );
        assert_eq!(serde_yaml_ng::from_str::<Rect>(&yaml).unwrap(), rect);
    }

    #[test]
    fn edge_contract() {
        assert_eq!(Edge::Left.outward_normal(), (-1, 0));
        assert_eq!(Edge::Right.outward_normal(), (1, 0));
        assert_eq!(Edge::Top.outward_normal(), (0, -1));
        assert_eq!(Edge::Bottom.outward_normal(), (0, 1));
        assert!(Edge::Left.is_vertical());
        assert!(Edge::Right.is_vertical());
        assert!(!Edge::Top.is_vertical());
        assert!(!Edge::Bottom.is_vertical());
        assert_eq!(Edge::Left.opposite(), Edge::Right);
        assert_eq!(Edge::Top.opposite(), Edge::Bottom);
        assert_eq!(
            ALL_EDGES.map(|edge| edge.as_str()),
            ["left", "right", "top", "bottom"]
        );
    }

    /// 枚举在 YAML / JSON 里都是 Python StrEnum 的小写串。
    #[test]
    fn edge_and_align_serde_use_python_values() {
        for (edge, text) in [
            (Edge::Left, "left"),
            (Edge::Right, "right"),
            (Edge::Top, "top"),
            (Edge::Bottom, "bottom"),
        ] {
            assert_eq!(serde_json::to_string(&edge).unwrap(), format!("\"{text}\""));
            assert_eq!(serde_yaml_ng::to_string(&edge).unwrap().trim(), text);
            assert_eq!(text.parse::<Edge>(), Ok(edge));
        }
        for (align, text) in [
            (Align::Proportional, "proportional"),
            (Align::Center, "center"),
            (Align::OneToOne, "one_to_one"),
        ] {
            assert_eq!(serde_json::to_string(&align).unwrap(), format!("\"{text}\""));
            assert_eq!(serde_yaml_ng::to_string(&align).unwrap().trim(), text);
            assert_eq!(text.parse::<Align>(), Ok(align));
        }
        assert_eq!(Align::default(), Align::Proportional);
        assert_eq!(
            "middle".parse::<Edge>(),
            Err(GeometryError::UnknownEdge {
                value: "middle".to_string()
            })
        );
        assert_eq!(
            "one-to-one".parse::<Align>(),
            Err(GeometryError::UnknownAlign {
                value: "one-to-one".to_string()
            })
        );
    }

    #[test]
    fn monitor_contract() {
        let monitor = display1();
        assert_eq!(monitor.scale(), 1.0);
        assert_eq!(monitor.name(), "DISPLAY1");
        // pytest test_dual_horizontal_merges_outer_edges: 副屏 dpi=144 -> scale 1.5。
        let secondary = MonitorInfo::new(
            2,
            r"\\.\DISPLAY2",
            Rect::new(1920, 0, 3840, 1080),
            144,
            false,
        );
        assert_eq!(secondary.scale(), 1.5);
        assert!(!secondary.primary);
        // device 末尾带反斜杠时 name() 回退整串（Python: rsplit(...)[-1] or device）。
        let odd = MonitorInfo::new(3, r"\\.\", Rect::new(0, 0, 1, 1), 96, false);
        assert_eq!(odd.name(), r"\\.\");
        let empty = MonitorInfo::new(4, "", Rect::new(0, 0, 1, 1), 96, false);
        assert_eq!(empty.name(), "");
        // 序列化往返：负坐标、设备名里的反斜杠都不能被吃掉。
        let yaml = serde_yaml_ng::to_string(&secondary).unwrap();
        let parsed: MonitorInfo = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, secondary);
    }

    // -- EdgeSegment / Placement ----------------------------------------

    #[test]
    fn edge_segment_rejects_inverted_span() {
        let err = EdgeSegment::new(1, Edge::Left, 10, 5).unwrap_err();
        assert_eq!(
            err,
            GeometryError::InvertedSpan {
                span_start: 10,
                span_end: 5
            }
        );
        assert_eq!(err.to_string(), "EdgeSegment span_end(5) < span_start(10)");
        // 空段（span_end == span_start）合法，长度 0。
        let empty = EdgeSegment::new(1, Edge::Left, 10, 10).unwrap();
        assert_eq!(empty.length(), 0);
        assert!(!empty.contains_along(10));
        // 反序列化走同一份校验。
        assert!(serde_yaml_ng::from_str::<EdgeSegment>(
            "monitor_handle: 1\nedge: left\nspan_start: 10\nspan_end: 5\n"
        )
        .is_err());
    }

    #[test]
    fn edge_segment_length_and_contains() {
        let segment = EdgeSegment::new(1, Edge::Right, 0, 1080).unwrap();
        assert_eq!(segment.length(), 1080);
        assert!(segment.contains_along(0));
        assert!(segment.contains_along(1079));
        assert!(!segment.contains_along(1080));
        assert!(!segment.contains_along(-1));
        // 合并后的虚拟段可以很长（dual_horizontal 的 TOP 段 0..3840）。
        let merged = EdgeSegment::new(1, Edge::Top, 0, 3840).unwrap();
        assert_eq!(merged.length(), 3840);
        let yaml = serde_yaml_ng::to_string(&merged).unwrap();
        assert_eq!(
            serde_yaml_ng::from_str::<EdgeSegment>(&yaml).unwrap(),
            merged
        );
    }

    #[test]
    fn placement_rejects_out_of_range_offset() {
        let segment = single_segment(Edge::Right);
        assert_eq!(
            Placement::new(segment, 1.5, Align::Proportional, None).unwrap_err(),
            GeometryError::OffsetOutOfRange { offset: 1.5 }
        );
        assert_eq!(
            GeometryError::OffsetOutOfRange { offset: 1.5 }.to_string(),
            "Placement.offset out of range: 1.5"
        );
        // 整数值的浮点也要带 ".0"（Python f-string 的 str(2.0) == "2.0"）。
        assert_eq!(
            GeometryError::OffsetOutOfRange { offset: 2.0 }.to_string(),
            "Placement.offset out of range: 2.0"
        );
        assert_eq!(
            Placement::new(segment, -0.1, Align::Proportional, None).unwrap_err(),
            GeometryError::OffsetOutOfRange { offset: -0.1 }
        );
        // 边界值 0.0 / 1.0 合法；-0.0 也合法（Python 同样接受）。
        for offset in [0.0, 1.0, -0.0] {
            assert!(Placement::new(segment, offset, Align::Proportional, None).is_ok());
        }
        // NaN 会被拒（Python: not (0.0 <= nan <= 1.0) == True）。
        assert!(Placement::new(segment, f64::NAN, Align::Proportional, None).is_err());
    }

    /// YAML 形状必须能直接写进 config.yaml 的 placement 段。
    #[test]
    fn placement_serde_matches_config_shape() {
        let placement = Placement::new(
            single_segment(Edge::Right),
            0.5,
            Align::Proportional,
            Some(Edge::Right),
        )
        .unwrap();
        let yaml = serde_yaml_ng::to_string(&placement).unwrap();
        assert_eq!(
            yaml,
            concat!(
                "monitor_handle: 1\n",
                "edge: right\n",
                "span_start: 0\n",
                "span_end: 1080\n",
                "offset: 0.5\n",
                "align: proportional\n",
                "corner_priority: right\n",
            )
        );

        // 没有角落偏好时写空串（Python PlacementCfg.corner_priority = ""）。
        let no_corner =
            Placement::new(single_segment(Edge::Top), 0.25, Align::Center, None).unwrap();
        let yaml = serde_yaml_ng::to_string(&no_corner).unwrap();
        assert!(yaml.ends_with("corner_priority: ''\n"), "{yaml}");
        assert_eq!(
            serde_yaml_ng::from_str::<Placement>(&yaml).unwrap(),
            no_corner
        );
    }

    #[test]
    fn placement_deserializes_config_yaml() {
        let yaml = concat!(
            "monitor_handle: 3\n",
            "edge: left\n",
            "span_start: 100\n",
            "span_end: 200\n",
            "offset: 0.75\n",
            "align: one_to_one\n",
            "corner_priority: bottom\n",
        );
        let placement: Placement = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(placement.segment.monitor_handle, 3);
        assert_eq!(placement.segment.edge, Edge::Left);
        assert_eq!(placement.segment.span_start, 100);
        assert_eq!(placement.segment.span_end, 200);
        assert_eq!(placement.offset, 0.75);
        assert_eq!(placement.align, Align::OneToOne);
        assert_eq!(placement.corner_priority, Some(Edge::Bottom));

        // 缺省字段回退 Python dataclass 默认值（offset=0.5 / proportional / None）。
        let defaults: Placement =
            serde_yaml_ng::from_str("monitor_handle: 1\nedge: top\nspan_start: 0\nspan_end: 10\n")
                .unwrap();
        assert_eq!(defaults.offset, 0.5);
        assert_eq!(defaults.align, Align::Proportional);
        assert_eq!(defaults.corner_priority, None);

        // 空串 / null 都表示“无偏好”。
        for text in ["corner_priority: ''", "corner_priority: null"] {
            let yaml =
                format!("monitor_handle: 1\nedge: top\nspan_start: 0\nspan_end: 10\n{text}\n");
            let parsed: Placement = serde_yaml_ng::from_str(&yaml).unwrap();
            assert_eq!(parsed.corner_priority, None);
        }

        // 非法值在反序列化时就拒绝，而不是留下坏对象。
        for bad in [
            "monitor_handle: 1\nedge: middle\nspan_start: 0\nspan_end: 10\n",
            "monitor_handle: 1\nedge: left\nspan_start: 10\nspan_end: 5\n",
            "monitor_handle: 1\nedge: left\nspan_start: 0\nspan_end: 10\noffset: 1.5\n",
        ] {
            assert!(serde_yaml_ng::from_str::<Placement>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn clamp01_matches_python() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(0.0), 0.0);
        assert_eq!(clamp01(0.5), 0.5);
        assert_eq!(clamp01(1.0), 1.0);
        assert_eq!(clamp01(1.5), 1.0);
        // Python 的 min/max 会让 NaN 一路传播。
        assert!(clamp01(f64::NAN).is_nan());
    }

    // -- 摆位几何：tablet_rect -------------------------------------------

    #[test]
    fn tablet_rect_offsets_against_each_edge() {
        let monitor = display1();
        for (edge, expected) in [
            (Edge::Left, (-3200.0, -528.0, 0.0, 1608.0)),
            (Edge::Right, (1920.0, -528.0, 5120.0, 1608.0)),
            (Edge::Top, (-640.0, -2136.0, 2560.0, 0.0)),
            (Edge::Bottom, (-640.0, 1080.0, 2560.0, 3216.0)),
        ] {
            let rect = tablet_rect(&single_segment(edge), &monitor, 0.5, TABLET_W, TABLET_H);
            assert_eq!(rect.as_tuple(), expected, "{edge}");
        }
        // 平板贴边一侧的长度 = 平板高（垂直边）或宽（水平边）。
        assert_eq!(
            tablet_rect(&single_segment(Edge::Right), &monitor, 0.5, TABLET_W, TABLET_H).width(),
            TABLET_W as f64
        );
        assert_eq!(
            tablet_rect(&single_segment(Edge::Top), &monitor, 0.5, TABLET_W, TABLET_H).height(),
            TABLET_H as f64
        );
    }

    #[test]
    fn tablet_rect_clamps_offset() {
        let monitor = display1();
        let segment = single_segment(Edge::Right);
        // offset 越界时 clamp01 把投影点压到段端点（Python 同样处理）。
        let low = tablet_rect(&segment, &monitor, -1.0, TABLET_W, TABLET_H);
        let high = tablet_rect(&segment, &monitor, 2.0, TABLET_W, TABLET_H);
        assert_eq!(low.top, -TABLET_H as f64 / 2.0);
        assert_eq!(low.bottom, TABLET_H as f64 / 2.0);
        assert_eq!(high.top, 1080.0 - TABLET_H as f64 / 2.0);
        assert_eq!(high.bottom, 1080.0 + TABLET_H as f64 / 2.0);
        // 空段按 1px 处理（Python: max(segment.length, 1)）。
        let empty = EdgeSegment::new(1, Edge::Top, 0, 0).unwrap();
        let rect = tablet_rect(&empty, &monitor, 0.0, TABLET_W, TABLET_H);
        assert_eq!(rect.left, -TABLET_W as f64 / 2.0);
    }

    // -- 摆位几何：segment_contacts_tablet -------------------------------

    #[test]
    fn segment_contacts_tablet_checks_scan_line() {
        let monitor = display1();
        for edge in ALL_EDGES {
            let segment = single_segment(edge);
            let tablet = tablet_rect(&segment, &monitor, 0.5, TABLET_W, TABLET_H);
            assert!(
                segment_contacts_tablet(&segment, &monitor, &tablet, 540.0),
                "{edge}"
            );
            // 沿边坐标落在 span 之外 -> 不接触（contains_along 先拦）。
            assert!(!segment_contacts_tablet(&segment, &monitor, &tablet, -1.0));
        }

        // 平板只贴住边的一小段：光标压在 span 内但不在平板覆盖范围内 -> 不接触。
        let right = single_segment(Edge::Right);
        let bottom_aligned = tablet_rect(&right, &monitor, 1.0, TABLET_W, TABLET_H);
        assert!(right.contains_along(5));
        assert!(!segment_contacts_tablet(
            &right,
            &monitor,
            &bottom_aligned,
            5.0
        ));
        assert!(segment_contacts_tablet(
            &right,
            &monitor,
            &bottom_aligned,
            540.0
        ));

        // 段属于另一台显示器：1px 扫描线不在平板范围内 -> 不接触。
        let other = MonitorInfo::new(
            2,
            r"\\.\DISPLAY2",
            Rect::new(0, 1080, 1920, 2160),
            96,
            false,
        );
        let other_segment = EdgeSegment::new(2, Edge::Left, 1000, 2000).unwrap();
        assert!(other_segment.contains_along(1500));
        assert!(!segment_contacts_tablet(
            &other_segment,
            &other,
            &bottom_aligned,
            1500.0
        ));
    }

    // -- 摆位几何：入口 / 出口 -------------------------------------------

    /// pytest test_entry_and_exit_single_screen 的几何口径（tablet_rect + 1:1 映射）。
    #[test]
    fn tablet_entry_point_maps_to_tablet_edge() {
        let monitor = display1();
        for (edge, want) in [
            (Edge::Right, (0.0, 1068.0)),
            (Edge::Left, (3199.0, 1068.0)),
            (Edge::Top, (1600.0, 2135.0)),
            (Edge::Bottom, (1600.0, 0.0)),
        ] {
            let segment = single_segment(edge);
            let tablet = tablet_rect(&segment, &monitor, 0.5, TABLET_W, TABLET_H);
            let got = tablet_entry_point(&segment, &tablet, 960.0, 540.0, TABLET_W, TABLET_H);
            assert_eq!(got, want, "{edge}");
        }

        // 合并段（dual_horizontal 的 TOP 0..3840）：落点按真实像素偏移，不归一化。
        let merged = EdgeSegment::new(1, Edge::Top, 0, 3840).unwrap();
        let tablet = tablet_rect(&merged, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(tablet.as_tuple(), (320.0, -2136.0, 3520.0, 0.0));
        assert_eq!(
            tablet_entry_point(&merged, &tablet, 960.0, 540.0, TABLET_W, TABLET_H),
            (640.0, 2135.0)
        );

        // 平板贴左边时入口在平板最右侧像素（W-1）。
        let left = single_segment(Edge::Left);
        let tablet = tablet_rect(&left, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_entry_point(&left, &tablet, 0.0, 540.0, TABLET_W, TABLET_H),
            (3199.0, 1068.0)
        );
        // 沿边方向超出平板高度时夹到边界。
        assert_eq!(
            tablet_entry_point(&left, &tablet, 0.0, 100_000.0, TABLET_W, TABLET_H),
            (3199.0, 2135.0)
        );
        assert_eq!(
            tablet_entry_point(&left, &tablet, 0.0, -100_000.0, TABLET_W, TABLET_H),
            (3199.0, 0.0)
        );
    }

    /// pytest test_entry_and_exit_single_screen / test_exit_position_* 的落点。
    #[test]
    fn tablet_exit_point_clamps_to_monitor_and_segment() {
        let monitor = display1();
        let cases = [
            (Edge::Right, 1067.5, 2, (1918, 540)),
            (Edge::Right, -1.0, 2, (1918, 0)),
            (Edge::Right, 10_000.0, 2, (1918, 1079)),
            (Edge::Left, 1067.5, 2, (1, 540)),
            (Edge::Left, -1.0, 2, (1, 0)),
            (Edge::Left, 10_000.0, 2, (1, 1079)),
            (Edge::Top, 1067.5, 2, (428, 1)),
            (Edge::Top, -1.0, 2, (0, 1)),
            (Edge::Top, 10_000.0, 2, (1919, 1)),
            (Edge::Bottom, 1067.5, 2, (428, 1078)),
            (Edge::Bottom, -1.0, 2, (0, 1078)),
            (Edge::Bottom, 10_000.0, 2, (1919, 1078)),
        ];
        for (edge, tablet_along, offset_px, want) in cases {
            let segment = single_segment(edge);
            let tablet = tablet_rect(&segment, &monitor, 0.5, TABLET_W, TABLET_H);
            let got = tablet_exit_point(&segment, &monitor, &tablet, tablet_along, offset_px);
            assert_eq!(got, want, "{edge} along={tablet_along}");
        }
    }

    #[test]
    fn tablet_exit_point_accepts_custom_margin() {
        let monitor = display1();
        // margin = max(1, offset_px)：0 / 负数都退化成 1
        // （pytest test_exit_position_margin_and_inset_overrides）。
        for offset_px in [0, -5] {
            let right = single_segment(Edge::Right);
            let tablet = tablet_rect(&right, &monitor, 0.5, TABLET_W, TABLET_H);
            assert_eq!(
                tablet_exit_point(&right, &monitor, &tablet, 1067.5, offset_px),
                (1919, 540)
            );
            assert_eq!(
                tablet_exit_point(&right, &monitor, &tablet, 100.0, offset_px),
                (1919, 0)
            );
            let left = single_segment(Edge::Left);
            let tablet = tablet_rect(&left, &monitor, 0.5, TABLET_W, TABLET_H);
            assert_eq!(
                tablet_exit_point(&left, &monitor, &tablet, 1067.5, offset_px),
                (0, 540)
            );
        }
        // 显式 margin=2 与默认值一致。
        let right = single_segment(Edge::Right);
        let tablet = tablet_rect(&right, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&right, &monitor, &tablet, 1067.5, 2),
            (1918, 540)
        );
    }

    #[test]
    fn tablet_exit_point_on_merged_segment() {
        let monitor = display1();
        let merged = EdgeSegment::new(1, Edge::Top, 0, 3840).unwrap();
        let tablet = tablet_rect(&merged, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&merged, &monitor, &tablet, 799.75, 2),
            (1120, 1)
        );
        assert_eq!(
            tablet_exit_point(&merged, &monitor, &tablet, 3199.0, 2),
            (1919, 1)
        );
        assert_eq!(
            tablet_exit_point(&merged, &monitor, &tablet, 0.0, 2),
            (320, 1)
        );
    }

    #[test]
    fn tablet_exit_point_negative_coordinates() {
        let monitor = MonitorInfo::new(
            2,
            r"\\.\DISPLAY2",
            Rect::new(-1920, 0, 0, 1080),
            96,
            false,
        );
        let left = EdgeSegment::new(2, Edge::Left, 0, 1080).unwrap();
        let tablet = tablet_rect(&left, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(tablet.as_tuple(), (-5120.0, -528.0, -1920.0, 1608.0));
        assert_eq!(
            tablet_exit_point(&left, &monitor, &tablet, 1067.5, 2),
            (-1919, 540)
        );
        // 右侧段必须落在负坐标显示器内部（right - margin = -2）。
        let right = EdgeSegment::new(2, Edge::Right, 0, 1080).unwrap();
        let tablet = tablet_rect(&right, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&right, &monitor, &tablet, 1067.5, 2),
            (-2, 540)
        );
    }

    /// Python `int(round(x))` 是“就近取偶”，不是 Rust `f64::round` 的远离零。
    #[test]
    fn tablet_exit_point_rounds_half_to_even() {
        let monitor = display1();
        let right = single_segment(Edge::Right);
        let tablet = tablet_rect(&right, &monitor, 0.5, TABLET_W, TABLET_H);
        // tablet.top = -528；-528 + 1068.5 = 540.5 -> 540（偶），不是 541。
        assert_eq!(
            tablet_exit_point(&right, &monitor, &tablet, 1068.5, 2),
            (1918, 540)
        );
        // -528 + 1069.5 = 541.5 -> 542（偶）。
        assert_eq!(
            tablet_exit_point(&right, &monitor, &tablet, 1069.5, 2),
            (1918, 542)
        );
        // 水平边同理：tablet.left = -640；-640 + 1272.5 = 632.5 -> 632。
        let top = single_segment(Edge::Top);
        let tablet = tablet_rect(&top, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&top, &monitor, &tablet, 1272.5, 2),
            (632, 1)
        );
        // 段被夹到 [span_start, span_end-1]：越界落点不会跑出段。
        let short = EdgeSegment::new(1, Edge::Top, 100, 200).unwrap();
        let tablet = tablet_rect(&short, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&short, &monitor, &tablet, 10_000.0, 2),
            (199, 1)
        );
        assert_eq!(
            tablet_exit_point(&short, &monitor, &tablet, -10_000.0, 2),
            (100, 1)
        );
    }

    /// 段与显示器不重叠时退回整条显示器边
    /// （Python: if hi <= lo: lo, hi = area.top, area.bottom）。
    #[test]
    fn tablet_exit_point_falls_back_when_segment_outside_monitor() {
        let monitor = display1();
        let segment = EdgeSegment::new(1, Edge::Top, 5000, 6000).unwrap();
        let tablet = tablet_rect(&segment, &monitor, 0.5, TABLET_W, TABLET_H);
        assert_eq!(
            tablet_exit_point(&segment, &monitor, &tablet, -10_000.0, 2),
            (0, 1)
        );
    }
}

