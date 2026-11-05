"""1.0.0 发版表面测试：版本单源、主题令牌、托盘入口与关于对话框。"""

from __future__ import annotations

import os
import re
import tomllib
from pathlib import Path

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest
from PySide6.QtWidgets import QApplication, QDialog

import constants as C
from config import Config
from ui import about_dialog
from ui.about_dialog import NOTICE_NAME, AboutDialog
from ui.diagnostics import CommandResult
from ui.theme import tokens
from ui.tray import TrayController
from ui.wizard import SetupWizard, UhidPage

ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture(scope="session")
def qapp() -> QApplication:
    app = QApplication.instance()
    if app is None:
        app = QApplication([])
    if not isinstance(app, QApplication):
        raise TypeError("需要 QApplication")
    return app


def test_version_has_a_single_source() -> None:
    import padlend

    assert padlend.__version__ == C.VERSION
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    assert "version" not in project["project"], "版本只能来自 padlend.__version__"
    assert "version" in project["project"]["dynamic"]
    dynamic = project["tool"]["setuptools"]["dynamic"]["version"]
    assert dynamic["attr"] == "padlend.__version__"


def test_dark_qss_only_uses_known_tokens() -> None:
    template = (ROOT / "ui" / "theme" / "dark.qss").read_text(encoding="utf-8")
    used = set(re.findall(r"@([a-z0-9_]+)", template))
    unknown = used - set(tokens.qss_tokens())
    assert not unknown, f"dark.qss 使用了未定义的令牌：{sorted(unknown)}"


def test_tray_offers_logs_and_about(qapp: QApplication) -> None:
    tray = TrayController()
    try:
        labels = [a.text() for a in tray.menu.actions() if not a.isSeparator()]
        assert "打开日志目录…" in labels
        assert "关于 PadLend…" in labels
    finally:
        tray.dispose()


def test_about_dialog_reports_version_and_notice(qapp: QApplication) -> None:
    dialog = AboutDialog()
    try:
        assert C.VERSION in dialog.version_label.text()
        assert dialog.notice_path.name == NOTICE_NAME
        assert dialog.update_button.isEnabled() == bool(about_dialog.RELEASES_URL)
    finally:
        dialog.close()


def test_uhid_copy_does_not_promise_missing_fallback(qapp: QApplication) -> None:
    page = UhidPage()
    ok, detail = page.interpret(
        [
            CommandResult("uhid 组", 0, "groups=2000(shell),uhid", ""),
            CommandResult("uhid 可写", 1, "", "Permission denied"),
        ]
    )
    assert ok is False
    assert "功能不受影响" not in detail
    assert "绝对坐标" not in detail
    assert "无法创建虚拟鼠标/键盘" in detail


def test_wizard_finish_applies_checkbox_choices(qapp: QApplication) -> None:
    cfg = Config()
    wizard = SetupWizard(config=cfg)
    try:
        wizard.finish_page.config_check.setChecked(False)
        wizard.finish_page.next_check.setChecked(True)
        wizard._on_finished(int(QDialog.DialogCode.Accepted))
    finally:
        wizard.close()
    assert cfg.device.auto_connect is False
    assert cfg.ui.close_to_tray is True
