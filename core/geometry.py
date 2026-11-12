"""纯几何数据类型（设计文档 6.3 / 8.1）。

设计文档把这些类型放在 ``win/desktop.py``；为了让 ``core`` / ``tablet`` 层在不
import win32 平台代码的前提下使用它们，这里给出唯一的定义处，``win/desktop.py``
按契约重新导出（``from win.desktop import Edge`` 等写法保持有效）。

本模块只允许 import 标准库。
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum


# ---------------------------------------------------------------------------
# Rect
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class Rect:
    """虚拟桌面矩形，物理像素，``right`` / ``bottom`` 为开区间。"""

    left: int
    top: int
    right: int
    bottom: int

    @property
    def width(self) -> int:
        return self.right - self.left

    @property
    def height(self) -> int:
        return self.bottom - self.top

    @property
    def empty(self) -> bool:
        return self.width <= 0 or self.height <= 0

    def contains(self, x: int, y: int) -> bool:
        return self.left <= x < self.right and self.top <= y < self.bottom

    def union(self, other: Rect) -> Rect:
        return Rect(
            min(self.left, other.left),
            min(self.top, other.top),
            max(self.right, other.right),
            max(self.bottom, other.bottom),
        )

    @property
    def center(self) -> tuple[float, float]:
        return (self.left + self.width / 2.0, self.top + self.height / 2.0)

    def as_tuple(self) -> tuple[int, int, int, int]:
        return (self.left, self.top, self.right, self.bottom)


# ---------------------------------------------------------------------------
# Edge / Align
# ---------------------------------------------------------------------------
class Edge(StrEnum):
    LEFT = "left"
    RIGHT = "right"
    TOP = "top"
    BOTTOM = "bottom"

    def outward_normal(self) -> tuple[int, int]:
        """指向屏幕外侧的单位向量（6.3.6 角落裁决用）。"""
        if self is Edge.LEFT:
            return (-1, 0)
        if self is Edge.RIGHT:
            return (1, 0)
        if self is Edge.TOP:
            return (0, -1)
        return (0, 1)

    @property
    def is_vertical(self) -> bool:
        """left/right 的 span 沿 y 轴；top/bottom 沿 x 轴。"""
        return self in (Edge.LEFT, Edge.RIGHT)

    @property
    def opposite(self) -> Edge:
        return {
            Edge.LEFT: Edge.RIGHT,
            Edge.RIGHT: Edge.LEFT,
            Edge.TOP: Edge.BOTTOM,
            Edge.BOTTOM: Edge.TOP,
        }[self]


class Align(StrEnum):
    PROPORTIONAL = "proportional"
    CENTER = "center"
    ONE_TO_ONE = "one_to_one"


# ---------------------------------------------------------------------------
# MonitorInfo
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class MonitorInfo:
    """一台显示器。``rect`` 为物理像素，副屏在主屏左侧时可为负坐标（13.3）。"""

    handle: int
    device: str  # "\\\\.\\DISPLAY1"
    rect: Rect
    dpi: int  # 96 / 144 ...
    primary: bool

    @property
    def scale(self) -> float:
        return self.dpi / 96.0

    @property
    def name(self) -> str:
        return self.device.rsplit("\\", 1)[-1] or self.device


# ---------------------------------------------------------------------------
# EdgeSegment
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class EdgeSegment:
    """虚拟桌面外轮廓上的一段可吸附边。

    ``span_start`` 含、``span_end`` 不含，单位是沿边方向的虚拟桌面坐标：
    - left/right 边：沿 y 轴，坐标是虚拟桌面的 y；
    - top/bottom 边：沿 x 轴，坐标是虚拟桌面的 x。
    """

    monitor_handle: int
    edge: Edge
    span_start: int
    span_end: int

    @property
    def length(self) -> int:
        return max(0, self.span_end - self.span_start)

    def contains_along(self, along: int) -> bool:
        return self.span_start <= along < self.span_end

    def __post_init__(self) -> None:
        if self.span_end < self.span_start:
            raise ValueError(
                f"EdgeSegment span_end({self.span_end}) < span_start({self.span_start})"
            )


# ---------------------------------------------------------------------------
# Placement
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class Placement:
    """平板摆位（6.3.4）：吸附到某个外轮廓段 + 段内相对位置。"""

    segment: EdgeSegment
    offset: float = 0.5
    align: Align = Align.PROPORTIONAL
    corner_priority: Edge | None = None

    def __post_init__(self) -> None:
        if not (0.0 <= self.offset <= 1.0):
            raise ValueError(f"Placement.offset out of range: {self.offset}")


# ---------------------------------------------------------------------------
# 摆位几何
# ---------------------------------------------------------------------------
def clamp01(value: float) -> float:
    return min(max(float(value), 0.0), 1.0)


def tablet_rect(
    segment: EdgeSegment,
    monitor: MonitorInfo,
    offset: float,
    tablet_w: int,
    tablet_h: int,
) -> tuple[float, float, float, float]:
    """平板矩形 (left, top, right, bottom)，单位是虚拟桌面物理像素。

    平板贴在外轮廓段外侧，段上的投影点对准平板中心。贴边一侧的长度是
    平板的高（left/right 边）或宽（top/bottom 边）。画布和 Router 共用这
    一份几何，否则拖动预览与实际穿越范围会不一致。
    """
    area = monitor.rect
    along = segment.span_start + clamp01(offset) * max(segment.length, 1)
    if segment.edge.is_vertical:
        x = area.left - tablet_w if segment.edge is Edge.LEFT else area.right
        return (
            float(x),
            along - tablet_h / 2.0,
            float(x + tablet_w),
            along + tablet_h / 2.0,
        )
    y = area.top - tablet_h if segment.edge is Edge.TOP else area.bottom
    return (
        along - tablet_w / 2.0,
        float(y),
        along + tablet_w / 2.0,
        float(y + tablet_h),
    )


def segment_contacts_tablet(
    segment: EdgeSegment,
    monitor: MonitorInfo,
    tablet: tuple[float, float, float, float],
    along: float,
) -> bool:
    """鼠标在 along 处压线时，这条外轮廓段是否真的贴着平板。

    只比较段所在的那条 1px 扫描线：线必须落在平板范围内，且光标沿边坐标
    也必须落在平板范围内。这样同一台显示器的同一条边不会因为显示器本身
    够长就被整段当成可穿透——平板只贴住其中一小段时，只有那一小段能触发。
    """
    if not segment.contains_along(int(along)):
        return False
    left, top, right, bottom = tablet
    area = monitor.rect
    if segment.edge.is_vertical:
        line = area.left - 1 if segment.edge is Edge.LEFT else area.right
        return left <= line < right and top <= along < bottom
    line = area.top - 1 if segment.edge is Edge.TOP else area.bottom
    return top <= line < bottom and left <= along < right


def tablet_entry_point(
    segment: EdgeSegment,
    tablet: tuple[float, float, float, float],
    pc_x: float,
    pc_y: float,
    tablet_w: int,
    tablet_h: int,
) -> tuple[float, float]:
    """PC 压线点 → 平板入口点（1:1 几何）。

    画布按真实像素摆放平板，因此鼠标从哪一点穿过去，就落在平板对应的那一点。
    不再把整段 PC 边归一化到整条平板边；平板跨在两块显示器之间时，这样才
    不会发生落点跳变。
    """
    left, top, _right, _bottom = tablet
    max_x = float(max(tablet_w - 1, 0))
    max_y = float(max(tablet_h - 1, 0))
    if segment.edge is Edge.LEFT:
        return (max_x, min(max(pc_y - top, 0.0), max_y))
    if segment.edge is Edge.RIGHT:
        return (0.0, min(max(pc_y - top, 0.0), max_y))
    x = min(max(pc_x - left, 0.0), max_x)
    if segment.edge is Edge.TOP:
        return (x, max_y)
    return (x, 0.0)


def tablet_exit_point(
    segment: EdgeSegment,
    monitor: MonitorInfo,
    tablet: tuple[float, float, float, float],
    tablet_along: float,
    offset_px: int,
) -> tuple[int, int]:
    """平板入口边上的点 → PC 落点（1:1 几何，已夹到显示器与段范围内）。"""
    area = monitor.rect
    left, top, _right, _bottom = tablet
    margin = max(1, int(offset_px))
    if segment.edge.is_vertical:
        lo = max(area.top, segment.span_start)
        hi = min(area.bottom, segment.span_end)
        if hi <= lo:
            lo, hi = area.top, area.bottom
        y = int(round(top + tablet_along))
        y = min(max(y, lo), max(lo, hi - 1))
        x = area.left + margin - 1 if segment.edge is Edge.LEFT else area.right - margin
        x = min(max(x, area.left), area.right - 1)
        return (int(x), int(y))
    lo = max(area.left, segment.span_start)
    hi = min(area.right, segment.span_end)
    if hi <= lo:
        lo, hi = area.left, area.right
    x = int(round(left + tablet_along))
    x = min(max(x, lo), max(lo, hi - 1))
    y = area.top + margin - 1 if segment.edge is Edge.TOP else area.bottom - margin
    y = min(max(y, area.top), area.bottom - 1)
    return (int(x), int(y))
