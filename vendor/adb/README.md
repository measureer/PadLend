# 内置 adb

`vendor/adb/platform-tools/` 是 Android SDK Platform-Tools 37.0.1 的最小分发子集，
供未在 PATH 中安装 adb 的机器直接运行打包版 PadLend。

| 文件 | 说明 |
|---|---|
| `adb.exe` | Android Debug Bridge 1.0.41 / 37.0.1-15733141 |
| `AdbWinApi.dll`、`AdbWinUsbApi.dll` | adb 的 Windows USB 依赖 |
| `libwinpthread-1.dll` | adb 运行时依赖 |
| `NOTICE.txt` | 上游 NOTICE |
| `source.properties` | 上游版本元数据 |

来源：本机 Android SDK Platform-Tools 37.0.1。更新时替换整个
`platform-tools/` 目录并同步 `vendor/SHA256SUMS`。
许可与再分发条款见 `NOTICE.txt` 及 Android SDK 许可。
