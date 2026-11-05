"""PadLend — 把 Android 平板变成 Windows 的扩展输入屏。

顶层包只做版本声明（``__version__`` 是唯一版本源，``constants.VERSION``、
``pyproject.toml`` 的动态版本都由它派生）；真正的入口在 ``main.py``
（``padlend`` 控制台脚本和 ``python -m padlend`` 都转调它）。
"""

from __future__ import annotations

__version__ = "1.0.0"
__all__ = ["__version__"]
