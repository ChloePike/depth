//! Kraken Futures (futures.kraken.com, multi-collateral linear perps like PF_XBTUSD; BTC is XBT).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out; native symbols only inside this file. PF contracts are one unit of base, so
//! sizes pass through; prices are USD and pass through as-is (venue-native).
//! Kraken Futures is net (one-way) only: a close is a reduceOnly order.
//! `T1_TESTNET=1` uses demo-futures.kraken.com.
use super::*;
use base64::Engine;
use sha2::{Digest, Sha512};

fn host() -> &'static str { if testnet() { "https://demo-futures.kraken.com" } else { "https://futures.kraken.com" } }
fn ws_url() -> &'static str { if testnet() { "wss://demo-futures.kraken.com/ws/v1" } else { "wss://futures.kraken.com/ws/v1" } }

/// "BTCUSDT" -> "PF_XBTUSD"
pub fn native(symbol: &str) -> String {
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    format!("PF_{}USD", if base == "BTC" { "XBT" } else { base })
}

/// "PF_XBTUSD" -> "BTCUSDT"; None for anything but a linear multi-collateral perp
pub fn unified(inst: &str) -> Option<String> {
    let base = inst.strip_prefix("PF_")?.strip_suffix("USD")?;
    Some(format!("{}USDT", if base == "XBT" { "BTC" } else { base }))
}

/// base64(HMAC-SHA512(base64decode(secret), SHA256(msg))). REST: msg = postData + nonce +
/// endpointPath; WS: msg = challenge.
fn sign(secret: &str, msg: &str) -> Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let key = b64.decode(secret.trim()).map_err(|e| anyhow!("kraken: API secret is not base64: {e}"))?;
    let mut m = Hmac::<Sha512>::new_from_slice(&key).expect("hmac key");
    m.update(&Sha256::digest(msg.as_bytes()));
    Ok(b64.encode(m.finalize().into_bytes()))
}

/// Signed REST call. `path` is the part after /derivatives (e.g. "/api/v3/sendorder"), which is
/// what Authent signs. GET calls here take no parameters (postData empty); POST parameters go
/// form-encoded in the body and the exact body string is signed.
async fn call(k: &Keys, post: bool, path: &str, params: &[(&str, String)]) -> Result<Value> {
    // ponytail: values are plain symbols / numbers / uuids, joined unescaped; percent-encode if free text is ever sent
    let body = params.iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>().join("&");
    let nonce = crate::now_ms().to_string();
    let url = format!("{}/derivatives{path}", host());
    ws::check_paused(&url)?;
    let req = if post {
        ws::http().post(&url).header("Content-Type", "application/x-www-form-urlencoded").body(body.clone())
    } else {
        ws::http().get(&url)
    };
    let r = req.header("APIKey", &k.key).header("Nonce", &nonce).header("Authent", sign(&k.secret, &format!("{body}{nonce}{path}"))?).send().await?;
    let st = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let text = r.text().await?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    // the cost budget (500 per 10s) answers apiLimitExceeded, sometimes with HTTP 200
    ws::note_limit(&url, if v["error"] == "apiLimitExceeded" { 429 } else { st }, retry_after, &text);
    if v["result"] != "success" {
        bail!("kraken {path}: HTTP {st}: {}", v["error"].as_str().map(String::from).unwrap_or_else(|| text[..text.len().min(200)].to_string()));
    }
    Ok(v)
}

pub async fn rules(symbol: &str) -> Result<Rules> {
    let sym = native(symbol);
    let v = ws::get_json(&format!("{}/derivatives/api/v3/instruments", host())).await?;
    let i = v["instruments"].as_array().into_iter().flatten().find(|i| i["symbol"] == sym.as_str()).ok_or_else(|| anyhow!("kraken: {sym} not listed"))?;
    rules_of(i)
}

/// contractValueTradePrecision p: sizes are multiples of 10^-p (PF_XBTUSD 4 -> 0.0001, PF_PEPEUSD -3 -> 1000)
fn rules_of(i: &Value) -> Result<Rules> {
    if i["tradeable"] == false { bail!("kraken: {} not tradeable", i["symbol"]); }
    let p = i["contractValueTradePrecision"].as_i64().ok_or_else(|| anyhow!("kraken: no trade precision for {}", i["symbol"]))?;
    // PF contracts are 1 base unit; a different contractSize would need a size conversion here
    if n(&i["contractSize"]) != 1.0 { bail!("kraken: {} contractSize {} not supported", i["symbol"], i["contractSize"]); }
    let step = 10f64.powi(-(p as i32));
    Ok(Rules { tick: n(&i["tickSize"]), step, min_qty: step, min_notional: 0.0 })
}

