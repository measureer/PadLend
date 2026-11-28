# 布局缩放与「画布即事实」的穿越几何（设计 v1.0）

> 状态：**已实现**（2.5.0，2026-11-29）
> 日期：2026-11-28 起草，2026-11-29 落地
> 目标版本：2.5.0（2.4.0 已发；本次删配置字段 + 改几何，属破坏性变更，走 minor）
> 关联：docs/DESIGN.md §6.3、docs/PITFALLS.md §31.12
> 一句话：**删掉「对齐方式」，给平板一个画布缩放系数，穿越范围完全由画布上画出来的矩形决定。**

---

## 实现记录（2.5.0）

已按本提案落地 P0 + P1（P2 的触发带高亮未做，留待后续）：

| 提案 | 实际 |
|---|---|
| 删 Align | 枚举、`placement.align`、序列化、config 校验/修复/默认值、前端类型与校验、右键三项、夹具全部清掉 |
| 加 scale | `Placement.scale` / `PlacementCfg.scale` / `ScaleOutOfRange` / `clampScale`；0.1–4.0，默认 1.0；`placement_field_kind` 放行 `scale` |
| 足迹 | `geometry::footprint_size` + `tablet_footprint`；router 四处几何入口统一走它，取整只做一次 |
| 入口/出口 | `tablet_entry_point` 除以 `s`、`tablet_exit_point` 乘 `s`（`s = 足迹/设备`）；`scale = 1` 与 2.4.0 逐像素一致 |
| 重锚定 | 保留旧 `scale`，新摆位用 `SCALE_DEFAULT` |
| 横竖屏同步 | 保存时 `LayoutsCfg::set_scale` 只改另一方向**已有**存档的 scale，不动它的边与位置；没有存档就不创建（旋转时走全局摆位，scale 已同值） |
| 画布 | 右键「缩放」预设 50/75/100/125/150% + 「自定义…」；只进预览，点「保存布局」才写盘；不做拖拽手柄 |
| 「测试穿越」 | 只删画布右键那一行；`actions.toggleManual()` / 托盘 / 主界面入口保留 |

与提案的偏差：

1. **足迹最小 1 px，不是 16 px**：`footprint_size` 只保证 ≥1。0.1 下限已经保证 3200×2136 设备的最小足迹是 320×214，不会退化到没法点；16 px 门槛反而要再造一套"低于门槛就提示"的 UI，本期不做。
2. **repair 文案是 `[0.1,4.0]`**（用 `py_float_repr` 格式化），不是提案里的 `[0.1,4]`。
3. **自定义输入只认回车**：失焦不提交（避免把输了一半的 "37." 当成 37 写进预览）；Escape 或点画布即放弃。
4. **夹具是机械改写**：`config_cases.json` 里合法 align → `scale: 1.0`；`stretch` → `scale: 9.0`（期望 4.0）；`bad_types` 的 `align: ""` → `scale: -1`（期望 0.1）；三条 repair 文案同步改成 scale 版本。
5. **P2 未做**：触发带高亮（把足迹 ∩ 外轮廓段画出来）不在本期范围。
6. **旧配置里的 align 不报错**：`PlacementCfg` 没有 `deny_unknown_fields`，旧的 `align: stretch` 会被忽略，`scale` 缺省 1.0；只有显式写坏的 `scale` 才进 repair 流程。

---

## 0. 一页纸

| 项 | 结论 |
|---|---|
| 对齐方式（等比 / 居中 / 1:1） | **删除**。Align 枚举、placement.align 字段、右键菜单三项、前端类型与校验、夹具一起清掉 |
| 新增 | scale：平板在**桌面像素**里的足迹 = 设备像素 × scale，范围 0.1 .. 4.0，默认 1.0 |
| 穿越几何 | 触发带 = 足迹 ∩ 外轮廓边段；入口 / 出口在足迹上按比例映射，不再假设 1 设备像素 = 1 桌面像素 |
| 画布交互 | 右键平板 →「缩放」选预设百分比或用「自定义…」输入数字；改动只进预览，「保存布局」才生效 |
| 不变 | 平板仍必须贴在**某条外轮廓边**上；offset（贴在段的哪个位置）、corner_priority（角落裁决）、指针速度 / gain 全部不变 |
| 风险点 | 入口 / 出口忘了同乘同一个比例；前端与引擎两份几何不一致；scale 被配置补丁白名单静默吞掉；夹具 30+ 处 align 漏改 |

---

## 1. 问题：画布上的平板比显示器还大

现状（2.4.0）：画布把**设备像素**直接当成**桌面像素**摆平板——tablet_rect() 用 estimator.width()/height()（如 3200×2136）当矩形边长，tablet_entry_point() 也按 1:1 像素做映射。

以「13 寸 3:2 平板（3200×2136，约 281×187 mm）竖放在 27 寸 1440p 显示器（2560×1440，约 597×336 mm）右侧」为例：

