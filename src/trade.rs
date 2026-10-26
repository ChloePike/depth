//! Private trading APIs: shared types and helpers, and the venue-neutral functions that dispatch to
//! one module per venue (`trade/<venue>.rs`: Bybit v5 unified linear USDT perps, Binance USD-M
//! futures, OKX, Bitget, Gate, Kraken Futures, Hyperliquid, Lighter), all behind the same functions.
//! Public API always uses unified symbols ("BTCUSDT") and base-unit
//! sizes; each venue module maps to its native symbol / contracts.
//! Keys live in the macOS Keychain (service "depth", accounts "<venue>-key" /
//! "<venue>-secret"). `T1_TESTNET=1` switches the venues that have one to their testnet / demo hosts.
//!
//! Prices and sizes are always sent as strings rounded to the instrument's tick / step, never as
//! raw f64 formatting.
use crate::{ws, Exchange, Side};
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

pub mod binance;
pub mod bitget;
pub mod bybit;
pub mod gate;
pub mod hyperliquid;
pub mod kraken;
pub mod lighter;
pub mod okx;
pub mod spot;
pub mod tpsl;
pub use binance::{is_pm, pm_account};
use binance::{binance_at, binance_pm, binance_url, papi_url};
use bybit::{bybit, bybit_url};

pub const TRADABLE: [Exchange; 8] = KEY_VENUES;

/// Venues whose trading has run against a real account. The others are implemented and
/// unit-tested only: the order confirmation warns until they are verified.
pub const VERIFIED: [Exchange; 2] = [Exchange::Bybit, Exchange::Binance];

/// API credentials from the Keychain. `extra` is the third secret some venues need:
/// OKX / Bitget passphrase; Hyperliquid: key = account (main wallet) address, secret = API
/// (agent) wallet private key; Lighter: key = "<account_index>:<api_key_index>", secret = API key.
#[derive(Clone)]
pub struct Keys { pub key: String, pub secret: String, pub extra: Option<String>,
    /// simulated venue account: never reaches a real exchange (default false; Debug still redacts everything)
    pub demo: bool }

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

/// Read keys from the OS secret store (macOS Keychain, Windows Credential Manager, libsecret).
pub fn keychain(ex: Exchange) -> Option<Keys> {
    let get = |what: &str| crate::sys::secret_get(&kc_account(ex, what));
    let extra = get("passphrase");
    if needs_extra(ex) && extra.is_none() { return None; }
    Some(Keys { key: get("key")?, secret: get("secret")?, extra, demo: false })
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

fn kc_set(account: &str, value: &str) -> Result<()> { crate::sys::secret_set(account, value) }
fn kc_delete(account: &str) { crate::sys::secret_delete(account) }

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
    let mut s = format!("security add-generic-password -U -s depth -a {v}-key{suffix} -w\nsecurity add-generic-password -U -s depth -a {v}-secret{suffix} -w", v = venue(ex));
    if needs_extra(ex) { s += &format!("\nsecurity add-generic-password -U -s depth -a {}-passphrase{suffix} -w", venue(ex)); }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tif { Gtc, Ioc, PostOnly, Fok }

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
pub struct Caps { pub fok: bool, pub stop: bool, pub trailing: bool, pub post_only: bool,
    /// position TP/SL (`tpsl` module)
    pub tpsl: bool }

pub fn caps(ex: Exchange) -> Caps {
    match ex {
        Exchange::Bybit | Exchange::Binance => Caps { fok: true, stop: true, trailing: true, post_only: true, tpsl: true },
        Exchange::Okx | Exchange::Bitget | Exchange::Gate => Caps { fok: true, stop: false, trailing: false, post_only: true, tpsl: false },
        Exchange::Kraken | Exchange::Hyperliquid | Exchange::Lighter => Caps { fok: false, stop: false, trailing: false, post_only: true, tpsl: false },
        _ => Caps::default(),
    }
}

/// BBO levels each venue accepts (Binance priceMatch QUEUE/OPPONENT[_5|_10|_20], Bybit bboLevel 1-5).
pub fn bbo_levels(ex: Exchange) -> &'static [u8] {
    match ex { Exchange::Binance => &[1, 5, 10, 20], Exchange::Bybit => &[1, 2, 3, 4, 5], _ => &[] }
}

/// An order expressed as intent: which position (`pos`: Buy = long, Sell = short) and whether it
/// opens / adds to it or closes / reduces it. Venue params are derived per position mode.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
pub struct OpenOrder { pub ex: Exchange, pub symbol: String, pub id: String, pub side: Side, pub price: f64, pub qty: f64, pub filled: f64, pub kind: String,
    /// time in force as the venue spells it (GTC, IOC, PostOnly, alo, poc, ...); empty when the venue puts it in `kind`
    pub tif: String, pub reduce_only: bool, pub ts: i64,
    /// hedge mode: which position the order belongs to
    pub pos: Option<Side>,
    /// our id for the order, where the venue row carries it (Bybit orderLinkId, Binance clientOrderId); None elsewhere
    pub client_id: Option<String>,
    /// trigger price when the row is a conditional order waiting on the venue; None for plain resting orders
    pub trigger: Option<f64> }

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

