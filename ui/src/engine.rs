//! Background data engine: one tokio runtime per subscribed base asset, feeding a shared `Agg`.
//! Switching the base drops the whole runtime, which cancels every connector task (some
//! connectors spawn nested tasks, so aborting handles one by one would leak).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::VecDeque;
use depth::trade::{self, Balance, Keys, Mode, OpenOrder, OrderReq, Position, Rules};
use depth::{agg::Agg, ex, now_ms, Event, Exchange, History, Market, Msg, Sub};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub const HISTORY_MINUTES: usize = 1000;
const HISTORY_MARKETS: [Market; 2] = [Market::Spot, Market::Perp];
/// A keyed margin borrow rate is read again only when it is at least this old.
const BORROW_TTL_MS: i64 = 3_600_000;

/// Private account state for venues with Keychain keys, refreshed every 2s.
#[derive(Default)]
pub struct Account {
    pub keys: HashMap<Exchange, Keys>,
    /// protection buy priced for the insurance dialog: (venue, option symbol) -> plan or error
    pub opt_plan: Option<((Exchange, String), Result<trade::OptBuy, String>)>,
    /// per keyed venue: may the key trade (false = read-only); absent = unknown / not asked
    pub can_trade: HashMap<Exchange, bool>,
    pub positions: Vec<Position>,
    /// held options (bought protection), every venue
    pub opts: Vec<trade::OptPos>,
    /// spot: resting orders and wallet coins per venue (trade::spot::VENUES)
    pub spot_orders: Vec<trade::OpenOrder>,
    pub spot_bal: HashMap<Exchange, Vec<trade::spot::Bal>>,
    /// margin (Margin mode): Binance cross-margin orders of `margin_sym` (Bybit's come with spot_orders),
    /// margin wallet coins with loans per venue
    pub margin_orders: Vec<trade::OpenOrder>,
    pub margin_bal: HashMap<Exchange, Vec<trade::spot::Bal>>,
    pub margin_sym: Option<String>,
    /// resting option orders (option venues), refreshed on resync and after our own option orders
    pub opt_orders: Vec<trade::OpenOrder>,
    pub orders: Vec<OpenOrder>,
    pub balances: HashMap<Exchange, Balance>,
    pub rules: HashMap<(Exchange, String), Rules>,
    /// position mode per (venue, symbol), read from the venue (None while being fetched)
    pub modes: HashMap<(Exchange, String), Option<Mode>>,
    /// leverage setting per (venue, symbol): read once, then kept current by position pushes
    pub levs: HashMap<(Exchange, String), f64>,
    /// last failed mode/leverage lookup: retried after 30s, never every frame
    lookup_failed: HashMap<(Exchange, String), i64>,
    pub errors: HashMap<Exchange, String>,
    /// order round trip (submit -> exchange ack), smoothed ms
    pub order_rtt: HashMap<Exchange, f64>,
    /// (local ms, message, ok), newest last
    pub log: VecDeque<(i64, String, bool)>,
    /// every account per venue with notes (missing permissions) and fetch time; loaded on demand
    pub wallets: HashMap<Exchange, (Vec<trade::Wallet>, Vec<String>, i64)>,
    pub wallets_loading: std::collections::HashSet<Exchange>,
    /// active TP/SL (whole-position and partial) across venues, pushed by the private streams
    pub tpsl: Vec<trade::tpsl::TpSl>,
    /// funding paid / received since open per open position (venue, symbol, side); see `funding_due`
    pub funding_paid: HashMap<(Exchange, String, depth::Side), FundingSlot>,
    /// client-side execution jobs (TWAP), newest last
    pub algos: Vec<AlgoJob>,
    /// venue maximum leverage per (venue, symbol), read when the leverage sheet opens
    pub max_levs: HashMap<(Exchange, String), f64>,
    /// order / trade / closed-PnL history per venue with fetch time; loaded on demand
    pub history: HashMap<Exchange, (trade::History, i64)>,
    pub history_loading: std::collections::HashSet<Exchange>,
    /// bumped when own fills were stored (the chart re-reads them from the db); (venue, symbol)
    /// pairs already back-filled over REST this session
    pub fills_ver: u64,
    /// live fills (fill sound + toast) and the newest one: pushed fills on Binance / Bybit, a
    /// pushed position size change on the other venues
    pub fill_seq: u64,
    pub last_fill: Option<FillNote>,
    /// when each venue's private stream last (re)subscribed: position pushes right after are its
    /// snapshot, not fills
    pub resync_ms: HashMap<Exchange, i64>,
    pub fills_done: std::collections::HashSet<(Exchange, String)>,
    pub transferring: bool,
    /// auto top-up rules per venue (persisted in settings.json) and the last time one fired
    pub auto: HashMap<Exchange, AutoTopUp>,
    auto_last: HashMap<Exchange, i64>,
    /// pre-trade risk limits (Settings > Trading), copied from the prefs
    pub limits: depth::risk::Limits,
    /// each keyed venue's equity at the start of the local day (day number in the display time
    /// zone): the daily loss stop's baseline, persisted in settings.json
    pub day_start: (i64, HashMap<Exchange, f64>),
    /// TP/SL waiting for their entry order to fill (persisted in settings.json)
    pub tpsl_wait: Vec<TpSlWait>,
    /// bumped by the kill switch: scaled ladders and sequential orders started before it stop
    pub kill_gen: u64,
    /// the last kill switch run, for its report
    pub kill: Option<KillReport>,
    /// the last routed orders as sent (newest last, at most 10): each leg's state for the order panel
    pub plans: VecDeque<PlanRun>,
    /// orders whose request got no readable answer and that no push has shown yet: (venue, client id)
    pub unknown: Vec<(Exchange, String)>,
}

/// What became of one order request: the venue's id, refused (or never sent), or unknown: no
/// answer could be read, so it may be live. Unknown orders are looked up by client id, never re-sent.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent { Ok(String), Failed, Unknown }

/// One routed order as sent: its legs by client id, for the panel (and "retry remainder").
#[derive(Clone, Debug, PartialEq)]
pub struct PlanRun { pub ts: i64, pub legs: Vec<LegRun> }
#[derive(Clone, Debug, PartialEq)]
pub struct LegRun { pub ex: Exchange, pub client_id: String, pub qty: f64, pub state: LegState }
#[derive(Clone, Debug, PartialEq)]
pub enum LegState { Sent, Ok(String), Live(String), Filled, Failed(String), Unknown }

impl LegState {
    /// (state word for the panel, detail)
    pub fn label(&self) -> (&'static str, String) {
        match self {
            LegState::Sent => ("sent", String::new()), LegState::Ok(id) => ("ok", id.clone()), LegState::Live(id) => ("live", id.clone()),
            LegState::Filled => ("filled", String::new()), LegState::Failed(e) => ("failed", e.clone()), LegState::Unknown => ("unknown", String::new()),
        }
    }
}

impl Account {
    /// A routed order about to be sent: every leg "sent" until its venue answers.
    pub(crate) fn record_plan(&mut self, legs: &[depth::route::Leg]) {
        let legs = legs.iter().filter_map(|l| Some(LegRun { ex: l.ex, client_id: l.req.client_id.clone()?, qty: l.req.qty, state: LegState::Sent })).collect();
        self.plans.push_back(PlanRun { ts: now_ms(), legs });
        if self.plans.len() > 10 { self.plans.pop_front(); }
    }

    fn set_leg(&mut self, cid: &str, s: &LegState) {
        for l in self.plans.iter_mut().flat_map(|p| p.legs.iter_mut()).filter(|l| l.client_id == cid) { l.state = s.clone(); }
    }
}

/// The part of a routed order to route again: what failed, once no leg is still sent or unknown.
pub(crate) fn remainder(p: &PlanRun) -> Option<f64> {
    if p.legs.iter().any(|l| matches!(l.state, LegState::Sent | LegState::Unknown)) { return None; }
    let q: f64 = p.legs.iter().filter(|l| matches!(l.state, LegState::Failed(_))).map(|l| l.qty).sum();
    (q > 0.0).then_some(q)
}

/// A pushed order carrying the client id of an order not known to be live (no answer read, or
/// looked up and not found): the venue has it after all.
fn found_by_push(a: &mut Account, ex: Exchange, ev: &trade::AccEvent) {
    let trade::AccEvent::Order(o) = ev else { return };
    let Some(cid) = o.client_id.as_deref() else { return };
    let was = a.unknown.iter().position(|u| u.0 == ex && u.1 == cid).map(|i| a.unknown.remove(i)).is_some();
    let mut late = false;
    for l in a.plans.iter_mut().flat_map(|p| p.legs.iter_mut()).filter(|l| l.ex == ex && l.client_id == cid) {
        late |= matches!(l.state, LegState::Unknown | LegState::Failed(_));
        if !matches!(l.state, LegState::Ok(_)) { l.state = LegState::Live(o.id.clone()); }
    }
    if was || late { a.note(format!("OK {ex:?} {} order {cid} is on the venue after all (id {})", o.symbol, o.id), true); }
}

/// A client id for an order that has none yet (routed legs carry the planner's), unique in this run.
fn new_cid() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("t1m{:x}{:03x}", now_ms(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 4096)
}

/// The one lookup of an order whose request got no readable answer, 10 s later and only if no push
/// has shown it meanwhile: live, filled, ended unfilled or not placed, noted in the order log and on
/// its leg (a TP/SL waiting on it learns the same). Never re-sent.
async fn look_up(account: &Arc<Mutex<Account>>, ex: Exchange, symbol: String, cid: String) {
    tokio::time::sleep(Duration::from_secs(10)).await;
    let k = {
        let a = account.lock().unwrap();
        if !a.unknown.iter().any(|u| u.0 == ex && u.1 == cid) { return; }
        a.keys.get(&ex).cloned()
    };
    let r = match k { Some(k) => trade::order_by_client_id(ex, &k, &symbol, &cid).await, None => Err(anyhow::anyhow!("no API key")) };
    let mut a = account.lock().unwrap();
    // a push that came while asking wins
    let Some(i) = a.unknown.iter().position(|u| u.0 == ex && u.1 == cid) else { return };
    a.unknown.remove(i);
    let (state, msg) = match &r {
        Ok(Some(trade::Found::Open(o))) => (LegState::Live(o.id.clone()), format!("is live on the venue (id {})", o.id)),
        Ok(Some(trade::Found::Done { id, filled, status })) if *filled > 0.0 => (LegState::Filled, format!("filled {filled} ({status}, id {id})")),
        Ok(Some(trade::Found::Done { status, .. })) => (LegState::Failed(format!("ended {status} unfilled")), format!("ended {status} without a fill")),
        Ok(None) => (LegState::Failed("not placed (no answer)".into()), "was not placed".into()),
        Err(e) => (LegState::Unknown, format!("could not be looked up ({e:#}): check the venue for it")),
    };
    let ok = matches!(state, LegState::Live(_) | LegState::Filled);
    a.note(format!("{} {ex:?} {symbol} order {cid} {msg}", if ok { "OK" } else { "NOTE" }), ok);
    a.set_leg(&cid, &state);
    // a failed lookup leaves a waiting TP/SL to the next snapshot
    if let Ok(found) = r { if let Some(w) = a.tpsl_wait.iter().find(|w| w.ex == ex && w.client_id == cid).cloned() { resolve(&mut a, &w, Ok(found), now_ms()); } }
}

/// One kill switch run: per keyed venue, every step's line (ok or failed) and whether it is done.
#[derive(Clone, Debug, Default)]
pub struct KillReport { pub started_ms: i64, pub venues: Vec<(Exchange, Vec<(String, bool)>, bool)> }

/// What the kill switch will act on, per keyed venue: (venue, orders, TP/SL, algorithms with the
/// waiting TP/SL, positions).
pub(crate) fn kill_plan(a: &Account) -> Vec<(Exchange, usize, usize, usize, usize)> {
    let mut v: Vec<_> = a.keys.keys().map(|&e| (e,
        a.orders.iter().filter(|o| o.ex == e).count(),
        a.tpsl.iter().filter(|t| t.ex == e).count(),
        a.algos.iter().filter(|j| j.ex == e && !j.cancelled && j.status != "done").count() + a.tpsl_wait.iter().filter(|w| w.ex == e).count(),
        a.positions.iter().filter(|p| p.ex == e && p.qty > 0.0).count())).collect();
    v.sort_by_key(|x| x.0 as u8);
    v
}

fn market_close(p: &Position) -> (OrderReq, f64) {
    (OrderReq { symbol: p.symbol.clone(), pos: p.side, close: true, kind: trade::Kind::Market, qty: p.qty, client_id: None, attach: None }, if p.mark > 0.0 { p.mark } else { p.entry })
}

/// The kill switch's closes of one venue's positions (as the venue reports them): the planner's
/// slippage-capped leg where there is one for the position, sized to what the venue holds, else a
/// market close; and whether a capped leg went out as a limit (its unfilled rest: `kill_rest`).
fn kill_closes(ps: &[Position], capped: &[(OrderReq, f64)]) -> (Vec<(OrderReq, f64)>, bool) {
    let mut limited = false;
    let out = ps.iter().filter(|p| p.qty > 0.0).map(|p| match capped.iter().find(|(r, _)| r.symbol == p.symbol && r.pos == p.side) {
        Some((r, px)) => { limited |= r.kind != trade::Kind::Market; (OrderReq { qty: p.qty, ..r.clone() }, *px) }
        None => market_close(p),
    }).collect();
    (out, limited)
}

/// What is left of positions a capped limit leg was sent for: closed at market.
fn kill_rest(after: &[Position], capped: &[(OrderReq, f64)]) -> Vec<(OrderReq, f64)> {
    after.iter().filter(|p| p.qty > 0.0 && capped.iter().any(|(r, _)| r.symbol == p.symbol && r.pos == p.side && r.kind != trade::Kind::Market)).map(market_close).collect()
}

/// One line of the kill switch report (and of the order log).
fn kill_line(account: &Arc<Mutex<Account>>, ex: Exchange, msg: String, ok: bool) {
    let mut a = account.lock().unwrap();
    if let Some(v) = a.kill.as_mut().and_then(|k| k.venues.iter_mut().find(|v| v.0 == ex)) { v.1.push((msg.clone(), ok)); }
    a.note(format!("KILL {ex:?} {msg}"), ok);
}

/// The kill switch on one venue, step by step, each step reported whatever the one before did:
/// cancel its orders (trade::cancel_all), its TP/SL (tpsl::cancel_all) and its venue-run TWAPs,
/// read its positions once (the local ones if that fails), close them (`kill_closes`), then close at
/// market what a capped leg left (one more read).
async fn kill_venue(account: &Arc<Mutex<Account>>, ex: Exchange, k: &Keys, twaps: Vec<(u64, String, String)>, capped: Vec<(OrderReq, f64)>) {
    let line = |msg: String, ok: bool| kill_line(account, ex, msg, ok);
    let report = |what: &str, r: anyhow::Result<trade::CancelReport>| match r {
        Ok(r) => { line(format!("{what}: {} cancelled", r.sent), true); for f in r.failed { line(format!("{what}: {f}"), false); } }
        Err(e) => line(format!("{what}: {e:#}"), false),
    };
    let orders: Vec<OpenOrder> = account.lock().unwrap().orders.iter().filter(|o| o.ex == ex).cloned().collect();
    report("orders", trade::cancel_all(ex, k, &orders).await);
    let tps: Vec<trade::tpsl::TpSl> = account.lock().unwrap().tpsl.iter().filter(|t| t.ex == ex).cloned().collect();
    if !tps.is_empty() { report("TP/SL", trade::tpsl::cancel_all(ex, k, &tps).await); }
    for (id, symbol, vid) in twaps {
        let r = trade::cancel_twap(ex, k, &symbol, &vid).await;
        if let Some(j) = account.lock().unwrap().algos.iter_mut().find(|j| j.id == id) {
            j.status = match &r { Ok(()) => "cancelled".into(), Err(e) => format!("cancel failed: {e:#}") };
            j.end_ms = now_ms();
        }
        match r { Ok(()) => line(format!("TWAP {vid} cancelled"), true), Err(e) => line(format!("TWAP {vid}: {e:#}"), false) }
    }
    let ps = match trade::positions(ex, k).await {
        Ok(p) => p,
        Err(e) => { line(format!("positions: {e:#}; closing the positions Depth shows"), false); account.lock().unwrap().positions.iter().filter(|p| p.ex == ex).cloned().collect() }
    };
    let (closes, limited) = kill_closes(&ps, &capped);
    if closes.is_empty() { line("no position".into(), true); }
    let close = |req: OrderReq, px: f64| async move {
        let sent = place_logged(account, ex, &req, px).await;
        let side = if req.pos == depth::Side::Buy { "long" } else { "short" };
        let how = match sent { Sent::Ok(_) => "sent", Sent::Failed => "FAILED, see the order log", Sent::Unknown => "NO ANSWER, being looked up by client id (see the order log)" };
        kill_line(account, ex, format!("close {} {side} {} {}: {how}", req.symbol, req.qty, req.kind.label()), matches!(sent, Sent::Ok(_)));
    };
    for (req, px) in closes { close(req, px).await; }
    if limited {
        // an IOC is matched by the time the venue answers on most venues, not all: a moment before reading
        // what is left (a reduce-only market close never flips the position if it read too much)
        tokio::time::sleep(Duration::from_millis(500)).await;
        match trade::positions(ex, k).await {
            Ok(after) => for (req, px) in kill_rest(&after, &capped) { close(req, px).await; },
            Err(e) => line(format!("positions after the capped closes: {e:#}; check the venue"), false),
        }
    }
    if let Some(v) = account.lock().unwrap().kill.as_mut().and_then(|k| k.venues.iter_mut().find(|v| v.0 == ex)) { v.2 = true; }
}

/// A TP/SL sent with an opening order on a venue that cannot attach it to the order
/// (`Caps.attach_tpsl` false): set for the whole position once that order is known to have filled
/// and the position exists, dropped with a note when the order ended without a fill. The order is
/// matched by its client id / venue id only, never by "some order on the symbol", so it can never
/// land on an unrelated later fill. Kept across restarts and checked against the venue again.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TpSlWait {
    pub ex: Exchange, pub symbol: String, pub pos: depth::Side,
    pub tp: Option<f64>, pub sl: Option<f64>, pub trigger: trade::tpsl::Trigger,
    /// the entry order: our client id, and the venue's id once known
    pub client_id: String, pub order_id: Option<String>,
    pub created_ms: i64,
    /// when the entry order was first seen filling (local ms)
    pub filled_ms: Option<i64>,
    /// a lookup by client id is running
    #[serde(skip)] pub checking: bool,
    /// the venue's positions are loaded since it was created; `restored`: read from disk and not yet
    /// seen resting in the venue's first snapshot (both false / true until then)
    #[serde(skip)] pub armed: bool,
    #[serde(skip)] pub restored: bool,
}

impl TpSlWait {
    pub fn new(ex: Exchange, req: &OrderReq, a: trade::tpsl::Attach, now: i64) -> Option<Self> {
        Some(TpSlWait { ex, symbol: req.symbol.clone(), pos: req.pos, tp: a.tp, sl: a.sl, trigger: a.trigger, client_id: req.client_id.clone()?,
            order_id: None, created_ms: now, filled_ms: None, checking: false, armed: true, restored: false })
    }
}

/// What a waiting TP/SL asks for, run after the account lock is released.
#[derive(Debug, PartialEq)]
pub enum WaitAct { Fire(TpSlWait), Check(TpSlWait) }

