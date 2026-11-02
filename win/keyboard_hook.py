"""低级键盘钩子（设计文档 6.2.2 / 8.2）。

- 专用命名线程安装 ``WH_KEYBOARD_LL`` 并跑 ``GetMessage`` 消息循环。
- 回调只做三件事：构造 :class:`KeyEvent`、交给 ``on_event``、返回 1 或
  ``CallNextHookEx``。**回调内绝不做 I/O**（超时会被系统静默卸载钩子）。
- 看门狗每 30s 检查一次：系统有输入但回调计数不增长 → 判定钩子已失效，
  通过 ``PostThreadMessage`` 让钩子线程原地重装（重装必须在钩子线程做）。
- ``import`` 本模块不安装任何钩子；只有 ``install()`` 有副作用。
"""

from __future__ import annotations

import ctypes
import logging
import sys
import threading
import time
from collections.abc import Callable
from ctypes import wintypes
from typing import Any

import constants as C
from core.errors import ErrorCode, PadLendError
from core.events import KeyEvent

log = logging.getLogger("win.keyboard_hook")

# ---------------------------------------------------------------------------
# Win32 常量
# ---------------------------------------------------------------------------
WH_KEYBOARD_LL = 13
HC_ACTION = 0
WM_KEYDOWN = 0x0100
WM_KEYUP = 0x0101
WM_SYSKEYDOWN = 0x0104
WM_SYSKEYUP = 0x0105
WM_QUIT = 0x0012
PM_NOREMOVE = 0x0000
LLKHF_EXTENDED = 0x01
SCAN_EXTENDED = 0xE000

WM_APP = 0x8000
WM_APP_REINSTALL = WM_APP + 1

_INSTALL_TIMEOUT_S = 5.0
_UNINSTALL_TIMEOUT_S = 3.0
_WATCHDOG_TICK_S = 0.5
_JOIN_TIMEOUT_S = 1.0

_LRESULT = ctypes.c_ssize_t


