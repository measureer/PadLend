# PadLend 项目设计文档

| 项目 | 内容 |
|---|---|
| **名称** | PadLend（中文：借屏） |
| **版本** | v1.0（可实施版） |
| **日期** | 2026-11-02 |
| **状态** | ✅ 已定稿，可直接作为实施依据 |
| **目标读者** | 实施者（人或 AI agent）/ 评审者 |
| **运行平台** | Windows 10 1809+ / Windows 11（x64）；Android 12+ 平板 |
| **技术栈** | Python 3.11+ / PySide6 / ctypes / adb / scrcpy-server |
| **许可** | 待定（建议 Apache-2.0） |

> **一句话**：把平板变成电脑屏幕"边上的一块屏"——鼠标从电脑屏幕边缘自动滑进平板、从平板边缘自动滑回，全程不用按任何键，且平板上有**真正的系统鼠标指针**和**可用的中文键盘输入**。

---

## 0. 本文档的使用方式

### 0.1 给实施者

1. **先读第 3 章**——那里是"必须相信的实测数字"，全篇设计的依据都在那张表里，不要凭直觉改动。
2. **按第 14 章的顺序做**——每个阶段有独立可验收的 DoD 和可直接执行的命令。
3. **接口以第 8、9 章为准**——实现时不要自行改变签名、报文格式、错误码。
4. **第 5 章记录了"为什么不是别的方案"**——如果要偏离设计，先看那章是否已经否决过。

### 0.2 约定

| 词 | 含义 |
|---|---|
| **MUST / 必须** | 硬性要求，违反即缺陷 |
| **SHOULD / 应该** | 推荐做法，偏离需说明理由 |
| **MAY / 可以** | 可选 |
| `→` | 调用/流向 |
| `⇒` | 推导结论 |

### 0.3 文档地图

| 我想… | 去看 |
|---|---|
| 知道这软件到底要干什么 | 第 1、2 章 |
| 知道哪些数字不能改 | 第 3 章 |
| 理解整体怎么搭 | 第 4 章 |
| 知道为什么不用虚拟显示器/投屏 | 第 5 章 |
| 搞懂坐标、边缘、指针位置怎么算 | 6.3、6.4 |
| 搞懂状态机怎么迁移 | 6.5 |
| 搞懂键盘怎么转发 | 6.7 |
| 写 GUI | 6.10 |
| 定义函数/类型 | 第 8 章 |
| 拼报文 | 第 9 章 |
| 处理线程/卡顿 | 第 10 章 |
| 出错怎么办 | 第 12 章 |
| 支持多显示器 | 第 13 章 |
| **开始动手** | **第 14 章** |
| 怎么验收 | 第 15 章 |

---

# 1. 项目概述

## 1.1 它解决什么问题

用户有笔记本（Windows）和平板（Android）。平板物理上就摆在笔记本旁边。用户希望：

- 鼠标推到笔记本屏幕右边缘，**自动**穿到平板上，平板出现指针
- 鼠标在平板里推到边缘，**自动**穿回笔记本
- 键盘也跟着走，能在平板上用中文输入法打字
- 平板摆哪边都行，多显示器也支持

## 1.2 上一版为什么失败

上一版（已被本方案取代）用"全局钩子抓输入 → 自定义 1px ClipCursor → 状态机 → APK 注入"实现，卡在三点：

| 编号 | 现象 | 根因 |
|---|---|---|
| **P1** | 平板上看不见鼠标指哪 | 走 Android **绝对注入**（`INJECT_TOUCH_EVENT` / `--mouse=sdk`），系统不为这类事件绘制指针图标。实测：悬停时 `pointer_location` 叠加层读到 `X:0.0 Y:0.0` |
| **P2** | 必须按 `LeftAlt` 才能回电脑 | 相对位移设备不知道自己指针在哪，只能靠"模式开关"，无法靠"到达边缘"判定 |
| **P3** | 按键 40–120ms、无组合键、打不了中文 | 走 `input keyevent`（shell 注入），不是真实输入设备 |

**结论**：把输入通道换成 **HID**（UHID 虚拟鼠标/键盘）一次性解决 P1/P3；在 PC 端**自己维护平板指针位置估计器**解决 P2。

## 1.3 目标（MUST）

| ID | 目标 |
|---|---|
| G1 | 平板出现**系统级真实鼠标指针**（`dumpsys input` 中 `Classes: CURSOR`），由系统绘制与处理 |
| G2 | 鼠标在电脑屏幕**外轮廓边缘**外推 → 自动接管，无需按键 |
| G3 | 鼠标在平板屏幕边缘外推 → **自动交还**，无需按键 |
| G4 | 平板相对电脑屏幕位置由用户在 GUI 中**自由摆放** |
| G5 | **多显示器完整适配**：多屏、混合 DPI、负坐标、热插拔 |
| G6 | **键盘全量转发**：组合键、中文输入法、F1–F12、可选屏蔽本机 |
| G7 | 常驻托盘 + 图形设置界面 |
| G8 | **不装驱动、不要管理员权限、不 root** |

## 1.4 目标（SHOULD / MAY）

| ID | 目标 | 优先级 |
|---|---|---|
| G9 | 剪贴板双向同步（文本） | SHOULD |
| G10 | 可选"看得见的画面"（外挂 scrcpy 视频窗），与指针逻辑解耦 | MAY |
| G11 | 平板横/竖屏自适应 | SHOULD |
| G12 | 热键临时切换（不依赖边缘） | SHOULD |

## 1.5 非目标（明确不做）

- ❌ 绝对坐标控制（无指针，与 G1 冲突）
- ❌ 虚拟显示器 / IddCx 驱动（不需要且要提权）
- ❌ root 平板
- ❌ 文件传输 / 通知同步 / 多平板同时接管（一期）
- ❌ macOS / Linux 客户端（架构预留，不实现）
- ❌ 高安全桌面（UAC 提权窗口）内的键鼠转发（Windows UIPI 固有限制）

## 1.6 三条设计铁律

1. **指针是唯一真相，画面是可选项**。视频层只能旁挂，任何逻辑 MUST NOT 依赖窗口尺寸、黑边、帧率。
2. **状态只有一个写者**。`Router` 独占状态迁移；其他模块只投递事件。
3. **钩子回调里禁止 I/O**。`WH_KEYBOARD_LL` 与 `WM_INPUT` 回调只入队；超过 `LowLevelHooksTimeout`（默认 300ms）会被系统**静默卸载钩子**。

---

# 2. 需求与验收

## 2.1 用户故事（测试用例引用这些编号）

### US-1 首次连接
> 我插着平板打开 PadLend，它自己找到设备连上，托盘变绿。

- **AC1.1** 自动枚举 `adb devices`；有 USB 优先 USB，否则无线
- **AC1.2** 失败时给可操作提示（未开 USB 调试 / 未授权 / 被其它 scrcpy 占用 / 端口冲突）
- **AC1.3** 连接后显示：型号、分辨率、朝向、`XScale`、`pointer_speed`

### US-2 摆放平板
> 平板摆在笔记本右边，我在界面里把它拖到屏幕右侧。

- **AC2.1** 画布按真实比例绘制所有显示器
- **AC2.2** 只有**外轮廓边段**可吸附（红色虚线）；内缝为灰色点线且不可吸附
- **AC2.3** 角落可裁决优先边
- **AC2.4** 松手立即生效并持久化

### US-3 穿过去
- **AC3.1** 边缘带宽 1px，需连续外推 ≥ `arm_px`(40) 才触发
- **AC3.2** 触发瞬间 PC 光标隐藏，平板指针出现在**入口边**、与出口位置成比例处
- **AC3.3** 触发到平板指针可动，无线 < 30ms / USB < 15ms
- **AC3.4** 贴边后反向推 1px 立即响应（无虚位）

### US-4 穿回来
- **AC4.1** 需连续外推 ≥ `exit_overflow_px`(24)
- **AC4.2** 回 PC 后光标出现在对应边、比例位置，内缩 2px
- **AC4.3** 连续来回 20 次，估计器与实际指针偏差 ≤ 3px

### US-5 键盘
- **AC5.1** 平板识别为**物理键盘**，中文输入法候选框正常
- **AC5.2** 组合键正确，不粘键
- **AC5.3** 可配置 PC 端屏蔽，默认开
- **AC5.4** 交还时自动清空按键状态

### US-6 多显示器
- **AC6.1** 150%/100% 混合 DPI 下速度一致
- **AC6.2** 副屏在左侧（负坐标）正确
- **AC6.3** 拔屏后自动重锚主屏同边并提示
- **AC6.4** 平板可跨两块屏同一侧（`span` 合并）

## 2.2 真机验收矩阵（第 15.3 节给出执行方法）

| # | 场景 | 通过标准 |
|---|---|---|
| A1 | 单屏 + 平板右侧，来回 20 次 | 偏差 ≤3px，无卡顿 |
| A2 | 横屏→竖屏切换后继续用 | 位置重置中心，继续可用 |
| A3 | 上/下/左侧摆放 | 出入口点正确 |
| A4 | 双屏 150%+100%，挂外接屏右侧 | 速度一致 |
| A5 | 副屏在左（负坐标） | 正确 |
| A6 | 拔掉外接屏 | 自动重锚 + 提示 |
| A7 | 快速甩动（>3000px/s） | 不丢指针、甩后贴边验证不脱同步 |
| A8 | 全键位 + 中文输入法 | 无错字、无粘键 |
| A9 | 接管期 Alt+Tab / Win 组合 | 按配置正确路由 |
| A10 | 拔 USB 切无线 / 直接断开 | 状态正确、无残留 |
| A11 | 强杀进程 | 光标锁释放、平板无幽灵设备 |
| A12 | 与 Sunshine/Moonlight 并存 | 互不干扰 |
| A13 | 连续 2 小时 | 无漂移、无内存增长 |

## 2.3 兼容性矩阵

| 维度 | 支持 | 说明 |
|---|---|---|
| Windows | 10 1809+ / 11 | Win10 无 DWM 圆角，其余一致 |
| 显示器数 | 1–6 | 上限可调 |
| DPI | 100%–300% 混合 | 引擎用物理像素，天然一致 |
| Android | 12+ | 需 `/dev/uhid` 可写（见 3.3） |
| 连接 | USB / 无线 adb | USB 优先 |
| 冲突 | 与其它 scrcpy/InputShare 互斥 | 同一端口/同一 UHID 设备 ID 冲突时提示 |

---

# 3. 技术基线与实测事实

> ⚠️ **本章所有数字都是在真实设备上实测得到的**。实现时**直接使用**，不要重新猜测；若与你的环境不符，先跑附录 C 的诊断命令核实，再决定是否调整。

## 3.1 目标平板基线

| 项 | 值 | 来源 |
|---|---|---|
| 型号 | Honor MagicPad 3 / **CGA-W00** | `getprop ro.product.model` |
| 系统 | Android 17 / SDK 37 | `getprop ro.build.version.*` |
| 分辨率（横屏） | **3200 × 2136** | `wm size` |
| 物理分辨率 | 2136 × 3200（竖屏） | `wm size` physical |
| 密度 | **420 dpi** | `wm density` |
| 刷新率 | 最高 165Hz | `dumpsys display` |
| 宽高比 | 3200/2136 = **1.4981**（3:2） | 计算 |
| `/dev/uhid` 权限 | `crw-rw---- uhid uhid u:object_r:uhid_device:s0` | `ls -lZ` |
| adb shell 身份 | `uid=2000(shell)`，**组含 3011(uhid)** | `id` |
| `pointer_speed` 出厂值 | **-3** | `settings get system pointer_speed` |
| 鼠标 `XScale` / `YScale` | **1.312 / 1.312**（≈420/320） | `dumpsys input` → `Cursor Input Mapper` |

## 3.2 开发机基线（参考）

| 项 | 值 |
|---|---|
| 系统 | Windows 11 Pro build 26200 x64 |
| 屏幕 | 单屏 AUO 面板 2560×1600 @165Hz，**缩放 150%（DPI 144）** |
| GPU | NVIDIA RTX 3080 Ti Laptop + Intel Iris Xe（混合） |
| adb | 37.0.1（`platform-tools`） |
| 无线 adb | `192.168.68.10:46511` |
| scrcpy | 5.0.1（只用其 `scrcpy-server` jar） |

## 3.3 已验证结论（全篇设计依据）

