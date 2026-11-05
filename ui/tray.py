"""系统托盘（设计文档 6.10.7）。

- 图标 = 品牌标 + 右下角状态点（灰未连接 / 绿空闲 / 红接管中 / 黄故障），
  不依赖外部资源；0.3.16 之前只有一个状态色圆点，通知区里认不出是哪个程序。
- 左键（Trigger/DoubleClick）显隐主窗；右键菜单：手动接管·交还、重连、
  打开配置、复制诊断、退出。
- 托盘不可用时（无桌面会话、RDP、部分 offscreen 平台）不创建
  ``QSystemTrayIcon``，但菜单与 ``QAction`` 仍可用，主窗把它挂到顶栏 ☰
  上，即"降级为菜单动作"。
"""

from __future__ import annotations

import logging

from PySide6.QtCore import QObject, QRectF, Qt, Signal
from PySide6.QtGui import QAction, QColor, QIcon, QPainter, QPen, QPixmap
from PySide6.QtWidgets import QMenu, QSystemTrayIcon, QWidget

import constants as C
from ui.icons import brand_pixmap
from ui.theme import tokens

log = logging.getLogger("ui.tray")

_TRAY_ICON_SIZE = 32


def probe_registration() -> tuple[bool, int]:
    """直接问 Windows 能不能挂上通知区图标。

    ``QSystemTrayIcon.isVisible()`` 只说明 Qt 自己 ``show()`` 过了，图标是否
    真的被 ``Shell_NotifyIcon`` 接受它并不知道；0.3.16/0.3.17 的日志里
    ``available=True visible=True`` 却看不到图标就是这个原因。这里挂一个
    message-only 的探针图标、立刻摘掉，用返回值当可信判据。
    """
    try:
        from win.trayprobe import probe_icon
    except Exception:  # pragma: no cover - 非 Windows 或导入失败
        return (True, 0)
    try:
        return probe_icon()
    except Exception as exc:  # pragma: no cover - 探针自身出错不拦主流程
        log.debug("tray probe failed: %r", exc)
        return (True, 0)


def registered_paths() -> list[str]:
    """Windows 为托盘图标记下的 ``ExecutablePath`` 列表（诊断用）。"""
    try:
        from win.trayprobe import registered_paths as paths
    except Exception:  # pragma: no cover - 非 Windows
        return []
    try:
        return paths()
    except Exception:  # pragma: no cover
        return []


def status_icon(state: str, size: int = _TRAY_ICON_SIZE) -> QIcon:
    """托盘图标：品牌标 + 右下角状态点（0.3.16）。

    早期版本只画一个状态色圆点；通知区缩到 16px 后既看不出是哪个程序，
    也容易被当成系统图标，用户会以为「程序没在托盘里」。现在以品牌标为
    底，右下角叠一颗状态色小圆点（未连接/空闲/接管/故障）。
    """
    icon = QIcon()
    for px in (16, 20, 24, 32, 48):
        icon.addPixmap(_tray_pixmap(state, px))
    return icon


def _tray_pixmap(state: str, size: int) -> QPixmap:
    """品牌标 + 右下角状态点，按目标像素尺寸 1:1 绘制。

    通知区在 100% 缩放下只有 16px：这里不放状态点，整格都给品牌标，
    否则两样都糊成一团、又回到“认不出是哪个程序”。24px 起才叠状态点。
    每个尺寸单独出图（16/20/24/32/48），交给 Windows 直接选，避免缩放模糊。
    """
    canvas = QPixmap(size, size)
    canvas.fill(Qt.GlobalColor.transparent)
    painter = QPainter(canvas)
    try:
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)

        mark_size = size if size <= 20 else round(size * 0.84)
        mark = brand_pixmap(mark_size)
        target = QRectF(0.0, 0.0, float(mark_size), float(mark_size))
        painter.drawPixmap(target, mark, QRectF(mark.rect()))

        if size > 20:
            dot = max(7.0, size * 0.36)
            x = size - dot
            y = size - dot
            painter.setPen(QPen(QColor(16, 20, 28), max(1.0, size * 0.08)))
            painter.setBrush(QColor(tokens.state_color(state)))
            painter.drawEllipse(QRectF(x, y, dot, dot))
    finally:
        painter.end()
    return canvas


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
        self.last_error = 0
        self._registered: bool | None = None
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
            self._registered, self.last_error = probe_registration()
            paths = registered_paths()
            own = _own_exe_name()
            registered_self = any(p.lower().endswith(own) for p in paths) if own else False
            log.info(
                "tray shown: qt_available=%s qt_visible=%s native_ok=%s err=%d "
                "entries=%d self_entry=%s",
                QSystemTrayIcon.isSystemTrayAvailable(),
                self.tray.isVisible(),
                self._registered,
                self.last_error,
                len(paths),
                registered_self,
            )

    def hide(self) -> None:
        if self.tray is not None:
            self.tray.hide()

    @property
    def visible(self) -> bool:
        """图标此刻是否真的挂在通知区（Windows 上 show() 可能被吞）。"""
        return bool(self.tray is not None and self.tray.isVisible())

    @property
    def registered(self) -> bool | None:
        """``Shell_NotifyIcon`` 是否接受了我们的图标；None 表示还没探测过。"""
        return self._registered

    def ensure_visible(self) -> bool:
        """确认图标在通知区；不在就再 show() 一次。

        0.3.16 修「关闭到托盘后在托盘里找不到」：Windows 11 上
        ``QSystemTrayIcon.show()`` 偶尔静默失败（尤其图标是在窗口显示
        之前创建的），这里在要隐藏主窗时兜底再挂一次，并把结果写进
        日志，方便下次从 ``padlend.log`` 判断是「没注册」还是「被系统
        收进了 ^ 溢出区」。
        """
        if self.tray is None:
            return False
        if not self.tray.isVisible():
            log.info("tray icon not visible; re-showing before hiding the window")
            self.tray.show()
        ok, err = probe_registration()
        self._registered, self.last_error = ok, err
        if ok:
            log.info(
                "tray ensure_visible -> True (native_ok, qt_visible=%s)",
                self.tray.isVisible(),
            )
            return True
        log.warning("tray registration rejected by the shell (err=%d)", err)
        return False

    def dispose(self) -> None:
        """退出前清理，避免托盘幽灵图标残留。"""
        if self.tray is not None:
            self.tray.setContextMenu(None)
            self.tray.hide()
            self.tray = None

    def explain_location(self) -> None:
        """一次性告诉用户图标去了哪儿（不看通知开关）。

        Windows 11 默认把**新**托盘图标收进任务栏右下角的「^」溢出区，
        用户第一反应就是“程序没在托盘里”。这条提示只在首次启动弹一次。
        """
        if self.tray is None:
            return
        try:
            self.tray.showMessage(
                C.APP_NAME,
                "我在系统托盘里运行：图标在任务栏右下角的「^」里，"
                "拖到任务栏上可以常驻；右键它可退出。",
                QSystemTrayIcon.MessageIcon.Information,
                6000,
            )
        except Exception as exc:  # pragma: no cover - 平台差异
            log.debug("tray hint failed: %r", exc)

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


def _own_exe_name() -> str:
    """当前进程的可执行文件名（打包版 = PadLend.exe，源码版 = python.exe）。"""
    import os
    import sys

    try:
        return os.path.basename(sys.executable).lower()
    except Exception:  # pragma: no cover - 拿不到就当未知
        return ""
