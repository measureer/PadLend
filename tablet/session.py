"""TabletSession 门面（设计文档 6.1 / 8.2，冻结签名）。

上层（Router / supervisor / GUI）只认这一个类：

===============  ====================================================
``start()``      幂等；完成 6.1.2 的九步并返回 :class:`DeviceFacts`
``stop()``       幂等；完成 6.1.3 的六步（未 start 也可安全调用）
``send_mouse()`` 4 字节 UHID 鼠标报告
``send_key()``   8 字节 UHID 键盘全量报告
``send_keycode()`` 兜底：``INJECT_KEYCODE``
``inject_text()`` 中文备选：``INJECT_TEXT``
``health()``     无 adb 调用的健康快照（GUI 线程可调）
===============  ====================================================

线程约定（10.4）：发送只由 sender 线程（T5）调用；``start``/``stop``/``reconnect``
由 supervisor（T6）调用。构造本类**不**碰 adb、不建 socket、不起线程。
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import re
import socket
import subprocess
import threading
import time
from dataclasses import dataclass, replace
from pathlib import Path
from typing import TYPE_CHECKING, Any

import constants as C
from core.errors import ErrorCode, PadLendError
from core.logsetup import log_event

from . import protocol
from .adb import Adb, DeviceEntry
from .audio import AUDIO_SOCKET_POLL_S, AudioRelay
from .locator import split_address, tcp_open
from .server import KILL_PATTERN, ScrcpyServer
from .uhid import keyboard_report, mouse_report

if TYPE_CHECKING:
    from collections.abc import Callable, Sequence
    from pathlib import Path

    from config import Config

LOG = logging.getLogger("tablet.session")

# ``settings get`` 读不到时 Android 返回这个字符串。
_NULL_SETTING = "null"


def _audio_sink_port() -> int | None:
    """Tauri 外壳启动 sidecar 时注入的本地音频端口；源码直跑时没有。"""
    raw = os.environ.get("PADLEND_AUDIO_PORT", "").strip()
    if not raw:
        return None
    try:
        port = int(raw)
    except ValueError:
        LOG.warning("PADLEND_AUDIO_PORT 不是整数: %r", raw)
        return None
    return port if 0 < port < 65_536 else None


# 出厂 pointer_speed（3.1）；恢复时若原值不可读则回退到它。
_FACTORY_POINTER_SPEED = -3

# 保持亮屏（1.2.1）：连接期间临时调大系统熄屏超时，退出/断开时还原。
# 绝不改「开发者选项 → 保持唤醒状态」（stay_on_while_plugged_in）：
# 那个选项只在充电时生效，且会把用户的开发者设置改坏。
# 5 小时覆盖一个完整工作时段，又不会像 int32 最大值那样在异常退出后
# 长期不熄屏；输入事件仍会重置计时。部分 ROM 可能把该值夹到自己的上限。
_SCREEN_TIMEOUT_KEEP_MS = 5 * 60 * 60 * 1000

# 朝向监听（1.0.3）：常驻一条 adb shell，每秒输出一行 Viewport。
# 单独跑 dumpsys input 一次要 300ms 以上，不能塞进 supervisor 的 2s 轮询。
_ORIENTATION_POLL_S = 1.0
_ORIENTATION_RESTART_S = 2.0
# 设备注册表候选端点：先做一次短 TCP 预检，再让 adb connect。
# adb connect 自身要等 20s（常数 ADB_CONNECT_TIMEOUT），失败地址不能把
# 首连拖成 40s 的“假死”；TCP 不通时直接换下一个历史地址。
_CANDIDATE_PROBE_TIMEOUT_S = 0.6
_CANDIDATE_CONNECT_TIMEOUT_S = 4.0
_ORIENTATION_SCRIPT = (
    "while true; do "
    "dumpsys input 2>/dev/null | grep -m1 'Viewport.*orientation='; "
    "sleep 1; "
    "done"
)


def _looks_wireless(serial: str) -> bool:
    """判断 serial 是否是无线 adb 地址（与 ``DeviceEntry.is_wireless`` 同义）。"""
    return (
        ":" in serial
        or "_adb-tls-connect" in serial
        or ".tcp" in serial
    )


# ---------------------------------------------------------------------------
# 数据
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class DeviceFacts:
    """``start()`` 返回的设备事实（8.2 冻结字段，不要增删）。"""

    serial: str
    model: str
    width: int
    height: int
    orientation: int  # 0..3，Surface.ROTATION_*
    xscale: float
    pointer_speed: int
    # ro.serialno：USB 与无线调试时都保持不变，用来按设备记忆布局。
    # 读不到时回退 adb serial；默认值保证旧构造方式仍然可用。
    hardware_serial: str = ""


@dataclass(frozen=True, slots=True)
class HealthReport:
    """健康快照（不含 adb 往返，10.4 允许在 GUI 线程取）。"""

    alive: bool
    socket_ok: bool
    uhid_created: bool
    server_alive: bool
    consecutive_failures: int
    last_error: str
    uptime_s: float
    sent: int


# ---------------------------------------------------------------------------
# 纯解析函数（tools/probe.py 与单测共用）
# ---------------------------------------------------------------------------
def parse_wm_size(text: str) -> tuple[int, int]:
    """解析 ``wm size``。

    优先 ``Override size``（当前逻辑尺寸），否则 ``Physical size``。
    """
    override = re.search(r"Override size:\s*(\d+)x(\d+)", text)
    physical = re.search(r"Physical size:\s*(\d+)x(\d+)", text)
    match = override or physical
    if not match:
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, f"无法解析 wm size 输出: {text!r}")
    return int(match.group(1)), int(match.group(2))


def parse_orientation(dumpsys_input: str, width: int = 0, height: int = 0) -> int:
    """从 ``dumpsys input`` 的 Viewport 行取 orientation（0..3）。"""
    match = re.search(r"Viewport[^\n]*?orientation=(\d)", dumpsys_input)
    if match:
        return int(match.group(1)) & 0x03
    match = re.search(r"orientation=(\d)", dumpsys_input)
    if match:
        return int(match.group(1)) & 0x03
    # 兜底：横向屏基本是 ROTATION_90，其余按 0 处理。
    return 1 if width > height else 0


def oriented_size(width: int, height: int, orientation: int) -> tuple[int, int]:
    """把 ``wm size`` 的自然方向尺寸换成当前旋转下的实际宽高。

    Surface.ROTATION_90/270 时显示宽高互换；估计器与画布都用当前尺寸。
    """
    return (height, width) if orientation & 1 else (width, height)


def parse_xscale(dumpsys_input: str) -> float:
    """从 ``dumpsys input`` 读鼠标 ``XScale``（3.1 / 附录 C.2）。

    优先 ``Cursor Input Mapper`` 段，避免命中别的 mapper；读不到返回 1.0。
    """
    block = dumpsys_input
    mapper = re.search(r"Cursor Input Mapper:(.*?)(?:\n\s*\n|\Z)", dumpsys_input, re.S)
    if mapper:
        block = mapper.group(1)
    match = re.search(r"XScale:\s*([0-9]*\.?[0-9]+)", block)
    if match is None:
        match = re.search(r"XScale:\s*([0-9]*\.?[0-9]+)", dumpsys_input)
    if match is None:
        LOG.warning("dumpsys input 中找不到 XScale，暂用 1.0（A1 前必须核实）")
        return 1.0
    try:
        value = float(match.group(1))
    except ValueError:
        return 1.0
    return value if value > 0 else 1.0


def parse_pointer_speed(text: str) -> int:
    """``settings get system pointer_speed`` → int；``null``/异常回退 0。"""
    match = re.search(r"-?\d+", text)
    return int(match.group(0)) if match else 0


def _recv_exactly(sock: socket.socket, size: int) -> bytes:
    """读满 size 字节或抛 ``OSError``（短读视为断连）。"""
    chunks: list[bytes] = []
    remaining = size
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise OSError("对端在握手阶段关闭了连接")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def clean_serial(text: str) -> str:
    """getprop ro.serialno 的清洗：无效值返回空串（调用方回退 adb serial）。"""
    lines = [line.strip() for line in str(text or "").splitlines() if line.strip()]
    if not lines:
        return ""
    value = lines[-1]
    if value.lower() in {"unknown", "null", "0", "none"}:
        return ""
    return value


def read_facts(adb: Adb) -> DeviceFacts:
    """读取 8.2 的 DeviceFacts（附录 C.2 的命令集合）。

    ``adb`` 必须已绑定 serial。
    """
    if not adb.serial:
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, "read_facts 需要一个已绑定的 serial")
    model = adb.shell("getprop", "ro.product.model")
    hardware_serial = clean_serial(adb.shell("getprop", "ro.serialno")) or adb.serial
    natural_w, natural_h = parse_wm_size(adb.shell("wm", "size"))
    dumpsys = adb.shell("dumpsys", "input")
    pointer_speed = parse_pointer_speed(adb.shell("settings", "get", "system", "pointer_speed"))
    orientation = parse_orientation(dumpsys, natural_w, natural_h)
    width, height = oriented_size(natural_w, natural_h, orientation)
    return DeviceFacts(
        serial=adb.serial,
        model=model.strip() or "unknown",
        width=width,
        height=height,
        orientation=orientation,
        xscale=parse_xscale(dumpsys),
        pointer_speed=pointer_speed,
        hardware_serial=hardware_serial,
    )


def probe_diagnostics(adb: Adb) -> dict[str, str]:
    """附录 C.2 的诊断项（tools/probe.py 用，全部 best-effort）。"""
    out: dict[str, str] = {}
    checks: tuple[tuple[str, str], ...] = (
        ("density", "wm density"),
        ("pointer_speed", "settings get system pointer_speed"),
        ("uhid_groups", "id"),
        ("uhid_open", "exec 3<>/dev/uhid && echo OPEN_OK"),
        ("input_scales", 'dumpsys input | grep -E "Pointer Speed:|XScale:|YScale:"'),
    )
    for key, command in checks:
        try:
            out[key] = adb.shell(command).strip()
        except PadLendError as exc:
            out[key] = f"<失败: {exc.detail}>"
    return out


# ---------------------------------------------------------------------------
# TabletSession
# ---------------------------------------------------------------------------
class TabletSession:
    """平板连接层门面（8.2）。构造零副作用。"""

    def __init__(
        self,
        cfg: Config | None = None,
        *,
        adb: Adb | None = None,
        port: int | None = None,
        jar: Path | None = None,
        candidates: Callable[[], Sequence[str]] | None = None,
        verify_facts: Callable[[str, DeviceFacts], bool] | None = None,
    ) -> None:
        self._cfg = cfg
        self._adb = adb if adb is not None else Adb()
        self._candidates = candidates
        self._verify_facts = verify_facts
        self._dev_adb: Adb | None = None
        self._port = port if port is not None else (
            cfg.device.server_port if cfg is not None else C.DEFAULT_SERVER_PORT
        )
        self._jar = jar
        self._server: ScrcpyServer | None = None

        # 生命周期锁（start/stop/reconnect 互斥；RLock 便于内部复用）
        self._lock = threading.RLock()
        # socket 写锁（只有 sender 线程写，但 stop 也要能安全关闭）
        self._send_lock = threading.Lock()
        self._closing = threading.Event()

        self._started = False
        self._facts: DeviceFacts | None = None
        self._sock: socket.socket | None = None
        self._uhid_created = False
        self._sock_broken = False
        # 只在写日志时用来识别"这一帧是按下还是抬起"；平板端收到的是状态快照。
        self._last_mouse_buttons: int | None = None

        self._orig_pointer_speed: int | None = None
        self._orig_screen_timeout: int | None = None

        # 音频通道（1.4.0）：开启后 scrcpy 先 accept audio 再 accept control。
        self._audio_active = False
        self._audio_sink_port = _audio_sink_port()
        self._audio_sock: socket.socket | None = None
        self._audio_relay: AudioRelay | None = None

        # 1.0.3 朝向监听：回调由 Router 注册；线程只在 start 成功后存在。
        self._facts_callback: Callable[[DeviceFacts], None] | None = None
        self._orientation_stop: threading.Event | None = None
        self._orientation_proc: subprocess.Popen[str] | None = None
        self._orientation_thread: threading.Thread | None = None

        # scrcpy 控制通道是双向的：接收线程处理平板剪贴板、ACK 与 UHID 输出。
        self._clipboard_callback: Callable[[str], None] | None = None
        self._receiver_stop: threading.Event | None = None
        self._receiver_thread: threading.Thread | None = None

        self._last_error = ""
        self._consecutive_failures = 0
        self._sent = 0
        self._started_at = 0.0

    # -- 只读属性 ----------------------------------------------------------
    @property
    def adb(self) -> Adb:
        return self._adb

    @property
    def facts(self) -> DeviceFacts | None:
        return self._facts

    @property
    def started(self) -> bool:
        return self._started

    def set_facts_callback(
        self, callback: Callable[[DeviceFacts], None] | None
    ) -> None:
        """注册朝向变化回调（在监听线程里调用，GUI 侧负责转成队列信号）。"""
        self._facts_callback = callback

    def set_clipboard_callback(self, callback: Callable[[str], None] | None) -> None:
        """注册平板剪贴板回调（接收线程调用，GUI 侧负责排队回主线程）。"""
        self._clipboard_callback = callback

    @property
    def port(self) -> int | None:
        return self._port

    @property
    def server(self) -> ScrcpyServer:
        """惰性创建；构造 ScrcpyServer 不碰设备。"""
        if self._server is None:
            self._server = ScrcpyServer(
                self._dev_adb or self._adb, port=self._port, jar=self._jar
            )
        return self._server

    # -- 生命周期 ----------------------------------------------------------
    def start(self) -> DeviceFacts:
        """6.1.2 启动序列；已启动且 socket 健康时直接返回缓存的事实。"""
        with self._lock:
            if self._started and self._facts is not None and self._socket_ok():
                return self._facts
            if self._started or self._sock is not None:
                LOG.info("检测到半启动状态，先清理再重启")
                self._teardown()
                self._facts = None
            self._closing.clear()
            self._started = True  # 先置位：失败时 stop 路径仍会清理
            try:
                facts = self._start_locked()
            except PadLendError as exc:
                self._last_error = exc.detail or exc.code.value
                # 无设备/被占用是用户环境问题，用 warning 即可，不必打栈。
                LOG.warning("启动失败: %s (%s)", self._last_error, exc.code.value)
                self._teardown()
                self._started = False
                raise
            except Exception as exc:  # 兜底：任何未预期异常都转 E900 并清理
                self._last_error = f"{type(exc).__name__}: {exc}"
                LOG.exception("启动时未预期异常")
                self._teardown()
                self._started = False
                raise PadLendError(
                    ErrorCode.UNEXPECTED, self._last_error, cause=exc
                ) from exc
            self._facts = facts
            self._started_at = time.monotonic()
            self._sock_broken = False
            self._consecutive_failures = 0
            self._start_orientation_watch()
            return facts

    def _start_locked(self) -> DeviceFacts:
        entry, facts = self._connect_candidate()
        # 音频开关在本次 transport 生命周期内固定：中途改配置不会让已经按
        # audio=false 启动的 server 突然多出一个 socket。
        self._audio_active = self._audio_wanted()
        # 服务端封装必须绑定到具体设备（多设备时不能落到默认设备）
        self._server = ScrcpyServer(
            self._dev_adb, port=self._port, jar=self._jar, audio=self._audio_active
        )
        LOG.info("使用设备 %s（%s）", entry.serial, entry.model or "unknown")

        # _normalize_pointer_speed() 要按设备序列号落盘兜底状态；以前在
        # start() 末尾才赋值 self._facts，这里先放好，避免保存空 serial。
        self._facts = facts
        self._normalize_pointer_speed()
        self.server.push_if_needed()
        self.server.kill()  # 清旧实例，保证幂等
        self.server.spawn()
        self._port = self.server.forward()
        try:
            self._connect_socket()
        except PadLendError as exc:
            if not self._audio_active:
                raise
            # Android 11 以下或 scrcpy 音频初始化失败时，不能因为可选功能
            # 把整个平板连接拖死；丢弃服务端，按 audio=false 重来一次。
            LOG.warning("音频通道不可用，回退为仅控制通道: %s", exc.detail)
            self._restart_without_audio()
            self._connect_socket()
        self._create_uhid()
        self._start_receiver()
        self._keep_screen_on()
        log_event(
            LOG,
            C.EVENT_TABLET_CONNECT,
            serial=facts.serial,
            size=f"{facts.width}x{facts.height}",
            orientation=facts.orientation,
            xscale=facts.xscale,
            pointer_speed=facts.pointer_speed,
        )
        return facts

    def _restart_without_audio(self) -> None:
        """丢弃音频服务端，按 audio=false 重新启动（服务端每实例只接受一次通道）。"""
        self._close_socket()
        old = self._server
        if old is not None:
            with contextlib.suppress(PadLendError):
                old.kill()
            with contextlib.suppress(PadLendError):
                old.forward_remove()
        self._audio_active = False
        self._server = ScrcpyServer(
            self._dev_adb, port=self._port, jar=self._jar, audio=False
        )
        self._server.spawn()
        self._port = self._server.forward()

    def stop(self) -> None:
        """6.1.3 退出序列；幂等，未 start 时也安全。

        先把 ``_closing`` 置位**再**拿锁：若另一线程正卡在 ``start()`` 的
        40s 等待设备里（它持有生命周期锁），等待循环会很快看到取消标志并
        退出，随后本方法才能拿到锁完成清理（GUI 退出不再假死）。
        """
        self._closing.set()
        # 先停监听线程再拿生命周期锁：线程会在设备上多跑一条 shell，
        # 但不会调用任何需要 _lock 的方法，不会和 stop 互等。
        self._stop_orientation_watch()
        with self._lock:
            if not (self._started or self._sock is not None or self._uhid_created):
                LOG.debug("stop(): 未启动，忽略")
                self._closing.clear()
                return
            self._teardown()
            self._started = False
            self._facts = None
            self._closing.clear()

    def cancel_start(self) -> None:
        """请求中断进行中的 ``start()``（不清理资源，清理仍归 ``stop()``）。

        供 ``Router.stop()`` 在 GUI 退出时尽快解除首连等待；``stop()`` 会在
        锁可用后完成 socket/服务端/pointer_speed 的收尾。
        """
        self._closing.set()

    def reconnect(self, *, attempts: int | None = None) -> DeviceFacts | None:
        """6.1.4：退避 1/2/4/8/16s 重连，成功则重建 UHID 设备。

        返回最新 facts；全部失败返回 ``None``（不抛，交给 supervisor 决定）。
        """
        if not self._started:
            return None
        backoff = C.RECONNECT_BACKOFF_S
        total = attempts if attempts is not None else len(backoff)
        for index in range(max(1, total)):
            delay = backoff[min(index, len(backoff) - 1)]
            if self._closing.wait(delay):
                return None  # stop() 打断
            with self._lock:
                if not self._started:
                    return None
                try:
                    self._restart_transport()
                except PadLendError as exc:
                    self._last_error = exc.detail or exc.code.value
                    LOG.warning("重连第 %d 次失败: %s", index + 1, self._last_error)
                    continue
                log_event(LOG, C.EVENT_TABLET_RECONNECT, attempt=index + 1)
                return self._facts
        LOG.error("重连 %d 次均失败", total)
        return None

    def _restart_transport(self) -> None:
        """只重建服务端 + socket + UHID，不动 pointer_speed/stayon。"""
        self._close_socket()
        server = self.server
        try:
            server.kill()
        except PadLendError as exc:
            LOG.debug("重连时 kill 失败（忽略）: %s", exc.detail)
        server.forward_remove()
        server.push_if_needed()
        server.spawn()
        self._port = server.forward()
        self._connect_socket()
        self._create_uhid()
        self._start_receiver()

    # -- 发送（只由 T5 调用） ----------------------------------------------
    def send_mouse(self, buttons: int, dx: int, dy: int, wheel: int) -> None:
        """4 字节 UHID 鼠标报告（9.3）。"""
        self._ensure_connected()
        value = int(buttons) & 0x1F
        if value != self._last_mouse_buttons:
            # 平板上的"按钮被按了一下"要么来自这条日志，要么与我们无关。
            LOG.info("tablet.mouse.btn 0x%02x dx=%d dy=%d", value, int(dx), int(dy))
            self._last_mouse_buttons = value
        self._send(protocol.uhid_report(C.UHID_MOUSE_ID, mouse_report(buttons, dx, dy, wheel)))

    def send_key(self, modifier: int, usages: Sequence[int]) -> None:
        """8 字节 UHID 键盘**全量**报告（6.7.2：不是增量，否则粘键）。"""
        self._ensure_connected()
        self._send(protocol.uhid_report(C.UHID_KEYBOARD_ID, keyboard_report(modifier, usages)))

    def send_keycode(self, keycode: int, down: bool) -> None:
        """降级路径：``INJECT_KEYCODE``（8.2 / 12.2）。"""
        self._ensure_connected()
        action = protocol.ACTION_DOWN if down else protocol.ACTION_UP
        self._send(protocol.inject_keycode(action, keycode))

    def inject_text(self, text: str) -> None:
        """UTF-8 直注入（6.7.4）。空串直接忽略，避免无意义报文。"""
        if not text:
            return
        self._ensure_connected()
        self._send(protocol.inject_text(text))

    def set_clipboard(self, text: str, *, sequence: int = 0) -> None:
        """写平板剪贴板（6.8）：不敲键盘，因此可以自动同步。"""
        if not text:
            return
        self._ensure_connected()
        self._send(protocol.set_clipboard(text, sequence=sequence))

    def health(self) -> HealthReport:
        """健康快照；不调用 adb（10.4）。"""
        server_alive = self._server.proc_alive if self._server is not None else False
        socket_ok = self._socket_ok()
        return HealthReport(
            alive=self._started and socket_ok and not self._sock_broken,
            socket_ok=socket_ok,
            uhid_created=self._uhid_created,
            server_alive=server_alive,
            consecutive_failures=self._consecutive_failures,
            last_error=self._last_error,
            uptime_s=(time.monotonic() - self._started_at) if self._started_at else 0.0,
            sent=self._sent,
        )

    # -- 12.1 旋转/分辨率变化后重建事实 ------------------------------------
    def refresh_facts(self) -> DeviceFacts:
        """重新读取设备事实（12.1：旋转后必须重建，中心点由调用方重置）。

        不重启服务端、不动 socket；未 start 时抛 ``E101``。
        """
        with self._lock:
            if self._dev_adb is None:
                raise PadLendError(
                    ErrorCode.DEVICE_OFFLINE, "尚未 start，没有可刷新的设备事实"
                )
            facts = read_facts(self._dev_adb)
            self._facts = facts
            LOG.info("设备事实已刷新: %dx%d orientation=%d",
                     facts.width, facts.height, facts.orientation)
            return facts

    # -- 12.1 pointer_speed 复查（supervisor 每 10s 调） --------------------
    def ensure_pointer_speed(self) -> bool:
        """若不是 0 就重新归零（F4：速度系数不可读，只能靠归零）。"""
        if self._dev_adb is None:
            return False
        try:
            current = parse_pointer_speed(
                self._dev_adb.shell("settings", "get", "system", "pointer_speed")
            )
            if current == 0:
                return True
            LOG.warning("pointer_speed 被改为 %d，重新归零", current)
            self._dev_adb.shell_ok("settings", "put", "system", "pointer_speed", "0")
            return parse_pointer_speed(
                self._dev_adb.shell("settings", "get", "system", "pointer_speed")
            ) == 0
        except PadLendError as exc:
            LOG.warning("pointer_speed 复查失败: %s", exc.detail)
            return False

    # -- 内部：设备 --------------------------------------------------------
    def _select_device(self) -> DeviceEntry:
        serial = self._cfg.device.serial if self._cfg is not None else None
        prefer_usb = self._cfg.device.prefer_usb if self._cfg is not None else True
        try:
            self._adb.version()  # E100：adb 不存在时立刻失败
            return self._adb.select_device(serial, prefer_usb=prefer_usb)
        except PadLendError as exc:
            if exc.code is not ErrorCode.DEVICE_OFFLINE:
                raise
            LOG.info("暂无可用设备，%gs 内每 %gs 重试", C.INITIAL_RETRY_WINDOW_S,
                     C.INITIAL_RETRY_INTERVAL_S)
            return self._adb.wait_for_device(
                C.INITIAL_RETRY_WINDOW_S,
                cancel=self._closing.is_set,
            )

    # -- 内部：注册表候选端点（1.2.2） ------------------------------------
    def _candidate_serials(self) -> list[str | None]:
        """当前设备记录里的候选地址；没有 provider 时保持旧的首连等待。"""
        current = self._cfg.device.serial if self._cfg is not None else ""
        if self._candidates is None:
            return [current or None]
        try:
            raw = list(self._candidates())
        except Exception:  # 注册表/回调故障不能阻止手动连接
            LOG.exception("读取设备候选地址失败，回退当前地址")
            raw = []
        result: list[str | None] = []
        seen: set[str] = set()
        for value in raw:
            text = str(value or "").strip()
            if not text or text.casefold() in seen:
                continue
            seen.add(text.casefold())
            result.append(text)
        # 空注册表/首次启动仍走 _select_device 的 40s 首连窗口。
        return result[:8] if result else [current or None]

    def _select_candidate(self, serial: str | None) -> DeviceEntry:
        """只认注册表给出的地址；不再接受“唯一在线设备”兜底。"""
        if serial is None or self._candidates is None:
            return self._select_device()
        self._adb.version()
        entry = next((item for item in self._adb.devices()
                      if item.serial == serial), None)
        if entry is not None and entry.online:
            return entry
        if not _looks_wireless(serial):
            detail = f"USB 设备 {serial} 未出现在 adb devices"
            if entry is not None:
                detail = f"USB 设备 {serial} 当前状态是 {entry.state}"
            raise PadLendError(ErrorCode.DEVICE_OFFLINE, detail)

        host, port = split_address(serial)
        if host and port and not tcp_open(
            host, port, timeout=_CANDIDATE_PROBE_TIMEOUT_S
        ):
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE,
                f"{serial} 的 TCP 端口不可达（已跳过，避免等待 adb connect 超时）",
            )
        try:
            self._adb.connect(serial, timeout=_CANDIDATE_CONNECT_TIMEOUT_S)
        except PadLendError as exc:
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE,
                f"候选地址 {serial} 连接失败：{exc.detail}",
                cause=exc,
            ) from exc
        entry = next((item for item in self._adb.devices()
                      if item.serial == serial), None)
        if entry is not None and entry.online:
            return entry
        state = entry.state if entry is not None else "未出现在 adb devices"
        raise PadLendError(
            ErrorCode.DEVICE_OFFLINE,
            f"候选地址 {serial} 连接后状态为 {state}（若为 unauthorized，请在平板上允许 USB 调试）",
        )

    def _connect_candidate(self) -> tuple[DeviceEntry, DeviceFacts]:
        """按候选顺序连接并校验身份；只返回读得到 facts 的设备。"""
        serials = self._candidate_serials()
        errors: list[PadLendError] = []
        for serial in serials:
            try:
                entry = self._select_candidate(serial)
                self._dev_adb = self._adb.with_serial(entry.serial)
                facts = read_facts(self._dev_adb)
                verified = True
                if self._verify_facts is not None:
                    try:
                        verified = bool(self._verify_facts(entry.serial, facts))
                    except Exception as exc:  # 校验器故障按“未通过”处理
                        LOG.exception("设备身份校验器异常")
                        verified = False
                        errors.append(PadLendError(
                            ErrorCode.DEVICE_OFFLINE,
                            f"{entry.serial} 身份校验异常：{exc}",
                            cause=exc,
                        ))
                if not verified:
                    if not errors:
                        errors.append(PadLendError(
                            ErrorCode.DEVICE_OFFLINE,
                            f"{entry.serial} 的连接结果不是注册表里的同一台设备",
                        ))
                    self._dev_adb = None
                    continue
                return entry, facts
            except PadLendError as exc:
                if exc.code is not ErrorCode.DEVICE_OFFLINE:
                    raise
                errors.append(exc)
                self._dev_adb = None
                if len(serials) > 1:
                    LOG.info("候选 %s 不可用：%s", serial or "默认设备", exc.detail)
        if errors:
            last = errors[-1]
            detail = last.detail or last.code.value
            if len(errors) > 1:
                detail = f"{detail}（已尝试 {len(errors)} 个地址）"
            raise PadLendError(last.code, detail, cause=last.cause) from last
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, "没有可用的设备地址")

    def _normalize_pointer_speed(self) -> None:
        """记录原值 → 归零（6.1.2）。失败按 E101 处理（设备拒绝写设置）。"""
        assert self._dev_adb is not None
        original = parse_pointer_speed(
            self._dev_adb.shell("settings", "get", "system", "pointer_speed")
        )
        self._orig_pointer_speed = original
        # 崩溃兜底：先把原值落盘，下次启动即使没有 exit.clean 也能还原。
        serial = self._facts.serial if self._facts is not None else ""
        save_pointer_state(serial, original)
        if not self._dev_adb.shell_ok("settings", "put", "system", "pointer_speed", "0"):
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE, "无法把 pointer_speed 归零（settings put 失败）"
            )
        actual = parse_pointer_speed(
            self._dev_adb.shell("settings", "get", "system", "pointer_speed")
        )
        if actual != 0:
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE, f"pointer_speed 归零后仍为 {actual}"
            )
        LOG.info("pointer_speed: %d → 0（退出时还原）", original)

    def _restore_pointer_speed(self) -> None:
        if self._dev_adb is None or self._orig_pointer_speed is None:
            return
        target = self._orig_pointer_speed
        if target == 0:
            # 我们把它归零了；原值就是 0 时无需再写。
            clear_pointer_state()
            return
        if self._dev_adb.shell_ok("settings", "put", "system", "pointer_speed", str(target)):
            LOG.info("pointer_speed 已还原为 %d", target)
            clear_pointer_state()
        else:
            LOG.warning("pointer_speed 还原失败（原值 %d），保留状态待下次启动", target)

    def _keep_screen_on(self) -> None:
        """连接期间临时调大 ``screen_off_timeout``，并记录原值以便退出还原。

        不使用开发者选项的 ``stay_on_while_plugged_in`` / ``svc power stayon``：
        前者只在充电时有效，且会改坏用户的开发者设置；后者是全局电源状态。
        这里只动当前设备的系统熄屏超时，断开时写回原值。
        """
        assert self._dev_adb is not None
        enabled = self._cfg.misc.keep_screen_on if self._cfg is not None else True
        if not enabled:
            return
        try:
            raw = self._dev_adb.shell(
                "settings", "get", "system", "screen_off_timeout"
            ).strip()
        except PadLendError as exc:
            LOG.warning("读取 screen_off_timeout 失败（不致命）：%s", exc.detail)
            return
        if not raw.isdigit():
            LOG.warning("screen_off_timeout 原值不可读（%r），保持系统设置不动", raw)
            return
        original = int(raw)
        if original >= _SCREEN_TIMEOUT_KEEP_MS:
            return
        serial = self._facts.serial if self._facts is not None else ""
        # 先落盘再修改：进程在 put 之后、恢复之前崩溃时，下次启动仍能还原。
        save_screen_timeout_state(serial, original)
        if not self._dev_adb.shell_ok(
            "settings", "put", "system", "screen_off_timeout", str(_SCREEN_TIMEOUT_KEEP_MS)
        ):
            clear_screen_timeout_state()
            LOG.warning("screen_off_timeout 调大失败（不致命）")
            return
        self._orig_screen_timeout = original
        LOG.info("screen_off_timeout: %d → %d（退出时还原）", original, _SCREEN_TIMEOUT_KEEP_MS)

    def _restore_keep_screen_on(self) -> None:
        """还原 system.screen_off_timeout；失败时保留状态文件等下次启动。"""
        if self._dev_adb is None or self._orig_screen_timeout is None:
            return
        original = self._orig_screen_timeout
        if self._dev_adb.shell_ok(
            "settings", "put", "system", "screen_off_timeout", str(original)
        ):
            clear_screen_timeout_state()
            self._orig_screen_timeout = None
            LOG.info("screen_off_timeout 已还原为 %d", original)
        else:
            LOG.warning("screen_off_timeout 还原失败（原值 %d），保留状态待下次启动", original)

    # -- 内部：socket / uhid ----------------------------------------------
    def _audio_wanted(self) -> bool:
        """配置里的音频开关；测试用 FakeConfig 没 misc 时按关闭处理。"""
        misc = getattr(self._cfg, "misc", None)
        return bool(getattr(misc, "forward_audio", False))

    def _handshake_socket(self, label: str) -> socket.socket:
        """连接 forward 端口并校验 1 字节 dummy，返回可用 socket。

        实测 scrcpy 5.0.1：tunnel_forward 下每个通道 accept 后都会先写一个
        0 字节。audio=true 时必须先连 audio、读掉 dummy，再连 control；
        顺序反过来会把 control 当成 audio（DESIGN 9.1 的 4 字节为早期误记）。

        app_process 从 spawn 到真正监听 ``localabstract:scrcpy`` 有可见延迟；
        在这之前 adb forward 会接受 TCP 连接然后立即关闭，读 dummy 得到 EOF。
        因此把"连接/读失败"视为未就绪并在 ``HANDSHAKE_RETRY_TIMEOUT_S`` 内重试；
        一旦读到字节但值不对，则是真正的协议错误，立即失败。
        """
        if self._port is None:
            raise PadLendError(ErrorCode.FORWARD_FAILED, "尚未分配转发端口")
        deadline = time.monotonic() + C.HANDSHAKE_RETRY_TIMEOUT_S
        while True:
            if self._closing.is_set():
                raise PadLendError(ErrorCode.HANDSHAKE_FAILED, "启动已取消")
            sock: socket.socket | None = None
            try:
                sock = socket.create_connection(
                    ("127.0.0.1", self._port), timeout=C.SOCKET_CONNECT_TIMEOUT
                )
                sock.settimeout(C.SOCKET_HANDSHAKE_TIMEOUT)
                dummy = _recv_exactly(sock, len(C.HANDSHAKE_DUMMY))
                if dummy != C.HANDSHAKE_DUMMY:
                    raise PadLendError(
                        ErrorCode.HANDSHAKE_FAILED,
                        f"{label}通道 dummy 字节不符: {dummy.hex()}"
                        f"（期望 {C.HANDSHAKE_DUMMY.hex()}）",
                    )
                sock.settimeout(C.SOCKET_SEND_TIMEOUT)
            except PadLendError:
                if sock is not None:
                    sock.close()
                raise
            except OSError as exc:
                if sock is not None:
                    sock.close()
                if time.monotonic() >= deadline:
                    raise PadLendError(
                        ErrorCode.HANDSHAKE_FAILED,
                        f"连接 {label}通道 127.0.0.1:{self._port} 失败（已重试 "
                        f"{C.HANDSHAKE_RETRY_TIMEOUT_S:.0f}s）: {exc}",
                        cause=exc,
                    ) from exc
                LOG.debug("服务端尚未就绪，重试%s握手: %s", label, exc)
                time.sleep(C.HANDSHAKE_RETRY_INTERVAL_S)
                continue
            LOG.info("%s通道已建立（tcp:%d）", label, self._port)
            return sock

    def _connect_socket(self) -> None:
        """按 audio -> control 顺序建立通道（9.1）。"""
        audio_sock: socket.socket | None = None
        if self._audio_active:
            audio_sock = self._handshake_socket("音频")
        try:
            ctrl = self._handshake_socket("控制")
        except BaseException:
            if audio_sock is not None:
                with contextlib.suppress(OSError):
                    audio_sock.close()
            raise

        self._sock = ctrl
        self._sock_broken = False
        if audio_sock is not None:
            try:
                # relay 用短超时轮询 stop 标志；不能阻塞在 recv 上导致退出卡住。
                audio_sock.settimeout(AUDIO_SOCKET_POLL_S)
                relay = AudioRelay(audio_sock, self._audio_sink_port)
                relay.start()
            except OSError as exc:
                with contextlib.suppress(OSError):
                    audio_sock.close()
                raise PadLendError(
                    ErrorCode.HANDSHAKE_FAILED,
                    f"音频通道初始化失败: {exc}",
                    cause=exc,
                ) from exc
            self._audio_sock = audio_sock
            self._audio_relay = relay
            if relay.degraded:
                LOG.warning("音频播放器不可用，本次连接只保留控制通道")
        LOG.info("控制通道已建立（tcp:%d）", self._port)

    def _close_socket(self) -> None:
        # 先关音频：relay 可能正阻塞/轮询在 recv 上。
        audio_sock = getattr(self, "_audio_sock", None)
        self._audio_sock = None
        if audio_sock is not None:
            with contextlib.suppress(OSError):
                audio_sock.shutdown(socket.SHUT_RDWR)
            with contextlib.suppress(OSError):
                audio_sock.close()
        relay = getattr(self, "_audio_relay", None)
        self._audio_relay = None
        if relay is not None:
            relay.stop()

        stop = self._receiver_stop
        thread = self._receiver_thread
        self._receiver_stop = None
        self._receiver_thread = None
        if stop is not None:
            stop.set()
        with self._send_lock:
            sock, self._sock = self._sock, None
        if sock is not None:
            with contextlib.suppress(OSError):
                sock.shutdown(socket.SHUT_RDWR)
            try:
                sock.close()
            except OSError:  # pragma: no cover - 已关闭
                LOG.debug("socket 关闭时已失效")
        if (
            thread is not None
            and thread.is_alive()
            and thread is not threading.current_thread()
        ):
            thread.join(timeout=2.0)

    def _start_receiver(self) -> None:
        """启动控制通道接收线程；发送与接收共用 socket，但互不持有对方的锁。"""
        if self._receiver_thread is not None and self._receiver_thread.is_alive():
            return
        sock = self._sock
        if sock is None:
            return
        stop = threading.Event()
        thread = threading.Thread(
            target=self._receiver_loop,
            args=(sock, stop),
            name="padlend-receiver",
            daemon=True,
        )
        self._receiver_stop = stop
        self._receiver_thread = thread
        thread.start()
        LOG.debug("控制通道接收监听已启动")

    def _receiver_loop(self, sock: socket.socket, stop: threading.Event) -> None:
        buffer = bytearray()
        while not stop.is_set():
            try:
                chunk = sock.recv(65536)
            except TimeoutError:
                continue
            except OSError as exc:
                self._mark_receiver_broken(sock, stop, f"socket 读失败: {exc}")
                return
            if not chunk:
                self._mark_receiver_broken(sock, stop, "对端关闭了控制通道")
                return
            buffer.extend(chunk)
            try:
                while buffer and not stop.is_set():
                    message, consumed = protocol.deserialize_device_message(buffer)
                    if message is None:
                        break
                    del buffer[:consumed]
                    self._handle_device_message(message)
            except (ValueError, UnicodeError) as exc:
                self._mark_receiver_broken(sock, stop, f"设备消息解析失败: {exc}")
                return
        LOG.debug("控制通道接收监听已停止")

    def _mark_receiver_broken(
        self, sock: socket.socket, stop: threading.Event, detail: str
    ) -> None:
        if stop.is_set() or self._sock is not sock:
            return
        self._sock_broken = True
        self._last_error = detail
        LOG.warning("控制通道接收中断: %s", detail)

    def _handle_device_message(self, message: protocol.DeviceMessage) -> None:
        if message.type == protocol.DEVICE_MSG_CLIPBOARD:
            callback = self._clipboard_callback
            if callback is not None:
                try:
                    callback(message.text)
                except Exception:  # noqa: BLE001 - GUI 回调不能杀死接收线程
                    LOG.exception("平板剪贴板回调失败")
            return
        if message.type == protocol.DEVICE_MSG_ACK_CLIPBOARD:
            LOG.debug("平板剪贴板 ACK sequence=%d", message.sequence)
            return
        if message.type == protocol.DEVICE_MSG_UHID_OUTPUT:
            # 当前鼠标/键盘描述符没有需要回写的输出报告；必须消费以保持流对齐。
            LOG.debug("忽略 UHID 输出 id=%d size=%d", message.uhid_id, len(message.data))

    def _create_uhid(self) -> None:
        """UHID_CREATE 鼠标(id=1) + 键盘(id=2)（9.2 / 9.3）。"""
        self._send(
            protocol.uhid_create(
                C.UHID_MOUSE_ID,
                C.HID_VENDOR_ID,
                C.HID_MOUSE_PRODUCT_ID,
                C.HID_MOUSE_NAME,
                C.HID_MOUSE_DESCRIPTOR,
            )
        )
        self._send(
            protocol.uhid_create(
                C.UHID_KEYBOARD_ID,
                C.HID_VENDOR_ID,
                C.HID_KEYBOARD_PRODUCT_ID,
                C.HID_KEYBOARD_NAME,
                C.HID_KEYBOARD_DESCRIPTOR,
            )
        )
        self._uhid_created = True
        self._last_mouse_buttons = None  # 新设备的第一条报告也算一次状态
        LOG.info(
            "UHID 已创建：鼠标 id=0x%04X、键盘 id=0x%04X",
            C.UHID_MOUSE_ID,
            C.UHID_KEYBOARD_ID,
        )

    def _destroy_uhid(self) -> None:
        """UHID_DESTROY ×2；socket 已断时静默跳过。"""
        if not self._uhid_created or self._sock is None:
            self._uhid_created = False
            return
        for uhid_id in (C.UHID_KEYBOARD_ID, C.UHID_MOUSE_ID):
            try:
                self._send_quiet(protocol.uhid_destroy(uhid_id))
            except PadLendError as exc:
                LOG.debug("UHID_DESTROY 0x%04X 失败: %s", uhid_id, exc.detail)
        self._uhid_created = False

    # -- 内部：写入 --------------------------------------------------------
    def _socket_ok(self) -> bool:
        return (
            self._sock is not None
            and not self._sock_broken
            and self._consecutive_failures < C.CONSECUTIVE_WRITE_FAILURES
        )

    def _ensure_connected(self) -> None:
        if not self._socket_ok():
            raise PadLendError(ErrorCode.SOCKET_BROKEN, "控制通道未建立或已断开")

    def _send(self, message: bytes) -> bool:
        """写一条消息；前两次失败容忍，第三次起抛 E106（6.1.4）。"""
        return self._send_quiet(message, raise_on_broken=True)

    def _send_quiet(self, message: bytes, *, raise_on_broken: bool = False) -> bool:
        sock = self._sock
        if sock is None:
            if raise_on_broken:
                raise PadLendError(ErrorCode.SOCKET_BROKEN, "socket 已关闭")
            return False
        with self._send_lock:
            try:
                sock.sendall(message)
            except OSError as exc:
                self._consecutive_failures += 1
                self._last_error = f"socket 写失败: {exc}"
                LOG.warning("socket 写失败（连续 %d 次）: %s", self._consecutive_failures, exc)
                if self._consecutive_failures >= C.CONSECUTIVE_WRITE_FAILURES:
                    self._sock_broken = True
                    if raise_on_broken:
                        raise PadLendError(
                            ErrorCode.SOCKET_BROKEN,
                            f"连续 {self._consecutive_failures} 次写失败",
                            cause=exc,
                        ) from exc
                return False
            self._consecutive_failures = 0
            self._sent += 1
            return True

    # -- 内部：朝向监听（1.0.3） -------------------------------------------
    def _start_orientation_watch(self) -> None:
        if self._orientation_thread is not None and self._orientation_thread.is_alive():
            return
        dev = self._dev_adb
        if dev is None:
            return
        stop = threading.Event()
        thread = threading.Thread(
            target=self._orientation_loop,
            args=(dev, stop),
            name="padlend-orientation",
            daemon=True,
        )
        self._orientation_stop = stop
        self._orientation_thread = thread
        thread.start()
        LOG.debug("朝向监听已启动 serial=%s", self._facts.serial if self._facts else "?")

    def _stop_orientation_watch(self) -> None:
        stop = self._orientation_stop
        proc = self._orientation_proc
        thread = self._orientation_thread
        self._orientation_stop = None
        self._orientation_thread = None
        if stop is not None:
            stop.set()
        if proc is not None and proc.poll() is None:
            with contextlib.suppress(OSError):
                proc.terminate()
        if (
            thread is not None
            and thread.is_alive()
            and thread is not threading.current_thread()
        ):
            thread.join(timeout=2.0)
        if proc is not None and proc.poll() is None:
            with contextlib.suppress(OSError):
                proc.kill()
        self._orientation_proc = None

    def _orientation_loop(self, dev: Adb, stop: threading.Event) -> None:
        while not stop.is_set():
            proc: subprocess.Popen[str] | None = None
            try:
                proc = dev.popen_shell_stream(_ORIENTATION_SCRIPT)
                self._orientation_proc = proc
                stream = proc.stdout
                if stream is None:  # pragma: no cover - Popen 已指定 PIPE
                    return
                for line in stream:
                    if stop.is_set():
                        break
                    if "orientation=" in line:
                        self._apply_orientation(line, stop)
                LOG.debug("朝向监听进程结束 rc=%s", proc.poll())
            except (OSError, PadLendError) as exc:
                if not stop.is_set():
                    LOG.warning("朝向监听中断: %s", getattr(exc, "detail", exc))
            finally:
                if proc is not None:
                    with contextlib.suppress(OSError):
                        proc.terminate()
                    try:
                        proc.wait(timeout=1.0)
                    except (OSError, subprocess.TimeoutExpired):
                        with contextlib.suppress(OSError):
                            proc.kill()
                if self._orientation_proc is proc:
                    self._orientation_proc = None
            if stop.wait(_ORIENTATION_RESTART_S):
                break

    def _apply_orientation(self, line: str, stop: threading.Event) -> None:
        orientation = parse_orientation(line)
        old = self._facts
        if old is None or old.orientation == orientation:
            return
        natural_w, natural_h = old.width, old.height
        if old.orientation & 1:
            natural_w, natural_h = old.height, old.width
        width, height = oriented_size(natural_w, natural_h, orientation)
        new = replace(old, width=width, height=height, orientation=orientation)
        if stop.is_set() or not self._started:
            return
        self._facts = new
        LOG.info(
            "设备朝向变化: %dx%d orientation=%d",
            new.width,
            new.height,
            new.orientation,
        )
        callback = self._facts_callback
        if callback is not None:
            try:
                callback(new)
            except Exception:  # noqa: BLE001 - 回调失败不能让监听线程退出
                LOG.exception("朝向回调失败")

    # -- 内部：清理 --------------------------------------------------------
    def _teardown(self) -> None:
        """6.1.3 六步，任何一步失败都不阻断后续步骤。"""
        self._stop_orientation_watch()
        try:
            self._destroy_uhid()
        except PadLendError as exc:  # pragma: no cover - _destroy_uhid 已自吞
            LOG.warning("UHID_DESTROY 失败: %s", exc.detail)
        self._close_socket()

        server = self._server
        if server is not None:
            try:
                server.kill(timeout=2.0)
                server.forward_remove()
            except PadLendError as exc:
                LOG.warning("服务端清理失败: %s", exc.detail)

        self._restore_pointer_speed()
        self._restore_keep_screen_on()

        if server is not None:
            try:
                if not server.verify_clean(timeout=2.0):
                    LOG.warning("清理校验失败：%s", server.verify_detail)
                    log_event(LOG, C.EVENT_TABLET_CLEANUP_GHOST,
                              detail=server.verify_detail)
                    # 再补一刀，尽力消除幽灵 UHID（F7）
                    (self._dev_adb or self._adb).shell_ok("pkill", "-9", "-f", KILL_PATTERN)
                    if not server.verify_clean():
                        LOG.error("幽灵设备仍在：%s", server.verify_detail)
            except PadLendError as exc:
                LOG.warning("清理校验无法执行: %s", exc.detail)

        self._uhid_created = False
        self._sock_broken = False
        self._audio_active = False
        self._consecutive_failures = 0
        LOG.info("连接层已清理")

    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return (
            f"TabletSession(started={self._started}, port={self._port}, "
            f"serial={self._facts.serial if self._facts else None})"
        )


# ---------------------------------------------------------------------------
# 全局设置崩溃兜底：原值落盘，下次启动恢复
# ---------------------------------------------------------------------------
_POINTER_STATE_NAME = "pointer_speed_state.json"
_SCREEN_STATE_NAME = "screen_timeout_state.json"


def _state_path(name: str) -> Path:
    return C.config_dir() / name


def _write_state(name: str, payload: dict[str, Any]) -> None:
    """原子写状态文件；失败只告警，绝不阻断会话。"""
    try:
        cfg_dir = C.config_dir()
        cfg_dir.mkdir(parents=True, exist_ok=True)
        path = _state_path(name)
        tmp = path.with_suffix(".tmp")
        body = json.dumps(
            {**payload, "saved_at": time.time()},
            ensure_ascii=False,
        )
        with tmp.open("w", encoding="utf-8") as fh:
            fh.write(body)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
    except Exception as exc:  # noqa: BLE001 - 状态文件失败不能影响主流程
        LOG.warning("保存 %s 状态失败: %r", name, exc)


def _load_state(name: str) -> tuple[str, int] | None:
    try:
        raw = json.loads(_state_path(name).read_text(encoding="utf-8"))
        value = int(raw["value"])
        serial = str(raw.get("serial") or "")
    except (OSError, ValueError, TypeError, KeyError):
        return None
    return serial, value


def _clear_state(name: str) -> None:
    with contextlib.suppress(OSError):
        _state_path(name).unlink(missing_ok=True)


def save_pointer_state(serial: str, value: int | None) -> None:
    """记录归零前的原值。"""
    if value is None:
        return
    _write_state(_POINTER_STATE_NAME, {"serial": serial or "", "value": int(value)})


def load_pointer_state() -> tuple[str, int] | None:
    return _load_state(_POINTER_STATE_NAME)


def clear_pointer_state() -> None:
    _clear_state(_POINTER_STATE_NAME)


def save_screen_timeout_state(serial: str, value: int) -> None:
    """记录连接前的系统熄屏超时（毫秒）。"""
    _write_state(_SCREEN_STATE_NAME, {"serial": serial or "", "value": int(value)})


def load_screen_timeout_state() -> tuple[str, int] | None:
    return _load_state(_SCREEN_STATE_NAME)


def clear_screen_timeout_state() -> None:
    _clear_state(_SCREEN_STATE_NAME)


def restore_pending_pointer_speed(adb: Adb | None) -> int | None:
    """启动时恢复上次崩溃残留的 pointer_speed；成功返回恢复值。

    与 exit.clean 无关：只要状态文件还在就尝试恢复（设备可能没插，
    此时保留文件，下次再试）。
    """
    state = load_pointer_state()
    if state is None or adb is None:
        return None
    serial, value = state
    dev = adb.with_serial(serial) if serial else adb
    if value != 0 and not dev.shell_ok("settings", "put", "system", "pointer_speed", str(value)):
        LOG.warning("启动恢复 pointer_speed=%d 失败（设备未连接？），保留状态", value)
        return None
    clear_pointer_state()
    LOG.info("启动恢复 pointer_speed=%d serial=%s", value, serial or "(默认)")
    return value


def restore_pending_screen_timeout(adb: Adb | None) -> int | None:
    """启动时恢复上次崩溃残留的 screen_off_timeout；成功返回恢复值。

    与 exit.clean 无关：只要状态文件还在就尝试恢复（设备可能没插，
    此时保留文件，下次再试）。
    """
    state = load_screen_timeout_state()
    if state is None or adb is None:
        return None
    serial, value = state
    dev = adb.with_serial(serial) if serial else adb
    if not dev.shell_ok("settings", "put", "system", "screen_off_timeout", str(value)):
        LOG.warning("启动恢复 screen_off_timeout=%d 失败（设备未连接？），保留状态", value)
        return None
    clear_screen_timeout_state()
    LOG.info("启动恢复 screen_off_timeout=%d serial=%s", value, serial or "(默认)")
    return value


# ---------------------------------------------------------------------------
# 12.3 下次启动 / 崩溃兜底：强制清理
# ---------------------------------------------------------------------------
def force_cleanup(
    adb: Adb | None,
    serial: str = "",
    *,
    port: int | None = None,
    restore_pointer_speed: int = _FACTORY_POINTER_SPEED,
) -> None:
    """``exit.clean`` 缺失时先跑这个（12.3）。

    绝不抛异常：这是兜底路径，清理失败只记日志。
    """
    if adb is None:
        return
    # 持久化的原值优先于配置里的 -3（-3 只是"恢复出厂"保底）。
    pending = load_pointer_state()
    if pending is not None:
        pending_serial, pending_value = pending
        if not serial or not pending_serial or pending_serial == serial:
            serial = serial or pending_serial
            restore_pointer_speed = pending_value
    dev = adb.with_serial(serial) if serial else adb
    try:
        dev.shell_ok("pkill", "-9", "-f", KILL_PATTERN)
        if port is not None:
            dev.forward_remove(port)
        if serial:
            if dev.shell_ok("settings", "put", "system", "pointer_speed",
                            str(restore_pointer_speed)):
                clear_pointer_state()
            dev.shell_ok("settings", "put", "system", "pointer_location", "0")
    except PadLendError as exc:  # pragma: no cover - shell_ok 已吞
        LOG.warning("force_cleanup 失败: %s", exc.detail)
