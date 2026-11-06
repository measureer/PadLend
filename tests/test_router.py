"""Router / Metrics 单元测试（设计文档 6.5 / 6.6 / 6.7 / 10 / 12）。

全部用 ``tests/fakes.py`` 的结构化替身驱动，不碰 Windows API、ADB 或真机；
时间通过 ``FakeClock`` 精确推进，线程相关用例只做必要的真实等待。
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field, replace

from fakes import (
    FakeClock,
    FakeConfig,
    FakeDesktop,
    FakeKeymap,
    FakeMask,
    FakeSession,
    default_facts,
    key,
    make_placement,
    raw,
)

import constants as C
from core.errors import ErrorCode, PadLendError
from core.geometry import Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect
from core.metrics import Metrics
from core.router import Router
from core.state import State
from win.desktop import DesktopMap

RIGHT = EdgeSegment(1, Edge.RIGHT, 0, 1080)


@dataclass
class Rig:
    config: FakeConfig
    session: FakeSession
    desktop: FakeDesktop
    mask: FakeMask
    clock: FakeClock
    keymap: FakeKeymap | None
    router: Router
    transitions: list[tuple[State, State, str]] = field(default_factory=list)


def make_rig(
    *,
    edges: tuple[Edge, ...] = (Edge.RIGHT,),
    placement: Placement | None = None,
    config: FakeConfig | None = None,
    session: FakeSession | None = None,
    attach_keymap: bool = True,
) -> Rig:
    clock = FakeClock()
    cfg = config if config is not None else FakeConfig()
    ses = session if session is not None else FakeSession()
    desktop = FakeDesktop(edges=edges)
    mask = FakeMask()
    keymap = FakeKeymap() if attach_keymap else None
    router = Router(cfg, ses, desktop, mask, clock=clock, keymap=keymap)
    rig = Rig(cfg, ses, desktop, mask, clock, keymap, router)
    router.on_state_changed = lambda old, new, reason: rig.transitions.append((old, new, reason))
    router.set_placement(placement if placement is not None else make_placement())
    return rig


def at_right_edge(rig: Rig, y: int = 500) -> None:
    rig.desktop.cursor = (1919, y)


def arm(rig: Rig) -> None:
    at_right_edge(rig)
    rig.router.on_raw(raw(5, 0))
    assert rig.router.state is State.ARMING


def capture_by_arm(rig: Rig) -> None:
    arm(rig)
    for _ in range(4):
        rig.clock.advance_ms(10)
        rig.router.on_raw(raw(10, 0))
    assert rig.router.state is State.CAPTURED


def capture_manually(rig: Rig) -> None:
    rig.router.toggle_manual()
    assert rig.router.state is State.CAPTURED


# ---------------------------------------------------------------------------
# 6.5.2 / 6.6 手动接管与遮罩
# ---------------------------------------------------------------------------
def test_manual_capture_shows_one_pixel_clip_and_warps() -> None:
    rig = make_rig()
    capture_manually(rig)
    assert len(rig.mask.shown) == 1
    clip = rig.mask.shown[0]
    assert clip is not None
    assert clip.left == 1919 and clip.right == 1920
    assert clip.top == 0 and clip.bottom == 1080
    assert clip.width == 1
    assert rig.router.estimator is not None
    assert rig.router.estimator.hit() is Edge.LEFT
    assert rig.session.mouse_reports == []
    rig.router._sender_frame()
    reports = rig.session.mouse_reports
    assert reports
    assert any(dx < 0 for _buttons, dx, _dy, _wheel in reports)
    assert all(dy == 0 for _buttons, _dx, dy, _wheel in reports)
    assert (State.IDLE, State.ARMING, "manual") in rig.transitions
    assert (State.ARMING, State.CAPTURED, "manual") in rig.transitions


def test_entry_releases_stale_buttons_before_warp() -> None:
    """平板若还停在上一次会话的按住状态，进入前必须先收到一条全松。"""
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    reports = rig.session.mouse_reports
    assert reports
    assert reports[0] == (0, 0, 0, 0)
    assert all(buttons == 0 for buttons, _dx, _dy, _wheel in reports)


def test_entry_pins_the_tablet_pointer_onto_the_entry_edge() -> None:
    """估计器启动时在 (0,0)，设备上的指针却可能停在屏幕中间：进入时要补一段
    外向位移把它真正顶到入口边上，否则镜面先到边界、指针还差一截就穿回去了。"""
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    reports = rig.session.mouse_reports
    estimator = rig.router.estimator
    assert estimator is not None

    total = -sum(dx for _buttons, dx, _dy, _wheel in reports)
    assert total >= estimator.width
    assert all(dy == 0 and buttons == 0 for buttons, _dx, dy, _wheel in reports)
    assert estimator.x == 0.0


def test_manual_release_restores_cursor_and_debounces() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    rig.router.toggle_manual()
    assert rig.router.state is State.RETURNING
    assert rig.mask.release_calls >= 1
    assert rig.session.key_reports[-1] == (0, ())
    assert rig.keymap is not None and rig.keymap.reset_calls == 1
    assert rig.session.cursor_sets
    x, y = rig.session.cursor_sets[-1]
    assert 0 <= x < 1920 and 0 <= y < 1080
    rig.clock.advance(0.2)
    rig.router._supervisor_tick()
    assert rig.router.state is State.RETURNING
    rig.clock.advance(0.06)
    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE


def test_toggle_manual_without_placement_is_safe() -> None:
    session = FakeSession()
    router = Router(FakeConfig(), session, FakeDesktop(), FakeMask())
    router.toggle_manual()
    assert router.state is State.IDLE
    router.stop()
    assert router.state is State.IDLE


# ---------------------------------------------------------------------------
# 6.5.3 边缘检测 / ARMING
# ---------------------------------------------------------------------------
def test_inward_delta_does_not_arm_but_outward_does() -> None:
    rig = make_rig()
    at_right_edge(rig)
    rig.router.on_raw(raw(-5, 0))
    assert rig.router.state is State.IDLE
    rig.router.on_raw(raw(5, 0))
    assert rig.router.state is State.ARMING


def test_arm_accumulates_to_capture() -> None:
    rig = make_rig()
    capture_by_arm(rig)
    assert rig.router.state is State.CAPTURED
    assert rig.mask.active


def test_arm_timeout_returns_idle() -> None:
    rig = make_rig()
    arm(rig)
    rig.clock.advance(0.09)
    rig.router.on_raw(raw(10, 0))
    assert rig.router.state is State.IDLE
    assert (State.ARMING, State.IDLE, "arm:timeout") in rig.transitions


def test_arm_slow_push_still_captures() -> None:
    # 慢速把鼠标压到边缘：80ms 内只攒了 6px，没到默认 10px；
    # 超时后达到慢推兜底阈值（arm_px 的一半，这里为 5px），仍应接管。
    rig = make_rig()
    arm(rig)
    rig.router.on_raw(raw(3, 0))
    rig.router.on_raw(raw(3, 0))
    rig.clock.advance(0.09)
    rig.router.on_raw(raw(3, 0))
    assert rig.router.state is State.CAPTURED
    assert (State.ARMING, State.CAPTURED, "arm_slow") in rig.transitions


def test_arm_reverse_returns_idle() -> None:
    rig = make_rig()
    arm(rig)
    rig.router.on_raw(raw(-5, 0))
    assert rig.router.state is State.IDLE
    assert (State.ARMING, State.IDLE, "arm:reverse") in rig.transitions


def test_arm_leaving_edge_band_returns_idle() -> None:
    rig = make_rig()
    arm(rig)
    rig.desktop.cursor = (1700, 500)
    rig.router.on_raw(raw(5, 0))
    assert rig.router.state is State.IDLE
    assert (State.ARMING, State.IDLE, "arm:left_edge") in rig.transitions


def test_arm_ignores_same_edge_where_tablet_does_not_reach() -> None:
    # 配置摆在 DISPLAY1 底部，用户在 DISPLAY2 底部同朝向的边上推：
    # 那里其实没有平板，不应再因为边朝向相同就整段接管。
    first = MonitorInfo(1, r"\\.\DISPLAY1", Rect(0, 0, 1920, 1080), 96, True)
    second = MonitorInfo(2, r"\\.\DISPLAY2", Rect(1920, 0, 3840, 1080), 96, False)
    first_bottom = EdgeSegment(1, Edge.BOTTOM, 0, 1920)
    second_bottom = EdgeSegment(2, Edge.BOTTOM, 1920, 3840)
    desktop = FakeDesktop([first, second], segments=[first_bottom, second_bottom])
    router = Router(
        FakeConfig(), FakeSession(), desktop, FakeMask(), clock=FakeClock(), keymap=FakeKeymap()
    )
    router.set_placement(Placement(first_bottom, 0.5, Align.PROPORTIONAL, None))
    desktop.cursor = (2500, 1080)
    router.on_raw(raw(0, 10))
    assert router.state is State.IDLE
    assert router.placement is not None
    assert router.placement.segment == first_bottom
    router.stop()


def test_arm_clips_to_touching_monitor_without_rewriting_placement() -> None:
    # 平板横跨两块屏的下方：只有真正被平板覆盖的 DISPLAY2 段能接管，
    # 锁定条也只覆盖平板这一段；用户配置本身保持不动。
    first = MonitorInfo(1, r"\\.\DISPLAY1", Rect(0, 0, 1920, 1080), 96, True)
    second = MonitorInfo(2, r"\\.\DISPLAY2", Rect(1920, 0, 3840, 1080), 96, False)
    first_bottom = EdgeSegment(1, Edge.BOTTOM, 0, 1920)
    second_bottom = EdgeSegment(2, Edge.BOTTOM, 1920, 3840)
    merged = EdgeSegment(1, Edge.BOTTOM, 0, 3840)
    desktop = FakeDesktop([first, second], segments=[first_bottom, second_bottom])
    mask = FakeMask()
    router = Router(
        FakeConfig(), FakeSession(), desktop, mask, clock=FakeClock(), keymap=FakeKeymap()
    )
    router.set_placement(Placement(merged))
    desktop.cursor = (2500, 1080)
    for _ in range(5):
        router.on_raw(raw(0, 10))
    assert router.state is State.CAPTURED
    assert router.placement is not None
    assert router.placement.segment == merged
    assert router._arm_segment == second_bottom
    clip = mask.shown[-1]
    assert clip is not None
    assert clip.as_tuple() == (1920, 1079, 2720, 1080)
    router.stop()


def test_corner_priority_selects_right_edge() -> None:
    rig = make_rig(edges=(Edge.RIGHT, Edge.TOP))
    rig.desktop.cursor = (1919, 0)
    rig.router.on_raw(raw(5, -5))
    assert rig.router.state is State.ARMING
    assert rig.router.placement is not None
    assert rig.router.placement.segment.edge is Edge.RIGHT
    assert rig.desktop.corner_calls


# ---------------------------------------------------------------------------
# 6.5.3 / 6.5.4 溢出与交还
# ---------------------------------------------------------------------------
def test_overflow_triggers_returning_and_debounce() -> None:
    rig = make_rig()
    capture_by_arm(rig)
    rig.router.on_raw(raw(-(C.EXIT_OVERFLOW_PX + 10), 0))
    assert rig.router.state is State.RETURNING
    assert rig.mask.release_calls >= 1
    rig.clock.advance(0.2)
    rig.router._supervisor_tick()
    assert rig.router.state is State.RETURNING
    rig.clock.advance(0.06)
    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE


def test_overflow_requires_outward_push_at_entry_edge() -> None:
    rig = make_rig()
    capture_by_arm(rig)
    rig.router.on_raw(raw(30, 0))
    assert rig.router.state is State.CAPTURED
    assert rig.router._overflow == 0.0


def test_overflow_counts_only_after_hit() -> None:
    rig = make_rig()
    capture_by_arm(rig)
    rig.router.on_raw(raw(127, 0))
    rig.router.on_raw(raw(-30, 0))
    assert rig.router.state is State.CAPTURED
    assert rig.router._overflow == 0.0


def test_immediate_reverse_at_entry_returns_to_pc() -> None:
    # 误入平板后不需要先深入固定距离；抵达入口边并外推阈值即可返回。
    rig = make_rig()
    capture_by_arm(rig)
    rig.router.on_raw(raw(-(C.EXIT_OVERFLOW_PX + 10), 0))
    assert rig.router.state is State.RETURNING


def test_entry_corner_does_not_hide_return_edge() -> None:
    # 从平板覆盖的底边两端进入时，平板同时位于左/右上角和顶边；顶边仍可返回。
    for pc_x in (200, 1700):
        rig = make_rig(
            edges=(Edge.BOTTOM,),
            placement=make_placement(Edge.BOTTOM, span=(0, 1920)),
        )
        rig.desktop.cursor = (pc_x, 1080)
        for _ in range(5):
            rig.router.on_raw(raw(0, 10))
        assert rig.router.state is State.CAPTURED
        assert rig.router.estimator is not None
        # 默认阈值降低后，第 5 个事件可能已经在平板内移动了一小段；
        # 这里只要求仍贴着入口的顶边，重点是顶边本身还能用来返回。
        assert 0 <= rig.router.estimator.y < 50
        rig.router.on_raw(raw(0, -(C.EXIT_OVERFLOW_PX + 100)))
        assert rig.router.state is State.RETURNING


def test_heartbeat_preserves_held_buttons() -> None:
    # 空格子里的心跳不能把按住的左键冲掉，否则 Android 会在拖拽中途松开。
    rig = make_rig()
    capture_manually(rig)
    for _ in range(10):
        rig.router._sender_frame()  # 排空入口 warp burst
    rig.session.mouse_reports.clear()
    rig.router.on_raw(raw(0, 0, buttons=C.MOUSE_BTN_LEFT))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][0] == C.MOUSE_BTN_LEFT
    rig.session.mouse_reports.clear()
    rig.router._sender_frame()  # 队列为空 → 心跳
    assert rig.session.mouse_reports[-1] == (C.MOUSE_BTN_LEFT, 0, 0, 0)


def test_buttons_held_during_entry_stay_neutral_until_physical_release() -> None:
    # 穿屏前已经按住的键不能在平板入口重新按下，否则顶边会形成意外拖拽。
    rig = make_rig()
    rig.router.on_raw(raw(0, 0, buttons=C.MOUSE_BTN_LEFT))
    capture_manually(rig)
    rig.router._sender_frame()  # 入口 warp 全部为 buttons=0
    assert all(buttons == 0 for buttons, _dx, _dy, _wheel in rig.session.mouse_reports)

    rig.session.mouse_reports.clear()
    rig.router._sender_frame()  # 心跳也不能恢复入口前的按下状态
    assert rig.session.mouse_reports == [(0, 0, 0, 0)]
    rig.router.on_raw(raw(5, 0, buttons=C.MOUSE_BTN_LEFT))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][0] == 0

    # 物理松键后，下一次新的按下/拖拽仍正常转发。
    rig.router.on_raw(raw(0, 0, buttons=0))
    rig.router.on_raw(raw(2, 0, buttons=C.MOUSE_BTN_LEFT))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][0] == C.MOUSE_BTN_LEFT


def test_mouse_buttons_block_exit_until_release() -> None:
    # 拖动时指针常被推回入口边，不能因此把光标交还给 PC。
    rig = make_rig()
    capture_manually(rig)
    rig.router.on_raw(raw(300, 0))
    rig.router.on_raw(raw(-300, 0, buttons=C.MOUSE_BTN_LEFT))
    assert rig.router.state is State.CAPTURED
    rig.router.on_raw(raw(-(C.EXIT_OVERFLOW_PX + 10), 0))
    assert rig.router.state is State.RETURNING


def test_wheel_detent_conversion_accumulates_remainder() -> None:
    # Windows 一格滚轮是 120 单位；HID 一格是 1，不能把 120 当成 120 格。
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    rig.router._sender_frame()  # 首帧先清掉入口 warp burst

    rig.router.on_raw(raw(0, 0, wheel=120))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][3] == 1

    rig.router.on_raw(raw(0, 0, wheel=60))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][3] == 0
    rig.router.on_raw(raw(0, 0, wheel=60))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][3] == 1

    rig.router.on_raw(raw(0, 0, wheel=-120))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][3] == -1


def test_keyboard_send_does_not_hold_router_lock() -> None:
    # 钩子线程里的键盘发送若阻塞，不能把鼠标 Raw Input 一起卡住。
    rig = make_rig()
    capture_manually(rig)
    entered = threading.Event()
    release = threading.Event()

    def blocking_send(modifier: int, usages: tuple[int, ...]) -> None:
        entered.set()
        release.wait(timeout=2.0)

    rig.session.send_key = blocking_send  # type: ignore[method-assign]
    worker = threading.Thread(target=lambda: rig.router.on_key(key(0x41)), daemon=True)
    worker.start()
    assert entered.wait(timeout=1.0)
    started = time.monotonic()
    rig.router.on_raw(raw(1, 0))
    assert time.monotonic() - started < 0.5
    release.set()
    worker.join(timeout=2.0)
    assert not worker.is_alive()


def test_keyboard_refreshes_cursor_for_next_edge_entry() -> None:
    # 用 PC 键盘后，下一次边缘判定必须基于真实绝对光标，而不是旧缓存。
    rig = make_rig()
    rig.desktop.cursor = (432, 123)
    rig.router.on_key(key(0x41))
    assert rig.router._pc_cursor == (432, 123)


def test_button_state_is_forwarded_while_dragging() -> None:
    # 左键按下/拖动/释放必须持续出现在 HID 报告里，否则平板上的拖拽会中断。
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    rig.router._sender_frame()

    rig.router.on_raw(raw(0, 0, buttons=1))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][0] & 0x01
    rig.router.on_raw(raw(3, -2, buttons=1))
    rig.router._sender_frame()
    assert rig.session.mouse_reports[-1][0] & 0x01
    rig.router.on_raw(raw(0, 0, buttons=0))
    rig.router._sender_frame()
    assert not (rig.session.mouse_reports[-1][0] & 0x01)


# ---------------------------------------------------------------------------
# I1：非 CAPTURED 状态绝不发送位移
# ---------------------------------------------------------------------------
def test_i1_no_displacement_outside_captured() -> None:
    rig = make_rig()
    at_right_edge(rig)
    rig.router.on_raw(raw(200, 0))
    assert rig.router.state is State.ARMING
    rig.router._sender_frame()
    assert rig.session.displacements == [(0, 0)]

    for _ in range(4):
        rig.clock.advance_ms(10)
        rig.router.on_raw(raw(10, 0))
    assert rig.router.state is State.CAPTURED
    rig.router._sender_frame()
    rig.router.on_raw(raw(1, 0))
    rig.router._sender_frame()
    assert (1, 0) in rig.session.displacements

    marker = len(rig.session.mouse_reports)
    rig.router.on_raw(raw(-(C.EXIT_OVERFLOW_PX + 10), 0))
    assert rig.router.state is State.RETURNING
    rig.router.on_raw(raw(50, 50))
    rig.router._sender_frame()
    new = rig.session.mouse_reports[marker:]
    assert new
    assert all(dx == 0 and dy == 0 for _buttons, dx, dy, _wheel in new)


# ---------------------------------------------------------------------------
# 发送线程批处理 / warp burst
# ---------------------------------------------------------------------------
def test_drain_batches_at_most_eight() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.router._sender_frame()
    for _ in range(10):
        rig.router.on_raw(raw(1, 0))
    batch = rig.router._drain()
    rest = rig.router._drain()
    assert len(batch) == C.MAX_BATCH
    assert len(rest) == 2
    assert rig.router._drain() == []


def test_saturated_queue_preserves_motion_and_final_button_release() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.router._drain()  # 清空入口 warp

    for _ in range(C.SEND_QUEUE_MAXLEN):
        rig.router.on_raw(raw(1, 0, buttons=C.MOUSE_BTN_LEFT))
    rig.router.on_raw(raw(0, 0, buttons=0))  # 全零，但它是真实松键而不是心跳
    assert len(rig.router._send_queue) == C.SEND_QUEUE_MAXLEN + 1

    drained: list[object] = []
    while rig.router._send_queue:
        drained.extend(rig.router._drain())
    assert len(drained) == C.SEND_QUEUE_MAXLEN + 1
    release = drained[-1]
    assert release.buttons == 0
    assert release.dx == 0
    assert release.dy == 0


def test_warp_burst_exceeds_normal_batch() -> None:
    rig = make_rig(edges=(Edge.RIGHT, Edge.TOP), placement=make_placement(Edge.TOP, span=(0, 1920)))
    capture_manually(rig)
    rig.router._sender_frame()
    reports = rig.session.mouse_reports
    assert len(reports) > C.MAX_BATCH
    assert all(dx == 0 for _buttons, dx, _dy, _wheel in reports)


# ---------------------------------------------------------------------------
# 线程生命周期
# ---------------------------------------------------------------------------
def test_stop_is_idempotent_and_joins_threads() -> None:
    rig = make_rig()
    rig.router.start()
    sender = rig.router._sender_thread
    supervisor = rig.router._supervisor_thread
    assert sender is not None and supervisor is not None
    assert sender.is_alive() and supervisor.is_alive()
    rig.router.stop()
    assert not sender.is_alive() and not supervisor.is_alive()
    releases = rig.mask.release_calls
    assert releases >= 1
    rig.router.stop()
    assert rig.mask.release_calls == releases
    assert rig.session.stop_calls == 0
    rig.router.on_raw(raw(1, 0))
    assert rig.router.state is State.IDLE


def test_sender_thread_emits_heartbeats() -> None:
    rig = make_rig()
    rig.router.start()
    try:
        deadline = time.monotonic() + 1.0
        while not rig.session.mouse_reports and time.monotonic() < deadline:
            time.sleep(0.005)
    finally:
        rig.router.stop()
    assert rig.session.mouse_reports
    assert all(dx == 0 and dy == 0 for _buttons, dx, dy, _wheel in rig.session.mouse_reports)


# ---------------------------------------------------------------------------
# 12.1 故障与重连
# ---------------------------------------------------------------------------
def _wait_until(predicate: Callable[[], bool], timeout: float = 1.0) -> None:
    """轮询等待条件成立；仅线程相关用例使用。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.005)
    raise AssertionError("condition not met within timeout")


