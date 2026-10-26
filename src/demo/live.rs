//! The live demo account: one global `Sim`, its event log in depth.db, market data from the
//! engine's `Agg`, and the pushes a venue's private stream would send. The `trade` / `tpsl` /
//! `spot` dispatch hands every call made with demo keys (`Keys.demo`) to the functions here; none
//! of them opens a connection.
//!
//! LOCK RULE: never lock the Agg while holding the Sim. A venue's book is copied first (its OWN raw
//! book in its own quote, never the composite), then the Sim is locked.
use super::{coin, fill::Levels, Ev, Sim, EvKind, MAX_LEV, START_BALANCE};
use crate::agg::Agg;
use crate::db::{self, DemoRow};
use crate::trade::{self, spot::Bal, tpsl::{TpSl, Trigger}, AccEvent, AccTx, Balance, Coin, Fill, Found, FundingPaid, History, Mode, OpenOrder, OrderReq, Position, Rules, Wallet};
use crate::{now_ms, Event, Exchange, Market, Msg, Side};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Levels copied per side of a venue book.
const DEPTH: usize = 50;
/// A book not updated for this long is stale: market and crossing orders are refused.
const STALE_MS: i64 = 5_000;

type Book = (Vec<(f64, f64)>, Vec<(f64, f64)>);
type Shared<T> = Arc<Mutex<T>>;

/// The engine's Agg, the base it streams, and the last prices of the watched other bases.
static MARKET: Mutex<Option<(Shared<Agg>, String, Shared<HashMap<String, f64>>)>> = Mutex::new(None);
/// (taker, maker) fractions per venue until the Sim exists
static FEES: Mutex<BTreeMap<Exchange, (f64, f64)>> = Mutex::new(BTreeMap::new());
static LIVE: OnceLock<Mutex<Live>> = OnceLock::new();

struct Live {
    sim: Sim,
    /// resting orders and TP/SL pushed so far -> venue, market: whatever leaves the Sim gets its done push
    ids: HashMap<String, (Exchange, Market)>,
    /// each venue's private-stream channel and the registration owning it
    tx: HashMap<Exchange, (u64, AccTx)>,
    /// (venue, symbol) -> (hourly funding rate, next settlement ms), as last seen
    funding: HashMap<(Exchange, String), (f64, i64)>,
    /// positions + wallet the tick last pushed per venue
    last: HashMap<Exchange, String>,
    persist: bool,
}

/// The account from a stored log: an empty log starts with a recorded reset to `START_BALANCE`, and
/// this device's next seq follows the highest stored one (rows this build cannot read included).
fn load(device: &str, rows: &[DemoRow], last_seq: u64, now: i64) -> Sim {
    let mut s = Sim::replay(device, rows.iter().filter_map(Ev::from_row));
    s.seq = s.seq.max(last_seq);
    if rows.is_empty() { s.reset(START_BALANCE, now); }
    s
}

fn live() -> MutexGuard<'static, Live> {
    LIVE.get_or_init(|| {
        // unit tests never touch the user's database
        let (dev, rows, last) = if cfg!(test) { ("test".to_string(), vec![], 0) } else {
            let d = db::device_id();
            let last = db::demo_last_seq(&d);
            (d, db::demo_rows(), last)
        };
        let mut sim = load(&dev, &rows, last, now_ms());
        sim.fees = FEES.lock().unwrap().iter().map(|(e, f)| (*e, *f)).collect();
        let mut l = Live::new(sim, !cfg!(test));
        l.commit();
        Mutex::new(l)
    }).lock().unwrap_or_else(|e| e.into_inner())
}

fn levels(b: Option<&Book>) -> Option<Levels<'_>> { b.map(|(bids, asks)| Levels { bids, asks }) }

/// The venue's own fresh book (its BBO when it streams no depth), None when stale.
fn copy(a: &Agg, k: (Exchange, Market)) -> Option<Book> {
    let v = a.venues.get(&k)?;
    if now_ms() - v.fresh > STALE_MS { return None; }
    let b: Book = (v.book.bids().take(DEPTH).collect(), v.book.asks().take(DEPTH).collect());
    if !b.0.is_empty() || !b.1.is_empty() { return Some(b); }
    v.bbo.map(|[bid, bq, ask, aq]| (vec![(bid, bq)], vec![(ask, aq)]))
}

