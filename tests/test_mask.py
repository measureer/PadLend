"""``win/mask.py`` 单测：遮罩 / ClipCursor 生命周期与边缘条几何。

全部业务逻辑都通过 ``MaskApi`` 假实现驱动，不创建真实窗口、不触碰用户光标；
真实 Win32 路径只在 ``PADLEND_UI_TESTS=1`` 时运行（会短暂显示遮罩窗口）。
"""

from __future__ import annotations

import ctypes
import os
import sys
import threading
from collections.abc import Sequence

import pytest

from core.geometry import Edge, EdgeSegment, Rect
from win import mask as mask_module
from win.mask import (
    WS_EX_LAYERED,
    WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST,
    WS_EX_TRANSPARENT,
    CursorMask,
    Win32MaskApi,
    clip_rect_for_segment,
)

MON_A = Rect(0, 0, 1920, 1080)
MON_B = Rect(1920, 0, 3840, 1080)
BAND_LEFT = Rect(0, 0, 1, 1080)

_RUN_UI_TESTS = os.environ.get("PADLEND_UI_TESTS") == "1"


class FakeMaskApi:
    """记录调用的假后端；可注入故障，用来验证失败路径与幂等性。"""

    def __init__(
        self,
        monitors: Sequence[Rect] | None = None,
        *,
        fail_create_at: int | None = None,
        fail_destroy: bool = False,
        fail_clip: bool = False,
        clip_ok: bool = True,
        topmost_repair_ok: bool = True,
    ) -> None:
        self.monitors: list[Rect] = list(MON_A if monitors is None else monitors)
        self.fail_create_at = fail_create_at
        self.fail_destroy = fail_destroy
        self.fail_clip = fail_clip
        self.clip_ok = clip_ok
        self.topmost_repair_ok = topmost_repair_ok
        self.created: list[Rect] = []
        self.destroyed: list[int] = []
        self.close_posted: list[int] = []
        self.clip_calls: list[Rect | None] = []
        self.cursor_hidden_calls: list[bool] = []
        self.cursor_refreshes: list[int] = []
        self.ensure_topmost_calls: list[int] = []
        self.topmost: dict[int, bool] = {}
        self.cursor_shown = False
        self._next_handle = 100

    def monitor_rects(self) -> list[Rect]:
        return list(self.monitors)

    def create_mask_window(self, rect: Rect) -> int:
        if self.fail_create_at is not None and len(self.created) == self.fail_create_at:
            raise OSError("CreateWindowExW 失败（测试注入）")
        handle = self._next_handle
        self._next_handle += 1
        self.created.append(rect)
        self.topmost[handle] = True
        return handle

    def destroy_window(self, hwnd: int) -> None:
        if self.fail_destroy:
            raise OSError("DestroyWindow 失败（测试注入）")
        self.destroyed.append(hwnd)

    def clip_cursor(self, rect: Rect | None) -> bool:
        self.clip_calls.append(rect)
        if self.fail_clip:
            raise OSError("ClipCursor 失败（测试注入）")
        return self.clip_ok

    def clip_rect(self) -> Rect | None:
        for rect in reversed(self.clip_calls):
            if rect is not None:
                return rect
        return None

    def set_cursor_hidden(self, hidden: bool) -> None:
        self.cursor_hidden_calls.append(hidden)
        self.cursor_shown = not hidden

    def cursor_visible(self) -> bool | None:
        return self.cursor_shown

    def ensure_topmost(self, hwnd: int) -> bool:
        self.ensure_topmost_calls.append(hwnd)
        if not self.topmost_repair_ok:
            return False
        self.topmost[hwnd] = True
        return True

    def window_topmost(self, hwnd: int) -> bool | None:
        return self.topmost.get(hwnd)

    def refresh_cursor(self, hwnd: int) -> None:
        self.cursor_refreshes.append(hwnd)

    def post_close_window(self, hwnd: int) -> bool:
        self.close_posted.append(hwnd)
        return True


