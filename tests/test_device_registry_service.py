"""服务层设备注册表集成测试：迁移、列表去重、身份升级与移除。"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from config import Config, PlacementCfg
from service.api import PadLendService
from service.device_registry import DeviceRegistry
from tablet.locator import STATUS_MOVED, Located


def _service(tmp_path: Path) -> PadLendService:
    events: list[tuple[str, object]] = []
    svc = PadLendService(
        lambda event, data: events.append((event, data)),
        simulate=True,
        no_hooks=True,
        config_path=str(tmp_path / "config.yaml"),
    )
    svc.cfg = Config()
    svc._events = events  # type: ignore[attr-defined]
    return svc


def _entry(serial: str, *, wireless: bool, state: str = "device") -> SimpleNamespace:
    return SimpleNamespace(
        serial=serial,
        state=state,
        model="Pad",
        is_wireless=wireless,
    )


def test_migrates_legacy_known_and_macs_once(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.cfg.device.serial = "192.168.1.9:5555"
    svc.cfg.device.known = ["192.168.1.9:5555", "USB123"]
    svc.cfg.device.macs = {"192.168.1.9:5555": "AA:BB:CC:DD:EE:FF"}

    svc._migrate_device_registry()

    assert (tmp_path / "devices.json").exists()
    assert len(svc.registry.records()) == 2
    wireless = svc.registry.find_by_serial("192.168.1.9:5555")
    assert wireless is not None
    assert wireless.macs == ["aa:bb:cc:dd:ee:ff"]
    assert svc.registry.find_by_serial("USB123") is not None

    # 第二次启动（文件已存在）不能再从旧 config 导入，否则删掉的设备会回魂。
    svc.cfg.device.known.append("SHOULD_NOT_IMPORT")
    svc._migrate_device_registry()
    assert len(svc.registry.records()) == 2
    assert svc.registry.find_by_serial("SHOULD_NOT_IMPORT") is None


def test_publish_keeps_one_row_per_record_and_shows_offline(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.registry.upsert("USB123", hardware_serial="HW1", model="Pad")
    svc.registry.upsert("192.168.1.9:5555", hardware_serial="HW1", model="Pad")
    assert len(svc.registry.records()) == 1

    rows = svc._publish_devices(
        [
            _entry("USB123", wireless=False),
            _entry("192.168.1.9:5555", wireless=True),
        ]
    )
    assert [row["serial"] for row in rows] == ["USB123"]
    assert rows[0]["online"] is True

    rows = svc._publish_devices([])
    assert len(rows) == 1
    assert rows[0]["online"] is False
    assert rows[0]["remembered"] is True
    assert rows[0]["model"] == "Pad"


def test_remember_promotes_provisional_record_and_moves_layout(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.cfg.layouts.set("192.168.1.9:5555", "landscape", PlacementCfg(span_end=1234))
    svc.registry.upsert("192.168.1.9:5555")
    assert svc.registry.find_by_serial("192.168.1.9:5555").id.startswith("ep:")

    svc._remember_device(
        "192.168.1.9:5555",
        facts=SimpleNamespace(hardware_serial="HW1", model="Pad"),
    )

    record = svc.registry.find_by_hardware("HW1")
    assert record is not None
    assert record.id == "hw:hw1"
    assert svc.cfg.device.serial == "192.168.1.9:5555"
    assert svc.cfg.layouts.get("HW1", "landscape") is not None
    assert svc.cfg.layouts.get("192.168.1.9:5555", "landscape") is None


def test_adopt_address_moves_current_pointer_and_keeps_alias(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    old = "192.168.1.9:5555"
    new = "192.168.1.20:5555"
    svc.registry.upsert(old)
    svc.cfg.device.serial = old

    record = svc._adopt_address(
        old,
        Located(status=STATUS_MOVED, address=new, ip="192.168.1.20", port=5555),
    )

    assert record is not None
    assert svc.registry.find_by_serial(new) is record
    assert record.endpoint(new).aliases == [old]
    assert svc.cfg.device.serial == new
    assert svc.registry.find_by_serial(old) is record


def test_remove_clears_pointer_and_located_without_touching_legacy_config(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.registry.upsert("USB1", hardware_serial="HW1")
    svc.registry.upsert("USB2", hardware_serial="HW2")
    svc.cfg.device.serial = "USB1"
    svc.cfg.device.known = ["USB1", "USB2"]
    svc._located["USB1"] = {"status": "online"}

    result = svc.devices_remove({"serial": "USB1"})

    assert result["ok"] is True
    assert svc.registry.find_by_serial("USB1") is None
    assert svc.cfg.device.serial == "USB2"
    assert "USB1" not in svc._located
    # 旧字段保留，避免回退 1.2.1 时看不到任何设备。
    assert svc.cfg.device.known == ["USB1", "USB2"]


def test_rebuilds_corrupt_registry_from_legacy_config(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.cfg.device.serial = "192.168.1.9:5555"
    svc.cfg.device.known = ["192.168.1.9:5555", "USB123"]
    svc.cfg.device.macs = {"192.168.1.9:5555": "AA:BB:CC:DD:EE:FF"}
    path = tmp_path / "devices.json"
    path.write_text("{not-json", encoding="utf-8")

    svc.registry = DeviceRegistry(path).load()
    assert svc.registry.needs_rebuild is True
    svc._migrate_device_registry()

    assert not svc.registry.needs_rebuild
    assert (tmp_path / "devices.json").read_text(encoding="utf-8").startswith("{")
    backups = list(tmp_path.glob("devices.json.bad-*"))
    assert len(backups) == 1
    assert svc.registry.find_by_serial("192.168.1.9:5555") is not None
    assert svc.registry.find_by_serial("USB123") is not None

    # 重建后旧 config 不再是数据源，新加进 known 的设备不能回魂。
    svc.cfg.device.known.append("SHOULD_NOT_IMPORT")
    svc._migrate_device_registry()
    assert svc.registry.find_by_serial("SHOULD_NOT_IMPORT") is None


def test_remove_deletes_all_endpoints_of_one_device(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.registry.upsert("USB123", hardware_serial="HW1", model="Pad")
    svc.registry.upsert("192.168.1.9:5555", hardware_serial="HW1", model="Pad")
    svc.cfg.device.serial = "192.168.1.9:5555"
    svc.cfg.device.known = ["USB123", "192.168.1.9:5555"]
    svc._located["USB123"] = {"status": "online"}
    svc._located["192.168.1.9:5555"] = {"status": "online"}

    result = svc.devices_remove({"serial": "USB123"})

    assert result["ok"] is True
    assert svc.registry.records() == ()
    assert svc.registry.find_by_serial("USB123") is None
    assert svc.registry.find_by_serial("192.168.1.9:5555") is None
    assert svc.cfg.device.serial == ""
    assert svc._located == {}
    assert svc.cfg.device.known == ["USB123", "192.168.1.9:5555"]


def test_candidates_follow_current_endpoint_then_last_ok(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    usb = "USB123"
    wifi = "192.168.1.9:5555"
    svc.registry.upsert(usb, hardware_serial="HW1")
    svc.registry.upsert(wifi, hardware_serial="HW1")
    record = svc.registry.find_by_hardware("HW1")
    assert record is not None
    record.endpoint(usb).last_ok = "2026-01-01T00:00:00+00:00"
    record.endpoint(wifi).last_ok = "2026-01-02T00:00:00+00:00"
    svc.cfg.device.serial = usb

    assert svc._device_candidates()[:2] == [usb, wifi]

    svc.cfg.device.serial = "UNKNOWN"
    assert svc._device_candidates() == ["UNKNOWN"]
    svc.cfg.device.serial = ""
    assert svc._device_candidates() == []


def test_verify_facts_rejects_a_reused_endpoint(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    serial = "192.168.1.9:5555"
    svc.registry.upsert(serial, hardware_serial="HW1")
    svc.cfg.device.serial = serial

    ok = svc._verify_device_facts(
        serial,
        SimpleNamespace(hardware_serial="HW2", serial=serial, model="Other"),
    )

    assert ok is False
    record = svc.registry.find_by_serial(serial)
    assert record is not None and record.hardware_serial == "HW1"
    assert record.endpoint(serial).fail_count == 1
    assert "硬件序列号不符" in record.endpoint(serial).last_error
    assert svc.cfg.device.serial == serial


def test_verify_facts_accepts_matching_hardware_and_clears_failure(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    serial = "192.168.1.9:5555"
    svc.registry.upsert(serial, hardware_serial="HW1")
    svc.registry.mark_failure(serial, "旧错误")
    svc.cfg.device.serial = serial

    ok = svc._verify_device_facts(
        serial,
        SimpleNamespace(hardware_serial="HW1", serial=serial, model="Pad"),
    )

    assert ok is True
    record = svc.registry.find_by_serial(serial)
    assert record is not None
    assert record.endpoint(serial).fail_count == 0
    assert record.endpoint(serial).last_error == ""


def test_locate_passes_hardware_serial_to_mdns_lookup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from service import api as api_module
    from tablet.mdns import MdnsService

    svc = _service(tmp_path)
    old = "192.168.1.9:5555"
    new = "10.0.0.5:5555"
    svc.registry.upsert(old, hardware_serial="HW1")
    svc.cfg.device.serial = old
    seen: dict[str, object] = {}

    monkeypatch.setattr(
        api_module,
        "discovered_mdns",
        lambda services, timeout=1.2: [
            MdnsService(ip="10.0.0.5", port=5555, serial="HW1")
        ],
    )

    def fake_locate(serial: str, mac: str = "", **kwargs):  # noqa: ANN003
        seen.update({"serial": serial, "mac": mac, **kwargs})
        return Located(status=STATUS_MOVED, address=new, ip="10.0.0.5", port=5555)

    monkeypatch.setattr(api_module, "locate", fake_locate)
    svc.runtime = SimpleNamespace(
        adb=SimpleNamespace(path="/usr/bin/adb", mdns_services=lambda: [])
    )

    svc.devices_locate({})

    assert seen["serial"] == old
    assert seen["hardware_serial"] == "HW1"
    record = svc.registry.find_by_serial(new)
    assert record is not None
    assert svc.cfg.device.serial == new


def test_remember_moves_layout_from_secondary_endpoint(tmp_path: Path) -> None:
    svc = _service(tmp_path)
    svc.registry.upsert("USB123", mac="AA:BB:CC:DD:EE:FF")
    svc.registry.upsert("192.168.1.9:5555", mac="AA:BB:CC:DD:EE:FF")
    assert len(svc.registry.records()) == 1
    svc.cfg.layouts.set("USB123", "landscape", PlacementCfg(span_end=4321))

    svc._remember_device(
        "192.168.1.9:5555",
        facts=SimpleNamespace(hardware_serial="HW1", model="Pad"),
    )

    record = svc.registry.find_by_hardware("HW1")
    assert record is not None
    assert record.id == "hw:hw1"
    assert svc.cfg.layouts.get("HW1", "landscape") is not None
    assert svc.cfg.layouts.get("USB123", "landscape") is None