/// Receive windows: reads tolerate a slow proxy hop; orders and cancels keep the venue
/// default so an order stuck in transit for seconds is rejected rather than filled late.
const RECV_READ: u32 = 15_000;
const RECV_WRITE: u32 = 5_000;

// ---------------------------------------------------------------- venue-neutral API

fn n(v: &Value) -> f64 { crate::num(v) }

/// A venue's client order id field: None when absent or empty.
fn opt_id(v: &Value) -> Option<String> { v.as_str().filter(|s| !s.is_empty()).map(String::from) }

/// Rows of a paged REST listing. `fetch(cursor)` returns one page and the cursor of the next one
/// (None on the last page; the first call gets None). Stops after `max` pages or when a cursor
/// repeats; rows seen on an earlier page (the list moved while paging) are dropped by `key`.
/// Snapshot paths only (resync, reconcile): paging is never a polling loop.
pub(super) async fn paged<F, Fut>(max: usize, key: impl Fn(&Value) -> String, mut fetch: F) -> Result<Vec<Value>>
where F: FnMut(Option<String>) -> Fut, Fut: std::future::Future<Output = Result<(Vec<Value>, Option<String>)>> {
    let (mut out, mut seen, mut cursor) = (vec![], std::collections::HashSet::new(), None);
    for _ in 0..max {
        let (rows, next) = fetch(cursor.clone()).await?;
        out.extend(rows.into_iter().filter(|r| seen.insert(key(r))));
        match next { Some(c) if cursor.as_ref() != Some(&c) => cursor = Some(c), _ => break }
    }
    Ok(out)
}

