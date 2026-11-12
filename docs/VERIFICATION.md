# 验收记录

> 环境：Windows 11、Python 3.13.14、Node.js 22、adb 37.0.1；平板 Honor MagicPad 3（CGA-W00，Android 17 / SDK 37，3200×2136，420 dpi）。
> 1.1.0 起界面不再使用 PySide6；第 0 节及以下历史记录中的 Qt 相关内容仅作回归对照。
> 记录原则：每条都写命令、期望、实际、结论；无硬件时明确标"待复验"，不得默认为通过。

## 1.2.0 发布验收（2026-11-13）

自动化门禁在本机 Windows 11 + bundled Python 3.12.14 / Node 24.21.0 上执行：

| 项 | 命令 | 结果 |
|---|---|---|
| Python 回归 | `python -m pytest -o addopts="" -q` | **421 passed, 3 skipped**（基线 HEAD `07218c6` 为 403 passed, 3 skipped，新增 18 例） |
| 代码风格 | `python -m ruff check .` | All checks passed（F401/F811/F841 为 0） |
| 前端单测 | `pnpm exec vitest run` | 7 files / 29 tests passed（基线 28，新增托盘退出 1 例） |
| 类型检查 | `pnpm exec tsc --noEmit` | 通过（`noUnusedLocals`/`noUnusedParameters` 已开启） |
| 原生外壳编译 | GitHub Actions `windows-latest` | 本机无 Rust/MSVC，必须由 CI 的 `npx tauri build --no-bundle` 与 Release 工作流验证；本地结果不代签 |
| 托盘退出实测 | 启动 → 托盘「退出 PadLend」 | **待复验**：本轮桌面会话被系统锁定，无法点托盘；代码路径以 `tests/quit.test.ts` 与 Rust 单测面代替，发布前需在解锁桌面点一次并确认进程树/托盘/遮罩全清 |
| Win 键实测 | 接管中按 L/R Win | **部分复验**：Router 单测覆盖 down/up/scan code/交还后迟到 keyup；真实外壳的「开始菜单不再盖在遮罩下」需解锁桌面手工确认 |
| 崩溃栈 | 事件查看器 / WER / `logs\crash-*` | **未取得**：无 PadLend 崩溃记录，缺陷 A 的崩溃本身未复现；按 `docs/PITFALLS.md` 4.1 保留证据收集步骤 |

> 结论：自动化门禁全绿；两项 GUI 实测因会话锁定标为待复验，不能据此宣称「已手工验收」。
> 发布说明与 CHANGELOG 已同步写入该限制。

## 1.1.0 发布验收（2026-11-12）

| 项 | 命令 | 结果 |
|---|---|---|
| Python 引擎 + 服务协议 | `pytest -q` | 全部通过（含 `tests/test_service_bridge.py` 真实子进程 + 假平板） |
| 代码风格 | `ruff check .` | 全通过 |
| 前端单元测试与交互冒烟 | `npx vitest run` | 28 passed |
| 类型检查 / 构建 | `npm run build`（含 `tsc --noEmit`） | 通过，输出 `frontend-dist/` |
| 业务服务联调 | `python -m service --simulate --no-hooks` | bootstrap / 扫描 / 布局保存 / 诊断导出 / 引擎事件正常 |
| 视觉 | 无头 Edge 对 `frontend-dist` 截图 | 设备/布局/设置/日志、亮色主题、900×700 窄窗口均无重叠/裁切 |
| 原生外壳编译 | GitHub Actions `windows-latest`（Rust stable + MSVC） | 由 CI 的 `npx tauri build --no-bundle` 与 Release 工作流验证；本机无 Rust/MSVC |

> 本机没有 Rust/Cargo 与 MSVC 构建工具，Tauri 原生 exe 与 NSIS 安装器由
> GitHub Actions 在 `windows-latest` 上产出后发布；本地只产出前端静态资源、
> Python 业务 sidecar 与免安装资源包。

## 0. 1.0.0 发布验收（2026-11-05）

> 本节记录 1.0.0 首发验收。1.0.1 是设备地址发现相关的补丁版本，自动化验收以 CI 的
> `ruff` 与 `pytest` 为准。第 1–23 节是 0.3.x 期间的历史记录，其中的 onefile 体积、
> `PadLend.exe --version`、`ui.tray_hint_shown`、收进托盘气泡等已不适用于 1.0.0，仅作回归对照。

构建命令（标准库脚本，封装 onedir / zip / 可选安装器 / 签名 / 校验和）：

```powershell
.\.venv\Scripts\python.exe tools\release.py --build --zip --sha256
```

