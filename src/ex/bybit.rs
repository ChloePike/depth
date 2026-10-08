//! Bybit v5: spot/margin on /v5/public/spot, USDT perps on /v5/public/linear, options on /v5/public/option.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const WS: &str = "wss://stream.bybit.com/v5/public";
const API: &str = "https://api.bybit.com/v5";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Bybit, market: sub.market, tx };
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    let topics = |names: &[&str]| names.iter().map(|n| format!("{n}.{sym}")).collect::<Vec<_>>();
    match sub.market {
        Market::Spot | Market::Margin => {
            let args = topics(&["publicTrade", "orderbook.1000", "orderbook.1"]);
            let (e1, sym1) = (e.clone(), sym.clone());
            let mut book = 0i64;
            let mut v = vec![tokio::spawn(session(format!("bybit {:?}", sub.market), format!("{WS}/spot"), args,
                move |m| on_market(&e1, &sym1, m, &mut book, &mut Map::new())))];
            if sub.market == Market::Margin {
                let assets = [sub.base.to_uppercase(), sub.quote.to_uppercase()];
                v.push(tokio::spawn(ws::poll("bybit borrow".into(), Duration::from_secs(300), move || poll_borrow(e.clone(), sym.clone(), assets.clone()))));
            }
            v
        }
        Market::Perp => {
            let args = topics(&["publicTrade", "orderbook.1000", "orderbook.1", "tickers", "allLiquidation"]);
            let (e1, sym1) = (e.clone(), sym.clone());
            let (mut book, mut ticker) = (0i64, Map::new());
            vec![
                tokio::spawn(session("bybit perp".into(), format!("{WS}/linear"), args, move |m| on_market(&e1, &sym1, m, &mut book, &mut ticker))),
                tokio::spawn(ws::poll("bybit ls".into(), Duration::from_secs(60), move || poll_ls(e.clone(), sym.clone()))),
            ]
        }
        Market::Option => {
            let base = sub.base.to_uppercase();
            let e1 = e.clone();
            let trades = vec![format!("publicTrade.{base}")];
            vec![
                tokio::spawn(session("bybit option trades".into(), format!("{WS}/option"), trades, move |m| on_option(&e1, m, &mut HashMap::new()))),
                tokio::spawn(option_chain(e, base)),
            ]
        }
    }
}

