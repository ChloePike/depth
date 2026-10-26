//! Demo (paper) trading: a simulated venue behind the same contract as the real ones (`trade`'s
//! OrderReq / Position / OpenOrder / Balance / Fill / History, `tpsl::TpSl`, `spot::Bal`). One
//! USDT pool is shared by every venue, so cross margin covers the whole demo account.
//!
//! The account IS a fold over an append-only event log. `Sim::apply` is the only code that changes
//! it: the live session records events through `Sim::record` (which applies them) and a restart
//! replays the stored log through the same `apply`. Events carry a uuid, the device that made them
//! and a per-device sequence number, so a future sync only exchanges events: `replay` sorts by
//! (ts, device, seq) and skips ids it has already applied. No I/O here: the caller persists
//! `Sim::pending` and turns it into account pushes.
//!
//! Margin math, per position with entry notional `n = qty * entry` and a flat `MMR`:
//! - initial margin `n / lev`, maintenance margin `n * MMR` (entry based, Bybit's convention)
//! - equity = cash + upnl of every position at its mark + spot coins at their last price
//! - available: cross `equity - initial margins - resting order margin`; isolated the same with
//!   cash + coins in place of the equity (an isolated position's PnL stays inside its own margin)
//! - isolated liquidation: long `entry * (1 - 1/lev + MMR)`, short `entry * (1 + 1/lev - MMR)`;
//!   the whole initial margin is lost there (what is left of the maintenance margin is the fee)
//! - cross liquidation: every position at once when `equity <= sum of maintenance margins`; the
//!   price shown per position is `mark - sign * (equity - maintenance) / qty`, other marks fixed
use crate::trade::{self, spot::Bal, tpsl::TpSl, Balance, Fill, FundingPaid, HistOrder, History, Kind, OpenOrder, OrderReq, Position, Tif};
use crate::{Exchange, Market, Side};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub mod fill;

/// Maintenance margin rate of every position. ponytail: flat 0.5% (the first BTC tier on Bybit /
/// Binance), no risk tiers; read the venue's tier table once positions past tier 1 matter.
pub const MMR: f64 = 0.005;
pub const DEFAULT_LEV: f64 = 10.0;
/// ponytail: venue leverage brackets are not read; every symbol allows 1x to 100x.
pub const MAX_LEV: f64 = 100.0;
pub const START_BALANCE: f64 = 10_000.0;

static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Demo mode: keyed trade calls go to the simulated account and its fills stay out of `db::fills_put`.
pub fn on() -> bool { ON.load(std::sync::atomic::Ordering::Relaxed) }
pub fn set_on(v: bool) { ON.store(v, std::sync::atomic::Ordering::Relaxed) }

/// One entry of the log: `id` a uuid, `seq` counts this device's events from 1, `ts` in ms.
#[derive(Clone, Debug, PartialEq)]
pub struct Ev { pub id: String, pub device: String, pub seq: u64, pub ts: i64, pub kind: EvKind }

impl Ev {
    /// The stored form: the kind's JSON as `body`, its tag as `kind`.
    pub fn row(&self) -> crate::db::DemoRow {
        let v = serde_json::to_value(&self.kind).unwrap_or_default();
        crate::db::DemoRow { id: self.id.clone(), device: self.device.clone(), seq: self.seq, ts: self.ts, kind: v["kind"].as_str().unwrap_or_default().into(), body: v.to_string() }
    }

    /// None for a row this build cannot read (a kind from a newer version): replay skips it.
    pub fn from_row(r: &crate::db::DemoRow) -> Option<Ev> {
        Some(Ev { id: r.id.clone(), device: r.device.clone(), seq: r.seq, ts: r.ts, kind: serde_json::from_str(&r.body).ok()? })
    }
}

