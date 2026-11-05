"""首次配置向导（设计文档 6.10.6 / 附录 C）。

五步：adb → USB 调试 → UHID 可写 → scrcpy server jar → 完成。
每一步的检测都通过 ``AdbChain``（QProcess）异步执行，向导本身不阻塞
GUI；构造向导不会启动任何进程。
"""

from __future__ import annotations

import logging
from collections.abc import Sequence
from pathlib import Path
from typing import Any

from PySide6.QtCore import Qt, Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QDialog,
    QFrame,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
    QWizard,
    QWizardPage,
)

import constants as C
from ui.diagnostics import (
    AdbChain,
    CommandResult,
    parse_devices,
    parse_pointer_speed,
    parse_uhid_group,
    parse_uhid_open,
    parse_xscale,
)
from ui.style import repolish

log = logging.getLogger("ui.wizard")

_REMOTE_JAR = "/data/local/tmp/scrcpy-server.jar"


def _summarize(results: Sequence[CommandResult]) -> str:
    if not results:
        return "没有返回结果"
    lines = []
    for result in results:
        head = "OK" if result.ok else f"失败(exit {result.code})"
        body = (result.stdout or result.stderr).strip().splitlines()
        snippet = body[0][:120] if body else ""
        lines.append(f"{result.label}: {head} {snippet}".strip())
    return "\n".join(lines)


class CheckPage(QWizardPage):
    """带异步检测按钮的向导页基类。"""

    def __init__(
        self,
        title: str,
        subtitle: str,
        body: str,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setTitle(title)
        self.setSubTitle(subtitle)
        self._chain: AdbChain | None = None

        self.body_label = QLabel(body, self)
        self.body_label.setWordWrap(True)
        self.body_label.setTextFormat(Qt.TextFormat.RichText)
        self.body_label.setObjectName("wizardBody")

        self.status_label = QLabel("尚未检测", self)
        self.status_label.setObjectName("banner")
        self.status_label.setWordWrap(True)
        self.status_label.setProperty("severity", "warn")

        self.check_button = QPushButton("开始检测", self)
        self.check_button.setProperty("accent", True)
        self.check_button.clicked.connect(self.run_check)

        self.result_label = QLabel("", self)
        self.result_label.setObjectName("mutedText")
        self.result_label.setWordWrap(True)
        self.result_label.setVisible(False)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 8, 0, 0)
        layout.setSpacing(12)
        layout.addWidget(self.body_label)
        layout.addWidget(self.status_label)
        layout.addWidget(self.check_button, 0, Qt.AlignmentFlag.AlignLeft)
        layout.addWidget(self.result_label)
        layout.addStretch(1)

    # ------------------------------------------------------------------ 子类
    def steps(self) -> Sequence[tuple[str, Sequence[str]]]:
        """要执行的 adb 命令；空列表表示纯本地检查。"""
        return ()

    def interpret(self, results: Sequence[CommandResult]) -> tuple[bool, str]:
        ok = bool(results) and all(item.ok for item in results)
        return ok, _summarize(results)

    def local_check(self) -> tuple[bool, str]:
        return False, "此步骤需要手动确认"

    # ------------------------------------------------------------------ 执行
    def run_check(self) -> None:
        steps = self.steps()
        if not steps:
            ok, text = self.local_check()
            self._apply_result(ok, text)
            return
        self._stop_chain()
        self.check_button.setEnabled(False)
        self._set_status("检测中 …", "warn")
        chain = AdbChain(steps, timeout_ms=12000, parent=self)
        chain.done.connect(self._on_done)
        self._chain = chain
        chain.start()

    def stop(self) -> None:
        self._stop_chain()

    def _stop_chain(self) -> None:
        if self._chain is not None and self._chain.running:
            self._chain.stop()
        self._chain = None

    def _on_done(self, _ok: bool, results: object) -> None:
        items = [item for item in (results or ()) if isinstance(item, CommandResult)]
        ok, text = self.interpret(items)
        self._apply_result(ok, text)

    def _apply_result(self, ok: bool, text: str) -> None:
        self._set_status("检测通过" if ok else "需要处理", "ok" if ok else "warn")
        self.result_label.setText(text)
        self.result_label.setVisible(bool(text))
        self.check_button.setEnabled(True)

    def _set_status(self, text: str, severity: str) -> None:
        self.status_label.setText(text)
        self.status_label.setProperty("severity", severity)
        repolish(self.status_label)


