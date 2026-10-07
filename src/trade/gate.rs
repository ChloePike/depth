//! Gate.io v4 USDT perpetual futures (settle usdt, e.g. BTC_USDT; sizes in contracts via quanto_multiplier).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out; native symbols / contracts only inside this file.
//!
//! Orders: `size` is a signed integer number of contracts (positive buys, negative sells). Per Gate's
//! create-order docs the same body works in both position modes: single mode closes with
//! `reduce_only` (partial size allowed); dual mode adds long with size > 0, adds short with size < 0,
//! and reduces with `reduce_only=true` where size > 0 reduces the short and size < 0 the long.
//! (`close=true` / `auto_size` are only for size-0 "close the whole position" orders, not used.)
use super::*;
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::sync::Mutex;

const REST: &str = "https://api.gateio.ws/api/v4";
const WS: &str = "wss://fx-ws.gateio.ws/v4/ws/usdt";

/// "BTCUSDT" -> "BTC_USDT"
fn native(symbol: &str) -> String {
    symbol.strip_suffix("USDT").map(|b| format!("{b}_USDT")).unwrap_or_else(|| symbol.to_string())
}

/// "BTC_USDT" -> "BTCUSDT"
fn unified(contract: &str) -> String { contract.replace('_', "") }

/// missing / unparsable -> 0 (push rows leave fields out)
fn z(v: &Value) -> f64 { crate::opt_num(v).unwrap_or(0.0) }

// ---------------------------------------------------------------- contract multipliers

/// quanto_multiplier per native contract (base units per contract), from the public contract list.
static MULTS: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);

fn mult(contract: &str) -> Option<f64> { MULTS.lock().unwrap().as_ref()?.get(contract).copied() }

/// Reload every USDT contract's multiplier (one public request); returns the contract names.
async fn load_contracts() -> Result<Vec<String>> {
    let v = ws::get_json(&format!("{REST}/futures/usdt/contracts")).await?;
    let m: HashMap<String, f64> = v.as_array().into_iter().flatten()
        .filter_map(|c| Some((c["name"].as_str()?.to_string(), crate::opt_num(&c["quanto_multiplier"]).filter(|m| *m > 0.0)?))).collect();
    if m.is_empty() { bail!("gate: empty contract list"); }
    let names = m.keys().cloned().collect();
    *MULTS.lock().unwrap() = Some(m);
    Ok(names)
}

/// multiplier for a contract seen in a REST answer; reloads the list once on a miss (new listing)
async fn mult_or_load(contract: &str) -> Result<f64> {
    if let Some(m) = mult(contract) { return Ok(m); }
    load_contracts().await?;
    mult(contract).ok_or_else(|| anyhow!("gate: no multiplier for {contract}"))
}

// ---------------------------------------------------------------- signed REST

/// APIv4 signature: hex(HMAC_SHA512(secret, METHOD\n/api/v4/path\nquery\nhex(SHA512(body))\ntimestamp))
pub fn sign(secret: &str, method: &str, path: &str, query: &str, body: &str, ts: i64) -> String {
    let body_hash = hex::encode(Sha512::digest(body.as_bytes()));
    hmac512(secret, &format!("{method}\n{path}\n{query}\n{body_hash}\n{ts}"))
}

