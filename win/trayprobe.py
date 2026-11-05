"""托盘注册自检（Windows）。

``QSystemTrayIcon.isVisible()`` 只反映 Qt 自己的状态：图标在通知区里到底有没有
挂上，Qt 并不知道。0.3.16/0.3.17 里用户报「关闭到托盘后找不到 PadLend」，
日志里却是 ``available=True visible=True``，无法区分下面三种情况：

1. 图标真的挂上了，只是被 Windows 11 收进了任务栏右下角的「^」溢出区；
2. 图标没挂上（``Shell_NotifyIcon`` 静默失败），Qt 照样报 visible；
3. 挂上了但画出来的位图为空（看起来是一块透明）。

这个模块用一次「挂一个探针图标、立刻摘掉」的动作，直接读 ``Shell_NotifyIconW``
的返回值与 ``GetLastError``，给日志一个可信的结论；顺带读注册表里 Windows 为
每个托盘图标记的 ``ExecutablePath``，确认系统这一侧有没有登记我们。

探针窗口是 message-only 窗口（``HWND_MESSAGE``），不显示、不进任务栏。
"""

from __future__ import annotations

import ctypes
import logging
import sys
from ctypes import wintypes

log = logging.getLogger("win.trayprobe")

WM_APP = 0x8000
_WM_TRAY_PROBE = WM_APP + 0x2F

NIM_ADD = 0x00000000
NIM_DELETE = 0x00000002
NIF_MESSAGE = 0x00000001
NIF_ICON = 0x00000002
NIF_TIP = 0x00000004

_IDI_APPLICATION = 32512
_PROBE_UID = 0x5041444C  # "PADL"

_WNDPROC = ctypes.WINFUNCTYPE(
    ctypes.c_ssize_t,
    wintypes.HWND,
    wintypes.UINT,
    wintypes.WPARAM,
    wintypes.LPARAM,
)

HCURSOR = getattr(wintypes, "HCURSOR", wintypes.HANDLE)


class _WNDCLASSW(ctypes.Structure):
    _fields_ = [
        ("style", wintypes.UINT),
        ("lpfnWndProc", _WNDPROC),
        ("cbClsExtra", ctypes.c_int),
        ("cbWndExtra", ctypes.c_int),
        ("hInstance", wintypes.HINSTANCE),
        ("hIcon", wintypes.HICON),
        ("hCursor", HCURSOR),
        ("hbrBackground", wintypes.HBRUSH),
        ("lpszMenuName", wintypes.LPCWSTR),
        ("lpszClassName", wintypes.LPCWSTR),
    ]


class _NOTIFYICONDATAW(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.DWORD),
        ("hWnd", wintypes.HWND),
        ("uID", wintypes.UINT),
        ("uFlags", wintypes.UINT),
        ("uCallbackMessage", wintypes.UINT),
        ("hIcon", wintypes.HICON),
        ("szTip", wintypes.WCHAR * 128),
        ("dwState", wintypes.DWORD),
        ("dwStateMask", wintypes.DWORD),
        ("szInfo", wintypes.WCHAR * 256),
        ("uTimeoutOrVersion", wintypes.UINT),
        ("szInfoTitle", wintypes.WCHAR * 64),
        ("dwInfoFlags", wintypes.DWORD),
    ]


def supported() -> bool:
    return sys.platform == "win32"


_winproc_ref: _WNDPROC | None = None
_class_atom: int | None = None


def _register_class(user32: ctypes.WinDLL) -> int | None:
    """注册（一次）message-only 窗口类，返回 atom；失败返回 None。"""
    global _winproc_ref, _class_atom
    if _class_atom is not None:
        return _class_atom

    wndclass = _WNDCLASSW()
    wndclass.style = 0
    wndclass.lpfnWndProc = _WNDPROC(_noop_wndproc)
    wndclass.cbClsExtra = 0
    wndclass.cbWndExtra = 0
    kernel32 = ctypes.windll.kernel32
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    wndclass.hInstance = kernel32.GetModuleHandleW(None)
    wndclass.hIcon = None
    wndclass.hCursor = None
    wndclass.hbrBackground = None
    wndclass.lpszMenuName = None
    wndclass.lpszClassName = "PadLendTrayProbe"
    _winproc_ref = wndclass.lpfnWndProc
    user32.RegisterClassW.argtypes = [ctypes.POINTER(_WNDCLASSW)]
    user32.RegisterClassW.restype = wintypes.ATOM
    if not user32.RegisterClassW(ctypes.byref(wndclass)):
        return None
    _class_atom = 1
    return _class_atom


