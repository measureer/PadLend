# 实施说明（给实施者/AI agent）

> **版本提示**：本文记录 1.0.x 的实施约定；`ui/`、`PadLend.spec` 等 Qt 相关路径
> 已在 1.1.0 删除。当前目录结构与构建流程见 [README.md](../README.md) 与
> [TAURI_MIGRATION.md](TAURI_MIGRATION.md)。

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

## 7. 真机实测偏差（2026-11-02，Honor CGA-W00 / Android 17）

1. **握手是 1 字节，不是 4 字节**：scrcpy 5.0.1 在 `tunnel_forward=true` 且
   `video=false audio=false control=true` 时只 accept 一个 control socket，并写
   一个 `0x00`。`constants.HANDSHAKE_DUMMY = b"\x00"`，`DESIGN.md` 已就地勘误。
2. **jar 需重打包**：官方 jar 的 `classes.dex` 是 DEFLATE，Android 17 的
   `app_process` 会 `ClassNotFoundException`；`tools/repack_server_jar.py` 把
   `classes.dex` 以 stored 方式放回，vendor jar 已替换（2,316,791 B，
   sha256 `d0127708…d810ca`），`vendor/SHA256SUMS` 已更新。
3. **cleanup=true 会删 jar**：`CleanUp.main()` 启动即 `unlinkSelf()` 删除
   CLASSPATH 首项，所以每次 start/reconnect 都要重新 push 2.3MB；属预期行为。
4. **幽灵进程清理模式**：当前进程命令行只有 `com.genymobile.scrcpy.Server`
   和 `com.genymobile.scrcpy.CleanUp`，不含 jar 文件名；`tablet/server.py`
   已改为三模式轮询 pkill（jar 名保留兼容）。
5. **尺寸按当前旋转**：`wm size` 给自然方向尺寸；`orientation` 为 1/3 时
   `read_facts()` 用 `oriented_size()` 交换宽高（真机 orientation=1 →
   3200×2136）。
6. **配置目录回退**：本机 Python 不能写 `%APPDATA%`，`constants.config_dir()`
   依次尝试 `PADLEND_HOME` / `%APPDATA%` / `%LOCALAPPDATA%` / `<项目>/.padlend`，
   用真实写探针判定；当前实际落在 `<项目>/.padlend`。

## 8. 0.2.0 UI 重设计与设备管理

- 主窗由「顶栏 + 左右分栏」改为「顶栏 + 84px 导航栏 + QStackedWidget 四页」：设备 / 布局 / 设置 / 日志；旧 `QSplitter` 布局与 emoji 按钮已移除。
- 新增 `ui/device_page.py`：后台 `QThread` 扫描 adb，设备列表与详情卡；连接/断开由 `Router.connect_device(serial)` / `Router.disconnect_device()` 驱动。
- `Router` 新增 `_device_suspended`：用户主动断开后 supervisor 不再自动重连；`connect_device()` 切换 `cfg.device.serial` 并清空旧 facts/estimator。
- 未连接门控：`TabletItem` 默认 `connected=False`，只画“未连接设备 / 连接后显示实际尺寸”；`MainWindow.update_device(None)` 时设备页分辨率/方向为“—”，不出现任何型号默认尺寸。
- 无设备时 Router 的 FAULT 不再直接显示为“故障”：`_present_state()` 在无 facts 时映射为“未连接”（连接中映射为“连接中”），避免误报。
- 设置页由 SettingsPanel 自带滚动区直接铺满页面（0.2.5 去掉了外层重复滚动与 760px 限宽）；0.2.5 起为左右两列：平板位置/键盘 | 手感/快捷键，杂项跨两列。
- 主题：新深色分层配色、12px 卡片圆角、QPainter 图标与渐变品牌标识；禁用态按钮补样式。

## 9. 0.2.1 白天/黑夜主题

- `ui/theme/tokens.py` 提供 `DARK_COLORS`/`LIGHT_COLORS`；`COLORS` 是可变字典，`set_theme()` 就地更新（保持其他模块持有的引用有效），并重建 `STATE_COLORS`/`LOG_EVENT_COLORS`。
- `ui/style.py::apply_theme(app, theme)` 在应用 QSS 前切换令牌并清缓存；QSS 仍是同一份 `dark.qss` 模板，只是 `@token` 值不同。
- 顶栏太阳/月亮按钮 → `MainWindow.toggle_theme()` → `apply_theme_choice()`：应用主题、重绘品牌标/导航/头部图标、状态点、设备页、画布、托盘标题栏，并写回 `config.ui.theme`。
- 画布图元 `color()` 每次绘制读取当前 `tokens.COLORS`（修复旧实现读取不存在的 `TOKENS`）；`win32` 标题栏明暗跟随主题。
- 配置校验：`ui.theme` 仅接受 `dark`/`light`，非法值回退 `dark` 并记录 issue。

## 10. 0.2.5–0.2.6 设置页说明与进入保护

- 0.2.5：设置页改为两列等分布局，去掉外层重复滚动与 760px 限宽，保存按钮通栏。
- 0.2.6：每个控件下方增加 `QLabel#fieldHint` 说明（小字号、`@text_secondary`、自动换行）；
  `_labeled_row()` 接受 hint，`_check_row()` 负责“复选框 + 说明”的组合。
- 新增“进入保护”滑杆，绑定 `config.motion.entry_guard_px`（0–300，默认 160），
  与 Router 的 `_entry_depth_ok` 门槛一一对应；此前该参数只能手改 YAML。
- 杂项改为两列，避免宽屏右侧空白；测试 `test_settings_hints_and_entry_guard`
  断言说明存在、进入保护值能保存并写回 Config。

## 11. 1.0.2 布局摆位与边缘穿越

- 设置页删除「平板位置 / 吸附边 / 对齐」分组，`SettingsPanel` 不再接收 `desktop`；
  摆位只由「桌面布局」页决定，避免设置里的边与实际加载段不一致。
- 布局页拖动、松手、右键对齐/移除都只更新画布预览和 `placementEdited/Committed`
  信号；点「保存布局」才调用 `_apply_placement_to_config()` 写配置、`Router.set_placement()`
  立即生效。有未保存改动时标题旁显示提示。
- `core/router.py` 不再把同朝向的其它显示器边缘 adopt 进来。进入前用平板矩形
  （`core/geometry.tablet_rect`）检查压线点是否真的落在平板覆盖范围内，只允许从
  实际接触的段进入；入口/出口按 1:1 映射。接触判定必须按候选段自己的朝向取沿边坐标
  （竖直段用 y、水平段用 x），这是修复 DISPLAY1 左下角进不去的根因。
- 「适应窗口」改名「显示全部」，行为不变；`fit_view()` 仍扩展 `sceneRect`
  以容纳桌面包围盒之外的平板。

## 12. 任务板

每个实施者从 `team_task_list` 领取自己的任务，完成后 `team_task_update(action="complete")`
并向 Lead 发送简报（完成项 / 测试命令与结果 / 未决风险）。写作用域见任务描述。
