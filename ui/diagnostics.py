"""诊断信息采集与导出（设计文档 6.10.8 + 附录 C.2/C.3）。

所有 adb 调用都通过 ``QProcess`` 异步执行，绝不在 GUI 线程里阻塞等待；
``DiagnosticsDialog.start()`` 才真正拉起进程，纯构造是安全的（offscreen
测试可直接构造）。
"""

from __future__ import annotations

import contextlib
import json
import logging
import re
import sys
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

from PySide6.QtCore import QObject, QProcess, QTimer, Signal
from PySide6.QtGui import QGuiApplication
from PySide6.QtWidgets import (
    QDialog,
    QFrame,
    QHBoxLayout,
    QLabel,
    QPlainTextEdit,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

import constants as C
from ui.style import build_mono_font

log = logging.getLogger("ui.diagnostics")

_MAX_DUMPSYS_CHARS = 20000
_DEFAULT_TIMEOUT_MS = 12000
_DEVICE_STATES = frozenset(
    {"device", "offline", "unauthorized", "recovery", "sideload", "bootloader", "host"}
)
_XSCALE_RE = re.compile(r"XScale\s*:\s*([0-9]*\.?[0-9]+)", re.IGNORECASE)
_UHID_GROUP_RE = re.compile(r"\buhid\b")

# 诊断包会离开用户机器，以下内容默认打码（发版审计 P0-3）。
_MAX_LOG_TAIL_BYTES = 512 * 1024
_IP_RE = re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b")
_MAC_RE = re.compile(r"\b(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}\b")
_WIN_USER_RE = re.compile(r"([A-Za-z]:\\Users\\)[^\\\r\n\t ]+", re.IGNORECASE)
_POSIX_USER_RE = re.compile(r"((?:/Users|/home)/)[^/\s]+")


def redact_text(text: str, *, serials: Sequence[str] = ()) -> str:
    """抹掉序列号、IP/MAC、用户名与用户目录路径。"""
    if not text:
        return text
    out = text
    for serial in sorted({s for s in serials if s}, key=len, reverse=True):
        if len(serial) >= 4:
            out = out.replace(serial, "<serial>")
    out = _IP_RE.sub("<IP>", out)
    out = _MAC_RE.sub("<MAC>", out)
    out = _WIN_USER_RE.sub(r"\1<user>", out)
    out = _POSIX_USER_RE.sub(r"\1<user>", out)
    return out


def redact_adb_devices(raw: str, *, serials: Sequence[str] = ()) -> str:
    """把 adb devices -l 的 serial 列逐行打码。"""
    if not raw:
        return raw
    masked = redact_text(raw, serials=serials)
    lines = []
    for line in masked.splitlines(keepends=True):
        stripped = line.strip()
        if not stripped or stripped.startswith(("List of devices", "*")):
            lines.append(line)
            continue
        parts = line.split(maxsplit=1)
        if parts and len(parts[0]) >= 4 and not parts[0].startswith("<"):
            line = "<serial>" + ((" " + parts[1]) if len(parts) > 1 else "")
        lines.append(line)
    return "".join(lines)


def _read_crash_latest(limit: int = 256 * 1024) -> tuple[str, str]:
    """最近一次崩溃日志 (文件名, 内容)；没有返回 ('', '')。"""
    try:
        log_dir = C.log_dir()
        candidates = sorted(log_dir.glob("crash-*.log"), key=lambda p: p.stat().st_mtime)
    except Exception:  # noqa: BLE001 - 诊断绝不能因日志目录失败而崩
        return "", ""
    if not candidates:
        return "", ""
    path = candidates[-1]
    try:
        data = path.read_bytes()
    except OSError:
        return "", ""
    if len(data) > limit:
        data = data[-limit:]
    return path.name, data.decode("utf-8", errors="replace")


def _read_log_tail(limit: int = _MAX_LOG_TAIL_BYTES) -> str:
    """读取最近日志尾部（超过 512 KB 只保留最后一段完整行）。"""
    try:
        log_dir = C.log_dir()
    except Exception:  # noqa: BLE001 - 诊断绝不能因日志目录失败而崩
        return ""
    path = log_dir / "padlend.log"
    try:
        if not path.is_file():
            rotated = [item for item in log_dir.glob("padlend.log.*") if item.is_file()]
            if not rotated:
                return ""
            # 轮转文件是 padlend.log.1/.2/.3，字典序最后一个反而最旧；
            # 日志被清空时取 mtime 最新的一份。
            path = max(rotated, key=lambda item: item.stat().st_mtime)
        size = path.stat().st_size
        with path.open("rb") as fh:
            if size > limit:
                fh.seek(size - limit)
                data = fh.read()
                cut = data.find(b"\n")
                data = data[cut + 1 :] if cut >= 0 else data
            else:
                data = fh.read()
    except OSError:
        return ""
    return data.decode("utf-8", errors="replace")


_README = """PadLend 诊断包
==============
包含：
  diagnostics.txt      概要 + adb devices + dumpsys 节选（已脱敏）
  dumpsys-input.txt    dumpsys input 原文节选（已脱敏）
  adb-devices.txt      adb devices -l（序列号已打码）
  logs/padlend-tail.log  最近 512 KB 运行日志（已脱敏）
  logs/crash-latest.log  最近一次崩溃日志（如有，已脱敏）
  meta.json            机器可读摘要（不含序列号/绝对路径）

已自动隐藏：设备序列号、IP/MAC 地址、Windows 用户名与用户目录路径。
如仍看到隐私内容，请先自行删除再分享。
"""

DIAGNOSTIC_STEPS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("adb devices -l", ("devices", "-l")),
    ("dumpsys input", ("shell", "dumpsys", "input")),
    ("pointer_speed", ("shell", "settings", "get", "system", "pointer_speed")),
    ("uhid 组", ("shell", "id")),
    ("uhid 可写", ("shell", "exec 3<>/dev/uhid && echo OPEN_OK")),
)


