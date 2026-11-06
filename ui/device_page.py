"""设备连接页：扫描 adb、选择设备、连接/断开、展示真实设备事实。

只通过注入的 ``adb`` 对象调用公开方法（``devices()``），不 import win32，
也不在 GUI 线程里执行 adb 命令；扫描与连接动作都在后台线程完成。
"""

from __future__ import annotations

import logging
from typing import Any

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, QThread, Signal
from PySide6.QtGui import QColor, QPainter, QPen
from PySide6.QtWidgets import (
    QFrame,
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QSizePolicy,
    QVBoxLayout,
    QWidget,
)

from ui.icons import make_icon
from ui.style import repolish
from ui.theme import tokens

log = logging.getLogger("ui.device_page")

# 启动探测的结论 → 记忆行上的短标签（0.3.13）
_LOCATED_LABELS = {
    "online": "在线",
    "moved": "已换地址",
    "no_port": "端口未知",
    "offline": "不在线",
}


class _DeviceRow(QWidget):
    """设备列表的一行：型号 + 「序列号  传输 · 状态」。

    不用 ``QListWidgetItem`` 的多行文本，因为 ``QListWidget::item`` 的样式规则
    会让默认代理只渲染一行（型号后面的序列号被省略号吃掉）。
    """

    def __init__(self, title: str, subtitle: str, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("deviceRow")
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents, True)
        layout = QVBoxLayout(self)
        # 内边距由 deviceList::item 的 QSS padding 提供，这里再加边距会把
        # 第二行挤出 62px 的行高（表现为文字下半截被切掉）。
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(2)
        name = QLabel(title)
        name.setObjectName("deviceRowTitle")
        meta = QLabel(subtitle)
        meta.setObjectName("deviceRowMeta")
        layout.addWidget(name)
        layout.addWidget(meta)
        layout.addStretch(1)



