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
