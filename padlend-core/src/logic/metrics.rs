//! 实时指标滑窗（对应 `core/metrics.py`，设计文档 7.4 / 10.3 / 11）。
//!
//! GUI 每 2s 取一次 `Metrics::snapshot()`，得到 7.4 约定的指标（JSON 形状与
//! Python 返回的 dict 逐键一致）：
//!
//! ```text
//! {"raw_hz": 998.0, "send_hz": 998.0, "backlog": 0,
//!  "rtt_ms": 12, "p50_ms": 10, "p95_ms": 21,
//!  "drop_pct": 0.0, "cpu_pct": 2.1, "mem_mb": 84}
//! ```
//!
//! 设计要点（与 Python 版逐条对应）：
//!
//! * 所有计数器有锁；`record_*` 由 T4/T5 线程调用，`snapshot` 由 GUI 线程调用。
//! * 速率是**最近 1 秒窗口**内的计数（窗口内事件数 / `window_s`），而不是累计平均；
//!   因此启动/空闲时会自然回落到 0。
//! * 延迟样本只有最近 256 条（`deque(maxlen=256)`）；`reset` 会一并清空。
//! * 没有采样时**不报** `rtt_ms` / `p50_ms` / `p95_ms` 三个键——报 0 会被
//!   GUI 当成真实值，用户看到的就是“右上角 RTT 一直是 0ms”。
//! * Python 的 `psutil` 是可选依赖：导入失败或调用失败时 CPU/内存返回 0，绝不
//!   让 GUI 因为指标采集而崩溃。Rust 侧用 [`ProcessUsage`] 注入同样的语义，
//!   默认实现 [`NullProcessUsage`] 永远返回 `(0.0, 0.0)`（等价于 psutil 缺失）。

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use serde::Serialize;
use thiserror::Error;

/// 延迟样本滑窗长度（Python `_LATENCY_WINDOW = 256`）。
pub const LATENCY_WINDOW: usize = 256;

/// 字节 → MB 的换算因子（Python `_BYTES_PER_MB = 1024.0 * 1024.0`）。
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// 指标层的错误（Python 里抛 `ValueError` 的地方 + 序列化失败）。
#[derive(Debug, Error)]
pub enum MetricsError {
    /// Python: `raise ValueError(f"window_s 必须为正: {window_s!r}")`。
    #[error("window_s 必须为正: {0:?}")]
    WindowNotPositive(f64),
    /// 快照转 JSON 失败（理论上不会发生，保留以便上层统一处理）。
    #[error("指标快照序列化失败: {0}")]
    Json(#[from] serde_json::Error),
}

/// 进程 CPU / 内存采样（Python `psutil.Process().cpu_percent(None)` 与
/// `memory_info().rss / 1MiB` 的替身）。
///
/// 实现必须是“绝不失败”的：采集异常就在内部吞掉并返回 0.0，与 Python 的
/// `try/except` 一致。`usage` 每次 `snapshot` 调用一次。
pub trait ProcessUsage: Send + Sync + fmt::Debug {
    /// 返回 `(cpu_pct, rss_mb)`；失败字段为 0.0。
    fn usage(&self) -> (f64, f64);
}

/// 默认实现：没有 CPU/内存采样能力时恒返回 `(0.0, 0.0)`
/// （对应 Python 里 `psutil` 导入失败）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NullProcessUsage;

impl ProcessUsage for NullProcessUsage {
    fn usage(&self) -> (f64, f64) {
        (0.0, 0.0)
    }
}

/// 字节 → MB（实现 [`ProcessUsage`] 时换算 `memory_info().rss` 用）。
pub fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_MB
}

/// 一次快照，字段与 Python `snapshot()` 返回的 dict 一一对应。
///
/// 序列化顺序就是 Python dict 的插入顺序（raw_hz / send_hz / backlog /
/// drop_pct / cpu_pct / mem_mb / rtt_ms / p50_ms / p95_ms）；没有延迟样本时
/// 后三个键整个不出现（`skip_serializing_if`）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    /// 最近窗口内的 RawInput 事件速率。
    pub raw_hz: f64,
    /// 最近窗口内成功写出的 HID 报告速率（含心跳）。
    pub send_hz: f64,
    /// 发送队列当前深度。
    pub backlog: i64,
    /// 丢包百分比（`dropped / (dropped + received) * 100`）。
    pub drop_pct: f64,
    /// 进程 CPU 占用百分比。
    pub cpu_pct: f64,
    /// 进程常驻内存 MB。
    pub mem_mb: f64,
    /// 最近一次往返延迟；无样本时**不出现**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// 延迟中位数；无样本时不出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    /// 延迟 P95；无样本时不出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
}

