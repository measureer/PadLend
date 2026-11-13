"""设备注册表单元测试：纯文件操作，不连 adb、不联网、不依赖系统时间。"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import pytest

from service import device_registry as registry_module
from service.device_registry import (
    DeviceRegistry,
    canonical_hardware_serial,
    normalize_mac,
    split_endpoint,
)

NOW = datetime(2026, 11, 13, 12, 0, 0, tzinfo=UTC)


def _registry(tmp_path: Path) -> DeviceRegistry:
    return DeviceRegistry(tmp_path / "devices.json").load()


def test_missing_file_returns_empty_registry(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    assert reg.records() == ()
    assert reg.existed is False
    assert reg.serials("1.2.3.4:5555") == []


def test_save_load_roundtrip_keeps_identity_and_endpoints(tmp_path: Path) -> None:
    path = tmp_path / "devices.json"
    reg = DeviceRegistry(path).load()
    record = reg.upsert(
        "192.168.1.5:5555",
        hardware_serial="ABC123",
        model="Pad 7",
        mac="AA:BB:CC:DD:EE:FF",
        android_id="android-1",
        source="usb",
        now=NOW,
    )
    assert record is not None
    assert record.id == "hw:abc123"
    assert reg.save() is True

    loaded = DeviceRegistry(path).load()
    assert loaded.issues == []
    restored = loaded.find_by_hardware("abc123")
    assert restored is not None
    assert restored.model == "Pad 7"
    assert restored.android_id == "android-1"
    assert restored.macs == ["aa:bb:cc:dd:ee:ff"]
    assert restored.endpoints[0].serial == "192.168.1.5:5555"
    assert restored.endpoints[0].transport == "wifi"
    assert restored.endpoints[0].last_ok == "2026-11-13T12:00:00+00:00"


def test_wireless_address_change_updates_same_device(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    first = reg.upsert(
        "192.168.1.5:5555", hardware_serial="ABC123", model="Pad 7", now=NOW
    )
    assert first is not None
    second = reg.upsert(
        "10.0.0.9:5555", hardware_serial="abc123", model="Pad 7", now=NOW
    )
    assert second is first
    assert len(reg.records()) == 1
    assert [endpoint.serial for endpoint in second.endpoints] == [
        "10.0.0.9:5555",
        "192.168.1.5:5555",
    ]
    assert reg.serials("10.0.0.9:5555") == ["10.0.0.9:5555"]


def test_provisional_record_is_upgraded_in_place(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    provisional = reg.upsert(
        "192.168.1.5:5555",
        model="Pad 7",
        mac="aa:bb:cc:dd:ee:ff",
        source="legacy",
        now=NOW,
    )
    assert provisional is not None
    assert provisional.id == "ep:192.168.1.5:5555"
    assert provisional.hardware_serial == ""

    upgraded = reg.upsert(
        "192.168.1.5:5555",
        hardware_serial="HW-9",
        model="Pad 7",
        now=NOW,
    )
    assert upgraded is provisional
    assert upgraded.id == "hw:hw-9"
    assert upgraded.hardware_serial == "HW-9"
    assert upgraded.macs == ["aa:bb:cc:dd:ee:ff"]
    assert [endpoint.serial for endpoint in upgraded.endpoints] == ["192.168.1.5:5555"]


def test_two_provisional_records_merge_by_mac(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    old = reg.upsert(
        "192.168.1.5:5555", mac="aa:bb:cc:dd:ee:ff", source="legacy", now=NOW
    )
    new = reg.upsert(
        "10.0.0.9:5555", mac="AA-BB-CC-DD-EE-FF", now=NOW
    )
    assert old is new
    assert len(reg.records()) == 1
    assert [endpoint.serial for endpoint in new.endpoints] == [
        "10.0.0.9:5555",
        "192.168.1.5:5555",
    ]


def test_different_hardware_serial_is_never_merged_by_mac(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    reg.upsert("1.1.1.1:5555", hardware_serial="HW-A", mac="aa:bb:cc:dd:ee:ff", now=NOW)
    reg.upsert("2.2.2.2:5555", hardware_serial="HW-B", mac="aa:bb:cc:dd:ee:ff", now=NOW)
    assert len(reg.records()) == 2
    assert reg.find_by_hardware("HW-A") is not None
    assert reg.find_by_hardware("HW-B") is not None


def test_endpoint_reused_by_other_device_moves_to_new_record(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    old = reg.upsert("1.1.1.1:5555", hardware_serial="HW-A", now=NOW)
    assert old is not None
    reg.upsert("1.1.1.2:5555", hardware_serial="HW-A", now=NOW)
    new = reg.upsert("1.1.1.1:5555", hardware_serial="HW-B", now=NOW)
    assert new is not None
    assert new.id == "hw:hw-b"
    assert [endpoint.serial for endpoint in new.endpoints] == ["1.1.1.1:5555"]
    assert [endpoint.serial for endpoint in old.endpoints] == ["1.1.1.2:5555"]


def test_remove_last_endpoint_drops_record(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    reg.upsert("1.1.1.1:5555", hardware_serial="HW-A", now=NOW)
    assert reg.remove("1.1.1.1:5555") is True
    assert reg.records() == ()
    assert reg.remove("1.1.1.1:5555") is False


def test_rename_endpoint_keeps_alias_and_record(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    record = reg.upsert(
        "192.168.1.5:5555",
        hardware_serial="HW-A",
        mac="aa:bb:cc:dd:ee:ff",
        now=NOW,
    )
    assert record is not None
    moved = reg.rename_endpoint(
        "192.168.1.5:5555", "10.0.0.9:5555", mac="aa:bb:cc:dd:ee:ff", now=NOW
    )
    assert moved is record
    assert [endpoint.serial for endpoint in moved.endpoints] == ["10.0.0.9:5555"]
    assert "192.168.1.5:5555" in moved.endpoints[0].aliases
    assert reg.find_by_serial("192.168.1.5:5555") is record
    assert reg.find_by_serial("10.0.0.9:5555") is record


def test_rename_endpoint_refuses_address_owned_by_another_device(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    reg.upsert("1.1.1.1:5555", hardware_serial="HW-A", now=NOW)
    reg.upsert("2.2.2.2:5555", hardware_serial="HW-B", now=NOW)
    assert reg.rename_endpoint("1.1.1.1:5555", "2.2.2.2:5555", now=NOW) is None
    assert reg.find_by_hardware("HW-A") is not None
    assert reg.find_by_hardware("HW-B") is not None


def test_import_legacy_groups_endpoints_by_mac(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    count = reg.import_legacy(
        current="1.1.1.1:5555",
        known=["1.1.1.1:5555", "1.1.1.2:5555", "USB123"],
        macs={"1.1.1.1:5555": "aa:bb:cc:dd:ee:ff", "1.1.1.2:5555": "AA:BB:CC:DD:EE:FF"},
        now=NOW,
    )
    assert count == 3
    assert len(reg.records()) == 2
    wireless = reg.find_by_serial("1.1.1.2:5555")
    assert wireless is not None
    assert [endpoint.serial for endpoint in wireless.endpoints] == [
        "1.1.1.1:5555",
        "1.1.1.2:5555",
    ]
    assert wireless.macs == ["aa:bb:cc:dd:ee:ff"]
    assert wireless.hardware_serial == ""
    assert all(
        endpoint.source == "legacy" for record in reg.records() for endpoint in record.endpoints
    )
    assert reg.serials("1.1.1.1:5555")[0] == "1.1.1.1:5555"


def test_import_legacy_without_known_is_noop(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    assert reg.import_legacy(now=NOW) == 0
    assert reg.records() == ()


def test_corrupt_file_is_backed_up_and_rebuilt(tmp_path: Path) -> None:
    path = tmp_path / "devices.json"
    path.write_text("{not-json", encoding="utf-8")
    reg = DeviceRegistry(path).load()
    assert reg.records() == ()
    assert reg.issues
    assert list(tmp_path.glob("devices.json.bad-*"))
    assert path.exists()


def test_future_schema_is_backed_up_and_not_loaded(tmp_path: Path) -> None:
    path = tmp_path / "devices.json"
    path.write_text('{"version": 99, "devices": []}', encoding="utf-8")
    reg = DeviceRegistry(path).load()
    assert reg.records() == ()
    assert "版本 99" in reg.issues[0]
    assert list(tmp_path.glob("devices.json.bad-*"))


def test_save_failure_keeps_previous_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / "devices.json"
    reg = _registry(tmp_path)
    reg.upsert("1.1.1.1:5555", hardware_serial="HW-A", now=NOW)
    assert reg.save() is True
    previous = path.read_text(encoding="utf-8")

    reg.upsert("2.2.2.2:5555", hardware_serial="HW-B", now=NOW)

    def boom(source: str, target: str) -> None:
        raise OSError("disk full")

    monkeypatch.setattr(registry_module.os, "replace", boom)
    assert reg.save() is False
    assert path.read_text(encoding="utf-8") == previous
    assert not list(tmp_path.glob("*.tmp"))


def test_endpoint_parser_supports_ipv6_and_hostnames() -> None:
    assert split_endpoint("[fd7a::1]:5555") == ("fd7a::1", 5555, "wifi")
    assert split_endpoint("fd7a::1") == ("fd7a::1", 0, "wifi")
    assert split_endpoint("pad.local:5555") == ("pad.local", 5555, "wifi")
    assert split_endpoint("R58M123ABC") == ("R58M123ABC", 0, "usb")


def test_hardware_serial_rejects_volatile_values() -> None:
    assert canonical_hardware_serial("ABC123", "ABC123") == "ABC123"
    assert canonical_hardware_serial("192.168.1.5:5555", "192.168.1.5:5555") == ""
    assert canonical_hardware_serial("unknown", "USB1") == ""
    assert canonical_hardware_serial("", "USB1") == ""


def test_mac_normalization() -> None:
    assert normalize_mac("AA-BB-CC-DD-EE-FF") == "aa:bb:cc:dd:ee:ff"
    assert normalize_mac("aa:bb:cc:dd:ee") == ""
    assert normalize_mac(None) == ""