class _EmptyArt(QWidget):
    """空状态插画：一台平板、一条线缆和一点微光。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setMinimumHeight(130)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)

    def paintEvent(self, event: Any) -> None:  # noqa: N802 - Qt 命名
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        w = float(self.width())
        h = float(self.height())
        cx = w / 2.0
        cy = h / 2.0 - 2.0

        glow = QColor(tokens.COLORS["accent"])
        glow.setAlpha(28)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(glow)
        painter.drawEllipse(QPointF(cx, cy), 54.0, 42.0)

        tablet = QRectF(cx - 40.0, cy - 30.0, 80.0, 58.0)
        painter.setBrush(QColor(tokens.COLORS["bg_raised"]))
        painter.setPen(QPen(QColor(tokens.COLORS["border_strong"]), 1.4))
        painter.drawRoundedRect(tablet, 8.0, 8.0)
        painter.setPen(QPen(QColor(tokens.COLORS["accent"]), 1.4))
        inner = tablet.adjusted(8.0, 8.0, -8.0, -8.0)
        painter.drawRoundedRect(inner, 3.0, 3.0)

        painter.setPen(QPen(QColor(tokens.COLORS["text_secondary"]), 1.4))
        painter.drawLine(cx, cy + 30.0, cx, cy + 44.0)
        painter.setPen(QPen(QColor(tokens.COLORS["accent"]), 1.6))
        painter.drawLine(cx, cy + 44.0, cx - 18.0, cy + 52.0)
        painter.drawLine(cx, cy + 44.0, cx + 18.0, cy + 52.0)
        painter.end()


class _ScanThread(QThread):
    """一次性 adb 扫描；信号携带 ``list[DeviceEntry]`` 与错误文本。"""

    completed = Signal(object, str)

    def __init__(self, scan: Any, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._scan = scan

    def run(self) -> None:  # noqa: D401 - QThread 入口
        try:
            entries = list(self._scan() or [])
        except Exception as exc:  # noqa: BLE001 - 设备扫描失败要显示在页面上
            log.warning("device scan failed: %r", exc)
            self.completed.emit([], f"扫描失败：{exc}")
            return
        self.completed.emit(entries, "")


class DevicePage(QWidget):
    """设备管理页。"""

    connectRequested = Signal(str)
    disconnectRequested = Signal()
    addRequested = Signal()
    removeRequested = Signal(str)
    # 用户主动点「刷新设备」时额外发一次：主窗用它按已保存的 MAC 在局域网里
    # 找地址变了的平板（程序内部的 refresh() 调用不触发，避免启动时到处探测）。
    refreshRequested = Signal()

    def __init__(
        self,
        adb: Any | None = None,
        router: Any | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("devicePage")
        self._adb = adb
        self._router = router
        self._entries: list[Any] = []
        # 记住的设备（上次连过的，离线也列出来）
        self._known: list[str] = []
        # 启动探测的结论：serial → (状态文本, 说明)（0.3.13）
        self._located: dict[str, str] = {}
        self._located_detail: dict[str, str] = {}
        self._error = ""
        self._scan_thread: _ScanThread | None = None
        self._facts: Any | None = None
        self._busy = False
        self._state = ""
        self._values: dict[str, QLabel] = {}
        self._build()
        self.set_facts(None)

    # ------------------------------------------------------------------ 构建
    def _build(self) -> None:
        layout = QVBoxLayout(self)
        layout.setContentsMargins(28, 24, 28, 22)
        layout.setSpacing(18)

        title = QLabel("设备")
        title.setObjectName("pageTitle")
        subtitle = QLabel("连接平板后才会读取并显示它的真实分辨率、朝向与输入参数。")
        subtitle.setObjectName("pageSubtitle")
        subtitle.setWordWrap(True)
        heading = QVBoxLayout()
        heading.setSpacing(3)
        heading.addWidget(title)
        heading.addWidget(subtitle)

        self.refresh_button = QPushButton("刷新设备")
        self.refresh_button.setObjectName("ghostButton")
        self.refresh_button.setIcon(make_icon("refresh", tokens.COLORS["text_primary"], 16))
        self.refresh_button.setIconSize(QSize(16, 16))
        self.refresh_button.setToolTip("重新扫描 adb，并按已保存的 MAC 在局域网里查找平板")
        self.refresh_button.clicked.connect(self._on_refresh_clicked)

        self.add_button = QPushButton("添加设备")
        self.add_button.setObjectName("ghostButton")
        self.add_button.setIcon(make_icon("wifi", tokens.COLORS["text_primary"], 16))
        self.add_button.setIconSize(QSize(16, 16))
        self.add_button.setToolTip("用无线调试添加平板：输入平板上的 IP 地址与端口")
        self.add_button.clicked.connect(self._request_add)

        self.scan_hint = QLabel("")
        self.scan_hint.setObjectName("cardHint")

        top = QHBoxLayout()
        top.addLayout(heading, 1)
        top.addWidget(self.scan_hint, 0, Qt.AlignmentFlag.AlignVCenter)
        top.addWidget(self.add_button, 0, Qt.AlignmentFlag.AlignVCenter)
        top.addWidget(self.refresh_button, 0, Qt.AlignmentFlag.AlignVCenter)
        layout.addLayout(top)

        # 长错误单独占一行：塞在标题右边会把「添加设备」挤出可视区。
        self.error_strip = QLabel("")
        self.error_strip.setObjectName("errorStrip")
        self.error_strip.setWordWrap(True)
        # Ignored：让标签宽度跟随页面而不是被长文本撑开。
        self.error_strip.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Preferred)
        self.error_strip.setVisible(False)
        layout.addWidget(self.error_strip)

        content = QHBoxLayout()
        content.setSpacing(18)
        content.addWidget(self._build_list_card(), 5)
        content.addWidget(self._build_detail_card(), 6)
        layout.addLayout(content, 1)

    def _build_list_card(self) -> QFrame:
        card = QFrame()
        card.setObjectName("sectionCard")
        box = QVBoxLayout(card)
        box.setContentsMargins(18, 16, 18, 16)
        box.setSpacing(10)

        title = QLabel("设备列表")
        title.setObjectName("cardTitle")
        hint = QLabel(
            "这里只列已经通过 ADB 授权的设备（USB 或无线）。"
            "上次连过、但这次没扫到的设备也会留一行，可以点「从列表移除」。"
        )
        hint.setObjectName("cardHint")
        hint.setWordWrap(True)
        box.addWidget(title)
        box.addWidget(hint)

        self.device_list = QListWidget()
        self.device_list.setObjectName("deviceList")
        self.device_list.setSpacing(2)
        # 每行是「型号\n序列号  传输 · 状态」两行；不开启换行时样式代理会把整段
        # 压成单行并加省略号，只剩型号，看不出是哪台设备。
        self.device_list.setWordWrap(True)
        self.device_list.itemSelectionChanged.connect(self._on_selection)
        self.device_list.itemDoubleClicked.connect(lambda _item: self._connect_selected())
        box.addWidget(self.device_list, 1)

        self.empty_box = QWidget()
        empty_layout = QVBoxLayout(self.empty_box)
        empty_layout.setContentsMargins(0, 0, 0, 0)
        empty_layout.setSpacing(6)
        empty_layout.addWidget(_EmptyArt())
        self.empty_title = QLabel("未检测到设备")
        self.empty_title.setObjectName("emptyTitle")
        self.empty_title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.empty_hint = QLabel(
            "USB 连接：插上数据线并在平板上允许「USB 调试」。\n"
            "无线连接：点右上角「添加设备」，填平板「无线调试」里的 IP 与端口。"
        )
        self.empty_hint.setObjectName("emptyHint")
        self.empty_hint.setWordWrap(True)
        self.empty_hint.setAlignment(Qt.AlignmentFlag.AlignCenter)
        empty_layout.addWidget(self.empty_title)
        empty_layout.addWidget(self.empty_hint)
        box.addWidget(self.empty_box, 1)

        self.connect_button = QPushButton("连接此设备")
        self.connect_button.setObjectName("primaryButton")
        self.connect_button.setIcon(make_icon("link", tokens.COLORS["text_on_accent"], 16))
        self.connect_button.setIconSize(QSize(16, 16))
        self.connect_button.setEnabled(False)
        self.connect_button.clicked.connect(self._connect_selected)

        self.remove_button = QPushButton("从列表移除")
        self.remove_button.setObjectName("ghostButton")
        self.remove_button.setIcon(make_icon("close", tokens.COLORS["text_secondary"], 16))
        self.remove_button.setIconSize(QSize(16, 16))
        self.remove_button.setToolTip("把选中的「上次连接的设备」从记忆列表里删掉；正在线的设备不受影响。")
        self.remove_button.setEnabled(False)
        self.remove_button.clicked.connect(self._remove_selected)

        footer = QHBoxLayout()
        footer.setSpacing(8)
        footer.addWidget(self.connect_button, 1)
        footer.addWidget(self.remove_button)
        box.addLayout(footer)
        return card

    def _build_detail_card(self) -> QFrame:
        card = QFrame()
        card.setObjectName("sectionCard")
        box = QVBoxLayout(card)
        box.setContentsMargins(20, 16, 20, 16)
        box.setSpacing(12)

        head = QHBoxLayout()
        title = QLabel("设备详情")
        title.setObjectName("cardTitle")
        head.addWidget(title)
        head.addStretch(1)
        self.state_pill = QFrame()
        self.state_pill.setObjectName("statusPill")
        pill_layout = QHBoxLayout(self.state_pill)
        pill_layout.setContentsMargins(9, 3, 10, 3)
        pill_layout.setSpacing(6)
        self.pill_dot = QLabel()
        self.pill_dot.setFixedSize(6, 6)
        self.pill_text = QLabel("未连接")
        self.pill_text.setObjectName("statusPillText")
        pill_layout.addWidget(self.pill_dot)
        pill_layout.addWidget(self.pill_text)
        head.addWidget(self.state_pill)
        box.addLayout(head)

        self.detail_headline = QLabel("尚未连接平板")
        self.detail_headline.setObjectName("deviceHeading")
        self.detail_meta = QLabel("连接后显示型号、序列号与真实屏幕参数")
        self.detail_meta.setObjectName("deviceMeta")
        self.detail_meta.setWordWrap(True)
        box.addWidget(self.detail_headline)
        box.addWidget(self.detail_meta)

        self._divider = QFrame()
        self._divider.setFrameShape(QFrame.Shape.HLine)
        self._divider.setStyleSheet(f"color: {tokens.COLORS['border']};")
        box.addWidget(self._divider)

        for key, label in (
            ("model", "型号"),
            ("serial", "序列号"),
            ("resolution", "分辨率"),
            ("orientation", "方向"),
            ("xscale", "输入比例"),
            ("pointer_speed", "指针速度"),
        ):
            row = QHBoxLayout()
            row.setSpacing(12)
            name = QLabel(label)
            name.setObjectName("detailLabel")
            value = QLabel("—")
            value.setObjectName("detailValue")
            value.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            row.addWidget(name)
            row.addStretch(1)
            row.addWidget(value)
            self._values[key] = value
            box.addLayout(row)

        box.addSpacing(10)
        tips = QLabel(
            "连接步骤\n"
            "1. 在平板上开启“开发者选项 → USB 调试”；\n"
            "2. 首次连接时在平板上允许此电脑的调试授权；\n"
            "3. 连接后自动读取真实分辨率与输入参数，不依赖型号。"
        )
        tips.setObjectName("deviceMeta")
        tips.setWordWrap(True)
        tips.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        box.addWidget(tips)

        box.addStretch(1)
        self.disconnect_button = QPushButton("断开连接")
        self.disconnect_button.setObjectName("dangerButton")
        self.disconnect_button.setIcon(make_icon("unlink", tokens.COLORS["err"], 16))
        self.remove_button.setIcon(
            make_icon("close", tokens.COLORS["text_secondary"], 16)
        )
        self.disconnect_button.setIconSize(QSize(16, 16))
        self.disconnect_button.setEnabled(False)
        self.disconnect_button.clicked.connect(self._disconnect)
        box.addWidget(self.disconnect_button)
        return card

    # ------------------------------------------------------------------ 扫描
    def set_adb(self, adb: Any | None) -> None:
        self._adb = adb

    def _on_refresh_clicked(self) -> None:
        """按钮入口：先做常规 adb 扫描，再请主窗做一次局域网查找。"""
        self.refresh()
        self.refreshRequested.emit()

    def refresh(self) -> None:
        if self._adb is None:
            self._set_scan_hint("未找到 adb（打包版应内置）")
            self._show_entries([], "")
            return
        if self._scan_thread is not None:
            return
        scan = getattr(self._adb, "devices", None)
        if not callable(scan):
            self._set_scan_hint("adb 接口不可用")
            return
        self._clear_error()
        self._set_scan_hint("正在扫描…")
        self.refresh_button.setEnabled(False)
        thread = _ScanThread(scan, self)
        thread.completed.connect(self._on_scan_done)
        thread.finished.connect(self._on_thread_finished)
        self._scan_thread = thread
        thread.start()

    def _on_thread_finished(self) -> None:
        thread = self._scan_thread
        self._scan_thread = None
        if thread is not None:
            thread.deleteLater()
        self.refresh_button.setEnabled(True)

    def _on_scan_done(self, entries: object, error: str) -> None:
        self._show_entries(list(entries or []), error)

    def set_known(self, serials: list[str]) -> None:
        """记住的设备（最近连过的）；离线时也在列表里露一行（0.3.11）。"""
        known = [str(s) for s in serials if str(s)]
        if known == self._known:
            return
        self._known = known
        self._render()

    def set_located(self, serial: str, status: str, detail: str = "") -> None:
        """启动探测结论：写在对应的记忆行上，扫不到时也有个交代（0.3.13）。"""
        serial = str(serial or "")
        if not serial:
            return
        label = _LOCATED_LABELS.get(status, "")
        if label:
            self._located[serial] = label
        else:
            self._located.pop(serial, None)
        if detail:
            self._located_detail[serial] = str(detail)
        elif not label:
            self._located_detail.pop(serial, None)
        self._render()

    def _remembered_selected(self) -> bool:
        item = self.device_list.currentItem()
        return bool(item is not None and item.data(Qt.ItemDataRole.UserRole + 1))

    def _show_entries(self, entries: list[Any], error: str) -> None:
        self._entries = entries
        self._error = error
        if not error:
            self._clear_error()
        self._render()

    def _render(self) -> None:
        entries = self._entries
        error = self._error
        current = self.device_list.currentItem()
        selected_serial = ""
        if current is not None:
            selected_serial = str(current.data(Qt.ItemDataRole.UserRole) or "")
        self.device_list.clear()

        for entry in entries:
            model = str(getattr(entry, "model", "") or "Android 设备").replace("_", " ")
            serial = str(getattr(entry, "serial", "") or "")
            transport = "USB" if getattr(entry, "is_usb", False) else "无线"
            state = str(getattr(entry, "state", "") or "")
            item = QListWidgetItem()
            item.setData(Qt.ItemDataRole.UserRole, serial)
            item.setSizeHint(QSize(0, 62))
            item.setToolTip(serial)
            self.device_list.addItem(item)
            # 两行文字不能放在 item 文本里：`QListWidget::item` 的样式规则会让
            # 代理只画一行并省略，型号后面的序列号全被截掉。用一个两层标签的
            # 行部件承载文字，item 只负责背景/选中态。
            subtitle = f"{serial}    {transport} · {state}"
            self.device_list.setItemWidget(item, _DeviceRow(model, subtitle))
            if serial == selected_serial:
                self.device_list.setCurrentItem(item)

        # 记住的设备：当前扫不到的也列出来，省得每次重开软件都要重新输地址。
        present = {str(getattr(entry, "serial", "") or "") for entry in entries}
        remembered = 0
        for serial in self._known:
            if not serial or serial in present:
                continue
            transport = "无线" if ":" in serial else "USB"
            status = self._located.get(serial, "")
            detail = self._located_detail.get(serial, "")
            item = QListWidgetItem()
            item.setData(Qt.ItemDataRole.UserRole, serial)
            item.setData(Qt.ItemDataRole.UserRole + 1, True)  # 标记：记忆行
            item.setSizeHint(QSize(0, 62))
            item.setToolTip(
                f"{serial}（{detail}）"
                if detail
                else f"{serial}（上次连接，当前未接入）"
            )
            self.device_list.addItem(item)
            subtitle = (
                f"上次连接的设备 · {status}"
                if status
                else f"上次连接的设备 · {transport}"
            )
            self.device_list.setItemWidget(item, _DeviceRow(serial, subtitle))
            if serial == selected_serial:
                self.device_list.setCurrentItem(item)
            remembered += 1

        has_entries = bool(entries) or remembered > 0
        self.device_list.setVisible(has_entries)
        self.empty_box.setVisible(not has_entries)
        if error:
            self.empty_title.setText("扫描失败")
            self.empty_hint.setText(error)
        else:
            self.empty_title.setText("未检测到设备")
            self.empty_hint.setText(
                "USB 连接：插上数据线并在平板上允许「USB 调试」。\n"
                "无线连接：点右上角「添加设备」，填平板「无线调试」里的 IP 与端口。"
            )
        if has_entries and self.device_list.currentItem() is None:
            self.device_list.setCurrentRow(0)
        self._on_selection()
        if entries:
            hint = f"{len(entries)} 台设备"
        elif remembered:
            hint = f"{remembered} 台已记住"
        else:
            hint = ""
        self._set_scan_hint(hint)

    def _set_scan_hint(self, text: str) -> None:
        self.scan_hint.setText(text)

    # ------------------------------------------------------------------ 交互
    def _on_selection(self) -> None:
        item = self.device_list.currentItem()
        self.connect_button.setEnabled(item is not None and not self._busy)
        remembered = bool(item is not None and item.data(Qt.ItemDataRole.UserRole + 1))
        self.remove_button.setEnabled(remembered and not self._busy)

    def _remove_selected(self) -> None:
        item = self.device_list.currentItem()
        if item is None or self._busy:
            return
        remembered = bool(item.data(Qt.ItemDataRole.UserRole + 1))
        serial = str(item.data(Qt.ItemDataRole.UserRole) or "")
        if remembered and serial:
            self.removeRequested.emit(serial)

    def _connect_selected(self) -> None:
        item = self.device_list.currentItem()
        if item is None or self._busy:
            return
        serial = str(item.data(Qt.ItemDataRole.UserRole) or "")
        if not serial:
            return
        self.set_busy(True)
        self.connectRequested.emit(serial)

    def _disconnect(self) -> None:
        if self._busy:
            return
        self.set_busy(True)
        self.disconnectRequested.emit()

    def _request_add(self) -> None:
        """请求主窗打开「添加无线设备」对话框（本页不直接弹窗）。"""
        if self._busy:
            return
        self.addRequested.emit()

    def set_busy(self, busy: bool) -> None:
        self._busy = busy
        self.refresh_button.setEnabled(not busy)
        self.add_button.setEnabled(not busy)
        self.connect_button.setEnabled(not busy and self.device_list.currentItem() is not None)
        self.disconnect_button.setEnabled(not busy and self._facts is not None)
        self.remove_button.setEnabled(not busy and self._remembered_selected())
        if busy:
            self._set_scan_hint("正在连接…")

    def show_error(self, text: str) -> None:
        """在列表上方显示可恢复的错误，不弹模态框、不挤压标题行。"""
        self.error_strip.setText(text or "操作失败")
        self.error_strip.setVisible(True)
        self._set_scan_hint("")

    def _clear_error(self) -> None:
        self.error_strip.clear()
        self.error_strip.setVisible(False)

    # ------------------------------------------------------------------ 状态
    def set_state(self, state: str) -> None:
        self._state = state
        label, color = {
            "disconnected": ("未连接", tokens.COLORS["disconnected"]),
            "connecting": ("连接中", tokens.COLORS["accent"]),
            "idle": ("已连接", tokens.COLORS["ok"]),
            "arming": ("外推中", tokens.COLORS["warn"]),
            "captured": ("接管中", tokens.COLORS["capturing"]),
            "returning": ("回程中", tokens.COLORS["warn"]),
            "fault": ("连接异常", tokens.COLORS["err"]),
        }.get(state, ("未连接", tokens.COLORS["disconnected"]))
        self.pill_text.setText(label)
        self.pill_dot.setStyleSheet(f"background-color: {color}; border-radius: 3px;")
        self.state_pill.setProperty("state", state)
        repolish(self.state_pill)

    def refresh_theme(self) -> None:
        """主题切换后刷新自绘制颜色与按钮图标。"""
        self.refresh_button.setIcon(make_icon("refresh", tokens.COLORS["text_primary"], 16))
        self.add_button.setIcon(make_icon("wifi", tokens.COLORS["text_primary"], 16))
        self.connect_button.setIcon(make_icon("link", tokens.COLORS["text_on_accent"], 16))
        self.disconnect_button.setIcon(make_icon("unlink", tokens.COLORS["err"], 16))
        self._divider.setStyleSheet(f"color: {tokens.COLORS['border']};")
        self.set_state(self._state)
        self.update()

    def set_facts(self, facts: Any | None) -> None:
        self._facts = facts
        if facts is None:
            self.detail_headline.setText("尚未连接平板")
            self.detail_meta.setText("连接后显示型号、序列号与真实屏幕参数")
            for value in self._values.values():
                value.setText("—")
            self.disconnect_button.setEnabled(False)
            self.set_state("")
            return

        model = str(getattr(facts, "model", "") or "Android 设备").replace("_", " ")
        serial = str(getattr(facts, "serial", "") or "")
        width = int(getattr(facts, "width", 0) or 0)
        height = int(getattr(facts, "height", 0) or 0)
        orientation = int(getattr(facts, "orientation", 0) or 0)
        xscale = float(getattr(facts, "xscale", 1.0) or 1.0)
        pointer_speed = getattr(facts, "pointer_speed", None)

        self.detail_headline.setText(model)
        self.detail_meta.setText(serial or "序列号未知")
        self._values["model"].setText(model)
        self._values["serial"].setText(serial or "—")
        self._values["resolution"].setText(f"{width} × {height}" if width and height else "—")
        self._values["orientation"].setText("横屏" if orientation & 1 else "竖屏")
        self._values["xscale"].setText(f"{xscale:.3f}")
        self._values["pointer_speed"].setText("—" if pointer_speed is None else str(pointer_speed))
        self.disconnect_button.setEnabled(not self._busy)
        if self._state in ("", "fault"):
            self.set_state("idle")

    def set_connected(self, connected: bool) -> None:
        self.disconnect_button.setEnabled(connected and not self._busy)

    # ------------------------------------------------------------------ 清理
    def stop(self) -> None:
        thread = self._scan_thread
        if thread is None:
            return
        thread.requestInterruption()
        thread.wait(1500)
        self._scan_thread = None