/// A pushed order change for the waiting TP/SL: the entry order's venue id is learnt from its
/// client id, a fill of it marks the entry filled, and its end without a known fill asks the venue
/// (one lookup by client id: OrderDone does not say filled or cancelled).
fn wait_touch(a: &mut Account, ex: Exchange, ev: &trade::AccEvent, now: i64) -> Vec<WaitAct> {
    use trade::AccEvent as E;
    let mut out = vec![];
    for w in a.tpsl_wait.iter_mut().filter(|w| w.ex == ex) {
        match ev {
            // venue ids are unique per symbol only (Binance)
            E::Order(o) if o.symbol == w.symbol && (o.client_id.as_deref() == Some(w.client_id.as_str()) || w.order_id.as_deref() == Some(o.id.as_str())) => {
                w.order_id = Some(o.id.clone());
                if o.filled > 0.0 { w.filled_ms.get_or_insert(now); }
            }
            E::OrderDone { id } if w.order_id.as_deref() == Some(id.as_str()) && w.filled_ms.is_none() && !w.checking => {
                w.checking = true;
                out.push(WaitAct::Check(w.clone()));
            }
            _ => {}
        }
    }
    out
}

/// Filled entries fire once their position exists (or are dropped when it does not appear within
/// 30 s: closed again before its TP/SL was set); entries unfilled after a day are dropped.
fn settle(a: &mut Account, now: i64) -> Vec<WaitAct> {
    let (mut out, mut notes) = (vec![], vec![]);
    let positions = &a.positions;
    a.tpsl_wait.retain(|w| {
        if !w.armed { return true; }
        let held = positions.iter().any(|p| p.ex == w.ex && p.symbol == w.symbol && p.side == w.pos && p.qty > 0.0);
        match w.filled_ms {
            Some(_) if held => { out.push(WaitAct::Fire(w.clone())); false }
            Some(t) if now - t > 30_000 => { notes.push(format!("NOTE {:?} {}: TP/SL not set, the position its order {} opened is gone", w.ex, w.symbol, w.client_id)); false }
            None if now - w.created_ms > 86_400_000 => { notes.push(format!("NOTE {:?} {}: TP/SL dropped, its order {} has not filled in a day", w.ex, w.symbol, w.client_id)); false }
            _ => true,
        }
    });
    for n in notes { a.note(n, false); }
    out
}

/// After a snapshot of `ex` (connect, reconnect, the 5-minute reconcile): an entry read from disk is
/// re-armed while its order still rests, else dropped with a notice (it may have filled while Depth was
/// closed, but the position now there may be a later, unrelated one); an entry whose order is not
/// resting and whose fill is not known is checked with the venue once (pushes missed while
/// disconnected). A fresh entry still being placed waits.
fn rearm(a: &mut Account, ex: Exchange, now: i64) -> Vec<WaitAct> {
    let (mut out, mut notes) = (vec![], vec![]);
    let orders = &a.orders;
    a.tpsl_wait.retain_mut(|w| {
        if w.ex != ex { return true; }
        let resting = orders.iter().find(|o| o.ex == ex && o.symbol == w.symbol && (w.order_id.as_deref() == Some(o.id.as_str()) || o.client_id.as_deref() == Some(w.client_id.as_str())));
        if let Some(o) = resting {
            (w.armed, w.restored) = (true, false);
            w.order_id = Some(o.id.clone());
            if o.filled > 0.0 { w.filled_ms.get_or_insert(now); }
        } else if w.restored {
            notes.push(format!("NOTE {ex:?} {}: TP/SL dropped after the restart, its order {} no longer rests (it may have filled while Depth was closed): set it from the positions table", w.symbol, w.client_id));
            return false;
        } else if w.filled_ms.is_none() && !w.checking && (w.order_id.is_some() || now - w.created_ms > 15_000) {
            w.checking = true;
            out.push(WaitAct::Check(w.clone()));
        }
        true
    });
    for n in notes { a.note(n, false); }
    out.extend(settle(a, now));
    out
}

/// The venue's answer for a checked entry: still open (filled or not), finished with a fill, or
/// finished / unknown without one (dropped with a note; a failed lookup too: never a stale TP/SL).
fn resolve(a: &mut Account, w: &TpSlWait, r: anyhow::Result<Option<trade::Found>>, now: i64) {
    let Some(i) = a.tpsl_wait.iter().position(|x| x.ex == w.ex && x.client_id == w.client_id) else { return };
    let x = &mut a.tpsl_wait[i];
    x.checking = false;
    let gone = match r {
        Ok(Some(trade::Found::Open(o))) => { x.order_id = Some(o.id); if o.filled > 0.0 { x.filled_ms.get_or_insert(now); } None }
        Ok(Some(trade::Found::Done { filled, .. })) if filled > 0.0 => { x.filled_ms.get_or_insert(now); None }
        Ok(Some(trade::Found::Done { status, .. })) => Some(format!("its order {} ended {status} without a fill", w.client_id)),
        Ok(None) => Some(format!("its order {} is not on the venue", w.client_id)),
        Err(e) => Some(format!("could not confirm a fill of its order {} ({e:#}); set it from the positions table if it filled", w.client_id)),
    };
    if let Some(why) = gone {
        a.tpsl_wait.remove(i);
        a.note(format!("NOTE {:?} {}: TP/SL dropped, {why}", w.ex, w.symbol), false);
    }
}

/// Runs what the waiting TP/SL asked for: set a TP/SL (the venue's whole-position one), or look an
/// entry order up by client id (once) and act on the answer.
fn run_wait(account: &Arc<Mutex<Account>>, acts: Vec<WaitAct>, ctx: &eframe::egui::Context) {
    for act in acts {
        let (account, ctx) = (account.clone(), ctx.clone());
        tokio::spawn(async move {
            match act {
                WaitAct::Fire(w) => set_tpsl_logged(&account, w.ex, w.symbol, w.pos, w.tp, w.sl, None, w.trigger).await,
                WaitAct::Check(w) => {
                    let k = account.lock().unwrap().keys.get(&w.ex).cloned();
                    let r = match k { Some(k) => trade::order_by_client_id(w.ex, &k, &w.symbol, &w.client_id).await, None => Err(anyhow::anyhow!("no API key")) };
                    let acts = { let mut a = account.lock().unwrap(); resolve(&mut a, &w, r, now_ms()); settle(&mut a, now_ms()) };
                    run_wait(&account, acts, &ctx);
                }
            }
            ctx.request_repaint();
        });
    }
}

/// A REST snapshot of `ex`, then the waiting TP/SL checked against it.
async fn snapshot(account: &Arc<Mutex<Account>>, ex: Exchange, ctx: &eframe::egui::Context) {
    refresh_venue(account, ex).await;
    let acts = rearm(&mut account.lock().unwrap(), ex, now_ms());
    run_wait(account, acts, ctx);
}

/// Keep the margin account's available balance topped up: when it drops below `min` USD, move
/// USDT from the venue's other accounts (Bybit FUND, EARN; Binance SPOT, FUNDING, EARN) up to
/// `target`. At most once a minute per venue; every transfer lands in the order log.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AutoTopUp { pub enabled: bool, pub min: f64, pub target: f64 }

impl Account {
    pub(crate) fn note(&mut self, msg: String, ok: bool) {
        if !ok { eprintln!("[trade] {msg}"); }
        self.log.push_back((now_ms(), msg, ok));
        if self.log.len() > 200 { self.log.pop_front(); }
    }
}

/// A fill for the toast: venue, symbol, side, size, price when the venue said it.
#[derive(Clone, Debug)]
pub struct FillNote { pub ex: Exchange, pub symbol: String, pub buy: bool, pub qty: f64, pub price: Option<f64> }

/// One USDT perpetual for the symbol picker.
#[derive(Clone, Debug)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Ticker { pub base: String, pub last: f64, pub chg_pct: f64, pub quote_vol: f64,
    /// 24h high / low from the first venue listing it; base volume summed across venues
    pub high: f64, pub low: f64, pub base_vol: f64 }

/// A client-side execution job (TWAP) as shown in the Algos tab.
#[derive(Clone, Debug)]
pub struct AlgoJob {
    pub id: u64, pub ex: Exchange, pub symbol: String, pub buy: bool, pub close: bool,
    pub total: f64, pub sent: f64, pub slices: usize, pub done: usize,
    pub started_ms: i64, pub end_ms: i64, pub status: String, pub cancelled: bool,
    /// run by the venue (Binance / Hyperliquid TWAP): its id there, to cancel it
    pub venue_id: Option<String>,
}

fn set_status(account: &Arc<Mutex<Account>>, id: u64, s: &str) {
    if let Some(j) = account.lock().unwrap().algos.iter_mut().find(|j| j.id == id) { if j.status != s { j.status = s.into(); } }
}

/// Place one order with the account's keys, cached rules and position mode; logs the outcome in
/// the order log and tracks the round trip. Returns the venue's order id.
/// Asks the venue once whether the key may trade (read-only keys disable the order buttons).
fn check_perms(rt: &tokio::runtime::Runtime, account: &Arc<Mutex<Account>>, ex: Exchange, k: Keys) {
    let account = account.clone();
    rt.spawn(async move {
        match trade::can_trade(ex, &k).await {
            Ok(Some(ok)) => { account.lock().unwrap().can_trade.insert(ex, ok); }
            Ok(None) => {}
            Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} key permissions: {e:#}\n")),
        }
    });
}

/// The day's PnL over the keyed venues: equity now minus each venue's equity at the start of the
/// local day (`off`: UTC offset of the display time zone, ms). A venue seen first today, or first
/// after midnight, takes its equity now as the baseline. None before any balance.
/// ponytail: equity-based, so funding, fees and transfers made outside Depth count as PnL
pub(crate) fn day_pnl(a: &mut Account, now: i64, off: i64) -> Option<f64> {
    let day = (now + off).div_euclid(86_400_000);
    if a.day_start.0 != day { a.day_start = (day, HashMap::new()); }
    let (keys, base) = (&a.keys, &mut a.day_start.1);
    a.balances.iter().filter(|(e, _)| keys.contains_key(e)).fold(None, |s, (e, b)| Some(s.unwrap_or(0.0) + b.equity - *base.entry(*e).or_insert(b.equity)))
}

/// A transfer Depth made into or out of a venue's margin account moves that venue's day baseline
/// with it, so it counts as neither profit nor loss.
/// ponytail: stablecoins only (amount = USD); a coin moved in as collateral still shows as PnL
fn shift_day(a: &mut Account, ex: Exchange, from: &str, to: &str, coin: &str, amount: f64) {
    if !matches!(coin, "USDT" | "USDC" | "USD") { return; }
    let m = |id: &str| if trade::MARGIN_IDS.contains(&id) { 1.0 } else { 0.0 };
    if let Some(b) = a.day_start.1.get_mut(&ex) { *b += amount * (m(to) - m(from)); }
}

/// The risk check's view of an order on `ex`: the venue's own price (`ref_px`, the touch or mid the
/// order was priced from, else the mark of a position held there; never the composite index),
/// everything held in the symbol on every venue at its mark, both sides, and the day's PnL; with the
/// part of a one-way order that closes the opposite position first.
/// ponytail: held positions and this guard count USD / USDC 1:1 with USDT (a few bp off); the route
/// preview (`check_plan`) values each leg at the live rate
pub(crate) fn risk_ctx(a: &mut Account, ex: Exchange, req: &OrderReq, ref_px: f64, now: i64, off: i64) -> (depth::risk::Ctx, f64) {
    let sym = |p: &&Position| p.symbol == req.symbol;
    let mark = Some(ref_px).filter(|x| *x > 0.0).or_else(|| a.positions.iter().filter(sym).find(|p| p.ex == ex && p.mark > 0.0).map(|p| p.mark));
    let held_usdt = a.positions.iter().filter(sym).map(|p| p.qty * p.mark).sum();
    let one_way = a.modes.get(&(ex, req.symbol.clone())).copied().flatten() == Some(Mode::OneWay);
    let reduces = if one_way && !req.close { a.positions.iter().filter(sym).filter(|p| p.ex == ex && p.side != req.pos).map(|p| p.qty).sum() } else { 0.0 };
    (depth::risk::Ctx { mark, fx: 1.0, held_usdt, day_pnl: day_pnl(a, now, off) }, reduces)
}

/// The engine's risk guard, which every perp order passes (`place_logged`, and the algos before they
/// start): Err with the reason when the order breaks a limit; it is then never sent.
fn risk_gate(a: &mut Account, ex: Exchange, req: &OrderReq, ref_px: f64) -> Result<(), String> {
    let lim = a.limits;
    let (c, r) = risk_ctx(a, ex, req, ref_px, now_ms(), crate::local_offset_ms());
    depth::risk::check(req, r, &c, &lim)
}

/// Risk limits for a routed order before it is shown or sent: the price band per leg against its
/// venue's own touch, the order and position limits for the legs together (one order, however split).
pub(crate) fn check_plan(a: &mut Account, legs: &[depth::route::Leg]) -> Result<(), String> {
    let (lim, now, off) = (a.limits, now_ms(), crate::local_offset_ms());
    let band = depth::risk::Limits { band_bps: lim.band_bps, ..Default::default() };
    let (mut usdt, mut qty, mut red) = (0.0, 0.0, 0.0);
    for l in legs {
        let (c, r) = risk_ctx(a, l.ex, &l.req, l.ref_px, now, off);
        depth::risk::check(&l.req, r, &depth::risk::Ctx { fx: l.fx, ..c }, &band).map_err(|e| format!("{:?}: {e}", l.ex))?;
        (usdt, qty, red) = (usdt + l.est_px * l.fx * l.req.qty, qty + l.req.qty, red + r.min(l.req.qty));
    }
    let Some(l0) = legs.first().filter(|_| qty > 0.0) else { return Ok(()) };
    let (c, _) = risk_ctx(a, l0.ex, &l0.req, 0.0, now, off);
    let whole = OrderReq { qty, kind: trade::Kind::Market, ..l0.req.clone() };
    depth::risk::check(&whole, red, &depth::risk::Ctx { mark: Some(usdt / qty), fx: 1.0, ..c }, &depth::risk::Limits { band_bps: 0.0, ..lim })
}

/// The venue's own perp mid for `symbol` while it is the base on screen (`base`) and its book is
/// fresh; 0 otherwise.
fn venue_mid(agg: &Arc<Mutex<Agg>>, base: &str, ex: Exchange, symbol: &str) -> f64 {
    if symbol != trade::symbol(base) { return 0.0; }
    let a = agg.lock().unwrap();
    a.venues.get(&(ex, Market::Perp)).filter(|v| now_ms() - v.fresh < 5_000).and_then(|v| v.mid()).unwrap_or(0.0)
}

