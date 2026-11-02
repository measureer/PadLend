"""Router / Metrics 单测用的测试替身（仅测试代码，不属于产品模块）。

这些 fake 按 8.2 的**结构化契约**实现 ``session`` / ``desktop`` / ``mask``，
因此 Router 可以完全在无 Windows、无 ADB、无真机的环境下被驱动：

- :class:`FakeSession`：记录 ``send_mouse`` / ``send_key``，可注入健康状态、
  写失败、启动失败与重连失败。
- :class:`FakeDesktop`：真实实现 6.3.5 的 ``entry_point`` / ``exit_position``
  与 6.3.6 的 ``resolve_corner``，并允许测试直接摆放 PC 光标。
- :class:`FakeMask`：记录 ``show`` / ``hide`` / ``release`` 调用与最后一次裁剪矩形。
- :class:`FakeKeymap`：返回固定 HID 报告，记录 ``reset``。
- :class:`FakeClock`：可推进的单调时钟（Router 构造参数 ``clock``）。
- :class:`FakeConfig`：基于 ``config`` 的真实配置段，便于按用例改阈值。
"""

from __future__ import annotations

import threading
from collections.abc import Iterable, Sequence

import constants as C
from config import (
    DeviceCfg,
    HotkeyCfg,
    KeyboardCfg,
    MiscCfg,
    MotionCfg,
    PlacementCfg,
    UiCfg,
)
from core.errors import ErrorCode, PadLendError
from core.events import KeyEvent, RawDelta
from core.geometry import Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect
from tablet.session import DeviceFacts, HealthReport

_PRIMARY = MonitorInfo(
    handle=1,
    device="\\\\.\\DISPLAY1",
    rect=Rect(0, 0, 1920, 1080),
    dpi=96,
    primary=True,
)


def default_facts(
    width: int = 1600,
    height: int = 2560,
    *,
    serial: str = "FAKE0001",
    orientation: int = 0,
    xscale: float = 1.0,
    pointer_speed: int = 0,
) -> DeviceFacts:
    """构造一份默认的平板设备信息（竖屏 1600×2560）。"""
    return DeviceFacts(
        serial=serial,
        model="FakePad",
        width=width,
        height=height,
        orientation=orientation,
        xscale=xscale,
        pointer_speed=pointer_speed,
    )


class FakeClock:
    """可推进的单调时钟；``Router(clock=...)`` 与 ``Metrics(clock=...)`` 共用。"""

    def __init__(self, now: float = 1000.0) -> None:
        self.now = float(now)

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> float:
        self.now += float(seconds)
        return self.now

    def advance_ms(self, milliseconds: float) -> float:
        return self.advance(float(milliseconds) / 1000.0)


class FakeConfig:
    """真实配置段的可调版本；避免每个用例都写一长串 dataclass 参数。"""

    def __init__(
        self,
        *,
        arm_px: int = C.ARM_PX,
        arm_window_ms: int = C.ARM_WINDOW_MS,
        exit_overflow_px: int = C.EXIT_OVERFLOW_PX,
        entry_guard_px: int = 0,
        edge_band_px: int = C.EDGE_BAND_PX,
        debounce_ms: int = C.DEBOUNCE_MS,
        gain: float = C.DEFAULT_GAIN,
        report_hz: int = C.DEFAULT_REPORT_HZ,
        hid_clamp: int = C.HID_CLAMP,
        forward: bool = True,
        suppress_pc: bool = True,
        passthrough_keys: Iterable[str] = ("VK_LWIN", "VK_RWIN"),
        restore_pointer_speed: int = -3,
        corner_priority: str = "right",
    ) -> None:
        self.device = DeviceCfg()
        self.placement = PlacementCfg(corner_priority=corner_priority)
        self.motion = MotionCfg(
            gain=gain,
            report_hz=report_hz,
            hid_clamp=hid_clamp,
            exit_overflow_px=exit_overflow_px,
            entry_guard_px=entry_guard_px,
            debounce_ms=debounce_ms,
            arm_px=arm_px,
            arm_window_ms=arm_window_ms,
            edge_band_px=edge_band_px,
        )
        self.keyboard = KeyboardCfg(
            forward=forward,
            suppress_pc=suppress_pc,
            passthrough_keys=list(passthrough_keys),
        )
        self.hotkeys = HotkeyCfg()
        self.ui = UiCfg()
        self.misc = MiscCfg(restore_pointer_speed=restore_pointer_speed)


