# PadLend 前端迁移：PySide6 → Tauri 2 + React/TypeScript

> 版本基线：源码 1.0.5 → 发布为 **1.1.0**。本文说明选型理由、目标架构、主要改动、
> 视觉/性能/架构优化结果，以及验证结论与后续构建步骤。1.1.0 已删除全部旧 Qt 代码。

---

## 1. 选型理由

### 1.1 为什么是 Tauri 2

| 维度 | 结论 |
|---|---|
| 体积与内存 | Tauri 用系统 WebView2，不打包 Chromium。原 PySide6 发布包约 138 MB（含 Qt 运行时），新外壳只有 Rust 可执行文件 + 静态资源；WebView2 在 Windows 10/11 上已随系统提供。 |
| 界面表现力 | CSS 的渐变、阴影、圆角、容器查询、过渡动画、暗/亮主题变量、SVG 图形比 QSS 更可控，且能做出一致的自适应布局；这是「界面不美观」的根治手段。 |
| 线程模型 | Rust async 命令 + 事件推送 + 独立 Python 业务进程，天然避免「耗时操作跑在 GUI 线程」；Qt 里必须靠 QThread/QProcess/信号排队小心规避。 |
| 与现有引擎的关系 | `core/`、`tablet/`、`win/`、`config.py` 完全不依赖 Qt（除 `core/crash.py` 的弹窗兜底），可以原样保留，不必用 Rust 重写 90KB 的 Router 状态机、UHID/ADB 协议与 Win32 钩子。 |
| 生态 | 官方插件覆盖托盘、单实例、剪贴板、打开路径；Rust 侧只做进程/窗口编排。 |

没有选择 Electron：内存/体积与 Qt 相比没有决定性收益，且仍要维护 Node 主进程。
没有选择纯 Rust 重写业务层：会推翻已经过 488 个测试验证的状态机与协议实现，风险远大于收益。

### 1.2 为什么选 React 19 + TypeScript（而不是 Svelte）

| 维度 | React + TS | Svelte 5 |
|---|---|---|
| 复杂状态（设备/状态机/指标/日志/布局/profile） | `zustand` 按字段订阅，selector 精确控制重渲染，生态成熟 | runes 更轻量，但大型多切片状态的调试工具与模式较少 |
| 虚拟列表、拖拽画布、按需加载 | 有成熟的 memo / `useSyncExternalStore` / lazy 分包模式 | 需自行组织，团队迁移成本更高 |
| 类型契约 | TS 与 `service/serializers.py` 一一对应，接口漂移编译期暴露 | 同样支持 TS，但生态与泛型工具略少 |
| 可维护性 | 组件职责、hooks、store 的分层在多人协作中更稳 | 单文件组件更紧凑，但本项目有大量业务表单/对话框 |

**取舍说明**：Svelte 的编译期响应式确实能少一点运行时 diff 开销；本项目的瓶颈并不在框架 diff，
而在「全量重建列表 / 每帧重建画布 / 2000 行富文本重绘 / 主线程轮询」。这些已通过
**事件驱动 + 精确订阅 + rAF 直接改 DOM + 虚拟列表 + 线程池** 解决，React 的运行时开销
（gzip 后约 4.7 KB 的 react chunk + 84 KB 主包）不是主要矛盾。因此选 React 换取更好的
可维护性与更低的迁移风险。若后续要进一步压缩首屏体积，可在同一 `Backend` 契约下替换 UI 框架。

---

## 2. 目标架构

```
┌───────────────────────────── Tauri 2 进程 ─────────────────────────────┐
│ WebView (React 19 + TS)        │  Rust 外壳                              │
│  src/                          │  src-tauri/src/                         │
│  ├ api/     传输契约 + mock    │  ├ bridge.rs   NDJSON RPC / 事件转发     │
│  ├ store/   单一状态源         │  ├ commands.rs #[tauri::command]        │
│  ├ lib/     纯逻辑/编排        │  ├ tray.rs    托盘菜单                   │
│  ├ components/ 设计系统        │  └ lib.rs     窗口/单实例/关闭到托盘      │
│  └ features/ 页面与对话框      │                                         │
└─────────────── invoke("rpc") / emit(event) ────────────────────────────┘
                            │ stdin/stdout NDJSON（一行一个 JSON）
┌───────────────────────────── Python 业务服务 ──────────────────────────┐
│ service/  server.py(线程池) → api.py(门面) → serializers.py(DTO)         │
│           core/router.py · core/geometry.py · core/diagnostics.py        │
│           tablet/* (adb/protocol/session/uhid/locator) · win/* · config  │
└────────────────────────────────────────────────────────────────────────┘
```

职责边界：

