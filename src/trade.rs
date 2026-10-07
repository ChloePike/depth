//! Private trading APIs: Bybit v5 (unified, linear USDT perps) and Binance USD-M futures here;
//! OKX, Bitget, Gate, Kraken Futures, Hyperliquid and Lighter each in their own `trade/<venue>.rs`
//! behind the same functions. Public API always uses unified symbols ("BTCUSDT") and base-unit
//! sizes; each venue module maps to its native symbol / contracts.
//! Keys live in the macOS Keychain (service "terminal-one", accounts "<venue>-key" /
//! "<venue>-secret"). `T1_TESTNET=1` switches both venues to their testnets.
//!
//! Prices and sizes are always sent as strings rounded to the instrument's tick / step, never as
//! raw f64 formatting.
use crate::{ws, Exchange, Side};
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

pub mod bitget;
pub mod gate;
pub mod hyperliquid;
pub mod kraken;
pub mod lighter;
pub mod okx;
pub mod tpsl;

pub const TRADABLE: [Exchange; 8] = KEY_VENUES;

/// Venues whose trading has run against a real account. The others are implemented and
/// unit-tested only: the order confirmation warns until they are verified.
pub const VERIFIED: [Exchange; 2] = [Exchange::Bybit, Exchange::Binance];

/// API credentials from the Keychain. `extra` is the third secret some venues need:
/// OKX / Bitget passphrase; Hyperliquid: key = account (main wallet) address, secret = API
/// (agent) wallet private key; Lighter: key = "<account_index>:<api_key_index>", secret = API key.
#[derive(Clone)]
pub struct Keys { pub key: String, pub secret: String, pub extra: Option<String> }

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Keys({}…)", &self.key[..self.key.len().min(4)]) }
}

fn venue(ex: Exchange) -> &'static str {
    match ex {
        Exchange::Bybit => "bybit", Exchange::Binance => "binance", Exchange::Okx => "okx", Exchange::Bitget => "bitget", Exchange::Gate => "gate",
        Exchange::Kraken => "kraken", Exchange::Hyperliquid => "hyperliquid", Exchange::Lighter => "lighter", Exchange::Coinbase => "coinbase", Exchange::Mexc => "mexc",
    }
}

/// Venues whose third Keychain item ("<venue>-passphrase") is required.
fn needs_extra(ex: Exchange) -> bool { matches!(ex, Exchange::Okx | Exchange::Bitget) }

pub fn testnet() -> bool { std::env::var("T1_TESTNET").is_ok() }

/// Venues whose keys can be configured (Settings > API Keys).
pub const KEY_VENUES: [Exchange; 8] = [Exchange::Bybit, Exchange::Binance, Exchange::Okx, Exchange::Bitget, Exchange::Gate, Exchange::Kraken, Exchange::Hyperliquid, Exchange::Lighter];

fn kc_account(ex: Exchange, what: &str) -> String { format!("{}-{what}{}", venue(ex), if testnet() { "-testnet" } else { "" }) }

/// Read keys from the macOS Keychain (service "terminal-one") with the `security` tool. Items are
/// created by `security` too, so it is on their access list: no Keychain prompt, and no prompt
/// again after every rebuild (an ad-hoc signed app reading them directly would trigger both).
pub fn keychain(ex: Exchange) -> Option<Keys> {
    let get = |what: &str| -> Option<String> {
        let out = std::process::Command::new("security").args(["find-generic-password", "-s", "terminal-one", "-a", &kc_account(ex, what), "-w"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    };
    let extra = get("passphrase");
    if needs_extra(ex) && extra.is_none() { return None; }
    Some(Keys { key: get("key")?, secret: get("secret")?, extra })
}

/// Store keys for `ex` in the Keychain (replacing existing items). `extra` is the passphrase /
/// third secret where the venue needs one; None removes it.
pub fn save_keys(ex: Exchange, key: &str, secret: &str, extra: Option<&str>) -> Result<()> {
    if key.trim().is_empty() || secret.trim().is_empty() { bail!("key and secret are required"); }
    if needs_extra(ex) && extra.is_none_or(|e| e.trim().is_empty()) { bail!("{ex:?} also needs its passphrase"); }
    kc_set(&kc_account(ex, "key"), key.trim())?;
    kc_set(&kc_account(ex, "secret"), secret.trim())?;
    match extra.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => kc_set(&kc_account(ex, "passphrase"), e)?,
        None => kc_delete(&kc_account(ex, "passphrase")),
    }
    Ok(())
}

/// Write one item through `security -i`: the command (with the secret) goes over stdin, never
/// into process arguments where `ps` could see it.
fn kc_set(account: &str, value: &str) -> Result<()> {
    use std::io::Write;
    if value.contains(['\n', '\r', '"', '\\']) { bail!("unsupported character in the value"); }
    let mut child = std::process::Command::new("security").arg("-i").stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?
        .write_all(format!("add-generic-password -U -s terminal-one -a \"{account}\" -w \"{value}\"\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || err.contains("rror") { bail!("Keychain: {}", err.trim()); }
    Ok(())
}

fn kc_delete(account: &str) {
    let _ = std::process::Command::new("security").args(["delete-generic-password", "-s", "terminal-one", "-a", account]).output();
}

/// Remove every Keychain item of `ex`.
pub fn delete_keys(ex: Exchange) {
    for what in ["key", "secret", "passphrase"] { kc_delete(&kc_account(ex, what)); }
}

/// Field labels for the key form: (key, secret, extra) — extra None when the venue has none.
pub fn key_fields(ex: Exchange) -> (&'static str, &'static str, Option<&'static str>) {
    match ex {
        Exchange::Okx | Exchange::Bitget => ("API key", "Secret", Some("Passphrase")),
        Exchange::Hyperliquid => ("Account address (0x...)", "API wallet private key", None),
        Exchange::Lighter => ("Account index:API key index", "API key private key", None),
        _ => ("API key", "Secret", None),
    }
}

/// Shell command that stores a key for `ex` (shown in the UI when keys are missing).
pub fn keychain_hint(ex: Exchange) -> String {
    let suffix = if testnet() { "-testnet" } else { "" };
    // a bare trailing -w makes `security` prompt, so the secret never lands in shell history
    let mut s = format!("security add-generic-password -U -s terminal-one -a {v}-key{suffix} -w\nsecurity add-generic-password -U -s terminal-one -a {v}-secret{suffix} -w", v = venue(ex));
    if needs_extra(ex) { s += &format!("\nsecurity add-generic-password -U -s terminal-one -a {}-passphrase{suffix} -w", venue(ex)); }
    s
}

pub fn hmac_hex(secret: &str, msg: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(msg.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// Decimals of a step like 0.001 -> 3, 0.5 -> 1, 1 -> 0.
pub fn step_dp(step: f64) -> usize {
    (0..12).find(|d| { let x = step * 10f64.powi(*d as i32); (x - x.round()).abs() < 1e-7 * x.max(1.0) }).unwrap_or(12)
}

/// `v` rounded to a multiple of `step` (down when `floor`), formatted exactly.
pub fn fmt_step(v: f64, step: f64, floor: bool) -> String {
    let n = v / step;
    let n = if floor { (n + 1e-9).floor() } else { n.round() };
    format!("{:.*}", step_dp(step), n * step)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tif { Gtc, Ioc, PostOnly, Fok }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Limit { price: f64, tif: Tif },
    Market,
    /// BBO limit: the venue prices it from its own book when the order arrives. `queue` = the
    /// order's own side (join the bid when buying), else the opposite side; `level` = book level.
    Bbo { queue: bool, level: u8 },
    /// Conditional order: waits on the venue until `trigger` is reached (by mark price, or last
    /// trade when `by_mark` is false), then sends a market order, or a limit at `limit`. Whether it
    /// acts as a stop or a take-profit follows from the trigger's side of the current price.
    Stop { trigger: f64, limit: Option<f64>, by_mark: bool },
    /// Trailing stop: market order once price retraces `callback_pct` percent from its best level
    /// since `activation` (or since placement).
    Trailing { callback_pct: f64, activation: Option<f64> },
}

impl Kind {
    /// Short machine-readable form for logs and the native UI: "market", "limit 123.4", "ioc 1",
    /// "fok 1", "post 1", "bbo queue 5", "stop 110 mark", "stop 110 limit 111 last", "trail 1.2% from 95".
    pub fn label(&self) -> String {
        match *self {
            Kind::Market => "market".into(),
            Kind::Limit { price, tif } => format!("{} {price}", match tif { Tif::Gtc => "limit", Tif::Ioc => "ioc", Tif::PostOnly => "post", Tif::Fok => "fok" }),
            Kind::Bbo { queue, level } => format!("bbo {} {level}", if queue { "queue" } else { "opponent" }),
            Kind::Stop { trigger, limit, by_mark } => format!("stop {trigger}{} {}", limit.map(|l| format!(" limit {l}")).unwrap_or_default(), if by_mark { "mark" } else { "last" }),
            Kind::Trailing { callback_pct, activation } => format!("trail {callback_pct}%{}", activation.map(|a| format!(" from {a}")).unwrap_or_default()),
        }
    }
}

/// Order types a venue accepts natively (the UI offers only these; TWAP and scaled orders are
/// built from plain limits and work everywhere).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Caps { pub fok: bool, pub stop: bool, pub trailing: bool, pub post_only: bool }

pub fn caps(ex: Exchange) -> Caps {
    match ex {
        Exchange::Bybit | Exchange::Binance => Caps { fok: true, stop: true, trailing: true, post_only: true },
        Exchange::Okx | Exchange::Bitget | Exchange::Gate => Caps { fok: true, stop: false, trailing: false, post_only: true },
        Exchange::Kraken | Exchange::Hyperliquid | Exchange::Lighter => Caps { fok: false, stop: false, trailing: false, post_only: true },
        _ => Caps::default(),
    }
}

/// BBO levels each venue accepts (Binance priceMatch QUEUE/OPPONENT[_5|_10|_20], Bybit bboLevel 1-5).
pub fn bbo_levels(ex: Exchange) -> &'static [u8] {
    match ex { Exchange::Binance => &[1, 5, 10, 20], Exchange::Bybit => &[1, 2, 3, 4, 5], _ => &[] }
}

/// An order expressed as intent: which position (`pos`: Buy = long, Sell = short) and whether it
/// opens / adds to it or closes / reduces it. Venue params are derived per position mode.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderReq {
    pub symbol: String, pub pos: Side, pub close: bool, pub kind: Kind, pub qty: f64,
    /// our id for the order (Bybit orderLinkId, Binance newClientOrderId): lets a leg whose
    /// request timed out be found in the private stream instead of being blindly re-sent
    pub client_id: Option<String>,
}

impl OrderReq {
    /// The buy/sell side sent to the venue: open long / close short buy, the others sell.
    pub fn side(&self) -> Side {
        match (self.pos, self.close) { (Side::Buy, false) | (Side::Sell, true) => Side::Buy, _ => Side::Sell }
    }
}

/// Account position mode for a symbol. Hedge = long and short tracked separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { OneWay, Hedge }

#[derive(Clone, Copy, Debug, Default)]
pub struct Rules { pub tick: f64, pub step: f64, pub min_qty: f64, pub min_notional: f64 }

#[derive(Clone, Debug)]
pub struct Position { pub ex: Exchange, pub symbol: String, pub side: Side, pub qty: f64, pub entry: f64, pub mark: f64, pub liq: Option<f64>, pub upnl: f64, pub lev: f64, pub margin: f64,
    /// margin mode: Some(true) cross, Some(false) isolated, None when the venue does not say
    pub cross: Option<bool> }

#[derive(Clone, Debug)]
pub struct OpenOrder { pub ex: Exchange, pub symbol: String, pub id: String, pub side: Side, pub price: f64, pub qty: f64, pub filled: f64, pub kind: String, pub reduce_only: bool, pub ts: i64,
    /// hedge mode: which position the order belongs to
    pub pos: Option<Side> }

/// The trading (margin) account, USD. Every other account is listed by `wallets`.
#[derive(Clone, Debug, Default)]
pub struct Balance {
    pub equity: f64, pub available: f64,
    /// Binance Portfolio Margin uniMMR: the whole account is liquidated below 1.05 (higher is safer);
    /// PM positions mostly carry no own liquidation price
    pub uni_mmr: Option<f64>,
    /// Bybit unified account maintenance margin rate, 0..1 (liquidation at 100%, lower is safer)
    pub mm_rate: Option<f64>,
    /// maintenance margin of the whole account, USD
    pub maint_margin: Option<f64>,
    /// Binance PM actualEquity (collateral after haircuts) / Bybit totalMarginBalance
    pub adj_equity: Option<f64>,
}

fn side_str(ex: Exchange, s: Side) -> &'static str {
    match (ex, s) { (Exchange::Binance, Side::Buy) => "BUY", (Exchange::Binance, Side::Sell) => "SELL", (_, Side::Buy) => "Buy", (_, Side::Sell) => "Sell" }
}

/// Native perp symbol for a base asset on a tradable venue (both use BASEUSDT).
pub fn symbol(base: &str) -> String { format!("{}USDT", base.to_uppercase()) }

// ---------------------------------------------------------------- Bybit v5

fn bybit_url() -> &'static str { if testnet() { "https://api-testnet.bybit.com" } else { "https://api.bybit.com" } }

/// v5 signature: HMAC_SHA256(secret, timestamp + key + recv_window + (query | json body))
pub fn bybit_sign(k: &Keys, ts: i64, recv: u32, payload: &str) -> String { hmac_hex(&k.secret, &format!("{ts}{}{recv}{payload}", k.key)) }

/// Receive windows: reads tolerate a slow proxy hop; orders and cancels keep the venue
/// default so an order stuck in transit for seconds is rejected rather than filled late.
const RECV_READ: u32 = 15_000;
const RECV_WRITE: u32 = 5_000;

async fn bybit(k: &Keys, get: bool, path: &str, params: Value) -> Result<Value> {
    let ts = crate::now_ms();
    let recv = if get { RECV_READ } else { RECV_WRITE };
    let (payload, req) = if get {
        let q = params.as_object().map(|o| o.iter().map(|(a, b)| format!("{a}={}", b.as_str().map(String::from).unwrap_or(b.to_string()))).collect::<Vec<_>>().join("&")).unwrap_or_default();
        (q.clone(), ws::http().get(format!("{}{path}?{q}", bybit_url())))
    } else {
        let b = params.to_string();
        (b.clone(), ws::http().post(format!("{}{path}", bybit_url())).header("Content-Type", "application/json").body(b))
    };
    let url = format!("{}{path}", bybit_url());
    ws::check_paused(&url)?;
    let r = req.header("X-BAPI-API-KEY", &k.key).header("X-BAPI-TIMESTAMP", ts.to_string())
        .header("X-BAPI-RECV-WINDOW", recv.to_string()).header("X-BAPI-SIGN", bybit_sign(k, ts, recv, &payload)).send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    // Bybit answers an IP ban with 403
    ws::note_limit(&url, if st == 403 { 418 } else { st }, None, &body);
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("bybit {path}: HTTP {st}: {}", &body[..body.len().min(200)]))?;
    if v["retCode"].as_i64() != Some(0) { bail!("bybit {path}: {} ({})", v["retMsg"].as_str().unwrap_or("?"), v["retCode"]); }
    Ok(v["result"].clone())
}

// ---------------------------------------------------------------- Binance USD-M

fn binance_url() -> &'static str { if testnet() { "https://testnet.binancefuture.com" } else { "https://fapi.binance.com" } }

fn papi_url() -> &'static str { "https://papi.binance.com" }

/// Portfolio Margin accounts reject /fapi and trade USD-M through /papi/v1/um. Detected once per
/// process from /papi/v1/account; only a definite answer is cached (network errors retry).
static BINANCE_PM: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

async fn binance_pm(k: &Keys) -> Result<bool> {
    if testnet() { return Ok(false); }
    if let Some(pm) = *BINANCE_PM.lock().unwrap() { return Ok(pm); }
    let pm = match binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/account", &[]).await {
        Ok(_) => true,
        // only a real API refusal means "not Portfolio Margin"; network or rate-limit errors retry later
        Err(e) if e.downcast_ref::<reqwest::Error>().is_some() || format!("{e}").contains("rate limited") => return Err(e),
        Err(_) => false,
    };
    *BINANCE_PM.lock().unwrap() = Some(pm);
    Ok(pm)
}

