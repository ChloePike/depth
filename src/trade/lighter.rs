//! Lighter perps (zkLighter; USDC-margined; transactions signed with the account's API key).
//! Same contract as the Bybit/Binance code in trade.rs: unified symbols ("BTCUSDT") and base-unit
//! sizes in and out; native market ids and integer price/size only inside this file.
//!
//! Keys: key = "<account_index>:<api_key_index>", secret = the API key private key (40 bytes hex).
//! Transactions (create / cancel order) and auth tokens are Schnorr signatures over the ECgFp5 curve
//! with Poseidon2 (Goldilocks) hashing, ported in `zk` below from elliottech/poseidon_crypto +
//! lighter-go and checked byte-for-byte against vectors generated with the official Go code.
//! Lighter accounts are one-way (one net position per market).
use super::*;
use std::sync::Mutex;

fn host() -> &'static str { if testnet() { "https://testnet.zklighter.elliot.ai" } else { "https://mainnet.zklighter.elliot.ai" } }
fn ws_url() -> &'static str { if testnet() { "wss://testnet.zklighter.elliot.ai/stream" } else { "wss://mainnet.zklighter.elliot.ai/stream" } }
fn chain_id() -> u64 { if testnet() { 300 } else { 304 } }

const TX_CREATE_ORDER: u8 = 14;
const TX_CANCEL_ORDER: u8 = 15;
/// Worst price a market order accepts, as a fraction away from the reference price (Lighter market
/// orders are IOC with a limit price).
const MARKET_SLIPPAGE: f64 = 0.02;

// ---------------------------------------------------------------- markets

#[derive(Clone, Debug)]
struct Mkt { id: i64, base: String, size_dp: u32, price_dp: u32, min_base: f64, min_quote: f64 }

static MKTS: Mutex<Vec<Mkt>> = Mutex::new(Vec::new());

fn parse_markets(v: &Value) -> Vec<Mkt> {
    v["order_books"].as_array().into_iter().flatten().filter(|o| o["market_type"] == "perp" && o["status"] == "active").map(|o| Mkt {
        id: o["market_id"].as_i64().unwrap_or(-1), base: o["symbol"].as_str().unwrap_or_default().into(),
        size_dp: o["supported_size_decimals"].as_u64().unwrap_or(0) as u32, price_dp: o["supported_price_decimals"].as_u64().unwrap_or(0) as u32,
        min_base: n(&o["min_base_amount"]), min_quote: n(&o["min_quote_amount"]),
    }).collect()
}

/// Perp markets, fetched once per process (again when a lookup misses: new listing).
async fn markets(refresh: bool) -> Result<Vec<Mkt>> {
    if !refresh { let m = MKTS.lock().unwrap(); if !m.is_empty() { return Ok(m.clone()); } }
    let list = parse_markets(&ws::get_json(&format!("{}/api/v1/orderBooks", host())).await?);
    *MKTS.lock().unwrap() = list.clone();
    Ok(list)
}

/// "BTCUSDT" / "BTCUSDC" / "BTC" -> "BTC"
fn base_of(symbol: &str) -> &str {
    ["USDT", "USDC", "USD"].iter().find_map(|q| symbol.strip_suffix(q)).filter(|b| !b.is_empty()).unwrap_or(symbol)
}

async fn market(symbol: &str) -> Result<Mkt> {
    let base = base_of(symbol);
    if let Some(m) = markets(false).await?.into_iter().find(|m| m.base == base) { return Ok(m); }
    markets(true).await?.into_iter().find(|m| m.base == base).ok_or_else(|| anyhow!("lighter: no {base} perp market"))
}

fn sym_of(mkts: &[Mkt], id: i64) -> String {
    mkts.iter().find(|m| m.id == id).map(|m| symbol(&m.base)).unwrap_or_else(|| format!("LIGHTER#{id}"))
}

fn rules_of(m: &Mkt) -> Rules {
    Rules { tick: 10f64.powi(-(m.price_dp as i32)), step: 10f64.powi(-(m.size_dp as i32)), min_qty: m.min_base, min_notional: m.min_quote }
}

/// Exact decimal string -> integer scaled by 10^dp ("0.0015", 5 -> 150). Rejects more digits than dp.
fn scaled(s: &str, dp: u32) -> Result<i64> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let frac = frac.trim_end_matches('0');
    if frac.len() > dp as usize { bail!("{s} has more than {dp} decimals"); }
    let digits = format!("{int}{frac:0<width$}", width = dp as usize);
    digits.parse::<i64>().map_err(|e| anyhow!("bad number {s}: {e}"))
}

// ---------------------------------------------------------------- keys, REST

fn account(k: &Keys) -> Result<(i64, u8)> {
    let (a, i) = k.key.split_once(':').ok_or_else(|| anyhow!("lighter key must be \"<account_index>:<api_key_index>\""))?;
    Ok((a.trim().parse()?, i.trim().parse()?))
}

fn private_key(k: &Keys) -> Result<zk::Scalar> {
    let b = hex::decode(k.secret.trim().trim_start_matches("0x"))?;
    let b: [u8; 40] = b.try_into().map_err(|_| anyhow!("lighter API private key must be 40 bytes hex"))?;
    Ok(zk::scalar_from_le(&b))
}

/// GET with the per-host rate-limit pause; `auth` is a Lighter auth token.
async fn get(path: &str, auth: Option<&str>) -> Result<Value> {
    let url = format!("{}{path}", host());
    ws::check_paused(&url)?;
    let mut req = ws::http().get(&url);
    if let Some(a) = auth { req = req.header("authorization", a); }
    let r = req.send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    ws::note_limit(&url, st, None, &body);
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("lighter {path}: HTTP {st}: {}", &body[..body.len().min(200)]))?;
    if st != 200 || v["code"].as_i64().is_some_and(|c| c != 200) { bail!("lighter {}: {} ({})", path.split('?').next().unwrap_or(path), v["message"].as_str().unwrap_or("?"), v["code"]); }
    Ok(v)
}

