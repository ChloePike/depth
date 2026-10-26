//! The fill model: what the chosen venue's OWN book and trades would do to a demo order. Pure
//! functions over levels and prices, and the `Sim` methods that record their outcome as events
//! (all checks run before the first event, so a refused order leaves no trace).
//!
//! - market orders walk the venue's book, one fill per level at the taker fee; a book that holds
//!   less than the size is refused, never part-filled in silence
//! - a limit that crosses on arrival takes the book up to its price (taker); the rest rests (GTC),
//!   is cancelled (IOC), or the order is refused (FOK all-or-nothing; post-only would take)
//! - a resting limit fills only when a trade prints strictly through its price: all of it at once,
//!   at its price, maker fee. ponytail: we are assumed LAST in the queue at our price (a print at
//!   the limit may have been the orders ahead of us) and the print's size is ignored; a queue
//!   model needs the volume traded at the level since the order joined it
//! - BBO orders are priced once from the book at arrival, then are plain GTC limits
//! - stops fire on mark or last as the order says; trailing stops track the last price (Binance's
//!   default working type, Bybit's trailing stop); TP/SL on mark or last per `tpsl::Trigger`
//! - close / reduce-only never exceeds the position; a side gone flat takes its close orders and
//!   TP/SL with it (OCO)
//! - funding at the rate per settlement the caller passes (the venue's own rate and interval);
//!   liquidation at maintenance margin (`liquidate`, run from the caller's tick)
use super::{coin, limit_price, opposite, sign, DemoOrder, EvKind, Pos, PosKey, Sim, MMR};
use crate::trade::{self, tpsl::{self, TpSl, Trigger}, Kind, OrderReq, Rules, Tif};
use crate::{route, Exchange, Market, Side};
use anyhow::{anyhow, bail, Result};

/// One venue's own raw book, best level first (native quote, base units), as its feed holds it.
#[derive(Clone, Copy, Debug)]
pub struct Levels<'a> { pub bids: &'a [(f64, f64)], pub asks: &'a [(f64, f64)] }

/// Per-level executions taking `qty` from `levels` (the opposite side, best first), never past
/// `limit` (a buy takes asks up to it, a sell bids down to it).
pub fn take(levels: &[(f64, f64)], qty: f64, limit: Option<f64>, buy: bool) -> Vec<(f64, f64)> {
    let (mut left, mut out) = (qty, vec![]);
    for &(px, q) in levels {
        if left <= qty * 1e-9 || limit.is_some_and(|l| !crosses(buy, l, px)) { break; }
        let t = q.min(left);
        out.push((px, t));
        left -= t;
    }
    out
}

/// Whether a limit at `price` takes a book whose best opposite level is `best`.
pub fn crosses(buy: bool, price: f64, best: f64) -> bool { if buy { price >= best } else { price <= best } }

/// A resting limit fills only on a print strictly through it (see the module notes).
pub fn through(buy: bool, limit: f64, trade_px: f64) -> bool { if buy { trade_px < limit } else { trade_px > limit } }

/// A conditional order with direction `dir` (1 rising to the trigger, -1 falling) hit at `px`.
pub fn stop_hit(dir: i8, trigger: f64, px: f64) -> bool { if dir > 0 { px >= trigger } else { px <= trigger } }

/// A long's take-profit sits above the price and its stop-loss below; a short's mirrored.
pub fn tpsl_hit(pos: Side, take_profit: bool, trigger_px: f64, px: f64) -> bool {
    if (pos == Side::Buy) == take_profit { px >= trigger_px } else { px <= trigger_px }
}

/// One trailing-stop step at `px`: the best price since activation (None until `activation` is
/// reached; a sell tracks the high, a buy the low) and whether the retrace reached `callback_pct`.
pub fn trailing(buy: bool, best: Option<f64>, activation: Option<f64>, callback_pct: f64, px: f64) -> (Option<f64>, bool) {
    if best.is_none() && activation.is_some_and(|a| if buy { px > a } else { px < a }) { return (None, false); }
    let b = best.map_or(px, |b| if buy { b.min(px) } else { b.max(px) });
    let retrace = (if buy { px - b } else { b - px }) / b * 100.0;
    (Some(b), retrace >= callback_pct - 1e-9)
}

fn filled(levels: &[(f64, f64)]) -> f64 { levels.iter().map(|l| l.1).sum() }

impl Sim {
    /// (taker, maker) fee fractions of `ex`.
    pub fn fee(&self, ex: Exchange) -> (f64, f64) { self.fees.get(&ex).copied().unwrap_or(route::DEFAULT_FEES) }