/// Raw Portfolio Margin account (diagnostics: `account BTC pm`).
pub async fn pm_account(k: &Keys) -> Result<Value> { binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/account", &[]).await }

/// The Portfolio Margin equivalent of a USD-M endpoint.
fn pm_path(fapi: &str) -> &str {
    match fapi {
        "/fapi/v3/account" => "/papi/v1/account",
        "/fapi/v3/positionRisk" => "/papi/v1/um/positionRisk",
        "/fapi/v1/openOrders" => "/papi/v1/um/openOrders",
        "/fapi/v1/order" => "/papi/v1/um/order",
        "/fapi/v1/positionSide/dual" => "/papi/v1/um/positionSide/dual",
        "/fapi/v1/symbolConfig" => "/papi/v1/um/symbolConfig",
        "/fapi/v1/algoOrder" => "/papi/v1/um/algo/order",
        "/fapi/v1/openAlgoOrders" => "/papi/v1/um/algo/openAlgoOrders",
        p => p,
    }
}

async fn binance(k: &Keys, method: reqwest::Method, path: &str, params: &[(&str, String)]) -> Result<Value> {
    if binance_pm(k).await? { binance_at(k, papi_url(), method, pm_path(path), params).await }
    else { binance_at(k, binance_url(), method, path, params).await }
}

async fn binance_at(k: &Keys, base: &str, method: reqwest::Method, path: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut q: Vec<String> = params.iter().map(|(a, b)| format!("{a}={b}")).collect();
    q.push(format!("recvWindow={}", if method == reqwest::Method::GET { RECV_READ } else { RECV_WRITE }));
    q.push(format!("timestamp={}", crate::now_ms()));
    let q = q.join("&");
    let url = format!("{base}{path}?{q}&signature={}", hmac_hex(&k.secret, &q));
    ws::check_paused(&url)?;
    let r = ws::http().request(method, &url).header("X-MBX-APIKEY", &k.key).send().await?;
    let st = r.status();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let body = r.text().await?;
    ws::note_limit(&url, st.as_u16(), retry_after, &body);
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !st.is_success() { bail!("binance {path}: {} ({})", v["msg"].as_str().unwrap_or("?"), v["code"]); }
    Ok(v)
}

// ---------------------------------------------------------------- venue-neutral API

fn n(v: &Value) -> f64 { crate::num(v) }

pub async fn rules(ex: Exchange, symbol: &str) -> Result<Rules> {
    match ex {
        Exchange::Bybit => {
            let v = ws::get_json(&format!("{}/v5/market/instruments-info?category=linear&symbol={symbol}", bybit_url())).await?;
            let i = &v["result"]["list"][0];
            Ok(Rules { tick: n(&i["priceFilter"]["tickSize"]), step: n(&i["lotSizeFilter"]["qtyStep"]), min_qty: n(&i["lotSizeFilter"]["minOrderQty"]),
                       min_notional: crate::opt_num(&i["lotSizeFilter"]["minNotionalValue"]).unwrap_or(0.0) })
        }
        Exchange::Binance => {
            let v = ws::get_json(&format!("{}/fapi/v1/exchangeInfo", binance_url())).await?;
            let s = v["symbols"].as_array().into_iter().flatten().find(|s| s["symbol"] == symbol).ok_or_else(|| anyhow!("{symbol} not listed"))?;
            let f = |t: &str| s["filters"].as_array().into_iter().flatten().find(|x| x["filterType"] == t).cloned().unwrap_or_default();
            Ok(Rules { tick: n(&f("PRICE_FILTER")["tickSize"]), step: n(&f("LOT_SIZE")["stepSize"]), min_qty: n(&f("LOT_SIZE")["minQty"]), min_notional: n(&f("MIN_NOTIONAL")["notional"]) })
        }
        Exchange::Okx => okx::rules(symbol).await,
        Exchange::Bitget => bitget::rules(symbol).await,
        Exchange::Gate => gate::rules(symbol).await,
        Exchange::Kraken => kraken::rules(symbol).await,
        Exchange::Hyperliquid => hyperliquid::rules(symbol).await,
        Exchange::Lighter => lighter::rules(symbol).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

/// Order params with price/size rounded to the rules; rejects sizes below the venue minimum.
pub fn checked(req: &OrderReq, r: &Rules, ref_px: f64) -> Result<(String, Option<String>)> {
    let qty = fmt_step(req.qty, r.step, true);
    let q: f64 = qty.parse()?;
    if q < r.min_qty || q <= 0.0 { bail!("size {q} below minimum {}", r.min_qty); }
    let px = match req.kind {
        Kind::Limit { price, .. } | Kind::Stop { limit: Some(price), .. } => Some(fmt_step(price, r.tick, false)),
        Kind::Market | Kind::Bbo { .. } | Kind::Stop { limit: None, .. } | Kind::Trailing { .. } => None,
    };
    if let Kind::Stop { trigger, .. } = req.kind { if !(trigger > 0.0 && trigger.is_finite()) { bail!("invalid trigger price {trigger}"); } }
    if let Kind::Trailing { callback_pct, .. } = req.kind { if !(0.1..=10.0).contains(&callback_pct) { bail!("trailing callback must be 0.1% to 10%, got {callback_pct}%"); } }
    let notional = q * px.as_deref().and_then(|p| p.parse().ok()).unwrap_or(ref_px);
    if !req.close && r.min_notional > 0.0 && notional < r.min_notional { bail!("notional {notional:.2} below minimum {}", r.min_notional); }
    Ok((qty, px))
}

/// Bybit v5 order body. Hedge: positionIdx 1 = long, 2 = short (reduceOnly allowed);
/// one-way: positionIdx 0.
pub fn bybit_body(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>, ref_px: f64, tick: f64) -> Value {
    let idx = match (mode, req.pos) { (Mode::OneWay, _) => 0, (Mode::Hedge, Side::Buy) => 1, (Mode::Hedge, Side::Sell) => 2 };
    let mut b = json!({"category": "linear", "symbol": req.symbol, "side": side_str(Exchange::Bybit, req.side()), "qty": qty,
                       "positionIdx": idx, "reduceOnly": req.close});
    if let Some(id) = &req.client_id { b["orderLinkId"] = id.clone().into(); }
    match req.kind {
        Kind::Market => { b["orderType"] = "Market".into(); }
        Kind::Limit { tif, .. } => {
            b["orderType"] = "Limit".into();
            b["price"] = px.unwrap_or_default().into();
            b["timeInForce"] = match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::PostOnly => "PostOnly", Tif::Fok => "FOK" }.into();
        }
        // conditional: triggerDirection 1 = fires when price rises to the trigger, 2 = falls to it
        Kind::Stop { trigger, limit, by_mark } => {
            b["orderType"] = if limit.is_some() { "Limit" } else { "Market" }.into();
            if limit.is_some() { b["price"] = px.unwrap_or_default().into(); b["timeInForce"] = "GTC".into(); }
            b["triggerPrice"] = fmt_step(trigger, tick, false).into();
            b["triggerDirection"] = if trigger > ref_px { 1 } else { 2 }.into();
            b["triggerBy"] = if by_mark { "MarkPrice" } else { "LastPrice" }.into();
        }
        // a trailing stop is a position setting on Bybit (see place); never sent as an order
        Kind::Trailing { .. } => { b["orderType"] = "Market".into(); }
        // no price: Bybit takes it from its book (bboSideType / bboLevel)
        Kind::Bbo { queue, level } => {
            b["orderType"] = "Limit".into();
            b["timeInForce"] = "GTC".into();
            b["bboSideType"] = if queue { "Queue" } else { "Counterparty" }.into();
            b["bboLevel"] = level.to_string().into();
        }
    }
    b
}

/// Binance USD-M order params. Hedge: positionSide LONG/SHORT and no reduceOnly (rejected there);
/// one-way: reduceOnly marks a close.
pub fn binance_params(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>) -> Vec<(&'static str, String)> {
    let mut p = vec![("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into())];
    if let Some(id) = &req.client_id { p.push(("newClientOrderId", id.clone())); }
    match req.kind {
        Kind::Market => p.push(("type", "MARKET".into())),
        Kind::Limit { tif, .. } => {
            p.push(("type", "LIMIT".into()));
            p.push(("price", px.unwrap_or_default().into()));
            p.push(("timeInForce", match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::PostOnly => "GTX", Tif::Fok => "FOK" }.into()));
        }
        // conditional and trailing orders go to the algo service (binance_algo_params)
        Kind::Stop { .. } | Kind::Trailing { .. } => p.push(("type", "MARKET".into())),
        // no price: Binance takes it from its book (priceMatch; cannot be sent with a price)
        Kind::Bbo { queue, level } => {
            p.push(("type", "LIMIT".into()));
            p.push(("timeInForce", "GTC".into()));
            let side = if queue { "QUEUE" } else { "OPPONENT" };
            p.push(("priceMatch", if level <= 1 { side.to_string() } else { format!("{side}_{level}") }));
        }
    }
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    p
}

/// Binance algo-order params for conditional and trailing orders (`POST /fapi/v1/algoOrder`).
/// STOP / TAKE_PROFIT follows from the trigger's side of the current price and the order side:
/// a buy triggered above the price is a stop, below it a take-profit (the reverse for a sell).
pub fn binance_algo_params(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>, ref_px: f64, tick: f64) -> Result<Vec<(&'static str, String)>> {
    let buy = req.side() == Side::Buy;
    let mut p = vec![("algoType", "CONDITIONAL".to_string()), ("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into())];
    match req.kind {
        Kind::Stop { trigger, limit, by_mark } => {
            let stop = (trigger > ref_px) == buy;
            let t = match (stop, limit.is_some()) { (true, false) => "STOP_MARKET", (false, false) => "TAKE_PROFIT_MARKET", (true, true) => "STOP", (false, true) => "TAKE_PROFIT" };
            p.push(("type", t.into()));
            p.push(("triggerPrice", fmt_step(trigger, tick, false)));
            p.push(("workingType", if by_mark { "MARK_PRICE" } else { "CONTRACT_PRICE" }.into()));
            if limit.is_some() { p.push(("price", px.unwrap_or_default().into())); p.push(("timeInForce", "GTC".into())); }
        }
        Kind::Trailing { callback_pct, activation } => {
            p.push(("type", "TRAILING_STOP_MARKET".into()));
            p.push(("callbackRate", format!("{callback_pct:.1}")));
            if let Some(a) = activation { p.push(("activatePrice", fmt_step(a, tick, false))); }
        }
        _ => bail!("not a conditional order"),
    }
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    if let Some(id) = &req.client_id { p.push(("clientAlgoId", id.clone())); }
    Ok(p)
}

/// Bybit trailing stop: a position setting (`/v5/position/trading-stop`), so only for closing.
/// The distance is the callback percent of the current price, in price units.
pub fn bybit_trailing_body(req: &OrderReq, mode: Mode, ref_px: f64, tick: f64) -> Result<Value> {
    let Kind::Trailing { callback_pct, activation } = req.kind else { bail!("not a trailing order") };
    if !req.close { bail!("bybit: a trailing stop protects an open position: use it from Close"); }
    let idx = match (mode, req.pos) { (Mode::OneWay, _) => 0, (Mode::Hedge, Side::Buy) => 1, (Mode::Hedge, Side::Sell) => 2 };
    let mut b = json!({"category": "linear", "symbol": req.symbol, "positionIdx": idx, "tpslMode": "Full",
                       "trailingStop": fmt_step(ref_px * callback_pct / 100.0, tick, false)});
    if let Some(a) = activation { b["activePrice"] = fmt_step(a, tick, false).into(); }
    Ok(b)
}

/// Venues that run TWAP themselves (the job keeps going with the app closed): allowed duration
/// in minutes. Others use the client-side TWAP.
pub fn native_twap(ex: Exchange) -> Option<(u32, u32)> {
    match ex {
        // POST /sapi/v1/algo/futures/newOrderTwap: 300-86400 s, notional at least 1,000 USDT
        Exchange::Binance => Some((5, 1440)),
        // twapOrder: 1-1440 min
        Exchange::Hyperliquid => Some((1, 1440)),
        _ => None,
    }
}

/// Binance futures TWAP params. Hedge: positionSide, never reduceOnly; one-way close: reduceOnly.
pub fn binance_twap_params(req: &OrderReq, mode: Mode, qty: &str, minutes: u32, limit: Option<&str>, client_id: &str) -> Vec<(&'static str, String)> {
    let mut p = vec![("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into()),
                     ("duration", (minutes as u64 * 60).to_string()), ("clientAlgoId", client_id.into())];
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    if let Some(l) = limit { p.push(("limitPrice", l.into())); }
    p
}

/// Start a venue-run TWAP; returns the id to cancel it with. Binance: notional at least 1,000
/// USDT, weight 3000 (UID), so it is placed once and never polled.
pub async fn place_twap(ex: Exchange, k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode, minutes: u32, limit: Option<f64>, randomize: bool) -> Result<String> {
    let Some((lo, hi)) = native_twap(ex) else { bail!("{ex:?} has no native TWAP") };
    if !(lo..=hi).contains(&minutes) { bail!("{ex:?}: TWAP duration must be {lo} to {hi} minutes"); }
    match ex {
        Exchange::Binance => {
            let qty = fmt_step(req.qty, r.step, true);
            let notional = qty.parse::<f64>().unwrap_or(0.0) * limit.unwrap_or(ref_px);
            if notional < 1_000.0 { bail!("binance: a TWAP needs at least 1,000 USDT notional (this is {notional:.0})"); }
            let id = format!("t1twap{}", crate::now_ms());
            let lim = limit.map(|l| fmt_step(l, r.tick, false));
            let v = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/algo/futures/newOrderTwap", &binance_twap_params(req, mode, &qty, minutes, lim.as_deref(), &id)).await?;
            if v["success"] != true { bail!("binance TWAP: {}", v["msg"].as_str().unwrap_or("rejected")); }
            Ok(v["clientAlgoId"].as_str().unwrap_or(&id).to_string())
        }
        Exchange::Hyperliquid => hyperliquid::place_twap(k, req, minutes, randomize).await,
        _ => unreachable!(),
    }
}

pub async fn cancel_twap(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Binance => {
            let v = binance_at(k, sapi_url(), reqwest::Method::DELETE, "/sapi/v1/algo/futures/order", &[("clientAlgoId", id.into())]).await?;
            if v["success"] == false { bail!("binance TWAP cancel: {}", v["msg"].as_str().unwrap_or("rejected")); }
            Ok(())
        }
        Exchange::Hyperliquid => hyperliquid::cancel_twap(k, symbol, id).await,
        _ => bail!("{ex:?} has no native TWAP"),
    }
}

/// Position mode of the account for `symbol` (read only; never changed from here).
pub async fn mode(ex: Exchange, k: &Keys, symbol: &str) -> Result<Mode> {
    match ex {
        // per-symbol query returns one row per positionIdx even when flat: 1/2 means hedge
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "symbol": symbol})).await?;
            let hedge = v["list"].as_array().into_iter().flatten().any(|p| p["positionIdx"].as_i64().is_some_and(|i| i > 0));
            Ok(if hedge { Mode::Hedge } else { Mode::OneWay })
        }
        Exchange::Binance => {
            let v = binance(k, reqwest::Method::GET, "/fapi/v1/positionSide/dual", &[]).await?;
            Ok(if v["dualSidePosition"].as_bool() == Some(true) { Mode::Hedge } else { Mode::OneWay })
        }
        Exchange::Okx => okx::mode(k, symbol).await,
        Exchange::Bitget => bitget::mode(k, symbol).await,
        Exchange::Gate => gate::mode(k, symbol).await,
        Exchange::Kraken => kraken::mode(k, symbol).await,
        Exchange::Hyperliquid => hyperliquid::mode(k, symbol).await,
        Exchange::Lighter => lighter::mode(k, symbol).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

/// The account's leverage setting for `symbol` (weight 5 on Binance; read once per symbol).
pub async fn leverage(ex: Exchange, k: &Keys, symbol: &str) -> Result<f64> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "symbol": symbol})).await?;
            v["list"][0]["leverage"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("bybit: no leverage for {symbol}"))
        }
        Exchange::Binance => {
            let v = binance(k, reqwest::Method::GET, "/fapi/v1/symbolConfig", &[("symbol", symbol.into())]).await?;
            v[0]["leverage"].as_f64().ok_or_else(|| anyhow!("binance: no leverage for {symbol}"))
        }
        Exchange::Okx => okx::leverage(k, symbol).await,
        Exchange::Bitget => bitget::leverage(k, symbol).await,
        Exchange::Gate => gate::leverage(k, symbol).await,
        Exchange::Kraken => kraken::leverage(k, symbol).await,
        Exchange::Hyperliquid => hyperliquid::leverage(k, symbol).await,
        Exchange::Lighter => lighter::leverage(k, symbol).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

