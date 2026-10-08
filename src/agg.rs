//! Aggregation above the connectors: per-venue state, cross-venue metrics (CVD, basis,
//! OI sum, OI-weighted funding, liquidations), minute bars for chart overlays, book heatmap
//! samples and option chains with forwards.
//!
//! One `Agg` per base asset (e.g. BTC). Feed it every `Msg` for that asset via `on`, and call
//! `sample_heatmap` about once a second.
use crate::*;
use std::collections::{HashMap, VecDeque};

pub const BAR_MS: i64 = 60_000;
/// minutes of venue volume used to weight the composite price
const INDEX_MIN: i64 = 60;
/// a venue whose book/BBO is this far behind the freshest venue of its market is left out of
/// cross-venue prices and books (a dead socket keeps its last book until the reconnect)
const STALE_MS: i64 = 10_000;
/// live updates kept per venue while its history loads
const PENDING_CAP: usize = 200_000;
/// 7 days of minute bars per series
const BAR_CAP: usize = 7 * 24 * 60;
/// 2 hours of one-second heatmap columns
const HEAT_CAP: usize = 2 * 3600;
/// heatmap / aggregated book range around mid
const HEAT_RANGE: f64 = 0.02;

#[derive(Default)]
pub struct Venue {
    pub symbol: Option<Arc<str>>,
    pub book: Book,
    /// bid, bid_qty, ask, ask_qty
    pub bbo: Option<[f64; 4]>,
    pub last: Option<f64>,
    pub mark: Option<f64>,
    pub index: Option<f64>,
    /// predicted hourly rate for the next settlement (live)
    pub funding_h: Option<f64>,
    /// last settled hourly rate (history, then the prediction in force when a settlement passes)
    pub funding_settled: Option<f64>,
    pub next_funding_ms: Option<i64>,
    /// base units
    pub oi: Option<f64>,
    pub ls: HashMap<LsKind, f64>,
    pub borrow_apr: HashMap<String, f64>,
    /// cumulative aggressor buy - sell volume since start, base units
    pub cvd: f64,
    /// cumulative liquidated (longs, shorts), base units
    pub liq: (f64, f64),
    pub ts: i64,
    /// local receive time of the last book/BBO update
    pub fresh: i64,
}

impl Venue {
    pub fn mid(&self) -> Option<f64> {
        if let Some([b, _, a, _]) = self.bbo { if b > 0.0 && a > 0.0 { return Some((b + a) / 2.0); } }
        match (self.book.best_bid(), self.book.best_ask()) {
            (Some(b), Some(a)) => Some((b.0 + a.0) / 2.0),
            _ => self.last,
        }
    }
    /// Mid of the depth book itself (None when a side is empty or the book is crossed).
    /// Aligning a book must use this, not `mid()`: the BBO stream runs ahead of depth snapshots,
    /// and shifting snapshot levels by a newer BBO mid makes the merged book cross.
    pub fn book_mid(&self) -> Option<f64> {
        let (b, a) = (self.book.best_bid()?.0, self.book.best_ask()?.0);
        (b > 0.0 && a > b).then_some((b + a) / 2.0)
    }
    pub fn price(&self) -> Option<f64> { self.mark.or_else(|| self.mid()) }
    pub fn oi_usd(&self) -> Option<f64> { Some(self.oi? * self.price()?) }
}

/// One minute of chart data. State fields (cvd, oi, funding, basis, ls) carry forward into
/// bars that have no new update; flow fields (volumes, liquidations) start at zero.
#[derive(Clone, Debug, Default)]
pub struct Bar {
    pub t: i64,
    pub o: f64, pub h: f64, pub l: f64, pub c: f64,
    /// total base volume; buy/sell hold the aggressor split when known (live trades always,
    /// history only where the venue publishes it), so buy + sell may be less than vol
    pub vol: f64,
    pub buy: f64, pub sell: f64,
    pub cvd: f64,
    pub oi: Option<f64>,
    /// predicted hourly funding (live only: venues publish no prediction history)
    pub funding_h: Option<f64>,
    /// settled hourly funding, a step line from history onward
    pub funding_settled: Option<f64>,
    pub basis_bps: Option<f64>,
    pub liq_long: f64, pub liq_short: f64,
    pub ls: Option<f64>,
}

#[derive(Default)]
pub struct Series { pub bars: VecDeque<Bar> }

impl Series {
    /// Bar for the bucket containing `ts`, creating it if it is newer than the last one.
    /// Messages older than the window are dropped (None).
    pub fn at(&mut self, ts: i64) -> Option<&mut Bar> {
        let t = ts.div_euclid(BAR_MS) * BAR_MS;
        match self.bars.back().map(|b| b.t) {
            Some(last) if t < last => return self.bars.iter_mut().rev().find(|b| b.t == t),
            Some(last) if t == last => {}
            _ => {
                let mut n = Bar { t, ..Default::default() };
                if let Some(p) = self.bars.back() {
                    n = Bar { t, o: p.c, h: p.c, l: p.c, c: p.c, cvd: p.cvd, oi: p.oi, funding_h: p.funding_h, funding_settled: p.funding_settled,
                              basis_bps: p.basis_bps, ls: p.ls, ..Default::default() };
                }
                self.bars.push_back(n);
                if self.bars.len() > BAR_CAP { self.bars.pop_front(); }
            }
        }
        self.bars.back_mut()
    }
}

impl Series {
    /// A late trade landed in bar `t`: every later bar's CVD moves with it.
    fn shift_cvd_after(&mut self, t: i64, delta: f64) {
        for b in self.bars.iter_mut().rev().take_while(|b| b.t > t) { b.cvd += delta; }
    }

    /// Prepend history older than the first live bar. Live bars keep their data; their CVD is
    /// shifted by the history's final CVD so the curve is continuous. Returns that offset.
    pub fn seed(&mut self, mut hist: Vec<Bar>) -> f64 {
        if let Some(first) = self.bars.front().map(|b| b.t) { hist.retain(|b| b.t < first); }
        let offset = hist.last().map(|b| b.cvd).unwrap_or(0.0);
        for b in self.bars.iter_mut() { b.cvd += offset; }
        for b in hist.into_iter().rev() { self.bars.push_front(b); }
        while self.bars.len() > BAR_CAP { self.bars.pop_front(); }
        offset
    }
}

/// Venue history -> minute bars: klines, aggressor split (candle field or taker series),
/// cumulative CVD from zero, and forward-filled OI / funding / long-short.
pub fn history_bars(h: &History) -> Vec<Bar> { history_bars_tf(h, BAR_MS) }

/// `history_bars` for candles `bar_ms` wide.
pub fn history_bars_tf(h: &History, bar_ms: i64) -> Vec<Bar> {
    let mut bars: Vec<Bar> = h.klines.iter().map(|k| {
        let (buy, sell) = k.buy.map(|b| (b, (k.vol - b).max(0.0))).unwrap_or((0.0, 0.0));
        Bar { t: k.t.div_euclid(bar_ms) * bar_ms, o: k.o, h: k.h, l: k.l, c: k.c, vol: k.vol, buy, sell, ..Default::default() }
    }).collect();
    let split_known = h.klines.iter().any(|k| k.buy.is_some());
    if !split_known {
        // ponytail: taker history is coarser (5m); its whole bucket lands on the bucket's first minute
        for &(t, buy, sell) in &h.taker {
            let t = t.div_euclid(bar_ms) * bar_ms;
            if let Ok(i) = bars.binary_search_by_key(&t, |b| b.t) { bars[i].buy += buy; bars[i].sell += sell; }
        }
    }
    let mut cvd = 0.0;
    for b in &mut bars { cvd += b.buy - b.sell; b.cvd = cvd; }
    fn ffill(bars: &mut [Bar], s: &[(i64, f64)], bar_ms: i64, set: impl Fn(&mut Bar, f64)) {
        let (mut j, mut cur) = (0, None);
        for b in bars {
            while j < s.len() && s[j].0 < b.t + bar_ms { cur = Some(s[j].1); j += 1; }
            if let Some(v) = cur { set(b, v); }
        }
    }
    ffill(&mut bars, &h.oi, bar_ms, |b, v| b.oi = Some(v));
    ffill(&mut bars, &h.funding, bar_ms, |b, v| b.funding_settled = Some(v));
    ffill(&mut bars, &h.long_short, bar_ms, |b, v| b.ls = Some(v));
    bars
}