/// Net positions only.
pub async fn mode(_k: &Keys, _symbol: &str) -> Result<Mode> { Ok(Mode::OneWay) }

/// The isolated-margin leverage preference for the symbol (read only). No preference means the
/// symbol trades cross margin, which has no fixed leverage: that is an error here.
pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let sym = native(symbol);
    let v = call(k, false, "/api/v3/leveragepreferences", &[]).await?;
    v["leveragePreferences"].as_array().into_iter().flatten().find(|p| p["symbol"] == sym.as_str()).and_then(|p| crate::opt_num(&p["maxLeverage"]))
        .ok_or_else(|| anyhow!("kraken: {sym} is cross margin (no leverage preference)"))
}

/// sendorder params. Market = "mkt" (IOC with 1% price protection), post-only = "post".
pub fn order_params(req: &OrderReq, qty: &str, px: Option<&str>) -> Result<Vec<(&'static str, String)>> {
    let ty = match req.kind {
        Kind::Market => "mkt",
        Kind::Limit { tif: Tif::Gtc, .. } => "lmt",
        Kind::Limit { tif: Tif::Ioc, .. } => "ioc",
        Kind::Limit { tif: Tif::PostOnly, .. } => "post",
        Kind::Bbo { .. } => bail!("kraken: BBO orders not supported"),
        Kind::Limit { tif: Tif::Fok, .. } => bail!("kraken: no fill-or-kill orders"),
        Kind::Stop { .. } | Kind::Trailing { .. } => bail!("kraken: conditional orders are not supported here yet"),
    };
    let mut p = vec![("orderType", ty.to_string()), ("symbol", native(&req.symbol)), ("side", if req.side() == Side::Buy { "buy" } else { "sell" }.into()), ("size", qty.into())];
    if let Some(px) = px { p.push(("limitPrice", px.into())); }
    if req.close { p.push(("reduceOnly", "true".into())); }
    Ok(p)
}

pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, _mode: Mode) -> Result<String> {
    let (qty, px) = checked(req, r, ref_px)?;
    let v = call(k, true, "/api/v3/sendorder", &order_params(req, &qty, px.as_deref())?).await?;
    send_result(&v)
}

/// "success" only means the request was assessed: sendStatus.status says whether it was placed.
fn send_result(v: &Value) -> Result<String> {
    let s = &v["sendStatus"];
    match s["status"].as_str() {
        Some("placed" | "partiallyFilled" | "filled") => Ok(s["order_id"].as_str().unwrap_or_default().to_string()),
        st => bail!("kraken sendorder: {}", st.unwrap_or("no status")),
    }
}

pub async fn cancel(k: &Keys, _symbol: &str, id: &str) -> Result<()> {
    let v = call(k, true, "/api/v3/cancelorder", &[("order_id", id.into())]).await?;
    match v["cancelStatus"]["status"].as_str() {
        Some("cancelled") => Ok(()),
        st => bail!("kraken cancelorder: {}", st.unwrap_or("no status")),
    }
}

/// REST rows carry side/size/entry/upnl only; mark/liq/margin come from the open_positions push.
fn rest_position(p: &Value) -> Option<Position> {
    let symbol = unified(p["symbol"].as_str()?)?;
    Some(Position { ex: Exchange::Kraken, symbol, side: if p["side"] == "short" { Side::Sell } else { Side::Buy }, qty: n(&p["size"]).abs(),
        entry: n(&p["price"]), mark: 0.0, liq: None, upnl: crate::opt_num(&p["unrealizedPnl"]).unwrap_or(0.0), lev: 0.0, margin: 0.0, cross: None })
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    let v = call(k, false, "/api/v3/openpositions", &[]).await?;
    Ok(v["openPositions"].as_array().into_iter().flatten().filter_map(rest_position).filter(|p| p.qty > 0.0).collect())
}

