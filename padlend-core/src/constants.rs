//! 全局常量、超时、HID 描述符（对应 `constants.py`；设计文档 7.1 / 9.3 / 9.4 / 附录 A）。
//!
//! 本模块是依赖图的最底层：只依赖标准库与 `crate::VERSION`（唯一版本源）。
//! 魔法数字统一收在这里，业务代码通过名字引用。
//!
//! 路径语义与 Python 版逐条对位：
//! - `PROJECT_ROOT` 在源码里是 `constants.py` 所在目录；Rust sidecar 是单文件
//!   可执行程序，没有 `__file__`，对位物是**可执行文件所在目录**（`app_dir()`）。
//! - PyInstaller 的 `sys.frozen` / `_MEIPASS` 分支在 Rust 里不存在：`app_dir`、
//!   `bundle_dir` 都等于可执行文件目录。
//! - `config_dir()` 的回退顺序与错误文案保持不变：`PADLEND_HOME` →
//!   `APPDATA` → `LOCALAPPDATA` → 项目目录（`.padlend`）。

use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 应用标识
// ---------------------------------------------------------------------------
/// 应用名；用户目录、互斥体、窗口类名都用它。
pub const APP_NAME: &str = "PadLend";
/// 版本号：唯一版本源是 `Cargo.toml`（对应 Python 的 `padlend/__init__.py`）。
pub const VERSION: &str = crate::VERSION;
/// 许可证标识（关于对话框/诊断包用）。
pub const LICENSE: &str = "Apache-2.0";

/// 单实例互斥体（7.5）。`Global\` 前缀保证跨用户会话唯一。
pub const SINGLE_INSTANCE_MUTEX: &str = r"Global\PadLend.SingleInstance";
/// 第二实例用它通知首实例“把窗口拿到前台”（1.0.2）。
pub const SINGLE_INSTANCE_EVENT: &str = r"Global\PadLend.Activate";

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------
/// 可执行文件所在目录（Python 的 `PROJECT_ROOT` / `APP_DIR` 对位物）。
///
/// 取不到时退化为当前工作目录，绝不 panic —— 路径解析失败也要让上层能继续
/// 走 `config_dir()` 的候选链。
pub fn app_dir() -> PathBuf {
    // 打包后（Tauri 把 resources 放在 exe 同目录）：用 exe 目录，
    // 与 Python 的 frozen 分支语义一致；源码运行时回到仓库根，对应 PROJECT_ROOT。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("THIRD_PARTY_NOTICES.md").is_file() || dir.join("vendor").is_dir() {
                return dir.to_path_buf();
            }
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 源码/资源根目录；单文件 sidecar 下与 `app_dir()` 相同。
pub fn project_root() -> PathBuf {
    app_dir()
}

/// 只读资源根目录（`vendor/` 的父目录）；与 `app_dir()` 相同。
pub fn bundle_dir() -> PathBuf {
    app_dir()
}

/// 随程序分发的二进制资源目录（scrcpy-server、adb 等）。
pub fn vendor_dir() -> PathBuf {
    bundle_dir().join("vendor")
}

/// 路径解析错误；文案与 Python 抛出的 `OSError` 逐字对应，便于界面直接显示。
#[derive(Debug, thiserror::Error)]
pub enum ConstantsError {
    /// 候选目录全部不可写（对应 Python 的 `OSError`）。
    #[error("找不到可写的配置目录（尝试过 PADLEND_HOME/APPDATA/LOCALAPPDATA/项目目录）")]
    NoWritableConfigDir,
    /// `PADLEND_HOME` 指定的目录创建失败（Python 会原样抛出 mkdir 的 OSError）。
    #[error("无法创建配置目录 {path}：{source}")]
    CreateConfigDir {
        /// 出错的路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: io::Error,
    },
}

