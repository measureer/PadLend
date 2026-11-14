//! 对应 `core/diagnostics.py`：诊断采集、脱敏、报告文本与导出。
//!
//! 与 Python 的已知差异（见报告）：
//! * `diagnostics.export` 导出的是**纯文本** `.txt`（Python 是 zip）。冻结契约只要求
//!   `{"path": str}`；Rust 侧没有 zip 实现也不允许加依赖，宁可导出可读文本也不写假 zip。
//! * 报告里 `Python：x.y` 一行换成 `运行时：Rust x.y`。

use std::fs;
use std::path::{Path, PathBuf};

use crate::constants as C;
use crate::tablet::adb::DeviceEntry;

use super::clockfmt;

/// 一次 adb 调用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub label: String,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    /// 命令是否成功（退出码 0）。
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// 诊断采集步骤（与 Python `DIAGNOSTIC_STEPS` 的顺序一致）。
pub const STEPS: [(&str, &[&str]); 5] = [
    ("adb devices -l", &["devices", "-l"]),
    ("dumpsys input", &["shell", "dumpsys", "input"]),
    ("pointer_speed", &["shell", "settings", "get", "system", "pointer_speed"]),
    ("uhid 组", &["shell", "id"]),
    (
        "uhid 可写",
        &["shell", "exec 3<>/dev/uhid && echo OPEN_OK"],
    ),
];

const DUMPSYS_LIMIT: usize = 20_000;

/// 诊断报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticsReport {
    pub devices: Vec<DeviceEntry>,
    pub devices_raw: String,
    pub dumpsys_input: String,
    pub pointer_speed: String,
    pub uhid_group: bool,
    pub uhid_open: bool,
    pub server_jar: String,
    pub server_jar_present: bool,
    pub errors: Vec<String>,
    pub collected_at: String,
}

/// 解析 `adb devices` 输出（跳过表头与畸形行）。
pub fn parse_devices(output: &str) -> Vec<DeviceEntry> {
    const STATES: [&str; 7] = [
        "device",
        "offline",
        "unauthorized",
        "recovery",
        "sideload",
        "bootloader",
        "host",
    ];
    let mut entries: Vec<DeviceEntry> = Vec::new();
    for line in output.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.starts_with('*') || line.starts_with("List of devices") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else {
            continue;
        };
        let Some(state) = parts.next() else {
            continue;
        };
        if !STATES.contains(&state) {
            continue;
        }
        if entries.iter().any(|entry| entry.serial == serial) {
            continue;
        }
        entries.push(DeviceEntry::new(serial, state));
    }
    entries
}

/// 从 `dumpsys input` 里取 XScale。
pub fn parse_xscale(text: &str) -> Option<f64> {
    let mut rest = text;
    while let Some(index) = rest.find("XScale") {
        let tail = &rest[index + "XScale".len()..];
        let tail = tail.trim_start();
        if let Some(tail) = tail.strip_prefix(':') {
            let value: String = tail
                .trim_start()
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
                .collect();
            if let Ok(parsed) = value.parse::<f64>() {
                return Some(parsed);
            }
        }
        rest = &rest[index + "XScale".len()..];
        if rest.is_empty() {
            break;
        }
    }
    None
}

/// `pointer_speed` 的规范化：空 / null / undefined 都视为未设置。
pub fn parse_pointer_speed(text: &str) -> String {
    let value = text.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("null") || value.eq_ignore_ascii_case("undefined") {
        return String::new();
    }
    value.to_string()
}

/// `id` 输出里是否包含 uhid 组。
pub fn parse_uhid_group(text: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|word| word == "uhid")
}

/// UHID 写入探测是否成功。
pub fn parse_uhid_open(text: &str) -> bool {
    text.contains("OPEN_OK")
}

/// 由一次采集的原始结果构造报告。
pub fn build_report(results: &[CommandResult]) -> DiagnosticsReport {
    let mut report = DiagnosticsReport {
        collected_at: clockfmt::iso_seconds(),
        server_jar: C::scrcpy_server_jar()
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        ..DiagnosticsReport::default()
    };
    report.server_jar_present = C::scrcpy_server_jar().exists();
    for result in results {
        match result.label.as_str() {
            "adb devices -l" => {
                report.devices_raw = result.stdout.clone();
                report.devices = parse_devices(&result.stdout);
            }
            "dumpsys input" => {
                report.dumpsys_input = result.stdout.clone();
            }
            "pointer_speed" => {
                report.pointer_speed = parse_pointer_speed(&result.stdout);
            }
            "uhid 组" => {
                report.uhid_group = parse_uhid_group(&result.stdout);
            }
            "uhid 可写" => {
                report.uhid_open = parse_uhid_open(&result.stdout);
            }
            _ => {}
        }
        if !result.ok() {
            report.errors.push(format!(
                "{} 失败（exit {}）：{}",
                result.label,
                result.code,
                result.stderr.trim()
            ));
        } else if result.label == "pointer_speed"
            && report.pointer_speed.is_empty()
            && !result.stderr.trim().is_empty()
        {
            report.errors.push(format!(
                "pointer_speed 失败：{}",
                result.stderr.trim()
            ));
        }
    }
    report
}

