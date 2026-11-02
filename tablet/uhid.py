"""UHID 报文构造（设计文档 9.3 / 9.4 / 6.7.2）。

两种设备、两种定长报告：

* **鼠标**：4 字节 ``[buttons, dx, dy, wheel]``，位移/滚轮有符号 ``-127..127``。
* **键盘**：8 字节 ``[modifier, 0x00, k1..k6]``，**全量状态**而不是增量。

键盘报告必须是"当前所有按下的键 + 修饰位"的快照，否则平板侧会粘键（6.7.2）。
超过 6 键（6KRO 上限）按"先按优先"：第 7 个键被忽略，直到有键释放。

本模块纯计算，import 无副作用。
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

import constants as C

if TYPE_CHECKING:
    from collections.abc import Iterable, Sequence

LOG = logging.getLogger("tablet.uhid")

__all__ = [
    "KeyboardState",
    "all_released_report",
    "keyboard_report",
    "mouse_report",
]


def _clamp_signed(value: int, limit: int = C.HID_CLAMP) -> int:
    return max(-limit, min(limit, int(value)))


def mouse_report(buttons: int, dx: int, dy: int, wheel: int) -> bytes:
    """4 字节鼠标报告（9.3）。

    ``dx``/``dy``/``wheel`` 先夹到 ``±HID_CLAMP``（描述符 Logical Max = 127），
    再按补码编码（``value & 0xFF``）。按键位图只保留低 5 位。
    """
    return bytes(
        (
            int(buttons) & 0x1F,
            _clamp_signed(dx) & 0xFF,
            _clamp_signed(dy) & 0xFF,
            _clamp_signed(wheel) & 0xFF,
        )
    )


def all_released_report() -> bytes:
    """全零报告 = 所有键/修饰释放（防粘键的收尾）。"""
    return bytes(C.KEYBOARD_REPORT_SIZE)


def keyboard_report(modifier: int, usages: Sequence[int] | Iterable[int]) -> bytes:
    """8 字节键盘报告（9.3）。

    * 字节 0：修饰位图（低 8 位）
    * 字节 1：保留 ``0x00``
    * 字节 2–7：当前按下的 HID usage，按传入顺序（先按优先），未用填 ``0x00``
    """
    kept: list[int] = []
    for usage in usages:
        value = int(usage)
        if value == 0:
            continue
        if not 1 <= value <= 0xFF:
            raise ValueError(f"HID usage 超出 1..255: {usage}")
        if value in kept:
            continue
        if len(kept) >= C.KEYBOARD_KRO:
            LOG.debug("6KRO 已满，忽略第 %d 个键 usage=0x%02X（先按优先）", len(kept) + 1, value)
            continue
        kept.append(value)
    return bytes((int(modifier) & 0xFF, 0x00, *kept, *([0x00] * (C.KEYBOARD_KRO - len(kept)))))


class KeyboardState:
    """按键状态机：维护修饰位图 + 按下顺序（6KRO，先按优先）。

    只做状态，不发包；调用方拿 :meth:`report` 的结果走 ``protocol.uhid_report``。
    同一次按下重复出现是幂等的（低级钩子可能重复投递）。
    """

    def __init__(self) -> None:
        self._modifier = 0
        self._pressed: list[int] = []
        self._ignored: list[int] = []

    # -- 查询 --------------------------------------------------------------
    @property
    def modifier(self) -> int:
        return self._modifier

    @property
    def usages(self) -> tuple[int, ...]:
        """当前按下的 usage，按按下顺序。"""
        return tuple(self._pressed)

    @property
    def ignored(self) -> tuple[int, ...]:
        """因 6KRO 满而被忽略的键（诊断用；释放后不会自动补上）。"""
        return tuple(self._ignored)

    @property
    def pressed_count(self) -> int:
        return len(self._pressed)

    # -- 修改 --------------------------------------------------------------
    def set_modifiers(self, bits: int) -> None:
        self._modifier = int(bits) & 0xFF

    def press_modifier(self, bit: int) -> None:
        self._modifier |= int(bit) & 0xFF

    def release_modifier(self, bit: int) -> None:
        self._modifier &= ~(int(bit) & 0xFF) & 0xFF

    def press(self, usage: int) -> bool:
        """按下一个 usage；6KRO 已满时忽略并返回 ``False``。"""
        value = int(usage)
        if value == 0:
            return False
        if not 1 <= value <= 0xFF:
            raise ValueError(f"HID usage 超出 1..255: {usage}")
        if value in self._pressed:
            return True
        if len(self._pressed) >= C.KEYBOARD_KRO:
            LOG.debug("6KRO 已满，忽略 usage=0x%02X（先按优先）", value)
            self._ignored.append(value)
            return False
        self._pressed.append(value)
        return True

    def release(self, usage: int) -> None:
        value = int(usage)
        if value in self._pressed:
            self._pressed.remove(value)
        if value in self._ignored:
            self._ignored.remove(value)

    def release_all(self) -> None:
        self._pressed.clear()
        self._ignored.clear()
        self._modifier = 0

    # -- 输出 --------------------------------------------------------------
    def report(self) -> bytes:
        return keyboard_report(self._modifier, self._pressed)

    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return (
            f"KeyboardState(mod=0x{self._modifier:02X}, "
            f"usages={[hex(u) for u in self._pressed]})"
        )
