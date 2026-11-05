# PadLend 1.0.0

把 Android 平板变成 Windows 的第二块输入屏：平板显示扩展桌面，鼠标/键盘在屏幕边缘无缝穿到平板，并在平板上使用真实的系统指针。

## 下载

- `PadLend-1.0.0-win64.zip` — Windows x64，解压后运行 `PadLend.exe`，无需安装
- `PadLend-1.0.0-setup.exe` — 可选安装器（CI 成功编译时提供；安装到当前用户目录，保留配置）
- `SHA256SUMS.txt` — 下载后请核对 ZIP 的 SHA-256

## 首次使用

1. 平板打开“开发者选项 → USB 调试”和“无线调试”，与电脑在同一局域网。
2. 解压 ZIP，运行 `PadLend.exe`，按向导完成 adb、设备、UHID 与 scrcpy-server 检查。
3. 在“布局”页把平板拖到目标屏幕边缘，进入边缘后鼠标会穿到平板。

要求：Windows 10/11 x64；平板 `/dev/uhid` 可写。UHID 是硬条件，不可写时无法创建虚拟鼠标/键盘，程序不能接管输入。

## 1.0.0 主要变化

- 版本号收敛到唯一来源，exe 写入文件版本/产品版本资源。
- 发布形态改为 onedir：不再按当前工作目录解包，可放在 Program Files 或只读目录运行。
- 新增崩溃日志、日志轮转、配置损坏自动备份恢复、诊断包脱敏。
- 只停止 PadLend 自己启动的 adb server，不再误杀 IDE/其他工具的 adb 会话。
- 推送 scrcpy-server 前校验哈希；补齐第三方许可与 Qt LGPLv3 说明。
- 修复布局页在平板位于桌面包围盒外时被裁切的问题。
- 修复 `device.auto_connect` 与向导完成页复选框不生效的问题。

完整变更见 [CHANGELOG.md](CHANGELOG.md)。

## 已知限制

- 本版本未做代码签名，Windows SmartScreen 可能提示“未知发布者”；请核对 SHA-256 后运行。
- 界面目前仅中文；实机验证以 Honor MagicPad 3 / Android 17 为主，其他 OEM 的 UHID 权限可能不同。
- 不含自动更新；后续版本请从 Releases 手动下载。
- 控制台输出：窗口程序不附着控制台，`--version` 需重定向（`PadLend.exe --version > version.txt`）或从源码运行。

## 许可

PadLend 以 Apache-2.0 发布。发布包包含 LGPLv3 授权的 Qt/PySide6 动态库，替换方式与源码获取地址见 `THIRD_PARTY_NOTICES.md`；其他第三方许可见 `LICENSES/`。

## 反馈

请在 Issues 中描述平板型号、Android 版本与复现步骤。需要日志时，用“关于 PadLend → 打开日志目录”或“复制诊断信息”；诊断包默认脱敏，提交前仍请检查是否包含可识别信息。
