r"""生成 tablet 层字节级对拍向量（Python 真值 → Rust 回放）。

产物 tests/fixtures/tablet_vectors.json：
  uhid_mouse / uhid_keyboard / vk_to_usage / modifier_bit / protocol

用法：.\.venv\Scripts\python.exe tools\gen_tablet_vectors.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

import constants as C  # noqa: E402
from tablet import keymap, protocol, uhid  # noqa: E402


def main() -> int:
    mouse = []
    for buttons, dx, dy, wheel in [
        (0, 0, 0, 0),
        (1, 7, 3, 0),
        (0, -12, -9, 0),
        (5, 200, -200, 3),
        (0x1F, 127, -127, 1),
        (0x1F, -128, 128, -1),
    ]:
        mouse.append({
            "buttons": buttons, "dx": dx, "dy": dy, "wheel": wheel,
            "hex": uhid.mouse_report(buttons, dx, dy, wheel).hex(),
        })

    keyboard = []
    for modifier, usages in [
        (0, []),
        (2, [4]),
        (0, [4, 5, 6, 7, 8, 9, 10, 11]),
        (1, [0x0A, 0x0A, 0]),
        (0, [0x65]),
        (0xFF, [0xFF]),
    ]:
        keyboard.append({
            "modifier": modifier, "usages": usages,
            "hex": uhid.keyboard_report(modifier, usages).hex(),
        })

    vks = sorted(set(list(range(0, 0x100)) + [0x5B, 0x5C, 0x5D, 0x20, 0x0D, 0x1B, 0x2C]))
    vk_to_usage = [{"vk": vk, "usage": keymap.usage_for(vk)} for vk in vks]

    modifier_bit = []
    for vk, scan, extended in [
        (0x10, 0, False), (0x11, 0, False), (0xA0, 0x2A, False), (0xA1, 0x36, False),
        (0xA2, 0x1D, True), (0xA3, 0x1D, False), (0x41, 0, False), (0x12, 0, True),
        (0x5B, 0x5B, True),
    ]:
        modifier_bit.append({
            "vk": vk, "scan": scan, "extended": extended,
            "bit": keymap.modifier_bit(vk, scan, extended),
        })

    proto = [
        {"fn": "inject_keycode", "args": [0, 29, 0, 0],
         "hex": protocol.inject_keycode(0, 29, 0, 0).hex()},
        {"fn": "inject_text", "arg": "hello 中文", "hex": protocol.inject_text("hello 中文").hex()},
        {"fn": "set_clipboard", "arg": "clipboard-payload", "sequence": 7,
         "hex": protocol.set_clipboard("clipboard-payload", sequence=7).hex()},
        {"fn": "back_or_screen_on", "arg": 1, "hex": protocol.back_or_screen_on(1).hex()},
        {"fn": "open_hard_keyboard_settings", "hex": protocol.open_hard_keyboard_settings().hex()},
        {"fn": "uhid_create", "id": C.UHID_MOUSE_ID, "vendor": C.HID_VENDOR_ID,
         "product": C.HID_MOUSE_PRODUCT_ID,
         "name_hex": bytes(C.HID_MOUSE_NAME).hex(),
         "desc_hex": bytes(C.HID_MOUSE_DESCRIPTOR).hex(),
         "hex": protocol.uhid_create(
             C.UHID_MOUSE_ID, C.HID_VENDOR_ID, C.HID_MOUSE_PRODUCT_ID,
             C.HID_MOUSE_NAME, C.HID_MOUSE_DESCRIPTOR,
         ).hex()},
        {"fn": "uhid_input", "id": 1, "data_hex": "010203", "hex": protocol.uhid_input(1, b"\x01\x02\x03").hex()},
        {"fn": "uhid_report", "id": 1, "data_hex": "00000000",
         "hex": protocol.uhid_report(1, b"\x00\x00\x00\x00").hex()},
        {"fn": "uhid_destroy", "id": 1, "hex": protocol.uhid_destroy(1).hex()},
    ]

    payload = {
        "uhid_mouse": mouse,
        "uhid_keyboard": keyboard,
        "vk_to_usage": vk_to_usage,
        "modifier_bit": modifier_bit,
        "protocol": proto,
    }
    target = ROOT / "tests" / "fixtures" / "tablet_vectors.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    print(
        f"[ok] {target}: mouse={len(mouse)} keyboard={len(keyboard)} "
        f"vk={len(vk_to_usage)} mod={len(modifier_bit)} proto={len(proto)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
