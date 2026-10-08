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
    let deep = Deep::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::join!(main_stream(e, coin.clone(), subs, deep.clone(), rx), follow_group(coin, deep, tx));
}

async fn main_stream(e: Emit, coin: String, subs: &'static [&'static str], deep: Deep, rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
    let mut spec = Spec::new(format!("hyperliquid {:?}", e.market), WS)
        // server drops connections idle for 60s
        .ping(Duration::from_secs(30), r#"{"method":"ping"}"#);
    for s in subs {
        // the UI gateway's own channels: "l2" keys the coin as "c"; fastAssetCtxs covers every asset
        let sub = match *s { "l2" => json!({"type": "l2", "c": coin}), "fastAssetCtxs" => json!({"type": s}), _ => json!({"type": s, "coin": coin}) };
        spec = spec.sub(json!({"method": "subscribe", "subscription": sub}).to_string());
    }
    // on (re)connect: the grouped ladder in force; the new connection has nothing to unsubscribe
    let (d, c) = (deep.clone(), coin.clone());
    spec = spec.dynamic(move || {
        let mut d = d.lock().unwrap();
        (d.unsub_pending, d.bids, d.asks) = (0, vec![], vec![]);
        d.cur.map(|g| grouped_msg("subscribe", &c, g)).into_iter().collect()
    }, rx);
    let mut skip_trades = false;
    let mut book = L2::default();
    ws::run(spec, move |f| on_msg(&e, &coin, &mut skip_trades, &mut book, &deep, f)).await
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

/// Book grouping the UI shows, in price units: the grouped l2Book session follows it.
static GROUP: std::sync::OnceLock<tokio::sync::watch::Sender<f64>> = std::sync::OnceLock::new();
fn group() -> &'static tokio::sync::watch::Sender<f64> { GROUP.get_or_init(|| tokio::sync::watch::channel(0.0).0) }

/// Called by the book view whenever its grouping (bin width) changes; Hyperliquid then groups its
/// book server-side (l2Book nSigFigs / mantissa) to match, so the 20 levels per side it publishes
/// cover the rows on screen instead of $20 of exact ticks.
pub fn set_book_group(bin: f64) { group().send_if_modified(|g| if *g != bin { *g = bin; true } else { false }); }

/// (nSigFigs, mantissa, bucket width) of the coarsest server grouping not wider than `bin`;
/// None when that is the exact ladder (5 significant figures is Hyperliquid's tick).
fn grouping(bin: f64, px: f64) -> Option<(i32, Option<i32>, f64)> {
    if !(px > 0.0 && bin > 0.0) { return None; }
    let s5 = 10f64.powi(px.log10().floor() as i32 + 1 - 5);
    [(2, None, 1000.0), (3, None, 100.0), (4, None, 10.0), (5, Some(5), 5.0), (5, Some(2), 2.0)]
        .into_iter().map(|(n, m, k)| (n, m, k * s5)).find(|g| g.2 <= bin * (1.0 + 1e-9))
}

/// Shared between the exact book and the group follower: the exact mid (to pick the grouping),
/// the grouped l2Book in force, unsubscribes not yet acknowledged (l2Book data carries no
/// nSigFigs, so until then a push may still be the old grouping and is dropped), and the latest
/// grouped ladder.
#[derive(Default)]
struct DeepState { mid: f64, cur: Option<(i32, Option<i32>, f64)>, unsub_pending: u32, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)> }
type Deep = std::sync::Arc<std::sync::Mutex<DeepState>>;

fn grouped_msg(method: &str, coin: &str, (n, m, _): (i32, Option<i32>, f64)) -> String {
    let mut sub = json!({"type": "l2Book", "coin": coin, "nSigFigs": n});
    if let Some(m) = m { sub["mantissa"] = json!(m); }
    json!({"method": method, "subscription": sub}).to_string()
}

/// Keeps one grouped l2Book on the main connection matching the book view: on a change the old
/// one is unsubscribed, then the new one subscribed; none while the view shows exact ticks.
async fn follow_group(coin: String, deep: Deep, tx: tokio::sync::mpsc::UnboundedSender<String>) {
    let mut rx = group().subscribe();
    loop {
        {
            let mut d = deep.lock().unwrap();
            let want = grouping(*rx.borrow_and_update(), d.mid);
            if want != d.cur {
                if let Some(old) = d.cur { let _ = tx.send(grouped_msg("unsubscribe", &coin, old)); d.unsub_pending += 1; }
                if let Some(new) = want { let _ = tx.send(grouped_msg("subscribe", &coin, new)); }
                (d.cur, d.bids, d.asks) = (want, vec![], vec![]);
            }
        }
        // the exact mid may not be known yet (or change magnitude): look again every few seconds
        let _ = tokio::time::timeout(Duration::from_secs(3), rx.changed()).await;
    }
}

