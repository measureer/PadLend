//! scrcpy 控制协议封包（对应 `tablet/protocol.py`，设计文档 9.1 / 9.2，纯函数）。
//!
//! 链路：
//!
//! ```text
//! PC --TCP 127.0.0.1:{port}--> adb forward --> localabstract:scrcpy --> scrcpy-server
//! ```
//!
//! * 连接后先读 1 字节 `00`（`send_dummy_byte=true`；实测 scrcpy 5.0.1）。
//! * 之后是连续消息流，无长度前缀，首字节 `type` 分派。
//! * **全部大端**（网络序）。
//!
//! 本模块只做字节拼装：不碰 socket、不碰 adb、无副作用。
//! 版本差异（9.5）只改这里。

// ---------------------------------------------------------------------------
// 9.2 控制消息 type 枚举（与 scrcpy app/src/control_msg.h 对齐）
// ---------------------------------------------------------------------------

/// INJECT_KEYCODE：降级路径 / 兜底键（8.2）。
pub const MSG_INJECT_KEYCODE: u8 = 0;
/// INJECT_TEXT：UTF-8 直注入（6.7.4 中文备选路径）。
pub const MSG_INJECT_TEXT: u8 = 1;
/// INJECT_TOUCH_EVENT：不使用（F2：绝对注入没有系统指针）。
pub const MSG_INJECT_TOUCH_EVENT: u8 = 2;
/// INJECT_SCROLL_EVENT：不使用（滚轮走 UHID）。
pub const MSG_INJECT_SCROLL_EVENT: u8 = 3;
/// BACK_OR_SCREEN_ON：按键/亮屏。
pub const MSG_BACK_OR_SCREEN_ON: u8 = 4;
/// SET_CLIPBOARD：直接写平板剪贴板，不敲键盘（6.8）。
pub const MSG_SET_CLIPBOARD: u8 = 9;
/// UHID_CREATE：创建 UHID 设备。
pub const MSG_UHID_CREATE: u8 = 12;
/// UHID_INPUT：投递一条 HID 报告。
pub const MSG_UHID_INPUT: u8 = 13;
/// UHID_DESTROY：销毁 UHID 设备。
pub const MSG_UHID_DESTROY: u8 = 14;
/// OPEN_HARD_KEYBOARD_SETTINGS：拉起系统"物理键盘布局"设置页（6.7.4）。
pub const MSG_OPEN_HARD_KEYBOARD_SETTINGS: u8 = 15;

/// 平板 → PC 的 device message：剪贴板内容（与 scrcpy 5.0.1
/// `app/src/device_msg.h` 对齐）。
pub const DEVICE_MSG_CLIPBOARD: u8 = 0;
/// 平板 → PC 的 device message：剪贴板序列号确认。
pub const DEVICE_MSG_ACK_CLIPBOARD: u8 = 1;
/// 平板 → PC 的 device message：UHID 输出（系统把报告回送给宿主）。
pub const DEVICE_MSG_UHID_OUTPUT: u8 = 2;
/// scrcpy 对单条 device message 的大小上限（256KB）。
pub const DEVICE_MSG_MAX_SIZE: usize = 1 << 18;

// android.view.KeyEvent.ACTION_* / MotionEvent.ACTION_*
/// `ACTION_DOWN`。
pub const ACTION_DOWN: i64 = 0;
/// `ACTION_UP`。
pub const ACTION_UP: i64 = 1;

// Android KeyEvent 的常用键码（发送端与 probe 用）
/// `KEYCODE_BACK`。
pub const KEYCODE_BACK: i64 = 4;
/// `KEYCODE_ENTER`。
pub const KEYCODE_ENTER: i64 = 66;
/// `KEYCODE_DEL`（退格）。
pub const KEYCODE_DEL: i64 = 67;

// ---------------------------------------------------------------------------
// 错误（Python 抛 ValueError）
// ---------------------------------------------------------------------------

