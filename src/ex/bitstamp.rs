//! Bitstamp spot on WS v2 (wss://ws.bitstamp.net), REST v2 on www.bitstamp.net.
//!
//! Every market takes two channels: `live_trades_<m>` (Trade, plus `OrderFill` for the resting order it hit: trades
//! carry both order ids) and `detail_order_book_<m>`, a full snapshot of the top 100 orders per side with their ids
//! on every book change. From that snapshot come the L2 `Book` (orders summed per price, snapshot = replace), `Bbo`
//! when the top moves and `Orders` (exact L3, at most once a second per market).
//! `live_orders_<m>` (each order's created / changed / deleted event) is not used: it has no snapshot, and the
//! unified model carries order-level books only as top-of-book replacements (`Event::Orders`), which the detail
//! book already is.
//!
//! Limits (https://www.bitstamp.net/websocket/v2/, https://www.bitstamp.net/api/): 1,024 channels per connection,
//! subscribe messages under 512 bytes; REST 400 requests a second and 10,000 per 10 minutes.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::task::JoinHandle;

const WS: &str = "wss://ws.bitstamp.net";
const REST: &str = "https://www.bitstamp.net/api/v2";
/// orders per side in `Event::Orders` (the detail book has 100)
const TOP: usize = 50;
/// `Orders` at most this often per market (ms)
const ORDERS_EVERY: i64 = 1000;

/// microtimestamp string (µs) -> ms
fn micros(v: &Value) -> i64 { v.as_str().and_then(|s| s.parse::<i64>().ok()).map_or(0, |u| u / 1000) }

/// [price, amount, order id] rows -> resting orders in the venue's order (best first, time priority inside a level)
fn rows(v: &Value) -> Vec<l3::Resting> {
    v.as_array().into_iter().flatten().enumerate().filter_map(|(i, r)| {
        let qty = num(&r[1]);
        Some(l3::Resting { id: r[2].as_str()?.parse().ok()?, px: num(&r[0]), qty, initial: qty, seq: i as u64 })
    }).collect()
}

/// orders summed per price, in list order
fn levels(orders: &[l3::Resting]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = vec![];
    for o in orders {
        match out.last_mut() { Some(l) if l.0 == o.px => l.1 += o.qty, _ => out.push((o.px, o.qty)) }
    }
    out
}

/// Per market: the last Bbo sent, the newest unsent Orders and when Orders last went out.
#[derive(Default)]
struct Mk { bbo: (f64, f64, f64, f64), pending: Option<(Vec<l3::Resting>, Vec<l3::Resting>)>, sent: i64 }

fn on_frame(e: &Emit, m: &str, st: &mut Mk, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    let ch = v["channel"].as_str().unwrap_or("");
    match v["event"].as_str().unwrap_or("") {
        "trade" if ch.strip_prefix("live_trades_") == Some(m) => {
            let d = &v["data"];
            let (ts, qty) = (micros(&d["microtimestamp"]), num(&d["amount"]));
            // type 0 = a buy taker, which hit a resting sell
            let buy = d["type"].as_i64() == Some(0);
            e.send(m, ts, Event::Trade { px: num(&d["price"]), qty, side: if buy { Side::Buy } else { Side::Sell } });
            if let Some(id) = d[if buy { "sell_order_id" } else { "buy_order_id" }].as_u64() { e.send(m, ts, Event::OrderFill { id, qty }) }
        }
        "data" if ch.strip_prefix("detail_order_book_") == Some(m) => {
            let d = &v["data"];
            let ts = micros(&d["microtimestamp"]);
            let (bids, asks) = (rows(&d["bids"]), rows(&d["asks"]));
            e.send(m, ts, Event::Book { snapshot: true, bids: levels(&bids), asks: levels(&asks) });
            let lv = |o: &[l3::Resting]| o.first().map_or((0.0, 0.0), |f| (f.px, o.iter().take_while(|x| x.px == f.px).map(|x| x.qty).sum()));
            let ((b, bq), (a, aq)) = (lv(&bids), lv(&asks));
            if (b, bq, a, aq) != st.bbo && b > 0.0 && a > 0.0 {
                st.bbo = (b, bq, a, aq);
                e.send(m, ts, Event::Bbo { bid: b, bid_qty: bq, ask: a, ask_qty: aq });
            }
            let orders = (bids.into_iter().take(TOP).collect(), asks.into_iter().take(TOP).collect());
            if now_ms() - st.sent >= ORDERS_EVERY { send_orders(e, m, st, orders) } else { st.pending = Some(orders) }
        }
        "trade" | "data" | "bts:subscription_succeeded" | "bts:heartbeat" | "bts:unsubscription_succeeded" => {}
        "bts:request_reconnect" => eprintln!("[bitstamp] the server asks to reconnect (it closes the socket shortly)"),
        ev => eprintln!("[bitstamp] {ev} {ch}: {}", v["data"]),
    }
    // held-back Orders go out on the socket's next frame once their second is over
    if now_ms() - st.sent >= ORDERS_EVERY { if let Some(o) = st.pending.take() { send_orders(e, m, st, o) } }
}

fn send_orders(e: &Emit, m: &str, st: &mut Mk, (bids, asks): (Vec<l3::Resting>, Vec<l3::Resting>)) {
    e.send(m, 0, Event::Orders { bids, asks });
    (st.pending, st.sent) = (None, now_ms());
}

// ------------------------------------------------------------------ REST

pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_until(sub, 1, minutes, None).await }