| | 分辨率 | 物理（估） | 竖放时贴边长度 |
|---|---|---|---|
| 显示器 | 2560×1440 | 597×336 mm | 1440 px / 336 mm |
| 平板 | 3200×2136 | 281×187 mm | 3200 px / 281 mm |

- 画布上：平板 3200 px vs 显示器侧边 1440 px → 平板是显示器的 **2.22 倍**；
- 物理上：281 mm vs 336 mm → 平板只有显示器的 **0.84 倍**；
- 该缩小多少：scale = (281/3200) / (336/1440) = 0.0878 / 0.2333 ≈ 0.376。

后果不只是「看着别扭」：触发带（segment_contacts_tablet）判定的是「光标在不在平板矩形内」，所以屏幕上那 3200 px 全都能穿进去——而用户以为只有 281 mm 那么一小段。**画布骗了眼睛，也改了手感。**

同时，「对齐方式」三项只是配置字段：Align 只在序列化、校验、默认值里出现，几何函数从头到尾没读过它（见 §4.2）。菜单摆着三个选项却什么都不改，本身就是误导。

---

## 2. 目标 / 非目标

### 目标

1. 删除「对齐方式」及其配置字段，几何里不再有模式分支——**只有一条路径**。
2. 平板在布局画布上可等比缩放，让「画布上的比例」≈「物理比例」。
3. 穿越的一切（能不能穿、从哪一点进、从哪一点出、遮挡条大小）都由缩放后的矩形推导。
4. 老配置能直接加载，不需要手工改文件。

### 非目标

1. **不改指针速度/加速度**（motion.gain、XScale、pointer_speed）。物理速度一致是另一个问题，见附录 A。
2. **不做自由 2D 摆放**：平板仍要吸附在某条外轮廓边段上；不能漂在屏幕中间，也不能同时压两条边。
3. 不改外轮廓边段算法、角落裁决规则、多屏合并段规则。
4. 不改设备事实（facts.width/height 仍是设备像素）。
5. **不做「按物理尺寸自动估算 scale」**（Q2 已定）：EDID / 安卓物理 dpi 都不可靠（附录 B），本期只做手动调。

---

## 3. 术语

| 词 | 含义 |
|---|---|
| 设备像素 device px | 平板自己的 3200×2136，来自 facts.width/height |
| 桌面像素 desktop px | 虚拟桌面坐标（Windows 物理像素，跨屏可为负） |
| 足迹 footprint | 平板在桌面坐标里占的矩形边长 = round(device_px × scale) |
| 边段 segment | 显示器外轮廓上一段可吸附的 1px 线（EdgeSegment） |
| 触发带 band | 足迹 ∩ 边段 ∩ 该边 1px 扫描线——鼠标真正能穿进平板的那一段 |
| scale | 足迹 / 设备像素。1.0 = 现在的行为；<1 表示「平板在桌面上占的地方比它自己的像素数小」 |

---

## 4. 现状与改造点

### 4.1 数据链路

    config.yaml                    RPC / DTO                   运行时
    placement.align            →  placement_dict.align      →  Placement.align
    layouts.devices.<id>.<o>.align                          （谁都不读，只在保存时原样写回）

几何链路（画布与引擎共用同一套公式）：

    Placement {segment, offset, scale}
        └─ tablet_rect(segment, monitor, offset, w, h)   # 当前 w/h = 设备像素
             └─ TabletRect {left, top, right, bottom}
                  ├─ segment_contacts_tablet()   # 能不能进
                  ├─ tablet_entry_point()        # 从哪一点进（当前 1 设备像素 ↔ 1 桌面像素）
                  ├─ tablet_exit_point()         # 从哪一点出
                  └─ Router::clip_rect() / mask  # 接管期间的遮挡条

关键位置：

- padlend-core/src/logic/geometry.rs:646 tablet_rect()
- padlend-core/src/logic/geometry.rs:719 tablet_entry_point()
- padlend-core/src/logic/geometry.rs:759 tablet_exit_point()
- padlend-core/src/logic/geometry.rs:687 segment_contacts_tablet()
- padlend-core/src/logic/router.rs:2291 / 2371 / 2799 / 3105 四处 tablet_rect() 调用
- src/lib/geometry.ts:56 前端 tabletRect()

### 4.2 align 是死字段（证据）

| 位置 | 用途 |
|---|---|
| padlend-core/src/logic/geometry.rs:149-203 | 枚举定义、序列化、Default |
| padlend-core/src/logic/geometry.rs:416-529 | Placement.align 字段与 PlacementRaw |
| padlend-core/src/config.rs:246 / 1382 / 1523 / 1573 | 字段、校验、补丁树校验、修复 |
| padlend-core/src/service/serializers.rs:62 / 80 / 313 | DTO、反序列化、补丁字段白名单 |
| padlend-core/src/service/engine.rs:1314 / 1338 | 配置 → Placement、回写配置 |
| padlend-core/src/logic/router.rs:3733 | 重锚定时**原样保留**（唯一"业务"用途） |
| padlend-core/src/win/desktop.rs:303 | 真正的等比映射 pc_to_tablet_along()，**无调用方**，只剩测试 |
| src/features/layout/DesktopCanvas.tsx:351 | 菜单三项 |