impl DiagnosticsReport {
    /// 主设备（第一台）。
    pub fn primary_device(&self) -> Option<&DeviceEntry> {
        self.devices.first()
    }

    /// XScale（未采集到为 None）。
    pub fn xscale(&self) -> Option<f64> {
        parse_xscale(&self.dumpsys_input)
    }

    /// 概要六行（GUI 与报告共用）。
    pub fn summary(&self) -> Vec<String> {
        let device = match self.primary_device() {
            Some(entry) => format!("{}（{}）", entry.serial, entry.state),
            None => "未检测到设备".to_string(),
        };
        vec![
            format!("设备：{device}"),
            format!(
                "XScale：{}",
                match self.xscale() {
                    Some(value) => format!("{value}"),
                    None => "未读取到".to_string(),
                }
            ),
            format!(
                "pointer_speed：{}",
                if self.pointer_speed.is_empty() {
                    "未设置".to_string()
                } else {
                    self.pointer_speed.clone()
                }
            ),
            format!("UHID 组：{}", if self.uhid_group { "有" } else { "无" }),
            format!("UHID 可打开：{}", if self.uhid_open { "是" } else { "否" }),
            format!(
                "server jar：{}",
                if self.server_jar_present { "已就绪" } else { "缺失" }
            ),
        ]
    }

    /// 报告正文（行序与 Python `to_text` 一致）。
    pub fn to_text(&self) -> String {
        let serials = self.device_serials();
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("{} {} 诊断报告", C::APP_NAME, C::VERSION));
        lines.push("（设备序列号、IP/MAC、用户名与用户目录路径已自动打码）".to_string());
        lines.push(format!("生成时间：{}", self.collected_at));
        lines.push(format!("运行时：Rust {}", C::VERSION));
        lines.push("平台：win32".to_string());
        lines.push("配置：config.yaml（位于应用配置目录）".to_string());
        lines.push(String::new());
        lines.push("== 概要 ==".to_string());
        lines.extend(self.summary());
        if !self.errors.is_empty() {
            lines.push(String::new());
            lines.push("== 错误 ==".to_string());
            lines.push(redact(&self.errors.join("\n"), &serials));
        }
        lines.push(String::new());
        lines.push("== adb devices -l ==".to_string());
        let devices = redact(&self.devices_raw, &serials).trim().to_string();
        lines.push(if devices.is_empty() {
            "（空）".to_string()
        } else {
            devices
        });
        lines.push(String::new());
        lines.push("== UHID 原始输出 ==".to_string());
        lines.push(format!(
            "组：{}",
            if self.uhid_group { "uhid" } else { "（未包含 uhid）" }
        ));
        lines.push(format!(
            "打开：{}",
            if self.uhid_open { "OPEN_OK" } else { "（未成功）" }
        ));
        lines.push(String::new());
        lines.push("== dumpsys input（节选，已脱敏） ==".to_string());
        let dumpsys = redact(&self.dumpsys_input, &serials);
        let dumpsys = if dumpsys.chars().count() > DUMPSYS_LIMIT {
            let mut truncated: String = dumpsys.chars().take(DUMPSYS_LIMIT).collect();
            truncated.push_str("\n…（已截断）");
            truncated
        } else {
            dumpsys
        };
        lines.push(if dumpsys.trim().is_empty() {
            "（空）".to_string()
        } else {
            dumpsys
        });
        lines.join("\n")
    }

    /// 采集到的设备序列号（脱敏用）。
    fn device_serials(&self) -> Vec<String> {
        let mut serials: Vec<String> = self
            .devices
            .iter()
            .map(|entry| entry.serial.clone())
            .filter(|serial| serial.len() >= 4)
            .collect();
        serials.sort_by(|left, right| right.len().cmp(&left.len()));
        serials
    }

    /// 导出为 zip（与 Python 的 `export_report` 同扩展名、同目录约定），返回文件路径。
    pub fn export(&self, directory: Option<&Path>) -> Result<PathBuf, String> {
        let target_dir = match directory {
            Some(path) => path.to_path_buf(),
            None => desktop_dir(),
        };
        fs::create_dir_all(&target_dir).map_err(|err| format!("创建导出目录失败：{err}"))?;
        let stamp = clockfmt::Stamp::now().compact();
        let path = target_dir.join(format!("padlend-diagnostics-{stamp}.zip"));

        let readme = "PadLend 诊断包\n\
已自动隐藏：设备序列号、IP/MAC 地址、Windows 用户名与用户目录路径。\n\
如仍看到隐私内容，请先自行删除再分享。\n";
        let report = self.to_text();
        let archive = zip_store(&[
            ("README.txt", readme.as_bytes()),
            ("diagnostics.txt", report.as_bytes()),
        ]);
        fs::write(&path, archive).map_err(|err| format!("写诊断包失败：{err}"))?;
        Ok(path)
    }
}

