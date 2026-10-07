//! Binance: spot/margin on stream.binance.com, USDT-M perps on fstream, options on eoptions.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Binance, market: sub.market, tx };
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    let s = sym.to_lowercase();
    match sub.market {
        // ponytail: margin borrow rates need an API key, so margin is spot data only; add /sapi/v1/margin/interestRateHistory once keys exist
        Market::Spot | Market::Margin => {
            let url = format!("wss://stream.binance.com:9443/stream?streams={s}@trade/{s}@bookTicker");
            let depth = format!("wss://stream.binance.com:9443/stream?streams={s}@depth@100ms");
            let snap = format!("https://api.binance.com/api/v3/depth?symbol={sym}&limit=1000");
            let (e1, sym1) = (e.clone(), sym.clone());
            vec![
                tokio::spawn(ws::run(Spec::new(format!("binance {:?}", sub.market), url), move |f| on_spot(&e, &sym, f))),
                tokio::spawn(diff_depth(e1, sym1, format!("binance {:?} depth", sub.market), depth, snap, false)),
            ]
        }
        Market::Perp => {
            // Futures streams are split: book data on /public, trades/mark/liquidations on /market.
            // The legacy root path still accepts connections but no longer pushes these streams.
            let public = format!("wss://fstream.binance.com/public/stream?streams={s}@bookTicker");
            let depth = format!("wss://fstream.binance.com/public/stream?streams={s}@depth@100ms");
            let snap = format!("https://fapi.binance.com/fapi/v1/depth?symbol={sym}&limit=1000");
            let market = format!("wss://fstream.binance.com/market/stream?streams={s}@aggTrade/{s}@markPrice@1s/{s}@forceOrder");
            let (e1, e2, e3, sym1, sym2, sym3) = (e.clone(), e.clone(), e.clone(), sym.clone(), sym.clone(), sym.clone());
            // funding interval in hours, 0 until fundingInfo has been fetched; the public stream carries no funding
            let (hours, none) = (Arc::new(AtomicU32::new(0)), AtomicU32::new(0));
            let (e4, sym4) = (e.clone(), sym.clone());
            vec![
                tokio::spawn(diff_depth(e4, sym4, "binance perp depth".into(), depth, snap, true)),
                tokio::spawn(ws::run(Spec::new("binance perp public", public), move |f| on_perp(&e, &sym, &none, f))),
                tokio::spawn(funding_interval(sym1.clone(), hours.clone())),
                tokio::spawn(ws::run(Spec::new("binance perp market", market), move |f| on_perp(&e1, &sym1, &hours, f))),
                tokio::spawn(ws::poll("binance oi".into(), Duration::from_secs(5), move || poll_oi(e2.clone(), sym2.clone()))),
                tokio::spawn(ws::poll("binance ls".into(), Duration::from_secs(60), move || poll_ls(e3.clone(), sym3.clone()))),
            ]
        }
        Market::Option => {
            // Options moved from nbstream/eoptions (now 404) to fstream /market, keyed by underlying pair.
            let u = sub.base.to_uppercase();
            // Mark/greeks are on /market, option trades on /public.
            let market = format!("wss://fstream.binance.com/market/stream?streams={s}@optionMarkPrice");
            let public = format!("wss://fstream.binance.com/public/stream?streams={s}@optionTrade");
            let (e1, e2, u2) = (e.clone(), e.clone(), u.clone());
            vec![
                tokio::spawn(ws::run(Spec::new("binance option market", market), move |f| on_option(&e, f))),
                tokio::spawn(ws::run(Spec::new("binance option public", public), move |f| on_option(&e1, f))),
                tokio::spawn(ws::poll("binance optinfo".into(), Duration::from_secs(600), move || poll_option_info(e2.clone(), u2.clone()))),
            ]
        }
    }
}

