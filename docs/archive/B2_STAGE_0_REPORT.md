# B2 阶段 0 验收报告（测量 + spike + 门禁骨架）

> 日期 2026-11-14；基线 1.4.0（HEAD `cb240b3` + 本阶段未提交改动）。
> 目标：先回答"Python 要不要换"，并把后续每一步的门禁跑起来。

## 1. 数据

### 1.1 onefile vs onedir（5 次取中位，`--simulate --no-hooks`，临时 PADLEND_HOME）

| 指标 | onefile（现发布形态） | onedir | 差异 |
|---|---|---|---|
| 盘上体积 | **14.51 MB** | **31.92 MB** | onedir 大 2.2× |
| spawn → `ready` | **366.6 ms**（p95 368.5） | **146.3 ms**（p95 148.5） | onedir 快 220 ms |
| `app.bootstrap` 往返 | 0.5 ms | 0.4 ms | 持平 |
| 常驻 RSS（进程树） | 39.6 MB | 31.2 MB | onedir 少 8.4 MB |
| 私有内存 | 21.5 MB | 20.2 MB | 持平 |
| 进程数 | 2（引导父 + 真身） | 1 | — |

**解读**：onefile 的 366 ms 里约 **220 ms 是解压**（每次启动都解到 `%TEMP%\_MEIxxxx`），Python 本身冷启动约 146 ms。
所以"换 Rust"能拿到的启动收益 ≈ 360 ms（对 onefile）/ ≈ 140 ms（对 onedir），**不是** 2.5 倍那种口径。
若要的是启动速度，先切 onedir 就能拿走 60%；若要的是体积/依赖，才需要 Rust。

### 1.2 跨语言门禁基线

| 门禁 | 结果 |
|---|---|
| G1（无头）Raw Input 字节解码 | **18/18 一致**（`cargo test`，Python 生成真值） |
| G1（活体）计数对拍 | **未执行**：桌面已锁，SendInput 返回 ERROR_ACCESS_DENIED（需人工解锁，属权限类介入） |
| G3 25 方法 golden trace（Python 自检） | **25/25**（证明 harness 正确） |
| G3 25 方法 golden trace（Rust 现状） | **1/25**（仅 `app.shutdown`，其余 "not implemented"） |

## 2. 已完成

1. `padlend-core/` crate：NDJSON 服务壳、`--debug-raw`、Raw Input 捕获线程、`decode_raw_input`、25 方法表。
2. 三个测量/对拍工具 + 两套 fixture（解码向量 18 条、RPC trace 25 条）。
3. `tools/build_sidecar.py` 增加 `--mode onefile|onedir` 与 `--distpath`（默认不变，发布链路无影响）。

## 3. 风险与未覆盖

| 项 | 说明 |
|---|---|
| 活体计数门禁 | 需解锁桌面；已拆成无头（已绿）+ 活体（待人工）两层 |
| pyinstaller 环境 | PyPI 直连不稳定，venv 依赖阿里云镜像安装；CI 无需改（CI 走官方源） |
| Rust 依赖版本 | `windows 0.62.2` 需要 MSRV 1.82（crate 已设），与 `src-tauri` 的 1.77.2 无关 |
| 25 方法只比结构 | 值差异（端口/路径/时间戳）默认不算失败，M5 前要补"值白名单"对比 |

## 4. 回退方式

本阶段**没有改动**任何前端协议、外壳代码或 Python 实现；回退 = 删除 `padlend-core/`、`tools/*`、fixture 与两份文档即可。

## 5. 下一步（M0）

1. `--debug-raw` 与 Python 的活体计数对拍（需解锁桌面，作为人工项排队）；
2. 把 Raw Input 线程的计数改到窗口用户数据（`GWLP_USERDATA`），为多实例/测试铺路；
3. 录制 RawDelta 流（G2 的输入素材），为 M2/M3 的回放门禁准备。
