"""PadLend 组合根（main.py）。

1.1.0 起界面由 Tauri 2 + React/TS 承担，本模块只做编排，不含任何 GUI 代码：

1. 最早设置 Per-Monitor V2（Raw Input 物理坐标的前提）；
2. 组装 DesktopMap / Adb / TabletSession / CursorMask / Router 为 :class:`Runtime`；
3. 默认入口转调 ``service.server.main``（NDJSON 业务服务，供 Tauri 外壳调用）；
4. 保留 ``--headless``（引擎验收）与 ``--debug-raw``（Raw Input 诊断）两个开发模式。

``python -m padlend`` 与 ``padlend`` 控制台脚本都转调 :func:`main`。
"""

from __future__ import annotations

import argparse
import contextlib
import logging
import os
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
    from tablet.session import DeviceFacts

LOG = logging.getLogger("padlend.main")

# 无头模式等待设备连接的窗口（与 supervisor 的首轮重试窗口一致）。
_HEADLESS_CONNECT_WAIT_S = float(C.INITIAL_RETRY_WINDOW_S)


def _safe_print(message: str) -> None:
    """console=False 打包后 stdout/stderr 可能是 None，输出失败不能二次崩溃。"""
    try:
        if sys.stderr is not None:
            print(message, file=sys.stderr, flush=True)
    except (OSError, ValueError):
        pass


def _safe_log_dir() -> Path | None:
    try:
        return C.log_dir()
    except Exception:  # noqa: BLE001 - 日志目录失败也要能启动
        return None


