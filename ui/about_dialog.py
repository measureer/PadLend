"""关于 PadLend 对话框（1.0.0）。

发版审计 R0-5：版本、第三方许可与日志入口要放在用户找得到的地方。
对话框只读本地文件，不发任何网络请求；检查更新仅打开固定 Releases 页。
"""

from __future__ import annotations

import logging
import os
import sys
from pathlib import Path

from PySide6.QtCore import QUrl
from PySide6.QtGui import QDesktopServices
from PySide6.QtWidgets import (
    QDialog,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

import constants as C
from ui.icons import brand_pixmap

log = logging.getLogger("ui.about")

NOTICE_NAME = "THIRD_PARTY_NOTICES.md"

# 发布地址确定后填写（例如 https://github.com/<owner>/PadLend）；
# 为空时“检查更新…”保持禁用，避免把用户带到不存在的页面。
RELEASES_URL = ""


def open_local(path: Path) -> bool:
    """用系统默认程序打开文件/目录；失败返回 False，不抛给调用方。"""
    try:
        if not path.exists():
            return False
        if sys.platform == "win32":
            os.startfile(str(path))  # type: ignore[attr-defined]
        else:
            QDesktopServices.openUrl(QUrl.fromLocalFile(str(path)))
        return True
    except OSError as exc:
        log.warning("open local path failed path=%s err=%r", path, exc)
        return False


class AboutDialog(QDialog):
    """版本、许可与支持入口。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("aboutDialog")
        self.setWindowTitle(f"关于 {C.APP_NAME}")
        self.setMinimumWidth(580)
        self.notice_path = C.BUNDLE_DIR / NOTICE_NAME
        self._build()

    def _build(self) -> None:
        icon = QLabel(self)
        icon.setFixedSize(56, 56)
        icon.setPixmap(brand_pixmap(56))

        self.version_label = QLabel(f"{C.APP_NAME} {C.VERSION}", self)
        self.version_label.setObjectName("sectionTitle")
        subtitle = QLabel("把 Android 平板变成 Windows 的第二块输入屏", self)
        subtitle.setObjectName("hintText")
        subtitle.setWordWrap(True)

        title_column = QVBoxLayout()
        title_column.setContentsMargins(0, 0, 0, 0)
        title_column.setSpacing(2)
        title_column.addWidget(self.version_label)
        title_column.addWidget(subtitle)

        header = QHBoxLayout()
        header.setContentsMargins(0, 0, 0, 0)
        header.setSpacing(14)
        header.addWidget(icon, 0)
        header.addLayout(title_column, 1)

        license_text = QLabel(
            f"PadLend 以 {C.LICENSE} 许可发布。界面使用 Qt for Python"
            "（PySide6，LGPLv3）；发行包以目录形式分发（onedir），Qt 运行库"
            "位于 _internal 目录，可按 LGPLv3 要求自行替换。\n\n"
            "scrcpy 与 Android platform-tools 为 Apache-2.0；"
            "PyYAML 为 MIT；psutil 为 BSD-3-Clause；"
            "PyInstaller 为 GPLv2 + bootloader exception。"
        )
        license_text.setWordWrap(True)

        self.status_label = QLabel("", self)
        self.status_label.setObjectName("mutedText")
        self.status_label.setWordWrap(True)
        self.status_label.hide()

        self.notice_button = QPushButton("第三方许可…", self)
        self.notice_button.setProperty("flat", True)
        self.logs_button = QPushButton("打开日志目录…", self)
        self.logs_button.setProperty("flat", True)
        self.update_button = QPushButton("检查更新…", self)
        self.update_button.setEnabled(bool(RELEASES_URL))
        if not RELEASES_URL:
            self.update_button.setToolTip("发行地址确定后启用")
        self.close_button = QPushButton("关闭", self)
        self.close_button.setProperty("accent", True)

        buttons = QHBoxLayout()
        buttons.setContentsMargins(0, 0, 0, 0)
        buttons.setSpacing(8)
        buttons.addWidget(self.notice_button)
        buttons.addWidget(self.logs_button)
        buttons.addStretch(1)
        buttons.addWidget(self.update_button)
        buttons.addWidget(self.close_button)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(20, 20, 20, 16)
        layout.setSpacing(14)
        layout.addLayout(header)
        layout.addWidget(license_text)
        layout.addWidget(self.status_label)
        layout.addLayout(buttons)

        self.notice_button.clicked.connect(self.open_notice)
        self.logs_button.clicked.connect(self.open_logs)
        self.update_button.clicked.connect(self.open_releases)
        self.close_button.clicked.connect(self.reject)

    # ------------------------------------------------------------------ 动作
    def _status(self, text: str) -> None:
        self.status_label.setText(text)
        self.status_label.show()

    def open_notice(self) -> None:
        if not open_local(self.notice_path):
            self._status(
                f"未找到 {NOTICE_NAME}；请重新解压完整的发行包，"
                "或到项目主页查看许可说明。"
            )

    def open_logs(self) -> None:
        path = C.log_dir()
        try:
            path.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            self._status(f"无法创建日志目录：{exc}")
            return
        if not open_local(path):
            self._status(f"无法打开日志目录：{path}")

    def open_releases(self) -> None:
        if not RELEASES_URL:
            return
        if not QDesktopServices.openUrl(QUrl(RELEASES_URL)):
            self._status("无法打开浏览器；请手动访问项目 Releases 页面。")
