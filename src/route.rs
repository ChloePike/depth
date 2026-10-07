//! Smart order routing: a pure planner (no network, no locks). See docs/sor.md.
//! Every leg is priced from its venue's own raw book in its own quote; the composite index never
//! prices a leg. Fixed-venue mode is the same path restricted to one venue and one leg.
use crate::trade::{self, Kind, Mode, OrderReq, Position, Rules, Tif};
use crate::{Exchange, Side};
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

/// One venue's raw book in its own quote, best level first.
#[derive(Clone, Debug)]
pub struct VenueSnap { pub ex: Exchange, pub bids: Vec<(f64, f64)>, pub asks: Vec<(f64, f64)>, pub ts_ms: i64, pub lat_ms: Option<f64> }

/// Local account state per venue (no REST in the planner).
#[derive(Clone, Debug, Default)]
pub struct AcctSnap {
    pub available: HashMap<Exchange, f64>,
    pub lev: HashMap<Exchange, f64>,
    pub rules: HashMap<Exchange, Rules>,
    pub mode: HashMap<Exchange, Mode>,
    /// already filtered to the symbol
    pub positions: Vec<Position>,
    /// venues with errors / resyncing, with the reason
    pub unavailable: HashMap<Exchange, String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Policy {
    pub smart: bool, pub fixed: Exchange, pub allow_split: bool, pub max_legs: usize,
    pub max_slip_bps: f64, pub split_bps: f64, pub min_split_notional: f64, pub max_stale_ms: i64, pub max_disp_bps: f64,
    /// (taker, maker) as fractions
    pub fees: HashMap<Exchange, (f64, f64)>,
}

const DEFAULT_FEES: (f64, f64) = (0.00055, 0.0002);

impl Default for Policy {
    fn default() -> Self {
        Policy { smart: false, fixed: Exchange::Bybit, allow_split: true, max_legs: 2, max_slip_bps: 10.0, split_bps: 2.0,
                 min_split_notional: 500.0, max_stale_ms: 1500, max_disp_bps: 30.0,
                 fees: trade::TRADABLE.iter().map(|&e| (e, DEFAULT_FEES)).collect() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Leg { pub ex: Exchange, pub req: OrderReq, pub ref_px: f64, pub mode: Mode, pub est_px: f64, pub est_fee: f64, pub slip_bps: f64, pub avail_after: f64 }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoutePlan { pub legs: Vec<Leg>, pub excluded: Vec<(Exchange, String)>, pub clamped_from: Option<f64>, pub total_qty: f64, pub vwap: f64, pub fees: f64 }

/// VWAP and filled size of taking `qty` from `levels` (best first).
pub fn walk(levels: &[(f64, f64)], qty: f64) -> (f64, f64) {
    let (mut cost, mut filled) = (0.0, 0.0);
    for &(px, q) in levels {
        if filled >= qty { break; }
        let t = q.min(qty - filled);
        cost += t * px;
        filled += t;
    }
    (if filled > 0.0 { cost / filled } else { 0.0 }, filled)
}

/// Estimated fill price of `kind` for `q` on one venue: (price, takes liquidity, fills now).
/// `lv` is the side we trade against, `own` our own side.
fn quote(kind: Kind, sgn: f64, lv: &[(f64, f64)], own: &[(f64, f64)], q: f64) -> (f64, bool, bool) {
    match kind {
        Kind::Market => { let (px, f) = walk(lv, q); (px, true, f >= q * (1.0 - 1e-9)) }
        Kind::Limit { price, .. } if sgn * (lv[0].0 - price) <= 0.0 => {
            let within: Vec<_> = lv.iter().copied().take_while(|l| sgn * (l.0 - price) <= 0.0).collect();
            (walk(&within, q).0, true, true)
        }
        Kind::Limit { price, .. } => (price, false, false),
        Kind::Bbo { queue, level } => { let l = level.max(1) as usize - 1; (if queue { own[l].0 } else { lv[l].0 }, !queue, true) }
    }
}

fn floor_step(x: f64, step: f64) -> f64 { trade::fmt_step(x, step, true).parse().unwrap_or(0.0) }

/// Price on the tick grid, rounded away from the aggressive side (`up` for sells).
fn to_tick(px: f64, tick: f64, up: bool) -> f64 {
    if tick <= 0.0 { return px; }
    let n = px / tick;
    let n = if up { (n - 1e-9).ceil() } else { (n + 1e-9).floor() };
    trade::fmt_step(n * tick, tick, false).parse().unwrap_or(px)
}

struct Cand<'a> {
    ex: Exchange, lv: &'a [(f64, f64)], own: &'a [(f64, f64)], rules: Rules, mode: Mode,
    /// most this venue may take: margin cap (open) or the position held (close)
    cap: f64, avail: f64, lev: f64, est: f64, fee: f64, complete: bool, lat: f64,
}

pub fn plan(req: &OrderReq, venues: &[VenueSnap], acct: &AcctSnap, p: &Policy, now_ms: i64, plan_id: u64) -> Result<RoutePlan> {
    let q = req.qty;
    if !(q > 0.0 && q.is_finite()) { bail!("invalid size {q}"); }
    let fixed;
    let p = if p.smart { p } else { fixed = Policy { allow_split: false, max_legs: 1, ..p.clone() }; &fixed };
    let side = req.side();
    let sgn = if side == Side::Buy { 1.0 } else { -1.0 };
    let market = matches!(req.kind, Kind::Market);
    let mut out = RoutePlan::default();

    // eligibility
    let mut cands: Vec<Cand> = vec![];
    for v in venues.iter().filter(|v| p.smart || v.ex == p.fixed) {
        let ex = v.ex;
        let (lv, own) = if side == Side::Buy { (&v.asks[..], &v.bids[..]) } else { (&v.bids[..], &v.asks[..]) };
        let held: f64 = acct.positions.iter().filter(|x| x.ex == ex && x.side == req.pos).map(|x| x.qty).sum();
        let (avail, lev) = (acct.available.get(&ex).copied(), acct.lev.get(&ex).copied().filter(|l| *l > 0.0));
        let (taker, maker) = p.fees.get(&ex).copied().unwrap_or(DEFAULT_FEES);
        let why = (|| -> Result<Cand, String> {
            if let Some(r) = acct.unavailable.get(&ex) { return Err(r.clone()); }
            if !trade::TRADABLE.contains(&ex) { return Err("trading not supported".into()); }
            let age = now_ms - v.ts_ms;
            if age > p.max_stale_ms { return Err(format!("stale book ({age} ms)")); }
            if v.bids.is_empty() || v.asks.is_empty() { return Err("empty book".into()); }
            if v.bids[0].0 >= v.asks[0].0 { return Err("crossed book".into()); }
            let rules = acct.rules.get(&ex).copied().filter(|r| r.step > 0.0).ok_or("no instrument rules")?;
            let mode = acct.mode.get(&ex).copied().ok_or("no position mode")?;
            if let Kind::Bbo { level, .. } = req.kind {
                if !trade::bbo_levels(ex).contains(&level) { return Err(format!("BBO level {level} unsupported")); }
                if lv.len().min(own.len()) < level as usize { return Err("book shallower than BBO level".into()); }
            }
            if let Kind::Limit { price, tif: Tif::PostOnly } = req.kind {
                if sgn * (lv[0].0 - price) <= 0.0 { return Err(format!("post-only would cross ({})", lv[0].0)); }
            }
            let (est, taker_side, complete) = quote(req.kind, sgn, lv, own, q);
            let fee = if taker_side { taker } else { maker };
            let lat = v.lat_ms.unwrap_or(f64::MAX);
            if req.close {
                if held <= 0.0 { return Err("no position on that side".into()); }
                return Ok(Cand { ex, lv, own, rules, mode, cap: held, avail: avail.unwrap_or(0.0), lev: lev.unwrap_or(1.0), est, fee, complete, lat });
            }
            let lev = lev.ok_or("no leverage")?;
            let avail = avail.ok_or("no balance")?;
            let cap = avail * lev / est * 0.98;
            if cap < rules.min_qty.max(rules.step) { return Err("insufficient margin".into()); }
            Ok(Cand { ex, lv, own, rules, mode, cap, avail, lev, est, fee, complete, lat })
        })();
        match why { Ok(c) => cands.push(c), Err(r) => out.excluded.push((ex, r)) }
    }

    // dispersion guard: a venue whose touch is far from the median touch, either way, is refused (bad / lagging data).
    // ponytail: with two venues the median is their mean, so a lone bad venue needs a third to be caught
    let mut touches: Vec<f64> = cands.iter().map(|c| c.lv[0].0).collect();
    touches.sort_by(f64::total_cmp);
    if let Some(&m) = touches.get(touches.len() / 2) {
        let med = if touches.len() % 2 == 0 { (m + touches[touches.len() / 2 - 1]) / 2.0 } else { m };
        cands.retain(|c| {
            let off = (c.lv[0].0 - med).abs() / med * 1e4;
            if off > p.max_disp_bps { out.excluded.push((c.ex, format!("dispersion {off:.1} bp off the median"))); }
            off <= p.max_disp_bps
        });
    }
    if cands.is_empty() {
        let why: Vec<_> = out.excluded.iter().map(|(e, r)| format!("{e:?}: {r}")).collect();
        bail!("no eligible venue ({})", if why.is_empty() { "no books".into() } else { why.join("; ") });
    }

    // rank: fills now, then fee-adjusted price, then distance of a resting price from the touch, then latency.
    // closes: largest position first.
    let key = |c: &Cand| (if req.close { -c.cap } else { 0.0 }, !c.complete, sgn * c.est * (1.0 + sgn * c.fee), sgn * (c.lv[0].0 - c.est), c.lat);
    cands.sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal));
    let best = &cands[0];
    let touch = best.lv[0].0;
    let legs = p.max_legs.max(1);
    let can_split = p.allow_split && legs > 1 && (req.close || q * touch >= p.min_split_notional);
    let costly = market && !req.close && (!best.complete || sgn * (best.est - touch) / touch * 1e4 > p.split_bps);
    let eps = q * 1e-9;

