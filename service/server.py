"""Tauri ⇄ Python 的 NDJSON 传输层（stdin/stdout）。

协议（每行一个 JSON 对象，UTF-8，``\\n`` 结尾）：

* 请求 ``{"id": 1, "method": "devices.scan", "params": {...}}``
* 响应 ``{"id": 1, "ok": true, "result": {...}}`` / ``{"id": 1, "ok": false, "error": "..."}``
* 事件 ``{"event": "state", "data": {...}}``

stdout **只**承载协议数据；所有人类可读日志走 stderr / 日志文件，
否则一条 print 就会破坏 Rust 侧的解析。慢方法交给线程池，
读循环和界面都不会被 adb / 诊断 / mDNS 阻塞。
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import json
import logging
import os
import re
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from typing import Any

import constants as C
from core.logsetup import setup_logging
from service.api import PadLendService

LOG = logging.getLogger("service.server")
_EVENT_RE = re.compile(r"^[A-Za-z][\w.]*$")


class EventBus:
    """线程安全地往 stdout 写事件/响应。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()

    def write(self, payload: dict[str, Any]) -> None:
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":"), default=str)
        with self._lock:
            try:
                sys.stdout.write(line + "\n")
                sys.stdout.flush()
            except (OSError, ValueError):
                pass  # 外壳已关闭，退出路径不抛异常

    def emit(self, event: str, data: Any) -> None:
        self.write({"event": event, "data": data})

    def respond(self, request_id: Any, *, result: Any = None, error: str = "") -> None:
        if error:
            self.write({"id": request_id, "ok": False, "error": error})
        else:
            self.write({"id": request_id, "ok": True, "result": result})


class UiLogHandler(logging.Handler):
    """把引擎日志批量转发给前端日志页（150ms 合批，最多 40 条/批）。

    前端的日志虚拟列表只渲染可视区，因此这里不需要逐条推送；旧版每敲一个
    过滤字符就重建 2000 行富文本的做法不再存在。
    """

    def __init__(self, bus: EventBus, *, interval: float = 0.15, batch: int = 40) -> None:
        super().__init__(level=logging.INFO)
        self._bus = bus
        self._interval = interval
        self._batch = batch
        self._buffer: list[dict[str, str]] = []
        self._lock = threading.Lock()
        self._stop = threading.Event()
        threading.Thread(target=self._flush_loop, name="service-log-flush", daemon=True).start()

    def emit(self, record: logging.LogRecord) -> None:
        if getattr(record, "ui_logged", False):
            return
        text = record.getMessage().strip()
        if not text:
            return
        parts = text.split(" ", 1)
        if parts and _EVENT_RE.match(parts[0]) and ("." in parts[0] or parts[0].isupper()):
            event, message = parts[0], (parts[1] if len(parts) > 1 else "")
        else:
            event, message = "APP", f"[{record.name}] {text}"
        now = datetime.now()
        entry = {
            "timestamp": now.strftime("%H:%M:%S.") + f"{now.microsecond // 1000:03d}",
            "event": event,
            "message": message[:600],
        }
        with self._lock:
            if len(self._buffer) < 500:
                self._buffer.append(entry)

    def _flush_loop(self) -> None:
        while not self._stop.wait(self._interval):
            with self._lock:
                if not self._buffer:
                    continue
                entries, self._buffer = self._buffer[: self._batch], self._buffer[self._batch :]
            self._bus.emit("log.batch", {"entries": entries})

    def close(self) -> None:
        self._stop.set()
        super().close()


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="padlend-service", description="PadLend Tauri 业务服务")
    parser.add_argument("--simulate", action="store_true", help="用内存假平板，不访问真实 adb")
    parser.add_argument(
        "--no-hooks", action="store_true", help="不安装 Raw Input / 键盘钩子 / 全局热键（测试用）"
    )
    parser.add_argument("--config", default=None, help="指定 config.yaml 路径")
    parser.add_argument(
        "--log-level", default="INFO", choices=["DEBUG", "INFO", "WARNING", "ERROR"]
    )
    return parser.parse_args(argv)


