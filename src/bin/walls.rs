//! Liquidity-wall detectors side by side on live data: `walls <base> [minutes=20]`.
//! heat = the chart's current rule (60 one-second heatmap columns, mean size, 80% presence);
//! fast = the same size floor met in every one of the last 10 columns;
//! l3   = the same floor met by ONE resting order (Lighter exact, OKX / Hyperliquid inferred) aged 10 s.
//! Also: for each fast wall, the biggest order-by-order (L3) order resting in it, to see whether
//! walls carried by one aged order last / hold better than walls made of many orders.
//! Prints per detector: count, lifetime (short = flashed / spoofed), how much earlier than heat it
//! saw the same wall, and what price did when it reached a wall.
use std::collections::HashMap;
use std::time::Duration;
use depth::{agg::Agg, l3::Src, *};

const MIN_AGE: i64 = 10_000;
const NAMES: [&str; 3] = ["heat", "fast", "l3"];

/// One detected wall, open while the detector keeps reporting it. `l3` = biggest share of the wall
/// one aged L3 order made up while it was open.
struct Ep { bid: bool, px: f64, start: i64, end: i64, ex: String, l3: f64 }

#[derive(Default)]
struct Det { open: HashMap<u64, Ep>, done: Vec<Ep> }

impl Det {
    /// `seen`: (key, bid, px, venue) reported this second.
    fn step(&mut self, now: i64, seen: Vec<(u64, bool, f64, String)>) {
        for (k, bid, px, ex) in &seen {
            self.open.entry(*k).or_insert(Ep { bid: *bid, px: *px, start: now, end: now, ex: ex.clone(), l3: 0.0 }).end = now;
        }
        let gone: Vec<u64> = self.open.keys().filter(|k| !seen.iter().any(|s| s.0 == **k)).copied().collect();
        for k in gone { self.done.push(self.open.remove(&k).unwrap()); }
    }
    fn all(&self) -> Vec<&Ep> { self.done.iter().chain(self.open.values()).collect() }
}

/// Price reaching a wall: did it hold for the next 60 s (never more than 2 bins through)?
struct Touch { det: usize, px: f64, bid: bool, at: i64, worst: f64, l3: f64 }

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() { return f64::NAN; }
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn life(eps: &[&Ep]) -> String {
    let dur: Vec<f64> = eps.iter().map(|e| (e.end - e.start) as f64 / 1000.0 + 1.0).collect();
    let short = dur.iter().filter(|s| **s < 30.0).count();
    format!("{:>4} walls, lasted p25 {:>4.0}s median {:>4.0}s p75 {:>4.0}s, gone within 30 s {:>3.0}%",
        dur.len(), pct(&dur, 0.25), pct(&dur, 0.5), pct(&dur, 0.75), short as f64 * 100.0 / dur.len().max(1) as f64)
}

fn held(t: &[&Touch], bin: f64) -> String {
    let h = t.iter().filter(|t| t.worst < 2.0 * bin).count();
    format!("touched {:>3}, held {h} ({:.0}%)", t.len(), h as f64 * 100.0 / t.len().max(1) as f64)
}

/// Biggest aged, single (not equal-split) L3 order within one bin of `px` on that side, aligned to the composite mid.
fn biggest_l3(a: &Agg, mid: f64, px: f64, bid: bool, bin: f64, now: i64) -> f64 {
    let mut best: f64 = 0.0;
    for ((ex, m), book) in a.l3.iter().filter(|((_, m), _)| *m == Market::Perp) {
        let Some(vm) = a.venues.get(&(*ex, *m)).and_then(|v| v.book_mid()) else { continue };
        let k = mid / vm;
        for (p, orders) in book.levels(bid, usize::MAX) {
            if (p * k - px).abs() > bin { continue; }
            for o in orders.iter().filter(|o| o.src != Src::Split && now - o.since >= MIN_AGE) { best = best.max(o.qty); }
        }
    }
    best
}

