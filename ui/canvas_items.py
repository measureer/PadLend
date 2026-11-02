"""屏幕布局画布的图元（设计文档 6.10.2）。

本模块只负责"画"和"把鼠标事件转交给画布"：吸附、角落裁决、缩放等交互逻辑在
:mod:`ui.canvas`。所有图元使用**虚拟桌面物理像素**作为场景坐标（6.3.1），
因此 1 场景单位 = 1 物理像素，缩放由视图变换负责。

颜色优先取自 ``ui.theme.tokens``（由 ui-shell 维护）；该模块尚未存在时使用本地
兜底常量，绝不修改它。
"""

from __future__ import annotations

import importlib
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QLineF, QPointF, QRectF, Qt
from PySide6.QtGui import QBrush, QColor, QFont, QPainter, QPen
from PySide6.QtWidgets import QGraphicsItem, QMenu

from core.geometry import Edge, EdgeSegment, MonitorInfo

if TYPE_CHECKING:
    from ui.canvas import DesktopCanvas

# ---------------------------------------------------------------------------
# 颜色：ui.theme.tokens 优先，缺失时兜底
# ---------------------------------------------------------------------------
_FALLBACK_COLORS: dict[str, str] = {
    "bg_window": "#1F1F1F",
    "bg_surface": "#272727",
    "bg_raised": "#323232",
    "border": "#3A3A3A",
    "text_primary": "#E8E8E8",
    "text_secondary": "#9A9A9A",
    "accent": "#4CC2FF",
    "ok": "#4ADE80",
    "warn": "#FBBF24",
    "err": "#F87171",
    "capturing": "#FF5C5C",
}


def _resolve_colors() -> dict[str, str]:
    """读取 ui.theme.tokens；模块或字段缺失时逐项回退到本地常量。"""
    colors = dict(_FALLBACK_COLORS)
    try:
        module = importlib.import_module("ui.theme.tokens")
    except ImportError:
        return colors
    table: Any = getattr(module, "TOKENS", None)
    for key in colors:
        value: Any = None
        if isinstance(table, dict):
            value = table.get(key)
        elif table is not None:
            value = getattr(table, key, None)
        if value is None:
            value = getattr(module, key, None)
        if isinstance(value, str) and value:
            colors[key] = value
    return colors


COLORS: dict[str, str] = _resolve_colors()


def color(name: str) -> QColor:
    """按令牌名取颜色；未知名字回退到窗口底色。"""
    return QColor(COLORS.get(name, _FALLBACK_COLORS["bg_window"]))


def _scale_of(painter: QPainter) -> float:
    scale = painter.transform().m11()
    return scale if scale > 0.0 else 1.0


def _device_px(painter: QPainter, value: float) -> float:
    """把"屏幕像素"换算成当前缩放下的场景单位（文字/线宽保持视觉恒定）。"""
    return value / _scale_of(painter)


def _font_for(painter: QPainter, screen_px: float, *, bold: bool = False) -> QFont:
    size = _device_px(painter, screen_px)
    font = QFont(painter.font())
    font.setPixelSize(max(2, min(400, int(round(size)))))
    font.setBold(bold)
    return font


# ---------------------------------------------------------------------------
# 段几何：全部以虚拟桌面物理像素表示
# ---------------------------------------------------------------------------
def clamp01(value: float) -> float:
    if value < 0.0:
        return 0.0
    if value > 1.0:
        return 1.0
    return value


def monitor_for_segment(
    monitors: tuple[MonitorInfo, ...] | list[MonitorInfo], segment: EdgeSegment
) -> MonitorInfo | None:
    for monitor in monitors:
        if monitor.handle == segment.monitor_handle:
            return monitor
    return None


def segment_line(segment: EdgeSegment, monitor: MonitorInfo) -> QLineF:
    """段在场景中的直线（含端点）。"""
    rect = monitor.rect
    if segment.edge is Edge.LEFT:
        return QLineF(rect.left, segment.span_start, rect.left, segment.span_end)
    if segment.edge is Edge.RIGHT:
        return QLineF(rect.right, segment.span_start, rect.right, segment.span_end)
    if segment.edge is Edge.TOP:
        return QLineF(segment.span_start, rect.top, segment.span_end, rect.top)
    return QLineF(segment.span_start, rect.bottom, segment.span_end, rect.bottom)


