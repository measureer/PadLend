"""Windows Raw Input 鼠标捕获（设计文档 6.2.1 / 10.1 T2 / 附录 A.4）。

数据通路（铁律）：``WM_INPUT`` → 解析 ``RAWMOUSE`` → ``RawDelta`` → ``on_event``。
``on_event`` **只允许入队**，绝不阻塞（10.4）；本模块不做队列、不做背压，
那属于 ``core/router.py``。

线程模型（10.1 T2）：专用线程 ``rawinput`` 创建 message-only 窗口
（``HWND_MESSAGE``），以 ``RIDEV_INPUTSINK`` 注册鼠标 —— 窗口没有焦点也能收到输入。
线程阻塞在 ``GetMessage``；``stop()`` 通过 ``PostThreadMessageW(WM_QUIT)`` 唤醒并 join。

关键性质（F3）：Raw Input 是**累加前**数据，``ClipCursor`` 把光标钉死时仍照常上报，
这是"PC 光标不动、平板指针继续走"的物理基础。

绝对设备（绘图板 / RDP）：``usFlags & MOUSE_MOVE_ABSOLUTE`` 时 ``lLastX/lLastY`` 是
0..65535 的归一化坐标，按附录 A.4 换算到虚拟桌面物理矩形后再做帧间差分，
所以对外仍然只暴露相对位移。

导入本模块不注册设备、不创建窗口（实施说明 2.6）。
"""

from __future__ import annotations

import ctypes
import logging
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Protocol

import constants
from core.errors import ErrorCode, PadLendError
from core.events import RawDelta
from core.geometry import Rect

_log = logging.getLogger("win.rawinput")

# 线程启动/停止的等待上限（秒）。超过即视为启动失败 / 线程未能退出。
START_TIMEOUT_S = 5.0
STOP_TIMEOUT_S = 2.0

# ---------------------------------------------------------------------------
# Win32 常量
# ---------------------------------------------------------------------------
WM_DESTROY = 0x0002
WM_INPUT = 0x00FF
WM_QUIT = 0x0012

HID_USAGE_PAGE_GENERIC = 0x01
HID_USAGE_GENERIC_MOUSE = 0x02

RIDEV_REMOVE = 0x00000001
RIDEV_INPUTSINK = 0x00000100

RID_INPUT = 0x10000003
RIM_TYPEMOUSE = 0
_RID_ERROR = 0xFFFFFFFF  # GetRawInputData 的 (UINT)-1

MOUSE_MOVE_ABSOLUTE = 0x0001
MOUSE_VIRTUAL_DESKTOP = 0x0002

RI_MOUSE_WHEEL = 0x0400
RI_MOUSE_HWHEEL = 0x0800

SM_CXSCREEN = 0
SM_CYSCREEN = 1
SM_XVIRTUALSCREEN = 76
SM_YVIRTUALSCREEN = 77
SM_CXVIRTUALSCREEN = 78
SM_CYVIRTUALSCREEN = 79

ERROR_CLASS_ALREADY_EXISTS = 1410

_WINDOW_CLASS_NAME = "PadLendRawInputWindow"
_WINDOW_TITLE = "PadLend Raw Input"

# 所有"按下/抬起"标志的并集，用于判断一个包是否携带按键变化。
_BUTTON_FLAG_MASK = 0
for _flag in constants.RAW_BUTTON_FLAGS:
    _BUTTON_FLAG_MASK |= _flag
del _flag


# ---------------------------------------------------------------------------
# 数据：原始包 + 跨帧状态
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class MousePacket:
    """一个 ``RAWMOUSE`` 的原始字段（尚未解释成按键/位移）。

    保留原始字段是为了让解释逻辑（``interpret``）成为纯函数：
    测试不需要真实窗口或真实设备就能覆盖全部分支。
    """

    flags: int
    x: int
    y: int
    button_flags: int
    button_data: int
    device: int = 0
    ts: float = 0.0