/// Full-depth book: diff stream + REST snapshot (1000 levels; weight 20 futures / 50 spot, once
/// per connection), validated with Binance's sequencing rules. Futures: each event's `pu` must
/// equal the previous `u`; spot: `U` must be previous `u + 1`. Any gap resyncs from scratch.
async fn diff_depth(e: Emit, sym: String, name: String, url: String, snap_url: String, futures: bool) {
    loop {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let spec = Spec::new(name.clone(), url.clone());
        let session = tokio::spawn(async move {
            let r = ws::once(&spec, move |f| match unwrap(f) { Some((_, d)) => tx.send(d).is_ok(), None => true }).await;
            if let Err(err) = r { eprintln!("[{}] {err:#}", spec.name); }
        });
        // the snapshot must be newer than the first buffered event: wait for one first
        let Some(head) = rx.recv().await else { session.abort(); tokio::time::sleep(Duration::from_secs(2)).await; continue };
        let snap = match ws::get_json(&snap_url).await {
            Ok(v) => v,
            Err(err) => { eprintln!("[{name}] snapshot: {err:#}"); session.abort(); tokio::time::sleep(Duration::from_secs(10)).await; continue }
        };
        let last = snap["lastUpdateId"].as_i64().unwrap_or(0);
        e.send(&sym, 0, Event::Book { snapshot: true, bids: levels(&snap["bids"]), asks: levels(&snap["asks"]) });
        let mut prev: Option<i64> = None;
        let mut head = Some(head);
        let diag = std::env::var("T1_DIAG").is_ok();
        while let Some(d) = match head.take() { Some(h) => Some(h), None => rx.recv().await } {
            if diag && prev.is_none() { eprintln!("[{name}] last {last} U {} u {} pu {:?}", d["U"], d["u"], d["pu"]); }
            let (first, fin, pu) = (d["U"].as_i64().unwrap_or(0), d["u"].as_i64().unwrap_or(0), d["pu"].as_i64());
            if fin < last { continue; } // already in the snapshot
            let ok = match prev {
                // the first applied event must straddle the snapshot
                None => first <= last + 1 && fin >= last,
                Some(p) => if futures { pu == Some(p) } else { first == p + 1 },
            };
            if !ok { eprintln!("[{name}] sequence gap, resyncing"); break; }
            prev = Some(fin);
            e.send(&sym, d["E"].as_i64().unwrap_or(0), Event::Book { snapshot: false, bids: levels(&d["b"]), asks: levels(&d["a"]) });
        }
        session.abort();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// combined-stream envelope {"stream": "...", "data": {...}}
fn unwrap(f: Frame) -> Option<(String, Value)> {
    let Frame::Text(t) = f else { return None };
    let mut v: Value = serde_json::from_str(t).ok()?;
    Some((v["stream"].as_str()?.to_string(), v["data"].take()))
}

/// m = buyer is maker, so the aggressor sold
fn aggressor(m: &Value) -> Side { if m.as_bool() == Some(true) { Side::Sell } else { Side::Buy } }

fn on_spot(e: &Emit, sym: &str, f: Frame) {
    let Some((stream, d)) = unwrap(f) else { return };
    if stream.ends_with("@trade") {
        e.send(sym, d["T"].as_i64().unwrap_or(0), Event::Trade { px: num(&d["p"]), qty: num(&d["q"]), side: aggressor(&d["m"]) });
    } else if stream.ends_with("@bookTicker") {
        e.send(sym, 0, Event::Bbo { bid: num(&d["b"]), bid_qty: num(&d["B"]), ask: num(&d["a"]), ask_qty: num(&d["A"]) });
    }
}

fn on_perp(e: &Emit, sym: &str, hours: &AtomicU32, f: Frame) {
    let Some((_, d)) = unwrap(f) else { return };
    let ts = d["E"].as_i64().unwrap_or(0);
    match d["e"].as_str().unwrap_or("") {
        "aggTrade" => e.send(sym, d["T"].as_i64().unwrap_or(ts), Event::Trade { px: num(&d["p"]), qty: num(&d["q"]), side: aggressor(&d["m"]) }),
        "bookTicker" => e.send(sym, ts, Event::Bbo { bid: num(&d["b"]), bid_qty: num(&d["B"]), ask: num(&d["a"]), ask_qty: num(&d["A"]) }),
        "markPriceUpdate" => {
            // r is per funding interval; emitted per hour, None until the interval is known
            let h = hours.load(Relaxed);
            let funding = opt_num(&d["r"]).filter(|_| h > 0).map(|r| r / h as f64);
            e.send(sym, ts, Event::Mark { mark: num(&d["p"]), index: opt_num(&d["i"]), funding, next_funding_ms: d["T"].as_i64() })
        }
        "forceOrder" => {
            let o = &d["o"];
            let side = if o["S"] == "SELL" { Side::Sell } else { Side::Buy };
            e.send(sym, o["T"].as_i64().unwrap_or(ts), Event::Liquidation { px: num(&o["ap"]), qty: num(&o["q"]), side });
        }
        _ => {}
    }
}

/// fundingInfo lists only symbols whose interval differs from the default 8h. Binance changes
/// intervals live, so refresh hourly; retry every 10s until a fetch succeeds.
async fn funding_interval(sym: String, hours: Arc<AtomicU32>) {
    loop {
        let ok = match interval_hours(&sym).await {
            Ok(h) => { hours.store(h as u32, Relaxed); true }
            Err(err) => { eprintln!("[binance fundingInfo] {err:#}"); false }
        };
        tokio::time::sleep(Duration::from_secs(if ok { 3600 } else { 10 })).await;
    }
}

async fn interval_hours(sym: &str) -> anyhow::Result<u64> {
    match ws::get_json("https://fapi.binance.com/fapi/v1/fundingInfo").await? {
        Value::Array(list) => Ok(list.iter().find(|x| x["symbol"] == sym).and_then(|x| x["fundingIntervalHours"].as_u64()).unwrap_or(8)),
        v => anyhow::bail!("not a list: {v}"),
    }
}

async fn poll_oi(e: Emit, sym: String) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("https://fapi.binance.com/fapi/v1/openInterest?symbol={sym}")).await?;
    e.send(&sym, v["time"].as_i64().unwrap_or(0), Event::OpenInterest { oi: num(&v["openInterest"]), oi_usd: None });
    Ok(())
}

