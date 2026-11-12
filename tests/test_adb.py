"""``adb devices -l`` 解析：畸形行必须丢弃，重复 serial 只保留一条。"""

from __future__ import annotations

from tablet.adb import Adb

DUP_OUTPUT = (
    "List of devices attached\n"
    "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp device "
    "product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:3\n"
    "adb-AM4NBB5C23001691-xn14x2._adb-tls-connect._tcp device "
    "product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:2\n"
)


class _StubAdb(Adb):
    def __init__(self, output: str) -> None:
        super().__init__()
        self._output = output

    def _text(self, args, *, timeout, code, check=True):  # noqa: ANN001, ANN003
        return self._output


def test_devices_drops_malformed_state_and_keeps_one_entry() -> None:
    entries = _StubAdb(DUP_OUTPUT).devices()

    assert len(entries) == 1
    entry = entries[0]
    assert entry.serial == "adb-AM4NBB5C23001691-xn14x2._adb-tls-connect._tcp"
    assert entry.state == "device"
    assert entry.model == "CGA_W00"
    assert entry.online


def test_devices_deduplicates_same_serial() -> None:
    output = """\
List of devices attached
emulator-5554 device product:sdk model:Pixel device:generic transport_id:1
emulator-5554 device product:sdk model:Pixel device:generic transport_id:1
"""
    entries = _StubAdb(output).devices()
    assert [e.serial for e in entries] == ["emulator-5554"]


def test_devices_keeps_offline_and_unauthorized() -> None:
    output = """\
List of devices attached
A offline transport_id:4
B unauthorized usb:1-1 transport_id:5
"""
    entries = _StubAdb(output).devices()
    assert [(e.serial, e.state) for e in entries] == [
        ("A", "offline"),
        ("B", "unauthorized"),
    ]


ONLY_MDNS_OUTPUT = (
    "List of devices attached\n"
    "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp device "
    "product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:3\n"
)


def test_devices_keeps_mdns_serial_with_space_when_it_is_the_only_entry() -> None:
    entries = _StubAdb(ONLY_MDNS_OUTPUT).devices()
    assert len(entries) == 1
    entry = entries[0]
    # adb -s 接受带空格的 serial；不能因为 state 不是第二列就把设备丢掉。
    assert entry.serial == "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp"
    assert entry.state == "device"
    assert entry.online
    assert entry.is_wireless and not entry.is_usb
    assert entry.transport_id == "3"


def test_select_device_falls_back_to_only_online_device() -> None:
    output = """List of devices attached
192.168.1.8:37495 device product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:15
"""
    entry = _StubAdb(output).select_device("adb-old-name._adb-tls-connect._tcp")
    assert entry.serial == "192.168.1.8:37495"
    assert entry.online


# ---------------------------------------------------------------------------
# 无线调试：地址归一化与 connect/pair 的成败判定
# ---------------------------------------------------------------------------
import subprocess  # noqa: E402
from pathlib import Path  # noqa: E402

import pytest  # noqa: E402

from core.errors import ErrorCode, PadLendError  # noqa: E402
from tablet.adb import normalize_address  # noqa: E402


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("192.168.1.8:37495", "192.168.1.8:37495"),
        ("  192.168.1.8  ", "192.168.1.8:5555"),
        ("adb connect 192.168.1.8:5555", "192.168.1.8:5555"),
        ("adb pair 192.168.1.8:37001", "192.168.1.8:37001"),
        ("tcp://192.168.1.8:5555", "192.168.1.8:5555"),
        ('"192.168.1.8:5555"', "192.168.1.8:5555"),
        ("adb connect 1.2.3.4:5555 already connected", "1.2.3.4:5555"),
        ("", ""),
        ("   ", ""),
        ("192.168.1.8:abc", ""),
        (":5555", ""),
        ("tablet.local", "tablet.local:5555"),
    ],
)
def test_normalize_address(raw: str, expected: str) -> None:
    assert normalize_address(raw) == expected


class _RunStub(Adb):
    """用固定 stdout/returncode 替代真实 subprocess。"""

    def __init__(self, output: str, returncode: int = 0) -> None:
        super().__init__()
        self._output = output
        self._returncode = returncode
        self.calls: list[list[str]] = []

    def _run(self, args, *, timeout, code=ErrorCode.ADB_NOT_FOUND, check=True):  # noqa: ANN001, ANN003
        self.calls.append(list(args))
        return subprocess.CompletedProcess(
            args=args,
            returncode=self._returncode,
            stdout=self._output.encode("utf-8"),
            stderr=b"",
        )


def test_connect_returns_adb_hint_on_success() -> None:
    adb = _RunStub("connected to 192.168.1.8:37495\n")
    assert "connected to" in adb.connect("192.168.1.8:37495")
    assert adb.calls[0][-2:] == ["connect", "192.168.1.8:37495"]


def test_connect_raises_e101_on_failure_output() -> None:
    adb = _RunStub("failed to connect to '192.168.1.8:37495': No route to host\n", 1)
    with pytest.raises(PadLendError) as info:
        adb.connect("192.168.1.8:37495")
    assert info.value.code is ErrorCode.DEVICE_OFFLINE
    assert "同一 Wi-Fi" in info.value.user_message


def test_connect_raises_when_adb_says_nothing() -> None:
    with pytest.raises(PadLendError):
        _RunStub("", 1).connect("192.168.1.8:37495")


def test_connect_rejects_blank_address() -> None:
    with pytest.raises(PadLendError):
        _RunStub("").connect("   ")


