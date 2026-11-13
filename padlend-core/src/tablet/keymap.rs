//! VK → HID usage 映射与 6KRO 键盘状态机（对应 `tablet/keymap.py`，设计文档 6.7 / 8.2 / 附录 B）。
//!
//! 职责边界：
//! - 纯逻辑，只依赖标准库与 [`super::uhid`] 的常量；**不碰 win32**，
//!   因此可以被 Router 线程之外的单元测试直接驱动。
//! - 维护"当前按下的 usage 集合"与"修饰键位图"，每次返回**全量状态**（6.7.2）：
//!   报告不是增量，否则会粘键。
//! - 低级钩子回调**不得**调用本模块（本模块有日志 I/O），由 Router 在转发线程调用。
//!
//! 修饰键状态机必须自己维护：`WH_KEYBOARD_LL` 事件不带修饰位（6.2.2）。
//! 左右修饰优先信任事件的 `vk`（`VK_LCONTROL` 等）；钩子只报通用 VK 时，
//! 用 `scan` / `is_extended` 判左右（附录 B：`scan & 0xE000`）。
//!
//! Python 侧的 `logging` 在 Rust 这里没有对应框架：告警写到 stderr，同时留一份
//! 有上限的缓冲给 [`Keymap::warnings`]（测试与排查用）。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use super::uhid::{
    KEYBOARD_KRO, KEYBOARD_REPORT_SIZE, MOD_LEFT_ALT, MOD_LEFT_CTRL, MOD_LEFT_GUI, MOD_LEFT_SHIFT,
    MOD_RIGHT_ALT, MOD_RIGHT_CTRL, MOD_RIGHT_GUI, MOD_RIGHT_SHIFT,
};

// ---------------------------------------------------------------------------
// 虚拟键码（只列本模块关心的；数值来自 WinUser.h）
// ---------------------------------------------------------------------------

/// `VK_SHIFT`：通用 Shift。
pub const VK_SHIFT: i32 = 0x10;
/// `VK_CONTROL`：通用 Ctrl。
pub const VK_CONTROL: i32 = 0x11;
/// `VK_MENU`：通用 Alt。
pub const VK_MENU: i32 = 0x12;
/// `VK_LWIN`：左 Win。
pub const VK_LWIN: i32 = 0x5B;
/// `VK_RWIN`：右 Win。
pub const VK_RWIN: i32 = 0x5C;
/// `VK_LSHIFT`：左 Shift。
pub const VK_LSHIFT: i32 = 0xA0;
/// `VK_RSHIFT`：右 Shift。
pub const VK_RSHIFT: i32 = 0xA1;
/// `VK_LCONTROL`：左 Ctrl。
pub const VK_LCONTROL: i32 = 0xA2;
/// `VK_RCONTROL`：右 Ctrl。
pub const VK_RCONTROL: i32 = 0xA3;
/// `VK_LMENU`：左 Alt。
pub const VK_LMENU: i32 = 0xA4;
/// `VK_RMENU`：右 Alt。
pub const VK_RMENU: i32 = 0xA5;

/// 6KRO 报告最多容纳的键数（修饰键走位图，不占槽位）。
pub const MAX_USAGES: usize = KEYBOARD_KRO;

/// 诊断日志缓冲上限：告警本身已被"未知 VK 只报一次 / 溢出只在状态迁移时报"限流，
/// 这里再兜一层，避免长跑进程无界增长。
const WARN_LOG_CAP: usize = 64;

// ---------------------------------------------------------------------------
// 附录 B 的静态映射（6.7.3 公式之外的全部行）
// ---------------------------------------------------------------------------

/// 附录 B 里不能用公式生成的固定行：`(VK, HID usage)`。逐条来自设计文档附录 B。
pub const APPENDIX_B_TABLE: &[(i32, u8)] = &[
    (0x0D, 0x28), // VK_RETURN
    (0x1B, 0x29), // VK_ESCAPE
    (0x08, 0x2A), // VK_BACK
    (0x09, 0x2B), // VK_TAB
    (0x20, 0x2C), // VK_SPACE
    (0xBD, 0x2D), // VK_OEM_MINUS  "-_"
    (0xBB, 0x2E), // VK_OEM_PLUS   "=+"
    (0xBA, 0x33), // VK_OEM_1      ";:"
    (0xBF, 0x38), // VK_OEM_2      "/?"
    (0xC0, 0x35), // VK_OEM_3      （反引号/波浪号）
    (0xDB, 0x2F), // VK_OEM_4      "[{"
    (0xDC, 0x31), // VK_OEM_5      （反斜杠/竖线）
    (0xDD, 0x30), // VK_OEM_6      "]}"
    (0xDE, 0x34), // VK_OEM_7      （单引号/双引号）
    (0x25, 0x50), // VK_LEFT
    (0x27, 0x4F), // VK_RIGHT
    (0x26, 0x52), // VK_UP
    (0x28, 0x51), // VK_DOWN
    (0x24, 0x4A), // VK_HOME
    (0x23, 0x4D), // VK_END
    (0x21, 0x4B), // VK_PRIOR (PageUp)
    (0x22, 0x4E), // VK_NEXT  (PageDown)
    (0x2D, 0x49), // VK_INSERT
    (0x2E, 0x4C), // VK_DELETE
    (0x14, 0x39), // VK_CAPITAL
    (0x5D, 0x65), // VK_APPS
];

