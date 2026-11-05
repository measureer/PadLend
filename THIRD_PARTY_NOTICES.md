# PadLend 第三方软件声明（THIRD PARTY NOTICES）

PadLend 1.0.0 的 Windows 发行物（`PadLend.exe` + `_internal/`）包含下列第三方
组件。本文件随每一个 zip / 安装器分发；各许可证全文位于同目录的 `LICENSES/`
（安装后在 `{app}\LICENSES\`）。

PadLend 自身以 Apache License 2.0 发布，见仓库根目录 `LICENSE`（发行物内为
`LICENSES/Apache-2.0.txt`）。

## 组件清单

| 组件 | 版本 | 许可证 | 版权 | 源码 / 许可全文 |
| --- | --- | --- | --- | --- |
| Qt for Python（PySide6、shiboken6） | 6.12.0 | LGPL-3.0-only（或 GPL-2.0/3.0，双许可） | Copyright (C) The Qt Company Ltd. and contributors | <https://download.qt.io/official_releases/QtForPython/> · `LICENSES/LGPL-3.0.txt`、`LICENSES/GPL-3.0.txt` |
| Qt 6 运行库（QtCore/QtGui/QtWidgets/QtNetwork/QtSvg/QtDBus 等） | 6.12.0 | LGPL-3.0 | Copyright (C) The Qt Company Ltd. and contributors | <https://download.qt.io/official_releases/qt/6.12/> · `LICENSES/LGPL-3.0.txt` |
| PyYAML | 6.0.3 | MIT | Copyright (c) 2017-2021 Ingy döt Net; 2006-2016 Kirill Simonov | `LICENSES/MIT-PyYAML.txt` |
| psutil | 7.2.2 | BSD-3-Clause | Copyright (c) 2009 Giampaolo Rodola | `LICENSES/BSD-3-Clause-psutil.txt` |
| PyInstaller（打包工具；其 bootloader 被嵌入 exe） | 6.22.3 | GPL-2.0-or-later，附 bootloader 例外 | Copyright (c) 2010-2023 PyInstaller Development Team 等 | `LICENSES/PyInstaller-COPYING.txt` |
| CPython 运行时 | 3.13 | PSF License 2.0 | Copyright (c) Python Software Foundation | `LICENSES/Python-3.13-PSF.txt` |
| scrcpy-server（Android 端） | 5.0.1 | Apache-2.0 | Copyright (C) Romain Vimont and scrcpy contributors | <https://github.com/Genymobile/scrcpy> · `LICENSES/Apache-2.0.txt` |
| adb / Android platform-tools | 见 `vendor/adb/README.md` | Apache-2.0 | Copyright (C) 2007-2025 The Android Open Source Project | `LICENSES/adb-NOTICE.txt`、`LICENSES/Apache-2.0.txt` |
| Microsoft Visual C++ 运行库（vcruntime140.dll 等） | 14.x | Microsoft Visual Studio 可再分发组件条款 | Copyright (c) Microsoft Corporation | 随 Windows / Visual Studio 许可分发，非开源组件 |

## Qt / PySide6（LGPLv3）合规说明

PadLend 使用**动态链接**方式使用 Qt：发行物为 onedir 形态，Qt 的 DLL 以普通
文件位于 `_internal\PySide6\` 内（如 `_internal\PySide6\Qt6Core.dll`），启动时不复制、
不覆盖、不加密，最终用户可以直接替换。
这满足 LGPLv3 对“可替换库”（replaceable library）的要求。

- **未修改 Qt / PySide6 / shiboken6**。上游源码获取地址：
  - Qt for Python：<https://download.qt.io/official_releases/QtForPython/>
  - PySide6 源码仓库：<https://code.qt.io/cgit/pyside/pyside-setup.git/>
  - Qt 6.12.0：<https://download.qt.io/official_releases/qt/6.12/>
- **替换方法**：关闭 PadLend，用同一 Qt 6.12.x、同一 ABI（MSVC 2022 x64）的
  `Qt6*.dll` 替换 `_internal\PySide6\` 下同名文件，再启动即可。若替换的是 PySide6
  的 Python 扩展（`_internal\PySide6\*.pyd`），请使用与 Python 3.13 匹配的构建。
- 若需要以 GPL 方式使用 Qt，`LICENSES/GPL-3.0.txt` 提供 GPLv3 全文；PadLend
  本身不启用 GPL 选项。
- Qt 的商业许可选项见 <https://www.qt.io/licensing/>；本发行物未购买商业许可，
  按 LGPLv3 条款分发。

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
