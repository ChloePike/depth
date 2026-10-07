//! Lighter (zkLighter perp DEX): USDC-settled perps, market_id resolved from /api/v1/orderBooks.
//! A USDT or USDC quote maps to the USDC-settled perp (perps are keyed by base symbol only).
//! WS wss://mainnet.zklighter.elliot.ai/stream: trade/{id}, order_book/{id}, market_stats/{id}.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const REST: &str = "https://mainnet.zklighter.elliot.ai/api/v1";
const WS: &str = "wss://mainnet.zklighter.elliot.ai/stream";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Lighter, market: sub.market, tx };
    match sub.market {
        Market::Perp => {
            let base = sub.base.to_uppercase();
            vec![tokio::spawn(async move {
                let id = loop {
                    match market_id(&base).await {
                        Ok(Some(id)) => break id,
                        Ok(None) => { eprintln!("[lighter perp] no {base} perp market"); return }
                        Err(err) => { eprintln!("[lighter perp] orderBooks: {err:#}"); tokio::time::sleep(Duration::from_secs(5)).await }
                    }
                };
                // a book gap fires `gap`, which drops the session and reconnects for a fresh snapshot
                loop {
                    let gap = Arc::new(Notify::new());
                    let mut spec = Spec::new("lighter perp", WS).ping(Duration::from_secs(30), r#"{"type":"ping"}"#);
                    for ch in ["trade", "order_book", "market_stats"] {
                        spec = spec.sub(json!({"type": "subscribe", "channel": format!("{ch}/{id}")}).to_string());
                    }
                    let (e, base, g) = (e.clone(), base.clone(), gap.clone());
                    let mut st = (None, Book::default());
                    tokio::select! {
                        _ = ws::run(spec, move |f| on_msg(&e, &base, &mut st, &g, f)) => {}
                        _ = gap.notified() => eprintln!("[lighter perp] book sequence gap, reconnecting"),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })]
        }
        // spot exists (ETH/USDC, LINK/USDC, ...) but there is no BTC spot market and no margin lending
        Market::Spot | Market::Margin => vec![],
        // no options on Lighter
        Market::Option => vec![],
    }
}

async fn market_id(base: &str) -> anyhow::Result<Option<i64>> {
    let v = ws::get_json(&format!("{REST}/orderBooks")).await?;
    Ok(v["order_books"].as_array().into_iter().flatten()
        .find(|o| o["market_type"] == "perp" && o["symbol"] == base && o["status"] == "active")
        .and_then(|o| o["market_id"].as_i64()))
}

fn levels_obj(v: &Value) -> Vec<(f64, f64)> {
    v.as_array().map(|a| a.iter().map(|l| (num(&l["price"]), num(&l["size"]))).collect()).unwrap_or_default()
}

/// emits the book message and a Bbo when the top of book changed
fn book_msg(e: &Emit, sym: &str, ts: i64, book: &mut Book, snapshot: bool, b: &Value) {
    let (bids, asks) = (levels_obj(&b["bids"]), levels_obj(&b["asks"]));
    let top = (book.best_bid(), book.best_ask());
    book.apply(snapshot, &bids, &asks);
    if let (Some((bid, bid_qty)), Some((ask, ask_qty))) = (book.best_bid(), book.best_ask()) {
        if snapshot || top != (book.best_bid(), book.best_ask()) { e.send(sym, ts, Event::Bbo { bid, bid_qty, ask, ask_qty }); }
    }
    e.send(sym, ts, Event::Book { snapshot, bids, asks });
}

/// `st.0`: nonce of the last applied book message; each update's begin_nonce must equal it.
/// `st.1`: local book, used to derive Bbo (Lighter has no BBO channel).
fn on_msg(e: &Emit, sym: &str, st: &mut (Option<i64>, Book), gap: &Notify, f: Frame) {
    let (nonce, book) = (&mut st.0, &mut st.1);
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    let ts = v["timestamp"].as_i64().unwrap_or(0);
    match v["type"].as_str().unwrap_or("") {
        // "subscribed/trade" holds the last 50 trades and liquidations: history, skipped so CVD is not
        // double counted on reconnect
        "update/trade" => {
            // is_maker_ask: the seller was the maker, so the aggressor bought
            let side = |t: &Value| if t["is_maker_ask"] == true { Side::Buy } else { Side::Sell };
            for t in v["trades"].as_array().into_iter().flatten() {
                e.send(sym, t["timestamp"].as_i64().unwrap_or(ts), Event::Trade { px: num(&t["price"]), qty: num(&t["size"]), side: side(t) });
            }
            // the liquidated account is the taker, so its order side is the aggressor side
            // (a liquidated short buys back: Buy). `trades` only holds type "trade" fills, so
            // liquidation fills are also emitted as trades.
            for t in v["liquidation_trades"].as_array().into_iter().flatten() {
                let (px, qty, side) = (num(&t["price"]), num(&t["size"]), side(t));
                let ts = t["timestamp"].as_i64().unwrap_or(ts);
                e.send(sym, ts, Event::Trade { px, qty, side });
                e.send(sym, ts, Event::Liquidation { px, qty, side });
            }
        }
        "subscribed/order_book" => {
            let b = &v["order_book"];
            *nonce = b["nonce"].as_i64();
            book_msg(e, sym, ts, book, true, b);
        }
        "update/order_book" => {
            let b = &v["order_book"];
            if nonce.is_none() || b["begin_nonce"].as_i64() != *nonce {
                *nonce = None;
                gap.notify_one();
                return;
            }
            *nonce = b["nonce"].as_i64();
            book_msg(e, sym, ts, book, false, b);
        }
        "update/market_stats" | "subscribed/market_stats" => {
            let m = &v["market_stats"];
            let mark = num(&m["mark_price"]);
            // current_funding_rate is the predicted hourly rate in percent; funding settles every hour
            let funding = opt_num(&m["current_funding_rate"]).map(|r| r / 100.0);
            let next = m["funding_timestamp"].as_i64().map(|t| t + 3_600_000);
            e.send(sym, ts, Event::Mark { mark, index: opt_num(&m["index_price"]), funding, next_funding_ms: next });
            // open_interest here is USD notional (orderBookDetails gives coins); convert at mark
            let oi_usd = num(&m["open_interest"]);
            if mark > 0.0 { e.send(sym, ts, Event::OpenInterest { oi: oi_usd / mark, oi_usd: Some(oi_usd) }); }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn book_sequence_and_bbo() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (e, gap, mut st) = (Emit { ex: Exchange::Lighter, market: Market::Perp, tx }, Notify::new(), (None, Book::default()));
        let mut feed = |s: &str| on_msg(&e, "BTC", &mut st, &gap, Frame::Text(s));
        feed(r#"{"type":"subscribed/order_book","order_book":{"nonce":5,"bids":[{"price":"100","size":"1"}],"asks":[{"price":"101","size":"2"}]}}"#);
        feed(r#"{"type":"update/order_book","order_book":{"begin_nonce":5,"nonce":7,"bids":[{"price":"100","size":"0"},{"price":"99","size":"3"}],"asks":[]}}"#);
        feed(r#"{"type":"update/order_book","order_book":{"begin_nonce":8,"nonce":9,"bids":[],"asks":[]}}"#);
        let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).map(|m| m.ev).collect();
        assert_eq!(kinds.len(), 4, "bbo+book, bbo+book, gap dropped");
        assert!(matches!(kinds[2], Event::Bbo { bid: 99.0, ask: 101.0, .. }));
        assert!(tokio::time::timeout(Duration::from_millis(10), gap.notified()).await.is_ok(), "gap must notify");
    }
}

/// Recent REST history for the chart (one-minute candles); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `bars` candles of `tf` minutes; see `ex::history_tf`.
/// /candles (resolution 1m/5m/15m/1h/4h, 500 per request, newest within the range; v = base volume,
/// no taker split) and /fundings (hourly, unsigned percent `rate` + `direction`, 500 hours per
/// request; the server caps count_back near 750). Perp only.
// ponytail: no public OI, long/short or taker history on Lighter; those stay empty
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let res = match tf { 1 => "1m", 5 => "5m", 15 => "15m", 60 => "1h", 240 => "4h", _ => anyhow::bail!("lighter has no {tf}m candles") };
    if sub.market != Market::Perp { return Ok(History::default()) }
    let Some(id) = market_id(&sub.base.to_uppercase()).await? else { return Ok(History::default()) };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    let page = 500 * step;
    let pages = (start..=end).step_by(page as usize).map(|s| {
        let url = format!("{REST}/candles?market_id={id}&resolution={res}&start_timestamp={s}&end_timestamp={}&count_back=500", (s + page - 1).min(end + step - 1));
        async move { ws::get_json(&url).await }
    });
    // funding settles hourly, in seconds here; the server returns the newest `count_back` rows before
    // end_timestamp and ignores start_timestamp, so count_back is sized to each page (+1: pages lost
    // their edge row otherwise; the overlap is deduped below)
    const FPAGE: i64 = 500 * 3600;
    let (fstart, fend) = (start / 1000 - 3600, now_ms() / 1000);
    let fpages = (fstart..=fend).step_by(FPAGE as usize).map(|s| {
        let to = (s + FPAGE - 1).min(fend);
        let url = format!("{REST}/fundings?market_id={id}&resolution=1h&start_timestamp={s}&end_timestamp={to}&count_back={}", (to - s) / 3600 + 2);
        async move { ws::get_json(&url).await }
    });
    let (k, f) = tokio::join!(futures_util::future::try_join_all(pages), futures_util::future::try_join_all(fpages));
    let klines = k?.iter().flat_map(|v| v["c"].as_array().cloned().unwrap_or_default()).map(|c| Kline {
        t: c["t"].as_i64().unwrap_or(0), o: num(&c["o"]), h: num(&c["h"]), l: num(&c["l"]), c: num(&c["c"]), vol: num(&c["v"]), buy: None,
    }).collect();
    // percent -> fraction, like the live connector; direction "short" means shorts pay (negative)
    let mut funding: Vec<(i64, f64)> = f?.iter().flat_map(|v| v["fundings"].as_array().cloned().unwrap_or_default()).map(|r| {
        let sign = if r["direction"] == "short" { -1.0 } else { 1.0 };
        (r["timestamp"].as_i64().unwrap_or(0) * 1000, sign * num(&r["rate"]) / 100.0)
    }).filter(|(t, r)| r.is_finite() && *t >= fstart * 1000).collect();
    funding.sort_by_key(|x| x.0);
    funding.dedup_by_key(|x| x.0);
    Ok(History { klines: continuous_tf(klines, end, bars, tf), funding, ..Default::default() })
}