// ---------------------------------------------------------------------------
// 错误（Python 抛 ValueError）
// ---------------------------------------------------------------------------

/// 6KRO 报告构造错误；对应 Python `key_report` 抛出的 `ValueError`。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    /// 一帧最多 6 个键（修饰键不占槽位）。
    #[error("6KRO 最多 {max} 个键，收到 {got} 个")]
    TooManyUsages {
        /// 上限（[MAX_USAGES]）。
        max: usize,
        /// 实际收到的键数。
        got: usize,
    },
    /// 修饰位图必须是 0..=255。
    #[error("修饰位图越界: {0}")]
    ModifierOutOfRange(i64),
    /// 槽位里的 HID usage 必须是 0..=255（0 = 空槽）。
    #[error("HID usage 越界: {0}")]
    UsageOutOfRange(String),
}

// ---------------------------------------------------------------------------
// 映射表
// ---------------------------------------------------------------------------

/// 按 6.7.3 公式生成字母/数字/功能键，再并入附录 B 静态表。
fn build_vk_to_usage() -> HashMap<i32, u8> {
    let mut table: HashMap<i32, u8> = HashMap::with_capacity(74);
    // VK_A..VK_Z (0x41..0x5A) -> 0x04..0x1D
    for offset in 0x41i32..0x5B {
        table.insert(offset, (0x04 + (offset - 0x41)) as u8);
    }
    // VK_1..VK_9 -> 0x1E..0x26
    for vk in 0x31i32..=0x39 {
        table.insert(vk, (0x1E + (vk - 0x31)) as u8);
    }
    // VK_0 -> 0x27
    table.insert(0x30, 0x27);
    // VK_F1..VK_F12 (0x70..0x7B) -> 0x3A..0x45
    for vk in 0x70i32..0x7C {
        table.insert(vk, (0x3A + (vk - 0x70)) as u8);
    }
    for &(vk, usage) in APPENDIX_B_TABLE {
        table.insert(vk, usage);
    }
    table
}

/// 非修饰键 VK → HID usage（附录 B 全量，单向唯一）。
pub static VK_TO_USAGE: LazyLock<HashMap<i32, u8>> = LazyLock::new(build_vk_to_usage);

/// 修饰键 VK → 报告字节 0 的位图（6.7.2）。
pub static MODIFIER_VK_BITS: LazyLock<HashMap<i32, u8>> = LazyLock::new(|| {
    HashMap::from([
        (VK_LCONTROL, MOD_LEFT_CTRL),
        (VK_RCONTROL, MOD_RIGHT_CTRL),
        (VK_LSHIFT, MOD_LEFT_SHIFT),
        (VK_RSHIFT, MOD_RIGHT_SHIFT),
        (VK_LMENU, MOD_LEFT_ALT),
        (VK_RMENU, MOD_RIGHT_ALT),
        (VK_LWIN, MOD_LEFT_GUI),
        (VK_RWIN, MOD_RIGHT_GUI),
    ])
});

/// 通用修饰 VK → 左右两个位（事件只报通用 VK 时用于白名单展开）。
static MODIFIER_FAMILY: LazyLock<HashMap<i32, [u8; 2]>> = LazyLock::new(|| {
    HashMap::from([
        (VK_SHIFT, [MOD_LEFT_SHIFT, MOD_RIGHT_SHIFT]),
        (VK_CONTROL, [MOD_LEFT_CTRL, MOD_RIGHT_CTRL]),
        (VK_MENU, [MOD_LEFT_ALT, MOD_RIGHT_ALT]),
    ])
});

// ---------------------------------------------------------------------------
// VK 名称（配置里的 passthrough_keys 是名字，不是数字）
// ---------------------------------------------------------------------------

/// 不能用公式生成的 VK 规范名字。
const LITERAL_NAMES: &[(i32, &str)] = &[
    (0x0D, "VK_RETURN"),
    (0x1B, "VK_ESCAPE"),
    (0x08, "VK_BACK"),
    (0x09, "VK_TAB"),
    (0x20, "VK_SPACE"),
    (0xBD, "VK_OEM_MINUS"),
    (0xBB, "VK_OEM_PLUS"),
    (0xBA, "VK_OEM_1"),
    (0xBF, "VK_OEM_2"),
    (0xC0, "VK_OEM_3"),
    (0xDB, "VK_OEM_4"),
    (0xDC, "VK_OEM_5"),
    (0xDD, "VK_OEM_6"),
    (0xDE, "VK_OEM_7"),
    (0x25, "VK_LEFT"),
    (0x27, "VK_RIGHT"),
    (0x26, "VK_UP"),
    (0x28, "VK_DOWN"),
    (0x24, "VK_HOME"),
    (0x23, "VK_END"),
    (0x21, "VK_PRIOR"),
    (0x22, "VK_NEXT"),
    (0x2D, "VK_INSERT"),
    (0x2E, "VK_DELETE"),
    (0x14, "VK_CAPITAL"),
    (0x5D, "VK_APPS"),
    (VK_SHIFT, "VK_SHIFT"),
    (VK_CONTROL, "VK_CONTROL"),
    (VK_MENU, "VK_MENU"),
    (VK_LWIN, "VK_LWIN"),
    (VK_RWIN, "VK_RWIN"),
    (VK_LSHIFT, "VK_LSHIFT"),
    (VK_RSHIFT, "VK_RSHIFT"),
    (VK_LCONTROL, "VK_LCONTROL"),
    (VK_RCONTROL, "VK_RCONTROL"),
    (VK_LMENU, "VK_LMENU"),
    (VK_RMENU, "VK_RMENU"),
];

