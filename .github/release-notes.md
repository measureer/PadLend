# PadLend 1.7.4

只加了一个开关：**设置 → 杂项 → 打开软件自动连接上次连接的设备**。

## 为什么加

这个开关本来就存在（配置里的 auto_connect），但它只在首次运行向导里出现过一次。装好之后再想关掉自动连接、或者想重新打开，界面上找不到入口，只能去改 config.yaml。

现在它固定放在杂项第一项：

- 开着：启动后自动连上次用过的那台设备（地址就存在配置里，无线调试换了地址也能自己找回来）。
- 关掉：启动后停在设备页，需要手动点「连接此设备」。

这项在引擎启动时读取，所以**保存后下次启动生效**——当前这次运行里已经连着的设备不会因为关掉开关就断开。

## 升级提示

- 覆盖安装或解压覆盖即可；config.yaml、devices.json 格式没有变化，已有设置原样保留。
- 1.7.2 的音频修复、1.7.3 的自动连接状态与设置页分列都在。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 前端：vitest 45 passed / 9 files（含新增的开关用例），tsc --noEmit 通过。
- 引擎：cargo test 541 passed / 13 ignored（本次未改引擎）。
- 真机：关掉开关重启应用 → 停在设备页不自动连接；打开开关重启 → 自动连到 HONOR CGA-W00。

## 下载

- PadLend-1.7.4-setup.exe — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- PadLend-1.7.4-win64.zip — 免安装包，解压后运行 padlend-desktop.exe
- SHA256SUMS.txt — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.4-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 /dev/uhid 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
