//! 进程 CPU / 内存采样（对应 Python `metrics.py` 里的 `psutil.Process()`）。
//!
//! Python 用 `cpu_percent(None)`：**第一次调用返回 0.0**，之后按"上次采样到现在"的
//! CPU 时间差分算百分比。这里保持同样语义（首帧 0.0），所以调用方要持续复用同一个
//! 实例；内存取 `WorkingSetSize`，除以 1 MiB 与 `psutil` 的 RSS 口径一致。

use std::sync::Mutex;
use std::time::Instant;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::logic::metrics::{bytes_to_mb, ProcessUsage};

const FILETIME_UNITS_PER_SEC: f64 = 10_000_000.0; // 100ns

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn cpu_time_100ns() -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    ok.ok()?;
    Some(filetime_to_u64(kernel) + filetime_to_u64(user))
}

fn working_set_bytes() -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            counters.cb,
        )
    };
    ok.ok()?;
    Some(counters.WorkingSetSize as u64)
}

/// 真正的本进程采样器；构造后一直复用（首帧 CPU 为 0.0，与 psutil 一致）。
#[derive(Debug)]
pub struct Win32ProcessUsage {
    last: Mutex<Option<(u64, Instant)>>,
}

impl Win32ProcessUsage {
    pub fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }
}

impl Default for Win32ProcessUsage {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessUsage for Win32ProcessUsage {
    fn usage(&self) -> (f64, f64) {
        let mem_mb = working_set_bytes().map(bytes_to_mb).unwrap_or(0.0);
        let Some(total) = cpu_time_100ns() else {
            return (0.0, mem_mb);
        };
        let now = Instant::now();
        let mut guard = match self.last.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let cpu_pct = match *guard {
            None => 0.0,
            Some((prev_total, prev_at)) => {
                let elapsed = now.duration_since(prev_at).as_secs_f64();
                if elapsed <= 0.0 || total < prev_total {
                    0.0
                } else {
                    (total - prev_total) as f64 / FILETIME_UNITS_PER_SEC / elapsed * 100.0
                }
            }
        };
        *guard = Some((total, now));
        (cpu_pct, mem_mb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_reports_zero_cpu_and_positive_memory() {
        let usage = Win32ProcessUsage::new();
        let (cpu, mem) = usage.usage();
        assert_eq!(cpu, 0.0, "首帧 CPU 必须为 0.0（与 psutil.cpu_percent(None) 一致）");
        assert!(mem > 0.0, "本进程常驻内存应当大于 0，实际 {mem}");
    }

    #[test]
    fn second_sample_is_finite_and_non_negative() {
        let usage = Win32ProcessUsage::new();
        let _ = usage.usage();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let (cpu, mem) = usage.usage();
        assert!(cpu.is_finite() && cpu >= 0.0, "CPU 百分比非法：{cpu}");
        assert!(mem > 0.0);
    }

    #[test]
    fn cpu_percentage_is_bounded_by_core_count() {
        // psutil.Process().cpu_percent(None) 是多核口径：满负荷可以到 100% × 核数，
        // 所以上界按核数算，而不是 100%。
        let cores = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1) as f64;
        let usage = Win32ProcessUsage::new();
        let _ = usage.usage();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (cpu, _) = usage.usage();
        assert!(cpu >= 0.0 && cpu.is_finite(), "CPU 百分比非法：{cpu}");
        assert!(cpu <= 100.0 * cores + 1.0, "CPU 超过核数上限：{cpu} > {}", 100.0 * cores);
    }
}
