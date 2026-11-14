# B2 迁移执行计划（Python 引擎 → Rust sidecar）

> 决策依据：`docs/RUST_BACKEND_FEASIBILITY.md`（2026-11-14，1.4.0 基线）。
> 执行契约：用户 m00105 授权 **B2 路线自主推进到 M5**；M6 单独立项，不自动执行。

## 1. 执行契约（不可协商的边界）

| 项 | 要求 |
|---|---|
| 目标 | Rust 版 sidecar（`padlend-core.exe`）实现与 Python 版**相同**的 25 个 RPC 方法与事件 |
| 架构 | 保持**双进程 + NDJSON**（sidecar 可回退、可 A/B）；**不**并进 Tauri 单进程 |
| 前端 | 协议一行不改（方法名/事件名/NDJSON 语义保持不变） |
| Python | 回退路径常驻仓库，直到 M5 全绿且用户明确同意才谈退役 |
| 推进 | 门禁全绿自动进入下一阶段；失败自动修复重试 |
| 升级人工 | 同一门禁连续 2 次失败；超预算/工期；需真机插拔、权限、签名、杀软；协议语义歧义；M6 决策 |

## 2. 四条门禁（每阶段都要跑）

| # | 门禁 | 判定命令 | 通过标准 |
|---|---|---|---|
| G1 | Raw Input 计数等价 | `tools/raw_input_probe.py --only rust`（活体）＋`cargo test rawinput_vectors`（无头） | 字节级解码 18/18 一致；活体注入计数 \|rust−python\| ≤ 2 |
| G2 | Raw Input 回放状态一致 | （M2 起）录制 RawDelta 流回放，比对 router 状态迁移序列 | 逐条一致，无缺漏 |
| G3 | 25 个 RPC golden trace | `tools/rpc_trace.py replay --exe …` | 25/25 |
| G4 | 真机回归可自动化项 | `tools/acceptance.py` + 真机脚本 | 自动化项全绿；不可自动化项列清单交人工 |

> G1 的活体部分需要**解锁的桌面**（当前机器桌面已锁，SendInput 返回 ERROR_ACCESS_DENIED）；
> 无头部分（G1 的 `cargo test`、G3、G4 的可模拟项）随时可跑。

## 3. 里程碑

| 里程碑 | 内容 | 出口 | 状态 |
|---|---|---|---|
| 阶段 0 | onefile/onedir 测量 + sidecar spike + 门禁骨架 | 测量数据、Rust 骨架能跑通 NDJSON、G1 无头部分、G3 基线 | **已完成**（见 `B2_STAGE_0_REPORT.md`） |
| M0 | Raw Input 计数等价（`--debug-raw`） | G1 全绿 | 进行中 |
| M1 | 纯逻辑层（config/geometry/metrics/uhid/keymap/protocol） | 对应单测 + G3 增量 | 待开始 |
| M2 | Win32 层（desktop/dpi/mask/rawinput/keyboard_hook/hotkeys/single_instance） | G1、G2 全绿；遮罩/ClipCursor 手工项 | 待开始 |
| M3 | router 状态机 + sender + supervisor | G2 全绿；1000Hz p95 不劣化 | 待开始 |
| M4 | tablet 层（adb/locator/mdns/server/session/cursor） | 真机连接/接管/交还各 3 次 | 待开始 |
| M5 | RPC 层 + 打包 + 发布 + A/B | G3 25/25；安装/卸载/升级；A/B 数据 | 待开始 |
| M6 | 并入 Tauri 单进程 | — | **不执行**（单独立项） |

## 4. 工件

| 工件 | 用途 |
|---|---|
| `padlend-core/` | Rust sidecar crate（lib + bin，`cargo test` 可跑） |
| `tools/bench_sidecar.py` | onefile/onedir 冷启动与内存测量 |
| `tools/raw_input_probe.py` | Raw Input 计数对拍（SendInput 注入） |
| `tools/gen_rawinput_vectors.py` | 生成跨语言解码向量 `tests/fixtures/rawinput_vectors.json` |
| `tools/rpc_trace.py` | 25 方法 golden trace 录制/回放/比对 |
| `tests/fixtures/rpc_golden/simulate.ndjson` | Python 侧录制的 golden trace |

## 5. 回退与 A/B

1. **回退**：外壳按 `padlend-core.exe` → `padlend-service.exe` → 开发期 `python -m service.server` 的顺序查找；删掉前者即回退到 Python。
   （查找顺序的改动属于 M5，本次未改任何前端/外壳代码。）
2. **A/B**：同一台机器上分别用两个 exe 跑 `bench_sidecar.py` 与 `rpc_trace.py`，对比 ready/p95/RSS 与 25 方法结果。
3. **Python 回退路径**：`service/`、`tools/build_sidecar.py` 均保持不变（只加了 `--mode/--distpath` 两个可选参数，默认行为不变）。

## 6. 阶段报告模板

每阶段在 `docs/B2_<阶段>_REPORT.md` 写：**数据**（门禁输出、测量值）、**风险**（新发现 + 未覆盖项）、**回退方式**（如何退回上一状态）、**下一步**（下一阶段入口条件）。

## 7. 最终状态（2026-11-14）

| 项 | 状态 | 证据 |
|---|---|---|
| 阶段 0 | 完成 | `B2_STAGE_0_REPORT.md`（onefile 366.6ms/14.51MB vs onedir 146.3ms/31.92MB） |
| M0 | **完成** | 活体计数对拍 PASS（rust=100 / python=100 / delta=0） |
| M1 | **完成** | 524 Rust 测试、274 项跨语言字节对拍 |
| M2 | **完成** | 7 个 Win32 模块；13 个 `#[ignore]` 全通过，ClipCursor 无泄漏 |
| M3 | **完成** | 状态机 5/5 场景回放 |
| M4 | **完成** | 真机 connect 6.7–9.8s、接管×3、交还×3、send_hz≈993、p95≈0.1–0.2ms |
| M5 | **完成** | G3 25/25、parity 0 差异、externalBin 打包、A/B 数据 |
| 提交 | — | `bb33e4a` `80cc63c` `ede1fa2` `aec645a` `f7c57a9` `ba79b55` `ddef42a` `b1b28c7` `1d5796a` `673d54d` `1bb5fa3` |
| 遗留（非移植） | 产品竞态 | `auto_connect=true` 与显式连接互 cancel；修法见 `B2_M4_REPORT.md` §7.3 |