fn add_flow(b: &mut Bar, qty: f64, side: Side) {
    b.vol += qty;
    match side { Side::Buy => { b.buy += qty; b.cvd += qty } Side::Sell => { b.sell += qty; b.cvd -= qty } }
}

fn add_trade(b: &mut Bar, px: f64, qty: f64, side: Side) {
    if b.vol == 0.0 { b.o = px; b.h = px; b.l = px; }
    b.vol += qty;
    b.h = b.h.max(px);
    b.l = b.l.min(px);
    b.c = px;
    match side { Side::Buy => { b.buy += qty; b.cvd += qty } Side::Sell => { b.sell += qty; b.cvd -= qty } }
}

/// Aggregated book level: total base qty in the price bin and the per-venue split.
#[derive(Clone, Debug)]
pub struct Level { pub px: f64, pub qty: f64, pub by: Vec<(Exchange, f64)> }

/// One-second snapshots of the aggregated book, binned by price, for the chart background.
#[derive(Default)]
pub struct Heatmap {
    pub bin: f64,
    /// (ts, [(price bin index, base qty)]); price = index * bin
    pub cols: VecDeque<(i64, Vec<(i64, f32)>)>,
}

/// Wall thresholds against the median level: enter above IN x, a wall already shown stays until
/// it drops under OUT x (hysteresis), and it must have stood at half its mean size or more for
/// PRESENT of the window (a flashed/spoofed order does not count).
const WALL_IN: f32 = 5.0;
const WALL_OUT: f32 = 4.0;
const WALL_PRESENT: f32 = 0.8;

impl Heatmap {
    /// Liquidity walls over the last `n` columns: (price, mean base qty) for up to two bids and two
    /// asks, largest first. Sizes are summed over a bin and its neighbours, so an order that
    /// hops between two bins as venue mids re-align still counts as one wall. `prev` holds the
    /// prices shown last time (hysteresis).
    pub fn walls(&self, mid: f64, n: usize, prev: &[f64]) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let cols: Vec<&Vec<(i64, f32)>> = self.cols.iter().rev().take(n).map(|(_, c)| c).collect();
        let none = (vec![], vec![]);
        if cols.len() < n.min(10) || self.bin <= 0.0 { return none; }
        let Some(lo) = cols.iter().flat_map(|c| c.iter().map(|x| x.0)).min() else { return none };
        let hi = cols.iter().flat_map(|c| c.iter().map(|x| x.0)).max().unwrap();
        let bins = (hi - lo + 1) as usize;
        if bins > 20_000 { return none; }
        // per column, the 3-bin windowed size
        let win: Vec<Vec<f32>> = cols.iter().map(|c| {
            let mut v = vec![0f32; bins + 2];
            for &(i, q) in c.iter() { let k = (i - lo) as usize + 1; v[k - 1] += q; v[k] += q; v[k + 1] += q; }
            v
        }).collect();
        let avg: Vec<f32> = (0..bins + 2).map(|k| win.iter().map(|v| v[k]).sum::<f32>() / win.len() as f32).collect();
        let mut nz: Vec<f32> = avg.iter().copied().filter(|q| *q > 0.0).collect();
        if nz.is_empty() { return none; }
        nz.sort_by(f32::total_cmp);
        let med = nz[nz.len() / 2];
        let mid_k = mid / self.bin - lo as f64 + 1.0;
        let (mut bids, mut asks) = (vec![], vec![]);
        for k in 1..bins + 1 {
            let a = avg[k];
            // local peak only; the touch (2 bins either side of mid) churns constantly
            if !(a >= avg[k - 1] && a > avg[k + 1]) || (k as f64 - mid_k).abs() <= 2.0 { continue; }
            let px = (lo + k as i64 - 1) as f64 * self.bin;
            let shown = prev.iter().any(|p| (p - px).abs() <= self.bin * 1.5);
            if a <= med * if shown { WALL_OUT } else { WALL_IN } { continue; }
            let present = win.iter().filter(|v| v[k] >= a * 0.5).count() as f32 / win.len() as f32;
            if present < WALL_PRESENT { continue; }
            if (k as f64) < mid_k { bids.push((px, a as f64)) } else { asks.push((px, a as f64)) }
        }
        for s in [&mut bids, &mut asks] { s.sort_by(|x, y| y.1.total_cmp(&x.1)); s.truncate(2); }
        (bids, asks)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Opt {
    pub has_info: bool,
    pub expiry_ms: i64,
    pub strike: f64,
    pub call: bool,
    /// USD
    pub mark: Option<f64>,
    pub index: Option<f64>,
    pub forward: Option<f64>,
    pub bid: Option<(f64, f64)>,
    pub ask: Option<(f64, f64)>,
    pub iv: Option<f64>,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    pub delta: f64, pub gamma: f64, pub vega: f64, pub theta: f64,
    pub last: Option<f64>,
}

#[derive(Default)]
pub struct Chain { pub opts: HashMap<Arc<str>, Opt> }

#[derive(Clone, Debug)]
pub struct Expiry {
    pub expiry_ms: i64,
    pub forward: Option<f64>,
    /// "venue" if published by the exchange, "parity" if derived from call/put marks
    pub forward_src: &'static str,
    pub atm_strike: f64,
    pub atm_iv: Option<f64>,
    pub n: usize,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() { return None; }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

impl Chain {
    /// Per-expiry forward and ATM IV, sorted by expiry.
    /// ponytail: parity forward ignores discounting (F = K + C - P); fine for short-dated crypto,
    /// add a rate term if long-dated accuracy matters.
    pub fn expiries(&self) -> Vec<Expiry> {
        let mut by: HashMap<i64, Vec<&Opt>> = HashMap::new();
        for o in self.opts.values().filter(|o| o.has_info) { by.entry(o.expiry_ms).or_default().push(o); }
        let mut out: Vec<Expiry> = by.into_iter().map(|(exp, os)| {
            let index = median(os.iter().filter_map(|o| o.index).collect());
            let published = median(os.iter().filter_map(|o| o.forward).collect());
            let parity = {
                let mut calls: HashMap<i64, f64> = HashMap::new();
                let mut puts: HashMap<i64, f64> = HashMap::new();
                for o in &os {
                    if let Some(m) = o.mark { (if o.call { &mut calls } else { &mut puts }).insert((o.strike * 1e6) as i64, m); }
                }
                median(calls.iter().filter_map(|(k, c)| {
                    let (p, kf) = (puts.get(k)?, *k as f64 / 1e6);
                    index.is_none_or(|ix| (kf / ix - 1.0).abs() < 0.10).then_some(kf + c - p)
                }).collect())
            };
            let (forward, forward_src) = match (published, parity) {
                (Some(f), _) => (Some(f), "venue"),
                (None, Some(f)) => (Some(f), "parity"),
                _ => (None, "-"),
            };
            let reference = forward.or(index).unwrap_or(0.0);
            let atm_strike = os.iter().map(|o| o.strike).min_by(|a, b| (a - reference).abs().total_cmp(&(b - reference).abs())).unwrap_or(0.0);
            let atm_iv = median(os.iter().filter(|o| o.strike == atm_strike).filter_map(|o| o.iv).filter(|v| *v > 0.0).collect());
            Expiry { expiry_ms: exp, forward, forward_src, atm_strike, atm_iv, n: os.len() }
        }).collect();
        out.sort_by_key(|e| e.expiry_ms);
        out
    }
}

/// Merge per-venue bars (each with its USD factor) into cross-venue bars: volumes, CVD, OI and
/// liquidations summed; funding and long/short OI-weighted; basis as the cross-venue median;
/// prices volume-weighted.
fn combine(venues: Vec<(f64, Vec<&Bar>)>) -> Vec<Bar> {
    let mut ts: Vec<i64> = venues.iter().flat_map(|(_, bars)| bars.iter().map(|b| b.t)).collect();
    ts.sort_unstable();
    ts.dedup();
    let maps: Vec<(f64, HashMap<i64, &Bar>)> = venues.iter().map(|(k, bars)| (*k, bars.iter().map(|b| (b.t, *b)).collect())).collect();
    // price weight: each venue's volume over its last INDEX_MIN minutes, so the composite moves
    // smoothly instead of hopping between venue clusters a few bp apart
    let weights: Vec<HashMap<i64, f64>> = venues.iter().map(|(_, bars)| {
        let (mut q, mut sum, mut out) = (VecDeque::new(), 0.0, HashMap::new());
        for b in bars {
            q.push_back((b.t, b.vol));
            sum += b.vol;
            while q.front().is_some_and(|(t, _)| *t <= b.t - INDEX_MIN * BAR_MS) { sum -= q.pop_front().unwrap().1; }
            out.insert(b.t, sum.max(1e-9));
        }
        out
    }).collect();
    let mut carry: Vec<Option<&Bar>> = vec![None; maps.len()];
    let mut out = Vec::with_capacity(ts.len());
    for t in ts {
        let mut b = Bar { t, ..Default::default() };
        let (mut o, mut h, mut l, mut c, mut wsum, mut basis) = (0.0, 0.0, 0.0, 0.0, 0.0, vec![]);
        let (mut oi, mut has_oi) = (0.0, false);
        let (mut fnum, mut fden, mut snum, mut sden, mut lnum, mut lden) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, (k, map)) in maps.iter().enumerate() {
            if let Some(x) = map.get(&t) {
                carry[i] = Some(x);
                b.vol += x.vol; b.buy += x.buy; b.sell += x.sell;
                b.liq_long += x.liq_long; b.liq_short += x.liq_short;
                if x.c > 0.0 {
                    let w = weights[i][&t];
                    o += x.o * k * w; h += x.h * k * w; l += x.l * k * w; c += x.c * k * w; wsum += w;
                }
            }
            if let Some(x) = carry[i] {
                b.cvd += x.cvd;
                if let Some(v) = x.oi {
                    oi += v;
                    has_oi = true;
                    let w = v * x.c * k;
                    if let Some(f) = x.funding_h { fnum += f * w; fden += w; }
                    if let Some(f) = x.funding_settled { snum += f * w; sden += w; }
                    if let Some(r) = x.ls { lnum += r * w; lden += w; }
                }
                if let Some(v) = x.basis_bps { basis.push(v); }
            }
        }
        let prev_c = out.last().map(|p: &Bar| p.c).unwrap_or(0.0);
        if wsum > 0.0 { b.o = o / wsum; b.h = h / wsum; b.l = l / wsum; b.c = c / wsum; } else { b.o = prev_c; b.h = prev_c; b.l = prev_c; b.c = prev_c; }
        b.oi = has_oi.then_some(oi);
        b.funding_h = (fden > 0.0).then(|| fnum / fden);
        b.funding_settled = (sden > 0.0).then(|| snum / sden);
        b.ls = (lden > 0.0).then(|| lnum / lden);
        b.basis_bps = median(basis);
        out.push(b);
    }
    out
}


#[derive(Default)]
pub struct Agg {
    pub venues: HashMap<(Exchange, Market), Venue>,
    /// key (Some(exchange), market) per venue, (None, market) aggregated across venues
    pub series: HashMap<(Option<Exchange>, Market), Series>,
    pub chains: HashMap<Exchange, Chain>,
    pub heat: HashMap<Market, Heatmap>,
    cvd: HashMap<Market, f64>,
    /// USD per unit of quote currency (e.g. "USDT" -> 0.9997); missing quotes count as 1.0
    fx: HashMap<&'static str, f64>,
    /// venues whose history is still loading: series-affecting live messages wait here
    pending: HashMap<(Exchange, Market), Vec<Msg>>,
    /// venues whose series were seeded from history
    seeded: std::collections::HashSet<(Exchange, Market)>,
    /// most recent trades across spot/perp venues, newest last
    pub trades: VecDeque<Tick>,
    /// native-period history for chart timeframes above 1m, keyed by minutes
    pub tf: HashMap<u32, TfSet>,
}

/// History at one chart timeframe: per-venue bars and the cross-venue bars built from them.
#[derive(Default)]
pub struct TfSet {
    pub venues: HashMap<(Exchange, Market), Vec<Bar>>,
    pub agg: HashMap<Market, Vec<Bar>>,
}

impl TfSet {
    pub fn bars(&self, src: Option<Exchange>, market: Market) -> Option<&Vec<Bar>> {
        match src { Some(e) => self.venues.get(&(e, market)), None => self.agg.get(&market) }
    }
}

/// One trade for the tape.
#[derive(Clone, Copy, Debug)]
pub struct Tick { pub ts: i64, pub ex: Exchange, pub market: Market, pub px: f64, pub qty: f64, pub side: Side }

const TAPE_CAP: usize = 400;

/// Quote currency of a venue's spot/perp book (after the connectors' quote mapping).
pub fn quote_of(ex: Exchange, _market: Market) -> &'static str {
    match ex {
        Exchange::Coinbase | Exchange::Kraken => "USD",
        Exchange::Hyperliquid | Exchange::Lighter => "USDC",
        _ => "USDT",
    }
}