# ---------------------------------------------------------------------- 执行
@dataclass(frozen=True, slots=True)
class CommandResult:
    """一条 adb 命令的执行结果。"""

    label: str
    code: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.code == 0


class AdbChain(QObject):
    """顺序执行若干条 adb 命令，全部结束后发 ``done(ok, results)``。"""

    progress = Signal(str)
    done = Signal(bool, object)

    def __init__(
        self,
        steps: Sequence[tuple[str, Sequence[str]]],
        *,
        timeout_ms: int = _DEFAULT_TIMEOUT_MS,
        parent: QObject | None = None,
    ) -> None:
        super().__init__(parent)
        self._steps: list[tuple[str, tuple[str, ...]]] = [
            (str(label), tuple(str(arg) for arg in args)) for label, args in steps
        ]
        self._timeout_ms = int(timeout_ms)
        self._results: list[CommandResult] = []
        self._index = 0
        self._proc: QProcess | None = None
        self._timer: QTimer | None = None
        self._running = False
        self._killed = False

    @property
    def running(self) -> bool:
        return self._running

    def start(self) -> None:
        if self._running:
            log.debug("AdbChain already running; ignored")
            return
        self._running = True
        self._index = 0
        self._results = []
        self._start_next()

    def stop(self) -> None:
        self._running = False
        self._stop_timer()
        proc = self._proc
        self._proc = None
        if proc is not None:
            with contextlib.suppress(RuntimeError):  # 进程对象已被 Qt 回收
                proc.kill()
            proc.deleteLater()

    # ------------------------------------------------------------------ 内部
    def _start_next(self) -> None:
        if not self._running:
            return
        if self._index >= len(self._steps):
            self._finish()
            return
        label, args = self._steps[self._index]
        self.progress.emit(f"{label} …")
        proc = QProcess(self)
        proc.setProgram(C.ADB_EXE)
        proc.setArguments(list(args))
        proc.setProcessChannelMode(QProcess.ProcessChannelMode.SeparateChannels)
        proc.finished.connect(self._on_finished)
        proc.errorOccurred.connect(self._on_error)
        self._proc = proc
        timer = QTimer(self)
        timer.setSingleShot(True)
        timer.timeout.connect(self._on_timeout)
        self._timer = timer
        self._killed = False
        proc.start()
        timer.start(self._timeout_ms)

    def _stop_timer(self) -> None:
        timer = self._timer
        self._timer = None
        if timer is not None:
            timer.stop()
            timer.deleteLater()

    def _read_all(self) -> tuple[str, str]:
        proc = self._proc
        if proc is None:
            return "", ""
        out = proc.readAllStandardOutput().data().decode("utf-8", "replace")
        err = proc.readAllStandardError().data().decode("utf-8", "replace")
        return out, err

    def _record(self, code: int, stdout: str, stderr: str) -> None:
        label = self._steps[self._index][0] if self._index < len(self._steps) else ""
        self._results.append(CommandResult(label, int(code), stdout, stderr))
        self._index += 1

    def _on_finished(self, code: int, _status: QProcess.ExitStatus) -> None:
        if self._proc is None:
            return
        self._stop_timer()
        stdout, stderr = self._read_all()
        if self._killed:
            stderr = f"{stderr}（超时）".strip()
        self._record(code, stdout, stderr)
        self._cleanup_proc()
        self._start_next()

    def _on_error(self, error: QProcess.ProcessError) -> None:
        if self._proc is None:
            return
        if error != QProcess.ProcessError.FailedToStart:
            return
        self._stop_timer()
        self._record(-1, "", f"无法启动 {C.ADB_EXE}（未安装或不在 PATH）")
        self._cleanup_proc()
        self._start_next()

    def _on_timeout(self) -> None:
        proc = self._proc
        if proc is None:
            return
        log.warning("adb step timeout: %s", self._steps[self._index][0])
        self._killed = True
        proc.kill()

    def _cleanup_proc(self) -> None:
        proc = self._proc
        self._proc = None
        if proc is not None:
            proc.deleteLater()

    def _finish(self) -> None:
        self._running = False
        results = self._results
        self._results = []
        ok = all(result.ok for result in results) if results else False
        self.done.emit(ok, results)


