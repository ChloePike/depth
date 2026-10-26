//! Deribit (public JSON-RPC over wss://www.deribit.com/ws/api/v2, REST under /api/v2): the BTC and
//! ETH option chains only so far.
//!
//! Options are inverse: premiums are quoted in the underlying coin, so every price is emitted in USD
//! as the coin price times the spot index carried by the same message (like OKX). Sizes are already
//! base units (contract_size 1). Deribit IVs are percents, emitted as fractions. One connection
//! carries `ticker.<instrument>.agg2` (about once a second) for every listed option (mark, IV, greeks, best bid / ask,
//! open interest, per-expiry forward) and one `trades.option.<BASE>.100ms`.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

// T1_TESTNET: test.deribit.com, so the chain the ticket fills prices from is the book the orders hit
fn ws_url() -> &'static str { if crate::trade::testnet() { "wss://test.deribit.com/ws/api/v2" } else { "wss://www.deribit.com/ws/api/v2" } }
const PING: &str = r#"{"jsonrpc":"2.0","id":0,"method":"public/test"}"#;
/// channels per (un)subscribe request (a request costs 3000 credits of a 50000 pool, so a full chain is 4 requests)
const CHUNK: usize = 250;

/// Whether Deribit streams anything for this market and coin (the UI shows it idle, not down, when not).
/// ponytail: coin-settled BTC / ETH options only; the *_USDC linear options repeat the same
/// strikes (the chain is keyed by strike), perps and dated futures come in later batches
pub fn streams(market: Market, base: &str) -> bool { market == Market::Option && matches!(base.to_uppercase().as_str(), "BTC" | "ETH") }

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let base = sub.base.to_uppercase();
    if !streams(sub.market, &base) { return vec![]; }
    vec![tokio::spawn(option_feed(Emit { ex: Exchange::Deribit, market: Market::Option, tx }, base))]
}

fn rpc(method: &str, channels: &[String]) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {"channels": channels}}).to_string()
}