/// Place one order with the account's keys, cached rules and position mode; every order carries a
/// client id (so an unanswered one can be found), the outcome lands in the order log and on its
/// routed leg, and the round trip is tracked.
async fn place_logged(account: &Arc<Mutex<Account>>, ex: Exchange, req: &OrderReq, ref_px: f64) -> Sent {
    let mut req = req.clone();
    let cid = req.client_id.get_or_insert_with(new_cid).clone();
    let req = &req;
    let what = format!("{ex:?} {} {} {} {} {}", req.symbol, if req.close { "close" } else { "open" }, if req.pos == depth::Side::Buy { "long" } else { "short" }, req.qty, req.kind.label());
    let fail = |msg: String| { let mut a = account.lock().unwrap(); a.note(format!("FAIL {what}: {msg}"), false); a.set_leg(&cid, &LegState::Failed(msg)); Sent::Failed };
    // never sent past a risk limit (a separate statement: the guard's lock ends before the note's)
    let gate = risk_gate(&mut account.lock().unwrap(), ex, req, ref_px);
    if let Err(e) = gate { return fail(format!("risk limit: {e}")); }
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return fail("no API key".into()) };
    let key = (ex, req.symbol.clone());
    let cached = account.lock().unwrap().rules.get(&key).copied();
    let rules = match cached { Some(r) => Ok(r), None => trade::rules(ex, &req.symbol).await };
    let cached_mode = account.lock().unwrap().modes.get(&key).copied().flatten();
    let mode = match cached_mode { Some(m) => Ok(m), None => trade::mode(ex, &k, &req.symbol).await };
    // a failed rules / mode read sent nothing: a plain failure, whatever kind of error it was
    let (r, m) = match (rules, mode) { (Ok(r), Ok(m)) => (r, m), (Err(e), _) | (_, Err(e)) => return fail(format!("{e:#}")) };
    {
        let mut a = account.lock().unwrap();
        a.rules.insert(key.clone(), r);
        a.modes.insert(key, Some(m));
    }
    let t0 = std::time::Instant::now();
    let res = trade::place(ex, &k, req, &r, ref_px, m).await;
    let rtt = t0.elapsed().as_secs_f64() * 1e3;
    match res {
        Ok(id) => {
            let mut a = account.lock().unwrap();
            let r = a.order_rtt.entry(ex).or_insert(rtt);
            *r += (rtt - *r) * 0.3;
            a.note(format!("OK {what} id {id} ({rtt:.0} ms)"), true);
            a.set_leg(&cid, &LegState::Ok(id.clone()));
            Sent::Ok(id)
        }
        Err(e) if trade::is_unknown_outcome(&e) => {
            {
                let mut a = account.lock().unwrap();
                a.note(format!("UNKNOWN {what}: no answer read ({e:#}); it may be live: looking it up by client id {cid} in 10 s, it is never sent again"), false);
                a.set_leg(&cid, &LegState::Unknown);
                a.unknown.push((ex, cid.clone()));
            }
            let (account, symbol) = (account.clone(), req.symbol.clone());
            tokio::spawn(async move { look_up(&account, ex, symbol, cid).await });
            Sent::Unknown
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// Set / replace whole-position TP and/or SL, or add one partial level; the result lands in the order log.
#[allow(clippy::too_many_arguments)]
async fn set_tpsl_logged(account: &Arc<Mutex<Account>>, ex: Exchange, symbol: String, pos: depth::Side, tp: Option<f64>, sl: Option<f64>, partial: Option<f64>, trigger: trade::tpsl::Trigger) {
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let cached = account.lock().unwrap().modes.get(&(ex, symbol.clone())).copied().flatten();
    let mode = match cached { Some(m) => Ok(m), None => trade::mode(ex, &k, &symbol).await };
    let res = match mode {
        Err(e) => Err(e),
        Ok(m) => match partial {
            None => trade::tpsl::set_position_tpsl(ex, &k, &symbol, pos, m, tp, sl, trigger).await.map(|_| String::new()),
            Some(q) => {
                let (take, px) = match (tp, sl) { (Some(p), _) => (true, p), (_, Some(p)) => (false, p), _ => (true, 0.0) };
                trade::tpsl::add_partial(ex, &k, &symbol, pos, m, take, px, q, trigger).await
            }
        },
    };
    let what = format!("{ex:?} {symbol} {} TP {} SL {}{}", if pos == depth::Side::Buy { "long" } else { "short" },
        tp.map(|x| x.to_string()).unwrap_or("-".into()), sl.map(|x| x.to_string()).unwrap_or("-".into()), partial.map(|q| format!(" size {q}")).unwrap_or_default());
    let mut a = account.lock().unwrap();
    match res { Ok(_) => a.note(format!("OK TP/SL {what}"), true), Err(e) => a.note(format!("FAIL TP/SL {what}: {e:#}"), false) }
}

/// Funding-since-open of one position: the last result (None until fetched, or on venues that do not
/// report it), the size it was fetched at, when it was fetched and when it is due again (local ms).
#[derive(Clone, Debug)]
pub struct FundingSlot { pub val: Option<trade::FundingPaid>, qty: f64, at: i64, due: i64 }

/// Positions whose funding-since-open is due: never fetched, size changed (at most once per 60 s), or
/// ~20 s after a settlement. Marks them fetched now and drops the slots of closed positions.
/// Each comes with the open time an earlier fetch found for it (kept while the position lives).
fn funding_due(a: &mut Account, fund: &HashMap<(Exchange, String), VenueFunding>, now: i64) -> Vec<(Keys, Position, Option<i64>)> {
    let live: std::collections::HashSet<_> = a.positions.iter().map(|p| (p.ex, p.symbol.clone(), p.side)).collect();
    a.funding_paid.retain(|k, _| live.contains(k));
    let mut out = vec![];
    for p in a.positions.iter().filter(|p| p.qty > 0.0) {
        let Some(k) = a.keys.get(&p.ex) else { continue };
        let key = (p.ex, p.symbol.clone(), p.side);
        let old = a.funding_paid.get(&key);
        if old.is_some_and(|s| now < s.due && (s.qty == p.qty || now - s.at < 60_000)) { continue; }
        let slot = FundingSlot { val: old.and_then(|s| s.val), qty: p.qty, at: now, due: next_settle(fund.get(&(p.ex, p.symbol.clone())), now) + 20_000 };
        let known = slot.val.as_ref().filter(|v| !v.lower && v.since > 0).map(|v| v.since);
        a.funding_paid.insert(key, slot);
        out.push((k.clone(), p.clone(), known));
    }
    out
}

/// Next funding settlement after `now`: the listed time rolled forward by the interval when stale;
/// venues without a listed rate assume the common 00/08/16 UTC schedule.
fn next_settle(f: Option<&VenueFunding>, now: i64) -> i64 {
    let (next, iv) = f.map_or((0, 8 * 3_600_000), |f| (f.next_ms, ((f.interval_h * 3_600_000.0) as i64).max(3_600_000)));
    if next > now { next } else { now + iv - (now - next).rem_euclid(iv) }
}

/// Funding of one venue's perp: rate per settlement, settlement interval (hours), next settlement (ms).
#[derive(Clone, Copy, Debug, Default)]
pub struct VenueFunding { pub rate: f64, pub interval_h: f64, pub next_ms: i64 }

impl Stats {
    /// Median push latency over the last 200 trades/BBOs (robust to stale replays and outliers).
    pub fn latency(&self, ex: Exchange) -> Option<f64> {
        let q = self.lat_samples.get(&ex)?;
        if q.len() < 5 { return None; }
        let mut v: Vec<f64> = q.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        Some(v[v.len() / 2])
    }
}

#[derive(Default)]
pub struct Stats {
    pub total: u64,
    /// (messages, last message local ms) per venue
    pub venue: HashMap<(Exchange, Market), (u64, i64)>,
    pub hist_pending: usize,
    pub hist_total: usize,
    pub hist_errors: Vec<String>,
    pub usdt: Option<f64>,
    /// venues still loading per chart timeframe (minutes)
    pub tf_pending: HashMap<u32, usize>,
    /// messages waiting in the pump queue (should stay near 0)
    pub backlog: usize,
    /// last exchange-timestamp -> local receive delays per venue, ms (trades and BBOs only:
    /// polled REST series carry no push latency); read with `latency()`
    pub lat_samples: HashMap<Exchange, VecDeque<f64>>,
    /// resident memory of this process, MB (sampled every 5s)
    pub rss_mb: Option<u64>,
}

pub struct Engine {
    pub agg: Arc<Mutex<Agg>>,
    pub stats: Arc<Mutex<Stats>>,
    pub base: String,
    rt: Option<tokio::runtime::Runtime>,
    tx: Option<UnboundedSender<Msg>>,
    started: Vec<Market>,
    tfs: Vec<u32>,
    /// live connector tasks per venue, so one venue can be switched off or on alone
    venue_tasks: HashMap<Exchange, Vec<tokio::task::JoinHandle<()>>>,
    /// symbol universe for the picker, by 24h quote volume (kept across base switches)
    pub tickers: Arc<Mutex<Vec<Ticker>>>,
    /// (venue, "BTCUSDT") -> funding, for every listed perp (positions in symbols not on screen)
    pub funding: Arc<Mutex<HashMap<(Exchange, String), VenueFunding>>>,
    /// alternative.me crypto Fear & Greed (0 fear .. 100 greed, daily) and its label
    pub fng: Arc<Mutex<Option<(f64, String)>>>,
    /// detected signals for the current base (quant::detect every 5 s, per-kind cooldown)
    pub signals: Arc<Mutex<depth::quant::Feed>>,
    /// private trading state (kept across base switches)
    pub account: Arc<Mutex<Account>>,
    /// set by the insurance panel: option chains are needed
    pub wants_options: std::sync::atomic::AtomicBool,
    /// set by the L3 view: start the exact order feeds (Lighter's order snapshots are REST polls,
    /// so they run only once someone looks)
    pub wants_l3: std::sync::atomic::AtomicBool,
    l3_started: bool,
    /// Hyperliquid's L3 feed in use (public l2Book, or Flowscan when `l3_flowscan`)
    hl_l3: Option<(bool, tokio::task::JoinHandle<()>)>,
    pub l3_flowscan: bool,
    /// venues switched off in the status bar: never connected (applied on `start`)
    pub off: std::collections::HashSet<Exchange>,
    /// private account streams live on their own runtime so base switches don't drop them
    acct_rt: Option<tokio::runtime::Runtime>,
    /// private-stream sender and running stream per venue (replaced when keys change)
    acct_tx: Option<tokio::sync::mpsc::UnboundedSender<(Exchange, trade::AccEvent)>>,
    streams: HashMap<Exchange, tokio::task::JoinHandle<()>>,
    /// result of the last "test connection" per venue: (ok, message)
    pub key_tests: Arc<Mutex<HashMap<Exchange, (bool, String)>>>,
    /// price alerts on symbols not on screen: one venue's spot stream per base, on its own runtime
    watch_rt: Option<tokio::runtime::Runtime>,
    watching: HashMap<String, Vec<tokio::task::JoinHandle<()>>>,
    /// last trade price per watched base (USDT)
    pub watched: Arc<Mutex<HashMap<String, f64>>>,
    /// Binance margin borrow APR from the user's key: asset -> (APR, read at ms); kept across base switches
    borrow: Arc<Mutex<HashMap<String, (f64, i64)>>>,
}

impl Engine {
    pub fn new() -> Self {
        Engine { agg: Default::default(), stats: Default::default(), base: String::new(), rt: None, tx: None, started: vec![], tfs: vec![], venue_tasks: HashMap::new(), tickers: Default::default(), funding: Default::default(), fng: Default::default(), signals: Default::default(), account: {
            let mut a = Account::default();
            for ex in trade::TRADABLE { if let Some(k) = trade::keychain(ex) { a.keys.insert(ex, k); } }
            Arc::new(Mutex::new(a))
        }, wants_options: Default::default(), wants_l3: Default::default(), l3_started: false, hl_l3: None, l3_flowscan: false, off: Default::default(), acct_rt: None, acct_tx: None, streams: HashMap::new(), key_tests: Default::default(), watch_rt: None, watching: HashMap::new(), watched: Default::default(), borrow: Default::default() }
    }

    /// (Re)subscribe everything for `base`: spot + perp live and history, USDT/USD, heatmap sampling.
    pub fn start(&mut self, base: &str, ctx: &eframe::egui::Context) {
        if let Some(rt) = self.rt.take() { rt.shutdown_background(); }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
        self.base = base.to_uppercase();
        self.agg = Arc::new(Mutex::new(Agg::default()));
        self.agg.lock().unwrap().load_opt_hist(&self.base);
        self.stats = Arc::new(Mutex::new(Stats { hist_pending: self.venues().len() * HISTORY_MARKETS.len(), hist_total: self.venues().len() * HISTORY_MARKETS.len(), ..Default::default() }));
        self.started = vec![];
        self.tfs = vec![];
        self.venue_tasks.clear();
        self.l3_started = false;
        self.hl_l3 = None;
        let _guard = rt.enter();

        // pump: apply messages in batches under one lock
        let (tx, mut rx) = unbounded_channel::<Msg>();
        let (agg, stats) = (self.agg.clone(), self.stats.clone());
        let diag = std::env::var("T1_DIAG").is_ok();
        rt.spawn(async move {
            let mut buf = Vec::with_capacity(4096);
            let mut last_diag = now_ms();
            let mut lag_detail: HashMap<(Exchange, Market, &'static str), (f64, u64)> = HashMap::new();
            while rx.recv_many(&mut buf, 4096).await > 0 {
                {
                    let mut a = agg.lock().unwrap();
                    for m in &buf {
                        // margin streams duplicate spot books; only their borrow rates are new
                        if m.market == Market::Margin && !matches!(m.ev, Event::BorrowRate { .. }) { continue; }
                        a.on(m);
                    }
                }
                let now = now_ms();
                let mut s = stats.lock().unwrap();
                s.total += buf.len() as u64;
                s.backlog = rx.len();
                if diag && now - last_diag >= 5_000 {
                    eprintln!("[diag] total {} backlog {}", s.total, s.backlog);
                    for ((e, mk, k), (sum, n)) in &lag_detail { if *n > 0 && sum / *n as f64 > 1000.0 { eprintln!("[lag] {e:?} {mk:?} {k} avg {:.0}ms over {n}", sum / *n as f64); } }
                    lag_detail.clear();
                    last_diag = now;
                }
                for m in &buf {
                    let e = s.venue.entry((m.ex, m.market)).or_default();
                    e.0 += 1;
                    e.1 = now;
                    if diag && m.ts != m.recv { let d = lag_detail.entry((m.ex, m.market, m.ev.kind())).or_default(); d.0 += (m.recv - m.ts) as f64; d.1 += 1; }
                    if matches!(m.ev, Event::Trade { .. } | Event::Bbo { .. }) && m.ts != m.recv && m.market != Market::Option {
                        let q = s.lat_samples.entry(m.ex).or_default();
                        q.push_back((m.recv - m.ts).max(0) as f64);
                        if q.len() > 200 { q.pop_front(); }
                    }
                }
                buf.clear();
            }
        });

        // history first (queued live updates replay after each venue loads)
        {
            let mut a = self.agg.lock().unwrap();
            for ex in self.venues() { for m in HISTORY_MARKETS { a.expect_history(ex, m); } }
        }
        for ex in self.venues() {
            for market in HISTORY_MARKETS {
                let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
                rt.spawn(async move {
                    let h = cached_history(ex, &sub, 1, &stats).await;
                    let mut a = agg.lock().unwrap();
                    a.load_history(ex, market, &h);
                    let mut s = stats.lock().unwrap();
                    s.hist_pending -= 1;
                    if s.hist_pending == 0 { a.finish_history(); }
                    ctx.request_repaint();
                });
            }
        }

        for market in HISTORY_MARKETS { self.spawn_market(market, &tx); }

        // USDT/USD for cross-venue USD views, from Kraken and Coinbase books
        let (fx_tx, mut fx_rx) = unbounded_channel::<Msg>();
        for e in [Exchange::Kraken, Exchange::Coinbase] {
            ex::spawn(e, &Sub { market: Market::Spot, base: "USDT".into(), quote: "USD".into() }, fx_tx.clone());
        }
        let (agg, stats) = (self.agg.clone(), self.stats.clone());
        rt.spawn(async move {
            let mut mids: HashMap<Exchange, f64> = HashMap::new();
            while let Some(m) = fx_rx.recv().await {
                if let Event::Bbo { bid, ask, .. } = m.ev {
                    if bid <= 0.0 || ask <= 0.0 { continue; }
                    mids.insert(m.ex, (bid + ask) / 2.0);
                    let mut v: Vec<f64> = mids.values().copied().collect();
                    v.sort_by(f64::total_cmp);
                    let r = v[v.len() / 2];
                    agg.lock().unwrap().set_fx("USDT", r);
                    stats.lock().unwrap().usdt = Some(r);
                }
            }
        });

        let stats_m = self.stats.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let mb = tokio::task::spawn_blocking(depth::sys::rss_mb).await.ok().flatten();
                stats_m.lock().unwrap().rss_mb = mb;
            }
        });

        // signals: one detection pass every 5 s over complete 1m bars of the aggregated series
        self.signals = Arc::new(Mutex::new(Default::default()));
        let (agg_s, feed, base_s) = (self.agg.clone(), self.signals.clone(), self.base.clone());
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            // algo detections already reported (key -> last seen): each algo once while it runs, again after 30 min quiet
            let mut algos_seen: HashMap<String, i64> = HashMap::new();
            loop {
                tick.tick().await;
                let now = depth::now_ms();
                let algos = {
                    let mut a = agg_s.lock().unwrap();
                    let vols: Vec<f64> = a.series.get(&(None, Market::Perp)).map(|s| s.bars.iter().rev().take(120).map(|b| b.vol * b.c).collect()).unwrap_or_default();
                    let vol_1m = depth::quant::robust(&vols).map_or(0.0, |x| x.0);
                    let found = depth::algoscan::significant(&a.scan, now, vol_1m, |e, m| a.usdt(e, m),
                        |e, m, bid, px| a.venues.get(&(e, m)).is_some_and(|v| v.book.qty_at(bid, px) > 0.0));
                    let s = depth::algoscan::signals(&found, now, vol_1m, |e, m| a.usdt(e, m));
                    a.scan.remember(now, found);
                    s
                };
                algos_seen.retain(|_, t| now - *t < 30 * 60_000);
                let algos: Vec<_> = algos.into_iter().filter(|(k, _)| algos_seen.insert(k.clone(), now).is_none()).map(|(_, s)| s).collect();
                let found = {
                    let a = agg_s.lock().unwrap();
                    let bars = |m: Market| a.series.get(&(None, m)).map(|s| s.bars.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
                    let (perp, spot) = (bars(Market::Perp), bars(Market::Spot));
                    let trades: Vec<(i64, f64, bool)> = a.trades.iter().filter(|t| t.market == Market::Perp)
                        .map(|t| (t.ts, t.px * t.qty * a.usdt(t.ex, t.market), t.side == depth::Side::Buy)).collect();
                    let index = a.mid(Market::Perp);
                    let dislocation: Vec<(String, f64)> = a.venues.iter().filter(|((_, m), v)| *m == Market::Perp && a.live(v, *m))
                        .filter_map(|((e, m), v)| {
                            // measured against the venue's own normal premium (USD vs USDT perps sit a few bp apart
                            // all day), and only when that move is far outside its usual noise
                            let now_bp = (v.mid()? * a.usdt(*e, *m) / index? - 1.0) * 1e4;
                            let hist: Vec<f64> = a.premium_bps(*e, *m).iter().rev().take(60).map(|x| x.1).collect();
                            let (med, sd) = depth::quant::robust(&hist)?;
                            ((now_bp - med).abs() >= 5.0 * sd.max(1.0)).then(|| (format!("{e:?}"), now_bp - med))
                        }).collect();
                    let input = depth::quant::Input {
                        perp: depth::quant::complete(&perp, now), spot: depth::quant::complete(&spot, now),
                        trades: &trades, dislocation: &dislocation, base: &base_s,
                    };
                    depth::quant::detect(&input, now)
                };
                let mut f = feed.lock().unwrap();
                for sg in found { f.offer(sg, 600_000); }
                // already deduplicated per algo above
                for sg in algos { f.offer(sg, 0); }
            }
        });

        let tickers = self.tickers.clone();
        let funding = self.funding.clone();
        // the last list from disk first (the picker is usable at once), then refreshed every minute
        let cache = depth::sys::cache_dir().map(|d| d.join("tickers.json"));
        if let Some(t) = cache.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Vec<Ticker>>(&b).ok()) {
            let mut g = self.tickers.lock().unwrap();
            if g.is_empty() { *g = t; }
        }
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let (t, f) = all_tickers().await;
                if !t.is_empty() {
                    if let (Some(p), Ok(j)) = (&cache, serde_json::to_vec(&t)) { let _ = std::fs::write(p, j); }
                    *tickers.lock().unwrap() = t;
                }
                if !f.is_empty() { funding.lock().unwrap().extend(f); }
            }
        });

        // market-wide daily Fear & Greed, the absolute anchor next to the intraday panic index
        let fng = self.fng.clone();
        rt.spawn(depth::ws::poll("fear & greed".into(), Duration::from_secs(3600), move || {
            let fng = fng.clone();
            async move {
                let v = depth::ws::get_json("https://api.alternative.me/fng/?limit=1").await?;
                let d = &v["data"][0];
                if let Some(x) = d["value"].as_str().and_then(|x| x.parse().ok()) {
                    *fng.lock().unwrap() = Some((x, d["value_classification"].as_str().unwrap_or_default().to_string()));
                }
                Ok(())
            }
        }));

        // live last / 24h change between REST refreshes: Binance futures leads `all_tickers`, so its
        // all-market mini ticker (changed symbols, 1/s, no weight) updates the same rows in place
        // ponytail: always on (~a few KB/s deflated); gate on the picker being open if bandwidth matters
        let tickers = self.tickers.clone();
        rt.spawn(depth::ws::run(depth::ws::Spec::new("binance tickers", "wss://fstream.binance.com/market/ws/!miniTicker@arr"), move |f| {
            let depth::ws::Frame::Text(s) = f else { return };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { return };
            let num = depth::num;
            let upd: HashMap<&str, &serde_json::Value> = v.as_array().into_iter().flatten()
                .filter_map(|x| Some((x["s"].as_str()?.strip_suffix("USDT")?, x))).collect();
            for t in tickers.lock().unwrap().iter_mut() {
                let Some(x) = upd.get(t.base.as_str()) else { continue };
                let (c, o) = (num(&x["c"]), num(&x["o"]));
                if c > 0.0 && o > 0.0 { t.last = c; t.chg_pct = (c / o - 1.0) * 100.0; t.high = num(&x["h"]); t.low = num(&x["l"]); }
            }
        }));

        let agg = self.agg.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop { tick.tick().await; agg.lock().unwrap().sample_heatmap(now_ms()); }
        });

        self.tx = Some(tx);
        self.rt = Some(rt);
    }

    /// Keeps a live price for each base in `bases` (alerts on symbols not on screen): the first
    /// enabled of Binance / Bybit / OKX spot, trades only. Spot, because perp connectors also poll
    /// open interest over REST every few seconds per symbol.
    /// ponytail: spot last price (a few bp from the perp / composite), and a perp-only coin gets no
    /// price; a venue-native mini-ticker subscription would cover both.
    pub fn watch(&mut self, bases: &[String]) {
        self.watching.retain(|b, hs| {
            let keep = bases.contains(b);
            if !keep { for h in hs.iter() { h.abort(); } }
            keep
        });
        self.watched.lock().unwrap().retain(|b, _| bases.contains(b));
        let Some(ex) = [Exchange::Binance, Exchange::Bybit, Exchange::Okx].into_iter().find(|e| !self.off.contains(e)) else { return };
        let new: Vec<String> = bases.iter().filter(|b| !self.watching.contains_key(*b)).cloned().collect();
        for b in &new {
            let rt = self.watch_rt.get_or_insert_with(|| tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("watch runtime"));
            let _g = rt.enter();
            let (tx, mut rx) = unbounded_channel::<Msg>();
            let mut hs = ex::spawn(ex, &Sub { market: Market::Spot, base: b.clone(), quote: "USDT".into() }, tx);
            let (px, base) = (self.watched.clone(), b.clone());
            hs.push(rt.spawn(async move {
                while let Some(m) = rx.recv().await {
                    if let Event::Trade { px: p, .. } = m.ev { if p > 0.0 { px.lock().unwrap().insert(base.clone(), p); } }
                }
            }));
            self.watching.insert(b.clone(), hs);
        }
    }

    pub fn venues(&self) -> Vec<Exchange> { Exchange::ALL.into_iter().filter(|e| !self.off.contains(e)).collect() }

    /// Private account state for venues with keys: pushed over each venue's private stream.
    /// REST takes a snapshot whenever a stream (re)connects, refetches balances (at most every 3s)
    /// when a push says they changed without totals, and reconciles everything every 5 minutes.
    pub fn start_account(&mut self, ctx: &eframe::egui::Context) {
        let keys: Vec<(Exchange, Keys)> = self.account.lock().unwrap().keys.iter().map(|(e, k)| (*e, k.clone())).collect();
        if self.acct_rt.is_some() { return; }
        // the runtime exists even without keys: keys can be added from Settings at any time
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("account runtime");
        let (tx, mut rx) = unbounded_channel::<(Exchange, trade::AccEvent)>();
        for (ex, k) in &keys {
            check_perms(&rt, &self.account, *ex, k.clone());
            self.streams.insert(*ex, rt.spawn(trade::stream(*ex, k.clone(), tx.clone())));
        }
        self.acct_tx = Some(tx.clone());
        let (account, funding, ctx) = (self.account.clone(), self.funding.clone(), ctx.clone());
        rt.spawn(async move {
            let mut buf = Vec::with_capacity(1024);
            let mut dirty: std::collections::HashSet<Exchange> = Default::default();
            // spot orders / balances changed (Bybit order.spot push): one REST refetch per 3s at most
            let mut spot_dirty: std::collections::HashSet<Exchange> = Default::default();
            let mut last_spot: HashMap<Exchange, std::time::Instant> = HashMap::new();
            // Binance position pushes carry no liquidation price: positions are refetched after a push,
            // at most every 3s, so the venue's own figure replaces the old one
            let mut pos_dirty: std::collections::HashSet<Exchange> = Default::default();
            let mut last_pos: HashMap<Exchange, std::time::Instant> = HashMap::new();
            let mut last_bal: HashMap<Exchange, std::time::Instant> = HashMap::new();
            let mut bal_tick = tokio::time::interval(Duration::from_secs(1));
            let mut reconcile = tokio::time::interval(Duration::from_secs(300));
            reconcile.tick().await; // the first snapshot comes from each stream's Resync
            loop {
                tokio::select! {
                    n = rx.recv_many(&mut buf, 1024) => {
                        if n == 0 { break; }
                        let mut changed = false;
                        let mut resync = vec![];
                        let mut top_ups = vec![];
                        let mut waits = vec![];
                        {
                            let mut a = account.lock().unwrap();
                            for (ex, ev) in buf.drain(..) {
                                match ev {
                                    trade::AccEvent::Resync => { a.resync_ms.insert(ex, now_ms()); resync.push(ex); }
                                    trade::AccEvent::SpotDirty => { spot_dirty.insert(ex); }
                                    trade::AccEvent::BalanceDirty => {
                                        dirty.insert(ex);
                                        // equity moved (fill, funding, transfer): cross liquidation prices with it
                                        if ex == Exchange::Binance { pos_dirty.insert(ex); }
                                    }
                                    ev => {
                                        if let trade::AccEvent::Position { p, .. } = &ev { if wants_liq_refetch(ex, p) { pos_dirty.insert(ex); } }
                                        let wallet = matches!(ev, trade::AccEvent::Wallet(_));
                                        waits.extend(wait_touch(&mut a, ex, &ev, now_ms()));
                                        found_by_push(&mut a, ex, &ev);
                                        changed |= apply(&mut a, ex, ev);
                                        // run after the lock is released: spawn_top_up locks the account itself
                                        if wallet { if let Some(need) = auto_due(&mut a, ex) { top_ups.push((ex, need)); } }
                                    }
                                }
                            }
                            waits.extend(settle(&mut a, now_ms()));
                        }
                        run_wait(&account, waits, &ctx);
                        for ex in resync { let (acc, ctx) = (account.clone(), ctx.clone()); tokio::spawn(async move { snapshot(&acc, ex, &ctx).await }); }
                        for (ex, need) in top_ups { spawn_top_up(&account, ex, need, &ctx); }
                        if changed { ctx.request_repaint(); }
                    }
                    _ = bal_tick.tick() => {
                        let waits = settle(&mut account.lock().unwrap(), now_ms());
                        run_wait(&account, waits, &ctx);
                        // funding since open: in-memory check each tick, a REST fetch only when `funding_due` says so
                        let due = { let mut a = account.lock().unwrap(); let f = funding.lock().unwrap(); funding_due(&mut a, &f, now_ms()) };
                        for (k, p, known) in due {
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move {
                                let r = trade::funding_paid(p.ex, &k, &p, known).await;
                                if let Err(e) = &r { depth::sys::append_log("trade.log", &format!("{:?} {} funding paid: {e:#}\n", p.ex, p.symbol)); }
                                let mut a = acc.lock().unwrap();
                                let Some(s) = a.funding_paid.get_mut(&(p.ex, p.symbol.clone(), p.side)) else { return };
                                match r { Ok(v) => s.val = v, Err(_) => s.due = s.due.min(now_ms() + 300_000) }
                                ctx.request_repaint();
                            });
                        }
                        let due: Vec<Exchange> = pos_dirty.iter().copied().filter(|e| last_pos.get(e).is_none_or(|t| t.elapsed() >= Duration::from_secs(3))).collect();
                        for ex in due {
                            pos_dirty.remove(&ex);
                            last_pos.insert(ex, std::time::Instant::now());
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move {
                                let Some(k) = acc.lock().unwrap().keys.get(&ex).cloned() else { return };
                                if let Ok(p) = trade::positions(ex, &k).await {
                                    replace_positions(&mut acc.lock().unwrap(), ex, p);
                                    ctx.request_repaint();
                                }
                            });
                        }
                        let due: Vec<Exchange> = spot_dirty.iter().copied().filter(|e| last_spot.get(e).is_none_or(|t| t.elapsed() >= Duration::from_secs(3))).collect();
                        for ex in due {
                            spot_dirty.remove(&ex);
                            last_spot.insert(ex, std::time::Instant::now());
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move { refresh_spot(&acc, ex).await; ctx.request_repaint(); });
                        }
                        let due: Vec<Exchange> = dirty.iter().copied().filter(|e| last_bal.get(e).is_none_or(|t| t.elapsed() >= Duration::from_secs(3))).collect();
                        for ex in due {
                            dirty.remove(&ex);
                            last_bal.insert(ex, std::time::Instant::now());
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move {
                                let Some(k) = acc.lock().unwrap().keys.get(&ex).cloned() else { return };
                                if let Ok(b) = trade::balance(ex, &k).await {
                                    let need = { let mut a = acc.lock().unwrap(); a.balances.insert(ex, b); auto_due(&mut a, ex) };
                                    if let Some(need) = need { spawn_top_up(&acc, ex, need, &ctx); }
                                    ctx.request_repaint();
                                }
                            });
                        }
                    }
                    _ = reconcile.tick() => {
                        let venues: Vec<Exchange> = account.lock().unwrap().keys.keys().copied().collect();
                        for ex in venues { let (acc, ctx) = (account.clone(), ctx.clone()); tokio::spawn(async move { snapshot(&acc, ex, &ctx).await }); }
                    }
                }
            }
        });
        self.acct_rt = Some(rt);
    }

    fn sub(&self, market: Market) -> Sub { Sub { market, base: self.base.clone(), quote: "USDT".into() } }

    fn spawn_market(&mut self, market: Market, tx: &UnboundedSender<Msg>) {
        if self.started.contains(&market) { return; }
        self.started.push(market);
        for e in self.venues() {
            let hs = ex::spawn(e, &self.sub(market), tx.clone());
            self.venue_tasks.entry(e).or_default().extend(hs);
        }
        if market == Market::Margin { self.spawn_borrow(tx); }
    }

    /// Binance cross-margin borrow APR of the base and USDT at the user's own VIP tier (the public
    /// margin data has none). Runs when Margin starts (first use, base change, Binance switched on):
    /// the cache outlives base switches, so the sapi call (weight 1 per coin) only goes out for a
    /// rate older than an hour, and the cached one is replayed into the fresh Agg. No timer.
    fn spawn_borrow(&self, tx: &UnboundedSender<Msg>) {
        let Some(k) = self.account.lock().unwrap().keys.get(&Exchange::Binance).cloned() else { return };
        if self.off.contains(&Exchange::Binance) { return; }
        let (cache, tx, base) = (self.borrow.clone(), tx.clone(), self.base.clone());
        tokio::spawn(async move {
            let assets = [base.as_str(), "USDT"];
            let stale = stale_borrow(&cache.lock().unwrap(), &assets, now_ms());
            if !stale.is_empty() {
                let list: Vec<&str> = stale.iter().map(String::as_str).collect();
                match trade::spot::borrow_rates(Exchange::Binance, &k, &list).await {
                    Ok(r) => { let now = now_ms(); let mut c = cache.lock().unwrap(); for (a, apr) in r { c.insert(a, (apr, now)); } }
                    Err(e) => depth::sys::append_log("trade.log", &format!("Binance margin borrow rates: {e:#}\n")),
                }
            }
            let rows: Vec<(String, f64)> = { let c = cache.lock().unwrap(); assets.iter().filter_map(|a| c.get(*a).map(|(apr, _)| (a.to_string(), *apr))).collect() };
            let now = now_ms();
            for (asset, apr) in rows {
                let _ = tx.send(Msg { ex: Exchange::Binance, market: Market::Margin, symbol: format!("{base}USDT").into(), ts: now, recv: now, ev: Event::BorrowRate { asset, apr } });
            }
        });
    }

    /// Exact L3 feeds for the current base (Lighter perp), once.
    pub fn ensure_l3(&mut self) {
        let (Some(rt), Some(tx)) = (self.rt.as_ref(), self.tx.clone()) else { return };
        let _guard = rt.enter();
        let sub = self.sub(Market::Perp);
        // Lighter exact (REST order snapshots)
        if !self.l3_started && !self.off.contains(&Exchange::Lighter) {
            self.l3_started = true;
            if let Some(s) = depth::ex::venue_sub(Exchange::Lighter, &sub) {
                let h = depth::ex::lighter::spawn_l3(&s, tx.clone());
                self.venue_tasks.entry(Exchange::Lighter).or_default().push(h);
            }
        }
        // Hyperliquid inferred: Flowscan's fast book when chosen in Settings, else the public l2Book;
        // a changed choice replaces the running feed
        if self.hl_l3.as_ref().is_some_and(|(f, _)| *f != self.l3_flowscan) { if let Some((_, h)) = self.hl_l3.take() { h.abort(); } }
        if let (true, false, Some(s)) = (self.hl_l3.is_none(), self.off.contains(&Exchange::Hyperliquid), depth::ex::venue_sub(Exchange::Hyperliquid, &sub)) {
            let h = if self.l3_flowscan { depth::ex::hyperliquid::spawn_l3_flowscan(&s, tx) } else { depth::ex::hyperliquid::spawn_l3(&s, tx) };
            self.hl_l3 = Some((self.l3_flowscan, h));
        }
    }

    /// Switch one venue off (its connections close, its data leaves the cross-venue views) or on
    /// (it connects and loads its own history); every other venue keeps its connections.
    pub fn set_venue(&mut self, e: Exchange, on: bool, ctx: &eframe::egui::Context) {
        if on == !self.off.contains(&e) { return; }
        if !on {
            self.off.insert(e);
            for h in self.venue_tasks.remove(&e).unwrap_or_default() { h.abort(); }
            self.agg.lock().unwrap().drop_venue(e);
            if e == Exchange::Lighter { self.l3_started = false; }
            if e == Exchange::Hyperliquid { if let Some((_, h)) = self.hl_l3.take() { h.abort(); } }
            return;
        }
        self.off.remove(&e);
        let (Some(rt), Some(tx)) = (self.rt.as_ref(), self.tx.clone()) else { return };
        let _guard = rt.enter();
        {
            let mut a = self.agg.lock().unwrap();
            for m in HISTORY_MARKETS { a.expect_history(e, m); }
        }
        for market in HISTORY_MARKETS {
            let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
            rt.spawn(async move {
                let h = cached_history(e, &sub, 1, &stats).await;
                let mut a = agg.lock().unwrap();
                a.load_history(e, market, &h);
                a.finish_history();
                ctx.request_repaint();
            });
        }
        for &tf in &self.tfs {
            for market in HISTORY_MARKETS {
                let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
                rt.spawn(async move {
                    let h = cached_history(e, &sub, tf, &stats).await;
                    let mut a = agg.lock().unwrap();
                    a.load_history_tf(e, market, tf, &h);
                    a.finish_history_tf(tf);
                    ctx.request_repaint();
                });
            }
        }
        for &market in &self.started {
            let hs = ex::spawn(e, &self.sub(market), tx.clone());
            self.venue_tasks.entry(e).or_default().extend(hs);
        }
        if e == Exchange::Binance && self.started.contains(&Market::Margin) { self.spawn_borrow(&tx); }
    }

    /// Fetch native `tf`-minute history for every venue the first time a timeframe is shown.
    pub fn ensure_tf(&mut self, tf: u32, ctx: &eframe::egui::Context) {
        if tf <= 1 || self.tfs.contains(&tf) { return; }
        let Some(rt) = self.rt.as_ref() else { return };
        self.tfs.push(tf);
        self.stats.lock().unwrap().tf_pending.insert(tf, self.venues().len() * HISTORY_MARKETS.len());
        for ex in self.venues() {
            for market in HISTORY_MARKETS {
                let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
                rt.spawn(async move {
                    let h = cached_history(ex, &sub, tf, &stats).await;
                    let mut a = agg.lock().unwrap();
                    a.load_history_tf(ex, market, tf, &h);
                    let mut s = stats.lock().unwrap();
                    let p = s.tf_pending.entry(tf).or_default();
                    *p = p.saturating_sub(1);
                    if *p == 0 { a.finish_history_tf(tf); }
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Place an order (rules fetched once per symbol and cached); the result lands in the account log.
    /// On the account runtime: an order in flight survives a switch of the base.
    pub fn submit(&self, ex: Exchange, req: OrderReq, ref_px: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let sent = place_logged(&account, ex, &req, ref_px).await;
            // a TP/SL waiting on this order: its venue id, nothing to wait for, or (unknown) the lookup's answer
            let mut a = account.lock().unwrap();
            if let Some(i) = a.tpsl_wait.iter().position(|w| w.ex == ex && Some(&w.client_id) == req.client_id.as_ref()) {
                match sent {
                    Sent::Ok(id) => { a.tpsl_wait[i].order_id.get_or_insert(id); }
                    Sent::Failed => { a.tpsl_wait.remove(i); a.note(format!("NOTE {ex:?} {}: TP/SL dropped, its order was not placed", req.symbol), false); }
                    Sent::Unknown => {}
                }
            }
            ctx.request_repaint();
        });
    }

    /// The kill switch: stop every client TWAP, scaled ladder, sequential order and waiting TP/SL, then
    /// on every keyed venue at once (`kill_venue`) cancel its orders, TP/SL and venue-run TWAPs and close
    /// every position it reports: `capped` (the planner's slippage-capped closes of the symbol on
    /// screen) first and their rest at market, every other position at market. Every step is in the
    /// report and the order log, a notification says when it is over; running it again repeats it.
    pub fn kill(&self, capped: Vec<(Exchange, OrderReq, f64)>, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let now = now_ms();
        let (venues, twaps) = {
            let mut a = self.account.lock().unwrap();
            a.kill_gen += 1;
            let mut twaps = vec![];
            for j in a.algos.iter_mut().filter(|j| !j.cancelled && j.status != "done" && (j.venue_id.is_none() || j.end_ms > now)) {
                j.cancelled = true;
                j.status = "cancelling".into();
                if let Some(v) = &j.venue_id { twaps.push((j.ex, j.id, j.symbol.clone(), v.clone())); }
            }
            let waits = std::mem::take(&mut a.tpsl_wait).len();
            if waits > 0 { a.note(format!("KILL {waits} waiting TP/SL dropped"), true); }
            let venues: Vec<(Exchange, Keys)> = a.keys.iter().map(|(e, k)| (*e, k.clone())).collect();
            a.kill = Some(KillReport { started_ms: now, venues: venues.iter().map(|v| (v.0, vec![], false)).collect() });
            (venues, twaps)
        };
        let (account, ctx) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
            let mut hs = vec![];
            for (ex, k) in venues {
                let tw = twaps.iter().filter(|t| t.0 == ex).map(|t| (t.1, t.2.clone(), t.3.clone())).collect();
                let cap = capped.iter().filter(|c| c.0 == ex).map(|c| (c.1.clone(), c.2)).collect();
                let (account, ctx) = (account.clone(), ctx.clone());
                hs.push(tokio::spawn(async move { kill_venue(&account, ex, &k, tw, cap).await; ctx.request_repaint(); }));
            }
            for h in hs { let _ = h.await; }
            let (ok, bad) = account.lock().unwrap().kill.as_ref().map_or((0, 0), |r| r.venues.iter().flat_map(|v| &v.1).fold((0, 0), |(o, b), l| if l.1 { (o + 1, b) } else { (o, b + 1) }));
            depth::sys::notify(crate::l("Kill switch"), &crate::lf("%d steps done, %d failed", &[ok.to_string(), bad.to_string()]));
            ctx.request_repaint();
        });
    }

    /// Place `reqs` on `ex` one after another in one task: each only once the one before was
    /// accepted, so a refused close never leaves the open after it on its own.
    pub fn submit_seq(&self, ex: Exchange, reqs: Vec<OrderReq>, ref_px: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        let kill0 = account.lock().unwrap().kill_gen;
        rt.spawn(async move {
            let n = reqs.len();
            for (i, req) in reqs.iter().enumerate() {
                if account.lock().unwrap().kill_gen != kill0 { account.lock().unwrap().note(format!("NOTE {ex:?} {}: kill switch, {} of {n} orders not sent", req.symbol, n - i), false); break; }
                // an unanswered one stops it too: the next must not go out while it may be live or not
                if !matches!(place_logged(&account, ex, req, ref_px).await, Sent::Ok(_)) {
                    if i + 1 < n { account.lock().unwrap().note(format!("NOTE {ex:?} {}: stopped after {i} of {n} orders, the rest was not sent", req.symbol), false); }
                    break;
                }
            }
            ctx.request_repaint();
        });
    }

    /// Price a protection buy on the venue's ask side (REST book + rules), into `Account::opt_plan`.
    pub fn plan_option(&self, ex: Exchange, symbol: String, qty: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        account.lock().unwrap().opt_plan = None;
        rt.spawn(async move {
            let r = trade::option_plan(ex, &symbol, qty).await.map_err(|e| format!("{e:#}"));
            account.lock().unwrap().opt_plan = Some(((ex, symbol), r));
            ctx.request_repaint();
        });
    }

    /// Send a priced protection buy (limit IOC); the result lands in the account log.
    pub fn buy_option(&self, p: trade::OptBuy, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&p.ex).cloned() else { return };
            let res = trade::buy_option(&k, &p).await;
            let what = format!("{:?} {} buy {} limit {} IOC", p.ex, p.symbol, p.qty, p.limit);
            let mut a = account.lock().unwrap();
            match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) }
            ctx.request_repaint();
        });
    }

    /// Spot buy / sell on `ex` (limit or market, base units), on the account runtime; the result goes
    /// to the account log, then spot orders and balances are refetched (Binance has no spot push here).
    #[allow(clippy::too_many_arguments)]
    pub fn spot_submit(&self, ex: Exchange, symbol: String, side: depth::Side, kind: trade::Kind, qty: f64, ref_px: f64, margin: Option<trade::spot::Effect>, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            let key = (ex, format!("spot:{symbol}"));
            let cached = account.lock().unwrap().rules.get(&key).copied();
            let rules = match cached { Some(r) => Ok(r), None => trade::spot::rules(ex, &symbol).await };
            let res = match rules {
                Ok(r) => { account.lock().unwrap().rules.insert(key, r); trade::spot::place(ex, &k, &symbol, side, kind, qty, &r, ref_px, margin).await }
                Err(e) => Err(e),
            };
            let what = format!("{ex:?} {} {} {symbol} {qty} {}", if margin.is_some() { "margin" } else { "spot" }, if side == depth::Side::Buy { "buy" } else { "sell" }, kind.label());
            { let mut a = account.lock().unwrap(); match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) } }
            ctx.request_repaint();
            // a market order has filled by now; a limit shows as resting
            for wait in [500, 2_500] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                refresh_spot(&account, ex).await;
                ctx.request_repaint();
            }
        });
    }

    /// Fetch spot orders and balances for `ex` now (the spot panel's first look at a venue).
    pub fn spot_refresh(&self, ex: Exchange, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move { refresh_spot(&account, ex).await; ctx.request_repaint(); });
    }

    /// Cancel a spot order, or a Binance cross-margin order when `margin`.
    pub fn spot_cancel(&self, o: &trade::OpenOrder, margin: bool, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            let res = trade::spot::cancel(o.ex, &k, &o.symbol, &o.id, margin).await;
            { let mut a = account.lock().unwrap(); match res { Ok(()) => a.note(format!("OK {:?} spot cancel {} {}", o.ex, o.symbol, o.id), true), Err(e) => a.note(format!("FAIL {:?} spot cancel {} {}: {e:#}", o.ex, o.symbol, o.id), false) } }
            refresh_spot(&account, o.ex).await;
            ctx.request_repaint();
        });
    }

    /// One option limit order (rules fetched once per symbol); option orders and positions are
    /// refetched after it.
    #[allow(clippy::too_many_arguments)]
    pub fn opt_submit(&self, ex: Exchange, symbol: String, side: depth::Side, qty: f64, price: f64, tif: trade::Tif, reduce_only: bool, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            let key = (ex, format!("opt:{symbol}"));
            let cached = account.lock().unwrap().rules.get(&key).copied();
            let rules = match cached { Some(r) => Ok(r), None => trade::option_rules(ex, &symbol).await };
            let res = match rules {
                Ok(r) => { account.lock().unwrap().rules.insert(key, r); trade::option_order(&k, ex, &symbol, side, qty, price, tif, reduce_only, &r).await }
                Err(e) => Err(e),
            };
            let what = format!("{ex:?} option {} {symbol} {qty} @ {price} {tif:?}{}", if side == depth::Side::Buy { "buy" } else { "sell" }, if reduce_only { " reduce-only" } else { "" });
            { let mut a = account.lock().unwrap(); match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) } }
            ctx.request_repaint();
            for wait in [500, 2_500] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                refresh_opts(&account, ex).await;
                ctx.request_repaint();
            }
        });
    }

    pub fn opt_cancel(&self, o: &trade::OpenOrder, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            let res = trade::option_cancel(o.ex, &k, &o.symbol, &o.id).await;
            { let mut a = account.lock().unwrap(); match res { Ok(()) => a.note(format!("OK {:?} option cancel {} {}", o.ex, o.symbol, o.id), true), Err(e) => a.note(format!("FAIL {:?} option cancel {} {}: {e:#}", o.ex, o.symbol, o.id), false) } }
            refresh_opts(&account, o.ex).await;
            ctx.request_repaint();
        });
    }

    /// Fetch option orders and positions for `ex` now (the option ticket's first look at a venue).
    pub fn opt_refresh(&self, ex: Exchange, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move { refresh_opts(&account, ex).await; ctx.request_repaint(); });
    }

    /// Scaled (ladder) order: `n` limit orders from `from` to `to`, sizes tilted by `skew`.
    pub fn submit_scaled(&self, ex: Exchange, base: OrderReq, from: f64, to: f64, n: usize, skew: f64, post_only: bool, ctx: &eframe::egui::Context) -> Result<usize, String> {
        let rules = self.account.lock().unwrap().rules.get(&(ex, base.symbol.clone())).copied().ok_or("venue rules not loaded yet: try again in a second")?;
        let legs = depth::algo::scaled(base.qty, from, to, n, skew, rules.tick, rules.step);
        if legs.is_empty() { return Err("nothing to place: check size, prices and count".into()); }
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return Err("not connected".into()) };
        let count = legs.len();
        let tif = if post_only { trade::Tif::PostOnly } else { trade::Tif::Gtc };
        // the ladder as one order against the limits, and every price against the band, before any is sent
        let mid = venue_mid(&self.agg, &self.base, ex, &base.symbol);
        {
            let mut a = account.lock().unwrap();
            risk_gate(&mut a, ex, &OrderReq { kind: trade::Kind::Market, ..base.clone() }, mid)?;
            for (px, q) in &legs { risk_gate(&mut a, ex, &OrderReq { kind: trade::Kind::Limit { price: *px, tif }, qty: *q, client_id: None, ..base.clone() }, mid)?; }
        }
        let kill0 = account.lock().unwrap().kill_gen;
        rt.spawn(async move {
            // one after another: venues rate-limit bursts of new orders per second
            for (px, q) in legs {
                if account.lock().unwrap().kill_gen != kill0 { break; }
                let req = OrderReq { kind: trade::Kind::Limit { price: px, tif }, qty: q, client_id: None, ..base.clone() };
                place_logged(&account, ex, &req, if mid > 0.0 { mid } else { px }).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            ctx.request_repaint();
        });
        Ok(count)
    }

    /// TWAP: `total` in `slices` IOC children spread evenly over `minutes`, each priced off the
    /// venue's own best price with at most `max_slip_bps` slippage and never beyond `limit`. A
    /// slice whose price is out of reach, or whose venue book is stale, waits and retries. Runs on
    /// the account runtime (survives pair switches); stops on cancel or when the app quits.
    /// ponytail: children are fire-and-forget IOC; the unfilled part of a child is not re-queued
    /// (fills come back through the order pushes), and jobs do not survive a restart.
    #[allow(clippy::too_many_arguments)]
    pub fn start_twap(&self, ex: Exchange, base: OrderReq, minutes: f64, slices: usize, max_slip_bps: f64, limit: Option<f64>, ctx: &eframe::egui::Context) -> Result<u64, String> {
        if !(minutes > 0.0) || slices == 0 { return Err("duration and slices must be positive".into()); }
        let rules = self.account.lock().unwrap().rules.get(&(ex, base.symbol.clone())).copied().ok_or("venue rules not loaded yet: try again in a second")?;
        let sizes = depth::algo::twap_slices(base.qty, slices, rules.step);
        if sizes.is_empty() || sizes.iter().any(|q| *q < rules.min_qty) { return Err(format!("each slice must be at least {} (try fewer slices)", rules.min_qty)); }
        let (Some(rt), account, agg, ctx) = (self.acct_rt.as_ref(), self.account.clone(), self.agg.clone(), ctx.clone()) else { return Err("not connected".into()) };
        // the whole TWAP as one order against the limits (each slice is checked again when sent)
        risk_gate(&mut account.lock().unwrap(), ex, &base, venue_mid(&agg, &self.base, ex, &base.symbol))?;
        let id = depth::now_ms() as u64;
        let gap = Duration::from_secs_f64(minutes * 60.0 / sizes.len() as f64);
        let buy = base.side() == depth::Side::Buy;
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy, close: base.close, total: base.qty, sent: 0.0, slices: sizes.len(), done: 0,
            started_ms: depth::now_ms(), end_ms: depth::now_ms() + (minutes * 60_000.0) as i64, status: "running".into(), cancelled: false, venue_id: None,
        });
        rt.spawn(async move {
            let mut i = 0;
            while i < sizes.len() {
                if account.lock().unwrap().algos.iter().any(|j| j.id == id && j.cancelled) { break; }
                // the venue's own book; stale (pair switched away, venue down) means wait
                let bbo = {
                    let a = agg.lock().unwrap();
                    a.venues.get(&(ex, depth::Market::Perp)).filter(|v| depth::now_ms() - v.ts < 5_000)
                        .and_then(|v| match (v.book.best_bid(), v.book.best_ask()) { (Some(b), Some(k)) => Some((b.0, k.0)), _ => v.bbo.map(|[b, _, k, _]| (b, k)) })
                };
                let px = bbo.and_then(|(b, k)| depth::algo::twap_price(buy, b, k, max_slip_bps, limit));
                let Some(px) = px else {
                    set_status(&account, id, if bbo.is_none() { "waiting: no fresh book for this pair (keep it open)" } else { "waiting: price beyond your limit" });
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                };
                set_status(&account, id, "running");
                let req = OrderReq { kind: trade::Kind::Limit { price: px, tif: trade::Tif::Ioc }, qty: sizes[i], client_id: None, ..base.clone() };
                place_logged(&account, ex, &req, bbo.map_or(px, |(b, k)| (b + k) / 2.0)).await;
                {
                    let mut a = account.lock().unwrap();
                    if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.done = i + 1; j.sent += sizes[i]; }
                }
                ctx.request_repaint();
                i += 1;
                if i < sizes.len() { tokio::time::sleep(gap).await; }
            }
            let cancelled = account.lock().unwrap().algos.iter().any(|j| j.id == id && j.cancelled);
            set_status(&account, id, if cancelled { "cancelled" } else { "done" });
            ctx.request_repaint();
        });
        Ok(id)
    }

    /// Venue-run TWAP: placed once, then the venue slices it (keeps running with the app closed).
    pub fn start_native_twap(&self, ex: Exchange, base: OrderReq, minutes: u32, limit: Option<f64>, randomize: bool, ref_px: f64, ctx: &eframe::egui::Context) -> Result<u64, String> {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return Err("not connected".into()) };
        risk_gate(&mut account.lock().unwrap(), ex, &base, ref_px)?;
        let id = depth::now_ms() as u64;
        let now = depth::now_ms();
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy: base.side() == depth::Side::Buy, close: base.close, total: base.qty, sent: 0.0, slices: 0, done: 0,
            started_ms: now, end_ms: now + minutes as i64 * 60_000, status: "starting".into(), cancelled: false, venue_id: None,
        });
        rt.spawn(async move {
            let k = account.lock().unwrap().keys.get(&ex).cloned();
            let key = (ex, base.symbol.clone());
            let (rules, mode) = { let a = account.lock().unwrap(); (a.rules.get(&key).copied(), a.modes.get(&key).copied().flatten()) };
            let res = match (k, rules) {
                (Some(k), Some(r)) => {
                    let mode = match mode { Some(m) => Ok(m), None => trade::mode(ex, &k, &base.symbol).await };
                    match mode { Ok(m) => trade::place_twap(ex, &k, &base, &r, ref_px, m, minutes, limit, randomize).await, Err(e) => Err(e) }
                }
                _ => Err(anyhow::anyhow!("keys or venue rules not loaded")),
            };
            let what = format!("{ex:?} {} TWAP {} {} over {minutes} min", base.symbol, if base.side() == depth::Side::Buy { "buy" } else { "sell" }, base.qty);
            // cancelled (Cancel, the kill switch) while the start was in flight: stopped on the venue right away
            let stop = {
                let mut a = account.lock().unwrap();
                match res {
                    Ok(vid) => {
                        a.note(format!("OK {what} id {vid}"), true);
                        let mut cancelled = false;
                        if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) {
                            j.venue_id = Some(vid.clone());
                            cancelled = j.cancelled;
                            j.status = if cancelled { "cancelling".into() } else { format!("running on {ex:?}") };
                        }
                        if cancelled { a.keys.get(&ex).cloned().map(|k| (k, vid)) } else { None }
                    }
                    Err(e) => {
                        a.note(format!("FAIL {what}: {e:#}"), false);
                        if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = format!("failed: {e:#}"); j.cancelled = true; j.end_ms = depth::now_ms(); }
                        None
                    }
                }
            };
            if let Some((k, vid)) = stop {
                let r = trade::cancel_twap(ex, &k, &base.symbol, &vid).await;
                let mut a = account.lock().unwrap();
                match &r { Ok(()) => a.note(format!("OK {ex:?} {} TWAP {vid} cancelled", base.symbol), true), Err(e) => a.note(format!("FAIL cancel TWAP {vid}: {e:#}"), false) }
                if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = match r { Ok(()) => "cancelled".into(), Err(e) => format!("cancel failed: {e:#}") }; j.end_ms = depth::now_ms(); }
            }
            ctx.request_repaint();
        });
        Ok(id)
    }

    /// Read the venue's maximum leverage for `symbol` in the background (leverage sheet).
    pub fn load_max_leverage(&self, ex: Exchange, symbol: String, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            match trade::max_leverage(ex, &k, &symbol).await {
                Ok(m) => { account.lock().unwrap().max_levs.insert((ex, symbol), m); }
                Err(e) => account.lock().unwrap().note(format!("FAIL {ex:?} {symbol} max leverage: {e:#}"), false),
            }
            ctx.request_repaint();
        });
    }

    /// Set the venue's leverage for `symbol` (user-confirmed), then re-read it.
    pub fn set_leverage(&self, ex: Exchange, symbol: String, lev: u32, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            let r = trade::set_leverage(ex, &k, &symbol, lev).await;
            let read = trade::leverage(ex, &k, &symbol).await;
            let mut a = account.lock().unwrap();
            match r {
                Ok(()) => a.note(format!("OK {ex:?} {symbol} leverage {lev}x"), true),
                Err(e) => a.note(format!("FAIL {ex:?} {symbol} leverage {lev}x: {e:#}"), false),
            }
            if let Ok(l) = read { a.levs.insert((ex, symbol), l); }
            ctx.request_repaint();
        });
    }

    pub fn cancel_algo(&self, id: u64) {
        let mut a = self.account.lock().unwrap();
        let Some(j) = a.algos.iter_mut().find(|j| j.id == id) else { return };
        j.cancelled = true;
        j.status = "cancelling".into();
        // venue-run: cancel it there too
        if let (Some(vid), Some(rt)) = (j.venue_id.clone(), self.acct_rt.as_ref()) {
            let (ex, symbol, account) = (j.ex, j.symbol.clone(), self.account.clone());
            let Some(k) = a.keys.get(&ex).cloned() else { return };
            rt.spawn(async move {
                let r = trade::cancel_twap(ex, &k, &symbol, &vid).await;
                let mut a = account.lock().unwrap();
                match r {
                    Ok(()) => { a.note(format!("OK {ex:?} {symbol} TWAP {vid} cancelled"), true); if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = "cancelled".into(); j.end_ms = depth::now_ms(); } }
                    Err(e) => { a.note(format!("FAIL cancel TWAP {vid}: {e:#}"), false); if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = format!("cancel failed: {e:#}"); j.cancelled = false; } }
                }
            });
        }
    }

    /// Look up the account's position mode for (venue, symbol) once.
    pub fn detect_mode(&self, ex: Exchange, symbol: &str) {
        let Some(rt) = self.rt.as_ref() else { return };
        let key = (ex, symbol.to_string());
        let k = {
            let mut a = self.account.lock().unwrap();
            // called every frame by the order panel: one lookup in flight, 30s after a failure
            if a.modes.contains_key(&key) { return; }
            if a.lookup_failed.get(&key).is_some_and(|t| now_ms() - t < 30_000) { return; }
            a.modes.insert(key.clone(), None);
            a.keys.get(&ex).cloned()
        };
        let Some(k) = k else { return };
        let account = self.account.clone();
        rt.spawn(async move {
            let (m, l, r) = tokio::join!(trade::mode(ex, &k, &key.1), trade::leverage(ex, &k, &key.1), trade::rules(ex, &key.1));
            let mut a = account.lock().unwrap();
            if let Ok(l) = l { a.levs.insert(key.clone(), l); }
            if let Ok(r) = r { a.rules.insert(key.clone(), r); }
            match m {
                Ok(m) => { a.modes.insert(key, Some(m)); }
                Err(e) => {
                    a.modes.remove(&key);
                    a.lookup_failed.insert(key, now_ms());
                    a.note(format!("{ex:?} position mode: {e:#}"), false);
                }
            }
        });
    }

    pub fn cancel(&self, o: &OpenOrder, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            let res = trade::cancel_order(o.ex, &k, &o).await;
            let ok = res.is_ok();
            account.lock().unwrap().note(match res { Ok(()) => format!("OK cancel {:?} {} {}", o.ex, o.symbol, o.id), Err(e) => format!("FAIL cancel {:?} {}: {e:#}", o.ex, o.id) }, ok);
            ctx.request_repaint();
        });
    }

    /// Move a resting limit order (dragged on the chart): amended in place where the venue allows it
    /// (Bybit, Binance: same id, nothing in between); elsewhere cancelled, then the rest of it placed
    /// at `price` only once the cancel succeeded, so a failed cancel never leaves two orders.
    /// ponytail: OKX / Bitget / Gate / Kraken / Hyperliquid have amend endpoints too; wire them once
    /// those venues are verified on a real account.
    pub fn replace_order(&self, o: &OpenOrder, price: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o, agg, base) = (self.rt.as_ref(), self.account.clone(), ctx.clone(), o.clone(), self.agg.clone(), self.base.clone()) else { return };
        let opp = |s: depth::Side| if s == depth::Side::Buy { depth::Side::Sell } else { depth::Side::Buy };
        let req = OrderReq {
            symbol: o.symbol.clone(), close: o.reduce_only, kind: trade::Kind::Limit { price, tif: trade::Tif::Gtc }, qty: o.qty - o.filled, client_id: None, attach: None,
            // the position the order acts on: its own side when opening, the other one when reducing
            pos: o.pos.unwrap_or(if o.reduce_only { opp(o.side) } else { o.side }),
        };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            // the new price against the risk band too (in the task: the caller may hold the agg lock)
            let mid = venue_mid(&agg, &base, o.ex, &o.symbol);
            let gate = risk_gate(&mut account.lock().unwrap(), o.ex, &req, mid);
            if let Err(e) = gate { account.lock().unwrap().note(format!("FAIL move {:?} {} {} -> {price}: risk limit: {e}", o.ex, o.symbol, o.id), false); ctx.request_repaint(); return; }
            if trade::can_amend(o.ex) {
                let cached = account.lock().unwrap().rules.get(&(o.ex, o.symbol.clone())).copied();
                let res = async {
                    let r = match cached { Some(r) => r, None => trade::rules(o.ex, &o.symbol).await? };
                    trade::amend(o.ex, &k, &o, price, &r).await
                }.await;
                let what = format!("{:?} {} {} {} -> {price}", o.ex, o.symbol, o.id, o.price);
                let ok = res.is_ok();
                account.lock().unwrap().note(match res { Ok(()) => format!("OK amend {what}"), Err(e) => format!("FAIL amend {what}: {e:#}") }, ok);
                ctx.request_repaint();
                return;
            }
            match trade::cancel(o.ex, &k, &o.symbol, &o.id).await {
                Ok(()) => { place_logged(&account, o.ex, &req, if mid > 0.0 { mid } else { price }).await; }
                Err(e) => account.lock().unwrap().note(format!("FAIL move {:?} {} {}: cancel: {e:#}", o.ex, o.symbol, o.id), false),
            }
            ctx.request_repaint();
        });
    }

    /// Move a TP / SL trigger (dragged on the chart): a whole-position one is replaced in place, a
    /// partial one is re-added at the new price after its cancel succeeded.
    pub fn move_tpsl(&self, t: &trade::tpsl::TpSl, price: f64, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (account, ctx, t) = (self.account.clone(), ctx.clone(), t.clone());
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&t.ex).cloned() else { return };
            let cached = account.lock().unwrap().modes.get(&(t.ex, t.symbol.clone())).copied().flatten();
            let res = async {
                let m = match cached { Some(m) => m, None => trade::mode(t.ex, &k, &t.symbol).await? };
                let (tp, sl) = if t.take_profit { (Some(price), None) } else { (None, Some(price)) };
                match t.qty {
                    None => trade::tpsl::set_position_tpsl(t.ex, &k, &t.symbol, t.pos, m, tp, sl, t.trigger).await,
                    Some(q) => {
                        trade::tpsl::cancel(t.ex, &k, &t.symbol, &t.id).await?;
                        trade::tpsl::add_partial(t.ex, &k, &t.symbol, t.pos, m, t.take_profit, price, q, t.trigger).await.map(|_| ())
                    }
                }
            }.await;
            let what = format!("{:?} {} {} -> {price}", t.ex, t.symbol, if t.take_profit { "TP" } else { "SL" });
            let mut a = account.lock().unwrap();
            match res { Ok(()) => a.note(format!("OK move {what}"), true), Err(e) => a.note(format!("FAIL move {what}: {e:#}"), false) }
            ctx.request_repaint();
        });
    }

    /// Set / replace whole-position TP and/or SL (None leaves a side as is), or add one partial
    /// level (`partial = Some(qty)`); the result lands in the order log, the stream updates state.
    #[allow(clippy::too_many_arguments)]
    pub fn set_tpsl(&self, ex: Exchange, symbol: String, pos: depth::Side, tp: Option<f64>, sl: Option<f64>, partial: Option<f64>, trigger: trade::tpsl::Trigger, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (account, ctx) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
            set_tpsl_logged(&account, ex, symbol, pos, tp, sl, partial, trigger).await;
            ctx.request_repaint();
        });
    }

    pub fn cancel_tpsl(&self, t: &trade::tpsl::TpSl, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (account, ctx, t) = (self.account.clone(), ctx.clone(), t.clone());
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&t.ex).cloned() else { return };
            let r = trade::tpsl::cancel(t.ex, &k, &t.symbol, &t.id).await;
            let mut a = account.lock().unwrap();
            match r { Ok(()) => a.note(format!("OK cancel TP/SL {:?} {} {}", t.ex, t.symbol, t.id), true), Err(e) => a.note(format!("FAIL cancel TP/SL {:?} {}: {e:#}", t.ex, t.id), false) }
            ctx.request_repaint();
        });
    }

    /// Keys changed in Settings: restart that venue's private stream (or stop it and drop its
    /// state when the keys were removed).
    /// ponytail: aborting the stream task leaves tasks it spawned itself (Binance's mark feed)
    /// running until restart; harmless, add cancellation tokens if venues churn often.
    pub fn set_keys(&mut self, ex: Exchange, keys: Option<Keys>) {
        if let Some(h) = self.streams.remove(&ex) { h.abort(); }
        {
            let mut a = self.account.lock().unwrap();
            a.positions.retain(|p| p.ex != ex);
            a.orders.retain(|o| o.ex != ex);
            a.balances.remove(&ex);
            a.errors.remove(&ex);
            a.wallets.remove(&ex);
            a.history.remove(&ex);
            a.modes.retain(|(e, _), _| *e != ex);
            a.levs.retain(|(e, _), _| *e != ex);
            match &keys { Some(k) => { a.keys.insert(ex, k.clone()); } None => { a.keys.remove(&ex); } }
        }
        self.account.lock().unwrap().can_trade.remove(&ex);
        if let (Some(k), Some(rt), Some(tx)) = (keys, self.acct_rt.as_ref(), self.acct_tx.clone()) {
            check_perms(rt, &self.account, ex, k.clone());
            self.streams.insert(ex, rt.spawn(trade::stream(ex, k, tx)));
        }
    }

    /// Read-only connection test: fetch the balance with these keys (nothing is changed).
    pub fn test_keys(&self, ex: Exchange, k: Keys, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (tests, ctx) = (self.key_tests.clone(), ctx.clone());
        tests.lock().unwrap().insert(ex, (true, "…".into()));
        rt.spawn(async move {
            let r = trade::balance(ex, &k).await;
            tests.lock().unwrap().insert(ex, match r { Ok(b) => (true, format!("OK · equity {:.2}", b.equity)), Err(e) => (false, format!("{e:#}")) });
            ctx.request_repaint();
        });
    }

    /// Fetch every account of `ex` (on demand: expensive on Binance, never on a timer).
    pub fn load_wallets(&self, ex: Exchange, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let k = {
            let mut a = self.account.lock().unwrap();
            if !a.wallets_loading.insert(ex) { return; }
            a.keys.get(&ex).cloned()
        };
        let Some(k) = k else { return };
        let (account, ctx) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
            let (w, notes) = trade::wallets(ex, &k).await;
            let mut a = account.lock().unwrap();
            a.wallets.insert(ex, (w, notes, now_ms()));
            a.wallets_loading.remove(&ex);
            ctx.request_repaint();
        });
    }

    /// Fetch recent history for every venue with keys (on demand; Binance is per symbol, so the
    /// symbols are those with positions, open orders, or the one on screen).
    pub fn load_history(&self, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (keys, symbols) = {
            let a = self.account.lock().unwrap();
            let mut syms: Vec<String> = a.positions.iter().map(|p| p.symbol.clone()).chain(a.orders.iter().map(|o| o.symbol.clone())).collect();
            syms.push(trade::symbol(&self.base));
            syms.sort();
            syms.dedup();
            (a.keys.iter().filter(|(e, _)| !a.history_loading.contains(e)).map(|(e, k)| (*e, k.clone())).collect::<Vec<_>>(), syms)
        };
        for (ex, k) in keys {
            self.account.lock().unwrap().history_loading.insert(ex);
            let (account, ctx, symbols) = (self.account.clone(), ctx.clone(), symbols.clone());
            rt.spawn(async move {
                let r = trade::history(ex, &k, &symbols).await;
                let mut a = account.lock().unwrap();
                match r {
                    Ok(h) => { a.history.insert(ex, (h, now_ms())); a.fills_ver += 1; }
                    Err(e) => a.note(format!("{ex:?} history: {e:#}"), false),
                }
                a.history_loading.remove(&ex);
                ctx.request_repaint();
            });
        }
    }

    /// Own fills of `symbol` missed while the app was closed: once per venue and symbol per
    /// session, from the newest stored fill (pushes keep it current afterwards).
    pub fn backfill_fills(&self, symbol: &str, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let todo: Vec<(Exchange, Keys)> = {
            let mut a = self.account.lock().unwrap();
            let ks: Vec<_> = a.keys.iter().filter(|(e, _)| matches!(e, Exchange::Bybit | Exchange::Binance)).map(|(e, k)| (*e, k.clone())).collect();
            ks.into_iter().filter(|(e, _)| a.fills_done.insert((*e, symbol.to_string()))).collect()
        };
        for (ex, k) in todo {
            let (account, ctx, symbol) = (self.account.clone(), ctx.clone(), symbol.to_string());
            rt.spawn(async move {
                let since = depth::db::fills_last(ex, &symbol).map_or(0, |t| t + 1);
                match trade::fills_since(ex, &k, &symbol, since).await {
                    Ok(f) => { depth::db::fills_put(&f); account.lock().unwrap().fills_ver += 1; ctx.request_repaint(); }
                    Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} {symbol} fills: {e:#}\n")),
                }
            });
        }
    }

    /// Run one transfer; the result goes to the order log and the venue's wallets are reloaded.
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(&self, ex: Exchange, from: String, to: String, coin: String, amount: f64, product: Option<String>, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let Some(k) = self.account.lock().unwrap().keys.get(&ex).cloned() else { return };
        self.account.lock().unwrap().transferring = true;
        let (account, ctx2) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
            let r = trade::transfer(ex, &k, &from, &to, &coin, amount, product.as_deref()).await;
            {
                let mut a = account.lock().unwrap();
                match r {
                    Ok(lines) => { for l in lines { a.note(format!("OK {l}"), true); } shift_day(&mut a, ex, &from, &to, &coin, amount); }
                    Err(e) => a.note(format!("FAIL transfer {ex:?} {from} -> {to} {amount} {coin}: {e:#}"), false),
                }
                a.transferring = false;
            }
            reload_wallets(&account, ex, &k).await;
            ctx2.request_repaint();
        });
    }

    /// Margin (borrow rates) and option streams are heavy; start them on first use.
    pub fn ensure(&mut self, market: Market) {
        let (Some(rt), Some(tx)) = (self.rt.as_ref(), self.tx.clone()) else { return };
        let _guard = rt.enter();
        self.spawn_market(market, &tx);
    }
}

