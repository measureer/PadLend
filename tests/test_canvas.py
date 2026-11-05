"""屏幕布局画布测试（offscreen）。

只依赖注入的 DesktopMap 协议，不 import win/desktop 的具体实现。
"""

from __future__ import annotations

import os
from collections import Counter

import pytest
from PySide6.QtCore import QEvent, QPointF, QRectF, Qt
from PySide6.QtGui import QKeyEvent, QMouseEvent
from PySide6.QtTest import QSignalSpy, QTest
from PySide6.QtWidgets import QApplication

from core.geometry import Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect
from ui.canvas import DesktopCanvas
from ui.canvas_items import (
    point_segment_distance,
    tablet_rect_for,
)

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")


# ---------------------------------------------------------------------------
# 测试替身：只实现 DesktopMap 的公开协议
# ---------------------------------------------------------------------------
class FakeDesktop:
    def __init__(self, monitors: list[MonitorInfo]) -> None:
        self._monitors = tuple(monitors)

    def set_monitors(self, monitors: list[MonitorInfo]) -> None:
        self._monitors = tuple(monitors)

    @property
    def monitors(self) -> tuple[MonitorInfo, ...]:
        return self._monitors

    @property
    def bounding_box(self) -> Rect:
        box = self._monitors[0].rect
        for monitor in self._monitors[1:]:
            box = box.union(monitor.rect)
        return box

    def _occupied(self, x: int, y: int) -> bool:
        return any(m.rect.contains(x, y) for m in self._monitors)

    def exterior_segments(self) -> tuple[EdgeSegment, ...]:
        """6.3.3 的 1px 扫描算法（仅测试用，显示器很小）。"""
        out: list[EdgeSegment] = []
        for monitor in self._monitors:
            rect = monitor.rect
            probes = (
                (Edge.LEFT, rect.left - 1, rect.top, rect.bottom, True),
                (Edge.RIGHT, rect.right, rect.top, rect.bottom, True),
                (Edge.TOP, rect.top - 1, rect.left, rect.right, False),
                (Edge.BOTTOM, rect.bottom, rect.left, rect.right, False),
            )
            for edge, probe, low, high, vertical in probes:
                start: int | None = None
                for along in range(low, high + 1):
                    probe_x = probe if vertical else along
                    probe_y = along if vertical else probe
                    outside = not self._occupied(probe_x, probe_y)
                    if outside and start is None:
                        start = along
                    elif not outside and start is not None:
                        out.append(EdgeSegment(monitor.handle, edge, start, along))
                        start = None
                if start is not None and start < high:
                    out.append(EdgeSegment(monitor.handle, edge, start, high))
        return tuple(out)

    def segments_at(self, x: int, y: int, band: int = 1) -> tuple[EdgeSegment, ...]:
        aloof: list[EdgeSegment] = []
        point = QPointF(float(x), float(y))
        for segment in self.exterior_segments():
            monitor = next(m for m in self._monitors if m.handle == segment.monitor_handle)
            if point_segment_distance(segment, monitor, point) <= max(band, 1):
                aloof.append(segment)
        return tuple(aloof)

    def monitor_at(self, x: int, y: int) -> MonitorInfo | None:
        for monitor in self._monitors:
            if monitor.rect.contains(x, y):
                return monitor
        return None


def make_monitor(
    handle: int,
    device: str,
    rect: Rect,
    dpi: int = 96,
    primary: bool = False,
) -> MonitorInfo:
    return MonitorInfo(handle=handle, device=device, rect=rect, dpi=dpi, primary=primary)


@pytest.fixture(scope="session")
def qapp() -> QApplication:
    app = QApplication.instance()
    if app is None:
        app = QApplication([])
    return app


@pytest.fixture()
def two_monitors() -> list[MonitorInfo]:
    return [
        make_monitor(1, r"\\.\DISPLAY1", Rect(0, 0, 1600, 900), 96, True),
        make_monitor(2, r"\\.\DISPLAY2", Rect(1600, 0, 3200, 900), 144, False),
    ]


@pytest.fixture()
def canvas(qapp: QApplication, two_monitors: list[MonitorInfo]) -> DesktopCanvas:
    view = DesktopCanvas(FakeDesktop(two_monitors))
    view.resize(1000, 700)
    view.show()
    QTest.qWait(10)
    yield view
    view.close()


def item_counts(canvas: DesktopCanvas) -> Counter[str]:
    return Counter(type(item).__name__ for item in canvas.scene().items())


