"""1.2.0 退出/失效安全回归：teardown 幂等、父进程看门狗、首次运行不误报。"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
import threading
from pathlib import Path
from types import SimpleNamespace

import constants as C
import main
import service.api as service_api
import service.server as server
from config import Config
from service.api import PadLendService


class _Counter:
    def __init__(self) -> None:
        self.value = 0
        self.lock = threading.Lock()

    def bump(self) -> None:
        with self.lock:
            self.value += 1


def _runtime_with_fakes() -> tuple[main.Runtime, dict[str, _Counter]]:
    runtime = main.Runtime.__new__(main.Runtime)
    runtime._stop_lock = threading.Lock()
    runtime._stopped = False
    counts = {
        name: _Counter()
        for name in (
            "hook",
            "router",
            "raw",
            "session",
            "mask",
            "adb",
        )
    }
    runtime.keyboard_hook = SimpleNamespace(uninstall=counts["hook"].bump)
    runtime.router = SimpleNamespace(stop=counts["router"].bump)
    runtime.cursor_watch = None
    runtime.rawinput = SimpleNamespace(stop=counts["raw"].bump)
    runtime.session = SimpleNamespace(stop=counts["session"].bump)
    runtime.mask = SimpleNamespace(release=counts["mask"].bump)
    runtime.raw_debug = None
    runtime.adb = SimpleNamespace(kill_server=counts["adb"].bump)
    return runtime, counts


def test_runtime_stop_is_concurrent_idempotent() -> None:
    runtime, counts = _runtime_with_fakes()
    threads = [threading.Thread(target=runtime.stop) for _ in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(2)

    assert all(not thread.is_alive() for thread in threads)
    assert {name: counter.value for name, counter in counts.items()} == {
        "hook": 1,
        "router": 1,
        "raw": 1,
        "session": 1,
        "mask": 1,
        "adb": 1,
    }


def test_service_shutdown_is_idempotent(monkeypatch) -> None:
    calls: list[str] = []
    svc = PadLendService(lambda *_: None)
    svc.hotkeys = SimpleNamespace(stop=lambda: calls.append("hotkeys"))
    svc.runtime = SimpleNamespace(stop=lambda: calls.append("runtime"))
    monkeypatch.setattr(service_api, "_mark_clean_exit", lambda: calls.append("mark"))

    threads = [threading.Thread(target=svc.shutdown) for _ in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(2)
    svc.shutdown()

    assert calls == ["hotkeys", "runtime", "mark"]
    assert all(not thread.is_alive() for thread in threads)


def test_parent_alive_detects_self_and_exited_child() -> None:
    assert server._parent_alive(os.getpid()) is True

    child = subprocess.Popen([sys.executable, "-c", "pass"])
    child.wait(timeout=10)
    assert server._parent_alive(child.pid) is False


def test_parent_watchdog_tears_down_when_parent_disappears(monkeypatch) -> None:
    calls: list[str] = []
    exited = threading.Event()
    monkeypatch.setattr(server, "_parent_alive", lambda _pid: False)

    def fake_exit(_code: int) -> None:
        exited.set()

    monkeypatch.setattr(server.os, "_exit", fake_exit)
    service = SimpleNamespace(shutdown=lambda: calls.append("shutdown"))

    thread = server._start_parent_watchdog(service, 12345, interval=0.01)
    try:
        assert exited.wait(2.0)
    finally:
        thread.join(2.0)

    assert calls == ["shutdown"]
    assert not thread.is_alive()


def test_stale_cleanup_first_run_does_not_report_crash(
    tmp_path: Path, monkeypatch, caplog
) -> None:
    marker = tmp_path / "exit.clean"
    config = tmp_path / "config.yaml"
    config.write_text("version: 1\n", encoding="utf-8")
    monkeypatch.setattr(C, "clean_exit_marker", lambda: marker)
    monkeypatch.setattr(C, "config_path", lambda: config)
    monkeypatch.setattr(C, "config_dir", lambda: tmp_path)
    monkeypatch.setattr("win.mask.cleanup_orphan_mask", lambda: None)
    monkeypatch.setattr("tablet.session.load_pointer_state", lambda: None)

    with caplog.at_level(logging.WARNING, logger="padlend.main"):
        main._stale_cleanup(Config(), first_run=True)

    assert "检测到上次未正常退出" not in caplog.text
    assert marker.exists()
