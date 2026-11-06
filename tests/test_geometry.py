"""虚拟桌面几何、沿边映射与单实例单测（设计文档 6.3 / 8.1 / 8.2 / 13）。"""

from __future__ import annotations

import ctypes
import json
import sys
import threading
import uuid
from pathlib import Path
from typing import Any

import pytest

import constants as C
import core.geometry as geometry
from core.errors import ErrorCode, PadLendError
from core.geometry import Edge, EdgeSegment, MonitorInfo, Placement, Rect
from win import desktop as desktop_module
from win import dpi, single_instance
from win.desktop import (
    DesktopMap,
    compute_exterior_segments,
    pc_to_tablet_along,
    tablet_to_pc_along,
)

FIXTURES = Path(__file__).resolve().parent / "fixtures"
TABLET_W = 3200
TABLET_H = 2136


def fixture_data(name: str) -> dict[str, Any]:
    path = FIXTURES / f"desktop_{name}.json"
    return json.loads(path.read_text(encoding="utf-8"))


def monitors_from(name: str) -> tuple[MonitorInfo, ...]:
    data = fixture_data(name)
    monitors: list[MonitorInfo] = []
    for item in data["monitors"]:
        left = int(item["left"])
        top = int(item["top"])
        monitors.append(
            MonitorInfo(
                handle=int(item["handle"]),
                device=str(item["device"]),
                rect=Rect(left, top, left + int(item["width"]), top + int(item["height"])),
                dpi=int(item["dpi"]),
                primary=bool(item["primary"]),
            )
        )
    return tuple(monitors)


def make_map(name: str) -> DesktopMap:
    return DesktopMap(monitors=monitors_from(name))


def edge_spans(segments: tuple[EdgeSegment, ...], edge: Edge) -> set[tuple[int, int]]:
    return {(seg.span_start, seg.span_end) for seg in segments if seg.edge == edge}


def segments_by_edge(dm: DesktopMap) -> dict[Edge, EdgeSegment]:
    return {seg.edge: seg for seg in dm.exterior_segments()}


# ---------------------------------------------------------------------------
# 类型契约与纯函数
# ---------------------------------------------------------------------------
def test_geometry_types_are_reexported_not_redefined():
    assert desktop_module.Edge is geometry.Edge
    assert desktop_module.Align is geometry.Align
    assert desktop_module.Rect is geometry.Rect
    assert desktop_module.MonitorInfo is geometry.MonitorInfo
    assert desktop_module.EdgeSegment is geometry.EdgeSegment
    assert desktop_module.Placement is geometry.Placement


def test_compute_exterior_segments_is_pure_and_order_independent():
    for name in (
        "single",
        "dual_horizontal",
        "dual_vertical",
        "l_shape",
        "t_shape",
        "negative",
    ):
        monitors = monitors_from(name)
        dm = DesktopMap(monitors=monitors)
        assert compute_exterior_segments(monitors) == dm.exterior_segments()
        assert compute_exterior_segments(tuple(reversed(monitors))) == dm.exterior_segments()


def test_empty_desktop_map():
    dm = DesktopMap(monitors=())
    assert dm.monitors == ()
    assert dm.exterior_segments() == ()
    assert dm.bounding_box == Rect(0, 0, 0, 0)
    assert dm.monitor_at(0, 0) is None
    assert dm.segments_at(0, 0) == ()
    assert dm.resolve_corner((), (1, 1), Edge.TOP) is None


# ---------------------------------------------------------------------------
# 布局：单屏 / 左右双屏 / 上下双屏
# ---------------------------------------------------------------------------
def test_single_screen_exterior_segments():
    dm = make_map("single")
    expected = {
        EdgeSegment(1, Edge.LEFT, 0, 1080),
        EdgeSegment(1, Edge.RIGHT, 0, 1080),
        EdgeSegment(1, Edge.TOP, 0, 1920),
        EdgeSegment(1, Edge.BOTTOM, 0, 1920),
    }
    segments = dm.exterior_segments()
    assert set(segments) == expected
    assert len(segments) == 4
    assert [seg.edge for seg in segments] == [Edge.LEFT, Edge.RIGHT, Edge.TOP, Edge.BOTTOM]
    assert dm.bounding_box == Rect(0, 0, 1920, 1080)
    assert dm.monitor_at(0, 0) == dm.monitors[0]
    assert dm.monitor_at(1919, 1079) == dm.monitors[0]
    assert dm.monitor_at(1920, 0) is None
    assert dm.monitor_at(-1, 0) is None


