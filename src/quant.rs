//! Signals and statistical models over the aggregated 1m series. Pure functions: the engine
//! feeds them bars and book/trade snapshots, the UI shows the results.
//!
//! Every threshold is relative to the instrument's own recent history (robust z-scores:
//! median / MAD), so the same rules work for BTC and for a small coin.

use crate::agg::Bar;

/// One detected event.
#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub ts: i64,
    /// stable id of the rule, also the cooldown key
    pub kind: &'static str,
    /// 1 notable, 2 strong, 3 extreme
    pub severity: u8,
    /// +1 bullish pressure, -1 bearish, 0 neutral / informational
    pub dir: i8,
    pub title: String,
    pub detail: String,
}

/// (median, robust sigma = 1.4826 * MAD); None with fewer than 10 finite samples.
pub fn robust(xs: &[f64]) -> Option<(f64, f64)> {
    let mut v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if v.len() < 10 { return None; }
    v.sort_by(f64::total_cmp);
    let med = v[v.len() / 2];
    let mut dev: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(f64::total_cmp);
    Some((med, 1.4826 * dev[dev.len() / 2]))
}

/// Robust z of `x` against `sample`; the sigma is floored at 1% of |median| (or 1e-12) so a
/// flat history does not turn every tick into an extreme.
pub fn z(x: f64, sample: &[f64]) -> Option<f64> {
    let (m, s) = robust(sample)?;
    let floor = (m.abs() * 0.01).max(1e-12);
    Some((x - m) / s.max(floor))
}

fn sev(z: f64) -> u8 { if z.abs() >= 8.0 { 3 } else if z.abs() >= 5.0 { 2 } else { 1 } }

/// Bars whose minute is over (the last one is usually still filling).
pub fn complete(bars: &[Bar], now_ms: i64) -> &[Bar] {
    let n = bars.iter().rposition(|b| b.t + 60_000 <= now_ms).map_or(0, |i| i + 1);
    &bars[..n]
}

/// Volatility model from 1m closes: EWMA variance of log returns (60-minute half-life), scaled
/// to 1h and 24h, and where the current level sits among the last day's rolling 60-minute
/// realized vols (percentile 0..1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VolModel { pub sigma_1m: f64, pub sigma_1h: f64, pub sigma_24h: f64, pub percentile: f64 }

pub fn vol_model(bars: &[Bar]) -> Option<VolModel> {
    let r: Vec<f64> = bars.windows(2).filter(|w| w[0].c > 0.0 && w[1].c > 0.0).map(|w| (w[1].c / w[0].c).ln()).collect();
    if r.len() < 60 { return None; }
    let lam = 0.5f64.powf(1.0 / 60.0);
    // seed with the first hour's plain variance
    let mut var = r[..60].iter().map(|x| x * x).sum::<f64>() / 60.0;
    for x in &r[60..] { var = lam * var + (1.0 - lam) * x * x; }
    let s1 = var.sqrt();
    let rolling: Vec<f64> = r.windows(60).map(|w| (w.iter().map(|x| x * x).sum::<f64>() / 60.0).sqrt()).collect();
    let cur = rolling.last().copied().unwrap_or(s1);
    let pct = rolling.iter().filter(|v| **v <= cur).count() as f64 / rolling.len() as f64;
    Some(VolModel { sigma_1m: s1, sigma_1h: s1 * 60f64.sqrt(), sigma_24h: s1 * 1440f64.sqrt(), percentile: pct })
}

/// Short-term pressure in -100..100 from book depth imbalance (bid vs ask USD within a band)
/// and aggressor flow over the last bars. Describes the present, not a forecast.
pub fn pressure(bid_usd: f64, ask_usd: f64, bars: &[Bar], last_n: usize) -> f64 {
    let book = if bid_usd + ask_usd > 0.0 { (bid_usd - ask_usd) / (bid_usd + ask_usd) } else { 0.0 };
    let (b, s) = bars.iter().rev().take(last_n).fold((0.0, 0.0), |a, x| (a.0 + x.buy, a.1 + x.sell));
    let flow = if b + s > 0.0 { (b - s) / (b + s) } else { 0.0 };
    (50.0 * book + 50.0 * flow).clamp(-100.0, 100.0)
}

