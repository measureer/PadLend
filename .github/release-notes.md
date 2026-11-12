# PadLend 1.1.0

把 Android 平板变成 Windows 的第二块输入屏：鼠标从屏幕边缘穿到平板上，平板显示真实的系统指针，键盘和剪贴板双向同步。

**1.1.0 是一次前端架构升级**：界面从 PySide6/Qt 迁移到 **Tauri 2 + React 19 + TypeScript**，业务引擎（Router 状态机、ADB/UHID、几何、配置）保持原样，由独立的 Python 业务服务进程承载。功能与交互不变，观感、流畅度和可维护性显著提升。

## 下载

- `PadLend-1.1.0-setup.exe` — NSIS 安装器，安装到当前用户目录，不需要管理员权限
- `PadLend-1.1.0-win64.zip` — 免安装包，解压后运行 `PadLend.exe`
- `SHA256SUMS.txt` — 下载后建议核对

```powershell
Get-FileHash .\PadLend-1.1.0-win64.zip -Algorithm SHA256
```

## 主要变化

### 视觉

- 建立统一设计系统：配色、字体、间距、圆角、控件高度与组件样式全部令牌化，日间/夜间主题一致。
- 统一页面布局：自定义标题栏 + 顶栏状态 + 侧栏导航 + 内容卡片 + 底栏；所有页面与对话框共用组件库。
- 响应式：1180 / 900 / 700px 断点，侧栏收起、卡片单列、窄窗口切换底部导航；布局画布按虚拟桌面自动缩放。

### 性能

- 耗时任务（ADB、连接等待、诊断采集、mDNS/ARP 探测）全部移到服务进程线程池，界面线程不再阻塞。
- 取消 1 秒 GUI 轮询，改为事件驱动推送；指标键名与后端快照对齐。
- 日志过滤防抖 + 虚拟列表（最多渲染可视区约 30 行）；设备列表 keyed diff。
- 画布拖动/平移/缩放只改 DOM transform 并用 rAF 合帧，松手才提交一次状态。
- 设置草稿本地化，保存时一次提交；主题切换只改根 CSS 变量；路由与对话框按需分包。

### 架构

- 界面层、编排层、传输层、业务层单向依赖；前端不再持有任何 Python 业务对象。
- 声明式设置 schema 一份定义驱动渲染/取值/写回/校验；诊断纯逻辑从 Qt 层上移到 `core/diagnostics.py`。
- 旧 `ui/`（PySide6）代码、Qt 主题、旧打包脚本与 Inno Setup 配置已删除，发行物不再包含 Qt DLL。

## 功能保留

多显示器与负坐标、边缘 1:1 穿越、UHID 虚拟鼠标/键盘、键盘转发与中文输入法、双向剪贴板、按设备横竖屏分别保存的布局、托盘菜单、全局热键（`Ctrl+Alt+S` / `Ctrl+Alt+Q`）、日志与脱敏诊断包、首次设置向导、关闭窗口收进托盘。

## 环境要求

- Windows 10/11 x64；系统需具备 WebView2 Runtime（缺失时安装器会引导安装）
- Android 8.0+ 且 `/dev/uhid` 可写；USB 数据线或无线 adb

## 校验

`ruff check .`、`pytest`（含服务协议端到端测试）、`vitest`（28 项）、`tsc --noEmit` 与 `npm run build` 全部通过；发布产物由 GitHub Actions 在 `windows-latest` 上构建（含 Rust/MSVC 与 WebView2）。

> 当前构建没有代码签名，Windows 可能提示「未知发布者」；点「更多信息 → 仍要运行」即可，或用 SHA256SUMS.txt 核对。