| # | 结论 | 证据 | 对设计的影响 |
|---|---|---|---|
| **F1** | **UHID 完全可用**，无需 root | `exec 3<>/dev/uhid` → OPEN OK；写入非法载荷返回 `EINVAL` 而非 `EACCES`；`scrcpy --mouse=uhid` 后 `dumpsys input` 出现 `15: scrcpy Classes: CURSOR` | 整条技术路线成立 |
| **F2** | 绝对注入**不产生系统指针** | `pointer_location=1` 悬停时读到 `X:0.0 Y:0.0`；悬停前后截图仅状态栏时钟有差异 | 永久放弃绝对注入 |
| **F3** | Raw Input 是**累加前**数据 | 光标被钉在屏幕边缘时 `WM_INPUT` 仍上报真实位移 | "PC 光标不动 + 平板指针继续走"成立 |
| **F4** | `XScale` **不受** `pointer_speed` 影响 | 把 `pointer_speed` 改成 -3/0/3/7，`XScale` 恒为 1.312 | 速度系数在更上层且不可读 ⇒ **必须把 pointer_speed 归零** |
| **F5** | `dumpsys input` **不输出**指针坐标 | 全文搜索只有 `Pointer Speed: -3` | 位置只能靠估计器；标定只能靠 `pointer_location` 叠加层 |
| **F6** | headless 模式可行 | `video=false control=true` 启动成功，客户端读 1 字节 `0x00` dummy（2026-11-02 实测修正；原记 4 字节有误） | 不需要视频/窗口 |
| **F7** | 强杀客户端会留**幽灵 UHID 设备** | 平板残留 `app_process` 持有 `/dev/uhid` fd，`ps \| grep scrcpy` **抓不到**（进程名是 `app_process`） | 必须 `pkill -9 -f com.genymobile.scrcpy.Server` 与 `...CleanUp`（命令行不含 jar 文件名，2026-11-02 实测修正） |
| **F8** | 首次点击不丢 | scrcpy 未聚焦时单击仍正确命中 | 无需特殊处理焦点 |

## 3.4 必须在 S0 验证的残留假设

| # | 假设 | 验证法 | 不成立时的退路 |
|---|---|---|---|
| **H1** | 相对鼠标**无加速残量**：反向推 1 计数立即有响应 | `pointer_location=1`，连推 30×`-127` 到左上角，再发 1×`+1`，看 X 是否 +1.3 | 若滞后，改标定补偿曲线（6.4.5） |
| **H2** | 实际 `px/count` **等于** `XScale`(1.312) | 推固定 100 计数，读叠加层位移 | 若不等，启动时自动标定一次并缓存 |
| **H3** | MagicOS 边缘手势不抢鼠标 | 指针贴屏幕最左/右/下停留 2 秒 | 加 `boundary_inset_px`（8–16px） |

## 3.5 已知限制（不解决，写进用户文档）

| 限制 | 原因 |
|---|---|
| UAC 提权窗口内无法转发键鼠 | Windows UIPI 隔离 |
| 平板熄屏/锁屏会断连 | scrcpy-server 随显示状态退出 |
| 与其它 adb 程序互斥 | 同一设备 UHID 设备名/端口冲突 |
| 无线 adb 有 20–50ms 抖动 | Wi-Fi 链路 |
| 平板若限制后台进程 | server 可能被杀，需加入白名单 |

---

# 4. 总体架构

## 4.1 分层

```
┌───────────────────────── Windows / Python ─────────────────────────┐
│  UI 层      PySide6：主窗 / 画布 / 设置 / 日志 / 托盘 / 向导         │
│              ↕ Qt 信号槽（只读订阅，不直接改状态）                    │
│  ────────────────────────────────────────────────────────────────  │
│  核心层     Router（状态机·唯一写者）                                 │
│              ├─ CursorEstimator   平板指针位置估计                   │
│              ├─ Placement/DesktopMap 外轮廓边段与摆位                 │
│              └─ Metrics           频率/延迟统计                      │
│  ────────────────────────────────────────────────────────────────  │
│  平台层     win/          tablet/                                    │
│             RawInput       Adb / Server / Session / Uhid            │
│             KeyboardHook   Protocol / Keymap                        │
│             CursorMask                                              │
│             DesktopMap                                              │
└──────────────────────────────┬─────────────────────────────────────┘
                               │ adb forward tcp:1234 → localabstract:scrcpy
┌──────────────────────────────▼───── Android ───────────────────────┐
│  app_process  com.genymobile.scrcpy.Server                          │
│    video=false audio=false control=true                              │
│       ↓ 写 /dev/uhid                                                │
│  虚拟鼠标（CURSOR）+ 虚拟键盘（KEYBOARD）                             │
│       ↓                                                             │
│  系统 PointerController：累加位移 → 夹紧边界 → 绘制指针                │
└─────────────────────────────────────────────────────────────────────┘
```

## 4.2 一次完整穿越的时序

```
t0  用户把鼠标推向右边缘
t1  RawInput: WM_INPUT 累计外推 42px
t2  Router: IDLE → ARMING(观察 80ms) → 判定命中合法外轮廓段 → CAPTURED
t3  Mask.show() + ClipCursor(右边缘 1px 条)
t4  Estimator.reset() → 算入口点 P0（沿边比例继承）
t5  Estimator.warp_to(P0) → 打包 26 个 HID 报告 → 发送线程 flush（≈3ms）
t6  之后每个 WM_INPUT → 1:1 转 HID 报告 → 平板
     平板系统把指针画在左边缘 ✓
t7  用户把平板指针推到左边缘并继续外推 26px
t8  Estimator 报告 LEFT 命中 → Router: CAPTURED → RETURNING
t9  Mask.hide() + ClipCursor(None) + SetCursorPos(落点)
t10 Router → IDLE（250ms 去抖）
```

## 4.3 关键不变量（Invariants）

实现时必须保持，测试要断言：

| # | 不变量 |
|---|---|
| **I1** | `CAPTURED` 之外，MUST NOT 发送任何 HID 鼠标位移 |
| **I2** | `CursorMask` 显示 ⇔ `ClipCursor` 生效 ⇔ 状态为 `CAPTURED`（三者同步） |
| **I3** | `Estimator` 每帧 feed 后立即 `clamp()`，位置始终 ∈ `[0, W-1] × [0, H-1]` |
| **I4** | 任何退出路径（正常/异常/崩溃/强杀）都必须执行清理（12.3） |
| **I5** | 钩子回调内无阻塞调用（无 socket / 无 adb / 无文件） |
| **I6** | 同一时刻只有一个 `Router` 实例（单实例锁） |

---

# 5. 技术决策（ADR）

| # | 决策 | 备选 | 理由（"because"） |
|---|---|---|---|
| **D1** | UHID 相对鼠标 | 绝对注入 | 绝对注入不产生系统指针（F2），G1 无法满足 |
| **D2** | headless（`video=false`） | 全量投屏 | 相对位移与分辨率/DPI/黑边无关；省 8–30Mbps 与 GPU；无窗口 ⇒ 无焦点/激活问题 |
| **D3** | PC 端累加器估位置 | 读平板坐标 | `dumpsys input` 不输出坐标（F5），无 root 无法读 |
| **D4** | 复用 scrcpy-server jar 作 UHID 宿主 | 自写 APK / 自写 NDK 程序 | 已实现 UHID_CREATE/INPUT/DESTROY 与协议；无需编译环境、无需装 APK |
| **D5** | 键盘走 UHID | `input keyevent` | 只有真实输入设备才能正常用输入法打中文，且 40–120ms → ~6ms |
| **D6** | Raw Input | `pynput`/`GetCursorPos` 差分 | 差分在屏幕边缘恒为 0（无法判断"还在外推"），且被加速污染（F3） |
| **D7** | 外轮廓边段模型 | 直接用显示器矩形四边 | 内缝不可吸附，否则鼠标会穿进显示器之间的缝（13.2） |
| **D8** | GUI = PySide6 Widgets + QSS 主题 | Tkinter / QML / Web / WinUI3 | 需要 QGraphicsView 做拖拽画布 + 高 DPI + 托盘；WinUI3 在 Python 无可用绑定 |
| **D9** | 配置 YAML + 热应用 | 仅 GUI 存注册表 | 便于手改、diff、自动化测试 |
| **D10** | 引擎全程**物理像素** | 逻辑像素 | Raw Input 本身是物理计数；DPI 只在 GUI 渲染换算一次 |
| **D11** | 窗口用**原生框架 + DWM 暗色/圆角** | 自绘无边框 | 一行 ctypes 换来原生圆角/投影/贴边布局，比伪装无边框可靠 |
| **D12** | 发送节奏 1000Hz、每帧 ≤8 报文、不丢老数据 | 125Hz + 丢帧 | 对齐平板 `×1.3125` 缩放仍需连续位移；丢帧是"手感飘"的根因 |

## 5.1 被明确否决的方案

| 方案 | 否决理由 |
|---|---|
| 虚拟显示器（IddCx）+ 投屏全屏 | 幽灵屏无物理输出 ⇒ 看不见；改 `Program Files` 与驱动 ⇒ 要提权；两屏缩放不同会毁映射；与"平板要有指针"零关系 |
| `INJECT_TOUCH_EVENT` 绝对定位 | 精确但无指针、会画触控圈；且仍需估计器才能无缝（白亏手感） |
| 仅模式切换（按下热键切换目标） | 不满足 G3"到达边缘自动返回" |
| 抄荣耀 `virtbus.sys` | 需签名驱动 + 提权 + 无源码 |
| 直接改平板 `pointer_speed` 来"对齐速度" | 治标：用户改一次系统设置就全乱；且不能解决 STALE 位置 |

---

# 6. 核心设计

## 6.1 连接层（`tablet/`）

### 6.1.1 设备选择

```
枚举 adb devices
 ├─ 配置指定 serial 且在线            → 用它
 ├─ 有 USB 设备（非 IP:port 形式）    → 优先（延迟低 6–10ms、不断连）
 ├─ 有无线设备                        → 用它，并启动保活
 └─ 无设备 → 托盘提示 + 引导（40s 内每 2s 重试，之后指数退避 1/2/4/8/16s 上限 5 次）
```

### 6.1.2 启动序列（必须幂等）

```python
def start(self) -> DeviceFacts:
    self._assert_adb()             # adb version / device state == "device"
    facts = self._probe()          # 分辨率/朝向/XScale/pointer_speed（见附录 C.2）
    self._normalize_pointer_speed()# 记录原值 → settings put system pointer_speed 0
    self._push_server()            # 校验远端 md5，不一致才推
    self._spawn_server()           # 见下方参数
    self._forward()                # adb forward tcp:1234 localabstract:scrcpy
    self._connect_socket()         # 读 1 字节 dummy，MUST == b"\x00"（实测修正：原 4 字节有误）
    self._create_uhid()            # UHID_CREATE 鼠标(id=1) + 键盘(id=2)
    self._keep_screen_on()         # svc power stayon true
    return facts
```

**服务端启动命令（实测可用，勿改参数名）**：

```bash
adb shell "CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar app_process / \
  com.genymobile.scrcpy.Server 5.0.1 \
  tunnel_forward=true video=false audio=false control=true \
  cleanup=true raw_stream=true send_dummy_byte=true max_size=4096"
```

| 参数 | 作用 |
|---|---|
| `video=false` / `audio=false` | 不要视频音频流 |
| `control=true` | 只要控制通道 |
| `tunnel_forward=true` | 配合 `adb forward` 的 socket 方式 |
| `send_dummy_byte=true` | 连接后先读 1 字节 `0x00` 做握手（实测修正：原 4 字节有误） |
| `cleanup=true` | 退出时清理（**注意 F7：强杀时不生效**；启动时 CleanUp 会删除 CLASSPATH 首项的 jar，重连需重新 push） |

### 6.1.3 退出序列（缺一不可）

```python
def stop(self):
    self._destroy_uhid()          # UHID_DESTROY ×2
    self._close_socket()
    self._kill_server()           # adb shell pkill -9 -f scrcpy-server-manual.jar
    self._forward_remove()        # adb forward --remove tcp:1234
    self._restore_pointer_speed() # settings put system pointer_speed <原值>
    self._verify_clean()          # dumpsys input 中 MUST NOT 再有 "scrcpy" 设备
```

**清理校验命令**（S0 起就写进 `stop()`）：

```bash
adb shell dumpsys input | grep -c "scrcpy"     # 期望 0
```

> ⚠️ **F7 详解**：直接杀 PC 端客户端时 `cleanup=true` **不生效**，平板会残留 `app_process` 持有 `/dev/uhid` fd，永久挂着一只幽灵鼠标（`15: scrcpy Classes: CURSOR`）。而且 `ps -A | grep scrcpy` **抓不到**它（进程名是 `app_process`）。必须先 `pkill -9 -f scrcpy-server-manual.jar`，再校验。

### 6.1.4 健康检查与重连

