//! Bitget v2 USDT-M futures (productType USDT-FUTURES, marginCoin USDT).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out. Bitget already uses "BTCUSDT" and base-coin sizes, so no mapping is needed.
//! Docs: https://www.bitget.com/docs/catalog/classic-contract-trade/classic-contract-trade (and
//! -market, -position, -account), private WS https://www.bitget.com/docs/classic/websocket/intro.
use super::*;
use base64::Engine;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

const REST: &str = "https://api.bitget.com";
const WS: &str = "wss://ws.bitget.com/v2/ws/private";
const PT: &str = "USDT-FUTURES";
const EX: Exchange = Exchange::Bitget;

/// sign = base64(HMAC_SHA256(secret, timestamp + METHOD + requestPath[?query] + body))
pub fn sign(secret: &str, prehash: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(prehash.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes())
}

/// Signed REST call; returns `data`. Query values are plain (symbols, enums, ids): no encoding needed.
async fn bitget(k: &Keys, post: bool, path: &str, query: &[(&str, String)], body: Option<Value>) -> Result<Value> {
    let q = query.iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>().join("&");
    let path_q = if q.is_empty() { path.to_string() } else { format!("{path}?{q}") };
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let ts = crate::now_ms();
    let method = if post { "POST" } else { "GET" };
    let url = format!("{REST}{path_q}");
    ws::check_paused(&url)?;
    let req = if post { ws::http().post(&url).body(body.clone()) } else { ws::http().get(&url) };
    let r = req.header("ACCESS-KEY", &k.key).header("ACCESS-SIGN", sign(&k.secret, &format!("{ts}{method}{path_q}{body}")))
        .header("ACCESS-TIMESTAMP", ts.to_string()).header("ACCESS-PASSPHRASE", k.extra.as_deref().unwrap_or_default())
        .header("Content-Type", "application/json").header("locale", "en-US").send().await?;
    let st = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let text = r.text().await?;
    ws::note_limit(&url, st, retry_after, &text);
    let v: Value = serde_json::from_str(&text).map_err(|_| anyhow!("bitget {path}: HTTP {st}: {}", &text[..text.len().min(200)]))?;
    if v["code"] != "00000" { bail!("bitget {path}: {} ({})", v["msg"].as_str().unwrap_or("?"), v["code"]); }
    Ok(v["data"].clone())
}

pub async fn rules(symbol: &str) -> Result<Rules> {
    let v = ws::get_json(&format!("{REST}/api/v2/mix/market/contracts?productType={PT}&symbol={symbol}")).await?;
    parse_rules(&v["data"][0]).ok_or_else(|| anyhow!("bitget: {symbol} not listed"))
}

/// tick = priceEndStep * 10^-pricePlace; sizes are multiples of sizeMultiplier (volumePlace decimals)
fn parse_rules(c: &Value) -> Option<Rules> {
    let pp = crate::opt_num(&c["pricePlace"])? as i32;
    let tick = crate::opt_num(&c["priceEndStep"]).unwrap_or(1.0) * 10f64.powi(-pp);
    let step = crate::opt_num(&c["sizeMultiplier"]).filter(|s| *s > 0.0)
        .or_else(|| crate::opt_num(&c["volumePlace"]).map(|d| 10f64.powi(-(d as i32))))?;
    Some(Rules { tick, step, min_qty: crate::opt_num(&c["minTradeNum"]).unwrap_or(0.0), min_notional: crate::opt_num(&c["minTradeUSDT"]).unwrap_or(0.0) })
}

/// marginMode per symbol ("crossed" / "isolated"): place-order requires it. Filled by every
/// account read (mode / leverage, which the engine calls before the first order).
static MARGIN: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// Single account for a symbol (10/s/UID): posMode, marginMode, leverage.
async fn account(k: &Keys, symbol: &str) -> Result<Value> {
    let a = bitget(k, false, "/api/v2/mix/account/account", &[("symbol", symbol.into()), ("productType", PT.into()), ("marginCoin", "USDT".into())], None).await?;
    if let Some(m) = a["marginMode"].as_str() { MARGIN.lock().unwrap().insert(symbol.into(), m.into()); }
    Ok(a)
}

