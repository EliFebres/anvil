//! How the dashboard words times, sizes and money.

use chrono::{DateTime, FixedOffset, Utc};

/// A wall-clock time such as `3:49 PM`.
pub fn clock(at: DateTime<Utc>, tz: FixedOffset) -> String {
    at.with_timezone(&tz).format("%-I:%M %p").to_string()
}

/// When something happened: the time today, `yesterday`, else the date.
pub fn when(at: DateTime<Utc>, now: DateTime<Utc>, tz: FixedOffset) -> String {
    let day = at.with_timezone(&tz).date_naive();
    let today = now.with_timezone(&tz).date_naive();
    if day == today {
        clock(at, tz)
    } else if today.pred_opt() == Some(day) {
        "yesterday".to_owned()
    } else {
        at.with_timezone(&tz).format("%b %-d").to_string()
    }
}

/// An uptime such as `3d 04:12` or `04:12` (hours and minutes).
pub fn uptime(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}

/// A countdown such as `2:14`; `now` once it has run out.
pub fn countdown(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_owned();
    }
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A live elapsed time such as `22:41` or `1:02:03`.
pub fn running_clock(secs: i64) -> String {
    anvil_tui::layout::elapsed(u64::try_from(secs.max(0)).unwrap_or(0))
}

/// A finished stage's length: `2s`, `4m12s` or `1h05m`.
pub fn stage_length(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A size such as `1.4 GB`, `61 GB` or `512 MB`.
pub fn bytes(n: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let n = n as f64;
    if n >= 10.0 * GB {
        format!("{:.0} GB", n / GB)
    } else if n >= GB {
        format!("{:.1} GB", n / GB)
    } else {
        format!("{:.0} MB", n / MB)
    }
}

/// Dollars such as `$4.71`.
pub fn money(usd: f64) -> String {
    format!("${usd:.2}")
}

/// An age such as `2d old`, `5h old` or `12m old`.
pub fn age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 86_400 {
        format!("{}d old", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h old", secs / 3600)
    } else {
        format!("{}m old", secs / 60)
    }
}

/// A latency such as `640ms` or `1.1s`.
pub fn latency(millis: u64) -> String {
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", millis as f64 / 1000.0)
    }
}

/// The first eight characters of a ticket id: enough to tell tickets apart.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The part of a rejection reason before its first colon, such as `gate C`.
pub fn short_reason(detail: &str) -> &str {
    detail.split(':').next().unwrap_or(detail).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).expect("utc")
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("rfc3339")
    }

    #[test]
    fn times() {
        let now = at("2026-11-02T15:49:00Z");
        assert_eq!(clock(now, utc()), "3:49 PM");
        assert_eq!(when(at("2026-11-02T10:05:00Z"), now, utc()), "10:05 AM");
        assert_eq!(when(at("2026-11-01T10:05:00Z"), now, utc()), "yesterday");
        assert_eq!(when(at("2026-10-31T10:05:00Z"), now, utc()), "Oct 31");
        assert_eq!(uptime(3 * 86_400 + 4 * 3600 + 12 * 60 + 5), "3d 04:12");
        assert_eq!(uptime(59 * 60), "00:59");
        assert_eq!(countdown(134), "2:14");
        assert_eq!(countdown(-3), "now");
        assert_eq!(running_clock(22 * 60 + 41), "22:41");
        assert_eq!(stage_length(2), "2s");
        assert_eq!(stage_length(252), "4m12s");
        assert_eq!(stage_length(3900), "1h05m");
        assert_eq!(age(2 * 86_400 + 5), "2d old");
    }

    #[test]
    fn sizes_money_and_ids() {
        assert_eq!(bytes(61 * 1024 * 1024 * 1024), "61 GB");
        assert_eq!(bytes(1_503_238_553), "1.4 GB");
        assert_eq!(bytes(512 * 1024 * 1024), "512 MB");
        assert_eq!(money(4.706), "$4.71");
        assert_eq!(latency(640), "640ms");
        assert_eq!(latency(1100), "1.1s");
        assert_eq!(short_id("01M6D20KVA7TZJNN66ZFVCRWVD"), "01M6D20K");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_reason("gate C: two reviewers"), "gate C");
        assert_eq!(short_reason("plan budget"), "plan budget");
    }
}
