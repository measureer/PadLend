# 故障排查

日志：`%APPDATA%\PadLend\logs\padlend.log`。所有错误码以 `E1xx`/`E2xx`/`E3xx`/`E9xx` 开头，见下表。

## 0. 实测事实（开发机，2026-11-02）

设备：Honor MagicPad 3 `CGA-W00`，Android 17（SDK 37），3200×2136 横屏 / 2136×3200 竖屏，420 dpi。

| 事实 | 值 | 结论 |
|---|---|---|
| `pointer_speed` 出厂值 | `-3` | 程序启动时归零，退出还原 |
| 鼠标 `XScale` / `YScale` | `1.312` / `1.312` | 1 HID count ≈ 1.312 平板像素，估计器系数来源 |
| 相对位移线性度（H1） | 无系统加速残量 | 可 1:1 映射 |
| px/count（H2） | ≈ `XScale` | 估计器 `k = (W_tablet/W_monitor)/XScale·gain` |
| 边缘手势（H3） | 贴边外推仍能收到 Raw Input | 边缘接管可行；必要时 `boundary_inset_px` 内缩 |

> 重新采信这些值：`tools/probe.py` 会现场读取并打印事实表；H1/H2 的漂移标定步骤见 `tools/calibrate.py`。

## 1. 启动/连接

### E100 `ADB_NOT_FOUND` — 找不到 adb

