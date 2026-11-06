"""主窗体外壳冒烟测试（offscreen）。

只依赖注入的 Router / DesktopMap 协议，不 import win32 或具体实现，
也不启动任何外部进程（adb 相关路径全部被替换或未触发）。
"""

from __future__ import annotations

import os
import zipfile
from dataclasses import replace
from pathlib import Path
from typing import Any

import pytest
import yaml
from PySide6.QtCore import QSize
from PySide6.QtGui import QGuiApplication, QKeySequence
from PySide6.QtWidgets import QApplication, QLabel, QWizardPage

import constants as C
from config import Config
from core.geometry import Edge, EdgeSegment, MonitorInfo, Placement, Rect
from core.state import State
from tablet.session import DeviceFacts
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
        self.on_orientation_changed: Any = None
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

    def inject_clipboard(self, text: str) -> bool:
        self.calls.append(("clipboard", text))
        return True


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

    @property
    def bounding_box(self) -> Rect:
        return Rect(0, 0, 3840, 1080)

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


def test_present_window_restores_hidden_window(window: Any, qapp: QApplication) -> None:
    window.show()
    qapp.processEvents()
    window.hide()
    qapp.processEvents()
    assert window.isVisible() is False
    window.presentRequested.emit()
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


def _facts(
    width: int = 2560,
    height: int = 1600,
    orientation: int = 1,
    hardware_serial: str = "HW-0001",
) -> DeviceFacts:
    return DeviceFacts(
        serial="192.0.2.10:5555",
        model="CGA-W00",
        width=width,
        height=height,
        orientation=orientation,
        xscale=1.0,
        pointer_speed=0,
        hardware_serial=hardware_serial,
    )


def test_save_layout_records_profile_for_device_and_orientation(
    window: Any, qapp: QApplication
) -> None:
    window.update_device(_facts())
    qapp.processEvents()
    assert window._placement is not None
    window._placement = Placement(segment=window._placement.segment, offset=0.25)
    window._save_layout()
    qapp.processEvents()

    profile = window.config.layouts.get("HW-0001", "landscape")
    assert profile is not None
    assert profile.offset == pytest.approx(0.25)
    assert window.config.placement.offset == pytest.approx(0.25)


def test_rotating_device_switches_to_its_saved_profile(
    window: Any, qapp: QApplication
) -> None:
    landscape = _facts()
    window.update_device(landscape)
    qapp.processEvents()
    assert window._placement is not None
    saved = Placement(segment=window._placement.segment, offset=0.25)
    window._placement = saved
    window._save_layout()
    qapp.processEvents()

    portrait_profile = type(window.config.placement)(edge="left", offset=0.75)
    window.config.layouts.set("HW-0001", "portrait", portrait_profile)
    window.update_device(replace(landscape, width=1600, height=2560, orientation=0))
    qapp.processEvents()

    assert window._orientation == "竖屏"
    assert window._placement is not None
    assert window._placement.offset == pytest.approx(0.75)
    assert window._layout_dirty is False


def test_rotating_without_a_profile_keeps_the_current_placement(
    window: Any, qapp: QApplication
) -> None:
    landscape = _facts()
    window.update_device(landscape)
    qapp.processEvents()
    assert window._placement is not None
    before = window._placement

    window.update_device(replace(landscape, width=1600, height=2560, orientation=0))
    qapp.processEvents()
    assert window._placement == before


def test_profile_with_missing_monitor_is_retried(
    window: Any, qapp: QApplication
) -> None:
    landscape = _facts()
    window.update_device(landscape)
    qapp.processEvents()
    assert window._placement is not None
    before = window._placement

    missing = type(window.config.placement)(
        monitor="\\\\.\\DISPLAY9", edge="left", offset=0.75
    )
    portrait = replace(landscape, width=1600, height=2560, orientation=0)
    window.config.layouts.set("HW-0001", "portrait", missing)
    window.update_device(portrait)
    qapp.processEvents()
    assert window._placement == before

    usable = type(window.config.placement)(
        monitor="\\\\.\\DISPLAY2", edge="right", offset=0.75
    )
    window.config.layouts.set("HW-0001", "portrait", usable)
    window.update_device(portrait)
    qapp.processEvents()
    assert window._placement is not None
    assert window._placement.offset == pytest.approx(0.75)


