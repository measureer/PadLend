"""日志框架（设计文档 7.4）。

日志行格式固定，便于 grep 与测试断言::

    2026-11-02 01:12:03.114 INFO  [router] cap.start edge=RIGHT seg=0 entry=(0,1068)

发版硬化（release-audit P1）：
* 文件日志改为 RotatingFileHandler（2 MB x 3，追加写），不再无限增长；
* 每次 setup 写一行会话头，方便把日志按运行切段；
* 打包后 console=False 时 sys.stderr 可能是 None，此时不装 StreamHandler
  （旧代码会创建一个写 None 的 handler，异常日志反而报错）。
"""

from __future__ import annotations

import logging
import sys
from datetime import datetime
from logging.handlers import RotatingFileHandler
from pathlib import Path

LOG_FORMAT = "%(asctime)s.%(msecs)03d %(levelname)-5s [%(name)s] %(message)s"
DATE_FORMAT = "%Y-%m-%d %H:%M:%S"
_FILE_FORMAT = LOG_FORMAT

# 单文件 2 MB，保留 3 个历史文件（最多约 8 MB）。
LOG_MAX_BYTES = 2 * 1024 * 1024
LOG_BACKUP_COUNT = 3

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

    # console=False（PyInstaller windowed）下 stderr 为 None：装了也只会在
    # emit 时抛 AttributeError，所以直接跳过。
    if sys.stderr is not None:
        stderr = logging.StreamHandler(stream=sys.stderr)
        stderr.setFormatter(formatter)
        root.addHandler(stderr)

    if log_file is not None:
        log_file.parent.mkdir(parents=True, exist_ok=True)
        file_handler = RotatingFileHandler(
            log_file,
            mode="a",
            maxBytes=LOG_MAX_BYTES,
            backupCount=LOG_BACKUP_COUNT,
            encoding="utf-8",
            delay=False,
        )
        file_handler.setFormatter(logging.Formatter(_FILE_FORMAT, DATE_FORMAT))
        root.addHandler(file_handler)

    _CONFIGURED = True

    if log_file is not None:
        logging.getLogger("padlend.logsetup").info(
            "==== 会话开始 %s · pid=%d · level=%s · log=%s ====",
            datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
            __import__("os").getpid(),
            logging.getLevelName(level),
            log_file,
        )


def logger_for(name: str) -> logging.Logger:
    return logging.getLogger(name)


def log_event(logger: logging.Logger, event: str, **fields: object) -> None:
    """按 7.4 的 "事件名 key=value" 格式输出一行 INFO 日志。"""
    parts = [event]
    for key, value in fields.items():
        parts.append(f"{key}={value}")
    logger.info(" ".join(parts))