impl Agg {
    pub fn on(&mut self, m: &Msg) {
        if m.market == Market::Option { return self.on_option(m); }
        if let Some(q) = self.pending.get_mut(&(m.ex, m.market)) {
            if matches!(m.ev, Event::Trade { .. } | Event::Mark { .. } | Event::OpenInterest { .. } | Event::Liquidation { .. } | Event::LongShort { .. }) {
                // bounded: a history fetch that never returns must not grow memory forever
                if q.len() >= PENDING_CAP {
                    q.drain(..PENDING_CAP / 2);
                    eprintln!("[agg] {:?} {:?} history still loading, dropped old queued updates", m.ex, m.market);
                }
                q.push(m.clone());
                return;
            }
        }
        let (ex, mk, ts) = (m.ex, m.market, m.ts);
        let Agg { venues, series, cvd, seeded, trades, .. } = self;
        let v = venues.entry((ex, mk)).or_default();
        if v.symbol.is_none() { v.symbol = Some(m.symbol.clone()); }
        v.ts = ts;
        match &m.ev {
            Event::Trade { px, qty, side } => {
                let signed = if *side == Side::Buy { *qty } else { -*qty };
                trades.push_back(Tick { ts, ex, market: mk, px: *px, qty: *qty, side: *side });
                if trades.len() > TAPE_CAP { trades.pop_front(); }
                // the cross-venue bar is in USD; the venue bar stays in native quote
                v.last = Some(*px);
                v.cvd += signed;
                let total = cvd.entry(mk).or_default();
                *total += signed;
                for k in [(Some(ex), mk), (None, mk)] {
                    let s = series.entry(k).or_default();
                    if let Some(b) = s.at(ts) {
                        // the venue bar takes the trade price; the cross-venue bar only its flow
                        // (its OHLC follows the composite index, see `touch_index`)
                        if k.0.is_some() { add_trade(b, *px, *qty, *side) } else { add_flow(b, *qty, *side) }
                        let t = b.t;
                        s.shift_cvd_after(t, signed);
                    }
                }
            }
            Event::Book { snapshot, bids, asks } => { v.book.apply(*snapshot, bids, asks); v.fresh = m.recv; }
            Event::Bbo { bid, bid_qty, ask, ask_qty } => { v.bbo = Some([*bid, *bid_qty, *ask, *ask_qty]); v.fresh = m.recv; }
            Event::Mark { mark, index, funding, next_funding_ms } => {
                v.mark = Some(*mark);
                if index.is_some() { v.index = *index; }
                // the settlement time moved forward: the prediction in force has just been settled
                if let (Some(prev), Some(next)) = (v.next_funding_ms, *next_funding_ms) {
                    if next > prev && v.funding_h.is_some() { v.funding_settled = v.funding_h; }
                }
                if funding.is_some() { v.funding_h = *funding; }
                if next_funding_ms.is_some() { v.next_funding_ms = *next_funding_ms; }
                if mk == Market::Perp { self.update_perp_bars(ex, ts); }
            }
            Event::OpenInterest { oi, .. } => {
                let first = v.oi.is_none();
                v.oi = Some(*oi);
                if first {
                    // ponytail: a venue without OI history gets its first live value as a flat line
                    // back through history, so the cross-venue OI does not jump when it first reports
                    let s = series.entry((Some(ex), mk)).or_default();
                    if seeded.contains(&(ex, mk)) && s.bars.iter().all(|b| b.oi.is_none()) {
                        for b in s.bars.iter_mut() { b.oi = Some(*oi); }
                        self.rebuild_agg(mk);
                        let tfs: Vec<u32> = self.tf.iter().filter(|(_, set)| set.venues.get(&(ex, mk)).is_some_and(|b| b.iter().all(|x| x.oi.is_none()))).map(|(t, _)| *t).collect();
                        for t in tfs {
                            if let Some(b) = self.tf.get_mut(&t).and_then(|s| s.venues.get_mut(&(ex, mk))) { for x in b.iter_mut() { x.oi = Some(*oi); } }
                            self.finish_history_tf(t);
                        }
                        return;
                    }
                }
                if let Some(b) = series.entry((Some(ex), mk)).or_default().at(ts) { b.oi = Some(*oi); }
                let total: f64 = venues.iter().filter(|((_, m2), _)| *m2 == mk).filter_map(|(_, v)| v.oi).sum();
                if let Some(b) = series.entry((None, mk)).or_default().at(ts) { b.oi = Some(total); }
            }
            Event::Liquidation { qty, side, .. } => {
                // Sell = a long was liquidated
                if *side == Side::Sell { v.liq.0 += qty } else { v.liq.1 += qty }
                for k in [(Some(ex), mk), (None, mk)] {
                    if let Some(b) = series.entry(k).or_default().at(ts) {
                        if *side == Side::Sell { b.liq_long += qty } else { b.liq_short += qty }
                    }
                }
            }
            Event::LongShort { kind, ratio, .. } => {
                v.ls.insert(*kind, *ratio);
                if *kind == LsKind::Accounts {
                    if let Some(b) = series.entry((Some(ex), mk)).or_default().at(ts) { b.ls = Some(*ratio); }
                }
            }
            Event::BorrowRate { asset, apr } => { v.borrow_apr.insert(asset.clone(), *apr); }
            _ => {}
        }
        if matches!(m.ev, Event::Trade { .. } | Event::Bbo { .. }) { self.touch_index(mk, ts); }
        if matches!(m.ev, Event::LongShort { kind: LsKind::Accounts, .. }) && mk == Market::Perp {
            let ls = self.weighted(|v| v.ls.get(&LsKind::Accounts).copied());
            if let Some(b) = self.series.entry((None, mk)).or_default().at(ts) { b.ls = ls; }
        }
    }