def test_start_failure_faults_then_recovers() -> None:
    gate = threading.Event()
    session = FakeSession(
        has_facts=False,
        start_error=PadLendError(ErrorCode.DEVICE_OFFLINE, "no device"),
        start_gate=gate,
    )
    rig = make_rig(session=session)
    try:
        rig.router.start()
        assert rig.router.state is State.FAULT
        assert rig.router.estimator is None
        _wait_until(lambda: session.start_calls >= 1)
        gate.set()
        _wait_until(lambda: rig.router._next_reconnect_at > rig.clock.now)
        rig.clock.advance(1.01)
        rig.router._supervisor_tick()
        assert rig.router.state is State.IDLE
        assert rig.router.estimator is not None
    finally:
        gate.set()
        rig.router.stop()


def test_start_returns_immediately_while_session_start_blocks() -> None:
    gate = threading.Event()
    session = FakeSession(has_facts=False, start_gate=gate)
    rig = make_rig(session=session)
    started = time.monotonic()
    try:
        rig.router.start()
        elapsed = time.monotonic() - started
        assert elapsed < 1.0
        assert rig.router.state is State.FAULT
        _wait_until(lambda: session.start_calls >= 1)
        assert rig.router.state is State.FAULT
        gate.set()
        _wait_until(lambda: rig.router.state is State.IDLE, timeout=2.0)
        assert rig.router.estimator is not None
    finally:
        gate.set()
        rig.router.stop()


