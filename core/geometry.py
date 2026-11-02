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

    def contains_rect(self, other: Rect) -> bool:
        return (
            self.left <= other.left
            and self.top <= other.top
            and other.right <= self.right
            and other.bottom <= self.bottom
        )

    def intersects(self, other: Rect) -> bool:
        return not (
            other.left >= self.right
            or other.right <= self.left
            or other.top >= self.bottom
            or other.bottom <= self.top
        )

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
