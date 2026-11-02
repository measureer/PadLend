"""首连等待的取消路径（GUI 退出不能假死 40s）。"""

from __future__ import annotations

import threading
import time

import pytest

import constants as C
from core.errors import ErrorCode, PadLendError
from tablet.adb import Adb
from tablet.session import TabletSession


def test_wait_for_device_cancel_interrupts_retry_loop(monkeypatch: pytest.MonkeyPatch) -> None:
    adb = Adb(adb_path="adb")
    calls = 0

    def fake_select(serial: str | None = None, *, prefer_usb: bool = True):
        nonlocal calls
        calls += 1
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, "没有设备")

    monkeypatch.setattr(adb, "select_device", fake_select)
    monkeypatch.setattr(C, "INITIAL_RETRY_INTERVAL_S", 0.05)

    cancel = threading.Event()
    result: list[BaseException] = []

    def wait() -> None:
        try:
            adb.wait_for_device(40.0, cancel=cancel.is_set)
        except BaseException as exc:  # noqa: BLE001 - 测试记录异常类型
            result.append(exc)

    worker = threading.Thread(target=wait, daemon=True)
    started = time.monotonic()
    worker.start()
    cancel.set()
    worker.join(2.0)

    assert not worker.is_alive(), "cancel 后等待循环必须尽快退出"
    assert time.monotonic() - started < 1.5
    assert calls >= 1
    assert len(result) == 1
    assert isinstance(result[0], PadLendError)
    assert result[0].code is ErrorCode.DEVICE_OFFLINE
    assert "取消" in result[0].detail


def test_session_stop_sets_cancel_before_taking_lifecycle_lock(monkeypatch) -> None:
    session = TabletSession()
    release = threading.Event()
    locked = threading.Event()

    def hold_lock() -> None:
        with session._lock:  # noqa: SLF001 - 故意模拟 start() 长期持锁
            locked.set()
            release.wait(3.0)

    holder = threading.Thread(target=hold_lock, daemon=True)
    holder.start()
    assert locked.wait(1.0)

    stopper = threading.Thread(target=session.stop, daemon=True)
    stopper.start()
    # stop() 应先把 _closing 置位再去竞争生命周期锁。
    assert session._closing.wait(1.0)  # noqa: SLF001
    assert stopper.is_alive(), "锁被占用时 stop() 应停在阻塞等待"

    release.set()
    holder.join(2.0)
    stopper.join(2.0)
    assert not stopper.is_alive()