/// One Bybit connection: subscribes `args` in batches, app ping every 20s, and reconnects
/// (forcing a fresh book snapshot) whenever the handler returns false.
async fn session(name: String, url: String, args: Vec<String>, mut h: impl FnMut(&Value) -> bool + Send) {
    let gap = Arc::new(Notify::new());
    loop {
        let mut spec = Spec::new(name.clone(), url.clone()).ping(Duration::from_secs(20), r#"{"op":"ping"}"#);
        // option requests are limited to ~21000 chars of args; 100 topics stay well below
        for c in args.chunks(100) { spec = spec.sub(serde_json::json!({"op": "subscribe", "args": c}).to_string()); }
        let g = gap.clone();
        let h = &mut h;
        tokio::select! {
            _ = ws::run(spec, move |f| {
                let Frame::Text(t) = f else { return };
                let Ok(v) = serde_json::from_str::<Value>(t) else { return };
                if v["success"] == false { eprintln!("[bybit] {t}"); }
                if v["topic"].is_string() && !h(&v) { g.notify_one(); }
            }) => {}
            _ = gap.notified() => eprintln!("[{name}] book sequence gap, resubscribing"),
        }
    }
}

/// spot and linear: trades, books, top of book, ticker state (perp), liquidations
fn on_market(e: &Emit, sym: &str, m: &Value, book_u: &mut i64, ticker: &mut Map<String, Value>) -> bool {
    let topic = m["topic"].as_str().unwrap_or("");
    let ts = m["ts"].as_i64().unwrap_or(0);
    let d = &m["data"];
    if topic.starts_with("publicTrade.") {
        for t in d.as_array().into_iter().flatten() {
            e.send(sym, t["T"].as_i64().unwrap_or(ts), Event::Trade { px: num(&t["p"]), qty: num(&t["v"]), side: side(&t["S"]) });
        }
    } else if topic.starts_with("orderbook.1.") {
        let (b, a) = (&d["b"][0], &d["a"][0]);
        e.send(sym, ts, Event::Bbo { bid: num(&b[0]), bid_qty: num(&b[1]), ask: num(&a[0]), ask_qty: num(&a[1]) });
    } else if topic.starts_with("orderbook.") {
        let u = d["u"].as_i64().unwrap_or(0);
        let snapshot = m["type"] == "snapshot";
        if !snapshot && (*book_u == 0 || u != *book_u + 1) { *book_u = 0; return false; }
        *book_u = u;
        e.send(sym, ts, Event::Book { snapshot, bids: levels(&d["b"]), asks: levels(&d["a"]) });
    } else if topic.starts_with("tickers.") {
        // linear tickers: snapshot, then deltas carrying only changed fields
        if m["type"] == "snapshot" { ticker.clear(); }
        let Some(delta) = d.as_object() else { return true };
        ticker.extend(delta.clone());
        let t = &*ticker;
        if ["markPrice", "indexPrice", "fundingRate", "nextFundingTime", "fundingIntervalHour"].iter().any(|k| delta.contains_key(*k)) {
            // fundingRate is per interval; the ticker itself carries the live per-symbol interval
            // (fundingIntervalHour, same value as instruments-info fundingInterval / 60), so no REST call is needed
            let hours = opt_num(&t["fundingIntervalHour"]).filter(|h| *h > 0.0);
            e.send(sym, ts, Event::Mark { mark: num(&t["markPrice"]), index: opt_num(&t["indexPrice"]),
                funding: opt_num(&t["fundingRate"]).zip(hours).map(|(r, h)| r / h), next_funding_ms: opt_num(&t["nextFundingTime"]).map(|x| x as i64) });
        }
        // `openInterest` counts both sides; `singleOpenInterest` is one side, the convention used by
        // Binance/OKX/CME and by every other connector, so cross-venue sums stay comparable.
        if delta.contains_key("singleOpenInterest") || delta.contains_key("singleOpenInterestValue") {
            e.send(sym, ts, Event::OpenInterest { oi: num(&t["singleOpenInterest"]), oi_usd: opt_num(&t["singleOpenInterestValue"]) });
        }
    } else if topic.starts_with("allLiquidation.") {
        // S is the liquidated position's side: Buy = a long was liquidated, so the order sells
        for l in d.as_array().into_iter().flatten() {
            let s = if l["S"] == "Buy" { Side::Sell } else { Side::Buy };
            e.send(sym, l["T"].as_i64().unwrap_or(ts), Event::Liquidation { px: num(&l["p"]), qty: num(&l["v"]), side: s });
        }
    }
    true
}

fn side(s: &Value) -> Side { if s == "Sell" { Side::Sell } else { Side::Buy } }

async fn poll_ls(e: Emit, sym: String) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{API}/market/account-ratio?category=linear&symbol={sym}&period=5min&limit=1")).await?;
    let r = &v["result"]["list"][0];
    let (buy, sell) = (num(&r["buyRatio"]), num(&r["sellRatio"]));
    if buy.is_finite() && sell > 0.0 {
        e.send(&sym, num(&r["timestamp"]) as i64, Event::LongShort { kind: LsKind::Accounts, ratio: buy / sell, long_pct: Some(buy) });
    }
    Ok(())
}

/// public, no key: hourly borrow rates per coin for the "No VIP" tier
async fn poll_borrow(e: Emit, sym: String, assets: [String; 2]) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{API}/spot-margin-trade/data")).await?;
    let tiers = v["result"]["vipCoinList"].as_array().cloned().unwrap_or_default();
    let tier = tiers.iter().find(|t| t["vipLevel"] == "No VIP").or(tiers.first());
    for c in tier.and_then(|t| t["list"].as_array()).into_iter().flatten() {
        let asset = c["currency"].as_str().unwrap_or("");
        if let (true, Some(h)) = (assets.iter().any(|a| a == asset), opt_num(&c["hourlyBorrowRate"])) {
            e.send(&sym, 0, Event::BorrowRate { asset: asset.into(), apr: h * 24.0 * 365.0 });
        }
    }
    Ok(())
}

/// aborts the per-chunk ticker connections when the chain task is dropped or the chain changes
struct Tasks(Vec<JoinHandle<()>>);
impl Drop for Tasks { fn drop(&mut self) { self.0.iter().for_each(|t| t.abort()); } }