pub async fn rules(ex: Exchange, symbol: &str) -> Result<Rules> {
    // every order starts here: a market this venue lists under another name (crate::alias, e.g.
    // XAU = Hyperliquid xyz:GOLD) or whose same-name market is another asset is refused, never
    // sent to the wrong instrument
    // ponytail: alias-aware order entry (HIP-3 asset ids, positions mapped back) not built yet
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    match crate::alias::venue_base(ex, crate::Market::Perp, base) {
        Some(b) if b.eq_ignore_ascii_case(base) => {}
        Some(b) => anyhow::bail!("{base} trades as {b} on {ex:?}; order entry for renamed markets is not supported yet"),
        None => anyhow::bail!("{ex:?}'s {base} is a different market from the one charted; not tradable here"),
    }
    match ex {
        Exchange::Bybit => bybit::rules(symbol).await,
        Exchange::Binance => binance::rules(symbol).await,
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

/// Start a venue-run TWAP; returns the id to cancel it with. Binance: notional at least 1,000
/// USDT, weight 3000 (UID), so it is placed once and never polled.
pub async fn place_twap(ex: Exchange, k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode, minutes: u32, limit: Option<f64>, randomize: bool) -> Result<String> {
    let Some((lo, hi)) = native_twap(ex) else { bail!("{ex:?} has no native TWAP") };
    if !(lo..=hi).contains(&minutes) { bail!("{ex:?}: TWAP duration must be {lo} to {hi} minutes"); }
    match ex {
        Exchange::Binance => binance::place_twap(k, req, r, ref_px, mode, minutes, limit).await,
        Exchange::Hyperliquid => hyperliquid::place_twap(k, req, minutes, randomize).await,
        _ => unreachable!(),
    }
}

pub async fn cancel_twap(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Binance => binance::cancel_twap(k, symbol, id).await,
        Exchange::Hyperliquid => hyperliquid::cancel_twap(k, symbol, id).await,
        _ => bail!("{ex:?} has no native TWAP"),
    }
}

/// Position mode of the account for `symbol` (read only; never changed from here).
pub async fn mode(ex: Exchange, k: &Keys, symbol: &str) -> Result<Mode> {
    match ex {
        Exchange::Bybit => bybit::mode(k, symbol).await,
        Exchange::Binance => binance::mode(k, symbol).await,
        Exchange::Okx => okx::mode(k, symbol).await,
        Exchange::Bitget => bitget::mode(k, symbol).await,
        Exchange::Gate => gate::mode(k, symbol).await,
        Exchange::Kraken => kraken::mode(k, symbol).await,
        Exchange::Hyperliquid => hyperliquid::mode(k, symbol).await,
        Exchange::Lighter => lighter::mode(k, symbol).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

/// Highest leverage the venue allows for `symbol` at the smallest position size.
/// Bybit: public instrument info. Binance: leverage brackets (signed, weight 1).
pub async fn max_leverage(ex: Exchange, k: &Keys, symbol: &str) -> Result<f64> {
    match ex {
        Exchange::Bybit => bybit::max_leverage(symbol).await,
        Exchange::Binance => binance::max_leverage(k, symbol).await,
        _ => bail!("{ex:?}: changing leverage from Depth is not supported yet"),
    }
}

/// Change the account's leverage for `symbol` (both sides on Bybit). This changes an exchange
/// setting, so it is only called after the user confirms.
pub async fn set_leverage(ex: Exchange, k: &Keys, symbol: &str, lev: u32) -> Result<()> {
    if lev == 0 { bail!("leverage must be at least 1x"); }
    match ex {
        Exchange::Bybit => bybit::set_leverage(k, symbol, lev).await,
        Exchange::Binance => binance::set_leverage(k, symbol, lev).await,
        _ => bail!("{ex:?}: changing leverage from Depth is not supported yet"),
    }
}

/// The account's leverage setting for `symbol` (weight 5 on Binance; read once per symbol).
pub async fn leverage(ex: Exchange, k: &Keys, symbol: &str) -> Result<f64> {
    match ex {
        Exchange::Bybit => bybit::leverage(k, symbol).await,
        Exchange::Binance => binance::leverage(k, symbol).await,
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
            bybit::place_trailing(k, req, r, ref_px, mode).await
        }
        Exchange::Bybit => bybit::place(k, req, r, ref_px, mode, &qty, px.as_deref()).await,
        Exchange::Binance if matches!(req.kind, Kind::Stop { .. } | Kind::Trailing { .. }) =>
            binance::place_algo(k, req, r, ref_px, mode, &qty, px.as_deref()).await,
        Exchange::Binance => binance::place(k, req, r, ref_px, mode, &qty, px.as_deref()).await,
        Exchange::Okx => okx::place(k, req, r, ref_px, mode).await,
        Exchange::Bitget => bitget::place(k, req, r, ref_px, mode).await,
        Exchange::Gate => gate::place(k, req, r, ref_px, mode).await,
        Exchange::Kraken => kraken::place(k, req, r, ref_px, mode).await,
        Exchange::Hyperliquid => hyperliquid::place(k, req, r, ref_px, mode).await,
        Exchange::Lighter => lighter::place(k, req, r, ref_px, mode).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

// ---------------------------------------------------------------- options (position insurance)

/// Venues where protection options can be bought. ponytail: Bybit only; Binance / OKX / Gate
/// option order entry goes here behind the same two functions.
pub const OPTION_VENUES: &[Exchange] = &[Exchange::Bybit];

/// A protection buy priced off the venue's real ask side: one IOC limit at the worst level the
/// size needs, so it fills at once or not at all past that price.
#[derive(Clone, Debug)]
pub struct OptBuy { pub ex: Exchange, pub symbol: String, pub qty: f64, pub limit: f64, pub tick: f64, pub step: f64,
    /// size the book holds up to `limit` (< qty when the asks run out), its average price and cost (venue quote)
    pub fill: f64, pub avg: f64, pub cost: f64 }

/// Walk `asks` (best first) for `qty`: (limit = last level touched, filled qty, average price).
pub fn sweep(asks: &[(f64, f64)], qty: f64) -> Option<(f64, f64, f64)> {
    let (mut got, mut paid, mut limit) = (0.0, 0.0, None);
    for &(px, q) in asks {
        if got >= qty - 1e-12 { break; }
        let take = q.min(qty - got);
        got += take;
        paid += take * px;
        limit = Some(px);
    }
    limit.map(|l| (l, got, paid / got))
}

/// Size rounded UP to the venue step (a hedge rounded down leaves part of the position bare).
pub fn ceil_step(qty: f64, step: f64, min: f64) -> f64 { ((qty / step - 1e-9).ceil() * step).max(min) }

/// Tick, size step and minimum size of one option.
pub async fn option_rules(ex: Exchange, symbol: &str) -> Result<Rules> {
    match ex {
        Exchange::Bybit => bybit::option_rules(symbol).await,
        _ => bail!("{ex:?}: option orders not supported yet"),
    }
}

pub async fn option_plan(ex: Exchange, symbol: &str, qty: f64) -> Result<OptBuy> {
    match ex {
        Exchange::Bybit => bybit::option_plan(symbol, qty).await,
        _ => bail!("{ex:?}: option orders not supported yet"),
    }
}

/// A held option (bought protection). Symbol parts are parsed from the venue symbol
/// (Bybit: BASE-DDMMMYY-STRIKE-P|C[-USDT]); prices in the venue quote.
#[derive(Clone, Debug, PartialEq)]
pub struct OptPos { pub ex: Exchange, pub symbol: String, pub base: String, pub expiry: String, pub strike: f64, pub call: bool,
    pub side: Side, pub qty: f64, pub avg: f64, pub mark: f64, pub upnl: f64 }

/// Open option positions on a venue (empty where option trading is not built).
pub async fn option_positions(ex: Exchange, k: &Keys) -> Result<Vec<OptPos>> {
    match ex {
        Exchange::Bybit => bybit::option_positions(k).await,
        _ => Ok(vec![]),
    }
}

/// Buy the planned options: limit IOC at `p.limit`, so nothing rests and nothing fills above it.
pub async fn buy_option(k: &Keys, p: &OptBuy) -> Result<String> {
    let r = Rules { tick: p.tick, step: p.step, min_qty: 0.0, min_notional: 0.0 };
    option_order(k, p.ex, &p.symbol, Side::Buy, p.qty, p.limit, Tif::Ioc, false, &r).await
}

/// One option limit order; size floored and price rounded to the venue grid.
#[allow(clippy::too_many_arguments)]
pub async fn option_order(k: &Keys, ex: Exchange, symbol: &str, side: Side, qty: f64, price: f64, tif: Tif, reduce_only: bool, r: &Rules) -> Result<String> {
    let q = fmt_step(qty, r.step, true);
    if q.parse::<f64>().unwrap_or(0.0) < r.min_qty.max(1e-12) { bail!("size {qty} below minimum {}", r.min_qty); }
    let px = fmt_step(price, r.tick, false);
    if px.parse::<f64>().unwrap_or(0.0) <= 0.0 { bail!("invalid price {price}"); }
    match ex {
        Exchange::Bybit => bybit::option_order(k, symbol, side, &q, &px, tif, reduce_only).await,
        _ => bail!("{ex:?}: option orders not supported yet"),
    }
}

pub async fn option_cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Bybit => bybit::option_cancel(k, symbol, id).await,
        _ => bail!("{ex:?}: option orders not supported yet"),
    }
}

/// Resting option orders (empty where option trading is not built).
pub async fn option_orders(ex: Exchange, k: &Keys) -> Result<Vec<OpenOrder>> {
    match ex {
        Exchange::Bybit => bybit::option_orders(k).await,
        _ => Ok(vec![]),
    }
}

/// Whether the key may trade USDT perps: Some(false) for a read-only key, None when the venue
/// has no way to ask (Gate, Kraken, Hyperliquid, Lighter, Bitget) or the answer is unclear.
/// Read once per key (stream start, key change), never polled.
pub async fn can_trade(ex: Exchange, k: &Keys) -> Result<Option<bool>> {
    Ok(match ex {
        Exchange::Bybit => bybit::can_trade(k).await?,
        Exchange::Binance => binance::can_trade(k).await?,
        Exchange::Okx => okx::can_trade(k).await?,
        _ => None,
    })
}

/// Venues whose resting limit orders `amend` changes in place (same id); the others are moved by
/// cancel + new.
pub fn can_amend(ex: Exchange) -> bool { matches!(ex, Exchange::Bybit | Exchange::Binance) }

/// Re-price a resting limit order in place, keeping its size left and its id. Binance's modify
/// needs side and quantity again (it rejects a quantity below what already filled).
pub async fn amend(ex: Exchange, k: &Keys, o: &OpenOrder, price: f64, r: &Rules) -> Result<()> {
    let px = fmt_step(price, r.tick, false);
    if !(px.parse::<f64>().is_ok_and(|p| p > 0.0)) { bail!("invalid price {price}"); }
    match ex {
        Exchange::Bybit => { bybit::amend(k, o, px).await?; }
        Exchange::Binance => { binance::amend(k, o, px, r).await?; }
        _ => bail!("{ex:?}: no in-place amend"),
    }
    Ok(())
}

/// Cancels one resting order row. ponytail: plain cancel by id on every venue; trigger rows
/// (`o.trigger`) get their venue's algo cancel here once those rows are listed.
pub async fn cancel_order(ex: Exchange, k: &Keys, o: &OpenOrder) -> Result<()> { cancel(ex, k, &o.symbol, &o.id).await }

/// Outcome of a bulk cancel: how many cancels the venue accepted, and one line per refusal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CancelReport { pub sent: usize, pub failed: Vec<String> }

impl CancelReport {
    pub(crate) fn add(&mut self, what: String, r: Result<()>) {
        match r { Ok(()) => self.sent += 1, Err(e) => self.failed.push(format!("{what}: {e:#}")) }
    }
}

/// Cancels every given order, one request after another (the rate-limit pause applies to each).
/// ponytail: a loop of single cancels; native cancel-all endpoints come with the kill switch.
pub async fn cancel_all(ex: Exchange, k: &Keys, orders: &[OpenOrder]) -> Result<CancelReport> {
    let mut rep = CancelReport::default();
    for o in orders { rep.add(format!("{} {}", o.symbol, o.id), cancel_order(ex, k, o).await); }
    Ok(rep)
}

pub async fn cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Bybit => { bybit::cancel(k, symbol, id).await?; }
        Exchange::Binance => { binance::cancel(k, symbol, id).await?; }
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
        Exchange::Bybit => bybit::positions(k).await,
        Exchange::Binance => binance::positions(k).await,
        Exchange::Okx => okx::positions(k).await,
        Exchange::Bitget => bitget::positions(k).await,
        Exchange::Gate => gate::positions(k).await,
        Exchange::Kraken => kraken::positions(k).await,
        Exchange::Hyperliquid => hyperliquid::positions(k).await,
        Exchange::Lighter => lighter::positions(k).await,
        _ => bail!("{ex:?} trading not implemented"),
    }
}

