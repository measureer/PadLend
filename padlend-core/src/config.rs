//! 配置系统（对应 `config.py`；设计文档 6.9 / 8.3）。
//!
//! 路径：`%APPDATA%\PadLend\config.yaml`
//! - GUI 写入；手改后热加载（UI 层负责监听 mtime，本模块只负责解析/校验/落盘）。
//! - 非法值 → 回退默认 + 记录 issue（GUI 标红）+ 日志告警。
//! - `version` 驱动迁移；无法识别则备份 `config.yaml.bad-<时间戳>` 并重置。
//!
//! **兼容性契约**：字段名、嵌套结构、默认值，以及 `load()` 对缺失/多余/非法字段的
//! 容错行为必须与 Python 版逐字段一致 —— 老用户的 `config.yaml` 要能被 Rust 版原样
//! 读入。因此读取路径**不走 serde 的严格反序列化**，而是像 Python 一样先把 YAML
//! 解析成值树，再按 `_section()` 的规则逐字段强制转换（见 `fill`）。
//!
//! 已知偏差（都不影响 Python 自己写出的配置文件）：
//! - YAML 解析器是 YAML 1.2 核心模式，不是 PyYAML 的 YAML 1.1：`yes/on/off` 这类裸
//!   标量解析成字符串而非布尔（`py_as_bool` 会再兜一层，结果一致）；`01234`、
//!   `1_000` 解析成字符串（`py_int` 按十进制兜底，但 `01234` 在 Python 里是八进制
//!   668 —— 只有手改出来的配置会碰到）。
//! - 备份文件名的时间戳用 UTC（不能新增 chrono 依赖），Python 用本地时间；格式一致。
//! - 顶层未知键排序：Python 的 `sorted()` 遇到"整数键 + 字符串键"会抛 TypeError
//!   （整个配置加载失败），Rust 按字符串键排序后继续，不 panic。
//! - 重复键：PyYAML 取"后出现的值"，`serde_yaml_ng` 的 Mapping 反序列化直接报
//!   `duplicate entry`，于是走"备份 + 重置默认"（原文件留在 `.bad-*` 里）。
//! - YAML 1.1 的 merge key（`<<: *defaults`）不生效：PyYAML 会合并锚点内容，
//!   这里会把 `<<` 当成普通未知字段。
//! - 文首 UTF-8 BOM 已对齐：PyYAML 忽略它，这里在读入前同样剥掉一个 BOM。
//! - 日志：Python 版用 `logging` 记录 `cfg.invalid` 等；本模块不引入日志依赖，
//!   同等信息通过 `issues()` 暴露给界面。

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_yaml_ng::{Mapping, Value};

use crate::constants as C;

/// 当前配置版本；`version` 字段驱动迁移（目前只有 v1）。
pub const CURRENT_VERSION: i64 = 1;

/// 平板可以贴的桌面边。
pub const PLACEMENT_EDGES: [&str; 4] = ["left", "right", "top", "bottom"];
/// 摆位对齐方式。
pub const ALIGNS: [&str; 3] = ["proportional", "center", "one_to_one"];
/// `layouts` 二级键只允许这两个屏幕方向。
pub const LAYOUT_ORIENTATIONS: [&str; 2] = ["landscape", "portrait"];

// ---------------------------------------------------------------------------
// 保序映射（Python dict 语义）
// ---------------------------------------------------------------------------
/// 保序字符串键映射，语义对齐 Python `dict`。
///
/// 配置段在 Python 里是 `dict`，`validate()` / `_repair()` 的输出顺序依赖 YAML
/// 文档顺序，所以这里不能用 `BTreeMap`（会按 key 排序）或 `HashMap`（无序）。
/// 赋值保留首次插入的位置 —— Python `dict[k] = v` 覆盖已有键时同样保留原位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<V> OrderedMap<V> {
    /// 空映射。
    pub fn new() -> Self {
        Self::default()
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按文档顺序遍历。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> + '_ {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// 按文档顺序可变遍历（原地修复用）。
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut V)> + '_ {
        self.entries
            .iter_mut()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// 按键取引用。
    pub fn get(&self, key: &str) -> Option<&V> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// 按键取可变引用。
    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.entries
            .iter_mut()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// 键是否存在。
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.iter().any(|(existing, _)| existing == key)
    }

    /// 插入；键已存在时原地覆盖并返回旧值（与 `dict[k] = v` 一致）。
    pub fn insert(&mut self, key: String, value: V) -> Option<V> {
        for (existing, slot) in self.entries.iter_mut() {
            if *existing == key {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.entries.push((key, value));
        None
    }

    /// 删除并返回旧值；其余条目保持相对顺序。
    pub fn remove(&mut self, key: &str) -> Option<V> {
        match self.entries.iter().position(|(existing, _)| existing == key) {
            Some(index) => Some(self.entries.remove(index).1),
            None => None,
        }
    }

    /// 取或插入默认值后返回可变引用（对应 `dict.setdefault(k, {})`）。
    ///
    /// 返回 `Option` 只是为了避免非法索引；实际调用点传入的都是刚插入的下标。
    pub fn entry_or_insert_default(&mut self, key: String) -> Option<&mut V>
    where
        V: Default,
    {
        if let Some(index) = self.entries.iter().position(|(existing, _)| *existing == key) {
            return self.entries.get_mut(index).map(|(_, value)| value);
        }
        self.entries.push((key, V::default()));
        self.entries.last_mut().map(|(_, value)| value)
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = OrderedMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a mapping")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut out = OrderedMap::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    out.insert(key, value);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_map(MapVisitor(std::marker::PhantomData))
    }
}

// ---------------------------------------------------------------------------
// 各配置段（8.3）
// ---------------------------------------------------------------------------
/// 设备连接偏好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceCfg {
    /// 指定序列号；空 = 自动选择。
    pub serial: String,
    /// 优先走 USB（否则优先无线）。
    pub prefer_usb: bool,
    /// 启动后自动连接。
    pub auto_connect: bool,
    /// 服务端端口，合法范围 1024..=65535。
    pub server_port: i64,
    /// 最近连接过的设备（新的在前，最多 8 条）：设备页用它在离线时也列出来。
    pub known: Vec<String>,
    /// 每台无线设备的 Wi-Fi MAC（地址 → MAC）：路由器换了 IP 之后靠它在局域网里
    /// 找回来（0.3.13 只记最后一台，此后按设备分别保存）。
    pub macs: OrderedMap<String>,
}

impl Default for DeviceCfg {
    fn default() -> Self {
        Self {
            serial: String::new(),
            prefer_usb: true,
            auto_connect: true,
            server_port: i64::from(C::DEFAULT_SERVER_PORT),
            known: Vec::new(),
            macs: OrderedMap::new(),
        }
    }
}

/// 平板在桌面上的摆位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlacementCfg {
    /// 目标显示器接口名（如 `\\.\DISPLAY1`）；空 = 主屏。
    pub monitor: String,
    /// 贴哪条边：left | right | top | bottom。
    pub edge: String,
    /// 平板在该边上的起始坐标（像素）。
    pub span_start: i64,
    /// 结束坐标（像素），必须大于 `span_start`。
    pub span_end: i64,
    /// 沿边的相对位置 0.0~1.0。
    pub offset: f64,
    /// 对齐方式：proportional | center | one_to_one。
    pub align: String,
    /// 角落优先边（只允许 left/right/top/bottom 或空）。
    pub corner_priority: String,
}

impl Default for PlacementCfg {
    fn default() -> Self {
        Self {
            monitor: String::new(),
            edge: "right".to_string(),
            span_start: 0,
            span_end: 1600,
            offset: 0.5,
            align: "proportional".to_string(),
            corner_priority: "right".to_string(),
        }
    }
}

/// 每台设备、每个屏幕方向的摆位（1.0.3）。
///
/// `devices` 的键是设备身份（优先硬件序列号，见 `tablet/session.py` 的
/// `DeviceFacts.hardware_serial`）；二级键只有 `landscape` / `portrait`。
/// 旧配置文件没有这一段时为空，程序继续使用全局 `placement` 作为回退。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutsCfg {
    /// 设备身份（归一化后）→ 屏幕方向 → 摆位。
    pub devices: OrderedMap<OrderedMap<PlacementCfg>>,
}

impl LayoutsCfg {
    /// 取某设备某方向的摆位；缺失或空表返回 `None`。
    pub fn get(&self, serial: &str, orientation: &str) -> Option<&PlacementCfg> {
        let profiles = self.devices.get(&normalize_layout_key(serial))?;
        if profiles.is_empty() {
            return None;
        }
        profiles.get(&py_strip(orientation).to_lowercase())
    }

    /// 写入某设备某方向的摆位；设备身份为空或方向非法时静默忽略（与 Python 一致）。
    pub fn set(&mut self, serial: &str, orientation: &str, placement: PlacementCfg) {
        let key = normalize_layout_key(serial);
        let bucket = py_strip(orientation).to_lowercase();
        if key.is_empty() || !LAYOUT_ORIENTATIONS.contains(&bucket.as_str()) {
            return;
        }
        if let Some(profiles) = self.devices.entry_or_insert_default(key) {
            profiles.insert(bucket, placement);
        }
    }

    /// 丢弃某设备的全部摆位（Python 的 `drop`，改名避开 `Drop::drop`）。
    pub fn remove_device(&mut self, serial: &str) {
        self.devices.remove(&normalize_layout_key(serial));
    }
}

/// 指针运动与状态机阈值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionCfg {
    /// 指针增益，合法范围 0.25~3.0。
    pub gain: f64,
    /// 上报频率（Hz），只能是 125/250/500/1000。
    pub report_hz: i64,
    /// HID 单报文位移上限，合法范围 1~127。
    pub hid_clamp: i64,
    /// 进入平板前的安全边距（像素）。
    pub entry_margin_px: i64,
    /// 反向拖回多少像素才交还 PC。
    pub exit_overflow_px: i64,
    /// 边缘事件去抖窗口（毫秒）。
    pub debounce_ms: i64,
    /// 判定接管所需位移（像素）。
    pub arm_px: i64,
    /// 判定窗口（毫秒）。
    pub arm_window_ms: i64,
    /// 边缘捕获带宽度（像素）。
    pub edge_band_px: i64,
    /// 桌面边界内缩（像素）；H3 规避手势时 8~16。
    pub boundary_inset_px: i64,
}

