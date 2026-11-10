//! Bitfinex: spot pairs (`tBTCUSD`, `tAVAX:USD`) and perpetual derivatives (`tBTCF0:USTF0`) on the public WS v2
//! (wss://api-pub.bitfinex.com/ws/2), REST v2 on api-pub.bitfinex.com.
//!
//! Every symbol takes two channels: `trades` and the raw book (`book`, prec R0, len 250): one entry per resting order
//! (id, price, signed amount; price 0 deletes). The connector keeps that order book and emits from it the L2 levels
//! (`Book` deltas of the touched prices), `Bbo` when the top moves and the best `TOP` orders per side as `Orders`
//! (exact L3, at most once a second per symbol). Public trades carry no maker order id, so there is no `OrderFill`.
//!
//! Limits (https://docs.bitfinex.com/docs/requirements-and-limitations, https://docs.bitfinex.com/docs/ws-general):
//! 20 connections a minute on api-pub, 25 channels per connection, REST 10-90 requests a minute per endpoint.
//! Derivatives: mark / spot index / funding / OI from `status/deriv?keys=<sym>` every 15 s. Funding is settled every 8 h.
//! ponytail: no liquidations (they come only from the global `liq:global` status feed); add a socket for it if wanted.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use tokio::task::JoinHandle;

const WS: &str = "wss://api-pub.bitfinex.com/ws/2";
const REST: &str = "https://api-pub.bitfinex.com/v2";
/// orders per side of the raw book subscription (the largest `len`)
const LEN: usize = 250;
/// orders per side in `Event::Orders`
const TOP: usize = 50;
/// `Orders` at most this often per symbol (ms)
const ORDERS_EVERY: i64 = 1000;
/// conf flag TIMESTAMP: every data message ends with the server's ms timestamp
const CONF: &str = r#"{"event":"conf","flags":32768}"#;

/// Bitfinex currency codes that differ from the unified ones (`pub:map:currency:sym`).
const CODES: &[(&str, &str)] = &[("UST", "USDT"), ("UDC", "USDC"), ("ALG", "ALGO"), ("ATO", "ATOM"), ("BCHN", "BCH"),
    ("DSH", "DASH"), ("IOT", "IOTA"), ("WBT", "WBTC"), ("OPX", "OP"), ("LIFIII", "LIF3")];
fn native(code: &str) -> &str { CODES.iter().find(|c| c.1 == code).map_or(code, |c| c.0) }

/// The venue symbol of (base, quote): three-letter codes concatenate, longer ones take a colon; perps are `F0`.
fn symbol(market: Market, base: &str, quote: &str) -> String {
    let (b, q) = (native(base), native(quote));
    match market {
        Market::Perp => format!("t{b}F0:{q}F0"),
        _ if b.len() == 3 && q.len() == 3 => format!("t{b}{q}"),
        _ => format!("t{b}:{q}"),
    }
}

fn tf_name(tf: u32) -> &'static str { match tf { 5 => "5m", 15 => "15m", 60 => "1h", 240 => "4h", _ => "1m" } }

// ------------------------------------------------------------------ order book

/// Price as an ordered integer (Bitfinex quotes 5 significant digits; 1e-9 resolves sub-satoshi prices).
fn pkey(px: f64) -> i64 { (px * 1e9).round() as i64 }

/// One symbol's raw book: orders by (price key, arrival) per side, so a level's queue is in arrival order.
#[derive(Default)]
struct Orders {
    bids: BTreeMap<(i64, u64), (f64, f64, f64, u64)>,
    asks: BTreeMap<(i64, u64), (f64, f64, f64, u64)>,
    /// id -> (bid, price key, arrival)
    ids: HashMap<u64, (bool, i64, u64)>,
    next: u64,
    bbo: (f64, f64, f64, f64),
    dirty: bool,
    sent: i64,
}