def test_stop_cancels_inflight_start_and_discards_late_result() -> None:
    gate = threading.Event()
    session = FakeSession(has_facts=False, start_gate=gate)
    rig = make_rig(session=session)
    rig.router.start()
    _wait_until(lambda: session.start_calls >= 1)
    started = time.monotonic()
    rig.router.stop()
    elapsed = time.monotonic() - started
    assert elapsed < 2.0
    assert session.cancel_calls == 1
    assert rig.router.state is State.IDLE
    settled = len(rig.transitions)
    time.sleep(0.1)
    assert len(rig.transitions) == settled
    assert rig.router.state is State.IDLE


def test_health_failures_fault_and_reconnect() -> None:
    rig = make_rig()
    rig.session.alive = False
    rig.router._supervisor_tick()
    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE
    rig.router._supervisor_tick()
    assert rig.router.state is State.FAULT
    assert rig.mask.release_calls >= 1
    rig.session.alive = True
    rig.clock.advance(1.01)
    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE
    assert rig.session.reconnect_calls == 1


def test_reconnect_backoff_grows() -> None:
    rig = make_rig()
    rig.session.alive = False
    for _ in range(3):
        rig.router._supervisor_tick()
    assert rig.router.state is State.FAULT
    rig.session.reconnect_error = PadLendError(ErrorCode.DEVICE_OFFLINE, "still down")
    rig.clock.advance(1.01)
    rig.router._supervisor_tick()
    assert rig.session.reconnect_calls == 1
    rig.router._supervisor_tick()
    assert rig.session.reconnect_calls == 1
    rig.clock.advance(1.01)
    rig.router._supervisor_tick()
    assert rig.session.reconnect_calls == 2
    assert rig.router.state is State.FAULT


