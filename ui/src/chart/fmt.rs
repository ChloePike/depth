//! Number, size and time formatting for the chart's labels.
use super::*;
pub(super) use charting::fmt::{countdown, fmt_big, fmt_fp, fmt_fp_signed};

/// Simple moving average of `vals` over `n` (None until n values are in).
/// 1.2K / 3.4M / 1.1B for wall sizes.
pub(crate) fn fmt_usd_short(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.0}") }
}




/// Order size with the decimals a size of that magnitude needs.
pub(super) fn fmt_qty_chart(q: f64) -> String { let d = if q > 0.0 { (3.0 - q.log10().floor()).clamp(0.0, 6.0) as usize } else { 2 }; fmt_dp(q, d) }


pub(super) fn fmt_time(ms: i64, date: bool) -> String {
    let (d, h, m) = local_parts(ms);
    if date || (h == 0 && m == 0) { d } else { format!("{h:02}:{m:02}") }
}

/// The widget's time labels in local time: a time (the date at midnight), the date, or date and time.
pub(super) fn axis_time(ms: i64, l: charting::TimeLabel) -> String {
    match l { charting::TimeLabel::Time => fmt_time(ms, false), charting::TimeLabel::Day => fmt_time(ms, true), charting::TimeLabel::Full => fmt_time_full(ms) }
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