    let mut alloc: Vec<(usize, f64)> = if (best.cap < q || costly) && can_split {
        let mut got = vec![0.0; cands.len()];
        let mut left = q;
        let used = |got: &[f64]| got.iter().filter(|g| **g > 0.0).count();
        if costly {
            // merged fee-adjusted ladder across venues, respecting margin caps and max legs
            let mut lad: Vec<(f64, usize, f64)> = cands.iter().enumerate()
                .flat_map(|(i, c)| c.lv.iter().map(move |&(px, lq)| (sgn * px * (1.0 + sgn * c.fee), i, lq))).collect();
            lad.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, i, lq) in lad {
                if left <= eps { break; }
                if got[i] == 0.0 && used(&got) >= legs { continue; }
                let t = lq.min(left).min(cands[i].cap - got[i]);
                if t > 0.0 { got[i] += t; left -= t; }
            }
        }
        // in rank order up to each cap (also the remainder once every book is exhausted: the slippage cap makes it an IOC)
        for i in 0..cands.len() {
            if left <= eps { break; }
            if got[i] == 0.0 && used(&got) >= legs { continue; }
            let t = left.min(cands[i].cap - got[i]);
            if t > 0.0 { got[i] += t; left -= t; }
        }
        got.into_iter().enumerate().filter(|g| g.1 > 0.0).collect()
    } else {
        let i = cands.iter().position(|c| c.cap >= q).unwrap_or(0);
        vec![(i, q.min(cands[i].cap))]
    };
    let mut clamped = alloc.iter().map(|a| a.1).sum::<f64>() < q - eps;