| 项 | 命令 / 检查 | 期望 | 状态 |
|---|---|---|---|
| 产物结构 | `ls dist\PadLend\` | `PadLend.exe` + `_internal\` + `LICENSES\` + `THIRD_PARTY_NOTICES.md` | ✅ 129 MB 目录 |
| 发布 zip | `dist\PadLend-1.0.0-win64.zip` | 约 52 MB，解压即可运行 | ✅ |
| 版本资源 | PowerShell `(Get-Item dist\PadLend\PadLend.exe).VersionInfo` | FileVersion `1.0.0.0`、ProductVersion `1.0.0`、FileDescription/CompanyName 非空 | ✅ |
| 版本单一源 | `python -c "import padlend, constants; print(padlend.__version__, constants.VERSION)"` | 两处均为 `1.0.0`；`pyproject.toml` 动态读取 | 待执行（代码核对通过） |
| 任意 CWD 启动 | `cd $env:TEMP; & 'C:\...\dist\PadLend\PadLend.exe'` | 窗口出现；配置/日志写到 `%APPDATA%\PadLend`，**不写 exe 目录** | 待执行（GUI） |
| 状态栏/关于版本 | 主窗底栏、托盘「关于 PadLend…」 | 均显示 `v1.0.0`；向导底部同样 | 待执行（GUI） |
| SHA256SUMS | `Get-FileHash .\dist\PadLend-1.0.0-win64.zip -Algorithm SHA256` | 与 `dist\SHA256SUMS.txt` 对应行的 64 位十六进制一致 | 待执行 |
| 崩溃日志 | 临时 `PADLEND_HOME`；`python -c "from core.crash import install_crash_handlers, report_crash; ..."` 后触发 `report_crash` | 生成 `logs\crash-<时间>-<pid>.log`，含版本、argv、traceback、线程栈 | 待执行 |
| 原生崩溃栈 | 同上，检查 `faulthandler-*.log` | 存在且非空 | 待执行 |
| 配置损坏恢复 | 临时 `PADLEND_HOME` 写坏 YAML 后启动 | 原文件备份为 `config.yaml.bad-<时间戳>`，加载默认值，日志 `cfg.backup` | 待执行 |
| 损坏可重复 | 连续两次写坏并启动 | 生成两个不同时间戳的 `.bad-` 文件，不互相覆盖 | 待执行 |
| 日志轮转 | 运行至 `padlend.log` > 2 MB，或复制大日志后重启 | `padlend.log.1`…`.3`，单文件 ≤ 2 MB，不无限增长 | 待执行 |
| 诊断脱敏 | 顶栏右侧菜单（⋯）→「运行诊断…」→ 导出 zip，解压检查 | `diagnostics.txt` 无完整序列号/IP/MAC/用户名；`meta.json` 的 `redacted: true` | 待执行（GUI） |
| About 许可 | 托盘/顶栏菜单（⋯）→「关于 PadLend…」 | 显示 1.0.0、Apache-2.0、PySide6 LGPLv3 说明；「第三方许可…」能打开 `THIRD_PARTY_NOTICES.md`；「打开日志目录…」有效 | 待执行（GUI） |
| 安装器（可选） | `tools\release.py --installer` 后运行 `PadLend-1.0.0-setup.exe` | 每用户安装到 `%LOCALAPPDATA%\Programs\PadLend`，无需 UAC；卸载保留 `%APPDATA%\PadLend` | 待执行（需 Inno Setup 6.3+） |
| 内置 adb | 临时清空 PATH 后启动 headless | 仍能执行 `adb devices`；日志 adb 路径指向 `_internal\vendor\adb\platform-tools\adb.exe` | 待执行 |
| vendor 校验 | 临时改动 jar 的一个字节后连接设备 | 推送前抛 E103「与 vendor/SHA256SUMS 不一致」，不启动 server；恢复后可连 | 待执行（破坏性，需副本） |
| SmartScreen | 在未加白名单的机器运行 zip 内 exe | 出现「未知发布者」；「更多信息 → 仍要运行」后可启动 | 待执行（干净机） |
| 截图 | `python tools\make_screenshots.py` | 4 张 1280×820，设备页/布局/设置/浅色；无缺字、无旧控件 | ✅ 已生成 |

上表是 1.0.0 首发时的记录，部分 GUI / 干净机 / 破坏性项后续没有逐条回填；1.0.2 及之后的自动化门禁如下。

**1.0.5 本地验收（2026-11-11）**：`ruff check .` 通过；`pytest -o addopts="" -q`
为 `481 passed, 3 skipped`。本版把控制通道改成双向：新增 device message 拆包
（剪贴板 / ACK / UHID 输出）与常驻接收线程；平板 → PC 剪贴板只在
`misc.clipboard_sync` 打开时回传，回写后 1 秒内只吞完全相同的回显。
产物已本地构建，尚未发布。

**1.0.4 发布验收（2026-11-07）**：`ruff check .` 通过；`pytest -o addopts="" -q`
为 `475 passed, 3 skipped`。本次只调整触发阈值：滑条 1–200 px、默认 10 px；
慢推兜底按 `arm_px` 缩放，避免默认值降低后兜底失效。

**1.0.3 发布验收（2026-11-06）**：`ruff check .` 通过；`pytest -o addopts="" -q`
为 `474 passed, 3 skipped`。本版改动包括按设备与横/竖屏保存布局、常驻 adb shell
监听真实朝向并自动切换、移除双击伪造横竖屏；详见 `docs/CHANGELOG.md`。

**1.0.2 本地验收（2026-11-06，随 1.0.3 发布）**：`ruff check .` 通过；`pytest -o addopts="" -q` 为
`452 passed, 3 skipped`。本版改动包括无线调试端口 mDNS 发现、第二实例唤起已有窗口、
布局吸附边改由画布决定、布局页「保存布局」、按实际接触段判定边缘穿越；详见 `docs/CHANGELOG.md`。

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

## 7. 打包（0.2.x onefile 历史，2026-11-02 实测）

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

## 11. 0.3.6 无硬件验收与点击期光标取证（2026-11-04）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 假平板可跑全链路 | `main.py --simulate`（源码实例）+ 截图 | 设备页列出 `Simulated Tablet`（`sim:5555 无线 · device`），连接后顶栏/详情显示 1707×1067 竖屏 | ✅ |
| 端到端验收工具 | `python tools/acceptance.py` | 24 项、失败 0、跳过 3（本机光标不可观测） | ✅ |
| 遮盖机制自证 | 验收项 | 接管后 `len(mask._windows)=2`，对每个遮罩窗发 `WM_SETCURSOR` 窗口过程均返回 1 | ✅ |
| 光标锁自证 | 验收项 | `GetClipCursor=(1706,0,1707,1067)`，即主屏右边缘 1px 条，非整屏 | ✅ |
| 点击不踢出接管 | 验收项（单击/连点 3/双击） | 三种点击后仍 `state=captured`、`transitions=[]`，遮罩与裁剪区保持 | ✅ |
| 点击送达平板 | 验收项 | 单击 `clicks=1`、连点 `clicks=3`、双击 `clicks=2`；报告里同时出现按下与抬起 | ✅ |
| 拖动持续上报 | 验收项（按下 + 8 段 dx=6,dy=4 + 抬起） | `held_reports=171`、`dx=48 dy=32`、仍 CAPTURED | ✅ |
| 交还清理 | 验收项 | `state=idle`、`windows=0`、`GetClipCursor` 恢复整桌面 `(-1707,-132,1706,1067)` | ✅ |
| 点击期现场日志 | 单测 `test_press_in_captured_logs_cursor_diagnose` | 按下/抬起各写一条 `cap.cursor visible=… clip=… windows=…` | ✅ |
| 设备行不再截断 | `--simulate` 截图 | 两行：「Simulated Tablet」+「sim:5555 无线 · device」，序列号可见 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 361 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.6 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,264,524 B，sha256 `b306f5b4b9d2691e86201040080553e29b275cc85d757435657ef7463f5a7acf`，`--version` → `PadLend 0.3.6` | ✅ |
| 打包 `--simulate` 无头 | `PADLEND_HOME=…build/home-pkg6 --simulate --headless --duration 2` | exit 0；`[device] sim:5555 1707x1067 orientation=0 xscale=2.0` | ✅ |
| 打包 `--simulate` GUI | `PADLEND_HOME=…build/home-pkg7 --simulate`，UIA 截图 | 设备页两行行：`Simulated Tablet` / `sim:5555 无线 · device`，底部 `v0.3.6`；`WM_CLOSE` 后 `alive=False`、PadLend.exe 计数 0 | ✅ |

环境限制（影响结论口径）：本机为远程会话，`SendInput`/`SetCursorPos` 都不改变真实光标，
`SetCursor(None)` 后 `GetCursorInfo` 仍报 `CURSOR_SHOWING`。因此三项「光标可见性」断言只能 SKIP，
其余断言全部真实执行 —— 遮罩、`ClipCursor`、状态机、平板端收包都不是 mock。

仍需真机确认：用户报的「穿透时点一下，鼠标可能同时出现在平板和电脑上」需要在真机上复现一次；
0.3.6 起接管期每次按下/抬起都会写 `cap.cursor`，用日志区分「遮罩被拆 / 锁被系统收回 / 外部程序
把光标显示回来」三种成因。

## 12. 0.3.7 RTT 与穿屏误触（2026-11-04）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 延迟样本真的在记 | 单测 `test_sent_report_records_latency_sample` | 入队→写出成功 3ms，`rtt_ms=3.0`、`p95_ms=3.0` | ✅ |
| 空闲不报 0 | 单测 `test_metrics_reset_clears_windows` | 无样本时 `rtt_ms/p50_ms/p95_ms` 键不存在 | ✅ |
| 界面显示「RTT —」 | `--simulate` GUI 截图 | 顶栏右侧显示 `RTT —`（不再是 `RTT 0ms`） | ✅ |
| 验收工具里的 RTT | `tools/acceptance.py` | 「指标里有真实 RTT 样本」→ `rtt_ms=2.0` | ✅ |
| 交还时补全松报告 | 单测 `test_return_sends_mouse_release_report` / `test_abort_capture_sends_mouse_release_report` | `session.mouse_reports` 里出现 `(0,0,0,0)` | ✅ |
| 按住离开不残留拖拽 | 验收项 | 按住离开后 0 条带按下位的位移报告 | ✅ |
| 重新进入无残留按下 | 验收项 | 重新穿入后前 3 条报告 `buttons=0x0` | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 364 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.7 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 见下 | ✅ |

仍需真机确认：RTT 数值是否符合实际手感（设备端没有 ack，量的是「输入进队 → HID 写入成功」），
以及「穿屏瞬间点一下平板按钮」在真机上是否消失。

## 13. 0.3.8 穿屏误触诊断与入口全松（2026-11-04）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 进入时先补全松 | 单测 `test_entry_releases_stale_buttons_before_warp` | 排空后第一条报告是 `(0,0,0,0)`，其后的 warp 全为 0 按键 | ✅ |
| 按键日志只在变化时打 | 单测 `test_send_mouse_logs_only_on_button_change` | `0x00 → 0x01 → 0x00` 三条，相同状态不重复 | ✅ |
| 滚轮标志位不落进 HID 报告 | 单测 `test_send_mouse_masks_buttons_to_five_bits` | `mouse_report(0x0400, …)[0] == 0` | ✅ |
| 验收覆盖入口全松 | `tools/acceptance.py` | 「进入时先补一条全松」PASS，30 项 0 失败 3 跳过 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 367 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.8 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 见下 | ✅ |

仍需真机确认：穿屏误触是否消失。若仍出现，日志里 `tablet.mouse.btn`（真的发出了按下）、
`cursorwatch.btn`（Raw Input 停摆后由 `GetAsyncKeyState` 补的按下）、`cap.btn` 三条能定位来源。

## 14. 0.3.9 入口贴边对齐（2026-11-04）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 入口补位覆盖整轴 | 单测 `test_entry_pins_the_tablet_pointer_onto_the_entry_edge` | 全部报告 `dy=0`、`buttons=0`，累计位移 ≥ 平板宽度，`estimator.x == 0.0` | ✅ |
| 入口补位走输入队列 | 同批 `test_manual_capture_shows_one_pixel_clip_and_warps` 等 60 例 | 捕获时仍零写入，`_sender_frame()` 才 flush | ✅ |
| 验收覆盖贴边对齐 | `tools/acceptance.py` | 「进入时把指针顶到入口边」`sweep=-1520 need=768 on_edge=True`，31 项 0 失败 3 跳过 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 368 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.9 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 见下 | ✅ |

仍需真机确认：指针是否在到达平板边沿之后才穿回电脑（入口那一下可能看到指针被吸到边上，
这是镜面与真实位置对齐的过程）。

## 15. 0.3.10 接管不再自动发剪贴板（2026-11-05）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 进入接管不发文本 | 验收「进入接管不会向平板打字」 | 假平板 `texts=[] keys=[]`，32 项 0 失败 3 跳过 | ✅ |
| GUI 层不再触发 | 单测 `test_entering_capture_does_not_push_the_clipboard` | `IDLE → CAPTURED` 后 `clipboardSyncRequested` 未发出 | ✅ |
| 热键路径仍在 | `main.py:461` `HotkeyManager(..., window.clipboardSyncRequested.emit)` | `Ctrl+Alt+V` 直接走同一信号 → `sync_clipboard()` | ✅ |
| 设计文档同步 | `docs/DESIGN.md` 6.8 | 触发改为「仅热键」，并写明 `INJECT_TEXT` 是真实按键 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 369 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 0.3.10 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,267,515 B，sha256 `744f380e50cee18a13c0997b44fa696b50d0f1fff813a5c9d9bf99e30aa7ab06`，`--version` → `PadLend 0.3.10` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate`，`ps-close-check.ps1 -Hwnd 9112762 -ProcId 80652` | 顶栏 `RTT —`、底部 `v0.3.10`；`WM_CLOSE` 后进程归零，日志 `已退出`，全程 0 条 `clipboard_sent` | ✅ |