/// Read only: posMode is never changed from here.
pub async fn mode(k: &Keys, symbol: &str) -> Result<Mode> {
    let a = account(k, symbol).await?;
    match a["posMode"].as_str() {
        Some("hedge_mode") => Ok(Mode::Hedge),
        Some("one_way_mode") => Ok(Mode::OneWay),
        m => bail!("bitget: unknown posMode {m:?}"),
    }
}

pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let a = account(k, symbol).await?;
    // ponytail: isolated mode reports the long leverage only; isolatedShortLever can differ, pass the side if that matters
    let l = if a["marginMode"] == "isolated" { &a["isolatedLongLever"] } else { &a["crossedMarginLeverage"] };
    crate::opt_num(l).filter(|x| *x > 0.0).ok_or_else(|| anyhow!("bitget: no leverage for {symbol}"))
}

/// Place-order body. Hedge mode: `side` is the POSITION direction (buy = long, sell = short) and
/// tradeSide open/close says what happens to it (Bitget docs: close long = buy + close).
/// One-way: `side` is the order direction, reduceOnly YES marks a close, no tradeSide.
pub fn order_body(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>, margin_mode: &str) -> Result<Value> {
    let lower = |s: Side| if s == Side::Buy { "buy" } else { "sell" };
    let mut b = json!({"symbol": req.symbol, "productType": PT, "marginMode": margin_mode, "marginCoin": "USDT", "size": qty});
    match mode {
        Mode::Hedge => { b["side"] = lower(req.pos).into(); b["tradeSide"] = if req.close { "close" } else { "open" }.into(); }
        Mode::OneWay => { b["side"] = lower(req.side()).into(); if req.close { b["reduceOnly"] = "YES".into(); } }
    }
    match req.kind {
        Kind::Market => { b["orderType"] = "market".into(); }
        Kind::Limit { tif, .. } => {
            b["orderType"] = "limit".into();
            b["price"] = px.ok_or_else(|| anyhow!("limit order without price"))?.into();
            b["force"] = match tif { Tif::Gtc => "gtc", Tif::Ioc => "ioc", Tif::PostOnly => "post_only" }.into();
        }
        // place-order documents no book-priced (BBO / queue / opponent) order type
        Kind::Bbo { .. } => bail!("bitget has no BBO order type"),
    }
    Ok(b)
}

pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode) -> Result<String> {
    let (qty, px) = checked(req, r, ref_px)?;
    let cached = MARGIN.lock().unwrap().get(&req.symbol).cloned();
    let mm = match cached { Some(m) => m, None => { account(k, &req.symbol).await?; MARGIN.lock().unwrap().get(&req.symbol).cloned().unwrap_or_else(|| "crossed".into()) } };
    let body = order_body(req, mode, &qty, px.as_deref(), &mm)?;
    let d = bitget(k, true, "/api/v2/mix/order/place-order", &[], Some(body)).await?;
    Ok(d["orderId"].as_str().unwrap_or_default().to_string())
}

pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    bitget(k, true, "/api/v2/mix/order/cancel-order", &[], Some(json!({"symbol": symbol, "productType": PT, "marginCoin": "USDT", "orderId": id}))).await?;
    Ok(())
}

/// One position row: REST all-position (`symbol`) and the WS positions push (`instId`) share fields.
fn position(p: &Value) -> Position {
    Position {
        ex: EX, symbol: p["symbol"].as_str().or(p["instId"].as_str()).unwrap_or_default().to_uppercase(),
        side: if p["holdSide"] == "short" { Side::Sell } else { Side::Buy },
        qty: crate::opt_num(&p["total"]).unwrap_or(0.0), entry: crate::opt_num(&p["openPriceAvg"]).unwrap_or(0.0),
        mark: crate::opt_num(&p["markPrice"]).unwrap_or(0.0),
        // <= 0 means no liquidation price
        liq: crate::opt_num(&p["liquidationPrice"]).filter(|x| *x > 0.0),
        upnl: crate::opt_num(&p["unrealizedPL"]).unwrap_or(0.0), lev: crate::opt_num(&p["leverage"]).unwrap_or(0.0),
        margin: crate::opt_num(&p["marginSize"]).unwrap_or(0.0),
        cross: p["marginMode"].as_str().map(|m| m == "crossed"),
    }
}