/// 控制协议封包/解包错误；对应 Python 的 `ValueError`。
///
/// Python 里 `bytes.decode('utf-8')` 抛的 `UnicodeDecodeError` 也是 `ValueError`
/// 的子类，所以非法 UTF-8 同样走这个枚举（[ProtocolError::InvalidUtf8]）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// 无符号 8 位字段越界。
    #[error("{field} 超出 u8 范围 0..255: {value}")]
    U8OutOfRange {
        /// 字段名（与 Python 的错误文本一致）。
        field: &'static str,
        /// 实际值。
        value: i64,
    },
    /// 无符号 16 位字段越界。
    #[error("{field} 超出 u16 范围 0..65535: {value}")]
    U16OutOfRange {
        /// 字段名（与 Python 的错误文本一致）。
        field: &'static str,
        /// 实际值。
        value: i64,
    },
    /// 无符号 32 位字段越界。
    #[error("{field} 超出 u32 范围: {value}")]
    U32OutOfRange {
        /// 字段名（与 Python 的错误文本一致）。
        field: &'static str,
        /// 实际值。
        value: i64,
    },
    /// UHID_CREATE 的 name 不能为空。
    #[error("UHID name 不能为空")]
    EmptyUhidName,
    /// UHID_CREATE 的 desc 不能为空。
    #[error("UHID 描述符不能为空")]
    EmptyUhidDescriptor,
    /// UHID_INPUT 的 data 不能为空。
    #[error("UHID_INPUT 的 data 不能为空")]
    EmptyUhidInput,
    /// 平板剪贴板消息超过 scrcpy 的 256KB 上限。
    #[error("平板剪贴板消息过长: {length} 字节")]
    ClipboardTooLong {
        /// 消息声明的长度（含异常值本身，便于排查）。
        length: u32,
    },
    /// 未知的 device message 类型：流已无法可靠对齐，必须断开重连。
    #[error("未知平板设备消息 type={msg_type}")]
    UnknownDeviceMessage {
        /// 首字节（未知类型）。
        msg_type: u8,
    },
    /// 剪贴板文本不是合法 UTF-8。
    #[error("平板剪贴板消息不是合法 UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
}

// ---------------------------------------------------------------------------
// 字段校验（Python 的 _u8 / _u16 / _u32）
// ---------------------------------------------------------------------------

/// 校验并转换 u8 字段。
fn u8_field(value: i64, field: &'static str) -> Result<u8, ProtocolError> {
    if !(0..=0xFF).contains(&value) {
        return Err(ProtocolError::U8OutOfRange { field, value });
    }
    Ok(value as u8)
}

/// 校验并转换 u16 字段。
fn u16_field(value: i64, field: &'static str) -> Result<u16, ProtocolError> {
    if !(0..=0xFFFF).contains(&value) {
        return Err(ProtocolError::U16OutOfRange { field, value });
    }
    Ok(value as u16)
}

/// 校验并转换 u32 字段。
fn u32_field(value: i64, field: &'static str) -> Result<u32, ProtocolError> {
    if !(0..=i64::from(u32::MAX)).contains(&value) {
        return Err(ProtocolError::U32OutOfRange { field, value });
    }
    Ok(value as u32)
}

/// 校验并转换"字节长度"这类 usize 字段（超 u32 时按 u32 越界报错）。
fn u32_len_field(len: usize, field: &'static str) -> Result<u32, ProtocolError> {
    match u32::try_from(len) {
        Ok(value) => Ok(value),
        Err(_) => Err(ProtocolError::U32OutOfRange {
            field,
            value: len as i64,
        }),
    }
}

// ---------------------------------------------------------------------------
// 平板回传消息
// ---------------------------------------------------------------------------

/// 一条完整的平板回传消息；只保留对应类型需要的字段。
///
/// 字段与 Python 的 `DeviceMessage` 一一对应（Rust 里 `type` 是关键字，故改名
/// [`DeviceMessage::msg_type`]）。未使用的字段保持默认值（空串 / 0 / 空字节）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceMessage {
    /// 消息类型（[DEVICE_MSG_CLIPBOARD] / [DEVICE_MSG_ACK_CLIPBOARD] /
    /// [DEVICE_MSG_UHID_OUTPUT]）。
    pub msg_type: u8,
    /// 剪贴板文本（type 0）。
    pub text: String,
    /// 剪贴板序列号（type 1）。
    pub sequence: u64,
    /// UHID 设备 id（type 2）。
    pub uhid_id: u16,
    /// UHID 输出负载（type 2）。
    pub data: Vec<u8>,
}

impl DeviceMessage {
    /// 平板剪贴板回传（type 0）。
    pub fn clipboard(text: impl Into<String>) -> Self {
        Self {
            msg_type: DEVICE_MSG_CLIPBOARD,
            text: text.into(),
            ..Self::default()
        }
    }

