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

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::core::{implement, GUID, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl,
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
const TARGET_FRAMES: usize = 4_800; // 100 ms 预缓冲
const MAX_FRAMES: usize = 48_000; // 最多缓冲 1 s，防止内存无限增长
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
            if let Ok(mut guard) = buffer.lock() {
                guard.feed(&pending[..usable]);
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
    let mut last_error = String::new();
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

/// 将交错立体声 f32 样本转换成设备采样率；队列不足时输出静音。
struct PcmBuffer {
    queue: VecDeque<f32>,
    step: f64,
    pos: f64,
    started: bool,
}

impl PcmBuffer {
    fn new(step: f64) -> Self {
        Self {
            queue: VecDeque::with_capacity(MAX_FRAMES * 2),
            step,
            pos: 0.0,
            started: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks_exact(4) {
            let left = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0;
            let right = i16::from_le_bytes([chunk[2], chunk[3]]) as f32 / 32768.0;
            self.queue.push_back(left);
            self.queue.push_back(right);
        }
        let max_samples = MAX_FRAMES * 2;
        if self.queue.len() > max_samples {
            let drop = self.queue.len() - max_samples;
            self.queue.drain(..drop);
            // 丢弃旧样本后，插值位置也要跟着回退，避免音调跳变。
            self.pos = 0.0;
        }
    }

    fn next_frame(&mut self) -> (f32, f32) {
        if !self.started {
            if self.queue.len() < TARGET_FRAMES * 2 {
                return (0.0, 0.0);
            }
            self.started = true;
        }
        // 起播后只在“不足一帧”时静音；不能继续用 100ms 水位当门槛，
        // 否则每次缓冲降到水位以下都会插一段静音，形成周期性爆音。
        if self.queue.len() < 4 {
            return (0.0, 0.0);
        }
        let index = self.pos as usize;
        let sample_index = index * 2;
        if sample_index + 3 >= self.queue.len() {
            return (0.0, 0.0);
        }
        let frac = self.pos - index as f64;
        let l0 = self.queue[sample_index];
        let r0 = self.queue[sample_index + 1];
        let l1 = self.queue[sample_index + 2];
        let r1 = self.queue[sample_index + 3];
        let left = l0 + (l1 - l0) * frac as f32;
        let right = r0 + (r1 - r0) * frac as f32;

        self.pos += self.step;
        while self.pos >= 1.0 {
            self.queue.pop_front();
            self.queue.pop_front();
            self.pos -= 1.0;
        }
        (left, right)
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

    /// 一帧（左右声道同值）的 16-bit LE PCM 字节。
    fn frame_bytes(value: f32) -> Vec<u8> {
        let raw = (value * 32768.0) as i16;
        let bytes = raw.to_le_bytes();
        [bytes[0], bytes[1], bytes[0], bytes[1]].to_vec()
    }

    #[test]
    fn buffer_retiming_on_step_change() {
        // 设备换采样率（48k → 96k）后只改步长：相位 pos 保留，重采样无缝续播。
        let mut b = PcmBuffer::new(1.0);
        b.feed(&frame_bytes(0.5));
        b.feed(&frame_bytes(0.25));
        b.feed(&frame_bytes(0.125));
        // 跳过 100ms 预缓冲水位（私有字段，同 crate 测试可直改）。
        b.started = true;
        assert_eq!(b.next_frame(), (0.5, 0.5));
        // 步长 1.0 → 0.5：同一对帧之间走半程。
        b.set_step(0.5);
        assert_eq!(b.next_frame(), (0.25, 0.25));
        assert_eq!(b.next_frame(), (0.1875, 0.1875));
    }
}
