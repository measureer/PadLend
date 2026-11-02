"""事件日志视图（设计文档 6.10.1 底部左栏）。

- 行格式：``HH:MM:SS.mmm EVENT  说明``；事件名着色（tokens.log_event_color）。
- 最多保留 2000 行（``QPlainTextEdit.setMaximumBlockCount`` + 同步的 deque）。
- 过滤框只影响显示，不丢弃已收日志。
- 线程安全：只由 :class:`ui.bridge.RouterBridge` 的排队信号驱动，视图本身不加锁。
"""

from __future__ import annotations

import html
import logging
import re
import time
from collections import deque
from dataclasses import dataclass

from PySide6.QtCore import Qt
from PySide6.QtGui import QFont
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPlainTextEdit,
    QVBoxLayout,
    QWidget,
)

from ui.style import build_mono_font
from ui.theme.tokens import COLORS, log_event_color

log = logging.getLogger("ui.log_view")

MAX_LINES = 2000

_LINE_RE = re.compile(
    r"^(?P<ts>\d{2}:\d{2}:\d{2}\.\d{3})\s+(?P<event>[A-Za-z][\w.]*)\s+(?P<message>.*)$"
)


@dataclass(frozen=True, slots=True)
class LogEntry:
    """一条已解析的日志。``event`` 为空表示原始文本行。"""

    timestamp: str
    event: str
    message: str

    @property
    def text(self) -> str:
        if not self.event:
            return self.message if not self.timestamp else f"{self.timestamp} {self.message}"
        return f"{self.timestamp} {self.event} {self.message}".strip()


def format_stamp(when: float | None = None) -> str:
    """``HH:MM:SS.mmm``（本地时间）。"""
    moment = time.time() if when is None else when
    base = time.strftime("%H:%M:%S", time.localtime(moment))
    return f"{base}.{int(moment * 1000) % 1000:03d}"


def parse_line(line: str) -> LogEntry:
    """解析一行日志；不符合格式时整行作为说明文本。"""
    text = line.strip("\r\n")
    match = _LINE_RE.match(text)
    if match is None:
        return LogEntry(timestamp="", event="", message=text)
    return LogEntry(
        timestamp=match.group("ts"),
        event=match.group("event"),
        message=match.group("message"),
    )


class LogView(QWidget):
    """限长、可过滤、按事件着色的事件日志。"""

    def __init__(self, parent: QWidget | None = None, max_lines: int = MAX_LINES) -> None:
        super().__init__(parent)
        self.setObjectName("logPanel")
        self._max_lines = max(1, int(max_lines))
        self._entries: deque[LogEntry] = deque(maxlen=self._max_lines)
        self._needle = ""

        self.title_label = QLabel("事件日志", self)
        self.title_label.setObjectName("sectionTitle")

        self.filter_edit = QLineEdit(self)
        self.filter_edit.setPlaceholderText("过滤…")
        self.filter_edit.setClearButtonEnabled(True)
        self.filter_edit.setFixedWidth(180)
        self.filter_edit.setToolTip("按子串过滤事件名或说明（不影响已收日志）")

        header = QHBoxLayout()
        header.setContentsMargins(0, 0, 0, 0)
        header.setSpacing(8)
        header.addWidget(self.title_label)
        header.addStretch(1)
        header.addWidget(self.filter_edit)

        self.view = QPlainTextEdit(self)
        self.view.setObjectName("logView")
        self.view.setReadOnly(True)
        self.view.setUndoRedoEnabled(False)
        self.view.setMaximumBlockCount(self._max_lines)
        self.view.setLineWrapMode(QPlainTextEdit.LineWrapMode.NoWrap)
        self.view.setFont(build_mono_font())
        self.view.setPlaceholderText("等待事件…")
        self.view.setFocusPolicy(Qt.FocusPolicy.StrongFocus)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(8, 8, 8, 8)
        layout.setSpacing(6)
        layout.addLayout(header)
        layout.addWidget(self.view, 1)

        self.filter_edit.textChanged.connect(self.set_filter)

    # -- 只读属性 ----------------------------------------------------------
    @property
    def max_lines(self) -> int:
        return self._max_lines

    @property
    def entry_count(self) -> int:
        return len(self._entries)

    @property
    def filter_text(self) -> str:
        return self._needle

    def text(self) -> str:
        """全部已收日志（含被过滤掉的行），测试/诊断包用。"""
        return "\n".join(entry.text for entry in self._entries)

    def visible_text(self) -> str:
        return self.view.toPlainText()

    # -- 追加 --------------------------------------------------------------
    def append_line(self, line: str) -> None:
        """追加一行（可能自带时间戳/事件名）。"""
        text = str(line).rstrip("\n")
        if not text.strip():
            return
        self.append_entry(parse_line(text))

    def append_event(self, event: str, message: str, when: float | None = None) -> None:
        """按事件名追加（时间戳自动生成）。"""
        self.append_entry(LogEntry(format_stamp(when), str(event), str(message)))

    def append_entry(self, entry: LogEntry) -> None:
        self._entries.append(entry)
        if self._matches(entry):
            self._append_html(entry)

    # -- 清空/过滤 ---------------------------------------------------------
    def clear(self) -> None:
        self._entries.clear()
        self.view.clear()

    def set_filter(self, text: str) -> None:
        needle = str(text).strip().lower()
        if needle == self._needle:
            return
        self._needle = needle
        self._rebuild()

    # -- 内部 --------------------------------------------------------------
    def _matches(self, entry: LogEntry) -> bool:
        return not self._needle or self._needle in entry.text.lower()

    def _rebuild(self) -> None:
        self.view.clear()
        for entry in self._entries:
            if self._matches(entry):
                self._append_html(entry)

    def _append_html(self, entry: LogEntry) -> None:
        scroll = self.view.verticalScrollBar()
        stick = scroll.value() >= scroll.maximum() - 4
        self.view.appendHtml(self._render(entry))
        if stick:
            scroll.setValue(scroll.maximum())

    @staticmethod
    def _render(entry: LogEntry) -> str:
        if not entry.event:
            color = COLORS["text_secondary"]
            return f'<span style="color:{color};">{html.escape(entry.message)}</span>'
        ts = html.escape(entry.timestamp)
        event = html.escape(entry.event)
        message = html.escape(entry.message)
        color = log_event_color(entry.event)
        return (
            f'<span style="color:{COLORS["text_secondary"]};">{ts}</span> '
            f'<span style="color:{color}; font-weight:600;">{event}</span> '
            f'<span style="color:{COLORS["text_primary"]};">{message}</span>'
        )


def mono_font(point_size: int | None = None) -> QFont:
    """兼容旧调用的等宽字体入口。"""
    return build_mono_font(point_size)