    // fold legs under min qty / min notional into the largest leg
    let px_of = |c: &Cand| if let Kind::Limit { price, .. } = req.kind { price } else { c.lv[0].0 };
    let small = |c: &Cand, x: f64| { let r = floor_step(x, c.rules.step); r <= 0.0 || r < c.rules.min_qty || (!req.close && r * px_of(c) < c.rules.min_notional) };
    let largest = |a: &[(usize, f64)]| (0..a.len()).max_by(|&x, &y| a[x].1.total_cmp(&a[y].1)).unwrap_or(0);
    while alloc.len() > 1 {
        let big = largest(&alloc);
        let Some(s) = (0..alloc.len()).find(|&k| k != big && small(&cands[alloc[k].0], alloc[k].1)) else { break };
        let (_, x) = alloc.remove(s);
        let b = &mut alloc[if s < big { big - 1 } else { big }];
        let room = cands[b.0].cap - b.1;
        if x > room + eps { clamped = true; }
        b.1 += x.min(room);
    }

    // round every leg down to its step; the dust goes to the largest leg (within its cap)
    let mut dust = 0.0;
    for a in alloc.iter_mut() { let r = floor_step(a.1, cands[a.0].rules.step); dust += a.1 - r; a.1 = r; }
    let big = largest(&alloc);
    let (bi, bq) = alloc[big];
    alloc[big].1 = floor_step((bq + dust).min(cands[bi].cap), cands[bi].rules.step);