/// Polls the instrument list, emits OptionInfo, and (re)subscribes tickers for every trading option.
async fn option_chain(e: Emit, base: String) {
    let mut live: Vec<String> = vec![];
    let mut _tasks = Tasks(vec![]);
    loop {
        match option_instruments(&base).await {
            Ok(list) => {
                let mut syms = vec![];
                for o in list.iter().filter(|o| o["status"] == "Trading") {
                    let s = o["symbol"].as_str().unwrap_or("");
                    // symbol: BTC-25DEC26-105000-P-USDT
                    e.send(s, 0, Event::OptionInfo { underlying: base.clone(), expiry_ms: num(&o["deliveryTime"]) as i64,
                        strike: s.split('-').nth(2).and_then(|x| x.parse().ok()).unwrap_or(f64::NAN), call: o["optionsType"] == "Call" });
                    syms.push(s.to_string());
                }
                syms.sort();
                if syms != live && !syms.is_empty() {
                    // ponytail: any listing change reconnects all ticker sockets; diff subscribe/unsubscribe if churn hurts
                    _tasks = Tasks(syms.chunks(400).enumerate().map(|(i, c)| {
                        let e = e.clone();
                        let args = c.iter().map(|s| format!("tickers.{s}")).collect();
                        let mut fwd = HashMap::new();
                        tokio::spawn(session(format!("bybit option tickers {i}"), format!("{WS}/option"), args, move |m| on_option(&e, m, &mut fwd)))
                    }).collect());
                    live = syms;
                }
            }
            Err(err) => eprintln!("[bybit optinfo] {err:#}"),
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

async fn option_instruments(base: &str) -> anyhow::Result<Vec<Value>> {
    let (mut out, mut cursor) = (vec![], String::new());
    loop {
        let v = ws::get_json(&format!("{API}/market/instruments-info?category=option&baseCoin={base}&limit=1000&cursor={cursor}")).await?;
        let r = &v["result"];
        let page = r["list"].as_array().cloned().unwrap_or_default();
        if page.is_empty() && out.is_empty() { anyhow::bail!("no option instruments: {}", v["retMsg"]) }
        let done = page.is_empty();
        out.extend(page);
        cursor = r["nextPageCursor"].as_str().unwrap_or("").to_string();
        if done || cursor.is_empty() { return Ok(out) }
    }
}

/// `fwd`: last Forward sent per symbol, so it is only emitted on change
fn on_option(e: &Emit, m: &Value, fwd: &mut HashMap<String, f64>) -> bool {
    let topic = m["topic"].as_str().unwrap_or("");
    let ts = m["ts"].as_i64().unwrap_or(0);
    let d = &m["data"];
    if topic.starts_with("tickers.") {
        // option tickers are full snapshots every push
        let s = d["symbol"].as_str().unwrap_or("");
        e.send(s, ts, Event::Mark { mark: num(&d["markPrice"]), index: opt_num(&d["indexPrice"]), funding: None, next_funding_ms: None });
        // underlyingPrice = per-expiry forward (USD)
        if let Some(px) = opt_num(&d["underlyingPrice"]).filter(|p| *p > 0.0) {
            if fwd.insert(s.to_string(), px) != Some(px) { e.send(s, ts, Event::Forward { px }); }
        }
        // IV is "0" when that side has no quote
        let iv = |v: &Value| opt_num(v).filter(|x| *x > 1e-4);
        e.send(s, ts, Event::Greeks { mark_iv: num(&d["markPriceIv"]), bid_iv: iv(&d["bidIv"]), ask_iv: iv(&d["askIv"]),
            delta: num(&d["delta"]), gamma: num(&d["gamma"]), vega: num(&d["vega"]), theta: num(&d["theta"]) });
        let (bid, ask) = (num(&d["bidPrice"]), num(&d["askPrice"]));
        if bid > 0.0 || ask > 0.0 {
            e.send(s, ts, Event::Bbo { bid, bid_qty: num(&d["bidSize"]), ask, ask_qty: num(&d["askSize"]) });
        }
    } else if topic.starts_with("publicTrade.") {
        for t in d.as_array().into_iter().flatten() {
            e.send(t["s"].as_str().unwrap_or(""), t["T"].as_i64().unwrap_or(ts), Event::Trade { px: num(&t["p"]), qty: num(&t["v"]), side: side(&t["S"]) });
        }
    }
    true
}

/// Recent 1m REST history for the chart; see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// Last `bars` `tf`-minute candles (forming one included) plus, for perps, OI / long-short over
/// the same window and settled funding. Bybit candles have no taker split and there is no public
/// taker-volume history, so `taker` stays empty. Perp extras are best effort: a failing endpoint
/// is logged and left empty.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    if ![1, 5, 15, 60, 240].contains(&tf) { anyhow::bail!("unsupported timeframe {tf}m") }
    let step = tf as i64 * 60_000;
    let (start, end) = window_tf(tf, bars);
    let now = now_ms();
    let cat = match sub.market {
        Market::Option => return Ok(History::default()),
        Market::Spot | Market::Margin => "spot",
        Market::Perp => "linear",
    };
    // interval is the tf in minutes; linear volume is already in base coin
    let klines = async {
        let urls = pages(start, now, step, 1000).into_iter()
            .map(|(s, e)| format!("{API}/market/kline?category={cat}&symbol={sym}&interval={tf}&start={s}&end={e}&limit=1000")).collect();
        let k = rows(urls).await?.iter().map(|r| Kline {
            t: num(&r[0]) as i64, o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[5]), buy: None,
        }).filter(|x| x.h > 0.0).collect(); // all-zero rows are missing data
        anyhow::Ok(continuous_tf(k, end, bars, tf))
    };
    if sub.market != Market::Perp { return Ok(History { klines: klines.await?, ..Default::default() }) }

    // OI and account-ratio periods: 5min 15min 30min 1h 4h 1d; history reaches back well past 1000 4h bars
    let (pm, period) = [(5, "5min"), (15, "15min"), (60, "1h"), (240, "4h")].into_iter().filter(|p| p.0 <= tf).last().unwrap_or((5, "5min"));
    let ps = pm as i64 * 60_000;
    let start_p = start / ps * ps;
    let ts = |r: &Value| num(&r["timestamp"]) as i64;
    let oi = async {
        // singleOpenInterest (one side, base coin), the same field the live ticker emits
        let urls = pages(start_p, now, ps, 200).into_iter()
            .map(|(s, e)| format!("{API}/market/open-interest?category=linear&symbol={sym}&intervalTime={period}&startTime={s}&endTime={e}&limit=200")).collect();
        series(rows(urls).await?.iter().map(|r| (ts(r), num(&r["singleOpenInterest"]))))
    };
    let long_short = async {
        let urls = pages(start_p, now, ps, 500).into_iter()
            .map(|(s, e)| format!("{API}/market/account-ratio?category=linear&symbol={sym}&period={period}&startTime={s}&endTime={e}&limit=500")).collect();
        series(rows(urls).await?.iter().map(|r| (ts(r), num(&r["buyRatio"]) / num(&r["sellRatio"]))))
    };
    let funding = async {
        // rates are per interval; the ticker's fundingIntervalHour is the live connector's interval source.
        // Start one max interval (8h) early so the settlement covering the window start is included.
        // ponytail: every rate is divided by the CURRENT interval
        let t = rows(vec![format!("{API}/market/tickers?category=linear&symbol={sym}")]).await?;
        let Some(h) = t.first().and_then(|t| opt_num(&t["fundingIntervalHour"])).filter(|h| *h > 0.0) else { anyhow::bail!("no fundingIntervalHour") };
        // 200 rows per request, newest first within [startTime, endTime]
        let urls = pages(start - 8 * 3_600_000, now, h as i64 * 3_600_000, 200).into_iter()
            .map(|(s, e)| format!("{API}/market/funding/history?category=linear&symbol={sym}&startTime={s}&endTime={e}&limit=200")).collect();
        series(rows(urls).await?.iter().map(|r| (num(&r["fundingRateTimestamp"]) as i64, num(&r["fundingRate"]) / h)))
    };
    let (klines, oi, funding, long_short) = tokio::join!(klines, oi, funding, long_short);
    let opt = |name: &str, r: anyhow::Result<Vec<(i64, f64)>>| r.unwrap_or_else(|e| { eprintln!("[bybit history {name}] {e:#}"); vec![] });
    // zero OI / ratio rows are missing data
    let pos = |v: Vec<(i64, f64)>| v.into_iter().filter(|x| x.1 > 0.0).collect();
    Ok(History { klines: klines?, oi: pos(opt("oi", oi)), funding: opt("funding", funding), long_short: pos(opt("ls", long_short)), taker: vec![] })
}

/// [start, end] split into inclusive windows of at most `n` steps of `step` ms
fn pages(start: i64, end: i64, step: i64, n: i64) -> Vec<(i64, i64)> {
    (start..=end).step_by((step * n) as usize).map(|s| (s, (s + step * (n - 1)).min(end))).collect()
}

/// GETs all urls concurrently and concatenates their `result.list` arrays
async fn rows(urls: Vec<String>) -> anyhow::Result<Vec<Value>> {
    let pages = futures_util::future::try_join_all(urls.iter().map(|u| ws::get_json(u))).await?;
    let mut out = vec![];
    for mut p in pages {
        if p["retCode"] != 0 { anyhow::bail!("bybit: {} {}", p["retCode"], p["retMsg"]) }
        if let Value::Array(a) = p["result"]["list"].take() { out.extend(a) }
    }
    Ok(out)
}

/// ascending, deduplicated by time, NaN rows dropped
fn series(it: impl Iterator<Item = (i64, f64)>) -> anyhow::Result<Vec<(i64, f64)>> {
    let mut v: Vec<_> = it.filter(|(t, x)| *t > 0 && x.is_finite()).collect();
    v.sort_by_key(|x| x.0);
    v.dedup_by_key(|x| x.0);
    Ok(v)
}