/// 返回可写的 `base/name`；无法创建或写入时返回 `None`。
///
/// 设计要求 `%APPDATA%\PadLend`，但受限环境（沙箱/商店版 Python/
/// 受控文件夹访问）可能拒绝写用户目录，因此用真实探针而不是 `os.access`：
/// 建目录 → 写一个空文件 → 删掉，任一步失败都算不可写。
fn writable_dir(base: &Path, name: &str) -> Option<PathBuf> {
    let target = base.join(name);
    fs::create_dir_all(&target).ok()?;
    let probe = target.join(".write-probe");
    fs::write(&probe, b"").ok()?;
    // Python 用 `probe.unlink(missing_ok=True)`：删除失败同样算不可写。
    fs::remove_file(&probe).ok()?;
    Some(target)
}

/// 取环境变量；空值按“未设置”处理（Python 里 `if appdata:` 也是这个语义）。
fn env_nonempty(name: &str) -> Option<std::ffi::OsString> {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// 当前用户主目录（`ntpath.expanduser` 的取法：USERPROFILE → HOMEDRIVE+HOMEPATH）。
fn home_dir() -> Option<PathBuf> {
    if let Some(profile) = env_nonempty("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    let drive = env_nonempty("HOMEDRIVE");
    let path = env_nonempty("HOMEPATH")?;
    let mut home = PathBuf::from(drive.unwrap_or_default());
    home.push(path);
    Some(home)
}

/// `Path.expanduser()`：只处理 `~` 与 `~/...`；
/// `~user` 在 Windows 上与 Python 一样原样返回。
fn expand_user(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().unwrap_or(path);
    }
    let rest = text.strip_prefix("~\\").or_else(|| text.strip_prefix("~/"));
    match (rest, home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => path,
    }
}

/// Windows 路径比较不区分大小写、分隔符不敏感；与 `PureWindowsPath.__eq__` 对齐。
fn paths_eq(a: &Path, b: &Path) -> bool {
    fn norm(path: &Path) -> String {
        let mut text = path.to_string_lossy().replace('/', "\\");
        while text.len() > 3 && text.ends_with('\\') {
            text.pop();
        }
        text.to_lowercase()
    }
    norm(a) == norm(b)
}

/// `%APPDATA%\PadLend`（6.9）。
///
/// 找不到时按 `PADLEND_HOME` / `APPDATA` / `LOCALAPPDATA` / 项目目录逐级回退；
/// 项目目录下用隐藏名 `.padlend`，避免污染仓库。
pub fn config_dir() -> Result<PathBuf, ConstantsError> {
    config_dir_with(&|name| env_nonempty(name))
}

/// `config_dir` 的实现；`env` 只为测试时注入环境变量，运行时永远读真实环境。
fn config_dir_with(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, ConstantsError> {
    if let Some(override_home) = env("PADLEND_HOME") {
        let target = expand_user(PathBuf::from(override_home));
        // Python 在这里不捕获异常：创建失败直接向上抛。
        fs::create_dir_all(&target).map_err(|source| ConstantsError::CreateConfigDir {
            path: target.clone(),
            source,
        })?;
        return Ok(target);
    }

    let app_dir = app_dir();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = env("APPDATA") {
        candidates.push(PathBuf::from(appdata));
    }
    if let Some(local) = env("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local));
    }
    candidates.push(app_dir.clone());

    for base in candidates {
        let name = if paths_eq(&base, &app_dir) {
            ".padlend"
        } else {
            APP_NAME
        };
        if let Some(found) = writable_dir(&base, name) {
            return Ok(found);
        }
    }
    Err(ConstantsError::NoWritableConfigDir)
}

/// `%APPDATA%\PadLend\config.yaml`。
pub fn config_path() -> Result<PathBuf, ConstantsError> {
    Ok(config_dir()?.join("config.yaml"))
}

/// 日志目录（`config_dir()/logs`）。
pub fn log_dir() -> Result<PathBuf, ConstantsError> {
    Ok(config_dir()?.join("logs"))
}

/// 正常退出标记文件：存在 = 上次正常退出；缺失 = 需要启动前强制清理（7.5 / 12.3）。
pub fn clean_exit_marker() -> Result<PathBuf, ConstantsError> {
    Ok(config_dir()?.join("exit.clean"))
}

// ---------------------------------------------------------------------------
// scrcpy-server（6.1.2 / 9.5）
// ---------------------------------------------------------------------------
/// 服务端版本；必须与 `vendor/` 里的 jar 一致（设计文档 9.5）。
pub const SCRCPY_VERSION: &str = "5.0.1";
/// 本地 jar 文件名（Python 里由 `f"scrcpy-server-v{SCRCPY_VERSION}.jar"` 拼出）。
pub const SCRCPY_SERVER_JAR_NAME: &str = "scrcpy-server-v5.0.1.jar";
/// 远端文件名与设计一致，`pkill -f` 依赖它，改名会使 F7 清理失效。
pub const SCRCPY_REMOTE_JAR: &str = "/data/local/tmp/scrcpy-server-manual.jar";
/// 服务端主类名。
pub const SCRCPY_SERVER_CLASS: &str = "com.genymobile.scrcpy.Server";

/// `vendor/scrcpy-server-v5.0.1.jar` 的完整路径。
pub fn scrcpy_server_jar() -> PathBuf {
    vendor_dir().join(SCRCPY_SERVER_JAR_NAME)
}

/// 默认服务端端口（E104 换端口范围 1234–1244，默认取第一个）。
pub const DEFAULT_SERVER_PORT: u16 = 1234;
/// 端口候选范围 `tuple(range(1234, 1245))`，即 1234..=1244。
pub const SERVER_PORT_RANGE: Range<u16> = 1234..1245;

/// 服务端启动参数（6.1.2 实测可用，勿改参数名）。
///
/// `audio = true` 时服务端按 audio → control 的顺序 accept 两个 socket；
/// `raw_stream=true` 保证音频 socket 上是裸 PCM（无 codec 头）。
///
/// `port` 与 Python 版一样**不参与参数**（`tunnel_forward=true` 时端口由
/// adb forward 侧决定）；保留该参数只为让调用点与 Python 一一对应。
pub fn scrcpy_server_args(port: u16, audio: bool) -> Vec<String> {
    let _ = port;
    vec![
        format!("CLASSPATH={SCRCPY_REMOTE_JAR}"),
        "app_process".to_string(),
        "/".to_string(),
        SCRCPY_SERVER_CLASS.to_string(),
        SCRCPY_VERSION.to_string(),
        "tunnel_forward=true".to_string(),
        "video=false".to_string(),
        format!("audio={}", if audio { "true" } else { "false" }),
        "control=true".to_string(),
        "cleanup=true".to_string(),
        "raw_stream=true".to_string(),
        "send_dummy_byte=true".to_string(),
        "max_size=4096".to_string(),
    ]
}

/// 握手字节：scrcpy 5.0.1 在 `tunnel_forward` 且只开 control 通道时，
/// `DesktopConnection` 只 accept 一次并写 **1 个** 0 字节。
pub const HANDSHAKE_DUMMY: &[u8] = b"\x00";

// ---------------------------------------------------------------------------
// adb
// ---------------------------------------------------------------------------
/// adb 可执行文件名（在 PATH 或 `vendor/` 里查找）。
pub const ADB_EXE: &str = "adb.exe";
/// 普通 shell 命令超时（秒）。
pub const ADB_SHELL_TIMEOUT: f64 = 5.0;
/// 推送 jar 的超时（秒）。
pub const ADB_PUSH_TIMEOUT: f64 = 120.0;
/// `adb forward` 超时（秒）。
pub const ADB_FORWARD_TIMEOUT: f64 = 10.0;
/// 无线调试：connect/pair 要做 TCP 探测，比普通 adb 命令慢得多。
pub const ADB_CONNECT_TIMEOUT: f64 = 20.0;
/// 配对超时（秒）；配对码输入慢，留足余量。
pub const ADB_PAIR_TIMEOUT: f64 = 30.0;
/// adb server 启动/重启超时（秒）。
pub const ADB_SERVER_TIMEOUT: f64 = 8.0;

/// 无线调试端口提示：Android 11+ 的「无线调试」默认 5555，配对端口随机。
pub const WIRELESS_DEFAULT_PORT: u16 = 5555;
/// 建连超时（秒）。
pub const SOCKET_CONNECT_TIMEOUT: f64 = 5.0;
/// 握手超时（秒）。
pub const SOCKET_HANDSHAKE_TIMEOUT: f64 = 3.0;
/// app_process 起来后到开始在 abstract socket 上监听有几百毫秒延迟；
/// adb forward 会先接受本地连接再因远端未就绪而立即关闭，所以握手必须重试。
pub const HANDSHAKE_RETRY_TIMEOUT_S: f64 = 8.0;
/// 握手重试间隔（秒）。
pub const HANDSHAKE_RETRY_INTERVAL_S: f64 = 0.1;
/// 发送超时（秒）。
pub const SOCKET_SEND_TIMEOUT: f64 = 2.0;

/// 6.1.4 健康检查间隔（秒）。
pub const HEALTHCHECK_INTERVAL_S: f64 = 2.0;
/// 重连退避序列（秒）。
pub const RECONNECT_BACKOFF_S: [f64; 5] = [1.0, 2.0, 4.0, 8.0, 16.0];
/// 连续写失败多少次判定链路已死。
pub const CONSECUTIVE_WRITE_FAILURES: u32 = 3;

/// 6.1.1 首连重试：40s 内每 2s 一次，然后指数退避。
pub const INITIAL_RETRY_WINDOW_S: f64 = 20.0;
/// 首连重试间隔（秒）。
pub const INITIAL_RETRY_INTERVAL_S: f64 = 2.0;

/// 12.1 `pointer_speed` 复查间隔（秒）。
pub const POINTER_SPEED_POLL_S: f64 = 10.0;

// ---------------------------------------------------------------------------
// UHID / HID（9.3 / 9.4 / 附录 A）
// ---------------------------------------------------------------------------
/// UHID 设备标识：鼠标。
pub const UHID_MOUSE_ID: u16 = 0x0001;
/// UHID 设备标识：键盘。
pub const UHID_KEYBOARD_ID: u16 = 0x0002;

/// USB 厂商号（Linux Foundation）。
pub const HID_VENDOR_ID: u16 = 0x1D6B;
/// 鼠标产品号。
pub const HID_MOUSE_PRODUCT_ID: u16 = 0x0001;
/// 键盘产品号。
pub const HID_KEYBOARD_PRODUCT_ID: u16 = 0x0002;

/// 鼠标设备名（UHID `name` 字段要求定长字节串）。
pub const HID_MOUSE_NAME: &[u8] = b"PadLend Mouse";
/// 键盘设备名。
pub const HID_KEYBOARD_NAME: &[u8] = b"PadLend Keyboard";

/// 5 键 + 相对 X/Y + 滚轮（附录 A.2）。长度以实际字节为准，不写死 0x34。
pub const HID_MOUSE_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01,
    0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F,
    0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x06,
    0xC0, 0xC0,
];

/// 6KRO 键盘（附录 A.3）。
pub const HID_KEYBOARD_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
    0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
    0xC0,
];

