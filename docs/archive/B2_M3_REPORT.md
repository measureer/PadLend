# B2 阶段 M3 验收报告（跨越状态机移植）

> 日期 2026-11-14。提交：`80cc63c`。

## 1. 数据

| 项 | 结果 |
|---|---|
| `src/logic/router.rs` | **5447 行**（实现 3733 / 测试 1714），36 个 `#[test]` |
| 门槛测试 | `fixture_scenarios_replay_identically` **5/5 场景通过** |
| crate 单测 | lib **340 passed / 11 ignored** |
| 编译告警 | 0（router 模块） |

门槛测试的做法：`tools/router_scenarios.py` 用 Python 的假时钟/假会话驱动真实 Router，把 5 个场景（手动接管+移动、按住、滚轮累加、键盘、交还）的**状态迁移三元组、鼠标报文、按键报文、遮罩矩形、估计器坐标**录进 `tests/fixtures/router_scenarios.json`；Rust 侧逐 op 回放（capture/release=toggle_manual、raw=on_raw、frame=sender_frame、key=on_key），除"fixture 开头可能多一条纯 `[0,0,0,0]` 心跳"外逐条严格相等。

## 2. 结构

- 五个端口：`SessionPort`(`router.rs:324`)、`MaskPort`(:409)、`DesktopPort`(:431)、`KeymapPort`(:483)、`Clock`(:283)。
- `sender_frame()` / `tick_supervisor()` 是公开方法，线程只负责按节奏调用 → 测试可完全确定性推进。
- `src/logic/ports.rs`：`DesktopAdapter`（内套 `Mutex<DesktopMap>`，因为 `refresh()` 要 `&mut`）、`MaskAdapter`（`CursorMask`，ClipCursor 失败映射 E202）。
- `main.rs` 在创建任何窗口前调用 `win::dpi::set_dpi_awareness()`（PITFALLS 5.1）。

## 3. 与 Python 的已知差异

| 差异 | 说明 |
|---|---|
| 回调在锁外派发 | Python 靠 RLock 重入在锁内派发；迁移三元组顺序不变 |
| `connect_device` 不回写 `cfg.device.serial` | 配置不可变，改记 `Router::requested_serial()` |
| `stop()` 无超时 join | std 无 timed join；supervisor 卡在 `session.start()` 时最多等 40s（Python 2s 后放弃） |
| `timeBeginPeriod(1)` | 未开 `Win32_Media`，从 `winmm.dll` 动态解析，失败即 no-op |
| 会话回调注册 | 构造期拿不到 `&Arc<Self>`，改为 `register_session_callback` + Weak 破环 |

## 4. 发现的原实现死锁风险（Rust 侧 1:1 保留并注明）

`_enter_captured_locked` 在**持有 Router 锁**时调 `mask.show()`（建窗 + ClipCursor）；`begin_return`/`_fault` 也在锁内直接 `send_mouse_release`/`send_keyboard_zero`。PITFALLS 2.5 只把键盘转发移出了锁，这三处仍是潜在死锁点（若遮罩窗口的 UI 线程恰好是 RawInput 线程）。键盘**转发**路径已用锁探针测试证明在锁外。

## 5. 未完成（M4 依赖）

- `SessionPort` 的真实实现要等 `tablet/session.rs` 移植完成；
- `Metrics` 仍用 `NullProcessUsage`，`cpu_pct`/`mem_mb` 恒为 0（GUI 指标显示需要，M5 前接 `Win32ProcessUsage`）。

## 6. 回退

`git revert 80cc63c`；Python 侧未改。
