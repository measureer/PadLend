"""虚拟桌面几何与多显示器外轮廓（设计文档 6.3 / 8.1 / 8.2 / 13）。

几何数据类定义在 :mod:`core.geometry`（避免 ``core`` 反向依赖 ``win``），
本模块按契约重新导出，保证 ``from win.desktop import Edge, Rect, DesktopMap``
等写法继续有效。

核心概念
--------
虚拟桌面坐标：主屏左上角为原点，单位物理像素，允许负值（13.3）。

外轮廓边段：平板只能吸附到整个虚拟桌面并集的**外轮廓**，不能吸附到显示器
之间的内缝（6.3.3 / 13.2）。算法对每台显示器的每条边，在边外侧 1px 的扫描线
上按 1px 单位找出"不被任何显示器占据"的连续区间（6.3.3），然后合并共线且
span 相接/重叠的段（6.3.4 的"虚拟段"）。

合并必须同时满足三个条件：``edge`` 相同、边线坐标相同（垂直边是 x，水平边
是 y）、span 相接或重叠。仅检查 edge+span 会把左右双屏中两块屏各自的 left
边（x 不同但 span 相同）错误地并成一条，落点会落在两屏之间。

``import win.desktop`` 不访问 win32；只有 :meth:`DesktopMap.refresh` 会枚举
显示器。测试可以用 ``DesktopMap(monitors=...)`` 注入假显示器，走纯几何路径。
"""

from __future__ import annotations

import ctypes
import logging
from collections.abc import Sequence
from ctypes import byref, c_int32, c_void_p
from functools import lru_cache
from typing import Any

import constants as C
from core.errors import ErrorCode, PadLendError
from core.geometry import Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect
from win.dpi import get_monitor_dpi, set_cursor_pos, try_get_cursor_pos

log = logging.getLogger("win.desktop")

__all__ = [
    "Align",
    "DesktopMap",
    "Edge",
    "EdgeSegment",
    "MonitorInfo",
    "Placement",
    "Rect",
    "compute_exterior_segments",
    "pc_to_tablet_along",
    "tablet_to_pc_along",
]

# 固定顺序，保证 exterior_segments() 的结果可复现（也是 GUI 绘制的顺序）。
_EDGES: tuple[Edge, ...] = (Edge.LEFT, Edge.RIGHT, Edge.TOP, Edge.BOTTOM)
_EDGE_INDEX: dict[Edge, int] = {edge: index for index, edge in enumerate(_EDGES)}


# ---------------------------------------------------------------------------
# 纯几何辅助
# ---------------------------------------------------------------------------
def _clamp(value: float, low: float, high: float) -> float:
    if value < low:
        return low
    if value > high:
        return high
    return value


def _clamp_int(value: int, low: int, high: int) -> int:
    if high < low:
        return low
    if value < low:
        return low
    if value > high:
        return high
    return value


def _edge_line(monitor: MonitorInfo, edge: Edge) -> int:
    """返回边线坐标：外侧 1px 扫描线的 x（垂直边）或 y（水平边）。"""
    rect = monitor.rect
    if edge is Edge.LEFT:
        return rect.left - 1
    if edge is Edge.RIGHT:
        return rect.right
    if edge is Edge.TOP:
        return rect.top - 1
    return rect.bottom


def _free_runs(
    monitor: MonitorInfo, edge: Edge, monitors: Sequence[MonitorInfo]
) -> list[tuple[int, int]]:
    """在 ``monitor`` 某条边外侧 1px 的扫描线上，返回未被占据的连续区间。"""
    rect = monitor.rect
    if rect.empty:
        return []

    if edge.is_vertical:
        line = _edge_line(monitor, edge)
        along_start, along_end = rect.top, rect.bottom
        vertical = True
    else:
        line = _edge_line(monitor, edge)
        along_start, along_end = rect.left, rect.right
        vertical = False

    runs: list[tuple[int, int]] = []
    run_start: int | None = None
    for along in range(along_start, along_end):
        blocked = False
        for other in monitors:
            hit = (
                other.rect.contains(line, along)
                if vertical
                else other.rect.contains(along, line)
            )
            if hit:
                blocked = True
                break
        if blocked:
            if run_start is not None:
                runs.append((run_start, along))
                run_start = None
        elif run_start is None:
            run_start = along
    if run_start is not None:
        runs.append((run_start, along_end))
    return runs


