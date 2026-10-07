//! OKX v5: everything (spot, margin, USDT swaps, options) streams from ws /public; REST for
//! instrument metadata, borrow rates and rubik long/short stats.
//!
//! Units: swap and option sizes are converted from contracts to base units (BTC) using
//! ctVal * ctMult. Option prices (mark, bbo, trades) are quoted by OKX in BTC and emitted in USD:
//! mark/bbo are multiplied by the latest BTC-USD index, trades by the index carried in the trade.
//! Option greeks are OKX's Black-Scholes greeks (deltaBS, gammaBS, vegaBS, thetaBS; USD terms).
use crate::ws::{self, Frame, Spec};
use crate::*;
use anyhow::bail;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WS: &str = "wss://ws.okx.com:8443/ws/v5/public";
const REST: &str = "https://www.okx.com/api/v5";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Okx, market: sub.market, tx };
    let (b, q) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    match sub.market {
        Market::Spot | Market::Margin => {
            let sym = format!("{b}-{q}");
            let mut v = vec![tokio::spawn(book_feed(e.clone(), sym.clone(), 1.0))];
            if sub.market == Market::Margin {
                v.push(tokio::spawn(ws::poll("okx borrow".into(), Duration::from_secs(300),
                    move || poll_borrow(e.clone(), sym.clone(), [b.clone(), q.clone()]))));
            }
            v
        }
        Market::Perp => {
            let sym = format!("{b}-{q}-SWAP");
            let (e2, sym2) = (e.clone(), sym.clone());
            vec![
                tokio::spawn(async move { let ct = contract_size(&sym).await; book_feed(e, sym, ct).await }),
                tokio::spawn(ws::poll("okx ls".into(), Duration::from_secs(60), move || poll_ls(e2.clone(), sym2.clone()))),
            ]
        }
        Market::Option => vec![tokio::spawn(option_feed(e, format!("{b}-USD"), b))],
    }
}

/// GET {REST}/{path}, returns `data` or fails on a non-zero OKX code
async fn get(path: &str) -> anyhow::Result<Value> {
    let mut v = ws::get_json(&format!("{REST}/{path}")).await?;
    if v["code"] != "0" { bail!("okx {path}: {} {}", v["code"], v["msg"]) }
    Ok(v["data"].take())
}

fn arg(channel: &str, inst: &str) -> Value { json!({ "channel": channel, "instId": inst }) }
/// OKX timestamps are ms strings
fn ms(v: &Value) -> i64 { v.as_str().and_then(|s| s.parse().ok()).or(v.as_i64()).unwrap_or(0) }
fn side(v: &Value) -> Side { if v == "sell" { Side::Sell } else { Side::Buy } }
fn spec(name: String) -> Spec { Spec::new(name, WS).ping(Duration::from_secs(25), "ping") }

/// aborts the task when dropped
struct Abort(JoinHandle<()>);
impl Drop for Abort { fn drop(&mut self) { self.0.abort() } }