/// 组装 VK → 规范名字表（`VK_A` / `VK_0` / `VK_F1` / `VK_RETURN` …）。
fn build_vk_names() -> HashMap<i32, String> {
    let mut names: HashMap<i32, String> = LITERAL_NAMES
        .iter()
        .map(|&(vk, name)| (vk, name.to_string()))
        .collect();
    for vk in 0x41i32..0x5B {
        names.insert(vk, format!("VK_{}", (b'A' + (vk as u8 - 0x41)) as char));
    }
    for vk in 0x30i32..=0x39 {
        names.insert(vk, format!("VK_{}", (vk as u8) as char));
    }
    for index in 0..12 {
        names.insert(0x70 + index, format!("VK_F{}", index + 1));
    }
    names
}

/// VK → 规范名字（`VK_A` / `VK_0` / `VK_F1` / `VK_RETURN` …）。
pub static VK_NAMES: LazyLock<HashMap<i32, String>> = LazyLock::new(build_vk_names);

/// 规范名字 → VK（`passthrough_keys` 解析用）。
pub static VK_NAME_TO_CODE: LazyLock<HashMap<String, i32>> = LazyLock::new(|| {
    VK_NAMES
        .iter()
        .map(|(&vk, name)| (name.clone(), vk))
        .collect()
});

// ---------------------------------------------------------------------------
// 键盘事件（对应 `core/events.py` 的 `KeyEvent`）
// ---------------------------------------------------------------------------

/// 一次键盘事件（`WH_KEYBOARD_LL`）。
///
/// `vk` 为虚拟键码，`scan` 为扫描码（含扩展位，见 6.2.2），
/// `is_extended` 用于区分左右修饰键，`down` 表示按下。
///
/// 字段与 Python 的 `core.events.KeyEvent` 一一对应；`core/events.rs` 落地后
/// 应改为复用它，而不是各模块自带一份。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct KeyEvent {
    /// 虚拟键码。
    pub vk: i32,
    /// 扫描码（可含 `0xE000` 扩展位）。
    pub scan: i32,
    /// 是否带扩展位（区分左右 Ctrl/Alt）。
    pub is_extended: bool,
    /// `true` = 按下，`false` = 抬起。
    pub down: bool,
    /// `time.perf_counter()` 时间戳，仅用于延迟统计。
    pub ts: f64,
}

impl KeyEvent {
    /// 构造一个事件（`ts` 置 0）。
    pub fn new(vk: i32, scan: i32, is_extended: bool, down: bool) -> Self {
        Self {
            vk,
            scan,
            is_extended,
            down,
            ts: 0.0,
        }
    }

    /// 构造一个"按下"事件（`scan = 0`、非扩展）。
    pub fn key_down(vk: i32) -> Self {
        Self::new(vk, 0, false, true)
    }

    /// 构造一个"抬起"事件（`scan = 0`、非扩展）。
    pub fn key_up(vk: i32) -> Self {
        Self::new(vk, 0, false, false)
    }
}

// ---------------------------------------------------------------------------
// 纯函数
// ---------------------------------------------------------------------------

/// VK → HID usage；不是可转发键时返回 `None`。
pub fn usage_for(vk: i32) -> Option<u8> {
    VK_TO_USAGE.get(&vk).copied()
}

/// 返回事件对应的修饰键位；不是修饰键返回 `None`。
///
/// `scan` 可以只含低 8 位扫描码，也可以带 `0xE000` 扩展位。
/// Python 的 `scan`/`is_extended` 有默认值，Rust 无默认参数，调用方显式传 `0` / `false`。
pub fn modifier_bit(vk: i32, scan: i32, is_extended: bool) -> Option<u8> {
    if let Some(&bit) = MODIFIER_VK_BITS.get(&vk) {
        return Some(bit);
    }

    let scan_low = scan & 0xFF;
    if vk == VK_SHIFT {
        // Shift 的左右由扫描码区分（左 0x2A / 右 0x36），扩展位不可靠。
        return Some(if scan_low == 0x36 {
            MOD_RIGHT_SHIFT
        } else {
            MOD_LEFT_SHIFT
        });
    }

    let extended = is_extended || (scan & 0xE000) != 0;
    if vk == VK_CONTROL {
        return Some(if extended { MOD_RIGHT_CTRL } else { MOD_LEFT_CTRL });
    }
    if vk == VK_MENU {
        return Some(if extended { MOD_RIGHT_ALT } else { MOD_LEFT_ALT });
    }
    None
}

