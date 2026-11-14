# PadLend 1.7.1

修掉 1.7.0 里「开『转发平板声音』就连不上」的问题。改动只在引擎的平板会话层，外壳、配置格式和 RPC 契约都没变。

## 修了什么

从 1.7.0 开始，勾上「转发平板声音」再连接，界面会一直停在「连接中」，约 30 秒后报连接超时。日志里的表现是：音频通道握手正常（重试几次后「音频通道已建立」），控制通道却连续「对端在 dummy 字节前关闭」，随后自动「回退为仅控制通道」，回退之后仍然是几十次同样的失败。根因是两个 bug 叠在一起：

1. **dummy 字节只发给第一条连接**。scrcpy 5.0.1 的 `send_dummy_byte` 只对**第一个** accept 的 socket 生效；开音频时服务端先接音频、再接控制，而引擎让两条通道都去等这个 dummy，控制通道白等满 8 秒 `handshake_retry_timeout` 后被判成「音频通道不可用」。
2. **回退重启没有重新推送 jar**。服务端以 `cleanup=true` 启动，被 kill 时会把 `/data/local/tmp/scrcpy-server-manual.jar` 删掉；而回退路径重启服务端前漏了 `push_if_needed()`，新服务端因 ClassNotFound 立即退出，后面的每次握手都被立刻关闭，一路耗到 `CONNECT_TIMEOUT_S=30` 才失败。

修法：握手层按「只有该通道确实是第一条连接时才读 dummy」处理（开音频时音频读、控制不读；关音频时单通道仍读）；回退重启前先 `push_if_needed()`，用 `adb md5` 确认远端 jar 还在。这样即使音频通道将来再次出问题，也只是静音回退，不会拖死整次连接。

## 升级提示

- 覆盖安装或解压覆盖即可；`config.yaml`、`devices.json` 的格式没有变化，配置保留。
- 如果你之前是靠「关掉转发平板声音」来规避这个问题的，升级后可以重新打开。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 引擎：`cargo test` **539 passed / 13 ignored**（新增「开音频时控制通道不等 dummy」用例，回退用例补了「重启前必须重新推送 jar」的断言）。
- 前端：`vitest` **44 passed / 9 files**，`tsc --noEmit` + `vite build` 通过。
- 真机 HONOR CGA-W00（Android 17）：开启「转发平板声音」连接成功，日志顺序为「音频通道已建立 → 控制通道已建立 → UHID 已创建」，`tablet.connect` 2.2 秒返回 3200×2136。

## 下载

- `PadLend-1.7.1-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.7.1-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.1-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
