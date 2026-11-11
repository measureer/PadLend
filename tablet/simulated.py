"""无硬件的平板替身（``--simulate`` 与自动验收用）。

用途有两个：

1. 让 GUI / 无头模式在没有平板时也能进入 CAPTURED，方便看界面与复现状态机问题；
2. 给 ``tools/acceptance.py`` 一个可断言的设备端：发出去的 HID 报告被记录下来，
   可以验证"点一下平板应该收到 按下+抬起"这类行为，而不需要真机。

只实现 ``TabletSession`` 被 Router / MainWindow 用到的公开面，不碰 ADB、socket、
scrcpy server。**不要**在正式路径里用它顶替真会话：``--simulate`` 必须显式开启。
"""

from __future__ import annotations

import logging
import threading
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

from tablet.adb import STATE_DEVICE, DeviceEntry
from tablet.session import DeviceFacts, HealthReport

log = logging.getLogger("tablet.simulated")

DEFAULT_WIDTH = 1707
DEFAULT_HEIGHT = 1067
SIM_SERIAL = "sim:5555"


@dataclass
class SimReport:
    """记录一次发出的鼠标报告。"""

    buttons: int
    dx: int
    dy: int
    wheel: int


@dataclass
class SimulatedSession:
    """``TabletSession`` 的替身：永远在线，报告只进内存。"""

    serial: str = SIM_SERIAL
    model: str = "Simulated Tablet"
    width: int = DEFAULT_WIDTH
    height: int = DEFAULT_HEIGHT
    orientation: int = 0
    xscale: float = 2.0
    pointer_speed: int = -3

    reports: list[SimReport] = field(default_factory=list)
    keys: list[tuple[int, tuple[int, ...]]] = field(default_factory=list)
    texts: list[str] = field(default_factory=list)
    clipboards: list[str] = field(default_factory=list)
    cursor: tuple[float, float] = (0.0, 0.0)

    _lock: threading.RLock = field(default_factory=threading.RLock, repr=False)
    _started: bool = False
    _facts: DeviceFacts | None = None
    _pointer_speed_calls: int = 0
    _clipboard_callback: Callable[[str], None] | None = field(default=None, repr=False)

    # -- 身份 ---------------------------------------------------------------
    @property
    def simulated(self) -> bool:
        return True

    @property
    def started(self) -> bool:
        with self._lock:
            return self._started

    @property
    def facts(self) -> DeviceFacts | None:
        with self._lock:
            return self._facts

    def _make_facts(self) -> DeviceFacts:
        return DeviceFacts(
            serial=self.serial,
            model=self.model,
            width=int(self.width),
            height=int(self.height),
            orientation=int(self.orientation),
            xscale=float(self.xscale),
            pointer_speed=int(self.pointer_speed),
        )

    # -- 生命周期 -----------------------------------------------------------
    def start(self) -> DeviceFacts:
        with self._lock:
            facts = self._facts or self._make_facts()
            self._facts = facts
            self._started = True
        log.info("simulated.session.started serial=%s", facts.serial)
        return facts

    def stop(self) -> None:
        with self._lock:
            self._started = False
        log.info("simulated.session.stopped")

    def cancel_start(self) -> None:
        return None

    def reconnect(self, *, attempts: int | None = None) -> DeviceFacts | None:
        return self.start()

    # -- 发送 ---------------------------------------------------------------
    def send_mouse(self, buttons: int, dx: int, dy: int, wheel: int = 0) -> None:
        with self._lock:
            if not self._started:
                return
            self.reports.append(SimReport(int(buttons), int(dx), int(dy), int(wheel)))
            x, y = self.cursor
            self.cursor = (x + int(dx), y + int(dy))

    def send_key(self, modifier: int, usages: object) -> None:
        with self._lock:
            self.keys.append((int(modifier), tuple(int(u) for u in usages)))  # type: ignore[arg-type]

    def send_keycode(self, keycode: int, down: bool = True) -> None:
        self.send_key(0, (int(keycode),))

    def inject_text(self, text: str) -> None:
        with self._lock:
            self.texts.append(str(text))

    def set_clipboard_callback(self, callback: Callable[[str], None] | None) -> None:
        """注册模拟平板的剪贴板回传回调。"""
        self._clipboard_callback = callback

    def publish_clipboard(self, text: str) -> None:
        """模拟用户在平板上复制文本。"""
        callback = self._clipboard_callback
        if callback is not None:
            callback(str(text))

    def set_clipboard(self, text: str, *, sequence: int = 0) -> None:
        """记录写平板剪贴板的请求（0.3.14 起自动同步走这条路）。"""
        if not text:
            return
        with self._lock:
            self.clipboards.append(str(text))

    def health(self) -> HealthReport:
        with self._lock:
            return HealthReport(
                alive=self._started,
                socket_ok=self._started,
                uhid_created=self._started,
                server_alive=self._started,
                consecutive_failures=0,
                last_error="",
                uptime_s=1.0,
                sent=len(self.reports),
            )

    # -- 桌面指针 / 指针速度 -------------------------------------------------
    def set_cursor_pos(self, x: int, y: int) -> None:
        with self._lock:
            self.cursor = (float(x), float(y))

    def ensure_pointer_speed(self) -> bool:
        with self._lock:
            self._pointer_speed_calls += 1
        return True

    def set_pointer_speed(self, speed: int) -> None:
        with self._lock:
            self.pointer_speed = int(speed)

    @property
    def pointer_speed_calls(self) -> int:
        """``ensure_pointer_speed`` 走了几次（验收用）。"""
        with self._lock:
            return self._pointer_speed_calls

    # -- 断言辅助 -----------------------------------------------------------
    def clear(self) -> None:
        with self._lock:
            self.reports.clear()
            self.keys.clear()
            self.texts.clear()
            self.clipboards.clear()

    @property
    def clicks(self) -> int:
        """按下次数（buttons 从 0 变 1 的次数）。"""
        with self._lock:
            previous = 0
            count = 0
            for report in self.reports:
                if previous == 0 and report.buttons & 0x01:
                    count += 1
                previous = report.buttons
            return count

    @property
    def sent(self) -> int:
        with self._lock:
            return len(self.reports)


