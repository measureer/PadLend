"""``tablet/keymap.py`` 单元测试（设计文档 6.7 / 附录 B）。

纯逻辑测试：不安装钩子、不依赖 Windows 输入，只喂 :class:`KeyEvent`。
"""

from __future__ import annotations

import logging

import pytest

import constants as C
from config import KeyboardCfg
from core.events import KeyEvent
from tablet import keymap

#: 附录 B 静态表的独立抄本（故意不引用实现里的表，避免同错同对）。
APPENDIX_B_LITERALS: dict[int, int] = {
    0x0D: 0x28,  # VK_RETURN
    0x1B: 0x29,  # VK_ESCAPE
    0x08: 0x2A,  # VK_BACK
    0x09: 0x2B,  # VK_TAB
    0x20: 0x2C,  # VK_SPACE
    0xBD: 0x2D,  # VK_OEM_MINUS
    0xBB: 0x2E,  # VK_OEM_PLUS
    0xBA: 0x33,  # VK_OEM_1
    0xBF: 0x38,  # VK_OEM_2
    0xC0: 0x35,  # VK_OEM_3
    0xDB: 0x2F,  # VK_OEM_4
    0xDC: 0x31,  # VK_OEM_5
    0xDD: 0x30,  # VK_OEM_6
    0xDE: 0x34,  # VK_OEM_7
    0x25: 0x50,  # VK_LEFT
    0x27: 0x4F,  # VK_RIGHT
    0x26: 0x52,  # VK_UP
    0x28: 0x51,  # VK_DOWN
    0x24: 0x4A,  # VK_HOME
    0x23: 0x4D,  # VK_END
    0x21: 0x4B,  # VK_PRIOR
    0x22: 0x4E,  # VK_NEXT
    0x2D: 0x49,  # VK_INSERT
    0x2E: 0x4C,  # VK_DELETE
    0x14: 0x39,  # VK_CAPITAL
    0x5D: 0x65,  # VK_APPS
}


def _ev(
    vk: int,
    *,
    scan: int = 0,
    extended: bool = False,
    down: bool = True,
) -> KeyEvent:
    return KeyEvent(vk=vk, scan=scan, is_extended=extended, down=down)


# ---------------------------------------------------------------------------
# 映射表
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(("vk", "usage"), [(0x41, 0x04), (0x42, 0x05), (0x5A, 0x1D)])
def test_letters_use_appendix_b_formula(vk: int, usage: int) -> None:
    assert keymap.usage_for(vk) == usage


@pytest.mark.parametrize(
    ("vk", "usage"),
    [(0x31, 0x1E), (0x32, 0x1F), (0x39, 0x26), (0x30, 0x27)],
)
def test_digits_use_appendix_b_formula(vk: int, usage: int) -> None:
    assert keymap.usage_for(vk) == usage


@pytest.mark.parametrize(("vk", "usage"), [(0x70, 0x3A), (0x71, 0x3B), (0x7B, 0x45)])
def test_function_keys_use_appendix_b_formula(vk: int, usage: int) -> None:
    assert keymap.usage_for(vk) == usage


@pytest.mark.parametrize(("vk", "usage"), sorted(APPENDIX_B_LITERALS.items()))
def test_appendix_b_literal_sample(vk: int, usage: int) -> None:
    """附录 B 全表逐行核对（抽样由参数化覆盖到每一行）。"""
    assert keymap.usage_for(vk) == usage


def test_table_covers_every_rule_without_gaps() -> None:
    expected = (
        set(range(0x41, 0x5B))  # A..Z
        | set(range(0x30, 0x3A))  # 0..9
        | set(range(0x70, 0x7C))  # F1..F12
        | set(APPENDIX_B_LITERALS)
    )
    assert set(keymap.VK_TO_USAGE) == expected


def test_table_is_one_to_one() -> None:
    usages = list(keymap.VK_TO_USAGE.values())
    assert len(set(usages)) == len(usages)


