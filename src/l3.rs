//! Order-by-order books (L3) with each order's life cycle (L4 without owner ids):
//! - exact, from venues that publish resting orders with ids (Lighter: REST snapshot of the top
//!   orders plus maker order ids on trades);
//! - inferred, from L2 levels that carry an order count plus the trade stream (OKX `books`):
//!   a level whose count moves by one gains or loses exactly that order; ambiguous updates fall
//!   back to an equal split, and every order says how it is known (`Src`).
//! Measured accuracy of the inference (research notes, BTC, 100 ms batches against true L4):
//! about 90 % of order sizes and queue positions right on the top 10 levels of a book with few
//! large orders, much less at the touch of a heavily re-quoted book. The UI must show `Src`.
use std::collections::{BTreeMap, HashMap, VecDeque};

/// How an order is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src { Exact, Inferred, Split }

#[derive(Clone, Debug)]
pub struct Order {
    /// venue order id (exact) or a local id (inferred)
    pub id: u64,
    /// size left / size when first seen (base units)
    pub qty: f64,
    pub initial: f64,
    /// first seen (ms); `since_known` false for orders already resting when the stream started
    pub since: i64,
    pub since_known: bool,
    pub src: Src,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind { Add, Amend, Partial, Fill, Cancel }

/// One life-cycle event.
#[derive(Clone, Debug)]
pub struct Life { pub ts: i64, pub bid: bool, pub px: f64, pub qty: f64, pub id: u64, pub kind: Kind, pub src: Src }

/// A venue order as published (exact L3).
#[derive(Clone, Debug)]
pub struct Resting { pub id: u64, pub px: f64, pub qty: f64, pub initial: f64, pub seq: u64 }

const EVENTS: usize = 2000;
const SCALE: f64 = 1e8;
fn key(px: f64) -> i64 { (px * 1e6).round() as i64 }
fn units(q: f64) -> i64 { (q * SCALE).round() as i64 }
fn qty(u: i64) -> f64 { u as f64 / SCALE }

#[derive(Default)]
pub struct L3Book {
    /// price key -> orders in queue order (front fills first)
    pub bids: BTreeMap<i64, VecDeque<Order>>,
    pub asks: BTreeMap<i64, VecDeque<Order>>,
    px: HashMap<i64, f64>,
    /// newest last
    pub events: VecDeque<Life>,
    next: u64,
    /// exact: id -> (bid, key); fills seen per id since the last snapshot
    index: HashMap<u64, (bool, i64)>,
    filled: HashMap<u64, f64>,
    /// inferred: taker volume per (bid side, key) since the last level update
    traded: HashMap<(bool, i64), i64>,
    started: bool,
}

impl L3Book {
    fn side(&mut self, bid: bool) -> &mut BTreeMap<i64, VecDeque<Order>> { if bid { &mut self.bids } else { &mut self.asks } }
    fn log(&mut self, ts: i64, bid: bool, k: i64, q: f64, id: u64, kind: Kind, src: Src) {
        if !self.started { return; }
        let px = self.px.get(&k).copied().unwrap_or(k as f64 / 1e6);
        self.events.push_back(Life { ts, bid, px, qty: q, id, kind, src });
        if self.events.len() > EVENTS { self.events.pop_front(); }
    }

    /// Best `n` levels of a side, best first: (price, orders in queue order).
    pub fn levels(&self, bid: bool, n: usize) -> Vec<(f64, &VecDeque<Order>)> {
        let px = |k: &i64| self.px.get(k).copied().unwrap_or(*k as f64 / 1e6);
        if bid { self.bids.iter().rev().take(n).map(|(k, v)| (px(k), v)).collect() } else { self.asks.iter().take(n).map(|(k, v)| (px(k), v)).collect() }
    }

    // ---------------------------------------------------------------- exact

