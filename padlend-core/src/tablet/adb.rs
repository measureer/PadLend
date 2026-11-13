//! adb 子进程封装（对应 tablet/adb.py，DESIGN 6.1.1 / 8.2 / 附录 C.1、D.1）。
//!
//! 这一层只做三件事：定位 adb、拼参数、把子进程的失败翻译成 E1xx。所有
//! 设备语义（选哪台、是否在线）都落在 [DeviceEntry] 与 [select_device_from]
//! 里，保持纯函数以便单测。
//!
//! **进程启动抽象成 [AdbRunner]**：生产路径用 [SystemRunner] 真的起进程，
//! 测试注入 [ScriptRunner] 之类的替身，绝不会在测试里跑 adb。
//!
//! 路径解析顺序与 Python 完全一致（explicit → PADLEND_ADB → vendor/adb →
//! PATH → 各种 Android SDK 默认位置），失败文案也逐字保留。

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::constants as C;

/// 设备在线。
pub const STATE_DEVICE: &str = "device";
/// 未授权 USB 调试。
pub const STATE_UNAUTHORIZED: &str = "unauthorized";
/// 离线。
pub const STATE_OFFLINE: &str = "offline";
/// Windows 上 adb 对未授权设备的另一种状态串（两个 token）。
pub const STATE_NO_PERMISSIONS: &str = "no permissions";

/// adb 默认的 server 端口。
const ADB_SERVER_DEFAULT_PORT: u16 = 5037;
/// 参数里需要打码的子命令（配对码不能进日志）。
const SENSITIVE_SUBCOMMANDS: [&str; 1] = ["pair"];
/// adb devices 可能出现的状态串。
const KNOWN_STATES: [&str; 12] = [
    "device",
    "offline",
    "unauthorized",
    "bootloader",
    "recovery",
    "sideload",
    "rescue",
    "host",
    "connecting",
    "authorizing",
    "unknown",
    "no",
];
/// 等待设备时检查取消标志的粒度（秒）。
const CANCEL_POLL_S: f64 = 0.05;

/// server 归属：0 = 未知，1 = 由 PadLend 拉起，2 = 早就存在。
static SERVER_OWNED: AtomicU8 = AtomicU8::new(0);

/// adb 层的错误码（与 core/errors.py 的 E1xx/E9xx 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdbErrorCode {
    /// E100：找不到 adb 或无法启动。
    AdbNotFound,
    /// E101：没有在线设备 / 地址无效。
    DeviceOffline,
    /// E103：服务端（jar 推送、popen）失败。
    ServerStartFailed,
    /// E104：端口转发失败。
    ForwardFailed,
    /// E900：未预期的内部错误（替身未实现也算这里）。
    Unexpected,
}

impl AdbErrorCode {
    /// 冻结的错误码字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            AdbErrorCode::AdbNotFound => "E100",
            AdbErrorCode::DeviceOffline => "E101",
            AdbErrorCode::ServerStartFailed => "E103",
            AdbErrorCode::ForwardFailed => "E104",
            AdbErrorCode::Unexpected => "E900",
        }
    }
}

impl fmt::Display for AdbErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// adb 层的错误：错误码 + 可直接展示的中文说明（对应 PadLendError）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct AdbError {
    /// E1xx 错误码。
    pub code: AdbErrorCode,
    /// 中文说明（部分文案会直接进 GUI）。
    pub detail: String,
}

impl AdbError {
    /// 构造错误。
    pub fn new(code: AdbErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// 设备离线（E101）。
    pub fn offline(detail: impl Into<String>) -> Self {
        Self::new(AdbErrorCode::DeviceOffline, detail)
    }
}

/// 无线调试地址归一化：接受 adb 命令粘贴、tcp:// 前缀、引号与裸主机名。
///
/// 返回空串表示输入不可用（调用方据此给出 E101 的提示）。
pub fn normalize_address(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    // 允许用户直接粘贴 adb 的整行输出，逐层剥掉前缀。
    for prefix in ["adb://", "tcp://", "adb connect ", "adb pair "] {
        if let Some(stripped) = text.strip_prefix(prefix) {
            text = stripped.trim().to_string();
        }
    }
    text = text.trim_matches('"').trim_matches('\'').trim().to_string();
    let first = text.split_whitespace().next().unwrap_or_default().to_string();
    let first = first.trim_end_matches('/').to_string();
    if first.is_empty() || first.contains(char::is_whitespace) {
        return String::new();
    }

    if first.starts_with('[') {
        // [IPv6]:port 原样保留；只校验端口部分。
        let Some(close) = first.find(']') else {
            return String::new();
        };
        let host = &first[1..close];
        if host.is_empty() {
            return String::new();
        }
        let rest = &first[close + 1..];
        if rest.is_empty() {
            return format!("[{host}]:{}", C::WIRELESS_DEFAULT_PORT);
        }
        let Some(port) = rest.strip_prefix(':') else {
            return String::new();
        };
        if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) {
            return format!("[{host}]:{port}");
        }
        return String::new();
    }

    if first.matches(':').count() >= 2 {
        // 裸 IPv6：adb 需要用方括号包起来才能带端口。
        return format!("[{first}]:{}", C::WIRELESS_DEFAULT_PORT);
    }

    match first.split_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return String::new();
            }
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return String::new();
            }
            format!("{host}:{port}")
        }
        None => format!("{first}:{}", C::WIRELESS_DEFAULT_PORT),
    }
}

/// 一条 adb devices -l 的结果（对应 Python 的 DeviceEntry）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceEntry {
    /// adb serial（无线地址可能带空格，adb -s 接受原样）。
    pub serial: String,
    /// 状态串，例如 device / offline / unauthorized。
    pub state: String,
    /// product: 属性。
    pub product: String,
    /// model: 属性。
    pub model: String,
    /// device: 属性。
    pub device: String,
    /// transport_id 属性。
    pub transport_id: String,
}

impl DeviceEntry {
    /// 用 serial + state 构造（测试与替身最常用）。
    pub fn new(serial: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            state: state.into(),
            ..Self::default()
        }
    }

    /// 是否可用。
    pub fn online(&self) -> bool {
        self.state == STATE_DEVICE
    }

    /// 是否是无线 adb 地址（与 Python 的 DeviceEntry.is_wireless 同义）。
    pub fn is_wireless(&self) -> bool {
        self.serial.contains(':')
            || self.serial.contains("_adb-tls-connect")
            || self.serial.contains(".tcp")
    }

    /// 是否是 USB 设备。
    pub fn is_usb(&self) -> bool {
        !self.is_wireless()
    }
}

