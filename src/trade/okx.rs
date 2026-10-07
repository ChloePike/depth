//! OKX v5 USDT-margined perpetual swaps (instType SWAP, e.g. BTC-USDT-SWAP; sizes in contracts via ctVal).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out; native symbols / contracts only inside this file.
//! Orders use tdMode cross (valid for swaps in Futures, Multi-currency and Portfolio margin modes).
use super::*;
use base64::Engine;
use std::collections::HashMap;

const REST: &str = "https://www.okx.com";

fn ws_url() -> &'static str { if testnet() { "wss://wspap.okx.com:8443/ws/v5/private" } else { "wss://ws.okx.com:8443/ws/v5/private" } }

/// "BTCUSDT" -> "BTC-USDT-SWAP"
pub fn native(symbol: &str) -> String { format!("{}-USDT-SWAP", symbol.strip_suffix("USDT").unwrap_or(symbol)) }

/// "BTC-USDT-SWAP" -> "BTCUSDT"; None for anything but a USDT swap
pub fn unified(inst: &str) -> Option<String> { inst.strip_suffix("-USDT-SWAP").map(|b| format!("{b}USDT")) }

/// Contract metadata: `ct` = base units per contract (ctVal * ctMult); lot / min size in contracts.
#[derive(Clone, Copy, Debug)]
struct Inst { ct: f64, lot: f64, min: f64, tick: f64 }

/// Every SWAP instrument, keyed by instId. Loaded with one public request; reloaded when a symbol
/// is missing and on every private-stream (re)connect.
// ponytail: tick / lot changes are only seen on reload; refetch in rules() if OKX starts changing them often
static INSTS: std::sync::Mutex<Option<HashMap<String, Inst>>> = std::sync::Mutex::new(None);

async fn load_insts() -> Result<()> {
    let v = ws::get_json(&format!("{REST}/api/v5/public/instruments?instType=SWAP")).await?;
    if v["code"] != "0" { bail!("okx instruments: {} {}", v["code"], v["msg"]); }
    let m: HashMap<String, Inst> = v["data"].as_array().into_iter().flatten().filter_map(|i| {
        let ct = crate::opt_num(&i["ctVal"])? * crate::opt_num(&i["ctMult"]).unwrap_or(1.0);
        Some((i["instId"].as_str()?.to_string(), Inst { ct, lot: crate::opt_num(&i["lotSz"])?, min: crate::opt_num(&i["minSz"])?, tick: crate::opt_num(&i["tickSz"])? }))
    }).filter(|(_, i)| i.ct > 0.0 && i.lot > 0.0).collect();
    if m.is_empty() { bail!("okx instruments: empty list"); }
    *INSTS.lock().unwrap() = Some(m);
    Ok(())
}

fn cached(inst: &str) -> Option<Inst> { INSTS.lock().unwrap().as_ref()?.get(inst).copied() }

async fn inst(inst: &str) -> Result<Inst> {
    if let Some(i) = cached(inst) { return Ok(i); }
    load_insts().await?;
    cached(inst).ok_or_else(|| anyhow!("okx: {inst} not listed"))
}