pub async fn place(ex: Exchange, k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode) -> Result<String> {
    let (qty, px) = checked(req, r, ref_px)?;
    if let Kind::Bbo { level, .. } = req.kind { if !bbo_levels(ex).contains(&level) { bail!("{ex:?} has no BBO level {level}"); } }
    let c = caps(ex);
    match req.kind {
        Kind::Limit { tif: Tif::Fok, .. } if !c.fok => bail!("{ex:?} has no fill-or-kill orders"),
        Kind::Stop { .. } if !c.stop => bail!("{ex:?}: conditional orders are not supported here yet"),
        Kind::Trailing { .. } if !c.trailing => bail!("{ex:?}: trailing stops are not supported here yet"),
        _ => {}
    }
    match ex {
        Exchange::Bybit if matches!(req.kind, Kind::Trailing { .. }) => {
            bybit(k, false, "/v5/position/trading-stop", bybit_trailing_body(req, mode, ref_px, r.tick)?).await?;
            Ok("trailing".into())
        }
        Exchange::Bybit => Ok(bybit(k, false, "/v5/order/create", bybit_body(req, mode, &qty, px.as_deref(), ref_px, r.tick)).await?["orderId"].as_str().unwrap_or_default().to_string()),
        Exchange::Binance if matches!(req.kind, Kind::Stop { .. } | Kind::Trailing { .. }) =>
            tpsl::binance_place(k, &binance_algo_params(req, mode, &qty, px.as_deref(), ref_px, r.tick)?).await,
        Exchange::Binance => Ok(binance(k, reqwest::Method::POST, "/fapi/v1/order", &binance_params(req, mode, &qty, px.as_deref())).await?["orderId"].to_string()),
        Exchange::Okx => okx::place(k, req, r, ref_px, mode).await,
        Exchange::Bitget => bitget::place(k, req, r, ref_px, mode).await,
        Exchange::Gate => gate::place(k, req, r, ref_px, mode).await,
        Exchange::Kraken => kraken::place(k, req, r, ref_px, mode).await,
        Exchange::Hyperliquid => hyperliquid::place(k, req, r, ref_px, mode).await,
        Exchange::Lighter => lighter::place(k, req, r, ref_px, mode).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

pub async fn cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Bybit => { bybit(k, false, "/v5/order/cancel", json!({"category": "linear", "symbol": symbol, "orderId": id})).await?; }
        Exchange::Binance => { binance(k, reqwest::Method::DELETE, "/fapi/v1/order", &[("symbol", symbol.into()), ("orderId", id.into())]).await?; }
        Exchange::Okx => { okx::cancel(k, symbol, id).await?; }
        Exchange::Bitget => { bitget::cancel(k, symbol, id).await?; }
        Exchange::Gate => { gate::cancel(k, symbol, id).await?; }
        Exchange::Kraken => { kraken::cancel(k, symbol, id).await?; }
        Exchange::Hyperliquid => { hyperliquid::cancel(k, symbol, id).await?; }
        Exchange::Lighter => { lighter::cancel(k, symbol, id).await?; }
        _ => bail!("{ex:?} trading not implemented"),
    }
    Ok(())
}

