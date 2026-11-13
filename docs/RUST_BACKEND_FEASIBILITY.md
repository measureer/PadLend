# PadLend 后端 Python → Rust 可行性评估

> 评估基线：**1.4.0**（HEAD `cb240b3`，2026-11-14）。本文只讨论**后端引擎**（`core/` `win/` `tablet/` `service/` `main.py` `config.py` `constants.py`），
> 不含 UI/GPUI 选型（那部分见 `docs/GPUI_RUST_FEASIBILITY.md`）。
>
> **结论先行：技术可行，没有硬阻塞。但这不是"替换语言"，而是把 9,934 行有效代码用 Rust 重写一遍，并把 `docs/PITFALLS.md` 里 38 条真机行为重新验一遍。**
> 成本 **80–111 人日**（另加 3–5 人日 spike）+ **2–4 周真机回归**；收益是"去 Python 运行时 / 单进程 / 类型安全的 Win32"，**不是性能**。

---

## 1. 结论

| 问题 | 结论 | 依据 |
|---|---|---|
| 能做到吗 | **能，无硬阻塞**。55 个 Win32 API、scrcpy 应用层协议、mDNS、YAML 配置，全部有 Rust 对应物；运行时依赖只有 PyYAML + psutil | §3 |
| 工作量多大 | **9,934 行有效代码**（另有 3,823 行注释/文档，是规格不是冗余）→ 约 12–15k 行 Rust；**80–111 人日** + spike 3–5 | §2、§5 |
| 风险在哪 | 不在语法，在**行为**：38 条踩坑 + 411 个测试 + 真机时序 | §4 |
| 拿到什么 | 单一 exe、无 PyInstaller/Python 运行时、删掉 2,067 行 RPC 胶水、Win32 编译期类型检查、热路径无 GIL | §6.1 |
| 拿不到什么 | **延迟不会变好**（瓶颈在 adb 链路与 Android 输入子系统，DESIGN §10）；内存不一定下降 | §6.2 |
| 现在该做吗 | 只有当"去 Python 运行时"本身是产品需求时才做；否则先做 §6.2 的测量和 §9 的门禁 | §7、§9 |

---

## 2. 基线（实测，非估计）

### 2.1 代码量

| 层 | 文件 | 总行 | **有效代码行** | 说明 |
|---|---|---|---|---|
| `core/` | 10 | 3,547 | **2,781** | router 1,854 占 67%，其余是纯逻辑 |
| `win/` | 10 | 4,287 | **3,178** | mask 942 / rawinput 634 / keyboard_hook 355 / desktop 358 |
| `tablet/` | 12 | 4,446 | **3,264** | session 1,089 / adb 582 / locator 277 / mdns 244 |
| `service/` | 6 | 2,463 | **2,067** | RPC 门面 + DTO，**并进外壳后整层消失** |
| 顶层 | 3 | 1,481 | **1,108** | config 521 / constants 190 / main 397 |
| **后端合计** | 41 | **16,224** | **12,398** | 注释与 docstring 占 24%（3,823 行是行为规格，重写时先搬注释再搬代码） |
| 测试 | 29 | 7,831 | **5,797** | **411 个 test 函数**，`tests/fakes.py` 470 行结构化替身 |
| Rust 外壳 | 7 | 743 | — | bridge 244 / audio 254 / tray 104 / lib 80 / commands 54 |

> 必须移植的是 **core + win + tablet + config + constants ≈ 9,934 行**；
> `service/` 2,067 行在"并进外壳"路线下删除、在"Rust sidecar"路线下重写成约 900 行 Rust（§7）。

### 2.2 复杂度分布

**5 个文件吃掉了 57% 的有效代码**：

| 文件 | 有效代码 | 为什么难 |
|---|---|---|
| `core/router.py` | 1,854 | 5 状态状态机（合法迁移表是冻结契约）+ 累加器 + 1000Hz sender + supervisor + 健康/旋转/DPI 变化处理 |
| `tablet/session.py` | 1,089 | scrcpy 握手顺序、UHID 创建、指针速度/熄屏超时的保存与恢复、重连 |
| `win/mask.py` | 942 | 每个显示器一块 topmost 分层窗口 + 独立消息泵线程 + ClipCursor + 看门狗 |
| `win/rawinput.py` | 634 | 隐藏窗口 + RIDEV_INPUTSINK 注册与生命周期 + 原始报文解码 |
| `tablet/adb.py` | 582 | 子进程超时编排、serial 选择、jar 推送、forward/reverse |