fn form_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn send_tx(tx_type: u8, info: &str) -> Result<Value> {
    let url = format!("{}/api/v1/sendTx", host());
    ws::check_paused(&url)?;
    let r = ws::http().post(&url).header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("tx_type={tx_type}&tx_info={}", form_escape(info))).send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    ws::note_limit(&url, st, None, &body);
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("lighter sendTx: HTTP {st}: {}", &body[..body.len().min(200)]))?;
    if st != 200 || v["code"].as_i64() != Some(200) { bail!("lighter sendTx: {} ({})", v["message"].as_str().unwrap_or("?"), v["code"]); }
    Ok(v)
}

/// Next transaction nonce per API key: fetched once, then counted locally; dropped after any failed
/// send so the next order re-reads the server's value.
static NONCE: Mutex<Option<(String, i64)>> = Mutex::new(None);

async fn next_nonce(k: &Keys) -> Result<i64> {
    {
        let mut g = NONCE.lock().unwrap();
        if let Some((_, n)) = g.as_mut().filter(|(key, _)| *key == k.key) { *n += 1; return Ok(*n); }
    }
    let (a, i) = account(k)?;
    let v = get(&format!("/api/v1/nextNonce?account_index={a}&api_key_index={i}"), None).await?;
    let n = v["nonce"].as_i64().ok_or_else(|| anyhow!("lighter nextNonce: no nonce"))?;
    *NONCE.lock().unwrap() = Some((k.key.clone(), n));
    Ok(n)
}

/// Sign and send; the nonce cache is reset on any failure.
async fn submit(tx_type: u8, info: String) -> Result<Value> {
    let r = send_tx(tx_type, &info).await;
    if r.is_err() { *NONCE.lock().unwrap() = None; }
    r
}

/// Auth token for private reads / WS channels: "<deadline>:<account>:<api_key>:<sig hex>".
fn auth_token(k: &Keys, ttl_s: i64) -> Result<String> {
    let (a, i) = account(k)?;
    let msg = format!("{}:{a}:{i}", crate::now_ms() / 1000 + ttl_s);
    let sig = zk::sign(&zk::hash_bytes(msg.as_bytes()), &private_key(k)?);
    Ok(format!("{msg}:{}", hex::encode(sig)))
}

// ---------------------------------------------------------------- transactions

#[derive(Debug, Clone, Copy, PartialEq)]
struct OrderTx { account: i64, api_key: u8, market: i64, client_index: i64, base: i64, price: u32, ask: bool, otype: u8, tif: u8, reduce: bool, expiry: i64, expired_at: i64, nonce: i64 }

impl OrderTx {
    fn hash(&self) -> zk::Fp5 {
        zk::hash(&[chain_id(), TX_CREATE_ORDER as u64, self.nonce as u64, self.expired_at as u64, self.account as u64, self.api_key as u64, self.market as u64,
            self.client_index as u64, self.base as u64, self.price as u64, self.ask as u64, self.otype as u64, self.tif as u64, self.reduce as u64, 0, self.expiry as u64])
    }
    /// tx_info JSON, field for field what lighter-go's json.Marshal emits
    fn info(&self, sig: &[u8; 80]) -> String {
        json!({"AccountIndex": self.account, "ApiKeyIndex": self.api_key, "MarketIndex": self.market, "ClientOrderIndex": self.client_index,
            "BaseAmount": self.base, "Price": self.price, "IsAsk": self.ask as u8, "Type": self.otype, "TimeInForce": self.tif, "ReduceOnly": self.reduce as u8,
            "TriggerPrice": 0, "OrderExpiry": self.expiry, "ExpiredAt": self.expired_at, "Nonce": self.nonce, "Sig": b64(sig), "L2TxAttributes": null}).to_string()
    }
}

#[derive(Debug, Clone, Copy)]
struct CancelTx { account: i64, api_key: u8, market: i64, index: i64, expired_at: i64, nonce: i64 }

impl CancelTx {
    fn hash(&self) -> zk::Fp5 {
        zk::hash(&[chain_id(), TX_CANCEL_ORDER as u64, self.nonce as u64, self.expired_at as u64, self.account as u64, self.api_key as u64, self.market as u64, self.index as u64])
    }
    fn info(&self, sig: &[u8; 80]) -> String {
        json!({"AccountIndex": self.account, "ApiKeyIndex": self.api_key, "MarketIndex": self.market, "Index": self.index,
            "ExpiredAt": self.expired_at, "Nonce": self.nonce, "Sig": b64(sig), "L2TxAttributes": null}).to_string()
    }
}

fn b64(b: &[u8]) -> String { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(b) }

/// Transactions expire 10 minutes after signing (lighter-go default, minus a second of margin).
fn tx_expiry() -> i64 { crate::now_ms() + 599_000 }

/// Client order index: unique per process, inside Lighter's 48-bit client range (non-zero).
fn client_index() -> i64 {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let _ = NEXT.compare_exchange(0, crate::now_ms(), std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) & ((1 << 48) - 1)
}

