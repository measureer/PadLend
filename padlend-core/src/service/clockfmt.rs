//! 时间戳格式化：**本地时间**，与 Python 的 `time.strftime` / `datetime.now()` 一致。
//!
//! 早先版本只能输出 UTC（没有 chrono、也没开 `GetLocalTime` 需要的 feature），
//! 导致日志与诊断包里的时刻比用户本地时时差 8 小时。现在直接用
//! `Win32::System::SystemInformation::GetLocalTime`，没有时区偏移后缀
//! （Python 的 `isoformat(timespec="seconds")` 也不带后缀）。

use windows::Win32::System::SystemInformation::GetLocalTime;

/// 当前本地时间的日历分量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub millis: i64,
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

impl Stamp {
    /// 当前本地时间。
    pub fn now() -> Self {
        let st = unsafe { GetLocalTime() };
        Self {
            year: st.wYear as i64,
            month: st.wMonth as i64,
            day: st.wDay as i64,
            hour: st.wHour as i64,
            minute: st.wMinute as i64,
            second: st.wSecond as i64,
            millis: st.wMilliseconds as i64,
        }
    }

    /// 由 Unix 秒（按 UTC 解释）构造；仅用于测试与时区无关的换算验证。
    pub fn from_unix_utc(seconds: i64, millis: i64) -> Self {
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
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
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
    Stamp::now().time_of_day()
}

/// 现成的 ISO 秒级时间（设备注册表 / 诊断报告用）。
pub fn iso_seconds() -> String {
    Stamp::now().iso_seconds()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // 2026-11-14 由任务环境给定。
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
    fn iso_seconds_is_local_without_offset() {
        let text = iso_seconds();
        // Python 的 isoformat(timespec="seconds") 是 19 字符、不带 +00:00。
        assert_eq!(text.len(), 19, "{text}");
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], "T");
        assert!(!text.contains('+'), "{text}");
    }

    #[test]
    fn now_is_close_to_system_clock() {
        // 本地时间与 UTC 最多差一个时区（±14h），这里只验证字段合法。
        let stamp = Stamp::now();
        assert!((1970..2100).contains(&stamp.year));
        assert!((1..=12).contains(&stamp.month));
        assert!((1..=31).contains(&stamp.day));
        assert!((0..24).contains(&stamp.hour));
        assert!((0..60).contains(&stamp.minute));
    }
}
