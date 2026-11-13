//! scrcpy-server 的推送 / 启动 / 端口转发 / 清理（对应 tablet/server.py）。
//!
//! 这一层只碰 adb 与本地 jar，不建控制 socket（那是 session.rs 的事）。
//! 关键约束（DESIGN 6.1.2 / 6.1.3、PITFALLS 1.4~1.6）：
//!
//! * 端口转发必须先 --remove 再 forward，否则 adb 会报 already exists；
//! * 每台平板上的 app_process 只接受一次通道，退避重启前必须 kill；
//! * 清理要幂等：kill → forward --remove → 校验（grep scrcpy 计数），
//!   校验失败再补一刀 pkill，否则会留下幽灵 UHID（F7）；
//! * jar 完整性用 vendor/SHA256SUMS 校验；本地 md5 与远端相同就跳过推送。
//!
//! 本模块不依赖外部 crate，所以 md5 / sha256 是手写的（见文件末尾的已知答案
//! 单测）。两者都只用于 jar 比对与完整性校验，不涉及安全边界。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::constants as C;
use crate::tablet::adb::{AdbApi, AdbError, AdbErrorCode, ChildHandle};

/// adb forward 的远端抽象 socket 名。
pub const REMOTE_ABSTRACT: &str = "localabstract:scrcpy";
/// 清理时要杀掉的进程特征（Server、CleanUp、jar 名）。
pub const KILL_PATTERNS: [&str; 3] = [
    "com.genymobile.scrcpy.Server",
    "com.genymobile.scrcpy.CleanUp",
    "scrcpy-server-manual.jar",
];
/// 默认的清理特征（DESIGN 12.3 的"补一刀"用它）。
pub const KILL_PATTERN: &str = KILL_PATTERNS[0];
/// 清理校验命令：数 scrcpy 相关输入设备。
pub const VERIFY_CMD: &str = "dumpsys input | grep -c scrcpy";
/// 清理阶段的超时（秒）。
const CLEANUP_TIMEOUT: f64 = 2.0;

/// scrcpy-server jar 的名称（日志与报错文案里用）。
fn jar_name(jar: &Path) -> String {
    jar.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 读文件算 md5；读失败返回 None（Python 会抛 OSError，这里降级为 None，
/// 调用方按"需要重推"处理）。
pub fn file_md5(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    Some(md5_hex(&data))
}

/// 读文件算 sha256；读失败返回 None。
pub fn file_sha256(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    Some(sha256_hex(&data))
}

/// 从 vendor/SHA256SUMS 里找 jar 的期望摘要；读不到或没有条目返回 None。
///
/// sums 为 None 时用 vendor_dir()/SHA256SUMS。
pub fn expected_jar_sha256(jar: &Path, sums: Option<&Path>) -> Option<String> {
    let path = sums
        .map(|value| value.to_path_buf())
        .unwrap_or_else(|| C::vendor_dir().join("SHA256SUMS"));
    let name = jar_name(jar);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "[tablet.server] 读取 {} 失败，跳过 jar 完整性校验: {error}",
                path.display()
            );
            return None;
        }
    };
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 || parts[0].starts_with('#') {
            continue;
        }
        let entry = parts[parts.len() - 1].trim_start_matches('*');
        if entry == name || entry.ends_with(&format!("/{name}")) {
            return Some(parts[0].to_lowercase());
        }
    }
    eprintln!("[tablet.server] SHA256SUMS 中没有 {name} 的条目，跳过完整性校验");
    None
}

/// scrcpy-server 的生命周期封装（对应 Python 的 ScrcpyServer）。
#[derive(Debug)]
pub struct ScrcpyServer {
    adb: Arc<dyn AdbApi>,
    port: Mutex<Option<u16>>,
    jar: PathBuf,
    remote_jar: String,
    version: String,
    audio: bool,
    sums: Option<PathBuf>,
    proc: Mutex<Option<Box<dyn ChildHandle>>>,
    pushed: AtomicBool,
    last_error: Mutex<String>,
    verify_detail: Mutex<String>,
}

