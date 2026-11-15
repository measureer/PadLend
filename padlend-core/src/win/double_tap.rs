//! 双击修饰键（例如「连按两下 Ctrl」）的识别与解析。
//!
//! # 为什么需要单独一套逻辑
//!
//! Windows 的 `RegisterHotKey` 只接受「修饰键 + 主键」的组合：`fsModifiers` 可以为 0，
//! 但 `vk` 必须是真正的按键，直接注册 Ctrl 这类修饰键不会按预期触发（按下 Ctrl 时系统
//! 认为是在组一个组合键，不会派发 `WM_HOTKEY`）。所以「双击左 Ctrl」只能靠低级键盘钩子
//! 自己观察按键序列，本模块只做纯逻辑部分（时间窗 + 状态机），装钩子的部分在
//! `service/engine.rs` 里接在既有 `WH_KEYBOARD_LL` 回调上。
//!
//! # 判定规则
//!
//! - 只统计**按下**事件，并且两次必须是同一个修饰键（Ctrl / Alt / Shift / Win 任选其一）；
//! - 两次间隔不超过 [`DOUBLE_TAP_WINDOW`]（400ms）；
//! - 中间只要按下任何**非修饰键**（例如 Ctrl+C 里的 C），等待状态立刻作废：
//!   否则「先按 Ctrl 再按 C 再按 Ctrl」会被当成双击，接管状态会在打字时乱跳。
//!
//! 这里不碰 Win32，测试可以完全离线跑。

use std::time::{Duration, Instant};

/// 两次按下的最大间隔。
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// 可以被「双击」的修饰键族（左右不分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierFamily {
    /// Ctrl。
    Ctrl,
    /// Alt。
    Alt,
    /// Shift。
    Shift,
    /// Win。
    Win,
}

impl ModifierFamily {
    /// 全部修饰键族（注册顺序即日志顺序）。
    pub const ALL: [ModifierFamily; 4] = [
        ModifierFamily::Ctrl,
        ModifierFamily::Alt,
        ModifierFamily::Shift,
        ModifierFamily::Win,
    ];

    /// 中文名（日志与设置页提示用）。
    pub fn label(self) -> &'static str {
        match self {
            ModifierFamily::Ctrl => "Ctrl",
            ModifierFamily::Alt => "Alt",
            ModifierFamily::Shift => "Shift",
            ModifierFamily::Win => "Win",
        }
    }

    /// 配置文本里的修饰键名 → 族。
    pub fn from_text(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(ModifierFamily::Ctrl),
            "alt" | "menu" => Some(ModifierFamily::Alt),
            "shift" => Some(ModifierFamily::Shift),
            "win" | "meta" | "super" | "cmd" | "command" => Some(ModifierFamily::Win),
            _ => None,
        }
    }

    /// 虚拟键码 → 族（左右通用写法与具体左右键都认）。
    pub fn from_vk(vk: u32) -> Option<Self> {
        match vk {
            0x11 | 0xA2 | 0xA3 => Some(ModifierFamily::Ctrl),
            0x12 | 0xA4 | 0xA5 => Some(ModifierFamily::Alt),
            0x10 | 0xA0 | 0xA1 => Some(ModifierFamily::Shift),
            0x5B | 0x5C => Some(ModifierFamily::Win),
            _ => None,
        }
    }
}

/// 解析「Ctrl+Ctrl」这种双击写法；不是双击写法返回 `None`（交给普通的组合键解析）。
pub fn parse_double_tap(text: &str) -> Option<ModifierFamily> {
    let tokens: Vec<&str> = text
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() != 2 {
        return None;
    }
    let first = ModifierFamily::from_text(tokens[0])?;
    let second = ModifierFamily::from_text(tokens[1])?;
    (first == second).then_some(first)
}

/// 双击状态机：喂按键事件，命中的时候返回是哪个修饰键被双击了。
#[derive(Debug)]
pub struct DoubleTapMatcher {
    window: Duration,
    pending: Option<(ModifierFamily, Instant)>,
}

impl Default for DoubleTapMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl DoubleTapMatcher {
    /// 用默认 400ms 时间窗。
    pub fn new() -> Self {
        Self::with_window(DOUBLE_TAP_WINDOW)
    }