class KBDLLHOOKSTRUCT(ctypes.Structure):
    """``KBDLLHOOKSTRUCT``（WinUser.h）。"""

    _fields_ = [
        ("vkCode", wintypes.DWORD),
        ("scanCode", wintypes.DWORD),
        ("flags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.c_size_t),
    ]


class LASTINPUTINFO(ctypes.Structure):
    """``LASTINPUTINFO``（WinUser.h），看门狗判断系统是否还有输入。"""

    _fields_ = [
        ("cbSize", wintypes.UINT),
        ("dwTime", wintypes.DWORD),
    ]


if sys.platform == "win32":  # pragma: no branch - 本项目只支持 Windows
    _HOOKPROC: Any = ctypes.WINFUNCTYPE(
        _LRESULT, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM
    )
else:  # pragma: no cover - 非 Windows 仅保证 import 不炸
    _HOOKPROC = None

_USER32: Any = None
_KERNEL32: Any = None


def _load_user32() -> Any:
    """惰性加载并配置 user32 原型；import 时不做任何 Win32 调用。"""
    global _USER32
    if _USER32 is not None:
        return _USER32
    if sys.platform != "win32" or _HOOKPROC is None:
        raise PadLendError(ErrorCode.HOOK_FAILED, "低级键盘钩子仅支持 Windows")

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.SetWindowsHookExW.argtypes = [
        ctypes.c_int,
        _HOOKPROC,
        ctypes.c_void_p,
        wintypes.DWORD,
    ]
    user32.SetWindowsHookExW.restype = ctypes.c_void_p
    user32.UnhookWindowsHookEx.argtypes = [ctypes.c_void_p]
    user32.UnhookWindowsHookEx.restype = wintypes.BOOL
    user32.CallNextHookEx.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    user32.CallNextHookEx.restype = _LRESULT
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
    user32.DispatchMessageW.restype = _LRESULT
    user32.PostThreadMessageW.argtypes = [
        wintypes.DWORD,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    user32.PostThreadMessageW.restype = wintypes.BOOL
    user32.GetCurrentThreadId.restype = wintypes.DWORD
    user32.GetLastInputInfo.argtypes = [ctypes.POINTER(LASTINPUTINFO)]
    user32.GetLastInputInfo.restype = wintypes.BOOL
    _USER32 = user32
    return user32


def _load_kernel32() -> Any:
    """惰性加载 kernel32（取模块句柄）。"""
    global _KERNEL32
    if _KERNEL32 is not None:
        return _KERNEL32
    if sys.platform != "win32":
        raise PadLendError(ErrorCode.HOOK_FAILED, "低级键盘钩子仅支持 Windows")
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    _KERNEL32 = kernel32
    return kernel32


def _module_handle() -> int | None:
    """``SetWindowsHookExW`` 的 hMod；失败时退回 NULL（低级钩子允许）。"""
    try:
        handle = _load_kernel32().GetModuleHandleW(None)
    except OSError:
        return None
    return int(handle) if handle else None


def _last_input_tick() -> int | None:
    """系统最后一次输入的时间（``GetTickCount`` 毫秒）；不可用返回 ``None``。"""
    try:
        user32 = _load_user32()
        info = LASTINPUTINFO()
        info.cbSize = ctypes.sizeof(LASTINPUTINFO)
        if not user32.GetLastInputInfo(ctypes.byref(info)):
            return None
        return int(info.dwTime)
    except (OSError, PadLendError):
        return None


class KeyboardHook:
    """键盘低级钩子（6.2.2）。``on_event`` 必须非阻塞，返回 True 表示吞键。"""

    def __init__(self, on_event: Callable[[KeyEvent], bool]) -> None:
        self._on_event = on_event
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._watchdog: threading.Thread | None = None
        self._ready = threading.Event()
        self._stop = threading.Event()
        self._thread_id = 0
        self._hook: int | None = None
        self._proc: Any = None
        self._installed = False
        self._startup_error: BaseException | None = None
        self._calls = 0
        self._callback_errors = 0
        self._reinstalls = 0

    # -- 生命周期 ----------------------------------------------------------
    def install(self) -> None:
        """启动钩子线程并安装钩子；失败抛 :class:`PadLendError`（E200）。"""
        if sys.platform != "win32":
            raise PadLendError(ErrorCode.HOOK_FAILED, "低级键盘钩子仅支持 Windows")

        with self._lock:
            alive = self._thread is not None and self._thread.is_alive()
        if alive:
            return  # 幂等

        self._ready.clear()
        self._stop.clear()
        self._startup_error = None
        self._installed = False

        thread = threading.Thread(
            target=self._run,
            name="padlend-kbd-hook",
            daemon=True,
        )
        with self._lock:
            self._thread = thread
        thread.start()

        if not self._ready.wait(_INSTALL_TIMEOUT_S):
            raise PadLendError(ErrorCode.HOOK_FAILED, "键盘钩子线程启动超时")

        error = self._startup_error
        if error is not None:
            thread.join(_JOIN_TIMEOUT_S)
            raise PadLendError(
                ErrorCode.HOOK_FAILED,
                f"SetWindowsHookExW 失败: {error}",
                cause=error,
            )

    def uninstall(self) -> None:
        """退出消息循环并卸载钩子；幂等。"""
        with self._lock:
            thread = self._thread

        self._stop.set()
        if thread is None:
            return

        if thread.is_alive() and threading.current_thread() is not thread:
            try:
                _load_user32().PostThreadMessageW(self._thread_id, WM_QUIT, 0, 0)
            except (OSError, PadLendError):
                log.warning("kbd.hook.post_quit_failed", exc_info=True)
            thread.join(_UNINSTALL_TIMEOUT_S)
            if thread.is_alive():
                log.error("kbd.hook.uninstall_timeout")

        watchdog = self._watchdog
        if watchdog is not None and watchdog.is_alive():
            watchdog.join(_JOIN_TIMEOUT_S)

        with self._lock:
            hook = self._hook
            self._hook = None
            self._installed = False
            self._thread = None

        # 线程若已异常退出，钩子可能仍挂在句柄上；尽力从当前线程卸载。
        if hook and not thread.is_alive():
            try:
                _load_user32().UnhookWindowsHookEx(hook)
            except (OSError, PadLendError):
                log.warning("kbd.hook.unhook_fallback_failed", exc_info=True)

    @property
    def healthy(self) -> bool:
        """钩子已安装且消息循环线程仍存活。"""
        with self._lock:
            thread = self._thread
            return bool(
                self._installed
                and self._startup_error is None
                and thread is not None
                and thread.is_alive()
            )

    @property
    def reinstalls(self) -> int:
        """看门狗重装次数（诊断用）。"""
        return self._reinstalls

    @property
    def callback_errors(self) -> int:
        """``on_event`` 抛异常的次数（诊断用）。"""
        return self._callback_errors

    # -- 钩子线程 ----------------------------------------------------------
    def _run(self) -> None:
        user32: Any = None
        try:
            user32 = _load_user32()
            # 先创建线程消息队列：PostThreadMessage / 看门狗依赖它。
            msg = wintypes.MSG()
            user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_NOREMOVE)
            self._thread_id = int(user32.GetCurrentThreadId())

            self._proc = _HOOKPROC(self._hook_proc)
            handle = user32.SetWindowsHookExW(WH_KEYBOARD_LL, self._proc, _module_handle(), 0)
            if not handle:
                raise OSError(ctypes.get_last_error(), "SetWindowsHookExW 返回 NULL")

            with self._lock:
                self._hook = int(handle)
                self._installed = True
            self._ready.set()
            self._start_watchdog()
            self._message_loop(user32, msg)
        except BaseException as exc:  # noqa: BLE001 - 线程边界必须兜底
            if not self._ready.is_set():
                self._startup_error = exc
            else:
                log.exception("kbd.hook.thread_crashed")
        finally:
            with self._lock:
                hook = self._hook
                self._hook = None
                self._installed = False
            if user32 is not None and hook:
                user32.UnhookWindowsHookEx(hook)
            self._stop.set()
            self._ready.set()

    def _message_loop(self, user32: Any, msg: wintypes.MSG) -> None:
        while True:
            result = user32.GetMessageW(ctypes.byref(msg), None, 0, 0)
            if result == 0:
                return  # WM_QUIT
            if result == -1:
                log.error("kbd.hook.get_message_failed err=%s", ctypes.get_last_error())
                return
            if msg.message == WM_APP_REINSTALL:
                self._reinstall(user32)
                continue
            user32.TranslateMessage(ctypes.byref(msg))
            user32.DispatchMessageW(ctypes.byref(msg))

    def _reinstall(self, user32: Any) -> None:
        """在钩子线程原地重装（看门狗请求）。"""
        old = self._hook
        if old:
            user32.UnhookWindowsHookEx(old)
        handle = user32.SetWindowsHookExW(WH_KEYBOARD_LL, self._proc, _module_handle(), 0)
        if not handle:
            log.error(
                "kbd.hook.reinstall_failed err=%s",
                ctypes.get_last_error(),
            )
            with self._lock:
                self._hook = None
                self._installed = False
            return
        with self._lock:
            self._hook = int(handle)
            self._installed = True
            self._reinstalls += 1
        log.warning("kbd.hook.reinstalled count=%d", self._reinstalls)

    # -- 回调 --------------------------------------------------------------
    def _hook_proc(self, n_code: int, w_param: int, l_param: int) -> int:
        """低级钩子回调：只构造事件 + 调 on_event，禁止任何 I/O。"""
        self._calls += 1
        if n_code != HC_ACTION:
            return int(self._call_next(n_code, w_param, l_param))

        if w_param in (WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP):
            info = KBDLLHOOKSTRUCT.from_address(l_param)
            scan = int(info.scanCode)
            if int(info.flags) & LLKHF_EXTENDED:
                scan |= SCAN_EXTENDED
            down = w_param in (WM_KEYDOWN, WM_SYSKEYDOWN)
            ev = KeyEvent(
                vk=int(info.vkCode),
                scan=scan,
                is_extended=bool(scan & SCAN_EXTENDED),
                down=down,
                ts=time.perf_counter(),
            )
            try:
                if self._on_event(ev):
                    return 1
            except Exception:  # noqa: BLE001 - 绝不能把异常抛回系统
                self._callback_errors += 1

        return int(self._call_next(n_code, w_param, l_param))

    @staticmethod
    def _call_next(n_code: int, w_param: int, l_param: int) -> int:
        return int(_load_user32().CallNextHookEx(None, n_code, w_param, l_param))

    # -- 看门狗 ------------------------------------------------------------
    def _start_watchdog(self) -> None:
        thread = threading.Thread(
            target=self._watchdog_loop,
            name="padlend-kbd-watchdog",
            daemon=True,
        )
        self._watchdog = thread
        thread.start()

    def _watchdog_loop(self) -> None:
        try:
            interval = max(1.0, float(C.HOOK_WATCHDOG_S))
            last_calls = self._calls
            last_tick = _last_input_tick()
            deadline = time.monotonic() + interval
            while not self._stop.wait(_WATCHDOG_TICK_S):
                now = time.monotonic()
                if now < deadline:
                    continue
                deadline = now + interval

                calls = self._calls
                tick = _last_input_tick()
                stalled = (
                    tick is not None
                    and last_tick is not None
                    and tick != last_tick
                    and calls == last_calls
                )
                if stalled:
                    log.warning(
                        "kbd.watchdog.stalled calls=%d last_input_tick=%d", calls, tick
                    )
                    self._request_reinstall()
                last_calls = calls
                last_tick = tick
        except Exception:
            log.exception("kbd.watchdog.crashed")

    def _request_reinstall(self) -> None:
        """让钩子线程原地重装；跨线程只能发消息，不能直接 SetWindowsHookEx。"""
        try:
            user32 = _load_user32()
            ok = user32.PostThreadMessageW(self._thread_id, WM_APP_REINSTALL, 0, 0)
            if not ok:
                log.error("kbd.watchdog.post_failed err=%s", ctypes.get_last_error())
        except (OSError, PadLendError):
            log.warning("kbd.watchdog.post_failed", exc_info=True)
