"""发版加固回归测试（task-5：崩溃/隐私/依赖/清理）。

覆盖审计报告里的 P0/P1 关键路径：
- 非法编码/未知键的配置不会把用户配置静默丢掉；
- 诊断包脱敏且附带日志尾巴；
- 只在 PadLend 自己启动 adb server 时才 kill-server；
- pointer_speed 原值崩溃后能跨进程恢复；
- 崩溃日志确实带 traceback。
"""

from __future__ import annotations

import zipfile
from pathlib import Path

import constants as C
from config import load_config
from core import crash
from tablet import adb as adb_mod
from tablet import session as session_mod
from tablet.adb import Adb
from ui import diagnostics as diag


def test_config_non_utf8_falls_back_without_destroying_file(tmp_path: Path) -> None:
    path = tmp_path / "config.yaml"
    path.write_bytes(b"\xff\xfe version: 1\n")

    cfg = load_config(path)

    assert cfg.issues, "非 UTF-8 配置必须告诉用户本次用了默认值"
    assert any("UTF-8" in issue for issue in cfg.issues)
    backups = list(tmp_path.glob("config.yaml.bad-*"))
    assert backups, "解析失败前必须先备份原文件"
    assert backups[0].read_bytes() == b"\xff\xfe version: 1\n"


def test_config_unknown_keys_are_warned_but_preserved(tmp_path: Path) -> None:
    path = tmp_path / "config.yaml"
    path.write_text(
        "version: 1\ndevice:\n  serial: R58M12345\nfuture_section:\n  x: 1\n",
        encoding="utf-8",
    )

    cfg = load_config(path)

    assert cfg.device.serial == "R58M12345", "未知键不能导致整份配置被重置"
    assert any("未知配置项" in issue for issue in cfg.issues)


def test_diagnostics_export_redacts_serial_ip_and_username(
    tmp_path: Path, monkeypatch
) -> None:
    log_dir = tmp_path / "logs"
    log_dir.mkdir()
    (log_dir / "padlend.log").write_text(
        "connect 192.168.1.9:5555 serial=R58M12345\n"
        "path C:\\Users\\alice\\AppData\\Local\\PadLend\n",
        encoding="utf-8",
    )
    (log_dir / "crash-20260101-000000-1.log").write_text(
        "kind: sys.excepthook\nserial: R58M12345\nRuntimeError: boom\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(C, "log_dir", lambda: log_dir)

    results = [
        diag.CommandResult(
            "adb devices -l",
            0,
            "List of devices attached\nR58M12345\tdevice usb:1-1\n",
            "",
        )
    ]
    report = diag.build_report(results, server_jar=tmp_path / "scrcpy-server-v5.0.1.jar")

    text = report.to_text()
    assert "R58M12345" not in text
    assert "<serial>" in text

    archive = diag.export_report(report, directory=tmp_path)
    with zipfile.ZipFile(archive) as handle:
        names = set(handle.namelist())
        assert "README.txt" in names
        assert "logs/padlend-tail.log" in names
        assert "logs/crash-latest.log" in names
        blob = "".join(
            handle.read(name).decode("utf-8", "replace") for name in names
        )
    for secret in ("R58M12345", "192.168.1.9", "alice"):
        assert secret not in blob, f"诊断包里泄露了 {secret}"
    assert "<user>" in blob


def test_adb_kill_server_skips_when_server_not_owned(monkeypatch) -> None:
    adb = Adb()
    calls: list[list[str]] = []
    monkeypatch.setattr(adb, "_run", lambda args, **kwargs: calls.append(list(args)))
    monkeypatch.setattr(adb_mod, "_SERVER_OWNED_BY_PADLEND", False)

    assert adb.kill_server() is False
    assert calls == [], "不是自己拉起的 adb server，绝不能 kill"


def test_adb_kill_server_runs_once_when_owned(monkeypatch) -> None:
    adb = Adb()
    calls: list[list[str]] = []
    monkeypatch.setattr(adb, "_run", lambda args, **kwargs: calls.append(list(args)))
    monkeypatch.setattr(adb_mod, "_SERVER_OWNED_BY_PADLEND", True)

    assert adb.kill_server() is True
    assert calls == [["kill-server"]]
    assert adb.kill_server() is False
    assert calls == [["kill-server"]], "只允许 kill 一次"


def test_adb_pair_code_is_masked_in_display_args() -> None:
    args = Adb._display_args(["pair", "192.168.1.5:4321", "123456"])
    assert args == ["pair", "192.168.1.5:4321", "<code>"]


def test_pointer_state_survives_restart(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(session_mod.C, "config_dir", lambda: tmp_path)

    session_mod.save_pointer_state("R58M12345", -2)
    assert session_mod.load_pointer_state() == ("R58M12345", -2)

    class FakeAdb:
        def __init__(self) -> None:
            self.calls: list[tuple[str, ...]] = []

        def with_serial(self, serial: str) -> FakeAdb:
            self.serial = serial
            return self

        def shell_ok(self, *args: str) -> bool:
            self.calls.append(args)
            return True

    fake = FakeAdb()
    assert session_mod.restore_pending_pointer_speed(fake) == -2
    assert fake.calls == [("settings", "put", "system", "pointer_speed", "-2")]
    assert session_mod.load_pointer_state() is None, "恢复成功后状态文件必须清掉"


def test_pointer_state_kept_when_device_offline(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(session_mod.C, "config_dir", lambda: tmp_path)
    session_mod.save_pointer_state("", 7)

    class OfflineAdb:
        def with_serial(self, serial: str) -> OfflineAdb:
            return self

        def shell_ok(self, *args: str) -> bool:
            return False

    assert session_mod.restore_pending_pointer_speed(OfflineAdb()) is None
    assert session_mod.load_pointer_state() == ("", 7), "设备不在线时要保留下次再试"


def test_crash_log_contains_traceback(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(crash, "_LOG_DIR", tmp_path)
    try:
        raise ValueError("boom-xyz")
    except ValueError as exc:
        path = crash.write_crash("test", type(exc), exc, exc.__traceback__)

    assert path is not None
    body = path.read_text(encoding="utf-8")
    assert "boom-xyz" in body
    assert "traceback:" in body
    assert "ValueError" in body