# ---------------------------------------------------------------------------
# 命令行
# ---------------------------------------------------------------------------
def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="padlend",
        description="把 Android 平板变成 Windows 的扩展输入屏（Tauri 业务服务）",
    )
    parser.add_argument("--headless", action="store_true", help="只跑接管引擎，不启动业务服务")
    parser.add_argument("--serial", default=None, help="指定 adb serial（默认自动选择）")
    parser.add_argument(
        "--config",
        default=None,
        help="配置文件路径（默认 %%APPDATA%%\\PadLend\\config.yaml）",
    )
    parser.add_argument(
        "--duration", type=float, default=0.0, help="无头模式运行秒数（0=直到 Ctrl+C）"
    )
    parser.add_argument("--debug-raw", action="store_true", help="打印 Raw Input 计数后退出")
    parser.add_argument(
        "--simulate",
        action="store_true",
        default=bool(os.environ.get("PADLEND_SIMULATE")),
        help="用内存里的假平板代替真机（验收/演示用）",
    )
    parser.add_argument("--no-single-instance", action="store_true", help="跳过单实例锁（调试用）")
    parser.add_argument(
        "--log-level", default="INFO", choices=["DEBUG", "INFO", "WARNING", "ERROR"]
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


def _cleanup_orphan_mask() -> None:
    """收掉上一个 sidecar 可能留下的 topmost 遮罩，并解除 ClipCursor。

    必须早于单实例锁：挂起的旧进程可能仍持有互斥体，晚一步清就会先被
    “已有实例在运行”挡回去，屏幕上的遮罩永远没人收。
    """
    try:
        from win.mask import cleanup_orphan_mask

        cleanup_orphan_mask()
    except Exception as exc:  # noqa: BLE001 - 清理失败不能阻断启动
        LOG.debug("孤儿遮罩清理跳过: %r", exc)


def _stale_cleanup(cfg: Config, *, first_run: bool = False) -> None:
    """``exit.clean`` 缺失 = 上次没正常退出；先清理幽灵设备/指针速度（12.3）。"""
    marker = C.clean_exit_marker()
    # first_run 必须由调用方在写配置文件之前判定：service/api.py 会先保存默认
    # 配置再调用本函数，若这里再看 config.yaml 是否存在，首次启动就会被误判成
    # “上次未正常退出”（1.2.0 修复的假阳性）。
    had_config = not first_run and C.config_path().exists()
    # 服务模式没有单实例互斥体，在这里再清一次；_run 已在加锁前清过，
    # 两次都幂等。
    _cleanup_orphan_mask()
    try:
        from tablet.adb import Adb
        from tablet.session import (
            force_cleanup,
            load_pointer_state,
            load_screen_timeout_state,
            restore_pending_pointer_speed,
            restore_pending_screen_timeout,
        )
    except Exception as exc:  # noqa: BLE001 - adb 都不可用时照常启动
        LOG.debug("adb 后端不可用，跳过残留清理: %r", exc)
        return

    adb = None
    pending = load_pointer_state()
    pending_screen = load_screen_timeout_state()
    if pending is not None or pending_screen is not None or (marker.exists() and had_config):
        try:
            adb = Adb(serial=cfg.device.serial or None)
        except PadLendError as exc:
            LOG.warning("adb 不可用，跳过残留清理：%s", exc.user_message)
            adb = None

    if adb is not None and pending is not None:
        with contextlib.suppress(Exception):
            restore_pending_pointer_speed(adb)

    if adb is not None and pending_screen is not None:
        with contextlib.suppress(Exception):
            restore_pending_screen_timeout(adb)

    if marker.exists() or not had_config:
        try:
            C.config_dir().mkdir(parents=True, exist_ok=True)
            marker.touch()
        except OSError:
            pass
        return
    LOG.warning("检测到上次未正常退出，执行强制清理")
    if adb is None:
        return
    try:
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
    """一组协作对象；业务服务与无头模式共用。"""

    def __init__(
        self,
        cfg: Config,
        *,
        simulate: bool = False,
        candidates: Any = None,
        verify_facts: Any = None,
    ) -> None:
        from core.router import Router
        from win.desktop import DesktopMap
        from win.keyboard_hook import KeyboardHook
        from win.mask import CursorMask
        from win.rawinput import RawInputSource

        self.cfg = cfg
        self.simulated = bool(simulate)
        if self.simulated:
            from tablet.simulated import SimulatedAdb, SimulatedSession

            self.adb: Any = SimulatedAdb()
            self.session: Any = SimulatedSession()
        else:
            from tablet.adb import Adb
            from tablet.session import TabletSession

            self.adb = Adb(serial=cfg.device.serial or None)
            self.session = TabletSession(
                cfg,
                adb=self.adb,
                candidates=candidates,
                verify_facts=verify_facts,
            )
        self.desktop = DesktopMap()
        self.mask: CursorMask = CursorMask()
        self.router: Router = Router(cfg, self.session, self.desktop, self.mask)
        self.rawinput: RawInputSource = RawInputSource(self.router.on_raw)
        self.keyboard_hook: KeyboardHook = KeyboardHook(self.router.on_key)
        self.cursor_watch: Any = None
        self.raw_debug: Any = None
        self.facts: DeviceFacts | None = None
        self._stop_lock = threading.Lock()
        self._stopped = False

    def start(self) -> None:
        self.router.start()
        try:
            self.rawinput.start()
            LOG.info("Raw Input 已启动")
        except PadLendError as exc:
            LOG.warning("Raw Input 不可用：%s", exc.user_message)
        try:
            from win.cursor_watch import CursorWatch

            self.cursor_watch = CursorWatch(self.router, self.rawinput)
            self.cursor_watch.start()
        except Exception:
            LOG.debug("cursor watch 启动失败", exc_info=True)

    def stop(self) -> None:
        # teardown 可能被正常退出、父进程看门狗、RPC shutdown 三条路径同时触发；
        # 先到的执行，后到的直接返回，避免重复 kill 子进程/重复释放遮罩。
        with self._stop_lock:
            if self._stopped:
                return
            self._stopped = True
        try:
            try:
                self.keyboard_hook.uninstall()
            finally:
                try:
                    self.router.stop()
                finally:
                    if self.cursor_watch is not None:
                        try:
                            self.cursor_watch.stop()
                        finally:
                            self.cursor_watch = None
                    self.rawinput.stop()
        finally:
            try:
                self.session.stop()
            finally:
                try:
                    self.mask.release()
                finally:
                    if self.raw_debug is not None:
                        self.raw_debug.stop()
                    try:
                        if self.adb is not None:
                            self.adb.kill_server()
                    except Exception as exc:  # noqa: BLE001 - 清理失败不阻断退出
                        LOG.debug("adb kill-server failed: %r", exc)


# ---------------------------------------------------------------------------
# 无头模式（引擎验收）
# ---------------------------------------------------------------------------
def run_headless(cfg: Config, args: argparse.Namespace) -> int:
    from tablet.cursor import CursorEstimator
    from win.rawinput import RawInputSource

    runtime = Runtime(cfg, simulate=getattr(args, "simulate", False))
    stop_event = threading.Event()
    source: RawInputSource | None = None

    def _on_state(old: Any, new: Any, reason: str) -> None:
        print(f"[state] {old} -> {new} ({reason})", flush=True)

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
                print("[error] 未发现可用设备（见 tools/probe.py）", flush=True)
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


def run_raw_debug() -> int:
    from win.rawinput import RawInputSource

    count = 0
    last = time.monotonic()

    def _on_raw(_ev: Any) -> None:
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
# 入口
# ---------------------------------------------------------------------------
def _service_argv(args: argparse.Namespace) -> list[str]:
    """把兼容 CLI 的参数映射成业务服务参数。"""
    argv: list[str] = ["--log-level", args.log_level]
    if args.simulate:
        argv.append("--simulate")
    if args.config:
        argv.extend(["--config", str(args.config)])
    return argv


def main(argv: list[str] | None = None) -> int:
    _force_utf8_console()
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    from core.crash import install_crash_handlers, show_startup_error

    log_dir = _safe_log_dir()
    with contextlib.suppress(Exception):
        install_crash_handlers(log_dir, version=C.VERSION)
    try:
        if args.debug_raw:
            _set_dpi_awareness()
            return run_raw_debug()
        if args.headless:
            return _run(args)
        # 默认：Tauri 外壳的业务服务（stdin/stdout NDJSON）。
        from service.server import main as service_main

        return service_main(_service_argv(args))
    except PadLendError as exc:
        LOG.error("%s", exc.user_message)
        _safe_print(f"[error] {exc.user_message}")
        show_startup_error(exc.user_message)
        return 1
    except KeyboardInterrupt:
        return 0
    except BaseException as exc:  # noqa: BLE001 - 最外层兜底，保证有日志
        LOG.exception("未捕获异常，程序即将退出")
        show_startup_error(f"程序遇到未处理的错误：{exc!r}")
        return 1


def _run(args: argparse.Namespace) -> int:
    """无头引擎模式：DPI、日志、配置、单实例锁、残留清理。"""
    _set_dpi_awareness()
    log_dir = _safe_log_dir()
    setup_logging(
        level=getattr(logging, args.log_level, logging.INFO),
        log_file=(log_dir / "padlend.log") if log_dir is not None else None,
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

    # 先收孤儿遮罩，再抢单实例锁：旧 sidecar 若挂起并持有互斥体，先抢锁只会
    # 得到“已有实例在运行”，没人能清掉屏幕上那块 topmost 窗口。
    _cleanup_orphan_mask()

    acquired = True
    release_fn = None
    if not args.no_single_instance:
        from win.single_instance import acquire, release

        release_fn = release
        acquired = acquire()
        if not acquired:
            LOG.info("已有实例在运行，本次退出")
            return 0
    try:
        _stale_cleanup(cfg, first_run=first_run)
        _mark_running()
        return run_headless(cfg, args)
    except PadLendError as exc:
        LOG.error("%s", exc.user_message)
        _safe_print(f"[error] {exc.user_message}")
        return 1
    except KeyboardInterrupt:
        return 0
    finally:
        if release_fn is not None:
            with contextlib.suppress(Exception):
                release_fn()


if __name__ == "__main__":
    raise SystemExit(main())