/// 鼠标报文长度：按键位图 + dx + dy + wheel。
pub const MOUSE_REPORT_SIZE: usize = 4;
/// 键盘报文长度：修饰位 + 保留字节 + 6 键。
pub const KEYBOARD_REPORT_SIZE: usize = 8;

/// 键盘最多同时按下的键数（6KRO）。
pub const KEYBOARD_KRO: u8 = 6;

/// 描述符 Logical Max = 127，单报文位移上限。
pub const HID_CLAMP: i32 = 127;

/// Windows 原始滚轮一格是 120 单位（WHEEL_DELTA）；HID 滚轮一格是 1。
/// Router 按 120 累计换算成 HID detent，余数留到下一次事件。
pub const WHEEL_UNITS_PER_DETENT: i32 = 120;

/// 鼠标按键位图（9.3 报告字节 0）：左键。
pub const MOUSE_BTN_LEFT: u8 = 0x01;
/// 右键位。
pub const MOUSE_BTN_RIGHT: u8 = 0x02;
/// 中键位。
pub const MOUSE_BTN_MIDDLE: u8 = 0x04;
/// 侧键 1（后退）位。
pub const MOUSE_BTN_SIDE1: u8 = 0x08;
/// 侧键 2（前进）位。
pub const MOUSE_BTN_SIDE2: u8 = 0x10;