impl Default for MotionCfg {
    fn default() -> Self {
        Self {
            gain: C::DEFAULT_GAIN,
            report_hz: i64::from(C::DEFAULT_REPORT_HZ),
            hid_clamp: i64::from(C::HID_CLAMP),
            entry_margin_px: i64::from(C::ENTRY_MARGIN_PX),
            exit_overflow_px: i64::from(C::EXIT_OVERFLOW_PX),
            debounce_ms: i64::from(C::DEBOUNCE_MS),
            arm_px: i64::from(C::ARM_PX),
            arm_window_ms: i64::from(C::ARM_WINDOW_MS),
            edge_band_px: i64::from(C::EDGE_BAND_PX),
            boundary_inset_px: i64::from(C::BOUNDARY_INSET_PX),
        }
    }
}

/// 键盘转发。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardCfg {
    /// 是否把 PC 键盘转发到平板。
    pub forward: bool,
    /// 接管期间抑制 PC 本地按键。
    pub suppress_pc: bool,
    /// 目标键盘布局（如 `us`）。
    pub layout: String,
    /// 原样透传、不拦截的按键名。
    pub passthrough_keys: Vec<String>,
}

impl Default for KeyboardCfg {
    fn default() -> Self {
        Self {
            forward: true,
            suppress_pc: true,
            layout: "us".to_string(),
            passthrough_keys: vec!["VK_LWIN".to_string(), "VK_RWIN".to_string()],
        }
    }
}

/// 全局热键。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyCfg {
    /// 接管开关热键。
    pub toggle: String,
    /// 紧急停止（panic）热键。
    pub panic: String,
}

impl Default for HotkeyCfg {
    fn default() -> Self {
        Self {
            toggle: "Ctrl+Alt+S".to_string(),
            panic: "Ctrl+Alt+Q".to_string(),
        }
    }
}

/// 界面选项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiCfg {
    /// 预留字段：1.0.0 只有中文界面，本值不生效；保留是为了兼容
    /// 0.3.x 已写出的 config.yaml（删键会让旧配置触发"未知配置项"告警）。
    pub language: String,
    /// 主题：dark | light（读入时统一小写）。
    pub theme: String,
    /// 托盘通知。
    pub tray_notifications: bool,
    /// 关闭主窗时：`true` = 收进托盘继续跑；`false` = 完全退出（0.3.12）。
    pub close_to_tray: bool,
}

impl Default for UiCfg {
    fn default() -> Self {
        Self {
            language: "zh_CN".to_string(),
            theme: "dark".to_string(),
            tray_notifications: true,
            close_to_tray: false,
        }
    }
}

/// 杂项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MiscCfg {
    /// 平板与 PC 共享剪贴板。
    pub clipboard_sync: bool,
    /// 接管期间保持平板屏幕常亮。
    pub keep_screen_on: bool,
    /// 停止接管后恢复的指针速度档位。
    pub restore_pointer_speed: i64,
    /// 1.4.0：可选把平板声音转发到 PC；需要重启 transport 才生效。
    pub forward_audio: bool,
}

impl Default for MiscCfg {
    fn default() -> Self {
        Self {
            clipboard_sync: true,
            keep_screen_on: true,
            restore_pointer_speed: -3,
            forward_audio: false,
        }
    }
}

/// 全部配置（对应 Python 的 `Config` dataclass）。
///
/// 字段顺序即 YAML 落盘顺序（Python 的 `asdict()` 同样按声明顺序）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 设备连接偏好。
    pub device: DeviceCfg,
    /// 全局摆位（每设备布局缺失时的回退）。
    pub placement: PlacementCfg,
    /// 1.0.3：每台设备 × 屏幕方向的摆位；`placement` 仍是全局回退。
    pub layouts: LayoutsCfg,
    /// 指针运动与状态机阈值。
    pub motion: MotionCfg,
    /// 键盘转发。
    pub keyboard: KeyboardCfg,
    /// 全局热键。
    pub hotkeys: HotkeyCfg,
    /// 界面选项。
    pub ui: UiCfg,
    /// 杂项。
    pub misc: MiscCfg,
    /// 非契约字段：最近一次加载/修复产生的问题（GUI 用来标红）。
    #[serde(skip)]
    issues: Vec<String>,
}

/// 设备身份归一化：去空白并忽略大小写（USB 序列号大小写不统一）。
///
/// Python 用 `str(serial or "").strip().casefold()`；这里用 `to_lowercase` 近似
/// casefold（ASCII 完全一致；`ß` 这类极少数字符有差异，设备序列号不会出现）。
pub fn normalize_layout_key(serial: &str) -> String {
    py_strip(serial).to_lowercase()
}
// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------
/// 文件无法读取时记录的问题（与 Python 文案逐字一致，且**不**改动磁盘上的文件）。
const READ_FAILED_ISSUE: &str = "配置文件无法读取，本次使用默认值（文件未改动）";

