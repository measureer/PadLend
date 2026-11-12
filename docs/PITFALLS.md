# PadLend 开发者陷阱（Pitfalls）

> 面向维护者、贡献者和发版负责人：这里记录**已经踩过一次、且很容易再踩**的坑。
> 普通用户排障请看 [TROUBLESHOOTING.md](TROUBLESHOOTING.md)；架构与关键实施决策看 [IMPLEMENTATION_NOTES.md](IMPLEMENTATION_NOTES.md) 和 [DESIGN.md](DESIGN.md)；版本变更看 [CHANGELOG.md](CHANGELOG.md)。

**怎么读**

- 每条固定为：现象 → 触发条件 → 根因 → 修法 → 如何验证/回归 → 相关文件:行 → 版本，最后一句「再犯的代价」。
- 版本列写的是该行为**引入/定型**的版本。标注 1.2.0 的条目是 1.2.0 的约定；若代码与本文不一致，以代码和 CHANGELOG 为准。
- 行号基于 1.2.0 开发工作区，重构后请顺手更新；找不到行号时按函数名/类名搜索。
- 代码与文档的 `文件:行` 默认相对**仓库根**；只有 Markdown 链接用同目录相对路径。
- 「遮罩」默认指 Python sidecar 创建的 `PadLendCursorMaskWindow`，不是 Tauri 窗口。这个心智模型错了，后面所有判断都会错。

**按症状速查**

