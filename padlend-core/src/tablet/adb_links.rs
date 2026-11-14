//! 记录「PadLend 自己建立的无线 adb 连接」（1.7.2）。
//!
//! `adb connect host:port` 建起来的 TCP 链路不会随会话 teardown 消失：scrcpy 的
//! pkill、`adb forward --remove` 都只清会话自己的东西，adbd 连接一直挂在
//! adb server 里。用户看到的现象就是——点了「断开连接」、甚至退出 PadLend，
//! 平板还留在 `adb devices` 里，「无线调试」压根没断开。
//!
//! 这里只登记 PadLend **亲手建立**的连接（usb 序列号、空串一律不记），断开或
//! 退出时只关登记过的地址：用户自己在终端里 `adb connect` 的链路不归我们管，
//! 不能顺手掐掉。

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

fn registry() -> &'static Mutex<BTreeSet<String>> {
    static REGISTRY: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// 登记一条由 PadLend 建立的无线连接（必须带端口，如 `10.0.0.2:5555`）。
pub fn mark(serial: &str) {
    let serial = serial.trim();
    if serial.is_empty() || !serial.contains(':') {
        return;
    }
    if let Ok(mut set) = registry().lock() {
        set.insert(serial.to_string());
    }
}

/// 取消登记（PadLend 已经自己关掉了这条连接）。
pub fn unmark(serial: &str) {
    let serial = serial.trim();
    if let Ok(mut set) = registry().lock() {
        set.remove(serial);
    }
}

/// 取出并清空全部登记；断开/退出时调用一次。
pub fn take_all() -> Vec<String> {
    let Ok(mut set) = registry().lock() else {
        return Vec::new();
    };
    let all: Vec<String> = set.iter().cloned().collect();
    set.clear();
    all
}

/// 当前登记的快照（日志/测试用，不改变状态）。
pub fn snapshot() -> Vec<String> {
    match registry().lock() {
        Ok(set) => set.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_take_and_unmark_round_trip() {
        let serial = "198.51.100.7:49999";
        mark(serial);
        assert!(snapshot().contains(&serial.to_string()));
        unmark(serial);
        assert!(!snapshot().contains(&serial.to_string()));
        mark(serial);
        assert!(take_all().contains(&serial.to_string()));
        assert!(
            !snapshot().contains(&serial.to_string()),
            "take_all 必须把登记清空，否则退出时会重复 disconnect"
        );
    }

    #[test]
    fn usb_serials_and_empty_values_are_ignored() {
        mark("");
        mark("   ");
        mark("AM4NBB5C23001691");
        let all = snapshot();
        assert!(!all.iter().any(|s| s == "AM4NBB5C23001691"));
        assert!(!all.iter().any(|s| s.trim().is_empty()));
    }
}
