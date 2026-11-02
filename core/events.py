"""跨线程事件类型（设计文档 8.1，冻结契约）。

这些 dataclass 是唯一允许跨线程传递的输入载体：
- Raw Input 线程 → Router
- 键盘钩子线程 → Router

禁止在事件里携带 socket/窗口句柄等可变资源；回调内只入队（铁律 3）。
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class RawDelta:
    """一次鼠标原始事件（Raw Input，设备计数，未经系统加速）。

    - ``dx``/``dy``：相对位移（设备计数）；绝对设备已被归一化成差分。
    - ``buttons``：HID 语义的按键位图（见 constants.MOUSE_BTN_*）。
    - ``wheel``：有符号滚轮量，120 = 一格。
    - ``absolute``：原始设备是否为绝对设备（绘图板 / RDP），仅诊断用。
    - ``ts``：``time.perf_counter()`` 时间戳，用于延迟统计。
    """

    dx: int
    dy: int
    buttons: int = 0
    wheel: int = 0
    absolute: bool = False
    ts: float = 0.0


@dataclass(frozen=True, slots=True)
class KeyEvent:
    """一次键盘事件（WH_KEYBOARD_LL）。

    ``vk`` 为虚拟键码，``scan`` 为扫描码（含扩展位，见 6.2.2），
    ``is_extended`` 用于区分左右修饰键，``down`` 表示按下。
    """

    vk: int
    scan: int
    is_extended: bool
    down: bool
    ts: float = 0.0
