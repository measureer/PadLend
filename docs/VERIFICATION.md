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
| headless 1:1 | `python -m padlend --headless`（S1-6） | 已实现：连接后 Raw Input → CursorEstimator → UHID；无设备定时退出冒烟 exit 0 | ⏳ 待设备重连后实机复验 |
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

## 6. 界面与设备管理（0.2.0 / 0.2.1）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 新主窗 | 原生窗口截图（`docs/screenshots/ui-devices.png`） | 84px 导航、64px 顶栏、设备页双卡布局、中文正常 | ✅ |
| 未连接门控 | `test_device_page_hides_resolution_until_facts`、`test_main_window_device_header_has_no_default_resolution` | 未连接时分辨率/方向均为“—”，界面无 3200/2136 字样 | ✅ |
| 连接后事实 | 同一测试注入 `DeviceFacts(3200×2136)` | 显示 `3200 × 2136`、横屏、画布切到已连接 | ✅ |
| 设备管理 Router | `test_connect_device_switches_serial_and_clears_old_facts`、`test_disconnect_device_suspends_auto_reconnect_until_user_connects` | 切换 serial、断开后 supervisor 不自动重连 | ✅ |
| 主题切换 | `test_theme_toggle_persists_and_repaints`、`test_theme_repair_and_roundtrip` | 切换后 `tokens.current_theme()` 与 `config.ui.theme` 同步、重新加载仍生效 | ✅ |
| 亮色原生渲染 | 原生窗口截图 `docs/screenshots/ui-light.png` | 浅色窗口/卡片/导航/设备页，中文正常 | ✅ |
| 设置项说明 | `test_settings_hints_and_entry_guard` | 每个摆位/手感/键盘/快捷键/杂项控件均有中文说明；进入保护 220px 可保存 | ✅ |
| 打包 GUI | `dist\PadLend.exe` 原生启动截图 | 无设备时顶栏/设备页显示中性“未连接”，不误报故障 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 328 passed, 3 skipped | ✅ |

真机点击“连接此设备” → 画布出现真实尺寸 → 边缘穿越的交互链路，待平板重新连接后按 A1–A13 复验。

## 7. 打包（2026-11-02 实测）