def segment_along(segment: EdgeSegment, point: QPointF) -> float:
    """点在该段沿边方向上的坐标。"""
    return point.y() if segment.edge.is_vertical else point.x()


def segment_offset(segment: EdgeSegment, point: QPointF) -> float:
    """点投影到段上的相对位置 [0,1]（Placement.offset 的定义）。"""
    length = max(segment.length, 1)
    return clamp01((segment_along(segment, point) - segment.span_start) / length)


def point_segment_distance(segment: EdgeSegment, monitor: MonitorInfo, point: QPointF) -> float:
    """点到段（线段，不是无限长直线）的距离。"""
    line = segment_line(segment, monitor)
    if segment.edge.is_vertical:
        y = min(max(point.y(), line.y1()), line.y2())
        dx = point.x() - line.x1()
        dy = point.y() - y
    else:
        x = min(max(point.x(), line.x1()), line.x2())
        dx = point.x() - x
        dy = point.y() - line.y1()
    return float((dx * dx + dy * dy) ** 0.5)


def tablet_rect_for(
    segment: EdgeSegment,
    monitor: MonitorInfo,
    offset: float,
    tablet_w: int,
    tablet_h: int,
) -> QRectF:
    """平板矩形：贴在外轮廓段外侧，段上投影点对准平板中心。

    平板始终按设备方向绘制（横屏 3200×2136 / 竖屏 2136×3200）：
    贴边一侧的长度是平板的高（left/right 边）或宽（top/bottom 边）。
    """
    rect = monitor.rect
    along = segment.span_start + clamp01(offset) * max(segment.length, 1)
    if segment.edge.is_vertical:
        x = rect.left - tablet_w if segment.edge is Edge.LEFT else rect.right
        return QRectF(x, along - tablet_h / 2.0, float(tablet_w), float(tablet_h))
    y = rect.top - tablet_h if segment.edge is Edge.TOP else rect.bottom
    return QRectF(along - tablet_w / 2.0, y, float(tablet_w), float(tablet_h))


def resolve_corner_local(
    segments: tuple[EdgeSegment, ...],
    delta: tuple[float, float],
    priority: Edge | None,
) -> EdgeSegment | None:
    """6.3.6 角落裁决的本地实现（注入的 DesktopMap 没有该方法时使用）。"""
    if not segments:
        return None
    if len(segments) == 1:
        return segments[0]
    scored: list[tuple[EdgeSegment, float]] = []
    for segment in segments:
        nx, ny = segment.edge.outward_normal()
        scored.append((segment, delta[0] * nx + delta[1] * ny))
    scored.sort(key=lambda item: -item[1])
    (first, first_score), (_, second_score) = scored[0], scored[1]
    if first_score > 0 and (first_score - second_score) / max(first_score, 1.0) > 0.15:
        return first
    for segment, _ in scored:
        if segment.edge is priority:
            return segment
    return first


def _merge_intervals(intervals: list[tuple[int, int]]) -> list[tuple[int, int]]:
    if not intervals:
        return []
    intervals = sorted(intervals)
    merged = [intervals[0]]
    for start, end in intervals[1:]:
        last_start, last_end = merged[-1]
        if start <= last_end:
            merged[-1] = (last_start, max(last_end, end))
        else:
            merged.append((start, end))
    return merged


