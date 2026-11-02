"""VK → HID usage 映射与 6KRO 键盘状态机（设计文档 6.7 / 8.2 / 附录 B）。

职责边界：
- 纯逻辑，只依赖标准库、``constants``、``core.events``；**不 import win32**，
  因此可以被 Router 线程之外的单元测试直接驱动。
- 维护"当前按下的 usage 集合"与"修饰键位图"，每次返回**全量状态**（6.7.2）：
  报告不是增量，否则会粘键。
- 低级钩子回调**不得**调用本模块（本模块有日志 I/O），由 Router 在转发线程调用。

修饰键状态机必须自己维护：``WH_KEYBOARD_LL`` 事件不带修饰位（6.2.2）。
左右修饰优先信任事件的 ``vk``（``VK_LCONTROL`` 等）；钩子只报通用 VK 时，
用 ``scan`` / ``is_extended`` 判左右（附录 B：``scan & 0xE000``）。
"""

from __future__ import annotations

import logging
from collections.abc import Iterable, Sequence
from typing import Final

import constants as C
from core.events import KeyEvent

log = logging.getLogger("tablet.keymap")

# ---------------------------------------------------------------------------
# 虚拟键码（只列本模块关心的；数值来自 WinUser.h）
# ---------------------------------------------------------------------------
VK_SHIFT: Final = 0x10
VK_CONTROL: Final = 0x11
VK_MENU: Final = 0x12
VK_LWIN: Final = 0x5B
VK_RWIN: Final = 0x5C
VK_LSHIFT: Final = 0xA0
VK_RSHIFT: Final = 0xA1
VK_LCONTROL: Final = 0xA2
VK_RCONTROL: Final = 0xA3
VK_LMENU: Final = 0xA4
VK_RMENU: Final = 0xA5

#: 6KRO 报告最多容纳的键数（修饰键走位图，不占槽位）。
MAX_USAGES: Final = C.KEYBOARD_KRO

# ---------------------------------------------------------------------------
# 附录 B 的静态映射（6.7.3 公式之外的全部行）
# ---------------------------------------------------------------------------
_APPENDIX_B_TABLE: Final[dict[int, int]] = {
    0x0D: 0x28,  # VK_RETURN
    0x1B: 0x29,  # VK_ESCAPE
    0x08: 0x2A,  # VK_BACK
    0x09: 0x2B,  # VK_TAB
    0x20: 0x2C,  # VK_SPACE
    0xBD: 0x2D,  # VK_OEM_MINUS  "-_"
    0xBB: 0x2E,  # VK_OEM_PLUS   "=+"
    0xBA: 0x33,  # VK_OEM_1      ";:"
    0xBF: 0x38,  # VK_OEM_2      "/?"
    0xC0: 0x35,  # VK_OEM_3      "`~"
    0xDB: 0x2F,  # VK_OEM_4      "[{"
    0xDC: 0x31,  # VK_OEM_5      "\\|"
    0xDD: 0x30,  # VK_OEM_6      "]}"
    0xDE: 0x34,  # VK_OEM_7      "'\""
    0x25: 0x50,  # VK_LEFT
    0x27: 0x4F,  # VK_RIGHT
    0x26: 0x52,  # VK_UP
    0x28: 0x51,  # VK_DOWN
    0x24: 0x4A,  # VK_HOME
    0x23: 0x4D,  # VK_END
    0x21: 0x4B,  # VK_PRIOR (PageUp)
    0x22: 0x4E,  # VK_NEXT  (PageDown)
    0x2D: 0x49,  # VK_INSERT
    0x2E: 0x4C,  # VK_DELETE
    0x14: 0x39,  # VK_CAPITAL
    0x5D: 0x65,  # VK_APPS
}


def _build_vk_to_usage() -> dict[int, int]:
    """按 6.7.3 公式生成字母/数字/功能键，再并入附录 B 静态表。"""
    table: dict[int, int] = {}
    for offset in range(26):  # VK_A..VK_Z (0x41..0x5A) -> 0x04..0x1D
        table[0x41 + offset] = 0x04 + offset
    for vk in range(0x31, 0x3A):  # VK_1..VK_9 -> 0x1E..0x26
        table[vk] = 0x1E + (vk - 0x31)
    table[0x30] = 0x27  # VK_0
    for offset in range(12):  # VK_F1..VK_F12 (0x70..0x7B) -> 0x3A..0x45
        table[0x70 + offset] = 0x3A + offset
    table.update(_APPENDIX_B_TABLE)
    return table


#: 非修饰键 VK → HID usage（附录 B 全量，单向唯一）。
VK_TO_USAGE: Final[dict[int, int]] = _build_vk_to_usage()

#: 修饰键 VK → 报告字节 0 的位图（6.7.2）。
MODIFIER_VK_BITS: Final[dict[int, int]] = {
    VK_LCONTROL: C.MOD_LEFT_CTRL,
    VK_RCONTROL: C.MOD_RIGHT_CTRL,
    VK_LSHIFT: C.MOD_LEFT_SHIFT,
    VK_RSHIFT: C.MOD_RIGHT_SHIFT,
    VK_LMENU: C.MOD_LEFT_ALT,
    VK_RMENU: C.MOD_RIGHT_ALT,
    VK_LWIN: C.MOD_LEFT_GUI,
    VK_RWIN: C.MOD_RIGHT_GUI,
}

