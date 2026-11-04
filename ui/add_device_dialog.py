"""添加无线设备对话框（6.1.1 的无线调试补充）。

Android 11+ 的「无线调试」有两个端口：配对端口（一次性，随机）和连接端口
（「IP 地址和端口」里那个）。mDNS 在 Windows 上经常发现不到设备，所以设备页
必须提供手动输入地址的入口——本对话框就是它。

约定：对话框只做输入与校验，**不碰 adb**；配对/连接由主窗在后台线程执行，
避免阻塞 GUI。
"""

from __future__ import annotations

import re

from PySide6.QtCore import Qt
from PySide6.QtWidgets import (
    QDialog,
    QFrame,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from tablet.adb import normalize_address
from ui.style import repolish

_HOST_RE = re.compile(r"^[0-9A-Za-z._-]+:\d{1,5}$")
_CODE_RE = re.compile(r"^\d{6}$")


def validate_address(text: str) -> tuple[str, str]:
    """返回 ``(规范化地址, 错误文案)``；地址合法时错误文案为空。"""
    normalized = normalize_address(text)
    if not normalized:
        return "", "请填写平板上的 IP 地址（例如 192.168.1.23:5555）"
    if not _HOST_RE.match(normalized):
        return normalized, "地址格式应为 IP:端口，例如 192.168.1.23:5555"
    port = int(normalized.rsplit(":", 1)[1])
    if not 1 <= port <= 65535:
        return normalized, "端口必须在 1–65535 之间"
    return normalized, ""


def validate_pairing_code(text: str) -> tuple[str, str]:
    """配对码可留空（已配对过），填了就必须是 6 位数字。"""
    code = str(text or "").strip()
    if not code:
        return "", ""
    if not _CODE_RE.match(code):
        return code, "配对码是平板上的 6 位数字"
    return code, ""


class AddDeviceDialog(QDialog):
    """无线调试：地址 + 可选配对码。"""

    def __init__(self, parent: QWidget | None = None, *, prefill: str = "") -> None:
        super().__init__(parent)
        self.setObjectName("addDeviceDialog")
        self.setWindowTitle("添加无线设备")
        self.setModal(True)
        self.resize(460, 0)
        self._build()
        if prefill:
            self.address_edit.setText(str(prefill))

    # ------------------------------------------------------------------ 构建
    def _build(self) -> None:
        title = QLabel("添加无线设备", self)
        title.setObjectName("sectionTitle")

        hint = QLabel(
            "在平板上打开「设置 → 开发者选项 → 无线调试」。\n"
            "1. 首次连接点「使用配对码配对设备」，把弹窗里的地址和 6 位配对码填到下面；\n"
            "2. 配对成功后，回到无线调试页，填「IP 地址和端口」（不是配对端口）再点连接。\n"
            "电脑与平板要在同一个 Wi-Fi，且路由器没开客户端隔离。",
            self,
        )
        hint.setObjectName("hintText")
        hint.setWordWrap(True)

        self.address_edit = QLineEdit(self)
        self.address_edit.setPlaceholderText("平板 IP:端口，例如 192.168.1.23:5555")
        self.address_edit.setClearButtonEnabled(True)
        self.address_edit.textChanged.connect(self._clear_error)
        self.address_edit.returnPressed.connect(self._accept)

        self.code_edit = QLineEdit(self)
        self.code_edit.setPlaceholderText("配对码（首次配对时填 6 位数字，可留空）")
        self.code_edit.setMaxLength(6)
        self.code_edit.textChanged.connect(self._clear_error)
        self.code_edit.returnPressed.connect(self._accept)

        self.error_label = QLabel("", self)
        self.error_label.setObjectName("mutedText")
        self.error_label.setProperty("error", True)
        self.error_label.setWordWrap(True)
        self.error_label.hide()

        form = QVBoxLayout()
        form.setSpacing(10)
        for label_text, widget in (("设备地址", self.address_edit), ("配对码", self.code_edit)):
            row = QVBoxLayout()
            row.setSpacing(4)
            label = QLabel(label_text, self)
            label.setObjectName("detailLabel")
            row.addWidget(label)
            row.addWidget(widget)
            form.addLayout(row)

        card = QFrame(self)
        card.setObjectName("card")
        card_layout = QVBoxLayout(card)
        card_layout.setContentsMargins(14, 14, 14, 14)
        card_layout.setSpacing(10)
        card_layout.addLayout(form)
        card_layout.addWidget(self.error_label)

        self.cancel_button = QPushButton("取消", self)
        self.cancel_button.setProperty("flat", True)
        self.cancel_button.clicked.connect(self.reject)

        self.connect_button = QPushButton("连接", self)
        self.connect_button.setObjectName("primaryButton")
        self.connect_button.setDefault(True)
        self.connect_button.clicked.connect(self._accept)
        self.code_edit.textChanged.connect(self._sync_button_label)

        buttons = QHBoxLayout()
        buttons.setContentsMargins(0, 0, 0, 0)
        buttons.setSpacing(8)
        buttons.addStretch(1)
        buttons.addWidget(self.cancel_button)
        buttons.addWidget(self.connect_button)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(16, 16, 16, 16)
        layout.setSpacing(12)
        layout.addWidget(title)
        layout.addWidget(hint)
        layout.addWidget(card)
        layout.addLayout(buttons)

    # ------------------------------------------------------------------ 交互
    @property
    def address(self) -> str:
        return validate_address(self.address_edit.text())[0]

    @property
    def pairing_code(self) -> str:
        return validate_pairing_code(self.code_edit.text())[0]

    def show_error(self, text: str) -> None:
        self.error_label.setText(str(text or "操作失败"))
        self.error_label.show()
        repolish(self.error_label)

    def _clear_error(self, *_args: object) -> None:
        if self.error_label.isVisible():
            self.error_label.hide()
        for edit in (self.address_edit, self.code_edit):
            if edit.property("invalid"):
                edit.setProperty("invalid", False)
                repolish(edit)

    def _sync_button_label(self, *_args: object) -> None:
        """填了配对码才需要「配对并连接」，否则只是连接已有授权的设备。"""
        has_code = bool(self.code_edit.text().strip())
        self.connect_button.setText("配对并连接" if has_code else "连接")

    def _accept(self) -> None:
        address, address_error = validate_address(self.address_edit.text())
        code, code_error = validate_pairing_code(self.code_edit.text())
        if address_error:
            self._flag(self.address_edit, address_error)
            return
        if code_error:
            self._flag(self.code_edit, code_error)
            return
        if self.code_edit.text().strip() and not code:
            self._flag(self.code_edit, "配对码是 6 位数字")
            return
        self.address_edit.setText(address)
        self.accept()

    def _flag(self, edit: QLineEdit, message: str) -> None:
        edit.setProperty("invalid", True)
        repolish(edit)
        edit.setFocus(Qt.FocusReason.OtherFocusReason)
        self.show_error(message)