impl Orders {
    fn side(&mut self, bid: bool) -> &mut BTreeMap<(i64, u64), (f64, f64, f64, u64)> { if bid { &mut self.bids } else { &mut self.asks } }
    fn level(&self, bid: bool, k: i64) -> f64 { (if bid { &self.bids } else { &self.asks }).range((k, 0)..=(k, u64::MAX)).map(|(_, o)| o.1).sum() }
    fn remove(&mut self, id: u64, touched: &mut Vec<(bool, i64, f64)>) {
        let Some((bid, k, n)) = self.ids.remove(&id) else { return };
        if let Some(o) = self.side(bid).remove(&(k, n)) { touched.push((bid, k, o.0)) }
    }
    /// One raw book entry: (id, price, signed amount; bids positive), price 0 deletes. Adds the touched levels.
    fn apply(&mut self, id: u64, px: f64, amt: f64, touched: &mut Vec<(bool, i64, f64)>) {
        if px == 0.0 || amt == 0.0 { return self.remove(id, touched) }
        let (bid, k, q) = (amt > 0.0, pkey(px), amt.abs());
        match self.ids.get(&id).copied() {
            // same price and side: the order keeps its place
            Some((b, k0, n)) if b == bid && k0 == k => { if let Some(o) = self.side(bid).get_mut(&(k, n)) { o.1 = q } }
            prev => {
                let initial = prev.and_then(|(b, k0, n)| (if b { &self.bids } else { &self.asks }).get(&(k0, n)).map(|o| o.2)).unwrap_or(q);
                self.remove(id, touched);
                self.next += 1;
                let n = self.next;
                self.side(bid).insert((k, n), (px, q, initial, id));
                self.ids.insert(id, (bid, k, n));
            }
        }
        touched.push((bid, k, px));
        // the venue keeps LEN orders per side; one pushed past the edge is gone from its window
        while self.bids.len() > LEN { let ((k, _), o) = self.bids.pop_first().unwrap(); self.ids.remove(&o.3); touched.push((true, k, o.0)) }
        while self.asks.len() > LEN { let ((k, _), o) = self.asks.pop_last().unwrap(); self.ids.remove(&o.3); touched.push((false, k, o.0)) }
    }
    /// L2 levels of the touched prices (qty 0 = gone), deduplicated
    fn levels(&self, touched: &[(bool, i64, f64)]) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let (mut bids, mut asks, mut seen) = (vec![], vec![], HashSet::new());
        for &(bid, k, px) in touched {
            if !seen.insert((bid, k)) { continue }
            let l = (px, self.level(bid, k));
            if bid { bids.push(l) } else { asks.push(l) }
        }
        (bids, asks)
    }
    fn all_levels(&self, bid: bool) -> Vec<(f64, f64)> {
        let mut out: Vec<(f64, f64)> = vec![];
        let mut last = None;
        let it: Box<dyn Iterator<Item = (&(i64, u64), &(f64, f64, f64, u64))>> = if bid { Box::new(self.bids.iter().rev()) } else { Box::new(self.asks.iter()) };
        for (&(k, _), o) in it {
            if last == Some(k) { out.last_mut().unwrap().1 += o.1 } else { out.push((o.0, o.1)); last = Some(k) }
        }
        out
    }
    fn top(&self) -> (f64, f64, f64, f64) {
        let lv = |bid: bool| {
            let k = if bid { self.bids.keys().next_back() } else { self.asks.keys().next() };
            k.map_or((0.0, 0.0), |&(k, n)| ((if bid { &self.bids } else { &self.asks })[&(k, n)].0, self.level(bid, k)))
        };
        let ((b, bq), (a, aq)) = (lv(true), lv(false));
        (b, bq, a, aq)
    }
    /// the best TOP orders per side, best first, queue order inside a level
    fn resting(&self) -> (Vec<l3::Resting>, Vec<l3::Resting>) {
        let r = |(&(_, n), o): (&(i64, u64), &(f64, f64, f64, u64))| l3::Resting { id: o.3, px: o.0, qty: o.1, initial: o.2, seq: n };
        // bids: best price first, inside a level the earliest order first
        let (mut bids, mut last) = (vec![], None);
        for k in self.bids.keys().rev().map(|x| x.0) {
            if last == Some(k) { continue }
            last = Some(k);
            bids.extend(self.bids.range((k, 0)..=(k, u64::MAX)).take(TOP - bids.len()).map(r));
            if bids.len() == TOP { break }
        }
        (bids, self.asks.iter().take(TOP).map(r).collect())
    }
}

