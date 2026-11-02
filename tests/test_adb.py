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
