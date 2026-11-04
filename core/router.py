"""Router：状态机 + 发送线程 + supervisor（设计文档 6.5 / 6.7 / 10 / 12 / 13.4）。

职责
----
- **唯一状态写者**：IDLE / ARMING / CAPTURED / RETURNING / FAULT 的迁移只发生
  在本类内部，并且每次迁移都经 :func:`core.state.can_transition` 校验（I4）。
- **输入路由**：``on_raw`` 由 RawInput 线程调用；只有 CAPTURED 才把位移转成
  HID 报告（I1），其余状态只发零位移心跳。
- **发送线程**（T5）：1000Hz 批量 flush，队列满时优先合并零位移心跳，
  warp 报告可一次性突破 8 条/帧的上限。
- **supervisor 线程**（T6）：2s 健康检查、旋转归中、显示器热插拔、
  pointer_speed 轮询、断线指数退避重连（1/2/4/8/16s×5）。
- **键盘**：``on_key`` 是钩子转发线程的唯一入口；CAPTURED 且 forward 时经
  ``tablet.keymap`` 转 HID，``suppress_pc`` 决定是否吞掉本机按键；白名单键
  （默认 Win 键）与保命热键永远不会被吞。

依赖边界
--------
只允许 import 标准库、``constants``、``core.*``、``tablet.cursor``；``session`` /
``desktop`` / ``mask`` 全部按 8.2 的结构化契约鸭子类型调用，**不得 import win**
（``TYPE_CHECKING`` 中的类型引用除外）。``tablet.keymap`` 在首次需要时延迟
导入：keyboard 队友的模块缺失或初始化失败时，Router 退化为"不转发、不吞键"，
而不是崩溃。
"""

from __future__ import annotations

import contextlib
import ctypes
import logging
import threading
import time
from collections import deque
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import constants as C
from core.errors import ErrorCode, PadLendError, wrap_unexpected
from core.events import KeyEvent, RawDelta
from core.geometry import Align, Edge, Placement, Rect
from core.logsetup import log_event
from core.metrics import Metrics
from core.state import State, can_transition
from tablet.cursor import CursorEstimator

if TYPE_CHECKING:
    from config import Config
    from tablet.session import DeviceFacts, TabletSession
    from win.desktop import DesktopMap
    from win.mask import CursorMask

log = logging.getLogger("router")

#: 构造参数 ``session`` / ``desktop`` / ``mask`` 的类型只用于文档；运行时鸭子类型。
#: （``from __future__ import annotations`` 让这些注解不产生运行时 import。）


# ---------------------------------------------------------------------------
# 小工具
# ---------------------------------------------------------------------------
def _noop_state(_old: State, _new: State, _reason: str) -> None:
    return None


def _noop_metrics(_snapshot: dict[str, float | int]) -> None:
    return None


def _fallback_vk_names() -> dict[str, int]:
    """``tablet.keymap`` 不可用时的最小 VK 名字表（只覆盖默认白名单）。"""
    return {
        "VK_LWIN": 0x5B,
        "VK_RWIN": 0x5C,
        "VK_APPS": 0x5D,
        "VK_LCONTROL": 0xA2,
        "VK_RCONTROL": 0xA3,
        "VK_LSHIFT": 0xA0,
        "VK_RSHIFT": 0xA1,
        "VK_LMENU": 0xA4,
        "VK_RMENU": 0xA5,
        "VK_CONTROL": 0x11,
        "VK_SHIFT": 0x10,
        "VK_MENU": 0x12,
    }


#: Control / Alt 的左右判定（通用 VK 用扩展位区分）。
_CTRL_VKS: frozenset[int] = frozenset({0x11, 0xA2, 0xA3})
_ALT_VKS: frozenset[int] = frozenset({0x12, 0xA4, 0xA5})
_SAFETY_LETTERS: frozenset[int] = frozenset({0x53, 0x51})  # S / Q


def _timer_begin(period_ms: int) -> bool:
    """尽力调用 ``timeBeginPeriod``；失败不影响正确性。"""
    try:
        winmm = getattr(ctypes, "windll", None)
        if winmm is None:
            return False
        winmm.winmm.timeBeginPeriod(period_ms)
        return True
    except Exception:  # pragma: no cover - 非 Windows / 权限异常
        log.debug("timeBeginPeriod 失败", exc_info=True)
        return False


def _timer_end(period_ms: int) -> None:
    try:
        winmm = getattr(ctypes, "windll", None)
        if winmm is not None:
            winmm.winmm.timeEndPeriod(period_ms)
    except Exception:  # pragma: no cover - 非 Windows / 权限异常
        log.debug("timeEndPeriod 失败", exc_info=True)


# ---------------------------------------------------------------------------
# 发送队列消息
# ---------------------------------------------------------------------------
@dataclass(frozen=True, slots=True)
class _SendMsg:
    """一条鼠标 HID 报告；全零的真实松键报告与空闲心跳必须区分。"""

    buttons: int
    dx: int
    dy: int
    wheel: int
    burst: bool = False
    heartbeat: bool = False
    #: 入队时刻（``clock()``）；写出成功后用它算 7.4 的 rtt_ms。
    stamp: float = 0.0

    def is_heartbeat(self) -> bool:
        return self.heartbeat


