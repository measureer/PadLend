"""adb 封装（设计文档 6.1.1 / 8.2 / 附录 C.1、D）。

只做三件事：找到 adb、把命令跑起来、把失败翻译成 :class:`PadLendError`。

约定（不要改）：

* 路径解析顺序：``PADLEND_ADB`` → ``vendor/adb/platform-tools/adb.exe`` →
  ``PATH`` → 常见 SDK 安装目录。全部找不到才抛 ``E100``。
* 子进程一律**列表参数**（绝不 ``shell=True``），并注入 ``MSYS_NO_PATHCONV=1``
  （附录 D：Git Bash 会把 ``/sdcard/x`` 改写成 Windows 路径）。
* 超时统一取 :mod:`constants`，不在调用点写魔法数字。

本模块 import 时**不**解析路径、**不**启动进程，纯类定义。
"""

from __future__ import annotations

import logging
import os
import re
import shutil
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

import constants as C
from core.errors import ErrorCode, PadLendError

if TYPE_CHECKING:
    from collections.abc import Callable, Sequence

LOG = logging.getLogger("tablet.adb")

# Android 设备的 adb 状态值（``adb devices`` 第二列）。
STATE_DEVICE = "device"
STATE_UNAUTHORIZED = "unauthorized"
STATE_OFFLINE = "offline"

# Windows 上隐藏 adb 的控制台窗口（打包后尤其重要）。
_CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0) if sys.platform == "win32" else 0

# vendor 目录里可能出现的两种摆放方式。
_VENDOR_ADB_CANDIDATES = (
    C.VENDOR_DIR / "adb" / "platform-tools" / C.ADB_EXE,
    C.VENDOR_DIR / "adb" / C.ADB_EXE,
)

# 发版审计 P1：kill-server 只能杀 PadLend 自己拉起的 server，否则会把用户
# 正在用的 Android Studio / logcat 会话一起掐掉。
_ADB_SERVER_DEFAULT_PORT = 5037
_SENSITIVE_SUBCOMMANDS = frozenset({"pair"})
# 设备上报的网卡名会拼进远程 shell 字符串，必须先过白名单（防注入）。
_IFACE_RE = re.compile(r"^[A-Za-z0-9_.:-]{1,32}$")

_SERVER_OWNED_BY_PADLEND: bool | None = None


def server_owned_by_padlend() -> bool | None:
    """本进程是否启动过 adb server：None=还没执行过会拉起 server 的命令。"""
    return _SERVER_OWNED_BY_PADLEND


def _adb_server_port() -> int:
    raw = os.environ.get("ANDROID_ADB_SERVER_PORT", "")
    try:
        return int(raw) if raw else _ADB_SERVER_DEFAULT_PORT
    except ValueError:
        return _ADB_SERVER_DEFAULT_PORT


def _adb_server_listening(timeout: float = 0.2) -> bool:
    port = _adb_server_port()
    for host in ("127.0.0.1", "::1"):
        try:
            with socket.create_connection((host, port), timeout=timeout):
                return True
        except OSError:
            continue
    return False


def _note_server_ownership(args: Sequence[str]) -> None:
    global _SERVER_OWNED_BY_PADLEND
    if _SERVER_OWNED_BY_PADLEND is not None:
        return
    if not args or args[0] in {"version", "kill-server", "start-server"}:
        return
    _SERVER_OWNED_BY_PADLEND = not _adb_server_listening()


def normalize_address(address: str | None) -> str:
    """把用户输入的无线调试地址整成 ``host:port``。

    允许粘贴 ``adb connect 192.168.1.8:5555`` 整行、带 ``adb://`` 前缀，
    或只给 IP（补默认端口 5555）。无法解析时返回空串，由调用方报错。
    """
    text = str(address or "").strip().strip("'\"")
    if not text:
        return ""
    for prefix in ("adb://", "tcp://", "adb connect ", "adb pair "):
        if text.lower().startswith(prefix):
            text = text[len(prefix) :].strip()
    # 用户可能粘了整行 "adb connect 1.2.3.4:5555  1.2.3.4:5555"；取第一个 token。
    text = text.split()[0] if text.split() else ""
    text = text.rstrip("/")
    if not text or " " in text:
        return ""
    host, sep, port = text.partition(":")
    if not host:
        return ""
    if not sep:
        return f"{text}:{C.WIRELESS_DEFAULT_PORT}"
    return text if port.isdigit() else ""