    /// Whether a venue's book is current: updated within STALE_MS of the freshest venue of the
    /// same market (relative, so a replay or a sleeping Mac does not drop every venue).
    pub fn live(&self, v: &Venue, market: Market) -> bool {
        let newest = self.venues.iter().filter(|((_, m), _)| *m == market).map(|(_, v)| v.fresh).max().unwrap_or(0);
        v.fresh >= newest - STALE_MS
    }

    /// Composite USD price for `market`: live venue mids weighted by each venue's volume over the
    /// last INDEX_MIN minutes. Stable across venues that sit a few bp apart, unlike a median.
    pub fn index_price(&self, market: Market) -> Option<f64> {
        let (mut num, mut den) = (0.0, 0.0);
        // minutes, not bars: a thin venue's last 60 bars can span hours
        let since = self.series.get(&(None, market)).and_then(|s| s.bars.back()).map_or(i64::MIN, |b| b.t - INDEX_MIN * BAR_MS);
        for ((e, m), v) in self.venues.iter().filter(|((_, m), v)| *m == market && self.live(v, market)) {
            let Some(p) = v.mid() else { continue };
            let w = self.series.get(&(Some(*e), market)).map(|s| s.bars.iter().rev().take_while(|b| b.t > since).map(|b| b.vol).sum::<f64>()).unwrap_or(0.0).max(1e-9);
            num += p * self.usd(*e, *m) * w;
            den += w;
        }
        (den > 0.0).then(|| num / den)
    }

    fn touch_index(&mut self, market: Market, ts: i64) {
        let Some(p) = self.index_price(market) else { return };
        if let Some(b) = self.series.entry((None, market)).or_default().at(ts) {
            if b.o == 0.0 { b.o = p; b.h = p; b.l = p; }
            b.h = b.h.max(p);
            b.l = b.l.min(p);
            b.c = p;
        }
    }

    fn update_perp_bars(&mut self, ex: Exchange, ts: i64) {
        let basis = self.basis_bps(ex);
        let (funding, settled) = self.venues.get(&(ex, Market::Perp)).map(|v| (v.funding_h, v.funding_settled)).unwrap_or_default();
        if let Some(b) = self.series.entry((Some(ex), Market::Perp)).or_default().at(ts) {
            b.funding_h = funding;
            if settled.is_some() { b.funding_settled = settled; }
            b.basis_bps = basis;
        }
        let (agg_f, agg_s) = (self.funding_oi_weighted(), self.weighted(|v| v.funding_settled));
        let agg_b = median(self.perp_venues().filter_map(|e| self.basis_bps(e)).collect());
        if let Some(b) = self.series.entry((None, Market::Perp)).or_default().at(ts) {
            b.funding_h = agg_f;
            if agg_s.is_some() { b.funding_settled = agg_s; }
            b.basis_bps = agg_b;
        }
    }