pub async fn positions(ex: Exchange, k: &Keys) -> Result<Vec<Position>> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "settleCoin": "USDT"})).await?;
            Ok(v["list"].as_array().into_iter().flatten().filter(|p| n(&p["size"]) > 0.0).map(bybit_position).collect())
        }
        Exchange::Binance => {
            let v = binance(k, reqwest::Method::GET, "/fapi/v3/positionRisk", &[]).await?;
            Ok(v.as_array().into_iter().flatten().filter(|p| n(&p["positionAmt"]) != 0.0).map(|p| {
                let amt = n(&p["positionAmt"]);
                // Portfolio Margin rows carry leverage instead of initialMargin
                let margin = crate::opt_num(&p["initialMargin"]).unwrap_or_else(|| { let l = n(&p["leverage"]); if l > 0.0 { n(&p["notional"]).abs() / l } else { 0.0 } });
                // hedge mode reports LONG / SHORT rows; one-way reports BOTH with a signed amount
                let side = match p["positionSide"].as_str() { Some("LONG") => Side::Buy, Some("SHORT") => Side::Sell, _ => if amt < 0.0 { Side::Sell } else { Side::Buy } };
                Position { ex, symbol: p["symbol"].as_str().unwrap_or_default().into(), side,
                    qty: amt.abs(), entry: n(&p["entryPrice"]), mark: n(&p["markPrice"]), liq: crate::opt_num(&p["liquidationPrice"]).filter(|x| *x > 0.0),
                    upnl: n(&p["unRealizedProfit"]), lev: if margin > 0.0 { n(&p["notional"]).abs() / margin } else { 0.0 }, margin,
                    // v3 USD-M rows: isolatedWallet > 0 is isolated; Portfolio Margin is always cross
                    cross: Some(is_pm() || match p["marginType"].as_str() { Some(m) => m != "isolated", None => n(&p["isolatedWallet"]) == 0.0 }) }
            }).collect())
        }
        Exchange::Okx => okx::positions(k).await,
        Exchange::Bitget => bitget::positions(k).await,
        Exchange::Gate => gate::positions(k).await,
        Exchange::Kraken => kraken::positions(k).await,
        Exchange::Hyperliquid => hyperliquid::positions(k).await,
        Exchange::Lighter => lighter::positions(k).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

pub async fn open_orders(ex: Exchange, k: &Keys) -> Result<Vec<OpenOrder>> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/order/realtime", json!({"category": "linear", "settleCoin": "USDT"})).await?;
            // TP/SL rows live in the same list; they are reported by tpsl::list instead
            Ok(v["list"].as_array().into_iter().flatten().filter(|o| tpsl::bybit_row(o).is_none()).map(bybit_order).collect())
        }
        Exchange::Binance => {
            let v = binance(k, reqwest::Method::GET, "/fapi/v1/openOrders", &[]).await?;
            Ok(v.as_array().into_iter().flatten().map(|o| OpenOrder {
                ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].to_string(),
                side: if o["side"] == "SELL" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["origQty"]), filled: n(&o["executedQty"]),
                kind: o["type"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: o["time"].as_i64().unwrap_or(0),
                pos: match o["positionSide"].as_str() { Some("LONG") => Some(Side::Buy), Some("SHORT") => Some(Side::Sell), _ => None },
            }).collect())
        }
        Exchange::Okx => okx::open_orders(k).await,
        Exchange::Bitget => bitget::open_orders(k).await,
        Exchange::Gate => gate::open_orders(k).await,
        Exchange::Kraken => kraken::open_orders(k).await,
        Exchange::Hyperliquid => hyperliquid::open_orders(k).await,
        Exchange::Lighter => lighter::open_orders(k).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

pub async fn balance(ex: Exchange, k: &Keys) -> Result<Balance> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/account/wallet-balance", json!({"accountType": "UNIFIED"})).await?;
            let a = &v["list"][0];
            Ok(Balance { equity: n(&a["totalEquity"]), available: n(&a["totalAvailableBalance"]), uni_mmr: None, mm_rate: crate::opt_num(&a["accountMMRate"]),
                maint_margin: crate::opt_num(&a["totalMaintenanceMargin"]), adj_equity: crate::opt_num(&a["totalMarginBalance"]) })
        }
        Exchange::Binance => {
            let v = binance(k, reqwest::Method::GET, "/fapi/v3/account", &[]).await?;
            // Portfolio Margin (/papi/v1/account) vs USD-M (/fapi/v3/account) field names
            Ok(if v.get("accountEquity").is_some() {
                Balance { equity: n(&v["accountEquity"]), available: n(&v["totalAvailableBalance"]), uni_mmr: crate::opt_num(&v["uniMMR"]), mm_rate: None,
                    maint_margin: crate::opt_num(&v["accountMaintMargin"]), adj_equity: crate::opt_num(&v["actualEquity"]) }
            } else {
                Balance { equity: n(&v["totalMarginBalance"]), available: n(&v["availableBalance"]), ..Default::default() }
            })
        }
        Exchange::Okx => okx::balance(k).await,
        Exchange::Bitget => bitget::balance(k).await,
        Exchange::Gate => gate::balance(k).await,
        Exchange::Kraken => kraken::balance(k).await,
        Exchange::Hyperliquid => hyperliquid::balance(k).await,
        Exchange::Lighter => lighter::balance(k).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

/// One Bybit v5 position row (REST list and the private `position` push share the shape).
/// A closed hedge-mode row has side "" and size 0; positionIdx says which side it was.
fn bybit_position(p: &Value) -> Position {
    let side = match (p["side"].as_str(), p["positionIdx"].as_i64()) { (Some("Sell"), _) | (_, Some(2)) => Side::Sell, _ => Side::Buy };
    Position {
        ex: Exchange::Bybit, symbol: p["symbol"].as_str().unwrap_or_default().into(), side,
        qty: n(&p["size"]), entry: crate::opt_num(&p["avgPrice"]).or(crate::opt_num(&p["entryPrice"])).unwrap_or(0.0), mark: n(&p["markPrice"]),
        liq: crate::opt_num(&p["liqPrice"]).filter(|x| *x > 0.0), upnl: n(&p["unrealisedPnl"]), lev: n(&p["leverage"]), margin: n(&p["positionIM"]),
        cross: p["tradeMode"].as_i64().map(|m| m == 0),
    }
}