/// 构造 8 字节 6KRO 报告：`[mod, 0x00, k1..k6]`（6.7.2）。
///
/// 与 Python 一样做**全量校验**：超过 6 个键、修饰位图超出 0..=255、任一 usage
/// 超出 0..=255 都返回 `Err`（Python 抛 `ValueError`）。本函数**不去重、不跳 0**：
/// 那是 [`super::uhid::keyboard_report`] 的职责。
///
/// 泛型参数只为了同时接受 `&[u8]`（状态机输出）与 `&[i32]`（外部调用），不改变语义。
pub fn key_report<M, U>(modifier: M, usages: &[U]) -> Result<[u8; KEYBOARD_REPORT_SIZE], KeymapError>
where
    M: Into<i64>,
    U: Copy + Into<i64>,
{
    if usages.len() > MAX_USAGES {
        return Err(KeymapError::TooManyUsages {
            max: MAX_USAGES,
            got: usages.len(),
        });
    }
    let modifier = modifier.into();
    if !(0..=0xFF).contains(&modifier) {
        return Err(KeymapError::ModifierOutOfRange(modifier));
    }
    let mut slots: Vec<i64> = usages.iter().map(|usage| (*usage).into()).collect();
    slots.resize(MAX_USAGES, 0);
    if slots.iter().any(|&usage| !(0..=0xFF).contains(&usage)) {
        // 与 Python 的 f"HID usage 越界: {slots}" 对齐（Rust 的 {:?} 对 Vec 同样输出 [a, b, ...]）。
        return Err(KeymapError::UsageOutOfRange(format!("{slots:?}")));
    }

    let mut report = [0u8; KEYBOARD_REPORT_SIZE];
    report[0] = modifier as u8;
    for (slot, &usage) in slots.iter().enumerate() {
        report[2 + slot] = usage as u8;
    }
    Ok(report)
}

/// 记一条告警：写 stderr（生产观察）并保留一份给 [`Keymap::warnings`]（测试/排查）。
fn push_warning(log: &mut Vec<String>, message: String) {
    eprintln!("[keymap] {message}");
    if log.len() < WARN_LOG_CAP {
        log.push(message);
    }
}

/// 把配置里的 VK 名字解析成（精确 VK 集合，修饰位集合，未知名字告警）。
fn resolve_passthrough<I, S>(names: I) -> (HashSet<i32>, HashSet<u8>, Vec<String>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut vks: HashSet<i32> = HashSet::new();
    let mut mods: HashSet<u8> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    for raw in names {
        let raw = raw.as_ref();
        // 配置里允许小写与首尾空白（Python: str(raw).strip().upper()）。
        let name = raw.trim().to_uppercase();
        let Some(&code) = VK_NAME_TO_CODE.get(name.as_str()) else {
            warnings.push(format!("keymap.passthrough.unknown name={raw}"));
            continue;
        };
        vks.insert(code);
        if let Some(&bit) = MODIFIER_VK_BITS.get(&code) {
            mods.insert(bit);
            continue;
        }
        if let Some(family) = MODIFIER_FAMILY.get(&code) {
            // 通用修饰 VK 展开成左右两个位。
            mods.extend(family.iter().copied());
        }
    }
    (vks, mods, warnings)
}

// ---------------------------------------------------------------------------
// 状态机
// ---------------------------------------------------------------------------

/// 6KRO 键盘状态机；只在 Router 转发线程访问，无需加锁。
///
/// [`Keymap::on_key`] 每次返回 `(修饰位图, 当前按下的 usage 列表)`。
/// 列表按**先按优先**排序；超过 6 键时忽略新键并记一次警告（6.7.2）。
#[derive(Debug, Clone)]
pub struct Keymap {
    modifiers: u8,
    pressed: Vec<u8>,
    overflowing: bool,
    unknown_logged: HashSet<i32>,
    passthrough_vks: HashSet<i32>,
    passthrough_mods: HashSet<u8>,
    warnings: Vec<String>,
}

impl Keymap {
    /// 等价于 Python `Keymap()`：没有白名单。
    pub fn new() -> Self {
        Self::default()
    }

    /// 等价于 Python `Keymap(passthrough_keys)`：解析 `6.7.5` 的白名单。
    ///
    /// 名字大小写不敏感、允许首尾空白；未知名字记一条告警后忽略。
    pub fn with_passthrough<I, S>(passthrough_keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (passthrough_vks, passthrough_mods, messages) = resolve_passthrough(passthrough_keys);
        let mut warnings: Vec<String> = Vec::new();
        for message in messages {
            push_warning(&mut warnings, message);
        }
        Self {
            modifiers: 0,
            pressed: Vec::new(),
            overflowing: false,
            unknown_logged: HashSet::new(),
            passthrough_vks,
            passthrough_mods,
            warnings,
        }
    }

    // -- 状态 --------------------------------------------------------------

    /// 当前修饰键位图（6.7.2 报告字节 0）。
    pub fn modifiers(&self) -> u8 {
        self.modifiers
    }

    /// 当前按下的 HID usage（先按优先，最多 6 个）。
    pub fn pressed(&self) -> &[u8] {
        self.pressed.as_slice()
    }

    /// 白名单中的精确 VK。
    pub fn passthrough_vks(&self) -> &HashSet<i32> {
        &self.passthrough_vks
    }

    /// 白名单中的修饰位（含通用 VK 展开出的左右位）。
    pub fn passthrough_mods(&self) -> &HashSet<u8> {
        &self.passthrough_mods
    }

    /// 已记录的告警（Python 走 `logging`，无对应 API；Rust 侧留给测试与排查）。
    pub fn warnings(&self) -> &[String] {
        self.warnings.as_slice()
    }

    /// 当前全量状态（不改变状态）：`(修饰位图, usage 列表)`。
    pub fn report(&self) -> (u8, Vec<u8>) {
        (self.modifiers, self.pressed.clone())
    }