/// What happened. Stored as JSON (`kind` is the tag); field names are the sync contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EvKind {
    /// start over with `balance` USDT: positions, orders, TP/SL, coins and history go (modes and leverage stay)
    Reset { balance: f64 },
    Config { hedge: bool, cross: bool },
    Leverage { ex: Exchange, symbol: String, lev: f64 },
    /// an order the simulated venue accepted (market orders too: their fills follow)
    Place { o: DemoOrder },
    /// an order or TP/SL is gone: cancelled, an IOC remainder, a fired TP/SL, an orphaned close
    Cancel { id: String },
    /// a resting limit order's new price
    Amend { id: String, price: f64 },
    /// a stop / trailing order fired at `px`: from here it is a market order, or a limit at its limit price
    Trigger { id: String, px: f64 },
    /// one execution at one price; `order` = the DemoOrder or TP/SL id, `pos` = the position side it acts on
    /// (spot: the side), `fee` in USDT
    Fill { order: String, ex: Exchange, market: Market, symbol: String, side: Side, pos: Side, px: f64, qty: f64, fee: f64 },
    /// one settlement: `rate` per settlement, `amount` received (+) or paid (-) in USDT
    Funding { ex: Exchange, symbol: String, pos: Side, rate: f64, amount: f64 },
    /// the venue closed the position at `px`; `fee` = its maintenance margin, kept by the venue
    Liquidate { ex: Exchange, symbol: String, pos: Side, qty: f64, px: f64, fee: f64 },
    /// set a TP/SL (a whole-position one replaces the previous one of its kind)
    TpSl { t: TpSl },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DemoOrder {
    pub id: String, pub ex: Exchange, pub market: Market,
    /// as accepted: size and prices on the venue grid, a BBO order already priced as a GTC limit;
    /// spot orders carry their side in `pos` and never `close`
    pub req: OrderReq,
    pub filled: f64, pub avg: f64, pub ts: i64,
    /// conditional orders: 1 fires when the price rises to the trigger, -1 when it falls (fixed at
    /// placement from the trigger's side of the price); 0 otherwise
    pub dir: i8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pos { pub qty: f64, pub entry: f64,
    /// when it opened (ms) and the funding it received (+) or paid (-) since
    pub opened: i64, pub funding: f64 }

pub type PosKey = (Exchange, String, Side);

#[derive(Clone, Debug, Default)]
pub struct Sim {
    pub device: String,
    /// this device's last event: seq and ts (ts never goes back, so a replay keeps the live order)
    pub seq: u64, last_ts: i64,
    pub hedge: bool, pub cross: bool,
    /// THE USDT pool of every venue
    pub cash: f64,
    pub pos: BTreeMap<PosKey, Pos>,
    pub orders: Vec<DemoOrder>,
    pub tpsl: Vec<TpSl>,
    pub levs: BTreeMap<(Exchange, String), f64>,
    /// spot coins per (venue, coin): (qty, last fill price, their value until a live price arrives)
    pub coins: BTreeMap<(Exchange, String), (f64, f64)>,
    pub fills: Vec<Fill>,
    pub hist: Vec<HistOrder>,
    seen: HashSet<String>,
    /// recorded events the caller has not taken yet (persist, then push, in this order)
    pub pending: Vec<Ev>,
    // live inputs, not part of the fold
    pub marks: HashMap<(Exchange, String), f64>,
    pub lasts: HashMap<(Exchange, Market, String), f64>,
    /// (taker, maker) fee fractions per venue (the user's fee settings); `route`'s defaults otherwise
    pub fees: HashMap<Exchange, (f64, f64)>,
    /// trailing orders' best price since activation. ponytail: not logged, so after a restart it
    /// starts again from the live price (fires later, never earlier); log it if that matters
    trail: HashMap<String, f64>,
}

pub(crate) fn sign(s: Side) -> f64 { if s == Side::Buy { 1.0 } else { -1.0 } }
pub(crate) fn opposite(s: Side) -> Side { if s == Side::Buy { Side::Sell } else { Side::Buy } }
fn coin(symbol: &str) -> &str { symbol.strip_suffix("USDT").unwrap_or(symbol) }

fn kind_name(k: &Kind) -> &'static str {
    match k { Kind::Limit { .. } => "limit", Kind::Market => "market", Kind::Bbo { .. } => "bbo", Kind::Stop { .. } => "stop", Kind::Trailing { .. } => "trailing" }
}

fn tif_name(t: Tif) -> &'static str { match t { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::PostOnly => "PostOnly", Tif::Fok => "FOK" } }

fn limit_price(k: &Kind) -> f64 { match *k { Kind::Limit { price, .. } | Kind::Stop { limit: Some(price), .. } => price, _ => 0.0 } }

impl Sim {
    pub fn new(device: &str) -> Sim { Sim { device: device.into(), cross: true, ..Default::default() } }

    /// Fold a log: sorted by (ts, device, seq), ids seen before skipped; `device`'s next event
    /// follows its highest stored seq.
    pub fn replay(device: &str, evs: impl IntoIterator<Item = Ev>) -> Sim {
        let mut evs: Vec<Ev> = evs.into_iter().collect();
        evs.sort_by(|a, b| (a.ts, &a.device, a.seq).cmp(&(b.ts, &b.device, b.seq)));
        let mut s = Sim::new(device);
        for e in &evs { s.apply(e); }
        for e in evs.iter().filter(|e| e.device == device) { s.seq = s.seq.max(e.seq); s.last_ts = s.last_ts.max(e.ts); }
        s
    }

    /// Mint this device's next event, fold it in and queue it in `pending`.
    pub fn record(&mut self, ts: i64, kind: EvKind) {
        self.seq += 1;
        self.last_ts = self.last_ts.max(ts);
        let ev = Ev { id: trade::uuid_v4(), device: self.device.clone(), seq: self.seq, ts: self.last_ts, kind };
        self.apply(&ev);
        self.pending.push(ev);
    }

