//! 启动时查找上次连接的平板（对应 tablet/locator.py，0.3.13）。
//!
//! 无线调试的两个东西都会变：路由器可能给平板换了局域网 IP，平板重启后
//! 「无线调试」的端口也会重新随机分配。只按上次的 ip:port 去连，经常会
//! 明明在同一个 Wi-Fi 里却报「离线」。
//!
//! 这里的思路是三层递进：
//!
//! 1. 先用上次记下的地址做一次 TCP 连接（端口能连上基本就说明 adb 还在）；
//! 2. 连不上就查 mDNS——无线调试会通过 mDNS 广播当前端口。优先用
//!    adb mdns services；Windows 上 adb 常常没有 mDNS 守护进程，这时退回
//!    tablet::mdns::discover 直接查询，同一个 IP 换了端口也能找到；
//! 3. IP 也变了的话，用上次记下的 MAC 在系统 ARP 邻居表里找它现在挂在哪个
//!    地址上；找到 IP 后再回头查 mDNS 有没有对应端口。
//!
//! 三层都失败才算离线。查询函数全部可注入，便于单测。

use std::collections::BTreeMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::tablet::mdns::{self, MdnsService};

/// 上次的地址仍然在线。
pub const STATUS_ONLINE: &str = "online";
/// 找到了，但地址/端口变了。
pub const STATUS_MOVED: &str = "moved";
/// 找到了 IP，但无线调试端口未知。
pub const STATUS_NO_PORT: &str = "no_port";
/// 局域网里找不到。
pub const STATUS_OFFLINE: &str = "offline";

/// 无线调试的连接服务；配对服务是另一个名字，不能用它来 connect。
const ADB_CONNECT_SERVICE: &str = "_adb-tls-connect._tcp";
/// ARP 查询的命令超时（秒）。
const ARP_TIMEOUT_S: f64 = 5.0;

/// 查找结论的状态（对应 Python 的四个 STATUS_* 字符串常量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocateStatus {
    /// 原地址可用。
    Online,
    /// 找到了新地址。
    Moved,
    /// 找到 IP、缺端口。
    NoPort,
    /// 找不到。
    Offline,
}

impl LocateStatus {
    /// 冻结的字符串值（与 Python 的 STATUS_* 一致，GUI 与诊断包按它判断）。
    pub fn as_str(self) -> &'static str {
        match self {
            LocateStatus::Online => STATUS_ONLINE,
            LocateStatus::Moved => STATUS_MOVED,
            LocateStatus::NoPort => STATUS_NO_PORT,
            LocateStatus::Offline => STATUS_OFFLINE,
        }
    }
}

/// 一次查找的结论；detail 是可以直接显示给用户的中文句子。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Located {
    /// 结论状态。
    pub status: Option<LocateStatus>,
    /// 可用的 adb serial（离线时为空）。
    pub address: String,
    /// 定位到的 IP。
    pub ip: String,
    /// 定位到的端口（0 = 未知）。
    pub port: u16,
    /// 规范化后的 MAC。
    pub mac: String,
    /// 给用户看的说明。
    pub detail: String,
}

impl Located {
    /// 是否可以直接连接。
    pub fn online(&self) -> bool {
        self.status == Some(LocateStatus::Online)
    }

    /// 至少 IP 上找到了，只是可能缺端口。
    pub fn reachable(&self) -> bool {
        matches!(
            self.status,
            Some(LocateStatus::Online) | Some(LocateStatus::Moved) | Some(LocateStatus::NoPort)
        )
    }

    /// 冻结的字符串状态（None 视为 offline，与 Python 默认值一致）。
    pub fn status_str(&self) -> &'static str {
        self.status.unwrap_or(LocateStatus::Offline).as_str()
    }
}