/// Order type / time-in-force / order expiry (ms, 0 = none) for a request.
/// Lighter: type 0 limit, 1 market; tif 0 IOC, 1 good-till-time, 2 post-only. Resting orders
/// need an expiry (28 days, as the official SDKs); IOC and market orders must have none.
fn order_params(kind: Kind, now: i64) -> (u8, u8, i64) {
    let gtt = now + 28 * 24 * 3_600_000;
    match kind {
        Kind::Market => (1, 0, 0),
        Kind::Limit { tif: Tif::Ioc, .. } => (0, 0, 0),
        Kind::Limit { tif: Tif::Gtc, .. } | Kind::Bbo { .. } => (0, 1, gtt),
        Kind::Limit { tif: Tif::PostOnly, .. } => (0, 2, gtt),
    }
}

// ---------------------------------------------------------------- venue API

pub async fn rules(symbol: &str) -> Result<Rules> { Ok(rules_of(&market(symbol).await?)) }

/// Lighter has no hedge mode.
pub async fn mode(_k: &Keys, _symbol: &str) -> Result<Mode> { Ok(Mode::OneWay) }

async fn account_row(k: &Keys) -> Result<Value> {
    let (a, _) = account(k)?;
    let v = get(&format!("/api/v1/account?by=index&value={a}"), None).await?;
    Ok(v["accounts"][0].clone())
}

/// initial_margin_fraction is in percent on account rows ("5.00" = 20x); the market default is in
/// hundredths of a percent (500 = 20x) and applies until the account sets its own.
pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let m = market(symbol).await?;
    let acc = account_row(k).await?;
    let imf = acc["positions"].as_array().into_iter().flatten().find(|p| p["market_id"].as_i64() == Some(m.id)).map(|p| n(&p["initial_margin_fraction"]));
    if let Some(imf) = imf.filter(|x| *x > 0.0) { return Ok(100.0 / imf); }
    let d = get(&format!("/api/v1/orderBookDetails?market_id={}", m.id), None).await?;
    let imf = n(&d["order_book_details"][0]["default_initial_margin_fraction"]);
    if imf > 0.0 { Ok(10_000.0 / imf) } else { bail!("lighter: no leverage for {symbol}") }
}

/// Returns the client order index (cancel accepts it as well as the server order index).
pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, _mode: Mode) -> Result<String> {
    let (qty, px) = checked(req, r, ref_px)?;
    let m = market(&req.symbol).await?;
    let ask = req.side() == Side::Sell;
    let px = match (req.kind, px) {
        (Kind::Limit { .. }, Some(p)) => p,
        (Kind::Market, _) => {
            if !(ref_px > 0.0) { bail!("lighter market order needs a reference price"); }
            // worst acceptable price, rounded away from the book so the cap is never tighter
            let worst = if ask { ref_px * (1.0 - MARKET_SLIPPAGE) } else { ref_px * (1.0 + MARKET_SLIPPAGE) };
            let tick = r.tick;
            fmt_step(if ask { (worst / tick).floor() * tick } else { (worst / tick).ceil() * tick }, tick, false)
        }
        _ => bail!("lighter has no BBO orders"),
    };
    let (a, i) = account(k)?;
    let (otype, tif, expiry) = order_params(req.kind, crate::now_ms());
    let price = u32::try_from(scaled(&px, m.price_dp)?).map_err(|_| anyhow!("price {px} out of range"))?;
    if price == 0 { bail!("price {px} rounds to zero"); }
    let tx = OrderTx { account: a, api_key: i, market: m.id, client_index: client_index(), base: scaled(&qty, m.size_dp)?, price, ask, otype, tif,
        reduce: req.close, expiry, expired_at: tx_expiry(), nonce: next_nonce(k).await? };
    let sig = zk::sign(&tx.hash(), &private_key(k)?);
    submit(TX_CREATE_ORDER, tx.info(&sig)).await?;
    Ok(tx.client_index.to_string())
}

/// `id`: server order index (from open_orders / the stream) or the client index `place` returned.
pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    let m = market(symbol).await?;
    let (a, i) = account(k)?;
    let tx = CancelTx { account: a, api_key: i, market: m.id, index: id.parse().map_err(|_| anyhow!("lighter: bad order id {id}"))?,
        expired_at: tx_expiry(), nonce: next_nonce(k).await? };
    let sig = zk::sign(&tx.hash(), &private_key(k)?);
    submit(TX_CANCEL_ORDER, tx.info(&sig)).await?;
    Ok(())
}

/// One account position row (REST /account and the account_all_positions push share the shape).
fn position(p: &Value) -> Position {
    let qty = n(&p["position"]).abs();
    let value = n(&p["position_value"]).abs();
    let imf = n(&p["initial_margin_fraction"]);
    // margin_mode 1 = isolated (allocated margin); cross margin is the initial margin of the position
    let margin = if p["margin_mode"].as_i64() == Some(1) { n(&p["allocated_margin"]) } else { value * imf / 100.0 };
    Position {
        ex: Exchange::Lighter, symbol: symbol(p["symbol"].as_str().unwrap_or_default()), side: if p["sign"].as_i64() == Some(-1) { Side::Sell } else { Side::Buy },
        qty, entry: n(&p["avg_entry_price"]), mark: if qty > 0.0 { value / qty } else { 0.0 },
        liq: crate::opt_num(&p["liquidation_price"]).filter(|x| *x > 0.0), upnl: n(&p["unrealized_pnl"]),
        lev: if imf > 0.0 { 100.0 / imf } else { 0.0 }, margin,
        cross: Some(p["margin_mode"].as_i64() != Some(1)),
    }
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    let acc = account_row(k).await?;
    Ok(acc["positions"].as_array().into_iter().flatten().map(position).filter(|p| p.qty > 0.0).collect())
}

fn is_open(status: &str) -> bool { matches!(status, "open" | "pending" | "in-progress") }