/// 解析 adb devices -l 的输出：畸形行丢弃、重复 serial 只保留第一条、
/// 剔除 mDNS 造成的"带空格重复项"（与 Python 行为逐条对齐）。
pub fn parse_devices(output: &str) -> Vec<DeviceEntry> {
    let mut entries: Vec<DeviceEntry> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for line in output.lines() {
        let text = line.trim();
        if text.is_empty() || text.starts_with('*') || text.to_lowercase().contains("list of devices")
        {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut state_index: Option<usize> = None;
        let mut state = String::new();
        for (index, token) in parts.iter().enumerate() {
            let lowered = token.to_lowercase();
            // 两个 token 的状态优先判断：no permissions 不能被当成 no。
            if lowered == "no"
                && parts
                    .get(index + 1)
                    .map(|next| next.eq_ignore_ascii_case("permissions"))
                    .unwrap_or(false)
            {
                state_index = Some(index);
                state = STATE_NO_PERMISSIONS.to_string();
                break;
            }
            if KNOWN_STATES.contains(&lowered.as_str()) {
                state_index = Some(index);
                state = lowered;
                break;
            }
        }
        let Some(index) = state_index else {
            continue;
        };
        if index == 0 {
            continue;
        }
        let serial = parts[..index].join(" ");
        if serial.is_empty() || seen.contains(&serial) {
            continue;
        }
        seen.push(serial.clone());

        let mut entry = DeviceEntry {
            serial,
            state,
            ..DeviceEntry::default()
        };
        for token in &parts[index + 1..] {
            if let Some((key, value)) = token.split_once(':') {
                match key {
                    "product" => entry.product = value.to_string(),
                    "model" => entry.model = value.to_string(),
                    "device" => entry.device = value.to_string(),
                    "transport_id" => entry.transport_id = value.to_string(),
                    _ => {}
                }
            }
        }
        entries.push(entry);
    }

    // mDNS 会让同一台设备出现两次：带空格的实例名 + 干净的地址。只要干净的
    // 在线行存在，就丢掉带空格的那条。
    let cleaned: Vec<DeviceEntry> = entries
        .iter()
        .filter(|entry| {
            if !entry.serial.contains(' ') {
                return true;
            }
            !entries.iter().any(|other| {
                other.serial != entry.serial
                    && !other.serial.contains(' ')
                    && other.online()
                    && other.model == entry.model
                    && other.device == entry.device
            })
        })
        .cloned()
        .collect();
    cleaned
}

/// 从设备列表里挑一台（纯函数，行为与 Python select_device 一致）。
pub fn select_device_from(
    entries: &[DeviceEntry],
    serial: Option<&str>,
    prefer_usb: bool,
) -> Result<DeviceEntry, AdbError> {
    let online: Vec<&DeviceEntry> = entries.iter().filter(|entry| entry.online()).collect();
    if let Some(serial) = serial {
        let serial = serial.trim();
        if !serial.is_empty() {
            if let Some(found) = entries.iter().find(|entry| entry.serial == serial) {
                if found.online() {
                    return Ok(found.clone());
                }
                return Err(AdbError::offline(format!(
                    "设备 {serial} 状态为 {}（需在平板确认 USB 调试授权）",
                    found.state
                )));
            }
            if online.len() == 1 {
                let only = online[0];
                eprintln!(
                    "[tablet.adb] 找不到设备 {serial}，改用唯一在线的 {}",
                    only.serial
                );
                return Ok(only.clone());
            }
            return Err(AdbError::offline(format!(
                "找不到设备 {serial}（adb devices 为空？）"
            )));
        }
    }

    if online.is_empty() {
        if entries
            .iter()
            .any(|entry| entry.state == STATE_UNAUTHORIZED)
        {
            return Err(AdbError::offline("平板未授权 USB 调试，请在平板上点『允许』"));
        }
        return Err(AdbError::offline("没有在线的平板（adb devices 为空）"));
    }
    if prefer_usb {
        if let Some(usb) = online.iter().find(|entry| entry.is_usb()) {
            return Ok((*usb).clone());
        }
    }
    Ok(online[0].clone())
}

/// 一次子进程执行的结果（对应 subprocess.CompletedProcess）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// 退出码。
    pub returncode: i32,
    /// 原始 stdout 字节。
    pub stdout: Vec<u8>,
    /// 原始 stderr 字节。
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// 成功、stdout 为给定文本。
    pub fn ok(stdout: impl AsRef<[u8]>) -> Self {
        Self {
            returncode: 0,
            stdout: stdout.as_ref().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// stdout 按 UTF-8 宽松解码。
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr 按 UTF-8 宽松解码。
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// 子进程启动/等待失败（对应 Python 的 FileNotFoundError / TimeoutExpired / OSError）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    /// 可执行文件不存在。
    #[error("找不到可执行文件 {0}")]
    NotFound(String),
    /// 超时。
    #[error("命令超时 {0:?}")]
    TimedOut(Duration),
    /// 其他启动失败。
    #[error("{0}")]
    Spawn(String),
}

/// 后台子进程的 stdio 组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnStdio {
    /// 全部 DEVNULL（scrcpy-server 只要活着）。
    Detached,
    /// stdout 走管道、逐行读（朝向监听）。
    Stream,
}

/// 长驻子进程句柄（scrcpy-server / 朝向监听）。
pub trait ChildHandle: Send + fmt::Debug {
    /// 还在运行则返回 None，已退出返回退出码（未知用 -1）。
    fn try_wait(&mut self) -> Option<i32>;
    /// 终止（Windows 上等同 kill）。
    fn terminate(&mut self);
    /// 强杀。
    fn kill(&mut self);
    /// 最多等 timeout，返回是否已退出。
    fn wait_timeout(&mut self, timeout: Duration) -> bool;
    /// 进程号（诊断用）。
    fn pid(&self) -> Option<u32>;
}

/// 逐行输出型子进程（朝向监听用）。
pub trait StreamChild: Send + fmt::Debug {
    /// 读下一行；EOF 返回 None。
    fn next_line(&mut self) -> Option<String>;
    /// 还在运行则返回 None，已退出返回退出码。
    fn try_wait(&mut self) -> Option<i32>;
    /// 终止。
    fn terminate(&mut self);
    /// 强杀。
    fn kill(&mut self);
}

/// 子进程启动抽象：生产用 [SystemRunner]，测试注入替身。
pub trait AdbRunner: Send + Sync + fmt::Debug {
    /// 同步执行并等结果；超时必须杀掉进程并返回 [RunnerError::TimedOut]。
    fn run(&self, argv: &[String], timeout: Duration) -> Result<CommandOutput, RunnerError>;
    /// 后台长驻进程。
    fn spawn_detached(&self, argv: &[String]) -> Result<Box<dyn ChildHandle>, RunnerError>;
    /// 后台长驻进程，stdout 逐行可读。
    fn spawn_stream(&self, argv: &[String]) -> Result<Box<dyn StreamChild>, RunnerError>;
}

/// 真实子进程实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

/// 给 Windows 子进程加上 CREATE_NO_WINDOW，避免弹出黑框。
fn prepare_command(program: &str, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command.env("MSYS_NO_PATHCONV", "1");
    if std::env::var_os("PYTHONIOENCODING").is_none() {
        command.env("PYTHONIOENCODING", "utf-8");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn read_all<R: Read>(mut stream: R) -> Vec<u8> {
    let mut buffer: Vec<u8> = Vec::new();
    let _ = stream.read_to_end(&mut buffer);
    buffer
}

/// 把 runner 错误映射成 Python 里对应的异常。
fn start_error(program: &str, error: &std::io::Error) -> RunnerError {
    if error.kind() == std::io::ErrorKind::NotFound {
        RunnerError::NotFound(program.to_string())
    } else {
        RunnerError::Spawn(error.to_string())
    }
}

impl AdbRunner for SystemRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<CommandOutput, RunnerError> {
        let Some(program) = argv.first() else {
            return Err(RunnerError::Spawn("空命令".to_string()));
        };
        let mut command = prepare_command(program, &argv[1..]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| start_error(program, &error))?;
        let stdout = child.stdout.take().map(|stream| {
            std::thread::spawn(move || read_all(stream))
        });
        let stderr = child.stderr.take().map(|stream| {
            std::thread::spawn(move || read_all(stream))
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunnerError::Spawn(error.to_string()));
                }
            }
        };
        let stdout_bytes = stdout
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stderr_bytes = stderr
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        match status {
            Some(status) => Ok(CommandOutput {
                returncode: status.code().unwrap_or(-1),
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            }),
            None => Err(RunnerError::TimedOut(timeout)),
        }
    }

    fn spawn_detached(&self, argv: &[String]) -> Result<Box<dyn ChildHandle>, RunnerError> {
        let Some(program) = argv.first() else {
            return Err(RunnerError::Spawn("空命令".to_string()));
        };
        let mut command = prepare_command(program, &argv[1..]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|error| start_error(program, &error))?;
        Ok(Box::new(SystemChild {
            child,
            exited: None,
        }))
    }

    fn spawn_stream(&self, argv: &[String]) -> Result<Box<dyn StreamChild>, RunnerError> {
        let Some(program) = argv.first() else {
            return Err(RunnerError::Spawn("空命令".to_string()));
        };
        let mut command = prepare_command(program, &argv[1..]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| start_error(program, &error))?;
        if let Some(stderr) = child.stderr.take() {
            // Python 用 stderr=STDOUT 合并；这里把 stderr 抽干，避免管道写满。
            std::thread::spawn(move || {
                let _ = read_all(stderr);
            });
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunnerError::Spawn("无法取到子进程 stdout".to_string()))?;
        Ok(Box::new(SystemStream {
            child,
            stdout: std::io::BufReader::new(stdout),
            exited: None,
        }))
    }
}