/// all-position: 5/s/UID
pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    let v = bitget(k, false, "/api/v2/mix/position/all-position", &[("productType", PT.into()), ("marginCoin", "USDT".into())], None).await?;
    Ok(v.as_array().into_iter().flatten().map(position).filter(|p| p.qty > 0.0).collect())
}

/// One order (REST orders-pending and the WS orders push). In hedge mode (posSide long/short)
/// `side` is the position direction, so the real buy/sell is flipped for closes.
fn order(o: &Value) -> OpenOrder {
    let hedge_pos = match o["posSide"].as_str() { Some("long") => Some(Side::Buy), Some("short") => Some(Side::Sell), _ => None };
    let closing = o["tradeSide"].as_str().is_some_and(|t| t.contains("close"));
    let side = match hedge_pos {
        Some(p) if closing => if p == Side::Buy { Side::Sell } else { Side::Buy },
        Some(p) => p,
        None => if o["side"] == "sell" { Side::Sell } else { Side::Buy },
    };
    OpenOrder {
        ex: EX, symbol: o["symbol"].as_str().or(o["instId"].as_str()).unwrap_or_default().to_uppercase(),
        id: o["orderId"].as_str().unwrap_or_default().into(), side,
        price: crate::opt_num(&o["price"]).unwrap_or(0.0), qty: crate::opt_num(&o["size"]).unwrap_or(0.0),
        // WS: accBaseVolume is cumulative, baseVolume the latest fill; REST: baseVolume is cumulative
        filled: crate::opt_num(&o["accBaseVolume"]).or(crate::opt_num(&o["baseVolume"])).unwrap_or(0.0),
        kind: o["orderType"].as_str().unwrap_or_default().into(),
        reduce_only: o["reduceOnly"].as_str().is_some_and(|r| r.eq_ignore_ascii_case("yes")) || (hedge_pos.is_some() && closing),
        ts: crate::opt_num(&o["cTime"]).unwrap_or(0.0) as i64, pos: hedge_pos,
    }
}

/// orders-pending: 10/s/UID, 100 per page (paged by idLessThan = endId)
pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let mut out = vec![];
    let mut before: Option<String> = None;
    loop {
        let mut q = vec![("productType", PT.to_string()), ("limit", "100".to_string())];
        if let Some(b) = &before { q.push(("idLessThan", b.clone())); }
        let v = bitget(k, false, "/api/v2/mix/order/orders-pending", &q, None).await?;
        let page = v["entrustedList"].as_array().cloned().unwrap_or_default();
        out.extend(page.iter().map(order));
        match v["endId"].as_str() { Some(e) if page.len() >= 100 && !e.is_empty() => before = Some(e.into()), _ => break }
    }
    Ok(out)
}