/// Raw Input 里表示“抬起”的 flag（Python 的 `RAW_BUTTON_UP_FLAGS` 集合）。
pub const RAW_BUTTON_UP_FLAGS: [u16; 5] = [0x0002, 0x0008, 0x0020, 0x0080, 0x0200];

/// Raw Input `usButtonFlags` → HID 按键位图（6.2.1）。
///
/// 对应 Python 的 `RAW_BUTTON_FLAGS` 字典（down/up 用同一个位图位，置位/清位
/// 由调用方决定）。
pub fn raw_button_flag(flag: u16) -> Option<u8> {
    match flag {
        // RI_MOUSE_LEFT_BUTTON_DOWN / UP
        0x0001 | 0x0002 => Some(MOUSE_BTN_LEFT),
        // RI_MOUSE_RIGHT_BUTTON_DOWN / UP
        0x0004 | 0x0008 => Some(MOUSE_BTN_RIGHT),
        // RI_MOUSE_MIDDLE_BUTTON_DOWN / UP
        0x0010 | 0x0020 => Some(MOUSE_BTN_MIDDLE),
        // RI_MOUSE_BUTTON_4_DOWN / UP
        0x0040 | 0x0080 => Some(MOUSE_BTN_SIDE1),
        // RI_MOUSE_BUTTON_5_DOWN / UP
        0x0100 | 0x0200 => Some(MOUSE_BTN_SIDE2),
        _ => None,
    }
}