    /// A new order on `ex`'s Perp market (or Spot on the spot venues; `req.pos` is then the side).
    /// `r` / `ref_px` are the venue's real rules and the reference price: size and prices go on its
    /// grid as they would live. `book` = the venue's own fresh book, None when stale or not streamed
    /// (market and crossing orders are then refused). Returns the order id.
    #[allow(clippy::too_many_arguments)]
    pub fn place(&mut self, ex: Exchange, market: Market, req: &OrderReq, r: &Rules, ref_px: f64, book: Option<Levels>, now: i64) -> Result<String> {
        let mut req = req.clone();
        let (q, px) = match market {
            Market::Perp if trade::TRADABLE.contains(&ex) => { trade::accepts(ex, &req)?; trade::checked(&req, r, ref_px)? }
            Market::Spot if trade::spot::VENUES.contains(&ex) => { req.close = false; trade::spot::checked(req.kind, req.qty, r, ref_px)? }
            _ => bail!("demo: {ex:?} {market:?} trading is not simulated"),
        };
        req.qty = q.parse()?;
        if let (Some(p), Kind::Limit { price, .. } | Kind::Stop { limit: Some(price), .. }) = (px, &mut req.kind) { *price = p.parse()?; }
        let symbol = req.symbol.clone();
        let buy = req.side() == Side::Buy;
        let (opp, own) = match book { Some(b) if buy => (b.asks, b.bids), Some(b) => (b.bids, b.asks), None => (&[][..], &[][..]) };
        let no_book = || anyhow!("demo: no live {ex:?} book for {symbol}; open it on the chart");
        if let Kind::Bbo { queue, level } = req.kind {
            let &(p, _) = (if queue { own } else { opp }).get(level as usize - 1).ok_or_else(no_book)?;
            req.kind = Kind::Limit { price: p, tif: Tif::Gtc };
        }
        if req.close {
            let held = self.held(ex, &symbol, req.pos);
            if held <= 0.0 { bail!("demo: no {} {symbol} position on {ex:?} to close", if req.pos == Side::Buy { "long" } else { "short" }); }
            req.qty = req.qty.min(held);
        }
        // what executes on arrival
        let (mut takes, mut dir, mut expire) = (vec![], 0, false);
        match req.kind {
            Kind::Market => {
                if book.is_none() { return Err(no_book()); }
                takes = take(opp, req.qty, None, buy);
                let got = filled(&takes);
                if got < req.qty * (1.0 - 1e-9) { bail!("demo: the {ex:?} book shows only {got} {symbol} within {} levels", opp.len()); }
            }
            Kind::Limit { price, tif } => {
                let cross = crosses(buy, price, opp.first().map_or(ref_px, |l| l.0));
                if cross && book.is_none() { return Err(no_book()); }
                if cross && tif == Tif::PostOnly { bail!("demo: post-only order would take liquidity"); }
                if cross { takes = take(opp, req.qty, Some(price), buy); }
                let got = filled(&takes);
                if tif == Tif::Fok && got < req.qty * (1.0 - 1e-9) { bail!("demo: fill-or-kill: only {got} {symbol} at {price} or better"); }
                expire = tif == Tif::Ioc && got < req.qty * (1.0 - 1e-9);
            }
            Kind::Stop { trigger, by_mark, .. } => {
                let cur = if by_mark { self.marks.get(&(ex, symbol.clone())) } else { self.lasts.get(&(ex, market, symbol.clone())) };
                let cur = cur.copied().unwrap_or(ref_px);
                if trigger == cur { bail!("demo: trigger {trigger} is at the current price"); }
                dir = if trigger > cur { 1 } else { -1 };
            }
            Kind::Trailing { .. } | Kind::Bbo { .. } => {}
        }
        // conditional orders hold no margin until they fire
        match req.kind {
            Kind::Stop { .. } | Kind::Trailing { .. } => {}
            Kind::Limit { price, .. } => self.afford(ex, market, &req, price)?,
            _ => self.afford(ex, market, &req, route::walk(&takes, f64::INFINITY).0)?,
        }
        let id = trade::uuid_v4();
        let o = DemoOrder { id: id.clone(), ex, market, req, filled: 0.0, avg: 0.0, ts: now, dir };
        self.record(now, EvKind::Place { o: o.clone() });
        self.fill(&o, &takes, false, now);
        if expire { self.record(now, EvKind::Cancel { id: id.clone() }); }
        if market == Market::Perp { self.tidy(ex, &symbol, now); }
        Ok(id)
    }

    /// Refuses what the pool cannot carry: the margin of what a perp order opens (one-way nets the
    /// opposite position first), the USDT a spot buy costs with its fee, the coins a spot sell needs.
    fn afford(&self, ex: Exchange, market: Market, req: &OrderReq, px: f64) -> Result<()> {
        let avail = self.available();
        if market == Market::Spot {
            if req.side() == Side::Buy {
                let need = req.qty * px * (1.0 + self.fee(ex).0);
                if need > avail + 1e-9 { bail!("demo: insufficient USDT: {need:.2} > {avail:.2}"); }
            } else {
                let c = coin(&req.symbol);
                let free = self.coins.get(&(ex, c.to_string())).map_or(0.0, |x| x.0) - self.selling(ex, c);
                if free < req.qty * (1.0 - 1e-9) { bail!("demo: insufficient {c}: {free} < {}", req.qty); }
            }
        } else if !req.close {
            let netted = if self.hedge { 0.0 } else { self.held(ex, &req.symbol, opposite(req.pos)) };
            let need = (req.qty - netted).max(0.0) * px / self.lev(ex, &req.symbol);
            if need > avail + 1e-9 { bail!("demo: insufficient margin: {need:.2} > {avail:.2} USDT"); }
        }
        Ok(())
    }

    /// Records `o`'s executions, one per level, at the taker or maker fee.
    fn fill(&mut self, o: &DemoOrder, levels: &[(f64, f64)], maker: bool, now: i64) {
        let rate = if maker { self.fee(o.ex).1 } else { self.fee(o.ex).0 };
        for &(px, qty) in levels {
            self.record(now, EvKind::Fill { order: o.id.clone(), ex: o.ex, market: o.market, symbol: o.req.symbol.clone(), side: o.req.side(), pos: o.req.pos, px, qty, fee: px * qty * rate });
        }
    }

    /// A side gone flat takes its close orders and TP/SL with it, as the venues cancel them.
    fn tidy(&mut self, ex: Exchange, symbol: &str, now: i64) {
        let flat = |s: &Self, side| s.held(ex, symbol, side) == 0.0;
        let gone: Vec<String> = self.orders.iter().filter(|o| o.ex == ex && o.market == Market::Perp && o.req.symbol == symbol && o.req.close && flat(self, o.req.pos)).map(|o| o.id.clone())
            .chain(self.tpsl.iter().filter(|t| t.ex == ex && t.symbol == symbol && flat(self, t.pos)).map(|t| t.id.clone())).collect();
        for id in gone { self.record(now, EvKind::Cancel { id }); }
    }

    /// A triggered order's market execution: the book's levels (the part past its copied depth at
    /// the last level); off the book (stale / not streamed) all at the triggering price.
    /// ponytail: no slippage model off the book or past the copied depth
    fn sweep(&self, buy: bool, qty: f64, px: f64, book: Option<Levels>) -> Vec<(f64, f64)> {
        let Some(b) = book else { return vec![(px, qty)] };
        let mut v = take(if buy { b.asks } else { b.bids }, qty, None, buy);
        let rest = qty - filled(&v);
        if rest > qty * 1e-9 { match v.last_mut() { Some(l) => l.1 += rest, None => v.push((px, rest)) } }
        v
    }

    /// Cancel an order or a TP/SL.
    pub fn cancel(&mut self, id: &str, now: i64) -> Result<()> {
        if !self.orders.iter().any(|o| o.id == id) && !self.tpsl.iter().any(|t| t.id == id) { bail!("demo: order {id} not found (filled or cancelled)"); }
        self.record(now, EvKind::Cancel { id: id.into() });
        Ok(())
    }

