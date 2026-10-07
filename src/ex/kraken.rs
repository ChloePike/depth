//! Kraken: spot/margin on WS v2 (wss://ws.kraken.com/v2), perps on Kraken Futures
//! (wss://futures.kraken.com/ws/v1, linear multi-collateral PF_<BASE>USD).
//!
//! Quote mapping: USDT/USDC requests go to `BASE/USD`, the venue's liquid book (BTC/USDT is listed
//! but often has no trades for minutes). Other quotes use `BASE/QUOTE` when listed, else `BASE/USD`.
//! WS v2 uses BTC; REST and futures use XBT. Perps are always USD-quoted.
//!
//! Perp funding: the ticker's `funding_rate` is an absolute USD amount per contract per hour;
//! we emit `relative_funding_rate` (= funding_rate / price), the hourly relative rate.
//! OI is in contracts, and one PF contract is one unit of base.
//!
//! Options: Kraken has no public options market data.
use crate::ws::{self, Frame, Spec};
use crate::*;
use crate::ws::run_resync;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::task::JoinHandle;

/// spot book depth we subscribe to and keep; Kraken does not send deletes for levels pushed out
const DEPTH: usize = 100;

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Kraken, market: sub.market, tx };
    let (base, mut quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    if quote == "USDT" || quote == "USDC" { quote = "USD".into(); }
    let xbt = if base == "BTC" { "XBT".to_string() } else { base.clone() };
    match sub.market {
        // ponytail: margin borrow (rollover) fees are not in any public endpoint (AssetPairs has no
        // margin fee fields any more), so margin is spot data only; needs private API to add BorrowRate
        Market::Spot | Market::Margin => vec![tokio::spawn(async move {
            // ponytail: a failed lookup (network) also falls back to USD
            let listed = ws::get_json(&format!("https://api.kraken.com/0/public/AssetPairs?pair={xbt}{quote}")).await
                .is_ok_and(|v| v["result"].as_object().is_some_and(|r| !r.is_empty()));
            let sym = format!("{base}/{}", if listed { &quote } else { "USD" });
            let spec = Spec::new("kraken spot", "wss://ws.kraken.com/v2")
                .sub(json!({"method": "subscribe", "params": {"channel": "trade", "symbol": [&sym]}}).to_string())
                .sub(json!({"method": "subscribe", "params": {"channel": "book", "symbol": [&sym], "depth": DEPTH}}).to_string())
                .sub(json!({"method": "subscribe", "params": {"channel": "ticker", "symbol": [&sym], "event_trigger": "bbo"}}).to_string())
                .ping(Duration::from_secs(30), r#"{"method":"ping"}"#);
            let mut book = Book::default();
            ws::run(spec, move |f| on_spot(&e, &sym, &mut book, f)).await
        })],
        Market::Perp => {
            let sym = format!("PF_{xbt}USD");
            let spec = move || {
                // {"event":"ping"} is rejected; re-subscribing to heartbeat is the client keepalive (<60s)
                let hb = json!({"event": "subscribe", "feed": "heartbeat"}).to_string();
                ["trade", "book", "ticker"].iter().fold(Spec::new("kraken perp", "wss://futures.kraken.com/ws/v1").sub(hb.clone()), |s, feed| {
                    s.sub(json!({"event": "subscribe", "feed": feed, "product_ids": [&sym]}).to_string())
                }).ping(Duration::from_secs(30), hb)
            };
            vec![tokio::spawn(run_resync(spec, move || {
                let (e, mut seq) = (e.clone(), None);
                move |f| on_perp(&e, &mut seq, f)
            }))]
        }
        Market::Option => vec![], // no public options market data
    }
}

fn obj_levels(v: &Value) -> Vec<(f64, f64)> {
    v.as_array().map(|a| a.iter().map(|l| (num(&l["price"]), num(&l["qty"]))).collect()).unwrap_or_default()
}

fn rfc_ms(v: &Value) -> i64 { v.as_str().map(iso_ms).unwrap_or(0) }

// ponytail: book checksum (CRC32 of top 10) is not verified; add it if books drift in practice
fn on_spot(e: &Emit, sym: &str, book: &mut Book, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    for d in v["data"].as_array().into_iter().flatten() {
        match v["channel"].as_str().unwrap_or("") {
            "trade" => {
                let side = if d["side"] == "sell" { Side::Sell } else { Side::Buy };
                e.send(sym, rfc_ms(&d["timestamp"]), Event::Trade { px: num(&d["price"]), qty: num(&d["qty"]), side });
            }
            "book" => {
                let snapshot = v["type"] == "snapshot";
                let (mut bids, mut asks) = (obj_levels(&d["bids"]), obj_levels(&d["asks"]));
                book.apply(snapshot, &bids, &asks);
                // levels pushed beyond DEPTH get no delete from Kraken; drop them here and downstream
                let out_b: Vec<_> = book.bids().skip(DEPTH).map(|(p, _)| (p, 0.0)).collect();
                let out_a: Vec<_> = book.asks().skip(DEPTH).map(|(p, _)| (p, 0.0)).collect();
                book.apply(false, &out_b, &out_a);
                if snapshot { bids = book.bids().collect(); asks = book.asks().collect(); } else { bids.extend(out_b); asks.extend(out_a); }
                e.send(sym, rfc_ms(&d["timestamp"]), Event::Book { snapshot, bids, asks });
            }
            "ticker" => e.send(sym, rfc_ms(&d["timestamp"]), Event::Bbo {
                bid: num(&d["bid"]), bid_qty: num(&d["bid_qty"]), ask: num(&d["ask"]), ask_qty: num(&d["ask_qty"]) }),
            _ => {}
        }
    }
}

/// returns false on a book sequence gap so the session restarts with a fresh snapshot
fn on_perp(e: &Emit, seq: &mut Option<i64>, f: Frame) -> bool {
    let Frame::Text(t) = f else { return true };
    let Ok(d) = serde_json::from_str::<Value>(t) else { return true };
    // subscription acks carry "event" plus the feed name
    if d.get("event").is_some() { return true; }
    let sym = d["product_id"].as_str().unwrap_or("");
    match d["feed"].as_str().unwrap_or("") {
        // trade_snapshot replays old trades; skipped so reconnects do not double count
        "trade" => {
            // side = taker side; for liquidations the taker is the liquidation order
            let side = if d["side"] == "sell" { Side::Sell } else { Side::Buy };
            let (px, qty, ts) = (num(&d["price"]), num(&d["qty"]), d["time"].as_i64().unwrap_or(0));
            e.send(sym, ts, Event::Trade { px, qty, side });
            if d["type"] == "liquidation" { e.send(sym, ts, Event::Liquidation { px, qty, side }); }
        }
        "book_snapshot" => {
            *seq = d["seq"].as_i64();
            e.send(sym, d["timestamp"].as_i64().unwrap_or(0), Event::Book { snapshot: true, bids: obj_levels(&d["bids"]), asks: obj_levels(&d["asks"]) });
        }
        "book" => {
            let n = d["seq"].as_i64();
            if seq.is_some_and(|p| n != Some(p + 1)) { return false; }
            *seq = n;
            let l = vec![(num(&d["price"]), num(&d["qty"]))];
            let (bids, asks) = if d["side"] == "buy" { (l, vec![]) } else { (vec![], l) };
            e.send(sym, d["timestamp"].as_i64().unwrap_or(0), Event::Book { snapshot: false, bids, asks });
        }
        "ticker" => {
            let ts = d["time"].as_i64().unwrap_or(0);
            e.send(sym, ts, Event::Bbo { bid: num(&d["bid"]), bid_qty: num(&d["bid_size"]), ask: num(&d["ask"]), ask_qty: num(&d["ask_size"]) });
            e.send(sym, ts, Event::Mark { mark: num(&d["markPrice"]), index: opt_num(&d["index"]),
                funding: opt_num(&d["relative_funding_rate"]), next_funding_ms: d["next_funding_rate_time"].as_i64() });
            e.send(sym, ts, Event::OpenInterest { oi: num(&d["openInterest"]), oi_usd: None });
        }
        _ => {}
    }
    true
}

const CHARTS: &str = "https://futures.kraken.com/api/charts/v1";

/// Recent REST history for the chart (one-minute candles); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `bars` candles of `tf` minutes; see `ex::history_tf`.
/// Spot: OHLC (interval = tf) keeps only the last 720 candles of each interval; for 1m older minutes
/// are rebuilt from public Trades, larger periods return at most 720 candles.
/// Perp: charts API (2000 points per request, resolution 1m..4h) for candles, and analytics
/// (interval = tf in seconds) for OI, long/short accounts and the taker split
/// (trade-volume +- aggressor-differential); funding from historicalfundingrates.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let res = match tf { 1 => "1m", 5 => "5m", 15 => "15m", 60 => "1h", 240 => "4h", _ => anyhow::bail!("kraken has no {tf}m candles") };
    let (base, mut quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    if quote == "USDT" || quote == "USDC" { quote = "USD".into(); }
    let xbt = if base == "BTC" { "XBT".to_string() } else { base.clone() };
    let (start, end) = window_tf(tf, bars);
    match sub.market {
        Market::Spot | Market::Margin => {
            let listed = ws::get_json(&format!("https://api.kraken.com/0/public/AssetPairs?pair={xbt}{quote}")).await
                .is_ok_and(|v| v["result"].as_object().is_some_and(|r| !r.is_empty()));
            let pair = format!("{xbt}{}", if listed { &quote } else { "USD" });
            spot_history(&pair, tf, start, end, bars).await
        }
        Market::Perp => perp_history(&format!("PF_{xbt}USD"), tf, res, start, end, bars).await,
        Market::Option => Ok(History::default()),
    }
}

/// first entry of a Kraken spot `result` object that is not "last"
fn spot_rows(v: &Value) -> Vec<Value> {
    v["result"].as_object().into_iter().flatten().find(|(k, _)| *k != "last")
        .and_then(|(_, a)| a.as_array().cloned()).unwrap_or_default()
}

async fn spot_history(pair: &str, tf: u32, start: i64, end: i64, bars: usize) -> anyhow::Result<History> {
    use futures_util::{StreamExt, TryStreamExt};
    let v = ws::get_json(&format!("https://api.kraken.com/0/public/OHLC?pair={pair}&interval={tf}&since={}", start / 1000 - tf as i64 * 60)).await?;
    // [time, open, high, low, close, vwap, volume, count]
    let mut k: Vec<Kline> = spot_rows(&v).iter().map(|r| Kline { t: r[0].as_i64().unwrap_or(0) * 1000,
        o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[6]), buy: None }).collect();
    // ponytail: above 1m the 720-candle OHLC cap is the limit (15m ~7.5 days, 4h ~120 days); a
    // trades backfill would cost thousands of requests there
    if tf != 1 { return Ok(History { klines: continuous_tf(k, end, bars, tf), ..Default::default() }) }
    let first = k.iter().map(|x| x.t).min().unwrap_or(end + 60_000);
    // ponytail: ~1 Trades request per 10-20 min of BTC/USD history, 4 in flight; a busy market or a
    // long window costs many requests (Kraken public limit ~1/s sustained). Drop if startup gets slow.
    const CHUNK: i64 = 15 * 60_000;
    let chunks: Vec<i64> = (start..first).step_by(CHUNK as usize).collect();
    // best effort: on a rate limit keep the OHLC part rather than failing the whole history
    match futures_util::stream::iter(chunks).map(|s| trade_bars(pair, s, (s + CHUNK).min(first)))
        .buffered(4).try_collect::<Vec<Vec<Kline>>>().await {
        Ok(rebuilt) => k.extend(rebuilt.into_iter().flatten()),
        Err(e) => eprintln!("[kraken spot history] trades backfill: {e:#}"),
    }
    Ok(History { klines: continuous(k, end, bars), ..Default::default() })
}