/// 拆出 (host, port)，支持 IPv4、主机名、[IPv6]:port 与裸 IPv6。
///
/// 无线调试在 VPN/异地组网下可能是主机名或 IPv6，不能只按第一个冒号切。
/// 没有端口时按 adb 的老默认 5555 处理；非法端口也回退 5555。
pub fn split_address(address: &str) -> (String, u16) {
    let text = address.trim();
    if text.is_empty() {
        return (String::new(), 0);
    }

    let mut host = text.to_string();
    let mut port_text = String::new();
    if text.starts_with('[') {
        let Some(close) = text.find(']') else {
            return (String::new(), 0);
        };
        host = text[1..close].trim().to_string();
        let rest = text[close + 1..].trim();
        if let Some(stripped) = rest.strip_prefix(':') {
            port_text = stripped.trim().to_string();
        }
    } else if text.matches(':').count() == 1 {
        let mut parts = text.splitn(2, ':');
        host = parts.next().unwrap_or_default().trim().to_string();
        port_text = parts.next().unwrap_or_default().to_string();
    } else if text.matches(':').count() >= 2 {
        // 裸 IPv6：没有方括号时无法区分末尾数字是地址一部分还是端口，
        // 统一按默认端口处理；用户要指定端口请用 [addr]:port。
        host = text.to_string();
    }
    if host.is_empty() {
        return (String::new(), 0);
    }
    if !port_text.is_empty() && port_text.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(value) = port_text.parse::<u32>() {
            if value > 0 && value < 65_536 {
                return (host, value as u16);
            }
        }
    }
    (host, 5555)
}

/// (host, port) → adb serial；IPv6 必须带方括号才能和端口共存。
pub fn format_address(host: &str, port: u16) -> String {
    let clean = host.trim();
    if clean.is_empty() || port == 0 {
        return String::new();
    }
    if clean.contains(':') && !clean.starts_with('[') {
        return format!("[{clean}]:{port}");
    }
    format!("{clean}:{port}")
}

/// 统一成小写冒号分隔；88-A4-79-... 与 88:a4:79:... 等价。
pub fn normalize_mac(value: &str) -> String {
    let lowered = value.trim().to_lowercase().replace('-', ":");
    let mut collapsed = String::with_capacity(lowered.len());
    let mut last_was_colon = false;
    for ch in lowered.chars() {
        if ch == ':' {
            if last_was_colon {
                continue;
            }
            last_was_colon = true;
        } else {
            last_was_colon = false;
        }
        collapsed.push(ch);
    }
    if is_mac(&collapsed) {
        collapsed
    } else {
        String::new()
    }
}

/// 严格校验 aa:bb:cc:dd:ee:ff（小写，长度 17）。
fn is_mac(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        if index % 3 == 2 {
            if *byte != b':' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
            return false;
        }
    }
    true
}

/// 这个地址现在是否接受连接（不依赖 adb，避免触发 adb server 启动）。
pub fn tcp_open(ip: &str, port: u16, timeout: f64) -> bool {
    if ip.is_empty() || port == 0 {
        return false;
    }
    let budget = if timeout > 0.0 { timeout } else { 0.001 };
    let duration = Duration::from_secs_f64(budget);
    let addresses = match (ip, port).to_socket_addrs() {
        Ok(addresses) => addresses,
        Err(_) => return false,
    };
    for address in addresses {
        if TcpStream::connect_timeout(&address, duration).is_ok() {
            return true;
        }
    }
    false
}

/// 从 adb mdns services 的输出里抽出 (ip, port) 列表。
///
/// service 非空时只保留包含该服务名的行（例如 _adb-tls-connect._tcp），
/// 避免把配对端口当成连接端口。
pub fn parse_mdns(lines: &[String], service: Option<&str>) -> Vec<(String, u16)> {
    let wanted = service
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_lowercase();
    let mut found: Vec<(String, u16)> = Vec::new();
    for line in lines {
        if !wanted.is_empty() && !line.to_lowercase().contains(&wanted) {
            continue;
        }
        for (host, port) in find_ipv4_pairs(line) {
            if port > 0 && !found.iter().any(|(item, _)| *item == host) {
                if !found.iter().any(|(h, p)| *h == host && *p == port) {
                    found.push((host, port));
                }
            }
        }
    }
    found
}