仍需真机确认：进接管时平板是否还会闪一下；若仍出现，看日志里是否还有
`router.clipboard_sent`（有 → 热键或别的路径在发），没有则说明来源不是我们。

## 16. 0.3.11 图标统一 / 启动居中 / 设备记忆（2026-11-05）

| 项 | 验证方式 | 实际 | 结论 |
|---|---|---|---|
| 图标同一份画法 | `tools/make_icon.py`（offscreen 渲染 `brand_pixmap`；16…128 写 DIB，256 写 PNG） | `assets/padlend.ico` 172,532 B，10 档尺寸 16…256 | ✅ |
| 图标真的进了 exe | 逐条在 `dist/PadLend.exe` 里搜 ico 的 10 段 payload | 10/10 `in_exe=True`（含 DIB 原始像素） | ✅ |
| exe 图标已是新的 | `shell32.SHGetFileInfo`（`build/ps-shellicon.ps1`）对原路径与新拷贝各取一次大图标 | `build/shellicon-dist.png` 与 `shellicon-copy.png` sha256 相同，均为品牌 P | ✅ |
| ico 结构校验 | 单测 `test_packaged_icon_matches_the_runtime_brand` | `count == 10`，每项偏移可读、数据以 `\x89PNG` 开头 | ✅ |
| 启动居中 | `ps-rect.ps1 -Hwnd 5641964` | 窗口 `(207,85)-(1502,942)`，中心 `(854,514)`，可用区中心 `(854,513)`（屏幕 1707×1067，任务栏 40） | ✅ |
| 居中单测 | `test_center_on_screen_puts_the_window_in_the_middle` | 窗口中心与 `availableGeometry().center()` 误差 ≤ 2px | ✅ |
| 设备记忆存盘 | `test_known_devices_roundtrip` / `test_known_devices_default_empty_and_coerced` | `device.known` 顺序往返、缺省 `[]`、`[7, XYZ]` → `["7","XYZ"]` | ✅ |
| 设备页列记忆行 | `test_device_page_lists_remembered_devices` | 扫不到时列出 2 行「上次连接的设备」，扫到后不重复，提示 `1 台设备` | ✅ |
| 记住上次设备 | `test_window_remembers_the_last_device` | 新的在前、去重、≤8 条，`device.serial` 同步 | ✅ |
| 记忆行实机渲染 | 打包版 GUI（`build/home-0311-gui/config.yaml` 预置 `serial` + `known`） | 列表出现「192.0.2.10:37495 上次连接的设备 · 无线」「ABC123USB · USB」，右卡可点「连接此设备」，右上「2 台已记住」 | ✅ |
| 无线重连补 adb connect | `_connect_worker` 对含 `:` 的 serial 先 `adb.connect` | 失败仅 `log.info`，结论仍由 45s 超时给出 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 375 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 32 项，0 失败，3 跳过（本机光标不可观测） | ✅ |
| 0.3.11 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,170,677 B，sha256 `e0b8b0d2826e98bf525e90574ef1ba4add49102f7a16ba9b3e20b5d66b71cff4`，`--version` → `PadLend 0.3.11` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2` | exit 0，生成 `config.yaml`/`logs`/`exit.clean` | ✅ |
| 打包版 GUI | `ps-close-check.ps1 -Hwnd 5641964 -ProcId 64816` | 底部 `v0.3.11`，`WM_CLOSE` 后 `alive=False`，日志 `adb server 已停止` + `已退出` | ✅ |

仍需真机确认：记忆行点「连接此设备」能否把平板上线（平板需开着无线调试，
端口若变了要先在平板上看一眼当前端口；`adb connect` 失败时会等到 45s 超时）。

## 17. 0.3.12 关闭窗口 = 退出还是收进托盘（2026-11-05）

| 项 | 命令 / 检查 | 实际 | 结论 |
|---|---|---|---|
| 配置项默认值 | `ui.close_to_tray` 缺省 | `False`（保持 0.3.11 的“点 X 即退出”） | ✅ |
| 存盘往返 | `pytest tests/test_ui_smoke.py -k close_to_tray` | 勾选后 save 写入 `config.yaml`，重载仍为 `True` | ✅ |
| 设置页渲染 | 源码 GUI（预置 `ui.close_to_tray: true`）滚到「⚙ 杂项」 | 第三行「关闭窗口时最小化到托盘」带说明，已勾选 | ✅ |
| 勾选后点 X | 打包版 GUI（`close_to_tray: true`）+ `ps-close-check.ps1` | `padlend_alive=True`、`window_visible=False`，日志 `window hidden to tray` | ✅ |
| 不勾选点 X | 打包版 GUI（默认配置）+ `ps-close-check.ps1` | `alive=False`，PadLend.exe 归零 | ✅ |
| 托盘不可用时 | 单测 `test_request_close_quits_when_tray_is_missing` | 即使勾选也走真退出 | ✅ |
| 重复触发幂等 | 原生 `WM_CLOSE` 与 `closeEvent` 同时到达 | 只隐藏一次（`isHidden()` 守卫） | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 380 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 32 项，0 失败，3 跳过 | ✅ |
| 0.3.12 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,172,393 B，sha256 `3c026d6b21f92b989e211c83c77c20bd4d50e8831a9f8e589fcccc2ea0b231ea`，`--version` → `PadLend 0.3.12` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2` | exit 0 | ✅ |