    /// Re-price a resting limit (on the `tick` grid). A price that now crosses takes the book like a
    /// new order would; post-only is refused instead.
    pub fn amend(&mut self, id: &str, price: f64, tick: f64, book: Option<Levels>, now: i64) -> Result<()> {
        let price: f64 = trade::fmt_step(price, tick, false).parse()?;
        if !price.is_finite() || price <= 0.0 { bail!("invalid price {price}"); }
        let Some(o) = self.orders.iter().find(|o| o.id == id).cloned() else { bail!("demo: order {id} not found (filled or cancelled)") };
        let Kind::Limit { price: old, tif } = o.req.kind else { bail!("demo: only resting limit orders can be amended") };
        let buy = o.req.side() == Side::Buy;
        let opp = book.map_or(&[][..], |b| if buy { b.asks } else { b.bids });
        let last = self.lasts.get(&(o.ex, o.market, o.req.symbol.clone())).copied();
        let Some(best) = opp.first().map(|l| l.0).or(last) else { bail!("demo: no live {:?} price for {}", o.ex, o.req.symbol) };
        let cross = crosses(buy, price, best);
        if cross && book.is_none() { bail!("demo: no live {:?} book for {}; open it on the chart", o.ex, o.req.symbol); }
        if cross && tif == Tif::PostOnly { bail!("demo: post-only order would take liquidity"); }
        // what a higher price adds to the order's reserve (as `Sim::reserved` counts it)
        let left = o.req.qty - o.filled;
        let per = match o.market { Market::Spot if buy => 1.0, Market::Spot => 0.0, _ if o.req.close => 0.0, _ => 1.0 / self.lev(o.ex, &o.req.symbol) };
        let more = (price - old) * left * per;
        if more > self.available() + 1e-9 { bail!("demo: insufficient margin: {more:.2} > {:.2} USDT", self.available()); }
        self.record(now, EvKind::Amend { id: id.into(), price });
        if cross {
            let left = if o.req.close { left.min(self.held(o.ex, &o.req.symbol, o.req.pos)) } else { left };
            self.fill(&o, &take(opp, left, Some(price), buy), false, now);
            self.tidy(o.ex, &o.req.symbol, now);
        }
        Ok(())
    }

    /// Set a TP/SL on an open position (Bybit / Binance only, as live), checked against the live
    /// mark or last price like the venues do.
    pub fn set_tpsl(&mut self, t: TpSl, now: i64) -> Result<()> {
        if !trade::caps(t.ex).tpsl { bail!("{:?}: TP/SL is not supported here yet", t.ex); }
        if self.held(t.ex, &t.symbol, t.pos) <= 0.0 { bail!("demo: no position to protect"); }
        let px = match t.trigger { Trigger::Mark => self.marks.get(&(t.ex, t.symbol.clone())), Trigger::Last => self.lasts.get(&(t.ex, Market::Perp, t.symbol.clone())) };
        if let Some(&px) = px { tpsl::validate(t.pos, px, t.take_profit, t.trigger_px)?; }
        self.record(now, EvKind::TpSl { t });
        Ok(())
    }

    /// A trade printed on `ex`'s `market`: resting limits it went strictly through fill, stops,
    /// trailing stops and TP/SL watching the last price fire. `book` as for `place`.
    pub fn on_trade(&mut self, ex: Exchange, market: Market, symbol: &str, px: f64, book: Option<Levels>, now: i64) {
        self.lasts.insert((ex, market, symbol.to_string()), px);
        if self.orders.is_empty() && self.tpsl.is_empty() { return; }
        let hit: Vec<DemoOrder> = self.orders.iter().filter(|o| o.ex == ex && o.market == market && o.req.symbol == symbol
            && matches!(o.req.kind, Kind::Limit { price, .. } if through(o.req.side() == Side::Buy, price, px))).cloned().collect();
        for o in &hit {
            let mut left = o.req.qty - o.filled;
            if o.req.close { left = left.min(self.held(ex, symbol, o.req.pos)); }
            if left > 0.0 { self.fill(o, &[(limit_price(&o.req.kind), left)], true, now); }
        }
        if market == Market::Perp {
            if !hit.is_empty() { self.tidy(ex, symbol, now); }
            self.triggers(ex, symbol, px, false, book, now);
        }
    }

    /// A venue mark: stops and TP/SL watching the mark fire. Liquidation is the caller's tick.
    pub fn on_mark(&mut self, ex: Exchange, symbol: &str, mark: f64, book: Option<Levels>, now: i64) {
        self.marks.insert((ex, symbol.to_string()), mark);
        if !self.orders.is_empty() || !self.tpsl.is_empty() { self.triggers(ex, symbol, mark, true, book, now); }
    }

    fn triggers(&mut self, ex: Exchange, symbol: &str, px: f64, by_mark: bool, book: Option<Levels>, now: i64) {
        let mut fire = vec![];
        for o in self.orders.iter().filter(|o| o.ex == ex && o.market == Market::Perp && o.req.symbol == symbol) {
            match o.req.kind {
                Kind::Stop { trigger, by_mark: m, .. } => if m == by_mark && stop_hit(o.dir, trigger, px) { fire.push(o.id.clone()) },
                Kind::Trailing { callback_pct, activation } if !by_mark => {
                    let (best, hit) = trailing(o.req.side() == Side::Buy, self.trail.get(&o.id).copied(), activation, callback_pct, px);
                    if let Some(b) = best { self.trail.insert(o.id.clone(), b); }
                    if hit { fire.push(o.id.clone()); }
                }
                _ => {}
            }
        }
        for id in fire { self.fire(&id, px, book, now); }
        let hits: Vec<TpSl> = self.tpsl.iter().filter(|t| t.ex == ex && t.symbol == symbol && (t.trigger == Trigger::Mark) == by_mark
            && tpsl_hit(t.pos, t.take_profit, t.trigger_px, px)).cloned().collect();
        // an earlier one may have closed the side (and so removed the others)
        for t in hits { if self.tpsl.iter().any(|x| x.id == t.id) { self.close_tpsl(&t, px, book, now); } }
    }