/// Inputs of one detection pass (aggregated series, complete bars only).
pub struct Input<'a> {
    pub perp: &'a [Bar],
    pub spot: &'a [Bar],
    /// latest trades: (ts, notional USD, buy aggressor)
    pub trades: &'a [(i64, f64, bool)],
    /// per venue: (name, mid deviation from the composite in bp)
    pub dislocation: &'a [(String, f64)],
    pub base: &'a str,
}

/// All rules over one snapshot. The caller applies cooldowns (per `kind`).
pub fn detect(i: &Input, now_ms: i64) -> Vec<Signal> {
    let mut out = vec![];
    let mut push = |kind: &'static str, severity: u8, dir: i8, title: String, detail: String| out.push(Signal { ts: now_ms, kind, severity, dir, title, detail });
    let px = |b: &[Bar]| b.last().map_or(0.0, |x| x.c);

    // 1. volume spike: last minute vs the two hours before it
    let p = i.perp;
    if p.len() > 30 {
        let (last, hist) = (p[p.len() - 1].vol, &p[p.len().saturating_sub(121)..p.len() - 1]);
        if let Some(zv) = z(last, &hist.iter().map(|b| b.vol).collect::<Vec<_>>()) {
            if zv >= 5.0 {
                let b = &p[p.len() - 1];
                let dir = if b.buy > b.sell * 1.2 { 1 } else if b.sell > b.buy * 1.2 { -1 } else { 0 };
                let usd = last * b.c;
                push("vol_spike", sev(zv), dir, format!("Volume spike {:.0}x", zv.max(1.0)),
                    format!("{} ${:.0}k traded in 1m ({:.0}% buy)", i.base, usd / 1e3, if b.buy + b.sell > 0.0 { b.buy / (b.buy + b.sell) * 100.0 } else { 50.0 }));
            }
        }
    }
    // 2. liquidation cascade: last 3 minutes vs up to 4 hours before. History has no liquidations
    // (all-zero = missing), so the sample starts at the first bar with one: live data only.
    // Sigma is floored at the sample mean: a mostly-zero sample has MAD 0.
    let live = p.iter().position(|b| b.liq_long + b.liq_short > 0.0).unwrap_or(p.len());
    if p.len() >= live + 63 {
        let n = p.len();
        let recent: (f64, f64) = p[n - 3..].iter().fold((0.0, 0.0), |a, b| (a.0 + b.liq_long * b.c, a.1 + b.liq_short * b.c));
        let hist: Vec<f64> = p[n.saturating_sub(243).max(live)..n - 3].windows(3).map(|w| w.iter().map(|b| (b.liq_long + b.liq_short) * b.c).sum()).collect();
        let tot = recent.0 + recent.1;
        let mean = hist.iter().sum::<f64>() / hist.len().max(1) as f64;
        if let Some((m, sd)) = robust(&hist) {
            let zl = (tot - m) / sd.max(mean).max(1e-12);
            if zl >= 6.0 && tot > 0.0 {
                let longs = recent.0 >= recent.1;
                push("liq_cascade", sev(zl), if longs { -1 } else { 1 },
                    format!("{} liquidation cascade", if longs { "Long" } else { "Short" }),
                    format!("${:.0}k liquidated in 3m (longs ${:.0}k / shorts ${:.0}k)", tot / 1e3, recent.0 / 1e3, recent.1 / 1e3));
            }
        }
    }
    // 3. open interest vs price over 15 minutes, against the distribution of 15m changes
    if p.len() > 120 && p.iter().rev().take(16).all(|b| b.oi.is_some()) {
        let ch = |k: usize, f: &dyn Fn(&Bar) -> f64| -> Vec<f64> { (k..p.len()).step_by(5).filter_map(|j| { let (a, b) = (f(&p[j - k]), f(&p[j])); (a > 0.0).then(|| b / a - 1.0) }).collect() };
        let oi = |b: &Bar| b.oi.unwrap_or(0.0);
        let (n, k) = (p.len(), 15);
        let (doi, dp) = (oi(&p[n - 1]) / oi(&p[n - 1 - k]).max(1e-12) - 1.0, p[n - 1].c / p[n - 1 - k].c.max(1e-12) - 1.0);
        if let (Some(zo), Some(zp)) = (z(doi, &ch(k, &oi)), z(dp, &ch(k, &|b: &Bar| b.c))) {
            if zo.abs() >= 3.0 && zp.abs() >= 1.5 {
                let (title, dir) = match (doi > 0.0, dp > 0.0) {
                    (true, true) => ("New longs: OI and price up", 1),
                    (true, false) => ("New shorts: OI up, price down", -1),
                    (false, true) => ("Short covering: OI down, price up", 1),
                    (false, false) => ("Long unwinding: OI and price down", -1),
                };
                push("oi_price", sev(zo), dir, title.into(), format!("15m: OI {:+.2}%, price {:+.2}%", doi * 100.0, dp * 100.0));
            }
        }
    }
    // 4. crowded funding: OI-weighted predicted rate, annualized
    if let Some(f) = p.last().and_then(|b| b.funding_h) {
        let apr = f * 24.0 * 365.0 * 100.0;
        if apr.abs() >= 30.0 {
            push("funding", if apr.abs() >= 100.0 { 3 } else if apr.abs() >= 60.0 { 2 } else { 1 }, if apr > 0.0 { -1 } else { 1 },
                format!("Crowded {}: funding {:+.0}% APR", if apr > 0.0 { "longs" } else { "shorts" }, apr),
                format!("{:+.4}%/h across venues; the paying side tends to get squeezed", f * 100.0));
        }
    }
    // 5. spot vs perp CVD over 30 minutes (normalized by volume)
    if p.len() > 30 && i.spot.len() > 30 {
        let flow = |b: &[Bar]| { let w = &b[b.len() - 30..]; let (x, v) = w.iter().fold((0.0, 0.0), |a, b| (a.0 + b.buy - b.sell, a.1 + b.buy + b.sell)); if v > 0.0 { x / v } else { 0.0 } };
        let (fp, fs) = (flow(p), flow(i.spot));
        if fp.signum() != fs.signum() && fp.abs() >= 0.12 && fs.abs() >= 0.12 {
            let perp_buys = fp > 0.0;
            push("cvd_div", if fp.abs().min(fs.abs()) >= 0.25 { 2 } else { 1 }, if perp_buys { -1 } else { 1 },
                format!("Perp {} vs spot {}", if perp_buys { "buying" } else { "selling" }, if perp_buys { "selling" } else { "buying" }),
                format!("30m net flow: perp {:+.0}%, spot {:+.0}% of volume. Spot-led moves tend to hold better.", fp * 100.0, fs * 100.0));
        }
    }
    // 6. basis: perp premium vs its own day
    if let Some(bb) = p.last().and_then(|b| b.basis_bps) {
        let hist: Vec<f64> = p.iter().rev().skip(1).take(1000).filter_map(|b| b.basis_bps).collect();
        if let Some(zb) = z(bb, &hist) {
            if zb.abs() >= 4.0 {
                push("basis", sev(zb), if zb > 0.0 { -1 } else { 1 }, format!("Basis {} {:+.1} bp", if zb > 0.0 { "stretched" } else { "collapsed" }, bb),
                    format!("Perp vs spot, {zb:+.1} sigma from its day"));
            }
        }
    }
    // 7. whale prints: trades in the top 0.1% of the recent tape, in the last 10 s
    if i.trades.len() >= 200 {
        let mut sz: Vec<f64> = i.trades.iter().map(|t| t.1).collect();
        sz.sort_by(f64::total_cmp);
        // the tape only holds a few hundred prints, so its own percentile is not enough: a whale also
        // has to be at least a tenth of a typical minute's turnover
        let minute_usd = { let w = &p[p.len().saturating_sub(60)..]; if w.is_empty() { 0.0 } else { w.iter().map(|b| b.vol * b.c).sum::<f64>() / w.len() as f64 } };
        let cut = sz[(sz.len() as f64 * 0.999) as usize - 1].max(sz[sz.len() / 2] * 20.0).max(minute_usd * 0.1);
        let big: Vec<&(i64, f64, bool)> = i.trades.iter().filter(|t| t.1 >= cut && now_ms - t.0 <= 10_000).collect();
        if let Some(t) = big.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            push("whale", if t.1 >= cut * 3.0 { 2 } else { 1 }, if t.2 { 1 } else { -1 },
                format!("Whale {} ${:.0}k", if t.2 { "buy" } else { "sell" }, t.1 / 1e3), format!("{} market order at {:.6}", i.base, px(p)));
        }
    }
    // 8. a venue away from its usual premium to the composite (deviation in bp, pre-filtered on its own noise)
    for (v, d) in i.dislocation {
        if d.abs() >= 10.0 {
            push("dislocation", if d.abs() >= 30.0 { 2 } else { 1 }, 0, format!("{v} {d:+.0} bp off its usual premium"),
                format!("{v} trades {} the composite than it normally does: a venue leading the move, a stale feed or an opening for arbitrage (see Venues)", if *d > 0.0 { "further above" } else { "further below" }));
        }
    }
    out
}