def test_write_failures_fault() -> None:
    rig = make_rig()
    capture_by_arm(rig)
    rig.router._sender_frame()
    rig.session.fail_sends = C.CONSECUTIVE_WRITE_FAILURES
    for _ in range(C.CONSECUTIVE_WRITE_FAILURES):
        rig.router.on_raw(raw(1, 0))
    rig.router._sender_frame()
    assert rig.router.state is State.FAULT
    assert rig.mask.release_calls >= 1


def test_mask_release_when_show_fails() -> None:
    rig = make_rig()
    rig.mask.fail_show = True
    rig.router.toggle_manual()
    assert rig.router.state is State.FAULT
    assert rig.mask.release_calls >= 1


def test_illegal_transition_faults() -> None:
    rig = make_rig()
    assert rig.router._change_state(State.RETURNING, "bogus") is False
    assert rig.router.state is State.FAULT
    assert rig.transitions[-1] == (State.IDLE, State.FAULT, "fault:E900")


def test_emergency_stop_releases_and_stays_usable() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.router.emergency_stop()
    assert rig.router.state is State.IDLE
    assert rig.mask.release_calls >= 1
    assert rig.session.key_reports[-1] == (0, ())
    assert rig.keymap is not None and rig.keymap.reset_calls >= 1
    rig.router.toggle_manual()
    assert rig.router.state is State.CAPTURED