@dataclass(slots=True)
class PointerState:
    """跨帧累加状态。

    - ``buttons``：HID 语义的按键位图，由 ``usButtonFlags`` 的 down/up 事件增量维护；
    - ``last_abs``：**每台设备**上一帧的绝对坐标基线（``hDevice`` → 坐标）。
      绘图板和鼠标通常同时存在，共用一个基线会在设备切换时产生跳变。
    """

    buttons: int = 0
    last_abs: dict[int, tuple[int, int]] = field(default_factory=dict)


# ---------------------------------------------------------------------------
# 纯逻辑：按键 / 坐标 / 轮
# ---------------------------------------------------------------------------
def as_signed_short(value: int) -> int:
    """把 ``usButtonData``（USHORT）解释成有符号 short。"""
    raw = value & 0xFFFF
    return raw - 0x10000 if raw & 0x8000 else raw


def apply_button_flags(buttons: int, flags: int) -> int:
    """按 ``constants.RAW_BUTTON_FLAGS`` 增量更新按键位图（up 事件清除对应位）。"""
    result = buttons & 0xFF
    for bit, mask in constants.RAW_BUTTON_FLAGS.items():
        if not flags & bit:
            continue
        if bit in constants.RAW_BUTTON_UP_FLAGS:
            result &= ~mask
        else:
            result |= mask
    return result & 0xFF


def normalize_absolute(x: int, y: int, rect: Rect) -> tuple[int, int]:
    """0..65535 的绝对坐标 → 虚拟桌面物理像素（附录 A.4）。

    ``rect`` 是虚拟桌面矩形；换算结果加上 ``rect`` 的原点，负坐标桌面也成立
    （只有帧间差分会进入 ``RawDelta``，所以原点偏移不影响输出）。
    """
    if rect.width <= 0 or rect.height <= 0:
        return 0, 0
    ax = int(x * rect.width / 65535)
    ay = int(y * rect.height / 65535)
    return rect.left + ax, rect.top + ay


def interpret(
    packet: MousePacket,
    state: PointerState,
    rect: Rect | None = None,
) -> RawDelta | None:
    """把一个原始包解释成 ``RawDelta``；空包（无位移/无按键/无滚轮）返回 ``None``。

    ``state`` 会被就地更新（按键位图、绝对设备基线）。绝对设备必须提供 ``rect``；
    缺失时差分按 0 处理（仅防御，生产路径总会提供）。
    """
    state.buttons = apply_button_flags(state.buttons, packet.button_flags)
    absolute = bool(packet.flags & MOUSE_MOVE_ABSOLUTE)

    if absolute:
        point = (0, 0) if rect is None else normalize_absolute(packet.x, packet.y, rect)
        previous = state.last_abs.get(packet.device)
        state.last_abs[packet.device] = point
        dx, dy = (0, 0) if previous is None else (point[0] - previous[0], point[1] - previous[1])
    else:
        dx, dy = packet.x, packet.y

    wheel = 0
    if packet.button_flags & RI_MOUSE_WHEEL:
        wheel = as_signed_short(packet.button_data)

    changed = bool(packet.button_flags & _BUTTON_FLAG_MASK)
    if not changed and dx == 0 and dy == 0 and wheel == 0:
        return None

    return RawDelta(
        dx=dx,
        dy=dy,
        buttons=state.buttons,
        wheel=wheel,
        absolute=absolute,
        ts=packet.ts,
    )


# ---------------------------------------------------------------------------
# Win32 结构体（只在长度/偏移上依赖平台，构造本身无副作用）
# ---------------------------------------------------------------------------
class _POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_int32), ("y", ctypes.c_int32)]


class _MSG(ctypes.Structure):
    _fields_ = [
        ("hwnd", ctypes.c_void_p),
        ("message", ctypes.c_uint32),
        ("wParam", ctypes.c_size_t),
        ("lParam", ctypes.c_ssize_t),
        ("time", ctypes.c_uint32),
        ("pt", _POINT),
        ("lPrivate", ctypes.c_uint32),
    ]