# ---------------------------------------------------------------------------
# 场景构建
# ---------------------------------------------------------------------------
def test_scene_items_built(canvas: DesktopCanvas) -> None:
    counts = item_counts(canvas)
    assert counts["MonitorItem"] == 2
    # 左屏 left/top/bottom + 右屏 right/top/bottom = 6 段外轮廓
    assert counts["EdgeSegmentItem"] == 6 + 2  # + 两条内缝
    assert counts["TabletItem"] == 1
    assert counts["GhostItem"] == 1

    externals = [item for item in canvas.segment_items() if item.kind == "exterior"]
    assert len(externals) == 6
    assert len(canvas.monitor_items()) == 2


def test_scene_rect_covers_desktop_and_tablet(canvas: DesktopCanvas) -> None:
    # 场景矩形会被 fit_view 扩到平板摆位，导航范围必须同时覆盖桌面与平板。
    rect = canvas.sceneRect()
    assert rect.contains(QRectF(0.0, 0.0, 3200.0, 900.0))
    assert rect.contains(canvas._tablet_item.sceneBoundingRect())


def test_scene_rect_handles_negative_coordinates(qapp: QApplication) -> None:
    monitors = [
        make_monitor(1, r"\\.\DISPLAY1", Rect(0, 0, 1600, 900), 96, True),
        make_monitor(3, r"\\.\DISPLAY3", Rect(-1280, 0, 0, 1024), 120, False),
    ]
    view = DesktopCanvas(FakeDesktop(monitors))
    view.resize(800, 600)
    try:
        rect = view.sceneRect()
        assert rect.contains(QRectF(-1280.0, 0.0, 2880.0, 1024.0))
    finally:
        view.close()


def test_monitor_item_tooltip_and_geometry(canvas: DesktopCanvas) -> None:
    items = canvas.monitor_items()
    primary = next(item for item in items if item.info.primary)
    assert "主屏" in primary.tooltip_text()
    assert "1600×900" in primary.tooltip_text()
    assert "DPI 96" in primary.tooltip_text()
    rect = primary.boundingRect()
    assert rect.width() > 1600.0  # 含标签留白
    assert primary.pos().x() == pytest.approx(0.0)


# ---------------------------------------------------------------------------
# 摆位
# ---------------------------------------------------------------------------
def test_set_placement_attaches_tablet(canvas: DesktopCanvas) -> None:
    segment = EdgeSegment(2, Edge.RIGHT, 0, 900)
    placement = Placement(segment, 0.5, Align.PROPORTIONAL, None)
    canvas.set_placement(placement)

    assert canvas.placement is not None
    assert canvas.placement.segment == segment
    tablet = canvas.tablet_item
    width, height = canvas.tablet_size
    assert tablet.pos().x() == pytest.approx(3200.0)
    assert tablet.pos().y() == pytest.approx(450.0 - height / 2.0)
    assert tablet.tablet_size == (width, height)


def test_tablet_rect_helper_keeps_device_orientation() -> None:
    monitor = make_monitor(1, r"\\.\DISPLAY1", Rect(0, 0, 1600, 900))
    rect = tablet_rect_for(EdgeSegment(1, Edge.BOTTOM, 200, 1400), monitor, 0.5, 3200, 2136)
    assert rect.width() == pytest.approx(3200.0)
    assert rect.height() == pytest.approx(2136.0)
    assert rect.top() == pytest.approx(900.0)
    assert rect.left() == pytest.approx(800.0 - 1600.0)


def test_set_placement_offset_extremes(canvas: DesktopCanvas) -> None:
    segment = EdgeSegment(1, Edge.LEFT, 0, 900)
    for offset in (0.0, 1.0):
        canvas.set_placement(Placement(segment, offset))
        tablet = canvas.tablet_item
        expected_center = 0.0 + offset * 900.0
        assert tablet.pos().y() + canvas.tablet_size[1] / 2.0 == pytest.approx(expected_center)


def test_set_placement_falls_back_when_segment_stale(canvas: DesktopCanvas) -> None:
    stale = EdgeSegment(99, Edge.RIGHT, 0, 10)
    canvas.set_placement(Placement(stale, 0.25))
    placement = canvas.placement
    assert placement is not None
    assert placement.segment.monitor_handle in (1, 2)
    assert placement.segment.edge is Edge.RIGHT


