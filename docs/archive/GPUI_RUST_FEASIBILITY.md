# PadLend 改用 GPUI、并把部分 Python 换成 Rust 的可行性评估

> 评估基线：源码 **1.4.0**（Tauri 2 + React 19 + Python sidecar）。评估日期 2026-11-14。
> 结论先行：**前端换 GPUI 可行，属于中等规模重写；Python 换 Rust 同样可行，但那是一次完整重写而不是"替换几块"**。
> 本文给出当前基线、GPUI 生态的事实核查、本机实测结果、两条方案的逐模块工作量、风险与建议路线。
> 未验证项（中文 IME 实测、系统托盘、多屏 150% DPI、真机回归）在文中显式标注。

---

## 1. 一句话结论

| 问题 | 结论 | 代价 |
|---|---|---|
| 前端改 GPUI 可以吗 | **可以**。Zed 的 Windows 后端已 GA（D3D11 + DirectWrite），gpui-kit 提供 75+ 组件，本机已跑通中文窗口 | 26–40 人日（含托盘/单实例/打包胶水 3–6 天）；失去 CSS/DevTools/HMR 的开发循环 |
| 后端"有些 Python"能换 Rust 吗 | **能，但不能只换一部分**。1000Hz 热路径横跨 rawinput→router→sender→session，切一半就是把这条链塞进 IPC | 全量重写 ≈ 15,000 行 Rust、70–100 人日 + 真机 2–4 周 |
| 现在值得动吗 | **UI 值得，引擎不值得"顺手改"**。现有引擎没有性能问题证据，它的价值在 411 个测试和 30+ 条踩坑 | 见 §8 建议路线 |

---

## 2. 现状基线（实测，非估计）

| 层 | 文件数 | 行数 | 说明 |
|---|---|---|---|
| React 19 + TS 前端 | 36 | 4,851（含测试 ~500） | 4 个页面、4 个对话框、10 个界面文件 |
| CSS | 2 | 881 行 / 31.7 KB | `tokens.css` 双主题 + `app.css` 20+ 组件样式 |
| Rust 外壳（Tauri） | 7 个 .rs | 743 | bridge 244 / audio 254 / tray 104 / lib 80 / commands 54 |
| Python 引擎 | 41 | **14,186** | core 3,158 / win 3,684 / tablet 3,841 / service 2,228 / 顶层 1,275 |
| Python 测试 | 35 | 6,206，**411 个测试函数**（参数化后用例更多） | 另有前端 43 个用例 |

引擎的技术底色（这些数字决定了移植难度）：

- `ctypes` 出现 **458** 处、`windll` **38** 处 —— Win32 完全是手工绑定的原生调用；
- `threading` 111 处、`socket` 91 处、`subprocess` 30 处、`asyncio` **0** 处 —— 线程 + 阻塞 IO 模型；
- 运行时依赖只有 PyYAML、psutil；发布侧依赖 PyInstaller 打 sidecar exe。

输入热路径（DESIGN §10）：`RawInput(T2) → Router(T4) 状态机 → sender(T5, 1000Hz) → TabletSession.socket`，
单帧预算 1ms，p95 端到端 < 40ms（无线）/ 25ms（USB）。**这条链是"魔法"所在**，也是 PITFALLS 里 2.2/2.4/2.5 三次事故的位置。

---

## 3. GPUI 生态事实核查（2026-11-14）

> 核查对象是**当前最新**：`gpui-pre 0.3.12`（zed@d2a4fb6，2026-11-09，docs.rs 全量 API 索引）
> 与 `gpui-kit 0.9.1`（2026-11-09，75+ 项组件清单）。crates.io 上那个停在 0.2.2 的 `gpui` 不是现在在用的版本。

