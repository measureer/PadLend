//! 跨语言解码对拍（M0 门禁 A，无需桌面）。
//!
//! 向量由 `tools/gen_rawinput_vectors.py` 用 **Python 实现** 生成真值，
//! 这里用 Rust 实现解码同一批字节，逐字段比对。

use padlend_core::decode::decode_raw_input;

fn hex_to_bytes(text: &str) -> Vec<u8> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len() / 2);
    let mut index = 0;
    while index + 1 < chars.len() {
        let byte = u8::from_str_radix(&format!("{}{}", chars[index], chars[index + 1]), 16)
            .expect("向量里的 hex 非法");
        out.push(byte);
        index += 2;
    }
    out
}

#[test]
fn decodes_every_python_vector() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("rawinput_vectors.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("读不到 {}：{err}", path.display()));
    let vectors: Vec<serde_json::Value> = serde_json::from_str(&text).expect("向量 JSON 非法");
    assert!(!vectors.is_empty(), "向量为空");

    for vector in &vectors {
        let name = vector["name"].as_str().unwrap_or("<unnamed>");
        let bytes = hex_to_bytes(vector["hex"].as_str().expect("缺 hex"));
        let got = decode_raw_input(&bytes);
        match &vector["expect"] {
            serde_json::Value::Null => {
                assert!(got.is_none(), "{name}: 期望 None，得到 {got:?}");
            }
            expect => {
                let packet = got.unwrap_or_else(|| panic!("{name}: 期望 MousePacket，得到 None"));
                let fields = [
                    ("flags", packet.flags as i64, "flags"),
                    ("x", packet.x as i64, "x"),
                    ("y", packet.y as i64, "y"),
                    ("button_flags", packet.button_flags as i64, "button_flags"),
                    ("button_data", packet.button_data as i64, "button_data"),
                    ("device", packet.device as i64, "device"),
                ];
                for (key, actual, label) in fields {
                    let expected = expect[key].as_i64().unwrap_or_else(|| panic!("{name}: 缺 {key}"));
                    assert_eq!(actual, expected, "{name}: {label} 不一致");
                }
            }
        }
    }
}