class _WNDCLASSEXW(ctypes.Structure):
    _fields_ = [
        ("cbSize", ctypes.c_uint32),
        ("style", ctypes.c_uint32),
        ("lpfnWndProc", ctypes.c_void_p),
        ("cbClsExtra", ctypes.c_int),
        ("cbWndExtra", ctypes.c_int),
        ("hInstance", ctypes.c_void_p),
        ("hIcon", ctypes.c_void_p),
        ("hCursor", ctypes.c_void_p),
        ("hbrBackground", ctypes.c_void_p),
        ("lpszMenuName", ctypes.c_wchar_p),
        ("lpszClassName", ctypes.c_wchar_p),
        ("hIconSm", ctypes.c_void_p),
    ]


class _RAWINPUTDEVICE(ctypes.Structure):
    _fields_ = [
        ("usUsagePage", ctypes.c_uint16),
        ("usUsage", ctypes.c_uint16),
        ("dwFlags", ctypes.c_uint32),
        ("hwndTarget", ctypes.c_void_p),
    ]


class _RAWINPUTHEADER(ctypes.Structure):
    _fields_ = [
        ("dwType", ctypes.c_uint32),
        ("dwSize", ctypes.c_uint32),
        ("hDevice", ctypes.c_void_p),
        ("wParam", ctypes.c_size_t),
    ]


class _RAWMOUSE_BUTTONS(ctypes.Structure):
    _fields_ = [
        ("usButtonFlags", ctypes.c_uint16),
        ("usButtonData", ctypes.c_uint16),
    ]


class _RAWMOUSE_BUTTON_UNION(ctypes.Union):
    """``ulButtons`` 与 ``{usButtonFlags, usButtonData}`` 共用一块内存（C 匿名 union）。"""

    _fields_ = [
        ("ulButtons", ctypes.c_uint32),
        ("buttons", _RAWMOUSE_BUTTONS),
    ]


class _RAWMOUSE(ctypes.Structure):
    """``RAWMOUSE``。用 ctypes 让编译器对齐规则自己算偏移，不手写 padding。"""

    _fields_ = [
        ("usFlags", ctypes.c_uint16),
        ("u", _RAWMOUSE_BUTTON_UNION),
        ("ulRawButtons", ctypes.c_uint32),
        ("lLastX", ctypes.c_int32),
        ("lLastY", ctypes.c_int32),
        ("ulExtraInformation", ctypes.c_uint32),
    ]


class _RAWINPUTDATA(ctypes.Union):
    _fields_ = [("mouse", _RAWMOUSE)]


class _RAWINPUT(ctypes.Structure):
    _fields_ = [
        ("header", _RAWINPUTHEADER),
        ("data", _RAWINPUTDATA),
    ]


def _wndproc_type() -> type:
    maker = getattr(ctypes, "WINFUNCTYPE", ctypes.CFUNCTYPE)
    return maker(
        ctypes.c_ssize_t,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_size_t,
        ctypes.c_ssize_t,
    )


_WNDPROC_TYPE = _wndproc_type()


# ---------------------------------------------------------------------------
# Win32 加载（延迟到真正使用时；import 不碰 user32）
# ---------------------------------------------------------------------------
_user32_lib: ctypes.WinDLL | None = None
_kernel32_lib: ctypes.WinDLL | None = None


def _require_windows() -> None:
    if sys.platform != "win32":
        raise PadLendError(ErrorCode.RAWINPUT_FAILED, "Raw Input 仅支持 Windows")