仍需真机确认：勾选后在真机上点 X，托盘图标应仍在、双击可重新打开窗口；
从托盘菜单「退出 PadLend」应完全退出且 `adb server 已停止`。

## 18. 0.3.13 设备列表与自动找回（2026-11-05）

| 项 | 命令 / 检查 | 实际 | 结论 |
|---|---|---|---|
| 标题改名 | GUI / 单测 `test_device_page_is_called_a_device_list` | 卡片标题是「设备列表」，不再出现「可用设备」 | ✅ |
| 移除记忆设备 | 单测 `test_remembered_device_can_be_removed` | 选中记忆行后按钮可用；点一下从 `device.known` 删除，`device.serial` 改指下一条 | ✅ |
| 在线设备不可移除 | 单测 + `_on_selection` 判定 | 扫描到的设备行按钮保持禁用 | ✅ |
| 探测结论上屏 | 单测 `test_remembered_device_shows_the_probe_result` | 记忆行副标题显示「不在线」，tooltip 带原因 | ✅ |
| 原地仍在线 | `tests/test_locator.py::test_locate_reports_online_when_address_answers` | `STATUS_ONLINE`，detail「仍然在线」 | ✅ |
| 只换端口 | `::test_locate_follows_a_new_port_from_mdns` | `STATUS_MOVED`，地址换成 mDNS 广播的新端口 | ✅ |
| 只换 IP | `::test_locate_follows_a_new_ip_through_the_arp_table` | 按 MAC 在 ARP 表找到新 IP，再取 mDNS 端口 | ✅ |
| 找到 IP 没端口 | `::test_locate_reports_no_port_when_only_the_ip_is_found` | `STATUS_NO_PORT`，`reachable=True`、`online=False` | ✅ |
| 彻底找不到 | `::test_locate_reports_offline_when_nothing_answers` | `STATUS_OFFLINE`，detail「找不到这台平板」 | ✅ |
| ARP 解析 | `::test_arp_table_reads_windows_output` | GBK 输出也能解析出 MAC→IP | ✅ |
| 记住 Wi-Fi MAC | `tests/test_adb.py::test_wifi_mac_reads_wlan0` 等 3 例 | 读 `wlan0`、扫描接口兜底、非法输出返回空串 | ✅ |
| MAC 存盘 | 连接成功后 `_capture_mac` → `device.mac` | 只在值变化时写配置 | ✅ |
| 设置不吃滚轮 | 单测 `test_settings_widgets_do_not_react_to_the_wheel` | 4 个组合框 + 2 个滑条都 `event.ignore()` | ✅ |
| 设置说明重写 | GUI 设置页逐条目视 | 19 条都改成“是什么/什么时候调/例子” | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 398 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 32 项，0 失败，3 跳过 | ✅ |
| 0.3.13 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,185,332 B，sha256 `66055bdb2c09ccc2c77ade1003fb791c8cab55baaa15187c75bc2c7ee4bc2f27`，`--version` → `PadLend 0.3.13` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate` + `ps-close-check.ps1` | 「设备列表」标题、两行说明、「从列表移除」按钮、`v0.3.13`；`WM_CLOSE` 后 `alive=False`、日志 `已退出` | ✅ |

仍需真机确认：真机换 IP / 换端口后能否在列表里自动标成新地址；ARP 表里查不到时
（例如平板刚休眠）应显示「不在线」而不是卡住。

## 19. 0.3.14 剪贴板自动同步（2026-11-05）

| 项 | 命令 / 检查 | 实际 | 结论 |
|---|---|---|---|
| 服务端支持 type 9 | 解包 `vendor/scrcpy-server-v5.0.1.jar` 后 `grep -c` `classes.dex` | `SET_CLIPBOARD`=1、`setPrimaryClip`=1、`GET_CLIPBOARD`=1、`getPrimaryClip`=1 | ✅ |
| 报文逐字节 | `tests/test_protocol.py::test_set_clipboard_packs_sequence_paste_and_text` | `09` + `sequence(8)` + `paste(1)` + `len(4)` + utf8；`paste=False` 时该字节为 0 | ✅ |
| 不再敲键盘 | `tests/test_clipboard.py` 四例 | 只记录 `set_clipboard`，`inject_text` 从未被调用 | ✅ |
| 4KB 截断 | `::test_inject_clipboard_truncates_to_4kb` | 5000 个 `a` → 4096 字节 | ✅ |
| 进入接管自动同步 | `tests/test_ui_smoke.py::test_entering_capture_syncs_the_clipboard_when_enabled` | 勾选时 `_clipboard_timer` 启动，触发后 router 收到 `("hello tablet", False)` | ✅ |
| 未勾选不发 | `::test_entering_capture_does_not_queue_clipboard_when_switched_off` | 不排队、无 clipboard 调用 | ✅ |
| 复制触发同步 | `::test_clipboard_changes_only_sync_while_captured` | 仅 `captured` 状态排队；`idle` 不排队 | ✅ |
| 只写剪贴板 | `tools/acceptance.py` 第 33 项 | `clipboards=[("acceptance clipboard", False)]`、`texts=[]`、`keys=[]` | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 404 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 33 项，0 失败，3 跳过 | ✅ |
| 0.3.14 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | `dist/PadLend.exe` 53,189,288 B，sha256 `549d1f8c5fbe62e8abe3f6e67ea6449bab710f8ede36d089abca01bddbad584e`，`--version` → `PadLend 0.3.14` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2 --no-single-instance` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate` + UIA + `ps-close-check.ps1` | 设置页显示「剪贴板同步」勾选与新说明、快捷键条目写「立即粘贴」、底部 `v0.3.14`、RTT 2ms；`WM_CLOSE` 后进程退出 | ✅ |

仍需真机确认：接管时复制一段文字后，在平板输入框长按应能粘贴出同一段文字；
刷抖音时穿过屏幕不应再触发「不感兴趣」。

---

## 第 20 节：0.3.15 删除剪贴板快捷键（2026-11-05）

| 项 | 命令 / 检查 | 实际 | 结论 |
|---|---|---|---|
| 快捷键组只剩两行 | 打包版 `--simulate` → 设置页 UIA | 「⚡ 快捷键」组 = 接管/交还、急停；无「剪贴板」行 | ✅ |
| 设置面板无残留控件 | `tests/test_ui_smoke.py::test_settings_panel_has_no_clipboard_hotkey` | `hasattr(panel, "clipboard_edit")` 为假，`_field_widgets` 无 `hotkeys.clipboard` | ✅ |
| 配置字段移除 | `config.py` 的 `HotkeyCfg` / `_fill` / 默认补齐 / 校验 | 只剩 `toggle`、`panic`；旧 yaml 里的 `clipboard:` 被忽略 | ✅ |
| 热键注册只在两处 | `win/hotkeys.py` | `HotkeyManager(_toggle, _panic)`；无 `HOTKEY_CLIPBOARD_ID`、无 `VK_V` | ✅ |
| 剪贴板仍只写不敲键 | `tools/acceptance.py` 第 33 项 | `clipboards` 收到文本，`texts=[]`、`keys=[]` | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 404 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 33 项，0 失败，3 跳过 | ✅ |
| 0.3.15 构建 | `python -m PyInstaller --noconfirm --distpath build/dist-0315 --workpath build/pyi-0315 PadLend.spec` | 53,187,231 B，sha256 `511f84362138fab862437a3bc8239504118b0c7e51e6eefc79c8f7b00af18cf4`，`--version` → `PadLend 0.3.15` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2 --no-wizard --no-single-instance` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate` + UIA + `ps-close-check.ps1` | 设备页仍是「设备列表」；设置页快捷键组无剪贴板行；`v0.3.15`；`WM_CLOSE` 后日志 `已退出` | ✅ |

仍需真机确认：旧的 `config.yaml` 里若有 `hotkeys.clipboard`，保存一次后该行应消失且
不影响接管/急停热键。截图：`build/padlend-0.3.15-settings.png`。

## 第 21 节：0.3.16 上报频率定档 / 穿回阈值 120 / 托盘兜底（2026-11-05）

| 项 | 命令 / 检查 | 实际 | 结论 |
|---|---|---|---|
| 上报频率只有四档 | `tests/test_ui_smoke.py::test_report_hz_is_four_fixed_choices` | 下拉项 = `[125, 250, 500, 1000]`；选 250 保存后 `motion.report_hz == 250` | ✅ |
| 旧值自动收敛 | `tests/test_config.py::test_report_hz_snaps_to_a_fixed_choice` | `report_hz: 998` → 1000，并记一条 issue | ✅ |
| 穿回阈值默认 120 | `test_exit_threshold_default_is_120` | `C.EXIT_OVERFLOW_PX == 120`、`Config().motion.exit_overflow_px == 120` | ✅ |
| 边界用例仍成立 | `tests/test_router.py` 五个溢出用例 | 位移改用 `C.EXIT_OVERFLOW_PX + 10`，仍按时交还 | ✅ |
| 托盘图标不可见时不藏窗口 | `test_request_close_minimizes_when_tray_icon_is_invisible` | 退化成最小化，`_shutdown_done` 仍为 False | ✅ |
| 托盘可见时正常收起 | `test_request_close_hides_when_close_to_tray` | 先 `ensure_visible()`（调用 1 次）再 `hide()` | ✅ |
| 日志可定位托盘问题 | 源码 `--simulate` + `close_to_tray: true` → `WM_CLOSE` | 日志 `tray shown: available=True visible=True` → `tray ensure_visible -> True` → `window hidden to tray`（本机 agent 进程 `Shell_NotifyIcon` 返回 ACCESS_DENIED，**图标是否真的挂上无法在本机证实**） | ⚠️ 本机环境限制 |
| 托盘图标可辨认 | `tests/test_ui_smoke.py::test_tray_icon_carries_the_brand_and_the_state` + `build/padlend-0.3.16-tray-icons.png` | 图标 = 品牌标「P」；≥24px 在右下角叠状态点（`idle`/`captured` 像素不同）；16/20px 只留品牌标 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 409 passed, 3 skipped | ✅ |
| 静态检查 | `ruff check .` | All checks passed | ✅ |
| 验收工具 | `tools/acceptance.py` | 33 项，0 失败，3 跳过 | ✅ |
| 0.3.16 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 53,189,727 B，sha256 `cec28aefcb15be57bc2100626600037a0c7ab454519bb8e40d7d8c51bc16a71b`，`--version` → `PadLend 0.3.16` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2 --no-wizard --no-single-instance` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate` + `close_to_tray: true` → `ps-close-check.ps1` | 设备页「设备列表」、设置页「上报频率 1000 Hz」下拉与「穿回阈值 120 px」；`v0.3.16`；`WM_CLOSE` 后 `alive=True`、日志 `tray ensure_visible -> True` + `window hidden to tray`，窗口句柄归 0 | ✅ |

仍需真机确认：在系统托盘（含「^」溢出区）里能看到 PadLend 的品牌标图标；
关闭到托盘后从托盘菜单「打开主窗口」/重新运行 PadLend 能唤回窗口。
截图：`build/padlend-0.3.16-settings.png`、托盘图标预览 `build/padlend-0.3.16-tray-icons.png`。

---

## 第 22 节：0.3.17 设置页删掉「偏移」「角优先」（2026-11-05）

| 项 | 命令 / 方式 | 结果 | 状态 |
|---|---|---|---|
| 设置页不再有这两行 | 源码实例 `Ctrl+,` → UIA 读「📍 平板位置」卡片 | 只有「吸附边」「对齐」两行；无「偏移」「角优先」 | ✅ |
| 面板不引用已删控件 | `ruff check .` | All checks passed | ✅ |
| 控件注册表同步清理 | 单测 `test_settings_widgets_do_not_react_to_the_wheel` | 通过（控件列表改为 edge/align/hz/layout/exit/arm） | ✅ |
| 摆位仍能保存 | `_build_placement()` 从 `config.placement` 读回 `offset` / `corner_priority` | 未覆盖画布拖动写入的值 | ✅ |
| 配置兼容 | 旧 yaml 里的 `placement.offset` / `placement.corner_priority` | 字段保留、继续生效；设置页不再显示 | ✅ |
| 全量回归 | `pytest -o addopts="" -q` | 409 passed, 3 skipped | ✅ |
| 0.3.17 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 53,185,982 B，sha256 `1a601a1e9891f2d8b42e58ae92d7a93fe156e6a4ec2fbf6bb7854839a132223d`，`--version` → `PadLend 0.3.17` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2 --no-wizard --no-single-instance` | exit 0，`[device] sim:5555 1707x1067` | ✅ |
| 打包版 GUI | `--simulate` → `Ctrl+,` 截图 | 「📍 平板位置」只剩吸附边/对齐，无偏移/角优先；`v0.3.17`；`WM_CLOSE` 后进程归零 | ✅ |

