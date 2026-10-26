//! Hunter: a market-wide scanner of perps across venues, built from each venue's all-symbol
//! tickers. Facts only (no model labels yet): per coin the venues' short-window price changes, a
//! volume multiple, funding, open-interest change, liquidations, the largest cross-venue gap and
//! the venue that moved first. Pure: the engine polls the venues and feeds `Scan`.
//!
//! Every payload becomes `Tick`s (unified base, price, 24 h volume, one-sided OI in base units,
//! HOURLY funding); `Scan` keeps a ring buffer of samples per (coin, venue, market) and
//! `Scan::rows` assembles one `Row` per coin listed as a perp somewhere.
use crate::{alias, num, opt_num, Exchange, Market};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

/// The venues scanned: REST all-ticker polls, Binance over its all-market streams.
pub const VENUES: [Exchange; 7] = [Exchange::Binance, Exchange::Bybit, Exchange::Okx, Exchange::Bitget, Exchange::Gate, Exchange::Mexc, Exchange::Hyperliquid];
/// Venues with a public liquidation feed the scanner reads (Binance's is one snapshot per symbol per
/// second). Bitget, MEXC and Hyperliquid publish none.
pub const LIQ_VENUES: [Exchange; 4] = [Exchange::Binance, Exchange::Okx, Exchange::Bybit, Exchange::Gate];
/// venues whose spot is scanned: the perp / spot turnover ratio compares these only
pub const SPOT_VENUES: [Exchange; 5] = [Exchange::Binance, Exchange::Bybit, Exchange::Okx, Exchange::Bitget, Exchange::Gate];

const MIN: i64 = 60_000;
/// history kept per series: the 1 h window and some slack
const KEEP_MS: i64 = 65 * MIN;
/// samples closer together than this are merged into the newest (Binance pushes every second)
const STEP_MS: i64 = 15_000;
/// how far outside a series' first / last sample a time may be and still read that sample
const SLACK_MS: i64 = 45_000;
/// a venue whose newest sample is older than this is left out of its coin's row
const STALE_MS: i64 = 120_000;
/// change windows of `Row::chg`
pub const WINDOWS: [i64; 3] = [5 * MIN, 15 * MIN, 60 * MIN];
/// a price more than this far off a coin's median is another asset under the same name
const ODD: f64 = 0.30;
/// a market trading less than this a day (USD) is left out of gaps: its last trade is not a price
const GAP_MIN_VOL: f64 = 100_000.0;
/// the leading venue is looked for on 15 min moves at least this large (fraction)
const LEAD_MIN: f64 = 0.01;
/// one alert per coin per this long
pub const COOL_MS: i64 = 15 * MIN;

/// One reading of a series: last price, rolling 24 h volume in base units, OI in base units (0 = none).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample { pub ts: i64, pub px: f64, pub bvol: f32, pub oi: f32 }

/// One venue's numbers for one coin from one payload. `px` 0: the payload has no price (OI or
/// funding only); None: not in this payload (the venue's last value stays).
#[derive(Clone, Debug, PartialEq)]
pub struct Tick { pub ex: Exchange, pub spot: bool, pub base: String, pub px: f64,
    /// 24 h volume in USD(T) and in base units
    pub qvol: Option<f64>, pub bvol: Option<f64>,
    /// open interest, one side, base units
    pub oi: Option<f64>,
    /// funding rate PER HOUR (fraction)
    pub fund: Option<f64> }

fn tick(ex: Exchange, spot: bool, base: String) -> Tick { Tick { ex, spot, base, px: 0.0, qvol: None, bvol: None, oi: None, fund: None } }

/// Per native symbol: funding interval (hours) and contract size (base units per contract); per
/// coin: its oldest perp listing (ms) on the venues whose contract lists carry one (Gate, MEXC, Bitget),
/// and `tradfi`: the coins any venue's own metadata marks as a tokenized stock, ETF, commodity, index or
/// currency (Binance contractType / underlyingSubType, Bybit symbolType, OKX instCategory, Bitget isRwa,
/// Gate contract_type, MEXC conceptPlate). They are left out on every venue.
/// ponytail: a coin sharing its ticker with a stock (PURR) is left out too; the flag is per name
#[derive(Default)]
pub struct Meta { pub hours: HashMap<(Exchange, String), f64>, pub size: HashMap<(Exchange, String), f64>, pub listed: HashMap<String, i64>, pub tradfi: HashSet<String> }

impl Meta {
    fn flag(&mut self, ex: Exchange, native: Option<&str>) {
        if let Some(b) = native.filter(|b| ok(b)) { self.tradfi.insert(alias::unified_base(ex, b)); }
    }

    fn listed_at(&mut self, base: Option<String>, ms: Option<f64>) {
        let (Some(b), Some(ms)) = (base, ms.filter(|x| *x > 0.0)) else { return };
        let t = self.listed.entry(b).or_insert(ms as i64);
        *t = (*t).min(ms as i64);
    }
}

struct Venue { ex: Exchange, spot: bool, s: VecDeque<Sample>, qvol: f64, bvol: f64, oi: f64, fund: Option<f64>,
    /// the venue's own OI history (ms, base units, oldest first) where it has no all-symbol OI (`Scan::oi_hist`)
    oih: Vec<(i64, f64)> }

impl Venue {
    fn push(&mut self, x: Sample) {
        if self.s.back().is_some_and(|l| x.ts <= l.ts) { return; }
        let n = self.s.len();
        if n >= 2 && self.s[n - 1].ts - self.s[n - 2].ts < STEP_MS { self.s[n - 1] = x; } else { self.s.push_back(x); }
        while self.s.len() > 2 && self.s[1].ts < x.ts - KEEP_MS { self.s.pop_front(); }
    }
}

/// The series at `t`: interpolated between the samples around it, or its first / last sample when
/// `t` lies at most SLACK_MS outside them. OI reads 0 (unknown) unless both neighbours have one.
fn at(s: &VecDeque<Sample>, t: i64) -> Option<Sample> {
    let i = s.partition_point(|x| x.ts <= t);
    if i == 0 { return s.front().filter(|f| f.ts - t <= SLACK_MS).copied(); }
    let a = s[i - 1];
    let Some(&b) = s.get(i) else { return (t - a.ts <= SLACK_MS).then_some(a) };
    let k = (t - a.ts) as f64 / (b.ts - a.ts) as f64;
    let mix = |x: f32, y: f32| if x > 0.0 && y > 0.0 { x + (y - x) * k as f32 } else { 0.0 };
    Some(Sample { ts: t, px: a.px + (b.px - a.px) * k, bvol: mix(a.bvol, b.bvol), oi: mix(a.oi, b.oi) })
}

/// a venue's OI history older than this (newest point) is not used
const OIH_MAX_AGE: i64 = 15 * MIN;

/// The history point nearest `t`, within half of Binance's 5 min step.
fn oih_at(p: &[(i64, f64)], t: i64) -> Option<f64> {
    p.iter().min_by_key(|x| (x.0 - t).abs()).filter(|x| (x.0 - t).abs() <= 150_000 && x.1 > 0.0).map(|x| x.1)
}

/// Binance `futures/data/openInterestHist` rows ({sumOpenInterest (base units), timestamp}) as (ms, OI), oldest first.
pub fn oi_points(v: &Value) -> Vec<(i64, f64)> {
    let mut p: Vec<(i64, f64)> = arr(v).iter().filter_map(|x| Some((x["timestamp"].as_i64()?, num(&x["sumOpenInterest"])))).filter(|x| x.1 > 0.0 && x.1.is_finite()).collect();
    p.sort_by_key(|x| x.0);
    p
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() { return None; }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Largest cross-venue price gap: `hi` above `lo` by `bp`; (venue, spot).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gap { pub bp: f64, pub hi: (Exchange, bool), pub lo: (Exchange, bool) }

/// The venue (and market) whose price first covered half of the 15 min move, `secs` before the next one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lead { pub ex: Exchange, pub spot: bool, pub secs: f64 }

/// One coin: facts only. Percentages are in percent, funding a fraction per hour.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub base: String,
    /// median perp price
    pub px: f64,
    /// change over WINDOWS (5 m, 15 m, 1 h): median of the perp venues' own changes
    pub chg: [Option<f64>; 3],
    /// last 15 min volume / the 24 h average rate (from the rolling 24 h volumes)
    pub vmult: Option<f64>,
    /// 24 h perp volume summed over venues, USD
    pub vol24: f64,
    /// 24 h (perp, spot) volume over SPOT_VENUES only, USD (perp-only venues would inflate the ratio)
    pub ps24: (f64, f64),
    /// most extreme hourly funding and its venue
    pub fund: Option<(Exchange, f64)>,
    /// OI change over 15 m and 1 h, summed over the venues that report OI
    pub oi_chg: [Option<f64>; 2],
    /// liquidations in the last 15 min, USD; None where no listing venue has a feed
    pub liq15: Option<f64>,
    pub gap: Option<Gap>,
    pub lead: Option<Lead>,
    /// perp venues listing it
    pub venues: Vec<Exchange>,
    /// (venue, spot) priced too far from the others: probably another asset with this ticker
    pub odd: Vec<(Exchange, bool)>,
    /// detector labels; empty until a backtest decides which ship
    pub flags: Vec<&'static str>,
    /// operator-type score (hourly, from the stored hours); None for coins not recorded
    pub op: Option<Op>,
}

#[derive(Default)]
pub struct Scan { coins: HashMap<String, Vec<Venue>>, liq: HashMap<String, VecDeque<(i64, f64)>>, pub meta: Meta,
    /// `meta.tradfi` size when the coins were last cleared of it
    cleared: usize }