几何函数（tablet_rect / entry / exit）没有任何一处读 align，所以三项在运行时**完全等价**。

---

## 5. 方案

### 5.1 原则：画布即事实

> 画布上画出来的平板矩形，就是穿越时唯一的事实源。
> 代码里不允许存在第二个能改变"穿越范围"的开关。

推论：
- 删除一切「对齐/模式」选项；
- 任何影响占位的参数（offset、scale、设备分辨率）都必须同时反映到画布与触发带；
- 预览与引擎必须用同一份公式（现状：geometry.ts ↔ geometry.rs 双份实现，保持）。

### 5.2 删除 align

改动清单见 §7。要点：

1. Align 枚举整体删除；Placement::new(segment, offset, align, corner) 的第三个参数**换成 scale**（保持参数个数，调用点改动最小）。
2. PlacementCfg.align: String → scale: f64；ALIGNS 常量、placement_problems / placement_tree_problems / repair_placement 里对应的三段换成 scale 区间校验。
3. 补丁白名单 placement_field_kind() 里把 align 换成 scale（**这一步漏了，前端改 scale 会被静默丢弃**，见 §8.3）。
4. 右键菜单三项删除。
5. 老配置里的 align: 由 serde 忽略未知键自动吞掉，下次保存自然消失；**不做显式迁移、不写升级提示**（删掉的是无效字段，没有行为损失）。

### 5.3 新增 scale

**存储在摆位里**（与 align 同一位置，语义也最接近：都是"平板怎么摆"）：

    placement:                 # 全局回退
      monitor: ''
      edge: right
      span_start: 0
      span_end: 1440
      offset: 0.5
      scale: 0.375             # 新增：足迹 = 设备像素 × 0.375
      corner_priority: right
    layouts:
      devices:
        'HW-SERIAL':
          landscape: { edge: right, span_start: 0, span_end: 1440, offset: 0.5, scale: 0.375, corner_priority: right }
          portrait:  { edge: left,  span_start: 0, span_end: 1440, offset: 0.5, scale: 0.375, corner_priority: left }

- 范围 0.1 .. 4.0，默认 1.0；非有限或越界 → 修复为最近边界并记 issue（沿用 repair_placement 的口径）。
- 为什么放在摆位里而不是设备属性：足迹取决于**这台平板配哪块显示器**（不同显示器 px/mm 不同），同一个设备接两块屏可能需要不同 scale。
- 为什么不是"每方向一份独立值"：屏幕像素是各向同性的，物理比例与横竖无关，理论上两个方向共用一个 scale。**但**布局档本来就是按方向存的，所以物理上会有两份；保存时两档都写同一个值，用户不必分别调（见 Q1）。

### 5.4 几何公式

设设备沿边长度 D（left/right 边取设备高，top/bottom 边取设备宽），足迹沿边长度 F = round(D × scale)，映射比 s = F / D（注意：**用 round 之后的 F 反推，不要再直接用 scale**，否则往返不互逆）。

    rect = tablet_rect(segment, monitor, offset, footprint_w, footprint_h)   # 现有函数，改的就是传进去的尺寸
    入口（PC → 平板）:  t = clamp((pc_along - rect.top) / s, 0, D-1)
    出口（平板 → PC）:  pc_along = round(rect.top + t * s)                    # round-half-even，沿用 py_round_int
    触发带:            [max(segment.span_start, rect.top), min(segment.span_end, rect.bottom))

- scale = 1 时 s = 1，公式退化成今天的 clamp(pc_along - rect.top, ...)——**逐位等价**，既有 golden 测试不用改数值。
- 入口/出口是同一仿射映射的逆，保证「进去再从同一位置出来」落在同一 PC 像素（±1 舍入）。
- 垂直方向的"贴边那一侧"（left 边取 max_x、right 边取 0）不变。

伪代码（Rust，签名不变，只在函数体内引入比例）：

    pub fn tablet_entry_point(segment, tablet, pc_x, pc_y, tablet_w, tablet_h) -> (f64, f64) {
        let max_x = (tablet_w - 1) as f64;
        let max_y = (tablet_h - 1) as f64;
        match segment.edge {
            Edge::Left => {
                let s = tablet.height() / tablet_h as f64;      // 足迹 / 设备
                (max_x, clamp((pc_y - tablet.top) / s, 0.0, max_y))
            }
            Edge::Right => { /* 同上，x 取 0 */ }
            Edge::Top => {
                let s = tablet.width() / tablet_w as f64;
                (clamp((pc_x - tablet.left) / s, 0.0, max_x), max_y)
            }
            Edge::Bottom => { /* 同上，y 取 0 */ }
        }
    }

    pub fn tablet_exit_point(segment, monitor, tablet, tablet_along, offset_px) -> (i32, i32) {
        // 现有逻辑不变，只把两处
        //   y = tablet.top + tablet_along
        //   x = tablet.left + tablet_along
        // 换成
        //   y = tablet.top  + tablet_along * s
        //   x = tablet.left + tablet_along * s
    }

