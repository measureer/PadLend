"""屏幕布局画布（设计文档 6.10.1 / 6.10.2）。

场景坐标 = 虚拟桌面物理像素（允许负坐标，支持多显示器），视图变换负责缩放，
因此 1 场景单位 = 1 物理像素，真实比例天然成立（6.10.6）。

``desktop`` 由外部注入（``win.desktop.DesktopMap`` 或测试替身），本模块只按
结构化协议使用它，不 import 任何具体实现：

- ``monitors``：``tuple[MonitorInfo, ...]``
- ``bounding_box``：``Rect``
- ``exterior_segments()``：``tuple[EdgeSegment, ...]``
- ``segments_at(x, y, band=1)``：命中点附近的外轮廓段（可选，缺失时本地计算）
- ``resolve_corner(segs, delta, priority)``：可选，缺失时用 6.3.6 的本地实现
"""

from __future__ import annotations

from functools import partial
from typing import Any

from PySide6.QtCore import QPointF, QRectF, Qt, Signal
from PySide6.QtGui import QMouseEvent, QPainter, QResizeEvent, QWheelEvent
from PySide6.QtWidgets import QGraphicsScene, QGraphicsView, QMenu, QWidget

from core.geometry import Align, Edge, EdgeSegment, Placement, Rect
from ui.canvas_items import (
    EdgeSegmentItem,
    GhostItem,
    MonitorItem,
    TabletItem,
    build_tablet_menu,
    color,
    interior_seams,
    monitor_for_segment,
    point_segment_distance,
    resolve_corner_local,
    segment_offset,
    tablet_rect_for,
)

DEFAULT_TABLET_SIZE = (3200, 2136)