    fn perp_venues(&self) -> impl Iterator<Item = Exchange> + '_ {
        self.venues.keys().filter(|(_, m)| *m == Market::Perp).map(|(e, _)| *e)
    }

    /// Set the USD value of one unit of `quote` (fed from a live USDT/USD book).
    pub fn set_fx(&mut self, quote: &'static str, usd: f64) { if usd.is_finite() && usd > 0.0 { self.fx.insert(quote, usd); } }

    /// Multiplier that converts a venue's native prices to USD.
    pub fn usd(&self, ex: Exchange, market: Market) -> f64 { self.fx.get(quote_of(ex, market)).copied().unwrap_or(1.0) }

    /// Median spot/margin mid across live venues, in USD.
    pub fn spot_mid(&self) -> Option<f64> {
        median(self.venues.iter().filter(|((_, m), v)| matches!(m, Market::Spot | Market::Margin) && self.live(v, *m))
            .filter_map(|((e, m), v)| Some(v.mid()? * self.usd(*e, *m))).collect())
    }

    /// Perp mid vs spot mid in basis points (mid, not mark: history basis is close vs close, so
    /// the two join without a step): the venue's own spot book if live, else the cross-venue
    /// median spot. None while the perp book is stale.
    pub fn basis_bps(&self, ex: Exchange) -> Option<f64> {
        let pv = self.venues.get(&(ex, Market::Perp)).filter(|v| self.live(v, Market::Perp))?;
        let perp = pv.mid().or(pv.mark)? * self.usd(ex, Market::Perp);
        let spot = [Market::Spot, Market::Margin].iter()
            .find_map(|m| Some(self.venues.get(&(ex, *m)).filter(|v| self.live(v, *m))?.mid()? * self.usd(ex, *m)))
            .or_else(|| self.spot_mid())?;
        Some((perp / spot - 1.0) * 1e4)
    }

    /// Open-interest-weighted predicted hourly funding across perp venues.
    pub fn funding_oi_weighted(&self) -> Option<f64> { self.weighted(|v| v.funding_h) }

    /// Open-interest-weighted settled hourly funding across perp venues.
    pub fn funding_settled_oi_weighted(&self) -> Option<f64> { self.weighted(|v| v.funding_settled) }

    fn weighted(&self, f: impl Fn(&Venue) -> Option<f64>) -> Option<f64> {
        let (mut num, mut den) = (0.0, 0.0);
        for ((e, m), v) in self.venues.iter().filter(|((_, m), _)| *m == Market::Perp) {
            if let (Some(x), Some(w)) = (f(v), v.oi_usd()) { let w = w * self.usd(*e, *m); num += x * w; den += w; }
        }
        (den > 0.0).then(|| num / den)
    }

    /// Total open interest in base units and USD across perp venues.
    pub fn oi_total(&self) -> (f64, f64) {
        self.venues.iter().filter(|((_, m), _)| *m == Market::Perp)
            .fold((0.0, 0.0), |(b, u), ((e, m), v)| (b + v.oi.unwrap_or(0.0), u + v.oi_usd().unwrap_or(0.0) * self.usd(*e, *m)))
    }

    pub fn cvd_total(&self, market: Market) -> f64 { self.cvd.get(&market).copied().unwrap_or(0.0) }

    /// Cross-venue mid in USD: the composite index (see `index_price`).
    pub fn mid(&self, market: Market) -> Option<f64> { self.index_price(market) }

    /// Aggregated book for `market` in USD: every venue's levels are converted with `usd()` before
    /// merging (USDT/USD/USDC books otherwise cross by a few bp), then bids binned down and asks
    /// binned up, within `range` of mid.
    /// ponytail: USDC is taken as 1.0 USD unless `set_fx("USDC", ..)` is fed.
    pub fn book(&self, market: Market, bin: f64, range: f64) -> (Vec<Level>, Vec<Level>) {
        self.book_where(market, bin, range, false, |_| true)
    }

    /// `book` restricted to the venues for which `keep` returns true.
    /// With `align`, each venue's book is first scaled so its own mid sits on the cross-venue
    /// mid: depth is then shown relative to each venue's price, so USDT perps and USD/USDC perps
    /// (a few bp apart) stack instead of crossing. Prices are then not directly executable.
    pub fn book_where(&self, market: Market, bin: f64, range: f64, align: bool, keep: impl Fn(Exchange) -> bool) -> (Vec<Level>, Vec<Level>) {
        let Some(mid) = self.mid(market) else { return (vec![], vec![]) };
        let (lo, hi) = (mid * (1.0 - range), mid * (1.0 + range));
        let mut bids: std::collections::BTreeMap<i64, Level> = Default::default();
        let mut asks: std::collections::BTreeMap<i64, Level> = Default::default();
        for ((ex, m), v) in self.venues.iter().filter(|((e, m), v)| *m == market && keep(*e) && self.live(v, market)) {
            let mut k = self.usd(*ex, *m);
            if align {
                // a crossed or one-sided book has no meaningful mid: leave it out of the aligned view
                let Some(vm) = v.book_mid() else { continue };
                k = mid / vm;
            }
            // every level a venue holds within the range: grouping is local, so a count cap would leave
            // coarse groups (or a deep range) showing only the first few dollars of each book
            for (side, levels, up) in [(&mut bids, v.book.bids().map(|(p, q)| (p * k, q)).take_while(|(p, _)| *p >= lo).collect::<Vec<_>>(), false),
                                        (&mut asks, v.book.asks().map(|(p, q)| (p * k, q)).take_while(|(p, _)| *p <= hi).collect::<Vec<_>>(), true)] {
                for (p, q) in levels {
                    let i = if up { (p / bin).ceil() } else { (p / bin).floor() } as i64;
                    let l = side.entry(i).or_insert_with(|| Level { px: i as f64 * bin, qty: 0.0, by: vec![] });
                    l.qty += q;
                    match l.by.iter_mut().find(|(e, _)| e == ex) { Some(x) => x.1 += q, None => l.by.push((*ex, q)) }
                }
            }
        }
        (bids.into_values().rev().collect(), asks.into_values().collect())
    }

    /// Announce that history for this venue is being fetched: until `load_history` is called for
    /// it, its trades/marks/OI/liquidations/ratios are queued (books and BBOs apply immediately).
    pub fn expect_history(&mut self, ex: Exchange, market: Market) { self.pending.entry((ex, market)).or_default(); }

    /// Seed one venue's series with REST history, then replay its queued live messages.
    /// Call it for every venue passed to `expect_history` (with an empty `History` if the fetch
    /// failed), then call `finish_history` once.
    /// Queued trades inside a complete history minute are already in its kline and are skipped;
    /// the still-forming minute is dropped from history and rebuilt from the live trades.
    pub fn load_history(&mut self, ex: Exchange, market: Market, h: &History) {
        let covered = self.seed_history(ex, market, h);
        for m in self.pending.remove(&(ex, market)).unwrap_or_default() {
            if matches!(m.ev, Event::Trade { .. }) && m.ts.div_euclid(BAR_MS) * BAR_MS <= covered { continue; }
            self.on(&m);
        }
    }

    /// Returns the open time of the last history minute kept (i64::MIN if none).
    fn seed_history(&mut self, ex: Exchange, market: Market, h: &History) -> i64 {
        self.seeded.insert((ex, market));
        let mut bars = history_bars(h);
        // ponytail: the forming minute's trades before the connect are lost (volume undercounted
        // for that one minute); keeping it instead would double count every queued trade
        if bars.last().is_some_and(|b| b.t + BAR_MS > now_ms()) { bars.pop(); }
        let covered = bars.last().map_or(i64::MIN, |b| b.t);
        let last = bars.last().cloned();
        let offset = self.series.entry((Some(ex), market)).or_default().seed(bars);
        let v = self.venues.entry((ex, market)).or_default();
        v.cvd += offset;
        if let Some(b) = last {
            if v.oi.is_none() { v.oi = b.oi; }
            if v.funding_settled.is_none() { v.funding_settled = b.funding_settled; }
            if v.last.is_none() { v.last = Some(b.c); }
        }
        covered
    }

    /// After all `load_history` calls: derive historical basis and rebuild the cross-venue series.
    pub fn finish_history(&mut self) {
        self.rebuild_agg(Market::Spot);
        self.fill_basis();
        self.rebuild_agg(Market::Perp);
    }

    /// Per-venue perp basis for bars that lack it (history): perp close vs the venue's own spot
    /// close at the same minute, else the cross-venue spot close, both in USD.
    fn fill_basis(&mut self) {
        let spot: HashMap<(Option<Exchange>, i64), f64> = self.series.iter()
            .filter(|((_, m), _)| *m == Market::Spot)
            .flat_map(|((e, m), s)| {
                let k = e.map(|e| self.usd(e, *m)).unwrap_or(1.0);
                s.bars.iter().map(move |b| ((*e, b.t), b.c * k))
            }).collect();
        let fx: HashMap<Exchange, f64> = Exchange::ALL.iter().map(|e| (*e, self.usd(*e, Market::Perp))).collect();
        for ((e, m), s) in self.series.iter_mut() {
            let (Some(e), Market::Perp) = (e, m) else { continue };
            for b in s.bars.iter_mut().filter(|b| b.basis_bps.is_none() && b.c > 0.0) {
                if let Some(sp) = spot.get(&(Some(*e), b.t)).or_else(|| spot.get(&(None, b.t))) {
                    b.basis_bps = Some((b.c * fx[e] / sp - 1.0) * 1e4);
                }
            }
        }
    }

    /// Rebuild the cross-venue series for `market` from the venue series: volumes, CVD, OI and
    /// liquidations summed; funding OI-weighted; basis and USD prices as the cross-venue median.
    /// ponytail: live bars append trades in arrival order instead of the median; fine at 1m.
    pub fn rebuild_agg(&mut self, market: Market) {
        let venues: Vec<(f64, Vec<&Bar>)> = self.series.iter()
            .filter_map(|((e, m), s)| Some((self.usd((*e)?, market), s.bars.iter().collect())).filter(|_| *m == market)).collect();
        let out = combine(venues);
        if let Some(last) = out.last() { self.cvd.insert(market, last.cvd); }
        let s = self.series.entry((None, market)).or_default();
        s.bars = out.into();
        while s.bars.len() > BAR_CAP { s.bars.pop_front(); }
    }

    /// Store one venue's history at timeframe `tf` (minutes); then `finish_history_tf(tf)`.
    pub fn load_history_tf(&mut self, ex: Exchange, market: Market, tf: u32, h: &History) {
        let bars = history_bars_tf(h, tf as i64 * 60_000);
        self.tf.entry(tf).or_default().venues.insert((ex, market), bars);
    }

    /// Derive basis and the cross-venue bars for timeframe `tf` from its venue bars.
    pub fn finish_history_tf(&mut self, tf: u32) {
        let fx: HashMap<Exchange, f64> = Exchange::ALL.iter().map(|e| (*e, self.usd(*e, Market::Perp))).collect();
        let live_oi: HashMap<Exchange, f64> = self.venues.iter().filter(|((_, m), _)| *m == Market::Perp).filter_map(|((e, _), v)| Some((*e, v.oi?))).collect();
        let Some(set) = self.tf.get_mut(&tf) else { return };
        // same rule as the 1m series: no OI history -> flat at the live value, so no step
        for ((e, m), bars) in set.venues.iter_mut() {
            if *m == Market::Perp && bars.iter().all(|b| b.oi.is_none()) {
                if let Some(oi) = live_oi.get(e) { for b in bars.iter_mut() { b.oi = Some(*oi); } }
            }
        }
        let agg_of = |set: &TfSet, market: Market| combine(set.venues.iter().filter(|((_, m), _)| *m == market)
            .map(|((e, _), b)| (fx[e], b.iter().collect())).collect());
        let spot = agg_of(set, Market::Spot);
        let spot_at: HashMap<(Option<Exchange>, i64), f64> = set.venues.iter().filter(|((_, m), _)| *m == Market::Spot)
            .flat_map(|((e, _), b)| { let k = fx[e]; let e = *e; b.iter().map(move |x| ((Some(e), x.t), x.c * k)) })
            .chain(spot.iter().map(|x| ((None, x.t), x.c))).collect();
        for ((e, m), bars) in set.venues.iter_mut().filter(|((_, m), _)| *m == Market::Perp) {
            let _ = m;
            for b in bars.iter_mut().filter(|b| b.basis_bps.is_none() && b.c > 0.0) {
                if let Some(sp) = spot_at.get(&(Some(*e), b.t)).or_else(|| spot_at.get(&(None, b.t))) {
                    b.basis_bps = Some((b.c * fx[e] / sp - 1.0) * 1e4);
                }
            }
        }
        let perp = agg_of(set, Market::Perp);
        set.agg.insert(Market::Spot, spot);
        set.agg.insert(Market::Perp, perp);
    }

    /// One venue's per-minute premium over the composite, in bp: its close (USD) against the
    /// cross-venue close of the same minute.
    /// ponytail: converts with today's FX rate, fine while USDT/USD moves a few bp a day.
    pub fn premium_bps(&self, ex: Exchange, market: Market) -> Vec<(i64, f64)> {
        let (Some(v), Some(x)) = (self.series.get(&(Some(ex), market)), self.series.get(&(None, market))) else { return vec![] };
        let k = self.usd(ex, market);
        let idx: HashMap<i64, f64> = x.bars.iter().filter(|b| b.c > 0.0).map(|b| (b.t, b.c)).collect();
        // minutes with a trade only: a quiet venue's carried-forward close is not a price
        v.bars.iter().filter(|b| b.c > 0.0 && b.vol > 0.0).filter_map(|b| Some((b.t, (b.c * k / idx.get(&b.t)? - 1.0) * 1e4))).collect()
    }

    /// Push one heatmap column per spot/perp market. Call about once a second.
    pub fn sample_heatmap(&mut self, now: i64) {
        for market in [Market::Spot, Market::Perp] {
            let Some(mid) = self.mid(market) else { continue };
            // the bin is fixed per market, but re-derived if the first mid it came from was off
            // (a venue's half-loaded book at startup) so bin indices stay small
            let bin = match self.heat.get(&market) {
                Some(h) if h.bin > 0.0 && h.bin > mid * 1e-5 && h.bin < mid * 4e-3 => h.bin,
                _ => { self.heat.remove(&market); nice(mid * 0.0002) }
            };
            let (b, a) = self.book_where(market, bin, HEAT_RANGE, true, |_| true);
            let col = b.iter().chain(a.iter()).map(|l| ((l.px / bin).round() as i64, l.qty as f32)).collect();
            let h = self.heat.entry(market).or_default();
            h.bin = bin;
            h.cols.push_back((now, col));
            if h.cols.len() > HEAT_CAP { h.cols.pop_front(); }
        }
    }

    fn on_option(&mut self, m: &Msg) {
        let o = self.chains.entry(m.ex).or_default().opts.entry(m.symbol.clone()).or_default();
        match &m.ev {
            Event::OptionInfo { expiry_ms, strike, call, .. } => { o.has_info = true; o.expiry_ms = *expiry_ms; o.strike = *strike; o.call = *call; }
            Event::Mark { mark, index, .. } => { o.mark = Some(*mark); if index.is_some() { o.index = *index; } }
            Event::Forward { px } => o.forward = Some(*px),
            Event::Bbo { bid, bid_qty, ask, ask_qty } => {
                o.bid = (*bid > 0.0).then_some((*bid, *bid_qty));
                o.ask = (*ask > 0.0).then_some((*ask, *ask_qty));
            }
            Event::Greeks { mark_iv, bid_iv, ask_iv, delta, gamma, vega, theta } => {
                o.iv = Some(*mark_iv); o.bid_iv = *bid_iv; o.ask_iv = *ask_iv;
                o.delta = *delta; o.gamma = *gamma; o.vega = *vega; o.theta = *theta;
            }
            Event::Trade { px, .. } => o.last = Some(*px),
            _ => {}
        }
    }
}