前端同构：tabletRect(placement, monitor, deviceW, deviceH) 内部先算 footprint = round(device × scale) 再摆矩形；其余函数不动。

### 5.5 画布交互

| 操作 | 行为 |
|---|---|
| 拖动平板本体 | 与现在一致：吸附最近边段、offset 跟着指针走（不改 scale） |
| 右键菜单 | 删除「对齐方式」三项与「测试穿越」；新增「缩放 ▸」：50% / 75% / 100% / 125% / 150% /「自定义…」 |
| 双击平板 | 保留：临时切换横竖预览（不改 scale，足迹随设备宽高交换重新计算） |
| 滚轮 | 仍是画布缩放；不改平板 scale（避免误触，见 §8.2） |
| 状态 | 改 scale = 布局脏（layoutDirty），和拖动共用一条「预览 → 保存布局」链路 |

**不提供拖拽缩放手柄**（Q3 已定）：连续微调的需求靠预设百分比就够，精确值靠「自定义…」输入框；
少一个可交互元素，也就少掉"手柄与拖动抢事件、手柄随画布缩放变大变小、手柄命中区太小"这一整类坑。

缩放菜单细节：
- 位置：右键平板（与「移除」同一个菜单），新开一组「缩放 ▸」子菜单。
- 预设：50% / 75% / 100% / 125% / 150%，当前值用 ● 标记。
- 自定义：输入百分数（如 37.6），夹紧到 10%–400%，保留一位小数；回车与失焦提交，Esc 关闭。
- 缩放只改 scale，不动 offset（平板在边段上的位置不变），所以缩小时不会从边上滑走。

#### 5.5.1 缩放菜单长什么样

    ┌ 右键菜单 ──────────┐        ┌ 缩放 ──────────────┐
    │ 缩放            ▸ │  ───►  │ ● 100%             │
    │ 移除               │        │   75%              │
    └────────────────────┘        │   50%              │
                                  │   125%             │
                                  │   150%             │
                                  │   自定义…           │
                                  └────────────────────┘

- 「自定义…」输入 37.6，直接落成 scale = 0.376——正是 §1 那个算例，不靠眼估。
- 缩放只进预览，点「保存布局」才写盘（与拖动平板一致）。

### 5.6 保存与生效

现状链路保持不变：

    拖动/缩放 → 前端预览（store，dirty=true）→ 「保存布局」→ RPC layout.save → 引擎写 config + router.set_placement → 立即生效

- layout.save 目前会**同时**把摆位写进当前方向的布局档和全局 placement（padlend-core/src/service/api.rs:604）。因此 scale 天然有"回退值"：换方向没有专档时，回退到全局摆位会带着上次保存的 scale。这一点要写进文档（§8.3 第 23 条）。
- 「测试穿越」按钮**已删**（Q4）：它只是 actions.toggleManual() 的另一个入口，底层与主界面「手动接管 / 交还」（src/features/shell/Shell.tsx:46）、托盘 toggle（src/store/events.ts:15）是同一个 RPC。删入口、留功能。

---

## 6. 详细设计

### 6.1 数据结构

    // padlend-core/src/logic/geometry.rs
    pub struct Placement {
        pub segment: EdgeSegment,
        pub offset: f64,          // 0..1
        pub scale: f64,           // 0.1..4.0，默认 1.0
        pub corner_priority: Option<Edge>,
    }

    impl Placement {
        pub fn new(segment, offset, scale, corner_priority) -> Result<Self, GeometryError>;
        pub fn validate(&self) -> Result<(), GeometryError>;   // offset 区间 + scale 区间
    }

    // padlend-core/src/config.rs
    pub struct PlacementCfg {
        pub monitor: String,
        pub edge: String,
        pub span_start: i64,
        pub span_end: i64,
        pub offset: f64,
        pub scale: f64,           // 新增，默认 1.0
        pub corner_priority: String,
    }

DTO（padlend-core/src/service/serializers.rs）：

    {
      "monitorHandle": 1,
      "edge": "right",
      "spanStart": 0,
      "spanEnd": 1440,
      "length": 1440,
      "offset": 0.5,
      "scale": 0.375,
      "cornerPriority": "right"
    }

- 旧引擎不认 scale → 掉回 1.0；新引擎不认 align → 忽略。两个方向都不崩。
- scale 缺失时按 1.0（serde default = default_scale）。