class DesktopCanvas(QGraphicsView):
    """虚拟桌面 + 平板摆位画布。"""

    placementEdited = Signal(object)  # 拖动过程中的实时 Placement
    placementCommitted = Signal(object)  # 预览 Placement（移除为 None；保存布局才落盘）
    tabletOrientationToggled = Signal()  # 双击切换横竖
    testCaptureRequested = Signal()  # 右键"测试穿越"
    pointerMoved = Signal(float, float)  # 鼠标在虚拟桌面物理坐标中的位置

    SNAP_BAND = 64.0
    MIN_ZOOM = 0.02
    MAX_ZOOM = 8.0

    def __init__(
        self,
        desktop: Any,
        placement: Placement | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._desktop = desktop
        self._scene = QGraphicsScene(self)
        self.setScene(self._scene)

        self._tablet_size: tuple[int, int] = DEFAULT_TABLET_SIZE
        self._monitors: tuple[Any, ...] = ()
        self._segments: tuple[EdgeSegment, ...] = ()
        self._bounding_box: Rect | None = None
        self._monitor_items: list[MonitorItem] = []
        self._segment_items: list[EdgeSegmentItem] = []
        self._placement: Placement | None = None
        self._dragging = False
        self._drag_origin = QPointF(0.0, 0.0)
        self._last_drag_pos = QPointF(0.0, 0.0)
        self._corner_priority: Edge | None = None
        self._ghosts_visible = True
        self._has_pc_cursor = False
        self._auto_fit = True

        self._tablet_item = TabletItem(self._tablet_size, owner=self)
        self._pc_ghost = GhostItem("accent", 5.0)
        self._pc_ghost.hide()

        self.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        self.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)
        self.setTransformationAnchor(QGraphicsView.ViewportAnchor.AnchorUnderMouse)
        self.setResizeAnchor(QGraphicsView.ViewportAnchor.AnchorViewCenter)
        self.setDragMode(QGraphicsView.DragMode.NoDrag)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setBackgroundBrush(color("bg_window"))

        self.refresh_desktop()
        if placement is not None:
            self.set_placement(placement)

    # ------------------------------------------------------------------
    # 只读状态
    # ------------------------------------------------------------------
    @property
    def tablet_item(self) -> TabletItem:
        return self._tablet_item

    @property
    def tablet_size(self) -> tuple[int, int]:
        return self._tablet_size

    @property
    def placement(self) -> Placement | None:
        """当前画布摆位（未设置时返回默认预览；保存布局才写入配置）。"""
        if self._placement is not None:
            return self._placement
        return self._default_placement()

    @property
    def monitors(self) -> tuple[Any, ...]:
        return self._monitors

    @property
    def segments(self) -> tuple[EdgeSegment, ...]:
        return self._segments

    def is_dragging_tablet(self) -> bool:
        return self._dragging

    def segment_items(self) -> tuple[EdgeSegmentItem, ...]:
        return tuple(self._segment_items)

    def monitor_items(self) -> tuple[MonitorItem, ...]:
        return tuple(self._monitor_items)

    # ------------------------------------------------------------------
    # 公共操作
    # ------------------------------------------------------------------
    def set_placement(self, placement: Placement | None) -> None:
        """程序化设置摆位（不发射信号）。"""
        self._placement = self._resolve_placement(placement)
        if self._placement is None:
            self._tablet_item.set_attached_edge(None)
            self._clear_segment_states()
        else:
            self._apply_placement(self._placement)
        if self._auto_fit:
            self.fit_view()

    def set_tablet_size(self, width: int, height: int) -> None:
        """设置平板尺寸（平板像素，横屏 3200×2136 / 竖屏 2136×3200）。"""
        self._tablet_size = (int(width), int(height))
        self._tablet_item.set_size(*self._tablet_size)
        current = self._placement or self._default_placement()
        if current is not None:
            self._apply_placement(current)
        if self._auto_fit:
            self.fit_view()

    def set_connected(self, connected: bool) -> None:
        """未连接时画布只显示占位文案，不暴露默认分辨率。"""
        self._tablet_item.set_connected(connected)

    def set_pc_cursor(self, x: float, y: float) -> None:
        """PC 光标位置（虚拟桌面物理像素）。"""
        self._pc_ghost.setPos(float(x), float(y))
        self._has_pc_cursor = True
        self._pc_ghost.setVisible(self._ghosts_visible)

    def set_tablet_pointer(self, x: float, y: float) -> None:
        """平板估计指针位置（平板像素，左上角原点）。"""
        self._tablet_item.set_pointer(QPointF(float(x), float(y)))

    def set_ghosts_visible(self, visible: bool) -> None:
        self._ghosts_visible = bool(visible)
        self._pc_ghost.setVisible(self._ghosts_visible and self._has_pc_cursor)
        if not self._ghosts_visible:
            self._tablet_item.set_pointer(None)

    def refresh_desktop(self) -> None:
        """按当前 DesktopMap 重建场景（显示器热插拔后由外部调用）。"""
        self._monitors = self._read_sequence("monitors")
        self._segments = tuple(self._read_sequence("exterior_segments"))
        self._bounding_box = self._read_bounding_box()

        for item in [*self._monitor_items, *self._segment_items]:
            self._scene.removeItem(item)
        self._monitor_items = []
        self._segment_items = []

        for info in self._monitors:
            item = MonitorItem(info)
            self._scene.addItem(item)
            self._monitor_items.append(item)

        for segment in self._segments:
            monitor = monitor_for_segment(self._monitors, segment)
            if monitor is None:
                continue
            item = EdgeSegmentItem(segment, monitor, kind="exterior")
            self._scene.addItem(item)
            self._segment_items.append(item)

        for monitor in self._monitors:
            for edge, start, end in interior_seams(self._monitors, monitor):
                try:
                    seam = EdgeSegment(monitor.handle, edge, start, end)
                except ValueError:  # pragma: no cover - 防御异常输入
                    continue
                item = EdgeSegmentItem(seam, monitor, kind="seam")
                self._scene.addItem(item)
                self._segment_items.append(item)

        if self._tablet_item.scene() is None:
            self._scene.addItem(self._tablet_item)
        if self._pc_ghost.scene() is None:
            self._scene.addItem(self._pc_ghost)

        box = self._bounding_box
        if box is None or box.empty:
            self._scene.setSceneRect(QRectF(0.0, 0.0, 1.0, 1.0))
            self._tablet_item.setVisible(False)
            return

        self._scene.setSceneRect(QRectF(box.left, box.top, box.width, box.height))
        self._placement = self._resolve_placement(self._placement)
        current = self._placement or self._default_placement()
        if current is not None:
            self._apply_placement(current)
            self._tablet_item.setVisible(True)
        else:
            self._tablet_item.setVisible(False)

        if self._auto_fit:
            self.fit_view()

    def toggle_tablet_orientation(self) -> None:
        """横/竖屏切换（双击平板），同步当前摆位并发出信号。"""
        width, height = self._tablet_size
        self._tablet_size = (height, width)
        self._tablet_item.set_size(*self._tablet_size)
        current = self._placement or self._default_placement()
        if current is not None:
            self._apply_placement(current)
        self.tabletOrientationToggled.emit()
        if self._auto_fit:
            self.fit_view()

    def set_align(self, align: Align | str) -> None:
        """改变对齐方式并发出预览（右键菜单用；保存布局才落盘）。"""
        current = self._placement or self._default_placement()
        if current is None:
            return
        value = align if isinstance(align, Align) else Align(str(align))
        placement = Placement(current.segment, current.offset, value, current.corner_priority)
        self._placement = placement
        self._apply_placement(placement)
        self.placementCommitted.emit(placement)

    def clear_placement(self) -> None:
        """移除摆位（右键菜单用）：保留平板可拖，但不再有预览摆位。"""
        self._placement = None
        self._clear_segment_states()
        current = self._default_placement()
        if current is not None:
            self._apply_placement(current, mark_attached=False)
        self.placementCommitted.emit(None)

    # ------------------------------------------------------------------
    # 缩放 / 适配 / 平移
    # ------------------------------------------------------------------
    def reset_view(self) -> None:
        """Ctrl+0：回到自动适配。"""
        self._auto_fit = True
        self.fit_view()

    def fit_view(self) -> None:
        rect = self._fit_rect()
        if rect is None or rect.isEmpty():
            return
        # 平板可能位于桌面包围盒之外；sceneRect 只含显示器时，fitInView
        # 的滚动范围会被夹回桌面内，布局页右侧的平板就会被裁掉。
        if self._scene.sceneRect() != rect:
            self._scene.setSceneRect(rect)
        self.fitInView(rect, Qt.AspectRatioMode.KeepAspectRatio)
        self._auto_fit = True

    def zoom_by(self, factor: float) -> None:
        if factor <= 0.0:
            return
        current = self.transform().m11()
        if current <= 0.0:
            return
        target = min(max(current * factor, self.MIN_ZOOM), self.MAX_ZOOM)
        applied = target / current
        if abs(applied - 1.0) < 1e-6:
            return
        self.scale(applied, applied)
        self._auto_fit = False

    # ------------------------------------------------------------------
    # 平板拖动（TabletItem 转交）
    # ------------------------------------------------------------------
    def begin_tablet_drag(self, scene_pos: QPointF) -> None:
        self._dragging = True
        self._drag_origin = QPointF(scene_pos)
        self._last_drag_pos = QPointF(scene_pos)
        self._tablet_item.set_highlighted(True)

    def drag_tablet_to(self, scene_pos: QPointF) -> None:
        if not self._dragging:
            return
        self._last_drag_pos = QPointF(scene_pos)
        placement = self._snap_at(scene_pos)
        if placement is None:
            return
        self._placement = placement
        self._apply_placement(placement)
        self.placementEdited.emit(placement)

    def end_tablet_drag(self) -> None:
        if not self._dragging:
            return
        self._dragging = False
        self._tablet_item.set_highlighted(False)
        for item in self._segment_items:
            item.set_highlighted(False)
            item.set_active(False)
        placement = self._placement or self._default_placement()
        if placement is not None:
            self._placement = placement
            self._apply_placement(placement)
            self.placementCommitted.emit(placement)

    # ------------------------------------------------------------------
    # 右键菜单
    # ------------------------------------------------------------------
    def build_tablet_menu(self) -> QMenu:
        current = self._placement or self._default_placement()
        align = current.align if current is not None else Align.PROPORTIONAL
        return build_tablet_menu(
            self,
            align,
            partial(self.set_align),
            self.clear_placement,
            self.testCaptureRequested.emit,
        )

    def show_tablet_menu(self, screen_pos: Any) -> None:
        menu = self.build_tablet_menu()
        menu.exec(screen_pos)

    # ------------------------------------------------------------------
    # Qt 事件
    # ------------------------------------------------------------------
    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        if self._auto_fit:
            self.fit_view()

    def wheelEvent(self, event: QWheelEvent) -> None:
        delta = event.angleDelta().y()
        if delta == 0:
            event.ignore()
            return
        self.zoom_by(1.0015**delta)
        event.accept()

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        scene_pos = self.mapToScene(event.position().toPoint())
        self.pointerMoved.emit(float(scene_pos.x()), float(scene_pos.y()))
        super().mouseMoveEvent(event)

    def keyPressEvent(self, event: Any) -> None:
        key = event.key()
        modifiers = event.modifiers()
        if key == Qt.Key.Key_Space and not event.isAutoRepeat():
            self.setDragMode(QGraphicsView.DragMode.ScrollHandDrag)
            self.setCursor(Qt.CursorShape.OpenHandCursor)
            event.accept()
            return
        if key == Qt.Key.Key_0 and modifiers & Qt.KeyboardModifier.ControlModifier:
            self.reset_view()
            event.accept()
            return
        if key == Qt.Key.Key_Shift and self._dragging:
            self._toggle_corner_priority()
            event.accept()
            return
        super().keyPressEvent(event)

    def keyReleaseEvent(self, event: Any) -> None:
        if event.key() == Qt.Key.Key_Space and not event.isAutoRepeat():
            self.setDragMode(QGraphicsView.DragMode.NoDrag)
            self.unsetCursor()
            event.accept()
            return
        super().keyReleaseEvent(event)

    # ------------------------------------------------------------------
    # 内部：吸附
    # ------------------------------------------------------------------
    def _snap_at(self, scene_pos: QPointF) -> Placement | None:
        candidates = self._nearby_segments(scene_pos)
        if not candidates:
            return self._placement
        chosen = candidates[0][1]
        highlight: list[EdgeSegment] = [chosen]
        others = [
            (distance, segment)
            for distance, segment in candidates
            if segment.edge is not chosen.edge and distance <= self.SNAP_BAND
        ]
        if others:
            pair = (chosen, others[0][1])
            chosen = self._resolve_corner(pair, scene_pos)
            highlight = list(pair)
        self._set_segment_highlight(highlight, chosen)
        align = self._placement.align if self._placement is not None else Align.PROPORTIONAL
        return Placement(chosen, segment_offset(chosen, scene_pos), align, self._corner_priority)

    def _nearby_segments(self, scene_pos: QPointF) -> list[tuple[float, EdgeSegment]]:
        if not self._segments:
            return []
        near: tuple[EdgeSegment, ...] = ()
        segments_at = getattr(self._desktop, "segments_at", None)
        if callable(segments_at):
            try:
                near = tuple(
                    segments_at(int(scene_pos.x()), int(scene_pos.y()), int(self.SNAP_BAND))
                )
            except (TypeError, ValueError):  # 注入对象语义差异时回退到全量
                near = ()
        pool = near if near else self._segments
        scored: list[tuple[float, EdgeSegment]] = []
        for segment in pool:
            monitor = monitor_for_segment(self._monitors, segment)
            if monitor is None:
                continue
            scored.append((point_segment_distance(segment, monitor, scene_pos), segment))
        scored.sort(key=lambda pair: pair[0])
        within = [pair for pair in scored if pair[0] <= self.SNAP_BAND]
        if within:
            return within
        return scored[:1]

    def _resolve_corner(
        self, pair: tuple[EdgeSegment, EdgeSegment], scene_pos: QPointF
    ) -> EdgeSegment:
        delta = (scene_pos.x() - self._drag_origin.x(), scene_pos.y() - self._drag_origin.y())
        resolver = getattr(self._desktop, "resolve_corner", None)
        if callable(resolver):
            try:
                resolved = resolver(tuple(pair), delta, self._corner_priority)
            except (TypeError, ValueError):
                resolved = None
            if resolved is not None:
                return resolved
        return resolve_corner_local(tuple(pair), delta, self._corner_priority) or pair[0]

    def _toggle_corner_priority(self) -> None:
        candidates = self._nearby_segments(self._last_drag_pos)
        edges: list[Edge] = []
        for _, segment in candidates:
            if segment.edge not in edges:
                edges.append(segment.edge)
        if not edges:
            return
        if self._corner_priority is None:
            self._corner_priority = edges[1] if len(edges) > 1 else edges[0]
        elif self._corner_priority in edges:
            index = edges.index(self._corner_priority)
            self._corner_priority = edges[(index + 1) % len(edges)]
        else:
            self._corner_priority = edges[0]
        self.drag_tablet_to(self._last_drag_pos)

    # ------------------------------------------------------------------
    # 内部：摆位应用 / 场景构建
    # ------------------------------------------------------------------
    def _default_placement(self) -> Placement | None:
        if not self._segments:
            return None
        segment = self._segments[0]
        align = self._placement.align if self._placement is not None else Align.PROPORTIONAL
        return Placement(segment, 0.5, align, self._corner_priority)

    def _resolve_placement(self, placement: Placement | None) -> Placement | None:
        if placement is None:
            return None
        if monitor_for_segment(self._monitors, placement.segment) is not None:
            return placement
        fallback = next(
            (segment for segment in self._segments if segment.edge is placement.segment.edge),
            self._segments[0] if self._segments else None,
        )
        if fallback is None:
            return None
        return Placement(fallback, placement.offset, placement.align, placement.corner_priority)

    def _apply_placement(self, placement: Placement, mark_attached: bool = True) -> None:
        monitor = monitor_for_segment(self._monitors, placement.segment)
        if monitor is None:
            return
        width, height = self._tablet_size
        rect = tablet_rect_for(placement.segment, monitor, placement.offset, width, height)
        self._tablet_item.set_size(width, height)
        self._tablet_item.setPos(rect.topLeft())
        self._tablet_item.set_attached_edge(placement.segment.edge)
        if mark_attached:
            self._mark_attached(placement.segment)

    def _mark_attached(self, segment: EdgeSegment) -> None:
        for item in self._segment_items:
            item.set_attached(item.segment == segment)

    def _clear_segment_states(self) -> None:
        for item in self._segment_items:
            item.set_attached(False)
            item.set_highlighted(False)
            item.set_active(False)

    def _set_segment_highlight(
        self, segments: list[EdgeSegment], active: EdgeSegment
    ) -> None:
        wanted = set(segments)
        for item in self._segment_items:
            if item.kind != "exterior":
                continue
            item.set_highlighted(item.segment in wanted)
            item.set_active(item.segment == active)

    def _fit_rect(self) -> QRectF | None:
        box = self._bounding_box
        if box is None or box.empty:
            return None
        rect = QRectF(box.left, box.top, box.width, box.height)
        if self._tablet_item.isVisible():
            rect = rect.united(self._tablet_item.sceneBoundingRect())
        margin = max(24.0, 0.06 * max(rect.width(), rect.height()))
        return rect.adjusted(-margin, -margin, margin, margin)

    def _read_sequence(self, name: str) -> tuple[Any, ...]:
        value = getattr(self._desktop, name)
        if callable(value):
            value = value()
        return tuple(value or ())

    def _read_bounding_box(self) -> Rect | None:
        value = getattr(self._desktop, "bounding_box", None)
        if value is None:
            return None
        if callable(value):
            value = value()
        return value