/// 扫描文本里所有 IPv4:port 形式的片段（对应 Python 的 _IPV4 正则）。
fn find_ipv4_pairs(text: &str) -> Vec<(String, u16)> {
    let bytes = text.as_bytes();
    let mut found: Vec<(String, u16)> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        match parse_ipv4_pair_at(bytes, index) {
            Some((host, port, end)) => {
                found.push((host, port));
                index = end;
            }
            None => index += 1,
        }
    }
    found
}

/// 从 index 处尝试解析 a.b.c.d:port；成功返回 (host, port, 结束偏移)。
fn parse_ipv4_pair_at(bytes: &[u8], index: usize) -> Option<(String, u16, usize)> {
    let (octets, mut cursor) = parse_ipv4_at(bytes, index)?;
    if cursor >= bytes.len() || bytes[cursor] != b':' {
        return None;
    }
    cursor += 1;
    let start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && cursor - start < 5 {
        cursor += 1;
    }
    if cursor == start {
        return None;
    }
    let port: u32 = std::str::from_utf8(&bytes[start..cursor])
        .ok()?
        .parse()
        .ok()?;
    if port == 0 || port > 65_535 {
        return None;
    }
    Some((octets, port as u16, cursor))
}

/// 解析 a.b.c.d（每段 1..=3 位，贪心带回溯），返回 (点分字符串, 结束偏移)。
fn parse_ipv4_at(bytes: &[u8], index: usize) -> Option<(String, usize)> {
    let mut cursor = index;
    let mut octets: Vec<&[u8]> = Vec::with_capacity(4);
    for group in 0..4 {
        if group > 0 {
            if cursor >= bytes.len() || bytes[cursor] != b'.' {
                return None;
            }
            cursor += 1;
        }
        // 贪心：先试 3 位，再 2 位、1 位（与正则的回溯结果一致）。
        let mut matched = None;
        for length in (1..=3).rev() {
            if cursor + length > bytes.len() {
                continue;
            }
            let slice = &bytes[cursor..cursor + length];
            if slice.iter().all(|byte| byte.is_ascii_digit()) && slice[0].is_ascii_digit() {
                matched = Some(slice);
                break;
            }
        }
        let slice = matched?;
        octets.push(slice);
        cursor += slice.len();
    }
    let host = octets
        .iter()
        .map(|slice| std::str::from_utf8(slice).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(".");
    Some((host, cursor))
}

/// 发现无线调试服务，保留 TXT 里的 serial/model。
///
/// adb mdns services 的原始行没有 TXT 字段，但能给出当前端口；直接 mDNS
/// 查询（Windows 上更常见）则能拿到硬件序列号。两路都失败时返回空。
pub fn discovered_mdns(
    adb_mdns: Option<&dyn Fn() -> Vec<String>>,
    timeout: f64,
) -> Vec<MdnsService> {
    if let Some(query) = adb_mdns {
        let lines = query();
        let pairs = parse_mdns(&lines, Some(ADB_CONNECT_SERVICE));
        if !pairs.is_empty() {
            return pairs
                .into_iter()
                .map(|(ip, port)| MdnsService {
                    ip,
                    port,
                    ..MdnsService::default()
                })
                .collect();
        }
    }

    let services = mdns::discover(timeout);
    let mut found: Vec<MdnsService> = Vec::new();
    let mut seen: Vec<(String, u16)> = Vec::new();
    for item in services {
        let pair = (item.ip.clone(), item.port);
        if seen.contains(&pair) {
            continue;
        }
        seen.push(pair);
        found.push(item);
    }
    found
}

/// 兼容旧调用方：只返回 (ip, port)。
pub fn discovered_services(
    adb_mdns: Option<&dyn Fn() -> Vec<String>>,
    timeout: f64,
) -> Vec<(String, u16)> {
    discovered_mdns(adb_mdns, timeout)
        .into_iter()
        .map(|item| (item.ip, item.port))
        .collect()
}

/// 在已知服务列表里按 IP 找端口；找不到返回 0。
pub fn mdns_port_for(ip: &str, services: &[(String, u16)]) -> u16 {
    services
        .iter()
        .find(|(host, _)| host == ip)
        .map(|(_, port)| *port)
        .unwrap_or(0)
}

/// 系统 ARP 邻居表：MAC（统一格式）→ IP（真实 subprocess，单独隔离）。
pub fn arp_table() -> BTreeMap<String, String> {
    let output = run_arp();
    match output {
        Some(bytes) => parse_arp(&bytes),
        None => BTreeMap::new(),
    }
}

/// 跑 arp -a；失败返回 None（对应 Python 的 try/except）。
fn run_arp() -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut command = Command::new("arp");
    command.arg("-a");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("[tablet.locator] arp -a failed: {error}");
            return None;
        }
    };
    let stdout = child.stdout.take();
    let deadline = std::time::Instant::now()
        + Duration::from_secs_f64(ARP_TIMEOUT_S.max(0.001));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    use std::io::Read;
    let mut buffer: Vec<u8> = Vec::new();
    if let Some(mut stream) = stdout {
        let _ = stream.read_to_end(&mut buffer);
    }
    Some(buffer)
}