impl ScrcpyServer {
    /// 默认构造：vendor/scrcpy-server-v{SCRCPY_VERSION}.jar、官方远端路径。
    pub fn new(adb: Arc<dyn AdbApi>) -> Self {
        Self {
            adb,
            port: Mutex::new(None),
            jar: C::scrcpy_server_jar(),
            remote_jar: C::SCRCPY_REMOTE_JAR.to_string(),
            version: C::SCRCPY_VERSION.to_string(),
            audio: false,
            sums: None,
            proc: Mutex::new(None),
            pushed: AtomicBool::new(false),
            last_error: Mutex::new(String::new()),
            verify_detail: Mutex::new(String::new()),
        }
    }

    /// 指定端口 / jar / 版本 / 音频开关（等价 Python 构造参数）。
    pub fn with_parts(
        adb: Arc<dyn AdbApi>,
        port: Option<u16>,
        jar: Option<PathBuf>,
        remote_jar: Option<String>,
        version: Option<String>,
        audio: bool,
    ) -> Self {
        let mut server = Self::new(adb);
        server.port = Mutex::new(port);
        if let Some(jar) = jar {
            server.jar = jar;
        }
        if let Some(remote) = remote_jar {
            server.remote_jar = remote;
        }
        if let Some(version) = version {
            server.version = version;
        }
        server.audio = audio;
        server
    }

    /// 指定 SHA256SUMS 路径（None 用 vendor/SHA256SUMS；测试注入不存在路径以跳过校验）。
    pub fn with_sums(mut self, sums: Option<PathBuf>) -> Self {
        self.sums = sums;
        self
    }

    /// 当前转发端口。
    pub fn port(&self) -> Option<u16> {
        self.port.lock().ok().and_then(|guard| *guard)
    }

    /// 是否走音频通道。
    pub fn audio(&self) -> bool {
        self.audio
    }

    /// 本地 jar 路径。
    pub fn jar(&self) -> &Path {
        &self.jar
    }

    /// 最近一次失败原因。
    pub fn last_error(&self) -> String {
        self.last_error
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 最近一次清理校验的结论。
    pub fn verify_detail(&self) -> String {
        self.verify_detail
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 是否已经推送过（诊断用）。
    pub fn pushed(&self) -> bool {
        self.pushed.load(Ordering::SeqCst)
    }

    /// 本地 Popen 是否还活着（对应 Python proc_alive）。
    pub fn proc_alive(&self) -> bool {
        match self.proc.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(child) => child.try_wait().is_none(),
                None => false,
            },
            Err(_) => false,
        }
    }

    /// 本地校验 jar 的 SHA256（对应 verify_local_jar）。
    pub fn verify_local_jar(&self) -> Result<(), AdbError> {
        let Some(expected) = expected_jar_sha256(&self.jar, self.sums.as_deref()) else {
            return Ok(());
        };
        let actual = file_sha256(&self.jar).unwrap_or_default();
        if actual != expected {
            let name = jar_name(&self.jar);
            eprintln!(
                "[tablet.server] jar.sha256_mismatch file={name} expected={expected} actual={actual}"
            );
            return Err(AdbError::new(
                AdbErrorCode::ServerStartFailed,
                format!(
                    "服务端 jar 完整性校验失败：{name} 与 vendor/SHA256SUMS 不一致，安装包可能损坏或被替换，请重新下载 PadLend 发行包"
                ),
            ));
        }
        Ok(())
    }

    /// 按需推送 jar；返回是否真的推送了（对应 push_if_needed）。
    pub fn push_if_needed(&self) -> Result<bool, AdbError> {
        if !self.jar.is_file() {
            let name = jar_name(&self.jar);
            return Err(AdbError::new(
                AdbErrorCode::ServerStartFailed,
                format!(
                    "找不到 scrcpy-server jar（{name}）：从 scrcpy v{} 发布包提取 scrcpy-server 并放到 vendor/ 目录",
                    self.version
                ),
            ));
        }
        self.verify_local_jar()?;
        let local = file_md5(&self.jar);
        if let Some(local) = local {
            if let Some(remote) = self.adb.md5(&self.remote_jar) {
                if remote == local {
                    eprintln!(
                        "[tablet.server] 服务端 jar 已是同一版本（md5={local}），跳过推送"
                    );
                    return Ok(false);
                }
            }
            eprintln!(
                "[tablet.server] 推送服务端 jar（local={} remote={}）",
                self.jar.display(),
                self.remote_jar
            );
            self.adb.push(&self.jar, &self.remote_jar)?;
            self.pushed.store(true, Ordering::SeqCst);
            return Ok(true);
        }
        // 读不到本地文件（不会发生，is_file 已通过）：只能盲推。
        eprintln!(
            "[tablet.server] 推送服务端 jar（local={} remote={}）",
            self.jar.display(),
            self.remote_jar
        );
        self.adb.push(&self.jar, &self.remote_jar)?;
        self.pushed.store(true, Ordering::SeqCst);
        Ok(true)
    }

