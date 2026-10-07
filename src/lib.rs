//! Terminal One data layer: every exchange feed is normalized into `Msg` and sent over one channel.
pub mod agg;
pub mod algo;
pub mod ex;
pub mod insure;
pub mod quant;
pub mod route;
pub mod trade;
pub mod ws;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Exchange { Binance, Bybit, Bitget, Okx, Mexc, Coinbase, Kraken, Gate, Hyperliquid, Lighter }

impl Exchange {
    pub const ALL: [Exchange; 10] = [Self::Binance, Self::Bybit, Self::Bitget, Self::Okx, Self::Mexc,
        Self::Coinbase, Self::Kraken, Self::Gate, Self::Hyperliquid, Self::Lighter];
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| format!("{e:?}").eq_ignore_ascii_case(s))
    }
}

/// Margin shares the spot order book; it only adds borrow rates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Market { Spot, Margin, Perp, Option }

impl Market {
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Spot, Self::Margin, Self::Perp, Self::Option].into_iter().find(|m| format!("{m:?}").eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side { Buy, Sell }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LsKind { Accounts, TopAccounts, TopPositions, TakerVolume }

#[derive(Clone, Debug)]
pub enum Event {
    /// side = aggressor
    Trade { px: f64, qty: f64, side: Side },
    /// snapshot=true replaces the whole book; qty 0 deletes a level
    Book { snapshot: bool, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)> },
    Bbo { bid: f64, bid_qty: f64, ask: f64, ask_qty: f64 },
    /// perp: mark/index/funding; option: mark + underlying index.
    /// `funding` is always the rate PER HOUR (venue rate / its funding interval in hours),
    /// so venues with 1h, 4h and 8h intervals are directly comparable.
    Mark { mark: f64, index: Option<f64>, funding: Option<f64>, next_funding_ms: Option<i64> },
    /// option: forward / underlying price for this contract's expiry, when the venue publishes it.
    /// Venues without it get a put-call-parity forward computed in `agg`.
    Forward { px: f64 },
    /// oi in base units; oi_usd only when the exchange provides it
    OpenInterest { oi: f64, oi_usd: Option<f64> },
    /// side = liquidation order side; Sell means a long was liquidated
    Liquidation { px: f64, qty: f64, side: Side },
    /// ratio = long/short; long_pct in 0..1
    LongShort { kind: LsKind, ratio: f64, long_pct: Option<f64> },
    Greeks { mark_iv: f64, bid_iv: Option<f64>, ask_iv: Option<f64>, delta: f64, gamma: f64, vega: f64, theta: f64 },
    /// option contract metadata from the exchange instrument list
    OptionInfo { underlying: String, expiry_ms: i64, strike: f64, call: bool },
    /// annualized borrow rate
    BorrowRate { asset: String, apr: f64 },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Trade { .. } => "trade", Event::Book { .. } => "book", Event::Bbo { .. } => "bbo",
            Event::Mark { .. } => "mark", Event::OpenInterest { .. } => "oi", Event::Liquidation { .. } => "liq",
            Event::LongShort { .. } => "ls", Event::Greeks { .. } => "greeks", Event::OptionInfo { .. } => "optinfo",
            Event::BorrowRate { .. } => "borrow", Event::Forward { .. } => "forward",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub ex: Exchange,
    pub market: Market,
    /// exchange-native symbol
    pub symbol: Arc<str>,
    /// exchange timestamp in ms, local time if the exchange gives none
    pub ts: i64,
    pub recv: i64,
    pub ev: Event,
}

/// One historical candle. Sizes in base units.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Kline {
    /// open time, ms
    pub t: i64,
    pub o: f64, pub h: f64, pub l: f64, pub c: f64,
    pub vol: f64,
    /// aggressor-buy volume, when the venue's candles carry it (e.g. Binance taker buy volume)
    pub buy: Option<f64>,
}

/// Recent history fetched over REST at startup to seed the chart, ascending by time.
/// Every field may be empty when the venue has no such endpoint.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct History {
    /// one-minute candles
    pub klines: Vec<Kline>,
    /// open interest in base units
    pub oi: Vec<(i64, f64)>,
    /// HOURLY funding rate, at each settlement (or sample) time
    pub funding: Vec<(i64, f64)>,
    /// accounts long/short ratio
    pub long_short: Vec<(i64, f64)>,
    /// (t, aggressor buy, aggressor sell) in base units, for venues whose candles lack the split
    pub taker: Vec<(i64, f64, f64)>,
}

/// subscription; for options only `base` (the underlying) is used
#[derive(Clone, Debug)]
pub struct Sub { pub market: Market, pub base: String, pub quote: String }

pub type Tx = tokio::sync::mpsc::UnboundedSender<Msg>;

pub fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64 }

