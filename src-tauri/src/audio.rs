//! 平板音频在本机播放（1.4.0）。
//!
//! 引擎把 scrcpy 的裸 PCM（48 kHz / 双声道 / 16-bit LE）通过
//! 127.0.0.1 TCP 发过来；本模块用 cpal 打开默认输出设备播放。协议只有
//! 一个 16 字节头：PLAU + 采样率 + 声道数 + 位深（均为 u32 LE）。
//!
//! 音频是可选能力：设备打不开、sink 断开都不应该影响控制通道，所以
//! 这里所有失败都只是结束当前连接，不向 Tauri 主流程传播。
//!
//! 1.7.2：每一步都写 `shell.log`（GUI 进程没有控制台，eprintln 看不到）。
//! 之前「平板静音了但电脑没声音」的现场只有引擎侧一句「播放器把连接关了」，
//! 播放器自己的退出原因完全丢失，只能靠这层日志定位。
//!
//! 2.2.2：输出设备跟随。此前实现是连接建立时查一次 `default_output_device()`、
//! 把流开在那个设备上直到连接结束——于是插上耳机（Windows 把默认设备切到
//! 耳机，其它应用都跟着切）平板声音还钉在扬声器上；反过来拔掉耳机（默认设备
//! 回到扬声器）流还挂在已消失的耳机端点上一路静音。现在每条连接由独立的
//! 渲染线程持有输出流，跟随系统默认输出设备：
//! - 设备身份用 MMDeviceEnumerator 的**端点 ID**（稳定唯一，同名设备不会认错）；
//! - 注册 `IMMNotificationClient`，默认设备切换 / 端点增删时即时跟随，另有
//!   500ms 轮询兜底；
//! - 端点变化就地重建 cpal 流（先停旧流再开新流，避免两个设备同播），TCP
//!   读循环与 PCM 缓冲原样保留，换设备不断流；
//! - 打开设备失败改为周期重试，不再放弃整条转发；输出流错误回调触发重建；
//! - COM 初始化 / 事件订阅失败时降级为按 cpal 设备名轮询。
//!
//! 2.3.2：音量合成器静音自愈。Windows 的 per-app 静音按「输出设备 + exe」
//! 持久化：用户某天在「音量合成器」里点掉 PadLend 的小喇叭，之后每次启动
//! 都无声，而本进程侧一切正常（转发在跑、缓冲区有水、峰值表有跳动、日志里
//! 没有任何错误）。现在每次打开 / 重建输出流之后枚举一遍本进程的音频会话，
//! `ISimpleAudioVolume::GetMute` 为真就取消静音并写一行日志；音量被调到 0%
//! 不自动改（更像有意压低），只提示位置。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, GUID, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, DEVICE_STATE, EDataFlow, ERole, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, ISimpleAudioVolume,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

const MAGIC: &[u8; 4] = b"PLAU";
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const BITS: u32 = 16;
const HEADER_LEN: usize = 16;
const READ_SIZE: usize = 4096;
/// 起播预缓冲：40 ms（2.3.1 之前是 100 ms，那是整条链路里最大的一笔固定延迟）。
const TARGET_FRAMES: usize = 1_920;
/// 自适应目标的下限 / 上限：20 ms ～ 120 ms。
const MIN_TARGET_FRAMES: usize = 960;
const MAX_TARGET_FRAMES: usize = 5_760;
/// 水位死区：误差在 10 ms 以内不修正，避免来回微调。
const DEADZONE_FRAMES: usize = 480;
/// 比例修正的时间常数：10 秒吃掉全部水位误差。
const TRIM_TAU_S: f64 = 10.0;
/// 最大音调偏移 ±0.5%（约 8 音分，正常听不出来）。
const TRIM_MAX: f64 = 0.005;
/// 水位超过目标 60 ms 就直接丢最旧的一段（突发/卡顿后的延迟回收）。
const HARD_TRIM_FRAMES: usize = 2_880;
/// 起播 / 丢块后的淡入长度：3 ms，避免爆音。
const FADE_FRAMES: usize = 144;
/// 每次欠载目标上调 10 ms。
const UNDER_RUN_STEP_FRAMES: usize = 480;
/// 稳定 30 秒才把目标降一档（5 ms）。
const DECAY_INTERVAL: Duration = Duration::from_secs(30);
/// 水位统计与日志周期。
const STATS_INTERVAL: Duration = Duration::from_secs(10);
const MAX_FRAMES: usize = 48_000; // 最多缓冲 1 s 的硬上限（正常由 trim 收回，不会走到这里）
/// 打开输出流之后补查「本应用是否被音量合成器静音」的轮数：WASAPI 会话要等
/// 第一个渲染回调才出现在会话枚举里，一次查不到很正常，按渲染线程的唤醒周期补查。
const MUTE_SCAN_ROUNDS: u8 = 3;
/// 设备跟随的轮询周期：正常时事件回调即时唤醒，这里只是兜底；
/// 也是重试「打开失败设备」的间隔。
const DEVICE_POLL_S: Duration = Duration::from_millis(500);
/// MMDeviceEnumerator 的 CLSID `{BCDE0395-E52F-467C-8E3D-C4579291692E}`。
/// windows 0.62 没有导出这个常量，手工定义（踩坑记录见 docs/PITFALLS.md 32.1）。
const CLSID_MM_DEVICE_ENUMERATOR: GUID =
    GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

/// 渲染线程的唤醒来源：设备事件回调要求复查，或 TCP 读循环宣告结束。
enum Wake {
    Recheck,
    Stop,
}