def test_dual_horizontal_merges_outer_edges():
    dm = make_map("dual_horizontal")
    segments = dm.exterior_segments()
    assert len(segments) == 4
    assert edge_spans(segments, Edge.LEFT) == {(0, 1080)}
    assert edge_spans(segments, Edge.RIGHT) == {(0, 1080)}
    assert edge_spans(segments, Edge.TOP) == {(0, 3840)}
    assert edge_spans(segments, Edge.BOTTOM) == {(0, 3840)}
    assert all(
        seg.length == 3840 for seg in segments if seg.edge in (Edge.TOP, Edge.BOTTOM)
    )
    # 显示器之间的竖直内缝不能成为吸附边。
    assert dm.segments_at(1920, 500) == ()
    assert dm.bounding_box == Rect(0, 0, 3840, 1080)
    assert dm.monitor_at(1919, 0) == dm.monitors[0]
    assert dm.monitor_at(1920, 0) == dm.monitors[1]
    assert dm.monitor_at(2000, 0) == dm.monitors[1]
    assert dm.monitor_at(2000, 0).dpi == 144  # type: ignore[union-attr]
    assert dm.monitors[1].scale == 1.5


def test_dual_vertical_merges_side_edges():
    dm = make_map("dual_vertical")
    segments = dm.exterior_segments()
    assert len(segments) == 4
    assert edge_spans(segments, Edge.LEFT) == {(0, 2160)}
    assert edge_spans(segments, Edge.RIGHT) == {(0, 2160)}
    assert edge_spans(segments, Edge.TOP) == {(0, 1920)}
    assert edge_spans(segments, Edge.BOTTOM) == {(0, 1920)}
    # 上下双屏之间的水平内缝同样不可吸附。
    assert dm.segments_at(500, 1080) == ()
    assert dm.bounding_box == Rect(0, 0, 1920, 2160)
    assert dm.monitor_at(0, 1079) == dm.monitors[0]
    assert dm.monitor_at(0, 1080) == dm.monitors[1]


# ---------------------------------------------------------------------------
# 布局：L 形 / T 形 / 负坐标
# ---------------------------------------------------------------------------
def test_l_shape_exterior_segments():
    dm = make_map("l_shape")
    segments = dm.exterior_segments()
    assert len(segments) == 6
    assert edge_spans(segments, Edge.LEFT) == {(0, 2104)}
    assert edge_spans(segments, Edge.RIGHT) == {(0, 1080), (1080, 2104)}
    assert edge_spans(segments, Edge.TOP) == {(0, 1920)}
    assert edge_spans(segments, Edge.BOTTOM) == {(0, 1280), (1280, 1920)}
    assert dm.bounding_box == Rect(0, 0, 1920, 2104)
    assert dm.monitor_at(1279, 2103) == dm.monitors[1]
    assert dm.monitor_at(1280, 1080) is None
    assert dm.segments_at(1280, 500) == ()


def test_t_shape_three_monitors_exterior_segments():
    dm = make_map("t_shape")
    segments = dm.exterior_segments()
    assert len(segments) == 8
    assert edge_spans(segments, Edge.TOP) == {(0, 2560)}
    assert edge_spans(segments, Edge.BOTTOM) == {(0, 640), (640, 1920), (1920, 2560)}
    assert edge_spans(segments, Edge.LEFT) == {(0, 1024), (1024, 2048)}
    assert edge_spans(segments, Edge.RIGHT) == {(0, 1024), (1024, 2048)}
    assert dm.bounding_box == Rect(0, 0, 2560, 2048)
    assert dm.segments_at(1280, 500) == ()
    assert dm.segments_at(1000, 1024) == ()
    assert dm.monitor_at(639, 1024) is None
    assert dm.monitor_at(640, 1024) == dm.monitors[2]