/// 后台长驻子进程的真实句柄。
#[derive(Debug)]
struct SystemChild {
    child: Child,
    exited: Option<i32>,
}

impl ChildHandle for SystemChild {
    fn try_wait(&mut self) -> Option<i32> {
        if let Some(code) = self.exited {
            return Some(code);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                self.exited = Some(code);
                Some(code)
            }
            _ => None,
        }
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }

    fn wait_timeout(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_wait().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
}

/// 逐行输出的真实句柄。
#[derive(Debug)]
struct SystemStream {
    child: Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    exited: Option<i32>,
}

impl StreamChild for SystemStream {
    fn next_line(&mut self) -> Option<String> {
        use std::io::BufRead;
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                Some(line)
            }
            Err(_) => None,
        }
    }

    fn try_wait(&mut self) -> Option<i32> {
        if let Some(code) = self.exited {
            return Some(code);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                self.exited = Some(code);
                Some(code)
            }
            _ => None,
        }
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// server 归属探测（只在第一次需要时做，避免多一次 adb 往返）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Real,
    NeverListening,
    AlwaysListening,
}

/// adb server 是否已经在监听（对应 Python 的 _server_listening）。
pub fn server_listening() -> bool {
    use std::net::{SocketAddr, TcpStream};
    let address = SocketAddr::from(([127, 0, 0, 1], ADB_SERVER_DEFAULT_PORT));
    TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok()
}

/// 记录 adb server 是不是 PadLend 拉起来的（发版审计 P1：只 kill 自己拉起的）。
fn note_server_ownership(args: &[String], probe: Probe) {
    if SERVER_OWNED.load(Ordering::SeqCst) != 0 {
        return;
    }
    let subcommand = args
        .first()
        .map(|value| value.to_lowercase())
        .unwrap_or_default();
    if subcommand == "version" || subcommand == "kill-server" || subcommand == "start-server" {
        return;
    }
    let listening = match probe {
        Probe::Real => server_listening(),
        Probe::NeverListening => false,
        Probe::AlwaysListening => true,
    };
    SERVER_OWNED.store(if listening { 2 } else { 1 }, Ordering::SeqCst);
}

/// 当前 server 归属（None = 还不知道）。
pub fn server_ownership() -> Option<bool> {
    match SERVER_OWNED.load(Ordering::SeqCst) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// 测试/诊断用：强制设置归属（None 复位为未知）。
pub fn force_server_ownership(owned: Option<bool>) {
    SERVER_OWNED.store(
        match owned {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::SeqCst,
    );
}

/// 进程 PATH 里找可执行文件（对应 shutil.which）。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 候选 adb 路径（顺序与 Python 的 resolve_adb 一致）。
fn candidate_paths(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    }
    if let Some(value) = std::env::var_os("PADLEND_ADB") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            candidates.push(value);
        }
    }
    candidates.push(C::vendor_dir().join("adb").join("platform-tools").join(C::ADB_EXE));
    candidates.push(C::vendor_dir().join("adb").join(C::ADB_EXE));
    if let Some(path) = which(C::ADB_EXE) {
        candidates.push(path);
    }
    if let Some(path) = which("adb") {
        candidates.push(path);
    }
    for key in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(key) {
            if !root.is_empty() {
                candidates.push(
                    PathBuf::from(root)
                        .join("platform-tools")
                        .join(C::ADB_EXE),
                );
            }
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(C::ADB_EXE),
        );
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        candidates.push(
            PathBuf::from(home)
                .join("AppData")
                .join("Local")
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(C::ADB_EXE),
        );
    }
    candidates.push(PathBuf::from("C:\\Android\\platform-tools\\adb.exe"));
    candidates.push(PathBuf::from("C:\\Program Files\\Android\\platform-tools\\adb.exe"));
    candidates.push(PathBuf::from(
        "C:\\Program Files (x86)\\Android\\platform-tools\\adb.exe",
    ));
    candidates
}

/// 按候选顺序找第一个存在的 adb；全都没有时给出 E100 与排查建议。
pub fn resolve_adb(explicit: Option<&Path>) -> Result<PathBuf, AdbError> {
    let candidates = candidate_paths(explicit);
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    let shown = candidates
        .iter()
        .take(8)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("、");
    Err(AdbError::new(
        AdbErrorCode::AdbNotFound,
        format!(
            "找不到 adb。已尝试: {shown}；可设置环境变量 PADLEND_ADB 或把 platform-tools 放到 vendor/adb/"
        ),
    ))
}

/// 把秒数格式化成像 Python 的 {value:g}（去尾零）。
fn format_seconds(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 日志里展示参数；配对码必须打码。
fn display_args(args: &[String]) -> String {
    let mut shown: Vec<String> = args.to_vec();
    if let Some(subcommand) = shown.first().map(|value| value.to_lowercase()) {
        if SENSITIVE_SUBCOMMANDS.contains(&subcommand.as_str()) && shown.len() >= 3 {
            shown[2] = "<code>".to_string();
        }
    }
    shown.join(" ")
}

/// 在文本里找 32 位十六进制（对应 md5 输出解析）。
fn find_hex32(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_hexdigit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        if index - start == 32 {
            let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let after_ok = index >= bytes.len() || !is_word_byte(bytes[index]);
            if before_ok && after_ok {
                return Some(
                    String::from_utf8_lossy(&bytes[start..index]).to_lowercase(),
                );
            }
        }
    }
    None
}

/// Python 正则 \w 的字符集（ASCII 部分）。
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 接口名合法性（对应 _IFACE_RE：1..=32 个 [A-Za-z0-9_.:-]）。
fn is_iface_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(*byte, b'_' | b'.' | b':' | b'-')
        })
}

/// adb 会话端口（server.rs 与 session.rs 只依赖这个 trait）。
///
/// 除 [AdbApi::with_serial] 外都有"未实现"的默认实现，替身只要覆盖自己用到的
/// 方法即可；这样测试替身不会因为接口膨胀而被迫写一堆空函数。
pub trait AdbApi: Send + Sync + fmt::Debug {
    /// 绑定到某台设备（多设备时必须绑定，不能落到默认设备）。
    fn with_serial(&self, serial: &str) -> Arc<dyn AdbApi>;

    /// 绑定后的 serial（未绑定时为空串；替身按需覆盖）。
    fn serial(&self) -> String {
        String::new()
    }

    /// adb 版本串。
    fn version(&self) -> Result<String, AdbError> {
        Err(AdbError::new(AdbErrorCode::Unexpected, "adb 替身未实现 version"))
    }