    /// A stop / trailing order fired at `px`: now a market order, or a limit at its limit price whose
    /// crossing part takes the book at once. The venue refuses a fired order the account cannot carry.
    fn fire(&mut self, id: &str, px: f64, book: Option<Levels>, now: i64) {
        self.record(now, EvKind::Trigger { id: id.into(), px });
        self.trail.remove(id);
        let Some(o) = self.orders.iter().find(|o| o.id == id).cloned() else { return };
        let buy = o.req.side() == Side::Buy;
        let mut left = o.req.qty - o.filled;
        if o.req.close { left = left.min(self.held(o.ex, &o.req.symbol, o.req.pos)); }
        let takes = match o.req.kind {
            Kind::Limit { price, .. } => book.map_or(vec![], |b| take(if buy { b.asks } else { b.bids }, left, Some(price), buy)),
            _ => self.sweep(buy, left, px, book),
        };
        let est = if takes.is_empty() { limit_price(&o.req.kind) } else { route::walk(&takes, f64::INFINITY).0 };
        if left <= 0.0 || self.afford(o.ex, o.market, &OrderReq { qty: left, ..o.req.clone() }, est).is_err() {
            self.record(now, EvKind::Cancel { id: id.into() });
            return;
        }
        self.fill(&o, &takes, false, now);
        // a market order never rests (a close clamped to what was held)
        if o.req.kind == Kind::Market && self.orders.iter().any(|x| x.id == id) { self.record(now, EvKind::Cancel { id: id.into() }); }
        self.tidy(o.ex, &o.req.symbol, now);
    }

    /// A TP/SL fired: a market close of its size (None = the whole side) at the taker fee, shown in
    /// the order history as a market order; the record goes.
    fn close_tpsl(&mut self, t: &TpSl, px: f64, book: Option<Levels>, now: i64) {
        self.record(now, EvKind::Cancel { id: t.id.clone() });
        let held = self.held(t.ex, &t.symbol, t.pos);
        let qty = t.qty.map_or(held, |q| q.min(held));
        if qty <= 0.0 { return; }
        let req = OrderReq { symbol: t.symbol.clone(), pos: t.pos, close: true, kind: Kind::Market, qty, client_id: None, attach: None };
        let o = DemoOrder { id: t.id.clone(), ex: t.ex, market: Market::Perp, req, filled: 0.0, avg: 0.0, ts: now, dir: 0 };
        let levels = self.sweep(o.req.side() == Side::Buy, qty, px, book);
        self.record(now, EvKind::Place { o: o.clone() });
        self.fill(&o, &levels, false, now);
        self.tidy(t.ex, &t.symbol, now);
    }

    /// One funding settlement of (ex, symbol) at `rate` per settlement (the venue's own rate over
    /// its own interval) on the mark: a long pays a positive rate, a short receives it.
    pub fn funding(&mut self, ex: Exchange, symbol: &str, rate: f64, now: i64) {
        let rows: Vec<(Side, f64)> = self.pos.iter().filter(|(k, _)| k.0 == ex && k.1 == symbol).map(|(k, p)| (k.2, p.qty * self.mark_of(k, p))).collect();
        for (pos, notional) in rows {
            self.record(now, EvKind::Funding { ex, symbol: symbol.into(), pos, rate, amount: -sign(pos) * notional * rate });
        }
    }