fn rest_order(o: &Value) -> Option<OpenOrder> {
    let symbol = unified(o["symbol"].as_str()?)?;
    let filled = n(&o["filledSize"]);
    Some(OpenOrder { ex: Exchange::Kraken, symbol, id: o["order_id"].as_str()?.into(), side: if o["side"] == "sell" { Side::Sell } else { Side::Buy },
        price: crate::opt_num(&o["limitPrice"]).unwrap_or(0.0), qty: crate::opt_num(&o["unfilledSize"]).unwrap_or(0.0) + filled, filled,
        kind: o["orderType"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false),
        ts: o["receivedTime"].as_str().map(crate::iso_ms).unwrap_or(0), pos: None })
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let v = call(k, false, "/api/v3/openorders", &[]).await?;
    Ok(v["openOrders"].as_array().into_iter().flatten().filter_map(rest_order).collect())
}

/// Multi-collateral (flex) wallet, USD: equity = portfolioValue (balance + upnl),
/// available = availableMargin; mm_rate = maintenance margin / margin equity.
fn flex_balance(f: &Value) -> Balance {
    let eq = n(&f["marginEquity"]);
    Balance { equity: n(&f["portfolioValue"]), available: n(&f["availableMargin"]), uni_mmr: None,
        mm_rate: (eq > 0.0).then(|| n(&f["maintenanceMargin"]) / eq),
        maint_margin: Some(n(&f["maintenanceMargin"])), adj_equity: None }
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let v = call(k, false, "/api/v3/accounts", &[]).await?;
    let f = &v["accounts"]["flex"];
    if f.is_null() { bail!("kraken: no multi-collateral (flex) account"); }
    Ok(flex_balance(f))
}

// ---------------------------------------------------------------- private stream

/// One open_positions row. `balance` is the signed net size (negative = short).
fn ws_position(p: &Value) -> Option<Position> {
    let symbol = unified(p["instrument"].as_str()?)?;
    let (bal, mark, im) = (n(&p["balance"]), n(&p["mark_price"]), n(&p["initial_margin"]));
    Some(Position { ex: Exchange::Kraken, symbol, side: if bal < 0.0 { Side::Sell } else { Side::Buy }, qty: bal.abs(), entry: n(&p["entry_price"]),
        mark: if mark.is_finite() { mark } else { 0.0 }, liq: crate::opt_num(&p["liquidation_threshold"]).filter(|x| *x > 0.0),
        upnl: crate::opt_num(&p["pnl"]).unwrap_or(0.0), lev: if im > 0.0 { bal.abs() * mark / im } else { 0.0 }, margin: if im.is_finite() { im } else { 0.0 }, cross: None })
}

/// One open_orders order. `qty` is the REMAINING size (0 with filled = full on a full fill).
fn ws_order(o: &Value) -> Option<OpenOrder> {
    let symbol = unified(o["instrument"].as_str()?)?;
    let filled = n(&o["filled"]);
    Some(OpenOrder { ex: Exchange::Kraken, symbol, id: o["order_id"].as_str()?.into(), side: if o["direction"] == 1 { Side::Sell } else { Side::Buy },
        price: crate::opt_num(&o["limit_price"]).unwrap_or(0.0), qty: n(&o["qty"]) + filled, filled,
        kind: o["type"].as_str().unwrap_or_default().into(), reduce_only: o["reduce_only"].as_bool().unwrap_or(false), ts: o["time"].as_i64().unwrap_or(0), pos: None })
}

/// Maps one private-feed message. `held` = unified symbols with an open position from the last
/// open_positions push (each push is the full list; a symbol that drops out was closed).
fn on_msg(v: &Value, held: &mut Vec<String>, send: &mut impl FnMut(AccEvent)) {
    match v["feed"].as_str() {
        Some("open_positions") => {
            let now: Vec<Position> = v["positions"].as_array().into_iter().flatten().filter_map(ws_position).filter(|p| p.qty > 0.0).collect();
            for s in held.iter().filter(|s| !now.iter().any(|p| &p.symbol == *s)) {
                send(AccEvent::Position { one_way: true, p: Position { ex: Exchange::Kraken, symbol: s.clone(), side: Side::Buy, qty: 0.0, entry: 0.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None } });
            }
            *held = now.iter().map(|p| p.symbol.clone()).collect();
            for p in now { send(AccEvent::Position { p, one_way: true }); }
        }
        Some("open_orders_snapshot") => for o in v["orders"].as_array().into_iter().flatten().filter_map(ws_order) { send(AccEvent::Order(o)); },
        Some("open_orders") => {
            if v["is_cancel"] == true {
                // cancels carry only order_id at the top; fill-driven removals carry the order
                let id = v["order_id"].as_str().or(v["order"]["order_id"].as_str()).unwrap_or_default();
                send(AccEvent::OrderDone { id: id.into() });
            } else if let Some(o) = ws_order(&v["order"]) {
                send(AccEvent::Order(o));
            }
        }
        Some("balances" | "balances_snapshot") => {
            let f = &v["flex_futures"];
            if f.is_object() { send(AccEvent::Wallet { equity: n(&f["portfolio_value"]), available: n(&f["available_margin"]) }); }
        }
        _ => {}
    }
}