    for (n, &(i, x)) in alloc.iter().enumerate() {
        let c = &cands[i];
        let t = c.lv[0].0;
        let mut kind = req.kind;
        let (mut est, mut taker_side, complete) = quote(kind, sgn, c.lv, c.own, x);
        let mut slip = sgn * (est - t) / t * 1e4;
        if market && (slip > p.max_slip_bps || !complete) {
            kind = Kind::Limit { price: to_tick(t * (1.0 + sgn * p.max_slip_bps / 1e4), c.rules.tick, side == Side::Sell), tif: Tif::Ioc };
            (est, taker_side, _) = quote(kind, sgn, c.lv, c.own, x);
            slip = sgn * (est - t) / t * 1e4;
        }
        let (taker, maker) = p.fees.get(&c.ex).copied().unwrap_or(DEFAULT_FEES);
        let est_fee = est * x * if taker_side { taker } else { maker };
        let leg_req = OrderReq { kind, qty: x, client_id: Some(format!("t1{plan_id:x}l{n}")), ..req.clone() };
        trade::checked(&leg_req, &c.rules, t).map_err(|e| anyhow!("{:?}: {e}", c.ex))?;
        let avail_after = if req.close { c.avail - est_fee } else { c.avail - x * est / c.lev - est_fee };
        out.legs.push(Leg { ex: c.ex, req: leg_req, ref_px: t, mode: c.mode, est_px: est, est_fee, slip_bps: if taker_side { slip } else { 0.0 }, avail_after });
    }
    out.total_qty = out.legs.iter().map(|l| l.req.qty).sum();
    out.vwap = out.legs.iter().map(|l| l.est_px * l.req.qty).sum::<f64>() / out.total_qty;
    out.fees = out.legs.iter().map(|l| l.est_fee).sum();
    out.clamped_from = clamped.then_some(q);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Exchange::{Binance, Bybit, Okx};

    const NOW: i64 = 1_000_000;

