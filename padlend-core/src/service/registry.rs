//! 对应 `service/device_registry.py`：设备身份（硬件序列号）、端点合并与
//! `devices.json` 持久化。
//!
//! 目标不是"行为完全一致"的学术复刻，而是让 GUI 的"上次连接的设备"列表、去重规则
//! 与布局迁移的输入（身份、主端点、MAC）和 Python 版一致。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::clockfmt;

/// 设备记录文件格式版本。
pub const SCHEMA_VERSION: i64 = 1;
/// 最多保留的设备记录数。
pub const MAX_DEVICES: usize = 32;
/// 单台设备最多保留的端点（USB + 若干无线地址）。
pub const MAX_ENDPOINTS: usize = 8;
/// 单台设备最多信任的 MAC。
pub const MAX_MACS: usize = 8;

/// 明显不是硬件序列号的占位值（Python `_INVALID_IDS`）。
const INVALID_IDS: [&str; 6] = ["", "unknown", "null", "0", "none", "n/a"];
/// adb mDNS 的临时 serial 后缀：这类值不能当设备身份。
const VOLATILE_SUFFIXES: [&str; 3] = ["_adb-tls-connect._tcp", "_adb-tls-pairing._tcp", ".tcp"];

/// 去空白（Python `normalize_serial` 只做 strip）。
pub fn normalize_serial(serial: &str) -> String {
    serial.trim().to_string()
}

/// 统一成小写冒号分隔；非法 MAC 返回空串。
pub fn normalize_mac(value: &str) -> String {
    let lowered = value.trim().to_lowercase().replace('-', ":");
    let bytes = lowered.as_bytes();
    if bytes.len() != 17 {
        return String::new();
    }
    for (index, byte) in bytes.iter().enumerate() {
        let ok = if index % 3 == 2 {
            *byte == b':'
        } else {
            byte.is_ascii_hexdigit()
        };
        if !ok {
            return String::new();
        }
    }
    lowered
}

/// 是否是无线 adb 地址（含冒号或 mDNS 临时 serial）。
pub fn is_wireless_serial(serial: &str) -> bool {
    let text = serial.trim();
    if text.is_empty() {
        return false;
    }
    text.contains(':') || VOLATILE_SUFFIXES.iter().any(|suffix| text.to_lowercase().ends_with(suffix))
}

/// 硬件序列号归一化：无效值、无线地址、与无线端点相同的值都不能当身份。
pub fn canonical_hardware_serial(hardware_serial: &str, endpoint: &str) -> String {
    let text = hardware_serial.trim();
    if text.is_empty() || INVALID_IDS.contains(&text.to_lowercase().as_str()) {
        return String::new();
    }
    if is_wireless_serial(text) {
        return String::new();
    }
    if text.eq_ignore_ascii_case(endpoint.trim()) && is_wireless_serial(endpoint) {
        return String::new();
    }
    text.to_string()
}

/// 拆 `serial` → (host, port, transport)。transport 是 "usb" / "wifi"。
pub fn split_endpoint(serial: &str) -> (String, u16, String) {
    let text = serial.trim();
    if text.is_empty() {
        return (String::new(), 0, "usb".to_string());
    }
    if let Some(rest) = text.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let host = rest[..close].to_string();
            let tail = rest[close + 1..].trim();
            if let Some(port_text) = tail.strip_prefix(':') {
                if let Ok(port) = port_text.trim().parse::<u32>() {
                    if port > 0 && port < 65_536 {
                        return (host, port as u16, "wifi".to_string());
                    }
                }
            }
            return (host, 0, "wifi".to_string());
        }
    }
    let colons = text.matches(':').count();
    if colons == 1 {
        let mut parts = text.splitn(2, ':');
        let host = parts.next().unwrap_or_default().trim().to_string();
        let port_text = parts.next().unwrap_or_default().trim();
        if let Ok(port) = port_text.parse::<u32>() {
            if port > 0 && port < 65_536 {
                return (host, port as u16, "wifi".to_string());
            }
        }
        return (text.to_string(), 0, "wifi".to_string());
    }
    if colons >= 2 {
        return (text.to_string(), 0, "wifi".to_string());
    }
    (text.to_string(), 0, "usb".to_string())
}