    /// 剪贴板序列号确认（type 1）。
    pub fn ack(sequence: u64) -> Self {
        Self {
            msg_type: DEVICE_MSG_ACK_CLIPBOARD,
            sequence,
            ..Self::default()
        }
    }

    /// UHID 输出（type 2）。
    pub fn uhid_output(uhid_id: u16, data: impl Into<Vec<u8>>) -> Self {
        Self {
            msg_type: DEVICE_MSG_UHID_OUTPUT,
            uhid_id,
            data: data.into(),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// INJECT_KEYCODE (type 0)
// ---------------------------------------------------------------------------

/// 降级路径 / 兜底键（8.2 的 `send_keycode`）。
///
/// 载荷：`action(1) keycode(4) repeat(4) metaState(4)`，大端。
/// Python 的 `repeat` / `meta_state` 默认 0，Rust 无默认参数，调用方显式传 0。
pub fn inject_keycode(
    action: i64,
    keycode: i64,
    repeat: i64,
    meta_state: i64,
) -> Result<Vec<u8>, ProtocolError> {
    let action = u8_field(action, "action")?;
    let keycode = u32_field(keycode, "keycode")?;
    let repeat = u32_field(repeat, "repeat")?;
    let meta_state = u32_field(meta_state, "meta_state")?;

    let mut out = Vec::with_capacity(14);
    out.push(MSG_INJECT_KEYCODE);
    out.push(action);
    out.extend_from_slice(&keycode.to_be_bytes());
    out.extend_from_slice(&repeat.to_be_bytes());
    out.extend_from_slice(&meta_state.to_be_bytes());
    Ok(out)
}

/// UTF-8 直注入（6.7.4 中文备选路径）。
///
/// 载荷：`len(4) utf8(len)`。长度为**字节**数，不是字符数。
pub fn inject_text(text: &str) -> Result<Vec<u8>, ProtocolError> {
    let payload = text.as_bytes();
    let length = u32_len_field(payload.len(), "text 长度")?;

    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(MSG_INJECT_TEXT);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// 直接写平板剪贴板（type 9，6.8）。
///
/// 载荷：`sequence(8) paste(1) len(4) utf8(len)`，大端。`paste` 恒为 0：
/// 只写剪贴板、不触发粘贴，也不产生任何按键事件。
///
/// 和 [`inject_text`] 的区别是它**不会敲键盘**：服务端调
/// `ClipboardManager.setPrimaryClip()`，平板当前 App 收不到任何按键，
/// 所以可以安全地在接管瞬间与每次复制时自动同步。
///
/// Python 的 `sequence` 默认 0 且允许任意 int（越界抛 `ValueError`）；Rust 用
/// `u64` 表达同一取值范围，负数在类型层面就无法传入。
pub fn set_clipboard(text: &str, sequence: u64) -> Result<Vec<u8>, ProtocolError> {
    let payload = text.as_bytes();
    let length = u32_len_field(payload.len(), "text 长度")?;

    let mut out = Vec::with_capacity(14 + payload.len());
    out.push(MSG_SET_CLIPBOARD);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.push(0); // paste = 0：只写剪贴板，不触发粘贴
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// 从接收缓冲区解出一条 device message，返回 `(消息, 消费字节数)`。
///
/// 数据不足时返回 `(None, 0)`，调用方保留缓冲区等待下一次 `recv()`。
/// 未知类型或超出 scrcpy 256KB 上限时返回 `Err`（Python 抛 `ValueError`）：
/// 流已无法可靠对齐，应由连接层断开并走既有重连流程。
pub fn deserialize_device_message(
    buffer: &[u8],
) -> Result<(Option<DeviceMessage>, usize), ProtocolError> {
    let Some((&msg_type, body)) = buffer.split_first() else {
        return Ok((None, 0));
    };

    match msg_type {
        DEVICE_MSG_CLIPBOARD => {
            // 头：type(1) len(4)
            let Some(length_bytes) = body.first_chunk::<4>() else {
                return Ok((None, 0));
            };
            let length = u32::from_be_bytes(*length_bytes) as usize;
            // scrcpy 的 256KB 上限按整条消息算（含 5 字节头）。
            if length > DEVICE_MSG_MAX_SIZE - 5 {
                return Err(ProtocolError::ClipboardTooLong {
                    length: length as u32,
                });
            }
            let Some(payload) = body.get(4..4 + length) else {
                return Ok((None, 0));
            };
            let text = std::str::from_utf8(payload)?;
            Ok((
                Some(DeviceMessage::clipboard(text.to_string())),
                5 + length,
            ))
        }
        DEVICE_MSG_ACK_CLIPBOARD => {
            // 头：type(1) sequence(8)
            let Some(sequence_bytes) = body.first_chunk::<8>() else {
                return Ok((None, 0));
            };
            let sequence = u64::from_be_bytes(*sequence_bytes);
            Ok((Some(DeviceMessage::ack(sequence)), 9))
        }
        DEVICE_MSG_UHID_OUTPUT => {
            // 头：type(1) id(2) len(2)
            let Some(header) = body.first_chunk::<4>() else {
                return Ok((None, 0));
            };
            let uhid_id = u16::from_be_bytes([header[0], header[1]]);
            let length = u16::from_be_bytes([header[2], header[3]]) as usize;
            let Some(payload) = body.get(4..4 + length) else {
                return Ok((None, 0));
            };
            Ok((
                Some(DeviceMessage::uhid_output(uhid_id, payload)),
                5 + length,
            ))
        }
        _ => Err(ProtocolError::UnknownDeviceMessage { msg_type }),
    }
}

/// `BACK_OR_SCREEN_ON`（type 4），载荷 `action(1)`。
pub fn back_or_screen_on(action: i64) -> Result<Vec<u8>, ProtocolError> {
    let action = u8_field(action, "action")?;
    Ok(vec![MSG_BACK_OR_SCREEN_ON, action])
}

// ---------------------------------------------------------------------------
// UHID（type 12 / 13 / 14）
// ---------------------------------------------------------------------------

/// UHID_CREATE（type 12）。
///
/// 布局严格按 9.2/9.3：`id(2) vendor(2) product(2) nameLen(1) name descLen(2) desc`。
/// `nameLen` 是 u8（最长 255），`descLen` 是 u16。
pub fn uhid_create(
    uhid_id: i64,
    vendor: i64,
    product: i64,
    name: &[u8],
    desc: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    if name.is_empty() {
        return Err(ProtocolError::EmptyUhidName);
    }
    if desc.is_empty() {
        return Err(ProtocolError::EmptyUhidDescriptor);
    }
    // 校验顺序与 Python 的 struct.pack 参数求值顺序一致：
    // id -> vendor -> product -> name 长度 -> desc 长度（desc 长度最后）。
    let uhid_id = u16_field(uhid_id, "uhid_id")?;
    let vendor = u16_field(vendor, "vendor")?;
    let product = u16_field(product, "product")?;
    let name_len = u8_field(name.len() as i64, "name 长度")?;
    let desc_len = u16_field(desc.len() as i64, "desc 长度")?;

    let mut out = Vec::with_capacity(8 + name.len() + 2 + desc.len());
    out.push(MSG_UHID_CREATE);
    out.extend_from_slice(&uhid_id.to_be_bytes());
    out.extend_from_slice(&vendor.to_be_bytes());
    out.extend_from_slice(&product.to_be_bytes());
    out.push(name_len);
    out.extend_from_slice(name);
    out.extend_from_slice(&desc_len.to_be_bytes());
    out.extend_from_slice(desc);
    Ok(out)
}

/// UHID_INPUT（type 13）：`id(2) len(2) data(len)`。
pub fn uhid_input(uhid_id: i64, data: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if data.is_empty() {
        return Err(ProtocolError::EmptyUhidInput);
    }
    let uhid_id = u16_field(uhid_id, "uhid_id")?;
    let length = u16_field(data.len() as i64, "data 长度")?;

    let mut out = Vec::with_capacity(5 + data.len());
    out.push(MSG_UHID_INPUT);
    out.extend_from_slice(&uhid_id.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// UHID_DESTROY（type 14）：`id(2)`。
pub fn uhid_destroy(uhid_id: i64) -> Result<Vec<u8>, ProtocolError> {
    let uhid_id = u16_field(uhid_id, "uhid_id")?;
    let mut out = Vec::with_capacity(3);
    out.push(MSG_UHID_DESTROY);
    out.extend_from_slice(&uhid_id.to_be_bytes());
    Ok(out)
}

/// 拉起系统"物理键盘布局"设置页（6.7.4），无载荷。
pub fn open_hard_keyboard_settings() -> Vec<u8> {
    vec![MSG_OPEN_HARD_KEYBOARD_SETTINGS]
}

// ---------------------------------------------------------------------------
// 便捷封装：把 UHID 报告包成 UHID_INPUT
// ---------------------------------------------------------------------------

/// 把 HID 报告（4/8 字节）包成 `UHID_INPUT`（发送热路径）。
pub fn uhid_report(uhid_id: i64, report: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    uhid_input(uhid_id, report)
}


// ---------------------------------------------------------------------------
// 单测：逐条对应 tests/test_protocol.py（协议部分），外加半包/截断向量
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::uhid::{
        mouse_report, HID_KEYBOARD_DESCRIPTOR, HID_KEYBOARD_NAME, HID_KEYBOARD_PRODUCT_ID,
        HID_MOUSE_DESCRIPTOR, HID_MOUSE_NAME, HID_MOUSE_PRODUCT_ID, HID_VENDOR_ID, UHID_KEYBOARD_ID,
        UHID_MOUSE_ID,
    };

    /// 拼一条剪贴板回传消息（type 0）。
    fn clipboard_packet(text: &str) -> Vec<u8> {
        let payload = text.as_bytes();
        let mut packet = vec![DEVICE_MSG_CLIPBOARD];
        packet.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    // -- INJECT_KEYCODE / INJECT_TEXT / BACK_OR_SCREEN_ON -------------------

    #[test]
    fn inject_keycode_down_back_golden() {
        // type=0 action=0 keycode=4 repeat=0 meta=0，共 14 字节
        let packet = inject_keycode(ACTION_DOWN, KEYCODE_BACK, 0, 0).unwrap();
        assert_eq!(
            packet,
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(packet.len(), 14);
    }

    #[test]
    fn inject_keycode_up_enter_with_meta_golden() {
        let packet = inject_keycode(ACTION_UP, KEYCODE_ENTER, 1, 0x1000).unwrap();
        assert_eq!(
            packet,
            vec![0, 1, 0, 0, 0, 0x42, 0, 0, 0, 1, 0, 0, 0x10, 0]
        );
        assert_eq!(packet.len(), 14);
    }

    #[test]
    fn inject_text_uses_byte_length_not_char_count() {
        assert_eq!(
            inject_text("abc").unwrap(),
            b"\x01\x00\x00\x00\x03abc".to_vec()
        );
        // 中文 2 字符 = 6 字节 UTF-8
        assert_eq!(
            inject_text("中文").unwrap(),
            b"\x01\x00\x00\x00\x06\xe4\xb8\xad\xe6\x96\x87".to_vec()
        );
        assert_eq!(inject_text("").unwrap(), b"\x01\x00\x00\x00\x00".to_vec());
    }

    #[test]
    fn back_or_screen_on_is_two_bytes() {
        assert_eq!(back_or_screen_on(ACTION_UP).unwrap(), b"\x04\x01".to_vec());
        assert_eq!(open_hard_keyboard_settings(), b"\x0f".to_vec());
    }

    // -- UHID_CREATE / INPUT / DESTROY -------------------------------------

    #[test]
    fn uhid_create_mouse_layout_golden() {
        let packet = uhid_create(
            i64::from(UHID_MOUSE_ID),
            i64::from(HID_VENDOR_ID),
            i64::from(HID_MOUSE_PRODUCT_ID),
            HID_MOUSE_NAME,
            HID_MOUSE_DESCRIPTOR,
        )
        .unwrap();
        // type(1) id(2) vendor(2) product(2) nameLen(1)
        assert_eq!(
            &packet[..8],
            &[0x0C, 0x00, 0x01, 0x1D, 0x6B, 0x00, 0x01, 0x0D][..]
        );
        assert_eq!(&packet[8..21], b"PadLend Mouse");
        assert_eq!(&packet[21..23], &[0x00, 0x3e][..]); // descLen u16 = 62
        assert_eq!(&packet[23..], HID_MOUSE_DESCRIPTOR);
        assert_eq!(HID_MOUSE_DESCRIPTOR.len(), 62);
        assert_eq!(packet.len(), 1 + 2 + 2 + 2 + 1 + 13 + 2 + 62);
    }

    #[test]
    fn uhid_create_keyboard_layout_golden() {
        let packet = uhid_create(
            i64::from(UHID_KEYBOARD_ID),
            i64::from(HID_VENDOR_ID),
            i64::from(HID_KEYBOARD_PRODUCT_ID),
            HID_KEYBOARD_NAME,
            HID_KEYBOARD_DESCRIPTOR,
        )
        .unwrap();
        assert_eq!(
            &packet[..8],
            &[0x0C, 0x00, 0x02, 0x1D, 0x6B, 0x00, 0x02, 0x10][..]
        );
        assert_eq!(&packet[8..24], b"PadLend Keyboard");
        assert_eq!(&packet[24..26], &[0x00, 0x2d][..]); // descLen u16 = 45
        assert_eq!(&packet[26..], HID_KEYBOARD_DESCRIPTOR);
        assert_eq!(HID_KEYBOARD_DESCRIPTOR.len(), 45);
        assert_eq!(packet.len(), 1 + 2 + 2 + 2 + 1 + 16 + 2 + 45);
    }

    #[test]
    fn uhid_input_and_destroy_golden() {
        let report = mouse_report(0, 1, 0, 0);
        let packet = uhid_input(i64::from(UHID_MOUSE_ID), &report).unwrap();
        assert_eq!(
            packet,
            vec![0x0D, 0x00, 0x01, 0x00, 0x04, 0x00, 0x01, 0x00, 0x00]
        );
        // uhid_report 就是 uhid_input 的别名
        assert_eq!(uhid_report(i64::from(UHID_MOUSE_ID), &report).unwrap(), packet);
        assert_eq!(
            uhid_destroy(i64::from(UHID_MOUSE_ID)).unwrap(),
            b"\x0e\x00\x01".to_vec()
        );
        assert_eq!(
            uhid_destroy(i64::from(UHID_KEYBOARD_ID)).unwrap(),
            b"\x0e\x00\x02".to_vec()
        );
    }

    // -- SET_CLIPBOARD -----------------------------------------------------

    #[test]
    fn set_clipboard_packs_sequence_and_text() {
        let message = set_clipboard("ab", 7).unwrap();
        assert_eq!(message[0], MSG_SET_CLIPBOARD);
        assert_eq!(MSG_SET_CLIPBOARD, 9);
        assert_eq!(
            u64::from_be_bytes(message[1..9].try_into().unwrap()),
            7
        );
        assert_eq!(message[9], 0); // paste 恒为 0
        assert_eq!(u32::from_be_bytes(message[10..14].try_into().unwrap()), 2);
        assert_eq!(&message[14..], b"ab");
    }

    #[test]
    fn set_clipboard_defaults_to_sequence_zero() {
        // Python 的默认 sequence=0
        let message = set_clipboard("x", 0).unwrap();
        assert_eq!(message[9], 0);
        assert_eq!(u64::from_be_bytes(message[1..9].try_into().unwrap()), 0);
    }

    // -- device message 解包 -----------------------------------------------

    #[test]
    fn deserialize_clipboard_waits_for_fragment_and_decodes_utf8() {
        let packet = clipboard_packet("平板复制");
        assert_eq!(deserialize_device_message(&packet[..3]).unwrap(), (None, 0));
        assert_eq!(
            deserialize_device_message(&packet[..packet.len() - 1]).unwrap(),
            (None, 0)
        );

        let mut buffer = packet.clone();
        buffer.extend_from_slice(b"next");
        let (message, consumed) = deserialize_device_message(&buffer).unwrap();
        assert_eq!(message, Some(DeviceMessage::clipboard("平板复制")));
        assert_eq!(consumed, packet.len());
    }

    #[test]
    fn deserialize_ack_and_uhid_output() {
        let mut ack = vec![DEVICE_MSG_ACK_CLIPBOARD];
        ack.extend_from_slice(&7u64.to_be_bytes());
        let (message, consumed) = deserialize_device_message(&ack).unwrap();
        assert_eq!(message, Some(DeviceMessage::ack(7)));
        assert_eq!(consumed, 9);

        let uhid = [0x02, 0x00, 0x02, 0x00, 0x02, 0xab, 0xcd];
        let (message, consumed) = deserialize_device_message(&uhid).unwrap();
        assert_eq!(message, Some(DeviceMessage::uhid_output(2, [0xab, 0xcd])));
        assert_eq!(consumed, 7);
    }

    #[test]
    fn deserialize_rejects_unknown_or_oversized_message() {
        assert_eq!(
            deserialize_device_message(&[0xff]).unwrap_err(),
            ProtocolError::UnknownDeviceMessage { msg_type: 0xff }
        );

        let mut oversized = vec![DEVICE_MSG_CLIPBOARD];
        oversized.extend_from_slice(&(DEVICE_MSG_MAX_SIZE as u32).to_be_bytes());
        assert_eq!(
            deserialize_device_message(&oversized).unwrap_err(),
            ProtocolError::ClipboardTooLong {
                length: DEVICE_MSG_MAX_SIZE as u32
            }
        );
        assert_eq!(
            deserialize_device_message(&oversized).unwrap_err().to_string(),
            "平板剪贴板消息过长: 262144 字节"
        );
    }

    #[test]
    fn deserialize_uhid_output_waits_for_full_header_and_payload() {
        let full = [0x02, 0x00, 0x02, 0x00, 0x02, 0xab, 0xcd];
        assert_eq!(deserialize_device_message(&full[..3]).unwrap(), (None, 0));
        assert_eq!(deserialize_device_message(&full[..6]).unwrap(), (None, 0));

        let mut buffer = full.to_vec();
        buffer.extend_from_slice(b"tail");
        let (message, consumed) = deserialize_device_message(&buffer).unwrap();
        assert_eq!(message, Some(DeviceMessage::uhid_output(2, [0xab, 0xcd])));
        assert_eq!(consumed, full.len());
    }

    #[test]
    fn deserialize_ack_waits_for_full_sequence() {
        let mut full = vec![DEVICE_MSG_ACK_CLIPBOARD];
        full.extend_from_slice(&7u64.to_be_bytes());
        assert_eq!(
            deserialize_device_message(&full[..full.len() - 1]).unwrap(),
            (None, 0)
        );
        let (message, consumed) = deserialize_device_message(&full).unwrap();
        assert_eq!(message, Some(DeviceMessage::ack(7)));
        assert_eq!(consumed, 9);
    }

    #[test]
    fn deserialize_empty_buffer_waits() {
        assert_eq!(deserialize_device_message(&[]).unwrap(), (None, 0));
    }

    #[test]
    fn deserialize_every_prefix_of_each_type_waits() {
        // "半包/截断"：任何不完整的消息都必须返回 (None, 0)，且不得 panic。
        let clipboard = clipboard_packet("半包");
        let mut ack = vec![DEVICE_MSG_ACK_CLIPBOARD];
        ack.extend_from_slice(&7u64.to_be_bytes());
        let uhid = vec![0x02, 0x00, 0x02, 0x00, 0x02, 0xab, 0xcd];

        for packet in [&clipboard, &ack, &uhid] {
            for end in 0..packet.len() {
                let (message, consumed) = deserialize_device_message(&packet[..end]).unwrap();
                assert!(message.is_none(), "前缀 {end} 字节不应解出消息");
                assert_eq!(consumed, 0);
            }
            // 完整消息本身必须解出来
            let (message, consumed) = deserialize_device_message(packet).unwrap();
            assert!(message.is_some());
            assert_eq!(consumed, packet.len());
        }
    }

    #[test]
    fn deserialize_clipboard_rejects_invalid_utf8() {
        let mut packet = vec![DEVICE_MSG_CLIPBOARD];
        packet.extend_from_slice(&2u32.to_be_bytes());
        packet.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            deserialize_device_message(&packet).unwrap_err(),
            ProtocolError::InvalidUtf8(_)
        ));
    }

    #[test]
    fn deserialize_clipboard_size_limit_is_inclusive() {
        // 上限比较是"大于"，正好 MAX-5 视为合法（只是数据还不够）
        let mut boundary = vec![DEVICE_MSG_CLIPBOARD];
        boundary.extend_from_slice(&((DEVICE_MSG_MAX_SIZE - 5) as u32).to_be_bytes());
        assert_eq!(deserialize_device_message(&boundary).unwrap(), (None, 0));

        let mut over = vec![DEVICE_MSG_CLIPBOARD];
        over.extend_from_slice(&((DEVICE_MSG_MAX_SIZE - 4) as u32).to_be_bytes());
        assert!(matches!(
            deserialize_device_message(&over).unwrap_err(),
            ProtocolError::ClipboardTooLong { .. }
        ));
    }

    // -- 参数校验：宁可本地 Err，也不要发出错位报文 ------------------------

    #[test]
    fn uhid_id_must_fit_u16() {
        for value in [-1, 70000] {
            assert!(matches!(
                uhid_destroy(value).unwrap_err(),
                ProtocolError::U16OutOfRange {
                    field: "uhid_id",
                    ..
                }
            ));
        }
        assert_eq!(
            uhid_destroy(70000).unwrap_err().to_string(),
            "uhid_id 超出 u16 范围 0..65535: 70000"
        );
    }

    #[test]
    fn uhid_create_rejects_bad_inputs() {
        // name 为空
        assert_eq!(
            uhid_create(
                1,
                i64::from(HID_VENDOR_ID),
                1,
                b"",
                HID_MOUSE_DESCRIPTOR
            )
            .unwrap_err(),
            ProtocolError::EmptyUhidName
        );
        assert_eq!(
            uhid_create(1, i64::from(HID_VENDOR_ID), 1, b"", HID_MOUSE_DESCRIPTOR)
                .unwrap_err()
                .to_string(),
            "UHID name 不能为空"
        );
        // name 过长（nameLen 是 u8）
        assert_eq!(
            uhid_create(1, i64::from(HID_VENDOR_ID), 1, &[b'x'; 256], HID_MOUSE_DESCRIPTOR)
                .unwrap_err(),
            ProtocolError::U8OutOfRange {
                field: "name 长度",
                value: 256
            }
        );
        // desc 为空
        assert_eq!(
            uhid_create(1, i64::from(HID_VENDOR_ID), 1, HID_MOUSE_NAME, b"").unwrap_err(),
            ProtocolError::EmptyUhidDescriptor
        );
        // 255 字节的 name 仍然合法（u8 上界）
        let name = [b'x'; 255];
        let packet = uhid_create(1, i64::from(HID_VENDOR_ID), 1, &name, HID_MOUSE_DESCRIPTOR)
            .unwrap();
        assert_eq!(packet[7], 0xFF);
    }

    #[test]
    fn uhid_input_rejects_empty_and_oversized() {
        assert_eq!(
            uhid_input(1, b"").unwrap_err(),
            ProtocolError::EmptyUhidInput
        );
        let oversized = vec![b'x'; 0x10000];
        assert_eq!(
            uhid_input(1, &oversized).unwrap_err(),
            ProtocolError::U16OutOfRange {
                field: "data 长度",
                value: 0x10000
            }
        );
        // 65535 字节是 u16 上界，仍合法
        let max = vec![0u8; 0xFFFF];
        assert!(uhid_input(1, &max).is_ok());
    }

    #[test]
    fn keycode_and_text_validation() {
        assert!(matches!(
            inject_keycode(-1, KEYCODE_BACK, 0, 0).unwrap_err(),
            ProtocolError::U8OutOfRange {
                field: "action",
                ..
            }
        ));
        assert!(matches!(
            inject_keycode(ACTION_DOWN, -1, 0, 0).unwrap_err(),
            ProtocolError::U32OutOfRange {
                field: "keycode",
                ..
            }
        ));
        assert!(matches!(
            inject_keycode(ACTION_DOWN, KEYCODE_BACK, -1, 0).unwrap_err(),
            ProtocolError::U32OutOfRange {
                field: "repeat",
                ..
            }
        ));
        assert!(matches!(
            back_or_screen_on(-1).unwrap_err(),
            ProtocolError::U8OutOfRange { field: "action", .. }
        ));
        // 错误文本与 Python 一致
        assert_eq!(
            inject_keycode(-1, KEYCODE_BACK, 0, 0).unwrap_err().to_string(),
            "action 超出 u8 范围 0..255: -1"
        );
    }

    #[test]
    fn device_message_fields_default_to_empty() {
        let message = DeviceMessage::ack(3);
        assert_eq!(message.msg_type, DEVICE_MSG_ACK_CLIPBOARD);
        assert_eq!(message.sequence, 3);
        assert!(message.text.is_empty());
        assert_eq!(message.uhid_id, 0);
        assert!(message.data.is_empty());
    }
}