#: 通用修饰 VK → 左右两个位（事件只报通用 VK 时用于白名单/诊断）。
_MODIFIER_FAMILY: Final[dict[int, tuple[int, int]]] = {
    VK_SHIFT: (C.MOD_LEFT_SHIFT, C.MOD_RIGHT_SHIFT),
    VK_CONTROL: (C.MOD_LEFT_CTRL, C.MOD_RIGHT_CTRL),
    VK_MENU: (C.MOD_LEFT_ALT, C.MOD_RIGHT_ALT),
}

# ---------------------------------------------------------------------------
# VK 名称（配置里的 passthrough_keys 是名字，不是数字）
# ---------------------------------------------------------------------------
_LITERAL_NAMES: Final[dict[int, str]] = {
    0x0D: "VK_RETURN",
    0x1B: "VK_ESCAPE",
    0x08: "VK_BACK",
    0x09: "VK_TAB",
    0x20: "VK_SPACE",
    0xBD: "VK_OEM_MINUS",
    0xBB: "VK_OEM_PLUS",
    0xBA: "VK_OEM_1",
    0xBF: "VK_OEM_2",
    0xC0: "VK_OEM_3",
    0xDB: "VK_OEM_4",
    0xDC: "VK_OEM_5",
    0xDD: "VK_OEM_6",
    0xDE: "VK_OEM_7",
    0x25: "VK_LEFT",
    0x27: "VK_RIGHT",
    0x26: "VK_UP",
    0x28: "VK_DOWN",
    0x24: "VK_HOME",
    0x23: "VK_END",
    0x21: "VK_PRIOR",
    0x22: "VK_NEXT",
    0x2D: "VK_INSERT",
    0x2E: "VK_DELETE",
    0x14: "VK_CAPITAL",
    0x5D: "VK_APPS",
    VK_SHIFT: "VK_SHIFT",
    VK_CONTROL: "VK_CONTROL",
    VK_MENU: "VK_MENU",
    VK_LWIN: "VK_LWIN",
    VK_RWIN: "VK_RWIN",
    VK_LSHIFT: "VK_LSHIFT",
    VK_RSHIFT: "VK_RSHIFT",
    VK_LCONTROL: "VK_LCONTROL",
    VK_RCONTROL: "VK_RCONTROL",
    VK_LMENU: "VK_LMENU",
    VK_RMENU: "VK_RMENU",
}


def _build_vk_names() -> dict[int, str]:
    names = dict(_LITERAL_NAMES)
    for offset in range(26):
        names[0x41 + offset] = f"VK_{chr(0x41 + offset)}"
    for vk in range(0x30, 0x3A):
        names[vk] = f"VK_{chr(vk)}"
    for offset in range(12):
        names[0x70 + offset] = f"VK_F{offset + 1}"
    return names


#: VK → 规范名字（``VK_A`` / ``VK_0`` / ``VK_F1`` / ``VK_RETURN`` …）。
VK_NAMES: Final[dict[int, str]] = _build_vk_names()

#: 规范名字 → VK（``passthrough_keys`` 解析用）。
VK_NAME_TO_CODE: Final[dict[str, int]] = {name: vk for vk, name in VK_NAMES.items()}


# ---------------------------------------------------------------------------
# 纯函数
# ---------------------------------------------------------------------------
def usage_for(vk: int) -> int | None:
    """VK → HID usage；不是可转发键时返回 ``None``。"""
    return VK_TO_USAGE.get(vk)


def modifier_bit(vk: int, scan: int = 0, is_extended: bool = False) -> int | None:
    """返回事件对应的修饰键位；不是修饰键返回 ``None``。

    ``scan`` 可以只含低 8 位扫描码，也可以带 ``0xE000`` 扩展位。
    """
    bit = MODIFIER_VK_BITS.get(vk)
    if bit is not None:
        return bit

    scan_low = scan & 0xFF
    if vk == VK_SHIFT:
        # Shift 的左右由扫描码区分（左 0x2A / 右 0x36），扩展位不可靠。
        return C.MOD_RIGHT_SHIFT if scan_low == 0x36 else C.MOD_LEFT_SHIFT

    extended = bool(is_extended or (scan & 0xE000))
    if vk == VK_CONTROL:
        return C.MOD_RIGHT_CTRL if extended else C.MOD_LEFT_CTRL
    if vk == VK_MENU:
        return C.MOD_RIGHT_ALT if extended else C.MOD_LEFT_ALT
    return None


