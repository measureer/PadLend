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
import logging
import re
import socket
import threading
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

import constants as C
from core.errors import ErrorCode, PadLendError
from core.logsetup import log_event

from . import protocol
from .adb import Adb, DeviceEntry
from .server import KILL_PATTERN, ScrcpyServer
from .uhid import keyboard_report, mouse_report

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path

    from config import Config

LOG = logging.getLogger("tablet.session")

# ``settings get`` 读不到时 Android 返回这个字符串。
_NULL_SETTING = "null"
# 出厂 pointer_speed（3.1）；恢复时若原值不可读则回退到它。
_FACTORY_POINTER_SPEED = -3


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


def read_facts(adb: Adb) -> DeviceFacts:
    """读取 8.2 的 DeviceFacts（附录 C.2 的命令集合）。

    ``adb`` 必须已绑定 serial。
    """
    if not adb.serial:
        raise PadLendError(ErrorCode.DEVICE_OFFLINE, "read_facts 需要一个已绑定的 serial")
    model = adb.shell("getprop", "ro.product.model")
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
    ) -> None:
        self._cfg = cfg
        self._adb = adb if adb is not None else Adb()
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
        self._orig_stay_on: str | None = None

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
            return facts

    def _start_locked(self) -> DeviceFacts:
        entry = self._select_device()
        self._dev_adb = self._adb.with_serial(entry.serial)
        # 服务端封装必须绑定到具体设备（多设备时不能落到默认设备）
        self._server = ScrcpyServer(self._dev_adb, port=self._port, jar=self._jar)
        LOG.info("使用设备 %s（%s）", entry.serial, entry.model or "unknown")

        facts = read_facts(self._dev_adb)
        self._normalize_pointer_speed()
        self.server.push_if_needed()
        self.server.kill()  # 清旧实例，保证幂等
        self.server.spawn()
        self._port = self.server.forward()
        self._connect_socket()
        self._create_uhid()
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

    def stop(self) -> None:
        """6.1.3 退出序列；幂等，未 start 时也安全。

        先把 ``_closing`` 置位**再**拿锁：若另一线程正卡在 ``start()`` 的
        40s 等待设备里（它持有生命周期锁），等待循环会很快看到取消标志并
        退出，随后本方法才能拿到锁完成清理（GUI 退出不再假死）。
        """
        self._closing.set()
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

    def _normalize_pointer_speed(self) -> None:
        """记录原值 → 归零（6.1.2）。失败按 E101 处理（设备拒绝写设置）。"""
        assert self._dev_adb is not None
        original = parse_pointer_speed(
            self._dev_adb.shell("settings", "get", "system", "pointer_speed")
        )
        self._orig_pointer_speed = original
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
            return
        if self._dev_adb.shell_ok("settings", "put", "system", "pointer_speed", str(target)):
            LOG.info("pointer_speed 已还原为 %d", target)
        else:
            LOG.warning("pointer_speed 还原失败（原值 %d）", target)

    def _keep_screen_on(self) -> None:
        """``svc power stayon true``（6.1.2），并记录原值以便退出还原。"""
        assert self._dev_adb is not None
        enabled = self._cfg.misc.keep_screen_on if self._cfg is not None else True
        if not enabled:
            return
        self._orig_stay_on = self._dev_adb.shell(
            "settings", "get", "global", "stay_on_while_plugged_in"
        ).strip()
        if not self._dev_adb.shell_ok("svc", "power", "stayon", "true"):
            LOG.warning("svc power stayon true 失败（不致命）")

    def _restore_keep_screen_on(self) -> None:
        """还原 keep screen on；文档未列此步，但对称清理避免改坏用户设置。"""
        if self._dev_adb is None or self._orig_stay_on is None:
            return
        previous = self._orig_stay_on
        if previous.isdigit():
            self._dev_adb.shell_ok(
                "settings", "put", "global", "stay_on_while_plugged_in", previous
            )
        else:
            self._dev_adb.shell_ok("svc", "power", "stayon", "false")
        self._orig_stay_on = None

    # -- 内部：socket / uhid ----------------------------------------------
    def _connect_socket(self) -> None:
        """连接 forward 端口并校验 1 字节 dummy（9.1）。

        实测 scrcpy 5.0.1：``tunnel_forward=true`` 且 ``video=false audio=false
        control=true`` 时只接受一个 control socket，并写一个 0 字节
        （DesktopConnection 注释：send one byte so the client may read() to
        detect a connection error）。DESIGN 9.1 的 4 字节为早期误记。

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
                        f"dummy 字节不符: {dummy.hex()}（期望 {C.HANDSHAKE_DUMMY.hex()}）",
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
                        f"连接 127.0.0.1:{self._port} 失败（已重试 "
                        f"{C.HANDSHAKE_RETRY_TIMEOUT_S:.0f}s）: {exc}",
                        cause=exc,
                    ) from exc
                LOG.debug("服务端尚未就绪，重试握手: %s", exc)
                time.sleep(C.HANDSHAKE_RETRY_INTERVAL_S)
                continue
            self._sock = sock
            self._sock_broken = False
            LOG.info("控制通道已建立（tcp:%d）", self._port)
            return

    def _close_socket(self) -> None:
        with self._send_lock:
            sock, self._sock = self._sock, None
        if sock is None:
            return
        with contextlib.suppress(OSError):
            sock.shutdown(socket.SHUT_RDWR)
        try:
            sock.close()
        except OSError:  # pragma: no cover - 已关闭
            LOG.debug("socket 关闭时已失效")

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

    # -- 内部：清理 --------------------------------------------------------
    def _teardown(self) -> None:
        """6.1.3 六步，任何一步失败都不阻断后续步骤。"""
        try:
            self._destroy_uhid()
        except PadLendError as exc:  # pragma: no cover - _destroy_uhid 已自吞
            LOG.warning("UHID_DESTROY 失败: %s", exc.detail)
        self._close_socket()

        server = self._server
        if server is not None:
            try:
                server.kill()
                server.forward_remove()
            except PadLendError as exc:
                LOG.warning("服务端清理失败: %s", exc.detail)

        self._restore_pointer_speed()
        self._restore_keep_screen_on()

        if server is not None:
            try:
                if not server.verify_clean():
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
        self._consecutive_failures = 0
        LOG.info("连接层已清理")

    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return (
            f"TabletSession(started={self._started}, port={self._port}, "
            f"serial={self._facts.serial if self._facts else None})"
        )


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
    dev = adb.with_serial(serial) if serial else adb
    try:
        dev.shell_ok("pkill", "-9", "-f", KILL_PATTERN)
        if port is not None:
            dev.forward_remove(port)
        if serial:
            dev.shell_ok("settings", "put", "system", "pointer_speed",
                         str(restore_pointer_speed))
            dev.shell_ok("settings", "put", "system", "pointer_location", "0")
    except PadLendError as exc:  # pragma: no cover - shell_ok 已吞
        LOG.warning("force_cleanup 失败: %s", exc.detail)