    fn snap(ex: Exchange, bids: &[(f64, f64)], asks: &[(f64, f64)]) -> VenueSnap {
        VenueSnap { ex, bids: bids.to_vec(), asks: asks.to_vec(), ts_ms: NOW - 100, lat_ms: Some(50.0) }
    }
    fn acct(exs: &[Exchange]) -> AcctSnap {
        let mut a = AcctSnap::default();
        for &e in exs {
            a.available.insert(e, 1e6);
            a.lev.insert(e, 10.0);
            a.rules.insert(e, Rules { tick: 0.01, step: 0.001, min_qty: 0.001, min_notional: 5.0 });
            a.mode.insert(e, Mode::OneWay);
        }
        a
    }
    fn req(kind: Kind, qty: f64) -> OrderReq { OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close: false, kind, qty, client_id: None } }
    fn smart() -> Policy { Policy { smart: true, ..Policy::default() } }
    fn leg(p: &RoutePlan, ex: Exchange) -> f64 { p.legs.iter().filter(|l| l.ex == ex).map(|l| l.req.qty).sum() }
    fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-9 }
    const BIDS: &[(f64, f64)] = &[(99.0, 100.0)];

    #[test]
    fn cheaper_after_fees_wins() {
        let mut p = smart();
        p.fees.insert(Bybit, (0.001, 0.0002));
        p.fees.insert(Binance, (0.0002, 0.0002));
        let v = [snap(Bybit, BIDS, &[(100.0, 10.0)]), snap(Binance, BIDS, &[(100.05, 10.0)])];
        let r = plan(&req(Kind::Market, 1.0), &v, &acct(&[Bybit, Binance]), &p, NOW, 1).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert_eq!(r.legs[0].ex, Binance);
        assert_eq!(r.legs[0].req.client_id.as_deref(), Some("t11l0"));
    }

    #[test]
    fn split_on_thin_book() {
        let v = [snap(Bybit, BIDS, &[(100.0, 1.0), (100.1, 1.0), (101.0, 10.0)]), snap(Binance, BIDS, &[(100.02, 1.0), (102.0, 10.0)])];
        let r = plan(&req(Kind::Market, 6.0), &v, &acct(&[Bybit, Binance]), &smart(), NOW, 2).unwrap();
        assert_eq!(r.legs.len(), 2);
        assert!(close(leg(&r, Binance), 1.0), "{r:?}");
        assert!(close(leg(&r, Bybit), 5.0), "{r:?}");
        assert!(close(r.total_qty, 6.0));
        assert_eq!(r.legs[1].req.client_id.as_deref(), Some("t12l1"));
    }

    #[test]
    fn no_split_under_min_notional() {
        let v = [snap(Bybit, BIDS, &[(100.0, 1.0), (100.1, 1.0), (101.0, 10.0)]), snap(Binance, BIDS, &[(100.02, 1.0), (102.0, 10.0)])];
        let r = plan(&req(Kind::Market, 3.0), &v, &acct(&[Bybit, Binance]), &smart(), NOW, 3).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert_eq!(r.legs[0].ex, Bybit);
    }

    #[test]
    fn small_leg_folds() {
        let mut a = acct(&[Bybit, Binance]);
        a.rules.get_mut(&Binance).unwrap().min_qty = 1.0;
        let v = [snap(Bybit, BIDS, &[(100.0, 1.0), (100.05, 20.0)]), snap(Binance, BIDS, &[(100.01, 0.2), (103.0, 10.0)])];
        let r = plan(&req(Kind::Market, 10.0), &v, &a, &smart(), NOW, 4).unwrap();
        assert_eq!(r.legs.len(), 1, "{r:?}");
        assert_eq!(r.legs[0].ex, Bybit);
        assert!(close(r.total_qty, 10.0));
        assert!(r.clamped_from.is_none());
    }

    #[test]
    fn margin_cap_split_and_clamp() {
        let mut a = acct(&[Bybit, Binance]);
        a.available.insert(Bybit, 100.0); // cap 100 * 10 / 100 * 0.98 = 9.8
        a.available.insert(Binance, 50.0); // cap ~4.9
        let v = [snap(Bybit, BIDS, &[(100.0, 100.0)]), snap(Binance, BIDS, &[(100.01, 100.0)])];
        let lim = Kind::Limit { price: 100.0, tif: Tif::Gtc };
        let r = plan(&req(lim, 12.0), &v, &a, &smart(), NOW, 5).unwrap();
        assert_eq!(r.legs.len(), 2, "{r:?}");
        assert!(leg(&r, Bybit) <= 9.8 + 1e-9 && leg(&r, Bybit) > 9.79, "{r:?}");
        assert!((r.total_qty - 12.0).abs() < 0.002, "{r:?}");
        assert!(r.clamped_from.is_none());
        let r = plan(&req(Kind::Market, 20.0), &v, &a, &smart(), NOW, 5).unwrap();
        assert_eq!(r.clamped_from, Some(20.0));
        assert!(r.total_qty < 14.71 && r.total_qty > 14.6, "{r:?}");
        // no split allowed: the best venue alone, clamped to its cap
        let r = plan(&req(Kind::Market, 12.0), &v, &a, &Policy { allow_split: false, ..smart() }, NOW, 5).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert_eq!(r.clamped_from, Some(12.0));
    }

    #[test]
    fn stale_and_crossed_excluded() {
        let mut stale = snap(Okx, BIDS, &[(99.5, 10.0)]);
        stale.ts_ms = NOW - 5000;
        let v = [snap(Bybit, BIDS, &[(100.0, 10.0)]), stale, snap(Binance, &[(100.5, 1.0)], &[(100.0, 1.0)])];
        let r = plan(&req(Kind::Market, 1.0), &v, &acct(&[Bybit, Binance, Okx]), &smart(), NOW, 6).unwrap();
        assert_eq!(r.legs[0].ex, Bybit);
        assert!(r.excluded.iter().any(|(e, why)| *e == Okx && why.contains("stale")), "{r:?}");
        assert!(r.excluded.iter().any(|(e, why)| *e == Binance && why.contains("crossed")), "{r:?}");
        let mut a = acct(&[Bybit]);
        a.unavailable.insert(Bybit, "resyncing".into());
        let e = plan(&req(Kind::Market, 1.0), &v[..1], &a, &smart(), NOW, 6).unwrap_err();
        assert!(e.to_string().contains("resyncing"));
    }

    #[test]
    fn post_only_never_crosses() {
        let v = [snap(Bybit, BIDS, &[(100.02, 10.0)]), snap(Binance, BIDS, &[(100.1, 10.0)])];
        let k = Kind::Limit { price: 100.05, tif: Tif::PostOnly };
        let r = plan(&req(k, 1.0), &v, &acct(&[Bybit, Binance]), &smart(), NOW, 7).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert_eq!(r.legs[0].ex, Binance);
        assert!(r.excluded.iter().any(|(e, why)| *e == Bybit && why.contains("post-only")));
    }

    #[test]
    fn bbo_level_per_venue() {
        let deep: Vec<(f64, f64)> = (0..20).map(|i| (100.0 + i as f64 * 0.1, 1.0)).collect();
        let bids: Vec<(f64, f64)> = (0..20).map(|i| (99.9 - i as f64 * 0.1, 1.0)).collect();
        let v = [snap(Bybit, &bids, &deep), snap(Binance, &bids, &deep)];
        let k = Kind::Bbo { queue: true, level: 10 };
        let r = plan(&req(k, 1.0), &v, &acct(&[Bybit, Binance]), &smart(), NOW, 8).unwrap();
        assert_eq!(r.legs[0].ex, Binance);
        assert!(r.excluded.iter().any(|(e, why)| *e == Bybit && why.contains("BBO level")));
        let r = plan(&req(Kind::Bbo { queue: true, level: 5 }, 1.0), &v, &acct(&[Bybit, Binance]), &smart(), NOW, 8).unwrap();
        assert!(r.excluded.is_empty(), "{:?}", r.excluded);
        assert!(close(r.legs[0].est_px, 99.5));
    }

    #[test]
    fn slippage_cap_makes_ioc() {
        let v = [snap(Bybit, BIDS, &[(100.0, 0.1), (101.0, 10.0)])];
        let r = plan(&req(Kind::Market, 1.0), &v, &acct(&[Bybit]), &smart(), NOW, 9).unwrap();
        match r.legs[0].req.kind { Kind::Limit { price, tif: Tif::Ioc } => assert!(close(price, 100.1)), k => panic!("{k:?}") }
        assert!(close(r.legs[0].est_px, 100.0));
    }

    #[test]
    fn closes_only_holding_venues_never_over_close() {
        let mut a = acct(&[Bybit, Binance, Okx]);
        let pos = |ex, side, qty| Position { ex, symbol: "BTCUSDT".into(), side, qty, entry: 100.0, mark: 100.0, liq: None, upnl: 0.0, lev: 10.0, margin: 0.0, cross: None };
        a.positions = vec![pos(Bybit, Side::Buy, 1.0), pos(Binance, Side::Buy, 3.0), pos(Okx, Side::Sell, 5.0)];
        let bids = &[(100.0, 100.0)];
        let v = [snap(Bybit, bids, &[(100.1, 1.0)]), snap(Binance, bids, &[(100.1, 1.0)]), snap(Okx, &[(100.05, 100.0)], &[(100.1, 1.0)])];
        let r = plan(&OrderReq { close: true, ..req(Kind::Market, 5.0) }, &v, &a, &smart(), NOW, 10).unwrap();
        assert_eq!(r.legs[0].ex, Binance, "largest first");
        assert!(close(leg(&r, Binance), 3.0) && close(leg(&r, Bybit), 1.0) && close(leg(&r, Okx), 0.0), "{r:?}");
        assert_eq!(r.clamped_from, Some(5.0));
        assert!(r.legs.iter().all(|l| l.req.side() == Side::Sell && l.req.close));
        assert!(r.excluded.iter().any(|(e, _)| *e == Okx));
    }

    #[test]
    fn fixed_policy_single_leg() {
        let v = [snap(Bybit, BIDS, &[(100.0, 10.0)]), snap(Binance, BIDS, &[(100.2, 10.0)])];
        let a = acct(&[Bybit, Binance]);
        let p = Policy { fixed: Binance, ..Policy::default() };
        let rq = req(Kind::Market, 0.12345);
        let r = plan(&rq, &v, &a, &p, NOW, 11).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert_eq!(r.legs[0].ex, Binance);
        let (want, _) = trade::checked(&rq, &a.rules[&Binance], 100.2).unwrap();
        assert_eq!(trade::fmt_step(r.legs[0].req.qty, 0.001, true), want);
        assert!(r.clamped_from.is_none());
    }

    #[test]
    fn rounding_dust_to_largest_leg() {
        let mut a = acct(&[Bybit, Binance]);
        for r in a.rules.values_mut() { r.step = 0.1; r.min_qty = 0.1; }
        let v = [snap(Bybit, BIDS, &[(100.0, 1.1), (100.1, 0.25), (101.0, 10.0)]), snap(Binance, BIDS, &[(100.02, 1.15), (102.0, 10.0)])];
        let r = plan(&req(Kind::Market, 2.5), &v, &a, &Policy { min_split_notional: 0.0, ..smart() }, NOW, 12).unwrap();
        assert!(close(leg(&r, Bybit), 1.4) && close(leg(&r, Binance), 1.1), "{r:?}");
        assert!(close(r.total_qty, 2.5));
    }

    #[test]
    fn dispersion_guard() {
        let a = acct(&[Bybit, Binance, Okx]);
        // too expensive
        let v = [snap(Bybit, BIDS, &[(100.0, 0.5), (101.0, 10.0)]), snap(Okx, BIDS, &[(100.01, 10.0)]), snap(Binance, BIDS, &[(100.5, 10.0)])];
        let r = plan(&req(Kind::Market, 10.0), &v, &a, &smart(), NOW, 13).unwrap();
        assert!(r.legs.iter().all(|l| l.ex != Binance));
        assert!(r.excluded.iter().any(|(e, why)| *e == Binance && why.contains("dispersion")), "{r:?}");
        // too good to be true: refused although it is the cheapest
        let v = [snap(Bybit, BIDS, &[(100.0, 10.0)]), snap(Okx, BIDS, &[(100.01, 10.0)]), snap(Binance, BIDS, &[(99.5, 10.0)])];
        let r = plan(&req(Kind::Market, 1.0), &v, &a, &smart(), NOW, 13).unwrap();
        assert_eq!(r.legs[0].ex, Bybit);
        assert!(r.excluded.iter().any(|(e, why)| *e == Binance && why.contains("dispersion")), "{r:?}");
    }

    #[test]
    fn plans_compare_equal() {
        let v = [snap(Bybit, BIDS, &[(100.0, 10.0)])];
        let run = || plan(&req(Kind::Market, 1.0), &v, &acct(&[Bybit]), &smart(), NOW, 14).unwrap();
        assert_eq!(run(), run());
    }

    #[test]
    fn policy_serde_roundtrip() {
        let p = Policy::default();
        assert_eq!(p.fees[&Bybit], (0.00055, 0.0002));
        let back: Policy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let partial: Policy = serde_json::from_str(r#"{"max_legs":3}"#).unwrap();
        assert_eq!(partial.max_legs, 3);
        assert_eq!(partial.max_slip_bps, 10.0);
    }
}
