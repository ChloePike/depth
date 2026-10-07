//! MEXC: spot v3 WS (protobuf frames) and contract WS (JSON) for USDT perps.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::task::JoinHandle;

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Mexc, market: sub.market, tx };
    let (b, q) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    match sub.market {
        // ponytail: MEXC publishes no public margin borrow rates, so margin is spot data only
        Market::Spot | Market::Margin => {
            let sym = format!("{b}{q}");
            let params = [format!("spot@public.aggre.deals.v3.api.pb@100ms@{sym}"),
                          format!("spot@public.aggre.bookTicker.v3.api.pb@100ms@{sym}")];
            let spec = Spec::new(format!("mexc {:?}", sub.market), "wss://wbs-api.mexc.com/ws")
                .sub(json!({"method": "SUBSCRIPTION", "params": params}).to_string())
                .ping(Duration::from_secs(20), r#"{"method":"PING"}"#);
            let (e1, sym1) = (e.clone(), sym.clone());
            vec![tokio::spawn(ws::run(spec, move |f| on_spot(&e, &sym, f))), tokio::spawn(spot_depth(e1, sym1))]
        }
        // ponytail: no public liquidation or long/short feeds on MEXC contract API
        Market::Perp => vec![tokio::spawn(perp(e, format!("{b}_{q}")))],
        Market::Option => vec![], // MEXC lists no options
    }
}

// ---- perp (contract WS, JSON) ----