    /// A venue snapshot of its best resting orders. Orders that vanished inside the price range
    /// the snapshot covers were filled (fills seen) or cancelled; ones past its far edge may just
    /// have dropped out of the window and are forgotten without an event.
    pub fn exact_snapshot(&mut self, bids: &[Resting], asks: &[Resting], ts: i64) {
        for (bid, list) in [(true, bids), (false, asks)] {
            let edge = list.iter().map(|o| key(o.px)).reduce(|a, b| if bid { a.min(b) } else { a.max(b) });
            let ids: HashMap<u64, &Resting> = list.iter().map(|o| (o.id, o)).collect();
            // gone
            let gone: Vec<(u64, i64)> = self.index.iter().filter(|(id, (b, _))| *b == bid && !ids.contains_key(id)).map(|(id, (_, k))| (*id, *k)).collect();
            for (id, k) in gone {
                self.index.remove(&id);
                let inside = edge.is_some_and(|e| if bid { k >= e } else { k <= e });
                let Some(o) = self.take(bid, k, id) else { continue };
                let fills = self.filled.remove(&id).unwrap_or(0.0);
                if inside { self.log(ts, bid, k, o.qty, id, if fills >= o.qty * 0.999 { Kind::Fill } else { Kind::Cancel }, Src::Exact); }
            }
            // new and changed
            for o in list {
                let k = key(o.px);
                self.px.insert(k, o.px);
                match self.index.get(&o.id).copied() {
                    None => {
                        self.index.insert(o.id, (bid, k));
                        let started = self.started;
                        let ord = Order { id: o.id, qty: o.qty, initial: o.initial, since: ts, since_known: started, src: Src::Exact };
                        self.side(bid).entry(k).or_default().push_back(ord);
                        self.log(ts, bid, k, o.qty, o.id, Kind::Add, Src::Exact);
                    }
                    Some((_, k0)) => {
                        let Some(cur) = self.side(bid).get_mut(&k0).and_then(|v| v.iter_mut().find(|x| x.id == o.id)) else { continue };
                        let before = cur.qty;
                        cur.qty = o.qty;
                        if (o.qty - before).abs() > 1e-12 {
                            let fills = self.filled.remove(&o.id).unwrap_or(0.0);
                            let kind = if o.qty < before && fills > 0.0 { Kind::Partial } else { Kind::Amend };
                            self.log(ts, bid, k0, (before - o.qty).abs(), o.id, kind, Src::Exact);
                        }
                    }
                }
            }
            // queue order inside a level: venue sequence (time priority)
            let seq: HashMap<u64, u64> = list.iter().map(|o| (o.id, o.seq)).collect();
            for v in self.side(bid).values_mut() { v.make_contiguous().sort_by_key(|o| seq.get(&o.id).copied().unwrap_or(u64::MAX)); }
            self.side(bid).retain(|_, v| !v.is_empty());
        }
        self.started = true;
    }

    fn take(&mut self, bid: bool, k: i64, id: u64) -> Option<Order> {
        let side = self.side(bid);
        let v = side.get_mut(&k)?;
        let i = v.iter().position(|o| o.id == id)?;
        let o = v.remove(i);
        if v.is_empty() { side.remove(&k); }
        o
    }

    /// A trade against resting order `id` (maker id from the venue's trade stream).
    pub fn exact_fill(&mut self, id: u64, q: f64, ts: i64) {
        *self.filled.entry(id).or_default() += q;
        let Some((bid, k)) = self.index.get(&id).copied() else { return };
        let Some(o) = self.side(bid).get_mut(&k).and_then(|v| v.iter_mut().find(|x| x.id == id)) else { return };
        o.qty = (o.qty - q).max(0.0);
        let done = o.qty <= 1e-12;
        self.log(ts, bid, k, q, id, if done { Kind::Fill } else { Kind::Partial }, Src::Exact);
        if done { self.take(bid, k, id); self.index.remove(&id); self.filled.remove(&id); }
    }

    // ---------------------------------------------------------------- inferred

    /// Taker volume at a price since the last update (`buy` taker hits the asks).
    pub fn trade(&mut self, px: f64, q: f64, buy: bool) { *self.traded.entry((!buy, key(px))).or_default() += units(q); }