    /// 清空修饰位图与按键集合（6.7.6：每次状态迁移都重置）。
    pub fn reset(&mut self) {
        self.modifiers = 0;
        self.pressed.clear();
        self.overflowing = false;
    }

    // -- 白名单 ------------------------------------------------------------

    /// 该事件的键是否只给 PC（6.7.5 `passthrough_keys`）。
    ///
    /// 只用 `vk` / `scan` / `is_extended` 判断，**不改变**状态；Router 应在调用
    /// [`Keymap::on_key`] 之前分流，白名单键不进入平板报告。
    pub fn is_passthrough(&self, ev: &KeyEvent) -> bool {
        if self.passthrough_vks.contains(&ev.vk) {
            return true;
        }
        modifier_bit(ev.vk, ev.scan, ev.is_extended)
            .is_some_and(|bit| self.passthrough_mods.contains(&bit))
    }

    // -- 事件入口 ----------------------------------------------------------

    /// 处理一个键盘事件，返回 `(modifier, usages)` 全量报告。
    pub fn on_key(&mut self, ev: &KeyEvent) -> (u8, Vec<u8>) {
        if let Some(bit) = modifier_bit(ev.vk, ev.scan, ev.is_extended) {
            if ev.down {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
            return self.report();
        }

        let Some(usage) = usage_for(ev.vk) else {
            self.log_unknown(ev);
            return self.report();
        };

        if ev.down {
            if self.pressed.contains(&usage) {
                // 自动重复，不重复占槽。
                return self.report();
            }
            if self.pressed.len() >= MAX_USAGES {
                if !self.overflowing {
                    self.overflowing = true;
                    let message = format!(
                        "keymap.overflow pressed={} ignored_vk=0x{:02X} usage=0x{:02X}",
                        self.pressed.len(),
                        ev.vk,
                        usage
                    );
                    push_warning(&mut self.warnings, message);
                }
                return self.report();
            }
            self.overflowing = false;
            self.pressed.push(usage);
        } else {
            // Python 用 while 删除，保持同样的语义（幂等集合下最多删一次）。
            while let Some(pos) = self.pressed.iter().position(|&pressed| pressed == usage) {
                self.pressed.remove(pos);
            }
            self.overflowing = false;
        }
        self.report()
    }

    /// 同一未知 VK 只告警一次，避免按住时刷屏。
    fn log_unknown(&mut self, ev: &KeyEvent) {
        if !self.unknown_logged.insert(ev.vk) {
            return;
        }
        let message = format!("keymap.unknown vk=0x{:02X} scan=0x{:04X}", ev.vk, ev.scan);
        push_warning(&mut self.warnings, message);
    }
}

impl Default for Keymap {
    /// 等价于 Python `Keymap()`。
    fn default() -> Self {
        Self::with_passthrough(std::iter::empty::<&str>())
    }
}


// ---------------------------------------------------------------------------
// 单测：逐条对应 tests/test_keymap.py
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 附录 B 静态表的独立抄本（故意不引用实现里的表，避免同错同对）。
    const APPENDIX_B_LITERALS: &[(i32, u8)] = &[
        (0x0D, 0x28), // VK_RETURN
        (0x1B, 0x29), // VK_ESCAPE
        (0x08, 0x2A), // VK_BACK
        (0x09, 0x2B), // VK_TAB
        (0x20, 0x2C), // VK_SPACE
        (0xBD, 0x2D), // VK_OEM_MINUS
        (0xBB, 0x2E), // VK_OEM_PLUS
        (0xBA, 0x33), // VK_OEM_1
        (0xBF, 0x38), // VK_OEM_2
        (0xC0, 0x35), // VK_OEM_3
        (0xDB, 0x2F), // VK_OEM_4
        (0xDC, 0x31), // VK_OEM_5
        (0xDD, 0x30), // VK_OEM_6
        (0xDE, 0x34), // VK_OEM_7
        (0x25, 0x50), // VK_LEFT
        (0x27, 0x4F), // VK_RIGHT
        (0x26, 0x52), // VK_UP
        (0x28, 0x51), // VK_DOWN
        (0x24, 0x4A), // VK_HOME
        (0x23, 0x4D), // VK_END
        (0x21, 0x4B), // VK_PRIOR
        (0x22, 0x4E), // VK_NEXT
        (0x2D, 0x49), // VK_INSERT
        (0x2E, 0x4C), // VK_DELETE
        (0x14, 0x39), // VK_CAPITAL
        (0x5D, 0x65), // VK_APPS
    ];

    /// 一个"按下"事件。
    fn ev(vk: i32) -> KeyEvent {
        KeyEvent::key_down(vk)
    }

    /// 一个"抬起"事件。
    fn up(vk: i32) -> KeyEvent {
        KeyEvent::key_up(vk)
    }

    /// 带扫描码/扩展位的事件。
    fn ev_at(vk: i32, scan: i32, extended: bool, down: bool) -> KeyEvent {
        KeyEvent::new(vk, scan, extended, down)
    }

    fn overflow_warnings(km: &Keymap) -> usize {
        km.warnings()
            .iter()
            .filter(|message| message.contains("overflow"))
            .count()
    }

    fn counted_warnings(km: &Keymap, needle: &str) -> usize {
        km.warnings()
            .iter()
            .filter(|message| message.contains(needle))
            .count()
    }

    // -- 映射表 ------------------------------------------------------------

