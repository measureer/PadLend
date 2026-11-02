"""热键模块 Win32 加载冒烟（无 Win32 环境跳过）。"""

from __future__ import annotations

import sys

import pytest

pytestmark = pytest.mark.skipif(sys.platform != "win32", reason="仅 Windows")


def test_kernel32_get_current_thread_id_available() -> None:
    from win.hotkeys import _load_kernel32, _load_user32

    _load_user32()  # 不应因 GetCurrentThreadId 不在 user32 而抛错
    assert _load_kernel32().GetCurrentThreadId() > 0