async fn poll_ls(e: Emit, sym: String) -> anyhow::Result<()> {
    for (path, kind) in [("globalLongShortAccountRatio", LsKind::Accounts), ("topLongShortAccountRatio", LsKind::TopAccounts),
                         ("topLongShortPositionRatio", LsKind::TopPositions), ("takerlongshortRatio", LsKind::TakerVolume)] {
        let v = ws::get_json(&format!("https://fapi.binance.com/futures/data/{path}?symbol={sym}&period=5m&limit=1")).await?;
        let r = &v[0];
        let ts = r["timestamp"].as_i64().unwrap_or(0);
        if kind == LsKind::TakerVolume {
            e.send(&sym, ts, Event::LongShort { kind, ratio: num(&r["buySellRatio"]), long_pct: None });
        } else {
            e.send(&sym, ts, Event::LongShort { kind, ratio: num(&r["longShortRatio"]), long_pct: opt_num(&r["longAccount"]) });
        }
    }
    Ok(())
}

fn on_option(e: &Emit, f: Frame) {
    let Some((stream, d)) = unwrap(f) else { return };
    if stream.ends_with("@optionMarkPrice") {
        // mark, greeks and top of book for every option on this underlying
        for o in d.as_array().into_iter().flatten() {
            let sym = o["s"].as_str().unwrap_or("");
            let ts = o["E"].as_i64().unwrap_or(0);
            e.send(sym, ts, Event::Mark { mark: num(&o["mp"]), index: opt_num(&o["i"]), funding: None, next_funding_ms: None });
            // IV is -1 / ~0 when that side has no quote
            let iv = |v: &Value| opt_num(v).filter(|x| *x > 1e-4);
            e.send(sym, ts, Event::Greeks { mark_iv: num(&o["vo"]), bid_iv: iv(&o["b"]), ask_iv: iv(&o["a"]),
                delta: num(&o["d"]), gamma: num(&o["g"]), vega: num(&o["v"]), theta: num(&o["t"]) });
            let (bid, ask) = (num(&o["bo"]), num(&o["ao"]));
            if bid > 0.0 || ask > 0.0 {
                e.send(sym, ts, Event::Bbo { bid, bid_qty: num(&o["bq"]), ask, ask_qty: num(&o["aq"]) });
            }
        }
    } else if stream.ends_with("@optionTrade") {
        // S is the aggressor side ("BUY"/"SELL")
        let side = if d["S"] == "SELL" { Side::Sell } else { Side::Buy };
        e.send(d["s"].as_str().unwrap_or(""), d["T"].as_i64().unwrap_or(0), Event::Trade { px: num(&d["p"]), qty: num(&d["q"]), side });
    }
}

async fn poll_option_info(e: Emit, underlying: String) -> anyhow::Result<()> {
    let v = ws::get_json("https://eapi.binance.com/eapi/v1/exchangeInfo").await?;
    for o in v["optionSymbols"].as_array().into_iter().flatten() {
        if !o["underlying"].as_str().unwrap_or("").starts_with(&underlying) { continue; }
        e.send(o["symbol"].as_str().unwrap_or(""), 0, Event::OptionInfo {
            underlying: underlying.clone(), expiry_ms: o["expiryDate"].as_i64().unwrap_or(0),
            strike: num(&o["strikePrice"]), call: o["side"] == "CALL" });
    }
    Ok(())
}