fn order(mkts: &[Mkt], o: &Value) -> OpenOrder {
    // timestamps arrive in seconds on some payloads, milliseconds on others
    let ts = o["timestamp"].as_i64().or(o["created_at"].as_i64()).unwrap_or(0);
    OpenOrder {
        ex: Exchange::Lighter, symbol: sym_of(mkts, o["market_index"].as_i64().unwrap_or(-1)), id: o["order_index"].to_string(),
        side: if o["is_ask"] == true { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["initial_base_amount"]), filled: n(&o["filled_base_amount"]),
        kind: o["type"].as_str().unwrap_or_default().into(), reduce_only: o["reduce_only"].as_bool().unwrap_or(false),
        ts: if ts > 0 && ts < 100_000_000_000 { ts * 1000 } else { ts }, pos: None,
    }
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let (a, _) = account(k)?;
    let mkts = markets(false).await?;
    let v = get(&format!("/api/v1/accountActiveOrders?account_index={a}"), Some(&auth_token(k, 600)?)).await?;
    Ok(v["orders"].as_array().into_iter().flatten().map(|o| order(&mkts, o)).collect())
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let acc = account_row(k).await?;
    let upnl: f64 = acc["positions"].as_array().into_iter().flatten().map(|p| n(&p["unrealized_pnl"])).sum();
    let equity = crate::opt_num(&acc["total_asset_value"]).filter(|x| *x > 0.0).unwrap_or(n(&acc["collateral"]) + upnl);
    Ok(Balance { equity, available: n(&acc["available_balance"]), ..Default::default() })
}

/// Private account stream: positions (public channel), orders (auth token), account stats and
/// marks of every market, one connection. Resync once subscribed.
pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Lighter;
    loop {
        let res: Result<bool> = async {
            let (a, _) = account(&k)?;
            let mkts = markets(false).await?;
            let token = auth_token(&k, 600)?;
            let spec = ws::Spec::new("lighter account", ws_url())
                .sub(json!({"type": "subscribe", "channel": format!("account_all_positions/{a}")}).to_string())
                .sub(json!({"type": "subscribe", "channel": format!("account_all_orders/{a}"), "auth": token}).to_string())
                .sub(json!({"type": "subscribe", "channel": format!("user_stats/{a}")}).to_string())
                .sub(json!({"type": "subscribe", "channel": "market_stats/all"}).to_string())
                .ping(std::time::Duration::from_secs(30), r#"{"type":"ping"}"#);
            let mut auth_failed = false;
            ws::once(&spec, |f| {
                let ws::Frame::Text(s) = f else { return true };
                let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
                let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
                let ty = v["type"].as_str().unwrap_or_default();
                if let Some(e) = v.get("error") {
                    eprintln!("[lighter account] {e}");
                    if e.to_string().to_lowercase().contains("auth") { auth_failed = true; return false; }
                    return true;
                }
                if ty == "subscribed/account_all_positions" { send(AccEvent::Resync); }
                if ty.ends_with("/account_all_positions") {
                    for p in v["positions"].as_object().into_iter().flat_map(|o| o.values()) { send(AccEvent::Position { p: position(p), one_way: true }); }
                } else if ty.ends_with("/account_all_orders") {
                    for o in v["orders"].as_object().into_iter().flat_map(|o| o.values()).filter_map(|l| l.as_array()).flatten() {
                        send(if is_open(o["status"].as_str().unwrap_or_default()) { AccEvent::Order(order(&mkts, o)) } else { AccEvent::OrderDone { id: o["order_index"].to_string() } });
                    }
                } else if ty.ends_with("/user_stats") {
                    let st = &v["stats"];
                    send(AccEvent::Wallet { equity: n(&st["portfolio_value"]), available: n(&st["available_balance"]) });
                } else if ty.ends_with("/market_stats") {
                    // market_stats/all: a map of market id -> stats; a single market: the stats object
                    let ms = &v["market_stats"];
                    let rows: Vec<&Value> = if ms.get("mark_price").is_some() { vec![ms] } else { ms.as_object().into_iter().flat_map(|o| o.values()).collect() };
                    for m in rows {
                        let mark = n(&m["mark_price"]);
                        if mark > 0.0 { send(AccEvent::Mark { symbol: sym_of(&mkts, m["market_id"].as_i64().unwrap_or(-1)), mark }); }
                    }
                }
                true
            }).await?;
            Ok(auth_failed)
        }.await;
        let wait = match res { Ok(true) => 60, Ok(false) => 2, Err(e) => { eprintln!("[lighter account] {e:#}"); 5 } };
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }
}

// ---------------------------------------------------------------- signer

/// Lighter's signature scheme, ported from elliottech/poseidon_crypto (Go) as used by lighter-go:
/// Goldilocks field p = 2^64 - 2^32 + 1, quintic extension Fp5 = Fp[X]/(X^5 - 3), Poseidon2
/// (width 12, rate 8, 8 full + 22 partial rounds) hashing to Fp5, Schnorr over the prime-order
/// ECgFp5 curve: s = k - e*sk, e = H(encode(k*G) || H(m)), signature = s || e (2 x 40 bytes LE).
/// The nonce k is derived deterministically from the key and the message (no RNG dependency).
// ponytail: plain u128 % arithmetic and a bit-by-bit ladder (~ms per signature); port the
// windowed/Montgomery paths of poseidon_crypto if signing latency ever matters
mod zk {
    const P: u64 = 0xFFFF_FFFF_0000_0001;
    pub type Fp5 = [u64; 5];
    pub type Scalar = [u64; 5];

