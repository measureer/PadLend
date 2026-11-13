//! 直接用 mDNS 找 adb 无线调试端口（对应 tablet/mdns.py，1.0.2）。
//!
//! Windows 上的 adb 不一定带 mDNS 守护进程：adb mdns check 会返回
//! "mdns daemon unavailable"，adb mdns services 只有表头。平板自己仍然会在
//! 5353 端口广播 _adb-tls-connect._tcp.local，所以这里用标准库直接发一次 DNS
//! 查询、解析响应，拿到当前的随机端口。
//!
//! 只依赖 std::net / std::time，不需要 Bonjour 或 zeroconf。解析函数是纯函数，
//! 方便单测；真正的网络查询集中在 [discover]（测试里一律 #[ignore]）。

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// 无线调试的连接服务类型。
pub const SERVICE_TYPE: &str = "_adb-tls-connect._tcp.local";
/// mDNS 组播地址。
pub const MDNS_GROUP: &str = "224.0.0.251";
/// mDNS 端口。
pub const MDNS_PORT: u16 = 5353;

/// DNS 记录类型 A。
pub const TYPE_A: u16 = 1;
/// DNS 记录类型 PTR。
pub const TYPE_PTR: u16 = 12;
/// DNS 记录类型 TXT。
pub const TYPE_TXT: u16 = 16;
/// DNS 记录类型 SRV。
pub const TYPE_SRV: u16 = 33;
/// DNS class IN。
pub const CLASS_IN: u16 = 1;
/// DNS class 的 QU 位（请求单播响应）。
pub const CLASS_QU: u16 = 0x8000;
/// 单个响应里最多接受的记录条数（防病态包）。
const MAX_RECORDS: usize = 256;
/// 默认收集窗口（秒）。
pub const DEFAULT_TIMEOUT: f64 = 1.2;

/// mDNS 解析错误；对应 Python 里抛出的 ValueError。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MdnsError {
    /// 名称片段长度不在 1..=63（Python：mDNS 名称片段长度非法: {part!r}）。
    #[error("mDNS 名称片段长度非法: {label:?}")]
    LabelLength {
        /// 违规的片段。
        label: String,
    },
    /// 名称片段含非 ASCII 字符（Python 的 .encode("ascii") 会抛 UnicodeEncodeError）。
    #[error("mDNS 名称片段含非 ASCII 字符: {label:?}")]
    NonAsciiLabel {
        /// 违规的片段。
        label: String,
    },
    /// 编码后的名称超过 255 字节。
    #[error("mDNS 名称过长")]
    NameTooLong,
    /// 读名称时偏移越界。
    #[error("DNS 名称越界")]
    NameOutOfBounds,
    /// 压缩指针指向的位置越界。
    #[error("DNS 压缩指针越界")]
    PointerOutOfBounds,
    /// 压缩指针成环（跳转超过 16 次）。
    #[error("DNS 压缩指针成环")]
    PointerLoop,
    /// 使用了不支持的标签类型（0x40 / 0x80 位）。
    #[error("不支持的 DNS 标签类型")]
    UnsupportedLabelType,
    /// 标签内容越界。
    #[error("DNS 标签越界")]
    LabelOutOfBounds,
    /// 名称标签过多（> 63）。
    #[error("DNS 名称标签过多")]
    TooManyLabels,
}

/// 一条 adb 无线调试服务广播（对应 Python 的 MdnsService）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MdnsService {
    /// 平板当前 IP。
    pub ip: String,
    /// 无线调试端口。
    pub port: u16,
    /// 服务实例名（已小写化？不——保留原始大小写，与 Python 一致）。
    pub name: String,
    /// SRV 记录的目标主机名。
    pub target: String,
    /// TXT 里的 serial（硬件序列号）。
    pub serial: String,
    /// TXT 里的 model。
    pub model: String,
}

