//! Deribit private API: option order entry only, on the coin-settled BTC / ETH options the chain shows (no
//! perp / futures orders, no *_USDC linear options). JSON-RPC 2.0 over REST GET, every call signed on its own
//! with deri-hmac-sha256 (no token kept). Keys are the API key's Client ID / Client secret. Option amounts are
//! base coin; prices are the venue quote (BTC / ETH per coin of underlying), never USD.
use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub fn url() -> &'static str { if testnet() { "https://test.deribit.com" } else { "https://www.deribit.com" } }

/// hex HMAC-SHA256(secret, ts \n nonce \n data). REST: data = "GET\n{uri}\n\n" (method, path + query, empty
/// body); WebSocket client_signature: data = the auth request's `data` field.
pub fn sign(secret: &str, ts: i64, nonce: &str, data: &str) -> String { hmac_hex(secret, &format!("{ts}\n{nonce}\n{data}")) }

fn nonce() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{:x}{:04x}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// `/api/v2/{method}?{query}`. The signature covers the URI as sent, so values must need no percent-encoding
/// (instrument names, order ids, numbers, flags); anything else is refused rather than signed wrong.
fn uri(method: &str, params: &[(&str, String)]) -> Result<String> {
    if let Some((k, v)) = params.iter().find(|(_, v)| !v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))) {
        bail!("deribit {method}: {k}={v:?} cannot be sent");
    }
    let q = params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    Ok(if q.is_empty() { format!("/api/v2/{method}") } else { format!("/api/v2/{method}?{q}") })
}

/// One JSON-RPC call; signed when `k` is given (private methods). Never retried: buy / sell / cancel are
/// matching-engine calls.
async fn deribit(k: Option<&Keys>, method: &str, params: &[(&str, String)]) -> Result<Value> {
    let uri = uri(method, params)?;
    let url = format!("{}{uri}", url());
    ws::check_paused(&url)?;
    let mut req = ws::http().get(&url);
    if let Some(k) = k {
        let (ts, nonce) = (crate::now_ms(), nonce());
        let sig = sign(&k.secret, ts, &nonce, &format!("GET\n{uri}\n\n"));
        req = req.header("Authorization", format!("deri-hmac-sha256 id={},ts={ts},sig={sig},nonce={nonce}", k.key));
    }
    let r = req.send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    // status only: note_limit also matches Binance's "-1003" in a body, which a Deribit order id can contain
    ws::note_limit(&url, st, None, "");
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("deribit {method}: HTTP {st}: {}", body.chars().take(200).collect::<String>()))?;
    rpc_result(&url, method, v)
}

/// The `result` of a JSON-RPC reply, or its error with the venue's own text. too_many_requests (10028) and
/// too_many_concurrent_requests (10066) pause the host: the HTTP status of those answers is undocumented, so the
/// JSON code decides.
fn rpc_result(url: &str, method: &str, mut v: Value) -> Result<Value> {
    let e = &v["error"];
    if e.is_object() {
        let code = e["code"].as_i64().unwrap_or(0);
        if matches!(code, 10028 | 10066) { ws::note_limit(url, 429, None, ""); }
        let reason = e["data"]["reason"].as_str().map(|r| format!(": {r}")).unwrap_or_default();
        bail!("deribit {method}: {} ({code}){reason}", e["message"].as_str().unwrap_or("error"));
    }
    Ok(v["result"].take())
}

/// Price steps per option (tick from `above` on, above), stored by `option_rules`, which every order path runs
/// first in this process (ticket orders through the rules cache, protection buys through the plan).
static STEPS: Mutex<BTreeMap<String, (f64, f64)>> = Mutex::new(BTreeMap::new());

/// The tick that applies at `px`: the coarser one from `above` on (hi 0 = one tick for every price).
/// ponytail: one step only; every live BTC / ETH option has exactly one (0.0005 from 0.005, 2026-10-26).
pub fn tick_at(tick: f64, (hi, above): (f64, f64), px: f64) -> f64 { if hi > 0.0 && px >= above { hi } else { tick } }

pub(super) fn tick_for(symbol: &str, tick: f64, px: f64) -> f64 { tick_at(tick, STEPS.lock().unwrap().get(symbol).copied().unwrap_or_default(), px) }