| 项 | 规则 |
|---|---|
| 心跳 | 每 2s 发一次 `dx=dy=0` 的 HID 报告 |
| 断连判定 | 连续 3 次 socket 写失败 / 收到 EOF |
| 重连 | 退避 1/2/4/8/16s，共 5 次；成功则重建 UHID 设备 |
| 状态 | 重连期间 `FAULT`，输入回流到系统（不吞键） |

---

## 6.2 输入捕获层（`win/`）

### 6.2.1 鼠标：Raw Input

```python
raw = RAWINPUTDEVICE(
    usUsagePage=0x01, usUsage=0x02,
    dwFlags=RIDEV_INPUTSINK,      # 后台也收，窗口无需焦点
    hwndTarget=self.message_window)
RegisterRawInputDevices([raw], 1, sizeof(raw))
```

**为什么不用光标差分**（D6）：屏幕边缘顶住时差分为 0，无法判断"还在往外推"；且被系统加速污染；混合 DPI 下还会跳变。

**处理分支**：

```python
if mouse.usFlags & MOUSE_MOVE_ABSOLUTE:
    # 绘图板 / RDP：0..65535 归一化 → 差分 → 按显示器物理尺寸放大（附录 A.4）
else:
    dx, dy = mouse.lLastX, mouse.lLastY     # 设备计数，单位不是像素
buttons = mouse.usButtonFlags               # 位标志
wheel   = 有符号 short mouse.usButtonData   # 120 = 一格；HiRes 可达 ±120×n
```

**关键性质（F3）**：Raw Input 是**累加前**数据，光标被 `ClipCursor` 钉住时仍照常上报 —— 这是"PC 光标不动、平板指针继续走"的物理基础。

### 6.2.2 键盘：低级钩子

```python
SetWindowsHookEx(WH_KEYBOARD_LL, low_level_proc, hinst, 0)   # 必须在有消息循环的线程
```

**回调只做三件事**：读取事件 → `queue.put_nowait` → 返回 `1`（吞掉）或 `CallNextHookEx`。

| 要点 | 做法 | 原因 |
|---|---|---|
| 无 I/O | 回调里禁止 socket/adb/日志写盘 | 超时会被系统静默卸载钩子 |
| 修饰键 | `GetAsyncKeyState(VK_LCONTROL/VK_RCONTROL/...)` 补齐 | 钩子事件不带修饰位 |
| Alt 组合 | 同时处理 `WM_SYSKEYDOWN/UP` | 否则 Alt+Tab 等漏掉 |
| 扩展键 | `scan & 0xE000` 判左右 | 区分左右 Ctrl/Alt/Shift |
| 看门狗 | 30s 内计数器无增长但系统有输入 → 重装钩子 | 钩子被卸载后的自愈 |

### 6.2.3 保命键

`Ctrl+Alt+S`（切换）与 `Ctrl+Alt+Q`（急停）**永不屏蔽**，并用 `RegisterHotKey` 额外注册系统级热键兜底 —— 防止钩子异常时用户被困在"按键全发到平板"。

---

## 6.3 虚拟桌面与多显示器（`win/desktop.py`）

### 6.3.1 坐标系

- **虚拟桌面坐标**：主屏左上角为原点，单位**物理像素**，**允许负值**（副屏在主屏左侧）
- **进程必须在创建任何窗口前声明 DPI 感知**：

```python
ctypes.windll.user32.SetProcessDpiAwarenessContext(-4)   # PER_MONITOR_AWARE_V2
```

- **平板坐标**：左上角原点，单位平板像素（横屏 3200×2136 / 竖屏 2136×3200）

### 6.3.2 枚举

```python
EnumDisplayMonitors(None, None, cb, 0)
  → GetMonitorInfoW → rcMonitor(物理矩形) / szDevice("\\\\.\\DISPLAY1") / dwFlags(&MONITORINFOF_PRIMARY)
  → GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI)
```

DPR 变化监听：`WM_DISPLAYCHANGE` + `WM_SETTINGCHANGE("ConvertibleLaptopMode"/"WindowMetrics")`。

### 6.3.3 外轮廓边段（本项目最关键的几何抽象）

**规则：平板只能吸附到"虚拟桌面外轮廓"，不能吸附到显示器之间的内缝。**

```
单屏：        ┌─────────┐            四边全部可吸附
              │   D1    │
              └─────────┘

左右双屏：    ┌────┬────┐            D1右、D2左 = 内缝（禁止）
              │ D1 │ D2 │            D1左/上/下、D2右/上/下 可吸附
              └────┴────┘

上下双屏：    ┌─────────┐            D1下、D2上 = 内缝
              │   D1    │
              ├─────────┤
              │   D2    │
              └─────────┘

错位/L 形：   ┌─────────┐            D1下边只有 x≥D2.right 的一段是外轮廓
              │   D1    │
              ├────┬────┘
              │ D2 │
              └────┘
```

**算法**：对每个显示器每条边，沿边按 1px 单位扫描"该段外侧是否被其它显示器占据"，把未占据的连续区间合并成 `EdgeSegment`。

```text
for m in monitors:
  for edge in (LEFT, RIGHT, TOP, BOTTOM):
    probe_line = 边外侧 1px 处的扫描线
    for seg in contiguous_runs(probe_line, where=lambda p: not any_monitor_contains(p)):
        segments.append(EdgeSegment(m.handle, edge, seg.start, seg.end))
```

- 复杂度 O(边像素数 × 显示器数)，4K×6 屏 < 5ms，**仅在显示器变化时重算**
- 结果缓存为不可变元组，GUI 与 Router 共享

### 6.3.4 摆位模型

```python
@dataclass(frozen=True)
class Placement:
    segment: EdgeSegment      # 吸附到的外轮廓段
    offset: float = 0.5       # 平板中心在段上的相对位置 [0,1]
    align: Align = Align.PROPORTIONAL   # PROPORTIONAL | CENTER | ONE_TO_ONE
    corner_priority: Edge | None = None # 角落同时命中两条边时的优先边
```

| 能力 | 实现 |
|---|---|
| 挂任意显示器任意可吸附边 | `segment` 精确到段 |
| 平板跨两块屏同一侧 | 相邻同 `edge` 且 `span` 相邻的两段合并为"虚拟段" |
| 角落裁决 | 6.3.6 |
| 显示器热插拔 | 重建 `DesktopMap`；`segment` 失效 → 重锚到主屏同 `edge` 最长段 + 提示 |

### 6.3.5 沿边对齐（跨屏手感核心）

```python
def pc_to_tablet_along(pc_along: int, seg: EdgeSegment, t_size: int) -> float:
    t = (pc_along - seg.span_start) / max(seg.length, 1)
    return clamp(t, 0.0, 1.0) * (t_size - 1)

def tablet_to_pc_along(t_along: float, seg: EdgeSegment, t_size: int) -> int:
    t = clamp(t_along / max(t_size - 1, 1), 0.0, 1.0)
    return int(round(seg.span_start + t * (seg.length - 1)))
```

`align = ONE_TO_ONE` 时取段中点、按 1:1 像素平移（段比平板短时两侧留白），用于"段长与平板边长差 10 倍以上"的极端布局。

### 6.3.6 角落裁决

```python
def resolve_corner(segs, delta, priority):
    if len(segs) == 1: return segs[0]
    scores = [(s, dot(delta, outward_normal(s.edge))) for s in segs]
    scores.sort(key=lambda t: -t[1])
    (a, sa), (b, sb) = scores[0], scores[1]
    if sa > 0 and (sa - sb) / max(sa, 1) > 0.15:
        return a                                   # 方向明确
    return next((s for s, _ in scores if s.edge == priority), a)   # 否则用偏好
```

---

## 6.4 平板指针位置估计器（`tablet/cursor.py`）

### 6.4.1 换算链

```
PC 物理像素位移 Δp
  └─×G→ HID 相对计数 c（整数，clamp ±127）
        └─×XScale→ 平板像素位移 Δx
```

$$c = \operatorname{clamp}\Big(\operatorname{round}(\Delta p \cdot G),\ -127,\ +127\Big),\qquad G = \frac{W_{tablet} / W_{monitor\_ref}}{XScale}\cdot gain$$

- `XScale` **运行时从设备读**（`dumpsys input` → `Cursor Input Mapper: XScale`），不要写死
- `pointer_speed` **启动时归零**（F4：速度系数隐藏在不可读的一层，归零才能让 1 计数 = XScale 像素）
- 开发机实测：`G = (3200/2560)/1.3125 ≈ 0.952`，`gain` 默认 1.0，GUI 可调 0.25–3.0

### 6.4.2 实现

```python
class CursorEstimator:
    def __init__(self, width: int, height: int, xscale: float):
        self.W, self.H, self.k = width, height, xscale
        self.x = self.y = 0.0
        self.resid = [0.0, 0.0]      # 小数残差，防慢速拖动丢步

    def feed(self, dpx: float, dpy: float, gain: float) -> tuple[int, int]:
        self.resid[0] += dpx * gain
        self.resid[1] += dpy * gain
        cx, cy = int(self.resid[0]), int(self.resid[1])
        self.resid[0] -= cx; self.resid[1] -= cy
        cx = max(-127, min(127, cx)); cy = max(-127, min(127, cy))
        self.x += cx * self.k; self.y += cy * self.k
        self.clamp()
        return cx, cy

    def clamp(self) -> None:
        """与系统行为对齐：PointerController 也会夹紧，我们不夹就会越走越偏"""
        self.x = min(max(self.x, 0.0), self.W - 1.0)
        self.y = min(max(self.y, 0.0), self.H - 1.0)

    def hit(self) -> Edge | None:
        if self.x <= 0.0:          return Edge.LEFT
        if self.x >= self.W - 1.0: return Edge.RIGHT
        if self.y <= 0.0:          return Edge.TOP
        if self.y >= self.H - 1.0: return Edge.BOTTOM
        return None

    def warp_to(self, tx: float, ty: float) -> list[tuple[int, int]]:
        """相对设备做绝对定位：穿屏时把指针搬到入口点"""
        out = []
        for _ in range(64):
            dx, dy = tx - self.x, ty - self.y
            if abs(dx) < self.k and abs(dy) < self.k:
                break
            cx = max(-127, min(127, int(dx / self.k)))
            cy = max(-127, min(127, int(dy / self.k)))
            self.x += cx * self.k; self.y += cy * self.k
            out.append((cx, cy))
        self.clamp()
        return out    # 批量一次 flush，26 个报文 ≈ 3ms
```

### 6.4.3 脱同步来源与自愈

| 来源 | 症状 | 对策 |
|---|---|---|
| 平板侧夹紧 | 累加器超界，反向有虚位 | 每帧 `clamp()`（I3） |
| ±127 截断 | 快甩少位移 | `resid` 残差下帧补齐 |
| 发送背压丢包 | 指针比估计器慢 | 每帧至少发 1 个、最多 8 个，**不丢老数据** |
| 平板旋转/重启 | 位置归中 | 监听旋转 → `reset(center)` |
| `pointer_speed` 被改 | 比例失衡 | 每 10s 复查，非 0 重新归零 |
| 用户按了平板触摸屏 | 真实指针被手指移动 | 检测触摸后 `reset(center)` + 可选提示 |

**验收**：连续 20 次来回偏差 ≤3px（A1）。

### 6.4.4 位置不可读的补救（标定）

`dumpsys input` 不输出坐标（F5）。标定流程（仅开发/诊断用，`tools/calibrate.py`）：

1. `adb shell settings put system pointer_location 1`
2. 发送已知计数序列（如 +100, 0）
3. `adb exec-out screencap -p` → 在叠加层文本区域 OCR/模板匹配读 `X:… Y:…`
4. 计算 `实测 px / 计数`，与 `XScale` 比对（H2）
5. `settings put system pointer_location 0` 复原

---

## 6.5 跨越状态机（`core/router.py`）

### 6.5.1 状态图

```
                    ┌───────────────────────────────────┐
                    ▼                                   │
   ┌────────┐ 边缘命中+外推≥40px  ┌────────┐ 校验通过  ┌─┴────────┐
   │  IDLE  ├────────────────────►│ ARMING ├──────────►│ CAPTURED │
   └───┬────┘  (80ms 观察窗)       └───┬────┘           └────┬─────┘
       │                              │ 失败/缩回            │ 平板边界+溢出≥24px
       │                              ▼                      ▼
       │                            IDLE               ┌──────────┐
       │                                              │RETURNING │
       │        ┌────────┐  断连                        └────┬─────┘
       └───────►│ FAULT  │◄──────────────┐  250ms 去抖       │
                └────────┘               └───────────────────┘
```

### 6.5.2 状态行为