/// 1, 2, 2.5, 5 x 10^k at or above x
pub fn nice(x: f64) -> f64 {
    let m = 10f64.powf(x.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|k| k * m).find(|v| *v >= x).unwrap_or(10.0 * m)
}

#[cfg(test)]
mod tests {

    #[test]
    fn aligned_book_never_crosses_on_a_stale_bbo() {
        let mut a = Agg::default();
        let m = |ex, ev| Msg { ex, market: Market::Perp, symbol: "X".into(), ts: 1, recv: 1, ev };
        let book = |b: f64, k: f64| Event::Book { snapshot: true, bids: vec![(b, 1.0)], asks: vec![(k, 1.0)] };
        a.on(&m(Exchange::Bybit, book(100.0, 100.2)));
        // a newer BBO far above the book snapshot (the BBO stream runs ahead of depth)
        a.on(&m(Exchange::Bybit, Event::Bbo { bid: 101.0, bid_qty: 1.0, ask: 101.2, ask_qty: 1.0 }));
        a.on(&m(Exchange::Okx, book(100.05, 100.15)));
        // a crossed venue book is left out entirely
        a.on(&m(Exchange::Gate, book(100.5, 100.0)));
        let (bids, asks) = a.book_where(Market::Perp, 0.01, 0.05, true, |_| true);
        let (bb, ba) = (bids[0].px, asks[0].px);
        assert!(bb < ba, "crossed: bid {bb} ask {ba}");
        assert!(bids.iter().chain(asks.iter()).all(|l| l.by.iter().all(|(e, _)| *e != Exchange::Gate)));
    }

    use super::*;

    fn msg(ex: Exchange, market: Market, symbol: &str, ts: i64, ev: Event) -> Msg {
        Msg { ex, market, symbol: symbol.into(), ts, recv: ts, ev }
    }

    #[test]
    fn cvd_bars_and_carry_forward() {
        let mut a = Agg::default();
        let t0 = 1_000 * BAR_MS;
        a.on(&msg(Exchange::Binance, Market::Perp, "X", t0, Event::Trade { px: 100.0, qty: 2.0, side: Side::Buy }));
        a.on(&msg(Exchange::Bybit, Market::Perp, "X", t0 + 1, Event::Trade { px: 101.0, qty: 0.5, side: Side::Sell }));
        a.on(&msg(Exchange::Binance, Market::Perp, "X", t0 + 2, Event::OpenInterest { oi: 10.0, oi_usd: None }));
        a.on(&msg(Exchange::Bybit, Market::Perp, "X", t0 + 3, Event::OpenInterest { oi: 5.0, oi_usd: None }));
        // next minute: one sell on binance
        a.on(&msg(Exchange::Binance, Market::Perp, "X", t0 + BAR_MS, Event::Trade { px: 99.0, qty: 1.0, side: Side::Sell }));
        let s = &a.series[&(None, Market::Perp)].bars;
        assert_eq!(s.len(), 2);
        // composite: Binance 100 (vol 2) and Bybit 101 (vol 0.5) -> 100.2
        assert_eq!((s[0].o, s[0].l, s[0].buy, s[0].sell, s[0].cvd), (100.0, 100.0, 2.0, 0.5, 1.5));
        assert!((s[0].c - 100.2).abs() < 1e-9 && (s[0].h - 100.2).abs() < 1e-9);
        assert_eq!(s[0].oi, Some(15.0));
        assert!((s[1].o - 100.2).abs() < 1e-9);
        assert_eq!((s[1].cvd, s[1].oi), (0.5, Some(15.0)));
        assert_eq!(a.cvd_total(Market::Perp), 0.5);
        assert_eq!(a.venues[&(Exchange::Binance, Market::Perp)].cvd, 1.0);
    }