def key_report(modifier: int, usages: Sequence[int]) -> bytes:
    """构造 8 字节 6KRO 报告：``[mod, 0x00, k1..k6]``（6.7.2）。"""
    if len(usages) > MAX_USAGES:
        raise ValueError(f"6KRO 最多 {MAX_USAGES} 个键，收到 {len(usages)} 个")
    if not 0 <= modifier <= 0xFF:
        raise ValueError(f"修饰位图越界: {modifier}")
    slots = list(usages) + [0] * (MAX_USAGES - len(usages))
    if any(not 0 <= usage <= 0xFF for usage in slots):
        raise ValueError(f"HID usage 越界: {slots}")
    return bytes([modifier, 0x00, *slots])


def _resolve_passthrough(names: Iterable[str]) -> tuple[frozenset[int], frozenset[int]]:
    """把配置里的 VK 名字解析成（精确 VK 集合，修饰位集合）。"""
    vks: set[int] = set()
    mods: set[int] = set()
    for raw in names:
        name = str(raw).strip().upper()
        code = VK_NAME_TO_CODE.get(name)
        if code is None:
            log.warning("keymap.passthrough.unknown name=%s", raw)
            continue
        vks.add(code)
        bit = MODIFIER_VK_BITS.get(code)
        if bit is not None:
            mods.add(bit)
            continue
        family = _MODIFIER_FAMILY.get(code)
        if family is not None:
            mods.update(family)
    return frozenset(vks), frozenset(mods)


# ---------------------------------------------------------------------------
# 状态机
# ---------------------------------------------------------------------------
class Keymap:
    """6KRO 键盘状态机；只在 Router 转发线程访问，无需加锁。

    ``on_key`` 每次返回 ``(修饰位图, 当前按下的 usage 列表)``。
    列表按**先按优先**排序；超过 6 键时忽略新键并记一次警告（6.7.2）。
    """

    def __init__(self, passthrough_keys: Iterable[str] = ()) -> None:
        self._mods = 0
        self._pressed: list[int] = []
        self._overflowing = False
        self._unknown_logged: set[int] = set()
        vks, mods = _resolve_passthrough(passthrough_keys)
        self._passthrough_vks = vks
        self._passthrough_mods = mods

    # -- 状态 --------------------------------------------------------------
    @property
    def modifiers(self) -> int:
        """当前修饰键位图（6.7.2 报告字节 0）。"""
        return self._mods

    @property
    def pressed(self) -> tuple[int, ...]:
        """当前按下的 HID usage（先按优先，最多 6 个）。"""
        return tuple(self._pressed)

    @property
    def passthrough_vks(self) -> frozenset[int]:
        """白名单中的精确 VK。"""
        return self._passthrough_vks

    @property
    def passthrough_mods(self) -> frozenset[int]:
        """白名单中的修饰位（含通用 VK 展开出的左右位）。"""
        return self._passthrough_mods

    def report(self) -> tuple[int, list[int]]:
        """当前全量状态（不改变状态）。"""
        return self._mods, list(self._pressed)

    def reset(self) -> None:
        """清空修饰位图与按键集合（6.7.6：每次状态迁移都重置）。"""
        self._mods = 0
        self._pressed.clear()
        self._overflowing = False

    # -- 白名单 ------------------------------------------------------------
    def is_passthrough(self, ev: KeyEvent) -> bool:
        """该事件的键是否只给 PC（6.7.5 ``passthrough_keys``）。

        只用 ``vk`` / ``scan`` 判断，**不改变**状态；Router 应在调用
        ``on_key`` 之前分流，白名单键不进入平板报告。
        """
        if ev.vk in self._passthrough_vks:
            return True
        bit = modifier_bit(ev.vk, ev.scan, ev.is_extended)
        return bit is not None and bit in self._passthrough_mods

    # -- 事件入口 ----------------------------------------------------------
    def on_key(self, ev: KeyEvent) -> tuple[int, list[int]]:
        """处理一个键盘事件，返回``(modifier, usages)``全量报告。"""
        bit = modifier_bit(ev.vk, ev.scan, ev.is_extended)
        if bit is not None:
            if ev.down:
                self._mods |= bit
            else:
                self._mods &= ~bit
            return self.report()

        usage = VK_TO_USAGE.get(ev.vk)
        if usage is None:
            self._log_unknown(ev)
            return self.report()

        if ev.down:
            if usage in self._pressed:
                return self.report()  # 自动重复，不重复占槽
            if len(self._pressed) >= MAX_USAGES:
                if not self._overflowing:
                    self._overflowing = True
                    log.warning(
                        "keymap.overflow pressed=%d ignored_vk=0x%02X usage=0x%02X",
                        len(self._pressed),
                        ev.vk,
                        usage,
                    )
                return self.report()
            self._overflowing = False
            self._pressed.append(usage)
        else:
            while usage in self._pressed:
                self._pressed.remove(usage)
            self._overflowing = False
        return self.report()

    def _log_unknown(self, ev: KeyEvent) -> None:
        """同一未知 VK 只告警一次，避免按住时刷屏。"""
        if ev.vk in self._unknown_logged:
            return
        self._unknown_logged.add(ev.vk)
        log.warning("keymap.unknown vk=0x%02X scan=0x%04X", ev.vk, ev.scan)
