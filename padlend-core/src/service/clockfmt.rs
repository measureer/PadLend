//! 时间戳格式化（只用标准库）。
//!
//! Python 版用本地时间（`time.strftime` / `datetime.now()`）。Rust 侧没有 chrono，
//! `windows` crate 也没启用 `GetLocalTime` 需要的 feature，所以统一输出 **UTC**；
//! 这是与 Python 的已知偏差，字段格式逐字保持（"HH:MM:SS.mmm" 与 ISO 秒级 +00:00）。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 UTC 时间的日历分量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcStamp {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub millis: i64,
}

fn now_epoch() -> (i64, i64) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, i64::from(duration.subsec_millis())),
        Err(_) => (0, 0),
    }
}

/// 1970-01-01 起的天数 → 公历 (year, month, day)（Howard Hinnant 算法）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

impl UtcStamp {
    /// 当前时刻。
    pub fn now() -> Self {
        let (seconds, millis) = now_epoch();
        let days = seconds.div_euclid(86_400);
        let rem = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: rem / 3_600,
            minute: (rem % 3_600) / 60,
            second: rem % 60,
            millis,
        }
    }

    /// 日志条目的 `timestamp`：`"HH:MM:SS.mmm"`。
    pub fn time_of_day(&self) -> String {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            self.hour, self.minute, self.second, self.millis
        )
    }

    /// 诊断报告 / 设备注册表的秒级 ISO 时间（Python 的 `isoformat(timespec="seconds")`）。
    pub fn iso_seconds(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// 文件名用的紧凑时间（`padlend-diagnostics-%Y%m%d-%H%M%S`）。
    pub fn compact(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// 现成的 `timestamp` 字符串（日志条目最常用）。
pub fn time_of_day() -> String {
    UtcStamp::now().time_of_day()
}

/// 现成的 ISO 秒级时间（设备注册表 / 诊断报告用）。
pub fn iso_seconds() -> String {
    UtcStamp::now().iso_seconds()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // 2026-11-14 是任务环境给定的一天。
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
    }

    #[test]
    fn time_of_day_has_millisecond_precision() {
        let text = time_of_day();
        assert_eq!(text.len(), 12, "HH:MM:SS.mmm");
        assert_eq!(text.as_bytes()[2], b':');
        assert_eq!(text.as_bytes()[8], b'.');
    }

    #[test]
    fn iso_seconds_uses_utc_suffix() {
        let text = iso_seconds();
        assert!(text.ends_with("+00:00"), "{text}");
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], "T");
    }
}
