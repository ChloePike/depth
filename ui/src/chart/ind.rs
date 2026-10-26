//! Indicator math over closes: EMA, RSI, MACD, Bollinger bands, SMA.

/// Exponential moving average (seeded with the SMA of the first n values, as TradingView does).
pub(super) fn ema(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let k = 2.0 / (n as f64 + 1.0);
    let mut prev: Option<f64> = None;
    vals.iter().enumerate().map(|(i, v)| {
        prev = match prev {
            Some(p) => Some(p + k * (v - p)),
            None if i + 1 >= n => Some(vals[i + 1 - n..=i].iter().sum::<f64>() / n as f64),
            None => None,
        };
        prev
    }).collect()
}

/// RSI with Wilder smoothing, seeded with the plain average of the first n changes (TradingView's ta.rsi).
pub(super) fn rsi(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let (mut up, mut dn) = (0.0, 0.0);
    let k = n as f64;
    (0..vals.len()).map(|i| {
        if i == 0 { return None; }
        let d = vals[i] - vals[i - 1];
        let (u, w) = (d.max(0.0), (-d).max(0.0));
        if i <= n { up += u / k; dn += w / k; } else { up = (up * (k - 1.0) + u) / k; dn = (dn * (k - 1.0) + w) / k; }
        (i >= n).then(|| if dn == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + up / dn) })
    }).collect()
}

/// MACD line, signal and histogram (fast, slow, signal EMAs); the signal starts once the MACD has n values.
pub(super) fn macd(vals: &[f64], fast: usize, slow: usize, sig: usize) -> Vec<Option<(f64, f64, f64)>> {
    let (f, s) = (ema(vals, fast), ema(vals, slow));
    let line: Vec<Option<f64>> = f.iter().zip(&s).map(|(a, b)| Some((*a)? - (*b)?)).collect();
    let start = line.iter().position(Option::is_some).unwrap_or(vals.len());
    let defined: Vec<f64> = line[start..].iter().map(|v| v.unwrap_or(0.0)).collect();
    let sg = ema(&defined, sig);
    (0..vals.len()).map(|i| { let m = line[i]?; let g = sg[i.checked_sub(start)?]?; Some((m, g, m - g)) }).collect()
}

/// Bollinger bands: (middle SMA, half width = k population standard deviations).
pub(super) fn bollinger(vals: &[f64], n: usize, k: f64) -> Vec<Option<(f64, f64)>> {
    (0..vals.len()).map(|i| {
        let w = vals.get((i + 1).checked_sub(n)?..=i)?;
        let m = w.iter().sum::<f64>() / n as f64;
        Some((m, k * (w.iter().map(|v| (v - m).powi(2)).sum::<f64>() / n as f64).sqrt()))
    }).collect()
}

pub(super) fn sma(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(vals.len());
    let mut sum = 0.0;
    for (i, v) in vals.iter().enumerate() {
        sum += v;
        if i >= n { sum -= vals[i - n]; }
        out.push((i + 1 >= n).then_some(sum / n as f64));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::Tool;
    use super::super::fmt::countdown;

    #[test]
    fn ema_seeds_with_sma_then_smooths() {
        let e = ema(&[1.0, 2.0, 3.0, 4.0], 3);
        assert_eq!(e[..2], [None, None]);
        assert_eq!(e[2], Some(2.0));
        // k = 2 / (3 + 1) = 0.5
        assert_eq!(e[3], Some(3.0));
        assert_eq!(Tool::parse("fib").map(Tool::id), Some("fib"));
    }

    #[test]
    fn rsi_macd_bollinger() {
        let r = rsi(&[1.0, 2.0, 1.0, 2.0, 3.0], 2);
        assert_eq!(r[..2], [None, None]);
        assert_eq!(r[2], Some(50.0));
        // Wilder: up = (0.5 + 1) / 2, dn = 0.5 / 2
        assert_eq!(r[3], Some(75.0));
        assert_eq!(rsi(&[1.0, 2.0, 3.0], 2)[2], Some(100.0));
        let m = macd(&[5.0; 40], 12, 26, 9);
        assert!(m[32].is_none() && m[33] == Some((0.0, 0.0, 0.0)));
        let b = bollinger(&[1.0, 3.0, 1.0, 3.0], 2, 2.0);
        assert_eq!((b[0], b[1]), (None, Some((2.0, 2.0))));
    }

    #[test]
    fn sma_and_countdown() {
        assert_eq!(sma(&[1.0, 2.0, 3.0, 4.0], 2), vec![None, Some(1.5), Some(2.5), Some(3.5)]);
        assert_eq!(sma(&[5.0], 3), vec![None]);
        assert_eq!(countdown(65_000), "01:05");
        assert_eq!(countdown(3_725_000), "1:02:05");
        assert_eq!(countdown(-5), "00:00");
    }
}
