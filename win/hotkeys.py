"""系统级保命热键（设计文档 6.2.3 / 6.7.5）。

``RegisterHotKey`` 注册 ``Ctrl+Alt+S``（切换）、``Ctrl+Alt+Q``（急停）、
``Ctrl+Alt+V``（剪贴板），专用命名线程跑消息循环，``WM_HOTKEY`` 分发回调。
不依赖键盘钩子是否正常：钩子被系统卸载时，保命键仍然可用。

``import`` 本模块不注册任何热键；只有 :meth:`HotkeyManager.start` 有副作用。
"""

from __future__ import annotations

import ctypes
import logging
import sys
import threading
from collections.abc import Callable
from ctypes import wintypes
from typing import Any

from core.errors import ErrorCode, PadLendError

log = logging.getLogger("win.hotkeys")

MOD_ALT = 0x0001
MOD_CONTROL = 0x0002
MOD_SHIFT = 0x0004
MOD_WIN = 0x0008

WM_HOTKEY = 0x0312
WM_QUIT = 0x0012
PM_NOREMOVE = 0x0000

HOTKEY_TOGGLE_ID = 0x1001
HOTKEY_PANIC_ID = 0x1002

VK_Q = 0x51
VK_S = 0x53

_CTRL_ALT = MOD_CONTROL | MOD_ALT
_START_TIMEOUT_S = 2.0
_STOP_TIMEOUT_S = 3.0
_JOIN_TIMEOUT_S = 1.0

_USER32: Any = None
_KERNEL32: Any = None


