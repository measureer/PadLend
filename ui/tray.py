"""系统托盘（设计文档 6.10.7）。

- 图标用 ``QPainter`` 现画状态圆点，不依赖外部资源：灰未连接 / 绿空闲 /
  红接管中 / 黄故障。
- 左键（Trigger/DoubleClick）显隐主窗；右键菜单：手动接管·交还、重连、
  打开配置、复制诊断、退出。
- 托盘不可用时（无桌面会话、RDP、部分 offscreen 平台）不创建
  ``QSystemTrayIcon``，但菜单与 ``QAction`` 仍可用，主窗把它挂到顶栏 ☰
  上，即"降级为菜单动作"。
"""

from __future__ import annotations

import logging

from PySide6.QtCore import QObject, QRectF, Qt, Signal
from PySide6.QtGui import QAction, QColor, QIcon, QPainter, QPixmap
from PySide6.QtWidgets import QMenu, QSystemTrayIcon, QWidget

import constants as C
from ui.theme import tokens

log = logging.getLogger("ui.tray")

_TRAY_ICON_SIZE = 32


def status_icon(state: str, size: int = _TRAY_ICON_SIZE) -> QIcon:
    """按状态色画一个圆点图标（外圈半透明 + 内圈实心）。"""
    color = QColor(tokens.state_color(state))
    pixmap = QPixmap(size, size)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    try:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setPen(Qt.PenStyle.NoPen)

        halo = QColor(color)
        halo.setAlpha(90)
        painter.setBrush(halo)
        painter.drawEllipse(QRectF(0.0, 0.0, float(size), float(size)))

        painter.setBrush(color)
        inner = size * 0.56
        offset = (size - inner) / 2.0
        painter.drawEllipse(QRectF(offset, offset, inner, inner))
    finally:
        painter.end()
    return QIcon(pixmap)


class TrayController(QObject):
    """托盘图标 + 右键菜单；所有动作以信号形式暴露给主窗。"""

    toggleManualRequested = Signal()
    reconnectRequested = Signal()
    openConfigRequested = Signal()
    copyDiagnosticsRequested = Signal()
    quitRequested = Signal()
    visibilityToggled = Signal()

    def __init__(
        self,
        parent: QWidget | None = None,
        *,
        notifications: bool = True,
    ) -> None:
        super().__init__(parent)
        self._notifications = notifications
        self._state = ""
        self._device = ""
        self._connected = False
        self.menu = QMenu(parent)
        self.menu.setObjectName("trayMenu")
        self.toggle_action = self._add_action("手动接管", self.toggleManualRequested)
        self.reconnect_action = self._add_action("重连", self.reconnectRequested)
        self.menu.addSeparator()
        self.open_config_action = self._add_action("打开配置…", self.openConfigRequested)
        self.copy_diagnostics_action = self._add_action(
            "复制诊断信息", self.copyDiagnosticsRequested
        )
        self.menu.addSeparator()
        self.quit_action = self._add_action("退出 PadLend", self.quitRequested)

        self.tray: QSystemTrayIcon | None = None
        if QSystemTrayIcon.isSystemTrayAvailable():
            tray = QSystemTrayIcon(status_icon(""), self)
            tray.setToolTip(f"{C.APP_NAME} · {tokens.state_label('')}")
            tray.setContextMenu(self.menu)
            tray.activated.connect(self._on_activated)
            self.tray = tray
        else:
            log.info("system tray unavailable; menu actions only")
        self.toggle_action.setEnabled(False)
        self.reconnect_action.setEnabled(True)

    # ------------------------------------------------------------------ 状态
    @property
    def available(self) -> bool:
        return self.tray is not None

    @property
    def state(self) -> str:
        return self._state

    @property
    def notifications_enabled(self) -> bool:
        return self._notifications

    def set_notifications(self, enabled: bool) -> None:
        self._notifications = bool(enabled)

    def set_connected(self, connected: bool) -> None:
        self._connected = bool(connected)
        self.toggle_action.setEnabled(bool(connected))
        self.reconnect_action.setEnabled(True)

    def set_device(self, device: str) -> None:
        self._device = device
        self._update_tooltip()

    def set_state(self, state: str) -> None:
        self._state = state
        captured = state == "captured"
        self.toggle_action.setText("交还控制权" if captured else "手动接管")
        self._update_tooltip()
        if self.tray is not None:
            self.tray.setIcon(status_icon(state))

    def show(self) -> None:
        if self.tray is not None:
            self.tray.show()

    def hide(self) -> None:
        if self.tray is not None:
            self.tray.hide()

    def dispose(self) -> None:
        """退出前清理，避免托盘幽灵图标残留。"""
        if self.tray is not None:
            self.tray.setContextMenu(None)
            self.tray.hide()
            self.tray = None

    def notify(self, title: str, message: str) -> None:
        """气泡通知（仅托盘可用且用户未关闭时）。"""
        if not self._notifications or self.tray is None:
            return
        self.tray.showMessage(
            title,
            message,
            QSystemTrayIcon.MessageIcon.Information,
            4000,
        )

    # ------------------------------------------------------------------ 内部
    def _add_action(self, text: str, signal: Signal) -> QAction:
        action = QAction(text, self.menu)
        action.triggered.connect(lambda *_: signal.emit())
        self.menu.addAction(action)
        return action

    def _update_tooltip(self) -> None:
        if self.tray is None:
            return
        parts = [C.APP_NAME, tokens.state_label(self._state)]
        if self._device:
            parts.append(self._device)
        self.tray.setToolTip(" · ".join(parts))

    def _on_activated(self, reason: QSystemTrayIcon.ActivationReason) -> None:
        if reason in (
            QSystemTrayIcon.ActivationReason.Trigger,
            QSystemTrayIcon.ActivationReason.DoubleClick,
        ):
            self.visibilityToggled.emit()
