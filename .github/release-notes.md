# PadLend 1.7.3

这一版修的是「明明连上了，界面却说没连上」，顺带把设置页的分组重排了一下。

## 修了什么

### 1. 打开应用就自动连上了平板，界面却一直显示「未连接」

auto_connect 打开时，supervisor 线程会在启动后自己把设备连上，可界面里的设备详情、标题栏、托盘全停在「未连接」，连接按钮也还是灰的——实际上平板已经可以被控制了。

根因：设备事实（分辨率、方向、序列号这些）的镜像开关只挂在 devices.connect 这个 RPC 里，而自动首连根本不走它。supervisor 连上以后，事实轮询线程每次都在第一行就提前返回，bootstrap 的 facts 恒为 null，facts 事件也从没推出去过。

修法：引擎启动时如果 auto_connect 已经打开，就先打开事实镜像；手动连接那条路径保持原样。这样无论谁连上的，界面都能拿到事实。

### 2. 设置页：手感 → 鼠标，按列重排

- 原来的「手感」分组改名为「鼠标」。
- 左列放「鼠标」「键盘」「快捷键」三块，右列单独放「杂项」。
- 窗口窄于 1180px 时两列自动叠回单列，不会出现挤成两条窄条的情况。

## 升级提示

- 覆盖安装或解压覆盖即可；config.yaml、devices.json 格式没有变化。
- 1.7.2 的音频修复都在，声音不需要重新设。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 引擎：cargo test **541 passed / 13 ignored**。
- 前端：vitest **44 passed / 9 files**，tsc --noEmit 通过。
- 独立引擎 + auto_connect=true：不调 devices.connect，8 秒后 bootstrap 返回 connected=true、facts.serial=192.168.68.129:35071（3200×2136）。
- 设置页真机截图确认左列三块、右列一块。

## 下载

- PadLend-1.7.3-setup.exe — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- PadLend-1.7.3-win64.zip — 免安装包，解压后运行 padlend-desktop.exe
- SHA256SUMS.txt — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.3-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 /dev/uhid 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