/// one-minute bars for [from, to) built from public trades, paging with `last` until `to` is reached
async fn trade_bars(pair: &str, from: i64, to: i64) -> anyhow::Result<Vec<Kline>> {
    let mut bars: std::collections::BTreeMap<i64, Kline> = Default::default();
    let mut since = format!("{}", from as i128 * 1_000_000);
    let mut retries = 0;
    loop {
        let v = ws::get_json(&format!("https://api.kraken.com/0/public/Trades?pair={pair}&count=1000&since={since}")).await?;
        if let Some(e) = v["error"].as_array().filter(|e| !e.is_empty()) {
            // the public counter refills at ~1 call/s; wait it out a few times before giving up
            if retries < 5 && format!("{e:?}").contains("Too many requests") {
                retries += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            anyhow::bail!("kraken Trades: {e:?}")
        }
        let rows = spot_rows(&v);
        // [price, volume, time (s, fractional), side, type, misc, id]
        let mut reached = rows.is_empty();
        for r in &rows {
            let t = (num(&r[2]) * 1000.0) as i64;
            if t >= to { reached = true; break }
            let (px, q, m) = (num(&r[0]), num(&r[1]), t / 60_000 * 60_000);
            let b = bars.entry(m).or_insert(Kline { t: m, o: px, h: px, l: px, c: px, vol: 0.0, buy: None });
            (b.h, b.l, b.c, b.vol) = (b.h.max(px), b.l.min(px), px, b.vol + q);
        }
        match v["result"]["last"].as_str() { Some(l) if !reached && rows.len() >= 1000 => since = l.to_string(), _ => break }
    }
    Ok(bars.into_values().collect())
}

/// charts API series over [start, end] in pages of 2000 `tf`-minute points; `path` is e.g. "trade/PF_XBTUSD/1m"
async fn charts(path: String, tf: u32, start: i64, end: i64, key: &str) -> anyhow::Result<Vec<Value>> {
    let step = tf as i64 * 60_000;
    let pages = (start..=end).step_by((2000 * step) as usize).map(|s| {
        let to = (s + 1999 * step).min(end);
        let (from, to) = (s / 1000, to / 1000);
        let url = if key == "candles" { format!("{CHARTS}/{path}?from={from}&to={to}") }
            else { format!("{CHARTS}/analytics/{path}?since={from}&to={to}&interval={}", tf * 60) };
        async move { ws::get_json(&url).await }
    });
    let res = futures_util::future::try_join_all(pages).await?;
    Ok(res.into_iter().map(|mut v| if key == "candles" { v["candles"].take() } else { v["result"].take() }).collect())
}

/// analytics `result` -> (ms, value) pairs; `f` picks the value from `data` at index i
fn series(pages: &[Value], f: impl Fn(&Value, usize) -> f64) -> Vec<(i64, f64)> {
    pages.iter().flat_map(|r| r["timestamp"].as_array().into_iter().flatten().enumerate()
        .map(|(i, t)| (t.as_i64().unwrap_or(0) * 1000, f(&r["data"], i))).collect::<Vec<_>>())
        .filter(|(_, x)| x.is_finite()).collect()
}

async fn perp_history(sym: &str, tf: u32, res: &str, start: i64, end: i64, bars: usize) -> anyhow::Result<History> {
    let fund_url = format!("https://futures.kraken.com/derivatives/api/v4/historicalfundingrates?symbol={sym}");
    let (candles, oi, ls, diff, vol, fund) = tokio::join!(
        charts(format!("trade/{sym}/{res}"), tf, start, end, "candles"),
        charts(format!("{sym}/open-interest"), tf, start, end, "result"),
        charts(format!("{sym}/long-short-info"), tf, start, end, "result"),
        charts(format!("{sym}/aggressor-differential"), tf, start, end, "result"),
        charts(format!("{sym}/trade-volume"), tf, start, end, "result"),
        // ponytail: returns the whole year (~9k rows, one request); no server-side time filter
        ws::get_json(&fund_url),
    );
    // candle volume and every analytics size are in contracts; one PF contract is one unit of base
    let klines = candles?.iter().flat_map(|c| c.as_array().cloned().unwrap_or_default()).map(|c| Kline {
        t: c["time"].as_i64().unwrap_or(0), o: num(&c["open"]), h: num(&c["high"]), l: num(&c["low"]), c: num(&c["close"]), vol: num(&c["volume"]), buy: None,
    }).collect();
    // OI data rows are [open, high, low, close]; take the close
    let oi = series(&oi?, |d, i| num(&d[i][3]));
    let ls = series(&ls?, |d, i| num(&d["longCount"][i]) / num(&d["shortCount"][i]));
    let diff: std::collections::HashMap<i64, f64> = series(&diff?, |d, i| num(&d[i])).into_iter().collect();
    // aggressor-differential = buy - sell, trade-volume = buy + sell
    let taker = series(&vol?, |d, i| num(&d[i])).into_iter()
        .filter_map(|(t, v)| diff.get(&t).map(|d| (t, ((v + d) / 2.0).max(0.0), ((v - d) / 2.0).max(0.0)))).collect();
    // relativeFundingRate is already the hourly relative rate, stamped at settlement
    let funding = fund?["rates"].as_array().into_iter().flatten()
        .map(|r| (rfc_ms(&r["timestamp"]), num(&r["relativeFundingRate"])))
        .filter(|(t, r)| *t >= start - 3_600_000 && r.is_finite()).collect();
    Ok(History { klines: continuous_tf(klines, end, bars, tf), oi, funding, long_short: ls, taker })
}