class SimulatedAdb:
    """``Adb`` 的替身：只回答"有一台已授权的无线平板"，不做任何 I/O。

    GUI 的设备页、``_add_device_worker``、``Router.connect_device`` 都通过
    ``self._adb`` 访问 ADB；``--simulate`` 时换成它，界面就能像真机一样
    列出并连接假平板。
    """

    path = "simulated-adb"

    def version(self) -> str:
        return "simulated"

    def with_serial(self, serial: str | None = None) -> SimulatedAdb:
        return self

    def mdns_services(self) -> list[str]:
        return []

    def devices(self) -> list[DeviceEntry]:
        return [
            DeviceEntry(
                serial=SIM_SERIAL,
                state=STATE_DEVICE,
                product="sim",
                model=SimulatedSession.model,
                device="sim",
                transport_id="1",
            )
        ]

    def online_devices(self) -> list[DeviceEntry]:
        return self.devices()

    def select_device(self, serial: str | None = None, *, prefer_usb: bool = True) -> DeviceEntry:
        entries = self.devices()
        if serial:
            for entry in entries:
                if entry.serial == serial:
                    return entry
        return entries[0]

    def wait_for_device(self, timeout: float = 5.0, *, cancel: Any = None) -> DeviceEntry:
        return self.devices()[0]

    def connect(self, address: str, *, timeout: float = 5.0) -> str:
        return f"connected to {address}"

    def pair(self, address: str, code: str, *, timeout: float = 5.0) -> str:
        return f"Successfully paired to {address}"

    def disconnect(self, address: str | None = None, *, timeout: float = 5.0) -> None:
        return None

    def kill_server(self, *, timeout: float = 5.0) -> None:
        return None

    def wifi_mac(self, serial: str | None = None, *, timeout: float = 8.0) -> str:
        return ""
