"""Python 业务对象 ⇄ JSON DTO 的唯一转换点。

前端 ``src/api/types.ts`` 与这里的字段一一对应；契约集中在两个文件，
避免旧版 GUI 里「每个控件各自 getattr 解析一遍」的重复代码。
"""

from __future__ import annotations

import contextlib
from dataclasses import asdict, fields, is_dataclass
from typing import Any

from config import Config, LayoutsCfg, PlacementCfg
from core.geometry import Align, Edge, EdgeSegment, Placement

EDGE_VALUES = ("left", "right", "top", "bottom")
ALIGN_VALUES = ("proportional", "center", "one_to_one")


# --------------------------------------------------------------------------- 基础
def rect_dict(rect: Any) -> dict[str, Any]:
    left, top, right, bottom = (
        int(getattr(rect, "left", 0) or 0),
        int(getattr(rect, "top", 0) or 0),
        int(getattr(rect, "right", 0) or 0),
        int(getattr(rect, "bottom", 0) or 0),
    )
    return {
        "left": left,
        "top": top,
        "right": right,
        "bottom": bottom,
        "width": right - left,
        "height": bottom - top,
    }


def monitor_dict(monitor: Any) -> dict[str, Any]:
    return {
        "handle": int(getattr(monitor, "handle", 0) or 0),
        "device": str(getattr(monitor, "device", "") or ""),
        "name": str(getattr(monitor, "name", "") or ""),
        "rect": rect_dict(getattr(monitor, "rect", None)),
        "dpi": int(getattr(monitor, "dpi", 96) or 96),
        "scale": float(getattr(monitor, "scale", 1.0) or 1.0),
        "primary": bool(getattr(monitor, "primary", False)),
    }


def segment_dict(segment: Any) -> dict[str, Any]:
    edge = getattr(segment, "edge", None)
    return {
        "monitorHandle": int(getattr(segment, "monitor_handle", 0) or 0),
        "edge": str(getattr(edge, "value", edge) or "right"),
        "spanStart": int(getattr(segment, "span_start", 0) or 0),
        "spanEnd": int(getattr(segment, "span_end", 0) or 0),
        "length": int(getattr(segment, "length", 0) or 0),
    }


def placement_dict(placement: Any) -> dict[str, Any] | None:
    if placement is None:
        return None
    segment = getattr(placement, "segment", None)
    if segment is None:
        return None
    align = getattr(placement, "align", Align.PROPORTIONAL)
    corner = getattr(placement, "corner_priority", None)
    return {
        **segment_dict(segment),
        "offset": float(getattr(placement, "offset", 0.5) or 0.0),
        "align": str(getattr(align, "value", align) or "proportional"),
        "cornerPriority": str(getattr(corner, "value", corner) or "") or None,
    }


def placement_from_dict(data: dict[str, Any]) -> Placement:
    edge_text = str(data.get("edge", "right"))
    if edge_text not in EDGE_VALUES:
        edge_text = "right"
    segment = EdgeSegment(
        monitor_handle=int(data.get("monitorHandle", 0) or 0),
        edge=Edge(edge_text),
        span_start=int(data.get("spanStart", 0) or 0),
        span_end=int(data.get("spanEnd", 0) or 0),
    )
    align_text = str(data.get("align", "proportional"))
    if align_text not in ALIGN_VALUES:
        align_text = "proportional"
    corner_text = str(data.get("cornerPriority") or "")
    corner = Edge(corner_text) if corner_text in EDGE_VALUES else None
    offset = max(0.0, min(1.0, float(data.get("offset", 0.5) or 0.0)))
    return Placement(segment, offset, Align(align_text), corner)


def facts_dict(facts: Any) -> dict[str, Any] | None:
    if facts is None:
        return None
    width = int(getattr(facts, "width", 0) or 0)
    height = int(getattr(facts, "height", 0) or 0)
    if width and height:
        orientation_text = "landscape" if width >= height else "portrait"
    else:
        orientation_text = (
            "portrait" if int(getattr(facts, "orientation", 0) or 0) & 1 else "landscape"
        )
    return {
        "serial": str(getattr(facts, "serial", "") or ""),
        "model": str(getattr(facts, "model", "") or "").replace("_", " "),
        "width": width,
        "height": height,
        "orientation": orientation_text,
        "xscale": float(getattr(facts, "xscale", 1.0) or 0.0),
        "pointerSpeed": int(getattr(facts, "pointer_speed", 0) or 0),
        "hardwareSerial": str(getattr(facts, "hardware_serial", "") or ""),
    }