    /// 启动服务端用的 shell 命令（对应 command()）。
    pub fn command(&self) -> String {
        let port = self.port().unwrap_or(C::DEFAULT_SERVER_PORT);
        let mut args = C::scrcpy_server_args(port, self.audio);
        // scrcpy_server_args 把 SCRCPY_VERSION 固定在索引 4；允许覆盖版本。
        if args.len() > 4 {
            args[4] = self.version.clone();
        }
        args.join(" ")
    }

    /// 后台启动 app_process；已活着则跳过（对应 spawn）。
    pub fn spawn(&self) -> Result<(), AdbError> {
        if self.proc_alive() {
            return Ok(());
        }
        let command = self.command();
        eprintln!("[tablet.server] 启动服务端: {command}");
        let child = match self.adb.popen_shell(&command) {
            Ok(child) => child,
            Err(error) => {
                self.set_last_error(&error.detail);
                return Err(error);
            }
        };
        if let Ok(mut guard) = self.proc.lock() {
            *guard = Some(child);
        }
        Ok(())
    }

    fn set_last_error(&self, detail: &str) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = detail.to_string();
        }
    }

    /// 服务端是否在跑（本地进程或远端 pgrep，对应 running()）。
    pub fn running(&self) -> bool {
        if self.proc_alive() {
            return true;
        }
        KILL_PATTERNS
            .iter()
            .any(|pattern| self.adb.shell_ok(&["pgrep", "-f", pattern], CLEANUP_TIMEOUT))
    }

    /// 分配并建立端口转发（对应 forward）。
    pub fn forward(&self) -> Result<u16, AdbError> {
        let candidates: Vec<u16> = match self.port() {
            Some(port) => vec![port],
            None => C::SERVER_PORT_RANGE.collect(),
        };
        let start = candidates.first().copied().unwrap_or(C::DEFAULT_SERVER_PORT);
        let end = candidates.last().copied().unwrap_or(start);
        let mut errors: Vec<String> = Vec::new();
        for port in &candidates {
            // 旧 forward 必须先摘掉，否则 adb 报 "already exists"。
            self.adb.forward_remove(*port);
            match self.adb.forward(*port, REMOTE_ABSTRACT) {
                Ok(()) => {
                    eprintln!("[tablet.server] adb forward tcp:{port} → {REMOTE_ABSTRACT}");
                    if let Ok(mut guard) = self.port.lock() {
                        *guard = Some(*port);
                    }
                    return Ok(*port);
                }
                Err(error) => {
                    self.set_last_error(&error.detail);
                    errors.push(error.detail);
                }
            }
        }
        if errors.is_empty() {
            return Err(AdbError::new(
                AdbErrorCode::ForwardFailed,
                "没有可用端口".to_string(),
            ));
        }
        let tail = errors
            .iter()
            .skip(errors.len().saturating_sub(3))
            .cloned()
            .collect::<Vec<_>>()
            .join("；");
        Err(AdbError::new(
            AdbErrorCode::ForwardFailed,
            format!("{start}–{end} 全部失败：{tail}"),
        ))
    }

    /// 摘掉当前转发（幂等）。
    pub fn forward_remove(&self) {
        if let Some(port) = self.port() {
            self.adb.forward_remove(port);
        }
    }

    /// 推送 → 清理 → 启动 → 转发，返回端口（对应 start）。
    pub fn start(&self) -> Result<u16, AdbError> {
        self.push_if_needed()?;
        let _ = self.kill(5.0);
        self.spawn()?;
        self.forward()
    }

    /// 杀掉远端残留与本进程启动的服务端（对应 kill）。
    ///
    /// 返回是否出了错（远端 pkill 失败不致命，与 Python 一样只看本地回收）。
    pub fn kill(&self, timeout: f64) -> Result<(), AdbError> {
        for pattern in KILL_PATTERNS {
            self.adb.shell_ok(&["pkill", "-9", "-f", pattern], timeout);
        }
        self.reap_local();
        eprintln!(
            "[tablet.server] 已执行 pkill -9 -f Server / CleanUp / scrcpy-server-manual.jar"
        );
        Ok(())
    }

    /// 回收本地 app_process（terminate → 等 2s → kill → 等 2s）。
    fn reap_local(&self) {
        let mut child = match self.proc.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(handle) = child.as_mut() {
            if handle.try_wait().is_none() {
                handle.terminate();
                if !handle.wait_timeout(Duration::from_secs(2)) {
                    handle.kill();
                    let _ = handle.wait_timeout(Duration::from_secs(2));
                }
            }
        }
    }

    /// 校验远端是否真的没有 scrcpy 残留（对应 verify_clean）。
    pub fn verify_clean(&self, timeout: f64) -> bool {
        // 注意：grep -c 在没有匹配时退出码是 1，所以**不能**先看 rc 再看计数。
        // Python 版也是先解析计数（server.py:268-281），这里必须保持一致，
        // 否则设备其实干净时会被误判为"校验失败"，每次退出都多补一刀 pkill。
        let (rc, text) = self.adb.shell_result(&[VERIFY_CMD], timeout);
        let mut count: Option<i64> = None;
        for line in text.lines().rev() {
            let value = line.trim();
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                count = value.parse::<i64>().ok();
                break;
            }
        }
        let Some(count) = count else {
            self.set_verify_detail(&format!("grep 输出不可解析（rc={rc}）: {text:?}"));
            return false;
        };
        if count != 0 {
            self.set_verify_detail(&format!("仍有 {count} 处 scrcpy 残留"));
            return false;
        }
        self.set_verify_detail("clean");
        true
    }

    fn set_verify_detail(&self, detail: &str) {
        if let Ok(mut guard) = self.verify_detail.lock() {
            *guard = detail.to_string();
        }
    }

    /// 退出序列（对应 stop）：kill → forward --remove → 校验，失败重试一次。
    /// 返回最终是否干净。
    pub fn stop(&self) -> bool {
        if let Err(error) = self.kill(5.0) {
            eprintln!("[tablet.server] kill 失败: {}", error.detail);
        }
        self.forward_remove();
        if self.verify_clean(CLEANUP_TIMEOUT) {
            return true;
        }
        eprintln!(
            "[tablet.server] 清理校验失败：{}；重试一次 pkill",
            self.verify_detail()
        );
        std::thread::sleep(Duration::from_millis(200));
        for pattern in KILL_PATTERNS {
            self.adb.shell_ok(&["pkill", "-9", "-f", pattern], CLEANUP_TIMEOUT);
        }
        self.verify_clean(CLEANUP_TIMEOUT)
    }
}

