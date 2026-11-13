"""启动时在局域网里找上次那台平板（0.3.13）。"""

from __future__ import annotations

import subprocess

import pytest

from tablet import locator


def test_split_address_handles_port_and_fallbacks() -> None:
    assert locator.split_address("192.168.1.8:37495") == ("192.168.1.8", 37495)
    assert locator.split_address("192.168.1.8") == ("192.168.1.8", 5555)
    assert locator.split_address(" 192.168.1.8:abc ") == ("192.168.1.8", 5555)
    assert locator.split_address("192.168.1.8:70000") == ("192.168.1.8", 5555)
    assert locator.split_address(":5555") == ("", 0)
    assert locator.split_address("") == ("", 0)


def test_normalize_mac_accepts_dashes() -> None:
    assert locator.normalize_mac("88-A4-79-1A-2B-3C") == "88:a4:79:1a:2b:3c"
    assert locator.normalize_mac("88:a4:79:1a:2b:3c") == "88:a4:79:1a:2b:3c"
    assert locator.normalize_mac("nope") == ""
    assert locator.normalize_mac(None) == ""


def test_parse_mdns_and_port_lookup() -> None:
    lines = [
        "List of discovered mdns services",
        "adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234",
        "adb-XYZ._adb-tls-pairing._tcp\t192.168.1.9:37000",
        "adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234",
    ]
    services = locator.parse_mdns(lines)
    assert services == [("192.168.1.8", 41234), ("192.168.1.9", 37000)]
    assert locator.mdns_port_for("192.168.1.8", services) == 41234
    assert locator.mdns_port_for("10.0.0.1", services) == 0


def test_parse_mdns_can_filter_to_connect_service() -> None:
    lines = [
        "adb-ABC._adb-tls-connect._tcp	192.168.1.8:41234",
        "adb-XYZ._adb-tls-pairing._tcp	192.168.1.9:37000",
    ]
    assert locator.parse_mdns(lines, service="_adb-tls-connect._tcp") == [
        ("192.168.1.8", 41234)
    ]


