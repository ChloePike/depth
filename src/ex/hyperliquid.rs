//! Hyperliquid: perps (coin "BTC") and spot (coin id from spotMeta, e.g. "@142" = UBTC/USDC).
//! Everything settles in USDC: a USDT or USDC quote maps to the USDC market. Spot BTC is the
//! bridged "UBTC" token. One WS (wss://api-ui.hyperliquid.xyz/ws) carries every channel.
use crate::ws::{self, Frame, Spec};
use crate::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::task::JoinHandle;

// the UI gateway pushes book updates with lower latency than api.hyperliquid.xyz
const WS: &str = "wss://api-ui.hyperliquid.xyz/ws";
const INFO: &str = "https://api.hyperliquid.xyz/info";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Hyperliquid, market: sub.market, tx };
    match sub.market {
        // perps are USDC-margined and keyed by base only
        Market::Perp => {
            let coin = sub.base.to_uppercase();
            vec![tokio::spawn(stream(e, coin, &["trades", "l2", "bbo", "activeAssetCtx", "fastAssetCtxs"]))]
        }
        // no margin borrowing on Hyperliquid spot, so margin is plain spot data
        Market::Spot | Market::Margin => {
            let (base, quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
            vec![tokio::spawn(async move {
                let coin = loop {
                    match spot_coin(&base, &quote).await {
                        Ok(Some(c)) => break c,
                        Ok(None) => { eprintln!("[hyperliquid spot] no {base}/{quote} spot pair"); return }
                        Err(err) => { eprintln!("[hyperliquid spot] spotMeta: {err:#}"); tokio::time::sleep(Duration::from_secs(5)).await }
                    }
                };
                stream(e, coin, &["trades", "l2", "bbo"]).await
            })]
        }
        // no options on Hyperliquid
        Market::Option => vec![],
    }
}

/// Resolves the spot coin id ("@142" or "PURR/USDC"). Base matches the token name exactly, else
/// the Unit-bridged "U"+base (BTC -> UBTC). USDT maps to USDC.
// ponytail: name heuristic for bridged tokens; use an explicit alias table if more U-tokens collide
async fn spot_coin(base: &str, quote: &str) -> anyhow::Result<Option<String>> {
    let m = ws::post_json(INFO, &json!({"type": "spotMeta"})).await?;
    let quote = if quote == "USDT" { "USDC" } else { quote };
    let name = |i: &Value| m["tokens"].as_array().and_then(|t| t.iter().find(|t| t["index"] == *i)).and_then(|t| t["name"].as_str()).unwrap_or("");
    let find = |b: &str| m["universe"].as_array().into_iter().flatten()
        .find(|u| name(&u["tokens"][0]) == b && name(&u["tokens"][1]) == quote)
        .and_then(|u| u["name"].as_str().map(String::from));
    Ok(find(base).or_else(|| find(&format!("U{base}"))))
}