    /// One L2 level update with its order count; `snapshot` starts over (queues inside existing
    /// levels unknown: equal split, marked `Split`).
    pub fn infer_level(&mut self, bid: bool, px: f64, q: f64, n: u32, ts: i64) {
        let k = key(px);
        self.px.insert(k, px);
        let (q, n) = (units(q), n as i64);
        let traded = self.traded.remove(&(bid, k)).unwrap_or(0);
        let mut v = self.side(bid).remove(&k).unwrap_or_default();
        let (q0, n0) = (v.iter().map(|o| units(o.qty)).sum::<i64>(), v.len() as i64);
        let (dq, dn) = (q - q0, n - n0);
        let mut ev: Vec<(i64, u64, Kind, Src)> = vec![];
        let mut tr = traded;
        let out = |o: &Order, tr: &mut i64, ev: &mut Vec<(i64, u64, Kind, Src)>| {
            let u = units(o.qty);
            let kind = if *tr >= u { *tr -= u; Kind::Fill } else { Kind::Cancel };
            ev.push((u, o.id, kind, o.src));
        };
        if n == 0 {
            for o in v.drain(..) { out(&o, &mut tr, &mut ev); }
        } else if n0 == 0 {
            if n == 1 { let o = self.order(q, ts, Src::Inferred); ev.push((q, o.id, Kind::Add, Src::Inferred)); v.push_back(o); } else { self.split(&mut v, q, n, ts); }
        } else if dn == 1 && dq > 0 {
            let o = self.order(dq, ts, Src::Inferred);
            ev.push((dq, o.id, Kind::Add, Src::Inferred));
            v.push_back(o);
        } else if dn == -1 && dq < 0 {
            let sz = -dq;
            let i = if tr >= sz && v.front().is_some_and(|o| units(o.qty) == sz) { Some(0) } else { v.iter().position(|o| units(o.qty) == sz) }
                .or_else(|| (0..v.len()).min_by_key(|&i| (units(v[i].qty) - sz).abs()));
            if let Some(i) = i { let o = v.remove(i).unwrap(); out(&o, &mut tr, &mut ev); }
        } else if dn == 0 && dq != 0 {
            if dq < 0 {
                // a partial fill eats the front; otherwise some order was amended down (largest that fits)
                let mut left = -dq;
                if tr > 0 {
                    while left > 0 && !v.is_empty() {
                        let f = v.front_mut().unwrap();
                        let u = units(f.qty);
                        if u > left { f.qty = qty(u - left); ev.push((left, f.id, Kind::Partial, f.src)); left = 0; } else { left -= u; let o = v.pop_front().unwrap(); ev.push((u, o.id, Kind::Fill, o.src)); }
                    }
                }
                if left > 0 {
                    if let Some(i) = (0..v.len()).filter(|&i| units(v[i].qty) > left).max_by_key(|&i| units(v[i].qty)) {
                        v[i].qty = qty(units(v[i].qty) - left);
                        ev.push((left, v[i].id, Kind::Amend, Src::Inferred));
                    }
                }
            } else {
                // an amend up loses priority on most venues: the back order grows
                if let Some(b) = v.back_mut() { b.qty = qty(units(b.qty) + dq); ev.push((dq, b.id, Kind::Amend, Src::Inferred)); }
            }
        } else if dn < 0 {
            let k_rm = (-dn) as usize;
            match pick(&v, k_rm, -dq) {
                Some(idx) => { for &i in idx.iter().rev() { let o = v.remove(i).unwrap(); out(&o, &mut tr, &mut ev); } }
                None => { for _ in 0..k_rm.min(v.len()) { let o = if tr > 0 { v.pop_front() } else { v.pop_back() }.unwrap(); out(&o, &mut tr, &mut ev); } }
            }
        } else if dn > 0 {
            if dq <= 0 {
                if let Some(i) = (0..v.len()).filter(|&i| units(v[i].qty) + dq > 0).min_by_key(|&i| units(v[i].qty)) {
                    let o = v.remove(i).unwrap();
                    out(&o, &mut tr, &mut ev);
                }
            }
            self.split(&mut v, q, n, ts);
        }
        // the level must hold exactly n orders summing to q; otherwise rescale / rebuild as a split
        let sum = v.iter().map(|o| units(o.qty)).sum::<i64>();
        if v.len() as i64 != n || sum != q {
            if v.len() as i64 == n && sum > 0 {
                let mut acc = 0;
                for o in v.iter_mut() { let u = (units(o.qty) as i128 * q as i128 / sum as i128) as i64; o.qty = qty(u); acc += u; if o.src == Src::Inferred { o.src = Src::Split; } }
                if let Some(b) = v.back_mut() { b.qty = qty(units(b.qty) + q - acc); }
            } else {
                v.clear();
                if n > 0 { self.split(&mut v, q, n, ts); }
            }
        }
        for (u, id, kind, src) in ev { self.log(ts, bid, k, qty(u), id, kind, src); }
        if n > 0 { self.side(bid).insert(k, v); }
    }

    /// A book snapshot (resubscribe): every level becomes an equal split of its count.
    pub fn infer_reset(&mut self) { self.bids.clear(); self.asks.clear(); self.traded.clear(); }
    /// After applying a snapshot: from now on changes are logged.
    pub fn infer_started(&mut self) { self.started = true; }

