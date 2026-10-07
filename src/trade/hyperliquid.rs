//! Hyperliquid perps (main dex, USDC-margined; orders signed with an API/agent wallet, EIP-712 + msgpack action hash).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out; native coins ("BTC") and asset indices only inside this file.
//! `Keys.key` = account (main wallet) address, used for every /info query and the private stream;
//! `Keys.secret` = API wallet private key (hex), used only to sign /exchange actions.
//! Signing reproduces hyperliquid-python-sdk `sign_l1_action` (see the tests for its vectors).
use super::*;
use serde::Serialize;
use sha3::{Digest, Keccak256};
use std::collections::{HashMap, HashSet};

fn api() -> &'static str { if testnet() { "https://api.hyperliquid-testnet.xyz" } else { "https://api.hyperliquid.xyz" } }
fn ws_url() -> &'static str { if testnet() { "wss://api.hyperliquid-testnet.xyz/ws" } else { "wss://api.hyperliquid.xyz/ws" } }

/// Market orders are IOC limits this far through the mid (the SDK's DEFAULT_SLIPPAGE).
const SLIPPAGE: f64 = 0.05;
/// Hyperliquid rejects orders below $10 notional.
const MIN_NOTIONAL: f64 = 10.0;

fn f(v: &Value) -> f64 { crate::opt_num(v).unwrap_or(0.0) }
fn user(k: &Keys) -> String { k.key.trim().to_lowercase() }
fn unified(coin: &str) -> String { format!("{}USDT", coin.to_uppercase()) }
/// Spot coins ("@107", "PURR/USDC") show up in first-dex order lists; only perps are traded here.
fn is_perp(coin: &str) -> bool { !coin.starts_with('@') && !coin.contains('/') }

async fn info(body: Value) -> Result<Value> { ws::post_json(&format!("{}/info", api()), &body).await }

// ---------------------------------------------------------------- meta: coin -> asset index

#[derive(Clone)]
struct Asset { coin: String, index: u32, sz_dec: u32 }

/// Perp universe keyed by upper-case coin. Indices are stable (delisted coins keep their slot),
/// so the cache is only refilled when a coin is missing (new listing).
static META: std::sync::Mutex<Option<HashMap<String, Asset>>> = std::sync::Mutex::new(None);

fn load_meta(meta: &Value) {
    let m = meta["universe"].as_array().into_iter().flatten().enumerate().filter_map(|(i, u)| {
        let coin = u["name"].as_str()?.to_string();
        Some((coin.to_uppercase(), Asset { coin, index: i as u32, sz_dec: u["szDecimals"].as_u64()? as u32 }))
    }).collect();
    *META.lock().unwrap() = Some(m);
}

fn base_of(symbol: &str) -> String {
    let s = symbol.to_uppercase();
    s.strip_suffix("USDT").or(s.strip_suffix("USDC")).unwrap_or(&s).to_string()
}

async fn asset(symbol: &str) -> Result<Asset> {
    let base = base_of(symbol);
    let hit = || META.lock().unwrap().as_ref().and_then(|m| m.get(&base).cloned());
    if let Some(a) = hit() { return Ok(a); }
    load_meta(&info(json!({"type": "meta"})).await?);
    hit().ok_or_else(|| anyhow!("hyperliquid: no perp {base}"))
}

// ---------------------------------------------------------------- price / size wire format

/// Decimals a price may carry: 5 significant figures, at most 6 - szDecimals decimals;
/// integers are always valid.
fn px_decimals(px: f64, sz_dec: u32) -> usize {
    let e = px.abs().log10().floor() as i32;
    (4 - e).clamp(0, 6 - sz_dec as i32) as usize
}

/// Smallest price step at `px` (for `Rules.tick`; it changes with the price's magnitude).
fn tick_at(px: f64, sz_dec: u32) -> f64 { 10f64.powi(-(px_decimals(px, sz_dec) as i32)) }