    /// adb devices -l。
    fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
        Err(AdbError::new(AdbErrorCode::Unexpected, "adb 替身未实现 devices"))
    }

    /// 选设备（默认基于 [AdbApi::devices]，替身只覆盖 devices 即可）。
    fn select_device(
        &self,
        serial: Option<&str>,
        prefer_usb: bool,
    ) -> Result<DeviceEntry, AdbError> {
        let entries = self.devices()?;
        select_device_from(&entries, serial, prefer_usb)
    }

    /// 等设备上线（40s 首连窗口；cancel 为真时立刻返回 E101）。
    fn wait_for_device(
        &self,
        timeout: f64,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<DeviceEntry, AdbError> {
        let started = Instant::now();
        let mut last = AdbError::offline("没有在线的平板（adb devices 为空）");
        loop {
            if cancel() {
                return Err(AdbError::offline("等待设备已取消"));
            }
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed >= timeout {
                return Err(last);
            }
            match self.select_device(None, true) {
                Ok(entry) => return Ok(entry),
                Err(error) => last = error,
            }
            let mut remaining = (timeout - elapsed).min(C::INITIAL_RETRY_INTERVAL_S);
            while remaining > 0.0 {
                if cancel() {
                    return Err(AdbError::offline("等待设备已取消"));
                }
                let step = remaining.min(CANCEL_POLL_S);
                std::thread::sleep(Duration::from_secs_f64(step));
                remaining -= step;
            }
        }
    }

    /// adb shell（返回 stdout，已 trim）。
    fn shell(&self, args: &[&str], timeout: f64) -> Result<String, AdbError> {
        let _ = (args, timeout);
        Err(AdbError::new(AdbErrorCode::Unexpected, "adb 替身未实现 shell"))
    }

    /// adb shell，返回 (rc, stdout)，绝不抛。
    fn shell_result(&self, args: &[&str], timeout: f64) -> (i32, String) {
        let _ = (args, timeout);
        (1, String::new())
    }

    /// adb shell，只看 rc==0。
    fn shell_ok(&self, args: &[&str], timeout: f64) -> bool {
        self.shell_result(args, timeout).0 == 0
    }

    /// 原始 stdout 字节。
    fn exec_out(&self, args: &[&str], timeout: f64) -> Result<Vec<u8>, AdbError> {
        let _ = (args, timeout);
        Err(AdbError::new(AdbErrorCode::Unexpected, "adb 替身未实现 exec_out"))
    }

    /// adb push。
    fn push(&self, local: &Path, remote: &str) -> Result<(), AdbError> {
        let _ = (local, remote);
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 push",
        ))
    }

    /// 远端文件 md5（读不到返回 None）。
    fn md5(&self, remote: &str) -> Option<String> {
        let _ = remote;
        None
    }

    /// adb forward。
    fn forward(&self, port: u16, remote: &str) -> Result<(), AdbError> {
        let _ = (port, remote);
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 forward",
        ))
    }

    /// adb forward --remove（幂等、吞错）。
    fn forward_remove(&self, port: u16) {
        let _ = port;
    }

    /// 后台启动远端命令（scrcpy-server）。
    fn popen_shell(&self, command: &str) -> Result<Box<dyn ChildHandle>, AdbError> {
        let _ = command;
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 popen_shell",
        ))
    }

    /// 后台启动远端命令并逐行读输出（朝向监听）。
    fn popen_shell_stream(&self, command: &str) -> Result<Box<dyn StreamChild>, AdbError> {
        let _ = command;
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 popen_shell_stream",
        ))
    }

    /// adb connect。
    fn connect(&self, address: &str, timeout: f64) -> Result<String, AdbError> {
        let _ = (address, timeout);
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 connect",
        ))
    }

    /// adb pair。
    fn pair(&self, address: &str, code: &str, timeout: f64) -> Result<String, AdbError> {
        let _ = (address, code, timeout);
        Err(AdbError::new(
            AdbErrorCode::Unexpected,
            "adb 替身未实现 pair",
        ))
    }

    /// adb disconnect（吞错）。
    fn disconnect(&self, address: Option<&str>, timeout: f64) -> String {
        let _ = (address, timeout);
        String::new()
    }

    /// adb kill-server（只杀自己拉起的）。
    fn kill_server(&self) -> bool {
        false
    }

    /// adb mdns services 的原始行。
    fn mdns_services(&self) -> Vec<String> {
        Vec::new()
    }

    /// 当前 Wi-Fi 网卡 MAC（读不到返回空串）。
    fn wifi_mac(&self, serial: Option<&str>) -> String {
        let _ = serial;
        String::new()
    }
}

/// adb 命令执行器（对应 Python 的 Adb）。
///
/// 构造零副作用：路径解析、版本探测都是惰性的。
#[derive(Debug)]
pub struct Adb {
    serial: String,
    runner: Arc<dyn AdbRunner>,
    explicit_path: Option<PathBuf>,
    probe: Probe,
    path: Mutex<Option<PathBuf>>,
    version: Mutex<Option<String>>,
}

impl Adb {
    /// 生产用：真子进程 + 默认路径解析。
    pub fn new() -> Self {
        Self::with_runner(Arc::new(SystemRunner))
    }

    /// 用指定 runner 构造（测试注入替身）。
    pub fn with_runner(runner: Arc<dyn AdbRunner>) -> Self {
        Self {
            serial: String::new(),
            runner,
            explicit_path: None,
            probe: Probe::Real,
            path: Mutex::new(None),
            version: Mutex::new(None),
        }
    }

    /// 指定 adb 可执行文件（对应 Adb(adb_path=...)）。
    pub fn with_explicit_path(mut self, path: PathBuf) -> Self {
        self.explicit_path = Some(path);
        self
    }

    /// 测试/诊断用：跳过 PATH 解析，直接把 adb 路径写进缓存。
    pub fn with_resolved_path(runner: Arc<dyn AdbRunner>, path: PathBuf) -> Self {
        Self {
            serial: String::new(),
            runner,
            explicit_path: None,
            probe: Probe::NeverListening,
            path: Mutex::new(Some(path)),
            version: Mutex::new(None),
        }
    }

    /// 关掉 server 归属探测（测试用；避免真的连 5037）。
    pub fn without_probe(mut self) -> Self {
        self.probe = Probe::NeverListening;
        self
    }

    /// 覆盖 server 归属探测结果（诊断/测试用）。
    pub fn with_server_probe(mut self, listening: bool) -> Self {
        self.probe = if listening {
            Probe::AlwaysListening
        } else {
            Probe::NeverListening
        };
        self
    }

    /// 当前缓存的 adb 路径（None = 还没解析或刚刚失效）。
    pub fn cached_path(&self) -> Option<PathBuf> {
        self.path.lock().ok().and_then(|guard| guard.clone())
    }

    /// 绑定到某台设备（对应 with_serial）。
    pub fn bound(&self, serial: &str) -> Self {
        Self {
            serial: serial.to_string(),
            runner: self.runner.clone(),
            explicit_path: self.explicit_path.clone(),
            probe: self.probe,
            path: Mutex::new(self.path.lock().map(|guard| guard.clone()).unwrap_or_default()),
            version: Mutex::new(
                self.version
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default(),
            ),
        }
    }

    /// 当前绑定的 serial（空串 = 默认设备）。
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// 惰性解析 adb 路径。
    pub fn path(&self) -> Result<PathBuf, AdbError> {
        if let Ok(guard) = self.path.lock() {
            if let Some(path) = guard.as_ref() {
                return Ok(path.clone());
            }
        }
        let path = resolve_adb(self.explicit_path.as_deref())?;
        if let Ok(mut guard) = self.path.lock() {
            *guard = Some(path.clone());
        }
        Ok(path)
    }

    /// 清掉路径缓存（可执行文件消失时调用）。
    fn clear_path(&self) {
        if let Ok(mut guard) = self.path.lock() {
            *guard = None;
        }
    }

    /// 拼 [path, -s serial, ...args]。
    fn build_argv(&self, args: &[&str]) -> Result<Vec<String>, AdbError> {
        let path = self.path()?;
        let mut argv: Vec<String> = vec![path.display().to_string()];
        if !self.serial.is_empty() {
            argv.push("-s".to_string());
            argv.push(self.serial.clone());
        }
        argv.extend(args.iter().map(|value| (*value).to_string()));
        Ok(argv)
    }

    /// 统一执行入口（对应 Python 的 _run）。
    pub fn run(
        &self,
        args: &[&str],
        timeout: f64,
        code: AdbErrorCode,
        check: bool,
    ) -> Result<CommandOutput, AdbError> {
        let argv = self.build_argv(args)?;
        let subcommand: Vec<String> = args.iter().map(|value| (*value).to_string()).collect();
        note_server_ownership(&subcommand, self.probe);
        let duration = Duration::from_secs_f64(if timeout > 0.0 { timeout } else { 0.001 });
        let output = match self.runner.run(&argv, duration) {
            Ok(output) => output,
            Err(RunnerError::TimedOut(_)) => {
                return Err(AdbError::new(
                    code,
                    format!(
                        "adb 超时 {}s: {}",
                        format_seconds(timeout),
                        display_args(&subcommand)
                    ),
                ));
            }
            Err(RunnerError::NotFound(program)) => {
                self.clear_path();
                return Err(AdbError::new(
                    AdbErrorCode::AdbNotFound,
                    format!("adb 不可执行: {program}"),
                ));
            }
            Err(RunnerError::Spawn(message)) => {
                return Err(AdbError::new(
                    AdbErrorCode::AdbNotFound,
                    format!("无法启动 adb: {message}"),
                ));
            }
        };
        if check && output.returncode != 0 {
            let mut detail = output.stderr_text();
            if detail.trim().is_empty() {
                detail = output.stdout_text();
            }
            if detail.trim().is_empty() {
                detail = format!("exit {}", output.returncode);
            }
            return Err(AdbError::new(
                code,
                format!("{} → {}", display_args(&subcommand), detail.trim()),
            ));
        }
        Ok(output)
    }

