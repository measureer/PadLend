"""PadLend 组合根（main.py）。

只做编排，不实现业务：

1. 最早设置 Per-Monitor V2（遮罩/Raw Input 的物理坐标前提）；
2. 单实例、日志、配置、崩溃残留清理（``exit.clean``）；
3. 组装 DesktopMap / Adb / TabletSession / CursorMask / Router；
4. GUI 交给 :class:`ui.main_window.MainWindow`，无头模式跑 Router 主循环；
5. 退出时按顺序停止热键、Router、Session 并释放遮罩，最后写 ``exit.clean``。

``python -m padlend`` 与 ``padlend`` 控制台脚本都转调 :func:`main`。
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import logging
import os
import signal
import sys
import threading
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any

import constants as C
from config import Config, load_config
from core.errors import PadLendError
from core.logsetup import setup_logging

if TYPE_CHECKING:
    from tablet.session import DeviceFacts, TabletSession

LOG = logging.getLogger("padlend.main")

# 无头模式等待设备连接的窗口（与 supervisor 的首轮重试窗口一致）。
_HEADLESS_CONNECT_WAIT_S = float(C.INITIAL_RETRY_WINDOW_S)


# ---------------------------------------------------------------------------
# 命令行
# ---------------------------------------------------------------------------
def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="padlend",
        description="把 Android 平板变成 Windows 的扩展输入屏",
    )
    parser.add_argument("--headless", action="store_true", help="不启动 GUI，只跑接管引擎")
    parser.add_argument("--serial", default=None, help="指定 adb serial（默认自动选择）")
    parser.add_argument(
        "--config",
        default=None,
        help="配置文件路径（默认 %%APPDATA%%\\PadLend\\config.yaml）",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=0.0,
        help="无头模式运行秒数（0=直到 Ctrl+C）",
    )
    parser.add_argument(
        "--debug-raw",
        action="store_true",
        help="打印 Raw Input 计数后退出（诊断用）",
    )
    parser.add_argument("--no-wizard", action="store_true", help="首次运行也不弹向导")
    parser.add_argument(
        "--no-single-instance",
        action="store_true",
        help="跳过单实例锁（调试用）",
    )
    parser.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
    )
    parser.add_argument("--version", action="version", version=f"{C.APP_NAME} {C.VERSION}")
    return parser.parse_args(argv)


# ---------------------------------------------------------------------------
# 进程级前置
# ---------------------------------------------------------------------------
def _set_dpi_awareness() -> None:
    """必须早于任何窗口 / Raw Input（附录 D 与 win-input 的实测）。"""
    try:
        from win.dpi import set_dpi_awareness

        set_dpi_awareness()
    except Exception as exc:  # noqa: BLE001 - 旧系统没有 DPI API 时继续
        LOG.debug("DPI awareness unavailable: %r", exc)


def _force_utf8_console() -> None:
    """管道/重定向下 Windows 默认 cp936，中文日志会乱码（附录 D）。"""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is None:
            continue
        with contextlib.suppress(Exception):
            reconfigure(encoding="utf-8", errors="replace")


def _wake_existing_instance() -> None:
    """第二实例：把已有主窗还原并置前（7.5）。"""
    if sys.platform != "win32":
        return
    try:
        user32 = ctypes.windll.user32  # type: ignore[attr-defined]
    except (AttributeError, OSError):
        return

    found: list[int] = []
    enum_proc_type = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

    def _enum(hwnd: int, _lparam: int) -> bool:
        length = user32.GetWindowTextLengthW(hwnd)
        if not length:
            return True
        buf = ctypes.create_unicode_buffer(length + 1)
        user32.GetWindowTextW(hwnd, buf, length + 1)
        if C.APP_NAME.lower() in buf.value.lower():
            found.append(int(hwnd))
            return False
        return True

    try:
        user32.EnumWindows(enum_proc_type(_enum), 0)
    except Exception:  # noqa: BLE001 - 唤醒失败不应阻塞退出
        return
    if not found:
        return
    hwnd = found[0]
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    user32.SetForegroundWindow(hwnd)


def _stale_cleanup(cfg: Config) -> None:
    """``exit.clean`` 缺失 = 上次没正常退出；先清理幽灵设备/指针速度（12.3）。"""
    marker = C.clean_exit_marker()
    had_config = C.config_path().exists()
    if marker.exists() or not had_config:
        # 首次运行没有历史残留；写一次标记，避免第一次崩溃被误判。
        try:
            C.config_dir().mkdir(parents=True, exist_ok=True)
            marker.touch()
        except OSError:
            pass
        return
    LOG.warning("检测到上次未正常退出，执行强制清理")
    try:
        from tablet.adb import Adb
        from tablet.session import force_cleanup

        adb = Adb(serial=cfg.device.serial or None)
        force_cleanup(
            adb,
            cfg.device.serial,
            port=cfg.device.server_port,
            restore_pointer_speed=cfg.misc.restore_pointer_speed,
        )
    except PadLendError as exc:
        LOG.warning("强制清理跳过：%s", exc.user_message)
    except Exception as exc:  # noqa: BLE001 - 兜底路径不阻断启动
        LOG.warning("强制清理异常：%r", exc)


def _mark_running() -> None:
    """启动即删除 exit.clean；正常退出时再写回。"""
    try:
        C.config_dir().mkdir(parents=True, exist_ok=True)
        C.clean_exit_marker().unlink(missing_ok=True)
    except OSError:
        pass


def _mark_clean_exit() -> None:
    try:
        C.config_dir().mkdir(parents=True, exist_ok=True)
        C.clean_exit_marker().touch()
    except OSError:
        pass


# ---------------------------------------------------------------------------
# 后端组装
# ---------------------------------------------------------------------------
class Runtime:
    """一组协作对象；GUI 与无头模式共用。"""

    def __init__(self, cfg: Config) -> None:
        # 延迟导入：--version / 参数错误时不必加载 ctypes 后端。
        from core.router import Router
        from tablet.adb import Adb
        from tablet.session import TabletSession
        from win.desktop import DesktopMap
        from win.keyboard_hook import KeyboardHook
        from win.mask import CursorMask
        from win.rawinput import RawInputSource

        self.cfg = cfg
        self.adb = Adb(serial=cfg.device.serial or None)
        self.desktop = DesktopMap()
        self.session: TabletSession = TabletSession(cfg, adb=self.adb)
        self.mask: CursorMask = CursorMask()
        self.router: Router = Router(cfg, self.session, self.desktop, self.mask)
        # Raw Input 在独立线程回调 Router；是否接管由 Router 状态机决定。
        self.rawinput: RawInputSource = RawInputSource(self.router.on_raw)
        # 低级钩子必须在 GUI 线程外独立线程；回调只把事件交给 Router，
        # 是否吞键完全由 Router 的状态机/白名单决定。
        self.keyboard_hook: KeyboardHook = KeyboardHook(self.router.on_key)
        # 光标轮询兜底：Raw Input 在焦点/桌面切换后可能静默，但绝对光标仍在动；
        # 仅在 Raw Input 长时间无事件时合成位移，恢复后让位。
        self.cursor_watch: Any = None
        # 诊断模式：主线程计数由 Router 外的旁路 RawInput 读取，不影响接管。
        self.raw_debug: Any = None
        self.facts: DeviceFacts | None = None

    def start(self) -> None:
        self.router.start()
        try:
            self.rawinput.start()
            LOG.info("Raw Input 已启动")
        except PadLendError as exc:
            # 无 Raw Input 时鼠标仍可用，只是无法触发边缘接管；不阻断 GUI。
            LOG.warning("Raw Input 不可用：%s", exc.user_message)
        try:
            from win.cursor_watch import CursorWatch

            self.cursor_watch = CursorWatch(self.router, self.rawinput)
            self.cursor_watch.start()
        except Exception:
            LOG.debug("cursor watch 启动失败", exc_info=True)

    def stop(self) -> None:
        try:
            if self.cursor_watch is not None:
                try:
                    self.cursor_watch.stop()
                finally:
                    self.cursor_watch = None
            self.rawinput.stop()
        finally:
            try:
                self.keyboard_hook.uninstall()
            finally:
                try:
                    self.router.stop()
                finally:
                    try:
                        self.session.stop()
                    finally:
                        try:
                            self.mask.release()
                        finally:
                            if self.raw_debug is not None:
                                self.raw_debug.stop()


# ---------------------------------------------------------------------------
# 无头模式
# ---------------------------------------------------------------------------
def run_headless(cfg: Config, args: argparse.Namespace) -> int:
    from tablet.cursor import CursorEstimator
    from win.rawinput import RawInputSource

    runtime = Runtime(cfg)
    stop_event = threading.Event()
    source: RawInputSource | None = None

    def _on_state(old: Any, new: Any, reason: str) -> None:
        print(f"[state] {old} -> {new} ({reason})", flush=True)

    # S1-6：无 GUI 时把 Raw Input 位移直接换算成 UHID 相对报告，
    # 不经过边缘状态机（没有遮罩/ClipCursor），用于验证 1:1 指针链路。
    estimator: CursorEstimator | None = None
    estimator_key: tuple[int, int, float] | None = None
    wheel_accum = 0

    def _on_raw(ev: Any) -> None:
        nonlocal estimator, estimator_key, wheel_accum
        if not runtime.session.started:
            return
        facts = runtime.session.facts
        if facts is None:
            return
        key = (facts.width, facts.height, facts.xscale)
        if key != estimator_key:
            estimator = CursorEstimator(facts.width, facts.height, facts.xscale)
            estimator_key = key
            wheel_accum = 0
        if estimator is None:
            return
        cx, cy = estimator.feed(ev.dx, ev.dy, cfg.motion.gain)
        wheel_accum += int(ev.wheel)
        detents = int(wheel_accum / C.WHEEL_UNITS_PER_DETENT)
        if detents:
            wheel_accum -= detents * C.WHEEL_UNITS_PER_DETENT
        try:
            runtime.session.send_mouse(ev.buttons, cx, cy, detents)
        except PadLendError:
            # 断线期间丢弃；supervisor 会重连并重新发布 facts。
            return

    runtime.router.on_state_changed = _on_state
    runtime.router.start()
    print(f"{C.APP_NAME} {C.VERSION} headless；按 Ctrl+C 退出", flush=True)

    deadline = time.monotonic() + (_HEADLESS_CONNECT_WAIT_S if not args.duration else args.duration)
    connected = False
    try:
        while not stop_event.is_set():
            now = time.monotonic()
            if args.duration and now >= deadline:
                break
            if runtime.session.started:
                if not connected:
                    facts = runtime.session.facts
                    if facts is None:
                        # session 先置 started，随后才发布 facts；等下一轮再查。
                        # 定时模式已在循环顶部按 deadline 退出，不会卡在这里。
                        stop_event.wait(0.05)
                        continue
                    runtime.facts = facts
                    print(
                        f"[device] {facts.serial} {facts.width}x{facts.height} "
                        f"orientation={facts.orientation} xscale={facts.xscale}",
                        flush=True,
                    )
                    connected = True
                    if source is None:
                        try:
                            source = RawInputSource(_on_raw)
                            source.start()
                            print("[input] Raw Input 已启动，鼠标位移 1:1 发送到平板", flush=True)
                        except PadLendError as exc:
                            LOG.warning("Raw Input 不可用：%s", exc.user_message)
            elif not connected and now >= deadline:
                print(
                    "[error] 未发现可用设备（见 tools/probe.py 或 docs/TROUBLESHOOTING.md）",
                    flush=True,
                )
                return 2
            stop_event.wait(0.25)
    except KeyboardInterrupt:
        print("\n收到 Ctrl+C", flush=True)
    finally:
        if source is not None:
            source.stop()
        runtime.stop()
        _mark_clean_exit()
    return 0


# ---------------------------------------------------------------------------
# Raw Input 诊断
# ---------------------------------------------------------------------------
def run_raw_debug() -> int:
    from win.rawinput import RawInputSource

    count = 0
    last = time.monotonic()

    def _on_raw(ev: Any) -> None:
        nonlocal count
        count += 1

    source = RawInputSource(_on_raw)
    source.start()
    print("Raw Input 已启动；移动鼠标/滚轮，Ctrl+C 退出", flush=True)
    try:
        while True:
            time.sleep(1.0)
            now = time.monotonic()
            print(f"[raw] {count} events / {now - last:.1f}s", flush=True)
            count = 0
            last = now
    except KeyboardInterrupt:
        return 0
    finally:
        source.stop()


# ---------------------------------------------------------------------------
# GUI
# ---------------------------------------------------------------------------
def run_gui(cfg: Config, args: argparse.Namespace, first_run: bool) -> int:
    from PySide6.QtCore import QTimer
    from PySide6.QtWidgets import QApplication

    from ui.main_window import MainWindow
    from ui.style import apply_theme

    app = QApplication.instance() or QApplication([sys.argv[0]])
    app.setApplicationName(C.APP_NAME)
    app.setApplicationDisplayName(C.APP_NAME)
    app.setApplicationVersion(C.VERSION)
    app.setQuitOnLastWindowClosed(False)  # 托盘常驻

    apply_theme(app, getattr(cfg.ui, "theme", "dark"))
    runtime = Runtime(cfg)
    window = MainWindow(
        cfg,
        runtime.router,
        runtime.desktop,
        config_path=Path(args.config) if args.config else None,
        adb=runtime.adb,
    )
    window.show()

    hotkeys = None
    if sys.platform == "win32":
        from win.hotkeys import HotkeyManager

        def _toggle() -> None:
            try:
                runtime.router.toggle_manual()
            except Exception as exc:  # noqa: BLE001 - 热键线程不能抛
                LOG.warning("toggle_manual failed: %r", exc)

        def _panic() -> None:
            action = getattr(runtime.router, "emergency_stop", None) or getattr(
                runtime.router, "stop", None
            )
            try:
                if callable(action):
                    action()
            except Exception as exc:  # noqa: BLE001
                LOG.warning("panic failed: %r", exc)
            finally:
                with contextlib.suppress(Exception):
                    runtime.mask.release()

        hotkeys = HotkeyManager(_toggle, _panic, window.clipboardSyncRequested.emit)
        try:
            hotkeys.start()
        except PadLendError as exc:
            LOG.warning("全局热键不可用：%s", exc.user_message)
            hotkeys = None

    runtime.start()

    if sys.platform == "win32":
        try:
            runtime.keyboard_hook.install()
            LOG.info("键盘钩子已安装")
        except PadLendError as exc:
            # 钩子失败不阻断鼠标链路；GUI 仍可用，只是键盘不转发/不屏蔽。
            LOG.warning("键盘钩子不可用：%s", exc.user_message)

    # 设备信息同步到设备页/顶栏/画布（只在事实变化时）。
    last_facts: tuple[Any, ...] | None = None

    def _sync_device() -> None:
        nonlocal last_facts
        facts = getattr(runtime.router, "facts", None)
        key = None
        if facts is not None:
            key = (facts.serial, facts.width, facts.height, facts.orientation)
        if key == last_facts:
            return
        last_facts = key
        runtime.facts = facts
        window.update_device(facts)

    timer = QTimer()
    timer.setInterval(1000)
    timer.timeout.connect(_sync_device)
    timer.start()

    stopping = threading.Event()

    def _shutdown() -> None:
        if stopping.is_set():
            return
        stopping.set()
        timer.stop()
        if hotkeys is not None:
            try:
                hotkeys.stop()
            except Exception as exc:  # noqa: BLE001
                LOG.debug("hotkeys.stop failed: %r", exc)
        try:
            runtime.stop()
        finally:
            # 托盘/窗口可能已经销毁，不再触碰 Qt 对象。
            with contextlib.suppress(Exception):
                window._shutdown()  # noqa: SLF001 - 组合根负责最终清理
            _mark_clean_exit()
            LOG.info("已退出")

    app.aboutToQuit.connect(_shutdown)

    # Ctrl+C（控制台）/ SIGTERM：交给 Qt 事件循环退出。
    def _signal_handler(_signum: int, _frame: Any) -> None:
        QTimer.singleShot(0, app.quit)

    try:
        signal.signal(signal.SIGINT, _signal_handler)
        signal.signal(signal.SIGTERM, _signal_handler)
    except (ValueError, OSError):
        pass

    if first_run and not args.no_wizard:
        QTimer.singleShot(400, window.show_wizard)

    try:
        return int(app.exec())
    finally:
        _shutdown()


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------
def main(argv: list[str] | None = None) -> int:
    _force_utf8_console()
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    # 必须在任何窗口/Raw Input 之前。
    _set_dpi_awareness()

    setup_logging(
        level=getattr(logging, args.log_level, logging.INFO),
        log_file=C.log_dir() / "padlend.log",
    )
    LOG.info("%s %s 启动 (pid=%d)", C.APP_NAME, C.VERSION, os.getpid())

    config_file = Path(args.config) if args.config else C.config_path()
    first_run = not config_file.exists()
    cfg = load_config(config_file)
    if args.serial:
        cfg.device.serial = args.serial
    issues = cfg.validate()
    if issues:
        LOG.warning("配置校验发现问题：%s", "; ".join(issues))
    if first_run:
        try:
            cfg.save(config_file)
        except OSError as exc:
            LOG.warning("无法写入默认配置：%r", exc)

    if args.debug_raw:
        return run_raw_debug()

    acquired = True
    if not args.no_single_instance:
        from win.single_instance import acquire, release

        acquired = acquire()
        if not acquired:
            LOG.info("已有实例在运行，唤醒后退出")
            _wake_existing_instance()
            return 0

    try:
        _stale_cleanup(cfg)
        _mark_running()
        if args.headless:
            return run_headless(cfg, args)
        return run_gui(cfg, args, first_run)
    except PadLendError as exc:
        LOG.error("%s", exc.user_message)
        print(f"[error] {exc.user_message}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 0
    finally:
        if acquired:
            with contextlib.suppress(Exception):
                release()


if __name__ == "__main__":
    raise SystemExit(main())