impl Scan {
    /// Records `ticks` read at `ts` (local ms). Spot venues are kept only for coins with a perp.
    pub fn ingest(&mut self, ts: i64, ticks: Vec<Tick>) {
        if self.meta.tradfi.len() != self.cleared {
            self.cleared = self.meta.tradfi.len();
            let tf = &self.meta.tradfi;
            self.coins.retain(|b, _| !tf.contains(b));
            self.liq.retain(|b, _| !tf.contains(b));
        }
        for t in ticks {
            if self.meta.tradfi.contains(&t.base) || (t.spot && !self.coins.contains_key(&t.base)) { continue; }
            let vs = self.coins.entry(t.base).or_default();
            let i = vs.iter().position(|v| v.ex == t.ex && v.spot == t.spot).unwrap_or_else(|| {
                vs.push(Venue { ex: t.ex, spot: t.spot, s: VecDeque::new(), qvol: 0.0, bvol: 0.0, oi: 0.0, fund: None, oih: vec![] });
                vs.len() - 1
            });
            let v = &mut vs[i];
            let good = |x: &f64| x.is_finite() && *x >= 0.0;
            if let Some(x) = t.qvol.filter(good) { v.qvol = x; }
            if let Some(x) = t.bvol.filter(good) { v.bvol = x; }
            if let Some(x) = t.oi.filter(good) { v.oi = x; }
            if let Some(x) = t.fund.filter(|x| x.is_finite()) { v.fund = Some(x); }
            if t.px > 0.0 && t.px.is_finite() { let (bvol, oi) = (v.bvol as f32, v.oi as f32); v.push(Sample { ts, px: t.px, bvol, oi }); }
        }
    }

    /// `ex`'s perp open-interest history of `base` (`oi_points`), for venues with no all-symbol OI.
    pub fn oi_hist(&mut self, ex: Exchange, base: &str, pts: Vec<(i64, f64)>) {
        if let Some(v) = self.coins.get_mut(base).and_then(|vs| vs.iter_mut().find(|v| v.ex == ex && !v.spot)) { v.oih = pts; }
    }

    /// A liquidation of `usd` on `base` at `ts`.
    pub fn liq(&mut self, base: String, ts: i64, usd: f64) {
        if self.meta.tradfi.contains(&base) { return; }
        let q = self.liq.entry(base).or_default();
        q.push_back((ts, usd));
        while q.front().is_some_and(|x| x.0 < ts - 15 * MIN) { q.pop_front(); }
    }

    /// Each coin's hour starting at `start` on its main perp venue (Binance where it has the hour,
    /// else the venue trading most), for coins trading at least REC_MIN_VOL a day.
    pub fn hour_bars(&self, start: i64) -> Vec<(Exchange, String, HourBar)> {
        let mut out = vec![];
        for (b, vs) in &self.coins {
            let mut perps: Vec<&Venue> = vs.iter().filter(|v| !v.spot).collect();
            if perps.iter().map(|v| v.qvol).sum::<f64>() < REC_MIN_VOL { continue; }
            perps.sort_by(|x, y| (y.ex == Exchange::Binance).cmp(&(x.ex == Exchange::Binance)).then(y.qvol.total_cmp(&x.qvol)));
            if let Some((ex, bar)) = perps.iter().find_map(|v| Some((v.ex, hour_bar(&v.s, start)?))) { out.push((ex, b.clone(), bar)); }
        }
        out
    }

    /// One row per coin with a live perp.
    pub fn rows(&mut self, now: i64) -> Vec<Row> {
        for q in self.liq.values_mut() { while q.front().is_some_and(|x| x.0 < now - 15 * MIN) { q.pop_front(); } }
        self.liq.retain(|_, q| !q.is_empty());
        self.coins.iter().filter_map(|(b, vs)| row(b, vs, self.liq.get(b), now)).collect()
    }
}

fn row(base: &str, vs: &[Venue], liq: Option<&VecDeque<(i64, f64)>>, now: i64) -> Option<Row> {
    let live: Vec<(&Venue, Sample)> = vs.iter().filter_map(|v| v.s.back().filter(|x| now - x.ts <= STALE_MS).map(|x| (v, *x))).collect();
    let med = median(live.iter().map(|x| x.1.px).collect())?;
    let off = |x: &(&Venue, Sample)| (x.1.px / med - 1.0).abs() > ODD;
    let odd: Vec<(Exchange, bool)> = live.iter().filter(|x| off(x)).map(|x| (x.0.ex, x.0.spot)).collect();
    // two venues far apart: no telling which one is the other asset, so none is dropped (and no gap)
    let clean = live.iter().any(|x| !x.0.spot && !off(x));
    let ok: Vec<(&Venue, Sample)> = live.iter().filter(|x| !clean || !off(x)).copied().collect();
    let perps: Vec<(&Venue, Sample)> = ok.iter().filter(|x| !x.0.spot).copied().collect();
    let px = median(perps.iter().map(|x| x.1.px).collect())?;
    let chg = WINDOWS.map(|w| median(perps.iter().filter_map(|(v, last)| Some((last.px / at(&v.s, last.ts - w)?.px - 1.0) * 100.0)).collect()));
    // rolling 24 h volume: its change over 15 min is the last 15 min minus the 15 min that left the
    // window a day ago, taken as the 24 h average rate
    // ponytail: that average is an assumption; per-venue 15m klines would measure it, at a request per coin
    let (mut n, mut d) = (0.0, 0.0);
    for (v, last) in &perps {
        let Some(then) = at(&v.s, last.ts - 15 * MIN).filter(|t| t.bvol > 0.0 && last.bvol > 0.0) else { continue };
        let avg = last.bvol as f64 / 96.0;
        n += (last.bvol as f64 - then.bvol as f64 + avg).max(0.0);
        d += avg;
    }
    let vmult = (d > 0.0).then(|| n / d);
    let oi_chg = [15 * MIN, 60 * MIN].map(|w| {
        let (mut a, mut b) = (0.0, 0.0);
        for (v, last) in &perps {
            // a venue's own history (Binance) is read between its points, the newest up to ~10 min old; the others between samples
            let hist = v.oih.last().filter(|p| now - p.0 <= OIH_MAX_AGE).and_then(|p| Some((p.1, oih_at(&v.oih, p.0 - w)?)));
            let samp = || at(&v.s, last.ts - w).filter(|t| t.oi > 0.0 && last.oi > 0.0).map(|t| (last.oi as f64, t.oi as f64));
            if let Some((x, y)) = hist.or_else(samp) { a += x; b += y; }
        }
        (b > 0.0).then(|| (a / b - 1.0) * 100.0)
    });
    let mut venues: Vec<Exchange> = perps.iter().map(|x| x.0.ex).collect();
    venues.sort();
    Some(Row {
        base: base.to_string(), px, chg, vmult,
        vol24: perps.iter().map(|x| x.0.qvol).sum(),
        ps24: [false, true].map(|spot| ok.iter().filter(|x| x.0.spot == spot && SPOT_VENUES.contains(&x.0.ex)).map(|x| x.0.qvol).sum()).into(),
        fund: perps.iter().filter_map(|x| Some((x.0.ex, x.0.fund?))).max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())),
        oi_chg,
        liq15: perps.iter().any(|x| LIQ_VENUES.contains(&x.0.ex)).then(|| liq.map_or(0.0, |q| q.iter().map(|x| x.1).sum())),
        gap: if clean { gap(&ok) } else { None },
        lead: chg[1].filter(|m| m.abs() >= LEAD_MIN * 100.0).and_then(|m| lead(&ok, now, m / 100.0)),
        venues, odd, flags: vec![], op: None,
    })
}

/// Largest gap at one moment (the oldest of the venues' newest samples, the others interpolated
/// there), perp vs perp and spot vs perp; spot vs spot is not a perp question. Markets under
/// GAP_MIN_VOL a day are left out.
/// ponytail: USDC (Hyperliquid) and USDT prices are compared as equal, a few bp of quote basis.
fn gap(vs: &[(&Venue, Sample)]) -> Option<Gap> {
    let vs: Vec<&(&Venue, Sample)> = vs.iter().filter(|x| x.0.qvol >= GAP_MIN_VOL).collect();
    let t0 = vs.iter().map(|x| x.1.ts).min()?;
    let px: Vec<(Exchange, bool, f64)> = vs.iter().filter_map(|(v, _)| Some((v.ex, v.spot, at(&v.s, t0)?.px))).collect();
    let mut best: Option<Gap> = None;
    for (i, a) in px.iter().enumerate() {
        for (j, b) in px.iter().enumerate() {
            if i == j || (a.1 && b.1) || a.2 < b.2 { continue; }
            let bp = (a.2 / b.2 - 1.0) * 1e4;
            if best.is_none_or(|g| bp > g.bp) { best = Some(Gap { bp, hi: (a.0, a.1), lo: (b.0, b.1) }); }
        }
    }
    best
}

/// When `s` first covered `half` (signed fraction) of a move from its price at `from`, interpolated between samples.
fn cross(s: &VecDeque<Sample>, from: i64, half: f64) -> Option<i64> {
    let start = at(s, from)?;
    let target = start.px * (1.0 + half);
    let hit = |p: f64| if half > 0.0 { p >= target } else { p <= target };
    let mut prev = (from, start.px);
    for x in s.iter().filter(|x| x.ts > from) {
        if hit(x.px) {
            let k = ((target - prev.1) / (x.px - prev.1)).clamp(0.0, 1.0);
            return Some(prev.0 + ((x.ts - prev.0) as f64 * k) as i64);
        }
        prev = (x.ts, x.px);
    }
    None
}

/// The venue that covered half of the 15 min move `mv` (fraction) first, among at least two venues
/// with the whole window; its lead over the next venue (or over now when no other got there).
fn lead(vs: &[(&Venue, Sample)], now: i64, mv: f64) -> Option<Lead> {
    let from = now - 15 * MIN;
    if vs.iter().filter(|x| at(&x.0.s, from).is_some()).count() < 2 { return None; }
    let mut t: Vec<(i64, Exchange, bool)> = vs.iter().filter_map(|(v, _)| Some((cross(&v.s, from, mv / 2.0)?, v.ex, v.spot))).collect();
    t.sort_by_key(|x| x.0);
    let first = *t.first()?;
    let next = t.get(1).map_or(now, |x| x.0);
    Some(Lead { ex: first.1, spot: first.2, secs: (next - first.0) as f64 / 1000.0 })
}