/// Extend one exact side (best first) with grouped levels of width `step` beyond it. Grouped bids
/// are floored to their bucket [p, p + step), asks ceiled to (p - step, p]; the bucket holding the
/// exact edge keeps only the size the exact levels do not already account for.
fn extend(exact: &mut Vec<(f64, f64)>, deep: &[(f64, f64)], step: f64, bid: bool) {
    let Some(&(edge, _)) = exact.last() else { return };
    let eps = edge.abs() * 1e-9;
    let mut more = vec![];
    for &(p, q) in deep {
        // a bucket wholly inside the exact range is already exact
        if (bid && p >= edge - eps) || (!bid && p <= edge + eps) { continue; }
        let (lo, hi) = if bid { (p, p + step) } else { (p - step, p) };
        let inside = if bid { edge < hi - eps } else { edge > lo + eps };
        let covered: f64 = if inside { exact.iter().filter(|(x, _)| if bid { *x >= lo - eps && *x < hi - eps } else { *x > lo + eps && *x <= hi + eps }).map(|l| l.1).sum() } else { 0.0 };
        if q - covered > 0.0 { more.push((p, q - covered)); }
    }
    exact.extend(more);
}

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

fn on_msg(e: &Emit, coin: &str, skip_trades: &mut bool, book: &mut L2, deep: &Deep, f: Frame) {
    let Frame::Text(t) = f else { return };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return };
    let d = &v["data"];
    match v["channel"].as_str().unwrap_or("") {
        // the first trades push after (re)subscribing is recent history; drop it so CVD is not double counted
        "subscriptionResponse" => {
            if d["subscription"]["type"] == "trades" { *skip_trades = true }
            if d["method"] == "unsubscribe" && d["subscription"]["type"] == "l2Book" {
                let mut s = deep.lock().unwrap();
                s.unsub_pending = s.unsub_pending.saturating_sub(1);
            }
        }
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
            let (mut bids, mut asks) = book.levels();
            if let (Some(b), Some(a)) = (bids.first(), asks.first()) {
                let mut d = deep.lock().unwrap();
                d.mid = (b.0 + a.0) / 2.0;
                if let Some((_, _, step)) = d.cur { extend(&mut bids, &d.bids, step, true); extend(&mut asks, &d.asks, step, false); }
            }
            e.send(coin, ts, Event::Book { snapshot: true, bids, asks });
        }
        "l2Book" => {
            let mut s = deep.lock().unwrap();
            if s.unsub_pending == 0 && s.cur.is_some() {
                let side = |i: usize| -> Vec<(f64, f64)> { d["levels"][i].as_array().map(|a| a.iter().map(lvl).collect()).unwrap_or_default() };
                (s.bids, s.asks) = (side(0), side(1));
            }
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

#[cfg(test)]
mod group_tests {
    use super::*;

    #[test]
    fn grouping_follows_the_book_bin() {
        let px = 82_300.0;
        assert_eq!(grouping(1.0, px), None);
        assert_eq!(grouping(2.0, px), Some((5, Some(2), 2.0)));
        assert_eq!(grouping(5.0, px), Some((5, Some(5), 5.0)));
        assert_eq!(grouping(10.0, px), Some((4, None, 10.0)));
        assert_eq!(grouping(50.0, px), Some((4, None, 10.0)));
        assert_eq!(grouping(0.5, 3_000.0), Some((5, Some(5), 0.5)));
    }

    #[test]
    fn grouped_ladder_extends_exact_book_without_double_count() {
        // exact bids 82395..82380 (sum 4); grouped bucket [82300, 82400) holds them plus 6 below the edge
        let mut b = vec![(82395.0, 1.0), (82390.0, 1.0), (82385.0, 1.0), (82380.0, 1.0)];
        extend(&mut b, &[(82300.0, 10.0), (82200.0, 7.0)], 100.0, true);
        assert_eq!(&b[4..], &[(82300.0, 6.0), (82200.0, 7.0)]);
        // asks: (82300, 82400] is all exact (skipped); (82400, 82500] holds 82410 (2) plus 3 beyond
        let mut a = vec![(82396.0, 1.0), (82410.0, 2.0)];
        extend(&mut a, &[(82400.0, 9.0), (82500.0, 5.0), (82600.0, 4.0)], 100.0, false);
        assert_eq!(&a[2..], &[(82500.0, 3.0), (82600.0, 4.0)]);
    }
}