/// 本机音频接收器；绑定随机端口，把端口注入 sidecar 环境变量。
pub struct AudioServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl AudioServer {
    pub fn start() -> Result<Arc<Self>, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("音频端口绑定失败：{e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("读取音频端口失败：{e}"))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("设置音频端口非阻塞失败：{e}"))?;
        crate::shell_log!("[audio] 接收器已监听 127.0.0.1:{port}（引擎用 PADLEND_AUDIO_PORT 连过来）");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("padlend-audio".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let peer = peer.to_string();
                            crate::shell_log!("[audio] 引擎已连上接收器（{peer}），开始读 PLAU 头");
                            thread::spawn(move || match play_connection(stream) {
                                Ok(played) => crate::shell_log!(
                                    "[audio] 本次转发结束：{played} 字节 PCM，连接正常关闭"
                                ),
                                Err(e) => crate::shell_log!("[audio] 本次转发失败：{e}"),
                            });
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(40));
                        }
                        Err(e) => {
                            crate::shell_log!("[audio] 接收器 accept 失败：{e}");
                            thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            })
            .map_err(|e| format!("启动音频线程失败：{e}"))?;
        Ok(Arc::new(Self {
            port,
            stop,
            handle: Mutex::new(Some(handle)),
        }))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

}

impl Drop for AudioServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn play_connection(mut tcp: TcpStream) -> Result<u64, String> {
    // Windows 上 accept() 出来的 socket 会继承监听套接字的非阻塞标志：
    // AudioServer 为了能定时检查 stop 用的是非阻塞 listener，于是每条音频连接
    // 都是非阻塞的，第一次 read 立刻返回 WSAEWOULDBLOCK(10035)，播放线程当场
    // 退出——这正是 1.4.0~1.7.1「平板静音了但电脑没声音」的根因。
    // 播放连接必须是阻塞的（下面用读超时控制等待）。
    tcp.set_nonblocking(false)
        .map_err(|e| format!("切换音频连接为阻塞模式失败：{e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("设置音频读超时失败：{e}"))?;
    let mut header = [0u8; HEADER_LEN];
    tcp.read_exact(&mut header)
        .map_err(|e| format!("读取音频头失败：{e}"))?;
    if &header[0..4] != MAGIC {
        return Err(format!(
            "音频头 magic 不符：{:?}（期望 PLAU）",
            &header[0..4]
        ));
    }
    let rate = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let channels = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let bits = u32::from_le_bytes(header[12..16].try_into().unwrap());
    crate::shell_log!("[audio] 收到头部：{rate} Hz / {channels} ch / {bits} bit");
    if rate != SAMPLE_RATE || channels != CHANNELS || bits != BITS {
        return Err(format!(
            "不支持的音频格式：{rate} Hz / {channels} ch / {bits} bit"
        ));
    }
    tcp.set_read_timeout(None)
        .map_err(|e| format!("恢复音频阻塞读失败：{e}"))?;

    // 2.2.2：输出流不再「连接建立时开一次、开在哪个设备就锁死哪个设备」，
    // 而是交给下面的渲染线程持有并跟随系统默认输出设备（见模块注释）。
    // 读循环只负责喂数据；插拔耳机 / 手动切换默认设备既不断转发管道，
    // 也不会让平板声音留在旧设备上。
    let buffer = Arc::new(Mutex::new(PcmBuffer::new(1.0))); // 步长由首次建流按设备采样率设置
    let force = Arc::new(AtomicBool::new(false));
    let (wake_tx, wake_rx) = mpsc::channel::<Wake>();
    let renderer = {
        let buffer = Arc::clone(&buffer);
        let force = Arc::clone(&force);
        // 设备事件回调需要一个唤醒通道的克隆（COM 侧长期持有）。
        let notify_tx = wake_tx.clone();
        thread::Builder::new()
            .name("padlend-audio-render".to_string())
            .spawn(move || renderer_loop(buffer, force, notify_tx, wake_rx))
            .map_err(|e| format!("启动音频渲染线程失败：{e}"))?
    };

    let mut pending: Vec<u8> = Vec::new();
    // i16 → f32 的转换挪到锁外：一个 64KB 块是 1.6 万帧，放在 Mutex 里转会让
    // 输出回调的 try_lock 扑空、整周期静音（目标水位压到 40ms 后尤其明显）。
    let mut samples: Vec<f32> = Vec::with_capacity(READ_SIZE / 2);
    let mut chunk = [0u8; READ_SIZE];
    let mut received: u64 = 0;
    let mut failure: Option<String> = None;
    loop {
        let n = match tcp.read(&mut chunk) {
            Ok(0) => {
                crate::shell_log!("[audio] 引擎关闭了接收器连接（已收 {received} 字节）");
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // 兜底：万一 socket 又被谁改回非阻塞，短暂等待后继续，别丢整条连接。
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(e) => {
                failure = Some(format!("读取音频流失败（已收 {received} 字节）：{e}"));
                break;
            }
        };
        received += n as u64;
        pending.extend_from_slice(&chunk[..n]);
        let usable = pending.len() - pending.len() % 4;
        if usable > 0 {
            samples.clear();
            for frame in pending[..usable].chunks_exact(4) {
                samples.push(i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0);
                samples.push(i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32768.0);
            }
            if let Ok(mut guard) = buffer.lock() {
                guard.push_samples(&samples);
            }
            pending.drain(..usable);
        }
    }

    // TCP 结束（引擎断开或读错误）：通知渲染线程停流、反注册设备回调、
    // 退出 COM。读错误也先收尾再返回，保证渲染线程一定被回收。
    let _ = wake_tx.send(Wake::Stop);
    drop(wake_tx);
    if renderer.join().is_err() {
        crate::shell_log!("[audio] 音频渲染线程异常退出");
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(received),
    }
}

/// 当前流绑定：端点标识（或降级时的设备名）、展示名、cpal 流本体。
struct LiveStream {
    key: String,
    display: String,
    /// 字段从不读取，但必须持有：流 drop 即停止播放（切换设备时先 drop 旧流）。
    #[allow(dead_code)]
    stream: cpal::Stream,
}

/// 渲染线程：持有 cpal 输出流并跟随系统默认输出设备（2.2.2）。
///
/// COM 必须在本线程、任何 cpal 调用之前初始化成 MTA：cpal 的 wasapi 宿主
/// 随后会在同线程做 STA 初始化并拿到 `RPC_E_CHANGED_MODE`，而它把该返回值
/// 当成功（cpal host/wasapi/com.rs），所以「我们的 MTA + cpal 的流」同线程
/// 混用是安全的——但顺序必须是我们先初始化。
fn renderer_loop(
    buffer: Arc<Mutex<PcmBuffer>>,
    force: Arc<AtomicBool>,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
) {
    let com = ComGuard::init();
    let mut enumerator: Option<IMMDeviceEnumerator> = None;
    let mut registered: Option<IMMNotificationClient> = None;
    if com.is_some() {
        unsafe {
            // P1 显式钉成 Option<&IUnknown>：裸 None 推不出来 Param 的实现。
            match CoCreateInstance::<Option<&windows::core::IUnknown>, IMMDeviceEnumerator>(
                &CLSID_MM_DEVICE_ENUMERATOR,
                None,
                CLSCTX_ALL,
            ) {
                Ok(enm) => {
                    // 设备事件回调来自系统线程；mpsc::Sender 不是 Sync，
                    // 包一层 Mutex 才能放进 #[implement] 结构体。
                    let client: IMMNotificationClient = Notifications {
                        wake: Mutex::new(wake_tx.clone()),
                    }
                    .into();
                    if enm.RegisterEndpointNotificationCallback(&client).is_ok() {
                        registered = Some(client);
                        enumerator = Some(enm);
                        crate::shell_log!(
                            "[audio] 已订阅系统音频设备事件，默认设备切换即时跟随"
                        );
                    } else {
                        crate::shell_log!("[audio] 设备事件订阅失败：降级为 500ms 轮询跟随");
                    }
                }
                Err(e) => {
                    crate::shell_log!(
                        "[audio] 音频设备枚举器创建失败（{e}）：降级为 500ms 轮询跟随"
                    );
                }
            }
        }
    } else {
        crate::shell_log!("[audio] COM 初始化失败：降级为 500ms 轮询（按设备名比对）");
    }

    let mut current: Option<LiveStream> = None;
    // 2.3.2：还剩几轮「本应用是否被音量合成器静音」的补查（0 = 不查）。
    let mut mute_scan: u8 = 0;
    let mut last_error = String::new();
    let mut last_stats = Instant::now();
    loop {
        let forced = force.swap(false, Ordering::Relaxed);
        let want_key = match enumerator.as_ref() {
            Some(enm) => default_endpoint_id(enm),
            None => default_device_fallback_key(),
        };
        if should_rebuild(
            current.as_ref().map(|live| live.key.as_str()),
            want_key.as_deref(),
            forced,
        ) {
            let old = current.take();
            let old_name = old.as_ref().map(|live| live.display.clone());
            // 先停旧流再开新流：切换瞬间宁可无声几十毫秒，也不要两个设备同播。
            drop(old);
            match open_default_stream(&buffer, &force) {
                Ok((display, stream)) => {
                    if let Some(old) = old_name {
                        crate::shell_log!("[audio] 默认输出设备变化：{old} → {display}");
                    }
                    current = Some(LiveStream {
                        key: want_key.unwrap_or_else(|| display.clone()),
                        display,
                        stream,
                    });
                    last_error.clear();
                    mute_scan = MUTE_SCAN_ROUNDS;
                }
                Err(e) => {
                    // 打不开（设备被独占、配置读不出等）不打断转发：保住 TCP
                    // 通道，下个周期自动重试；同样的错误只记一次，避免刷日志。
                    if e != last_error {
                        crate::shell_log!("[audio] 打开输出流失败（{e}），稍后自动重试");
                        last_error = e;
                    }
                }
            }
        }
        // 2.3.2：新流的会话刚建出来时还不在枚举里，补查几轮；查到就收手，
        // 免得每 500 ms 白枚举一次会话表。播放中途用户手动静音是有意为之，
        // 合成器就在手边——这里不跟用户抢。
        if mute_scan > 0 {
            let outcome = match enumerator.as_ref() {
                Some(enm) => unmute_own_session(enm),
                None => SessionScan::Failed("COM 不可用".to_string()),
            };
            if let SessionScan::Failed(reason) = &outcome {
                crate::shell_log!("[audio] 音量合成器静音检查未完成（{reason}）：不影响播放");
            }
            mute_scan = next_mute_scan(mute_scan, &outcome);
        }
        if last_stats.elapsed() >= STATS_INTERVAL {
            if let Ok(pcm) = buffer.lock() {
                let s = pcm.stats();
                crate::shell_log!(
                    "[audio] 缓冲水位 {:.1}ms / 目标 {:.0}ms（欠载 {} 次，丢块 {} 次）",
                    s.fill_ms,
                    s.target_ms,
                    s.underruns,
                    s.drops
                );
            }
            last_stats = Instant::now();
        }
        match wake_rx.recv_timeout(DEVICE_POLL_S) {
            Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Wake::Recheck) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    if let (Some(enm), Some(client)) = (enumerator.as_ref(), registered.as_ref()) {
        unsafe {
            let _ = enm.UnregisterEndpointNotificationCallback(client);
        }
    }
    // registered / enumerator 在此依声明逆序释放（回调引用先于枚举器），
    // com 声明在最先、最后释放——正好是 COM 要求的反注册后再退出顺序。
}

/// 是否需要（重）建输出流。
///
/// - `current` / `want`：当前流绑定的端点标识 / 系统此刻的默认端点标识；
/// - `want = None` 表示此刻查不到默认设备（输出设备全被拔掉/禁用，或查询
///   瞬时失败）：保住现有流等下一轮，不要拆掉仅剩的流；
/// - `forced` 来自输出流错误回调（最典型是端点被拔掉后的
///   `AUDCLNT_E_DEVICE_INVALIDATED`）：标识相同也要就地重建。
fn should_rebuild(current: Option<&str>, want: Option<&str>, forced: bool) -> bool {
    if forced {
        return true;
    }
    match (current, want) {
        (Some(current), Some(want)) => current != want,
        (None, Some(_)) => true,
        (Some(_), None) | (None, None) => false,
    }
}

/// 系统当前默认输出端点的 ID（形如 `{0.0.0.00000000}.{……}`）。
/// 端点 ID 稳定唯一：两块同名设备（如「扬声器 (2- High Definition Audio)」）
/// 也不会认错——这正是设备名做不到的。
fn default_endpoint_id(enm: &IMMDeviceEnumerator) -> Option<String> {
    unsafe {
        let device = enm.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id = device.GetId().ok()?;
        // GetId 返回的字符串由 CoTaskMem 分配，必须手工释放
        // （本函数每 500ms 就可能被调用一次，泄漏会持续累积）。
        let key = id.to_string().ok();
        CoTaskMemFree(Some(id.as_ptr() as *const std::ffi::c_void));
        key
    }
}

/// COM 不可用时的降级设备身份：cpal 默认输出设备的名字。
/// 名字可能撞（两块同型号声卡），聊胜于无。
fn default_device_fallback_key() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

/// 一轮「本应用音频会话是否被音量合成器静音」检查的结果。
enum SessionScan {
    /// 找到了本进程的音频会话（无论原本是否静音）。
    Found,
    /// 会话还没出现：流的第一次渲染可能尚未发生。
    NotYet,
    /// 查询本身失败（COM、会话管理器等）：重试也不会变。
    Failed(String),
}

/// 一轮检查之后还剩几轮补查（0 表示不再查）。
fn next_mute_scan(rounds_left: u8, outcome: &SessionScan) -> u8 {
    match outcome {
        SessionScan::NotYet => rounds_left.saturating_sub(1),
        SessionScan::Found | SessionScan::Failed(_) => 0,
    }
}

/// 2.3.2：本应用在 Windows 音量合成器里被单独静音时，把它取消掉。
///
/// 合成器的 per-app 静音（`ISimpleAudioVolume::SetMute`）是按「输出设备 + 可执行
/// 文件」持久化的：用户某天在「音量合成器」里点过一次小喇叭，之后每次启动
/// PadLend 声音都是哑的。PadLend 自己完全看不出异常——TCP 在转发、缓冲区有水、
/// 峰值表有跳动、日志里也没有任何错误——只有合成器里那一个静音图标。3a1fb88 修
/// 的是「平板静音 / scrcpy 采集断」，这类本地静音完全不在那条链路上。
///
/// 只在打开 / 重建输出流之后调用：此时 WASAPI 会话刚按「设备 + exe」的持久化
/// 状态创建出来，正是要纠正的时刻。只清静音、不改音量：把音量调到 0% 更像有意
/// 压低，只记一行日志交给用户。
fn unmute_own_session(enm: &IMMDeviceEnumerator) -> SessionScan {
    // SAFETY: 调用方（renderer_loop）已在本线程把 COM 初始化成 MTA，会话对象只在
    // 本线程内使用、不跨线程传递。
    let volumes = match unsafe { own_session_volumes(enm) } {
        Ok(volumes) => volumes,
        Err(reason) => return SessionScan::Failed(reason),
    };
    if volumes.is_empty() {
        // 会话要等输出流第一次渲染才出现：这不是错误，留给补查。
        return SessionScan::NotYet;
    }
    for volume in &volumes {
        let muted = unsafe { volume.GetMute() }
            .map(|flag| flag.as_bool())
            .unwrap_or(false);
        if muted {
            match unsafe { volume.SetMute(false, std::ptr::null()) } {
                Ok(()) => crate::shell_log!(
                    "[audio] 本应用被 Windows 音量合成器静音，已自动恢复（该状态按 设备+程序 持久化）"
                ),
                Err(e) => crate::shell_log!(
                    "[audio] 本应用被 Windows 音量合成器静音，自动取消失败（{e}）：请到 设置→系统→声音→音量合成器 里手动打开 PadLend"
                ),
            }
        }
        if let Ok(level) = unsafe { volume.GetMasterVolume() } {
            if level <= 0.001 {
                crate::shell_log!(
                    "[audio] 注意：音量合成器里 PadLend 的音量是 0%，请在 设置→系统→声音→音量合成器 里调高（静音会自动恢复，音量不会）"
                );
            }
        }
    }
    SessionScan::Found
}

/// 本进程在当前默认输出端点上的音频会话音量对象。
///
/// WASAPI 会话按「进程 + 端点」建：同一个 exe 每起一条输出流就多一个会话，而静音
/// 标志是这组会话共用的——全部收上来才不会被「另一个会话没静音」骗过去。
unsafe fn own_session_volumes(
    enm: &IMMDeviceEnumerator,
) -> Result<Vec<ISimpleAudioVolume>, String> {
    let me = std::process::id();
    let device = enm
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| format!("取默认输出设备失败：{e}"))?;
    let manager: IAudioSessionManager2 = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("打开音频会话管理器失败：{e}"))?;
    let sessions = manager
        .GetSessionEnumerator()
        .map_err(|e| format!("枚举音频会话失败：{e}"))?;
    let mut volumes = Vec::new();
    for index in 0..sessions.GetCount().unwrap_or(0) {
        let Ok(control) = sessions.GetSession(index) else {
            continue;
        };
        // 进程 ID 在 IAudioSessionControl2 上；拿不到就跳过这一路。
        let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
            continue;
        };
        if control2.GetProcessId().ok() != Some(me) {
            continue;
        }
        // 会话对象本身实现 ISimpleAudioVolume（QI 就能拿到），
        // 不需要按 GUID 版 GetSimpleAudioVolume。
        if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
            volumes.push(volume);
        }
    }
    Ok(volumes)
}