class FakeSession:
    """``TabletSession`` 的结构化替身（8.2）。"""

    def __init__(
        self,
        facts: DeviceFacts | None = None,
        *,
        alive: bool = True,
        socket_ok: bool = True,
        start_error: BaseException | None = None,
        reconnect_error: BaseException | None = None,
        has_facts: bool = True,
        start_gate: threading.Event | None = None,
    ) -> None:
        if facts is not None:
            self._facts: DeviceFacts | None = facts
        elif has_facts:
            self._facts = default_facts()
        else:
            self._facts = None
        self.alive = bool(alive)
        self.socket_ok = bool(socket_ok)
        self.start_error = start_error
        self.reconnect_error = reconnect_error
        self.start_gate = start_gate
        self.mouse_reports: list[tuple[int, int, int, int]] = []
        self.key_reports: list[tuple[int, tuple[int, ...]]] = []
        self.cursor_sets: list[tuple[int, int]] = []
        self.pointer_speed_sets: list[int] = []
        self.start_calls = 0
        self.stop_calls = 0
        self.reconnect_calls = 0
        self.health_calls = 0
        self.ensure_pointer_speed_calls = 0
        self.cancel_calls = 0
        self.fail_sends = 0

    # -- DeviceFacts / 生命周期 --------------------------------------------
    @property
    def facts(self) -> DeviceFacts | None:
        return self._facts

    def start(self) -> DeviceFacts:
        self.start_calls += 1
        if self.start_gate is not None:
            self.start_gate.wait(timeout=5.0)
        if self.start_error is not None:
            error = self.start_error
            self.start_error = None
            raise error
        if self._facts is None:
            self._facts = default_facts()
        self.alive = True
        self.socket_ok = True
        return self._facts

    def stop(self) -> None:
        self.stop_calls += 1
        self.alive = False

    def cancel_start(self) -> None:
        """模拟 TabletSession.cancel_start()：中止首连等待。"""
        self.cancel_calls += 1
        if self.start_gate is not None:
            self.start_gate.set()

    def reconnect(self) -> DeviceFacts | None:
        self.reconnect_calls += 1
        if self.reconnect_error is not None:
            raise self.reconnect_error
        self.alive = True
        self.socket_ok = True
        return self._facts

    # -- 发送 --------------------------------------------------------------
    def send_mouse(self, buttons: int, dx: int, dy: int, wheel: int = 0) -> None:
        if self.fail_sends:
            self.fail_sends -= 1
            raise PadLendError(ErrorCode.SOCKET_BROKEN, "fake write failure")
        self.mouse_reports.append((int(buttons), int(dx), int(dy), int(wheel)))

    def send_key(self, modifier: int, usages: Iterable[int]) -> None:
        self.key_reports.append((int(modifier), tuple(int(usage) for usage in usages)))

    def send_keycode(self, vk: int) -> None:
        self.send_key(0, (int(vk),))

    def inject_text(self, text: str) -> None:
        return None

    def health(self) -> HealthReport:
        self.health_calls += 1
        return HealthReport(
            alive=self.alive,
            socket_ok=self.socket_ok,
            uhid_created=True,
            server_alive=True,
            consecutive_failures=0,
            last_error="",
            uptime_s=1.0,
            sent=len(self.mouse_reports),
        )

    # -- 桌面指针 / 指针速度 ----------------------------------------------
    def set_cursor_pos(self, x: int, y: int) -> None:
        self.cursor_sets.append((int(x), int(y)))

    def ensure_pointer_speed(self) -> None:
        self.ensure_pointer_speed_calls += 1

    def set_pointer_speed(self, speed: int) -> None:
        self.pointer_speed_sets.append(int(speed))

    # -- 断言辅助 ----------------------------------------------------------
    @property
    def displacements(self) -> list[tuple[int, int]]:
        return [(dx, dy) for _buttons, dx, dy, _wheel in self.mouse_reports]

    def clear(self) -> None:
        self.mouse_reports.clear()
        self.key_reports.clear()


