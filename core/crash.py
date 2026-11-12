"""崩溃可观测性（发版审计 P0-1）。

打包后 console=False：未捕获异常时 stderr 是 None，用户只看到
"双击没反应"。这里做三件事：

1. sys.excepthook / threading.excepthook 把完整 traceback 落盘到
   <log_dir>/crash-YYYYmmdd-HHMMSS-<pid>.log（追加写，含版本/argv/
   Python/平台/线程名）；
2. faulthandler.enable(all_threads=True) 单独写 faulthandler-*.log，
   原生崩溃（ctypes/Qt）也能留下线程栈；
3. 崩溃/启动失败时把位置写进日志与 stderr，由 Tauri 前端通过事件提示用户。

1.1.0 起界面层不再使用 Qt，本模块也完全不依赖任何 GUI 框架；sidecar
进程可以独立安装崩溃钩子而不拉起窗口。
"""

from __future__ import annotations

import contextlib
import faulthandler
import logging
import os
import platform
import sys
import tempfile
import threading
import traceback
from datetime import datetime
from pathlib import Path
from typing import Any

LOG = logging.getLogger("padlend.crash")

_INSTALLED = False
_VERSION = ""
_LOG_DIR: Path | None = None
_FAULT_FILE: Any = None
_FAULT_PATH: Path | None = None


def _fallback_dir() -> Path:
    return Path(tempfile.gettempdir()) / "PadLend" / "logs"


def _safe_dir(log_dir: Path | None) -> Path:
    """选出第一个可写目录：调用方给的 → constants.log_dir() → %TEMP%。"""
    candidates: list[Path] = []
    if log_dir is not None:
        candidates.append(Path(log_dir))
    try:
        import constants as C

        candidates.append(C.log_dir())
    except Exception:  # noqa: BLE001 - config_dir 全回退失败/环境异常
        pass
    candidates.append(_fallback_dir())
    for candidate in candidates:
        try:
            candidate.mkdir(parents=True, exist_ok=True)
            return candidate
        except OSError:
            continue
    return _fallback_dir()


def log_dir() -> Path:
    """当前崩溃日志目录（测试/诊断可用）。"""
    return _LOG_DIR if _LOG_DIR is not None else _safe_dir(None)


def install_crash_handlers(log_dir: Path | None = None, *, version: str = "") -> Path | None:
    """安装全部崩溃钩子；返回 faulthandler 文件路径（失败为 None）。

    幂等：重复调用只重启 faulthandler，不会叠加 except hook。
    """
    global _INSTALLED, _VERSION, _LOG_DIR, _FAULT_FILE, _FAULT_PATH

    _VERSION = str(version or "")
    directory = _safe_dir(log_dir)
    _LOG_DIR = directory

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    fault_path = directory / f"faulthandler-{stamp}-{os.getpid()}.log"
    try:
        # 行缓冲 + utf-8：faulthandler 直接写 fd，进程死掉也不丢。
        _FAULT_FILE = fault_path.open("a", encoding="utf-8", buffering=1)
        faulthandler.enable(file=_FAULT_FILE, all_threads=True)
        _FAULT_PATH = fault_path
    except (OSError, RuntimeError, ValueError) as exc:  # pragma: no cover - 受限文件系统
        _FAULT_FILE = None
        _FAULT_PATH = None
        LOG.debug("faulthandler 不可用: %r", exc)

    sys.excepthook = _excepthook
    threading.excepthook = _thread_excepthook
    _INSTALLED = True
    return _FAULT_PATH


def _format_crash(
    kind: str,
    exc_type: type[BaseException],
    exc: BaseException,
    tb: Any,
    thread_name: str,
) -> str:
    argv = " ".join(sys.argv)
    lines = [
        "==== PadLend crash report ====",
        f"time     : {datetime.now().isoformat(timespec='seconds')}",
        f"kind     : {kind}",
        f"version  : {_VERSION or 'unknown'}",
        f"pid      : {os.getpid()}",
        f"argv     : {argv}",
        f"python   : {sys.version.split()[0]}",
        f"platform : {platform.platform()}",
        f"executable: {sys.executable}",
        f"thread   : {thread_name}",
        "",
        f"exception: {exc_type.__name__}: {exc}",
        "",
        "traceback:",
        "".join(traceback.format_exception(exc_type, exc, tb)).rstrip(),
        "",
    ]
    return "\n".join(lines)


def write_crash(
    kind: str,
    exc_type: type[BaseException],
    exc: BaseException,
    tb: Any,
    *,
    thread_name: str = "MainThread",
) -> Path | None:
    """把一次异常写成独立 crash 文件；返回路径（写失败返回 None）。"""
    directory = log_dir()
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    path = directory / f"crash-{stamp}-{os.getpid()}.log"
    text = _format_crash(kind, exc_type, exc, tb, thread_name)
    try:
        with path.open("a", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
    except OSError as write_exc:  # pragma: no cover - 磁盘满/只读
        LOG.error("崩溃日志写入失败 path=%s err=%r", path, write_exc)
        return None
    LOG.critical("未捕获异常已写入 %s（%s: %s）", path, exc_type.__name__, exc)
    return path


def report_crash(
    exc: BaseException, *, kind: str = "uncaught", notify: bool = True
) -> Path | None:
    """给最外层 except 用：落盘 + 尽力弹窗；绝不抛异常。

    ``notify=False`` 时只落盘，由调用方自己弹更合适的提示（启动失败）。
    """
    path = write_crash(
        kind,
        type(exc),
        exc,
        exc.__traceback__,
        thread_name=threading.current_thread().name,
    )
    if notify:
        notify_user(path, kind=kind)
    return path


def _safe_stderr(message: str) -> None:
    """console=False 的打包版 stderr 可能为 None，输出失败不能二次崩溃。"""
    with contextlib.suppress(OSError, ValueError, AttributeError):
        print(message, file=sys.stderr, flush=True)


def notify_user(path: Path | None, *, kind: str = "uncaught") -> None:
    """崩溃位置写入日志与 stderr；前端通过 `fatal` / toast 事件提示用户。"""
    if path is None:
        return
    LOG.error("崩溃日志：%s (%s)", path, kind)
    _safe_stderr(f"[PadLend] 已写入崩溃日志：{path}")


def show_startup_error(message: str, log_path: Path | None = None) -> None:
    """启动失败时留下可读线索（不弹窗、不依赖 GUI）。"""
    if log_path is None:
        with contextlib.suppress(Exception):
            log_path = log_dir() / "padlend.log"
    LOG.error("启动失败：%s（日志：%s）", message, log_path)
    _safe_stderr(f"[PadLend] 启动失败：{message}")
    if log_path is not None:
        _safe_stderr(f"[PadLend] 详细信息：{log_path.parent}")


def _excepthook(exc_type: type[BaseException], exc: BaseException, tb: Any) -> None:
    if issubclass(exc_type, KeyboardInterrupt):  # Ctrl+C 不写 crash
        sys.__excepthook__(exc_type, exc, tb)
        return
    path = write_crash(
        "sys.excepthook",
        exc_type,
        exc,
        tb,
        thread_name=threading.current_thread().name,
    )
    notify_user(path)


def _thread_excepthook(args: Any) -> None:
    exc_type = args.exc_type
    if issubclass(exc_type, SystemExit):
        return
    name = (
        getattr(args.thread, "name", "unknown-thread")
        if args.thread is not None
        else "unknown-thread"
    )
    write_crash(
        "threading.excepthook",
        exc_type,
        args.exc_value,
        args.exc_traceback,
        thread_name=name,
    )