def device_entry_dict(entry: Any) -> dict[str, Any]:
    serial = str(getattr(entry, "serial", "") or "")
    state = str(getattr(entry, "state", "") or "")
    wireless = bool(getattr(entry, "is_wireless", ":" in serial))
    return {
        "serial": serial,
        "state": state,
        "model": str(getattr(entry, "model", "") or "").replace("_", " "),
        "transport": "无线" if wireless else "USB",
        "online": state == "device",
        "remembered": False,
        "located": "",
        "locatedDetail": "",
    }


def located_dict(located: Any) -> dict[str, Any]:
    return {
        "status": str(getattr(located, "status", "offline") or "offline"),
        "address": str(getattr(located, "address", "") or ""),
        "ip": str(getattr(located, "ip", "") or ""),
        "mac": str(getattr(located, "mac", "") or ""),
        "port": int(getattr(located, "port", 0) or 0),
        "detail": str(getattr(located, "detail", "") or ""),
    }


def desktop_dict(desktop: Any) -> dict[str, Any]:
    try:
        monitors = tuple(desktop.monitors)
    except Exception:  # noqa: BLE001 - 枚举失败时给前端一个可用的空桌面
        monitors = ()
    try:
        segments = tuple(desktop.exterior_segments())
    except Exception:  # noqa: BLE001
        segments = ()
    box = rect_dict(getattr(desktop, "bounding_box", None))
    return {
        "monitors": [monitor_dict(m) for m in monitors],
        "segments": [segment_dict(s) for s in segments],
        "boundingBox": box,
    }


# --------------------------------------------------------------------------- 配置
def config_dict(cfg: Config) -> dict[str, Any]:
    """``Config.to_dict()`` 已经递归 asdict；这里再兜一层，保证全是 JSON 原生值。"""
    data = cfg.to_dict()
    return _jsonable(data)


def _jsonable(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return _jsonable(asdict(value))
    if isinstance(value, dict):
        return {str(k): _jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_jsonable(v) for v in value]
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    return str(value)


def _coerce(value: Any, expected: type) -> Any:
    if expected is bool:
        if isinstance(value, bool):
            return value
        return str(value).strip().lower() in {"1", "true", "yes", "on"}
    if expected is int:
        return int(value)
    if expected is float:
        return float(value)
    if expected is str:
        return str(value)
    if expected is list:
        return [str(x) for x in value] if isinstance(value, (list, tuple)) else []
    if expected is dict:
        return dict(value) if isinstance(value, dict) else {}
    return value


def _type_of(default: Any) -> type:
    if isinstance(default, bool):
        return bool
    if isinstance(default, int):
        return int
    if isinstance(default, float):
        return float
    if isinstance(default, str):
        return str
    if isinstance(default, list):
        return list
    if isinstance(default, dict):
        return dict
    return str


def apply_config_patch(cfg: Config, patch: dict[str, Any]) -> list[str]:
    """把前端传来的配置补丁写回 dataclass（保留未提交的字段）。

    与旧版「设置页逐控件赋值」不同，这里按 dataclass 字段元数据统一处理，
    类型错误直接跳过并回报，不会写坏配置对象。
    """
    problems: list[str] = []
    for section_name, section in vars(cfg).items():
        if section_name.startswith("_") or not is_dataclass(section):
            continue
        raw = patch.get(section_name)
        if not isinstance(raw, dict):
            continue
        if section_name == "layouts":
            continue  # 布局由 layout.save 单独维护，设置页不得覆盖
        for field in fields(section):
            if field.name not in raw:
                continue
            expected = _type_of(getattr(section, field.name))
            try:
                setattr(section, field.name, _coerce(raw[field.name], expected))
            except (TypeError, ValueError) as exc:
                problems.append(f"{section_name}.{field.name} 非法值：{exc}")
    # layouts.devices 允许显式覆盖（用于迁移/备份恢复），逐条校验方向。
    layouts_raw = patch.get("layouts")
    if isinstance(layouts_raw, dict) and isinstance(layouts_raw.get("devices"), dict):
        rebuilt = LayoutsCfg()
        for serial, profiles in layouts_raw["devices"].items():
            if not isinstance(profiles, dict):
                continue
            for orientation, payload in profiles.items():
                if orientation not in ("landscape", "portrait") or not isinstance(payload, dict):
                    continue
                profile = PlacementCfg()
                for field in fields(profile):
                    if field.name not in payload:
                        continue
                    with contextlib.suppress(TypeError, ValueError):
                        setattr(
                            profile,
                            field.name,
                            _coerce(payload[field.name], _type_of(getattr(profile, field.name))),
                        )
                rebuilt.set(str(serial), str(orientation), profile)
        cfg.layouts = rebuilt
    return problems