# ---------------------------------------------------------------------- 解析
@dataclass(frozen=True, slots=True)
class DeviceEntry:
    """``adb devices -l`` 的一行。"""

    serial: str
    state: str
    extra: str = ""

    @property
    def usable(self) -> bool:
        return self.state == "device"


def parse_devices(text: str) -> list[DeviceEntry]:
    entries: list[DeviceEntry] = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("*") or line.startswith("List of devices"):
            continue
        parts = line.split()
        if len(parts) < 2 or parts[1] not in _DEVICE_STATES:
            continue
        entries.append(DeviceEntry(parts[0], parts[1], " ".join(parts[2:])))
    return entries


def parse_xscale(text: str) -> float | None:
    match = _XSCALE_RE.search(text or "")
    if match is None:
        return None
    try:
        return float(match.group(1))
    except ValueError:
        return None


def parse_pointer_speed(text: str) -> str:
    value = (text or "").strip()
    return "" if value.lower() in {"", "null", "undefined"} else value


def parse_uhid_group(text: str) -> bool:
    return _UHID_GROUP_RE.search(text or "") is not None


def parse_uhid_open(text: str) -> bool:
    return "OPEN_OK" in (text or "")


def desktop_dir() -> Path:
    """优先桌面目录，其次用户主目录。"""
    candidates = [Path.home() / "Desktop", Path.home() / "桌面"]
    for candidate in candidates:
        if candidate.is_dir():
            return candidate
    return Path.home()


