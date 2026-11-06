"""单实例互斥量（设计文档 7.5）。

第一个进程 ``acquire()`` 建立 ``Global\\PadLend.SingleInstance`` 并返回
``True``；后启动的进程会看到 ``ERROR_ALREADY_EXISTS``，返回 ``False``，
由调用方唤醒已有实例后退出。互斥量只用于"是否存在"，不做 ``WaitForSingleObject``
等待，所以句柄持有到 :func:`release` 或进程结束即可。

导入本模块不会创建任何内核对象；所有 win32 调用都是惰性加载。
"""

from __future__ import annotations

import contextlib
import ctypes
import logging
import sys
import threading
import time
from collections.abc import Callable
from ctypes import c_int32, c_uint32, c_void_p, c_wchar_p
from functools import lru_cache
from typing import Any

import constants as C
from core.errors import ErrorCode, PadLendError

log = logging.getLogger("win.single_instance")

__all__ = [
    "ERROR_ALREADY_EXISTS",
    "acquire",
    "already_running",
    "create_activation_event",
    "release",
    "signal_running_instance",
    "start_activation_watcher",
    "stop_activation_watcher",
]

ERROR_ALREADY_EXISTS = 183
ERROR_FILE_NOT_FOUND = 2
SYNCHRONIZE = 0x00100000
EVENT_MODIFY_STATE = 0x0002
_WAIT_OBJECT_0 = 0
_WAIT_TIMEOUT = 258

_handle: int | None = None
_held_name: str | None = None
_lock = threading.RLock()

# 第二实例 → 第一实例的“请把窗口拿到前台”命名事件。
_activation_handle: int | None = None
_activation_stop: threading.Event | None = None
_activation_thread: threading.Thread | None = None


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
    kernel32.OpenEventW.argtypes = [c_uint32, c_int32, c_wchar_p]
    kernel32.OpenEventW.restype = c_void_p
    kernel32.CreateEventW.argtypes = [c_void_p, c_int32, c_int32, c_wchar_p]
    kernel32.CreateEventW.restype = c_void_p
    kernel32.SetEvent.argtypes = [c_void_p]
    kernel32.SetEvent.restype = c_int32
    kernel32.WaitForSingleObject.argtypes = [c_void_p, c_uint32]
    kernel32.WaitForSingleObject.restype = c_uint32
    kernel32.CloseHandle.argtypes = [c_void_p]
    kernel32.CloseHandle.restype = c_int32
    return kernel32


def create_activation_event(name: str = C.SINGLE_INSTANCE_EVENT) -> bool:
    """由首实例创建“唤到前台”命名事件；重复调用幂等。

    ``CreateEventW`` 创建的是自动复位事件：第二实例 ``SetEvent`` 后，
    首实例的等待线程唤醒一次，事件自动回到无信号状态。
    """
    global _activation_handle

    if sys.platform != "win32":
        return False
    with _lock:
        if _activation_handle is not None:
            return True
        kernel32 = _bind()
        ctypes.set_last_error(0)
        handle = kernel32.CreateEventW(None, False, False, name)
        last_error = ctypes.get_last_error()
        if not handle:
            log.warning("CreateEventW(%r) 失败: GetLastError=%d", name, last_error)
            return False
        _activation_handle = int(handle)
        log.debug("已创建单实例唤起事件 %s", name)
        return True


def _allow_foreground() -> None:
    """允许首实例把窗口抢到前台；失败也不影响事件通知。"""
    with contextlib.suppress(AttributeError, OSError):
        ctypes.windll.user32.AllowSetForegroundWindow(-1)  # type: ignore[attr-defined]


def signal_running_instance(
    name: str = C.SINGLE_INSTANCE_EVENT, *, timeout: float = 2.0
) -> bool:
    """第二实例：通知首实例显示窗口。

    首实例可能刚启动、事件还没建好，所以在一个短窗口内重试
    ``OpenEventW`。旧版本没有这个事件时返回 ``False``，调用方再退回
    按窗口标题查找。
    """
    if sys.platform != "win32":
        return False
    kernel32 = _bind()
    deadline = time.monotonic() + max(0.1, float(timeout))
    while True:
        ctypes.set_last_error(0)
        handle = kernel32.OpenEventW(EVENT_MODIFY_STATE, False, name)
        if handle:
            try:
                _allow_foreground()
                if not kernel32.SetEvent(handle):
                    log.debug(
                        "SetEvent(%r) 失败: GetLastError=%d",
                        name,
                        ctypes.get_last_error(),
                    )
                    return False
                return True
            finally:
                kernel32.CloseHandle(handle)
        last_error = ctypes.get_last_error()
        if last_error != ERROR_FILE_NOT_FOUND:
            log.debug("OpenEventW(%r) 失败: GetLastError=%d", name, last_error)
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)


def _activation_loop(
    handle: int, stop: threading.Event, callback: Callable[[], None]
) -> None:
    kernel32 = _bind()
    while not stop.is_set():
        result = kernel32.WaitForSingleObject(ctypes.c_void_p(handle), 250)
        if result == _WAIT_OBJECT_0:
            try:
                callback()
            except Exception:  # noqa: BLE001 - 唤起失败不能让后台线程退出
                log.exception("处理单实例唤起事件失败")
        elif result == _WAIT_TIMEOUT:
            continue
        else:
            log.warning("等待单实例唤起事件失败: %d", result)
            return


def start_activation_watcher(
    callback: Callable[[], None], name: str = C.SINGLE_INSTANCE_EVENT
) -> bool:
    """首实例：在后台线程等待第二实例的唤起事件。"""
    global _activation_stop, _activation_thread

    if sys.platform != "win32":
        return False
    if not create_activation_event(name):
        return False
    with _lock:
        if _activation_thread is not None and _activation_thread.is_alive():
            return True
        stop = threading.Event()
        thread = threading.Thread(
            target=_activation_loop,
            args=(int(_activation_handle or 0), stop, callback),
            name="padlend-activate",
            daemon=True,
        )
        _activation_stop = stop
        _activation_thread = thread
        thread.start()
        return True


def stop_activation_watcher() -> None:
    """停止唤起监听并关闭事件句柄；未启动时是空操作。"""
    global _activation_handle, _activation_stop, _activation_thread

    with _lock:
        stop = _activation_stop
        thread = _activation_thread
        handle = _activation_handle
        _activation_stop = None
        _activation_thread = None
    if stop is not None:
        stop.set()
    if thread is not None and thread.is_alive():
        thread.join(1.5)
    if handle is not None:
        with _lock:
            if _activation_handle == handle:
                try:
                    _bind().CloseHandle(handle)
                except Exception:  # noqa: BLE001 - 退出路径不再抛
                    log.debug("关闭唤起事件失败", exc_info=True)
                _activation_handle = None


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
    """释放本进程持有的互斥量与唤起事件；未持有时是空操作（幂等）。"""
    global _handle, _held_name

    stop_activation_watcher()
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