/// 端点显示名："USB" 或 `host:port`。
pub fn endpoint_label(serial: &str) -> String {
    let (host, port, transport) = split_endpoint(serial);
    if transport == "usb" {
        return "USB".to_string();
    }
    if port == 0 {
        host
    } else {
        format!("{host}:{port}")
    }
}

fn record_id(hardware_serial: &str, serial: &str) -> String {
    if !hardware_serial.is_empty() {
        format!("hw:{}", hardware_serial.to_lowercase())
    } else {
        format!("ep:{}", serial.to_lowercase())
    }
}

/// 一个 adb 端点（USB 序列号或无线地址）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub serial: String,
    pub host: String,
    pub port: u16,
    pub transport: String,
    pub source: String,
    pub label: String,
    pub aliases: Vec<String>,
    pub last_ok: String,
    pub last_seen: String,
    pub fail_count: i64,
    pub last_error: String,
}

impl Endpoint {
    /// 新端点（插入记录头部时用）。
    pub fn new(serial: &str, source: &str, stamp: &str) -> Self {
        let (host, port, transport) = split_endpoint(serial);
        Self {
            serial: serial.to_string(),
            host,
            port,
            transport,
            source: source.to_string(),
            label: endpoint_label(serial),
            aliases: Vec::new(),
            last_ok: stamp.to_string(),
            last_seen: stamp.to_string(),
            fail_count: 0,
            last_error: String::new(),
        }
    }

    /// 是否对应某个 serial（serial 或历史别名，忽略大小写）。
    pub fn matches(&self, serial: &str) -> bool {
        let needle = normalize_serial(serial);
        if needle.is_empty() {
            return false;
        }
        self.serial.eq_ignore_ascii_case(&needle)
            || self.aliases.iter().any(|item| item.eq_ignore_ascii_case(&needle))
    }

    /// snake_case 字典（与 Python `to_dict` 逐键一致）。
    pub fn to_dict(&self) -> Value {
        json!({
            "serial": self.serial,
            "host": self.host,
            "port": self.port,
            "transport": self.transport,
            "source": self.source,
            "label": self.label,
            "aliases": self.aliases,
            "last_ok": self.last_ok,
            "last_seen": self.last_seen,
            "fail_count": self.fail_count,
            "last_error": self.last_error,
        })
    }

    /// 从字典恢复（缺字段按空值）。
    pub fn from_dict(data: &Value) -> Self {
        let text = |key: &str| -> String {
            data.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let serial = text("serial");
        let (host_default, port_default, transport_default) = split_endpoint(&serial);
        let mut aliases: Vec<String> = Vec::new();
        if let Some(items) = data.get("aliases").and_then(Value::as_array) {
            for item in items {
                if let Some(alias) = item.as_str() {
                    let alias = normalize_serial(alias);
                    if !alias.is_empty() && !aliases.iter().any(|item| item.eq_ignore_ascii_case(&alias)) {
                        aliases.push(alias);
                    }
                }
            }
        }
        aliases.truncate(MAX_ENDPOINTS);
        let mut endpoint = Self {
            serial: serial.clone(),
            host: text("host"),
            port: data.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
            transport: text("transport"),
            source: text("source"),
            label: text("label"),
            aliases,
            last_ok: text("last_ok"),
            last_seen: text("last_seen"),
            fail_count: data.get("fail_count").and_then(Value::as_i64).unwrap_or(0),
            last_error: text("last_error"),
        };
        if endpoint.host.is_empty() {
            endpoint.host = host_default;
        }
        if endpoint.port == 0 {
            endpoint.port = port_default;
        }
        if endpoint.transport.is_empty() {
            endpoint.transport = transport_default;
        }
        if endpoint.label.is_empty() {
            endpoint.label = endpoint_label(&serial);
        }
        endpoint
    }
}

/// 一台设备的记录（可能含多个端点）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: String,
    pub hardware_serial: String,
    pub model: String,
    pub android_id: String,
    pub macs: Vec<String>,
    pub endpoints: Vec<Endpoint>,
    pub first_seen: String,
    pub last_seen: String,
}

