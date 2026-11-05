"""视觉令牌（设计文档 6.10.3）。

唯一来源：QSS 模板 ``ui/theme/dark.qss`` 中的 ``@token`` 占位符由
``ui/style.py`` 用 :func:`qss_tokens` 的结果替换；Python 布局代码直接引用
本模块的常量，避免样式与代码各写一份数字。

本模块只允许 import 标准库（不得 import Qt）。
"""

from __future__ import annotations

from typing import Final

# ---------------------------------------------------------------------------
# 颜色
# ---------------------------------------------------------------------------
LIGHT_COLORS: Final[dict[str, str]] = {
    "bg_window": "#F4F6FA",
    "bg_surface": "#FFFFFF",
    "bg_raised": "#EEF2F7",
    "bg_sunken": "#E7ECF4",
    "border": "#D6DEE9",
    "border_strong": "#B7C3D3",
    "text_primary": "#18212C",
    "text_secondary": "#5D6B7D",
    "text_muted": "#8A94A3",
    "text_on_accent": "#FFFFFF",
    "accent": "#3D6FE0",
    "accent_hover": "#2D5FCC",
    "accent_soft": "#E3EBFB",
    "ok": "#1F9D6B",
    "warn": "#A96A12",
    "err": "#D14343",
    "capturing": "#D9485F",
    "disconnected": "#8794A5",
}

DARK_COLORS: Final[dict[str, str]] = {
    "bg_window": "#0E1116",
    "bg_surface": "#151A21",
    "bg_raised": "#1D242D",
    "bg_sunken": "#0A0D12",
    "border": "#27313C",
    "border_strong": "#3B4756",
    "text_primary": "#EDF2F7",
    "text_secondary": "#93A1B1",
    "text_muted": "#6E7B8A",
    "text_on_accent": "#071018",
    "accent": "#6E9BFF",
    "accent_hover": "#8FB2FF",
    "accent_soft": "#22304A",
    "ok": "#3DDC97",
    "warn": "#FFC857",
    "err": "#FF6B6B",
    "capturing": "#FF5C7A",
    "disconnected": "#5D6A79",
}

_THEME = "dark"
COLORS: dict[str, str] = dict(DARK_COLORS)

# 托盘/状态点颜色（6.10.7：灰=未连接 / 绿=空闲 / 红=接管中 / 黄=故障）
STATE_COLORS: dict[str, str] = {}


def _rebuild_state_colors() -> None:
    STATE_COLORS.clear()
    STATE_COLORS.update({
        "": COLORS["disconnected"],
        "disconnected": COLORS["disconnected"],
        "connecting": COLORS["accent"],
        "idle": COLORS["ok"],
        "arming": COLORS["warn"],
        "returning": COLORS["warn"],
        "captured": COLORS["capturing"],
        "fault": COLORS["err"],
    })


_rebuild_state_colors()


# 顶栏状态文案（与 State 枚举值一一对应）
STATE_LABELS: Final[dict[str, str]] = {
    "": "未连接",
    "disconnected": "未连接",
    "connecting": "连接中",
    "idle": "已连接",
    "arming": "外推中",
    "returning": "回程中",
    "captured": "接管中",
    "fault": "故障",
}

# 日志事件着色（事件名前缀）
LOG_EVENT_COLORS: dict[str, str] = {}


def _rebuild_log_colors() -> None:
    LOG_EVENT_COLORS.clear()
    LOG_EVENT_COLORS.update({
        "arm": COLORS["warn"],
        "cap": COLORS["capturing"],
        "hit": COLORS["ok"],
        "back": COLORS["accent"],
        "state": COLORS["text_secondary"],
        "tablet": COLORS["accent"],
        "kbd": COLORS["accent"],
        "cfg": COLORS["warn"],
        "ui": COLORS["text_secondary"],
        "err": COLORS["err"],
        "fault": COLORS["err"],
    })


_rebuild_log_colors()