/// Private subscribes for a challenge reply (`{"event":"challenge","message":<uuid>}`); empty for
/// any other reply (Kraken greets with `{"event":"info"}` first).
fn login_subs(k: &Keys, reply: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(reply) else { return vec![] };
    let (Some("challenge"), Some(ch)) = (v["event"].as_str(), v["message"].as_str()) else { return vec![] };
    let signed = match sign(&k.secret, ch) { Ok(s) => s, Err(e) => { eprintln!("[kraken account] {e:#}"); return vec![] } };
    ["open_positions", "open_orders", "balances"].iter()
        .map(|feed| json!({"event": "subscribe", "feed": feed, "api_key": k.key, "original_challenge": ch, "signed_challenge": signed}).to_string()).collect()
}

/// One connection: subs[0] requests the challenge, `on_login` signs it and subscribes the private
/// feeds; re-subscribing to heartbeat is the keepalive (as in the public perp connector).
pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Kraken;
    loop {
        let mut auth_failed = false;
        let hb = json!({"event": "subscribe", "feed": "heartbeat"}).to_string();
        let k2 = k.clone();
        let spec = ws::Spec::new("kraken account", ws_url())
            .sub(json!({"event": "challenge", "api_key": k.key}).to_string())
            .sub(hb.clone())
            .on_login(move |reply| login_subs(&k2, reply))
            .ping(std::time::Duration::from_secs(30), hb);
        let (mut held, mut synced) = (vec![], false);
        let r = ws::once(&spec, |f| {
            let ws::Frame::Text(s) = f else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
            let mut send = |e: AccEvent| { let _ = tx.send((ex, e)); };
            match v["event"].as_str() {
                Some("subscribed") if v["feed"] != "heartbeat" && !synced => { synced = true; send(AccEvent::Resync); }
                Some("error" | "subscribed_failed") => { eprintln!("[kraken account] {v}"); auth_failed = true; return false; }
                Some(_) => {}
                None => on_msg(&v, &mut held, &mut send),
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[kraken account] {e:#}"); }
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 2 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_signature_matches_docs() {
        // example from Kraken's Derivatives WebSocket "Sign challenge" documentation
        assert_eq!(sign("7zxMEF5p/Z8l2p2U7Ghv6x14Af+Fx+92tPgUdVQ748FOIrEoT9bgT+bTRfXc5pz8na+hL/QdrCVG7bh9KpT0eMTm", "c100b894-1729-464d-ace1-52dbce11db42").unwrap(),
                   "4JEpF3ix66GA2B+ooK128Ift4XQVtc137N9yeg4Kqsn9PI0Kpzbysl9M1IeCEdjg0zl00wkVqcsnG4bmnlMb3A==");
        assert!(sign("not base64!", "x").is_err());
    }

    #[test]
    fn authent_layout() {
        // REST docs give inputs but no expected value (and their example secret is not valid
        // base64): documented postData / nonce / endpointPath with the WS doc secret, reference
        // computed independently with Python hashlib/hmac/base64
        let msg = format!("{}{}{}", "symbol=fi_xbtusd_180615", "1415957147987", "/api/v3/orderbook");
        assert_eq!(sign("7zxMEF5p/Z8l2p2U7Ghv6x14Af+Fx+92tPgUdVQ748FOIrEoT9bgT+bTRfXc5pz8na+hL/QdrCVG7bh9KpT0eMTm", &msg).unwrap(),
                   "JHLjN8OUDYaXjHRGT0z4nUvJorORrXoL9omot4BK5ihtp6jKHPHfzX9MrpVjqAgKqGJejoO3gySwoVCMJQlx/Q==");
    }

    #[test]
    fn symbols() {
        assert_eq!(native("BTCUSDT"), "PF_XBTUSD");
        assert_eq!(native("ETHUSDT"), "PF_ETHUSD");
        assert_eq!(unified("PF_XBTUSD").as_deref(), Some("BTCUSDT"));
        assert_eq!(unified("PF_SOLUSD").as_deref(), Some("SOLUSDT"));
        assert_eq!(unified("PI_XBTUSD"), None);
        assert_eq!(unified("OF_ETHUSD_240101_1000_C"), None);
    }

    #[test]
    fn rules_from_instrument() {
        let r = rules_of(&json!({"symbol": "PF_XBTUSD", "tickSize": 1, "contractSize": 1, "tradeable": true, "contractValueTradePrecision": 4})).unwrap();
        assert_eq!((r.tick, r.min_qty), (1.0, 0.0001));
        assert_eq!(fmt_step(0.123456, r.step, true), "0.1234");
        let r = rules_of(&json!({"symbol": "PF_PEPEUSD", "tickSize": 1e-10, "contractSize": 1, "tradeable": true, "contractValueTradePrecision": -3})).unwrap();
        assert_eq!(fmt_step(12_345.0, r.step, true), "12000");
    }

    #[test]
    fn order_params_by_intent() {
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let o = |pos, close, kind| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty: 0.01, client_id: None };
        let p = order_params(&o(Side::Sell, true, Kind::Limit { price: 90000.0, tif: Tif::PostOnly }), "0.01", Some("90000")).unwrap();
        assert_eq!(p.iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>().join("&"),
                   "orderType=post&symbol=PF_XBTUSD&side=buy&size=0.01&limitPrice=90000&reduceOnly=true");
        let m = order_params(&o(Side::Buy, false, Kind::Market), "0.01", None).unwrap();
        assert_eq!((get(&m, "orderType").as_deref(), get(&m, "side").as_deref(), get(&m, "limitPrice"), get(&m, "reduceOnly")), (Some("mkt"), Some("buy"), None, None));
        assert_eq!(get(&order_params(&o(Side::Buy, false, Kind::Limit { price: 1.0, tif: Tif::Gtc }), "1", Some("1")).unwrap(), "orderType").as_deref(), Some("lmt"));
        assert_eq!(get(&order_params(&o(Side::Sell, false, Kind::Limit { price: 1.0, tif: Tif::Ioc }), "1", Some("1")).unwrap(), "side").as_deref(), Some("sell"));
        assert!(order_params(&o(Side::Buy, false, Kind::Bbo { queue: true, level: 1 }), "1", None).is_err());
    }

    #[test]
    fn rest_samples() {
        // documented examples (sendorder, openpositions, openorders, accounts)
        assert_eq!(send_result(&json!({"result": "success", "sendStatus": {"status": "placed", "order_id": "c18f0c17-9971-40e6-8e5b10df05d422f0"}})).unwrap(), "c18f0c17-9971-40e6-8e5b10df05d422f0");
        assert!(send_result(&json!({"result": "success", "sendStatus": {"status": "insufficientAvailableFunds"}})).is_err());
        let p = rest_position(&json!({"side": "short", "symbol": "PF_XBTUSD", "price": 9392.75, "size": 0.5, "unrealizedPnl": -6.0})).unwrap();
        assert_eq!((p.symbol.as_str(), p.side, p.qty, p.entry), ("BTCUSDT", Side::Sell, 0.5, 9392.75));
        assert!(rest_position(&json!({"side": "long", "symbol": "FI_XBTUSD_201225", "size": 1})).is_none());
        let o = rest_order(&json!({"order_id": "59302619-41d2-4f0b-941f-7e7914760ad3", "symbol": "PF_ETHUSD", "side": "sell", "orderType": "lmt", "limitPrice": 10640,
            "unfilledSize": 304, "receivedTime": "2019-09-05T17:01:17.410Z", "status": "untouched", "filledSize": 6, "reduceOnly": true})).unwrap();
        assert_eq!((o.symbol.as_str(), o.side, o.price, o.qty, o.filled, o.reduce_only), ("ETHUSDT", Side::Sell, 10640.0, 310.0, 6.0, true));
        assert!(o.ts > 1_500_000_000_000);
        let b = flex_balance(&json!({"balanceValue": 34995.52, "portfolioValue": 34995.52, "marginEquity": 34122.66, "availableMargin": 34122.66, "maintenanceMargin": 0}));
        assert_eq!((b.equity, b.available, b.mm_rate), (34995.52, 34122.66, Some(0.0)));
    }

    #[test]
    fn ws_samples() {
        let mut ev = vec![];
        let mut held = vec![];
        let mut send = |e: AccEvent| ev.push(e);
        // documented open_positions snapshot: XRP + XBT perps and an option (ignored)
        on_msg(&json!({"feed": "open_positions", "positions": [
            {"instrument": "PF_XRPUSD", "balance": 500.0, "pnl": -239.65, "entry_price": 0.3985, "mark_price": 0.4925844, "liquidation_threshold": 0.0, "initial_margin": 101.5},
            {"instrument": "PF_XBTUSD", "balance": -0.04, "pnl": 119.56, "entry_price": 26911.75, "mark_price": 29900.0, "liquidation_threshold": 9572.8, "initial_margin": 21.5294},
            {"instrument": "OF_ETHUSD_240101_1000_C", "balance": 0.04}]}), &mut held, &mut send);
        // next push: XRP closed
        on_msg(&json!({"feed": "open_positions", "positions": [{"instrument": "PF_XBTUSD", "balance": -0.04, "mark_price": 29900.0}]}), &mut held, &mut send);
        // documented open_orders new + cancel, a full fill, and a flex balances delta
        on_msg(&json!({"feed": "open_orders", "order": {"instrument": "PF_XBTUSD", "time": 1567702877410_i64, "qty": 304.0, "filled": 0.0, "limit_price": 10640.0,
            "type": "limit", "order_id": "59302619-41d2-4f0b-941f-7e7914760ad3", "direction": 1, "reduce_only": true}, "is_cancel": false, "reason": "new_placed_order_by_user"}), &mut held, &mut send);
        on_msg(&json!({"feed": "open_orders", "order_id": "660c6b23-8007-48c1-a7c9-4893f4572e8c", "is_cancel": true, "reason": "cancelled_by_user"}), &mut held, &mut send);
        on_msg(&json!({"feed": "open_orders", "order": {"instrument": "PF_XBTUSD", "qty": 0, "filled": 0.0001, "order_id": "f1", "direction": 0}, "is_cancel": true, "reason": "full_fill"}), &mut held, &mut send);
        on_msg(&json!({"feed": "balances", "flex_futures": {"portfolio_value": 5000.0, "available_margin": 4800.0}}), &mut held, &mut send);
        on_msg(&json!({"feed": "balances", "futures": {"F-XBT:USD": {}}}), &mut held, &mut send);
        let s: Vec<String> = ev.iter().map(|e| match e {
            AccEvent::Position { p, one_way } => format!("pos {} {:?} {} {} {:?} {:.2} {one_way}", p.symbol, p.side, p.qty, p.mark, p.liq, p.lev),
            AccEvent::Order(o) => format!("order {} {} {:?} {} {} {}", o.symbol, o.id, o.side, o.price, o.qty, o.reduce_only),
            AccEvent::OrderDone { id } => format!("done {id}"),
            AccEvent::Wallet { equity, available } => format!("wallet {equity} {available}"),
            e => format!("{e:?}"),
        }).collect();
        assert_eq!(s, vec![
            "pos XRPUSDT Buy 500 0.4925844 None 2.43 true",
            "pos BTCUSDT Sell 0.04 29900 Some(9572.8) 55.55 true",
            "pos XRPUSDT Buy 0 0 None 0.00 true",
            "pos BTCUSDT Sell 0.04 29900 None 0.00 true",
            "order BTCUSDT 59302619-41d2-4f0b-941f-7e7914760ad3 Sell 10640 304 true",
            "done 660c6b23-8007-48c1-a7c9-4893f4572e8c",
            "done f1",
            "wallet 5000 4800",
        ]);
    }

    #[test]
    fn login_handshake() {
        let k = Keys { key: "KEY".into(), secret: "7zxMEF5p/Z8l2p2U7Ghv6x14Af+Fx+92tPgUdVQ748FOIrEoT9bgT+bTRfXc5pz8na+hL/QdrCVG7bh9KpT0eMTm".into(), extra: None };
        assert!(login_subs(&k, r#"{"event":"info","version":1}"#).is_empty());
        let m = login_subs(&k, r#"{"event":"challenge","message":"c100b894-1729-464d-ace1-52dbce11db42"}"#);
        assert_eq!(m.len(), 3);
        let v: Value = serde_json::from_str(&m[0]).unwrap();
        assert_eq!((v["feed"].as_str(), v["api_key"].as_str(), v["original_challenge"].as_str()), (Some("open_positions"), Some("KEY"), Some("c100b894-1729-464d-ace1-52dbce11db42")));
        assert_eq!(v["signed_challenge"], "4JEpF3ix66GA2B+ooK128Ift4XQVtc137N9yeg4Kqsn9PI0Kpzbysl9M1IeCEdjg0zl00wkVqcsnG4bmnlMb3A==");
    }
}