impl DeviceRecord {
    /// 某个 serial（或别名）对应的端点。
    pub fn endpoint(&self, serial: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.matches(serial))
    }

    /// 当前应使用的端点地址：current 优先，其次最近成功、最近出现，最后第一个。
    pub fn primary_serial(&self, current: &str) -> String {
        let current = normalize_serial(current);
        if !current.is_empty() && self.endpoint(&current).is_some() {
            return current;
        }
        let mut best: Option<&Endpoint> = None;
        for endpoint in &self.endpoints {
            let better = match best {
                None => true,
                Some(best) => {
                    (endpoint.last_ok.as_str(), endpoint.last_seen.as_str())
                        > (best.last_ok.as_str(), best.last_seen.as_str())
                }
            };
            if better {
                best = Some(endpoint);
            }
        }
        match best.or_else(|| self.endpoints.first()) {
            Some(endpoint) => endpoint.serial.clone(),
            None => String::new(),
        }
    }

    /// snake_case 字典。
    pub fn to_dict(&self) -> Value {
        json!({
            "id": self.id,
            "hardware_serial": self.hardware_serial,
            "model": self.model,
            "android_id": self.android_id,
            "macs": self.macs,
            "endpoints": self.endpoints.iter().map(Endpoint::to_dict).collect::<Vec<_>>(),
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
        })
    }

    /// 从字典恢复。
    pub fn from_dict(data: &Value) -> Option<Self> {
        let text = |key: &str| -> String {
            data.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut endpoints: Vec<Endpoint> = Vec::new();
        if let Some(items) = data.get("endpoints").and_then(Value::as_array) {
            for item in items {
                let endpoint = Endpoint::from_dict(item);
                if endpoint.serial.is_empty() {
                    continue;
                }
                if endpoints.iter().any(|known| known.matches(&endpoint.serial)) {
                    continue;
                }
                endpoints.push(endpoint);
            }
        }
        if endpoints.is_empty() {
            return None;
        }
        let hardware_serial = text("hardware_serial");
        let mut id = text("id");
        if id.is_empty() {
            id = record_id(&hardware_serial, &endpoints[0].serial);
        }
        let mut macs: Vec<String> = Vec::new();
        if let Some(items) = data.get("macs").and_then(Value::as_array) {
            for item in items {
                if let Some(mac) = item.as_str().map(normalize_mac) {
                    if !mac.is_empty() && !macs.contains(&mac) {
                        macs.push(mac);
                    }
                }
            }
        }
        macs.truncate(MAX_MACS);
        Some(Self {
            id,
            hardware_serial,
            model: text("model"),
            android_id: text("android_id"),
            macs,
            endpoints,
            first_seen: text("first_seen"),
            last_seen: text("last_seen"),
        })
    }
}

fn merge_record(target: &mut DeviceRecord, other: DeviceRecord) {
    if target.model.is_empty() {
        target.model = other.model;
    }
    if target.android_id.is_empty() {
        target.android_id = other.android_id;
    }
    for mac in other.macs {
        if !target.macs.contains(&mac) && target.macs.len() < MAX_MACS {
            target.macs.push(mac);
        }
    }
    for endpoint in other.endpoints {
        if !target.endpoints.iter().any(|known| known.matches(&endpoint.serial)) {
            target.endpoints.push(endpoint);
        }
    }
    target.endpoints.truncate(MAX_ENDPOINTS);
    if target.first_seen.is_empty() || (!other.first_seen.is_empty() && other.first_seen < target.first_seen) {
        target.first_seen = other.first_seen;
    }
    if other.last_seen > target.last_seen {
        target.last_seen = other.last_seen;
    }
}

/// 设备注册表（内存 + 落盘）。
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
    pub devices: Vec<DeviceRecord>,
    pub issues: Vec<String>,
    pub existed: bool,
    pub needs_rebuild: bool,
}