# ---------------------------------------------------------------------- 报告
@dataclass(slots=True)
class DiagnosticsReport:
    """一次诊断采集的汇总。"""

    devices: list[DeviceEntry] = field(default_factory=list)
    devices_raw: str = ""
    dumpsys_input: str = ""
    pointer_speed: str = ""
    uhid_group: bool = False
    uhid_open: bool = False
    server_jar: str = ""
    server_jar_present: bool = False
    errors: list[str] = field(default_factory=list)
    collected_at: str = ""

    @property
    def xscale(self) -> float | None:
        return parse_xscale(self.dumpsys_input)

    def primary_device(self) -> DeviceEntry | None:
        for entry in self.devices:
            if entry.usable:
                return entry
        return self.devices[0] if self.devices else None

    def serials(self) -> list[str]:
        return [entry.serial for entry in self.devices if entry.serial]

    def summary(self) -> list[str]:
        device = self.primary_device()
        device_text = "未检测到设备" if device is None else f"<serial>（{device.state}）"
        xscale = self.xscale
        return [
            f"设备：{device_text}",
            f"XScale：{xscale if xscale is not None else '未读取到'}",
            f"pointer_speed：{self.pointer_speed or '未设置'}",
            f"UHID 组：{'有' if self.uhid_group else '无'}",
            f"UHID 可打开：{'是' if self.uhid_open else '否'}",
            f"server jar：{'已就绪' if self.server_jar_present else '缺失'}",
        ]

    def to_text(self) -> str:
        serials = self.serials()
        lines = [
            f"{C.APP_NAME} {C.VERSION} 诊断报告",
            "（设备序列号、IP/MAC、用户名与用户目录路径已自动打码）",
            f"生成时间：{self.collected_at or datetime.now().isoformat(timespec='seconds')}",
            f"Python：{sys.version.split()[0]}",
            f"平台：{sys.platform}",
            "配置：config.yaml（位于应用配置目录）",
            "",
            "== 概要 ==",
            *self.summary(),
        ]
        if self.errors:
            lines.extend(["", "== 错误 ==", redact_text("\n".join(self.errors), serials=serials)])
        lines.extend(
            [
                "",
                "== adb devices -l ==",
                redact_adb_devices(self.devices_raw, serials=serials).strip() or "（空）",
                "",
                "== UHID 原始输出 ==",
                f"组：{'uhid' if self.uhid_group else '（未包含 uhid）'}",
                f"打开：{'OPEN_OK' if self.uhid_open else '（未成功）'}",
            ]
        )
        dumpsys = redact_text(self.dumpsys_input.strip(), serials=serials)
        truncated = len(dumpsys) > _MAX_DUMPSYS_CHARS
        if truncated:
            dumpsys = dumpsys[:_MAX_DUMPSYS_CHARS] + "\n…（已截断）"
        lines.extend(["", "== dumpsys input（节选，已脱敏） ==", dumpsys or "（空）"])
        return "\n".join(lines)


def build_report(
    results: Sequence[CommandResult],
    *,
    server_jar: Path | None = None,
) -> DiagnosticsReport:
    """把 ``AdbChain`` 的结果整理成报告。"""
    report = DiagnosticsReport(collected_at=datetime.now().isoformat(timespec="seconds"))
    jar = Path(server_jar) if server_jar is not None else Path(C.SCRCPY_SERVER_JAR)
    report.server_jar = str(jar)
    try:
        report.server_jar_present = jar.is_file()
    except OSError:
        report.server_jar_present = False

    for result in results:
        label = result.label
        if not result.ok:
            report.errors.append(f"{label} 失败（exit {result.code}）：{result.stderr.strip()}")
        if label == "adb devices -l":
            report.devices_raw = result.stdout
            report.devices = parse_devices(result.stdout)
        elif label == "dumpsys input":
            report.dumpsys_input = result.stdout
        elif label == "pointer_speed":
            report.pointer_speed = parse_pointer_speed(result.stdout)
            if not report.pointer_speed and result.stderr.strip():
                report.errors.append(f"pointer_speed：{result.stderr.strip()}")
        elif label == "uhid 组":
            report.uhid_group = parse_uhid_group(result.stdout)
        elif label == "uhid 可写":
            report.uhid_open = parse_uhid_open(result.stdout)
    return report