fn cache_key(ex: Exchange, sub: &Sub, tf: u32) -> String { format!("{}/{ex:?}-{:?}-{tf}m", sub.base, sub.market) }

/// History from the local cache plus only the candles since its last bar; a full fetch when
/// there is no usable cache. On fetch failure the cache alone is used. The result is written back.
async fn cached_history(ex: Exchange, sub: &Sub, tf: u32, stats: &Arc<Mutex<Stats>>) -> History {
    let key = cache_key(ex, sub, tf);
    let cached: Option<History> = depth::db::hist_get(&key).and_then(|b| serde_json::from_slice(&b).ok());
    let step = tf as i64 * 60_000;
    // bars missing since the cached tip (+2 to refresh the forming candle and overlap)
    let need = cached.as_ref().and_then(|c| c.klines.last()).map(|k| ((now_ms() - k.t) / step + 2) as usize).filter(|n| *n < HISTORY_MINUTES);
    let t0 = std::time::Instant::now();
    let fetched = ex::history_tf(ex, sub, tf, need.unwrap_or(HISTORY_MINUTES)).await;
    if std::env::var("T1_DIAG").is_ok() { eprintln!("[cache] {ex:?} {:?} {tf}m fetched {} bars in {:?}", sub.market, need.unwrap_or(HISTORY_MINUTES), t0.elapsed()); }
    let h = match (cached, fetched, need) {
        (Some(c), Ok(f), Some(_)) => c.merge(f, HISTORY_MINUTES, tf),
        (_, Ok(f), _) => f,
        (c, Err(e), _) => {
            eprintln!("[history] {ex:?} {:?} {tf}m: {e:#}", sub.market);
            stats.lock().unwrap().hist_errors.push(format!("{ex:?} {:?} {tf}m: {e:#}", sub.market));
            return c.unwrap_or_default();
        }
    };
    if !h.klines.is_empty() { if let Ok(b) = serde_json::to_vec(&h) { depth::db::hist_put(&key, &b); } }
    h
}