impl Registry {
    /// 空表（不读文件）。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            devices: Vec::new(),
            issues: Vec::new(),
            existed: false,
            needs_rebuild: false,
        }
    }

    /// 从 `devices.json` 读取；损坏时备份并从空表重建（记录一条 issue）。
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut registry = Self::new(path.clone());
        if !path.exists() {
            return registry;
        }
        registry.existed = true;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                registry
                    .issues
                    .push(format!("设备记录读取失败：{err}"));
                return registry;
            }
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                let backup = backup(&path);
                registry.needs_rebuild = true;
                registry.issues.push(match backup {
                    Some(name) => format!("设备记录损坏，已从空表重建（备份为 {name}）"),
                    None => "设备记录损坏，已从空表重建".to_string(),
                });
                return registry;
            }
        };
        let Some(object) = parsed.as_object() else {
            registry
                .issues
                .push("设备记录格式不正确，已从空表重建".to_string());
            registry.needs_rebuild = true;
            return registry;
        };
        let version = object
            .get("version")
            .and_then(Value::as_i64)
            .unwrap_or(SCHEMA_VERSION);
        if version > SCHEMA_VERSION {
            let backup = backup(&path);
            registry.needs_rebuild = true;
            registry.issues.push(match backup {
                Some(name) => format!("设备记录版本 {version} 高于当前程序（备份为 {name}）"),
                None => format!("设备记录版本 {version} 高于当前程序"),
            });
            return registry;
        }
        if let Some(items) = object.get("devices").and_then(Value::as_array) {
            for item in items {
                let Some(record) = DeviceRecord::from_dict(item) else {
                    continue;
                };
                if registry.devices.iter().any(|known| known.id == record.id) {
                    continue;
                }
                registry.devices.push(record);
                if registry.devices.len() >= MAX_DEVICES {
                    break;
                }
            }
        }
        registry
    }

    /// 原子写回 `devices.json`；返回是否成功。
    pub fn save(&mut self) -> bool {
        let payload = json!({
            "version": SCHEMA_VERSION,
            "devices": self.devices.iter().map(DeviceRecord::to_dict).collect::<Vec<_>>(),
        });
        let text = match serde_json::to_string_pretty(&payload) {
            Ok(text) => format!("{text}\n"),
            Err(_) => return false,
        };
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let temporary = self.path.with_extension("json.tmp");
        if fs::write(&temporary, text).is_err() {
            return false;
        }
        if fs::rename(&temporary, &self.path).is_err() {
            let _ = fs::remove_file(&temporary);
            return false;
        }
        self.existed = true;
        self.needs_rebuild = false;
        true
    }

    /// 按 id 或 serial 查找。
    pub fn find(&self, needle: &str) -> Option<&DeviceRecord> {
        let needle = normalize_serial(needle);
        self.devices
            .iter()
            .find(|record| record.id == needle)
            .or_else(|| self.find_by_serial(&needle))
    }

    /// 按端点 serial（或别名）查找。
    pub fn find_by_serial(&self, serial: &str) -> Option<&DeviceRecord> {
        let needle = normalize_serial(serial);
        if needle.is_empty() {
            return None;
        }
        self.devices
            .iter()
            .find(|record| record.endpoints.iter().any(|endpoint| endpoint.matches(&needle)))
    }

    /// 按硬件序列号查找（忽略大小写）。
    pub fn find_by_hardware(&self, hardware_serial: &str) -> Option<&DeviceRecord> {
        let needle = hardware_serial.trim();
        if needle.is_empty() {
            return None;
        }
        self.devices
            .iter()
            .find(|record| record.hardware_serial.eq_ignore_ascii_case(needle))
    }

    /// 按 id 查找。
    pub fn find_by_id(&self, id: &str) -> Option<&DeviceRecord> {
        self.devices.iter().find(|record| record.id == id)
    }

    /// 记录排序：当前设备优先，其次最近出现。
    fn ordered(&self, current: &str) -> Vec<&DeviceRecord> {
        let mut items: Vec<&DeviceRecord> = self.devices.iter().collect();
        items.sort_by(|left, right| {
            let left_owner = left.endpoint(current).is_some();
            let right_owner = right.endpoint(current).is_some();
            right_owner
                .cmp(&left_owner)
                .then_with(|| right.last_seen.cmp(&left.last_seen))
                .then_with(|| right.primary_serial(current).cmp(&left.primary_serial(current)))
        });
        items
    }

    /// 全部设备的主端点地址（当前设备排第一）。
    pub fn serials(&self, current: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in self.ordered(current) {
            let serial = record.primary_serial(current);
            if !serial.is_empty() {
                out.push(serial);
            }
        }
        out
    }

    /// 所有记录的 id（调试/诊断用）。
    pub fn ids(&self) -> Vec<String> {
        self.devices.iter().map(|record| record.id.clone()).collect()
    }

    /// 插入或合并一次连接（对应 Python `upsert`）；返回记录 id。
    pub fn upsert(
        &mut self,
        serial: &str,
        hardware_serial: &str,
        model: &str,
        mac: &str,
        android_id: &str,
        source: &str,
    ) -> Option<String> {
        let text = normalize_serial(serial);
        if text.is_empty() {
            return None;
        }
        let hardware = canonical_hardware_serial(hardware_serial, &text);
        let mac = normalize_mac(mac);
        let stamp = clockfmt::iso_seconds();

        let mut target: Option<usize> = None;
        if !hardware.is_empty() {
            target = self.devices.iter().position(|record| {
                !record.hardware_serial.is_empty()
                    && record.hardware_serial.eq_ignore_ascii_case(&hardware)
            });
        }
        if target.is_none() {
            target = self
                .devices
                .iter()
                .position(|record| record.endpoint(&text).is_some());
        }
        if target.is_none() && !mac.is_empty() {
            target = self
                .devices
                .iter()
                .position(|record| record.macs.iter().any(|item| item == &mac));
        }
        if let Some(index) = target {
            let existing = self.devices[index].hardware_serial.clone();
            if !hardware.is_empty() && !existing.is_empty() && !existing.eq_ignore_ascii_case(&hardware)
            {
                target = None;
            }
        }

        let mut index = match target {
            Some(index) => index,
            None => {
                let id = record_id(&hardware, &text);
                match self.devices.iter().position(|record| record.id == id) {
                    Some(index) => index,
                    None => {
                        if self.devices.len() >= MAX_DEVICES {
                            self.devices.remove(0);
                        }
                        self.devices.push(DeviceRecord {
                            id,
                            hardware_serial: hardware.clone(),
                            model: String::new(),
                            android_id: String::new(),
                            macs: Vec::new(),
                            endpoints: Vec::new(),
                            first_seen: stamp.clone(),
                            last_seen: stamp.clone(),
                        });
                        self.devices.len() - 1
                    }
                }
            }
        };

        if !hardware.is_empty() {
            if let Some(record) = self.devices.get_mut(index) {
                record.hardware_serial = hardware.clone();
                record.id = record_id(&hardware, &text);
            }
        }

        // 合并同一身份的多条记录（同 hw 必合并；临时记录按 mac/端点合并）。
        let mut cursor = 0;
        while cursor < self.devices.len() {
            if cursor != index {
                let same_hw = !hardware.is_empty()
                    && self.devices[cursor]
                        .hardware_serial
                        .eq_ignore_ascii_case(&hardware);
                let temporary = self.devices[cursor].hardware_serial.is_empty();
                let same_mac = !mac.is_empty() && self.devices[cursor].macs.iter().any(|item| item == &mac);
                let same_endpoint = self.devices[cursor].endpoint(&text).is_some();
                if same_hw || (temporary && (same_mac || same_endpoint)) {
                    let other = self.devices.remove(cursor);
                    if let Some(record) = self.devices.get_mut(index) {
                        merge_record(record, other);
                    }
                    if cursor < index {
                        index -= 1;
                    }
                    continue;
                }
            }
            cursor += 1;
        }

        // 同一端点不能挂在两台设备上。
        for (position, record) in self.devices.iter_mut().enumerate() {
            if position == index {
                continue;
            }
            record
                .endpoints
                .retain(|endpoint| !endpoint.matches(&text));
        }

        let Some(record) = self.devices.get_mut(index) else {
            return None;
        };
        match record.endpoints.iter_mut().find(|endpoint| endpoint.matches(&text)) {
            Some(endpoint) => {
                endpoint.last_ok = stamp.clone();
                endpoint.last_seen = stamp.clone();
                endpoint.fail_count = 0;
                endpoint.last_error.clear();
            }
            None => {
                record.endpoints.insert(0, Endpoint::new(&text, source, &stamp));
            }
        }
        record.endpoints.truncate(MAX_ENDPOINTS);
        record.last_seen = stamp;
        if !model.trim().is_empty() {
            record.model = model.trim().to_string();
        }
        if !android_id.trim().is_empty() {
            record.android_id = android_id.trim().to_string();
        }
        if !mac.is_empty() && !record.macs.contains(&mac) {
            record.macs.push(mac);
            record.macs.truncate(MAX_MACS);
        }
        Some(record.id.clone())
    }

    /// 端点改名（无线地址变化时保留历史别名）。
    pub fn rename_endpoint(
        &mut self,
        previous: &str,
        target: &str,
        mac: &str,
        source: &str,
    ) -> Option<String> {
        let previous = normalize_serial(previous);
        let target = normalize_serial(target);
        if previous.is_empty() || target.is_empty() {
            return None;
        }
        let index = self
            .devices
            .iter()
            .position(|record| record.endpoint(&previous).is_some())?;
        if self
            .devices
            .iter()
            .enumerate()
            .any(|(position, record)| position != index && record.endpoint(&target).is_some())
        {
            return None;
        }
        let stamp = clockfmt::iso_seconds();
        let record = self.devices.get_mut(index)?;
        if previous.eq_ignore_ascii_case(&target) {
            if let Some(endpoint) = record.endpoints.iter_mut().find(|item| item.matches(&target)) {
                endpoint.last_ok = stamp;
            }
            return Some(record.id.clone());
        }
        if let Some(endpoint) = record.endpoints.iter_mut().find(|item| item.matches(&previous)) {
            let old = endpoint.serial.clone();
            if !endpoint.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&old)) {
                endpoint.aliases.push(old);
            }
            endpoint.aliases.truncate(MAX_ENDPOINTS);
            let (host, port, transport) = split_endpoint(&target);
            endpoint.serial = target.clone();
            endpoint.host = host;
            endpoint.port = port;
            endpoint.transport = transport;
            endpoint.label = endpoint_label(&target);
            endpoint.source = source.to_string();
            endpoint.last_ok = stamp.clone();
            endpoint.last_seen = stamp.clone();
            endpoint.fail_count = 0;
            endpoint.last_error.clear();
        }
        let mac = normalize_mac(mac);
        if !mac.is_empty() && !record.macs.contains(&mac) {
            record.macs.push(mac);
            record.macs.truncate(MAX_MACS);
        }
        record.last_seen = stamp;
        Some(record.id.clone())
    }

    /// 删除某个端点；记录空了就整条删除。返回是否删掉。
    pub fn remove(&mut self, serial: &str) -> bool {
        let Some(index) = self
            .devices
            .iter()
            .position(|record| record.endpoint(serial).is_some())
        else {
            return false;
        };
        let Some(record) = self.devices.get_mut(index) else {
            return false;
        };
        record.endpoints.retain(|endpoint| !endpoint.matches(serial));
        if record.endpoints.is_empty() {
            self.devices.remove(index);
        }
        true
    }

    /// 失败计数（身份校验失败时）。
    pub fn mark_failure(&mut self, serial: &str, detail: &str) {
        let Some(index) = self
            .devices
            .iter()
            .position(|record| record.endpoint(serial).is_some())
        else {
            return;
        };
        if let Some(record) = self.devices.get_mut(index) {
            if let Some(endpoint) = record.endpoints.iter_mut().find(|item| item.matches(serial)) {
                endpoint.fail_count = (endpoint.fail_count + 1).min(9_999);
                let mut text = detail.to_string();
                text.truncate(300);
                endpoint.last_error = text;
                endpoint.last_seen = clockfmt::iso_seconds();
            }
        }
    }

    /// 旧配置（device.known / device.macs）导入；返回导入条数。
    pub fn import_legacy(
        &mut self,
        current: &str,
        known: &[String],
        macs: &[(String, String)],
    ) -> usize {
        let mut serials: Vec<String> = Vec::new();
        for serial in known {
            let serial = normalize_serial(serial);
            if serial.is_empty() || serials.iter().any(|item| item.eq_ignore_ascii_case(&serial)) {
                continue;
            }
            serials.push(serial);
        }
        let current = normalize_serial(current);
        if !current.is_empty() && !serials.iter().any(|item| item.eq_ignore_ascii_case(&current)) {
            serials.insert(0, current);
        }
        let mac_for = |serial: &str| -> String {
            macs.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(serial))
                .map(|(_, value)| normalize_mac(value))
                .unwrap_or_default()
        };
        let mut imported = 0;
        let stamp = clockfmt::iso_seconds();
        for serial in serials {
            if self.find_by_serial(&serial).is_some() {
                continue;
            }
            let mac = mac_for(&serial);
            let id = record_id("", &serial);
            self.devices.push(DeviceRecord {
                id,
                hardware_serial: String::new(),
                model: String::new(),
                android_id: String::new(),
                macs: if mac.is_empty() { Vec::new() } else { vec![mac] },
                endpoints: vec![Endpoint {
                    source: "legacy".to_string(),
                    ..Endpoint::new(&serial, "legacy", &stamp)
                }],
                first_seen: stamp.clone(),
                last_seen: stamp.clone(),
            });
            imported += 1;
        }
        if self.devices.len() > MAX_DEVICES {
            let overflow = self.devices.len() - MAX_DEVICES;
            self.devices.drain(0..overflow);
        }
        imported
    }
}

