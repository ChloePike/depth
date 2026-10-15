//! Other traders' execution algos, read from public data. Two footprints:
//! - clips: a TWAP / slicer sends the same size at a steady cadence. Taker prints (the fills of one
//!   order merged, see `Agg::on`) of one venue and side are bucketed by size (3 significant
//!   digits); a bucket of at least MIN_CLIPS prints whose gaps mostly sit near their median gap is
//!   reported. Random flow at a common size has exponential gaps: only ~17% land within +-25% of
//!   the median, against REGULAR (60%) required here.
//! - icebergs: a resting level that keeps trading far more than it ever shows. Only fills that
//!   arrive while the level is in our book count (a level eaten before its fills arrive shows 0).
//! Heuristics: the UI says "likely" and shows the evidence (count, cadence, size).
//! ponytail: exact-size clips only; randomized slicers (size jitter) need a cadence-only pass.
use crate::{Exchange, Market};
use std::collections::{HashMap, VecDeque};

const WINDOW_MS: i64 = 30 * 60_000;
const MIN_CLIPS: usize = 6;
/// prints kept per venue, market and side (memory bound)
const MAX_PRINTS: usize = 20_000;
const REGULAR: f64 = 0.6;
/// shortest cadence reported: faster repeats are market-making / HFT, not a slicer
const MIN_GAP_MS: f64 = 2_000.0;
const ICE_TTL_MS: i64 = 10 * 60_000;
/// traded / largest shown size for an iceberg
const ICE_RATIO: f64 = 3.0;
const ICE_FILLS: u32 = 5;

#[derive(Clone, Debug, PartialEq)]
pub struct Clip { pub ex: Exchange, pub market: Market, pub buy: bool, pub qty: f64, pub px: f64, pub count: usize, pub period_ms: f64, pub first: i64, pub last: i64,
    /// every clip: (ts, price in the venue quote)
    pub pts: Vec<(i64, f64)> }

#[derive(Clone, Debug, PartialEq)]
pub struct Iceberg { pub ex: Exchange, pub market: Market, pub bid: bool, pub px: f64, pub traded: f64, pub shown_max: f64, pub fills: u32, pub first: i64, pub last: i64 }

#[derive(Default)]
struct Ice { px: f64, traded: f64, shown_max: f64, fills: u32, first: i64, last: i64 }

#[derive(Default)]
pub struct Scan {
    /// (venue, market, buy aggressor) -> (ts, qty, px), oldest first
    prints: HashMap<(Exchange, Market, bool), VecDeque<(i64, f64, f64)>>,
    /// (venue, market, resting bid, price key)
    ice: HashMap<(Exchange, Market, bool, i64), Ice>,
    /// last `significant` pass (engine, every 5 s): what is running now (book tags)
    pub found: (Vec<Clip>, Vec<Iceberg>),
    /// every detection of the last 30 min by key, latest state (chart marks stay after an algo stops)
    pub seen: (HashMap<String, Clip>, HashMap<String, Iceberg>),
}

const SEEN_MS: i64 = 30 * 60_000;

/// Stable key per algo: venue, market, side and size bucket (slicer) or price (iceberg).
pub fn clip_key(c: &Clip) -> String { format!("clip {:?} {:?} {} {:?}", c.ex, c.market, c.buy, size_key(c.qty)) }
pub fn ice_key(i: &Iceberg) -> String { format!("ice {:?} {:?} {} {}", i.ex, i.market, i.bid, px_key(i.px)) }

/// Size bucket: 3 significant digits (exact-size clips land in one bucket).
fn size_key(q: f64) -> (i32, i64) {
    let e = q.abs().log10().floor() as i32 - 2;
    (e, (q / 10f64.powi(e)).round() as i64)
}

fn px_key(px: f64) -> i64 { (px * 1e8).round() as i64 }

/// Plain number without float noise (0.0035000000000000005 -> 0.0035).
fn num(x: f64) -> String { format!("{}", (x * 1e8).round() / 1e8) }

impl Scan {
    /// Store a `significant` pass: running now in `found`, kept for the chart in `seen`.
    pub fn remember(&mut self, now: i64, found: (Vec<Clip>, Vec<Iceberg>)) {
        for c in &found.0 { self.seen.0.insert(clip_key(c), c.clone()); }
        for i in &found.1 { self.seen.1.insert(ice_key(i), i.clone()); }
        self.seen.0.retain(|_, c| c.last >= now - SEEN_MS);
        self.seen.1.retain(|_, i| i.last >= now - SEEN_MS);
        self.found = found;
    }