def test_negative_coordinates_exterior_segments():
    dm = make_map("negative")
    segments = dm.exterior_segments()
    assert len(segments) == 4
    assert edge_spans(segments, Edge.LEFT) == {(0, 1080)}
    assert edge_spans(segments, Edge.RIGHT) == {(0, 1080)}
    assert edge_spans(segments, Edge.TOP) == {(-1920, 1920)}
    assert edge_spans(segments, Edge.BOTTOM) == {(-1920, 1920)}
    assert dm.bounding_box == Rect(-1920, 0, 1920, 1080)
    assert dm.monitor_at(-1920, 0) == dm.monitors[1]
    assert dm.monitor_at(-1, 50) == dm.monitors[1]
    assert dm.monitor_at(0, 0) == dm.monitors[0]
    assert dm.monitor_at(1920, 0) is None
    # x=0 是两屏之间的内缝，不是外轮廓。
    assert dm.segments_at(0, 500) == ()
    assert edge_spans(dm.segments_at(-1921, 500), Edge.LEFT) == {(0, 1080)}


def test_merge_requires_same_edge_line():
    first = MonitorInfo(1, r"\\.\DISPLAY1", Rect(0, 0, 100, 100), 96, True)
    second = MonitorInfo(2, r"\\.\DISPLAY2", Rect(200, 100, 300, 200), 96, False)
    segments = compute_exterior_segments((first, second))
    lefts = [seg for seg in segments if seg.edge is Edge.LEFT]
    assert len(lefts) == 2
    assert {(seg.span_start, seg.span_end) for seg in lefts} == {(0, 100), (100, 200)}


# ---------------------------------------------------------------------------
# 命中 / 角落裁决 / 沿边映射
# ---------------------------------------------------------------------------
def test_segments_at_band_and_corners():
    dm = make_map("single")
    by_edge = segments_by_edge(dm)
    left, top = by_edge[Edge.LEFT], by_edge[Edge.TOP]
    assert dm.segments_at(-1, 500) == (left,)
    assert dm.segments_at(-3, 500) == ()
    assert dm.segments_at(-3, 500, band=2) == (left,)
    assert dm.segments_at(-1, 500, band=0) == (left,)
    assert dm.segments_at(-1, 500, band=-5) == (left,)
    assert set(dm.segments_at(0, 0)) == {left, top}
    assert {seg.edge for seg in dm.segments_at(1919, 1079)} == {Edge.RIGHT, Edge.BOTTOM}
    assert {seg.edge for seg in dm.segments_at(0, 1080)} == {Edge.BOTTOM}
    assert dm.segments_at(960, 540) == ()


def test_resolve_corner_rules():
    dm = make_map("single")
    by_edge = segments_by_edge(dm)
    left, top = by_edge[Edge.LEFT], by_edge[Edge.TOP]
    corner = (left, top)
    assert dm.resolve_corner((), (1, 1), Edge.TOP) is None
    assert dm.resolve_corner((left,), (1, 1), Edge.TOP) is left
    assert dm.resolve_corner(corner, (-1, 0), Edge.TOP) is left
    assert dm.resolve_corner(corner, (0, -1), Edge.LEFT) is top
    assert dm.resolve_corner(corner, (-1, -1), Edge.TOP) is top
    assert dm.resolve_corner(corner, (-1, -1), None) is left
    assert dm.resolve_corner(corner, (0, 0), Edge.TOP) is top
    assert dm.resolve_corner(corner, (1, 1), Edge.TOP) is top  # 分数均负，走 priority
    assert dm.resolve_corner(corner, (-100, -1), Edge.TOP) is left  # 方向差 > 15%
    assert dm.resolve_corner(corner, (-10, -9), Edge.TOP) is top  # 方向差 <= 15%
    assert dm.resolve_corner(corner, (1, 1), Edge.RIGHT) is left  # 无匹配 priority


def test_along_mapping_formulas():
    dm = make_map("single")
    by_edge = segments_by_edge(dm)
    right, top = by_edge[Edge.RIGHT], by_edge[Edge.TOP]

    assert pc_to_tablet_along(0, right, TABLET_H) == 0.0
    assert pc_to_tablet_along(540, right, TABLET_H) == pytest.approx(1067.5)
    assert pc_to_tablet_along(1079, right, TABLET_H) == pytest.approx(1079 / 1080 * 2135)
    assert pc_to_tablet_along(1080, right, TABLET_H) == pytest.approx(2135.0)
    assert pc_to_tablet_along(-50, right, TABLET_H) == 0.0

    assert tablet_to_pc_along(0, right, TABLET_H) == 0
    assert tablet_to_pc_along(1067.5, right, TABLET_H) == 540
    assert tablet_to_pc_along(2135, right, TABLET_H) == 1079
    assert tablet_to_pc_along(-10, right, TABLET_H) == 0
    assert tablet_to_pc_along(9999, right, TABLET_H) == 1079

    # 水平边用平板宽；t_size=1 时避免除零。
    assert pc_to_tablet_along(960, top, TABLET_W) == pytest.approx(1599.5)
    assert tablet_to_pc_along(1599.5, top, TABLET_W) == 960
    assert pc_to_tablet_along(500, right, 1) == 0.0
    assert tablet_to_pc_along(0, right, 1) == 0
    assert tablet_to_pc_along(50, right, 1) == 1079  # t_size=1 时非零 t 夹到 1


