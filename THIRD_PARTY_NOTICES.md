# PadLend 第三方软件声明（THIRD PARTY NOTICES）

本文件随 PadLend 1.1.0 的每个 zip / 安装器分发；各许可证全文位于同目录的
`LICENSES/`。PadLend 自身以 Apache License 2.0 发布，见仓库根目录 `LICENSE`
（发行物内为 `LICENSES/Apache-2.0.txt`）。

1.1.0 起界面改为 **Tauri 2 + React/TypeScript**：不再包含 Qt / PySide6 /
shiboken6，因此不再随包分发 Qt DLL，也不再适用 LGPLv3 的「可替换库」条款。
Windows 上的 WebView2 运行时由操作系统提供（安装器会在缺失时引导安装），
不随本发行物分发。

## 组件清单

| 组件 | 版本 | 许可证 | 版权 | 源码 / 许可全文 |
| --- | --- | --- | --- | --- |
| Tauri 2（`tauri`、`tauri-build`、官方插件） | 2.x | MIT / Apache-2.0 双许可 | Copyright (c) Tauri Programme within The Commons Conservancy | <https://github.com/tauri-apps/tauri> |
| React / React DOM | 19.x | MIT | Copyright (c) Meta Platforms, Inc. and affiliates | <https://github.com/facebook/react> |
| zustand | 5.x | MIT | Copyright (c) Paul Henschel | <https://github.com/pmndrs/zustand> |
| Vite / TypeScript / @tauri-apps/api 与插件 | 见 `package.json` / `package-lock.json` | MIT / Apache-2.0 | 各项目作者 | 各 npm 包自带的 LICENSE |
| Rust crates（间接依赖） | 见 `src-tauri/Cargo.lock` | MIT / Apache-2.0 / 其他宽松许可 | 各 crate 作者 | <https://crates.io/>；`cargo tree` 可列出完整依赖树 |
| WebView2 Runtime | 随 Windows / Edge | Microsoft 软件许可条款 | Copyright (c) Microsoft Corporation | 系统组件，不随包分发 |
| PyYAML | 6.0.3 | MIT | Copyright (c) 2017-2021 Ingy döt Net; 2006-2016 Kirill Simonov | `LICENSES/MIT-PyYAML.txt` |
| psutil | 7.2.2 | BSD-3-Clause | Copyright (c) 2009 Giampaolo Rodola | `LICENSES/BSD-3-Clause-psutil.txt` |
| PyInstaller（仅用于生成 Python 业务 sidecar） | 6.22.3 | GPL-2.0-or-later，附 bootloader 例外 | Copyright (c) 2010-2023 PyInstaller Development Team 等 | `LICENSES/PyInstaller-COPYING.txt` |
| CPython 运行时（sidecar 内） | 3.13 | PSF License 2.0 | Copyright (c) Python Software Foundation | `LICENSES/Python-3.13-PSF.txt` |
| scrcpy-server（Android 端） | 5.0.1 | Apache-2.0 | Copyright (C) Romain Vimont and scrcpy contributors | <https://github.com/Genymobile/scrcpy> · `LICENSES/Apache-2.0.txt` |
| adb / Android platform-tools | 见 `vendor/adb/README.md` | Apache-2.0 | Copyright (C) 2007-2025 The Android Open Source Project | `LICENSES/adb-NOTICE.txt`、`LICENSES/Apache-2.0.txt` |

> 说明：Tauri 外壳是静态编译的 Rust 可执行文件；Python 业务 sidecar 由
> PyInstaller 打包，其 bootloader 适用 GPL 例外条款，不影响 PadLend 自身代码。

## scrcpy-server 重打包声明（Apache-2.0 §4(b)）

`vendor/scrcpy-server-v5.0.1.jar` 由官方 v5.0.1 release 重打包而来：**仅**将
`classes.dex` 从 deflate 改为 stored（不压缩），以规避 Android 17 上
`app_process`/ART 通过 `CLASSPATH` 加载压缩 dex 时的 `ClassNotFoundException`；
其余条目逐字节保持原样，未修改任何代码。重打包脚本：
`tools/repack_server_jar.py`。文件哈希见 `vendor/SHA256SUMS` 与
`vendor/README.md`。

## adb / platform-tools

`vendor/adb/platform-tools/` 为 Android SDK Platform-Tools 的原样拷贝
（adb.exe、AdbWinApi.dll、AdbWinUsbApi.dll、libwinpthread-1.dll 等），
按 Apache-2.0 分发，原始 `NOTICE.txt` 一并保留为 `LICENSES/adb-NOTICE.txt`。

## 其他

- PadLend 的日志、配置不包含上述组件的源代码；本文件不构成法律意见。
- 如发现遗漏的组件或许可证文本，请提交 issue，我们会在下一个发行版补齐。