    #[test]
    fn letters_use_appendix_b_formula() {
        assert_eq!(usage_for(0x41), Some(0x04));
        assert_eq!(usage_for(0x42), Some(0x05));
        assert_eq!(usage_for(0x5A), Some(0x1D));
    }

    #[test]
    fn digits_use_appendix_b_formula() {
        assert_eq!(usage_for(0x31), Some(0x1E));
        assert_eq!(usage_for(0x32), Some(0x1F));
        assert_eq!(usage_for(0x39), Some(0x26));
        assert_eq!(usage_for(0x30), Some(0x27));
    }

    #[test]
    fn function_keys_use_appendix_b_formula() {
        assert_eq!(usage_for(0x70), Some(0x3A));
        assert_eq!(usage_for(0x71), Some(0x3B));
        assert_eq!(usage_for(0x7B), Some(0x45));
    }

    #[test]
    fn appendix_b_literal_rows() {
        // 附录 B 全表逐行核对
        assert_eq!(APPENDIX_B_LITERALS.len(), 26);
        for &(vk, usage) in APPENDIX_B_LITERALS {
            assert_eq!(usage_for(vk), Some(usage), "vk=0x{vk:02X}");
        }
        // 实现里的表必须与独立抄本完全一致（含顺序）
        assert_eq!(APPENDIX_B_TABLE, APPENDIX_B_LITERALS);
    }

    #[test]
    fn table_covers_every_rule_without_gaps() {
        let mut expected: HashSet<i32> = HashSet::new();
        expected.extend(0x41..0x5B); // A..Z
        expected.extend(0x30..0x3A); // 0..9
        expected.extend(0x70..0x7C); // F1..F12
        expected.extend(APPENDIX_B_LITERALS.iter().map(|&(vk, _)| vk));
        let actual: HashSet<i32> = VK_TO_USAGE.keys().copied().collect();
        assert_eq!(actual, expected);
        assert_eq!(VK_TO_USAGE.len(), 74);
    }

    #[test]
    fn table_is_one_to_one() {
        let usages: Vec<u8> = VK_TO_USAGE.values().copied().collect();
        let unique: HashSet<u8> = usages.iter().copied().collect();
        assert_eq!(unique.len(), usages.len());
    }

    #[test]
    fn modifiers_are_not_in_usage_table() {
        for vk in MODIFIER_VK_BITS.keys() {
            assert_eq!(usage_for(*vk), None, "vk=0x{vk:02X}");
        }
    }

    #[test]
    fn every_table_key_has_a_name() {
        for vk in VK_TO_USAGE.keys().chain(MODIFIER_VK_BITS.keys()) {
            assert!(VK_NAMES.contains_key(vk), "缺少名字: 0x{vk:02X}");
        }
        assert_eq!(VK_NAME_TO_CODE.len(), VK_NAMES.len());
        // 名字表必须双向唯一（passthrough 解析依赖反查）
        for (vk, name) in VK_NAMES.iter() {
            assert_eq!(VK_NAME_TO_CODE.get(name.as_str()), Some(vk));
        }
        assert_eq!(VK_NAMES.get(&0x41).map(String::as_str), Some("VK_A"));
        assert_eq!(VK_NAMES.get(&0x30).map(String::as_str), Some("VK_0"));
        assert_eq!(VK_NAMES.get(&0x70).map(String::as_str), Some("VK_F1"));
    }

    #[test]
    fn unknown_usage_returns_none() {
        assert_eq!(usage_for(0x99), None);
        assert_eq!(usage_for(0x00), None);
    }

    // -- 修饰键 ------------------------------------------------------------

    #[test]
    fn explicit_modifier_vks() {
        let cases = [
            (VK_LCONTROL, MOD_LEFT_CTRL),
            (VK_RCONTROL, MOD_RIGHT_CTRL),
            (VK_LSHIFT, MOD_LEFT_SHIFT),
            (VK_RSHIFT, MOD_RIGHT_SHIFT),
            (VK_LMENU, MOD_LEFT_ALT),
            (VK_RMENU, MOD_RIGHT_ALT),
            (VK_LWIN, MOD_LEFT_GUI),
            (VK_RWIN, MOD_RIGHT_GUI),
        ];
        for (vk, bit) in cases {
            let mut km = Keymap::new();
            assert_eq!(km.on_key(&ev(vk)), (bit, vec![]), "vk=0x{vk:02X}");
            assert_eq!(km.on_key(&up(vk)), (0, vec![]), "vk=0x{vk:02X}");
        }
    }

    #[test]
    fn generic_modifier_side_detection() {
        // 钩子只报通用 VK_SHIFT/VK_CONTROL/VK_MENU 时，用 scan / 扩展位判左右
        let cases = [
            (VK_SHIFT, 0x2A, false, MOD_LEFT_SHIFT),
            (VK_SHIFT, 0x36, false, MOD_RIGHT_SHIFT),
            (VK_CONTROL, 0x1D, false, MOD_LEFT_CTRL),
            (VK_CONTROL, 0x1D, true, MOD_RIGHT_CTRL),
            (VK_CONTROL, 0xE01D, false, MOD_RIGHT_CTRL),
            (VK_MENU, 0x38, false, MOD_LEFT_ALT),
            (VK_MENU, 0x38, true, MOD_RIGHT_ALT),
        ];
        for (vk, scan, extended, bit) in cases {
            let mut km = Keymap::new();
            assert_eq!(
                km.on_key(&ev_at(vk, scan, extended, true)),
                (bit, vec![]),
                "vk=0x{vk:02X} scan=0x{scan:04X}"
            );
            assert_eq!(km.on_key(&ev_at(vk, scan, extended, false)), (0, vec![]));
        }
        // 非修饰键返回 None
        assert_eq!(modifier_bit(0x41, 0, false), None);
        assert_eq!(modifier_bit(0x99, 0x1D, true), None);
    }