@dataclass(frozen=True, slots=True)
class DeviceEntry:
    """``adb devices -l`` 的一行。"""

    serial: str
    state: str
    product: str = ""
    model: str = ""
    device: str = ""
    transport_id: str = ""

    @property
    def online(self) -> bool:
        """``device`` 状态才是可用的；未授权/离线都不可用。"""
        return self.state == STATE_DEVICE

    @property
    def is_wireless(self) -> bool:
        """无线 adb 有两种 serial：``ip:port`` 和 mDNS 的 ``adb-..._tcp``。"""
        return ":" in self.serial or "_adb-tls-connect" in self.serial or ".tcp" in self.serial

    @property
    def is_usb(self) -> bool:
        return not self.is_wireless


def _candidate_paths(explicit: str | Path | None) -> list[Path]:
    """按优先级列出候选 adb 路径（存在的才算数，由调用方过滤）。"""
    out: list[Path] = []
    if explicit:
        out.append(Path(explicit))
    env_path = os.environ.get("PADLEND_ADB", "").strip()
    if env_path:
        out.append(Path(env_path))
    out.extend(_VENDOR_ADB_CANDIDATES)

    which = shutil.which("adb")
    if which:
        out.append(Path(which))
    which_exe = shutil.which(C.ADB_EXE)
    if which_exe:
        out.append(Path(which_exe))

    local_appdata = os.environ.get("LOCALAPPDATA", "")
    sdk_roots = (
        os.environ.get("ANDROID_HOME", ""),
        os.environ.get("ANDROID_SDK_ROOT", ""),
        str(Path(local_appdata) / "Android" / "Sdk") if local_appdata else "",
        str(Path.home() / "AppData" / "Local" / "Android" / "Sdk"),
    )
    for root in sdk_roots:
        if root:
            out.append(Path(root) / "platform-tools" / C.ADB_EXE)

    out.extend(
        [
            Path(r"C:\Android\platform-tools") / C.ADB_EXE,
            Path(r"C:\Program Files\Android\platform-tools") / C.ADB_EXE,
            Path(r"C:\Program Files (x86)\Android\platform-tools") / C.ADB_EXE,
        ]
    )
    return out


def resolve_adb(explicit: str | Path | None = None) -> Path:
    """找到可执行的 adb；找不到抛 ``E100``（8.4）。"""
    for candidate in _candidate_paths(explicit):
        try:
            if candidate.is_file():
                return candidate
        except OSError:  # pragma: no cover - 权限异常极罕见
            continue
    tried = ", ".join(str(p) for p in _candidate_paths(explicit)[:8])
    raise PadLendError(
        ErrorCode.ADB_NOT_FOUND,
        f"已尝试: {tried}；可设置环境变量 PADLEND_ADB 或把 platform-tools 放到 vendor/adb/",
    )


def _run_env() -> dict[str, str]:
    """子进程环境：附录 D 的路径转换坑 + UTF-8 输出。"""
    env = os.environ.copy()
    env["MSYS_NO_PATHCONV"] = "1"
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