    /// adb version（缓存）。
    pub fn version(&self) -> Result<String, AdbError> {
        if let Ok(guard) = self.version.lock() {
            if let Some(value) = guard.as_ref() {
                return Ok(value.clone());
            }
        }
        let output = self.run(&["version"], C::ADB_SERVER_TIMEOUT, AdbErrorCode::AdbNotFound, true)?;
        let text = output.stdout_text();
        let value = text.lines().next().unwrap_or_default().trim().to_string();
        if let Ok(mut guard) = self.version.lock() {
            *guard = Some(value.clone());
        }
        Ok(value)
    }

    /// adb devices -l。
    pub fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
        let output = self.run(
            &["devices", "-l"],
            C::ADB_SERVER_TIMEOUT,
            AdbErrorCode::AdbNotFound,
            true,
        )?;
        Ok(parse_devices(&output.stdout_text()))
    }

    /// 选设备（见 [select_device_from]）。
    pub fn select_device(
        &self,
        serial: Option<&str>,
        prefer_usb: bool,
    ) -> Result<DeviceEntry, AdbError> {
        let entries = self.devices()?;
        select_device_from(&entries, serial, prefer_usb)
    }

    /// 等设备上线。
    pub fn wait_for_device(
        &self,
        timeout: f64,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<DeviceEntry, AdbError> {
        let started = Instant::now();
        let mut last = AdbError::offline("没有在线的平板（adb devices 为空）");
        loop {
            if cancel() {
                return Err(AdbError::offline("等待设备已取消"));
            }
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed >= timeout {
                return Err(last);
            }
            match self.select_device(None, true) {
                Ok(entry) => return Ok(entry),
                Err(error) => last = error,
            }
            let mut remaining = (timeout - elapsed).min(C::INITIAL_RETRY_INTERVAL_S);
            while remaining > 0.0 {
                if cancel() {
                    return Err(AdbError::offline("等待设备已取消"));
                }
                let step = remaining.min(CANCEL_POLL_S);
                std::thread::sleep(Duration::from_secs_f64(step));
                remaining -= step;
            }
        }
    }

    /// adb mdns services 的原始行（失败返回空列表）。
    pub fn mdns_services(&self) -> Vec<String> {
        match self.run(
            &["mdns", "services"],
            C::ADB_SERVER_TIMEOUT,
            AdbErrorCode::AdbNotFound,
            true,
        ) {
            Ok(output) => output
                .stdout_text()
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| {
                    !line.is_empty() && !line.to_lowercase().starts_with("list of discovered")
                })
                .collect(),
            Err(error) => {
                eprintln!("[tablet.adb] adb mdns services 失败: {error}");
                Vec::new()
            }
        }
    }

    /// adb connect（无线调试第一步）。
    pub fn connect(&self, address: &str, timeout: f64) -> Result<String, AdbError> {
        let target = normalize_address(address);
        if target.is_empty() {
            return Err(AdbError::offline("请填写平板上的 IP 地址与端口"));
        }
        // check=false：失败时 adb 的退出码不稳定，失败判定靠下面的文案特征。
        let output = self.run(
            &["connect", &target],
            timeout,
            AdbErrorCode::DeviceOffline,
            false,
        )?;
        let text = format!("{}{}", output.stdout_text(), output.stderr_text())
            .trim()
            .to_string();
        let lowered = text.to_lowercase();
        let failed = text.is_empty()
            || [
                "failed to connect",
                "cannot connect",
                "unable to connect",
                "no route to host",
            ]
            .iter()
            .any(|marker| lowered.contains(marker));
        if failed {
            return Err(AdbError::offline(format!(
                "连接 {target} 失败：{text}（确认平板与电脑在同一 Wi-Fi、且已开启无线调试）"
            )));
        }
        Ok(text)
    }

    /// adb pair（无线调试配对，地址与配对码都不能为空）。
    pub fn pair(&self, address: &str, code: &str, timeout: f64) -> Result<String, AdbError> {
        let target = normalize_address(address);
        let code = code.trim();
        if target.is_empty() || code.is_empty() {
            return Err(AdbError::offline("配对需要同时填写地址与 6 位配对码"));
        }
        // check=false：配对失败时 adb 仍可能返回 0，必须看输出。
        let output = self.run(
            &["pair", &target, code],
            timeout,
            AdbErrorCode::DeviceOffline,
            false,
        )?;
        let text = format!("{}{}", output.stdout_text(), output.stderr_text())
            .trim()
            .to_string();
        if !text.to_lowercase().contains("successfully paired") {
            return Err(AdbError::offline(format!(
                "配对 {target} 失败：{text}（配对端口与连接端口不同，请填「使用配对码配对设备」窗口里的地址）"
            )));
        }
        Ok(text)
    }

    /// adb disconnect（吞错，返回原始文本）。
    pub fn disconnect(&self, address: Option<&str>, timeout: f64) -> String {
        let mut args: Vec<&str> = vec!["disconnect"];
        let target = address.map(normalize_address).unwrap_or_default();
        if !target.is_empty() {
            args.push(&target);
        }
        match self.run(&args, timeout, AdbErrorCode::DeviceOffline, false) {
            Ok(output) => format!("{}{}", output.stdout_text(), output.stderr_text())
                .trim()
                .to_string(),
            Err(_) => String::new(),
        }
    }

    /// adb kill-server：只杀 PadLend 自己拉起的 server。
    pub fn kill_server(&self) -> bool {
        if server_ownership() != Some(true) {
            return false;
        }
        let _ = self.run(
            &["kill-server"],
            C::ADB_SERVER_TIMEOUT,
            AdbErrorCode::AdbNotFound,
            false,
        );
        force_server_ownership(Some(false));
        true
    }

    /// adb shell，返回 trim 过的 stdout。
    pub fn shell(&self, args: &[&str], timeout: f64) -> Result<String, AdbError> {
        let mut full: Vec<&str> = vec!["shell"];
        full.extend_from_slice(args);
        let output = self.run(&full, timeout, AdbErrorCode::DeviceOffline, true)?;
        Ok(output.stdout_text().trim().to_string())
    }

    /// adb shell，返回 (rc, stdout)，绝不抛。
    pub fn shell_result(&self, args: &[&str], timeout: f64) -> (i32, String) {
        let mut full: Vec<&str> = vec!["shell"];
        full.extend_from_slice(args);
        match self.run(&full, timeout, AdbErrorCode::DeviceOffline, false) {
            Ok(output) => (output.returncode, output.stdout_text()),
            Err(_) => (1, String::new()),
        }
    }

    /// adb shell，只看是否成功。
    pub fn shell_ok(&self, args: &[&str], timeout: f64) -> bool {
        self.shell_result(args, timeout).0 == 0
    }

    /// adb exec-out 的原始字节。
    pub fn exec_out(&self, args: &[&str], timeout: f64) -> Result<Vec<u8>, AdbError> {
        let mut full: Vec<&str> = vec!["exec-out"];
        full.extend_from_slice(args);
        let output = self.run(&full, timeout, AdbErrorCode::DeviceOffline, true)?;
        Ok(output.stdout)
    }

    /// 当前 Wi-Fi 网卡 MAC（wlan0 → eth0 → wlan1 → 扫描其余接口）。
    pub fn wifi_mac(&self, serial: Option<&str>) -> String {
        let dev = match serial {
            Some(value) if !value.trim().is_empty() => self.bound(value.trim()),
            _ => self.bound(&self.serial),
        };
        let mut tried: Vec<String> = Vec::new();
        for iface in ["wlan0", "eth0", "wlan1"] {
            tried.push(iface.to_string());
            if let Some(mac) = dev.read_iface_mac(iface) {
                return mac;
            }
        }
        let (rc, listing) = dev.shell_result(&["ls", "/sys/class/net"], C::ADB_SHELL_TIMEOUT);
        if rc != 0 {
            return String::new();
        }
        for line in listing.lines() {
            let name = line.trim();
            if name.is_empty() || !is_iface_name(name) || tried.iter().any(|item| item == name) {
                continue;
            }
            if let Some(mac) = dev.read_iface_mac(name) {
                return mac;
            }
        }
        String::new()
    }

    /// 读一个网卡的地址文件并校验 MAC。
    fn read_iface_mac(&self, iface: &str) -> Option<String> {
        let path = format!("/sys/class/net/{iface}/address");
        let (rc, text) = self.shell_result(&["cat", &path], C::ADB_SHELL_TIMEOUT);
        if rc != 0 {
            return None;
        }
        let value = text.trim().to_lowercase();
        if is_mac_text(&value) {
            Some(value)
        } else {
            None
        }
    }

    /// 后台启动远端命令（scrcpy-server）。
    pub fn popen_shell(&self, command: &str) -> Result<Box<dyn ChildHandle>, AdbError> {
        let argv = self.build_argv(&["shell", command])?;
        self.runner
            .spawn_detached(&argv)
            .map_err(|error| {
                AdbError::new(
                    AdbErrorCode::ServerStartFailed,
                    format!("无法启动服务端进程: {error}"),
                )
            })
    }

    /// 后台启动远端命令并逐行读输出（朝向监听）。
    pub fn popen_shell_stream(&self, command: &str) -> Result<Box<dyn StreamChild>, AdbError> {
        let argv = self.build_argv(&["shell", command])?;
        self.runner.spawn_stream(&argv).map_err(|error| {
            AdbError::new(
                AdbErrorCode::ServerStartFailed,
                format!("无法启动 adb 流式命令: {error}"),
            )
        })
    }

    /// adb forward tcp:port → remote。
    pub fn forward(&self, port: u16, remote: &str) -> Result<(), AdbError> {
        let local = format!("tcp:{port}");
        self.run(
            &["forward", &local, remote],
            C::ADB_FORWARD_TIMEOUT,
            AdbErrorCode::ForwardFailed,
            true,
        )?;
        Ok(())
    }

    /// adb forward --remove（幂等、吞错）。
    pub fn forward_remove(&self, port: u16) {
        let local = format!("tcp:{port}");
        let _ = self.run(
            &["forward", "--remove", &local],
            C::ADB_FORWARD_TIMEOUT,
            AdbErrorCode::ForwardFailed,
            false,
        );
    }

    /// adb push。
    pub fn push(&self, local: &Path, remote: &str) -> Result<(), AdbError> {
        let local_text = local.display().to_string();
        self.run(
            &["push", &local_text, remote],
            C::ADB_PUSH_TIMEOUT,
            AdbErrorCode::ServerStartFailed,
            true,
        )?;
        Ok(())
    }

    /// 远端文件 md5（md5sum 优先，再试 md5）。
    pub fn md5(&self, remote: &str) -> Option<String> {
        for command in ["md5sum", "md5"] {
            let (rc, text) = self.shell_result(&[command, remote], C::ADB_SHELL_TIMEOUT);
            if rc != 0 {
                continue;
            }
            if let Some(value) = find_hex32(&text) {
                return Some(value);
            }
        }
        None
    }
}