    /// The fold: pure and deterministic. An id applied before changes nothing; ids it refers to
    /// that are gone (a cancel after the fill) are no-ops.
    pub fn apply(&mut self, ev: &Ev) {
        if !self.seen.insert(ev.id.clone()) { return; }
        let ts = ev.ts;
        match &ev.kind {
            EvKind::Reset { balance } => {
                self.cash = *balance;
                self.pos.clear(); self.orders.clear(); self.tpsl.clear(); self.coins.clear(); self.fills.clear(); self.hist.clear();
            }
            EvKind::Config { hedge, cross } => (self.hedge, self.cross) = (*hedge, *cross),
            EvKind::Leverage { ex, symbol, lev } => { self.levs.insert((*ex, symbol.clone()), *lev); }
            EvKind::Place { o } => if !self.orders.iter().any(|x| x.id == o.id) { self.orders.push(o.clone()) },
            EvKind::Cancel { id } => {
                if let Some(i) = self.orders.iter().position(|o| &o.id == id) { let o = self.orders.remove(i); self.done(&o, true, ts); }
                self.tpsl.retain(|t| &t.id != id);
            }
            EvKind::Amend { id, price } => if let Some(o) = self.orders.iter_mut().find(|o| &o.id == id)
                && let Kind::Limit { tif, .. } = o.req.kind { o.req.kind = Kind::Limit { price: *price, tif }; },
            EvKind::Trigger { id, .. } => if let Some(o) = self.orders.iter_mut().find(|o| &o.id == id) {
                o.req.kind = match o.req.kind {
                    Kind::Stop { limit: Some(price), .. } => Kind::Limit { price, tif: Tif::Gtc },
                    Kind::Stop { .. } | Kind::Trailing { .. } => Kind::Market,
                    k => k,
                };
                o.dir = 0;
            },
            EvKind::Fill { order, ex, market, symbol, side, pos, px, qty, fee } => {
                // ponytail: spot fees are charged in USDT (the venues take a buy's fee from the coin
                // bought); charge it in the base coin if exact spot balances matter
                let realized = if *market == Market::Spot {
                    let key = (*ex, coin(symbol).to_string());
                    let c = self.coins.entry(key.clone()).or_insert((0.0, *px));
                    (c.0, c.1) = (c.0 + sign(*side) * qty, *px);
                    if c.0.abs() <= qty * 1e-9 { self.coins.remove(&key); }
                    self.cash -= sign(*side) * px * qty;
                    None
                } else { Some(self.net(*ex, symbol, *side, *pos, (*px, *qty), ts)) };
                self.cash += realized.unwrap_or(0.0) - fee;
                self.fills.push(Fill { ex: *ex, id: ev.id.clone(), symbol: symbol.clone(), side: *side, price: *px, qty: *qty, fee: *fee, realized, ts });
                if let Some(i) = self.orders.iter().position(|o| &o.id == order) {
                    let o = &mut self.orders[i];
                    o.avg = (o.avg * o.filled + px * qty) / (o.filled + qty);
                    o.filled += qty;
                    if o.filled >= o.req.qty * (1.0 - 1e-9) { let o = self.orders.remove(i); self.done(&o, false, ts); }
                }
            }
            EvKind::Funding { ex, symbol, pos, amount, .. } => {
                self.cash += amount;
                if let Some(p) = self.pos.get_mut(&(*ex, symbol.clone(), *pos)) { p.funding += amount; }
            }
            EvKind::Liquidate { ex, symbol, pos, qty, px, fee } => {
                let key = (*ex, symbol.clone(), *pos);
                let Some(p) = self.pos.get(&key) else { return };
                let q = qty.min(p.qty);
                let mut pnl = (px - p.entry) * q * sign(*pos) - fee;
                // isolated: the loss stops at the position's own margin (a gap past it is the venue's insurance fund)
                if !self.cross { pnl = pnl.max(-q * p.entry / self.lev(*ex, symbol)); }
                // cross: the account never owes the venue; `fill::liquidate` records profitable closes first
                // so this floor equals the floor of the whole liquidation's sum
                self.cash = (self.cash + pnl).max(0.0);
                self.shrink(&key, q);
                let side = opposite(*pos);
                self.fills.push(Fill { ex: *ex, id: ev.id.clone(), symbol: symbol.clone(), side, price: *px, qty: q, fee: *fee, realized: Some(pnl + fee), ts });
                self.hist.push(HistOrder { ex: *ex, symbol: symbol.clone(), side, kind: "liquidation".into(), price: *px, avg: *px, qty: q, filled: q, status: "Filled".into(), ts });
            }
            EvKind::TpSl { t } => {
                self.tpsl.retain(|x| x.id != t.id && !(t.qty.is_none() && x.qty.is_none() && x.ex == t.ex && x.symbol == t.symbol && x.pos == t.pos && x.take_profit == t.take_profit));
                self.tpsl.push(t.clone());
            }
        }
    }

    /// Nets one perp fill into the positions; returns the PnL realized by its reducing part.
    /// One-way: the held side takes it (average entry when adding; a flip opens the rest at the
    /// fill price). Hedge: only the `pos` side, and a close never takes it past zero.
    fn net(&mut self, ex: Exchange, symbol: &str, side: Side, pos: Side, (px, qty): (f64, f64), ts: i64) -> f64 {
        let has = |s: &Self, sd: Side| s.pos.contains_key(&(ex, symbol.to_string(), sd));
        let target = if self.hedge { pos } else if has(self, Side::Buy) { Side::Buy } else if has(self, Side::Sell) { Side::Sell } else { side };
        let key = (ex, symbol.to_string(), target);
        let (mut realized, mut left) = (0.0, qty);
        if let Some(p) = self.pos.get_mut(&key) {
            if side == target {
                p.entry = (p.entry * p.qty + px * qty) / (p.qty + qty);
                p.qty += qty;
                return 0.0;
            }
            let r = qty.min(p.qty);
            realized = (px - p.entry) * r * sign(target);
            left -= r;
            self.shrink(&key, r);
        }
        if left > qty * 1e-9 && (!self.hedge || side == pos) {
            self.pos.insert((ex, symbol.to_string(), side), Pos { qty: left, entry: px, opened: ts, funding: 0.0 });
        }
        realized
    }