/// A REST position snapshot merged into the rows pushed so far (`old`, one venue): fields a
/// snapshot leaves out keep their pushed value (Kraken REST rows carry no mark / liq / margin;
/// a venue may omit liq for a moment). Liq and margin belonged to the old size, so they are only
/// kept while qty and entry are unchanged. Positions missing from the snapshot are dropped.
pub fn merge_snapshot(old: &[Position], new: Vec<Position>) -> Vec<Position> {
    new.into_iter().map(|mut p| {
        if let Some(o) = old.iter().find(|o| o.ex == p.ex && o.symbol == p.symbol && o.side == p.side) {
            let same = o.qty == p.qty && o.entry == p.entry;
            if p.mark == 0.0 { p.mark = o.mark; }
            if p.liq.is_none() && same { p.liq = o.liq; }
            if p.lev == 0.0 { p.lev = o.lev; }
            if p.margin == 0.0 && same { p.margin = o.margin; }
            if p.cross.is_none() { p.cross = o.cross; }
        }
        p
    }).collect()
}

pub async fn open_orders(ex: Exchange, k: &Keys) -> Result<Vec<OpenOrder>> {
    match ex {
        Exchange::Bybit => bybit::open_orders(k).await,
        Exchange::Binance => binance::open_orders(k).await,
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
        Exchange::Bybit => bybit::balance(k).await,
        Exchange::Binance => binance::balance(k).await,
        Exchange::Okx => okx::balance(k).await,
        Exchange::Bitget => bitget::balance(k).await,
        Exchange::Gate => gate::balance(k).await,
        Exchange::Kraken => kraken::balance(k).await,
        Exchange::Hyperliquid => hyperliquid::balance(k).await,
        Exchange::Lighter => lighter::balance(k).await,
        _ => bail!("{ex:?} trading not implemented"),
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
    /// margin account totals pushed by the venue; `equity` / `available` always replace, the Option
    /// fields keep the last value when None (a push that does not carry them)
    Wallet(Balance),
    /// the private stream is down (emitted by streams later; the engine only logs it for now)
    Down,
    /// one of our positions was liquidated by the venue (emitted later; the engine only logs it for now)
    Liquidated { symbol: String, side: Side, qty: f64, price: f64 },
    /// balances changed but the push has no account totals (Binance): refetch the balance
    BalanceDirty,
    /// a TP/SL (whole-position or partial) appeared or changed; removed with TpSlDone
    TpSl(tpsl::TpSl),
    TpSlDone { id: String },
    /// one of our orders traded (stored in the db, drawn on the chart)
    Fill(Fill),
    /// option position upsert by symbol; qty 0 removes
    OptPos(OptPos),
    /// a spot order changed (placed, filled, cancelled): refetch spot orders and balances
    SpotDirty,
}

pub type AccTx = tokio::sync::mpsc::UnboundedSender<(Exchange, AccEvent)>;

/// Runs a venue's private account stream forever (reconnecting, re-snapshotting).
pub async fn stream(ex: Exchange, k: Keys, tx: AccTx) {
    match ex {
        Exchange::Bybit => bybit::stream(k, tx).await,
        Exchange::Binance => binance::stream(k, tx).await,
        Exchange::Okx => okx::stream(k, tx).await,
        Exchange::Bitget => bitget::stream(k, tx).await,
        Exchange::Gate => gate::stream(k, tx).await,
        Exchange::Kraken => kraken::stream(k, tx).await,
        Exchange::Hyperliquid => hyperliquid::stream(k, tx).await,
        Exchange::Lighter => lighter::stream(k, tx).await,
        _ => {}
    }
}

// ---------------------------------------------------------------- history (on demand only)

/// Own fills of one symbol since `since` (ms), for the chart's buy / sell marks. Called once per
/// symbol per session; pushes keep it current after that. Both venues cap a query at 7 days, so
/// the span is walked in week windows, each paged. Bybit / Binance only (others: empty).
/// ponytail: at most the last 30 days (what the chart shows); older gaps stay empty
pub async fn fills_since(ex: Exchange, k: &Keys, symbol: &str, since: i64) -> Result<Vec<Fill>> {
    if !matches!(ex, Exchange::Bybit | Exchange::Binance) { return Ok(vec![]); }
    let now = crate::now_ms();
    let binance = if ex == Exchange::Binance {
        Some(if binance_pm(k).await? { (papi_url(), "/papi/v1/um/userTrades") } else { (binance_url(), "/fapi/v1/userTrades") })
    } else { None };
    let mut out = vec![];
    let mut t0 = since.max(now - 30 * 86_400_000);
    while t0 < now {
        let t1 = (t0 + 7 * 86_400_000 - 1).min(now);
        if let Some((base, path)) = binance {
            binance::fills_window(k, symbol, base, path, t0, t1, &mut out).await?;
        } else {
            bybit::fills_window(k, symbol, t0, t1, &mut out).await?;
        }
        t0 = t1 + 1;
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Fill { pub ex: Exchange, pub id: String, pub symbol: String, pub side: Side, pub price: f64, pub qty: f64, pub fee: f64, pub realized: Option<f64>, pub ts: i64 }

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
        Exchange::Bybit => bybit::history(k, side, &mut h).await?,
        Exchange::Binance => binance::history(k, symbols, side, &mut h).await?,
        _ => bail!("{ex:?} history not implemented"),
    }
    crate::db::fills_put(&h.fills);
    h.orders.sort_by(|a, b| b.ts.cmp(&a.ts));
    h.fills.sort_by(|a, b| b.ts.cmp(&a.ts));
    h.closed.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(h)
}

/// Funding a position has paid or received since it opened, in its quote.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FundingPaid {
    /// received positive, paid negative
    pub amount: f64,
    /// open time (ms); 0 when the venue does not say
    pub since: i64,
    /// the fills ran out before the open was found: `since` is the oldest fill seen, the position is older
    pub lower: bool,
}

