//! 音频通道转发（对应 tablet/audio.py，DESIGN 6.1.5、PITFALLS 2.6）。
//!
//! 这一层**只搬字节**：从 scrcpy 的音频 socket 读裸 PCM，原样写进本机播放器监听的
//! 端口（解码与播放都在 src-tauri/src/audio.rs）。保持极薄的理由是桌面侧已经有
//! 一套播放器，这里再做一次解码只会多一处不同步。
//!
//! 两个关键约定：
//! * 握手顺序：音频通道必须先于控制通道 accept（PITFALLS 2.6），session 负责顺序，
//!   本模块只负责建连与写 16 字节头部；
//! * 播放器不可用不是致命错误：degraded=true 后继续把音频读掉丢弃，否则服务端会
//!   因为 socket 缓冲满而卡住整条控制通道。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::tablet::session::{SocketFactory, SocketIo};

/// PCM 采样率（Hz）。
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// PCM 声道数。
pub const AUDIO_CHANNELS: u32 = 2;
/// PCM 位深。
pub const AUDIO_BITS: u32 = 16;
/// 本机播放器监听的头部：魔数 PLAU + 采样率/声道/位深（小端）。
pub const AUDIO_HEADER: [u8; 16] = build_header();
/// 连接播放器端口的超时（秒）。
pub const AUDIO_SINK_CONNECT_TIMEOUT_S: f64 = 2.0;
/// 音频 socket 的轮询超时（秒）：决定 stop() 的最长阻塞时间。
pub const AUDIO_SOCKET_POLL_S: f64 = 0.5;
/// 每次读取的字节数。
pub const AUDIO_READ_SIZE: usize = 65_536;
/// 日志前缀。
const LOG: &str = "[tablet.audio]";

/// 常量求值的头部构造（与 Python 的 struct.pack("<4sIII", b"PLAU", 48000, 2, 16) 逐字节一致）。
const fn build_header() -> [u8; 16] {
    let rate = AUDIO_SAMPLE_RATE.to_le_bytes();
    let channels = AUDIO_CHANNELS.to_le_bytes();
    let bits = AUDIO_BITS.to_le_bytes();
    [
        b'P', b'L', b'A', b'U', rate[0], rate[1], rate[2], rate[3], channels[0], channels[1],
        channels[2], channels[3], bits[0], bits[1], bits[2], bits[3],
    ]
}

/// 连到本机播放器端口并写头部（对应 connect_sink）。
///
/// 成功后把读超时设为 None 并返回 socket；失败时调用方按 degraded 处理。
pub fn connect_sink(
    port: u16,
    timeout: f64,
    factory: &dyn SocketFactory,
) -> Result<Arc<dyn SocketIo>, String> {
    let addr = format!("127.0.0.1:{port}");
    let duration = Duration::from_secs_f64(timeout.max(0.0));
    let socket = factory.connect(&addr, duration)?;
    if let Err(error) = socket.write_all(&AUDIO_HEADER) {
        let _ = socket.shutdown();
        return Err(format!("写入音频头部失败: {error}"));
    }
    let _ = socket.set_read_timeout(None);
    Ok(socket)
}

