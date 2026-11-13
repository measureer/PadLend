# PadLend 1.4.0

跨设备共享键鼠，鼠标从屏幕边缘穿到平板上，平板显示真实的系统指针，键盘和剪贴板双向同步。

**1.4.0 汇总了 1.3.0 与 1.4.0 两个版本的改动**，从 1.2.0 升级的用户可以一次拿到「换网络后仍能找回同一台平板」和「可选地把平板声音转发到电脑」。

## 1.3.0：换 Wi-Fi、换 IP 后仍认成同一台平板

旧版用 adb serial 当设备身份，无线调试的 serial 是 `IP:端口`；换网络后地址变了，同一台平板就会被当成新设备，列表里出现两行。

- 新增 `devices.json` 设备注册表，身份优先使用 `ro.serialno` 生成的 `hw:` id；读不到硬件号时才用端点临时记录，拿到硬件号后原地升级。
- 一台设备可以挂 USB 和多个历史无线端点；换 Wi-Fi、换 IP、换调试端口后仍是一行。
- 启动/重连只在当前设备自己的端点之间切换：当前端点 → 按 `last_ok/last_seen` 排序的历史端点 → mDNS/ARP 线索；无线端点先做 0.6 秒 TCP 预检，避免失效地址把首连拖 40 秒。
- 连接后校验硬件序列号，发现是另一台平板时拒绝并换下一个端点，不会把设备认错。
- 添加设备支持主机名、`host:port`、`[IPv6]:port` 和裸 IPv6；跨网或 VPN 只要电脑能访问调试端口即可。
- 旧 `config.yaml` 的 `device.known/macs` 只在首次启动时迁移一次，之后由 `devices.json` 接管。
- 设备页显示最后在线日期，并按稳定 id 去重、保持选中。

## 1.4.0：可选地把平板声音转发到电脑

设置页新增 **转发平板声音**（`misc.forward_audio`，默认关闭）。

- 开启并重新连接后，平板按 scrcpy `audio_source=output` 接管声音输出，平板自身不再外放。
- Python sidecar 从独立的音频 socket 读裸 PCM（48 kHz / 双声道 / 16-bit LE），经 `127.0.0.1` TCP 送到 Tauri 外壳；Rust 用 cpal 在本机默认输出设备播放。
- 音频通道与控制通道分离，音频缓冲/断流不会阻塞键鼠。
- 音频初始化失败（例如 Android 11 以下）会自动丢弃服务端并按 `audio=false` 重连一次，不影响键鼠接管。
- 音频开关在连接建立时固定；修改设置只落盘并提示「重新连接平板后生效」。
- 需要 Android 11+；Android 11 首次使用可能需要解锁并确认系统弹窗；会转发全部系统声音，DRM 内容可能无声。

## 其他修复

- **保持平板亮屏**改用临时调大 `screen_off_timeout`，不再改开发者选项的「保持唤醒状态」；连接期间设为 5 小时，断开/退出还原，异常退出下次启动补还原。
- **teardown 单步失败不再跳过后续清理**：cursor_watch 失败仍会停 rawinput，raw_debug 失败仍会清理 adb；`KeyboardInterrupt/SystemExit` 也会先完成全部清理再抛出。
- 新增真实父进程退出测试：Tauri 崩溃后 sidecar 会走同步 teardown 再退出。
- 鼠标「穿回阈值」默认值从 120 调整为 80。
- 删除设备页上一段冗余说明文字。

## 升级提示

- 首次启动会自动把旧 `config.yaml` 里的设备记录迁移到 `devices.json`；迁移后旧字段保留但不再读写。
- 音频默认关闭，不会改变现有用户的行为；开启后需要重新连接平板。
- 1.2.0 的托盘退出、接管期 Win 键吞掉、孤儿遮罩清理继续有效。

## 验证

- Python：`pytest` **488 passed / 3 skipped**；`ruff check .` 通过。
- 前端：`vitest` **43 passed / 9 files**；`tsc --noEmit` 通过。
- 原生外壳：本机 VS 2022 Build Tools（MSVC 14.44 + Windows SDK 10.0.26100）`cargo check` / `cargo build` 通过；GitHub Actions 会再跑一次 `npx tauri build --no-bundle`。
- 音频真机试听仍未验证：当前环境没有可用的 Windows 音频端点；需 Android 11+ 平板与有声卡的 Windows 验收。

## 下载

- `PadLend-1.4.0-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.4.0-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.4.0-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
