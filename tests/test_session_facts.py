"""设备事实解析/旋转测试（纯函数 + FakeAdb，不碰硬件）。"""

from __future__ import annotations

import pytest

from tablet.session import (
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
