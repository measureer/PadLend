"""配置系统（设计文档 6.9 / 8.3）。

路径：``%APPDATA%\\PadLend\\config.yaml``
- GUI 写入；手改后热加载（UI 层负责监听 mtime，本模块只负责解析/校验/落盘）。
- 非法值 → 回退默认 + 记录 issue（GUI 标红）+ 日志告警。
- ``version`` 驱动迁移；无法识别则备份 ``config.yaml.bak`` 并重置。
"""

from __future__ import annotations

import contextlib
import logging
import os
import shutil
import tempfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

import yaml

import constants as C

log = logging.getLogger("config")

CURRENT_VERSION = 1

PLACEMENT_EDGES = ("left", "right", "top", "bottom")
ALIGNS = ("proportional", "center", "one_to_one")


# ---------------------------------------------------------------------------
# 各配置段（8.3）
# ---------------------------------------------------------------------------
@dataclass
class DeviceCfg:
    serial: str = ""  # 空 = 自动选择
    prefer_usb: bool = True
    auto_connect: bool = True
    server_port: int = C.DEFAULT_SERVER_PORT
    # 最近连接过的设备（新的在前，最多 8 条）：设备页用它在离线时也列出来。
    known: list[str] = field(default_factory=list)
    # 上次那台平板的 Wi-Fi MAC：路由器换了 IP 之后靠它在局域网里找回来（0.3.13）。
    mac: str = ""


@dataclass
class PlacementCfg:
    monitor: str = ""  # "\\\\.\\DISPLAY1"；空 = 主屏
    edge: str = "right"  # left | right | top | bottom
    span_start: int = 0
    span_end: int = 1600
    offset: float = 0.5
    align: str = "proportional"  # proportional | center | one_to_one
    corner_priority: str = "right"


@dataclass
class MotionCfg:
    gain: float = C.DEFAULT_GAIN  # 0.25 ~ 3.0
    report_hz: int = C.DEFAULT_REPORT_HZ  # 125 ~ 1000
    hid_clamp: int = C.HID_CLAMP  # 1 ~ 127
    entry_margin_px: int = C.ENTRY_MARGIN_PX
    exit_overflow_px: int = C.EXIT_OVERFLOW_PX
    debounce_ms: int = C.DEBOUNCE_MS
    arm_px: int = C.ARM_PX
    arm_window_ms: int = C.ARM_WINDOW_MS
    edge_band_px: int = C.EDGE_BAND_PX
    boundary_inset_px: int = C.BOUNDARY_INSET_PX  # H3 规避手势时 8~16


@dataclass
class KeyboardCfg:
    forward: bool = True
    suppress_pc: bool = True
    layout: str = "us"
    passthrough_keys: list[str] = field(default_factory=lambda: ["VK_LWIN", "VK_RWIN"])


@dataclass
class HotkeyCfg:
    toggle: str = "Ctrl+Alt+S"
    panic: str = "Ctrl+Alt+Q"


@dataclass
class UiCfg:
    language: str = "zh_CN"
    theme: str = "dark"
    tray_notifications: bool = True
    show_pointer_ghost: bool = True
    # 关闭主窗时：True = 收进托盘继续跑；False = 完全退出（0.3.12）
    close_to_tray: bool = False


@dataclass
class MiscCfg:
    clipboard_sync: bool = True
    keep_screen_on: bool = True
    restore_pointer_speed: int = -3


