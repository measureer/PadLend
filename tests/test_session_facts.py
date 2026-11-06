"""设备事实解析/旋转测试（纯函数 + FakeAdb，不碰硬件）。"""

from __future__ import annotations

import threading

import pytest

from tablet.session import (
    DeviceFacts,
    TabletSession,
    clean_serial,
    oriented_size,
    parse_orientation,
    parse_wm_size,
    read_facts,
)


class FakeAdb:
    serial = "fake-serial"

    def __init__(self) -> None:
        self.answers = {
            ("getprop", "ro.product.model"): "CGA-W00",
            ("getprop", "ro.serialno"): "AM4NBB5C23001691",
            ("wm", "size"): "Physical size: 2136x3200",
            ("settings", "get", "system", "pointer_speed"): "-3",
        }

    def shell(self, *args: str, timeout: float = 5.0) -> str:
        if args == ("dumpsys", "input"):
            return (
                "Viewport[0] orientation=1\n"
                "Cursor Input Mapper:\n  XScale: 1.3125\n  YScale: 1.3125\n"
            )
        return self.answers[args]


def test_parse_wm_size_prefers_override() -> None:
    text = "Physical size: 2136x3200\nOverride size: 1600x2560"
    assert parse_wm_size(text) == (1600, 2560)


def test_parse_orientation_from_viewport() -> None:
    assert parse_orientation("Viewport[3] orientation=3") == 3


def test_parse_orientation_fallback_side() -> None:
    assert parse_orientation("", 3200, 2136) == 1
    assert parse_orientation("", 2136, 3200) == 0


@pytest.mark.parametrize(
    ("natural", "orientation", "expected"),
    [
        ((2136, 3200), 0, (2136, 3200)),
        ((2136, 3200), 1, (3200, 2136)),
        ((2136, 3200), 2, (2136, 3200)),
        ((2136, 3200), 3, (3200, 2136)),
    ],
)
def test_oriented_size_swaps_on_quarter_turns(
    natural: tuple[int, int], orientation: int, expected: tuple[int, int]
) -> None:
    assert oriented_size(*natural, orientation) == expected


def test_read_facts_uses_current_orientation_size() -> None:
    facts = read_facts(FakeAdb())
    assert (facts.width, facts.height) == (3200, 2136)
    assert facts.orientation == 1
    assert facts.xscale == pytest.approx(1.3125)
    assert facts.pointer_speed == -3
    assert facts.hardware_serial == "AM4NBB5C23001691"


def test_read_facts_falls_back_to_adb_serial() -> None:
    adb = FakeAdb()
    adb.answers[("getprop", "ro.serialno")] = "unknown"
    assert read_facts(adb).hardware_serial == "fake-serial"


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("AM4NBB5C23001691\n", "AM4NBB5C23001691"),
        ("  AM4NBB5C23001691  ", "AM4NBB5C23001691"),
        ("", ""),
        ("unknown", ""),
        ("null", ""),
        ("0", ""),
    ],
)
def test_clean_serial(raw: str, expected: str) -> None:
    assert clean_serial(raw) == expected


def _landscape_facts() -> DeviceFacts:
    return DeviceFacts(
        serial="192.0.2.10:5555",
        model="CGA-W00",
        width=3200,
        height=2136,
        orientation=1,
        xscale=1.0,
        pointer_speed=0,
        hardware_serial="AM4NBB5C23001691",
    )


def test_apply_orientation_updates_facts_and_notifies() -> None:
    session = TabletSession()
    session._started = True
    session._facts = _landscape_facts()
    seen: list[DeviceFacts] = []
    session.set_facts_callback(seen.append)

    session._apply_orientation("Viewport[0] orientation=0\n", threading.Event())

    assert session.facts is not None
    assert (session.facts.width, session.facts.height) == (2136, 3200)
    assert session.facts.orientation == 0
    assert session.facts.hardware_serial == "AM4NBB5C23001691"
    assert seen and seen[0].orientation == 0


def test_apply_orientation_ignores_same_orientation_and_stopped_session() -> None:
    session = TabletSession()
    session._started = True
    session._facts = _landscape_facts()
    seen: list[DeviceFacts] = []
    session.set_facts_callback(seen.append)

    session._apply_orientation("Viewport[0] orientation=1", threading.Event())
    stopped = threading.Event()
    stopped.set()
    session._apply_orientation("Viewport[0] orientation=0", stopped)
    assert seen == []
    assert session.facts is not None and session.facts.orientation == 1


def test_apply_orientation_callback_failure_does_not_raise() -> None:
    session = TabletSession()
    session._started = True
    session._facts = _landscape_facts()

    def boom(_facts: DeviceFacts) -> None:
        raise RuntimeError("gui closed")

    session.set_facts_callback(boom)
    session._apply_orientation("Viewport[0] orientation=0", threading.Event())
    assert session.facts is not None and session.facts.orientation == 0


def test_orientation_watch_without_device_is_a_noop() -> None:
    session = TabletSession()
    session._start_orientation_watch()
    assert session._orientation_thread is None
    session._stop_orientation_watch()