| 层 | 允许做 | 禁止做 |
|---|---|---|
| React 组件 | 展示、交互、草稿态 | 直接拼 IPC 字符串、持有 Router/Adb/DesktopMap |
| `src/lib/actions.ts` | 语义化动作编排、错误/提示 | 业务规则计算 |
| Rust | 窗口/托盘/单实例/剪贴板/路径/sidecar 生命周期、转发 RPC | 业务判断 |
| Python 服务 | 状态机、几何、ADB/UHID、配置校验与落盘、诊断、设备记忆与布局 profile | 任何 UI |

**关键点**：业务接口 100% 复用原实现，前端不再持有 Python 对象；Qt 只在保留的
`ui/` 旧版代码里存在，新进程不会加载 PySide6。

---

## 3. 主要迁移改动

### 3.1 新增 / 修改的文件

| 路径 | 作用 |
|---|---|
| `service/{server,api,serializers}.py` | NDJSON 服务、业务门面、DTO 唯一转换点 |
| `core/diagnostics.py` | 从旧 `ui/diagnostics.py` **上移**的纯业务逻辑（脱敏、解析、报告、zip 导出）；1.1.0 起成为唯一实现 |
| ~~`ui/diagnostics.py`~~ | 迁移期曾作为 Qt 采集器重导出同名符号，1.1.0 随 `ui/` 一起删除 |
| `src-tauri/` | Tauri 2 配置、图标、capability、Rust 桥接/命令/托盘 |
| `src/` | React/TS 前端（设计系统、4 个页面、4 个对话框、mock 后端、测试） |
| `tools/build_sidecar.py` | 把 `service/` 打成随包 sidecar exe |
| `tools/make_tauri_icons.py` | 从既有 `assets/padlend.ico` 生成 Tauri 图标，保持品牌一致 |
| `tests/test_service_bridge.py` | 真实子进程 + 假平板的协议端到端测试 |
| `frontend-dist/` | 前端构建输出（**与 PyInstaller 的 `dist/` 分离**） |

### 3.2 业务接口映射

| 旧 UI 调用 | 新 RPC 方法 | 说明 |
|---|---|---|
| `router.connect_device / disconnect_device` | `devices.connect / disconnect` | 保留 ADB 预连接、45s 事实等待、设备记忆与 MAC 采集 |
| `adb.devices()`（`_ScanThread`） | `devices.scan` | 合并「在线设备 + 记忆设备 + 局域网定位状态」 |
| `adb.pair/connect` + `normalize_address` | `devices.add` | 地址补全、端口/配对码校验仍在前后端双校验 |
| `_known_devices/_remember_device/_forget_device/_locate_all_worker` | `devices.locate / remove` + 服务内部方法 | 业务规则从主窗下沉到服务层 |
| `router.toggle_manual/emergency_stop/resync_pointer/inject_clipboard` | `engine.*` | 全局热键由服务进程的 `win.hotkeys` 继续注册 |
| `router.set_placement` + `_save_layout/_record_layout_profile` | `layout.save` | 按 `(hardware_serial, landscape/portrait)` 写 profile，并立即推给 Router |
| `DesktopMap.monitors/exterior_segments/bounding_box` | `layout.desktop` | DTO 化，前端不再 getattr 猜结构 |
| `Config.load/save/validate` | `config.get / config.save` | 统一补丁 + 服务端权威校验 + 失败整体回滚 |
| `ui.diagnostics` 采集/导出 | `diagnostics.run / export / static` | 复用 `core.diagnostics`，脱敏与 zip 结构不变 |
| `SetupWizard` 3 步检测 | `wizard.check` | 检测在服务进程执行；adb/scrcpy-server 随包内置 |
| `ui.bridge.RouterBridge` 4 个回调 | `state / metrics / facts / orientation / clipboard / log` 事件 | 事件驱动，取消 1s GUI 轮询 |
| `MetricsPanel._pick` 别名猜测 | `metrics` 事件原样使用 `raw_hz/send_hz/backlog/drop_pct/...` | 修复旧版 3 个字段恒为「—」的键名不一致 |

### 3.3 平台能力迁移

- **托盘**：Rust `TrayIconBuilder`，菜单 9 项与旧版一致；动作以 `tray:action` 事件交给前端编排；状态变化时只替换菜单，不重建图标。
- **单实例**：`tauri-plugin-single-instance`，第二实例显示并置前已有窗口。
- **关闭到托盘**：`WindowEvent::CloseRequested` 按前端同步的 `set_close_to_tray` 决定 `prevent_close + hide` 还是退出。
- **全局热键**：仍由 Python 服务的 `win.hotkeys.HotkeyManager`（RegisterHotKey）承担；窗口内快捷键由 `useHotkeys` 处理。
- **DPI / Raw Input / 键盘钩子 / 遮罩**：服务进程启动时调用 `_set_dpi_awareness()`，其余由 `main.Runtime` 原样组装。
- **窗口外观**：自定义无边框标题栏 + CSS 变量主题，不再依赖 DWM 暗色属性注入。
- **剪贴板**：`tauri-plugin-clipboard-manager`，接管期间 500ms 去重轮询，平板→PC 回写有 1s 抑制窗口，避免回环。

