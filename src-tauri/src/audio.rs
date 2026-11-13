//! 平板音频在本机播放（1.4.0）。
//!
//! Python 侧把 scrcpy 的裸 PCM（48 kHz / 双声道 / 16-bit LE）通过
//! 127.0.0.1 TCP 发过来；本模块用 cpal 打开默认输出设备播放。协议只有
//! 一个 16 字节头：PLAU + 采样率 + 声道数 + 位深（均为 u32 LE）。
//!
//! 音频是可选能力：设备打不开、sink 断开都不应该影响控制通道，所以
//! 这里所有失败都只是结束当前连接，不向 Tauri 主流程传播。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAGIC: &[u8; 4] = b"PLAU";
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const BITS: u32 = 16;
const HEADER_LEN: usize = 16;
const READ_SIZE: usize = 4096;
const TARGET_FRAMES: usize = 4_800; // 100 ms 预缓冲
const MAX_FRAMES: usize = 48_000; // 最多缓冲 1 s，防止内存无限增长

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
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("padlend-audio".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            thread::spawn(move || {
                                if let Err(e) = play_connection(stream) {
                                    eprintln!("音频播放结束：{e}");
                                }
                            });
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(40));
                        }
                        Err(e) => {
                            eprintln!("音频 accept 失败：{e}");
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

fn play_connection(mut tcp: TcpStream) -> Result<(), String> {
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("设置音频读超时失败：{e}"))?;
    let mut header = [0u8; HEADER_LEN];
    tcp.read_exact(&mut header)
        .map_err(|e| format!("读取音频头失败：{e}"))?;
    if &header[0..4] != MAGIC {
        return Err("音频头 magic 不符".to_string());
    }
    let rate = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let channels = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let bits = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if rate != SAMPLE_RATE || channels != CHANNELS || bits != BITS {
        return Err(format!(
            "不支持的音频格式：{rate} Hz / {channels} ch / {bits} bit"
        ));
    }
    tcp.set_read_timeout(None)
        .map_err(|e| format!("恢复音频阻塞读失败：{e}"))?;

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "没有可用的音频输出设备".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("读取默认音频格式失败：{e}"))?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let device_channels = config.channels as usize;
    let step = SAMPLE_RATE as f64 / config.sample_rate.0.max(1) as f64;
    let buffer = Arc::new(Mutex::new(PcmBuffer::new(step)));

    let output = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, &buffer, device_channels),
        SampleFormat::I16 => build_stream::<i16>(&device, &config, &buffer, device_channels),
        SampleFormat::U16 => build_stream::<u16>(&device, &config, &buffer, device_channels),
        other => Err(format!("不支持的输出采样格式：{other:?}")),
    }?;
    output
        .play()
        .map_err(|e| format!("启动音频输出失败：{e}"))?;

    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; READ_SIZE];
    loop {
        let n = match tcp.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("读取音频流失败：{e}")),
        };
        pending.extend_from_slice(&chunk[..n]);
        let usable = pending.len() - pending.len() % 4;
        if usable > 0 {
            if let Ok(mut guard) = buffer.lock() {
                guard.feed(&pending[..usable]);
            }
            pending.drain(..usable);
        }
    }
    Ok(())
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: &Arc<Mutex<PcmBuffer>>,
    device_channels: usize,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Sample,
{
    let shared = Arc::clone(buffer);
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
            |err| eprintln!("音频输出错误：{err}"),
            None,
        )
        .map_err(|e| format!("创建音频输出流失败：{e}"))
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
}