def test_switching_device_without_a_profile_falls_back_to_global_layout(
    window: Any, qapp: QApplication
) -> None:
    window.update_device(_facts())
    qapp.processEvents()
    assert window._placement is not None
    before = window._placement
    window._placement = Placement(segment=before.segment, offset=0.9)

    window.update_device(_facts(2560, 1600, 1, hardware_serial="HW-0002"))
    qapp.processEvents()
    assert window._placement is not None
    assert window._placement.offset == pytest.approx(window.config.placement.offset)


def test_canvas_signals_reach_window(window: Any, qapp: QApplication) -> None:
    placement = window._placement
    assert placement is not None
    preview = Placement(segment=placement.segment, offset=0.25)
    window.canvas.placementEdited.emit(preview)
    qapp.processEvents()
    assert window._placement == preview
    assert window._layout_dirty is True
    assert window.layout_status.isHidden() is False

    window.canvas.placementCommitted.emit(preview)
    qapp.processEvents()
    assert window.config.placement.offset == pytest.approx(0.5)
    window._save_layout()
    qapp.processEvents()
    assert window.config.placement.offset == pytest.approx(0.25)
    assert window._layout_dirty is False
    assert window.layout_status.isHidden() is True


def test_canvas_programmatic_calls_are_guarded(window: Any) -> None:
    window._canvas_call("set_tablet_size", 2136, 3200)
    window._canvas_call("set_pc_cursor", 10.0, 20.0)
    window._canvas_call("set_tablet_pointer", 5.0, 6.0)
    window._canvas_call("does_not_exist", 1)
    assert window._canvas_call("missing_method") is None


def test_canvas_fits_tablet_that_sits_outside_the_desktop(
    window: Any, qapp: QApplication
) -> None:
    """平板在桌面包围盒之外时，适配视图不能把它裁掉。"""
    window.show()
    for _ in range(3):
        qapp.processEvents()
    canvas = window.canvas
    canvas.refresh_desktop()
    window._switch_page(1)  # noqa: SLF001
    canvas.fit_view()
    qapp.processEvents()
    tablet = canvas._tablet_item.sceneBoundingRect()  # noqa: SLF001
    assert tablet.width() > 0
    assert canvas._scene.sceneRect().contains(tablet)  # noqa: SLF001
    view_rect = canvas.mapFromScene(tablet).boundingRect()
    assert canvas.viewport().rect().adjusted(-1, -1, 1, 1).contains(view_rect)


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


def test_entering_capture_syncs_the_clipboard_when_enabled(
    window: Any, router: FakeRouter, qapp: QApplication
) -> None:
    """勾选「剪贴板同步」后，进入接管会自动同步一次（0.3.14）。

    走 ``SET_CLIPBOARD``（不敲键盘），所以不会再触发平板 App 的快捷键。
    """
    QGuiApplication.clipboard().setText("hello tablet")
    window.config.misc.clipboard_sync = True
    window._on_state(State.IDLE, State.CAPTURED, "smoke")
    assert window._clipboard_timer.isActive()
    window._clipboard_timer.timeout.emit()
    assert ("clipboard", "hello tablet") in router.calls


def test_entering_capture_does_not_queue_clipboard_when_switched_off(
    window: Any, router: FakeRouter
) -> None:
    window.config.misc.clipboard_sync = False
    window._on_state(State.IDLE, State.CAPTURED, "smoke")
    assert window._clipboard_timer.isActive() is False
    assert not [call for call in router.calls if call[0] == "clipboard"]


def test_clipboard_changes_only_sync_while_captured(window: Any) -> None:
    window.config.misc.clipboard_sync = True
    window._state = "idle"
    window._on_clipboard_changed()
    assert window._clipboard_timer.isActive() is False
    window._state = "captured"
    window._on_clipboard_changed()
    assert window._clipboard_timer.isActive() is True
    window._clipboard_timer.stop()