/// 音频转发器（对应 Python 的 AudioRelay）。
#[derive(Debug)]
pub struct AudioRelay {
    audio: Arc<dyn SocketIo>,
    factory: Arc<dyn SocketFactory>,
    sink: Mutex<Option<Arc<dyn SocketIo>>>,
    degraded: AtomicBool,
    bytes_forwarded: AtomicU64,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl AudioRelay {
    /// 构造转发器；sink_port 非 None 时立刻连播放器（失败只置 degraded）。
    pub fn new(
        audio: Arc<dyn SocketIo>,
        sink_port: Option<u16>,
        factory: Arc<dyn SocketFactory>,
    ) -> Arc<Self> {
        let mut sink: Option<Arc<dyn SocketIo>> = None;
        let mut degraded = false;
        if let Some(port) = sink_port {
            match connect_sink(port, AUDIO_SINK_CONNECT_TIMEOUT_S, factory.as_ref()) {
                Ok(socket) => sink = Some(socket),
                Err(error) => {
                    degraded = true;
                    eprintln!("{LOG} 音频播放器不可用，继续以静音方式读取音频: {error}");
                }
            }
        }
        Arc::new(Self {
            audio,
            factory,
            sink: Mutex::new(sink),
            degraded: AtomicBool::new(degraded),
            bytes_forwarded: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        })
    }

    /// 是否已经降级为静音（播放器不可用或写失败）。
    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// 已成功转发给播放器的字节数。
    pub fn bytes_forwarded(&self) -> u64 {
        self.bytes_forwarded.load(Ordering::SeqCst)
    }

    /// 转发线程是否在跑。
    pub fn running(&self) -> bool {
        self.thread
            .lock()
            .map(|guard| guard.as_ref().map(|handle| !handle.is_finished()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// 启动转发线程（幂等）。
    pub fn start(self: &Arc<Self>) {
        let mut guard = match self.thread.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if guard.is_some() {
            return;
        }
        let relay = Arc::clone(self);
        match thread::Builder::new()
            .name("padlend-audio".to_string())
            .spawn(move || relay.run())
        {
            Ok(handle) => *guard = Some(handle),
            Err(error) => eprintln!("{LOG} 音频转发线程启动失败: {error}"),
        }
    }

    /// 停止转发并关闭播放器端口（幂等；不关音频 socket 本身）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let handle = match self.thread.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(handle) = handle {
            let current = thread::current().id();
            if handle.thread().id() != current {
                join_timeout(handle, Duration::from_secs_f64(2.0));
            }
        }
        self.close_sink();
        if let Ok(mut guard) = self.sink.lock() {
            *guard = None;
        }
    }

    /// 关闭并丢弃播放器 socket。
    fn close_sink(&self) {
        let socket = match self.sink.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(socket) = socket {
            let _ = socket.shutdown();
        }
    }

    /// 转发线程主体（对应 _run）。
    fn run(self: Arc<Self>) {
        let mut buffer = vec![0u8; AUDIO_READ_SIZE];
        loop {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let read = self.audio.read(&mut buffer);
            let count = match read {
                Ok(0) => break,
                Ok(count) => count,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            };
            let chunk = &buffer[..count];
            let sink = match self.sink.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => None,
            };
            let Some(sink) = sink else {
                // 没有播放器时继续把音频读掉，避免服务端写阻塞。
                continue;
            };
            match sink.write_all(chunk) {
                Ok(()) => {
                    self.bytes_forwarded
                        .fetch_add(count as u64, Ordering::SeqCst);
                }
                Err(error) => {
                    self.degraded.store(true, Ordering::SeqCst);
                    eprintln!("{LOG} 音频播放器写入失败，后续音频将丢弃: {error}");
                    let _ = sink.shutdown();
                    if let Ok(mut guard) = self.sink.lock() {
                        *guard = None;
                    }
                }
            }
        }
        self.close_sink();
        eprintln!(
            "{LOG} 音频转发线程结束（已转发 {} 字节）",
            self.bytes_forwarded()
        );
        let _ = &self.factory;
    }
}

/// 带超时地等待线程结束：超时后放弃 join（线程自行退出，JoinHandle 被丢弃）。
fn join_timeout(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;

    #[derive(Debug)]
    struct FakeSocket {
        reads: Mutex<VecDeque<Vec<u8>>>,
        written: Mutex<Vec<u8>>,
        write_ok: AtomicBool,
        shut: AtomicBool,
        read_timeout: Mutex<Option<Option<Duration>>>,
        /// 写够这么多字节后开始报错（模拟播放器中途退出）；None = 不失败。
        fail_after: Option<usize>,
    }

    impl FakeSocket {
        fn new(chunks: Vec<&[u8]>) -> Arc<Self> {
            Arc::new(Self {
                reads: Mutex::new(chunks.into_iter().map(|chunk| chunk.to_vec()).collect()),
                written: Mutex::new(Vec::new()),
                write_ok: AtomicBool::new(true),
                shut: AtomicBool::new(false),
                read_timeout: Mutex::new(None),
                fail_after: None,
            })
        }

        /// 写够 limit 字节后开始报错（用于"头部写完、PCM 写失败"的用例）。
        fn new_failing_after(limit: usize) -> Arc<Self> {
            Arc::new(Self {
                reads: Mutex::new(VecDeque::new()),
                written: Mutex::new(Vec::new()),
                write_ok: AtomicBool::new(true),
                shut: AtomicBool::new(false),
                read_timeout: Mutex::new(None),
                fail_after: Some(limit),
            })
        }

        fn written(&self) -> Vec<u8> {
            self.written
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }

        fn read_timeout(&self) -> Option<Option<Duration>> {
            self.read_timeout.lock().ok().and_then(|guard| *guard)
        }
    }

    impl SocketIo for FakeSocket {
        fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
            let mut guard = match self.reads.lock() {
                Ok(guard) => guard,
                Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "锁损坏")),
            };
            match guard.pop_front() {
                None => Err(io::Error::new(io::ErrorKind::WouldBlock, "暂时没有数据")),
                Some(chunk) if chunk.is_empty() => Ok(0),
                Some(chunk) => {
                    let count = chunk.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&chunk[..count]);
                    Ok(count)
                }
            }
        }

        fn write_all(&self, data: &[u8]) -> io::Result<()> {
            if !self.write_ok.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "播放器已退出"));
            }
            let mut guard = match self.written.lock() {
                Ok(guard) => guard,
                Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "锁损坏")),
            };
            if let Some(limit) = self.fail_after {
                if guard.len() >= limit {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "播放器中途退出"));
                }
            }
            guard.extend_from_slice(data);
            Ok(())
        }

        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            if let Ok(mut guard) = self.read_timeout.lock() {
                *guard = Some(timeout);
            }
            Ok(())
        }

        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> io::Result<()> {
            self.shut.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeFactory {
        fail: bool,
        sink_fail_after: Option<usize>,
        created: Mutex<Vec<Arc<FakeSocket>>>,
    }

    impl FakeFactory {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                fail: false,
                sink_fail_after: None,
                created: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                fail: true,
                sink_fail_after: None,
                created: Mutex::new(Vec::new()),
            })
        }

        /// 头部能写完，PCM 开始失败（模拟播放器读到一半退出）。
        fn sink_dies_after_header() -> Arc<Self> {
            Arc::new(Self {
                fail: false,
                sink_fail_after: Some(AUDIO_HEADER.len()),
                created: Mutex::new(Vec::new()),
            })
        }

        fn last(&self) -> Option<Arc<FakeSocket>> {
            self.created
                .lock()
                .ok()
                .and_then(|guard| guard.last().cloned())
        }
    }

    impl SocketFactory for FakeFactory {
        fn connect(&self, _addr: &str, _timeout: Duration) -> Result<Arc<dyn SocketIo>, String> {
            if self.fail {
                return Err("播放器没在监听".to_string());
            }
            let socket = match self.sink_fail_after {
                Some(limit) => FakeSocket::new_failing_after(limit),
                None => FakeSocket::new(Vec::new()),
            };
            if let Ok(mut guard) = self.created.lock() {
                guard.push(socket.clone());
            }
            Ok(socket)
        }
    }

    fn wait_until<F: Fn() -> bool>(condition: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[test]
    fn header_is_plau_with_little_endian_fields() {
        let expected: [u8; 16] = [
            0x50, 0x4c, 0x41, 0x55, 0x80, 0xbb, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(AUDIO_HEADER, expected);
        assert_eq!(AUDIO_SAMPLE_RATE, 48_000);
        assert_eq!(AUDIO_CHANNELS, 2);
        assert_eq!(AUDIO_BITS, 16);
        assert_eq!(AUDIO_READ_SIZE, 65_536);
    }

    #[test]
    fn connect_sink_writes_header_and_clears_read_timeout() {
        let factory = FakeFactory::ok();
        let socket = connect_sink(40001, 2.0, factory.as_ref()).expect("应当连上");
        let created = factory.last().expect("应当创建 socket");
        assert_eq!(created.written(), AUDIO_HEADER.to_vec());
        assert_eq!(created.read_timeout(), Some(None));
        assert!(socket.set_read_timeout(None).is_ok());
    }

    #[test]
    fn relay_forwards_pcm_and_counts_bytes() {
        let audio = FakeSocket::new(vec![b"abcd", b"efgh", b""]);
        let factory = FakeFactory::ok();
        let relay = AudioRelay::new(audio, Some(9), factory.clone());
        relay.start();
        assert!(wait_until(|| relay.bytes_forwarded() == 8));
        relay.stop();
        let sink = factory.last().expect("应当连过播放器");
        let written = sink.written();
        assert_eq!(&written[..AUDIO_HEADER.len()], &AUDIO_HEADER[..]);
        assert_eq!(&written[AUDIO_HEADER.len()..], b"abcdefgh");
        assert!(!relay.degraded());
        assert_eq!(relay.bytes_forwarded(), 8);
    }

    #[test]
    fn relay_degrades_when_sink_write_fails() {
        let audio = FakeSocket::new(vec![b"abcd", b""]);
        let factory = FakeFactory::sink_dies_after_header();
        let relay = AudioRelay::new(audio, Some(9), factory.clone());
        relay.start();
        assert!(wait_until(|| relay.degraded()));
        relay.stop();
        assert_eq!(relay.bytes_forwarded(), 0);
    }

    #[test]
    fn connect_failure_marks_degraded_at_construction() {
        let audio = FakeSocket::new(vec![b""]);
        let factory = FakeFactory::failing();
        let relay = AudioRelay::new(audio, Some(9), factory);
        assert!(relay.degraded());
        assert!(!relay.running());
        relay.start();
        relay.stop();
        assert_eq!(relay.bytes_forwarded(), 0);
    }

    #[test]
    fn sink_port_none_still_drains_audio() {
        let audio = FakeSocket::new(vec![b"pcm", b""]);
        let factory = FakeFactory::ok();
        let relay = AudioRelay::new(audio, None, factory.clone());
        relay.start();
        assert!(wait_until(|| !relay.running()));
        relay.stop();
        assert!(factory.last().is_none());
        assert_eq!(relay.bytes_forwarded(), 0);
        assert!(!relay.degraded());
    }
}