fn hmac512(secret: &str, msg: &str) -> String {
    let mut m = Hmac::<Sha512>::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(msg.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// Orders and cancels expire after this (X-Gate-Exptime), like Bybit's write receive window.
const EXPIRE_WRITE_MS: i64 = 5_000;

async fn gate(k: &Keys, method: reqwest::Method, path: &str, query: &str, body: Option<&Value>) -> Result<Value> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let ts = crate::now_ms() / 1000;
    let sig = sign(&k.secret, method.as_str(), &format!("/api/v4{path}"), query, &body, ts);
    let url = if query.is_empty() { format!("{REST}{path}") } else { format!("{REST}{path}?{query}") };
    ws::check_paused(&url)?;
    let mut req = ws::http().request(method.clone(), &url).header("KEY", &k.key).header("Timestamp", ts.to_string()).header("SIGN", sig)
        .header("Accept", "application/json").header("Content-Type", "application/json");
    if method != reqwest::Method::GET { req = req.header("X-Gate-Exptime", (crate::now_ms() + EXPIRE_WRITE_MS).to_string()); }
    if !body.is_empty() { req = req.body(body); }
    let r = req.send().await?;
    let st = r.status();
    let body = r.text().await?;
    ws::note_limit(&url, st.as_u16(), None, &body);
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !st.is_success() { bail!("gate {path}: {} ({}, HTTP {})", v["message"].as_str().unwrap_or(&body[..body.len().min(200)]), v["label"].as_str().unwrap_or("?"), st.as_u16()); }
    Ok(v)
}

// ---------------------------------------------------------------- API

pub async fn rules(symbol: &str) -> Result<Rules> {
    let c = ws::get_json(&format!("{REST}/futures/usdt/contracts/{}", native(symbol))).await?;
    let m = crate::opt_num(&c["quanto_multiplier"]).filter(|m| *m > 0.0).ok_or_else(|| anyhow!("gate: no quanto_multiplier for {symbol}"))?;
    // ponytail: integer contracts only; `enable_decimal` contracts would allow finer sizes (step unknown from the API)
    // step = one contract in base units; `place` turns the rounded size back into contracts with it
    Ok(Rules { tick: n(&c["order_price_round"]), step: m, min_qty: z(&c["order_size_min"]).max(1.0) * m, min_notional: 0.0 })
}

/// Last mode read; only used to label which hedge position an open order belongs to.
static DUAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn mode_of(acc: &Value) -> Result<Mode> {
    // position_mode (single / dual / split) supersedes the deprecated in_dual_mode
    match acc["position_mode"].as_str() {
        Some("dual") => Ok(Mode::Hedge),
        Some("single") => Ok(Mode::OneWay),
        Some(m) => bail!("gate: position mode {m} not supported"),
        None => Ok(if acc["in_dual_mode"].as_bool() == Some(true) { Mode::Hedge } else { Mode::OneWay }),
    }
}

/// Account-wide on Gate (`symbol` unused). Read only.
pub async fn mode(k: &Keys, symbol: &str) -> Result<Mode> {
    let _ = symbol;
    let m = mode_of(&gate(k, reqwest::Method::GET, "/futures/usdt/accounts", "", None).await?)?;
    DUAL.store(m == Mode::Hedge, std::sync::atomic::Ordering::Relaxed);
    Ok(m)
}

/// leverage 0 means cross margin: the setting is then cross_leverage_limit
fn lev_of(p: &Value) -> f64 {
    let l = z(&p["leverage"]);
    if l > 0.0 { l } else { let c = z(&p["cross_leverage_limit"]); if c > 0.0 { c } else { z(&p["lever"]) } }
}

pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let c = native(symbol);
    // dual mode positions live under /dual_comp (one row per side); the single endpoint refuses them
    let p = match mode(k, symbol).await? {
        Mode::Hedge => gate(k, reqwest::Method::GET, &format!("/futures/usdt/dual_comp/positions/{c}"), "", None).await?[0].clone(),
        Mode::OneWay => gate(k, reqwest::Method::GET, &format!("/futures/usdt/positions/{c}"), "", None).await?,
    };
    Some(lev_of(&p)).filter(|l| *l > 0.0).ok_or_else(|| anyhow!("gate: no leverage for {symbol}"))
}

/// Create-order body. Identical in both position modes (see the module doc); `contracts` is the
/// unsigned size, signed here by the order side.
pub fn order_body(req: &OrderReq, contracts: i64, px: Option<&str>) -> Result<Value> {
    let size = if req.side() == Side::Buy { contracts } else { -contracts };
    let (price, tif) = match req.kind {
        // Gate: price "0" with tif ioc is a market order
        Kind::Market => ("0".to_string(), "ioc"),
        Kind::Limit { tif, .. } => (px.unwrap_or_default().to_string(), match tif { Tif::Gtc => "gtc", Tif::Ioc => "ioc", Tif::PostOnly => "poc" }),
        Kind::Bbo { .. } => bail!("gate: BBO orders are not supported by the futures order API"),
    };
    Ok(json!({"contract": native(&req.symbol), "size": size, "price": price, "tif": tif, "reduce_only": req.close}))
}

/// The id as a string whether Gate sends it as a number or a string.
fn id_of(o: &Value) -> String {
    o["id_string"].as_str().map(String::from).or_else(|| o["id"].as_str().map(String::from)).unwrap_or_else(|| o["id"].to_string())
}

pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode) -> Result<String> {
    let _ = mode; // same body in both modes
    let (qty, px) = checked(req, r, ref_px)?;
    let contracts = (qty.parse::<f64>()? / r.step).round() as i64;
    if contracts <= 0 { bail!("gate: size {qty} is below one contract"); }
    let v = gate(k, reqwest::Method::POST, "/futures/usdt/orders", "", Some(&order_body(req, contracts, px.as_deref())?)).await?;
    Ok(id_of(&v))
}

/// Order ids are global on Gate; `symbol` is not needed.
pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    let _ = symbol;
    gate(k, reqwest::Method::DELETE, &format!("/futures/usdt/orders/{id}"), "", None).await?;
    Ok(())
}

/// One position row (REST list and the futures.positions push). Dual mode: mode dual_long /
/// dual_short, the short's size is negative.
fn position(p: &Value, mult: f64) -> Position {
    let size = z(&p["size"]);
    let side = match p["mode"].as_str() { Some("dual_long") => Side::Buy, Some("dual_short") => Side::Sell, _ => if size < 0.0 { Side::Sell } else { Side::Buy } };
    let margin = crate::opt_num(&p["margin"]).filter(|m| *m > 0.0).unwrap_or_else(|| z(&p["initial_margin"]));
    Position {
        ex: Exchange::Gate, symbol: unified(p["contract"].as_str().unwrap_or_default()), side, qty: size.abs() * mult,
        entry: z(&p["entry_price"]), mark: z(&p["mark_price"]), liq: crate::opt_num(&p["liq_price"]).filter(|x| *x > 0.0),
        upnl: z(&p["unrealised_pnl"]), lev: lev_of(p), margin,
        // leverage 0 means cross margin on Gate futures
        cross: crate::opt_num(&p["leverage"]).map(|l| l == 0.0),
    }
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    // ponytail: first page only (limit 100 positions); page with offset if an account ever holds more
    let v = gate(k, reqwest::Method::GET, "/futures/usdt/positions", "holding=true&limit=100", None).await?;
    let mut out = vec![];
    for p in v.as_array().into_iter().flatten().filter(|p| z(&p["size"]) != 0.0) {
        out.push(position(p, mult_or_load(p["contract"].as_str().unwrap_or_default()).await?));
    }
    Ok(out)
}

fn order(o: &Value, mult: f64, dual: bool) -> OpenOrder {
    let size = z(&o["size"]);
    let side = if size < 0.0 { Side::Sell } else { Side::Buy };
    let reduce_only = o["is_reduce_only"].as_bool().unwrap_or(false);
    let price = z(&o["price"]);
    let ts = crate::opt_num(&o["create_time_ms"]).unwrap_or_else(|| z(&o["create_time"]) * 1000.0) as i64;
    OpenOrder {
        ex: Exchange::Gate, symbol: unified(o["contract"].as_str().unwrap_or_default()), id: id_of(o), side, price,
        qty: size.abs() * mult, filled: (size.abs() - z(&o["left"]).abs()) * mult,
        kind: if price == 0.0 { "market".into() } else { format!("limit {}", o["tif"].as_str().unwrap_or("gtc")) }, reduce_only, ts,
        // dual mode: a buy adds to the long unless it reduces the short
        pos: dual.then(|| match (reduce_only, side) { (false, s) => s, (true, Side::Buy) => Side::Sell, (true, Side::Sell) => Side::Buy }),
    }
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let v = gate(k, reqwest::Method::GET, "/futures/usdt/orders", "status=open&limit=100", None).await?;
    let dual = DUAL.load(std::sync::atomic::Ordering::Relaxed);
    let mut out = vec![];
    for o in v.as_array().into_iter().flatten() {
        out.push(order(o, mult_or_load(o["contract"].as_str().unwrap_or_default()).await?, dual));
    }
    Ok(out)
}

fn balance_of(a: &Value) -> Balance {
    // total excludes unrealised PnL
    Balance { equity: z(&a["total"]) + z(&a["unrealised_pnl"]), available: z(&a["available"]), ..Default::default() }
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    Ok(balance_of(&gate(k, reqwest::Method::GET, "/futures/usdt/accounts", "", None).await?))
}

// ---------------------------------------------------------------- private stream

/// Gate user id (private channel payloads need it); read once per process.
static UID: Mutex<Option<String>> = Mutex::new(None);

async fn user_id(k: &Keys) -> Result<String> {
    if let Some(u) = UID.lock().unwrap().clone() { return Ok(u); }
    let v = gate(k, reqwest::Method::GET, "/account/detail", "", None).await?;
    let u = v["user_id"].as_i64().map(|u| u.to_string()).or_else(|| v["user_id"].as_str().map(String::from)).ok_or_else(|| anyhow!("gate: no user_id"))?;
    *UID.lock().unwrap() = Some(u.clone());
    Ok(u)
}