/// exchanges often encode numbers as strings
pub fn num(v: &serde_json::Value) -> f64 {
    match v { serde_json::Value::String(s) => s.parse().unwrap_or(f64::NAN), v => v.as_f64().unwrap_or(f64::NAN) }
}
pub fn opt_num(v: &serde_json::Value) -> Option<f64> { let x = num(v); x.is_finite().then_some(x) }
/// parses [[px, qty, ...], ...]; object-shaped levels are handled by the caller
pub fn levels(v: &serde_json::Value) -> Vec<(f64, f64)> {
    v.as_array().map(|a| a.iter().map(|l| (num(&l[0]), num(&l[1]))).collect()).unwrap_or_default()
}

/// builds and sends `Msg`; each connector owns one
#[derive(Clone)]
pub struct Emit { pub ex: Exchange, pub market: Market, pub tx: Tx }
impl Emit {
    pub fn send(&self, symbol: &str, ts: i64, ev: Event) {
        let recv = now_ms();
        let _ = self.tx.send(Msg { ex: self.ex, market: self.market, symbol: symbol.into(), ts: if ts > 0 { ts } else { recv }, recv, ev });
    }
}

#[derive(Clone, Copy, PartialEq, PartialOrd)]
struct Px(f64);
impl Eq for Px {}
impl Ord for Px { fn cmp(&self, o: &Self) -> std::cmp::Ordering { self.0.total_cmp(&o.0) } }

/// local order book fed by Event::Book
#[derive(Default)]
pub struct Book { bids: BTreeMap<Px, f64>, asks: BTreeMap<Px, f64> }

impl Book {
    pub fn apply(&mut self, snapshot: bool, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        if snapshot { self.bids.clear(); self.asks.clear(); }
        for (side, lv) in [(&mut self.bids, bids), (&mut self.asks, asks)] {
            for &(p, q) in lv { if q > 0.0 { side.insert(Px(p), q); } else { side.remove(&Px(p)); } }
        }
    }
    pub fn best_bid(&self) -> Option<(f64, f64)> { self.bids.iter().next_back().map(|(p, q)| (p.0, *q)) }
    pub fn best_ask(&self) -> Option<(f64, f64)> { self.asks.iter().next().map(|(p, q)| (p.0, *q)) }
    pub fn depth(&self) -> (usize, usize) { (self.bids.len(), self.asks.len()) }
    pub fn bids(&self) -> impl Iterator<Item = (f64, f64)> + '_ { self.bids.iter().rev().map(|(p, q)| (p.0, *q)) }
    pub fn asks(&self) -> impl Iterator<Item = (f64, f64)> + '_ { self.asks.iter().map(|(p, q)| (p.0, *q)) }
}

/// "2026-10-06T09:44:54.387165186Z" -> unix ms (UTC only); 0 if malformed
pub fn iso_ms(s: &str) -> i64 {
    let n = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d), Some(hh), Some(mm), Some(ss)) = (n(0, 4), n(5, 7), n(8, 10), n(11, 13), n(14, 16), n(17, 19)) else { return 0 };
    let ms = s.get(19..).and_then(|f| f.strip_prefix('.')).map(|f| {
        let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).chain("000".chars()).take(3).collect();
        digits.parse::<i64>().unwrap_or(0)
    }).unwrap_or(0);
    // days from civil (H. Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    ((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms
}

/// Sorts, dedups and forward-fills minutes without trades (o=h=l=c=previous close, vol 0) up to
/// `end` (open time of the current minute), keeping the last `n`. Venues omit empty candles.
pub fn continuous(k: Vec<Kline>, end: i64, n: usize) -> Vec<Kline> { continuous_tf(k, end, n, 1) }

/// `continuous` for `tf`-minute candles: `end` is the open time of the current candle.
pub fn continuous_tf(mut k: Vec<Kline>, end: i64, n: usize, tf: u32) -> Vec<Kline> {
    let step = tf as i64 * 60_000;
    k.sort_by_key(|x| x.t);
    k.dedup_by_key(|x| x.t);
    let flat = |p: Kline| Kline { t: p.t + step, o: p.c, h: p.c, l: p.c, c: p.c, vol: 0.0, buy: p.buy.map(|_| 0.0) };
    let mut out: Vec<Kline> = Vec::with_capacity(n);
    for x in k.into_iter().filter(|x| x.t <= end) {
        while let Some(p) = out.last().copied().filter(|p| p.t + step < x.t) { out.push(flat(p)) }
        out.push(x);
    }
    while let Some(p) = out.last().copied().filter(|p| p.t + step <= end) { out.push(flat(p)) }
    out.drain(..out.len().saturating_sub(n));
    out
}

/// open time of the current minute and of the first of the last `minutes`, in ms
pub fn window(minutes: usize) -> (i64, i64) { window_tf(1, minutes) }

