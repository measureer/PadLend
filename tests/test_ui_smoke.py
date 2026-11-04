"""主窗体外壳冒烟测试（offscreen）。

只依赖注入的 Router / DesktopMap 协议，不 import win32 或具体实现，
也不启动任何外部进程（adb 相关路径全部被替换或未触发）。
"""

from __future__ import annotations

import os
import zipfile
from pathlib import Path
from typing import Any

import pytest
from PySide6.QtCore import QSize
from PySide6.QtGui import QKeySequence
from PySide6.QtWidgets import QApplication, QLabel, QWizardPage

from config import Config
from core.geometry import Edge, EdgeSegment, MonitorInfo, Placement, Rect
from core.state import State
from ui import main_window as mw
from ui.diagnostics import (
    CommandResult,
    build_report,
    export_report,
    parse_devices,
    parse_pointer_speed,
    parse_uhid_group,
    parse_uhid_open,
    parse_xscale,
    static_summary,
)
from ui.log_view import LogView, format_stamp, parse_line
from ui.style import apply_theme
from ui.theme import tokens
from ui.tray import status_icon
from ui.wizard import SetupWizard

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")


# ---------------------------------------------------------------------------
# 测试替身：只实现被注入对象的公开协议
# ---------------------------------------------------------------------------
class FakeRouter:
    """Router 的公开协议子集（8.2），记录所有调用。"""

    def __init__(self) -> None:
        self.on_state_changed: Any = None
        self.on_metrics: Any = None
        self.calls: list[tuple[str, Any]] = []

    def start(self) -> None:
        self.calls.append(("start", None))

    def stop(self) -> None:
        self.calls.append(("stop", None))

    def on_raw(self, data: Any) -> None:
        self.calls.append(("on_raw", data))

    def on_key(self, event: Any) -> None:
        self.calls.append(("on_key", event))

    def toggle_manual(self) -> None:
        self.calls.append(("toggle_manual", None))

    def set_placement(self, placement: Placement) -> None:
        self.calls.append(("set_placement", placement))

    def resync_pointer(self) -> None:
        self.calls.append(("resync_pointer", None))


class FakeDesktop:
    """DesktopMap 协议子集：monitors 属性 + exterior_segments()。"""

    def __init__(self) -> None:
        self._monitors: tuple[MonitorInfo, ...] = (
            MonitorInfo(
                handle=1,
                device="\\\\.\\DISPLAY1",
                rect=Rect(0, 0, 1920, 1080),
                dpi=96,
                primary=True,
            ),
            MonitorInfo(
                handle=2,
                device="\\\\.\\DISPLAY2",
                rect=Rect(1920, 0, 3840, 1080),
                dpi=96,
                primary=False,
            ),
        )
        self._segments: tuple[EdgeSegment, ...] = (
            EdgeSegment(1, Edge.RIGHT, 0, 1080),
            EdgeSegment(1, Edge.LEFT, 0, 1080),
            EdgeSegment(2, Edge.RIGHT, 0, 1080),
        )

    @property
    def monitors(self) -> tuple[MonitorInfo, ...]:
        return self._monitors

    def exterior_segments(self) -> tuple[EdgeSegment, ...]:
        return self._segments


@pytest.fixture(scope="session")
def qapp() -> QApplication:
    app = QApplication.instance()
    if app is None:
        app = QApplication([])
    if not isinstance(app, QApplication):
        raise TypeError("需要 QApplication")
    apply_theme(app)
    return app


@pytest.fixture()
def config_path(tmp_path: Path) -> Path:
    return tmp_path / "config.yaml"


@pytest.fixture()
def router() -> FakeRouter:
    return FakeRouter()


@pytest.fixture()
def window(qapp: QApplication, router: FakeRouter, config_path: Path) -> Any:
    win = mw.MainWindow(Config(), router, FakeDesktop(), config_path=config_path)
    yield win
    win._force_quit = True
    win.close()
    qapp.processEvents()
    win.deleteLater()
    qapp.processEvents()