async fn stream(e: Emit, coin: String, subs: &'static [&'static str]) {
    let mut spec = Spec::new(format!("hyperliquid {:?}", e.market), WS)
        // server drops connections idle for 60s
        .ping(Duration::from_secs(30), r#"{"method":"ping"}"#);
    for s in subs {
        // the UI gateway's own channels: "l2" keys the coin as "c"; fastAssetCtxs covers every asset
        let sub = match *s { "l2" => json!({"type": "l2", "c": coin}), "fastAssetCtxs" => json!({"type": s}), _ => json!({"type": s, "coin": coin}) };
        spec = spec.sub(json!({"method": "subscribe", "subscription": sub}).to_string());
    }
    let mut skip_trades = false;
    let mut book = L2::default();
    ws::run(spec, move |f| on_msg(&e, &coin, &mut skip_trades, &mut book, f)).await
}

/// UI-gateway payloads: JSON, a base64 raw-deflate string of JSON, or that string wrapped as {"c": ...}.
fn payload(d: &Value) -> Option<Value> {
    match d {
        Value::String(b64) => inflate(b64),
        Value::Object(o) if o.len() == 1 && o.get("c").is_some_and(Value::is_string) => inflate(o["c"].as_str()?),
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn inflate(b64: &str) -> Option<Value> {
    use base64::Engine;
    use std::io::Read;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| eprintln!("[hyperliquid l2] base64: {e}")).ok()?;
    let mut out = String::new();
    flate2::read::DeflateDecoder::new(&raw[..]).read_to_string(&mut out).map_err(|e| eprintln!("[hyperliquid l2] inflate: {e}")).ok()?;
    serde_json::from_str(&out).map_err(|e| eprintln!("[hyperliquid l2] json: {e}")).ok()
}

/// next hourly funding time
fn next_hour(now: i64) -> i64 { (now / 3_600_000 + 1) * 3_600_000 }

fn lvl(l: &Value) -> (f64, f64) { (num(&l["px"]), num(&l["sz"])) }

/// Local top-20 book for the UI gateway's "l2" deltas, keyed by price in 1e-8 units.
#[derive(Default)]
struct L2 { bids: std::collections::BTreeMap<i64, f64>, asks: std::collections::BTreeMap<i64, f64> }

fn key(px: f64) -> i64 { (px * 1e8).round() as i64 }

impl L2 {
    /// Apply changed levels; levels the new prices traded through are gone (the deltas never list
    /// them: "r" stays empty), and each side keeps its 20 best.
    fn apply(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        for (side, lv) in [(&mut self.bids, bids), (&mut self.asks, asks)] {
            for &(p, q) in lv { if q > 0.0 { side.insert(key(p), q); } else { side.remove(&key(p)); } }
        }
        if let Some(b) = bids.iter().filter(|l| l.1 > 0.0).map(|l| key(l.0)).max() { self.asks.retain(|p, _| *p > b); }
        if let Some(a) = asks.iter().filter(|l| l.1 > 0.0).map(|l| key(l.0)).min() { self.bids.retain(|p, _| *p < a); }
        while self.bids.len() > 20 { self.bids.pop_first(); }
        while self.asks.len() > 20 { self.asks.pop_last(); }
    }
    fn levels(&self) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        (self.bids.iter().rev().map(|(p, q)| (*p as f64 / 1e8, *q)).collect(), self.asks.iter().map(|(p, q)| (*p as f64 / 1e8, *q)).collect())
    }
}

fn on_msg(e: &Emit, coin: &str, skip_trades: &mut bool, book: &mut L2, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    let d = &v["data"];
    match v["channel"].as_str().unwrap_or("") {
        // the first trades push after (re)subscribing is recent history; drop it so CVD is not double counted
        "subscriptionResponse" => if d["subscription"]["type"] == "trades" { *skip_trades = true },
        "trades" => {
            if std::mem::take(skip_trades) { return }
            // side is the aggressor: "B" buy, "A" sell.
            // ponytail: public trades carry no liquidation flag and there is no public liquidation feed
            for t in d.as_array().into_iter().flatten() {
                let side = if t["side"] == "B" { Side::Buy } else { Side::Sell };
                e.send(coin, t["time"].as_i64().unwrap_or(0), Event::Trade { px: num(&t["px"]), qty: num(&t["sz"]), side });
            }
        }
                // UI-gateway book (undocumented; ~6x the update rate of l2Book, same 20 levels): a
        // snapshot {"s": {levels}}, then deltas {"t", "l": [bids, asks] changed {"p","s"}}, kept
        // as a local top-20 book and re-emitted whole
        "l2" => {
            let Some(u) = payload(d) else { return };
            let (side_of, ts) = match u.get("s").filter(|s| s.is_object()) {
                Some(snap) => {
                    *book = L2::default();
                    let side = |i: usize| -> Vec<(f64, f64)> { snap["levels"][i].as_array().map(|a| a.iter().map(lvl).collect()).unwrap_or_default() };
                    ((side(0), side(1)), snap["time"].as_i64().unwrap_or(0))
                }
                None => {
                    let side = |i: usize| -> Vec<(f64, f64)> {
                        let changed = u["l"][i].as_array().into_iter().flatten().map(|l| (num(&l["p"]), num(&l["s"])));
                        // explicit removals, when present, as {"p": px} or a bare price
                        let removed = u["r"][i].as_array().into_iter().flatten().map(|r| (if r.is_object() { num(&r["p"]) } else { num(r) }, 0.0));
                        changed.chain(removed).collect()
                    };
                    ((side(0), side(1)), u["t"].as_i64().unwrap_or(0))
                }
            };
            book.apply(&side_of.0, &side_of.1);
            let (bids, asks) = book.levels();
            e.send(coin, ts, Event::Book { snapshot: true, bids, asks });
        }
        // mark (and mid) of every asset, faster than activeAssetCtx; funding / OI stay on that one
        "fastAssetCtxs" => if let Some(c) = payload(d).and_then(|u| u.get(coin).cloned()) {
            if let Some(mark) = opt_num(&c["markPx"]) { e.send(coin, 0, Event::Mark { mark, index: None, funding: None, next_funding_ms: None }); }
        },
        "bbo" => {
            let (b, a) = (&d["bbo"][0], &d["bbo"][1]);
            if b.is_null() || a.is_null() { return }
            let ((bid, bid_qty), (ask, ask_qty)) = (lvl(b), lvl(a));
            e.send(coin, d["time"].as_i64().unwrap_or(0), Event::Bbo { bid, bid_qty, ask, ask_qty });
        }
        "activeAssetCtx" => {
            let c = &d["ctx"];
            let mark = num(&c["markPx"]);
            // funding is the hourly rate (fraction); oraclePx is the index
            e.send(coin, 0, Event::Mark { mark, index: opt_num(&c["oraclePx"]), funding: opt_num(&c["funding"]), next_funding_ms: Some(next_hour(now_ms())) });
            let oi = num(&c["openInterest"]);
            e.send(coin, 0, Event::OpenInterest { oi, oi_usd: Some(oi * mark) });
        }
        _ => {}
    }
}

/// Recent REST history for the chart (one-minute candles); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `bars` candles of `tf` minutes; see `ex::history_tf`.
/// candleSnapshot (interval 1m/5m/15m/1h/4h, sizes in base, empty periods omitted -> forward-filled;
/// only the newest 5000 candles of each interval exist, so 1m reaches back ~3.5 days) and
/// fundingHistory (hourly fraction, stamped at settlement, 500 rows per request, paged by startTime).
/// No public OI, long/short or taker history.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    let iv = match tf { 1 => "1m", 5 => "5m", 15 => "15m", 60 => "1h", 240 => "4h", _ => anyhow::bail!("hyperliquid has no {tf}m candles") };
    let coin = match sub.market {
        Market::Perp => sub.base.to_uppercase(),
        Market::Spot | Market::Margin => match spot_coin(&sub.base.to_uppercase(), &sub.quote.to_uppercase()).await? {
            Some(c) => c,
            None => return Ok(History::default()),
        },
        Market::Option => return Ok(History::default()),
    };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    // 5000 candles per request
    let pages = (start..=end).step_by((5000 * step) as usize).map(|s| {
        let req = json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": iv, "startTime": s, "endTime": (s + 5000 * step - 1).min(end + step - 1)}});
        async move { ws::post_json(INFO, &req).await }
    });
    let funding = async {
        let mut out: Vec<(i64, f64)> = vec![];
        if sub.market != Market::Perp { return anyhow::Ok(out) }
        // pages run in sequence: each starts after the last row of the previous one
        let mut from = start - 3_600_000;
        loop {
            let v = ws::post_json(INFO, &json!({"type": "fundingHistory", "coin": coin, "startTime": from})).await?;
            let rows: Vec<(i64, f64)> = v.as_array().into_iter().flatten().map(|r| (r["time"].as_i64().unwrap_or(0), num(&r["fundingRate"]))).collect();
            let Some(&(last, _)) = rows.last() else { break };
            let full = rows.len() >= 500;
            out.extend(rows);
            if !full { break }
            from = last + 1;
        }
        Ok(out)
    };
    let (k, f) = tokio::join!(futures_util::future::try_join_all(pages), funding);
    let klines = k?.iter().flat_map(|v| v.as_array().cloned().unwrap_or_default()).map(|c| Kline {
        t: c["t"].as_i64().unwrap_or(0), o: num(&c["o"]), h: num(&c["h"]), l: num(&c["l"]), c: num(&c["c"]), vol: num(&c["v"]), buy: None,
    }).collect();
    Ok(History { klines: continuous_tf(klines, end, bars, tf), funding: f?, ..Default::default() })
}