/// REST snapshot of one venue's spot orders and coins (replaces that venue's rows). Errors (a key
/// without spot permission) go to the trade log only: they must not mark the venue's perp account.
async fn refresh_spot(account: &Arc<Mutex<Account>>, ex: Exchange) {
    if !trade::spot::VENUES.contains(&ex) { return; }
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (o, b) = tokio::join!(trade::spot::open_orders(ex, &k), trade::spot::balances(ex, &k));
    {
        let mut a = account.lock().unwrap();
        match o { Ok(o) => { a.spot_orders.retain(|x| x.ex != ex); a.spot_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} spot orders: {e:#}\n")) }
        match b { Ok(b) => { a.spot_bal.insert(ex, b); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} spot balances: {e:#}\n")) }
    }
    // margin: only once Margin mode has asked for a symbol (Binance margin orders are per symbol)
    let Some(sym) = account.lock().unwrap().margin_sym.clone() else { return };
    let (o, b) = tokio::join!(trade::spot::margin_orders(ex, &k, &sym), trade::spot::margin_balances(ex, &k));
    let mut a = account.lock().unwrap();
    match o { Ok(o) => { a.margin_orders.retain(|x| x.ex != ex); a.margin_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} margin orders: {e:#}\n")) }
    match b { Ok(b) => { a.margin_bal.insert(ex, b); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} margin balances: {e:#}\n")) }
}

/// REST snapshot of one venue's option orders and positions (option venues only).
async fn refresh_opts(account: &Arc<Mutex<Account>>, ex: Exchange) {
    if !trade::OPTION_VENUES.contains(&ex) { return; }
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (o, p) = tokio::join!(trade::option_orders(ex, &k), trade::option_positions(ex, &k));
    let mut a = account.lock().unwrap();
    match o { Ok(o) => { a.opt_orders.retain(|x| x.ex != ex); a.opt_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} option orders: {e:#}\n")) }
    if let Ok(p) = p { a.opts.retain(|x| x.ex != ex); a.opts.extend(p); }
}

/// REST snapshot of one venue's positions, open orders and balance (replaces that venue's rows).
async fn refresh_venue(account: &Arc<Mutex<Account>>, ex: Exchange) {
    refresh_spot(account, ex).await;
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (p, o, b, op, oo) = tokio::join!(trade::positions(ex, &k), trade::open_orders(ex, &k), trade::balance(ex, &k), trade::option_positions(ex, &k), trade::option_orders(ex, &k));
    // TP/SL snapshot (Binance: 40 weight for the account; snapshots only run on resync / 5 min)
    if matches!(ex, Exchange::Bybit | Exchange::Binance) {
        if let Ok(t) = trade::tpsl::list(ex, &k).await {
            let mut a = account.lock().unwrap();
            a.tpsl.retain(|x| x.ex != ex);
            a.tpsl.extend(t);
        }
    }
    let mut a = account.lock().unwrap();
    // each part applies on its own: a failing orders or balance call must not freeze positions
    let mut err = None;
    match p { Ok(p) => replace_positions(&mut a, ex, p), Err(e) => err = err.or(Some(e)) }
    match o { Ok(o) => { a.orders.retain(|x| x.ex != ex); a.orders.extend(o); } Err(e) => err = err.or(Some(e)) }
    // an account without option trading answers with an error: keep it out of the venue's status
    if let Ok(op) = op { a.opts.retain(|x| x.ex != ex); a.opts.extend(op); }
    if let Ok(oo) = oo { a.opt_orders.retain(|x| x.ex != ex); a.opt_orders.extend(oo); }
    match b { Ok(b) => { a.balances.insert(ex, b); } Err(e) => err = err.or(Some(e)) }
    match err {
        None => { a.errors.remove(&ex); }
        Some(e) => {
            let e = format!("{e:#}");
            if a.errors.get(&ex) != Some(&e) { a.note(format!("{ex:?} account: {e}"), false); }
            a.errors.insert(ex, e);
        }
    }
}

/// Assets whose cached keyed borrow rate is missing or at least `BORROW_TTL_MS` old.
fn stale_borrow(cache: &HashMap<String, (f64, i64)>, assets: &[&str], now: i64) -> Vec<String> {
    assets.iter().filter(|a| cache.get(**a).is_none_or(|(_, at)| now - at >= BORROW_TTL_MS)).map(|a| a.to_string()).collect()
}

/// Binance is the one venue whose position pushes lack the liquidation price while its REST rows
/// have it, so a push without one is followed by a refetch. Kraken's REST rows carry no mark / liq /
/// margin either, and the other pushes bring their own liq: a refetch there gains nothing.
fn wants_liq_refetch(ex: Exchange, p: &Position) -> bool { ex == Exchange::Binance && p.qty > 0.0 && p.liq.is_none() }

/// A REST position snapshot replaces the venue's rows but keeps what the snapshot leaves out
/// (Kraken: mark, liq, margin) from the rows pushed so far.
/// Kraken's REST rows carry no mark / liq / margin, so they merge into the pushed rows; every other venue's
/// snapshot is authoritative (a missing liq there means the position has none, e.g. well-funded cross).
fn replace_positions(a: &mut Account, ex: Exchange, new: Vec<Position>) {
    let old: Vec<Position> = a.positions.iter().filter(|x| x.ex == ex).cloned().collect();
    a.positions.retain(|x| x.ex != ex);
    a.positions.extend(if ex == Exchange::Kraken { trade::merge_snapshot(&old, new) } else { new });
}

async fn reload_wallets(account: &Arc<Mutex<Account>>, ex: Exchange, k: &Keys) {
    let (w, notes) = trade::wallets(ex, k).await;
    account.lock().unwrap().wallets.insert(ex, (w, notes, now_ms()));
}

/// USD to top up now, if the venue's rule says so (marks the rule as fired).
fn auto_due(a: &mut Account, ex: Exchange) -> Option<f64> {
    let rule = *a.auto.get(&ex)?;
    let avail = a.balances.get(&ex)?.available;
    let now = now_ms();
    if !rule.enabled || rule.target <= rule.min || avail >= rule.min || a.auto_last.get(&ex).is_some_and(|t| now - t < 60_000) { return None; }
    a.auto_last.insert(ex, now);
    Some(rule.target - avail)
}

/// Move up to `need` USDT into the margin account from the venue's other accounts, in order.
fn spawn_top_up(account: &Arc<Mutex<Account>>, ex: Exchange, need: f64, ctx: &eframe::egui::Context) {
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (account, ctx) = (account.clone(), ctx.clone());
    tokio::spawn(async move {
        let (wallets, _) = trade::wallets(ex, &k).await;
        let margin = wallets.iter().find(|w| trade::MARGIN_IDS.contains(&w.id.as_str())).map(|w| w.id.clone());
        let Some(margin) = margin else { account.lock().unwrap().note(format!("AUTO {ex:?}: margin account not readable"), false); return };
        let order: &[&str] = if ex == Exchange::Bybit { &["FUND", "EARN"] } else { &["SPOT", "FUNDING", "EARN"] };
        let mut left = need;
        for src in order {
            if left < 1.0 { break; }
            let Some(c) = wallets.iter().find(|w| w.id == *src).and_then(|w| w.coins.iter().find(|c| c.coin == "USDT" && c.free >= 1.0)) else { continue };
            let amt = left.min(c.free);
            let r = trade::transfer(ex, &k, src, &margin, "USDT", amt, c.product.as_deref()).await;
            let mut a = account.lock().unwrap();
            match r {
                Ok(lines) => { for l in lines { a.note(format!("AUTO {l}"), true); } shift_day(&mut a, ex, src, &margin, "USDT", amt); left -= amt; }
                Err(e) => a.note(format!("AUTO FAIL {ex:?} {src} -> {margin} {amt:.2} USDT: {e:#}"), false),
            }
        }
        if left >= 1.0 { account.lock().unwrap().note(format!("AUTO {ex:?}: {left:.2} USDT short, nothing left to move"), false); }
        reload_wallets(&account, ex, &k).await;
        ctx.request_repaint();
    });
}

/// The orders that reverse `p` on its venue: one-way, one market order of twice the size on the
/// other side (the venue closes and opens in one fill); hedge (or mode unknown), a market close of
/// the position, then a market open of the same size on the other side (`Engine::submit_seq`).
pub fn reverse_reqs(mode: Option<Mode>, p: &Position) -> Vec<OrderReq> {
    let other = if p.side == depth::Side::Buy { depth::Side::Sell } else { depth::Side::Buy };
    let o = |pos, close, qty| OrderReq { symbol: p.symbol.clone(), pos, close, kind: trade::Kind::Market, qty, client_id: None, attach: None };
    match mode {
        Some(Mode::OneWay) => vec![o(other, false, 2.0 * p.qty)],
        _ => vec![o(p.side, true, p.qty), o(other, false, p.qty)],
    }
}

/// Apply one pushed account change; true when something visible changed.
fn apply(a: &mut Account, ex: Exchange, ev: trade::AccEvent) -> bool {
    use trade::AccEvent as E;
    match ev {
        E::Position { mut p, one_way } => {
            let same = |x: &Position| x.ex == ex && x.symbol == p.symbol && (one_way || x.side == p.side);
            let old = a.positions.iter().find(|x| same(x)).cloned();
            a.positions.retain(|x| !same(x));
            // venues without a fill push: a size change is a fill (long = +, short = -)
            let signed = |x: &Position| if x.side == depth::Side::Buy { x.qty } else { -x.qty };
            let d = signed(&p) - old.as_ref().map_or(0.0, signed);
            if !matches!(ex, Exchange::Binance | Exchange::Bybit) && d.abs() > 1e-12 && a.resync_ms.get(&ex).is_some_and(|t| now_ms() - t > 5_000) {
                a.fill_seq += 1;
                a.last_fill = Some(FillNote { ex, symbol: p.symbol.clone(), buy: d > 0.0, qty: d.abs(), price: None });
            }
            if p.qty > 0.0 {
                // fields a push leaves out keep their last known value, except the liquidation price
                // and margin of a position whose size or entry changed: those belonged to the old
                // position (the venue's figure is refetched, see pos_dirty)
                if let Some(o) = old {
                    let same = o.qty == p.qty && o.entry == p.entry;
                    if p.mark == 0.0 { p.mark = o.mark; }
                    if p.liq.is_none() && same { p.liq = o.liq; }
                    if p.lev == 0.0 { p.lev = o.lev; }
                    if p.margin == 0.0 && same { p.margin = o.margin; }
                }
                if p.mark == 0.0 { p.mark = p.entry; }
                if p.margin == 0.0 && p.lev > 0.0 { p.margin = p.mark * p.qty / p.lev; }
                if p.lev > 0.0 { a.levs.insert((ex, p.symbol.clone()), p.lev); }
                a.positions.push(p);
            }
            true
        }
        E::Mark { symbol, mark } => {
            let mut changed = false;
            // equity change from this mark, applied below to the other cross positions' liquidation prices
            let mut moved = vec![];
            for (i, p) in a.positions.iter_mut().enumerate().filter(|(_, p)| p.ex == ex && p.symbol == symbol) {
                let sign = if p.side == depth::Side::Buy { 1.0 } else { -1.0 };
                let upnl = (mark - p.entry) * p.qty * sign;
                if p.cross == Some(true) { moved.push((i, upnl - p.upnl)); }
                p.mark = mark;
                p.upnl = upnl;
                // cross margin is notional at mark over leverage (what the venues show); isolated stays put
                if p.cross == Some(true) && p.lev > 0.0 { p.margin = mark * p.qty / p.lev; }
                changed = true;
            }
            // Venues' cross liquidation price of a position solves equity(P) = maintenance at fixed
            // other prices, so between the venue's own figures (refetched after each account push)
            // another position's PnL change d moves it by d / qty: down for a long, up for a short.
            // ponytail: ignores the maintenance-rate factor 1 / (1 - mmr) (~0.5%) and collateral
            // haircut moves; the next refetch replaces it with the venue's number.
            for (i, d) in moved {
                for (j, o) in a.positions.iter_mut().enumerate() {
                    if j == i || o.ex != ex || o.cross != Some(true) || o.qty <= 0.0 { continue }
                    let sign = if o.side == depth::Side::Buy { 1.0 } else { -1.0 };
                    // at or below zero the venue shows none
                    o.liq = o.liq.map(|l| l - d * sign / o.qty).filter(|l| *l > 0.0);
                }
            }
            changed
        }
        E::Order(o) => { a.orders.retain(|x| !(x.ex == ex && x.id == o.id)); a.orders.push(o); true }
        E::OrderDone { id } => { a.orders.retain(|x| !(x.ex == ex && x.id == id)); true }
        E::Wallet(w) => {
            // equity / available always replace; a risk field the push does not carry keeps its last value
            let b = a.balances.entry(ex).or_default();
            b.equity = w.equity;
            b.available = w.available;
            b.uni_mmr = w.uni_mmr.or(b.uni_mmr);
            b.mm_rate = w.mm_rate.or(b.mm_rate);
            b.maint_margin = w.maint_margin.or(b.maint_margin);
            b.adj_equity = w.adj_equity.or(b.adj_equity);
            true
        }
        E::Down => { depth::sys::append_log("trade.log", &format!("{ex:?} account stream down\n")); false }
        E::Liquidated { symbol, side, qty, price } => {
            depth::sys::append_log("trade.log", &format!("{ex:?} liquidated {symbol} {side:?} {qty} @ {price}\n"));
            false
        }
        E::TpSl(t) => { a.tpsl.retain(|x| !(x.ex == ex && x.id == t.id)); a.tpsl.push(t); true }
        E::TpSlDone { id } => { a.tpsl.retain(|x| !(x.ex == ex && x.id == id)); true }
        E::Fill(f) => {
            depth::db::fills_put(std::slice::from_ref(&f));
            a.fills_ver += 1;
            a.fill_seq += 1;
            a.last_fill = Some(FillNote { ex, symbol: f.symbol, buy: f.side == depth::Side::Buy, qty: f.qty, price: Some(f.price) });
            true
        }
        E::OptPos(p) => {
            a.opts.retain(|x| !(x.ex == ex && x.symbol == p.symbol));
            if p.qty > 0.0 { a.opts.push(p); }
            true
        }
        E::Resync | E::BalanceDirty | E::SpotDirty => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth::Side;
    use trade::AccEvent as E;

    fn pos(side: Side, qty: f64) -> Position {
        Position { ex: Exchange::Binance, symbol: "BTCUSDT".into(), side, qty, entry: 100.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None }
    }

    #[test]
    fn funding_fetch_only_when_due() {
        let mut a = Account::default();
        a.keys.insert(Exchange::Binance, Keys { key: "k".into(), secret: "s".into(), extra: None, demo: false });
        a.positions.push(pos(Side::Buy, 1.0));
        let fund = HashMap::from([((Exchange::Binance, "BTCUSDT".to_string()), VenueFunding { rate: 0.0, interval_h: 8.0, next_ms: 1_000_000 })]);
        assert_eq!(funding_due(&mut a, &fund, 0).len(), 1);
        assert!(funding_due(&mut a, &fund, 1_000).is_empty());
        // size change: debounced 60 s from the last fetch
        a.positions[0].qty = 2.0;
        assert!(funding_due(&mut a, &fund, 30_000).is_empty());
        assert_eq!(funding_due(&mut a, &fund, 60_000).len(), 1);
        // 20 s after the settlement, then the next one (stale listed time rolled forward 8h)
        assert!(funding_due(&mut a, &fund, 1_019_000).is_empty());
        assert_eq!(funding_due(&mut a, &fund, 1_020_000).len(), 1);
        assert_eq!(a.funding_paid.values().next().unwrap().due, 1_000_000 + 8 * 3_600_000 + 20_000);
        // closed: slot dropped
        a.positions.clear();
        assert!(funding_due(&mut a, &fund, 1_030_000).is_empty() && a.funding_paid.is_empty());
    }

    #[test]
    fn cross_liq_follows_other_positions_pnl() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        let p = |sym: &str, side, qty, entry, liq| Position { ex, symbol: sym.into(), side, qty, entry, mark: entry, liq, upnl: 0.0, lev: 10.0, margin: 0.0, cross: Some(true) };
        a.positions.push(p("SOLUSDT", Side::Buy, 10.0, 100.0, Some(50.0)));
        a.positions.push(p("ETHUSDT", Side::Sell, 2.0, 3000.0, Some(4000.0)));
        // SOL +1 => +10 equity: the short ETH liq rises by 10 / 2, SOL's own liq is unchanged
        apply(&mut a, ex, E::Mark { symbol: "SOLUSDT".into(), mark: 101.0 });
        assert_eq!(a.positions[0].liq, Some(50.0));
        assert_eq!(a.positions[1].liq, Some(4005.0));
        // cross margin follows mark: 101 * 10 / 10
        assert_eq!(a.positions[0].margin, 101.0);
        // ETH +10 => short loses 20: SOL's long liq rises by 20 / 10
        apply(&mut a, ex, E::Mark { symbol: "ETHUSDT".into(), mark: 3010.0 });
        assert_eq!(a.positions[0].liq, Some(52.0));
    }

    #[test]
    fn pushes_merge_into_account_state() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        // REST snapshot row with fields a push does not carry
        a.positions.push(Position { liq: Some(80.0), lev: 10.0, margin: 50.0, mark: 101.0, ..pos(Side::Buy, 1.0) });
        // a push with the same size keeps liq/margin; a resized one drops them (they belonged to the
        // old position): liq until the refetch, margin recomputed at mark; short added alongside
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 1.0), one_way: false });
        assert_eq!((a.positions[0].liq, a.positions[0].margin), (Some(80.0), 50.0));
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 2.0), one_way: false });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 1.0), one_way: false });
        assert_eq!(a.positions.len(), 2);
        let long = a.positions.iter().find(|p| p.side == Side::Buy).unwrap();
        assert_eq!((long.qty, long.liq, long.lev, long.mark, long.margin), (2.0, None, 10.0, 101.0, 20.2));
        // mark drives upnl on both sides
        apply(&mut a, ex, E::Mark { symbol: "BTCUSDT".into(), mark: 110.0 });
        let upnl: Vec<f64> = a.positions.iter().map(|p| p.upnl).collect();
        assert!(upnl.contains(&20.0) && upnl.contains(&-10.0), "{upnl:?}");
        // a one-way zero row clears the symbol on either side
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 0.0), one_way: true });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 0.0), one_way: true });
        assert!(a.positions.is_empty());
        // orders upsert by id and leave on done
        let o = OpenOrder { ex, symbol: "BTCUSDT".into(), id: "7".into(), side: Side::Buy, price: 99.0, qty: 1.0, filled: 0.0, kind: "LIMIT".into(), tif: String::new(), reduce_only: false, ts: 0, pos: None, client_id: None, trigger: None };
        apply(&mut a, ex, E::Order(o.clone()));
        apply(&mut a, ex, E::Order(OpenOrder { filled: 0.5, ..o }));
        assert_eq!(a.orders.len(), 1);
        assert_eq!(a.orders[0].filled, 0.5);
        apply(&mut a, ex, E::OrderDone { id: "7".into() });
        assert!(a.orders.is_empty());
    }

    fn wait(cid: &str) -> TpSlWait {
        TpSlWait::new(Exchange::Binance, &OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind: trade::Kind::Market, qty: 1.0, client_id: Some(cid.into()), attach: None },
            trade::tpsl::Attach { tp: Some(120.0), sl: Some(90.0), trigger: trade::tpsl::Trigger::Mark }, 0).unwrap()
    }
    fn order(id: &str, cid: Option<&str>, filled: f64) -> OpenOrder {
        OpenOrder { ex: Exchange::Binance, symbol: "BTCUSDT".into(), id: id.into(), side: Side::Buy, price: 99.0, qty: 1.0, filled, kind: "LIMIT".into(), tif: String::new(), reduce_only: false, ts: 0, pos: None, client_id: cid.map(String::from), trigger: None }
    }
    fn fired(acts: &[WaitAct]) -> usize { acts.iter().filter(|a| matches!(a, WaitAct::Fire(_))).count() }

    #[test]
    fn tpsl_wait_fires_once_on_its_own_fill() {
        let ex = Exchange::Binance;
        let mut a = Account::default();
        a.tpsl_wait.push(wait("t1al0"));
        // the venue id comes with the order's push, matched by our client id
        assert!(wait_touch(&mut a, ex, &E::Order(order("55", Some("t1al0"), 0.0)), 1).is_empty());
        assert_eq!(a.tpsl_wait[0].order_id.as_deref(), Some("55"));
        // a partial fill: waits for the position, then fires once
        wait_touch(&mut a, ex, &E::Order(order("55", None, 0.4)), 2);
        assert!(settle(&mut a, 3).is_empty());
        a.positions.push(pos(Side::Buy, 0.4));
        let acts = settle(&mut a, 4);
        assert_eq!(fired(&acts), 1);
        assert!(matches!(&acts[0], WaitAct::Fire(w) if w.tp == Some(120.0) && w.sl == Some(90.0)));
        assert!(a.tpsl_wait.is_empty() && settle(&mut a, 5).is_empty());
    }

    #[test]
    fn tpsl_wait_drops_when_its_order_ends_unfilled() {
        let ex = Exchange::Binance;
        let mut a = Account::default();
        a.tpsl_wait.push(wait("t1al0"));
        wait_touch(&mut a, ex, &E::Order(order("55", Some("t1al0"), 0.0)), 1);
        // done without a known fill: one lookup, never two
        let acts = wait_touch(&mut a, ex, &E::OrderDone { id: "55".into() }, 2);
        assert!(matches!(&acts[..], [WaitAct::Check(w)] if w.client_id == "t1al0"));
        assert!(wait_touch(&mut a, ex, &E::OrderDone { id: "55".into() }, 3).is_empty());
        resolve(&mut a, &wait("t1al0"), Ok(Some(trade::Found::Done { id: "55".into(), filled: 0.0, status: "CANCELED".into() })), 4);
        assert!(a.tpsl_wait.is_empty());
        assert!(a.log.back().is_some_and(|l| l.1.contains("TP/SL dropped") && l.1.contains("CANCELED")));
        // a market order that filled: the lookup says so and the position is there
        a.tpsl_wait.push(wait("t1bl0"));
        a.positions.push(pos(Side::Buy, 1.0));
        resolve(&mut a, &wait("t1bl0"), Ok(Some(trade::Found::Done { id: "56".into(), filled: 1.0, status: "FILLED".into() })), 5);
        assert_eq!(fired(&settle(&mut a, 6)), 1);
        // a lookup that fails drops it with a note
        a.tpsl_wait.push(wait("t1cl0"));
        resolve(&mut a, &wait("t1cl0"), Err(anyhow::anyhow!("timeout")), 7);
        assert!(a.tpsl_wait.is_empty());
    }

    #[test]
    fn tpsl_wait_ignores_other_orders_and_growth() {
        let ex = Exchange::Binance;
        let mut a = Account::default();
        a.tpsl_wait.push(wait("t1al0"));
        wait_touch(&mut a, ex, &E::Order(order("55", Some("t1al0"), 0.0)), 1);
        // another order on the symbol fills and ends, the position grows: nothing fires, nothing is asked
        assert!(wait_touch(&mut a, ex, &E::Order(order("77", Some("other"), 0.5)), 2).is_empty());
        assert!(wait_touch(&mut a, ex, &E::OrderDone { id: "77".into() }, 3).is_empty());
        assert!(wait_touch(&mut a, Exchange::Bybit, &E::OrderDone { id: "55".into() }, 3).is_empty());
        a.positions.push(pos(Side::Buy, 0.5));
        assert!(settle(&mut a, 4).is_empty());
        assert_eq!(a.tpsl_wait.len(), 1);
        // entries without a fill expire after a day
        assert!(settle(&mut a, 86_400_001).is_empty() && a.tpsl_wait.is_empty());
    }

    #[test]
    fn tpsl_wait_restored_rearms_while_resting_else_drops() {
        let ex = Exchange::Binance;
        let mut a = Account::default();
        let mut w = wait("t1al0");
        w.order_id = Some("55".into());
        // settings.json round trip: what matters survives, the runtime flags start over
        let mut back: TpSlWait = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert!(!back.armed && back.order_id.as_deref() == Some("55") && back.client_id == "t1al0");
        back.restored = true;
        a.tpsl_wait.push(back.clone());
        // before the venue's snapshot nothing happens, even with a position on screen
        a.positions.push(pos(Side::Buy, 1.0));
        assert!(settle(&mut a, 10).is_empty());
        // snapshot: the order rests no more: dropped with a notice, never set on whatever position is there now
        assert!(rearm(&mut a, ex, 11).is_empty());
        assert!(a.tpsl_wait.is_empty() && a.log.back().is_some_and(|l| l.1.contains("dropped after the restart")));
        // still resting: re-armed, and its fill later sets it
        a.tpsl_wait.push(back);
        a.orders.push(order("55", Some("t1al0"), 0.0));
        assert!(rearm(&mut a, ex, 12).is_empty());
        assert!(a.tpsl_wait[0].armed && !a.tpsl_wait[0].restored);
        wait_touch(&mut a, ex, &E::Order(order("55", Some("t1al0"), 1.0)), 13);
        assert_eq!(fired(&settle(&mut a, 14)), 1);
    }

    #[test]
    fn day_pnl_rolls_at_local_midnight_and_shifts_on_transfer() {
        let mut a = Account::default();
        let ex = Exchange::Bybit;
        a.keys.insert(ex, Keys { key: "k".into(), secret: "s".into(), extra: None, demo: false });
        let day = 86_400_000;
        let eq = |a: &mut Account, e: f64| a.balances.insert(ex, trade::Balance { equity: e, ..Default::default() });
        assert_eq!(day_pnl(&mut a, 10 * day, 0), None);
        eq(&mut a, 1000.0);
        assert_eq!(day_pnl(&mut a, 10 * day + 1, 0), Some(0.0));
        eq(&mut a, 900.0);
        assert_eq!(day_pnl(&mut a, 10 * day + 2, 0), Some(-100.0));
        // 200 USDT moved in from the funding account is not profit
        eq(&mut a, 1100.0);
        shift_day(&mut a, ex, "FUND", "UNIFIED", "USDT", 200.0);
        assert_eq!(day_pnl(&mut a, 10 * day + 3, 0), Some(-100.0));
        // UTC+8: local midnight comes 8 h before UTC midnight, then a new baseline
        assert_eq!(day_pnl(&mut a, 11 * day - 8 * 3_600_000 - 1, 8 * 3_600_000), Some(-100.0));
        assert_eq!(day_pnl(&mut a, 11 * day - 8 * 3_600_000, 8 * 3_600_000), Some(0.0));
        // a venue without keys does not count
        a.balances.insert(Exchange::Okx, trade::Balance { equity: 5.0, ..Default::default() });
        assert_eq!(day_pnl(&mut a, 11 * day, 8 * 3_600_000), Some(0.0));
    }

    #[test]
    fn place_guard_blocks_before_send() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        a.limits = depth::risk::Limits { max_order_usdt: 150.0, band_bps: 100.0, ..Default::default() };
        a.positions.push(Position { mark: 100.0, ..pos(Side::Buy, 1.0) });
        a.modes.insert((ex, "BTCUSDT".into()), Some(Mode::OneWay));
        let o = |kind, qty| OrderReq { symbol: "BTCUSDT".into(), pos: Side::Sell, close: false, kind, qty, client_id: None, attach: None };
        // a one-way sell of 2 against the long closes 1 first: 100 USDT new exposure passes
        let (c, r) = risk_ctx(&mut a, ex, &o(trade::Kind::Market, 2.0), 0.0, 0, 0);
        assert_eq!((c.mark, c.held_usdt, r), (Some(100.0), 100.0, 1.0));
        assert!(risk_gate(&mut a, ex, &o(trade::Kind::Market, 2.0), 100.0).is_ok());
        assert!(risk_gate(&mut a, ex, &o(trade::Kind::Market, 3.0), 100.0).unwrap_err().contains("above the 150"));
        // the band is judged on the venue price the order was priced from
        let lim = |p| trade::Kind::Limit { price: p, tif: trade::Tif::Gtc };
        assert!(risk_gate(&mut a, ex, &o(lim(102.0), 0.1), 100.0).is_err());
        assert!(risk_gate(&mut a, ex, &o(lim(102.0), 0.1), 101.5).is_ok());
        // a hedge account reduces nothing; a close is never stopped
        a.modes.insert((ex, "BTCUSDT".into()), Some(Mode::Hedge));
        assert!(risk_gate(&mut a, ex, &o(trade::Kind::Market, 2.0), 100.0).is_err());
        assert!(risk_gate(&mut a, ex, &OrderReq { close: true, ..o(trade::Kind::Market, 50.0) }, 100.0).is_ok());
    }

    fn leg(ex: Exchange, cid: &str, qty: f64) -> depth::route::Leg {
        depth::route::Leg { ex, req: OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind: trade::Kind::Market, qty, client_id: Some(cid.into()), attach: None },
            ref_px: 100.0, mode: Mode::OneWay, est_px: 100.0, est_fee: 0.0, slip_bps: 0.0, avail_after: 0.0, fx: 1.0 }
    }

    #[test]
    fn plan_run_resolves_unknown_leg_from_push_by_client_id() {
        let mut a = Account::default();
        a.record_plan(&[leg(Exchange::Binance, "t1al0", 1.0), leg(Exchange::Bybit, "t1al1", 2.0)]);
        a.set_leg("t1al0", &LegState::Unknown);
        a.unknown.push((Exchange::Binance, "t1al0".into()));
        // another venue's push with the same id, or an order without one, changes nothing
        found_by_push(&mut a, Exchange::Bybit, &E::Order(order("9", Some("t1al0"), 0.0)));
        found_by_push(&mut a, Exchange::Binance, &E::Order(order("9", None, 0.0)));
        assert_eq!(a.plans[0].legs[0].state, LegState::Unknown);
        found_by_push(&mut a, Exchange::Binance, &E::Order(order("55", Some("t1al0"), 0.0)));
        assert_eq!(a.plans[0].legs[0].state, LegState::Live("55".into()));
        assert!(a.unknown.is_empty() && a.log.back().is_some_and(|l| l.1.contains("after all")));
        // a leg that answered keeps its id
        a.set_leg("t1al1", &LegState::Ok("77".into()));
        found_by_push(&mut a, Exchange::Bybit, &E::Order(OpenOrder { ex: Exchange::Bybit, ..order("77", Some("t1al1"), 0.0) }));
        assert_eq!(a.plans[0].legs[1].state, LegState::Ok("77".into()));
        // ids for orders without one never repeat
        assert_ne!(new_cid(), new_cid());
        assert!(new_cid().len() <= 36);
    }

    #[test]
    fn retry_remainder_only_when_nothing_unknown() {
        let mut a = Account::default();
        a.record_plan(&[leg(Exchange::Binance, "a", 1.0), leg(Exchange::Bybit, "b", 2.0), leg(Exchange::Okx, "c", 0.5)]);
        assert_eq!(remainder(&a.plans[0]), None);
        a.set_leg("a", &LegState::Ok("1".into()));
        a.set_leg("b", &LegState::Failed("insufficient margin".into()));
        a.set_leg("c", &LegState::Unknown);
        assert_eq!(remainder(&a.plans[0]), None);
        a.set_leg("c", &LegState::Failed("not placed (no answer)".into()));
        assert_eq!(remainder(&a.plans[0]), Some(2.5));
        a.set_leg("b", &LegState::Live("2".into()));
        a.set_leg("c", &LegState::Filled);
        assert_eq!(remainder(&a.plans[0]), None);
        // at most ten plans are kept
        for _ in 0..12 { a.record_plan(&[leg(Exchange::Binance, "x", 1.0)]); }
        assert_eq!(a.plans.len(), 10);
    }

    #[test]
    fn kill_plan_lists_every_venue_step() {
        let mut a = Account::default();
        let k = Keys { key: "k".into(), secret: "s".into(), extra: None, demo: false };
        a.keys.insert(Exchange::Bybit, k.clone());
        a.keys.insert(Exchange::Binance, k);
        a.orders.push(order("1", None, 0.0));
        a.orders.push(order("2", None, 0.0));
        a.positions.push(pos(Side::Buy, 1.0));
        a.positions.push(Position { ex: Exchange::Bybit, ..pos(Side::Sell, 2.0) });
        a.tpsl_wait.push(wait("t1al0"));
        a.algos.push(AlgoJob { id: 1, ex: Exchange::Bybit, symbol: "BTCUSDT".into(), buy: true, close: false, total: 1.0, sent: 0.0, slices: 2, done: 0, started_ms: 0, end_ms: 1, status: "running".into(), cancelled: false, venue_id: None });
        // a venue without keys is not touched
        a.orders.push(OpenOrder { ex: Exchange::Okx, ..order("3", None, 0.0) });
        assert_eq!(kill_plan(&a), vec![(Exchange::Binance, 2, 0, 1, 1), (Exchange::Bybit, 0, 0, 1, 1)]);
    }

    #[test]
    fn kill_closes_capped_first_then_the_rest_at_market() {
        let capped = vec![(OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: true, kind: trade::Kind::Limit { price: 99.9, tif: trade::Tif::Ioc }, qty: 0.5, client_id: Some("t1kl0".into()), attach: None }, 100.0)];
        let other = Position { symbol: "ETHUSDT".into(), mark: 0.0, entry: 50.0, ..pos(Side::Sell, 3.0) };
        // the venue holds 0.6 of the capped symbol (more than the screen knew) and an ETH short
        let (closes, limited) = kill_closes(&[pos(Side::Buy, 0.6), other.clone(), pos(Side::Sell, 0.0)], &capped);
        assert!(limited && closes.len() == 2);
        assert_eq!((closes[0].0.qty, closes[0].0.kind, closes[0].0.close, closes[0].1), (0.6, capped[0].0.kind, true, 100.0));
        assert_eq!((closes[1].0.symbol.as_str(), closes[1].0.kind, closes[1].0.pos, closes[1].0.close, closes[1].1), ("ETHUSDT", trade::Kind::Market, Side::Sell, true, 50.0));
        // the IOC left 0.2: that goes at market; the ETH short (market) is not sent twice
        let rest = kill_rest(&[pos(Side::Buy, 0.2), other], &capped);
        assert_eq!(rest.len(), 1);
        assert_eq!((rest[0].0.qty, rest[0].0.kind, rest[0].0.close), (0.2, trade::Kind::Market, true));
        assert!(kill_rest(&[], &capped).is_empty());
    }

    #[test]
    fn reverse_one_way_is_one_order_of_2x() {
        let r = reverse_reqs(Some(Mode::OneWay), &pos(Side::Buy, 0.3));
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].pos, r[0].close, r[0].qty, r[0].side(), r[0].kind), (Side::Sell, false, 0.6, Side::Sell, trade::Kind::Market));
    }

    #[test]
    fn reverse_hedge_is_close_then_open() {
        for mode in [Some(Mode::Hedge), None] {
            let r = reverse_reqs(mode, &pos(Side::Sell, 2.0));
            assert_eq!(r.len(), 2);
            // close the short (a buy), then open a long of the same size (a buy)
            assert_eq!((r[0].pos, r[0].close, r[0].qty, r[0].side()), (Side::Sell, true, 2.0, Side::Buy));
            assert_eq!((r[1].pos, r[1].close, r[1].qty, r[1].side()), (Side::Buy, false, 2.0, Side::Buy));
        }
    }

    #[test]
    fn borrow_rate_reads_only_when_an_hour_old() {
        let cache = HashMap::from([("BTC".to_string(), (0.1, 1_000_000)), ("USDT".to_string(), (0.2, 1_000_000 - BORROW_TTL_MS))]);
        // BTC is fresh, USDT is over an hour old, ETH was never read
        assert_eq!(stale_borrow(&cache, &["BTC", "USDT", "ETH"], 1_000_000 + 1_000), vec!["USDT".to_string(), "ETH".to_string()]);
        assert_eq!(stale_borrow(&cache, &["BTC"], 1_000_000 + BORROW_TTL_MS - 1), Vec::<String>::new());
        assert_eq!(stale_borrow(&cache, &["BTC"], 1_000_000 + BORROW_TTL_MS), vec!["BTC".to_string()]);
    }

    #[test]
    fn rest_snapshots_merge_into_pushed_positions() {
        let ex = Exchange::Kraken;
        let mut a = Account::default();
        let pushed = Position { ex, liq: Some(80.0), lev: 10.0, margin: 50.0, mark: 101.0, cross: Some(true), ..pos(Side::Buy, 1.0) };
        a.positions.push(pushed);
        a.positions.push(Position { ex: Exchange::Bybit, ..pos(Side::Buy, 3.0) });
        // Kraken REST row: mark 0, liq None, margin 0; another venue's row is untouched
        replace_positions(&mut a, ex, vec![Position { ex, upnl: 2.0, ..pos(Side::Buy, 1.0) }]);
        let k = a.positions.iter().find(|p| p.ex == ex).unwrap();
        assert_eq!((k.mark, k.liq, k.margin, k.lev, k.cross, k.upnl), (101.0, Some(80.0), 50.0, 10.0, Some(true), 2.0));
        assert_eq!(a.positions.iter().filter(|p| p.ex == Exchange::Bybit).map(|p| p.qty).collect::<Vec<_>>(), vec![3.0]);
        // a position closed meanwhile is gone after the snapshot
        replace_positions(&mut a, ex, vec![]);
        assert!(a.positions.iter().all(|p| p.ex != ex));
        // only Binance refetches after a push without a liquidation price
        let p = |qty| Position { ex, ..pos(Side::Buy, qty) };
        assert!(!wants_liq_refetch(Exchange::Kraken, &p(1.0)) && !wants_liq_refetch(Exchange::Bybit, &p(1.0)));
        assert!(wants_liq_refetch(Exchange::Binance, &p(1.0)));
        assert!(!wants_liq_refetch(Exchange::Binance, &p(0.0)));
        assert!(!wants_liq_refetch(Exchange::Binance, &Position { liq: Some(1.0), ..p(1.0) }));
    }

    #[test]
    fn wallet_push_keeps_risk_fields_it_omits() {
        let mut a = Account::default();
        let ex = Exchange::Bybit;
        let full = trade::Balance { equity: 1000.0, available: 800.0, uni_mmr: Some(3.0), mm_rate: Some(0.05), maint_margin: Some(50.0), adj_equity: Some(990.0) };
        apply(&mut a, ex, E::Wallet(full));
        // a push with totals only: equity / available replace, the risk fields stay
        apply(&mut a, ex, E::Wallet(trade::Balance { equity: 900.0, available: 700.0, ..Default::default() }));
        let b = &a.balances[&ex];
        assert_eq!((b.equity, b.available, b.uni_mmr, b.mm_rate, b.maint_margin, b.adj_equity), (900.0, 700.0, Some(3.0), Some(0.05), Some(50.0), Some(990.0)));
        // a push that carries them replaces
        apply(&mut a, ex, E::Wallet(trade::Balance { equity: 900.0, available: 700.0, mm_rate: Some(0.07), adj_equity: Some(880.0), ..Default::default() }));
        let b = &a.balances[&ex];
        assert_eq!((b.mm_rate, b.adj_equity, b.maint_margin), (Some(0.07), Some(880.0), Some(50.0)));
        // stream down / liquidation notices are logged, not state
        assert!(!apply(&mut a, ex, E::Down));
        assert!(!apply(&mut a, ex, E::Liquidated { symbol: "BTCUSDT".into(), side: Side::Buy, qty: 1.0, price: 90.0 }));
    }
}