---

## 3. 为什么可行：移植面的六个事实

### 3.1 运行时依赖面极窄

`requirements.txt` 只有 **PyYAML + psutil**（psutil 还是"可选"）。Rust 侧对应 `serde`+`serde_yaml_ng`（YAML 必须保持兼容，见 §8 的契约清单）与 `sysinfo`/`windows`。
**没有任何第三方 C 扩展、没有 numpy、没有数据库驱动、没有 async 框架。**

### 3.2 Win32 面是 55 个函数，`windows` crate 全覆盖

`ctypes` 出现 **499 次**、`windll` 16 次、手工声明的 struct（`_RAWINPUT`、`_WNDCLASSEXW`、`_CURSORINFO`…）十余个。实际调用的 Win32 函数只有 **55 个**：

```
AllowSetForegroundWindow ClipCursor CloseHandle CreateEventW CreateMutexW CreateWindowExW
DefWindowProcW DestroyWindow DispatchMessageW EnumDisplayMonitors FindWindowW GetAsyncKeyState
GetClipCursor GetCurrentThreadId GetCursorInfo GetCursorPos GetExitCodeProcess GetLastError
GetLastInputInfo GetMessageW GetModuleHandleW GetMonitorInfoW GetRawInputData GetSystemMetrics
GetWindowLongW GetWindowThreadProcessId LoadCursorW LoadIconW OpenEventW OpenMutexW OpenProcess
PeekMessageW PostMessageW PostQuitMessage PostThreadMessageW RegisterClassExW RegisterClassW
RegisterHotKey RegisterRawInputDevices SendMessageW SetCursor SetCursorPos SetEvent
SetLayeredWindowAttributes SetWindowPos SetWindowsHookExW Shell_NotifyIconW ShowWindow
TerminateProcess timeBeginPeriod timeEndPeriod TranslateMessage UnhookWindowsHookEx UnregisterHotKey
WaitForSingleObject
```

这 55 个全部在 `windows` crate 的 `Win32_UI_Input`/`Win32_UI_WindowsAndMessaging`/`Win32_Foundation`/`Win32_UI_Shell`/`Win32_Media` 覆盖范围内。
**顺带的白拿收益**：现在这些 ctypes 结构体布局和 `argtypes` 全靠手工维护（`HANDLE` 当 `c_void_p`、`LONG`/`DWORD` 混用、返回值忘记查 `GetLastError`），
Rust 侧编译期就报错；`Result<T, windows::core::Error>` 强制处理失败分支。

### 3.3 并发模型是"线程 + 阻塞 IO"

统计：`threading` **115** 处、`socket` **106** 处、`subprocess` **34** 处、`asyncio` **0** 处。
这说明**不需要**引入 async 运行时思维——`std::thread` + `parking_lot::Mutex`（或 `crossbeam` 无锁通道）+ `std::net` 就能 1:1 对应；
只有 adb 子进程的超时读取用得上 `wait_timeout` 或 `tokio::process`。**这是移植里最容易的一段。**

### 3.4 外部协议都是应用层字节协议

- **scrcpy 控制协议**：`tablet/protocol.py` 190 行（`DeviceMessage` 编解码），定长字段 + 变长文本，`struct.pack` 手写；
- **UHID 报告**：`tablet/uhid.py` 95 行，鼠标 4 字节 / 键盘 8 字节定长，纯位运算；
- **mDNS**：`tablet/mdns.py` 244 行，自己发 DNS 查询包（Windows 的 adb 常常没有 mDNS 守护进程），`socket`+`struct` 手写。

三者都是**无依赖的字节级代码**，Rust 版本只会更短（不需要 `struct.pack` 的格式串，直接写 `[u8; 8]`）。
注意：**scrcpy-server 的 jar 与协议版本是绑死的**（`vendor/scrcpy-server-v5.0.1.jar`），移植**不升级**协议版本。

### 3.5 411 个测试是可执行规格，但也是第二份代码

