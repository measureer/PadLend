# Changelog

遵循 [Semantic Versioning](https://semver.org/)。

## [1.6.0] — 2026-11-14

### 修复

- **连接期间整个界面不再卡死**：引擎的 RPC 循环（`padlend-core/src/rpc.rs`）过去在
  stdin 线程里同步执行每个请求，`devices.connect` 这种要等设备出现的请求会把后面所有请求
  （保存设置、移除设备、急停）都堵在队列里——表现就是「点了连接之后整页点什么都没反应，
  去设置里关掉声音转发也不生效」。1.6.0 改为**每个请求一个线程**，慢请求打
  `[rpc] devices.connect 耗时 12000ms（期间其它请求不受影响）`，界面其它按钮随时可用。
- **连接可以取消**：设备页在连接中把「连接此设备」换成「取消连接」（新 RPC
  `devices.cancel`）。取消后服务端立刻返回，会话在选设备、推 jar、握手、建 UHID 的每一步之间
  检查取消标记，不会再出现「点了连接就锁死十几秒」。取消也可能发生在 `adb connect` 那 6 秒里，
  这时会在真正拉起会话之前就收尾，不会出现「点了取消、后台又自己连上」。连接总超时从 45 秒收到
  30 秒，并且每 8 秒在日志里报告一次进度（`仍在连接 <serial>（可点「取消连接」中止）`）。
- **开启「转发平板声音」不再导致连不上**：Android 11（API 30）以下没有系统音频回采，
  `audio=true` 时 scrcpy 服务端起不来，过去会白等 8 秒握手再回退。现在连接前先查
  `ro.build.version.sdk`，低于 30 直接只开控制通道并记一条
  `系统 Android API 29 不支持音频回采（需要 11/API 30），本次只开控制通道`；
  同时音频握手失败仍保留原来的「回退为仅控制通道」路径。
- **同一台平板换端口不再变成两台**：无线调试每开一次端口都会变，扫描会先为「客户端未知的新端口」
  建一条临时记录，随后按硬件序列号改名时又因为端点已被占用而被拒绝，于是列表里一行在线、一行
  已记住。1.6.0 在注册表里做了三件事：改名撞到「同主机 + 型号兼容」的记录就合并而不是拒绝；
  在线端点出现时会把同主机、已下线、型号兼容的旧记录就地改成新端口（旧端口留作别名）；
  启动读 `devices.json` 时自动合并历史遗留的重复条目并提示
  `设备记录里有同一台平板的重复条目，已自动合并`。

### 变更

- 版本号 1.6.0：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、
  `padlend-core/Cargo.toml`，前端默认状态与演示数据同步。
- 初次重连的等待窗口（等设备出现）由 40 秒缩到 20 秒，配合可取消的连接减少「干等」。

### 验证

- `cargo test`：532 passed / 13 ignored。新增
  `audio_is_skipped_on_android_below_eleven`、
  `rename_endpoint_merges_the_record_holding_the_new_port`、
  `adopt_live_endpoint_updates_the_old_record_in_place`、
  `adopt_live_endpoint_leaves_a_still_online_sibling_alone`、
  `merge_duplicates_folds_the_same_tablet_on_one_host`、
  `merge_duplicates_keeps_different_hosts_apart`。
- 前端 `vitest` 44 passed（新增「取消连接」用例），`tsc --noEmit` 通过。

## [1.5.0] — 2026-11-14

### 修复

- **无线端点过期后不再一直重试死地址**：`Engine::locate_devices()` 过去只由 RPC
  `devices.locate`（界面上的「查找设备」）触发，`device_candidates()` 又只读
  `devices.json`，所以平板换 IP/端口（重开无线调试、换网络）之后，会话会永远重试旧地址，
  每 30 秒刷一条 `TCP 端口不可达（已跳过，避免等待 adb connect 超时） (E101)`。
  1.5.0 把定位逻辑抽成 `wireless_targets()` + `relocate_wireless()`，由 `spawn_watcher`
  每 500 ms 顺带调用 `maybe_auto_relocate()`：仅当「未连接 + `device.auto_connect` +
  当前地址是无线端点」时，每 10 秒做一次 1.2 秒的静默 mDNS/ARP 查找；找到迁移后的地址就
  写回 `devices.json` 与配置（日志 `平板换了地址：旧 → 新（已自动更新）`），下一轮重连
  直接使用新地址。
- **连不上时给出可操作的提示**：连续 3 次定位落空只记一条
  `连不上平板：<地址> 无响应，局域网里也没有它的 mDNS 广播…`，不再让用户把「平板离线」
  误判成「新版本坏了」。

### 变更

- 版本号 1.5.0：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、
  `padlend-core/Cargo.toml`，前端默认状态与演示数据同步。

### 验证

- `cargo test`：新增 `wireless_serial_detection_matches_adb_endpoints`、
  `auto_relocate_is_a_noop_in_simulate_mode`，全套通过。
- 本机实测（`adb devices` 为空、EasyTier 虚拟网里没有平板）：服务日志在约 30 秒后
  出现「连不上平板…」提示，说明「平板不在线」能被明确看到。

## [1.4.0] — 2026-11-14

### 新增

- **可选的平板声音转发**：设置页新增 `misc.forward_audio`（默认关闭）。开启并重新连接后，
  平板按 scrcpy 的 `audio_source=output` 接管声音输出，Python 从音频 socket 读裸 PCM
  （48 kHz / 双声道 / 16-bit LE），经 `127.0.0.1` TCP 送到 Tauri 外壳；Rust 用 cpal
  在本机默认输出设备播放。平板自身不再外放，键鼠控制通道独立，不受音频缓冲反压影响。
- **音频播放器在 Tauri 侧**：新增 `src-tauri/src/audio.rs`，绑定随机本地端口，校验
  16 字节 `PLAU` 头，带 100 ms 起播预缓冲、1 s 上限和线性重采样；sink 断开只结束本次
  播放，不向主流程传播。新增 `tablet/audio.py` 负责读 scrcpy 音频并转发，sink 不可用时
  继续把音频读掉，避免控制通道被堵。

### 变更

- scrcpy 启动参数支持 `audio=true`，并保持 `video=false/control=true/raw_stream=true`；
  `audio=true` 时服务端先接受音频通道、再接受控制通道，握手顺序写错就会把
  控制流当 PCM。音频通道启动失败（如 Android 11 以下）会自动丢弃服务端并按
  `audio=false` 重连一次，不让可选功能拖死键鼠。
- 音频开关在连接建立时固定；修改设置只落盘并提示「重新连接平板后生效」，不会热切换
  已启动的服务端。

### 验证

- `pytest`：488 passed，3 skipped（新增音频转发、双通道握手顺序、sink 降级、
  `audio` 参数与仅控制回退用例）；`ruff check .`、`tsc --noEmit` 通过；
  `vitest run`：43 passed。
- 本机已用 VS 2022 Build Tools（MSVC 14.44 + Windows SDK 10.0.26100）完成
  `cargo check` 与 `cargo build`；GitHub Actions 仍会再跑一次。
- 未在真实平板上试听：当前环境没有可用的 Windows 音频端点；真机验收还需
  Android 11+ 平板与有声卡的 Windows。

## [1.3.0] — 2026-11-13

### 新增

- **设备注册表 `devices.json`**：设备身份不再用会变的 adb serial（无线地址是 `IP:端口`），
  而是优先 `ro.serialno`（先清洗 unknown/null/0 等无效值）生成稳定的 `hw:` id；读不到硬件号时用 `ep:` 临时记录，
  拿到硬件号后原地升级。一台设备可以挂 USB 和多个历史无线端点，换 Wi-Fi、换 IP、换调试端口后
  仍是一行。旧 `config.yaml` 的 `device.known/macs` 只在首次启动时迁移一次，之后不再读写。
- **按设备记录切换端点**：启动/重连时只在当前设备自己的端点间切换。当前端点优先，其余按
  `last_ok/last_seen` 排序；无线端点先做 0.6 秒 TCP 预检，通了才 `adb connect`（4 秒超时），
  避免一个失效地址把首连拖 40 秒。没有记录时才保留旧的 40 秒首连等待。
- **连接后校验硬件序列号**：读得到且与记录不一致时拒绝连接、给端点记失败并在日志说明，
  不会把另一台平板当成当前设备；读不到硬件号时按记录端点接受，避免少数 ROM 把用户锁死。
- **发现层利用 mDNS TXT**：`MdnsService` 的 serial/model 不再被丢弃，定位时先按硬件序列号匹配新
  IP，再按同 IP 新端口、MAC/ARP 兜底。
- **地址输入支持主机名与 IPv6**：添加设备接受 `host:port`、`[IPv6]:port` 和裸 IPv6（补默认端口），
  跨网或 VPN 只要电脑能访问调试端口即可；mDNS 自动发现仍通常只在同一局域网有效。
- **设备页显示最后在线日期，并按稳定 id 去重/保持选中**；端点迁移后不会丢当前选中设备。

### 变更

- `device.known` / `device.macs` 标为已停用，只用于 1.3.0 首次迁移；设备列表与离线行改由
  `devices.json` 生成，一台设备只占一行。
- 设备页「从列表移除」删除整条设备记录（含所有端点），不再只删一个 serial。
- 「保持平板亮屏」的临时熄屏超时从 int32 最大值调整为 5 小时；设置页明确提示会临时修改
  平板熄屏时间，用户可关闭。

### 验证

- `pytest`：478 passed，3 skipped（新增设备注册表、候选连接、IPv6/主机名、mDNS 硬件号，
  以及 teardown 单步失败注入、真实父进程退出、facts 轮询落盘用例）；
  `ruff check .`、`tsc --noEmit` 通过；`vitest run`：43 passed（新增 actions 错误分支用例）。

## [1.2.1] — 2026-11-13

### 修复

- **「保持平板亮屏」不再改开发者选项**：旧实现连接时执行 `svc power stayon true` 并改写
  `settings global stay_on_while_plugged_in`，断开后平板仍处于「充电时保持唤醒」，拔线就熄屏。
  1.2.1 改为只临时调大当前设备的 `settings system screen_off_timeout`：原值先原子落盘，
  断开/退出写回，进程崩溃则下次启动恢复；读取失败或原值不是数字时不修改系统设置。
  全程不碰 `stay_on_while_plugged_in` / `svc power stayon`。
- **删除没有实现的「显示指针幽灵」开关**：该功能只落地了配置字段、TS 类型、设置页 schema、
  mock 和文档，运行时没有任何绘制链路，属于空开关。1.2.1 删除开关与全部相关代码；
  旧配置里的 `ui.show_pointer_ghost` 在解析时静默丢弃，不触发「未知配置项」告警。

### 变更

- **穿回阈值默认值 120 → 80 px**：从平板边缘回 PC 需要反向拖动的距离缩短，同时保留
  防止边缘点按/晃动误交还的余量。旧配置里已显式写下的值不受影响。
- **设备页删除多余说明**：首页不再显示「连接平板后才会读取……保留一行」这段长说明，
  设备列表和详情本身已表达同样的信息。
- **首次向导由 5 步压成 3 步**：连接平板/USB 调试 → 检查 UHID → 完成。删除前端
  `wizardPushJar`、`wizard.push_jar` 与后端 `wizard.finish` RPC，以及「安装 adb / 检查并
  推送 scrcpy-server」步骤；adb 与 `scrcpy-server` 本就随安装包内置，向导文案已说明，
  用户不需要另外准备。`wizardFinish` 前端动作保留，只负责保存自动连接/收进托盘选项。

### 验证

- `pytest`：426 passed，3 skipped（新增 5 个回归用例）；`ruff check .`、`tsc --noEmit` 通过；
  `vitest run`：29 passed。

## [1.2.0] — 2026-11-13

### 修复

- **托盘「退出」只隐藏窗口，进程和 sidecar 继续跑**：1.1.2 的退出复用了窗口关闭路径，
  而 `ui.close_to_tray=true` 时 Tauri 的 `CloseRequested` 会 `prevent_close()` + `hide()`，
  所以点「退出」只是把窗口藏起来。1.2.0 新增显式 `quit_app` 命令：先同步停 sidecar，
  再 `app.exit(0)`；托盘菜单和托盘的 quit 事件都走这条路径，窗口关闭（X）仍按
  `close_to_tray` 的语义隐藏。`close_to_tray` 默认仍是 `false`，行为未变。
- **接管期 Win 键会把开始菜单盖在遮罩下，看起来像崩溃/黑屏**：`passthrough_keys`
  默认把左右 Win 放给 Windows，开始菜单弹出后被 topmost 遮罩挡住；鼠标点不进去，
  其余按键又被吞掉，用户只能强杀。1.2.0 在 Router 的 CAPTURED 状态无条件吞掉
  L/R Win 的 down/up（含 `vk==0` 的 scan code 路径），不转发平板、不进 keymap；
  `_swallowed_win` 锁存到 keyup，交还瞬间也不会漏掉松键。IDLE 下 Win 键行为不变。
- **崩溃/强杀后遗留孤儿遮罩**：遮罩窗口属于 Python sidecar；1.1.2 的建窗函数把
  `CreateWindowExW` 与 `SetLayeredWindowAttributes`/`ShowWindow`/`SetWindowPos` 放在
  一次调用里，超时重试会在调用线程再建一个未被跟踪的 topmost 窗口。1.2.0 把创建
  拆成「`create_mask_window` 拿 hwnd → 登记 → `configure_mask_window` 配置」，
  超时直接报错而不再重跑；启动时 `cleanup_orphan_mask()` 按窗口类名找到残留，
  先隐藏/关闭，必要时终止仍挂起的 sidecar，并复位 `ClipCursor`。
- **Python 关闭被守护线程截断**：`service.shutdown()` 以前只起一个 daemon 线程就返回，
  解释器退出会截断 `hotkeys.stop`/`runtime.stop`/`_mark_clean_exit`。现在 shutdown
  在调用线程串行执行并用锁防重；`Runtime.stop()` 也加了幂等保护，重复/并发调用安全。
- **Tauri 崩溃后 sidecar 变孤儿**：Rust 启动 sidecar 时注入 `PADLEND_PARENT_PID`，
  Python 每秒检查父进程；父进程消失就同步 teardown 后退出，不再等 stdin EOF。
- **首次运行误报「上次未正常退出」**：`_stale_cleanup` 以前用 `config.yaml` 是否存在
  判断是否运行过，但首次启动会先写默认配置。现在由调用方在写配置前传 `first_run`。

### 变更

- 托盘「退出」、窗口关闭（X）、`Ctrl+Alt+Q` 急停现在是三条明确不同的路径：
  quit 停 sidecar 后退出进程；X 按 `close_to_tray` 隐藏或停 sidecar 退出；
  急停只交还输入并释放遮罩，不退出程序。
- `bridge.shutdown()` 改为同步等待 sidecar 退出（上限 8s，覆盖遮罩线程最坏 5s），
  超时才 `kill()` 并用 `taskkill /T` 清进程树。
- 清理经全仓引用检索确认零调用的死代码：`constants.py` 的 13 个孤立常量
  （`APP_ID`、Qt 时代 `ASSETS_DIR`/`ICONS_DIR`、`RAW_QUEUE_MAXSIZE`、`KEY_QUEUE_MAXSIZE`、
  `CLIPCURSOR_WATCHDOG_S`、DWM 主题常量、`EVENT_NAMES`、`SAFETY_HOTKEYS`、
  `HEARTBEAT_INTERVAL_S`、`HID_MOUSE_REPORT_ID`）、`core/geometry.contains_rect/intersects`、
  `core/logsetup.logger_for`、`core/metrics.record_rtt` 别名、`core/state.IllegalTransition`、
  `win/dpi.py` 三个 awareness 常量、`win/hotkeys` 的 `MOD_SHIFT/MOD_WIN`、
  `win/rawinput` 的 `MOUSE_VIRTUAL_DESKTOP/RI_MOUSE_HWHEEL`、`config.default_config/save_config`
  和前端 `formatGain()`；`win/trayprobe.py` 保留为手工诊断模块，文档注明 runtime 无调用。
- 新增 `docs/PITFALLS.md`（开发期踩坑，按模块分节），`TROUBLESHOOTING.md`
  只保留用户可执行步骤，两处互链。

### 验证

- `pytest` 421 passed / 3 skipped（收集 424）（新增 Win 键锁存、遮罩先登记/配置超时、孤儿遮罩清理、
  adb 故障映射、UHID 半包、teardown 幂等/父进程看门狗/首启假阳性等用例）；
  `ruff check .` 通过。
- `vitest` 29 tests 通过（新增托盘退出走 `quit_app` 而非 `windowClose` 的用例）；
  `tsc --noEmit` 通过。
- **缺陷 A 的崩溃本身未能复现**：日志目录、WER 与事件查看器都没有 PadLend 的
  `crash-*`/`faulthandler-*` 记录，复现时桌面已锁屏无法回放。因此本轮只按已确认的
  Win 键输入路径和失效安全修复，不声称拿到了崩溃栈；现场若再出现，请按
  `docs/PITFALLS.md` 4.1 收集完整日志与 WER 报告。

## [1.1.2] — 2026-11-12

### 修复

- **接管交还后遮罩窗口残留：整块屏幕没有光标，所有点击被吞**（遮罩窗口线程归属的老问题）。
  1.1.1 的遮罩只在「创建它的那条线程」上处理 `WM_CLOSE`。从界面按钮进入接管时，这条
  线程是 RPC 线程池里的线程，**没有消息泵**——`PostMessage(WM_CLOSE)` 永远不会被处理。
  鼠标划到平板边缘自动交还恰好发生在 raw input 线程，于是 `ClipCursor` 已经复位、屏幕
  却还被一块透明的顶层遮罩盖着：看不到光标，点哪里都没反应，只能退出应用。
  现在遮罩窗口有专用线程（`padlend-mask-win`，daemon，自带 Win32 消息泵）：创建、
  `WM_SETCURSOR`、销毁都在该线程执行；释放时把 `DestroyWindow` 直接投递过去，不再
  依赖跨线程 `WM_CLOSE`（实测 9 ms 内销毁）。万一投递超时，退路是先 `ShowWindow(SW_HIDE)`
  把屏幕立刻让出来，再补发 `WM_CLOSE` 收尾。平台后端没有消息泵时（单测假实现）仍走原路径。

### 验证

- 真机复现原故障场景：在**没有消息泵**的线程创建遮罩、从另一线程释放。修复后遮罩在
  专用线程创建（线程 id 与调用方不同），释放 9 ms 完成，`EnumWindows` 剩余遮罩 0 个，
  光标恢复可见；修复前同一场景遮罩一直残留，且 `WindowFromPoint` 命中遮罩。
- `pytest` 全量通过（新增「跨线程释放必须落在遮罩线程、不产生 `WM_CLOSE` 残留」与
  「遮罩线程卡死时先隐藏窗口」两组单测）；`ruff check .` 通过。

## [1.1.1] — 2026-11-12

### 修复

- **接管期间电脑屏幕上仍出现鼠标指针**（电脑和平板各一个光标）：遮罩窗口没有稳定
  拿到 `WS_EX_TOPMOST`。实测安装版进程里 `SetWindowPos(HWND_TOPMOST)` 返回成功却
  不生效，遮罩被任务栏压住，`WM_SETCURSOR` 收不到，系统光标就重新显示出来。现在：
  遮罩扩展样式直接带 `WS_EX_TOPMOST`（创建即置顶）；创建后回读扩展样式校验并重试；
  每次接管复用窗口时重新置顶；接管期间每秒一次看门狗复查，丢了顶层位置或光标重新
  可见就立刻修复并记 `mask.repair` 日志。`cap.start` 现在以 INFO 输出遮罩诊断
  （`visible` / `topmost` / `clip` / `windows`），便于现场排查。
- **文案**：标题栏与「关于」不再把平板说成「第二块屏幕 / 第二块输入屏」，改为
  「跨设备共享键鼠无缝操作平板」——平板只共享键鼠，不占用显示器。

### 验证

- `pytest` 全量通过（遮罩新增置顶回读、复用置顶、看门狗修复等 6 项单测）；
  `ruff check .`、`vitest`、`tsc --noEmit`、`npm run build` 通过。
- 用打包后的 sidecar（`padlend-service.exe`，正是此前复现「双指针」的冻结进程）实测：
  手动接管与边缘接管期间遮罩扩展样式始终带 `WS_EX_TOPMOST`，系统光标全程隐藏；
  交还后遮罩窗口与 `ClipCursor` 锁均干净释放。
## [1.1.0] — 2026-11-12

### 变更

- **前端架构迁移**：PySide6/Qt 界面整体替换为 Tauri 2 + React 19 + TypeScript。
  Rust 外壳负责窗口/托盘/单实例/剪贴板/sidecar 生命周期；新增 `service/`
  以 NDJSON（stdin/stdout）把既有 `core/`、`tablet/`、`win/`、`config.py`
  暴露给前端，业务接口与状态机行为不变。
- **统一视觉规范**：`src/styles/tokens.css` 单一令牌源（两套主题、8px 间距、
  字体/圆角/控件高度），统一组件库与页面骨架；1180 / 900 / 700px 断点自适应，
  布局画布 SVG viewBox 随窗口缩放。
- **性能**：取消 1s GUI 轮询改事件推送；日志防抖 + 虚拟列表；画布拖动只改 DOM
  transform 且 rAF 合帧；慢任务全部移到服务进程线程池；设置草稿本地化；
  主题切换只改根 CSS 变量；路由/对话框按需分包。
- **架构治理**：诊断纯逻辑从 Qt 层上移到 `core/diagnostics.py`；设置页 14 个字段
  由 `configSchema.ts` 一份声明驱动；指标键名与 `core.metrics.snapshot()` 对齐；
  非法配置补丁现在整体回滚，不污染运行中的 Config。
- **清理旧代码**：删除 `ui/`（PySide6）、旧 GUI 测试与截图、旧截图/图标工具、
  `PadLend.spec`、Inno Setup 配置、旧版 `tools/release.py` 打包流程与 Qt 许可文本；
  `main.py` 瘦身为无头/服务组合根，`pyproject.toml` 与依赖文件去掉 PySide6。
- **发布工程**：CI 与 Release 工作流改为 Python 测试 + 前端测试 + Rust/MSVC 编译 +
  Tauri NSIS 安装包；`tools/release.py` 统一构建前端/sidecar/zip/校验和。

### 修复

- 旧版 `MetricsPanel._pick` 的别名猜测导致 `report_hz` / `backpressure` / `loss_pct`
  三个字段恒显示「—」；新前端直接消费后端键名 `raw_hz` / `backlog` / `drop_pct`。

### 验证

- `pytest` 全量通过（含新增服务协议端到端测试）；`ruff check .` 通过；
  `vitest` 28 项通过；`tsc --noEmit` 与 `npm run build` 通过；
  无头浏览器对构建产物截图覆盖 4 个页面、亮色主题与 900×700 窄窗口。

## [1.0.5] — 2026-11-11

### 新增

- **平板 → PC 剪贴板回传**：接管期间在平板上复制文本，Windows 剪贴板会跟着更新。
  沿用 `misc.clipboard_sync` 一个开关，不新增选项；设置页文案改成「双向同步」。
  控制通道增加常驻接收线程，按 scrcpy 5.0.1 的 device message 拆包
  （`CLIPBOARD` / `ACK_CLIPBOARD` / `UHID_OUTPUT`）：一次 `recv` 只到半条消息时
  保留缓冲区继续攒，未知类型或超过 256KB 判流断开并走既有重连。
  回写 Windows 剪贴板后 1 秒内只吞掉与远程文本**完全相同**的那次回显，用户紧接着
  复制别的内容仍然照常同步。

### 修复

- 控制通道此前只发不收，平板回传的字节一直堆在 socket 缓冲区里没人读；现在由接收
  线程消费。关闭连接时先置停止位、`shutdown` 再 `join`，避免接收线程卡在 `recv`。

## [1.0.4] — 2026-11-07

### 变更

- 「触发阈值」滑条下限从 10 px 降到 1 px；默认值从 40 px 改为 10 px。
  默认值更跟手，觉得一蹭就进时仍可在设置里调大。

### 修复

- 慢推兜底不再固定为 12 px，改为不超过 `arm_px` 的一半。否则默认阈值降到
  10 px 后，12 px 的兜底永远攒不到，慢慢推鼠标会超时回 IDLE。

## [1.0.3] — 2026-11-06

### 新增

- **每台设备、每个屏幕方向各记一份布局**：配置新增
  `layouts.devices.<设备身份>.landscape/portrait`，字段与全局 `placement` 相同。
  在布局页点「保存布局」时，除了更新全局 `placement`，还会写入当前设备和当前
  方向的 profile；旧配置没有这一段时继续使用全局 `placement`，无需迁移。
- **按真实朝向自动切换布局**：连接后保持一条 adb shell 常驻监听
  `dumpsys input` 的 Viewport orientation，约每秒检查一次；平板旋转时更新设备
  事实并通知界面，自动切到该设备该方向保存过的布局。设备身份优先用
  `ro.serialno`，读不到时回退 adb 序列号，USB 与无线调试共用同一份布局。
- 设备页刷新和启动探测不依赖朝向监听；监听复用一条常驻 adb shell，
  只读取 Viewport 那一行，不额外执行 `wm size` 或完整设备扫描。

### 变更

- 布局页不再支持“双击平板切换横竖屏”的预览：画布尺寸只认真机事实，避免
  手动双击与真实朝向不一致。提示文字改为“每台设备的横屏、竖屏布局分别保存，
  旋转平板会自动切换”。
- 同一台设备切到尚未保存过的方向时，保留当前摆位让用户继续调整并保存；
  换到另一台没有 profile 的设备时，回退到全局 `placement`。

### 修复

- 设置页移除「平板位置」分组后，画布右键菜单里的「对齐方式」仍然保留；
  它和拖动一样只改预览，需要点「保存布局」才落盘。

## [1.0.2] — 2026-11-06

### 修复

- **无线调试换端口后找不到设备**：Windows 上的 adb 经常没有 mDNS 守护进程
  （`adb mdns check` 返回 unavailable），旧逻辑只能去 `adb mdns services` 拿端口，
  拿到空表就失败，即使平板仍在同一 IP 上广播新端口。新增 `tablet/mdns.py`，
  用标准库直接查询 `_adb-tls-connect._tcp.local`，解析 SRV/TXT/A 记录拿到当前端口；
  启动探测与设备页刷新都会用上。adb 自带 mDNS 可用时仍优先用它，之后再用保存的
  MAC 和 ARP 表处理换 IP 的情况。
- **重复启动没有唤起已有窗口**：第二个实例以前只按窗口标题查找首实例，窗口收进
  托盘或标题变化时会静默退出，看起来像崩溃。现在首实例创建命名事件
  `Global\PadLend.Activate`，第二实例发事件请求首实例恢复窗口；监听线程只发 Qt
  信号，窗口操作仍在 GUI 线程。旧版本没有事件时保留标题查找兜底。
- **吸附边改由布局决定**：删除设置页的「平板位置 / 吸附边 / 对齐」分组，摆位只在
  「桌面布局」页调整，不再出现设置里的边与实际吸附加载段不一致的情况。
- **边缘穿越按实际接触判定**：进入前先用平板矩形检查鼠标压线点是否真的贴着平板，
  只允许从平板覆盖到的外轮廓段落入；不再把同朝向的整条显示器边缘都当成可穿越。
  修复了平板在 DISPLAY2 下方、同时压住 DISPLAY1 左下角时，DISPLAY1 整条底边都能
  穿过去、但从 DISPLAY1 左边靠近角落却进不去的问题。
- **布局页新增「保存布局」**：拖动、松手、右键对齐/移除都只生成预览，不再即时写盘；
  点「保存布局」才写入 `placement` 并立即用于边缘穿越。有未保存改动时标题旁会提示。
- **「适应窗口」改名「显示全部」**：按钮行为不变，仍是一次缩放到同时看见所有显示器
  和平板；滚轮缩放、`Ctrl+0` 恢复。

## [1.0.1] — 2026-11-06

### 变更

- 无线设备的 Wi-Fi MAC 从单一 `device.mac`（只记最近一台）改为
  `device.macs`（地址 → MAC，每台设备各存一份）。旧配置里的 `device.mac`
  在读取时自动迁移到当前 `serial` 对应的条目，用户无需手动改。
- 设备页的「刷新设备」除重新扫描 adb 外，还会用已保存的 MAC 在局域网里
  逐台查找；平板换了 IP 或无线调试端口时，列表里的旧地址会被新地址替换，
  MAC 一并带过去。程序内部切换页面触发的刷新不会启动这项查找。
- 修正定位器没有拿到 ARP 表的问题：保存的 MAC 在设备换 IP 时现在能真正
  通过 ARP 表匹配到新地址，而不是只依赖 mDNS。
- 后台读取 MAC 期间设备被删除时，不再把 MAC 写回配置。

## [1.0.0] — 2026-11-05

首个正式版本。功能行为与 0.3.19 一致，重点是发布工程、可诊断性与文档。

### 版本与构建

- 版本号收敛到唯一源 `padlend/__init__.py`（`__version__ = "1.0.0"`），
  `constants.VERSION`、GUI 底栏、向导、关于对话框与 exe 版本资源均从它派生；
  修正此前 `constants.py`（0.3.19）与 `pyproject.toml` / `padlend/__init__.py`（0.3.18）漂移。
- 发布物从 onefile 改为 **PyInstaller onedir**：`dist\PadLend\`（exe + `_internal\` + `LICENSES\` +
  `THIRD_PARTY_NOTICES.md`），发布 zip 约 52 MB；首次启动不再解包到 `%TEMP%` 或 exe 目录。
- 新增 `tools/release.py`：一条命令完成 onedir 构建、zip、SHA256SUMS、可选 Inno Setup 安装器
  与可选代码签名（未提供证书时明确跳过并提示未签名）；CI 增加 ruff + pytest，Release 工作流
  构建并上传 zip / setup / SHA256SUMS.txt。
- exe 写入版本资源（FileVersion 1.0.0.0、ProductVersion 1.0.0、产品名、版权），
  Windows 文件属性里不再是空白。

### 稳定性与可诊断性

- 新增崩溃兜底：`sys.excepthook` / `threading.excepthook` 写
  `logs\crash-YYYYmmdd-HHMMSS-<pid>.log`（版本、argv、异常线程 traceback 与线程名）；
  `faulthandler` 捕获原生崩溃写 `faulthandler-*.log`（全线程栈）；启动失败弹窗并落盘。
- 日志轮转：`padlend.log` 单文件 2 MB、保留 3 个轮转文件，长时间运行不再无限增长。
- 配置损坏恢复：YAML 语法错误、非映射、版本非法/过新、字段越界时，原文件另存为
  `config.yaml.bad-<YYYYmmdd-HHMMSS>`（不覆盖旧备份）并加载默认值，不再启动即崩。
- 诊断包脱敏：序列号、IP、MAC、用户名、用户目录默认替换为占位符；导出 zip 含
  `diagnostics.txt`、`meta.json`、adb 输出、dumpsys 片段与最近 512 KB 日志。
- adb server 所有权：只停止由 PadLend 自己启动的 adb server，不再误杀用户/IDE 正在用的实例。
- scrcpy-server jar 在推送前按 `vendor/SHA256SUMS` 校验，不一致抛 E103 并提示重新下载，
  不带病启动。

### 界面与文档

- 新增「关于 PadLend…」：版本、许可证摘要、第三方许可文件、打开日志目录；托盘菜单增加
  「打开日志目录…」。
- 重新生成四张界面截图（1280×820，离屏渲染，脚本 `tools/make_screenshots.py` 可重复执行），
  替换 0.1.0/0.2.5 时期的旧图。
- 修复布局页适配视图时的裁剪：平板位于桌面包围盒之外时，自动扩展场景矩形，
  右侧平板与边缘吸附提示不再被截断。
- README 按 1.0.0 重写：下载/校验、SmartScreen、onedir 目录、安装器、配置全字段、日志与诊断、
  已知限制；删除已移除的剪贴板热键与 exe 同级 `.padlend\` 描述，修正 `exit_overflow_px` 默认值。
- UHID 文案改为硬条件：不可写就无法创建虚拟鼠标/键盘，删除“退回绝对坐标模式”的旧说法。
- 修复 `device.auto_connect` 与向导完成页复选框此前不生效的问题：关闭自动连接后启动停留在设备页，
  向导里的“自动连接 / 关闭时收进托盘”会写回配置。
- TROUBLESHOOTING / VERIFICATION / DESIGN 同步：补 SmartScreen、jar 校验失败、日志路径与
  脱敏说明；删除不存在的“释放鼠标捕获”入口与 `ui.tray_hint_shown`；新增 1.0.0 发布验收清单。
- 新增 `THIRD_PARTY_NOTICES.md` 与 `LICENSES/`，随发布包分发第三方许可证原文。

## [0.3.19] — 2026-11-05

### 变更

- 移除启动时的托盘位置气泡，以及点 X 收进托盘时的“已最小化到托盘”气泡。
  `ui.tray_notifications` 保持默认开启，继续控制“诊断摘要已复制”这类其余气泡。
- 删除 `ui.tray_hint_shown` 配置项；旧配置里已有的该键会被忽略。

### 测试

- `tests/test_ui_smoke.py`：收进托盘不再发送气泡；托盘注册与退化逻辑保持原样。

## [0.3.18] — 2026-11-05

### 修复

- **托盘图标的「注册成功」不再靠 Qt 自说自话**：0.3.16/0.3.17 的日志里一直是
  `available=True visible=True`，但图标可能根本没被外壳接受。新增
  `win/trayprobe.py`：挂一个 message-only 的探针图标、立刻摘掉，用
  `Shell_NotifyIconW` 的返回值当判据。
  - 启动日志改为
    `tray shown: qt_visible=… native_ok=… err=… entries=… self_entry=…`；
    启动 1.2 秒后再复核一次（`tray recheck after startup -> …`）。
  - 隐藏窗口前用探针复核（`ensure_visible`）；被拒时不再把窗口藏进“找不到”
    的状态，改为最小化到任务栏，并在底栏写明原因与错误码。
  - Windows 11 默认把新图标收进任务栏右下角的「^」溢出区：首次启动弹一次
    位置提示（`ui.tray_hint_shown` 记录已弹过），设置页的「关闭窗口时最小化
    到托盘」说明也补了这一句。

### 测试

- `tests/test_ui_smoke.py`：探针判定（`probe_registration` 返回 False → 不
  隐藏、返回 True → 隐藏）、退化最小化时的底栏文案、首次提示只记一次。
- 全量 411 passed, 3 skipped；`tools/acceptance.py` 33 项 0 失败 3 跳过。

## [0.3.17] — 2026-11-05

### 修改

- **设置页删掉「偏移」和「角优先」**：这两个选项看不出效果，也解释不清。
  - 偏移原本只是「布局」页上平板贴着那条边的位置，**不参与穿入/穿出的坐标映射**；
    现在改为在「布局」页直接拖动平板，不再单列滑条。
  - 角优先原本是「角落同时命中两条边时优先哪条」，默认已经按移动方向自动裁决；
    想临时改一边，在「布局」页拖动时按住 Shift 即可，不再单列下拉框。
  - 配置文件里 `placement.offset` / `placement.corner_priority` 仍保留（画布拖动会写），
    旧配置继续有效。

## [0.3.16] — 2026-11-05

### 修改

- **上报频率改成四个固定档**：设置页从滑条换成下拉框，只给 125 / 250 / 500 / 1000 Hz。
  旧配置里的任意值（例如 998）在加载时取最近的档位并记一条 issue；`motion.report_hz`
  不再接受中间值。
- **穿回阈值默认值 24 → 120 px**：在平板边缘点按/轻微晃动不再把光标误交还给 PC；
  代价是从平板边往回拖的时间稍长一点（设置页仍可在 4–120 之间调）。
- **托盘图标换成品牌标**：以前只是一个状态色圆点，缩到 16px 后看不出是哪个程序，
  很容易被当成系统图标（“在托盘里找不到 PadLend”多半就是这个）。现在图标是品牌标
  「P」，24px 以上再在右下角叠一颗状态色小点（灰未连接 / 绿空闲 / 红接管中 / 黄故障）；
  16/20px 只留品牌标，保证缩到通知区大小仍然认得出。
- **关闭到托盘更可靠**：隐藏主窗前先确认托盘图标真的挂在通知区
  （`TrayController.ensure_visible()`），挂不上就退化成最小化，避免窗口藏进
  “找不到图标也找不到窗口”的状态；日志新增 `tray shown` / `tray ensure_visible` /
  `tray icon is not visible; minimized …`，下次可据此判断是没注册还是被系统收进
  任务栏右下角的「^」溢出区。气泡提示也补上了「找不到就点 ^ 或重新运行 PadLend」
  的说明——重新运行会通过单实例互斥量唤回已隐藏的窗口。

### 测试

- `test_report_hz_is_four_fixed_choices`、`test_exit_threshold_default_is_120`、
  `test_report_hz_snaps_to_a_fixed_choice`，以及托盘不可见时退化成最小化的
  `test_request_close_minimizes_when_tray_icon_is_invisible`。
- `tools/acceptance.py` 33 项 0 失败 3 跳过（阈值改大后原有边界用例改用
  `C.EXIT_OVERFLOW_PX + 10` 的相对位移）。

## [0.3.15] — 2026-11-05

### 修改

- **删掉「剪贴板」快捷键（Ctrl+Alt+V）**：0.3.14 起剪贴板同步已经在勾选后自动工作，这个热键只是把同一段文本再写一次，留着反而让人以为同步要手动按。
- `hotkeys.clipboard` 配置字段一并移除；旧 `config.yaml` 里残留的 `clipboard:` 行会被忽略（不会报错），下次保存时自然消失。
- 设置页「⚡ 快捷键」组只剩「接管/交还」「急停」两行。
- `SET_CLIPBOARD` 的 `paste` 字节固定为 0：只写剪贴板、不触发粘贴，也不产生任何按键事件。

### 测试

- 删除与新行为冲突的旧断言；`tests/test_clipboard.py` 断言写剪贴板后 `inject_text` 从未被调用。
- 新增 `tests/test_ui_smoke.py::test_settings_panel_has_no_clipboard_hotkey`（设置页没有 `clipboard_edit`、`_field_widgets` 里没有 `hotkeys.clipboard`）。
- `tools/acceptance.py` 33 项，0 失败，3 跳过。

## [0.3.14] — 2026-11-05

### 修复 / 改进

- **剪贴板同步改为自动**：勾选「⚙ 杂项 → 剪贴板同步」后，进入接管时同步一次，接管期间每次在 PC 上复制都会自动同步到平板，不需要再按快捷键；`Ctrl+Alt+V` 改成「立即粘贴一次」，只给需要往输入框贴文本的场合用。
- **改用 `SET_CLIPBOARD` 协议（type 9）**：不再用 `INJECT_TEXT`。后者在服务端会被 `KeyCharacterMap.getEvents()` 翻成真实按键事件，等于把剪贴板当键盘打进平板前台 App（刷抖音命中 `R` 就是「不感兴趣」）。`SET_CLIPBOARD` 只调 `setPrimaryClip()` 写剪贴板，不产生任何按键，所以自动同步是安全的（scrcpy 5.0.1 服务端已支持该消息）。
- 说明文字同步更新：快捷键条目改成「立即粘贴」，勾选项说明写清「进入接管 + 每次复制自动同步、不敲键盘」。

### 测试

- 新增 `tests/test_protocol.py::test_set_clipboard_packs_sequence_paste_and_text`（逐字节钉死 `sequence(8) paste(1) len(4) utf8`）与 `::test_set_clipboard_defaults_to_paste_off`。
- `tests/test_clipboard.py` 改为断言写剪贴板路径：4KB 截断、开关、空串、`paste` 透传，并断言不再产生 `inject_text`。
- `tests/test_ui_smoke.py`：勾选时进入接管会排队同步、未勾选不排队、`dataChanged` 只在接管期间触发。
- `tools/acceptance.py` 新增「剪贴板同步只写平板剪贴板（不敲键盘）」：33 项，0 失败，3 跳过。

## [0.3.13] — 2026-11-05

### 新增

- **设备列表**：设备页标题从「可用设备」改为「设备列表」，只列已经通过 ADB 授权的设备（USB 或无线）；说明文字写清了这一点。
- **从列表移除**：选中一行「上次连接的设备」后可点底部的「从列表移除」，只删记忆列表（`device.known`），不会断开正在连接的设备。当前在线的设备没有这个按钮。
- **启动时自动找回上次的设备**：打开窗口约 0.4 秒后在后台探测上次那台无线平板，顺序是 ① 原 `ip:port` 做一次 TCP 连接；② 换了端口就查 `adb mdns services` 找新端口；③ 换了 IP 就按上次记下的 Wi-Fi MAC 查系统 ARP 邻居表找它现在挂在哪个地址；④ 都不行才在列表里标「不在线」。命中新地址时自动切换并记住。
- **记住平板 Wi-Fi MAC**：连接成功后后台读一次 `/sys/class/net/wlan0/address`（退路是 `ls /sys/class/net` 后逐个读），存进 `device.mac`，供换 IP 时找回。

### 改进

- **设置项不吃滚轮**：设置页的组合框和滑条不再响应鼠标滚轮（滚动页面时指针停在控件上会顺手改掉选项，实测把「布局」从 US 滚成 UK）；滚轮现在只会滚动页面。
- **设置说明重写**：19 条说明从“一句话概念”改成“是什么 + 什么时候调 + 例子/默认值”，例如吸附边会说明平板在电脑下面该选哪项、上报频率什么时候该降。

### 测试

- `tests/test_locator.py`（新，11 例）：地址拆分、MAC 归一、mDNS 解析、ARP 表解析、`locate()` 的在线/换端口/换 IP/缺端口/离线五种结论。
- `tests/test_adb.py`：`wifi_mac()` 读 `wlan0`、扫描接口兜底、非法输出返回空串。
- `tests/test_ui_smoke.py`：标题是「设备列表」、记忆行可移除（含 `device.serial` 改指下一条）、探测结论显示在记忆行、设置页控件忽略滚轮。
- 全量 398 passed, 3 skipped；`tools/acceptance.py` 32 项 0 失败 3 跳过。

## [0.3.12] — 2026-11-05

### 新增

- **关闭窗口的行为可选**：设置页「⚙ 杂项」新增「关闭窗口时最小化到托盘」。勾选后点窗口 X 只收进系统托盘（弹一次气泡，进程继续在后台跑），从托盘菜单选「退出 PadLend」才真正退出；不勾选（默认）保持原来的“点 X = 完全退出”。配置键 `ui.close_to_tray`。
- 关窗入口统一成 `MainWindow.request_close()`：原生 `WM_CLOSE` 过滤器、`closeEvent`、托盘「退出」都走同一条判定，重复触发是幂等的。

### 测试

- `tests/test_ui_smoke.py`：`ui.close_to_tray` 默认值与存盘往返；设置页勾选后保存会写进文件；设置打开且托盘可用时 `request_close()` 只隐藏不退出、设置关闭时立即退出、托盘不可用时即使勾选也退出。
- 全量 380 passed, 3 skipped；`tools/acceptance.py` 32 项 0 失败。

## [0.3.11] — 2026-11-05

### 新增

- **设备记忆**：连接过的设备会记进 `device.known`（新的在前，最多 8 条），设备页在它当前扫不到时也列一行「上次连接的设备」，点「连接此设备」即可重连。无线设备重连前会先补一次 `adb connect`，平板重启换了端口也不会干等到超时。
- **记住上次设备**：启动时即使 adb 看不到它，列表里也还留着那一行，不再“一开软件上次的设备就没了”。

### 修复

- **启动位置**：每次打开都摆到当前屏幕可用区域的正中间（`MainWindow.center_on_screen()`，`main.py` 在 `show()` 后调用）。
- **图标不统一**：`assets/padlend.ico` 重新用运行时那套 `brand_pixmap` 画法渲染（`tools/make_icon.py`，10 档尺寸），exe 图标与窗口/任务栏图标现在是同一枚。小尺寸写 BMP/DIB 条目、只有 256 用 PNG——全 PNG 的 ICO 在资源管理器里可能沿用旧缓存或显示空白；资源管理器仍显示旧图标时，删掉图标缓存或把 exe 改名/换目录即可。

### 测试

- `tests/test_config.py`：`device.known` 存盘往返、缺省为空、怪类型能被强转成字符串。
- `tests/test_ui_smoke.py`：设备页记忆行渲染且不与扫描结果重复、`_remember_device` 去重/上限/排序、`center_on_screen` 落在屏幕中心、`assets/padlend.ico` 结构与尺寸校验。
- `tools/acceptance.py` 32 项 0 失败。

## [0.3.10] — 2026-11-05

### 修复

- **进接管时平板被“敲”了一下（抖音里命中 R = 不感兴趣）**：`misc.clipboard_sync` 默认打开，而每次进入接管都会把 PC 剪贴板整段 `INJECT_TEXT` 过去；服务端把文本用 `KeyCharacterMap.getEvents()` 翻成**真实按键事件**打进当前焦点窗口，于是刷抖音时剪贴板里的 `` R `` 就成了快捷键，输入法/提示条也跟着闪一下。现在**接管瞬间不再发送**，剪贴板只在用户显式按 `Ctrl+Alt+V` 时发送（`docs/DESIGN.md` 6.8 同步更新）。

### 测试

- `tools/acceptance.py` 新增「进入接管不会向平板打字」：断言假平板既没收到 `texts` 也没收到 `keys`。
- `tests/test_ui_smoke.py` 新增 `test_entering_capture_does_not_push_the_clipboard`：`IDLE → CAPTURED` 不得触发 `clipboardSyncRequested`。

## [0.3.9] — 2026-11-04

### 修复

- **还没滑到平板顶边就穿回电脑**：平板是相对设备，系统不暴露指针坐标，位置只能由 PC 端镜像。镜像从 `(0,0)` 起步，设备上的指针却可能停在屏幕中间，于是进入时那一串 warp 只把「镜像」送到了入口边，真实指针还在里面；用户往里推一点再拉回来，镜像先碰到边界就交还了光标。现在进入接管时沿入口边法线补一段**整轴位移**：设备在边界会丢掉多余的位移，最坏也只是指针贴着边停住，镜面与真实位置就此对齐，之后每一次进出都从同一条边开始计数。

### 测试

- `tools/acceptance.py` 增至 31 项断言（新增「进入时把指针顶到入口边」，实测 `sweep=-1520 need=768`），0 失败 3 跳过。
- 368 passed, 3 skipped；新增路由器用例断言入口补位覆盖整轴且落在边界上。

## [0.3.8] — 2026-11-04

### 修复

- **穿屏时平板上某个按钮被按了一下（0.3.7 之后仍然出现）**：进入接管时不再假设平板还停在前一次会话的松键状态——队列里先补一条 `buttons=0` 的全松再发 warp，把异常退出/重连/换设备遗留的「左键按着」清掉，否则进入后的第一次位移会被平板当成拖拽、松手就点中控件。

### 诊断

- `tablet.session` 在按键状态变化时打一条 `tablet.mouse.btn 0x…`，接管期按下/抬起同时记 `cap.btn`、`cap.cursor`、`cursorwatch.btn`。用户报「平板上按钮被触发」时，这一组日志能直接区分：真的是我们发出去的按下，还是 Raw Input 停摆后由 `GetAsyncKeyState` 补的按下，或者压根与我们无关（平板侧的 hover 效果）。

### 测试

- `tools/acceptance.py` 增至 30 项断言（新增「进入时先补一条全松」），0 失败 3 跳过。
- 367 passed, 3 skipped；新增 `tests/test_session_mouse.py`（按键日志只在变化时打、滚轮标志位不会落进 HID 报告）与路由器入口全松用例。

## [0.3.7] — 2026-11-04

### 修复

- **右上角 RTT 一直是 0ms**：`Metrics.record_latency()` 从来没有调用方，延迟样本永远为空，`snapshot()` 又把"没有样本"报成 `rtt_ms=0.0`，界面就把它当真实值显示成「RTT 0ms」。现在每条鼠标报告入队时打时间戳、写入 socket 成功后记一次延迟（设备端没有 ack，量的是"输入进队 → HID 写入成功"这一段），并且**没有样本时整个 `rtt_ms/p50_ms/p95_ms` 键都不出现**，界面显示「RTT —」。
- **穿屏瞬间会触发一下平板上的按钮**：按住鼠标在平板上拖动时退出接管/断连/故障，路由器只发键盘"全松"、不发鼠标"全松"，平板就一直保留「左键按着」；下次穿屏的位移被当成拖拽，松手时正好点中入口处的控件。现在退出接管、emergency stop、FAULT、`stop()` 四条路径都会补发 `buttons=0` 的鼠标报告。

### 测试

- `tools/acceptance.py` 增至 29 项断言（新增「指标里有真实 RTT 样本」「按住离开平板后不再有拖拽位移」「重新进入时平板没有残留按下」），0 失败 3 跳过。
- 364 passed, 3 skipped；新增路由器用例覆盖延迟采样与交还时的全松报告。

## [0.3.6] — 2026-11-04

### 新增

- **`--simulate`：没有平板也能跑完整链路**。用内存里的假平板（`tablet/simulated.py`，含假 ADB）替代 scrcpy/adb，设备页会列出并连接一台「Simulated Tablet」，穿透、点击、拖动、交还都照常走真实状态机与遮罩。
- **`tools/acceptance.py`：agent 可自证的端到端验收**。24 项断言覆盖「接管 → 遮罩吞 `WM_SETCURSOR` → `ClipCursor` 钉在边缘条 → 单击/连点/双击不踢出 CAPTURED → 拖动持续上报 → 交还后遮罩销毁并解锁」，退出码即结论；`--json` 输出可存档。本机光标不可注入/不可观测时会明确 SKIP，而不是假装通过。
- **`cap.cursor` 诊断日志**：接管期每次按下/抬起记录 `visible=… clip=… windows=… active=…`，用来分辨"电脑上又冒出鼠标"是遮罩被拆、锁被系统收回，还是外部程序把光标显示回来。

### 修复

- 设备列表每行只显示型号、序列号被省略号吃掉：`QListWidget::item` 的样式规则让默认代理只画一行，改为「型号 + 序列号/传输/状态」两行行部件。
- 设备列表在 QSS 下与 `--simulate` 的假设备一起首次有了真实内容，顺带修掉行内边距导致的第二行被裁切。

### 测试

- 361 passed, 3 skipped；`tools/acceptance.py` 24 项 0 失败（3 项因本机光标不可观测而 SKIP）。

## [0.3.5] — 2026-11-04

### 新增

- **设备页「添加设备」（无线调试）**：设备列表右上角新增入口，填平板「无线调试」里的 IP 与端口即可 `adb connect`；首次配对时加填 6 位配对码，主按钮随之变成「配对并连接」（先 `adb pair` 再 `adb connect`）。
- 地址容错：可粘贴 `adb connect 192.168.1.8:5555`、`adb://…`、`tcp://…` 这类整行，自动去前缀、去引号、缺端口补 5555；格式不对当场在对话框里提示，不发起后台任务。
- 空状态提示拆成 USB / 无线两行，直接告诉用户端口随平板重启会变、该点哪里。

### 修复

- **「未检测到设备」时无从下手**：无线调试的端口每次重启平板都会变，旧版只会对着配置里失效的 `192.0.2.10:37495` 每 40s 重连失败。现在可手动添加/改地址，失败原因直接显示在设备页顶部。
- **顶栏与系统标题栏重复**：删掉顶栏的「—」「×」（系统标题栏本来就有最小化/关闭），标题栏图标由绿色状态圆点换成品牌标；顶栏只留菜单、设置、明暗切换三个按钮。
- **退出后仍有后台残留**：
  - 关闭主窗口现在等于退出应用（原先只是藏到托盘，进程、Raw Input 线程、adb 一直留着），托盘「退出 PadLend」走同一条路径；
  - 模态对话框（添加设备/诊断/向导）开着时点 X，Qt 会拒绝关闭请求、`closeEvent` 根本不触发，进程就留在后台——新增原生 `WM_CLOSE` 过滤器接管退出；
  - 退出时顺带 `adb kill-server`，不再遗留 fork-server 进程；清理流程幂等，`aboutToQuit` 与 `finally` 双重触发也只跑一次。

### 测试

- 355 passed, 3 skipped（新增 `normalize_address` 参数化、`connect`/`pair`/`kill_server` 失败路径、添加设备对话框校验与按钮文案切换、顶栏不再有最小化/关闭按钮）。
- 实测：源码与打包版关闭窗口后进程退出、`adb server 已停止`；模态对话框打开时发 `WM_CLOSE` 同样退出。

## [0.3.4] — 2026-11-03

### 修复

- **鼠标偶尔无法从平板返回**：移除“先深入平板 160px 才能返回”的入口保护；出口只判断入口边、连续外推阈值和平板拖拽状态。角落现在直接检查入口边坐标，不再被单边命中优先级遮挡。
- **向下穿屏触发意外拖拽**：穿屏前已经按住的按键不再于平板入口重新按下，物理松键后才恢复按键转发；Raw Input 静默期间由系统按键状态补齐漏掉的松键。
- **松键报告可能被队列丢弃**：真实全零松键报告与空闲心跳显式区分，发送队列达到软阈值后仍保留全部真实报告；主动断开后停止无意义的心跳。
- **跨屏合并边锁错显示器**：按实际穿越位置选择 `ClipCursor` 所在显示器；补齐 `PostMessageW` 的 64 位句柄签名。

- **系统光标被永久隐藏（只有开始菜单能看到）**：0.3.3 用进程级 `ShowCursor(FALSE)` 计数隐藏光标，遮罩窗口又可能因为跨线程 `DestroyWindow` 失败而泄漏；任一计数/窗口失衡都会让整个桌面看不到光标，只有开始菜单等更高层窗口能绘制自己的光标。
- 现在彻底移除 `ShowCursor`：光标只由遮罩窗口的 `WM_SETCURSOR` → `SetCursor(NULL)` 隐藏，遮罩销毁即恢复。
- 跨线程释放改为 `PostMessageW(WM_CLOSE)`，由创建窗口的线程执行 `DestroyWindow`；`Runtime.stop()` 调整为先释放遮罩、再停 Raw Input 线程，保证窗口所属线程仍能处理关闭消息。
- 新增回归测试：`set_cursor_hidden` 不再触碰 `ShowCursor`；跨线程 release 使用 `WM_CLOSE`；真实遮罩 show/hide 后 `GetCursorInfo.flags` 从 0 恢复为 1。

### 测试

- 334 passed, 3 skipped；`ruff check .` 全绿。

## [0.3.3] — 2026-11-03

### 修复

- **点击输入框后兜底接管失效（滑不进平板）**：0.3.2 把兜底事件注入 rawinput 线程时，同时刷新了“真实 Raw Input 最近事件时间”，导致 CursorWatch 误以为 Raw Input 已恢复，每 250ms 才补一帧；ARMING 的 80ms 窗口必然超时，表现为一直 `arm.fail timeout accum=0`。
- 现在注入事件不再刷新该时间戳：Raw Input 静默期间兜底按 20ms 连续补帧，Raw Input 一恢复立即停止；新增回归测试。

### 测试

- 326 passed, 3 skipped（新增“注入不刷新 Raw Input 年龄”测试）。

## [0.3.2] — 2026-11-03

### 修复

- **接管后无法穿回、点击后黑屏卡死**：0.3.1 的光标兜底在轮询线程直接调用 Router；若接管正是由兜底事件触发，全屏遮罩窗口会归属没有消息泵的轮询线程。窗口无法处理鼠标消息，点击后整个应用挂起（Windows AppHang），同时 PC 光标被隐藏、平板指针停住，表现为“两边光标都消失、无法回电脑”。
- 兜底事件改为注入 Raw Input 线程再分发（`RawInputSource.inject()` + `WM_APP` 唤醒），遮罩窗口始终归属有消息泵的线程；新增线程归属回归测试。

### 测试

- 325 passed, 3 skipped（新增注入事件在 rawinput 线程分发、停止后拒绝注入两组测试）。

## [0.3.1] — 2026-11-03

### 修复

- **点击 Windows 输入框后无法再滑入平板**：部分焦点/桌面切换会让 Raw Input 在最后一次按键事件后静默（日志中不再有 `cap.start`/`arm.fail`，但系统光标仍在移动）。新增 `win/cursor_watch.py`：仅在 Raw Input 超过 250ms 没有事件时轮询绝对光标位置并合成 `RawDelta`，交给 Router 继续做边缘判定；Raw Input 恢复后立即让位，不重复计数。日志以 `cursorwatch.fallback` 记录兜底启用。
- 按键位图由 `GetAsyncKeyState` 补齐，避免兜底期间丢失按住状态。

### 测试

- 322 passed, 3 skipped；新增 CursorWatch 的静态 Raw、Raw 正常、CAPTURED 跳过三组测试。

## [0.3.0] — 2026-11-03

### 修复

- **换一块显示器就无法接管**：配置里的吸附段在多屏桌面上只是一块屏的一条边；用户在另一块屏的同朝向边缘（例如主屏底部）向外推时，Router 因“边不匹配”直接静默返回。现在只要方向相同，会自动把摆位切换到光标所在的那条边（`router.placement_adopt`），平板只有一块，用户不需要记住上次摆在哪块屏。
- 配合 0.2.9 的慢推兜底（`arm_slow`），打字后慢慢向下推也能接管。

### 测试

- 319 passed, 3 skipped；新增跨显示器同朝向边缘自动接管测试。

## [0.2.9] — 2026-11-03

### 修复

- **打字后/慢推时无法接管**：ARMING 原来只给 80ms 攒 40px（相当于要求 500px/s 的甩动速度），轻推或从静止慢慢压到屏幕边缘时会被判超时；日志中实际只攒到 19px。现在超时前若已向外推出 ≥12px，仍会接管（`arm_slow`），不再依赖甩动速度。

### 测试

- 318 passed, 3 skipped；新增慢推接管回归测试。

## [0.2.8] — 2026-11-03

### 修复

- **拖拽刚开始就中断**：发送线程在队列为空时每帧发心跳 `[0,0,0,0]`，把按住的左键持续冲成“松开”，Android 随即结束拖拽。心跳现携带当前按键位图（CAPTURED 期间），非 CAPTURED 仍发全零；新增 `cap.btn` 与 `cap.start`/`cap.end` 的 `buttons=` 日志便于取证。

### 测试

- 317 passed, 3 skipped；新增心跳保持左键按下的回归测试。

## [0.2.7] — 2026-11-03

### 修复

- **滚轮过快**：Windows 一格滚轮是 120 单位，HID 一格是 1；此前把 120 原样写进报告，平板上会滚动 120 格。现按 120 单位累计换算成 1 格并保留余数（鼠标与 `--headless` 路径一致）。
- **键盘转发阻塞鼠标**：键盘发送从 Router 锁内移到锁外，socket 写入变慢时不会再卡住 Raw Input 的鼠标事件。
- **键盘操作后无法再次接管**：键盘输入或回程去抖结束时刷新一次绝对光标，避免 `_pc_cursor` 长期停留在旧位置；同时为接管失败增加 `arm_skip` 诊断日志（仅 debug）。
- **左键拖动**：按住鼠标键期间不再评估出口，避免拖动/框选时把指针推回入口边就跳回 PC；补充按钮位持续上报与拖动不中断的回归测试。

### 测试

- 316 passed, 3 skipped；新增滚轮换算、键盘锁外发送、按键位拖动上报、拖动期间不穿回测试。

## [0.2.6] — 2026-11-03

### 界面

- **每个设置都有中文说明**：平板位置、手感、键盘、快捷键、杂项下的所有控件都增加一行次要色说明，解释作用、方向与推荐值，不再只靠控件名猜测。
- **新增“进入保护”滑杆**：对应 `motion.entry_guard_px`，可直接调整“进入平板后先向内移动多少像素才允许穿回”，设为 0 可关闭；此前只能手改配置文件。
- 杂项改为两列排布，托盘/指针幽灵与剪贴板/保持亮屏分别对齐，右侧不再大片留白。

### 测试

- 310 passed, 3 skipped；设置页说明与进入保护控件通过 UI 冒烟测试。

## [0.2.5] — 2026-11-03

### 界面

- **设置页不再挤在左侧**：改用左右两列等分布局（平板位置/键盘 | 手感/快捷键，杂项跨两列），设置项铺满可用宽度；去掉外层重复滚动与 760px 限宽，也去掉与页面标题重复的面板内标题。
- “保存设置”改为通栏按钮，滚动区只包设置内容，长页面滚动更自然。

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