    #[test]
    fn modifier_bitmap_accumulates_and_clears() {
        let mut km = Keymap::new();
        assert_eq!(km.on_key(&ev(VK_LCONTROL)), (MOD_LEFT_CTRL, vec![]));
        assert_eq!(
            km.on_key(&ev(VK_LSHIFT)),
            (MOD_LEFT_CTRL | MOD_LEFT_SHIFT, vec![])
        );
        assert_eq!(
            km.on_key(&ev(VK_RMENU)),
            (MOD_LEFT_CTRL | MOD_LEFT_SHIFT | MOD_RIGHT_ALT, vec![])
        );
        // 没按下的修饰键抬起时不得清掉别的位
        assert_eq!(
            km.on_key(&up(VK_LMENU)),
            (MOD_LEFT_CTRL | MOD_LEFT_SHIFT | MOD_RIGHT_ALT, vec![])
        );
        assert_eq!(
            km.on_key(&up(VK_LCONTROL)),
            (MOD_LEFT_SHIFT | MOD_RIGHT_ALT, vec![])
        );
        assert_eq!(km.on_key(&up(VK_LSHIFT)), (MOD_RIGHT_ALT, vec![]));
        assert_eq!(km.on_key(&up(VK_RMENU)), (0, vec![]));
    }

    #[test]
    fn modifiers_do_not_consume_six_slots() {
        let mut km = Keymap::new();
        for vk in 0x41..0x47 {
            // A..F 共 6 键
            km.on_key(&ev(vk));
        }
        let (mods, usages) = km.on_key(&ev(VK_LSHIFT));
        assert_eq!(mods, MOD_LEFT_SHIFT);
        assert_eq!(usages, vec![0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
    }

    // -- 组合、顺序、释放 --------------------------------------------------

    #[test]
    fn combination_reports_modifier_and_keys() {
        let mut km = Keymap::new();
        km.on_key(&ev(VK_LCONTROL));
        let (mods, usages) = km.on_key(&ev(0x41)); // Ctrl+A
        assert_eq!(mods, MOD_LEFT_CTRL);
        assert_eq!(usages, vec![0x04]);
    }

    #[test]
    fn first_pressed_priority_is_preserved() {
        let mut km = Keymap::new();
        km.on_key(&ev(VK_LSHIFT));
        km.on_key(&ev(0x41));
        km.on_key(&ev(0x42));
        km.on_key(&ev(0x43));
        let (_, usages) = km.on_key(&ev(0x44));
        assert_eq!(usages, vec![0x04, 0x05, 0x06, 0x07]);

        let (_, usages) = km.on_key(&up(0x41)); // 释放 A
        assert_eq!(usages, vec![0x05, 0x06, 0x07]);

        let (_, usages) = km.on_key(&ev(0x41)); // 再按 A → 排到队尾
        assert_eq!(usages, vec![0x05, 0x06, 0x07, 0x04]);
    }

    #[test]
    fn release_reports_full_state_not_delta() {
        let mut km = Keymap::new();
        km.on_key(&ev(VK_LCONTROL));
        km.on_key(&ev(0x41));
        km.on_key(&ev(0x42));
        let (mods, usages) = km.on_key(&up(0x41));
        assert_eq!(mods, MOD_LEFT_CTRL);
        assert_eq!(usages, vec![0x05]); // 仍按住 B
    }

    #[test]
    fn release_of_unpressed_key_is_noop() {
        let mut km = Keymap::new();
        km.on_key(&ev(0x41));
        assert_eq!(km.on_key(&up(0x42)), (0, vec![0x04]));
    }

    #[test]
    fn auto_repeat_does_not_duplicate_slot() {
        let mut km = Keymap::new();
        assert_eq!(km.on_key(&ev(0x41)), (0, vec![0x04]));
        assert_eq!(km.on_key(&ev(0x41)), (0, vec![0x04]));
        assert_eq!(km.on_key(&up(0x41)), (0, vec![]));
    }

    // -- 6KRO 上限 ---------------------------------------------------------

    #[test]
    fn six_key_limit_ignores_new_key_and_warns() {
        let mut km = Keymap::new();
        for vk in 0x41..0x47 {
            // A..F
            km.on_key(&ev(vk));
        }
        let (mods, usages) = km.on_key(&ev(0x47)); // G 被忽略
        assert_eq!(mods, 0);
        assert_eq!(usages, vec![0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
        assert!(overflow_warnings(&km) >= 1);

        // 释放一个槽位后，新键可以进入
        km.on_key(&up(0x41));
        let (_, usages) = km.on_key(&ev(0x47));
        assert_eq!(usages, vec![0x05, 0x06, 0x07, 0x08, 0x09, 0x0A]);
    }

    #[test]
    fn overflow_warning_is_rate_limited() {
        let mut km = Keymap::new();
        for vk in 0x41..0x47 {
            km.on_key(&ev(vk));
        }
        km.on_key(&ev(0x47));
        km.on_key(&ev(0x48)); // 第二个溢出键不再重复告警
        assert_eq!(overflow_warnings(&km), 1);
    }

    // -- 未知键、重置 ------------------------------------------------------

    #[test]
    fn unknown_vk_is_ignored_and_warned_once() {
        let mut km = Keymap::new();
        km.on_key(&ev(0x41));
        assert_eq!(km.on_key(&ev(0x99)), (0, vec![0x04]));
        assert_eq!(km.on_key(&up(0x99)), (0, vec![0x04]));
        assert_eq!(counted_warnings(&km, "unknown"), 1);
    }

    #[test]
    fn unknown_vk_does_not_enter_slots() {
        let mut km = Keymap::new();
        assert_eq!(km.on_key(&ev(0x7C)), (0, vec![]));
        assert!(km.pressed().is_empty());
    }

    #[test]
    fn reset_clears_modifiers_and_keys() {
        let mut km = Keymap::new();
        km.on_key(&ev(VK_LCONTROL));
        km.on_key(&ev(0x41));
        km.reset();
        assert_eq!(km.modifiers(), 0);
        assert!(km.pressed().is_empty());
        assert_eq!(km.on_key(&ev(0x42)), (0, vec![0x05]));
    }

    // -- 白名单（passthrough_keys）-----------------------------------------

    #[test]
    fn passthrough_uses_config_defaults() {
        // config.KeyboardCfg().passthrough_keys 的默认值
        let km = Keymap::with_passthrough(["VK_LWIN", "VK_RWIN"]);
        assert!(km.is_passthrough(&ev(VK_LWIN)));
        assert!(km.is_passthrough(&ev(VK_RWIN)));
        assert!(!km.is_passthrough(&ev(0x41)));
        assert!(!km.is_passthrough(&ev(VK_LSHIFT)));
    }

    #[test]
    fn passthrough_matches_generic_modifier_family() {
        let km = Keymap::with_passthrough(["VK_CONTROL"]);
        assert!(km.is_passthrough(&ev(VK_LCONTROL)));
        assert!(km.is_passthrough(&ev_at(VK_CONTROL, 0x1D, true, true)));
        assert!(!km.is_passthrough(&ev(VK_LSHIFT)));
    }

    #[test]
    fn passthrough_does_not_mutate_state() {
        let km = Keymap::with_passthrough(["VK_LWIN"]);
        assert!(km.is_passthrough(&ev(VK_LWIN)));
        assert_eq!(km.modifiers(), 0);
        assert!(km.pressed().is_empty());
    }

    #[test]
    fn unknown_passthrough_name_is_logged() {
        let km = Keymap::with_passthrough(["VK_NOT_A_REAL_KEY"]);
        assert!(km.passthrough_vks().is_empty());
        assert!(km.passthrough_mods().is_empty());
        assert_eq!(counted_warnings(&km, "passthrough"), 1);
    }

    #[test]
    fn passthrough_name_is_case_and_space_insensitive() {
        // Python: str(raw).strip().upper()
        let km = Keymap::with_passthrough([" vk_lwin "]);
        assert!(km.passthrough_vks().contains(&VK_LWIN));
        assert!(km.passthrough_mods().contains(&MOD_LEFT_GUI));
        assert!(!km.is_passthrough(&ev(0x41)));
    }

    // -- 8 字节报告 --------------------------------------------------------

    #[test]
    fn key_report_layout() {
        let report = key_report(MOD_LEFT_CTRL, &[0x04, 0x05]).unwrap();
        assert_eq!(report, [0x01, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(report.len(), KEYBOARD_REPORT_SIZE);
        // 不满 6 个时补 0
        let empty: [u8; 0] = [];
        assert_eq!(key_report(0, &empty).unwrap(), [0u8; 8]);
    }

    #[test]
    fn key_report_from_state_machine() {
        let mut km = Keymap::new();
        km.on_key(&ev(VK_LCONTROL));
        km.on_key(&ev(0x41));
        let (mods, usages) = km.on_key(&ev(0x42));
        assert_eq!(
            key_report(mods, &usages).unwrap(),
            [0x01, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]
        );
        // 状态机的 &[u8] 与外部 &[i32] 都能直接传
        assert_eq!(key_report(0, &[0x04]).unwrap(), [0x00, 0x00, 0x04, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn key_report_rejects_invalid_input() {
        assert_eq!(
            key_report(0, &[0x04u8; 7]).unwrap_err(),
            KeymapError::TooManyUsages { max: 6, got: 7 }
        );
        let empty: [i32; 0] = [];
        assert_eq!(
            key_report(0x100, &empty).unwrap_err(),
            KeymapError::ModifierOutOfRange(0x100)
        );
        assert_eq!(
            key_report(-1, &empty).unwrap_err(),
            KeymapError::ModifierOutOfRange(-1)
        );
        // 越界 usage 的错误文本带上补零后的 6 个槽位（与 Python 的 f-string 一致）
        let error = key_report(0, &[0x04, 0x100]).unwrap_err();
        assert_eq!(
            error,
            KeymapError::UsageOutOfRange("[4, 256, 0, 0, 0, 0]".to_string())
        );
        assert_eq!(error.to_string(), "HID usage 越界: [4, 256, 0, 0, 0, 0]");
    }
}