/// Every USDT/USDC-quoted base on the major venues (Binance futures + spot, Bybit linear, OKX swaps,
/// Hyperliquid perps), merged by base: price and 24h change from the first venue that lists it,
/// quote volume summed. A venue that fails is skipped.
async fn all_tickers() -> (Vec<Ticker>, HashMap<(Exchange, String), VenueFunding>) {
    use depth::{num, ws};
    let ok = |b: &str| !b.is_empty() && !b.contains('_') && !b.contains('-') && b.is_ascii();
    // a venue's own name back to the unified (Binance) one (Bybit AMDSTOCK -> AMD); None when its
    // same-name market is another asset (crate alias table), so it does not merge into that row
    let unify = |ex: Exchange, b: &str| { let u = depth::alias::unified_base(ex, b); depth::alias::venue_base(ex, Market::Perp, &u).is_some().then_some(u) };
    let hl_req = serde_json::json!({"type": "metaAndAssetCtxs"});
    let mut fund: HashMap<(Exchange, String), VenueFunding> = HashMap::new();
    // Binance: rates and next times for all perps; intervals other than 8h are listed in fundingInfo
    let (pi, fi) = tokio::join!(ws::get_json("https://fapi.binance.com/fapi/v1/premiumIndex"), ws::get_json("https://fapi.binance.com/fapi/v1/fundingInfo"));
    let mut bin_iv: HashMap<String, f64> = HashMap::new();
    if let Ok(v) = fi { for x in v.as_array().into_iter().flatten() { if let Some(s) = x["symbol"].as_str() { bin_iv.insert(s.into(), num(&x["fundingIntervalHours"])); } } }
    if let Ok(v) = pi {
        for x in v.as_array().into_iter().flatten() {
            if let Some(s) = x["symbol"].as_str() {
                fund.insert((Exchange::Binance, s.into()), VenueFunding { rate: num(&x["lastFundingRate"]), interval_h: bin_iv.get(s).copied().filter(|h| *h > 0.0).unwrap_or(8.0), next_ms: num(&x["nextFundingTime"]) as i64 });
            }
        }
    }
    let (bf, bs, by, ox, hl) = tokio::join!(
        ws::get_json("https://fapi.binance.com/fapi/v1/ticker/24hr"),
        ws::get_json("https://api.binance.com/api/v3/ticker/24hr?type=MINI"),
        ws::get_json("https://api.bybit.com/v5/market/tickers?category=linear"),
        ws::get_json("https://www.okx.com/api/v5/market/tickers?instType=SWAP"),
        ws::post_json("https://api.hyperliquid.xyz/info", &hl_req),
    );
    // (base, last, change %, quote vol, high, low, base vol)
    let mut rows: Vec<(String, f64, f64, f64, f64, f64, f64)> = vec![];
    let mut push = |base: &str, last: f64, chg: f64, vol: f64, hi: f64, lo: f64, bvol: f64| if ok(base) && last > 0.0 { rows.push((base.to_string(), last, chg, vol, hi, lo, bvol)); };
    match bf {
        Ok(v) => for x in v.as_array().into_iter().flatten() {
            if let Some(b) = x["symbol"].as_str().and_then(|s| s.strip_suffix("USDT")) { push(b, num(&x["lastPrice"]), num(&x["priceChangePercent"]), num(&x["quoteVolume"]), num(&x["highPrice"]), num(&x["lowPrice"]), num(&x["volume"])); }
        },
        Err(e) => eprintln!("[tickers binance futures] {e:#}"),
    }
    match bs {
        // MINI has no change percent: derive it from the open
        Ok(v) => for x in v.as_array().into_iter().flatten() {
            if let Some(b) = x["symbol"].as_str().and_then(|s| s.strip_suffix("USDT")) {
                let (l, o) = (num(&x["lastPrice"]), num(&x["openPrice"]));
                push(b, l, if o > 0.0 { (l / o - 1.0) * 100.0 } else { 0.0 }, num(&x["quoteVolume"]), num(&x["highPrice"]), num(&x["lowPrice"]), num(&x["volume"]));
            }
        },
        Err(e) => eprintln!("[tickers binance spot] {e:#}"),
    }
    match by {
        Ok(v) => for x in v["result"]["list"].as_array().into_iter().flatten() {
            if let Some(sym) = x["symbol"].as_str() {
                if !x["fundingRate"].is_null() && x["fundingRate"] != "" {
                    fund.insert((Exchange::Bybit, sym.into()), VenueFunding { rate: num(&x["fundingRate"]), interval_h: { let h = num(&x["fundingIntervalHour"]); if h > 0.0 { h } else { 8.0 } }, next_ms: num(&x["nextFundingTime"]) as i64 });
                }
            }
            if let Some(b) = x["symbol"].as_str().and_then(|s| s.strip_suffix("USDT")).and_then(|b| unify(Exchange::Bybit, b)) { push(&b, num(&x["lastPrice"]), num(&x["price24hPcnt"]) * 100.0, num(&x["turnover24h"]), num(&x["highPrice24h"]), num(&x["lowPrice24h"]), num(&x["volume24h"])); }
        },
        Err(e) => eprintln!("[tickers bybit] {e:#}"),
    }
    match ox {
        Ok(v) => for x in v["data"].as_array().into_iter().flatten() {
            if let Some(b) = x["instId"].as_str().and_then(|s| s.strip_suffix("-USDT-SWAP")).and_then(|b| unify(Exchange::Okx, b)) {
                let (l, o) = (num(&x["last"]), num(&x["open24h"]));
                // volCcy24h is in base units for USDT swaps
                push(&b, l, if o > 0.0 { (l / o - 1.0) * 100.0 } else { 0.0 }, num(&x["volCcy24h"]) * l, num(&x["high24h"]), num(&x["low24h"]), num(&x["volCcy24h"]));
            }
        },
        Err(e) => eprintln!("[tickers okx] {e:#}"),
    }
    match hl {
        Ok(v) => {
            let names = v[0]["universe"].as_array().cloned().unwrap_or_default();
            for (u, c) in names.iter().zip(v[1].as_array().into_iter().flatten()) {
                let (l, p) = (num(&c["markPx"]), num(&c["prevDayPx"]));
                // Hyperliquid funds hourly
                if let Some(b) = u["name"].as_str().and_then(|b| unify(Exchange::Hyperliquid, b)) {
                    fund.insert((Exchange::Hyperliquid, format!("{b}USDT")), VenueFunding { rate: num(&c["funding"]), interval_h: 1.0, next_ms: (depth::now_ms() / 3_600_000 + 1) * 3_600_000 });
                    push(&b, l, if p > 0.0 { (l / p - 1.0) * 100.0 } else { 0.0 }, num(&c["dayNtlVlm"]), 0.0, 0.0, num(&c["dayBaseVlm"]));
                }
            }
        }
        Err(e) => eprintln!("[tickers hyperliquid] {e:#}"),
    }
    let mut by_base: std::collections::HashMap<String, Ticker> = Default::default();
    for (b, last, chg, vol, hi, lo, bvol) in rows {
        by_base.entry(b.clone())
            .and_modify(|t| { t.quote_vol += vol; t.base_vol += bvol; if t.high <= 0.0 { t.high = hi; t.low = lo; } })
            .or_insert(Ticker { base: b, last, chg_pct: chg, quote_vol: vol, high: hi, low: lo, base_vol: bvol });
    }
    let mut t: Vec<Ticker> = by_base.into_values().collect();
    t.sort_by(|a, b| b.quote_vol.total_cmp(&a.quote_vol));
    (t, fund)
}