    /// One complete taker print (fills of one order merged).
    pub fn print(&mut self, ex: Exchange, market: Market, buy: bool, ts: i64, qty: f64, px: f64) {
        if !(qty > 0.0) { return; }
        let q = self.prints.entry((ex, market, buy)).or_default();
        q.push_back((ts, qty, px));
        // ponytail: a busy venue's window shrinks to its last MAX_PRINTS prints (~5 min of BTC on
        // Binance; a 30 s slicer needs 3); bucket per size on arrival if longer windows matter
        while q.front().is_some_and(|x| x.0 < ts - WINDOW_MS) || q.len() > MAX_PRINTS { q.pop_front(); }
    }

    /// One raw fill against a resting level; `shown` is that level's size in our book right now.
    pub fn fill(&mut self, ex: Exchange, market: Market, buy: bool, ts: i64, px: f64, qty: f64, shown: f64) {
        if !(shown > 0.0) { return; }
        // a buy aggressor lifts a resting ask
        let i = self.ice.entry((ex, market, !buy, px_key(px))).or_insert_with(|| Ice { px, first: ts, ..Default::default() });
        i.traded += qty;
        i.shown_max = i.shown_max.max(shown);
        i.fills += 1;
        i.last = ts;
        if self.ice.len() > 5_000 { self.ice.retain(|_, i| i.last >= ts - ICE_TTL_MS); }
    }

    /// Slicers still running at `now` (last clip within 3 periods).
    pub fn clips(&self, now: i64) -> Vec<Clip> {
        let mut out = vec![];
        for (&(ex, market, buy), q) in &self.prints {
            let mut groups: HashMap<(i32, i64), Vec<(i64, f64, f64)>> = HashMap::new();
            for &x in q { groups.entry(size_key(x.1)).or_default().push(x); }
            for g in groups.into_values().filter(|g| g.len() >= MIN_CLIPS) {
                if let Some(c) = cadence(&g) {
                    let last = g.last().unwrap();
                    if (now - last.0) as f64 > 3.0 * c { continue; }
                    out.push(Clip { ex, market, buy, qty: g[0].1, px: last.2, count: g.len(), period_ms: c, first: g[0].0, last: last.0, pts: g.iter().map(|x| (x.0, x.2)).collect() });
                }
            }
        }
        out
    }

    /// Icebergs whose level traded recently; `alive` says whether the level is still in the book.
    pub fn icebergs(&self, now: i64, alive: impl Fn(Exchange, Market, bool, f64) -> bool) -> Vec<Iceberg> {
        self.ice.iter().filter(|(_, i)| i.fills >= ICE_FILLS && i.traded >= ICE_RATIO * i.shown_max && now - i.last <= 120_000)
            .filter(|(k, i)| alive(k.0, k.1, k.2, i.px))
            .map(|(k, i)| Iceberg { ex: k.0, market: k.1, bid: k.2, px: i.px, traded: i.traded, shown_max: i.shown_max, fills: i.fills, first: i.first, last: i.last })
            .collect()
    }
}

/// Detections worth showing. Floors are relative to the instrument: a slicer must have traded 0.1%
/// of a typical cross-venue minute (`vol_1m_usd`, median) and $5k, an iceberg 0.5% and $10k. Live
/// BTC check: without floors dust repeats (0.00000024 BTC) and market makers re-quoting the touch
/// flood it. `usd` converts a venue's quote to USD; `alive` says whether a level is still in its book.
pub fn significant(scan: &Scan, now: i64, vol_1m_usd: f64, usd: impl Fn(Exchange, Market) -> f64,
                   alive: impl Fn(Exchange, Market, bool, f64) -> bool) -> (Vec<Clip>, Vec<Iceberg>) {
    let (clip_min, ice_min) = floors(vol_1m_usd);
    (scan.clips(now).into_iter().filter(|c| clip_usd(c, &usd) >= clip_min).collect(),
     scan.icebergs(now, alive).into_iter().filter(|i| ice_usd(i, &usd) >= ice_min).collect())
}

fn floors(vol_1m_usd: f64) -> (f64, f64) { ((vol_1m_usd * 0.001).max(5_000.0), (vol_1m_usd * 0.005).max(10_000.0)) }
fn clip_usd(c: &Clip, usd: impl Fn(Exchange, Market) -> f64) -> f64 { c.qty * c.px * c.count as f64 * usd(c.ex, c.market) }
fn ice_usd(i: &Iceberg, usd: impl Fn(Exchange, Market) -> f64) -> f64 { i.traded * i.px * usd(i.ex, i.market) }