/// 配置落盘/取路径时的错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// YAML 序列化失败。
    #[error("配置文件序列化失败：{0}")]
    Serialize(#[from] serde_yaml_ng::Error),
    /// JSON 序列化失败（`to_dict()` 用）。
    #[error("配置无法序列化为 JSON：{0}")]
    Json(#[from] serde_json::Error),
    /// 结构体没有序列化成对象（不可能发生，留着避免 unwrap）。
    #[error("配置无法序列化为 JSON 对象")]
    NotAnObject,
    /// 配置目录创建失败。
    #[error("无法创建配置目录 {path}：{source}")]
    CreateDir {
        /// 出错的目录。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: io::Error,
    },
    /// 临时文件写入失败。
    #[error("无法写入临时配置文件 {path}：{source}")]
    WriteTemp {
        /// 出错的临时文件。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: io::Error,
    },
    /// 原子替换失败。
    #[error("无法替换配置文件 {path}：{source}")]
    Replace {
        /// 目标文件。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: io::Error,
    },
    /// 默认配置路径解析失败（`constants::config_dir`）。
    #[error(transparent)]
    Path(#[from] crate::constants::ConstantsError),
}

impl Config {
    /// 最近一次加载/修复留下的问题（与 Python 的 `issues` 属性一样是只读视图）。
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    /// 记录一条问题；同一条消息只保留第一次出现（与 Python 一致）。
    pub fn add_issue(&mut self, message: impl Into<String>) {
        let message = message.into();
        if !self.issues.contains(&message) {
            self.issues.push(message);
        }
    }

    /// 从文件加载配置；**任何**文件级错误都退化为"默认配置 + issue"，不返回 Err。
    ///
    /// 分支与 Python `Config.load` 一一对应：
    /// - 文件不存在 → 默认配置（不记 issue）；
    /// - 非 UTF-8 → 备份 `config.yaml.bad-<时间戳>` 后重置；
    /// - 读取失败（权限/目录）→ 默认配置，**不动**文件；
    /// - YAML 解析失败 / 根节点不是映射 → 备份后重置；
    /// - `version` 无法识别或高于当前程序 → 备份后重置；
    /// - 其余 → `from_mapping`（未知键告警 → 分段填充 → 自我修复）。
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let mut cfg = Config::default();
        if !path.exists() {
            return cfg;
        }

        // 先读字节再自己判 UTF-8，才能像 Python 一样区分“编码错误”和“读失败”。
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => {
                cfg.add_issue(READ_FAILED_ISSUE.to_string());
                return cfg;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                let backup = backup(path, "encoding");
                cfg.add_issue(reset_message(
                    "配置文件不是 UTF-8 编码",
                    backup.as_deref(),
                ));
                return cfg;
            }
        };

        // 记事本"另存为 UTF-8"会在文首写 BOM：PyYAML 会跳过它，serde_yaml_ng 则
        // 当非法字符报错。为了老配置能原样读入，这里同样去掉文首的一个 BOM。
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let parsed = match serde_yaml_ng::from_str::<Value>(text) {
            Ok(value) => value,
            Err(_) => {
                let backup = backup(path, "parse");
                cfg.add_issue(reset_message("配置文件无法解析", backup.as_deref()));
                return cfg;
            }
        };

        // Python 是 yaml.safe_load(text) or {}：空文档、~、[]、0、false 这类
        // "假值"根节点都会变成空映射，而不是"根节点不是映射"错误。
        let raw = match parsed {
            Value::Mapping(mapping) => mapping,
            other if !py_truthy(&other) => Mapping::new(),
            _ => {
                let backup = backup(path, "not-mapping");
                cfg.add_issue(reset_message("配置文件根节点不是映射", backup.as_deref()));
                return cfg;
            }
        };

        let version_value = match raw.get("version") {
            Some(value) => value.clone(),
            None => Value::from(CURRENT_VERSION),
        };
        let Some(version) = py_int(&version_value) else {
            let backup = backup(path, "bad-version");
            cfg.add_issue(reset_message(
                &format!("无法识别的配置版本 {}", py_repr(&version_value)),
                backup.as_deref(),
            ));
            return cfg;
        };
        if version > CURRENT_VERSION {
            let backup = backup(path, "future-version");
            cfg.add_issue(reset_message(
                &format!("配置版本 {version} 高于当前程序"),
                backup.as_deref(),
            ));
            return cfg;
        }

        apply_mapping(&raw, &mut cfg);
        cfg
    }

    /// 从已解析的 YAML 映射构造配置：未知键告警 → 分段填充 → 自我修复。
    ///
    /// 这是 `load()` 去掉"读文件 / 版本检查（含备份）"之后的纯数据部分；
    /// RPC 收到 GUI 传来的字典时也可以直接用它（`version` 会被当作已知键忽略）。
    pub fn from_mapping(raw: &Mapping) -> Self {
        let mut cfg = Config::default();
        apply_mapping(raw, &mut cfg);
        cfg
    }

    /// 与 Python `to_dict()` 等价（`version` + 各段），已剔除 `issues`。
    ///
    /// Python 版没有失败分支；这里返回 `Result` 只是因为 `serde_json::to_value`
    /// 的签名如此（本类型序列化不可能失败）。
    pub fn to_dict(&self) -> Result<serde_json::Value, ConfigError> {
        let mut out = serde_json::Map::new();
        out.insert(
            "version".to_string(),
            serde_json::Value::from(CURRENT_VERSION),
        );
        match serde_json::to_value(self)? {
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    out.insert(key, value);
                }
                Ok(serde_json::Value::Object(out))
            }
            _ => Err(ConfigError::NotAnObject),
        }
    }

    /// 落盘用的 YAML 文本：`version` 在最前，其余按字段声明顺序（与 Python 的
    /// {"version": _, **asdict(self)} + safe_dump 的顺序一致）。
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        let body = serde_yaml_ng::to_string(self)?;
        Ok(format!("version: {CURRENT_VERSION}\n{body}"))
    }

    /// 原子写入 YAML（先写临时文件再 replace，避免半截文件）。
    ///
    /// 与 Python 一样：同目录建临时文件 → 写入 → fsync → 原子替换；任何一步失败
    /// 都删掉临时文件，原配置文件保持原样。
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&parent).map_err(|source| ConfigError::CreateDir {
            path: parent.clone(),
            source,
        })?;

        let payload = self.to_yaml()?;
        let temporary = temporary_path(&parent);
        let written = fs::File::create(&temporary).and_then(|mut file| {
            file.write_all(payload.as_bytes())?;
            // 断电也不留半截配置：对应 Python 的 flush() + os.fsync()。
            file.sync_all()
        });
        if let Err(source) = written {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::WriteTemp {
                path: temporary,
                source,
            });
        }
        if let Err(source) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::Replace {
                path: path.to_path_buf(),
                source,
            });
        }
        Ok(())
    }

    /// 8.3 契约：返回问题列表，空 = 合法。**不修改对象**。
    pub fn validate(&self) -> Vec<String> {
        let mut problems: Vec<String> = Vec::new();

        if !(1024..=65535).contains(&self.device.server_port) {
            problems.push("device.server_port 必须在 1024..65535".to_string());
        }
        problems.extend(placement_problems(&self.placement, "placement"));
        for (serial, profiles) in self.layouts.devices.iter() {
            for (orientation, profile) in profiles.iter() {
                let prefix = format!("layouts.devices.{serial}.{orientation}");
                problems.extend(placement_problems(profile, &prefix));
            }
        }

        if !(1..=i64::from(C::HID_CLAMP)).contains(&self.motion.hid_clamp) {
            problems.push("motion.hid_clamp 必须在 1..127".to_string());
        }
        if !C::REPORT_HZ_CHOICES
            .iter()
            .any(|hz| i64::from(*hz) == self.motion.report_hz)
        {
            problems.push("motion.report_hz 必须是 125/250/500/1000 之一".to_string());
        }
        if !(0.25..=3.0).contains(&self.motion.gain) {
            problems.push("motion.gain 必须在 0.25..3.0".to_string());
        }
        if self.motion.arm_px < 1 {
            problems.push("motion.arm_px 必须 ≥1".to_string());
        }
        if self.motion.arm_window_ms < 1 {
            problems.push("motion.arm_window_ms 必须 ≥1".to_string());
        }
        if self.motion.exit_overflow_px < 1 {
            problems.push("motion.exit_overflow_px 必须 ≥1".to_string());
        }
        if self.motion.edge_band_px < 1 {
            problems.push("motion.edge_band_px 必须 ≥1".to_string());
        }
        if self.motion.debounce_ms < 0 {
            problems.push("motion.debounce_ms 不能为负".to_string());
        }
        if self.motion.boundary_inset_px < 0 {
            problems.push("motion.boundary_inset_px 不能为负".to_string());
        }

        if py_strip(&self.keyboard.layout).is_empty() {
            problems.push("keyboard.layout 不能为空".to_string());
        }
        // Python 还检查 passthrough_keys 是否为 list：_section() 之后必然是 list，
        // Rust 的类型上也不可能，故不移植这条死分支。

        if py_strip(&self.hotkeys.toggle).is_empty() {
            problems.push("hotkeys.toggle 不能为空".to_string());
        }
        if py_strip(&self.hotkeys.panic).is_empty() {
            problems.push("hotkeys.panic 不能为空".to_string());
        }
        problems
    }

    /// 把越界值夹回合法区间/回退默认，并记录 issue（6.9 校验规则）。
    ///
    /// issue 的产生顺序必须与 Python `_repair` 完全一致 —— GUI 按列表顺序展示，
    /// 对拍 fixture 也逐条比对。这里先收进 `found` 再统一 `add_issue`，因为
    /// `add_issue` 需要 `&mut self`，而修复过程要同时借用各个配置段。
    fn repair(&mut self) {
        let mut found: Vec<String> = Vec::new();

        if !(1024..=65535).contains(&self.device.server_port) {
            found.push(format!(
                "device.server_port={} 越界，已恢复默认",
                self.device.server_port
            ));
            self.device.server_port = i64::from(C::DEFAULT_SERVER_PORT);
        }

        // Python: str(self.ui.theme or "").lower()；读入时 theme 必为字符串。
        let theme = self.ui.theme.to_lowercase();
        if theme != "dark" && theme != "light" {
            found.push(format!(
                "ui.theme={} 非法，已回退 dark",
                py_repr_str(&self.ui.theme)
            ));
            self.ui.theme = "dark".to_string();
        } else {
            self.ui.theme = theme;
        }

        repair_placement(&mut self.placement, "placement", &mut found);

        // Python 遍历 list(devices.items())：空表直接删掉（不记 issue），其余逐条修复。
        // 空表分两步处理等价，因为删除本身不产生 issue。
        let empty: Vec<String> = self
            .layouts
            .devices
            .iter()
            .filter(|(_, profiles)| profiles.is_empty())
            .map(|(serial, _)| serial.to_string())
            .collect();
        for serial in empty {
            self.layouts.devices.remove(&serial);
        }
        for (serial, profiles) in self.layouts.devices.iter_mut() {
            for (orientation, profile) in profiles.iter_mut() {
                let prefix = format!("layouts.devices.{serial}.{orientation}");
                repair_placement(profile, &prefix, &mut found);
            }
        }

        let motion_defaults = MotionCfg::default();
        {
            let motion = &mut self.motion;
            if !(1..=i64::from(C::HID_CLAMP)).contains(&motion.hid_clamp) {
                found.push(format!(
                    "motion.hid_clamp={} 越界，已夹紧到 [1,127]",
                    motion.hid_clamp
                ));
                motion.hid_clamp = motion.hid_clamp.clamp(1, i64::from(C::HID_CLAMP));
            }
            if !C::REPORT_HZ_CHOICES
                .iter()
                .any(|hz| i64::from(*hz) == motion.report_hz)
            {
                let mut nearest = C::REPORT_HZ_CHOICES[0];
                let mut best = (i64::from(nearest) - motion.report_hz).abs();
                for hz in C::REPORT_HZ_CHOICES.iter().skip(1) {
                    let distance = (i64::from(*hz) - motion.report_hz).abs();
                    // 与 Python 的 min() 一样：距离相等时保留先出现的档位。
                    if distance < best {
                        best = distance;
                        nearest = *hz;
                    }
                }
                found.push(format!(
                    "motion.report_hz={} 不是固定档位，已改成 {}",
                    motion.report_hz, nearest
                ));
                motion.report_hz = i64::from(nearest);
            }
            if !(0.25..=3.0).contains(&motion.gain) {
                found.push(format!(
                    "motion.gain={} 越界，已夹紧到 [0.25,3.0]",
                    py_float_repr(motion.gain)
                ));
                // NaN 时 Python 的 min(max(...)) 仍是 NaN（比较全为 False），照抄。
                motion.gain = py_min_f64(py_max_f64(motion.gain, 0.25), 3.0);
            }
            if motion.arm_px < 1 {
                found.push("motion.arm_px 必须 ≥1，已恢复默认".to_string());
                motion.arm_px = motion_defaults.arm_px;
            }
            if motion.exit_overflow_px < 1 {
                found.push("motion.exit_overflow_px 必须 ≥1，已恢复默认".to_string());
                motion.exit_overflow_px = motion_defaults.exit_overflow_px;
            }
            if motion.edge_band_px < 1 {
                found.push("motion.edge_band_px 必须 ≥1，已恢复默认".to_string());
                motion.edge_band_px = motion_defaults.edge_band_px;
            }
            if motion.arm_window_ms < 1 {
                found.push("motion.arm_window_ms 必须 ≥1，已恢复默认".to_string());
                motion.arm_window_ms = motion_defaults.arm_window_ms;
            }
            if motion.debounce_ms < 0 {
                found.push("motion.debounce_ms 不能为负，已恢复默认".to_string());
                motion.debounce_ms = motion_defaults.debounce_ms;
            }
            if motion.boundary_inset_px < 0 {
                found.push("motion.boundary_inset_px 不能为负，已恢复默认".to_string());
                motion.boundary_inset_px = motion_defaults.boundary_inset_px;
            }
            if motion.entry_margin_px < 0 {
                found.push("motion.entry_margin_px 不能为负，已恢复默认".to_string());
                motion.entry_margin_px = motion_defaults.entry_margin_px;
            }
            // Python 在上一轮之后又检查了一次 arm_window_ms < 1：此时已恢复默认，
            // 永远不成立，这里只保留说明。
        }

        if py_strip(&self.keyboard.layout).is_empty() {
            found.push("keyboard.layout 为空，已回退 us".to_string());
            self.keyboard.layout = "us".to_string();
        }
        // Python 还有 if not isinstance(passthrough_keys, list) 分支：_section()
        // 之后必然是 list，Rust 类型上也不可能，故省略。

        let hotkey_defaults = HotkeyCfg::default();
        if py_strip(&self.hotkeys.toggle).is_empty() {
            found.push("hotkeys.toggle 为空，已恢复默认".to_string());
            self.hotkeys.toggle = hotkey_defaults.toggle;
        }
        if py_strip(&self.hotkeys.panic).is_empty() {
            found.push("hotkeys.panic 为空，已恢复默认".to_string());
            self.hotkeys.panic = hotkey_defaults.panic;
        }

        // Python 里还有一段 restore_pointer_speed == 0 and != -3 的空操作，
        // 只为注明"0 也是合法运行值"，Rust 无需移植。
        for issue in found {
            self.add_issue(issue);
        }
    }
}