def test_modifiers_are_not_in_usage_table() -> None:
    assert not set(keymap.MODIFIER_VK_BITS) & set(keymap.VK_TO_USAGE)


def test_every_table_key_has_a_name() -> None:
    assert set(keymap.VK_TO_USAGE) <= set(keymap.VK_NAMES)
    assert set(keymap.MODIFIER_VK_BITS) <= set(keymap.VK_NAMES)
    assert len(keymap.VK_NAME_TO_CODE) == len(keymap.VK_NAMES)


def test_unknown_usage_returns_none() -> None:
    assert keymap.usage_for(0x99) is None
    assert keymap.usage_for(0x00) is None


# ---------------------------------------------------------------------------
# 修饰键
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    ("vk", "bit"),
    [
        (keymap.VK_LCONTROL, C.MOD_LEFT_CTRL),
        (keymap.VK_RCONTROL, C.MOD_RIGHT_CTRL),
        (keymap.VK_LSHIFT, C.MOD_LEFT_SHIFT),
        (keymap.VK_RSHIFT, C.MOD_RIGHT_SHIFT),
        (keymap.VK_LMENU, C.MOD_LEFT_ALT),
        (keymap.VK_RMENU, C.MOD_RIGHT_ALT),
        (keymap.VK_LWIN, C.MOD_LEFT_GUI),
        (keymap.VK_RWIN, C.MOD_RIGHT_GUI),
    ],
)
def test_explicit_modifier_vks(vk: int, bit: int) -> None:
    km = keymap.Keymap()
    assert km.on_key(_ev(vk)) == (bit, [])
    assert km.on_key(_ev(vk, down=False)) == (0, [])


@pytest.mark.parametrize(
    ("vk", "scan", "extended", "bit"),
    [
        (keymap.VK_SHIFT, 0x2A, False, C.MOD_LEFT_SHIFT),
        (keymap.VK_SHIFT, 0x36, False, C.MOD_RIGHT_SHIFT),
        (keymap.VK_CONTROL, 0x1D, False, C.MOD_LEFT_CTRL),
        (keymap.VK_CONTROL, 0x1D, True, C.MOD_RIGHT_CTRL),
        (keymap.VK_CONTROL, 0xE01D, False, C.MOD_RIGHT_CTRL),
        (keymap.VK_MENU, 0x38, False, C.MOD_LEFT_ALT),
        (keymap.VK_MENU, 0x38, True, C.MOD_RIGHT_ALT),
    ],
)
def test_generic_modifier_side_detection(
    vk: int, scan: int, extended: bool, bit: int
) -> None:
    """钩子只报通用 VK_SHIFT/VK_CONTROL/VK_MENU 时，用 scan / 扩展位判左右。"""
    km = keymap.Keymap()
    assert km.on_key(_ev(vk, scan=scan, extended=extended)) == (bit, [])
    assert km.on_key(_ev(vk, scan=scan, extended=extended, down=False)) == (0, [])


def test_modifier_bitmap_accumulates_and_clears() -> None:
    km = keymap.Keymap()
    assert km.on_key(_ev(keymap.VK_LCONTROL)) == (C.MOD_LEFT_CTRL, [])
    assert km.on_key(_ev(keymap.VK_LSHIFT)) == (
        C.MOD_LEFT_CTRL | C.MOD_LEFT_SHIFT,
        [],
    )
    assert km.on_key(_ev(keymap.VK_RMENU)) == (
        C.MOD_LEFT_CTRL | C.MOD_LEFT_SHIFT | C.MOD_RIGHT_ALT,
        [],
    )
    assert km.on_key(_ev(keymap.VK_LMENU, down=False)) == (
        C.MOD_LEFT_CTRL | C.MOD_LEFT_SHIFT | C.MOD_RIGHT_ALT,
        [],
    )
    assert km.on_key(_ev(keymap.VK_LCONTROL, down=False)) == (
        C.MOD_LEFT_SHIFT | C.MOD_RIGHT_ALT,
        [],
    )
    assert km.on_key(_ev(keymap.VK_LSHIFT, down=False)) == (C.MOD_RIGHT_ALT, [])
    assert km.on_key(_ev(keymap.VK_RMENU, down=False)) == (0, [])