| 项目 | 事实 | 对 PadLend 的意义 |
|---|---|---|
| Zed for Windows | 2025-10-15 GA，DirectX 11 渲染 + DirectWrite 文本，每周发版，有专职 Windows 团队 | Windows 后端不是"实验性" |
| `gpui`（crates.io） | 最新 **0.2.2，2025-10-22**，此后未在 crates.io 发版 | 不能按老教程直接依赖 `gpui = "0.2"` |
| `gpui-pre` | **0.3.12，2026-11-09**，Zed 快照的发布通道（GPUI Kit 团队维护） | 实际路径是 gpui-kit 锁定的快照 |
| `gpui-kit` | **0.9.1，2026-11-09**；周更（0.7.3→0.8.0→0.8.1→0.9.0→0.9.1 只用了一个月）；36 万下载；20.5k stars；Apache-2.0；中文文档 | 组件齐全，但 0.9.x 仍在快速破坏式演进 |
| 可用能力 | 多窗口、菜单、虚拟列表/数据表格、代码编辑器、38 套主题、I18N、无障碍（AccessKit）、无头 UI 测试 + 离屏像素渲染、系统通知、打包指南、自动更新、FPS HUD | 日志页、设备表、设置表单、对话框都能直接对上 |
| 框架内没有 | **系统托盘**（0.3.12 API 索引里没有 Tray 类型；gpui-kit 75+ 项组件清单里也没有） | 要用生态补，见 §3.1 |
| 非框架职责 | 单实例、安装器生成 | 任何 UI 框架都不提供，见 §3.1 |
| 已有能力 | 剪贴板（`ClipboardItem`）、拖拽/画布绘制、IME 平台层（`PlatformInputHandler`）、**系统通知（核心 `SystemNotification`）** | 剪贴板、系统通知都不用像 Tauri 那样靠插件 |
| 许可 | Apache-2.0 | 与 PadLend 一致 |

> 供应链提醒：把 `gpui-kit` 与 `gpui-pre` **精确锁版本**（`=0.9.1` / `=0.3.12`），必要时 vendor 进仓库；
> 上游是"Zed 主线快照 + 社区按周发布"，不存在语义化版本的稳定性承诺。

### 3.1 缺口的真实成本（生态补位）

| 需求 | 框架内 | 补位方案 | 版本 / 日期 | 成本 |
|---|---|---|---|---|
| 系统托盘 | ❌ 无 API | `tray-icon` + `muda`（原生菜单） | 0.26.1 / 2026-10-07；muda 0.21.2 / 2026-10-08 | 200–300 行 + 2–4 天：Windows 端要求**独立线程跑消息循环**，菜单动作要回到 GPUI 主线程派发 |
| 单实例 | ❌ 非职责 | 自写命名互斥量 + 事件 + `SetForegroundWindow`（现成参考：`win/single_instance.py` 292 行）；或 `interprocess` 2.4.8 走命名管道 | interprocess 2.4.8 / 2026-11-08 | 80–120 行 / 1 天 |
| 安装器 | ❌ 非职责 | `cargo-packager`（NSIS + MSI/WiX + updater），或沿用 NSIS/Inno 脚本 + 现有 `tools/release.py` | 0.11.8 / 2025-11-27 | 1 天 |
| 系统通知 | ✅ 核心 `SystemNotification` | — | gpui 0.3.12 | 0 |

> 结论修正：**不是"三缺"**。托盘是唯一真正的框架空白（一片 200–300 行的胶水），单实例与打包本来就不该由 UI 框架提供，
> 而这两样在 PadLend 里都有现成参考实现或脚本。三项合计 **3–6 人日**，已计入 §5.1 的 26–40 人日。
> 不推荐 `single-instance` crate（0.3.3 停在 2021-12）与已停更的 `cargo-wix`（0.3.9 / 2025-03）。

---

## 4. 本机实测（这台开发机，不是推测）