def make_mask(api: FakeMaskApi) -> CursorMask:
    return CursorMask(api=api)


# ---------------------------------------------------------------------------
# show / hide
# ---------------------------------------------------------------------------
def test_show_creates_one_window_per_monitor() -> None:
    api = FakeMaskApi([MON_A, MON_B])
    mask = make_mask(api)

    mask.show(None)

    assert mask.active is True
    assert api.created == [MON_A, MON_B]
    # None 语义 = 只显示遮罩，不改动当前 ClipCursor（6.6）
    assert api.clip_calls == []
    assert mask.clip_owned is False
    assert mask.clip_rect is None


def test_show_with_clip_rect_locks_cursor() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(BAND_LEFT)

    assert api.clip_calls == [BAND_LEFT]
    assert mask.clip_owned is True
    assert mask.clip_rect == BAND_LEFT
    assert mask.active is True


def test_repeated_show_does_not_rebuild_windows() -> None:
    api = FakeMaskApi([MON_A, MON_B])
    mask = make_mask(api)

    mask.show(None)
    mask.show(None)
    mask.show(BAND_LEFT)

    assert len(api.created) == 2
    assert api.destroyed == []


def test_show_none_after_clip_releases_clip() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(BAND_LEFT)
    mask.show(None)

    assert api.clip_calls == [BAND_LEFT, None]
    assert mask.clip_owned is False
    assert mask.active is True


def test_hide_destroys_windows_and_unclips() -> None:
    api = FakeMaskApi([MON_A, MON_B])
    mask = make_mask(api)

    mask.show(BAND_LEFT)
    mask.hide()

    assert api.destroyed == [100, 101]
    assert api.clip_calls == [BAND_LEFT, None]
    assert mask.active is False
    assert mask.clip_owned is False
    assert mask.clip_rect is None


def test_hide_without_show_is_noop() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.hide()

    assert api.created == []
    assert api.destroyed == []
    assert api.clip_calls == []
    assert mask.active is False


def test_show_after_hide_creates_windows_again() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(None)
    mask.hide()
    mask.show(None)

    assert len(api.created) == 2
    assert mask.active is True


# ---------------------------------------------------------------------------
# 失败与兜底
# ---------------------------------------------------------------------------
def test_show_rolls_back_when_window_creation_fails() -> None:
    api = FakeMaskApi([MON_A, MON_B], fail_create_at=1)
    mask = make_mask(api)

    with pytest.raises(OSError):
        mask.show(BAND_LEFT)

    assert mask.active is False
    assert mask.clip_owned is False
    assert api.clip_calls == []
    assert api.destroyed == [100]  # 已创建的第一个窗口被回滚
    # 回滚后再 show 仍然可用
    api.fail_create_at = None
    mask.show(None)
    assert mask.active is True
    assert len(api.created) == 3


def test_show_keeps_mask_when_clip_fails() -> None:
    api = FakeMaskApi([MON_A], clip_ok=False)
    mask = make_mask(api)

    mask.show(BAND_LEFT)

    assert mask.active is True
    assert mask.clip_owned is False
    assert mask.clip_rect is None
    assert api.clip_calls == [BAND_LEFT]


def test_show_keeps_mask_when_clip_raises() -> None:
    api = FakeMaskApi([MON_A], fail_clip=True)
    mask = make_mask(api)

    mask.show(BAND_LEFT)

    assert mask.active is True
    assert mask.clip_owned is False


def test_release_is_idempotent() -> None:
    api = FakeMaskApi([MON_A, MON_B])
    mask = make_mask(api)

    mask.show(BAND_LEFT)
    mask.release()
    mask.release()
    mask.release()

    assert mask.active is False
    assert mask.clip_owned is False
    assert mask.clip_rect is None
    assert api.destroyed == [100, 101]
    # 每次 release 都无条件 ClipCursor(None)：宁可多解一次，绝不泄漏锁定
    assert api.clip_calls == [BAND_LEFT, None, None, None]