def test_modifiers_do_not_consume_six_slots() -> None:
    km = keymap.Keymap()
    for vk in range(0x41, 0x47):  # A..F 共 6 键
        km.on_key(_ev(vk))
    mods, usages = km.on_key(_ev(keymap.VK_LSHIFT))
    assert mods == C.MOD_LEFT_SHIFT
    assert usages == [0x04, 0x05, 0x06, 0x07, 0x08, 0x09]


# ---------------------------------------------------------------------------
# 组合、顺序、释放
# ---------------------------------------------------------------------------
def test_combination_reports_modifier_and_keys() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(keymap.VK_LCONTROL))
    mods, usages = km.on_key(_ev(0x41))  # Ctrl+A
    assert mods == C.MOD_LEFT_CTRL
    assert usages == [0x04]


def test_first_pressed_priority_is_preserved() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(keymap.VK_LSHIFT))
    km.on_key(_ev(0x41))  # Shift
    km.on_key(_ev(0x42))  # A
    km.on_key(_ev(0x43))  # B
    _, usages = km.on_key(_ev(0x44))  # C，A/B/C 保持先按顺序
    assert usages == [0x04, 0x05, 0x06, 0x07]

    _, usages = km.on_key(_ev(0x41, down=False))  # 释放 A
    assert usages == [0x05, 0x06, 0x07]

    _, usages = km.on_key(_ev(0x41))  # 再按 A → 排到队尾
    assert usages == [0x05, 0x06, 0x07, 0x04]


def test_release_reports_full_state_not_delta() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(keymap.VK_LCONTROL))
    km.on_key(_ev(0x41))
    km.on_key(_ev(0x42))
    mods, usages = km.on_key(_ev(0x41, down=False))
    assert mods == C.MOD_LEFT_CTRL
    assert usages == [0x05]  # 仍按住 B


def test_release_of_unpressed_key_is_noop() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(0x41))
    assert km.on_key(_ev(0x42, down=False)) == (0, [0x04])


def test_auto_repeat_does_not_duplicate_slot() -> None:
    km = keymap.Keymap()
    assert km.on_key(_ev(0x41)) == (0, [0x04])
    assert km.on_key(_ev(0x41)) == (0, [0x04])
    assert km.on_key(_ev(0x41, down=False)) == (0, [])


# ---------------------------------------------------------------------------
# 6KRO 上限
# ---------------------------------------------------------------------------
def test_six_key_limit_ignores_new_key_and_warns(
    caplog: pytest.LogCaptureFixture,
) -> None:
    km = keymap.Keymap()
    for vk in range(0x41, 0x47):  # A..F
        km.on_key(_ev(vk))
    with caplog.at_level(logging.WARNING, logger="tablet.keymap"):
        mods, usages = km.on_key(_ev(0x47))  # G 被忽略
    assert mods == 0
    assert usages == [0x04, 0x05, 0x06, 0x07, 0x08, 0x09]
    assert any("overflow" in record.message for record in caplog.records)

    # 释放一个槽位后，新键可以进入
    km.on_key(_ev(0x41, down=False))
    _, usages = km.on_key(_ev(0x47))
    assert usages == [0x05, 0x06, 0x07, 0x08, 0x09, 0x0A]