impl Default for Adb {
    fn default() -> Self {
        Self::new()
    }
}

/// 校验 aa:bb:cc:dd:ee:ff（小写）。
fn is_mac_text(text: &str) -> bool {
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

impl AdbApi for Adb {
    fn serial(&self) -> String {
        self.serial.clone()
    }

    fn with_serial(&self, serial: &str) -> Arc<dyn AdbApi> {
        Arc::new(self.bound(serial))
    }

    fn version(&self) -> Result<String, AdbError> {
        Adb::version(self)
    }

    fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
        Adb::devices(self)
    }

    fn select_device(
        &self,
        serial: Option<&str>,
        prefer_usb: bool,
    ) -> Result<DeviceEntry, AdbError> {
        Adb::select_device(self, serial, prefer_usb)
    }

    fn wait_for_device(
        &self,
        timeout: f64,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<DeviceEntry, AdbError> {
        Adb::wait_for_device(self, timeout, cancel)
    }

    fn shell(&self, args: &[&str], timeout: f64) -> Result<String, AdbError> {
        Adb::shell(self, args, timeout)
    }

    fn shell_result(&self, args: &[&str], timeout: f64) -> (i32, String) {
        Adb::shell_result(self, args, timeout)
    }

    fn shell_ok(&self, args: &[&str], timeout: f64) -> bool {
        Adb::shell_ok(self, args, timeout)
    }

    fn exec_out(&self, args: &[&str], timeout: f64) -> Result<Vec<u8>, AdbError> {
        Adb::exec_out(self, args, timeout)
    }

    fn push(&self, local: &Path, remote: &str) -> Result<(), AdbError> {
        Adb::push(self, local, remote)
    }

    fn md5(&self, remote: &str) -> Option<String> {
        Adb::md5(self, remote)
    }

    fn forward(&self, port: u16, remote: &str) -> Result<(), AdbError> {
        Adb::forward(self, port, remote)
    }

    fn forward_remove(&self, port: u16) {
        Adb::forward_remove(self, port);
    }

    fn popen_shell(&self, command: &str) -> Result<Box<dyn ChildHandle>, AdbError> {
        Adb::popen_shell(self, command)
    }

    fn popen_shell_stream(&self, command: &str) -> Result<Box<dyn StreamChild>, AdbError> {
        Adb::popen_shell_stream(self, command)
    }

    fn connect(&self, address: &str, timeout: f64) -> Result<String, AdbError> {
        Adb::connect(self, address, timeout)
    }

    fn pair(&self, address: &str, code: &str, timeout: f64) -> Result<String, AdbError> {
        Adb::pair(self, address, code, timeout)
    }

    fn disconnect(&self, address: Option<&str>, timeout: f64) -> String {
        Adb::disconnect(self, address, timeout)
    }

    fn kill_server(&self) -> bool {
        Adb::kill_server(self)
    }

    fn mdns_services(&self) -> Vec<String> {
        Adb::mdns_services(self)
    }

    fn wifi_mac(&self, serial: Option<&str>) -> String {
        Adb::wifi_mac(self, serial)
    }
}

/// 把 adb 错误码映射成 Router 的错误码（session.rs 用）。
pub fn to_router_code(code: AdbErrorCode) -> crate::logic::router::ErrorCode {
    use crate::logic::router::ErrorCode;
    match code {
        AdbErrorCode::AdbNotFound => ErrorCode::AdbNotFound,
        AdbErrorCode::DeviceOffline => ErrorCode::DeviceOffline,
        AdbErrorCode::ServerStartFailed => ErrorCode::ServerStartFailed,
        AdbErrorCode::ForwardFailed => ErrorCode::ForwardFailed,
        AdbErrorCode::Unexpected => ErrorCode::Unexpected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 脚本化 runner：按顺序吐出预设结果，并记录每次 argv。
    #[derive(Debug)]
    struct ScriptRunner {
        outputs: Mutex<VecDeque<Result<CommandOutput, RunnerError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptRunner {
        fn new(outputs: Vec<Result<CommandOutput, RunnerError>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().map(|guard| guard.clone()).unwrap_or_default()
        }
    }

    impl AdbRunner for ScriptRunner {
        fn run(&self, argv: &[String], _timeout: Duration) -> Result<CommandOutput, RunnerError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(argv.to_vec());
            }
            let next = self.outputs.lock().ok().and_then(|mut queue| queue.pop_front());
            next.unwrap_or_else(|| Ok(CommandOutput::ok("")))
        }

        fn spawn_detached(&self, _argv: &[String]) -> Result<Box<dyn ChildHandle>, RunnerError> {
            Err(RunnerError::Spawn("替身不启动后台进程".to_string()))
        }

        fn spawn_stream(&self, _argv: &[String]) -> Result<Box<dyn StreamChild>, RunnerError> {
            Err(RunnerError::Spawn("替身不启动后台进程".to_string()))
        }
    }

    fn fake_adb(runner: Arc<ScriptRunner>) -> Adb {
        Adb::with_resolved_path(runner, PathBuf::from("adb.exe"))
    }

    fn ok(stdout: &str) -> Result<CommandOutput, RunnerError> {
        Ok(CommandOutput::ok(stdout))
    }

    fn rc(returncode: i32, stdout: &str) -> Result<CommandOutput, RunnerError> {
        Ok(CommandOutput {
            returncode,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    const DUP_OUTPUT: &str = "List of devices attached\n\
adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp device product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:3\n\
adb-AM4NBB5C23001691-xn14x2._adb-tls-connect._tcp device product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:2\n";

    #[test]
    fn devices_drops_malformed_state_and_keeps_one_entry() {
        let entries = parse_devices(DUP_OUTPUT);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(
            entry.serial,
            "adb-AM4NBB5C23001691-xn14x2._adb-tls-connect._tcp"
        );
        assert_eq!(entry.state, STATE_DEVICE);
        assert_eq!(entry.model, "CGA_W00");
        assert!(entry.online());
    }

    #[test]
    fn devices_deduplicates_same_serial() {
        let output = "List of devices attached\n\
emulator-5554 device product:sdk model:Pixel device:generic transport_id:1\n\
emulator-5554 device product:sdk model:Pixel device:generic transport_id:1\n";
        let entries = parse_devices(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].serial, "emulator-5554");
    }

    #[test]
    fn devices_keeps_offline_and_unauthorized() {
        let output =
            "List of devices attached\nA offline transport_id:4\nB unauthorized usb:1-1 transport_id:5\n";
        let entries = parse_devices(output);
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|entry| (entry.serial.as_str(), entry.state.as_str()))
            .collect();
        assert_eq!(pairs, vec![("A", STATE_OFFLINE), ("B", STATE_UNAUTHORIZED)]);
    }

    #[test]
    fn devices_keeps_mdns_serial_with_space_when_it_is_the_only_entry() {
        let output = "List of devices attached\n\
adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp device product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:3\n";
        let entries = parse_devices(output);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        // adb -s 接受带空格的 serial；不能因为 state 不是第二列就把设备丢掉。
        assert_eq!(
            entry.serial,
            "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp"
        );
        assert_eq!(entry.state, STATE_DEVICE);
        assert!(entry.online());
        assert!(entry.is_wireless() && !entry.is_usb());
        assert_eq!(entry.transport_id, "3");
    }

    #[test]
    fn devices_reports_no_permissions_state() {
        let output = "List of devices attached\nC no permissions (user in plugdev group); see [http://developer.android.com]\n";
        let entries = parse_devices(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].serial, "C");
        assert_eq!(entries[0].state, STATE_NO_PERMISSIONS);
    }

    #[test]
    fn select_device_falls_back_to_only_online_device() {
        let entries = parse_devices(
            "List of devices attached\n192.168.1.8:37495 device product:CGA-W00 model:CGA_W00 device:HNCGA-Q transport_id:15\n",
        );
        let entry =
            select_device_from(&entries, Some("adb-old-name._adb-tls-connect._tcp"), true)
                .expect("唯一在线设备应被采用");
        assert_eq!(entry.serial, "192.168.1.8:37495");
        assert!(entry.online());
    }

    #[test]
    fn normalize_address_cases() {
        let cases = [
            ("192.168.1.8:37495", "192.168.1.8:37495"),
            ("  192.168.1.8  ", "192.168.1.8:5555"),
            ("adb connect 192.168.1.8:5555", "192.168.1.8:5555"),
            ("adb pair 192.168.1.8:37001", "192.168.1.8:37001"),
            ("tcp://192.168.1.8:5555", "192.168.1.8:5555"),
            ("\"192.168.1.8:5555\"", "192.168.1.8:5555"),
            ("adb connect 1.2.3.4:5555 already connected", "1.2.3.4:5555"),
            ("", ""),
            ("   ", ""),
            ("192.168.1.8:abc", ""),
            (":5555", ""),
            ("tablet.local", "tablet.local:5555"),
            ("[fd7a::1]:5555", "[fd7a::1]:5555"),
            ("[fd7a::1]", "[fd7a::1]:5555"),
            ("fd7a::1", "[fd7a::1]:5555"),
            ("[fd7a::1]:bad", ""),
        ];
        for (raw, expected) in cases {
            assert_eq!(normalize_address(raw), expected, "raw={raw:?}");
        }
    }

    #[test]
    fn connect_returns_adb_hint_on_success() {
        let runner = ScriptRunner::new(vec![ok("connected to 192.168.1.8:37495\n")]);
        let adb = fake_adb(runner.clone());
        let text = adb.connect("192.168.1.8:37495", 20.0).expect("应当成功");
        assert!(text.contains("connected to"));
        let calls = runner.calls();
        let call = &calls[0];
        assert_eq!(&call[call.len() - 2..], &["connect", "192.168.1.8:37495"]);
    }

    #[test]
    fn connect_raises_e101_on_failure_output() {
        let runner = ScriptRunner::new(vec![rc(
            1,
            "failed to connect to '192.168.1.8:37495': No route to host\n",
        )]);
        let error = fake_adb(runner)
            .connect("192.168.1.8:37495", 20.0)
            .expect_err("必须失败");
        assert_eq!(error.code, AdbErrorCode::DeviceOffline);
        assert!(error.detail.contains("同一 Wi-Fi"));
    }

    #[test]
    fn connect_raises_when_adb_says_nothing() {
        let runner = ScriptRunner::new(vec![rc(1, "")]);
        assert!(fake_adb(runner)
            .connect("192.168.1.8:37495", 20.0)
            .is_err());
    }

    #[test]
    fn connect_rejects_blank_address() {
        let runner = ScriptRunner::new(vec![]);
        let error = fake_adb(runner)
            .connect("   ", 20.0)
            .expect_err("空地址必须拒绝");
        assert_eq!(error.code, AdbErrorCode::DeviceOffline);
    }

    #[test]
    fn pair_requires_success_line() {
        let runner = ScriptRunner::new(vec![ok(
            "Successfully paired to 192.168.1.8:37001 [guid=adb-x]\n",
        )]);
        let text = fake_adb(runner)
            .pair("192.168.1.8:37001", "123456", 30.0)
            .expect("配对成功");
        assert!(text.to_lowercase().contains("paired"));

        let runner = ScriptRunner::new(vec![rc(1, "Failed: Wrong password\n")]);
        let error = fake_adb(runner)
            .pair("192.168.1.8:37001", "000000", 30.0)
            .expect_err("密码错误必须失败");
        assert!(error.detail.contains("配对端口与连接端口不同"));
    }

    #[test]
    fn pair_rejects_missing_code() {
        let runner = ScriptRunner::new(vec![]);
        assert!(fake_adb(runner)
            .pair("192.168.1.8:37001", "", 30.0)
            .is_err());
    }

    #[test]
    fn pair_code_is_masked_in_logs() {
        let args = vec![
            "pair".to_string(),
            "1.2.3.4:5555".to_string(),
            "123456".to_string(),
        ];
        assert_eq!(display_args(&args), "pair 1.2.3.4:5555 <code>");
        let args = vec!["devices".to_string(), "-l".to_string()];
        assert_eq!(display_args(&args), "devices -l");
    }

    #[test]
    fn kill_server_never_raises_and_only_when_owned() {
        force_server_ownership(Some(true));
        let runner = ScriptRunner::new(vec![rc(1, "")]);
        let adb = fake_adb(runner.clone());
        assert!(adb.kill_server());
        assert_eq!(
            runner.calls().last().and_then(|call| call.last().cloned()),
            Some("kill-server".to_string())
        );
        // 第二次不再执行（归属已置为 false）。
        assert!(!adb.kill_server());
        force_server_ownership(None);
    }

    #[test]
    fn kill_server_skips_when_server_was_already_running() {
        force_server_ownership(Some(false));
        let runner = ScriptRunner::new(vec![]);
        let adb = fake_adb(runner.clone());
        assert!(!adb.kill_server());
        assert!(runner.calls().is_empty());
        force_server_ownership(None);
    }

    #[test]
    fn wifi_mac_reads_wlan0() {
        let runner = ScriptRunner::new(vec![ok("88:A4:79:1A:2B:3C\n")]);
        let adb = fake_adb(runner.clone());
        assert_eq!(adb.wifi_mac(Some("192.168.1.8:37495")), "88:a4:79:1a:2b:3c");
        let first = &runner.calls()[0];
        assert_eq!(
            first,
            &vec![
                "adb.exe".to_string(),
                "-s".to_string(),
                "192.168.1.8:37495".to_string(),
                "shell".to_string(),
                "cat".to_string(),
                "/sys/class/net/wlan0/address".to_string(),
            ]
        );
    }

    #[test]
    fn wifi_mac_falls_back_to_scanning_interfaces() {
        let runner = ScriptRunner::new(vec![
            rc(1, ""),
            rc(1, ""),
            rc(1, ""),
            ok("lo\nwlan0\nwlan9\n"),
            rc(1, ""),
            ok("aa:bb:cc:dd:ee:ff\n"),
        ]);
        assert_eq!(fake_adb(runner).wifi_mac(None), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn wifi_mac_returns_empty_when_nothing_valid() {
        let runner = ScriptRunner::new(vec![ok("not-a-mac\n"), rc(1, ""), rc(1, ""), rc(1, "")]);
        assert_eq!(fake_adb(runner).wifi_mac(None), "");
    }

    #[test]
    fn run_maps_timeout_to_the_requested_error_code() {
        let runner = ScriptRunner::new(vec![Err(RunnerError::TimedOut(
            Duration::from_millis(1500),
        ))]);
        let adb = fake_adb(runner);
        let error = adb
            .run(&["devices", "-l"], 1.5, AdbErrorCode::DeviceOffline, true)
            .expect_err("超时必须失败");
        assert_eq!(error.code, AdbErrorCode::DeviceOffline);
        assert!(error.detail.contains("adb 超时 1.5s"));
    }

    #[test]
    fn run_resets_cached_path_when_binary_disappears() {
        let runner = ScriptRunner::new(vec![Err(RunnerError::NotFound("adb.exe".to_string()))]);
        let adb = fake_adb(runner);
        assert!(adb.cached_path().is_some());
        let error = adb
            .run(&["devices", "-l"], 1.0, AdbErrorCode::AdbNotFound, true)
            .expect_err("可执行文件消失必须失败");
        assert_eq!(error.code, AdbErrorCode::AdbNotFound);
        assert!(adb.cached_path().is_none(), "路径缓存必须被清掉");
    }

    #[test]
    fn run_maps_spawn_error_to_adb_not_found() {
        let runner = ScriptRunner::new(vec![Err(RunnerError::Spawn(
            "cannot spawn".to_string(),
        ))]);
        let error = fake_adb(runner)
            .run(&["devices", "-l"], 1.0, AdbErrorCode::AdbNotFound, true)
            .expect_err("启动失败必须失败");
        assert_eq!(error.code, AdbErrorCode::AdbNotFound);
        assert!(error.detail.contains("无法启动 adb"));
    }

    #[test]
    fn disconnect_swallows_adb_errors() {
        let runner = ScriptRunner::new(vec![Err(RunnerError::TimedOut(Duration::from_secs(1)))]);
        assert_eq!(
            fake_adb(runner).disconnect(Some("192.168.1.8:37495"), 20.0),
            ""
        );
    }

    #[test]
    fn select_device_reports_unauthorized_hint() {
        let output = "List of devices attached\nABCD1234 unauthorized usb:1-1 transport_id:5\n";
        let entries = parse_devices(output);
        let error = select_device_from(&entries, None, true).expect_err("必须失败");
        assert_eq!(error.code, AdbErrorCode::DeviceOffline);
        assert!(error.detail.contains("允许"));
    }

    #[test]
    fn select_device_reports_state_of_known_serial() {
        let entries = parse_devices(
            "List of devices attached\nABCD1234 unauthorized usb:1-1 transport_id:5\n",
        );
        let error = select_device_from(&entries, Some("ABCD1234"), true).expect_err("必须失败");
        assert!(error.detail.contains("状态为 unauthorized"));
    }

    #[test]
    fn md5_parses_hexdigest_from_md5sum_output() {
        let runner = ScriptRunner::new(vec![ok(
            "d41d8cd98f00b204e9800998ecf8427e  /data/local/tmp/scrcpy-server-manual.jar\n",
        )]);
        assert_eq!(
            fake_adb(runner).md5("/data/local/tmp/scrcpy-server-manual.jar"),
            Some("d41d8cd98f00b204e9800998ecf8427e".to_string())
        );
    }

    #[test]
    fn md5_falls_back_to_the_second_command() {
        let runner = ScriptRunner::new(vec![rc(1, ""), ok("D41D8CD98F00B204E9800998ECF8427E\n")]);
        assert_eq!(
            fake_adb(runner).md5("/data/x.jar"),
            Some("d41d8cd98f00b204e9800998ecf8427e".to_string())
        );
    }

    #[test]
    fn forward_failure_is_e104_and_push_is_e103() {
        let runner = ScriptRunner::new(vec![
            rc(1, "error: cannot bind listener: Address already in use\n"),
            rc(1, "adb: error: failed to copy\n"),
        ]);
        let adb = fake_adb(runner);
        let forward = adb
            .forward(1234, "localabstract:scrcpy")
            .expect_err("转发失败必须报错");
        assert_eq!(forward.code, AdbErrorCode::ForwardFailed);
        let push = adb
            .push(Path::new("vendor/scrcpy.jar"), "/data/local/tmp/x.jar")
            .expect_err("推送失败必须报错");
        assert_eq!(push.code, AdbErrorCode::ServerStartFailed);
    }

    #[test]
    fn shell_ok_never_raises() {
        let runner = ScriptRunner::new(vec![
            Err(RunnerError::Spawn("boom".to_string())),
            Err(RunnerError::Spawn("boom".to_string())),
        ]);
        let adb = fake_adb(runner);
        assert!(!adb.shell_ok(&["settings", "put", "system", "pointer_speed", "0"], 5.0));
        assert_eq!(
            adb.shell_result(&["getprop", "ro.product.model"], 5.0),
            (1, String::new())
        );
    }

    #[test]
    fn error_display_keeps_code_prefix() {
        let error = AdbError::offline("设备离线");
        assert_eq!(error.to_string(), "E101: 设备离线");
        assert_eq!(AdbErrorCode::ServerStartFailed.as_str(), "E103");
    }

    #[test]
    fn format_seconds_matches_python_g() {
        assert_eq!(format_seconds(5.0), "5");
        assert_eq!(format_seconds(1.5), "1.5");
        assert_eq!(format_seconds(120.0), "120");
    }

    #[test]
    fn find_hex32_requires_word_boundaries() {
        assert_eq!(
            find_hex32("abc d41d8cd98f00b204e9800998ecf8427e  x"),
            Some("d41d8cd98f00b204e9800998ecf8427e".to_string())
        );
        assert_eq!(find_hex32("zd41d8cd98f00b204e9800998ecf8427e"), None);
        assert_eq!(find_hex32("d41d8cd98f00b204e9800998ecf8427eff"), None);
    }

    #[test]
    fn wait_for_device_cancel_is_prompt() {
        let runner = ScriptRunner::new(vec![]);
        let adb = fake_adb(runner);
        let cancel = || true;
        let started = Instant::now();
        let error = adb.wait_for_device(40.0, &cancel).expect_err("必须取消");
        assert_eq!(error.code, AdbErrorCode::DeviceOffline);
        assert!(error.detail.contains("取消"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_for_device_returns_the_online_entry() {
        let runner = ScriptRunner::new(vec![ok(
            "List of devices attached\nA device product:p model:m device:d transport_id:1\n",
        )]);
        let adb = fake_adb(runner);
        let cancel = || false;
        let entry = adb.wait_for_device(1.0, &cancel).expect("应当拿到设备");
        assert_eq!(entry.serial, "A");
    }
}
