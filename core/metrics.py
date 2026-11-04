"""实时指标滑窗（设计文档 7.4 / 10.3 / 11）。

GUI 每 2s 取一次 :meth:`Metrics.snapshot`，得到 7.4 约定的字典::

    {"raw_hz": 998.0, "send_hz": 998.0, "backlog": 0,
     "rtt_ms": 12, "p50_ms": 10, "p95_ms": 21,
     "drop_pct": 0.0, "cpu_pct": 2.1, "mem_mb": 84}

设计要点：

- 所有计数器有锁；``record_*`` 由 T4/T5 线程调用，``snapshot`` 由 GUI 线程调用。
- 速率是**最近 1 秒窗口**内的计数（窗口内事件数 / ``window_s``），而不是
  累计平均；因此启动/空闲时会自然回落到 0。
- ``psutil`` 是可选依赖：导入失败或调用失败时 CPU/内存返回 0，绝不让 GUI
  因为指标采集而崩溃。
"""

from __future__ import annotations

import logging
import threading
import time
from collections import deque
from collections.abc import Callable
from typing import Any

log = logging.getLogger("metrics")

_LATENCY_WINDOW = 256
_BYTES_PER_MB = 1024.0 * 1024.0


def _load_psutil() -> Any | None:
    """尽力导入 psutil；缺失时返回 ``None``（不写日志，避免导入期噪音）。"""
    try:
        import psutil
    except ImportError:  # pragma: no cover - 依赖通常存在
        return None
    return psutil


_PSUTIL: Any | None = _load_psutil()


def _percentile(sorted_values: list[float], fraction: float) -> float:
    """线性插值分位数；空列表返回 0。"""
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    pos = (len(sorted_values) - 1) * fraction
    lower = int(pos)
    upper = min(lower + 1, len(sorted_values) - 1)
    weight = pos - lower
    return sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight


class Metrics:
    """线程安全的滑窗指标聚合器。"""

    def __init__(
        self,
        *,
        window_s: float = 1.0,
        clock: Callable[[], float] | None = None,
    ) -> None:
        if window_s <= 0:
            raise ValueError(f"window_s 必须为正: {window_s!r}")
        self._window_s = float(window_s)
        self._clock: Callable[[], float] = clock or time.perf_counter
        self._lock = threading.Lock()
        self._raw: deque[float] = deque()
        self._sent: deque[float] = deque()
        self._latencies: deque[float] = deque(maxlen=_LATENCY_WINDOW)
        self._backlog = 0
        self._raw_total = 0
        self._sent_total = 0
        self._dropped = 0
        self._process: Any | None = None
        if _PSUTIL is not None:
            try:
                self._process = _PSUTIL.Process()
            except Exception:  # pragma: no cover - 平台/权限异常
                log.debug("psutil.Process 初始化失败", exc_info=True)
                self._process = None

    # -- 记录（T4/T5 线程） -------------------------------------------------
    def record_raw(self, count: int = 1) -> None:
        """记录 ``count`` 个 RawInput 事件（默认 1）。"""
        if count <= 0:
            return
        now = self._clock()
        with self._lock:
            for _ in range(count):
                self._raw.append(now)
            self._raw_total += count
            self._prune_locked(now)

    def record_send(self, count: int = 1) -> None:
        """记录成功写出的 HID 报告数（含心跳）。"""
        if count <= 0:
            return
        now = self._clock()
        with self._lock:
            for _ in range(count):
                self._sent.append(now)
            self._sent_total += count
            self._prune_locked(now)

    def record_latency(self, ms: float) -> None:
        """记录一次输入→设备写入的往返延迟（毫秒）。"""
        value = float(ms)
        if value < 0:
            return
        with self._lock:
            self._latencies.append(value)

    #: ``record_latency`` 的别名（7.4 的 ``rtt_ms``）。
    record_rtt = record_latency

    def record_drop(self, count: int = 1) -> None:
        """记录因发送队列满而丢弃的报告数。"""
        if count <= 0:
            return
        with self._lock:
            self._dropped += count

    def set_backlog(self, depth: int) -> None:
        """更新发送队列当前深度（0..``SEND_QUEUE_MAXLEN``）。"""
        with self._lock:
            self._backlog = max(0, int(depth))

    # -- 快照（GUI 线程） ---------------------------------------------------
    def snapshot(self) -> dict[str, float | int]:
        """返回 7.4 约定的指标字典（新 dict，调用方可安全持有）。"""
        cpu_pct, mem_mb = self._process_usage()
        now = self._clock()
        with self._lock:
            self._prune_locked(now)
            raw_hz = len(self._raw) / self._window_s
            send_hz = len(self._sent) / self._window_s
            latencies = sorted(self._latencies)
            dropped = self._dropped
            received = self._raw_total
            backlog = self._backlog
        rtt_ms = latencies[-1] if latencies else None
        p50_ms = _percentile(latencies, 0.50) if latencies else None
        p95_ms = _percentile(latencies, 0.95) if latencies else None
        attempted = dropped + received
        drop_pct = (dropped / attempted * 100.0) if attempted else 0.0
        snapshot = {
            "raw_hz": round(raw_hz, 1),
            "send_hz": round(send_hz, 1),
            "backlog": int(backlog),
            "drop_pct": round(drop_pct, 2),
            "cpu_pct": round(cpu_pct, 1),
            "mem_mb": round(mem_mb, 1),
        }
        # 没有采样就**不报** rtt（GUI 会显示「RTT —」）。报 0 会被当成真实值，
        # 用户看到的就是"右上角 RTT 一直是 0ms"。
        if rtt_ms is not None:
            snapshot["rtt_ms"] = round(rtt_ms, 1)
        if p50_ms is not None:
            snapshot["p50_ms"] = round(p50_ms, 1)
        if p95_ms is not None:
            snapshot["p95_ms"] = round(p95_ms, 1)
        return snapshot

    def reset(self) -> None:
        """清空所有滑窗与累计计数（配置重载 / 重连后调用）。"""
        with self._lock:
            self._raw.clear()
            self._sent.clear()
            self._latencies.clear()
            self._backlog = 0
            self._raw_total = 0
            self._sent_total = 0
            self._dropped = 0

    # -- 内部 ---------------------------------------------------------------
    def _prune_locked(self, now: float) -> None:
        cutoff = now - self._window_s
        while self._raw and self._raw[0] < cutoff:
            self._raw.popleft()
        while self._sent and self._sent[0] < cutoff:
            self._sent.popleft()

    def _process_usage(self) -> tuple[float, float]:
        """进程 CPU/内存；任何失败都退化为 ``(0.0, 0.0)``。"""
        process = self._process
        if process is None:
            return 0.0, 0.0
        cpu = 0.0
        mem = 0.0
        try:
            cpu = float(process.cpu_percent(None))
        except Exception:  # pragma: no cover - psutil 平台差异
            log.debug("psutil.cpu_percent 失败", exc_info=True)
        try:
            mem = float(process.memory_info().rss) / _BYTES_PER_MB
        except Exception:  # pragma: no cover - psutil 平台差异
            log.debug("psutil.memory_info 失败", exc_info=True)
        return cpu, mem