    fn shrink(&mut self, key: &PosKey, q: f64) {
        if let Some(p) = self.pos.get_mut(key) {
            p.qty -= q;
            if p.qty <= (p.qty + q) * 1e-9 { self.pos.remove(key); }
        }
    }

    fn done(&mut self, o: &DemoOrder, cancelled: bool, ts: i64) {
        let status = match (cancelled, o.filled > 0.0) { (false, _) => "Filled", (true, true) => "PartiallyFilledCanceled", (true, false) => "Cancelled" };
        self.hist.push(HistOrder { ex: o.ex, symbol: o.req.symbol.clone(), side: o.req.side(), kind: kind_name(&o.req.kind).into(), price: limit_price(&o.req.kind),
                                   avg: o.avg, qty: o.req.qty, filled: o.filled, status: status.into(), ts });
    }

    // ---------------------------------------------------------------- account settings

    /// Start over with `balance` USDT.
    pub fn reset(&mut self, balance: f64, now: i64) { self.record(now, EvKind::Reset { balance }); }

    /// Position mode and margin mode of the whole demo account. Like the venues, refused while a
    /// position or perp order is open. ponytail: one margin mode for every symbol (venues set it per
    /// symbol); key `cross` by (ex, symbol) like `levs` if mixed modes are wanted.
    pub fn configure(&mut self, hedge: bool, cross: bool, now: i64) -> Result<()> {
        if (hedge, cross) == (self.hedge, self.cross) { return Ok(()); }
        if !self.pos.is_empty() || self.orders.iter().any(|o| o.market == Market::Perp) {
            bail!("demo: close every position and order before switching the position or margin mode");
        }
        self.record(now, EvKind::Config { hedge, cross });
        Ok(())
    }

    /// Leverage of (ex, symbol); a lower one needs the extra margin of what is already open.
    pub fn set_leverage(&mut self, ex: Exchange, symbol: &str, lev: f64, now: i64) -> Result<()> {
        if !(1.0..=MAX_LEV).contains(&lev) { bail!("demo: leverage must be 1x to {MAX_LEV}x"); }
        let old = self.lev(ex, symbol);
        let more: f64 = self.pos.iter().filter(|(k, _)| k.0 == ex && k.1 == symbol).map(|(_, p)| p.qty * p.entry * (1.0 / lev - 1.0 / old)).sum();
        if more > 0.0 && more > self.available() { bail!("demo: insufficient margin for {lev}x: {more:.2} > {:.2} USDT", self.available()); }
        self.record(now, EvKind::Leverage { ex, symbol: symbol.into(), lev });
        Ok(())
    }

    // ---------------------------------------------------------------- account math

    pub fn lev(&self, ex: Exchange, symbol: &str) -> f64 { self.levs.get(&(ex, symbol.to_string())).copied().unwrap_or(DEFAULT_LEV) }

    /// Size of the `side` position of (ex, symbol), 0 when none.
    pub fn held(&self, ex: Exchange, symbol: &str, side: Side) -> f64 { self.pos.get(&(ex, symbol.to_string(), side)).map_or(0.0, |p| p.qty) }

    /// The position's mark; its entry until the venue sent one.
    fn mark_of(&self, k: &PosKey, p: &Pos) -> f64 { self.marks.get(&(k.0, k.1.clone())).copied().unwrap_or(p.entry) }

    fn upnl(&self, k: &PosKey, p: &Pos) -> f64 { (self.mark_of(k, p) - p.entry) * p.qty * sign(k.2) }

    fn coin_value(&self) -> f64 {
        self.coins.iter().map(|((ex, c), &(q, px))| q * self.lasts.get(&(*ex, Market::Spot, format!("{c}USDT"))).copied().unwrap_or(px)).sum()
    }

    pub fn equity(&self) -> f64 { self.cash + self.pos.iter().map(|(k, p)| self.upnl(k, p)).sum::<f64>() + self.coin_value() }

    pub fn maint(&self) -> f64 { self.pos.values().map(|p| p.qty * p.entry * MMR).sum() }

    fn initial(&self) -> f64 { self.pos.iter().map(|(k, p)| p.qty * p.entry / self.lev(k.0, &k.1)).sum() }

    /// Held by resting limits: opening perp orders `left * price / lev`, spot buys `left * price`.
    /// Conditional orders hold nothing until they fire (as on Bybit / Binance).
    /// ponytail: a one-way order against the held side reserves its full size, not only the flip
    fn reserved(&self) -> f64 {
        self.orders.iter().filter_map(|o| {
            let Kind::Limit { price, .. } = o.req.kind else { return None };
            let left = o.req.qty - o.filled;
            match o.market {
                Market::Spot => (o.req.side() == Side::Buy).then_some(left * price),
                _ => (!o.req.close).then(|| left * price / self.lev(o.ex, &o.req.symbol)),
            }
        }).sum()
    }

    pub fn available(&self) -> f64 {
        (if self.cross { self.equity() } else { self.cash + self.coin_value() }) - self.initial() - self.reserved()
    }

    fn liq(&self, k: &PosKey, p: &Pos) -> f64 {
        let s = sign(k.2);
        if self.cross { (self.mark_of(k, p) - s * (self.equity() - self.maint()) / p.qty).max(0.0) }
        else { p.entry * (1.0 - s / self.lev(k.0, &k.1) + s * MMR) }
    }

