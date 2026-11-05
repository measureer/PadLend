"""scrcpy 控制协议封包（设计文档 9.1 / 9.2，纯函数）。

链路::

    PC --TCP 127.0.0.1:{port}--> adb forward --> localabstract:scrcpy --> scrcpy-server

* 连接后先读 1 字节 ``00``（``send_dummy_byte=true``；实测 scrcpy 5.0.1）。
* 之后是连续消息流，无长度前缀，首字节 ``type`` 分派。
* **全部大端**（网络序）。

本模块只做字节拼装：不碰 socket、不碰 adb、import 无副作用。
版本差异（9.5）只改这里。
"""

from __future__ import annotations

import struct
from typing import Final

# 9.2 控制消息 type 枚举（与 scrcpy app/src/control_msg.h 对齐）
MSG_INJECT_KEYCODE: Final = 0
MSG_INJECT_TEXT: Final = 1
MSG_INJECT_TOUCH_EVENT: Final = 2  # 不使用（F2：绝对注入没有系统指针）
MSG_INJECT_SCROLL_EVENT: Final = 3  # 不使用（滚轮走 UHID）
MSG_BACK_OR_SCREEN_ON: Final = 4
MSG_UHID_CREATE: Final = 12
MSG_UHID_INPUT: Final = 13
MSG_UHID_DESTROY: Final = 14
MSG_OPEN_HARD_KEYBOARD_SETTINGS: Final = 15
MSG_SET_CLIPBOARD: Final = 9  # 直接写平板剪贴板，不敲键盘（6.8）

# android.view.KeyEvent.ACTION_* / MotionEvent.ACTION_*
ACTION_DOWN: Final = 0
ACTION_UP: Final = 1

# 单字段宽度（9.2）
_U8 = 0xFF
_U16 = 0xFFFF
_U32 = 0xFFFF_FFFF
_U64 = 0xFFFF_FFFF_FFFF_FFFF

# Android KeyEvent 的常用键码（发送端与 probe 用）
KEYCODE_BACK: Final = 4
KEYCODE_ENTER: Final = 66
KEYCODE_DEL: Final = 67


def _u8(value: int, field: str) -> int:
    if not 0 <= value <= _U8:
        raise ValueError(f"{field} 超出 u8 范围 0..255: {value}")
    return value


def _u16(value: int, field: str) -> int:
    if not 0 <= value <= _U16:
        raise ValueError(f"{field} 超出 u16 范围 0..65535: {value}")
    return value


def _u32(value: int, field: str) -> int:
    if not 0 <= value <= _U32:
        raise ValueError(f"{field} 超出 u32 范围: {value}")
    return value


def _u64(value: int, field: str) -> int:
    if not 0 <= value <= _U64:
        raise ValueError(f"{field} 超出 u64 范围: {value}")
    return value


# ---------------------------------------------------------------------------
# INJECT_KEYCODE (type 0)
# ---------------------------------------------------------------------------
def inject_keycode(action: int, keycode: int, repeat: int = 0, meta_state: int = 0) -> bytes:
    """降级路径 / 兜底键（8.2 的 ``send_keycode``）。

    载荷：action(1) keycode(4) repeat(4) metaState(4)，大端。
    """
    return struct.pack(
        ">BBIII",
        MSG_INJECT_KEYCODE,
        _u8(action, "action"),
        _u32(keycode, "keycode"),
        _u32(repeat, "repeat"),
        _u32(meta_state, "meta_state"),
    )


def inject_text(text: str) -> bytes:
    """UTF-8 直注入（6.7.4 中文备选路径）。

    载荷：len(4) utf8(len)。长度为**字节**数，不是字符数。
    """
    payload = text.encode("utf-8")
    return struct.pack(">BI", MSG_INJECT_TEXT, _u32(len(payload), "text 长度")) + payload


def set_clipboard(text: str, *, sequence: int = 0, paste: bool = False) -> bytes:
    """直接写平板剪贴板（type 9，6.8）。

    载荷：``sequence(8) paste(1) len(4) utf8(len)``，大端。

    和 ``inject_text`` 的区别是它**不会敲键盘**：服务端调
    ``ClipboardManager.setPrimaryClip()``，平板当前 App 收不到任何按键，
    所以可以安全地在接管瞬间与每次复制时自动同步。``paste=True`` 时服务端
    还会在焦点输入框里执行一次粘贴（快捷键路径用）。
    """
    payload = text.encode("utf-8")
    return (
        struct.pack(
            ">BQBI",
            MSG_SET_CLIPBOARD,
            _u64(sequence, "sequence"),
            1 if paste else 0,
            _u32(len(payload), "text 长度"),
        )
        + payload
    )


def back_or_screen_on(action: int) -> bytes:
    """``BACK_OR_SCREEN_ON``（type 4），载荷 action(1)。"""
    return struct.pack(">BB", MSG_BACK_OR_SCREEN_ON, _u8(action, "action"))


# ---------------------------------------------------------------------------
# UHID（type 12 / 13 / 14）
# ---------------------------------------------------------------------------
def uhid_create(uhid_id: int, vendor: int, product: int, name: bytes, desc: bytes) -> bytes:
    """UHID_CREATE（type 12）。

    布局严格按 9.2/9.3：``id(2) vendor(2) product(2) nameLen(1) name descLen(2) desc``。
    ``nameLen`` 是 u8（最长 255），``descLen`` 是 u16。
    """
    if not name:
        raise ValueError("UHID name 不能为空")
    if not desc:
        raise ValueError("UHID 描述符不能为空")
    return (
        struct.pack(
            ">BHHHB",
            MSG_UHID_CREATE,
            _u16(uhid_id, "uhid_id"),
            _u16(vendor, "vendor"),
            _u16(product, "product"),
            _u8(len(name), "name 长度"),
        )
        + name
        + struct.pack(">H", _u16(len(desc), "desc 长度"))
        + desc
    )


def uhid_input(uhid_id: int, data: bytes) -> bytes:
    """UHID_INPUT（type 13）：``id(2) len(2) data(len)``。"""
    if not data:
        raise ValueError("UHID_INPUT 的 data 不能为空")
    return (
        struct.pack(">BHH", MSG_UHID_INPUT, _u16(uhid_id, "uhid_id"), _u16(len(data), "data 长度"))
        + data
    )


def uhid_destroy(uhid_id: int) -> bytes:
    """UHID_DESTROY（type 14）：``id(2)``。"""
    return struct.pack(">BH", MSG_UHID_DESTROY, _u16(uhid_id, "uhid_id"))


def open_hard_keyboard_settings() -> bytes:
    """拉起系统『物理键盘布局』设置页（6.7.4），无载荷。"""
    return bytes([MSG_OPEN_HARD_KEYBOARD_SETTINGS])


# ---------------------------------------------------------------------------
# 便捷封装：把 UHID 报告包成 UHID_INPUT
# ---------------------------------------------------------------------------
def uhid_report(uhid_id: int, report: bytes) -> bytes:
    """把 HID 报告（4/8 字节）包成 ``UHID_INPUT``（发送热路径）。"""
    return uhid_input(uhid_id, report)