/// `symbol`'s book on (ex, market) when it is the one streamed, else None.
fn book(ex: Exchange, market: Market, symbol: &str) -> Option<Book> {
    let (agg, base) = MARKET.lock().unwrap().as_ref().map(|(a, b, _)| (a.clone(), b.clone()))?;
    if symbol != trade::symbol(&base) { return None; }
    let a = agg.lock().unwrap();
    copy(&a, (ex, market))
}

impl Live {
    fn new(sim: Sim, persist: bool) -> Live {
        let ids = sim.orders.iter().map(|o| (o.id.clone(), (o.ex, o.market))).chain(sim.tpsl.iter().map(|t| (t.id.clone(), (t.ex, Market::Perp)))).collect();
        Live { sim, ids, tx: HashMap::new(), funding: HashMap::new(), last: HashMap::new(), persist }
    }

    /// Store what the Sim calls since the last commit recorded, then push it to the venues' streams.
    fn commit(&mut self) -> Vec<Ev> {
        let evs = std::mem::take(&mut self.sim.pending);
        if evs.is_empty() { return evs; }
        if self.persist {
            let rows: Vec<DemoRow> = evs.iter().map(Ev::row).collect();
            let n = db::demo_append(&rows);
            if n < rows.len() { crate::sys::append_log("trade.log", &format!("demo: stored {n} of {} events\n", rows.len())); }
        }
        let venues: Vec<Exchange> = self.tx.keys().copied().collect();
        for (ex, ev) in pushes(&self.sim, &mut self.ids, &venues, &evs) {
            if let Some((_, tx)) = self.tx.get(&ex) { let _ = tx.send((ex, ev)); }
        }
        evs
    }

    /// (venue, market) feeds the Sim needs for `symbol`: orders, TP/SL, positions (marks), coins.
    fn watching(&self, symbol: &str) -> Vec<(Exchange, Market)> {
        let s = &self.sim;
        let mut v = vec![];
        for k in s.pos.keys().filter(|k| k.1 == symbol).map(|k| (k.0, Market::Perp))
            .chain(s.orders.iter().filter(|o| o.req.symbol == symbol).map(|o| (o.ex, o.market)))
            .chain(s.tpsl.iter().filter(|t| t.symbol == symbol).map(|t| (t.ex, Market::Perp)))
            .chain(s.coins.keys().filter(|k| k.1 == coin(symbol)).map(|k| (k.0, Market::Spot))) {
            if !v.contains(&k) { v.push(k); }
        }
        v
    }

    /// One pump batch of the on-screen `symbol`: trades fill resting limits and fire last-price
    /// triggers, marks fire mark triggers and settle funding.
    fn feed(&mut self, symbol: &str, want: &[(Exchange, Market)], books: &HashMap<(Exchange, Market), Book>, msgs: &[Msg], now: i64) {
        for m in msgs.iter().filter(|m| want.contains(&(m.ex, m.market))) {
            let b = levels(books.get(&(m.ex, m.market)));
            match m.ev {
                Event::Trade { px, .. } if px > 0.0 => self.sim.on_trade(m.ex, m.market, symbol, px, b, now),
                Event::Mark { mark, funding, next_funding_ms, .. } if m.market == Market::Perp && mark > 0.0 => {
                    self.sim.on_mark(m.ex, symbol, mark, b, now);
                    if let (Some(h), Some(n)) = (funding, next_funding_ms) { self.observe_funding(m.ex, symbol, h, n, now); }
                }
                _ => {}
            }
        }
        self.commit();
    }

    /// A venue's funding as seen now: hourly rate and next settlement. When the next settlement
    /// moves forward past one that is due, that one is settled at the rate in force before it,
    /// over the venue's own interval (the step between the two times), on positions opened before it.
    /// ponytail: settles only while the app runs; off-screen symbols only on venues `funding_update`
    /// lists (Binance, Bybit, Hyperliquid), within its 60 s refresh
    fn observe_funding(&mut self, ex: Exchange, symbol: &str, hourly: f64, next: i64, now: i64) {
        let key = (ex, symbol.to_string());
        if let Some(&(h, at)) = self.funding.get(&key) {
            if next < at { return; }
            // no venue settles more often than hourly: a smaller step is two sources disagreeing
            if at <= now && next - at >= 1_800_000 && self.sim.pos.iter().any(|(k, p)| k.0 == ex && k.1 == symbol && p.opened < at) {
                self.sim.funding(ex, symbol, h * (next - at) as f64 / 3_600_000.0, now);
            }
        }
        self.funding.insert(key, (hourly, next));
    }