# ---------------------------------------------------------------------------
# 6.7 键盘
# ---------------------------------------------------------------------------
def test_keyboard_suppress_passthrough_and_safety() -> None:
    rig = make_rig()
    capture_manually(rig)
    before = len(rig.session.key_reports)
    assert rig.router.on_key(key(0x41)) is True
    assert len(rig.session.key_reports) == before + 1
    assert rig.session.key_reports[-1] == (0, (4,))

    rig.config.keyboard.suppress_pc = False
    assert rig.router.on_key(key(0x42)) is False
    assert len(rig.session.key_reports) == before + 2

    sent = len(rig.session.key_reports)
    assert rig.router.on_key(key(0x5B)) is False
    assert len(rig.session.key_reports) == sent

    rig.config.keyboard.suppress_pc = True
    assert rig.router.on_key(key(0xA2)) is True
    assert rig.router.on_key(key(0xA4)) is True
    sent = len(rig.session.key_reports)
    assert rig.router.on_key(key(0x53)) is False
    assert len(rig.session.key_reports) == sent


def test_keyboard_forward_disabled_does_not_send() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.config.keyboard.forward = False
    before = len(rig.session.key_reports)
    assert rig.router.on_key(key(0x41)) is False
    assert len(rig.session.key_reports) == before


def test_fault_does_not_swallow_keys() -> None:
    rig = make_rig()
    rig.router._change_state(State.FAULT, "test")
    assert rig.router.on_key(key(0x41)) is False


