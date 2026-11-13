//! Raw Input 字节流解码（对应 `win/rawinput.py:decode_raw_input`）。
//!
//! 纯函数、无状态：M0 的跨语言对拍就喂同一批字节给它和 Python 版，比对结构体字段。

use windows::Win32::UI::Input::{RAWINPUT, RIM_TYPEMOUSE};

pub const MOUSE_MOVE_ABSOLUTE: u16 = 0x0001;
pub const RI_MOUSE_WHEEL: u16 = 0x0400;
/// usButtonFlags 里表示"按键状态变化"的位（5 个键的 down/up）。
pub const BUTTON_FLAG_MASK: u16 = 0x03FF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MousePacket {
    pub flags: u16,
    pub x: i32,
    pub y: i32,
    pub button_flags: u16,
    pub button_data: u16,
    pub device: u64,
}

impl MousePacket {
    pub fn is_absolute(&self) -> bool {
        self.flags & MOUSE_MOVE_ABSOLUTE != 0
    }

    pub fn has_wheel(&self) -> bool {
        self.button_flags & RI_MOUSE_WHEEL != 0
    }

    pub fn has_button_change(&self) -> bool {
        self.button_flags & BUTTON_FLAG_MASK != 0
    }
}

/// 把 `GetRawInputData` 的输出缓冲区解码成 `MousePacket`；非鼠标包返回 `None`。
pub fn decode_raw_input(data: &[u8]) -> Option<MousePacket> {
    let need = std::mem::size_of::<RAWINPUT>();
    if data.len() < need {
        return None;
    }
    // RAWINPUT 是 C 结构体，缓冲区不保证对齐，必须用 read_unaligned。
    let raw: RAWINPUT = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const RAWINPUT) };
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    let mouse = unsafe { raw.data.mouse };
    let buttons = unsafe { mouse.Anonymous.Anonymous };
    Some(MousePacket {
        flags: mouse.usFlags.0,
        x: mouse.lLastX,
        y: mouse.lLastY,
        button_flags: buttons.usButtonFlags,
        button_data: buttons.usButtonData,
        device: raw.header.hDevice.0 as usize as u64,
    })
}
