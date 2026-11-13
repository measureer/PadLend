"""配置系统单测（设计文档 6.9 / 8.3）。"""

from __future__ import annotations

import textwrap
from pathlib import Path

import yaml

import constants as C
from config import Config, DeviceCfg, LayoutsCfg, MotionCfg, PlacementCfg


def test_legacy_ghost_pointer_key_is_ignored(tmp_path, caplog):
    path = tmp_path / "config.yaml"
    path.write_text("ui:\n  show_pointer_ghost: true\n  theme: dark\n", encoding="utf-8")
    cfg = Config.load(path)
    assert not hasattr(cfg.ui, "show_pointer_ghost")
    assert cfg.ui.theme == "dark"
    assert "unknown_fields" not in caplog.text


def test_default_when_missing(tmp_path):
    cfg = Config.load(tmp_path / "nope.yaml")
    assert cfg.validate() == []
    assert cfg.motion.report_hz == C.DEFAULT_REPORT_HZ
    assert cfg.placement.edge in {"left", "right", "top", "bottom"}


def test_roundtrip(tmp_path):
    path = tmp_path / "config.yaml"
    cfg = Config()
    cfg.motion.gain = 1.75
    cfg.device.serial = "ABC123"
    cfg.placement.edge = "left"
    cfg.save(path)

    assert path.exists()
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert raw["version"] == 1
    assert raw["motion"]["gain"] == 1.75

    again = Config.load(path)
    assert again.device.serial == "ABC123"
    assert again.motion.gain == 1.75
    assert again.placement.edge == "left"
    assert again.validate() == []


def test_repair_clamps_and_reports(tmp_path):
    path = tmp_path / "bad.yaml"
    path.write_text(
        textwrap.dedent(
            """\
            version: 1
            device:
              server_port: 80
            placement:
              edge: diagonal
              align: stretch
              offset: 3.0
              span_start: 100
              span_end: 10
            motion:
              gain: 99
              report_hz: 3
              hid_clamp: 999
              arm_px: 0
            keyboard:
              layout: ""
              passthrough_keys: not-a-list
            hotkeys:
              toggle: ""
            """
        ),
        encoding="utf-8",
    )
    cfg = Config.load(path)

    assert cfg.device.server_port == C.DEFAULT_SERVER_PORT
    assert cfg.placement.edge == "right"
    assert cfg.placement.align == "proportional"
    assert cfg.placement.offset == 1.0
    assert cfg.placement.span_start == 0 and cfg.placement.span_end == 1600
    assert cfg.motion.gain == 3.0
    assert cfg.motion.report_hz == C.MIN_REPORT_HZ
    assert cfg.motion.hid_clamp == 127
    assert cfg.motion.arm_px == C.ARM_PX
    assert cfg.keyboard.layout == "us"
    assert cfg.keyboard.passthrough_keys == ["VK_LWIN", "VK_RWIN"]
    assert cfg.hotkeys.toggle == "Ctrl+Alt+S"
    assert cfg.issues, "修复过的配置必须留下 issue 供 GUI 标红"
    assert cfg.validate() == [], "load 之后对象必须已经自愈"


def test_report_hz_snaps_to_a_fixed_choice(tmp_path: Path) -> None:
    """0.3.16：只允许 125/250/500/1000；旧的任意值（如 998）取最近的档位。"""
    path = tmp_path / "config.yaml"
    path.write_text(
        "version: 1\nmotion:\n  report_hz: 998\n", encoding="utf-8"
    )
    cfg = Config.load(path)
    assert cfg.motion.report_hz == 1000
    assert any("report_hz" in issue for issue in cfg.issues)
    assert cfg.validate() == []


def test_theme_repair_and_roundtrip(tmp_path):
    path = tmp_path / "theme.yaml"
    path.write_text("version: 1\nui:\n  theme: neon\n", encoding="utf-8")
    cfg = Config.load(path)
    assert cfg.ui.theme == "dark"
    assert any("ui.theme" in issue for issue in cfg.issues)

    cfg.ui.theme = "light"
    cfg.save(path)
    again = Config.load(path)
    assert again.ui.theme == "light"
    assert again.validate() == []


def test_corrupt_yaml_backs_up_and_resets(tmp_path):
    path = tmp_path / "broken.yaml"
    path.write_text(":: not yaml ::: [", encoding="utf-8")

    cfg = Config.load(path)
    assert cfg.validate() == []
    assert cfg.issues
    assert list(tmp_path.glob("broken.yaml.bad-*")), "损坏配置必须先备份再重置"


def test_future_version_resets(tmp_path):
    path = tmp_path / "future.yaml"
    path.write_text("version: 99\n", encoding="utf-8")

    cfg = Config.load(path)
    assert cfg.issues
    assert list(tmp_path.glob("future.yaml.bad-*"))
    assert cfg.validate() == []


def test_validate_does_not_mutate():
    cfg = Config(device=DeviceCfg(server_port=1), placement=PlacementCfg(offset=2.0))
    problems = cfg.validate()
    assert any("server_port" in p for p in problems)
    assert any("offset" in p for p in problems)
    assert cfg.device.server_port == 1
    assert cfg.placement.offset == 2.0


def test_legacy_entry_guard_is_ignored_and_removed_on_save(tmp_path):
    path = tmp_path / "legacy.yaml"
    path.write_text("version: 1\nmotion:\n  entry_guard_px: 160\n", encoding="utf-8")
    cfg = Config.load(path)
    assert cfg.validate() == []
    assert not hasattr(cfg.motion, "entry_guard_px")
    cfg.save(path)
    saved = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert "entry_guard_px" not in saved["motion"]