/// Recent 1m REST history for the chart; see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// Last `bars` `tf`-minute candles (forming one included) plus, for perps, OI / long-short over
/// the same window and settled funding. Klines carry taker-buy volume, so `taker` stays empty.
/// Perp extras are best effort: a failing endpoint is logged and left empty.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    let interval = match tf { 1 => "1m", 5 => "5m", 15 => "15m", 60 => "1h", 240 => "4h", _ => anyhow::bail!("unsupported timeframe {tf}m") };
    let step = tf as i64 * 60_000;
    let (start, end) = window_tf(tf, bars);
    let now = now_ms();
    let api = match sub.market {
        Market::Option => return Ok(History::default()),
        Market::Spot | Market::Margin => "https://api.binance.com/api/v3/klines",
        Market::Perp => "https://fapi.binance.com/fapi/v1/klines",
    };
    // limit 1000 on both (perp allows 1500 but at double weight)
    let klines = async {
        let urls = pages(start, now, step, 1000).into_iter()
            .map(|(s, e)| format!("{api}?symbol={sym}&interval={interval}&startTime={s}&endTime={e}&limit=1000")).collect();
        let k = rows(urls).await?.iter().map(|r| Kline {
            t: r[0].as_i64().unwrap_or(0), o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[5]), buy: opt_num(&r[9]),
        }).filter(|x| x.h > 0.0).collect(); // all-zero rows are missing data
        anyhow::Ok(continuous_tf(k, end, bars, tf))
    };
    if sub.market != Market::Perp { return Ok(History { klines: klines.await?, ..Default::default() }) }

    const F: &str = "https://fapi.binance.com";
    // stats periods: 5m 15m 30m 1h 2h 4h ...; max 500 rows per request, only the last 30 days are
    // served (an older startTime is rejected), so the window is clamped to that
    let (pm, period) = [(5, "5m"), (15, "15m"), (60, "1h"), (240, "4h")].into_iter().filter(|p| p.0 <= tf).last().unwrap_or((5, "5m"));
    let ps = pm as i64 * 60_000;
    let start_p = (start / ps * ps).max((now - 30 * 86_400_000 + 60_000 + ps - 1) / ps * ps);
    let stat = |path: &str, field: &'static str| {
        let urls = pages(start_p, now, ps, 500).into_iter()
            .map(|(s, e)| format!("{F}/futures/data/{path}?symbol={sym}&period={period}&startTime={s}&endTime={e}&limit=500")).collect();
        async move { series(rows(urls).await?.iter().map(|r| (r["timestamp"].as_i64().unwrap_or(0), num(&r[field])))) }
    };
    let funding = async {
        // rates are per interval; fundingInfo is the same interval source the live connector uses.
        // Start one max interval (8h) early so the settlement covering the window start is included.
        // ponytail: every rate is divided by the CURRENT interval
        let h = interval_hours(&sym).await?;
        let urls = pages(start - 8 * 3_600_000, now, h as i64 * 3_600_000, 1000).into_iter()
            .map(|(s, e)| format!("{F}/fapi/v1/fundingRate?symbol={sym}&startTime={s}&endTime={e}&limit=1000")).collect();
        series(rows(urls).await?.iter().map(|r| (r["fundingTime"].as_i64().unwrap_or(0), num(&r["fundingRate"]) / h as f64)))
    };
    let (klines, oi, funding, long_short) = tokio::join!(klines,
        stat("openInterestHist", "sumOpenInterest"), funding, stat("globalLongShortAccountRatio", "longShortRatio"));
    let opt = |name: &str, r: anyhow::Result<Vec<(i64, f64)>>| r.unwrap_or_else(|e| { eprintln!("[binance history {name}] {e:#}"); vec![] });
    // zero OI / ratio rows are missing data
    let pos = |v: Vec<(i64, f64)>| v.into_iter().filter(|x| x.1 > 0.0).collect();
    Ok(History { klines: klines?, oi: pos(opt("oi", oi)), funding: opt("funding", funding), long_short: pos(opt("ls", long_short)), taker: vec![] })
}

/// [start, end] split into inclusive windows of at most `n` steps of `step` ms
fn pages(start: i64, end: i64, step: i64, n: i64) -> Vec<(i64, i64)> {
    (start..=end).step_by((step * n) as usize).map(|s| (s, (s + step * (n - 1)).min(end))).collect()
}

/// GETs all urls concurrently and concatenates their top-level arrays
async fn rows(urls: Vec<String>) -> anyhow::Result<Vec<Value>> {
    let pages = futures_util::future::try_join_all(urls.iter().map(|u| ws::get_json(u))).await?;
    Ok(pages.into_iter().flat_map(|p| p.as_array().cloned().unwrap_or_default()).collect())
}

/// ascending, deduplicated by time, NaN rows dropped
fn series(it: impl Iterator<Item = (i64, f64)>) -> anyhow::Result<Vec<(i64, f64)>> {
    let mut v: Vec<_> = it.filter(|(t, x)| *t > 0 && x.is_finite()).collect();
    v.sort_by_key(|x| x.0);
    v.dedup_by_key(|x| x.0);
    Ok(v)
}