/// Emits L2 deltas, Bbo on change and marks Orders due after `touched` levels moved.
fn emit_book(e: &Emit, sym: &str, ob: &mut Orders, ts: i64, snapshot: bool, touched: &[(bool, i64, f64)]) {
    let (bids, asks) = if snapshot { (ob.all_levels(true), ob.all_levels(false)) } else { ob.levels(touched) };
    if !snapshot && bids.is_empty() && asks.is_empty() { return }
    e.send(sym, ts, Event::Book { snapshot, bids, asks });
    let top = ob.top();
    if top != ob.bbo && top.0 > 0.0 && top.2 > 0.0 {
        ob.bbo = top;
        e.send(sym, ts, Event::Bbo { bid: top.0, bid_qty: top.1, ask: top.2, ask_qty: top.3 });
    }
    ob.dirty = true;
    let now = now_ms();
    if snapshot || now - ob.sent >= ORDERS_EVERY { send_orders(e, sym, ob, now) }
}

fn send_orders(e: &Emit, sym: &str, ob: &mut Orders, now: i64) {
    let (bids, asks) = ob.resting();
    e.send(sym, 0, Event::Orders { bids, asks });
    (ob.dirty, ob.sent) = (false, now);
}

// ------------------------------------------------------------------ frames

enum Ch { Trades(bool), Book }

/// One session's state; a new session (the server's `info` greeting) starts over.
#[derive(Default)]
struct St { chans: HashMap<i64, Ch>, book: Orders, flushed: i64 }

/// Frames of the socket of `sym`.
fn on_frame(e: &Emit, sym: &str, st: &mut St, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    if let Some(ev) = v["event"].as_str() {
        match ev {
            "info" if v.get("version").is_some() => *st = St::default(),
            "info" => eprintln!("[bitfinex] info {}: {}", v["code"], v["msg"]),
            "subscribed" => {
                let Some(id) = v["chanId"].as_i64() else { return };
                if v["symbol"] != sym { return }
                st.chans.insert(id, if v["channel"] == "trades" { Ch::Trades(false) } else { Ch::Book });
            }
            "error" => eprintln!("[bitfinex {:?}] error {}: {} {}", e.market, v["code"], v["msg"], v["symbol"]),
            _ => {}
        }
        return;
    }
    let Some(a) = v.as_array() else { return };
    let Some(ch) = a.first().and_then(Value::as_i64).and_then(|id| st.chans.get_mut(&id)) else { return };
    let ts = a.last().and_then(Value::as_i64).unwrap_or(0);
    match ch {
        Ch::Trades(started) => {
            // the first message is the recent-trades snapshot: history, not new prints
            if a[1].is_array() { *started = true; return }
            if a[1] != "te" || !*started { return }
            let tr = &a[2];
            let amt = num(&tr[2]);
            e.send(sym, tr[1].as_i64().unwrap_or(ts), Event::Trade { px: num(&tr[3]), qty: amt.abs(), side: if amt < 0.0 { Side::Sell } else { Side::Buy } });
        }
        Ch::Book => {
            let Some(rows) = a[1].as_array() else { return };
            let ob = &mut st.book;
            let mut touched = vec![];
            let snapshot = rows.first().is_some_and(Value::is_array);
            if snapshot {
                *ob = Orders::default();
                for r in rows { ob.apply(r[0].as_u64().unwrap_or(0), num(&r[1]), num(&r[2]), &mut touched) }
            } else {
                ob.apply(rows[0].as_u64().unwrap_or(0), num(&rows[1]), num(&rows[2]), &mut touched);
            }
            emit_book(e, sym, ob, ts, snapshot, &touched);
        }
    }
    // due Orders go out on the socket's next frame after a second (heartbeats come every 15 s per channel)
    let now = now_ms();
    if now - st.flushed >= ORDERS_EVERY {
        st.flushed = now;
        if st.book.dirty && now - st.book.sent >= ORDERS_EVERY { send_orders(e, sym, &mut st.book, now) }
    }
}

// ------------------------------------------------------------------ REST

/// Mark / index / hourly funding / OI rows of `status/deriv` for `sym`.
fn status_rows(e: &Emit, sym: &str, v: &Value) {
    for r in v.as_array().into_iter().flatten().filter(|r| r[0] == sym) {
        let ts = r[1].as_i64().unwrap_or(0);
        if let Some(mark) = opt_num(&r[15]) {
            e.send(sym, ts, Event::Mark { mark, index: opt_num(&r[4]), funding: opt_num(&r[12]).map(|f| f / 8.0), next_funding_ms: r[8].as_i64() });
        }
        if let Some(oi) = opt_num(&r[18]) { e.send(sym, ts, Event::OpenInterest { oi, oi_usd: None }) }
    }
}