def interior_seams(
    monitors: tuple[MonitorInfo, ...] | list[MonitorInfo], monitor: MonitorInfo
) -> list[tuple[Edge, int, int]]:
    """显示器之间不可吸附的内缝（6.3.3 的灰点线）。

    对显示器每条边取"边外侧 1px"的探针线，被其它显示器覆盖的连续区间就是内缝。
    """
    rect = monitor.rect
    seams: list[tuple[Edge, int, int]] = []
    for edge in (Edge.LEFT, Edge.RIGHT, Edge.TOP, Edge.BOTTOM):
        intervals: list[tuple[int, int]] = []
        if edge.is_vertical:
            probe_x = rect.left - 1 if edge is Edge.LEFT else rect.right
            low, high = rect.top, rect.bottom
            for other in monitors:
                if other.handle == monitor.handle:
                    continue
                other_rect = other.rect
                if not (other_rect.left <= probe_x < other_rect.right):
                    continue
                start = max(low, other_rect.top)
                end = min(high, other_rect.bottom)
                if end > start:
                    intervals.append((start, end))
        else:
            probe_y = rect.top - 1 if edge is Edge.TOP else rect.bottom
            low, high = rect.left, rect.right
            for other in monitors:
                if other.handle == monitor.handle:
                    continue
                other_rect = other.rect
                if not (other_rect.top <= probe_y < other_rect.bottom):
                    continue
                start = max(low, other_rect.left)
                end = min(high, other_rect.right)
                if end > start:
                    intervals.append((start, end))
        for start, end in _merge_intervals(intervals):
            seams.append((edge, start, end))
    return seams


# ---------------------------------------------------------------------------
# MonitorItem
# ---------------------------------------------------------------------------
class MonitorItem(QGraphicsItem):
    """一台显示器：真实比例矩形 + 设备名/分辨率/DPI/主屏标记。"""

    PADDING = 40.0

    def __init__(self, info: MonitorInfo, parent: QGraphicsItem | None = None) -> None:
        super().__init__(parent)
        self.info = info
        self._hover = False
        self.setPos(info.rect.left, info.rect.top)
        self.setZValue(0)
        self.setAcceptHoverEvents(True)
        self.setToolTip(self.tooltip_text())

    def tooltip_text(self) -> str:
        info = self.info
        lines = [
            info.name,
            f"{info.rect.width}×{info.rect.height}",
            f"DPI {info.dpi}（缩放 {info.scale * 100:.0f}%）",
        ]
        if info.primary:
            lines.append("主屏")
        return "\n".join(lines)

    def boundingRect(self) -> QRectF:
        return QRectF(
            0.0,
            0.0,
            float(self.info.rect.width),
            float(self.info.rect.height),
        ).adjusted(-self.PADDING, -self.PADDING, self.PADDING, self.PADDING)

    def hoverEnterEvent(self, event: Any) -> None:
        self._hover = True
        self.update()
        super().hoverEnterEvent(event)

    def hoverLeaveEvent(self, event: Any) -> None:
        self._hover = False
        self.update()
        super().hoverLeaveEvent(event)

    def paint(self, painter: QPainter, option: Any, widget: Any = None) -> None:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        rect = QRectF(
            0.0,
            0.0,
            float(self.info.rect.width),
            float(self.info.rect.height),
        )
        radius = min(_device_px(painter, 6.0), rect.width() / 4.0, rect.height() / 4.0)
        fill = color("bg_raised") if self._hover else color("bg_surface")
        border = color("accent") if self._hover else color("border")
        painter.setBrush(QBrush(fill))
        painter.setPen(QPen(border, max(0.8, _device_px(painter, 1.5))))
        painter.drawRoundedRect(rect, radius, radius)

        pad = _device_px(painter, 12.0)
        line_gap = _device_px(painter, 18.0)
        x = rect.left() + pad
        y = rect.top() + pad + _device_px(painter, 13.0)

        painter.setFont(_font_for(painter, 13.0, bold=True))
        painter.setPen(QPen(color("text_primary")))
        painter.drawText(QPointF(x, y), self.info.name)

        y += line_gap
        painter.setFont(_font_for(painter, 11.0))
        painter.setPen(QPen(color("text_secondary")))
        resolution = (
            f"{self.info.rect.width}×{self.info.rect.height}   "
            f"{self.info.scale * 100:.0f}%"
        )
        painter.drawText(QPointF(x, y), resolution)

        y += line_gap
        painter.drawText(QPointF(x, y), f"DPI {self.info.dpi}")

        if self.info.primary:
            y += line_gap
            painter.setPen(QPen(color("accent")))
            painter.drawText(QPointF(x, y), "主屏")