| 症状 | 先看 |
| --- | --- |
| 透明遮罩不消失、光标不见、点不动 | [3.1](#31-交还后遮罩残留透明层盖住屏幕) [3.2](#32-1-1-2-建窗重入产生未跟踪的-topmost-窗口) |
| 按 Win 键像死机 | [2.1](#21-win-键穿透开始菜单--遮罩--假死) [4.1](#41-按-win-键崩溃未能复现现场证据不足) |
| 托盘退出后进程还在 | [6.1](#61-托盘退出只隐藏窗口进程还在) [6.2](#62-close_to_tray-把窗口生命周期和-sidecar-生命周期缠在一起) |
| 退出后下次启动报「上次未正常退出」 | [4.2](#42-python-关闭被守护线程截断) [4.3](#43-首次运行就报上次未正常退出) |
| 高 DPI / 多屏遮罩错位、落点偏 | [5.1](#51-dpi-感知晚于建窗遮罩只盖住一部分) [5.2](#52-物理坐标与逻辑坐标混用) [5.3](#53-接管中显示器拓扑变化) [5.4](#54-负坐标副屏) |
| 连上平板但输入乱/没反应 | [1.1](#11-幽灵-scrcpy-server-杀不掉) [2.2](#22-心跳把按住冲成松开) [2.4](#24-进入接管没有先补发全松) |
| 安装版缺 jar/adb、sidecar 找不到 | [7.2](#72-sidecar-解析失败回退到开发期-python) [7.3](#73-资源清单双轨安装版与-zip-版不一致) |
| CI 红了但不知道是不是环境问题 | [8.1](#81-无设备环境测试必须-skip-而不是-fail) [8.2](#82-真实窗口测试藏在-padlend_ui_tests-后面) |

---

## 1. adb 与连接

### 1.1 幽灵 scrcpy server 杀不掉

- **现象**：上次会话异常结束后，`adb shell ps` 里仍有 scrcpy server；下一次连接时报端口/UHID 设备异常，或平板端输入完全没反应。
- **触发条件**：会话被强杀、USB 拔线、平板重启后重连；尤其是 Android 17 真机。
- **根因**：Android 17 上 `app_process` 的命令行只有 `com.genymobile.scrcpy.Server`，**不含 jar 路径**。旧清理逻辑按 `scrcpy-server-manual.jar` 做 `pkill -f`，匹配不到真正的进程。
- **修法**：`KILL_PATTERNS` 同时覆盖 Server/CleanUp 类名和 jar 名（兼容旧版）；启动顺序固定为 push → kill → spawn → forward，先清再起；停止后 `dumpsys input` 复检 scrcpy 计数，非 0 则重试。
- **如何验证 / 回归**：连接后执行 `adb shell ps -A | findstr scrcpy`，应为空；强杀 PadLend 再启动，仍应能正常接管；日志中能看到逐 pattern 的 kill 记录，且 `verify_clean` 返回 0。
- **相关文件**：`tablet/server.py:35-40`、`tablet/server.py:225-238`、`tablet/server.py:259-280`、`tablet/server.py:282-295`
- **版本**：1.1.0（真机偏差；清理机制自 0.1.0 存在）
- **再犯的代价**：残留 server 占着 1234-1244 的 forward 端口和旧 UHID 设备，表现为「设备明明连上了但输入乱/没反应」，用户只能拔线重连，问题单也很难复现。

### 1.2 `cleanup=true` 导致每次重连都重推 2.3MB jar

- **现象**：每次重连日志里都有 push jar，传输约 2.3MB；看起来像缓存没生效。
- **触发条件**：每次建立 scrcpy 会话。
- **根因**：server 参数带 `cleanup=true`，scrcpy server 启动即 `unlinkSelf` 删除远端 jar，避免在平板 `/data/local/tmp` 留垃圾。这是**预期行为**，不是 bug。
- **修法**：不要为了「省流量」改成 `cleanup=false`；`push_if_needed` 的 md5 跳过只在文件还存在时有效；要减少重推，应尽量保持同一次 adb 连接，而不是改清理策略。
- **如何验证 / 回归**：连接成功后 `adb shell ls /data/local/tmp/scrcpy-server-manual.jar` 应不存在；重连日志应出现一次 push；把「文件不存在」当正常分支，不要当错误。
- **相关文件**：`constants.py:109`、`constants.py:116-128`、`tablet/server.py:136-167`、`docs/IMPLEMENTATION_NOTES.md:119`
- **版本**：1.1.0
- **再犯的代价**：关掉 cleanup 会换来看似省流量的假优化，实际在平板留下 jar 和旧进程；升级后新旧版本混淆，Android 17 上更难清理。

### 1.3 mDNS 重名 serial 带 ` (2)` 后缀，设备被丢弃

- **现象**：平板无线调试明明在线，设备列表里没有或出现重复项；日志里的 mDNS serial 形如 `adb-xxx (2)`。
- **触发条件**：Android 17 无线调试断开重连后，mDNS 实例名被系统加 ` (2)` 后缀。
- **根因**：把 mDNS 的 `info["serial"]` 直接当 adb serial 用；`adb devices` 里实际是 `adb-xxx`，两者对不上就被过滤掉。
- **修法**：从 `_adb-tls-connect._tcp` 记录的 state 位置回拼 serial；优先使用 `ip:port`；旧 serial 且当前只有一台在线设备时自动切换。
- **如何验证 / 回归**：无线调试反复断开/重连，UI 列表与 `adb devices -l` 始终一致；单元测试覆盖带后缀、带空格、旧格式三种 serial。
- **相关文件**：`tablet/mdns.py:23`、`tablet/mdns.py:188-209`、`docs/CHANGELOG.md:588-591`
- **版本**：0.2.4
- **再犯的代价**：用户只能手动输 IP，或者连到另一台设备；重连在真机上时好时坏，最后被归因成「无线调试不稳定」。

### 1.4 退出时 `kill-server` 误杀别人共享的 adb

- **现象**：关闭 PadLend 后，Android Studio / logcat 的 adb 连接也断了。
- **触发条件**：本机已有其他工具在 5037 端口跑着 adb server，PadLend 退出时执行 `kill-server`。
- **根因**：旧实现无条件停 server，没有区分「PadLend 拉起的」和「本来就在跑的」。
- **修法**：用 `_SERVER_OWNED_BY_PADLEND` 三态记录归属；只有执行过非 version/kill-server/start-server 且 5037 未监听时才记为 owned；`kill_server` 非 `True` 直接跳过并记 `adb.kill_server skipped reason=not_owned`。
- **如何验证 / 回归**：先手动 `adb start-server`，再启动/退出 PadLend，5037 应仍然活着；清空环境由 PadLend 拉起 server 时，退出应停掉。
- **相关文件**：`tablet/adb.py:59-64`、`tablet/adb.py:86-92`、`tablet/adb.py:570-587`
- **版本**：1.0.0
- **再犯的代价**：影响同机其他开发工具，是「抢占式」体验；用户会以为 PadLend 卸载不干净，或者干脆不敢同时开 Android Studio。

---

## 2. UHID 与输入注入

### 2.1 Win 键穿透开始菜单 + 遮罩 = 假死

- **现象**：接管期间按 Win，开始菜单弹出来但被透明置顶遮罩盖住；鼠标点不到、其他键又被吞掉，用户报「电脑死机/PadLend 崩溃」。松手后仍可能卡在这个状态。
- **触发条件**：CAPTURED 状态按左/右 Win。
- **根因**：`passthrough_keys` 默认包含 `VK_LWIN`/`VK_RWIN`，按下被转发给 Windows 打开开始菜单；与此同时遮罩是 topmost 且拦截点击，其他键由 PadLend 吞掉，于是整台机器看起来「冻住了」。这不是崩溃，但观感和崩溃一样。
- **修法**：1.2.0 在 Router 状态机里无条件吞掉 L/R Win 的 down/up：不查 passthrough、不进 keymap、不发送平板；`_swallowed_win` 锁存到 keyup，避免只吞 down 让系统以为 Win 仍按着；`_win_vk` 兼容 `ev.vk == 0` 时用 `scan & 0xFF` 反推。**不要**用删除默认 `passthrough_keys` 的方式修——IDLE 时 Win 键必须正常工作。
- **如何验证 / 回归**：`tests/test_router.py::test_win_key_swallowed_latched_and_passthrough_when_idle`；手动在接管中按 Win，开始菜单不应出现，松手后单击/拖拽正常；交还后 Win 键恢复。
- **相关文件**：`core/router.py:111`、`core/router.py:230`、`core/router.py:1874-1886`、`core/router.py:1924-1931`、`config.py:119-123`、`tablet/keymap.py:32-33`
- **版本**：1.2.0
- **再犯的代价**：这是最容易被当成「程序崩溃」的输入路径：没有栈、日志也正常，用户只会强杀进程；强杀之后又留下 3.1 的遮罩残留，一个坑变成两个。

### 2.2 心跳把按住冲成松开

- **现象**：拖拽/选中文本时突然松开；长按被中断；文件拖到一半掉下。
- **触发条件**：平板端周期性心跳/状态包在按住鼠标时发出。
- **根因**：心跳只发 `[0,0,0,0]`，Router 把 `buttons=0` 当成真实状态覆盖，于是一次心跳就「松开」了。
- **修法**：心跳必须携带当前按键位图；`_neutral_until_release` 在回中/换边期间保持按住；不要把「无位移」编码成「无按键」。
- **如何验证 / 回归**：按住左键拖 5 秒不松，在平板上持续移动，电脑端应保持拖拽；断线重连后以最新一条全量报告为准，不能残留旧按键。
- **相关文件**：`core/router.py:999`、`core/router.py:1320-1325`、`docs/CHANGELOG.md:546`
- **版本**：0.2.8
- **再犯的代价**：拖拽文件/选文本时莫名断开，用户可能丢数据；这类「偶发」最难复现，最后往往被当成平板触控问题。

### 2.3 滚轮 120 单位 vs HID 1 格

- **现象**：平板滚一格，电脑滚了 120 格；或者几乎不动、反向滚动会回跳。
- **触发条件**：把 Windows 原始滚轮单位直接发给 UHID。
- **根因**：Windows 是 120 单位/格，HID 是 1 detent/格；整数除法还会丢掉不足一格的余数。
- **修法**：`_take_wheel_locked` 累加 `_wheel_accum`，按 `WHEEL_UNITS_PER_DETENT` 取整，余数留到下次；进入/退出/急停时清零，避免跨会话串余数。
- **如何验证 / 回归**：慢速滚 1/3 格多次，总位移应等于累计格数；正向后再反向，余数不应反号；`tests/test_router.py` 的滚轮用例。
- **相关文件**：`core/router.py:192`、`core/router.py:1217-1223`、`tablet/uhid.py:5`、`docs/CHANGELOG.md:556`
- **版本**：0.2.7
- **再犯的代价**：滚动要么飞快要么不动，用户会以为平板坏了；余数处理错还会在反向时「回跳一格」，看起来像幽灵输入。

### 2.4 进入接管没有先补发全松

- **现象**：刚接管就误点/误拖；或者从平板交还后，电脑仍处于「按住」状态，下一次点击变成拖拽。
- **触发条件**：进入接管时电脑物理键正按着；或上次退出/断线时没补发 `buttons=0`。
- **根因**：只发移动、不发全量按键状态；进入时先 `warp` 再发状态，顺序反了。
- **修法**：进入前先 queue `buttons=0` 再 `warp` 到入口点；退出、急停、FAULT、stop 四条路径都补发全松；`emergency_stop` 额外释放遮罩、键盘清零、清 keymap。
- **如何验证 / 回归**：按住鼠标进入接管，电脑端不应出现拖拽；四条退出路径各测一次，日志里应能看到 `buttons=0`；异常断线后重新接管，第一次点击必须是单击。
- **相关文件**：`core/router.py:994-1008`、`core/router.py:416-440`、`core/router.py:463-485`、`docs/CHANGELOG.md:408`、`docs/CHANGELOG.md:424`
- **版本**：0.3.8（1.2.0 仍适用）
- **再犯的代价**：误拖窗口/误选文本；更糟的是交还后系统仍认为左键按着，用户点任何东西都变成拖拽，只能重启。

### 2.5 键盘发送在 Router 锁内阻塞鼠标

- **现象**：快速移动鼠标时卡顿，键盘连发时更明显；平板网络稍慢就整体拖影。
- **触发条件**：在 Router 锁内做平板 socket 写。
- **根因**：socket 写可能阻塞，Raw Input 线程等锁，鼠标事件在系统队列里堆积。
- **修法**：发送移到锁外（`_sender` 队列），锁内只改状态和入队；发送线程命名并加 try/except 兜底；绝不在锁内做 I/O。
- **如何验证 / 回归**：连续移动 + 键盘压测，帧间隔不应抖动；断开平板网络时，电脑本地鼠标仍应响应（接管中表现为延迟，而不是死锁）。
- **相关文件**：`core/router.py:1852-1857`、`docs/CHANGELOG.md:557`
- **版本**：0.2.7
- **再犯的代价**：一个慢 socket 拖垮整条输入链路；用户感知是「电脑卡了」，几乎不会归因到平板连接。

---

## 3. Windows 遮罩与光标

### 3.1 交还后遮罩残留，透明层盖住屏幕

- **现象**：鼠标交还后，屏幕上仍有一层透明遮罩：光标不显示、点击无效、窗口无法聚焦；有时重启 PadLend 才恢复。
- **触发条件**：1.1.2 在 RPC 线程池线程里调用 `release/hide`；或 sidecar 进程卡死但没退出。
- **根因**：遮罩窗口属于 **Python sidecar 进程**（类名 `PadLendCursorMaskWindow`，标题 `PadLend Cursor Mask`），不是 Tauri 窗口。1.1.2 的窗口只在创建它的线程处理 `WM_CLOSE`，而 RPC 线程池线程没有消息泵 → `DestroyWindow` 永远不会执行，窗口留了下来。**进程边界要记牢**：sidecar 真正退出时 Windows 会销毁它的所有窗口（已用 `os._exit` + `FindWindowW` 返回 `None` 验证）。所以看到残留，只可能是二选一：sidecar 还活着但卡住；或窗口创建后没有被跟踪、销毁路径找不到它。
- **修法**：用专用线程 `padlend-mask-win`（daemon，自带 `PeekMessage` 消息泵）创建/销毁所有遮罩；销毁直接向窗口线程投递 `DestroyWindow`；投递失败时先 `ShowWindow(SW_HIDE)` 把屏幕抢回来，再 `PostMessageW(WM_CLOSE)` 兜底；`release()` 幂等，可重复调用。
- **如何验证 / 回归**：`tests/test_mask.py` 的跨线程释放用例；交还后用 Spy++ / `EnumWindows` 搜类名应为 0；强杀 sidecar 后窗口应立刻消失（反过来证明进程边界）；连续进出 20 次不应残留。
- **相关文件**：`win/mask.py:101-102`、`win/mask.py:578-601`、`win/mask.py:645-654`、`win/mask.py:968-993`、`docs/CHANGELOG.md:9-17`、`docs/CHANGELOG.md:21-25`
- **版本**：1.1.2
- **再犯的代价**：用户电脑「看得见但点不动」，只能强杀；强杀又可能留下 sidecar 子进程，问题从「一个窗口」升级成「一组进程」。

### 3.2 1.1.2 建窗重入产生未跟踪的 topmost 窗口

- **现象**：屏幕上叠了多层遮罩；交还后仍有残留；日志出现「遮罩线程 5.0s 未响应，退回调用线程执行」。
- **触发条件**：1.1.2 创建窗口时 DWM/开始菜单卡住，超过 `_MASK_THREAD_TIMEOUT_S`（5s）。
- **根因**：`create_mask_window` 把 `CreateWindowExW` + `SetLayeredWindowAttributes` + `ShowWindow` + `SetWindowPos(HWND_TOPMOST)` 全放在一个阻塞调用里，`hwnd` 要等函数返回才记录；`_run_on_mask_thread` 超时后在**调用线程**重跑创建，而原任务稍后也在遮罩线程跑完 → 第二个 topmost 窗口无人跟踪，释放时找不到它。
- **修法**：1.2.0 拆分创建与配置：`create_mask_window` 只建窗并返回 `hwnd`；`configure_mask_window` 再做 alpha/Show/SetWindowPos/回读 topmost。`_create_windows` 拿到 `hwnd` 后**先 append 到 `self._windows` 再 configure**，这样配置阶段卡住也能被释放路径找到；超时不再回调用线程重跑，直接抛 `TimeoutError`（任务已被取走时明确说「仍在执行」）。
- **如何验证 / 回归**：`test_create_timeout_does_not_build_a_second_window`、`test_configure_failure_tears_down_registered_window`；人为在 configure 前 sleep，`EnumWindows` 应只有 1 个类名窗口；超时后进程应仍能退出，不留窗口。
- **相关文件**：`win/mask.py:473-501`、`win/mask.py:503-532`、`win/mask.py:906-949`、`win/mask.py:1051-1087`、`tests/test_mask.py:586`、`tests/test_mask.py:605`
- **版本**：1.2.0
- **再犯的代价**：未跟踪的 topmost 窗口没有任何句柄可释放，只能杀进程；多层遮罩会让 DWM 合成变慢，表现为全屏闪烁/卡顿。

### 3.3 系统光标永久隐藏（ShowCursor 计数）

- **现象**：退出 PadLend 后 Windows 光标永久消失，重启才恢复。
- **触发条件**：多次进出接管；或在非创建线程调用 `ShowCursor`。
- **根因**：`ShowCursor` 是**进程级计数**，`ShowCursor(FALSE)` 必须等量 `TRUE` 才能恢复；加上跨线程 `DestroyWindow` 失败，计数永远回不来。
- **修法**：彻底移除 `ShowCursor`。遮罩窗口在 `WM_SETCURSOR` 里 `SetCursor(None)` 并 return 1；`set_cursor_hidden` 只调 `SetCursor`/`LoadCursorW`；`release()` 无条件 `ClipCursor(None)`（`ImportError` 静默，属于解释器关机）。
- **如何验证 / 回归**：`test_set_cursor_hidden_never_touches_show_cursor`；连续进出 20 次后光标仍在；退出后切到任务管理器/其他程序，光标应正常。
- **相关文件**：`win/mask.py:17`、`win/mask.py:628-638`、`win/mask.py:645-654`、`docs/CHANGELOG.md:479-482`
- **版本**：0.3.4（1.2.0 仍适用）
- **再犯的代价**：系统级光标消失是「P0 观感」故障，用户会直接卸载；而且重启才能恢复，口碑损失远大于功能缺陷本身。

### 3.4 topmost 假成功：任务栏压住遮罩、光标重新出现

- **现象**：接管期间任务栏/开始菜单盖住遮罩，光标重新出现；或点击穿透到桌面。
- **触发条件**：其他 topmost 窗口抢 Z 序；`SetWindowPos` 返回成功但实际没生效。
- **根因**：只调一次 `SetWindowPos` 并相信返回值；窗口样式里没有 `WS_EX_TOPMOST`。
- **修法**：创建样式自带 `WS_EX_TOPMOST`；`configure_mask_window` 回读 `window_topmost` 校验，失败重试一次；每次复用遮罩重新置顶；watchdog 每秒检查，`topmost=False` 时 `_reassert_topmost`，光标可见时 `_refresh_cursor`，日志限流为 `mask.repair topmost=... visible=... active=...`。
- **如何验证 / 回归**：打开任务栏/开始菜单再接管，遮罩应仍在最上层；日志不应持续刷 `mask.repair`；把遮罩手动 `SetWindowPos(HWND_NOTOPMOST)` 后，1 秒内应被 watchdog 拉回。
- **相关文件**：`win/mask.py:61`、`win/mask.py:503-532`、`win/mask.py:1218-1241`、`win/mask.py:1243-1253`、`docs/CHANGELOG.md:34-35`
- **版本**：1.1.1
- **再犯的代价**：遮罩失效等于「接管期电脑还能被误操作」，是数据安全问题；用户不会报「遮罩没置顶」，只会报「点了别的窗口」。

---

## 4. 崩溃与失效安全

### 4.1 「按 Win 键→崩溃」未能复现，现场证据不足

- **现象**：用户报告按 Win 键后 PadLend 崩溃/电脑死机；但日志目录没有 `crash-*.log`、没有 `faulthandler-*.log`，WER 和事件日志里也没有 PadLend 记录。
- **触发条件**：接管中按 Win（与 2.1 同一条路径）。
- **根因**：**没有拿到真实崩溃栈，未能复现**。当时的屏幕会话已锁定，无法回放；现有证据只能证明「输入路径会把机器带进假死外观」，不能证明进程真的崩了。把 2.1 的 Win 键修复当成「崩溃已修复」是过度归因。
- **修法**：1.2.0 从输入层消除该路径（CAPTURED 期间吞 Win，见 2.1），同时保留 `crash-*`/`faulthandler-*` 作为判定依据。文档和问题单要写「未能复现/证据不足」，并列出残余风险：锁屏/远程桌面下 Raw Input 与遮罩行为可能不同；低完整性进程的钩子/托盘可能被系统拒绝；真崩溃可能被 WER 吞掉而没有落盘。
- **如何验证 / 回归**：按 2.1 做手动回归；若再次收到报告，要求提供 `%APPDATA%\PadLend\logs\` 全目录（含 `padlend.log`、`crash-*`、`faulthandler-*`）、WER 报告/内存 dump、复现录屏，以及复现时是否锁屏/远程桌面、按的是左 Win 还是右 Win。这些证据才能把「假死」和「真崩溃」分开。
- **相关文件**：`core/router.py:1874-1886`、`core/crash.py:71-97`、`TROUBLESHOOTING.md`（日志位置）
- **版本**：1.2.0
- **再犯的代价**：把「未复现」写成「已修复」会关闭错误的问题单；下次真崩溃时没有基线证据，无法定位，也无法判断是不是同一个问题。

### 4.2 Python 关闭被守护线程截断

- **现象**：退出后 `exit.clean` 没更新，下次启动报「上次未正常退出」；或者 sidecar 已退出但 adb 没停、遮罩没释放。
- **触发条件**：退出时 Python 进程被较快终止，`_clean` 还在跑。
- **根因**：1.1.2 及更早，`service/api.py` 用 **daemon 线程** `service-shutdown` 执行 `_clean`，主线程不 join；解释器退出会直接截断 daemon 线程，收尾只做了一半。
- **修法**：1.2.0 teardown 同步化、幂等：`Runtime.stop` 加锁 + 标志，`service.shutdown` 串行化，调用方等待完成；重复调用直接返回。顺序固定为 keyboard_hook → router → cursor_watch → rawinput → session → mask → raw_debug → adb。
- **如何验证 / 回归**：连续启动/退出 20 次，`exit.clean` 每次更新、任务管理器无 `padlend-service.exe`、无 adb forward 残留；`app.shutdown` 后日志应有 `service.stopped`；重复调用 shutdown 不应报错。
- **相关文件**：`service/api.py:103`、`service/api.py:194-214`、`main.py:247-284`、`service/server.py:269-272`
- **版本**：1.2.0
- **再犯的代价**：「退出不干净」会累积成僵尸 sidecar/端口占用，下一次启动连不上；用户只能重启电脑，且问题看起来像「PadLend 坏了」。

### 4.3 首次运行就报「上次未正常退出」

- **现象**：全新安装第一次启动，日志就写「检测到上次未正常退出，执行强制清理」；或者每次启动都触发清理。
- **触发条件**：配置目录为空，首次启动。
- **根因**：1.1.2 及更早，`_stale_cleanup` 只看 `config.yaml` 是否存在来判断 `had_config`；而 `service/api.py` 在首次启动时会**先 save 默认配置**，等 cleanup 跑的时候 `had_config` 已是 True，于是「有配置 + 没有 exit.clean」被当成异常退出。
- **修法**：1.2.0 改为由调用方在写配置之前判定 `first_run` 并传入：`had_config = not first_run and C.config_path().exists()`；首次启动直接 touch marker 返回，不告警、不清理。
- **如何验证 / 回归**：删除 `%APPDATA%\PadLend` 后首次启动，日志不应出现该 warning；正常退出后重启也不应出现；只有强杀后重启才应出现。
- **相关文件**：`main.py:125-176`、`service/api.py:128-139`
- **版本**：1.2.0
- **再犯的代价**：首启就「强制清理」会污染日志、掩盖真正的异常退出；如果清理碰到用户文件，还会变成数据损失。

### 4.4 崩溃后没有证据链

- **现象**：程序「消失了」，没有弹窗、没有日志，用户只能描述「突然没了」。
- **触发条件**：原生崩溃、线程内未捕获异常、启动阶段失败。
- **根因**：没有统一钩子；frozen 模式下 stderr 不可见；异常只打到控制台。
- **修法**：1.0.0 起安装 `sys.excepthook` / `threading.excepthook` + `faulthandler`（`all_threads=True`），分别写 `crash-YYYYmmdd-HHMMSS-<pid>.log` 和 `faulthandler-*.log`；启动失败弹窗并落盘；日志目录 `%APPDATA%\PadLend\logs`，2MB 轮转保留 3 个。
- **如何验证 / 回归**：在主线程/子线程各抛一次未捕获异常，检查 `crash-*`；用本地测试触发原生崩溃，检查 `faulthandler-*` 是否含全线程栈；确认日志不会无限增长。
- **相关文件**：`core/crash.py:1-15`、`core/crash.py:71-97`、`core/crash.py:100-120`、`main.py:409-438`、`docs/TROUBLESHOOTING.md:7-28`
- **版本**：1.0.0
- **再犯的代价**：没有栈就只能猜；用户报「崩溃」时，无法区分 Python 异常、原生崩溃，还是 2.1 的假死。

---

## 5. DPI 与多显示器

### 5.1 DPI 感知晚于建窗，遮罩只盖住一部分

- **现象**：高 DPI 下遮罩只盖住屏幕一部分，或整体偏移；鼠标坐标和遮罩对不上。
- **触发条件**：进程未设置 Per-Monitor V2 就创建窗口/读坐标。
- **根因**：默认 DPI 虚拟化，`GetCursorPos`/窗口矩形返回逻辑坐标，`CreateWindowExW` 又按逻辑尺寸缩放；两者叠加就错位。
- **修法**：启动最早处调用 `set_dpi_awareness`：优先 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，失败回退 `SetProcessDPIAware`；遮罩线程再 `SetThreadDpiAwarenessContext(PMv2)`；所有几何统一用物理像素。
- **如何验证 / 回归**：125%/150%/200% 缩放各测一次，遮罩边缘应与显示器边缘重合；跨不同 DPI 显示器拖动，落点不偏；启动日志应能看到 DPI 模式。
- **相关文件**：`main.py:91-99`、`win/dpi.py:86-120`、`win/mask.py:523-539`
- **版本**：1.0.0
- **再犯的代价**：高 DPI 是默认配置，不测就等于大多数用户遮罩错位；修起来却只需一行初始化，属于「最贵的低级错误」。

### 5.2 物理坐标与逻辑坐标混用

- **现象**：多屏不同缩放时，鼠标落点偏一段；穿屏后坐标跳到另一块屏的错误位置。
- **触发条件**：混用 `GetCursorPos`、Qt/窗口逻辑坐标、`GetMonitorInfo` 物理矩形。
- **根因**：Windows 的窗口坐标受 DPI 虚拟化影响，只有 `rcMonitor` 和 Raw Input 是物理像素；一旦在某个环节用了逻辑坐标，后面全错。
- **修法**：统一走 `win/dpi.py` 的物理坐标 API；`win/desktop.py` 只用 `rcMonitor` 的并集；Router 内不做 DPI 换算；遮罩用物理矩形 `SetWindowPos`。
- **如何验证 / 回归**：双屏 100% + 150%，在每块屏边缘各穿一次；`tests/test_mask.py:458` 负原点用例；日志打印的坐标应为物理值，不能出现 1.25/1.5 的缩放痕迹。
- **相关文件**：`win/dpi.py:146-200`、`win/desktop.py:9-21`、`win/mask.py:453-471`、`win/mask.py:503-532`
- **版本**：1.0.0
- **再犯的代价**：偏几十像素就会点错按钮；跨屏场景下用户无法判断是平板延迟还是坐标错误，问题单会反复来回。

### 5.3 接管中显示器拓扑变化

- **现象**：接管中拔掉/接入显示器、改分辨率后，鼠标锁在旧屏幕，或落点全错。
- **触发条件**：显示器拓扑变化（插投影、切扩展/复制、改缩放）。
- **根因**：缓存的显示器列表/吸附段失效；`ClipCursor` 仍指向旧矩形。
- **修法**：`_check_desktop_locked` 比较显示器签名，变化时 `desktop.refresh()`；CAPTURED 状态走 `_begin_return_locked("display")` 主动交还，再校验 placement；不要静默继续用旧坐标。
- **如何验证 / 回归**：接管中切换投影模式/改分辨率，应自动交还且遮罩释放；重新接管落点正确；日志应有 `display` 原因的 return。
- **相关文件**：`core/router.py:1584-1604`、`core/router.py:2143-2157`、`win/desktop.py:314-323`
- **版本**：1.0.0（1.2.0 仍适用）
- **再犯的代价**：用户演示/开会时插投影必触发；锁在旧坐标会让鼠标「消失」，只能强杀。

### 5.4 负坐标副屏

- **现象**：副屏在主屏左侧/上方时，遮罩不显示或位置镜像错。
- **触发条件**：虚拟桌面原点为负。
- **根因**：代码假设 `(0,0)` 是左上角；RECT 归一化/宽高计算用错符号。
- **修法**：以虚拟桌面并集为坐标系，允许负 origin；宽高用 `right-left`/`bottom-top`；裁剪矩形按物理坐标；测试覆盖负原点。
- **如何验证 / 回归**：把副屏拖到主屏左侧，接管应正常；`test_clip_rect_handles_negative_origin_monitor`；遮罩边缘不应超出副屏。
- **相关文件**：`win/desktop.py:9-21`、`win/mask.py:453-471`、`tests/test_mask.py:458`
- **版本**：1.0.0
- **再犯的代价**：双屏用户里「副屏在左」很常见；遮罩缺失时点击会穿透，属于安全问题。

---

## 6. Tauri 与托盘

### 6.1 托盘退出只隐藏窗口，进程还在

- **现象**：点托盘「退出」后窗口消失，但进程还在；再启动提示已在运行、端口被占。
- **触发条件**：`ui.close_to_tray=true`（**非默认**，用户在设置/向导里显式开启）时点退出。默认是 `false`（`config.py:141`），所以这是「开了托盘模式」才会踩到的坑。
- **根因**：1.1.2 及更早，托盘退出走前端 `window.close()`；`lib.rs` 的 `CloseRequested` 看到 `close_to_tray` 就 `prevent_close()` + `hide()`，根本没有退出。窗口关闭和进程退出被混成了同一个动作。
- **修法**：1.2.0 增加显式 `quit_app` 命令（`src-tauri/src/commands.rs:50-53`）：先 `bridge.shutdown()` 同步停 sidecar，再 `app.exit(0)`；托盘 `quit` 直接走同一路径（`src-tauri/src/tray.rs:68-72`），前端 `quitApp()` 也改为调它（`src/lib/actions.ts:309-315`），不再 `window.close()`。关窗（X）仍按 `close_to_tray` 分支：`false`（默认）停 sidecar 后退出，`true` 才隐藏。
- **如何验证 / 回归**：托盘退出后任务管理器无 `PadLend.exe`/`padlend-service.exe`，`exit.clean` 更新，下次启动正常；`close_to_tray=true` 时点 X 只应隐藏、托盘可唤回，默认 `false` 时点 X 应退出且 sidecar 同步停（`lib.rs:52-67` 的两个分支各测一次）。
- **相关文件**：`src/lib/actions.ts:309-315`、`src-tauri/src/tray.rs:33`、`src-tauri/src/tray.rs:68-72`、`src-tauri/src/commands.rs:50-53`、`src-tauri/src/lib.rs:52-67`、`config.py:141`
- **版本**：1.2.0
- **再犯的代价**：「退不掉的程序」会被用户强杀，sidecar 和遮罩一起失控；这也是 4.2 的常见诱因。

### 6.2 close_to_tray 把窗口生命周期和 sidecar 生命周期缠在一起

- **现象**：关窗后 sidecar 继续跑，遮罩/热键仍在；或者反过来，sidecar 被 8s 超时强杀，Python 没机会清理（遮罩释放最坏要等 5s）。
- **触发条件**：关窗 vs 退出的语义混用；`bridge.shutdown` 等 8s（80×100ms）后强杀。
- **根因**：窗口生命周期和 sidecar 生命周期绑在一起；`CloseRequested` 的 hide 分支只隐藏窗口，不通知 sidecar。
- **修法**：语义固定为：hide 不动 sidecar；quit 必须先 `service_shutdown` 再 `app.exit`；bridge 只在 quit 时发 `app.shutdown`，8s 内没退出才 `child.kill()`（随后 taskkill /T /F 清进程树）——这是最后手段，不能当正常路径。
- **如何验证 / 回归**：关窗后 sidecar 应仍在（托盘可用、热键正常）；托盘退出后 sidecar 应在 8s 内退出（正常通常远快于此），日志有 `service.stopped`；强杀 Tauri 窗口不应留下孤儿 sidecar（或明确记录这是兜底）。
- **相关文件**：`src-tauri/src/bridge.rs:151-167`（8s 上限）、`src-tauri/src/bridge.rs:151-188`、`src-tauri/src/lib.rs:52-67`、`service/server.py:244-247`、`service/server.py:269-272`
- **版本**：1.1.0（1.2.0 明确语义）
- **再犯的代价**：两个生命周期缠在一起，任何一边改动都会制造「幽灵 sidecar」或「退出被截断」，而且两边日志各说各话。

### 6.3 托盘图标「看起来在」，实际 Shell 没接受

- **现象**：托盘图标不显示，但代码认为注册成功；或退出后图标残留。
- **触发条件**：Explorer 重启、低完整性进程、`Shell_NotifyIconW` 失败。
- **根因**：0.3.x（Qt 托盘）的 `isVisible()` 只反映请求状态，不反映 Shell 是否真的接受，错误被吞掉。1.x 起托盘改由 Tauri Rust 侧创建（`src-tauri/src/tray.rs`），Python 的托盘代码已退役；`win/trayprobe.py` **当前没有任何 runtime 调用点**，只是保留的手工诊断模块。
- **修法**：需要验证 Shell 是否接受图标时，**手工**调用 `win.trayprobe.probe_icon()`：它用 message-only 窗口挂一个探针图标再摘掉，直接读 `Shell_NotifyIconW` 返回值与 `GetLastError`（`native_ok=False err=5` = 被 Explorer 拒绝，常见于低完整性）；顺带读注册表里的 `ExecutablePath`。探针结果只用于诊断，不要当用户可见状态，也不要以为应用启动时会自动跑它；若要重新接入运行时，必须先补调用点和测试。
- **如何验证 / 回归**：在诊断脚本/REPL 里手工调 `probe_icon()` 能看到 `native_ok` 与错误码；**正常运行时日志不应出现 `native_ok`**（没有调用点）；Tauri 托盘本身用 `tray.rs` 的菜单项验证（含「退出 PadLend」）；打包后 hidden import 仍应包含 `win.trayprobe`。
- **相关文件**：`win/trayprobe.py:1-14`（docstring 明说无 runtime 调用）、`src-tauri/src/tray.rs:1-33`、`tools/build_sidecar.py:57`、`docs/TROUBLESHOOTING.md:53`
- **版本**：0.3.18（历史 Qt 行为；1.x 起托盘在 Rust，本模块仅诊断）
- **再犯的代价**：托盘是唯一入口，图标没了用户只能杀进程；「假成功」会让日志一直显示正常，排查方向全错。

### 6.4 第二实例不唤回原窗口

- **现象**：双击图标没反应，或者开出第二个实例抢热键/端口。
- **触发条件**：已有实例在托盘/隐藏；Tauri 单实例插件与 Python 互斥量不一致。
- **根因**：只做进程级互斥，没做「唤回」；或者两套单实例机制各自为政。
- **修法**：Python 用 `Global\PadLend.SingleInstance` 互斥量 + `Global\PadLend.Activate` 命名事件；Tauri 侧用 single-instance 插件负责 show + set_focus + emit `app:present`；第二实例启动后立刻退出。
- **如何验证 / 回归**：隐藏窗口后再次启动，应恢复原窗口而不是新进程；任务管理器只有一个 `PadLend.exe`；托盘菜单仍可用。
- **相关文件**：`win/single_instance.py:1-8`、`win/single_instance.py:51-55`、`src-tauri/src/lib.rs:22-30`、`docs/CHANGELOG.md:150`
- **版本**：1.0.2
- **再犯的代价**：双实例会争抢 Raw Input 钩子和 adb forward，表现为输入重复/错乱，极难排查。

---

## 7. 打包与发布

### 7.1 版本号三处手工同步，只校验了一处

- **现象**：安装包/关于页/exe 属性版本不一致；release 通过了，但 `tauri.conf.json` 还是旧版。
- **触发条件**：发版只改 `padlend/__init__.py`。
- **根因**：版本号有三个来源：`padlend/__init__.py`、`package.json`、`src-tauri/tauri.conf.json`；`release.yml` 只校验 tag 与 Python `__version__`。
- **修法**：发版清单三处一起改；`tools/release.py` 以 `padlend.__version__` 为唯一事实源；建议在 CI 增加三处一致性检查，避免只靠人记。
- **如何验证 / 回归**：`python -c "import padlend; print(padlend.__version__)"`、`package.json`、`tauri.conf.json` 三处一致；安装包属性/关于页/文件名一致；tag 与三处一致。
- **相关文件**：`padlend/__init__.py:11`、`package.json:4`、`src-tauri/tauri.conf.json:4`、`.github/workflows/release.yml:36-44`、`tools/release.py:34`
- **版本**：1.0.0（1.2.0 仍适用）
- **再犯的代价**：用户装了两个版本无法区分；自动更新/问题单对不上版本，排障先输一半。

### 7.2 sidecar 解析失败，回退到开发期 Python

- **现象**：安装后启动报「服务未找到」，或直接回退到 `python -m service.server` 然后失败。
- **触发条件**：打包漏了 sidecar，或 exe 改名/移动。
- **根因**：`resolve_service` 有一串候选路径，最后回退到开发期 Python；frozen 环境没有 Python，回退只会把「缺文件」变成「看不懂的 Python 错误」。
- **修法**：打包时 `externalBin` 带 target triple 后缀；`tauri.conf.json` 与 `build_sidecar.py` 的输出名必须一致；发版前在**干净机器**上跑安装包，而不是只跑 `tauri dev`。
- **如何验证 / 回归**：解压 zip 后直接运行 `PadLend.exe`，日志应显示 sidecar 路径；把 sidecar 改名应报明确错误而不是静默回退；安装版同样验证。
- **相关文件**：`src-tauri/src/bridge.rs:195-248`、`src-tauri/tauri.conf.json:37`、`tools/build_sidecar.py`
- **版本**：1.1.0
- **再犯的代价**：开发机永远复现不了，用户一装就废；这类问题通常到发布后才发现。

### 7.3 资源清单双轨，安装版与 zip 版不一致

- **现象**：zip 版能用，NSIS 安装版缺 jar/adb；或反之。
- **触发条件**：只测一种分发形态。
- **根因**：`tauri.conf.json` 的 `resources` 与 `tools/release.py` 的 `collect_files` 是两份清单，容易漂移。
- **修法**：两份清单尽量从同一份 manifest 生成；至少发版前各装一次；jar 推送前按 `vendor/SHA256SUMS` 校验，缺失就报 E103，不要静默继续。
- **如何验证 / 回归**：安装版和 zip 版分别检查 `vendor/`、`platform-tools/`、`THIRD_PARTY_NOTICES`；启动后能连设备；校验失败路径有明确提示。
- **相关文件**：`src-tauri/tauri.conf.json:45-54`、`tools/release.py:101-134`、`tablet/server.py:84-99`
- **版本**：1.1.0
- **再犯的代价**：缺 jar 时用户看到的是 E103/连接失败，不会知道是打包问题；两种形态的 bug 报告还会互相矛盾。

### 7.4 Windows 上 npm 需要 shell，控制台需要 UTF-8

- **现象**：CI 在 Windows 上 `npm` 调不起来；或中文路径/日志乱码。
- **触发条件**：`subprocess` 直接执行 `npm.cmd`；控制台代码页不是 UTF-8。
- **根因**：Windows 的 `.cmd` 不是 PE，`CreateProcess` 不能直接执行；Python 默认用 locale 编码。
- **修法**：`shell=True` 执行 npm；`_force_utf8` 设置 UTF-8 环境；CI 的 `env` 里显式设 `PYTHONIOENCODING: utf-8`。
- **如何验证 / 回归**：在中文 Windows 上跑 `python tools/release.py --build`；CI 日志无 `UnicodeEncodeError`；路径含中文也能打包。
- **相关文件**：`tools/release.py:37-43`、`tools/release.py:55-62`、`.github/workflows/ci.yml:13-36`
- **版本**：1.1.0（1.2.0 仍适用）
- **再犯的代价**：发版脚本只在作者机器上能跑；错误信息还是 `FileNotFoundError: npm`，误导排查方向。

---

## 8. 测试与 CI

### 8.1 无设备环境测试必须 skip 而不是 fail

- **现象**：没连平板时 `pytest` 一片红；CI 无法区分「环境缺失」和「真 bug」。
- **触发条件**：测试直接假设设备/窗口存在。
- **根因**：没有在收集阶段判断环境；或 skip 条件写在用例内部，fixture 先炸了。
- **修法**：设备测试用 `skipif` 并打印原因；真实窗口测试用 `PADLEND_UI_TESTS=1` 显式开启；无设备时单测必须全绿（见 `docs/IMPLEMENTATION_NOTES.md:51`）。
- **如何验证 / 回归**：不接设备跑 `pytest -q`，应只有 skip；接设备再跑同一套，skip 变为 pass；skip 原因要能看懂缺什么。
- **相关文件**：`tests/test_mask.py:469-478`、`docs/IMPLEMENTATION_NOTES.md:51`
- **版本**：1.0.0
- **再犯的代价**：红色 CI 会被忽略，真回归混在噪声里；久而久之没人看测试结果。

### 8.2 真实窗口测试藏在 PADLEND_UI_TESTS 后面

- **现象**：遮罩相关回归在 CI 永远不跑；1.1.2 的残留问题就是这类测试缺口。
- **触发条件**：默认跳过真实窗口测试。
- **根因**：创建真实 topmost 窗口会干扰桌面/CI 无桌面会话，只能用环境变量隔离。
- **修法**：本地/专用机用 `PADLEND_UI_TESTS=1` 跑全套；CI 至少跑无窗口的 mock 版；新增遮罩逻辑必须同时有 mock 单测和 UI 测试。
- **如何验证 / 回归**：`PADLEND_UI_TESTS=1 pytest tests/test_mask.py -q` 应全绿；CI 日志里能看到 skip 数量，不能静默 0；两类测试都要能在无设备时独立运行。
- **相关文件**：`tests/test_mask.py:469-478`、`tests/test_mask.py:559-640`
- **版本**：1.2.0
- **再犯的代价**：遮罩是最高频的 P0，跳过它等于把最危险的路径交给用户当测试。

### 8.3 时序测试靠真等和 sleep，必然 flaky

- **现象**：遮罩超时测试偶发失败/挂起 5 秒；CI 变慢。
- **触发条件**：真的等 `_MASK_THREAD_TIMEOUT_S`；或依赖 sleep 排序。
- **根因**：超时值是常量，测试没注入更短的值；线程调度不可控。
- **修法**：用 monkeypatch 把超时改到 0.05s 级；用 `Event` 同步而不是 sleep；断言「没有建第二个窗口」而不是「日志里没有某句话」。
- **如何验证 / 回归**：连续跑 50 次不 flaky；`pytest -q tests/test_mask.py` 总时长应短于 2s（不含 UI 测试）；失败时能稳定复现。
- **相关文件**：`tests/test_mask.py:605-640`、`win/mask.py:98`
- **版本**：1.2.0
- **再犯的代价**：flaky 测试会被 `-x` 跳过或重跑掩盖，真正的重入 bug 又回来了。

### 8.4 CI 依赖与 tag 校验

- **现象**：desktop job 在 python/frontend 失败时仍跑；tag 推了但 release 没发。
- **触发条件**：workflow 依赖写错；tag 与版本不一致。
- **根因**：`desktop` 需要 `[python, frontend]` 都成功；release 只在 tag 且版本匹配时发布。
- **修法**：保持 `needs: [python, frontend]`；发布前本地跑 `ruff check . && pytest -q && npm test`；tag 必须等于 `padlend.__version__`。
- **如何验证 / 回归**：故意让 python job 失败，desktop 应 skipped；推 `v1.2.0` 前确认 `__version__` 已是 1.2.0；release 日志里能看到 zip 和 sha256。
- **相关文件**：`.github/workflows/ci.yml:58-99`、`.github/workflows/release.yml:4-6`、`.github/workflows/release.yml:36-44`、`.github/workflows/release.yml:68-81`
- **版本**：1.1.0
- **再犯的代价**：坏包被发出去，或 tag 打好了却没有产物；回滚要删 tag/重发，release 记录会脏。

---

## 相关文档

- [TROUBLESHOOTING.md](TROUBLESHOOTING.md)：用户向排障（错误码、日志、配置恢复）
- [IMPLEMENTATION_NOTES.md](IMPLEMENTATION_NOTES.md)：关键实施决策与真机偏差
- [DESIGN.md](DESIGN.md)：架构、启动/退出序列、坐标与遮罩设计
- [CHANGELOG.md](CHANGELOG.md)：每个修复的原始记录
- [../README.md](../README.md)：项目入口
