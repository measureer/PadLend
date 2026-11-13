"""配置系统（设计文档 6.9 / 8.3）。

路径：``%APPDATA%\\PadLend\\config.yaml``
- GUI 写入；手改后热加载（UI 层负责监听 mtime，本模块只负责解析/校验/落盘）。
- 非法值 → 回退默认 + 记录 issue（GUI 标红）+ 日志告警。
- ``version`` 驱动迁移；无法识别则备份 ``config.yaml.bad-<时间戳>`` 并重置。
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
LAYOUT_ORIENTATIONS = ("landscape", "portrait")

PLACEMENT_FIELD_TYPES: dict[str, type] = {
    "monitor": str,
    "edge": str,
    "span_start": int,
    "span_end": int,
    "offset": float,
    "align": str,
    "corner_priority": str,
}


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
    # 每台无线设备的 Wi-Fi MAC（地址 → MAC）：路由器换了 IP 之后靠它在局域网里
    # 找回来（0.3.13 只记最后一台，此后按设备分别保存）。
    macs: dict[str, str] = field(default_factory=dict)


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
class LayoutsCfg:
    """每台设备、每个屏幕方向的摆位（1.0.3）。

    ``devices`` 的键是设备身份（优先硬件序列号，见
    ``tablet/session.py`` 的 ``DeviceFacts.hardware_serial``）；二级键只有
    ``landscape`` / ``portrait``。旧配置文件没有这一段时为空，
    程序继续使用全局 ``placement`` 作为回退。
    """

    devices: dict[str, dict[str, PlacementCfg]] = field(default_factory=dict)

    def get(self, serial: str, orientation: str) -> PlacementCfg | None:
        profiles = self.devices.get(normalize_layout_key(serial))
        if not profiles:
            return None
        return profiles.get(str(orientation).strip().lower())

    def set(self, serial: str, orientation: str, placement: PlacementCfg) -> None:
        key = normalize_layout_key(serial)
        bucket = str(orientation).strip().lower()
        if not key or bucket not in LAYOUT_ORIENTATIONS:
            return
        self.devices.setdefault(key, {})[bucket] = placement

    def drop(self, serial: str) -> None:
        self.devices.pop(normalize_layout_key(serial), None)


def normalize_layout_key(serial: str) -> str:
    """设备身份归一化：去空白并忽略大小写（USB 序列号大小写不统一）。"""
    return str(serial or "").strip().casefold()


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
    # 预留字段：1.0.0 只有中文界面，本值不生效；保留是为了兼容
    # 0.3.x 已写出的 config.yaml（删键会让旧配置触发“未知配置项”告警）。
    language: str = "zh_CN"
    theme: str = "dark"
    tray_notifications: bool = True
    # 关闭主窗时：True = 收进托盘继续跑；False = 完全退出（0.3.12）
    close_to_tray: bool = False


@dataclass
class MiscCfg:
    clipboard_sync: bool = True
    keep_screen_on: bool = True
    restore_pointer_speed: int = -3
    # 1.4.0：可选把平板声音转发到 PC；需要重启 transport 才生效。
    forward_audio: bool = False


@dataclass
class Config:
    device: DeviceCfg = field(default_factory=DeviceCfg)
    placement: PlacementCfg = field(default_factory=PlacementCfg)
    # 1.0.3：每台设备 × 屏幕方向的摆位；placement 仍是全局回退
    layouts: LayoutsCfg = field(default_factory=LayoutsCfg)
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
                fh.flush()
                os.fsync(fh.fileno())  # 断电也不留半截配置
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
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            # read_text 解不开非 UTF-8：以前这里直接抛出，GUI 启动即崩。
            log.error("cfg.decode_failed path=%s err=%r", path, exc)
            backup = _backup(path, reason="encoding")
            cfg.add_issue(_reset_message("配置文件不是 UTF-8 编码", backup))
            return cfg
        except OSError as exc:
            log.error("cfg.read_failed path=%s err=%r", path, exc)
            cfg.add_issue("配置文件无法读取，本次使用默认值（文件未改动）")
            return cfg

        try:
            raw = yaml.safe_load(text) or {}
        except yaml.YAMLError as exc:
            log.error("cfg.parse_failed path=%s err=%r", path, exc)
            backup = _backup(path, reason="parse")
            cfg.add_issue(_reset_message("配置文件无法解析", backup))
            return cfg

        if not isinstance(raw, dict):
            log.error("cfg.not_mapping path=%s", path)
            backup = _backup(path, reason="not-mapping")
            cfg.add_issue(_reset_message("配置文件根节点不是映射", backup))
            return cfg

        version = raw.get("version", CURRENT_VERSION)
        try:
            version_int = int(version)
        except (TypeError, ValueError):
            log.error("cfg.bad_version value=%r", version)
            backup = _backup(path, reason="bad-version")
            cfg.add_issue(_reset_message(f"无法识别的配置版本 {version!r}", backup))
            return cfg

        if version_int > CURRENT_VERSION:
            log.error("cfg.future_version value=%s current=%s", version_int, CURRENT_VERSION)
            backup = _backup(path, reason="future-version")
            cfg.add_issue(_reset_message(f"配置版本 {version_int} 高于当前程序", backup))
            return cfg

        known_sections = {
            "version",
            "device",
            "placement",
            "layouts",
            "motion",
            "keyboard",
            "hotkeys",
            "ui",
            "misc",
        }
        unknown = sorted(set(raw) - known_sections)
        if unknown:
            # 未知键只告警不阻断：可能是新版本写回、也可能是手拍错。
            log.warning("cfg.unknown_keys keys=%s", unknown)
            cfg.add_issue(f"未知配置项已忽略：{', '.join(unknown)}")

        migrated = _migrate(raw, version_int, cfg)
        cls._fill(migrated, cfg)
        cfg._repair()
        if cfg._issues:
            log.warning("cfg.repaired issues=%s", len(cfg._issues))
        return cfg

    # -- 填充与修复 --------------------------------------------------------
    @classmethod
    def _fill(cls, raw: dict[str, Any], cfg: Config) -> None:
        device_raw = raw.get("device")
        if isinstance(device_raw, dict):
            # 0.3.13–1.0.0 只存了最后一台设备的 MAC；读旧配置时把它挪进
            # macs[serial]，用户升级后仍能用原来的地址找回平板。
            device_raw = dict(device_raw)
            legacy_mac = str(device_raw.pop("mac", "") or "").strip()
            serial = str(device_raw.get("serial", "") or "").strip()
            if legacy_mac and serial:
                macs = device_raw.get("macs")
                if not isinstance(macs, dict):
                    macs = {}
                macs = dict(macs)
                macs.setdefault(serial, legacy_mac)
                device_raw["macs"] = macs
        cfg.device = _section(
            DeviceCfg,
            device_raw,
            {
                "serial": str,
                "prefer_usb": bool,
                "auto_connect": bool,
                "server_port": int,
                "known": list,
                "macs": dict,
            },
        )
        cfg.placement = _section(
            PlacementCfg,
            raw.get("placement"),
            PLACEMENT_FIELD_TYPES,
        )
        cfg.layouts = _parse_layouts(raw.get("layouts"), cfg)
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
        ui_raw = raw.get("ui")
        if isinstance(ui_raw, dict):
            # 1.2.1 删除了「显示指针幽灵」空开关；旧配置里的键静默丢弃，
            # 不能让它触发 _section 的未知字段告警。
            ui_raw = dict(ui_raw)
            ui_raw.pop("show_pointer_ghost", None)
        cfg.ui = _section(
            UiCfg,
            ui_raw,
            {
                "language": str,
                "theme": str,
                "tray_notifications": bool,
                "close_to_tray": bool,
            },
        )
        cfg.misc = _section(
            MiscCfg,
            raw.get("misc"),
            {
                "clipboard_sync": bool,
                "keep_screen_on": bool,
                "restore_pointer_speed": int,
                "forward_audio": bool,
            },
        )

    def _repair(self) -> None:
        """把越界值夹回合法区间/回退默认，并记录 issue（6.9 校验规则）。"""
        d, m, k = self.device, self.motion, self.keyboard

        if not (1024 <= d.server_port <= 65535):
            self.add_issue(f"device.server_port={d.server_port} 越界，已恢复默认")
            d.server_port = C.DEFAULT_SERVER_PORT

        theme = str(self.ui.theme or "").lower()
        if theme not in ("dark", "light"):
            self.add_issue(f"ui.theme={self.ui.theme!r} 非法，已回退 dark")
            self.ui.theme = "dark"
        else:
            self.ui.theme = theme

        self._repair_placement(self.placement, "placement")
        for serial, profiles in list(self.layouts.devices.items()):
            if not profiles:
                del self.layouts.devices[serial]
                continue
            for orientation, profile in profiles.items():
                self._repair_placement(
                    profile, f"layouts.devices.{serial}.{orientation}"
                )

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

    def _repair_placement(self, p: PlacementCfg, prefix: str) -> None:
        if p.edge not in PLACEMENT_EDGES:
            self.add_issue(f"{prefix}.edge={p.edge!r} 非法，已回退 right")
            p.edge = "right"
        if p.align not in ALIGNS:
            self.add_issue(f"{prefix}.align={p.align!r} 非法，已回退 proportional")
            p.align = "proportional"
        if p.corner_priority not in ("", None) and p.corner_priority not in PLACEMENT_EDGES:
            self.add_issue(f"{prefix}.corner_priority={p.corner_priority!r} 非法，已回退 right")
            p.corner_priority = "right"
        if not (0.0 <= p.offset <= 1.0):
            self.add_issue(f"{prefix}.offset={p.offset} 越界，已夹紧到 [0,1]")
            p.offset = min(max(p.offset, 0.0), 1.0)
        if p.span_end <= p.span_start:
            self.add_issue(f"{prefix}.span_end 必须大于 span_start，已恢复默认")
            p.span_start, p.span_end = 0, 1600

    def validate(self) -> list[str]:
        """8.3 契约：返回问题列表，空 = 合法。不修改对象。"""
        problems: list[str] = []
        d, m, k = self.device, self.motion, self.keyboard

        if not (1024 <= d.server_port <= 65535):
            problems.append("device.server_port 必须在 1024..65535")
        problems.extend(_placement_issues(self.placement, "placement"))
        for serial, profiles in self.layouts.devices.items():
            for orientation, profile in profiles.items():
                problems.extend(
                    _placement_issues(
                        profile, f"layouts.devices.{serial}.{orientation}"
                    )
                )
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
def _parse_layouts(raw: Any, cfg: Config) -> LayoutsCfg:
    """解析 1.0.3 的嵌套布局表；坏条目逐条忽略，不影响其它设备。"""
    layouts = LayoutsCfg()
    if raw is None:
        return layouts
    if not isinstance(raw, dict):
        cfg.add_issue("layouts 必须是映射，已忽略")
        return layouts
    unknown = sorted(set(raw) - {"devices"})
    if unknown:
        cfg.add_issue(f"layouts 未知配置项已忽略：{', '.join(unknown)}")
    devices = raw.get("devices")
    if devices is None:
        return layouts
    if not isinstance(devices, dict):
        cfg.add_issue("layouts.devices 必须是映射，已忽略")
        return layouts
    for serial, profiles in devices.items():
        key = normalize_layout_key(serial)
        if not key or not isinstance(profiles, dict):
            cfg.add_issue(f"layouts.devices.{serial} 非法，已忽略")
            continue
        parsed: dict[str, PlacementCfg] = {}
        for orientation, payload in profiles.items():
            bucket = str(orientation).strip().lower()
            if bucket not in LAYOUT_ORIENTATIONS:
                cfg.add_issue(
                    f"layouts.devices.{serial}.{orientation} 方向非法"
                    "（只支持 landscape/portrait），已忽略"
                )
                continue
            if not isinstance(payload, dict):
                cfg.add_issue(f"layouts.devices.{serial}.{orientation} 必须是映射，已忽略")
                continue
            parsed[bucket] = _section(PlacementCfg, payload, PLACEMENT_FIELD_TYPES)
        if parsed:
            layouts.devices[key] = parsed
    return layouts


def _placement_issues(p: PlacementCfg, prefix: str) -> list[str]:
    problems: list[str] = []
    if p.edge not in PLACEMENT_EDGES:
        problems.append(f"{prefix}.edge 必须是 left/right/top/bottom")
    if p.align not in ALIGNS:
        problems.append(f"{prefix}.align 必须是 proportional/center/one_to_one")
    if p.corner_priority not in ("", None) and p.corner_priority not in PLACEMENT_EDGES:
        problems.append(f"{prefix}.corner_priority 必须是合法边或空")
    if not (0.0 <= p.offset <= 1.0):
        problems.append(f"{prefix}.offset 必须在 0.0..1.0")
    if p.span_end <= p.span_start:
        problems.append(f"{prefix}.span_end 必须大于 span_start")
    return problems


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
            elif expected is dict:
                if not isinstance(value, dict):
                    raise TypeError("not a mapping")
                coerced = {
                    str(key).strip(): str(item).strip()
                    for key, item in value.items()
                    if str(key).strip() and str(item).strip()
                }
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
    unknown = sorted(set(raw) - set(field_types))
    if unknown:
        log.warning("cfg.unknown_fields section=%s keys=%s", cls.__name__, unknown)
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


def _reset_message(reason: str, backup: Path | None) -> str:
    if backup is None:
        return f"{reason}，已重置为默认值（备份失败，请检查目录权限）"
    return f"{reason}，已备份为 {backup.name} 并重置为默认值"


def _backup(path: Path, *, reason: str = "invalid") -> Path | None:
    """把损坏配置另存为 config.yaml.bad-<时间戳>，返回备份路径。

    旧版只写 .bak 且会覆盖，用户连续两次损坏后原始内容就丢了；带时间戳
    的文件可以一直保留，便于用户发回来定位问题。
    """
    from datetime import datetime

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    target = path.with_name(f"{path.name}.bad-{stamp}")
    try:
        shutil.copy2(path, target)
        log.warning("cfg.backup path=%s reason=%s", target, reason)
        return target
    except OSError as exc:  # pragma: no cover
        log.error("cfg.backup_failed path=%s err=%r", path, exc)
        return None


def load_config(path: Path | None = None) -> Config:
    """快捷入口：默认路径为 ``%APPDATA%\\PadLend\\config.yaml``。"""
    return Config.load(Path(path) if path is not None else C.config_path())