/// 把 "a.b.local" 编码成 DNS 名称（末尾 0 字节）。
pub fn encode_name(name: &str) -> Result<Vec<u8>, MdnsError> {
    let parts: Vec<&str> = name
        .trim_matches('.')
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    let mut out: Vec<u8> = Vec::new();
    for part in parts {
        if !part.is_ascii() {
            return Err(MdnsError::NonAsciiLabel {
                label: part.to_string(),
            });
        }
        let raw = part.as_bytes();
        if raw.is_empty() || raw.len() >= 64 {
            return Err(MdnsError::LabelLength {
                label: part.to_string(),
            });
        }
        out.push(raw.len() as u8);
        out.extend_from_slice(raw);
    }
    out.push(0);
    if out.len() > 255 {
        return Err(MdnsError::NameTooLong);
    }
    Ok(out)
}

/// 构造一个单问题 DNS 查询；qclass 带 QU 位时请求单播响应。
pub fn build_query(service: &str, qclass: u16) -> Result<Vec<u8>, MdnsError> {
    // header: id=0 flags=0 qdcount=1 ancount=0 nscount=0 arcount=0
    let mut out = vec![0u8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(&encode_name(service)?);
    out.extend_from_slice(&TYPE_PTR.to_be_bytes());
    out.extend_from_slice(&qclass.to_be_bytes());
    Ok(out)
}

/// 默认 qclass 的查询（对应 Python 的 build_query(service)）。
pub fn build_query_default(service: &str) -> Result<Vec<u8>, MdnsError> {
    build_query(service, CLASS_IN)
}

/// 读 DNS 名称，支持压缩指针；返回 (名称, 下一个记录偏移)。
pub fn read_name(data: &[u8], offset: usize) -> Result<(String, usize), MdnsError> {
    let mut labels: Vec<String> = Vec::new();
    let mut next_offset: Option<usize> = None;
    let mut jumps = 0u32;
    let mut offset = offset;
    loop {
        if offset >= data.len() {
            return Err(MdnsError::NameOutOfBounds);
        }
        let length = data[offset];
        if length == 0 {
            offset += 1;
            if next_offset.is_none() {
                next_offset = Some(offset);
            }
            return Ok((labels.join("."), next_offset.unwrap_or(offset)));
        }
        if length & 0xC0 == 0xC0 {
            if offset + 1 >= data.len() {
                return Err(MdnsError::PointerOutOfBounds);
            }
            let pointer = ((usize::from(length) & 0x3F) << 8) | usize::from(data[offset + 1]);
            if next_offset.is_none() {
                next_offset = Some(offset + 2);
            }
            jumps += 1;
            if jumps > 16 {
                return Err(MdnsError::PointerLoop);
            }
            offset = pointer;
            continue;
        }
        if length & 0xC0 != 0 {
            return Err(MdnsError::UnsupportedLabelType);
        }
        let start = offset + 1;
        let end = start + usize::from(length);
        if end > data.len() {
            return Err(MdnsError::LabelOutOfBounds);
        }
        labels.push(String::from_utf8_lossy(&data[start..end]).into_owned());
        if labels.len() > 63 {
            return Err(MdnsError::TooManyLabels);
        }
        offset = end;
    }
}

/// 解析 TXT 记录：key=value 逐条切分，重复 key 只保留第一次。
pub fn parse_txt(rdata: &[u8]) -> BTreeMap<String, String> {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut pos = 0usize;
    while pos < rdata.len() {
        let length = usize::from(rdata[pos]);
        pos += 1;
        if length == 0 || pos + length > rdata.len() {
            continue;
        }
        let text = String::from_utf8_lossy(&rdata[pos..pos + length]).into_owned();
        pos += length;
        let mut split = text.splitn(2, '=');
        let key = split.next().unwrap_or_default();
        let value = split.next();
        if !key.is_empty() {
            if let Some(value) = value {
                values.entry(key.to_string()).or_insert_with(|| value.to_string());
            }
        }
    }
    values
}

/// 这个服务名是不是无线调试的连接服务。
pub fn is_connect_service(name: &str) -> bool {
    let lowered = name.to_lowercase();
    let trimmed = lowered.trim_end_matches('.');
    trimmed.ends_with(SERVICE_TYPE)
}

/// 插入有序映射：同 key 覆盖值但保留首次出现的位置（Python dict 语义）。
struct OrderedMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> OrderedMap<V> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn insert(&mut self, key: String, value: V) {
        if let Some(slot) = self.entries.iter_mut().find(|(item, _)| *item == key) {
            slot.1 = value;
            return;
        }
        self.entries.push((key, value));
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.entries
            .iter()
            .find(|(item, _)| item == key)
            .map(|(_, value)| value)
    }
}