`tests/fakes.py`（470 行）已经把所有外部依赖抽象成了替身：`FakeClock`（精确推进时间）、`FakeSession`、`FakeDesktop`、`FakeMask`、`FakeConfig`。
这意味着 Rust 侧可以沿用同一套注入点（trait + 测试实现），**不需要为了可测性重构架构** —— 这是这个代码库最大的一笔可移植资产。

代价：411 个测试要跟着重写（`cargo test`），有效代码 5,797 行；其中 `test_router` 68 个、`test_mask` 40 个、`test_protocol` 32 个、`test_rawinput` 31 个是硬骨头。**测试移植量必须计入估算，它不免费。**

### 3.6 音频已经不在 Python 里

`tablet/audio.py`（107 行）只做一件事：把 scrcpy 的裸 PCM 从 socket 搬到本机 `audio.rs`（cpal）的 sink；
解码、重采样、播放**已经在 Rust 侧**。所以移植清单里**没有音频编解码**——这是最容易误判的一块，先排除掉。

---

## 4. 为什么难：三个不随语言消失的部分

### 4.1 38 条踩坑是"行为"，不是"语法"

`docs/PITFALLS.md` 38 条（1.x 有 7 条、2.x 6 条、3.x 4 条、4.x 4 条、5.x 4 条、6.x 5 条、7.x 4 条、8.x 4 条）。
其中**至少 20 条只有真机能验证**，例如：

- 2.2 心跳把按住冲成松开；2.4 进入接管没先补发全松；2.3 滚轮 120 单位 vs HID 1 格；
- 2.6 音频通道握手顺序写反 → 控制通道收不到事件（**只有真机能验**）；
- 3.3 `ShowCursor` 计数不平衡导致光标永久隐藏；3.4 topmost 假成功（任务栏压住遮罩）；
- 5.1 DPI 感知晚于建窗；5.4 负坐标副屏。

**Rust 不会让这些消失**，它们会被原样搬过去，并且必须再验一遍。真正能"按定义消灭"的只有一类：**双进程 + 解释器退出**带来的
4.2（守护线程被截断）、6.2（窗口与 sidecar 生命周期耦合）、2.5（锁内 IO 阻塞鼠标）——这三条只有在**并进单进程**的路线里才会消失，保留 sidecar 的路线里会原样保留。

### 4.2 五个硬模块 = 57% 的代码，且都不能"先绕过去"

它们互相咬合：`rawinput` 的回调 → `router` 状态机 → `sender` 1000Hz → `session.socket`。中间任何一个没到位，整条链就跑不起来。
所以**不存在"先换 30% 就能上线"的切法**（见 §7 的路线对比）。

### 4.3 Rust 会新增两类风险

1. **低级键盘钩子的死锁 = 全系统键盘卡死**。`SetWindowsHookEx(WH_KEYBOARD_LL)` 的回调超时（`LowLevelHooksTimeout`）后 Windows 会**静默摘钩**；
   如果 Rust 侧在钩子线程里阻塞在一把和 sender 共享的锁上，表现是"键盘全局失灵几百毫秒"。Python 用 `deque + Lock` 已经踩过（PITFALLS 2.5），
   Rust 的 `Mutex` 更容易让人写出无意识阻塞 —— 钩子线程必须走**无锁通道**，不能碰共享锁。
2. **`panic = "abort"` 会杀掉整个应用**。`src-tauri/Cargo.toml` 的 release profile 就是 `panic = "abort"`（第 35 行）。
   现在引擎崩了只是 `service:disconnected`，UI 还能提示；并进外壳后任何 `unwrap()` 都是整窗消失。
   要并入就必须改成 `unwind` + 引擎边界 `catch_unwind`，或者接受"崩就崩"。

---

## 5. 逐模块分解与估算

估算口径：一名熟悉 Rust 与本项目的工程师；**含**对应测试的移植、`cargo test` 对齐；**不含**真机回归（单列 2–4 周）。

