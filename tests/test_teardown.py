"""1.2.0 退出/失效安全回归：teardown 幂等、父进程看门狗、首次运行不误报。"""

from __future__ import annotations

import json
import logging
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

import constants as C
import main
import service.api as service_api
import service.server as server
from config import Config
from service.api import PadLendService

ROOT = Path(__file__).resolve().parent.parent


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


_TEARDOWN_ORDER = (
    "hook",
    "router",
    "cursor_watch",
    "rawinput",
    "session",
    "mask",
    "raw_debug",
    "adb",
)


def _runtime_with_one_failing_step(
    fail_step: str,
    error_type: type[BaseException] = RuntimeError,
) -> tuple[main.Runtime, list[str]]:
    runtime = main.Runtime.__new__(main.Runtime)
    runtime._stop_lock = threading.Lock()
    runtime._stopped = False
    seen: list[str] = []

    def action(name: str):
        def run() -> None:
            seen.append(name)
            if name == fail_step:
                raise error_type(f"{name} failed")

        return run

    runtime.keyboard_hook = SimpleNamespace(uninstall=action("hook"))
    runtime.router = SimpleNamespace(stop=action("router"))
    runtime.cursor_watch = SimpleNamespace(stop=action("cursor_watch"))
    runtime.rawinput = SimpleNamespace(stop=action("rawinput"))
    runtime.session = SimpleNamespace(stop=action("session"))
    runtime.mask = SimpleNamespace(release=action("mask"))
    runtime.raw_debug = SimpleNamespace(stop=action("raw_debug"))
    runtime.adb = SimpleNamespace(kill_server=action("adb"))
    return runtime, seen


@pytest.mark.parametrize("fail_step", _TEARDOWN_ORDER)
def test_runtime_stop_releases_every_resource_when_one_step_fails(fail_step: str) -> None:
    """任一清理步骤抛异常都不能让后面的资源泄漏。

    adb.kill_server 的异常本来就被吞掉；其余步骤的异常会继续向上抛，
    但必须先把 hook / 遮罩 / session / adb 等全部释放完。
    """
    runtime, seen = _runtime_with_one_failing_step(fail_step)

    if fail_step == "adb":
        runtime.stop()
    else:
        with pytest.raises(RuntimeError, match=fail_step):
            runtime.stop()

    assert seen == list(_TEARDOWN_ORDER)

    # 已经进入过 teardown（哪怕中途有异常）后，再调用不得重复释放资源。
    runtime.stop()
    assert seen == list(_TEARDOWN_ORDER)


def test_runtime_stop_cleans_up_before_keyboard_interrupt() -> None:
    """Ctrl+C 落在某一步清理中时，也不能把后续资源留在半释放状态。"""
    runtime, seen = _runtime_with_one_failing_step("cursor_watch", KeyboardInterrupt)

    with pytest.raises(KeyboardInterrupt):
        runtime.stop()

    assert seen == list(_TEARDOWN_ORDER)


def test_sidecar_exits_when_parent_process_dies(tmp_path: Path) -> None:
    """真实父子进程：父进程被杀后 sidecar 必须 teardown 并以 0 退出。

    现有看门狗测试把 _parent_alive 和 os._exit 都替换掉了；这里启动真实
    sleeper 父进程和真实 sidecar，只保留程序自己的父子存活检测与退出路径。
    """
    parent = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(60)"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    env = dict(os.environ)
    env.update(
        {
            "PADLEND_HOME": str(tmp_path),
            "PADLEND_PARENT_PID": str(parent.pid),
            "PYTHONIOENCODING": "utf-8",
            "PYTHONUNBUFFERED": "1",
        }
    )
    stderr = (tmp_path / "sidecar-stderr.log").open("w", encoding="utf-8")
    proc: subprocess.Popen[str] | None = None
    try:
        proc = subprocess.Popen(
            [sys.executable, "-m", "service", "--simulate", "--no-hooks"],
            cwd=ROOT,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        assert proc.stdout is not None
        stdout = proc.stdout
        lines: queue.Queue[str] = queue.Queue()

        def pump() -> None:
            try:
                for line in stdout:
                    lines.put(line)
            finally:
                lines.put("")

        threading.Thread(target=pump, name="sidecar-stdout", daemon=True).start()
        deadline = time.monotonic() + 20
        ready = False
        while time.monotonic() < deadline:
            try:
                line = lines.get(timeout=0.1)
            except queue.Empty:
                continue
            if not line:
                break
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            if payload.get("event") == "ready":
                ready = True
                break
        assert ready, "sidecar 未在超时内发出 ready 事件"

        parent.terminate()
        parent.wait(timeout=10)
        proc.wait(timeout=15)

        assert proc.returncode == 0
        assert (tmp_path / "exit.clean").exists()
    finally:
        if parent.poll() is None:
            parent.kill()
            parent.wait(timeout=5)
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5)
        stderr.close()
