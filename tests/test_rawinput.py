"""``win/rawinput.py`` 单测：包解释、字节解码、线程生命周期。

纯逻辑部分直接在字节 / 数据类上跑，不需要窗口或真实设备；线程部分注入
``FakeBackend``。真实 Raw Input 只在 ``PADLEND_RAWINPUT_HW=1`` 时启动
（否则会抢走桌面鼠标事件）。
"""

from __future__ import annotations

import ctypes
import os
import sys
import threading
import time
from collections.abc import Callable

import pytest

import constants
from core.errors import ErrorCode, PadLendError
from core.events import RawDelta
from core.geometry import Rect
from win import rawinput as ri

_RUN_HW_TESTS = os.environ.get("PADLEND_RAWINPUT_HW") == "1"


def packet(
    *,
    flags: int = 0,
    x: int = 0,
    y: int = 0,
    button_flags: int = 0,
    button_data: int = 0,
    device: int = 0,
    ts: float = 0.0,
) -> ri.MousePacket:
    return ri.MousePacket(
        flags=flags,
        x=x,
        y=y,
        button_flags=button_flags,
        button_data=button_data,
        device=device,
        ts=ts,
    )


def wait_until(predicate: Callable[[], bool], timeout: float = 3.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError("条件未在超时前满足")


class Collector:
    """线程安全地收集回调事件（回调只允许入队，测试里用列表模拟）。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._deltas: list[RawDelta] = []

    def __call__(self, delta: RawDelta) -> None:
        with self._lock:
            self._deltas.append(delta)

    def snapshot(self) -> list[RawDelta]:
        with self._lock:
            return list(self._deltas)

    def count(self) -> int:
        with self._lock:
            return len(self._deltas)


class FakeBackend:
    """假后端：``pump`` 阻塞到 ``request_stop``，测试用 ``push`` 注入包。"""

    def __init__(self) -> None:
        self.prepared = 0
        self.closed = 0
        self.pump_calls = 0
        self.stop_requests = 0
        self.prepare_error: BaseException | None = None
        self.pump_error: BaseException | None = None
        self._stop = threading.Event()
        self._wake = threading.Event()
        self._lock = threading.Lock()
        self._pending: list[ri.MousePacket] = []

    def push(self, item: ri.MousePacket) -> None:
        with self._lock:
            self._pending.append(item)
        self._wake.set()

    # -- RawInputBackend --------------------------------------------------
    def prepare(self) -> None:
        self.prepared += 1
        if self.prepare_error is not None:
            raise self.prepare_error

    def pump(
        self,
        on_packet: Callable[[ri.MousePacket], None],
        on_wake: Callable[[], None] | None = None,
    ) -> None:
        self.pump_calls += 1
        if self.pump_error is not None:
            raise self.pump_error
        while not self._stop.is_set():
            self._wake.wait(0.05)
            self._wake.clear()
            with self._lock:
                batch, self._pending = self._pending, []
            for item in batch:
                on_packet(item)
            if on_wake is not None:
                on_wake()

    def wake(self) -> bool:
        self._wake.set()
        return True

    def request_stop(self) -> bool:
        self.stop_requests += 1
        self._stop.set()
        self._wake.set()
        return True

    def close(self) -> None:
        self.closed += 1


# ---------------------------------------------------------------------------
# 纯逻辑：相对包
# ---------------------------------------------------------------------------
def test_interpret_relative_packet() -> None:
    state = ri.PointerState()
    delta = ri.interpret(packet(x=12, y=-7, ts=3.5), state)

    assert delta == RawDelta(dx=12, dy=-7, buttons=0, wheel=0, absolute=False, ts=3.5)


def test_interpret_empty_packet_returns_none() -> None:
    state = ri.PointerState()
    assert ri.interpret(packet(), state) is None


def test_button_down_then_up_updates_bitmap() -> None:
    state = ri.PointerState()

    down = ri.interpret(packet(button_flags=0x0001), state)
    assert down is not None
    assert down.buttons == constants.MOUSE_BTN_LEFT

    up = ri.interpret(packet(button_flags=0x0002), state)
    assert up is not None
    assert up.buttons == 0


def test_button_state_persists_across_move_packets() -> None:
    state = ri.PointerState()

    press = ri.interpret(packet(button_flags=0x0004), state)  # 右键按下
    assert press is not None
    assert press.buttons == constants.MOUSE_BTN_RIGHT

    move = ri.interpret(packet(x=5, y=6), state)
    assert move is not None
    assert move.buttons == constants.MOUSE_BTN_RIGHT
    assert (move.dx, move.dy) == (5, 6)


def test_side_buttons_map_to_hid_bits() -> None:
    state = ri.PointerState()
    delta = ri.interpret(packet(button_flags=0x0040 | 0x0100), state)
    assert delta is not None
    assert delta.buttons == constants.MOUSE_BTN_SIDE1 | constants.MOUSE_BTN_SIDE2


def test_wheel_value_is_signed_short() -> None:
    state = ri.PointerState()

    up = ri.interpret(packet(button_flags=ri.RI_MOUSE_WHEEL, button_data=120), state)
    down = ri.interpret(
        packet(button_flags=ri.RI_MOUSE_WHEEL, button_data=0x10000 - 120), state
    )

    assert up is not None and up.wheel == 120
    assert down is not None and down.wheel == -120
    assert up.buttons == 0 and down.buttons == 0


def test_as_signed_short_boundaries() -> None:
    assert ri.as_signed_short(0xFFFF) == -1
    assert ri.as_signed_short(0x8000) == -32768
    assert ri.as_signed_short(0x7FFF) == 32767
    assert ri.as_signed_short(120) == 120
    assert ri.as_signed_short(0x10000 + 120) == 120


# ---------------------------------------------------------------------------
# 纯逻辑：绝对设备（附录 A.4）
# ---------------------------------------------------------------------------
def test_normalize_absolute_maps_full_range() -> None:
    rect = Rect(0, 0, 1920, 1080)
    assert ri.normalize_absolute(0, 0, rect) == (0, 0)
    assert ri.normalize_absolute(65535, 65535, rect) == (1920, 1080)
    assert ri.normalize_absolute(32768, 0, rect) == (960, 0)


def test_normalize_absolute_keeps_negative_origin() -> None:
    rect = Rect(-1920, 0, 0, 1080)
    assert ri.normalize_absolute(0, 0, rect) == (-1920, 0)
    assert ri.normalize_absolute(65535, 65535, rect) == (0, 1080)


def test_absolute_first_frame_has_no_delta_then_diffs() -> None:
    state = ri.PointerState()
    rect = Rect(0, 0, 1000, 1000)

    first = ri.interpret(packet(flags=ri.MOUSE_MOVE_ABSOLUTE, x=0, y=0), state, rect)
    delta = ri.interpret(
        packet(flags=ri.MOUSE_MOVE_ABSOLUTE, x=65535, y=0, ts=1.0), state, rect
    )

    assert first is None  # 首帧没有基线，零差分 = 无事件
    assert delta is not None
    assert (delta.dx, delta.dy) == (1000, 0)
    assert delta.absolute is True
    assert delta.ts == 1.0


def test_absolute_baseline_is_per_device() -> None:
    state = ri.PointerState()
    rect = Rect(0, 0, 100, 100)

    assert ri.interpret(packet(flags=1, x=0, y=0, device=1), state, rect) is None
    assert ri.interpret(packet(flags=1, x=0, y=0, device=2), state, rect) is None

    first = ri.interpret(packet(flags=1, x=65535, y=0, device=1), state, rect)
    second = ri.interpret(packet(flags=1, x=65535, y=0, device=2), state, rect)

    assert first is not None and first.dx == 100
    assert second is not None and second.dx == 100


def test_absolute_without_rect_is_defensive() -> None:
    state = ri.PointerState()
    assert ri.interpret(packet(flags=1, x=1000, y=1000), state) is None
    # 有 rect 后基线已经存在（0,0），差分仍然成立
    delta = ri.interpret(packet(flags=1, x=65535, y=0), state, Rect(0, 0, 10, 10))
    assert delta is not None and delta.dx == 10


# ---------------------------------------------------------------------------
# GetRawInputData 输出解码
# ---------------------------------------------------------------------------
def make_raw_input_bytes(
    *,
    dw_type: int = ri.RIM_TYPEMOUSE,
    device: int = 7,
    flags: int = 0,
    x: int = 0,
    y: int = 0,
    button_flags: int = 0,
    button_data: int = 0,
) -> bytes:
    raw = ri._RAWINPUT()
    raw.header.dwType = dw_type
    raw.header.dwSize = ctypes.sizeof(ri._RAWINPUT)
    raw.header.hDevice = device
    raw.data.mouse.usFlags = flags
    raw.data.mouse.lLastX = x
    raw.data.mouse.lLastY = y
    raw.data.mouse.u.buttons.usButtonFlags = button_flags
    raw.data.mouse.u.buttons.usButtonData = button_data
    return ctypes.string_at(ctypes.byref(raw), ctypes.sizeof(raw))


def test_decode_raw_input_relative() -> None:
    data = make_raw_input_bytes(x=-5, y=9, button_flags=0x0001, device=42)

    decoded = ri.decode_raw_input(data)

    assert decoded is not None
    assert (decoded.flags, decoded.x, decoded.y) == (0, -5, 9)
    assert decoded.button_flags == 0x0001
    assert decoded.device == 42


def test_decode_raw_input_keeps_raw_button_data() -> None:
    decoded = ri.decode_raw_input(
        make_raw_input_bytes(button_flags=ri.RI_MOUSE_WHEEL, button_data=0x10000 - 120)
    )

    assert decoded is not None
    assert decoded.button_data == 0x10000 - 120
    assert ri.as_signed_short(decoded.button_data) == -120


def test_decode_raw_input_rejects_keyboard_packet() -> None:
    assert ri.decode_raw_input(make_raw_input_bytes(dw_type=1)) is None


def test_decode_raw_input_rejects_short_buffer() -> None:
    assert ri.decode_raw_input(b"\x00" * 8) is None


def test_struct_sizes_match_win32_abi() -> None:
    assert ctypes.sizeof(ri._RAWINPUTHEADER) == 24
    assert ctypes.sizeof(ri._RAWINPUTDEVICE) == ctypes.sizeof(
        ctypes.c_uint16
    ) * 2 + ctypes.sizeof(ctypes.c_uint32) + ctypes.sizeof(ctypes.c_void_p)
    assert ctypes.sizeof(ri._RAWMOUSE) == 24


# ---------------------------------------------------------------------------
# RawInputSource 线程
# ---------------------------------------------------------------------------
def test_source_delivers_events_and_reports_thread_name() -> None:
    backend = FakeBackend()
    collector = Collector()
    source = ri.RawInputSource(collector, backend=backend)

    source.start()
    try:
        assert source.healthy is True
        assert backend.prepared == 1
        assert any(t.name == "rawinput" and t.is_alive() for t in threading.enumerate())

        backend.push(packet(x=3, y=4, ts=1.0))
        wait_until(lambda: collector.count() == 1)

        delta = collector.snapshot()[0]
        assert (delta.dx, delta.dy, delta.ts) == (3, 4, 1.0)

        source.start()  # 幂等
        assert backend.prepared == 1
    finally:
        source.stop()

    assert source.healthy is False
    assert backend.closed >= 1

    source.stop()  # 幂等
    assert backend.stop_requests == 1


def test_source_uses_screen_rect_for_absolute() -> None:
    backend = FakeBackend()
    collector = Collector()
    rect = Rect(0, 0, 100, 100)
    source = ri.RawInputSource(collector, backend=backend, screen_rect=lambda: rect)

    source.start()
    try:
        backend.push(packet(flags=ri.MOUSE_MOVE_ABSOLUTE, x=0, y=0))
        backend.push(packet(flags=ri.MOUSE_MOVE_ABSOLUTE, x=65535, y=0))
        wait_until(lambda: collector.count() == 1)
    finally:
        source.stop()

    delta = collector.snapshot()[0]
    assert delta.absolute is True
    assert delta.dx == 100


def test_source_start_failure_raises_e201() -> None:
    backend = FakeBackend()
    backend.prepare_error = RuntimeError("RegisterRawInputDevices 失败")
    source = ri.RawInputSource(Collector(), backend=backend)

    with pytest.raises(PadLendError) as excinfo:
        source.start()

    assert excinfo.value.code is ErrorCode.RAWINPUT_FAILED
    assert isinstance(source.last_error, RuntimeError)
    assert source.healthy is False
    assert backend.closed >= 1
    source.stop()


def test_source_passes_through_padlend_error() -> None:
    backend = FakeBackend()
    error = PadLendError(ErrorCode.RAWINPUT_FAILED, "注册被拒")
    backend.prepare_error = error
    source = ri.RawInputSource(Collector(), backend=backend)

    with pytest.raises(PadLendError) as excinfo:
        source.start()

    assert excinfo.value is error
    source.stop()


def test_source_callback_error_does_not_kill_thread() -> None:
    backend = FakeBackend()
    seen: list[RawDelta] = []

    def boom(delta: RawDelta) -> None:
        seen.append(delta)
        raise RuntimeError("回调故意失败")

    source = ri.RawInputSource(boom, backend=backend)
    source.start()
    try:
        backend.push(packet(x=1))
        wait_until(lambda: len(seen) == 1)
        backend.push(packet(x=2))
        wait_until(lambda: len(seen) == 2)

        assert source.healthy is True
        assert source.last_error is None
    finally:
        source.stop()


def test_source_pump_failure_marks_unhealthy() -> None:
    backend = FakeBackend()
    backend.pump_error = RuntimeError("消息循环异常")
    source = ri.RawInputSource(Collector(), backend=backend)

    source.start()
    wait_until(lambda: source.healthy is False)

    assert isinstance(source.last_error, RuntimeError)
    assert backend.closed >= 1
    source.stop()


def test_source_stop_before_start_is_noop() -> None:
    backend = FakeBackend()
    source = ri.RawInputSource(Collector(), backend=backend)

    source.stop()

    assert backend.stop_requests == 0
    assert source.healthy is False


# ---------------------------------------------------------------------------
# 注入事件：必须回到 rawinput 线程分发（遮罩窗口归属该线程）
# ---------------------------------------------------------------------------
def test_inject_dispatches_on_rawinput_thread() -> None:
    backend = FakeBackend()
    collector = Collector()
    source = ri.RawInputSource(collector, backend=backend)
    source.start()
    try:
        delta = ri.RawDelta(dx=7, dy=-3, buttons=0x01)
        assert source.inject(delta) is True
        deadline = time.monotonic() + 1.0
        while not collector.count() and time.monotonic() < deadline:
            time.sleep(0.01)
        deltas = collector.snapshot()
        assert len(deltas) == 1
        assert (deltas[0].dx, deltas[0].dy, deltas[0].buttons) == (7, -3, 0x01)
    finally:
        source.stop()


def test_inject_after_stop_is_rejected() -> None:
    backend = FakeBackend()
    source = ri.RawInputSource(Collector(), backend=backend)
    source.start()
    source.stop()
    assert source.inject(ri.RawDelta(dx=1, dy=0)) is False


def test_inject_does_not_refresh_raw_input_age() -> None:
    backend = FakeBackend()
    source = ri.RawInputSource(Collector(), backend=backend)
    source.start()
    try:
        assert source.inject(ri.RawDelta(dx=1, dy=0)) is True
        time.sleep(0.05)
        # 注入事件不算真实 Raw Input；否则兜底会以 250ms 的间隔“自激”，
        # 永远赶不上 ARMING 的 80ms 窗口。
        assert source.last_event_age == float("inf")
    finally:
        source.stop()


def test_inject_callback_runs_on_rawinput_thread() -> None:
    backend = FakeBackend()
    seen: list[int] = []
    source = ri.RawInputSource(
        lambda ev: seen.append(threading.get_ident()), backend=backend
    )
    source.start()
    try:
        assert source.inject(ri.RawDelta(dx=2, dy=0)) is True
        deadline = time.monotonic() + 1.0
        while not seen and time.monotonic() < deadline:
            time.sleep(0.01)
        assert seen
        assert seen[0] != threading.get_ident()
    finally:
        source.stop()


# ---------------------------------------------------------------------------
# 真实 Win32（默认跳过：会抢走桌面鼠标事件）
# ---------------------------------------------------------------------------
@pytest.mark.skipif(sys.platform != "win32", reason="需要真实 Windows Raw Input")
def test_win32_backend_construction_has_no_side_effects() -> None:
    backend = ri.Win32RawInputBackend()
    assert backend._prepared is False
    assert backend._hwnd is None


@pytest.mark.skipif(not _RUN_HW_TESTS, reason="真实 Raw Input 需 PADLEND_RAWINPUT_HW=1")
def test_win32_source_start_stop() -> None:
    collector = Collector()
    source = ri.RawInputSource(collector)
    source.start()
    try:
        assert source.healthy is True
    finally:
        source.stop()
    assert source.healthy is False