    /// The venue's 1 s tick: off-screen marks, liquidation, then positions and wallet when changed.
    fn tick(&mut self, ex: Exchange, base: &str, watched: &HashMap<String, f64>, now: i64) {
        // a symbol not on screen has no venue feed here: its mark (and its coins' value) follows the
        // watched spot last price. ponytail: spot last, a few bp off the perp mark; triggers fire on
        // it as on the mark, and resting limits off screen wait until the symbol is on screen
        let mut off: Vec<String> = self.sim.pos.keys().filter(|k| k.0 == ex).map(|k| k.1.clone())
            .chain(self.sim.tpsl.iter().filter(|t| t.ex == ex).map(|t| t.symbol.clone()))
            .chain(self.sim.orders.iter().filter(|o| o.ex == ex && o.market == Market::Perp).map(|o| o.req.symbol.clone()))
            .filter(|s| coin(s) != base).collect();
        off.sort();
        off.dedup();
        for s in off { if let Some(&p) = watched.get(coin(&s)) { self.sim.on_mark(ex, &s, p, None, now); } }
        let coins: Vec<String> = self.sim.coins.keys().filter(|k| k.0 == ex && k.1 != base).map(|k| k.1.clone()).collect();
        for c in coins { if let Some(&p) = watched.get(&c) { self.sim.lasts.insert((ex, Market::Spot, trade::symbol(&c)), p); } }
        self.sim.liquidate(now);
        self.commit();
        let (rows, bal) = (self.sim.positions(ex), self.sim.balance());
        let snap = format!("{rows:?}{bal:?}");
        if self.last.get(&ex) == Some(&snap) { return; }
        self.last.insert(ex, snap);
        let Some((_, tx)) = self.tx.get(&ex) else { return };
        for p in rows { let _ = tx.send((ex, AccEvent::Position { p, one_way: false })); }
        let _ = tx.send((ex, AccEvent::Wallet(bal)));
    }
}

/// What the venues' private streams would push for `evs` (already folded into `sim`). `ids` holds
/// the resting orders and TP/SL pushed so far: whatever left the Sim (filled, cancelled, replaced,
/// liquidated, reset) gets its done push.
fn pushes(sim: &Sim, ids: &mut HashMap<String, (Exchange, Market)>, venues: &[Exchange], evs: &[Ev]) -> Vec<(Exchange, AccEvent)> {
    let mut out = vec![];
    // both sides of (ex, symbol): a side not held is a qty-0 row (removes it), then the wallet
    let rows = |out: &mut Vec<(Exchange, AccEvent)>, ex: Exchange, symbol: &str| {
        let held = sim.positions(ex);
        for side in [Side::Buy, Side::Sell] {
            let p = held.iter().find(|p| p.symbol == symbol && p.side == side).cloned()
                .unwrap_or(Position { ex, symbol: symbol.into(), side, qty: 0.0, entry: 0.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None });
            out.push((ex, AccEvent::Position { p, one_way: false }));
        }
        out.push((ex, AccEvent::Wallet(sim.balance())));
    };
    let order = |out: &mut Vec<(Exchange, AccEvent)>, id: &str| {
        let Some(o) = sim.orders.iter().find(|o| o.id == id) else { return };
        if o.market == Market::Spot { return out.push((o.ex, AccEvent::SpotDirty)); }
        if let Some(r) = sim.open_orders(o.ex, o.market).into_iter().find(|r| r.id == id) { out.push((o.ex, AccEvent::Order(r))); }
    };
    let fill = |out: &mut Vec<(Exchange, AccEvent)>, ex: Exchange, id: &str| {
        if let Some(f) = sim.fills.iter().rev().find(|f| f.id == id) { out.push((ex, AccEvent::Fill(f.clone()))); }
    };
    for e in evs {
        match &e.kind {
            EvKind::Reset { .. } | EvKind::Config { .. } => out.extend(venues.iter().map(|&ex| (ex, AccEvent::Resync))),
            EvKind::Leverage { ex, symbol, .. } | EvKind::Funding { ex, symbol, .. } => rows(&mut out, *ex, symbol),
            EvKind::Place { o } => { ids.insert(o.id.clone(), (o.ex, o.market)); order(&mut out, &o.id); }
            EvKind::Amend { id, .. } | EvKind::Trigger { id, .. } => order(&mut out, id),
            EvKind::TpSl { t } => { ids.insert(t.id.clone(), (t.ex, Market::Perp)); out.push((t.ex, AccEvent::TpSl(t.clone()))); }
            EvKind::Fill { ex, market: Market::Spot, .. } => { out.push((*ex, AccEvent::SpotDirty)); out.push((*ex, AccEvent::Wallet(sim.balance()))); }
            EvKind::Fill { ex, symbol, .. } => { fill(&mut out, *ex, &e.id); rows(&mut out, *ex, symbol); }
            EvKind::Liquidate { ex, symbol, pos, qty, px, .. } => {
                out.push((*ex, AccEvent::Liquidated { symbol: symbol.clone(), side: *pos, qty: *qty, price: *px }));
                fill(&mut out, *ex, &e.id);
                rows(&mut out, *ex, symbol);
            }
            EvKind::Cancel { .. } => {}
        }
    }
    let gone: Vec<String> = ids.keys().filter(|id| !sim.orders.iter().any(|o| &o.id == *id) && !sim.tpsl.iter().any(|t| &t.id == *id)).cloned().collect();
    for id in gone {
        let Some((ex, m)) = ids.remove(&id) else { continue };
        if m == Market::Spot { out.push((ex, AccEvent::SpotDirty)); continue; }
        // an id is an order or a TP/SL: the push for the other kind changes nothing
        out.push((ex, AccEvent::OrderDone { id: id.clone() }));
        out.push((ex, AccEvent::TpSlDone { id }));
    }
    out
}