/// 解析一个 mDNS 响应；坏包返回空列表，绝不抛给调用方。
pub fn parse_response(data: &[u8], source_ip: &str) -> Vec<MdnsService> {
    if data.len() < 12 {
        return Vec::new();
    }
    let read_u16 = |at: usize| -> u16 { u16::from_be_bytes([data[at], data[at + 1]]) };
    let flags = read_u16(2);
    let qdcount = read_u16(4);
    let ancount = read_u16(6);
    let nscount = read_u16(8);
    let arcount = read_u16(10);
    if flags & 0x8000 == 0 {
        return Vec::new();
    }

    let mut offset = 12usize;
    for _ in 0..usize::from(qdcount).min(64) {
        match read_name(data, offset) {
            Ok((_name, next)) => offset = next + 4, // QTYPE + QCLASS
            Err(_) => return Vec::new(),
        }
        if offset > data.len() {
            return Vec::new();
        }
    }

    let mut records: Vec<(String, u16, usize, usize)> = Vec::new();
    let total = usize::from(ancount)
        .saturating_add(usize::from(nscount))
        .saturating_add(usize::from(arcount))
        .min(MAX_RECORDS);
    for _ in 0..total {
        let name = match read_name(data, offset) {
            Ok((name, next)) => {
                offset = next;
                name
            }
            Err(_) => break,
        };
        if offset + 10 > data.len() {
            break;
        }
        let rtype = read_u16(offset);
        let rdlength = usize::from(read_u16(offset + 8));
        offset += 10;
        if offset + rdlength > data.len() {
            break;
        }
        records.push((name, rtype, offset, rdlength));
        offset += rdlength;
    }

    let mut addresses: OrderedMap<String> = OrderedMap::new();
    let mut services: OrderedMap<(u16, String)> = OrderedMap::new();
    let mut text: OrderedMap<BTreeMap<String, String>> = OrderedMap::new();
    for (name, rtype, rdata, rdlength) in records {
        let key = name.to_lowercase().trim_end_matches('.').to_string();
        if rtype == TYPE_A && rdlength >= 4 {
            let octets = [
                data[rdata],
                data[rdata + 1],
                data[rdata + 2],
                data[rdata + 3],
            ];
            addresses.insert(key, Ipv4Addr::from(octets).to_string());
        } else if rtype == TYPE_SRV && rdlength >= 6 {
            let port = u16::from_be_bytes([data[rdata + 4], data[rdata + 5]]);
            match read_name(data, rdata + 6) {
                Ok((target, _)) => services.insert(key, (port, target)),
                Err(_) => continue,
            }
        } else if rtype == TYPE_TXT {
            let payload = &data[rdata..rdata + rdlength];
            text.insert(key, parse_txt(payload));
        }
    }

    let mut found: Vec<MdnsService> = Vec::new();
    let mut seen: Vec<(String, u16)> = Vec::new();
    for (name, (port, target)) in services.entries.iter() {
        if !is_connect_service(name) || *port == 0 {
            continue;
        }
        let target_key = target.to_lowercase().trim_end_matches('.').to_string();
        let mut ip = addresses
            .get(&target_key)
            .cloned()
            .unwrap_or_default();
        if ip.is_empty() {
            ip = source_ip.trim().to_string();
        }
        if ip.is_empty() {
            continue;
        }
        let pair = (ip.clone(), *port);
        if seen.contains(&pair) {
            continue;
        }
        seen.push(pair);
        let info = text.get(name);
        found.push(MdnsService {
            ip,
            port: *port,
            name: name.clone(),
            target: target.clone(),
            serial: info
                .and_then(|values| values.get("serial"))
                .cloned()
                .unwrap_or_default(),
            model: info
                .and_then(|values| values.get("model"))
                .cloned()
                .unwrap_or_default(),
        });
    }
    found
}