### 6.2 引擎改动

1. **单一足迹入口**：在 padlend-core/src/logic/router.rs 加一个私有辅助，四个调用点全走它：

    fn footprint_rect(&self, placement: &Placement, anchor: &MonitorInfo, w: i32, h: i32) -> TabletRect {
        let fw = round_i32(w as f64 * placement.scale);
        let fh = round_i32(h as f64 * placement.scale);
        tablet_rect(&placement.segment, anchor, placement.offset, fw.max(1), fh.max(1))
    }

2. tablet_entry_point / tablet_exit_point 内部按 §5.4 引入 s（签名不变，tablet_w/h 仍传设备像素）。
3. placement_from_config()（padlend-core/src/service/engine.rs:1314）读 scale 而不是 align；apply_placement_to_config() 写回 scale。
4. reanchor_placement()（padlend-core/src/logic/router.rs:3690）：保留旧 scale（与今天保留 align 同理）；日志里打印 scale。
5. api.rs::layout_save 的日志补 scale=…，方便排查"为什么触发带这么短"。
6. Placement::new 的校验加 scale 区间；geometry.rs 的 golden 测试补 scale≠1 的用例。

### 6.3 前端改动

| 位置 | 改动 |
|---|---|
| src/api/types.ts | 删 Align；Placement.scale: number；Segment.align? 删除 |
| src/lib/geometry.ts | tabletRect() 先算 footprint；新增 footprintSize(placement, deviceW, deviceH) |
| src/lib/configSchema.ts:265 | align 校验 → scale 必须是有限数且在 0.1..4.0 |
| src/features/layout/DesktopCanvas.tsx | 右键菜单删对齐三项与「测试穿越」（:351 / :367）、加「缩放」子菜单（预设 + 自定义输入）；computeSnap() 保留原 scale |
| src/features/layout/LayoutPage.tsx | :70 提示文案（现在写着「右键调整对齐方式或测试穿越」） |
| src/api/mock.ts:154 | 默认 config 里 align → scale |
| src/tests/* | 构造 Placement 的地方全部改字段 |

### 6.4 边界与错误处理

| 情况 | 处理 |
|---|---|
| scale 非法（NaN/Inf/<0.1/>4.0） | repair_placement 夹紧并记 issue："<prefix>.scale 越界，已夹紧到 [0.1,4]" |
| footprint < 16 px | 说明 scale 已经小到没法用；仍按 16 算并提示（避免 0 高度矩形让触发带消失） |
| 足迹比段长 | 允许（现状也允许）；触发带 = 交集；offset 仍夹在 0..1 |
| 足迹跨出显示器段两端 | 允许；出口落点按现有规则夹回段内 |
| 设备未连接 | 画布用默认 3200×2136，scale 照常可调；保存时按 identity 记录 |
| 旋屏 | 足迹重算（宽高交换 × 同一个 scale），fit 重算视野 |

---

## 7. 影响面清单

| 文件 | 改动 |
|---|---|
| padlend-core/src/logic/geometry.rs | 删 Align；Placement 用 scale 替 align；entry/exit 引入 s；测试与 golden 更新 |
| padlend-core/src/logic/router.rs | 4 处 tablet_rect 改走 footprint_rect；reanchor 保留 scale；日志 |
| padlend-core/src/config.rs | PlacementCfg.scale；ALIGNS 删除；validate / tree 校验 / repair / default |
| padlend-core/src/service/serializers.rs | placement_dict / placement_from_dict；placement_field_kind 白名单 |
| padlend-core/src/service/engine.rs | placement_from_config / apply_placement_to_config |
| padlend-core/src/service/api.rs | layout_save 日志（可选） |
| padlend-core/src/win/desktop.rs | 删 Align::Center 测试夹具（802-809）；pc_to_tablet_along / tablet_to_pc_along 死代码按 Q5 处理 |
| padlend-core/tests/fixtures/config_cases.json | 30+ 处 align 删除、3 条 repair 文案改用例、expected 重算 |
| padlend-core/tests/fixtures/config_default_saved.yaml、config_mutated_saved.yaml | 同上 |
| padlend-core/tests/fixtures/router_scenarios.json | 若含放置字段则同步 |
| src/api/types.ts、src/api/mock.ts、src/lib/configSchema.ts、src/lib/geometry.ts | 见 §6.3 |
| src/features/layout/DesktopCanvas.tsx | 删对齐三项与「测试穿越」菜单项（:351 / :367）；加「缩放」子菜单（预设 + 自定义输入） |
| src/features/layout/LayoutPage.tsx | :70 提示文案 |
| src/lib/actions.ts | toggleManual() **保留**（Shell.tsx:46、store/events.ts:15 仍在用），只删画布入口 |
| src/tests/geometry.test.ts、store.test.ts、actions.test.ts | 构造与断言 |
| README.md:113、docs/DESIGN.md §6.3.4/6.3.5、docs/PITFALLS.md §31.12、docs/CHANGELOG.md | 文档；历史段落不改写，只加"2.5.0 起删除 align"的注记 |

---

## 8. 坑（按踩到的顺序）

### 8.1 几何与映射

1. **入口/出口比例不一致**：只改入口除以 s、出口没乘 s，表现是「进得去，出来时指针跳到别处」，而且越偏离中心越明显。两者必须是同一仿射变换的逆，并有一条 entry(exit(t)) ≈ t 的往返单测（±1 px）。
2. **s 用 scale 还是 footprint/device**：footprint 是 round(device × scale)，两者不等（如 3200×0.373=1193.6→1194，s=0.373125）。必须用后者，否则每穿一次漂移 1 px，越穿越歪。
3. **沿边维度取错**：left/right 边用设备**高**，top/bottom 边用设备**宽**。写反在横屏时不明显、竖屏时落点被压扁。
4. **tablet_entry_point 的 tablet_w/h 参数被顺手改成足迹**：函数内部用它算 max_x/max_y（设备坐标上限），改成足迹会把落点截到半个屏幕。**这两个参数永远是设备像素**。
5. **tablet_rect 的既有测试**：12 处测试调用仍在传设备像素，语义应保持"传进来的就是足迹"。scale=1 时输出必须与 2.4.0 逐位相同——拿现有 golden 数值当回归基线。
6. **footprint 为 0 或负**：scale 极小 / NaN 漏进几何 → 高度 0 的矩形 → segment_contacts_tablet 永远 false，"平板突然穿不进去了"。repair + 构造处 .max(1) 双保险，另设 16 px 下限。
7. **足迹小于触发器阈值**：motion.arm_px=10 要求沿法线推出 10 px，与沿边长度无关，但足迹太短（<16 px）时用户几乎点不中。下限同上，并在画布上给提示。
8. **合并虚拟段**：平板跨两块同侧显示器时，segments_at() 返回多条，segment_contacts_tablet 逐段判断——足迹正确就自动正确，但要补一条"足迹横跨两段"的用例，防止有人把 contacts 改成只取第一条。
9. **遮挡条 / clip_rect**：它用 tablet.top/bottom 裁 1px 条带，足迹缩小后条带自动变短，这是**期望行为**（遮住的就是平板所在的位置）。不要为了"看起来覆盖整条边"把它改回整段——那会在平板范围外吞掉光标。
10. **沿边方向"1 桌面 px ≈ 1/s 设备 px"**：缩放后用户沿边滑动时，平板内指针相对屏幕移动更快，会在足迹内更早碰到平板自身的边而交还。这是比例映射的正常结果（画布即事实），但真机上要专门验一次手感；必要时调 exit_overflow_px 而不是改几何。

### 8.2 画布交互

11. **缩放不重算视野**：fitView 在 sizeNonce 变化时会跑（src/features/layout/DesktopCanvas.tsx:109）。若 scale 也触发它，点一次预设画面就跳一次。只在「显示全部」和**设备尺寸**变化时 fit。
12. **小足迹下的标签与命中区**：canvas-tablet-label / -meta 按 size.width/2 定位，footprint 掉到两三百像素时文字压出边框；足迹更小时右键都点不中。按足迹宽高隐藏 meta 行、字号随足迹收缩（下限 24 场景单位），并给平板本体一个最小命中矩形。
13. **右键菜单的坐标夹紧**：现在是硬编码 Math.min(ctx.x, 520) / Math.min(ctx.y, 300)（src/features/layout/DesktopCanvas.tsx:349），加一行「缩放 ▸」和它的子菜单后容易顶出窗口。改成按容器实际宽高夹紧，子菜单朝有空间的一侧展开。
14. **自定义输入的非法值**：空串、负号、0、9、abc、9999 都要处理——夹紧到 10%–400%，非法则回退原值且不置脏；回车与失焦都提交，但失焦时不要把输了一半的「37.」当成 37 提交。
15. **保存语义**：缩放必须走「预览 → 保存布局」，layoutDirty 必须亮（setPlacement(next, true)）。漏了就是"画布上变了、重启回原样"。
16. **预览与事实同源**：缩放后要立刻重算矩形（以及 P2 的触发带高亮），且高亮必须用与引擎相同的 footprint 公式；只改 SVG 不改 store 的话，保存时会把旧值写回去。
17. **滚轮不要复用**：滚轮仍是画布缩放；不给平板 scale 加滚轮/修饰键手势，否则用户分不清在缩谁（§5.5.1 已说明）。
18. **高 DPI 与 client 坐标**：ctx 记的是 e.clientX/clientY，画布容器有缩放时要在夹紧前换算到容器坐标系，否则缩放视图后菜单会偏。
19. **删「测试穿越」时别把功能一起删**：它只是 actions.toggleManual() 的一个入口，底层 router.toggle_manual（padlend-core/src/logic/router.rs:1940）同时被主界面「手动接管 / 交还」（src/features/shell/Shell.tsx:46）和托盘 toggle（src/store/events.ts:15）调用。**只删画布右键那一行**（src/features/layout/DesktopCanvas.tsx:367），RPC、action、托盘菜单都别动，否则手动接管就没了；再顺手改 src/features/layout/LayoutPage.tsx:70 的文案。

### 8.3 配置与迁移

20. **补丁白名单吞字段**：padlend-core/src/service/serializers.rs:308-316 的 placement_field_kind() 只放行已知字段，漏加 scale → 前端改 scale 通过校验、写盘时被丢掉，**而且不报错**（P1-23 那类坑）。三处一起改：PlacementCfg、placement_field_kind、placement_tree_problems。
21. **默认值散落**（PITFALLS §31.12 的教训）：PlacementCfg::default、PlacementRaw 的 serde 默认、repair_placement 回退、placement_from_dict 回退、mock.ts 的默认 config——五处都要是 1.0，缺一处就会出现"新配置是 1.0、老配置却变 1.0 之外的值"。
22. **夹具 30+ 处**：config_cases.json 里有 30 多处 align，其中 3 条是 repair 文案断言（如 "placement.align='stretch' 非法，已回退 one_to_one"）。删字段后这些用例要么改写成 scale 越界文案，要么删除；只改一处会让整个 cargo test 红一片，且分不清是真 bug 还是夹具没改完。
23. **layout.save 顺带写全局 placement**（padlend-core/src/service/api.rs:604）：这是现状，正好让 scale 有回退值；但也会让"全局摆位"跟着最后一次保存变。文档要写清，否则以后排查"全局摆位怎么自己变了"会重走一遍。
24. **换方向没有专档**：回退 config.placement（带 scale）。apply_layout_for_facts()（padlend-core/src/service/engine.rs:1390）这条分支要加单测，防止回退时 scale 掉回 1.0。
25. **重锚定保留 scale 可能不对**：显示器拓扑变了、平板被重锚到另一块屏，px/mm 不同，旧 scale 不一定合适。第一阶段：保留并打日志（与今天保留 align 一致）；第二阶段：若拿到物理尺寸就重算（附录 B）。
26. **旧配置里的非法 align**：以前会进 repair 输出 "placement.align='x' 非法…"，删字段后这些 issue 消失——如果有人依赖日志排查，会在 CHANGELOG 里说明。

### 8.4 兼容与测试

27. **版本错配（开发期最常见）**：tools/build-core.ps1 产出的 exe 与前端必须同版本。新前端 + 旧引擎 → scale 被忽略、画布能缩但穿越还是老的（症状："画布小了，屏幕上还能整条边穿过去"）。发版说明里写清；npm run dev 前先重建引擎。
28. **旧前端 + 新引擎**：placement_from_dict 忽略未知的 align，scale 缺省 1.0，行为退化为 2.4.0。可接受。
29. **scale=1 的逐位回归**：golden trace / router_scenarios.json 的期望值不能因为这次重构发生任何变化（否则说明改动碰到了不该碰的路径）。
30. **真机验收要成对做**：同一台机器上分别用 scale=1 和 scale≈0.375 各穿 20 次，对比触发带长度、入口落点、出口落点；再旋屏一次重复。

---

## 9. 测试计划

### 引擎（cargo test -p padlend-core）

| 用例 | 断言 |
|---|---|
| tablet_rect_scale_1_matches_legacy | scale=1 时与既有数值逐位相同 |
| footprint_uses_rounded_scale | device 3200×0.373 → 1194，不是 1193.6 |
| entry_exit_round_trip | 随机 200 组 (scale, offset, along) 下 exit(entry(pc)) == pc (±1) |
| entry_clamps_to_device_edges | 足迹外/内极值 |
| contacts_band_shrinks_with_scale | 足迹 1200 px 时只有 1200 px 段可穿 |
| merged_segment_footprint | 跨两段的足迹 |
| repair_scale_clamps | 0.0 / 9.0 / NaN → 夹紧并记 issue |
| config_roundtrip_without_align | 老 YAML（含 align）加载 → 保存后不含 align、含 scale |

### 前端（npm test）

- geometry.test.ts：与 Rust 同一组数值表（同一输入 → 同一矩形/落点）。
- store/actions：改 scale → dirty；保存后清零。
- 画布：右键预设与自定义输入改 scale → 置脏；非法输入回退且不置脏。

### 手工验收（真机）

1. 3K 平板竖放 + 1440p 显示器，scale 调到 ≈0.375：画布上平板应明显短于显示器侧边（约 0.84 倍）。
2. 触发带只覆盖平板那一段（高亮条验证）。
3. 从触发带上/中/下三个位置各进 5 次，落点与入口成比例；退出后光标回到对应位置。
4. 旋屏 → 足迹重算、比例不变。
5. 老配置文件（含 align）启动：不报错、不丢其他设置。

---

## 10. 实施顺序

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P0** | 删 align（引擎 + 前端 + 夹具 + 文档）、删画布「测试穿越」入口、删 win/desktop.rs 死代码（Q5）；scale 字段先只做「存读」，几何仍按设备像素 | cargo test / npm test 全绿；老配置能加载；UI 无对齐菜单、无测试穿越；托盘/主菜单的手动接管仍可用 |
| **P1** | 几何引入 s；footprint_rect；画布「缩放」子菜单（预设 + 自定义）；保存链路 | scale≠1 时触发带、入口、出口都按比例；scale=1 行为不变 |
| **P2** | 触发带高亮（悬停时显示真正能穿的那一段）；重锚定后 scale 重算 | 可选，不阻塞 |

---

## 11. 备选方案与否决理由

| 方案 | 结论 |
|---|---|
| 保留 align 并真正实现三种模式 | 否决：与"画布即事实"冲突，三种模式意味着三条几何路径 + 两套 UI 语义，用户也只需要"画多少、穿多少" |
| 把显示器画小（而不是缩小平板） | 否决：所有屏幕坐标、边段、多屏相对位置都要改，纯粹为了视觉效果动核心坐标系 |
| 用物理尺寸自动推导、不给用户调 | 否决：EDID / 安卓 dpi 都不可靠（附录 B），推导错了用户无从修正 |
| 让平板自由 2D 摆放 | 暂不做：没有"贴边"就无法定义入口边与角落裁决；等真的有跨角需求再说 |

---

## 12. 已确认（2026-11-28）

| # | 问题 | 决定 |
|---|---|---|
| Q1 | scale 存哪份 | 每台设备横 / 竖各存一份，**保存时两档联动**（写同一个值，UI 一个滑块） |
| Q2 | 按物理尺寸自动估算 | **不做**。附录 B 降级为"未来可选"，不进版本计划；scale 全手动 |
| Q3 | 画布缩放交互 | **只用右键百分比**（预设 + 自定义输入，见 §5.5.1）；不做拖拽手柄，不做 Ctrl+滚轮 |
| Q4 | 「测试穿越」按钮 | **删除**（画布右键入口）；手动接管保留在主界面「手动接管 / 交还」与托盘菜单 |
| Q5 | win/desktop.rs 死代码 | **删除**（pc_to_tablet_along / tablet_to_pc_along 及测试） |

无阻塞项。若之后要物理 1:1 的指针速度，单开一版（附录 A）。

---

## 附录 A：为什么不动 gain

缩放只改"平板在桌面上占多大一块"，不改"平板内指针多快"。后者由
G = (W_tablet / W_monitor_ref) / XScale × gain 决定（docs/DESIGN.md §6.4.1），是按**分辨率**而不是物理尺寸算的。

所以做完本提案后，仍然可能存在：同样的鼠标位移，屏幕上走的厘米数和平板上走的厘米数不成 1:1（比如平板 ppi 是显示器的两倍）。

如果要物理 1:1，需要把 gain 改成按 mm/px 比例：
G_phys = (tablet_mm_per_px / monitor_mm_per_px) / XScale × gain。
这依赖本提案之外的数据（显示器 EDID、平板物理 dpi），而且改了会让现在的"屏幕比例感"变化——**单独一个版本做，别混在这次里**。

## 附录 B：自动估算 scale 的可行性（本期不做）

目标公式：scale = (tablet_mm / tablet_px) / (monitor_mm / monitor_px)。

| 数据 | 来源 | 坑 |
|---|---|---|
| 平板物理尺寸 | adb shell dumpsys display 的 xdpi/ydpi（物理 dpi）→ mm = px / dpi × 25.4 | 有些机型只报逻辑 dpi（如 320），算出来差 1.5–2 倍；拿不到就返回 None |
| 显示器物理尺寸 | EDID（EDID 里的 cm 宽高），或 GetDeviceCaps(HORZSIZE) | 多屏时要按 \\.\DISPLAYn 映射到 EDID 实例；虚拟显示器/投影仪常常没有 EDID |
| 兜底 | 用户手动输入"平板对角线英寸 / 显示器对角线英寸" | 需要新 UI |

结论：只做"尽力估算 + 可回退 + 可手调"，绝不作为默认值。

## 附录 C：本次改动后的几何示意

    显示器外轮廓（right 边，span 0..1440）
       │
       │        ┌──────────┐
       │        │          │  ← 平板足迹（footprint = device × 0.375）
       │        │  3200    │     竖放 3200×2136 → 1200×801
    1440        │          │
       │        │          │
       │        └──────────┘
       │  ↑ 触发带 = 足迹 ∩ 段（约 801 px，而不是整条 1440）
       └────────────────────────