def test_bridge_forwards_orientation(
    window: Any, router: FakeRouter, qapp: QApplication
) -> None:
    facts = DeviceFacts(
        serial="FAKE0001",
        model="FakePad",
        width=2560,
        height=1600,
        orientation=1,
        xscale=1.0,
        pointer_speed=0,
        hardware_serial="HW-0001",
    )
    router.on_orientation_changed(facts)
    qapp.processEvents()
    assert window._facts is facts
    assert window._orientation == "横屏"


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
    assert not hasattr(panel, "edge_combo")
    assert not hasattr(panel, "align_combo")
    assert not hasattr(panel, "placementChanged")

    assert panel.save() is True
    assert config_path.is_file()
    assert panel.issues_label.text().startswith("已保存")


def test_settings_panel_rejects_blank_hotkey(window: Any) -> None:
    panel = window.settings
    panel.toggle_edit.setKeySequence(QKeySequence(""))
    assert panel.save() is False
    assert "hotkeys.toggle" in panel.issues_label.text()
    assert panel.toggle_edit.property("invalid") is True


def test_settings_save_does_not_touch_layout(window: Any) -> None:
    before = window._placement
    assert window.settings.save() is True
    assert window._placement == before
    assert window._layout_dirty is False


def test_settings_save_does_not_push_placement_but_layout_save_does(
    window: Any, router: FakeRouter
) -> None:
    def pushes() -> int:
        return sum(1 for call in router.calls if call[0] == "set_placement")

    before = pushes()
    assert window.settings.save() is True
    assert pushes() == before, "保存设置不应再改布局"
    window._save_layout()
    assert pushes() == before + 1


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


def test_tray_icon_carries_the_brand_and_the_state(qapp) -> None:
    """0.3.16：托盘图标是品牌标 + 状态点，不是一颗匿名圆点。"""
    def pixels(state: str, size: int) -> bytes:
        image = status_icon(state).pixmap(QSize(size, size)).toImage()
        return bytes(image.constBits())

    assert pixels("idle", 24) != pixels("captured", 24), "状态点没有落在图标上"
    small = status_icon("captured").pixmap(QSize(16, 16))
    assert not small.isNull()
    assert any(pixels("captured", 16)), "16px 图标是空的"


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
    # 序列号在导出的诊断文本里必须打码（发版审计 P0-3）。
    assert "R58M12345" not in text
    assert "<serial>" in text

    archive = export_report(report, directory=tmp_path)
    assert archive.is_file()
    with zipfile.ZipFile(archive) as handle:
        names = set(handle.namelist())
    expected = {
        "README.txt",
        "diagnostics.txt",
        "dumpsys-input.txt",
        "adb-devices.txt",
        "meta.json",
    }
    assert expected <= names


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
    assert any("反向拖回" in hint.text() for hint in hints)

    window.settings.exit_slider.setValue(36)
    assert window.settings.save() is True
    assert window.settings._config.motion.exit_overflow_px == 36


def test_report_hz_is_four_fixed_choices(window: Any) -> None:
    """0.3.16：上报频率从滑条改成 125/250/500/1000 四个固定档。"""
    combo = window.settings.hz_combo
    assert [combo.itemData(i) for i in range(combo.count())] == [125, 250, 500, 1000]

    window._switch_page(2)
    combo.setCurrentIndex(combo.findData(250))
    assert window.settings.save() is True
    assert window.settings._config.motion.report_hz == 250

    window.settings._fill_motion()
    assert combo.currentData() == 250


def test_exit_threshold_default_is_120() -> None:
    """0.3.16：穿回阈值默认从 24 提到 120 px。"""
    assert C.EXIT_OVERFLOW_PX == 120
    assert Config().motion.exit_overflow_px == 120


def test_arm_threshold_default_is_10_and_slider_starts_at_1(window: Any) -> None:
    """触发阈值默认 10 px；滑条允许调到 1 px 试验。"""
    assert C.ARM_PX == 10
    assert Config().motion.arm_px == 10
    slider = window.settings.arm_slider
    assert slider.minimum() == 1
    assert slider.maximum() == 200
    assert slider.value() == 10

    slider.setValue(1)
    assert window.settings.save() is True
    assert window.settings._config.motion.arm_px == 1


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


