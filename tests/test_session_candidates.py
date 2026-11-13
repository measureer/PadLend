"""候选端点连接（1.2.2）：只认注册表地址、短预检、身份校验。"""

from __future__ import annotations

import pytest

from config import Config
from core.errors import ErrorCode, PadLendError
from tablet import session as session_module
from tablet.adb import DeviceEntry
from tablet.session import DeviceFacts, TabletSession


class FakeAdb:
    def __init__(self, entries: list[DeviceEntry] | None = None) -> None:
        self.entries = list(entries or [])
        self.connections: list[tuple[str, float]] = []
        self.current = ""

    def version(self) -> str:
        return "1.0.41"

    def devices(self) -> list[DeviceEntry]:
        return list(self.entries)

    def connect(self, address: str, *, timeout: float = 20.0) -> str:
        self.connections.append((address, timeout))
        if not any(item.serial == address for item in self.entries):
            self.entries.append(DeviceEntry(serial=address, state="device"))
        return f"connected to {address}"

    def with_serial(self, serial: str) -> FakeAdb:
        self.current = serial
        return self


def _facts(serial: str, hardware: str = "") -> DeviceFacts:
    return DeviceFacts(
        serial=serial,
        model="Pad",
        width=1600,
        height=2560,
        orientation=0,
        xscale=1.0,
        pointer_speed=-3,
        hardware_serial=hardware or serial,
    )


def _session(
    monkeypatch: pytest.MonkeyPatch,
    adb: FakeAdb,
    candidates: list[str],
    verify=None,
) -> TabletSession:
    monkeypatch.setattr(
        session_module,
        "read_facts",
        lambda bound: _facts(bound.current),
    )
    cfg = Config()
    cfg.device.serial = candidates[0] if candidates else ""
    return TabletSession(
        cfg,
        adb=adb,
        candidates=lambda: list(candidates),
        verify_facts=verify,
    )


def test_candidate_order_and_dedupe() -> None:
    cfg = Config()
    cfg.device.serial = ""
    more = TabletSession(cfg, adb=FakeAdb(), candidates=lambda: ["A", "a", "", "B"])
    assert more._candidate_serials() == ["A", "B"]
    missing = TabletSession(cfg, adb=FakeAdb(), candidates=lambda: [])
    assert missing._candidate_serials() == [None]


def test_preflight_skips_dead_wireless_before_adb_connect(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    probes: list[tuple[str, int, float]] = []

    def fake_tcp(host: str, port: int, *, timeout: float = 1.5) -> bool:
        probes.append((host, port, timeout))
        return host == "192.168.1.20"

    monkeypatch.setattr(session_module, "tcp_open", fake_tcp)
    adb = FakeAdb()
    session = _session(monkeypatch, adb, ["192.168.1.10:5555", "192.168.1.20:5555"])

    entry, facts = session._connect_candidate()

    assert entry.serial == "192.168.1.20:5555"
    assert facts.serial == "192.168.1.20:5555"
    assert adb.connections == [("192.168.1.20:5555", 4.0)]
    assert probes == [
        ("192.168.1.10", 5555, 0.6),
        ("192.168.1.20", 5555, 0.6),
    ]


def test_wrong_hardware_is_rejected_and_next_candidate_wins(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    adb = FakeAdb(
        [
            DeviceEntry("USB1", "device"),
            DeviceEntry("USB2", "device"),
        ]
    )
    seen: list[tuple[str, str]] = []

    def verify(serial: str, facts: DeviceFacts) -> bool:
        seen.append((serial, facts.hardware_serial))
        return serial == "USB2"

    session = _session(monkeypatch, adb, ["USB1", "USB2"], verify=verify)
    entry, _ = session._connect_candidate()

    assert entry.serial == "USB2"
    assert seen == [("USB1", "USB1"), ("USB2", "USB2")]


def test_usb_candidate_never_falls_back_to_the_only_online_device(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    adb = FakeAdb([DeviceEntry("SOME_OTHER_TABLET", "device")])
    session = _session(monkeypatch, adb, ["USB1"])

    with pytest.raises(PadLendError) as caught:
        session._connect_candidate()

    assert caught.value.code is ErrorCode.DEVICE_OFFLINE
    assert "USB1" in caught.value.detail
    assert adb.connections == []
