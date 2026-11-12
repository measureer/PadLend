"""诊断采集的纯业务层（1.1.0 起无 Qt 依赖）。

迁移说明：原 ``ui/diagnostics.py`` 把脱敏、解析、报告组装和 zip 导出与
``QProcess`` / ``QDialog`` 混在一个文件里，Tauri 侧的 Python 服务无法复用
（引入它就得带上 PySide6）。1.1.0 把这些纯逻辑独立到 ``core``，
旧 Qt 层与 ``ui/`` 已整体删除；本模块由 ``service/api.py`` 直接调用。

唯一数据来源仍是 ``constants`` / 配置文件 / 日志文件，行为与旧版逐字一致。
"""

from __future__ import annotations

import json
import re
import sys
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

import constants as C

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
    """把采集结果整理成报告。"""
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
    """把报告写成 zip，返回文件路径（默认脱敏 + 最近 512 KB 日志）。"""
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
    """不跑 adb 的静态摘要（托盘「复制诊断」用，绝不阻塞）。"""
    report = DiagnosticsReport(collected_at=datetime.now().isoformat(timespec="seconds"))
    report.server_jar = str(C.SCRCPY_SERVER_JAR)
    try:
        report.server_jar_present = Path(C.SCRCPY_SERVER_JAR).is_file()
    except OSError:
        report.server_jar_present = False
    report.errors.append("未运行 adb 采集（仅静态摘要）")
    return report.to_text()