### 3.4 传输协议

NDJSON（每行一个 UTF-8 JSON）：

```
→ {"id":1,"method":"devices.scan","params":{}}
← {"event":"devices","data":{"devices":[...]}}
← {"id":1,"ok":true,"result":{"devices":[...],"hint":"1 台设备","error":""}}
```

- `stdout` **只**承载协议数据；日志全部走 stderr / 文件，避免污染解析。
- 慢方法在 `ThreadPoolExecutor(8)` 执行；Rust 侧 `call()` 是 async，等待响应不占用线程。
- 退出先发 `app.shutdown` 让 Python 优雅释放 UHID/遮罩/指针速度，超时才强杀。
- 开发期自动 `python -m service.server`；发布期优先加载同目录 sidecar exe。

---

## 4. 视觉优化结果

1. **统一设计系统**：`src/styles/tokens.css` 是唯一令牌源——两套主题（dark/light）各 20+ 色值、
   8px 间距标尺、圆角、控件高度、字体栈（Segoe UI Variable / Microsoft YaHei UI / Cascadia Mono）、
   动效曲线。组件里没有裸色值，旧 QSS 里写死的 `rgba(...)` 全部变量化，亮色主题不再残留暗色块。
2. **一致的组件库**：`src/components/ui.tsx` + `Icon.tsx` 提供 Button/IconButton/Card/Field/Toggle/
   Slider/Select/TextInput/StatusPill/Banner/Modal/Toast/EmptyState，4 个页面与 4 个对话框共用。
3. **统一布局**：自定义标题栏 → 顶栏（品牌/状态胶囊/设备信息/操作） → 侧栏导航 + 内容区 → 底栏状态。
   卡片头部/正文/底部结构一致，标题层级与留白统一。
4. **响应式**：断点 1180px（侧栏收成图标）、900px（两列变单列、隐藏次要头部信息）、
   700px（侧栏改底部标签栏、隐藏底栏、弹窗紧凑）、680px 高度以下压缩画布/日志高度。
   画布用 SVG `viewBox` 自动适配任意窗口尺寸，字号/线宽按虚拟桌面像素量级给定，缩放后仍可读。
5. **验证截图**：`docs/screenshots/tauri-*.png`（设备页、布局页、设置页、日志页、亮色主题、900×700 窄窗口），
   由无头 Edge 对真实构建产物截图，确认无重叠、无裁切、无横向溢出。

---

## 5. 性能优化结果

| 旧瓶颈 | 新方案 |
|---|---|
| 1s `QTimer` 在 GUI 线程轮询 `router.facts` | 服务线程 0.5s 检测事实签名，变化才推 `facts` 事件；前端零轮询 |
| `DevicePage._render()` 每次扫描/定位都清空并重建全部行 | React keyed 列表 + `memo` 行组件，只更新变化的行 |
| `LogView.set_filter()` 每敲一个字符重建最多 2000 行富文本 | 140ms 防抖 + `useMemo` 过滤 + 虚拟滚动（只渲染可视区 ~30 行），最多保留 2000 条 |
| `canvas_items.color()` 每次绘制 `importlib` 解析主题表 | 颜色全部为 CSS 变量/SVG class，主题切换只改根属性，零逐帧解析 |
| `refresh_desktop()` 全量重建 QGraphicsScene；拖动遍历全部边段并逐个 `update()` | SVG 静态场景，拖动/平移/缩放只改 `<g transform>`，`pointermove` rAF 合帧，pointerup 才提交一次 React 状态 |
| `mouseMoveEvent` 每次移动发信号并写 debug 日志 | 无事件、无日志；只在落点提交时更新 store |
| 诊断采集/导出在 GUI 线程读 512KB 日志写 zip | 服务进程线程池执行，前端只显示进度事件 |
| adb 扫描 / 连接 45s 轮询 / mDNS-ARP 探测 | 全部在 Python 服务进程线程池；WebView 从不阻塞 |
| 主题切换重绘全部图标 + 全量刷新设备页 | `document.documentElement.dataset.theme` 一处赋值，业务组件不重渲染 |
| 设置页每个控件改动都触发一轮控件刷新 | 草稿本地 state，滑条拖动零 IPC，只在「保存设置」提交一次 |
| 资源加载 | 路由/对话框级 `React.lazy` 分包（首屏主包 gzip 84KB，页面 chunk 各 1.7–4.6KB）；内联 SVG 图标，无字体/图片网络请求 |
| `metrics` 键名不一致导致 3 个字段恒为「—」 | 前端直接消费 `core.metrics.snapshot()` 键名 |