def test_release_without_show_still_unclips() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.release()

    assert api.clip_calls == [None]
    assert api.destroyed == []
    assert mask.active is False


def test_release_survives_api_failures() -> None:
    api = FakeMaskApi([MON_A, MON_B], fail_clip=True, fail_destroy=True)
    mask = make_mask(api)

    mask.show(None)
    mask.release()  # 不抛
    mask.release()  # 仍然不抛，且内部状态已清空

    assert mask.active is False
    assert mask.clip_owned is False
    assert len(api.clip_calls) == 2
    assert api.destroyed == []


def test_release_then_show_works_again() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(BAND_LEFT)
    mask.release()
    mask.show(BAND_LEFT)

    assert mask.active is True
    assert mask.clip_owned is True
    assert api.clip_rect() == BAND_LEFT


def test_show_registers_atexit_guard() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(None)

    assert mask_module._guard_registered is True


def test_ex_style_matches_design() -> None:
    # 接管期遮罩必须接收 WM_SETCURSOR，所以不再使用 WS_EX_TRANSPARENT。
    # 1.1.1：样式里必须带 WS_EX_TOPMOST，否则任务栏会盖住遮罩、PC 光标重新显示。
    expected = WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST
    assert expected == CursorMask.EX_STYLE
    assert not (CursorMask.EX_STYLE & WS_EX_TRANSPARENT)


def test_show_refreshes_cursor_without_mouse_move() -> None:
    # 1.1.1：遮罩创建后不能等用户动鼠标才执行 WM_SETCURSOR，必须立即投递一次。
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(BAND_LEFT)

    assert api.cursor_refreshes == [100]
    mask.hide()


def test_repeated_show_reasserts_topmost() -> None:
    # 两次接管之间系统可能把 topmost 抢走；复用窗口时必须重新置顶。
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(None)
    assert api.ensure_topmost_calls == []

    api.topmost[100] = False
    mask.show(None)

    assert api.ensure_topmost_calls == [100]
    assert api.topmost[100] is True
    mask.hide()


def test_watchdog_repairs_topmost_and_cursor() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)
    mask.show(BAND_LEFT)
    api.cursor_refreshes.clear()

    api.topmost[100] = False
    api.cursor_shown = True  # 模拟别的进程把系统光标显示回来
    mask._watchdog_tick()

    assert api.ensure_topmost_calls == [100]
    assert api.topmost[100] is True
    assert api.cursor_refreshes == [100]
    mask.hide()


def test_watchdog_tick_is_noop_before_show() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask._watchdog_tick()

    assert api.ensure_topmost_calls == []
    assert api.cursor_refreshes == []


def test_watchdog_stops_on_hide() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)
    mask.show(None)
    thread = mask._watchdog_thread
    assert thread is not None
    assert thread.is_alive()

    mask.hide()

    assert mask._watchdog_thread is None
    thread.join(timeout=2.0)
    assert not thread.is_alive()


def test_diagnose_reports_topmost() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)

    mask.show(None)

    assert "topmost=[True]" in mask.diagnose()
    mask.hide()


# ---------------------------------------------------------------------------
# 边缘 1px 条几何（show 的 clip_rect 由 Router 用同名纯函数算出）
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    ("edge", "span_end", "expected"),
    [
        (Edge.LEFT, 1080, Rect(0, 0, 1, 1080)),
        (Edge.RIGHT, 1080, Rect(1919, 0, 1920, 1080)),
        (Edge.TOP, 1920, Rect(0, 0, 1920, 1)),
        (Edge.BOTTOM, 1920, Rect(0, 1079, 1920, 1080)),
    ],
)
def test_clip_rect_for_full_edge(edge: Edge, span_end: int, expected: Rect) -> None:
    segment = EdgeSegment(1, edge, 0, span_end)
    assert clip_rect_for_segment(MON_A, segment) == expected


def test_clip_rect_respects_span() -> None:
    segment = EdgeSegment(1, Edge.LEFT, 100, 300)
    assert clip_rect_for_segment(MON_A, segment) == Rect(0, 100, 1, 300)