/// 快捷入口：默认路径为 `%APPDATA%\PadLend\config.yaml`（对应 Python `load_config(path=None)`）。
///
/// 传入路径时等价于 `Config::load(path)`（Python 里测试就是这么用的）；
/// 只有"默认路径解析失败"（`constants::config_dir` 找不到可写目录）才会返回 `Err`，
/// 文件本身的损坏一律退化为"默认配置 + issue"。
pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    match path {
        Some(path) => Ok(Config::load(path)),
        None => Ok(Config::load(C::config_path()?)),
    }
}
// ---------------------------------------------------------------------------
// 填充与解析（对应 Python 的 _fill / _section / _parse_layouts）
// ---------------------------------------------------------------------------
/// 顶层已知段；其余键只告警不阻断（可能是新版本写回、也可能是手拍错）。
const KNOWN_SECTIONS: [&str; 9] = [
    "version",
    "device",
    "placement",
    "layouts",
    "motion",
    "keyboard",
    "hotkeys",
    "ui",
    "misc",
];

/// `load()` 的纯数据部分：未知键告警 → `fill` → `repair`。
fn apply_mapping(raw: &Mapping, cfg: &mut Config) {
    let mut unknown: Vec<String> = raw
        .keys()
        .map(py_str)
        .filter(|key| !KNOWN_SECTIONS.contains(&key.as_str()))
        .collect();
    // Python: sorted(set(raw) - known_sections)；UTF-8 字节序与码点序一致。
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        cfg.add_issue(format!("未知配置项已忽略：{}", unknown.join(", ")));
    }

    fill(raw, cfg);
    cfg.repair();
}

/// 逐段填充；段的顺序影响 issue 顺序（未知键 → 布局解析 → 修复）。
fn fill(raw: &Mapping, cfg: &mut Config) {
    cfg.device = section_device(raw.get("device"));
    cfg.placement = section_placement(raw.get("placement"));

    let (layouts, layout_issues) = parse_layouts(raw.get("layouts"));
    cfg.layouts = layouts;
    for issue in layout_issues {
        cfg.add_issue(issue);
    }

    cfg.motion = section_motion(raw.get("motion"));
    cfg.keyboard = section_keyboard(raw.get("keyboard"));
    cfg.hotkeys = section_hotkeys(raw.get("hotkeys"));
    cfg.ui = section_ui(raw.get("ui"));
    cfg.misc = section_misc(raw.get("misc"));
}

/// 设备段：额外处理 0.3.13–1.0.0 的 `device.mac` 单 MAC → `macs[serial]` 迁移。
fn section_device(raw: Option<&Value>) -> DeviceCfg {
    let mut out = DeviceCfg::default();
    let Some(Value::Mapping(original)) = raw else {
        return out;
    };
    let mut map = original.clone();

    // 0.3.13–1.0.0 只存了最后一台设备的 MAC；读旧配置时把它挪进 macs[serial]，
    // 用户升级后仍能用原来的地址找回平板。
    let legacy_mac = match map.remove("mac") {
        Some(value) if py_truthy(&value) => py_strip(&py_str(&value)).to_string(),
        _ => String::new(),
    };
    let serial = match map.get("serial") {
        Some(value) if py_truthy(value) => py_strip(&py_str(value)).to_string(),
        _ => String::new(),
    };
    if !legacy_mac.is_empty() && !serial.is_empty() {
        // Python 先在**原始** dict 上 setdefault(serial, mac) 再交给 _section() 转换，
        // 所以这里也必须先改原始映射（否则空值被过滤后 setdefault 语义就对不上）。
        let key = Value::from(serial);
        let mut macs = match map.get("macs") {
            Some(Value::Mapping(macs)) => macs.clone(),
            _ => Mapping::new(),
        };
        if !macs.contains_key(&key) {
            macs.insert(key, Value::from(legacy_mac));
        }
        map.insert(Value::from("macs"), Value::Mapping(macs));
    }

    take_str(&map, "serial", &mut out.serial);
    take_bool(&map, "prefer_usb", &mut out.prefer_usb);
    take_bool(&map, "auto_connect", &mut out.auto_connect);
    take_int(&map, "server_port", &mut out.server_port);
    take_strings(&map, "known", &mut out.known);
    take_str_map(&map, "macs", &mut out.macs);
    out
}

/// 摆位段。
fn section_placement(raw: Option<&Value>) -> PlacementCfg {
    let mut out = PlacementCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    take_str(map, "monitor", &mut out.monitor);
    take_str(map, "edge", &mut out.edge);
    take_int(map, "span_start", &mut out.span_start);
    take_int(map, "span_end", &mut out.span_end);
    take_float(map, "offset", &mut out.offset);
    take_str(map, "align", &mut out.align);
    take_str(map, "corner_priority", &mut out.corner_priority);
    out
}

/// 运动段。
fn section_motion(raw: Option<&Value>) -> MotionCfg {
    let mut out = MotionCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    take_float(map, "gain", &mut out.gain);
    take_int(map, "report_hz", &mut out.report_hz);
    take_int(map, "hid_clamp", &mut out.hid_clamp);
    take_int(map, "entry_margin_px", &mut out.entry_margin_px);
    take_int(map, "exit_overflow_px", &mut out.exit_overflow_px);
    take_int(map, "debounce_ms", &mut out.debounce_ms);
    take_int(map, "arm_px", &mut out.arm_px);
    take_int(map, "arm_window_ms", &mut out.arm_window_ms);
    take_int(map, "edge_band_px", &mut out.edge_band_px);
    take_int(map, "boundary_inset_px", &mut out.boundary_inset_px);
    out
}

/// 键盘段。
fn section_keyboard(raw: Option<&Value>) -> KeyboardCfg {
    let mut out = KeyboardCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    take_bool(map, "forward", &mut out.forward);
    take_bool(map, "suppress_pc", &mut out.suppress_pc);
    take_str(map, "layout", &mut out.layout);
    take_strings(map, "passthrough_keys", &mut out.passthrough_keys);
    out
}

/// 热键段。
fn section_hotkeys(raw: Option<&Value>) -> HotkeyCfg {
    let mut out = HotkeyCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    take_str(map, "toggle", &mut out.toggle);
    take_str(map, "panic", &mut out.panic);
    out
}

/// 界面段；1.2.1 删掉的 `show_pointer_ghost` 静默丢弃（不能触发未知字段告警）。
fn section_ui(raw: Option<&Value>) -> UiCfg {
    let mut out = UiCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    let mut map = map.clone();
    map.remove("show_pointer_ghost");
    take_str(&map, "language", &mut out.language);
    take_str(&map, "theme", &mut out.theme);
    take_bool(&map, "tray_notifications", &mut out.tray_notifications);
    take_bool(&map, "close_to_tray", &mut out.close_to_tray);
    out
}

/// 杂项段。
fn section_misc(raw: Option<&Value>) -> MiscCfg {
    let mut out = MiscCfg::default();
    let Some(map) = raw.and_then(Value::as_mapping) else {
        return out;
    };
    take_bool(map, "clipboard_sync", &mut out.clipboard_sync);
    take_bool(map, "keep_screen_on", &mut out.keep_screen_on);
    take_int(map, "restore_pointer_speed", &mut out.restore_pointer_speed);
    take_bool(map, "forward_audio", &mut out.forward_audio);
    out
}

/// 解析 1.0.3 的嵌套布局表；坏条目逐条忽略，不影响其它设备。
///
/// 返回（布局, 问题列表）——问题由调用方按顺序记入 `Config`，保证与 Python
/// 的 issue 顺序一致。
fn parse_layouts(raw: Option<&Value>) -> (LayoutsCfg, Vec<String>) {
    let mut layouts = LayoutsCfg::default();
    let mut issues: Vec<String> = Vec::new();

    let Some(value) = raw else {
        return (layouts, issues);
    };
    if value.is_null() {
        return (layouts, issues);
    }
    let Some(map) = value.as_mapping() else {
        issues.push("layouts 必须是映射，已忽略".to_string());
        return (layouts, issues);
    };

    let mut unknown: Vec<String> = map
        .keys()
        .map(py_str)
        .filter(|key| key != "devices")
        .collect();
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        issues.push(format!("layouts 未知配置项已忽略：{}", unknown.join(", ")));
    }

    let Some(devices_value) = map.get("devices") else {
        return (layouts, issues);
    };
    if devices_value.is_null() {
        return (layouts, issues);
    }
    let Some(devices) = devices_value.as_mapping() else {
        issues.push("layouts.devices 必须是映射，已忽略".to_string());
        return (layouts, issues);
    };

    for (serial_value, profiles_value) in devices {
        let serial_text = py_str(serial_value);
        // Python: normalize_layout_key(serial) 里是 str(serial or "")，
        // 因此假值键（null/0/false）归一化成空串 → 判非法。
        let key = if py_truthy(serial_value) {
            normalize_layout_key(&serial_text)
        } else {
            String::new()
        };
        if key.is_empty() {
            issues.push(format!("layouts.devices.{serial_text} 非法，已忽略"));
            continue;
        }
        let Some(profiles) = profiles_value.as_mapping() else {
            issues.push(format!("layouts.devices.{serial_text} 非法，已忽略"));
            continue;
        };

        let mut parsed: OrderedMap<PlacementCfg> = OrderedMap::new();
        for (orientation_value, payload) in profiles {
            let orientation_text = py_str(orientation_value);
            let bucket = py_strip(&orientation_text).to_lowercase();
            if !LAYOUT_ORIENTATIONS.contains(&bucket.as_str()) {
                issues.push(format!(
                    "layouts.devices.{serial_text}.{orientation_text} 方向非法\
                     （只支持 landscape/portrait），已忽略"
                ));
                continue;
            }
            if payload.as_mapping().is_none() {
                issues.push(format!(
                    "layouts.devices.{serial_text}.{orientation_text} 必须是映射，已忽略"
                ));
                continue;
            }
            parsed.insert(bucket, section_placement(Some(payload)));
        }
        if !parsed.is_empty() {
            // dict 语义：同归一化键覆盖值但保留首次出现的位置。
            layouts.devices.insert(key, parsed);
        }
    }
    (layouts, issues)
}