---

## 6. 架构优化结果

- **界面层 / 业务逻辑彻底分离**：前端不 import 任何 Python 模块；Rust 不做业务判断；
  业务规则集中在 `service/api.py`，DTO 集中在 `service/serializers.py` + `src/api/types.ts`。
- **消除重复代码**：
  - 设置页 14 个字段由 `configSchema.ts` 一份声明驱动渲染/取值/写回/校验，取代旧版逐控件赋值；
  - 诊断纯逻辑从 Qt 文件上移到 `core/diagnostics.py`，Qt 与新服务共用一份实现；
  - 旧主窗里 20+ 个 `_xxx_worker` 线程/signal 组合被 `Backend.call` + 事件订阅替代；
  - 托盘菜单与顶栏 ⋯ 菜单复用 `src/lib/actions.ts` 的同一批动作函数。
- **明确依赖方向**：`components → features → lib/actions → api/client → (Tauri|mock)`，
  `store` 只被订阅，`service` 单向依赖既有引擎；无循环依赖。
- **可测试性**：`MockBackend` 实现同一 `Backend` 接口，浏览器里可跑完整交互；
  `tests/test_service_bridge.py` 用真实子进程验证协议。

---

## 7. 验证结果

| 项目 | 结果 |
|---|---|
| Python 全量测试 | 迁移阶段 **488 passed, 3 skipped**（含 7 项服务协议测试）；1.1.0 删除 Qt 测试后全量重跑通过，`ruff check .` 全通过 |
| 前端单元测试 / 交互冒烟 | **28 passed**（6 个文件：几何、配置 schema、store、格式化、外壳、核心交互） |
| TypeScript | `tsc --noEmit` 无错误 |
| 前端构建 | `npm run build` 成功，输出 `frontend-dist/`；按路由/对话框分包 |
| 服务联调 | `python -m service --simulate --no-hooks` 实测 bootstrap/扫描/布局/诊断/引擎事件均正常 |
| 视觉验证 | 6 张无头浏览器截图，覆盖 4 个页面、亮色主题、900×700 窄窗口 |
| 发布产物 | `tools/release.py --build --zip --sha256` 生成前端/sidecar/zip/校验和 |

> **环境限制（需如实说明）**：本机未安装 Rust/Cargo 与 MSVC 构建工具，因此 Tauri 原生 exe
> 与 NSIS 安装包由 GitHub Actions 在 `windows-latest`（Rust stable + MSVC + WebView2）上编译，
> CI 的 `npx tauri build --no-bundle` 负责验证 Rust 代码可编译。前端、Python 服务、
> 协议联调与视觉/交互验证均在本机完成。

---

## 8. 构建与运行

```powershell
npm ci
npm run dev                    # http://127.0.0.1:1420（浏览器演示后端，无需 Rust）
# 预览各页面： ?page=layout|settings|logs|devices&theme=light|dark

# 单独启动业务服务（调试用）
.\.venv\Scripts\python.exe -m service --simulate --no-hooks

# 构建全部并生成发布包（无 Rust 时跳过原生外壳）
.\.venv\Scripts\python.exe tools\release.py --build --zip --sha256

# 完整 Tauri 应用（需 Rust 1.77+ / MSVC / WebView2）
npm run tauri:dev
npm run tauri:build

# 验证
.\.venv\Scripts\python.exe -m pytest -q
.\.venv\Scripts\python.exe -m ruff check .
npx vitest run
npm run build
```

开发模式若没有 sidecar exe，Rust 会自动回退到 `python -m service.server`；
可用 `PADLEND_PYTHON`、`PADLEND_PROJECT_ROOT`、`PADLEND_SERVICE_CMD`、`PADLEND_SIMULATE`
指定解释器/项目根/自定义命令/演示假平板。

---

## 9. 已知事项

- **1.1.0 已删除全部旧 Qt 代码**：`ui/`、旧 GUI 测试与截图、`PadLend.spec`、
  Inno Setup 配置、Qt 相关依赖与许可文本都不再存在；`main.py` 只保留无头/服务组合根，
  `core/crash.py` 不再 import PySide6。发行物不再包含 Qt DLL。
- 前端构建输出为 `frontend-dist/`，`dist/` 专用于发布产物，两者互不干扰。
  `tools/release.py` 统一生成 zip / NSIS 安装器副本 / SHA256SUMS.txt。
- 配置「手改后热加载」在原版只是设计意图、并未实现；新架构同样未开启 fs watch，
  如需可在 Rust 侧加 `notify` 后推送 `cfg.reload` 事件。
- 全局热键仍由 Python 服务注册，因此服务进程退出时热键随之注销。
- 未做代码签名；Windows SmartScreen 提示与原版一致。