| 状态 | 鼠标事件 | 键盘事件 | PC 光标 | 平板指针 |
|---|---|---|---|---|
| `IDLE` | 交系统 + 边缘监测 | 交系统 | 可见可动 | 冻结 |
| `ARMING` | **不吞**，继续监测 | 交系统 | 可见（顶边） | 冻结 |
| `CAPTURED` | 全转 HID | 按配置转发/吞 | 隐藏 + 锁定边缘 | 移动 |
| `RETURNING` | 吞本轮残余 | 交系统 | 恢复 + SetCursorPos | 冻结 |
| `FAULT` | 交系统 | 交系统 | 恢复 | 冻结 |

### 6.5.3 迁移条件（精确）

```python
# IDLE → ARMING
segs = desktop.segment_at(cursor.x, cursor.y, band=cfg.edge_band_px)   # 1px
方向检查：delta 与 segs[0].outward_normal 点积 > 0
→ arm_deadline = now + arm_window_ms(80), arm_accum = 0

# ARMING → CAPTURED
arm_accum += |delta|
若 arm_accum >= arm_px(40) 且 now <= arm_deadline       → CAPTURED
若 光标离开边缘带 / 方向反转 / now > arm_deadline        → IDLE

# CAPTURED → RETURNING
若平板上的有效按键仍按下（拖动/框选）→ 不评估出口，清零溢出
若 estimator 未抵达 entry_edge（直接检查对应坐标，角落可同时命中多条边）→ 清零溢出
在 entry_edge 连续向外累计溢出 >= exit_overflow_px(24)       → RETURNING
进入时已经按住的键保持中性，收到物理松键后才恢复平板按键转发

# RETURNING → IDLE
执行交还后 sleep 去抖 250ms；期间不评估 IDLE→ARMING
```

### 6.5.4 “溢出”的测量

```python
# 溢出 = 触边后仍在同方向的累计计数（不是 |位移| 的累加）
if hit_edge == Edge.LEFT and delta.dx < 0:
    overflow += -delta.dx
```

用"同方向累计"而非绝对累计，避免手抖来回蹭也触发。

---

## 6.6 光标遮罩与 ClipCursor（`win/mask.py`）

```python
class CursorMask:
    """接管期藏掉 PC 光标；不抢焦点、不进 Alt-Tab"""
    # 接管期需要接收 WM_SETCURSOR 才能把光标置空，因此不再使用 WS_EX_TRANSPARENT。
    EX_STYLE = WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
```

| 要点 | 做法 | 原因 |
|---|---|---|
| 覆盖范围 | 每显示器独立一个遮罩（推荐） | 错位/L 形桌面时单一包围盒会盖住不存在区域 |
| 视觉 | `SetLayeredWindowAttributes(alpha=1)` 近乎全透明 | 用户不应看到任何遮挡 |
| 隐藏光标 | 只由遮罩窗口处理 `WM_SETCURSOR`→`SetCursor(NULL)`；**禁止使用**进程级 `ShowCursor` 计数 | `ShowCursor` 计数跨线程失衡会让整个桌面光标永久消失；遮罩销毁即恢复（0.3.4 实测修复） |
| 释放 | `ClipCursor(None)` **必须在 finally** | 泄漏后果 = 用户光标被锁死在 1px 条里，系统不可用 |
| 崩溃兜底 | 三层（12.3） | 同上 |

> ⚠️ 上一版教训：`ClipCursor` 泄漏是灾难级故障。本设计要求**任何退出路径**都走 `Mask.release()`，并在 `Router.__del__` / `atexit` / 信号处理 / 看门狗四处兜底。

---

## 6.7 键盘转发（`tablet/keymap.py` + `core/router.py`）

### 6.7.1 数据通路

```
WH_KEYBOARD_LL 回调（只入队）
  → KbdQueue
    → 转发线程
        ├─ VK + scan → HID usage
        ├─ 维护修饰键位图（8 位）
        ├─ 生成 8 字节报告 [mod, 0x00, k1..k6]
        └─ UHID_INPUT 发给平板
```

### 6.7.2 报告语义（最关键）

**报告是全量状态，不是增量**：每次必须把"当前所有按下的键 + 修饰键位图"一起发，否则会粘键。

| 偏移 | 内容 |
|---|---|
| 0 | 修饰键位图：LCtrl 0x01 / LShift 0x02 / LAlt 0x04 / LGui 0x08 / RCtrl 0x10 / RShift 0x20 / RAlt 0x40 / RGui 0x80 |
| 1 | 保留 `0x00` |
| 2–7 | 当前按下的 6 个 HID usage，未用填 `0x00` |

- 修饰键状态机必须**自己维护**（钩子事件不带修饰位）
- 同时按超过 6 键（6KRO 上限）时按"先按优先"处理并记日志

### 6.7.3 VK → HID usage

| 规则 | 说明 |
|---|---|
| 字母 | `VK_A..VK_Z (0x41..0x5A)` → `0x04..0x1D`（`usage = vk - 0x3D`） |
| 数字 | `VK_1..VK_9 (0x31..0x39)` → `0x1E..0x26`；`VK_0 (0x30)` → `0x27` |
| 功能键 | `VK_F1..VK_F12` → `0x3A..0x45` |
| 左右修饰 | `MapVirtualKeyEx(vk, MAPVK_VK_TO_VSC_EX)` + `scan & 0xE000` |
| 其余 | 静态表（附录 B），未命中 → 走 `INJECT_KEYCODE` 兜底或丢弃并记日志 |

### 6.7.4 中文输入法

UHID 键盘被识别为**物理键盘** ⇒ 输入法按物理键位工作，候选框正常（这是相对上一版的关键优势）。

- 首次可能弹"选择键盘布局" → 引导执行
  `adb shell am start -a android.settings.HARD_KEYBOARD_SETTINGS`
- 布局默认 `US`；用户若改了布局导致错位 → 配置 `keyboard.layout`
- 中文快捷输入备选：`INJECT_TEXT`（UTF-8 直注入，不经 HID）

### 6.7.5 PC 端屏蔽

| 模式 | 行为 | 默认 |
|---|---|---|
| `suppress_pc: true` | 接管期钩子 `return 1`，PC 收不到 | ✅ |
| `suppress_pc: false` | 两边都收到 | — |
| 白名单 | `passthrough_keys`（如 `VK_LWIN`）永远只给 PC | — |
| 保命键 | `Ctrl+Alt+S` / `Ctrl+Alt+Q` 永不屏蔽 + `RegisterHotKey` 兜底 | 强制 |

### 6.7.6 防粘键

- 每次离开 `CAPTURED`：发**全零报告**
- 进程退出/崩溃：同样发全零（尽力而为，失败不影响退出）
- 每次状态迁移：重置修饰键位图

---

## 6.8 剪贴板（G9，SHOULD）

| 项 | 设计 |
|---|---|
| 方向 | PC → 平板（主）、平板 → PC（次） |
| 通道 | `INJECT_TEXT`（UTF-8），长度上限 4KB |
| 触发 | **仅**热键 `Ctrl+Alt+V`（接管瞬间不发，见下） |
| 不做 | 图片、富文本、持续监听 |

**为什么接管瞬间不发**（0.3.10 修正）：`INJECT_TEXT` 在服务端会被
`KeyCharacterMap.getEvents()` 翻译成**真实按键事件**注入当前焦点窗口。接管是
用户“把鼠标推过去”的高频动作，一旦自动发送，剪贴板里的 ASCII 文本就会当成
键盘输入打进平板前台 App（刷抖音时命中 `R` 就是「不感兴趣」），并让输入法/
提示条闪一下。所以只在用户显式按 `Ctrl+Alt+V` 时发送。

---

## 6.9 配置系统（`config.py`）

**路径**：`%APPDATA%\PadLend\config.yaml`（GUI 写入；手改后热加载）

```yaml
version: 1

device:
  serial: ""                    # 空 = 自动选择
  prefer_usb: true
  auto_connect: true
  server_port: 1234

placement:                      # 由 GUI 写入
  monitor: "\\\\.\\DISPLAY1"
  edge: right                   # left | right | top | bottom
  span_start: 0
  span_end: 1600
  offset: 0.5
  align: proportional           # proportional | center | one_to_one
  corner_priority: right

motion:
  gain: 1.0                     # 0.25 ~ 3.0
  report_hz: 1000               # 125 ~ 1000
  hid_clamp: 127                # ≤ 127（描述符上限）
  entry_margin_px: 2
  exit_overflow_px: 24
  debounce_ms: 250
  arm_px: 40
  arm_window_ms: 80
  edge_band_px: 1
  boundary_inset_px: 0          # H3 需规避手势时 8~16

keyboard:
  forward: true
  suppress_pc: true
  layout: us
  passthrough_keys: ["VK_LWIN", "VK_RWIN"]

hotkeys:
  toggle: "Ctrl+Alt+S"
  panic: "Ctrl+Alt+Q"
  clipboard: "Ctrl+Alt+V"

ui:
  language: zh_CN
  theme: dark
  tray_notifications: true
  show_pointer_ghost: true

misc:
  clipboard_sync: true
  keep_screen_on: true
  restore_pointer_speed: -3
```

**校验规则**（非法值 → 回退默认 + GUI 标红 + 日志）：

| 字段 | 约束 |
|---|---|
| `motion.hid_clamp` | 1–127 |
| `motion.report_hz` | 125–1000 |
| `motion.gain` | 0.25–3.0 |
| `placement.offset` | 0.0–1.0 |
| `placement.span_end` | > `span_start` |
| `placement.edge` | 枚举值 |

**迁移**：`version` 低于当前 → 逐级升级函数 `_migrate_v1_to_v2()`；无法识别 → 备份为 `config.yaml.bak` 并重置。

---

## 6.10 GUI（PySide6 Widgets + QSS）

### 6.10.1 主窗口布局

```
┌───────────────────────────────────────────────────────────────────────────────┐
│ PadLend       ● 已连接  CGA-W00 · 3200×2136 · 横屏      延迟 12ms   [⚙][—][×] │
├──────────────────────────────────────────────┬────────────────────────────────┤
│  屏幕布局（真实比例，可拖拽吸附）               │  📍 平板位置                    │
│                                              │   吸附边: (DISPLAY1) 右侧  ▾   │
│   ┌────────────────────┐   ┌──────────────┐  │   对齐:   等比     ▾           │
│   │ DISPLAY1           │   │  平板         │  │   偏移: ───●─────  0.50        │
│   │ 2560×1600  150%    │◄──┤ 3200×2136    │  │   角优先: 右  ▾                │
│   │ 主屏               │红 │ 横屏  [拖动]  │  │  ─────────────────────         │
│   │        ●           │   │      ○       │  │  🖱 手感                        │
│   │  (PC 光标)         │边 │  (估计指针)   │  │   速度: ────●───  1.00×        │
│   └────────────────────┘   └──────────────┘  │   触发阈值: ──●──  40px        │
│                                              │   穿回阈值: ──●──  24px        │
│   红虚线 = 可吸附外轮廓边段                    │  ─────────────────────         │
│   灰点线 = 内缝（不可吸附）                    │  ⌨ 键盘                        │
│                                              │   转发到平板   [✓]             │
│                                              │   屏蔽本机按键 [✓]             │
│                                              │   布局: US      ▾              │
│                                              │  ─────────────────────         │
│                                              │  ⚡ 快捷键                      │
│                                              │   切换: Ctrl+Alt+S  [编辑]     │
│                                              │   急停: Ctrl+Alt+Q  [编辑]     │
├──────────────────────────────────────────────┴────────────────────────────────┤
│  事件日志                            │ 指标                                    │
│  01:12:03.114 ARM   外推 42px 命中右段 │ 上报 998Hz  发送 998Hz  背压 0         │
│  01:12:03.118 CAP   入口 (0,1068)      │ RTT 12ms   p95 21ms  丢包 0.0%          │
│  01:12:05.902 HIT   LEFT 溢出 26px     │ CPU 2.1%   内存 84MB                    │
│  01:12:05.905 BACK  落点 (2558,1067)   │                                         │
└───────────────────────────────────────────────┴────────────────────────────────┘
```

### 6.10.2 画布交互（QGraphicsView）

| 操作 | 行为 |
|---|---|
| 拖平板 | 实时命中最近合法外轮廓段并高亮；松手生效 + 落盘 |
| 拖到角落 | 显示两条候选边，`Shift` 切换优先边 |
| 双击平板 | 切换横/竖屏预览（同步真实朝向） |
| 右键平板 | 对齐方式 / 移除 / 测试穿越 |
| 滚轮 / 空格拖 / `Ctrl+0` | 缩放 / 平移 / 复位 |
| 悬停显示器 | ToolTip：设备名、分辨率、DPI、缩放、主屏标记 |
| 指针幽灵 | 半透明圆点显示 PC 光标与估计的平板指针（`show_pointer_ghost`） |