# ---------------------------------------------------------------------------
# 拖动
# ---------------------------------------------------------------------------
def test_drag_uses_canvas_api_and_emits(canvas: DesktopCanvas) -> None:
    edited = QSignalSpy(canvas.placementEdited)
    committed = QSignalSpy(canvas.placementCommitted)

    canvas.begin_tablet_drag(QPointF(-1600.0, 450.0))
    assert canvas.is_dragging_tablet()
    canvas.drag_tablet_to(QPointF(800.0, 20.0))
    assert edited.count() == 1
    placement = canvas.placement
    assert placement is not None
    assert placement.segment.edge is Edge.TOP
    assert 0.0 <= placement.offset <= 1.0
    assert placement.offset == pytest.approx(0.5, abs=0.02)

    canvas.end_tablet_drag()
    assert not canvas.is_dragging_tablet()
    assert committed.count() == 1
    assert committed.at(0)[0].segment.edge is Edge.TOP


def test_drag_with_mouse_events(canvas: DesktopCanvas) -> None:
    committed = QSignalSpy(canvas.placementCommitted)
    start = canvas.mapFromScene(QPointF(-1600.0, 450.0))
    target = canvas.mapFromScene(QPointF(800.0, 20.0))

    QTest.mousePress(
        canvas.viewport(), Qt.MouseButton.LeftButton, Qt.KeyboardModifier.NoModifier, start
    )
    QTest.mouseMove(canvas.viewport(), target, 10)
    QTest.mouseRelease(
        canvas.viewport(), Qt.MouseButton.LeftButton, Qt.KeyboardModifier.NoModifier, target
    )

    assert committed.count() == 1
    placement = committed.at(0)[0]
    assert placement.segment.edge is Edge.TOP


def test_corner_candidates_and_shift_priority(canvas: DesktopCanvas) -> None:
    canvas.begin_tablet_drag(QPointF(100.0, 100.0))
    canvas.drag_tablet_to(QPointF(8.0, 8.0))

    highlighted = [item for item in canvas.segment_items() if item.highlighted]
    assert len(highlighted) == 2
    edges = {item.segment.edge for item in highlighted}
    assert edges == {Edge.LEFT, Edge.TOP}
    first = canvas.placement
    assert first is not None

    canvas.keyPressEvent(
        QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Shift, Qt.KeyboardModifier.NoModifier)
    )
    second = canvas.placement
    assert second is not None
    assert second.corner_priority is not None
    assert second.corner_priority is not first.segment.edge
    canvas.end_tablet_drag()


# ---------------------------------------------------------------------------
# 缩放 / 适配
# ---------------------------------------------------------------------------
def test_zoom_and_reset(canvas: DesktopCanvas) -> None:
    initial = canvas.transform().m11()
    assert initial > 0.0

    canvas.zoom_by(1.5)
    assert canvas.transform().m11() == pytest.approx(initial * 1.5, rel=1e-6)

    for _ in range(200):
        canvas.zoom_by(0.5)
    assert canvas.transform().m11() == pytest.approx(canvas.MIN_ZOOM, rel=1e-6)

    canvas.reset_view()
    assert canvas.transform().m11() == pytest.approx(initial, rel=1e-3)


def test_wheel_zoom_clamped(canvas: DesktopCanvas) -> None:
    for _ in range(200):
        canvas.zoom_by(2.0)
    zoom = canvas.transform().m11()
    assert canvas.MIN_ZOOM <= zoom <= canvas.MAX_ZOOM
    assert zoom == pytest.approx(canvas.MAX_ZOOM, rel=1e-6)


def test_ctrl_zero_resets_view(canvas: DesktopCanvas) -> None:
    initial = canvas.transform().m11()
    canvas.zoom_by(2.0)
    assert canvas.transform().m11() != pytest.approx(initial)

    canvas.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            Qt.Key.Key_0,
            Qt.KeyboardModifier.ControlModifier,
        )
    )
    assert canvas.transform().m11() == pytest.approx(initial, rel=1e-3)


# ---------------------------------------------------------------------------
# 信号 / 交互
# ---------------------------------------------------------------------------
def test_signals_exist(canvas: DesktopCanvas) -> None:
    for name in (
        "placementEdited",
        "placementCommitted",
        "tabletOrientationToggled",
        "testCaptureRequested",
        "pointerMoved",
    ):
        assert hasattr(canvas, name)
        assert QSignalSpy(getattr(canvas, name)).isValid()