def current_theme() -> str:
    return _THEME


def set_theme(name: str) -> str:
    """切换当前调色板并就地更新派生表（保留其他模块持有的 dict 引用）。"""
    global _THEME
    chosen = "light" if str(name).lower() == "light" else "dark"
    palette = LIGHT_COLORS if chosen == "light" else DARK_COLORS
    _THEME = chosen
    COLORS.clear()
    COLORS.update(palette)
    _rebuild_state_colors()
    _rebuild_log_colors()
    return chosen

# ---------------------------------------------------------------------------
# 尺寸
# ---------------------------------------------------------------------------
RADIUS: Final[dict[str, int]] = {"card": 12, "button": 8, "input": 6}

SPACING: Final[dict[str, int]] = {"xs": 4, "s": 8, "m": 12, "l": 16, "xl": 24}

CONTROL_HEIGHT: Final[dict[str, int]] = {"normal": 34, "compact": 28}

ICON_SIZE: Final[int] = 16
STATUS_DOT_SIZE: Final[int] = 10

WINDOW_DEFAULT_SIZE: Final[tuple[int, int]] = (1280, 820)
WINDOW_MIN_SIZE: Final[tuple[int, int]] = (1060, 680)
SETTINGS_PANEL_WIDTH: Final[int] = 340
SIDEBAR_WIDTH: Final[int] = 84
HEADER_HEIGHT: Final[int] = 64

# ---------------------------------------------------------------------------
# 字体
# ---------------------------------------------------------------------------
FONT_UI_FAMILIES: Final[tuple[str, ...]] = (
    "Segoe UI Variable Text",
    "Microsoft YaHei UI",
    "Segoe UI",
)
FONT_MONO_FAMILIES: Final[tuple[str, ...]] = ("Cascadia Mono", "Consolas", "Courier New")

FONT_SIZES: Final[dict[str, int]] = {"body": 13, "title": 15, "small": 11}


def font_families(mono: bool = False) -> list[str]:
    """返回 Qt 字体族候选列表（``QFont.setFamilies`` 用）。"""
    return list(FONT_MONO_FAMILIES if mono else FONT_UI_FAMILIES)


def state_color(state: str) -> str:
    """``State`` 值 → 状态点/托盘颜色；未知状态按未连接处理。"""
    return STATE_COLORS.get(state, COLORS["disconnected"])


def state_label(state: str) -> str:
    """``State`` 值 → 顶栏中文文案。"""
    return STATE_LABELS.get(state, state or STATE_LABELS[""])


def log_event_color(event: str) -> str:
    """日志事件名 → 颜色（按第一段前缀匹配，大小写不敏感）。"""
    key = event.split(".", 1)[0].lower()
    return LOG_EVENT_COLORS.get(key, COLORS["text_secondary"])


def qss_tokens() -> dict[str, str]:
    """扁平的 QSS 替换表：``@bg_window`` / ``@radius_card`` / ``@font_ui`` 等。"""
    table: dict[str, str] = dict(COLORS)
    for name, value in RADIUS.items():
        table[f"radius_{name}"] = f"{value}px"
    for name, value in SPACING.items():
        table[f"space_{name}"] = f"{value}px"
    table["control_h"] = f"{CONTROL_HEIGHT['normal']}px"
    table["control_h_compact"] = f"{CONTROL_HEIGHT['compact']}px"
    table["icon_size"] = f"{ICON_SIZE}px"
    table["dot_size"] = f"{STATUS_DOT_SIZE}px"
    table["font_ui"] = ", ".join(f'"{name}"' for name in FONT_UI_FAMILIES)
    table["font_mono"] = ", ".join(f'"{name}"' for name in FONT_MONO_FAMILIES)
    table["fs_body"] = f"{FONT_SIZES['body']}px"
    table["fs_title"] = f"{FONT_SIZES['title']}px"
    table["fs_small"] = f"{FONT_SIZES['small']}px"
    return table