// ---------------------------------------------------------------- operator-type score

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
/// coins recorded hourly: perp volume of at least this much a day (USD)
pub const REC_MIN_VOL: f64 = 1e6;

/// One hour of a coin on one venue: open, high, low, close, quote volume (USD).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HourBar { pub ts: i64, pub o: f64, pub h: f64, pub l: f64, pub c: f64, pub qv: f64 }

/// The hour [start, start + 1 h) of a series: prices from the samples (the edges interpolated),
/// quote volume from the rolling 24 h volume's change plus the hour that left the window (taken as
/// the day's average, as `Row::vmult`), clamped at 0. None unless samples cover the whole hour at
/// most 5 min apart.
fn hour_bar(s: &VecDeque<Sample>, start: i64) -> Option<HourBar> {
    let (a, b) = (at(s, start)?, at(s, start + HOUR)?);
    let inside: Vec<&Sample> = s.iter().filter(|x| x.ts > start && x.ts < start + HOUR).collect();
    let ts: Vec<i64> = std::iter::once(start).chain(inside.iter().map(|x| x.ts)).chain([start + HOUR]).collect();
    if ts.windows(2).any(|w| w[1] - w[0] > 5 * MIN) || a.bvol <= 0.0 || b.bvol <= 0.0 { return None; }
    let px: Vec<f64> = inside.iter().map(|x| x.px).chain([a.px, b.px]).collect();
    let (h, l) = px.iter().fold((f64::MIN, f64::MAX), |(h, l), p| (h.max(*p), l.min(*p)));
    let base = (b.bvol as f64 - a.bvol as f64 + b.bvol as f64 / 24.0).max(0.0);
    Some(HourBar { ts: start, o: a.px, h, l, c: b.px, qv: base * px.iter().sum::<f64>() / px.len() as f64 })
}

/// Binance 1h klines ([open ms, o, h, l, c, base vol, close ms, quote vol, ...]) as hour bars,
/// without the one still open at `now`.
pub fn kline_bars(v: &Value, now: i64) -> Vec<HourBar> {
    arr(v).iter().filter_map(|k| {
        let ts = k[0].as_i64().filter(|t| t + HOUR <= now)?;
        let b = HourBar { ts, o: num(&k[1]), h: num(&k[2]), l: num(&k[3]), c: num(&k[4]), qv: num(&k[7]) };
        (b.c > 0.0 && b.qv >= 0.0).then_some(b)
    }).collect()
}

/// A coin's operator-type score: which traits of the 37 documented operator coins
/// (docs/althunt-backtest.md, "Classic cases only") it shows over the trailing 30 days. It
/// classifies the coin; it does not time a move (on those coins the frequent "precursors" were
/// their normal state).
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// fewer than 7 days of hourly bars: whole days collected
    Collecting(usize),
    /// 0-100, the share of the evaluable traits shown; each (trait, value, shown)
    Score { score: f64, parts: Vec<(&'static str, f64, bool)> },
}

/// The traits, over the trailing 30 days of one venue's hours: an hour of 20x the median hourly
/// volume in at least 40% of 72 h blocks (the cases 40-84%, ordinary alts about 19%); a two-sided
/// wick (each wick longer than the body, range over 3x the median) in at least 25% of blocks; two
/// or more days moving over 20%; listed under 60 days ago (when a venue says); perp turnover over
/// 30x spot (about twice the market's 90th percentile: ETH and HYPE sit near 15x), or no spot on the scanned venues; a 7-day run-up over 100%. Values: block shares,
/// day count, age in days, perp / spot ratio (infinite: no spot), run-up fraction.
/// ponytail: equal weights and hand-set thresholds, uncalibrated; fit them on the 37 cases with scripts/althunt.py
pub fn op_score(bars: &[HourBar], now: i64, perp24: f64, spot24: f64, listed: Option<i64>) -> Op {
    let bars: Vec<&HourBar> = bars.iter().filter(|b| b.ts >= now - 30 * DAY && b.ts + HOUR <= now).collect();
    if bars.len() < 7 * 24 { return Op::Collecting(bars.len() / 24); }
    let mv = median(bars.iter().map(|b| b.qv).collect()).unwrap_or(0.0);
    let mr = median(bars.iter().map(|b| b.h - b.l).collect()).unwrap_or(0.0);
    // share of the 72 h blocks back from now (those with a day of hours or more) with an hour that hits
    let blocks = |hit: &dyn Fn(&HourBar) -> bool| {
        let (mut n, mut k) = (0, 0);
        for i in 0..10 {
            let (a, z) = (now - (i + 1) * 3 * DAY, now - i * 3 * DAY);
            let inb: Vec<&&HourBar> = bars.iter().filter(|b| b.ts >= a && b.ts < z).collect();
            if inb.len() < 24 { continue; }
            n += 1;
            if inb.iter().any(|b| hit(b)) { k += 1; }
        }
        if n == 0 { 0.0 } else { k as f64 / n as f64 }
    };
    let spikes = blocks(&|b| mv > 0.0 && b.qv >= 20.0 * mv);
    let wicks = blocks(&|b| { let (top, bot) = (b.o.max(b.c), b.o.min(b.c)); b.h - top > top - bot && bot - b.l > top - bot && b.h - b.l > 3.0 * mr });
    // closes a day apart, back from the last hour; a missing close skips that day
    let close = |t: i64| bars.iter().rev().find(|b| b.ts + HOUR <= t && b.ts + 3 * HOUR > t).map(|b| b.c);
    let end = bars.last().map_or(now, |b| b.ts + HOUR);
    let days = (0..30).filter_map(|i| Some(close(end - i * DAY)? / close(end - (i + 1) * DAY)? - 1.0)).filter(|r| r.abs() > 0.2).count();
    // highest close over the lowest of the 7 days before it (a rising minimum queue)
    let (mut q, mut run) = (VecDeque::<(i64, f64)>::new(), 0.0f64);
    for b in &bars {
        while q.back().is_some_and(|x| x.1 >= b.c) { q.pop_back(); }
        q.push_back((b.ts, b.c));
        while q.front().is_some_and(|x| x.0 < b.ts - 7 * DAY) { q.pop_front(); }
        run = run.max(b.c / q[0].1 - 1.0);
    }
    let ratio = if spot24 > 0.0 { perp24 / spot24 } else { f64::INFINITY };
    let mut parts = vec![("spikes", spikes, spikes >= 0.4), ("wicks", wicks, wicks >= 0.25), ("days", days as f64, days >= 2)];
    if let Some(t) = listed { let age = (now - t) as f64 / DAY as f64; parts.push(("new", age, age < 60.0)); }
    parts.extend([("perp", ratio, ratio > 30.0), ("runup", run, run > 1.0)]);
    Op::Score { score: 100.0 * parts.iter().filter(|p| p.2).count() as f64 / parts.len() as f64, parts }
}

/// Scores of the coins in `vols` ((perp, spot) 24 h volume on SPOT_VENUES by base) from the stored hours by base,
/// each coin on the venue with the most hours. Coins with no hours that are not recorded either get none.
pub fn op_scores(bars: &HashMap<String, Vec<(Exchange, HourBar)>>, vols: &HashMap<String, (f64, f64)>, listed: &HashMap<String, i64>, now: i64) -> HashMap<String, Op> {
    let mut out = HashMap::new();
    for (b, &(perp, spot)) in vols {
        let hours = bars.get(b).map_or(&[][..], |v| v.as_slice());
        if hours.is_empty() && perp < REC_MIN_VOL { continue; }
        let mut n: HashMap<Exchange, usize> = HashMap::new();
        for (e, _) in hours { *n.entry(*e).or_default() += 1; }
        let ex = n.into_iter().max_by_key(|x| (x.1, x.0)).map(|x| x.0);
        let mut series: Vec<HourBar> = hours.iter().filter(|x| Some(x.0) == ex).map(|x| x.1).collect();
        series.sort_by_key(|x| x.ts);
        out.insert(b.clone(), op_score(&series, now, perp, spot, listed.get(b).copied()));
    }
    out
}

// ---------------------------------------------------------------- alerts

/// Alert rules of the Hunter page, checked on every rebuild for coins above `min_vol` (the
/// page's volume filter). `rules[i]` = (on, threshold): 5 m move (abs %), volume multiple, funding
/// below minus the threshold (%/h), cross-venue gap (bp).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Alerts { pub min_vol: f64, pub rules: [(bool, f64); 4] }

impl Default for Alerts {
    fn default() -> Self { Alerts { min_vol: 5e6, rules: [(false, 5.0), (false, 5.0), (false, 0.05), (false, 100.0)] } }
}

/// The rules `r` crosses (indices into `Alerts::rules`).
pub fn crossed(a: &Alerts, r: &Row) -> Vec<usize> {
    let [m, v, f, g] = a.rules;
    let hits = [
        m.0 && r.chg[0].is_some_and(|x| x.abs() >= m.1),
        v.0 && r.vmult.is_some_and(|x| x >= v.1),
        f.0 && r.fund.is_some_and(|x| x.1 * 100.0 <= -f.1),
        g.0 && r.gap.is_some_and(|x| x.bp >= g.1),
    ];
    (0..4).filter(|&i| hits[i]).collect()
}

/// Coins above the minimum volume crossing a rule, each at most once per COOL_MS (`last`: coin -> ms of its last alert).
pub fn check<'a>(a: &Alerts, rows: &'a [Row], now: i64, last: &mut HashMap<String, i64>) -> Vec<(&'a Row, Vec<usize>)> {
    last.retain(|_, t| now - *t < COOL_MS);
    let mut out = vec![];
    for r in rows.iter().filter(|r| r.vol24 >= a.min_vol) {
        let k = crossed(a, r);
        if !k.is_empty() && !last.contains_key(&r.base) { last.insert(r.base.clone(), now); out.push((r, k)); }
    }
    out
}