def test_to_dict_version():
    raw = Config().to_dict()
    assert raw["version"] == 1
    assert set(raw) >= {"device", "placement", "motion", "keyboard", "hotkeys", "ui", "misc"}


def test_motion_defaults_are_sane():
    m = MotionCfg()
    assert 0.25 <= m.gain <= 3.0
    assert 1 <= m.hid_clamp <= 127
    assert m.arm_px >= 1 and m.exit_overflow_px >= 1


def test_known_devices_roundtrip(tmp_path):
    """记住的设备要能存盘、能读回，且顺序（新的在前）不变。"""
    path = tmp_path / "config.yaml"
    cfg = Config()
    cfg.device.serial = "192.168.1.9:5555"
    cfg.device.known = ["192.168.1.9:5555", "ABC123"]
    cfg.save(path)

    again = Config.load(path)
    assert again.device.known == ["192.168.1.9:5555", "ABC123"]
    assert again.device.serial == "192.168.1.9:5555"
    assert again.validate() == []


def test_known_devices_default_empty_and_coerced(tmp_path):
    """缺省是空列表；配置文件里写了怪类型也不能炸。"""
    assert DeviceCfg().known == []
    assert DeviceCfg().macs == {}

    path = tmp_path / "config.yaml"
    path.write_text("version: 1\ndevice:\n  known: [7, XYZ]\n", encoding="utf-8")
    cfg = Config.load(path)
    assert cfg.device.known == ["7", "XYZ"]


def test_device_macs_roundtrip(tmp_path):
    """每台无线设备的 MAC 都要各自存盘、读回，不能再挤在一个 mac 字段里。"""
    path = tmp_path / "config.yaml"
    cfg = Config()
    first = "192.168.1.9:5555"
    second = "192.168.1.10:5555"
    cfg.device.serial = first
    cfg.device.known = [first, second]
    cfg.device.macs = {
        first: "88:a4:79:1a:2b:3c",
        second: "88:a4:79:1a:2b:4d",
    }
    cfg.save(path)

    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert "mac" not in raw["device"]
    assert raw["device"]["macs"] == cfg.device.macs

    again = Config.load(path)
    assert again.device.macs == cfg.device.macs
    assert again.device.known == [first, second]
    assert again.validate() == []


def test_legacy_single_mac_is_migrated_to_device_map(tmp_path):
    """1.0.0 及更早的 device.mac 要挪到 macs[serial]，升级后仍能找回平板。"""
    path = tmp_path / "config.yaml"
    serial = "192.168.1.9:5555"
    path.write_text(
        "version: 1\n"
        "device:\n"
        f"  serial: {serial}\n"
        "  known: []\n"
        "  mac: 88:a4:79:1a:2b:3c\n",
        encoding="utf-8",
    )
    cfg = Config.load(path)
    assert cfg.device.macs == {serial: "88:a4:79:1a:2b:3c"}
    assert not hasattr(cfg.device, "mac")

    cfg.save(path)
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert raw["device"]["macs"] == {serial: "88:a4:79:1a:2b:3c"}
    assert "mac" not in raw["device"]


def test_macs_invalid_type_falls_back_to_default(tmp_path):
    path = tmp_path / "config.yaml"
    path.write_text(
        "version: 1\ndevice:\n  macs: [not, a, mapping]\n", encoding="utf-8"
    )
    cfg = Config.load(path)
    assert cfg.device.macs == {}


def test_layouts_roundtrip_by_device_and_orientation(tmp_path):
    """每台设备、每个方向一份布局，存盘再读回要完全一致。"""
    path = tmp_path / "config.yaml"
    cfg = Config()
    landscape = PlacementCfg(
        monitor="DISPLAY2",
        edge="bottom",
        span_start=-2560,
        span_end=0,
        offset=0.25,
        align="one_to_one",
        corner_priority="left",
    )
    portrait = PlacementCfg(edge="left", offset=0.75, align="proportional")
    cfg.layouts.set("AM4NBB5C23001691", "landscape", landscape)
    cfg.layouts.set("am4nbb5c23001691", "portrait", portrait)
    cfg.save(path)

    again = Config.load(path)
    saved_landscape = again.layouts.get("AM4NBB5C23001691", "landscape")
    saved_portrait = again.layouts.get("AM4NBB5C23001691", "portrait")
    assert saved_landscape is not None and saved_portrait is not None
    assert saved_landscape.edge == "bottom"
    assert saved_landscape.offset == 0.25
    assert saved_portrait.edge == "left"
    assert again.validate() == []


def test_layouts_invalid_profiles_are_reported_and_repaired(tmp_path):
    path = tmp_path / "config.yaml"
    path.write_text(
        textwrap.dedent(
            """\
            version: 1
            layouts:
              devices:
                "":
                  landscape:
                    edge: right
                SERIAL1:
                  landscape:
                    edge: diagonal
                    offset: 3.0
                    align: stretch
                  portrait: oops
                SERIAL2:
                  diagonal:
                    edge: right
                  landscape:
                    monitor: ""
            """
        ),
        encoding="utf-8",
    )
    cfg = Config.load(path)
    assert cfg.issues
    assert cfg.validate() == []
    profile = cfg.layouts.get("SERIAL1", "landscape")
    assert profile is not None
    assert profile.edge == "right"
    assert profile.offset == 1.0
    assert profile.align == "proportional"
    assert cfg.layouts.get("SERIAL1", "portrait") is None
    assert cfg.layouts.get("SERIAL2", "diagonal") is None
    assert cfg.layouts.get("SERIAL3", "landscape") is None


def test_layouts_default_empty():
    layouts = LayoutsCfg()
    assert layouts.devices == {}
    assert layouts.get("", "landscape") is None
    assert layouts.get("", "diagonal") is None
