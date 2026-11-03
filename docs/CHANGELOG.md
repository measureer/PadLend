# Changelog

遵循 [Semantic Versioning](https://semver.org/)。

## [0.2.4] — 2026-11-03

### 修复

- **无线调试已开但设备列表为空**：Android 17 的 ADB 在 mDNS 重名时会把 serial 打成 `adb-xxx (2)._adb-tls-connect._tcp`，第二列因此不是 `device`。旧解析直接丢弃该行，而界面只认在线设备，于是“adb 明明看得到、PadLend 却检测不到”。现在按已知 state 的位置回拼 serial（`adb -s` 接受带空格的 serial），不再误丢。
- 同一台平板同时以 mDNS 串名和 `ip:port` 出现时，优先保留可直接使用的 `ip:port`，设备列表只显示一条；纯 mDNS 设备仍保留。
- mDNS 形式的 `adb-..._adb-tls-connect._tcp` 现在正确识别为无线而非 USB。
- 无线调试重启后 mDNS 串名会变化，配置里的旧 serial 可能失效；若此时只有一台在线设备，自动改用该设备，不再一直 `E101/FAULT`。

### 测试

- 310 passed, 3 skipped；新增“仅 mDNS 带空格串名仍保留”“旧 serial 回退到唯一在线设备”等回归测试。

## [0.2.3] — 2026-11-03

### 修复

- **双指针**：接管期遮罩不再使用 `WS_EX_TRANSPARENT`，窗口过程处理 `WM_SETCURSOR` 并调用 `SetCursor(NULL)`；`CursorMask.show()` 同时通过 `ShowCursor(FALSE)` 隐藏系统光标，`hide()/release()` 对称恢复。以前只锁 `ClipCursor` 不藏光标，平板指针和 PC 光标会同时可见。
- **过早穿回**：新增 `motion.entry_guard_px`（默认 160）。指针进入平板后必须先离开入口边至少 160px，才允许累计出口溢出；从入口边直接向上/下/左/右滑不会再立刻被误判成“穿回 PC”。设为 0 可关闭。
- 遮罩不再点击穿透（接管期 PC 点击本来就不应生效）；光标隐藏失败只记 warning，不阻断截获。

### 测试

- 308 passed, 3 skipped；新增光标隐藏/恢复计数平衡、重复 `show()` 幂等、进入保护阻止立即穿回等回归测试。

## [0.2.2] — 2026-11-03

### 修复

- 设备列表重复：`adb devices -l` 的畸形 mDNS 行（state 字段是串名碎片）不再进入列表；同一 serial 只保留一条。
- **鼠标无法穿透到平板**：`DesktopMap` 补上 `cursor_pos()` / `set_cursor_pos()`，Router 现在读取系统光标的绝对物理坐标，而不是把 Raw Input 的鼠标计数当像素累加。后者在开启指针加速时会迅速漂移，导致边缘判定落在错误位置、`arm.fail left_edge` 循环。
- 光标查询偶发失败时保持上一次绝对位置，不再退化为计数累加。
- 连接成功后顶栏/设备页状态胶囊未刷新、仍显示“未连接”的问题；底部状态栏现在提示从哪一侧边缘滑入平板。
- 键盘钩子看门狗把“用户只动了鼠标”误判为钩子失效并每 30 秒重装、刷屏日志的问题；改为调试日志，不再自动重装。

### 测试

- 305 passed, 3 skipped；新增 adb 畸形行去重、真实 `DesktopMap` 绝对光标外推 40px 进入 CAPTURED、光标查询失败不累加等回归测试。

## [0.2.1] — 2026-11-02

### 新增

- 白天/黑夜双主题：顶栏新增太阳/月亮切换按钮，切换后立即重绘 QSS、调色板、标题栏、画布、设备页与托盘，并持久化到 `ui.theme`；启动时读取配置应用。
- 亮色令牌（`LIGHT_COLORS`）覆盖窗口/卡片/边框/文字/强调色与状态色；`tokens.set_theme()` 就地更新派生表，QSS 仍使用同一份模板。
- 界面截图：`docs/screenshots/ui-light.png`。

### 修复

- 画布图元此前读取不存在的 `tokens.TOKENS`，主题切换后仍使用旧深色兜底；改为每次绘制读取当前 `tokens.COLORS`。
- Windows 标题栏固定深色：改为跟随当前主题设置 `DWMWA_USE_IMMERSIVE_DARK_MODE`。
- 配置 `ui.theme` 仅做类型校验；非法值现在回退 `dark` 并记录 issue。

### 已知限制

- 亮色模式已通过原生窗口截图与 offscreen 测试验证；真机连接/边缘穿越/键盘交互仍待平板重新连接后复验。

## [0.2.0] — 2026-11-02

### 新增

- 全新 GUI 视觉：深色分层主题、靛蓝渐变品牌标识、84px 左侧导航栏（设备/布局/设置/日志四页）、64px 顶栏状态胶囊与 QPainter 线性图标，替换旧版文字/emoji 按钮。
- 设备管理页：扫描并列出设备（型号、序列号、USB/无线、授权/离线状态）、刷新、连接/断开、连接后展示真实设备事实；扫描与连接均在后台线程执行，不阻塞界面。
- Router 新增 `connect_device(serial)` / `disconnect_device()` 与用户主动断开挂起标志；断开后 supervisor 不再自动重连，重新连接由用户显式触发。
- 未连接门控：连接前画布显示“未连接设备 / 连接后显示实际尺寸”，设备详情的分辨率/方向全部为“—”；连接后才按设备上报的真实 facts 显示，不再假设 3200×2136。
- 界面截图：`docs/screenshots/ui-devices.png`、`docs/screenshots/ui-layout.png`。

### 修复

- 设备详情“断开连接”与“连接此设备”按钮的禁用态样式缺失，未连接时看起来仍可点击。
- 顶栏设备名改为型号优先、序列号作为 tooltip，减少长序列号对标题栏的挤压。
- `open_settings` 仍引用已删除的旧分割器布局；改为切到设置页并聚焦。

### 已知限制

- 设备连接/断开与画布门控已通过 offscreen 与原生窗口截图验证；真机交互（点击连接、边缘穿越、键盘转发）待平板重新连接后复验。

## [0.1.0] — 2026-11-02

### 新增

- 平板连接层：adb 枚举/推送/forward、scrcpy-server 5.0.1 headless 启动、1 字节握手（实测修正，原设计记 4 字节）、UHID 鼠标/键盘注册与销毁、幽灵进程清理、指针速度记录/归零/还原、断线重连。
- 相对指针估计器：残差累加、±127 截断、边界夹紧、`warp_to` 批量定位、边缘命中判定。
- 虚拟桌面几何：EnumDisplayMonitors 枚举、外轮廓边段计算（1px 扫描线 + 跨屏合并）、沿边映射、角落裁决、入口/落点计算、混合 DPI。
- 状态机 Router：IDLE → ARMING → CAPTURED → RETURNING → IDLE，FAULT 与重连；不变量 I1–I6 校验。
- Raw Input 捕获（RIDEV_INPUTSINK、绝对坐标归一化、按钮/滚轮、时间戳）与 1000Hz 发送线程。
- CursorMask：每显示器透明遮罩 + ClipCursor 边缘锁，`release()` 幂等，多级兜底。
- 键盘转发：VK→HID 全表、左右修饰键、6KRO、扩展键、passthrough 白名单、`Ctrl+Alt+S/Q` 保命键。
- 剪贴板（G9，SHOULD）：接管瞬间与 `Ctrl+Alt+V` 把 PC 文本经 `INJECT_TEXT` 发给平板，UTF-8 上限 4KB，仅文本。
- GUI：暗色主窗（状态条/画布/设置栏/日志/指标）、可拖拽吸附画布、设置面板、托盘状态色、首次向导、诊断页与诊断包导出。
- 配置：YAML 读写、校验与自动修复、版本迁移、单实例锁、日志轮转。
- 打包：PyInstaller 单文件 `dist/PadLend.exe`（约 51MB，内置 PySide6、scrcpy jar 与 adb），`runtime_tmpdir="."` 适配受限 Windows 临时目录；frozen 配置/日志目录固定在 exe 同目录 `.padlend/`；内置 adb 37.0.1 与图标。已实际构建并 GUI/无设备冒烟。
- 测试：协议黄金字节、估计器、几何 8 种布局、配置、键盘映射、Router 状态机、UI offscreen smoke、键盘钩子安装。

### 修复

- `GetCurrentThreadId` 误从 user32 加载（全局热键与低级键盘钩子启动即失败）：改从 kernel32 加载，并加真实安装测试。
- GUI 启动时安装低级键盘钩子、退出时卸载；此前键盘转发虽已实现但未接线。
- GUI 启动时启动 RawInputSource 并接线 `Router.on_raw`；此前只有 `--debug-raw` 使用 Raw Input，正常 GUI 模式收不到鼠标事件。
- `warp_to` 亚像素残差导致入口不命中/出口溢出失效：边界轴补一帧精确落边。
- 错位双屏下默认吸附段取错（首条而非最长段）：改为同边同显示器取最长合法段。
- `--headless` 此前只连接设备、不转发鼠标，且 `--duration` 在 session 已置 started 但 facts 未发布的窗口内失效（会卡到 40s 首连超时）：连接后启动 Raw Input，位移经 `CursorEstimator` 直接发 UHID（不经过边缘状态机），定时检查移到循环顶部。
- 单文件打包：onefile 解包目录因环境 `%TEMP%` 不可写而失败 → `runtime_tmpdir="."`；frozen 时配置目录误落 `_MEIPASS` 导致退出即丢 → 拆 `APP_DIR`/`BUNDLE_DIR`。
- 打包版缺少 adb：内置 platform-tools 37.0.1 到 `vendor/adb/`，`tablet/adb.py` 自动命中。

### 已知限制

- 管理员权限窗口（UAC）无法接收钩子输入。
- A4–A6 多显示器真机场景待双屏环境复验。
- 当前开发环境没有连接平板，A1–A13 真机验收记录见 `docs/TROUBLESHOOTING.md`。
- 2026-11-02 晚补：Honor CGA-W00 已连接，真机验证通过 adb 无线连接、1 字节握手、UHID 鼠标/键盘注册、`pointer_location` 可见光标移动、正常退出还原 `pointer_speed`；边缘穿越/键盘输入等交互项仍待复验。