/// 按字段类型从映射里取一个字符串字段；缺失时保持默认值。
fn take_str(map: &Mapping, key: &str, slot: &mut String) {
    if let Some(value) = map.get(key) {
        *slot = py_str(value);
    }
}

/// 取一个布尔字段（字符串按 `_as_bool` 的规则转换）。
fn take_bool(map: &Mapping, key: &str, slot: &mut bool) {
    if let Some(value) = map.get(key) {
        *slot = py_as_bool(value);
    }
}

/// 取一个整数字段；转换失败（Python 抛 TypeError/ValueError）时保持默认值。
fn take_int(map: &Mapping, key: &str, slot: &mut i64) {
    if let Some(value) = map.get(key) {
        if let Some(number) = py_int(value) {
            *slot = number;
        }
    }
}

/// 取一个浮点字段；转换失败时保持默认值。
fn take_float(map: &Mapping, key: &str, slot: &mut f64) {
    if let Some(value) = map.get(key) {
        if let Some(number) = py_float(value) {
            *slot = number;
        }
    }
}

/// 取字符串列表字段（非序列时保持默认值；元素一律 `str()`）。
fn take_strings(map: &Mapping, key: &str, slot: &mut Vec<String>) {
    if let Some(value) = map.get(key) {
        if let Some(sequence) = value.as_sequence() {
            *slot = sequence.iter().map(py_str).collect();
        }
    }
}

/// 取 `str → str` 映射字段：键值都 `strip`，任一为空则丢弃（与 Python 一致）。
fn take_str_map(map: &Mapping, key: &str, slot: &mut OrderedMap<String>) {
    if let Some(value) = map.get(key) {
        if let Some(inner) = value.as_mapping() {
            *slot = coerce_str_map(inner);
        }
    }
}

/// `{str(k).strip(): str(v).strip() for ... if str(k).strip() and str(v).strip()}`。
fn coerce_str_map(source: &Mapping) -> OrderedMap<String> {
    let mut out = OrderedMap::new();
    for (key, value) in source {
        let key = py_strip(&py_str(key)).to_string();
        let value = py_strip(&py_str(value)).to_string();
        if !key.is_empty() && !value.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

/// 摆位的校验问题（`validate()` 用，纯函数、不修改对象）。
fn placement_problems(placement: &PlacementCfg, prefix: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !PLACEMENT_EDGES.contains(&placement.edge.as_str()) {
        problems.push(format!("{prefix}.edge 必须是 left/right/top/bottom"));
    }
    if !ALIGNS.contains(&placement.align.as_str()) {
        problems.push(format!("{prefix}.align 必须是 proportional/center/one_to_one"));
    }
    if !placement.corner_priority.is_empty()
        && !PLACEMENT_EDGES.contains(&placement.corner_priority.as_str())
    {
        problems.push(format!("{prefix}.corner_priority 必须是合法边或空"));
    }
    if !(0.0..=1.0).contains(&placement.offset) {
        problems.push(format!("{prefix}.offset 必须在 0.0..1.0"));
    }
    if placement.span_end <= placement.span_start {
        problems.push(format!("{prefix}.span_end 必须大于 span_start"));
    }
    problems
}

/// 摆位越界就地修复，问题追加到 `found`（顺序与 Python `_repair_placement` 一致）。
fn repair_placement(placement: &mut PlacementCfg, prefix: &str, found: &mut Vec<String>) {
    if !PLACEMENT_EDGES.contains(&placement.edge.as_str()) {
        found.push(format!(
            "{prefix}.edge={} 非法，已回退 right",
            py_repr_str(&placement.edge)
        ));
        placement.edge = "right".to_string();
    }
    if !ALIGNS.contains(&placement.align.as_str()) {
        found.push(format!(
            "{prefix}.align={} 非法，已回退 proportional",
            py_repr_str(&placement.align)
        ));
        placement.align = "proportional".to_string();
    }
    if !placement.corner_priority.is_empty()
        && !PLACEMENT_EDGES.contains(&placement.corner_priority.as_str())
    {
        found.push(format!(
            "{prefix}.corner_priority={} 非法，已回退 right",
            py_repr_str(&placement.corner_priority)
        ));
        placement.corner_priority = "right".to_string();
    }
    if !(0.0..=1.0).contains(&placement.offset) {
        found.push(format!(
            "{prefix}.offset={} 越界，已夹紧到 [0,1]",
            py_float_repr(placement.offset)
        ));
        placement.offset = py_min_f64(py_max_f64(placement.offset, 0.0), 1.0);
    }
    if placement.span_end <= placement.span_start {
        found.push(format!("{prefix}.span_end 必须大于 span_start，已恢复默认"));
        placement.span_start = 0;
        placement.span_end = 1600;
    }
}

// ---------------------------------------------------------------------------
// 备份与时间戳
// ---------------------------------------------------------------------------
/// 临时文件名计数器（同一进程内多次保存也不撞名）。
static TEMP_SEED: AtomicU64 = AtomicU64::new(0);

/// 与 Python 的 `mkstemp(prefix="config.", suffix=".tmp", dir=...)` 同目录、同前后缀。
fn temporary_path(parent: &Path) -> PathBuf {
    let seed = TEMP_SEED.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        "config.{}.{}.{}.tmp",
        std::process::id(),
        nanos,
        seed
    ))
}

/// 把损坏配置另存为 `config.yaml.bad-<时间戳>`，返回备份路径。
///
/// 旧版只写 `.bak` 且会覆盖，用户连续两次损坏后原始内容就丢了；带时间戳的文件
/// 可以一直保留，便于用户发回来定位问题。
///
/// 时间戳是 UTC（`%Y%m%d-%H%M%S`）：不能新增 `chrono` 依赖，而 `windows` crate 的
/// `GetLocalTime` 需要未启用的 feature。格式与 Python 完全一致，只有时区不同。
fn backup(path: &Path, reason: &str) -> Option<PathBuf> {
    let _ = reason;
    let name = path.file_name()?.to_string_lossy();
    let target = path.with_file_name(format!("{name}.bad-{}", timestamp_now()));
    match fs::copy(path, &target) {
        Ok(_) => Some(target),
        Err(_) => None,
    }
}

/// UTC 时间戳 `%Y%m%d-%H%M%S`。
fn timestamp_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    )
}

/// 1970-01-01 起的天数 → (年, 月, 日)，Howard Hinnant 的 civil_from_days 算法。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 重置类 issue 的文案：`<原因>，已备份为 config.yaml.bad-xxx 并重置为默认值`。
fn reset_message(reason: &str, backup: Option<&Path>) -> String {
    match backup.and_then(Path::file_name) {
        Some(name) => format!(
            "{reason}，已备份为 {} 并重置为默认值",
            name.to_string_lossy()
        ),
        None => format!("{reason}，已重置为默认值（备份失败，请检查目录权限）"),
    }
}

// ---------------------------------------------------------------------------
// Python 值语义（str()/repr()/int()/float()/bool()）
// ---------------------------------------------------------------------------
/// Python `str.strip()`：比 Rust `trim` 多认 `\x1c`–`\x1f` 四个分隔符。
fn py_strip(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}'))
}

/// Python `str(value)`：容器用 repr，标量用各自的文本形式。
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(flag) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(number) => py_number_str(number),
        Value::String(text) => text.clone(),
        Value::Sequence(sequence) => yaml_repr_sequence(sequence),
        Value::Mapping(mapping) => yaml_repr_mapping(mapping),
        // 标签值（!!binary 等）：Python 会得到 bytes/日期对象，这里退化为内层值。
        Value::Tagged(tagged) => py_repr(&tagged.value),
    }
}

/// Python `repr(value)`：字符串带引号，其余同 `str()`。
fn py_repr(value: &Value) -> String {
    match value {
        Value::String(text) => py_repr_str(text),
        other => py_str(other),
    }
}

/// 字符串的 Python `repr` 形式（用于 issue 文案里的 `{value!r}`）。
fn py_repr_str(text: &str) -> String {
    // CPython：含单引号且不含双引号时用双引号，否则用单引号。
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other == quote => {
                out.push('\\');
                out.push(other);
            }
            other if (other as u32) < 0x20 || other as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push(quote);
    out
}

/// Python `str(number)`：整数直接十进制，浮点用最短往返表示。
fn py_number_str(number: &serde_yaml_ng::Number) -> String {
    if let Some(value) = number.as_i64() {
        value.to_string()
    } else if let Some(value) = number.as_u64() {
        value.to_string()
    } else if let Some(value) = number.as_f64() {
        py_float_repr(value)
    } else {
        number.to_string()
    }
}

/// Python 的浮点 `repr`：最短往返 + 科学计数法指数带符号且至少两位。
fn py_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    let shortest = format!("{value:?}");
    if let Some(position) = shortest.find('e') {
        let (mantissa, exponent) = shortest.split_at(position);
        let digits = &exponent[1..];
        let (sign, digits) = match digits.strip_prefix('-') {
            Some(rest) => ('-', rest),
            None => ('+', digits.strip_prefix('+').unwrap_or(digits)),
        };
        let digits = if digits.len() < 2 {
            format!("0{digits}")
        } else {
            digits.to_string()
        };
        return format!("{mantissa}e{sign}{digits}");
    }
    if shortest.contains('.') {
        shortest
    } else {
        format!("{shortest}.0")
    }
}

