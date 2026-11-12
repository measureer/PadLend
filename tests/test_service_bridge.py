"""Tauri 业务服务的端到端协议测试（真实子进程 + 假平板，不碰硬件）。

验证了「保留业务接口」：配置读写/校验、设备扫描、桌面几何、布局保存、
诊断采集导出、向导检测、引擎状态与事件推送都走同一份 Python 引擎实现。
"""

from __future__ import annotations

import contextlib
import json
import os
import queue
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parent.parent


class ServiceClient:
    def __init__(self, home: Path) -> None:
        env = dict(os.environ)
        env["PADLEND_HOME"] = str(home)
        env["PYTHONIOENCODING"] = "utf-8"
        env["PYTHONUNBUFFERED"] = "1"
        self.stderr_path = home / "service-stderr.log"
        self._stderr = self.stderr_path.open("w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [sys.executable, "-m", "service", "--simulate", "--no-hooks"],
            cwd=ROOT,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._id = 0
        self._lock = threading.Lock()
        self._responses: dict[int, dict[str, Any]] = {}
        self._events: list[tuple[str, Any]] = []
        self._lines: queue.Queue[str] = queue.Queue()
        self._ready = threading.Event()
        threading.Thread(target=self._read, daemon=True).start()
        assert self._ready.wait(15), "服务未发出 ready 事件"

    def _read(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in payload and payload["id"] != 0:
                with self._lock:
                    self._responses[int(payload["id"])] = payload
            elif "event" in payload:
                with self._lock:
                    self._events.append((payload["event"], payload.get("data")))
                if payload["event"] == "ready":
                    self._ready.set()
            self._lines.put(line)

    def call(
        self, method: str, params: dict[str, Any] | None = None, timeout: float = 30.0
    ) -> dict[str, Any]:
        with self._lock:
            self._id += 1
            request_id = self._id
        assert self.proc.stdin is not None
        self.proc.stdin.write(
            json.dumps(
                {"id": request_id, "method": method, "params": params or {}}, ensure_ascii=False
            )
            + "\n"
        )
        self.proc.stdin.flush()
        deadline = __import__("time").monotonic() + timeout
        while __import__("time").monotonic() < deadline:
            with self._lock:
                if request_id in self._responses:
                    return self._responses.pop(request_id)
            __import__("time").sleep(0.02)
        raise AssertionError(f"RPC 超时：{method}")

    def ok(self, method: str, params: dict[str, Any] | None = None, timeout: float = 30.0) -> Any:
        response = self.call(method, params, timeout=timeout)
        assert response.get("ok"), f"{method} 失败：{response.get('error')}"
        return response["result"]

    def events(self, name: str) -> list[Any]:
        with self._lock:
            return [data for event, data in self._events if event == name]

    def close(self) -> None:
        with contextlib.suppress(AssertionError, OSError):
            self.ok("app.shutdown", timeout=5)
        try:
            self.proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self._stderr.close()


@pytest.fixture(scope="module")
def client(tmp_path_factory: pytest.TempPathFactory):
    home = tmp_path_factory.mktemp("padlend-service")
    instance = ServiceClient(home)
    yield instance, home
    instance.close()


def test_bootstrap_keeps_python_config_contract(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    data = svc.ok("app.bootstrap")
    assert data["version"]
    cfg = data["config"]
    assert cfg["version"] == 1
    assert cfg["motion"]["report_hz"] == 1000
    assert cfg["hotkeys"] == {"toggle": "Ctrl+Alt+S", "panic": "Ctrl+Alt+Q"}
    assert cfg["misc"]["clipboard_sync"] is True
    assert data["desktop"]["monitors"], "应能枚举真实显示器"
    assert data["desktop"]["segments"], "应能计算外轮廓边段"
    assert data["placement"]["edge"] in {"left", "right", "top", "bottom"}


def test_config_save_and_validation(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    result = svc.ok("config.save", {"patch": {"ui": {"theme": "light"}, "motion": {"gain": 1.5}}})
    assert result["config"]["ui"]["theme"] == "light"
    assert result["config"]["motion"]["gain"] == 1.5
    bad = svc.call("config.save", {"patch": {"motion": {"report_hz": 300}}})
    assert bad["ok"] is False
    assert "report_hz" in bad["error"]
    # 非法补丁必须整体回滚，不能污染运行中的 Config。
    assert svc.ok("config.get")["config"]["motion"]["report_hz"] == 1000


def test_devices_scan_and_desktop_geometry(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    scan = svc.ok("devices.scan")
    assert any(item["online"] for item in scan["devices"]), "演示平板应在线"
    assert svc.events("devices"), "扫描应推送 devices 事件"
    desktop = svc.ok("layout.desktop")
    assert "boundingBox" in desktop
    assert all("monitorHandle" in seg for seg in desktop["segments"])


def test_layout_save_uses_real_placement(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    initial = svc.ok("layout.initial")["placement"]
    assert initial is not None
    placement = {**initial, "edge": "top", "offset": 0.25, "align": "center"}
    # 上边的 span 需要与该显示器上边一致，否则 service 会拒绝；用 desktop 里真实的 top 段。
    desktop = svc.ok("layout.desktop")
    top = next((seg for seg in desktop["segments"] if seg["edge"] == "top"), None)
    if top is None:
        pytest.skip("当前显示器没有 top 外轮廓段")
    placement = {**top, "offset": 0.25, "align": "center", "cornerPriority": None}
    result = svc.ok(
        "layout.save",
        {"placement": placement, "identity": "TESTDEVICE", "orientation": "landscape"},
    )
    assert result["config"]["placement"]["edge"] == "top"
    assert result["config"]["layouts"]["devices"]["testdevice"]["landscape"]["align"] == "center"
    assert svc.events("placement")


def test_diagnostics_report_and_export_are_redacted(
    client: tuple[ServiceClient, Path], tmp_path: Path
) -> None:
    svc, _ = client
    report = svc.ok("diagnostics.run")
    assert "PadLend" in report["text"]
    assert "<serial>" in report["text"]
    exported = svc.ok("diagnostics.export", {"directory": str(tmp_path)})
    path = Path(exported["path"])
    assert path.is_file()
    import zipfile

    with zipfile.ZipFile(path) as archive:
        names = set(archive.namelist())
        assert {"README.txt", "diagnostics.txt", "adb-devices.txt", "meta.json"} <= names
        assert "<serial>" in archive.read("diagnostics.txt").decode("utf-8")


def test_wizard_checks_and_engine_events(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    for step in range(5):
        result = svc.ok("wizard.check", {"step": step})
        assert result["text"]
    state = svc.ok("engine.state")
    assert state["state"] in {"idle", "captured", "fault", "arming", "returning"}
    before = len(svc.events("state"))
    svc.ok("engine.toggle")
    assert len(svc.events("state")) >= before
    assert svc.events("log"), "引擎应推送日志事件"


def test_logs_recent_and_system_paths(client: tuple[ServiceClient, Path]) -> None:
    svc, _ = client
    assert isinstance(svc.ok("logs.recent", {"limit": 50})["logs"], list)
    paths = svc.ok("system.paths")
    assert paths["configPath"].endswith("config.yaml")
    assert paths["license"] == "Apache-2.0"
