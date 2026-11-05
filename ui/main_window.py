"""主窗口外壳（设计文档 6.10.1 / 6.10.4 / 6.10.7）。

结构::

    ┌──────────────────────────── 顶栏 ────────────────────────────┐
    │ PadLend ● 状态  设备 · 朝向 · 延迟         [☰] [⚙] [☀]        │
    ├───────────────┬──────────────────────────────────────────────┤
    │               │  设置面板                                    │
    │   桌面画布     │  （位置/手感/键盘/快捷键/杂项）                │
    ├───────────────┴──────────────────────────────────────────────┤
    │ 事件日志                                   │ 指标             │
    └──────────────────────────────────────────────────────────────┘

画布来自 ``ui/canvas.py``（并行开发）。模块缺失或构造失败时退回占位
``QLabel``，主窗自身绝不 import win32，也不会自己实现画布。
"""

from __future__ import annotations

import ctypes
import logging
import os
import sys
import threading
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QAbstractNativeEventFilter, QSize, Qt, QTimer, Signal
from PySide6.QtGui import (
    QAction,
    QCloseEvent,
    QGuiApplication,
    QKeySequence,
    QShortcut,
    QShowEvent,
)
from PySide6.QtWidgets import (
    QApplication,
    QButtonGroup,
    QDialog,
    QFrame,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QPushButton,
    QSplitter,
    QStackedWidget,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

import constants as C
from tablet.locator import STATUS_MOVED, STATUS_ONLINE, Located, locate
from ui.add_device_dialog import AddDeviceDialog
from ui.bridge import RouterBridge, state_value
from ui.device_page import DevicePage
from ui.diagnostics import DiagnosticsDialog, static_summary
from ui.icons import brand_icon, brand_pixmap, make_icon
from ui.log_view import LogView
from ui.settings_panel import SettingsPanel
from ui.style import apply_theme, repolish
from ui.theme import tokens
from ui.tray import TrayController
from ui.wizard import SetupWizard

if TYPE_CHECKING:
    from config import Config

try:  # 并行开发阶段 canvas 可能还不存在
    from ui.canvas import DesktopCanvas
except ImportError:  # pragma: no cover - 由 tests/test_ui_smoke.py 覆盖占位分支
    DesktopCanvas = None  # type: ignore[assignment,misc]

log = logging.getLogger("ui.main_window")

_METRICS_WIDTH = 320


def _pick(data: dict[str, Any], keys: tuple[str, ...]) -> float | None:
    for key in keys:
        value = data.get(key)
        if value is None:
            continue
        try:
            return float(value)
        except (TypeError, ValueError):
            continue
    return None


def _fmt(value: float | None, pattern: str, default: str = "—") -> str:
    return default if value is None else format(value, pattern)


class MetricsPanel(QFrame):
    """底部右侧指标卡：上报/发送/背压、RTT/p95/丢包、CPU/内存。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("card")
        self.setMinimumWidth(_METRICS_WIDTH)
        self.title_label = QLabel("指标", self)
        self.title_label.setObjectName("sectionTitle")
        self.rate_label = QLabel(self)
        self.rtt_label = QLabel(self)
        self.cpu_label = QLabel(self)
        for label in (self.rate_label, self.rtt_label, self.cpu_label):
            label.setObjectName("metricsText")
            label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        self.clear()

        layout = QVBoxLayout(self)
        layout.setContentsMargins(12, 10, 12, 10)
        layout.setSpacing(4)
        layout.addWidget(self.title_label)
        layout.addWidget(self.rate_label)
        layout.addWidget(self.rtt_label)
        layout.addWidget(self.cpu_label)
        layout.addStretch(1)

    def clear(self) -> None:
        self.rate_label.setText("上报 — 发送 — 背压 —")
        self.rtt_label.setText("RTT — p95 — 丢包 —")
        self.cpu_label.setText("CPU — 内存 —")

    def update_metrics(self, data: dict[str, Any]) -> None:
        if not data:
            return
        report = _pick(data, ("report_hz", "reportHz", "hz", "report"))
        send = _pick(data, ("send_hz", "sendHz", "send", "tx_hz"))
        backpressure = _pick(data, ("backpressure", "back_pressure", "bp", "queue"))
        rtt = _pick(data, ("rtt_ms", "rtt", "latency_ms", "latency"))
        p95 = _pick(data, ("p95_ms", "p95", "rtt_p95_ms"))
        loss = _pick(data, ("loss_pct", "loss", "packet_loss", "loss_percent"))
        cpu = _pick(data, ("cpu_pct", "cpu", "cpu_percent"))
        mem = _pick(data, ("mem_mb", "memory_mb", "rss_mb", "mem"))

        self.rate_label.setText(
            f"上报 {_fmt(report, '.0f')}Hz 发送 {_fmt(send, '.0f')}Hz "
            f"背压 {_fmt(backpressure, '.0f')}"
        )
        self.rtt_label.setText(
            f"RTT {_fmt(rtt, '.0f')}ms p95 {_fmt(p95, '.0f')}ms "
            f"丢包 {_fmt(loss, '.1f')}%"
        )
        self.cpu_label.setText(
            f"CPU {_fmt(cpu, '.1f')}% 内存 {_fmt(mem, '.0f')}MB"
        )


class _CloseMessage(ctypes.Structure):
    """``MSG`` 的最小映射，只为读取 WM_CLOSE 的窗口句柄。"""

    _fields_ = [
        ("hwnd", ctypes.c_void_p),
        ("message", ctypes.c_uint),
        ("wParam", ctypes.c_size_t),
        ("lParam", ctypes.c_ssize_t),
        ("time", ctypes.c_uint),
        ("pt_x", ctypes.c_long),
        ("pt_y", ctypes.c_long),
    ]


WM_CLOSE = 0x0010


class _NativeCloseFilter(QAbstractNativeEventFilter):
    """在原生消息层截获 WM_CLOSE，绕过模态对话框对关闭的阻断。

    Qt 在应用级模态窗口（``QDialog.exec()``）打开时会拒绝普通窗口的关闭
    请求：``QWidget::close()`` 直接返回 false，``closeEvent`` 根本不触发，
    于是点 X 毫无反应、进程连 adb 一起留在后台。这里把 WM_CLOSE 拦下来
    自己走退出流程，行为与直接点 X 一致。
    """

    def __init__(self, window: MainWindow) -> None:
        super().__init__()
        self._window = window

    def nativeEventFilter(self, event_type, message) -> bool:  # noqa: N802
        if event_type != b"windows_generic_MSG":
            return False
        try:
            msg = ctypes.cast(int(message), ctypes.POINTER(_CloseMessage)).contents
        except (TypeError, ValueError):
            return False
        if msg.message != WM_CLOSE:
            return False
        window = self._window
        try:
            if int(msg.hwnd or 0) != int(window.winId()):
                return False
        except RuntimeError:  # 窗口已销毁
            return False
        window.request_close()
        return True


class MainWindow(QMainWindow):
    """PadLend 主窗；只做外壳与接线，业务在 Router/DesktopMap。"""

    placementChanged = Signal(object)
    settingsSaved = Signal(object)
    # 供热键线程/非 GUI 线程请求“把 PC 剪贴板发给平板”；排队回 GUI 线程读取。
    clipboardSyncRequested = Signal()
    # 设备连接/断开在后台线程执行，完成后经队列信号回到 GUI 线程。
    deviceActionFinished = Signal(bool, str)
    # 「添加无线设备」：adb connect/pair 也在后台线程跑。
    addDeviceFinished = Signal(bool, str)
    locatedFinished = Signal(object)
    macKnown = Signal(str)

    def __init__(
        self,
        config: Config,
        router: object,
        desktop: object,
        *,
        config_path: Path | None = None,
        adb: object | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("mainWindow")
        self.setWindowTitle(C.APP_NAME)
        # 标题栏图标用品牌标（原来的状态圆点会和顶栏状态胶囊重复）。
        self.setWindowIcon(brand_icon())
        self.resize(*tokens.WINDOW_DEFAULT_SIZE)
        self.setMinimumSize(*tokens.WINDOW_MIN_SIZE)
        # 应用级模态对话框会阻断 Qt 的关闭请求，这里自己接 WM_CLOSE。
        self._close_filter = _NativeCloseFilter(self)
        app = QApplication.instance()
        if app is not None:
            app.installNativeEventFilter(self._close_filter)

        self.config = config
        self.router = router
        self.desktop = desktop
        self._adb = adb
        self._config_path = Path(config_path) if config_path is not None else C.config_path()
        self._state = ""
        self._orientation = "—"
        self._pointer_pos: tuple[float, float] = (0.0, 0.0)
        self._placement: object | None = None
        self._force_quit = False
        self._dwm_applied = False
        self._hidden_notice_shown = False
        self._wizard: SetupWizard | None = None
        self._diagnostics: DiagnosticsDialog | None = None
        self._facts: Any | None = None
        self._connecting = False
        self._last_wireless_address = ""
        self._probe_thread: threading.Thread | None = None
        self._shutdown_done = False

        self._build_ui()
        self._build_tray()
        self._build_bridge()
        self.clipboardSyncRequested.connect(
            self._paste_clipboard, Qt.ConnectionType.QueuedConnection
        )
        self.deviceActionFinished.connect(self._on_device_action_finished)
        self.addDeviceFinished.connect(self._on_add_device_finished)
        self.locatedFinished.connect(self._on_located)
        self.macKnown.connect(self._on_mac_known)
        self._build_shortcuts()
        # 自动同步：接管时与每次复制都把文本写进平板剪贴板（0.3.14）。
        self._clipboard_timer = QTimer(self)
        self._clipboard_timer.setSingleShot(True)
        self._clipboard_timer.setInterval(150)
        self._clipboard_timer.timeout.connect(self.sync_clipboard)
        self._watch_clipboard()
        self._build_actions()
        self._connect_canvas()
        self.update_device(None)
        self._refresh_theme_assets()
        self._install_initial_placement()
        self.log_view.append_event("APP", f"{C.APP_NAME} {C.VERSION} 启动")
        log.info("main window ready (canvas=%s)", self.canvas_mode)

    # ------------------------------------------------------------------ 构建
    def _build_ui(self) -> None:
        root = QWidget(self)
        root.setObjectName("root")
        self.setCentralWidget(root)
        layout = QVBoxLayout(root)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        layout.addWidget(self._build_header())

        self.settings = SettingsPanel(
            self.config, self.desktop, config_path=self._config_path, parent=self
        )
        self.canvas, self.canvas_mode = self._create_canvas()
        self.device_page = DevicePage(self._adb, self.router, self)
        self.device_page.connectRequested.connect(self._on_device_connect_requested)
        self.device_page.disconnectRequested.connect(self._on_device_disconnect_requested)
        self.device_page.addRequested.connect(self._on_add_device_requested)
        self.device_page.set_known(self._known_devices())
        self.device_page.removeRequested.connect(self._forget_device)

        self.log_view = LogView(self)
        self.metrics_panel = MetricsPanel(self)

        body = QHBoxLayout()
        body.setContentsMargins(0, 0, 0, 0)
        body.setSpacing(0)
        self.sidebar = self._build_sidebar()
        self.pages = QStackedWidget()
        self.pages.setObjectName("pages")
        self.pages.addWidget(self.device_page)
        self.pages.addWidget(self._build_canvas_page())
        self.pages.addWidget(self._build_settings_page())
        self.pages.addWidget(self._build_logs_page())
        body.addWidget(self.sidebar)
        body.addWidget(self.pages, 1)
        layout.addLayout(body, 1)
        layout.addWidget(self._build_bottom_bar())

        self.settings.placementChanged.connect(self._on_settings_placement)
        self.settings.configChanged.connect(self._on_config_saved)
        self._switch_page(0)

    def _build_header(self) -> QFrame:
        header = QFrame(self)
        header.setObjectName("header")
        header.setFixedHeight(tokens.HEADER_HEIGHT)

        mark = QLabel(header)
        self._brand_mark = mark
        mark.setPixmap(brand_pixmap(34))
        mark.setFixedSize(QSize(34, 34))

        self.wordmark = QLabel(C.APP_NAME, header)
        self.wordmark.setObjectName("wordmark")
        subtitle = QLabel("TABLET BRIDGE", header)
        subtitle.setObjectName("brandSub")
        brand_text = QVBoxLayout()
        brand_text.setSpacing(0)
        brand_text.addWidget(self.wordmark)
        brand_text.addWidget(subtitle)

        divider = QFrame(header)
        divider.setObjectName("headerSeparator")
        divider.setFixedHeight(26)

        self.status_pill = QFrame(header)
        self.status_pill.setObjectName("statusPill")
        pill = QHBoxLayout(self.status_pill)
        pill.setContentsMargins(10, 4, 11, 4)
        pill.setSpacing(7)
        self.status_dot = QLabel(self.status_pill)
        self.status_dot.setObjectName("statusDot")
        size = tokens.STATUS_DOT_SIZE
        self.status_dot.setFixedSize(QSize(size, size))
        self._paint_status_dot("")
        self.status_text = QLabel(tokens.state_label(""), self.status_pill)
        self.status_text.setObjectName("statusText")
        pill.addWidget(self.status_dot)
        pill.addWidget(self.status_text)

        self.device_text = QLabel("未连接设备", header)
        self.device_text.setObjectName("deviceText")
        self.orientation_text = QLabel("—", header)
        self.orientation_text.setObjectName("orientationText")
        self.latency_text = QLabel("RTT —", header)
        self.latency_text.setObjectName("latencyText")

        icon_color = tokens.COLORS["text_secondary"]
        self.menu_button = QPushButton(header)
        self.settings_button = QPushButton(header)
        self.theme_button = QPushButton(header)
        self._header_icon_names: list[tuple[QPushButton, str]] = [
            (self.menu_button, "dots"),
            (self.settings_button, "settings"),
        ]
        for button, icon_name, tooltip in (
            (self.menu_button, "dots", "菜单"),
            (self.settings_button, "settings", "设置 (Ctrl+,)"),
            (self.theme_button, "sun", "切换到白天模式"),
        ):
            button.setObjectName("iconButton")
            button.setFixedSize(QSize(28, 28))
            button.setCursor(Qt.CursorShape.PointingHandCursor)
            button.setIcon(make_icon(icon_name, icon_color, 18))
            button.setIconSize(QSize(18, 18))
            button.setToolTip(tooltip)

        self.settings_button.clicked.connect(self.open_settings)
        self.theme_button.clicked.connect(self.toggle_theme)
        self.menu_button.clicked.connect(self._popup_menu)

        layout = QHBoxLayout(header)
        layout.setContentsMargins(18, 0, 12, 0)
        layout.setSpacing(9)
        layout.addWidget(mark)
        layout.addLayout(brand_text)
        layout.addSpacing(8)
        layout.addWidget(divider)
        layout.addSpacing(4)
        layout.addWidget(self.status_pill)
        layout.addSpacing(8)
        layout.addWidget(self.device_text)
        layout.addWidget(self.orientation_text)
        layout.addStretch(1)
        layout.addWidget(self.latency_text)
        layout.addSpacing(6)
        layout.addWidget(self.menu_button)
        layout.addWidget(self.settings_button)
        layout.addWidget(self.theme_button)
        self.header = header
        return header

    def _build_sidebar(self) -> QFrame:
        bar = QFrame(self)
        bar.setObjectName("sideBar")
        bar.setFixedWidth(tokens.SIDEBAR_WIDTH)
        layout = QVBoxLayout(bar)
        layout.setContentsMargins(9, 16, 9, 14)
        layout.setSpacing(7)

        self.nav_buttons: list[QToolButton] = []
        self._nav_icon_names = ["devices", "canvas", "settings", "logs"]
        self.nav_group = QButtonGroup(bar)
        self.nav_group.setExclusive(True)
        for index, (name, label) in enumerate(
            (("devices", "设备"), ("canvas", "布局"), ("settings", "设置"), ("logs", "日志"))
        ):
            button = QToolButton(bar)
            button.setObjectName("navButton")
            button.setText(label)
            button.setIcon(make_icon(name, tokens.COLORS["text_primary"], 20))
            button.setIconSize(QSize(20, 20))
            button.setToolButtonStyle(Qt.ToolButtonStyle.ToolButtonTextUnderIcon)
            button.setCheckable(True)
            button.setCursor(Qt.CursorShape.PointingHandCursor)
            button.setToolTip(label)
            self.nav_group.addButton(button, index)
            self.nav_buttons.append(button)
            layout.addWidget(button)
        layout.addStretch(1)
        self.nav_group.idClicked.connect(self._switch_page)
        return bar

    def _build_canvas_page(self) -> QWidget:
        page = QWidget()
        page.setObjectName("canvasPage")
        layout = QVBoxLayout(page)
        layout.setContentsMargins(22, 20, 24, 18)
        layout.setSpacing(12)

        title = QLabel("桌面布局")
        title.setObjectName("pageTitle")
        hint = QLabel("把平板拖到屏幕外轮廓边；Shift 切换角落优先边，双击平板切换横竖。")
        hint.setObjectName("pageSubtitle")
        hint.setWordWrap(True)
        heading = QVBoxLayout()
        heading.setSpacing(3)
        heading.addWidget(title)
        heading.addWidget(hint)
        row = QHBoxLayout()
        row.addLayout(heading, 1)
        self.fit_button = QPushButton("适应窗口")
        self.fit_button.setObjectName("ghostButton")
        self.fit_button.setIcon(make_icon("canvas", tokens.COLORS["text_primary"], 15))
        self.fit_button.clicked.connect(lambda: self._canvas_call("fit_view"))
        row.addWidget(self.fit_button, 0, Qt.AlignmentFlag.AlignVCenter)
        layout.addLayout(row)

        card = QFrame()
        card.setObjectName("canvasCard")
        card_box = QVBoxLayout(card)
        card_box.setContentsMargins(2, 2, 2, 2)
        card_box.addWidget(self.canvas)
        layout.addWidget(card, 1)
        return page

    def _build_settings_page(self) -> QWidget:
        page = QWidget()
        page.setObjectName("settingsPage")
        layout = QVBoxLayout(page)
        layout.setContentsMargins(24, 20, 24, 16)
        layout.setSpacing(10)

        title = QLabel("设置")
        title.setObjectName("pageTitle")
        hint = QLabel("摆位、手感、键盘与快捷键；保存后立即生效。")
        hint.setObjectName("pageSubtitle")
        layout.addWidget(title)
        layout.addWidget(hint)

        # SettingsPanel 自带滚动区；直接铺满页面，不要套第二层滚动/限宽。
        layout.addWidget(self.settings, 1)
        return page

    def _build_logs_page(self) -> QWidget:
        page = QWidget()
        page.setObjectName("logsPage")
        layout = QVBoxLayout(page)
        layout.setContentsMargins(22, 20, 24, 16)
        layout.setSpacing(10)

        title = QLabel("运行日志")
        title.setObjectName("pageTitle")
        hint = QLabel("固定事件名与实时指标；复制诊断时会包含最近日志。")
        hint.setObjectName("pageSubtitle")
        layout.addWidget(title)
        layout.addWidget(hint)

        split = QSplitter(Qt.Orientation.Horizontal)
        split.setChildrenCollapsible(False)
        split.addWidget(self.log_view)
        split.addWidget(self.metrics_panel)
        split.setStretchFactor(0, 1)
        split.setStretchFactor(1, 0)
        split.setSizes([820, _METRICS_WIDTH])
        layout.addWidget(split, 1)
        self.logs_split = split
        return page

    def _build_bottom_bar(self) -> QFrame:
        bar = QFrame(self)
        bar.setObjectName("bottomBar")
        bar.setFixedHeight(30)
        layout = QHBoxLayout(bar)
        layout.setContentsMargins(16, 0, 14, 0)
        layout.setSpacing(10)
        self.bottom_state = QLabel("未连接设备", bar)
        self.bottom_state.setObjectName("bottomHint")
        self.bottom_hint = QLabel("等待设备连接 · 命令行参数仅用于开发验证", bar)
        self.bottom_hint.setObjectName("bottomHint")
        layout.addWidget(self.bottom_state)
        layout.addStretch(1)
        layout.addWidget(self.bottom_hint)
        version = QLabel(f"v{C.VERSION}", bar)
        version.setObjectName("bottomHint")
        layout.addWidget(version)
        self.bottom_bar = bar
        return bar

    def _switch_page(self, index: int) -> None:
        if index < 0 or index >= self.pages.count():
            return
        self.pages.setCurrentIndex(index)
        for position, button in enumerate(self.nav_buttons):
            button.setChecked(position == index)
        if index == 0:
            self.device_page.refresh()
        elif index == 1:
            QTimer.singleShot(0, lambda: self._canvas_call("fit_view"))

    def _create_canvas(self) -> tuple[QWidget, str]:
        canvas_cls = DesktopCanvas
        if canvas_cls is not None:
            initial = self.settings.current_placement()
            candidates: list[tuple[tuple[Any, ...], dict[str, Any]]] = [
                ((self.desktop, initial), {"parent": self}),
                ((self.desktop, initial), {}),
                ((self.desktop,), {"parent": self}),
                ((self.desktop,), {}),
                ((), {}),
            ]
            for args, kwargs in candidates:
                try:
                    widget = canvas_cls(*args, **kwargs)
                except TypeError:
                    continue
                except Exception as exc:  # 画布内部初始化失败
                    log.warning("canvas init failed (%r); using placeholder", exc)
                    break
                if isinstance(widget, QWidget):
                    widget.setParent(self)
                    log.info("canvas loaded: %s", type(widget).__name__)
                    return widget, "native"
        placeholder = QLabel(
            "虚拟桌面画布暂不可用\nui/canvas.py 尚未就绪（并行开发中）",
            self,
        )
        placeholder.setObjectName("canvasPlaceholder")
        placeholder.setAlignment(Qt.AlignmentFlag.AlignCenter)
        placeholder.setMinimumSize(QSize(480, 360))
        placeholder.setWordWrap(True)
        log.warning("canvas unavailable; placeholder shown")
        return placeholder, "placeholder"

    def _build_bridge(self) -> None:
        self.bridge = RouterBridge(self.router, self)
        if self.router is not None:
            try:
                self.bridge.attach(self.router)
            except Exception as exc:
                log.warning("bridge.attach failed: %r", exc)
        self.bridge.stateChanged.connect(self._on_state)
        self.bridge.metricsChanged.connect(self._on_metrics)
        self.bridge.logLine.connect(self._on_log)
        self._on_state("", state_value(getattr(self.bridge, "state", "")), "")

    def _build_tray(self) -> None:
        self.tray = TrayController(self)
        self.tray.set_state(self._state)
        self.tray.toggleManualRequested.connect(self.toggle_manual)
        self.tray.reconnectRequested.connect(self.reconnect)
        self.tray.openConfigRequested.connect(self.open_config)
        self.tray.copyDiagnosticsRequested.connect(self.copy_diagnostics)
        self.tray.quitRequested.connect(self.quit_app)
        self.tray.visibilityToggled.connect(self.toggle_window)
        self.tray.set_notifications(
            bool(getattr(getattr(self.config, "ui", None), "tray_notifications", True))
        )
        self.menu_button.setVisible(not self.tray.available)
        if self.tray.available:
            self.tray.show()

    def _build_shortcuts(self) -> None:
        self.settings_shortcut = QShortcut(QKeySequence("Ctrl+,"), self)
        self.settings_shortcut.activated.connect(self.open_settings)
        self.toggle_shortcut = QShortcut(QKeySequence("Ctrl+Alt+S"), self)
        self.toggle_shortcut.activated.connect(self.toggle_manual)
        self.panic_shortcut = QShortcut(QKeySequence("Ctrl+Alt+Q"), self)
        self.panic_shortcut.activated.connect(self.panic_release)

    def _build_actions(self) -> None:
        self.diagnostics_action = QAction("运行诊断…", self)
        self.diagnostics_action.triggered.connect(self.show_diagnostics)
        self.wizard_action = QAction("首次设置向导…", self)
        self.wizard_action.triggered.connect(self.show_wizard)

        menu = self.tray.menu
        insert_before = self.tray.quit_action
        menu.insertAction(insert_before, self.diagnostics_action)
        menu.insertAction(insert_before, self.wizard_action)
        menu.insertSeparator(insert_before)

    # ------------------------------------------------------------------ 画布接线
    def _connect_canvas(self) -> None:
        bindings = (
            ("placementEdited", self._on_placement_edited),
            ("placementCommitted", self._on_placement_committed),
            ("tabletOrientationToggled", self._on_tablet_orientation_toggled),
            ("testCaptureRequested", self._on_test_capture_requested),
            ("pointerMoved", self._on_pointer_moved),
        )
        for name, slot in bindings:
            signal = getattr(self.canvas, name, None)
            if signal is None or not hasattr(signal, "connect"):
                continue
            try:
                signal.connect(slot)
            except (RuntimeError, TypeError) as exc:
                log.warning("canvas.%s connect failed: %r", name, exc)

    def _canvas_call(self, name: str, *args: object) -> object | None:
        method = getattr(self.canvas, name, None)
        if not callable(method):
            return None
        try:
            return method(*args)
        except Exception as exc:
            log.warning("canvas.%s failed: %r", name, exc)
            return None

    def _install_initial_placement(self) -> None:
        placement = self.settings.current_placement()
        if placement is not None:
            self._placement = placement
            self._canvas_call("set_placement", placement)
            # 启动即把配置里的摆位交给 Router，否则用户不拖动画布时
            # 边缘穿越永远不会触发（Router 只有在 set_placement 后才有段）。
            setter = getattr(self.router, "set_placement", None)
            if callable(setter):
                try:
                    setter(placement)
                except Exception as exc:
                    log.warning("router.set_placement(initial) failed: %r", exc)

    def _on_placement_edited(self, *_args: object) -> None:
        placement = _args[0] if _args else None
        if placement is None:
            return
        self._placement = placement
        log.debug("placement edited: %r", placement)
        self.placementChanged.emit(placement)

    def _on_placement_committed(self, *_args: object) -> None:
        placement = _args[0] if _args else None
        if placement is None:
            return
        self._placement = placement
        self._apply_placement_to_config(placement)
        saved = self._save_config()
        self._push_placement(placement)
        segment = getattr(placement, "segment", None)
        edge = getattr(getattr(segment, "edge", None), "value", "?")
        offset = float(getattr(placement, "offset", 0.5))
        self.log_view.append_event("PLACE", f"吸附 {edge} offset={offset:.2f} 已保存={saved}")
        self.placementChanged.emit(placement)
        if saved:
            self.settingsSaved.emit(self.config)
        load = getattr(self.settings, "load_from_config", None)
        if callable(load):
            load()

    def _apply_placement_to_config(self, placement: object) -> None:
        cfg = getattr(self.config, "placement", None)
        if cfg is None:
            return
        try:
            segment = getattr(placement, "segment", None)
            if segment is not None:
                edge = getattr(segment, "edge", None)
                cfg.edge = getattr(edge, "value", cfg.edge)
                cfg.span_start = int(getattr(segment, "span_start", cfg.span_start))
                cfg.span_end = int(getattr(segment, "span_end", cfg.span_end))
                cfg.monitor = self._monitor_device(
                    int(getattr(segment, "monitor_handle", 0))
                ) or cfg.monitor
            cfg.offset = float(getattr(placement, "offset", cfg.offset))
            align = getattr(placement, "align", None)
            cfg.align = getattr(align, "value", cfg.align)
            corner = getattr(placement, "corner_priority", None)
            cfg.corner_priority = getattr(corner, "value", "") if corner else ""
        except (AttributeError, TypeError, ValueError) as exc:
            log.warning("apply placement to config failed: %r", exc)

    def _push_placement(self, placement: object) -> None:
        self._canvas_call("set_placement", placement)
        setter = getattr(self.router, "set_placement", None)
        if callable(setter):
            try:
                setter(placement)
            except Exception as exc:
                log.warning("router.set_placement failed: %r", exc)
        if self._facts is not None:
            self.update_device(self._facts)

    def _on_tablet_orientation_toggled(self, *_args: object) -> None:
        self._orientation = "竖屏" if self._orientation == "横屏" else "横屏"
        self.orientation_text.setText(self._orientation)
        self.log_view.append_event("ORIENT", f"平板朝向切换为 {self._orientation}")
        self._canvas_call("refresh_desktop")

    def _on_test_capture_requested(self, *_args: object) -> None:
        log.info("canvas requested capture test")
        self.log_view.append_event("TEST", "画布请求测试接管")
        self.toggle_manual()

    def _on_pointer_moved(self, *args: object) -> None:
        if len(args) >= 2:
            try:
                self._pointer_pos = (float(args[0]), float(args[1]))
            except (TypeError, ValueError):
                return
        log.debug("pointer moved: %r", self._pointer_pos)

    # ------------------------------------------------------------------ 状态/指标
    def _present_state(self, state: str) -> str:
        """未拿到设备事实前，不把 FAULT 当成用户可见的“故障”。"""
        if not state:
            return "disconnected"
        if self._facts is None and state in ("fault", "idle"):
            return "connecting" if self._connecting else "disconnected"
        return state

    def _on_state(self, old: object, new: object, reason: object) -> None:
        old_value = state_value(old)
        new_value = state_value(new)
        self._state = new_value
        present = self._present_state(new_value)
        self._paint_status_dot(present)
        self.status_text.setText(tokens.state_label(present))
        self.status_text.setProperty("state", present)
        repolish(self.status_text)
        self.tray.set_state(present)
        self.tray.set_connected(self._facts is not None)
        self.device_page.set_state(present)
        if present == "disconnected":
            self.bottom_state.setText("未连接设备")
        elif present == "connecting":
            self.bottom_state.setText("正在连接设备…")
        else:
            self.bottom_state.setText(f"状态 · {tokens.state_label(present)}")
        text = f"{old_value or '-'} → {new_value or '未连接'}"
        if reason:
            text = f"{text} {reason}"
        self.log_view.append_event("STATE", text)
        if new_value == "captured":
            # 接管瞬间同步一次：SET_CLIPBOARD 只写平板剪贴板，不敲键盘，
            # 所以不会再出现「穿进去先按到某个键」的问题（0.3.14）。
            self._schedule_clipboard_sync()

    # ------------------------------------------------------------------ 剪贴板
    def _watch_clipboard(self) -> None:
        """PC 剪贴板一变就排队同步（只在接管期间，节流见 ``_clipboard_timer``）。"""
        try:
            clip = QGuiApplication.clipboard()
        except Exception as exc:  # noqa: BLE001 - 无剪贴板时静默降级
            log.debug("clipboard unavailable: %r", exc)
            return
        if clip is not None:
            clip.dataChanged.connect(self._on_clipboard_changed)

    def _on_clipboard_changed(self) -> None:
        if self._state != "captured":
            return
        self._schedule_clipboard_sync()

    def _schedule_clipboard_sync(self) -> None:
        """合并短时间内的多次复制，避免一次复制发多条报文。"""
        if not bool(getattr(getattr(self.config, "misc", None), "clipboard_sync", False)):
            return
        timer = getattr(self, "_clipboard_timer", None)
        if timer is not None:
            timer.start()

    def _paste_clipboard(self) -> None:
        """Ctrl+Alt+V：除了写剪贴板，还让平板在焦点输入框里粘贴一次。"""
        self.sync_clipboard(paste=True)

    def sync_clipboard(self, *, paste: bool = False) -> None:
        """把 PC 剪贴板文本写进平板剪贴板（6.8，SHOULD）。

        勾选 ``misc.clipboard_sync`` 时，接管瞬间与每次复制都会自动调用
        （``paste=False``：只同步剪贴板，不敲键盘）；``Ctrl+Alt+V`` 走
        ``_paste_clipboard``，额外让平板粘贴一次。图片/富文本不做。
        """
        if not bool(getattr(getattr(self.config, "misc", None), "clipboard_sync", False)):
            return
        try:
            text = QGuiApplication.clipboard().text()
        except Exception as exc:  # noqa: BLE001 - 剪贴板不可用不应影响主流程
            log.debug("clipboard read failed: %r", exc)
            return
        if not text:
            return
        inject = getattr(self.router, "inject_clipboard", None)
        if not callable(inject):
            return
        try:
            inject(text, paste=paste)
        except TypeError:  # 兼容只接受一个参数的旧 router 替身
            inject(text)
        except Exception as exc:  # noqa: BLE001
            log.debug("clipboard inject failed: %r", exc)

    def _paint_status_dot(self, state: str) -> None:
        color = tokens.state_color(state)
        radius = tokens.STATUS_DOT_SIZE // 2
        self.status_dot.setStyleSheet(
            f"background-color: {color}; border-radius: {radius}px;"
        )

    def _on_metrics(self, data: object) -> None:
        if not isinstance(data, dict):
            return
        self.metrics_panel.update_metrics(data)
        rtt = _pick(data, ("rtt_ms", "rtt", "latency_ms", "latency"))
        self.latency_text.setText(f"RTT {rtt:.0f}ms" if rtt is not None else "RTT —")

    def _on_log(self, line: object) -> None:
        self.log_view.append_line(str(line))

    def _on_settings_placement(self, placement: object) -> None:
        self._placement = placement
        self._canvas_call("set_placement", placement)
        self.placementChanged.emit(placement)

    def _on_config_saved(self, config: object) -> None:
        self.log_view.append_event("CONFIG", f"配置已保存：{self._config_path}")
        self.tray.set_notifications(
            bool(getattr(getattr(config, "ui", None), "tray_notifications", True))
        )
        placement = self.settings.current_placement()
        if placement is not None:
            self._push_placement(placement)
        self._refresh_header_device()

    # ------------------------------------------------------------------ 动作
    def open_settings(self) -> None:
        """Ctrl+, / ⚙：切到设置页并聚焦第一个控件。"""
        self.showNormal()
        self.raise_()
        self.activateWindow()
        self._switch_page(2)
        self.settings.focus_first()

    def toggle_theme(self) -> None:
        """白天/黑夜主题切换，并持久化到 config.ui.theme。"""
        self.apply_theme_choice("light" if tokens.current_theme() == "dark" else "dark")

    def apply_theme_choice(self, name: str) -> None:
        """应用并保存主题（``dark`` / ``light``），同步所有自绘制图标与颜色。"""
        chosen = tokens.set_theme(name)
        app = QApplication.instance()
        if app is not None:
            apply_theme(app, chosen)
        self.config.ui.theme = chosen
        self._refresh_theme_assets()
        self._apply_dwm_dark(chosen == "dark")
        self._save_config()
        self.log_view.append_event("UI", f"主题：{'白天' if chosen == 'light' else '黑夜'}")

    def _refresh_theme_assets(self) -> None:
        """主题切换后重绘 QSS 管不到的图标、品牌标与状态点。"""
        muted = tokens.COLORS["text_secondary"]
        for button, icon_name in self._header_icon_names:
            button.setIcon(make_icon(icon_name, muted, 18))
        current = tokens.current_theme()
        self.theme_button.setIcon(
            make_icon("moon" if current == "light" else "sun", muted, 18)
        )
        self.theme_button.setToolTip(
            "切换到黑夜模式" if current == "light" else "切换到白天模式"
        )
        self._brand_mark.setPixmap(brand_pixmap(34))
        primary = tokens.COLORS["text_primary"]
        for button, icon_name in zip(self.nav_buttons, self._nav_icon_names, strict=False):
            button.setIcon(make_icon(icon_name, primary, 20))
        if hasattr(self, "fit_button"):
            self.fit_button.setIcon(make_icon("canvas", primary, 15))
        self._paint_status_dot(self._present_state(self._state))
        if hasattr(self, "device_page"):
            self.device_page.refresh_theme()
        if hasattr(self, "canvas"):
            self.canvas.update()
        if hasattr(self, "tray"):
            self.tray.set_state(self._present_state(self._state))

    def toggle_window(self) -> None:
        if self.isVisible() and not self.isMinimized():
            self.hide()
        else:
            self.showNormal()
            self.raise_()
            self.activateWindow()

    def toggle_manual(self) -> None:
        action = getattr(self.router, "toggle_manual", None)
        if not callable(action):
            self.log_view.append_event("MANUAL", "Router 未就绪，无法切换接管")
            return
        try:
            result = action()
        except Exception as exc:
            log.warning("router.toggle_manual failed: %r", exc)
            self.log_view.append_event("MANUAL", f"切换失败：{exc}")
            return
        self.log_view.append_event("MANUAL", f"手动接管切换 → {result!r}")

    def panic_release(self) -> None:
        # 优先 emergency_stop：只交还控制权并回到 IDLE，不停线程，
        # 用户松手后还能继续用；老 Router 没有该方法时退回 stop()。
        action = getattr(self.router, "emergency_stop", None) or getattr(
            self.router, "stop", None
        )
        if callable(action):
            try:
                action()
            except Exception as exc:
                log.warning("router emergency stop failed: %r", exc)
        self.log_view.append_event("PANIC", "急停：已交还控制权")

    def reconnect(self) -> None:
        action = getattr(self.router, "resync_pointer", None)
        if not callable(action):
            self.log_view.append_event("RECONNECT", "Router 未就绪，无法重连")
            return
        try:
            action()
        except Exception as exc:
            log.warning("router.resync_pointer failed: %r", exc)
            self.log_view.append_event("RECONNECT", f"重连失败：{exc}")
            return
        self.log_view.append_event("RECONNECT", "已请求重新同步")

    def open_config(self) -> None:
        path = Path(self._config_path)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            if not path.exists():
                self._save_config()
        except OSError as exc:
            log.error("prepare config path failed: %r", exc)
            self.log_view.append_event("CONFIG", f"无法创建配置目录：{exc}")
            return
        try:
            if sys.platform == "win32":
                os.startfile(str(path))  # type: ignore[attr-defined]
            else:
                from PySide6.QtCore import QUrl
                from PySide6.QtGui import QDesktopServices

                QDesktopServices.openUrl(QUrl.fromLocalFile(str(path)))
        except OSError as exc:
            log.error("open config failed: %r", exc)
            self.log_view.append_event("CONFIG", f"打开配置失败：{exc}")

    def copy_diagnostics(self) -> None:
        text = static_summary()
        try:
            QGuiApplication.clipboard().setText(text)
        except RuntimeError:
            self.log_view.append_event("DIAG", "剪贴板不可用")
            return
        self.log_view.append_event("DIAG", "诊断摘要已复制到剪贴板")
        self.tray.notify(C.APP_NAME, "诊断摘要已复制")

    def show_diagnostics(self) -> DiagnosticsDialog:
        dialog = DiagnosticsDialog(self)
        self._diagnostics = dialog
        dialog.start()
        dialog.exec()
        return dialog

    def show_wizard(self) -> SetupWizard:
        wizard = SetupWizard(self, config=self.config)
        self._wizard = wizard
        wizard.completed.connect(lambda: self.log_view.append_event("SETUP", "首次设置已完成"))
        wizard.exec()
        return wizard

    def quit_app(self) -> None:
        self.request_quit()

    def _close_to_tray(self) -> bool:
        """X 应该只收进托盘吗？（设置开启 + 托盘真的存在 + 不是强制退出）"""
        if self._force_quit or self._shutdown_done:
            return False
        if not bool(getattr(self.config.ui, "close_to_tray", False)):
            return False
        tray = getattr(self, "tray", None)
        return bool(tray is not None and getattr(tray, "available", False))

    def request_close(self) -> bool:
        """处理“用户想关窗口”。

        返回 True 表示窗口已收进托盘、调用方不要继续关闭；返回 False 表示
        已经走完退出流程（``request_quit``）。
        """
        if self._shutdown_done:
            return True
        if self._close_to_tray():
            if not self.isHidden():
                self.hide()
                notify = getattr(self.tray, "notify", None)
                if callable(notify):
                    notify(C.APP_NAME, "已最小化到托盘，从托盘菜单可重新打开或退出。")
                log.info("window hidden to tray (ui.close_to_tray)")
            return True
        self.request_quit()
        return False

    def request_quit(self) -> None:
        """优雅退出入口：无论窗口是否被模态对话框挡住都能走完清理。

        顺序很关键：先隐藏窗口（用户立刻看到“已退出”），再关掉模态对话框
        （让它们的 ``exec()`` 嵌套循环返回），然后做幂等清理，最后交给事件
        循环 ``quit()``。
        """
        if self._shutdown_done:
            return
        self._force_quit = True
        self.hide()
        self._close_modal_dialogs()
        self._shutdown()
        app = QApplication.instance()
        if app is not None:
            QTimer.singleShot(0, app.quit)

    def _shutdown(self) -> None:
        """幂等清理：桥、托盘、向导、设备页扫描线程。"""
        if self._shutdown_done:
            return
        self._shutdown_done = True
        for step in (
            lambda: self.bridge.detach(),
            lambda: self.tray.dispose(),
            lambda: self._wizard.stop_all() if self._wizard is not None else None,
            lambda: self.device_page.stop(),
        ):
            try:
                step()
            except Exception as exc:  # noqa: BLE001 - 退出清理绝不阻断
                log.warning("shutdown step failed: %r", exc)

    def _popup_menu(self) -> None:
        position = self.menu_button.mapToGlobal(self.menu_button.rect().bottomLeft())
        self.tray.menu.popup(position)

    # ------------------------------------------------------------------ 配置/标题
    def _monitor_device(self, handle: int) -> str:
        for monitor in getattr(self.desktop, "monitors", ()) or ():
            if getattr(monitor, "handle", None) == handle:
                return str(getattr(monitor, "device", ""))
        return ""

    def _refresh_header_device(self) -> None:
        self.update_device(self._facts)

    # ------------------------------------------------------------------ 设备管理
    def update_device(self, facts: Any | None) -> None:
        """把连接事实同步到顶栏/画布/设备页；None 表示未连接。"""
        self._facts = facts
        if facts is not None:
            serial = str(getattr(facts, "serial", "") or "")
            model = str(getattr(facts, "model", "") or "")
            width = int(getattr(facts, "width", 0) or 0)
            height = int(getattr(facts, "height", 0) or 0)
            orientation = int(getattr(facts, "orientation", 0) or 0)
            self._orientation = "横屏" if orientation & 1 else "竖屏"
            self.device_text.setText(model or serial or "已连接")
            self.device_text.setToolTip(serial)
            if width and height:
                self.orientation_text.setText(f"{self._orientation} · {width}×{height}")
                self._canvas_call("set_tablet_size", width, height)
                self.bottom_hint.setText(
                    self._connected_hint(f"{self._orientation} {width}×{height}")
                )
            else:
                self.orientation_text.setText(self._orientation)
                self.bottom_hint.setText(self._connected_hint("已连接 · 正在读取显示信息"))
            self._canvas_call("set_connected", True)
            self.device_page.set_facts(facts)
            self.device_page.set_connected(True)
            self.bottom_state.setText(f"{model or '平板'} · {serial}")
            self.tray.set_device(serial)
            self.tray.set_connected(True)
        else:
            self._orientation = "—"
            self.device_text.setText("未连接设备")
            self.device_text.setToolTip("")
            self.orientation_text.setText("—")
            self._canvas_call("set_connected", False)
            self.device_page.set_facts(None)
            self.device_page.set_connected(False)
            self.bottom_state.setText("未连接设备")
            self.bottom_hint.setText("在设备页连接平板 · 命令行仅用于开发验证")
            self.tray.set_device("")
            self.tray.set_connected(False)
        # 设备事实变化后重新呈现状态：连接过程中 _on_state 可能先以
        # “未连接”落盘，等 facts 到达后必须刷新状态胶囊/设备页标签。
        self._on_state(self._state, self._state, "device.updated")

    def _placement_edge_label(self) -> str:
        segment = getattr(self._placement, "segment", None)
        edge = getattr(segment, "edge", None)
        value = str(getattr(edge, "value", edge or "")).lower()
        return {
            "left": "左侧",
            "right": "右侧",
            "top": "上侧",
            "bottom": "下侧",
        }.get(value, "")

    def _connected_hint(self, base: str) -> str:
        label = self._placement_edge_label()
        if label:
            return f"{base} · 从屏幕{label}边缘滑入平板"
        return f"{base} · 在“布局”页把平板拖到屏幕边缘"

    def _on_add_device_requested(self) -> None:
        """「添加设备」：输入无线调试地址（需要时先配对），成功后直接连接。"""
        prefill = self._last_wireless_address or str(
            getattr(getattr(self.config, "device", None), "serial", "") or ""
        )
        dialog = AddDeviceDialog(self, prefill=prefill)
        # Accepted=1 / Rejected=0；点右上角关闭也是 0。
        if not dialog.exec():
            return
        address = dialog.address
        code = dialog.pairing_code
        if not address:
            return
        self._last_wireless_address = address
        self.device_page.set_busy(True)
        self._set_scan_hint_safe(f"正在连接 {address}…")
        self.log_view.append_event(
            "DEVICE", f"添加无线设备：{address}" + ("（含配对）" if code else "")
        )
        threading.Thread(
            target=self._add_device_worker,
            args=(address, code),
            name="padlend-add-device",
            daemon=True,
        ).start()

    def _add_device_worker(self, address: str, code: str) -> None:
        ok, message = True, ""
        adb = self._adb
        try:
            if adb is None:
                ok, message = False, "adb 不可用（打包版应内置）"
            else:
                if code:
                    adb.pair(address, code)
                adb.connect(address)
        except Exception as exc:  # noqa: BLE001 - 结果回报给 GUI 线程
            ok, message = False, str(exc)
        self.addDeviceFinished.emit(bool(ok), str(message or ""))

    def _on_add_device_finished(self, ok: bool, message: str) -> None:
        if not ok:
            self.device_page.set_busy(False)
            self.device_page.show_error(message or "添加设备失败")
            self.log_view.append_event("DEVICE", f"添加失败：{message or '未知错误'}")
            return
        address = self._last_wireless_address
        self.log_view.append_event("DEVICE", f"无线连接成功：{address}")
        self._remember_device(address)
        # 列表里出现该设备后，沿用普通设备页的连接流程（router.connect_device）。
        self.device_page.refresh()
        if address:
            self._on_device_connect_requested(address)
        else:
            self.device_page.set_busy(False)

    def _set_scan_hint_safe(self, text: str) -> None:
        """设备页的扫描提示是私有的，这里只做存在性检查后写入。"""
        hint = getattr(self.device_page, "_set_scan_hint", None)  # noqa: SLF001
        if callable(hint):
            hint(text)

    def _on_device_connect_requested(self, serial: str) -> None:
        serial = str(serial or "").strip()
        if not serial:
            self.device_page.show_error("请选择一个已授权的设备")
            return
        self.device_page.set_busy(True)
        self.log_view.append_event("DEVICE", f"连接请求：{serial}")
        self._run_device_action(self._connect_worker, serial)

    def _on_device_disconnect_requested(self) -> None:
        self._connecting = False
        self.device_page.set_busy(True)
        self.log_view.append_event("DEVICE", "断开当前设备")
        self._run_device_action(self._disconnect_worker)

    def _connect_worker(self, serial: str) -> tuple[bool, str]:
        connect = getattr(self.router, "connect_device", None)
        if not callable(connect):
            return False, "当前 Router 不支持设备切换"
        # 记忆里的无线地址端口可能变了（平板重启后 adb 端口会换），先补一次
        # adb connect 让设备重新出现在 devices 里；失败不急着报错，交给下面的
        # 超时兜底给结论（USB 设备走不到这里）。
        if self._adb is not None and ":" in serial:
            adb_connect = getattr(self._adb, "connect", None)
            if callable(adb_connect):
                try:
                    adb_connect(serial)
                except Exception as exc:  # noqa: BLE001 - 连接失败由超时兜底
                    log.info("adb connect %s failed: %r", serial, exc)
        try:
            connect(serial)
        except Exception as exc:  # noqa: BLE001 - 回报给 GUI 线程
            return False, str(exc)
        deadline = time.monotonic() + 45.0
        while time.monotonic() < deadline:
            facts = getattr(self.router, "facts", None)
            if facts is not None and getattr(facts, "serial", ""):
                return True, ""
            time.sleep(0.1)
        return False, "连接超时：请确认 USB 调试已授权，且 adb 能看到设备"

    def _disconnect_worker(self) -> tuple[bool, str]:
        disconnect = getattr(self.router, "disconnect_device", None)
        if not callable(disconnect):
            return False, "当前 Router 不支持断开"
        try:
            disconnect()
        except Exception as exc:  # noqa: BLE001
            return False, str(exc)
        return True, ""

    def _run_device_action(self, action: Any, *args: Any) -> None:
        def runner() -> None:
            try:
                ok, message = action(*args)
            except Exception as exc:  # noqa: BLE001 - 后台线程绝不吞异常退出
                ok, message = False, str(exc)
            self.deviceActionFinished.emit(bool(ok), str(message or ""))

        threading.Thread(target=runner, name="padlend-device-action", daemon=True).start()

    def _on_device_action_finished(self, ok: bool, message: str) -> None:
        self._connecting = False
        self.device_page.set_busy(False)
        if ok:
            facts = getattr(self.router, "facts", None)
            self.update_device(facts)
            if facts is not None:
                serial = str(getattr(facts, "serial", "") or "")
                self._remember_device(serial)
                self.log_view.append_event("DEVICE", f"已连接 {serial}".strip())
            else:
                self.log_view.append_event("DEVICE", "已断开")
        else:
            self.log_view.append_event("DEVICE", f"操作失败：{message or '未知错误'}")
            self.device_page.show_error(message or "操作失败")


    # -------------------------------------------------------------- 设备记忆
    def _known_devices(self) -> list[str]:
        """最近连过的设备，新的在前；当前配置里的 serial 永远排第一。"""
        known: list[str] = []
        current = str(getattr(self.config.device, "serial", "") or "")
        stored = list(getattr(self.config.device, "known", []) or [])
        for serial in [current, *stored]:
            text = str(serial or "").strip()
            if text and text not in known:
                known.append(text)
        return known

    def _remember_device(self, serial: str) -> None:
        """把设备写进 known（去重、新的在前、最多 8 条）并落盘。"""
        text = str(serial or "").strip()
        if not text:
            return
        known = [text, *(s for s in self._known_devices() if s != text)]
        self.config.device.known = known[:8]
        if str(getattr(self.config.device, "serial", "") or "") != text:
            self.config.device.serial = text
        self.device_page.set_known(self._known_devices())
        self._save_config()
        self._capture_mac(text)

    def _forget_device(self, serial: str) -> None:
        """从记忆列表里删掉一台设备（0.3.13：设备列表可删除）。"""
        text = str(serial or "").strip()
        if not text:
            return
        known = [s for s in (self.config.device.known or []) if str(s) != text]
        self.config.device.known = known
        if str(getattr(self.config.device, "serial", "") or "") == text:
            self.config.device.serial = known[0] if known else ""
        self.device_page.set_located(text, "")
        self.device_page.set_known(self._known_devices())
        self._save_config()
        self.log_view.append_event("DEVICE", f"已从列表移除 {text}")

    # ------------------------------------------------------------ 启动探测
    def probe_last_device(self) -> None:
        """打开应用后先确认上次那台平板还在不在线（0.3.13）。

        只有记过 ``ip:port`` 的无线设备才需要探；探测在后台线程里做，
        结果通过 ``locatedFinished`` 回到 GUI 线程。
        """
        serial = str(getattr(self.config.device, "serial", "") or "").strip()
        if ":" not in serial:
            return
        if getattr(self._adb, "path", "") == "simulated-adb":
            return  # 假平板没有局域网，别去探
        if getattr(self, "_probe_thread", None) is not None:
            return
        mac = str(getattr(self.config.device, "mac", "") or "")
        thread = threading.Thread(
            target=self._probe_worker,
            args=(serial, mac),
            name="padlend-locate",
            daemon=True,
        )
        self._probe_thread = thread
        self.log_view.append_event("DEVICE", f"正在确认 {serial} 是否还在线…")
        thread.start()

    def _probe_worker(self, serial: str, mac: str) -> None:
        located = Located(detail="探测失败")
        try:
            services = getattr(self._adb, "mdns_services", None)
            located = locate(
                serial,
                mac,
                services=services if callable(services) else None,
            )
        except Exception as exc:  # noqa: BLE001 - 探测失败按离线处理
            log.info("locate failed: %r", exc)
            located = Located(detail=f"探测失败：{exc}")
        self.locatedFinished.emit(located)

    def _on_located(self, located: object) -> None:
        self._probe_thread = None
        if not isinstance(located, Located):
            return
        serial = str(getattr(self.config.device, "serial", "") or "")
        log.info("device.locate status=%s detail=%s", located.status, located.detail)
        if located.status == STATUS_MOVED and located.address:
            previous = serial
            self.device_page.set_located(previous, "moved", located.detail)
            self._remember_device(located.address)
            self.device_page.set_located(located.address, "online", "启动时在局域网里找到")
            self.log_view.append_event(
                "DEVICE", f"平板换了地址：{previous} → {located.address}"
            )
            return
        if not serial:
            return
        self.device_page.set_located(serial, located.status, located.detail)
        if located.status == STATUS_ONLINE:
            self.log_view.append_event("DEVICE", f"上次的设备仍在线：{serial}")
        elif located.detail:
            self.log_view.append_event("DEVICE", f"设备探测：{located.detail}")

    def _capture_mac(self, serial: str) -> None:
        """后台问一次平板 Wi-Fi 的 MAC（只在无线设备上问，失败无所谓）。"""
        if ":" not in serial or self._adb is None:
            return
        try:
            threading.Thread(
                target=self._mac_worker,
                args=(serial,),
                name="padlend-mac",
                daemon=True,
            ).start()
        except RuntimeError:
            return

    def _mac_worker(self, serial: str) -> None:
        try:
            mac = self._adb.wifi_mac(serial)
        except Exception as exc:  # noqa: BLE001 - 拿不到 MAC 不算错误
            log.info("wifi_mac failed: %r", exc)
            return
        if mac:
            self.macKnown.emit(mac)

    def _on_mac_known(self, mac: str) -> None:
        text = str(mac or "").strip().lower()
        if not text or text == str(getattr(self.config.device, "mac", "") or ""):
            return
        self.config.device.mac = text
        self._save_config()
        log.info("device.mac learned %s", text)

    # ------------------------------------------------------------------ 几何
    def center_on_screen(self) -> None:
        """把窗口摆到所在屏幕可用区域的中间（0.3.11：每次启动都居中）。"""
        screen = QGuiApplication.screenAt(self.frameGeometry().center())
        if screen is None:
            screen = QGuiApplication.primaryScreen()
        if screen is None:
            return
        area = screen.availableGeometry()
        frame = self.frameGeometry()
        frame.moveCenter(area.center())
        self.move(frame.topLeft())

    def _save_config(self) -> bool:
        problems = list(self.config.validate())
        if problems:
            log.warning("config invalid, not saved: %s", problems)
            self.log_view.append_event("CONFIG", "配置非法：" + "；".join(problems))
            return False
        try:
            self.config.save(self._config_path)
        except OSError as exc:
            log.error("config save failed: %r", exc)
            self.log_view.append_event("CONFIG", f"保存失败：{exc}")
            return False
        return True

    def set_device_info(
        self,
        serial: str,
        width: int | None = None,
        height: int | None = None,
    ) -> None:
        """外部（Router/app）更新顶栏设备信息。"""
        text = serial or "未连接设备"
        if width and height:
            text = f"{text} · {width}×{height}"
        self.device_text.setText(text)
        self.tray.set_device(serial)

    # ------------------------------------------------------------------ Qt 事件
    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if not self._dwm_applied:
            self._dwm_applied = True
            QTimer.singleShot(0, self._apply_dwm)
            self._canvas_call("refresh_desktop")

    def _apply_dwm(self) -> None:
        """按当前主题设置标题栏明暗 + 圆角（Win11）；Win10/非 Windows 静默忽略。"""
        self._apply_dwm_dark(tokens.current_theme() == "dark")

    def _apply_dwm_dark(self, dark: bool) -> None:
        if sys.platform != "win32":
            return
        if QGuiApplication.platformName() == "offscreen":
            # 离屏窗口没有真实 HWND，DWM 调用会崩。
            return
        try:
            hwnd = int(self.winId())
            if not hwnd:
                return
            dwm = ctypes.windll.dwmapi  # type: ignore[attr-defined]
            # 不声明 argtypes 时 ctypes 会把 64 位 HWND 截断成 32 位，
            # 在某些窗口句柄上会让 DWM 解引用非法指针并崩溃。
            dwm.DwmSetWindowAttribute.argtypes = [
                ctypes.c_void_p,
                ctypes.c_uint,
                ctypes.c_void_p,
                ctypes.c_uint,
            ]
            dwm.DwmSetWindowAttribute.restype = ctypes.c_long
            value = ctypes.c_int(1 if dark else 0)
            dwm.DwmSetWindowAttribute(
                ctypes.c_void_p(hwnd),
                C.DWM_USE_IMMERSIVE_DARK_MODE,
                ctypes.cast(ctypes.byref(value), ctypes.c_void_p),
                ctypes.sizeof(value),
            )
            corner = ctypes.c_int(C.DWMWCP_ROUND)
            dwm.DwmSetWindowAttribute(
                ctypes.c_void_p(hwnd),
                C.DWM_WINDOW_CORNER_PREFERENCE,
                ctypes.cast(ctypes.byref(corner), ctypes.c_void_p),
                ctypes.sizeof(corner),
            )
        except Exception as exc:  # noqa: BLE001 - Win10 或无 dwmapi 时静默
            log.debug("DWM attributes unavailable: %r", exc)

    def closeEvent(self, event: QCloseEvent) -> None:
        # X 的行为由设置决定：默认完全退出；勾了“最小化到托盘”就藏起来继续跑。
        # 原生标题栏的 X 由 _NativeCloseFilter 先接住；这里覆盖程序化 close()。
        if self.request_close():
            event.ignore()
            return
        event.accept()

    def _close_modal_dialogs(self) -> None:
        app = QApplication.instance()
        if app is None:
            return
        for widget in app.topLevelWidgets():
            if widget is self or not isinstance(widget, QDialog):
                continue
            try:
                widget.reject()
            except Exception as exc:  # noqa: BLE001 - 对话框可能已销毁
                log.debug("dialog reject failed: %r", exc)
