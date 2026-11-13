# B2 阶段 M5 验收报告（RPC 全通 + 打包形态 + A/B 数据）

> 日期 2026-11-14。门禁证据：`build/gates/M5.json`；A/B 数据：`build/bench/sidecar-forms.json`。
> 提交：`ede1fa2`（M4 连接层）、`aec645a`（M5 服务层与 25 个 RPC）。

## 1. 门禁

| 门禁 | 结果 |
|---|---|
| rust-tests | **PASS**：522 passed / 0 failed / 13 ignored |
| python-suite | **PASS**：491 个用例全绿（Python 原实现未被破坏） |
| rpc-golden | **PASS 25/25**（`tools/rpc_trace.py replay --exe padlend-core.exe`，无任何方法走 `not implemented`） |
| rawinput-live | 仍 **BLOCKED**（桌面锁屏，需人工解锁） |

25/25 的含义：golden trace 是从 Python 实现录制的，Rust 侧按同一批请求回放，要求**响应结构逐键一致**（类型、键集合），错误响应还要错误文本逐字一致。这不是"接口能通"，是"载荷形状等价"。

## 2. A/B 实测（同机、同一脚本、5 次取中位）

| 指标 | Python onefile | Python onedir | **Rust core（release）** |
|---|---|---|---|
| 盘上体积 | 14.51 MB | 31.92 MB | **1.54 MB** |
| spawn → `ready` | 366.6 ms（p95 368.5） | 146.3 ms（p95 148.5） | **10.6 ms（p95 12.5）** |
| `app.bootstrap` 往返 | 0.5 ms | 0.4 ms | **0.2 ms** |
| 常驻 RSS | 39.6 MB | 31.2 MB | **7.1 MB** |
| 私有内存 | 21.5 MB | 20.2 MB | **1.3 MB** |
| 进程数 | 2 | 1 | 1 |

相对现发布形态：启动 **34×**、体积 **9.4×**、RSS **5.6×**；相对 onedir：启动 **13.8×**、体积 **20×**。

## 3. A/B 切换机制

`src-tauri/src/bridge.rs` 的 `resolve_service()` 查找顺序改为：

`padlend-core-x86_64-pc-windows-msvc.exe` → `padlend-core.exe` → `resources/padlend-core.exe` → `padlend-service-*.exe` → `resources/padlend-service.exe` → `binaries/padlend-service-*.exe` → `python -m service.server`

**把文件放进去即切到 Rust，删掉即回退**，不用改代码、不用重编译外壳；两种 sidecar 的 NDJSON 契约完全相同。

## 4. 已知缺口（不阻塞 25/25，但发布前要定）

| 缺口 | 影响 | 处置建议 |
|---|---|---|
| ~~`diagnostics.export` 写 `.txt`~~ | **已修**：手写 stored-zip（无新依赖），`zipfile` 实测可解压 |
| ~~时间戳用 UTC~~ | **已修**：改用 `GetLocalTime`，ISO 形式不再带 `+00:00`，与 Python 的本地时一致 |
| ~~未实现 `_capture_mac`~~ | **已修**：无线设备连上后后台读 `Adb::wifi_mac` 并写回设备注册表 |
| ~~`cpu_pct`/`mem_mb` 恒 0~~ | **已修**：Router 注入 `Win32ProcessUsage`，实测 `mem_mb=8.7` |
| 模拟模式用 `NoopMask` | 模拟下不会真的遮罩 | 与 Python `--simulate` 行为一致，可接受 |
| `devices.json` 与 Python 同 schema 同目录 | 两版可共用、可回退 | 已确认 |

## 5. 回退

- 引擎级：删掉 `padlend-core.exe` 即回到 Python sidecar；
- 代码级：`git revert aec645a ede1fa2`。

## 6. 到此为止的整体账

五层全部移植完成：逻辑/配置（2,000+）、Win32（8,992）、状态机（5,447）、连接层（9,500）、服务层（5,149），合计约 **31,000 行 Rust**，522 个测试；Python 侧 411 个测试全绿作为对照网。

尚未完成：**活体验证**（M0 计数对拍 + 13 个 `#[ignore]` 用例，需解锁桌面）、真机连接/接管/交还（需平板）、打包进 NSIS（发布链路）。