/// Python `max(a, b)`：只有 `b > a` 才取 `b`，所以 NaN 会"粘住"第一个位置。
fn py_max_f64(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Python `min(a, b)`：只有 `b < a` 才取 `b`（NaN 语义同样照抄）。
fn py_min_f64(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// 列表的 Python `repr`：`['a', 1, None]`。
fn yaml_repr_sequence(sequence: &[Value]) -> String {
    let items: Vec<String> = sequence.iter().map(py_repr).collect();
    format!("[{}]", items.join(", "))
}

/// 映射的 Python `repr`：`{'a': 1}`。
fn yaml_repr_mapping(mapping: &Mapping) -> String {
    let items: Vec<String> = mapping
        .iter()
        .map(|(key, value)| format!("{}: {}", py_repr(key), py_repr(value)))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// Python `bool(value)` 的真值语义。
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Sequence(sequence) => !sequence.is_empty(),
        Value::Mapping(mapping) => !mapping.is_empty(),
        Value::Tagged(tagged) => py_truthy(&tagged.value),
    }
}

/// Python `_as_bool`：字符串按白名单（不区分大小写、去空白），其余走真值语义。
fn py_as_bool(value: &Value) -> bool {
    if let Value::String(text) = value {
        return matches!(
            py_strip(text).to_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "y"
        );
    }
    py_truthy(value)
}

/// Python `int(value)`；失败（抛 TypeError/ValueError）返回 `None`。
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Some(int)
            } else if let Some(uint) = number.as_u64() {
                i64::try_from(uint).ok()
            } else {
                number.as_f64().and_then(py_float_to_int)
            }
        }
        Value::String(text) => parse_py_int(text),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}

/// `int(float)`：向零截断；NaN/无穷在 Python 里抛异常。
fn py_float_to_int(number: f64) -> Option<i64> {
    if number.is_finite() {
        Some(number.trunc() as i64)
    } else {
        None
    }
}

