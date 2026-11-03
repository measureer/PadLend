"""pytest 根配置：把仓库根目录加入 sys.path，保证 `import core/win/tablet/ui` 可用。"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))


def pytest_collection_modifyitems(items: list[object]) -> None:
    """让 Qt 相关测试先跑。

    本机 Windows 上，如果先跑低级钩子/热键（真实 Win32 线程）再跑
    ``QApplication.processEvents()``，Qt 事件循环偶发 access violation。
    把 UI/画布测试排到最前可稳定复现到干净进程状态，产品代码不受影响。
    """

    def priority(item: object) -> int:
        nodeid = getattr(item, "nodeid", "")
        if "test_ui_smoke" in nodeid:
            return 0
        if "test_canvas" in nodeid:
            return 1
        return 2

    items.sort(key=priority)