    /// 自定义时间窗（测试用）。
    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            pending: None,
        }
    }

    /// 清掉等待状态（例如停止热键、重新读配置时）。
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// 喂一个按键事件；返回刚完成的「双击」。
    ///
    /// `down == false`（抬起）不参与判定：长按 Ctrl 抬起再按下也算两次，
    /// 与用户「连按两下」的直觉一致。
    pub fn feed(&mut self, vk: u32, down: bool, now: Instant) -> Option<ModifierFamily> {
        if !down {
            return None;
        }
        let Some(family) = ModifierFamily::from_vk(vk) else {
            // 普通按键（Ctrl+C 的 C、字母、方向键…）：任何等待中的双击都作废。
            self.pending = None;
            return None;
        };
        match self.pending.take() {
            Some((previous, at)) if previous == family => {
                if now.saturating_duration_since(at) <= self.window {
                    Some(family)
                } else {
                    // 超时了：这一次算新的第一次。
                    self.pending = Some((family, now));
                    None
                }
            }
            // 换了修饰键（Ctrl 之后按 Alt）：重新等这一个。
            _ => {
                self.pending = Some((family, now));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VK_LCONTROL: u32 = 0xA2;
    const VK_RCONTROL: u32 = 0xA3;
    const VK_LMENU: u32 = 0xA4;
    const VK_LSHIFT: u32 = 0xA0;
    const VK_LWIN: u32 = 0x5B;
    const VK_C: u32 = 0x43;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn parses_double_tap_text() {
        assert_eq!(parse_double_tap("Ctrl+Ctrl"), Some(ModifierFamily::Ctrl));
        assert_eq!(parse_double_tap(" control + control "), Some(ModifierFamily::Ctrl));
        assert_eq!(parse_double_tap("Alt+Alt"), Some(ModifierFamily::Alt));
        assert_eq!(parse_double_tap("Shift+Shift"), Some(ModifierFamily::Shift));
        assert_eq!(parse_double_tap("Win+Win"), Some(ModifierFamily::Win));
        assert_eq!(parse_double_tap("Ctrl+Alt"), None);
        assert_eq!(parse_double_tap("Ctrl+Ctrl+Ctrl"), None);
        assert_eq!(parse_double_tap("Ctrl+S"), None);
        assert_eq!(parse_double_tap("Ctrl"), None);
        assert_eq!(parse_double_tap(""), None);
    }

    #[test]
    fn vk_mapping_covers_both_sides() {
        assert_eq!(ModifierFamily::from_vk(0x11), Some(ModifierFamily::Ctrl));
        assert_eq!(ModifierFamily::from_vk(VK_LCONTROL), Some(ModifierFamily::Ctrl));
        assert_eq!(ModifierFamily::from_vk(VK_RCONTROL), Some(ModifierFamily::Ctrl));
        assert_eq!(ModifierFamily::from_vk(VK_LMENU), Some(ModifierFamily::Alt));
        assert_eq!(ModifierFamily::from_vk(VK_LSHIFT), Some(ModifierFamily::Shift));
        assert_eq!(ModifierFamily::from_vk(VK_LWIN), Some(ModifierFamily::Win));
        assert_eq!(ModifierFamily::from_vk(VK_C), None);
    }

    #[test]
    fn two_presses_within_window_fire() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 0)), None);
        assert_eq!(
            matcher.feed(VK_LCONTROL, true, at(base, 120)),
            Some(ModifierFamily::Ctrl)
        );
    }

    #[test]
    fn left_and_right_count_as_the_same_modifier() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 0)), None);
        assert_eq!(
            matcher.feed(VK_RCONTROL, true, at(base, 100)),
            Some(ModifierFamily::Ctrl)
        );
    }

    #[test]
    fn slow_second_press_wait_again() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 0)), None);
        // 500ms 之后再来一次：不算双击，但它成为新的第一次。
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 500)), None);
        assert_eq!(
            matcher.feed(VK_LCONTROL, true, at(base, 600)),
            Some(ModifierFamily::Ctrl)
        );
    }

    #[test]
    fn ordinary_key_cancels_pending() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 0)), None);
        // Ctrl+C：C 按下就作废，后面再按 Ctrl 不算双击。
        assert_eq!(matcher.feed(VK_C, true, at(base, 80)), None);
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 120)), None);
        assert_eq!(
            matcher.feed(VK_LCONTROL, true, at(base, 200)),
            Some(ModifierFamily::Ctrl)
        );
    }

    #[test]
    fn switching_modifier_restarts_waiting() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 0)), None);
        // Ctrl 之后按 Alt：Ctrl 的等待作废，Alt 成为新的第一次。
        assert_eq!(matcher.feed(VK_LMENU, true, at(base, 60)), None);
        assert_eq!(
            matcher.feed(VK_LMENU, true, at(base, 120)),
            Some(ModifierFamily::Alt)
        );
    }

    #[test]
    fn key_up_events_are_ignored() {
        let base = Instant::now();
        let mut matcher = DoubleTapMatcher::new();
        assert_eq!(matcher.feed(VK_LCONTROL, false, at(base, 0)), None);
        assert_eq!(matcher.feed(VK_LCONTROL, true, at(base, 50)), None);
        assert_eq!(matcher.feed(VK_LCONTROL, false, at(base, 60)), None);
        assert_eq!(
            matcher.feed(VK_LCONTROL, true, at(base, 90)),
            Some(ModifierFamily::Ctrl)
        );
    }
}