// ---------------------------------------------------------------- engine hooks

/// The engine's market data (called on every `Engine::start`).
pub fn set_market(agg: Shared<Agg>, base: &str, watched: Shared<HashMap<String, f64>>) {
    *MARKET.lock().unwrap() = Some((agg, base.to_uppercase(), watched));
}

/// The user's fees (percent per venue, as Settings stores them).
pub fn set_fees(pct: &BTreeMap<String, (f64, f64)>) {
    let f: BTreeMap<Exchange, (f64, f64)> = pct.iter().filter_map(|(k, (t, m))| Some((Exchange::parse(k)?, (t / 100.0, m / 100.0)))).collect();
    match LIVE.get() {
        Some(l) => l.lock().unwrap_or_else(|e| e.into_inner()).sim.fees = f.into_iter().collect(),
        None => *FEES.lock().unwrap() = f,
    }
}

/// One pump batch (after the Agg applied it): the Sim's venues on the base on screen.
pub fn on_batch(msgs: &[Msg]) {
    let Some((agg, base)) = MARKET.lock().unwrap().as_ref().map(|(a, b, _)| (a.clone(), b.clone())) else { return };
    let symbol = trade::symbol(&base);
    let want = live().watching(&symbol);
    if want.is_empty() { return; }
    let books: HashMap<(Exchange, Market), Book> = { let a = agg.lock().unwrap(); want.iter().filter_map(|k| Some((*k, copy(&a, *k)?))).collect() };
    live().feed(&symbol, &want, &books, msgs, now_ms());
}

/// Funding of every listed perp (venue, symbol, hourly rate, next settlement ms), from the engine's
/// 60 s ticker pass.
pub fn funding_update(rows: &[(Exchange, String, f64, i64)]) {
    let mut l = live();
    let now = now_ms();
    for (e, s, h, n) in rows { l.observe_funding(*e, s, *h, *n, now); }
    l.commit();
}

/// Bases with demo positions, orders, TP/SL or coins (kept priced by the engine's watch feed).
pub fn bases() -> Vec<String> {
    let l = live();
    let s = &l.sim;
    let mut v: Vec<String> = s.pos.keys().map(|k| coin(&k.1).to_string()).chain(s.orders.iter().map(|o| coin(&o.req.symbol).to_string()))
        .chain(s.tpsl.iter().map(|t| coin(&t.symbol).to_string())).chain(s.coins.keys().map(|k| k.1.clone())).collect();
    v.sort();
    v.dedup();
    v
}

