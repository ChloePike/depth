//! Client-side execution algorithms built from plain venue orders: scaled (ladder) orders and
//! TWAP slices. Pure planning here; the app sends and tracks the orders.

/// `n` limit orders from `from` to `to` (inclusive) splitting `total`. `skew` in -1..1 tilts the
/// size: 0 = equal, +1 = the last order (at `to`) twice the average, the first near zero.
/// Prices are rounded to `tick`, sizes floored to `step`; levels that round to zero are dropped.
pub fn scaled(total: f64, from: f64, to: f64, n: usize, skew: f64, tick: f64, step: f64) -> Vec<(f64, f64)> {
    if n == 0 || !(total > 0.0) || !(from > 0.0) || !(to > 0.0) { return vec![]; }
    let skew = skew.clamp(-1.0, 1.0);
    let w: Vec<f64> = (0..n).map(|i| {
        let t = if n == 1 { 0.5 } else { i as f64 / (n - 1) as f64 };
        (1.0 + skew * (2.0 * t - 1.0)).max(0.0)
    }).collect();
    let sum: f64 = w.iter().sum();
    let round = |v: f64, s: f64, floor: bool| if s > 0.0 { if floor { (v / s + 1e-9).floor() * s } else { (v / s).round() * s } } else { v };
    (0..n).filter_map(|i| {
        let px = if n == 1 { from } else { from + (to - from) * i as f64 / (n - 1) as f64 };
        let q = round(total * w[i] / sum, step, true);
        (q > 0.0).then(|| (round(px, tick, false), q))
    }).collect()
}

/// Sizes of `slices` TWAP children summing to `total` (floored to `step`; the remainder goes
/// to the last slice so nothing is lost to rounding).
pub fn twap_slices(total: f64, slices: usize, step: f64) -> Vec<f64> {
    if slices == 0 || !(total > 0.0) { return vec![]; }
    let floor = |v: f64| if step > 0.0 { (v / step + 1e-9).floor() * step } else { v };
    let each = floor(total / slices as f64);
    let mut v = vec![each; slices];
    let rest = total - each * (slices - 1) as f64;
    v[slices - 1] = floor(rest + 1e-12);
    v.retain(|q| *q > 0.0);
    v
}

/// Limit price of one TWAP child: the far side of the venue's own book (ask to buy, bid to sell)
/// widened by `max_slip_bps`, never beyond the user's `limit`. Sent as IOC, so it fills what is
/// there up to that price and never rests. None when the book is missing or the limit is already
/// out of reach (the slice waits).
pub fn twap_price(buy: bool, bid: f64, ask: f64, max_slip_bps: f64, limit: Option<f64>) -> Option<f64> {
    if !(bid > 0.0 && ask >= bid) { return None; }
    let k = max_slip_bps.max(0.0) / 1e4;
    let px = if buy { ask * (1.0 + k) } else { bid * (1.0 - k) };
    match limit {
        Some(l) if buy && ask > l => None,
        Some(l) if !buy && bid < l => None,
        Some(l) => Some(if buy { px.min(l) } else { px.max(l) }),
        None => Some(px),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_ladder() {
        let v = scaled(10.0, 100.0, 90.0, 5, 0.0, 0.1, 0.001);
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].0, 100.0);
        assert!((v[4].0 - 90.0).abs() < 1e-9);
        assert!(v.iter().all(|(_, q)| (q - 2.0).abs() < 1e-9));
        // skew +1: sizes grow toward `to`, still summing to (at most) the total
        let s = scaled(10.0, 100.0, 90.0, 5, 1.0, 0.1, 0.001);
        assert!(s.windows(2).all(|w| w[1].1 >= w[0].1));
        assert!(s.iter().map(|x| x.1).sum::<f64>() <= 10.0 + 1e-9);
        assert!(scaled(1.0, 100.0, 90.0, 0, 0.0, 0.1, 0.001).is_empty());
        assert_eq!(scaled(1.0, 100.0, 90.0, 1, 0.0, 0.1, 0.001), vec![(100.0, 1.0)]);
    }

    #[test]
    fn twap_sizes_and_prices() {
        let s = twap_slices(1.0, 3, 0.001);
        assert_eq!(s.len(), 3);
        assert!((s.iter().sum::<f64>() - 1.0).abs() < 1e-9, "{s:?}");
        assert!(twap_slices(1.0, 0, 0.001).is_empty());
        // buy: ask + 10 bp, capped by the limit; above the limit: wait
        assert!((twap_price(true, 99.0, 100.0, 10.0, None).unwrap() - 100.1).abs() < 1e-9);
        assert_eq!(twap_price(true, 99.0, 100.0, 10.0, Some(100.05)), Some(100.05));
        assert_eq!(twap_price(true, 99.0, 100.0, 10.0, Some(99.5)), None);
        assert!((twap_price(false, 99.0, 100.0, 10.0, None).unwrap() - 98.901).abs() < 1e-9);
        assert_eq!(twap_price(false, 0.0, 100.0, 10.0, None), None);
    }
}