/// Account totals: multi-assets (union) mode margins the whole account in USD; single mode is the USDT row.
fn wallet(a: &Value) -> (f64, f64) {
    let union = a["assetMode"] == "union" || a["assetsMode"] == "union";
    let get = |f: &str| crate::opt_num(&a[f]);
    if union && get("unionTotalMargin").is_some() {
        (get("unionTotalMargin").unwrap_or(0.0), get("unionAvailable").unwrap_or(0.0))
    } else {
        // REST: accountEquity / crossedMaxAvailable; WS account push: equity / maxOpenPosAvailable
        (get("usdtEquity").or(get("accountEquity")).or(get("equity")).unwrap_or(0.0),
         get("crossedMaxAvailable").or(get("maxOpenPosAvailable")).or(get("available")).unwrap_or(0.0))
    }
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let v = bitget(k, false, "/api/v2/mix/account/accounts", &[("productType", PT.into())], None).await?;
    let a = v.as_array().into_iter().flatten().find(|a| a["marginCoin"].as_str().is_some_and(|c| c.eq_ignore_ascii_case("USDT")))
        .ok_or_else(|| anyhow!("bitget: no USDT futures account"))?;
    let (equity, available) = wallet(a);
    // crossedRiskRate: maintenance margin / equity, liquidation at 1 (same sense as Bybit accountMMRate)
    Ok(Balance { equity, available, uni_mmr: None, mm_rate: crate::opt_num(&a["crossedRiskRate"]), ..Default::default() })
}

/// The positions channel pushes the full position list every time: rows missing from this push
/// (closed since the last one) are emitted with qty 0.
fn position_push(data: &Value, held: &mut HashSet<(String, Side)>) -> Vec<AccEvent> {
    let mut now = HashSet::new();
    let mut out = vec![];
    for p in data.as_array().into_iter().flatten() {
        let pos = position(p);
        if pos.qty > 0.0 { now.insert((pos.symbol.clone(), pos.side)); }
        out.push(AccEvent::Position { one_way: p["posMode"] == "one_way_mode", p: pos });
    }
    for (symbol, side) in held.difference(&now) {
        out.push(AccEvent::Position { one_way: false, p: Position { ex: EX, symbol: symbol.clone(), side: *side, qty: 0.0, entry: 0.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None } });
    }
    *held = now;
    out
}