class FakeDesktop:
    """``DesktopMap`` 的结构化替身（8.2 + 6.3.5 / 6.3.6）。"""

    def __init__(
        self,
        monitors: Sequence[MonitorInfo] | None = None,
        *,
        edges: Iterable[Edge] = (Edge.RIGHT,),
        segments: Sequence[EdgeSegment] | None = None,
    ) -> None:
        self._monitors: tuple[MonitorInfo, ...] = tuple(monitors or (_PRIMARY,))
        self.cursor: tuple[int, int] = (960, 540)
        self.segments: tuple[EdgeSegment, ...] = tuple(
            segments if segments is not None else self._segments_for(edges)
        )
        self.refresh_calls = 0
        self.corner_calls: list[tuple[tuple[int, int], Edge | None]] = []
        self.cursor_sets: list[tuple[int, int]] = []

    # -- 基础属性 ----------------------------------------------------------
    @property
    def monitors(self) -> tuple[MonitorInfo, ...]:
        return self._monitors

    @property
    def bounding_box(self) -> Rect:
        boxes = [monitor.rect for monitor in self._monitors]
        if not boxes:
            return Rect(0, 0, 0, 0)
        box = boxes[0]
        for other in boxes[1:]:
            box = box.union(other)
        return box

    def refresh(self) -> None:
        self.refresh_calls += 1

    def set_monitors(self, monitors: Sequence[MonitorInfo]) -> None:
        """模拟显示器热插拔；段列表由测试通过 ``set_segments`` 显式更新。"""
        self._monitors = tuple(monitors)

    def set_segments(self, segments: Sequence[EdgeSegment]) -> None:
        self.segments = tuple(segments)

    def monitor_at(self, x: int, y: int) -> MonitorInfo | None:
        for monitor in self._monitors:
            if monitor.rect.contains(int(x), int(y)):
                return monitor
        return None

    def monitor_by_handle(self, handle: int) -> MonitorInfo | None:
        for monitor in self._monitors:
            if monitor.handle == handle:
                return monitor
        return None

    # -- 外轮廓 ------------------------------------------------------------
    def _segments_for(self, edges: Iterable[Edge]) -> list[EdgeSegment]:
        out: list[EdgeSegment] = []
        for monitor in self._monitors:
            rect = monitor.rect
            for edge in edges:
                if edge is Edge.RIGHT or edge is Edge.LEFT:
                    span = (rect.top, rect.bottom)
                else:
                    span = (rect.left, rect.right)
                out.append(EdgeSegment(monitor.handle, edge, span[0], span[1]))
        return out

    def exterior_segments(self) -> tuple[EdgeSegment, ...]:
        return self.segments

    def segment_exists(self, segment: EdgeSegment) -> bool:
        return segment in self.segments

    def placement_valid(self, placement: Placement) -> bool:
        return self.segment_exists(placement.segment)

    def segments_at(self, x: int, y: int, band: int = 1) -> tuple[EdgeSegment, ...]:
        band = max(0, int(band))
        x, y = int(x), int(y)
        out: list[EdgeSegment] = []
        for segment in self.segments:
            monitor = self.monitor_by_handle(segment.monitor_handle)
            if monitor is None:
                continue
            rect = monitor.rect
            if segment.edge is Edge.RIGHT:
                line, coord, along = rect.right, x, y
            elif segment.edge is Edge.LEFT:
                line, coord, along = rect.left - 1, x, y
            elif segment.edge is Edge.TOP:
                line, coord, along = rect.top - 1, y, x
            else:
                line, coord, along = rect.bottom, y, x
            if line - band <= coord <= line + band and segment.contains_along(along):
                out.append(segment)
        return tuple(out)

    def resolve_corner(
        self,
        segments: Sequence[EdgeSegment],
        delta: tuple[int, int],
        priority: Edge | None,
    ) -> EdgeSegment | None:
        """6.3.6：方向分数 + 15% 优势阈值 + 用户优先边。"""
        self.corner_calls.append((delta, priority))
        items = list(segments)
        if not items:
            return None
        if len(items) == 1:
            return items[0]
        scored = [
            (
                delta[0] * segment.edge.outward_normal()[0]
                + delta[1] * segment.edge.outward_normal()[1],
                segment,
            )
            for segment in items
        ]
        scored.sort(key=lambda pair: pair[0], reverse=True)
        top_score, top = scored[0]
        second_score = scored[1][0]
        if (
            top_score > 0
            and (top_score - second_score) / max(top_score, 1) > C.CORNER_DIRECTION_RATIO
        ):
            return top
        if priority is not None:
            for _score, segment in scored:
                if segment.edge is priority:
                    return segment
        return top

    # -- 6.3.5 映射 --------------------------------------------------------
    @staticmethod
    def _along(pc_along: float, segment: EdgeSegment, tablet_size: int) -> float:
        span = max(segment.length, 1)
        norm = (float(pc_along) - segment.span_start) / span
        norm = min(max(norm, 0.0), 1.0)
        return norm * (tablet_size - 1)

    def entry_point(
        self,
        placement: Placement,
        pc_along: int,
        tablet_w: int,
        tablet_h: int,
    ) -> tuple[float, float]:
        segment = placement.segment
        edge = segment.edge
        if edge is Edge.RIGHT:
            return (0.0, self._along(pc_along, segment, tablet_h))
        if edge is Edge.LEFT:
            return (float(tablet_w - 1), self._along(pc_along, segment, tablet_h))
        if edge is Edge.TOP:
            return (self._along(pc_along, segment, tablet_w), float(tablet_h - 1))
        return (self._along(pc_along, segment, tablet_w), 0.0)

    def exit_position(
        self,
        placement: Placement,
        tablet_along: float,
        tablet_w: int,
        tablet_h: int,
        *,
        entry_margin_px: int | None = None,
        boundary_inset_px: int | None = None,
    ) -> tuple[int, int]:
        segment = placement.segment
        monitor = self.monitor_by_handle(segment.monitor_handle) or self._monitors[0]
        rect = monitor.rect
        margin = C.ENTRY_MARGIN_PX if entry_margin_px is None else int(entry_margin_px)
        inset = C.BOUNDARY_INSET_PX if boundary_inset_px is None else int(boundary_inset_px)
        if segment.edge.is_vertical:
            norm = float(tablet_along) / max(tablet_h - 1, 1)
            along = segment.span_start + min(max(norm, 0.0), 1.0) * max(segment.length, 1)
            y = min(max(along, rect.top + inset), rect.bottom - 1 - margin)
            x = rect.right - 1 - margin if segment.edge is Edge.RIGHT else rect.left + inset
        else:
            norm = float(tablet_along) / max(tablet_w - 1, 1)
            along = segment.span_start + min(max(norm, 0.0), 1.0) * max(segment.length, 1)
            x = min(max(along, rect.left + inset), rect.right - 1 - margin)
            y = rect.top + inset if segment.edge is Edge.TOP else rect.bottom - 1 - margin
        return int(round(x)), int(round(y))

    # -- PC 光标 -----------------------------------------------------------
    def cursor_pos(self) -> tuple[int, int]:
        return self.cursor

    def set_cursor_pos(self, x: int, y: int) -> None:
        self.cursor = (int(x), int(y))
        self.cursor_sets.append(self.cursor)