/// Python `int("...")`：允许首尾空白与正负号，其余必须是十进制数字。
fn parse_py_int(text: &str) -> Option<i64> {
    let trimmed = py_strip(text);
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude: i64 = digits.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Python `float(value)`；失败返回 `None`。
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => py_strip(text).parse::<f64>().ok(),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicU64;

    // -- 测试辅助 ----------------------------------------------------------
    static TEST_DIR_SEED: AtomicU64 = AtomicU64::new(0);

    /// 自动清理的临时目录（不引入 tempfile 依赖）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir();
            for _ in 0..32 {
                let id = TEST_DIR_SEED.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("padlend-cfg-{tag}-{}-{id}", std::process::id()));
                if fs::create_dir_all(&path).is_ok() {
                    return Self { path };
                }
            }
            panic!("无法创建临时目录");
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn file(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn load_text(tag: &str, text: &str) -> (TempDir, Config) {
        let temp = TempDir::new(tag);
        let path = temp.file("config.yaml");
        fs::write(&path, text.as_bytes()).expect("写配置");
        let cfg = Config::load(&path);
        (temp, cfg)
    }

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
    }

    fn json_strings(value: &Json) -> Vec<String> {
        value
            .as_array()
            .expect("fixture 里的字段应是数组")
            .iter()
            .map(|item| item.as_str().expect("数组元素应是字符串").to_string())
            .collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn hex_decode(text: &str) -> Vec<u8> {
        let bytes = text.as_bytes();
        assert!(bytes.len() % 2 == 0, "hex 长度必须是偶数");
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks(2) {
            let high = (chunk[0] as char).to_digit(16).expect("hex 高位");
            let low = (chunk[1] as char).to_digit(16).expect("hex 低位");
            out.push((high * 16 + low) as u8);
        }
        out
    }

    /// 把备份文件名里的时间戳替换成 `.bad-<TS>`（两侧时间戳必然不同）。
    fn normalize_ts(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let marker: Vec<char> = ".bad-".chars().collect();
        let mut out = String::new();
        let mut index = 0;
        while index < chars.len() {
            if chars[index..].starts_with(&marker) {
                let start = index + marker.len();
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                if end - start == 8 && end < chars.len() && chars[end] == '-' {
                    let mut tail = end + 1;
                    while tail < chars.len() && chars[tail].is_ascii_digit() {
                        tail += 1;
                    }
                    if tail - (end + 1) == 6 {
                        out.push_str(".bad-<TS>");
                        index = tail;
                        continue;
                    }
                }
            }
            out.push(chars[index]);
            index += 1;
        }
        out
    }

    fn list_backups(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("读临时目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bad-"))
            .map(|name| normalize_ts(&name))
            .collect();
        names.sort();
        names
    }

    /// 递归比较 JSON：整数/浮点按数值比较（`1` 与 `1.0` 视为相等），其余严格。
    fn assert_json_eq(context: &str, actual: &Json, expected: &Json) {
        match (actual, expected) {
            (Json::Object(actual), Json::Object(expected)) => {
                let actual_keys: BTreeSet<&String> = actual.keys().collect();
                let expected_keys: BTreeSet<&String> = expected.keys().collect();
                assert_eq!(actual_keys, expected_keys, "{context}: 键集合不同");
                for (key, value) in actual {
                    assert_json_eq(
                        &format!("{context}.{key}"),
                        value,
                        expected.get(key).expect("键已对齐"),
                    );
                }
            }
            (Json::Array(actual), Json::Array(expected)) => {
                assert_eq!(actual.len(), expected.len(), "{context}: 数组长度不同");
                for (index, (left, right)) in actual.iter().zip(expected).enumerate() {
                    assert_json_eq(&format!("{context}[{index}]"), left, right);
                }
            }
            (Json::Number(actual), Json::Number(expected)) => {
                match (actual.as_i64(), expected.as_i64()) {
                    (Some(left), Some(right)) => assert_eq!(left, right, "{context}"),
                    _ => {
                        let left = actual.as_f64().expect("数值");
                        let right = expected.as_f64().expect("数值");
                        assert_eq!(left, right, "{context}");
                    }
                }
            }
            _ => assert_eq!(actual, expected, "{context}"),
        }
    }

    // -- 与 pytest test_config.py 逐条对应 ----------------------------------
    #[test]
    fn legacy_ghost_pointer_key_is_ignored() {
        // 1.2.1 删掉的 show_pointer_ghost 必须静默丢失，不能报未知字段。
        let (_temp, cfg) = load_text(
            "ghost",
            "version: 1\nui:\n  theme: dark\n  show_pointer_ghost: false\n",
        );
        assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
        assert_eq!(cfg.ui.theme, "dark");
        let dict = cfg.to_dict().expect("to_dict");
        assert!(dict["ui"].get("show_pointer_ghost").is_none());
    }

    #[test]
    fn default_when_missing() {
        let cfg = Config::default();
        assert!(cfg.issues().is_empty());
        assert!(cfg.validate().is_empty());
        assert_eq!(cfg.device.server_port, 1234);
        assert_eq!(cfg.device.serial, "");
        assert!(cfg.device.prefer_usb);
        assert!(cfg.device.auto_connect);
    }

    #[test]
    fn roundtrip() {
        let (temp, cfg) = load_text("roundtrip", "");
        assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());

        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存默认配置");
        let text = fs::read_to_string(&path).expect("读回");
        assert!(text.starts_with("version: 1\n"), "version 必须在最前：{text}");

        let again = Config::load(&path);
        assert!(again.issues().is_empty(), "{:?}", again.issues());
        assert_json_eq(
            "roundtrip",
            &again.to_dict().expect("to_dict"),
            &cfg.to_dict().expect("to_dict"),
        );
    }

    #[test]
    fn repair_clamps_and_reports() {
        let (_temp, cfg) = load_text(
            "repair",
            "version: 1\n\
             device:\n  server_port: 80\n\
             placement:\n  edge: diagonal\n  align: stretch\n  offset: 3.0\n  span_start: 100\n  span_end: 10\n\
             motion:\n  gain: 99\n  report_hz: 3\n  hid_clamp: 999\n  arm_px: 0\n\
             keyboard:\n  layout: \"\"\n  passthrough_keys: not-a-list\n\
             hotkeys:\n  toggle: \"\"\n",
        );
        assert!(cfg
            .issues()
            .contains(&"device.server_port=80 越界，已恢复默认".to_string()));
        assert!(cfg
            .issues()
            .contains(&"placement.edge='diagonal' 非法，已回退 right".to_string()));
        assert!(cfg
            .issues()
            .contains(&"motion.hid_clamp=999 越界，已夹紧到 [1,127]".to_string()));
        assert!(cfg.placement.offset >= 0.0 && cfg.placement.offset <= 1.0);
        assert!((0.25..=3.0).contains(&cfg.motion.gain));
        assert!(cfg.motion.hid_clamp >= 1 && cfg.motion.hid_clamp <= 127);
        assert_eq!(cfg.motion.report_hz, 125);
        // issue 顺序也是契约（GUI 按顺序展示）
        assert_eq!(
            cfg.issues(),
            strings(&[
                "device.server_port=80 越界，已恢复默认",
                "placement.edge='diagonal' 非法，已回退 right",
                "placement.align='stretch' 非法，已回退 proportional",
                "placement.offset=3.0 越界，已夹紧到 [0,1]",
                "placement.span_end 必须大于 span_start，已恢复默认",
                "motion.hid_clamp=999 越界，已夹紧到 [1,127]",
                "motion.report_hz=3 不是固定档位，已改成 125",
                "motion.gain=99.0 越界，已夹紧到 [0.25,3.0]",
                "motion.arm_px 必须 ≥1，已恢复默认",
                "keyboard.layout 为空，已回退 us",
                "hotkeys.toggle 为空，已恢复默认",
            ])
            .as_slice()
        );
        // 修复后的对象必须能通过 validate
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn report_hz_snaps_to_a_fixed_choice() {
        for (raw, expected) in [(187, 125), (990, 1000), (1100, 1000), (125, 125)] {
            let text = format!("version: 1\nmotion:\n  report_hz: {raw}\n");
            let (_temp, cfg) = load_text("hz", &text);
            assert_eq!(cfg.motion.report_hz, expected, "report_hz={raw}");
            if raw != expected {
                let wanted = format!("motion.report_hz={raw} 不是固定档位，已改成 {expected}");
                assert_eq!(cfg.issues()[0], wanted);
            } else {
                assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
            }
        }
    }

    #[test]
    fn theme_repair_and_roundtrip() {
        let (temp, cfg) = load_text("theme", "version: 1\nui:\n  theme: Neon\n");
        assert_eq!(cfg.issues(), strings(&["ui.theme='Neon' 非法，已回退 dark"]).as_slice());
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let again = Config::load(&path);
        assert!(again.issues().is_empty(), "{:?}", again.issues());
        assert_eq!(again.ui.theme, "dark");
    }

    #[test]
    fn future_version_resets() {
        let (temp, cfg) = load_text("future", "version: 2\ndevice:\n  serial: X\n");
        assert_eq!(cfg.issues().len(), 1, "{:?}", cfg.issues());
        assert!(
            cfg.issues()[0].starts_with("配置版本 2 高于当前程序，已备份为 "),
            "{:?}",
            cfg.issues()
        );
        assert!(cfg.issues()[0].ends_with(" 并重置为默认值"), "{:?}", cfg.issues());
        assert_eq!(cfg.device.serial, "");
        let backups: Vec<String> = fs::read_dir(temp.path())
            .expect("读目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bad-"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
    }

    #[test]
    fn validate_does_not_mutate() {
        let mut cfg = Config::default();
        cfg.placement.edge = "diagonal".to_string();
        cfg.motion.gain = 9.0;
        cfg.keyboard.layout = String::new();
        let before = cfg.to_dict().expect("to_dict");
        let problems = cfg.validate();
        assert!(problems.contains(&"placement.edge 必须是 left/right/top/bottom".to_string()));
        assert!(problems.contains(&"motion.gain 必须在 0.25..3.0".to_string()));
        assert!(problems.contains(&"keyboard.layout 不能为空".to_string()));
        assert_eq!(cfg.validate(), problems);
        assert_json_eq("validate", &cfg.to_dict().expect("to_dict"), &before);
    }

    #[test]
    fn legacy_entry_guard_is_ignored_and_removed_on_save() {
        let (temp, cfg) = load_text(
            "guard",
            "version: 1\nmisc:\n  single_instance: true\n  clipboard_sync: true\n",
        );
        // 段内未知字段只打日志（Rust 无日志，静默），cfg 上必须看不到它。
        assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let text = fs::read_to_string(&path).expect("读回");
        assert!(!text.contains("single_instance"), "{text}");
        assert!(text.contains("clipboard_sync: true"), "{text}");
    }

    #[test]
    fn to_dict_version() {
        let cfg = Config::default();
        let dict = cfg.to_dict().expect("to_dict");
        assert_eq!(dict["version"], Json::from(1));
        assert!(dict.get("issues").is_none());
        assert_eq!(dict.as_object().expect("对象").len(), 9);
    }

    #[test]
    fn motion_defaults_are_sane() {
        let cfg = Config::default();
        assert!((0.25..=3.0).contains(&cfg.motion.gain));
        assert!(C::REPORT_HZ_CHOICES
            .iter()
            .any(|hz| i64::from(*hz) == cfg.motion.report_hz));
        assert!(cfg.motion.hid_clamp >= 1 && cfg.motion.hid_clamp <= 127);
        assert_eq!(cfg.motion.exit_overflow_px, 80);
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn exit_overflow_default_is_80() {
        assert_eq!(MotionCfg::default().exit_overflow_px, 80);
    }

    #[test]
    fn known_devices_roundtrip() {
        let (temp, cfg) = load_text(
            "known",
            "version: 1\ndevice:\n  known:\n  - \"A1\"\n  - \"B2\"\n",
        );
        assert_eq!(cfg.device.known, strings(&["A1", "B2"]));
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let again = Config::load(&path);
        assert!(again.issues().is_empty(), "{:?}", again.issues());
        assert_eq!(again.device.known, strings(&["A1", "B2"]));
    }

    #[test]
    fn known_devices_default_empty_and_coerced() {
        let (_temp, cfg) = load_text("known-default", "version: 1\ndevice:\n  known: not-a-list\n");
        assert!(cfg.device.known.is_empty(), "{:?}", cfg.device.known);
        let (_temp, cfg) = load_text("known-coerce", "version: 1\ndevice:\n  known: [7, true]\n");
        assert_eq!(cfg.device.known, strings(&["7", "True"]));
    }

    #[test]
    fn device_macs_roundtrip() {
        let (temp, cfg) = load_text(
            "macs",
            "version: 1\ndevice:\n  macs:\n    \"a:b\": \"AA:BB\"\n    \"c:d\": \"CC:DD\"\n",
        );
        assert_eq!(cfg.device.macs.len(), 2);
        assert_eq!(cfg.device.macs.get("a:b").map(String::as_str), Some("AA:BB"));
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let again = Config::load(&path);
        assert!(again.issues().is_empty(), "{:?}", again.issues());
        assert_eq!(
            again.to_dict().expect("to_dict")["device"]["macs"]["c:d"],
            Json::from("CC:DD")
        );
    }

    #[test]
    fn legacy_single_mac_is_migrated_to_device_map() {
        let (temp, cfg) = load_text(
            "legacy-mac",
            "version: 1\ndevice:\n  serial: \"S1\"\n  mac: \"AA:BB\"\n",
        );
        assert_eq!(cfg.device.macs.get("S1").map(String::as_str), Some("AA:BB"));
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let text = fs::read_to_string(&path).expect("读回");
        assert!(!text.contains("mac: \"AA:BB\""), "{text}");
        let again = Config::load(&path);
        assert_eq!(again.device.macs.get("S1").map(String::as_str), Some("AA:BB"));
    }

    #[test]
    fn macs_invalid_type_falls_back_to_default() {
        let (_temp, cfg) = load_text("macs-bad", "version: 1\ndevice:\n  macs: not-a-map\n");
        assert!(cfg.device.macs.is_empty());
        assert!(cfg.to_dict().expect("to_dict")["device"]["macs"]
            .as_object()
            .expect("对象")
            .is_empty());
    }

    #[test]
    fn layouts_roundtrip_by_device_and_orientation() {
        let (temp, cfg) = load_text(
            "layouts",
            "version: 1\n\
             layouts:\n  devices:\n    \"SerialA\":\n      landscape:\n        edge: top\n        span_start: 10\n        span_end: 500\n        align: center\n        offset: 0.25\n        corner_priority: left\n      portrait:\n        edge: left\n",
        );
        assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
        // 设备键按 normalize_layout_key 归一化成小写
        assert!(cfg.layouts.devices.get("SerialA").is_none());
        let landscape = cfg
            .layouts
            .devices
            .get("seriala")
            .and_then(|profiles| profiles.get("landscape"))
            .expect("landscape 档");
        assert_eq!(landscape.edge, "top");
        assert_eq!(landscape.span_start, 10);
        assert_eq!(landscape.span_end, 500);
        assert_eq!(landscape.align, "center");
        assert_eq!(landscape.offset, 0.25);
        assert_eq!(landscape.corner_priority, "left");

        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        let again = Config::load(&path);
        assert!(again.issues().is_empty(), "{:?}", again.issues());
        assert_json_eq(
            "layouts",
            &again.to_dict().expect("to_dict"),
            &cfg.to_dict().expect("to_dict"),
        );
    }

    #[test]
    fn layouts_invalid_profiles_are_reported_and_repaired() {
        let (_temp, cfg) = load_text(
            "layouts-bad",
            "version: 1\nlayouts:\n  devices:\n    \"SerialX\":\n      landscape:\n        edge: diagonal\n",
        );
        assert_eq!(
            cfg.issues(),
            strings(&["layouts.devices.serialx.landscape.edge='diagonal' 非法，已回退 right"])
                .as_slice()
        );
        let landscape = cfg
            .layouts
            .devices
            .get("serialx")
            .and_then(|profiles| profiles.get("landscape"))
            .expect("归一化后的设备键");
        assert_eq!(landscape.edge, "right");
    }

    #[test]
    fn layouts_default_empty() {
        let dict = Config::default().to_dict().expect("to_dict");
        assert!(dict["layouts"]["devices"].as_object().expect("对象").is_empty());
    }

    #[test]
    fn forward_audio_defaults_off_and_roundtrips() {
        let cfg = Config::default();
        assert!(!cfg.misc.forward_audio);
        let (temp, cfg) = load_text("audio", "version: 1\nmisc:\n  forward_audio: true\n");
        assert!(cfg.misc.forward_audio);
        let path = temp.file("config.yaml");
        cfg.save(&path).expect("保存");
        assert!(Config::load(&path).misc.forward_audio);
    }

    /// 按 fixture 的 kind 造出要加载的文件：missing 不建，dir 建目录，其余写内容。
    fn write_case_file(path: &Path, case: &Json) {
        match case["kind"].as_str().expect("kind") {
            "dir" => fs::create_dir(path).expect("建目录"),
            "missing" => {}
            _ => {
                if let Some(text) = case["yaml"].as_str() {
                    fs::write(path, text.as_bytes()).expect("写 YAML");
                } else if let Some(hex) = case["yaml_hex"].as_str() {
                    fs::write(path, hex_decode(hex)).expect("写非 UTF-8 字节");
                } else {
                    panic!("fixture 没有内容：{}", case["name"]);
                }
            }
        }
    }

    fn case_bytes(case: &Json) -> Vec<u8> {
        if let Some(text) = case["yaml"].as_str() {
            text.as_bytes().to_vec()
        } else if let Some(hex) = case["yaml_hex"].as_str() {
            hex_decode(hex)
        } else {
            Vec::new()
        }
    }

    // -- 与 Python 生成的 fixture 逐字段对拍 --------------------------------
    #[test]
    fn fixture_cases_match_python() {
        let dir = fixtures_dir();
        let index_text =
            fs::read_to_string(dir.join("config_cases.json")).expect("缺少 config_cases.json");
        let cases: Json = serde_json::from_str(&index_text).expect("fixture 索引不是合法 JSON");
        let cases = cases.as_array().expect("fixture 索引应是数组");
        assert!(!cases.is_empty(), "fixture 索引为空");
        for case in cases {
            let name = case["name"].as_str().expect("name");
            // 重复键是已知偏差：PyYAML 取"后出现的值"，serde_yaml_ng 直接报解析错误
            // （mapping.rs 的 DuplicateKeyError）。这里断言偏差本身：备份 + 重置默认。
            if name == "duplicate_keys" {
                let temp = TempDir::new(name);
                let dup_path = temp.file("config.yaml");
                write_case_file(&dup_path, case);
                let cfg = Config::load(&dup_path);
                assert_json_eq(
                    &format!("{name}.config"),
                    &cfg.to_dict().expect("to_dict"),
                    &Config::default().to_dict().expect("to_dict"),
                );
                assert_eq!(cfg.issues().len(), 1, "{:?}", cfg.issues());
                assert!(
                    cfg.issues()[0].starts_with("配置文件无法解析，已备份为 "),
                    "{:?}",
                    cfg.issues()
                );
                assert_eq!(list_backups(temp.path()).len(), 1, "{name}: 应留下备份");
                continue;
            }
            // 先建好 fixture 想让我们加载的文件（Python 侧已放好真实内容）
            let temp = TempDir::new(name);
            let path = temp.file("config.yaml");
            write_case_file(&path, case);

            let cfg = Config::load(&path);
            assert_json_eq(
                &format!("{name}.config"),
                &cfg.to_dict().expect("to_dict"),
                &case["config"],
            );
            let actual_issues: Vec<String> = cfg.issues().iter().map(|item| normalize_ts(item)).collect();
            let expected_issues: Vec<String> = json_strings(&case["issues"])
                .iter()
                .map(|item| normalize_ts(item))
                .collect();
            assert_eq!(actual_issues, expected_issues, "{name}: issue 列表");
            assert_eq!(cfg.validate(), json_strings(&case["validate"]), "{name}: validate");

            let mut backups = list_backups(temp.path());
            let mut expected: Vec<String> = json_strings(&case["backups"])
                .into_iter()
                .map(|item| normalize_ts(&item))
                .collect();
            backups.sort();
            expected.sort();
            assert_eq!(backups, expected, "{name}: 备份文件");
            // 备份内容必须与原始配置逐字节一致（copy2 语义）
            if let Some(first) = expected.first() {
                let source = case_bytes(case);
                let backup_name = fs::read_dir(temp.path())
                    .expect("读目录")
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .find(|item| normalize_ts(item) == *first)
                    .expect("找到备份文件");
                let copied = fs::read(temp.file(&backup_name)).expect("读备份");
                assert_eq!(copied, source, "{name}: 备份内容");
            }
        }
    }

    #[test]
    fn python_saved_yaml_roundtrips() {
        let dir = fixtures_dir();
        let default_text = fs::read_to_string(dir.join("config_default_saved.yaml"))
            .expect("缺少 config_default_saved.yaml");
        let temp = TempDir::new("saved-default");
        let path = temp.file("config.yaml");
        fs::write(&path, default_text.as_bytes()).expect("写 Python 默认 YAML");
        let cfg = Config::load(&path);
        assert!(cfg.issues().is_empty(), "读入 Python 默认配置不应报错：{:?}", cfg.issues());
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        assert_json_eq(
            "default_saved",
            &cfg.to_dict().expect("to_dict"),
            &Config::default().to_dict().expect("to_dict"),
        );

        for name in ["config_default_saved.yaml", "config_mutated_saved.yaml"] {
            let text = fs::read_to_string(dir.join(name)).expect("缺少 Python 写出的 YAML");
            let temp = TempDir::new("saved");
            let path = temp.file("config.yaml");
            fs::write(&path, text.as_bytes()).expect("写配置");
            let first = Config::load(&path);
            assert!(first.issues().is_empty(), "{name}: {:?}", first.issues());
            first.save(&path).expect("Rust 再存一次");
            let second = Config::load(&path);
            assert!(second.issues().is_empty(), "{name}: {:?}", second.issues());
            assert_json_eq(
                name,
                &second.to_dict().expect("to_dict"),
                &first.to_dict().expect("to_dict"),
            );
            // 只能留下 config.yaml（临时文件必须清理干净）
            let names: Vec<String> = fs::read_dir(temp.path())
                .expect("读目录")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["config.yaml"], "{name}: 残留文件");
        }
    }

    // -- Python 值语义 / 边界 ----------------------------------------------
    #[test]
    fn nan_gain_matches_python() {
        // Python: min(max(nan, 0.25), 3.0) 仍是 nan —— 照抄这个语义。
        let (_temp, cfg) = load_text("nan", "version: 1\nmotion:\n  gain: .nan\n");
        assert!(cfg.issues().contains(&"motion.gain=nan 越界，已夹紧到 [0.25,3.0]".to_string()));
        assert!(cfg.motion.gain.is_nan());
        // NaN 参与比较恒为 False，所以 validate 会报"越界"（与 Python 一致）。
        assert!(cfg.validate().contains(&"motion.gain 必须在 0.25..3.0".to_string()));
    }

    #[test]
    fn mixed_type_unknown_keys_do_not_crash() {
        // Python 的 sorted({1, "zzz"}) 会抛 TypeError 让整个加载崩溃；Rust 版
        // 按字符串排序，记为一处有意差异（见模块文档）。
        let mut raw = Mapping::new();
        raw.insert(Value::from(1), Value::Null);
        raw.insert(Value::from("zzz"), Value::Null);
        let cfg = Config::from_mapping(&raw);
        assert_eq!(cfg.issues(), ["未知配置项已忽略：1, zzz"]);
    }

    #[test]
    fn python_value_semantics() {
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&Value::from(true)), "True");
        assert_eq!(py_str(&Value::from(7)), "7");
        assert_eq!(py_str(&Value::from(1.5)), "1.5");
        assert_eq!(py_str(&Value::from("abc")), "abc");
        assert_eq!(py_repr(&Value::from("abc")), "'abc'");
        assert_eq!(py_repr_str("a'b"), "\"a'b\"");
        assert_eq!(py_repr_str(""), "''");
        assert_eq!(py_repr_str("a\\b"), "'a\\\\b'");
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(0.5), "0.5");
        assert_eq!(py_float_repr(3.0), "3.0");
        assert_eq!(py_float_repr(1e20), "1e+20");
        assert_eq!(py_float_repr(1e-7), "1e-07");
        assert_eq!(py_float_repr(f64::NAN), "nan");
        assert_eq!(py_float_repr(f64::INFINITY), "inf");
        assert_eq!(py_int(&Value::from(3.9)), Some(3));
        assert_eq!(py_int(&Value::from(-3.9)), Some(-3));
        assert_eq!(py_int(&Value::from(" 42 ")), Some(42));
        assert_eq!(py_int(&Value::from("+7")), Some(7));
        assert_eq!(py_int(&Value::from("-7")), Some(-7));
        assert_eq!(py_int(&Value::from("1.0")), None);
        assert_eq!(py_int(&Value::from("abc")), None);
        assert_eq!(py_int(&Value::from(f64::NAN)), None);
        assert!(py_as_bool(&Value::from(" yes ")));
        assert!(py_as_bool(&Value::from("TRUE")));
        assert!(!py_as_bool(&Value::from("off")));
        assert!(!py_as_bool(&Value::Null));
        assert!(!py_as_bool(&Value::from(0)));
        assert!(py_as_bool(&Value::from(1)));
        assert_eq!(py_strip("\u{1c}a\u{1f} "), "a");
        assert_eq!(py_strip(" a "), "a");
    }

    #[test]
    fn layout_key_normalization() {
        assert_eq!(normalize_layout_key("  AM4N  "), "am4n");
        assert_eq!(normalize_layout_key(""), "");
        assert_eq!(normalize_layout_key("a:-b"), "a:-b");
    }

    #[test]
    fn ordered_map_keeps_insertion_order() {
        let mut map: OrderedMap<i64> = OrderedMap::new();
        assert!(map.is_empty());
        map.insert("b".to_string(), 2);
        map.insert("a".to_string(), 1);
        map.insert("b".to_string(), 3);
        assert_eq!(map.len(), 2);
        let seen: Vec<String> = map.iter().map(|(key, _)| key.to_string()).collect();
        assert_eq!(seen, ["b", "a"]);
        assert_eq!(map.get("b"), Some(&3));
        map.remove("b");
        let seen: Vec<String> = map.iter().map(|(key, _)| key.to_string()).collect();
        assert_eq!(seen, ["a"]);
        assert!(map.remove("b").is_none());
    }

    #[test]
    fn save_leaves_no_temporary_files() {
        let temp = TempDir::new("atomic");
        let path = temp.file("config.yaml");
        let cfg = Config::default();
        cfg.save(&path).expect("第一次保存");
        cfg.save(&path).expect("第二次保存");
        let names: Vec<String> = fs::read_dir(temp.path())
            .expect("读目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["config.yaml"]);
    }

    #[test]
    fn yaml_11_boolean_spellings_map_like_python() {
        // YAML 1.1 里 yes/on 是布尔；serde_yaml_ng 按 1.2 解析成字符串，
        // 但 _as_bool 的白名单把它们救回来，最终结果与 Python 一致。
        let (_temp, cfg) = load_text(
            "yaml11",
            "version: 1\ndevice:\n  prefer_usb: no\n  auto_connect: on\n",
        );
        assert!(!cfg.device.prefer_usb);
        assert!(cfg.device.auto_connect);
        assert!(cfg.issues().is_empty(), "{:?}", cfg.issues());
    }

    #[test]
    fn duplicate_keys_are_rejected_known_deviation() {
        // 已知差异：PyYAML 对重复键取"后出现的值"（gain=2.0），serde_yaml_ng 的
        // Mapping 反序列化直接报 duplicate entry（mapping.rs:817），于是走
        // "备份 + 重置默认"。只有手改出重复键才会碰到，原始内容留在 .bad-* 里。
        let text = "version: 1\nmotion:\n  gain: 1.0\n  gain: 2.0\n";
        let (temp, cfg) = load_text("dup", text);
        assert_eq!(cfg.motion.gain, MotionCfg::default().gain);
        assert_eq!(cfg.issues().len(), 1, "{:?}", cfg.issues());
        assert!(
            cfg.issues()[0].starts_with("配置文件无法解析，已备份为 "),
            "{:?}",
            cfg.issues()
        );
        assert_eq!(list_backups(temp.path()).len(), 1);
    }

    #[test]
    fn null_root_document_is_default() {
        for text in ["~", "", "[]", "{}", "0", "false"] {
            let (_temp, cfg) = load_text("root", text);
            assert!(cfg.issues().is_empty(), "{text:?}: {:?}", cfg.issues());
        }
    }
}
