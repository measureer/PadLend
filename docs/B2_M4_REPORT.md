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