/// 在系统当前默认输出设备上打开并启动播放流，返回（设备名, 流）。
fn open_default_stream(
    buffer: &Arc<Mutex<PcmBuffer>>,
    force: &Arc<AtomicBool>,
) -> Result<(String, cpal::Stream), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "没有可用的音频输出设备".to_string())?;
    let device_name = device.name().unwrap_or_else(|_| "未知设备".to_string());
    let supported = device
        .default_output_config()
        .map_err(|e| format!("读取默认音频格式失败（设备 {device_name}）：{e}"))?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let device_channels = config.channels as usize;
    // 设备采样率不是 48 kHz 也没关系：重采样步长跟着设备走（PcmBuffer 的
    // 相位 pos 恒在 [0,1)，跨步长保留，切换后无缝续播）。
    if let Ok(mut pcm) = buffer.lock() {
        pcm.set_step(SAMPLE_RATE as f64 / config.sample_rate.0.max(1) as f64);
    }
    crate::shell_log!(
        "[audio] 输出设备：{device_name} / {sample_format:?} / {} ch / {} Hz",
        config.channels,
        config.sample_rate.0
    );
    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, buffer, device_channels, force),
        SampleFormat::I16 => build_stream::<i16>(&device, &config, buffer, device_channels, force),
        SampleFormat::U16 => build_stream::<u16>(&device, &config, buffer, device_channels, force),
        SampleFormat::I8 => build_stream::<i8>(&device, &config, buffer, device_channels, force),
        SampleFormat::I32 => build_stream::<i32>(&device, &config, buffer, device_channels, force),
        SampleFormat::I64 => build_stream::<i64>(&device, &config, buffer, device_channels, force),
        SampleFormat::U8 => build_stream::<u8>(&device, &config, buffer, device_channels, force),
        SampleFormat::U32 => build_stream::<u32>(&device, &config, buffer, device_channels, force),
        SampleFormat::U64 => build_stream::<u64>(&device, &config, buffer, device_channels, force),
        SampleFormat::F64 => build_stream::<f64>(&device, &config, buffer, device_channels, force),
        // SampleFormat 是 non_exhaustive：以后 cpal 加新格式时落到这里降级报错。
        other => Err(format!("不支持的输出采样格式：{other:?}")),
    }
    .map_err(|e| format!("创建音频输出流失败（设备 {device_name} / {sample_format:?}）：{e}"))?;
    stream
        .play()
        .map_err(|e| format!("启动音频输出失败（设备 {device_name}）：{e}"))?;
    Ok((device_name, stream))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: &Arc<Mutex<PcmBuffer>>,
    device_channels: usize,
    force: &Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Sample,
{
    let shared = Arc::clone(buffer);
    let force_flag = Arc::clone(force);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let channels = device_channels.max(1);
                let mut guard = match shared.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        for sample in data.iter_mut() {
                            *sample = T::from_sample(0.0f32);
                        }
                        return;
                    }
                };
                for frame in data.chunks_mut(channels) {
                    let (left, right) = guard.next_frame();
                    for (index, sample) in frame.iter_mut().enumerate() {
                        let value = if channels == 1 {
                            (left + right) * 0.5
                        } else if index % 2 == 0 {
                            left
                        } else {
                            right
                        };
                        *sample = T::from_sample(value);
                    }
                }
            },
            move |err| {
                crate::shell_log!("[audio] 输出回调报错：{err}");
                // 端点失效（AUDCLNT_E_DEVICE_INVALIDATED 等）时流已不可救：
                // 置位让渲染线程就地重建（2.2.2）。
                force_flag.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| format!("创建音频输出流失败：{e}"))
}

