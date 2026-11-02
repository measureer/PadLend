# 实施说明（给实施者/AI agent）

本文件记录设计文档之下的**实施约定与已冻结的共享地基**。动手前先读：

1. `docs/DESIGN.md` 第 3 章（实测数字，不要改）
2. `docs/DESIGN.md` 第 8 章（冻结接口契约）
3. 本文件（共享模块、环境、命令、偏差记录）

---

## 1. 当前状态

| 模块 | 状态 | 负责人 |
|---|---|---|
| `pyproject.toml` / `requirements.txt` | ✅ 完成 | Lead |
| `constants.py` | ✅ 完成（HID 描述符、超时、事件词表） | Lead |
| `core/geometry.py` | ✅ 完成（Rect/MonitorInfo/Edge/EdgeSegment/Placement/Align） | Lead |
| `core/events.py` / `core/state.py` / `core/errors.py` / `core/logsetup.py` | ✅ 完成 | Lead |
| `config.py` | ✅ 完成（加载/校验/修复/迁移/原子落盘） | Lead |
| `tablet/cursor.py` | ✅ 完成（CursorEstimator） | Lead |
| `tablet/{adb,protocol,uhid,server,session}.py` | ⏳ 待实现 | `tablet-io` |
| `tablet/keymap.py` + `win/keyboard_hook.py` + `win/hotkeys.py` | ⏳ 待实现 | `keyboard` |
| `win/{dpi,desktop,single_instance}.py` | ⏳ 待实现 | `win-geometry` |
| `win/{rawinput,mask}.py` | ⏳ 待实现 | `win-input` |
| `core/{router,metrics}.py` | ⏳ 待实现 | `core-router` |
| `ui/*`（主窗/右栏/托盘/日志/主题/向导/诊断） | ⏳ 待实现 | `ui-shell` |
| `ui/{canvas,canvas_items}.py` | ⏳ 待实现 | `ui-canvas` |
| `main.py` 装配、集成测试、vendor jar、打包、文档 | ⏳ 待实现 | Lead |

**不要修改**其它任务范围内的文件。需要改共享地基（本表 ✅ 的模块）时，先给 Lead 发消息。

## 2. 关键实施决策（与文档的偏差/补充）

1. **几何类型定义在 `core/geometry.py`，`win/desktop.py` 重新导出。**
   文档 8.1 把它们列在 `win/desktop.py`。为满足"core 不得 import win"，唯一定义放在
   core（纯 Python，无 win32），`win/desktop.py` 必须写：
   `from core.geometry import Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect`
   并保证 `from win.desktop import Edge` 等既有写法继续可用。
2. **core 运行时不 import win。** `core/router.py` 对注入的 desktop/mask/session 走结构化
   访问；类型注解放 `if TYPE_CHECKING:`。允许 import `core.geometry`、`tablet.cursor`
   （纯计算）、`config`/`constants`。
3. **包布局**：按 7.1，`core/ win/ tablet/ ui/` 是仓库根的顶层包；`py-modules` 含
   `main/config/constants`。为满足 S1-6 的 `python -m padlend`，额外提供
   `padlend/__main__.py` 薄壳（调用 `main.main()`），不承载业务逻辑。
4. **Python 3.13 + PySide6 6.12**：机器上只有 Python 3.13，依赖装在 `C:\Code\PadLend\.venv`。
   文档要求 3.11+，满足；PySide6 6.12 在 `<7` 范围内。
5. **无真机连接**：当前 `adb devices` 为空。所有单元测试必须能在无设备时通过；
   依赖设备的测试用 `pytest.mark.skipif` 跳过，并打印原因。`tools/probe.py` 在有设备时
   输出事实表，无设备时给出可操作提示（退出码 2）。
6. **窗口/钩子代码只在 Windows 上运行**：本项目只支持 Windows，不需要跨平台兼容，
   但**模块 import 不得产生副作用**（不得在 import 时创建窗口/装钩子）。

## 3. 代码规范（7.3）

- Python 3.11+ 语法，`from __future__ import annotations`，全量类型注解。
- `ruff` line-length 100；禁止 `except: pass`；线程函数必须有名字并 try/except 兜底。
- 日志用 `logging.getLogger("模块名")`；事件日志用 `core.logsetup.log_event`。
- 错误一律用 `core.errors.PadLendError` + `ErrorCode`（8.4）。
- 所有跨线程数据用 `core/events.py` 的 frozen dataclass。

## 4. 环境与命令

```powershell
cd C:\Code\PadLend

# 依赖已装好（.venv）
.\.venv\Scripts\python.exe -m pytest -q
.\.venv\Scripts\python.exe -m ruff check .

# 无设备时的 probe
.\.venv\Scripts\python.exe tools\probe.py
```

- 不要 `pip install` 新依赖；不要联网下载（vendor jar 由 Lead 处理）。
- 不要 `git commit` / `git checkout` / `git reset`：Lead 统一提交，避免互相覆盖。
- 不要改 `.gitignore`、`pyproject.toml`、`conftest.py`。

## 5. 共享契约速查

```python
# core/geometry.py
Rect(left, top, right, bottom)          # right/bottom 开区间，物理像素，可为负
MonitorInfo(handle, device, rect, dpi, primary)
Edge.LEFT/RIGHT/TOP/BOTTOM              # .outward_normal() -> (dx,dy); .is_vertical
EdgeSegment(monitor_handle, edge, span_start, span_end)   # 沿边坐标，end 开区间
Placement(segment, offset, align, corner_priority)
Align.PROPORTIONAL/CENTER/ONE_TO_ONE

# core/events.py
RawDelta(dx, dy, buttons=0, wheel=0, absolute=False, ts=0.0)
KeyEvent(vk, scan, is_extended, down, ts=0.0)

# core/state.py
State.IDLE/ARMING/CAPTURED/RETURNING/FAULT; LEGAL_TRANSITIONS; can_transition(a,b)

# config.py
Config.load(path)/save(path)/validate(); Config.issues; 各段 dataclass

# tablet/cursor.py
CursorEstimator(w,h,xscale): reset(x=None,y=None) / feed(dpx,dpy,gain,clamp=127)
    -> (cx,cy) / hits() / clamp() / warp_to(tx,ty) -> list[(cx,cy)] / position
```

其余签名以 `docs/DESIGN.md` 第 8 章为准（冻结契约，公开签名不得改）。

## 6. 任务板

每个实施者从 `team_task_list` 领取自己的任务，完成后 `team_task_update(action="complete")`
并向 Lead 发送简报（完成项 / 测试命令与结果 / 未决风险）。写作用域见任务描述。