def test_overflow_warning_is_rate_limited(
    caplog: pytest.LogCaptureFixture,
) -> None:
    km = keymap.Keymap()
    for vk in range(0x41, 0x47):
        km.on_key(_ev(vk))
    with caplog.at_level(logging.WARNING, logger="tablet.keymap"):
        km.on_key(_ev(0x47))
        km.on_key(_ev(0x48))  # 第二个溢出键不再重复告警
    assert len([r for r in caplog.records if "overflow" in r.message]) == 1


# ---------------------------------------------------------------------------
# 未知键、重置
# ---------------------------------------------------------------------------
def test_unknown_vk_is_ignored_and_warned_once(
    caplog: pytest.LogCaptureFixture,
) -> None:
    km = keymap.Keymap()
    km.on_key(_ev(0x41))
    with caplog.at_level(logging.WARNING, logger="tablet.keymap"):
        assert km.on_key(_ev(0x99)) == (0, [0x04])
        assert km.on_key(_ev(0x99, down=False)) == (0, [0x04])
    assert len([r for r in caplog.records if "unknown" in r.message]) == 1


def test_unknown_vk_does_not_enter_slots() -> None:
    km = keymap.Keymap()
    assert km.on_key(_ev(0x7C)) == (0, [])
    assert km.pressed == ()


def test_reset_clears_modifiers_and_keys() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(keymap.VK_LCONTROL))
    km.on_key(_ev(0x41))
    km.reset()
    assert km.modifiers == 0
    assert km.pressed == ()
    assert km.on_key(_ev(0x42)) == (0, [0x05])


# ---------------------------------------------------------------------------
# 白名单（passthrough_keys）
# ---------------------------------------------------------------------------
def test_passthrough_uses_config_defaults() -> None:
    km = keymap.Keymap(KeyboardCfg().passthrough_keys)
    assert km.is_passthrough(_ev(keymap.VK_LWIN))
    assert km.is_passthrough(_ev(keymap.VK_RWIN))
    assert not km.is_passthrough(_ev(0x41))
    assert not km.is_passthrough(_ev(keymap.VK_LSHIFT))


def test_passthrough_matches_generic_modifier_family() -> None:
    km = keymap.Keymap(["VK_CONTROL"])
    assert km.is_passthrough(_ev(keymap.VK_LCONTROL))
    assert km.is_passthrough(_ev(keymap.VK_CONTROL, scan=0x1D, extended=True))
    assert not km.is_passthrough(_ev(keymap.VK_LSHIFT))


def test_passthrough_does_not_mutate_state() -> None:
    km = keymap.Keymap(["VK_LWIN"])
    assert km.is_passthrough(_ev(keymap.VK_LWIN))
    assert km.modifiers == 0
    assert km.pressed == ()


def test_unknown_passthrough_name_is_logged(
    caplog: pytest.LogCaptureFixture,
) -> None:
    with caplog.at_level(logging.WARNING, logger="tablet.keymap"):
        km = keymap.Keymap(["VK_NOT_A_REAL_KEY"])
    assert km.passthrough_vks == frozenset()
    assert km.passthrough_mods == frozenset()
    assert any("passthrough" in record.message for record in caplog.records)


# ---------------------------------------------------------------------------
# 8 字节报告
# ---------------------------------------------------------------------------
def test_key_report_layout() -> None:
    report = keymap.key_report(C.MOD_LEFT_CTRL, [0x04, 0x05])
    assert report == bytes([0x01, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00])
    assert len(report) == C.KEYBOARD_REPORT_SIZE


def test_key_report_from_state_machine() -> None:
    km = keymap.Keymap()
    km.on_key(_ev(keymap.VK_LCONTROL))
    km.on_key(_ev(0x41))
    mods, usages = km.on_key(_ev(0x42))
    assert keymap.key_report(mods, usages) == b"\x01\x00\x04\x05\x00\x00\x00\x00"


def test_key_report_rejects_invalid_input() -> None:
    with pytest.raises(ValueError):
        keymap.key_report(0, [0x04] * 7)
    with pytest.raises(ValueError):
        keymap.key_report(0x100, [])