impl Snapshot {
    /// 转成 `serde_json::Value`（键集合与 Python dict 相同）。
    pub fn to_value(&self) -> Result<serde_json::Value, MetricsError> {
        Ok(serde_json::to_value(self)?)
    }

    /// 转成一行 JSON（破坏性写入前做 diff 时用）。
    pub fn to_json(&self) -> Result<String, MetricsError> {
        Ok(serde_json::to_string(self)?)
    }
}

/// 滑窗内部状态；所有字段都在 [`Metrics`] 的锁里。
#[derive(Debug, Default)]
struct Inner {
    raw: VecDeque<f64>,
    sent: VecDeque<f64>,
    latencies: VecDeque<f64>,
    backlog: i64,
    raw_total: i64,
    sent_total: i64,
    dropped: i64,
}

/// 线程安全的滑窗指标聚合器（Python `class Metrics`）。
pub struct Metrics {
    window_s: f64,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    usage: Box<dyn ProcessUsage>,
    inner: Mutex<Inner>,
}

/// 默认时钟：进程内单调计时（对应 Python `time.perf_counter`）。
fn perf_counter() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

impl Metrics {
    /// 用默认时钟与“无 CPU/内存采样”构造（对应 Python `Metrics(window_s=..)`）。
    ///
    /// `window_s <= 0` 返回 [`MetricsError::WindowNotPositive`]（Python 抛 ValueError）。
    pub fn new(window_s: f64) -> Result<Self, MetricsError> {
        Self::with_parts(window_s, perf_counter, NullProcessUsage)
    }