### 6.10.3 视觉令牌（`ui/theme/tokens.py` → 生成 QSS）

| 令牌 | 值 |
|---|---|
| `bg_window` / `bg_surface` / `bg_raised` | `#1F1F1F` / `#272727` / `#323232` |
| `border` | `#3A3A3A` |
| `text_primary` / `text_secondary` | `#E8E8E8` / `#9A9A9A` |
| `accent` / `accent_hover` | `#4CC2FF` / `#6FCFFF` |
| `ok` / `warn` / `err` | `#4ADE80` / `#FBBF24` / `#F87171` |
| `capturing` | `#FF5C5C`（接管中：画布边框 + 托盘） |
| 圆角 | 卡片 8 / 按钮 6 / 输入 4 |
| 间距 | 4 / 8 / 12 / 16 / 24 |
| 控件高 | 常规 32 / 紧凑 28；图标 16（SVG） |
| 字体 | `Segoe UI Variable Text` → `Microsoft YaHei UI`；日志 `Cascadia Mono` |
| 字号 | 正文 13 / 标题 15·600 / 辅助 11 |

### 6.10.4 窗口现代化（DWM，不自绘无边框）

```python
hwnd = int(self.winId())
dwm = ctypes.windll.dwmapi
dwm.DwmSetWindowAttribute(hwnd, 20, ctypes.byref(ctypes.c_int(1)), 4)  # 暗色标题栏
dwm.DwmSetWindowAttribute(hwnd, 33, ctypes.byref(ctypes.c_int(2)), 4)  # Win11 圆角
```

一行 ctypes 换取原生圆角、投影、贴边布局（Snap Layouts）、原生动画。Win10 上不生效但无害。

### 6.10.5 QSS 已知短板与对策

| 短板 | 对策 |
|---|---|
| 无 `box-shadow` | 静态卡片用 `QGraphicsDropShadowEffect`（会禁用该控件硬件加速，仅少量使用） |
| `QComboBox` 弹出层要单独写 | `QComboBox QAbstractItemView { background:#272727; ... }` |
| 无过渡动画 | `QPropertyAnimation` 180ms `OutCubic`；**刻意克制**，工具软件重响应速度 |

### 6.10.6 高 DPI

```python
QGuiApplication.setHighDpiScaleFactorRoundingPolicy(
    Qt.HighDpiScaleFactorRoundingPolicy.PassThrough)
```

- 布局一律 Qt 逻辑单位（Qt6 自动换算）
- 画布内屏幕矩形用**物理像素 × 渲染缩放系数**，保证 150% 屏上比例正确
- 图标一律 SVG

### 6.10.7 托盘

| 元素 | 行为 |
|---|---|
| 图标 | 灰=未连接 / 绿=空闲 / **红=接管中** / 黄=故障 |
| 左键 | 显隐主窗口 |
| 右键 | 手动接管·交还 / 重连 / 打开配置 / 复制诊断 / 退出 |
| 气泡 | 仅首次连接、断连、重锚定（可关） |

### 6.10.8 其他界面

- **首次向导**：检查 adb → USB 调试 → UHID 可写 → server jar → 完成
- **诊断页**：`adb devices`、`dumpsys input` 关键段、`XScale`/`pointer_speed`、UHID 注册情况、一键导出诊断包
- **关于**：版本 / 许可 / 致谢（scrcpy、InputShare）

---

# 7. 工程规范

## 7.1 目录结构

```
padlend/
├─ pyproject.toml
├─ requirements.txt
├─ README.md
├─ main.py                      # 入口：DPI → 单实例 → 装配 → Qt 循环
├─ config.py                    # YAML 加载/校验/热应用/迁移
├─ constants.py                 # 常量、超时、HID 描述符
├─ core/
│  ├─ router.py                 # 状态机（唯一写者）
│  ├─ events.py                 # 事件类型
│  ├─ state.py                  # State 枚举 + 合法迁移表
│  ├─ metrics.py                # 频率/延迟/背压统计
│  └─ errors.py                 # 错误码
├─ win/
│  ├─ dpi.py
│  ├─ desktop.py                # 枚举 + 外轮廓边段 + 热插拔
│  ├─ rawinput.py               # message-only 窗口 + WM_INPUT 线程
│  ├─ keyboard_hook.py          # WH_KEYBOARD_LL + 看门狗
│  ├─ mask.py                   # 遮罩 + ClipCursor + 兜底
│  └─ single_instance.py        # 命名互斥体
├─ tablet/
│  ├─ adb.py                    # adb 封装（含 MSYS_NO_PATHCONV 兜底）
│  ├─ server.py                 # jar 推送/校验、app_process 启停、清理校验
│  ├─ session.py                # TabletSession 门面
│  ├─ uhid.py                   # UHID 报文构造
│  ├─ cursor.py                 # CursorEstimator
│  ├─ keymap.py                 # VK→HID + 修饰键状态机
│  └─ protocol.py               # 控制协议编解码
├─ ui/
│  ├─ main_window.py
│  ├─ canvas.py                 # QGraphicsView
│  ├─ canvas_items.py           # 显示器/平板/边段/指针幽灵
│  ├─ settings_panel.py
│  ├─ log_view.py
│  ├─ tray.py
│  ├─ wizard.py
│  ├─ diagnostics.py
│  ├─ style.py                  # 令牌 → QSS + DWM
│  └─ theme/{tokens.py,dark.qss,icons/*.svg}
├─ vendor/
│  ├─ scrcpy-server-v5.0.1.jar
│  └─ adb/platform-tools/       # 或从 PATH 找
├─ tools/
│  ├─ probe.py                  # 打印设备事实
│  └─ calibrate.py              # XScale/px-per-count 标定
├─ tests/
│  ├─ test_geometry.py
│  ├─ test_estimator.py
│  ├─ test_keymap.py
│  ├─ test_config.py
│  ├─ test_protocol.py
│  └─ fixtures/desktop_*.json
└─ docs/
   ├─ DESIGN.md                 # 本文件
   ├─ TROUBLESHOOTING.md
   └─ CHANGELOG.md
```

**依赖方向（禁止反向 import）**：

```
ui → core → win/tablet → constants
  ✗ win/tablet 不得 import ui
  ✗ core 不得直接 import win（通过协议/抽象注入，便于单测用 fake）
```

## 7.2 依赖与版本

`requirements.txt`：

```
PySide6>=6.7,<7
PyYAML>=6.0
psutil>=5.9          # 指标采集（可选）
pytest>=8.0          # 开发
ruff>=0.5            # 开发
```

| 外部二进制 | 来源 | 存放 |
|---|---|---|
| `adb` | Android platform-tools | `vendor/adb/` 或 PATH |
| `scrcpy-server` | scrcpy v5.0.1 发布的 `scrcpy-server` | `vendor/scrcpy-server-v5.0.1.jar` |

**不用** `pywin32`（全部走 `ctypes`）、不用 `pynput`、不用 `adbutils`（自己包 `adb.exe`，行为可控）。

## 7.3 代码规范

| 项 | 约定 |
|---|---|
| 语言级 | Python 3.11+，全量类型注解，`from __future__ import annotations` |
| 风格 | `ruff` 默认规则 + line-length 100 |
| 日志 | `logging` + 统一格式（7.4）；`print` 仅限 `tools/` |
| 异常 | 不吞异常；顶层统一 `except` 转 `ErrorCode` 并落日志 |
| 线程 | 线程函数内 `try/except` 兜底，异常写入日志并触发 `FAULT` |
| 常量 | 魔法数字一律进 `constants.py` 或 config |
| 禁止 | `except: pass`、裸 `threading.Thread` 无名字、回调内 I/O |

## 7.4 日志与指标格式

**日志行**（便于 grep 与测试断言）：

```
2026-11-02 01:12:03.114 INFO  [router] cap.start edge=RIGHT seg=0 entry=(0,1068)
2026-11-02 01:12:05.902 INFO  [router] cap.hit edge=LEFT overflow=26
2026-11-02 01:12:05.905 INFO  [router] cap.end exit=(2558,1067)
2026-11-02 01:12:06.100 WARN  [tablet] health.reconnect attempt=1
```

**事件名**（固定词表，测试据此断言）：`cap.start` `cap.hit` `cap.end` `arm.fail` `state.fault` `tablet.connect` `tablet.reconnect` `tablet.cleanup.ghost` `kbd.suppress.on` `cfg.reload`。

**指标**（`Metrics` → GUI 每 500ms 推送一次）：

```python
{
  "raw_hz": 998.0, "send_hz": 998.0, "backlog": 0,
  "rtt_ms": 12, "p95_ms": 21, "drop_pct": 0.0,
  "cpu_pct": 2.1, "mem_mb": 84,
}
```

## 7.5 单实例与资源清理

```python
mutex = CreateMutexW(None, False, "Global\\PadLend.SingleInstance")
if GetLastError() == ERROR_ALREADY_EXISTS:  # 已有实例
    唤醒已有实例窗口 → 退出
```

启动时若检测到"上次非正常退出"（`exit.clean` 标记不存在）→ **先跑一次全量清理**（12.3）再启动。

---

# 8. 接口契约

> 以下签名是**冻结契约**：实现可加私有方法，但公开签名 MUST NOT 改。

## 8.1 数据类型

```python
# core/events.py
@dataclass(frozen=True)
class RawDelta:
    dx: int; dy: int
    buttons: int = 0
    wheel: int = 0
    absolute: bool = False
    ts: float = 0.0

@dataclass(frozen=True)
class KeyEvent:
    vk: int; scan: int
    is_extended: bool
    down: bool
    ts: float

# win/desktop.py
@dataclass(frozen=True)
class Rect:
    left: int; top: int; right: int; bottom: int
    @property
    def width(self) -> int: ...
    @property
    def height(self) -> int: ...
    def contains(self, x: int, y: int) -> bool: ...

@dataclass(frozen=True)
class MonitorInfo:
    handle: int
    device: str          # "\\\\.\\DISPLAY1"
    rect: Rect           # 物理像素，可为负
    dpi: int             # 96 / 144 ...
    primary: bool

class Edge(Enum):
    LEFT = "left"; RIGHT = "right"; TOP = "top"; BOTTOM = "bottom"
    def outward_normal(self) -> tuple[int, int]: ...

@dataclass(frozen=True)
class EdgeSegment:
    monitor_handle: int
    edge: Edge
    span_start: int      # 沿边方向虚拟桌面坐标（含）
    span_end: int        # （不含）
    @property
    def length(self) -> int: ...

@dataclass(frozen=True)
class Placement:
    segment: EdgeSegment
    offset: float = 0.5
    align: Align = Align.PROPORTIONAL
    corner_priority: Edge | None = None

class Align(Enum):
    PROPORTIONAL = "proportional"
    CENTER = "center"
    ONE_TO_ONE = "one_to_one"

# core/state.py
class State(Enum):
    IDLE = "idle"; ARMING = "arming"; CAPTURED = "captured"
    RETURNING = "returning"; FAULT = "fault"

LEGAL_TRANSITIONS: dict[State, frozenset[State]] = {
    State.IDLE:      frozenset({State.ARMING, State.FAULT}),
    State.ARMING:    frozenset({State.CAPTURED, State.IDLE, State.FAULT}),
    State.CAPTURED:  frozenset({State.RETURNING, State.FAULT}),
    State.RETURNING: frozenset({State.IDLE, State.FAULT}),
    State.FAULT:     frozenset({State.IDLE}),
}
```

## 8.2 模块接口