class AdbPage(CheckPage):
    """步骤 1：adb 是否可用、是否能列出设备。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(
            "1 / 5 · 安装 adb",
            "PadLend 通过 adb 与平板通信。",
            "<p>请确认已安装 <b>Android Platform Tools</b>，并且 <code>adb</code> "
            "在 PATH 中（或把 <code>adb.exe</code> 放在程序目录）。</p>"
            "<p>点击下方按钮执行 <code>adb devices -l</code>；如果列出设备且状态为 "
            "<code>device</code>，说明 adb 正常。</p>",
            parent,
        )

    def steps(self) -> Sequence[tuple[str, Sequence[str]]]:
        return [("adb devices -l", ("devices", "-l"))]

    def interpret(self, results: Sequence[CommandResult]) -> tuple[bool, str]:
        if not results:
            return False, "没有返回结果"
        first = results[0]
        devices = parse_devices(first.stdout)
        if devices:
            usable = [entry for entry in devices if entry.usable]
            if usable:
                return True, "已连接：" + "、".join(entry.serial for entry in usable)
            return False, (
                "设备已列出但不可用："
                + "、".join(f"{entry.serial}({entry.state})" for entry in devices)
            )
        message = first.stderr.strip() or first.stdout.strip()
        return False, f"未检测到设备。{message}"


class UsbPage(CheckPage):
    """步骤 2：USB 调试与授权。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(
            "2 / 5 · 开启 USB 调试",
            "需要在平板上允许调试，否则 adb 只能看到 unauthorized。",
            "<p>设置 → 关于平板 → 连续点击「版本号」7 次开启开发者选项；"
            "再进入 开发者选项 → 打开 <b>USB 调试</b>。</p>"
            "<p>用数据线连接后，平板会弹出「允许 USB 调试吗？」，勾选"
            "「始终允许」并确定。</p>",
            parent,
        )

    def steps(self) -> Sequence[tuple[str, Sequence[str]]]:
        return [
            ("adb devices -l", ("devices", "-l")),
            ("adb_enabled", ("shell", "settings", "get", "global", "adb_enabled")),
        ]

    def interpret(self, results: Sequence[CommandResult]) -> tuple[bool, str]:
        if not results:
            return False, "没有返回结果"
        devices = parse_devices(results[0].stdout)
        usable = [entry for entry in devices if entry.usable]
        if usable:
            return True, "已授权：" + "、".join(entry.serial for entry in usable)
        unauthorized = [entry for entry in devices if entry.state == "unauthorized"]
        if unauthorized:
            return False, "设备显示 unauthorized：请在平板上点「允许 USB 调试」。"
        enabled = parse_pointer_speed(results[1].stdout) if len(results) > 1 else ""
        if enabled == "1":
            return False, "USB 调试已开启，但 adb 未看到设备：检查数据线与 USB 模式。"
        return False, "未检测到设备，且 adb_enabled 不为 1：请先开启 USB 调试。"