    fn add(a: u64, b: u64) -> u64 { ((a as u128 + b as u128) % P as u128) as u64 }
    fn sub(a: u64, b: u64) -> u64 { ((a as u128 + P as u128 - b as u128) % P as u128) as u64 }
    fn mul(a: u64, b: u64) -> u64 { ((a as u128 * b as u128) % P as u128) as u64 }
    fn pow(mut a: u64, mut e: u64) -> u64 {
        let mut r = 1;
        while e > 0 { if e & 1 == 1 { r = mul(r, a); } a = mul(a, a); e >>= 1; }
        r
    }

    // ---- Fp5
    fn f_add(a: Fp5, b: Fp5) -> Fp5 { std::array::from_fn(|i| add(a[i], b[i])) }
    fn f_sub(a: Fp5, b: Fp5) -> Fp5 { std::array::from_fn(|i| sub(a[i], b[i])) }
    fn f_dbl(a: Fp5) -> Fp5 { f_add(a, a) }
    fn f_mul(a: Fp5, b: Fp5) -> Fp5 {
        let mut c = [0u128; 5];
        for i in 0..5 { for j in 0..5 {
            let p = a[i] as u128 * b[j] as u128 % P as u128;
            let (k, p) = if i + j >= 5 { (i + j - 5, p * 3) } else { (i + j, p) };
            c[k] = (c[k] + p) % P as u128;
        } }
        c.map(|x| x as u64)
    }
    fn f_sq(a: Fp5) -> Fp5 { f_mul(a, a) }
    fn small(x: u64) -> Fp5 { [x, 0, 0, 0, 0] }
    /// x^(p^count): coefficient i times (w^((p-1)/5))^(i*count)
    fn frob(x: Fp5, count: u64) -> Fp5 {
        const DTH_ROOT: u64 = 1041288259238279555;
        let z = pow(DTH_ROOT, count);
        std::array::from_fn(|i| mul(x[i], pow(z, i as u64)))
    }
    fn f_inv(a: Fp5) -> Fp5 {
        if a == [0; 5] { return a; }
        let d = frob(a, 1);
        let e = f_mul(d, frob(d, 1));
        let f = f_mul(e, frob(e, 2));
        // a * f lies in the base field: only its constant coefficient is needed
        let g = add(mul(a[0], f[0]), mul(3, add(add(mul(a[1], f[4]), mul(a[2], f[3])), add(mul(a[3], f[2]), mul(a[4], f[1])))));
        let gi = pow(g, P - 2);
        f.map(|x| mul(x, gi))
    }

    // ---- Poseidon2 (constants from poseidon_crypto hash/poseidon2_goldilocks/config.go)
    const EXT: [[u64; 12]; 8] = [
        [15492826721047263190, 11728330187201910315, 8836021247773420868, 16777404051263952451, 5510875212538051896, 6173089941271892285, 2927757366422211339, 10340958981325008808, 8541987352684552425, 9739599543776434497, 15073950188101532019, 12084856431752384512],
        [4584713381960671270, 8807052963476652830, 54136601502601741, 4872702333905478703, 5551030319979516287, 12889366755535460989, 16329242193178844328, 412018088475211848, 10505784623379650541, 9758812378619434837, 7421979329386275117, 375240370024755551],
        [3331431125640721931, 15684937309956309981, 578521833432107983, 14379242000670861838, 17922409828154900976, 8153494278429192257, 15904673920630731971, 11217863998460634216, 3301540195510742136, 9937973023749922003, 3059102938155026419, 1895288289490976132],
        [5580912693628927540, 10064804080494788323, 9582481583369602410, 10186259561546797986, 247426333829703916, 13193193905461376067, 6386232593701758044, 17954717245501896472, 1531720443376282699, 2455761864255501970, 11234429217864304495, 4746959618548874102],
        [13571697342473846203, 17477857865056504753, 15963032953523553760, 16033593225279635898, 14252634232868282405, 8219748254835277737, 7459165569491914711, 15855939513193752003, 16788866461340278896, 7102224659693946577, 3024718005636976471, 13695468978618890430],
        [8214202050877825436, 2670727992739346204, 16259532062589659211, 11869922396257088411, 3179482916972760137, 13525476046633427808, 3217337278042947412, 14494689598654046340, 15837379330312175383, 8029037639801151344, 2153456285263517937, 8301106462311849241],
        [13294194396455217955, 17394768489610594315, 12847609130464867455, 14015739446356528640, 5879251655839607853, 9747000124977436185, 8950393546890284269, 10765765936405694368, 14695323910334139959, 16366254691123000864, 15292774414889043182, 10910394433429313384],
        [17253424460214596184, 3442854447664030446, 3005570425335613727, 10859158614900201063, 9763230642109343539, 6647722546511515039, 909012944955815706, 18101204076790399111, 11588128829349125809, 15863878496612806566, 5201119062417750399, 176665553780565743],
    ];
    const INT: [u64; 22] = [11921381764981422944, 10318423381711320787, 8291411502347000766, 229948027109387563, 9152521390190983261, 7129306032690285515, 15395989607365232011, 8641397269074305925, 17256848792241043600, 6046475228902245682, 12041608676381094092, 12785542378683951657, 14546032085337914034, 3304199118235116851, 16499627707072547655, 10386478025625759321, 13475579315436919170, 16042710511297532028, 1411266850385657080, 9024840976168649958, 14047056970978379368, 838728605080212101];
    const DIAG: [u64; 12] = [0xc3b6c08e23ba9300, 0xd84b5de94a324fb6, 0x0d0c371c5b35b84f, 0x7964f570e7188037, 0x5daf18bbd996604b, 0x6743bc47b9595257, 0x5528b9362c59bb70, 0xac45e25b7127b68b, 0xa2077d7dfbb606b5, 0xf3faac6faee378ae, 0x0c6388b51545e883, 0xd27dbb6944917b60];