/// When the current position opened: undo fills (newest first, signed base qty: + buys, - sells)
/// from the current signed size until it reaches zero or flips sign; that fill opened it.
/// (time, false) when found, (oldest fill time, true) as a lower bound when the fills run out.
pub fn open_time(cur: f64, fills: &[(i64, f64)]) -> Option<(i64, bool)> {
    let eps = cur.abs() * 1e-9;
    let mut s = cur;
    for &(t, q) in fills {
        s -= q;
        if s.abs() <= eps || s.signum() != cur.signum() { return Some((t, false)); }
    }
    fills.last().map(|f| (f.0, true))
}

const WEEK_MS: i64 = 7 * 86_400_000;

/// Funding paid / received by an open position since it opened. Never on a timer: the engine
/// fetches once per position, after a size change (debounced) and after each settlement.
/// Binance: userTrades 5 + income 30 (per 1000 rows); Bybit: execution/list and
/// transaction-log, a few pages per 7-day window. Ok(None) on venues that do not report it.
/// `known`: the open time found by an earlier call for this same position (the fill walk is skipped).
pub async fn funding_paid(ex: Exchange, k: &Keys, p: &Position, known: Option<i64>) -> Result<Option<FundingPaid>> {
    let cur = if p.side == Side::Buy { p.qty } else { -p.qty };
    let now = crate::now_ms();
    match ex {
        Exchange::Binance => binance::funding_paid(k, p, known, cur, now).await,
        Exchange::Bybit => bybit::funding_paid(k, p, known, cur, now).await,
        Exchange::Okx => okx::funding_paid(k, p).await,
        Exchange::Gate => gate::funding_paid(k, p).await,
        Exchange::Hyperliquid => hyperliquid::funding_paid(k, p).await,
        // ponytail: Bitget (all-position totalFee: sign undocumented), Kraken and Lighter not read; add once a
        // real account confirms the field and sign
        _ => Ok(None),
    }
}