fn bybit_order(o: &Value) -> OpenOrder {
    OpenOrder {
        ex: Exchange::Bybit, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].as_str().unwrap_or_default().into(),
        side: if o["side"] == "Sell" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]),
        kind: o["orderType"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: n(&o["createdTime"]) as i64,
        pos: match o["positionIdx"].as_i64() { Some(1) => Some(Side::Buy), Some(2) => Some(Side::Sell), _ => None },
    }
}

// ---------------------------------------------------------------- private streams

/// Account changes pushed by a venue's private stream. REST is only used to take a snapshot
/// when a stream (re)connects and for a slow reconcile.
#[derive(Debug)]
pub enum AccEvent {
    /// stream (re)connected and subscribed: take a REST snapshot (deltas may have been missed)
    Resync,
    /// upsert by (symbol, side); qty 0 removes. `one_way` rows (Binance BOTH, Bybit idx 0)
    /// replace either side. Zero mark/lev/margin and None liq mean "not in this push, keep".
    Position { p: Position, one_way: bool },
    /// live mark price for a symbol (upnl is recomputed for open positions)
    Mark { symbol: String, mark: f64 },
    Order(OpenOrder),
    OrderDone { id: String },
    /// margin account totals pushed by the venue (Bybit wallet)
    Wallet { equity: f64, available: f64 },
    /// balances changed but the push has no account totals (Binance): refetch the balance
    BalanceDirty,
    /// a TP/SL (whole-position or partial) appeared or changed; removed with TpSlDone
    TpSl(tpsl::TpSl),
    TpSlDone { id: String },
}

pub type AccTx = tokio::sync::mpsc::UnboundedSender<(Exchange, AccEvent)>;

/// Runs a venue's private account stream forever (reconnecting, re-snapshotting).
pub async fn stream(ex: Exchange, k: Keys, tx: AccTx) {
    match ex {
        Exchange::Bybit => bybit_stream(k, tx).await,
        Exchange::Binance => binance_stream(k, tx).await,
        Exchange::Okx => okx::stream(k, tx).await,
        Exchange::Bitget => bitget::stream(k, tx).await,
        Exchange::Gate => gate::stream(k, tx).await,
        Exchange::Kraken => kraken::stream(k, tx).await,
        Exchange::Hyperliquid => hyperliquid::stream(k, tx).await,
        Exchange::Lighter => lighter::stream(k, tx).await,
        _ => {}
    }
}

async fn bybit_stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Bybit;
    let url = if testnet() { "wss://stream-testnet.bybit.com/v5/private" } else { "wss://stream.bybit.com/v5/private" };
    loop {
        // auth: HMAC(secret, "GET/realtime" + expires), computed fresh for every connection
        let expires = crate::now_ms() + 10_000;
        let sig = hmac_hex(&k.secret, &format!("GET/realtime{expires}"));
        let spec = ws::Spec::new("bybit account", url)
            .sub(json!({"op": "auth", "args": [k.key, expires, sig]}).to_string())
            .sub(json!({"op": "subscribe", "args": ["position.linear", "order.linear", "wallet"]}).to_string())
            .ping(std::time::Duration::from_secs(20), r#"{"op":"ping"}"#);
        let mut auth_failed = false;
        let r = ws::once(&spec, |f| {
            let ws::Frame::Text(s) = f else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
            let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
            match (v["op"].as_str(), v["topic"].as_str()) {
                (Some("auth"), _) if v["success"] == false => { eprintln!("[bybit account] auth failed: {}", v["ret_msg"]); auth_failed = true; return false; }
                (Some("subscribe"), _) if v["success"] == true => send(AccEvent::Resync),
                (_, Some("position.linear")) => for p in v["data"].as_array().into_iter().flatten() {
                    send(AccEvent::Position { p: bybit_position(p), one_way: p["positionIdx"].as_i64() == Some(0) });
                },
                (_, Some("order.linear")) => for o in v["data"].as_array().into_iter().flatten() {
                    // TP/SL rows are kept apart from ordinary resting orders
                    if let Some((t, active)) = tpsl::bybit_row(o) {
                        send(if active { AccEvent::TpSl(t) } else { AccEvent::TpSlDone { id: o["orderId"].as_str().unwrap_or_default().into() } });
                        continue;
                    }
                    let open = matches!(o["orderStatus"].as_str(), Some("New" | "PartiallyFilled" | "Untriggered"));
                    send(if open { AccEvent::Order(bybit_order(o)) } else { AccEvent::OrderDone { id: o["orderId"].as_str().unwrap_or_default().into() } });
                },
                (_, Some("wallet")) => for w in v["data"].as_array().into_iter().flatten().filter(|w| w["accountType"] == "UNIFIED") {
                    send(AccEvent::Wallet { equity: n(&w["totalEquity"]), available: n(&w["totalAvailableBalance"]) });
                },
                _ => {}
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[bybit account] {e:#}"); }
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 2 })).await;
    }
}

fn binance_ws() -> &'static str { if testnet() { "wss://stream.binancefuture.com" } else { "wss://fstream.binance.com" } }

/// listenKey create (POST) / keepalive (PUT): API key header only, no signature.
async fn listen_key(k: &Keys, pm: bool, method: reqwest::Method) -> Result<String> {
    let url = if pm { format!("{}/papi/v1/listenKey", papi_url()) } else { format!("{}/fapi/v1/listenKey", binance_url()) };
    ws::check_paused(&url)?;
    let r = ws::http().request(method, &url).header("X-MBX-APIKEY", &k.key).send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    ws::note_limit(&url, st, None, &body);
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !(200..300).contains(&st) { bail!("binance listenKey: {} ({})", v["msg"].as_str().unwrap_or(&body), v["code"]); }
    Ok(v["listenKey"].as_str().unwrap_or_default().to_string())
}

async fn binance_stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Binance;
    // mark prices of every USD-M symbol, for live upnl of any open position (public, no weight)
    // ponytail: the all-symbol array (~600 rows/s); subscribe per position symbol if bandwidth matters
    let txm = tx.clone();
    tokio::spawn(ws::run(ws::Spec::new("binance marks", format!("{}/market/ws/!markPrice@arr@1s", binance_ws())), move |f| {
        let ws::Frame::Text(s) = f else { return };
        let Ok(v) = serde_json::from_str::<Value>(s) else { return };
        for m in v.as_array().into_iter().flatten() {
            let _ = txm.send((ex, AccEvent::Mark { symbol: m["s"].as_str().unwrap_or_default().into(), mark: n(&m["p"]) }));
        }
    }));
    loop {
        let res: Result<()> = async {
            let pm = binance_pm(&k).await?;
            let key = listen_key(&k, pm, reqwest::Method::POST).await?;
            // Portfolio Margin: /pm/ws/<key>; USD-M: the routed /private endpoint (unrouted /ws
            // URLs only carry public data since the 2026 split)
            let url = if pm { format!("{}/pm/ws/{key}", binance_ws()) } else { format!("{}/private/ws?listenKey={key}", binance_ws()) };
            let _ = tx.send((ex, AccEvent::Resync));
            let k2 = k.clone();
            let keepalive = async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
                    if let Err(e) = listen_key(&k2, pm, reqwest::Method::PUT).await { eprintln!("[binance account] keepalive: {e:#}"); }
                }
            };
            let spec = ws::Spec { idle: std::time::Duration::from_secs(3600), ..ws::Spec::new("binance account", url) };
            let session = ws::once(&spec, |f| {
                let ws::Frame::Text(s) = f else { return true };
                let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
                let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
                // Portfolio Margin also streams cross-margin / coin-M events; only USD-M is traded here
                if v["fs"].as_str().is_some_and(|fs| fs != "UM") { return true; }
                match v["e"].as_str() {
                    Some("ACCOUNT_UPDATE") => {
                        for p in v["a"]["P"].as_array().into_iter().flatten() {
                            let amt = n(&p["pa"]);
                            let ps = p["ps"].as_str().unwrap_or("BOTH");
                            let side = match ps { "LONG" => Side::Buy, "SHORT" => Side::Sell, _ => if amt < 0.0 { Side::Sell } else { Side::Buy } };
                            send(AccEvent::Position { one_way: ps == "BOTH", p: Position {
                                ex, symbol: p["s"].as_str().unwrap_or_default().into(), side, qty: amt.abs(), entry: n(&p["ep"]),
                                mark: 0.0, liq: None, upnl: n(&p["up"]), lev: 0.0, margin: 0.0, cross: if is_pm() { Some(true) } else { p["mt"].as_str().map(|m| m != "isolated") } } });
                        }
                        send(AccEvent::BalanceDirty);
                    }
                    Some("ORDER_TRADE_UPDATE") => {
                        let o = &v["o"];
                        let id = o["i"].to_string();
                        if matches!(o["X"].as_str(), Some("NEW" | "PARTIALLY_FILLED")) {
                            send(AccEvent::Order(OpenOrder {
                                ex, symbol: o["s"].as_str().unwrap_or_default().into(), id, side: if o["S"] == "SELL" { Side::Sell } else { Side::Buy },
                                price: n(&o["p"]), qty: n(&o["q"]), filled: n(&o["z"]), kind: o["o"].as_str().unwrap_or_default().into(),
                                reduce_only: o["R"].as_bool().unwrap_or(false), ts: o["T"].as_i64().unwrap_or(0),
                                pos: match o["ps"].as_str() { Some("LONG") => Some(Side::Buy), Some("SHORT") => Some(Side::Sell), _ => None },
                            }));
                        } else {
                            send(AccEvent::OrderDone { id });
                        }
                    }
                    Some("balanceUpdate" | "outboundAccountPosition") => send(AccEvent::BalanceDirty),
                    Some("ALGO_UPDATE") => if let Some((t, active)) = tpsl::binance_algo_update(&v) {
                        send(if active { AccEvent::TpSl(t) } else { AccEvent::TpSlDone { id: t.id } });
                    },
                    Some("listenKeyExpired") => return false,
                    _ => {}
                }
                true
            });
            tokio::select! { r = session => r, _ = keepalive => Ok(()) }
        }.await;
        if let Err(e) = res { eprintln!("[binance account] {e:#}"); }
        // a paused host (rate limit) fails fast: wait long enough not to spin
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

// ---------------------------------------------------------------- history (on demand only)

#[derive(Clone, Debug)]
pub struct Fill { pub ex: Exchange, pub symbol: String, pub side: Side, pub price: f64, pub qty: f64, pub fee: f64, pub realized: Option<f64>, pub ts: i64 }

#[derive(Clone, Debug)]
pub struct HistOrder { pub ex: Exchange, pub symbol: String, pub side: Side, pub kind: String, pub price: f64, pub avg: f64, pub qty: f64, pub filled: f64, pub status: String, pub ts: i64 }

/// A closed (or partly closed) position. Binance only reports realized PnL per close, so qty /
/// entry / exit are None there.
#[derive(Clone, Debug)]
pub struct ClosedPnl { pub ex: Exchange, pub symbol: String, pub long: Option<bool>, pub qty: Option<f64>, pub entry: Option<f64>, pub exit: Option<f64>, pub pnl: f64, pub ts: i64 }