/// Rules and price step from a get_instrument result: the amount step is the minimum (min_trade_amount, base coin).
fn rules_of(i: &Value) -> Result<(Rules, (f64, f64))> {
    let name = i["instrument_name"].as_str().unwrap_or("?");
    if i["kind"] != "option" || i["is_active"] == false { bail!("deribit: {name} is not an active option"); }
    if i["quote_currency"] != i["base_currency"] { bail!("deribit: {name}: only the coin-settled BTC / ETH options trade here"); }
    let min = n(&i["min_trade_amount"]);
    let r = Rules { tick: n(&i["tick_size"]), step: min, min_qty: min, min_notional: 0.0 };
    if !(r.tick > 0.0 && r.step > 0.0) { bail!("deribit: {name}: no trading rules"); }
    let s = &i["tick_size_steps"][0];
    Ok((r, (z(&s["tick_size"]), z(&s["above_price"]))))
}

/// public/get_instrument: 500 credits (get_instruments costs 10,000, so one instrument at a time).
pub async fn option_rules(symbol: &str) -> Result<Rules> {
    let (r, steps) = rules_of(&deribit(None, "public/get_instrument", &[("instrument_name", symbol.into())]).await?)?;
    STEPS.lock().unwrap().insert(symbol.into(), steps);
    Ok(r)
}

/// Protection priced on the venue's own ask side (coin per coin), converted to USD at the book's index.
pub async fn option_plan(symbol: &str, qty: f64) -> Result<OptBuy> {
    let r = option_rules(symbol).await?;
    let b = deribit(None, "public/get_order_book", &[("instrument_name", symbol.into()), ("depth", "20".into())]).await?;
    plan_of(symbol, qty, &r, &b)
}

fn plan_of(symbol: &str, qty: f64, r: &Rules, b: &Value) -> Result<OptBuy> {
    let asks: Vec<(f64, f64)> = crate::levels(&b["asks"]).into_iter().filter(|(p, q)| *p > 0.0 && *q > 0.0).collect();
    let qty = ceil_step(qty, r.step, r.min_qty);
    let (limit, fill, avg) = sweep(&asks, qty).ok_or_else(|| anyhow!("{symbol}: no asks"))?;
    // the venue refuses prices outside its band (price_too_high 10007)
    let (lo, hi) = (z(&b["min_price"]), z(&b["max_price"]));
    if hi > 0.0 && !(lo..=hi).contains(&limit) { bail!("{symbol}: the asks reach {limit}, outside the venue's price band {lo} to {hi}"); }
    let usd = z(&b["index_price"]);
    if !(usd > 0.0) { bail!("{symbol}: the book carries no index price"); }
    Ok(OptBuy { ex: Exchange::Deribit, symbol: symbol.into(), qty, limit, tick: r.tick, step: r.step, fill, avg, cost: fill * avg, usd })
}

/// private/buy or private/sell params of one limit order: amount in base coin, price in the venue quote.
pub fn order_params(symbol: &str, side: Side, q: &str, px: &str, tif: Tif, reduce_only: bool) -> (&'static str, Vec<(&'static str, String)>) {
    let mut p = vec![("instrument_name", symbol.to_string()), ("amount", q.into()), ("type", "limit".into()), ("price", px.into()),
        ("time_in_force", match tif { Tif::Ioc => "immediate_or_cancel", Tif::Fok => "fill_or_kill", Tif::Gtc | Tif::PostOnly => "good_til_cancelled" }.into()),
        ("label", format!("t1opt{}", crate::now_ms()))];
    // without reject_post_only Deribit re-prices a crossing post-only order to just inside the spread
    if tif == Tif::PostOnly { p.push(("post_only", "true".into())); p.push(("reject_post_only", "true".into())); }
    if reduce_only { p.push(("reduce_only", "true".into())); }
    (if side == Side::Buy { "private/buy" } else { "private/sell" }, p)
}

pub async fn option_order(k: &Keys, symbol: &str, side: Side, q: &str, px: &str, tif: Tif, reduce_only: bool) -> Result<String> {
    let (method, p) = order_params(symbol, side, q, px, tif, reduce_only);
    let v = deribit(Some(k), method, &p).await?;
    let o = &v["order"];
    if o["order_state"] == "rejected" { bail!("deribit {method}: order rejected ({})", o["reject_reason"].as_str().unwrap_or("no reason given")); }
    o["order_id"].as_str().map(String::from).ok_or_else(|| anyhow!("deribit {method}: no order id in the reply"))
}