class UhidPage(CheckPage):
    """步骤 3：/dev/uhid 可写（相对鼠标注入的前提）。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(
            "3 / 5 · 检查 UHID",
            "PadLend 通过 /dev/uhid 创建虚拟鼠标（无需 root 的机型可直接用）。",
            "<p>检测 adb shell 是否属于 <code>uhid</code> 组，以及能否打开 "
            "<code>/dev/uhid</code>。</p>"
            "<p>如果失败：部分机型需要 Magisk 模块或厂商开放 uhid 权限。"
            "UHID 是硬条件：不可写时 PadLend 无法创建虚拟鼠标/键盘，"
            "平板不会出现系统光标，键鼠输入也无法接管。</p>",
            parent,
        )

    def steps(self) -> Sequence[tuple[str, Sequence[str]]]:
        return [
            ("uhid 组", ("shell", "id")),
            ("uhid 可写", ("shell", "exec 3<>/dev/uhid && echo OPEN_OK")),
        ]

    def interpret(self, results: Sequence[CommandResult]) -> tuple[bool, str]:
        group = parse_uhid_group(results[0].stdout) if results else False
        opened = parse_uhid_open(results[1].stdout) if len(results) > 1 else False
        detail = (
            f"uhid 组：{'有' if group else '无'}\n"
            f"/dev/uhid 可打开：{'是' if opened else '否'}"
        )
        if opened:
            return True, detail + "\n相对鼠标注入可用。"
        return False, detail + "\nUHID 不可用：无法创建虚拟鼠标/键盘，不能接管。"


class JarPage(CheckPage):
    """步骤 4：scrcpy server jar 是否存在、能否推到设备。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(
            "4 / 5 · scrcpy server",
            "视频流与输入注入共用 scrcpy server，需要 jar 就位。",
            "<p>PadLend 自带 <code>vendor/scrcpy-server-v5.0.1.jar</code>；"
            "如果缺失，请重新解压发行包。</p>"
            "<p>「推送到平板」会把 jar 复制到 <code>/data/local/tmp/</code>。"
            "此操作会写入设备文件。</p>",
            parent,
        )
        self.push_button = QPushButton("推送到平板", self)
        self.push_button.setEnabled(False)
        self.push_button.clicked.connect(self._push)
        layout = self.layout()
        if layout is not None:
            layout.addWidget(self.push_button, 0, Qt.AlignmentFlag.AlignLeft)
        self._jar = Path(C.SCRCPY_SERVER_JAR)

    def local_check(self) -> tuple[bool, str]:
        try:
            exists = self._jar.is_file()
        except OSError as exc:
            return False, f"读取 {self._jar} 失败：{exc}"
        self.push_button.setEnabled(exists)
        if exists:
            size = self._jar.stat().st_size
            return True, f"{self._jar.name}（{size // 1024} KB）已就位。"
        return False, f"缺少 {self._jar}"

    def _push(self) -> None:
        if not self._jar.is_file():
            return
        self._stop_chain()
        self.push_button.setEnabled(False)
        self._set_status("推送中 …", "warn")
        chain = AdbChain(
            [("adb push", ("push", str(self._jar), _REMOTE_JAR))],
            timeout_ms=30000,
            parent=self,
        )
        chain.done.connect(self._on_push_done)
        self._chain = chain
        chain.start()

    def _on_push_done(self, ok: bool, results: object) -> None:
        self.push_button.setEnabled(True)
        items = [item for item in (results or ()) if isinstance(item, CommandResult)]
        text = _summarize(items)
        if ok:
            self._apply_result(True, f"已推送到 {_REMOTE_JAR}\n{text}")
        else:
            self._apply_result(False, text)