# ---------------------------------------------------------------------------
# EdgeSegmentItem：可吸附外轮廓段 / 内缝
# ---------------------------------------------------------------------------
class EdgeSegmentItem(QGraphicsItem):
    """外轮廓段（红虚线）或内缝（灰点线）。

    ``kind``：``"exterior"`` 可吸附；``"seam"`` 内缝不可吸附。
    ``attached``：当前 Placement 吸附的段；``highlighted``：拖动过程中的候选；
    ``active``：拖动过程中最终选中的段。
    """

    PADDING = 32.0

    def __init__(
        self,
        segment: EdgeSegment,
        monitor: MonitorInfo,
        kind: str = "exterior",
        parent: QGraphicsItem | None = None,
    ) -> None:
        super().__init__(parent)
        self.segment = segment
        self.monitor = monitor
        self.kind = kind
        self.attached = False
        self.highlighted = False
        self.active = False
        line = segment_line(segment, monitor)
        self.setPos(line.p1())
        self._end = QPointF(line.x2() - line.x1(), line.y2() - line.y1())
        self.setZValue(20 if kind == "exterior" else 1)

    def boundingRect(self) -> QRectF:
        return QRectF(QPointF(0.0, 0.0), self._end).normalized().adjusted(
            -self.PADDING, -self.PADDING, self.PADDING, self.PADDING
        )

    def set_attached(self, value: bool) -> None:
        if self.attached != value:
            self.attached = value
            self.update()

    def set_highlighted(self, value: bool) -> None:
        if self.highlighted != value:
            self.highlighted = value
            self.update()

    def set_active(self, value: bool) -> None:
        if self.active != value:
            self.active = value
            self.update()

    def paint(self, painter: QPainter, option: Any, widget: Any = None) -> None:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        start = QPointF(0.0, 0.0)
        end = QPointF(self._end)

        if self.kind == "seam":
            pen = QPen(color("border"), max(0.8, _device_px(painter, 1.4)))
            pen.setStyle(Qt.PenStyle.DotLine)
            painter.setPen(pen)
            painter.drawLine(start, end)
            return

        if self.active:
            glow = QPen(QColor(color("capturing").red(), color("capturing").green(),
                               color("capturing").blue(), 70))
            glow.setWidthF(max(2.0, _device_px(painter, 12.0)))
            glow.setCapStyle(Qt.PenCapStyle.RoundCap)
            painter.setPen(glow)
            painter.drawLine(start, end)
            pen = QPen(color("capturing"), max(1.5, _device_px(painter, 3.0)))
            pen.setCapStyle(Qt.PenCapStyle.RoundCap)
        elif self.highlighted:
            pen = QPen(color("capturing"), max(1.2, _device_px(painter, 2.4)))
            pen.setCapStyle(Qt.PenCapStyle.RoundCap)
        elif self.attached:
            pen = QPen(color("err"), max(1.0, _device_px(painter, 2.0)))
            pen.setStyle(Qt.PenStyle.DashLine)
        else:
            pen = QPen(color("err"), max(0.8, _device_px(painter, 1.4)))
            pen.setStyle(Qt.PenStyle.DashLine)

        painter.setPen(pen)
        painter.drawLine(start, end)


