# B2 阶段 M4 验收报告（平板连接层）

> 日期 2026-11-14。提交：`ede1fa2`。

## 1. 数据

| 模块 | 行 | 测试 |
|---|---|---|
| `tablet/session.rs` | 3541 | 31（impl `router::SessionPort`） |
| `tablet/adb.rs` | 1983 | 34 |
| `tablet/server.rs` | 1082 | 20 |
| `tablet/locator.rs` | 899 | 19（1 个真连 `#[ignore]`） |
| `tablet/mdns.rs` | 639 | 10（1 个真绑定 5353 `#[ignore]`） |
| `tablet/simulated.rs` | 615 | 7 |
| `tablet/audio.rs` | 450 | 6 |
| `tablet/cursor.rs` | 322 | 13 |

crate 累计 467 → 服务层完成后 518 个测试，全绿。

## 2. 保住的“看起来像 bug”的行为

- `verify_clean` **先解析计数、后看退出码**：`dumpsys input | grep -c scrcpy` 在干净时退出码是 1，按退出码先判会把干净设备误判成脏的。
- `parse_pointer_speed("null") == 0`：把“未设置”当成 0 是既有语义，改了会破坏复查逻辑。
- 发送模型：Rust 侧改成队列 + 1000Hz flush（`SessionConfig::auto_flush`），Python 是直写；Router 若自己驱动节拍需关闭自动 flush 并每帧调 `sender_frame()`。

## 3. 与原实现的差异

| 项 | 说明 |
|---|---|
| 朝向监听 | Python 常驻一条 `adb shell` 循环；Rust 改成每 1s `dumpsys input` 轮询 + `apply_orientation`，避免跨线程杀子进程 |
| `ScrcpyServer::kill` | 返回 `Result`，`teardown` 里按“不阻断后续步骤”忽略 |
| mdns | 无 `SO_REUSEADDR`（不引 socket2）：5353 被占时退到临时端口并置 QU 位 |

## 4. 未验证（需要真机）

- 真机连接 / 接管 / 交还各 3 次（M4 的出口标准）；
- `--simulate` 下 25 个 RPC 已全绿，但真机的 `adb forward`、`UHID` 创建、音频通道握手（PITFALLS 2.6）只能在平板上验证。

## 5. 回退

`git revert ede1fa2`；Python 侧未改。

## 6. 真机验证（2026-11-14，EasyTier 异地组网）

用户解锁桌面后首次跑真机；平板与 PC 不在同一局域网，经 EasyTier 组网。

| 步骤 | 结果 |
|---|---|
| `adb pair 10.144.144.5:39613 161243` | **成功**（guid `adb-AM4NBB5C23001691`） |
| `adb connect 10.144.144.5:43465` | **成功** |
| 设备 | HONOR CGA-W00，Android 17（SDK 37），arm64-v8a，tablet |
| `devices.scan`（Rust sidecar） | 能列出该设备 |
| `devices.connect`（Rust sidecar） | **失败**：`连接超时：请确认 USB 调试已授权，且 adb 能看到设备`（50s） |
| `devices.connect`（Python sidecar） | **同样失败**，错误码/文案一致 |

两侧 stderr 都是 `router.connect_failed attempt=1..6 code=E101 detail=连接后健康检查失败`。
**结论：不是 Rust 移植回归**——Python 原实现表现完全相同。

### 已定位到的现象

1. 手动用同一套参数起服务端（`app_process ... 5.0.1 tunnel_forward=true video=false audio=false control=true cleanup=true raw_stream=true send_dummy_byte=true max_size=4096`）**能正常打印**
   `[server] INFO: Device: [HONOR] HONOR CGA-W00 (Android 17)` 并驻留 6s 以上。
2. 先 sleep 1.5s 再 `adb forward tcp:1234 localabstract:scrcpy` 并连接，**能收到握手哑字节 `\x00`**。
3. 但应用自己的 `devices.connect` 期间轮询设备，**始终看不到 app_process 进程**，`adb forward --list` 只有 1 秒左右的窗口，
   随后被 teardown（`pkill -9 -f com.genymobile.scrcpy.Server` + `forward --remove tcp:1234` + `dumpsys input | grep -c scrcpy`）清掉。
4. 说明：`start()`（push → kill → spawn → forward）之后，握手/健康检查**在 100ms 量级就失败**，随即 teardown 杀掉刚起的服务端。

### 待查（不在 B2 移植范围，属产品既有行为）

- 握手重试是否被 `ECONNREFUSED`/`EOF` 直接短路（`HANDSHAKE_RETRY_TIMEOUT_S=8.0` 没起作用）；
- `forward` 在服务端尚未监听时建立，首连必然 EOF；
- Android 17 + scrcpy 5.0.1 的兼容性（手动能起、能被连上，倾向时序问题）。

建议下一步：① 先试 USB 连接（排除 EasyTier 延迟因素）；② 若必须无线，在 `tablet/session.py` 临时加一行异常日志定位，再决定修法。

