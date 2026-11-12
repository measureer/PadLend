# 故障排查

> **版本提示**：配置/日志路径、adb 与 UHID 排障步骤在 1.1.0 仍然有效；
> 文中提到的 Qt 窗口/托盘行为已改为 Tauri 2 外壳，界面问题另见
> [TAURI_MIGRATION.md](TAURI_MIGRATION.md)。
>
> 本文只保留**用户可执行**的排障步骤；开发期的根因、代码位置与回归测试见
> [PITFALLS.md](PITFALLS.md)。

## 0. 先拿到日志

| 内容 | 位置 |
|---|---|
| 运行日志 | `%APPDATA%\PadLend\logs\padlend.log`（2 MB 轮转，保留 3 个） |
| 崩溃日志 | `%APPDATA%\PadLend\logs\crash-YYYYmmdd-HHMMSS-<pid>.log` |
| 原生崩溃栈 | `%APPDATA%\PadLend\logs\faulthandler-*.log` |
| 配置 | `%APPDATA%\PadLend\config.yaml` |
| 退出标记 | `%APPDATA%\PadLend\exit.clean` |

`%APPDATA%` 不可写时会回退到 `%LOCALAPPDATA%\PadLend` 或程序目录下的 `.padlend\`；设置了 `PADLEND_HOME` 时以它为准。

获取方式：

- 托盘菜单 →「打开日志目录…」；
- 托盘菜单 →「复制诊断信息」：复制一份不跑 adb 的静态摘要（版本、系统、jar/配置状态）；设备/UHID 详情请用下一项的完整诊断；
- 顶栏右侧菜单（⋯）→「运行诊断…」→ 采集完成后点「导出 zip…」：诊断包内含 `diagnostics.txt`、`meta.json`、`adb-devices.txt`、`dumpsys-input.txt`、`logs/padlend-tail.log`（最近 512 KB）、有崩溃时的 `logs/crash-latest.log`，以及说明文件 `README.txt`。

**诊断包默认脱敏**：设备序列号、IP、MAC、Windows 用户名和用户目录会被替换为占位符（`<serial>` 等）。提交 issue 前仍建议打开 `diagnostics.txt` 扫一眼。

所有错误码以 `E1xx`/`E2xx`/`E3xx`/`E9xx` 开头，见下表与日志中的 `code=` 字段。

## 1. 安装与启动

### Windows SmartScreen 提示“未知发布者” / 无法运行

当前发布未做代码签名（除非发布者用证书构建），Windows 会拦未知发布者的 exe：

1. 点“更多信息”，再点“仍要运行”；
2. 确认下载完整：用 `Get-FileHash .\PadLend-1.0.1-win64.zip -Algorithm SHA256` 与 Release 附带的 `SHA256SUMS.txt` 比对；
3. 若被杀软隔离，核对哈希后加白名单，或按 README 从源码构建；
4. 不要只把 `PadLend.exe` 从 zip 里单独拖出来——它依赖同目录 `_internal\` 下的 Qt/Python 运行时，缺了会启动即报错。

### 双击没反应 / 启动后立即退出

- 先看 `logs\crash-*.log` 与 `logs\padlend.log` 末尾；窗口版没有控制台，错误只进日志。
- 确认整个 `PadLend\` 目录完整（`PadLend.exe`、`_internal\`、`vendor\` 在 `_internal\` 内）。
- 窗口版没有控制台，从 cmd 启动也看不到 stdout；启动失败会写入 `crash-*.log`（主窗尚未初始化时另有错误弹窗）。

### 提示找不到或校验失败 scrcpy-server jar

- 报错形如“服务端 jar 完整性校验失败：… 与 vendor/SHA256SUMS 不一致”；
- 程序在**推送 jar 到平板前**按 `vendor/SHA256SUMS` 校验，不匹配立即停止（E103），不会带病启动；
- 处理：重新解压发布包；源码运行则确认 `vendor/scrcpy-server-v5.0.1.jar` 存在且哈希匹配；
- 若手动替换过 jar，同步更新 `vendor/SHA256SUMS` 中的对应行；缺失条目只会告警并跳过校验。

### 托盘图标不显示

托盘注册失败时程序会退化成普通最小化，避免窗口“消失”。日志关键字：`tray shown: ... native_ok=... err=...`。

- `native_ok=True`：图标可能被 Windows 11 收进任务栏右下角「^」溢出区，拖到任务栏即可常驻；
- `native_ok=False err=5`：`ERROR_ACCESS_DENIED`，进程以低完整性运行，Explorer 拒绝注册托盘图标；
- 检查 `icacls dist\PadLend\PadLend.exe`，若带 `Low Mandatory Level`，把整个程序目录移动（不是复制）到普通目录，或重新解压到 `%LOCALAPPDATA%\Programs\` 等位置；
- 即使没有托盘图标，主窗口仍可通过任务栏恢复，热键与诊断不受影响。

## 2. 连接

### E100 `ADB_NOT_FOUND` — 找不到 adb

- 安装 [Android platform-tools](https://developer.android.com/tools/releases/platform-tools)，把目录加入 PATH；
- 或设置环境变量 `PADLEND_ADB=C:\path\to\adb.exe`；
- 或放到 `vendor\adb\platform-tools\adb.exe`（发布包已内置 37.0.1）；
- 验证：`adb version`。

### E101 `DEVICE_OFFLINE` — 设备未授权/离线

```bash
adb devices -l
adb kill-server && adb start-server
```

- 平板端点“允许 USB 调试”，勾选“始终允许这台计算机”；
- 确认 开发者选项 → 调试 → “USB 调试”已打开；
- 换数据线/换 USB 口（必须是数据线，不是充电线）；
- 无线 adb：确认与 PC 同一网段，见下节。

#### 无线调试已开，但 PadLend 设备列表为空

```bash
adb devices -l
adb mdns services
```

若出现形如 `adb-xxxx (2)._adb-tls-connect._tcp device ...` 的行，说明 Android 的 mDNS 服务重名，ADB 给 serial 加了 ` (2)` 后缀；0.2.4 起已能正确解析。仍为空时按顺序试：

1. 平板“无线调试”关掉再打开，等 3–5 秒后点 PadLend 的“刷新设备”。刷新会重新扫描 adb；adb 没有 mDNS 守护进程时，PadLend 会自己发 mDNS 查询找端口，再用保存的 MAC 和 ARP 表找新 IP；
2. `adb kill-server && adb start-server`（PadLend 与命令行共用同一个 adb server）；
3. `adb mdns check` 若提示 `mdns daemon unavailable` 不影响 PadLend（它会自己做 mDNS 查询），但 Windows 防火墙仍需允许 UDP 5353，否则收不到平板广播；
4. 确认 PC 与平板在同一子网（访客 Wi-Fi / AP 隔离会屏蔽 mDNS）；可改用 `adb connect <平板IP>:<无线调试端口>` 直连；
5. 无线调试每次重启端口会变；PadLend 启动时会自动查找上次那台设备（mDNS 端口 + MAC/ARP），也可以点“刷新设备”重新查找。

### E102 `UHID_UNAVAILABLE` — UHID 不可写

```bash
adb shell id | tr ',' '\n' | grep uhid
adb shell 'exec 3<>/dev/uhid && echo OPEN_OK'
```

- 输出 `OPEN_OK` 才可用；
- 权限不足时尝试 `adb root`（仅 userdebug 设备可用）；量产机通常不可行；
- UHID 是硬条件：不可写时 PadLend **无法创建虚拟鼠标/键盘**，平板不显示系统光标，键鼠输入不能接管；需要换支持 UHID 的机型/ROM，或在 userdebug 设备上 `adb root` 后再试；
- 部分厂商系统（MIUI/HyperOS 等）限制后台 UHID，需在开发者选项里放开“USB 调试（安全设置）”。

### E103 `SERVER_START_FAILED` — scrcpy server 启动失败

```bash
adb shell "CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar app_process / com.genymobile.scrcpy.Server 5.0.1 tunnel_forward=true video=false audio=false control=true cleanup=true raw_stream=true send_dummy_byte=true max_size=4096"
```

- 先排除 jar 缺失/哈希不符（见第 1 节）；
- 端口 1234 被占用时程序会自动尝试 1234–1244；也可手动改 `device.server_port`；
- 看 `adb logcat -s scrcpy`。

### E104 `FORWARD_FAILED` — adb forward 失败

```bash
adb forward --remove tcp:1234
adb forward tcp:1234 localabstract:scrcpy
```

### E105 `HANDSHAKE_FAILED` — 握手失败

- 正常应读到 **1 字节** `00`（scrcpy 5.0.1 `DesktopConnection` 实测）；
- 手动执行 server 报 `Address already in use`，是旧 server 残留占用 `localabstract:scrcpy`（见第 4 节）；
- logcat 出现 `ClassNotFoundException: com.genymobile.scrcpy.Server`，说明 jar 的 `classes.dex` 被压缩，Android 17 的 app_process 加载失败；用 `tools/repack_server_jar.py` 重打包为 stored；
- `cleanup=true` 时 server 启动即由 CleanUp 删除 CLASSPATH 首项 jar，重连前会重新 push，属预期行为；
- 确认 scrcpy-server 版本与 `SCRCPY_VERSION` 一致（5.0.1）。

### E106 `SOCKET_BROKEN` — 控制通道断开

- 程序会自动按 1/2/4/8/16s 重连 5 次，期间状态为 FAULT、输入全部交还系统；
- 检查 USB 线/无线信号；无线 adb 可降低 `motion.report_hz`；
- 连续失败后重启平板 adb：`adb reconnect`。

## 3. 鼠标/指针

### 平板没有出现系统指针

```bash
adb shell dumpsys input | grep -B1 -A3 "Classes: CURSOR"
```

- 应能看到 `PadLend Mouse`；看不到说明 UHID_CREATE 没成功（看 E102）；
- 平板设置里确认“显示触摸/指针位置”未被关闭；
- UHID 不可写时本来就不会有系统指针，这是降级模式，不是故障。

### 指针漂移/位置不准

- 运行 `tools/calibrate.py` 重新测 `XScale` 与实际 px/count；
- 调小 `motion.gain`（每次 0.02）；
- 检查 `pointer_speed` 是否被系统重置：`adb shell settings get system pointer_speed`（接管期间应为 `0`）；
- 托盘菜单 →「重连」会请求重新同步指针（内部 `resync_pointer`），把估计器拉回中心附近。

### 每次进入位置都偏

- 在「布局」页确认平板位置后点「保存布局」；只拖动不保存不会改变实际摆位。
- 检查 `placement` 的边/offset 是否与实际吸附段一致；
- 混合 DPI 屏：确认进程为 Per-Monitor V2（程序已设置），否则几何会按逻辑像素计算。

### 平板旋转后布局没有切换

- 1.0.3 起，布局按“设备身份 + 横屏/竖屏”保存在 `layouts.devices`；只有保存过的
  方向才会自动切换。转到新方向后先调整摆位，再点「保存布局」。
- 日志应出现 `设备朝向变化`；如果旋转平板时没有这条日志，确认 adb 连接正常，
  并检查是否有 `朝向监听中断`。监听只读取 `dumpsys input` 的 Viewport，不会修改设备。
- 同一台设备的新方向没有保存过时，程序会保留当前摆位，避免摆位突然跳回默认值。

### 点击 Windows 输入框后无法再滑入

- 某些焦点/桌面切换会让 Raw Input 在最后一次鼠标事件后静默；0.3.1 起 `win/cursor_watch.py` 会在 Raw Input 超过 250ms 无事件时轮询绝对光标并合成位移，自动恢复边缘接管；
- 日志出现 `cursorwatch.fallback` 即表示兜底已接管；若仍无法进入，附上该段日志与 `tools/probe.py --json` 输出。

### 边缘手势抢鼠标

- 平板系统手势区（左右边缘返回、底部上滑）会先于 UHID 指针响应；
- 把 `motion.boundary_inset_px` 调到 8–16，或把平板吸附到不与手势区冲突的边；
- MagicOS：设置 → 系统导航 → 关闭“边缘返回”手势。

### 回不到 PC / 卡在平板上

- 把指针推到平板**与吸附边相对**的那条边并继续外推；必要时调小 `motion.exit_overflow_px`；
- 热键 `Ctrl+Alt+Q` 急停强制交还；
- 仍卡住时，托盘菜单 →「重连」，或退出并重启程序（启动时会自动清理残留）。

## 4. 幽灵设备 / 进程残留

`adb shell ps | grep scrcpy` 抓不到是正常的——服务进程名是 `app_process`。清理：

```bash
adb shell pkill -9 -f scrcpy-server-manual.jar
adb forward --remove tcp:1234
adb shell settings put system pointer_speed -3
adb shell dumpsys input | grep -c scrcpy    # 期望输出 0
```

启动时若发现上次没有写 `exit.clean`，程序会自动执行上述清理（设计 12.3）。

## 5. ClipCursor / 鼠标被锁在边缘

- 程序正常退出、异常退出、看门狗（5s）都会调用 `ClipCursor(NULL)`；
- 若系统仍被锁：按 `Ctrl+Alt+Q` 急停，或关闭 PadLend；仍不恢复时注销/重启；
- 托盘菜单没有独立的“释放鼠标捕获”项；急停（`Ctrl+Alt+Q`）就是手动释放入口。

## 6. 键盘

### 平板上收不到键盘

- 确认 `keyboard.forward: true` 且当前状态为 CAPTURED（顶栏红灯）；
- `dumpsys input` 中应有 `PadLend Keyboard`；
- 某些输入法只接受 `INJECT_TEXT`；中文可先在 PadLend 里复制，再到平板输入框长按粘贴兜底（PadLend 只同步剪贴板内容，不注入文本输入）。

### PC 上某些键被吞/卡键

- `keyboard.passthrough_keys` 中加入不想转发的**其它**键；Win 键在接管期会被强制吞掉（1.2.0 起，避免开始菜单盖在遮罩下），不受此配置影响；
- 离开接管时程序会发送全零报告清键；若仍卡键，按一次对应键或重新接管再交还；
- UAC/管理员窗口收不到钩子，属 Windows 固有限制。

### 钩子失效（键盘突然不转发）

- 低级钩子可能被系统静默卸载；程序有 30s 看门狗自动重装；
- 看日志是否出现 `hook.failed`；
- 保命键 `Ctrl+Alt+Q` 走 `RegisterHotKey`，不依赖钩子。

## 7. 配置损坏 / 恢复

- YAML 语法错误、顶层不是映射、版本号非法或高于程序支持、字段越界时，程序不会崩溃；
- 原文件被另存为 `config.yaml.bad-<YYYYmmdd-HHMMSS>`（不会覆盖旧备份），随后加载默认配置；
- 日志里会有一条 `cfg.backup ... reason=...`；设置页底部也会显示原因横幅；
- 想恢复旧值：退出 PadLend，把 `.bad-` 文件改回 `config.yaml` 并修正报错字段，再启动。

## 8. 诊断与坐标系

- 托盘 →「复制诊断信息」；顶栏右侧菜单（⋯）→「运行诊断…」→「导出 zip…」；
- 手动命令见设计文档附录 C；
- 坐标系标定：

```bash
adb shell settings put system pointer_location 1
# 发送已知计数后截图读取叠加层 X/Y
adb shell settings put system pointer_location 0
```

## 9. 真机验收

验收项、环境与最近一次结果见 [VERIFICATION.md](VERIFICATION.md)。提交问题时请附：PadLend 版本（关于对话框）、Windows 版本、平板型号/Android 版本、诊断包，以及复现步骤。