def compute_exterior_segments(monitors: Sequence[MonitorInfo]) -> tuple[EdgeSegment, ...]:
    """计算虚拟桌面外轮廓边段（纯函数，不依赖 win32）。

    对每台显示器每条边，在边外侧 1px 的扫描线上按 1px 找出未被任何显示器
    占据的连续区间；随后把 ``edge`` 相同、边线坐标相同且 span 相接/重叠的
    段合并成虚拟段（6.3.3 / 6.3.4）。合并后的段保留起始 monitor 的 handle
    （共线且相接，落点计算不受影响）。
    """
    # 记录 (edge 序号, 边线坐标, span_start, span_end, monitor_handle)
    records: list[tuple[int, int, int, int, int]] = []
    for monitor in monitors:
        for edge in _EDGES:
            line = _edge_line(monitor, edge)
            for span_start, span_end in _free_runs(monitor, edge, monitors):
                records.append(
                    (_EDGE_INDEX[edge], line, span_start, span_end, monitor.handle)
                )
    records.sort()

    merged: list[list[int]] = []
    for edge_index, line, span_start, span_end, handle in records:
        if (
            merged
            and merged[-1][0] == edge_index
            and merged[-1][1] == line
            and span_start <= merged[-1][3]
        ):
            merged[-1][3] = max(merged[-1][3], span_end)
            continue
        merged.append([edge_index, line, span_start, span_end, handle])

    return tuple(
        EdgeSegment(handle, _EDGES[edge_index], span_start, span_end)
        for edge_index, _line, span_start, span_end, handle in merged
    )


def _bounding_box(monitors: Sequence[MonitorInfo]) -> Rect:
    if not monitors:
        return Rect(0, 0, 0, 0)
    return Rect(
        min(monitor.rect.left for monitor in monitors),
        min(monitor.rect.top for monitor in monitors),
        max(monitor.rect.right for monitor in monitors),
        max(monitor.rect.bottom for monitor in monitors),
    )


def pc_to_tablet_along(pc_along: int, seg: EdgeSegment, t_size: int) -> float:
    """PC 沿边坐标 → 平板沿边坐标（6.3.5，返回 ``0 .. t_size-1`` 的浮点值）。"""
    t = (pc_along - seg.span_start) / max(seg.length, 1)
    return _clamp(t, 0.0, 1.0) * max(t_size - 1, 0)


def tablet_to_pc_along(t_along: float, seg: EdgeSegment, t_size: int) -> int:
    """平板沿边坐标 → PC 沿边坐标（6.3.5，返回虚拟桌面整数坐标）。"""
    t = _clamp(t_along / max(t_size - 1, 1), 0.0, 1.0)
    return int(round(seg.span_start + t * max(seg.length - 1, 0)))


# ---------------------------------------------------------------------------
# Win32 显示器枚举
# ---------------------------------------------------------------------------
class _RECT(ctypes.Structure):
    _fields_ = [
        ("left", c_int32),
        ("top", c_int32),
        ("right", c_int32),
        ("bottom", c_int32),
    ]


class _MONITORINFOEXW(ctypes.Structure):
    _fields_ = [
        ("cbSize", c_int32),
        ("rcMonitor", _RECT),
        ("rcWork", _RECT),
        ("dwFlags", c_int32),
        ("szDevice", ctypes.c_wchar * 32),
    ]


MONITORINFOF_PRIMARY = 0x00000001


@lru_cache(maxsize=1)
def _user32() -> Any:
    """延迟加载 ``user32.dll``；失败抛 ``E900``（枚举是几何的必要条件）。"""
    try:
        return ctypes.WinDLL("user32", use_last_error=True)
    except (AttributeError, OSError) as exc:
        raise PadLendError(
            ErrorCode.UNEXPECTED, f"无法加载 user32.dll: {exc}", cause=exc
        ) from exc