/// contracts -> base units for a swap; retried until the REST call succeeds
async fn contract_size(sym: &str) -> f64 {
    loop {
        match ct_val(sym).await {
            Ok(x) => return x,
            Err(err) => eprintln!("[okx] {err:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn ct_val(sym: &str) -> anyhow::Result<f64> {
    let d = get(&format!("public/instruments?instType=SWAP&instId={sym}")).await?;
    if num(&d[0]["ctVal"]) > 0.0 { Ok(num(&d[0]["ctVal"]) * opt_num(&d[0]["ctMult"]).unwrap_or(1.0)) } else { bail!("no ctVal for {sym}: {d}") }
}

/// per-session state of the spot/swap connection
#[derive(Default)]
struct St { seq: Option<i64>, index: Option<f64>, funding: Option<f64>, next_funding: Option<i64> }

/// Spot (mult 1) or swap (mult = contract size). `books` is a 400-level snapshot + incremental
/// stream; a seqId gap tears the session down and resubscribes for a fresh snapshot.
async fn book_feed(e: Emit, sym: String, mult: f64) {
    let mut args = vec![arg("books", &sym), arg("bbo-tbt", &sym), arg("trades", &sym)];
    if let Some(idx) = sym.strip_suffix("-SWAP") {
        args.extend([arg("mark-price", &sym), arg("funding-rate", &sym), arg("open-interest", &sym), arg("index-tickers", idx),
            json!({ "channel": "liquidation-orders", "instType": "SWAP" })]);
    }
    let sub = json!({ "op": "subscribe", "args": args }).to_string();
    let (gap_tx, mut gap_rx) = mpsc::unbounded_channel();
    loop {
        let (e, s, g, mut st) = (e.clone(), sym.clone(), gap_tx.clone(), St::default());
        let _task = Abort(tokio::spawn(ws::run(spec(format!("okx {:?}", e.market)).sub(sub.clone()),
            move |f| on_msg(&e, &s, mult, &mut st, &g, f))));
        gap_rx.recv().await;
        eprintln!("[okx] {sym} book sequence gap, resubscribing");
    }
}

fn on_msg(e: &Emit, sym: &str, mult: f64, st: &mut St, gap: &mpsc::UnboundedSender<()>, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return }; // "pong"
    if v["event"] == "error" { eprintln!("[okx] {t}"); }
    let ch = v["arg"]["channel"].as_str().unwrap_or("");
    let lv = |x: &Value| levels(x).into_iter().map(|(p, q)| (p, q * mult)).collect::<Vec<_>>();
    for d in v["data"].as_array().into_iter().flatten() {
        let ts = ms(&d["ts"]);
        match ch {
            "books" => {
                // ponytail: OKX now sends checksum 0, so only seqId continuity is checked
                let snapshot = v["action"] == "snapshot";
                if !snapshot && (st.seq.is_none() || d["prevSeqId"].as_i64() != st.seq) {
                    if st.seq.take().is_some() { let _ = gap.send(()); }
                    return;
                }
                st.seq = d["seqId"].as_i64();
                e.send(sym, ts, Event::Book { snapshot, bids: lv(&d["bids"]), asks: lv(&d["asks"]) });
            }
            "bbo-tbt" => {
                let (b, a) = (&d["bids"][0], &d["asks"][0]);
                e.send(sym, ts, Event::Bbo { bid: num(&b[0]), bid_qty: num(&b[1]) * mult, ask: num(&a[0]), ask_qty: num(&a[1]) * mult });
            }
            "trades" => e.send(sym, ts, Event::Trade { px: num(&d["px"]), qty: num(&d["sz"]) * mult, side: side(&d["side"]) }),
            "index-tickers" => st.index = opt_num(&d["idxPx"]),
            "funding-rate" => {
                // fundingRate settles at fundingTime and accrues over [prevFundingTime, fundingTime];
                // no dedicated interval field exists, so derive it (fallback: nextFundingTime - fundingTime)
                let (prev, at, next) = (ms(&d["prevFundingTime"]), ms(&d["fundingTime"]), ms(&d["nextFundingTime"]));
                let span = if prev > 0 && at > prev { at - prev } else { next - at };
                let hours = (span > 0).then(|| span as f64 / 3_600_000.0);
                st.funding = opt_num(&d["fundingRate"]).zip(hours).map(|(r, h)| r / h);
                st.next_funding = Some(at).filter(|t| *t > 0);
            }
            "mark-price" => e.send(sym, ts, Event::Mark { mark: num(&d["markPx"]), index: st.index, funding: st.funding, next_funding_ms: st.next_funding }),
            "open-interest" => e.send(sym, ts, Event::OpenInterest { oi: num(&d["oiCcy"]), oi_usd: opt_num(&d["oiUsd"]) }),
            "liquidation-orders" if d["instId"] == sym => {
                for x in d["details"].as_array().into_iter().flatten() {
                    e.send(sym, ms(&x["ts"]), Event::Liquidation { px: num(&x["bkPx"]), qty: num(&x["sz"]) * mult, side: side(&x["side"]) });
                }
            }
            _ => {}
        }
    }
}

/// `rate` is a daily rate (market-wide basic tier), annualized x365
async fn poll_borrow(e: Emit, sym: String, assets: [String; 2]) -> anyhow::Result<()> {
    let d = get("public/interest-rate-loan-quota").await?;
    for r in d[0]["basic"].as_array().into_iter().flatten() {
        if let Some(a) = assets.iter().find(|a| r["ccy"] == a.as_str()) {
            e.send(&sym, 0, Event::BorrowRate { asset: a.clone(), apr: num(&r["rate"]) * 365.0 });
        }
    }
    Ok(())
}

async fn poll_ls(e: Emit, sym: String) -> anyhow::Result<()> {
    for (path, kind) in [("contracts/long-short-account-ratio-contract", LsKind::Accounts),
                         ("contracts/long-short-account-ratio-contract-top-trader", LsKind::TopAccounts),
                         ("contracts/long-short-position-ratio-contract-top-trader", LsKind::TopPositions),
                         ("taker-volume-contract", LsKind::TakerVolume)] {
        let d = get(&format!("rubik/stat/{path}?instId={sym}&period=5m&limit=2")).await?;
        // the newest taker-volume bucket can be half-filled (one side 0), so take the first complete row
        let Some(r) = d.as_array().into_iter().flatten().find(|r| r.as_array().is_some_and(|a| a.len() > 1 && a[1..].iter().all(|x| num(x) > 0.0))) else { continue };
        // taker volume rows are [ts, sellVol, buyVol]; ratio rows are [ts, long/short]
        let (ratio, long_pct) = if kind == LsKind::TakerVolume { (num(&r[2]) / num(&r[1]), None) }
                                else { let x = num(&r[1]); (x, Some(x / (1.0 + x))) };
        e.send(&sym, ms(&r[0]), Event::LongShort { kind, ratio, long_pct });
    }
    Ok(())
}

/// Whole option chain of one instFamily on one connection: opt-summary + option-trades by family,
/// mark-price + tickers per instId (~650 each, sent in chunks to stay under the 64KB request cap).
async fn option_feed(e: Emit, family: String, base: String) {
    loop {
        let list = match get(&format!("public/instruments?instType=OPTION&instFamily={family}")).await {
            Ok(d) => d,
            // 51000: OKX lists no options for this underlying; nothing to retry
            Err(err) if format!("{err:#}").contains("\"51000\"") => return,
            Err(err) => { eprintln!("[okx option] {err:#}"); tokio::time::sleep(Duration::from_secs(10)).await; continue }
        };
        let mut args = vec![json!({ "channel": "opt-summary", "instFamily": family }),
                            json!({ "channel": "option-trades", "instType": "OPTION", "instFamily": family }),
                            arg("index-tickers", &family)];
        let mut mult = 0.01;
        for o in list.as_array().into_iter().flatten() {
            let id = o["instId"].as_str().unwrap_or("");
            mult = num(&o["ctVal"]) * num(&o["ctMult"]);
            e.send(id, 0, Event::OptionInfo { underlying: base.clone(), expiry_ms: ms(&o["expTime"]), strike: num(&o["stk"]), call: o["optType"] == "C" });
            args.extend([arg("mark-price", id), arg("tickers", id)]);
        }
        let mut sp = spec("okx option".into());
        for c in args.chunks(300) { sp = sp.sub(json!({ "op": "subscribe", "args": c }).to_string()); }
        let (e2, mut idx, mut fwd) = (e.clone(), f64::NAN, HashMap::new());
        // ponytail: chain is re-listed by restarting every 6h; strikes listed in between stream only after that
        let _ = tokio::time::timeout(Duration::from_secs(6 * 3600), ws::run(sp, move |f| on_option(&e2, mult, &mut idx, &mut fwd, f))).await;
    }
}

/// `fwd`: last Forward sent per symbol, so it is only emitted on change
fn on_option(e: &Emit, mult: f64, idx: &mut f64, fwd: &mut HashMap<String, f64>, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    if v["event"] == "error" { eprintln!("[okx option] {t}"); }
    let ch = v["arg"]["channel"].as_str().unwrap_or("");
    for d in v["data"].as_array().into_iter().flatten() {
        let (sym, ts) = (d["instId"].as_str().unwrap_or(""), ms(&d["ts"]));
        match ch {
            "index-tickers" => *idx = num(&d["idxPx"]),
            "opt-summary" => {
                let iv = |v: &Value| opt_num(v).filter(|x| *x > 1e-4);
                e.send(sym, ts, Event::Greeks { mark_iv: num(&d["markVol"]), bid_iv: iv(&d["bidVol"]), ask_iv: iv(&d["askVol"]),
                    delta: num(&d["deltaBS"]), gamma: num(&d["gammaBS"]), vega: num(&d["vegaBS"]), theta: num(&d["thetaBS"]) });
                // fwdPx is the per-expiry forward, already in USD (unlike the BTC-quoted option prices)
                if let Some(px) = opt_num(&d["fwdPx"]).filter(|p| *p > 0.0) {
                    if fwd.insert(sym.to_string(), px) != Some(px) { e.send(sym, ts, Event::Forward { px }); }
                }
            }
            "option-trades" => e.send(sym, ts, Event::Trade { px: num(&d["px"]) * num(&d["idxPx"]), qty: num(&d["sz"]) * mult, side: side(&d["side"]) }),
            _ if !idx.is_finite() => {} // prices below need the index
            "mark-price" => e.send(sym, ts, Event::Mark { mark: num(&d["markPx"]) * *idx, index: Some(*idx), funding: None, next_funding_ms: None }),
            "tickers" => {
                // empty sides come as "" -> 0
                let z = |v: &Value| opt_num(v).unwrap_or(0.0);
                let (bid, ask) = (z(&d["bidPx"]) * *idx, z(&d["askPx"]) * *idx);
                if bid > 0.0 || ask > 0.0 {
                    e.send(sym, ts, Event::Bbo { bid, bid_qty: z(&d["bidSz"]) * mult, ask, ask_qty: z(&d["askSz"]) * mult });
                }
            }
            _ => {}
        }
    }
}

/// Recent 1m REST history for the chart; see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// Last `bars` `tf`-minute candles (forming one included) plus, for perps, OI / long-short /
/// taker volume over the same window and settled funding. OKX candles have no taker split, so
/// perps get rubik taker volume (base coin). Perp extras are best effort: a failing endpoint is
/// logged and left empty.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let (b, q) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    let sym = match sub.market {
        Market::Option => return Ok(History::default()),
        Market::Spot | Market::Margin => format!("{b}-{q}"),
        Market::Perp => format!("{b}-{q}-SWAP"),
    };
    // 1H/4H are Hong Kong time buckets, which coincide with UTC multiples of 1h/4h
    let bar = match tf { 1 => "1m", 5 => "5m", 15 => "15m", 60 => "1H", 240 => "4H", _ => bail!("unsupported timeframe {tf}m") };
    let perp = sub.market == Market::Perp;
    let step = tf as i64 * 60_000;
    let (start, end) = window_tf(tf, bars);
    let now = now_ms();
    let klines = async {
        let mult = if perp { ct_val(&sym).await? } else { 1.0 };
        // history-candles serves recent bars too (incl. the forming one), 300 per page, 20 req/2s;
        // before/after are exclusive bounds
        let urls = pages(start, now, step, 300).into_iter()
            .map(|(s, e)| format!("market/history-candles?instId={sym}&bar={bar}&before={}&after={}&limit=300", s - 1, e + 1)).collect();
        // rows: [ts, o, h, l, c, vol (contracts for swaps, base for spot), ...]
        let k = rows(urls, 20).await?.iter().map(|r| Kline {
            t: ms(&r[0]), o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[5]) * mult, buy: None,
        }).filter(|x| x.h > 0.0).collect(); // all-zero rows are missing data
        anyhow::Ok(continuous_tf(k, end, bars, tf))
    };
    if !perp { return Ok(History { klines: klines.await?, ..Default::default() }) }

    // rubik stats: periods 5m 15m 30m 1H 2H 4H ...; 100 rows per page, 5 req/2s per endpoint
    // (4 here, the live poller needs one); begin/end are exclusive bounds. Retention depends on the
    // period (about 4d at 5m, 10-15d at 15m, 40-60d at 1H, >200d at 4H; taker volume returns
    // all-zero rows past it), so pages go newest first and stop at the first page without data.
    // ponytail: capped at 20 pages (2000 rows) per endpoint; raise it if charts ask for more bars
    let (pm, period) = [(5, "5m"), (15, "15m"), (60, "1H"), (240, "4H")].into_iter().filter(|p| p.0 <= tf).last().unwrap_or((5, "5m"));
    let ps = pm as i64 * 60_000;
    let start_p = start / ps * ps;
    let stat = |path: &str, extra: &str| {
        stat_rows(pages(start_p, now, ps, 100).into_iter().rev().take(20)
            .map(|(s, e)| format!("rubik/stat/{path}?instId={sym}&period={period}&begin={}&end={}&limit=100{extra}", s - 1, e + 1)).collect())
    };
    // OKX returns all-zero rows for the bucket still being computed, for buckets it lost and past
    // its retention; they are missing data, not zero (agg forward-fills the gap)
    let oi = async {
        // rows: [ts, oi (contracts), oiCcy (base), oiUsd]
        let r = stat("contracts/open-interest-history", "").await?;
        anyhow::Ok(series(r.iter().map(|r| (ms(&r[0]), num(&r[2]))).filter(|(_, x)| *x > 0.0)))
    };
    let long_short = async {
        let r = stat("contracts/long-short-account-ratio-contract", "").await?;
        anyhow::Ok(series(r.iter().map(|r| (ms(&r[0]), num(&r[1]))).filter(|(_, x)| *x > 0.0)))
    };
    let taker = async {
        // unit=0: base coin; rows: [ts, sellVol, buyVol]
        let mut v: Vec<_> = stat("taker-volume-contract", "&unit=0").await?.iter().map(|r| (ms(&r[0]), num(&r[2]), num(&r[1])))
            .filter(|(t, b, s)| *t > 0 && b.is_finite() && s.is_finite() && (*b > 0.0 || *s > 0.0)).collect();
        v.sort_by_key(|x| x.0);
        v.dedup_by_key(|x| x.0);
        // the newest bucket is published empty / half-filled first
        while v.last().is_some_and(|x| x.1 == 0.0 || x.2 == 0.0) { v.pop(); }
        anyhow::Ok(v)
    };
    let funding = async {
        // rates are per interval; fundingTime - prevFundingTime from public/funding-rate is the
        // live connector's interval source. Keep one max interval (8h) before the window start.
        // ponytail: every rate is divided by the CURRENT interval
        let c = rows(vec![format!("public/funding-rate?instId={sym}")], 1).await?.first().cloned().unwrap_or_default();
        let (prev, at, next) = (ms(&c["prevFundingTime"]), ms(&c["fundingTime"]), ms(&c["nextFundingTime"]));
        let span = if prev > 0 && at > prev { at - prev } else { next - at };
        if span <= 0 { bail!("no funding interval: {c}") }
        let h = span as f64 / 3_600_000.0;
        let from = start - 8 * 3_600_000;
        // 400 rows per page, newest first; `after` pages to older rows. OKX serves about 3 months.
        // ponytail: at most 5 pages (2000 settlements)
        let (mut list, mut after) = (vec![], String::new());
        for _ in 0..5 {
            let page = get(&format!("public/funding-rate-history?instId={sym}&limit=400{after}")).await?.as_array().cloned().unwrap_or_default();
            let Some(oldest) = page.iter().map(|r| ms(&r["fundingTime"])).filter(|t| *t > 0).min() else { break };
            list.extend(page);
            if oldest <= from { break }
            after = format!("&after={oldest}");
        }
        anyhow::Ok(series(list.iter().map(|r| (ms(&r["fundingTime"]), num(&r["fundingRate"]) / h)).filter(|(t, _)| *t >= from)))
    };
    let (klines, oi, funding, long_short, taker) = tokio::join!(klines, oi, funding, long_short, taker);
    let opt = |name: &str, r: anyhow::Result<Vec<(i64, f64)>>| r.unwrap_or_else(|e| { eprintln!("[okx history {name}] {e:#}"); vec![] });
    Ok(History { klines: klines?, oi: opt("oi", oi), funding: opt("funding", funding), long_short: opt("ls", long_short),
        taker: taker.unwrap_or_else(|e| { eprintln!("[okx history taker] {e:#}"); vec![] }) })
}