- 安装 [Android platform-tools](https://developer.android.com/tools/releases/platform-tools)，把目录加入 PATH；
- 或设置环境变量 `PADLEND_ADB=C:\path\to\adb.exe`；
- 或放到 `vendor\adb\platform-tools\adb.exe`；
- 验证：`adb version`。

### E101 `DEVICE_OFFLINE` — 设备未授权/离线

```bash
adb devices -l
adb kill-server && adb start-server
```

- 平板端勾选"始终允许这台计算机"；
- 换数据线/换 USB 口（必须是数据线，不是充电线）；
- 无线 adb：`adb connect 192.168.x.x:5555`，确认同一网段。

### E102 `UHID_UNAVAILABLE` — UHID 不可写

```bash
adb shell id | tr ',' '\n' | grep uhid
adb shell 'exec 3<>/dev/uhid && echo OPEN_OK'
```

- 输出 `OPEN_OK` 才可用；若权限不足，尝试 `adb root`（仅 userdebug 设备可用），或在支持的系统上使用 `INJECT_TOUCH_EVENT` 降级模式（指针无系统光标，功能受限）；
- 部分厂商系统（MIUI/HyperOS 等）会限制后台 UHID，需在开发者选项里放开"USB 调试（安全设置）"。

### E103 `SERVER_START_FAILED` — scrcpy server 启动失败

```bash
adb shell "CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar app_process / com.genymobile.scrcpy.Server 5.0.1 tunnel_forward=true video=false audio=false control=true cleanup=true raw_stream=true send_dummy_byte=true max_size=4096"
```

- 确认 jar 已推送且 md5 一致；
- 端口 1234 被占用时程序会自动尝试 1234–1244；也可手动改 `device.server_port`；
- 看 `adb logcat -s scrcpy`。

### E104 `FORWARD_FAILED` — adb forward 失败

```bash
adb forward --remove tcp:1234
adb forward tcp:1234 localabstract:scrcpy
```

### E105 `HANDSHAKE_FAILED` — 握手失败

- 正常应读到 **1 字节** `00`（scrcpy 5.0.1 `DesktopConnection` 实测；早期设计误记为 4 字节）；
- 若手动执行 server 出现 `Address already in use`，是旧 server 残留占用 `localabstract:scrcpy`（见第 3 节）；
- 若 logcat 出现 `ClassNotFoundException: com.genymobile.scrcpy.Server`，说明 jar 的 `classes.dex` 被压缩，Android 17 的 app_process 加载失败；用 `tools/repack_server_jar.py` 重打包为 stored；
- `cleanup=true` 时 server 启动即由 CleanUp 删除 CLASSPATH 首项 jar，重连前会重新 push，属预期行为；
- 确认 scrcpy-server 版本与 `SCRCPY_VERSION` 一致（5.0.1）。

### E106 `SOCKET_BROKEN` — 控制通道断开

- 程序会自动按 1/2/4/8/16s 重连 5 次，期间状态为 FAULT、输入全部交还系统；
- 检查 USB 线/无线信号；无线 adb 可降低 `motion.report_hz`；
- 连续失败后重启平板 adb：`adb reconnect`。

## 2. 鼠标/指针

### 平板没有出现系统指针

```bash
adb shell dumpsys input | grep -B1 -A3 "Classes: CURSOR"
```

- 应能看到 `PadLend Mouse`；看不到说明 UHID_CREATE 没成功（看 E102）；
- 平板设置里确认"显示触摸/指针位置"未被关闭。

### 指针漂移/位置不准

- 运行 `tools/calibrate.py` 重新测 `XScale` 与实际 px/count；
- 调小 `motion.gain`（每次 0.02）；
- 检查 `pointer_speed` 是否被系统重置：`adb shell settings get system pointer_speed`（接管期间应为 `0`）；
- 热键 `resync`（默认在托盘菜单"重同步指针"）把估计器拉回中心。

### 每次进入位置都偏

- 检查 `placement` 的边/offset 是否与实际吸附段一致；
- 混合 DPI 屏：确认进程为 Per-Monitor V2（程序已设置），否则几何会按逻辑像素计算。

### 边缘手势抢鼠标

- 平板系统手势区（左右边缘返回、底部上滑）会先于 UHID 指针响应；
- 把 `motion.boundary_inset_px` 调到 2–4，或把平板吸附到不与手势区冲突的边；
- MagicOS：设置 → 系统导航 → 关闭"边缘返回"手势。

### 回不到 PC / 卡在平板上

- 把指针推到平板**与吸附边相对**的那条边并继续外推；
- 热键 `Ctrl+Alt+Q` 急停强制交还；
- 若仍卡住，托盘 → 重同步指针，或重启程序（启动时会自动清理残留）。

## 3. 幽灵设备 / 进程残留

`adb shell ps | grep scrcpy` 抓不到是正常的——服务进程名是 `app_process`。清理：

```bash
adb shell pkill -9 -f scrcpy-server-manual.jar
adb forward --remove tcp:1234
adb shell settings put system pointer_speed -3
adb shell dumpsys input | grep -c scrcpy    # 期望输出 0
```

启动时若发现上次没有写 `exit.clean`，程序会自动执行上面的清理（设计 12.3）。

## 4. ClipCursor / 鼠标被锁在边缘

- 程序正常退出、异常退出、看门狗（5s）都会调用 `ClipCursor(NULL)`；
- 若系统仍被锁：运行 `Ctrl+Alt+Q`，或注销/重启；
- 托盘 → 诊断 → "释放鼠标捕获"可手动释放。

## 5. 键盘

### 平板上收不到键盘

- 确认 `keyboard.forward: true` 且当前状态为 CAPTURED（顶栏红灯）；
- `dumpsys input` 中应有 `PadLend Keyboard`；
- 某些输入法只接受 `INJECT_TEXT`，中文可先用剪贴板同步（`Ctrl+Alt+V`）兜底。

### PC 上某些键被吞/卡键

- `keyboard.passthrough_keys` 中加入不想转发的键（默认 Win 键）；
- 离开接管时程序会发送全零报告清键；若仍卡键，按一次对应键或重新接管再交还；
- UAC/管理员窗口收不到钩子，属 Windows 固有限制。

### 钩子失效（键盘突然不转发）

- 低级钩子可能被系统静默卸载；程序有 30s 看门狗自动重装；
- 看日志是否出现 `hook.failed`；
- 保命键 `Ctrl+Alt+Q` 走 `RegisterHotKey`，不依赖钩子。

## 6. 诊断

- 托盘 → 诊断 → "导出诊断包"，生成脱敏 zip（日志、配置、adb 输出、显示器几何）；
- 手动命令见设计文档附录 C；
- 坐标系标定：

```bash
adb shell settings put system pointer_location 1
# 发送已知计数后截图读取叠加层 X/Y
adb shell settings put system pointer_location 0
```

## 7. 真机验收状态

| 项 | 状态 |
|---|---|
| A1 单屏右侧来回 | 待复验（开发机无平板连接） |
| A2–A3 精度/回程 | 待复验 |
| A4–A6 多显示器/混合 DPI/热插拔 | 逻辑由 fixture 单测覆盖，真机待双屏环境 |
| A7 键盘组合键 | 待复验 |
| A8 中文输入 | 待复验 |
| A9 边缘手势 | 待复验 |
| A10 断连重连 | 待复验 |
| A11 强杀清理 | 待复验 |
| A12 性能/延迟 | 待复验 |
| A13 长时间稳定性 | 待复验 |
