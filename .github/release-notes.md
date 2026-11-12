# PadLend 1.1.1

跨设备共享键鼠无缝操作平板：鼠标从屏幕边缘穿到平板上，平板显示真实的系统指针，键盘和剪贴板双向同步。

**1.1.1 是修复版本**：解决接管期间电脑屏幕上仍会出现鼠标指针的问题（电脑和平板各一个光标）。

## 修复：接管期电脑光标重新显示

根因在遮罩窗口的顶层状态。Windows 会静默忽略对某些窗口的置顶请求——实测安装版进程里 `SetWindowPos(HWND_TOPMOST)` 返回成功，但扩展样式里 `WS_EX_TOPMOST` 始终没置上；遮罩被任务栏压在下面，`WM_SETCURSOR` 收不到，系统光标就重新显示出来。

- 遮罩扩展样式直接包含 `WS_EX_TOPMOST`，创建即置顶；
- 创建后回读扩展样式校验，失败重试一次并记录 `ERROR`；
- 每次接管复用窗口时重新置顶；接管期间看门狗每秒复查，丢失顶层或光标重新可见立即修复（`mask.repair` 日志）；
- `cap.start` 以 INFO 输出遮罩诊断（`visible` / `topmost` / `clip` / `windows`），现场排查看日志即可。

## 文案

标题栏与「关于」改为「跨设备共享键鼠无缝操作平板」：平板只共享键鼠，不占用显示器，也不是第二块屏幕。

## 下载

- `PadLend-1.1.1-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.1.1-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.1.1-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb

## 校验

`ruff check .`、`pytest`、`vitest`、`tsc --noEmit` 与 `npm run build` 全部通过；发布产物由 GitHub Actions 在 `windows-latest` 上构建（含 Rust/MSVC 与 WebView2）。

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