class Adb:
    """单个 adb 可执行文件的薄封装（可绑定 serial，也可不绑）。"""

    def __init__(
        self,
        serial: str | None = None,
        adb_path: str | Path | None = None,
    ) -> None:
        self.serial = serial
        self._explicit_path = adb_path
        self._path: Path | None = None
        self._version: str | None = None
        self._lock_note = "发送只由 T5 调用；启停在 T6（10.4）"

    # -- 路径 --------------------------------------------------------------
    @property
    def path(self) -> Path:
        """惰性解析：构造 Adb 不会碰文件系统，也不会因为没有 adb 而失败。"""
        if self._path is None:
            self._path = resolve_adb(self._explicit_path)
            LOG.debug("adb 路径: %s", self._path)
        return self._path

    def version(self) -> str:
        """``adb version`` 首行；用于 ``_assert_adb`` 与诊断。"""
        if self._version is None:
            out = self._text(["version"], timeout=C.ADB_SHELL_TIMEOUT)
            self._version = out.strip().splitlines()[0] if out.strip() else ""
        return self._version

    def with_serial(self, serial: str) -> Adb:
        """返回绑定了 serial 的新实例（共享已解析的路径）。"""
        clone = Adb(serial=serial, adb_path=self._explicit_path)
        clone._path = self._path
        clone._version = self._version
        return clone

    # -- 底层执行 ----------------------------------------------------------
    def _base_args(self) -> list[str]:
        args = [str(self.path)]
        if self.serial:
            args += ["-s", self.serial]
        return args

    def _run(
        self,
        args: Sequence[str],
        *,
        timeout: float,
        code: ErrorCode = ErrorCode.ADB_NOT_FOUND,
        check: bool = True,
    ) -> subprocess.CompletedProcess[bytes]:
        """执行一次 adb 命令；失败按 ``code`` 映射成 PadLendError。"""
        cmd = self._base_args() + list(args)
        _note_server_ownership(args)
        LOG.debug("adb: %s", " ".join(self._display_args(args)))
        try:
            proc = subprocess.run(  # noqa: S603 - 列表参数，无 shell
                cmd,
                capture_output=True,
                text=False,
                timeout=timeout,
                env=_run_env(),
                creationflags=_CREATE_NO_WINDOW,
                check=False,
            )
        except FileNotFoundError as exc:
            self._path = None  # 下次重新解析
            raise PadLendError(
                ErrorCode.ADB_NOT_FOUND, f"adb 不可执行: {cmd[0]}", cause=exc
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise PadLendError(
                code,
                f"adb 超时 {timeout:g}s: {' '.join(self._display_args(args))}",
                cause=exc,
            ) from exc
        except OSError as exc:
            raise PadLendError(ErrorCode.ADB_NOT_FOUND, f"无法启动 adb: {exc}", cause=exc) from exc

        if check and proc.returncode != 0:
            raise PadLendError(code, self._describe_failure(args, proc))
        return proc

    @staticmethod
    def _display_args(args: Sequence[str]) -> list[str]:
        """日志/异常里使用的参数副本：pair 的配对码打码。"""
        values = [str(item) for item in args]
        if values and values[0] in _SENSITIVE_SUBCOMMANDS and len(values) >= 3:
            values[-1] = "<code>"
        return values

    @staticmethod
    def _decode(raw: bytes) -> str:
        # adb 在 Windows 上可能混入 GBK 提示，替换而不是抛错。
        return raw.decode("utf-8", errors="replace")

    @classmethod
    def _describe_failure(
        cls, args: Sequence[str], proc: subprocess.CompletedProcess[bytes]
    ) -> str:
        err = cls._decode(proc.stderr).strip()
        out = cls._decode(proc.stdout).strip()
        detail = err or out or f"exit {proc.returncode}"
        return f"{' '.join(cls._display_args(args))} → {detail}"

    def _text(
        self,
        args: Sequence[str],
        *,
        timeout: float,
        code: ErrorCode = ErrorCode.ADB_NOT_FOUND,
        check: bool = True,
    ) -> str:
        proc = self._run(args, timeout=timeout, code=code, check=check)
        return self._decode(proc.stdout)

    # -- 设备 --------------------------------------------------------------
    _KNOWN_STATES = frozenset(
        {
            "device",
            "offline",
            "unauthorized",
            "bootloader",
            "recovery",
            "sideload",
            "rescue",
            "host",
            "connecting",
            "authorizing",
            "unknown",
            "no",  # "no permissions (...)"
        }
    )

    def devices(self) -> list[DeviceEntry]:
        """枚举设备（含未授权/离线，由调用方裁决）。

        ``adb devices -l`` 偶尔会残留或打印畸形行（例如 mDNS 串名冲突后出现
        ``serial (2)._adb-tls-connect._tcp device ...``，state 字段其实是串名的
        碎片）。这里只接受已知 state，并按 serial 去重，避免同一台平板在
        设备列表里出现两次。
        """
        out = self._text(
            ["devices", "-l"], timeout=C.ADB_SHELL_TIMEOUT, code=ErrorCode.DEVICE_OFFLINE
        )
        entries: list[DeviceEntry] = []
        seen: set[str] = set()
        for raw in out.splitlines():
            line = raw.strip()
            if not line or line.startswith("*") or line.lower().startswith("list of devices"):
                continue
            parts = line.split()
            if len(parts) < 2:
                continue
            # serial 本身可能含空格：ADB 给重名 mDNS 服务加 " (2)" 后缀，
            # 此时 ``adb devices -l`` 是
            #   adb-xxx (2)._adb-tls-connect._tcp device product:...
            # 不能把 parts[1] 直接当 state。这里找已知 state 的位置，它之前的
            # 全部 token 拼回 serial（adb -s 接受带空格的 serial，已实测）。
            state_idx = -1
            state = ""
            attr_start = 0
            for i, token in enumerate(parts):
                if token == "no" and i + 1 < len(parts) and parts[i + 1] == "permissions":
                    state_idx, state, attr_start = i, "no permissions", i + 2
                    break
                if token in self._KNOWN_STATES and token != "no":
                    state_idx, state, attr_start = i, token, i + 1
                    break
            if state_idx <= 0:
                LOG.debug("忽略畸形 adb 设备行: %r", line)
                continue
            serial = " ".join(parts[:state_idx])
            if serial in seen:
                continue
            seen.add(serial)
            attrs: dict[str, str] = {}
            for token in parts[attr_start:]:
                key, sep, value = token.partition(":")
                if sep:
                    attrs[key] = value
            entries.append(
                DeviceEntry(
                    serial=serial,
                    state=state,
                    product=attrs.get("product", ""),
                    model=attrs.get("model", ""),
                    device=attrs.get("device", ""),
                    transport_id=attrs.get("transport_id", ""),
                )
            )

        # 同一台平板可能同时以 mDNS 串名和 ip:port 出现；优先保留无空格、
        # 可直接用作 ``adb -s`` 的干净 serial，避免设备页出现两个相同型号。
        clean = {
            (e.model, e.device)
            for e in entries
            if e.online and " " not in e.serial and (e.model or e.device)
        }
        result: list[DeviceEntry] = []
        for entry in entries:
            if (
                entry.online
                and " " in entry.serial
                and (entry.model, entry.device) in clean
            ):
                LOG.debug("忽略 mDNS 重复设备行: %r", entry.serial)
                continue
            result.append(entry)
        return result

    def online_devices(self) -> list[DeviceEntry]:
        return [d for d in self.devices() if d.online]

    def select_device(self, serial: str | None = None, *, prefer_usb: bool = True) -> DeviceEntry:
        """按 6.1.1 选设备：指定 serial → USB → 无线。"""
        entries = self.devices()
        online = [e for e in entries if e.online]
        if serial:
            for entry in entries:
                if entry.serial == serial:
                    if entry.online:
                        return entry
                    raise PadLendError(
                        ErrorCode.DEVICE_OFFLINE,
                        f"设备 {serial} 状态为 {entry.state}（需在平板确认 USB 调试授权）",
                    )
            # 无线调试重启后 mDNS 串名会带 " (2)" 后缀，配置里的旧 serial
            # 就不存在了。只有一台在线设备时直接改用它，避免“明明连着却
            # 一直 FAULT/检测不到”。
            if len(online) == 1:
                LOG.warning("配置设备 %s 不存在，改用唯一在线设备 %s", serial, online[0].serial)
                return online[0]
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE, f"找不到设备 {serial}（adb devices 为空？）"
            )
        if not online:
            unauthorized = [e for e in entries if e.state == STATE_UNAUTHORIZED]
            if unauthorized:
                raise PadLendError(
                    ErrorCode.DEVICE_OFFLINE,
                    "平板未授权 USB 调试，请在平板上点『允许』",
                )
            raise PadLendError(ErrorCode.DEVICE_OFFLINE, "没有在线的平板（adb devices 为空）")

        if prefer_usb:
            for entry in online:
                if entry.is_usb:
                    return entry
        return online[0]

    def wait_for_device(
        self,
        timeout: float = C.INITIAL_RETRY_WINDOW_S,
        *,
        cancel: Callable[[], bool] | None = None,
    ) -> DeviceEntry:
        """首连重试：窗口内每 2s 扫一次（6.1.1），窗口过后交给指数退避。

        ``cancel`` 为真时立即放弃等待（GUI 退出时由 ``TabletSession.stop()``
        置位），避免用户关窗后还要等满 40s。取消不是设备错误，但仍以
        ``E101 DEVICE_OFFLINE`` 形式抛出，交由上层按"无设备"处理。
        """
        deadline = time.monotonic() + max(0.0, timeout)
        last: PadLendError | None = None
        while True:
            if cancel is not None and cancel():
                raise PadLendError(ErrorCode.DEVICE_OFFLINE, "等待设备已取消")
            try:
                return self.select_device(self.serial)
            except PadLendError as exc:
                last = exc
                if time.monotonic() >= deadline:
                    break
                if cancel is not None and cancel():
                    raise PadLendError(ErrorCode.DEVICE_OFFLINE, "等待设备已取消") from exc
                time.sleep(C.INITIAL_RETRY_INTERVAL_S)
        assert last is not None
        raise last

    # -- 无线调试 ----------------------------------------------------------
    def mdns_services(self) -> list[str]:
        """``adb mdns services`` 的原始行（不含表头）。

        Windows 上的 mDNS 经常完全为空（Bonjour 未运行、多网卡/VPN 都会干扰），
        所以它只能作为「顺便发现」的加分项，不能用它替代手动添加地址。
        """
        try:
            out = self._text(
                ["mdns", "services"], timeout=C.ADB_SHELL_TIMEOUT, check=False
            )
        except PadLendError as exc:
            LOG.debug("adb mdns services 失败（忽略）: %s", exc.detail)
            return []
        lines: list[str] = []
        for raw in out.splitlines():
            line = raw.strip()
            if not line or line.lower().startswith("list of discovered mdns services"):
                continue
            lines.append(line)
        return lines

    def connect(self, address: str, *, timeout: float = C.ADB_CONNECT_TIMEOUT) -> str:
        """``adb connect host:port``（无线调试）。

        成功返回 adb 的提示行（``connected to ...`` / ``already connected ...``），
        失败抛 ``E101``。调用方应在后台线程执行：TCP 探测最坏要等满超时。
        """
        target = normalize_address(address)
        if not target:
            raise PadLendError(ErrorCode.DEVICE_OFFLINE, "请填写平板上的 IP 地址与端口")
        LOG.info("adb connect %s", target)
        proc = self._run(["connect", target], timeout=timeout, check=False)
        text = (self._decode(proc.stdout) + self._decode(proc.stderr)).strip()
        lowered = text.lower()
        _FAIL_MARKERS = (
            "failed to connect",
            "cannot connect",
            "unable to connect",
            "no route to host",
        )
        if any(marker in lowered for marker in _FAIL_MARKERS) or not text:
            detail = text.splitlines()[-1] if text else f"exit {proc.returncode}"
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE,
                f"连接 {target} 失败：{detail}（确认平板与电脑在同一 Wi-Fi、且已开启无线调试）",
            )
        return text

    def pair(self, address: str, code: str, *, timeout: float = C.ADB_PAIR_TIMEOUT) -> str:
        """``adb pair host:port code``（Android 11+ 无线调试配对）。"""
        target = normalize_address(address)
        pairing = str(code or "").strip()
        if not target or not pairing:
            raise PadLendError(ErrorCode.DEVICE_OFFLINE, "配对需要同时填写地址与 6 位配对码")
        LOG.info("adb pair %s", target)
        proc = self._run(["pair", target, pairing], timeout=timeout, check=False)
        text = (self._decode(proc.stdout) + self._decode(proc.stderr)).strip()
        if "successfully paired" not in text.lower():
            detail = text.splitlines()[-1] if text else f"exit {proc.returncode}"
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE,
                f"配对 {target} 失败：{detail}（配对端口与连接端口不同，"
                "请填「使用配对码配对设备」窗口里的地址）",
            )
        return text

    def disconnect(
        self, address: str | None = None, *, timeout: float = C.ADB_CONNECT_TIMEOUT
    ) -> str:
        """``adb disconnect [host:port]``；断开失败不抛（退出清理用）。"""
        args = ["disconnect"]
        target = normalize_address(address) if address else ""
        if target:
            args.append(target)
        try:
            proc = self._run(args, timeout=timeout, check=False)
        except PadLendError as exc:
            LOG.debug("adb disconnect 失败（忽略）: %s", exc.detail)
            return ""
        return (self._decode(proc.stdout) + self._decode(proc.stderr)).strip()

    def kill_server(self, *, timeout: float = C.ADB_SERVER_TIMEOUT) -> bool:
        """停掉 adb server，但只在 PadLend 自己拉起过它时才执行。

        返回是否真的执行了 kill-server。别的工具（Android Studio / logcat）
        在共用 5037 端口时不会被我们掐断。
        """
        global _SERVER_OWNED_BY_PADLEND
        if _SERVER_OWNED_BY_PADLEND is not True:
            LOG.info("adb.kill_server skipped reason=not_owned")
            return False
        try:
            self._run(["kill-server"], timeout=timeout, check=False)
            LOG.info("adb server 已停止（由 PadLend 启动）")
        except PadLendError as exc:
            LOG.debug("adb kill-server 失败（忽略）: %s", exc.detail)
        finally:
            _SERVER_OWNED_BY_PADLEND = False
        return True

    # -- shell -------------------------------------------------------------
    def shell(self, *args: str, timeout: float = C.ADB_SHELL_TIMEOUT) -> str:
        """远端 shell，返回 stdout（失败抛 ``E100``，除非调用方改用 shell_result）。"""
        command = " ".join(args)
        return self._text(["shell", command], timeout=timeout).strip()

    def shell_result(self, *args: str, timeout: float = C.ADB_SHELL_TIMEOUT) -> tuple[int, str]:
        """不抛异常的 shell：返回 ``(returncode, stdout)``。

        清理校验要用它——``grep -c`` 计数为 0 时退出码是 1，不是错误。
        """
        command = " ".join(args)
        proc = self._run(["shell", command], timeout=timeout, check=False)
        return proc.returncode, self._decode(proc.stdout)

    def wifi_mac(self, serial: str | None = None, *, timeout: float = 8.0) -> str:
        """平板 Wi-Fi 网卡的 MAC（``aa:bb:cc:dd:ee:ff``），拿不到返回空串。

        记住 MAC 是为了在路由器换了 IP 之后还能在局域网里找到它（0.3.13）：
        ``/sys/class/net/wlan0/address`` 是最稳的来源，有些设备网卡叫别的名字，
        再退一步扫 ``/sys/class/net`` 里带 ``address`` 的无线接口。
        """
        adb = self.with_serial(serial) if serial else self
        candidates = ("wlan0", "eth0", "wlan1")
        for iface in candidates:
            try:
                _rc, out = adb.shell_result(
                    "cat", f"/sys/class/net/{iface}/address", timeout=timeout
                )
            except Exception as exc:  # noqa: BLE001 - 拿不到就当没有
                LOG.debug("wifi_mac %s failed: %r", iface, exc)
                continue
            mac = out.strip().lower()
            if re.fullmatch(r"[0-9a-f]{2}(:[0-9a-f]{2}){5}", mac):
                return mac
        try:
            _rc, listing = adb.shell_result("ls", "/sys/class/net", timeout=timeout)
        except Exception:  # noqa: BLE001
            return ""
        for iface in listing.split():
            if iface in candidates or not _IFACE_RE.fullmatch(iface):
                continue
            try:
                _rc, out = adb.shell_result(
                    "cat", f"/sys/class/net/{iface}/address", timeout=timeout
                )
            except Exception:  # noqa: BLE001
                continue
            mac = out.strip().lower()
            if re.fullmatch(r"[0-9a-f]{2}(:[0-9a-f]{2}){5}", mac):
                return mac
        return ""

    def shell_ok(self, *args: str, timeout: float = C.ADB_SHELL_TIMEOUT) -> bool:
        """远端 shell 是否成功（清理步骤用，绝不抛）。"""
        try:
            rc, _ = self.shell_result(*args, timeout=timeout)
            return rc == 0
        except PadLendError:
            return False

    def popen_shell(self, command: str) -> subprocess.Popen[bytes]:
        """后台跑一条远端命令并返回本地 adb 进程（scrcpy-server 需要长驻）。"""
        cmd = self._base_args() + ["shell", command]
        LOG.debug("adb (bg): %s", command)
        try:
            return subprocess.Popen(  # noqa: S603 - 列表参数，无 shell
                cmd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=_run_env(),
                creationflags=_CREATE_NO_WINDOW,
            )
        except OSError as exc:
            raise PadLendError(
                ErrorCode.SERVER_START_FAILED,
                f"无法启动服务端进程: {exc}",
                cause=exc,
            ) from exc

    def popen_shell_stream(self, command: str) -> subprocess.Popen[str]:
        """长驻远端命令，逐行读取 stdout（朝向监听用）。

        与 :meth:`popen_shell` 的区别是 stdout 走管道且合并 stderr；调用方
        负责在结束时 ``terminate()`` 并 ``wait()``。设备掉线时进程会 EOF。
        """
        cmd = self._base_args() + ["shell", command]
        LOG.debug("adb (stream): %s", command)
        try:
            return subprocess.Popen(  # noqa: S603 - 列表参数，无 shell
                cmd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
                env=_run_env(),
                creationflags=_CREATE_NO_WINDOW,
            )
        except OSError as exc:
            raise PadLendError(
                ErrorCode.SERVER_START_FAILED,
                f"无法启动 adb 流式命令: {exc}",
                cause=exc,
            ) from exc

    def exec_out(self, *args: str, timeout: float = C.ADB_SHELL_TIMEOUT) -> bytes:
        """``adb exec-out``：二进制安全（screencap 用）。"""
        command = " ".join(args)
        proc = self._run(["exec-out", command], timeout=timeout, check=False)
        return proc.stdout

    # -- 端口转发 ----------------------------------------------------------
    def forward(self, local_port: int, remote: str) -> None:
        """``adb forward tcp:<port> <remote>``；失败抛 ``E104``。"""
        self._run(
            ["forward", f"tcp:{local_port}", remote],
            timeout=C.ADB_FORWARD_TIMEOUT,
            code=ErrorCode.FORWARD_FAILED,
        )

    def forward_remove(self, local_port: int) -> None:
        """撤销转发；端口本来不存在时静默（幂等清理）。"""
        try:
            self._run(
                ["forward", "--remove", f"tcp:{local_port}"],
                timeout=C.ADB_FORWARD_TIMEOUT,
                check=False,
            )
        except PadLendError as exc:
            LOG.debug("forward --remove tcp:%d 失败（忽略）: %s", local_port, exc.detail)

    # -- 文件 --------------------------------------------------------------
    def push(self, local: Path, remote: str) -> None:
        """推送文件；失败抛 ``E103``（服务端 jar 装不上就起不来）。"""
        self._run(
            ["push", str(local), remote],
            timeout=C.ADB_PUSH_TIMEOUT,
            code=ErrorCode.SERVER_START_FAILED,
        )

    def md5(self, remote: str) -> str | None:
        """远端文件 md5；文件不存在或命令不可用时返回 ``None``。"""
        try:
            rc, out = self.shell_result("md5sum", remote, timeout=C.ADB_SHELL_TIMEOUT)
        except PadLendError:
            return None
        if rc != 0:
            # 部分设备只认 busybox/toybox 的 md5，退一步再试。
            try:
                rc, out = self.shell_result("md5", remote, timeout=C.ADB_SHELL_TIMEOUT)
            except PadLendError:
                return None
            if rc != 0:
                return None
        match = re.search(r"\b([0-9a-fA-F]{32})\b", out)
        return match.group(1).lower() if match else None

    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return f"Adb(serial={self.serial!r}, path={self._path})"