| # | 模块 | 有效代码 | 难点 | 人日 |
|---|---|---|---|---|
| 1 | 纯逻辑：`config` `constants` `geometry` `metrics` `errors` `events` `state` `logsetup` `crash` `diagnostics` | 1,638 | 1:1 翻译；**YAML 字段与默认值必须逐字段兼容**（§3.6） | 11–15 |
| 2 | Win32 工具层：`desktop` `dpi` `hotkeys` `single_instance` `cursor_watch` `trayprobe` | 1,247 | `windows` crate 绑定、错误分支、消息循环习惯 | 9–12 |
| 3 | 遮罩：`win/mask.py` | 942 | 独立消息泵线程所有权、topmost 看门狗、分层窗口、多屏/DPI | 8–12 |
| 4 | 捕获：`win/rawinput.py` + `win/keyboard_hook.py` | 989 | 隐藏窗口寿命、`RIDEV_INPUTSINK`、钩子线程不得阻塞 | 7–10 |
| 5 | 状态机：`core/router.py` | 1,854 | 5 状态迁移、溢出测量、sender 1000Hz、supervisor、旋转/桌面变化 | 17–23 |
| 6 | 连接：`adb` `locator` `mdns` `server` | 1,326 | 子进程超时、mDNS 手写包、jar 推送与 `forward` | 8–11 |
| 7 | 会话：`session` `protocol` `uhid` `keymap` `cursor` `simulated`（+ `audio` 107 行转发层） | 1,831(+107) | 握手顺序、UHID 生命周期、6KRO、指针估计器 | 12–16 |
| 8 | 集成：RPC 层（重写或删除）、打包、CI、发布脚本 | — | 两条路线二选一 | 8–12 |
| | **合计** | **9,934** | | **80–111**（不含 M0 spike 3–5） |

外加：**真机回归 2–4 周**（可与编码部分并行，不能压缩）。

> 交叉验证：`docs/GPUI_RUST_FEASIBILITY.md` §6.1 给的 70–100 人日与本文的 80–111 同量级；差额来自本文把测试移植、CI/打包、RPC 层显式计入了。

---

## 6. 收益与代价（诚实版）

### 6.1 白拿的

| 收益 | 量化 |
|---|---|
| 无 Python 运行时 | sidecar 内的解释器 + stdlib 提取消失；构建链少掉 PyInstaller、`.venv` 与 `tools/build_sidecar.py`（127 行手写 hidden-import 清单，PITFALLS 7.2 的源头） |
| 少一个进程 | `service/*` 2,067 行 + `main.py` 编排 + `bridge.rs` 244 行 + `commands.rs` 54 行 **可删**（并进外壳路线） |
| 类型安全的 Win32 | 55 个 API、十余个 struct 布局从"手工 `argtypes`"变成编译期检查；`GetLastError` 不再靠人记得查 |
| 热路径无 GIL | 1000Hz sender 与 Raw Input 回调不再争 GIL；锁粒度可控 |
| 一类事故按定义消失 | PITFALLS 4.2 / 6.2 / 2.5（只在单进程路线） |
| 单一语言栈 | 外壳（Rust）+ 引擎（Rust）+ 前端（TS），未来 GPUI 迁移无语言断层 |

### 6.2 打折的 / 未必的

- **延迟不会变好**。DESIGN §10 的设计目标是 p50 < 25ms / p95 < 40ms，并写明"延迟主体在 adb 链路与 Android 输入子系统"，控制通道 1000Hz × 6 字节 ≈ 6KB/s 根本不是瓶颈。**当前没有证据表明 Python 是瓶颈**；移植后能改善的是**抖动方差**，不是绝对值。
- **内存不一定下降**。Rust 省掉解释器堆，但窗口/图形栈与 adb 子进程照旧；同类项目的实测差异通常在几十 MB 量级，需自己量。
- **单 exe 体积**。现在 `--onefile` 每次启动都要把解释器 + stdlib + `adb.exe`(8.27MB) + `scrcpy-server.jar`(2.32MB) 解压到 `%TEMP%\_MEIxxxx`。
  **但这半数是 PyInstaller 的 `--onefile` 造成的，不是 Python 本身**：改成 `--onedir` 或把 adb/jar 移到 Tauri resources，1–2 天就能拿掉大部分启动与体积成本。
  换句话说：**"启动慢/包大"这个理由，先被 §9 的门禁 4 证伪一次，再决定要不要为它重写引擎。**

### 6.3 不划算的路线