/// `ohlc` (step in seconds, at most 1,000 a call, the `limit` candles ending at `end`). Spot only: no OI or funding.
pub async fn history_until(sub: &Sub, tf: u32, bars: usize, until: Option<i64>) -> anyhow::Result<History> {
    if sub.market != Market::Spot { return Ok(History::default()) }
    let (_, end) = window_tf_until(tf, bars, until);
    let url = format!("{REST}/ohlc/{}/?step={}&limit={}&end={}", market_symbol(&sub.base, &sub.quote), tf * 60, bars.clamp(1, 1000), end / 1000);
    let k = parse_ohlc(&ws::get_json(&url).await?);
    Ok(History { klines: continuous_tf(k, end, bars, tf), ..Default::default() })
}

fn parse_ohlc(v: &Value) -> Vec<Kline> {
    v["data"]["ohlc"].as_array().into_iter().flatten().filter_map(|c| {
        let t = c["timestamp"].as_str()?.parse::<i64>().ok()? * 1000;
        Some(Kline { t, o: num(&c["open"]), h: num(&c["high"]), l: num(&c["low"]), c: num(&c["close"]), vol: num(&c["volume"]), buy: None })
    }).collect()
}

fn market_symbol(base: &str, quote: &str) -> String { format!("{base}{quote}").to_lowercase() }

// ------------------------------------------------------------------ socket

fn sub_msgs(m: &str) -> [String; 2] {
    ["live_trades_", "detail_order_book_"].map(|c| json!({"event": "bts:subscribe", "data": {"channel": format!("{c}{m}")}}).to_string())
}

/// One base, spot only: one socket for `<base><quote>`.
pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    if sub.market != Market::Spot { return vec![] }
    let e = Emit { ex: Exchange::Bitstamp, market: sub.market, tx };
    let m = market_symbol(&sub.base, &sub.quote);
    let [trades, book] = sub_msgs(&m);
    let sp = Spec::new(format!("bitstamp {m}"), WS).sub(trades).sub(book).ping(Duration::from_secs(30), r#"{"event":"bts:heartbeat"}"#);
    let mut st = Mk::default();
    vec![tokio::spawn(ws::run(sp, move |f| on_frame(&e, &m, &mut st, f)))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// frames recorded from ws.bitstamp.net on 2026-10-29 (btcusd), the detail books cut to 4 orders a side
    const FIXTURE: &str = include_str!("testdata/bitstamp.jsonl");

    fn collect(f: impl FnOnce(&Emit)) -> Vec<Msg> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        f(&Emit { ex: Exchange::Bitstamp, market: Market::Spot, tx });
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn subscriptions_fit_the_message_size() {
        let m = sub_msgs(&market_symbol("BTC", "USD"));
        assert!(m.iter().all(|s| s.len() < 512));
        assert_eq!(serde_json::from_str::<Value>(&m[1]).unwrap(), json!({"event": "bts:subscribe", "data": {"channel": "detail_order_book_btcusd"}}));
    }

    #[test]
    fn recorded_frames_give_books_bbo_orders_trades_and_maker_fills() {
        let mut st = Mk::default();
        let m = collect(|e| for t in FIXTURE.lines() { on_frame(e, "btcusd", &mut st, Frame::Text(t)) });
        let Some(Event::Book { snapshot: true, bids, asks }) = m.iter().map(|x| &x.ev).find(|e| matches!(e, Event::Book { .. })) else { panic!("no Book") };
        // orders summed per price
        assert_eq!(bids[..2], [(81348.16, 0.45), (81346.85, 0.25)]);
        assert_eq!(asks[0].0, 81348.17);
        assert!(matches!(m[1].ev, Event::Bbo { bid: 81348.16, bid_qty, ask: 81348.17, .. } if (bid_qty - 0.45).abs() < 1e-12));
        assert_eq!(m[0].ts, 1793291073774);
        // Orders once on the first book, the second book within the second stays pending
        let orders: Vec<_> = m.iter().filter_map(|x| match &x.ev { Event::Orders { bids, asks } => Some((bids.clone(), asks.len())), _ => None }).collect();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].0.iter().map(|o| (o.id, o.seq)).collect::<Vec<_>>(), [(2066252148604930, 0), (2066252156014592, 1), (2066252161609735, 2), (2066252181471232, 3)]);
        assert!(st.pending.is_some());
        let tr: Vec<_> = m.iter().filter_map(|x| match x.ev { Event::Trade { px, qty, side } => Some((x.ts, px, qty, side)), Event::OrderFill { id, qty } => Some((x.ts, id as f64, qty, Side::Buy)), _ => None }).collect();
        // type 1 = a sell taker: the fill is the resting buy's; type 0 hits the resting sell
        assert_eq!(tr[..2], [(1793291075294, 81348.16, 0.00021647, Side::Sell), (1793291075294, 2066252161609735u64 as f64, 0.00021647, Side::Buy)]);
        assert!(matches!(m.last().unwrap().ev, Event::OrderFill { id: 2066252230168580, .. }));
    }

    #[test]
    fn ohlc_parses() {
        let v = json!({"data": {"pair": "BTC/USD", "ohlc": [{"timestamp": "1793290440", "open": "81713", "high": "81747", "low": "81657", "close": "81728", "volume": "0.86408605"}]}});
        let k = parse_ohlc(&v);
        assert_eq!((k[0].t, k[0].o, k[0].h, k[0].l, k[0].c, k[0].vol), (1793290440000, 81713.0, 81747.0, 81657.0, 81728.0, 0.86408605));
    }
}
