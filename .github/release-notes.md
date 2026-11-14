# PadLend 1.7.2

修掉三件「看着像没生效」的真问题：开着「转发平板声音」但电脑没声音；修完没声音之后声音又变成一串爆音；点了「断开连接」、甚至退出程序，平板还留在 `adb devices` 里。另外给外壳加了日志，以后这类问题不用再猜。

## 修了什么

### 1. 平板静音了，电脑却没有声音

开启「转发平板声音」后，scrcpy 会把平板自己的播放静音（音频被采走了），但电脑这边一点声音都没有。引擎日志只有一句「音频播放器写入失败：远程主机强迫关闭了一个现有的连接」，每次会话真正转发的字节数只有几百字节。

根因在外壳：为了让音频接收线程能定时检查退出标志，监听套接字被设成了非阻塞；而 Windows 上 `accept()` 出来的连接会**继承**这个标志，于是播放线程第一次读就拿到 `WSAEWOULDBLOCK(10035)` 并退出，连接刚建立就断了。**从 1.4.0 起这条音频链路其实一次都没真正播出去过**，只是过去总被「平板静音了」这一半现象掩盖。

修法：连接建立后先把 socket 改回阻塞模式，再用读超时控制等待；读循环另外把偶发的 `WouldBlock` 当「稍等再读」而不是致命错误。

### 2. 声音通了，但电脑里全是爆音/杂音

上一条修完，音频确实播出来了，但听感是一串爆豆似的噪音。抓下引擎写给播放器的字节一看，`PLAU` 头部后面跟着的是 `4F 70 75 73 48 65 61 64`——ASCII `OpusHead`，也就是一整条 **Ogg Opus** 流。

scrcpy 5.0.1 的服务端音频**默认 codec 是 opus**，PadLend 只传了 `raw_stream=true`（那是视频侧的旧开关，5.0.1 已经忽略），于是把 48 kHz/2ch/16bit 的 opus 压缩包当成裸 PCM 直接播——自然是连续的爆音。

修法：服务端命令行在开音频时补 `audio_codec=raw`，让服务端走 AudioRecord 裸流分支、不写 codec 头。同样的 12 秒采集，修复前收到 156 KB（约等于 opus 码率），修复后 1.9 MB（48000×2ch×16bit 应有的量），首字节不再是 `OpusHead` 而是纯采样。

### 3. 断开连接 / 退出程序没有断开无线调试

`adb connect host:port` 建立的链路属于 adb server，而会话 teardown 只做了 pkill 服务端和 `adb forward --remove`，从来没执行 `adb disconnect`——所以平板会一直挂在 `adb devices` 里。

修法：新增 `tablet::adb_links` 登记「PadLend 自己建立的无线连接」；点「断开连接」与退出程序时逐条 `adb disconnect`。你在终端里自己 `adb connect` 的链路不会被登记，也不会被顺手掐掉。

## 新增

- **外壳日志** `%LOCALAPPDATA%\PadLend\shell.log`：Tauri 外壳是窗口程序，没有控制台，以前的 `eprintln!` 等于扔进虚空（上面那个音频问题因此在日志里只留了一半）。现在启动、音频接收器端口、输出设备与采样格式、播放失败原因、panic 位置都会落盘；超过 1 MiB 自动轮转成 `shell.log.1`。回传问题时把 `service.log` 和 `shell.log` 一起发来即可。
- 音频输出补齐 `I8/I32/I64/U8/U32/U64/F64` 采样格式（原来只认 `F32/I16/U16`，遇到只报 `I32` 的声卡会直接失败）。

## 升级提示

- 覆盖安装或解压覆盖即可；`config.yaml`、`devices.json` 格式没有变化。
- 声音默认走系统默认输出设备；如果还是没声音，先看 `%LOCALAPPDATA%\PadLend\shell.log` 里有没有「播放已开始」和输出设备名。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 引擎：`cargo test` **541 passed / 13 ignored**（新增 `tablet::adb_links` 两条用例）。
- 前端：`vitest` **44 passed / 9 files**，`tsc --noEmit` + `vite build` 通过。
- 真机 HONOR CGA-W00（Android 17）：开音频连接后 `shell.log` 为「收到头部 → 输出设备：扬声器 (Realtek(R) Audio) / F32 / 2 ch / 48000 Hz → 播放已开始」，引擎侧不再出现「播放器写入失败」；抓包确认音频 socket 上已是裸 PCM（12 秒 1.9 MB），不再是 Ogg Opus。
- 断开连接与退出程序后，`adb devices` 不再列出该无线端点。

## 下载

- `PadLend-1.7.2-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.7.2-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.2-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