def _enum_display_monitors() -> tuple[MonitorInfo, ...]:
    """``EnumDisplayMonitors`` + ``GetMonitorInfoW`` + ``GetDpiForMonitor``。"""
    user32 = _user32()
    monitors: list[MonitorInfo] = []
    failures: list[str] = []

    # WINFUNCTYPE 只在 Windows 存在；放到函数内，保证 import 无平台副作用。
    monitor_enum_proc = ctypes.WINFUNCTYPE(c_int32, c_void_p, c_void_p, c_void_p, c_void_p)

    user32.EnumDisplayMonitors.argtypes = [c_void_p, c_void_p, monitor_enum_proc, c_void_p]
    user32.EnumDisplayMonitors.restype = c_int32
    user32.GetMonitorInfoW.argtypes = [c_void_p, c_void_p]
    user32.GetMonitorInfoW.restype = c_int32

    def _on_monitor(hmon: int, _hdc: int, _rect: int, _data: int) -> int:
        try:
            info = _MONITORINFOEXW()
            info.cbSize = ctypes.sizeof(_MONITORINFOEXW)
            if not user32.GetMonitorInfoW(hmon, byref(info)):
                failures.append(
                    f"GetMonitorInfoW(0x{int(hmon or 0):X}) 失败: {ctypes.get_last_error()}"
                )
                return 1
            rc = info.rcMonitor
            handle = int(hmon or 0)
            monitors.append(
                MonitorInfo(
                    handle=handle,
                    device=str(info.szDevice),
                    rect=Rect(int(rc.left), int(rc.top), int(rc.right), int(rc.bottom)),
                    dpi=get_monitor_dpi(handle),
                    primary=bool(info.dwFlags & MONITORINFOF_PRIMARY),
                )
            )
        except Exception as exc:  # ctypes 回调不能向外抛异常
            failures.append(f"枚举显示器失败: {type(exc).__name__}: {exc}")
        return 1

    callback = monitor_enum_proc(_on_monitor)
    ctypes.set_last_error(0)
    ok = user32.EnumDisplayMonitors(None, None, callback, 0)
    last_error = ctypes.get_last_error()
    if not ok and (monitors or last_error):
        raise PadLendError(
            ErrorCode.UNEXPECTED,
            f"EnumDisplayMonitors 失败 (GetLastError={last_error})",
        )
    if failures:
        raise PadLendError(ErrorCode.UNEXPECTED, "; ".join(failures[:3]))
    if not monitors:
        log.warning("未枚举到任何显示器，虚拟桌面为空")
        return ()

    # 主屏优先，其余按位置排序，保证 GUI/Router 看到稳定顺序。
    monitors.sort(key=lambda m: (not m.primary, m.rect.left, m.rect.top, m.device))
    return tuple(monitors)