/// OK-ACCESS-TIMESTAMP: ISO 8601 UTC with milliseconds, e.g. 2020-12-08T09:08:57.715Z
fn iso(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// base64(HMAC-SHA256(secret, ts + METHOD + requestPath(+query) + body))
pub fn sign(secret: &str, ts: &str, method: &str, path: &str, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(format!("{ts}{method}{path}{body}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes())
}

/// Signed REST call; `path` includes the query string. Returns `data`.
async fn okx(k: &Keys, post: Option<Value>, path: &str) -> Result<Value> {
    let url = format!("{REST}{path}");
    ws::check_paused(&url)?;
    let ts = iso(crate::now_ms());
    let body = post.as_ref().map(|b| b.to_string()).unwrap_or_default();
    let method = if post.is_some() { "POST" } else { "GET" };
    let mut req = if post.is_some() { ws::http().post(&url).header("Content-Type", "application/json").body(body.clone()) } else { ws::http().get(&url) };
    if testnet() { req = req.header("x-simulated-trading", "1"); }
    let r = req.header("OK-ACCESS-KEY", &k.key).header("OK-ACCESS-SIGN", sign(&k.secret, &ts, method, path, &body))
        .header("OK-ACCESS-TIMESTAMP", &ts).header("OK-ACCESS-PASSPHRASE", k.extra.as_deref().unwrap_or_default()).send().await?;
    let st = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let text = r.text().await?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    // OKX answers its rate limit (50011) with HTTP 200 as well as 429
    ws::note_limit(&url, if v["code"] == "50011" { 429 } else { st }, retry_after, &text);
    if v.is_null() { bail!("okx {path}: HTTP {st}: {}", &text[..text.len().min(200)]); }
    if v["code"] != "0" {
        // order endpoints put the reason in data[0].sCode / sMsg (top-level code "1")
        let d = &v["data"][0];
        let detail = d["sMsg"].as_str().filter(|s| !s.is_empty()).map(|s| format!(": {s} ({})", d["sCode"])).unwrap_or_default();
        bail!("okx {path}: {} ({}){detail}", v["msg"].as_str().unwrap_or("?"), v["code"]);
    }
    Ok(v["data"].clone())
}

/// empty strings ("" = not applicable) read as 0
fn z(v: &Value) -> f64 { crate::opt_num(v).unwrap_or(0.0) }

pub async fn rules(symbol: &str) -> Result<Rules> {
    let i = inst(&native(symbol)).await?;
    Ok(Rules { tick: i.tick, step: i.lot * i.ct, min_qty: i.min * i.ct, min_notional: 0.0 })
}

/// Read only: account config posMode (never changed from here).
pub async fn mode(k: &Keys, _symbol: &str) -> Result<Mode> {
    let d = okx(k, None, "/api/v5/account/config").await?;
    let c = &d[0];
    if c["acctLv"] == "1" { bail!("okx: account is in Spot mode, switch to Futures / Multi-currency mode to trade swaps"); }
    match c["posMode"].as_str() {
        Some("long_short_mode") => Ok(Mode::Hedge),
        Some("net_mode") => Ok(Mode::OneWay),
        m => bail!("okx: unknown posMode {m:?}"),
    }
}

/// Cross-margin leverage for the symbol (hedge mode lists long and short; they are set together on cross).
pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let d = okx(k, None, &format!("/api/v5/account/leverage-info?instId={}&mgnMode=cross", native(symbol))).await?;
    crate::opt_num(&d[0]["lever"]).ok_or_else(|| anyhow!("okx: no leverage for {symbol}"))
}

/// Order body. Hedge: posSide long/short picks the position (reduceOnly is net-mode only);
/// one-way: posSide net, reduceOnly marks a close. `sz` is in contracts.
fn order_body(req: &OrderReq, mode: Mode, sz: &str, px: Option<&str>) -> Result<Value> {
    let mut b = json!({"instId": native(&req.symbol), "tdMode": "cross", "side": if req.side() == Side::Buy { "buy" } else { "sell" }, "sz": sz});
    match req.kind {
        Kind::Market => b["ordType"] = "market".into(),
        Kind::Limit { tif, .. } => {
            b["ordType"] = match tif { Tif::Gtc => "limit", Tif::Ioc => "ioc", Tif::PostOnly => "post_only" }.into();
            b["px"] = px.ok_or_else(|| anyhow!("limit order without price"))?.into();
        }
        Kind::Bbo { .. } => bail!("okx has no BBO order type"),
    }
    match mode {
        Mode::Hedge => b["posSide"] = if req.pos == Side::Buy { "long" } else { "short" }.into(),
        Mode::OneWay => {
            b["posSide"] = "net".into();
            if req.close { b["reduceOnly"] = true.into(); }
        }
    }
    Ok(b)
}

/// Base-unit size (already a multiple of the base step) -> contracts string on the lot grid.
fn contracts(base: &str, i: Inst) -> Result<String> { Ok(fmt_step(base.parse::<f64>()? / i.ct, i.lot, false)) }

pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode) -> Result<String> {
    if matches!(req.kind, Kind::Bbo { .. }) { bail!("okx has no BBO order type"); }
    let (qty, px) = checked(req, r, ref_px)?;
    let sz = contracts(&qty, inst(&native(&req.symbol)).await?)?;
    let d = okx(k, Some(order_body(req, mode, &sz, px.as_deref())?), "/api/v5/trade/order").await?;
    let o = &d[0];
    if o["sCode"] != "0" { bail!("okx order: {} ({})", o["sMsg"].as_str().unwrap_or("?"), o["sCode"]); }
    Ok(o["ordId"].as_str().unwrap_or_default().to_string())
}

pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    let d = okx(k, Some(json!({"instId": native(symbol), "ordId": id})), "/api/v5/trade/cancel-order").await?;
    if d[0]["sCode"] != "0" { bail!("okx cancel: {} ({})", d[0]["sMsg"].as_str().unwrap_or("?"), d[0]["sCode"]); }
    Ok(())
}

