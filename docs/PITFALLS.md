# PadLend 开发者陷阱（Pitfalls）

> 面向维护者、贡献者和发版负责人：这里记录**已经踩过一次、且很容易再踩**的坑。
> 普通用户排障请看 [TROUBLESHOOTING.md](TROUBLESHOOTING.md)；架构与关键实施决策看 [IMPLEMENTATION_NOTES.md](IMPLEMENTATION_NOTES.md) 和 [DESIGN.md](DESIGN.md)；版本变更看 [CHANGELOG.md](CHANGELOG.md)。

**怎么读**

- 每条固定为：现象 → 触发条件 → 根因 → 修法 → 如何验证/回归 → 相关文件:行 → 版本，最后一句「再犯的代价」。
- 版本列写的是该行为**引入/定型**的版本。标注 1.2.0/1.2.1/1.3.0/1.4.0 的条目是这些版本的约定；若代码与本文不一致，以代码和 CHANGELOG 为准。
- 行号基于 1.4.0 开发工作区，重构后请顺手更新；找不到行号时按函数名/类名搜索。
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
| 开「转发平板声音」后一直卡在「连接中」 | [2.8](#28-audio-模式下控制通道等-dummy回退重启又丢了-jar) |
| 连上平板但输入乱/没反应 | [1.1](#11-幽灵-scrcpy-server-杀不掉) [2.2](#22-心跳把按住冲成松开) [2.4](#24-进入接管没有先补发全松) |
| 换 Wi-Fi 后设备多出一台 / 自动连接卡 40 秒 | [1.6](#16-把-adb-serial-当设备身份换网就多出一台) [1.7](#17-坏历史地址把首连拖-40-秒) |
| 安装版缺 jar/adb、sidecar 找不到 | [7.2](#72-sidecar-解析失败回退到开发期-python) [7.3](#73-资源清单双轨安装版与-zip-版不一致) |
| 开启「保持平板亮屏」后开发者选项被改 | [1.5](#15-保持平板亮屏改坏了开发者选项的保持唤醒状态) |
| 设置页开关没反应、疑似空开关 | [6.5](#65-只有开关没有实现的显示指针幽灵) |
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

### 1.5 「保持平板亮屏」改坏了开发者选项的「保持唤醒状态」

- **现象**：打开 PadLend 的「保持平板亮屏」后，连接期间平板确实不熄屏；但断开后平板「开发者选项 → 保持唤醒状态」仍处于开启，而且拔掉充电线就恢复熄屏。用户会以为 PadLend 在偷偷改系统设置。
- **触发条件**：`misc.keep_screen_on=true`（默认）时连接平板。旧实现每次连接都执行 `svc power stayon true`，并改写 `settings global stay_on_while_plugged_in`。
- **根因**：`svc power stayon` 的语义是「充电时保持唤醒」，不是「连接期间保持亮屏」；它改的是全局电源/开发者设置，覆盖范围远超 PadLend 会话。1.1.x/1.2.0 的 `_keep_screen_on` 走的正是这条路（旧行号 `tablet/session.py:603-626`）。
- **修法**：1.2.1 改为只动当前设备的 `settings system screen_off_timeout`：读取原值 → 先原子落盘 `screen_timeout_state.json` → `settings put` 成 5 小时（`18_000_000` ms；原值更长则不动，部分 ROM 可能夹到自己的上限）；`_teardown` 写回原值并删状态文件；进程崩溃时下次启动 `restore_pending_screen_timeout()` 恢复。**绝不碰** `stay_on_while_plugged_in` / `svc power stayon`。读取失败或原值不是数字时直接放弃修改，不猜默认值。设置页文案明确说明会临时修改熄屏时间，用户可关闭。
- **如何验证 / 回归**：连接后 `adb shell settings get system screen_off_timeout` 应为 `18000000`（或被 ROM 夹到自己的上限）；断开/退出后应回到连接前的值；`adb shell settings get global stay_on_while_plugged_in` 全程不变；强杀 sidecar 后重启 PadLend，状态文件应被消费、原值恢复。单测见 `tests/test_release_hardening.py` 的 `test_keep_screen_uses_timeout_not_developer_option`、`test_screen_timeout_recovers_after_crash`、`test_screen_timeout_kept_when_device_offline`。
- **相关文件**：`tablet/session.py:738-786`（连接期设置/还原）、`tablet/session.py:1256-1271`（启动恢复）、`main.py:150-164`（启动恢复）、`src/lib/configSchema.ts:130-133`（设置页文案）
- **版本**：1.2.1（替换 1.1.x–1.2.0 的 `svc power stayon` 实现）；5 小时上限自 1.3.0
- **再犯的代价**：用户平板上的开发者选项被永久改掉，拔线即熄屏；这类「副作用外溢」比功能失效更难被发现，也最容易招来「PadLend 改我系统设置」的投诉。

### 1.6 把 adb serial 当设备身份，换网就多出一台

- **现象**：同一台平板换一个 Wi-Fi、路由器重新分配 IP，或无线调试重启换了端口后，设备页出现两台「不同」设备；旧的一直离线，新的一直被当成陌生设备。自动连接还可能拿着旧地址反复重试。
- **触发条件**：无线 adb 的 serial 是 `IP:端口`（如 `192.168.1.9:5555`），它随网络和调试端口变化；旧实现把它当设备主键。
- **根因**：`adb serial` 只是**当前端点**，不是设备身份。旧 `_publish_devices` 用 serial 去重，`_known_devices` 只存 serial 字符串；mDNS 解析虽然已经拿到 TXT 里的 hardware serial/model（`tablet/mdns.py:206`），但 `discovered_services` 只返回 `(ip, port)`，把身份信息丢了。换网后新 serial 自然被当成新设备。
- **修法**：1.3.0 引入 `devices.json` 注册表（`service/device_registry.py`）：设备记录 `id = hw:<hardware serial>`，没有硬件号时临时用 `ep:<端点>`；一台设备下挂多个端点，端点有 host/port/last_ok/fail_count。拿到 facts 后按读到的 `ro.serialno`（`tablet/session.py:219`，先清洗 unknown/null/0 等无效值）合并；读不到时该记录保持 `ep:` 临时身份，不会瞎猜硬件号；不同硬件序列号即使 MAC 相同也**绝不合并**。mDNS 查询保留 TXT serial/model（`tablet/locator.py:153`），定位时先按硬件序列号匹配新 IP；MAC/ARP 只做最后兜底。前端按稳定 `id` 去重和保持选中（`src/store/appStore.ts:153`），不再用会变的 serial 当 key。
- **如何验证 / 回归**：`tests/test_device_registry.py` 覆盖无线地址变化同设备、临时记录升级、不同硬件同 MAC 不合并、端点被另一设备复用；`tests/test_device_registry_service.py` 覆盖一台设备一行、迁移和布局搬迁；`tests/test_locator.py` 覆盖 mDNS 硬件序列号跨 IP 命中。手动：连上平板 → 换 Wi-Fi → 点「刷新设备」，应仍是一行，连接后 `devices.json` 里同一 `hw:` 记录新增端点。
- **相关文件**：`service/device_registry.py:69`、`service/device_registry.py:282`、`service/api.py:184`、`service/api.py:421`、`service/api.py:714`、`tablet/locator.py:153`、`tablet/locator.py:244`、`src/store/appStore.ts:153`
- **版本**：1.3.0
- **再犯的代价**：用户每换一次网络就多一条幽灵设备，最后只能删记录重配；自动连接拿着旧地址空转，看起来像「无线调试不稳定」，实际是身份模型错了。

### 1.7 坏历史地址把首连拖 40 秒

- **现象**：平板已经换了 IP，或者旧无线端点早已失效，PadLend 启动/重连仍卡很久（约 20–40 秒）才报连接失败；期间界面像没反应，也不会自动换到有效地址。
- **触发条件**：`device.serial` 指向一个已失效的无线端点，且启动时只有这一条候选。
- **根因**：旧 `_select_device` 对一个 serial 调 `adb.wait_for_device(INITIAL_RETRY_WINDOW_S)`，默认窗口 40 秒、每 2 秒重扫；`adb connect` 自身还可能等 20 秒。一个坏地址就把首连拖满整个窗口，且没有「先探活、失败立刻换下一个」的层次。
- **修法**：1.3.0 让 `TabletSession` 接收注册表候选（`tablet/session.py:594`）：当前端点优先，其余按 `last_ok/last_seen` 排序；无线候选先做 0.6 秒 `tcp_open` 预检（`tablet/session.py:71`），通了才 `adb connect`，超时 4 秒（`tablet/session.py:72`）；USB 候选直接查 `adb devices`。候选模式**只认指定 serial**，不再走「只有一台在线设备就换过去」的兜底；连上后校验硬件序列号，不符就 `mark_failure` 并换下一个。只有在没有注册表记录时，才保留旧的 40 秒首连路径。
- **如何验证 / 回归**：`tests/test_session_candidates.py` 覆盖候选去重、预检跳过死地址、错误硬件换下一个、USB 不误连另一台平板；`tests/test_device_registry_service.py` 覆盖候选顺序和 `fail_count`。手动：把 `devices.json` 里当前端点改成一个不存在的 IP，重启后应在约 1 秒内跳到下一个端点，而不是卡 40 秒。
- **相关文件**：`tablet/session.py:71-72`、`tablet/session.py:594`、`tablet/session.py:615`、`tablet/session.py:656`、`service/api.py:480`、`service/api.py:501`
- **版本**：1.3.0
- **再犯的代价**：用户会以为程序启动卡死，反复强杀；强杀又可能留下 UHID/遮罩副作用。更糟的是为了「快一点」去掉校验，直接连到另一台平板。

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

### 2.6 音频通道握手顺序写反，控制通道收不到事件

- **现象**：开启「转发平板声音」后连接失败，或连接成功但鼠标键盘完全没反应、日志里控制 socket 读到的是 PCM 字节。
- **触发条件**：`audio=true` 时仍按旧的单通道逻辑先连控制、或只连一个 socket。
- **根因**：scrcpy 5.0.1 的 `DesktopConnection` 在 `audio=true` 时按 **audio → control** 的顺序 accept；先连的那条被当成音频，控制流被当成裸 PCM，控制通道直接坏掉。注意 dummy 字节只发给**第一条** accept 的 socket（1.7.0 时按「两条都读」处理，正是 2.8 的第一个 bug）。
- **修法**：`TabletSession._connect_socket()` 固定先 `_handshake_socket("音频")`、再 `_handshake_socket("控制")`；只有 `audio=false` 才走单通道旧路径。音频 socket 交给 `AudioRelay` 前设 0.5s 轮询超时；控制 socket 保持发送超时。音频通道建立失败时丢弃服务端并按 `audio=false` 重连一次，不允许可选功能拖死键鼠。
- **如何验证 / 回归**：`tests/test_audio_relay.py::test_connect_socket_uses_audio_then_control_order`、`test_connect_socket_without_audio_keeps_single_channel`、`test_restart_without_audio_discards_server_and_clears_flag`；真机开音频后鼠标/键盘事件仍应正常，关闭音频后走单通道。
- **相关文件**：`tablet/session.py:846-931`（握手与顺序）、`tablet/audio.py:49-121`、`constants.py:113-129`、`src-tauri/src/audio.rs:104-166`
- **版本**：1.4.0
- **再犯的代价**：表现像「PadLend 连上了但键鼠全坏」，很容易误判成 UHID 或平板问题；音频和控制一起抓包时才会发现控制流里是 PCM。

---
### 2.7 auto_connect 与显式连接互相 cancel_start

- **现象**：真机（EasyTier 异地组网）上 `devices.connect` 稳定失败，50s 后报「连接超时」，
  日志是 `router.connect_failed attempt=1..6 code=E101 detail=连接后健康检查失败`；Python 与 Rust 实现表现一致。
- **根因**：`auto_connect=true` 时 supervisor 线程会自动发起首连（`_connect_once`），
  用户点「连接」又走 `router.connect_device()`，后者无条件 `stop_session_for_device()` →
  `cancel_start()` 掐掉**正在握手**的那次 start（E105「启动已取消」），teardown 随即杀掉刚起的
  scrcpy 服务端，显式连接拿到一条死通道。EasyTier 每次 adb 往返 300–700ms 把窗口放大到稳定复现；
  USB 本地太快，通常撞不上。
- **修法**：`connect_device` 里先判断"同一 serial 的连接是否正在进行"，是则**复用**（只刷新重试时间，
  不 stop 不 cancel）；目标 serial 不同才走停机切换。另外，会话尚未拿到 `DeviceFacts` 时的写失败
  不再计入 `CONSECUTIVE_WRITE_FAILURES`（否则启动 1 秒就被打成 E106 FAULT）。
- **补丁的补丁**：第一版只比 serial 相等，仍然失败——启动时的自动首连目标 serial 是**空串**
  （配置里还没写），与显式传入的 serial 不等，于是又被当成"切换设备"去取消。正确判据是
  **进行中目标为空（自动首连）或相等都算同一目标**。空串这一层是最容易漏的。
- **再犯的代价**：无线调试连不上、报错却指向"未授权/看不到设备"，排查方向完全被带偏。

### 2.8 audio 模式下控制通道等 dummy，回退重启又丢了 jar

- **现象**：1.7.0 开启「转发平板声音」后，连接一直停在「连接中」，约 30 秒后报连接超时。日志里音频握手正常（重试几次后「音频通道已建立」），控制握手连续「对端在 dummy 字节前关闭」，接着「音频通道不可用，回退为仅控制通道」，回退之后仍然是几十次同样的握手失败。
- **触发条件**：`forward_audio=true`（服务端起两条 socket），并且第一次握手失败触发了 audio=false 回退。
- **根因**：两个 bug 叠加。
  1. scrcpy 5.0.1 的 `send_dummy_byte` 只发给**第一个** accept 的 socket。audio=true 的顺序是音频→控制，而 `connect_socket()` 对两条通道都读 dummy，控制通道必然白等满 `handshake_retry_timeout`（8s）后被判「音频通道不可用」。
  2. `restart_without_audio()` 重启服务端前没有 `push_if_needed()`。服务端带 `cleanup=true`，被杀时会把 `/data/local/tmp/scrcpy-server-manual.jar` 删掉，新服务端 ClassNotFound 秒退，之后每次握手都被立即关闭，一路耗到 `CONNECT_TIMEOUT_S=30` 才失败。
  两个 bug 叠在一起，才表现为「可选功能失败 → 整次连接失败」。
- **修法**：`handshake_socket(label, expect_dummy)` / `try_handshake(..., expect_dummy)` 只在**该通道确实是第一条连接**时读 dummy——audio=true 时音频读、控制不读，audio=false 时单通道仍读；`restart_without_audio()` 在 `server.spawn()?` 前补 `server.push_if_needed()?`（`kill()` 后 `ScrcpyServer.pushed` 已失效，`push_if_needed` 会用 `adb md5` 判断远端还在不在）。
- **如何验证 / 回归**：`cargo test` 的 `audio_mode_does_not_wait_for_dummy_on_control_channel`（dummy 只在第一条也能连上）与 `audio_failure_falls_back_to_control_only`（断言 `pushes == 2`，回退必须重推 jar）；真机开音频应在 2 秒级连上，日志顺序为「音频通道已建立 → 控制通道已建立 → UHID 已创建」。
- **相关文件**：`padlend-core/src/tablet/session.rs`（`handshake_socket` / `try_handshake` / `connect_socket` / `restart_without_audio`）、`padlend-core/src/tablet/server.rs:224`（`push_if_needed`）、`padlend-core/src/tablet/protocol.rs:9`
- **版本**：1.7.1
- **再犯的代价**：开音频就等于连不上，而且现场证据全指向「对端关闭连接」，很容易去查网络、adb 授权或服务端 jar 版本，绕很远。

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

### 6.5 只有开关、没有实现的「显示指针幽灵」

- **现象**：设置页有「显示指针幽灵」开关，切换后屏幕边缘没有任何变化；用户以为功能坏了，维护者以为只是画得太淡。
- **触发条件**：打开设置 →「杂项」，切换该开关并保存。配置会写入 `ui.show_pointer_ghost`，mock 也返回成功，但运行时没有任何绘制代码。
- **根因**：功能只落地了配置字段、类型、设置页 schema、mock 和文档（`config.py`、`src/api/types.ts`、`src/lib/configSchema.ts`、`src/api/mock.ts`、README/DESIGN），缺少渲染器、状态和事件链路。属于「先暴露 UI、后写实现」的典型空开关。
- **修法**：1.2.1 直接删除该开关及其全部代码与文档承诺；`config.py` 解析旧配置时显式 `pop("show_pointer_ghost")`，旧 `config.yaml` 不会触发「未知配置项」告警。没有新增替代设置。
- **如何验证 / 回归**：`rg "show_pointer_ghost"` 只应命中 `config.py` 的兼容丢弃和对应测试；设置页「杂项」不再出现该开关；带该键的旧配置能正常加载、保存后键消失且无 warning。
- **相关文件**：`config.py:339-348`（旧键静默丢弃）、`tests/test_config.py:14-21`（旧配置兼容测试）、`src/lib/configSchema.ts`（开关已删）
- **版本**：1.2.1
- **再犯的代价**：空开关会让用户反复尝试并怀疑整个设置页；如果以后补实现，旧配置里的 `false`/`true` 还会被误读成用户意图，增加迁移成本。

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
- **1.7.0 更新**：Python 已移除，版本号只剩 `package.json`、`src-tauri/tauri.conf.json`、
  `src-tauri/Cargo.toml`、`padlend-core/Cargo.toml` 四处；发版脚本与 `release.yml` 统一以
  `tauri.conf.json` 为事实源，其余三处靠人同步。
- **再犯的代价**：用户装了两个版本无法区分；自动更新/问题单对不上版本，排障先输一半。

### 7.2 sidecar 解析失败，回退到开发期 Python

- **现象**：安装后启动报「服务未找到」，或直接回退到 `python -m service.server` 然后失败。
- **触发条件**：打包漏了 sidecar，或 exe 改名/移动。
- **根因**：`resolve_service` 有一串候选路径，最后回退到开发期 Python；frozen 环境没有 Python，回退只会把「缺文件」变成「看不懂的 Python 错误」。
- **修法**：打包时 `externalBin` 带 target triple 后缀；`tauri.conf.json` 与 `build_sidecar.py` 的输出名必须一致；发版前在**干净机器**上跑安装包，而不是只跑 `tauri dev`。
- **如何验证 / 回归**：解压 zip 后直接运行 `PadLend.exe`，日志应显示 sidecar 路径；把 sidecar 改名应报明确错误而不是静默回退；安装版同样验证。
- **相关文件**：`src-tauri/src/bridge.rs` 的 `resolve_service()`、`src-tauri/tauri.conf.json` 的 `externalBin`、`tools/build-core.ps1`
- **版本**：1.1.0
- **1.7.0 更新**：Python 回退分支已删除，找不到引擎时直接 `spawn` 失败并把
  `无法启动业务服务 ...` 交给界面显示；`externalBin` 只剩 `binaries/padlend-core`。
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


## 12. 1.7.0 补录：移除 Python 侧踩到的坑（2026-11-15）

> 1.7.0 把 Python 实现与 Python 构建工具链整体删除，发版脚本第一次改用 PowerShell。

### 12.1 `git mv` 到已存在的目录，fixture 被套了两层

- **现象**：迁移后出现 `padlend-core/tests/fixtures/fixtures/router_scenarios.json` 这种双层路径，
  测试找不到向量文件。
- **根因**：目标目录 `padlend-core/tests/fixtures/` 已经存在（放着 `config_*.json`），
  `git mv tests/fixtures <target>` 会把源目录整体塞成子目录。
- **修法**：把内容摊平到 `padlend-core/tests/fixtures/`；`tablet_vectors.rs`、
  `rawinput_vectors.rs`、`logic/router.rs` 三处引用改用 `CARGO_MANIFEST_DIR` 拼
  `tests/fixtures/...`，不再依赖进程工作目录。
- **如何验证 / 回归**：`cargo test --manifest-path padlend-core/Cargo.toml` 通过，
  且在任意 CWD 下运行都通过。

### 12.2 Windows PowerShell 5.1 读无 BOM 的 UTF-8 脚本：中文注释吃掉语法

- **现象**：`powershell -File tools\release.ps1` 报 `Missing expression after ','`、
  `Unexpected token '}'`，报错位置都紧跟在中文注释之后；同一个文件在 `pwsh` 7 下正常。
- **根因**：Windows PowerShell 5.1 按系统 ANSI 代码页读没有 BOM 的 `.ps1`，UTF-8 中文被
  拆成乱码，行尾的括号/反引号被吞掉，于是整段语法失效。
- **修法**：`.ps1` 一律存成 UTF-8 **带 BOM**：
  `[IO.File]::WriteAllText($p, $c, (New-Object System.Text.UTF8Encoding($true)))`。
  注意用编辑器/工具改完文件会丢 BOM，改完要补回去。
- **如何验证 / 回归**：`$null = [ScriptBlock]::Create((Get-Content -Raw $p))` 能解析；
  `powershell -NoProfile -File` 与 `pwsh -File` 都能跑通。

### 12.3 `Join-Path [Environment]::GetFolderPath(...) "..."` 参数绑定失败

- **现象**：`A positional parameter cannot be found that accepts argument 'Microsoft Visual Studio\Installer\vswhere.exe'`。
- **根因**：`Join-Path` 的位置参数不能直接跟方法调用，方法调用没加括号时整串被当成第一个参数。
- **修法**：方法调用加括号：`Join-Path ([Environment]::GetFolderPath("ProgramFilesX86")) "..."`。

### 12.4 `bundle/nsis` 留着旧安装器，`*-setup.exe` 抓到上一个版本

- **现象**：1.7.0 的 `dist/PadLend-1.7.0-setup.exe` 是 20.17 MB——实际是 1.6.0 的包
  （新包 5.59 MB），sha256 自然也对不上。
- **根因**：`tauri build` 输出到 `src-tauri/target/release/bundle/nsis/`，旧版本安装器不会
  被清掉；脚本用 `-Filter "*-setup.exe"` 取 `[0]`，按名字排序 1.6.0 排在 1.7.0 前面。
- **修法**：按版本过滤 `("PadLend_" + $Version + "_*-setup.exe")` 并校验文件名含当前版本；
  `dist/` 里旧的 `PadLend-*` 在打包前清掉，避免被 `make_sha256` 一起算进
  `SHA256SUMS.txt`（1.1.x 遗留产物就是这么混进去的）。

## 13. 1.7.2 补录：音频链路与 adb 连接（2026-11-15）

### 13.1 Windows 上 accept() 出来的 socket 继承 listener 的非阻塞标志

- **现象**：开启「转发平板声音」后平板确实静音了（scrcpy 采音时会把设备调成静音），
  但电脑完全没声音。引擎日志只有一句
  `[tablet.audio] 音频播放器写入失败，后续音频将丢弃: 远程主机强迫关闭了一个现有的连接。 (os error 10054)`，
  每次会话「已转发」的字节数只有几百字节。
- **根因**：外壳的 `AudioServer` 为了能定时检查停止标志，把监听套接字设成了非阻塞
  （`listener.set_nonblocking(true)`）。Windows 上 `accept()` 返回的连接会**继承**这个
  非阻塞标志，于是播放线程里的 `tcp.read()` 立刻返回
  `WSAEWOULDBLOCK (os error 10035)`，`play_connection` 直接返回错误并丢掉连接；
  引擎下一次 `write_all` 就收到 `10054`。1.4.0 起音频一次都没播出去过，只是以前没人
  同时开着音频和控制通道验证。
- **修法**：连接一进来先 `tcp.set_nonblocking(false)`，再用读超时控制等待；
  读循环另外把 `WouldBlock` 当「等 2ms 再读」而不是致命错误。
- **如何验证 / 回归**：`shell.log` 里应看到「收到头部 → 输出设备 → 播放已开始」，
  且引擎的 `[tablet.audio]` 不再出现「播放器写入失败」；`cargo test` 不受影响。

### 13.2 `adb connect` 的链路不随会话 teardown 关闭

- **现象**：点「断开连接」甚至退出 PadLend 之后，`adb devices` 里平板还在，
  看起来「无线调试根本没断开」。
- **根因**：`adb connect host:port` 建立的连接属于 adb server，会话 teardown 只做了
  `pkill` 服务端与 `adb forward --remove`，从来没有 `adb disconnect`。
- **修法**：新增 `padlend-core/src/tablet/adb_links.rs`，`Adb::connect()` 成功时登记
  `host:port`（`already connected` 不登记，表示链路是别人建的），`Adb::disconnect()`
  时注销；`devices.disconnect` 与 `Engine::shutdown()` 调
  `Engine::release_adb_connections()` 逐条 `adb disconnect`（3 秒超时，失败只记日志）。
- **如何验证 / 回归**：断开/退出后 `adb devices` 不再列出该端点；用户手动
  `adb connect` 的端点不会被 PadLend 断开。

### 13.3 GUI 进程里的 eprintln 等于扔进虚空

- **现象**：排查上面两条时，引擎侧服务日志齐全，外壳侧一行都没有——播放器为什么退出、
  选的哪个输出设备、是不是没有音频设备，全都无从判断。
- **根因**：Tauri 外壳是 `windows_subsystem = "windows"` 的窗口进程，没有控制台，
  `eprintln!` 写进空句柄，谁也看不到。
- **修法**：新增 `src-tauri/src/shell_log.rs`，外壳日志写到
  `%LOCALAPPDATA%\PadLend\shell.log`（与引擎的 `service.log` 同目录，超过 1 MiB
  轮转成 `shell.log.1`），并在启动时安装 panic hook。以后外壳侧的问题先看这个文件。
- **如何验证 / 回归**：启动应用后 `shell.log` 至少有一行「===== PadLend x.y.z 外壳启动 =====」。

### 13.4 scrcpy 5.0.1 的音频默认是 opus，不是裸 PCM

- **现象**：13.1 修完，电脑确实出声了，但全是爆音/杂音（像麦克风炸麦）。
- **根因**：scrcpy 5.0.1 服务端音频默认 codec 是 **opus**；PadLend 的服务端命令行只传了
  `raw_stream=true`，那是视频侧的旧开关，5.0.1 已经不认它。于是音频 socket 上是一条
  Ogg Opus 流（旁路抓包首 16 字节为 `50 4C 41 55 80 BB 00 00 02 00 00 00 10 00 00 00`
  接 `4F 70 75 73 48 65 61 64` = `OpusHead`），被播放器当 48 kHz/2ch/16bit 裸 PCM 播。
  数据量也能看出来：12 秒只有 156 KB（约 104 kbps，正是 opus 码率），而裸 PCM 应是
  48000×2×2×12 ≈ 2.3 MB。
- **修法**：`constants::scrcpy_server_args()` 在 `audio=true` 时追加 `audio_codec=raw`，
  服务端改走 AudioRecord 裸流分支、不写 codec 头。修后同样 12 秒收到 1.9 MB，
  首字节直接是采样。
- **如何验证 / 回归**：`server.rs` 的 `command_uses_configured_port_version_and_audio`
  断言命令行含 `audio_codec=raw`；`constants.rs` 的 `scrcpy_server_args_matches_python`
  断言带音频时比不带音频多这一个参数。想复现字节级检查时，用
  `PADLEND_AUDIO_PORT=<端口>` 起引擎、脚本在那个端口收字节，看开头是不是 `OpusHead`。
- **顺带记一笔**：`raw_stream` 保留着没删——它对 5.0.1 无害（未知/视频选项被忽略），
  删掉也不会让音频变 raw；音频侧只认 `audio_codec`。

## 14. 1.7.3 补录：自动连接不经过 devices.connect（2026-11-15）

### 14.1 事实镜像开关只挂在 devices.connect 上，自动首连就成了「已连接但界面未连接」

- **现象**：`auto_connect=true` 打开应用，supervisor 几秒内就把平板连上了（能控制、平板有反应），
  但设备页、标题栏、托盘全显示「未连接」，设备详情一片空白；手动点一次「连接此设备」之后就正常了。
- **根因**：`Engine::facts_ready` 是一道闸门——`poll_facts_once()` 第一行就检查它，没开就直接返回。
  这道闸门只在 `devices.connect`（`service/api.rs` 的 `devices_connect`）里被 `enable_facts()` 打开。
  而 `auto_connect` 的首次连接由 `Router::start()` 里的 supervisor 线程完成，**完全不经过**这个 RPC，
  于是闸门一直关着：`app.bootstrap` 的 `facts` 恒为 `null`、`connected` 恒为 `false`，
  `facts` 事件也永远不推。界面只能显示未连接。
- **修法**：`Engine::open()` 启动 Router 之前，若配置里 `device.auto_connect` 为真就先 `enable_facts()`；
  `devices_connect` 里的那次保留（关掉自动连接后手动连仍要能用）。
- **如何验证 / 回归**：起一个独立引擎（`--config` 指向 auto_connect=true 的临时配置），
  不调任何 RPC，8 秒后发一条 `{"id":1,"method":"app.bootstrap","params":{}}`，
  应答里应是 `connected=true` 且 `facts.serial` 有值；事件流里也应看到 `"event":"facts"`。
- **教训**：凡是「只有某条 RPC 才会打开的开关」，都要确认自动路径（supervisor / 定时器 / 回调）
  是不是也走那条 RPC。这一次的表现是「功能正常、界面骗人」，比功能坏掉更难被发现。

## 15. 1.7.5 补录：托盘菜单的两个「点了没反应」（2026-11-15）

### 15.1 「打开配置…」「打开日志目录…」被 capability 的空 scope 全拒

- **现象**：托盘右键点「打开配置…」，什么都不打开，右下角提示「无法打开：undefined」。
- **根因**：前端用的是 `@tauri-apps/plugin-opener` 的 `openPath()`，最终 invoke
  `plugin:opener|open_path`。这个命令是 scope 敏感的：先按 capability 里该权限的 scope 过滤路径。
  `src-tauri/capabilities/default.json` 里只写了字符串形式的 `opener:allow-open-path`，Tauri 的 ACL
  清单对它的描述是 “Enables the open_path command without any pre-configured scope.” —— 空 scope
  等于什么都不允许，所以任何路径都被拒。
- **为什么提示是 undefined**：Rust 侧返回 `Err(String)`，Tauri 以**裸字符串** reject；前端
  `catch (error) { (error as Error).message }` 对这种值取不到 `message`，于是显示 undefined。
- **修法**：外壳自己加 `#[tauri::command] pub fn open_path(path: String)`（`src-tauri/src/commands.rs`），
  内部直接调 `tauri_plugin_opener::open_path()`，不走 ACL；`src/api/client.ts` 改用这个命令，
  并用 `normalizeError()` 把字符串包成 `Error`。capability 里那行无用的 `opener:allow-open-path` 一并删掉。
- **接着踩的第二个坑**：改完命令后「打开配置…」还是没反应——这台机器上 `.yaml` 根本没有文件关联
  （`cmd /c assoc .yaml` → File association not found）。更坑的是 ShellExecute 对这种情况
  **返回成功却什么都不启动**：`Start-Process config.yaml` 无异常、无窗口、无进程，所以
  「先试系统关联、失败再兜底」的写法永远不会触发兜底（第一版就是这么写的，日志里一条都没有）。
  修法：`open_path` 直接按扩展名判断，文本类（yaml/yml/json/log/txt/ini/md/conf/csv）交给
  `notepad.exe`，目录与其他文件才走系统关联。
  验证问题时一定要区分「命令没跑到」「scope 拒了」「系统静默忽略」三层，不然会一直怀疑代码。
- **教训**：Tauri 2 里凡是「scope 敏感」的插件命令（opener / fs / shell …），只写字符串权限名往往等于
  **空 scope = 全拒**；要么在权限里配 `{ "identifier": ..., "allow": [{ "path": ... }] }`，要么在自己的
  命令里调插件 Rust API。另外：`invoke` 的 reject 值不一定是 Error，统一 `normalizeError` 再取 message。

### 15.2 窗口收在托盘时，托盘里开对话框的菜单项看不见效果

- **现象**：点窗口 X 收进托盘后，右键托盘点「关于」「运行诊断…」「首次设置向导…」，屏幕没反应。
- **根因**：这三项都是把 `openDialog(...)` 交给前端渲染成应用内 Modal，托盘事件里只 emit、没有把窗口
  唤回前台；窗口隐藏时对话框照常渲染，但不可见。
- **修法**：`src-tauri/src/tray.rs` 的 `on_menu_event` 在 emit 前判断 action，属于这三项就先
  `window.show()` + `unminimize()` + `set_focus()`。
- **教训**：托盘菜单里凡是「结果落在主窗口里」的项，点击时都要先把窗口唤回来，否则用户只能判断为坏了。

## 16. 1.7.6 补录：旋转平板，布局页不跟着转（2026-11-15）

### 16.1 画布尺寸从来没跟设备事实同步过

- **现象**：平板转成竖屏后，标题栏/设备页显示「竖屏 · 2136×3200」，但「桌面布局」里的矩形还是横的。
- **根因**：`src/features/layout/DesktopCanvas.tsx` 把平板尺寸放在 `tabletSizeRef`，初值写死
  `DEFAULT_TABLET = { 3200, 2136 }`，**唯一会改它的地方是双击平板**（`onDoubleClick` 里交换一次宽高）。
  引擎通过 `facts` 事件报来的真实宽高只用于文字标签，画布几何一律按写死的那套算，所以旋转永远不生效。
  另外 `orientation` 事件只写了一条日志，没有更新 store 里的 facts，旋转瞬间界面会短暂维持旧方向。
- **修法**：加一个 `useEffect` 监听 `facts.width/height`，变化时写进 `tabletSizeRef` 并 `setSizeNonce`
  触发重算；尺寸变化后再跑一次 `fitRef.current()` 重算视野（否则竖屏平板会跑到画布外）。标签改成读画出来
  的尺寸（`size.width/height`），双击预览时也不会出现「写着竖屏却是横的」。`orientation` 事件同时写 facts。
- **教训**：凡是「引擎报状态、界面画几何」的地方，几何必须从状态推出来；用 ref + 常量默认值兜底时，
  一定要留一条从真实状态同步的路径，否则表现是「状态对了但画面不对」，很容易被误判成引擎没检测到。

### 16.2 GitHub「检查更新」别只靠 REST API

- **现象**：第一版检查更新（只请求 `api.github.com/repos/.../releases/latest`）在真机上总是报
  「GitHub 接口限流，请稍后再试」——403。
- **原因**：未认证的 REST API 每个出口 IP 每小时 60 次，共享网络/代理很容易耗尽；`Invoke-WebRequest`
  与 WebView2 都复现。
- **修法**：主路径改成跟随网页重定向 `https://github.com/<repo>/releases/latest`（200 后 `response.url`
  就是 `/releases/tag/v1.4.0`，不消耗 API 配额），拿到 tag 后用 REST API *尽量*补发布说明与资产列表；
  API 401/403/限流都只是「没有详情」，不影响版本判断。没有资产列表时按本仓库发布命名约定推算
  `releases/download/<tag>/PadLend-<版本>-setup.exe`。
- **教训**：给桌面应用做「查最新版本」时，网页重定向是最便宜可靠的信号源；REST API 只当详情增强。

## 17. 1.7.7 补录：快捷键「能改文本但不生效」（2026-11-15）

### 17.1 三段各自为政：JS 写死、引擎写死、前台双触发

- **现象**：设置页的快捷键输入框能改、能保存（config.yaml 里文本确实变了），但不管改成什么，
  实际生效的永远是 `Ctrl+Alt+S` / `Ctrl+Alt+Q`，并且窗口在前台时按一次等于没按。
- **根因**（三条叠加，只修任何一条都不够）：
  1. `src/hooks/index.ts` 的 `useHotkeys()` 把这两个组合键**写死在 JS** 里，只监听 window keydown，
     窗口收进托盘就完全不响应；
  2. `padlend-core/src/win/hotkeys.rs` 的 `register_all` 也**写死**了 `(0x1001, VK_S)`/`(0x1002, VK_Q)`，
     配置文本从来没有任何代码去 `parse`；
  3. 前台时 JS 与引擎 `RegisterHotKey` **同时**处理同一次按键，切换两次 = 净效果为零。
- **修法**：
  - 全局热键只交给引擎：新增 `HotkeyAction` 枚举（id / config 键 / 中文名 / 取值全挂在一起），
    `install_hotkeys` 遍历它逐个 `parse_combo` 后注册；`config.save` 走 `Engine::apply_live_config`
    → `install_hotkeys`（先 `*lock = None` 让旧 `HotkeyManager` Drop 时 UnregisterHotKey，再重装新的）。
  - 前端删掉 `Ctrl+Alt+S/Q` 分支，只保留 `Ctrl+,`；设置页从「文本框」换成录制控件，
    写出的文本与引擎 `parse_combo` 同一套规则（`src/lib/hotkeys.ts`），非法组合（只 Shift、
    小键盘乘号…）当场拒绝并提示。
  - 另外补了一条：注册被别的程序占用时（`RegisterHotKey` 返回 0x80070581），热键模块只在 stderr 打一行，
    界面上表现为「设置里填了但按不动」。现在 `install_hotkeys` 会把没注册上的动作汇总成一条 HOTKEY 日志，
    明确写「可能已被其他程序占用，换一个组合键再试」。
- **教训**：「配置能改」不等于「配置生效」——凡是配置驱动的行为，必须有一条从配置到运行期的路径，
  而且只能有一个执行者。两处同时处理同一次输入，表现往往不是重复，而是**互相抵消**。

### 17.2 Router 的 `cfg` 曾是不可更新的构造期快照

- **现象**：改鼠标倍率 / 上报频率 / 键盘拦截 / 剪贴板同步之后要重启 PadLend 才生效。
- **根因**：`Router::new` 里把 `Config` 整个存进 `cfg: Config`，运行期没有任何写入口；
  `config.save` 只调 `SessionPort::sync_config`（音频 / 熄屏 / 端口 / 指针速度这几项）。
- **修法**：`cfg: RwLock<Config>` + `pub fn update_config`，`config.save` 之后
  `Engine::apply_live_config()` 依次 `router.update_config`、`session.sync_config`、`install_hotkeys`。
  15 处 `self.cfg.*` 改成 `self.cfg().*`（读锁，中毒取回内层）。
- **教训**：Rust 没有 Python 那种「会话与配置共享同一个可变对象」的便利，**构造期快照就一定要留热更入口**，
  否则界面写着「保存后立即生效」而实际要重启。顺带把「哪些项重连才生效」按引擎真实读取时机标在设置项上。

### 17.3 「关于」从对话框搬成页面后，托盘事件要改切页

- **现象**：托盘「关于 PadLend…」点了没反应（旧实现是 `openDialog('about')`，而 `DialogId` 已删掉 'about'）。
- **修法**：`src/store/events.ts` 的 `handleTrayAction` 改成 `setPage('about')`；`tray.rs` 里本来就有的
  「先 show + unminimize + set_focus」保持不变。菜单项与托盘共用同一套 action，不需要改两处。
- **教训**：把对话框升级成页面时，除了 `PageId`/`App.tsx` 路由，还要把所有 `openDialog('x')` 调用点
  （托盘、菜单、快捷键、向导）一起搜一遍；Token 化的 id 集合是编译期能兜住的，字符串 action 不能。

## 18. 1.7.8 补录：为什么「双击 Ctrl」不能当普通热键（2026-11-15）

### 18.1 RegisterHotKey 不收裸修饰键

- **现象**：用户想把「接管 / 交还」设成连按两下左 Ctrl。设置页的录制控件、校验和引擎的
  `parse_combo` 都要求「修饰键 + 主键」，直接填 `Ctrl` 被拒。
- **根因**：Windows 的热键注册只有 `RegisterHotKey(hwnd, id, fsModifiers, vk)` 一种形态，
  `fsModifiers` 是修饰位、`vk` 必须是主键。按下 Ctrl 而后面没有主键时，系统认为用户正在组组合键，
  **不会**派发 `WM_HOTKEY`；把 `vk` 填成 `VK_CONTROL` 也不会得到「只按 Ctrl 就触发」的效果。
- **修法**：双击改由**键盘钩子的按键序列**识别，并且刻意和热键路径分开：
  - 配置照旧是字符串（`Ctrl+Ctrl`），`win::double_tap::parse_double_tap` 只认「两个相同修饰键」；
  - 判定放在 `win::double_tap::DoubleTapMatcher` 里：只统计按下、同族修饰键、间隔 ≤ 400ms；
  - 真正的动作 `std::thread::spawn` 出去执行，钩子回调里只做一次 `try_lock` + 比较。
- **教训**：给桌面应用做「组合键」功能时先想清楚底层 API 的能力边界；`RegisterHotKey` 表达不了
  「双击」「连按」这类序列，硬塞只会得到「注册成功但永远不触发」。这类需求属于输入观察，不属于热键注册。

### 18.2 双击识别必须被普通按键打断

- **坑**：最初只按时间窗判断，结果「先按 Ctrl、再按 C（复制）、400ms 内再按 Ctrl」被当成双击，
  接管状态在正常打字时乱跳。
- **修法**：`DoubleTapMatcher::feed` 对任何**非修饰键**的按下直接清空等待状态；换修饰键
  （Ctrl 之后按 Alt）也重新计数。左右 Ctrl 视为同一族（用户不会在意自己按的是哪一边）。
- **教训**：状态机式的手势识别，要显式定义「什么事件作废」。只定义「什么事件成立」的写法，
  在真实输入里几乎一定会误判。

### 18.3 钩子回调里不要做动作，只做判定

- **约束**：低级钩子回调超时（默认 300ms）会被 Windows **静默摘钩**，PITFALLS 2.5 早有记录。
- **做法**：双击命中后不直接切状态、不发 RPC，而是 `Weak<Engine>::upgrade()` + `thread::spawn`，
  把 `fire_hotkey` 丢到普通线程；回调里只有 `try_lock`（绝不阻塞等锁）和原子操作。
- **教训**：所有「在钩子里顺便做点事」的想法都要先问一句「这件事会不会阻塞」；按键序列识别可以放钩子里，
  执行动作不行。

## 19. 1.7.10 补录：指针还在屏幕中部就交还 PC

| 项 | 内容 |
| --- | --- |
| 版本 | 1.7.10 |
| 相关文件 | `padlend-core/src/logic/router.rs`（`drain` / `sender_frame`）、`padlend-core/src/constants.rs`（`PACKETS_PER_FRAME`） |

### 19.1 现象与证据

- **现象**：接管平板后把指针往上推，平板光标明明还在屏幕中上部（约 30%–60% 高度），控制权就交还给了 PC。
  偶发，同一块平板有时正常、有时提前，所以用户描述成「有时候不知道为什么」。
- **插桩**：`PADLEND_ROUTER_DEBUG=1` 下在 `sender_frame` 里打印 `send.frame n=<本帧条数> burst=<其中 burst 条数> dx/dy`，
  在 `enter_captured` 里打印 `cap.burst queued=<队列长度> pin=<pin 条数>`。真实日志：
  `cap.burst queued=22 pin=13` → `send.frame n=22 burst=21 dx=-465 dy=-2443`，
  即**入口的对齐报文（warp + 顶边 pin）22 条全挤在同一帧里发出**。
- **根因**：Linux evdev 每个设备的环形缓冲只有 `EVDEV_BUF_PACKETS = 8` 个 packet，写满后内核**丢最旧的包**，
  并给读端一个 `SYN_DROPPED`。`router.drain()` 里 `burst` 报文走的是 `while burst { pop; push }`，
  **完全不受 `MAX_BATCH` 限制**；22 条一口气写完，平板指针实际只走了一小段。
  而估计器仍然按「已经发出」的条数累计位移，于是模型比真实指针超前几百像素 —— 向上推进时模型先到 `y=0`，
  触发交还，真实指针却还在中上部。丢多少包取决于时序，所以表现为「有时候」。
- **旁证**：修复前连续 4 轮「下移 500px → 交还 → 重新接管」，第 1/2 轮光标回到顶部，第 3/4 轮停在
  设备高度 1560 / 1892（约 73% / 88%）。单事件慢速移动（20 次 ×30px、3ms 间隔）时队列只有 1 条/帧，
  `getevent` 收到的 REL_Y 条数完全对得上，不丢包。

### 19.2 修法

- `constants::PACKETS_PER_FRAME = 4`：一帧最多 4 条报文，**burst 同样受限**；ring 是 8，留一半余量让
  InputReader 把队列排空。`drain()` 改成按帧取最多 4 条，剩下的留在队列里下一帧继续（非接管期的 burst 仍整批丢弃）。
- 入口 22 条报文现在分 6 帧发出：`n=4 burst=3` / `n=4` / `n=4` / `n=4` / `n=4` / `n=3`，总位移不变。
- 单测：`drain_sends_at_most_four_packets_per_frame`（10 条排队 → 4 / 4 / 2，队列空后发心跳）；
  测试台的 `flush()` 改成「连续出帧直到队列排空」，避免仍按「一帧发完」写断言。

### 19.3 教训

- **凡是「一口气发一批输入事件」的地方都要问一句对端能不能吃下**：HID 侧的缓冲远小于应用层队列，
  溢出时内核只丢包、不报错，发送端完全无感。
- **估计器（航位推算）的前提是「发出的每一条都生效」**。一旦链路可能丢包，模型就会漂移，
  而漂移在闭环里不可观测 —— 这类系统必须有一个「已知状态的重同步点」（这里的入口顶边 pin），
  并且要保证这个重同步动作本身不会被丢。
- 本机实测：估计器按 `xscale=1.312` px/count 计，而平板真实位移约 2.86 px/count（光标截图 + 点击标尺两路互证），
  比例本身就差 2 倍以上，所以**只能靠入口对齐把误差闭环掉**，不能指望模型长期准确。
- 调试手法沉淀：`send.frame` 插桩 + `find_cursor3.ps1`（整屏 1/4 下采样找橙色光标 bbox）+
  平板上的点击标尺页（`pl.html` 显示最后一次 click 的坐标），三者互证比盯着截图猜可靠得多。
- 陷阱：`/dev/input/eventN` 每次会话都会变，先 `adb shell dumpsys input | grep -A2 PadLend` 取当前 Path，
  别复用上次的编号（曾把键盘 event12 当鼠标盯了一轮）。

## 20. 2.0.0 补录：修饰键「按下了却抬不起来」

| 项 | 内容 |
| --- | --- |
| 症状 | 用 Ctrl+Alt+S 接管后，键盘很多按键「失灵」或「对不上」：字母键变成菜单快捷键，方向键乱跳 |
| 现场 | `GetAsyncKeyState(VK_LMENU)=0x8000`：左 Alt 处于**系统级按下未抬起**状态；粘滞键/筛选键都没开，键盘布局正常 |
| 根因 | 低级键盘钩子按「**当时的状态**」决定是否吞键 |
| 相关文件 | `padlend-core/src/logic/router.rs`（`prepare_key` / `Inner::passed_keys`） |

### 20.1 为什么抬起会被吞

组合键 `Ctrl+Alt+S` 的时序跨了状态边界：

1. `Ctrl` 按下 —— 此时还是 IDLE，钩子放行，Windows 记录「Ctrl 已按下」；
2. `Alt` 按下 —— 同上，放行；
3. `S` 按下 —— 热键触发，状态切到 CAPTURED；
4. `Ctrl` / `Alt` **抬起** —— CAPTURED 期间钩子一律吞键（`keyboard.suppress_pc = true`），抬起事件被吃掉了。

于是 Windows 的世界里 Ctrl 和 Alt 永远按着。Alt 被系统当成菜单修饰键，之后敲字母都变成
「Alt+字母」的菜单访问键 —— 用户看到的就是键盘大面积失灵。这类「不对称吞键」还会制造
粘滞的 Shift（大写锁死）、粘滞的 Win（开始菜单反复弹出）等问题。

### 20.2 修法：抬起必须跟随按下的决定

- `Inner` 新增 `passed_keys: HashSet<i32>`：记下「**按下被放行给 Windows**」的 vk。
- `prepare_key` 拆成外壳 + `prepare_key_inner`：外壳先处理抬起 —— 只要这个 vk 在
  `passed_keys` 里，抬起就**无条件放行**并移除记录；按下则按判定结果写入/清除该集合。
  `track_safety`（Ctrl/Alt 左右侧集合）留在外壳里，保证提前返回时不会漏掉状态维护。
- Win 键同理：以前 `swallowed_win` 只记「被吞的按下」，若按下被放行、抬起时已在 CAPTURED，
  抬起仍会被吞，开始菜单会卡在按下状态。
- 单测 `key_release_follows_down_decision`：IDLE 放行 Ctrl 按下 → 接管 → 抬起必须放行；
  而被吞下的键，抬起仍然吞（保持原行为）。

### 20.3 教训

- **吞键必须成对**：任何「按下/抬起」两个事件各自判定的拦截逻辑，都要么按 vk 记住决定，
  要么在状态切换时把当前按下的键补发抬起。系统级的修饰键状态是全局的，弄脏了影响整台机器。
- 用户描述「键盘对不上」时，先量 `GetAsyncKeyState` 的 0x8000 位，比反复猜布局快得多；
  确认卡住后可以直接 `keybd_event` 补 keyup 救场。

## 21. 2.0.0 补录：xscale 不是真实的 px/计数（指针校准）

| 项 | 内容 |
| --- | --- |
| 症状 | 交还 PC 的位置与「感觉」对不上；换一台平板或改指针速度后估计器明显漂移 |
| 根因 | 估计器用 `xscale`（屏幕密度比，本机 1.312）换算，而 Android 指针速度在更上层、读不到 |
| 相关文件 | `padlend-core/src/logic/router.rs`（`model_scale` / `calibrate_*`）、`padlend-core/src/config.rs`（`motion.px_per_count`） |

### 21.1 实测差多少

同一台平板、同一个 `pointer_speed = 0`：光标截图 + 平板上的点击标尺页两路互证，
真实位移约 **2.4–2.9 px/PC 像素**（≈2.9 px/计数），而 `xscale` 只有 1.312 px/计数。
而且这个比例随**速度**变化（Android 指针加速）：同一段距离快推测得 3.54，慢推 2.25。
所以航位推算做不到长期准确，只能「把误差闭环掉 + 用实测比例把模型拉近一点」。

### 21.2 校准流程（设置 → 鼠标 → 指针校准）

1. `pointer.calibrate_begin`：把平板指针**钉到左上角**（14 条 `(-127,-127)` 报文，PointerController
   到边会夹住，多余计数不累积），估计器同步归零到 `(0,0)`，清零计数；
2. 用户往下推，`handle_captured` 里把**真正发出的 HID 计数**累加到 `calib_dx/dy`（有符号，来回抵消）；
3. 用户按热键交还，`begin_return` 关掉计数窗口；前端每 400ms 轮询 `pointer.calibrate_state`；
4. `px/计数 = 平板高 ÷ 累计计数`，写进 `motion.px_per_count`；估计器**就地换比例**
   （`update_config` 里只改 `scale`，不重置位置 —— 重置会制造新的分叉）。

### 21.3 教训

- **能被测量的常识值不要写死**：`xscale` 只是「屏幕密度比」，不是「指针位移比」；
  凡是跨过一层别人的输入栈（Android PointerController）的换算，都要留一个实测入口。
- 校准时「起点必须已知」：钉到角落比「大概在中间」可靠得多，因为到边可以夹住，
  而中间位置无法验证。**闭环只需要一个可信的重同步点**。
- 校准得到的比例要能**运行期生效**，否则用户还得为了一次标定重连一次，体验上就没人愿意校准了。

## 22. 2.0.0 补录：接管之后全局快捷键全部失效（连「交还 / 急停」也是）

### 22.1 现象

2.0.0 装机走指针校准：点「开始校准」→ 指针被钉到左上角 → 推到平板最底部 → 按对话框第 3 步写的
`Ctrl+Alt+S` 交还 —— **毫无反应**。再按 `Ctrl+Alt+Q`（急停）同样没反应，最后只能把指针往上推出
上边界，让路由器自己交还。

日志里 `hotkeys.registered ids=["0x1001"…"0x1009"]` 说明注册是成功的，但事后 **一条 `cap.end`
都没有、`MANUAL`/`PANIC` 也没有**；用 `keybd_event` 注入真实的左 Ctrl + 左 Alt + S 同样无效。

### 22.2 根因：低级钩子吞掉了修饰键，`WM_HOTKEY` 永远不会来

- 接管时 `keyboard.suppress_pc=true`（默认），低级键盘钩子对**所有**转发键返回 1（吞掉），
  Ctrl / Alt 这些修饰键也不例外（`prepare_key_inner` 里只有「保命字母」S/Q 被放行）。
- `RegisterHotKey` 依赖系统自己的修饰键状态：Ctrl/Alt 的按下事件被钩子吞掉，Windows 认为它们
  **从没按下过**，于是无论后面按什么字母都不会派发 `WM_HOTKEY`。
- `is_safety_letter` 只放行了 **S / Q 两个字母**，修饰键仍在被吞 —— 「保命键」只保住了字母，
  没保住组合键。
- 结论：**接管期间所有全局快捷键都是死的**：交还、急停、平板后退/主页/最近任务/通知栏/电源/音量。
  这个问题 1.x 就在（不是 2.0.0 引入），只是以前交还都靠「把指针推出边界」，没人依赖热键退出。

### 22.3 修法：接管期间由钩子自己比对组合键，不再指望 Windows

- `Router` 新增 `pub struct CaptureHotkey { ctrl, alt, shift, win, vk, action }` 与
  `set_capture_hotkeys()`；`Engine::install_hotkeys` 把 `parse_combo` 解析出的
  `(modifiers, vk)` 同步一份进去（保存配置后重建，旧配置不会残留）。
- `Inner` 增加 `shift_sides`（Shift 左右侧，`track_safety` 一并维护）、
  `capture_hotkey_vk`（记下被吞的主键）、`pending_hotkey`（待执行动作）。
- `prepare_key_inner` 在 CAPTURED 分支里先做组合键匹配：命中就返回 `(true, None)` ——
  **既不发给平板，也不指望 Windows 再派发一次**；动作写进 `pending_hotkey`，
  由 `Router::on_key_action` 在锁外取走。
- 抬起跟按下同一个决定：`capture_hotkey_vk` 记住命中的主键，抬起时照样吞掉，不会漏到平板。
- 键盘钩子改用 `on_key_action`，命中后 `std::thread::spawn(move || engine.fire_hotkey(action))`
  —— 钩子线程里绝不能跑动作（PITFALLS 2.5）。
- 回归测试 `capture_hotkey_fires_while_captured`：CAPTURED 下 Ctrl+Alt+S 返回 `Some(Toggle)`、
  不向平板转发、抬起不再触发；松开修饰键后 S 恢复成普通按键。

### 22.4 教训

- **低级钩子吞键会打断系统级热键**：任何「吞掉修饰键 + 依赖 `RegisterHotKey` / `WM_HOTKEY`」的
  设计都是错的。要么放行修饰键，要么自己识别。
- 自己识别时**必须同时处理抬起**，否则会复刻 2.0.0 上半场的卡键问题（PITFALLS 20）。
- 界面文案里的操作路径要和实现一起验收：对话框写着「按 Ctrl+Alt+S 交还」，就必须真按一次。
- 顺带修：`CalibrationState::px_per_count()` 只认**正计数**。实测把指针推到底边以后继续往上推，
  计数会变成负数（本机 −84），旧公式会算出 `2136/84 = 25.43 px/计数` 这种离谱结果；
  现在返回 0，界面提示「这次没测到有效位移，请重新校准」。



## 23. 2.0.1 补录：校准值不是常数（指针加速），默认值该往哪边偏

### 23.1 现象

用户反馈两条：①「校准出来的值跟手速有关 —— 鼠标动得越快，测出来的 px/计数越小」；
②「px/计数填低一点表现反而更好，不会在屏幕中间就交回 PC」。

### 23.2 为什么

- 校准公式是「平板高 ÷ 累计计数」，而 Android 的指针加速（DESIGN F4：速度系数在更上层、读不到）
  让**同样的物理位移在不同手速下消耗不同数量的计数**：快推时每个计数走得远（测得偏小），慢推相反。
  本机不同次测得 2.4 / 2.55 / 2.77 / 2.86 / 3.54，跨度约 1.4 倍 —— 单点标定必然不准。
- 误差方向不对称，这决定了默认值该偏向哪边：
  - 模型比例**小于**真实比例 → 模型走得慢 → 「到顶」判晚 → 最坏只是交还迟钝；
  - 模型比例**大于**真实比例 → 模型走得快 → 真实指针还在屏幕中间，模型已经到顶 → 中途掉回 PC。
  用户踩的正是后者。

### 23.3 修法

- `motion.px_per_count` 默认值 0（未校准 → xscale 1.312）改为 **2.0**：实测中位约 2.8 偏下 20–30%，
  新装即「只会晚、不会早」。0 仍保留「按分辨率推理论值」的语义。
- 该项在设置页变成可输入数字框（0 或 1–10，保存即生效）；校准对话框只负责测值与填入。
- 校准结果文案改为「实测值当上限，建议再往下调 10–20%」。

### 23.4 教训

- **单点标定 + 非线性系统 = 必然漂移**。只能读到一半的系统，标定值要按「误差方向的不对称性」往安全侧偏，
  而不是追中位数。
- 又一个 `.input { width: 100% }` 覆盖类名宽度的坑：给控件类（`.field-number`）写 width 时，
  选择器必须比 `.input` 更具体（`.slider-input .input.field-number`），否则看起来「加了样式」，渲染上没生效。
- 设置页的字段行原本是「固定标签列 + 剩余宽度」，开关落在第二列开头就停在卡片中间 —— 卡片右半边全空。
  改成 `minmax(0,1fr) auto` 让标签吃掉剩余宽度、控件贴右，才符合「左右两列」的视觉预期。
## 24. 2.0.2 补录：鼠标一顿一顿（每秒一次的 dumpsys input）

### 24.1 现象

用户：「鼠标在平板上会感觉一顿一顿的……有时候好好的在移动，突然平板上的鼠标卡一下，一下子又跳到了
右边一点，移动不是很连续」。PC 侧操作是匀速的，平板上的指针却像丢了帧再追赶。

### 24.2 怎么量出来的（可复用）

- 平板侧直接读内核事件：先 `adb shell dumpsys input | Select-String "PadLend Mouse" -Context 0,2`
  取 `/dev/input/eventN`（**每次重连编号都会变**，而且鼠标/键盘常是相邻号，很容易抓成键盘），
  再 `adb shell "timeout 18 getevent -lt /dev/input/eventN"` 重定向到文件。
- PC 侧用 `mouse_event(MOUSEEVENTF_MOVE, ±2, ±1)` 以**精确 10ms** 间距注入 400 次（spin-wait：
  `while (Stopwatch.Elapsed.TotalMilliseconds -lt $next) {}`，不要用 Start-Sleep —— 分辨率只有 ~15ms），
  同时把每次注入的实际毫秒写 CSV。先按 Ctrl+Alt+S 接管，结束后再按一次交还。
- 分析：按 `SYN_REPORT` 把 getevent 归并成一个 report，只统计稳态段（|x|≤6 且 |y|≤6），输出到达间隔的
  p50/p90/p99/max 与 >25ms、>50ms 的个数。**注入侧 p99 必须一起测**（本轮 10.7–10.9ms），否则说不清
  抖动在 PC 侧还是链路。

### 24.3 结论

- 音频转发**不是**原因：关掉音频后同样出现 ~120ms 尖峰（第一轮 44ms 纯属运气）。
- 平板收到的报文数量基本 1:1（387–399/400），但**到达时间**抖动大：p90 15–17ms、p99 27–31ms、
  偶发 100–125ms 停顿，停顿后紧跟一串（catch-up burst）。这正是「卡一下再跳一步」的观感来源。
- `set_nodelay(true)` 早就在（padlend-core/src/tablet/session.rs:438 一带），传输层不是瓶颈。
- 真元凶：`orientation_loop` 每 1 秒调一次 `device.shell(&["dumpsys", "input"])` —— **完整**的 dumpsys input。
  它在平板上要拿 InputReader 的锁，并把整份 dump（几十 KB）经 adb 传回。实测平板侧的 gap 大致每秒一次
  （10ms 步长下 idx ≈ 99/198/259/298），量级 100ms，与 dumpsys 耗时吻合。注意常量 `ORIENTATION_SCRIPT`
  （带 `grep -m1` 的版本）从 1.7 起就没人用了，真正跑的是完整 dump。

### 24.4 修法

- 朝向轮询：`ORIENTATION_POLL_S` 1.0 → **2.0**；改成
  `dumpsys window displays | grep -m1 -o 'mDisplayRotation=ROTATION_[0-9]*'`（只读 WindowManager 状态，
  输出几十字节），解析 `ROTATION_N` → N/90（与 dumpsys input 的 Viewport orientation 同编号）。
  廉价查询连续失败 3 次才退回旧的 `dumpsys input`，老设备不会因此失去旋转检测。
- **接管期间彻底不查**：`SessionPort::set_capturing(bool)`（默认空实现）由 `Router::change_state`
  在状态迁移时调用，`TabletSession` 存成 `capturing: AtomicBool`，`orientation_loop` 见到就 `continue`。
  交还后下一轮（≤2s）自然补查。理由：dumpsys 真正的代价是**占用 system_server 主线程**
  （输入派发也在里面），换成 window 查询只是把 dump 从 69 KB 降到 26 KB —— 仍然每 2s 卡一次，
  只有在「不需要的时候不查」才能把抖动从接管路径上完全移除。
- 出帧限流：新增 `C::MOVEMENT_PER_FRAME = 2`。`drain()` 里 burst 仍受 `PACKETS_PER_FRAME`(4) 约束，
  普通移动报文稳态 1 条/帧、队列有积压最多 2 条/帧 —— 追赶从「一次 4 条」降到「一次 2 条」，
  指针加速不至于把追赶算成快甩。

### 24.5 教训

- **周期性 adb 命令要先想「它在设备上做了什么」**：`dumpsys input` 不只读一下状态，它会锁输入栈。
  任何 1Hz 的后台查询都值得问一句：能不能换成只读几十字节的那个接口。
- 测抖动必须**同时测注入侧**。少了这一步，就会把 PC 自己的 sleep 精度（~15ms）当成平板的卡顿。
- `/dev/input/eventN` 每次重连都会变，且鼠标/键盘常是相邻编号 —— 每次都要重新查 Path，别抄上一轮的值。