def test_lazy_keymap_import_uses_real_table() -> None:
    rig = make_rig(attach_keymap=False)
    capture_manually(rig)
    assert rig.router._ensure_keymap_locked() is not None
    assert rig.router.on_key(key(0x41, scan=0x1E)) is True
    modifier, usages = rig.session.key_reports[-1]
    assert modifier == 0
    assert 0x04 in usages


# ---------------------------------------------------------------------------
# 12.1 / 13.4 旋转与热插拔
# ---------------------------------------------------------------------------
def test_rotation_rebuilds_estimator() -> None:
    rig = make_rig()
    rig.router.start()
    try:
        rig.session._facts = default_facts(width=2560, height=1600, orientation=1)
        rig.router._supervisor_tick()
        assert rig.router.estimator is not None
        assert rig.router.estimator.width == 2560
        assert rig.router.estimator.height == 1600
        assert rig.router.estimator.position == (1279.5, 799.5)
    finally:
        rig.router.stop()


def test_session_facts_callback_rebuilds_estimator_and_notifies() -> None:
    """1.0.3：后台朝向监听报告的新事实要立刻生效，并通知 GUI 切换布局。"""
    rig = make_rig()
    rig.router.start()
    seen: list[object] = []
    rig.router.on_orientation_changed = seen.append
    try:
        assert rig.router.facts is not None
        rotated = replace(rig.router.facts, width=2560, height=1600, orientation=1)
        rig.session.publish_facts(rotated)

        assert rig.router.facts == rotated
        assert rig.router.estimator is not None
        assert (rig.router.estimator.width, rig.router.estimator.height) == (2560, 1600)
        assert seen == [rotated]
    finally:
        rig.router.stop()


def test_orientation_callback_failure_does_not_break_router() -> None:
    rig = make_rig()
    rig.router.start()

    def boom(_facts: object) -> None:
        raise RuntimeError("gui gone")

    rig.router.on_orientation_changed = boom
    try:
        assert rig.router.facts is not None
        rotated = replace(rig.router.facts, width=2560, height=1600, orientation=1)
        rig.session.publish_facts(rotated)
        assert rig.router.facts == rotated
    finally:
        rig.router.stop()


def test_display_hotplug_reanchors_placement() -> None:
    rig = make_rig()
    rig.router.start()
    try:
        rig.desktop.set_monitors(
            (MonitorInfo(2, "\\\\.\\DISPLAY2", Rect(1920, 0, 3840, 1080), 96, True),)
        )
        rig.desktop.set_segments((EdgeSegment(2, Edge.TOP, 1920, 3840),))
        rig.router._supervisor_tick()
        assert rig.desktop.refresh_calls == 1
        assert rig.router.placement is not None
        assert rig.router.placement.offset == 0.5
        assert rig.router.placement.align is Align.PROPORTIONAL
        assert rig.router.placement.segment.monitor_handle == 2
        assert rig.router.placement.segment.edge is Edge.TOP
    finally:
        rig.router.stop()


def test_resync_pointer_centers_estimator() -> None:
    rig = make_rig()
    capture_manually(rig)
    assert rig.router.estimator is not None
    rig.router.resync_pointer()
    assert rig.router.estimator.position == (799.5, 1279.5)
    before = len(rig.session.mouse_reports)
    rig.router._sender_frame()
    assert len(rig.session.mouse_reports) > before


def test_set_placement_updates_property() -> None:
    rig = make_rig()
    rig.router.set_placement(make_placement(Edge.TOP, span=(0, 1920)))
    assert rig.router.placement is not None
    assert rig.router.placement.segment.edge is Edge.TOP
    assert rig.router.state is State.IDLE
    assert rig.router.manual is False


