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


def test_kill_server_never_raises() -> None:
    adb = _RunStub("", 1)
    adb.kill_server()
    assert adb.calls[-1] == ["kill-server"]