def test_locate_reports_online_when_address_answers(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(locator, "tcp_open", lambda ip, port, *, timeout=1.5: True)
    found = locator.locate("192.168.1.8:37495")
    assert found.status == locator.STATUS_ONLINE
    assert found.address == "192.168.1.8:37495"
    assert "仍然在线" in found.detail


def test_locate_follows_a_new_port_from_mdns(monkeypatch: pytest.MonkeyPatch) -> None:
    def fake_tcp(ip: str, port: int, *, timeout: float = 1.5) -> bool:
        return (ip, port) == ("192.168.1.8", 41234)

    monkeypatch.setattr(locator, "tcp_open", fake_tcp)
    found = locator.locate(
        "192.168.1.8:37495",
        services=lambda: ["192.168.1.8:41234"],
    )
    assert found.status == locator.STATUS_MOVED
    assert found.address == "192.168.1.8:41234"


def test_locate_follows_a_new_port_from_direct_mdns(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_tcp(ip: str, port: int, *, timeout: float = 1.5) -> bool:
        return (ip, port) == ("192.168.1.8", 44803)

    monkeypatch.setattr(locator, "tcp_open", fake_tcp)
    found = locator.locate(
        "192.168.1.8:37495",
        mdns=lambda: [("192.168.1.8", 44803)],
    )
    assert found.status == locator.STATUS_MOVED
    assert found.address == "192.168.1.8:44803"


def test_locate_follows_a_new_ip_through_the_arp_table(monkeypatch: pytest.MonkeyPatch) -> None:
    def fake_tcp(ip: str, port: int, *, timeout: float = 1.5) -> bool:
        return (ip, port) == ("192.168.1.42", 41234)

    monkeypatch.setattr(locator, "tcp_open", fake_tcp)
    found = locator.locate(
        "192.168.1.8:37495",
        mac="88-a4-79-1a-2b-3c",
        services=lambda: ["192.168.1.42:41234"],
        arp=lambda: {"88:a4:79:1a:2b:3c": "192.168.1.42"},
    )
    assert found.status == locator.STATUS_MOVED
    assert found.address == "192.168.1.42:41234"
    assert "换成了" in found.detail


def test_locate_reports_no_port_when_only_the_ip_is_found(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(locator, "tcp_open", lambda ip, port, *, timeout=1.5: False)
    found = locator.locate(
        "192.168.1.8:37495",
        mac="88:a4:79:1a:2b:3c",
        arp=lambda: {"88:a4:79:1a:2b:3c": "192.168.1.42"},
    )
    assert found.status == locator.STATUS_NO_PORT
    assert found.ip == "192.168.1.42"
    assert found.reachable is True and found.online is False


def test_locate_reports_offline_when_nothing_answers(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(locator, "tcp_open", lambda ip, port, *, timeout=1.5: False)
    found = locator.locate("192.168.1.8:37495", mac="88:a4:79:1a:2b:3c", arp=lambda: {})
    assert found.status == locator.STATUS_OFFLINE
    assert "找不到" in found.detail


def test_locate_without_any_remembered_address_is_offline() -> None:
    found = locator.locate("")
    assert found.status == locator.STATUS_OFFLINE
    assert "还没有连接过" in found.detail


def test_discovered_services_prefers_adb_output(monkeypatch: pytest.MonkeyPatch) -> None:
    from tablet import mdns as mdns_module

    calls: list[float] = []

    def fake_discover(*, timeout: float = 1.2) -> list[object]:
        calls.append(timeout)
        return []

    monkeypatch.setattr(mdns_module, "discover", fake_discover)
    found = locator.discovered_services(
        lambda: ["adb-ABC._adb-tls-connect._tcp	192.168.1.8:41234"]
    )
    assert found == [("192.168.1.8", 41234)]
    assert calls == []


def test_discovered_services_falls_back_to_direct_mdns(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from tablet import mdns as mdns_module

    monkeypatch.setattr(
        mdns_module,
        "discover",
        lambda *, timeout=1.2: [
            mdns_module.MdnsService(ip="192.168.1.8", port=44803),
            mdns_module.MdnsService(ip="192.168.1.8", port=44803),
        ],
    )
    assert locator.discovered_services(lambda: []) == [("192.168.1.8", 44803)]



def test_split_address_handles_ipv6_and_hostnames() -> None:
    assert locator.split_address("[fd7a::1]:5555") == ("fd7a::1", 5555)
    assert locator.split_address("[fd7a::1]") == ("fd7a::1", 5555)
    assert locator.split_address("fd7a::1") == ("fd7a::1", 5555)
    assert locator.split_address("tablet.local") == ("tablet.local", 5555)
    assert locator.format_address("fd7a::1", 5555) == "[fd7a::1]:5555"
    assert locator.format_address("192.168.1.8", 5555) == "192.168.1.8:5555"
    assert locator.format_address("", 5555) == ""


def test_discovered_mdns_keeps_hardware_serial(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from tablet import mdns as mdns_module

    monkeypatch.setattr(
        mdns_module,
        "discover",
        lambda *, timeout=1.2: [
            mdns_module.MdnsService(
                ip="10.0.0.5", port=5555, serial="HW1", model="Pad"
            ),
            mdns_module.MdnsService(
                ip="10.0.0.5", port=5555, serial="HW1", model="Pad"
            ),
        ],
    )

    found = locator.discovered_mdns(lambda: [])

    assert [(item.ip, item.port, item.serial) for item in found] == [
        ("10.0.0.5", 5555, "HW1")
    ]


def test_locate_finds_a_new_ip_by_hardware_serial(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from tablet import mdns as mdns_module

    def fake_tcp(ip: str, port: int, *, timeout: float = 1.5) -> bool:
        return (ip, port) == ("10.0.0.5", 5555)

    monkeypatch.setattr(locator, "tcp_open", fake_tcp)
    found = locator.locate(
        "192.168.1.8:37495",
        hardware_serial="HW1",
        mdns=lambda: [
            mdns_module.MdnsService(ip="10.0.0.5", port=5555, serial="HW1")
        ],
    )

    assert found.status == locator.STATUS_MOVED
    assert found.address == "10.0.0.5:5555"
    assert "硬件序列号" in found.detail


def test_arp_table_reads_windows_output(monkeypatch: pytest.MonkeyPatch) -> None:
    output = (
        "接口: 192.168.1.5 --- 0x9\n"
        "  192.168.1.1           88-a4-79-1a-2b-3c     动态\n"
        "  192.168.1.42          88-a4-79-1a-2b-3c     动态\n".encode("gbk")
    )

    def fake_run(*args, **kwargs):  # noqa: ANN002, ANN003
        return subprocess.CompletedProcess(
            args=["arp", "-a"], returncode=0, stdout=output, stderr=b""
        )

    monkeypatch.setattr(locator.subprocess, "run", fake_run)
    table = locator.arp_table()
    assert locator.find_ip_by_mac("88-a4-79-1a-2b-3c", table) == "192.168.1.1"


def test_arp_table_survives_a_broken_command(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(*args, **kwargs):  # noqa: ANN002, ANN003
        raise OSError("arp missing")

    monkeypatch.setattr(locator.subprocess, "run", boom)
    assert locator.arp_table() == {}
