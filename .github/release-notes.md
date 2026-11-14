# PadLend 1.7.0

纯 Rust 版本：Python 业务服务已整体移除，安装包里只剩外壳与引擎两个可执行文件，构建与发版也只需要 Rust + Node。

## 这一版做了什么

- **Python 业务服务整体删除**：`core/`、`win/`、`tablet/`、`service/`、`padlend/`
  以及 `pyproject.toml` / `requirements*.txt` 全部从项目中移除。从 1.6.0 起这两个实现
  就只在 Rust 侧维护，这一版把已经停止维护的那份彻底清掉，避免两套 RPC 契约继续漂移。
- **安装包瘦身**：不再捆绑 14 MB 的 `padlend-service.exe`，也不再包含 CPython 运行时和
  PyInstaller bootloader，`LICENSES/` 相应少了四份 Python 相关许可证。
- **构建/发版脚本改用 PowerShell**：`tools/build-core.ps1` 编译引擎并放到 Tauri 期望的
  位置，`tools/release.ps1` 一条命令走完前端、引擎、测试、安装包、免安装 zip 与校验和。
  CI 与 Release 工作流同步去掉 Python 步骤，新增独立的 Rust 引擎测试 job。

## 修复

- **SHA256SUMS.txt 不再混入历史产物**：以前的发版脚本会把 `dist/` 里所有文件都算进校验和，
  包括 1.1.x 留下的旧安装包；现在打包前先清掉旧产物，只对本次发布的两个文件计算。
- **tag 校验只看一个版本号**：发版脚本与 Release 工作流统一读 `src-tauri/tauri.conf.json`。
  1.6.0 时 `padlend/__init__.py` 的 `__version__` 还停在 1.4.0，旧校验会让 `v1.6.0` 的
  tag 构建直接失败。

## 升级提示

- 覆盖安装或解压覆盖即可；`config.yaml` 与 `devices.json` 的格式没有变化，配置保留。
- 引擎仍是从 Shell 同目录的 `padlend-core.exe` 启动；如果手动替换过文件，注意不要留下旧版本。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 引擎：`cargo test` **538 passed / 13 ignored**（含 router 场景、UHID 报文、Raw Input 解码三组对拍基线）。
- 前端：`vitest` **44 passed / 9 files**，`tsc --noEmit` + `vite build` 通过。
- 本机 `npx tauri build` 产出 `PadLend_1.7.0_x64-setup.exe`，引擎 `--simulate` 的
  `system.paths` 返回 `version: 1.7.0`。

## 下载

- `PadLend-1.7.0-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.7.0-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.0-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
