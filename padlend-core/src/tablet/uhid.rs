//! UHID 报文构造（对应 `tablet/uhid.py`，设计文档 9.3 / 9.4 / 6.7.2）。
//!
//! 两种设备、两种定长报告：
//!
//! * **鼠标**：4 字节 `[buttons, dx, dy, wheel]`，位移/滚轮有符号 `-127..127`。
//! * **键盘**：8 字节 `[modifier, 0x00, k1..k6]`，**全量状态**而不是增量。
//!
//! 键盘报告必须是"当前所有按下的键 + 修饰位"的快照，否则平板侧会粘键（6.7.2）。
//! 超过 6 键（6KRO 上限）按"先按优先"：第 7 个键被忽略，直到有键释放。
//!
//! 本模块纯计算、import 无副作用（Python 侧只 import `logging`，Rust 侧不写日志）。
//!
//! 常量出处：Python 把它们放在 `constants.py`；`constants.rs` 目前还是占位，
//! 所以 HID 报告布局与附录 A 的描述符先放在本模块，`constants.rs` 落地后应迁走。

// ---------------------------------------------------------------------------
// HID 报告布局（Python: constants.py 9.3 / 9.4）
// ---------------------------------------------------------------------------

/// 鼠标报告字节数（9.3）。
pub const MOUSE_REPORT_SIZE: usize = 4;
/// 键盘报告字节数（9.3）。
pub const KEYBOARD_REPORT_SIZE: usize = 8;
/// 6KRO 报告最多容纳的键数（修饰键走位图，不占槽位）。
pub const KEYBOARD_KRO: usize = 6;
/// 描述符 Logical Max = 127，单报文位移上限（附录 A.2）。
pub const HID_CLAMP: i32 = 127;

/// 键盘报告字节 0：左 Ctrl（6.7.2）。
pub const MOD_LEFT_CTRL: u8 = 0x01;
/// 键盘报告字节 0：左 Shift（6.7.2）。
pub const MOD_LEFT_SHIFT: u8 = 0x02;
/// 键盘报告字节 0：左 Alt（6.7.2）。
pub const MOD_LEFT_ALT: u8 = 0x04;
/// 键盘报告字节 0：左 Win/GUI（6.7.2）。
pub const MOD_LEFT_GUI: u8 = 0x08;
/// 键盘报告字节 0：右 Ctrl（6.7.2）。
pub const MOD_RIGHT_CTRL: u8 = 0x10;
/// 键盘报告字节 0：右 Shift（6.7.2）。
pub const MOD_RIGHT_SHIFT: u8 = 0x20;
/// 键盘报告字节 0：右 Alt（6.7.2）。
pub const MOD_RIGHT_ALT: u8 = 0x40;
/// 键盘报告字节 0：右 Win/GUI（6.7.2）。
pub const MOD_RIGHT_GUI: u8 = 0x80;

// ---------------------------------------------------------------------------
// 错误（Python 抛 ValueError）
// ---------------------------------------------------------------------------

/// UHID 报告构造错误；对应 Python 的 `ValueError`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UhidError {
    /// HID usage 必须是 1..=255（0 表示"空槽"，由调用方跳过）。
    #[error("HID usage 超出 1..255: {0}")]
    UsageOutOfRange(i32),
}

// ---------------------------------------------------------------------------
// 鼠标 / 键盘报告
// ---------------------------------------------------------------------------