/// Group closing fills (realized != 0) by (symbol, second, side) into closes: a Sell that realizes
/// PnL closed a long. Entry is implied from exit and PnL per unit.
pub(crate) fn closes_from_fills(fills: &[Fill]) -> Vec<ClosedPnl> {
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
        Exchange::Bybit => bybit::wallets(k, &mut add).await,
        Exchange::Binance => binance::wallets(k, &mut add).await,
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

/// Amount as the venues want it: at most 8 decimals, rounded DOWN so it never exceeds `free`.
fn amount_str(x: f64) -> String {
    let s = format!("{:.8}", (x * 1e8).floor() / 1e8);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Move `amount` of `coin` from one account to another on the same venue. Never withdraws.
/// Routes without a direct API go through SPOT (Binance PM <-> FUNDING, EARN -> PM), step by
/// step; the returned lines say what happened, and a failed later step names where the funds are.
pub async fn transfer(ex: Exchange, k: &Keys, from: &str, to: &str, coin: &str, amount: f64, product: Option<&str>) -> Result<Vec<String>> {
    if !(amount > 0.0) { bail!("amount must be positive"); }
    let amt = amount_str(amount);
    let mut log = vec![];
    match ex {
        Exchange::Bybit => bybit::transfer(k, from, to, coin, amt, product, &mut log).await?,
        Exchange::Binance => binance::transfer(k, from, to, coin, amt, product, &mut log).await?,
        _ => bail!("{ex:?} transfers not implemented"),
    }
    Ok(log)
}

/// A v4-shaped id: time plus 64 bits of std's OS-seeded hasher over a counter, so ids made by
/// different processes and devices do not collide (demo events and the device id rely on that).
pub(crate) fn uuid_v4() -> String {
    use std::hash::BuildHasher;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let r = std::collections::hash_map::RandomState::new().hash_one(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let x = ((crate::now_ms() as u128) << 64) | r as u128;
    let h = format!("{x:032x}");
    format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

fn is_usd(coin: &str) -> bool { matches!(coin, "USDT" | "USDC" | "USD" | "USDE" | "FDUSD" | "DAI" | "PYUSD") }

#[cfg(test)]
mod tests {
    #[test]
    fn option_order_body() {
        use super::*;
        let b = bybit_opt_body("BTC-25DEC26-100000-P-USDT", Side::Sell, "0.01", "1500", Tif::PostOnly, true);
        assert_eq!((b["category"].as_str(), b["side"].as_str(), b["orderType"].as_str(), b["timeInForce"].as_str()), (Some("option"), Some("Sell"), Some("Limit"), Some("PostOnly")));
        assert!(b["reduceOnly"] == true && b["orderLinkId"].as_str().is_some_and(|s| s.starts_with("t1opt")));
        assert_eq!(spot::effect_str(spot::Effect::Borrow), "MARGIN_BUY");
    }

    #[test]
    fn option_sweep_and_size() {
        let asks = [(1.33, 334.0), (1.34, 30.0), (1.94, 451.0)];
        assert_eq!(sweep(&asks, 287.0), Some((1.33, 287.0, 1.33)));
        let (l, got, avg) = sweep(&asks, 400.0).unwrap();
        assert_eq!((l, got), (1.94, 400.0));
        assert!((avg - (334.0 * 1.33 + 30.0 * 1.34 + 36.0 * 1.94) / 400.0).abs() < 1e-12);
        assert_eq!(sweep(&asks, 2000.0).unwrap().1, 815.0);
        assert_eq!(sweep(&[], 1.0), None);
        assert_eq!(ceil_step(286.2, 1.0, 1.0), 287.0);
        assert_eq!(ceil_step(287.0, 1.0, 1.0), 287.0);
        assert_eq!(ceil_step(0.2, 1.0, 1.0), 1.0);
    }

    use super::*;
    use super::binance::{binance_algo_params, binance_fills, binance_params, binance_universal};
    use super::bybit::{bybit_body, bybit_fills, bybit_opt_body, bybit_trailing_body};

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
    fn open_time_walks_fills_back_to_zero() {
        // long 3 built by buys at t=10 (2) and t=20 (1): opened at 10, the older round trip is ignored
        assert_eq!(open_time(3.0, &[(20, 1.0), (10, 2.0), (5, -4.0), (1, 4.0)]), Some((10, false)));
        // short 1 after a flip: the sell at t=30 took a long 2 through zero, so that fill opened it
        assert_eq!(open_time(-1.0, &[(30, -3.0), (20, 2.0)]), Some((30, false)));
        // fills run out before zero: oldest fill, lower bound
        assert_eq!(open_time(5.0, &[(20, 1.0), (10, 1.0)]), Some((10, true)));
        assert_eq!(open_time(5.0, &[]), None);
        // hedge mode: the SHORT fill in between belongs to the other position
        let rows = json!([
            {"time": 10, "id": 1, "side": "BUY", "qty": "2", "positionSide": "LONG"},
            {"time": 15, "id": 2, "side": "SELL", "qty": "5", "positionSide": "SHORT"},
            {"time": 20, "id": 3, "side": "SELL", "qty": "1", "positionSide": "LONG"},
            {"time": 25, "id": 4, "side": "BUY", "qty": "2", "positionSide": "LONG"}]);
        assert_eq!(binance_fills(&rows, Side::Buy), vec![(25, 2.0), (20, -1.0), (10, 2.0)]);
        assert_eq!(open_time(3.0, &binance_fills(&rows, Side::Buy)), Some((10, false)));
        assert_eq!(open_time(-5.0, &binance_fills(&rows, Side::Sell)), Some((15, false)));
        // Bybit (newest first): a one-way flip buy closes the 1 short and opens 2 long; in hedge mode a sell
        // that opens a short (closedSize 0) does not touch the long
        let rows = [json!({"execTime": "30", "side": "Sell", "execQty": "4", "closedSize": "0", "execType": "Trade"}),
            json!({"execTime": "25", "side": "Buy", "execQty": "1", "closedSize": "0", "execType": "Funding"}),
            json!({"execTime": "20", "side": "Buy", "execQty": "3", "closedSize": "1", "execType": "Trade"}),
            json!({"execTime": "10", "side": "Sell", "execQty": "1", "closedSize": "0", "execType": "Trade"})];
        assert_eq!(bybit_fills(&rows, Side::Buy), vec![(30, 0.0), (20, 2.0), (10, 0.0)]);
        assert_eq!(open_time(2.0, &bybit_fills(&rows, Side::Buy)), Some((20, false)));
    }

    #[test]
    fn binance_signature_matches_docs() {
        // example from Binance's "SIGNED endpoint" documentation
        let s = hmac_hex("NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j",
            "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559");
        assert_eq!(s, "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71");
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
        let f = |side, price, qty, r: f64, ts| Fill { ex: Exchange::Binance, id: String::new(), symbol: "XUSDT".into(), side, price, qty, fee: 0.0, realized: Some(r), ts };
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

    fn pos(side: Side, qty: f64) -> Position {
        Position { ex: Exchange::Kraken, symbol: "BTCUSDT".into(), side, qty, entry: 100.0, mark: 101.0, liq: Some(80.0), upnl: 1.0, lev: 10.0, margin: 50.0, cross: Some(true) }
    }

    #[test]
    fn merge_snapshot_keeps_pushed_fields() {
        // Kraken REST row: mark 0, liq None, margin 0, lev 0, cross unknown; same size and entry as the pushed row
        let rest = Position { mark: 0.0, liq: None, margin: 0.0, lev: 0.0, cross: None, upnl: 2.0, ..pos(Side::Buy, 1.0) };
        let m = merge_snapshot(&[pos(Side::Buy, 1.0)], vec![rest]);
        let p = &m[0];
        assert_eq!((p.mark, p.liq, p.margin, p.lev, p.cross, p.upnl), (101.0, Some(80.0), 50.0, 10.0, Some(true), 2.0));
        // the venue's own figures win when it sends them
        let rest = Position { mark: 105.0, liq: Some(70.0), margin: 40.0, lev: 5.0, cross: Some(false), ..pos(Side::Buy, 1.0) };
        let p = &merge_snapshot(&[pos(Side::Buy, 1.0)], vec![rest])[0];
        assert_eq!((p.mark, p.liq, p.margin, p.lev, p.cross), (105.0, Some(70.0), 40.0, 5.0, Some(false)));
        // resized: liq and margin belonged to the old size and are dropped; mark / lev still carry over
        let rest = Position { mark: 0.0, liq: None, margin: 0.0, lev: 0.0, ..pos(Side::Buy, 2.0) };
        let p = &merge_snapshot(&[pos(Side::Buy, 1.0)], vec![rest])[0];
        assert_eq!((p.mark, p.liq, p.margin, p.lev), (101.0, None, 0.0, 10.0));
        // new entry price counts as a different position too
        let rest = Position { liq: None, entry: 99.0, ..pos(Side::Buy, 1.0) };
        assert_eq!(merge_snapshot(&[pos(Side::Buy, 1.0)], vec![rest])[0].liq, None);
    }

    #[test]
    fn merge_snapshot_drops_adds_and_matches_by_side() {
        let blank = |side, qty| Position { mark: 0.0, liq: None, margin: 0.0, lev: 0.0, cross: None, ..pos(side, qty) };
        // old long only; the snapshot has a short (new: nothing to carry) and no long (dropped)
        let m = merge_snapshot(&[pos(Side::Buy, 1.0)], vec![blank(Side::Sell, 1.0)]);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].side, m[0].mark, m[0].liq, m[0].cross), (Side::Sell, 0.0, None, None));
        // hedge: both sides present, each merges with its own old row
        let old = [Position { mark: 111.0, ..pos(Side::Buy, 1.0) }, Position { mark: 222.0, ..pos(Side::Sell, 1.0) }];
        let m = merge_snapshot(&old, vec![blank(Side::Sell, 1.0), blank(Side::Buy, 1.0)]);
        assert_eq!(m.iter().map(|p| (p.side, p.mark)).collect::<Vec<_>>(), vec![(Side::Sell, 222.0), (Side::Buy, 111.0)]);
        // another venue's row with the same symbol does not leak in
        let other = Position { ex: Exchange::Bybit, ..pos(Side::Buy, 1.0) };
        assert_eq!(merge_snapshot(&[other], vec![blank(Side::Buy, 1.0)])[0].mark, 0.0);
        assert!(merge_snapshot(&[pos(Side::Buy, 1.0)], vec![]).is_empty());
    }

    #[test]
    fn contract_types() {
        // TP/SL exists on the two verified venues only
        assert!(caps(Exchange::Bybit).tpsl && caps(Exchange::Binance).tpsl);
        assert!(![Exchange::Okx, Exchange::Bitget, Exchange::Gate, Exchange::Kraken, Exchange::Hyperliquid, Exchange::Lighter].iter().any(|e| caps(*e).tpsl));
        // an order request survives JSON (jobs persist them)
        let r = OrderReq { symbol: "BTCUSDT".into(), pos: Side::Sell, close: true, kind: Kind::Stop { trigger: 95_000.0, limit: Some(94_900.0), by_mark: true }, qty: 0.5, client_id: Some("t1x".into()) };
        let back: OrderReq = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let t: tpsl::Trigger = serde_json::from_str(&serde_json::to_string(&tpsl::Trigger::Last).unwrap()).unwrap();
        assert_eq!(t, tpsl::Trigger::Last);
        // a bulk cancel counts accepted cancels and names each refusal
        let mut rep = CancelReport::default();
        rep.add("BTCUSDT 1".into(), Ok(()));
        rep.add("BTCUSDT 2".into(), Err(anyhow!("too late")));
        assert_eq!((rep.sent, rep.failed), (1, vec!["BTCUSDT 2: too late".to_string()]));
        // the demo flag never shows in Debug
        let k = Keys { key: "ABCDEFGH".into(), secret: "s".into(), extra: None, demo: true };
        assert_eq!(format!("{k:?}"), "Keys(ABCD…)");
    }

    #[test]
    fn paged_follows_cursors_dedupes_and_stops() {
        use futures_util::FutureExt;
        let ids = |v: Vec<Value>| v.iter().map(|r| r["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let key = |r: &Value| r["id"].to_string();
        let row = |i: &str| json!({"id": i});
        // b moved onto page 2 while paging: listed once
        let server = |c: Option<String>| -> Result<(Vec<Value>, Option<String>)> {
            Ok(match c.as_deref() {
                None => (vec![row("a"), row("b")], Some("p2".into())),
                Some("p2") => (vec![row("b"), row("c")], Some("p3".into())),
                _ => (vec![row("d")], None),
            })
        };
        let all = paged(20, key, |c| async move { server(c) }).now_or_never().unwrap().unwrap();
        assert_eq!(ids(all), ["a", "b", "c", "d"]);
        // the page cap bounds the walk
        let capped = paged(2, key, |c| async move { server(c) }).now_or_never().unwrap().unwrap();
        assert_eq!(ids(capped), ["a", "b", "c"]);
        // a cursor that does not advance ends it after one repeat
        let calls = std::cell::Cell::new(0);
        let stuck = paged(20, key, |_| { calls.set(calls.get() + 1); async { Ok((vec![row("x")], Some("same".to_string()))) } }).now_or_never().unwrap().unwrap();
        assert_eq!((ids(stuck), calls.get()), (vec!["x".to_string()], 2));
        // a failing page fails the walk
        assert!(paged(20, key, |_| async { Err(anyhow!("limit")) }).now_or_never().unwrap().is_err());
    }
}