/// Trailing zeros must not reach the signed action ("100.0" and "100" hash differently).
fn wire(s: String) -> String {
    if !s.contains('.') { return s; }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A valid Hyperliquid perp price string, rounded to the nearest allowed value.
fn px_str(px: f64, sz_dec: u32) -> String { wire(format!("{:.*}", px_decimals(px, sz_dec), px)) }

/// Size floored to szDecimals.
fn sz_str(qty: f64, sz_dec: u32) -> String { wire(fmt_step(qty, 10f64.powi(-(sz_dec as i32)), true)) }

// ---------------------------------------------------------------- L1 action signing

#[derive(Serialize)]
struct Limit { tif: &'static str }
#[derive(Serialize)]
struct OrderType { limit: Limit }
/// Field order is the msgpack order the server hashes: a, b, p, s, r, t.
#[derive(Serialize)]
struct OrderWire { a: u32, b: bool, p: String, s: String, r: bool, t: OrderType }
#[derive(Serialize)]
struct OrderAction { #[serde(rename = "type")] ty: &'static str, orders: Vec<OrderWire>, grouping: &'static str }
#[derive(Serialize)]
struct CancelWire { a: u32, o: u64 }
#[derive(Serialize)]
struct CancelAction { #[serde(rename = "type")] ty: &'static str, cancels: Vec<CancelWire> }

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Keccak256::new();
    for p in parts { h.update(p); }
    h.finalize().into()
}

/// keccak(msgpack(action) || nonce BE u64 || vault flag 0 || [0 || expiresAfter BE u64]).
/// No vault / subaccount: the agent trades the master account itself.
fn action_hash(action: &impl Serialize, nonce: u64, expires: Option<u64>) -> Result<[u8; 32]> {
    let mut data = rmp_serde::to_vec_named(action)?;
    data.extend(nonce.to_be_bytes());
    data.push(0);
    if let Some(e) = expires { data.push(0); data.extend(e.to_be_bytes()); }
    Ok(keccak(&[&data]))
}

/// EIP-712 digest of the phantom agent {source, connectionId} under domain
/// {name "Exchange", version "1", chainId 1337, verifyingContract 0x0}.
fn agent_digest(connection_id: &[u8; 32], mainnet: bool) -> [u8; 32] {
    let mut chain = [0u8; 32];
    chain[24..].copy_from_slice(&1337u64.to_be_bytes());
    let domain = keccak(&[
        &keccak(&[b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"]),
        &keccak(&[b"Exchange"]), &keccak(&[b"1"]), &chain, &[0u8; 32],
    ]);
    let source: &[u8] = if mainnet { b"a" } else { b"b" };
    let msg = keccak(&[&keccak(&[b"Agent(string source,bytes32 connectionId)"]), &keccak(&[source]), connection_id]);
    keccak(&[b"\x19\x01", &domain, &msg])
}

/// {r, s, v} as the SDK sends it (eth_utils.to_hex of the integers: no leading zeros).
fn sign_digest(secret: &str, digest: &[u8; 32]) -> Result<Value> {
    let key = hex::decode(secret.trim().trim_start_matches("0x")).map_err(|_| anyhow!("hyperliquid: secret is not a hex private key"))?;
    let sk = k256::ecdsa::SigningKey::from_slice(&key).map_err(|_| anyhow!("hyperliquid: invalid private key"))?;
    let (sig, rid) = sk.sign_prehash_recoverable(digest);
    let (r, s) = sig.split_bytes();
    let h = |b: &[u8]| format!("0x{}", hex::encode(b).trim_start_matches('0'));
    Ok(json!({"r": h(&r), "s": h(&s), "v": 27 + rid.to_byte() as u64}))
}

fn sign_l1(secret: &str, action: &impl Serialize, nonce: u64, expires: Option<u64>, mainnet: bool) -> Result<Value> {
    sign_digest(secret, &agent_digest(&action_hash(action, nonce, expires)?, mainnet))
}

/// Unique, increasing nonce (ms timestamp, bumped when two actions share a millisecond).
fn next_nonce() -> u64 {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = crate::now_ms() as u64;
    let prev = LAST.fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |l| Some(l.max(now - 1) + 1)).unwrap();
    prev.max(now - 1) + 1
}

/// Signs and posts one L1 action; returns `response.data`. Sent once, never retried: a
/// retried order could fill twice. `expiresAfter` rejects an action stuck in transit
/// (same idea as Bybit's write receive window).
async fn exchange(k: &Keys, action: &impl Serialize) -> Result<Value> {
    let nonce = next_nonce();
    let expires = crate::now_ms() as u64 + RECV_WRITE as u64;
    let sig = sign_l1(&k.secret, action, nonce, Some(expires), !testnet())?;
    let body = json!({"action": serde_json::to_value(action)?, "nonce": nonce, "signature": sig, "vaultAddress": null, "expiresAfter": expires});
    let url = format!("{}/exchange", api());
    ws::check_paused(&url)?;
    let r = ws::http().post(&url).json(&body).send().await?;
    let st = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let text = r.text().await?;
    ws::note_limit(&url, st, retry_after, &text);
    let v: Value = serde_json::from_str(&text).map_err(|_| anyhow!("hyperliquid /exchange: HTTP {st}: {}", &text[..text.len().min(200)]))?;
    if v["status"] != "ok" { bail!("hyperliquid: {}", v["response"].as_str().map(String::from).unwrap_or_else(|| v["response"].to_string())); }
    Ok(v["response"]["data"].clone())
}

// ---------------------------------------------------------------- venue API

pub async fn rules(symbol: &str) -> Result<Rules> {
    // meta + mark in one call (weight 20); also refreshes the asset cache
    let v = info(json!({"type": "metaAndAssetCtxs"})).await?;
    load_meta(&v[0]);
    let a = asset(symbol).await?;
    let mark = f(&v[1][a.index as usize]["markPx"]);
    let step = 10f64.powi(-(a.sz_dec as i32));
    // ponytail: tick is valid near the current mark only (5 significant figures); place() re-rounds
    // every price with px_str, so a far-away limit is still sent valid
    Ok(Rules { tick: if mark > 0.0 { tick_at(mark, a.sz_dec) } else { 10f64.powi(-(6 - a.sz_dec as i32)) }, step, min_qty: step, min_notional: MIN_NOTIONAL })
}

/// Hyperliquid perps are always one-way; closing is reduce-only.
pub async fn mode(_k: &Keys, _symbol: &str) -> Result<Mode> { Ok(Mode::OneWay) }

pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let a = asset(symbol).await?;
    let v = info(json!({"type": "activeAssetData", "user": user(k), "coin": a.coin})).await?;
    v["leverage"]["value"].as_f64().ok_or_else(|| anyhow!("hyperliquid: no leverage for {symbol}"))
}

/// Order wire for a request; `mid` prices market orders (IOC through the mid by SLIPPAGE).
fn order_wire(req: &OrderReq, a: &Asset, mid: f64) -> Result<OrderWire> {
    let buy = req.side() == Side::Buy;
    let (px, tif) = match req.kind {
        Kind::Limit { price, tif } => (price, match tif { Tif::Gtc => "Gtc", Tif::Ioc => "Ioc", Tif::PostOnly => "Alo", Tif::Fok => bail!("hyperliquid has no fill-or-kill orders") }),
        Kind::Market => {
            if !(mid > 0.0) { bail!("hyperliquid: no price for a market order"); }
            (mid * if buy { 1.0 + SLIPPAGE } else { 1.0 - SLIPPAGE }, "Ioc")
        }
        Kind::Bbo { .. } => bail!("hyperliquid has no BBO orders"),
        Kind::Stop { .. } | Kind::Trailing { .. } => bail!("hyperliquid: conditional orders are not supported here yet"),
    };
    let s = sz_str(req.qty, a.sz_dec);
    if s.parse::<f64>().unwrap_or(0.0) <= 0.0 { bail!("size below the {} lot", 10f64.powi(-(a.sz_dec as i32))); }
    Ok(OrderWire { a: a.index, b: buy, p: px_str(px, a.sz_dec), s, r: req.close, t: OrderType { limit: Limit { tif } } })
}

pub async fn place(k: &Keys, req: &OrderReq, _r: &Rules, ref_px: f64, _mode: Mode) -> Result<String> {
    let a = asset(&req.symbol).await?;
    // a market order is priced from a fresh mid (weight 2), as the SDK does; the UI's ref_px
    // may be as old as the confirmation dialog
    let mid = if matches!(req.kind, Kind::Market) {
        info(json!({"type": "allMids"})).await.ok().and_then(|m| crate::opt_num(&m[&a.coin])).unwrap_or(ref_px)
    } else { ref_px };
    let action = OrderAction { ty: "order", orders: vec![order_wire(req, &a, mid)?], grouping: "na" };
    let d = exchange(k, &action).await?;
    let st = &d["statuses"][0];
    if let Some(e) = st["error"].as_str() { bail!("hyperliquid: {e}"); }
    let oid = st["resting"]["oid"].as_u64().or(st["filled"]["oid"].as_u64()).ok_or_else(|| anyhow!("hyperliquid: unexpected order status {st}"))?;
    Ok(oid.to_string())
}

pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    let a = asset(symbol).await?;
    let o: u64 = id.parse().map_err(|_| anyhow!("hyperliquid: bad order id {id}"))?;
    let d = exchange(k, &CancelAction { ty: "cancel", cancels: vec![CancelWire { a: a.index, o }] }).await?;
    let st = &d["statuses"][0];
    if st == "success" { Ok(()) } else { bail!("hyperliquid: {}", st["error"].as_str().map(String::from).unwrap_or_else(|| st.to_string())) }
}

/// One `assetPositions[].position` row (REST clearinghouseState and its WS push share the shape).
fn position(p: &Value) -> Position {
    let szi = f(&p["szi"]);
    let qty = szi.abs();
    Position {
        ex: Exchange::Hyperliquid, symbol: unified(p["coin"].as_str().unwrap_or_default()), side: if szi < 0.0 { Side::Sell } else { Side::Buy },
        qty, entry: f(&p["entryPx"]), mark: if qty > 0.0 { f(&p["positionValue"]) / qty } else { 0.0 },
        liq: crate::opt_num(&p["liquidationPx"]).filter(|x| *x > 0.0), upnl: f(&p["unrealizedPnl"]),
        lev: f(&p["leverage"]["value"]), margin: f(&p["marginUsed"]),
        cross: p["leverage"]["type"].as_str().map(|t| t == "cross"),
    }
}

fn positions_of(cs: &Value) -> Vec<Position> {
    cs["assetPositions"].as_array().into_iter().flatten().map(|a| position(&a["position"])).filter(|p| p.qty > 0.0).collect()
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    Ok(positions_of(&info(json!({"type": "clearinghouseState", "user": user(k)})).await?))
}

fn open_order(o: &Value) -> OpenOrder {
    let (orig, left) = (f(&o["origSz"]), f(&o["sz"]));
    OpenOrder {
        ex: Exchange::Hyperliquid, symbol: unified(o["coin"].as_str().unwrap_or_default()), id: o["oid"].to_string(),
        side: if o["side"] == "A" { Side::Sell } else { Side::Buy }, price: f(&o["limitPx"]), qty: orig, filled: (orig - left).max(0.0),
        // WS order updates document fewer fields than frontendOpenOrders; read them when present
        kind: o["orderType"].as_str().unwrap_or("Limit").into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false),
        ts: o["timestamp"].as_i64().unwrap_or(0), pos: None,
    }
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let v = info(json!({"type": "frontendOpenOrders", "user": user(k)})).await?;
    Ok(v.as_array().into_iter().flatten().filter(|o| is_perp(o["coin"].as_str().unwrap_or("@"))).map(open_order).collect())
}

/// Totals of a perp clearinghouse state; None when the perp account holds nothing (unified /
/// portfolio-margin accounts keep collateral in spot).
fn totals(cs: &Value) -> Option<Balance> {
    let equity = f(&cs["marginSummary"]["accountValue"]);
    if equity <= 0.0 { return None; }
    let cross = f(&cs["crossMarginSummary"]["accountValue"]);
    Some(Balance { equity, available: f(&cs["withdrawable"]), uni_mmr: None,
        mm_rate: (cross > 0.0).then(|| f(&cs["crossMaintenanceMarginUsed"]) / cross),
        maint_margin: Some(f(&cs["crossMaintenanceMarginUsed"])), adj_equity: None })
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    if let Some(b) = totals(&info(json!({"type": "clearinghouseState", "user": user(k)})).await?) { return Ok(b); }
    // ponytail: unified-account fallback reads spot USDC only (weight 2); add other collateral
    // tokens if the venue starts margining with them
    let v = info(json!({"type": "spotClearinghouseState", "user": user(k)})).await?;
    let usdc = v["balances"].as_array().into_iter().flatten().find(|b| b["coin"] == "USDC");
    Ok(usdc.map(|b| Balance { equity: f(&b["total"]), available: f(&b["total"]) - f(&b["hold"]), ..Default::default() }).unwrap_or_default())
}

/// Private stream: `clearinghouseState` (full positions + margin snapshot per push) and
/// `orderUpdates`, keyed by the account address; no auth. Resync after the clearinghouse
/// subscription is acknowledged.
pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Hyperliquid;
    let u = user(&k);
    loop {
        let spec = ws::Spec::new("hyperliquid account", ws_url())
            .sub(json!({"method": "subscribe", "subscription": {"type": "clearinghouseState", "user": u, "dex": ""}}).to_string())
            .sub(json!({"method": "subscribe", "subscription": {"type": "orderUpdates", "user": u}}).to_string())
            .ping(std::time::Duration::from_secs(50), r#"{"method":"ping"}"#);
        // symbols with an open position in the last push: a closed one simply disappears
        let mut held: HashSet<String> = HashSet::new();
        let r = ws::once(&spec, |fr| {
            let ws::Frame::Text(s) = fr else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
            let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
            let d = &v["data"];
            match v["channel"].as_str().unwrap_or("") {
                "subscriptionResponse" => if d["subscription"]["type"] == "clearinghouseState" { send(AccEvent::Resync) },
                "clearinghouseState" => {
                    let cs = &d["clearinghouseState"];
                    let ps = positions_of(cs);
                    let now: HashSet<String> = ps.iter().map(|p| p.symbol.clone()).collect();
                    for sym in held.difference(&now) {
                        send(AccEvent::Position { one_way: true, p: Position { ex, symbol: sym.clone(), side: Side::Buy, qty: 0.0, entry: 0.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None } });
                    }
                    for p in ps { send(AccEvent::Position { p, one_way: true }); }
                    held = now;
                    send(match totals(cs) { Some(b) => AccEvent::Wallet { equity: b.equity, available: b.available }, None => AccEvent::BalanceDirty });
                }
                "orderUpdates" => for w in d.as_array().into_iter().flatten().filter(|w| is_perp(w["order"]["coin"].as_str().unwrap_or("@"))) {
                    send(if w["status"] == "open" { AccEvent::Order(open_order(&w["order"])) } else { AccEvent::OrderDone { id: w["order"]["oid"].to_string() } });
                },
                "error" => eprintln!("[hyperliquid account] {d}"),
                _ => {}
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[hyperliquid account] {e:#}"); }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn wire_order(a: u32, p: &str, s: &str, tif: &'static str) -> OrderAction {
        OrderAction { ty: "order", orders: vec![OrderWire { a, b: true, p: p.into(), s: s.into(), r: false, t: OrderType { limit: Limit { tif } } }], grouping: "na" }
    }

    #[test]
    fn action_hash_matches_sdk() {
        // hyperliquid-python-sdk tests/signing_test.py::test_phantom_agent_creation_matches_production
        // (ETH = asset 4, buy 0.0147 @ 1670.1 IOC, nonce 1677777606040)
        let h = action_hash(&wire_order(4, &px_str(1670.1, 4), &sz_str(0.0147, 4), "Ioc"), 1677777606040, None).unwrap();
        assert_eq!(hex::encode(h), "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908");
    }

    #[test]
    fn order_signature_matches_sdk() {
        // signing_test.py::test_l1_action_signing_order_matches: asset 1, buy 100 @ 100 GTC, nonce 0
        let a = wire_order(1, &px_str(100.0, 0), &sz_str(100.0, 0), "Gtc");
        let m = sign_l1(KEY, &a, 0, None, true).unwrap();
        assert_eq!(m, json!({"r": "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e",
                             "s": "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e", "v": 28}));
        let t = sign_l1(KEY, &a, 0, None, false).unwrap();
        assert_eq!(t, json!({"r": "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54",
                             "s": "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5", "v": 27}));
        // expiresAfter changes the hash (SDK appends 0x00 + 8 bytes)
        assert_ne!(action_hash(&a, 0, Some(1)).unwrap(), action_hash(&a, 0, None).unwrap());
    }

    #[test]
    fn tick_rules() {
        // docs "Tick and lot size" examples
        assert_eq!(px_str(1234.56, 0), "1234.6");
        assert_eq!(px_str(123456.7, 5), "123457");
        assert_eq!(px_str(0.0012341, 0), "0.001234");
        assert_eq!(px_str(0.0123456, 1), "0.01235");
        assert_eq!(px_str(87123.456, 5), "87123");
        assert_eq!(px_str(3.0, 4), "3");
        assert_eq!(px_str(9.99996, 2), "10");
        assert!((tick_at(87_000.0, 5) - 1.0).abs() < 1e-12);
        assert!((tick_at(2_500.0, 4) - 0.1).abs() < 1e-12);
        assert!((tick_at(0.5, 0) - 0.00001).abs() < 1e-15);
        assert_eq!(sz_str(0.00123456, 5), "0.00123");
        assert_eq!(sz_str(100.0, 0), "100");
        assert_eq!(sz_str(0.30000000000000004, 1), "0.3");
    }

    #[test]
    fn orders_from_intent() {
        let a = Asset { coin: "BTC".into(), index: 0, sz_dec: 5 };
        let req = |pos, close, kind| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty: 0.001, client_id: None };
        let w = order_wire(&req(Side::Buy, false, Kind::Market), &a, 100_000.0).unwrap();
        assert_eq!((w.b, w.p.as_str(), w.s.as_str(), w.r, w.t.limit.tif), (true, "105000", "0.001", false, "Ioc"));
        // close long = reduce-only sell through the mid
        let w = order_wire(&req(Side::Buy, true, Kind::Market), &a, 100_000.0).unwrap();
        assert_eq!((w.b, w.p.as_str(), w.r), (false, "95000", true));
        let w = order_wire(&req(Side::Sell, false, Kind::Limit { price: 101_234.7, tif: Tif::PostOnly }), &a, 0.0).unwrap();
        assert_eq!((w.b, w.p.as_str(), w.t.limit.tif), (false, "101235", "Alo"));
        assert!(order_wire(&req(Side::Buy, false, Kind::Bbo { queue: true, level: 1 }), &a, 1.0).is_err());
        assert!(order_wire(&req(Side::Buy, false, Kind::Market), &a, 0.0).is_err());
        assert_eq!(base_of("BTCUSDT"), "BTC");
        assert_eq!(unified("kPEPE"), "KPEPEUSDT");
        let n1 = next_nonce();
        assert!(next_nonce() > n1);
    }
}