    #[test]
    fn funding_weighting_and_basis() {
        let mut a = Agg::default();
        let ev = |f: f64| Event::Mark { mark: 100.0, index: None, funding: Some(f), next_funding_ms: None };
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 1, ev(0.0001)));
        a.on(&msg(Exchange::Bybit, Market::Perp, "X", 1, ev(0.0004)));
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 2, Event::OpenInterest { oi: 3.0, oi_usd: None }));
        a.on(&msg(Exchange::Bybit, Market::Perp, "X", 2, Event::OpenInterest { oi: 1.0, oi_usd: None }));
        assert!((a.funding_oi_weighted().unwrap() - 0.000175).abs() < 1e-12);
        a.on(&msg(Exchange::Binance, Market::Spot, "X", 3, Event::Bbo { bid: 99.9, bid_qty: 1.0, ask: 100.1, ask_qty: 1.0 }));
        assert!(a.basis_bps(Exchange::Binance).unwrap().abs() < 1e-9);
        // Bybit has no spot venue here: falls back to the cross-venue spot median (100.0)
        assert!(a.basis_bps(Exchange::Bybit).unwrap().abs() < 1e-9);
    }

    #[test]
    fn aggregated_book_bins_and_splits() {
        let mut a = Agg::default();
        a.on(&msg(Exchange::Binance, Market::Spot, "X", 1, Event::Book { snapshot: true, bids: vec![(99.4, 1.0), (98.6, 2.0)], asks: vec![(100.6, 1.0)] }));
        a.on(&msg(Exchange::Okx, Market::Spot, "X", 1, Event::Book { snapshot: true, bids: vec![(99.2, 3.0)], asks: vec![(100.2, 4.0)] }));
        let (b, k) = a.book(Market::Spot, 1.0, 0.05);
        assert_eq!(b[0].px, 99.0);
        assert_eq!(b[0].qty, 4.0);
        assert_eq!(b[0].by.len(), 2);
        assert_eq!(k[0].px, 101.0);
        assert_eq!(k[0].qty, 5.0);
    }

    #[test]
    fn parity_forward_and_atm() {
        let mut a = Agg::default();
        let exp = 2_000_000_000_000;
        for (sym, strike, call, mark) in [("C90", 90.0, true, 14.0), ("P90", 90.0, false, 3.0), ("C100", 100.0, true, 7.0), ("P100", 100.0, false, 6.0)] {
            a.on(&msg(Exchange::Gate, Market::Option, sym, 1, Event::OptionInfo { underlying: "X".into(), expiry_ms: exp, strike, call }));
            a.on(&msg(Exchange::Gate, Market::Option, sym, 2, Event::Mark { mark, index: Some(100.0), funding: None, next_funding_ms: None }));
            a.on(&msg(Exchange::Gate, Market::Option, sym, 2, Event::Greeks { mark_iv: if strike == 100.0 { 0.5 } else { 0.6 }, bid_iv: None, ask_iv: None, delta: 0.0, gamma: 0.0, vega: 0.0, theta: 0.0 }));
        }
        let e = &a.chains[&Exchange::Gate].expiries()[0];
        // parity: 90 + 14 - 3 = 101, 100 + 7 - 6 = 101
        assert_eq!((e.forward, e.forward_src, e.atm_strike, e.atm_iv), (Some(101.0), "parity", 100.0, Some(0.5)));
    }

    #[test]
    fn fx_uncrosses_mixed_quote_books() {
        let mut a = Agg::default();
        // USDT trades at 0.9995 USD: 100.00 USDT == 99.95 USD
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 1, Event::Book { snapshot: true, bids: vec![(100.0, 1.0)], asks: vec![(100.02, 1.0)] }));
        a.on(&msg(Exchange::Kraken, Market::Perp, "X", 1, Event::Book { snapshot: true, bids: vec![(99.96, 1.0)], asks: vec![(99.98, 1.0)] }));
        let crossed = |a: &Agg| { let (b, k) = a.book(Market::Perp, 0.001, 0.05); b[0].px > k[0].px };
        assert!(crossed(&a));
        a.set_fx("USDT", 0.9995);
        assert!(!crossed(&a));
    }

    #[test]
    fn history_seeds_bars_and_continues_live() {
        let mut a = Agg::default();
        let t0 = 1_000 * BAR_MS;
        let k = |i: i64, c: f64, buy: Option<f64>| Kline { t: t0 + i * BAR_MS, o: c, h: c + 1.0, l: c - 1.0, c, vol: 10.0, buy };
        let h = History {
            klines: vec![k(0, 100.0, Some(7.0)), k(1, 101.0, Some(2.0)), k(2, 102.0, Some(5.0))],
            oi: vec![(t0 - 1, 50.0), (t0 + 2 * BAR_MS, 60.0)],
            funding: vec![(t0, 0.0001)],
            ..Default::default()
        };
        // live trade arrives in the last history minute before history is loaded
        a.on(&msg(Exchange::Binance, Market::Perp, "X", t0 + 2 * BAR_MS + 5, Event::Trade { px: 102.5, qty: 1.0, side: Side::Buy }));
        a.load_history(Exchange::Binance, Market::Perp, &h);
        a.finish_history();
        let s = &a.series[&(Some(Exchange::Binance), Market::Perp)].bars;
        // history cvd: +4, -6 -> -2 (minute 2 is covered by the live bar and dropped from history)
        assert_eq!(s.iter().map(|b| b.cvd).collect::<Vec<_>>(), vec![4.0, -2.0, -1.0]);
        assert_eq!(s[0].oi, Some(50.0));
        assert_eq!((s[1].funding_settled, s[1].funding_h), (Some(0.0001), None));
        assert_eq!(a.venues[&(Exchange::Binance, Market::Perp)].cvd, -1.0);
        // cross-venue series rebuilt from the venue series, then live trades continue from it
        a.on(&msg(Exchange::Binance, Market::Perp, "X", t0 + 3 * BAR_MS, Event::Trade { px: 103.0, qty: 2.0, side: Side::Sell }));
        let g = &a.series[&(None, Market::Perp)].bars;
        assert_eq!(g.iter().map(|b| b.cvd).collect::<Vec<_>>(), vec![4.0, -2.0, -1.0, -3.0]);
        assert_eq!(a.cvd_total(Market::Perp), -3.0);
    }

    #[test]
    fn history_taker_split_and_basis() {
        let mut a = Agg::default();
        let t0 = 1_000 * BAR_MS;
        let k = |i: i64, c: f64| Kline { t: t0 + i * BAR_MS, o: c, h: c, l: c, c, vol: 10.0, buy: None };
        a.load_history(Exchange::Okx, Market::Spot, &History { klines: vec![k(0, 100.0), k(1, 100.0)], ..Default::default() });
        a.load_history(Exchange::Okx, Market::Perp, &History {
            klines: vec![k(0, 100.5), k(1, 99.0)],
            taker: vec![(t0, 6.0, 4.0)],
            ..Default::default()
        });
        a.finish_history();
        let s = &a.series[&(Some(Exchange::Okx), Market::Perp)].bars;
        assert_eq!((s[0].buy, s[0].sell, s[0].cvd, s[1].cvd), (6.0, 4.0, 2.0, 2.0));
        assert!((s[0].basis_bps.unwrap() - 50.0).abs() < 1e-9);
        assert!((a.series[&(None, Market::Perp)].bars[1].basis_bps.unwrap() + 100.0).abs() < 1e-9);
    }

    #[test]
    fn live_waits_for_history_and_oi_backfills() {
        let mut a = Agg::default();
        let t0 = 1_000 * BAR_MS;
        let k = |i: i64| Kline { t: t0 + i * BAR_MS, o: 1.0, h: 1.0, l: 1.0, c: 1.0, vol: 1.0, buy: Some(1.0) };
        a.expect_history(Exchange::Gate, Market::Perp);
        a.expect_history(Exchange::Mexc, Market::Perp);
        // an old-stamped live trade arrives first: it must not cut off the history after it, and
        // its minute is complete in history, so the kline already holds it (not added twice)
        a.on(&msg(Exchange::Gate, Market::Perp, "X", t0 + BAR_MS, Event::Trade { px: 1.0, qty: 5.0, side: Side::Sell }));
        a.load_history(Exchange::Gate, Market::Perp, &History { klines: (0..4).map(k).collect(), oi: vec![(t0, 10.0)], ..Default::default() });
        a.load_history(Exchange::Mexc, Market::Perp, &History { klines: (0..4).map(k).collect(), ..Default::default() });
        a.finish_history();
        let g = &a.series[&(Some(Exchange::Gate), Market::Perp)].bars;
        assert_eq!(g.len(), 4);
        assert_eq!(g.iter().map(|b| b.cvd).collect::<Vec<_>>(), vec![1.0, 2.0, 3.0, 4.0]);
        assert!(g.iter().all(|b| b.oi == Some(10.0)));
        // MEXC has no OI history: its first live OI backfills, and the aggregate has no jump
        assert_eq!(a.series[&(None, Market::Perp)].bars[0].oi, Some(10.0));
        a.on(&msg(Exchange::Mexc, Market::Perp, "X", t0 + 3 * BAR_MS, Event::OpenInterest { oi: 7.0, oi_usd: None }));
        assert!(a.series[&(None, Market::Perp)].bars.iter().all(|b| b.oi == Some(17.0)));
    }

    #[test]
    fn settlement_turns_prediction_into_settled() {
        let mut a = Agg::default();
        let mk = |f: f64, next: i64| Event::Mark { mark: 100.0, index: None, funding: Some(f), next_funding_ms: Some(next) };
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 1, mk(0.0001, 1000)));
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 2, mk(0.0002, 1000)));
        assert_eq!(a.venues[&(Exchange::Binance, Market::Perp)].funding_settled, None);
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 3, mk(0.00005, 2000)));
        let v = &a.venues[&(Exchange::Binance, Market::Perp)];
        assert_eq!((v.funding_settled, v.funding_h), (Some(0.0002), Some(0.00005)));
    }

    #[test]
    fn aligned_book_stacks_dislocated_venues() {
        let mut a = Agg::default();
        // two venues 20 bp apart: raw merge crosses, aligned merge does not
        a.on(&msg(Exchange::Binance, Market::Perp, "X", 1, Event::Book { snapshot: true, bids: vec![(99.99, 1.0)], asks: vec![(100.01, 1.0)] }));
        a.on(&msg(Exchange::Kraken, Market::Perp, "X", 1, Event::Book { snapshot: true, bids: vec![(100.19, 2.0)], asks: vec![(100.21, 2.0)] }));
        let (b, k) = a.book(Market::Perp, 0.01, 0.05);
        assert!(b[0].px > k[0].px);
        let (b, k) = a.book_where(Market::Perp, 0.01, 0.05, true, |_| true);
        assert!(b[0].px < k[0].px);
        assert_eq!(b.iter().chain(k.iter()).map(|l| l.qty).sum::<f64>(), 6.0);
    }

    #[test]
    fn timeframe_history_builds_cross_venue_bars() {
        let mut a = Agg::default();
        let h = 3_600_000;
        let k = |i: i64, c: f64| Kline { t: 500 * h + i * h, o: c, h: c, l: c, c, vol: 2.0, buy: Some(1.5) };
        a.load_history_tf(Exchange::Binance, Market::Perp, 60, &History { klines: vec![k(0, 101.0), k(1, 102.0)], oi: vec![(500 * h, 7.0)], ..Default::default() });
        a.load_history_tf(Exchange::Okx, Market::Perp, 60, &History { klines: vec![k(0, 103.0), k(1, 104.0)], oi: vec![(500 * h, 3.0)], ..Default::default() });
        a.load_history_tf(Exchange::Binance, Market::Spot, 60, &History { klines: vec![k(0, 100.0), k(1, 100.0)], ..Default::default() });
        a.finish_history_tf(60);
        let set = &a.tf[&60];
        let g = set.bars(None, Market::Perp).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].c, g[0].vol, g[0].cvd, g[0].oi), (102.0, 4.0, 2.0, Some(10.0)));
        assert!((set.bars(Some(Exchange::Okx), Market::Perp).unwrap()[1].basis_bps.unwrap() - 400.0).abs() < 1e-9);
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice(17.2), 20.0);
        assert_eq!(nice(0.03), 0.05);
        assert_eq!(nice(2.0), 2.0);
    }

    #[test]
    fn walls_need_persistence_and_survive_bin_hops() {
        // flat book of 1 per bin around mid 100 (bin 1); a 30-lot bid wall hopping 90 <-> 91,
        // and a 300-lot ask flashed for 5 of 60 seconds
        let mut h = Heatmap { bin: 1.0, ..Default::default() };
        for t in 0..60i64 {
            let mut c: Vec<(i64, f32)> = (80..=120).map(|i| (i, 1.0)).collect();
            c[(90 + t % 2 - 80) as usize].1 += 30.0;
            if t >= 55 { c[110 - 80].1 += 300.0; }
            h.cols.push_back((t, c));
        }
        let (b, a) = h.walls(100.0, 60, &[]);
        assert_eq!(b.len(), 1);
        assert!((b[0].0 - 90.0).abs() <= 1.0, "{b:?}");
        assert!(a.is_empty(), "{a:?}");
        // hysteresis: a wall at 4.5x median shows only if it was already shown
        let mut h = Heatmap { bin: 1.0, ..Default::default() };
        for t in 0..60i64 {
            let mut c: Vec<(i64, f32)> = (80..=120).map(|i| (i, 1.0)).collect();
            c[110 - 80].1 += 10.5; // windowed: 13.5 vs median 3 = 4.5x
            h.cols.push_back((t, c));
        }
        assert!(h.walls(100.0, 60, &[]).1.is_empty());
        assert_eq!(h.walls(100.0, 60, &[110.0]).1.len(), 1);
    }

    #[test]
    fn stale_venue_leaves_index_book_and_basis() {
        let mut a = Agg::default();
        let at = |ex, market, recv: i64, ev| Msg { ex, market, symbol: "X".into(), ts: recv, recv, ev };
        let bbo = |b: f64| Event::Bbo { bid: b, bid_qty: 1.0, ask: b + 0.2, ask_qty: 1.0 };
        let book = |b: f64| Event::Book { snapshot: true, bids: vec![(b, 1.0)], asks: vec![(b + 0.2, 1.0)] };
        for (ex, px) in [(Exchange::Bybit, 100.0), (Exchange::Okx, 110.0)] {
            a.on(&at(ex, Market::Perp, 1_000, book(px)));
            a.on(&at(ex, Market::Perp, 1_000, bbo(px)));
        }
        assert!((a.index_price(Market::Perp).unwrap() - 105.1).abs() < 1e-6);
        // OKX goes silent; Bybit keeps updating 20 s later
        a.on(&at(Exchange::Bybit, Market::Perp, 21_000, bbo(100.0)));
        assert!((a.index_price(Market::Perp).unwrap() - 100.1).abs() < 1e-6);
        let (bids, _) = a.book_where(Market::Perp, 0.1, 0.5, false, |_| true);
        assert!(bids.iter().all(|l| l.by.iter().all(|(e, _)| *e != Exchange::Okx)));
        assert!(a.basis_bps(Exchange::Okx).is_none());
    }

    #[test]
    fn history_replay_does_not_double_count_queued_trades() {
        let mut a = Agg::default();
        let (ex, mk) = (Exchange::Binance, Market::Perp);
        let now = now_ms();
        let done = (now - 3 * BAR_MS).div_euclid(BAR_MS) * BAR_MS; // a complete minute
        let forming = now.div_euclid(BAR_MS) * BAR_MS;
        a.expect_history(ex, mk);
        let trade = |ts| Msg { ex, market: mk, symbol: "X".into(), ts, recv: ts, ev: Event::Trade { px: 100.0, qty: 1.0, side: Side::Buy } };
        a.on(&trade(done + 1_000));
        a.on(&trade(forming + 1));
        // the REST klines already hold that trade (complete minute) and part of the forming one
        let k = |t| Kline { t, o: 100.0, h: 100.0, l: 100.0, c: 100.0, vol: 1.0, buy: Some(1.0) };
        a.load_history(ex, mk, &History { klines: vec![k(done), k(forming)], ..Default::default() });
        let s = &a.series[&(Some(ex), mk)];
        let vol = |t| s.bars.iter().find(|b| b.t == t).map(|b| b.vol);
        assert_eq!(vol(done), Some(1.0));
        assert_eq!(vol(forming), Some(1.0));
        assert_eq!(a.venues[&(ex, mk)].cvd, 2.0);
    }
}