/// 发一次 mDNS 查询，收集 timeout 秒内的响应（网络入口，单独隔离）。
///
/// 5353 能绑定就加入组播组；绑不上（被 Bonjour 等占用）时改用临时端口并在
/// 查询里置 QU 位，请求对方单播回应。网络不可用时返回空列表。
pub fn discover(timeout: f64) -> Vec<MdnsService> {
    discover_service(timeout, SERVICE_TYPE)
}

/// 同上，可指定服务类型。
pub fn discover_service(timeout: f64, service: &str) -> Vec<MdnsService> {
    let budget = if timeout > 0.1 { timeout } else { 0.1 };
    let socket = match UdpSocket::bind(("0.0.0.0", MDNS_PORT)) {
        Ok(socket) => {
            let joined = socket
                .join_multicast_v4(&Ipv4Addr::UNSPECIFIED, &Ipv4Addr::UNSPECIFIED)
                .is_ok();
            (socket, true, joined)
        }
        Err(_) => match UdpSocket::bind(("0.0.0.0", 0)) {
            Ok(socket) => (socket, false, false),
            Err(err) => {
                eprintln!("[tablet.mdns] mDNS 查询不可用: {err}");
                return Vec::new();
            }
        },
    };
    let (socket, bound, joined) = socket;
    if bound && !joined {
        eprintln!("[tablet.mdns] mDNS 组播加入失败");
    }
    let _ = socket.set_multicast_ttl_v4(1);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));

    let qclass = if joined { CLASS_IN } else { CLASS_IN | CLASS_QU };
    let query = match build_query(service, qclass) {
        Ok(query) => query,
        Err(err) => {
            eprintln!("[tablet.mdns] mDNS 查询构造失败: {err}");
            return Vec::new();
        }
    };
    let group = match MDNS_GROUP.parse::<Ipv4Addr>() {
        Ok(group) => group,
        Err(_) => return Vec::new(),
    };
    let destination = SocketAddr::from((group, MDNS_PORT));

    let deadline = Instant::now() + Duration::from_secs_f64(budget);
    let mut next_send = Instant::now();
    let mut found: Vec<MdnsService> = Vec::new();
    let mut seen: Vec<(String, u16)> = Vec::new();
    let mut buffer = [0u8; 9000];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            if socket.send_to(&query, destination).is_err() {
                break;
            }
            next_send = now + Duration::from_millis(350);
        }
        let (size, address) = match socket.recv_from(&mut buffer) {
            Ok(received) => received,
            Err(error) => match error.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => continue,
                std::io::ErrorKind::Interrupted => continue,
                _ => break,
            },
        };
        let source = address.ip().to_string();
        for item in parse_response(&buffer[..size], &source) {
            let pair = (item.ip.clone(), item.port);
            if seen.contains(&pair) {
                continue;
            }
            seen.push(pair);
            found.push(item);
        }
    }
    if !found.is_empty() {
        let summary = found
            .iter()
            .map(|item| format!("{}:{}", item.ip, item.port))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[tablet.mdns] mDNS 找到 {} 个 adb 服务: {summary}",
            found.len()
        );
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    // 逐条对应 tests/test_mdns.py

    const INSTANCE: &str = "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp.local";
    const TARGET: &str = "Android_KXJHTBDK.local";
    const IP: &str = "192.0.2.10";
    const PORT: u16 = 44803;

    fn name(value: &str) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for label in value.trim_matches('.').split('.') {
            let raw = label.as_bytes();
            out.push(raw.len() as u8);
            out.extend_from_slice(raw);
        }
        out.push(0);
        out
    }

    fn record(name: &[u8], rtype: u16, rdata: &[u8]) -> Vec<u8> {
        let mut out = name.to_vec();
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
        out
    }

    fn txt(values: &[&str]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for value in values {
            let raw = value.as_bytes();
            out.push(raw.len() as u8);
            out.extend_from_slice(raw);
        }
        out
    }

    fn response(with_a: bool, compress_srv_name: bool) -> Vec<u8> {
        let service = name(SERVICE_TYPE);
        let instance = name(INSTANCE);
        let target = name(TARGET);
        let mut out: Vec<u8> = Vec::new();
        let ancount: u16 = if with_a { 4 } else { 3 };
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0x8400u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&ancount.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        out.extend_from_slice(&record(&service, TYPE_PTR, &instance));
        let instance_offset = 12 + service.len() + 10;
        let srv_name = if compress_srv_name {
            let mut pointer = vec![0xC0u8 | ((instance_offset >> 8) as u8)];
            pointer.push((instance_offset & 0xFF) as u8);
            pointer
        } else {
            instance.clone()
        };
        let mut srv_data = vec![0u8, 0, 0, 0];
        srv_data.extend_from_slice(&PORT.to_be_bytes());
        srv_data.extend_from_slice(&target);
        out.extend_from_slice(&record(&srv_name, TYPE_SRV, &srv_data));
        out.extend_from_slice(&record(
            &instance,
            TYPE_TXT,
            &txt(&["serial=AM4NBB5C23001691", "model=CGA-W00"]),
        ));
        if with_a {
            out.extend_from_slice(&record(
                &target,
                TYPE_A,
                &[192, 0, 2, 10],
            ));
        }
        out
    }

    #[test]
    fn parse_response_with_compressed_srv_name() {
        let found = parse_response(&response(true, true), "10.0.0.1");
        assert_eq!(found.len(), 1);
        let item = &found[0];
        assert_eq!((item.ip.as_str(), item.port), (IP, PORT));
        assert_eq!(item.serial, "AM4NBB5C23001691");
        assert_eq!(item.model, "CGA-W00");
        assert_eq!(item.target, TARGET);
    }

    #[test]
    fn parse_response_without_a_uses_source_ip() {
        let found = parse_response(&response(false, true), IP);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ip, IP);
        assert_eq!(found[0].port, PORT);
    }

    #[test]
    fn parse_response_deduplicates_same_endpoint() {
        let data = response(true, true);
        let mut duplicate = record(
            &name(INSTANCE),
            TYPE_SRV,
            &{
                let mut rdata = vec![0u8, 0, 0, 0];
                rdata.extend_from_slice(&PORT.to_be_bytes());
                rdata.extend_from_slice(&name(TARGET));
                rdata
            },
        );
        let mut packet: Vec<u8> = Vec::new();
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&0x8400u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&5u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&data[12..]);
        packet.append(&mut duplicate);
        let found = parse_response(&packet, IP);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].ip.as_str(), found[0].port), (IP, PORT));
    }

    #[test]
    fn parse_response_rejects_bad_packets() {
        assert!(parse_response(&[], "").is_empty());
        assert!(parse_response(&[0u8; 11], "").is_empty());
        assert!(parse_response(&[0u8; 12], "").is_empty());
        // 查询（QR=0）不是响应，必须忽略。
        let query = build_query_default(SERVICE_TYPE).expect("构造查询");
        assert!(parse_response(&query, IP).is_empty());
        // 截断的响应不能抛异常。
        let truncated = response(true, true);
        let _ = parse_response(&truncated[..truncated.len() - 40], IP);
    }

    #[test]
    fn build_query_has_one_ptr_question() {
        let query = build_query(SERVICE_TYPE, CLASS_IN | CLASS_QU).expect("构造查询");
        assert_eq!(&query[..2], &[0, 0]);
        assert_eq!(u16::from_be_bytes([query[2], query[3]]), 0);
        assert_eq!(u16::from_be_bytes([query[4], query[5]]), 1);
        assert_eq!(u16::from_be_bytes([query[6], query[7]]), 0);
        assert_eq!(u16::from_be_bytes([query[8], query[9]]), 0);
        assert_eq!(u16::from_be_bytes([query[10], query[11]]), 0);
        let tail = &query[query.len() - 4..];
        assert_eq!(u16::from_be_bytes([tail[0], tail[1]]), TYPE_PTR);
        assert_eq!(
            u16::from_be_bytes([tail[2], tail[3]]),
            CLASS_IN | CLASS_QU
        );
        assert!(query.windows(name(SERVICE_TYPE).len()).any(|window| window == name(SERVICE_TYPE)));
    }

    #[test]
    fn encode_name_rejects_bad_labels() {
        assert_eq!(
            encode_name("a..b").expect("空片段应被跳过"),
            vec![1, b'a', 1, b'b', 0]
        );
        let long = "x".repeat(64);
        assert_eq!(
            encode_name(&long).expect_err("64 字节片段必须拒绝"),
            MdnsError::LabelLength { label: long.clone() }
        );
        assert!(matches!(
            encode_name("中文.local").expect_err("非 ASCII 必须拒绝"),
            MdnsError::NonAsciiLabel { .. }
        ));
    }

    #[test]
    fn read_name_follows_pointers_and_detects_loops() {
        // 偏移 0: "local" 指针指向 7，偏移 7: "x.local"
        let packet = vec![
            5, b'l', b'o', b'c', b'a', b'l', 0, 1, b'x', 0xC0, 0x00,
        ];
        let (name, next) = read_name(&packet, 7).expect("读名称");
        assert_eq!(name, "x.local");
        assert_eq!(next, 11);

        // 自指指针必须报环
        let looping = vec![0xC0, 0x00];
        assert_eq!(read_name(&looping, 0).expect_err("成环"), MdnsError::PointerLoop);
        // 越界偏移
        assert_eq!(read_name(&[0u8; 4], 9).expect_err("越界"), MdnsError::NameOutOfBounds);
    }

    #[test]
    fn parse_txt_keeps_first_value_and_skips_bad_entries() {
        let data = txt(&["serial=ABC", "serial=XYZ", "novalue", "model=Pad"]);
        let values = parse_txt(&data);
        assert_eq!(values.get("serial").map(String::as_str), Some("ABC"));
        assert_eq!(values.get("model").map(String::as_str), Some("Pad"));
        assert!(!values.contains_key("novalue"));
        // 长度越界的条目被跳过
        assert!(parse_txt(&[9, b'a']).is_empty());
    }

    #[test]
    fn is_connect_service_matches_case_insensitively() {
        assert!(is_connect_service(INSTANCE));
        assert!(is_connect_service(&INSTANCE.to_uppercase()));
        assert!(!is_connect_service("adb-XYZ._adb-tls-pairing._tcp.local"));
    }

    /// 真实网络查询：只在手动跑时执行（不依赖真机，但会真的发组播）。
    #[test]
    #[ignore = "真实 socket：手动跑，避免占用 5353 影响其他测试"]
    fn discover_returns_within_budget() {
        let started = Instant::now();
        let found = discover(0.3);
        assert!(started.elapsed() < Duration::from_secs(3));
        // 局域网里有没有平板都可以，只要求不 panic、不越界。
        assert!(found.len() <= 64);
    }
}