/// 线程级 COM 初始化守卫（MTA）。必须在同线程任何 cpal 调用之前创建。
struct ComGuard;

impl ComGuard {
    /// 失败（如线程已被其它模式初始化）返回 None，调用方降级运行。
    fn init() -> Option<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.is_ok().then_some(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// `IMMNotificationClient` 实现：渲染端点的增删 / 状态 / 默认切换都唤醒
/// 渲染线程复查。回调来自系统线程，`mpsc::Sender` 不是 `Sync`，
/// 包一层 `Mutex` 才能放进 `#[implement]` 结构体。
#[implement(IMMNotificationClient)]
struct Notifications {
    wake: Mutex<mpsc::Sender<Wake>>,
}

impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device: &PCWSTR) -> windows::core::Result<()> {
        // 新端点插入时默认设备可能切换（系统「自动切换到新设备」策略），
        // OnDefaultDeviceChanged 会另行通知，这里保持安静。
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device: &PCWSTR) -> windows::core::Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _device: &PCWSTR,
    ) -> windows::core::Result<()> {
        // 只关心输出端点；输入设备的默认切换与我们无关。
        if flow == eRender {
            self.notify();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl Notifications_Impl {
    fn notify(&self) {
        if let Ok(tx) = self.wake.lock() {
            let _ = tx.send(Wake::Recheck);
        }
    }
}

/// 把交错立体声 f32 样本转换成设备采样率；队列不足时输出静音。
///
/// 2.3.1 起水位不再固定 100 ms：起播按 `target_frames`（默认 40 ms），欠载就
/// 上调、稳定就下调；平板与声卡的时钟差用 ±0.5% 的比例修正吃掉，延迟因此被
/// 钉在目标值附近，不会随播放时长往上漂。
struct PcmBuffer {
    queue: VecDeque<f32>,
    step: f64,
    pos: f64,
    started: bool,
    /// 刚欠载：等水位回到目标一半再恢复出声，避免在水位 0 附近一路断音。
    starved: bool,
    /// 当前目标水位（帧）：欠载上调、稳定下调。
    target_frames: usize,
    /// 淡入剩余帧数（起播、丢块后防爆音）。
    fade_remaining: usize,
    /// 每 1024 帧才查一次要不要降目标，别在回调里每帧读时钟。
    decay_tick: u32,
    underruns: u64,
    drops: u64,
    /// 最近一次欠载 / 降档的时刻。
    last_event: Instant,
    /// 测试里关掉比例修正，让重采样断言保持精确（生产恒为 true）。
    trim_enabled: bool,
}

/// 水位统计（写 shell.log 用）。
struct PcmStats {
    fill_ms: f64,
    target_ms: f64,
    underruns: u64,
    drops: u64,
}

impl PcmBuffer {
    fn new(step: f64) -> Self {
        Self {
            queue: VecDeque::with_capacity(MAX_FRAMES * 2),
            step,
            pos: 0.0,
            started: false,
            starved: false,
            target_frames: TARGET_FRAMES,
            fade_remaining: 0,
            decay_tick: 0,
            underruns: 0,
            drops: 0,
            last_event: Instant::now(),
            trim_enabled: true,
        }
    }

    fn fill_frames(&self) -> usize {
        self.queue.len() / 2
    }

    fn stats(&self) -> PcmStats {
        PcmStats {
            fill_ms: self.fill_frames() as f64 * 1000.0 / SAMPLE_RATE as f64,
            target_ms: self.target_frames as f64 * 1000.0 / SAMPLE_RATE as f64,
            underruns: self.underruns,
            drops: self.drops,
        }
    }

    /// 写入已解码的交错立体声样本（锁内只做搬运，不做转换）。
    fn push_samples(&mut self, samples: &[f32]) {
        self.queue.extend(samples.iter().copied());
        // 1 s 硬上限：极端卡顿后的兜底，正常由目标水位收回。
        let max_samples = MAX_FRAMES * 2;
        if self.queue.len() > max_samples {
            let drop = self.queue.len() - max_samples;
            self.queue.drain(..drop);
            self.pos = 0.0;
            self.fade_remaining = FADE_FRAMES;
            self.drops += 1;
        }
        // 水位超过目标 60 ms（突发、卡顿恢复后）不等比例修正慢慢收：
        // 直接丢最旧的一段，否则延迟要几十秒才降回来。
        if self.started {
            let fill = self.fill_frames();
            if fill > self.target_frames + HARD_TRIM_FRAMES {
                let drop = (fill - self.target_frames) * 2;
                self.queue.drain(..drop.min(self.queue.len()));
                self.pos = 0.0;
                self.fade_remaining = FADE_FRAMES;
                self.drops += 1;
            }
        }
    }

    fn next_frame(&mut self) -> (f32, f32) {
        let fill = self.fill_frames();

        // 稳定一段时间后把目标水位降回默认，延迟不越用越大。
        self.decay_tick += 1;
        if self.decay_tick >= 1_024 {
            self.decay_tick = 0;
            self.maybe_decay(fill);
        }

        if !self.started {
            if fill < self.target_frames {
                return (0.0, 0.0);
            }
            self.started = true;
            self.starved = false;
            self.fade_remaining = FADE_FRAMES;
        }

        if self.starved {
            // 欠载后必须攒回目标的一半，不然会在水位 0 附近反复断音。
            if fill < self.target_frames / 2 {
                return (0.0, 0.0);
            }
            self.starved = false;
            self.fade_remaining = FADE_FRAMES;
        }

        if fill < 2 {
            self.on_underrun();
            return (0.0, 0.0);
        }
        let index = self.pos as usize;
        let sample_index = index * 2;
        if sample_index + 3 >= self.queue.len() {
            self.on_underrun();
            return (0.0, 0.0);
        }
        let frac = self.pos - index as f64;
        let l0 = self.queue[sample_index];
        let r0 = self.queue[sample_index + 1];
        let l1 = self.queue[sample_index + 2];
        let r1 = self.queue[sample_index + 3];
        let mut left = l0 + (l1 - l0) * frac as f32;
        let mut right = r0 + (r1 - r0) * frac as f32;

        // 起播 / 丢块后的 3 ms 淡入，避免从半波起点出声时爆一下。
        if self.fade_remaining > 0 {
            self.fade_remaining -= 1;
            let gain = (FADE_FRAMES - self.fade_remaining) as f32 / FADE_FRAMES as f32;
            left *= gain;
            right *= gain;
        }

        // 时钟差补偿：水位富余就快一点点、吃紧就慢一点点（最多 ±0.5%）。
        let step = self.step * (1.0 + self.trim_for_fill(fill));
        self.pos += step;
        while self.pos >= 1.0 {
            self.queue.pop_front();
            self.queue.pop_front();
            self.pos -= 1.0;
        }
        (left, right)
    }

    /// 按水位误差给重采样步长一个比例修正。
    ///
    /// 误差 10 ms、时间常数 10 s → 步长偏 0.1%；误差 50 ms 以上封顶 ±0.5%
    /// （约 8 音分）。10 ms 死区保证在目标附近来回时不抖动。
    fn trim_for_fill(&self, fill: usize) -> f64 {
        if !self.trim_enabled {
            return 0.0;
        }
        let error = fill as f64 - self.target_frames as f64;
        if error.abs() <= DEADZONE_FRAMES as f64 {
            return 0.0;
        }
        (error / SAMPLE_RATE as f64 / TRIM_TAU_S).clamp(-TRIM_MAX, TRIM_MAX)
    }

    fn on_underrun(&mut self) {
        self.starved = true;
        self.underruns += 1;
        self.last_event = Instant::now();
        if self.target_frames < MAX_TARGET_FRAMES {
            self.target_frames =
                (self.target_frames + UNDER_RUN_STEP_FRAMES).min(MAX_TARGET_FRAMES);
        }
    }

    /// 连续 30 秒没有欠载，才把目标水位降 5 ms（最低 20 ms）。
    fn maybe_decay(&mut self, fill: usize) {
        if self.target_frames <= MIN_TARGET_FRAMES || fill + DEADZONE_FRAMES < self.target_frames {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_event) < DECAY_INTERVAL {
            return;
        }
        self.target_frames = self
            .target_frames
            .saturating_sub(240)
            .max(MIN_TARGET_FRAMES);
        self.last_event = now;
    }

    /// 重采样步长随新设备的采样率变化（2.2.2：切换输出设备用）。
    /// pos 是相位（恒在 [0,1)），跨步长保留，切换后无缝续播。
    fn set_step(&mut self, step: f64) {
        self.step = step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_decision() {
        // 输出回调报错（forced）：标识相同也要就地重建。
        assert!(should_rebuild(Some("a"), Some("a"), true));
        // 端点没变：保持。
        assert!(!should_rebuild(Some("a"), Some("a"), false));
        // 端点变了：重建。
        assert!(should_rebuild(Some("a"), Some("b"), false));
        // 尚无流而目标存在：开流。
        assert!(should_rebuild(None, Some("b"), false));
        // 瞬时查不到默认设备：保住现有流等下一轮，别拆掉仅剩的流。
        assert!(!should_rebuild(Some("a"), None, false));
        assert!(!should_rebuild(None, None, false));
    }

    /// 真机用例（默认忽略）：起一条输出流让 WASAPI 为本进程建出会话，把合成器里
    /// 本进程的静音打开，再让 `unmute_own_session` 去恢复——它必须报 `Found`，并且
    /// 会话确实从静音变回有声。没装音频设备的机器（CI）跑不了，所以默认忽略，
    /// 真机回归时用 `cargo test unmute_own_session -- --ignored --nocapture`。
    #[test]
    #[ignore = "需要真实音频输出设备"]
    fn unmute_own_session_restores_a_muted_session() {
        let com = ComGuard::init();
        assert!(com.is_some(), "COM 初始化失败");
        let enm: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, None, CLSCTX_ALL) }
                .expect("创建设备枚举器");
        let buffer = Arc::new(Mutex::new(PcmBuffer::new(1.0)));
        let force = Arc::new(AtomicBool::new(false));
        let device = cpal::default_host()
            .default_output_device()
            .expect("需要音频输出设备");
        let supported = device.default_output_config().expect("读默认输出格式");
        let config = supported.config();
        let channels = config.channels as usize;
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, &buffer, channels, &force),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, &buffer, channels, &force),
            other => {
                // 用例只覆盖本机真实出现过的默认格式，别的格式不代表功能没实现。
                eprintln!("跳过：本机默认输出格式 {other:?} 不在用例覆盖范围");
                return;
            }
        }
        .expect("建输出流");
        stream.play().expect("启动输出");
        // 会话要等第一帧渲染才出现；静音状态按「设备 + exe」持久化，先自己打开。
        std::thread::sleep(Duration::from_millis(500));
        let volumes = unsafe { own_session_volumes(&enm) }.expect("枚举本进程会话");
        assert!(!volumes.is_empty(), "本进程应当已有音频会话");
        for volume in &volumes {
            unsafe { volume.SetMute(true, std::ptr::null()) }.expect("静音");
        }
        assert!(
            matches!(unmute_own_session(&enm), SessionScan::Found),
            "应能找到本进程的会话"
        );
        for volume in &volumes {
            assert!(
                !unsafe { volume.GetMute() }.expect("读静音").as_bool(),
                "静音应当已被恢复"
            );
        }
    }

    #[test]
    fn mute_scan_stops_when_the_session_shows_up() {
        // 会话还没出现：按轮数递减，查完为止（说到底也就几次，别无限枚举）。
        assert_eq!(next_mute_scan(3, &SessionScan::NotYet), 2);
        assert_eq!(next_mute_scan(1, &SessionScan::NotYet), 0);
        assert_eq!(next_mute_scan(0, &SessionScan::NotYet), 0);
        // 找到会话、或查询本身失败：不再补查。
        assert_eq!(next_mute_scan(3, &SessionScan::Found), 0);
        assert_eq!(next_mute_scan(3, &SessionScan::Failed("x".to_string())), 0);
    }

    #[test]
    fn buffer_retiming_on_step_change() {
        // 设备换采样率（48k → 96k）后只改步长：相位 pos 保留，重采样无缝续播。
        let mut b = PcmBuffer::new(1.0);
        b.trim_enabled = false; // 关掉时钟修正，断言才精确
        b.push_samples(&[0.5, 0.5]);
        b.push_samples(&[0.25, 0.25]);
        b.push_samples(&[0.125, 0.125]);
        b.started = true;
        assert_eq!(b.next_frame(), (0.5, 0.5));
        // 步长 1.0 → 0.5：同一对帧之间走半程。
        b.set_step(0.5);
        assert_eq!(b.next_frame(), (0.25, 0.25));
        assert_eq!(b.next_frame(), (0.1875, 0.1875));
    }

    #[test]
    fn starts_at_the_new_40ms_watermark() {
        let mut b = PcmBuffer::new(1.0);
        b.trim_enabled = false;
        // 差 1 帧不到目标：仍然静音。
        b.push_samples(&vec![0.5; (TARGET_FRAMES - 1) * 2]);
        assert_eq!(b.next_frame(), (0.0, 0.0));
        // 补满目标：起播；首帧带 3 ms 淡入，所以只要求「出声」。
        b.push_samples(&[0.5, 0.5]);
        let (left, right) = b.next_frame();
        assert!(left > 0.0 && left < 0.5, "起播首帧应当淡入: {left}");
        assert_eq!(left, right);
    }

    #[test]
    fn excess_water_is_trimmed_by_resampling() {
        let mut b = PcmBuffer::new(1.0);
        b.started = true;
        // 比目标多 50 ms（2400 帧）：步长应当快 0.5%，水位被慢慢吃掉。
        b.push_samples(&vec![0.5; (TARGET_FRAMES + 2_400) * 2]);
        let before = b.fill_frames();
        // 0.5% 的修正即每秒吃掉 240 帧，攒回目标要 10 秒（48 万帧）。
        for _ in 0..480_000 {
            // 生产者按 1x 供数、消费者按约 1.005x 取数。
            b.push_samples(&[0.5, 0.5]);
            b.next_frame();
        }
        let after = b.fill_frames();
        assert!(
            after + 1_000 < before,
            "水位应当被吃回去: {before} -> {after}"
        );
    }

    #[test]
    fn trim_is_zero_inside_the_deadzone_and_clamped_outside() {
        // 把目标调大，才能同时构造出「富余 50ms」与「亏欠 50ms」两侧的封顶。
        let mut b = PcmBuffer::new(1.0);
        b.target_frames = 10_000;
        assert_eq!(b.trim_for_fill(10_000), 0.0);
        assert_eq!(b.trim_for_fill(10_000 + DEADZONE_FRAMES), 0.0);
        // 误差 50ms（时间常数 10s）正好打在 ±0.5% 的钳位上。
        assert!((b.trim_for_fill(10_000 + 2_400) - TRIM_MAX).abs() < 1e-9);
        assert!((b.trim_for_fill(10_000 - 2_400) + TRIM_MAX).abs() < 1e-9);
    }

    #[test]
    fn underrun_raises_target_and_waits_for_refill() {
        let mut b = PcmBuffer::new(1.0);
        b.trim_enabled = false;
        b.started = true;
        // 空队列打一拍：判定欠载，目标涨 10 ms。
        assert_eq!(b.next_frame(), (0.0, 0.0));
        assert_eq!(b.underruns, 1);
        assert_eq!(b.target_frames, TARGET_FRAMES + UNDER_RUN_STEP_FRAMES);
        // 水位回到目标一半之前继续静音。
        b.push_samples(&vec![0.5; (b.target_frames / 2 - 1) * 2]);
        assert_eq!(b.next_frame(), (0.0, 0.0));
        b.push_samples(&vec![0.5; 2]);
        let (left, _) = b.next_frame();
        assert!(left > 0.0, "攒回一半水位后应当恢复出声");
    }

    #[test]
    fn burst_above_target_is_dropped_immediately() {
        let mut b = PcmBuffer::new(1.0);
        b.started = true;
        b.push_samples(&vec![
            0.5;
            (TARGET_FRAMES + HARD_TRIM_FRAMES + 480) * 2
        ]);
        assert_eq!(b.drops, 1);
        assert!(b.fill_frames() <= TARGET_FRAMES + 8);
    }
}