# ---------------------------------------------------------------------------
# DesktopMap
# ---------------------------------------------------------------------------
class DesktopMap:
    """虚拟桌面几何快照（6.3）。

    ``DesktopMap()`` 立即枚举真实显示器；``DesktopMap(monitors=...)`` 注入
    已知快照（单元测试/诊断用），不触碰 win32。刷新是整体替换：构造新的
    不可变元组后一次性赋值，读线程不会看到半更新的组合。
    """

    def __init__(self, monitors: Sequence[MonitorInfo] | None = None) -> None:
        self._state: tuple[tuple[MonitorInfo, ...], tuple[EdgeSegment, ...], Rect] = (
            (),
            (),
            Rect(0, 0, 0, 0),
        )
        if monitors is None:
            self.refresh()
        else:
            self._install(monitors)

    # -- 快照 ---------------------------------------------------------------
    def _install(self, monitors: Sequence[MonitorInfo]) -> None:
        snapshot = tuple(monitors)
        segments = compute_exterior_segments(snapshot)
        self._state = (snapshot, segments, _bounding_box(snapshot))

    def refresh(self) -> None:
        """重新枚举显示器并重建外轮廓（13.4）。

        失败抛 :class:`~core.errors.PadLendError`（``E900``），并保留上一次
        成功的快照，避免一次瞬时失败把可用几何清空。
        """
        self._install(_enum_display_monitors())

    @property
    def monitors(self) -> tuple[MonitorInfo, ...]:
        return self._state[0]

    @property
    def bounding_box(self) -> Rect:
        """所有显示器并集的包围盒；无显示器时为 ``Rect(0, 0, 0, 0)``。"""
        return self._state[2]

    def cursor_pos(self) -> tuple[int, int] | None:
        """当前系统光标的物理像素坐标；查询失败返回 ``None``。

        Router 用它把 ``RawDelta`` 换算成绝对光标位置。若这里缺失，Router
        只能把原始鼠标计数当像素累加；在开启指针加速时两者会迅速漂移，
        导致边缘判定落在错误位置。``None`` 表示本次查询失败，Router 应保持
        上一次已知位置而不是自行累计。
        """
        return try_get_cursor_pos()

    def set_cursor_pos(self, x: int, y: int) -> bool:
        """把系统光标移到物理像素坐标；成功返回 ``True``。"""
        return set_cursor_pos(int(x), int(y))

    def exterior_segments(self) -> tuple[EdgeSegment, ...]:
        """当前外轮廓段（不可变元组，按 edge/span 稳定排序）。"""
        return self._state[1]

    # -- 查询 ---------------------------------------------------------------
    def _monitor_by_handle(
        self,
        handle: int,
        monitors: tuple[MonitorInfo, ...] | None = None,
    ) -> MonitorInfo | None:
        for monitor in self._state[0] if monitors is None else monitors:
            if monitor.handle == handle:
                return monitor
        return None

    def monitor_at(self, x: int, y: int) -> MonitorInfo | None:
        for monitor in self._state[0]:
            if monitor.rect.contains(x, y):
                return monitor
        return None

    def segments_at(self, x: int, y: int, band: int = 1) -> tuple[EdgeSegment, ...]:
        """返回光标 ``(x, y)`` 在 ``band`` 像素内可命中的外轮廓段。

        垂直边比较 x 与边线（left 边线 = ``left-1``，right 边线 = ``right``）；
        水平边比较 y。沿边方向必须落在 ``[span_start, span_end)`` 内，因此
        屏幕角落会同时命中两条边，交给 :meth:`resolve_corner` 裁决。
        """
        band = max(0, int(band))
        monitors, segments, _ = self._state
        found: list[EdgeSegment] = []
        for seg in segments:
            monitor = self._monitor_by_handle(seg.monitor_handle, monitors)
            if monitor is None:
                continue
            line = _edge_line(monitor, seg.edge)
            if seg.edge.is_vertical:
                if abs(x - line) > band or not seg.contains_along(y):
                    continue
            elif abs(y - line) > band or not seg.contains_along(x):
                continue
            found.append(seg)
        return tuple(found)

    def segment_exists(self, seg: EdgeSegment) -> bool:
        """``refresh`` 后校验旧 ``Placement.segment`` 是否仍然存在（13.4）。"""
        return seg in self._state[1]

    def placement_valid(self, pl: Placement) -> bool:
        return self.segment_exists(pl.segment)

    # -- 角落裁决 -----------------------------------------------------------
    def resolve_corner(
        self,
        segs: tuple[EdgeSegment, ...],
        delta: tuple[int, int],
        priority: Edge | None,
    ) -> EdgeSegment | None:
        """按 6.3.6 裁决角落同时命中的两条边。

        方向足够明确（最高分 > 0 且比次高分高 15% 以上）时取最高分；否则取
        ``priority`` 指定的边，没有偏好则取最高分（分数相同保持输入顺序）。
        """
        if not segs:
            return None
        if len(segs) == 1:
            return segs[0]

        dx, dy = delta
        scored: list[tuple[EdgeSegment, float]] = []
        for seg in segs:
            nx, ny = seg.edge.outward_normal()
            scored.append((seg, float(dx * nx + dy * ny)))
        scored.sort(key=lambda pair: -pair[1])

        (first, first_score), (_second, second_score) = scored[0], scored[1]
        if (
            first_score > 0
            and (first_score - second_score) / max(first_score, 1) > C.CORNER_DIRECTION_RATIO
        ):
            return first
        if priority is not None:
            for seg, _score in scored:
                if seg.edge == priority:
                    return seg
        return first

    # -- 沿边映射 -----------------------------------------------------------
    @staticmethod
    def _tablet_along_size(edge: Edge, tablet_w: int, tablet_h: int) -> int:
        """平板贴边那条边的长度：垂直 PC 边用平板高，水平 PC 边用平板宽。"""
        return tablet_h if edge.is_vertical else tablet_w

    def entry_point(
        self,
        pl: Placement,
        pc_along: int,
        tablet_w: int,
        tablet_h: int,
    ) -> tuple[float, float]:
        """PC 侧沿边坐标 → 平板入口点（t4/t5 的 ``P0``，6.3.5）。

        平板贴边方向（用户给定）：PC 右边 → 平板左缘 ``x=0``；PC 左边 →
        平板右缘 ``x=W-1``；PC 上边 → 平板下缘 ``y=H-1``；PC 下边 →
        平板上缘 ``y=0``。返回平板像素坐标（浮点，供 ``warp_to`` 使用）。
        """
        seg = pl.segment
        t_size = self._tablet_along_size(seg.edge, tablet_w, tablet_h)
        along = pc_to_tablet_along(pc_along, seg, t_size)
        if seg.edge is Edge.RIGHT:
            return (0.0, along)
        if seg.edge is Edge.LEFT:
            return (float(max(tablet_w - 1, 0)), along)
        if seg.edge is Edge.TOP:
            return (along, float(max(tablet_h - 1, 0)))
        return (along, 0.0)

    def exit_position(
        self,
        pl: Placement,
        tablet_along: float,
        tablet_w: int,
        tablet_h: int,
        *,
        entry_margin_px: int | None = None,
        boundary_inset_px: int | None = None,
    ) -> tuple[int, int]:
        """平板侧沿边坐标 → PC 虚拟桌面落点（t9 的 ``SetCursorPos``，6.3.5）。

        ``entry_margin_px`` / ``boundary_inset_px`` 缺省取
        ``constants.ENTRY_MARGIN_PX`` / ``constants.BOUNDARY_INSET_PX``。
        落点距边线 ``max(1, entry_margin) + boundary_inset`` 像素，位于桌面
        **内部**（Windows 不允许把光标留在所有显示器之外，1px 是边界像
        素）；例如默认值在 right 边得到 ``x=right-2``，left 边得到
        ``x=left+1``。落点还会夹到显示器与段范围内，避免极端布局越界。
        """
        seg = pl.segment
        monitors, segments, _ = self._state
        if seg not in segments:
            raise PadLendError(
                ErrorCode.PLACEMENT_INVALID,
                f"段 {seg.edge.value} [{seg.span_start}, {seg.span_end}) 已不存在",
            )
        monitor = self._monitor_by_handle(seg.monitor_handle, monitors)
        if monitor is None:
            raise PadLendError(
                ErrorCode.PLACEMENT_INVALID,
                f"段引用的显示器 handle=0x{seg.monitor_handle:X} 已不存在",
            )
        if monitor.rect.empty:
            raise PadLendError(
                ErrorCode.PLACEMENT_INVALID,
                f"显示器 {monitor.device or monitor.handle} 的矩形为空",
            )

        t_size = self._tablet_along_size(seg.edge, tablet_w, tablet_h)
        along = _clamp_int(
            tablet_to_pc_along(tablet_along, seg, t_size),
            seg.span_start,
            max(seg.span_start, seg.span_end - 1),
        )

        margin = C.ENTRY_MARGIN_PX if entry_margin_px is None else int(entry_margin_px)
        inset = C.BOUNDARY_INSET_PX if boundary_inset_px is None else int(boundary_inset_px)
        offset = max(1, margin) + max(0, inset)

        rect = monitor.rect
        if seg.edge is Edge.RIGHT:
            x = _clamp_int(rect.right - offset, rect.left, rect.right - 1)
            y = along
        elif seg.edge is Edge.LEFT:
            x = _clamp_int(rect.left + offset - 1, rect.left, rect.right - 1)
            y = along
        elif seg.edge is Edge.TOP:
            x = along
            y = _clamp_int(rect.top + offset - 1, rect.top, rect.bottom - 1)
        else:
            x = along
            y = _clamp_int(rect.bottom - offset, rect.top, rect.bottom - 1)
        return (int(x), int(y))