pub async fn option_cancel(k: &Keys, id: &str) -> Result<()> {
    deribit(Some(k), "private/cancel", &[("order_id", id.into())]).await?;
    Ok(())
}

/// One option position row (get_positions and user.changes share the shape); size 0 = closed. The size's sign
/// is undocumented: its magnitude and `direction` are used.
pub(super) fn opt_pos(p: &Value) -> Option<OptPos> {
    let symbol = p["instrument_name"].as_str()?;
    let (base, expiry, strike, call) = opt_parts(symbol)?;
    // inverse options price in coin: USD at the row's index; linear ones are USDC already
    let fx = if option_quote(Exchange::Deribit, symbol) == "USDC" { 1.0 } else { z(&p["index_price"]) };
    Some(OptPos { ex: Exchange::Deribit, symbol: symbol.into(), base, expiry, strike, call,
        side: if p["direction"] == "sell" { Side::Sell } else { Side::Buy }, qty: z(&p["size"]).abs(),
        avg: crate::opt_num(&p["average_price_usd"]).unwrap_or(z(&p["average_price"]) * fx), mark: z(&p["mark_price"]) * fx,
        upnl: crate::opt_num(&p["floating_profit_loss_usd"]).unwrap_or(z(&p["floating_profit_loss"]) * fx),
        delta: z(&p["delta"]), gamma: z(&p["gamma"]), vega: z(&p["vega"]), theta: z(&p["theta"]) })
}

pub async fn option_positions(k: &Keys) -> Result<Vec<OptPos>> {
    let v = deribit(Some(k), "private/get_positions", &[("currency", "any".into()), ("kind", "option".into())]).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(opt_pos).filter(|p| p.qty > 0.0).collect())
}

/// One order row (get_open_orders and user.changes share the shape); price in the venue quote.
pub(super) fn order(o: &Value) -> OpenOrder {
    let s = |k: &str| o[k].as_str().unwrap_or_default().to_string();
    OpenOrder { ex: Exchange::Deribit, symbol: s("instrument_name"), id: s("order_id"), side: if o["direction"] == "sell" { Side::Sell } else { Side::Buy },
        price: z(&o["price"]), qty: z(&o["amount"]), filled: z(&o["filled_amount"]), kind: s("order_type"),
        tif: format!("{}{}", s("time_in_force"), if o["post_only"] == true { " post" } else { "" }), reduce_only: o["reduce_only"] == true,
        ts: z(&o["creation_timestamp"]) as i64, pos: None, client_id: opt_id(&o["label"]), trigger: None }
}

pub async fn option_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let v = deribit(Some(k), "private/get_open_orders", &[("kind", "option".into())]).await?;
    Ok(v.as_array().into_iter().flatten().map(order).collect())
}

