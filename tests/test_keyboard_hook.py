"""低级键盘钩子的真实安装冒烟（仅 Windows）。

回归：``GetCurrentThreadId`` 属于 kernel32，误从 user32 取会报
``function 'GetCurrentThreadId' not found``，导致钩子线程启动即失败（E200）。
"""

from __future__ import annotations

import sys

import pytest

pytestmark = pytest.mark.skipif(sys.platform != "win32", reason="仅 Windows")


def test_install_and_uninstall() -> None:
    from win.keyboard_hook import KeyboardHook

    hook = KeyboardHook(lambda event: False)
    hook.install()
    try:
        assert hook.healthy is True
    finally:
        hook.uninstall()
    assert hook.healthy is False


def test_uninstall_is_idempotent() -> None:
    from win.keyboard_hook import KeyboardHook

    hook = KeyboardHook(lambda event: False)
    hook.uninstall()
    hook.uninstall()
