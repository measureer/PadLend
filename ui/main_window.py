"""主窗口外壳（设计文档 6.10.1 / 6.10.4 / 6.10.7）。

结构::

    ┌──────────────────────────── 顶栏 ────────────────────────────┐
    │ PadLend ● 状态  设备 · 朝向 · 延迟        [⚙] [—] [×]        │
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
from pathlib import Path
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import QSize, Qt, QTimer, Signal
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
    QFrame,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QPushButton,
    QSplitter,
    QVBoxLayout,
    QWidget,
)

import constants as C
from ui.bridge import RouterBridge, state_value
from ui.diagnostics import DiagnosticsDialog, static_summary
from ui.log_view import LogView
from ui.settings_panel import SettingsPanel
from ui.style import repolish
from ui.theme import tokens
from ui.tray import TrayController, status_icon
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


class MainWindow(QMainWindow):
    """PadLend 主窗；只做外壳与接线，业务在 Router/DesktopMap。"""

    placementChanged = Signal(object)
    settingsSaved = Signal(object)

    def __init__(
        self,
        config: Config,
        router: object,
        desktop: object,
        *,
        config_path: Path | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("mainWindow")
        self.setWindowTitle(C.APP_NAME)
        self.setWindowIcon(status_icon("idle"))
        self.resize(*tokens.WINDOW_DEFAULT_SIZE)
        self.setMinimumSize(*tokens.WINDOW_MIN_SIZE)

        self.config = config
        self.router = router
        self.desktop = desktop
        self._config_path = Path(config_path) if config_path is not None else C.config_path()
        self._state = ""
        self._orientation = "横屏"
        self._pointer_pos: tuple[float, float] = (0.0, 0.0)
        self._placement: object | None = None
        self._force_quit = False
        self._dwm_applied = False
        self._hidden_notice_shown = False
        self._wizard: SetupWizard | None = None
        self._diagnostics: DiagnosticsDialog | None = None

        self._build_ui()
        self._build_tray()
        self._build_bridge()
        self._build_shortcuts()
        self._build_actions()
        self._connect_canvas()
        self._refresh_header_device()
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
        self.settings.setMinimumWidth(tokens.SETTINGS_PANEL_WIDTH)
        self.settings.setMaximumWidth(tokens.SETTINGS_PANEL_WIDTH + 80)
        self.canvas, self.canvas_mode = self._create_canvas()

        self.main_split = QSplitter(Qt.Orientation.Horizontal, root)
        self.main_split.setObjectName("mainSplit")
        self.main_split.setChildrenCollapsible(False)
        self.main_split.addWidget(self.canvas)
        self.main_split.addWidget(self.settings)
        self.main_split.setStretchFactor(0, 1)
        self.main_split.setStretchFactor(1, 0)
        self.main_split.setSizes([860, tokens.SETTINGS_PANEL_WIDTH])

        self.log_view = LogView(root)
        self.metrics_panel = MetricsPanel(root)
        self.bottom_split = QSplitter(Qt.Orientation.Horizontal, root)
        self.bottom_split.setObjectName("bottomSplit")
        self.bottom_split.setChildrenCollapsible(False)
        self.bottom_split.addWidget(self.log_view)
        self.bottom_split.addWidget(self.metrics_panel)
        self.bottom_split.setStretchFactor(0, 1)
        self.bottom_split.setStretchFactor(1, 0)
        self.bottom_split.setSizes([800, _METRICS_WIDTH])

        self.vertical_split = QSplitter(Qt.Orientation.Vertical, root)
        self.vertical_split.setObjectName("verticalSplit")
        self.vertical_split.setChildrenCollapsible(False)
        self.vertical_split.addWidget(self.main_split)
        self.vertical_split.addWidget(self.bottom_split)
        self.vertical_split.setStretchFactor(0, 1)
        self.vertical_split.setStretchFactor(1, 0)
        self.vertical_split.setSizes([540, 220])
        layout.addWidget(self.vertical_split, 1)

        self.settings.placementChanged.connect(self._on_settings_placement)
        self.settings.configChanged.connect(self._on_config_saved)

    def _build_header(self) -> QFrame:
        header = QFrame(self)
        header.setObjectName("header")
        header.setFixedHeight(44)

        self.wordmark = QLabel(C.APP_NAME, header)
        self.wordmark.setObjectName("wordmark")

        self.status_dot = QLabel(header)
        self.status_dot.setObjectName("statusDot")
        size = tokens.STATUS_DOT_SIZE
        self.status_dot.setFixedSize(QSize(size, size))
        self._paint_status_dot("")

        self.status_text = QLabel(tokens.state_label(""), header)
        self.status_text.setObjectName("statusText")
        self.device_text = QLabel("未连接设备", header)
        self.device_text.setObjectName("deviceText")
        self.orientation_text = QLabel(self._orientation, header)
        self.orientation_text.setObjectName("deviceText")
        self.latency_text = QLabel("RTT —", header)
        self.latency_text.setObjectName("latencyText")

        self.menu_button = QPushButton("☰", header)
        self.settings_button = QPushButton("⚙", header)
        self.minimize_button = QPushButton("—", header)
        self.close_button = QPushButton("×", header)
        for button in (self.menu_button, self.settings_button, self.minimize_button):
            button.setObjectName("iconButton")
            button.setFixedSize(QSize(28, 28))
            button.setCursor(Qt.CursorShape.PointingHandCursor)
        self.close_button.setObjectName("closeButton")
        self.close_button.setFixedSize(QSize(28, 28))
        self.close_button.setCursor(Qt.CursorShape.PointingHandCursor)
        self.menu_button.setToolTip("菜单")
        self.settings_button.setToolTip("设置 (Ctrl+,)")
        self.minimize_button.setToolTip("最小化")
        self.close_button.setToolTip("关闭到托盘")

        self.settings_button.clicked.connect(self.open_settings)
        self.minimize_button.clicked.connect(self.showMinimized)
        self.close_button.clicked.connect(self.close)
        self.menu_button.clicked.connect(self._popup_menu)

        layout = QHBoxLayout(header)
        layout.setContentsMargins(16, 0, 12, 0)
        layout.setSpacing(8)
        layout.addWidget(self.wordmark)
        layout.addSpacing(8)
        layout.addWidget(self.status_dot)
        layout.addWidget(self.status_text)
        layout.addSpacing(12)
        layout.addWidget(self.device_text)
        layout.addWidget(self.orientation_text)
        layout.addWidget(self.latency_text)
        layout.addStretch(1)
        layout.addWidget(self.menu_button)
        layout.addWidget(self.settings_button)
        layout.addWidget(self.minimize_button)
        layout.addWidget(self.close_button)
        self.header = header
        return header

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
    def _on_state(self, old: object, new: object, reason: object) -> None:
        old_value = state_value(old)
        new_value = state_value(new)
        self._state = new_value
        self._paint_status_dot(new_value)
        self.status_text.setText(tokens.state_label(new_value))
        self.status_text.setProperty("state", new_value)
        repolish(self.status_text)
        self.tray.set_state(new_value)
        self.tray.set_connected(new_value != "")
        text = f"{old_value or '-'} → {new_value or '未连接'}"
        if reason:
            text = f"{text} {reason}"
        self.log_view.append_event("STATE", text)

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
        """Ctrl+, / ⚙：展开设置面板并聚焦。"""
        self.settings.setVisible(True)
        sizes = self.main_split.sizes()
        if len(sizes) == 2 and sizes[1] == 0:
            self.main_split.setSizes([max(560, sizes[0] - 340), 340])
        self.settings.focus_first()

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
        self._force_quit = True
        self._shutdown()
        self.close()
        app = QApplication.instance()
        if app is not None:
            app.quit()

    def _shutdown(self) -> None:
        try:
            self.bridge.detach()
        except Exception as exc:
            log.warning("bridge.detach failed: %r", exc)
        self.tray.dispose()
        if self._wizard is not None:
            self._wizard.stop_all()

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
        serial = str(getattr(getattr(self.config, "device", None), "serial", "") or "")
        self.device_text.setText(serial or "未连接设备")
        self.orientation_text.setText(self._orientation)
        self.tray.set_device(serial)

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
        """暗色标题栏 + 圆角（Win11）；Win10/非 Windows 失败即忽略。"""
        if sys.platform != "win32":
            return
        try:
            hwnd = int(self.winId())
            dwm = ctypes.windll.dwmapi  # type: ignore[attr-defined]
            dark = ctypes.c_int(1)
            dwm.DwmSetWindowAttribute(
                hwnd,
                C.DWM_USE_IMMERSIVE_DARK_MODE,
                ctypes.byref(dark),
                ctypes.sizeof(dark),
            )
            corner = ctypes.c_int(C.DWMWCP_ROUND)
            dwm.DwmSetWindowAttribute(
                hwnd,
                C.DWM_WINDOW_CORNER_PREFERENCE,
                ctypes.byref(corner),
                ctypes.sizeof(corner),
            )
        except Exception as exc:  # noqa: BLE001 - Win10 或无 dwmapi 时静默
            log.debug("DWM attributes unavailable: %r", exc)

    def closeEvent(self, event: QCloseEvent) -> None:
        if self._force_quit:
            self._shutdown()
            event.accept()
            return
        if self.tray.available:
            event.ignore()
            self.hide()
            if not self._hidden_notice_shown:
                self._hidden_notice_shown = True
                self.tray.notify(C.APP_NAME, "已最小化到托盘；退出请用托盘菜单。")
            return
        self._shutdown()
        event.accept()
        app = QApplication.instance()
        if app is not None:
            app.quit()