# ---------------------------------------------------------------------------
# 7.4 指标
# ---------------------------------------------------------------------------
def test_metrics_snapshot_keys_and_rates() -> None:
    clock = FakeClock(0.0)
    metrics = Metrics(clock=clock)
    metrics.record_raw(1000)
    metrics.record_send(500)
    metrics.record_drop(5)
    metrics.set_backlog(3)
    for value in (10.0, 20.0, 30.0, 40.0, 50.0):
        metrics.record_latency(value)
    snapshot = metrics.snapshot()
    assert snapshot["raw_hz"] == 1000.0
    assert snapshot["send_hz"] == 500.0
    assert snapshot["backlog"] == 3
    assert snapshot["rtt_ms"] == 50.0
    assert snapshot["p50_ms"] == 30.0
    assert snapshot["p95_ms"] == 48.0
    assert snapshot["drop_pct"] == 0.5
    assert snapshot["cpu_pct"] >= 0.0
    assert snapshot["mem_mb"] >= 0.0
    clock.advance(1.5)
    idle = metrics.snapshot()
    assert idle["raw_hz"] == 0.0
    assert idle["send_hz"] == 0.0


def test_metrics_reset_clears_windows() -> None:
    clock = FakeClock(0.0)
    metrics = Metrics(clock=clock)
    metrics.record_raw(10)
    metrics.record_send(10)
    metrics.record_drop(2)
    metrics.record_latency(12.0)
    metrics.set_backlog(4)
    metrics.reset()
    snapshot = metrics.snapshot()
    assert snapshot["raw_hz"] == 0.0
    assert snapshot["send_hz"] == 0.0
    assert snapshot["backlog"] == 0
    assert snapshot["drop_pct"] == 0.0
    # 没有采样就不报延迟：报 0 会被界面当成"RTT 0ms"显示出来。
    assert "rtt_ms" not in snapshot
    assert "p50_ms" not in snapshot
    assert "p95_ms" not in snapshot