# ---------------------------------------------------------------------------
# 0.3.11：设备记忆、启动居中、打包图标
# ---------------------------------------------------------------------------
def test_device_page_lists_remembered_devices(qapp: QApplication) -> None:
    """扫不到的设备也要出现一行“上次连接的设备”，但不能和真扫到的重复。"""
    from PySide6.QtCore import Qt

    from tablet.adb import DeviceEntry
    from ui.device_page import DevicePage

    page = DevicePage(None, None)
    page.set_known(["192.168.1.9:5555", "ABC123"])
    page._show_entries([], "")  # noqa: SLF001 - 直接验证渲染
    qapp.processEvents()
    assert page.device_list.count() == 2
    serials = [
        page.device_list.item(i).data(Qt.ItemDataRole.UserRole) for i in range(2)
    ]
    assert serials == ["192.168.1.9:5555", "ABC123"]
    assert "已记住" in page.scan_hint.text()

    entry = DeviceEntry(serial="192.168.1.9:5555", state="device", model="Pad")
    page._show_entries([entry], "")  # noqa: SLF001
    qapp.processEvents()
    assert page.device_list.count() == 2, "真设备与记忆行不能重复"
    assert page.scan_hint.text() == "1 台设备"
    page.deleteLater()
    qapp.processEvents()


def test_window_remembers_the_last_device(window: Any) -> None:
    window.config.device.serial = ""
    window.config.device.known = []
    window._remember_device("192.168.1.9:5555")  # noqa: SLF001
    window._remember_device("ABC123")  # noqa: SLF001
    assert window.config.device.known == ["ABC123", "192.168.1.9:5555"]
    assert window.config.device.serial == "ABC123"
    assert window._known_devices() == ["ABC123", "192.168.1.9:5555"]  # noqa: SLF001

    window._remember_device("192.168.1.9:5555")  # noqa: SLF001
    assert window.config.device.known[0] == "192.168.1.9:5555"
    assert window.config.device.known.count("192.168.1.9:5555") == 1
    assert len(window.config.device.known) <= 8


def test_remember_device_keeps_a_mac_per_device(window: Any) -> None:
    """每台无线设备各记各的 MAC，不能互相覆盖。"""
    first = "192.0.2.10:37495"
    second = "192.0.2.11:5555"
    window.config.device.serial = ""
    window.config.device.known = []
    window.config.device.macs = {}

    window._remember_device(first, mac="AA-BB-CC-DD-EE-01")  # noqa: SLF001
    window._remember_device(second, mac="aa:bb:cc:dd:ee:02")  # noqa: SLF001

    assert window.config.device.macs == {
        first: "aa:bb:cc:dd:ee:01",
        second: "aa:bb:cc:dd:ee:02",
    }
    assert window.config.device.serial == second
    assert window._known_devices() == [second, first]  # noqa: SLF001


def test_forget_device_drops_only_its_mac(window: Any) -> None:
    first = "192.0.2.10:37495"
    second = "192.0.2.11:5555"
    window.config.device.serial = first
    window.config.device.known = [first, second]
    window.config.device.macs = {
        first: "aa:bb:cc:dd:ee:01",
        second: "aa:bb:cc:dd:ee:02",
    }

    window._forget_device(first)  # noqa: SLF001

    assert first not in window.config.device.macs
    assert window.config.device.macs[second] == "aa:bb:cc:dd:ee:02"
    assert window.config.device.serial == second


def test_mac_known_updates_the_matching_serial(window: Any) -> None:
    first = "192.0.2.10:37495"
    second = "192.0.2.11:5555"
    window.config.device.known = [first, second]
    window.config.device.macs = {first: "aa:bb:cc:dd:ee:01"}

    window._on_mac_known(second, "AA-BB-CC-DD-EE-02")  # noqa: SLF001

    assert window.config.device.macs == {
        first: "aa:bb:cc:dd:ee:01",
        second: "aa:bb:cc:dd:ee:02",
    }
    window._forget_device(second)  # noqa: SLF001
    window._on_mac_known(second, "AA-BB-CC-DD-EE-03")  # noqa: SLF001
    assert second not in window.config.device.macs


def test_device_page_refresh_button_requests_a_lan_lookup(qapp: QApplication) -> None:
    from ui.device_page import DevicePage

    page = DevicePage(None, None)
    calls: list[int] = []
    page.refreshRequested.connect(lambda: calls.append(1))
    try:
        page.refresh_button.click()
        qapp.processEvents()
        assert calls == [1]
    finally:
        page.deleteLater()
        qapp.processEvents()