| 路线 | 为什么不行 |
|---|---|
| PyO3 嵌 CPython（把 Python 编进 Rust 进程） | 只消掉 IPC，消不掉体积、GIL、`python3xx.dll` 打包与 ABI 绑定；失败模式更差 |
| 只换外围（config/geometry/registry） | 换完仍要带 Python 运行时 + 全部 `win/` `tablet/`，收益 0、成本 4–6 人日 |
| 在热路径中间切一刀（捕获 Rust、路由 Python） | 每个鼠标包都要序列化 + IPC，1ms 预算直接吃掉一半，等于复现 PITFALLS 2.5 |
| 只把 `win/` 绑定层做成原生 DLL 给 Python 调 | 消掉的正是"已经最稳"的那层（ctypes 绑定经 78 次提交打磨）；GIL 与双进程都还在 |

---

## 7. 三条路线

| | A：不动 | B：全量并进外壳 | **B2：Rust sidecar 先行**（推荐） |
|---|---|---|---|
| 做法 | 保持 Python 引擎 | 引擎搬进 Tauri 进程，直接函数调用，删 RPC 与 bridge | 用 Rust 写一个**同协议**的 sidecar（`padlend-core.exe`），外壳按顺序找它；Python 版保留为回退 |
| 人日 | 0 | **76–105**（RPC 层删除，比 B2 省 4–6） | **80–111**（含 RPC 层重写） |
| 可回退 | — | **否**（一次性切换） | **是**（删一个 exe 就退回） |
| 可 A/B 对比 | — | 否 | **是**（同机对比延迟/内存/崩溃率） |
| 崩溃隔离 | 有 | **失去**（`panic=abort` 全崩） | 保留 |
| 双进程老问题 | 有 | 消除 | 保留（4.2/6.2 那一类） |
| 真正的"去 Python" | 否 | 是 | 是（Python 只是不再随包分发） |
| 建议 | 现在 | 只有确定要吃"单进程"红利时才做 | **先走这条，M6 再决定要不要并进去** |

**B2 的关键设计**：Rust sidecar 必须实现**同样的 25 个 RPC 方法名、同样的 NDJSON 语义、同样的事件名**
（`app.bootstrap` `app.shutdown` `config.get/save` `devices.*` `layout.*` `engine.*` `logs.recent` `system.paths` `wizard.check` `diagnostics.*`）。
外壳侧只改 `bridge::resolve_service()` 的查找顺序：`padlend-core.exe` → `padlend-service.exe` → 开发期 `python -m service.server`。
这样**前端、Tauri 侧、既有 RPC 契约全部不用动**，风险被锁在 sidecar 内部。

**对拍方法**（B2 最大价值）：用 Python 版录一批真实会话（`{id,method,params}` → `{id,ok,result}` / `{event,data}`），
作为 golden trace 在 CI 里回放给 Rust 版；`engine.state` 的状态迁移序列、`devices.scan` 的输出结构、`config.get` 的默认值必须逐字节等价。

---

## 8. 若决定做：里程碑与门禁

| 里程碑 | 内容 | 出口标准（不过就不许进下一阶段） | 人日 |
|---|---|---|---|
| **M0** | 骨架 + `windows` crate 验证：DPI、消息泵、Raw Input 计数 | 等价于 `python main.py --debug-raw`：移动鼠标计数正确、不丢事件 | 3–5 |
| **M1** | 纯逻辑层（`config`/`geometry`/`metrics`/`uhid`/`keymap`/`protocol`） | 对同一份 `config.yaml` 解析结果逐字段一致；`cargo test` 覆盖 §5 模块 1 的 pytest 用例 | 11–15 |
| **M2** | Win32 层（desktop/dpi/mask/rawinput/keyboard_hook/hotkeys/single_instance） | 遮罩能盖满多屏、接管/交还不残留、`ClipCursor` 无泄漏（PITFALLS 3.1–3.4、5.1–5.4 手工过一遍） | 24–34 |
| **M3** | `router` 状态机 + sender + supervisor | 回放录制的 Raw Input 流，状态迁移序列与 Python 版逐条一致；1000Hz 下 p95 不劣于现版 | 17–23 |
| **M4** | `tablet/`（adb/locator/mdns/server/session/cursor） | 真机连接/接管/交还各 3 次；**音频握手（2.6）必须真机过** | 20–27 |
| **M5** | RPC 层 + 打包 + 发布 + A/B | 25 个方法 golden trace 全绿；安装包能装、能卸、能升级（保留 `%APPDATA%\PadLend`） | 8–12 |
| **M6**（可选） | 并进外壳，删 `bridge.rs` 与 `service/` | `panic=unwind` + 边界 `catch_unwind`；外壳崩溃不影响引擎线程 | 5–8 |

