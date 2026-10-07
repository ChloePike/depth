//! Coinbase: public Advanced Trade WebSocket (wss://advanced-trade-ws.coinbase.com, no auth)
//! for spot and the US "perpetual-style" futures (Coinbase Derivatives, e.g. BIP-20DEC30-CDE).
//!
//! Quote mapping: USDT/USDC requests go to `BASE-USD`, the venue's liquid book (BTC-USDT exists
//! but trades ~0.5% of BTC-USD volume; Coinbase treats USDC books as USD). Other quotes use
//! `BASE-QUOTE` when listed, else `BASE-USD`. Perps are USD-quoted and ignore the requested quote.
//!
//! Perp: the CDE product for `base` is looked up via the public products list. Book/trade sizes
//! arrive in contracts and are converted to base units with `contract_size`. Funding is the rate per
//! `funding_interval` (3600s today), normalized to per hour. Coinbase International (INTX, BTC-PERP) is not used: its market-data WS rejects
//! unauthenticated subscriptions and the venue is paused; its REST quote is public if ever needed.
//!
//! Options: Coinbase has no public options market data.
use crate::ws::{self, Frame, Spec};
use crate::*;
use crate::ws::run_resync;
use serde_json::Value;
use std::time::Duration;
use tokio::task::JoinHandle;

const WS: &str = "wss://advanced-trade-ws.coinbase.com";
const REST: &str = "https://api.coinbase.com/api/v3/brokerage/market/products";