```python
# win/desktop.py
class DesktopMap:
    def refresh(self) -> None: ...
    @property
    def monitors(self) -> tuple[MonitorInfo, ...]: ...
    @property
    def bounding_box(self) -> Rect: ...
    def exterior_segments(self) -> tuple[EdgeSegment, ...]: ...
    def segments_at(self, x: int, y: int, band: int = 1) -> tuple[EdgeSegment, ...]: ...
    def monitor_at(self, x: int, y: int) -> MonitorInfo | None: ...
    def resolve_corner(self, segs: tuple[EdgeSegment, ...],
                       delta: tuple[int, int],
                       priority: Edge | None) -> EdgeSegment | None: ...
    def entry_point(self, pl: Placement, pc_along: int,
                    tablet_w: int, tablet_h: int) -> tuple[float, float]: ...
    def exit_position(self, pl: Placement, tablet_along: float,
                      tablet_w: int, tablet_h: int) -> tuple[int, int]: ...

# win/rawinput.py
class RawInputSource:
    def __init__(self, on_event: Callable[[RawDelta], None]): ...
    def start(self) -> None: ...
    def stop(self) -> None: ...

# win/keyboard_hook.py
class KeyboardHook:
    def __init__(self, on_event: Callable[[KeyEvent], bool]): ...  # 返回 True=吞掉
    def install(self) -> None: ...
    def uninstall(self) -> None: ...
    @property
    def healthy(self) -> bool: ...

# win/mask.py
class CursorMask:
    def show(self, clip_rect: Rect | None) -> None: ...
    def hide(self) -> None: ...
    def release(self) -> None: ...          # 幂等，必须能重复调用
    @property
    def active(self) -> bool: ...

# tablet/adb.py
class Adb:
    def devices(self) -> list[DeviceEntry]: ...
    def shell(self, *args: str, timeout: float = 5.0) -> str: ...
    def shell_ok(self, *args: str) -> bool: ...
    def forward(self, local_port: int, remote: str) -> None: ...
    def forward_remove(self, local_port: int) -> None: ...
    def push(self, local: Path, remote: str) -> None: ...
    def md5(self, remote: str) -> str | None: ...

# tablet/session.py
@dataclass
class DeviceFacts:
    serial: str; model: str
    width: int; height: int
    orientation: int      # 0..3
    xscale: float
    pointer_speed: int

class TabletSession:
    def start(self) -> DeviceFacts: ...
    def stop(self) -> None: ...
    def send_mouse(self, buttons: int, dx: int, dy: int, wheel: int) -> None: ...
    def send_key(self, modifier: int, usages: Sequence[int]) -> None: ...
    def send_keycode(self, keycode: int, down: bool) -> None: ...   # 兜底
    def inject_text(self, text: str) -> None: ...
    def health(self) -> HealthReport: ...

# tablet/cursor.py
class CursorEstimator:
    def __init__(self, width: int, height: int, xscale: float): ...
    def reset(self, x: float | None = None, y: float | None = None) -> None: ...
    def feed(self, dpx: float, dpy: float, gain: float) -> tuple[int, int]: ...
    def hits(self) -> Edge | None: ...
    def clamp(self) -> None: ...
    def warp_to(self, tx: float, ty: float) -> list[tuple[int, int]]: ...
    @property
    def position(self) -> tuple[float, float]: ...

# core/router.py
class Router:
    def __init__(self, cfg: Config, session: TabletSession,
                 desktop: DesktopMap, mask: CursorMask): ...
    def start(self) -> None: ...
    def stop(self) -> None: ...
    def on_raw(self, ev: RawDelta) -> None: ...
    def on_key(self, ev: KeyEvent) -> bool: ...
    def toggle_manual(self) -> None: ...
    def set_placement(self, p: Placement) -> None: ...
    def resync_pointer(self) -> None: ...        # 把估计器重置到中心（热键用）
    # 回调（GUI 订阅，MUST 在 GUI 线程外触发，由 UI 层转信号）
    on_state_changed: Callable[[State, State, str], None]
    on_metrics: Callable[[dict], None]
```

## 8.3 配置对象

```python
@dataclass
class Config:
    device: DeviceCfg
    placement: PlacementCfg
    motion: MotionCfg
    keyboard: KeyboardCfg
    hotkeys: HotkeyCfg
    ui: UiCfg
    misc: MiscCfg

    @classmethod
    def load(cls, path: Path) -> "Config": ...
    def save(self, path: Path) -> None: ...
    def validate(self) -> list[str]: ...     # 返回问题列表，空 = 合法
```

## 8.4 错误码

| 码 | 名称 | 触发 | 处置 |
|---|---|---|---|
| `E100` | ADB_NOT_FOUND | 找不到 adb | 提示配置路径 |
| `E101` | DEVICE_OFFLINE | 设备不在线/未授权 | 引导授权 |
| `E102` | UHID_UNAVAILABLE | 写 `/dev/uhid` 失败 | 降级模式（13.1） |
| `E103` | SERVER_START_FAILED | app_process 起不来 | 重推 jar 重试 2 次 |
| `E104` | FORWARD_FAILED | 端口占用/forward 失败 | 换端口 1234–1244 |
| `E105` | HANDSHAKE_FAILED | dummy 字节不符 | 版本不匹配提示 |
| `E106` | SOCKET_BROKEN | 写失败/EOF | FAULT + 重连 |
| `E200` | HOOK_FAILED | SetWindowsHookEx 失败 | 提示重启/权限 |
| `E201` | RAWINPUT_FAILED | 注册失败 | 提示重启 |
| `E202` | CLIPCURSOR_LEAK | 看门狗检测到不一致 | 强制释放 |
| `E300` | CONFIG_INVALID | 校验失败 | 回退默认 + 标红 |
| `E301` | PLACEMENT_INVALID | 段失效 | 重锚主屏 |
| `E900` | UNEXPECTED | 未捕获异常 | 落日志 + FAULT + 清理 |

---

# 9. 协议与报文

## 9.1 链路

```
PC --TCP 127.0.0.1:{port}--> adb forward --> localabstract:scrcpy --> scrcpy-server
```

- 握手：连接后先读 **1 字节**，MUST 等于 `00`（`send_dummy_byte=true`；2026-11-02 实测修正，scrcpy 5.0.1 `DesktopConnection` 只写一个 0 字节）
- 之后为连续消息流，无长度前缀，按首字节 `type` 分派
- 单向：PC → 平板（本方案不消费平板回传）

## 9.2 控制消息

| type | 名称 | 载荷 |
|---|---|---|
| 0 | INJECT_KEYCODE | action(1) keycode(4) repeat(4) metaState(4) |
| 1 | INJECT_TEXT | len(4) utf8(len) |
| 2 | INJECT_TOUCH_EVENT | （**不使用**） |
| 3 | INJECT_SCROLL_EVENT | （**不使用**，滚轮走 UHID） |
| 4 | BACK_OR_SCREEN_ON | action(1) |
| 12 | **UHID_CREATE** | **id(2) vendor(2) product(2) nameLen(1) name descLen(2) desc** |
| 13 | **UHID_INPUT** | **id(2) len(2) data(len)** |
| 14 | **UHID_DESTROY** | **id(2)** |
| 15 | OPEN_HARD_KEYBOARD_SETTINGS | 无 |

> 字节序：**大端**（网络序）。实现前用 scrcpy 源码 `app/src/control_msg.h` 复核一次枚举值。

## 9.3 UHID 报文

**鼠标**（4 字节报告）：

| 字节 | 含义 |
|---|---|
| 0 | 按键位图：bit0 左、bit1 右、bit2 中、bit3 侧1、bit4 侧2 |
| 1 | ΔX，有符号，`-127..127`，编码 `value & 0xFF` |
| 2 | ΔY，同上 |
| 3 | 滚轮，有符号，`-127..127` |

**键盘**（8 字节报告）：

| 字节 | 含义 |
|---|---|
| 0 | 修饰位图 |
| 1 | 保留 `0x00` |
| 2–7 | 6 个 HID usage，未用 `0x00` |

**设备 ID**：鼠标 `0x0001`，键盘 `0x0002`（自定义，双方一致即可）。

## 9.4 HID 描述符

见附录 A（鼠标 5 键 + 相对 X/Y + 滚轮；键盘 6KRO）。

## 9.5 版本兼容策略

- jar 文件名与版本号绑定（`scrcpy-server-v5.0.1.jar`），启动参数里的版本字符串 MUST 与 jar 一致
- 升级流程：换 jar → 改参数版本号 → 跑 A1 回归
- 协议层隔离在 `tablet/protocol.py`，版本差异只改这一个文件

---

# 10. 并发模型

## 10.1 线程

| # | 线程名 | 职责 | 阻塞点 | 通信 |
|---|---|---|---|---|
| T1 | `ui` | Qt 界面 | Qt 事件循环 | 收信号（只读） |
| T2 | `rawinput` | message-only 窗口 + `WM_INPUT` | `GetMessage` | `RawDelta` → Router 队列 |
| T3 | `kbdhook` | `WH_KEYBOARD_LL` + 看门狗 | `GetMessage` | `KeyEvent` → Router 队列 |
| T4 | `router` | 状态机、累加器、组包 | 条件变量 | 出队 → 发送队列 |
| T5 | `sender` | 1000Hz 批量 flush | `sleep` 到下一帧 | `deque` + `Lock` |
| T6 | `supervisor` | adb 健康、旋转、显示器变化 | `sleep 2s` | 事件 → Router |

## 10.2 队列与背压

```python
raw_queue  = queue.Queue(maxsize=4096)   # 满则丢最老（Raw Input 绝不可阻塞）
key_queue  = queue.Queue(maxsize=256)
send_queue = collections.deque(maxlen=64)
```

- `raw_queue` 满 → 丢最老 + `drop_pct` 计数（正常应恒为 0）
- `send_queue` 满 → 合并相邻的零位移心跳（不丢真实位移）

## 10.3 发送节奏

```python
period = 1.0 / cfg.report_hz
while running:
    t0 = time.perf_counter()
    batch = drain(send_queue, limit=8)
    for msg in batch:
        sock.sendall(msg)
    if not batch:
        sock.sendall(heartbeat)           # dx=dy=0
    sleep_until(t0 + period)
```

- `timeBeginPeriod(1)` 提升计时精度
- 单帧最多 8 个报文；`warp_to` 的 26 个报文在接管瞬间一次性塞入并允许单帧突破（标记为 burst）

## 10.4 锁与禁止事项

| 对象 | 保护 |
|---|---|
| `CursorEstimator` | 只由 T4 访问，无需锁 |
| `send_queue` | `threading.Lock` |
| `DesktopMap` | 不可变快照，变化时整体替换引用（copy-on-write） |
| `TabletSession` | 发送只由 T5 调用；启停在 T6 |

**禁止**：钩子回调内做任何阻塞操作；GUI 线程内调用 adb；跨线程直接改 GUI 控件。

---

# 11. 性能预算

| 指标 | 无线 | USB | 测量 |
|---|---|---|---|
| 指针端到端延迟 p50 | < 25ms | < 15ms | 帧标记/计数比对 |
| p95 | < 40ms | < 25ms | 同上 |
| 上报 / 发送频率 | 1000Hz | 1000Hz | `Metrics` |
| CPU（接管中） | < 5% 单核 | 同 | `psutil` |
| CPU（空闲） | < 1% | 同 | 同 |
| 内存常驻 | < 120MB | 同 | 同 |
| 带宽 | < 20KB/s | 同 | 报文计数 |
| 穿屏瞬移耗时 | < 5ms | 同 | 计时 |
| 冷启动到可用 | < 3s | 同 | 计时 |

> 参考：仅控制通道时 1000Hz × 6 字节 ≈ 6KB/s，带宽不是瓶颈；延迟主体在 adb 链路与 Android 输入子系统。

---

# 12. 错误处理与自愈

## 12.1 故障矩阵

| 故障 | 检测 | 处置 | 用户感知 |
|---|---|---|---|
| 设备掉线 | socket 连续 3 次写失败 / EOF | `FAULT` → 退避重连 5 次 | 托盘黄 + 气泡 |
| 平板熄屏/锁屏 | server 退出 / EOF | 保持 PC 侧，提示点亮平板 | 气泡 |
| server 崩溃 | 心跳无响应 | 重推 jar 重启（≤2 次） | 日志 |
| 端口占用 | 启动 bind 失败 | 自动扫 1234–1244 | 日志 |
| 平板旋转 | supervisor 轮询 viewport | 重建 facts + `reset(center)` | 画布转向 |
| 显示器热插拔 | `WM_DISPLAYCHANGE` | 重建 DesktopMap；段失效则重锚 | 重绘 + 气泡 |
| DPI 变化 | 同上 | 重建渲染缩放；物理坐标不变 | 重绘 |
| 钩子被卸载 | 看门狗 30s 无事件但系统有输入 | 重装 | 日志 |
| `pointer_speed` 被改 | 10s 轮询 | 重新归零 | 日志 |
| `ClipCursor` 不一致 | 看门狗 5s | 强制释放 | 日志 |
| 幽灵 UHID 设备 | `stop()` 后校验失败 | `pkill -9 -f` + 复检 | 日志 |

## 12.2 优雅降级

```
UHID 不可写（E102）
  → 降级：INJECT_KEYCODE + INJECT_TOUCH_EVENT（绝对坐标）
  → 明确告知："当前为降级模式，平板不会有系统指针"
  → 累加器仍工作（用于边缘判定），但指针不可见
```

## 12.3 崩溃兜底（三层）

