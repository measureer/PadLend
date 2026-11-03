"""Raw Input 停摆时的光标轮询兜底。

Windows 在某些焦点/桌面切换后会停止向 ``RIDEV_INPUTSINK`` 窗口投递
``WM_INPUT``（日志中表现为最后一次按键事件后彻底静默，但 ``GetCursorPos``
仍在变化）。这里以低频率轮询绝对光标位置，仅在 Raw Input 超过 ``stale_s``
没有事件时合成 ``RawDelta``，**注入回 rawinput 线程**交由 Router 处理；Raw
Input 恢复后立即让位，避免重复计数。

注入必须走 rawinput 线程：接管时创建的遮罩窗口归属该线程，只有该线程有
消息泵。若在轮询线程直接调用 Router，遮罩窗口会变成无响应窗口，点击后
整个应用会挂起。
"""

from __future__ import annotations

import ctypes
import logging
import threading
import time
from collections.abc import Callable

from core.events import RawDelta
from core.state import State
from win.dpi import try_get_cursor_pos

LOG = logging.getLogger("padlend.cursorwatch")

VK_LBUTTON = 0x01
VK_RBUTTON = 0x02
VK_MBUTTON = 0x04
VK_XBUTTON1 = 0x05
VK_XBUTTON2 = 0x06
_BUTTON_MAP = (
    (VK_LBUTTON, 0x01),
    (VK_RBUTTON, 0x02),
    (VK_MBUTTON, 0x04),
    (VK_XBUTTON1, 0x08),
    (VK_XBUTTON2, 0x10),
)


def _query_buttons() -> int:
    try:
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        value = 0
        for vk, bit in _BUTTON_MAP:
            if user32.GetAsyncKeyState(vk) & 0x8000:
                value |= bit
        return value
    except Exception:  # pragma: no cover - 极端环境无 user32
        return 0


class CursorWatch:
    """在 Raw Input 静默时轮询绝对光标并合成相对位移。"""

    def __init__(
        self,
        router: object,
        rawinput: object,
        *,
        get_cursor: Callable[[], tuple[int, int] | None] = try_get_cursor_pos,
        get_buttons: Callable[[], int] = _query_buttons,
        interval_s: float = 0.02,
        stale_s: float = 0.25,
    ) -> None:
        self._router = router
        self._rawinput = rawinput
        self._get_cursor = get_cursor
        self._get_buttons = get_buttons
        self._interval_s = interval_s
        self._stale_s = stale_s
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._last: tuple[int, int] | None = None
        self._last_log = 0.0

    @property
    def running(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def start(self) -> None:
        if self.running:
            return
        self._stop.clear()
        self._last = None
        self._thread = threading.Thread(
            target=self._run, name="padlend-cursorwatch", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        thread = self._thread
        if thread is not None:
            thread.join(timeout=1.5)
        self._thread = None
        self._last = None

    def poll_once(self, now: float | None = None) -> bool:
        """执行一次判定；返回是否合成了位移（便于测试）。"""
        if now is None:
            now = time.monotonic()
        try:
            if self._router.state is State.CAPTURED:  # type: ignore[attr-defined]
                self._last = None
                return False
        except Exception:
            pass
        try:
            age = float(self._rawinput.last_event_age)  # type: ignore[attr-defined]
        except Exception:
            age = float("inf")
        if age < self._stale_s:
            self._last = None
            return False
        position = self._get_cursor()
        if position is None:
            return False
        if self._last is None:
            self._last = position
            return False
        dx = int(position[0]) - int(self._last[0])
        dy = int(position[1]) - int(self._last[1])
        self._last = position
        if dx == 0 and dy == 0:
            return False
        if now - self._last_log >= 2.0:
            self._last_log = now
            LOG.info(
                "cursorwatch.fallback age=%.2fs move=(%d,%d) raw=stalled",
                age,
                dx,
                dy,
            )
        try:
            buttons = int(self._get_buttons())
        except Exception:
            buttons = 0
        try:
            injected = self._rawinput.inject(  # type: ignore[attr-defined]
                RawDelta(dx=dx, dy=dy, buttons=buttons, wheel=0, absolute=False, ts=time.time())
            )
        except Exception:
            LOG.debug("cursorwatch.router_failed", exc_info=True)
            return False
        return bool(injected)

    def _run(self) -> None:
        while not self._stop.wait(self._interval_s):
            self.poll_once()