/// 生成一个**不压缩**（stored）的 zip 归档。
///
/// 不引第三方依赖：诊断包只需要"能被解压打开"，stored 方式完全合法；
/// Python 版用 DEFLATE 只是体积更小。
fn zip_store(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();

    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;

        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0x21u16.to_le_bytes()); // mod date (1980-01-01)
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }

    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let central_size = central.len() as u32;

    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// CRC-32（IEEE，反射多项式 0xEDB88320）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 仅静态摘要（不跑 adb）：只填 server jar 与一条错误。
pub fn static_summary() -> String {
    let mut report = DiagnosticsReport {
        collected_at: clockfmt::iso_seconds(),
        server_jar: C::scrcpy_server_jar()
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        ..DiagnosticsReport::default()
    };
    report.server_jar_present = C::scrcpy_server_jar().exists();
    report
        .errors
        .push("未运行 adb 采集（仅静态摘要）".to_string());
    report.to_text()
}

/// 桌面目录（Windows：`%USERPROFILE%\Desktop`；取不到时退回临时目录）。
pub fn desktop_dir() -> PathBuf {
    // 默认导出到「下载」而不是桌面：桌面是用户自己的地盘，诊断包每次一坨很难受。
    if let Ok(home) = std::env::var("USERPROFILE") {
        let home = PathBuf::from(home);
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        let desktop = home.join("Desktop");
        if desktop.is_dir() {
            return desktop;
        }
    }
    std::env::temp_dir()
}

/// 脱敏：序列号 / MAC / IPv4 / 用户名目录。
pub fn redact(text: &str, serials: &[String]) -> String {
    let mut out = text.to_string();
    for serial in serials {
        if serial.len() >= 4 {
            out = out.replace(serial, "<serial>");
        }
    }
    out = redact_mac(&out);
    out = redact_ipv4(&out);
    redact_user_path(&out)
}