def export_report(report: DiagnosticsReport, *, directory: Path | None = None) -> Path:
    """把报告写成 zip，返回文件路径。

    发版审计 P0-3：导出物默认脱敏（serial / IP / MAC / 用户名 / 用户目录），
    并额外附带最近 512 KB 日志；旧版只写原始 dumpsys 与原始 adb devices。
    """
    target_dir = Path(directory) if directory is not None else desktop_dir()
    target_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    path = target_dir / f"padlend-diagnostics-{stamp}.zip"
    serials = report.serials()
    jar_name = Path(report.server_jar).name if report.server_jar else ""
    meta = {
        "app": C.APP_NAME,
        "version": C.VERSION,
        "collected_at": report.collected_at,
        "platform": sys.platform,
        "python": sys.version.split()[0],
        "config_path": "config.yaml",
        "xscale": report.xscale,
        "pointer_speed": report.pointer_speed,
        "uhid_group": report.uhid_group,
        "uhid_open": report.uhid_open,
        "server_jar": jar_name,
        "devices": [
            {
                "serial": "<serial>" if entry.serial else "",
                "state": entry.state,
                "extra": redact_text(entry.extra, serials=serials),
            }
            for entry in report.devices
        ],
        "errors": [redact_text(item, serials=serials) for item in report.errors],
        "log_tail": "logs/padlend-tail.log",
        "redacted": True,
    }
    dumpsys = redact_text(report.dumpsys_input, serials=serials)
    devices = redact_adb_devices(report.devices_raw, serials=serials)
    tail = redact_text(_read_log_tail(), serials=serials)
    _, crash_text = _read_crash_latest()
    crash_text = redact_text(crash_text, serials=serials)
    meta["crash_log"] = "logs/crash-latest.log" if crash_text.strip() else None
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("README.txt", _README)
        archive.writestr("diagnostics.txt", report.to_text())
        archive.writestr("dumpsys-input.txt", dumpsys)
        archive.writestr("adb-devices.txt", devices)
        if tail.strip():
            archive.writestr("logs/padlend-tail.log", tail)
        if crash_text.strip():
            archive.writestr("logs/crash-latest.log", crash_text)
        archive.writestr("meta.json", json.dumps(meta, ensure_ascii=False, indent=2))
    return path


def static_summary() -> str:
    """不跑 adb 的静态摘要（托盘"复制诊断"用，绝不阻塞）。"""
    report = DiagnosticsReport(collected_at=datetime.now().isoformat(timespec="seconds"))
    report.server_jar = str(C.SCRCPY_SERVER_JAR)
    try:
        report.server_jar_present = Path(C.SCRCPY_SERVER_JAR).is_file()
    except OSError:
        report.server_jar_present = False
    report.errors.append("未运行 adb 采集（仅静态摘要）")
    return report.to_text()


