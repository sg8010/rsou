//! 界面小工具:本地时间格式化(不引 chrono)。

/// Unix 毫秒 → `YYYY-MM-DD HH:MM`。
/// Linux 走 libc::localtime_r(本地时区);非 Linux 平台(Windows)按固定
/// UTC+8 显示——目标用户都在国内,不接 WinAPI 时区检测。
pub fn format_local_time(ms: i64) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(text) = linux_local_time(ms) {
            return text;
        }
    }
    civil_time(ms + 8 * 3600 * 1000)
}

#[cfg(target_os = "linux")]
fn linux_local_time(ms: i64) -> Option<String> {
    let secs = ms.div_euclid(1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ok.is_null() {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

/// civil-from-days 算法(Howard Hinnant)按 UTC 展开毫秒时间戳。
fn civil_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let secs_of_day = ms.rem_euclid(86_400_000) / 1000;
    let hour = secs_of_day / 3600;
    let minute = secs_of_day % 3600 / 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_timestamp() {
        // 2025-01-01 00:00:00 UTC
        let ms = 1_735_689_600_000i64;
        // Linux 分支走本地时区,只校验形状;UTC 分支校验内容。
        let text = format_local_time(ms);
        assert_eq!(text.len(), 16);
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[13..14], ":");
    }

    #[test]
    fn civil_algorithm_matches_epoch() {
        assert_eq!(civil_time(0), "1970-01-01 00:00");
        assert_eq!(civil_time(1_735_689_600_000), "2025-01-01 00:00");
    }

    #[test]
    fn utc8_offset_shifts_known_timestamp() {
        // 非 Linux 分支:2025-01-01 00:00 UTC 应显示为 08:00。
        let text = format_local_time(1_735_689_600_000);
        if cfg!(target_os = "linux") {
            // Linux 走本地时区,只校验形状。
            assert_eq!(text.len(), 16);
        } else {
            assert_eq!(text, "2025-01-01 08:00");
        }
    }
}