/// Start over with `balance` USDT (recorded: the log keeps what came before).
pub fn reset(balance: f64) {
    let mut l = live();
    l.sim.reset(balance, now_ms());
    l.commit();
}

struct Reg(Exchange, u64);

impl Drop for Reg {
    fn drop(&mut self) {
        let mut l = live();
        if l.tx.get(&self.0).is_some_and(|(n, _)| *n == self.1) { l.tx.remove(&self.0); }
    }
}

/// The demo venue's private stream: Resync (the engine snapshots through the demo readers), then
/// pushes as the Sim records events, and a 1 s tick until the task is dropped.
pub async fn stream(ex: Exchange, tx: AccTx) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let reg = Reg(ex, N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    { let mut l = live(); l.tx.insert(ex, (reg.1, tx.clone())); l.last.remove(&ex); }
    let _ = tx.send((ex, AccEvent::Resync));
    let mut t = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        t.tick().await;
        let (base, watched) = match MARKET.lock().unwrap().as_ref() { Some((_, b, w)) => (b.clone(), w.lock().unwrap().clone()), None => Default::default() };
        live().tick(ex, &base, &watched, now_ms());
    }
}

// ---------------------------------------------------------------- the venue contract (trade / tpsl / spot)

/// A new order on `ex`'s Perp (or Spot: `req.pos` is the side), priced off its own live book.
pub fn place(ex: Exchange, market: Market, req: &OrderReq, r: &Rules, ref_px: f64) -> Result<String> {
    let b = book(ex, market, &req.symbol);
    let mut l = live();
    let id = l.sim.place(ex, market, req, r, ref_px, levels(b.as_ref()), now_ms());
    l.commit();
    id
}

/// Cancel an order or a TP/SL by id.
pub fn cancel(id: &str) -> Result<()> {
    let mut l = live();
    let r = l.sim.cancel(id, now_ms());
    l.commit();
    r
}

/// Re-price a resting perp limit on the `tick` grid.
pub fn amend(o: &OpenOrder, price: f64, tick: f64) -> Result<()> {
    let b = book(o.ex, Market::Perp, &o.symbol);
    let mut l = live();
    let r = l.sim.amend(&o.id, price, tick, levels(b.as_ref()), now_ms());
    l.commit();
    r
}

pub fn positions(ex: Exchange) -> Vec<Position> { live().sim.positions(ex) }
pub fn open_orders(ex: Exchange, market: Market) -> Vec<OpenOrder> { live().sim.open_orders(ex, market) }
/// The one pool, the same on every venue.
pub fn balance() -> Balance { live().sim.balance() }
pub fn mode() -> Mode { if live().sim.hedge { Mode::Hedge } else { Mode::OneWay } }
pub fn leverage(ex: Exchange, symbol: &str) -> f64 { live().sim.lev(ex, symbol) }
pub fn max_leverage() -> f64 { MAX_LEV }

pub fn set_leverage(ex: Exchange, symbol: &str, lev: f64) -> Result<()> {
    let mut l = live();
    let r = l.sim.set_leverage(ex, symbol, lev, now_ms());
    l.commit();
    r
}

pub fn history(ex: Exchange) -> History { live().sim.history(ex) }

/// Every venue's demo fills of `symbol` since `since` (ms), oldest first (`db::fills` in demo mode).
pub fn fills(symbol: &str, since: i64) -> Vec<Fill> { live().sim.fills.iter().filter(|f| f.symbol == symbol && f.ts >= since).cloned().collect() }

pub fn fills_since(ex: Exchange, symbol: &str, since: i64) -> Vec<Fill> { fills(symbol, since).into_iter().filter(|f| f.ex == ex).collect() }

pub fn funding_paid(ex: Exchange, symbol: &str, side: Side) -> Option<FundingPaid> { live().sim.funding_paid(ex, symbol, side) }

/// A resting order by our client id, else a finished one; None when the Sim never had it.
pub fn order_by_client_id(ex: Exchange, client_id: &str) -> Option<Found> {
    let l = live();
    if let Some(o) = l.sim.open_orders(ex, Market::Perp).into_iter().find(|o| o.client_id.as_deref() == Some(client_id)) { return Some(Found::Open(o)); }
    l.sim.by_cid.get(client_id).map(|(id, filled, status)| Found::Done { id: id.clone(), filled: *filled, status: status.clone() })
}