async fn perp(e: Emit, sym: String) {
    // contract size converts contracts to base units; funding_rate seeds the next settle time and the interval
    // ponytail: collectCycle read once at start; reconnects keep it, restart the task if MEXC changes it
    let (cs, mut next, hours) = loop {
        match perp_init(&sym).await {
            Ok(x) => break x,
            Err(err) => { eprintln!("[mexc perp] init: {err:#}"); tokio::time::sleep(Duration::from_secs(5)).await }
        }
    };
    let mut spec = Spec::new("mexc perp", "wss://contract.mexc.com/edge").ping(Duration::from_secs(15), r#"{"method":"ping"}"#);
    for (m, extra) in [("sub.deal", json!({})), ("sub.depth.full", json!({"limit": 50})), ("sub.ticker", json!({})), ("sub.funding.rate", json!({}))] {
        let mut p = extra;
        p["symbol"] = json!(sym);
        spec = spec.sub(json!({"method": m, "param": p, "gzip": false}).to_string());
    }
    ws::run(spec, move |f| {
        let Frame::Text(t) = f else { return };
        let Ok(v) = serde_json::from_str::<Value>(t) else { return };
        let d = &v["data"];
        let ts = v["ts"].as_i64().unwrap_or(0);
        let lv = |a: &Value| levels(a).into_iter().map(|(p, n)| (p, n * cs)).collect::<Vec<_>>();
        match v["channel"].as_str().unwrap_or("") {
            "push.deal" => for t in d.as_array().into_iter().flatten() {
                // T: 1 = buy aggressor, 2 = sell
                let side = if t["T"] == 2 { Side::Sell } else { Side::Buy };
                e.send(&sym, t["t"].as_i64().unwrap_or(ts), Event::Trade { px: num(&t["p"]), qty: num(&t["v"]) * cs, side });
            },
            "push.depth.full" => {
                let (bids, asks) = (lv(&d["bids"]), lv(&d["asks"]));
                // 50 per side is the UI depth (verified live: limit 50 returns 50/50)
                // ponytail: BBO is the top of the 50-level snapshot; ticker bid1/ask1 has no sizes
                if let (Some(b), Some(a)) = (bids.first(), asks.first()) {
                    e.send(&sym, ts, Event::Bbo { bid: b.0, bid_qty: b.1, ask: a.0, ask_qty: a.1 });
                }
                e.send(&sym, ts, Event::Book { snapshot: true, bids, asks });
            }
            "push.ticker" => {
                let ts = d["timestamp"].as_i64().unwrap_or(ts);
                e.send(&sym, ts, Event::Mark { mark: num(&d["fairPrice"]), index: opt_num(&d["indexPrice"]),
                    funding: opt_num(&d["fundingRate"]).map(|r| r / hours), next_funding_ms: Some(next).filter(|n| *n > 0) });
                e.send(&sym, ts, Event::OpenInterest { oi: num(&d["holdVol"]) * cs, oi_usd: None });
            }
            "push.funding.rate" => if let Some(n) = d["nextSettleTime"].as_i64() { next = n },
            _ => {}
        }
    }).await
}

/// (contract size, next settle ms, funding interval hours)
async fn perp_init(sym: &str) -> anyhow::Result<(f64, i64, f64)> {
    let d = ws::get_json(&format!("https://contract.mexc.com/api/v1/contract/detail?symbol={sym}")).await?;
    let cs = opt_num(&d["data"]["contractSize"]).ok_or_else(|| anyhow::anyhow!("no contractSize: {d}"))?;
    let f = ws::get_json(&format!("https://contract.mexc.com/api/v1/contract/funding_rate/{sym}")).await?;
    let hours = opt_num(&f["data"]["collectCycle"]).filter(|h| *h > 0.0).ok_or_else(|| anyhow::anyhow!("no collectCycle: {f}"))?;
    Ok((cs, f["data"]["nextSettleTime"].as_i64().unwrap_or(0), hours))
}

// ---- spot (v3 WS, protobuf) ----
// Field numbers from mexcdevelop/websocket-proto, verified against live frames:
// wrapper: 1 channel, 3 symbol, 6 sendTime, 313 aggre depth, 314 aggre deals, 315 aggre book ticker
// deals: 1 repeated item {1 price, 2 qty, 3 tradeType (1 buy, 2 sell), 4 time}
// book ticker: 1 bid, 2 bidQty, 3 ask, 4 askQty, 6 time
// 313 aggre depth: 1 asks, 2 bids (items {1 price, 2 qty}), 4 fromVersion, 5 toVersion (decimal strings)

/// Spot book deeper than 20: limit.depth only offers 5/10/20, so this is the diff stream
/// (aggre.depth@100ms) + REST /api/v3/depth?limit=1000 (weight 1, once per connection), per MEXC's
/// "maintain a local order book": drop pushes with toVersion <= lastUpdateId, the first applied must
/// have fromVersion <= lastUpdateId + 1, then each fromVersion == previous toVersion + 1; a gap resyncs.
// ponytail: levels outside the 1000-level snapshot that never change stay unknown (MEXC caveat); fine for a 50-level view
async fn spot_depth(e: Emit, sym: String) {
    let name = "mexc spot depth";
    loop {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(i64, i64, i64, Vec<(f64, f64)>, Vec<(f64, f64)>)>();
        let spec = Spec::new(name, "wss://wbs-api.mexc.com/ws")
            .sub(json!({"method": "SUBSCRIPTION", "params": [format!("spot@public.aggre.depth.v3.api.pb@100ms@{sym}")]}).to_string())
            .ping(Duration::from_secs(20), r#"{"method":"PING"}"#);
        let session = tokio::spawn(async move {
            let r = ws::once(&spec, move |f| {
                let Frame::Binary(buf) = f else { return true };
                let wrap = pb(buf);
                let Some(m) = wrap.iter().find_map(|(k, v)| match v { Pb::B(b) if *k == 313 => Some(pb(b)), _ => None }) else { return true };
                let side = |want: u64| m.iter().filter_map(|(k, v)| match v { Pb::B(l) if *k == want => { let l = pb(l); Some((fnum(&l, 1), fnum(&l, 2))) } _ => None }).collect();
                tx.send((int(&wrap, 6), fnum(&m, 4) as i64, fnum(&m, 5) as i64, side(2), side(1))).is_ok()
            }).await;
            if let Err(err) = r { eprintln!("[{name}] {err:#}"); }
        });
        // the snapshot must be newer than the first buffered push: wait for one first
        let Some(head) = rx.recv().await else { session.abort(); tokio::time::sleep(Duration::from_secs(2)).await; continue };
        let snap = match ws::get_json(&format!("https://api.mexc.com/api/v3/depth?symbol={sym}&limit=1000")).await {
            Ok(v) => v,
            Err(err) => { eprintln!("[{name}] snapshot: {err:#}"); session.abort(); tokio::time::sleep(Duration::from_secs(10)).await; continue }
        };
        let last = snap["lastUpdateId"].as_i64().unwrap_or(0);
        e.send(&sym, 0, Event::Book { snapshot: true, bids: levels(&snap["bids"]), asks: levels(&snap["asks"]) });
        let mut prev: Option<i64> = None;
        let mut head = Some(head);
        while let Some((ts, from, to, bids, asks)) = match head.take() { Some(h) => Some(h), None => rx.recv().await } {
            if to <= last { continue } // already in the snapshot
            let ok = match prev { None => from <= last + 1, Some(p) => from == p + 1 };
            if !ok { eprintln!("[{name}] sequence gap, resyncing"); break }
            prev = Some(to);
            e.send(&sym, ts, Event::Book { snapshot: false, bids, asks });
        }
        session.abort();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn on_spot(e: &Emit, sym: &str, f: Frame) {
    let Frame::Binary(buf) = f else { return }; // text frames are sub acks / PONG
    let wrap = pb(buf);
    let ts = int(&wrap, 6);
    for (n, v) in &wrap {
        let Pb::B(body) = v else { continue };
        let m = pb(body);
        match n {
            314 => for (k, it) in &m {
                let (1, Pb::B(it)) = (k, it) else { continue };
                let it = pb(it);
                let side = if int(&it, 3) == 2 { Side::Sell } else { Side::Buy };
                e.send(sym, int(&it, 4), Event::Trade { px: fnum(&it, 1), qty: fnum(&it, 2), side });
            },
            315 => e.send(sym, int(&m, 6).max(ts), Event::Bbo { bid: fnum(&m, 1), bid_qty: fnum(&m, 2), ask: fnum(&m, 3), ask_qty: fnum(&m, 4) }),
            _ => {}
        }
    }
}

/// protobuf wire value: varint or length-delimited bytes (fixed32/64 are skipped; MEXC does not use them here)
#[derive(Debug, PartialEq)]
enum Pb<'a> { V(u64), B(&'a [u8]) }

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut r = 0u64;
    for s in 0..10 {
        let (&c, rest) = b.split_first()?;
        *b = rest;
        r |= ((c & 0x7f) as u64) << (7 * s);
        if c < 0x80 { return Some(r) }
    }
    None
}

/// one message level: (field number, value); stops at the first malformed byte
fn pb(mut b: &[u8]) -> Vec<(u64, Pb<'_>)> {
    let mut out = vec![];
    while let Some(key) = varint(&mut b) {
        let v = match key & 7 {
            0 => match varint(&mut b) { Some(x) => Pb::V(x), None => break },
            2 => {
                let Some(n) = varint(&mut b).map(|n| n as usize).filter(|n| *n <= b.len()) else { break };
                let (x, rest) = b.split_at(n);
                b = rest;
                Pb::B(x)
            }
            1 if b.len() >= 8 => { b = &b[8..]; continue }
            5 if b.len() >= 4 => { b = &b[4..]; continue }
            _ => break,
        };
        out.push((key >> 3, v));
    }
    out
}

fn int(m: &[(u64, Pb)], n: u64) -> i64 {
    m.iter().find_map(|(k, v)| match v { Pb::V(x) if *k == n => Some(*x as i64), _ => None }).unwrap_or(0)
}

/// MEXC encodes decimals as protobuf strings
fn fnum(m: &[(u64, Pb)], n: u64) -> f64 {
    m.iter().find_map(|(k, v)| match v { Pb::B(s) if *k == n => std::str::from_utf8(s).ok()?.parse().ok(), _ => None }).unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pb_decodes_nested_and_varints() {
        // {1: "1.5", 2: {1: "2"}, 4: 300, 6: fixed64 skipped, 3: 7}
        let b = [0x0a, 3, b'1', b'.', b'5', 0x12, 3, 0x0a, 1, b'2', 0x20, 0xac, 0x02, 0x31, 0, 0, 0, 0, 0, 0, 0, 0, 0x18, 7];
        let m = pb(&b);
        assert_eq!(fnum(&m, 1), 1.5);
        assert_eq!(int(&m, 4), 300);
        assert_eq!(int(&m, 3), 7);
        let Some((_, Pb::B(inner))) = m.iter().find(|(k, _)| *k == 2) else { panic!() };
        assert_eq!(fnum(&pb(inner), 1), 2.0);
        // field numbers above 15 use multi-byte keys (315 << 3 | 2)
        assert_eq!(pb(&[0xda, 0x13, 0])[0], (315, Pb::B(&[])));
    }
}

/// Recent REST history for the chart (1m); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `history` at `tf`-minute resolution; see `ex::history_tf`.
// ponytail: MEXC has no public OI, long/short or taker-volume history (contract web endpoints are blocked);
// those series stay empty and fill from the live feed
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let (b, q) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    let perp = match sub.market { Market::Option => return Ok(History::default()), m => m == Market::Perp };
    let (spot_iv, fut_iv) = match tf { 1 => ("1m", "Min1"), 5 => ("5m", "Min5"), 15 => ("15m", "Min15"),
        60 => ("60m", "Min60"), 240 => ("4h", "Hour4"), _ => anyhow::bail!("mexc: unsupported timeframe {tf}m") };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    // spot: 500 rows per request (startTime/endTime ms, endTime exclusive); contract: 2000 rows (start/end seconds)
    let per = if perp { 2000 } else { 500 } * step;
    let pages = (0..).map(|k| end - k * per).take_while(|e| *e >= start).map(|e| ((e - per + step).max(start), e));
    let urls = pages.map(|(a, e)| if perp {
        format!("https://contract.mexc.com/api/v1/contract/kline/{b}_{q}?interval={fut_iv}&start={}&end={}", a / 1000, e / 1000)
    } else {
        format!("https://api.mexc.com/api/v3/klines?symbol={b}{q}&interval={spot_iv}&limit=500&startTime={a}&endTime={}", e + step)
    }).collect();
    let mut klines = vec![];
    let mut h = History::default();
    if perp {
        let sym = format!("{b}_{q}");
        // funding: interval first (sizes the page count), then 1000-row pages; runs alongside the candles
        let funding = async {
            let (cs, _, hours) = perp_init(&sym).await?;
            let n = ((end - start) as f64 / (hours * 3.6e6)) as usize + 2;
            let pages = all((1..=n.div_ceil(1000)).map(|p| format!(
                "https://contract.mexc.com/api/v1/contract/funding_rate/history?symbol={sym}&page_num={p}&page_size={}", n.min(1000))).collect()).await;
            anyhow::Ok((cs, hours, pages))
        };
        let (candles, init) = tokio::join!(all(urls), funding);
        let (cs, hours, fund) = init?;
        // columnar: {time: [s], open, high, low, close, vol (contracts)}
        for v in candles? {
            let d = &v["data"];
            for i in 0..d["time"].as_array().map_or(0, |a| a.len()) {
                klines.push(Kline { t: num(&d["time"][i]) as i64 * 1000, o: num(&d["open"][i]), h: num(&d["high"][i]),
                    l: num(&d["low"][i]), c: num(&d["close"][i]), vol: num(&d["vol"][i]) * cs, buy: None });
            }
        }
        // each record carries its own collectCycle (same field the live connector reads); fall back to the current one
        let fund = fund.unwrap_or_else(|e| { eprintln!("[mexc history] {e:#}"); vec![] });
        let from = start - (hours * 3.6e6) as i64; // keep the settlement just before the window
        h.funding = fund.iter().flat_map(|v| v["data"]["resultList"].as_array().cloned().unwrap_or_default()).map(|r| {
            let cyc = opt_num(&r["collectCycle"]).filter(|c| *c > 0.0).unwrap_or(hours);
            (r["settleTime"].as_i64().unwrap_or(0), num(&r["fundingRate"]) / cyc)
        }).filter(|(t, r)| *t >= from && r.is_finite()).collect();
        h.funding.sort_by_key(|x| x.0);
        h.funding.dedup_by_key(|x| x.0);
    } else {
        // rows: [t, o, h, l, c, base vol, close t, quote vol]
        for v in all(urls).await? {
            klines.extend(v.as_array().into_iter().flatten().map(|r| Kline {
                t: num(&r[0]) as i64, o: num(&r[1]), h: num(&r[2]), l: num(&r[3]), c: num(&r[4]), vol: num(&r[5]), buy: None }));
        }
    }
    klines.retain(|k| k.t >= start && k.c.is_finite());
    h.klines = continuous_tf(klines, end, bars, tf);
    Ok(h)
}

/// GETs in order, at most 4 in flight (MEXC market endpoints allow ~20 req/s)
async fn all(urls: Vec<String>) -> anyhow::Result<Vec<Value>> {
    use futures_util::{StreamExt, TryStreamExt};
    futures_util::stream::iter(urls).map(|u: String| async move { ws::get_json(&u).await }).buffered(4).try_collect().await
}