#[derive(Clone, Debug, Default)]
pub struct History { pub orders: Vec<HistOrder>, pub fills: Vec<Fill>, pub closed: Vec<ClosedPnl> }

/// Recent order / trade / closed-PnL history (last ~7 days). Never on a timer: Binance needs a
/// symbol per query, so `symbols` (unified) bounds the weight (10 per symbol + 30).
pub async fn history(ex: Exchange, k: &Keys, symbols: &[String]) -> Result<History> {
    let side = |s: &Value| if s.as_str().is_some_and(|x| x.eq_ignore_ascii_case("sell")) { Side::Sell } else { Side::Buy };
    let mut h = History::default();
    match ex {
        Exchange::Bybit => {
            let o = bybit(k, true, "/v5/order/history", json!({"category": "linear", "settleCoin": "USDT", "limit": "50"})).await?;
            h.orders = o["list"].as_array().into_iter().flatten().map(|o| HistOrder {
                ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), side: side(&o["side"]), kind: o["orderType"].as_str().unwrap_or_default().into(),
                price: n(&o["price"]), avg: n(&o["avgPrice"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]), status: o["orderStatus"].as_str().unwrap_or_default().into(), ts: n(&o["createdTime"]) as i64,
            }).collect();
            let e = bybit(k, true, "/v5/execution/list", json!({"category": "linear", "settleCoin": "USDT", "limit": "100"})).await?;
            h.fills = e["list"].as_array().into_iter().flatten().map(|f| Fill {
                ex, symbol: f["symbol"].as_str().unwrap_or_default().into(), side: side(&f["side"]), price: n(&f["execPrice"]), qty: n(&f["execQty"]),
                fee: n(&f["execFee"]), realized: crate::opt_num(&f["execPnl"]), ts: n(&f["execTime"]) as i64,
            }).collect();
            let c = bybit(k, true, "/v5/position/closed-pnl", json!({"category": "linear", "limit": "100"})).await?;
            // the row's side is the closing order's side: Sell closed a long
            h.closed = c["list"].as_array().into_iter().flatten().map(|c| ClosedPnl {
                ex, symbol: c["symbol"].as_str().unwrap_or_default().into(), long: Some(side(&c["side"]) == Side::Sell), qty: crate::opt_num(&c["closedSize"]),
                entry: crate::opt_num(&c["avgEntryPrice"]), exit: crate::opt_num(&c["avgExitPrice"]), pnl: n(&c["closedPnl"]), ts: n(&c["updatedTime"]) as i64,
            }).collect();
        }
        Exchange::Binance => {
            let pm = binance_pm(k).await?;
            let (base, (orders, trades, income)) = if pm { (papi_url(), ("/papi/v1/um/allOrders", "/papi/v1/um/userTrades", "/papi/v1/um/income")) }
                else { (binance_url(), ("/fapi/v1/allOrders", "/fapi/v1/userTrades", "/fapi/v1/income")) };
            for sym in symbols.iter().take(6) {
                let q = [("symbol", sym.clone()), ("limit", "50".to_string())];
                let o = binance_at(k, base, reqwest::Method::GET, orders, &q).await?;
                h.orders.extend(o.as_array().into_iter().flatten().map(|o| HistOrder {
                    ex, symbol: sym.clone(), side: side(&o["side"]), kind: o["type"].as_str().unwrap_or_default().into(), price: n(&o["price"]), avg: n(&o["avgPrice"]),
                    qty: n(&o["origQty"]), filled: n(&o["executedQty"]), status: o["status"].as_str().unwrap_or_default().into(), ts: o["time"].as_i64().unwrap_or(0),
                }));
                let t = binance_at(k, base, reqwest::Method::GET, trades, &q).await?;
                h.fills.extend(t.as_array().into_iter().flatten().map(|f| Fill {
                    ex, symbol: sym.clone(), side: side(&f["side"]), price: n(&f["price"]), qty: n(&f["qty"]), fee: n(&f["commission"]),
                    realized: crate::opt_num(&f["realizedPnl"]), ts: f["time"].as_i64().unwrap_or(0),
                }));
            }
            // closes reconstructed from fills that realized PnL (side, size, exit VWAP, entry implied);
            // symbols whose fills weren't fetched fall back to the income ledger (PnL only)
            h.closed = closes_from_fills(&h.fills);
            let c = binance_at(k, base, reqwest::Method::GET, income, &[("incomeType", "REALIZED_PNL".into()), ("limit", "100".into())]).await?;
            let rows: Vec<ClosedPnl> = c.as_array().into_iter().flatten().map(|c| ClosedPnl {
                ex, symbol: c["symbol"].as_str().unwrap_or_default().into(), long: None, qty: None, entry: None, exit: None, pnl: n(&c["income"]), ts: c["time"].as_i64().unwrap_or(0),
            }).filter(|c| !symbols.contains(&c.symbol)).collect();
            h.closed.extend(merge_same_second(rows));
        }
        _ => bail!("{ex:?} history not implemented"),
    }
    h.orders.sort_by(|a, b| b.ts.cmp(&a.ts));
    h.fills.sort_by(|a, b| b.ts.cmp(&a.ts));
    h.closed.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(h)
}

/// Group closing fills (realized != 0) by (symbol, second, side) into closes: a Sell that realizes
/// PnL closed a long. Entry is implied from exit and PnL per unit.
fn closes_from_fills(fills: &[Fill]) -> Vec<ClosedPnl> {
    let mut out: Vec<ClosedPnl> = vec![];
    for f in fills.iter().filter(|f| f.realized.is_some_and(|r| r != 0.0)) {
        let long = f.side == Side::Sell;
        let r = f.realized.unwrap_or(0.0);
        match out.iter_mut().find(|c| c.ex == f.ex && c.symbol == f.symbol && c.long == Some(long) && c.ts / 1000 == f.ts / 1000) {
            Some(c) => {
                let q = c.qty.unwrap_or(0.0);
                c.exit = Some((c.exit.unwrap_or(0.0) * q + f.price * f.qty) / (q + f.qty));
                c.qty = Some(q + f.qty);
                c.pnl += r;
            }
            None => out.push(ClosedPnl { ex: f.ex, symbol: f.symbol.clone(), long: Some(long), qty: Some(f.qty), entry: None, exit: Some(f.price), pnl: r, ts: f.ts }),
        }
    }
    for c in &mut out {
        if let (Some(q), Some(x)) = (c.qty, c.exit) { if q > 0.0 { c.entry = Some(if c.long == Some(true) { x - c.pnl / q } else { x + c.pnl / q }); } }
    }
    out
}

/// Ledger rows of one close arrive per fill: fold rows of the same symbol and second.
fn merge_same_second(rows: Vec<ClosedPnl>) -> Vec<ClosedPnl> {
    let mut out: Vec<ClosedPnl> = vec![];
    for r in rows {
        match out.iter_mut().find(|c| c.symbol == r.symbol && c.ts / 1000 == r.ts / 1000) { Some(c) => c.pnl += r.pnl, None => out.push(r) }
    }
    out
}

// ---------------------------------------------------------------- wallets and transfers

/// Account ids. Bybit: UNIFIED (margin), FUND, EARN. Binance: PM or USDM (margin), SPOT, FUNDING, EARN.
pub const MARGIN_IDS: [&str; 3] = ["UNIFIED", "PM", "USDM"];