    // ---------------------------------------------------------------- the venue contract's readers

    /// The one pool, the same on every venue.
    pub fn balance(&self) -> Balance {
        let (equity, mm) = (self.equity(), self.maint());
        let mm_rate = if mm == 0.0 { 0.0 } else if equity > 0.0 { mm / equity } else { 1.0 };
        Balance { equity, available: self.available().max(0.0), uni_mmr: None, mm_rate: Some(mm_rate), maint_margin: Some(mm), adj_equity: None }
    }

    /// Positions on `ex`, always with a liquidation price.
    pub fn positions(&self, ex: Exchange) -> Vec<Position> {
        self.pos.iter().filter(|(k, _)| k.0 == ex).map(|(k, p)| {
            let lev = self.lev(ex, &k.1);
            Position { ex, symbol: k.1.clone(), side: k.2, qty: p.qty, entry: p.entry, mark: self.mark_of(k, p), liq: Some(self.liq(k, p)),
                       upnl: self.upnl(k, p), lev, margin: p.qty * p.entry / lev, cross: Some(self.cross) }
        }).collect()
    }

    /// Resting and conditional orders on `ex` (conditional rows carry `trigger`, price = their limit or 0).
    pub fn open_orders(&self, ex: Exchange, market: Market) -> Vec<OpenOrder> {
        self.orders.iter().filter(|o| o.ex == ex && o.market == market).map(|o| {
            let (price, trigger, tif) = match o.req.kind {
                Kind::Limit { price, tif } => (price, None, tif_name(tif)),
                Kind::Stop { trigger, limit, .. } => (limit.unwrap_or(0.0), Some(trigger), ""),
                // the level it fires at now; its activation price until it tracks
                Kind::Trailing { callback_pct, activation } => {
                    let s = sign(o.req.side()) * callback_pct / 100.0;
                    (0.0, self.trail.get(&o.id).map(|b| b * (1.0 + s)).or(activation), "")
                }
                Kind::Market | Kind::Bbo { .. } => (0.0, None, ""),
            };
            OpenOrder { ex, symbol: o.req.symbol.clone(), id: o.id.clone(), side: o.req.side(), price, qty: o.req.qty, filled: o.filled,
                        kind: kind_name(&o.req.kind).into(), tif: tif.into(), reduce_only: o.req.close, ts: o.ts,
                        pos: (self.hedge && market == Market::Perp).then_some(o.req.pos), client_id: o.req.client_id.clone(), trigger }
        }).collect()
    }

    /// Orders, fills and closes on `ex`, newest first (like `trade::history`).
    pub fn history(&self, ex: Exchange) -> History {
        let fills: Vec<Fill> = self.fills.iter().filter(|f| f.ex == ex).cloned().collect();
        let mut h = History { closed: trade::closes_from_fills(&fills), fills, orders: self.hist.iter().filter(|o| o.ex == ex).cloned().collect() };
        h.orders.reverse();
        h.fills.reverse();
        h.closed.reverse();
        h
    }

    /// Size of `coin` on `ex` that resting spot sells hold.
    fn selling(&self, ex: Exchange, c: &str) -> f64 {
        self.orders.iter().filter(|o| o.ex == ex && o.market == Market::Spot && o.req.side() == Side::Sell && coin(&o.req.symbol) == c).map(|o| o.req.qty - o.filled).sum()
    }

    /// The venue's spot wallet: USDT free = the pool's available (shared by every venue), plus the
    /// coins bought on this venue.
    pub fn spot_balances(&self, ex: Exchange) -> Vec<Bal> {
        let buying: f64 = self.orders.iter().filter(|o| o.ex == ex && o.market == Market::Spot && o.req.side() == Side::Buy).map(|o| (o.req.qty - o.filled) * limit_price(&o.req.kind)).sum();
        let mut v = vec![Bal { coin: "USDT".into(), free: self.available().max(0.0), locked: buying, borrowed: 0.0 }];
        for ((e, c), &(q, _)) in &self.coins {
            if *e == ex { let locked = self.selling(ex, c); v.push(Bal { coin: c.clone(), free: q - locked, locked, borrowed: 0.0 }); }
        }
        v
    }