def _load_user32() -> Any:
    """惰性加载并配置 user32 原型；import 时不做任何 Win32 调用。"""
    global _USER32
    if _USER32 is not None:
        return _USER32
    if sys.platform != "win32":
        raise PadLendError(ErrorCode.UNEXPECTED, "RegisterHotKey 仅支持 Windows")

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.RegisterHotKey.argtypes = [
        wintypes.HWND,
        ctypes.c_int,
        wintypes.UINT,
        wintypes.UINT,
    ]
    user32.RegisterHotKey.restype = wintypes.BOOL
    user32.UnregisterHotKey.argtypes = [wintypes.HWND, ctypes.c_int]
    user32.UnregisterHotKey.restype = wintypes.BOOL
    user32.PeekMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG),
        ctypes.c_void_p,
        wintypes.UINT,
        wintypes.UINT,
        wintypes.UINT,
    ]
    user32.PeekMessageW.restype = wintypes.BOOL
    user32.GetMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG),
        ctypes.c_void_p,
        wintypes.UINT,
        wintypes.UINT,
    ]
    user32.GetMessageW.restype = ctypes.c_int
    user32.TranslateMessage.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.TranslateMessage.restype = wintypes.BOOL
    user32.DispatchMessageW.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.DispatchMessageW.restype = ctypes.c_ssize_t
    user32.PostThreadMessageW.argtypes = [
        wintypes.DWORD,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    user32.PostThreadMessageW.restype = wintypes.BOOL
    _USER32 = user32
    return user32


def _load_kernel32() -> Any:
    """``GetCurrentThreadId`` 在 kernel32，不在 user32。"""
    global _KERNEL32
    if _KERNEL32 is not None:
        return _KERNEL32
    if sys.platform != "win32":
        raise PadLendError(ErrorCode.UNEXPECTED, "RegisterHotKey 仅支持 Windows")
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetCurrentThreadId.argtypes = []
    kernel32.GetCurrentThreadId.restype = wintypes.DWORD
    _KERNEL32 = kernel32
    return kernel32


class HotkeyManager:
    """注册并分发保命热键。回调在热键线程执行，必须非阻塞。"""

    def __init__(
        self,
        on_toggle: Callable[[], None],
        on_panic: Callable[[], None],
    ) -> None:
        self._callbacks: dict[int, Callable[[], None]] = {
            HOTKEY_TOGGLE_ID: on_toggle,
            HOTKEY_PANIC_ID: on_panic,
        }

        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._ready = threading.Event()
        self._stop = threading.Event()
        self._thread_id = 0
        self._startup_error: BaseException | None = None
        self._registered: tuple[int, ...] = ()

    # -- 生命周期 ----------------------------------------------------------
    def start(self) -> None:
        """启动热键线程并尽力注册；注册失败只记日志，不阻断主程序。"""
        if sys.platform != "win32":
            raise PadLendError(ErrorCode.UNEXPECTED, "RegisterHotKey 仅支持 Windows")

        with self._lock:
            alive = self._thread is not None and self._thread.is_alive()
        if alive:
            return

        self._ready.clear()
        self._stop.clear()
        self._startup_error = None
        self._registered = ()

        thread = threading.Thread(
            target=self._run,
            name="padlend-hotkeys",
            daemon=True,
        )
        with self._lock:
            self._thread = thread
        thread.start()
        if not self._ready.wait(_START_TIMEOUT_S):
            log.warning("hotkeys.start_timeout")
        if self._startup_error is not None:
            log.error("hotkeys.start_failed err=%r", self._startup_error)

    def stop(self) -> None:
        """退出消息循环并注销热键；幂等。"""
        with self._lock:
            thread = self._thread

        self._stop.set()
        if thread is None:
            return

        if threading.current_thread() is thread:
            return

        if thread.is_alive():
            try:
                _load_user32().PostThreadMessageW(self._thread_id, WM_QUIT, 0, 0)
            except (OSError, PadLendError):
                log.warning("hotkeys.post_quit_failed", exc_info=True)
            thread.join(_STOP_TIMEOUT_S)
            if thread.is_alive():
                log.error("hotkeys.stop_timeout")

        with self._lock:
            self._thread = None

    @property
    def active(self) -> bool:
        """热键线程仍存活。"""
        with self._lock:
            thread = self._thread
            return thread is not None and thread.is_alive()

    @property
    def registered(self) -> tuple[int, ...]:
        """成功注册的热键 id（诊断/测试用）。"""
        return self._registered

    # -- 线程 --------------------------------------------------------------
    def _run(self) -> None:
        user32: Any = None
        try:
            user32 = _load_user32()
            kernel32 = _load_kernel32()
            msg = wintypes.MSG()
            user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_NOREMOVE)
            self._thread_id = int(kernel32.GetCurrentThreadId())

            registered = self._register_all(user32)
            with self._lock:
                self._registered = registered
            self._ready.set()
            self._message_loop(user32, msg)
        except Exception as exc:
            self._startup_error = exc
            log.exception("hotkeys.thread_failed")
            self._ready.set()
        finally:
            if user32 is not None:
                self._unregister_all(user32, self._registered)
            with self._lock:
                self._registered = ()
                self._stop.set()

    def _register_all(self, user32: Any) -> tuple[int, ...]:
        entries = [(HOTKEY_TOGGLE_ID, VK_S), (HOTKEY_PANIC_ID, VK_Q)]

        registered: list[int] = []
        for hotkey_id, vk in entries:
            if user32.RegisterHotKey(None, hotkey_id, _CTRL_ALT, vk):
                registered.append(hotkey_id)
            else:
                log.error(
                    "hotkeys.register_failed id=0x%04X vk=0x%02X err=%s",
                    hotkey_id,
                    vk,
                    ctypes.get_last_error(),
                )
        log.info("hotkeys.registered ids=%s", [hex(i) for i in registered])
        return tuple(registered)

    def _unregister_all(self, user32: Any, registered: tuple[int, ...]) -> None:
        for hotkey_id in registered:
            try:
                if not user32.UnregisterHotKey(None, hotkey_id):
                    log.warning(
                        "hotkeys.unregister_failed id=0x%04X err=%s",
                        hotkey_id,
                        ctypes.get_last_error(),
                    )
            except OSError:
                log.warning("hotkeys.unregister_failed id=0x%04X", hotkey_id, exc_info=True)

    def _message_loop(self, user32: Any, msg: wintypes.MSG) -> None:
        while True:
            result = user32.GetMessageW(ctypes.byref(msg), None, 0, 0)
            if result == 0:
                return  # WM_QUIT
            if result == -1:
                log.error("hotkeys.get_message_failed err=%s", ctypes.get_last_error())
                return
            if msg.message == WM_HOTKEY:
                self._dispatch(int(msg.wParam))
                continue
            user32.TranslateMessage(ctypes.byref(msg))
            user32.DispatchMessageW(ctypes.byref(msg))

    def _dispatch(self, hotkey_id: int) -> None:
        callback = self._callbacks.get(hotkey_id)
        if callback is None:
            log.warning("hotkeys.unknown_id id=0x%04X", hotkey_id)
            return
        try:
            callback()
        except Exception:
            log.exception("hotkeys.callback_failed id=0x%04X", hotkey_id)