# ---------------------------------------------------------------------------
# TabletItem
# ---------------------------------------------------------------------------
class TabletItem(QGraphicsItem):
    """平板屏幕矩形：真实比例、可拖动、双击切横竖、右键菜单。"""

    PADDING = 64.0

    def __init__(
        self,
        size: tuple[int, int],
        owner: DesktopCanvas | None = None,
        parent: QGraphicsItem | None = None,
    ) -> None:
        super().__init__(parent)
        self._width = int(size[0])
        self._height = int(size[1])
        self._owner = owner
        self._highlighted = False
        self._connected = False
        self._attached_edge: Edge | None = None
        self._pointer: QPointF | None = None
        self.setZValue(10)
        self.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIsMovable, False)
        self.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIsSelectable, False)
        self.setAcceptHoverEvents(True)
        self.setAcceptedMouseButtons(Qt.MouseButton.LeftButton | Qt.MouseButton.RightButton)
        self.setCursor(Qt.CursorShape.OpenHandCursor)
        self.setToolTip("拖动吸附到外轮廓边；双击切换横竖；右键更多")

    @property
    def tablet_size(self) -> tuple[int, int]:
        return (self._width, self._height)

    def set_size(self, width: int, height: int) -> None:
        width = int(width)
        height = int(height)
        if (width, height) == (self._width, self._height):
            return
        self.prepareGeometryChange()
        self._width = width
        self._height = height
        self.update()

    def set_connected(self, connected: bool) -> None:
        """未连接时不显示任何具体分辨率（不同型号尺寸不同）。"""
        connected = bool(connected)
        if self._connected != connected:
            self._connected = connected
            self.update()

    def set_highlighted(self, value: bool) -> None:
        if self._highlighted != value:
            self._highlighted = value
            self.update()

    def set_attached_edge(self, edge: Edge | None) -> None:
        if self._attached_edge is not edge:
            self._attached_edge = edge
            self.update()

    def set_pointer(self, point: QPointF | None) -> None:
        self._pointer = QPointF(point) if point is not None else None
        self.update()

    def boundingRect(self) -> QRectF:
        return QRectF(0.0, 0.0, float(self._width), float(self._height)).adjusted(
            -self.PADDING, -self.PADDING, self.PADDING, self.PADDING
        )

    def paint(self, painter: QPainter, option: Any, widget: Any = None) -> None:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        rect = QRectF(0.0, 0.0, float(self._width), float(self._height))
        radius = min(_device_px(painter, 8.0), rect.width() / 6.0, rect.height() / 6.0)
        border = color("capturing") if self._highlighted else color("accent")
        width = _device_px(painter, 2.6 if self._highlighted else 1.6)
        painter.setBrush(QBrush(color("bg_raised")))
        painter.setPen(QPen(border, max(1.0, width)))
        painter.drawRoundedRect(rect, radius, radius)

        inset = min(_device_px(painter, 10.0), rect.width() / 8.0, rect.height() / 8.0)
        inner = rect.adjusted(inset, inset, -inset, -inset)
        painter.setBrush(QBrush(color("bg_window")))
        painter.setPen(QPen(color("border"), max(0.6, _device_px(painter, 1.0))))
        painter.drawRoundedRect(inner, radius * 0.6, radius * 0.6)

        pad = inner.left() + _device_px(painter, 12.0)
        y = inner.top() + _device_px(painter, 28.0)
        painter.setFont(_font_for(painter, 13.0, bold=True))
        painter.setPen(QPen(color("text_primary")))
        if self._connected:
            painter.drawText(QPointF(pad, y), f"{self._width} × {self._height}")
            y += _device_px(painter, 18.0)
            painter.setFont(_font_for(painter, 11.0))
            painter.setPen(QPen(color("text_secondary")))
            orientation = "横屏" if self._width >= self._height else "竖屏"
            painter.drawText(QPointF(pad, y), f"{orientation}  ·  拖动吸附")
        else:
            painter.drawText(QPointF(pad, y), "未连接设备")
            y += _device_px(painter, 18.0)
            painter.setFont(_font_for(painter, 11.0))
            painter.setPen(QPen(color("text_secondary")))
            painter.drawText(QPointF(pad, y), "连接后显示实际尺寸")

        if self._attached_edge is not None:
            pen = QPen(color("capturing"), max(1.2, _device_px(painter, 4.0)))
            pen.setCapStyle(Qt.PenCapStyle.RoundCap)
            painter.setPen(pen)
            if self._attached_edge is Edge.LEFT:
                painter.drawLine(QPointF(0.0, 0.0), QPointF(0.0, rect.height()))
            elif self._attached_edge is Edge.RIGHT:
                painter.drawLine(QPointF(rect.width(), 0.0), QPointF(rect.width(), rect.height()))
            elif self._attached_edge is Edge.TOP:
                painter.drawLine(QPointF(0.0, 0.0), QPointF(rect.width(), 0.0))
            else:
                painter.drawLine(QPointF(0.0, rect.height()), QPointF(rect.width(), rect.height()))

        if self._pointer is not None:
            x = min(max(self._pointer.x(), 0.0), rect.width())
            y = min(max(self._pointer.y(), 0.0), rect.height())
            ghost = color("ok")
            ghost.setAlpha(190)
            radius_px = min(max(_device_px(painter, 6.0), 3.0), 40.0)
            painter.setPen(QPen(QColor(0, 0, 0, 140), max(0.6, _device_px(painter, 1.0))))
            painter.setBrush(QBrush(ghost))
            painter.drawEllipse(QPointF(x, y), radius_px, radius_px)

    # -- 鼠标事件转交画布 ------------------------------------------------
    def mousePressEvent(self, event: Any) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._owner is not None:
            self.setCursor(Qt.CursorShape.ClosedHandCursor)
            self._owner.begin_tablet_drag(event.scenePos())
            event.accept()
            return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: Any) -> None:
        if self._owner is not None and self._owner.is_dragging_tablet():
            self._owner.drag_tablet_to(event.scenePos())
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: Any) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._owner is not None:
            self._owner.end_tablet_drag()
            self.setCursor(Qt.CursorShape.OpenHandCursor)
            event.accept()
            return
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event: Any) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._owner is not None:
            self._owner.toggle_tablet_orientation()
            event.accept()
            return
        super().mouseDoubleClickEvent(event)

    def contextMenuEvent(self, event: Any) -> None:
        if self._owner is not None:
            self._owner.show_tablet_menu(event.screenPos())
            event.accept()
            return
        super().contextMenuEvent(event)


