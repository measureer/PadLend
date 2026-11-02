# Changelog

遵循 [Semantic Versioning](https://semver.org/)。

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
