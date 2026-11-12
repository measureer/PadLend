# PadLend 1.2.0

跨设备共享键鼠无缝操作平板：鼠标从屏幕边缘穿到平板上，平板显示真实的系统指针，键盘和剪贴板双向同步。

**1.2.0 是可靠性与退出流程修复版本**：解决托盘点「退出」不退出、接管期按 Win 键把开始菜单盖在遮罩下（看起来像死机），以及崩溃/强杀后遗留孤儿遮罩的问题。1.1.2 的交还后遮罩线程修复继续有效。

## 修复：托盘「退出」不再只隐藏窗口

1.1.2 的退出复用了窗口关闭路径。开启 `close_to_tray` 时，Tauri 的 `CloseRequested` 会 `prevent_close()` + `hide()`，于是点「退出」只是把窗口藏起来，进程、sidecar、托盘图标和热键都还在。

- 新增显式 `quit_app` 命令：先同步停 sidecar，再 `app.exit(0)`；
- 托盘菜单和托盘 quit 事件都走这条路径；
- 关窗（X）仍按 `close_to_tray` 的语义：默认 `false` 时停 sidecar 退出，`true` 时隐藏到托盘；
- Rust 等待 sidecar 的上限从 4s 提到 8s（遮罩线程最坏等 5s），超时才强杀并按进程树清理。

## 修复：接管期 Win 键不再打开开始菜单

`passthrough_keys` 默认把左右 Win 放给 Windows，开始菜单弹出后被 topmost 遮罩挡住；鼠标点不进去，其余按键又被吞掉，观感就是「整屏卡死」。

- CAPTURED 期间 Router 无条件吞掉 L/R Win 的 down/up，不转发平板、不进 keymap；
- `_swallowed_win` 锁存到 keyup，交还瞬间也不会漏掉松键；
- 兼容 `vk == 0` 的 scan code 路径；
- IDLE 状态下 Win 键行为不变。

## 修复：崩溃/强杀后不再留下孤儿遮罩

遮罩窗口属于 Python sidecar，真正退出的进程不会留下窗口；残留只可能来自「sidecar 还活着但卡住」或「建了窗口却没被跟踪」。

- 建窗拆成「拿 hwnd → 先登记 → 再配置 alpha/置顶」，配置阶段卡住也能被释放路径找到；
- 遮罩线程任务超时直接报错，不再回调用线程重复建窗；
- 启动时 `cleanup_orphan_mask()` 按窗口类名找到残留，先隐藏/关闭，必要时终止仍挂起的 sidecar，并复位 `ClipCursor`；
- Rust 给 sidecar 注入 `PADLEND_PARENT_PID`，Python 每秒检查父进程，Tauri 崩溃后 sidecar 会同步 teardown 再退出；
- `service.shutdown()` / `Runtime.stop()` 改为同步、幂等，重复或并发调用安全；首次运行误报「上次未正常退出」也一并修复。

## 关于「按 Win 键崩溃」

本轮**没有拿到真实崩溃栈**：日志目录、WER 和事件查看器都没有 PadLend 的 `crash-*`/`faulthandler-*` 记录，复现时桌面已锁屏无法回放。因此上面的 Win 键修复针对的是已确认的输入路径与假死观感，失效安全则保证「即使再崩，屏幕也不会被遮罩吃掉」。若现场再次出现，请按 `docs/PITFALLS.md` 4.1 收集完整日志与 WER 报告。

## 验证

- `pytest` 421 passed / 3 skipped（收集 424）（新增 Win 键锁存、遮罩先登记/配置超时、孤儿遮罩清理、adb 故障映射、UHID 半包、teardown 幂等/父进程看门狗/首启假阳性等用例）；
- `vitest` 29 tests 通过（新增托盘退出走 `quit_app` 而非 `windowClose` 的用例）；
- `ruff check .`、`tsc --noEmit` 通过；Rust 由 GitHub Actions 在 `windows-latest` 上编译。

## 下载

- `PadLend-1.2.0-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.2.0-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.2.0-win64.zip -Algorithm SHA256
```

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