/// flag 是否为“抬起”（对应 Python 的 `flag in RAW_BUTTON_UP_FLAGS`）。
pub fn raw_button_is_up(flag: u16) -> bool {
    RAW_BUTTON_UP_FLAGS.contains(&flag)
}

/// 键盘报告修饰位（6.7.2）：左 Ctrl。
pub const MOD_LEFT_CTRL: u8 = 0x01;
/// 左 Shift。
pub const MOD_LEFT_SHIFT: u8 = 0x02;
/// 左 Alt。
pub const MOD_LEFT_ALT: u8 = 0x04;
/// 左 GUI（Win）。
pub const MOD_LEFT_GUI: u8 = 0x08;
/// 右 Ctrl。
pub const MOD_RIGHT_CTRL: u8 = 0x10;
/// 右 Shift。
pub const MOD_RIGHT_SHIFT: u8 = 0x20;
/// 右 Alt（AltGr）。
pub const MOD_RIGHT_ALT: u8 = 0x40;
/// 右 GUI（Win）。
pub const MOD_RIGHT_GUI: u8 = 0x80;

// ---------------------------------------------------------------------------
// 输入 / 状态机默认阈值（6.5 / 6.9）
// ---------------------------------------------------------------------------
/// 判定期望位移（像素）。
pub const ARM_PX: i32 = 10;
/// 判定窗口（毫秒）。
pub const ARM_WINDOW_MS: i32 = 80;
/// 慢推兜底：没在 80ms 内攒够 `arm_px`，但已经明显向外推了这么多，
/// 仍视为有意接管。轻推/慢推的用户不该被 500px/s 的速度门槛挡住。
pub const ARM_SLOW_PX: i32 = 12;
/// 在平板边缘反向拖回多少像素才交还 PC。0.3.16 起 120（原来 24），
/// 1.2.1 起默认 80：保留防误退，同时缩短回 PC 的拖动距离。
pub const EXIT_OVERFLOW_PX: i32 = 80;

/// 边缘捕获带宽度（像素）。
pub const EDGE_BAND_PX: i32 = 1;
/// 进入平板区域前的安全边距（像素）。
pub const ENTRY_MARGIN_PX: i32 = 2;
/// 边缘事件去抖窗口（毫秒）。
pub const DEBOUNCE_MS: i32 = 250;
/// 桌面边界内缩（像素）；H3 规避手势时 8~16。
pub const BOUNDARY_INSET_PX: i32 = 0;
/// 6.3.6 角落裁决：首选方向得分须领先次选的比例。
pub const CORNER_DIRECTION_RATIO: f64 = 0.15;