## 7. 真机回归通过（2026-11-14，EasyTier 网络）

### 7.1 根因：不是环境问题，是连接路径的竞态

失败时的 Rust 侧日志（`padlend-core` 的 stderr）给出了完整链条：

```
+0.01s router.start_waiting code=E101 detail=尚无设备信息；连接由 supervisor 线程异步完成
+1.09s state.fault code=E106 detail=连续 3 次写失败: E106: 控制通道未建立或已断开
+3.77s tablet.server 启动服务端: ... app_process ... scrcpy.Server 5.0.1 ...
+3.85s tablet.server adb forward tcp:1234 → localabstract:scrcpy
+3.85s tablet.session 启动失败: 启动已取消 (E105)
+4.60s router.connect_failed attempt=1 code=E105 detail=启动已取消
+6.61s router.connect_failed attempt=1 code=E101 detail=连接后健康检查失败
```

`router.start` 在 `auto_connect=true` 时会让 supervisor 线程**自动发起连接**；紧接着 `devices.connect`
又显式调用 `router.connect_device()`，后者对**正在进行的 start** 调用了 `cancel_start()` →
会话在握手阶段收到"启动已取消"（E105）→ 清理杀掉刚起的服务端 → 显式连接拿到的是一条已死的通道。

Python 原实现的行为完全相同（同样的 E105/E101 序列、同样的超时文案），所以这是**产品既有竞态**，
被 EasyTier 的每次 adb 往返 300–700ms 放大到稳定复现；USB 本地快得多，窗口小到通常撞不上。

### 7.2 绕过竞态后的真机结果

`tools/device_regression.py` 先写 `config.yaml`（`device.auto_connect: false`）关掉自动连接，
再走显式 `devices.connect`，全流程通过：

| 步骤 | 结果 |
|---|---|
| `devices.connect` | **ok**（6.7–9.8s） |
| `engine.state` | `connected: true`，facts: 3200×2136、xscale 1.312、landscape |
| 接管 ×3 | ok |
| 接管期指标 | `send_hz = 996 / 992 / 994`，`p95 = 0.1 / 0.1 / 0.2 ms` |
| 交还 ×3 | ok |
| 退出 | 遮罩与 ClipCursor 已释放（`GetClipCursor` 回到 `0,0,1705,987`） |

**1000Hz 发送链路在 EasyTier 上实测成立**：`send_hz≈993`、`p95≈0.1–0.2ms`、`cpu_pct≈16.6`。

### 7.3 已修复（2026-11-14）

`core/router.py` 与 `padlend-core/src/logic/router.rs` 同步修改，两处：

1. `connect_device()` 先判断"同一 serial 的连接是否正在进行"（`_connect_in_progress` / `connect_in_progress`）：
   是则只刷新重试时间并返回，**不 stop、不 cancel_start**；目标 serial 不同才走停机切换。
2. `_note_write_failure` / `note_write_failure`：会话还没有 `DeviceFacts` 时的写失败直接忽略
   （不再 1 秒就把状态打成 E106 FAULT）。

回归测试：Python `tests/test_router.py` 三个用例（同 serial 复用、不同 serial 仍切换、未连上时写失败不 FAULT），
Rust `logic::router` 同款三个用例；两边全绿（Rust 521 passed / 13 ignored、Python 491 全绿、golden trace 25/25）。
`tools/device_regression.py` 默认改为 `auto_connect=true`（真实用户路径），`--no-auto-connect` 保留用于对照。

**第一次修复没修好**：复跑仍报 E105。日志显示进行中的连接目标是空串（配置里还没写 serial 的自动首连），
而显式连接的目标是具体 serial，两者"不相等"于是又被当成切换设备。补丁随后放宽为：
**进行中目标为空（自动首连）或与目标相同 → 一律复用**，并把目标 serial 写回配置。

**真机复验（auto_connect=true，端口 10.144.144.5:34739）**：

| 项 | 结果 |
|---|---|
| `devices.connect` | **ok，3.6s**（修复前 48s 超时） |
| 接管 / 交还 ×3 | 全 ok，`send_hz = 993 / 983 / 988`，`p95 = 0.1` |
| 状态迁移 | connect→idle、capture→captured(manual=true)、release→idle（1s 内） |
| 退出 | 遮罩与 ClipCursor 释放正常 |

### 7.4 原建议（保留作背景）

1. `Router::connect_device` 在"同一 serial 的 start 正在进行"时**不要** `cancel_start`，直接等它完成或复用；
2. 或让 `devices.connect` 先等 supervisor 的 in-flight 连接结束（同 serial 时返回其结果）；
3. 顺带：`send_mouse` 在会话未就绪时不应计为"写失败"（当前 1.09s 就把状态打成 E106 fault）。

修复后 `auto_connect=true` 的默认路径应当即恢复；在修复前，真机回归用 `auto_connect=false` 可稳定通过。
