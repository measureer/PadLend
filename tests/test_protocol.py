"""协议黄金字节与纯逻辑单测（无设备也能跑）。

依据 DESIGN 9.2 / 9.3：所有多字节字段大端，UHID_CREATE 的 nameLen 是 u8、
descLen 是 u16。这些字节一旦错位，平板侧会静默丢掉 UHID 设备，所以逐字节钉死。
"""

from __future__ import annotations

import dataclasses
import os

import pytest

import constants as C
from tablet import protocol as P
from tablet.adb import Adb, DeviceEntry
from tablet.session import (
    DeviceFacts,
    parse_orientation,
    parse_pointer_speed,
    parse_wm_size,
    parse_xscale,
    read_facts,
)
from tablet.uhid import (
    KeyboardState,
    all_released_report,
    keyboard_report,
    mouse_report,
)

# ---------------------------------------------------------------------------
# INJECT_KEYCODE / INJECT_TEXT / BACK_OR_SCREEN_ON
# ---------------------------------------------------------------------------


def test_inject_keycode_down_back_golden() -> None:
    """type=0 action=0 keycode=4 repeat=0 meta=0，共 14 字节。"""
    assert P.inject_keycode(P.ACTION_DOWN, P.KEYCODE_BACK) == bytes(
        [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    )


def test_inject_keycode_up_enter_with_meta_golden() -> None:
    packet = P.inject_keycode(P.ACTION_UP, P.KEYCODE_ENTER, repeat=1, meta_state=0x1000)
    assert packet == bytes([0, 1, 0, 0, 0, 0x42, 0, 0, 0, 1, 0, 0, 0x10, 0])
    assert len(packet) == 14


def test_inject_text_uses_byte_length_not_char_count() -> None:
    assert P.inject_text("abc") == b"\x01\x00\x00\x00\x03abc"
    # 中文 2 字符 = 6 字节 UTF-8
    assert P.inject_text("中文") == b"\x01\x00\x00\x00\x06\xe4\xb8\xad\xe6\x96\x87"
    assert P.inject_text("") == b"\x01\x00\x00\x00\x00"


def test_back_or_screen_on_is_two_bytes() -> None:
    assert P.back_or_screen_on(P.ACTION_UP) == b"\x04\x01"
    assert P.open_hard_keyboard_settings() == b"\x0f"


# ---------------------------------------------------------------------------
# UHID_CREATE / INPUT / DESTROY
# ---------------------------------------------------------------------------


def test_uhid_create_mouse_layout_golden() -> None:
    packet = P.uhid_create(C.UHID_MOUSE_ID, C.HID_VENDOR_ID, C.HID_MOUSE_PRODUCT_ID,
                           C.HID_MOUSE_NAME, C.HID_MOUSE_DESCRIPTOR)
    # type(1) id(2) vendor(2) product(2) nameLen(1)
    assert packet[:8] == bytes([0x0C, 0x00, 0x01, 0x1D, 0x6B, 0x00, 0x01, 0x0D])
    assert packet[8:21] == b"PadLend Mouse"
    assert packet[21:23] == b"\x00\x3e"  # descLen u16 = 62
    assert packet[23:] == C.HID_MOUSE_DESCRIPTOR
    assert len(C.HID_MOUSE_DESCRIPTOR) == 62
    assert len(packet) == 1 + 2 + 2 + 2 + 1 + 13 + 2 + 62


def test_uhid_create_keyboard_layout_golden() -> None:
    packet = P.uhid_create(C.UHID_KEYBOARD_ID, C.HID_VENDOR_ID, C.HID_KEYBOARD_PRODUCT_ID,
                           C.HID_KEYBOARD_NAME, C.HID_KEYBOARD_DESCRIPTOR)
    assert packet[:8] == bytes([0x0C, 0x00, 0x02, 0x1D, 0x6B, 0x00, 0x02, 0x10])
    assert packet[8:24] == b"PadLend Keyboard"
    assert packet[24:26] == b"\x00\x2d"  # descLen u16 = 45
    assert packet[26:] == C.HID_KEYBOARD_DESCRIPTOR
    assert len(C.HID_KEYBOARD_DESCRIPTOR) == 45


def test_uhid_input_and_destroy_golden() -> None:
    report = mouse_report(0, 1, 0, 0)
    packet = P.uhid_input(C.UHID_MOUSE_ID, report)
    assert packet == bytes([0x0D, 0x00, 0x01, 0x00, 0x04, 0x00, 0x01, 0x00, 0x00])
    assert P.uhid_report(C.UHID_MOUSE_ID, report) == packet
    assert P.uhid_destroy(C.UHID_MOUSE_ID) == b"\x0e\x00\x01"
    assert P.uhid_destroy(C.UHID_KEYBOARD_ID) == b"\x0e\x00\x02"


# ---------------------------------------------------------------------------
# UHID 报告
# ---------------------------------------------------------------------------


def test_mouse_report_four_bytes_and_signed_encoding() -> None:
    assert mouse_report(0, 0, 0, 0) == b"\x00\x00\x00\x00"
    assert len(mouse_report(0, 0, 0, 0)) == C.MOUSE_REPORT_SIZE
    # -1 按补码编码成 0xFF
    assert mouse_report(int(C.MOUSE_BTN_LEFT), -1, 0, 0) == b"\x01\xff\x00\x00"
    assert mouse_report(0, 0, -1, 0) == b"\x00\x00\xff\x00"
    assert mouse_report(0, 0, 0, -1) == b"\x00\x00\x00\xff"


def test_mouse_report_clamps_to_hid_range() -> None:
    assert mouse_report(0, 200, 0, 0) == b"\x00\x7f\x00\x00"
    assert mouse_report(0, -200, 0, 0) == b"\x00\x81\x00\x00"
    assert mouse_report(0, C.HID_CLAMP, C.HID_CLAMP, C.HID_CLAMP) == bytes(
        [0, 0x7F, 0x7F, 0x7F]
    )
    # 按键位只保留低 5 位（9.3）
    assert mouse_report(0xFF, 0, 0, 0) == b"\x1f\x00\x00\x00"


def test_keyboard_report_eight_bytes_full_state() -> None:
    assert len(all_released_report()) == C.KEYBOARD_REPORT_SIZE
    assert all_released_report() == b"\x00" * 8
    assert keyboard_report(0, []) == b"\x00" * 8
    assert keyboard_report(0x02, [0x04, 0x05]) == bytes([0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0])


def test_keyboard_report_6kro_press_order_first() -> None:
    packet = keyboard_report(0, [1, 2, 3, 4, 5, 6, 7])
    assert len(packet) == 8
    assert packet == bytes([0, 0, 1, 2, 3, 4, 5, 6])  # 第 7 个键被丢弃

    # 重复 usage 去重、0 跳过
    assert keyboard_report(0, [0x04, 0x04, 0x00, 0x05]) == bytes(
        [0, 0, 0x04, 0x05, 0, 0, 0, 0]
    )


def test_keyboard_state_tracks_order_and_modifiers() -> None:
    state = KeyboardState()
    assert state.report() == b"\x00" * 8

    state.press(0x04)
    state.press(0x05)
    state.press(0x06)
    state.release(0x05)
    state.press(0x07)
    assert state.usages == (0x04, 0x06, 0x07)
    assert state.report() == bytes([0, 0, 0x04, 0x06, 0x07, 0, 0, 0])

    state.press_modifier(0x02)
    state.press_modifier(0x01)
    state.release_modifier(0x01)
    assert state.modifier == 0x02
    assert state.report()[0] == 0x02

    state.set_modifiers(0x1FF)  # 只保留低 8 位
    assert state.modifier == 0xFF


def test_keyboard_state_6kro_ignores_seventh_key() -> None:
    state = KeyboardState()
    for usage in range(1, 8):
        state.press(usage)
    assert state.pressed_count == C.KEYBOARD_KRO
    assert state.usages == (1, 2, 3, 4, 5, 6)
    assert state.ignored == (7,)

    state.release(1)
    assert state.press(8) is True
    assert state.report() == bytes([0, 0, 2, 3, 4, 5, 6, 8])


def test_keyboard_state_release_all_clears_everything() -> None:
    state = KeyboardState()
    state.set_modifiers(0x03)
    state.press(0x04)
    state.press(0x05)
    state.release_all()
    assert state.report() == b"\x00" * 8
    assert state.modifier == 0
    assert state.usages == ()


# ---------------------------------------------------------------------------
# 参数校验：宁可本地 ValueError，也不要发出错位报文
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("value", [-1, 70000])
def test_uhid_id_must_fit_u16(value: int) -> None:
    with pytest.raises(ValueError):
        P.uhid_destroy(value)


def test_uhid_create_rejects_bad_inputs() -> None:
    with pytest.raises(ValueError):
        P.uhid_create(1, 0x1D6B, 1, b"", C.HID_MOUSE_DESCRIPTOR)
    with pytest.raises(ValueError):
        P.uhid_create(1, 0x1D6B, 1, b"x" * 256, C.HID_MOUSE_DESCRIPTOR)
    with pytest.raises(ValueError):
        P.uhid_create(1, 0x1D6B, 1, b"PadLend Mouse", b"")


def test_uhid_input_rejects_empty_and_oversized() -> None:
    with pytest.raises(ValueError):
        P.uhid_input(1, b"")
    with pytest.raises(ValueError):
        P.uhid_input(1, b"x" * 0x10000)


def test_keycode_and_text_validation() -> None:
    with pytest.raises(ValueError):
        P.inject_keycode(-1, P.KEYCODE_BACK)
    with pytest.raises(ValueError):
        P.inject_keycode(P.ACTION_DOWN, -1)
    with pytest.raises(ValueError):
        keyboard_report(0, [256])


# ---------------------------------------------------------------------------
# 纯解析函数（tools/probe.py 的同一实现，无 adb 调用）
# ---------------------------------------------------------------------------


def test_parse_wm_size_prefers_override() -> None:
    text = "Physical size: 2136x3200\nOverride size: 3200x2136\n"
    assert parse_wm_size(text) == (3200, 2136)
    assert parse_wm_size("Physical size: 2136x3200") == (2136, 3200)


def test_parse_orientation_from_viewport() -> None:
    text = (
        "  Viewport INTERNAL: displayId=0, uniqueId=local:0, port=0, orientation=1, "
        "logicalFrame=[0, 0, 3200, 2136]"
    )
    assert parse_orientation(text) == 1
    # 没有 Viewport 时按宽高兜底
    assert parse_orientation("nothing here", width=3200, height=2136) == 1
    assert parse_orientation("nothing here", width=1080, height=2400) == 0


def test_parse_xscale_prefers_cursor_mapper_block() -> None:
    text = (
        "   1: Foo Mapper:\n      XScale: 9.900\n\n"
        "   2: Cursor Input Mapper:\n      XScale: 1.312000\n      YScale: 1.312000\n"
    )
    assert parse_xscale(text) == pytest.approx(1.312)
    assert parse_xscale("no scale") == 1.0


def test_parse_pointer_speed_handles_null() -> None:
    assert parse_pointer_speed("-3\n") == -3
    assert parse_pointer_speed("0") == 0
    assert parse_pointer_speed("null") == 0


def test_device_facts_is_frozen() -> None:
    facts = DeviceFacts(
        serial="ABC123",
        model="CGA-W00",
        width=3200,
        height=2136,
        orientation=1,
        xscale=1.312,
        pointer_speed=-3,
    )
    with pytest.raises(dataclasses.FrozenInstanceError):
        facts.width = 1000  # type: ignore[misc]


# ---------------------------------------------------------------------------
# adb 设备行解析（纯逻辑）
# ---------------------------------------------------------------------------


def test_device_entry_classifies_transport_and_online() -> None:
    usb = DeviceEntry(serial="ABCD1234", state="device", model="CGA-W00")
    wifi = DeviceEntry(serial="192.168.68.10:46511", state="device")
    unauthorized = DeviceEntry(serial="ABCD1234", state="unauthorized")

    assert usb.online and usb.is_usb and not usb.is_wireless
    assert wifi.online and wifi.is_wireless and not wifi.is_usb
    assert not unauthorized.online
    assert not DeviceEntry(serial="x", state="offline").online


# ---------------------------------------------------------------------------
# 真机测试：默认跳过（本机/CI 没有平板）
#
# 需要真机时：
#   PADLEND_REAL_DEVICE=1 .\.venv\Scripts\python.exe -m pytest tests/test_protocol.py -q
# 并确保平板已授权 USB 调试（tools/probe.py 能读到事实）。
# ---------------------------------------------------------------------------


@pytest.mark.skipif(
    os.environ.get("PADLEND_REAL_DEVICE") != "1",
    reason="需要真机（设置 PADLEND_REAL_DEVICE=1 后才会跑；见 tools/probe.py）",
)
def test_real_device_facts() -> None:
    adb = Adb()
    entry = adb.select_device()
    facts = read_facts(adb.with_serial(entry.serial))
    assert facts.serial == entry.serial
    assert facts.width > 0 and facts.height > 0
    assert facts.orientation in (0, 1, 2, 3)
    assert facts.xscale > 0


def test_set_clipboard_packs_sequence_and_text() -> None:
    message = P.set_clipboard("ab", sequence=7)
    assert message[0] == P.MSG_SET_CLIPBOARD == 9
    assert int.from_bytes(message[1:9], "big") == 7
    assert message[9] == 0
    assert int.from_bytes(message[10:14], "big") == 2
    assert message[14:] == b"ab"


def test_set_clipboard_defaults_to_sequence_zero() -> None:
    message = P.set_clipboard("x")
    assert message[9] == 0
    assert int.from_bytes(message[1:9], "big") == 0


def test_deserialize_clipboard_waits_for_fragment_and_decodes_utf8() -> None:
    payload = "平板复制".encode()
    packet = b"\x00" + len(payload).to_bytes(4, "big") + payload
    assert P.deserialize_device_message(packet[:3]) == (None, 0)
    assert P.deserialize_device_message(packet[:-1]) == (None, 0)
    message, consumed = P.deserialize_device_message(packet + b"next")
    assert message == P.DeviceMessage(P.DEVICE_MSG_CLIPBOARD, text="平板复制")
    assert consumed == len(packet)


def test_deserialize_ack_and_uhid_output() -> None:
    ack, ack_size = P.deserialize_device_message(b"\x01" + (7).to_bytes(8, "big"))
    assert ack == P.DeviceMessage(P.DEVICE_MSG_ACK_CLIPBOARD, sequence=7)
    assert ack_size == 9

    uhid, uhid_size = P.deserialize_device_message(b"\x02\x00\x02\x00\x02\xab\xcd")
    assert uhid == P.DeviceMessage(P.DEVICE_MSG_UHID_OUTPUT, uhid_id=2, data=b"\xab\xcd")
    assert uhid_size == 7


def test_deserialize_rejects_unknown_or_oversized_message() -> None:
    with pytest.raises(ValueError, match="未知平板设备消息"):
        P.deserialize_device_message(b"\xff")
    with pytest.raises(ValueError, match="剪贴板消息过长"):
        P.deserialize_device_message(b"\x00" + (P.DEVICE_MSG_MAX_SIZE).to_bytes(4, "big"))


def test_deserialize_uhid_output_waits_for_full_header_and_payload() -> None:
    full = b"\x02\x00\x02\x00\x02\xab\xcd"
    assert P.deserialize_device_message(full[:3]) == (None, 0)
    assert P.deserialize_device_message(full[:6]) == (None, 0)
    message, consumed = P.deserialize_device_message(full + b"tail")
    assert message == P.DeviceMessage(P.DEVICE_MSG_UHID_OUTPUT, uhid_id=2, data=b"\xab\xcd")
    assert consumed == len(full)


def test_deserialize_ack_waits_for_full_sequence() -> None:
    full = b"\x01" + (7).to_bytes(8, "big")
    assert P.deserialize_device_message(full[:-1]) == (None, 0)
    message, consumed = P.deserialize_device_message(full)
    assert message == P.DeviceMessage(P.DEVICE_MSG_ACK_CLIPBOARD, sequence=7)
    assert consumed == 9