def _user32() -> ctypes.WinDLL:
    global _user32_lib
    _require_windows()
    if _user32_lib is None:
        lib = ctypes.WinDLL("user32", use_last_error=True)
        lib.RegisterClassExW.argtypes = [ctypes.POINTER(_WNDCLASSEXW)]
        lib.RegisterClassExW.restype = ctypes.c_uint16
        lib.CreateWindowExW.argtypes = [
            ctypes.c_uint32,
            ctypes.c_wchar_p,
            ctypes.c_wchar_p,
            ctypes.c_uint32,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_void_p,
        ]
        lib.CreateWindowExW.restype = ctypes.c_void_p
        lib.DefWindowProcW.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_size_t,
            ctypes.c_ssize_t,
        ]
        lib.DefWindowProcW.restype = ctypes.c_ssize_t
        lib.GetMessageW.argtypes = [
            ctypes.POINTER(_MSG),
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_uint32,
        ]
        lib.GetMessageW.restype = ctypes.c_int
        lib.TranslateMessage.argtypes = [ctypes.POINTER(_MSG)]
        lib.TranslateMessage.restype = ctypes.c_int
        lib.DispatchMessageW.argtypes = [ctypes.POINTER(_MSG)]
        lib.DispatchMessageW.restype = ctypes.c_ssize_t
        lib.PostQuitMessage.argtypes = [ctypes.c_int]
        lib.PostQuitMessage.restype = None
        lib.PostThreadMessageW.argtypes = [
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_size_t,
            ctypes.c_ssize_t,
        ]
        lib.PostThreadMessageW.restype = ctypes.c_int
        lib.RegisterRawInputDevices.argtypes = [
            ctypes.POINTER(_RAWINPUTDEVICE),
            ctypes.c_uint32,
            ctypes.c_uint32,
        ]
        lib.RegisterRawInputDevices.restype = ctypes.c_int
        lib.GetRawInputData.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_uint32),
            ctypes.c_uint32,
        ]
        lib.GetRawInputData.restype = ctypes.c_uint32
        lib.DestroyWindow.argtypes = [ctypes.c_void_p]
        lib.DestroyWindow.restype = ctypes.c_int
        lib.GetSystemMetrics.argtypes = [ctypes.c_int]
        lib.GetSystemMetrics.restype = ctypes.c_int
        _user32_lib = lib
    return _user32_lib


def _kernel32() -> ctypes.WinDLL:
    global _kernel32_lib
    _require_windows()
    if _kernel32_lib is None:
        lib = ctypes.WinDLL("kernel32", use_last_error=True)
        lib.GetCurrentThreadId.argtypes = []
        lib.GetCurrentThreadId.restype = ctypes.c_uint32
        lib.GetModuleHandleW.argtypes = [ctypes.c_wchar_p]
        lib.GetModuleHandleW.restype = ctypes.c_void_p
        _kernel32_lib = lib
    return _kernel32_lib


def virtual_screen_rect() -> Rect:
    """虚拟桌面矩形（物理像素，可能是负坐标）。绝对设备归一化的参照系。"""
    user32 = _user32()
    left = int(user32.GetSystemMetrics(SM_XVIRTUALSCREEN))
    top = int(user32.GetSystemMetrics(SM_YVIRTUALSCREEN))
    width = int(user32.GetSystemMetrics(SM_CXVIRTUALSCREEN))
    height = int(user32.GetSystemMetrics(SM_CYVIRTUALSCREEN))
    if width <= 0 or height <= 0:
        width = int(user32.GetSystemMetrics(SM_CXSCREEN)) or 1
        height = int(user32.GetSystemMetrics(SM_CYSCREEN)) or 1
        left = top = 0
    return Rect(left, top, left + width, top + height)


# ---------------------------------------------------------------------------
# 解码 GetRawInputData 的输出（纯函数，单测直接喂字节）
# ---------------------------------------------------------------------------
def decode_raw_input(data: bytes) -> MousePacket | None:
    """把 ``GetRawInputData`` 的输出缓冲区解码成 ``MousePacket``。

    只接受鼠标包；键盘 / HID 包返回 ``None``（本模块只注册鼠标）。
    """
    if len(data) < ctypes.sizeof(_RAWINPUT):
        return None
    raw = _RAWINPUT.from_buffer_copy(data[: ctypes.sizeof(_RAWINPUT)])
    if raw.header.dwType != RIM_TYPEMOUSE:
        return None
    mouse = raw.data.mouse
    return MousePacket(
        flags=int(mouse.usFlags),
        x=int(mouse.lLastX),
        y=int(mouse.lLastY),
        button_flags=int(mouse.u.buttons.usButtonFlags),
        button_data=int(mouse.u.buttons.usButtonData),
        device=int(raw.header.hDevice or 0),
        ts=time.perf_counter(),
    )


