//! Number, size and time formatting for the chart's labels.
use super::*;

/// Simple moving average of `vals` over `n` (None until n values are in).
/// 1.2K / 3.4M / 1.1B for wall sizes.
pub(crate) fn fmt_usd_short(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.0}") }
}

/// Time left until a bar closes: "mm:ss", or "h:mm:ss" from an hour on.
pub(super) fn countdown(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

pub(super) fn fmt_big(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.2}") }
}

/// Footprint cell volume: 0.42 / 3.1 / 27 / 1.2K.
pub(super) fn fmt_fp(v: f64) -> String {
    let a = v.abs();
    if a == 0.0 { "0".into() } else if a >= 1e6 { format!("{:.1}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else if a >= 100.0 { format!("{v:.0}") } else if a >= 10.0 { format!("{v:.1}") } else { format!("{v:.2}") }
}
pub(super) fn fmt_fp_signed(v: f64) -> String { format!("{}{}", if v >= 0.0 { "+" } else { "" }, fmt_fp(v)) }

/// Order size with the decimals a size of that magnitude needs.
pub(super) fn fmt_qty_chart(q: f64) -> String { let d = if q > 0.0 { (3.0 - q.log10().floor()).clamp(0.0, 6.0) as usize } else { 2 }; fmt_dp(q, d) }

/// "3d 4h", "2h 15m", "45m" for a duration in ms.
pub(super) fn span(ms: i64) -> String {
    let m = ms / 60_000;
    let (d, h, mm) = (m / 1440, m / 60 % 24, m % 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {mm}m") } else { format!("{mm}m") }
}

pub(super) fn fmt_time(ms: i64, date: bool) -> String {
    let (d, h, m) = local_parts(ms);
    if date || (h == 0 && m == 0) { d } else { format!("{h:02}:{m:02}") }
}

pub fn local_date(ms: i64) -> String { local_parts(ms).0 }

pub(super) fn fmt_time_full(ms: i64) -> String {
    let (d, h, m) = local_parts(ms);
    format!("{d} {h:02}:{m:02}")
}

/// (M/D, hour, minute) in local time
fn local_parts(ms: i64) -> (String, i64, i64) {
    let off = super::local_offset_ms();
    let t = ms + off;
    let days = t.div_euclid(86_400_000);
    let rem = t.rem_euclid(86_400_000);
    // civil from days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (format!("{m}/{d}"), rem / 3_600_000, rem % 3_600_000 / 60_000)
}