    fn order(&mut self, u: i64, ts: i64, src: Src) -> Order {
        self.next += 1;
        Order { id: self.next, qty: qty(u), initial: qty(u), since: ts, since_known: self.started, src }
    }

    /// Fill `v` up to `n` orders summing to `q`: the unknown part split equally (marked `Split`).
    fn split(&mut self, v: &mut VecDeque<Order>, q: i64, n: i64, ts: i64) {
        let have = v.iter().map(|o| units(o.qty)).sum::<i64>();
        let k = n - v.len() as i64;
        if k <= 0 || q < have { return; }
        let each = (q - have) / k;
        let mut rem = (q - have) - each * k;
        for _ in 0..k {
            let extra = if rem > 0 { rem -= 1; 1 } else { 0 };
            let o = self.order(each + extra, ts, if k == 1 { Src::Inferred } else { Src::Split });
            v.push_back(o);
        }
    }
}

/// Indices of exactly `k` orders whose sizes sum to `need` (bounded search).
fn pick(v: &VecDeque<Order>, k: usize, need: i64) -> Option<Vec<usize>> {
    if k == 0 || k > v.len() || v.len() > 40 { return None; }
    fn rec(v: &VecDeque<Order>, start: usize, k: usize, need: i64, cur: &mut Vec<usize>, budget: &mut u32) -> bool {
        if k == 0 { return need == 0; }
        for i in start..v.len() {
            if *budget == 0 { return false; }
            *budget -= 1;
            let u = units(v[i].qty);
            if u > need { continue; }
            cur.push(i);
            if rec(v, i + 1, k - 1, need - u, cur, budget) { return true; }
            cur.pop();
        }
        false
    }
    let (mut cur, mut budget) = (vec![], 20_000u32);
    rec(v, 0, k, need, &mut cur, &mut budget).then_some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tracks_adds_partials_fills_and_cancels() {
        let r = |id, px, q, seq| Resting { id, px, qty: q, initial: q, seq };
        let mut b = L3Book::default();
        b.exact_snapshot(&[r(1, 100.0, 1.0, 1), r(2, 100.0, 2.0, 2), r(3, 99.0, 5.0, 3)], &[r(9, 101.0, 1.0, 1)], 0);
        assert!(b.events.is_empty(), "the first snapshot is the starting state");
        assert_eq!(b.levels(true, 1)[0].1.iter().map(|o| o.id).collect::<Vec<_>>(), vec![1, 2]);
        b.exact_fill(1, 0.4, 10);
        assert_eq!(b.events.back().unwrap().kind, Kind::Partial);
        b.exact_fill(1, 0.6, 11);
        assert_eq!(b.events.back().unwrap().kind, Kind::Fill);
        // order 2 cancelled inside the range, order 4 new; order 3 at the far edge stays
        b.exact_snapshot(&[r(4, 100.5, 3.0, 4), r(3, 99.0, 5.0, 3)], &[r(9, 101.0, 1.0, 1)], 20);
        let kinds: Vec<Kind> = b.events.iter().skip(2).map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::Cancel, Kind::Add]);
        assert_eq!(b.levels(true, 2).iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![100.5, 99.0]);
    }

    #[test]
    fn inference_reads_single_order_changes() {
        let mut b = L3Book::default();
        b.infer_level(true, 100.0, 3.0, 2, 0);
        b.infer_started();
        assert!(b.levels(true, 1)[0].1.iter().all(|o| o.src == Src::Split));
        // +1 order of 2: it joins the back of the queue
        b.infer_level(true, 100.0, 5.0, 3, 10);
        let q: Vec<f64> = b.levels(true, 1)[0].1.iter().map(|o| o.qty).collect();
        assert_eq!(q, vec![1.5, 1.5, 2.0]);
        assert_eq!(b.events.back().unwrap().kind, Kind::Add);
        // the front order traded away: a fill, not a cancel
        b.trade(100.0, 1.5, false);
        b.infer_level(true, 100.0, 3.5, 2, 20);
        assert_eq!(b.events.back().unwrap().kind, Kind::Fill);
        // the 2.0 order pulled without trades: a cancel of exactly that order
        b.infer_level(true, 100.0, 1.5, 1, 30);
        let last = b.events.back().unwrap();
        assert_eq!((last.kind, last.qty), (Kind::Cancel, 2.0));
        b.infer_level(true, 100.0, 0.0, 0, 40);
        assert!(b.levels(true, 1).is_empty());
    }
}
