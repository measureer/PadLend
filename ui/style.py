"""主题装配：令牌 → QSS → QApplication（设计文档 6.10.3 / 6.10.5）。

``dark.qss`` 是带 ``@token`` 占位符的模板；本模块负责替换、缓存并把样式与
调色板/字体装到 ``QApplication`` 上。窗口构造时不重复读文件（进程内只读一次），
开发期改主题可调用 :func:`reload_stylesheet`。
"""

from __future__ import annotations

import logging
import re
from functools import lru_cache
from pathlib import Path
from typing import Final

from PySide6.QtGui import QColor, QFont, QPalette
from PySide6.QtWidgets import QApplication, QWidget

from ui.theme.tokens import (
    COLORS,
    FONT_SIZES,
    FONT_UI_FAMILIES,
    qss_tokens,
)

log = logging.getLogger("ui.style")

THEME_DIR: Final[Path] = Path(__file__).resolve().parent / "theme"
DEFAULT_QSS_PATH: Final[Path] = THEME_DIR / "dark.qss"

# 模板占位符：@bg_window / @radius_card / @font_ui ...（小写+数字+下划线）
_TOKEN_RE: Final[re.Pattern[str]] = re.compile(r"@([a-z0-9_]+)")

# 极端情况下模板缺失时的最小可用样式（不让 UI 变成白底黑字）
_FALLBACK_TEMPLATE: Final[str] = """
QWidget { background-color: @bg_window; color: @text_primary;
          font-family: @font_ui; font-size: @fs_body; }
QFrame#card { background-color: @bg_surface; border: 1px solid @border;
              border-radius: @radius_card; }
"""


def render_qss(template: str, values: dict[str, str] | None = None) -> str:
    """把模板里的 ``@token`` 替换成令牌值；未知令牌保留原样并记一条告警。"""
    table = qss_tokens() if values is None else values
    missing: set[str] = set()

    def _replace(match: re.Match[str]) -> str:
        name = match.group(1)
        if name in table:
            return table[name]
        missing.add(name)
        return match.group(0)

    rendered = _TOKEN_RE.sub(_replace, template)
    if missing:
        log.warning("qss unknown tokens: %s", ", ".join(sorted(missing)))
    return rendered


def load_qss_template(path: Path | None = None) -> str:
    """读取 QSS 模板；文件缺失/不可读时退回内置最小模板。"""
    target = Path(path) if path is not None else DEFAULT_QSS_PATH
    try:
        return target.read_text(encoding="utf-8")
    except OSError as exc:
        log.error("qss template unavailable path=%s err=%r", target, exc)
        return _FALLBACK_TEMPLATE


@lru_cache(maxsize=1)
def stylesheet() -> str:
    """渲染后的 QSS（进程内缓存；canvas 等延迟创建的控件也会命中）。"""
    return render_qss(load_qss_template())


def reload_stylesheet() -> str:
    """清缓存并重新渲染（开发期/主题热切换用）。"""
    stylesheet.cache_clear()
    return stylesheet()


def build_font(point_size: int | None = None) -> QFont:
    """UI 正文字体；族名按 ``Segoe UI Variable Text`` → ``Microsoft YaHei UI`` 回退。"""
    font = QFont()
    font.setFamilies(list(FONT_UI_FAMILIES))
    font.setPointSize(point_size if point_size is not None else FONT_SIZES["body"])
    font.setStyleStrategy(QFont.StyleStrategy.PreferAntialias)
    return font


def build_mono_font(point_size: int | None = None) -> QFont:
    """日志/数值用等宽字体（``Cascadia Mono`` → ``Consolas``）。"""
    from ui.theme.tokens import FONT_MONO_FAMILIES  # 局部 import：避免顶部再列一次

    font = QFont()
    font.setFamilies(list(FONT_MONO_FAMILIES))
    font.setPointSize(point_size if point_size is not None else FONT_SIZES["small"])
    return font


def build_palette() -> QPalette:
    """当前主题调色板：给 QSS 覆盖不到的绘制路径（原生控件/滚动区）兜底。"""
    palette = QPalette()
    window = QColor(COLORS["bg_window"])
    surface = QColor(COLORS["bg_surface"])
    raised = QColor(COLORS["bg_raised"])
    text = QColor(COLORS["text_primary"])
    muted = QColor(COLORS["text_secondary"])
    accent = QColor(COLORS["accent"])

    palette.setColor(QPalette.ColorRole.Window, window)
    palette.setColor(QPalette.ColorRole.WindowText, text)
    palette.setColor(QPalette.ColorRole.Base, QColor(COLORS["bg_sunken"]))
    palette.setColor(QPalette.ColorRole.AlternateBase, surface)
    palette.setColor(QPalette.ColorRole.Button, raised)
    palette.setColor(QPalette.ColorRole.ButtonText, text)
    palette.setColor(QPalette.ColorRole.Text, text)
    palette.setColor(QPalette.ColorRole.ToolTipBase, raised)
    palette.setColor(QPalette.ColorRole.ToolTipText, text)
    palette.setColor(QPalette.ColorRole.Highlight, accent)
    palette.setColor(QPalette.ColorRole.HighlightedText, QColor(COLORS["text_on_accent"]))
    palette.setColor(QPalette.ColorRole.PlaceholderText, muted)
    palette.setColor(QPalette.ColorRole.Link, accent)
    palette.setColor(QPalette.ColorGroup.Disabled, QPalette.ColorRole.Text, muted)
    palette.setColor(QPalette.ColorGroup.Disabled, QPalette.ColorRole.ButtonText, muted)
    palette.setColor(QPalette.ColorGroup.Disabled, QPalette.ColorRole.WindowText, muted)
    return palette


def apply_theme(app: QApplication, theme: str | None = None) -> str:
    """把字体/调色板/QSS 装到 ``app``，返回最终 QSS（便于日志与测试断言）。

    ``theme`` 为 ``"dark"``/``"light"`` 时会先切换令牌并清掉 QSS 缓存，
    因此可在运行期直接重复调用实现主题热切换。
    """
    if theme is not None:
        from ui.theme import tokens

        chosen = tokens.set_theme(theme)
        reload_stylesheet()
        log.info("theme switched name=%s", chosen)
    app.setFont(build_font())
    app.setPalette(build_palette())
    qss = stylesheet()
    app.setStyleSheet(qss)
    log.debug("theme applied qss_bytes=%d", len(qss))
    return qss


def set_invalid(widget: QWidget, invalid: bool) -> None:
    """切换 ``invalid`` 动态属性（QSS 里对应红框/红字），并立即重绘。"""
    if widget.property("invalid") == invalid:
        return
    widget.setProperty("invalid", invalid)
    repolish(widget)


def repolish(widget: QWidget) -> None:
    """动态属性变更后让 QSS 重新生效。"""
    style = widget.style()
    style.unpolish(widget)
    style.polish(widget)
    widget.update()
