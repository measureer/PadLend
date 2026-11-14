# PadLend 1.7.5

这一版修的是托盘菜单里两处「点了没反应」。

## 修了什么

### 1. 「打开配置…」和「打开日志目录…」完全无效

点下去什么都不打开，右下角弹一句「无法打开：undefined」。

原因是前端调的是 Tauri opener 插件的前端命令，那条命令会先按能力清单里的路径白名单过滤；而我们的清单只声明了「允许调用」这一档（Tauri 的描述是 without any pre-configured scope），白名单是空的——空就等于什么都不许，任何路径都被拒。报错之所以显示 undefined，是因为 Rust 侧返回的是字符串，前端按 Error 取 message 取了个空。

现在改成外壳自己的 open_path 命令，直接调插件的 Rust 接口，不再受这层白名单限制；错误也统一包成 Error，真出错时会显示具体原因。
还有一层：Windows 默认不给 .yaml 登记文件关联，而且系统「用默认程序打开」在这种情况不报错、也不启动任何东西（静默成功）。所以现在配置、日志、许可这类纯文本直接交给记事本打开，不再指望文件关联。

### 2. 窗口收在托盘时，「关于」「运行诊断…」「首次设置向导…」点了像没反应

这三项打开的都是应用内对话框。窗口已经收进托盘时，对话框确实渲染了，只是没人看得见。现在点击时会先把主窗口唤回前台再开对话框。

## 顺手修的

- 「关于」里的说明还停留在 1.7.0 之前：写着业务引擎由 Python 服务进程承载、第三方是 PyYAML 和 psutil。实际早就换成 Rust 引擎 padlend-core 了，现在按 THIRD_PARTY_NOTICES.md 的实际清单重写。
- 诊断对话框状态栏会把脱敏后的 `<serial>` 当成设备名显示，现在优先显示真实序列号，读不到就说「序列号已脱敏」。

## 升级提示

- 覆盖安装或解压覆盖即可；config.yaml、devices.json 格式没有变化。
- 未签名发布，SmartScreen 仍可能提示「未知发布者」。

## 验证

- 前端 vitest 45 passed / 9 files，tsc --noEmit 通过；外壳 cargo check 通过。
- 真机逐项点过：托盘「打开配置…」用记事本打开 config.yaml，「打开日志目录…」打开资源管理器；把窗口收进托盘后点「关于」，窗口自动弹出并显示对话框。

## 下载

- PadLend-1.7.5-setup.exe — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- PadLend-1.7.5-win64.zip — 免安装包，解压后运行 padlend-desktop.exe
- SHA256SUMS.txt — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.7.5-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 /dev/uhid 可写；USB 数据线或无线 adb
- 音频转发需要 Android 11+

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
