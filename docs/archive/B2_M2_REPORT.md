# B2 阶段 M2 验收报告（Win32 层移植）

> 日期 2026-11-14。门禁证据：`build/gates/M2.json`。提交：`bb33e4a`（M1+M2 checkpoint）。

## 1. 数据

| 门禁 | 结果 |
|---|---|
| rust-tests | **PASS**：295 passed / 0 failed / 11 ignored（ignored 全是真建窗、装钩子、抢互斥量、移动光标） |
| python-suite | **PASS**：491 个用例节点全绿（原实现的回归网没被误伤） |
| rawinput-live | **BLOCKED**：桌面锁屏，`SendInput` 返回 `ERROR_ACCESS_DENIED`，需人工解锁后重跑 |
| rpc-golden | 4/25（进度指标，M5 才要求 25/25） |

本阶段新增模块：

| 模块 | 行 | 测试 | 关键约束 |
|---|---|---|---|
| `win/mask.rs` | 2683 | 38 + 2 ignored | 遮罩独占一条消息线程；`PostMessage(WM_CLOSE)` 释放 + 超时 `ShowWindow(SW_HIDE)` 兜底；`WM_SETCURSOR` 里 `SetCursor(NULL)`；绝不用 `ShowCursor` 计数；`release()` 幂等且无条件 `ClipCursor(None)` |
| `win/cursor_watch.rs` | 850 | 11 | 三后端抽象成 `RouterLike`/`RawInputLike`/`CursorProbe`；CAPTURED 期回读 topmost |
| `win/keyboard_hook.rs` | 1252 | 17 + 2 ignored | `WH_KEYBOARD_LL` 回调只投递不阻塞（PITFALLS 2.5）；单实例 `static ACTIVE` 槽 |
| `win/hotkeys.rs` | 854 | 12 + 1 ignored | `RegisterHotKey` + 消息循环；注册失败只记日志（与 Python 一致） |
| `win/single_instance.rs` | 1280 | 23 + 3 ignored | `Global\PadLend.SingleInstance` + `Activate` 事件；`CreateMutexW/CreateEventW` 走 `GetProcAddress`（未开 `Win32_Security`） |
| `win/desktop.rs` | 1471 | 25 + 1 ignored | 外轮廓边段/角落裁决；搬运 `tests/test_geometry.py` 的 23 条；真实枚举实测 `rect=(0,0,1705,987) dpi=96` |
| `win/dpi.rs` | 601 | 8 + 2 ignored | Per-Monitor V2；`GetDpiForMonitor` 动态解析，shcore 缺失回退 96 |

## 2. 发现的原实现问题（不阻塞迁移，登记备查）

| 位置 | 问题 |
|---|---|
| `win/keyboard_hook.py` | `_request_reinstall` 是死代码：消息循环处理 `WM_APP_REINSTALL`，但没人发这条消息 → **钩子被系统静默摘掉后没有自愈路径** |
| `win/hotkeys.py` | `_stop` 事件只写不读；注册失败只 `log.error`，上层没读 `registered()` |
| `win/single_instance.py` | `signal_running_instance` 每次都 `AllowSetForegroundWindow(-1)`，进程级全局放宽且不恢复 |
| `win/mask.py` | `show(None)` 且未持锁时不解遗留的 `ClipCursor`；`cleanup_orphan_mask` 按类名 `TerminateProcess`，pid 复用可能杀错 |
| `win/cursor_watch.py` | 切到 CAPTURED 后首次 poll 必然注入一次按键快照；`GetAsyncKeyState` 不校验会话，锁屏会误判"已松开" |
| `win/desktop.py` | `set_dpi_awareness()` 非幂等（第二次返回 False）；`int(round())` 是银行家舍入，.5 会差 1px（Rust 已用 `round_ties_even`） |

## 3. 风险

- **活体门禁仍未跑**：M2 的遮罩/ClipCursor/钩子/热键共 11 个 `#[ignore]` 用例需要解锁桌面才能执行；M0 的活体计数同理。
- **接线未做**（M3 一起）：`Router` 实现 `cursor_watch::RouterLike`、`RawInput` 实现 `RawInputLike`（`inject` 必须 `PostThreadMessage` 回 rawinput 线程）、启动调 `cleanup_orphan_mask` 与 `dpi::set_dpi_awareness`（建窗前）、退出调 `unclip_on_exit`。
- 两个 `RawDelta` 副本（mask 内与 `logic/events.rs`）需在 M3 统一。

## 4. 回退方式

M1+M2 是一个 checkpoint 提交（`bb33e4a`）；回退 = `git revert` 该提交，Python 侧完全未改。

## 5. 下一步（M3）

1. `core/router.py` 移植（子智能体进行中），门槛是回放 `tests/fixtures/router_scenarios.json` 的 5 个场景；
2. 统一 `KeyEvent`/`RawDelta` 到 `src/logic/events.rs`；
3. 建立 G2 回放门禁（RawDelta 流 → 状态迁移序列逐条一致）；
4. 解锁桌面后一次性跑：`cargo test -- --ignored` + `run_gates --stage M0/M2`。