// ---------------------------------------------------------------- venue payloads

/// The all-symbol payloads the scanner reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Src { BybitPerp, BybitSpot, OkxPerp, OkxSpot, OkxOi, OkxFunding, BitgetMeta, BitgetPerp, BitgetSpot, GateMeta, GatePerp, GateSpot, MexcMeta, MexcPerp, HlPerp,
    /// the picker's Binance spot poll (24 h MINI); Binance perps come over the all-market streams
    BinanceSpot, BinanceMini, BinanceMark,
    /// MEXC's funding of every contract: rate and interval (`collectCycle`) together
    MexcFunding,
    /// contract lists read for their TradFi flags only (hourly)
    BinanceInfo, BybitInfo, OkxInfo }

impl Src {
    /// REST URL (Hyperliquid: POST {"type":"metaAndAssetCtxs"}); "" for stream frames.
    pub fn url(self) -> &'static str {
        match self {
            Src::BybitPerp => "https://api.bybit.com/v5/market/tickers?category=linear",
            Src::BybitSpot => "https://api.bybit.com/v5/market/tickers?category=spot",
            Src::OkxPerp => "https://www.okx.com/api/v5/market/tickers?instType=SWAP",
            Src::OkxSpot => "https://www.okx.com/api/v5/market/tickers?instType=SPOT",
            Src::OkxOi => "https://www.okx.com/api/v5/public/open-interest?instType=SWAP",
            Src::OkxFunding => "https://www.okx.com/api/v5/public/funding-rate?instId=ANY",
            Src::BitgetMeta => "https://api.bitget.com/api/v2/mix/market/contracts?productType=USDT-FUTURES",
            Src::BitgetPerp => "https://api.bitget.com/api/v2/mix/market/tickers?productType=USDT-FUTURES",
            Src::BitgetSpot => "https://api.bitget.com/api/v2/spot/market/tickers",
            Src::GateMeta => "https://api.gateio.ws/api/v4/futures/usdt/contracts",
            Src::GatePerp => "https://api.gateio.ws/api/v4/futures/usdt/tickers",
            Src::GateSpot => "https://api.gateio.ws/api/v4/spot/tickers",
            Src::MexcMeta => "https://contract.mexc.com/api/v1/contract/detail",
            Src::MexcPerp => "https://contract.mexc.com/api/v1/contract/ticker",
            Src::MexcFunding => "https://contract.mexc.com/api/v1/contract/funding_rate",
            Src::HlPerp => "https://api.hyperliquid.xyz/info",
            Src::BinanceSpot => "https://api.binance.com/api/v3/ticker/24hr?type=MINI",
            Src::BinanceMini | Src::BinanceMark => "",
            Src::BinanceInfo => "https://fapi.binance.com/fapi/v1/exchangeInfo",
            // ponytail: one page (limit 1000, about 920 listed); follow nextPageCursor when Bybit passes 1000
            Src::BybitInfo => "https://api.bybit.com/v5/market/instruments-info?category=linear&limit=1000",
            Src::OkxInfo => "https://www.okx.com/api/v5/public/instruments?instType=SWAP",
        }
    }
}

/// A base the venue's tickers can be merged under (the picker's rule): plain ASCII, no dated or
/// quote-suffixed names.
fn ok(b: &str) -> bool { !b.is_empty() && !b.contains('_') && !b.contains('-') && b.is_ascii() }

/// A venue's own perp base under the unified (Binance) name (Bybit AMDSTOCK -> AMD); None when its
/// same-name market is another asset (crate alias table), so it never merges into that coin.
pub fn unify(ex: Exchange, b: &str) -> Option<String> {
    let u = alias::unified_base(ex, b);
    alias::venue_base(ex, Market::Perp, &u).is_some().then_some(u)
}

fn perp_base(ex: Exchange, b: Option<&str>) -> Option<String> { b.filter(|b| ok(b)).and_then(|b| unify(ex, b)) }
fn spot_base(b: Option<&str>) -> Option<String> { b.filter(|b| ok(b)).map(String::from) }
fn arr(v: &Value) -> &[Value] { v.as_array().map_or(&[], |a| a.as_slice()) }

/// Ticks in one payload; meta payloads (funding intervals, contract sizes) fill `meta` and give none.
pub fn parse(src: Src, v: &Value, meta: &mut Meta) -> Vec<Tick> {
    use Exchange as E;
    use Src::*;
    let rows = match src {
        BybitPerp | BybitSpot | BybitInfo => arr(&v["result"]["list"]),
        BinanceInfo => arr(&v["symbols"]),
        GateMeta | GatePerp | GateSpot | BinanceSpot | BinanceMini | BinanceMark => arr(v),
        HlPerp => return hl(v),
        _ => arr(&v["data"]),
    };
    let mut out = vec![];
    for x in rows {
        let s = |k: &str| x[k].as_str().unwrap_or_default();
        let f = |k: &str| opt_num(&x[k]);
        let hours = |ex: E, sym: &str| meta.hours.get(&(ex, sym.to_string())).copied().filter(|h| *h > 0.0);
        let t = match src {
            BybitPerp => perp_base(E::Bybit, s("symbol").strip_suffix("USDT")).map(|b| Tick {
                px: f("lastPrice").unwrap_or(0.0), qvol: f("turnover24h"), bvol: f("volume24h"),
                // `openInterest` counts both sides
                oi: f("singleOpenInterest").or(f("openInterest").map(|o| o / 2.0)),
                fund: f("fundingRate").zip(f("fundingIntervalHour").filter(|h| *h > 0.0)).map(|(r, h)| r / h),
                ..tick(E::Bybit, false, b) }),
            BybitSpot => spot_base(s("symbol").strip_suffix("USDT")).map(|b| Tick { px: f("lastPrice").unwrap_or(0.0), qvol: f("turnover24h"), bvol: f("volume24h"), ..tick(E::Bybit, true, b) }),
            // swaps: volCcy24h is in base units; spot: vol24h base, volCcy24h quote
            OkxPerp => perp_base(E::Okx, s("instId").strip_suffix("-USDT-SWAP")).map(|b| Tick {
                px: f("last").unwrap_or(0.0), qvol: f("volCcy24h").zip(f("last")).map(|(v, p)| v * p), bvol: f("volCcy24h"), ..tick(E::Okx, false, b) }),
            OkxSpot => spot_base(s("instId").strip_suffix("-USDT")).map(|b| Tick { px: f("last").unwrap_or(0.0), qvol: f("volCcy24h"), bvol: f("vol24h"), ..tick(E::Okx, true, b) }),
            OkxOi => {
                // oiCcy = contracts x contract value: that ratio sizes the liquidation feed's contracts
                if let Some(ct) = f("oiCcy").zip(f("oi").filter(|o| *o > 0.0)).map(|(c, o)| c / o) { meta.size.insert((E::Okx, s("instId").to_string()), ct); }
                perp_base(E::Okx, s("instId").strip_suffix("-USDT-SWAP")).map(|b| Tick { oi: f("oiCcy"), ..tick(E::Okx, false, b) })
            }
            OkxFunding => perp_base(E::Okx, s("instId").strip_suffix("-USDT-SWAP")).map(|b| {
                // the interval is fundingTime - prevFundingTime (no field of its own), else next - this
                let (prev, at, next) = (f("prevFundingTime").unwrap_or(0.0), f("fundingTime").unwrap_or(0.0), f("nextFundingTime").unwrap_or(0.0));
                let h = if prev > 0.0 && at > prev { at - prev } else { next - at } / 3.6e6;
                Tick { fund: f("fundingRate").filter(|_| h > 0.0).map(|r| r / h), ..tick(E::Okx, false, b) }
            }),
            BinanceInfo => {
                if s("contractType") == "TRADIFI_PERPETUAL" || arr(&x["underlyingSubType"]).iter().any(|t| t == "TradFi") { meta.flag(E::Binance, x["baseAsset"].as_str()); }
                None
            }
            BybitInfo => {
                if matches!(s("symbolType"), "stock" | "ETF" | "commodity" | "forex") { meta.flag(E::Bybit, x["baseCoin"].as_str()); }
                None
            }
            OkxInfo => {
                // instCategory 1 is crypto; 3 stocks / ETFs, 4 commodities (pre-IPO names are 3)
                if !matches!(s("instCategory"), "" | "1") { meta.flag(E::Okx, s("instId").split('-').next()); }
                None
            }
            BitgetMeta => {
                if s("isRwa") == "YES" { meta.flag(E::Bitget, s("symbol").strip_suffix("USDT")); }
                if let Some(h) = f("fundInterval") { meta.hours.insert((E::Bitget, s("symbol").to_string()), h); }
                meta.listed_at(perp_base(E::Bitget, s("symbol").strip_suffix("USDT")), f("launchTime"));
                None
            }
            BitgetPerp => perp_base(E::Bitget, s("symbol").strip_suffix("USDT")).map(|b| Tick {
                px: f("lastPr").unwrap_or(0.0), qvol: f("quoteVolume"), bvol: f("baseVolume"), oi: f("holdingAmount"),
                fund: f("fundingRate").zip(hours(E::Bitget, s("symbol"))).map(|(r, h)| r / h), ..tick(E::Bitget, false, b) }),
            BitgetSpot => spot_base(s("symbol").strip_suffix("USDT")).map(|b| Tick { px: f("lastPr").unwrap_or(0.0), qvol: f("quoteVolume"), bvol: f("baseVolume"), ..tick(E::Bitget, true, b) }),
            GateMeta => {
                if !s("contract_type").is_empty() { meta.flag(E::Gate, s("name").strip_suffix("_USDT")); }
                if let Some(secs) = f("funding_interval") { meta.hours.insert((E::Gate, s("name").to_string()), secs / 3600.0); }
                if let Some(m) = f("quanto_multiplier") { meta.size.insert((E::Gate, s("name").to_string()), m); }
                meta.listed_at(perp_base(E::Gate, s("name").strip_suffix("_USDT")), f("launch_time").or(f("create_time")).map(|x| x * 1000.0));
                None
            }
            GatePerp => perp_base(E::Gate, s("contract").strip_suffix("_USDT")).map(|b| {
                let mult = f("quanto_multiplier").or_else(|| meta.size.get(&(E::Gate, s("contract").to_string())).copied());
                Tick { px: f("last").unwrap_or(0.0), qvol: f("volume_24h_quote"), bvol: f("volume_24h_base"), oi: f("total_size").zip(mult).map(|(n, m)| n * m),
                    fund: f("funding_rate").zip(hours(E::Gate, s("contract"))).map(|(r, h)| r / h), ..tick(E::Gate, false, b) }
            }),
            GateSpot => spot_base(s("currency_pair").strip_suffix("_USDT")).map(|b| Tick { px: f("last").unwrap_or(0.0), qvol: f("quote_volume"), bvol: f("base_volume"), ..tick(E::Gate, true, b) }),
            MexcMeta => {
                if arr(&x["conceptPlate"]).iter().any(|p| p == "mc-trade-zone-tradfi" || p == "mc-trade-zone-Stock") { meta.flag(E::Mexc, s("symbol").strip_suffix("_USDT")); }
                if let Some(c) = f("contractSize") { meta.size.insert((E::Mexc, s("symbol").to_string()), c); }
                meta.listed_at(perp_base(E::Mexc, s("symbol").strip_suffix("_USDT")), f("createTime"));
                None
            }
            // funding: `fundingRate` per `collectCycle` hours (1, 4, 8 or 24 exist), no rate without its interval
            MexcFunding => perp_base(E::Mexc, s("symbol").strip_suffix("_USDT")).map(|b| Tick {
                fund: f("fundingRate").zip(f("collectCycle").filter(|h| *h > 0.0)).map(|(r, h)| r / h), ..tick(E::Mexc, false, b) }),
            // sizes are contracts: nothing until the contract sizes are known (funding: MexcFunding)
            MexcPerp => meta.size.get(&(E::Mexc, s("symbol").to_string())).copied().and_then(|cs| perp_base(E::Mexc, s("symbol").strip_suffix("_USDT")).map(|b| Tick {
                px: f("lastPrice").unwrap_or(0.0), qvol: f("amount24"), bvol: f("volume24").map(|v| v * cs), oi: f("holdVol").map(|v| v * cs), ..tick(E::Mexc, false, b) })),
            BinanceSpot => spot_base(s("symbol").strip_suffix("USDT")).map(|b| Tick { px: f("lastPrice").unwrap_or(0.0), qvol: f("quoteVolume"), bvol: f("volume"), ..tick(E::Binance, true, b) }),
            BinanceMini => perp_base(E::Binance, s("s").strip_suffix("USDT")).map(|b| Tick { px: f("c").unwrap_or(0.0), qvol: f("q"), bvol: f("v"), ..tick(E::Binance, false, b) }),
            BinanceMark => perp_base(E::Binance, s("s").strip_suffix("USDT")).map(|b| Tick { fund: f("r").zip(hours(E::Binance, s("s"))).map(|(r, h)| r / h), ..tick(E::Binance, false, b) }),
            HlPerp => None,
        };
        out.extend(t);
    }
    out
}