/// Polls `status/deriv` every 15 s.
async fn poll_status(e: Emit, sym: String) {
    let url = format!("{REST}/status/deriv?keys={sym}");
    ws::poll("bitfinex status".into(), Duration::from_secs(15), || async {
        status_rows(&e, &sym, &ws::get_json(&url).await?);
        Ok(())
    }).await
}

pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_until(sub, 1, minutes, None).await }

/// Candles (`trade:<tf>:<sym>`, up to 10,000 a call), and for perps OI and settled funding from `status/deriv/<sym>/hist`
/// (about one sample a minute, 5,000 a call: longer windows get their newest 5,000 samples only).
pub async fn history_until(sub: &Sub, tf: u32, bars: usize, until: Option<i64>) -> anyhow::Result<History> {
    let sym = symbol(sub.market, &sub.base, &sub.quote);
    let (start, end) = window_tf_until(tf, bars, until);
    let url = format!("{REST}/candles/trade:{}:{sym}/hist?start={start}&end={end}&limit={}&sort=1", tf_name(tf), bars.min(10_000));
    let mut h = History { klines: parse_candles(&ws::get_json(&url).await?), ..Default::default() };
    if sub.market == Market::Perp {
        let url = format!("{REST}/status/deriv/{sym}/hist?start={start}&end={}&limit=5000&sort=1", end + tf as i64 * 60_000 - 1);
        (h.oi, h.funding) = parse_status_hist(&ws::get_json(&url).await?);
    }
    h.klines = continuous_tf(h.klines, end, bars, tf);
    Ok(h)
}

/// [MTS, OPEN, CLOSE, HIGH, LOW, VOLUME]
fn parse_candles(v: &Value) -> Vec<Kline> {
    v.as_array().into_iter().flatten().filter_map(|c| Some(Kline { t: c[0].as_i64()?, o: num(&c[1]), c: num(&c[2]), h: num(&c[3]), l: num(&c[4]), vol: num(&c[5]), buy: None })).collect()
}

/// status history rows (ascending) -> (OI every 5 minutes, hourly funding at each settlement: the CURRENT_FUNDING
/// in force before the event at NEXT_FUNDING_EVT_MTS).
/// ponytail: funding follows the venue's CURRENT_FUNDING field (as the stream's Mark.funding does), read as the
/// rate of the 8 h period ending at the next event
fn parse_status_hist(v: &Value) -> (Vec<(i64, f64)>, Vec<(i64, f64)>) {
    let (mut oi, mut funding): (Vec<(i64, f64)>, Vec<(i64, f64)>) = (vec![], vec![]);
    for r in v.as_array().into_iter().flatten() {
        let Some(t) = r[0].as_i64() else { continue };
        let b = t / 300_000 * 300_000;
        if let Some(x) = opt_num(&r[17]).filter(|_| oi.last().is_none_or(|l| l.0 < b)) { oi.push((b, x)) }
        if let (Some(evt), Some(f)) = (r[7].as_i64(), opt_num(&r[11])) {
            match funding.last_mut() { Some(l) if l.0 == evt => l.1 = f / 8.0, _ => funding.push((evt, f / 8.0)) }
        }
    }
    // the period still running at the last sample has not settled yet
    let last = v.as_array().and_then(|a| a.last()).and_then(|r| r[0].as_i64()).unwrap_or(i64::MIN);
    funding.retain(|p| p.0 <= last);
    (oi, funding)
}

// ------------------------------------------------------------------ socket

fn sub_msgs(sym: &str) -> [String; 2] {
    [
        json!({"event": "subscribe", "channel": "trades", "symbol": sym}).to_string(),
        json!({"event": "subscribe", "channel": "book", "symbol": sym, "prec": "R0", "len": LEN.to_string()}).to_string(),
    ]
}