/// open time of the current `tf`-minute candle and of the first of the last `bars` candles, in ms
/// (candles aligned to UTC multiples of `tf`, which is how every venue buckets 1m..4h)
pub fn window_tf(tf: u32, bars: usize) -> (i64, i64) {
    let step = tf as i64 * 60_000;
    let end = now_ms() / step * step;
    (end - (bars.max(1) as i64 - 1) * step, end)
}

impl History {
    /// `self` (older, e.g. from the local cache) updated with `newer` (a fresh fetch): rows are
    /// merged by time with the newer value winning, then trimmed to the last `bars` candles.
    pub fn merge(mut self, newer: History, bars: usize, tf: u32) -> History {
        fn m<T: Copy>(a: &mut Vec<T>, b: Vec<T>, t: impl Fn(&T) -> i64) {
            let cut = b.first().map(&t).unwrap_or(i64::MAX);
            a.retain(|x| t(x) < cut);
            a.extend(b);
        }
        m(&mut self.klines, newer.klines, |k| k.t);
        m(&mut self.oi, newer.oi, |x| x.0);
        m(&mut self.funding, newer.funding, |x| x.0);
        m(&mut self.long_short, newer.long_short, |x| x.0);
        m(&mut self.taker, newer.taker, |x| x.0);
        self.klines.drain(..self.klines.len().saturating_sub(bars));
        // side series keep one step before the first candle so it starts covered
        let start = self.klines.first().map(|k| k.t - tf as i64 * 60_000 * 8).unwrap_or(i64::MIN);
        let keep = |v: &mut Vec<(i64, f64)>| { let i = v.iter().rposition(|x| x.0 <= start).unwrap_or(0); v.drain(..i); };
        keep(&mut self.oi); keep(&mut self.funding); keep(&mut self.long_short);
        if let Some(i) = self.taker.iter().rposition(|x| x.0 <= start) { self.taker.drain(..i); }
        self
    }
}

/// Candle periods the chart offers, in minutes.
pub const TIMEFRAMES: [u32; 5] = [1, 5, 15, 60, 240];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuous_fills_gaps_and_tail() {
        let k = |t: i64, c: f64| Kline { t: t * 60_000, o: c, h: c, l: c, c, vol: 1.0, buy: None };
        let out = continuous(vec![k(3, 3.0), k(1, 1.0), k(1, 1.0)], 5 * 60_000, 4);
        assert_eq!(out.iter().map(|x| (x.t / 60_000, x.c, x.vol)).collect::<Vec<_>>(),
            vec![(2, 1.0, 0.0), (3, 3.0, 1.0), (4, 3.0, 0.0), (5, 3.0, 0.0)]);
        let h = |t: i64| Kline { t: t * 3_600_000, o: 1.0, h: 1.0, l: 1.0, c: 1.0, vol: 1.0, buy: None };
        let out = continuous_tf(vec![h(1), h(4)], 5 * 3_600_000, 10, 60);
        assert_eq!(out.iter().map(|x| x.t / 3_600_000).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    }
    #[test]
    fn iso_ms_parses_utc() {
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), 0);
        assert_eq!(iso_ms("2026-10-06T09:44:54.387165186Z"), 1791279894387);
        assert_eq!(iso_ms("2024-02-29T23:59:59.5Z"), 1709251199500);
        assert_eq!(iso_ms("garbage"), 0);
    }
    #[test]
    fn history_merge_prefers_newer_and_trims() {
        let k = |i: i64, c: f64| Kline { t: i * 60_000, o: c, h: c, l: c, c, vol: 1.0, buy: None };
        let old = History { klines: (0..5).map(|i| k(i, 1.0)).collect(), oi: vec![(0, 1.0), (3 * 60_000, 3.0)], ..Default::default() };
        let new = History { klines: (4..7).map(|i| k(i, 2.0)).collect(), oi: vec![(4 * 60_000, 4.0)], ..Default::default() };
        let h = old.merge(new, 5, 1);
        assert_eq!(h.klines.iter().map(|x| (x.t / 60_000, x.c)).collect::<Vec<_>>(), vec![(2, 1.0), (3, 1.0), (4, 2.0), (5, 2.0), (6, 2.0)]);
        assert_eq!(h.oi.last(), Some(&(4 * 60_000, 4.0)));
    }

    #[test]
    fn book_snapshot_then_delta() {
        let mut b = Book::default();
        b.apply(true, &[(100.0, 1.0), (99.0, 2.0)], &[(101.0, 1.0)]);
        b.apply(false, &[(100.0, 0.0), (99.5, 3.0)], &[(100.5, 1.0)]);
        assert_eq!(b.best_bid(), Some((99.5, 3.0)));
        assert_eq!(b.best_ask(), Some((100.5, 1.0)));
        b.apply(true, &[(1.0, 1.0)], &[]);
        assert_eq!(b.depth(), (1, 0));
    }
}