def test_pointer_moved_signal(canvas: DesktopCanvas) -> None:
    spy = QSignalSpy(canvas.pointerMoved)
    event = QMouseEvent(
        QEvent.Type.MouseMove,
        QPointF(30.0, 40.0),
        QPointF(30.0, 40.0),
        Qt.MouseButton.NoButton,
        Qt.MouseButton.NoButton,
        Qt.KeyboardModifier.NoModifier,
    )
    canvas.mouseMoveEvent(event)
    assert spy.count() == 1
    expected = canvas.mapToScene(30, 40)
    assert spy.at(0)[0] == pytest.approx(float(expected.x()))
    assert spy.at(0)[1] == pytest.approx(float(expected.y()))


def test_double_click_toggles_orientation(canvas: DesktopCanvas) -> None:
    spy = QSignalSpy(canvas.tabletOrientationToggled)
    before = canvas.tablet_size
    center = canvas.mapFromScene(canvas.tablet_item.mapToScene(
        canvas.tablet_item.boundingRect().center()
    ))
    QTest.mouseDClick(
        canvas.viewport(), Qt.MouseButton.LeftButton, Qt.KeyboardModifier.NoModifier, center
    )
    assert spy.count() == 1
    assert canvas.tablet_size == (before[1], before[0])
    assert canvas.tablet_item.tablet_size == (before[1], before[0])


def test_set_tablet_size(canvas: DesktopCanvas) -> None:
    canvas.set_tablet_size(2136, 3200)
    assert canvas.tablet_size == (2136, 3200)
    assert canvas.tablet_item.tablet_size == (2136, 3200)


def test_pc_cursor_and_tablet_pointer(canvas: DesktopCanvas) -> None:
    canvas.set_pc_cursor(123.0, 456.0)
    assert canvas._pc_ghost.pos() == QPointF(123.0, 456.0)  # noqa: SLF001
    assert canvas._pc_ghost.isVisible()  # noqa: SLF001

    canvas.set_tablet_pointer(100.0, 200.0)
    assert canvas.tablet_item._pointer == QPointF(100.0, 200.0)  # noqa: SLF001

    canvas.set_tablet_pointer(99999.0, -5.0)  # 越界值不应崩溃（绘制时收敛）


# ---------------------------------------------------------------------------
# 右键菜单
# ---------------------------------------------------------------------------
def test_tablet_menu_actions(canvas: DesktopCanvas) -> None:
    menu = canvas.build_tablet_menu()
    labels = [action.text() for action in menu.actions()]
    assert "移除" in labels
    assert "测试穿越" in labels
    assert any(action.menu() is not None for action in menu.actions())


def test_menu_test_capture_and_remove(canvas: DesktopCanvas) -> None:
    test_spy = QSignalSpy(canvas.testCaptureRequested)
    committed = QSignalSpy(canvas.placementCommitted)

    menu = canvas.build_tablet_menu()
    for action in menu.actions():
        if action.text() == "测试穿越":
            action.trigger()
    assert test_spy.count() == 1

    for action in menu.actions():
        if action.text() == "移除":
            action.trigger()
    assert committed.count() == 1
    assert committed.at(0)[0] is None


# ---------------------------------------------------------------------------
# 热插拔刷新 / 绘制冒烟
# ---------------------------------------------------------------------------
def test_refresh_desktop_after_monitor_change(
    canvas: DesktopCanvas, two_monitors: list[MonitorInfo]
) -> None:
    desktop = canvas._desktop  # noqa: SLF001
    before = item_counts(canvas)
    desktop.set_monitors(two_monitors[:1])
    canvas.refresh_desktop()

    counts = item_counts(canvas)
    assert counts["MonitorItem"] == 1
    assert counts["EdgeSegmentItem"] == 4
    assert counts["TabletItem"] == 1
    assert before["EdgeSegmentItem"] > counts["EdgeSegmentItem"]


def test_render_smoke(canvas: DesktopCanvas) -> None:
    pixmap = canvas.grab()
    assert not pixmap.isNull()
    for factor in (0.1, 1.0, 5.0):
        canvas.zoom_by(factor)
        assert not canvas.grab().isNull()


def test_empty_desktop_does_not_crash(qapp: QApplication) -> None:
    class EmptyDesktop:
        monitors: tuple[MonitorInfo, ...] = ()
        bounding_box = Rect(0, 0, 0, 0)

        def exterior_segments(self) -> tuple[EdgeSegment, ...]:
            return ()

    view = DesktopCanvas(EmptyDesktop())
    view.resize(400, 300)
    try:
        assert view.placement is None
        assert view.segment_items() == ()
        assert not view.tablet_item.isVisible()
    finally:
        view.close()
