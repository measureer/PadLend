# 验收记录

> 环境：Windows 11、Python 3.13.14、PySide6 6.12.0、adb 37.0.1；平板 Honor MagicPad 3（CGA-W00，Android 17 / SDK 37，3200×2136，420 dpi）。
> 记录原则：每条都写命令、期望、实际、结论；无硬件时明确标"待复验"，不得默认为通过。

## 1. 单元/集成（无硬件）

| 项 | 命令 | 期望 | 状态 |
|---|---|---|---|
| 配置 | `pytest tests/test_config.py -q` | 全绿 | ✅ |
| 估计器 | `pytest tests/test_estimator.py -q` | 全绿 | ✅ |
| 协议黄金字节 | `pytest tests/test_protocol.py -q` | 全绿 | ✅ |
| 几何 8 布局 | `pytest tests/test_geometry.py -q` | 全绿 | ✅ |
| 状态机 | `pytest tests/test_router.py -q` | 全绿 | ✅ |
| 键盘映射 | `pytest tests/test_keymap.py -q` | 全绿 | ✅ |
| UI/画布 smoke | `QT_QPA_PLATFORM=offscreen pytest tests/test_ui_smoke.py tests/test_canvas.py -q` | 全绿 | ✅ |
| 设备事实/旋转 | `pytest tests/test_session_facts.py -q` | 全绿 | ✅ |
| 全量 | `pytest -q` | 全绿 | ✅ 292 passed, 3 skipped |
| 键盘钩子安装 | `pytest tests/test_keyboard_hook.py -q` | 安装/卸载/幂等 | ✅ |
| 剪贴板注入 | `pytest tests/test_clipboard.py -q` | 4KB 截断/开关/空串 | ✅ |
| 风格 | `ruff check .` | 无告警 | ✅ |

## 2. 事实表（S0，设计 3.1）

| 事实 | 值 | 状态 |
|---|---|---|
| 型号 | CGA-W00 | 实测 |
| Android | 17 / SDK 37 | 实测 |
| 分辨率 | 3200×2136（横）/ 2136×3200（竖） | 实测 |
| dpi | 420 | 实测 |
| pointer_speed | -3 | 实测 |
| XScale / YScale | 1.312 / 1.312 | 实测 |
| UHID 可写 | 是（无需 root） | 实测 |
| H1 无加速残量 | 待重新标定 | 设计实测 |
| H2 px/count ≈ XScale | 待重新标定 | 设计实测 |
| H3 边缘手势不抢鼠标 | 待复验 | 设计实测 |

## 3. 真机连接链路（2026-11-02 实测）

设备以无线 ADB 连接：`adb-AM4NBB5C23001691-xn14x2._adb-tls-connect._tcp`。

| 项 | 命令/操作 | 实际 | 结论 |
|---|---|---|---|
| 设备事实 | `tools/probe.py --json` | CGA-W00，3200×2136（orientation=1），420dpi，XScale 1.312，pointer_speed -3，uhid OPEN_OK | ✅ |
| 握手 | `python -m padlend --headless --duration 5` | 读 1 字节 `00` 成功（scrcpy 5.0.1 实测；设计原记 4 字节） | ✅ |
| UHID | `dumpsys input \| grep PadLend` | `PadLend Mouse` / `PadLend Keyboard` 均注册 | ✅ |
| 指针移动 | `pointer_location=1` + 发送相对位移后截图 | 叠加层出现 `P:1/1`、`X/Y` 变化，屏幕出现系统指针 | ✅ |
| 退出清理 | headless Ctrl+C/到时退出 | `pointer_speed` 还原 -3、server 进程清空、`dumpsys input \| grep -c scrcpy` = 0 | ✅ |
| jar 重打包 | `tools/repack_server_jar.py` | 官方 jar 的 classes.dex 为 DEFLATE，Android 17 加载失败；重打包为 stored 后启动成功 | ✅ |

> 已知偏差：`cleanup=true` 时 CleanUp 启动即删除 CLASSPATH 首项 jar，重连会重新 push；`server.py` 的清理模式改为 `com.genymobile.scrcpy.Server` / `CleanUp`（旧 jar 名匹配不到当前进程）。

## 4. 真机 A1–A13

| 项 | 场景 | 期望 | 结果 |
|---|---|---|---|
| A1 | 单屏右侧来回 10 次 | 无卡顿、位置正确 | 待复验 |
| A2 | 出口位置精度 | 误差 ≤ 3px | 待复验 |
| A3 | 回程落点 | 进入点 ±2px | 待复验 |
| A4 | 混合 DPI 双屏 | 物理坐标正确 | 待双屏环境 |
| A5 | 负坐标布局 | 边段/入口正确 | 待双屏环境 |
| A6 | 热插拔显示器 | 自动重锚不崩 | 待双屏环境 |
| A7 | 键盘组合键/长按 | 6KRO、无粘键 | 待复验 |
| A8 | 中文输入法 | 可输入中文 | 待复验 |
| A9 | 边缘手势冲突 | 不误触发系统手势 | 待复验 |
| A10 | 拔线/重连 | 5 次退避、FAULT 交还、恢复 | 待复验 |
| A11 | 强杀进程 | 无 ClipCursor/幽灵 UHID 残留 | 待复验 |
| A12 | 性能 | raw_hz≈1000、p95<8ms、drop<0.5% | 待复验 |
| A13 | 连续 2h | 无泄漏、无漂移 | 待复验 |

## 5. 复验步骤（接上平板后）

```powershell
adb devices -l
.\.venv\Scripts\python.exe tools\probe.py
.\.venv\Scripts\python.exe -m pytest -q
.\.venv\Scripts\python.exe -m padlend
```

1. 按 A1 做 10 次来回，记录托盘状态与日志；
2. 运行 `tools/calibrate.py --send 1000`，用 `pointer_location` 叠加层核对 Δ；
3. 拔线，确认 5 次重连与 FAULT 行为；
4. 任务管理器强杀 `PadLend.exe`，重新运行，确认启动自清理（`dumpsys input | grep -c scrcpy` == 0，`ClipCursor` 已释放）；
5. 连续 2h 后看 `%APPDATA%\PadLend\logs\padlend.log` 是否有 `drop_pct` 上升/异常。

## 6. 打包（2026-11-02 实测）

| 项 | 命令 | 实际 | 结论 |
|---|---|---|---|
| 安装 PyInstaller | `python -m pip install pyinstaller` | PyInstaller 6.22.3 | ✅ |
| onedir 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | Build complete（约 44s） | ✅ |
| 版本冒烟 | `dist\PadLend\PadLend.exe --version` | `PadLend 0.1.0` | ✅ |
| 无设备 headless | `dist\PadLend\PadLend.exe --headless --duration 1 --no-single-instance` | exit 0；E101 后 `fault -> idle (stop)`，无残留 | ✅ |
| vendor 完整性 | `dist\PadLend\_internal\vendor\` | jar 2,316,791 B + SHA256SUMS + README | ✅ |

产物在 `dist/PadLend/`；`build/`、`dist/` 均已 gitignore。
