# 归档文档（1.x / Python 时代）

这里的文档记录 PadLend **1.x 时期**的实现与迁移过程，**已不再代表当前代码**。
保留它们是为了留存决策依据与历史数据；新改动请写进 `CHANGELOG.md` 或对应现行文档。

现行文档：

- [../../README.md](../../README.md)：项目现状、安装、配置与排障入口；
- [../CHANGELOG.md](../CHANGELOG.md)：逐版本变更（权威）；
- [../TROUBLESHOOTING.md](../TROUBLESHOOTING.md)：用户排障；
- [../DESIGN.md](../DESIGN.md)：1.0.x 设计稿（仅协议与 Win32 设计部分仍作参考）；
- [../TAURI_MIGRATION.md](../TAURI_MIGRATION.md)：界面迁移记录（历史，结论已被全 Rust 重写推翻）；
- [../PITFALLS.md](../PITFALLS.md)：开发期踩坑（部分条目仍是 Python 时代，按代码为准）。

| 文件 | 内容 | 为什么过时 |
|---|---|---|
| `B2_MIGRATION_PLAN.md` | 2026-11 把 Python 引擎迁到 Rust 的 B2 计划（阶段划分与门禁） | 迁移已完成；文中「Python 回退路径常驻仓库」在 1.7.0 已删除 |
| `B2_STAGE_0_REPORT.md` | B2 阶段 0：onefile/onedir 启动测量、Rust 骨架 | 测量对象（PyInstaller 打包）已不存在 |
| `B2_M1_REPORT.md` | B2 阶段 1：NDJSON RPC 与常量迁移 | 已完成，模块布局已被 Rust crate 取代 |
| `B2_M2_REPORT.md` | B2 阶段 2：几何/布局对拍 | 已完成，fixture 已固化在 `padlend-core/tests` |
| `B2_M3_REPORT.md` | B2 阶段 3：Raw Input、钩子、遮罩 | 已完成 |
| `B2_M4_REPORT.md` | B2 阶段 4：ADB/UHID/会话 | 已完成 |
| `B2_M5_REPORT.md` | B2 阶段 5：切换默认引擎与回退链 | 回退到 `python -m service.server` 的分支已删除 |
| `RUST_BACKEND_FEASIBILITY.md` | 迁移前对「要不要用 Rust 重写」的可行性评估 | 决策已执行；评估结论只作历史参考 |
| `GPUI_RUST_FEASIBILITY.md` | GPUI 作为 GUI 框架的可行性评估 | 最终选的是 Tauri 2，GPUI 未采用 |
| `RELEASE_READINESS.md` | 1.x 发布门禁与检查单 | 门禁已换成 `cargo test` + `vitest`，且无 `crash-*.log`/`padlend.log` |
| `IMPLEMENTATION_NOTES.md` | Python 实现期的模块笔记（`constants.py`、`core/geometry.py` 等） | Python 模块已整体删除，文件名不再存在 |
| `VERIFICATION.md` | 到 1.4.0 为止的真机验收记录 | 之后的版本记录在 `CHANGELOG.md`；文中的 pytest/ruff 门禁已不适用 |

> 注：`docs/PITFALLS.md` 顶部与 1.2 节的链接已改为 `archive/IMPLEMENTATION_NOTES.md`；
> 文中其它 `文件:行` 仍是 Python 时代的坐标，按当前 Rust 代码搜索同名概念即可。