/// Account totals in USD from per-currency rows (get_account_summaries and user.portfolio pushes share the
/// fields). Cross-margin accounts carry the venue's own USD totals. Otherwise each currency is valued at its
/// index (`usd`; None = not known yet, so no total) and mm_rate is the worst currency's, since a segregated
/// currency is liquidated on its own.
/// ponytail: segregated accounts count BTC, ETH and USD stablecoins; other collateral counts 0 until its index is read
pub(super) fn fold<'a>(rows: impl IntoIterator<Item = &'a Value>, usd: impl Fn(&str) -> Option<f64>) -> Option<Balance> {
    let rows: Vec<&Value> = rows.into_iter().collect();
    if let Some(r) = rows.iter().find(|r| crate::opt_num(&r["total_equity_usd"]).is_some()) {
        let (mb, mm) = (z(&r["total_margin_balance_usd"]), crate::opt_num(&r["total_maintenance_margin_usd"]));
        return Some(Balance { equity: z(&r["total_equity_usd"]), available: mb - z(&r["total_initial_margin_usd"]), uni_mmr: None,
            mm_rate: mm.filter(|_| mb > 0.0).map(|m| m / mb), maint_margin: mm, adj_equity: Some(mb) });
    }
    let (mut b, mut mm, mut mb, mut worst) = (Balance::default(), 0.0, 0.0, None::<f64>);
    for r in rows {
        let c = r["currency"].as_str().unwrap_or_default();
        if z(&r["equity"]) == 0.0 { continue; }
        let x = if is_usd(c) { 1.0 } else if matches!(c, "BTC" | "ETH") { usd(c)? } else { 0.0 };
        b.equity += z(&r["equity"]) * x;
        b.available += z(&r["available_funds"]) * x;
        mm += z(&r["maintenance_margin"]) * x;
        mb += z(&r["margin_balance"]) * x;
        let m = z(&r["margin_balance"]);
        if m > 0.0 { worst = Some(worst.unwrap_or(0.0).max(z(&r["maintenance_margin"]) / m)); }
    }
    Some(Balance { maint_margin: Some(mm), adj_equity: Some(mb), mm_rate: worst, ..b })
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let v = deribit(Some(k), "private/get_account_summaries", &[]).await?;
    let rows = v["summaries"].as_array().cloned().unwrap_or_default();
    if let Some(b) = fold(&rows, |_| None) { return Ok(b); }
    // segregated margin with BTC / ETH: their indexes (only on a snapshot, never on a timer)
    let mut px = std::collections::HashMap::new();
    for c in ["BTC", "ETH"].into_iter().filter(|c| rows.iter().any(|r| r["currency"] == *c && z(&r["equity"]) != 0.0)) {
        let i = deribit(None, "public/get_index_price", &[("index_name", format!("{}_usd", c.to_lowercase()))]).await?;
        px.insert(c, z(&i["index_price"]));
    }
    fold(&rows, |c| px.get(c).copied().filter(|x| *x > 0.0)).ok_or_else(|| anyhow!("deribit: no index price for the account's coins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_the_docs_vector() {
        // Deribit's documented key (AMANDASECRECT, ts 1576074319000, nonce 1iqt2wls), recomputed with Python's hmac
        assert_eq!(sign("AMANDASECRECT", 1576074319000, "1iqt2wls", ""), "56590594f97921b09b18f166befe0d1319b198bbcdad7ca73382de2f88fe9aa1");
        let u = uri("private/get_account_summary", &[("currency", "BTC".into())]).unwrap();
        assert_eq!(u, "/api/v2/private/get_account_summary?currency=BTC");
        assert_eq!(sign("AMANDASECRECT", 1576074319000, "1iqt2wls", &format!("GET\n{u}\n\n")), "9bfbc51a2bc372d72cc396cf1a213dc78d42eb74cb7dc272351833ad0de276ab");
        assert_eq!(uri("private/get_account_summaries", &[]).unwrap(), "/api/v2/private/get_account_summaries");
        // a value that would be percent-encoded on the way is refused, not signed wrong
        assert!(uri("private/buy", &[("label", "a b".into())]).is_err() && uri("private/buy", &[("x", "1&y=2".into())]).is_err());
        assert!(nonce() != nonce());
    }

    #[test]
    fn order_params_are_coin_amount_and_venue_price() {
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let (m, p) = order_params("BTC-6NOV26-100000-P", Side::Buy, "0.3", "0.0125", Tif::PostOnly, true);
        assert_eq!(m, "private/buy");
        assert_eq!((get(&p, "amount").as_deref(), get(&p, "price").as_deref(), get(&p, "type").as_deref()), (Some("0.3"), Some("0.0125"), Some("limit")));
        assert_eq!(get(&p, "time_in_force").as_deref(), Some("good_til_cancelled"));
        assert_eq!((get(&p, "post_only").as_deref(), get(&p, "reject_post_only").as_deref(), get(&p, "reduce_only").as_deref()), (Some("true"), Some("true"), Some("true")));
        assert!(get(&p, "label").is_some_and(|l| l.starts_with("t1opt") && l.len() <= 64));
        assert!(get(&p, "advanced").is_none(), "native coin pricing, never advanced=usd");
        let (m, p) = order_params("ETH-6NOV26-2000-C", Side::Sell, "1", "0.0031", Tif::Ioc, false);
        assert_eq!((m, get(&p, "time_in_force").as_deref(), get(&p, "post_only"), get(&p, "reduce_only")), ("private/sell", Some("immediate_or_cancel"), None, None));
        assert_eq!(get(&order_params("X", Side::Buy, "1", "1", Tif::Fok, false).1, "time_in_force").as_deref(), Some("fill_or_kill"));
        // every param goes through the signed URI as is
        assert!(uri(m, &p).is_ok());
    }

    #[test]
    fn rules_and_price_steps() {
        let i = json!({"instrument_name": "BTC-6NOV26-100000-P", "kind": "option", "is_active": true, "base_currency": "BTC", "quote_currency": "BTC",
            "tick_size": 0.0001, "tick_size_steps": [{"tick_size": 0.0005, "above_price": 0.005}], "min_trade_amount": 0.1, "contract_size": 1.0});
        let (r, steps) = rules_of(&i).unwrap();
        assert_eq!((r.tick, r.step, r.min_qty, r.min_notional, steps), (0.0001, 0.1, 0.1, 0.0, (0.0005, 0.005)));
        // below the step the fine tick, from it on the coarse one
        assert_eq!(fmt_step(0.0123, tick_at(r.tick, steps, 0.0123), false), "0.0125");
        assert_eq!(fmt_step(0.0031, tick_at(r.tick, steps, 0.0031), false), "0.0031");
        assert_eq!(tick_at(r.tick, steps, 0.005), 0.0005);
        assert_eq!(tick_at(r.tick, (0.0, 0.0), 0.5), 0.0001, "no step: one tick");
        // ETH: min 1 ETH, no step listed
        let e = json!({"instrument_name": "ETH-6NOV26-2000-P", "kind": "option", "is_active": true, "base_currency": "ETH", "quote_currency": "ETH", "tick_size": 0.0001, "min_trade_amount": 1});
        assert_eq!(rules_of(&e).unwrap().1, (0.0, 0.0));
        // linear USDC options and anything not an active option are refused
        let lin = json!({"instrument_name": "BTC_USDC-26OCT26-72000-C", "kind": "option", "is_active": true, "base_currency": "BTC", "quote_currency": "USDC", "tick_size": 5.0, "min_trade_amount": 0.01});
        assert!(rules_of(&lin).is_err());
        assert!(rules_of(&json!({"instrument_name": "BTC-PERPETUAL", "kind": "future", "base_currency": "BTC", "quote_currency": "USD", "tick_size": 0.5, "min_trade_amount": 10})).is_err());
        assert!(rules_of(&json!({"kind": "option", "is_active": false, "base_currency": "BTC", "quote_currency": "BTC", "tick_size": 0.0001, "min_trade_amount": 0.1})).is_err());
    }

    #[test]
    fn plan_sweeps_coin_asks_and_carries_the_index() {
        let b = json!({"asks": [[0.25, 6.5], [0.2505, 7.5], [0.251, 49.4]], "bids": [[0.2465, 49.5]], "index_price": 80024.25, "min_price": 0.2075, "max_price": 0.2965});
        let r = Rules { tick: 0.0001, step: 0.1, min_qty: 0.1, min_notional: 0.0 };
        let p = plan_of("BTC-6NOV26-100000-P", 10.0, &r, &b).unwrap();
        assert_eq!((p.limit, p.qty, p.fill, p.usd), (0.2505, 10.0, 10.0, 80024.25));
        assert!((p.avg - (6.5 * 0.25 + 3.5 * 0.2505) / 10.0).abs() < 1e-12);
        assert!((p.cost - (6.5 * 0.25 + 3.5 * 0.2505)).abs() < 1e-12, "cost stays in BTC");
        assert_eq!(option_quote(p.ex, &p.symbol), "BTC");
        // size rounded up to the venue step; past the asks it is a partial
        let p = plan_of("BTC-6NOV26-100000-P", 70.0, &r, &b).unwrap();
        assert_eq!((p.limit, p.fill), (0.251, 63.4));
        assert_eq!(plan_of("BTC-6NOV26-100000-P", 0.04, &r, &b).unwrap().qty, 0.1);
        // outside the venue's band, no index, no asks: refused
        assert!(plan_of("X", 1.0, &r, &json!({"asks": [[0.3, 1.0]], "index_price": 80000.0, "min_price": 0.2, "max_price": 0.29})).is_err());
        assert!(plan_of("X", 1.0, &r, &json!({"asks": [[0.25, 1.0]]})).is_err());
        assert!(plan_of("X", 1.0, &r, &json!({"asks": [], "index_price": 80000.0})).is_err());
    }

    #[test]
    fn positions_orders_and_balances() {
        let p = json!({"instrument_name": "BTC-6NOV26-60000-P", "kind": "option", "direction": "sell", "size": -1.0, "average_price": 0.01, "average_price_usd": 801.5,
            "mark_price": 0.012, "index_price": 80000.0, "floating_profit_loss": -0.002, "floating_profit_loss_usd": -160.25, "delta": 0.21, "gamma": -0.00002, "vega": -40.1, "theta": 55.0});
        let o = opt_pos(&p).unwrap();
        assert_eq!((o.side, o.qty, o.avg, o.upnl, o.base.as_str(), o.strike, o.call), (Side::Sell, 1.0, 801.5, -160.25, "BTC", 60000.0, false));
        assert!((o.mark - 960.0).abs() < 1e-9, "mark in USD at the row's index");
        assert_eq!((o.delta, o.gamma, o.vega, o.theta), (0.21, -0.00002, -40.1, 55.0));
        assert_eq!(opt_pos(&json!({"instrument_name": "BTC-6NOV26-60000-P", "direction": "zero", "size": 0.0})).unwrap().qty, 0.0);
        let r = order(&json!({"instrument_name": "ETH-6NOV26-2000-C", "order_id": "ETH-349249", "direction": "buy", "price": 0.0125, "amount": 2.0, "filled_amount": 0.5,
            "order_type": "limit", "time_in_force": "good_til_cancelled", "post_only": true, "reduce_only": false, "creation_timestamp": 1792997254989i64, "label": "t1opt1", "order_state": "open"}));
        assert_eq!((r.id.as_str(), r.side, r.price, r.qty, r.filled, r.tif.as_str(), r.ts, r.client_id.as_deref()),
                   ("ETH-349249", Side::Buy, 0.0125, 2.0, 0.5, "good_til_cancelled post", 1792997254989, Some("t1opt1")));
        // segregated margin: each currency at its index; mm_rate is the worst currency's
        let rows = [json!({"currency": "BTC", "equity": 0.5, "available_funds": 0.4, "maintenance_margin": 0.05, "margin_balance": 0.5}),
            json!({"currency": "USDC", "equity": 1000.0, "available_funds": 900.0, "maintenance_margin": 10.0, "margin_balance": 1000.0}),
            json!({"currency": "ETH", "equity": 0.0})];
        assert!(fold(&rows, |_| None).is_none(), "BTC held: no total without its index");
        let b = fold(&rows, |c| (c == "BTC").then_some(80000.0)).unwrap();
        assert_eq!((b.equity, b.available, b.maint_margin, b.adj_equity, b.mm_rate), (41000.0, 32900.0, Some(4010.0), Some(41000.0), Some(0.1)));
        // cross margin: the venue's USD totals
        let x = [json!({"currency": "BTC", "equity": 0.5, "total_equity_usd": 50000.0, "total_margin_balance_usd": 49000.0, "total_initial_margin_usd": 9000.0, "total_maintenance_margin_usd": 4900.0})];
        let b = fold(&x, |_| None).unwrap();
        assert_eq!((b.equity, b.available, b.maint_margin, b.adj_equity, b.mm_rate), (50000.0, 40000.0, Some(4900.0), Some(49000.0), Some(0.1)));
    }

    #[test]
    fn rpc_errors_carry_the_venue_text() {
        let e = rpc_result("https://test.deribit.com/api/v2/private/buy", "private/buy",
            json!({"jsonrpc": "2.0", "error": {"code": 10039, "message": "not_enough_funds_in_currency", "data": {"reason": "BTC"}}})).unwrap_err();
        assert_eq!(format!("{e:#}"), "deribit private/buy: not_enough_funds_in_currency (10039): BTC");
        assert_eq!(rpc_result("", "public/test", json!({"jsonrpc": "2.0", "result": {"version": "1"}})).unwrap()["version"], "1");
    }
}