# ---------------------------------------------------------------------------
# 主窗与画布接线
# ---------------------------------------------------------------------------
def test_window_shell_builds_offscreen(window: Any, qapp: QApplication) -> None:
    assert window.objectName() == "mainWindow"
    assert window.windowTitle() == "PadLend"
    for name in (
        "header",
        "wordmark",
        "status_dot",
        "status_text",
        "device_text",
        "orientation_text",
        "latency_text",
        "menu_button",
        "settings_button",
        "theme_button",
        "canvas",
        "settings",
        "log_view",
        "metrics_panel",
    ):
        assert hasattr(window, name), name
    # 方案 A：保留系统标题栏，顶栏不再重复放「—」「×」。
    assert not hasattr(window, "minimize_button")
    assert not hasattr(window, "close_button")
    assert window.canvas_mode in {"native", "placeholder"}
    assert isinstance(window.canvas, mw.QWidget)
    assert window.minimumSize().width() >= tokens.WINDOW_MIN_SIZE[0]
    assert window.settings_button.size() == QSize(28, 28)
    window.show()
    qapp.processEvents()
    assert window.isVisible() is True
    window.hide()


def test_window_survives_missing_canvas(
    qapp: QApplication, router: FakeRouter, config_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(mw, "DesktopCanvas", None)
    win = mw.MainWindow(Config(), router, FakeDesktop(), config_path=config_path)
    assert win.canvas_mode == "placeholder"
    assert isinstance(win.canvas, QLabel)
    assert win.canvas.objectName() == "canvasPlaceholder"
    win._force_quit = True
    win.close()
    win.deleteLater()


def test_canvas_signals_reach_window(window: Any, qapp: QApplication) -> None:
    segment = window.settings.edge_combo.itemData(0)
    assert isinstance(segment, EdgeSegment)
    placement = Placement(segment=segment, offset=0.25)
    window.canvas.placementEdited.emit(placement)
    qapp.processEvents()
    assert window._placement == placement
    window.canvas.placementCommitted.emit(placement)
    qapp.processEvents()
    assert window.config.placement.offset == pytest.approx(0.25)
    assert window.config.placement.edge == segment.edge.value


def test_canvas_programmatic_calls_are_guarded(window: Any) -> None:
    window._canvas_call("set_tablet_size", 2136, 3200)
    window._canvas_call("set_pc_cursor", 10.0, 20.0)
    window._canvas_call("set_tablet_pointer", 5.0, 6.0)
    window._canvas_call("does_not_exist", 1)
    assert window._canvas_call("missing_method") is None


# ---------------------------------------------------------------------------
# RouterBridge：线程回调 → Qt 信号
# ---------------------------------------------------------------------------
def test_bridge_forwards_state_and_metrics(
    window: Any, router: FakeRouter, qapp: QApplication
) -> None:
    assert window.bridge.attached is True
    router.on_state_changed(State.IDLE, State.CAPTURED, "smoke")
    router.on_metrics(
        {
            "report_hz": 998.0,
            "send_hz": 997.0,
            "backpressure": 0,
            "rtt_ms": 12.0,
            "p95_ms": 21.0,
            "loss_pct": 0.0,
            "cpu_pct": 2.1,
            "mem_mb": 84.0,
        }
    )
    qapp.processEvents()
    assert window._state == "captured"
    assert window.status_text.text() == tokens.state_label("captured")
    assert "998" in window.metrics_panel.rate_label.text()
    assert "12" in window.latency_text.text()
    assert "2.1" in window.metrics_panel.cpu_label.text()
    assert "captured" in window.log_view.text()


def test_bridge_detach_restores_callbacks(window: Any, router: FakeRouter) -> None:
    original = router.on_state_changed
    window.bridge.detach()
    assert window.bridge.attached is False
    assert router.on_state_changed is original or callable(router.on_state_changed)
    window.bridge.attach(router)
    assert window.bridge.attached is True


# ---------------------------------------------------------------------------
# 日志
# ---------------------------------------------------------------------------
def test_log_view_timestamp_event_filter(window: Any) -> None:
    window.log_view.append_line("01:12:03.114 ARM   外推 42px 命中右段")
    qapp_text = window.log_view.text()
    assert "01:12:03.114" in qapp_text
    assert "外推 42px" in qapp_text
    window.log_view.append_event("CAP", "开始接管")
    assert "CAP" in window.log_view.text()
    window.log_view.set_filter("CAP")
    assert "CAP" in window.log_view.visible_text()
    assert "命中右段" not in window.log_view.visible_text()
    window.log_view.set_filter("")
    assert "命中右段" in window.log_view.visible_text()


def test_log_view_line_cap(qapp: QApplication) -> None:
    view = LogView()
    view.setParent(None)
    for index in range(2100):
        view.append_event("TICK", f"line {index}")
    assert view.entry_count == view.max_lines == 2000
    assert view.view.blockCount() <= 2000 + 1
    view.deleteLater()


def test_log_line_parsing() -> None:
    stamp = format_stamp()
    assert len(stamp) == 12 and stamp.count(":") == 2 and "." in stamp
    entry = parse_line("01:12:03.114 ARM   外推 42px 命中右段")
    assert entry.timestamp == "01:12:03.114"
    assert entry.event == "ARM"
    assert "外推 42px" in entry.message
    plain = parse_line("没有时间戳的杂项行")
    assert plain.event == ""
    assert plain.message == "没有时间戳的杂项行"


# ---------------------------------------------------------------------------
# 设置面板
# ---------------------------------------------------------------------------
def test_settings_panel_populates_and_saves(window: Any, config_path: Path) -> None:
    panel = window.settings
    assert panel.edge_combo.count() == 3
    assert panel.align_combo.count() == 3
    placement = panel.current_placement()
    assert isinstance(placement, Placement)
    assert placement.offset == pytest.approx(window.config.placement.offset)

    assert panel.save() is True
    assert config_path.is_file()
    assert panel.issues_label.text().startswith("已保存")


def test_settings_panel_rejects_blank_hotkey(window: Any) -> None:
    panel = window.settings
    panel.toggle_edit.setKeySequence(QKeySequence(""))
    assert panel.save() is False
    assert "hotkeys.toggle" in panel.issues_label.text()
    assert panel.toggle_edit.property("invalid") is True


def test_settings_placement_signal_updates_canvas(window: Any, qapp: QApplication) -> None:
    panel = window.settings
    segment = panel.edge_combo.itemData(1)
    placement = Placement(segment=segment, offset=0.5)
    panel.placementChanged.emit(placement)
    qapp.processEvents()
    assert window._placement == placement
    assert window.canvas.placement == placement


def test_config_saved_signal_reaches_router(window: Any, router: FakeRouter) -> None:
    panel = window.settings
    panel.save()
    pushes = [call for call in router.calls if call[0] == "set_placement"]
    assert pushes, "保存后应把摆位推给 Router"


# ---------------------------------------------------------------------------
# 托盘与快捷键
# ---------------------------------------------------------------------------
def test_tray_menu_and_status_icons(window: Any) -> None:
    tray = window.tray
    labels = [action.text() for action in tray.menu.actions() if not action.isSeparator()]
    for wanted in ("手动接管", "重连", "打开配置…", "复制诊断信息", "退出 PadLend"):
        assert wanted in labels, labels
    assert tray.menu.actions()
    assert window.menu_button.isHidden() is tray.available
    for state in ("", "idle", "arming", "captured", "fault"):
        icon = status_icon(state)
        assert icon.isNull() is False
        assert icon.pixmap(QSize(32, 32)).isNull() is False
    tray.set_state("captured")
    assert "交还" in tray.toggle_action.text()
    tray.set_state("idle")
    assert "接管" in tray.toggle_action.text()


def test_shortcuts_and_menu_actions(window: Any) -> None:
    assert window.settings_shortcut.key() == QKeySequence("Ctrl+,")
    assert window.toggle_shortcut.key() == QKeySequence("Ctrl+Alt+S")
    labels = [action.text() for action in window.tray.menu.actions() if not action.isSeparator()]
    assert "运行诊断…" in labels
    assert "首次设置向导…" in labels


def test_window_actions_do_not_raise(window: Any, router: FakeRouter) -> None:
    window.toggle_manual()
    assert ("toggle_manual", None) in router.calls
    window.reconnect()
    assert ("resync_pointer", None) in router.calls
    window.panic_release()
    assert ("stop", None) in router.calls
    window.copy_diagnostics()
    window.set_device_info("R58M12345", 3200, 2136)
    assert "R58M12345" in window.device_text.text()


def test_close_goes_to_tray_only_when_available(window: Any, qapp: QApplication) -> None:
    window._force_quit = True
    window.close()
    qapp.processEvents()
    assert window.isVisible() is False


# ---------------------------------------------------------------------------
# 诊断
# ---------------------------------------------------------------------------
def test_diagnostics_parsers() -> None:
    devices = [
        "R58M12345\tdevice usb:1-1 product:p3 model:SM_P3",
        "* daemon not running; starting now at tcp:5037",
        "emulator-5554\toffline",
        "",
    ]
    entries = parse_devices("\n".join(devices))
    assert [entry.serial for entry in entries] == ["R58M12345", "emulator-5554"]
    assert entries[0].usable is True
    assert entries[1].usable is False
    assert parse_xscale("Cursor Input Mapper: XScale: 1.312") == pytest.approx(1.312)
    assert parse_xscale("没有 XScale") is None
    assert parse_pointer_speed("-1\n") == "-1"
    assert parse_pointer_speed("null") == ""
    assert parse_uhid_group("uid=2000 gid=2000 groups=1003,uhid,1000") is True
    assert parse_uhid_group("uid=2000 gid=2000 groups=1003,1000") is False
    assert parse_uhid_open("OPEN_OK") is True
    assert parse_uhid_open("/system/bin/sh: /dev/uhid: Permission denied") is False


def test_diagnostics_report_and_export(tmp_path: Path) -> None:
    results = [
        CommandResult("adb devices -l", 0, "R58M12345\tdevice usb:1-1", ""),
        CommandResult(
            "dumpsys input",
            0,
            "Cursor Input Mapper:\n  XScale: 1.312\n  YScale: 1.312\n",
            "",
        ),
        CommandResult("pointer_speed", 0, "0\n", ""),
        CommandResult("uhid 组", 0, "uid=2000 gid=2000 groups=uhid", ""),
        CommandResult("uhid 可写", 0, "OPEN_OK", ""),
    ]
    report = build_report(results, server_jar=tmp_path / "missing.jar")
    assert report.primary_device() is not None
    assert report.xscale == pytest.approx(1.312)
    assert report.uhid_group is True
    assert report.uhid_open is True
    text = report.to_text()
    assert "PadLend" in text
    assert "1.312" in text
    assert "R58M12345" in text

    archive = export_report(report, directory=tmp_path)
    assert archive.is_file()
    with zipfile.ZipFile(archive) as handle:
        names = set(handle.namelist())
    assert {"diagnostics.txt", "dumpsys-input.txt", "adb-devices.txt", "meta.json"} <= names


def test_static_summary_does_not_need_adb() -> None:
    text = static_summary()
    assert "PadLend" in text


# ---------------------------------------------------------------------------
# 向导
# ---------------------------------------------------------------------------
def test_wizard_has_five_idle_pages(qapp: QApplication) -> None:
    wizard = SetupWizard()
    pages = wizard.pages_list
    assert len(pages) == 5
    assert all(isinstance(page, QWizardPage) for page in pages)
    assert [page.title() for page in pages] == [
        "1 / 5 · 安装 adb",
        "2 / 5 · 开启 USB 调试",
        "3 / 5 · 检查 UHID",
        "4 / 5 · scrcpy server",
        "5 / 5 · 完成",
    ]
    assert wizard.adb_page is pages[0]
    assert wizard.finish_page is pages[4]
    for page in wizard.check_pages():
        assert page._chain is None
    wizard.stop_all()
    wizard.deleteLater()


# ---------------------------------------------------------------------------
# 设备页：未连接不显示任何具体分辨率
# ---------------------------------------------------------------------------
def test_device_page_hides_resolution_until_facts(qapp: QApplication) -> None:
    from tablet.session import DeviceFacts
    from ui.device_page import DevicePage

    page = DevicePage()
    try:
        page.set_facts(None)
        assert page._values["resolution"].text() == "—"
        assert page._values["orientation"].text() == "—"
        texts = [label.text() for label in page.findChildren(QLabel)]
        assert not any("3200" in text or "2136" in text for text in texts)

        page.set_facts(
            DeviceFacts(
                serial="serial-1",
                model="HONOR CGA-W00",
                width=3200,
                height=2136,
                orientation=1,
                xscale=1.312,
                pointer_speed=-3,
            )
        )
        assert page._values["resolution"].text() == "3200 × 2136"
        assert page._values["orientation"].text() == "横屏"
    finally:
        page.deleteLater()
        qapp.processEvents()


def test_main_window_device_header_has_no_default_resolution(
    window: Any, qapp: QApplication
) -> None:
    from tablet.session import DeviceFacts

    window.update_device(None)
    assert "3200" not in window.device_text.text()
    assert "2136" not in window.orientation_text.text()
    assert window.orientation_text.text() == "—"

    window.update_device(
        DeviceFacts(
            serial="serial-1",
            model="Pad",
            width=2560,
            height=1600,
            orientation=1,
            xscale=1.0,
            pointer_speed=0,
        )
    )
    assert "2560" in window.orientation_text.text()
    assert window._orientation == "横屏"


def test_theme_toggle_persists_and_repaints(window: Any, qapp: QApplication) -> None:
    """主题按钮切到白天、保存到 config，再切回黑夜。"""
    tokens.set_theme("dark")
    assert tokens.current_theme() == "dark"
    assert window.config.ui.theme == "dark"

    window.theme_button.click()
    assert tokens.current_theme() == "light"
    assert window.config.ui.theme == "light"
    assert Config.load(window._config_path).ui.theme == "light"
    assert window.device_page._values["resolution"].text() == "—"
    assert "3200" not in window.device_page._values["resolution"].text()

    window.theme_button.click()
    assert tokens.current_theme() == "dark"
    assert Config.load(window._config_path).ui.theme == "dark"



def test_settings_hints_and_exit_threshold(window: Any) -> None:
    """每个设置分组都有说明；出口阈值可直接调整并落盘。"""
    window._switch_page(2)
    hints = window.settings.findChildren(QLabel, "fieldHint")
    assert len(hints) >= 13
    assert any("交还 PC" in hint.text() for hint in hints)
    assert any("反向移动" in hint.text() for hint in hints)

    window.settings.exit_slider.setValue(36)
    assert window.settings.save() is True
    assert window.settings._config.motion.exit_overflow_px == 36


# ---------------------------------------------------------------------------
# 添加无线设备对话框
# ---------------------------------------------------------------------------
def test_add_device_validation_helpers() -> None:
    from ui.add_device_dialog import validate_address, validate_pairing_code

    assert validate_address("192.168.1.8:5555") == ("192.168.1.8:5555", "")
    assert validate_address("192.168.1.8") == ("192.168.1.8:5555", "")
    assert validate_address("")[1]
    assert validate_address("1.2.3.4:99999")[1]
    assert validate_pairing_code("") == ("", "")
    assert validate_pairing_code("12345")[1]
    assert validate_pairing_code("123456") == ("123456", "")


def test_add_device_dialog_prompts_and_button_label(qapp: QApplication) -> None:
    from PySide6.QtWidgets import QLabel

    from ui.add_device_dialog import AddDeviceDialog

    dialog = AddDeviceDialog(prefill="192.168.1.8")
    try:
        assert dialog.address_edit.text() == "192.168.1.8"
        # 说明文字必须是真换行，不能出现字面量 "\n"。
        for label in dialog.findChildren(QLabel):
            assert (chr(92) + "n") not in label.text()
        assert dialog.connect_button.text() == "连接"
        dialog.code_edit.setText("123456")
        assert dialog.connect_button.text() == "配对并连接"
        dialog.code_edit.setText("")
        assert dialog.connect_button.text() == "连接"
    finally:
        dialog.deleteLater()