pub fn spawn(sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let e = Emit { ex: Exchange::Coinbase, market: sub.market, tx };
    let (base, mut quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    if quote == "USDT" || quote == "USDC" { quote = "USD".into(); }
    match sub.market {
        // ponytail: Coinbase publishes no margin borrow rates, so margin is spot data only
        Market::Spot | Market::Margin => vec![tokio::spawn(async move { stream(e, spot_id(&base, &quote).await, 1.0).await })],
        Market::Perp => vec![tokio::spawn(perp(e, base))],
        Market::Option => vec![], // no public options market data
    }
}

/// `BASE-QUOTE` when listed and trading, else `BASE-USD`; `quote` is already USDT/USDC -> USD mapped
async fn spot_id(base: &str, quote: &str) -> String {
    let id = format!("{base}-{quote}");
    // ponytail: a failed lookup (network) also falls back to USD
    let listed = ws::get_json(&format!("{REST}/{id}")).await.is_ok_and(|v| v["trading_disabled"] != true);
    if listed { id } else { format!("{base}-USD") }
}

async fn perp(e: Emit, base: String) {
    let p = loop {
        match find_perp(&base).await {
            Ok(Some(p)) => break p,
            Ok(None) => return eprintln!("[coinbase perp] no perpetual-style future for {base}"),
            Err(err) => eprintln!("[coinbase perp] product lookup: {err:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let id = p["product_id"].as_str().unwrap_or("").to_string();
    let size = num(&p["future_product_details"]["contract_size"]);
    let (e2, id2) = (e.clone(), id.clone());
    tokio::join!(
        stream(e, id, size),
        ws::poll("coinbase perp stats".into(), Duration::from_secs(10), move || poll_perp(e2.clone(), id2.clone())),
    );
}

/// CDE perps are listed as long-dated futures named "<Name> Perpetual"
async fn find_perp(base: &str) -> anyhow::Result<Option<Value>> {
    // ponytail: first page only (100 futures); paginate with `cursor` if perps stop being found
    let v = ws::get_json(&format!("{REST}?product_type=FUTURE")).await?;
    Ok(v["products"].as_array().into_iter().flatten().find(|p| {
        let f = &p["future_product_details"];
        f["contract_root_unit"] == base && f["display_name"].as_str().is_some_and(|n| n.ends_with("Perpetual"))
    }).cloned())
}

/// ponytail: no public mark or index price for CDE perps; mark = last trade price
async fn poll_perp(e: Emit, id: String) -> anyhow::Result<()> {
    let v = ws::get_json(&format!("{REST}/{id}")).await?;
    let f = &v["future_product_details"];
    // funding_time is the last funding; the next one is one interval later ("3600s")
    let every = f["funding_interval"].as_str().and_then(|s| s.trim_end_matches('s').parse::<i64>().ok()).unwrap_or(3600);
    let next = f["funding_time"].as_str().map(iso_ms).filter(|t| *t > 0).map(|t| t + every * 1000);
    e.send(&id, 0, Event::Mark { mark: num(&v["price"]), index: None, funding: opt_num(&f["funding_rate"]).map(|r| r * 3600.0 / every as f64), next_funding_ms: next });
    e.send(&id, 0, Event::OpenInterest { oi: num(&f["open_interest"]) * num(&f["contract_size"]), oi_usd: None });
    Ok(())
}

/// trades, level2 book and ticker BBO for one product; `mult` converts contracts to base units
async fn stream(e: Emit, id: String, mult: f64) {
    let spec = || {
        ["level2", "market_trades", "ticker", "heartbeats"].iter().fold(Spec::new("coinbase", WS), |s, ch| {
            s.sub(serde_json::json!({"type": "subscribe", "product_ids": [&id], "channel": ch}).to_string())
        })
    };
    run_resync(spec, || {
        let (e, id, mut seq) = (e.clone(), id.clone(), None);
        move |f| on_msg(&e, &id, mult, &mut seq, f)
    }).await
}

/// returns false on a sequence gap so the session restarts with a fresh book snapshot
fn on_msg(e: &Emit, sym: &str, mult: f64, seq: &mut Option<i64>, f: Frame) -> bool {
    let Frame::Text(t) = f else { return true };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return true };
    // sequence_num counts every message on the connection and restarts at 0 on reconnect
    if let Some(n) = v["sequence_num"].as_i64() {
        if n != 0 && seq.is_some_and(|p| n != p + 1) { return false; }
        *seq = Some(n);
    }
    let ts = v["timestamp"].as_str().map(iso_ms).unwrap_or(0);
    for ev in v["events"].as_array().into_iter().flatten() {
        match v["channel"].as_str().unwrap_or("") {
            "l2_data" => {
                let (mut bids, mut asks) = (vec![], vec![]);
                for u in ev["updates"].as_array().into_iter().flatten() {
                    let l = (num(&u["price_level"]), num(&u["new_quantity"]) * mult);
                    if u["side"] == "bid" { bids.push(l) } else { asks.push(l) }
                }
                e.send(sym, ts, Event::Book { snapshot: ev["type"] == "snapshot", bids, asks });
            }
            // the snapshot replays old trades; skip it so reconnects do not double count
            "market_trades" if ev["type"] == "update" => {
                for tr in ev["trades"].as_array().into_iter().flatten().rev() {
                    // side is the maker side (verified against the book), so the aggressor is the opposite
                    let side = if tr["side"] == "BUY" { Side::Sell } else { Side::Buy };
                    let t = tr["time"].as_str().map(iso_ms).unwrap_or(ts);
                    e.send(sym, t, Event::Trade { px: num(&tr["price"]), qty: num(&tr["size"]) * mult, side });
                }
            }
            "ticker" => {
                for k in ev["tickers"].as_array().into_iter().flatten() {
                    e.send(sym, ts, Event::Bbo { bid: num(&k["best_bid"]), bid_qty: num(&k["best_bid_quantity"]) * mult,
                        ask: num(&k["best_ask"]), ask_qty: num(&k["best_ask_quantity"]) * mult });
                }
            }
            _ => {}
        }
    }
    true
}

/// Recent REST history for the chart (one-minute candles); see `History`.
pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

/// `bars` candles of `tf` minutes; see `ex::history_tf`.
/// Candles: native granularity (1m/5m/15m/1h/4h), 300 per request (cap 350), newest first, empty
/// periods omitted (forward-filled here); no taker split.
/// ponytail: CDE perps have no public OI or funding history; funding is the single last settlement
/// from the product details, OI comes only from the live poll.
pub async fn history_tf(sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    use futures_util::{StreamExt, TryStreamExt};
    let gran = match tf { 1 => "ONE_MINUTE", 5 => "FIVE_MINUTE", 15 => "FIFTEEN_MINUTE", 60 => "ONE_HOUR", 240 => "FOUR_HOUR",
        _ => anyhow::bail!("coinbase has no {tf}m candles") };
    let (base, mut quote) = (sub.base.to_uppercase(), sub.quote.to_uppercase());
    if quote == "USDT" || quote == "USDC" { quote = "USD".into(); }
    let (id, mult, product) = match sub.market {
        Market::Spot | Market::Margin => (spot_id(&base, &quote).await, 1.0, Value::Null),
        Market::Perp => {
            let Some(p) = find_perp(&base).await? else { return Ok(History::default()) };
            let id = p["product_id"].as_str().unwrap_or("").to_string();
            (id, num(&p["future_product_details"]["contract_size"]), p)
        }
        Market::Option => return Ok(History::default()),
    };
    let (start, end) = window_tf(tf, bars);
    let step = tf as i64 * 60_000;
    const PAGE: i64 = 300; // candles per request, under the 350 cap
    let pages: Vec<i64> = (start..=end).step_by((PAGE * step) as usize).collect();
    // public limit is 10 req/s per IP
    let k: Vec<Vec<Kline>> = futures_util::stream::iter(pages).map(|s| {
        let url = format!("{REST}/{id}/candles?granularity={gran}&limit=350&start={}&end={}", s / 1000, (s + (PAGE - 1) * step) / 1000);
        async move {
            let v = ws::get_json(&url).await?;
            anyhow::Ok(v["candles"].as_array().into_iter().flatten().map(|c| Kline {
                t: c["start"].as_str().and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) * 1000,
                o: num(&c["open"]), h: num(&c["high"]), l: num(&c["low"]), c: num(&c["close"]), vol: num(&c["volume"]) * mult, buy: None,
            }).collect())
        }
    }).buffered(5).try_collect().await?;
    let mut h = History { klines: continuous_tf(k.concat(), end, bars, tf), ..Default::default() };
    let f = &product["future_product_details"];
    let every = f["funding_interval"].as_str().and_then(|s| s.trim_end_matches('s').parse::<i64>().ok()).unwrap_or(3600);
    if let (Some(t), Some(r)) = (f["funding_time"].as_str().map(iso_ms).filter(|t| *t > 0), opt_num(&f["funding_rate"])) {
        h.funding.push((t, r * 3600.0 / every as f64));
    }
    Ok(h)
}