#[tokio::main]
async fn main() {
    depth::ws::cli_proxy();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let base = a.first().map(|s| s.to_uppercase()).unwrap_or("BTC".into());
    let minutes: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sub = Sub { market: Market::Perp, base: base.clone(), quote: "USDT".into() };
    let mut tasks = vec![];
    for ex in Exchange::ALL { tasks.extend(ex::spawn(ex, &sub, tx.clone())); }
    tasks.push(ex::lighter::spawn_l3(&sub, tx.clone()));
    tasks.push(ex::hyperliquid::spawn_l3(&sub, tx.clone()));
    let mut agg = Agg::default();
    let mut det: [Det; 3] = Default::default();
    let mut touches: Vec<Touch> = vec![];
    let (mut prev_heat, mut prev_fast, mut prev_ids): (Vec<f64>, Vec<f64>, Vec<u64>) = (vec![], vec![], vec![]);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(minutes * 60);
    let mut secs = 0u64;
    loop {
        tokio::select! {
            m = rx.recv() => { if let Some(m) = m { agg.on(&m); } }
            _ = tick.tick() => {
                let now = now_ms();
                agg.sample_heatmap(now);
                secs += 1;
                let (Some(mid), Some(h)) = (agg.mid(Market::Perp), agg.heat.get(&Market::Perp)) else { continue };
                let bin = h.bin;
                // price walls are keyed by bin (a wall is a place); order walls by order id
                let key = |px: f64| (px / bin).round() as u64;
                let (b, k) = h.walls(mid, 60, &prev_heat);
                prev_heat = b.iter().chain(&k).map(|w| w.0).collect();
                det[0].step(now, b.iter().map(|w| (key(w.0), true, w.0, "agg".into())).chain(k.iter().map(|w| (key(w.0), false, w.0, "agg".into()))).collect());
                let (b, k) = h.walls_fast(mid, 10, 60, &prev_fast);
                prev_fast = b.iter().chain(&k).map(|w| w.0).collect();
                let sizes: HashMap<u64, f64> = b.iter().chain(&k).map(|w| (key(w.0), w.1)).collect();
                det[1].step(now, b.iter().map(|w| (key(w.0), true, w.0, "agg".into())).chain(k.iter().map(|w| (key(w.0), false, w.0, "agg".into()))).collect());
                for (kk, e) in det[1].open.iter_mut() {
                    let share = biggest_l3(&agg, mid, e.px, e.bid, bin, now) / sizes.get(kk).copied().unwrap_or(f64::MAX);
                    e.l3 = e.l3.max(share);
                }
                let (Some(floor), Some(stay)) = (h.wall_floor(60, false), h.wall_floor(60, true)) else { continue };
                let (ob, oa) = agg.order_walls(Market::Perp, floor, stay, MIN_AGE, 2.0 * bin, &prev_ids, now);
                prev_ids = ob.iter().chain(&oa).map(|w| w.id).collect();
                det[2].step(now, ob.iter().chain(&oa).map(|w| (w.id ^ ((w.ex as u64) << 56), w.bid, w.px, format!("{:?}/{:?}", w.ex, w.src))).collect());
                // touches: the mid within half a bin of an open wall starts one; worst = how far through it went in 60 s
                for (i, d) in det.iter().enumerate() {
                    for e in d.open.values() {
                        if (mid - e.px).abs() <= bin * 0.5 && !touches.iter().any(|t| t.det == i && (t.px - e.px).abs() < bin && now - t.at < 60_000) {
                            touches.push(Touch { det: i, px: e.px, bid: e.bid, at: now, worst: 0.0, l3: e.l3 });
                        }
                    }
                }
                for t in touches.iter_mut().filter(|t| now - t.at < 60_000) {
                    t.worst = t.worst.max(if t.bid { t.px - mid } else { mid - t.px });
                }
                let end = tokio::time::Instant::now() >= deadline;
                if secs % 300 == 0 || end {
                    println!("\n==== {base} after {} min  (bin {bin}, floor {floor:.2} {base} = {:.0} USD, mid {mid:.2})", secs / 60, floor * mid);
                    let done: Vec<&Touch> = touches.iter().filter(|t| now - t.at >= 60_000).collect();
                    for (i, d) in det.iter().enumerate() {
                        let eps = d.all();
                        let t: Vec<&Touch> = done.iter().copied().filter(|t| t.det == i).collect();
                        println!("{:<5} {}  | {}", NAMES[i], life(&eps), held(&t, bin));
                        if i > 0 {
                            // lead over heat: same side, within 1.5 bins, overlapping in time
                            let lead: Vec<f64> = det[0].all().iter().filter_map(|h| eps.iter()
                                .filter(|o| o.bid == h.bid && (o.px - h.px).abs() <= 1.5 * bin && o.start <= h.end && o.end + 60_000 >= h.start)
                                .map(|o| (h.start - o.start) as f64 / 1000.0).reduce(f64::max)).collect();
                            println!("      heat walls it also saw: {} of {}, earlier by median {:.0}s (p25 {:.0}s, p75 {:.0}s)",
                                lead.len(), det[0].all().len(), pct(&lead, 0.5), pct(&lead, 0.25), pct(&lead, 0.75));
                        }
                        if i == 2 {
                            let mut by: HashMap<&str, usize> = HashMap::new();
                            for e in &eps { *by.entry(e.ex.as_str()).or_default() += 1; }
                            println!("      by venue {by:?}");
                        }
                    }
                    // the L3 question: fast walls carried by one aged order vs the rest
                    let fast = det[1].all();
                    let (one, many): (Vec<&Ep>, Vec<&Ep>) = fast.iter().partition(|e| e.l3 >= 0.25);
                    let tf: Vec<&Touch> = done.iter().copied().filter(|t| t.det == 1).collect();
                    let (t1, tm): (Vec<&Touch>, Vec<&Touch>) = tf.iter().partition(|t| t.l3 >= 0.25);
                    println!("fast walls with one aged L3 order >= 25% of the wall: {}  | {}", life(&one), held(&t1, bin));
                    println!("fast walls without:                                  {}  | {}", life(&many), held(&tm, bin));
                }
                if end { break; }
            }
        }
    }
    for t in tasks { t.abort(); }
}