截图：`build/padlend-0.3.17-settings.png`（源码）、`build/padlend-0.3.17-pkg-settings.png`（打包版）。

## 第 23 节：0.3.18 托盘注册自检与兜底（2026-11-05）

| 项 | 命令 / 方式 | 结果 | 状态 |
|---|---|---|---|
| 探针直接问外壳 | `win/trayprobe.py::probe_icon()`（message-only 窗口 + `Shell_NotifyIconW` NIM_ADD/NIM_DELETE） | 本机返回 `(False, 5)`（ACCESS_DENIED）——本机 agent 会话挂不上通知区图标，属环境限制 | ✅ |
| 启动日志能区分「注册失败」与「被收进 ^」 | 打包版启动后读 `padlend.log` | `tray shown: qt_available=True qt_visible=True native_ok=False err=5 entries=64 self_entry=False` | ✅ |
| 启动后复核 | 同上，1.2 秒后 | `tray registration rejected by the shell (err=5)` + `tray recheck after startup -> False` | ✅ |
| 图标被拒时退化成最小化 | 打包版 `WM_CLOSE`（`build/ps-close-check.ps1 -Hwnd 6558056 -ProcId 92180`） | `alive=True`、窗口最小化、日志 `tray icon rejected by the shell; minimized instead of hiding (ui.close_to_tray err=5)`，**只出现一次**（原生 WM_CLOSE 与 closeEvent 已去重） | ✅ |
| 底栏告知原因 | 恢复窗口截图 | 底栏显示「系统托盘没有接受图标，已改为最小化到任务栏，可在设置里关闭「关闭窗口时最小化到托盘」」 | ✅ |
| 单元测试 | `pytest -o addopts="" -q` | 411 passed, 3 skipped（新增探针判定、退化文案、首次提示三例） | ✅ |
| 无硬件验收 | `python tools/acceptance.py` | 33 项 0 失败 3 跳过 | ✅ |
| 0.3.18 构建 | `python -m PyInstaller --noconfirm PadLend.spec` | 53,196,978 B，sha256 `f92ed0260b7771127ca2a5d47e9caaeb61b5a9a81eafc5d7c108a75e1aef4732`，`--version` → `PadLend 0.3.18` | ✅ |
| 打包版 headless | `--simulate --headless --duration 2 --no-wizard --no-single-instance` | exit 0 | ✅ |

截图：`build/padlend-0.3.18-tray-fallback.png`（打包版被拒后最小化 + 底栏说明，底部 `v0.3.18`）。

**待真机复验**：用户机器上 `tray shown` 行的 `native_ok` / `err` 才代表真实结果。若
`native_ok=True`，图标已被外壳接受、只是落在任务栏「^」溢出区（拖出来即可常驻）；
若 `native_ok=False`，把 `err=` 的值发回来即可定位（5=ACCESS_DENIED 表示外壳拒绝）。





