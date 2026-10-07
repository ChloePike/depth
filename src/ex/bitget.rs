//! Bitget: v2 public WS for spot and USDT-M perps, REST for long/short ratios and margin loan rates.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::task::JoinHandle;

const WS: &str = "wss://ws.bitget.com/v2/ws/public";
const REST: &str = "https://api.bitget.com";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Bitget, market: sub.market, tx };
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    let inst = match sub.market {
        Market::Spot | Market::Margin => "SPOT",
        Market::Perp => "USDT-FUTURES",
        Market::Option => return vec![], // Bitget lists no options
    };
    // spot: books50 = top-50 snapshot every 100ms (USDT-FUTURES rejects books50).
    // perp: books = 500-level snapshot then deltas chained by pseq == previous seq; a gap resubscribes.
    // books1 = top of book. Bitget v2 no longer sends a checksum field, so seq continuity is the only check.
    let perp = sub.market == Market::Perp;
    let mut chans = vec!["trade", if perp { "books" } else { "books50" }, "books1"];
    if perp { chans.push("ticker"); }
    let args: Vec<Value> = chans.iter().map(|c| json!({"instType": inst, "channel": c, "instId": sym})).collect();
    let (name, sub_msg) = (format!("bitget {:?}", sub.market), json!({"op": "subscribe", "args": args}).to_string());
    let spec = move || Spec::new(name.clone(), WS).sub(sub_msg.clone()).ping(Duration::from_secs(25), "ping");
    let (e1, sym1) = (e.clone(), sym.clone());
    let mut tasks = vec![tokio::spawn(async move {
        // ponytail: interval read once per task start; Bitget can switch a symbol 8h -> 4h, poll current-fund-rate if that matters
        let hours = if perp { fund_hours(&sym1).await } else { 1.0 };
        ws::run_resync(spec, move || {
            let (e, sym, mut seq) = (e1.clone(), sym1.clone(), None);
            move |f| on_msg(&e, &sym, hours, &mut seq, f)
        }).await
    })];
    match sub.market {
        Market::Margin => {
            let assets = [sub.base.to_uppercase(), sub.quote.to_uppercase()];
            tasks.push(tokio::spawn(ws::poll("bitget borrow".into(), Duration::from_secs(60), move || poll_borrow(e.clone(), sym.clone(), assets.clone()))));
        }
        Market::Perp => {
            // ponytail: no public liquidation feed on Bitget v2 (WS or REST); add if one appears
            tasks.push(tokio::spawn(ws::poll("bitget ls".into(), Duration::from_secs(60), move || poll_ls(e.clone(), sym.clone()))));
        }
        _ => {}
    }
    tasks
}

