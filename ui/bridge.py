"""Router → Qt 信号的桥（设计文档 8.2 回调契约 / 6.10.1）。

Router 的回调 **必须** 在 GUI 线程外触发；``QObject.emit`` 本身是线程安全的，
当接收者住在 GUI 线程时 Qt 会自动排队。为了让意图显式可见，界面侧一律用
:meth:`RouterBridge.connect_gui` 以 ``QueuedConnection`` 绑定槽函数。

本模块只做转发，不改 Router 状态、不解析业务。
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any, Final

from PySide6.QtCore import QObject, Qt, Signal

log = logging.getLogger("ui.bridge")

MetricSlot = Callable[[dict[str, Any]], None]
StateSlot = Callable[[str, str, str], None]
LogSlot = Callable[[str], None]
OrientationSlot = Callable[[object], None]


def state_value(value: object) -> str:
    """``State`` 枚举 / 字符串 / None → 稳定的字符串（容忍未装 State 的假 Router）。"""
    raw = getattr(value, "value", value)
    if raw is None:
        return ""
    return str(raw)


def _ignore_state(_old: object, _new: object, _reason: object) -> None:
    """``detach`` 后的空实现，避免 Router 回调打到已销毁的桥。"""


def _ignore_metrics(_payload: dict[str, Any]) -> None:
    """``detach`` 后的空实现。"""


def _ignore_facts(_facts: object) -> None:
    """``detach`` 后的空实现。"""


class RouterBridge(QObject):
    """把 Router 的线程回调转成 Qt 信号。

    信号签名与任务契约一致：
    - ``stateChanged(str old, str new, str reason)``
    - ``metricsChanged(dict)``
    - ``logLine(str)``
    - ``orientationChanged(object)``：设备横竖屏事实变化
    """

    stateChanged = Signal(str, str, str)
    metricsChanged = Signal(dict)
    logLine = Signal(str)
    orientationChanged = Signal(object)

    _STATE_CALLBACK: Final[str] = "on_state_changed"
    _METRICS_CALLBACK: Final[str] = "on_metrics"
    _ORIENTATION_CALLBACK: Final[str] = "on_orientation_changed"

    def __init__(self, router: object | None = None, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._router: object | None = None
        self._state: str = ""
        self._metrics: dict[str, Any] = {}
        self._previous: dict[str, object] = {}
        if router is not None:
            self.attach(router)

    # -- 只读状态 ----------------------------------------------------------
    @property
    def state(self) -> str:
        """最近一次 ``stateChanged`` 的新状态值（初始为空）。"""
        return self._state

    @property
    def metrics(self) -> dict[str, Any]:
        """最近一次指标快照（副本，调用方可随意修改）。"""
        return dict(self._metrics)

    @property
    def attached(self) -> bool:
        return self._router is not None

    # -- 生命周期 ----------------------------------------------------------
    def attach(self, router: object) -> None:
        """把本桥挂到 ``router.on_state_changed`` / ``router.on_metrics``。"""
        if self._router is router:
            return
        if self._router is not None:
            self.detach()
        self._router = router
        self._install(self._STATE_CALLBACK, self._on_state_changed)
        self._install(self._METRICS_CALLBACK, self._on_metrics)
        self._install(self._ORIENTATION_CALLBACK, self._on_orientation)

    def detach(self) -> None:
        """摘掉回调并恢复成空实现（Router 不会被裸 None 调用）。"""
        router, self._router = self._router, None
        if router is None:
            return
        self._restore(router, self._STATE_CALLBACK, _ignore_state)
        self._restore(router, self._METRICS_CALLBACK, _ignore_metrics)
        self._restore(router, self._ORIENTATION_CALLBACK, _ignore_facts)

    def _install(self, name: str, callback: Callable[..., None]) -> None:
        router = self._router
        if router is None:
            return
        if not hasattr(router, name):
            log.warning("router has no callback slot %s", name)
            return
        try:
            self._previous[name] = getattr(router, name)
            setattr(router, name, callback)
        except (AttributeError, TypeError) as exc:  # 只读属性/自定义 __slots__
            log.warning("bridge attach failed slot=%s err=%r", name, exc)

    def _restore(self, router: object, name: str, fallback: Callable[..., None]) -> None:
        original = self._previous.pop(name, None)
        replacement = original if callable(original) else fallback
        try:
            setattr(router, name, replacement)
        except (AttributeError, TypeError) as exc:
            log.debug("bridge detach skipped slot=%s err=%r", name, exc)

    # -- Router 回调（GUI 线程外） ----------------------------------------
    def _on_state_changed(self, old: object, new: object, reason: object) -> None:
        old_text = state_value(old)
        new_text = state_value(new)
        self._state = new_text
        self._emit(self.stateChanged, old_text, new_text, str(reason or ""))

    def _on_metrics(self, payload: dict[str, Any]) -> None:
        if not isinstance(payload, dict):
            log.debug("metrics payload not a dict: %r", type(payload).__name__)
            return
        self._metrics = dict(payload)
        self._emit(self.metricsChanged, dict(payload))

    def _on_orientation(self, facts: object) -> None:
        if facts is None:
            return
        self._emit(self.orientationChanged, facts)

    # -- 其它生产者的日志 ------------------------------------------------
    def log_line(self, text: str) -> None:
        """供 UI/后台线程追加日志行（同样走信号，保证落到 GUI 线程）。"""
        self._emit(self.logLine, str(text))

    def _emit(self, signal: Signal, *args: object) -> None:
        """信号发射兜底：窗口销毁后 Router 仍可能在回调里打一枪。"""
        try:
            signal.emit(*args)
        except RuntimeError as exc:  # pragma: no cover - 依赖对象销毁时序
            log.debug("signal emit after teardown: %r", exc)

    # -- 绑定 --------------------------------------------------------------
    def connect_gui(
        self,
        on_state: StateSlot | None = None,
        on_metrics: MetricSlot | None = None,
        on_log: LogSlot | None = None,
        on_orientation: OrientationSlot | None = None,
    ) -> None:
        """以 ``QueuedConnection`` 把信号接到 GUI 线程的槽上（8.2 要求）。"""
        connection = Qt.ConnectionType.QueuedConnection
        if on_state is not None:
            self.stateChanged.connect(on_state, connection)
        if on_metrics is not None:
            self.metricsChanged.connect(on_metrics, connection)
        if on_log is not None:
            self.logLine.connect(on_log, connection)
        if on_orientation is not None:
            self.orientationChanged.connect(on_orientation, connection)
