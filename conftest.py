"""pytest 根配置：把仓库根目录加入 sys.path，保证 `import core/win/tablet/ui` 可用。"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
