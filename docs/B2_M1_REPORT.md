# B2 阶段 M1 验收报告（纯逻辑层移植）

> 日期 2026-11-14。门禁证据：`build/gates/M1.json`（`tools/run_gates.py --stage M1`）。

## 1. 数据

| 门禁 | 结果 |
|---|---|
| rust-tests | **PASS**：167 个 `#[test]` 全绿；另 1 个集成测试（rawinput 向量）+ 5 个（tablet 向量） |
| python-suite | **PASS**：411 个 pytest 全绿（移植没有误伤原实现） |
| rpc-golden | **4/25**（`app.shutdown`、`system.paths`、`config.get`、`config.save`）——本阶段 `min_required=0`，只作进度指标 |
| 跨语言字节对拍 | **PASS**：Raw Input 解码 18 条；tablet 层 6 鼠标报文 + 6 键盘报文 + **256 条 VK→HID 映射** + 9 修饰位 + 9 协议帧，逐字节与 Python 一致 |

模块清单（有效行 / 测试）：

| 模块 | 行 | 测试 | 说明 |
|---|---|---|---|
| `src/logic/geometry.rs` | 1377 | 20 | Rect/EdgeSegment/MonitorInfo/Placement、进出点、角落裁决 |
| `src/logic/metrics.rs` | 711 | 15 | 窗口速率、P50/P95、drop_pct、CPU/内存采样（trait 注入） |
| `src/logic/state.rs` | 234 | 5 | 5 状态 + 合法迁移表（冻结契约） |
| `src/tablet/uhid.rs` | 432 | 16 | 4/8 字节 HID 报告、6KRO、KeyboardState |
| `src/tablet/keymap.rs` | 909 | 31 | VK→usage、修饰位、直通键、Keymap 状态机 |
| `src/tablet/protocol.rs` | 764 | 23 | scrcpy 控制协议编解码（含半包/截断） |
| `src/config.rs` | 2380 | 32 | YAML 结构与默认值、修复、原子写、36 个 Python 生成的 case |
| `src/constants.rs` | 725 | 10 | 路径解析、scrcpy/adb 常量、单实例名 |
| `src/win/process.rs` | 124 | 3 | GetProcessTimes/GetProcessMemoryInfo 采样 |

## 2. 已知偏差（需要 M5 前确认）

| 偏差 | 影响 | 处置 |
|---|---|---|
| serde_yaml_ng 是 YAML 1.2，PyYAML 是 1.1 | `01234` 解析成 1234（Python 668）、`1_000` 保持默认 | 手改配置才触发；已在 config.rs 头登记 |
| 重复键：PyYAML 取后者，Rust 报 duplicate → 备份 + 重置默认 | 手工写坏 YAML 时的行为不同 | 已有 fixture 固化，风险低 |
| 备份时间戳用 UTC（Python 用本地时） | 差 8 小时 | 升级兼容无影响；要逐字节一致需加 `Win32_System_SystemInformation` |
| `config.save` 没有逐字段强转与错误文本 | 非法补丁的报错粒度不同 | M5 前对齐 `apply_config_patch` |
| `KeyEvent` 在 keymap.rs 内自带一份 | M3 接线时与 router 的事件类型要统一 | M3 落到 `logic/events.rs` |
| HID 常量暂驻 `uhid.rs` | 与 constants.rs 重复 | M2 合并时迁走 |

## 3. 风险

- **gate 假绿**已修：`rpc_trace.py` 原先只比 `ok` 布尔，`layout.save`（Python 期望报错）被"两边都报错"混过；现在 `not implemented:` 一律判失败，且错误文本必须一致。
- **配置补丁路径**没有 Python 的逐字段强转，属"能用但不完全等价"，已列入 M5 待办。
- M2/M3 的 M1 依赖已就绪（几何、状态表、UHID、协议、配置都能被 router/session 直接调用）。

## 4. 回退方式

删除 `padlend-core/src/{logic,tablet}`、`config.rs`、`constants.rs` 与对应 fixture 即可回到 M0 状态；Python 侧未改动。

## 5. 下一步（M2）

1. 合并三路 Win32 层（desktop/dpi、mask/cursor_watch、keyboard_hook/hotkeys/single_instance）；
2. 跑 `tools/run_gates.py --stage M2`（含活体门禁；桌面锁屏时会标 blocked）；
3. 准备 M3：把 `KeyEvent`/`RawDelta` 统一到 `src/logic/events.rs`，并按 `tests/fixtures/router_scenarios.json` 建立回放门禁。
