"""日志框架（设计文档 7.4）。

日志行格式固定，便于 grep 与测试断言::

    2026-11-02 01:12:03.114 INFO  [router] cap.start edge=RIGHT seg=0 entry=(0,1068)
"""

from __future__ import annotations

import logging
import sys
from pathlib import Path

LOG_FORMAT = "%(asctime)s.%(msecs)03d %(levelname)-5s [%(name)s] %(message)s"
DATE_FORMAT = "%Y-%m-%d %H:%M:%S"
_FILE_FORMAT = LOG_FORMAT

_CONFIGURED = False


def setup_logging(level: int = logging.INFO, log_file: Path | None = None) -> None:
    """配置根 logger。重复调用是幂等的（先清空旧 handler）。"""
    global _CONFIGURED
    root = logging.getLogger()
    root.setLevel(level)

    for handler in list(root.handlers):
        root.removeHandler(handler)
        handler.close()

    formatter = logging.Formatter(LOG_FORMAT, DATE_FORMAT)

    stderr = logging.StreamHandler(stream=sys.stderr)
    stderr.setFormatter(formatter)
    root.addHandler(stderr)

    if log_file is not None:
        log_file.parent.mkdir(parents=True, exist_ok=True)
        file_handler = logging.FileHandler(log_file, encoding="utf-8")
        file_handler.setFormatter(logging.Formatter(_FILE_FORMAT, DATE_FORMAT))
        root.addHandler(file_handler)

    # Qt / PySide 的杂音降级
    logging.getLogger("PySide6").setLevel(logging.WARNING)
    _CONFIGURED = True


def logger_for(name: str) -> logging.Logger:
    return logging.getLogger(name)


def log_event(logger: logging.Logger, event: str, **fields: object) -> None:
    """按 7.4 的 ``事件名 key=value`` 格式输出一行 INFO 日志。"""
    parts = [event]
    for key, value in fields.items():
        parts.append(f"{key}={value}")
    logger.info(" ".join(parts))
