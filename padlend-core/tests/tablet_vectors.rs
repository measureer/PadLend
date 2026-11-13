//! tablet 层跨语言对拍：Python 生成的字节级真值 → Rust 必须逐字节一致。
//!
//! 向量由 `tools/gen_tablet_vectors.py` 生成（UHID 报文、VK→usage、修饰位、
//! scrcpy 控制协议），覆盖 M1 的 uhid/keymap/protocol 三个模块。

use padlend_core::tablet::{keymap, protocol, uhid};
use serde_json::Value;

fn fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("tablet_vectors.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("读不到 {}：{err}", path.display()));
    serde_json::from_str(&text).expect("tablet_vectors.json 非法")
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn from_hex(text: &str) -> Vec<u8> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len() / 2);
    let mut index = 0;
    while index + 1 < chars.len() {
        out.push(
            u8::from_str_radix(&format!("{}{}", chars[index], chars[index + 1]), 16)
                .expect("hex 非法"),
        );
        index += 2;
    }
    out
}

#[test]
fn uhid_mouse_reports_match_python() {
    let data = fixture();
    for case in data["uhid_mouse"].as_array().expect("uhid_mouse 缺失") {
        let buttons = case["buttons"].as_i64().unwrap() as i32;
        let dx = case["dx"].as_i64().unwrap() as i32;
        let dy = case["dy"].as_i64().unwrap() as i32;
        let wheel = case["wheel"].as_i64().unwrap() as i32;
        let expected = case["hex"].as_str().unwrap();
        let got = uhid::mouse_report(buttons, dx, dy, wheel);
        assert_eq!(to_hex(&got), expected, "mouse_report({buttons},{dx},{dy},{wheel})");
    }
}

#[test]
fn uhid_keyboard_reports_match_python() {
    let data = fixture();
    for case in data["uhid_keyboard"].as_array().expect("uhid_keyboard 缺失") {
        let modifier = case["modifier"].as_i64().unwrap() as i32;
        let usages: Vec<i32> = case["usages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap() as i32)
            .collect();
        let expected = case["hex"].as_str().unwrap();
        let got = uhid::keyboard_report(modifier, &usages).expect("keyboard_report 失败");
        assert_eq!(to_hex(&got), expected, "keyboard_report({modifier}, {usages:?})");
    }
}

#[test]
fn vk_to_usage_matches_python() {
    let data = fixture();
    for case in data["vk_to_usage"].as_array().expect("vk_to_usage 缺失") {
        let vk = case["vk"].as_i64().unwrap() as i32;
        let expected = case["usage"].as_u64().map(|value| value as u8);
        assert_eq!(keymap::usage_for(vk), expected, "usage_for(0x{vk:02X})");
    }
}

#[test]
fn modifier_bits_match_python() {
    let data = fixture();
    for case in data["modifier_bit"].as_array().expect("modifier_bit 缺失") {
        let vk = case["vk"].as_i64().unwrap() as i32;
        let scan = case["scan"].as_i64().unwrap() as i32;
        let extended = case["extended"].as_bool().unwrap();
        let expected = case["bit"].as_u64().map(|value| value as u8);
        assert_eq!(
            keymap::modifier_bit(vk, scan, extended),
            expected,
            "modifier_bit(0x{vk:02X}, {scan}, {extended})"
        );
    }
}

#[test]
fn protocol_frames_match_python() {
    let data = fixture();
    for case in data["protocol"].as_array().expect("protocol 缺失") {
        let name = case["fn"].as_str().unwrap();
        let expected = case["hex"].as_str().unwrap();
        let got = match name {
            "inject_keycode" => {
                let args = case["args"].as_array().unwrap();
                protocol::inject_keycode(
                    args[0].as_i64().unwrap(),
                    args[1].as_i64().unwrap(),
                    args[2].as_i64().unwrap(),
                    args[3].as_i64().unwrap(),
                )
            }
            "inject_text" => protocol::inject_text(case["arg"].as_str().unwrap()),
            "set_clipboard" => protocol::set_clipboard(
                case["arg"].as_str().unwrap(),
                case["sequence"].as_u64().unwrap(),
            ),
            "back_or_screen_on" => protocol::back_or_screen_on(case["arg"].as_i64().unwrap()),
            "open_hard_keyboard_settings" => Ok(protocol::open_hard_keyboard_settings()),
            "uhid_create" => protocol::uhid_create(
                case["id"].as_i64().unwrap(),
                case["vendor"].as_i64().unwrap(),
                case["product"].as_i64().unwrap(),
                &from_hex(case["name_hex"].as_str().unwrap()),
                &from_hex(case["desc_hex"].as_str().unwrap()),
            ),
            "uhid_input" => protocol::uhid_input(
                case["id"].as_i64().unwrap(),
                &from_hex(case["data_hex"].as_str().unwrap()),
            ),
            "uhid_report" => protocol::uhid_report(
                case["id"].as_i64().unwrap(),
                &from_hex(case["data_hex"].as_str().unwrap()),
            ),
            "uhid_destroy" => protocol::uhid_destroy(case["id"].as_i64().unwrap()),
            other => panic!("未知的 protocol 向量：{other}"),
        }
        .unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(to_hex(&got), expected, "{name} 帧不一致");
    }
}
