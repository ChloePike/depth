//! Gate: spot/margin on api.gateio.ws/ws/v4, USDT futures on fx-ws, options on op-ws.
//! Futures and options sizes are in contracts; converted to base units with the contract multiplier.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::Value;
use std::time::Duration;
use tokio::task::JoinHandle;

const REST: &str = "https://api.gateio.ws/api/v4";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Gate, market: sub.market, tx };
    let (base, quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    let sym = format!("{base}_{quote}");
    match sub.market {
        Market::Spot | Market::Margin => {
            let spec = subscribe(Spec::new(format!("gate {:?}", sub.market), "wss://api.gateio.ws/ws/v4/"), "spot", &[
                ("spot.trades", vec![sym.clone()]),
                ("spot.order_book", vec![sym.clone(), "50".into(), "100ms".into()]),
                ("spot.book_ticker", vec![sym.clone()]),
            ]);
            let e2 = e.clone();
            let mut tasks = vec![tokio::spawn(ws::run(spec, move |f| on_spot(&e, f)))];
            if sub.market == Market::Margin {
                tasks.push(tokio::spawn(ws::poll("gate borrow".into(), Duration::from_secs(300),
                    move || poll_borrow(e2.clone(), [base.clone(), quote.clone()]))));
            }
            tasks
        }
        Market::Perp => {
            let (e2, sym2) = (e.clone(), sym.clone());
            vec![
                tokio::spawn(async move {
                    // ponytail: interval read once per task start; Gate can change it mid-session (8h -> 4h), poll the contract if that matters
                    let (mult, hours) = multiplier(&format!("{REST}/futures/usdt/contracts/{sym}")).await;
                    let spec = subscribe(Spec::new("gate perp", "wss://fx-ws.gateio.ws/v4/ws/usdt"), "futures", &[
                        ("futures.trades", vec![sym.clone()]),
                        ("futures.order_book", vec![sym.clone(), "50".into(), "0".into()]),
                        ("futures.book_ticker", vec![sym.clone()]),
                        ("futures.tickers", vec![sym.clone()]),
                        ("futures.public_liquidates", vec![sym.clone()]),
                    ]);
                    ws::run(spec, move |f| on_perp(&e, mult, hours, f)).await
                }),
                tokio::spawn(ws::poll("gate ls".into(), Duration::from_secs(60), move || poll_ls(e2.clone(), sym2.clone()))),
            ]
        }
        Market::Option => {
            let e2 = e.clone();
            let (u, u2) = (sym.clone(), sym.clone());
            vec![
                tokio::spawn(async move {
                    loop {
                        let names = match option_names(&u).await {
                            Ok(n) if !n.is_empty() => n,
                            r => { eprintln!("[gate option] contract list: {r:?}"); tokio::time::sleep(Duration::from_secs(10)).await; continue }
                        };
                        // ponytail: one multiplier for the whole chain (all BTC options are 0.01 today); per-contract map if they ever differ
                        let (mult, _) = multiplier(&format!("{REST}/options/contracts/{}", names[0])).await;
                        let spec = subscribe(Spec::new("gate option", "wss://op-ws.gateio.live/v4/ws"), "options", &[
                            ("options.contract_tickers", names),
                            ("options.ul_trades", vec![u.clone()]),
                        ]);
                        let (e, mut fwd) = (e.clone(), std::collections::HashMap::new());
                        // ponytail: the contract list is fixed per session; rebuilt every 6h to pick up new listings
                        let _ = tokio::time::timeout(Duration::from_secs(6 * 3600), ws::run(spec, move |f| on_option(&e, mult, &mut fwd, f))).await;
                    }
                }),
                tokio::spawn(ws::poll("gate optinfo".into(), Duration::from_secs(600), move || poll_option_info(e2.clone(), u2.clone()))),
            ]
        }
    }
}