#[derive(Clone, Debug, Default)]
pub struct Coin {
    pub coin: String,
    pub qty: f64,
    /// what can leave this account now (transferable / redeemable)
    pub free: f64,
    pub usd: f64,
    /// Earn product the coin sits in (redeem source); None elsewhere or when not redeemable here
    pub product: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Wallet { pub id: String, pub usd: f64, pub coins: Vec<Coin> }

/// Every account the key can read, with coins valued in USD. Expensive on Binance (Earn
/// position = 150 weight): call on demand only, never on a timer. Per-account failures (missing
/// permission) are returned as notes so the other accounts still show.
pub async fn wallets(ex: Exchange, k: &Keys) -> (Vec<Wallet>, Vec<String>) {
    let (mut out, mut notes) = (vec![], vec![]);
    let mut add = |id: &str, coins: Result<Vec<Coin>>, px: &std::collections::HashMap<String, f64>| match coins {
        Ok(mut cs) => {
            cs.retain(|c| c.qty > 0.0);
            for c in &mut cs { c.usd = if is_usd(&c.coin) { c.qty } else { px.get(&c.coin).map_or(0.0, |p| p * c.qty) }; }
            cs.sort_by(|a, b| b.usd.total_cmp(&a.usd));
            out.push(Wallet { id: id.into(), usd: cs.iter().map(|c| c.usd).sum(), coins: cs });
        }
        Err(e) => notes.push(format!("{id}: {e:#}")),
    };
    match ex {
        Exchange::Bybit => {
            let px = bybit_usd_prices().await.unwrap_or_default();
            // UNIFIED: holdings from wallet-balance, then transferable amounts 10 coins per query
            // (the coin-balance endpoint refuses to list a unified account wholesale)
            let r: Result<Vec<Coin>> = async {
                let v = bybit(k, true, "/v5/account/wallet-balance", json!({"accountType": "UNIFIED"})).await?;
                let mut cs: Vec<Coin> = v["list"][0]["coin"].as_array().into_iter().flatten().map(|c| Coin {
                    coin: c["coin"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), ..Default::default()
                }).filter(|c| c.qty > 0.0).collect();
                let names: Vec<String> = cs.iter().map(|c| c.coin.clone()).collect();
                for chunk in names.chunks(10) {
                    let v = bybit(k, true, "/v5/asset/transfer/query-account-coins-balance", json!({"accountType": "UNIFIED", "coin": chunk.join(",")})).await?;
                    for b in v["balance"].as_array().into_iter().flatten() {
                        if let Some(c) = cs.iter_mut().find(|c| b["coin"] == c.coin.as_str()) { c.free = n(&b["transferBalance"]); }
                    }
                }
                Ok(cs)
            }.await;
            add("UNIFIED", r, &px);
            let r = bybit(k, true, "/v5/asset/transfer/query-account-coins-balance", json!({"accountType": "FUND"})).await.map(|v| {
                v["balance"].as_array().into_iter().flatten().map(|c| Coin {
                    coin: c["coin"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), free: n(&c["transferBalance"]), ..Default::default()
                }).collect()
            });
            add("FUND", r, &px);
            let mut earn: Result<Vec<Coin>> = Ok(vec![]);
            for cat in ["FlexibleSaving", "OnChain"] {
                match bybit(k, true, "/v5/earn/position", json!({"category": cat})).await {
                    Ok(v) => if let Ok(list) = earn.as_mut() {
                        for p in v["list"].as_array().into_iter().flatten() {
                            // on-chain redemptions take days and need a position id: shown, not redeemable here
                            let flex = cat == "FlexibleSaving";
                            let amt = n(&p["amount"]);
                            list.push(Coin { coin: p["coin"].as_str().unwrap_or_default().into(), qty: amt, free: if flex { amt } else { 0.0 },
                                product: flex.then(|| p["productId"].as_str().unwrap_or_default().to_string()), ..Default::default() });
                        }
                    },
                    Err(e) => { earn = Err(e); break; }
                }
            }
            add("EARN", earn, &px);
        }
        Exchange::Binance => {
            let px = binance_usd_prices().await.unwrap_or_default();
            let pm = binance_pm(k).await.unwrap_or(false);
            if pm {
                let r = binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/balance", &[]).await.map(|v| {
                    v.as_array().into_iter().flatten().map(|c| Coin {
                        coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["totalWalletBalance"]), free: n(&c["crossMarginFree"]), ..Default::default()
                    }).collect::<Vec<_>>()
                });
                // what can actually leave PM for the stablecoins is the margin max-withdraw
                let r = match r {
                    Ok(mut cs) => {
                        for c in cs.iter_mut().filter(|c| matches!(c.coin.as_str(), "USDT" | "USDC") && c.qty > 0.0) {
                            if let Ok(v) = binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/margin/maxWithdraw", &[("asset", c.coin.clone())]).await { c.free = n(&v["amount"]); }
                        }
                        Ok(cs)
                    }
                    Err(e) => Err(e),
                };
                add("PM", r, &px);
            } else {
                let r = binance_at(k, binance_url(), reqwest::Method::GET, "/fapi/v3/account", &[]).await.map(|v| {
                    v["assets"].as_array().into_iter().flatten().map(|c| Coin {
                        coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), free: n(&c["maxWithdrawAmount"]), ..Default::default()
                    }).collect()
                });
                add("USDM", r, &px);
            }
            let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v3/asset/getUserAsset", &[]).await.map(|v| {
                v.as_array().into_iter().flatten().map(|c| Coin {
                    coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["free"]) + n(&c["locked"]), free: n(&c["free"]), ..Default::default()
                }).collect()
            });
            add("SPOT", r, &px);
            let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/asset/get-funding-asset", &[]).await.map(|v| {
                v.as_array().into_iter().flatten().map(|c| Coin {
                    coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["free"]) + n(&c["locked"]) + n(&c["freeze"]), free: n(&c["free"]), ..Default::default()
                }).collect()
            });
            add("FUNDING", r, &px);
            let r = binance_at(k, sapi_url(), reqwest::Method::GET, "/sapi/v1/simple-earn/flexible/position", &[("size", "100".into())]).await.map(|v| {
                v["rows"].as_array().into_iter().flatten().map(|c| {
                    let amt = n(&c["totalAmount"]);
                    let can = c["canRedeem"].as_bool().unwrap_or(false);
                    Coin { coin: c["asset"].as_str().unwrap_or_default().into(), qty: amt, free: if can { amt } else { 0.0 },
                        product: can.then(|| c["productId"].as_str().unwrap_or_default().to_string()), ..Default::default() }
                }).collect()
            });
            add("EARN", r, &px);
        }
        _ => notes.push(format!("{ex:?}: wallets not implemented")),
    }
    (out, notes)
}

/// Accounts `from` can send to on this venue (routes `transfer` knows).
pub fn destinations(ex: Exchange, from: &str, pm: bool) -> Vec<&'static str> {
    let margin = if pm { "PM" } else { "USDM" };
    match (ex, from) {
        (Exchange::Bybit, "UNIFIED") => vec!["FUND"],
        (Exchange::Bybit, "FUND") => vec!["UNIFIED"],
        (Exchange::Bybit, "EARN") => vec!["UNIFIED", "FUND"],
        (Exchange::Binance, "SPOT") => vec![margin, "FUNDING"],
        (Exchange::Binance, "FUNDING") => vec![margin, "SPOT"],
        (Exchange::Binance, "PM" | "USDM") => vec!["SPOT", "FUNDING"],
        (Exchange::Binance, "EARN") => vec![margin, "SPOT", "FUNDING"],
        _ => vec![],
    }
}

pub fn is_pm() -> bool { BINANCE_PM.lock().unwrap().unwrap_or(false) }

/// Amount as the venues want it: at most 8 decimals, rounded DOWN so it never exceeds `free`.
fn amount_str(x: f64) -> String {
    let s = format!("{:.8}", (x * 1e8).floor() / 1e8);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn binance_universal(from: &str, to: &str) -> Option<&'static str> {
    Some(match (from, to) {
        ("SPOT", "PM") => "MAIN_PORTFOLIO_MARGIN", ("PM", "SPOT") => "PORTFOLIO_MARGIN_MAIN",
        ("SPOT", "USDM") => "MAIN_UMFUTURE", ("USDM", "SPOT") => "UMFUTURE_MAIN",
        ("SPOT", "FUNDING") => "MAIN_FUNDING", ("FUNDING", "SPOT") => "FUNDING_MAIN",
        ("FUNDING", "USDM") => "FUNDING_UMFUTURE", ("USDM", "FUNDING") => "UMFUTURE_FUNDING",
        _ => return None,
    })
}

/// Move `amount` of `coin` from one account to another on the same venue. Never withdraws.
/// Routes without a direct API go through SPOT (Binance PM <-> FUNDING, EARN -> PM), step by
/// step; the returned lines say what happened, and a failed later step names where the funds are.
pub async fn transfer(ex: Exchange, k: &Keys, from: &str, to: &str, coin: &str, amount: f64, product: Option<&str>) -> Result<Vec<String>> {
    if !(amount > 0.0) { bail!("amount must be positive"); }
    let amt = amount_str(amount);
    let mut log = vec![];
    match ex {
        Exchange::Bybit => {
            if from == "EARN" {
                let pid = product.ok_or_else(|| anyhow!("not redeemable here (on-chain or unknown product)"))?;
                let link = format!("t1-{}", crate::now_ms());
                let v = bybit(k, false, "/v5/earn/place-order", json!({"category": "FlexibleSaving", "orderType": "Redeem", "accountType": to,
                    "amount": amt, "coin": coin, "productId": pid, "orderLinkId": link})).await?;
                log.push(format!("Bybit redeem {amt} {coin} -> {to}: order {}", v["orderId"].as_str().unwrap_or("?")));
            } else {
                let id = uuid_v4();
                let v = bybit(k, false, "/v5/asset/transfer/inter-transfer", json!({"transferId": id, "coin": coin, "amount": amt,
                    "fromAccountType": from, "toAccountType": to})).await?;
                let st = v["status"].as_str().unwrap_or("?");
                if st == "FAILED" { bail!("Bybit transfer {from} -> {to} {amt} {coin}: FAILED"); }
                log.push(format!("Bybit {from} -> {to} {amt} {coin}: {st}"));
            }
        }
        Exchange::Binance => {
            let mut at = from.to_string();
            if from == "EARN" {
                let pid = product.ok_or_else(|| anyhow!("not redeemable now"))?;
                let dest = if to == "FUNDING" { "FUND" } else { "SPOT" };
                binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/simple-earn/flexible/redeem",
                    &[("productId", pid.into()), ("amount", amt.clone()), ("destAccount", dest.into())]).await?;
                at = if dest == "FUND" { "FUNDING".into() } else { "SPOT".into() };
                log.push(format!("Binance redeem {amt} {coin} -> {at}"));
                if at == to { return Ok(log); }
                // the redemption credits asynchronously; give it a moment before moving on
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let hops: Vec<(String, String)> = if binance_universal(&at, to).is_some() { vec![(at.clone(), to.into())] }
                else { vec![(at.clone(), "SPOT".into()), ("SPOT".into(), to.into())] };
            for (a, b) in hops {
                let ty = binance_universal(&a, &b).ok_or_else(|| anyhow!("no transfer route {a} -> {b}"))?;
                let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/asset/transfer",
                    &[("type", ty.into()), ("asset", coin.into()), ("amount", amt.clone())]).await;
                match r {
                    Ok(v) => log.push(format!("Binance {a} -> {b} {amt} {coin}: tran {}", v["tranId"])),
                    Err(e) => bail!("{}{e:#} (funds are in {a})", log.iter().map(|l| format!("{l}; ")).collect::<String>()),
                }
            }
        }
        _ => bail!("{ex:?} transfers not implemented"),
    }
    Ok(log)
}

fn uuid_v4() -> String {
    // random enough for an idempotency id: time + process + counter, formatted as a UUID
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let a = crate::now_ms() as u128;
    let b = (std::process::id() as u128) << 32 | N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
    let x = (a << 64) | b;
    let h = format!("{x:032x}");
    format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

fn sapi_url() -> &'static str { "https://api.binance.com" }

/// Spot last prices in USDT per asset from Binance (weight 4).
async fn binance_usd_prices() -> Result<std::collections::HashMap<String, f64>> {
    let v = ws::get_json(&format!("{}/api/v3/ticker/price", sapi_url())).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(|t| Some((t["symbol"].as_str()?.strip_suffix("USDT")?.to_string(), n(&t["price"])))).collect())
}

fn is_usd(coin: &str) -> bool { matches!(coin, "USDT" | "USDC" | "USD" | "USDE" | "FDUSD" | "DAI" | "PYUSD") }