@dataclass
class Config:
    device: DeviceCfg = field(default_factory=DeviceCfg)
    placement: PlacementCfg = field(default_factory=PlacementCfg)
    motion: MotionCfg = field(default_factory=MotionCfg)
    keyboard: KeyboardCfg = field(default_factory=KeyboardCfg)
    hotkeys: HotkeyCfg = field(default_factory=HotkeyCfg)
    ui: UiCfg = field(default_factory=UiCfg)
    misc: MiscCfg = field(default_factory=MiscCfg)

    def __post_init__(self) -> None:
        # 非契约字段：最近一次加载/修复产生的问题（GUI 用来标红）。
        self._issues: list[str] = []

    # -- 问题记录 ----------------------------------------------------------
    @property
    def issues(self) -> list[str]:
        return list(self._issues)

    def add_issue(self, message: str) -> None:
        if message not in self._issues:
            self._issues.append(message)
            log.warning("cfg.invalid %s", message)

    # -- 序列化 ------------------------------------------------------------
    def to_dict(self) -> dict[str, Any]:
        return {"version": CURRENT_VERSION, **asdict(self)}

    def save(self, path: Path) -> None:
        """原子写入 YAML（先写临时文件再 replace，避免半截文件）。"""
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = yaml.safe_dump(
            self.to_dict(), sort_keys=False, allow_unicode=True, default_flow_style=False
        )
        fd, tmp = tempfile.mkstemp(prefix="config.", suffix=".tmp", dir=str(path.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as fh:
                fh.write(payload)
            os.replace(tmp, path)
        finally:
            if os.path.exists(tmp):
                with contextlib.suppress(OSError):  # pragma: no cover - 极端文件系统错误
                    os.unlink(tmp)

    @classmethod
    def load(cls, path: Path) -> Config:
        path = Path(path)
        cfg = cls()
        if not path.exists():
            log.info("cfg.default created path=%s", path)
            return cfg

        try:
            raw = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        except (OSError, yaml.YAMLError) as exc:
            log.error("cfg.parse_failed path=%s err=%r", path, exc)
            _backup(path)
            cfg.add_issue("配置文件无法解析，已重置为默认值")
            return cfg

        if not isinstance(raw, dict):
            log.error("cfg.not_mapping path=%s", path)
            _backup(path)
            cfg.add_issue("配置文件根节点不是映射，已重置为默认值")
            return cfg

        version = raw.get("version", CURRENT_VERSION)
        try:
            version_int = int(version)
        except (TypeError, ValueError):
            log.error("cfg.bad_version value=%r", version)
            _backup(path)
            cfg.add_issue(f"无法识别的配置版本 {version!r}，已重置为默认值")
            return cfg

        if version_int > CURRENT_VERSION:
            log.error("cfg.future_version value=%s current=%s", version_int, CURRENT_VERSION)
            _backup(path)
            cfg.add_issue(f"配置版本 {version_int} 高于当前程序，已重置为默认值")
            return cfg

        migrated = _migrate(raw, version_int, cfg)
        cls._fill(migrated, cfg)
        cfg._repair()
        if cfg._issues:
            log.warning("cfg.repaired issues=%s", len(cfg._issues))
        return cfg

    # -- 填充与修复 --------------------------------------------------------
    @classmethod
    def _fill(cls, raw: dict[str, Any], cfg: Config) -> None:
        cfg.device = _section(
            DeviceCfg,
            raw.get("device"),
            {
                "serial": str,
                "prefer_usb": bool,
                "auto_connect": bool,
                "server_port": int,
                "known": list,
                "mac": str,
            },
        )
        cfg.placement = _section(
            PlacementCfg,
            raw.get("placement"),
            {
                "monitor": str,
                "edge": str,
                "span_start": int,
                "span_end": int,
                "offset": float,
                "align": str,
                "corner_priority": str,
            },
        )
        cfg.motion = _section(
            MotionCfg,
            raw.get("motion"),
            {
                "gain": float,
                "report_hz": int,
                "hid_clamp": int,
                "entry_margin_px": int,
                "exit_overflow_px": int,
                "debounce_ms": int,
                "arm_px": int,
                "arm_window_ms": int,
                "edge_band_px": int,
                "boundary_inset_px": int,
            },
        )
        cfg.keyboard = _section(
            KeyboardCfg,
            raw.get("keyboard"),
            {
                "forward": bool,
                "suppress_pc": bool,
                "layout": str,
                "passthrough_keys": list,
            },
        )
        cfg.hotkeys = _section(
            HotkeyCfg,
            raw.get("hotkeys"),
            {"toggle": str, "panic": str},
        )
        cfg.ui = _section(
            UiCfg,
            raw.get("ui"),
            {
                "language": str,
                "theme": str,
                "tray_notifications": bool,
                "show_pointer_ghost": bool,
                "close_to_tray": bool,
            },
        )
        cfg.misc = _section(
            MiscCfg,
            raw.get("misc"),
            {"clipboard_sync": bool, "keep_screen_on": bool, "restore_pointer_speed": int},
        )

    def _repair(self) -> None:
        """把越界值夹回合法区间/回退默认，并记录 issue（6.9 校验规则）。"""
        d, p, m, k = self.device, self.placement, self.motion, self.keyboard

        if not (1024 <= d.server_port <= 65535):
            self.add_issue(f"device.server_port={d.server_port} 越界，已恢复默认")
            d.server_port = C.DEFAULT_SERVER_PORT

        theme = str(self.ui.theme or "").lower()
        if theme not in ("dark", "light"):
            self.add_issue(f"ui.theme={self.ui.theme!r} 非法，已回退 dark")
            self.ui.theme = "dark"
        else:
            self.ui.theme = theme

        if p.edge not in PLACEMENT_EDGES:
            self.add_issue(f"placement.edge={p.edge!r} 非法，已回退 right")
            p.edge = "right"
        if p.align not in ALIGNS:
            self.add_issue(f"placement.align={p.align!r} 非法，已回退 proportional")
            p.align = "proportional"
        if p.corner_priority not in ("", None) and p.corner_priority not in PLACEMENT_EDGES:
            self.add_issue(f"placement.corner_priority={p.corner_priority!r} 非法，已回退 right")
            p.corner_priority = "right"
        if not (0.0 <= p.offset <= 1.0):
            self.add_issue(f"placement.offset={p.offset} 越界，已夹紧到 [0,1]")
            p.offset = min(max(p.offset, 0.0), 1.0)
        if p.span_end <= p.span_start:
            self.add_issue("placement.span_end 必须大于 span_start，已恢复默认")
            p.span_start, p.span_end = 0, 1600

        if not (1 <= m.hid_clamp <= C.HID_CLAMP):
            self.add_issue(f"motion.hid_clamp={m.hid_clamp} 越界，已夹紧到 [1,127]")
            m.hid_clamp = min(max(m.hid_clamp, 1), C.HID_CLAMP)
        if m.report_hz not in C.REPORT_HZ_CHOICES:
            nearest = min(C.REPORT_HZ_CHOICES, key=lambda hz: abs(hz - int(m.report_hz)))
            self.add_issue(
                f"motion.report_hz={m.report_hz} 不是固定档位，已改成 {nearest}"
            )
            m.report_hz = nearest
        if not (0.25 <= m.gain <= 3.0):
            self.add_issue(f"motion.gain={m.gain} 越界，已夹紧到 [0.25,3.0]")
            m.gain = min(max(m.gain, 0.25), 3.0)
        for name in ("arm_px", "exit_overflow_px", "edge_band_px", "arm_window_ms"):
            if getattr(m, name) < 1:
                self.add_issue(f"motion.{name} 必须 ≥1，已恢复默认")
                setattr(m, name, getattr(MotionCfg(), name))
        for name in ("debounce_ms", "boundary_inset_px", "entry_margin_px"):
            if getattr(m, name) < 0:
                self.add_issue(f"motion.{name} 不能为负，已恢复默认")
                setattr(m, name, getattr(MotionCfg(), name))
        if m.arm_window_ms < 1:
            self.add_issue("motion.arm_window_ms 必须 ≥1，已恢复默认")
            m.arm_window_ms = C.ARM_WINDOW_MS

        if k.layout.strip() == "":
            self.add_issue("keyboard.layout 为空，已回退 us")
            k.layout = "us"
        if not isinstance(k.passthrough_keys, list):
            self.add_issue("keyboard.passthrough_keys 不是列表，已回退默认")
            k.passthrough_keys = ["VK_LWIN", "VK_RWIN"]
        k.passthrough_keys = [str(x) for x in k.passthrough_keys]

        for name in ("toggle", "panic"):
            value = getattr(self.hotkeys, name)
            if not isinstance(value, str) or not value.strip():
                self.add_issue(f"hotkeys.{name} 为空，已恢复默认")
                setattr(self.hotkeys, name, getattr(HotkeyCfg(), name))

        if self.misc.restore_pointer_speed == 0 and self.misc.restore_pointer_speed != -3:
            # 0 是合法的运行值，但记录一次以免用户误以为没生效
            pass

    def validate(self) -> list[str]:
        """8.3 契约：返回问题列表，空 = 合法。不修改对象。"""
        problems: list[str] = []
        d, p, m, k = self.device, self.placement, self.motion, self.keyboard

        if not (1024 <= d.server_port <= 65535):
            problems.append("device.server_port 必须在 1024..65535")
        if p.edge not in PLACEMENT_EDGES:
            problems.append("placement.edge 必须是 left/right/top/bottom")
        if p.align not in ALIGNS:
            problems.append("placement.align 必须是 proportional/center/one_to_one")
        if p.corner_priority not in ("", None) and p.corner_priority not in PLACEMENT_EDGES:
            problems.append("placement.corner_priority 必须是合法边或空")
        if not (0.0 <= p.offset <= 1.0):
            problems.append("placement.offset 必须在 0.0..1.0")
        if p.span_end <= p.span_start:
            problems.append("placement.span_end 必须大于 span_start")
        if not (1 <= m.hid_clamp <= C.HID_CLAMP):
            problems.append("motion.hid_clamp 必须在 1..127")
        if m.report_hz not in C.REPORT_HZ_CHOICES:
            problems.append("motion.report_hz 必须是 125/250/500/1000 之一")
        if not (0.25 <= m.gain <= 3.0):
            problems.append("motion.gain 必须在 0.25..3.0")
        if m.arm_px < 1:
            problems.append("motion.arm_px 必须 ≥1")
        if m.arm_window_ms < 1:
            problems.append("motion.arm_window_ms 必须 ≥1")
        if m.exit_overflow_px < 1:
            problems.append("motion.exit_overflow_px 必须 ≥1")
        if m.edge_band_px < 1:
            problems.append("motion.edge_band_px 必须 ≥1")
        if m.debounce_ms < 0:
            problems.append("motion.debounce_ms 不能为负")
        if m.boundary_inset_px < 0:
            problems.append("motion.boundary_inset_px 不能为负")
        if not k.layout.strip():
            problems.append("keyboard.layout 不能为空")
        if not isinstance(k.passthrough_keys, list):
            problems.append("keyboard.passthrough_keys 必须是列表")
        for name in ("toggle", "panic"):
            if not getattr(self.hotkeys, name).strip():
                problems.append(f"hotkeys.{name} 不能为空")
        return problems


# ---------------------------------------------------------------------------
# 辅助
# ---------------------------------------------------------------------------
def _section(cls: type, raw: Any, field_types: dict[str, type]) -> Any:
    """按字段类型从 raw 构造配置段；缺失/类型错误一律用默认值。"""
    obj = cls()
    if not isinstance(raw, dict):
        if raw is not None:
            log.warning("cfg.section_invalid cls=%s raw=%r", cls.__name__, raw)
        return obj
    for name, expected in field_types.items():
        if name not in raw:
            continue
        value = raw[name]
        default = getattr(obj, name)
        try:
            if expected is bool:
                coerced = value if isinstance(value, bool) else _as_bool(value)
            elif expected is int:
                coerced = int(value)
            elif expected is float:
                coerced = float(value)
            elif expected is list:
                if not isinstance(value, list):
                    raise TypeError("not a list")
                coerced = [str(x) for x in value]
            else:
                coerced = str(value)
        except (TypeError, ValueError):
            log.warning("cfg.field_invalid %s.%s=%r", cls.__name__, name, value)
            continue
        if expected is int and isinstance(default, int) and not isinstance(default, bool):
            coerced = int(coerced)
        elif expected is float and isinstance(default, float):
            coerced = float(coerced)
        setattr(obj, name, coerced)
    return obj


def _as_bool(value: Any) -> bool:
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "yes", "on", "y"}
    return bool(value)


def _migrate(raw: dict[str, Any], version: int, cfg: Config) -> dict[str, Any]:
    """逐级升级到 CURRENT_VERSION。目前只有 v1，保留扩展点。"""
    data = dict(raw)
    while version < CURRENT_VERSION:
        step = _MIGRATIONS.get(version)
        if step is None:
            log.warning("cfg.no_migration version=%s", version)
            break
        data = step(data)
        version += 1
    return data


_MIGRATIONS: dict[int, Any] = {}


def _backup(path: Path) -> None:
    try:
        shutil.copy2(path, path.with_name(path.name + ".bak"))
        log.warning("cfg.backup path=%s", path.with_name(path.name + ".bak"))
    except OSError as exc:  # pragma: no cover
        log.error("cfg.backup_failed path=%s err=%r", path, exc)


def default_config() -> Config:
    return Config()


def load_config(path: Path | None = None) -> Config:
    """快捷入口：默认路径为 ``%APPDATA%\\PadLend\\config.yaml``。"""
    return Config.load(Path(path) if path is not None else C.config_path())


def save_config(cfg: Config, path: Path | None = None) -> None:
    cfg.save(Path(path) if path is not None else C.config_path())