/// One position row (REST and the private `positions` push share the shape). `pos` is in
/// contracts, signed in net mode. Returns (position, one_way); None for non-USDT swaps or an
/// instrument missing from `ct`.
fn position(p: &Value, ct: impl Fn(&str) -> Option<Inst>) -> Option<(Position, bool)> {
    let id = p["instId"].as_str()?;
    let symbol = unified(id)?;
    let i = ct(id)?;
    let pos = z(&p["pos"]);
    let side = match p["posSide"].as_str() { Some("long") => Side::Buy, Some("short") => Side::Sell, _ => if pos < 0.0 { Side::Sell } else { Side::Buy } };
    // cross positions carry imr, isolated ones margin
    let margin = crate::opt_num(&p["imr"]).filter(|x| *x > 0.0).unwrap_or_else(|| z(&p["margin"]));
    // lever is empty for cross positions under Portfolio margin
    let lev = crate::opt_num(&p["lever"]).filter(|x| *x > 0.0).unwrap_or_else(|| if margin > 0.0 { z(&p["notionalUsd"]).abs() / margin } else { 0.0 });
    Some((Position { ex: Exchange::Okx, symbol, side, qty: pos.abs() * i.ct, entry: z(&p["avgPx"]), mark: z(&p["markPx"]),
        liq: crate::opt_num(&p["liqPx"]).filter(|x| *x > 0.0), upnl: z(&p["upl"]), lev, margin,
        cross: p["mgnMode"].as_str().map(|m| m == "cross") }, p["posSide"] == "net"))
}

fn order(o: &Value, ct: impl Fn(&str) -> Option<Inst>) -> Option<OpenOrder> {
    let id = o["instId"].as_str()?;
    let symbol = unified(id)?;
    let i = ct(id)?;
    Some(OpenOrder {
        ex: Exchange::Okx, symbol, id: o["ordId"].as_str().unwrap_or_default().into(), side: if o["side"] == "sell" { Side::Sell } else { Side::Buy },
        price: z(&o["px"]), qty: z(&o["sz"]) * i.ct, filled: z(&o["accFillSz"]) * i.ct, kind: o["ordType"].as_str().unwrap_or_default().into(),
        reduce_only: o["reduceOnly"] == "true" || o["reduceOnly"] == true, ts: z(&o["cTime"]) as i64,
        pos: match o["posSide"].as_str() { Some("long") => Some(Side::Buy), Some("short") => Some(Side::Sell), _ => None },
    })
}

/// Account totals from a balance row (REST and the `account` push). Available: the account-level
/// availEq (Multi-currency / Portfolio margin), else USDT's availEq / availBal (Futures mode).
/// None when the row has no usable available figure (a push that omits USDT).
fn balance_of(a: &Value) -> Option<Balance> {
    let usdt = a["details"].as_array().into_iter().flatten().find(|c| c["ccy"] == "USDT");
    let available = crate::opt_num(&a["availEq"])
        .or_else(|| usdt.and_then(|c| crate::opt_num(&c["availEq"]).or(crate::opt_num(&c["availBal"]))))?;
    // liquidation when adjEq <= mmr: mmr / adjEq is 0..1 like Bybit's accountMMRate
    let (mmr, adj) = (z(&a["mmr"]), z(&a["adjEq"]));
    Some(Balance { equity: z(&a["totalEq"]), available, uni_mmr: None, mm_rate: (mmr > 0.0 && adj > 0.0).then(|| mmr / adj), maint_margin: Some(mmr), adj_equity: (adj > 0.0).then_some(adj) })
}

/// Contract sizes for positions / orders on symbols never passed to `rules`.
async fn ensure_insts() -> Result<()> { if INSTS.lock().unwrap().is_none() { load_insts().await?; } Ok(()) }

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    ensure_insts().await?;
    let d = okx(k, None, "/api/v5/account/positions?instType=SWAP").await?;
    let rows = d.as_array().cloned().unwrap_or_default();
    // a position opened on a symbol listed after the cache was loaded
    if rows.iter().any(|p| p["instId"].as_str().is_some_and(|i| unified(i).is_some() && cached(i).is_none())) { load_insts().await?; }
    Ok(rows.iter().filter_map(|p| position(p, cached)).map(|(p, _)| p).filter(|p| p.qty > 0.0).collect())
}