/// Spot last prices in USDT per coin from Bybit, for valuing non-margin accounts.
async fn bybit_usd_prices() -> Result<std::collections::HashMap<String, f64>> {
    let v = ws::get_json(&format!("{}/v5/market/tickers?category=spot", bybit_url())).await?;
    Ok(v["result"]["list"].as_array().into_iter().flatten().filter_map(|t| {
        let coin = t["symbol"].as_str()?.strip_suffix("USDT")?;
        Some((coin.to_string(), n(&t["lastPrice"])))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_amounts_routes_and_ids() {
        // never round up past the free balance
        assert_eq!(amount_str(1.234567899), "1.23456789");
        assert_eq!(amount_str(100.0), "100");
        assert_eq!(amount_str(0.1 + 0.2), "0.3");
        // Binance PM has no direct FUNDING route: transfer() goes through SPOT
        assert_eq!(binance_universal("FUNDING", "PM"), None);
        assert_eq!(binance_universal("SPOT", "PM"), Some("MAIN_PORTFOLIO_MARGIN"));
        assert_eq!(binance_universal("PM", "SPOT"), Some("PORTFOLIO_MARGIN_MAIN"));
        assert!(destinations(Exchange::Binance, "FUNDING", true).contains(&"PM"));
        assert_eq!(destinations(Exchange::Bybit, "FUND", false), vec!["UNIFIED"]);
        let (a, b) = (uuid_v4(), uuid_v4());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.split('-').map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn binance_signature_matches_docs() {
        // example from Binance's "SIGNED endpoint" documentation
        let s = hmac_hex("NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j",
            "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559");
        assert_eq!(s, "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71");
    }

    #[test]
    fn bybit_signature_layout() {
        // reference computed independently with Python's hmac module
        let k = Keys { key: "XXXXKEY".into(), secret: "YYYYSECRET".into(), extra: None };
        assert_eq!(bybit_sign(&k, 1_700_000_000_000, 5000, r#"{"category":"linear","symbol":"BTCUSDT"}"#),
                   "136ba24b5260b0829ea5217d7be22cff536e12d72edc8f9e72238c026fbaee4f");
    }

    #[test]
    fn rounding_and_minimums() {
        assert_eq!(fmt_step(0.123456, 0.001, true), "0.123");
        assert_eq!(fmt_step(86123.46, 0.1, false), "86123.5");
        assert_eq!(fmt_step(0.30000000000000004, 0.1, true), "0.3");
        assert_eq!(fmt_step(12.0, 1.0, true), "12");
        let r = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 5.0 };
        let req = |qty: f64| OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind: Kind::Limit { price: 86000.04, tif: Tif::Gtc }, qty, client_id: None };
        assert_eq!(checked(&req(0.0019), &r, 86000.0).unwrap(), ("0.001".into(), Some("86000.0".into())));
        assert!(checked(&req(0.0004), &r, 86000.0).is_err());
        let r2 = Rules { min_notional: 100.0, ..r };
        assert!(checked(&req(0.001), &r2, 86000.0).is_err());
    }

    #[test]
    fn closes_rebuilt_from_fills() {
        let f = |side, price, qty, r: f64, ts| Fill { ex: Exchange::Binance, symbol: "XUSDT".into(), side, price, qty, fee: 0.0, realized: Some(r), ts };
        // a long of 3 bought at 100 closed in two fills at 110 / 112 in the same second
        let c = closes_from_fills(&[f(Side::Sell, 110.0, 1.0, 10.0, 5_000), f(Side::Sell, 112.0, 2.0, 24.0, 5_400), f(Side::Buy, 99.0, 1.0, 0.0, 6_000)]);
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!((c.long, c.qty, c.pnl), (Some(true), Some(3.0), 34.0));
        assert!((c.exit.unwrap() - 334.0 / 3.0).abs() < 1e-9);
        assert!((c.entry.unwrap() - 100.0).abs() < 1e-9);
        let m = merge_same_second(vec![
            ClosedPnl { ex: Exchange::Binance, symbol: "A".into(), long: None, qty: None, entry: None, exit: None, pnl: 1.0, ts: 1_100 },
            ClosedPnl { ex: Exchange::Binance, symbol: "A".into(), long: None, qty: None, entry: None, exit: None, pnl: 2.0, ts: 1_900 },
            ClosedPnl { ex: Exchange::Binance, symbol: "A".into(), long: None, qty: None, entry: None, exit: None, pnl: 4.0, ts: 2_100 }]);
        assert_eq!(m.iter().map(|c| c.pnl).collect::<Vec<_>>(), vec![3.0, 4.0]);
    }

    #[test]
    fn bbo_params() {
        let r = |queue, level| OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind: Kind::Bbo { queue, level }, qty: 1.0, client_id: None };
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let b = binance_params(&r(true, 1), Mode::Hedge, "1", None);
        assert_eq!(get(&b, "priceMatch").as_deref(), Some("QUEUE"));
        assert_eq!(get(&b, "price"), None, "priceMatch cannot be sent with a price");
        assert_eq!(get(&binance_params(&r(false, 5), Mode::Hedge, "1", None), "priceMatch").as_deref(), Some("OPPONENT_5"));
        let y = bybit_body(&r(false, 3), Mode::OneWay, "1", None, 84_000.0, 0.1);
        assert_eq!((y["bboSideType"].as_str(), y["bboLevel"].as_str(), y.get("price")), (Some("Counterparty"), Some("3"), None));
        // BBO orders have no own price: the minimum-notional check falls back to the reference price
        let rules = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 5.0 };
        assert!(checked(&OrderReq { qty: 0.001, ..r(true, 1) }, &rules, 84_000.0).is_ok());
    }

    #[test]
    fn binance_twap_params_by_mode() {
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let o = |pos: Side, close: bool| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind: Kind::Market, qty: 1.0, client_id: None };
        let h = binance_twap_params(&o(Side::Buy, true), Mode::Hedge, "0.5", 15, Some("90000"), "t1twap1");
        assert_eq!((get(&h, "side").as_deref(), get(&h, "positionSide").as_deref(), get(&h, "reduceOnly"), get(&h, "duration").as_deref(), get(&h, "limitPrice").as_deref()),
                   (Some("SELL"), Some("LONG"), None, Some("900"), Some("90000")));
        let w = binance_twap_params(&o(Side::Sell, true), Mode::OneWay, "0.5", 5, None, "t1twap2");
        assert_eq!((get(&w, "side").as_deref(), get(&w, "reduceOnly").as_deref(), get(&w, "positionSide")), (Some("BUY"), Some("true"), None));
        assert_eq!(native_twap(Exchange::Bybit), None);
        assert_eq!(native_twap(Exchange::Binance), Some((5, 1440)));
    }

    #[test]
    fn conditional_trailing_and_fok_params() {
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let o = |pos: Side, close: bool, kind: Kind| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty: 1.0, client_id: None };
        // a buy that triggers above the price is a stop, below it a take-profit; the reverse for a sell
        let stop = |t: f64, l: Option<f64>| Kind::Stop { trigger: t, limit: l, by_mark: true };
        let ty = |r: &OrderReq| get(&binance_algo_params(r, Mode::OneWay, "1", None, 100.0, 0.1).unwrap(), "type").unwrap();
        assert_eq!(ty(&o(Side::Buy, false, stop(110.0, None))), "STOP_MARKET");
        assert_eq!(ty(&o(Side::Buy, false, stop(90.0, None))), "TAKE_PROFIT_MARKET");
        assert_eq!(ty(&o(Side::Buy, true, stop(110.0, None))), "TAKE_PROFIT_MARKET", "closing a long sells: above = take-profit");
        assert_eq!(ty(&o(Side::Buy, true, stop(90.0, Some(89.0)))), "STOP");
        let a = binance_algo_params(&o(Side::Buy, false, stop(110.04, Some(111.0))), Mode::Hedge, "1", Some("111.0"), 100.0, 0.1).unwrap();
        assert_eq!((get(&a, "triggerPrice").as_deref(), get(&a, "price").as_deref(), get(&a, "workingType").as_deref(), get(&a, "positionSide").as_deref()),
                   (Some("110.0"), Some("111.0"), Some("MARK_PRICE"), Some("LONG")));
        let tr = binance_algo_params(&o(Side::Sell, true, Kind::Trailing { callback_pct: 1.25, activation: Some(95.0) }), Mode::OneWay, "1", None, 100.0, 0.1).unwrap();
        assert_eq!((get(&tr, "type").as_deref(), get(&tr, "callbackRate").as_deref(), get(&tr, "reduceOnly").as_deref()), (Some("TRAILING_STOP_MARKET"), Some("1.2"), Some("true")));
        // Bybit: direction from the trigger's side, trigger by mark or last
        let b = bybit_body(&o(Side::Sell, false, Kind::Stop { trigger: 95.0, limit: None, by_mark: false }), Mode::OneWay, "1", None, 100.0, 0.1);
        assert_eq!((b["orderType"].as_str(), b["triggerDirection"].as_i64(), b["triggerBy"].as_str(), b["triggerPrice"].as_str()), (Some("Market"), Some(2), Some("LastPrice"), Some("95.0")));
        // Bybit trailing: closing only, distance in price
        let t = bybit_trailing_body(&o(Side::Buy, true, Kind::Trailing { callback_pct: 1.0, activation: None }), Mode::Hedge, 100.0, 0.1).unwrap();
        assert_eq!((t["trailingStop"].as_str(), t["positionIdx"].as_i64()), (Some("1.0"), Some(1)));
        assert!(bybit_trailing_body(&o(Side::Buy, false, Kind::Trailing { callback_pct: 1.0, activation: None }), Mode::Hedge, 100.0, 0.1).is_err());
        // FOK maps on both
        let f = o(Side::Buy, false, Kind::Limit { price: 100.0, tif: Tif::Fok });
        assert_eq!(get(&binance_params(&f, Mode::OneWay, "1", Some("100")), "timeInForce").as_deref(), Some("FOK"));
        assert_eq!(bybit_body(&f, Mode::OneWay, "1", Some("100"), 100.0, 0.1)["timeInForce"].as_str(), Some("FOK"));
        // validation
        let rules = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 5.0 };
        assert!(checked(&o(Side::Buy, false, Kind::Trailing { callback_pct: 12.0, activation: None }), &rules, 100.0).is_err());
        assert!(checked(&o(Side::Buy, false, stop(0.0, None)), &rules, 100.0).is_err());
    }

    #[test]
    fn hedge_and_one_way_params() {
        let o = |pos: Side, close: bool| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind: Kind::Market, qty: 1.0, client_id: None };
        // (pos, close) -> order side
        for (pos, close, side) in [(Side::Buy, false, "Buy"), (Side::Buy, true, "Sell"), (Side::Sell, false, "Sell"), (Side::Sell, true, "Buy")] {
            let r = o(pos, close);
            let h = bybit_body(&r, Mode::Hedge, "1", None, 84_000.0, 0.1);
            assert_eq!((h["side"].as_str(), h["positionIdx"].as_i64(), h["reduceOnly"].as_bool()), (Some(side), Some(if pos == Side::Buy { 1 } else { 2 }), Some(close)));
            let w = bybit_body(&r, Mode::OneWay, "1", None, 84_000.0, 0.1);
            assert_eq!((w["side"].as_str(), w["positionIdx"].as_i64(), w["reduceOnly"].as_bool()), (Some(side), Some(0), Some(close)));
            let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
            let bh = binance_params(&r, Mode::Hedge, "1", None);
            assert_eq!(get(&bh, "side").as_deref(), Some(side.to_uppercase().as_str()));
            assert_eq!(get(&bh, "positionSide").as_deref(), Some(if pos == Side::Buy { "LONG" } else { "SHORT" }));
            assert_eq!(get(&bh, "reduceOnly"), None, "binance rejects reduceOnly in hedge mode");
            let bw = binance_params(&r, Mode::OneWay, "1", None);
            assert_eq!(get(&bw, "positionSide"), None);
            assert_eq!(get(&bw, "reduceOnly").is_some(), close);
        }
    }
}
