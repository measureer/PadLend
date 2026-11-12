"""DPI 感知、光标位置与显示器基础查询（设计文档 6.3.1 / 13.1）。

本模块只封装 Windows 的少量查询接口：

- :func:`set_dpi_awareness`：在创建任何窗口前声明 ``PER_MONITOR_AWARE_V2``；
  新系统失败时回退 ``SetProcessDPIAware``。
- :func:`get_monitor_dpi`：``GetDpiForMonitor(MDT_EFFECTIVE_DPI)``。
- :func:`get_cursor_pos`：``GetCursorPos``（DPI 感知下是物理像素）。
- :func:`get_monitor_rect`：``GetMonitorInfoW`` 的 ``rcMonitor``。

约定（与 ``win/desktop.py`` 不同）：本模块的查询在失败时**只做安全回退，不抛
异常**，避免 GUI 的 DPI 探测把整个进程拖垮。需要严格错误处理的多显示器枚举
在 :mod:`win.desktop` 中完成。

``import win.dpi`` 没有副作用：不加载 DLL、不创建窗口、不改进程 DPI 状态；
所有 DLL 句柄都在函数首次调用时才解析并缓存。
"""

from __future__ import annotations

import ctypes
import logging
from ctypes import POINTER, byref, c_int32, c_long, c_uint, c_void_p
from functools import lru_cache
from typing import Any

import constants as C
from core.geometry import Rect

log = logging.getLogger("win.dpi")

# MONITOR_DPI_TYPE（shcore.h）
MDT_EFFECTIVE_DPI = 0

DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4


class _RECT(ctypes.Structure):
    _fields_ = [
        ("left", c_int32),
        ("top", c_int32),
        ("right", c_int32),
        ("bottom", c_int32),
    ]


class _POINT(ctypes.Structure):
    _fields_ = [
        ("x", c_int32),
        ("y", c_int32),
    ]


class _MONITORINFO(ctypes.Structure):
    _fields_ = [
        ("cbSize", c_int32),
        ("rcMonitor", _RECT),
        ("rcWork", _RECT),
        ("dwFlags", c_int32),
    ]


@lru_cache(maxsize=1)
def _user32() -> Any | None:
    """延迟加载 ``user32.dll``；失败返回 ``None``（按本模块约定不抛）。"""
    try:
        return ctypes.WinDLL("user32", use_last_error=True)
    except (AttributeError, OSError) as exc:  # 非 Windows / 系统损坏
        log.debug("加载 user32.dll 失败: %s", exc)
        return None


@lru_cache(maxsize=1)
def _shcore() -> Any | None:
    """延迟加载 ``shcore.dll``（Windows 8.1+ 的 DPI 接口）。"""
    try:
        return ctypes.WinDLL("shcore", use_last_error=True)
    except (AttributeError, OSError) as exc:
        log.debug("加载 shcore.dll 失败: %s", exc)
        return None


def set_dpi_awareness() -> bool:
    """声明进程 DPI 感知（必须在创建任何窗口之前调用）。

    优先 ``SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)``；旧系统或
    系统已锁定 DPI 感知导致失败时回退 ``SetProcessDPIAware()``。
    返回是否至少有一个调用成功；任何失败都不抛异常。
    """
    user32 = _user32()
    if user32 is None:
        return False

    try:
        set_context = user32.SetProcessDpiAwarenessContext
        set_context.argtypes = [c_void_p]
        set_context.restype = c_int32
        context = c_void_p(C.DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        if set_context(context):
            return True
        log.debug(
            "SetProcessDpiAwarenessContext(-4) 失败: %s", ctypes.get_last_error()
        )
    except (AttributeError, OSError, ValueError, OverflowError) as exc:
        log.debug("SetProcessDpiAwarenessContext 不可用: %s", exc)

    try:
        set_legacy = user32.SetProcessDPIAware
        set_legacy.argtypes = []
        set_legacy.restype = c_int32
        if set_legacy():
            return True
        log.debug("SetProcessDPIAware 失败: %s", ctypes.get_last_error())
    except (AttributeError, OSError, ValueError) as exc:
        log.warning("设置进程 DPI 感知失败: %s", exc)

    return False


def get_monitor_dpi(hmon: int) -> int:
    """返回某台显示器的有效 DPI（96 / 120 / 144 ...）。

    查询失败（含 Windows 7 没有 shcore）时回退 96，不抛异常。
    """
    shcore = _shcore()
    if shcore is None:
        return 96
    try:
        get_dpi = shcore.GetDpiForMonitor
        get_dpi.argtypes = [c_void_p, c_int32, POINTER(c_uint), POINTER(c_uint)]
        get_dpi.restype = c_long
        dpi_x = c_uint(0)
        dpi_y = c_uint(0)
        hresult = get_dpi(c_void_p(hmon), MDT_EFFECTIVE_DPI, byref(dpi_x), byref(dpi_y))
        if hresult == 0 and dpi_x.value > 0:
            return int(dpi_x.value)
        log.debug("GetDpiForMonitor(0x%X) 失败: HRESULT=0x%08X", hmon, hresult & 0xFFFFFFFF)
    except (AttributeError, OSError, ValueError) as exc:
        log.debug("GetDpiForMonitor 不可用: %s", exc)
    return 96


def try_get_cursor_pos() -> tuple[int, int] | None:
    """返回光标的物理像素坐标；查询失败返回 ``None``（不抛异常）。"""
    user32 = _user32()
    if user32 is None:
        return None
    try:
        get_cursor_pos = user32.GetCursorPos
        get_cursor_pos.argtypes = [POINTER(_POINT)]
        get_cursor_pos.restype = c_int32
        point = _POINT()
        if get_cursor_pos(byref(point)):
            return (int(point.x), int(point.y))
        log.debug("GetCursorPos 失败: %s", ctypes.get_last_error())
    except (AttributeError, OSError, ValueError) as exc:
        log.debug("GetCursorPos 不可用: %s", exc)
    return None


def get_cursor_pos() -> tuple[int, int]:
    """返回光标的物理像素坐标；失败回退 ``(0, 0)``（不抛异常）。"""
    return try_get_cursor_pos() or (0, 0)


def set_cursor_pos(x: int, y: int) -> bool:
    """把光标移到物理像素坐标；成功返回 ``True``。"""
    user32 = _user32()
    if user32 is None:
        return False
    try:
        setter = user32.SetCursorPos
        setter.argtypes = [ctypes.c_int, ctypes.c_int]
        setter.restype = ctypes.c_int
        return bool(setter(int(x), int(y)))
    except (AttributeError, OSError, ValueError) as exc:
        log.debug("SetCursorPos 不可用: %s", exc)
        return False


def get_monitor_rect(hmon: int) -> Rect | None:
    """返回某台显示器的物理矩形（``rcMonitor``）；失败返回 ``None``。"""
    user32 = _user32()
    if user32 is None or not hmon:
        return None
    try:
        get_info = user32.GetMonitorInfoW
        get_info.argtypes = [c_void_p, c_void_p]
        get_info.restype = c_int32
        info = _MONITORINFO()
        info.cbSize = ctypes.sizeof(_MONITORINFO)
        if not get_info(c_void_p(hmon), byref(info)):
            log.debug("GetMonitorInfoW(0x%X) 失败: %s", hmon, ctypes.get_last_error())
            return None
        rect = info.rcMonitor
        return Rect(int(rect.left), int(rect.top), int(rect.right), int(rect.bottom))
    except (AttributeError, OSError, ValueError) as exc:
        log.debug("GetMonitorInfoW 不可用: %s", exc)
        return None