/// 把有符号量夹到 `-limit..=limit`（Python 的 `_clamp_signed`）。
fn clamp_signed(value: i32, limit: i32) -> i32 {
    let high = if limit < 0 { -limit } else { limit };
    let low = -high;
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// 4 字节鼠标报告（9.3）。
///
/// `dx`/`dy`/`wheel` 先夹到 `±HID_CLAMP`（描述符 Logical Max = 127），
/// 再按补码编码（`value & 0xFF`）。按键位图只保留低 5 位。
pub fn mouse_report(buttons: i32, dx: i32, dy: i32, wheel: i32) -> [u8; MOUSE_REPORT_SIZE] {
    [
        (buttons & 0x1F) as u8,
        (clamp_signed(dx, HID_CLAMP) & 0xFF) as u8,
        (clamp_signed(dy, HID_CLAMP) & 0xFF) as u8,
        (clamp_signed(wheel, HID_CLAMP) & 0xFF) as u8,
    ]
}

/// 全零报告 = 所有键/修饰释放（防粘键的收尾）。
pub fn all_released_report() -> [u8; KEYBOARD_REPORT_SIZE] {
    [0u8; KEYBOARD_REPORT_SIZE]
}

/// 按"先按优先"排好槽位，拼出 8 字节报告。
///
/// 只做定长组装、不做校验；`kept` 的长度与取值范围由调用方保证。
fn assemble_report(modifier: u8, kept: &[u8]) -> [u8; KEYBOARD_REPORT_SIZE] {
    let mut report = [0u8; KEYBOARD_REPORT_SIZE];
    report[0] = modifier;
    // 字节 1 保留 0x00；字节 2..7 依次是当前按下的 usage，未用填 0x00。
    for (slot, &usage) in kept.iter().take(KEYBOARD_KRO).enumerate() {
        report[2 + slot] = usage;
    }
    report
}

/// 8 字节键盘报告（9.3）。
///
/// * 字节 0：修饰位图（低 8 位）
/// * 字节 1：保留 `0x00`
/// * 字节 2–7：当前按下的 HID usage，按传入顺序（先按优先），未用填 `0x00`
///
/// `usage == 0` 被跳过（空槽）；usage 不在 1..=255 时返回
/// [`UhidError::UsageOutOfRange`]（Python 抛 `ValueError`）；重复 usage 去重；
/// 第 7 个及以后的键直接丢弃（6KRO 上限，先按优先；Python 在这条路径打 debug
/// 日志，Rust 不写日志）。
pub fn keyboard_report<U>(modifier: i32, usages: &[U]) -> Result<[u8; KEYBOARD_REPORT_SIZE], UhidError>
where
    U: Copy + Into<i64>,
{
    let mut kept: Vec<u8> = Vec::with_capacity(KEYBOARD_KRO);
    for usage in usages {
        let value = (*usage).into();
        if value == 0 {
            continue;
        }
        if !(1..=0xFF).contains(&value) {
            return Err(UhidError::UsageOutOfRange(value as i32));
        }
        let value = value as u8;
        if kept.contains(&value) {
            continue;
        }
        if kept.len() >= KEYBOARD_KRO {
            // 6KRO 已满，忽略（先按优先）。
            continue;
        }
        kept.push(value);
    }
    Ok(assemble_report((modifier & 0xFF) as u8, &kept))
}

// ---------------------------------------------------------------------------
// 状态机
// ---------------------------------------------------------------------------

/// 按键状态机：维护修饰位图 + 按下顺序（6KRO，先按优先）。
///
/// 只做状态，不发包；调用方拿 [`KeyboardState::report`] 的结果走
/// `protocol::uhid_report`。同一次按下重复出现是幂等的（低级钩子可能重复投递）。
/// 只在 Router 转发线程访问，无需加锁。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardState {
    modifier: u8,
    pressed: Vec<u8>,
    ignored: Vec<u8>,
}

impl KeyboardState {
    /// 新建空状态（所有键/修饰均释放）。
    pub fn new() -> Self {
        Self::default()
    }

    // -- 查询 --------------------------------------------------------------

    /// 当前修饰位图（报告字节 0）。
    pub fn modifier(&self) -> u8 {
        self.modifier
    }

    /// 当前按下的 usage，按按下顺序。
    pub fn usages(&self) -> &[u8] {
        &self.pressed
    }

    /// 因 6KRO 满而被忽略的键（诊断用；释放后不会自动补上）。
    pub fn ignored(&self) -> &[u8] {
        &self.ignored
    }

    /// 当前按下的键数（不含修饰键）。
    pub fn pressed_count(&self) -> usize {
        self.pressed.len()
    }

    // -- 修改 --------------------------------------------------------------