/// 默认增益。
pub const DEFAULT_GAIN: f64 = 1.0;
/// 默认上报频率（Hz）。
pub const DEFAULT_REPORT_HZ: i32 = 1000;
/// 最小上报频率（Hz）。
pub const MIN_REPORT_HZ: i32 = 125;
/// 最大上报频率（Hz）。
pub const MAX_REPORT_HZ: i32 = 1000;
/// 0.3.16：上报频率只给四个固定档（设置页是下拉框，不再是滑条）。
pub const REPORT_HZ_CHOICES: [i32; 4] = [125, 250, 500, 1000];
/// 单帧最多 8 报文（D12）。
pub const MAX_BATCH: usize = 8;
/// 发送队列背压软阈值（不是硬上限）。
pub const SEND_QUEUE_MAXLEN: usize = 64;

/// 10.3 `timeBeginPeriod(1)`。
pub const TIMER_PERIOD_MS: u32 = 1;

/// 12.1 看门狗超时（秒）。
pub const HOOK_WATCHDOG_S: f64 = 30.0;

// ---------------------------------------------------------------------------
// Windows DPI（6.3.1）
// ---------------------------------------------------------------------------
/// `SetThreadDpiAwarenessContext` 的 PER_MONITOR_AWARE_V2 句柄值。
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: i32 = -4;