# ---------------------------------------------------------------------------
# 入口 / 出口坐标
# ---------------------------------------------------------------------------
def test_entry_and_exit_single_screen():
    dm = make_map("single")
    by_edge = segments_by_edge(dm)

    right = Placement(by_edge[Edge.RIGHT])
    assert dm.entry_point(right, 540, TABLET_W, TABLET_H) == (0.0, 1067.5)
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H) == (1918, 540)
    assert dm.exit_position(right, -1, TABLET_W, TABLET_H) == (1918, 0)
    assert dm.exit_position(right, 10_000, TABLET_W, TABLET_H) == (1918, 1079)

    left = Placement(by_edge[Edge.LEFT])
    assert dm.entry_point(left, 540, TABLET_W, TABLET_H) == (3199.0, 1067.5)
    assert dm.exit_position(left, 1067.5, TABLET_W, TABLET_H) == (1, 540)

    top = Placement(by_edge[Edge.TOP])
    assert dm.entry_point(top, 960, TABLET_W, TABLET_H) == (1599.5, 2135.0)
    assert dm.exit_position(top, 1599.5, TABLET_W, TABLET_H) == (960, 1)

    bottom = Placement(by_edge[Edge.BOTTOM])
    assert dm.entry_point(bottom, 960, TABLET_W, TABLET_H) == (1599.5, 0.0)
    assert dm.exit_position(bottom, 1599.5, TABLET_W, TABLET_H) == (960, 1078)


def test_exit_position_margin_and_inset_overrides():
    dm = make_map("single")
    right = Placement(segments_by_edge(dm)[Edge.RIGHT])
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H, entry_margin_px=1) == (
        1919,
        540,
    )
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H, entry_margin_px=0) == (
        1919,
        540,
    )
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H, entry_margin_px=-5) == (
        1919,
        540,
    )
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H, boundary_inset_px=8) == (
        1910,
        540,
    )
    left = Placement(segments_by_edge(dm)[Edge.LEFT])
    assert dm.exit_position(left, 1067.5, TABLET_W, TABLET_H, entry_margin_px=1) == (0, 540)
    assert dm.exit_position(left, 1067.5, TABLET_W, TABLET_H, boundary_inset_px=8) == (9, 540)


def test_entry_and_exit_on_merged_multi_monitor_segments():
    dm = make_map("dual_horizontal")
    by_edge = segments_by_edge(dm)

    top = Placement(by_edge[Edge.TOP])
    assert top.segment.span_start == 0 and top.segment.span_end == 3840
    assert dm.entry_point(top, 960, TABLET_W, TABLET_H) == (799.75, 2135.0)
    assert dm.exit_position(top, 799.75, TABLET_W, TABLET_H) == (960, 1)
    assert dm.exit_position(top, 3199, TABLET_W, TABLET_H) == (3839, 1)

    right = Placement(by_edge[Edge.RIGHT])
    assert right.segment.span_end == 1080
    assert dm.entry_point(right, 540, TABLET_W, TABLET_H) == (0.0, 1067.5)
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H) == (3838, 540)


def test_exit_position_negative_coordinates():
    dm = make_map("negative")
    by_edge = segments_by_edge(dm)
    right = Placement(by_edge[Edge.RIGHT])
    left = Placement(by_edge[Edge.LEFT])
    assert dm.exit_position(right, 1067.5, TABLET_W, TABLET_H) == (1918, 540)
    assert dm.exit_position(left, 1067.5, TABLET_W, TABLET_H) == (-1919, 540)


def test_exit_position_rejects_stale_monitor():
    dm = make_map("single")
    stale = Placement(EdgeSegment(99, Edge.RIGHT, 0, 1080))
    with pytest.raises(PadLendError) as excinfo:
        dm.exit_position(stale, 0, TABLET_W, TABLET_H)
    assert excinfo.value.code is ErrorCode.PLACEMENT_INVALID