# ---------------------------------------------------------------------------
# 后端接口（测试注入假实现，不需要真实窗口/设备）
# ---------------------------------------------------------------------------
class RawInputBackend(Protocol):
    """Raw Input 平台后端。

    ``prepare`` 在 rawinput 线程里调用一次；``pump`` 阻塞到 ``request_stop``；
    ``close`` 必须幂等（``prepare`` 半途失败时也会被调用）。
    """

    def prepare(self) -> None: ...

    def pump(self, on_packet: Callable[[MousePacket], None]) -> None: ...

    def request_stop(self) -> bool: ...

    def close(self) -> None: ...


# 窗口句柄 → 后端：WndProc 是 per-class 的，用模块级表把消息路由到对应实例。
_BACKENDS: dict[int, Win32RawInputBackend] = {}
_WINDOW_PROC: object | None = None
_CLASS_LOCK = threading.Lock()


def _dispatch_to_backend(hwnd: int, msg: int, wparam: int, lparam: int) -> int:
    backend = _BACKENDS.get(hwnd)
    if backend is None:
        return int(_user32().DefWindowProcW(hwnd, msg, wparam, lparam))
    return backend.handle_message(hwnd, msg, wparam, lparam)


class Win32RawInputBackend:
    """真实 Win32 实现：message-only 窗口 + ``RIDEV_INPUTSINK`` + ``GetMessage`` 循环。"""

    def __init__(self) -> None:
        self._hwnd: int | None = None
        self._thread_id: int = 0
        self._on_packet: Callable[[MousePacket], None] | None = None
        self._prepared = False

    # -- 生命周期 ---------------------------------------------------------
    def prepare(self) -> None:
        user32 = _user32()
        kernel32 = _kernel32()
        _ensure_window_class(user32)
        self._thread_id = int(kernel32.GetCurrentThreadId())

        hinst = kernel32.GetModuleHandleW(None)
        if not hinst:
            raise PadLendError(ErrorCode.RAWINPUT_FAILED, "GetModuleHandleW 失败")

        hwnd = user32.CreateWindowExW(
            0,
            _WINDOW_CLASS_NAME,
            _WINDOW_TITLE,
            0,
            0,
            0,
            0,
            0,
            ctypes.c_void_p(-3),  # HWND_MESSAGE：message-only 窗口
            None,
            hinst,
            None,
        )
        if not hwnd:
            raise PadLendError(
                ErrorCode.RAWINPUT_FAILED,
                f"CreateWindowExW 失败 err={ctypes.get_last_error()}",
            )
        self._hwnd = int(hwnd)
        _BACKENDS[self._hwnd] = self

        device = _RAWINPUTDEVICE(
            HID_USAGE_PAGE_GENERIC,
            HID_USAGE_GENERIC_MOUSE,
            RIDEV_INPUTSINK,
            ctypes.c_void_p(self._hwnd),
        )
        if not user32.RegisterRawInputDevices(
            ctypes.byref(device), 1, ctypes.sizeof(_RAWINPUTDEVICE)
        ):
            error = ctypes.get_last_error()
            self.close()
            raise PadLendError(
                ErrorCode.RAWINPUT_FAILED,
                f"RegisterRawInputDevices 失败 err={error}",
            )
        self._prepared = True

    def pump(self, on_packet: Callable[[MousePacket], None]) -> None:
        user32 = _user32()
        self._on_packet = on_packet
        message = _MSG()
        while True:
            result = int(user32.GetMessageW(ctypes.byref(message), None, 0, 0))
            if result == 0:
                return
            if result == -1:
                raise PadLendError(
                    ErrorCode.RAWINPUT_FAILED,
                    f"GetMessageW 失败 err={ctypes.get_last_error()}",
                )
            user32.TranslateMessage(ctypes.byref(message))
            user32.DispatchMessageW(ctypes.byref(message))

    def request_stop(self) -> bool:
        if not self._thread_id:
            return False
        user32 = _user32()
        for _attempt in range(5):
            if user32.PostThreadMessageW(self._thread_id, WM_QUIT, 0, 0):
                return True
            time.sleep(0.01)
        return False

    def close(self) -> None:
        user32 = _user32()
        hwnd, self._hwnd = self._hwnd, None
        self._on_packet = None
        self._prepared = False
        if not hwnd:
            return
        _BACKENDS.pop(hwnd, None)
        try:
            removal = _RAWINPUTDEVICE(
                HID_USAGE_PAGE_GENERIC, HID_USAGE_GENERIC_MOUSE, RIDEV_REMOVE, None
            )
            user32.RegisterRawInputDevices(
                ctypes.byref(removal), 1, ctypes.sizeof(_RAWINPUTDEVICE)
            )
        except Exception:
            _log.debug("注销 Raw Input 设备失败", exc_info=True)
        try:
            user32.DestroyWindow(hwnd)
        except Exception:
            _log.debug("销毁 Raw Input 窗口失败", exc_info=True)

    # -- 消息处理 ---------------------------------------------------------
    def handle_message(self, hwnd: int, msg: int, wparam: int, lparam: int) -> int:
        user32 = _user32()
        if msg == WM_INPUT:
            try:
                packet = self._read_packet(lparam)
                if packet is not None and self._on_packet is not None:
                    self._on_packet(packet)
            except Exception:
                _log.exception("处理 WM_INPUT 失败")
            # 交回系统做 Raw Input 缓冲区清理。
            return int(user32.DefWindowProcW(hwnd, msg, wparam, lparam))
        if msg == WM_DESTROY:
            user32.PostQuitMessage(0)
            return 0
        return int(user32.DefWindowProcW(hwnd, msg, wparam, lparam))

    def _read_packet(self, lparam: int) -> MousePacket | None:
        user32 = _user32()
        header_size = ctypes.sizeof(_RAWINPUTHEADER)
        size = ctypes.c_uint32(0)
        result = int(
            user32.GetRawInputData(
                ctypes.c_void_p(lparam), RID_INPUT, None, ctypes.byref(size), header_size
            )
        )
        if result == _RID_ERROR or size.value < ctypes.sizeof(_RAWINPUT):
            return None
        buffer = ctypes.create_string_buffer(size.value)
        result = int(
            user32.GetRawInputData(
                ctypes.c_void_p(lparam),
                RID_INPUT,
                ctypes.cast(buffer, ctypes.c_void_p),
                ctypes.byref(size),
                header_size,
            )
        )
        if result == _RID_ERROR or result == 0:
            return None
        return decode_raw_input(buffer.raw[:result])


