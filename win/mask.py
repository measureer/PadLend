"""光标遮罩与 ClipCursor（设计文档 6.6 / 8.2 / 12.3）。

接管期把 PC 光标"藏起来"：``ClipCursor`` 把它钉在屏幕边缘 1px 条上，同时每个显示器
盖一个几乎全透明（alpha=1）的 ``WS_EX_LAYERED | WS_EX_TRANSPARENT`` 窗口。
遮罩窗口：

- 不抢焦点（``WS_EX_NOACTIVATE`` + ``SW_SHOWNOACTIVATE`` + ``SWP_NOACTIVATE``）；
- 不进 Alt-Tab（``WS_EX_TOOLWINDOW``）；
- 不接鼠标（``WS_EX_TRANSPARENT`` 点击穿透）；
- 永远不调用 ``SetForegroundWindow``。

**`ClipCursor` 泄漏是灾难级故障**（6.6 的教训）：用户光标会被锁死在 1px 条里。
因此 `release()` 幂等、无异常、``ClipCursor(None)`` 走无条件路径，并在首次
``show()`` 时注册 ``atexit`` 兜底（12.3 第一层）。

导入本模块不创建窗口、不锁光标（实施说明 2.6）。
"""

from __future__ import annotations

import atexit
import ctypes
import logging
import sys
import threading
from typing import Protocol

import constants
from core.geometry import Edge, EdgeSegment, Rect

_log = logging.getLogger("win.mask")

# 遮罩扩展样式（6.6 的 EX_STYLE）
WS_EX_LAYERED = 0x00080000
WS_EX_TRANSPARENT = 0x00000020
WS_EX_NOACTIVATE = 0x08000000
WS_EX_TOOLWINDOW = 0x00000080
_MASK_EX_STYLE = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW

WS_POPUP = 0x80000000
SW_SHOWNOACTIVATE = 4
LWA_ALPHA = 0x00000002
SWP_NOACTIVATE = 0x0010
SWP_SHOWWINDOW = 0x0040

HWND_TOPMOST = -1

SM_CXSCREEN = 0
SM_CYSCREEN = 1
SM_XVIRTUALSCREEN = 76
SM_YVIRTUALSCREEN = 77
SM_CXVIRTUALSCREEN = 78
SM_CYVIRTUALSCREEN = 79

ERROR_CLASS_ALREADY_EXISTS = 1410

_WINDOW_CLASS_NAME = "PadLendCursorMaskWindow"
_WINDOW_TITLE = "PadLend Cursor Mask"


# ---------------------------------------------------------------------------
# Win32 结构体
# ---------------------------------------------------------------------------
class _RECT(ctypes.Structure):
    _fields_ = [
        ("left", ctypes.c_int32),
        ("top", ctypes.c_int32),
        ("right", ctypes.c_int32),
        ("bottom", ctypes.c_int32),
    ]


class _MONITORINFO(ctypes.Structure):
    _fields_ = [
        ("cbSize", ctypes.c_uint32),
        ("rcMonitor", _RECT),
        ("rcWork", _RECT),
        ("dwFlags", ctypes.c_uint32),
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


def _wndproc_type() -> type:
    maker = getattr(ctypes, "WINFUNCTYPE", ctypes.CFUNCTYPE)
    return maker(
        ctypes.c_ssize_t,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_size_t,
        ctypes.c_ssize_t,
    )


def _monitor_enum_proc_type() -> type:
    maker = getattr(ctypes, "WINFUNCTYPE", ctypes.CFUNCTYPE)
    return maker(
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.POINTER(_RECT),
        ctypes.c_ssize_t,
    )


_WNDPROC_TYPE = _wndproc_type()
_MONITORENUMPROC_TYPE = _monitor_enum_proc_type()


# ---------------------------------------------------------------------------
# Win32 加载（延迟；import 不碰 user32）
# ---------------------------------------------------------------------------
_user32_lib: ctypes.WinDLL | None = None
_kernel32_lib: ctypes.WinDLL | None = None
_class_lock = threading.Lock()
_window_proc: object | None = None


def _require_windows() -> None:
    if sys.platform != "win32":
        raise OSError("光标遮罩仅支持 Windows")


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
        lib.DestroyWindow.argtypes = [ctypes.c_void_p]
        lib.DestroyWindow.restype = ctypes.c_int
        lib.ShowWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
        lib.ShowWindow.restype = ctypes.c_int
        lib.SetLayeredWindowAttributes.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_uint8,
            ctypes.c_uint32,
        ]
        lib.SetLayeredWindowAttributes.restype = ctypes.c_int
        lib.SetWindowPos.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_uint32,
        ]
        lib.SetWindowPos.restype = ctypes.c_int
        lib.ClipCursor.argtypes = [ctypes.POINTER(_RECT)]
        lib.ClipCursor.restype = ctypes.c_int
        lib.GetClipCursor.argtypes = [ctypes.POINTER(_RECT)]
        lib.GetClipCursor.restype = ctypes.c_int
        lib.EnumDisplayMonitors.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            _MONITORENUMPROC_TYPE,
            ctypes.c_ssize_t,
        ]
        lib.EnumDisplayMonitors.restype = ctypes.c_int
        lib.GetMonitorInfoW.argtypes = [ctypes.c_void_p, ctypes.POINTER(_MONITORINFO)]
        lib.GetMonitorInfoW.restype = ctypes.c_int
        lib.GetSystemMetrics.argtypes = [ctypes.c_int]
        lib.GetSystemMetrics.restype = ctypes.c_int
        _user32_lib = lib
    return _user32_lib