/// One base: one socket for its symbol, plus the status poll on perps.
pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    if !matches!(sub.market, Market::Spot | Market::Perp) { return vec![] }
    let e = Emit { ex: Exchange::Bitfinex, market: sub.market, tx };
    let sym = symbol(sub.market, &sub.base, &sub.quote);
    let [trades, book] = sub_msgs(&sym);
    let sp = Spec::new(format!("bitfinex {:?} {sym}", sub.market).to_lowercase(), WS).sub(CONF).sub(trades).sub(book)
        .ping(Duration::from_secs(30), r#"{"event":"ping","cid":1}"#);
    let mut tasks = vec![];
    if sub.market == Market::Perp { tasks.push(tokio::spawn(poll_status(e.clone(), sym.clone()))) }
    let mut st = St::default();
    tasks.push(tokio::spawn(ws::run(sp, move |f| on_frame(&e, &sym, &mut st, f))));
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn collect(market: Market, f: impl FnOnce(&Emit)) -> Vec<Msg> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        f(&Emit { ex: Exchange::Bitfinex, market, tx });
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }
    /// frames recorded from api-pub on 2026-10-29 (BTC perp socket), snapshots cut short
    const FIXTURE: &str = include_str!("testdata/bitfinex.jsonl");

    #[test]
    fn symbols_map_codes() {
        assert_eq!(symbol(Market::Spot, "BTC", "USD"), "tBTCUSD");
        assert_eq!(symbol(Market::Spot, "ADA", "USDT"), "tADAUST");
        assert_eq!(symbol(Market::Spot, "AAVE", "USD"), "tAAVE:USD");
        assert_eq!(symbol(Market::Spot, "BCH", "USD"), "tBCHN:USD");
        assert_eq!(symbol(Market::Spot, "ALGO", "USD"), "tALGUSD");
        assert_eq!(symbol(Market::Perp, "BTC", "USDT"), "tBTCF0:USTF0");
        assert_eq!(symbol(Market::Perp, "ALGO", "USDT"), "tALGF0:USTF0");
        let m: Vec<Value> = sub_msgs("tBTCUSD").iter().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!((&m[0]["channel"], &m[1]["channel"], &m[1]["prec"], &m[1]["len"]), (&json!("trades"), &json!("book"), &json!("R0"), &json!("250")));
    }

    #[test]
    fn recorded_frames_give_trades_levels_bbo_and_orders() {
        let sym = "tBTCF0:USTF0";
        let mut st = St::default();
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let m = collect(Market::Perp, |e| for t in &lines { on_frame(e, sym, &mut st, Frame::Text(t)) });
        let trades: Vec<_> = m.iter().filter_map(|x| match x.ev { Event::Trade { px, qty, side } => Some((x.ts, px, qty, side)), _ => None }).collect();
        // the snapshot and the "tu" repeats are not prints
        assert_eq!(trades, [(1793291105561, 81509.0, 0.00073776, Side::Buy), (1793291109667, 81515.0, 0.00107219, Side::Buy)]);
        let books: Vec<_> = m.iter().filter_map(|x| match &x.ev { Event::Book { snapshot, bids, asks } => Some((*snapshot, bids.clone(), asks.clone())), _ => None }).collect();
        assert_eq!(books[0], (true, vec![(81504.0, 0.03684656), (81498.0, 0.00030999), (81487.0, 0.03685447)], vec![(81519.0, 0.21277785), (81526.0, 0.01), (81530.0, 0.21277785)]));
        // deletes of orders outside the cut snapshot touch nothing; each new order is one level delta
        assert_eq!(books[1..], [(false, vec![(81387.0, 0.008)], vec![]), (false, vec![(78909.0, 0.12174354)], vec![]), (false, vec![], vec![(81550.0, 0.1098572)])]);
        let bbo: Vec<_> = m.iter().filter(|x| matches!(x.ev, Event::Bbo { .. })).collect();
        assert!(matches!(bbo[..], [Msg { ev: Event::Bbo { bid: 81504.0, ask: 81519.0, .. }, .. }]));
        let Some(Event::Orders { bids, asks }) = m.iter().map(|x| &x.ev).find(|e| matches!(e, Event::Orders { .. })) else { panic!("no Orders") };
        assert_eq!(bids.iter().map(|o| (o.id, o.px)).collect::<Vec<_>>(), [(247549735613, 81504.0), (247539286001, 81498.0), (247549735549, 81487.0)]);
        assert_eq!(asks[0].id, 247549735365);
        // a new session starts over: the old channel ids mean nothing
        let m = collect(Market::Perp, |e| for t in [lines[0], r#"[440951,"te",[1,2,3.0,4.0],5]"#] { on_frame(e, sym, &mut st, Frame::Text(t)) });
        assert!(m.is_empty());
    }

    #[test]
    fn raw_book_keeps_queue_order_amends_and_the_window() {
        let mut o = Orders::default();
        let mut t = vec![];
        o.apply(1, 100.0, 1.0, &mut t);
        o.apply(2, 100.0, 2.0, &mut t);
        o.apply(3, 101.0, -1.0, &mut t);
        // a size change keeps the place; a new price goes to the back of its level, keeping the first size as initial
        o.apply(1, 100.0, 0.5, &mut t);
        o.apply(4, 100.0, 1.0, &mut t);
        o.apply(2, 99.0, 2.0, &mut t);
        let (bids, asks) = o.resting();
        assert_eq!(bids.iter().map(|r| (r.id, r.px, r.qty, r.initial)).collect::<Vec<_>>(), [(1, 100.0, 0.5, 1.0), (4, 100.0, 1.0, 1.0), (2, 99.0, 2.0, 2.0)]);
        assert_eq!((asks.len(), o.top()), (1, (100.0, 1.5, 101.0, 1.0)));
        // an order moved out of a level reports both levels
        t.clear();
        o.apply(4, 98.0, 1.0, &mut t);
        assert_eq!(o.levels(&t), (vec![(100.0, 0.5), (98.0, 1.0)], vec![]));
        // the window: past LEN orders a side loses its worst one (the venue no longer reports it)
        for i in 0..LEN as u64 { o.apply(100 + i, 50.0 - i as f64 * 0.01, 1.0, &mut t) }
        assert_eq!((o.bids.len(), o.ids.len()), (LEN, LEN + 1));
        assert!(o.bids.keys().next().unwrap().0 > pkey(50.0 - (LEN - 1) as f64 * 0.01));
        t.clear();
        o.apply(1, 0.0, -1.0, &mut t);
        assert_eq!(o.levels(&t), (vec![(100.0, 0.0)], vec![]));
    }

    #[test]
    fn status_rows_and_history_parse() {
        // recorded 2026-10-29: status/deriv?keys=tBTCF0:USTF0 (the row) and its /hist (the same row without the key)
        let row = json!([1793290525000i64, null, 81753.91815342, 81688, null, 60752727.23092484, null, 1793318400000i64, 0.00002114, 294, null, 0.00016938, null, null, 81689.1084, null, null, 9228.04040153, null, null, null, 0.0005, 0.0025]);
        let mut keyed = row.as_array().unwrap().clone();
        keyed.insert(0, json!("tBTCF0:USTF0"));
        let m = collect(Market::Perp, |e| status_rows(e, "tBTCF0:USTF0", &json!([keyed, ["tETHF0:USTF0", 1]])));
        assert!(matches!(m[0].ev, Event::Mark { mark, index: Some(i), funding: Some(f), next_funding_ms: Some(1793318400000) } if mark == 81689.1084 && i == 81688.0 && (f - 0.00016938 / 8.0).abs() < 1e-15));
        assert!(matches!(m[1].ev, Event::OpenInterest { oi, .. } if oi == 9228.04040153));
        assert_eq!((m.len(), m[0].ts), (2, 1793290525000));
        let mut later = row.clone();
        later[0] = json!(1793318460000i64);
        later[7] = json!(1793347200000i64);
        let (oi, funding) = parse_status_hist(&json!([row, row, later]));
        assert_eq!(oi.iter().map(|p| p.0).collect::<Vec<_>>(), [1793290500000, 1793318400000]);
        // the period that settles after the last sample is left out
        assert_eq!(funding, [(1793318400000, 0.00016938 / 8.0)]);
        let k = parse_candles(&json!([[1793290500000i64, 81726, 81702, 81746, 81702, 0.1606261]]));
        assert_eq!((k[0].o, k[0].c, k[0].h, k[0].l, k[0].vol), (81726.0, 81702.0, 81746.0, 81702.0, 0.1606261));
    }
}