def _ensure_window_class(user32: ctypes.WinDLL) -> None:
    """注册（一次）message-only 窗口类；窗口过程是模块级的，因此可安全复用。"""
    global _WINDOW_PROC
    with _CLASS_LOCK:
        if _WINDOW_PROC is not None:
            return
        kernel32 = _kernel32()
        hinst = kernel32.GetModuleHandleW(None)
        if not hinst:
            raise PadLendError(ErrorCode.RAWINPUT_FAILED, "GetModuleHandleW 失败")
        window_proc = _WNDPROC_TYPE(_dispatch_to_backend)
        window_class = _WNDCLASSEXW()
        window_class.cbSize = ctypes.sizeof(_WNDCLASSEXW)
        window_class.lpfnWndProc = ctypes.cast(window_proc, ctypes.c_void_p)
        window_class.hInstance = hinst
        window_class.lpszClassName = _WINDOW_CLASS_NAME
        atom = user32.RegisterClassExW(ctypes.byref(window_class))
        if not atom:
            error = ctypes.get_last_error()
            if error != ERROR_CLASS_ALREADY_EXISTS:
                raise PadLendError(
                    ErrorCode.RAWINPUT_FAILED,
                    f"RegisterClassExW 失败 err={error}",
                )
        _WINDOW_PROC = window_proc  # 必须保持引用，否则窗口过程会被回收