    fn sbox(x: u64) -> u64 { let x2 = mul(x, x); mul(mul(x2, x2), mul(x2, x)) }
    fn external(s: &mut [u64; 12]) {
        for c in s.chunks_mut(4) {
            let (t0, t1) = (add(c[0], c[1]), add(c[2], c[3]));
            let t2 = add(t0, t1);
            let (t3, t4) = (add(t2, c[1]), add(t2, c[3]));
            let (t5, t6) = (add(c[0], c[0]), add(c[2], c[2]));
            c[0] = add(t3, t0); c[1] = add(t6, t3); c[2] = add(t1, t4); c[3] = add(t5, t4);
        }
        let sums: [u64; 4] = std::array::from_fn(|k| add(add(s[k], s[k + 4]), s[k + 8]));
        for i in 0..12 { s[i] = add(s[i], sums[i % 4]); }
    }
    fn permute(s: &mut [u64; 12]) {
        external(s);
        let full = |s: &mut [u64; 12], r: usize| { for i in 0..12 { s[i] = sbox(add(s[i], EXT[r][i])); } external(s); };
        for r in 0..4 { full(s, r); }
        for rc in INT {
            s[0] = sbox(add(s[0], rc));
            let sum = s.iter().fold(0, |a, &x| add(a, x));
            for i in 0..12 { s[i] = add(mul(s[i], DIAG[i]), sum); }
        }
        for r in 4..8 { full(s, r); }
    }
    /// HashNToMNoPad(input, 5): overwrite-mode sponge, rate 8, first 5 lanes out.
    pub fn hash(input: &[u64]) -> Fp5 {
        let mut s = [0u64; 12];
        for chunk in input.chunks(8) { s[..chunk.len()].copy_from_slice(chunk); permute(&mut s); }
        [s[0], s[1], s[2], s[3], s[4]]
    }
    /// Bytes as little-endian u64 limbs (last one zero-padded), then `hash`.
    pub fn hash_bytes(b: &[u8]) -> Fp5 {
        let limbs: Vec<u64> = b.chunks(8).map(|c| { let mut x = [0u8; 8]; x[..c.len()].copy_from_slice(c); u64::from_le_bytes(x) % P }).collect();
        hash(&limbs)
    }

    // ---- scalars mod n (curve order, ~2^319)
    const N: Scalar = [0xE80FD996948BFFE1, 0xE8885C39D724A09C, 0x7FFFFFE6CFB80639, 0x7FFFFFF100000016, 0x7FFFFFFD80000007];
    const N0I: u64 = 0xD78BEF72057B7BDF;
    /// 2^640 mod n
    const R2: Scalar = [0xA01001DCE33DC739, 0x6C3228D33F62ACCF, 0xD1D796CC91CF8525, 0xAADFFF5D1574C1D8, 0x4ACA13B28CA251F5];

