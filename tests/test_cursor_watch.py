"""CursorWatch：Raw Input 静默时的绝对光标兜底。"""

from __future__ import annotations

from core.events import RawDelta
from core.state import State
from win.cursor_watch import CursorWatch


class _Router:
    def __init__(self, state: State = State.IDLE) -> None:
        self.state = state
        self.events: list[RawDelta] = []

    def on_raw(self, ev: RawDelta) -> None:
        self.events.append(ev)


class _Raw:
    def __init__(self, age: float) -> None:
        self.last_event_age = age


def test_stale_raw_emits_synthetic_delta() -> None:
    router = _Router()
    positions = iter([(100, 200), (110, 190)])
    watch = CursorWatch(
        router,
        _Raw(1.0),
        get_cursor=lambda: next(positions),
        get_buttons=lambda: 0x01,
        stale_s=0.25,
    )
    assert watch.poll_once(now=10.0) is False  # 记录基线
    assert watch.poll_once(now=10.1) is True
    assert len(router.events) == 1
    ev = router.events[0]
    assert (ev.dx, ev.dy, ev.buttons) == (10, -10, 0x01)


def test_fresh_raw_yields_no_synthetic_event() -> None:
    router = _Router()
    watch = CursorWatch(
        router,
        _Raw(0.05),
        get_cursor=lambda: (100, 200),
        stale_s=0.25,
    )
    assert watch.poll_once(now=1.0) is False
    assert router.events == []


def test_captured_state_skips_fallback() -> None:
    router = _Router(State.CAPTURED)
    watch = CursorWatch(
        router,
        _Raw(5.0),
        get_cursor=lambda: (100, 200),
        stale_s=0.25,
    )
    assert watch.poll_once(now=1.0) is False
    assert router.events == []