/// The demo wallet of `ex`: the shared USDT pool and the coins bought on this venue.
pub fn wallets(ex: Exchange) -> Vec<Wallet> {
    let l = live();
    let s = &l.sim;
    let mut coins = vec![Coin { coin: "USDT".into(), qty: s.cash, free: s.available().max(0.0), usd: s.cash, product: None }];
    for ((e, c), &(q, px)) in s.coins.iter().filter(|((e, _), _)| *e == ex) {
        coins.push(Coin { coin: c.clone(), qty: q, free: q, usd: q * s.lasts.get(&(*e, Market::Spot, trade::symbol(c))).copied().unwrap_or(px), product: None });
    }
    vec![Wallet { id: "DEMO".into(), usd: coins.iter().map(|c| c.usd).sum(), coins }]
}

pub fn spot_balances(ex: Exchange) -> Vec<Bal> { live().sim.spot_balances(ex) }

/// Whole-position TP and/or SL (None leaves that side as it is), on the venue's tick grid.
pub async fn set_tpsl(ex: Exchange, symbol: &str, pos: Side, tp: Option<f64>, sl: Option<f64>, trigger: Trigger) -> Result<()> {
    if tp.is_none() && sl.is_none() { bail!("nothing to set"); }
    let r = trade::rules(ex, symbol).await?;
    let new: Vec<TpSl> = [(true, tp), (false, sl)].into_iter().filter_map(|(take_profit, px)| {
        Some(TpSl { id: trade::uuid_v4(), ex, symbol: symbol.into(), pos, take_profit, trigger_px: trade::fmt_step(px?, r.tick, false).parse().ok()?, qty: None, trigger })
    }).collect();
    let mut l = live();
    let res = new.into_iter().try_for_each(|t| l.sim.set_tpsl(t, now_ms()));
    l.commit();
    res
}

/// Remove the whole-position TP and/or SL; partial levels stay.
pub fn clear_tpsl(ex: Exchange, symbol: &str, pos: Side, tp: bool, sl: bool) -> Result<()> {
    let mut l = live();
    let ids: Vec<String> = l.sim.tpsl.iter().filter(|t| t.ex == ex && t.symbol == symbol && t.pos == pos && t.qty.is_none() && if t.take_profit { tp } else { sl }).map(|t| t.id.clone()).collect();
    let res = ids.iter().try_for_each(|id| l.sim.cancel(id, now_ms()));
    l.commit();
    res
}

/// One staged TP or SL level of `qty` base units; returns its id.
pub async fn add_partial(ex: Exchange, symbol: &str, pos: Side, take_profit: bool, trigger_px: f64, qty: f64, trigger: Trigger) -> Result<String> {
    let r = trade::rules(ex, symbol).await?;
    let q: f64 = trade::fmt_step(qty, r.step, true).parse()?;
    if q <= 0.0 || q < r.min_qty { bail!("size {q} below minimum {}", r.min_qty); }
    let t = TpSl { id: trade::uuid_v4(), ex, symbol: symbol.into(), pos, take_profit, trigger_px: trade::fmt_step(trigger_px, r.tick, false).parse()?, qty: Some(q), trigger };
    let id = t.id.clone();
    let mut l = live();
    let res = l.sim.set_tpsl(t, now_ms());
    l.commit();
    res.map(|_| id)
}

