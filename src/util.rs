//! Small formatting helpers. No allocation-heavy paths here.

use chrono::{Local, TimeZone};

/// Drive capacity, 1024-based like Windows Explorer reports it.
pub fn fmt_capacity(bytes: u64) -> String {
    let gib = bytes as f64 / 1_073_741_824.0;
    if gib >= 10.0 {
        format!("{:.0} GB", gib)
    } else {
        format!("{:.1} GB", gib)
    }
}

/// Resident memory, in the units a task manager would use.
pub fn fmt_mb(mb: f32) -> String {
    if !mb.is_finite() {
        return "--".to_string();
    }
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 100.0 {
        format!("{:.0} MB", mb)
    } else {
        format!("{:.1} MB", mb)
    }
}

pub fn fmt_num(v: f32, dec: usize) -> String {
    if !v.is_finite() {
        return "--".to_string();
    }
    format!("{:.*}", dec, v)
}

pub fn fmt_pct(v: f32, dec: usize) -> String {
    format!("{}%", fmt_num(v, dec))
}

/// Self-reported CPU is often a fraction of a percent; printing "0.00%" looks
/// like the number is broken rather than small.
pub fn fmt_small_pct(v: f32) -> String {
    if !v.is_finite() {
        return "--".to_string();
    }
    if v < 0.01 {
        "<0.01".to_string()
    } else {
        format!("{:.2}", v)
    }
}

/// Share of total machine capacity, task-manager style. Small values keep an
/// extra digit so a 0.4% row does not read as "0.0".
pub fn fmt_cpu_share(v: f32) -> String {
    if !v.is_finite() {
        return "--".to_string();
    }
    if v < 1.0 {
        format!("{:.2}", v)
    } else {
        format!("{:.1}", v)
    }
}

pub fn fmt_rate(mb: f32) -> String {
    if !mb.is_finite() {
        return "--".to_string();
    }
    if mb >= 100.0 {
        format!("{:.0} MB/s", mb)
    } else if mb >= 1.0 {
        format!("{:.2} MB/s", mb)
    } else {
        format!("{:.2} MB/s", mb)
    }
}

pub fn fmt_change(cur: f32, prev: f32) -> String {
    if !prev.is_finite() || prev.abs() < 1e-3 {
        return "—".to_string();
    }
    let pct = (cur - prev) / prev * 100.0;
    if pct >= 0.0 {
        format!("+{:.1}%", pct)
    } else {
        format!("{:.1}%", pct)
    }
}

/// Change of a throughput reading, in MB/s.
///
/// Percent change is meaningless for a flow that idles near zero: 0.04 → 0.5
/// MB/s is "+1150%", which is arithmetically right and tells nobody anything.
/// A rate is quoted as the amount it moved by, the way a spread is.
pub fn fmt_delta_rate(prev: f32, cur: f32) -> String {
    if !prev.is_finite() || !cur.is_finite() {
        return "—".to_string();
    }
    let d = cur - prev;
    if d.abs() < 0.005 {
        return "0.00".to_string();
    }
    format!("{}{:.2}", if d > 0.0 { "+" } else { "" }, d)
}

/// Short axis / crosshair timestamp for the given timeframe index.
pub fn time_label(ts: i64, tf: usize) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(dt) => {
            if tf <= 2 {
                dt.format("%H:%M:%S").to_string()
            } else {
                dt.format("%H:%M").to_string()
            }
        }
        None => "--:--".to_string(),
    }
}

pub fn clock_now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

pub fn uptime(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Hard truncation with an ellipsis, measured in characters (not bytes).
pub fn shorten(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_change_is_an_absolute_delta_not_a_percentage() {
        // 0.04 -> 0.5 MB/s is +0.46 MB/s. The percentage form of the same move
        // is "+1150%": arithmetically right, useless on a chart, and what the
        // change column used to print for every NIC and disk.
        assert_eq!(fmt_delta_rate(0.04, 0.5), "+0.46");
        assert_eq!(fmt_delta_rate(12.0, 11.5), "-0.50");
        assert_eq!(fmt_delta_rate(3.0, 3.0), "0.00");
    }

    #[test]
    fn percentages_stay_percentages() {
        assert_eq!(fmt_change(50.0, 40.0), "+25.0%");
        // A near-zero baseline has no meaningful percentage; the dash says so
        // instead of printing a four-digit number.
        assert_eq!(fmt_change(0.5, 0.0), "—");
    }
}