fn backup(path: &Path) -> Option<String> {
    let stamp = clockfmt::Stamp::now().compact();
    let name = path.file_name()?.to_string_lossy().to_string();
    let target = path.with_file_name(format!("{name}.bad-{stamp}"));
    fs::copy(path, &target).ok()?;
    Some(
        target
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("padlend-registry-{name}-{}.json", std::process::id()));
        path
    }

    #[test]
    fn normalize_mac_accepts_dashes_and_rejects_garbage() {
        assert_eq!(normalize_mac("88-A4-79-1A-2B-3C"), "88:a4:79:1a:2b:3c");
        assert_eq!(normalize_mac("88:a4:79:1a:2b:3c"), "88:a4:79:1a:2b:3c");
        assert_eq!(normalize_mac("not-a-mac"), "");
        assert_eq!(normalize_mac(""), "");
    }

    #[test]
    fn canonical_hardware_serial_rejects_wireless_and_placeholders() {
        assert_eq!(canonical_hardware_serial("ABC123", "ABC123"), "ABC123");
        assert_eq!(canonical_hardware_serial("unknown", ""), "");
        assert_eq!(canonical_hardware_serial("1.2.3.4:5555", "1.2.3.4:5555"), "");
        assert_eq!(
            canonical_hardware_serial("adb-1234-_adb-tls-connect._tcp", ""),
            ""
        );
    }

    #[test]
    fn split_endpoint_distinguishes_usb_and_wifi() {
        assert_eq!(split_endpoint("R58M123"), ("R58M123".to_string(), 0, "usb".to_string()));
        assert_eq!(
            split_endpoint("192.168.1.5:5555"),
            ("192.168.1.5".to_string(), 5555, "wifi".to_string())
        );
        assert_eq!(
            split_endpoint("[fe80::1]:37000"),
            ("fe80::1".to_string(), 37000, "wifi".to_string())
        );
        // 多冒号（裸 IPv6）没有端口。
        assert_eq!(split_endpoint("fe80::1").1, 0);
    }

    #[test]
    fn upsert_merges_usb_and_wireless_endpoints_of_one_device() {
        let mut registry = Registry::new(temp_path("merge"));
        let id = registry
            .upsert("R58M123", "R58M123", "Galaxy_Tab", "", "", "connect")
            .expect("应插入");
        let same = registry
            .upsert("192.168.1.5:5555", "R58M123", "", "88:a4:79:1a:2b:3c", "", "connect")
            .expect("应合并");
        assert_eq!(id, same);
        assert_eq!(registry.devices.len(), 1);
        let record = registry.find_by_id(&id).expect("记录存在");
        assert_eq!(record.endpoints.len(), 2);
        assert_eq!(record.primary_serial("R58M123"), "R58M123");
        assert_eq!(record.macs, vec!["88:a4:79:1a:2b:3c".to_string()]);
    }

    #[test]
    fn upsert_does_not_merge_two_different_hardware_serials_on_one_mac() {
        let mut registry = Registry::new(temp_path("distinct"));
        registry.upsert("1.2.3.4:5555", "HW-ONE", "", "88:a4:79:1a:2b:3c", "", "");
        registry.upsert("1.2.3.5:5555", "HW-TWO", "", "88:a4:79:1a:2b:3c", "", "");
        assert_eq!(registry.devices.len(), 2);
    }

    #[test]
    fn upsert_replaces_endpoint_owner_and_caps_endpoints() {
        let mut registry = Registry::new(temp_path("cap"));
        registry.upsert("R58M123", "R58M123", "", "", "", "");
        for index in 0..12 {
            registry.upsert(
                &format!("10.0.0.{index}:5555"),
                "R58M123",
                "",
                "",
                "",
                "",
            );
        }
        let record = registry.find_by_hardware("R58M123").expect("记录存在");
        assert_eq!(record.endpoints.len(), MAX_ENDPOINTS);
    }

    #[test]
    fn rename_endpoint_keeps_alias_and_updates_display_label() {
        let mut registry = Registry::new(temp_path("rename"));
        let id = registry
            .upsert("10.0.0.2:5555", "", "", "", "", "")
            .expect("应插入");
        let renamed = registry
            .rename_endpoint("10.0.0.2:5555", "10.0.0.9:37000", "", "locate")
            .expect("可改名");
        assert_eq!(id, renamed);
        let record = registry.find_by_id(&id).expect("记录存在");
        assert_eq!(record.endpoints[0].serial, "10.0.0.9:37000");
        assert!(record.endpoints[0].aliases.contains(&"10.0.0.2:5555".to_string()));
        assert_eq!(record.endpoints[0].label, "10.0.0.9:37000");
        // 目标已被另一条记录占用时拒绝。
        registry.upsert("10.0.0.7:5555", "", "", "", "", "");
        assert!(registry
            .rename_endpoint("10.0.0.9:37000", "10.0.0.7:5555", "", "locate")
            .is_none());
    }

    #[test]
    fn mark_failure_counts_up_and_remove_drops_empty_records() {
        let mut registry = Registry::new(temp_path("failure"));
        registry.upsert("R58M1", "", "", "", "", "");
        registry.mark_failure("R58M1", "身份不符");
        let record = registry.find_by_serial("R58M1").expect("记录存在");
        assert_eq!(record.endpoints[0].fail_count, 1);
        assert_eq!(record.endpoints[0].last_error, "身份不符");
        assert!(registry.remove("R58M1"));
        assert!(registry.devices.is_empty());
    }

    #[test]
    fn save_and_load_round_trip_keeps_records() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let mut registry = Registry::new(path.clone());
        registry.upsert("R58M123", "R58M123", "Galaxy_Tab", "88-a4-79-1a-2b-3c", "", "connect");
        assert!(registry.save());
        let loaded = Registry::load(path.clone());
        assert_eq!(loaded.devices.len(), 1);
        assert_eq!(loaded.devices[0].hardware_serial, "R58M123");
        assert_eq!(loaded.devices[0].macs, vec!["88:a4:79:1a:2b:3c".to_string()]);
        assert!(loaded.issues.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_of_corrupt_file_rebuilds_and_backs_up() {
        let path = temp_path("corrupt");
        let _ = fs::remove_file(&path);
        fs::write(&path, "{not json").expect("写文件");
        let loaded = Registry::load(path.clone());
        assert!(loaded.devices.is_empty());
        assert!(loaded.needs_rebuild);
        assert!(loaded.issues[0].contains("损坏"));
        let _ = fs::remove_file(path);
        // 清理备份
        for entry in fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("padlend-registry-corrupt-") && name.contains(".bad-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    #[test]
    fn import_legacy_skips_known_serials_and_attaches_macs() {
        let mut registry = Registry::new(temp_path("legacy"));
        registry.upsert("R58M123", "R58M123", "", "", "", "");
        let imported = registry.import_legacy(
            "R58M123",
            &["R58M123".to_string(), "ABC:5555".to_string()],
            &[("ABC:5555".to_string(), "88-A4-79-1A-2B-3C".to_string())],
        );
        assert_eq!(imported, 1);
        let record = registry.find_by_serial("ABC:5555").expect("导入记录");
        assert_eq!(record.endpoints[0].source, "legacy");
        assert_eq!(record.macs, vec!["88:a4:79:1a:2b:3c".to_string()]);
    }
}