/// Hyperliquid metaAndAssetCtxs: [meta, ctxs] in the same order; funding is hourly already.
fn hl(v: &Value) -> Vec<Tick> {
    arr(&v[0]["universe"]).iter().zip(arr(&v[1])).filter(|(u, _)| u["isDelisted"] != true).filter_map(|(u, c)| {
        let f = |k: &str| opt_num(&c[k]);
        let b = perp_base(Exchange::Hyperliquid, u["name"].as_str())?;
        Some(Tick { px: f("midPx").or(f("markPx")).unwrap_or(0.0), qvol: f("dayNtlVlm"), bvol: f("dayBaseVlm"), oi: f("openInterest"), fund: f("funding"), ..tick(Exchange::Hyperliquid, false, b) })
    }).collect()
}

/// Liquidations in one liquidation frame: (base, ms, USD). Binance `!forceOrder@arr` data; OKX
/// `liquidation-orders` (contracts, sized from the OI payload's contract value); Bybit
/// `allLiquidation.<symbol>` (base units, at the bankruptcy price); Gate `futures.public_liquidates`
/// (signed contracts, sized from the contract list).
pub fn liqs(ex: Exchange, v: &Value, meta: &Meta) -> Vec<(String, i64, f64)> {
    let mut out = vec![];
    match ex {
        Exchange::Bybit => for d in arr(&v["data"]) {
            if let Some(b) = perp_base(ex, d["s"].as_str().and_then(|s| s.strip_suffix("USDT"))) { out.push((b, d["T"].as_i64().unwrap_or(0), num(&d["v"]) * num(&d["p"]))); }
        },
        Exchange::Gate => for d in arr(&v["result"]) {
            let c = d["contract"].as_str().unwrap_or_default();
            let (Some(b), Some(m)) = (perp_base(ex, c.strip_suffix("_USDT")), meta.size.get(&(ex, c.to_string()))) else { continue };
            // `time` is in ms on the wire today; seconds are read too
            let t = d["time"].as_i64().unwrap_or(0);
            out.push((b, if t < 100_000_000_000 { t * 1000 } else { t }, num(&d["size"]).abs() * m * num(&d["price"])));
        },
        Exchange::Binance => {
            let o = &v["o"];
            if let Some(b) = perp_base(ex, o["s"].as_str().and_then(|s| s.strip_suffix("USDT"))) { out.push((b, o["T"].as_i64().unwrap_or(0), num(&o["ap"]) * num(&o["q"]))); }
        }
        Exchange::Okx => for d in arr(&v["data"]) {
            let inst = d["instId"].as_str().unwrap_or_default();
            let (Some(b), Some(ct)) = (perp_base(ex, inst.strip_suffix("-USDT-SWAP")), meta.size.get(&(ex, inst.to_string()))) else { continue };
            for x in arr(&d["details"]) { out.push((b.clone(), num(&x["ts"]) as i64, num(&x["bkPx"]) * num(&x["sz"]) * ct)); }
        },
        _ => {}
    }
    out.retain(|x| x.2.is_finite() && x.2 > 0.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use Exchange as E;

    fn t(ex: E, spot: bool, base: &str, px: f64, bvol: f64, oi: f64) -> Tick {
        Tick { px, qvol: Some(bvol * px), bvol: Some(bvol), oi: (oi > 0.0).then_some(oi), ..tick(ex, spot, base.into()) }
    }

    #[test]
    fn ring_merges_close_samples_and_keeps_an_hour() {
        let mut s = Scan::default();
        // Binance-like: a tick every second for 30 s, then every 30 s for 2 h
        for k in 0..30 { s.ingest(k * 1000, vec![t(E::Binance, false, "X", 1.0 + k as f64, 1.0, 0.0)]); }
        let v = &s.coins["X"][0];
        // merged into a 15 s grid plus the moving newest sample
        assert_eq!(v.s.iter().map(|x| x.ts).collect::<Vec<_>>(), vec![0, 15_000, 29_000]);
        assert_eq!(v.s.back().unwrap().px, 30.0);
        for k in 1..=240 { s.ingest(29_000 + k * 30_000, vec![t(E::Binance, false, "X", 1.0, 1.0, 0.0)]); }
        let v = &s.coins["X"][0];
        let span = v.s.back().unwrap().ts - v.s[1].ts;
        assert!(span <= KEEP_MS && v.s.back().unwrap().ts - v.s[0].ts >= KEEP_MS);
        // older or repeated timestamps are ignored
        let n = v.s.len();
        s.ingest(0, vec![t(E::Binance, false, "X", 9.0, 1.0, 0.0)]);
        assert_eq!(s.coins["X"][0].s.len(), n);
        // spot without a perp listing is not kept
        s.ingest(1, vec![t(E::Bybit, true, "NOPERP", 1.0, 1.0, 0.0)]);
        assert!(!s.coins.contains_key("NOPERP"));
    }

    #[test]
    fn interpolation_and_slack() {
        let s: VecDeque<Sample> = [(0, 100.0, 10.0), (30_000, 130.0, 0.0), (60_000, 160.0, 40.0)].iter().map(|&(ts, px, oi)| Sample { ts, px, bvol: 1.0, oi }).collect();
        assert_eq!(at(&s, 15_000).unwrap().px, 115.0);
        // OI unknown on one side: unknown
        assert_eq!(at(&s, 15_000).unwrap().oi, 0.0);
        assert_eq!(at(&s, -40_000).unwrap().px, 100.0);
        assert!(at(&s, -50_000).is_none() && at(&s, 110_000).is_none());
        assert_eq!(at(&s, 100_000).unwrap().px, 160.0);
    }

    /// Two perps and a spot, 30 s apart for 61 minutes: price +10% over the last 5 min on both perps
    /// (Bybit first), volume three times the day's rate over the last 15 min, OI +20% over the hour.
    fn scan() -> (Scan, i64) {
        let mut s = Scan::default();
        let end = 61 * MIN;
        for k in 0..=(end / 30_000) {
            let ts = k * 30_000;
            let left = end - ts;
            let up = |lag: i64| if left >= 5 * MIN + lag { 1.0 } else { 1.0 + 0.1 * ((5 * MIN + lag - left) as f64 / (5 * MIN) as f64).min(1.0) };
            // rolling 24 h base volume of 96 000: 1 000 per 15 min, 3 000 per 15 min recently
            let vol = 96_000.0 + if left < 15 * MIN { 2_000.0 * (15 * MIN - left) as f64 / (15 * MIN) as f64 } else { 0.0 };
            let oi = 1_000.0 * (1.0 + 0.2 * ts as f64 / end as f64);
            s.ingest(ts, vec![t(E::Bybit, false, "PUMP", 2.0 * up(60_000), vol, oi), t(E::Gate, false, "PUMP", 2.0 * up(0) * 1.001, vol, oi), t(E::Okx, true, "PUMP", 2.0 * up(0) * 0.999, vol, 0.0)]);
        }
        s.liq("PUMP".into(), end - 20 * MIN, 1e6);
        s.liq("PUMP".into(), end - MIN, 5e5);
        (s, end)
    }

    #[test]
    fn rows_from_canned_samples() {
        let (mut s, now) = scan();
        let r = s.rows(now).pop().unwrap();
        assert_eq!(r.base, "PUMP");
        assert_eq!(r.venues, vec![E::Bybit, E::Gate]);
        // both perps ended +10%; 5 min ago Bybit was already +2% (median of 7.84% and 10%)
        assert!((r.chg[0].unwrap() - (1.1 / 1.02 - 1.0 + 0.1) * 50.0).abs() < 1e-6, "{:?}", r.chg);
        for c in &r.chg[1..] { assert!((c.unwrap() - 10.0).abs() < 1e-6, "{c:?}"); }
        // 2 000 more in the rolling day plus the 15 min that left it (a day's average, 98 000 / 96)
        assert!((r.vmult.unwrap() - (1.0 + 2000.0 * 96.0 / 98_000.0)).abs() < 1e-3, "{:?}", r.vmult);
        let oi1h = r.oi_chg[1].unwrap();
        assert!((oi1h - 100.0 * (1.2 / (1.0 + 0.2 / 61.0) - 1.0)).abs() < 0.5, "{oi1h}");
        // only the liquidation inside 15 min (Bybit and Gate both have a feed)
        assert_eq!(r.liq15, Some(5e5));
        // Gate perp 0.1% over the Okx spot = 0.2% = 20 bp; spot vs spot never counts
        let g = r.gap.unwrap();
        assert!((g.bp - 20.0).abs() < 0.5 && g.hi == (E::Gate, false) && g.lo == (E::Okx, true), "{g:?}");
        // Bybit started a minute before the others
        let l = r.lead.unwrap();
        assert!(l.ex == E::Bybit && !l.spot && (l.secs - 60.0).abs() < 1.0, "{l:?}");
        assert!(r.odd.is_empty() && r.flags.is_empty());
    }

    #[test]
    fn liquidations_count_where_a_feed_exists() {
        let (mut s, now) = scan();
        // Bitget, MEXC and Hyperliquid publish no liquidations: no figure (not zero) for a coin only they list
        s.ingest(now, vec![t(E::Bitget, false, "QUIET", 1.0, 1e6, 0.0), t(E::Hyperliquid, false, "QUIET", 1.0, 1e6, 0.0)]);
        let rows = s.rows(now);
        let liq = |b: &str| rows.iter().find(|r| r.base == b).unwrap().liq15;
        assert_eq!((liq("PUMP"), liq("QUIET")), (Some(5e5), None));
    }

    /// Binance has no all-symbol OI: its 5 min history (13 points, the last two minutes old) joins
    /// the sampled venues' sums, each venue measured against its own earlier value.
    #[test]
    fn open_interest_history_joins_the_sum() {
        let (mut s, now) = scan();
        s.ingest(now, vec![t(E::Binance, false, "PUMP", 2.2, 96_000.0, 0.0)]);
        let own = s.rows(now)[0].oi_chg;
        // no history yet: Bybit and Gate alone
        assert!((own[1].unwrap() - 100.0 * (1.2 / (1.0 + 0.2 / 61.0) - 1.0)).abs() < 0.5);
        let pts: Vec<(i64, f64)> = (0..13).map(|k| (now - 2 * MIN - (12 - k) * 5 * MIN, 2000.0 + 100.0 * k as f64)).collect();
        s.oi_hist(E::Binance, "PUMP", pts);
        // another venue's history is never attached to a coin it does not list
        s.oi_hist(E::Binance, "NOPE", vec![(now, 1.0)]);
        let r = s.rows(now).pop().unwrap();
        let (b1h, b15) = (1000.0 * (1.0 + 0.2 / 61.0), 1000.0 * (1.0 + 0.2 * 46.0 / 61.0));
        let want = [(2.0 * 1200.0 + 3200.0) / (2.0 * b15 + 2900.0), (2.0 * 1200.0 + 3200.0) / (2.0 * b1h + 2000.0)].map(|x| (x - 1.0) * 100.0);
        // samples hold OI as f32
        assert!((r.oi_chg[0].unwrap() - want[0]).abs() < 1e-4 && (r.oi_chg[1].unwrap() - want[1]).abs() < 1e-4, "{:?} {want:?}", r.oi_chg);
        // a history that stopped coming (newest point over 15 min old) drops out
        let old: Vec<(i64, f64)> = (0..13).map(|k| (now - 20 * MIN - (12 - k) * 5 * MIN, 2000.0)).collect();
        s.oi_hist(E::Binance, "PUMP", old);
        assert!((s.rows(now)[0].oi_chg[1].unwrap() - own[1].unwrap()).abs() < 1e-9);
        let p = oi_points(&json!([{"sumOpenInterest": "93699.12", "timestamp": 2}, {"sumOpenInterest": "0", "timestamp": 1}, {"sumOpenInterest": "5.5", "timestamp": 0}]));
        assert_eq!(p, vec![(0, 5.5), (2, 93699.12)]);
    }

    #[test]
    fn another_asset_under_the_same_name_is_left_out() {
        let mut s = Scan::default();
        s.ingest(0, vec![t(E::Binance, false, "TWIN", 1.0, 1e6, 0.0), t(E::Bybit, false, "TWIN", 1.01, 1e6, 0.0), t(E::Gate, false, "TWIN", 7.0, 1e6, 0.0),
            // a dead spot market's last trade is no price
            t(E::Okx, true, "TWIN", 1.05, 10.0, 0.0)]);
        let r = &s.rows(0)[0];
        assert_eq!((r.odd.clone(), r.venues.clone()), (vec![(E::Gate, false)], vec![E::Binance, E::Bybit]));
        assert!((r.gap.unwrap().bp - 100.0).abs() < 1e-6, "{:?}", r.gap);
        // two venues far apart: both flagged, both kept, no gap
        let mut s = Scan::default();
        s.ingest(0, vec![t(E::Binance, false, "TWO", 1.0, 1.0, 0.0), t(E::Gate, false, "TWO", 7.0, 1.0, 0.0)]);
        let r = &s.rows(0)[0];
        assert!(r.odd.len() == 2 && r.venues.len() == 2 && r.gap.is_none());
        // stale venues drop out, a coin without a live perp has no row
        assert!(s.rows(STALE_MS + 1).is_empty());
    }

    #[test]
    fn alerts_fire_once_per_coin_per_cooldown() {
        let (mut s, now) = scan();
        let rows = s.rows(now);
        let mut a = Alerts { min_vol: 0.0, ..Default::default() };
        assert!(check(&a, &rows, now, &mut HashMap::new()).is_empty());
        a.rules = [(true, 5.0), (true, 2.5), (true, 0.05), (true, 50.0)];
        // +10% in 5 min, x3 volume; funding unknown, gap 20 bp
        assert_eq!(crossed(&a, &rows[0]), vec![0, 1]);
        let mut last = HashMap::new();
        assert_eq!(check(&a, &rows, now, &mut last).len(), 1);
        assert!(check(&a, &rows, now + COOL_MS - 1, &mut last).is_empty());
        assert_eq!(check(&a, &rows, now + COOL_MS, &mut last).len(), 1);
        // below the volume floor: nothing
        a.min_vol = 1e12;
        assert!(check(&a, &rows, now + 3 * COOL_MS, &mut HashMap::new()).is_empty());
        let r = Row { fund: Some((E::Bybit, -0.0006)), ..rows[0].clone() };
        a.rules = [(false, 0.0), (false, 0.0), (true, 0.05), (false, 0.0)];
        assert_eq!(crossed(&a, &r), vec![2]);
    }

    fn hours(n: usize, now: i64, f: impl Fn(usize) -> (f64, f64, f64, f64)) -> Vec<HourBar> {
        (0..n).map(|i| { let (p, h, l, qv) = f(i); HourBar { ts: now - (n - i) as i64 * HOUR, o: p, h, l, c: p, qv } }).collect()
    }

    #[test]
    fn operator_score_from_hourly_series() {
        let now = 1000 * DAY;
        // an ordinary coin: flat, steady volume, spot half the perp turnover, listed long ago
        let calm = hours(30 * 24, now, |i| { let p = 1.0 + 0.001 * (i % 2) as f64; (p, p * 1.002, p * 0.998, 100.0) });
        let Op::Score { score, parts } = op_score(&calm, now, 1e7, 5e6, Some(now - 400 * DAY)) else { panic!() };
        assert!(score == 0.0 && parts.len() == 6, "{parts:?}");
        // the operator type: a 30x hour every other day, a two-sided wick every 60 h, a week from 1 to 3, a crash, no spot, new
        let op = hours(30 * 24, now, |i| {
            let p = match i { 0..240 => 1.0, 240..408 => 1.0 + 2.0 * (i - 240) as f64 / 168.0, 408..432 => 3.0, _ => 1.2 };
            let (h, l) = if i % 60 == 7 { (p * 1.08, p * 0.92) } else { (p * 1.002, p * 0.998) };
            (p, h, l, if i % 48 == 5 { 3000.0 } else { 100.0 })
        });
        let Op::Score { score, parts } = op_score(&op, now, 1e7, 0.0, Some(now - 20 * DAY)) else { panic!() };
        assert!(score == 100.0, "{parts:?}");
        let get = |k: &str| parts.iter().find(|p| p.0 == k).unwrap().1;
        assert!(get("spikes") == 1.0 && get("wicks") == 1.0 && get("days") >= 2.0 && get("perp").is_infinite() && (get("runup") - 2.0).abs() < 0.02 && (get("new") - 20.0).abs() < 1e-9);
        // no listing date: five traits; under a week of hours: collecting
        let Op::Score { score, parts } = op_score(&op, now, 1e7, 1e7, None) else { panic!() };
        assert!(parts.len() == 5 && score == 80.0, "{parts:?}");
        assert_eq!(op_score(&op[..5 * 24 + 3], now, 1e7, 0.0, None), Op::Collecting(5));
        // a coin's hours come from its venue with the most; unrecorded coins without hours get none
        let bars = HashMap::from([("OP".to_string(), op.iter().map(|b| (E::Bybit, *b)).chain(calm[..200].iter().map(|b| (E::Binance, *b))).collect::<Vec<_>>())]);
        let vols = HashMap::from([("OP".to_string(), (1e7, 0.0)), ("NEW".to_string(), (2e6, 0.0)), ("DUST".to_string(), (1e5, 0.0))]);
        let s = op_scores(&bars, &vols, &HashMap::new(), now);
        assert!(matches!(s["OP"], Op::Score { score, .. } if score == 100.0) && s["NEW"] == Op::Collecting(0) && !s.contains_key("DUST"));
    }

    #[test]
    fn hourly_bars_from_samples() {
        let mut s = Scan::default();
        let h0 = 10 * HOUR;
        // every 30 s from a minute before the hour to a minute after: price 1 -> 2, 1 000 more in the rolling day
        for k in -2..=122 {
            let ts = h0 + k * 30_000;
            let f = ((ts - h0) as f64 / HOUR as f64).clamp(0.0, 1.0);
            let (px, vol) = (1.0 + f, 96_000.0 + 1_000.0 * f);
            let mut ticks = vec![Tick { qvol: Some(5e6), ..t(E::Bybit, false, "REC", px, vol, 0.0) }, Tick { qvol: Some(1e6), ..t(E::Binance, false, "REC", px * 1.01, vol, 0.0) },
                Tick { qvol: Some(1e5), ..t(E::Gate, false, "THIN", px, vol, 0.0) }];
            // a 7 min hole in the hour: no bar
            if !(10 * MIN..17 * MIN).contains(&(ts - h0)) { ticks.push(Tick { qvol: Some(5e6), ..t(E::Gate, false, "HOLE", px, vol, 0.0) }); }
            s.ingest(ts, ticks);
        }
        let bars = s.hour_bars(h0);
        // Binance first, though Bybit trades more; under the volume floor or with a hole: nothing
        assert_eq!(bars.len(), 1, "{bars:?}");
        let (ex, base, b) = &bars[0];
        assert_eq!((*ex, base.as_str(), b.ts), (E::Binance, "REC", h0));
        assert!((b.o - 1.01).abs() < 1e-9 && (b.c - 2.02).abs() < 1e-9 && (b.h - 2.02).abs() < 1e-9 && (b.l - 1.01).abs() < 1e-9, "{b:?}");
        // 1 000 more in the day plus the hour that left it (97 000 / 24), at the hour's mean price
        assert!((b.qv - (1_000.0 + 97_000.0 / 24.0) * 1.515).abs() < 1.0, "{b:?}");
        // the next hour is not over
        assert!(s.hour_bars(h0 + HOUR).is_empty());
        let k = json!([[h0, "1", "2", "0.5", "1.5", "10", h0 + HOUR - 1, "15", 3], [h0 + HOUR, "1.5", "1.6", "1.4", "1.6", "1", h0 + 2 * HOUR - 1, "1.6", 1]]);
        assert_eq!(kline_bars(&k, h0 + HOUR + 1), vec![HourBar { ts: h0, o: 1.0, h: 2.0, l: 0.5, c: 1.5, qv: 15.0 }]);
    }

    /// Each venue's own flag (canned payloads cut from the live lists) marks its stocks, ETFs and
    /// commodities; any venue's flag drops the coin everywhere, rows already built included.
    #[test]
    fn tradfi_flags_drop_the_coin_on_every_venue() {
        let mut s = Scan::default();
        let tick = |ex: E, b: &str| Tick { px: 10.0, qvol: Some(5e6), ..tick(ex, false, b.into()) };
        s.ingest(0, vec![tick(E::Binance, "XAG"), tick(E::Bybit, "XAG"), tick(E::Binance, "TSLA"), tick(E::Okx, "CL"), tick(E::Gate, "SPY"), tick(E::Binance, "PEPE"),
            tick(E::Mexc, "AAPL"), tick(E::Bitget, "MU"), tick(E::Bybit, "AMD"), tick(E::Binance, "AMD")]);
        s.liq("TSLA".into(), 0, 1e6);
        assert_eq!(s.rows(0).len(), 8);
        let m = &mut s.meta;
        parse(Src::BinanceInfo, &json!({"symbols": [
            {"symbol": "XAGUSDT", "baseAsset": "XAG", "contractType": "TRADIFI_PERPETUAL", "underlyingType": "COMMODITY", "underlyingSubType": ["TradFi"]},
            {"symbol": "TSLAUSDT", "baseAsset": "TSLA", "contractType": "TRADIFI_PERPETUAL", "underlyingType": "EQUITY", "underlyingSubType": ["TradFi"]},
            {"symbol": "PEPEUSDT", "baseAsset": "PEPE", "contractType": "PERPETUAL", "underlyingType": "COIN", "underlyingSubType": ["Meme"]},
            {"symbol": "BTCDOMUSDT", "baseAsset": "BTCDOM", "contractType": "PERPETUAL", "underlyingType": "INDEX", "underlyingSubType": ["Index", "Crypto"]}]}), m);
        // Bybit: AMDSTOCK is Bybit's name of AMD; innovation-zone coins and untyped ones are crypto
        parse(Src::BybitInfo, &json!({"result": {"list": [
            {"symbol": "AMDSTOCKUSDT", "baseCoin": "AMDSTOCK", "symbolType": "stock"}, {"symbol": "KORUUSDT", "baseCoin": "KORU", "symbolType": "ETF"},
            {"symbol": "BZUSDT", "baseCoin": "BZ", "symbolType": "commodity"}, {"symbol": "EURUSDUSDT", "baseCoin": "EURUSD", "symbolType": "forex"},
            {"symbol": "AGIUSDT", "baseCoin": "AGI", "symbolType": "innovation"}, {"symbol": "0GUSDT", "baseCoin": "0G", "symbolType": ""}]}}), m);
        parse(Src::OkxInfo, &json!({"data": [{"instId": "CL-USDT-SWAP", "instCategory": "4"}, {"instId": "NVDA-USDT-SWAP", "instCategory": "3"},
            {"instId": "BTC-USDT-SWAP", "instCategory": "1"}, {"instId": "BTC-USD-SWAP", "instCategory": "1"}]}), m);
        parse(Src::BitgetMeta, &json!({"data": [{"symbol": "MUUSDT", "isRwa": "YES"}, {"symbol": "BTCUSDT", "isRwa": "NO"}]}), m);
        parse(Src::GateMeta, &json!([{"name": "SPY_USDT", "contract_type": "stocks"}, {"name": "XAU_USDT", "contract_type": "metals"}, {"name": "BTC_USDT", "contract_type": ""}]), m);
        parse(Src::MexcMeta, &json!({"data": [{"symbol": "AAPLSTOCK_USDT", "conceptPlate": ["mc-trade-zone-Stock", "mc-trade-zone-tradfi"]},
            {"symbol": "BTC_USDT", "conceptPlate": ["mc-trade-zone-mainly"]}, {"symbol": "XAU_USDT", "conceptPlate": ["mc-trade-zone-metals", "mc-trade-zone-tradfi"]}]}), m);
        let mut flagged: Vec<&str> = m.tradfi.iter().map(|b| b.as_str()).collect();
        flagged.sort();
        assert_eq!(flagged, vec!["AAPL", "AMD", "BZ", "CL", "EURUSD", "KORU", "MU", "NVDA", "SPY", "TSLA", "XAG", "XAU"]);
        // the next payload clears what was already scanned (also the liquidations) and refuses new ticks
        s.ingest(1000, vec![tick(E::Bybit, "TSLA"), tick(E::Binance, "PEPE")]);
        s.liq("TSLA".into(), 1000, 1e6);
        assert_eq!(s.rows(1000).iter().map(|r| r.base.as_str()).collect::<Vec<_>>(), vec!["PEPE"]);
        assert!(s.liq.is_empty());
    }

    #[test]
    fn venue_payloads_parse() {
        let mut m = Meta::default();
        let one = |src: Src, v: Value, m: &mut Meta| { let mut x = parse(src, &v, m); assert_eq!(x.len(), 1, "{src:?}: {x:?}"); x.pop().unwrap() };
        let bybit = json!({"result": {"list": [
            {"symbol": "BTCUSDT", "lastPrice": "80311.20", "openInterest": "61574.585", "singleOpenInterest": "30787.293", "turnover24h": "3314633146.8611", "volume24h": "41379.8840", "fundingRate": "0.0001", "fundingIntervalHour": "8"},
            {"symbol": "BTC-26DEC25", "lastPrice": "81000"}, {"symbol": "BTCPERP", "lastPrice": "80300"}]}});
        let x = one(Src::BybitPerp, bybit, &mut m);
        assert_eq!((x.base.as_str(), x.px, x.oi, x.fund), ("BTC", 80311.2, Some(30787.293), Some(0.0001 / 8.0)));
        // no singleOpenInterest: half of the two-sided figure; a stock under its venue name joins the unified coin
        let x = one(Src::BybitPerp, json!({"result": {"list": [{"symbol": "AMDSTOCKUSDT", "lastPrice": "520.57", "openInterest": "2905.36"}]}}), &mut m);
        assert_eq!((x.base.as_str(), x.oi, x.fund), ("AMD", Some(1452.68), None));
        let x = one(Src::BybitSpot, json!({"result": {"list": [{"symbol": "BTCUSDT", "lastPrice": "80346.4", "turnover24h": "613088792.66", "volume24h": "7649.609454"}, {"symbol": "ETHUSDC", "lastPrice": "2326.29"}]}}), &mut m);
        assert!(x.spot && x.base == "BTC" && x.qvol == Some(613088792.66));

        let x = one(Src::OkxPerp, json!({"data": [{"instId": "PEPE-USDT-SWAP", "last": "0.000004357", "volCcy24h": "1000000000", "vol24h": "100"}, {"instId": "BTC-USD-SWAP", "last": "80000"}]}), &mut m);
        assert_eq!((x.base.as_str(), x.bvol), ("PEPE", Some(1e9)));
        assert!((x.qvol.unwrap() - 4357.0).abs() < 1e-6);
        let x = one(Src::OkxSpot, json!({"data": [{"instId": "BTC-USDT", "last": "80000", "vol24h": "10", "volCcy24h": "800000"}]}), &mut m);
        assert_eq!((x.bvol, x.qvol), (Some(10.0), Some(800000.0)));
        let x = one(Src::OkxOi, json!({"data": [{"instId": "PEPE-USDT-SWAP", "oi": "730546.3", "oiCcy": "7305463000000", "oiUsd": "31827710.65"}]}), &mut m);
        assert_eq!((x.px, x.oi), (0.0, Some(7305463000000.0)));
        assert!((m.size[&(E::Okx, "PEPE-USDT-SWAP".into())] - 1e7).abs() < 1.0);
        let x = one(Src::OkxFunding, json!({"data": [{"instId": "CHIP-USDT-SWAP", "fundingRate": "0.00005", "fundingTime": "1793016000000", "nextFundingTime": "1793030400000", "prevFundingTime": "1793001600000"}]}), &mut m);
        assert_eq!(x.fund, Some(0.00005 / 4.0));
        let l = liqs(E::Okx, &json!({"data": [{"instId": "PEPE-USDT-SWAP", "details": [{"bkPx": "0.000004", "sz": "10", "side": "sell", "ts": "1793015422591"}]}, {"instId": "XYZ-USDT-SWAP", "details": [{"bkPx": "1", "sz": "1", "ts": "1"}]}]}), &m);
        assert_eq!(l.len(), 1);
        assert!((l[0].2 - 400.0).abs() < 1e-3 && l[0].1 == 1793015422591);

        // Bitget: funding per hour once the contracts (intervals) are known
        assert!(parse(Src::BitgetMeta, &json!({"data": [{"symbol": "BTCUSDT", "fundInterval": "8"}, {"symbol": "PEPEUSDT", "fundInterval": "4"}]}), &mut m).is_empty());
        let x = one(Src::BitgetPerp, json!({"data": [{"symbol": "PEPEUSDT", "lastPr": "0.0000043569", "baseVolume": "7321286393000", "quoteVolume": "31985811.78", "fundingRate": "0.0001", "holdingAmount": "7846527506578"}]}), &mut m);
        assert_eq!((x.base.as_str(), x.oi, x.fund), ("PEPE", Some(7846527506578.0), Some(0.0001 / 4.0)));
        let x = one(Src::BitgetSpot, json!({"data": [{"symbol": "PEPEUSDT", "lastPr": "0.0000043", "quoteVolume": "1000", "baseVolume": "2"}]}), &mut m);
        assert!(x.spot && x.qvol == Some(1000.0));

        assert!(parse(Src::GateMeta, &json!([{"name": "BTC_USDT", "quanto_multiplier": "0.0001", "funding_interval": 28800, "launch_time": 1574035200}]), &mut m).is_empty());
        let x = one(Src::GatePerp, json!([{"contract": "BTC_USDT", "last": "80306.8", "volume_24h_base": "48037", "volume_24h_quote": "3847719007", "funding_rate": "0.000068", "total_size": "575451480", "quanto_multiplier": "0.0001"}]), &mut m);
        assert_eq!((x.bvol, x.qvol, x.fund), (Some(48037.0), Some(3847719007.0), Some(0.000068 / 8.0)));
        assert!((x.oi.unwrap() - 57545.148).abs() < 1e-6);
        let x = one(Src::GateSpot, json!([{"currency_pair": "BTC_USDT", "last": "80300", "base_volume": "1", "quote_volume": "80300"}, {"currency_pair": "BTC_USDC", "last": "80300"}]), &mut m);
        assert!(x.spot && x.base == "BTC");

        // MEXC: nothing before the contract sizes; funding comes with its own interval (hours), none without one
        let f = parse(Src::MexcFunding, &json!({"data": [{"symbol": "BTC_USDT", "fundingRate": 0.000079, "collectCycle": 8}, {"symbol": "XAU_USDT", "fundingRate": 0.000029, "collectCycle": 4},
            {"symbol": "ODD_USDT", "fundingRate": 0.001}, {"symbol": "BTC_USDC", "fundingRate": 0.1, "collectCycle": 8}]}), &mut m);
        assert_eq!(f.iter().map(|t| (t.base.as_str(), t.fund)).collect::<Vec<_>>(), vec![("BTC", Some(0.000079 / 8.0)), ("XAU", Some(0.000029 / 4.0)), ("ODD", None)]);
        let mexc = json!({"data": [{"symbol": "BTC_USDT", "lastPrice": 80325.3, "volume24": 274644274, "amount24": 2202578850.54, "holdVol": 534683085, "fundingRate": 8.7e-05}]});
        assert!(parse(Src::MexcPerp, &mexc, &mut m).is_empty());
        parse(Src::MexcMeta, &json!({"data": [{"symbol": "BTC_USDT", "contractSize": 0.0001, "createTime": 1591242684000i64}]}), &mut m);
        // the oldest listing a venue reports (Gate, seconds) wins over a later one (MEXC, ms)
        assert_eq!(m.listed["BTC"], 1574035200000);
        let x = one(Src::MexcPerp, mexc, &mut m);
        assert!((x.oi.unwrap() - 53468.3085).abs() < 1e-6 && (x.bvol.unwrap() - 27464.4274).abs() < 1e-6 && x.fund.is_none());

        let hl = json!([{"universe": [{"name": "BTC"}, {"name": "MATIC", "isDelisted": true}, {"name": "xyz:GOLD"}]},
            [{"funding": "0.0000125", "openInterest": "42046.63642", "dayNtlVlm": "1828993275.75", "markPx": "80358.0", "midPx": "80360.5", "dayBaseVlm": "22818.56"},
             {"funding": "0.0", "markPx": "0.37621", "midPx": null}, {"funding": "0.00001", "markPx": "4000", "midPx": null}]]);
        let x = parse(Src::HlPerp, &hl, &mut m);
        assert_eq!(x.iter().map(|t| (t.base.as_str(), t.px)).collect::<Vec<_>>(), vec![("BTC", 80360.5), ("XAU", 4000.0)]);
        assert_eq!(x[0].fund, Some(0.0000125));

        // Binance: the all-market streams, funding per hour once the intervals are known
        let x = one(Src::BinanceMini, json!([{"e": "24hrMiniTicker", "s": "BTCUSDT", "c": "80300", "o": "80000", "v": "100", "q": "8030000"}, {"s": "BTCUSDT_261225", "c": "81000"}]), &mut m);
        assert_eq!((x.px, x.qvol, x.bvol), (80300.0, Some(8030000.0), Some(100.0)));
        let mark = json!([{"e": "markPriceUpdate", "s": "BTCUSDT", "p": "80300", "r": "0.0001", "T": 1793030400000i64}]);
        assert_eq!(one(Src::BinanceMark, mark.clone(), &mut m).fund, None);
        m.hours.insert((E::Binance, "BTCUSDT".into()), 8.0);
        let x = one(Src::BinanceMark, mark, &mut m);
        assert_eq!((x.px, x.fund), (0.0, Some(0.0001 / 8.0)));
        let x = one(Src::BinanceSpot, json!([{"symbol": "BTCUSDT", "lastPrice": "80310", "volume": "5", "quoteVolume": "401550"}]), &mut m);
        assert!(x.spot && x.px == 80310.0);
        let l = liqs(E::Binance, &json!({"e": "forceOrder", "o": {"s": "BTCUSDT", "S": "SELL", "q": "0.5", "ap": "80000", "T": 1793015422591i64}}), &m);
        assert_eq!(l, vec![("BTC".to_string(), 1793015422591, 40000.0)]);
        // Bybit: base units at the bankruptcy price, USDT perps only
        let l = liqs(E::Bybit, &json!({"topic": "allLiquidation.ARBUSDT", "type": "snapshot", "data": [{"T": 1793019741489i64, "s": "ARBUSDT", "S": "Buy", "v": "9581.4", "p": "0.16857"},
            {"T": 1793019741490i64, "s": "ARBUSDC", "S": "Buy", "v": "1", "p": "1"}]}), &m);
        assert!(l.len() == 1 && l[0].0 == "ARB" && l[0].1 == 1793019741489 && (l[0].2 - 1615.1).abs() < 0.1, "{l:?}");
        // Gate: signed contracts x multiplier (BTC_USDT: 0.0001 from the contract list) x price; size 0 and unlisted contracts give none
        let l = liqs(E::Gate, &json!({"channel": "futures.public_liquidates", "event": "update", "result": [{"contract": "BTC_USDT", "price": "79973.7", "time": 1793019693243i64, "size": -1000},
            {"contract": "BTC_USDT", "price": "79973.7", "time": 1793019693244i64, "size": 0}, {"contract": "NEW_USDT", "price": "1", "time": 1793019693245i64, "size": -5}]}), &m);
        assert!(l.len() == 1 && l[0].0 == "BTC" && l[0].1 == 1793019693243 && (l[0].2 - 7997.37).abs() < 1e-6, "{l:?}");
    }
}