class FakeMask:
    """``CursorMask`` 的替身；记录 show/hide/release 与最后一次裁剪矩形。"""

    def __init__(self) -> None:
        self.shown: list[Rect | None] = []
        self.hide_calls = 0
        self.release_calls = 0
        self.active = False
        self.clip_rect: Rect | None = None
        self.fail_show = False

    def show(self, clip_rect: Rect | None = None) -> None:
        if self.fail_show:
            raise RuntimeError("fake mask.show failure")
        self.shown.append(clip_rect)
        self.active = True
        self.clip_rect = clip_rect

    def hide(self) -> None:
        self.hide_calls += 1
        self.active = False
        self.clip_rect = None

    def release(self) -> None:
        self.release_calls += 1
        self.active = False
        self.clip_rect = None


class FakeKeymap:
    """``tablet.keymap.Keymap`` 的替身：固定报告 + 记录 reset。"""

    def __init__(self, report: tuple[int, Sequence[int]] = (0, (4,))) -> None:
        self.report = report
        self.events: list[KeyEvent] = []
        self.reset_calls = 0
        self.passthrough: set[int] = set()

    def on_key(self, ev: KeyEvent) -> tuple[int, list[int]]:
        self.events.append(ev)
        modifier, usages = self.report
        return int(modifier), [int(usage) for usage in usages]

    def reset(self) -> None:
        self.reset_calls += 1

    def is_passthrough(self, ev: KeyEvent) -> bool:
        return ev.vk in self.passthrough


# ---------------------------------------------------------------------------
# 事件 / 几何辅助
# ---------------------------------------------------------------------------
def raw(dx: int = 0, dy: int = 0, buttons: int = 0, wheel: int = 0) -> RawDelta:
    """构造一条 RawInput 位移事件。"""
    return RawDelta(int(dx), int(dy), int(buttons), int(wheel), False, 0.0)


def key(vk: int, down: bool = True, scan: int = 0, extended: bool = False) -> KeyEvent:
    """构造一条键盘事件。"""
    return KeyEvent(int(vk), int(scan), bool(extended), bool(down), 0.0)


def make_placement(
    edge: Edge = Edge.RIGHT,
    *,
    handle: int = 1,
    span: tuple[int, int] = (0, 1080),
    offset: float = 0.5,
    align: Align = Align.PROPORTIONAL,
    corner_priority: Edge | None = None,
) -> Placement:
    """构造一个默认吸附在 1920×1080 主屏右边、纵跨全高的摆位。"""
    return Placement(
        segment=EdgeSegment(handle, edge, span[0], span[1]),
        offset=offset,
        align=align,
        corner_priority=corner_priority,
    )