impl fmt::Display for ScrcpyServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ScrcpyServer(port={:?}, audio={}, jar={})",
            self.port(),
            self.audio,
            self.jar.display()
        )
    }
}

// ---------------------------------------------------------------------------
// 手写摘要（无外部依赖）：只用于 jar 比对与完整性校验
// ---------------------------------------------------------------------------

/// RFC 1321 的 K 表。
const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// RFC 1321 的位移表。
const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// 计算 md5（RFC 1321），返回 32 位小写十六进制。
pub fn md5_hex(data: &[u8]) -> String {
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for index in 0..64 {
            let (f, g) = match index {
                0..=15 => ((b & c) | (!b & d), index),
                16..=31 => ((d & b) | (!d & c), (5 * index + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let next_d = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[index])
                .wrapping_add(words[g]);
            b = b.wrapping_add(sum.rotate_left(MD5_S[index]));
            a = next_d;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in state {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// SHA-256 的 64 个轮常量。
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 计算 sha256（FIPS 180-4），返回 64 位小写十六进制。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for index in 0..16 {
            let start = index * 4;
            w[index] = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    /// 临时目录守卫：Drop 时清掉，避免在用户目录留垃圾。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let serial = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "padlend-server-{}-{}-{}",
                std::process::id(),
                tag,
                serial
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Debug, Default)]
    struct ChildStats {
        alive: bool,
        terminated: usize,
        killed: usize,
    }

    #[derive(Debug)]
    struct FakeChild {
        stats: Arc<Mutex<ChildStats>>,
    }

    impl ChildHandle for FakeChild {
        fn try_wait(&mut self) -> Option<i32> {
            let alive = self
                .stats
                .lock()
                .map(|guard| guard.alive)
                .unwrap_or(false);
            if alive {
                None
            } else {
                Some(0)
            }
        }

        fn terminate(&mut self) {
            if let Ok(mut guard) = self.stats.lock() {
                guard.terminated += 1;
                guard.alive = false;
            }
        }

        fn kill(&mut self) {
            if let Ok(mut guard) = self.stats.lock() {
                guard.killed += 1;
                guard.alive = false;
            }
        }

        fn wait_timeout(&mut self, _timeout: Duration) -> bool {
            !self.stats.lock().map(|guard| guard.alive).unwrap_or(false)
        }

        fn pid(&self) -> Option<u32> {
            Some(4242)
        }
    }

    #[derive(Debug, Default)]
    struct FakeState {
        md5: Option<String>,
        md5_calls: Vec<String>,
        pushes: Vec<(String, String)>,
        forwards: VecDeque<bool>,
        forward_calls: Vec<u16>,
        forward_removes: Vec<u16>,
        shell_ok_queue: VecDeque<bool>,
        shell_ok_calls: Vec<Vec<String>>,
        shell_results: VecDeque<(i32, String)>,
        spawned: Vec<String>,
        stats: Option<Arc<Mutex<ChildStats>>>,
    }

    #[derive(Debug, Clone)]
    struct FakeAdb {
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeAdb {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState::default())),
            }
        }

        fn with_md5(self, md5: Option<&str>) -> Self {
            if let Ok(mut guard) = self.state.lock() {
                guard.md5 = md5.map(|value| value.to_string());
            }
            self
        }

        fn with_forwards(self, results: Vec<bool>) -> Self {
            if let Ok(mut guard) = self.state.lock() {
                guard.forwards = results.into();
            }
            self
        }

        fn with_shell_ok(self, results: Vec<bool>) -> Self {
            if let Ok(mut guard) = self.state.lock() {
                guard.shell_ok_queue = results.into();
            }
            self
        }

        fn with_shell_results(self, results: Vec<(i32, &str)>) -> Self {
            if let Ok(mut guard) = self.state.lock() {
                guard.shell_results = results
                    .into_iter()
                    .map(|(rc, text)| (rc, text.to_string()))
                    .collect();
            }
            self
        }

        fn snapshot(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().expect("fake adb 状态锁")
        }
    }

    impl AdbApi for FakeAdb {
        fn with_serial(&self, _serial: &str) -> Arc<dyn AdbApi> {
            Arc::new(self.clone())
        }

        fn shell_result(&self, _args: &[&str], _timeout: f64) -> (i32, String) {
            let mut guard = self.snapshot();
            guard
                .shell_results
                .pop_front()
                .unwrap_or((0, "0".to_string()))
        }

        fn shell_ok(&self, args: &[&str], _timeout: f64) -> bool {
            let mut guard = self.snapshot();
            guard
                .shell_ok_calls
                .push(args.iter().map(|value| value.to_string()).collect());
            guard.shell_ok_queue.pop_front().unwrap_or(false)
        }

        fn push(&self, local: &Path, remote: &str) -> Result<(), AdbError> {
            let mut guard = self.snapshot();
            guard
                .pushes
                .push((local.display().to_string(), remote.to_string()));
            Ok(())
        }

        fn md5(&self, remote: &str) -> Option<String> {
            let mut guard = self.snapshot();
            guard.md5_calls.push(remote.to_string());
            guard.md5.clone()
        }

        fn forward(&self, port: u16, _remote: &str) -> Result<(), AdbError> {
            let mut guard = self.snapshot();
            guard.forward_calls.push(port);
            if guard.forwards.pop_front().unwrap_or(true) {
                Ok(())
            } else {
                Err(AdbError::new(
                    AdbErrorCode::ForwardFailed,
                    format!("tcp:{port} 被占用"),
                ))
            }
        }

        fn forward_remove(&self, port: u16) {
            if let Ok(mut guard) = self.state.lock() {
                guard.forward_removes.push(port);
            }
        }

        fn popen_shell(&self, command: &str) -> Result<Box<dyn ChildHandle>, AdbError> {
            let mut guard = self.snapshot();
            guard.spawned.push(command.to_string());
            let stats = Arc::new(Mutex::new(ChildStats {
                alive: true,
                terminated: 0,
                killed: 0,
            }));
            guard.stats = Some(stats.clone());
            Ok(Box::new(FakeChild { stats }))
        }
    }

    fn server_with(adb: &FakeAdb, port: Option<u16>) -> ScrcpyServer {
        ScrcpyServer::with_parts(Arc::new(adb.clone()), port, None, None, None, false)
    }

    /// 写一个 jar 与（可选的）SHA256SUMS，返回 (jar, sums)。
    fn write_jar(dir: &TempDir, contents: &[u8], sums_line: Option<&str>) -> (PathBuf, PathBuf) {
        let jar = dir.join("scrcpy-server-test.jar");
        std::fs::write(&jar, contents).expect("写 jar");
        let sums = dir.join("SHA256SUMS");
        match sums_line {
            Some(line) => std::fs::write(&sums, line).expect("写 SHA256SUMS"),
            None => {
                let _ = std::fs::remove_file(&sums);
            }
        }
        (jar, sums)
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_handles_multi_block_input() {
        // FIPS 180-4 官方测试向量：一百万个 a。
        let data = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&data),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn expected_jar_sha256_parses_vendor_sums() {
        let dir = TempDir::new("sums");
        let (jar, sums) = write_jar(
            &dir,
            b"jar",
            Some("# comment\nabcdef0123  scrcpy-server-test.jar\njunk\n"),
        );
        assert_eq!(
            expected_jar_sha256(&jar, Some(&sums)),
            Some("abcdef0123".to_string())
        );
    }

    #[test]
    fn expected_jar_sha256_accepts_starred_and_path_entries() {
        let dir = TempDir::new("sums2");
        let (jar, sums) = write_jar(
            &dir,
            b"jar",
            Some("0011  *scrcpy-server-test.jar\n0022  vendor/scrcpy-server-test.jar\n"),
        );
        assert_eq!(
            expected_jar_sha256(&jar, Some(&sums)),
            Some("0011".to_string())
        );
    }

    #[test]
    fn expected_jar_sha256_returns_none_without_entry_or_file() {
        let dir = TempDir::new("sums3");
        let (jar, sums) = write_jar(&dir, b"jar", Some("00  other.jar\n"));
        assert_eq!(expected_jar_sha256(&jar, Some(&sums)), None);
        let missing = dir.join("no-sums");
        assert_eq!(expected_jar_sha256(&jar, Some(&missing)), None);
    }

    #[test]
    fn verify_local_jar_rejects_sha_mismatch() {
        let dir = TempDir::new("verify");
        let (jar, sums) = write_jar(
            &dir,
            b"hello",
            Some("0000000000000000000000000000000000000000000000000000000000000000  scrcpy-server-test.jar\n"),
        );
        let adb = FakeAdb::new();
        let server =
            ScrcpyServer::with_parts(Arc::new(adb), Some(2000), Some(jar), None, None, false)
                .with_sums(Some(sums));
        let error = server.verify_local_jar().expect_err("摘要不符必须失败");
        assert_eq!(error.code, AdbErrorCode::ServerStartFailed);
        assert!(error.detail.contains("完整性校验失败"));
    }

    #[test]
    fn verify_local_jar_passes_without_sums() {
        let dir = TempDir::new("verify2");
        let (jar, sums) = write_jar(&dir, b"hello", None);
        let server = ScrcpyServer::with_parts(
            Arc::new(FakeAdb::new()),
            Some(2000),
            Some(jar),
            None,
            None,
            false,
        )
        .with_sums(Some(sums));
        assert!(server.verify_local_jar().is_ok());
    }

    #[test]
    fn push_is_skipped_when_remote_md5_matches() {
        let dir = TempDir::new("push1");
        let (jar, sums) = write_jar(&dir, b"same-bytes", None);
        let local_md5 = file_md5(&jar).expect("本地 md5");
        let adb = FakeAdb::new().with_md5(Some(&local_md5));
        let server = ScrcpyServer::with_parts(
            Arc::new(adb.clone()),
            Some(2000),
            Some(jar),
            None,
            None,
            false,
        )
        .with_sums(Some(sums));
        assert!(!server.push_if_needed().expect("应当跳过"));
        let guard = adb.snapshot();
        assert!(guard.pushes.is_empty());
        assert_eq!(guard.md5_calls, vec![C::SCRCPY_REMOTE_JAR.to_string()]);
        assert!(!server.pushed());
    }

    #[test]
    fn push_happens_when_remote_md5_differs() {
        let dir = TempDir::new("push2");
        let (jar, sums) = write_jar(&dir, b"new-bytes", None);
        let adb = FakeAdb::new().with_md5(Some("ffffffffffffffffffffffffffffffff"));
        let server = ScrcpyServer::with_parts(
            Arc::new(adb.clone()),
            Some(2000),
            Some(jar),
            None,
            None,
            false,
        )
        .with_sums(Some(sums));
        assert!(server.push_if_needed().expect("应当推送"));
        let guard = adb.snapshot();
        assert_eq!(guard.pushes.len(), 1);
        assert_eq!(guard.pushes[0].1, C::SCRCPY_REMOTE_JAR.to_string());
        assert!(server.pushed());
    }

    #[test]
    fn push_reports_missing_jar_with_e103() {
        let dir = TempDir::new("push3");
        let missing = dir.join("missing.jar");
        let server = ScrcpyServer::with_parts(
            Arc::new(FakeAdb::new()),
            Some(2000),
            Some(missing),
            None,
            None,
            false,
        );
        let error = server.push_if_needed().expect_err("缺 jar 必须失败");
        assert_eq!(error.code, AdbErrorCode::ServerStartFailed);
        assert!(error.detail.contains("找不到 scrcpy-server jar"));
    }

    #[test]
    fn command_uses_configured_port_version_and_audio() {
        let server = ScrcpyServer::with_parts(
            Arc::new(FakeAdb::new()),
            Some(1234),
            None,
            None,
            Some("9.9.9".to_string()),
            true,
        );
        let command = server.command();
        assert!(command.contains("9.9.9"), "command={command}");
        assert!(command.contains("audio=true"), "command={command}");
        assert!(command.contains("tunnel_forward=true"), "command={command}");
    }

    #[test]
    fn forward_falls_back_to_the_next_port() {
        let adb = FakeAdb::new().with_forwards(vec![false, false, true]);
        let server = server_with(&adb, None);
        let port = server.forward().expect("第三个端口应当成功");
        assert_eq!(port, C::SERVER_PORT_RANGE.start + 2);
        assert_eq!(server.port(), Some(port));
        let guard = adb.snapshot();
        let expected: Vec<u16> = C::SERVER_PORT_RANGE.take(3).collect();
        assert_eq!(guard.forward_calls, expected);
        assert_eq!(guard.forward_removes, expected);
    }

    #[test]
    fn forward_reports_e104_when_every_port_fails() {
        let adb = FakeAdb::new().with_forwards(vec![false, false]);
        let server = server_with(&adb, Some(2000));
        let error = server.forward().expect_err("必须失败");
        assert_eq!(error.code, AdbErrorCode::ForwardFailed);
        assert!(
            error.detail.contains("2000–2000 全部失败"),
            "detail={}",
            error.detail
        );
    }

    #[test]
    fn verify_clean_reads_the_last_numeric_line() {
        // grep -c 在无匹配时退出码为 1，但输出 0：必须判为干净（Python 同）。
        let adb = FakeAdb::new().with_shell_results(vec![(1, "0\n")]);
        let server = server_with(&adb, Some(2000));
        assert!(server.verify_clean(2.0));
        assert_eq!(server.verify_detail(), "clean");

        let adb = FakeAdb::new().with_shell_results(vec![(0, "noise\n0\n")]);
        let server = server_with(&adb, Some(2000));
        assert!(server.verify_clean(2.0));
        assert_eq!(server.verify_detail(), "clean");

        let adb = FakeAdb::new().with_shell_results(vec![(0, "3\n")]);
        let server = server_with(&adb, Some(2000));
        assert!(!server.verify_clean(2.0));
        assert_eq!(server.verify_detail(), "仍有 3 处 scrcpy 残留");
    }

    #[test]
    fn verify_clean_reports_unparsable_output() {
        let adb = FakeAdb::new().with_shell_results(vec![(1, "")]);
        let server = server_with(&adb, Some(2000));
        assert!(!server.verify_clean(2.0));
        assert!(server.verify_detail().contains("不可解析"), "detail={}", server.verify_detail());

        let adb = FakeAdb::new().with_shell_results(vec![(0, "garbage\n")]);
        let server = server_with(&adb, Some(2000));
        assert!(!server.verify_clean(2.0));
        assert!(server.verify_detail().contains("不可解析"));
    }

    #[test]
    fn kill_runs_every_pattern_and_reaps_the_local_child() {
        let adb = FakeAdb::new().with_shell_ok(vec![true, true, true]);
        let server = server_with(&adb, Some(2000));
        server.spawn().expect("应当启动");
        assert!(server.proc_alive());
        server.kill(2.0).expect("kill 不应失败");
        let guard = adb.snapshot();
        let patterns: Vec<String> = guard
            .shell_ok_calls
            .iter()
            .map(|args| args[args.len() - 1].clone())
            .collect();
        assert_eq!(patterns, KILL_PATTERNS.to_vec());
        let stats = guard.stats.clone().expect("应当有子进程");
        let stats = stats.lock().expect("子进程统计");
        assert_eq!(stats.terminated, 1);
        assert!(!stats.alive);
        drop(stats);
        drop(guard);
        assert!(!server.proc_alive());
    }

    #[test]
    fn stop_retries_pkill_when_the_first_check_fails() {
        let adb = FakeAdb::new().with_shell_results(vec![(0, "2\n"), (0, "0\n")]);
        let server = server_with(&adb, Some(2000));
        assert!(server.stop());
        let guard = adb.snapshot();
        // 第一次 kill 3 次 pkill，重试再 3 次。
        assert_eq!(guard.shell_ok_calls.len(), 6);
        assert_eq!(guard.forward_removes, vec![2000]);
    }

    #[test]
    fn running_falls_back_to_pgrep() {
        let adb = FakeAdb::new().with_shell_ok(vec![false, true]);
        let server = server_with(&adb, Some(2000));
        assert!(server.running());
        let guard = adb.snapshot();
        assert_eq!(guard.shell_ok_calls.len(), 2);
    }

    #[test]
    fn start_sequence_pushes_then_kills_then_spawns_then_forwards() {
        let dir = TempDir::new("start");
        let (jar, sums) = write_jar(&dir, b"fresh", None);
        let adb = FakeAdb::new().with_md5(None).with_forwards(vec![true]);
        let server = ScrcpyServer::with_parts(
            Arc::new(adb.clone()),
            Some(2000),
            Some(jar),
            None,
            None,
            false,
        )
        .with_sums(Some(sums));
        assert_eq!(server.start().expect("应当启动"), 2000);
        let guard = adb.snapshot();
        assert_eq!(guard.pushes.len(), 1);
        assert_eq!(guard.spawned.len(), 1);
        // 启动前必须先清理（kill 的 3 个 pkill），forward_remove 先摘后建。
        assert_eq!(guard.shell_ok_calls.len(), 3);
        assert_eq!(guard.forward_removes, vec![2000]);
        assert_eq!(server.port(), Some(2000));
    }
}