# ---------------------------------------------------------------------------
# 对外入口
# ---------------------------------------------------------------------------
class RawInputSource:
    """T2：Raw Input 线程（设计文档 8.2 冻结接口）。

    ``on_event`` 在 rawinput 线程内被调用，只允许 ``queue.put_nowait``；
    回调抛出的异常会被记录并吞掉，不会终止捕获线程。
    """

    def __init__(
        self,
        on_event: Callable[[RawDelta], None],
        *,
        backend: RawInputBackend | None = None,
        screen_rect: Callable[[], Rect] | None = None,
    ) -> None:
        self._on_event = on_event
        self._backend: RawInputBackend = backend if backend is not None else Win32RawInputBackend()
        self._screen_rect = screen_rect if screen_rect is not None else virtual_screen_rect
        self._state = PointerState()
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._ready = threading.Event()
        self._startup_error: BaseException | None = None
        self._last_error: BaseException | None = None
        self._healthy = False
        self._last_event_monotonic = 0.0

    # -- 接口 -------------------------------------------------------------
    def start(self) -> None:
        """启动捕获线程；重复调用是幂等的。注册失败抛 ``E201``。"""
        with self._lock:
            if self._thread is not None and self._thread.is_alive():
                return
            self._ready = threading.Event()
            self._startup_error = None
            self._last_error = None
            self._healthy = False
            thread = threading.Thread(target=self._thread_main, name="rawinput", daemon=True)
            self._thread = thread
            thread.start()

        if not self._ready.wait(START_TIMEOUT_S):
            raise PadLendError(
                ErrorCode.RAWINPUT_FAILED,
                f"Raw Input 线程 {START_TIMEOUT_S:.0f}s 内未就绪",
            )
        error = self._startup_error
        if error is not None:
            if isinstance(error, PadLendError):
                raise error
            raise PadLendError(
                ErrorCode.RAWINPUT_FAILED,
                f"{type(error).__name__}: {error}",
                cause=error,
            ) from error

    def stop(self) -> None:
        """请求线程退出并 join；重复调用是幂等的。"""
        with self._lock:
            thread = self._thread
        if thread is None:
            return
        if not self._backend.request_stop():
            # 可能与线程启动竞争（消息队列尚未建立）——等它就绪后再投递一次。
            self._ready.wait(START_TIMEOUT_S)
            if not self._backend.request_stop():
                _log.warning("Raw Input 线程未就绪，等待其自然退出")
        thread.join(timeout=STOP_TIMEOUT_S)
        if thread.is_alive():
            _log.warning("Raw Input 线程 %.1fs 内未退出", STOP_TIMEOUT_S)
            return
        self._healthy = False
        with self._lock:
            if self._thread is thread:
                self._thread = None

    @property
    def healthy(self) -> bool:
        """线程已成功注册设备且仍在消息循环中。"""
        return self._healthy

    @property
    def last_error(self) -> BaseException | None:
        """最近一次线程级异常（诊断用）。"""
        return self._last_error

    @property
    def last_event_age(self) -> float:
        """距最后一个已分发的 Raw Input 事件过去的秒数（无事件时为 inf）。"""
        stamp = self._last_event_monotonic
        if stamp <= 0.0:
            return float("inf")
        return max(0.0, time.monotonic() - stamp)

    # -- 线程 -------------------------------------------------------------
    def _thread_main(self) -> None:
        try:
            self._backend.prepare()
        except Exception as exc:
            self._startup_error = exc
            self._last_error = exc
            _log.error("Raw Input 启动失败: %s", exc)
            self._healthy = False
            self._ready.set()
            self._close_quietly()
            return

        self._healthy = True
        self._ready.set()
        try:
            self._backend.pump(self._dispatch)
        except Exception as exc:
            self._last_error = exc
            _log.exception("Raw Input 线程异常退出")
        finally:
            self._healthy = False
            self._close_quietly()
            self._ready.set()

    def _dispatch(self, packet: MousePacket) -> None:
        """在 rawinput 线程内解释一个包并交给回调；绝不阻塞、绝不抛出。"""
        try:
            rect = self._screen_rect() if packet.flags & MOUSE_MOVE_ABSOLUTE else None
            delta = interpret(packet, self._state, rect)
        except Exception:
            _log.exception("解释 Raw Input 失败")
            return
        if delta is None:
            return
        self._last_event_monotonic = time.monotonic()
        try:
            self._on_event(delta)
        except Exception:
            _log.exception("Raw Input 回调异常（回调必须只入队）")

    def _close_quietly(self) -> None:
        try:
            self._backend.close()
        except Exception:
            _log.exception("关闭 Raw Input 后端失败")