/// adds one subscribe message per channel plus the app-level ping
fn subscribe(mut spec: Spec, prefix: &str, chans: &[(&str, Vec<String>)]) -> Spec {
    for (ch, payload) in chans {
        spec = spec.sub(serde_json::json!({"time": now_ms() / 1000, "channel": ch, "event": "subscribe", "payload": payload}).to_string());
    }
    spec.ping(Duration::from_secs(10), format!(r#"{{"time":0,"channel":"{prefix}.ping"}}"#))
}

/// contract multiplier (quanto_multiplier for futures, multiplier for options) and the funding interval in hours
/// (futures only, from funding_interval seconds); retries until the multiplier is found
async fn multiplier(url: &str) -> (f64, Option<f64>) {
    loop {
        match ws::get_json(url).await {
            Ok(v) => {
                let m = opt_num(&v["quanto_multiplier"]).or_else(|| opt_num(&v["multiplier"]));
                let hours = opt_num(&v["funding_interval"]).map(|s| s / 3600.0).filter(|h| *h > 0.0);
                if let Some(m) = m.filter(|m| *m > 0.0) { return (m, hours); }
                eprintln!("[gate] no multiplier in {url}");
            }
            Err(err) => eprintln!("[gate] {err:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// {"channel": "...", "event": "update"|"all", "result": ...}; returns (channel, result)
fn unwrap(f: Frame) -> Option<(String, Value)> {
    let Frame::Text(t) = f else { return None };
    let mut v: Value = serde_json::from_str(t).ok()?;
    if !matches!(v["event"].as_str(), Some("update" | "all")) { return None; }
    Some((v["channel"].as_str()?.to_string(), v["result"].take()))
}

/// results are either one object or an array of them
fn each(v: &Value) -> Vec<&Value> {
    match v { Value::Array(a) => a.iter().collect(), v => vec![v] }
}

fn side_of(size: f64) -> Side { if size < 0.0 { Side::Sell } else { Side::Buy } }

fn on_spot(e: &Emit, f: Frame) {
    let Some((ch, d)) = unwrap(f) else { return };
    match ch.as_str() {
        "spot.trades" => {
            let side = if d["side"] == "sell" { Side::Sell } else { Side::Buy };
            e.send(d["currency_pair"].as_str().unwrap_or(""), num(&d["create_time_ms"]) as i64,
                Event::Trade { px: num(&d["price"]), qty: num(&d["amount"]), side });
        }
        "spot.order_book" => e.send(d["s"].as_str().unwrap_or(""), d["t"].as_i64().unwrap_or(0),
            Event::Book { snapshot: true, bids: levels(&d["bids"]), asks: levels(&d["asks"]) }),
        "spot.book_ticker" => e.send(d["s"].as_str().unwrap_or(""), d["t"].as_i64().unwrap_or(0),
            Event::Bbo { bid: num(&d["b"]), bid_qty: num(&d["B"]), ask: num(&d["a"]), ask_qty: num(&d["A"]) }),
        _ => {}
    }
}

/// `hours` = funding interval; funding is emitted per hour
fn on_perp(e: &Emit, mult: f64, hours: Option<f64>, f: Frame) {
    let Some((ch, d)) = unwrap(f) else { return };
    let lv = |v: &Value| v.as_array().map(|a| a.iter().map(|l| (num(&l["p"]), num(&l["s"]) * mult)).collect()).unwrap_or_default();
    match ch.as_str() {
        // size sign = taker side
        "futures.trades" => for t in each(&d) {
            let size = num(&t["size"]);
            e.send(t["contract"].as_str().unwrap_or(""), t["create_time_ms"].as_i64().unwrap_or(0),
                Event::Trade { px: num(&t["price"]), qty: size.abs() * mult, side: side_of(size) });
        },
        "futures.order_book" => e.send(d["contract"].as_str().unwrap_or(""), d["t"].as_i64().unwrap_or(0),
            Event::Book { snapshot: true, bids: lv(&d["bids"]), asks: lv(&d["asks"]) }),
        "futures.book_ticker" => e.send(d["s"].as_str().unwrap_or(""), d["t"].as_i64().unwrap_or(0),
            Event::Bbo { bid: num(&d["b"]), bid_qty: num(&d["B"]) * mult, ask: num(&d["a"]), ask_qty: num(&d["A"]) * mult }),
        "futures.tickers" => for t in each(&d) {
            let sym = t["contract"].as_str().unwrap_or("");
            let mark = num(&t["mark_price"]);
            e.send(sym, 0, Event::Mark { mark, index: opt_num(&t["index_price"]), funding: opt_num(&t["funding_rate"]).zip(hours).map(|(r, h)| r / h),
                next_funding_ms: t["funding_next_apply"].as_i64().map(|s| s * 1000) });
            let oi = num(&t["total_size"]) * mult;
            if oi.is_finite() { e.send(sym, 0, Event::OpenInterest { oi, oi_usd: Some(oi * mark).filter(|x| x.is_finite()) }); }
        },
        // size sign = liquidation order side; negative means a long was liquidated
        "futures.public_liquidates" => for l in each(&d) {
            let size = num(&l["size"]);
            e.send(l["contract"].as_str().unwrap_or(""), l["time_ms"].as_i64().or(l["time"].as_i64()).unwrap_or(0),
                Event::Liquidation { px: num(&l["price"]), qty: size.abs() * mult, side: side_of(size) });
        },
        _ => {}
    }
}

async fn poll_ls(e: Emit, sym: String) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{REST}/futures/usdt/contract_stats?contract={sym}&limit=1")).await?;
    let r = &v[0];
    let ts = r["time"].as_i64().unwrap_or(0) * 1000;
    let pct = |x: f64| Some(x / (1.0 + x)).filter(|p| p.is_finite());
    for (key, kind) in [("lsr_account", LsKind::Accounts), ("top_lsr_account", LsKind::TopAccounts),
                        ("top_lsr_size", LsKind::TopPositions), ("lsr_taker", LsKind::TakerVolume)] {
        let Some(ratio) = opt_num(&r[key]) else { continue };
        let long_pct = if kind == LsKind::TakerVolume { None } else { pct(ratio) };
        e.send(&sym, ts, Event::LongShort { kind, ratio, long_pct });
    }
    Ok(())
}

// ponytail: the real margin borrow rate (/margin/uni/estimate_rate) needs a signed request; this uses the public
// Simple Earn lending rate (annualized, est_rate) as a proxy. Switch to estimate_rate once API keys exist.
async fn poll_borrow(e: Emit, assets: [String; 2]) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{REST}/earn/uni/rate")).await?;
    for r in v.as_array().into_iter().flatten() {
        let Some(asset) = assets.iter().find(|a| r["currency"] == a.as_str()) else { continue };
        e.send(asset, 0, Event::BorrowRate { asset: asset.clone(), apr: num(&r["est_rate"]) });
    }
    Ok(())
}

async fn option_names(underlying: &str) -> anyhow::Result<Vec<String>> {
    let v = ws::get_json(&format!("{REST}/options/contracts?underlying={underlying}")).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(String::from)).collect())
}

/// `fwd` remembers the last forward per contract so Forward is only sent on change
fn on_option(e: &Emit, mult: f64, fwd: &mut std::collections::HashMap<String, f64>, f: Frame) {
    let Some((ch, d)) = unwrap(f) else { return };
    match ch.as_str() {
        "options.contract_tickers" => for t in each(&d) {
            let sym = t["name"].as_str().unwrap_or("");
            e.send(sym, 0, Event::Mark { mark: num(&t["mark_price"]), index: opt_num(&t["index_price"]), funding: None, next_funding_ms: None });
            // underlying_price is the per-expiry forward (differs across expiries, index_price is spot)
            if let Some(px) = opt_num(&t["underlying_price"]).filter(|p| *p > 0.0) {
                if fwd.insert(sym.to_string(), px) != Some(px) { e.send(sym, 0, Event::Forward { px }); }
            }
            // IV is "0" when that side has no quote
            let iv = |v: &Value| opt_num(v).filter(|x| *x > 1e-4);
            e.send(sym, 0, Event::Greeks { mark_iv: num(&t["mark_iv"]), bid_iv: iv(&t["bid_iv"]), ask_iv: iv(&t["ask_iv"]),
                delta: num(&t["delta"]), gamma: num(&t["gamma"]), vega: num(&t["vega"]), theta: num(&t["theta"]) });
            let (bid, ask) = (num(&t["bid1_price"]), num(&t["ask1_price"]));
            if bid > 0.0 || ask > 0.0 {
                e.send(sym, 0, Event::Bbo { bid, bid_qty: num(&t["bid1_size"]) * mult, ask, ask_qty: num(&t["ask1_size"]) * mult });
            }
        },
        "options.ul_trades" => for t in each(&d) {
            let size = num(&t["size"]);
            let ts = t["create_time_ms"].as_i64().unwrap_or_else(|| num(&t["create_time"]) as i64 * 1000);
            e.send(t["contract"].as_str().unwrap_or(""), ts, Event::Trade { px: num(&t["price"]), qty: size.abs() * mult, side: side_of(size) });
        },
        _ => {}
    }
}

async fn poll_option_info(e: Emit, underlying: String) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{REST}/options/contracts?underlying={underlying}")).await?;
    for c in v.as_array().into_iter().flatten() {
        e.send(c["name"].as_str().unwrap_or(""), 0, Event::OptionInfo {
            underlying: underlying.clone(), expiry_ms: c["expiration_time"].as_i64().unwrap_or(0) * 1000,
            strike: num(&c["strike_price"]), call: c["is_call"].as_bool() == Some(true) });
    }
    Ok(())
}

/// Recent REST history for the chart (1m); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `history` at `tf`-minute resolution; see `ex::history_tf`.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let sym = format!("{}_{}", sub.base, sub.quote).to_uppercase();
    let perp = match sub.market { Market::Option => return Ok(History::default()), m => m == Market::Perp };
    // candle interval (spot and futures agree) and the contract_stats period not coarser than it (finest is 5m)
    let (iv, stats_iv, stats_min) = match tf { 1 => ("1m", "5m", 5), 5 => ("5m", "5m", 5), 15 => ("15m", "15m", 15),
        60 => ("1h", "1h", 60), 240 => ("4h", "4h", 240), _ => anyhow::bail!("gate: unsupported timeframe {tf}m") };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    // spot: 1000 points per request, futures: 2000; from/to are inclusive seconds
    let per = if perp { 2000 } else { 1000 } * step;
    let urls = (0..).map(|k| end - k * per).take_while(|e| *e >= start).map(|e| ((e - per + step).max(start), e)).map(|(a, b)| if perp {
        format!("{REST}/futures/usdt/candlesticks?contract={sym}&interval={iv}&from={}&to={}", a / 1000, b / 1000)
    } else {
        format!("{REST}/spot/candlesticks?currency_pair={sym}&interval={iv}&from={}&to={}", a / 1000, b / 1000)
    }).collect();
    let candles = all(urls);
    if !perp {
        // spot rows: [t s, quote vol, close, high, low, open, base vol, closed]
        let klines: Vec<Kline> = candles.await?.iter().flat_map(|v| v.as_array().cloned().unwrap_or_default()).map(|r| Kline {
            t: num(&r[0]) as i64 * 1000, o: num(&r[5]), h: num(&r[3]), l: num(&r[4]), c: num(&r[2]), vol: num(&r[6]), buy: None,
        }).collect();
        return Ok(History { klines: tidy(klines, start, end, bars, tf), ..Default::default() });
    }
    // contract_stats with `from` returns `limit` rows ascending; funding: one row per settlement (>= 1h apart)
    // ponytail: one request each, capped at 2000 stats rows / 1000 settlements and 180 days back (venue limits)
    let (from_s, now_s) = (start / 1000, now_ms() / 1000);
    let stats_n = (bars * tf as usize / stats_min + 2).min(2000);
    let (candles, contract, stats, fund) = tokio::join!(candles,
        get(format!("{REST}/futures/usdt/contracts/{sym}")),
        get(format!("{REST}/futures/usdt/contract_stats?contract={sym}&interval={stats_iv}&from={}&limit={stats_n}", (from_s - from_s % (stats_min as i64 * 60)).max(now_s - 180 * 86400 + 3600))),
        // settlement stamps can trail the hour by seconds, so `to` is now
        get(format!("{REST}/futures/usdt/funding_rate?contract={sym}&from={}&to={now_s}&limit=1000", (from_s - 8 * 3600).max(now_s - 180 * 86400 + 60))));
    let contract = contract?;
    let mult = opt_num(&contract["quanto_multiplier"]).filter(|m| *m > 0.0).ok_or_else(|| anyhow::anyhow!("no quanto_multiplier: {contract}"))?;
    // same interval source as the live connector
    // ponytail: past settlements are scaled by today's interval; wrong only across an interval change
    let hours = opt_num(&contract["funding_interval"]).map(|s| s / 3600.0).filter(|h| *h > 0.0);
    let klines: Vec<Kline> = candles?.iter().flat_map(|v| v.as_array().cloned().unwrap_or_default()).map(|r| Kline {
        t: num(&r["t"]) as i64 * 1000, o: num(&r["o"]), h: num(&r["h"]), l: num(&r["l"]), c: num(&r["c"]), vol: num(&r["v"]) * mult, buy: None,
    }).collect();
    let mut h = History { klines: tidy(klines, start, end, bars, tf), ..Default::default() };
    let stats = stats.unwrap_or_else(|e| { eprintln!("[gate history] {e:#}"); Value::Null });
    for r in stats.as_array().into_iter().flatten() {
        let t = num(&r["time"]) as i64 * 1000;
        if let Some(oi) = opt_num(&r["open_interest"]).filter(|x| *x > 0.0) { h.oi.push((t, oi * mult)); }
        if let Some(x) = opt_num(&r["lsr_account"]) { h.long_short.push((t, x)); }
        if let (Some(b), Some(s)) = (opt_num(&r["long_taker_size"]), opt_num(&r["short_taker_size"])) { h.taker.push((t, b * mult, s * mult)); }
    }
    if let Some(hours) = hours {
        let fund = fund.unwrap_or_else(|e| { eprintln!("[gate history] {e:#}"); Value::Null });
        // keep the settlement just before the window so the series covers its start
        let from = start - (hours * 3.6e6) as i64;
        h.funding = fund.as_array().into_iter().flatten().map(|r| (num(&r["t"]) as i64 * 1000, num(&r["r"]) / hours))
            .filter(|(t, r)| *t >= from && r.is_finite()).collect();
        h.funding.sort_by_key(|x| x.0);
    }
    for s in [&mut h.oi, &mut h.long_short] { s.sort_by_key(|x| x.0); }
    h.taker.sort_by_key(|x| x.0);
    Ok(h)
}

/// owned-URL GET so futures can be joined
async fn get(url: String) -> anyhow::Result<Value> { ws::get_json(&url).await }

/// GETs in order, at most 4 in flight (Gate public endpoints allow 200 req/10s)
async fn all(urls: Vec<String>) -> anyhow::Result<Vec<Value>> {
    use futures_util::{StreamExt, TryStreamExt};
    futures_util::stream::iter(urls).map(get).buffered(4).try_collect().await
}

/// ascending, gap-filled `tf` candles inside the window, the last `bars`
fn tidy(mut k: Vec<Kline>, start: i64, end: i64, bars: usize, tf: u32) -> Vec<Kline> {
    k.retain(|x| x.t >= start && x.c.is_finite());
    continuous_tf(k, end, bars, tf)
}