    /// Funding of the open `side` position since it opened.
    pub fn funding_paid(&self, ex: Exchange, symbol: &str, side: Side) -> Option<FundingPaid> {
        self.pos.get(&(ex, symbol.to_string(), side)).map(|p| FundingPaid { amount: p.funding, since: p.opened, lower: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Side::{Buy, Sell};

    pub(super) const BTC: &str = "BTCUSDT";

    pub(super) fn fill(ex: Exchange, symbol: &str, side: Side, pos: Side, px: f64, qty: f64, fee: f64) -> EvKind {
        EvKind::Fill { order: String::new(), ex, market: Market::Perp, symbol: symbol.into(), side, pos, px, qty, fee }
    }

    fn by(side: Side, pos: Side, px: f64, qty: f64, fee: f64) -> EvKind { fill(Exchange::Bybit, BTC, side, pos, px, qty, fee) }

    fn ev(dev: &str, seq: u64, ts: i64, kind: EvKind) -> Ev { Ev { id: format!("{dev}-{seq}"), device: dev.into(), seq, ts, kind } }

    /// The folded state, comparable (Debug of exact floats and ordered maps).
    pub(super) fn snap(s: &Sim) -> String {
        format!("{:?}", (s.cash, &s.pos, &s.orders, &s.tpsl, &s.levs, &s.coins, &s.fills, &s.hist, s.hedge, s.cross))
    }

    pub(super) fn sim(balance: f64) -> Sim { let mut s = Sim::new("a"); s.reset(balance, 0); s }

    fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-9 }

    #[test]
    fn one_way_netting_flip_and_fees() {
        let mut s = sim(1000.0);
        s.record(1, by(Buy, Buy, 100.0, 1.0, 0.1));
        s.record(2, by(Buy, Buy, 110.0, 1.0, 0.1));
        let p = &s.pos[&(Exchange::Bybit, BTC.into(), Buy)];
        assert_eq!((p.qty, p.entry, p.opened), (2.0, 105.0, 1));
        // sell 3: closes the long 2 at +15 each, opens a short 1 at 120
        s.record(3, by(Sell, Sell, 120.0, 3.0, 0.2));
        assert!(!s.pos.contains_key(&(Exchange::Bybit, BTC.into(), Buy)));
        let p = &s.pos[&(Exchange::Bybit, BTC.into(), Sell)];
        assert_eq!((p.qty, p.entry, p.opened), (1.0, 120.0, 3));
        assert_eq!(s.fills[2].realized, Some(30.0));
        assert!(close(s.cash, 1000.0 + 30.0 - 0.4));
        // an exact close leaves nothing
        s.record(4, by(Buy, Buy, 120.0, 1.0, 0.0));
        assert!(s.pos.is_empty());
    }

    #[test]
    fn hedge_keeps_sides_apart() {
        let mut s = sim(1000.0);
        s.configure(true, true, 1).unwrap();
        s.record(2, by(Buy, Buy, 100.0, 1.0, 0.0));
        s.record(3, by(Sell, Sell, 110.0, 1.0, 0.0));
        assert_eq!((s.held(Exchange::Bybit, BTC, Buy), s.held(Exchange::Bybit, BTC, Sell)), (1.0, 1.0));
        // buy to close half the short: the long is untouched
        s.record(4, by(Buy, Sell, 100.0, 0.5, 0.0));
        assert_eq!((s.held(Exchange::Bybit, BTC, Buy), s.held(Exchange::Bybit, BTC, Sell)), (1.0, 0.5));
        assert_eq!(s.fills.last().unwrap().realized, Some(5.0));
        // closing the long with more than held stops at zero: no short is opened by it
        s.record(5, by(Sell, Buy, 90.0, 2.0, 0.0));
        assert_eq!((s.held(Exchange::Bybit, BTC, Buy), s.held(Exchange::Bybit, BTC, Sell)), (0.0, 0.5));
        assert!(close(s.cash, 1000.0 + 5.0 - 10.0));
        // modes are refused while something is open
        assert!(s.configure(false, true, 6).is_err());
    }

    /// Bybit long 10 BTC @100 and OKX short 20 ETH @50, both 10x.
    fn two(cash: f64, cross: bool) -> Sim {
        let mut s = sim(cash);
        s.configure(false, cross, 0).unwrap();
        s.record(1, by(Buy, Buy, 100.0, 10.0, 0.0));
        s.record(2, fill(Exchange::Okx, "ETHUSDT", Sell, Sell, 50.0, 20.0, 0.0));
        s
    }

    #[test]
    fn equity_available_and_mm_rate() {
        let mut s = two(10_000.0, true);
        s.marks.insert((Exchange::Bybit, BTC.into()), 105.0);
        s.marks.insert((Exchange::Okx, "ETHUSDT".into()), 55.0);
        // upnl +50 and -100; initial margin 100 + 100; maintenance (1000 + 1000) * 0.5%
        let b = s.balance();
        assert!(close(b.equity, 9950.0) && close(b.available, 9750.0));
        assert!(close(b.maint_margin.unwrap(), 10.0) && close(b.mm_rate.unwrap(), 10.0 / 9950.0));
        // a resting opening buy of 1 @90 at 10x holds 9; a resting close holds nothing
        let o = |id: &str, close| DemoOrder { id: id.into(), ex: Exchange::Bybit, market: Market::Perp, req: OrderReq { symbol: BTC.into(), pos: Buy, close,
            kind: Kind::Limit { price: 90.0, tif: Tif::Gtc }, qty: 1.0, client_id: None, attach: None }, filled: 0.0, avg: 0.0, ts: 3, dir: 0 };
        s.record(3, EvKind::Place { o: o("open", false) });
        s.record(3, EvKind::Place { o: o("close", true) });
        assert!(close(s.balance().available, 9741.0));
        // positions carry their figures, one pool on every venue
        let p = &s.positions(Exchange::Okx)[0];
        assert_eq!((p.side, p.qty, p.entry, p.mark, p.lev, p.margin, p.cross), (Sell, 20.0, 50.0, 55.0, 10.0, 100.0, Some(true)));
        assert!(close(p.upnl, -100.0));
        // isolated: PnL stays inside the positions' margins
        let mut s = two(10_000.0, false);
        s.marks.insert((Exchange::Bybit, BTC.into()), 105.0);
        assert!(close(s.balance().available, 9800.0) && close(s.balance().equity, 10_050.0));
    }

    #[test]
    fn liquidation_prices() {
        let s = two(10_000.0, false);
        let long = s.positions(Exchange::Bybit)[0].liq.unwrap();
        let short = s.positions(Exchange::Okx)[0].liq.unwrap();
        assert!(close(long, 100.0 * (1.0 - 0.1 + MMR)) && close(short, 50.0 * (1.0 + 0.1 - MMR)));
        // cross: equity 300, maintenance 10 -> long 100 - 290/10, short 50 + 290/20
        let mut s = two(300.0, true);
        assert!(close(s.positions(Exchange::Bybit)[0].liq.unwrap(), 71.0));
        assert!(close(s.positions(Exchange::Okx)[0].liq.unwrap(), 64.5));
        // the short loses 40: the long's liquidation price rises by 40 / 10
        s.marks.insert((Exchange::Okx, "ETHUSDT".into()), 52.0);
        assert!(close(s.positions(Exchange::Bybit)[0].liq.unwrap(), 75.0));
        // deep in profit: never below zero
        let s = two(1e9, true);
        assert_eq!(s.positions(Exchange::Bybit)[0].liq, Some(0.0));
    }

    #[test]
    fn liquidate_caps_isolated_loss_and_floors_cross_cash() {
        // isolated 10x long 10 @100: margin 100; a gap to 80 loses 200 + fee but only the margin goes
        let mut s = two(1000.0, false);
        s.record(3, EvKind::Liquidate { ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, qty: 10.0, px: 80.0, fee: 5.0 });
        assert!(close(s.cash, 900.0) && s.held(Exchange::Bybit, BTC, Buy) == 0.0);
        assert_eq!(s.history(Exchange::Bybit).orders[0].kind, "liquidation");
        // cross: cash never goes below zero
        let mut s = two(150.0, true);
        s.record(3, EvKind::Liquidate { ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, qty: 10.0, px: 80.0, fee: 5.0 });
        assert_eq!(s.cash, 0.0);
        assert_eq!(s.held(Exchange::Okx, "ETHUSDT", Sell), 20.0);
    }

    #[test]
    fn funding_sign_and_spot_balances() {
        let mut s = two(1000.0, true);
        s.record(3, EvKind::Funding { ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, rate: 0.0001, amount: -0.1 });
        assert!(close(s.cash, 999.9));
        assert_eq!(s.funding_paid(Exchange::Bybit, BTC, Buy), Some(FundingPaid { amount: -0.1, since: 1, lower: false }));
        // spot: USDT from the pool, coins on the venue they were bought on
        let mut s = sim(1000.0);
        let spot = |side, px, qty| EvKind::Fill { order: String::new(), ex: Exchange::Binance, market: Market::Spot, symbol: BTC.into(), side, pos: side, px, qty, fee: 0.05 };
        s.record(1, spot(Buy, 100.0, 0.5));
        assert!(close(s.cash, 949.95) && s.pos.is_empty());
        assert!(close(s.balance().equity, 999.95));
        s.lasts.insert((Exchange::Binance, Market::Spot, BTC.into()), 110.0);
        assert!(close(s.balance().equity, 1004.95));
        let b = s.spot_balances(Exchange::Binance);
        assert_eq!((b[1].coin.as_str(), b[1].free), ("BTC", 0.5));
        assert_eq!(s.spot_balances(Exchange::Bybit).len(), 1);
        s.record(2, spot(Sell, 110.0, 0.5));
        assert!(close(s.cash, 1004.9) && s.coins.is_empty());
        assert_eq!(s.fills[1].realized, None);
    }

    #[test]
    fn replay_is_order_independent_and_idempotent() {
        let a = |seq, ts, k| ev("a", seq, ts, k);
        let b = |seq, ts, k| ev("b", seq, ts, k);
        let log = vec![
            a(1, 1, EvKind::Reset { balance: 1000.0 }),
            b(1, 2, EvKind::Leverage { ex: Exchange::Bybit, symbol: BTC.into(), lev: 5.0 }),
            a(2, 2, by(Buy, Buy, 100.0, 1.0, 0.1)),
            b(2, 3, by(Buy, Buy, 102.0, 1.0, 0.1)),
            a(3, 3, by(Sell, Sell, 101.0, 3.0, 0.1)),
            b(3, 3, EvKind::Funding { ex: Exchange::Bybit, symbol: BTC.into(), pos: Sell, rate: 0.0001, amount: 0.0101 }),
            a(4, 4, EvKind::TpSl { t: TpSl { id: "t".into(), ex: Exchange::Bybit, symbol: BTC.into(), pos: Sell, take_profit: true, trigger_px: 90.0, qty: None, trigger: Default::default() } }),
        ];
        let once = Sim::replay("a", log.clone());
        let mut shuffled = log.clone();
        shuffled.reverse();
        shuffled.rotate_left(3);
        assert_eq!(snap(&Sim::replay("a", shuffled)), snap(&once));
        let twice: Vec<Ev> = log.iter().chain(log.iter()).cloned().collect();
        assert_eq!(snap(&Sim::replay("a", twice)), snap(&once));
        assert_eq!((once.seq, Sim::replay("b", log.clone()).seq), (4, 3));
        assert_eq!(once.held(Exchange::Bybit, BTC, Sell), 1.0);
        // live recording replays to the same account
        let mut live = Sim::new("a");
        for e in &log { live.record(e.ts, e.kind.clone()); }
        assert_eq!(snap(&Sim::replay("a", live.pending.clone())), snap(&live));
        assert_eq!(live.pending.iter().map(|e| e.seq).collect::<Vec<_>>(), (1..=7).collect::<Vec<_>>());
        // the clock going back never reorders this device's events
        let mut s = Sim::new("a");
        s.record(10, EvKind::Reset { balance: 1.0 });
        s.record(5, EvKind::Reset { balance: 2.0 });
        assert_eq!((s.pending[1].ts, Sim::replay("a", s.pending.clone()).cash), (10, 2.0));
    }

    #[test]
    fn events_survive_their_stored_form() {
        let mut s = sim(1000.0);
        s.record(1, by(Buy, Buy, 100.123456789, 0.001, 0.1 + 0.2));
        let o = DemoOrder { id: "o".into(), ex: Exchange::Okx, market: Market::Spot, req: OrderReq { symbol: BTC.into(), pos: Sell, close: false,
            kind: Kind::Stop { trigger: 1e-7, limit: Some(3.0), by_mark: false }, qty: 1.0 / 3.0, client_id: Some("c".into()), attach: None }, filled: 0.0, avg: 0.0, ts: 2, dir: -1 };
        s.record(2, EvKind::Place { o });
        s.record(3, EvKind::TpSl { t: TpSl { id: "t".into(), ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, take_profit: false, trigger_px: 90.5, qty: Some(0.5), trigger: trade::tpsl::Trigger::Last } });
        for e in &s.pending { assert_eq!(Ev::from_row(&e.row()).as_ref(), Some(e)); }
        let r = s.pending[1].row();
        assert_eq!((r.kind.as_str(), r.seq, r.device.as_str()), ("Fill", 2, "a"));
        assert!(r.body.contains("\"kind\":\"Fill\"") && r.body.contains("\"fee\":0.30000000000000004"), "{}", r.body);
        assert_eq!(Ev::from_row(&crate::db::DemoRow { body: "{\"kind\":\"FromTheFuture\"}".into(), ..r }), None);
        assert_eq!(snap(&Sim::replay("a", s.pending.iter().map(|e| Ev::from_row(&e.row()).unwrap()))), snap(&s));
    }

    #[test]
    fn reset_clears_and_later_events_stay() {
        let mut s = sim(1000.0);
        s.record(1, by(Buy, Buy, 100.0, 1.0, 0.1));
        s.record(1, EvKind::Leverage { ex: Exchange::Bybit, symbol: BTC.into(), lev: 3.0 });
        s.record(2, EvKind::TpSl { t: TpSl { id: "t".into(), ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, take_profit: true, trigger_px: 120.0, qty: None, trigger: Default::default() } });
        s.reset(500.0, 3);
        assert!(s.pos.is_empty() && s.tpsl.is_empty() && s.fills.is_empty() && s.hist.is_empty() && s.cash == 500.0);
        assert_eq!(s.lev(Exchange::Bybit, BTC), 3.0);
        s.record(4, by(Sell, Sell, 100.0, 1.0, 0.0));
        let r = Sim::replay("a", s.pending.clone());
        assert_eq!(snap(&r), snap(&s));
        assert_eq!((r.held(Exchange::Bybit, BTC, Sell), r.fills.len()), (1.0, 1));
    }

    #[test]
    fn unknown_ids_and_duplicates_are_no_ops() {
        let mut s = sim(1000.0);
        s.record(1, by(Buy, Buy, 100.0, 1.0, 0.1));
        let before = snap(&s);
        s.record(2, EvKind::Cancel { id: "nope".into() });
        s.record(2, EvKind::Amend { id: "nope".into(), price: 1.0 });
        s.record(2, EvKind::Trigger { id: "nope".into(), px: 1.0 });
        assert_eq!(snap(&s), before);
        let dup = s.pending[1].clone();
        s.apply(&dup);
        assert_eq!(snap(&s), before);
        // a whole-position TP replaces the previous one; partial levels add up
        let t = |id: &str, qty| EvKind::TpSl { t: TpSl { id: id.into(), ex: Exchange::Bybit, symbol: BTC.into(), pos: Buy, take_profit: true, trigger_px: 120.0, qty, trigger: Default::default() } };
        for k in [t("w1", None), t("w2", None), t("p1", Some(0.5)), t("p2", Some(0.5))] { s.record(3, k); }
        assert_eq!(s.tpsl.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["w2", "p1", "p2"]);
        // leverage bounds and the extra margin a lower leverage needs
        assert!(s.set_leverage(Exchange::Bybit, BTC, 0.5, 4).is_err() && s.set_leverage(Exchange::Bybit, BTC, 101.0, 4).is_err());
        let mut poor = sim(15.0);
        poor.record(1, by(Buy, Buy, 100.0, 1.0, 0.0));
        assert!(poor.set_leverage(Exchange::Bybit, BTC, 2.0, 2).is_err());
        assert!(poor.set_leverage(Exchange::Bybit, BTC, 20.0, 2).is_ok());
    }
}