1. **进程内**：`atexit` + `signal(SIGINT/SIGTERM)` + `try/finally`
   → `Mask.release()`、`ClipCursor(None)`、`UHID_DESTROY`、`pointer_speed` 还原、`pkill server`
2. **看门狗线程**：每 5s 校验 `I2`（mask/clip/state 一致），不一致强制释放
3. **下次启动**：检测 `exit.clean` 标记缺失 → 先跑 `force_cleanup_everything()`：

```python
def force_cleanup_everything(adb: Adb | None, serial: str) -> None:
    ctypes.windll.user32.ClipCursor(None)
    if adb:
        adb.shell_ok("pkill -9 -f scrcpy-server-manual.jar")
        adb.shell_ok(f"settings put system pointer_speed {RESTORE_POINTER_SPEED}")
        adb.forward_remove(1234)
```

---

# 13. 多显示器适配细则

## 13.1 混合 DPI

| 场景 | 处理 |
|---|---|
| 主屏 150%、副屏 100% | 引擎全程物理像素；Raw Input 是物理计数 ⇒ 速度天然一致 |
| GUI 绘制 | 物理像素 × 统一缩放系数，各屏视觉比例正确 |
| `GetCursorPos` | `PER_MONITOR_AWARE_V2` 下返回物理像素 |
| 遮罩窗口 | 用物理矩形 `SetWindowPos`，不用 `MoveWindow` 逻辑值 |

## 13.2 可吸附矩阵

| 布局 | 可吸附（外轮廓） | 禁止（内缝） |
|---|---|---|
| 单屏 | 四边整段 | — |
| 左右双屏 | 左屏左/上/下、右屏右/上/下 | 左屏右、右屏左 |
| 上下双屏 | 上屏上/左/右、下屏下/左/右 | 上屏下、下屏上 |
| L 形错位 | 所有未被遮挡段 | 重叠处两边 |
| T 形三屏 | 外框全部 + 凹陷两侧 | 三条内缝 |

## 13.3 负坐标

- 副屏在主屏左侧 → `rcMonitor.left < 0`
- 遮罩包围盒用 `min(left)` / `min(top)` 计算
- 边段计算在虚拟桌面绝对坐标下进行，不假设原点在 (0,0)

## 13.4 热插拔

```
WM_DISPLAYCHANGE
  → DesktopMap.refresh()
  → 校验 Placement.segment 是否仍存在
      存在 → 保持
      不存在 → 重锚到主屏同 edge 的最长段，托盘气泡"显示器变化，已重新锚定"
  → GUI 重绘画布
```

---

# 14. 实施计划

> 每阶段都有**可执行的验收命令**。做完一个阶段再进下一个；S1 与 S4 可并行，S3 的画布与右栏可并行。

## 14.0 环境准备（0.5h）

```bash
# 1) Python 3.11+
python --version

# 2) 依赖
python -m venv .venv && .venv\Scripts\activate
pip install -r requirements.txt

# 3) adb（Android platform-tools）
#    解压到 vendor/adb/，或确保 adb 在 PATH
adb version

# 4) scrcpy-server（从 scrcpy v5.0.1 发布包中提取）
#    下载 scrcpy-win64-v5.0.1.zip，解压后把 scrcpy-server 复制为：
#    vendor/scrcpy-server-v5.0.1.jar
#    记录其 sha256 到 vendor/SHA256SUMS
certutil -hashfile vendor\scrcpy-server-v5.0.1.jar SHA256

# 5) 连接平板并授权
adb devices -l
```

**验收**：`adb devices` 能看到设备且状态为 `device`。

## 14.1 S0 骨架与预验证（0.5d）

| # | 任务 | 产出 | 验收 |
|---|---|---|---|
| S0-1 | 创建目录骨架与 `pyproject.toml`/`requirements.txt` | 可 `pip install -e .` | `python -c "import core, win, tablet"` |
| S0-2 | `tablet/adb.py`：设备枚举、shell、forward | 类 `Adb` | `python tools/probe.py` 打印设备列表 |
| S0-3 | `tools/probe.py`：读分辨率/朝向/`XScale`/`pointer_speed`/uhid 可写 | 打印事实表 | 与本文件 3.1 表完全一致 |
| S0-4 | 验证 **H1**（无加速残量） | 结论记录 | 见 3.4 |
| S0-5 | 验证 **H2**（px/count == XScale） | 结论记录 | 同上 |
| S0-6 | 验证 **H3**（边缘手势不抢鼠标） | 结论记录 | 同上 |
| S0-7 | `config.py` + 默认配置 + 校验 | 可读写 YAML | `pytest tests/test_config.py` |
| S0-8 | 单实例锁 + 日志框架 | `win/single_instance.py` | 双开第二个自动退出 |

**S0 完成标志**：`python tools/probe.py` 打出正确事实表，且 H1/H2/H3 有明确结论（写进 `docs/TROUBLESHOOTING.md`）。

## 14.2 S1 平板指针（1d）

| # | 任务 | 产出 | 验收 |
|---|---|---|---|
| S1-1 | `tablet/protocol.py`：UHID_CREATE/INPUT/DESTROY 编码 | 单测对拍 | `pytest tests/test_protocol.py` |
| S1-2 | `tablet/server.py`：推 jar、启 server、读 dummy、清理校验 | 启停幂等 | 连跑 5 次无幽灵设备 |
| S1-3 | `tablet/uhid.py`：鼠标 4 字节 / 键盘 8 字节 | 可发报告 | 平板出现指针 |
| S1-4 | `tablet/cursor.py`：估计器 | 全量单测 | `pytest tests/test_estimator.py` |
| S1-5 | `win/rawinput.py`：WM_INPUT 线程 | 打印原始计数 | `--debug-raw` 有输出 |
| S1-6 | 串联：CLI 模式 `python -m padlend --headless` 把鼠标位移 1:1 送到平板 | 无 GUI 可动 | **平板指针跟随 PC 鼠标移动** |

**S1 完成标志（US-3 的核心）**：

```bash
python -m padlend --headless --serial 192.168.68.10:46511
# 期望：平板出现系统鼠标指针；PC 上移动鼠标，平板指针同步移动
adb shell dumpsys input | grep -A2 "scrcpy"     # 可见 Classes: CURSOR
```

## 14.3 S2 边缘穿越（1d）

| # | 任务 | 验收 |
|---|---|---|
| S2-1 | `win/desktop.py`：枚举 + 外轮廓边段 | `pytest tests/test_geometry.py`（8 种布局 fixture） |
| S2-2 | `core/state.py` + `core/router.py`：状态机骨架 | 单测覆盖全部合法/非法迁移 |
| S2-3 | `win/mask.py`：遮罩 + ClipCursor + `release()` 幂等 | 手动强制异常不泄漏 |
| S2-4 | 接管：入口点计算 + `warp_to` | 进入时指针出现在对应位置 |
| S2-5 | 交还：边界+溢出判定 + 落点 | 返回位置正确、内缩 2px |
| S2-6 | 去抖与 ARMING 观察窗 | 斜划边缘不误触发 |

**S2 完成标志**：单屏右侧来回 10 次，位置正确、无误触发、无 ClipCursor 泄漏。

## 14.4 S3 GUI（2.5d）

| # | 任务 | 验收 |
|---|---|---|
| S3-1 | 令牌 + QSS + DWM 暗色/圆角 | 观感达标（截图评审） |
| S3-2 | 主窗骨架（状态条/画布/右栏/日志四区） | 可启动、可缩放 |
| S3-3 | 画布：显示器/平板/边段渲染 | 与真实布局比例一致 |
| S3-4 | 拖拽吸附 + 热区高亮 + 角落裁决 | 松手生效并落盘 |
| S3-5 | 指针幽灵（PC 光标 + 估计指针） | 实时更新 |
| S3-6 | 右栏：滑块/下拉/开关/热键编辑 | 改动即时生效 |
| S3-7 | 托盘 + 状态色 + 菜单 | 接管中变红 |
| S3-8 | 日志/指标面板 | 500ms 刷新 |

**S3 完成标志**：纯 GUI 操作可完成"连上 → 摆放 → 使用"全流程。

## 14.5 S4 键盘（1d，可与 S1 并行）

| # | 任务 | 验收 |
|---|---|---|
| S4-1 | `tablet/keymap.py`：VK→HID 全表 + 单测 | 覆盖率 100%（有映射的键） |
| S4-2 | 修饰键状态机 + 8 字节报告 | 组合键正确、无粘键 |
| S4-3 | `win/keyboard_hook.py`：钩子 + 看门狗 | 30s 稳定不掉钩子 |
| S4-4 | 屏蔽策略 + 白名单 + 保命键 | 急停可用 |
| S4-5 | 中文输入法验证 | A8 通过 |
| S4-6 | 交还清键 | 无卡键 |

**S4 完成标志**：接管期在平板用中文输入法打一段中文、Ctrl+C/V、F5 均正常。

## 14.6 S5 多显示器（1d）

| # | 任务 | 验收 |
|---|---|---|
| S5-1 | 混合 DPI 渲染与物理坐标 | A4 |
| S5-2 | 负坐标布局 | A5 |
| S5-3 | 热插拔重锚 | A6 |
| S5-4 | 跨段合并（平板跨两屏同侧） | 手动验证 |
| S5-5 | 角落裁决 UI（Shift 切换） | 手动验证 |

**S5 完成标志**：A4–A6 全通过。

> 本机只有单屏。可用 **ZakoVDD（需提权）** 或 RDP 会话构造双屏环境；若不可得，用 `fixtures/desktop_*.json` + 单元测试保证逻辑正确，并在文档中标注"A4–A6 待双屏环境复验"。

## 14.7 S6 打磨与打包（1d）

| # | 任务 | 验收 |
|---|---|---|
| S6-1 | 首次向导 | 新机器可 3 步完成配置 |
| S6-2 | 诊断页 + 诊断包导出 | 可复现问题现场 |
| S6-3 | i18n（zh_CN / en） | 切换生效 |
| S6-4 | 三层崩溃兜底 | 强杀后无残留（A11） |
| S6-5 | PyInstaller 打包（onefile 单文件） | 目标机双击可运行 |
| S6-6 | `TROUBLESHOOTING.md` / `CHANGELOG.md` | 覆盖已知限制 |

## 14.8 最短可用路径（Vertical Slice，约 2.5d）

如果只想尽快"用起来"，砍掉 GUI 与多显示器，按这个顺序：

1. S0（0.5d）→ S1（1d）→ **S2 只做单屏右侧**（0.5d）→ S4 键盘只做基本键位（0.5d）
2. 用配置文件代替 GUI：`placement.edge: right`
3. 得到一个可用的命令行版；GUI 与多屏随后补

## 14.9 工期汇总

| 阶段 | 工期 | 可并行 |
|---|---|---|
| S0 | 0.5d | — |
| S1 | 1d | 与 S4 |
| S2 | 1d | — |
| S3 | 2.5d | 画布 ∥ 右栏 |
| S4 | 1d | 与 S1 |
| S5 | 1d | — |
| S6 | 1d | — |
| **合计** | **≈8 人日** | 并行后约 **6.5d** |

---

# 15. 测试计划

## 15.1 单元（无硬件）

| 文件 | 覆盖 |
|---|---|
| `test_geometry.py` | 8 种布局（单屏/左右/上下/L/T/负坐标/跨段/角落）的边段与裁决 |
| `test_estimator.py` | 累加、夹紧、warp 收敛、残差、±127 截断、20 次来回漂移 ≤3px |
| `test_keymap.py` | VK→HID 全表、左右修饰、扩展键、6KRO 上限 |
| `test_config.py` | 校验、非法回退、版本迁移 |
| `test_protocol.py` | 报文编码（与已知样例字节对拍） |
| `test_router.py` | 状态迁移合法性；用 fake session/mask 断言调用序列 |

## 15.2 集成（模拟输入）

- `SendInput` 合成位移 → 断言状态迁移与 HID 报文序列
- Fake `TabletSession` 捕获所有报文，断言"入口 warp + 后续 1:1"
- Fake `DesktopMap`（fixture）断言入口点/落点换算

## 15.3 真机（A1–A13）

每项都要记录：环境（USB/无线、布局、分辨率）、结果、耗时、异常。

## 15.4 漂移测量（A1 的量化手段）

```bash
adb shell settings put system pointer_location 1
# 1) 从 PC 推已知计数（tools/calibrate.py --send 1000）
# 2) adb exec-out screencap -p > shot.png，读叠加层 X/Y
# 3) 断言 Δ ≈ 1000 × 1.3125（误差 ≤1%）
# 4) 来回 20 次后再测，比较估计器预测
adb shell settings put system pointer_location 0
```