def test_clip_rect_clips_span_to_monitor() -> None:
    segment = EdgeSegment(1, Edge.RIGHT, -50, 200)
    assert clip_rect_for_segment(MON_B, segment) == Rect(3839, 0, 3840, 200)


def test_clip_rect_falls_back_for_stale_span() -> None:
    segment = EdgeSegment(1, Edge.LEFT, 5000, 6000)
    assert clip_rect_for_segment(MON_A, segment) == Rect(0, 0, 1, 1080)


def test_clip_rect_uses_thickness_override() -> None:
    segment = EdgeSegment(1, Edge.LEFT, 0, 1080)
    assert clip_rect_for_segment(MON_A, segment, thickness=3) == Rect(0, 0, 3, 1080)


def test_clip_rect_handles_negative_origin_monitor() -> None:
    monitor = Rect(-1920, 0, 0, 1080)
    left = EdgeSegment(1, Edge.LEFT, 0, 1080)
    right = EdgeSegment(1, Edge.RIGHT, 0, 1080)
    assert clip_rect_for_segment(monitor, left) == Rect(-1920, 0, -1919, 1080)
    assert clip_rect_for_segment(monitor, right) == Rect(-1, 0, 0, 1080)


# ---------------------------------------------------------------------------
# 真实 Win32（默认跳过）
# ---------------------------------------------------------------------------
@pytest.mark.skipif(sys.platform != "win32", reason="需要真实 Windows API")
def test_real_mask_release_is_safe() -> None:
    mask = CursorMask()
    mask.release()
    mask.release()
    assert mask.active is False
    assert mask.clip_owned is False


@pytest.mark.skipif(not _RUN_UI_TESTS, reason="真实窗口测试需 PADLEND_UI_TESTS=1")
def test_real_mask_show_and_hide() -> None:
    api = Win32MaskApi()
    mask = CursorMask(api=api)
    try:
        mask.show(None)
        assert mask.active is True
        assert api.monitor_rects()
    finally:
        mask.release()
    assert mask.active is False
    assert mask.clip_owned is False


def test_cursor_hidden_while_mask_active_and_restored() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)
    mask.show(None)
    mask.show(None)  # 重复 show 只隐藏一次，hide 只恢复一次
    assert api.cursor_hidden_calls == [True]
    mask.hide()
    assert api.cursor_hidden_calls == [True, False]
    mask.hide()  # 幂等
    assert api.cursor_hidden_calls == [True, False]


def test_release_restores_cursor() -> None:
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)
    mask.show(BAND_LEFT)
    mask.release()
    assert api.cursor_hidden_calls == [True, False]


def test_set_cursor_hidden_never_touches_show_cursor(monkeypatch: pytest.MonkeyPatch) -> None:
    """ShowCursor 是进程级计数；任何一次跨线程失衡都会让光标永久消失。"""
    calls: list[object] = []

    class _FakeUser32:
        def SetCursor(self, handle: object) -> None:
            calls.append(handle)

        def LoadCursorW(self, instance: object, name: object) -> int:
            calls.append(name)
            return 123

    monkeypatch.setattr(mask_module, "_user32", lambda: _FakeUser32())
    api = Win32MaskApi()
    api.set_cursor_hidden(True)
    api.set_cursor_hidden(False)

    assert calls[0] is None
    assert isinstance(calls[1], ctypes.c_void_p) and calls[1].value == 32512
    assert calls[2] == 123


def test_cross_thread_release_posts_close_instead_of_destroy() -> None:
    """遮罩窗口由 rawinput 线程创建；GUI 线程释放时必须 PostMessage(WM_CLOSE)。"""
    api = FakeMaskApi([MON_A])
    mask = make_mask(api)
    mask.show(None)

    worker = threading.Thread(target=mask.release)
    worker.start()
    worker.join(timeout=2.0)

    assert not worker.is_alive()
    assert api.close_posted == [100]
    assert api.destroyed == []
