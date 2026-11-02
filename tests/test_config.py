"""配置系统单测（设计文档 6.9 / 8.3）。"""

from __future__ import annotations

import textwrap

import yaml

import constants as C
from config import Config, DeviceCfg, MotionCfg, PlacementCfg


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


def test_corrupt_yaml_backs_up_and_resets(tmp_path):
    path = tmp_path / "broken.yaml"
    path.write_text(":: not yaml ::: [", encoding="utf-8")

    cfg = Config.load(path)
    assert cfg.validate() == []
    assert cfg.issues
    assert (tmp_path / "broken.yaml.bak").exists()


def test_future_version_resets(tmp_path):
    path = tmp_path / "future.yaml"
    path.write_text("version: 99\n", encoding="utf-8")

    cfg = Config.load(path)
    assert cfg.issues
    assert (tmp_path / "future.yaml.bak").exists()
    assert cfg.validate() == []


def test_validate_does_not_mutate():
    cfg = Config(device=DeviceCfg(server_port=1), placement=PlacementCfg(offset=2.0))
    problems = cfg.validate()
    assert any("server_port" in p for p in problems)
    assert any("offset" in p for p in problems)
    assert cfg.device.server_port == 1
    assert cfg.placement.offset == 2.0


def test_to_dict_version():
    raw = Config().to_dict()
    assert raw["version"] == 1
    assert set(raw) >= {"device", "placement", "motion", "keyboard", "hotkeys", "ui", "misc"}


def test_motion_defaults_are_sane():
    m = MotionCfg()
    assert 0.25 <= m.gain <= 3.0
    assert 1 <= m.hid_clamp <= 127
    assert m.arm_px >= 1 and m.exit_overflow_px >= 1