def _kernel32() -> ctypes.WinDLL:
    global _kernel32_lib
    _require_windows()
    if _kernel32_lib is None:
        lib = ctypes.WinDLL("kernel32", use_last_error=True)
        lib.GetModuleHandleW.argtypes = [ctypes.c_wchar_p]
        lib.GetModuleHandleW.restype = ctypes.c_void_p
        _kernel32_lib = lib
    return _kernel32_lib


def _virtual_desktop_rect() -> Rect:
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
# 平台后端接口（测试注入假实现）
# ---------------------------------------------------------------------------
class MaskApi(Protocol):
    """遮罩用到的全部平台调用；单测用 FakeMaskApi 替换。"""

    def monitor_rects(self) -> list[Rect]: ...

    def create_mask_window(self, rect: Rect) -> int: ...

    def destroy_window(self, hwnd: int) -> None: ...

    def clip_cursor(self, rect: Rect | None) -> bool: ...

    def clip_rect(self) -> Rect | None: ...


class Win32MaskApi:
    """真实 Win32 实现。

    需要进程处于 per-monitor DPI aware（``win/dpi.py`` 在启动早期设置 PMv2），
    否则 ``GetMonitorInfoW`` / ``GetSystemMetrics`` 返回的是被虚拟化的逻辑坐标，
    遮罩会错位。
    """

    def monitor_rects(self) -> list[Rect]:
        user32 = _user32()
        rects: list[Rect] = []
        callback_type = _MONITORENUMPROC_TYPE

        def _on_monitor(hmonitor: int, hdc: int, rect: object, data: int) -> int:
            info = _MONITORINFO()
            info.cbSize = ctypes.sizeof(_MONITORINFO)
            if user32.GetMonitorInfoW(hmonitor, ctypes.byref(info)):
                rc = info.rcMonitor
                rects.append(Rect(rc.left, rc.top, rc.right, rc.bottom))
            return 1

        callback = callback_type(_on_monitor)
        user32.EnumDisplayMonitors(None, None, callback, 0)
        if not rects:
            _log.warning("EnumDisplayMonitors 未返回显示器，退回虚拟桌面包围盒")
            rects.append(_virtual_desktop_rect())
        return rects

    def create_mask_window(self, rect: Rect) -> int:
        user32 = _user32()
        _ensure_window_class()
        hinst = _kernel32().GetModuleHandleW(None)
        width = max(1, rect.width)
        height = max(1, rect.height)
        hwnd = user32.CreateWindowExW(
            _MASK_EX_STYLE,
            _WINDOW_CLASS_NAME,
            _WINDOW_TITLE,
            WS_POPUP,
            rect.left,
            rect.top,
            width,
            height,
            None,
            None,
            hinst,
            None,
        )
        if not hwnd:
            raise OSError(f"CreateWindowExW 失败 err={ctypes.get_last_error()}")
        # alpha=1：近乎全透明，用户不应看到任何遮挡（6.6）。
        user32.SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA)
        user32.ShowWindow(hwnd, SW_SHOWNOACTIVATE)
        # 用物理矩形重排一次，确保跨显示器 / DPI 变化后位置正确。
        user32.SetWindowPos(
            hwnd,
            ctypes.c_void_p(HWND_TOPMOST & 0xFFFFFFFFFFFFFFFF),
            rect.left,
            rect.top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        return int(hwnd)

    def destroy_window(self, hwnd: int) -> None:
        _user32().DestroyWindow(ctypes.c_void_p(hwnd))

    def clip_cursor(self, rect: Rect | None) -> bool:
        user32 = _user32()
        if rect is None:
            return bool(user32.ClipCursor(None))
        native = _RECT(rect.left, rect.top, rect.right, rect.bottom)
        return bool(user32.ClipCursor(ctypes.byref(native)))

    def clip_rect(self) -> Rect | None:
        native = _RECT()
        if not _user32().GetClipCursor(ctypes.byref(native)):
            return None
        return Rect(native.left, native.top, native.right, native.bottom)


def _mask_window_proc(hwnd: int, msg: int, wparam: int, lparam: int) -> int:
    return int(_user32().DefWindowProcW(hwnd, msg, wparam, lparam))


def _ensure_window_class() -> None:
    """注册（一次）遮罩窗口类；用的是默认窗口过程，不需要消息处理。"""
    global _window_proc
    with _class_lock:
        if _window_proc is not None:
            return
        user32 = _user32()
        hinst = _kernel32().GetModuleHandleW(None)
        proc = _WNDPROC_TYPE(_mask_window_proc)
        window_class = _WNDCLASSEXW()
        window_class.cbSize = ctypes.sizeof(_WNDCLASSEXW)
        window_class.lpfnWndProc = ctypes.cast(proc, ctypes.c_void_p)
        window_class.hInstance = hinst
        window_class.lpszClassName = _WINDOW_CLASS_NAME
        atom = user32.RegisterClassExW(ctypes.byref(window_class))
        if not atom:
            error = ctypes.get_last_error()
            if error != ERROR_CLASS_ALREADY_EXISTS:
                raise OSError(f"RegisterClassExW 失败 err={error}")
        _window_proc = proc  # 保持引用，避免窗口过程被回收


# ---------------------------------------------------------------------------
# 几何：边缘 1px 条
# ---------------------------------------------------------------------------
def clip_rect_for_segment(
    monitor_rect: Rect,
    segment: EdgeSegment,
    *,
    thickness: int | None = None,
) -> Rect:
    """把外轮廓边段换算成 ``ClipCursor`` 用的边缘条（6.6：边缘 1px 条）。

    条带沿边方向被 ``span_start``/``span_end`` 限制在显示器范围内；如果边段完全
    落在显示器之外（摆位过期），退回整条边 —— 宁可锁住整条边，也不要产生非法矩形。
    """
    band = constants.EDGE_BAND_PX if thickness is None else thickness
    band = max(1, band)
    edge = Edge(segment.edge)

    if edge is Edge.LEFT:
        top = max(monitor_rect.top, segment.span_start)
        bottom = min(monitor_rect.bottom, segment.span_end)
        if bottom <= top:
            top, bottom = monitor_rect.top, monitor_rect.bottom
        return Rect(
            monitor_rect.left,
            top,
            min(monitor_rect.left + band, monitor_rect.right),
            bottom,
        )
    if edge is Edge.RIGHT:
        top = max(monitor_rect.top, segment.span_start)
        bottom = min(monitor_rect.bottom, segment.span_end)
        if bottom <= top:
            top, bottom = monitor_rect.top, monitor_rect.bottom
        return Rect(
            max(monitor_rect.right - band, monitor_rect.left),
            top,
            monitor_rect.right,
            bottom,
        )
    if edge is Edge.TOP:
        left = max(monitor_rect.left, segment.span_start)
        right = min(monitor_rect.right, segment.span_end)
        if right <= left:
            left, right = monitor_rect.left, monitor_rect.right
        return Rect(
            left,
            monitor_rect.top,
            right,
            min(monitor_rect.top + band, monitor_rect.bottom),
        )
    left = max(monitor_rect.left, segment.span_start)
    right = min(monitor_rect.right, segment.span_end)
    if right <= left:
        left, right = monitor_rect.left, monitor_rect.right
    return Rect(
        left,
        max(monitor_rect.bottom - band, monitor_rect.top),
        right,
        monitor_rect.bottom,
    )


# ---------------------------------------------------------------------------
# atexit 兜底（12.3 第一层）
# ---------------------------------------------------------------------------
_guard_lock = threading.Lock()
_guard_registered = False


def _atexit_unclip() -> None:
    """进程退出前最后一道保险：永远不要让 ``ClipCursor`` 泄漏。"""
    try:
        if sys.platform == "win32":
            _user32().ClipCursor(None)
    except Exception:
        _log.debug("atexit ClipCursor(None) 失败", exc_info=True)


def _register_atexit_guard() -> None:
    global _guard_registered
    with _guard_lock:
        if _guard_registered:
            return
        atexit.register(_atexit_unclip)
        _guard_registered = True


# ---------------------------------------------------------------------------
# 对外入口
# ---------------------------------------------------------------------------
class CursorMask:
    """接管期藏掉 PC 光标；不抢焦点、不接鼠标、不进 Alt-Tab（6.6 / 8.2）。

    语义：

    - ``show(rect)``：显示遮罩并把光标锁到 ``rect``（Router 传边缘 1px 条）；
    - ``show(None)``：只显示遮罩，**不改动**当前 ``ClipCursor``；
    - ``hide()``：销毁遮罩，并解除自己持有的锁；
    - ``release()``：幂等、无异常的兜底释放 —— 无条件 ``ClipCursor(None)`` 后销毁窗口。
    """

    EX_STYLE = _MASK_EX_STYLE

    def __init__(self, *, api: MaskApi | None = None) -> None:
        self._api: MaskApi = api if api is not None else Win32MaskApi()
        self._lock = threading.RLock()
        self._windows: list[int] = []
        self._active = False
        self._clip_owned = False
        self._clip_rect: Rect | None = None

    # -- 状态 -------------------------------------------------------------
    @property
    def active(self) -> bool:
        """当前是否处于接管状态（遮罩已显示）。"""
        return self._active

    @property
    def clip_owned(self) -> bool:
        """当前的光标锁定是否由本对象持有（诊断 / I2 一致性检查用）。"""
        return self._clip_owned

    @property
    def clip_rect(self) -> Rect | None:
        """本对象最后一次成功设置的光标裁剪矩形。"""
        return self._clip_rect

    # -- 生命周期 ---------------------------------------------------------
    def show(self, clip_rect: Rect | None) -> None:
        """显示遮罩（每显示器一个窗口），可选地把光标锁到 ``clip_rect``。"""
        with self._lock:
            _register_atexit_guard()
            try:
                self._create_windows()
                self._active = True
                if clip_rect is None:
                    if self._clip_owned:
                        self._apply_clip(None)
                else:
                    self._apply_clip(clip_rect)
            except Exception:
                self._cleanup_quietly(unclip=True)
                raise

    def hide(self) -> None:
        """销毁遮罩窗口；只解除本对象持有的光标锁。幂等。"""
        with self._lock:
            if self._clip_owned:
                self._apply_clip(None)
            self._destroy_windows()
            self._active = False

    def release(self) -> None:
        """兜底释放：无条件 ``ClipCursor(None)``，再销毁窗口。幂等且绝不抛异常。"""
        with self._lock:
            self._clip_owned = False
            self._clip_rect = None
            try:
                self._api.clip_cursor(None)
            except ImportError:
                # 解释器关机阶段 ctypes 已无法加载 DLL；进程退出时 Windows
                # 会自动解除剪贴，静默跳过以免析构器刷 traceback。
                pass
            except Exception:
                _log.exception("release: ClipCursor(None) 失败")
            finally:
                self._destroy_windows()
                self._active = False

    # -- 内部 -------------------------------------------------------------
    def _create_windows(self) -> None:
        if self._windows:
            return
        created: list[int] = []
        try:
            for rect in self._api.monitor_rects():
                created.append(self._api.create_mask_window(rect))
        except Exception:
            for hwnd in created:
                try:
                    self._api.destroy_window(hwnd)
                except Exception:
                    _log.exception("回滚遮罩窗口失败 hwnd=%s", hwnd)
            raise
        self._windows.extend(created)

    def _destroy_windows(self) -> None:
        windows, self._windows = self._windows, []
        for hwnd in windows:
            try:
                self._api.destroy_window(hwnd)
            except Exception:
                _log.exception("销毁遮罩窗口失败 hwnd=%s", hwnd)

    def _apply_clip(self, rect: Rect | None) -> None:
        try:
            ok = bool(self._api.clip_cursor(rect))
        except Exception:
            _log.exception("ClipCursor 调用失败")
            ok = False
        if rect is None:
            self._clip_owned = False
            self._clip_rect = None
            return
        if ok:
            self._clip_owned = True
            self._clip_rect = rect
        else:
            # 锁不住不是致命错误：遮罩仍然显示，但要留下诊断线索。
            _log.warning("ClipCursor(%s) 失败，遮罩已显示但光标未锁定", rect)
            self._clip_owned = False
            self._clip_rect = None

    def _cleanup_quietly(self, *, unclip: bool) -> None:
        if unclip and self._clip_owned:
            try:
                self._api.clip_cursor(None)
            except Exception:
                _log.exception("回滚 ClipCursor(None) 失败")
            self._clip_owned = False
            self._clip_rect = None
        self._destroy_windows()
        self._active = False