| 项目 | 结果 |
|---|---|
| 工具链 | rustc/cargo **1.96.0**（gpui-kit 要求 ≥1.92）；MSVC 14.44 + Win SDK 10.0.26100 可用 |
| cmake | 仅在 `VS2022\...\CMake\bin` 内，**不在 PATH**；加入 PATH 后可编译 |
| 依赖规模 | 353 个唯一 crate；一次冷编译 **2 分 24 秒**（debug）/ **3 分 14 秒**（release） |
| 增量编译 | 改 `main.rs` 后 **6.4 秒** 出包（debug） |
| 最小窗口 | 中文标题 + 按钮色块 + 圆角 + 字号层级，**运行正常**，Microsoft YaHei UI 与 DirectWrite 字体回退可用 |
| 产物体积 | 最小窗口 release exe **17.96 MB**（单文件，无运行时依赖）；带上 4 页 4 对话框与图标预计 18–25 MB |
| 运行开销 | debug 常驻 100.3 MB / 私有 80 MB / 26 线程；**release 常驻 96.5 MB / 私有 88 MB / 39 线程** |
| 结论 | 编译与运行在本机**可行**，不需要换机器；首次拉依赖是唯一的长等待 |

> 注意：release 也是 ~96 MB，说明 **GPUI 不等于"一定比 WebView2 省内存"**——两者都在 100 MB 量级
> （WebView2 会多出渲染进程）。真正的收益是**去依赖、去多进程**，不是内存数字；上线文案不要写"更省内存"，要写"不依赖 WebView2"。

---

## 5. 方案 A：只把前端换成 GPUI（Python 引擎原样保留）

### 5.1 工作分解

| 工作项 | 现状 | 估算 |
|---|---|---|
| 设计令牌 / 双主题 | tokens.css 187 行 | 1–2 人日 |
| 外壳：自绘标题栏、导航、顶栏、状态胶囊、溢出菜单、toast | Shell.tsx 182 行 + app.css | 4–6 |
| 设备页 + 添加设备对话框 | 151 + 68 行 | 3–4 |
| 布局页 + SVG 画布（平移/缩放/吸附/Shift 换角/保存） | 306 + 66 行 | 5–8 |
| 设置页（14 字段由 schema 驱动） | 122 + configSchema 197 行 | 2–3 |
| 日志页（2000 行虚拟滚动 + 过滤） | 104 行 | 1–2 |
| 关于/诊断/向导 3 个对话框 | 189 行 | 3–4 |
| 后端层：NDJSON RPC、事件订阅、窗口控制、剪贴板、托盘（tray-icon）、单实例、关闭到托盘 | client.ts 141 + Tauri 插件 | 4–6（含 §3.1 的 3–6） |
| 测试与视觉验证（无头 UI 测试 + 离屏截图） | 43 用例 + 6 张截图 | 3–5 |
| **合计** | | **26–40 人日** |

### 5.2 拿到什么

- 去掉 WebView2 运行时依赖（对没装 WebView2 的机器是真实收益）；
- **少一种语言**：TS 消失，只剩 Rust + Python；
- UI 与引擎同宿主（Rust 侧），将来若做方案 B，UI 不用再改；
- GPU 渲染留了 120Hz 余量（本机 Intel UHD 730 也够用，本项目也不是图形密集型）。

### 5.3 失去什么（必须承认）

- **开发循环变慢**：现在是 Vite 亚秒级热更新 + 浏览器 `MockBackend` 预览四个页面；GPUI 改一行要 5–20 秒编译，没有 DOM 检查器；
- **视觉验证流程要重做**：现在用无头 Edge 对构建产物截图（docs/screenshots/tauri-*.png），GPUI 要改用离屏渲染或窗口截图；
- **Tauri 白送的三件事要接胶水**（§3.1，共 3–6 天）：托盘接 `tray-icon` + 独立消息循环线程（可对比现存的 `win/trayprobe.py`）、单实例抄 Python 侧命名互斥量实现、打包把 `externalBin`/`resources` 写进 NSIS/Inno 脚本；
- **上游不稳定**：gpui-kit 周更且 0.x，升级要留出返工时间。

---

## 6. 方案 B：Rust 单进程（前端 GPUI + 引擎全量 Rust）

### 6.1 逐模块评估