// ---------------------------------------------------------------------------
// 事件词表（7.4，测试据此断言，勿改）
// ---------------------------------------------------------------------------
/// 开始捕获。
pub const EVENT_CAP_START: &str = "cap.start";
/// 捕获命中平板。
pub const EVENT_CAP_HIT: &str = "cap.hit";
/// 结束捕获。
pub const EVENT_CAP_END: &str = "cap.end";
/// 判定失败。
pub const EVENT_ARM_FAIL: &str = "arm.fail";
/// 状态机故障。
pub const EVENT_STATE_FAULT: &str = "state.fault";
/// 平板连接。
pub const EVENT_TABLET_CONNECT: &str = "tablet.connect";
/// 平板重连。
pub const EVENT_TABLET_RECONNECT: &str = "tablet.reconnect";
/// 清理幽灵设备。
pub const EVENT_TABLET_CLEANUP_GHOST: &str = "tablet.cleanup.ghost";
/// PC 键盘抑制开启。
pub const EVENT_KBD_SUPPRESS_ON: &str = "kbd.suppress.on";
/// 设计文档 7.4 的固定词表承诺；当前 Python 侧没有发送者，保留供前端/后续配置热载使用。
pub const EVENT_CFG_RELOAD: &str = "cfg.reload";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEED: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_SEED.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "padlend-const-{tag}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scrcpy_server_args_matches_python() {
        let expected_without_audio = [
            "CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar",
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            "5.0.1",
            "tunnel_forward=true",
            "video=false",
            "audio=false",
            "control=true",
            "cleanup=true",
            "raw_stream=true",
            "send_dummy_byte=true",
            "max_size=4096",
        ];
        assert_eq!(scrcpy_server_args(DEFAULT_SERVER_PORT, false), expected_without_audio);
        let with_audio = scrcpy_server_args(DEFAULT_SERVER_PORT, true);
        assert_eq!(with_audio[7], "audio=true");
        assert_eq!(with_audio.len(), expected_without_audio.len());
        // Python 版从不使用 port 参数（端口由 adb forward 决定），这里也保持一致。
        assert_eq!(
            scrcpy_server_args(1244, false),
            scrcpy_server_args(DEFAULT_SERVER_PORT, false)
        );
    }

    #[test]
    fn raw_button_mapping_matches_python() {
        let cases = [
            (0x0001, MOUSE_BTN_LEFT),
            (0x0002, MOUSE_BTN_LEFT),
            (0x0004, MOUSE_BTN_RIGHT),
            (0x0008, MOUSE_BTN_RIGHT),
            (0x0010, MOUSE_BTN_MIDDLE),
            (0x0020, MOUSE_BTN_MIDDLE),
            (0x0040, MOUSE_BTN_SIDE1),
            (0x0080, MOUSE_BTN_SIDE1),
            (0x0100, MOUSE_BTN_SIDE2),
            (0x0200, MOUSE_BTN_SIDE2),
        ];
        for (flag, expected) in cases {
            assert_eq!(raw_button_flag(flag), Some(expected), "flag={flag:#06x}");
        }
        // 滚轮没有对应的按键位
        assert_eq!(raw_button_flag(0x0400), None);
        assert_eq!(raw_button_flag(0x0800), None);
        assert_eq!(raw_button_flag(0x0000), None);

        for flag in RAW_BUTTON_UP_FLAGS {
            assert!(raw_button_is_up(flag), "flag={flag:#06x}");
        }
        for flag in [0x0001u16, 0x0004, 0x0010, 0x0040, 0x0100, 0x0400] {
            assert!(!raw_button_is_up(flag), "flag={flag:#06x}");
        }
    }

    #[test]
    fn hid_report_geometry_matches_python() {
        assert_eq!(MOUSE_REPORT_SIZE, 4);
        assert_eq!(KEYBOARD_REPORT_SIZE, 8);
        assert_eq!(KEYBOARD_KRO, 6);
        assert_eq!(HID_CLAMP, 127);
        assert_eq!(WHEEL_UNITS_PER_DETENT, 120);
        assert_eq!(HID_VENDOR_ID, 0x1D6B);
        assert_eq!(UHID_MOUSE_ID, 0x0001);
        assert_eq!(UHID_KEYBOARD_ID, 0x0002);

        assert!(HID_MOUSE_DESCRIPTOR.starts_with(&[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01]));
        assert_eq!(HID_MOUSE_DESCRIPTOR.last().copied(), Some(0xC0));
        assert_eq!(HID_KEYBOARD_DESCRIPTOR.starts_with(&[0x05, 0x01, 0x09, 0x06, 0xA1, 0x01]), true);
        assert_eq!(HID_KEYBOARD_DESCRIPTOR.last().copied(), Some(0xC0));
        assert_eq!(HID_MOUSE_NAME, b"PadLend Mouse");
        assert_eq!(HID_KEYBOARD_NAME, b"PadLend Keyboard");
    }

    #[test]
    fn mouse_button_bits_are_distinct() {
        let bits = [
            MOUSE_BTN_LEFT,
            MOUSE_BTN_RIGHT,
            MOUSE_BTN_MIDDLE,
            MOUSE_BTN_SIDE1,
            MOUSE_BTN_SIDE2,
        ];
        let mut mask = 0u8;
        for bit in bits {
            assert_eq!(bit.count_ones(), 1, "位图必须是单比特：{bit:#04x}");
            assert_eq!(mask & bit, 0, "位图重复：{bit:#04x}");
            mask |= bit;
        }
        assert_eq!(mask, 0b1_1111);
    }

    #[test]
    fn port_range_matches_python() {
        assert_eq!(DEFAULT_SERVER_PORT, 1234);
        assert!(SERVER_PORT_RANGE.contains(&DEFAULT_SERVER_PORT));
        assert_eq!(SERVER_PORT_RANGE.start, 1234);
        assert_eq!(SERVER_PORT_RANGE.end, 1245);
        assert_eq!(SERVER_PORT_RANGE.count(), 11);
        assert_eq!(REPORT_HZ_CHOICES, [125, 250, 500, 1000]);
        assert_eq!(MIN_REPORT_HZ, 125);
        assert_eq!(MAX_REPORT_HZ, 1000);
        assert_eq!(DEFAULT_REPORT_HZ, 1000);
        assert_eq!(WIRELESS_DEFAULT_PORT, 5555);
        assert_eq!(HANDSHAKE_DUMMY, b"\x00");
    }

    #[test]
    fn app_dirs_are_children_of_app_dir() {
        let root = app_dir();
        assert_eq!(project_root(), root);
        assert_eq!(bundle_dir(), root);
        assert_eq!(vendor_dir(), root.join("vendor"));
        let jar = scrcpy_server_jar();
        assert_eq!(jar.parent().map(Path::to_path_buf), Some(vendor_dir()));
        assert_eq!(
            jar.file_name().map(|name| name.to_string_lossy().into_owned()),
            Some(SCRCPY_SERVER_JAR_NAME.to_string())
        );
        assert_eq!(SCRCPY_SERVER_JAR_NAME, "scrcpy-server-v5.0.1.jar");
    }

    #[test]
    fn config_paths_live_under_config_dir() {
        let base = config_dir().expect("默认配置目录应可写");
        assert_eq!(config_path().expect("config.yaml"), base.join("config.yaml"));
        assert_eq!(log_dir().expect("logs"), base.join("logs"));
        assert_eq!(
            clean_exit_marker().expect("exit.clean"),
            base.join("exit.clean")
        );
    }

    #[test]
    fn padlend_home_override_is_honoured() {
        let temp = TempDir::new("home");
        // 带一截不存在的子目录：config_dir 必须自己 create_dir_all 出来。
        let target = temp.path.join("nested").join("padlend-home");
        let resolved = config_dir_with(&|name| match name {
            "PADLEND_HOME" => Some(target.clone().into_os_string()),
            _ => None,
        })
        .expect("PADLEND_HOME 应被采纳");
        assert_eq!(resolved, target);
        assert!(resolved.is_dir());
        assert_eq!(resolved.join("config.yaml"), target.join("config.yaml"));
    }

    #[test]
    fn blank_home_override_fails_like_python() {
        // Python: os.environ.get 返回 "   " 是真值，Path("   ").mkdir() 抛 OSError，
        // 不会继续走后面的回退链；Rust 必须同样直接报错而不是悄悄忽略。
        let resolved = config_dir_with(&|name| match name {
            "PADLEND_HOME" => Some(std::ffi::OsString::from("   ")),
            _ => None,
        });
        assert!(
            matches!(resolved, Err(ConstantsError::CreateConfigDir { .. })),
            "空白 PADLEND_HOME 应与 Python 一样报错：{resolved:?}"
        );
    }

    #[test]
    fn appdata_fallback_chain_matches_python() {
        let temp = TempDir::new("chain");
        // APPDATA 可写 → 用它
        let appdata = temp.path.join("roaming");
        fs::create_dir_all(&appdata).expect("建 APPDATA");
        let local = temp.path.join("local");
        fs::create_dir_all(&local).expect("建 LOCALAPPDATA");
        let not_a_dir = temp.path.join("a-file");
        fs::write(&not_a_dir, b"x").expect("写文件");

        let resolved = config_dir_with(&|name| match name {
            "APPDATA" => Some(appdata.clone().into_os_string()),
            "LOCALAPPDATA" => Some(local.clone().into_os_string()),
            _ => None,
        })
        .expect("APPDATA 可用");
        assert_eq!(resolved, appdata.join(APP_NAME));
        assert!(resolved.is_dir());

        // APPDATA 指向文件 → 不可写，回退 %LOCALAPPDATA%\PadLend
        let resolved = config_dir_with(&|name| match name {
            "APPDATA" => Some(not_a_dir.clone().into_os_string()),
            "LOCALAPPDATA" => Some(local.clone().into_os_string()),
            _ => None,
        })
        .expect("回退到 LOCALAPPDATA");
        assert_eq!(resolved, local.join(APP_NAME));

        // 两者都不可写 → 程序目录下的隐藏目录 .padlend
        let resolved = config_dir_with(&|name| match name {
            "APPDATA" => Some(not_a_dir.clone().into_os_string()),
            "LOCALAPPDATA" => Some(not_a_dir.clone().into_os_string()),
            _ => None,
        })
        .expect("回退到项目目录");
        assert_eq!(resolved, app_dir().join(".padlend"));
        assert!(resolved.is_dir());
        let _ = fs::remove_dir_all(app_dir().join(".padlend"));
    }
}