    /// 直接覆盖修饰位图（只保留低 8 位）。
    pub fn set_modifiers(&mut self, bits: i32) {
        self.modifier = (bits & 0xFF) as u8;
    }

    /// 置位一个修饰位（只保留低 8 位）。
    pub fn press_modifier(&mut self, bit: i32) {
        self.modifier |= (bit & 0xFF) as u8;
    }

    /// 清掉一个修饰位（只保留低 8 位）。
    pub fn release_modifier(&mut self, bit: i32) {
        self.modifier &= !((bit & 0xFF) as u8);
    }

    /// 按下一个 usage；6KRO 已满时忽略并返回 `false`。
    ///
    /// usage 为 0 时返回 `Ok(false)`；超出 1..=255 时返回
    /// [`UhidError::UsageOutOfRange`]（Python 抛 `ValueError`）；已按住的
    /// usage 返回 `Ok(true)`（幂等，不重复占槽）。
    pub fn press(&mut self, usage: i32) -> Result<bool, UhidError> {
        if usage == 0 {
            return Ok(false);
        }
        if !(1..=0xFF).contains(&usage) {
            return Err(UhidError::UsageOutOfRange(usage));
        }
        let value = usage as u8;
        if self.pressed.contains(&value) {
            return Ok(true);
        }
        if self.pressed.len() >= KEYBOARD_KRO {
            self.ignored.push(value);
            return Ok(false);
        }
        self.pressed.push(value);
        Ok(true)
    }

    /// 释放一个 usage；不在按下/忽略集合里时什么也不做。
    pub fn release(&mut self, usage: i32) {
        let Ok(value) = u8::try_from(usage) else {
            // 超出 0..=255 的 usage 不可能在集合里（Python 的 `in` 同样不命中）。
            return;
        };
        if let Some(pos) = self.pressed.iter().position(|&pressed| pressed == value) {
            self.pressed.remove(pos);
        }
        if let Some(pos) = self.ignored.iter().position(|&ignored| ignored == value) {
            self.ignored.remove(pos);
        }
    }

    /// 清空修饰位图与按键集合（6.7.6：每次状态迁移都重置）。
    pub fn release_all(&mut self) {
        self.pressed.clear();
        self.ignored.clear();
        self.modifier = 0;
    }

    // -- 输出 --------------------------------------------------------------

    /// 当前全量状态的 8 字节报告（不改变状态）。
    pub fn report(&self) -> [u8; KEYBOARD_REPORT_SIZE] {
        assemble_report(self.modifier, &self.pressed)
    }
}

// ---------------------------------------------------------------------------
// 附录 A 的 HID 描述符与设备标识（Python: constants.py 9.3 / 9.4）
// ---------------------------------------------------------------------------

/// UHID 设备 id：鼠标。
pub const UHID_MOUSE_ID: u16 = 0x0001;
/// UHID 设备 id：键盘。
pub const UHID_KEYBOARD_ID: u16 = 0x0002;

/// USB vendor id（Linux Foundation）。
pub const HID_VENDOR_ID: u16 = 0x1D6B;
/// 鼠标 product id。
pub const HID_MOUSE_PRODUCT_ID: u16 = 0x0001;
/// 键盘 product id。
pub const HID_KEYBOARD_PRODUCT_ID: u16 = 0x0002;

/// UHID_CREATE 里的鼠标设备名（13 字节，不含 NUL）。
pub const HID_MOUSE_NAME: &[u8] = b"PadLend Mouse";
/// UHID_CREATE 里的键盘设备名（16 字节，不含 NUL）。
pub const HID_KEYBOARD_NAME: &[u8] = b"PadLend Keyboard";

/// 5 键 + 相对 X/Y + 滚轮（附录 A.2）。长度以实际字节为准，不写死 0x34。
pub const HID_MOUSE_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0,
];

/// 6KRO 键盘（附录 A.3）。长度以实际字节为准，不写死 0x2D。
pub const HID_KEYBOARD_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08,
    0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