| 项 | 命令 | 实际 | 结论 |
|---|---|---|---|
| 安装 PyInstaller | `python -m pip install pyinstaller` | PyInstaller 6.22.3 | ✅ |
| onefile 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | Build complete（约 56s） | ✅ |
| 产物 | `dist\PadLend.exe` | 单文件约 51MB，含 PySide6/jar/adb/图标 | ✅ |
| 版本冒烟 | `dist\PadLend.exe --version` | `PadLend 0.2.1` | ✅ |
| 亮色打包启动 | `PADLEND_HOME=<临时目录> PadLend.exe --no-wizard`（config `ui.theme: light`） | 日志 `theme switched name=light`，窗口正常创建 | ✅ |
| 内置 adb | 去掉 PATH 后 `PadLend.exe --headless --duration 1` | exit 0；E101（说明用的是内置 adb，而非 PATH） | ✅ |
| GUI 启动 | 双击 `dist\PadLend.exe` | 主窗/画布/设置/日志正常，标题 PadLend | ✅ |
| 便携目录 | 运行后检查 `dist\.padlend\` | `config.yaml`、`logs\padlend.log`、`exit.clean` 持久化 | ✅ |
| vendor 内置 | 解包/运行检查 | scrcpy jar（2,316,791 B）+ SHA256SUMS + `adb/platform-tools/` | ✅ |

产物为 `dist/PadLend.exe`（单文件）；`build/`、`dist/` 均已 gitignore。
本机 `%TEMP%` 对进程不可写，spec 用 `runtime_tmpdir="."` 将解包目录放在 exe 同目录。

## 8. 0.2.2 修复验收（2026-11-03）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 设备列表去重 | 真实 `adb devices -l`（两条同一平板，其中一条 mDNS 畸形行）→ `Adb().devices()` | 只返回 1 条稳定 serial；畸形行被丢弃 | ✅ |
| 绝对光标外推 | `test_arm_uses_absolute_cursor_from_real_desktop_map`（真实 DesktopMap + 底边外推 40px） | 进入 CAPTURED，mask active | ✅ |
| 查询失败不漂移 | `test_cursor_query_failure_does_not_accumulate_raw_counts` | `_pc_cursor` 保持绝对位置 | ✅ |
| 连接状态一致性 | 源码 GUI 实机启动截图（自动连接） | 顶栏“已连接”、设备页“已连接”，分辨率仅在 facts 到达后显示 | ✅ |
| 进入边提示 | 同一截图底栏 | “横屏 3200×2136 · 从屏幕右侧边缘滑入平板” | ✅ |
| 键盘看门狗 | 运行 >1min 并只移动鼠标 | 不再每 30s 出现 `kbd.watchdog.stalled`/重装 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 328 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |

真实“把鼠标推过屏幕边缘 → 平板指针接管 → 划回交还”仍需用户在 0.2.2 exe 上完成一次目视确认。

## 9. 0.3.4 系统光标隐藏修复（2026-11-03）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 不使用 ShowCursor | 单测 `test_set_cursor_hidden_never_touches_show_cursor` | 只调用 `SetCursor(None)` / `LoadCursorW(IDC_ARROW)` / `SetCursor(箭头)` | ✅ |
| 跨线程释放 | 单测 `test_cross_thread_release_posts_close_instead_of_destroy` | GUI 线程 release 时对 rawinput 线程创建的窗口只发 `WM_CLOSE` | ✅ |
| 真实遮罩显示/恢复 | `_diag_mask.py`：`CursorMask.show(None)` → 泵消息 → `GetCursorInfo.flags`；`hide()` 后复测 | before 1 → masked 0 → after 1 | ✅ |
| 跨线程真实释放 | 工作线程 show + 消息泵，主线程 release；`GetCursorInfo.flags` | masked 0 → after release 1，工作线程正常退出 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 328 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.4 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 52,715,929 B，sha256 `78c6d4c84cb0873b3ea711e0888645301c4d31e74111d6a2012a6187d64f436c`，`--version` 0.3.4 | ✅ |
| 0.3.4 出口修复后重建（2026-11-04） | `pytest -o addopts="" -q` → `ruff check .` → `python -m PyInstaller --noconfirm PadLend.spec` | 334 passed, 3 skipped；ruff 全绿；`dist/PadLend.exe` 53,242,550 B，sha256 `8cbd78fc6068fe38e7e4ddd48b1b82f034d043e8cd4be6cb246a45cd9cff04d8`；`--version` → `PadLend 0.3.4`；`--headless --duration 1` exit 0（无设备 → E101，config/logs/`exit.clean` 正常生成） | ✅ |

仍需用户复验：正常接管/返回后 Windows 光标可见；异常退出（关进程）后光标也能恢复。

## 10. 0.3.5 设备添加与退出清理（2026-11-04）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 无线地址容错 | 单测 `test_normalize_address_*`（`adb connect 1.2.3.4:5555 already connected`、缺端口、非法端口、mDNS 名…） | 归一化为 `host:port`，非法返回空串 | ✅ |
| 连接/配对失败文案 | 单测 + 真机失败冒烟（`127.0.0.1:1`） | 设备页错误条显示 `E101 … cannot connect to 127.0.0.1:1 … (10061)`，附带排查提示 | ✅ |
| 添加设备对话框 | GUI 实测（UIA） | 标题「添加无线设备」；地址/配对码校验；配对码非空时按钮变「配对并连接」 | ✅ |
| 空状态可操作 | GUI 实测 | “未检测到设备”下方分两行给出 USB / 无线（右上角「添加设备」）两条路径 | ✅ |
| 长错误不挤压按钮 | `build/check_error_strip.py`（离屏）+ 真实窗口 | 1400 宽下 `strip w=1344 h=42`、`add_button_visible=True`；错误条横跨页面自动折行 | ✅ |
| 顶栏与标题栏去重 | GUI 实测（UIA + 截图） | 顶栏只剩 ☰/⚙/☀（托盘可用时隐藏 ☰），系统标题栏保留 ─ □ ×；窗口图标换品牌 P | ✅ |
| 模态对话框下关闭 | 打开「添加设备」后给主窗发 `WM_CLOSE` | 原生事件过滤器接管 → 先 `reject()` 模态框再退出，`alive=False` | ✅ |
| 退出后不留 PadLend | 打包版 GUI 关闭 → `tasklist` | PadLend.exe 计数归零，日志 `adb server 已停止` + `已退出` | ✅ |
| adb server 清理 | 隔离端口 `ADB_SERVER_SOCKET=tcp:127.0.0.1:5055` 跑 headless | 5055 上无残留 server（5037 上被外部程序重新拉起，与本程序无关） | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 355 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.5 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,256,401 B，sha256 `d092782ce1f573c86cab3b3723821a9b0188f70ac8bded3e6188a7beb3a41d00`，`--version` → `PadLend 0.3.5` | ✅ |
| 打包 headless | `PADLEND_HOME=…build/home-pkg --headless --duration 2 --no-wizard` | exit 0；`adb server 已停止`；config/logs/`exit.clean` 正常生成 | ✅ |
| 打包 GUI | `PADLEND_HOME=…build/home-pkg2`，UIA 截图 + `WM_CLOSE` | v0.3.5 界面正常；关闭后进程归零 | ✅ |

仍需真机确认：「添加设备」对真实平板（无线调试已开）能列出并连接；配对码首次配对流程。