    /// 默认参数（`window_s = 1.0`）：对应 Python `Metrics()`。
    pub fn with_defaults() -> Self {
        Self {
            window_s: 1.0,
            clock: Box::new(perf_counter),
            usage: Box::new(NullProcessUsage),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// 注入可推进的时钟（测试 / 回放用；Python `Metrics(clock=...)`）。
    pub fn with_clock<F>(window_s: f64, clock: F) -> Result<Self, MetricsError>
    where
        F: Fn() -> f64 + Send + Sync + 'static,
    {
        Self::with_parts(window_s, clock, NullProcessUsage)
    }

    /// 注入 CPU/内存采样实现。
    pub fn with_usage<U>(window_s: f64, usage: U) -> Result<Self, MetricsError>
    where
        U: ProcessUsage + 'static,
    {
        Self::with_parts(window_s, perf_counter, usage)
    }

    /// 同时注入时钟与采样实现。
    pub fn with_parts<F, U>(window_s: f64, clock: F, usage: U) -> Result<Self, MetricsError>
    where
        F: Fn() -> f64 + Send + Sync + 'static,
        U: ProcessUsage + 'static,
    {
        // 与 Python 完全一致：if window_s <= 0: raise（NaN 会被接受）。
        if window_s <= 0.0 {
            return Err(MetricsError::WindowNotPositive(window_s));
        }
        Ok(Self {
            window_s,
            clock: Box::new(clock),
            usage: Box::new(usage),
            inner: Mutex::new(Inner::default()),
        })
    }

    /// 取锁；锁被毒化（别的线程 panic 过）时继续用内部状态，绝不再 panic。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    // -- 记录（T4/T5 线程） -------------------------------------------------

    /// 记录 count 个 RawInput 事件（`<= 0` 直接忽略）。
    pub fn record_raw(&self, count: i64) {
        if count <= 0 {
            return;
        }
        let now = (self.clock)();
        let mut inner = self.lock();
        for _ in 0..count {
            inner.raw.push_back(now);
        }
        inner.raw_total += count;
        self.prune_locked(&mut inner, now);
    }

    /// 记录成功写出的 HID 报告数（含心跳）。
    pub fn record_send(&self, count: i64) {
        if count <= 0 {
            return;
        }
        let now = (self.clock)();
        let mut inner = self.lock();
        for _ in 0..count {
            inner.sent.push_back(now);
        }
        inner.sent_total += count;
        self.prune_locked(&mut inner, now);
    }

    /// 记录一次输入→设备写入的往返延迟（毫秒；负数忽略）。
    pub fn record_latency(&self, ms: f64) {
        let value = ms;
        if value < 0.0 {
            return;
        }
        let mut inner = self.lock();
        inner.latencies.push_back(value);
        // Python deque(maxlen=256)：超出后从左侧淘汰。
        while inner.latencies.len() > LATENCY_WINDOW {
            inner.latencies.pop_front();
        }
    }

    /// 记录因发送队列满而丢弃的报告数。
    pub fn record_drop(&self, count: i64) {
        if count <= 0 {
            return;
        }
        self.lock().dropped += count;
    }

    /// 更新发送队列当前深度（`0..SEND_QUEUE_MAXLEN`；负数夹到 0）。
    pub fn set_backlog(&self, depth: i64) {
        self.lock().backlog = depth.max(0);
    }

    // -- 快照（GUI 线程） ---------------------------------------------------

    /// 返回 7.4 约定的指标快照（新对象，调用方可安全持有）。
    pub fn snapshot(&self) -> Snapshot {
        // 与 Python 相同：先取 CPU/内存，再取时间戳，再进锁。
        let (cpu_pct, mem_mb) = self.usage.usage();
        let now = (self.clock)();
        let (raw_hz, send_hz, latencies, dropped, received, backlog) = {
            let mut inner = self.lock();
            self.prune_locked(&mut inner, now);
            let raw_hz = inner.raw.len() as f64 / self.window_s;
            let send_hz = inner.sent.len() as f64 / self.window_s;
            let mut latencies: Vec<f64> = inner.latencies.iter().copied().collect();
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (
                raw_hz,
                send_hz,
                latencies,
                inner.dropped,
                inner.raw_total,
                inner.backlog,
            )
        };

        let rtt_ms = latencies.last().copied();
        let p50_ms = if latencies.is_empty() {
            None
        } else {
            Some(percentile(&latencies, 0.50))
        };
        let p95_ms = if latencies.is_empty() {
            None
        } else {
            Some(percentile(&latencies, 0.95))
        };

        let attempted = dropped + received;
        let drop_pct = if attempted != 0 {
            dropped as f64 / attempted as f64 * 100.0
        } else {
            0.0
        };

        Snapshot {
            raw_hz: round_digits(raw_hz, 1),
            send_hz: round_digits(send_hz, 1),
            backlog,
            drop_pct: round_digits(drop_pct, 2),
            cpu_pct: round_digits(cpu_pct, 1),
            mem_mb: round_digits(mem_mb, 1),
            rtt_ms: rtt_ms.map(|value| round_digits(value, 1)),
            p50_ms: p50_ms.map(|value| round_digits(value, 1)),
            p95_ms: p95_ms.map(|value| round_digits(value, 1)),
        }
    }

    /// 清空所有滑窗与累计计数（配置重载 / 重连后调用）。
    pub fn reset(&self) {
        let mut inner = self.lock();
        inner.raw.clear();
        inner.sent.clear();
        inner.latencies.clear();
        inner.backlog = 0;
        inner.raw_total = 0;
        inner.sent_total = 0;
        inner.dropped = 0;
    }

    // -- 内部 ---------------------------------------------------------------

    /// 丢掉窗口之外的旧样本（严格小于 cutoff 才淘汰，与 Python 的 `<` 一致）。
    fn prune_locked(&self, inner: &mut Inner, now: f64) {
        let cutoff = now - self.window_s;
        while let Some(front) = inner.raw.front() {
            if *front < cutoff {
                inner.raw.pop_front();
            } else {
                break;
            }
        }
        while let Some(front) = inner.sent.front() {
            if *front < cutoff {
                inner.sent.pop_front();
            } else {
                break;
            }
        }
    }
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("Metrics")
            .field("window_s", &self.window_s)
            .field("raw", &inner.raw.len())
            .field("sent", &inner.sent.len())
            .field("latencies", &inner.latencies.len())
            .field("backlog", &inner.backlog)
            .field("raw_total", &inner.raw_total)
            .field("sent_total", &inner.sent_total)
            .field("dropped", &inner.dropped)
            .finish()
    }
}

/// 线性插值分位数；空列表返回 0（对应 Python `_percentile`）。
///
/// `sorted_values` 必须已排序（Python 也只接受排好序的 list）。
/// Python 的 `int(pos)` 是向零截断，这里用 `trunc` 保持一致；
/// `fraction` 超出 `[0, 1]` 时 Python 会 IndexError，这里夹到最后一档
/// （调用方只传 0.5 / 0.95）。
pub fn percentile(sorted_values: &[f64], fraction: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    if sorted_values.len() == 1 {
        return sorted_values[0];
    }
    let last = sorted_values.len() - 1;
    let pos = last as f64 * fraction;
    let lower = (pos.trunc() as usize).min(last);
    let upper = (lower + 1).min(last);
    let weight = pos - lower as f64;
    sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight
}

/// Python `round(value, digits)` 的等价实现。
///
/// CPython 的 `round` 对浮点做“按十进制正确舍入 + 半进偶”，Rust 的
/// `{:.N}` 格式化同样是精确十进制输出（实测 2.25 → 2.2、286.25 → 286.2、
/// 10.45 → 10.4 全部一致），再解析回最近的 f64 就得到与 Python 相同的值。
/// NaN / ±inf 原样返回（Python 也是）。
pub fn round_digits(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{:.*}", digits, value)
        .parse::<f64>()
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// 可推进的单调时钟（对应 pytest `tests/fakes.py:FakeClock`）。
    struct FakeClock(Arc<Mutex<f64>>);

    impl FakeClock {
        fn new(now: f64) -> Self {
            Self(Arc::new(Mutex::new(now)))
        }

        fn advance(&self, seconds: f64) {
            if let Ok(mut guard) = self.0.lock() {
                *guard += seconds;
            }
        }

        fn handle(&self) -> impl Fn() -> f64 + Send + Sync + 'static {
            let inner = Arc::clone(&self.0);
            move || match inner.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            }
        }
    }

    /// 固定 CPU/内存读数，用来验证注入点（Python 用 psutil，测试里替换）。
    #[derive(Debug)]
    struct FixedUsage(f64, f64);

    impl ProcessUsage for FixedUsage {
        fn usage(&self) -> (f64, f64) {
            (self.0, self.1)
        }
    }

    // -- 纯函数 ----------------------------------------------------------

    /// 数值全部来自 Python `core.metrics._percentile` 的实跑结果。
    #[test]
    fn percentile_matches_python() {
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[7.0], 0.95), 7.0);
        assert_eq!(percentile(&[1.0, 3.0], 0.5), 2.0);
        assert_eq!(percentile(&[10.0, 12.0, 14.0], 0.95), 13.799999999999999);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.95), 3.8499999999999996);
        let values: Vec<f64> = (0..256).map(|value| value as f64).collect();
        assert_eq!(percentile(&values, 0.5), 127.5);
        assert_eq!(percentile(&values, 0.95), 242.25);
    }

    /// Python `round(x, n)` 的取值（含 .5 边界与二进制误差）。
    #[test]
    fn round_digits_matches_python() {
        for (value, digits, want) in [
            (2.25, 1, 2.2),
            (2.35, 1, 2.4),
            (0.05, 1, 0.1),
            (0.15, 1, 0.1),
            (0.25, 1, 0.2),
            (0.125, 1, 0.1),
            (1.25, 1, 1.2),
            (1.35, 1, 1.4),
            (10.45, 1, 10.4),
            (286.25, 1, 286.2),
            (2.675, 2, 2.67),
            (1.005, 2, 1.0),
            (1234.56, 1, 1234.6),
        ] {
            assert_eq!(round_digits(value, digits), want, "{value} / {digits}");
        }
        assert!(round_digits(f64::NAN, 1).is_nan());
        assert_eq!(round_digits(f64::INFINITY, 1), f64::INFINITY);
        assert!(round_digits(-0.0, 1).is_sign_negative());
    }

    // -- pytest test_router.py::test_metrics_* ---------------------------

    /// 逐条对应 pytest `test_metrics_snapshot_keys_and_rates`。
    #[test]
    fn snapshot_keys_and_rates() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.record_raw(1000);
        metrics.record_send(500);
        metrics.record_drop(5);
        metrics.set_backlog(3);
        for value in [10.0, 20.0, 30.0, 40.0, 50.0] {
            metrics.record_latency(value);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.raw_hz, 1000.0);
        assert_eq!(snapshot.send_hz, 500.0);
        assert_eq!(snapshot.backlog, 3);
        assert_eq!(snapshot.rtt_ms, Some(50.0));
        assert_eq!(snapshot.p50_ms, Some(30.0));
        assert_eq!(snapshot.p95_ms, Some(48.0));
        assert_eq!(snapshot.drop_pct, 0.5);
        assert_eq!(snapshot.cpu_pct, 0.0);
        assert_eq!(snapshot.mem_mb, 0.0);

        clock.advance(1.5);
        let idle = metrics.snapshot();
        assert_eq!(idle.raw_hz, 0.0);
        assert_eq!(idle.send_hz, 0.0);
        // 窗口只裁剪事件速率，延迟样本只受 maxlen 限制。
        assert_eq!(idle.rtt_ms, Some(50.0));
    }

    /// 逐条对应 pytest `test_metrics_reset_clears_windows`。
    #[test]
    fn reset_clears_windows() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.record_raw(10);
        metrics.record_send(10);
        metrics.record_drop(2);
        metrics.record_latency(12.0);
        metrics.set_backlog(4);
        metrics.reset();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.raw_hz, 0.0);
        assert_eq!(snapshot.send_hz, 0.0);
        assert_eq!(snapshot.backlog, 0);
        assert_eq!(snapshot.drop_pct, 0.0);
        // 没有采样就不报延迟：报 0 会被界面当成“RTT 0ms”显示出来。
        assert_eq!(snapshot.rtt_ms, None);
        assert_eq!(snapshot.p50_ms, None);
        assert_eq!(snapshot.p95_ms, None);
    }

    /// 逐条对应 pytest `test_metrics_thread_safe`（4 × 500 = 2000 Hz）。
    #[test]
    fn metrics_are_thread_safe() {
        let clock = FakeClock::new(0.0);
        let metrics = Arc::new(Metrics::with_clock(1.0, clock.handle()).unwrap());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let metrics = Arc::clone(&metrics);
            handles.push(std::thread::spawn(move || {
                for _ in 0..500 {
                    metrics.record_raw(1);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(metrics.snapshot().raw_hz, 2000.0);
    }

    // -- 边界行为 --------------------------------------------------------

    /// 数值来自 Python 实跑：300 条延迟只保留最后 256 条（44..299）。
    #[test]
    fn latency_window_caps_at_256() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(100.0, clock.handle()).unwrap();
        for value in 0..300 {
            metrics.record_latency(value as f64);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.rtt_ms, Some(299.0));
        assert_eq!(snapshot.p50_ms, Some(171.5));
        assert_eq!(snapshot.p95_ms, Some(286.2));
    }

    /// Python 实跑：t=100 记 900 条，推进 1.5s 再记 3 条 → 窗口里只剩 3 条。
    #[test]
    fn rates_use_last_window_only() {
        let clock = FakeClock::new(100.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        for _ in 0..900 {
            metrics.record_raw(1);
        }
        assert_eq!(metrics.snapshot().raw_hz, 900.0);
        clock.advance(1.5);
        metrics.record_raw(3);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.raw_hz, 3.0);
        assert_eq!(snapshot.send_hz, 0.0);
    }

    /// 3 个 RawInput + 1 个丢弃 → 25%（Python 实跑 drop_pct = 25.0）。
    #[test]
    fn drop_pct_uses_dropped_over_attempted() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.record_raw(3);
        metrics.record_drop(1);
        assert_eq!(metrics.snapshot().drop_pct, 25.0);
    }

    #[test]
    fn window_must_be_positive() {
        let err = Metrics::new(0.0).unwrap_err();
        assert!(matches!(err, MetricsError::WindowNotPositive(0.0)));
        assert_eq!(err.to_string(), "window_s 必须为正: 0.0");
        let err = Metrics::new(-1.5).unwrap_err();
        assert_eq!(err.to_string(), "window_s 必须为正: -1.5");
        assert!(Metrics::new(1.0).is_ok());
    }

    #[test]
    fn zero_and_negative_counts_are_ignored() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.record_raw(0);
        metrics.record_raw(-3);
        metrics.record_send(0);
        metrics.record_send(-1);
        metrics.record_drop(0);
        metrics.record_drop(-2);
        metrics.record_latency(-1.0);
        metrics.set_backlog(-4);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.raw_hz, 0.0);
        assert_eq!(snapshot.send_hz, 0.0);
        assert_eq!(snapshot.drop_pct, 0.0);
        assert_eq!(snapshot.backlog, 0);
        assert_eq!(snapshot.rtt_ms, None);
        // 负延迟被忽略，0 延迟是合法样本。
        metrics.record_latency(0.0);
        assert_eq!(metrics.snapshot().rtt_ms, Some(0.0));
    }

    #[test]
    fn backlog_is_clamped_to_zero() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.set_backlog(7);
        assert_eq!(metrics.snapshot().backlog, 7);
        metrics.set_backlog(-7);
        assert_eq!(metrics.snapshot().backlog, 0);
    }

    // -- 序列化 / 注入 ---------------------------------------------------

    /// 键集合与顺序必须与 Python dict 一致；无延迟样本时三键不出现。
    #[test]
    fn snapshot_serializes_like_python_dict() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_clock(1.0, clock.handle()).unwrap();
        metrics.record_raw(1000);
        metrics.record_send(500);
        metrics.record_drop(5);
        metrics.set_backlog(3);
        for value in [10.0, 20.0, 30.0, 40.0, 50.0] {
            metrics.record_latency(value);
        }
        assert_eq!(
            metrics.snapshot().to_json().unwrap(),
            concat!(
                "{\"raw_hz\":1000.0,\"send_hz\":500.0,\"backlog\":3,",
                "\"drop_pct\":0.5,\"cpu_pct\":0.0,\"mem_mb\":0.0,",
                "\"rtt_ms\":50.0,\"p50_ms\":30.0,\"p95_ms\":48.0}"
            )
        );

        let value = metrics.snapshot().to_value().unwrap();
        let map = value.as_object().unwrap();
        assert_eq!(map.len(), 9);
        assert_eq!(map.get("rtt_ms"), Some(&serde_json::json!(50.0)));

        // 空闲（无延迟样本）时只有 6 个键，与 pytest test_router_metrics_callback 一致。
        let clock = FakeClock::new(0.0);
        let empty = Metrics::with_clock(1.0, clock.handle()).unwrap();
        let value = empty.snapshot().to_value().unwrap();
        let map = value.as_object().unwrap();
        assert_eq!(map.len(), 6);
        assert!(!map.contains_key("rtt_ms"));
        assert!(!map.contains_key("p50_ms"));
        assert!(!map.contains_key("p95_ms"));
        for key in ["raw_hz", "send_hz", "backlog", "drop_pct", "cpu_pct", "mem_mb"] {
            assert!(map.contains_key(key), "{key}");
        }
    }

    /// CPU/内存走注入点，并按 1 位小数取整（Python round(.., 1)）。
    #[test]
    fn process_usage_is_injectable() {
        let clock = FakeClock::new(0.0);
        let metrics = Metrics::with_parts(1.0, clock.handle(), FixedUsage(12.34, 1234.56)).unwrap();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.cpu_pct, 12.3);
        assert_eq!(snapshot.mem_mb, 1234.6);
    }

    #[test]
    fn bytes_are_converted_to_mib() {
        assert_eq!(bytes_to_mb(0), 0.0);
        assert_eq!(bytes_to_mb(1024 * 1024), 1.0);
        assert_eq!(bytes_to_mb(3 * 1024 * 1024 / 2), 1.5);
    }

    #[test]
    fn default_constructor_uses_one_second_window() {
        let metrics = Metrics::with_defaults();
        metrics.record_raw(2);
        metrics.record_send(1);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.raw_hz, 2.0);
        assert_eq!(snapshot.send_hz, 1.0);
        assert_eq!(snapshot.cpu_pct, 0.0);
        assert_eq!(snapshot.mem_mb, 0.0);
    }
}
