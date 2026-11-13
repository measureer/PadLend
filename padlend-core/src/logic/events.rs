//! 跨线程事件类型（对应 `core/events.py`，设计文档 8.1 的冻结契约）。
//!
//! 这两个结构是**唯一**允许跨线程传递的输入载体：
//! Raw Input 线程 → Router、键盘钩子线程 → Router。
//! 禁止在事件里携带 socket/窗口句柄等可变资源；回调内只入队（铁律 3）。

use serde::{Deserialize, Serialize};

/// 一次鼠标原始事件（Raw Input，设备计数，未经系统加速）。
///
/// - `dx`/`dy`：相对位移（设备计数）；绝对设备已被归一化成差分。
/// - `buttons`：HID 语义的按键位图（见 `constants::MOUSE_BTN_*`）。
/// - `wheel`：有符号滚轮量，120 = 一格。
/// - `absolute`：原始设备是否为绝对设备（绘图板 / RDP），仅诊断用。
/// - `ts`：`Instant` 起点的时间戳（秒），用于延迟统计。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RawDelta {
    pub dx: i32,
    pub dy: i32,
    pub buttons: i32,
    pub wheel: i32,
    pub absolute: bool,
    pub ts: f64,
}

impl Default for RawDelta {
    fn default() -> Self {
        Self {
            dx: 0,
            dy: 0,
            buttons: 0,
            wheel: 0,
            absolute: false,
            ts: 0.0,
        }
    }
}

impl RawDelta {
    pub fn new(dx: i32, dy: i32, buttons: i32, wheel: i32, absolute: bool, ts: f64) -> Self {
        Self {
            dx,
            dy,
            buttons,
            wheel,
            absolute,
            ts,
        }
    }

    /// 有位移或滚轮（只有按键变化时 Router 仍要处理，所以按键不计入）。
    pub fn has_motion(&self) -> bool {
        self.dx != 0 || self.dy != 0 || self.wheel != 0
    }
}

/// 一次键盘事件（`WH_KEYBOARD_LL`）。
///
/// `vk` 为虚拟键码，`scan` 为扫描码（含扩展位，见设计文档 6.2.2），
/// `is_extended` 用于区分左右修饰键，`down` 表示按下。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub vk: i32,
    pub scan: i32,
    pub is_extended: bool,
    pub down: bool,
    pub ts: f64,
}

impl KeyEvent {
    pub fn new(vk: i32, scan: i32, is_extended: bool, down: bool) -> Self {
        Self {
            vk,
            scan,
            is_extended,
            down,
            ts: 0.0,
        }
    }

    pub fn key_down(vk: i32) -> Self {
        Self::new(vk, 0, false, true)
    }

    pub fn key_up(vk: i32) -> Self {
        Self::new(vk, 0, false, false)
    }

    pub fn with_timestamp(mut self, ts: f64) -> Self {
        self.ts = ts;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_delta_defaults_match_python_dataclass() {
        let delta = RawDelta::default();
        assert_eq!(
            (delta.dx, delta.dy, delta.buttons, delta.wheel, delta.absolute),
            (0, 0, 0, 0, false)
        );
        assert_eq!(delta.ts, 0.0);
    }

    #[test]
    fn raw_delta_detects_motion() {
        assert!(!RawDelta::default().has_motion());
        // 只有按键变化不算位移：Router 仍会处理这条事件。
        assert!(!RawDelta::new(0, 0, 1, 0, false, 0.0).has_motion());
        assert!(RawDelta::new(3, 0, 0, 0, false, 0.0).has_motion());
        assert!(RawDelta::new(0, 0, 0, 120, false, 0.0).has_motion());
    }

    #[test]
    fn key_event_constructors_keep_down_semantics() {
        assert!(KeyEvent::key_down(0x41).down);
        assert!(!KeyEvent::key_up(0x41).down);
        assert_eq!(KeyEvent::new(0x41, 0x1E, false, true).scan, 0x1E);
        assert_eq!(KeyEvent::key_down(0x41).with_timestamp(1.5).ts, 1.5);
    }
}