# ---------------------------------------------------------------------------
# GhostItem：PC 光标 / 平板估计指针
# ---------------------------------------------------------------------------
class GhostItem(QGraphicsItem):
    """半透明圆点，位置用场景坐标（虚拟桌面物理像素）。"""

    PADDING = 64.0

    def __init__(
        self,
        color_name: str = "accent",
        screen_radius: float = 5.0,
        parent: QGraphicsItem | None = None,
    ) -> None:
        super().__init__(parent)
        self._color_name = color_name
        self._screen_radius = float(screen_radius)
        self.setZValue(30)
        self.setAcceptedMouseButtons(Qt.MouseButton.NoButton)

    @property
    def screen_radius(self) -> float:
        return self._screen_radius

    def set_color_name(self, name: str) -> None:
        self._color_name = name
        self.update()

    def boundingRect(self) -> QRectF:
        return QRectF(-self.PADDING, -self.PADDING, self.PADDING * 2, self.PADDING * 2)

    def paint(self, painter: QPainter, option: Any, widget: Any = None) -> None:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        radius = min(max(_device_px(painter, self._screen_radius), 2.0), 60.0)
        fill = color(self._color_name)
        fill.setAlpha(200)
        painter.setPen(QPen(QColor(0, 0, 0, 150), max(0.6, _device_px(painter, 1.0))))
        painter.setBrush(QBrush(fill))
        painter.drawEllipse(QPointF(0.0, 0.0), radius, radius)
        painter.setBrush(QBrush(QColor(255, 255, 255, 220)))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.drawEllipse(QPointF(0.0, 0.0), radius * 0.32, radius * 0.32)


# ---------------------------------------------------------------------------
# 右键菜单构造（画布调用；单独导出便于测试）
# ---------------------------------------------------------------------------
def build_tablet_menu(
    parent: Any,
    current_align: Any,
    on_align: Any,
    on_remove: Any,
    on_test: Any,
) -> QMenu:
    menu = QMenu(parent)
    align_menu = menu.addMenu("对齐方式")
    for value, label in (
        ("proportional", "等比"),
        ("center", "居中"),
        ("one_to_one", "1:1"),
    ):
        action = align_menu.addAction(label)
        action.setCheckable(True)
        action.setChecked(str(getattr(current_align, "value", current_align)) == value)
        action.triggered.connect(lambda _checked=False, v=value: on_align(v))
    menu.addSeparator()
    remove_action = menu.addAction("移除")
    remove_action.triggered.connect(lambda _checked=False: on_remove())
    test_action = menu.addAction("测试穿越")
    test_action.triggered.connect(lambda _checked=False: on_test())
    return menu