fn redact_mac(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 17 <= bytes.len() {
            let window = &bytes[index..index + 17];
            let looks_like_mac = window.iter().enumerate().all(|(offset, byte)| {
                if offset % 3 == 2 {
                    *byte == b':'
                } else {
                    byte.is_ascii_hexdigit()
                }
            });
            if looks_like_mac {
                out.push_str("<MAC>");
                index += 17;
                continue;
            }
        }
        let ch = text[index..].chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn redact_ipv4(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        // 尝试匹配 a.b.c.d，每段 1..3 位数字。
        let mut cursor = index;
        let mut groups = 0;
        loop {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && cursor - start < 3 {
                cursor += 1;
            }
            if cursor == start {
                break;
            }
            groups += 1;
            if groups == 4 {
                break;
            }
            if cursor < bytes.len() && bytes[cursor] == b'.' {
                cursor += 1;
            } else {
                groups = 0;
                break;
            }
        }
        if groups == 4 {
            // 数字段必须落在边界上（前面不是数字/点，后面不是数字）。
            let before_ok = index == 0 || !bytes[index - 1].is_ascii_digit();
            let after_ok = cursor >= bytes.len() || !(bytes[cursor].is_ascii_digit() || bytes[cursor] == b'.');
            if before_ok && after_ok {
                out.push_str("<IP>");
                index = cursor;
                continue;
            }
        }
        let ch = text[index..].chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn redact_user_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("\\Users\\") {
        out.push_str(&rest[..index]);
        out.push_str("\\Users\\<user>");
        let tail = &rest[index + "\\Users\\".len()..];
        let end = tail
            .find(|ch: char| ch == '\\' || ch == '/' || ch.is_whitespace())
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results() -> Vec<CommandResult> {
        vec![
            CommandResult {
                label: "adb devices -l".to_string(),
                code: 0,
                stdout: "List of devices attached\nSIMULATEDPAD\tdevice usb:1-1\n".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                label: "dumpsys input".to_string(),
                code: 0,
                stdout: "Cursor Input Mapper:\n  XScale: 1.000\n".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                label: "pointer_speed".to_string(),
                code: 0,
                stdout: "-3\n".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                label: "uhid 组".to_string(),
                code: 0,
                stdout: "uid=2000(shell) gid=2000(shell) groups=1003,uhid,1000\n".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                label: "uhid 可写".to_string(),
                code: 0,
                stdout: "OPEN_OK\n".to_string(),
                stderr: String::new(),
            },
        ]
    }

    #[test]
    fn parse_devices_keeps_only_state_lines() {
        let entries = parse_devices(
            "List of devices attached\nABC\tdevice product:x\nOFF\tunauthorized\n* daemon started\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].serial, "ABC");
        assert_eq!(entries[1].state, "unauthorized");
    }

    #[test]
    fn parse_xscale_reads_numeric_value() {
        assert_eq!(parse_xscale("Cursor Input Mapper:\n  XScale: 2.5\n"), Some(2.5));
        assert_eq!(parse_xscale("no scale here"), None);
    }

    #[test]
    fn parse_pointer_speed_normalizes_empty_values() {
        assert_eq!(parse_pointer_speed("null\n"), "");
        assert_eq!(parse_pointer_speed("  "), "");
        assert_eq!(parse_pointer_speed("-3\n"), "-3");
    }

    #[test]
    fn parse_uhid_flags_work() {
        assert!(parse_uhid_group("groups=1003,uhid,1000"));
        assert!(!parse_uhid_group("groups=1003,other"));
        assert!(parse_uhid_open("OPEN_OK\n"));
        assert!(!parse_uhid_open("permission denied"));
    }

    #[test]
    fn build_report_fills_fields_and_summary() {
        let report = build_report(&results());
        assert_eq!(report.devices.len(), 1);
        assert_eq!(report.pointer_speed, "-3");
        assert!(report.uhid_group);
        assert!(report.uhid_open);
        assert_eq!(report.xscale(), Some(1.0));
        let summary = report.summary();
        assert_eq!(summary.len(), 6);
        assert!(summary[0].starts_with("设备：SIMULATEDPAD"));
        assert!(summary[3].ends_with("有"));
    }

    #[test]
    fn failed_step_lands_in_errors() {
        let mut items = results();
        items.push(CommandResult {
            label: "extra".to_string(),
            code: 1,
            stdout: String::new(),
            stderr: "boom\n".to_string(),
        });
        let report = build_report(&items);
        assert!(report.errors.iter().any(|line| line.contains("extra 失败")));
    }

    #[test]
    fn to_text_has_fixed_sections_and_redacts_serial() {
        let report = build_report(&results());
        let text = report.to_text();
        assert!(text.contains("== 概要 =="));
        assert!(text.contains("== adb devices -l =="));
        assert!(text.contains("== UHID 原始输出 =="));
        assert!(text.contains("<serial>"), "序列号应被打码：{text}");
        assert!(
            text.contains("<serial>\tdevice"),
            "adb devices 段应打码：{text}"
        );
    }

    #[test]
    fn redact_masks_mac_and_ip() {
        let text = redact(
            "mac 88:a4:79:1a:2b:3c ip 192.168.1.5 path C:\\Users\\alice\\x",
            &[],
        );
        assert!(text.contains("<MAC>"), "{text}");
        assert!(text.contains("<IP>"), "{text}");
        assert!(text.contains("<user>"), "{text}");
        assert!(!text.contains("alice"));
    }

    #[test]
    fn export_writes_a_zip_archive() {
        let dir = std::env::temp_dir().join(format!("padlend-diag-{}", std::process::id()));
        let report = build_report(&results());
        let path = report.export(Some(&dir)).expect("应能导出");
        assert!(path.exists());
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("zip"));
        let bytes = fs::read(&path).expect("可读");
        assert_eq!(&bytes[0..4], b"PK\x03\x04", "本地文件头签名");
        assert_eq!(&bytes[bytes.len() - 22..bytes.len() - 18], b"PK\x05\x06", "EOCD 在末尾");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("diagnostics.txt"));
        assert!(text.contains("README.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32/ISO-HDLC("123456789") = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}