/// Keeps the recent signals and drops repeats of a kind inside its cooldown.
#[derive(Default)]
pub struct Feed { pub items: std::collections::VecDeque<Signal>, last: std::collections::HashMap<&'static str, (i64, u8)> }

impl Feed {
    /// Adds `s` unless the same kind fired within `cooldown_ms` at the same or higher severity.
    pub fn offer(&mut self, s: Signal, cooldown_ms: i64) -> bool {
        if let Some(&(t, sv)) = self.last.get(s.kind) { if s.ts - t < cooldown_ms && s.severity <= sv { return false; } }
        self.last.insert(s.kind, (s.ts, s.severity));
        self.items.push_front(s);
        self.items.truncate(200);
        true
    }
}

/// One venue's top of book for `best_cross`: prices in the venue's own quote, taker fee as a fraction.
pub struct Top<'a> { pub ex: &'a str, pub quote: &'a str, pub bid: f64, pub ask: f64, pub taker: f64 }

/// The best buy-here-sell-there pair among venues with the same quote currency (USD vs USDT
/// needs an FX leg, so it is never offered as arbitrage): (buy index, sell index, gross bp, net
/// of both taker fees bp). Returned even when net is negative, so the UI can show how close it is.
pub fn best_cross(v: &[Top]) -> Option<(usize, usize, f64, f64)> {
    let mut best: Option<(usize, usize, f64, f64)> = None;
    for (i, b) in v.iter().enumerate() {
        for (j, s) in v.iter().enumerate() {
            if i == j || b.quote != s.quote || !(b.ask > 0.0 && s.bid > 0.0) { continue; }
            let gross = (s.bid / b.ask - 1.0) * 1e4;
            let net = gross - (b.taker + s.taker) * 1e4;
            if best.is_none_or(|x| net > x.3) { best = Some((i, j, gross, net)); }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_same_quote_net_of_fees() {
        let t = |ex, quote, bid, ask| Top { ex, quote, bid, ask, taker: 0.0005 };
        // Kraken (USD) bids far above but is another currency; Bybit bids 20 bp over OKX's ask
        let v = [t("Okx", "USDT", 99.9, 100.0), t("Bybit", "USDT", 100.2, 100.3), t("Kraken", "USD", 101.0, 101.1)];
        let (i, j, gross, net) = best_cross(&v).unwrap();
        assert_eq!((v[i].ex, v[j].ex), ("Okx", "Bybit"));
        assert!((gross - 20.0).abs() < 1e-6 && (net - 10.0).abs() < 1e-6, "{gross} {net}");
        assert!(best_cross(&v[2..]).is_none());
    }

    fn bar(t: i64, c: f64, vol: f64) -> Bar { Bar { t: t * 60_000, o: c, h: c, l: c, c, vol, buy: vol / 2.0, sell: vol / 2.0, ..Default::default() } }

    #[test]
    fn robust_z_ignores_outliers_in_the_sample() {
        let mut s: Vec<f64> = (0..50).map(|i| 10.0 + (i % 5) as f64).collect();
        s.push(1e6);
        let zz = z(30.0, &s).unwrap();
        assert!(zz > 5.0 && zz < 30.0, "{zz}");
        assert!(z(1.0, &[1.0; 3]).is_none());
        // flat history: floor keeps z finite
        assert!(z(10.5, &[10.0; 20]).unwrap().is_finite());
    }

    #[test]
    fn volume_spike_and_cooldown() {
        let mut p: Vec<Bar> = (0..150).map(|i| bar(i, 100.0, 10.0 + (i % 3) as f64)).collect();
        let mut last = bar(150, 100.0, 200.0);
        last.buy = 180.0; last.sell = 20.0;
        p.push(last);
        let inp = Input { perp: &p, spot: &[], trades: &[], dislocation: &[], base: "BTC" };
        let s = detect(&inp, 151 * 60_000);
        let v = s.iter().find(|x| x.kind == "vol_spike").expect("spike");
        assert_eq!(v.dir, 1);
        let mut f = Feed::default();
        let mut first = v.clone(); first.severity = 1;
        assert!(f.offer(first.clone(), 600_000));
        let mut again = first.clone(); again.ts += 60_000;
        assert!(!f.offer(again.clone(), 600_000));
        again.severity = 3;
        assert!(f.offer(again, 600_000), "a stronger repeat breaks the cooldown");
        // quiet tape: nothing
        let quiet: Vec<Bar> = (0..150).map(|i| bar(i, 100.0, 10.0 + (i % 3) as f64)).collect();
        assert!(detect(&Input { perp: &quiet, spot: &[], trades: &[], dislocation: &[], base: "BTC" }, 0).iter().all(|x| x.kind != "vol_spike"));
    }

    #[test]
    fn liq_cascade_needs_live_sample() {
        let cascade = |p: &[Bar]| detect(&Input { perp: p, spot: &[], trades: &[], dislocation: &[], base: "BTC" }, 0).into_iter().any(|x| x.kind == "liq_cascade");
        // history only (zero liquidations), then one small liquidation: not a cascade
        let mut p: Vec<Bar> = (0..300).map(|i| bar(i, 100.0, 10.0)).collect();
        p.last_mut().unwrap().liq_long = 0.01;
        assert!(!cascade(&p));
        // live hour with sparse liquidations of ~1, then 50 in 3 minutes: cascade
        let mut p: Vec<Bar> = (0..300).map(|i| { let mut b = bar(i, 100.0, 10.0); if i >= 100 && i % 7 == 0 { b.liq_long = 1.0; } b }).collect();
        assert!(!cascade(&p));
        for b in &mut p[297..] { b.liq_long = 20.0; }
        assert!(cascade(&p));
    }

    #[test]
    fn vol_model_scales_and_ranks() {
        // alternating +-0.1% returns: sigma_1m ~ 0.001
        let p: Vec<Bar> = (0..400).map(|i| bar(i, if i % 2 == 0 { 100.0 } else { 100.1 }, 1.0)).collect();
        let m = vol_model(&p).unwrap();
        assert!((m.sigma_1m - 0.001).abs() < 0.0002, "{m:?}");
        assert!((m.sigma_1h / m.sigma_1m - 60f64.sqrt()).abs() < 1e-9);
        assert!(m.percentile > 0.0 && m.percentile <= 1.0);
        assert!(vol_model(&p[..20]).is_none());
    }

    #[test]
    fn pressure_combines_book_and_flow() {
        let mut b = bar(0, 100.0, 10.0);
        b.buy = 10.0; b.sell = 0.0;
        assert_eq!(pressure(1.0, 1.0, &[b.clone()], 5), 50.0);
        assert_eq!(pressure(3.0, 1.0, &[b], 5), 75.0);
        assert_eq!(pressure(0.0, 0.0, &[], 5), 0.0);
    }

    #[test]
    fn complete_drops_the_open_minute() {
        let p: Vec<Bar> = (0..5).map(|i| bar(i, 1.0, 1.0)).collect();
        assert_eq!(complete(&p, 4 * 60_000 + 30_000).len(), 4);
        assert_eq!(complete(&p, 5 * 60_000).len(), 5);
    }
}