def _prepare_stdio() -> None:
    """Windows 控制台默认 cp936；stdout 必须是 UTF-8 NDJSON。

    PyInstaller ``--noconsole`` 下 ``sys.stdout`` 可能是 None，但 fd 1 仍然
    是 Rust 侧创建的管道，因此先从 fd 复制一份文本流再谈编码。
    """
    import os

    if sys.stdout is None:
        try:
            sys.stdout = os.fdopen(os.dup(1), "w", encoding="utf-8", errors="replace", newline="\n")
        except OSError:
            return
    if sys.stderr is None:
        with contextlib.suppress(OSError):
            sys.stderr = os.fdopen(os.dup(2), "w", encoding="utf-8", errors="replace")
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            with contextlib.suppress(TypeError, ValueError, OSError):
                reconfigure(encoding="utf-8", errors="replace", newline="\n")


def _parent_alive(pid: int) -> bool:
    """父进程是否仍存活（Windows 用 OpenProcess/GetExitCodeProcess）。"""
    if pid <= 0:
        return False
    if sys.platform != "win32":
        try:
            os.kill(pid, 0)
        except OSError:
            return False
        return True
    kernel32 = ctypes.windll.kernel32
    kernel32.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    handle = kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        return False
    try:
        code = ctypes.c_uint32()
        if not kernel32.GetExitCodeProcess(handle, ctypes.byref(code)):
            return False
        return code.value == 259  # STILL_ACTIVE
    finally:
        kernel32.CloseHandle(handle)


def _start_parent_watchdog(
    service: PadLendService, parent_pid: int, *, interval: float = 1.0
) -> threading.Thread:
    """外壳消失时立刻同步 teardown 并退出，避免遮罩/ClipCursor 留给用户。

    Tauri 主进程崩溃后，stdin 不一定马上关闭（管道仍开着）；只靠读循环 EOF
    可能永远等不到。这个看门狗是缺陷 A 的失效安全：父子进程绑定，父死子退。
    """

    def _watch() -> None:
        while True:
            time.sleep(interval)
            if not _parent_alive(parent_pid):
                LOG.warning("父进程 %s 已退出，sidecar 执行 teardown 后退出", parent_pid)
                service.shutdown()
                os._exit(0)
                return  # 仅测试替身会走到；真实 os._exit 不返回

    thread = threading.Thread(target=_watch, name="service-parent-watch", daemon=True)
    thread.start()
    return thread


def main(argv: list[str] | None = None) -> int:
    _prepare_stdio()
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    try:
        log_dir = C.log_dir()
    except OSError:
        log_dir = None
    setup_logging(
        level=getattr(logging, args.log_level, logging.INFO),
        log_file=(log_dir / "padlend.log") if log_dir is not None else None,
    )

    bus = EventBus()
    logging.getLogger().addHandler(UiLogHandler(bus))
    service = PadLendService(
        bus.emit,
        simulate=args.simulate,
        no_hooks=args.no_hooks,
        config_path=args.config,
    )
    parent_pid = os.environ.get("PADLEND_PARENT_PID", "").strip()
    if parent_pid.isdigit():
        _start_parent_watchdog(service, int(parent_pid))

    def _boot() -> None:
        service.start()
        bus.emit("ready", service.snapshot())

    threading.Thread(target=_boot, name="service-boot", daemon=False).start()

    stopping = threading.Event()
    pool = ThreadPoolExecutor(max_workers=8, thread_name_prefix="service-rpc")

    def _handle(message: dict[str, Any]) -> None:
        request_id = message.get("id")
        method = str(message.get("method") or "")
        params = message.get("params") or {}
        try:
            service.wait_ready()
            result = service.call(method, params)
            bus.respond(request_id, result=result)
        except Exception as exc:  # noqa: BLE001 - 单个请求失败不能拖垮服务
            LOG.warning("rpc failed method=%s err=%r", method, exc)
            bus.respond(request_id, error=str(exc))
        if method == "app.shutdown":
            # 先把响应冲出去，再退出读循环。
            time.sleep(0.2)
            stopping.set()

    futures: list[Any] = []
    with contextlib.suppress(KeyboardInterrupt, OSError):
        for line in sys.stdin:
            if stopping.is_set():
                break
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError as exc:
                LOG.warning("bad request: %r", exc)
                continue
            if not isinstance(message, dict):
                continue
            futures.append(pool.submit(_handle, message))
    # stdin 关闭（外壳退出）时也要把已在排队的请求答完，避免响应乱序丢失。
    for future in futures:
        with contextlib.suppress(Exception):
            future.result(timeout=10)
    pool.shutdown(wait=True, cancel_futures=False)
    service.shutdown()
    bus.emit("service.stopped", {})
    time.sleep(0.2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