pub fn tpsl_list(ex: Exchange) -> Vec<TpSl> { live().sim.tpsl.iter().filter(|t| t.ex == ex).cloned().collect() }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trade::{Kind, Tif};
    use Side::{Buy, Sell};

    const BY: Exchange = Exchange::Bybit;
    const R: Rules = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 0.0 };

    fn req(pos: Side, close: bool, kind: Kind, qty: f64) -> OrderReq { OrderReq { symbol: "BTCUSDT".into(), pos, close, kind, qty, client_id: None, attach: None } }
    fn msg(ex: Exchange, ev: Event) -> Msg { Msg { ex, market: Market::Perp, symbol: "BTCUSDT".into(), ts: 0, recv: 0, ev } }

    /// A local account with Bybit's stream registered, and that stream's receiver.
    fn acct() -> (Live, tokio::sync::mpsc::UnboundedReceiver<(Exchange, AccEvent)>) {
        let mut l = Live::new(load("a", &[], 0, 1), false);
        l.commit();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        l.tx.insert(BY, (0, tx));
        (l, rx)
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(Exchange, AccEvent)>) -> Vec<String> {
        let mut v = vec![];
        while let Ok((_, e)) = rx.try_recv() {
            v.push(match e {
                AccEvent::Position { p, .. } => format!("Position {:?} {}", p.side, p.qty),
                AccEvent::Order(o) => format!("Order {}", o.price),
                AccEvent::Fill(f) => format!("Fill {}@{}", f.qty, f.price),
                AccEvent::Wallet(_) => "Wallet".into(),
                AccEvent::OrderDone { .. } => "OrderDone".into(),
                AccEvent::TpSlDone { .. } => "TpSlDone".into(),
                AccEvent::TpSl(t) => format!("TpSl {}", t.trigger_px),
                e => format!("{e:?}"),
            });
        }
        v
    }

    #[test]
    fn events_become_the_pushes_of_a_venue_stream() {
        let (mut l, mut rx) = acct();
        let books: HashMap<(Exchange, Market), Book> = [((BY, Market::Perp), (vec![(99.0, 5.0)], vec![(101.0, 5.0)]))].into();
        let id = l.sim.place(BY, Market::Perp, &req(Buy, false, Kind::Limit { price: 100.0, tif: Tif::Gtc }, 1.0), &R, 100.0, levels(books.get(&(BY, Market::Perp))), 2).unwrap();
        l.commit();
        assert_eq!(drain(&mut rx), ["Order 100"]);
        // a print at the limit does nothing; one through it fills the order, which then leaves the book
        let want = l.watching("BTCUSDT");
        assert_eq!(want, [(BY, Market::Perp)]);
        l.feed("BTCUSDT", &want, &books, &[msg(BY, Event::Trade { px: 100.0, qty: 1.0, side: Sell }), msg(Exchange::Okx, Event::Trade { px: 90.0, qty: 1.0, side: Sell })], 3);
        assert!(drain(&mut rx).is_empty());
        l.feed("BTCUSDT", &want, &books, &[msg(BY, Event::Trade { px: 99.9, qty: 1.0, side: Sell })], 4);
        assert_eq!(drain(&mut rx), ["Fill 1@100", "Position Buy 1", "Position Sell 0", "Wallet", "OrderDone", "TpSlDone"]);
        assert!(!l.ids.contains_key(&id));
        // a TP replaced by a new one: the old one's done push; a cancel: the order's
        let t = |id: &str, px| TpSl { id: id.into(), ex: BY, symbol: "BTCUSDT".into(), pos: Buy, take_profit: true, trigger_px: px, qty: None, trigger: Trigger::Last };
        l.sim.set_tpsl(t("t1", 120.0), 5).unwrap();
        l.sim.set_tpsl(t("t2", 130.0), 5).unwrap();
        l.commit();
        assert_eq!(drain(&mut rx), ["TpSl 120", "TpSl 130", "OrderDone", "TpSlDone"]);
        let o = l.sim.place(BY, Market::Perp, &req(Buy, true, Kind::Limit { price: 150.0, tif: Tif::Gtc }, 1.0), &R, 100.0, None, 6).unwrap();
        l.sim.cancel(&o, 7).unwrap();
        l.commit();
        assert_eq!(drain(&mut rx), ["OrderDone", "TpSlDone"]);
        // a market close: fills, position gone, its TP with it
        l.sim.place(BY, Market::Perp, &req(Buy, true, Kind::Market, 1.0), &R, 100.0, levels(books.get(&(BY, Market::Perp))), 8).unwrap();
        l.commit();
        assert_eq!(drain(&mut rx), ["Fill 1@99", "Position Buy 0", "Position Sell 0", "Wallet", "OrderDone", "TpSlDone", "OrderDone", "TpSlDone"]);
        // a reset resyncs every registered venue
        l.sim.reset(5_000.0, 9);
        l.commit();
        assert_eq!(drain(&mut rx), ["Resync"]);
    }

    #[test]
    fn funding_settles_once_per_settlement_on_older_positions() {
        let (mut l, mut rx) = acct();
        let h = 3_600_000;
        l.sim.record(1, super::super::tests::fill(BY, "BTCUSDT", Buy, Buy, 100.0, 1.0, 0.0));
        l.sim.marks.insert((BY, "BTCUSDT".into()), 100.0);
        // hourly 0.0000125 until the 8 h settlement at t = 8h, then the next one listed for 16h
        l.observe_funding(BY, "BTCUSDT", 0.0000125, 8 * h, 10);
        l.observe_funding(BY, "BTCUSDT", 0.0000125, 8 * h, 8 * h - 1);
        l.observe_funding(BY, "BTCUSDT", 0.00002, 16 * h, 8 * h + 5);
        // the same jump seen again (the other source) settles nothing more
        l.observe_funding(BY, "BTCUSDT", 0.00002, 16 * h, 8 * h + 9);
        l.commit();
        assert!((l.sim.cash - (10_000.0 - 0.01)).abs() < 1e-9, "{}", l.sim.cash);
        assert!(drain(&mut rx).contains(&"Position Buy 1".to_string()));
        // a position opened after the settlement time pays nothing for it
        let mut fresh = Live::new(load("a", &[], 0, 1), false);
        fresh.observe_funding(BY, "BTCUSDT", 0.0000125, 8 * h, 10);
        fresh.sim.record(8 * h + 1, super::super::tests::fill(BY, "BTCUSDT", Buy, Buy, 100.0, 1.0, 0.0));
        fresh.observe_funding(BY, "BTCUSDT", 0.0000125, 16 * h, 8 * h + 2);
        assert_eq!(fresh.sim.cash, 10_000.0);
    }

    #[test]
    fn tick_liquidates_off_screen_and_pushes_only_changes() {
        let (mut l, mut rx) = acct();
        l.sim.record(1, super::super::tests::fill(BY, "ETHUSDT", Buy, Buy, 100.0, 900.0, 0.0));
        l.commit();
        drain(&mut rx);
        let px: HashMap<String, f64> = [("ETH".to_string(), 100.0)].into();
        l.tick(BY, "BTC", &px, 2);
        assert_eq!(drain(&mut rx), ["Position Buy 900", "Wallet"]);
        l.tick(BY, "BTC", &px, 3);
        assert!(drain(&mut rx).is_empty());
        // ETH is not on screen: its watched price is the mark; cross equity at maintenance liquidates
        let px: HashMap<String, f64> = [("ETH".to_string(), 89.0)].into();
        l.tick(BY, "BTC", &px, 4);
        assert!(l.sim.pos.is_empty() && l.sim.cash >= 0.0);
        let p = drain(&mut rx);
        assert!(p.iter().any(|x| x.starts_with("Liquidated")) && p.contains(&"Position Buy 0".to_string()), "{p:?}");
    }

    #[test]
    fn the_stored_log_restores_the_account() {
        let mut l = Live::new(load("dev", &[], 0, 1), false);
        let mut log: Vec<DemoRow> = l.commit().iter().map(Ev::row).collect();
        assert_eq!(log[0].kind, "Reset");
        let b = (vec![(99.0, 5.0)], vec![(101.0, 5.0)]);
        l.sim.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 2.0), &R, 100.0, levels(Some(&b)), 2).unwrap();
        l.sim.place(BY, Market::Perp, &req(Sell, false, Kind::Limit { price: 110.0, tif: Tif::Gtc }, 0.5), &R, 100.0, levels(Some(&b)), 3).unwrap();
        l.sim.set_leverage(BY, "BTCUSDT", 5.0, 4).unwrap();
        log.extend(l.commit().iter().map(Ev::row));
        // restart: the same account, and the next event numbered after the stored ones
        let back = load("dev", &log, 0, 99);
        assert_eq!(super::super::tests::snap(&back), super::super::tests::snap(&l.sim));
        assert_eq!((back.seq, back.pending.len()), (l.sim.seq, 0));
        // an unreadable newer row still holds its seq
        let mut newer = log.clone();
        newer.push(DemoRow { id: "x".into(), device: "dev".into(), seq: 99, ts: 5, kind: "FromTheFuture".into(), body: "{}".into() });
        assert_eq!(load("dev", &newer, 99, 6).seq, 99);
        assert_eq!(Live::new(back, false).ids.len(), 1);
    }
}