    /// a - b over 320 bits, with the borrow
    fn sub_raw(a: Scalar, b: Scalar) -> (Scalar, bool) {
        let mut r = [0; 5];
        let mut borrow = false;
        for i in 0..5 {
            let (x, b1) = a[i].overflowing_sub(b[i]);
            let (x, b2) = x.overflowing_sub(borrow as u64);
            r[i] = x; borrow = b1 || b2;
        }
        (r, borrow)
    }
    fn add_raw(a: Scalar, b: Scalar) -> Scalar {
        let mut r = [0; 5];
        let mut c = 0u128;
        for i in 0..5 { c += a[i] as u128 + b[i] as u128; r[i] = c as u64; c >>= 64; }
        r
    }
    /// any 320-bit value -> canonical (2^320 < 3n: at most two subtractions)
    fn reduce(mut a: Scalar) -> Scalar {
        for _ in 0..2 { let (r, borrow) = sub_raw(a, N); if !borrow { a = r; } }
        a
    }
    fn s_sub(a: Scalar, b: Scalar) -> Scalar { let (r, borrow) = sub_raw(a, b); if borrow { add_raw(r, N) } else { r } }
    fn s_add(a: Scalar, b: Scalar) -> Scalar { let r = add_raw(a, b); let (r2, borrow) = sub_raw(r, N); if borrow { r } else { r2 } }
    /// a*b / 2^320 mod n, a < n
    fn monty(a: Scalar, b: Scalar) -> Scalar {
        let mut r = [0u64; 5];
        for i in 0..5 {
            let m = b[i];
            let f = a[0].wrapping_mul(m).wrapping_add(r[0]).wrapping_mul(N0I);
            let (mut c1, mut c2) = (0u64, 0u64);
            for j in 0..5 {
                let z = a[j] as u128 * m as u128 + r[j] as u128 + c1 as u128;
                c1 = (z >> 64) as u64;
                let z = f as u128 * N[j] as u128 + (z as u64) as u128 + c2 as u128;
                c2 = (z >> 64) as u64;
                if j > 0 { r[j - 1] = z as u64; }
            }
            r[4] = c1.wrapping_add(c2);
        }
        reduce(r)
    }
    fn s_mul(a: Scalar, b: Scalar) -> Scalar { monty(monty(a, R2), b) }
    pub fn scalar_from_le(b: &[u8; 40]) -> Scalar { reduce(std::array::from_fn(|i| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()))) }
    fn scalar_le(s: Scalar) -> [u8; 40] { let mut o = [0u8; 40]; for i in 0..5 { o[i * 8..i * 8 + 8].copy_from_slice(&s[i].to_le_bytes()); } o }

    // ---- ECgFp5, fractional (x, u) coordinates as X/Z, U/T; complete formulas
    #[derive(Clone, Copy)]
    struct Pt { x: Fp5, z: Fp5, u: Fp5, t: Fp5 }
    const B: Fp5 = [0, 263, 0, 0, 0];
    const NEUTRAL: Pt = Pt { x: [0; 5], z: [1, 0, 0, 0, 0], u: [0; 5], t: [1, 0, 0, 0, 0] };
    const GEN: Pt = Pt { x: [12883135586176881569, 4356519642755055268, 5248930565894896907, 2165973894480315022, 2448410071095648785],
                         z: [1, 0, 0, 0, 0], u: [1, 0, 0, 0, 0], t: [4, 0, 0, 0, 0] };

    fn pt_add(p: Pt, q: Pt) -> Pt {
        let (t1, t2, t3, t4) = (f_mul(p.x, q.x), f_mul(p.z, q.z), f_mul(p.u, q.u), f_mul(p.t, q.t));
        let t5 = f_sub(f_mul(f_add(p.x, p.z), f_add(q.x, q.z)), f_add(t1, t2));
        let t6 = f_sub(f_mul(f_add(p.u, p.t), f_add(q.u, q.t)), f_add(t3, t4));
        let t7 = f_add(t1, f_mul(t2, B));
        let t8 = f_mul(t4, t7);
        let t9 = f_mul(t3, f_add(f_mul(t5, f_dbl(B)), f_dbl(t7)));
        let t10 = f_mul(f_add(t4, f_dbl(t3)), f_add(t5, t7));
        Pt { x: f_mul(f_sub(t10, t8), B), z: f_sub(t8, t9), u: f_mul(t6, f_sub(f_mul(t2, B), t1)), t: f_add(t8, t9) }
    }
    fn pt_dbl(p: Pt) -> Pt {
        let t1 = f_mul(p.z, p.t);
        let t2 = f_mul(t1, p.t);
        let x1 = f_sq(t2);
        let z1 = f_mul(t1, p.u);
        let t3 = f_sq(p.u);
        let w1 = f_sub(t2, f_mul(t3, f_dbl(f_add(p.x, p.z))));
        let t4 = f_sq(z1);
        let z = f_sq(w1);
        Pt { x: f_mul(t4, f_mul(B, small(4))), z, u: f_sub(f_sq(f_add(w1, z1)), f_add(t4, z)), t: f_sub(f_dbl(x1), f_add(f_mul(t4, small(4)), z)) }
    }
    /// k*G, MSB first; both branches computed every bit
    fn mul_gen(k: Scalar) -> Pt {
        let mut acc = NEUTRAL;
        for bit in (0..320).rev() {
            acc = pt_dbl(acc);
            let sum = pt_add(acc, GEN);
            if (k[bit / 64] >> (bit % 64)) & 1 == 1 { acc = sum; }
        }
        acc
    }
    fn encode(p: Pt) -> Fp5 { f_mul(p.t, f_inv(p.u)) }

    #[cfg(test)]
    pub fn public_key(sk: &Scalar) -> [u8; 40] { scalar_le(encode(mul_gen(*sk))) }

    /// Deterministic nonce: SHA-512(domain || sk || msg) as 512 bits, reduced mod n
    /// (hi * 2^320 + lo: Montgomery with R2 gives hi * 2^320 mod n).
    fn nonce(sk: &Scalar, msg: &Fp5) -> Scalar {
        use sha2::{Digest, Sha512};
        let mut h = Sha512::new();
        h.update(b"terminal-one lighter schnorr nonce");
        h.update(scalar_le(*sk));
        h.update(scalar_le(*msg));
        let d = h.finalize();
        let w: [u64; 8] = std::array::from_fn(|i| u64::from_le_bytes(d[i * 8..i * 8 + 8].try_into().unwrap()));
        let lo = reduce([w[0], w[1], w[2], w[3], w[4]]);
        let hi = [w[5], w[6], w[7], 0, 0];
        s_add(monty(hi, R2), lo)
    }

    pub fn sign_with(msg: &Fp5, sk: &Scalar, k: Scalar) -> [u8; 80] {
        let r = encode(mul_gen(k));
        let mut pre = r.to_vec();
        pre.extend_from_slice(msg);
        let e = reduce(hash(&pre));
        let s = s_sub(k, s_mul(e, *sk));
        let mut out = [0u8; 80];
        out[..40].copy_from_slice(&scalar_le(s));
        out[40..].copy_from_slice(&scalar_le(e));
        out
    }
    pub fn sign(msg: &Fp5, sk: &Scalar) -> [u8; 80] { sign_with(msg, sk, nonce(sk, msg)) }
    #[cfg(test)]
    pub fn fp5_le(x: &Fp5) -> [u8; 40] { scalar_le(*x) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generated with the official Go code (lighter-go signer/txtypes + poseidon_crypto):
    // key from NewSeedKeyManager("000102..1f"), nonce k = 123456789012345678901234567890123456789012345678901234567890.
    const PRIV: &str = "ecc2d847fd011e1219f9b67a67773e0df619a6b51f114ed0c0cbd647df727d7c5ac643f4b0761149";
    const K: zk::Scalar = [0x8ccff196ce3f0ad2, 0x3f87a4378c37b49c, 0xaaf504e4bc1e6217, 0x13, 0];

    fn sk() -> zk::Scalar { zk::scalar_from_le(&hex::decode(PRIV).unwrap().try_into().unwrap()) }

    #[test]
    fn poseidon2_and_keys_match_go() {
        let input: Vec<u64> = (0..15u64).map(|i| i.wrapping_mul(0x1234567890abcdef) % 0xFFFF_FFFF_0000_0001).collect();
        assert_eq!(hex::encode(zk::fp5_le(&zk::hash(&input))), "cd4ee789c57617154b750c1ece3ce401a28c60082347c5ab5eb4f4401c5c20f212321ad76f2c07e4");
        assert_eq!(hex::encode(zk::public_key(&sk())), "064c8dac0945b2c6f6ab77c81b569dfbeee6ac834f05037847e82af66c20c761e39d2d21abfefcbc");
    }

    #[test]
    fn auth_token_signature_matches_go() {
        let h = zk::hash_bytes(b"1700000000:123:4");
        assert_eq!(hex::encode(zk::fp5_le(&h)), "fe769c30e649afa97d59a5aa9f48771b71d46893377f5f1bf011effa6196245685c9ae19dbe02039");
        assert_eq!(hex::encode(zk::sign_with(&h, &sk(), K)),
            "8bf68fc99efbb144816d4cf0ca70c9a4f0f63ae9dc2bfc9ac8f3f5eef66d33d80c6f45710de92243cbfd424a8509ea451535e8da18b07b12448b1a4bb1e2a4fef9ef50d401fa2a05bac0decd626c6862");
    }

    #[test]
    fn order_and_cancel_tx_match_go() {
        // chain id 304 = mainnet (T1_TESTNET unset in tests)
        let tx = OrderTx { account: 123, api_key: 4, market: 1, client_index: 777, base: 1500, price: 843210, ask: true, otype: 0, tif: 1, reduce: false,
            expiry: 1702419200000, expired_at: 1700000599000, nonce: 42 };
        let h = tx.hash();
        assert_eq!(hex::encode(zk::fp5_le(&h)), "a749fcef8e4c1694f5cbfa3e15aede098a94a2dbadcfb8041343232853b4fbbefa6cf20b17b15d06");
        let sig = zk::sign_with(&h, &sk(), K);
        assert_eq!(hex::encode(sig), "42447d360a44c7ca357d810d09cb81f1468a2792beecf012961d4912186cc3173b8aafe4118ca507fd06bb74bd5a2e66abf7232c6508175405697e908d8decb09d7fce88e633bbf5badfadf76a324c59");
        let go: Value = serde_json::from_str(r#"{"AccountIndex":123,"ApiKeyIndex":4,"MarketIndex":1,"ClientOrderIndex":777,"BaseAmount":1500,"Price":843210,"IsAsk":1,"Type":0,"TimeInForce":1,"ReduceOnly":0,"TriggerPrice":0,"OrderExpiry":1702419200000,"ExpiredAt":1700000599000,"Nonce":42,"Sig":"QkR9NgpEx8o1fYENCcuB8UaKJ5K+7PASlh1JEhhswxc7iq/kEYylB/0Gu3S9Wi5mq/cjLGUIF1QFaX6QjY3ssJ1/zojmM7v1ut+t92oyTFk=","L2TxAttributes":null}"#).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&tx.info(&sig)).unwrap(), go);
        let c = CancelTx { account: 123, api_key: 4, market: 1, index: 281474976710700, expired_at: 1700000599000, nonce: 43 };
        assert_eq!(hex::encode(zk::fp5_le(&c.hash())), "47ce578e2733f57780741729ba866eba5e046d364f019c19ff905960870903449b64d74ca54e8f1e");
        // deterministic nonce: same message, same signature; different message, different e
        assert_eq!(zk::sign(&h, &sk()), zk::sign(&h, &sk()));
        assert_ne!(zk::sign(&h, &sk())[40..], zk::sign(&c.hash(), &sk())[40..]);
    }

    #[test]
    fn scaling_symbols_and_params() {
        assert_eq!(scaled("0.0015", 5).unwrap(), 150);
        assert_eq!(scaled("84321.0", 1).unwrap(), 843210);
        assert_eq!(scaled("12", 2).unwrap(), 1200);
        assert!(scaled("0.123", 2).is_err());
        assert_eq!(base_of("BTCUSDT"), "BTC");
        assert_eq!(base_of("ETHUSDC"), "ETH");
        assert_eq!(base_of("BTC"), "BTC");
        assert_eq!(form_escape(r#"{"Sig":"a+b/="}"#), "%7B%22Sig%22%3A%22a%2Bb%2F%3D%22%7D");
        let now = 1_000;
        assert_eq!(order_params(Kind::Market, now), (1, 0, 0));
        assert_eq!(order_params(Kind::Limit { price: 1.0, tif: Tif::Ioc }, now), (0, 0, 0));
        assert_eq!(order_params(Kind::Limit { price: 1.0, tif: Tif::Gtc }, now).1, 1);
        assert!(order_params(Kind::Limit { price: 1.0, tif: Tif::PostOnly }, now).2 > now);
        assert!(account(&Keys { key: "123:4".into(), secret: String::new(), extra: None }).unwrap() == (123, 4));
        let a = client_index();
        assert!(a > 0 && client_index() == a + 1);
    }

    #[test]
    fn position_row() {
        let p: Value = serde_json::from_str(r#"{"market_id":1,"symbol":"BTC","initial_margin_fraction":"5.00","sign":-1,"position":"0.5","avg_entry_price":"80000","position_value":"42000","unrealized_pnl":"-2000","liquidation_price":"0","margin_mode":0,"allocated_margin":"0"}"#).unwrap();
        let p = position(&p);
        assert_eq!((p.symbol.as_str(), p.side, p.qty, p.mark, p.liq, p.lev, p.margin), ("BTCUSDT", Side::Sell, 0.5, 84000.0, None, 20.0, 2100.0));
    }
}
