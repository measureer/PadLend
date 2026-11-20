//! 物理鼠标按键状态（`GetAsyncKeyState`），用来给 Raw Input 的粘性位图做校正。
//!
//! ## 为什么需要这个模块
//!
//! 业务侧的按键状态来自 `service::engine` 的**粘性位图**：Raw Input 的
//! `RI_MOUSE_BUTTON_n_DOWN` 置位、`RI_MOUSE_BUTTON_n_UP` 清位。这条链路没有自愈能力——
//! 锁屏、休眠唤醒、设备重连时输入桌面会短暂收不到报文，此时发生的「抬起」永远不会送达，
//! 位图就永久停在按下状态。后果是连锁的：
//!
//! * `Router::handle_captured`（`logic/router.rs`）的出口判定是「`buttons != 0` 或不在
//!   入口边就清零 overflow 直接返回」——位图卡住之后出口永远不评估，鼠标推进平板就再也
//!   穿不回电脑；
//! * `Router::enter_captured` 会因为 `buttons != 0` 把平板侧按键静默
//!   （`neutral_until_release`），于是平板上连点击都没有反应；
//! * 只有重启引擎（位图归零）才能恢复。
//!
//! 所以这里补一个**物理真值**来源：`GetAsyncKeyState` 的高位（`0x8000`）表示按键此刻真的
//! 按住。它只用于校正粘性位图（[`reconcile_sticky`]），不进位移、滚轮或热路径。
//!
//! ## 约定
//!
//! * 校正**只清位、绝不造按下**：物理上没按住而位图里有 → 清掉；物理上按住而位图里没有 →
//!   保持没有（那是报文还没到的正常时序，等 Raw Input 自己置位）。
//! * 不抛异常：查询失败按「都没按住」处理——宁可多清一次，也不让位图永久卡死。
//!
//! ## 可注入后端
//!
//! 与 `win/dpi.rs` 一致：Win32 调用收在 [`ButtonsApi`] 后面，生产实现是
//! [`Win32ButtonsApi`]，测试注入假后端（`#[cfg(test)]` 的 `FakeButtonsApi`）验证位图拼装。

use std::sync::OnceLock;

use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

use crate::constants::{
    MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE1, MOUSE_BTN_SIDE2,
};

/// `VK_LBUTTON`：鼠标左键。
pub const VK_LBUTTON: i32 = 0x01;
/// `VK_RBUTTON`：鼠标右键。
pub const VK_RBUTTON: i32 = 0x02;
/// `VK_MBUTTON`：鼠标中键。
pub const VK_MBUTTON: i32 = 0x04;
/// `VK_XBUTTON1`：侧键 1（后退）。
pub const VK_XBUTTON1: i32 = 0x05;
/// `VK_XBUTTON2`：侧键 2（前进）。
pub const VK_XBUTTON2: i32 = 0x06;

/// 需要跟踪的鼠标按键：`(VK, constants::MOUSE_BTN_*)`，顺序即位图拼装顺序。
pub const BUTTON_KEYS: [(i32, u8); 5] = [
    (VK_LBUTTON, MOUSE_BTN_LEFT),
    (VK_RBUTTON, MOUSE_BTN_RIGHT),
    (VK_MBUTTON, MOUSE_BTN_MIDDLE),
    (VK_XBUTTON1, MOUSE_BTN_SIDE1),
    (VK_XBUTTON2, MOUSE_BTN_SIDE2),
];

/// 物理按键查询面（可注入，便于测试位图拼装）。
pub trait ButtonsApi: Send + Sync {
    /// 某个虚拟键此刻是否真的按住。
    fn is_down(&self, vk: i32) -> bool;
}

/// Win32 实现：`GetAsyncKeyState` 的高位就是「此刻按住」。
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32ButtonsApi;

impl ButtonsApi for Win32ButtonsApi {
    fn is_down(&self, vk: i32) -> bool {
        // 高位 0x8000 = 当前按下。低位 0x0001 表示「上次查询之后按过」，
        // 与「现在按着没有」无关，这里绝不能用它。
        let state = unsafe { GetAsyncKeyState(vk) };
        (state as u16 & 0x8000) != 0
    }
}

/// 用给定后端拼出当前物理鼠标按键位图（位与 `constants::MOUSE_BTN_*` 一致）。
pub fn collect_mask(api: &dyn ButtonsApi) -> i32 {
    let mut mask = 0i32;
    for (vk, bit) in BUTTON_KEYS {
        if api.is_down(vk) {
            mask |= i32::from(bit);
        }
    }
    mask
}

/// 进程级的生产查询：系统此刻物理按住的鼠标按键位图。
pub fn physical_buttons() -> i32 {
    static API: OnceLock<Win32ButtonsApi> = OnceLock::new();
    collect_mask(API.get_or_init(Win32ButtonsApi::default))
}

/// 用物理状态校正粘性位图：取交集，只清掉物理上没按住的位。
pub fn reconcile_sticky(sticky: i32, physical: i32) -> i32 {
    sticky & physical
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只报「这些 VK 按住」的假后端。
    struct FakeButtonsApi(Vec<i32>);

    impl ButtonsApi for FakeButtonsApi {
        fn is_down(&self, vk: i32) -> bool {
            self.0.contains(&vk)
        }
    }

    #[test]
    fn mask_covers_every_tracked_button() {
        for (vk, bit) in BUTTON_KEYS {
            assert_eq!(
                collect_mask(&FakeButtonsApi(vec![vk])),
                i32::from(bit),
                "VK 0x{vk:02X} 的位图不对"
            );
        }
    }

    #[test]
    fn mask_combines_simultaneous_buttons() {
        let mask = collect_mask(&FakeButtonsApi(vec![VK_LBUTTON, VK_XBUTTON2]));
        assert_eq!(mask, i32::from(MOUSE_BTN_LEFT | MOUSE_BTN_SIDE2));
    }

    #[test]
    fn nothing_down_is_empty_mask() {
        assert_eq!(collect_mask(&FakeButtonsApi(Vec::new())), 0);
    }

    #[test]
    fn reconcile_only_clears_bits() {
        // 卡住的左键、物理上没按住 → 清掉。
        assert_eq!(reconcile_sticky(i32::from(MOUSE_BTN_LEFT), 0), 0);
        // 真的按住 → 保留。
        assert_eq!(reconcile_sticky(0x03, 0x03), 0x03);
        // 物理上多出来的位不会凭空出现在位图里。
        assert_eq!(reconcile_sticky(0, i32::from(MOUSE_BTN_RIGHT)), 0);
        // 只清卡住的那一位，其它保持。
        assert_eq!(reconcile_sticky(0x05, 0x04), 0x04);
    }
}