| 模块 | 行数 | 性质 | 难度 | Rust 收益 |
|---|---|---|---|---|
| `config.py` / `core/geometry.py` / `service/device_registry.py` / `core/diagnostics.py` / `core/metrics.py` / `service/serializers.py` | 2,463 | 纯逻辑，可 1:1 翻译，测试齐全 | 低 | 中（去 PyYAML/psutil，单一 schema） |
| `win/desktop.py` / `dpi.py` / `hotkeys.py` / `single_instance.py` / `cursor_watch.py` / `trayprobe.py` | 1,702 | Win32 薄封装 | 中 | 高（windows-rs 类型安全、无 ctypes 编组） |
| `win/rawinput.py` / `keyboard_hook.py` / `mask.py` | 2,574 | 钩子、遮罩、ClipCursor、ShowCursor 计数 | **高** | **高**（PITFALLS 3.1–3.4 的直接受益者） |
| `core/router.py` | 2,206 | 状态机 + 累加器 + 队列 | **高** | **高**（单进程直连、无 GIL、无守护线程截断） |
| `tablet/adb.py` / `locator.py` / `mdns.py` / `server.py` | 1,736 | subprocess / socket / 超时编排 | 中 | 中（可取消的 async、启动更快） |
| `tablet/session.py` / `protocol.py` / `uhid.py` / `keymap.py` / `cursor.py` / `audio.py` / `simulated.py` | 2,697 | scrcpy 协议、UHID 报文、1000Hz 发送 | **高** | 中高（真机才能验） |
| `service/server.py` / `api.py` / `serializers.py` | 1,686 | RPC 门面 + DTO | **直接删除** | 高（胶水消失） |
| UI（同方案 A） | — | — | 中 | 中 |

合计：约 **15,000 行 Rust**，70–100 人日编码 + **真机回归 2–4 周**（不能和编码并行压缩）。

### 6.2 白拿的收益

- **单 exe**（实测最小窗口 17.96 MB，完整 UI 预计 18–25 MB），无 PyInstaller、无 sidecar、无双进程杀软误报、冷启动更快；
- `service/*` 1,686 行 + `bridge.rs`/`commands.rs` 约 300 行 + `api/client.ts`、`mock.ts` 约 580 行 **全部消失**；
- **按定义消灭一整类事故**：PITFALLS 3.1（sidecar 活着但遮罩残留）、6.2（close_to_tray 把窗口与 sidecar 生命周期缠在一起）、4.2（Python 关闭被守护线程截断）、2.5（锁内 IO 阻塞鼠标）都源于"两个进程 + 线程"；
- 1000Hz 路径从"三线程 + 队列 + 锁"变成"单线程状态机 + 无锁通道"，延迟方差更容易控。

### 6.3 必须付出的代价

- 411 个 Python 测试函数要等价迁移（或保留 Python 作 oracle 跑黄金用例对比）；
- **30+ 条踩坑要逐条在真机重验**：Win 键吞掉、鼠标全松补发、滚轮 120 vs 1、UHID 半包、音频通道握手顺序（2.6 只有真机能验证）；
- 现有引擎的价值恰恰在这些细节里，重写等于把最贵的部分再做一遍。

---

## 7. 为什么"只换一部分 Python"不成立

1000Hz 热路径是 `rawinput → router → sender → session.socket` 一条链。只要在中间切开：

- 要么 Raw Input 线程跨进程写管道（PITFALLS 2.5 的翻版：慢链路拖垮鼠标）；
- 要么路由状态机留在 Python，而捕获在 Rust —— 每个鼠标包都要序列化 + IPC，1ms 预算直接吃掉一半。

可以"独立替换"的只有外围：`config/geometry/registry/diagnostics` 共 2,463 行。换完仍然要 **Python 运行时 + 全部 win/tablet 代码**，
收益（去 PyYAML/psutil）和成本（多一层 FFI/IPC）不成比例。

**所以只有两个自洽的选项**：A（UI 换、引擎留）或 B（全换）。中间态是"三套进程 + 两门语言 + 一个没被验证过的边界"。

---

## 8. 建议路线

### 阶段 0：Spike（2–3 天，先写死否决条件）

按这个顺序做，任何一项不过就先别动主分支：