class Router:
    """PadLend 的核心状态机（8.2 冻结接口 + 测试注入点）。"""

    def __init__(
        self,
        cfg: Config,
        session: TabletSession,
        desktop: DesktopMap,
        mask: CursorMask,
        clock: Callable[[], float] | None = None,
        keymap: Any | None = None,
    ) -> None:
        self._cfg = cfg
        self._session = session
        self._desktop = desktop
        self._mask = mask
        self._clock: Callable[[], float] = clock or time.perf_counter

        self._lock = threading.RLock()
        self._send_lock = threading.Lock()
        # SEND_QUEUE_MAXLEN 是背压软阈值，不是硬上限：真实报告（尤其松键）不能
        # 因 deque(maxlen=...) 的隐式逐出而丢失。
        self._send_queue: deque[_SendMsg] = deque()
        self._stop_event = threading.Event()
        self._sender_thread: threading.Thread | None = None
        self._supervisor_thread: threading.Thread | None = None
        self._started = False
        self._stopped = False

        self._state = State.IDLE
        self._placement: Placement | None = None
        self._estimator: CursorEstimator | None = None
        self._facts: DeviceFacts | None = None
        self._manual = False
        self._pc_cursor: tuple[int, int] | None = None
        self._buttons = 0
        # Windows 滚轮是 120 单位/格，HID 是 1 detent/格；不足一格留到下次。
        self._wheel_accum = 0

        # ARMING
        self._arm_deadline = 0.0
        self._arm_accum = 0.0

        # CAPTURED / RETURNING
        self._entry_edge: Edge | None = None
        self._overflow = 0.0
        # 穿屏前已经按住的键不在平板上重新开始一次按下；等物理松键后再恢复。
        self._neutral_until_release = False
        self._hit_logged = False
        self._return_deadline = 0.0

        # 故障 / 重连 / 看护
        self._health_failures = 0
        self._write_failures = 0
        self._reconnect_attempt = 0
        self._next_reconnect_at = 0.0
        self._connect_in_progress = False
        self._connect_lock = threading.Lock()
        # 设备管理（6.10 设备页）：用户主动断开后禁止 supervisor 自动重连。
        self._device_suspended = False
        self._last_pointer_poll: float | None = None
        self._desktop_sig: tuple[tuple[int, tuple[int, int, int, int]], ...] | None = None

        # 键盘
        self._keymap = keymap
        self._keymap_loaded = keymap is not None
        self._passthrough_vks = self._resolve_passthrough(cfg)
        self._ctrl_sides: set[int] = set()
        self._alt_sides: set[int] = set()
        self._suppress_logged = False
        self._keymap_warned = False

        self._metrics = Metrics(clock=self._clock)

        # 公开回调：main/GUI 用属性赋值注入（不是构造参数，保持 8.2 四参数）。
        self.on_state_changed: Callable[[State, State, str], None] = _noop_state
        self.on_metrics: Callable[[dict[str, float | int]], None] = _noop_metrics

    # -- 生命周期 -----------------------------------------------------------
    @property
    def state(self) -> State:
        with self._lock:
            return self._state

    @property
    def placement(self) -> Placement | None:
        with self._lock:
            return self._placement

    @property
    def manual(self) -> bool:
        with self._lock:
            return self._manual

    @property
    def estimator(self) -> CursorEstimator | None:
        with self._lock:
            return self._estimator

    @property
    def facts(self) -> DeviceFacts | None:
        """当前已连接设备的真实事实；未连接时为 ``None``。"""
        with self._lock:
            return self._facts

    @property
    def connected(self) -> bool:
        """是否已拿到设备事实（GUI 设备页/画布据此显示真实尺寸）。"""
        with self._lock:
            return self._facts is not None and self._state is not State.FAULT

    def start(self) -> None:
        """启动 sender / supervisor 线程；可重复调用（第二次起为空操作）。

        **非阻塞**：设备连接（可能耗时数十秒的 ``session.start()``）交给
        supervisor 线程执行。这里只读取已有的 ``DeviceFacts``；没有设备时立即
        IDLE→FAULT 并让 supervisor 马上发起首连，GUI 线程不会被 ADB 卡住。
        """
        with self._lock:
            if self._started:
                return
            self._started = True
            self._stopped = False
            self._stop_event.clear()
        with self._lock:
            self._facts = self._read_facts() or self._facts
            try:
                self._ensure_estimator_locked()
                self._desktop_sig = self._monitor_signature()
                if self._pc_cursor is None:
                    self._pc_cursor = self._query_cursor_pos() or self._default_cursor()
            except PadLendError as exc:
                log.warning("router.start_waiting code=%s detail=%s", exc.code.value, exc.detail)
                self._fault(exc.code, f"启动等待设备: {exc.detail}")
                self._next_reconnect_at = self._clock()
            except Exception as exc:  # pragma: no cover - 防御性兜底
                wrapped = wrap_unexpected(exc, detail="router.start")
                log.exception("router.start_unexpected")
                self._fault(wrapped.code, f"启动失败: {wrapped.detail}")
                self._next_reconnect_at = self._clock()

        self._sender_thread = threading.Thread(
            target=self._sender_loop,
            name="padlend-sender",
            daemon=True,
        )
        self._supervisor_thread = threading.Thread(
            target=self._supervisor_loop,
            name="padlend-supervisor",
            daemon=True,
        )
        self._sender_thread.start()
        self._supervisor_thread.start()
        log.info("router.start state=%s", self._state.value)

    def connect_device(self, serial: str | None) -> None:
        """用户从设备页选择一台设备并连接（可重复调用）。

        会先停掉当前会话、清空旧 facts / 估计器，再把配置里的 serial 换成目标
        设备并立即触发一次首连。supervisor 随后按 1/2/4/8/16s 退避重试。
        调用方（GUI）应在后台线程执行，因为 ``session.stop()`` 可能做 adb I/O。
        """
        with self._lock:
            if self._stopped:
                return
            self._device_suspended = False
            current = str(getattr(self._cfg.device, "serial", "") or "")
            if current != (serial or ""):
                self._cfg.device.serial = serial or ""
            self._reconnect_attempt = 0
            self._next_reconnect_at = self._clock()
            try:
                self._abort_capture_locked("device.connect")
            except Exception:  # pragma: no cover - 清理不应阻断切换
                log.exception("router.connect_device_cleanup_failed")
            self._facts = None
            self._estimator = None
        self._stop_session_for_device()
        with self._lock:
            if self._stopped:
                return
            self._device_suspended = False
            self._reconnect_attempt = 0
            self._next_reconnect_at = self._clock()
            self._change_state(State.FAULT, "device.connect")
        log.info("router.device.connect serial=%s", serial or "")

    def disconnect_device(self) -> None:
        """用户主动断开设备；停止会话并禁止 supervisor 自动重连。"""
        with self._lock:
            if self._stopped:
                return
            self._device_suspended = True
            self._next_reconnect_at = float("inf")
            self._reconnect_attempt = 0
            try:
                self._abort_capture_locked("device.disconnect")
            except Exception:  # pragma: no cover
                log.exception("router.disconnect_device_cleanup_failed")
            self._facts = None
            self._estimator = None
        self._stop_session_for_device()
        with self._lock:
            if self._stopped:
                return
            self._facts = None
            self._estimator = None
            self._change_state(State.IDLE, "device.disconnect")
        log.info("router.device.disconnect")

    def _stop_session_for_device(self) -> None:
        """串行地停掉当前会话，避免与 supervisor 的连接尝试并发。"""
        self._cancel_connection_best_effort()
        stopper = getattr(self._session, "stop", None)
        if not callable(stopper):
            return
        with self._connect_lock:
            try:
                stopper()
            except Exception:
                log.warning("router.device.session_stop_failed", exc_info=True)

    def stop(self) -> None:
        """停止所有线程并**保证**释放遮罩；幂等，不调用 ``session.stop()``。

        若 supervisor 正卡在首次 ``session.start()``（ADB 等待最长 40s），
        ``join`` 会超时返回；晚到的结果由 ``_stopped`` 检查丢弃，不会再迁移
        状态、发送输入或创建线程。
        """
        with self._lock:
            if self._stopped:
                return
            self._stopped = True
        self._stop_event.set()
        self._cancel_connection_best_effort()
        try:
            with self._lock:
                self._abort_capture_locked("stop")
        except Exception:  # pragma: no cover - 清理绝不应阻断 stop
            log.exception("router.stop_cleanup_failed")
        finally:
            current = threading.current_thread()
            for thread in (self._sender_thread, self._supervisor_thread):
                if thread is not None and thread is not current:
                    thread.join(timeout=2.0)
            self._release_mask("stop")
        log.info("router.stop")

    def _cancel_connection_best_effort(self) -> None:
        """请求 session 取消正在进行的首连（可选扩展，不阻塞、绝不抛）。

        ``TabletSession`` 目前没有取消 API；若将来提供 ``cancel_start()`` /
        ``abort_start()``，Router 会自动使用。没有时只能等 ADB 自己超时。
        """
        with self._lock:
            active = self._connect_in_progress
        if not active:
            return
        for name in ("cancel_start", "abort_start"):
            cancel = getattr(self._session, name, None)
            if not callable(cancel):
                continue
            try:
                cancel()
                log.info("router.connect_cancel method=%s", name)
            except Exception:
                log.warning("router.connect_cancel_failed method=%s", name, exc_info=True)
            return

    def emergency_stop(self) -> None:
        """急停（Ctrl+Alt+Q）：立即交还输入，但保持 Router 线程可用。

        语义：CAPTURED 时先走一次交还；随后强制回 IDLE；无条件释放遮罩；
        停掉平板的键盘状态。不会停止 sender/supervisor 线程。
        """
        try:
            with self._lock:
                if self._stopped:
                    return
                if self._state is State.CAPTURED:
                    self._begin_return_locked("panic", debounce=False)
                if self._state is not State.IDLE:
                    self._change_state(State.IDLE, "panic")
                self._release_mask("panic")
                self._send_keyboard_zero()
                self._send_mouse_release()
                self._reset_keymap_locked()
                self._arm_accum = 0.0
                self._overflow = 0.0
        except Exception:  # pragma: no cover - 急停必须尽力而为
            log.exception("router.emergency_stop_failed")
            self._release_mask("panic")

    def set_placement(self, placement: Placement) -> None:
        """更新平板摆位；CAPTURED 期间调用只影响下一次接管。"""
        with self._lock:
            self._placement = placement
        log.info(
            "router.placement edge=%s span=%s..%s offset=%.3f",
            placement.segment.edge.value,
            placement.segment.span_start,
            placement.segment.span_end,
            placement.offset,
        )

    def toggle_manual(self) -> None:
        """手动接管/交还（设置页"立即连接"按钮与托盘菜单共用）。"""
        try:
            with self._lock:
                if self._stopped:
                    return
                state = self._state
                if state is State.RETURNING:
                    return  # 去抖期间忽略，避免抖动
                if state is State.CAPTURED:
                    self._manual = False
                    self._begin_return_locked("manual")
                    return
                if state is State.FAULT:
                    self._change_state(State.IDLE, "manual")
                if self._placement is None:
                    log.warning("router.manual.no_placement")
                    return
                self._manual = True
                self._enter_captured_locked("manual")
        except PadLendError as exc:
            log.warning("router.manual_failed code=%s detail=%s", exc.code.value, exc.detail)
            self._fault(exc.code, f"手动接管失败: {exc.detail}")
        except Exception as exc:  # pragma: no cover - 防御性兜底
            wrapped = wrap_unexpected(exc, detail="router.toggle_manual")
            log.exception("router.manual_unexpected")
            self._fault(wrapped.code, f"手动接管失败: {wrapped.detail}")

    def resync_pointer(self) -> None:
        """把平板指针重新归中（GUI 诊断按钮）；CAPTURED 时用 warp 连发。"""
        try:
            with self._lock:
                if self._stopped:
                    return
                estimator = self._ensure_estimator_locked()
                if self._state is State.CAPTURED:
                    target_x = (estimator.W - 1) / 2.0
                    target_y = (estimator.H - 1) / 2.0
                    reports = estimator.warp_to(target_x, target_y)
                    # warp 只能发整数步，会留下 <1 的残差；内部坐标对齐精确中心。
                    estimator.reset()
                    self._enqueue_burst_locked(reports)
                    log.info("router.resync reports=%d", len(reports))
                else:
                    estimator.reset()
        except PadLendError as exc:
            log.warning("router.resync_failed code=%s detail=%s", exc.code.value, exc.detail)
            self._fault(exc.code, f"归中失败: {exc.detail}")
        except Exception as exc:  # pragma: no cover - 防御性兜底
            wrapped = wrap_unexpected(exc, detail="router.resync_pointer")
            log.exception("router.resync_unexpected")
            self._fault(wrapped.code, f"归中失败: {wrapped.detail}")

    def metrics_snapshot(self) -> dict[str, float | int]:
        """7.4 指标快照（GUI 也可等 supervisor 的 ``on_metrics`` 推送）。"""
        return self._metrics.snapshot()

    # -- RawInput -----------------------------------------------------------
    def on_raw(self, ev: RawDelta) -> None:
        """RawInput 线程入口；绝不向调用方抛异常。"""
        try:
            with self._lock:
                if self._stopped:
                    return
                self._metrics.record_raw()
                self._on_raw_locked(ev)
        except PadLendError as exc:
            log.warning("router.raw_failed code=%s detail=%s", exc.code.value, exc.detail)
            self._fault(exc.code, f"处理鼠标输入失败: {exc.detail}")
        except Exception as exc:  # pragma: no cover - 防御性兜底
            wrapped = wrap_unexpected(exc, detail="router.on_raw")
            log.exception("router.raw_unexpected")
            self._fault(wrapped.code, f"处理鼠标输入失败: {wrapped.detail}")

    def _on_raw_locked(self, ev: RawDelta) -> None:
        previous_buttons = self._buttons
        self._buttons = int(ev.buttons)
        if self._buttons != previous_buttons:
            log_event(log, "cap.btn", buttons=f"0x{self._buttons:02x}")
            # 按下/抬起是"电脑上冒出鼠标"的现场：把遮罩、锁、光标可见性一起
            # 写进日志，事后能区分是遮罩被拆、锁被系统收回，还是外部程序把
            # 光标显示回来了（用户报的问题只在真实点击时出现）。
            if self._state is State.CAPTURED:
                try:
                    log.info("cap.cursor %s", self._mask.diagnose())
                except Exception:  # pragma: no cover - 诊断不应影响输入
                    log.debug("cap.cursor diagnose failed", exc_info=True)
        self._track_pc_cursor(ev)
        state = self._state
        if state is State.FAULT:
            return
        if state is State.RETURNING:
            if self._clock() >= self._return_deadline:
                self._change_state(State.IDLE, "debounce")
                # 交还后立刻对齐一次绝对光标，避免键盘/其它窗口操作让
                # ``_pc_cursor`` 与真实位置长期不一致而无法再次接管。
                position = self._query_cursor_pos()
                if position is not None:
                    self._pc_cursor = position
            return
        if state is State.IDLE:
            self._maybe_arm_locked(ev)
        elif state is State.ARMING:
            self._update_arm_locked(ev)
        elif state is State.CAPTURED:
            self._handle_captured_locked(ev)

    def _track_pc_cursor(self, ev: RawDelta) -> None:
        """维护 PC 光标位置：优先 ``desktop.cursor_pos()``，否则按增量累计。

        ``cursor_pos()`` 存在但本次返回 ``None``（例如 ``GetCursorPos`` 临时
        失败）时保持上一次已知位置，**不能**回退成原始计数累加：Raw Input 的
        ``dx/dy`` 是未经系统加速的鼠标计数，不是像素，累加会迅速漂移。
        """
        has_getter = callable(getattr(self._desktop, "cursor_pos", None))
        position = self._query_cursor_pos()
        if position is not None:
            if self._pc_cursor is None:
                log.debug("router.cursor_abs x=%d y=%d", position[0], position[1])
            self._pc_cursor = position
            return
        if has_getter:
            if self._pc_cursor is None:
                self._pc_cursor = self._default_cursor()
            return
        if self._pc_cursor is None:
            self._pc_cursor = self._default_cursor()
        x, y = self._pc_cursor
        self._pc_cursor = self._clamp_to_desktop(x + int(ev.dx), y + int(ev.dy))

    def _query_cursor_pos(self) -> tuple[int, int] | None:
        getter = getattr(self._desktop, "cursor_pos", None)
        if not callable(getter):
            return None
        try:
            value = getter()
        except Exception:
            log.debug("desktop.cursor_pos 失败", exc_info=True)
            return None
        if value is None:
            return None
        try:
            return int(value[0]), int(value[1])
        except (TypeError, ValueError, IndexError):
            log.debug("desktop.cursor_pos 返回值非法: %r", value)
            return None

    def _default_cursor(self) -> tuple[int, int]:
        box = self._bounding_box()
        try:
            cx, cy = box.center
            return int(cx), int(cy)
        except Exception:  # pragma: no cover - 空桌面
            return 0, 0

    def _clamp_to_desktop(self, x: int, y: int) -> tuple[int, int]:
        box = self._bounding_box()
        if box.empty:
            return x, y
        return (
            min(max(x, box.left), box.right - 1),
            min(max(y, box.top), box.bottom - 1),
        )

    def _bounding_box(self) -> Rect:
        value = getattr(self._desktop, "bounding_box", Rect(0, 0, 0, 0))
        if callable(value):
            try:
                value = value()
            except Exception:
                log.debug("desktop.bounding_box 失败", exc_info=True)
                return Rect(0, 0, 0, 0)
        if isinstance(value, Rect):
            return value
        return Rect(0, 0, 0, 0)

    # -- 边缘 / ARMING ------------------------------------------------------
    def _maybe_arm_locked(self, ev: RawDelta) -> None:
        placement = self._placement
        if placement is None:
            log.debug("router.arm_skip placement=none")
            return
        x, y = self._pc_cursor or self._default_cursor()
        segments = self._segments_at(x, y)
        if not segments:
            log.debug(
                "router.arm_skip reason=no_segments x=%d y=%d edge=%s",
                x,
                y,
                placement.segment.edge.value,
            )
            return
        chosen = self._resolve_corner_locked(segments, (int(ev.dx), int(ev.dy)))
        if chosen is None or chosen.edge is not placement.segment.edge:
            log.debug(
                "router.arm_skip reason=edge_mismatch x=%d y=%d want=%s got=%s",
                x,
                y,
                placement.segment.edge.value,
                chosen.edge.value if chosen is not None else "none",
            )
            return
        # 同一朝向的另一块显示器边缘（例如主屏底部而不是配置里的副屏底部）
        # 也当成同一条边：平板只有一块，用户不会记得自己上次摆在哪块屏。
        if self._segment_differs(chosen, placement.segment):
            self._adopt_segment_locked(chosen, placement)
            placement = self._placement
            assert placement is not None
        nx, ny = chosen.edge.outward_normal()
        if int(ev.dx) * nx + int(ev.dy) * ny <= 0:
            return
        along = y if chosen.edge.is_vertical else x
        if not placement.segment.contains_along(along):
            log.debug(
                "router.arm_skip reason=outside_span along=%d span=%d..%d",
                along,
                placement.segment.span_start,
                placement.segment.span_end,
            )
            return
        now = self._clock()
        self._arm_deadline = now + max(0, int(self._cfg.motion.arm_window_ms)) / 1000.0
        self._arm_accum = 0.0
        self._manual = False
        if self._change_state(State.ARMING, "arm"):
            log.debug(
                "router.arm edge=%s along=%d window_ms=%d",
                chosen.edge.value,
                along,
                self._cfg.motion.arm_window_ms,
            )

    @staticmethod
    def _segment_differs(left: Any, right: Any) -> bool:
        return (
            int(getattr(left, "monitor_handle", -1)) != int(getattr(right, "monitor_handle", -2))
            or int(getattr(left, "span_start", -1)) != int(getattr(right, "span_start", -2))
            or int(getattr(left, "span_end", -1)) != int(getattr(right, "span_end", -2))
        )

    def _adopt_segment_locked(self, segment: Any, old: Placement) -> None:
        self._placement = Placement(
            segment=segment,
            offset=old.offset,
            align=old.align,
            corner_priority=old.corner_priority,
        )
        log_event(
            log,
            "router.placement_adopt",
            edge=str(getattr(segment, "edge", "")),
            span_start=int(getattr(segment, "span_start", 0)),
            span_end=int(getattr(segment, "span_end", 0)),
        )

    def _update_arm_locked(self, ev: RawDelta) -> None:
        placement = self._placement
        if placement is None:
            self._arm_fail_locked("placement_lost")
            return
        now = self._clock()
        if now > self._arm_deadline:
            # 没赶上 80ms 的快速甩动，但如果已经明显向外推了一段，
            # 说明用户是慢慢把鼠标压到边上的，不应该判定失败。
            if self._arm_accum >= C.ARM_SLOW_PX:
                self._arm_accum = 0.0
                self._enter_captured_locked("arm_slow")
            else:
                self._arm_fail_locked("timeout")
            return
        x, y = self._pc_cursor or self._default_cursor()
        segments = self._segments_at(x, y)
        edge = placement.segment.edge
        if not any(seg.edge is edge for seg in segments):
            self._arm_fail_locked("left_edge")
            return
        nx, ny = edge.outward_normal()
        if int(ev.dx) * nx + int(ev.dy) * ny < 0:
            self._arm_fail_locked("reverse")
            return
        self._arm_accum += abs(int(ev.dx)) + abs(int(ev.dy))
        if self._arm_accum >= max(1, int(self._cfg.motion.arm_px)):
            self._arm_accum = 0.0
            self._enter_captured_locked("arm")

    def _arm_fail_locked(self, reason: str) -> None:
        log_event(
            log,
            C.EVENT_ARM_FAIL,
            reason=reason,
            accum=round(self._arm_accum, 1),
        )
        self._arm_accum = 0.0
        self._change_state(State.IDLE, f"arm:{reason}")

    def _segments_at(self, x: int, y: int) -> tuple[Any, ...]:
        band = max(0, int(self._cfg.motion.edge_band_px))
        finder = getattr(self._desktop, "segments_at", None)
        if callable(finder):
            try:
                value = finder(int(x), int(y), band=band)
            except TypeError:
                value = finder(int(x), int(y), band)
            except Exception:
                log.debug("desktop.segments_at 失败", exc_info=True)
                return ()
            return tuple(value or ())
        single = getattr(self._desktop, "segment_at", None)
        if callable(single):
            try:
                segment = single(int(x), int(y), band=band)
            except Exception:
                log.debug("desktop.segment_at 失败", exc_info=True)
                return ()
            return (segment,) if segment is not None else ()
        return ()

    def _resolve_corner_locked(self, segments: Sequence[Any], delta: tuple[int, int]) -> Any | None:
        if not segments:
            return None
        resolver = getattr(self._desktop, "resolve_corner", None)
        if callable(resolver):
            try:
                chosen = resolver(tuple(segments), delta, self._corner_priority())
            except Exception:
                log.debug("desktop.resolve_corner 失败", exc_info=True)
                chosen = None
            if chosen is not None:
                return chosen
        return segments[0] if len(segments) == 1 else None

    def _corner_priority(self) -> Edge | None:
        placement = self._placement
        if placement is not None and placement.corner_priority is not None:
            return placement.corner_priority
        raw = getattr(self._cfg.placement, "corner_priority", None)
        if raw is None:
            return None
        try:
            return Edge(str(raw))
        except ValueError:
            return None

    # -- CAPTURED / RETURNING ----------------------------------------------
    @staticmethod
    def _edge_settle_delta(current: float, target: float, size: int) -> int:
        """边界轴上 warp 残差的补齐方向；非边界轴返回 0。"""
        if size <= 1:
            return 0
        on_edge = abs(target) < 1e-9 or abs(target - float(size - 1)) < 1e-9
        if not on_edge:
            return 0
        gap = target - current
        if abs(gap) < 1e-9:
            return 0
        return 1 if gap > 0 else -1

    @classmethod
    def _warp_to_entry(
        cls,
        estimator: CursorEstimator,
        target_x: float,
        target_y: float,
    ) -> list[tuple[int, int]]:
        """warp 到入口，并补齐边缘轴上的亚像素残差。

        ``warp_to`` 在剩余位移小于 1 单位时停止，平板左缘可能停在 x=0.5；
        入口必须精确落在 x<=0 / x>=W-1 才能让 ``hit()`` 返回出口边。
        """
        reports = estimator.warp_to(target_x, target_y)
        dx = cls._edge_settle_delta(estimator.x, target_x, estimator.width)
        dy = cls._edge_settle_delta(estimator.y, target_y, estimator.height)
        if dx or dy:
            reports.append(estimator.feed(dx, dy, 1.0))
        return reports

    def _pin_to_edge_reports(self, edge: Edge) -> list[tuple[int, int]]:
        """把平板指针真正顶到 ``edge`` 上的补位报文。

        估计器只镜像我们发出的计数，而系统里指针的实际位置我们看不到
        （F5）：程序启动时估计器从 (0,0) 开始，设备那支指针却可能停在屏幕
        中间。于是 warp 只是"镜面"到了边，真实指针还在里面——用户往里推一点
        再拉回来，镜面先到边界，指针离边上还差一截，就表现为没滑到顶就穿回
        电脑了。

        这里补一段沿该边法线的整轴位移：设备在边界会丢掉多余的位移，
        所以最坏也只是指针贴着边停住，正好把镜面与真实位置对齐。
        """
        estimator = self._estimator
        if estimator is None:
            return []
        nx, ny = edge.outward_normal()
        axis = estimator.W if edge.is_vertical else estimator.H
        remaining = int(axis / (estimator.k or 1.0)) + 1
        out: list[tuple[int, int]] = []
        while remaining > 0:
            step = min(C.HID_CLAMP, remaining)
            remaining -= step
            out.append((step * nx, step * ny))
        return out

    def _enter_captured_locked(self, reason: str) -> None:
        if self._state is State.CAPTURED:
            return
        if self._state is State.IDLE:
            # 手动接管的合法路径：IDLE -> ARMING -> CAPTURED（LEGAL_TRANSITIONS）。
            self._change_state(State.ARMING, reason)
        if self._state is not State.ARMING:
            raise PadLendError(
                ErrorCode.UNEXPECTED,
                f"无法从 {self._state.value} 进入 CAPTURED",
            )
        placement = self._placement
        if placement is None:
            raise PadLendError(ErrorCode.PLACEMENT_INVALID, "未设置平板摆位")
        estimator = self._ensure_estimator_locked()
        x, y = self._pc_cursor or self._default_cursor()
        along = y if placement.segment.edge.is_vertical else x
        entry = self._entry_point_locked(placement, along, estimator.width, estimator.height)
        reports = self._warp_to_entry(estimator, entry[0], entry[1])
        clip = self._clip_rect(placement)
        try:
            self._mask.show(clip)
        except Exception as exc:
            self._release_mask("capture_show_failed")
            raise wrap_unexpected(exc, detail="mask.show") from exc
        if not self._change_state(State.CAPTURED, reason):
            self._release_mask("capture_transition_failed")
            return
        self._entry_edge = placement.segment.edge.opposite
        self._overflow = 0.0
        self._neutral_until_release = self._buttons != 0
        self._wheel_accum = 0
        self._hit_logged = False
        self._suppress_logged = False
        # 平板可能还停在上一次会话的"按住"状态（异常退出、重连、换了设备）：
        # 先排队一条全松再发 warp，否则进入后的第一次位移会被它当成拖拽，
        # 松手时就在平板上点了一下。走队列才能保证全松先于 warp 到达。
        self._queue_mouse_locked(0, 0, 0, 0)
        pin = self._pin_to_edge_reports(self._entry_edge)
        self._enqueue_burst_locked([*reports, *pin])
        log_event(
            log,
            C.EVENT_CAP_START,
            edge=placement.segment.edge.value,
            manual=reason == "manual",
            reports=len(reports),
            pin=len(pin),
            point=f"{entry[0]:.0f},{entry[1]:.0f}",
            buttons=f"0x{self._buttons:02x}",
        )

    def _entry_point_locked(
        self,
        placement: Placement,
        pc_along: int,
        tablet_w: int,
        tablet_h: int,
    ) -> tuple[float, float]:
        entry = getattr(self._desktop, "entry_point", None)
        if not callable(entry):
            raise PadLendError(ErrorCode.PLACEMENT_INVALID, "desktop 缺少 entry_point()")
        try:
            point = entry(placement, int(pc_along), int(tablet_w), int(tablet_h))
        except PadLendError:
            raise
        except Exception as exc:
            raise wrap_unexpected(exc, detail="desktop.entry_point") from exc
        try:
            return float(point[0]), float(point[1])
        except (TypeError, ValueError, IndexError) as exc:
            raise wrap_unexpected(exc, detail="desktop.entry_point 返回值") from exc

    def _clip_rect(self, placement: Placement) -> Rect | None:
        """边缘 1px 条（6.6）；返回 ``None`` 时 mask 仍显示但不锁光标。"""
        segment = placement.segment
        x, y = self._pc_cursor if self._pc_cursor is not None else (0, 0)
        along = y if segment.edge.is_vertical else x
        fallback = self._monitor_for_handle(segment.monitor_handle)
        if fallback is None:
            return None
        edge = segment.edge
        expected_line = (
            fallback.rect.right if edge is Edge.RIGHT
            else fallback.rect.left if edge is Edge.LEFT
            else fallback.rect.top if edge is Edge.TOP
            else fallback.rect.bottom
        )
        monitor = None
        for candidate in self._desktop.monitors:
            rect = candidate.rect
            candidate_line = (
                rect.right if edge is Edge.RIGHT
                else rect.left if edge is Edge.LEFT
                else rect.top if edge is Edge.TOP
                else rect.bottom
            )
            if candidate_line != expected_line:
                continue
            if edge.is_vertical and rect.top <= along < rect.bottom:
                monitor = candidate
                break
            if not edge.is_vertical and rect.left <= along < rect.right:
                monitor = candidate
                break
        # 合并边只保留首台显示器 handle，需按实际穿越位置选择锁定条所在屏。
        if monitor is None:
            monitor = fallback
        rect = monitor.rect
        if segment.edge.is_vertical:
            left = rect.right - 1 if segment.edge is Edge.RIGHT else rect.left
            if left < rect.left or left >= rect.right:
                return None
            top = max(rect.top, segment.span_start)
            bottom = min(rect.bottom, segment.span_end)
            if bottom <= top:
                return None
            return Rect(left, top, left + 1, bottom)
        top = rect.bottom - 1 if segment.edge is Edge.BOTTOM else rect.top
        if top < rect.top or top >= rect.bottom:
            return None
        left = max(rect.left, segment.span_start)
        right = min(rect.right, segment.span_end)
        if right <= left:
            return None
        return Rect(left, top, right, top + 1)

    def _handle_captured_locked(self, ev: RawDelta) -> None:
        estimator = self._estimator
        if estimator is None:
            raise PadLendError(ErrorCode.UNEXPECTED, "CAPTURED 状态缺少 estimator")
        if not self._buttons:
            self._neutral_until_release = False
        buttons = self._effective_buttons_locked()
        gain = float(self._cfg.motion.gain)
        clamp = max(1, int(self._cfg.motion.hid_clamp))
        dx, dy = estimator.feed(int(ev.dx), int(ev.dy), gain, clamp=clamp)
        wheel = self._take_wheel_locked(int(ev.wheel))
        self._queue_mouse_locked(buttons, dx, dy, wheel)

        # 出口只有三个条件：回到入口边、没有正在平板拖拽、连续向外推够阈值。
        # 直接进入后立即反向推也应该能返回，不再要求先深入平板固定距离。
        # 这里看物理按键（self._buttons）而不是平板看到的按键：守卫期内平板
        # 看不到按下，但我们仍不该在用户按着键时把他推出去。
        entry = self._entry_edge
        if self._buttons or entry is None or not self._at_edge(estimator, entry):
            self._overflow = 0.0
            return
        nx, ny = entry.outward_normal()
        push = int(ev.dx) * nx + int(ev.dy) * ny
        if push <= 0:
            self._overflow = 0.0
            return
        if not self._hit_logged:
            self._hit_logged = True
            log_event(log, C.EVENT_CAP_HIT, edge=entry.value)
        self._overflow += push
        if self._overflow >= max(1, int(self._cfg.motion.exit_overflow_px)):
            log.debug("router.exit overflow=%.1f", self._overflow)
            self._begin_return_locked("exit")

    def _effective_buttons_locked(self) -> int:
        """返回平板应看到的按键；穿屏前遗留的按下状态不跨设备延续。"""
        return 0 if self._neutral_until_release else self._buttons

    @staticmethod
    def _at_edge(estimator: CursorEstimator, edge: Edge) -> bool:
        """直接检查指定边，避免角落处被 hit() 的单边优先级遮住入口边。"""
        if edge is Edge.LEFT:
            return estimator.x <= 0.0
        if edge is Edge.RIGHT:
            return estimator.x >= estimator.width - 1
        if edge is Edge.TOP:
            return estimator.y <= 0.0
        return estimator.y >= estimator.height - 1

    def _begin_return_locked(self, reason: str, *, debounce: bool = True) -> None:
        if self._state is not State.CAPTURED:
            return
        estimator = self._estimator
        placement = self._placement
        exit_position: tuple[int, int] | None = None
        if estimator is not None and placement is not None:
            try:
                along = estimator.y if placement.segment.edge.is_vertical else estimator.x
                exit_position = self._exit_position_locked(
                    placement,
                    along,
                    estimator.width,
                    estimator.height,
                )
            except Exception as exc:
                log.warning("router.exit_position_failed err=%r", exc)
        try:
            if exit_position is not None:
                self._set_pc_cursor(exit_position)
                self._pc_cursor = exit_position
        finally:
            self._release_mask(reason)
            self._send_keyboard_zero()
            self._reset_keymap_locked()
        self._overflow = 0.0
        self._neutral_until_release = False
        self._wheel_accum = 0
        self._send_mouse_release()
        if not self._change_state(State.RETURNING, reason):
            return
        debounce_ms = max(0, int(self._cfg.motion.debounce_ms)) if debounce else 0
        self._return_deadline = self._clock() + debounce_ms / 1000.0
        log_event(
            log,
            C.EVENT_CAP_END,
            reason=reason,
            debounce_ms=debounce_ms,
            buttons=f"0x{self._buttons:02x}",
        )

    def _exit_position_locked(
        self,
        placement: Placement,
        tablet_along: float,
        tablet_w: int,
        tablet_h: int,
    ) -> tuple[int, int]:
        exit_fn = getattr(self._desktop, "exit_position", None)
        if not callable(exit_fn):
            raise PadLendError(ErrorCode.PLACEMENT_INVALID, "desktop 缺少 exit_position()")
        try:
            position = exit_fn(placement, float(tablet_along), int(tablet_w), int(tablet_h))
        except PadLendError:
            raise
        except Exception as exc:
            raise wrap_unexpected(exc, detail="desktop.exit_position") from exc
        return int(position[0]), int(position[1])

    def _set_pc_cursor(self, position: tuple[int, int]) -> None:
        x, y = int(position[0]), int(position[1])
        for owner in (self._session, self._desktop):
            setter = getattr(owner, "set_cursor_pos", None)
            if not callable(setter):
                continue
            try:
                setter(x, y)
                return
            except Exception:
                log.warning("router.set_cursor_pos_failed owner=%r", owner, exc_info=True)
        setter = getattr(self._desktop, "set_cursor", None)
        if callable(setter):
            try:
                setter(x, y)
                return
            except Exception:
                log.warning("desktop.set_cursor 失败", exc_info=True)
        try:
            windll = getattr(ctypes, "windll", None)
            if windll is not None:
                windll.user32.SetCursorPos(x, y)
        except Exception:  # pragma: no cover - 非 Windows / 权限异常
            log.warning("SetCursorPos 失败", exc_info=True)

    # -- 发送队列 -----------------------------------------------------------
    def _take_wheel_locked(self, units: int) -> int:
        """把 Windows 的 120 单位/格滚轮换算成 HID detent，余数留到下次。"""
        if units:
            self._wheel_accum += int(units)
        detents = int(self._wheel_accum / C.WHEEL_UNITS_PER_DETENT)
        if detents:
            self._wheel_accum -= detents * C.WHEEL_UNITS_PER_DETENT
        return detents

    def _queue_mouse_locked(self, buttons: int, dx: int, dy: int, wheel: int) -> None:
        self._enqueue_send(
            _SendMsg(int(buttons), int(dx), int(dy), int(wheel), stamp=self._clock())
        )

    def _enqueue_burst_locked(self, reports: Iterable[tuple[int, int]]) -> None:
        stamp = self._clock()
        items = [
            _SendMsg(0, int(dx), int(dy), 0, burst=True, stamp=stamp)
            for dx, dy in reports
        ]
        with self._send_lock:
            for item in items:
                self._append_send_locked(item)

    def _enqueue_send(self, message: _SendMsg) -> None:
        with self._send_lock:
            self._append_send_locked(message)

    def _append_send_locked(self, message: _SendMsg) -> None:
        """超过软阈值时丢弃心跳；真实报告始终保留且保持原始顺序。"""
        queue = self._send_queue
        if len(queue) >= C.SEND_QUEUE_MAXLEN:
            if message.is_heartbeat():
                self._metrics.record_drop()
                return
            for index, existing in enumerate(queue):
                if existing.is_heartbeat():
                    del queue[index]
                    break
        queue.append(message)

    # -- sender 线程 --------------------------------------------------------
    def _sender_loop(self) -> None:
        log.debug("sender.start hz=%d", self._report_hz())
        period = 1.0 / self._report_hz()
        timer_active = _timer_begin(C.TIMER_PERIOD_MS)
        next_at = time.perf_counter()
        try:
            while not self._stop_event.is_set():
                self._sender_frame()
                next_at += period
                delay = next_at - time.perf_counter()
                if delay < -period:
                    next_at = time.perf_counter()
                elif delay > 0:
                    self._stop_event.wait(delay)
        except Exception as exc:  # pragma: no cover - 线程级兜底
            wrapped = wrap_unexpected(exc, detail="sender loop")
            log.exception("sender.crashed")
            self._fault(wrapped.code, f"sender 线程崩溃: {wrapped.detail}")
        finally:
            if timer_active:
                _timer_end(C.TIMER_PERIOD_MS)
            log.debug("sender.stop")

    def _sender_frame(self) -> None:
        with self._lock:
            inactive = self._stopped or self._device_suspended
        if inactive:
            # 主动断开后没有接收方；继续发心跳只会制造写失败和状态抖动。
            with self._send_lock:
                self._send_queue.clear()
            self._metrics.set_backlog(0)
            return
        messages = self._drain()
        if messages:
            for message in messages:
                self._send_mouse_message(message)
        else:
            self._send_heartbeat()
        with self._send_lock:
            depth = len(self._send_queue)
        self._metrics.set_backlog(depth)

    def _drain(self) -> list[_SendMsg]:
        """每帧最多 8 条；warp burst 整批一次发出；非 CAPTURED 丢弃位移。"""
        captured = self._state is State.CAPTURED
        out: list[_SendMsg] = []
        with self._send_lock:
            while self._send_queue and len(out) < C.MAX_BATCH:
                head = self._send_queue[0]
                if head.burst:
                    while self._send_queue and self._send_queue[0].burst:
                        message = self._send_queue.popleft()
                        if captured:
                            out.append(message)
                    break
                message = self._send_queue.popleft()
                if (message.dx or message.dy) and not captured:
                    continue
                out.append(message)
        return out

    def _send_heartbeat(self) -> None:
        # 心跳必须带上当前按键状态，但穿屏前遗留的按下不能在平板上重新开始。
        with self._lock:
            captured = self._state is State.CAPTURED
            buttons = self._effective_buttons_locked() if captured else 0
        self._send_mouse_message(_SendMsg(buttons, 0, 0, 0, heartbeat=True))

    def _send_mouse_message(self, message: _SendMsg) -> None:
        try:
            self._session.send_mouse(
                int(message.buttons),
                int(message.dx),
                int(message.dy),
                int(message.wheel),
            )
        except Exception as exc:
            self._note_write_failure(exc)
        else:
            self._note_write_success(message)

    def _note_write_success(self, message: _SendMsg | None = None) -> None:
        with self._lock:
            self._write_failures = 0
        self._metrics.record_send()
        # 7.4 的 rtt_ms：设备端没有 ack，所以量的是"输入进队 → HID 写入成功"，
        # 也就是我们能观测到的那一段链路延迟。心跳不是输入，跳过。
        if message is not None and message.stamp and not message.heartbeat:
            elapsed_ms = (self._clock() - message.stamp) * 1000.0
            self._metrics.record_latency(max(0.0, elapsed_ms))

    def _note_write_failure(self, exc: Exception) -> None:
        with self._lock:
            if self._state is State.FAULT:
                return
            self._write_failures += 1
            failures = self._write_failures
        if failures < C.CONSECUTIVE_WRITE_FAILURES:
            log.debug("sender.write_failed failures=%d err=%r", failures, exc)
            return
        code = exc.code if isinstance(exc, PadLendError) else ErrorCode.SOCKET_BROKEN
        log.warning("sender.write_failed failures=%d err=%r", failures, exc)
        self._fault(code, f"连续 {failures} 次写失败: {exc}")

    def _report_hz(self) -> int:
        raw = int(getattr(self._cfg.motion, "report_hz", C.DEFAULT_REPORT_HZ))
        return max(C.MIN_REPORT_HZ, min(C.MAX_REPORT_HZ, raw))

    # -- supervisor 线程 ----------------------------------------------------
    def _supervisor_loop(self) -> None:
        try:
            while not self._stop_event.is_set():
                self._supervisor_tick()
                self._stop_event.wait(C.HEALTHCHECK_INTERVAL_S)
        except Exception as exc:  # pragma: no cover - 线程级兜底
            wrapped = wrap_unexpected(exc, detail="supervisor loop")
            log.exception("supervisor.crashed")
            self._fault(wrapped.code, f"supervisor 线程崩溃: {wrapped.detail}")

    def _supervisor_tick(self, now: float | None = None) -> None:
        """一次看护周期（测试直接调用并注入 fake clock）。

        首连/重连的阻塞调用在 ``_lock`` 之外执行，锁内只做决策与状态提交；
        因此即使 ADB 等待 40s，GUI 的 ``state`` 读取与 ``on_raw`` 也不会被拖住。
        """
        current = self._clock() if now is None else float(now)
        with self._lock:
            if self._stopped:
                return
            connect_due = (
                self._state is State.FAULT
                and current >= self._next_reconnect_at
                and not self._connect_in_progress
                and not self._device_suspended
            )
            if connect_due:
                self._connect_in_progress = True
        if connect_due:
            self._connect_once()
            return
        with self._lock:
            if self._stopped:
                return
            if self._device_suspended:
                if self._state is State.FAULT:
                    self._change_state(State.IDLE, "device.suspended")
                if self._state is not State.FAULT:
                    self._emit_metrics_locked()
                return
            if self._state is not State.FAULT:
                self._check_health_locked()
                self._check_rotation_locked()
                self._check_desktop_locked()
                self._poll_pointer_speed_locked(current)
                if self._state is State.RETURNING and current >= self._return_deadline:
                    self._change_state(State.IDLE, "debounce")
            self._emit_metrics_locked()

    def _connect_once(self) -> None:
        """在 supervisor 线程里执行一次（可能阻塞的）连接/重连尝试。"""
        facts: DeviceFacts | None = None
        error: BaseException | None = None
        ok = False
        try:
            with self._connect_lock:
                if self._stop_event.is_set():
                    with self._lock:
                        self._connect_in_progress = False
                    return
                facts = self._read_facts()
                if facts is None:
                    starter = getattr(self._session, "start", None)
                    if not callable(starter):
                        raise PadLendError(
                            ErrorCode.DEVICE_OFFLINE,
                            "session 没有 DeviceFacts 且不支持 start()",
                        )
                    facts = starter()
                else:
                    reconnect = getattr(self._session, "reconnect", None)
                    if callable(reconnect):
                        candidate = reconnect()
                        if candidate is not None:
                            facts = candidate
                    else:
                        starter = getattr(self._session, "start", None)
                        if not callable(starter):
                            raise PadLendError(
                                ErrorCode.DEVICE_OFFLINE,
                                "session 不支持 reconnect()/start()",
                            )
                        facts = starter()
                if facts is None:
                    raise PadLendError(ErrorCode.DEVICE_OFFLINE, "连接未返回 DeviceFacts")
                ok = self._session_healthy()
        except PadLendError as exc:
            error = exc
        except Exception as exc:  # pragma: no cover - 防御性兜底
            error = wrap_unexpected(exc, detail="session connect")
            log.exception("router.connect_unexpected")
        with self._lock:
            self._connect_in_progress = False
            if self._stopped:
                return
            if self._device_suspended:
                # 用户在连接过程中点了“断开”：丢弃晚到的 facts，保持 IDLE。
                self._facts = None
                self._estimator = None
                if self._state is not State.FAULT:
                    self._change_state(State.IDLE, "device.suspended")
                self._emit_metrics_locked()
                return
            if ok and facts is not None:
                self._facts = facts
                self._recover_locked()
                self._emit_metrics_locked()
                return
            self._reconnect_attempt += 1
            attempt = self._reconnect_attempt
            backoff = C.RECONNECT_BACKOFF_S
            delay = backoff[min(attempt, len(backoff)) - 1]
            self._next_reconnect_at = self._clock() + delay
            if isinstance(error, PadLendError):
                code = error.code.value
                detail = error.detail or code
            elif error is not None:
                code = ErrorCode.UNEXPECTED.value
                detail = str(error)
            else:
                code = ErrorCode.DEVICE_OFFLINE.value
                detail = "连接后健康检查失败"
            log.warning(
                "router.connect_failed attempt=%d code=%s detail=%s",
                attempt,
                code,
                detail,
            )
            log_event(log, C.EVENT_TABLET_RECONNECT, attempt=attempt, result="fail")
            self._emit_metrics_locked()

    def _check_health_locked(self) -> None:
        if self._session_healthy():
            self._health_failures = 0
            return
        self._health_failures += 1
        if self._health_failures >= C.CONSECUTIVE_WRITE_FAILURES:
            self._fault(
                ErrorCode.SOCKET_BROKEN,
                f"健康检查连续失败 {self._health_failures} 次",
            )

    def _check_rotation_locked(self) -> None:
        facts = self._read_facts()
        if facts is None:
            return
        old = self._facts
        if old is None:
            self._facts = facts
            return
        try:
            changed = (
                int(facts.width) != int(old.width)
                or int(facts.height) != int(old.height)
                or int(facts.orientation) != int(old.orientation)
            )
        except (AttributeError, TypeError, ValueError):
            return
        if not changed:
            return
        try:
            estimator = CursorEstimator(
                int(facts.width),
                int(facts.height),
                float(getattr(facts, "xscale", 1.0) or 1.0),
            )
            estimator.reset()
        except Exception as exc:
            wrapped = wrap_unexpected(exc, detail="rotation reset")
            log.exception("router.rotation_failed")
            self._fault(wrapped.code, f"旋转重建失败: {wrapped.detail}")
            return
        self._estimator = estimator
        self._facts = facts
        self._overflow = 0.0
        log.info(
            "router.rotation width=%d height=%d orientation=%d",
            int(facts.width),
            int(facts.height),
            int(facts.orientation),
        )

    def _check_desktop_locked(self) -> None:
        signature = self._monitor_signature()
        if signature is None:
            return
        if self._desktop_sig is None:
            self._desktop_sig = signature
            return
        if signature == self._desktop_sig:
            return
        self._desktop_sig = signature
        refresh = getattr(self._desktop, "refresh", None)
        if callable(refresh):
            try:
                refresh()
            except Exception as exc:
                log.warning("router.desktop_refresh_failed err=%r", exc)
                return
        log.info("router.desktop_changed monitors=%d", len(signature))
        if self._state is State.CAPTURED:
            self._begin_return_locked("display")
        self._validate_placement_locked()

    def _validate_placement_locked(self) -> None:
        placement = self._placement
        if placement is None:
            return
        exists_fn = getattr(self._desktop, "segment_exists", None)
        if callable(exists_fn):
            try:
                valid = bool(exists_fn(placement.segment))
            except Exception:
                log.debug("desktop.segment_exists 失败", exc_info=True)
                valid = False
        else:
            valid = self._segment_in_current_geometry(placement.segment)
        if not valid:
            self._reanchor_placement_locked()

    def _segment_in_current_geometry(self, segment: Any) -> bool:
        try:
            return segment in tuple(self._desktop.exterior_segments())
        except Exception:
            log.debug("desktop.exterior_segments 失败", exc_info=True)
            return False

    def _reanchor_placement_locked(self) -> None:
        try:
            segments = list(self._desktop.exterior_segments())
        except Exception as exc:
            log.error("router.reanchor_failed err=%r", exc)
            return
        if not segments:
            return
        old = self._placement
        target_edge = old.segment.edge if old is not None else self._config_edge()
        candidates = [segment for segment in segments if segment.edge is target_edge]
        primary = self._primary_monitor()
        if primary is not None:
            same_monitor = [
                segment for segment in candidates if segment.monitor_handle == primary.handle
            ]
            if same_monitor:
                candidates = same_monitor
        if not candidates:
            candidates = list(segments)
        best = max(candidates, key=lambda segment: segment.length)
        self._placement = Placement(
            segment=best,
            offset=0.5,
            align=Align.PROPORTIONAL,
            corner_priority=old.corner_priority if old is not None else self._corner_priority(),
        )
        log.warning(
            "router.placement_reanchored edge=%s span=%s..%s",
            best.edge.value,
            best.span_start,
            best.span_end,
        )

    def _config_edge(self) -> Edge:
        try:
            return Edge(str(self._cfg.placement.edge))
        except (ValueError, AttributeError):
            return Edge.RIGHT

    def _primary_monitor(self) -> Any | None:
        monitors = self._monitors()
        for monitor in monitors:
            if bool(getattr(monitor, "primary", False)):
                return monitor
        return monitors[0] if monitors else None

    def _poll_pointer_speed_locked(self, now: float) -> None:
        last = self._last_pointer_poll
        if last is not None and now - last < C.POINTER_SPEED_POLL_S:
            return
        self._last_pointer_poll = now
        self._restore_pointer_speed()

    def _restore_pointer_speed(self) -> None:
        ensure = getattr(self._session, "ensure_pointer_speed", None)
        if callable(ensure):
            try:
                ensure()
                return
            except Exception:
                log.warning("session.ensure_pointer_speed 失败", exc_info=True)
        speed = getattr(self._facts, "pointer_speed", None)
        if isinstance(speed, int) and speed == 0:
            return
        setter = getattr(self._session, "set_pointer_speed", None)
        if not callable(setter):
            return
        target = int(getattr(self._cfg.misc, "restore_pointer_speed", -3))
        try:
            setter(target)
        except Exception:
            log.warning("session.set_pointer_speed 失败", exc_info=True)

    # -- 健康 / 重连 / FAULT ------------------------------------------------
    def _session_healthy(self) -> bool:
        health_fn = getattr(self._session, "health", None)
        if not callable(health_fn):
            return True
        try:
            report = health_fn()
        except Exception:
            log.debug("session.health 失败", exc_info=True)
            return False
        if isinstance(report, bool):
            return report
        alive = getattr(report, "alive", None)
        socket_ok = getattr(report, "socket_ok", None)
        if isinstance(alive, bool) and isinstance(socket_ok, bool):
            return alive and socket_ok
        for attribute in ("ok", "healthy", "connected"):
            value = getattr(report, attribute, None)
            if isinstance(value, bool):
                return value
        return True

    def _recover_locked(self) -> None:
        if not self._change_state(State.IDLE, "reconnect"):
            return
        self._health_failures = 0
        self._write_failures = 0
        self._reconnect_attempt = 0
        self._next_reconnect_at = 0.0
        self._metrics.set_backlog(0)
        try:
            self._facts = self._read_facts() or self._facts
            estimator = self._ensure_estimator_locked()
            estimator.reset()
            candidate = self._read_facts()
            if candidate is not None:
                self._facts = candidate
                if (
                    int(candidate.width) != int(estimator.width)
                    or int(candidate.height) != int(estimator.height)
                ):
                    self._estimator = CursorEstimator(
                        int(candidate.width),
                        int(candidate.height),
                        float(getattr(candidate, "xscale", 1.0) or 1.0),
                    )
                    self._estimator.reset()
        except PadLendError as exc:
            self._fault(exc.code, f"重连恢复失败: {exc.detail}")
            return
        except Exception as exc:  # pragma: no cover - 防御性兜底
            wrapped = wrap_unexpected(exc, detail="router.recover")
            log.exception("router.recover_failed")
            self._fault(wrapped.code, f"重连恢复失败: {wrapped.detail}")
            return
        log_event(log, C.EVENT_TABLET_RECONNECT, result="ok")

    def _fault(self, code: ErrorCode, detail: str) -> None:
        """进入 FAULT：释放遮罩、停键盘、保留输入回流（I3）。"""
        try:
            with self._lock:
                was_captured = self._state is State.CAPTURED
                if self._state is not State.FAULT:
                    self._change_state(State.FAULT, f"fault:{code.value}")
                self._arm_accum = 0.0
                self._overflow = 0.0
                self._release_mask("fault")
                if was_captured:
                    self._send_keyboard_zero()
                    self._send_mouse_release()
                    self._reset_keymap_locked()
                self._reconnect_attempt = 0
                self._write_failures = 0
                self._next_reconnect_at = self._clock() + C.RECONNECT_BACKOFF_S[0]
                log_event(
                    log,
                    C.EVENT_STATE_FAULT,
                    code=code.value,
                    detail=detail,
                )
        except Exception:  # pragma: no cover - 故障处理自身绝不再抛
            log.exception("router.fault_handler_failed code=%s", code.value)

    # -- 剪贴板（6.8，SHOULD） ------------------------------------------------
    def inject_clipboard(self, text: str) -> bool:
        """把 PC 剪贴板文本注入平板（UTF-8，最多 4KB）。

        只在 ``misc.clipboard_sync`` 打开时生效；不改变状态机。调用方负责在
        GUI 线程读取剪贴板，本方法只做大小限制与发送。
        """
        if not bool(self._cfg.misc.clipboard_sync) or not text:
            return False
        payload = text.encode("utf-8")[:4096].decode("utf-8", "ignore")
        if not payload:
            return False
        try:
            self._session.inject_text(payload)
        except PadLendError as exc:
            log.debug("router.clipboard_skipped code=%s detail=%s", exc.code.value, exc.detail)
            return False
        except Exception as exc:  # noqa: BLE001 - 剪贴板失败不应影响接管
            log.warning("router.clipboard_failed %r", exc)
            return False
        log.info("router.clipboard_sent chars=%d", len(payload))
        return True

    # -- 键盘 ---------------------------------------------------------------
    def on_key(self, ev: KeyEvent) -> bool:
        """键盘入口；返回 ``True`` 表示本机应吞掉该键。

        只有 CAPTURED 且 ``keyboard.forward`` / ``suppress_pc`` 同时成立时才可能
        返回 ``True``；白名单键、保命热键、FAULT/IDLE/RETURNING 一律返回 ``False``。
        """
        try:
            if ev.down:
                # 键盘操作可能伴随窗口切换；趁 IDLE/RETURNING 时刷新一次
                # 绝对光标，避免边缘判定一直用旧的 PC 坐标。
                with self._lock:
                    refresh = self._state is not State.CAPTURED
                if refresh:
                    position = self._query_cursor_pos()
                    if position is not None:
                        with self._lock:
                            self._pc_cursor = position
            with self._lock:
                suppress, report = self._prepare_key_locked(ev)
            if report is None:
                return suppress
            modifier, usages = report
            try:
                self._session.send_key(int(modifier), tuple(int(usage) for usage in usages))
            except Exception as exc:
                # 发送放在 Router 锁外：socket 写阻塞绝不能拖住鼠标 Raw Input。
                self._note_write_failure(exc)
                return False
            self._note_write_success()
            return suppress
        except PadLendError as exc:
            log.warning("router.key_failed code=%s detail=%s", exc.code.value, exc.detail)
            self._fault(exc.code, f"键盘转发失败: {exc.detail}")
        except Exception as exc:  # pragma: no cover - 防御性兜底
            wrapped = wrap_unexpected(exc, detail="router.on_key")
            log.exception("router.key_unexpected")
            self._fault(wrapped.code, f"键盘转发失败: {wrapped.detail}")
        return False

    def _prepare_key_locked(
        self, ev: KeyEvent
    ) -> tuple[bool, tuple[int, tuple[int, ...]] | None]:
        """在锁内完成状态判定与键位表更新，返回 ``(是否屏蔽, 待发报告)``。"""
        self._track_safety(ev)
        if self._is_passthrough(ev):
            return False, None
        if self._state is not State.CAPTURED:
            return False, None
        keyboard = self._cfg.keyboard
        if not bool(keyboard.forward):
            return False, None
        if ev.vk in _SAFETY_LETTERS and self._ctrl_sides and self._alt_sides:
            log.info("router.safety_hotkey vk=0x%02X", ev.vk)
            return False, None
        keymap = self._ensure_keymap_locked()
        if keymap is None:
            return False, None
        report = self._keymap_report(keymap, ev)
        if report is None:
            return False, None
        modifier, usages = report
        suppress = bool(keyboard.suppress_pc)
        if suppress and not self._suppress_logged:
            self._suppress_logged = True
            log_event(log, C.EVENT_KBD_SUPPRESS_ON, layout=str(keyboard.layout))
        return suppress, (int(modifier), tuple(int(usage) for usage in usages))

    def _track_safety(self, ev: KeyEvent) -> None:
        if ev.vk in _CTRL_VKS:
            side = self._modifier_side(ev, right_vk=0xA3, generic_vk=0x11)
            if ev.down:
                self._ctrl_sides.add(side)
            else:
                self._ctrl_sides.discard(side)
        elif ev.vk in _ALT_VKS:
            side = self._modifier_side(ev, right_vk=0xA5, generic_vk=0x12)
            if ev.down:
                self._alt_sides.add(side)
            else:
                self._alt_sides.discard(side)

    @staticmethod
    def _modifier_side(ev: KeyEvent, *, right_vk: int, generic_vk: int) -> int:
        if ev.vk == right_vk:
            return 1
        if ev.vk == generic_vk:
            extended = bool(ev.is_extended or (int(ev.scan) & 0xE000))
            return 1 if extended else 0
        return 0

    def _is_passthrough(self, ev: KeyEvent) -> bool:
        if ev.vk in self._passthrough_vks:
            return True
        keymap = self._keymap
        if keymap is None:
            return False
        checker = getattr(keymap, "is_passthrough", None)
        if not callable(checker):
            return False
        try:
            return bool(checker(ev))
        except Exception:
            log.debug("keymap.is_passthrough 失败", exc_info=True)
            return False

    def _resolve_passthrough(self, cfg: Config) -> frozenset[int]:
        names = list(getattr(cfg.keyboard, "passthrough_keys", ()) or ())
        if not names:
            return frozenset()
        try:
            from tablet.keymap import VK_NAME_TO_CODE
        except Exception:
            mapping = _fallback_vk_names()
        else:
            mapping = dict(VK_NAME_TO_CODE)
        resolved: set[int] = set()
        for raw_name in names:
            name = str(raw_name).strip().upper()
            vk = mapping.get(name)
            if vk is None:
                log.warning("router.passthrough_unknown name=%s", raw_name)
                continue
            resolved.add(int(vk))
        return frozenset(resolved)

    def _ensure_keymap_locked(self) -> Any | None:
        if self._keymap_loaded:
            return self._keymap
        self._keymap_loaded = True
        try:
            from tablet.keymap import Keymap
        except Exception as exc:
            if not self._keymap_warned:
                self._keymap_warned = True
                log.warning("router.keymap_unavailable err=%r", exc)
            return None
        try:
            self._keymap = Keymap(
                passthrough_keys=list(self._cfg.keyboard.passthrough_keys or ()),
            )
        except Exception:
            if not self._keymap_warned:
                self._keymap_warned = True
                log.warning("router.keymap_init_failed", exc_info=True)
            self._keymap = None
        return self._keymap

    def _keymap_report(self, keymap: Any, ev: KeyEvent) -> tuple[int, tuple[int, ...]] | None:
        for name in ("on_key", "feed", "handle", "translate"):
            handler = getattr(keymap, name, None)
            if callable(handler):
                try:
                    return self._normalize_report(handler(ev))
                except Exception:
                    log.warning("router.keymap_failed method=%s", name, exc_info=True)
                    return None
        if callable(keymap):
            try:
                return self._normalize_report(keymap(ev))
            except Exception:
                log.warning("router.keymap_failed callable", exc_info=True)
        return None

    @staticmethod
    def _normalize_report(report: Any) -> tuple[int, tuple[int, ...]] | None:
        """把 keymap 的返回值统一成 ``(modifier, usages)``。"""
        if report is None:
            return None
        if isinstance(report, bytes):
            if len(report) < 2:
                return None
            return int(report[0]), tuple(int(byte) for byte in report[2:] if byte)
        if isinstance(report, tuple) and len(report) == 2:
            modifier, usages = report
            try:
                return int(modifier), tuple(int(usage) for usage in usages)
            except (TypeError, ValueError):
                return None
        modifier = getattr(report, "modifier", getattr(report, "mod", None))
        usages = getattr(report, "usages", getattr(report, "keys", None))
        if modifier is not None and usages is not None:
            try:
                return int(modifier), tuple(int(usage) for usage in usages)
            except (TypeError, ValueError):
                return None
        return None

    def _reset_keymap_locked(self) -> None:
        keymap = self._keymap
        if keymap is None:
            return
        reset = getattr(keymap, "reset", None)
        if not callable(reset):
            return
        try:
            reset()
        except Exception:
            log.warning("keymap.reset 失败", exc_info=True)

    def _send_keyboard_zero(self) -> None:
        try:
            self._session.send_key(0, ())
        except PadLendError as exc:
            # 未连接/启动中属正常路径；不要用 warning+traceback 吓用户。
            log.debug("router.keyboard_zero_skipped code=%s detail=%s", exc.code, exc.detail)
        except Exception:
            log.warning("router.keyboard_zero_failed", exc_info=True)

    def _send_mouse_release(self) -> None:
        """交还控制时补一条"全松"报告。

        不补的话，用户在平板上按住拖动时退出/断连，平板会一直以为左键按着；
        下次穿屏的位移就被当成一次拖拽，松开时正好落在入口处的控件上 ——
        表现就是"鼠标刚穿过去就点了一下平板上的按钮"。
        """
        try:
            self._session.send_mouse(0, 0, 0, 0)
        except PadLendError as exc:
            log.debug("router.mouse_release_skipped code=%s detail=%s", exc.code, exc.detail)
        except Exception:
            log.warning("router.mouse_release_failed", exc_info=True)

    # -- 状态迁移 / 设备 ----------------------------------------------------
    def _change_state(self, dst: State, reason: str) -> bool:
        """唯一状态写入口；非法迁移记日志并转 FAULT（I4）。"""
        with self._lock:
            old = self._state
            if dst is old:
                return False
            if not can_transition(old, dst):
                log.error(
                    "router.illegal_transition %s->%s reason=%s",
                    old.value,
                    dst.value,
                    reason,
                )
                if dst is not State.FAULT:
                    self._fault(
                        ErrorCode.UNEXPECTED,
                        f"非法状态迁移 {old.value}->{dst.value} ({reason})",
                    )
                return False
            self._state = dst
            if dst is not State.CAPTURED:
                self._arm_accum = 0.0
            log.debug("router.state %s->%s reason=%s", old.value, dst.value, reason)
            callback = self.on_state_changed
            if callable(callback):
                try:
                    callback(old, dst, reason)
                except Exception:
                    log.exception("router.state_callback_failed")
            return True

    def _ensure_estimator_locked(self) -> CursorEstimator:
        if self._estimator is not None:
            return self._estimator
        facts = self._facts or self._read_facts()
        if facts is None:
            raise PadLendError(
                ErrorCode.DEVICE_OFFLINE,
                "尚无设备信息；连接由 supervisor 线程异步完成",
            )
        try:
            width = int(facts.width)
            height = int(facts.height)
        except (AttributeError, TypeError, ValueError) as exc:
            raise PadLendError(
                ErrorCode.HANDSHAKE_FAILED,
                f"DeviceFacts 缺少有效尺寸: {facts!r}",
                cause=exc,
            ) from exc
        if width <= 0 or height <= 0:
            raise PadLendError(ErrorCode.HANDSHAKE_FAILED, f"非法设备尺寸 {width}x{height}")
        xscale = float(getattr(facts, "xscale", 1.0) or 1.0)
        self._facts = facts
        self._estimator = CursorEstimator(width, height, xscale)
        self._estimator.reset()
        return self._estimator

    def _read_facts(self) -> DeviceFacts | None:
        facts = getattr(self._session, "facts", None)
        if facts is None:
            return None
        if not hasattr(facts, "width") or not hasattr(facts, "height"):
            return None
        return facts

    def _monitors(self) -> tuple[Any, ...]:
        value = getattr(self._desktop, "monitors", ())
        if callable(value):
            value = value()
        return tuple(value or ())

    def _monitor_for_handle(self, handle: int) -> Any | None:
        for monitor in self._monitors():
            if int(getattr(monitor, "handle", -1)) == int(handle):
                return monitor
        return None

    def _monitor_signature(self) -> tuple[tuple[int, tuple[int, int, int, int]], ...] | None:
        try:
            monitors = self._monitors()
        except Exception:
            log.debug("router.monitors_failed", exc_info=True)
            return None
        signature: list[tuple[int, tuple[int, int, int, int]]] = []
        for monitor in monitors:
            try:
                signature.append((int(monitor.handle), monitor.rect.as_tuple()))
            except Exception:
                log.debug("router.monitor_signature_failed", exc_info=True)
                return None
        return tuple(signature)

    # -- 清理 / 指标 --------------------------------------------------------
    def _abort_capture_locked(self, reason: str) -> None:
        """stop()/emergency 的公共清理：交还光标、释放遮罩、停键盘。"""
        state = self._state
        if state is State.CAPTURED:
            self._begin_return_locked(reason, debounce=False)
        elif state is not State.IDLE:
            self._change_state(State.IDLE, reason)
        self._release_mask(reason)
        self._send_keyboard_zero()
        self._send_mouse_release()
        self._reset_keymap_locked()
        self._arm_accum = 0.0
        self._overflow = 0.0

    def _release_mask(self, reason: str) -> None:
        releaser = getattr(self._mask, "release", None)
        if not callable(releaser):
            return
        try:
            releaser()
        except Exception:
            log.exception("router.mask_release_failed reason=%s", reason)

    def _emit_metrics_locked(self) -> None:
        snapshot = self._metrics.snapshot()
        callback = self.on_metrics
        if not callable(callback):
            return
        try:
            callback(snapshot)
        except Exception:
            log.exception("router.metrics_callback_failed")

    def __del__(self) -> None:
        with contextlib.suppress(Exception):
            self._release_mask("del")