/// `significant` detections as signals with a stable key per algo (the caller reports each key once).
pub fn signals(found: &(Vec<Clip>, Vec<Iceberg>), now: i64, vol_1m_usd: f64, usd: impl Fn(Exchange, Market) -> f64) -> Vec<(String, crate::quant::Signal)> {
    let (clip_min, ice_min) = floors(vol_1m_usd);
    let sev = |x: f64, min: f64| if x >= 25.0 * min { 3 } else if x >= 5.0 * min { 2 } else { 1 };
    let mut out = vec![];
    for c in &found.0 {
        let total = clip_usd(c, &usd);
        let key = clip_key(c);
        out.push((key, crate::quant::Signal { ts: now, kind: "algo_slicer", severity: sev(total, clip_min), dir: if c.buy { 1 } else { -1 },
            title: (if c.buy { "Likely slicer buying" } else { "Likely slicer selling" }).into(),
            detail: format!("{:?} {:?}: {} every {:.0}s, {} clips, ${:.0}k so far", c.ex, c.market, num(c.qty), c.period_ms / 1e3, c.count, total / 1e3) }));
    }
    for i in &found.1 {
        let traded = ice_usd(i, &usd);
        let key = ice_key(i);
        out.push((key, crate::quant::Signal { ts: now, kind: "algo_iceberg", severity: sev(traded, ice_min), dir: if i.bid { 1 } else { -1 },
            title: (if i.bid { "Likely iceberg bid" } else { "Likely iceberg ask" }).into(),
            detail: format!("{:?} {:?} @ {}: ${:.0}k traded, never more than {} shown ({} fills)", i.ex, i.market, num(i.px), traded / 1e3, num(i.shown_max), i.fills) }));
    }
    out
}

/// Median gap (ms) of prints that repeat at a steady cadence, else None.
fn cadence(g: &[(i64, f64, f64)]) -> Option<f64> {
    let mut gaps: Vec<f64> = g.windows(2).map(|w| (w[1].0 - w[0].0) as f64).collect();
    gaps.sort_by(f64::total_cmp);
    let med = gaps[gaps.len() / 2];
    if med < MIN_GAP_MS { return None; }
    let near = gaps.iter().filter(|x| (**x - med).abs() <= 0.25 * med).count();
    (near as f64 >= REGULAR * gaps.len() as f64).then_some(med)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_clips_found_random_flow_not() {
        let mut s = Scan::default();
        // a buyer slicing 0.5 every 30 s (a little jitter), amid random 0.5 sells
        for i in 0..10 { s.print(Exchange::Binance, Market::Perp, true, i * 30_000 + (i % 3) * 400, 0.5, 100.0); }
        let mut t = 0;
        for i in 0..40 { t += [1_000, 9_000, 3_000, 47_000, 15_000, 2_000, 70_000][i % 7]; s.print(Exchange::Binance, Market::Perp, false, t, 0.5, 100.0); }
        let c = s.clips(9 * 30_000 + 5_000);
        assert_eq!(c.len(), 1);
        assert!(c[0].buy && c[0].count == 10 && (c[0].period_ms - 30_000.0).abs() < 1_000.0);
        // stopped for more than 3 periods: no longer reported
        assert!(s.clips(9 * 30_000 + 100_000).is_empty());
    }

    #[test]
    fn detections_stay_for_the_chart() {
        let mut s = Scan::default();
        let ice = Iceberg { ex: Exchange::Gate, market: Market::Perp, bid: true, px: 99.0, traded: 9.0, shown_max: 1.0, fills: 6, first: 0, last: 1_000 };
        s.remember(1_000, (vec![], vec![ice.clone()]));
        s.remember(60_000, (vec![], vec![]));
        assert!(s.found.1.is_empty() && s.seen.1.len() == 1);
        s.remember(1_000 + SEEN_MS + 1, (vec![], vec![]));
        assert!(s.seen.1.is_empty());
    }

    #[test]
    fn fast_repeats_are_not_slicers() {
        let mut s = Scan::default();
        for i in 0..20 { s.print(Exchange::Okx, Market::Perp, true, i * 500, 1.0, 100.0); }
        assert!(s.clips(10_000).is_empty());
    }

    #[test]
    fn iceberg_needs_refills_beyond_shown() {
        let mut s = Scan::default();
        // ask at 101 shows 2 but trades 12 over 6 fills; bid at 99 trades what it showed
        for i in 0..6 { s.fill(Exchange::Bybit, Market::Perp, true, i * 1000, 101.0, 2.0, 2.0); }
        s.fill(Exchange::Bybit, Market::Perp, false, 0, 99.0, 5.0, 5.0);
        let ice = s.icebergs(7_000, |_, _, _, _| true);
        assert_eq!(ice.len(), 1);
        assert!(!ice[0].bid && ice[0].px == 101.0 && ice[0].traded == 12.0);
        assert!(s.icebergs(7_000, |_, _, _, _| false).is_empty());
        // fills against a level our book no longer shows do not count
        s.fill(Exchange::Bybit, Market::Perp, true, 8_000, 102.0, 50.0, 0.0);
        assert_eq!(s.icebergs(8_000, |_, _, _, _| true).len(), 1);
    }
}