class FinishPage(QWizardPage):
    """步骤 5：完成。"""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setTitle("5 / 5 · 完成")
        self.setSubTitle("一切就绪，回到主窗口开始使用。")
        self.config_check = QCheckBox("启动时自动连接平板", self)
        self.next_check = QCheckBox("连接后自动进入托盘（关闭窗口不退出）", self)
        self.config_check.setChecked(True)
        self.next_check.setChecked(True)

        body = QLabel(
            "<p>接下来：</p>"
            "<ul>"
            "<li>主窗左侧是虚拟桌面，拖动平板图标即可改变吸附边。</li>"
            "<li><code>Ctrl+Alt+S</code> 手动接管 / 交还，<code>Ctrl+Alt+Q</code> 急停。</li>"
            "<li>遇到问题：托盘菜单 → 复制诊断信息，或「设置 → 诊断」导出 zip。</li>"
            "</ul>",
            self,
        )
        body.setWordWrap(True)
        body.setTextFormat(Qt.TextFormat.RichText)

        card = QFrame(self)
        card.setObjectName("card")
        card_layout = QVBoxLayout(card)
        card_layout.setContentsMargins(12, 12, 12, 12)
        card_layout.setSpacing(8)
        card_layout.addWidget(self.config_check)
        card_layout.addWidget(self.next_check)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 8, 0, 0)
        layout.setSpacing(12)
        layout.addWidget(body)
        layout.addWidget(card)
        layout.addStretch(1)


class SetupWizard(QWizard):
    """五步首次配置向导。"""

    completed = Signal()

    def __init__(self, parent: QWidget | None = None, *, config: object | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("setupWizard")
        self.setWindowTitle(f"{C.APP_NAME} 首次设置")
        self.resize(720, 520)
        self.setWizardStyle(QWizard.WizardStyle.ClassicStyle)
        self.setOption(QWizard.WizardOption.NoBackButtonOnStartPage, True)
        self.setOption(QWizard.WizardOption.HaveHelpButton, False)
        self._config = config

        self.adb_page = AdbPage(self)
        self.usb_page = UsbPage(self)
        self.uhid_page = UhidPage(self)
        self.jar_page = JarPage(self)
        self.finish_page = FinishPage(self)
        self._pages = [
            self.adb_page,
            self.usb_page,
            self.uhid_page,
            self.jar_page,
            self.finish_page,
        ]
        for page in self._pages:
            self.addPage(page)

        self._localize_buttons()
        self.finished.connect(self._on_finished)

    @property
    def pages_list(self) -> list[QWizardPage]:
        return list(self._pages)

    def check_pages(self) -> list[CheckPage]:
        return [page for page in self._pages if isinstance(page, CheckPage)]

    def _localize_buttons(self) -> None:
        labels = {
            QWizard.WizardButton.BackButton: "上一步",
            QWizard.WizardButton.NextButton: "下一步",
            QWizard.WizardButton.FinishButton: "完成",
            QWizard.WizardButton.CancelButton: "取消",
        }
        for button, text in labels.items():
            self.setButtonText(button, text)

    def stop_all(self) -> None:
        for page in self.check_pages():
            page.stop()

    def _on_finished(self, result: int) -> None:
        self.stop_all()
        if result == int(QDialog.DialogCode.Accepted):
            if self._config is not None:
                device = getattr(self._config, "device", None)
                ui = getattr(self._config, "ui", None)
                if device is not None:
                    device.auto_connect = bool(
                        self.finish_page.config_check.isChecked()
                    )
                if ui is not None:
                    ui.close_to_tray = bool(
                        self.finish_page.next_check.isChecked()
                    )
            self.completed.emit()
            log.info("setup wizard completed")

    def reject(self) -> None:
        self.stop_all()
        super().reject()

    def closeEvent(self, event: Any) -> None:  # noqa: N802 - Qt 命名
        self.stop_all()
        super().closeEvent(event)


def wizard_summary(config: object | None = None) -> str:
    """给日志/诊断用的向导行动摘要（纯文本，不执行任何命令）。"""
    serial = getattr(getattr(config, "device", None), "serial", "") if config else ""
    return (
        f"adb：{'已配置' if serial else '待检测'}\n"
        f"server jar：{C.SCRCPY_SERVER_JAR}"
    )


__all__ = [
    "AdbPage",
    "CheckPage",
    "FinishPage",
    "JarPage",
    "SetupWizard",
    "UhidPage",
    "UsbPage",
    "parse_xscale",
    "parse_pointer_speed",
    "wizard_summary",
]