def test_startup_probe_uses_the_arp_table(
    window: Any, qapp: QApplication, monkeypatch: pytest.MonkeyPatch
) -> None:
    """启动探测也要把系统 ARP 表交给 locate，否则换 IP 的第三步永远走不到。"""
    from tablet.locator import STATUS_OFFLINE, Located

    address = "192.0.2.10:37495"
    window.config.device.serial = address
    seen: list[Any] = []

    def fake_locate(host: str, mac: str = "", **kwargs: Any) -> Located:
        seen.append(kwargs.get("arp"))
        return Located(status=STATUS_OFFLINE, detail="不在线")

    monkeypatch.setattr(mw, "locate", fake_locate)
    window._probe_worker(address, "aa:bb:cc:dd:ee:01")  # noqa: SLF001
    qapp.processEvents()

    assert seen == [mw.arp_table]


def test_refresh_request_starts_a_lan_lookup(
    window: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """设备页的刷新按钮要真的把主窗的批量查找拉起来。"""
    first = "192.0.2.10:37495"
    second = "192.0.2.11:5555"
    window.config.device.serial = first
    window.config.device.known = [first, second]
    window.config.device.macs = {first: "aa:bb:cc:dd:ee:01"}

    class _Adb:
        path = "adb"

        def mdns_services(self) -> list[str]:
            return []

    window._adb = _Adb()  # noqa: SLF001
    seen: list[list[tuple[str, str]]] = []
    monkeypatch.setattr(
        mw.MainWindow,
        "_locate_all_worker",
        lambda _self, targets: seen.append(list(targets)),
    )

    window.device_page.refreshRequested.emit()
    thread = window._locate_thread  # noqa: SLF001
    assert thread is not None
    thread.join(2.0)
    assert seen == [[(first, "aa:bb:cc:dd:ee:01"), (second, "")]]


def test_refresh_batch_moves_device_and_carries_its_mac(
    window: Any, qapp: QApplication, monkeypatch: pytest.MonkeyPatch
) -> None:
    """刷新时按旧 MAC 找到新地址：列表里只留新地址，MAC 跟过去。"""
    from tablet.locator import STATUS_MOVED, Located

    previous = "192.0.2.10:37495"
    moved = "192.0.2.88:41000"
    mac = "88:a4:79:1a:2b:3c"
    window.config.device.serial = previous
    window.config.device.known = [previous, "192.0.2.11:5555"]
    window.config.device.macs = {previous: mac}

    class _Adb:
        path = "adb"

        def mdns_services(self) -> list[str]:
            return []

    window._adb = _Adb()  # noqa: SLF001

    def fake_locate(address: str, mac: str = "", **kwargs: Any) -> Located:
        assert address == previous
        assert mac == "88:a4:79:1a:2b:3c"
        assert kwargs.get("arp") is mw.arp_table
        return Located(status=STATUS_MOVED, address=moved, mac=mac, detail="IP 换了")

    monkeypatch.setattr(mw, "locate", fake_locate)
    window._locate_all_worker([(previous, mac)])  # noqa: SLF001
    qapp.processEvents()

    assert window.config.device.serial == moved
    assert window.config.device.known[0] == moved
    assert previous not in window.config.device.known
    assert window.config.device.macs == {moved: mac}


def test_center_on_screen_puts_the_window_in_the_middle(window: Any, qapp: QApplication) -> None:
    from PySide6.QtGui import QGuiApplication

    screen = QGuiApplication.primaryScreen()
    assert screen is not None
    area = screen.availableGeometry()
    window.resize(600, 400)
    window.show()
    qapp.processEvents()
    window.center_on_screen()
    qapp.processEvents()
    center = window.frameGeometry().center()
    assert abs(center.x() - area.center().x()) <= 2
    assert abs(center.y() - area.center().y()) <= 2


def test_packaged_icon_matches_the_runtime_brand() -> None:
    """assets/padlend.ico 必须是 brand_pixmap 渲染的同一套尺寸，别再是旧图标。

    16…128 用 BMP/DIB 条目（资源管理器读小尺寸只认这个），256 用 PNG；
    全 PNG 的 ICO 在部分 Windows 版本里会被显示成旧图标或空白。
    """
    import struct

    path = Path(__file__).resolve().parents[1] / "assets" / "padlend.ico"
    data = path.read_bytes()
    reserved, kind, count = struct.unpack("<HHH", data[:6])
    assert (reserved, kind) == (0, 1)
    assert count == 10
    sizes = []
    for index in range(count):
        offset = 6 + 16 * index
        width = data[offset] or 256
        size_bytes, image_offset = struct.unpack("<II", data[offset + 8 : offset + 16])
        assert image_offset + size_bytes <= len(data)
        blob = data[image_offset : image_offset + size_bytes]
        if width >= 256:
            assert blob[:4] == bytes.fromhex("89504e47"), "256 档用 PNG"
        else:
            header_size, dib_w, dib_h = struct.unpack("<Iii", blob[:12])
            assert header_size == 40, "小尺寸必须是 BITMAPINFOHEADER 条目"
            assert (dib_w, dib_h) == (width, width * 2)
            mask_row = ((width + 31) // 32) * 4
            assert len(blob) == 40 + width * width * 4 + mask_row * width
        sizes.append(width)
    assert sizes == [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]


# ---------------------------------------------------------------------------
# 0.3.12：关闭窗口 = 退出还是收进托盘
# ---------------------------------------------------------------------------
class _FakeTray:
    """只提供 close_to_tray 判定需要的那点协议。"""

    def __init__(self, available: bool = True, visible: bool = True) -> None:
        self._available = available
        self._visible = visible
        self.ensured = 0
        self.messages: list[tuple[str, str]] = []

    @property
    def available(self) -> bool:
        return self._available

    def ensure_visible(self) -> bool:
        self.ensured += 1
        return self._visible

    def notify(self, title: str, message: str) -> None:
        self.messages.append((title, message))

    def dispose(self) -> None:
        self._available = False


class _FakeTrayIcon:
    """假的 QSystemTrayIcon：只记调用，不碰真实通知区。"""

    def __init__(self, visible: bool = True) -> None:
        self._visible = visible
        self.shown = 0
        self.hidden = 0
        self.messages: list[tuple[str, str]] = []

    def show(self) -> None:
        self.shown += 1
        self._visible = True

    def hide(self) -> None:
        self.hidden += 1
        self._visible = False

    def isVisible(self) -> bool:  # noqa: N802 - Qt 命名
        return self._visible

    def setIcon(self, icon: Any) -> None:  # noqa: N802,N803 - Qt 命名
        pass

    def setToolTip(self, text: str) -> None:  # noqa: N802,N803 - Qt 命名
        pass

    def setContextMenu(self, menu: Any) -> None:  # noqa: N802,N803 - Qt 命名
        pass

    def showMessage(self, title: str, text: str, *args: Any) -> None:  # noqa: N802
        self.messages.append((title, text))


def test_close_to_tray_setting_roundtrips(tmp_path):
    path = tmp_path / "config.yaml"
    cfg = Config()
    assert cfg.ui.close_to_tray is False, "默认完全退出"
    cfg.ui.close_to_tray = True
    cfg.save(path)
    assert Config.load(path).ui.close_to_tray is True
    assert yaml.safe_load(path.read_text(encoding="utf-8"))["ui"]["close_to_tray"] is True


def test_settings_panel_close_to_tray_checkbox(window: Any, config_path: Path) -> None:
    panel = window.settings
    assert panel.close_tray_check.isChecked() is False
    panel.close_tray_check.setChecked(True)
    assert panel.save() is True
    assert window.config.ui.close_to_tray is True
    again = Config.load(config_path)
    assert again.ui.close_to_tray is True


def test_settings_panel_has_no_clipboard_hotkey(window: Any) -> None:
    """0.3.15 删掉了「剪贴板」快捷键：设置页只剩接管/交还与急停两行。"""
    panel = window.settings
    assert not hasattr(panel, "clipboard_edit")
    assert "hotkeys.clipboard" not in panel._field_widgets


def test_request_close_hides_when_close_to_tray(window: Any, qapp: Any) -> None:
    window.tray = _FakeTray(available=True)
    window.config.ui.close_to_tray = True
    window.show()
    qapp.processEvents()
    assert window.request_close() is True
    assert window.tray.ensured == 1, "隐藏前要先确认托盘图标真的挂上"
    assert window.isHidden() is True
    assert window._shutdown_done is False, "收进托盘不能走清理流程"
    assert window.tray.messages == [], "收进托盘不再弹“已最小化到托盘”气泡"


def test_request_close_minimizes_when_tray_icon_is_invisible(
    window: Any, qapp: Any
) -> None:
    # 0.3.16：图标挂不上时不能把窗口藏进“找不到”的状态，退化成最小化。
    window.tray = _FakeTray(available=True, visible=False)
    window.config.ui.close_to_tray = True
    window.show()
    qapp.processEvents()
    assert window.request_close() is True
    assert window.isMinimized() is True
    assert window._shutdown_done is False
    assert "系统托盘没有接受图标" in window.bottom_hint.text()


def test_tray_probe_decides_whether_the_icon_is_registered(monkeypatch: Any) -> None:
    # 0.3.18：Qt 的 isVisible() 不可信，判据改成 Shell_NotifyIcon 的返回值。
    import ui.tray as tray_mod

    controller = tray_mod.TrayController()
    fake = _FakeTrayIcon()
    controller.tray = fake

    monkeypatch.setattr(tray_mod, "probe_registration", lambda: (False, 5))
    monkeypatch.setattr(tray_mod, "registered_paths", lambda: [])
    assert controller.ensure_visible() is False
    assert controller.registered is False
    assert controller.last_error == 5
    assert fake.shown >= 0

    monkeypatch.setattr(tray_mod, "probe_registration", lambda: (True, 0))
    assert controller.ensure_visible() is True
    assert controller.registered is True


def test_request_close_quits_when_setting_off(window: Any) -> None:
    window.tray = _FakeTray(available=True)
    window.config.ui.close_to_tray = False
    assert window.request_close() is False
    assert window._shutdown_done is True


def test_request_close_quits_when_tray_is_missing(window: Any) -> None:
    window.tray = _FakeTray(available=False)  # 例如没有系统托盘的环境
    window.config.ui.close_to_tray = True
    assert window.request_close() is False
    assert window._shutdown_done is True


# ---------------------------------------------------------------------------
# 设备列表：改名、可移除、启动探测结论（0.3.13）
# ---------------------------------------------------------------------------
def test_device_page_is_called_a_device_list(window: Any) -> None:
    from PySide6.QtWidgets import QLabel

    labels = [label.text() for label in window.device_page.findChildren(QLabel)]
    assert "设备列表" in labels
    assert "可用设备" not in labels


def test_remembered_device_can_be_removed(window: Any) -> None:
    from PySide6.QtCore import Qt

    first = "192.0.2.10:37495"
    second = "10.0.0.9:5555"
    window.config.device.serial = first
    window.config.device.known = [first, second]
    page = window.device_page
    page.set_known([first, second])

    assert page.device_list.count() == 2
    item = page.device_list.item(0)
    assert item.data(Qt.ItemDataRole.UserRole) == first
    page.device_list.setCurrentItem(item)
    assert page.remove_button.isEnabled() is True

    page.remove_button.click()

    assert window.config.device.known == [second]
    assert window.config.device.serial == second
    assert page.device_list.count() == 1


def test_remembered_device_shows_the_probe_result(window: Any) -> None:
    serial = "192.0.2.10:37495"
    page = window.device_page
    page.set_known([serial])
    page.set_located(serial, "offline", "在局域网里找不到这台平板")

    item = page.device_list.item(0)
    row = page.device_list.itemWidget(item)
    texts = [label.text() for label in row.findChildren(QLabel)]
    assert any("不在线" in text for text in texts)
    assert "找不到" in item.toolTip()


def test_settings_widgets_do_not_react_to_the_wheel(window: Any) -> None:
    class _Wheel:
        def __init__(self) -> None:
            self.ignored = False

        def ignore(self) -> None:
            self.ignored = True

    window._switch_page(2)
    widgets = [
        window.settings.hz_combo,
        window.settings.layout_combo,
        window.settings.exit_slider,
        window.settings.arm_slider,
    ]
    for widget in widgets:
        event = _Wheel()
        widget.wheelEvent(event)
        assert event.ignored is True, widget