def test_metrics_thread_safe() -> None:
    metrics = Metrics(clock=FakeClock(0.0))
    threads = [
        threading.Thread(target=lambda: [metrics.record_raw(1) for _ in range(500)])
        for _ in range(4)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert metrics.snapshot()["raw_hz"] == 2000.0


def test_router_metrics_callback() -> None:
    rig = make_rig()
    collected: list[dict[str, float | int]] = []
    rig.router.on_metrics = collected.append
    rig.router._supervisor_tick()
    assert collected
    expected = {
        "raw_hz",
        "send_hz",
        "backlog",
        "drop_pct",
        "cpu_pct",
        "mem_mb",
    }
    # 空闲时没有延迟样本，rtt/p50/p95 整个键都不出现。
    assert set(collected[0]) == expected
    assert rig.router.metrics_snapshot()["backlog"] == 0


def test_sent_report_records_latency_sample() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.clock.advance_ms(3)
    rig.router.on_raw(raw(2, 0))
    rig.router._sender_frame()
    snapshot = rig.router.metrics_snapshot()
    assert abs(snapshot["rtt_ms"] - 3.0) < 0.01
    assert abs(snapshot["p95_ms"] - 3.0) < 0.01


# ---------------------------------------------------------------------------
# 交还控制时必须补一条"全松"鼠标报告
# ---------------------------------------------------------------------------
def test_return_sends_mouse_release_report() -> None:
    """退出接管要松开平板上的鼠标键，否则下次穿屏会变成一次拖拽点击。"""
    rig = make_rig()
    capture_manually(rig)
    rig.router.on_raw(raw(6, 0, buttons=1))
    rig.router._sender_frame()
    rig.session.mouse_reports.clear()
    rig.router.toggle_manual()  # 退出接管
    assert rig.router.state is State.RETURNING
    assert (0, 0, 0, 0) in rig.session.mouse_reports


def test_abort_capture_sends_mouse_release_report() -> None:
    rig = make_rig()
    capture_manually(rig)
    rig.session.mouse_reports.clear()
    rig.router.stop()
    assert (0, 0, 0, 0) in rig.session.mouse_reports


# ---------------------------------------------------------------------------
# 设备管理（GUI 设备页）：主动连接/断开不能破坏 supervisor 重连语义
# ---------------------------------------------------------------------------
def test_connect_device_switches_serial_and_clears_old_facts() -> None:
    rig = make_rig()
    rig.router._supervisor_tick()
    assert rig.router.facts is not None

    rig.router.connect_device("serial-2")
    assert rig.config.device.serial == "serial-2"
    assert rig.router.facts is None
    assert rig.router.state is State.FAULT
    assert rig.session.stop_calls >= 1

    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE
    assert rig.router.facts is not None
    assert rig.router.connected is True


def test_disconnect_device_suspends_auto_reconnect_until_user_connects() -> None:
    rig = make_rig()
    rig.router.connect_device("serial-1")
    rig.router._supervisor_tick()
    assert rig.router.facts is not None

    rig.router.disconnect_device()
    assert rig.router.state is State.IDLE
    assert rig.router.facts is None
    assert rig.router.connected is False
    calls = rig.session.start_calls
    sent = len(rig.session.mouse_reports)
    for _ in range(C.CONSECUTIVE_WRITE_FAILURES + 1):
        rig.router._sender_frame()
    assert len(rig.session.mouse_reports) == sent
    assert rig.router.state is State.IDLE

    rig.clock.advance(120.0)
    rig.router._supervisor_tick()
    assert rig.session.start_calls == calls
    assert rig.router.state is State.IDLE

    rig.router.connect_device("serial-2")
    rig.router._supervisor_tick()
    assert rig.router.state is State.IDLE
    assert rig.router.facts is not None
    assert rig.config.device.serial == "serial-2"


def test_auto_connect_off_waits_for_explicit_device_connect() -> None:
    rig = make_rig()
    rig.config.device.auto_connect = False
    try:
        rig.router.start()
        time.sleep(0.05)
        assert rig.session.start_calls == 0
        assert rig.router.state is State.IDLE
        assert rig.router.facts is None

        rig.router.connect_device("serial-1")
        rig.router._supervisor_tick()
        _wait_until(lambda: rig.router.facts is not None)
        assert rig.router.connected is True
        assert rig.config.device.serial == "serial-1"
    finally:
        rig.router.stop()


def test_cursor_query_failure_does_not_accumulate_raw_counts():
    """``cursor_pos()`` 存在但返回 None 时，保持绝对位置而不是累加鼠标计数。"""
    rig = make_rig()
    rig.desktop.cursor = (300, 300)
    rig.router.on_raw(raw(1, 0))
    assert rig.router._pc_cursor == (300, 300)

    rig.desktop.cursor_pos = lambda: None  # type: ignore[method-assign]
    rig.router.on_raw(raw(120, 80))
    assert rig.router._pc_cursor == (300, 300)


def test_arm_uses_absolute_cursor_from_real_desktop_map():
    """真实 DesktopMap 提供绝对光标时，底边外推 40px 应能进入 CAPTURED。"""
    desktop = DesktopMap(
        monitors=[
            MonitorInfo(1, r"\.\DISPLAY1", Rect(0, 0, 2560, 1600), 144, True),
            MonitorInfo(2, r"\.\DISPLAY2", Rect(-2560, -198, 0, 1242), 144, False),
        ]
    )
    bottom = next(
        seg
        for seg in desktop.exterior_segments()
        if seg.edge is Edge.BOTTOM and seg.length >= 2000
    )
    cfg = FakeConfig()
    session = FakeSession()
    mask = FakeMask()
    clock = FakeClock()
    router = Router(cfg, session, desktop, mask, clock=clock, keymap=FakeKeymap())
    router.set_placement(Placement(segment=bottom, offset=0.5))
    desktop.cursor_pos = lambda: (-2000, 1242)  # type: ignore[method-assign]

    for _ in range(5):
        router.on_raw(raw(0, 10))

    assert router.state is State.CAPTURED
    assert mask.active


def test_press_in_captured_logs_cursor_diagnose(caplog) -> None:
    """点击是“电脑上冒出鼠标”的现场：按下/抬起要在 CAPTURED 里留下遮罩诊断。

    真实故障只在用户点按时出现，日志里有 ``cap.cursor`` 才能判断是遮罩被拆、
    锁被系统收回，还是外部程序把光标显示回来。
    """
    import logging

    rig = make_rig()
    capture_manually(rig)
    assert rig.router.state is State.CAPTURED

    with caplog.at_level(logging.DEBUG, logger="router"):
        rig.router.on_raw(raw(0, 0, buttons=1))
        rig.router.on_raw(raw(0, 0, buttons=0))

    lines = [r.getMessage() for r in caplog.records if "cap.cursor" in r.getMessage()]
    assert len(lines) == 2, lines
    assert "windows=1" in lines[0]
    assert rig.router.state is State.CAPTURED


# ---------------------------------------------------------------------------
# 用户实机布局回归：平板横跨 DISPLAY2 下方与 DISPLAY1 左下角
# ---------------------------------------------------------------------------
def _corner_layout_rig() -> Rig:
    first = MonitorInfo(1, r"\\.\DISPLAY1", Rect(0, 0, 1707, 1067), 96, True)
    second = MonitorInfo(2, r"\\.\DISPLAY2", Rect(-1707, -132, 0, 828), 96, False)
    segments = (
        EdgeSegment(2, Edge.BOTTOM, -1707, 0),
        EdgeSegment(1, Edge.BOTTOM, 0, 1707),
        EdgeSegment(1, Edge.LEFT, 828, 1067),
    )
    desktop = FakeDesktop([first, second], segments=segments)
    session = FakeSession(default_facts(width=3200, height=2136))
    mask, clock, keymap = FakeMask(), FakeClock(), FakeKeymap()
    router = Router(FakeConfig(), session, desktop, mask, clock=clock, keymap=keymap)
    rig = Rig(FakeConfig(), session, desktop, mask, clock, keymap, router)
    router.set_placement(
        Placement(EdgeSegment(2, Edge.BOTTOM, -1707, 0), 0.3678359247135844)
    )
    return rig


def test_tablet_below_display2_can_enter_where_it_touches_display1_bottom() -> None:
    rig = _corner_layout_rig()
    rig.desktop.cursor = (300, 1067)
    rig.router.on_raw(raw(0, 5))
    assert rig.router.state is State.ARMING
    for _ in range(4):
        rig.clock.advance_ms(10)
        rig.router.on_raw(raw(0, 10))
    assert rig.router.state is State.CAPTURED
    rig.router.stop()


def test_tablet_does_not_arm_on_display1_bottom_where_it_is_absent() -> None:
    rig = _corner_layout_rig()
    rig.desktop.cursor = (1200, 1067)
    rig.router.on_raw(raw(0, 5))
    assert rig.router.state is State.IDLE
    rig.router.stop()


def test_tablet_can_enter_from_display1_left_near_the_corner() -> None:
    rig = _corner_layout_rig()
    rig.desktop.cursor = (0, 1000)
    rig.router.on_raw(raw(-5, 0))
    assert rig.router.state is State.ARMING
    for _ in range(4):
        rig.clock.advance_ms(10)
        rig.router.on_raw(raw(-10, 0))
    assert rig.router.state is State.CAPTURED
    rig.router.stop()