/// Pending swap orders.
// ponytail: first page only (100 orders); paginate with `after` if anyone keeps more open
pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    ensure_insts().await?;
    let d = okx(k, None, "/api/v5/trade/orders-pending?instType=SWAP").await?;
    let rows = d.as_array().cloned().unwrap_or_default();
    if rows.iter().any(|o| o["instId"].as_str().is_some_and(|i| unified(i).is_some() && cached(i).is_none())) { load_insts().await?; }
    Ok(rows.iter().filter_map(|o| order(o, cached)).collect())
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let d = okx(k, None, "/api/v5/account/balance").await?;
    balance_of(&d[0]).ok_or_else(|| anyhow!("okx balance: no available equity in {}", &d[0]))
}

/// WS login: sign over unix SECONDS + "GET" + "/users/self/verify".
fn login(k: &Keys, secs: i64) -> String {
    let ts = secs.to_string();
    json!({"op": "login", "args": [{"apiKey": k.key, "passphrase": k.extra.as_deref().unwrap_or_default(), "timestamp": ts,
        "sign": sign(&k.secret, &ts, "GET", "/users/self/verify", "")}]}).to_string()
}

/// Private account stream: push `AccEvent`s forever, reconnecting; send `AccEvent::Resync` once
/// subscribed (the engine then takes a REST snapshot). Positions push every ~5s with markPx, so no
/// separate mark feed is needed.
pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Okx;
    loop {
        // fresh contract sizes every connection: the handler cannot await a reload
        if let Err(e) = load_insts().await {
            eprintln!("[okx account] {e:#}");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        // ponytail: subscribe is pipelined right behind login (ws::Spec sends all subs at once);
        // OKX may answer 60011 "Please log in" if the login is not processed yet, then we reconnect.
        // Upgrade: a ws.rs hook that sends follow-up messages after the login ack.
        let spec = ws::Spec::new("okx account", ws_url()).login_first()
            .sub(login(&k, crate::now_ms() / 1000))
            .sub(json!({"op": "subscribe", "args": [{"channel": "positions", "instType": "SWAP"}, {"channel": "orders", "instType": "SWAP"}, {"channel": "account"}]}).to_string())
            .ping(std::time::Duration::from_secs(25), "ping");
        let (mut logged_in, mut auth_failed, mut acks, mut missing) = (false, false, 0, false);
        let r = ws::once(&spec, |f| {
            let ws::Frame::Text(s) = f else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true }; // "pong"
            let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
            match v["event"].as_str() {
                Some("login") => { logged_in = v["code"] == "0"; if !logged_in { auth_failed = true; eprintln!("[okx account] login failed: {s}"); return false; } return true; }
                Some("error") => {
                    eprintln!("[okx account] {s}");
                    auth_failed = !logged_in && v["code"] != "60011";
                    return false;
                }
                Some("subscribe") => { acks += 1; if acks == 3 { send(AccEvent::Resync); } return true; }
                _ => {}
            }
            let data = v["data"].as_array().cloned().unwrap_or_default();
            match v["arg"]["channel"].as_str() {
                Some("positions") => for p in &data {
                    if p["instId"].as_str().is_some_and(|i| unified(i).is_some() && cached(i).is_none()) { missing = true; return false; }
                    if let Some((p, one_way)) = position(p, cached) { send(AccEvent::Position { p, one_way }); }
                },
                Some("orders") => for o in &data {
                    if o["instId"].as_str().is_none_or(|i| unified(i).is_none()) { continue; }
                    if matches!(o["state"].as_str(), Some("live" | "partially_filled")) {
                        let Some(o) = order(o, cached) else { missing = true; return false };
                        send(AccEvent::Order(o));
                    } else {
                        send(AccEvent::OrderDone { id: o["ordId"].as_str().unwrap_or_default().into() });
                    }
                },
                Some("account") => for a in &data {
                    send(match balance_of(a) { Some(b) => AccEvent::Wallet { equity: b.equity, available: b.available }, None => AccEvent::BalanceDirty });
                },
                _ => {}
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[okx account] {e:#}"); }
        if missing { eprintln!("[okx account] unknown instrument, reloading instruments"); }
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 2 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: Inst = Inst { ct: 0.01, lot: 0.01, min: 0.01, tick: 0.1 };
    fn ct(id: &str) -> Option<Inst> { (id == "BTC-USDT-SWAP").then_some(BTC) }

    #[test]
    fn signature_and_timestamp() {
        // docs example secret / timestamp / path; reference values from Python hmac + base64
        assert_eq!(iso(1_607_418_537_715), "2020-12-08T09:08:57.715Z");
        assert_eq!(iso(1_709_251_199_001), "2024-02-29T23:59:59.001Z");
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        let s = "22582BD0CFF14C41EDBF1AB98506286D";
        assert_eq!(sign(s, "2020-12-08T09:08:57.715Z", "GET", "/api/v5/account/balance?ccy=BTC", ""), "HiZhvSfMtWJA3uUIVXV3a/bSXNPCWvYFXoGCVS8V4zY=");
        assert_eq!(sign(s, "2020-12-08T09:08:57.715Z", "POST", "/api/v5/account/set-leverage", r#"{"instId":"BTC-USDT","lever":"5","mgnMode":"isolated"}"#),
                   "eCnnCgWLjlQ9XnpUkrcny3qNq3WW/81KNrDr/XR6Xv8=");
        assert_eq!(sign(s, "1704876947", "GET", "/users/self/verify", ""), "5/36BgGV6m/6pmdc20zdqk0mzF5ZalmzzPD2fo3wavU=");
    }

    #[test]
    fn symbols_and_contracts() {
        assert_eq!(native("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(unified("BTC-USDT-SWAP").as_deref(), Some("BTCUSDT"));
        assert_eq!(unified("BTC-USDC-SWAP"), None);
        // 0.0123 BTC = 1.23 contracts of 0.01 BTC
        assert_eq!(contracts("0.0123", BTC).unwrap(), "1.23");
        assert_eq!(contracts("1", Inst { ct: 10.0, lot: 1.0, min: 1.0, tick: 0.0001 }).unwrap(), "0");
        assert_eq!(contracts("30", Inst { ct: 10.0, lot: 1.0, min: 1.0, tick: 0.0001 }).unwrap(), "3");
        // rules in base units: step 0.0001 BTC; the trade.rs rounding then lands on the lot grid
        let r = Rules { tick: BTC.tick, step: BTC.lot * BTC.ct, min_qty: BTC.min * BTC.ct, min_notional: 0.0 };
        let req = OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind: Kind::Market, qty: 0.012345, client_id: None };
        let (q, _) = checked(&req, &r, 100_000.0).unwrap();
        assert_eq!(q, "0.0123");
        assert_eq!(contracts(&q, BTC).unwrap(), "1.23");
    }

    #[test]
    fn order_bodies() {
        let o = |pos, close, kind| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty: 1.0, client_id: None };
        for (pos, close, side) in [(Side::Buy, false, "buy"), (Side::Buy, true, "sell"), (Side::Sell, false, "sell"), (Side::Sell, true, "buy")] {
            let h = order_body(&o(pos, close, Kind::Market), Mode::Hedge, "2", None).unwrap();
            assert_eq!((h["instId"].as_str(), h["side"].as_str(), h["posSide"].as_str(), h["ordType"].as_str(), h["sz"].as_str(), h["tdMode"].as_str()),
                       (Some("BTC-USDT-SWAP"), Some(side), Some(if pos == Side::Buy { "long" } else { "short" }), Some("market"), Some("2"), Some("cross")));
            assert!(h.get("reduceOnly").is_none(), "reduceOnly is net-mode only");
            let w = order_body(&o(pos, close, Kind::Market), Mode::OneWay, "2", None).unwrap();
            assert_eq!((w["side"].as_str(), w["posSide"].as_str(), w["reduceOnly"].as_bool()), (Some(side), Some("net"), close.then_some(true)));
        }
        let l = order_body(&o(Side::Buy, false, Kind::Limit { price: 1.0, tif: Tif::PostOnly }), Mode::Hedge, "1", Some("86000.1")).unwrap();
        assert_eq!((l["ordType"].as_str(), l["px"].as_str()), (Some("post_only"), Some("86000.1")));
        let l = order_body(&o(Side::Buy, false, Kind::Limit { price: 1.0, tif: Tif::Ioc }), Mode::Hedge, "1", Some("1")).unwrap();
        assert_eq!(l["ordType"].as_str(), Some("ioc"));
        assert!(order_body(&o(Side::Buy, false, Kind::Bbo { queue: true, level: 1 }), Mode::Hedge, "1", None).is_err());
    }

    #[test]
    fn parse_positions_orders_balance() {
        // shapes from the v5 docs (GET /account/positions, positions / orders / account pushes)
        let net: Value = serde_json::from_str(r#"{"instId":"BTC-USDT-SWAP","posSide":"net","pos":"-150","avgPx":"62961.4","markPx":"62891.9",
            "liqPx":"91000.5","upl":"10.4","lever":"5","imr":"18867.57","margin":"","mgnMode":"cross","notionalUsd":"94337.85"}"#).unwrap();
        let (p, one_way) = position(&net, ct).unwrap();
        assert!(one_way);
        assert_eq!((p.symbol.as_str(), p.side, p.entry, p.liq, p.lev), ("BTCUSDT", Side::Sell, 62961.4, Some(91000.5), 5.0));
        assert!((p.qty - 1.5).abs() < 1e-12 && (p.margin - 18867.57).abs() < 1e-9);
        // hedge row, isolated margin, PM-style empty lever, liqPx empty
        let long: Value = serde_json::from_str(r#"{"instId":"BTC-USDT-SWAP","posSide":"long","pos":"10","avgPx":"60000","markPx":"61000",
            "liqPx":"","upl":"10","lever":"","imr":"","margin":"122","mgnMode":"isolated","notionalUsd":"6100"}"#).unwrap();
        let (p, one_way) = position(&long, ct).unwrap();
        assert!(!one_way);
        assert_eq!((p.side, p.liq, p.margin, p.lev), (Side::Buy, None, 122.0, 50.0));
        assert!((p.qty - 0.1).abs() < 1e-12);
        // a closed position pushes pos "0": qty 0 removes it
        let closed: Value = serde_json::from_str(r#"{"instId":"BTC-USDT-SWAP","posSide":"net","pos":"0","avgPx":"","markPx":"61000"}"#).unwrap();
        assert_eq!(position(&closed, ct).unwrap().0.qty, 0.0);
        assert!(position(&serde_json::json!({"instId":"BTC-USDC-SWAP","pos":"1"}), ct).is_none());

        let o: Value = serde_json::from_str(r#"{"accFillSz":"2","cTime":"1724733617998","instId":"BTC-USDT-SWAP","ordId":"1752588852617379840",
            "ordType":"post_only","posSide":"short","px":"63013.5","reduceOnly":"true","side":"buy","state":"partially_filled","sz":"5"}"#).unwrap();
        let o = order(&o, ct).unwrap();
        assert_eq!((o.symbol.as_str(), o.id.as_str(), o.side, o.price, o.kind.as_str(), o.reduce_only, o.ts, o.pos),
                   ("BTCUSDT", "1752588852617379840", Side::Buy, 63013.5, "post_only", true, 1724733617998, Some(Side::Sell)));
        assert!((o.qty - 0.05).abs() < 1e-12 && (o.filled - 0.02).abs() < 1e-12);

        // Futures mode: account-level availEq / mmr / adjEq empty, USDT detail carries availEq
        let fut: Value = serde_json::from_str(r#"{"totalEq":"1000.5","availEq":"","adjEq":"","mmr":"","details":[{"ccy":"BTC","availEq":"0.1"},{"ccy":"USDT","availEq":"800.25","availBal":"900"}]}"#).unwrap();
        let b = balance_of(&fut).unwrap();
        assert_eq!((b.equity, b.available, b.mm_rate), (1000.5, 800.25, None));
        // Multi-currency / PM: account-level figures
        let pm: Value = serde_json::from_str(r#"{"totalEq":"5000","availEq":"4000","adjEq":"4800","mmr":"48","details":[]}"#).unwrap();
        let b = balance_of(&pm).unwrap();
        assert_eq!((b.available, b.mm_rate), (4000.0, Some(0.01)));
        // a push without USDT and without account-level availEq: refetch instead of guessing
        assert!(balance_of(&serde_json::json!({"totalEq":"1","availEq":"","details":[{"ccy":"BTC","availEq":"1"}]})).is_none());
    }
}