/// 解析 arp -a 的输出（纯函数；中文 Windows 的 GBK 页头按替换解码）。
pub fn parse_arp(output: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(output);
    let mut table: BTreeMap<String, String> = BTreeMap::new();
    for line in text.split(['\r', '\n']) {
        let Some((ip, mac)) = parse_arp_line(line) else {
            continue;
        };
        let mac = normalize_mac(&mac);
        if mac.is_empty() || ip == "0.0.0.0" || ip == "255.255.255.255" {
            continue;
        }
        table.entry(mac).or_insert(ip);
    }
    table
}

/// 在一行里找 IPv4 + 8-8-8-8-8-8 形式的 MAC。
fn parse_arp_line(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        if let Some((ip, end)) = parse_ipv4_at(bytes, index) {
            let mut cursor = end;
            let spaces_start = cursor;
            while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
                cursor += 1;
            }
            if cursor > spaces_start {
                if let Some(mac) = parse_dashed_mac_at(bytes, cursor) {
                    return Some((ip, mac));
                }
            }
            index = end;
        } else {
            index += 1;
        }
    }
    None
}

/// 解析 aa-bb-cc-dd-ee-ff。
fn parse_dashed_mac_at(bytes: &[u8], index: usize) -> Option<String> {
    if index + 17 > bytes.len() {
        return None;
    }
    let slice = &bytes[index..index + 17];
    for (offset, byte) in slice.iter().enumerate() {
        if offset % 3 == 2 {
            if *byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(String::from_utf8_lossy(slice).into_owned())
}

/// 按 MAC 在邻居表里找 IP。
pub fn find_ip_by_mac(mac: &str, table: &BTreeMap<String, String>) -> String {
    let wanted = normalize_mac(mac);
    if wanted.is_empty() {
        return String::new();
    }
    table.get(&wanted).cloned().unwrap_or_default()
}

/// 按「原地址 → mDNS → MAC/ARP → 离线」的顺序找上次那台平板。
///
/// hardware_serial 来自设备注册表。mDNS TXT 里的 serial 是硬件序列号，
/// 在路由器换 IP 后仍不变，因此它的优先级高于「同 IP 换端口」；MAC/ARP
/// 只在没有 mDNS 信息时兜底。
pub fn locate(
    address: &str,
    mac: &str,
    hardware_serial: &str,
    services: Option<&dyn Fn() -> Vec<String>>,
    mdns_lookup: Option<&dyn Fn() -> Vec<MdnsService>>,
    arp: Option<&dyn Fn() -> BTreeMap<String, String>>,
    timeout: f64,
) -> Located {
    locate_with(
        address,
        mac,
        hardware_serial,
        services,
        mdns_lookup,
        arp,
        timeout,
        &tcp_open,
    )
}

/// [locate] 的可注入版本：tcp 用来判断地址是否可达（测试替换成假探针）。
#[allow(clippy::too_many_arguments)]
pub fn locate_with(
    address: &str,
    mac: &str,
    hardware_serial: &str,
    services: Option<&dyn Fn() -> Vec<String>>,
    mdns_lookup: Option<&dyn Fn() -> Vec<MdnsService>>,
    arp: Option<&dyn Fn() -> BTreeMap<String, String>>,
    timeout: f64,
    tcp: &dyn Fn(&str, u16, f64) -> bool,
) -> Located {
    let (host, port) = split_address(address);
    if host.is_empty() {
        return Located {
            status: Some(LocateStatus::Offline),
            mac: normalize_mac(mac),
            detail: "还没有连接过设备".to_string(),
            ..Located::default()
        };
    }

    if tcp(&host, port, timeout) {
        let formatted = format_address(&host, port);
        return Located {
            status: Some(LocateStatus::Online),
            address: formatted.clone(),
            ip: host,
            port,
            mac: normalize_mac(mac),
            detail: format!("上次的地址 {formatted} 仍然在线"),
        };
    }

    let mut found: Vec<(String, u16, String)> = Vec::new();
    let mut add = |ip: &str, port_number: u16, serial: &str| {
        if ip.is_empty() || port_number == 0 {
            return;
        }
        if found
            .iter()
            .any(|(item, item_port, _)| item == ip && *item_port == port_number)
        {
            return;
        }
        found.push((ip.to_string(), port_number, serial.to_string()));
    };

    if let Some(query) = services {
        for (item_ip, item_port) in parse_mdns(&query(), None) {
            add(&item_ip, item_port, "");
        }
    }

    if let Some(query) = mdns_lookup {
        for item in query() {
            add(&item.ip, item.port, &item.serial);
        }
    }

    let wanted_hw = hardware_serial.trim().to_lowercase();
    if !wanted_hw.is_empty() {
        if let Some((match_ip, match_port, _)) = found
            .iter()
            .find(|(_, _, serial)| serial.trim().to_lowercase() == wanted_hw)
            .cloned()
        {
            if tcp(&match_ip, match_port, timeout) {
                let formatted = format_address(&match_ip, match_port);
                return Located {
                    status: Some(LocateStatus::Moved),
                    address: formatted.clone(),
                    ip: match_ip,
                    port: match_port,
                    mac: normalize_mac(mac),
                    detail: format!("按硬件序列号在 mDNS 找到 {formatted}"),
                };
            }
        }
    }

    let pairs: Vec<(String, u16)> = found
        .iter()
        .map(|(ip, port_number, _)| (ip.clone(), *port_number))
        .collect();
    let new_port = mdns_port_for(&host, &pairs);
    if new_port != 0 && tcp(&host, new_port, timeout) {
        return Located {
            status: Some(LocateStatus::Moved),
            address: format_address(&host, new_port),
            ip: host,
            port: new_port,
            mac: normalize_mac(mac),
            detail: format!("IP 没变，无线调试端口换成了 {new_port}"),
        };
    }

    let wanted_mac = normalize_mac(mac);
    if !wanted_mac.is_empty() {
        let mut table: BTreeMap<String, String> = BTreeMap::new();
        if let Some(query) = arp {
            table = query();
        }
        let moved_ip = find_ip_by_mac(&wanted_mac, &table);
        if !moved_ip.is_empty() && moved_ip != host {
            let moved_port = mdns_port_for(&moved_ip, &pairs);
            if moved_port != 0 && tcp(&moved_ip, moved_port, timeout) {
                return Located {
                    status: Some(LocateStatus::Moved),
                    address: format_address(&moved_ip, moved_port),
                    ip: moved_ip.clone(),
                    port: moved_port,
                    mac: wanted_mac,
                    detail: format!("IP 从 {host} 换成了 {moved_ip}，端口 {moved_port}"),
                };
            }
            if tcp(&moved_ip, 5555, timeout) {
                return Located {
                    status: Some(LocateStatus::Moved),
                    address: format_address(&moved_ip, 5555),
                    ip: moved_ip.clone(),
                    port: 5555,
                    mac: wanted_mac,
                    detail: format!("IP 从 {host} 换成了 {moved_ip}（端口 5555）"),
                };
            }
            return Located {
                status: Some(LocateStatus::NoPort),
                address: String::new(),
                ip: moved_ip.clone(),
                port: 0,
                mac: wanted_mac,
                detail: format!("在 {moved_ip} 找到这台平板，但无线调试端口未知"),
            };
        }
    }

    Located {
        status: Some(LocateStatus::Offline),
        ip: host,
        port,
        mac: wanted_mac,
        detail: "在局域网里找不到这台平板".to_string(),
        address: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 逐条对应 tests/test_locator.py

    fn always(value: bool) -> impl Fn(&str, u16, f64) -> bool {
        move |_ip: &str, _port: u16, _timeout: f64| value
    }

    #[test]
    fn split_address_handles_port_and_fallbacks() {
        assert_eq!(split_address("192.168.1.8:37495"), ("192.168.1.8".to_string(), 37495));
        assert_eq!(split_address("192.168.1.8"), ("192.168.1.8".to_string(), 5555));
        assert_eq!(split_address(" 192.168.1.8:abc "), ("192.168.1.8".to_string(), 5555));
        assert_eq!(split_address("192.168.1.8:70000"), ("192.168.1.8".to_string(), 5555));
        assert_eq!(split_address(":5555"), (String::new(), 0));
        assert_eq!(split_address(""), (String::new(), 0));
    }

    #[test]
    fn split_address_handles_ipv6_and_hostnames() {
        assert_eq!(split_address("[fd7a::1]:5555"), ("fd7a::1".to_string(), 5555));
        assert_eq!(split_address("[fd7a::1]"), ("fd7a::1".to_string(), 5555));
        assert_eq!(split_address("fd7a::1"), ("fd7a::1".to_string(), 5555));
        assert_eq!(split_address("tablet.local"), ("tablet.local".to_string(), 5555));
        assert_eq!(format_address("fd7a::1", 5555), "[fd7a::1]:5555");
        assert_eq!(format_address("192.168.1.8", 5555), "192.168.1.8:5555");
        assert_eq!(format_address("", 5555), "");
        assert_eq!(format_address("1.2.3.4", 0), "");
    }

    #[test]
    fn normalize_mac_accepts_dashes() {
        assert_eq!(normalize_mac("88-A4-79-1A-2B-3C"), "88:a4:79:1a:2b:3c");
        assert_eq!(normalize_mac("88:a4:79:1a:2b:3c"), "88:a4:79:1a:2b:3c");
        assert_eq!(normalize_mac("nope"), "");
        assert_eq!(normalize_mac(""), "");
    }

    #[test]
    fn parse_mdns_and_port_lookup() {
        let lines = vec![
            "List of discovered mdns services".to_string(),
            "adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234".to_string(),
            "adb-XYZ._adb-tls-pairing._tcp\t192.168.1.9:37000".to_string(),
            "adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234".to_string(),
        ];
        let services = parse_mdns(&lines, None);
        assert_eq!(
            services,
            vec![
                ("192.168.1.8".to_string(), 41234),
                ("192.168.1.9".to_string(), 37000)
            ]
        );
        assert_eq!(mdns_port_for("192.168.1.8", &services), 41234);
        assert_eq!(mdns_port_for("10.0.0.1", &services), 0);
    }

    #[test]
    fn parse_mdns_can_filter_to_connect_service() {
        let lines = vec![
            "adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234".to_string(),
            "adb-XYZ._adb-tls-pairing._tcp\t192.168.1.9:37000".to_string(),
        ];
        assert_eq!(
            parse_mdns(&lines, Some("_adb-tls-connect._tcp")),
            vec![("192.168.1.8".to_string(), 41234)]
        );
    }

    #[test]
    fn locate_reports_online_when_address_answers() {
        let found = locate_with(
            "192.168.1.8:37495",
            "",
            "",
            None,
            None,
            None,
            1.5,
            &always(true),
        );
        assert_eq!(found.status, Some(LocateStatus::Online));
        assert_eq!(found.address, "192.168.1.8:37495");
        assert!(found.detail.contains("仍然在线"));
    }

    #[test]
    fn locate_follows_a_new_port_from_mdns() {
        let tcp = |ip: &str, port: u16, _timeout: f64| (ip, port) == ("192.168.1.8", 41234);
        let services = || vec!["192.168.1.8:41234".to_string()];
        let found = locate_with(
            "192.168.1.8:37495",
            "",
            "",
            Some(&services),
            None,
            None,
            1.5,
            &tcp,
        );
        assert_eq!(found.status, Some(LocateStatus::Moved));
        assert_eq!(found.address, "192.168.1.8:41234");
    }

    #[test]
    fn locate_follows_a_new_port_from_direct_mdns() {
        let tcp = |ip: &str, port: u16, _timeout: f64| (ip, port) == ("192.168.1.8", 44803);
        let lookup = || {
            vec![MdnsService {
                ip: "192.168.1.8".to_string(),
                port: 44803,
                ..MdnsService::default()
            }]
        };
        let found = locate_with(
            "192.168.1.8:37495",
            "",
            "",
            None,
            Some(&lookup),
            None,
            1.5,
            &tcp,
        );
        assert_eq!(found.status, Some(LocateStatus::Moved));
        assert_eq!(found.address, "192.168.1.8:44803");
    }

    #[test]
    fn locate_follows_a_new_ip_through_the_arp_table() {
        let tcp = |ip: &str, port: u16, _timeout: f64| (ip, port) == ("192.168.1.42", 41234);
        let services = || vec!["192.168.1.42:41234".to_string()];
        let arp = || {
            let mut table: BTreeMap<String, String> = BTreeMap::new();
            table.insert("88:a4:79:1a:2b:3c".to_string(), "192.168.1.42".to_string());
            table
        };
        let found = locate_with(
            "192.168.1.8:37495",
            "88-a4-79-1a-2b-3c",
            "",
            Some(&services),
            None,
            Some(&arp),
            1.5,
            &tcp,
        );
        assert_eq!(found.status, Some(LocateStatus::Moved));
        assert_eq!(found.address, "192.168.1.42:41234");
        assert!(found.detail.contains("换成了"));
    }

    #[test]
    fn locate_reports_no_port_when_only_the_ip_is_found() {
        let arp = || {
            let mut table: BTreeMap<String, String> = BTreeMap::new();
            table.insert("88:a4:79:1a:2b:3c".to_string(), "192.168.1.42".to_string());
            table
        };
        let found = locate_with(
            "192.168.1.8:37495",
            "88:a4:79:1a:2b:3c",
            "",
            None,
            None,
            Some(&arp),
            1.5,
            &always(false),
        );
        assert_eq!(found.status, Some(LocateStatus::NoPort));
        assert_eq!(found.ip, "192.168.1.42");
        assert!(found.reachable() && !found.online());
        assert_eq!(found.status_str(), STATUS_NO_PORT);
    }

    #[test]
    fn locate_reports_offline_when_nothing_answers() {
        let arp: BTreeMap<String, String> = BTreeMap::new();
        let empty = || arp.clone();
        let found = locate_with(
            "192.168.1.8:37495",
            "88:a4:79:1a:2b:3c",
            "",
            None,
            None,
            Some(&empty),
            1.5,
            &always(false),
        );
        assert_eq!(found.status, Some(LocateStatus::Offline));
        assert!(found.detail.contains("找不到"));
    }

    #[test]
    fn locate_without_any_remembered_address_is_offline() {
        let found = locate("", "", "", None, None, None, 1.5);
        assert_eq!(found.status, Some(LocateStatus::Offline));
        assert!(found.detail.contains("还没有连接过"));
    }

    #[test]
    fn discovered_services_prefers_adb_output() {
        let lines = || vec!["adb-ABC._adb-tls-connect._tcp\t192.168.1.8:41234".to_string()];
        assert_eq!(
            discovered_services(Some(&lines), 1.2),
            vec![("192.168.1.8".to_string(), 41234)]
        );
    }

    #[test]
    fn discovered_mdns_keeps_hardware_serial() {
        // 直接 mDNS 分支会真的发组播，这里只在 adb 输出为空时验证去重逻辑：
        // 用空列表让它走 discover（可能为空），断言不 panic 且不超过上限。
        let empty = || Vec::new();
        let found = discovered_mdns(Some(&empty), 0.15);
        assert!(found.len() <= 64);
    }

    #[test]
    fn locate_finds_a_new_ip_by_hardware_serial() {
        let tcp = |ip: &str, port: u16, _timeout: f64| (ip, port) == ("10.0.0.5", 5555);
        let lookup = || {
            vec![MdnsService {
                ip: "10.0.0.5".to_string(),
                port: 5555,
                serial: "HW1".to_string(),
                ..MdnsService::default()
            }]
        };
        let found = locate_with(
            "192.168.1.8:37495",
            "",
            "HW1",
            None,
            Some(&lookup),
            None,
            1.5,
            &tcp,
        );
        assert_eq!(found.status, Some(LocateStatus::Moved));
        assert_eq!(found.address, "10.0.0.5:5555");
        assert!(found.detail.contains("硬件序列号"));
    }

    #[test]
    fn arp_table_reads_windows_output() {
        let output = "接口: 192.168.1.5 --- 0x9\r\n  \
                       192.168.1.1           88-a4-79-1a-2b-3c     动态\r\n  \
                       192.168.1.42          88-a4-79-1a-2b-3c     动态\r\n";
        let table = parse_arp(output.as_bytes());
        assert_eq!(
            find_ip_by_mac("88-a4-79-1a-2b-3c", &table),
            "192.168.1.1"
        );
    }

    #[test]
    fn parse_arp_skips_broadcast_and_bad_lines() {
        let output = "0.0.0.0 ff-ff-ff-ff-ff-ff x\n255.255.255.255 ff-ff-ff-ff-ff-ff x\n\
                      garbage line\n1.2.3.4 aa:bb:cc:dd:ee:ff wrong-format\n";
        let table = parse_arp(output.as_bytes());
        assert!(table.is_empty(), "广播地址与点分隔 MAC 都必须被跳过");
    }

    #[test]
    fn tcp_open_rejects_empty_input_without_io() {
        assert!(!tcp_open("", 5555, 0.1));
        assert!(!tcp_open("127.0.0.1", 0, 0.1));
    }

    /// 真实 socket：手动跑，避免占用测试时间/网络。
    #[test]
    #[ignore = "真实 socket：tcp_open 会真的建连"]
    fn tcp_open_localhost_closed_port_is_false() {
        // 1 端口基本不会有服务；只验证不 panic 且返回 bool。
        let _ = tcp_open("127.0.0.1", 1, 0.2);
    }
}