def _noop_wndproc(hwnd, msg, wparam, lparam):  # pragma: no cover - 只收消息
    user32 = ctypes.windll.user32
    user32.DefWindowProcW.argtypes = [
        wintypes.HWND,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    user32.DefWindowProcW.restype = ctypes.c_ssize_t
    return user32.DefWindowProcW(hwnd, msg, wparam, lparam)


def _open_window() -> int:
    user32 = ctypes.windll.user32
    if _register_class(user32) is None:
        return 0
    kernel32 = ctypes.windll.kernel32
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    user32.CreateWindowExW.restype = wintypes.HWND
    user32.CreateWindowExW.argtypes = [
        wintypes.DWORD,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        wintypes.HWND,
        wintypes.HMENU,
        wintypes.HINSTANCE,
        ctypes.c_void_p,
    ]
    user32.DestroyWindow.argtypes = [wintypes.HWND]
    hwnd = user32.CreateWindowExW(
        0,
        "PadLendTrayProbe",
        "PadLendTrayProbe",
        0,
        0,
        0,
        0,
        0,
        wintypes.HWND(-3),  # HWND_MESSAGE
        None,
        kernel32.GetModuleHandleW(None),
        None,
    )
    return int(hwnd or 0)


def probe_icon() -> tuple[bool, int]:
    """加一个探针图标再立刻删掉，返回 ``(Shell_NotifyIcon 成功, GetLastError)``。

    失败时 ``GetLastError`` 未必有意义（Shell_NotifyIcon 的文档说明它不保证
    设置 last error），但成功与否是可信的。
    """
    if not supported():
        return (True, 0)

    shell32 = ctypes.windll.shell32
    user32 = ctypes.windll.user32
    hwnd = _open_window()
    if not hwnd:
        return (False, int(ctypes.windll.kernel32.GetLastError() or 0))

    data = _NOTIFYICONDATAW()
    data.cbSize = ctypes.sizeof(_NOTIFYICONDATAW)
    data.hWnd = wintypes.HWND(hwnd)
    data.uID = _PROBE_UID
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP
    data.uCallbackMessage = _WM_TRAY_PROBE
    user32.LoadIconW.restype = wintypes.HICON
    data.hIcon = user32.LoadIconW(None, wintypes.LPCWSTR(_IDI_APPLICATION))
    shell32.Shell_NotifyIconW.restype = wintypes.BOOL
    data.szTip = "PadLend tray probe"

    ok = bool(shell32.Shell_NotifyIconW(NIM_ADD, ctypes.byref(data)))
    err = int(ctypes.windll.kernel32.GetLastError() or 0)
    if ok:
        shell32.Shell_NotifyIconW(NIM_DELETE, ctypes.byref(data))
    user32.DestroyWindow(hwnd)
    return (ok, err)


def registered_paths() -> list[str]:
    """读 Windows 给托盘图标记的 ``ExecutablePath``（Win10 1607+）。

    Windows 在 ``HKCU\\Control Panel\\NotifyIconSettings`` 下给每个挂上过的
    通知区图标建一条记录；读它可以看出系统有没有登记我们。
    """
    if not supported():
        return []
    try:
        import winreg
    except ImportError:  # pragma: no cover - 仅非 Windows
        return []

    paths: list[str] = []
    try:
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER, r"Control Panel\NotifyIconSettings"
        ) as root:
            index = 0
            while True:
                try:
                    name = winreg.EnumKey(root, index)
                except OSError:
                    break
                index += 1
                try:
                    with winreg.OpenKey(root, name) as child:
                        value, _ = winreg.QueryValueEx(child, "ExecutablePath")
                except OSError:
                    continue
                if isinstance(value, str) and value:
                    paths.append(value)
    except OSError:
        return []
    return paths
