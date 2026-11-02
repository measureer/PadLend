"""单实例互斥量（设计文档 7.5）。

第一个进程 ``acquire()`` 建立 ``Global\\PadLend.SingleInstance`` 并返回
``True``；后启动的进程会看到 ``ERROR_ALREADY_EXISTS``，返回 ``False``，
由调用方唤醒已有实例后退出。互斥量只用于"是否存在"，不做 ``WaitForSingleObject``
等待，所以句柄持有到 :func:`release` 或进程结束即可。

导入本模块不会创建任何内核对象；所有 win32 调用都是惰性加载。
"""

from __future__ import annotations

import ctypes
import logging
import threading
from ctypes import c_int32, c_void_p, c_wchar_p
from functools import lru_cache
from typing import Any

import constants as C
from core.errors import ErrorCode, PadLendError

log = logging.getLogger("win.single_instance")

__all__ = ["ERROR_ALREADY_EXISTS", "acquire", "already_running", "release"]

ERROR_ALREADY_EXISTS = 183
ERROR_FILE_NOT_FOUND = 2
SYNCHRONIZE = 0x00100000

_handle: int | None = None
_held_name: str | None = None
_lock = threading.RLock()


@lru_cache(maxsize=1)
def _kernel32() -> Any:
    try:
        return ctypes.WinDLL("kernel32", use_last_error=True)
    except (AttributeError, OSError) as exc:
        raise PadLendError(
            ErrorCode.UNEXPECTED, f"无法加载 kernel32.dll: {exc}", cause=exc
        ) from exc


def _bind() -> Any:
    kernel32 = _kernel32()
    kernel32.CreateMutexW.argtypes = [c_void_p, c_int32, c_wchar_p]
    kernel32.CreateMutexW.restype = c_void_p
    kernel32.OpenMutexW.argtypes = [c_int32, c_int32, c_wchar_p]
    kernel32.OpenMutexW.restype = c_void_p
    kernel32.CloseHandle.argtypes = [c_void_p]
    kernel32.CloseHandle.restype = c_int32
    return kernel32


def acquire(name: str = C.SINGLE_INSTANCE_MUTEX) -> bool:
    """尝试成为唯一实例。

    返回 ``True`` 表示本进程持有互斥量；``False`` 表示已有实例在运行。
    重复调用是幂等的：持有同一名称时再次调用仍返回 ``True``。
    """
    global _handle, _held_name

    with _lock:
        if _handle is not None:
            if _held_name == name:
                return True
            raise PadLendError(
                ErrorCode.UNEXPECTED,
                f"本进程已持有互斥量 {_held_name!r}，不能再获取 {name!r}",
            )

        kernel32 = _bind()
        ctypes.set_last_error(0)
        handle = kernel32.CreateMutexW(None, False, name)
        last_error = ctypes.get_last_error()
        if not handle:
            raise PadLendError(
                ErrorCode.UNEXPECTED,
                f"CreateMutexW({name!r}) 失败: GetLastError={last_error}",
            )
        if last_error == ERROR_ALREADY_EXISTS:
            kernel32.CloseHandle(handle)
            log.info("检测到已有实例（互斥量 %s）", name)
            return False
        _handle = int(handle)
        _held_name = name
        return True


def release() -> None:
    """释放本进程持有的互斥量；未持有时是空操作（幂等）。"""
    global _handle, _held_name

    with _lock:
        if _handle is None:
            return
        kernel32 = _bind()
        kernel32.CloseHandle(_handle)
        _handle = None
        _held_name = None


def already_running(name: str = C.SINGLE_INSTANCE_MUTEX) -> bool:
    """查询是否已有**其他**实例持有该互斥量。

    本进程自己持有时返回 ``False``（否则调用方会把自身当成"已有实例"）。
    互斥量不存在返回 ``False``；其他查询错误抛 ``E900``。
    """
    with _lock:
        if _handle is not None and _held_name == name:
            return False
        kernel32 = _bind()
        ctypes.set_last_error(0)
        handle = kernel32.OpenMutexW(SYNCHRONIZE, False, name)
        last_error = ctypes.get_last_error()
        if handle:
            kernel32.CloseHandle(handle)
            return True
        if last_error == ERROR_FILE_NOT_FOUND:
            return False
        raise PadLendError(
            ErrorCode.UNEXPECTED,
            f"OpenMutexW({name!r}) 失败: GetLastError={last_error}",
        )
