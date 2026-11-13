"""生成 Raw Input 解码对拍向量（Python 侧真值 → Rust 侧回放）。

产物：tests/fixtures/rawinput_vectors.json
      [{"name": "...", "hex": "....", "expect": {...} | null}]

M0 门禁：Rust 的 decode_raw_input 必须对同一批字节给出同样的字段。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

import ctypes  # noqa: E402

from win import rawinput  # noqa: E402


def build(dw_type: int, flags: int, x: int, y: int, button_flags: int, button_data: int,
          device: int = 0) -> bytes:
    raw = rawinput._RAWINPUT()
    raw.header.dwType = dw_type
    raw.header.dwSize = ctypes.sizeof(rawinput._RAWINPUT)
    raw.header.hDevice = device  # type: ignore[assignment]
    raw.data.mouse.usFlags = flags
    raw.data.mouse.lLastX = x
    raw.data.mouse.lLastY = y
    raw.data.mouse.u.buttons.usButtonFlags = button_flags
    raw.data.mouse.u.buttons.usButtonData = button_data
    return bytes(raw)


CASES: list[tuple[str, int, int, int, int, int, int]] = [
    ("move_right_down", rawinput.RIM_TYPEMOUSE, 0, 7, 3, 0x0000, 0),
    ("move_left_up", rawinput.RIM_TYPEMOUSE, 0, -12, -9, 0x0000, 0),
    ("left_down", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0001, 0),
    ("left_up", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0002, 0),
    ("right_down", rawinput.RIM_TYPEMOUSE, 0, 1, 1, 0x0004, 0),
    ("middle_multi", rawinput.RIM_TYPEMOUSE, 0, -3, 4, 0x0010 | 0x0020, 0),
    ("side_buttons", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0040 | 0x0100, 0),
    ("wheel_up_120", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0400, 120),
    ("wheel_down_121", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0400, 0xFF87),
    ("wheel_zero", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0400, 0),
    ("absolute_move", rawinput.RIM_TYPEMOUSE, 0x0001, 32768, 16384, 0x0000, 0),
    ("absolute_with_button", rawinput.RIM_TYPEMOUSE, 0x0001, 65535, 0, 0x0001, 0),
    ("button_and_move", rawinput.RIM_TYPEMOUSE, 0, 40, -40, 0x0004 | 0x0400, 0xFF88),
    ("empty_packet", rawinput.RIM_TYPEMOUSE, 0, 0, 0, 0x0000, 0),
    ("device_id", rawinput.RIM_TYPEMOUSE, 0, 1, 1, 0x0000, 0, 0x1234),
    ("extra_flags", rawinput.RIM_TYPEMOUSE, 0x0400, 5, 5, 0x0000, 0),
    ("keyboard_packet", 1, 0, 0, 0, 0x0000, 0),  # RIM_TYPEKEYBOARD
    ("hid_packet", 2, 0, 0, 0, 0x0000, 0),  # RIM_TYPEHID
]


def main() -> int:
    vectors = []
    for item in CASES:
        name, dw_type, flags, x, y, bflags, bdata, *rest = item
        device = rest[0] if rest else 0
        data = build(dw_type, flags, x, y, bflags, bdata, device)
        packet = rawinput.decode_raw_input(data)
        expect = None
        if packet is not None:
            expect = {
                "flags": packet.flags,
                "x": packet.x,
                "y": packet.y,
                "button_flags": packet.button_flags,
                "button_data": packet.button_data,
                "device": packet.device,
            }
        vectors.append({"name": name, "hex": data.hex(), "expect": expect})

    target = ROOT / "tests" / "fixtures" / "rawinput_vectors.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(vectors, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"[ok] {len(vectors)} 条向量 -> {target}")
    for vector in vectors:
        print(f"  {vector['name']:<22} {vector['expect']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
