# PadLend 1.0.0 发版就绪报告

> **版本提示**：本文是 1.0.0 的历史报告，其中 PyInstaller/Inno Setup/Qt 打包流程
> 已在 1.1.0 由 Tauri 2 发布流水线取代；当前发布流程见
> [CHANGELOG](CHANGELOG.md) 与 [.github/workflows/release.yml](../.github/workflows/release.yml)。

> 这是 1.0.0 首发时的报告。1.0.1 是设备地址发现相关的补丁版本，变更与产物见
> [CHANGELOG](CHANGELOG.md) 和 [Releases](https://github.com/measureer/PadLend/releases)。

- 目标版本：1.0.0
- 评估与修复日期：2026-11-05
- 发布渠道：GitHub Releases（Windows x64，个人玩家为主）
- 发布形态：PyInstaller onedir ZIP；可选 Inno Setup 安装器
- 结论：**审计发现的 8 个 P0 全部关闭；P1 中影响首发的项已修复；剩余为签名、硬件矩阵等需要外部资源的事项。**

## 验证基线

| 项目 | 结果 |
|---|---|
| 代码质量 | `ruff check .` 通过，0 error |
| 自动化测试 | 430 个用例收集，427 passed / 3 skipped |
| 版本单一来源 | `padlend/__init__.py` → `constants.VERSION` / pyproject / GUI / exe 版本资源 |
| 打包形态 | onedir；启动不解包到 CWD，可安装到 Program Files 或只读目录 |
| 版本资源 | FileVersion 1.0.0.0、ProductVersion 1.0.0、ProductName / CompanyName / Copyright 齐全 |
| 许可 | 产物根目录含 `THIRD_PARTY_NOTICES.md` 与 `LICENSES/`（8 份全文） |
| 产物 | GitHub Release [v1.0.0](https://github.com/measureer/PadLend/releases/tag/v1.0.0)：ZIP（54.3 MiB）、`setup.exe`、`SHA256SUMS.txt`；发布 ZIP SHA-256 `c90bda8448eff60be6a38297f2aff119e52c464935ce1eab33c703f686618b5c` |
| 冒烟 | 从外部 CWD 启动、自动连真机、UHID 键鼠、正常退出与强制清理均已验证 |

## P0 关闭情况

| # | 原问题 | 状态 | 落地 |
|---|---|---|---|
| R0-1 | 版本三处漂移、exe 无版本资源、无 tag | ✅ 关闭 | 单一版本源；`tools/version_info.py` 生成资源；release 工作流校验 tag 与版本一致 |
| R0-2 | onefile 按 CWD 解包，Program Files 起不来 | ✅ 关闭 | `PadLend.spec` 改为 onedir；删除 `runtime_tmpdir`；发布 zip 解压即用 |
| R0-3 | 未捕获异常零日志 | ✅ 关闭 | `core/crash.py` 安装 excepthook / threading.excepthook / faulthandler；启动失败弹窗并写 `crash-*.log` |
| R0-4 | 配置非 UTF-8 启动即崩 | ✅ 关闭 | 捕获编码/语法/版本/结构错误，备份 `config.yaml.bad-<ts>` 后重置默认配置 |
| R0-5 | Qt/PySide6 LGPLv3 义务与第三方署名 | ✅ 关闭 | onedir 允许替换 `_internal/PySide6`；QGPL 全文与 Qt 源码地址、scrcpy 重打包声明、adb NOTICE、PyYAML/psutil/PyInstaller 声明随包分发 |
| R0-6 | 未签名 / SmartScreen | ⚠️ 已知限制 | 无代码签名证书；发布页提供 SHA-256，README/TROUBLESHOOTING 说明 SmartScreen 与校验方式，并明确未签名。证书到位后 `tools/release.py` 可直接签名 |
| R0-7 | README/截图/向导/VERIFICATION 与实物不一致 | ✅ 关闭 | README、TROUBLESHOOTING、VERIFICATION、DESIGN、CHANGELOG 对齐 1.0.0；删除 Ctrl+Alt+V、修正配置路径/向导步骤/阈值；四张 1280×820 截图重生成 |
| R0-8 | 诊断包“脱敏”名不副实 | ✅ 关闭 | serial/IP/MAC/用户名/用户目录统一替换；zip 增加 README、最近日志与 crash 日志；meta.json 不再含绝对路径 |

### R0-1 版本与构建

- `padlend/__init__.py` 是唯一版本源；`pyproject.toml` 使用 dynamic version；`constants.VERSION` 直接导入。
- `tools/version_info.py` 在构建时生成版本资源，不再手写版本号。
- `tools/release.py` 负责 build / zip / sha256 / 可选 sign / 可选 installer；缺少证书或 Inno Setup 时跳过并明确提示。

### R0-2 启动与安装

- onedir 产物结构：`PadLend.exe` + `_internal/` + `LICENSES/` + `THIRD_PARTY_NOTICES.md`。
- 启动不再依赖 CWD 可写；旧 onefile 在 `dist/` 与仓库根留下的 `_MEI*` 已清理。

### R0-3 崩溃可诊断性

- `crash-YYYYmmdd-HHMMSS-<pid>.log`：版本、argv、Python/平台、线程名、完整 traceback。
- `faulthandler-*.log`：原生崩溃时的全线程栈。
- 启动早期失败也会弹窗并落盘；托盘与“关于”均提供打开日志目录入口。

### R0-4 配置恢复

- 非 UTF-8、YAML 损坏、根节点非 mapping、未来版本、字段越界均走“备份 + 默认值 + issue”。
- 保存使用临时文件 + `os.replace`，并执行 `flush` + `fsync`。

### R0-5 许可与合规

- `THIRD_PARTY_NOTICES.md` 说明 Qt 6.12.0 / PySide6 / shiboken6 的 LGPLv3 授权、动态链接与替换路径。
- `LICENSES/` 含 LGPLv3、Apache-2.0、MIT、BSD-3-Clause、GPLv2 + bootloader exception 等全文。
- “关于 PadLend…”可直接打开第三方许可文件。
- 发布说明需保留一句：“包含 LGPLv3 授权的 Qt 库，源码获取方式见 THIRD_PARTY_NOTICES.md。”

### R0-7 文档与首屏

- 删除已废弃的 `Ctrl+Alt+V` 与 `hotkeys.clipboard`。
- 配置路径统一为默认 `%APPDATA%\PadLend`（不可写时回退）。
- 向导按实际步骤描述（1.2.1 起为连接平板/USB 调试 → UHID → 完成 3 步）；adb 与 scrcpy-server 说明为随包内置；UHID 文案统一为硬条件（不可写即无法创建虚拟鼠标/键盘），不再承诺不存在的绝对坐标降级。
- `exit_overflow_px` 默认 80；补充 `device.known/mac`、`motion.hid_clamp/entry_margin_px`、`ui.close_to_tray`。
- `device.auto_connect` 真正生效（关闭后启动不自动连接）；向导最后一步的“自动连接 / 关闭时收进托盘”会写回配置。
- 截图由 `tools/make_screenshots.py` 用真实 MainWindow 离屏生成，可重复执行。

### R0-8 诊断包

- 新增 `redact_text` / `redact_adb_devices`；日志、配置、adb 输出、dumpsys 片段走同一脱敏路径。
- zip 内新增 `README.txt`、`logs/padlend-tail.log`、`logs/crash-latest.log`。
- UI 明确提示“已脱敏，但设备型号等信息仍可能识别设备”。

## P1 / P2 收敛

已修复的代表项：

- 退出只停止 PadLend 自己启动的 adb server；关闭窗口时清理移到后台线程，GUI 最多等待 8 秒。
- 日志轮转为 2 MB × 3，windowed 下不再安装无效 stderr handler。
- `pointer_speed` 原值持久化到 `state/pointer_speed_state.json`，崩溃后下次启动恢复。
- 推送 scrcpy-server 前按 `vendor/SHA256SUMS` 校验，失败抛 E103。
- 配对码在日志中打码；设备网卡名走白名单；未知配置键告警；设备状态中文显示。
- 增加 GitHub Actions CI（3.11 / 3.13）与 tag 触发的 Release 工作流。
- 修复布局页场景矩形：平板在桌面包围盒外时自动扩展，右侧不再被裁掉。
- 补齐崩溃、配置恢复、诊断脱敏、adb 所有权、pointer 恢复等回归测试。

仍属已知限制、不阻断 1.0.0：

| 项 | 说明 |
|---|---|
| 代码签名 | 本次未购买证书；SmartScreen 会提示“未知发布者”。证书到位后重新签名并附 RFC3161 时间戳 |
| 安装器本机编译 | `installer/padlend.iss` 与 `tools/release.py` 已就绪；本机未安装 Inno Setup，CI 可编译 |
| 设备矩阵 | 实机验证仍以 Honor MagicPad 3 / Android 17 为主；不同 OEM 的 UHID 权限差异需 beta 反馈 |
| 自动更新 | 1.0.0 不含自动更新；“关于 → 检查更新”在配置 Releases 地址后打开 Releases 页 |
| 控制台输出 | windowed exe 不附着控制台，直接运行时看不到 `--version` 输出；重定向（`> out.txt`）或源码运行可读取 |
| 遥测/崩溃上报 | 默认关闭，仅本地诊断包；后续如加入必须 opt-in |

## 发版检查清单

- [x] 版本单一来源，GUI / pyproject / exe 资源一致
- [x] `ruff check .` 通过
- [x] `pytest -q` 通过（427 passed / 3 skipped）
- [x] onedir 构建成功，版本资源与 notices 随包
- [x] ZIP 与 `SHA256SUMS.txt` 生成
- [x] 从外部 CWD 隔离启动冒烟（`PADLEND_HOME` 指向临时目录）
- [x] 用最终代码重建产物并复核 SHA-256
- [x] 本地提交并打 tag `v1.0.0`
- [x] 推送 GitHub、创建 Release，上传 ZIP、安装器与 SHA256SUMS
- [x] 发布说明模板 `.github/release-notes.md`：未签名、LGPLv3/Qt、已验证设备与反馈入口

## 复核方法

```powershell
.venv\Scripts\python -m ruff check .
.venv\Scripts\python -m pytest -q
.venv\Scripts\python tools\release.py --help
.venv\Scripts\python -m padlend --version
```

产物校验：

```powershell
Get-FileHash dist\PadLend-1.0.0-win64.zip -Algorithm SHA256
Get-Content dist\SHA256SUMS.txt
(Get-Item dist\PadLend\PadLend.exe).VersionInfo | Format-List
```