## 15.5 回归清单（每次改动后跑）

```bash
pytest -q                     # 单元
python tools/probe.py         # 环境未被破坏
# 手动：A1 来回 3 次 + A8 中文一句话
```

---

# 16. 交付物与发布

## 16.1 交付物

| 交付物 | 说明 |
|---|---|
| 源码仓库 | 7.1 结构 |
| `vendor/scrcpy-server-v5.0.1.jar` | 固定版本，附 sha256 |
| 可执行文件 | PyInstaller onefile `PadLend.exe`（单文件，内置 PySide6、scrcpy jar 与 adb） |
| `README.md` | 安装/使用/常见问题 |
| `docs/TROUBLESHOOTING.md` | 故障排查（含幽灵设备清理） |
| 诊断包导出 | 脱敏后的诊断信息 zip |

## 16.2 打包

```powershell
python -m PyInstaller --noconfirm PadLend.spec
```

- 采用 `onefile`（单文件、免安装；2026-11-02 实施修正，原计划 onedir），输出
  `dist/PadLend.exe`，约 51MB。
- `PadLend.spec` 设 `runtime_tmpdir="."`：受限 Windows 上 `%TEMP%` 不可写时，
  解包目录放在 exe 同目录。
- frozen 时配置/日志/`exit.clean` 写 exe 同目录 `.padlend/`（`APP_DIR`），
  只读资源从 `_MEIPASS`（`BUNDLE_DIR`）读取。
- 内置 adb（`vendor/adb/platform-tools/`）与重打包后的 scrcpy jar，目标机无需
  预装 Python/adb；首次运行检测 `vendor/` 完整性，缺文件给出明确提示。

## 16.3 版本与迁移

- 语义化版本 `MAJOR.MINOR.PATCH`
- 配置 `version` 字段驱动迁移
- jar 升级走 9.5 流程

---

# 17. 风险登记册

| # | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | 低级钩子被静默卸载 | 中 | 键盘失效 | 回调零 I/O + 看门狗 + `RegisterHotKey` 兜底 |
| R2 | UAC 窗口收不到键鼠 | 高（固有） | 该场景不可用 | 文档明示 |
| R3 | 估计器长期漂移 | 中 | 指针错位 | 夹紧对齐 + 残差 + 速度复核 + 重同步热键 |
| R4 | `pointer_speed` 被系统重置 | 中 | 速度突变 | 10s 轮询校正 |
| R5 | MagicOS 边缘手势抢鼠标 | 中 | 贴边误触 | `boundary_inset_px` 内缩 |
| R6 | 无线 adb 抖动 | 高 | 卡顿 | 优先 USB；`report_hz` 可降 |
| R7 | 幽灵 UHID 设备残留 | 中 | 平板多一只鼠标 | 完整 stop 序列 + 启动前清理 |
| R8 | 165Hz 屏 + 1000Hz 上报的调度抖动 | 中 | 微顿 | `timeBeginPeriod(1)`；必要时 500Hz |
| R9 | PySide6 体积（~150MB） | 高 | 分发成本 | 接受；Backend 与 GUI 解耦，必要时换轻量壳 |
| R10 | 无双屏环境难以验收多屏 | 高 | A4–A6 无法真机验证 | fixture + 单测；ZakoVDD/RDP 构造；标注待复验 |
| R11 | scrcpy 协议升级 | 低 | 需适配 | 版本锁定 + 协议层隔离 |
| R12 | 复用他人协议实现的许可问题 | 中 | 合规 | 仅借鉴结构，Apache-2.0 保留声明 |

---

# 18. 附录

## A. HID 报文与描述符

### A.1 UHID_CREATE（鼠标，示例字节）

```
0x0C                      type = UHID_CREATE
0x00 0x01                 id = 1
0x1D 0x6B                 vendor = 0x1D6B
0x00 0x01                 product = 0x0001
0x0E                      nameLen = 14
"PadLend Mouse"           name
0x00 0x34                 descLen = 52（实际长度以描述符字节数为准）
<desc bytes>              HID 报告描述符
```

### A.2 鼠标报告描述符（5 键 + 相对 X/Y + 滚轮）

```
05 01 09 02 A1 01 09 01 A1 00
  05 09 19 01 29 05 15 00 25 01 95 05 75 01 81 02
  95 01 75 03 81 01
  05 01 09 30 09 31 15 81 25 7F 75 08 95 02 81 06
  09 38 15 81 25 7F 75 08 95 01 81 06
C0 C0
```

> 二期优化（穿屏瞬移 1 报文）：把 X/Y 改为 16 位 `16 00 80 26 FF 7F 75 10`，`hid_clamp` 提到 2047。

### A.3 键盘报告描述符（6KRO）

```
05 01 09 06 A1 01 05 07 19 E0 29 E7 15 00 25 01 75 01 95 08 81 02
  95 01 75 08 81 01
  95 06 75 08 15 00 25 65 05 07 19 00 29 65 81 00
C0
```

### A.4 绝对坐标设备（绘图板/RDP）归一化

```python
if flags & MOUSE_MOVE_ABSOLUTE:
    ax = int(lLastX * mon.rect.width  / 65535)
    ay = int(lLastY * mon.rect.height / 65535)
    dx, dy = ax - last_abs[0], ay - last_abs[1]
    last_abs = (ax, ay)
```

## B. VK → HID usage 规则与节选表

| 规则 | 公式/表 |
|---|---|
| 字母 `A..Z` | `usage = vk - 0x3D`（0x41→0x04 … 0x5A→0x1D） |
| 数字 `1..9` | `usage = vk - 0x13`（0x31→0x1E … 0x39→0x26） |
| 数字 `0` | `0x27` |
| 功能键 | `VK_F1..VK_F12 (0x70..0x7B)` → `0x3A..0x45` |

| VK | HID | VK | HID |
|---|---|---|---|
| `VK_RETURN` 0x0D | 0x28 | `VK_ESCAPE` 0x1B | 0x29 |
| `VK_BACK` 0x08 | 0x2A | `VK_TAB` 0x09 | 0x2B |
| `VK_SPACE` 0x20 | 0x2C | `VK_OEM_MINUS` 0xBD | 0x2D |
| `VK_OEM_PLUS` 0xBB | 0x2E | `VK_OEM_1` 0xBA (`;:`) | 0x33 |
| `VK_OEM_2` 0xBF (`/?`) | 0x38 | `VK_OEM_3` 0xC0 (`` `~ ``) | 0x35 |
| `VK_OEM_4` 0xDB (`[{`) | 0x2F | `VK_OEM_5` 0xDC (`\|`) | 0x31 |
| `VK_OEM_6` 0xDD (`]}`) | 0x30 | `VK_OEM_7` 0xDE (`'"`) | 0x34 |
| `VK_LEFT` 0x25 | 0x50 | `VK_RIGHT` 0x27 | 0x4F |
| `VK_UP` 0x26 | 0x52 | `VK_DOWN` 0x28 | 0x51 |
| `VK_HOME` 0x24 | 0x4A | `VK_END` 0x23 | 0x4D |
| `VK_PRIOR` 0x21 | 0x4B | `VK_NEXT` 0x22 | 0x4E |
| `VK_INSERT` 0x2D | 0x49 | `VK_DELETE` 0x2E | 0x4C |
| `VK_CAPITAL` 0x14 | 0x39 | `VK_APPS` 0x5D | 0x65 |
| `VK_LWIN` 0x5B | mod 0x08 | `VK_RWIN` 0x5C | mod 0x80 |
| `VK_LCONTROL` 0xA2 | mod 0x01 | `VK_RCONTROL` 0xA3 | mod 0x10 |
| `VK_LSHIFT` 0xA0 | mod 0x02 | `VK_RSHIFT` 0xA1 | mod 0x20 |
| `VK_LMENU` 0xA4 | mod 0x04 | `VK_RMENU` 0xA5 | mod 0x40 |

> 完整表在 `tablet/keymap.py`，单测要求：表内每个 VK 单向映射唯一、无遗漏。

## C. 诊断命令

### C.1 设备与连接

```bash
adb devices -l
adb shell getprop ro.product.model
adb shell wm size
adb shell wm density
```

### C.2 关键参数（probe.py 实现依据）

```bash
adb shell settings get system pointer_speed
adb shell dumpsys input | grep -E "Pointer Speed:|XScale:|YScale:"
adb shell id | tr ',' '\n' | grep uhid          # 必须在组里
adb shell 'exec 3<>/dev/uhid && echo OPEN_OK'
```

### C.3 UHID 设备是否注册

```bash
adb shell dumpsys input | grep -B1 -A3 "Classes: CURSOR"
```

### C.4 清理幽灵设备

```bash
adb shell pkill -9 -f scrcpy-server-manual.jar
adb forward --remove tcp:1234
adb shell settings put system pointer_speed -3     # 还原出厂值
adb shell dumpsys input | grep -c "scrcpy"         # 期望 0
```

### C.5 坐标叠加层（标定用）

```bash
adb shell settings put system pointer_location 1
# … 观察/截图 …
adb shell settings put system pointer_location 0
```

### C.6 观察 HID 事件

```bash
adb shell getevent -lt /dev/input/event<N>
```

## D. 环境坑（实测，必看）

| 坑 | 现象 | 处理 |
|---|---|---|
| Git Bash 路径转换 | `adb shell cat /sdcard/x` 被改写 | `export MSYS_NO_PATHCONV=1` |
| Python 中文输出 | `UnicodeEncodeError` | `PYTHONIOENCODING=utf-8` 或统一用 logging |
| SDL/Qt 窗口查找 | `FindWindowW` 找不到 | 用 `EnumWindows` 枚举类名 |
| `SetForegroundWindow` 被拒 | 无法抢焦点 | `keybd_event(VK_MENU)` 解锁技巧（仅调试用） |
| `ps \| grep scrcpy` 抓不到 server | 进程名是 `app_process` | 用 `pkill -f <jar 名>` |

## E. 与既有方案对比

| 维度 | InputShare | 上一版 kvm | **PadLend** |
|---|---|---|---|
| 平板系统指针 | ✅ | ❌ | ✅ |
| 位置感知 | ❌ 盲推 | ❌ | ✅ 估计器 |
| 自动穿回 | ❌ 需按键 | ❌ | ✅ |
| 多显示器 | ❌ 粗略 | ❌ 四边 | ✅ 外轮廓边段 |
| 混合 DPI | ❌ | ❌ | ✅ |
| 中文输入 | ✅ | ❌ | ✅ |
| 上报频率 | 125Hz（丢位移） | 事件驱动 | 1000Hz（残差补齐） |
| 崩溃兜底 | ❌ | ❌ | ✅ 三层 |
| GUI | 简单设置 | 无 | ✅ 可拖拽画布 |
| 需要驱动/提权/root | 否/否/否 | 否/否/否 | **否/否/否** |

## F. 术语表

| 术语 | 定义 |
|---|---|
| **UHID** | Android 用户空间 HID 子系统，写 `/dev/uhid` 即注册虚拟输入设备 |
| **相对设备** | 只上报位移、不持有绝对坐标（鼠标）；位置由系统累加并夹紧 |
| **估计器** | PC 端镜像的"平板指针当前位置"（平板像素，浮点），唯一位置真相源 |
| **外轮廓边段** | 虚拟桌面外框上未被其它显示器遮挡的一段边；唯一可吸附对象 |
| **接管 / 交还** | 输入转发到平板 / 从平板返回 |
| **Gain** | PC 像素位移 → HID 计数的换算系数 |
| **headless** | `video=false control=true`，只开控制通道 |
| **KRO** | Key Rollover，键盘同时按键上限（本项目 6KRO） |

---

# 19. 参考与许可

| 项目 | 用途 | 许可 |
|---|---|---|
| [scrcpy](https://github.com/Genymobile/scrcpy) | `scrcpy-server` jar、UHID 协议与描述符参考 | Apache-2.0 |
| [InputShare](https://github.com/InputShare/InputShare) | 控制协议实现结构参考（不复制 UI 代码） | Apache-2.0 |
| Android `dumpsys input` / HID 规范 | 诊断与描述符 | — |

**致谢**：scrcpy 作者 Romain Vimont；InputShare 作者 BHznJNs。

**许可建议**：本项目采用 Apache-2.0，与所依赖/参考项目一致，便于合规复用。

---

## 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| v1.0 | 2026-11-02 | 定名 PadLend；补全实施计划（第 14 章）、工程规范（第 7 章）、接口契约与错误码（第 8 章）、测试与回归（第 15 章）；删除对旧工作区路径的依赖，改为自举式环境准备 |

**文档结束** — 可直接作为实施依据。