// ---------------------------------------------------------------------------
// 单测：逐条对应 tests/test_protocol.py 的 UHID 部分
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- 鼠标报告 ----------------------------------------------------------

    #[test]
    fn mouse_report_is_four_bytes() {
        assert_eq!(mouse_report(0, 0, 0, 0), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(mouse_report(0, 0, 0, 0).len(), MOUSE_REPORT_SIZE);
    }

    #[test]
    fn mouse_report_encodes_signed_values_as_twos_complement() {
        // -1 按补码编码成 0xFF
        assert_eq!(mouse_report(0x01, -1, 0, 0), [0x01, 0xff, 0x00, 0x00]);
        assert_eq!(mouse_report(0, 0, -1, 0), [0x00, 0x00, 0xff, 0x00]);
        assert_eq!(mouse_report(0, 0, 0, -1), [0x00, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn mouse_report_clamps_to_hid_range() {
        assert_eq!(mouse_report(0, 200, 0, 0), [0x00, 0x7f, 0x00, 0x00]);
        assert_eq!(mouse_report(0, -200, 0, 0), [0x00, 0x81, 0x00, 0x00]);
        assert_eq!(
            mouse_report(0, HID_CLAMP, HID_CLAMP, HID_CLAMP),
            [0x00, 0x7f, 0x7f, 0x7f]
        );
        // 边界：正好 -127，不被夹但按补码是 0x81
        assert_eq!(mouse_report(0, -HID_CLAMP, 0, 0), [0x00, 0x81, 0x00, 0x00]);
    }

    #[test]
    fn mouse_report_keeps_only_low_five_button_bits() {
        // 9.3：按键位只保留低 5 位
        assert_eq!(mouse_report(0xFF, 0, 0, 0), [0x1f, 0x00, 0x00, 0x00]);
        // tests/test_session_mouse.py：Raw Input 的高位标志不得漏进报告
        assert_eq!(mouse_report(0x0400, 0, 0, 0)[0], 0);
        assert_eq!(mouse_report(0x01, 0, 0, 0)[0], 0x01);
    }

    #[test]
    fn clamp_signed_respects_explicit_limit() {
        assert_eq!(clamp_signed(200, 127), 127);
        assert_eq!(clamp_signed(-200, 127), -127);
        assert_eq!(clamp_signed(7, 127), 7);
        assert_eq!(clamp_signed(9, 3), 3);
        assert_eq!(clamp_signed(-9, 3), -3);
    }

    // -- 键盘报告 ----------------------------------------------------------

    #[test]
    fn all_released_report_is_eight_zero_bytes() {
        assert_eq!(all_released_report().len(), KEYBOARD_REPORT_SIZE);
        assert_eq!(all_released_report(), [0u8; 8]);
    }

    #[test]
    fn keyboard_report_layout_matches_golden() {
        let empty: [i32; 0] = [];
        assert_eq!(keyboard_report(0, &empty).unwrap(), [0u8; 8]);
        assert_eq!(
            keyboard_report(0x02, &[0x04, 0x05]).unwrap(),
            [0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0]
        );
        // 修饰位只保留低 8 位
        assert_eq!(keyboard_report(0x1FF, &empty).unwrap()[0], 0xFF);
    }

    #[test]
    fn keyboard_report_6kro_first_pressed_wins() {
        let packet = keyboard_report(0, &[1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert_eq!(packet.len(), 8);
        // 第 7 个键被丢弃
        assert_eq!(packet, [0, 0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn keyboard_report_dedupes_and_skips_zero() {
        assert_eq!(
            keyboard_report(0, &[0x04, 0x04, 0x00, 0x05]).unwrap(),
            [0, 0, 0x04, 0x05, 0, 0, 0, 0]
        );
    }

    #[test]
    fn keyboard_report_rejects_out_of_range_usage() {
        assert_eq!(
            keyboard_report(0, &[256]).unwrap_err(),
            UhidError::UsageOutOfRange(256)
        );
        assert_eq!(
            keyboard_report(0, &[-1]).unwrap_err(),
            UhidError::UsageOutOfRange(-1)
        );
        // 校验发生在"满槽"之前：6KRO 满了也不许静默吞掉越界值
        assert_eq!(
            keyboard_report(0, &[1, 2, 3, 4, 5, 6, 0x100]).unwrap_err(),
            UhidError::UsageOutOfRange(0x100)
        );
    }

    // -- 状态机 ------------------------------------------------------------

    #[test]
    fn keyboard_state_tracks_order_and_modifiers() {
        let mut state = KeyboardState::new();
        assert_eq!(state.report(), [0u8; 8]);

        state.press(0x04).unwrap();
        state.press(0x05).unwrap();
        state.press(0x06).unwrap();
        state.release(0x05);
        state.press(0x07).unwrap();
        assert_eq!(state.usages(), &[0x04, 0x06, 0x07]);
        assert_eq!(state.report(), [0, 0, 0x04, 0x06, 0x07, 0, 0, 0]);

        state.press_modifier(0x02);
        state.press_modifier(0x01);
        state.release_modifier(0x01);
        assert_eq!(state.modifier(), 0x02);
        assert_eq!(state.report()[0], 0x02);

        // 只保留低 8 位
        state.set_modifiers(0x1FF);
        assert_eq!(state.modifier(), 0xFF);
    }

    #[test]
    fn keyboard_state_6kro_ignores_seventh_key() {
        let mut state = KeyboardState::new();
        for usage in 1..8 {
            state.press(usage).unwrap();
        }
        assert_eq!(state.pressed_count(), KEYBOARD_KRO);
        assert_eq!(state.usages(), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(state.ignored(), &[7]);

        // 释放一个槽位后新键可以进入（先前被忽略的键不会自动补上）
        state.release(1);
        assert!(state.press(8).unwrap());
        assert_eq!(state.report(), [0, 0, 2, 3, 4, 5, 6, 8]);
    }

    #[test]
    fn keyboard_state_release_all_clears_everything() {
        let mut state = KeyboardState::new();
        state.set_modifiers(0x03);
        state.press(0x04).unwrap();
        state.press(0x05).unwrap();
        state.release_all();
        assert_eq!(state.report(), [0u8; 8]);
        assert_eq!(state.modifier(), 0);
        assert!(state.usages().is_empty());
        assert!(state.ignored().is_empty());
    }

    #[test]
    fn keyboard_state_press_is_idempotent_and_validates() {
        let mut state = KeyboardState::new();
        assert!(!state.press(0).unwrap());
        assert_eq!(state.pressed_count(), 0);
        assert!(state.press(0x04).unwrap());
        // 低级钩子可能重复投递同一次按下
        assert!(state.press(0x04).unwrap());
        assert_eq!(state.usages(), &[0x04]);
        assert_eq!(state.press(256).unwrap_err(), UhidError::UsageOutOfRange(256));
        assert_eq!(state.press(-1).unwrap_err(), UhidError::UsageOutOfRange(-1));
    }

    #[test]
    fn keyboard_state_release_unknown_usage_is_noop() {
        let mut state = KeyboardState::new();
        state.press(0x04).unwrap();
        state.release(0x05);
        state.release(-1);
        state.release(999);
        assert_eq!(state.usages(), &[0x04]);
    }

    // -- 描述符 ------------------------------------------------------------

    #[test]
    fn descriptors_and_names_match_python_lengths() {
        // 附录 A：UHID_CREATE 的 descLen 是 u16，长度断言锁住逐字节搬运
        assert_eq!(HID_MOUSE_DESCRIPTOR.len(), 62);
        assert_eq!(HID_KEYBOARD_DESCRIPTOR.len(), 45);
        assert_eq!(HID_MOUSE_NAME.len(), 13);
        assert_eq!(HID_KEYBOARD_NAME.len(), 16);
        assert_eq!(HID_MOUSE_NAME, b"PadLend Mouse");
        assert_eq!(HID_KEYBOARD_NAME, b"PadLend Keyboard");
        // 鼠标描述符尾部两个 C0（collection end）不能少
        assert_eq!(&HID_MOUSE_DESCRIPTOR[60..], &[0xC0, 0xC0]);
        assert_eq!(HID_KEYBOARD_DESCRIPTOR[44], 0xC0);
    }
}