/// Private stream: login, then positions / orders / account for USDT-FUTURES; Resync once all
/// three subscriptions are acknowledged.
pub async fn stream(k: Keys, tx: AccTx) {
    loop {
        // WS login timestamp is in SECONDS (official SDKs; the docs' prose says ms but the example is seconds)
        let ts = crate::now_ms() / 1000;
        let login = json!({"op": "login", "args": [{"apiKey": k.key, "passphrase": k.extra.clone().unwrap_or_default(),
            "timestamp": ts.to_string(), "sign": sign(&k.secret, &format!("{ts}GET/user/verify"))}]});
        let sub = json!({"op": "subscribe", "args": [
            {"instType": PT, "channel": "positions", "instId": "default"},
            {"instType": PT, "channel": "orders", "instId": "default"},
            {"instType": PT, "channel": "account", "coin": "default"}]});
        // ponytail: subscribe is sent right after login without waiting for the ack (ws::once sends all subs
        // up front); if Bitget ever rejects it as "not logged in", ws::Spec needs a send-after-ack hook
        let spec = ws::Spec::new("bitget account", WS).login_first().sub(login.to_string()).sub(sub.to_string())
            .ping(std::time::Duration::from_secs(30), "ping");
        let (mut logged_in, mut auth_failed, mut acks) = (false, false, 0);
        let mut held = HashSet::new();
        let r = ws::once(&spec, |f| {
            let ws::Frame::Text(s) = f else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true }; // "pong"
            let send = |e: AccEvent| { let _ = tx.send((EX, e)); };
            match v["event"].as_str() {
                Some("login") if v["code"] == "0" || v["code"] == 0 => logged_in = true,
                Some("login" | "error") => {
                    eprintln!("[bitget account] {}: {} ({})", if logged_in { "error" } else { "login failed" }, v["msg"], v["code"]);
                    auth_failed = !logged_in;
                    return false;
                }
                Some("subscribe") => { acks += 1; if acks == 3 { send(AccEvent::Resync); } }
                _ => {}
            }
            match v["arg"]["channel"].as_str() {
                _ if v.get("event").is_some() => {}
                Some("positions") => for e in position_push(&v["data"], &mut held) { send(e) },
                Some("orders") => for o in v["data"].as_array().into_iter().flatten() {
                    let open = matches!(o["status"].as_str(), Some("live" | "new" | "partially_filled"));
                    send(if open { AccEvent::Order(order(o)) } else { AccEvent::OrderDone { id: o["orderId"].as_str().unwrap_or_default().into() } });
                },
                Some("account") => for a in v["data"].as_array().into_iter().flatten().filter(|a| a["marginCoin"] == "USDT") {
                    let (equity, available) = wallet(a);
                    send(AccEvent::Wallet { equity, available });
                },
                _ => {}
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[bitget account] {e:#}"); }
        // Bitget allows 300 connection attempts per IP per 5 minutes
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 5 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature() {
        // references computed independently with Python's hmac + base64
        assert_eq!(sign("YYYYSECRET", "1700000000000GET/api/v2/mix/account/account?symbol=BTCUSDT&productType=USDT-FUTURES&marginCoin=USDT"),
                   "3XCK1CFggL5jrx33KnUAD617++tBWlbHGm/gQsXCso0=");
        assert_eq!(sign("YYYYSECRET", r#"1700000000000POST/api/v2/mix/order/cancel-order{"orderId":"1","productType":"USDT-FUTURES","symbol":"BTCUSDT"}"#),
                   "TxnfTVc53z4D7XfWiKE2Jtnpiw640Rm9LzdH8Kd+C8Q=");
        assert_eq!(sign("YYYYSECRET", "1700000000GET/user/verify"), "ncaZkuPLdgAq/NBd8hHbsJUtC//TXVyn0ngncN5YKEQ=");
    }

    #[test]
    fn hedge_and_one_way_bodies() {
        let o = |pos, close, kind| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty: 1.0, client_id: None };
        for (pos, close) in [(Side::Buy, false), (Side::Buy, true), (Side::Sell, false), (Side::Sell, true)] {
            let r = o(pos, close, Kind::Market);
            let h = order_body(&r, Mode::Hedge, "0.01", None, "crossed").unwrap();
            // hedge: side = position direction, tradeSide = open / close, no reduceOnly
            assert_eq!(h["side"], if pos == Side::Buy { "buy" } else { "sell" });
            assert_eq!(h["tradeSide"], if close { "close" } else { "open" });
            assert!(h.get("reduceOnly").is_none());
            let w = order_body(&r, Mode::OneWay, "0.01", None, "isolated").unwrap();
            // one-way: side = order direction, reduceOnly marks a close, no tradeSide
            assert_eq!(w["side"], if r.side() == Side::Buy { "buy" } else { "sell" });
            assert!(w.get("tradeSide").is_none());
            assert_eq!(w.get("reduceOnly").is_some(), close);
            assert_eq!((w["orderType"].as_str(), w["marginMode"].as_str(), w["size"].as_str(), w["productType"].as_str()),
                       (Some("market"), Some("isolated"), Some("0.01"), Some("USDT-FUTURES")));
        }
        let l = order_body(&o(Side::Buy, false, Kind::Limit { price: 1.0, tif: Tif::PostOnly }), Mode::OneWay, "1", Some("86000.1"), "crossed").unwrap();
        assert_eq!((l["orderType"].as_str(), l["price"].as_str(), l["force"].as_str()), (Some("limit"), Some("86000.1"), Some("post_only")));
        assert!(order_body(&o(Side::Buy, false, Kind::Bbo { queue: true, level: 1 }), Mode::OneWay, "1", None, "crossed").is_err());
    }

    #[test]
    fn rules_from_contract_config() {
        // live /api/v2/mix/market/contracts rows (BTCUSDT, DOGEUSDT)
        let r = parse_rules(&json!({"pricePlace": "1", "priceEndStep": "1", "volumePlace": "4", "sizeMultiplier": "0.0001", "minTradeNum": "0.0001", "minTradeUSDT": "5"})).unwrap();
        assert!((r.tick - 0.1).abs() < 1e-12 && (r.step - 0.0001).abs() < 1e-12 && r.min_notional == 5.0);
        let d = parse_rules(&json!({"pricePlace": "5", "priceEndStep": "1", "volumePlace": "0", "sizeMultiplier": "1", "minTradeNum": "1", "minTradeUSDT": "5"})).unwrap();
        assert_eq!((fmt_step(0.123456, d.tick, false), fmt_step(12.7, d.step, true)), ("0.12346".into(), "12".into()));
        let e = parse_rules(&json!({"pricePlace": "2", "priceEndStep": "5", "volumePlace": "3"})).unwrap();
        assert_eq!((fmt_step(1.234, e.tick, false), e.step), ("1.25".into(), 0.001));
    }

    #[test]
    fn parse_documented_samples() {
        // all-position response (docs)
        let p = position(&json!({"symbol": "BTCUSDT", "holdSide": "short", "marginSize": "103.18", "total": "0.0155", "leverage": "14",
            "openPriceAvg": "88505.2", "posMode": "hedge_mode", "unrealizedPL": "-72.82", "liquidationPrice": "5737867.86", "markPrice": "93203.4"}));
        assert_eq!((p.symbol.as_str(), p.side, p.qty, p.lev, p.liq), ("BTCUSDT", Side::Sell, 0.0155, 14.0, Some(5737867.86)));
        assert_eq!(position(&json!({"instId": "ETHUSDT", "holdSide": "long", "total": "1", "liquidationPrice": "0"})).liq, None);
        // orders-pending row (docs): hedge, open long, partially filled, lowercase symbol
        let o = order(&json!({"symbol": "ethusdt", "size": "100", "orderId": "123", "baseVolume": "12.1", "price": "1900", "status": "partially_filled",
            "side": "buy", "posSide": "long", "tradeSide": "open", "posMode": "hedge_mode", "orderType": "limit", "cTime": "1627293504612", "reduceOnly": "NO"}));
        assert_eq!((o.symbol.as_str(), o.side, o.pos, o.filled, o.reduce_only, o.ts), ("ETHUSDT", Side::Buy, Some(Side::Buy), 12.1, false, 1627293504612));
        // hedge close long is sent as side=buy: the real direction is a sell
        let c = order(&json!({"instId": "BTCUSDT", "side": "buy", "posSide": "long", "tradeSide": "close", "accBaseVolume": "0", "baseVolume": "0.5", "reduceOnly": "no"}));
        assert_eq!((c.side, c.reduce_only, c.filled), (Side::Sell, true, 0.0));
        // one-way reduce-only sell
        let w = order(&json!({"instId": "BTCUSDT", "side": "sell", "posSide": "net", "tradeSide": "sell_single", "reduceOnly": "yes"}));
        assert_eq!((w.side, w.pos, w.reduce_only), (Side::Sell, None, true));
        // account: single mode REST row and union-mode WS push (docs)
        assert_eq!(wallet(&json!({"accountEquity": "100", "usdtEquity": "100.5", "crossedMaxAvailable": "80", "available": "90", "assetMode": "single"})), (100.5, 80.0));
        assert_eq!(wallet(&json!({"equity": "11.98", "maxOpenPosAvailable": "11.9", "unionTotalMargin": "100", "unionAvailable": "20", "assetsMode": "union"})), (100.0, 20.0));
    }

    #[test]
    fn positions_push_removes_closed_rows() {
        let mut held = HashSet::new();
        let ev = position_push(&json!([{"instId": "BTCUSDT", "holdSide": "long", "total": "0.1", "posMode": "hedge_mode"},
                                       {"instId": "ETHUSDT", "holdSide": "short", "total": "2", "posMode": "one_way_mode"}]), &mut held);
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[1], AccEvent::Position { one_way: true, .. }));
        let ev = position_push(&json!([{"instId": "BTCUSDT", "holdSide": "long", "total": "0.1", "posMode": "hedge_mode"}]), &mut held);
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[1], AccEvent::Position { p, .. } if p.symbol == "ETHUSDT" && p.side == Side::Sell && p.qty == 0.0));
        assert!(position_push(&json!([]), &mut held).len() == 1 && held.is_empty());
    }
}