/// funding interval in hours for a USDT-FUTURES symbol; retries until the venue answers
async fn fund_hours(sym: &str) -> f64 {
    loop {
        match ws::get_json(&format!("{REST}/api/v2/mix/market/current-fund-rate?symbol={sym}&productType=USDT-FUTURES")).await {
            Ok(v) => if let Some(h) = opt_num(&v["data"][0]["fundingRateInterval"]).filter(|h| *h > 0.0) { return h }
                      else { eprintln!("[bitget] no fundingRateInterval: {v}") },
            Err(err) => eprintln!("[bitget] {err:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `hours` = funding interval; funding is emitted per hour. `seq` = last applied `books` seq
/// (None until the snapshot). Returns false on a `books` sequence gap so the session resyncs.
fn on_msg(e: &Emit, sym: &str, hours: f64, seq: &mut Option<i64>, f: Frame) -> bool {
    let Frame::Text(t) = f else { return true };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return true }; // "pong" and other non-JSON
    let action = v["action"].as_str().unwrap_or("");
    for d in v["data"].as_array().into_iter().flatten() {
        let ts = num(&d["ts"]) as i64;
        match v["arg"]["channel"].as_str().unwrap_or("") {
            // the subscribe snapshot replays recent history newest-first; only live updates are emitted
            "trade" if action == "update" => {
                let side = if d["side"] == "sell" { Side::Sell } else { Side::Buy };
                e.send(sym, ts, Event::Trade { px: num(&d["price"]), qty: num(&d["size"]), side });
            }
            "books50" => e.send(sym, ts, Event::Book { snapshot: true, bids: levels(&d["bids"]), asks: levels(&d["asks"]) }),
            "books" => {
                let snapshot = action == "snapshot";
                let (s, ps) = (d["seq"].as_i64(), d["pseq"].as_i64());
                if !snapshot && (seq.is_none() || ps != *seq) {
                    eprintln!("[bitget] {sym} books gap: pseq {ps:?} after seq {seq:?}");
                    return false;
                }
                *seq = s;
                e.send(sym, ts, Event::Book { snapshot, bids: levels(&d["bids"]), asks: levels(&d["asks"]) });
            }
            "books1" => {
                let (b, a) = (&d["bids"][0], &d["asks"][0]);
                e.send(sym, ts, Event::Bbo { bid: num(&b[0]), bid_qty: num(&b[1]), ask: num(&a[0]), ask_qty: num(&a[1]) });
            }
            "ticker" => {
                e.send(sym, ts, Event::Mark { mark: num(&d["markPrice"]), index: opt_num(&d["indexPrice"]),
                    funding: opt_num(&d["fundingRate"]).map(|r| r / hours), next_funding_ms: opt_num(&d["nextFundingTime"]).map(|x| x as i64) });
                // holdingAmount is open interest in base coin
                e.send(sym, ts, Event::OpenInterest { oi: num(&d["holdingAmount"]), oi_usd: None });
            }
            _ => {}
        }
    }
    true
}

async fn poll_ls(e: Emit, sym: String) -> anyhow::Result<()> {
    // long-short = all accounts; account-/position-long-short = elite (top) traders
    for (path, kind, ratio, long) in [
        ("long-short", LsKind::Accounts, "longShortRatio", "longRatio"),
        ("account-long-short", LsKind::TopAccounts, "longShortAccountRatio", "longAccountRatio"),
        ("position-long-short", LsKind::TopPositions, "longShortPositionRatio", "longPositionRatio"),
        ("taker-buy-sell", LsKind::TakerVolume, "", ""),
    ] {
        let v = ws::get_json(&format!("{REST}/api/v2/mix/market/{path}?symbol={sym}&productType=USDT-FUTURES&period=5m")).await?;
        let Some(r) = v["data"].as_array().and_then(|a| a.last()) else { continue }; // ascending by ts
        let ts = num(&r["ts"]) as i64;
        let ev = if kind == LsKind::TakerVolume {
            Event::LongShort { kind, ratio: num(&r["buyVolume"]) / num(&r["sellVolume"]), long_pct: None }
        } else {
            Event::LongShort { kind, ratio: num(&r[ratio]), long_pct: opt_num(&r[long]) }
        };
        e.send(&sym, ts, ev);
    }
    Ok(())
}

/// public UTA margin loan rates; the classic v2 margin interest endpoints need an API key
async fn poll_borrow(e: Emit, sym: String, assets: [String; 2]) -> anyhow::Result<()> {
    for a in assets {
        let v = ws::get_json(&format!("{REST}/api/v3/market/margin-loans?coin={a}")).await?;
        if let Some(apr) = opt_num(&v["data"]["annualInterest"]) {
            e.send(&sym, num(&v["requestTime"]) as i64, Event::BorrowRate { asset: a, apr });
        }
    }
    Ok(())
}

/// Recent REST history for the chart (1m); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `history` at `tf`-minute resolution; see `ex::history_tf`.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let sym = format!("{}{}", sub.base, sub.quote).to_uppercase();
    let perp = match sub.market { Market::Option => return Ok(History::default()), m => m == Market::Perp };
    let (spot_iv, mix_iv) = match tf { 1 => ("1min", "1m"), 5 => ("5min", "5m"), 15 => ("15min", "15m"),
        60 => ("1h", "1H"), 240 => ("4h", "4H"), _ => anyhow::bail!("bitget: unsupported timeframe {tf}m") };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    // up to 1000 candles per request ending at endTime (inclusive open time); mix also caps one request at 90 days
    let per = if perp { 1000.min(90 * 1440 / tf as i64) } else { 1000 };
    let urls = (0..).map(|k| end - k * per * step).take_while(|e| *e >= start).map(|e| if perp {
        format!("{REST}/api/v2/mix/market/candles?symbol={sym}&productType=USDT-FUTURES&granularity={mix_iv}&limit={per}&endTime={e}")
    } else {
        format!("{REST}/api/v2/spot/market/candles?symbol={sym}&granularity={spot_iv}&limit={per}&endTime={e}")
    }).collect();
    // ratio endpoints return a fixed ~30 rows and ignore time params: chart period when the venue has it,
    // for 1m the finest period whose 30 rows cover the window
    // ponytail: one resolution for the whole window; merge fine (recent) + coarse (older) if the chart needs both
    let want = if tf == 1 { bars } else { tf as usize };
    let period = [(5, "5m"), (15, "15m"), (30, "30m"), (60, "1h"), (120, "2h"), (240, "4h")].into_iter()
        .find(|(m, _)| *m * if tf == 1 { 30 } else { 1 } >= want).map_or("4h", |p| p.1);
    let ratio = |path: &str| get(format!("{REST}/api/v2/mix/market/{path}?symbol={sym}&productType=USDT-FUTURES&period={period}"));
    // funding: interval first (sizes the page count), then 100-row pages; runs alongside the candles
    let funding = async {
        let rate = get(format!("{REST}/api/v2/mix/market/current-fund-rate?symbol={sym}&productType=USDT-FUTURES")).await?;
        let hours = opt_num(&rate["data"][0]["fundingRateInterval"]).filter(|h| *h > 0.0)
            .ok_or_else(|| anyhow::anyhow!("no fundingRateInterval: {rate}"))?;
        let n = ((end - start) as f64 / (hours * 3.6e6)) as usize + 2;
        let pages = all((1..=n.div_ceil(100)).map(|p| format!(
            "{REST}/api/v2/mix/market/history-fund-rate?symbol={sym}&productType=USDT-FUTURES&pageSize=100&pageNo={p}")).collect()).await?;
        anyhow::Ok((hours, pages))
    };
    let (candles, fund, ls, taker) = tokio::join!(all(urls),
        async { if perp { funding.await } else { Ok((1.0, vec![])) } },
        async { if perp { ratio("long-short").await } else { Ok(Value::Null) } },
        async { if perp { ratio("taker-buy-sell").await } else { Ok(Value::Null) } });
    // rows: [ts, o, h, l, c, base vol, ...]; perp sizes are already in base coin
    let klines: Vec<Kline> = candles?.iter().flat_map(|v| v["data"].as_array().cloned().unwrap_or_default()).map(|r| Kline {
        t: num(&r[0]) as i64, o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[5]), buy: None,
    }).filter(|k| k.t >= start && k.c.is_finite()).collect();
    let mut h = History { klines: continuous_tf(klines, end, bars, tf), ..Default::default() };
    if !perp { return Ok(h) }
    let soft = |r: anyhow::Result<Value>| r.unwrap_or_else(|e| { eprintln!("[bitget history] {e:#}"); Value::Null });
    let rows = |v: &Value| v["data"].as_array().cloned().unwrap_or_default();
    // ponytail: no public OI history on Bitget v2/v3 (only current open-interest); oi stays empty, live ticker fills it
    // same interval source as the live connector; past settlements are scaled by today's interval
    match fund {
        Ok((hours, pages)) => {
            let from = start - (hours * 3.6e6) as i64; // keep the settlement just before the window
            h.funding = pages.iter().flat_map(rows).map(|r| (num(&r["fundingTime"]) as i64, num(&r["fundingRate"]) / hours))
                .filter(|(t, r)| *t >= from && r.is_finite()).collect();
            h.funding.sort_by_key(|x| x.0);
            h.funding.dedup_by_key(|x| x.0);
        }
        Err(e) => eprintln!("[bitget history] {e:#}"),
    }
    h.long_short = rows(&soft(ls)).iter().map(|r| (num(&r["ts"]) as i64, num(&r["longShortRatio"])))
        .filter(|x| x.0 >= start && x.1.is_finite()).collect();
    h.long_short.sort_by_key(|x| x.0);
    // taker volumes are in base coin
    h.taker = rows(&soft(taker)).iter().map(|r| (num(&r["ts"]) as i64, num(&r["buyVolume"]), num(&r["sellVolume"])))
        .filter(|x| x.0 >= start && x.1.is_finite() && x.2.is_finite()).collect();
    h.taker.sort_by_key(|x| x.0);
    Ok(h)
}

/// owned-URL GET so futures can be joined
async fn get(url: String) -> anyhow::Result<Value> { ws::get_json(&url).await }

/// GETs in order, at most 4 in flight (Bitget market endpoints allow 20 req/s per IP)
async fn all(urls: Vec<String>) -> anyhow::Result<Vec<Value>> {
    use futures_util::{StreamExt, TryStreamExt};
    futures_util::stream::iter(urls).map(get).buffered(4).try_collect().await
}
