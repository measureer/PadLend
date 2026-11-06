# PadLend 1.0.1

把 Android 平板变成 Windows 的第二块输入屏：平板显示扩展桌面，鼠标/键盘在屏幕边缘无缝穿到平板，并在平板上使用真实的系统指针。

## 下载

- `PadLend-1.0.1-win64.zip` — Windows x64，解压后运行 `PadLend.exe`，无需安装
- `PadLend-1.0.1-setup.exe` — 可选安装器（CI 成功编译时提供；安装到当前用户目录，保留配置）
- `SHA256SUMS.txt` — 下载后请核对 ZIP 的 SHA-256

## 首次使用

1. 平板打开“开发者选项 → USB 调试”和“无线调试”，与电脑在同一局域网。
2. 解压 ZIP，运行 `PadLend.exe`，按向导完成 adb、设备、UHID 与 scrcpy-server 检查。
3. 在“布局”页把平板拖到目标屏幕边缘，进入边缘后鼠标会穿到平板。

要求：Windows 10/11 x64；平板 `/dev/uhid` 可写。UHID 是硬条件，不可写时无法创建虚拟鼠标/键盘，程序不能接管输入。

## 1.0.1 主要变化

- 每台无线设备分别保存 Wi-Fi MAC（`device.macs`），不再只记最近一台；旧的 `device.mac` 会自动迁移。
- 设备页的“刷新设备”现在会先重新扫描 adb，再用保存的 MAC 在局域网里逐台查找。
- 平板换了 IP 或无线调试端口时，列表里的旧地址会被新地址替换，MAC 一起带过去。
- 修正定位器没有使用 ARP 表的问题：设备换 IP 后可以通过 ARP 表匹配到新地址，不再只靠 mDNS。
- 后台读取 MAC 期间设备被删除时，不再把 MAC 写回配置。

1.0.0 的发布形态与修复（onedir、崩溃日志、日志轮转、配置恢复、诊断脱敏、adb 所有权、第三方许可等）保持不变，完整记录见 [CHANGELOG](https://github.com/measureer/PadLend/blob/master/docs/CHANGELOG.md)。

## 已知限制

- 本版本未做代码签名，Windows SmartScreen 可能提示“未知发布者”；请核对 SHA-256 后运行。
- 界面目前仅中文；实机验证以 Honor MagicPad 3 / Android 17 为主，其他 OEM 的 UHID 权限可能不同。
- 不含自动更新；后续版本请从 Releases 手动下载。
- 控制台输出：窗口程序不附着控制台，`--version` 需重定向（`PadLend.exe --version > version.txt`）或从源码运行。

## 许可

PadLend 以 Apache-2.0 发布。发布包包含 LGPLv3 授权的 Qt/PySide6 动态库，替换方式与源码获取地址见 `THIRD_PARTY_NOTICES.md`；其他第三方许可见 `LICENSES/`。

## 反馈

请在 Issues 中描述平板型号、Android 版本与复现步骤。需要日志时，用“关于 PadLend → 打开日志目录”或“复制诊断信息”；诊断包默认脱敏，提交前仍请检查是否包含可识别信息。