/// rubik pages (newest first), 4 at a time with 2s between bursts; stops after the first burst
/// containing a page with no non-zero row (past the period's retention)
async fn stat_rows(paths: Vec<String>) -> anyhow::Result<Vec<Value>> {
    let has_data = |r: &Value| r.as_array().is_some_and(|a| a.len() > 1 && a[1..].iter().all(|x| num(x) > 0.0));
    let mut out = vec![];
    for (i, c) in paths.chunks(4).enumerate() {
        if i > 0 { tokio::time::sleep(Duration::from_millis(2100)).await; }
        let mut done = false;
        // up to 2 retries 2s apart: the per-endpoint budget is shared with the live poller and any
        // other fetch still inside its 2s window, so an occasional 429 is expected
        let retry = |p: String| async move {
            let mut r = get(&p).await;
            for _ in 0..2 {
                if r.is_ok() { break }
                tokio::time::sleep(Duration::from_millis(2100)).await;
                r = get(&p).await;
            }
            r
        };
        for p in futures_util::future::try_join_all(c.iter().cloned().map(retry)).await? {
            let p = p.as_array().cloned().unwrap_or_default();
            done |= !p.iter().any(has_data);
            out.extend(p);
        }
        if done { break }
    }
    Ok(out)
}

/// [start, end] split into inclusive windows of at most `n` steps of `step` ms
fn pages(start: i64, end: i64, step: i64, n: i64) -> Vec<(i64, i64)> {
    (start..=end).step_by((step * n) as usize).map(|s| (s, (s + step * (n - 1)).min(end))).collect()
}

/// `get` on all paths, `burst` at a time concurrently with 2s between bursts (OKX limits are per 2s),
/// concatenating their `data` arrays
async fn rows(paths: Vec<String>, burst: usize) -> anyhow::Result<Vec<Value>> {
    let mut out = vec![];
    for (i, c) in paths.chunks(burst).enumerate() {
        if i > 0 { tokio::time::sleep(Duration::from_millis(2100)).await; }
        for p in futures_util::future::try_join_all(c.iter().map(|p| get(p))).await? { out.extend(p.as_array().cloned().unwrap_or_default()); }
    }
    Ok(out)
}

/// ascending, deduplicated by time, NaN rows dropped
fn series(it: impl Iterator<Item = (i64, f64)>) -> Vec<(i64, f64)> {
    let mut v: Vec<_> = it.filter(|(t, x)| *t > 0 && x.is_finite()).collect();
    v.sort_by_key(|x| x.0);
    v.dedup_by_key(|x| x.0);
    v
}
