# PadLend 1.1.2

跨设备共享键鼠无缝操作平板：鼠标从屏幕边缘穿到平板上，平板显示真实的系统指针，键盘和剪贴板双向同步。

**1.1.2 是修复版本**：解决接管交还后遮罩窗口残留——整块屏幕没有光标、所有点击被吞、只能退出应用的问题。1.1.1 的「接管期间电脑上重新出现鼠标指针」修复继续有效。

## 修复：交还后遮罩泄漏，屏幕被盖死

1.1.1 的遮罩窗口只会在创建它的线程上处理 `WM_CLOSE`。点界面按钮进入接管时，那条线程是 RPC 线程池线程，**没有消息泵**；鼠标划到平板边缘自动交还发生在 raw input 线程。于是 `PostMessage(WM_CLOSE)` 永远没人处理，`ClipCursor` 已经复位，遮罩却一直盖在屏幕上：看不到光标，点哪里都没反应。

- 遮罩窗口改由专用线程 `padlend-mask-win` 拥有：创建、`WM_SETCURSOR`、销毁都在这条线程上；
- 释放时把 `DestroyWindow` 直接投递到该线程执行，不再依赖跨线程 `WM_CLOSE`（实测 9 ms 内销毁）；
- 投递超时（线程卡死）时，退路是先 `ShowWindow(SW_HIDE)` 把屏幕立刻让出来，再补发 `WM_CLOSE` 收尾；
- `ClipCursor` 释放路径保持不变，退出时仍然无条件复位。

## 验证

- 真机复现原故障：在**没有消息泵**的线程创建遮罩、从另一线程释放托管。修复后遮罩在专用线程创建，释放 9 ms 完成，`EnumWindows` 剩余遮罩 0 个、光标恢复可见；修复前遮罩持续残留，`WindowFromPoint` 命中遮罩。
- `pytest` 全量通过（新增跨线程释放保留在遮罩线程、消息泵卡死时隐藏兜底两组单测），`ruff check .` 通过，前端 `vitest` / `tsc --noEmit` / `npm run build` 通过。

## 下载

- `PadLend-1.1.2-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.1.2-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.1.2-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb

## 校验

`ruff check .`、`pytest`、`vitest`、`tsc --noEmit` 与 `npm run build` 全部通过；发布产物由 GitHub Actions 在 `windows-latest` 上构建（含 Rust/MSVC 与 WebView2）。

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。