/// Signed channel subscribe: SIGN = hex(HMAC_SHA512(secret, "channel=<ch>&event=subscribe&time=<s>"))
fn private_sub(k: &Keys, channel: &str, payload: Value, ts: i64) -> String {
    let sig = hmac512(&k.secret, &format!("channel={channel}&event=subscribe&time={ts}"));
    json!({"time": ts, "channel": channel, "event": "subscribe", "payload": payload, "auth": {"method": "api_key", "KEY": k.key, "SIGN": sig}}).to_string()
}

const PRIVATE: [&str; 3] = ["futures.positions", "futures.orders", "futures.balances"];

/// What one stream frame means. `Err` = drop the session (auth failure is fatal for a while,
/// an unknown contract reconnects to reload the multipliers).
enum Out { Ev(AccEvent), AuthFailed, Reload }

fn on_frame(v: &Value, subscribed: &mut usize) -> Vec<Out> {
    let ch = v["channel"].as_str().unwrap_or_default();
    let mut out = vec![];
    if !v["error"].is_null() {
        if v["error"]["code"].as_i64() == Some(4) { eprintln!("[gate account] auth failed: {}", v["error"]); return vec![Out::AuthFailed]; }
        eprintln!("[gate account] {ch}: {}", v["error"]);
        return out;
    }
    match v["event"].as_str() {
        Some("subscribe") if PRIVATE.contains(&ch) && v["result"]["status"] == "success" => {
            *subscribed += 1;
            if *subscribed == PRIVATE.len() { out.push(Out::Ev(AccEvent::Resync)); }
        }
        Some("update") => {
            let rows: Vec<&Value> = match &v["result"] { Value::Array(a) => a.iter().collect(), r => vec![r] };
            for r in rows {
                let c = r["contract"].as_str().unwrap_or_default();
                match ch {
                    "futures.positions" | "futures.orders" => {
                        let Some(m) = mult(c) else { out.push(Out::Reload); return out };
                        out.push(Out::Ev(if ch == "futures.positions" {
                            AccEvent::Position { p: position(r, m), one_way: r["mode"].as_str().is_none_or(|m| m == "single") }
                        } else if r["status"] == "open" {
                            AccEvent::Order(order(r, m, DUAL.load(std::sync::atomic::Ordering::Relaxed)))
                        } else {
                            AccEvent::OrderDone { id: id_of(r) }
                        }));
                    }
                    // pushes carry the settled balance only, no account totals
                    "futures.balances" => { out.push(Out::Ev(AccEvent::BalanceDirty)); break; }
                    "futures.tickers" => if let Some(mark) = crate::opt_num(&r["mark_price"]) {
                        out.push(Out::Ev(AccEvent::Mark { symbol: unified(c), mark }));
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Gate;
    loop {
        let mut auth_failed = false;
        let res: Result<()> = async {
            let names = load_contracts().await?;
            let uid = user_id(&k).await?;
            // fresh timestamp per connection: Gate rejects stale signed subscribes
            let ts = crate::now_ms() / 1000;
            let mut spec = ws::Spec::new("gate account", WS)
                .sub(private_sub(&k, "futures.positions", json!([uid, "!all"]), ts))
                .sub(private_sub(&k, "futures.orders", json!([uid, "!all"]), ts))
                .sub(private_sub(&k, "futures.balances", json!([uid]), ts))
                .ping(std::time::Duration::from_secs(20), r#"{"time":0,"channel":"futures.ping"}"#);
            // position pushes carry no mark: live marks for upnl from the public tickers
            // ponytail: every USDT contract (~600, like Binance's all-symbol marks); subscribe per position symbol if bandwidth matters
            for chunk in names.chunks(100) {
                spec = spec.sub(json!({"time": ts, "channel": "futures.tickers", "event": "subscribe", "payload": chunk}).to_string());
            }
            let mut subscribed = 0;
            ws::once(&spec, |f| {
                let ws::Frame::Text(s) = f else { return true };
                let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
                for o in on_frame(&v, &mut subscribed) {
                    match o {
                        Out::Ev(e) => { let _ = tx.send((ex, e)); }
                        Out::AuthFailed => { auth_failed = true; return false; }
                        Out::Reload => return false,
                    }
                }
                true
            }).await
        }.await;
        if let Err(e) = res { eprintln!("[gate account] {e:#}"); }
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 5 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_reference() {
        // references computed independently with Python's hashlib / hmac
        let body = r#"{"contract":"BTC_USDT","size":10,"price":"84000.1","tif":"gtc","reduce_only":false}"#;
        assert_eq!(sign("YYYYSECRET", "POST", "/api/v4/futures/usdt/orders", "", body, 1_700_000_000),
            "88cd6a1be0585ef71ce1907168bb7e1181f3137391c7d14989590632b591312be66162a26019dba0f26a4532af9bde9b1df6e0a7c5b059d6c845bc5d72e9f895");
        assert_eq!(sign("YYYYSECRET", "GET", "/api/v4/futures/usdt/orders", "status=open", "", 1_700_000_000),
            "1a1d4afc689908ab8151e21ba90c2ec5dbc03ae88ced6c1f65f34be733ff8b2af645ce178da1427eeca06771c1b454d760585f6c34915c784bbbd7da9c98e68d");
        let k = Keys { key: "K".into(), secret: "YYYYSECRET".into(), extra: None };
        let s: Value = serde_json::from_str(&private_sub(&k, "futures.orders", json!(["1", "!all"]), 1_700_000_000)).unwrap();
        assert_eq!(s["auth"]["SIGN"], "25f832cd0c59bfc447dccc878de185082841e02b50c334b6179e9d018d5ae512d6792f86feb3ff9115a38857857de69953961ab1c218ef90a45cd806a1934cd3");
        assert_eq!((s["auth"]["method"].as_str(), s["time"].as_i64()), (Some("api_key"), Some(1_700_000_000)));
    }

    #[test]
    fn symbols_and_contract_sizes() {
        assert_eq!(native("BTCUSDT"), "BTC_USDT");
        assert_eq!(native("1000PEPEUSDT"), "1000PEPE_USDT");
        assert_eq!(unified("BTC_USDT"), "BTCUSDT");
        // BTC_USDT: 0.0001 BTC per contract; 0.00257 BTC -> 0.0025 -> 25 contracts
        let r = Rules { tick: 0.1, step: 0.0001, min_qty: 0.0001, min_notional: 0.0 };
        let req = OrderReq { symbol: "BTCUSDT".into(), pos: Side::Sell, close: false, kind: Kind::Limit { price: 84000.04, tif: Tif::PostOnly }, qty: 0.00257, client_id: None };
        let (qty, px) = checked(&req, &r, 84000.0).unwrap();
        let contracts = (qty.parse::<f64>().unwrap() / r.step).round() as i64;
        let b = order_body(&req, contracts, px.as_deref()).unwrap();
        assert_eq!((b["contract"].as_str(), b["size"].as_i64(), b["price"].as_str(), b["tif"].as_str()), (Some("BTC_USDT"), Some(-25), Some("84000.0"), Some("poc")));
    }

    #[test]
    fn open_close_bodies() {
        let o = |pos: Side, close: bool, kind: Kind| OrderReq { symbol: "ETHUSDT".into(), pos, close, kind, qty: 1.0, client_id: None };
        // (pos, close) -> size sign; reduce_only marks the close in both modes
        for (pos, close, sign) in [(Side::Buy, false, 1), (Side::Buy, true, -1), (Side::Sell, false, -1), (Side::Sell, true, 1)] {
            let b = order_body(&o(pos, close, Kind::Market), 7, None).unwrap();
            assert_eq!((b["size"].as_i64(), b["reduce_only"].as_bool()), (Some(7 * sign), Some(close)));
            assert_eq!((b["price"].as_str(), b["tif"].as_str()), (Some("0"), Some("ioc")), "market = price 0 + ioc");
            assert!(b.get("close").is_none() && b.get("auto_size").is_none());
        }
        assert!(order_body(&o(Side::Buy, false, Kind::Bbo { queue: true, level: 1 }), 1, None).is_err());
    }

    #[test]
    fn parses_documented_samples() {
        *MULTS.lock().unwrap() = Some([("BTC_USDT".to_string(), 0.0001)].into());
        // modes from /futures/usdt/accounts
        assert_eq!(mode_of(&json!({"position_mode": "dual", "in_dual_mode": true})).unwrap(), Mode::Hedge);
        assert_eq!(mode_of(&json!({"in_dual_mode": false})).unwrap(), Mode::OneWay);
        assert!(mode_of(&json!({"position_mode": "split"})).is_err());
        let b = balance_of(&json!({"total": "1000", "unrealised_pnl": "50", "available": "900"}));
        assert_eq!((b.equity, b.available), (1050.0, 900.0));
        // futures.positions push (Gate WS docs sample): cross (leverage 0) -> lever
        let mut sub = 0;
        let push = json!({"time": 1588212926, "channel": "futures.positions", "event": "update", "result": [{
            "contract": "BTC_USDT", "cross_leverage_limit": 0, "entry_price": 40000.36666661111, "leverage": 0, "liq_price": 0.1,
            "margin": 49.999890611186, "mode": "single", "size": "3", "user": "110xxxxx", "lever": "10"}]});
        let Out::Ev(AccEvent::Position { p, one_way }) = on_frame(&push, &mut sub).remove(0) else { panic!() };
        assert!(one_way);
        assert_eq!((p.symbol.as_str(), p.side, p.lev, p.mark, p.liq), ("BTCUSDT", Side::Buy, 10.0, 0.0, Some(0.1)));
        assert!((p.qty - 0.0003).abs() < 1e-12);
        // dual short row from REST
        let s = position(&json!({"contract": "BTC_USDT", "size": -20, "mode": "dual_short", "leverage": "5", "margin": "10", "entry_price": "80000", "mark_price": "81000", "unrealised_pnl": "-2"}), 0.0001);
        assert_eq!((s.side, s.lev, s.mark, s.upnl), (Side::Sell, 5.0, 81000.0, -2.0));
        assert!((s.qty - 0.002).abs() < 1e-12);
        // futures.orders push: finished -> done, open -> order (sizes in contracts, sign = side)
        let ord = |status: &str| json!({"channel": "futures.orders", "event": "update", "result": [{
            "contract": "BTC_USDT", "id": 4872460, "id_string": "4872460", "is_reduce_only": true, "left": "-4", "price": "40000.4",
            "size": "-10", "status": status, "tif": "gtc", "create_time_ms": 1628736847325u64}]});
        let Out::Ev(AccEvent::OrderDone { id }) = on_frame(&ord("finished"), &mut sub).remove(0) else { panic!() };
        assert_eq!(id, "4872460");
        DUAL.store(true, std::sync::atomic::Ordering::Relaxed);
        let Out::Ev(AccEvent::Order(o)) = on_frame(&ord("open"), &mut sub).remove(0) else { panic!() };
        assert_eq!((o.side, o.reduce_only, o.pos, o.ts), (Side::Sell, true, Some(Side::Buy), 1628736847325));
        assert!((o.qty - 0.001).abs() < 1e-12 && (o.filled - 0.0006).abs() < 1e-12);
        // unknown contract -> reconnect to reload multipliers
        let unknown = json!({"channel": "futures.orders", "event": "update", "result": [{"contract": "NEW_USDT", "status": "open"}]});
        assert!(matches!(on_frame(&unknown, &mut sub)[0], Out::Reload));
        // Resync only once all three private channels confirmed
        let ok = |ch: &str| json!({"channel": ch, "event": "subscribe", "error": null, "result": {"status": "success"}});
        assert!(on_frame(&ok("futures.positions"), &mut sub).is_empty());
        assert!(on_frame(&ok("futures.tickers"), &mut sub).is_empty());
        assert!(on_frame(&ok("futures.orders"), &mut sub).is_empty());
        assert!(matches!(on_frame(&ok("futures.balances"), &mut sub)[0], Out::Ev(AccEvent::Resync)));
        let fail = json!({"channel": "futures.orders", "event": "subscribe", "error": {"code": 4, "message": "authentication fail"}});
        assert!(matches!(on_frame(&fail, &mut sub)[0], Out::AuthFailed));
        let bal = json!({"channel": "futures.balances", "event": "update", "result": [{"balance": 9.99, "change": -0.01, "type": "fee", "currency": "usdt"}]});
        assert!(matches!(on_frame(&bal, &mut sub)[0], Out::Ev(AccEvent::BalanceDirty)));
        let tick = json!({"channel": "futures.tickers", "event": "update", "result": [{"contract": "BTC_USDT", "mark_price": "84000.5"}]});
        let Out::Ev(AccEvent::Mark { symbol, mark }) = on_frame(&tick, &mut sub).remove(0) else { panic!() };
        assert_eq!((symbol.as_str(), mark), ("BTCUSDT", 84000.5));
    }
}