/// One connection for the whole chain; the instrument list is re-read every 10 minutes and the live
/// socket follows it (daily expiries and new weeklies), see `swap`.
async fn option_feed(e: Emit, base: String) {
    let live = Arc::new(Mutex::new(Vec::<String>::new()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let l = live.clone();
    let spec = Spec::new("deribit option", ws_url()).ping(Duration::from_secs(30), PING)
        .dynamic(move || l.lock().unwrap().chunks(CHUNK).map(|c| rpc("public/subscribe", c)).collect(), rx);
    let (e2, mut last) = (e.clone(), HashMap::new());
    let stream = ws::run(spec, move |f| on_option(&e2, &mut last, f));
    let chain = async {
        loop {
            let list = match instruments(&base).await {
                Ok(l) => l,
                Err(err) => { eprintln!("[deribit optinfo] {err:#}"); tokio::time::sleep(Duration::from_secs(10)).await; continue }
            };
            let mut chans = vec![format!("trades.option.{base}.100ms")];
            for o in &list {
                let Some((s, info)) = option_info(o, &base) else { continue };
                chans.push(format!("ticker.{s}.agg2"));
                e.send(&s, 0, info);
            }
            chans.sort();
            let msgs = swap(&mut live.lock().unwrap(), chans);
            for m in msgs { let _ = tx.send(m); }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    };
    tokio::join!(stream, chain);
}

/// The requests that bring the live socket to `chans` (sorted): unsubscribe what left the chain,
/// then subscribe the whole list, not only the additions. Subscribe is idempotent, so a chunk the
/// venue refused (rate limit, error reply) is retried at the next refresh instead of never.
fn swap(cur: &mut Vec<String>, chans: Vec<String>) -> Vec<String> {
    let old: Vec<String> = cur.iter().filter(|c| chans.binary_search(c).is_err()).cloned().collect();
    let msgs = old.chunks(CHUNK).map(|c| rpc("public/unsubscribe", c)).chain(chans.chunks(CHUNK).map(|c| rpc("public/subscribe", c))).collect();
    *cur = chans;
    msgs
}

/// public/get_instruments costs 10000 credits (1 per second allowed), hence the slow refresh
async fn instruments(base: &str) -> anyhow::Result<Vec<Value>> {
    let v = ws::get_json(&format!("{}/api/v2/public/get_instruments?currency={base}&kind=option&expired=false", crate::trade::deribit::url())).await?;
    v["result"].as_array().cloned().filter(|l| !l.is_empty()).ok_or_else(|| anyhow::anyhow!("no option instruments: {}", v["error"]))
}

/// (native symbol, OptionInfo) of an open coin-settled option: BTC-26OCT26-72000-C
fn option_info(o: &Value, base: &str) -> Option<(String, Event)> {
    // linear (USDC) options quote their premium in USDC: converting that with the coin index would be wrong
    if o["quote_currency"] != base || o["state"] != "open" { return None; }
    Some((o["instrument_name"].as_str()?.to_string(), Event::OptionInfo {
        underlying: base.into(), expiry_ms: o["expiration_timestamp"].as_i64()?, strike: opt_num(&o["strike"])?, call: o["option_type"] == "call" }))
}

/// `last`: (forward, open interest) last sent per instrument; both change rarely, so they go out on change only
fn on_option(e: &Emit, last: &mut HashMap<String, (f64, f64)>, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    if v["error"].is_object() { eprintln!("[deribit option] {t}"); }
    let (ch, d) = (v["params"]["channel"].as_str().unwrap_or(""), &v["params"]["data"]);
    if ch.starts_with("ticker.") { ticker(e, last, d) }
    else if ch.starts_with("trades.") {
        for t in d.as_array().into_iter().flatten() {
            let side = if t["direction"] == "sell" { Side::Sell } else { Side::Buy };
            e.send(t["instrument_name"].as_str().unwrap_or(""), t["timestamp"].as_i64().unwrap_or(0),
                Event::Trade { px: num(&t["price"]) * num(&t["index_price"]), qty: num(&t["amount"]), side });
        }
    }
}

fn ticker(e: &Emit, last: &mut HashMap<String, (f64, f64)>, d: &Value) {
    let (s, ts, idx) = (d["instrument_name"].as_str().unwrap_or(""), d["timestamp"].as_i64().unwrap_or(0), num(&d["index_price"]));
    if !(idx > 0.0) { return; }
    e.send(s, ts, Event::Mark { mark: num(&d["mark_price"]) * idx, index: Some(idx), funding: None, next_funding_ms: None });
    // underlying_price = the expiry's forward (USD)
    let l = last.entry(s.into()).or_default();
    if let Some(px) = opt_num(&d["underlying_price"]).filter(|p| *p > 0.0) {
        if std::mem::replace(&mut l.0, px) != px { e.send(s, ts, Event::Forward { px }); }
    }
    if let Some(oi) = opt_num(&d["open_interest"]) {
        if std::mem::replace(&mut l.1, oi) != oi { e.send(s, ts, Event::OpenInterest { oi, oi_usd: None }); }
    }
    // IVs are percents, 0 when that side has no quote
    let iv = |k: &str| opt_num(&d[k]).filter(|x| *x > 0.01).map(|x| x / 100.0);
    let g = &d["greeks"];
    e.send(s, ts, Event::Greeks { mark_iv: num(&d["mark_iv"]) / 100.0, bid_iv: iv("bid_iv"), ask_iv: iv("ask_iv"),
        delta: num(&g["delta"]), gamma: num(&g["gamma"]), vega: num(&g["vega"]), theta: num(&g["theta"]) });
    // sent even when empty, so a pulled quote does not linger in the chain
    let z = |k: &str| opt_num(&d[k]).unwrap_or(0.0);
    e.send(s, ts, Event::Bbo { bid: z("best_bid_price") * idx, bid_qty: z("best_bid_amount"), ask: z("best_ask_price") * idx, ask_qty: z("best_ask_amount") });
}

pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

pub async fn history_tf(_sub: &Sub, _tf: u32, _bars: usize) -> anyhow::Result<History> { Ok(History::default()) }

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(f: impl FnOnce(&Emit)) -> Vec<Msg> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        f(&Emit { ex: Exchange::Deribit, market: Market::Option, tx });
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn instruments_keep_coin_settled_options_only() {
        let inv = json!({"state": "open", "instrument_name": "BTC-26OCT26-72000-C", "expiration_timestamp": 1793001600000i64,
            "strike": 7.2e4, "option_type": "call", "quote_currency": "BTC", "settlement_currency": "BTC", "contract_size": 1.0});
        let Some((s, Event::OptionInfo { underlying, expiry_ms, strike, call })) = option_info(&inv, "BTC") else { panic!("inverse option dropped") };
        assert_eq!((s.as_str(), underlying.as_str(), expiry_ms, strike, call), ("BTC-26OCT26-72000-C", "BTC", 1793001600000, 72000.0, true));
        let lin = json!({"state": "open", "instrument_name": "BTC_USDC-26OCT26-72000-C", "expiration_timestamp": 1793001600000i64,
            "strike": 72000.0, "option_type": "call", "quote_currency": "USDC", "settlement_currency": "USDC", "contract_size": 1.0});
        assert!(option_info(&lin, "BTC").is_none());
        let put = json!({"state": "open", "instrument_name": "ETH-30OCT26-2000-P", "expiration_timestamp": 1793347200000i64, "strike": 2000.0, "option_type": "put", "quote_currency": "ETH"});
        assert!(matches!(option_info(&put, "ETH"), Some((_, Event::OptionInfo { call: false, .. }))));
        assert!(option_info(&json!({"state": "closed", "quote_currency": "BTC", "instrument_name": "BTC-1JAN20-1-C"}), "BTC").is_none());
    }

    // a real ticker notification (BTC-30OCT26-64000-P, 2026-10-26)
    const TICKER: &str = r#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-30OCT26-64000-P.100ms","data":{"timestamp":1792995677897,"state":"open","stats":{"high":0.0001,"low":0.0001,"price_change":0.0,"volume":0.9,"volume_usd":7.24},"greeks":{"delta":-0.00499,"gamma":0.0,"vega":1.21971,"theta":-11.48317,"rho":-0.0458},"index_price":80036.59,"instrument_name":"BTC-30OCT26-64000-P","last_price":0.0001,"settlement_price":1.4392e-4,"min_price":0.0001,"max_price":0.015,"open_interest":1187.7,"mark_price":0.0001,"interest_rate":0.0,"estimated_delivery_price":80036.59,"best_ask_price":0.0002,"best_bid_price":0.0001,"mark_iv":83.93,"bid_iv":80.71,"ask_iv":87.22,"underlying_price":80102.37,"underlying_index":"BTC-30OCT26","best_ask_amount":31.5,"best_bid_amount":295.1}}}"#;

    #[test]
    fn ticker_is_usd_fractions_and_base_sizes() {
        let mut last = HashMap::new();
        let m = collect(|e| on_option(e, &mut last, Frame::Text(TICKER)));
        assert_eq!(m.iter().map(|m| m.ev.kind()).collect::<Vec<_>>(), ["mark", "forward", "oi", "greeks", "bbo"]);
        assert!(m.iter().all(|m| &*m.symbol == "BTC-30OCT26-64000-P" && m.ts == 1792995677897));
        let close = |a: f64, b: f64| assert!((a - b).abs() < 1e-9 * b.abs().max(1.0), "{a} vs {b}");
        let (Event::Mark { mark, index, funding, .. }, Event::Forward { px }, Event::OpenInterest { oi, .. }) = (&m[0].ev, &m[1].ev, &m[2].ev) else { panic!() };
        close(*mark, 0.0001 * 80036.59);
        assert_eq!((*index, *funding, *px, *oi), (Some(80036.59), None, 80102.37, 1187.7));
        let (Event::Greeks { mark_iv, bid_iv, ask_iv, delta, vega, theta, .. }, Event::Bbo { bid, bid_qty, ask, ask_qty }) = (&m[3].ev, &m[4].ev) else { panic!() };
        close(*mark_iv, 0.8393);
        assert_eq!((bid_iv.map(|x| (x * 1e4).round()), ask_iv.map(|x| (x * 1e4).round())), (Some(8071.0), Some(8722.0)));
        assert_eq!((*delta, *vega, *theta), (-0.00499, 1.21971, -11.48317));
        close(*bid, 0.0001 * 80036.59);
        close(*ask, 0.0002 * 80036.59);
        assert_eq!((*bid_qty, *ask_qty), (295.1, 31.5));
        // forward and open interest are not repeated while unchanged
        let again = collect(|e| on_option(e, &mut last, Frame::Text(TICKER)));
        assert_eq!(again.iter().map(|m| m.ev.kind()).collect::<Vec<_>>(), ["mark", "greeks", "bbo"]);
    }

    #[test]
    fn ticker_without_quotes_clears_them() {
        let t = TICKER.replace(r#""best_ask_price":0.0002"#, r#""best_ask_price":0.0"#).replace(r#""best_bid_price":0.0001"#, r#""best_bid_price":0.0"#)
            .replace(r#""bid_iv":80.71"#, r#""bid_iv":0.0"#).replace(r#""ask_iv":87.22"#, r#""ask_iv":0.0"#);
        let m = collect(|e| on_option(e, &mut HashMap::new(), Frame::Text(&t)));
        assert!(matches!(m[3].ev, Event::Greeks { bid_iv: None, ask_iv: None, .. }));
        assert!(matches!(m[4].ev, Event::Bbo { bid, ask, .. } if bid == 0.0 && ask == 0.0));
    }

    #[test]
    fn option_trade_is_usd_with_taker_side() {
        let t = r#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.option.BTC.100ms","data":[{"trade_seq":1,"trade_id":"1","timestamp":1792995700000,"tick_direction":0,"price":0.0125,"mark_price":0.0124,"iv":41.5,"instrument_name":"BTC-27NOV26-73000-P","index_price":80000.0,"direction":"sell","amount":2.5}]}}"#;
        let m = collect(|e| on_option(e, &mut HashMap::new(), Frame::Text(t)));
        assert_eq!(m.len(), 1);
        assert_eq!(&*m[0].symbol, "BTC-27NOV26-73000-P");
        assert!(matches!(m[0].ev, Event::Trade { px, qty, side: Side::Sell } if (px - 1000.0).abs() < 1e-9 && qty == 2.5));
    }

    #[test]
    fn replies_and_other_frames_are_ignored() {
        let m = collect(|e| {
            on_option(e, &mut HashMap::new(), Frame::Text(r#"{"jsonrpc":"2.0","id":1,"result":["ticker.BTC-30OCT26-64000-P.100ms"]}"#));
            on_option(e, &mut HashMap::new(), Frame::Text("not json"));
        });
        assert!(m.is_empty());
    }

    #[test]
    fn streams_btc_and_eth_options_only() {
        assert!(streams(Market::Option, "btc") && streams(Market::Option, "ETH"));
        assert!(!streams(Market::Perp, "BTC") && !streams(Market::Option, "SOL"));
    }

    #[test]
    fn refresh_subscribes_the_whole_chain_again() {
        let ch = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cur = ch(&["a", "b"]);
        // nothing changed: everything is subscribed again (a chunk refused earlier gets another try), nothing unsubscribed
        let m = swap(&mut cur, ch(&["a", "b"]));
        assert!(m.len() == 1 && m[0].contains("public/subscribe") && m[0].contains(r#""a","b""#));
        // what left the chain is unsubscribed first, then the whole new list
        let m = swap(&mut cur, ch(&["b", "c"]));
        assert!(m.len() == 2 && m[0].contains("public/unsubscribe") && m[0].contains(r#"["a"]"#) && m[1].contains("public/subscribe") && m[1].contains(r#""b","c""#));
        assert_eq!(cur, ch(&["b", "c"]));
        // a full chain goes out in CHUNK-sized requests
        let all: Vec<String> = (0..CHUNK + 1).map(|i| format!("ticker.{i:04}.agg2")).collect();
        assert_eq!(swap(&mut cur, all).len(), 1 + 2);
    }
}