def test_segment_exists_and_placement_valid():
    dm = make_map("single")
    seg = dm.exterior_segments()[0]
    assert dm.segment_exists(seg)
    assert dm.placement_valid(Placement(seg))
    stale = EdgeSegment(seg.monitor_handle, seg.edge, seg.span_start + 1, seg.span_end)
    assert not dm.segment_exists(stale)
    assert not dm.placement_valid(Placement(stale))
    assert not dm.segment_exists(EdgeSegment(99, Edge.LEFT, 0, 1))


# ---------------------------------------------------------------------------
# DPI / 单实例（依赖真实 win32，仅做安全性检查）
# ---------------------------------------------------------------------------
def test_dpi_helpers_are_safe_on_invalid_handles():
    pos = dpi.get_cursor_pos()
    assert isinstance(pos, tuple) and len(pos) == 2
    assert all(isinstance(value, int) for value in pos)
    assert dpi.get_monitor_dpi(0) == 96
    assert dpi.get_monitor_rect(0) is None
    assert isinstance(dpi.set_dpi_awareness(), bool)


@pytest.mark.skipif(sys.platform != "win32", reason="需要 Windows 互斥量")
def test_single_instance_acquire_release_is_idempotent():
    name = rf"Local\PadLend.Test.{uuid.uuid4().hex}"
    try:
        assert single_instance.already_running(name) is False
        assert single_instance.acquire(name) is True
        assert single_instance.acquire(name) is True
        assert single_instance.already_running(name) is False
    finally:
        single_instance.release()
    assert single_instance.already_running(name) is False
    single_instance.release()


@pytest.mark.skipif(sys.platform != "win32", reason="需要 Windows 互斥量")
def test_already_running_detects_external_mutex():
    name = rf"Local\PadLend.Other.{uuid.uuid4().hex}"
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_int32, ctypes.c_wchar_p]
    kernel32.CreateMutexW.restype = ctypes.c_void_p
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel32.CloseHandle.restype = ctypes.c_int32

    handle = kernel32.CreateMutexW(None, False, name)
    assert handle
    try:
        assert single_instance.already_running(name) is True
    finally:
        kernel32.CloseHandle(handle)
    assert single_instance.already_running(name) is False


@pytest.mark.skipif(sys.platform != "win32", reason="需要 Windows 命名事件")
def test_activation_event_wakes_watcher() -> None:
    name = rf"Local\PadLendTest.{uuid.uuid4().hex}"
    received = threading.Event()
    try:
        assert single_instance.create_activation_event(name) is True
        assert single_instance.start_activation_watcher(received.set, name) is True
        assert single_instance.signal_running_instance(name, timeout=1.0) is True
        assert received.wait(2.0) is True
    finally:
        single_instance.stop_activation_watcher()


@pytest.mark.skipif(sys.platform != "win32", reason="需要 Windows 命名事件")
def test_signal_missing_activation_event_returns_false() -> None:
    name = rf"Local\PadLendMissing.{uuid.uuid4().hex}"
    assert single_instance.signal_running_instance(name, timeout=0.15) is False


def test_constants_used_by_geometry():
    assert C.ENTRY_MARGIN_PX == 2
    assert C.BOUNDARY_INSET_PX == 0
    assert C.CORNER_DIRECTION_RATIO == 0.15


def test_desktop_cursor_helpers_delegate_to_dpi(monkeypatch):
    desktop = DesktopMap(
        monitors=[MonitorInfo(handle=1, device=r"\.\DISPLAY1", rect=Rect(0, 0, 1920, 1080), dpi=96, primary=True)]  # noqa: E501
    )
    calls: list[tuple[int, int]] = []

    monkeypatch.setattr(desktop_module, "try_get_cursor_pos", lambda: (321, 654))
    monkeypatch.setattr(desktop_module, "set_cursor_pos", lambda x, y: calls.append((x, y)) or True)

    assert desktop.cursor_pos() == (321, 654)
    assert desktop.set_cursor_pos(10, 20) is True
    assert calls == [(10, 20)]


def test_desktop_cursor_helpers_return_none_on_failure(monkeypatch):
    desktop = DesktopMap(
        monitors=[MonitorInfo(handle=1, device=r"\.\DISPLAY1", rect=Rect(0, 0, 1920, 1080), dpi=96, primary=True)]  # noqa: E501
    )
    monkeypatch.setattr(desktop_module, "try_get_cursor_pos", lambda: None)
    assert desktop.cursor_pos() is None