def test_pair_requires_success_line() -> None:
    ok = _RunStub("Successfully paired to 192.168.1.8:37001 [guid=adb-x]\n")
    assert "paired" in ok.pair("192.168.1.8:37001", "123456").lower()

    bad = _RunStub("Failed: Wrong password\n", 1)
    with pytest.raises(PadLendError) as info:
        bad.pair("192.168.1.8:37001", "000000")
    assert "配对端口与连接端口不同" in info.value.user_message


def test_pair_rejects_missing_code() -> None:
    with pytest.raises(PadLendError):
        _RunStub("").pair("192.168.1.8:37001", "")


def test_kill_server_never_raises(monkeypatch) -> None:
    from tablet import adb as adb_mod

    # 发版审计 P1：只在 PadLend 自己拉起 adb server 时才 kill。
    monkeypatch.setattr(adb_mod, "_SERVER_OWNED_BY_PADLEND", True)
    adb = _RunStub("", 1)
    assert adb.kill_server() is True
    assert adb.calls[-1] == ["kill-server"]


class _ShellStub(Adb):
    """按命令前缀返回固定 stdout，用来测 wifi_mac 的探测顺序。"""

    def __init__(self, answers: dict[str, tuple[int, str]]) -> None:
        super().__init__()
        self.answers = answers
        self.calls: list[str] = []

    def with_serial(self, serial: str | None) -> _ShellStub:  # noqa: D401 - stub
        self.calls.append(f"serial={serial}")
        return self

    def shell_result(self, *args: str, timeout: float = 0.0) -> tuple[int, str]:
        key = " ".join(args)
        self.calls.append(key)
        return self.answers.get(key, (1, ""))


def test_wifi_mac_reads_wlan0() -> None:
    adb = _ShellStub({"cat /sys/class/net/wlan0/address": (0, "88:A4:79:1A:2B:3C\n")})
    assert adb.wifi_mac("192.168.1.8:37495") == "88:a4:79:1a:2b:3c"
    assert adb.calls[0] == "serial=192.168.1.8:37495"


def test_wifi_mac_falls_back_to_scanning_interfaces() -> None:
    adb = _ShellStub(
        {
            "ls /sys/class/net": (0, "lo\nwlan0\nwlan9\n"),
            "cat /sys/class/net/wlan9/address": (0, "aa:bb:cc:dd:ee:ff\n"),
        }
    )
    assert adb.wifi_mac() == "aa:bb:cc:dd:ee:ff"


def test_wifi_mac_returns_empty_when_nothing_valid() -> None:
    adb = _ShellStub({"cat /sys/class/net/wlan0/address": (0, "not-a-mac\n")})
    assert adb.wifi_mac() == ""


# ---------------------------------------------------------------------------
# 子进程故障映射：超时 / adb 丢失 / 启动失败 / 断开吞错 / 未授权提示
# ---------------------------------------------------------------------------


def _bare_adb() -> Adb:
    adb = Adb()
    adb._path = Path("adb.exe")
    return adb


def test_run_maps_timeout_to_the_requested_error_code(monkeypatch) -> None:
    from tablet import adb as adb_mod

    monkeypatch.setattr(adb_mod, "_note_server_ownership", lambda args: None)

    def fake_run(*args, **kwargs):  # noqa: ANN002, ANN003
        raise subprocess.TimeoutExpired(cmd="adb devices -l", timeout=1.5)

    monkeypatch.setattr(adb_mod.subprocess, "run", fake_run)
    with pytest.raises(PadLendError) as info:
        _bare_adb()._run(["devices", "-l"], timeout=1.5, code=ErrorCode.DEVICE_OFFLINE)
    assert info.value.code is ErrorCode.DEVICE_OFFLINE
    assert "adb 超时" in info.value.detail


def test_run_resets_cached_path_when_binary_disappears(monkeypatch) -> None:
    from tablet import adb as adb_mod

    monkeypatch.setattr(adb_mod, "_note_server_ownership", lambda args: None)

    def fake_run(*args, **kwargs):  # noqa: ANN002, ANN003
        raise FileNotFoundError(2, "missing adb")

    monkeypatch.setattr(adb_mod.subprocess, "run", fake_run)
    adb = _bare_adb()
    with pytest.raises(PadLendError) as info:
        adb._run(["devices", "-l"], timeout=1.0)
    assert info.value.code is ErrorCode.ADB_NOT_FOUND
    assert adb._path is None


def test_run_maps_oserror_to_adb_not_found(monkeypatch) -> None:
    from tablet import adb as adb_mod

    monkeypatch.setattr(adb_mod, "_note_server_ownership", lambda args: None)

    def fake_run(*args, **kwargs):  # noqa: ANN002, ANN003
        raise OSError("cannot spawn")

    monkeypatch.setattr(adb_mod.subprocess, "run", fake_run)
    with pytest.raises(PadLendError) as info:
        _bare_adb()._run(["devices", "-l"], timeout=1.0)
    assert info.value.code is ErrorCode.ADB_NOT_FOUND


def test_disconnect_swallows_adb_errors(monkeypatch) -> None:
    adb = _bare_adb()

    def fake_run(*args, **kwargs):  # noqa: ANN002, ANN003
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, "adb 超时 1s")

    monkeypatch.setattr(adb, "_run", fake_run)
    assert adb.disconnect("192.168.1.8:37495") == ""


def test_select_device_reports_unauthorized_hint() -> None:
    output = """List of devices attached
ABCD1234 unauthorized usb:1-1 transport_id:5
"""
    with pytest.raises(PadLendError) as info:
        _StubAdb(output).select_device()
    assert info.value.code is ErrorCode.DEVICE_OFFLINE
    assert "允许" in info.value.user_message