    /// Liquidation at the current marks. Isolated: each position whose mark reached its liquidation
    /// price. Cross: every position on every venue once equity <= maintenance (one pool). The venue
    /// cancels the orders first and keeps the maintenance margin as its fee.
    /// ponytail: no partial liquidation, insurance fund or auto-deleveraging model
    pub fn liquidate(&mut self, now: i64) {
        if self.pos.is_empty() || (self.cross && self.equity() > self.maint()) { return; }
        let reached = |s: &Self, k: &PosKey, p: &Pos| s.marks.get(&(k.0, k.1.clone())).is_some_and(|&m| if k.2 == Side::Buy { m <= s.liq(k, p) } else { m >= s.liq(k, p) });
        let mut doomed: Vec<(PosKey, f64, f64, f64)> = self.pos.iter().filter(|(k, p)| self.cross || reached(self, k, p))
            .map(|(k, p)| (k.clone(), p.qty, self.mark_of(k, p), p.qty * p.entry * MMR)).collect();
        if doomed.is_empty() { return; }
        let cancels: Vec<String> = self.orders.iter().filter(|o| o.market == Market::Perp && doomed.iter().any(|d| d.0.0 == o.ex && d.0.1 == o.req.symbol)).map(|o| o.id.clone()).collect();
        for id in cancels { self.record(now, EvKind::Cancel { id }); }
        // profitable closes first: the cash floor in `apply` then equals the floor of the whole sum
        let pnl = |s: &Self, d: &(PosKey, f64, f64, f64)| (d.2 - s.pos[&d.0].entry) * d.1 * sign(d.0.2) - d.3;
        doomed.sort_by(|a, b| pnl(self, b).total_cmp(&pnl(self, a)));
        for (k, qty, px, fee) in doomed {
            self.record(now, EvKind::Liquidate { ex: k.0, symbol: k.1.clone(), pos: k.2, qty, px, fee });
            self.tidy(k.0, &k.1, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::{fill, sim, snap, BTC};
    use Side::{Buy, Sell};

    const R: Rules = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 0.0 };
    const BY: Exchange = Exchange::Bybit;

    fn req(pos: Side, close: bool, kind: Kind, qty: f64) -> OrderReq { OrderReq { symbol: BTC.into(), pos, close, kind, qty, client_id: None, attach: None } }
    fn lim(price: f64, tif: Tif) -> Kind { Kind::Limit { price, tif } }
    fn bk<'a>(bids: &'a [(f64, f64)], asks: &'a [(f64, f64)]) -> Option<Levels<'a>> { Some(Levels { bids, asks }) }
    fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-9 }

    /// 10,000 USDT, Bybit taker 0.1% / maker 0.05%.
    fn demo() -> Sim { let mut s = sim(10_000.0); s.fees.insert(BY, (0.001, 0.0005)); s }

    /// The live session folds back from its own log.
    fn replays(s: &Sim) { assert_eq!(snap(&Sim::replay(&s.device, s.pending.clone())), snap(s)); }

    fn fills(s: &Sim) -> Vec<(f64, f64)> { s.fills.iter().map(|f| (f.price, f.qty)).collect() }

    #[test]
    fn pure_helpers() {
        let asks = [(100.0, 1.0), (101.0, 1.0)];
        assert_eq!(take(&asks, 1.5, None, true), vec![(100.0, 1.0), (101.0, 0.5)]);
        assert_eq!(take(&asks, 1.5, Some(100.5), true), vec![(100.0, 1.0)]);
        assert_eq!(take(&[(99.0, 1.0), (98.0, 1.0)], 5.0, Some(98.0), false), vec![(99.0, 1.0), (98.0, 1.0)]);
        assert!(close(route::walk(&take(&asks, 1.5, None, true), f64::INFINITY).0, 150.5 / 1.5));
        assert!(through(true, 100.0, 99.9) && !through(true, 100.0, 100.0) && through(false, 100.0, 100.1) && !through(false, 100.0, 100.0));
        assert!(stop_hit(1, 110.0, 110.0) && !stop_hit(1, 110.0, 109.9) && stop_hit(-1, 90.0, 90.0) && !stop_hit(-1, 90.0, 90.1));
        assert!(tpsl_hit(Buy, true, 110.0, 110.0) && tpsl_hit(Buy, false, 90.0, 89.0) && tpsl_hit(Sell, true, 90.0, 90.0) && !tpsl_hit(Sell, false, 110.0, 109.0));
        // a sell trailing 1% from a best of 100
        assert_eq!(trailing(false, Some(100.0), None, 1.0, 99.1), (Some(100.0), false));
        assert_eq!(trailing(false, Some(100.0), None, 1.0, 99.0), (Some(100.0), true));
        assert_eq!(trailing(false, Some(100.0), None, 1.0, 101.0), (Some(101.0), false));
        // not active below its activation price, then tracks from there
        assert_eq!(trailing(false, None, Some(105.0), 1.0, 103.0), (None, false));
        assert_eq!(trailing(false, None, Some(105.0), 1.0, 105.0), (Some(105.0), false));
        assert_eq!(trailing(true, Some(100.0), None, 1.0, 101.0), (Some(100.0), true));
    }

    #[test]
    fn market_walks_the_book_or_is_refused() {
        let (bids, asks) = ([(99.0, 1.0)], [(100.0, 1.0), (101.0, 1.0)]);
        let mut s = demo();
        let id = s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 1.5), &R, 100.0, bk(&bids, &asks), 1).unwrap();
        assert_eq!(fills(&s), vec![(100.0, 1.0), (101.0, 0.5)]);
        let p = &s.positions(BY)[0];
        assert!(close(p.entry, 150.5 / 1.5) && p.qty == 1.5);
        assert!(close(s.cash, 10_000.0 - 0.1 - 0.0505));
        assert!(s.orders.is_empty() && s.hist[0].status == "Filled" && close(s.hist[0].avg, 150.5 / 1.5));
        assert_eq!(s.fills.iter().map(|f| f.realized).collect::<Vec<_>>(), vec![Some(0.0), Some(0.0)]);
        // thin book, stale book: refused, nothing recorded
        let n = s.pending.len();
        let e = s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 3.0), &R, 100.0, bk(&bids, &asks), 2).unwrap_err();
        assert!(e.to_string().contains("only 2"), "{e}");
        assert!(s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 0.1), &R, 100.0, None, 2).is_err());
        assert_eq!(s.pending.len(), n);
        // size floored to the venue step
        s.place(BY, Market::Perp, &req(Sell, false, Kind::Market, 0.12345), &R, 100.0, bk(&bids, &asks), 3).unwrap();
        assert_eq!(s.fills.last().unwrap().qty, 0.123);
        assert!(!id.is_empty());
        replays(&s);
    }

    #[test]
    fn crossing_limits_by_time_in_force() {
        let (bids, asks) = ([(99.0, 1.0)], [(100.0, 1.0), (102.0, 1.0)]);
        let b = || bk(&bids, &asks);
        // GTC: takes 1 @100, the rest rests @101
        let mut s = demo();
        let id = s.place(BY, Market::Perp, &req(Buy, false, lim(101.0, Tif::Gtc), 2.0), &R, 100.0, b(), 1).unwrap();
        assert_eq!(fills(&s), vec![(100.0, 1.0)]);
        let o = &s.open_orders(BY, Market::Perp)[0];
        assert_eq!((o.id.as_str(), o.price, o.qty, o.filled, o.tif.as_str()), (id.as_str(), 101.0, 2.0, 1.0, "GTC"));
        // IOC: the rest is cancelled
        let mut s = demo();
        s.place(BY, Market::Perp, &req(Buy, false, lim(101.0, Tif::Ioc), 2.0), &R, 100.0, b(), 1).unwrap();
        assert_eq!(fills(&s), vec![(100.0, 1.0)]);
        assert!(s.orders.is_empty() && s.hist[0].status == "PartiallyFilledCanceled");
        // an IOC that cannot fill is cancelled at once
        s.place(BY, Market::Perp, &req(Buy, false, lim(99.5, Tif::Ioc), 1.0), &R, 100.0, b(), 2).unwrap();
        assert!(s.orders.is_empty() && s.hist[1].status == "Cancelled");
        // FOK all-or-nothing, post-only never takes: refused, nothing recorded
        let n = s.pending.len();
        assert!(s.place(BY, Market::Perp, &req(Buy, false, lim(101.0, Tif::Fok), 2.0), &R, 100.0, b(), 3).is_err());
        assert!(s.place(BY, Market::Perp, &req(Buy, false, lim(101.0, Tif::PostOnly), 2.0), &R, 100.0, b(), 3).unwrap_err().to_string().contains("post-only"));
        assert_eq!(s.pending.len(), n);
        s.place(BY, Market::Perp, &req(Buy, false, lim(102.0, Tif::Fok), 2.0), &R, 100.0, b(), 4).unwrap();
        assert_eq!(fills(&s)[1..], [(100.0, 1.0), (102.0, 1.0)]);
        // post-only below the ask rests; a crossing limit without a live book is refused
        s.place(BY, Market::Perp, &req(Buy, false, lim(99.5, Tif::PostOnly), 1.0), &R, 100.0, b(), 5).unwrap();
        assert!(s.place(BY, Market::Perp, &req(Buy, false, lim(105.0, Tif::Gtc), 1.0), &R, 100.0, None, 5).is_err());
        s.place(BY, Market::Perp, &req(Buy, false, lim(90.0, Tif::Gtc), 1.0), &R, 100.0, None, 5).unwrap();
        assert_eq!(s.orders.len(), 2);
        replays(&s);
    }

    #[test]
    fn resting_limit_fills_only_through_its_price() {
        let (bids, asks) = ([(99.0, 1.0)], [(101.0, 1.0)]);
        let mut s = demo();
        s.place(BY, Market::Perp, &req(Buy, false, lim(100.0, Tif::Gtc), 2.0), &R, 100.0, bk(&bids, &asks), 1).unwrap();
        s.on_trade(BY, Market::Perp, BTC, 100.0, None, 2);
        assert!(s.fills.is_empty());
        s.on_trade(BY, Market::Perp, BTC, 99.9, None, 3);
        assert_eq!(fills(&s), vec![(100.0, 2.0)]);
        assert!(close(s.fills[0].fee, 200.0 * 0.0005) && s.orders.is_empty());
        // a sell (closing the long) rests above; another venue's or market's prints do not touch it
        s.place(BY, Market::Perp, &req(Buy, true, lim(105.0, Tif::Gtc), 1.0), &R, 100.0, bk(&bids, &asks), 4).unwrap();
        s.on_trade(Exchange::Okx, Market::Perp, BTC, 106.0, None, 5);
        s.on_trade(BY, Market::Spot, BTC, 106.0, None, 5);
        s.on_trade(BY, Market::Perp, BTC, 105.0, None, 5);
        assert_eq!(s.fills.len(), 1);
        s.on_trade(BY, Market::Perp, BTC, 105.1, None, 6);
        assert_eq!(fills(&s)[1], (105.0, 1.0));
        assert_eq!(s.fills[1].realized, Some(5.0));
        replays(&s);
    }

    #[test]
    fn bbo_prices_from_the_book_at_arrival() {
        let (bids, asks) = ([(99.0, 1.0), (98.0, 1.0)], [(100.0, 1.0), (101.0, 1.0)]);
        let mut s = demo();
        // queue: joins our own side's best
        s.place(BY, Market::Perp, &req(Buy, false, Kind::Bbo { queue: true, level: 1 }, 1.0), &R, 100.0, bk(&bids, &asks), 1).unwrap();
        assert_eq!((s.open_orders(BY, Market::Perp)[0].price, s.fills.len()), (99.0, 0));
        // opponent level 2: priced at the second ask, takes up to it
        s.place(BY, Market::Perp, &req(Buy, false, Kind::Bbo { queue: false, level: 2 }, 3.0), &R, 100.0, bk(&bids, &asks), 2).unwrap();
        assert_eq!(fills(&s), vec![(100.0, 1.0), (101.0, 1.0)]);
        assert_eq!(s.orders.iter().map(|o| limit_price(&o.req.kind)).collect::<Vec<_>>(), vec![99.0, 101.0]);
        // venue levels as live: Bybit 1-5 only; no book
        assert!(s.place(BY, Market::Perp, &req(Buy, false, Kind::Bbo { queue: true, level: 10 }, 1.0), &R, 100.0, bk(&bids, &asks), 3).is_err());
        assert!(s.place(BY, Market::Perp, &req(Buy, false, Kind::Bbo { queue: true, level: 1 }, 1.0), &R, 100.0, None, 3).is_err());
    }

    #[test]
    fn stops_fire_on_their_price_source_and_direction() {
        let asks = [(110.5, 5.0)];
        let mut s = demo();
        s.marks.insert((BY, BTC.into()), 100.0);
        // placed below the trigger: fires when the mark rises to it
        s.place(BY, Market::Perp, &req(Buy, false, Kind::Stop { trigger: 110.0, limit: None, by_mark: true }, 1.0), &R, 100.0, None, 1).unwrap();
        let o = &s.open_orders(BY, Market::Perp)[0];
        assert_eq!((o.kind.as_str(), o.trigger, o.price), ("stop", Some(110.0), 0.0));
        s.on_trade(BY, Market::Perp, BTC, 111.0, bk(&[], &asks), 2);
        s.on_mark(BY, BTC, 109.9, bk(&[], &asks), 2);
        assert!(s.fills.is_empty());
        s.on_mark(BY, BTC, 110.0, bk(&[], &asks), 3);
        assert_eq!(fills(&s), vec![(110.5, 1.0)]);
        assert!(s.orders.is_empty() && s.held(BY, BTC, Buy) == 1.0);
        // placed above: fires falling; by last, off the book at the triggering price
        s.place(BY, Market::Perp, &req(Buy, true, Kind::Stop { trigger: 90.0, limit: None, by_mark: false }, 1.0), &R, 100.0, None, 4).unwrap();
        s.on_mark(BY, BTC, 80.0, None, 5);
        s.on_trade(BY, Market::Perp, BTC, 90.1, None, 5);
        assert_eq!(s.fills.len(), 1);
        s.on_trade(BY, Market::Perp, BTC, 89.5, None, 6);
        assert_eq!(fills(&s)[1], (89.5, 1.0));
        assert!(s.pos.is_empty() && s.orders.is_empty());
        // a stop-limit becomes a resting limit; a trigger at the price is refused; OKX has none
        s.place(BY, Market::Perp, &req(Sell, false, Kind::Stop { trigger: 95.0, limit: Some(94.0), by_mark: false }, 1.0), &R, 100.0, None, 7).unwrap();
        s.on_trade(BY, Market::Perp, BTC, 95.0, bk(&[(93.0, 1.0)], &[]), 8);
        assert_eq!((s.open_orders(BY, Market::Perp)[0].kind.as_str(), s.fills.len()), ("limit", 2));
        assert!(s.place(BY, Market::Perp, &req(Buy, false, Kind::Stop { trigger: 95.0, limit: None, by_mark: false }, 1.0), &R, 100.0, None, 9).is_err());
        assert!(s.place(Exchange::Okx, Market::Perp, &req(Buy, false, Kind::Stop { trigger: 120.0, limit: None, by_mark: true }, 1.0), &R, 100.0, None, 9).is_err());
        replays(&s);
    }

    #[test]
    fn trailing_stop_tracks_the_last_price() {
        let mut s = demo();
        s.record(1, fill(BY, BTC, Buy, Buy, 100.0, 1.0, 0.0));
        // Bybit's trailing stop only protects a position
        assert!(s.place(BY, Market::Perp, &req(Sell, false, Kind::Trailing { callback_pct: 1.0, activation: None }, 1.0), &R, 100.0, None, 2).is_err());
        s.place(BY, Market::Perp, &req(Buy, true, Kind::Trailing { callback_pct: 1.0, activation: None }, 1.0), &R, 100.0, None, 2).unwrap();
        s.on_trade(BY, Market::Perp, BTC, 100.0, None, 3);
        s.on_mark(BY, BTC, 90.0, None, 3);
        s.on_trade(BY, Market::Perp, BTC, 99.1, None, 4);
        assert!(s.fills.len() == 1 && close(s.open_orders(BY, Market::Perp)[0].trigger.unwrap(), 99.0));
        s.on_trade(BY, Market::Perp, BTC, 99.0, None, 5);
        assert_eq!(fills(&s)[1], (99.0, 1.0));
        assert!(s.pos.is_empty() && s.orders.is_empty());
        replays(&s);
    }

    #[test]
    fn tpsl_fire_close_and_cancel_each_other() {
        let mut s = demo();
        s.record(1, fill(BY, BTC, Buy, Buy, 100.0, 1.0, 0.0));
        s.marks.insert((BY, BTC.into()), 100.0);
        let t = |id: &str, take_profit, trigger_px, trigger| TpSl { id: id.into(), ex: BY, symbol: BTC.into(), pos: Buy, take_profit, trigger_px, qty: None, trigger };
        assert!(s.set_tpsl(t("bad", true, 95.0, Trigger::Mark), 2).is_err());
        assert!(s.set_tpsl(TpSl { ex: Exchange::Okx, ..t("okx", true, 110.0, Trigger::Mark) }, 2).is_err());
        s.set_tpsl(t("tp", true, 110.0, Trigger::Mark), 2).unwrap();
        s.set_tpsl(t("sl", false, 90.0, Trigger::Mark), 2).unwrap();
        s.on_mark(BY, BTC, 110.0, None, 3);
        assert_eq!(fills(&s)[1], (110.0, 1.0));
        assert!(s.pos.is_empty() && s.tpsl.is_empty() && s.orders.is_empty());
        assert!(close(s.cash, 10_000.0 + 10.0 - 110.0 * 0.001));
        assert_eq!(s.hist.last().map(|h| (h.kind.as_str(), h.status.as_str())), Some(("market", "Filled")));
        // a stop-loss on the last price ignores the mark
        s.record(4, fill(BY, BTC, Buy, Buy, 100.0, 2.0, 0.0));
        s.set_tpsl(t("sl2", false, 90.0, Trigger::Last), 4).unwrap();
        s.set_tpsl(TpSl { qty: Some(0.5), ..t("tp2", true, 120.0, Trigger::Mark) }, 4).unwrap();
        s.on_mark(BY, BTC, 89.5, None, 5);
        assert_eq!(s.held(BY, BTC, Buy), 2.0);
        // a partial TP closes its size only
        s.on_mark(BY, BTC, 120.0, bk(&[(119.0, 5.0)], &[]), 6);
        assert_eq!((s.held(BY, BTC, Buy), fills(&s)[3]), (1.5, (119.0, 0.5)));
        s.on_trade(BY, Market::Perp, BTC, 89.5, None, 7);
        assert!(s.pos.is_empty() && s.tpsl.is_empty());
        replays(&s);
    }

    #[test]
    fn close_is_clamped_to_the_position_and_orphans_go() {
        let book = ([(99.0, 10.0)], [(100.0, 10.0)]);
        let b = || bk(&book.0, &book.1);
        let mut s = demo();
        assert!(s.place(BY, Market::Perp, &req(Buy, true, Kind::Market, 1.0), &R, 100.0, b(), 1).unwrap_err().to_string().contains("no long"));
        s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 2.0), &R, 100.0, b(), 1).unwrap();
        s.place(BY, Market::Perp, &req(Buy, true, lim(120.0, Tif::Gtc), 1.0), &R, 100.0, b(), 2).unwrap();
        s.place(BY, Market::Perp, &req(Buy, true, Kind::Market, 5.0), &R, 100.0, b(), 3).unwrap();
        assert_eq!(fills(&s)[1], (99.0, 2.0));
        // the resting reduce-only order went with the position
        assert!(s.pos.is_empty() && s.orders.is_empty());
        assert_eq!(s.hist.iter().map(|h| h.status.as_str()).collect::<Vec<_>>(), ["Filled", "Filled", "Cancelled"]);
        replays(&s);
    }

    #[test]
    fn margin_gate_and_hedge_mode() {
        let book = ([(99.0, 10.0)], [(100.0, 10.0)]);
        let b = || bk(&book.0, &book.1);
        let mut s = sim(10.0);
        // 2 @100 at 10x needs 20
        assert!(s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 2.0), &R, 100.0, b(), 1).unwrap_err().to_string().contains("insufficient margin"));
        assert!(s.place(BY, Market::Perp, &req(Buy, false, lim(95.0, Tif::Gtc), 2.0), &R, 100.0, b(), 1).is_err());
        let mut s = demo();
        s.configure(true, true, 1).unwrap();
        s.place(BY, Market::Perp, &req(Buy, false, Kind::Market, 1.0), &R, 100.0, b(), 2).unwrap();
        s.place(BY, Market::Perp, &req(Sell, false, Kind::Market, 1.0), &R, 100.0, b(), 2).unwrap();
        assert_eq!((s.held(BY, BTC, Buy), s.held(BY, BTC, Sell)), (1.0, 1.0));
        s.place(BY, Market::Perp, &req(Sell, true, Kind::Market, 1.0), &R, 100.0, b(), 3).unwrap();
        assert_eq!((s.held(BY, BTC, Buy), s.held(BY, BTC, Sell)), (1.0, 0.0));
        assert_eq!(s.positions(BY)[0].side, Buy);
        // Kraken has no FOK; options are not simulated
        assert!(s.place(Exchange::Kraken, Market::Perp, &req(Buy, false, lim(100.0, Tif::Fok), 1.0), &R, 100.0, b(), 4).is_err());
        assert!(s.place(BY, Market::Option, &req(Buy, false, Kind::Market, 1.0), &R, 100.0, b(), 4).is_err());
        replays(&s);
    }

    #[test]
    fn amend_reprices_and_takes_when_crossing() {
        let (bids, asks) = ([(98.0, 1.0)], [(100.0, 1.0), (101.0, 1.0)]);
        let mut s = demo();
        let id = s.place(BY, Market::Perp, &req(Buy, false, lim(99.0, Tif::Gtc), 2.0), &R, 99.0, bk(&bids, &asks), 1).unwrap();
        s.amend(&id, 99.04, 0.1, bk(&bids, &asks), 2).unwrap();
        assert_eq!(s.open_orders(BY, Market::Perp)[0].price, 99.0);
        s.amend(&id, 100.5, 0.1, bk(&bids, &asks), 3).unwrap();
        assert_eq!(fills(&s), vec![(100.0, 1.0)]);
        assert_eq!((s.orders[0].filled, limit_price(&s.orders[0].req.kind)), (1.0, 100.5));
        let post = s.place(BY, Market::Perp, &req(Buy, false, lim(99.0, Tif::PostOnly), 1.0), &R, 99.0, bk(&bids, &asks), 4).unwrap();
        assert!(s.amend(&post, 100.0, 0.1, bk(&bids, &asks), 5).is_err());
        s.cancel(&post, 6).unwrap();
        assert!(s.cancel(&post, 6).is_err() && s.amend(&post, 99.0, 0.1, None, 6).is_err());
        replays(&s);
    }

    #[test]
    fn funding_sign_for_long_and_short() {
        let mut s = demo();
        s.configure(true, true, 0).unwrap();
        s.record(1, fill(BY, BTC, Buy, Buy, 100.0, 1.0, 0.0));
        s.record(1, fill(BY, BTC, Sell, Sell, 100.0, 2.0, 0.0));
        s.marks.insert((BY, BTC.into()), 110.0);
        s.funding(BY, BTC, 0.0001, 2);
        // long pays 1 * 110 * 0.01%, short receives 2 * 110 * 0.01%
        assert!(close(s.funding_paid(BY, BTC, Buy).unwrap().amount, -0.011) && close(s.funding_paid(BY, BTC, Sell).unwrap().amount, 0.022));
        assert!(close(s.cash, 10_000.011));
        // a negative rate flips who pays; other symbols are untouched
        s.funding(BY, BTC, -0.0001, 3);
        s.funding(BY, "ETHUSDT", 0.01, 3);
        assert!(close(s.cash, 10_000.0));
        replays(&s);
    }

    #[test]
    fn cross_liquidation_closes_every_position_on_the_pool() {
        // Bybit long 10 @100 and OKX short 20 @50 at 10x on 300 USDT: maintenance 10
        let mut s = sim(300.0);
        s.record(1, fill(BY, BTC, Buy, Buy, 100.0, 10.0, 0.0));
        s.record(1, fill(Exchange::Okx, "ETHUSDT", Sell, Sell, 50.0, 20.0, 0.0));
        s.place(Exchange::Okx, Market::Perp, &OrderReq { symbol: "ETHUSDT".into(), ..req(Sell, false, lim(60.0, Tif::Gtc), 1.0) }, &R, 50.0, None, 1).unwrap();
        s.marks.insert((BY, BTC.into()), 72.0);
        s.liquidate(2);
        assert_eq!((s.pos.len(), s.orders.len()), (2, 1));
        assert!(close(s.positions(BY)[0].liq.unwrap(), 71.0));
        // equity reaches the maintenance margin: both go, the order first, cash floored at 0
        s.marks.insert((BY, BTC.into()), 71.0);
        s.liquidate(3);
        assert!(s.pos.is_empty() && s.orders.is_empty() && s.cash == 0.0);
        assert_eq!(s.hist.iter().filter(|h| h.kind == "liquidation").count(), 2);
        replays(&s);
    }

    #[test]
    fn isolated_liquidation_takes_the_margin() {
        let mut s = sim(1000.0);
        s.configure(false, false, 0).unwrap();
        s.record(1, fill(BY, BTC, Buy, Buy, 100.0, 10.0, 0.0));
        s.record(1, fill(Exchange::Okx, "ETHUSDT", Sell, Sell, 50.0, 20.0, 0.0));
        // the long's liquidation price: entry * (1 - 1/10 + 0.5%)
        let liq = s.positions(BY)[0].liq.unwrap();
        assert!(close(liq, 90.5));
        s.marks.insert((BY, BTC.into()), liq + 0.1);
        s.liquidate(2);
        assert_eq!(s.pos.len(), 2);
        // reached: its margin of 100 is gone, the short stays
        s.marks.insert((BY, BTC.into()), liq);
        s.liquidate(3);
        assert!(s.pos.len() == 1 && s.held(Exchange::Okx, "ETHUSDT", Sell) == 20.0);
        assert!(close(s.cash, 900.0));
        replays(&s);
    }

    #[test]
    fn spot_buys_and_sells_against_the_pool() {
        let (bids, asks) = ([(99.0, 1.0)], [(100.0, 1.0)]);
        let b = || bk(&bids, &asks);
        let bn = Exchange::Binance;
        let mut s = demo();
        s.fees.insert(bn, (0.001, 0.001));
        s.place(bn, Market::Spot, &req(Buy, false, Kind::Market, 0.5), &R, 100.0, b(), 1).unwrap();
        assert!(close(s.cash, 10_000.0 - 50.0 - 0.05) && s.pos.is_empty());
        assert!(s.place(bn, Market::Spot, &req(Sell, false, Kind::Market, 1.0), &R, 100.0, b(), 2).unwrap_err().to_string().contains("insufficient BTC"));
        // a resting sell holds the coins until a print goes through it
        s.place(bn, Market::Spot, &req(Sell, false, lim(105.0, Tif::Gtc), 0.5), &R, 100.0, b(), 3).unwrap();
        let bal = s.spot_balances(bn);
        assert_eq!((bal[1].free, bal[1].locked), (0.0, 0.5));
        assert!(s.place(bn, Market::Spot, &req(Sell, false, Kind::Market, 0.1), &R, 100.0, b(), 4).is_err());
        s.on_trade(bn, Market::Spot, BTC, 105.1, None, 5);
        assert!(close(s.cash, 10_000.0 - 50.05 + 52.5 - 0.0525) && s.coins.is_empty());
        // spot venues only, limit and market only
        assert!(s.place(Exchange::Okx, Market::Spot, &req(Buy, false, Kind::Market, 0.1), &R, 100.0, b(), 6).is_err());
        assert!(s.place(bn, Market::Spot, &req(Buy, false, Kind::Stop { trigger: 120.0, limit: None, by_mark: false }, 0.1), &R, 100.0, b(), 6).is_err());
        replays(&s);
    }
}