**可回退点**：M1/M2/M4 每个里程碑结束都能出「Rust 版可运行但没有 UI 入口」的中间态；M5 之后才允许切换默认。

**必须保持不变的对外契约**（否则老用户升级即坏）：
`%APPDATA%\PadLend\config.yaml` 的字段与默认值、`pointer_state.json`/`screen_timeout.json` 的 `{serial,value}` 格式与路径、
`Global\PadLend.SingleInstance` 互斥体与 `Global\PadLend.Activate` 事件名、RPC 方法名与事件名、`vendor/` 里的 jar 与 adb 版本。

---

## 9. 决策门禁：什么情况下先别做

1. **说不清要解决哪个具体问题时，不做。** "少一门语言""看起来更现代"不是收益；先把当前 Python 版的问题量化（崩溃率、启动时间、安装体积、杀软误报、p95 延迟），量化不出问题就不该重写。
2. **没有连续 3 个月、也没有第二个人时，不做。** 80–111 人日按每周 5 天算就是 4–5.5 个月；而本仓库 90 天里有 **78 次提交**，长分支会被不断 rebase 拖死。
3. **没有真机回归手段时，不做。** 38 条踩坑里至少 20 条只有真机能验；如果现在没有一台能反复插拔的平板 + 验收脚本（`tools/acceptance.py`），重写等于盲飞。
4. **如果目标只是"包更小/启动更快"，先做 1–2 天的测量**：把 `--onefile` 改 `--onedir`、把 `adb`/`jar` 移到 Tauri `resources`，量一下能回收多少。**能回收大头就不要重写。**

**反过来说，什么时候值得做**：当（a）"不依赖 Python 运行时"是产品要求（离线机器、企业分发、杀软白名单），
（b）你准备吃"单进程"的红利（4.2/6.2 那一类事故、UI 与引擎同宿主、将来 GPUI），
（c）你能接受 3–5 个月引擎功能冻结——**那么越早越好**：现在是 9,934 行，两年后就是 25,000 行，那时这个决定就做不动了。

---

## 10. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| 钩子线程阻塞 → 全系统键盘卡死 | 灾难级（用户以为键盘坏了） | 钩子线程走无锁通道；禁止与 sender 共享锁；加超时自检 |
| `panic = "abort"` 让引擎崩溃变整窗消失 | 体验倒退 | 并进外壳时改 `unwind` + `catch_unwind`；或保持 sidecar（B2） |
| 双实现漂移（Python 版继续修，Rust 版对不上） | 对拍失效 | 长分支 + 以 golden trace 为唯一真值；冻结引擎改动 |
| 38 条踩坑只验了"能跑"，没验"边界" | 真机回归期爆雷 | 每条踩坑写成一条真机验收脚本，逐条签字 |
| `windows` crate 版本与 MSVC/SDK 组合 | 构建失败 | 本机已验证 rustc 1.96 + MSVC 14.44 + SDK 10.0.26100；锁 `Cargo.lock` |
| 升级兼容（配置/状态文件/互斥体名） | 老用户升级即坏 | §8 的"契约清单"进 CI：用旧版生成的 `config.yaml` 做快照测试 |
| 进度估计过于乐观 | 半途而废 | M0 结束后用实际速率重估一次，超 1.5 倍就停 |

---

## 11. 参考

- 本仓库：`docs/DESIGN.md` §4（时序与预算）、§6.5（状态机）、§6.6（遮罩）、§10/§11（性能口径）；`docs/PITFALLS.md`（38 条）；`docs/TAURI_MIGRATION.md`
- 同类评估：`docs/GPUI_RUST_FEASIBILITY.md` §6（方案 B 逐模块）、§7（为什么只换一部分不成立）
- 待核实（本文未验证）：Rust 版的实际冷启动时间、常驻内存、p50/p95 延迟对比——**只有在 M5 的 A/B 里才能得到**