# ---------------------------------------------------------------------- 对话框
class DiagnosticsDialog(QDialog):
    """诊断面板：异步采集、展示、导出 zip。"""

    def __init__(
        self,
        parent: QWidget | None = None,
        *,
        server_jar: Path | None = None,
        timeout_ms: int = _DEFAULT_TIMEOUT_MS,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("diagnosticsDialog")
        self.setWindowTitle("诊断信息")
        self.resize(780, 620)
        self._server_jar = Path(server_jar) if server_jar is not None else Path(
            C.SCRCPY_SERVER_JAR
        )
        self._timeout_ms = int(timeout_ms)
        self._chain: AdbChain | None = None
        self._report: DiagnosticsReport | None = None
        self._build()

    # ------------------------------------------------------------------ 构建
    def _build(self) -> None:
        title = QLabel("诊断信息", self)
        title.setObjectName("sectionTitle")
        hint = QLabel(
            "采集 adb devices / dumpsys input / pointer_speed / UHID 状态；"
            "采集在后台执行，不会卡住界面。导出的 zip 会附带最近 512 KB 日志，"
            "并自动隐藏序列号、IP/MAC、用户名与用户目录路径。",
            self,
        )
        hint.setObjectName("hintText")
        hint.setWordWrap(True)

        self.status_label = QLabel("尚未采集", self)
        self.status_label.setObjectName("mutedText")
        self.status_label.setWordWrap(True)

        self.text_view = QPlainTextEdit(self)
        self.text_view.setObjectName("diagnosticsText")
        self.text_view.setReadOnly(True)
        self.text_view.setLineWrapMode(QPlainTextEdit.LineWrapMode.NoWrap)
        self.text_view.setFont(build_mono_font())
        self.text_view.setPlaceholderText("点击“重新采集”获取诊断信息")

        self.collect_button = QPushButton("重新采集", self)
        self.collect_button.setProperty("accent", True)
        self.export_button = QPushButton("导出 zip…", self)
        self.export_button.setEnabled(False)
        self.close_button = QPushButton("关闭", self)
        self.close_button.setProperty("flat", True)

        buttons = QHBoxLayout()
        buttons.setContentsMargins(0, 0, 0, 0)
        buttons.setSpacing(8)
        buttons.addWidget(self.status_label, 1)
        buttons.addWidget(self.collect_button)
        buttons.addWidget(self.export_button)
        buttons.addWidget(self.close_button)

        card = QFrame(self)
        card.setObjectName("card")
        card_layout = QVBoxLayout(card)
        card_layout.setContentsMargins(12, 12, 12, 12)
        card_layout.setSpacing(8)
        card_layout.addWidget(self.text_view, 1)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(16, 16, 16, 16)
        layout.setSpacing(12)
        layout.addWidget(title)
        layout.addWidget(hint)
        layout.addWidget(card, 1)
        layout.addLayout(buttons)

        self.collect_button.clicked.connect(self.start)
        self.export_button.clicked.connect(self._on_export_clicked)
        self.close_button.clicked.connect(self.reject)

    # ------------------------------------------------------------------ 采集
    def start(self) -> None:
        """启动异步采集（重复调用会重启）。"""
        if self._chain is not None and self._chain.running:
            self._chain.stop()
        self.status_label.setText("正在采集 …")
        self.collect_button.setEnabled(False)
        self.export_button.setEnabled(False)
        chain = AdbChain(DIAGNOSTIC_STEPS, timeout_ms=self._timeout_ms, parent=self)
        chain.progress.connect(self.status_label.setText)
        chain.done.connect(self._on_done)
        self._chain = chain
        chain.start()

    def report(self) -> DiagnosticsReport | None:
        return self._report

    def diagnostics_text(self) -> str:
        return self.text_view.toPlainText()

    def _on_done(self, ok: bool, results: object) -> None:
        items = list(results) if isinstance(results, (list, tuple)) else []
        report = build_report(
            [item for item in items if isinstance(item, CommandResult)],
            server_jar=self._server_jar,
        )
        self._report = report
        self.text_view.setPlainText(report.to_text())
        device = report.primary_device()
        state = f"{device.serial}（{device.state}）" if device else "未检测到设备"
        self.status_label.setText(f"采集完成 · {state}" if ok else f"采集完成（有错误）· {state}")
        self.collect_button.setEnabled(True)
        self.export_button.setEnabled(True)
        log.info(
            "diagnostics collected ok=%s device_state=%s",
            ok,
            device.state if device else "none",
        )

    def _on_export_clicked(self) -> None:
        self.export()

    def export(self) -> Path | None:
        if self._report is None:
            return None
        try:
            path = export_report(self._report)
        except OSError as exc:
            log.error("export diagnostics failed: %r", exc)
            self.status_label.setText(f"导出失败：{exc}")
            return None
        self.status_label.setText(f"已导出：{path}")
        try:
            QGuiApplication.clipboard().setText(str(path))
        except RuntimeError:  # 无剪贴板后端（部分 offscreen）
            log.debug("clipboard unavailable")
        return path

    def closeEvent(self, event: Any) -> None:  # noqa: N802 - Qt 命名
        if self._chain is not None and self._chain.running:
            self._chain.stop()
        super().closeEvent(event)