| # | 门禁 | 通过标准 |
|---|---|---|
| 1 | 中文界面 + **微软拼音在 TextInput 里输入** | 候选框位置正确、提交后文本正确、退格正常 |
| 2 | 托盘图标 + 右键 9 项菜单 + 关闭到托盘 + 第二实例唤回 | 与现版行为一致 |
| 3 | 布局画布：拖拽/滚轮缩放/吸附/Shift 换角 | 拖动 60fps 无抖动，落点与现版一致 |
| 4 | 日志页 2000 行虚拟滚动 + 140ms 防抖过滤 | 滚动不掉帧 |
| 5 | 多屏（含虚拟显示器）+ 150% DPI | 窗口与画布缩放正确，负坐标不炸 |
| 6 | NSIS 安装/卸载 + zip + 许可与资源 | CI 能产出可安装包 |
| 7 | 冷启动与常驻内存 vs 现版 | 启动 < 1.5s；内存不高于现版（实测，不假设） |

### 阶段 1：方案 A 落地（若 spike 全过）

1. 先立 `Backend` trait（Rust 侧）：`call/on/clipboard/window/tray`，让页面可以对 mock 实现测试；
2. 先做外壳 + 日志页（最小的两块），打通 RPC 与事件；
3. 再做设备页/设置页，最后做画布（最重、最容易返工）；
4. Python 引擎与 411 个测试**一行不改**，只保留 `service/` 协议；
5. 验收：4 页 4 对话框功能对等、43 个前端用例等价覆盖、真机连接/接管/交还各 3 次无回归。

### 阶段 2：方案 B（只在明确要"单 exe/去 Python"时启动）

按 §6.1 自下而上：纯逻辑 → Win32 工具层 → adb/locator → router + rawinput + mask → session/protocol/uhid → 删 `service/`。
每个模块的验收标准是**它的 Python 测试全部等价通过 + 真机对应用例通过**；Python 版本作为 oracle 保留到最后一个模块落地。

### 不建议

- **先上 GPUI，再"顺手"拆 Python**：两个未验证变量叠加，出问题无法归因；
- **PyO3 嵌 CPython**：只消掉 IPC，消不掉分发体积、GIL 和崩溃隔离问题，Windows 上还要处理 `python3xx.dll` 的打包与 ABI；
- **为了性能动引擎**：没有证据表明 Python 是瓶颈。延迟主体在 adb 链路与 Android 输入子系统（DESIGN §11 已经写明）。

---

## 9. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| gpui-kit 0.9.x 周更、破坏式改动 | 升级即返工 | 锁死版本；升级集中在独立提交；UI 层薄、业务逻辑不依赖 GPUI 类型 |
| 托盘要走 tray-icon + 独立消息循环 | 首次接入容易踩消息循环/线程派发/DPI 的坑 | spike 第 2 项先做；把托盘动作统一包装成事件回到 GPUI 主线程 |
| 中文 IME 在 GPUI 上的实际表现 | 输入类界面不可用 | spike 第 1 项一票否决 |
| 画布交互重写 | 摆位手感回归 | 保留 `lib/geometry.ts` 的纯函数逻辑，逐条移植 + 黄金用例对照 |
| 视觉验证流程断层 | 无法再出 docs/screenshots 那种证据 | 用 gpui 离屏渲染 + 固定窗口尺寸截图，纳入 CI |
| 全量重写期间功能冻结 | 3–5 个月不能发版 | 方案 B 采用长分支 + 双实现并行，不冻结现行版本 |

---

## 10. 参考

- GPUI 官方站点：<https://gpui.rs/>
- GPUI Kit 文档（中文）：<https://gpui-kit.com/zh-CN/docs>
- Zed for Windows 公告：<https://zed.dev/blog/zed-for-windows-is-here>
- crates.io：`gpui` 0.2.2（2025-10-22）、`gpui-component`/`gpui-kit` 0.9.1（2026-11-09）、`gpui-pre` 0.3.12
- 本项目：`docs/TAURI_MIGRATION.md`（上一轮 Qt→Tauri 的选型与结论）、`docs/DESIGN.md` §10/§11、`docs/PITFALLS.md`
