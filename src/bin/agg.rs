//! Live aggregation dashboard: `agg <base> [seconds=60]`.
//! Streams spot, perp and options for `base` from every exchange into one `Agg`, prints the
//! cross-venue view every 5s, then runs sanity checks. Non-zero exit if a check fails.
//! ponytail: margin is not streamed here (same books as spot); run `probe <ex> margin` for borrow rates.
use std::time::Duration;
use terminal_one::{agg::Agg, *};

fn bp(x: Option<f64>) -> String { x.map(|v| format!("{:+.4}", v * 1e4)).unwrap_or("-".into()) }
fn f(x: Option<f64>, d: usize) -> String { x.map(|v| format!("{v:.d$}")).unwrap_or("-".into()) }

fn print(a: &Agg, base: &str) {
    println!("\n==== {base} perps ====  (funding in bp per hour, basis in bp vs spot, USDT = {:.5} USD)", a.usd(Exchange::Binance, Market::Perp));
    println!("{:<12} {:>11} {:>10} {:>9} {:>12} {:>9} {:>10} {:>7}", "venue", "mark", "funding/h", "ann.%", "OI", "OI $M", "basis bp", "L/S");
    let mut perps: Vec<_> = a.venues.iter().filter(|((_, m), _)| *m == Market::Perp).collect();
    perps.sort_by(|x, y| y.1.oi_usd().unwrap_or(0.0).total_cmp(&x.1.oi_usd().unwrap_or(0.0)));
    for ((ex, _), v) in perps {
        println!("{:<12} {:>11} {:>10} {:>9} {:>12} {:>9} {:>10} {:>7}", format!("{ex:?}"), f(v.price(), 1), bp(v.funding_h),
            f(v.funding_h.map(|x| x * 24.0 * 365.0 * 100.0), 2), f(v.oi, 1), f(v.oi_usd().map(|x| x / 1e6), 1),
            f(a.basis_bps(*ex), 2), f(v.ls.get(&LsKind::Accounts).copied(), 2));
    }
    let (oi, oi_usd) = a.oi_total();
    println!("TOTAL        OI {oi:.0} {base} (${:.0}M)  OI-weighted funding {} bp/h  CVD perp {:+.1}  spot {:+.1}",
        oi_usd / 1e6, bp(a.funding_oi_weighted()), a.cvd_total(Market::Perp), a.cvd_total(Market::Spot));
    let liq: (f64, f64) = a.venues.values().fold((0.0, 0.0), |s, v| (s.0 + v.liq.0, s.1 + v.liq.1));
    println!("liquidated since start: longs {:.3} shorts {:.3} {base}", liq.0, liq.1);

    if let Some(mid) = a.mid(Market::Perp) {
        let bin = agg::nice(mid * 0.0002);
        let (b, k) = a.book(Market::Perp, bin, 0.01);
        let depth = |ls: &[agg::Level], pct: f64, up: bool| ls.iter().filter(|l| if up { l.px <= mid * (1.0 + pct) } else { l.px >= mid * (1.0 - pct) }).map(|l| l.qty).sum::<f64>();
        println!("agg perp book (bin {bin}): best {} / {}, depth 0.1% {:.1}/{:.1}  0.5% {:.1}/{:.1}  1% {:.1}/{:.1} {base}",
            b.first().map(|l| l.px).unwrap_or(0.0), k.first().map(|l| l.px).unwrap_or(0.0),
            depth(&b, 0.001, false), depth(&k, 0.001, true), depth(&b, 0.005, false), depth(&k, 0.005, true), depth(&b, 0.01, false), depth(&k, 0.01, true));
    }
    for (m, h) in &a.heat { println!("heatmap {m:?}: {} columns, bin {}, last column {} bins", h.cols.len(), h.bin, h.cols.back().map(|c| c.1.len()).unwrap_or(0)); }

    println!("\n==== {base} options (first 4 expiries per venue) ====");
    let mut chains: Vec<_> = a.chains.iter().collect();
    chains.sort_by_key(|(e, _)| format!("{e:?}"));
    for (ex, c) in chains {
        for e in c.expiries().iter().take(4) {
            let days = (e.expiry_ms - now_ms()) as f64 / 86_400_000.0;
            println!("{:<8} exp {:>6.1}d  n={:<4} fwd {:>10} ({:<6}) atm {:>8} iv {}", format!("{ex:?}"), days, e.n, f(e.forward, 1), e.forward_src,
                e.atm_strike, f(e.atm_iv.map(|v| v * 100.0), 1));
        }
    }
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let base = a.first().map(|s| s.to_uppercase()).unwrap_or("BTC".into());
    let secs: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = vec![];
    for ex in Exchange::ALL {
        for market in [Market::Spot, Market::Perp, Market::Option] {
            tasks.extend(ex::spawn(ex, &Sub { market, base: base.clone(), quote: "USDT".into() }, tx.clone()));
        }
    }
    // USDT/USD from Kraken and Coinbase books, on its own channel (it is not a BTC venue).
    let (fx_tx, mut fx_rx) = tokio::sync::mpsc::unbounded_channel();
    for ex in [Exchange::Kraken, Exchange::Coinbase] {
        tasks.extend(ex::spawn(ex, &Sub { market: Market::Spot, base: "USDT".into(), quote: "USD".into() }, fx_tx.clone()));
    }
    let mut fx: std::collections::HashMap<Exchange, f64> = Default::default();
    let mut agg = Agg::default();

    // Startup history (spot + perp, every venue) fetched concurrently while live streams warm up.
    const MINUTES: usize = 1000;
    let (h_tx, mut h_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending = 0;
    for ex in Exchange::ALL {
        for market in [Market::Spot, Market::Perp] {
            let (h_tx, sub) = (h_tx.clone(), Sub { market, base: base.clone(), quote: "USDT".into() });
            pending += 1;
            agg.expect_history(ex, market);
            tokio::spawn(async move {
                let t = std::time::Instant::now();
                let r = ex::history(ex, &sub, MINUTES).await;
                let _ = h_tx.send((ex, market, r, t.elapsed()));
            });
        }
    }
    let mut hist_report = vec![];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut n = 0u64;
    loop {
        tokio::select! {
            m = rx.recv() => { if let Some(m) = m { agg.on(&m); n += 1; } }
            h = h_rx.recv(), if pending > 0 => {
                let Some((ex, market, r, took)) = h else { continue };
                pending -= 1;
                match r {
                    Ok(h) => {
                        hist_report.push(format!("{ex:?} {market:?}: {} klines ({} with taker split), oi {}, funding {}, ls {}, taker {} in {took:?}",
                            h.klines.len(), h.klines.iter().filter(|k| k.buy.is_some()).count(), h.oi.len(), h.funding.len(), h.long_short.len(), h.taker.len()));
                        agg.load_history(ex, market, &h);
                    }
                    Err(e) => {
                        hist_report.push(format!("{ex:?} {market:?}: history error {e:#}"));
                        agg.load_history(ex, market, &History::default());
                    }
                }
                if pending == 0 {
                    agg.finish_history();
                    hist_report.sort();
                    println!("\n######## history loaded");
                    for l in &hist_report { println!("  {l}"); }
                }
            }
            m = fx_rx.recv() => {
                if let Some(Msg { ex, ev: Event::Bbo { bid, ask, .. }, .. }) = m {
                    if bid > 0.0 && ask > 0.0 { fx.insert(ex, (bid + ask) / 2.0); }
                    let mut v: Vec<f64> = fx.values().copied().collect();
                    v.sort_by(f64::total_cmp);
                    agg.set_fx("USDT", v[v.len() / 2]);
                }
            }
            _ = tick.tick() => {
                agg.sample_heatmap(now_ms());
                let left = deadline.saturating_duration_since(tokio::time::Instant::now()).as_secs();
                if left % 5 == 0 { println!("\n######## {n} messages, {left}s left"); print(&agg, &base); }
                if left == 0 { break; }
            }
        }
    }

    // sanity checks
    let mut bad = vec![];
    if pending > 0 { bad.push(format!("{pending} history fetches still pending")); }
    println!("\nper-venue perp series (bars / with oi / with funding, oldest oi, newest oi):");
    let mut keys: Vec<_> = agg.series.keys().filter(|(e, m)| e.is_some() && *m == Market::Perp).copied().collect();
    keys.sort_by_key(|k| format!("{k:?}"));
    for k in keys {
        let s = &agg.series[&k];
        let first_oi = s.bars.iter().find_map(|b| b.oi.map(|x| (b.t, x)));
        println!("  {:<12} {:>5} / {:>5} / {:>5}  first oi {:?}  last oi {:?}", format!("{:?}", k.0.unwrap()), s.bars.len(),
            s.bars.iter().filter(|b| b.oi.is_some()).count(), s.bars.iter().filter(|b| b.funding_h.is_some()).count(),
            first_oi.map(|(t, x)| (format!("{:.0}m ago", (now_ms() - t) as f64 / 6e4), x)), s.bars.back().and_then(|b| b.oi));
    }
    if let Some(s) = agg.series.get(&(None, Market::Perp)) {
        println!("\ncross-venue perp series: {} minute bars; last 4:", s.bars.len());
        for b in s.bars.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
            println!("  t-{:>3.0}m close {:>10.1} vol {:>9.1} buy {:>8.1} sell {:>8.1} cvd {:>+10.1} oi {:>9} funding/h pred {} settled {} basis {}",
                (now_ms() - b.t) as f64 / 6e4, b.c, b.vol, b.buy, b.sell, b.cvd, f(b.oi, 0), bp(b.funding_h), bp(b.funding_settled), f(b.basis_bps, 2));
        }
        // OI moves slowly: a >3% step between minutes means a stitching / coverage bug
        let steps: Vec<f64> = s.bars.iter().zip(s.bars.iter().skip(1))
            .filter_map(|(x, y)| Some((y.oi? / x.oi? - 1.0).abs())).collect();
        let worst = steps.iter().copied().fold(0.0, f64::max);
        println!("largest minute-to-minute cross-venue OI step: {:.3}%", worst * 100.0);
        if worst > 0.03 { bad.push(format!("cross-venue OI jumps {:.1}% between minutes", worst * 100.0)); }
        if s.bars.len() < 900 { bad.push(format!("only {} cross-venue perp bars after history", s.bars.len())); }
        if let (Some(b), (oi, _)) = (s.bars.back(), agg.oi_total()) {
            if b.oi.is_none_or(|x| (x / oi - 1.0).abs() > 0.1) { bad.push(format!("series OI {:?} vs live OI total {oi:.0}", b.oi)); }
        }
        if let (Some(first), Some(spot)) = (s.bars.front(), agg.spot_mid()) {
            if (first.c / spot - 1.0).abs() > 0.15 { bad.push(format!("oldest bar close {} implausible vs spot {spot}", first.c)); }
        }
    }
    let spot = agg.spot_mid();
    for ((ex, m), v) in &agg.venues {
        if let (Some(p), Some(s)) = (v.price(), spot) {
            if (p / s - 1.0).abs() > 0.01 { bad.push(format!("{ex:?} {m:?} price {p} is >1% from spot median {s}")); }
        }
        if let Some(fh) = v.funding_h { if fh.abs() > 0.001 { bad.push(format!("{ex:?} funding {fh}/h implausible (>0.1%/h)")); } }
    }
    for (ex, c) in &agg.chains {
        for e in c.expiries().iter().filter(|e| e.expiry_ms - now_ms() > 86_400_000).take(3) {
            if let (Some(fw), Some(s)) = (e.forward, spot) {
                if (fw / s - 1.0).abs() > 0.05 { bad.push(format!("{ex:?} forward {fw} for exp {} is >5% from spot {s}", e.expiry_ms)); }
            }
        }
    }
    for market in [Market::Spot, Market::Perp] {
        let (b, k) = agg.book(market, 0.01, 0.01);
        if let (Some(b), Some(k)) = (b.first(), k.first()) {
            // Venues legitimately disagree by a few bp (venue lag, funding regimes: USDT perps
            // vs USD/USDC perps). A cross wider than 15 bp means an FX or unit bug.
            let cross_bp = (b.px / k.px - 1.0) * 1e4;
            if cross_bp > 0.0 { println!("{market:?} cross-venue dispersion: best bid {} vs best ask {} ({cross_bp:.1} bp)", b.px, k.px); }
            if cross_bp > 15.0 {
                bad.push(format!("aggregated {market:?} book crossed by {cross_bp:.1} bp: bid {} > ask {}", b.px, k.px));
                for ((e, m), v) in agg.venues.iter().filter(|((_, m), _)| *m == market) {
                    let u = agg.usd(*e, *m);
                    if let (Some(bb), Some(ba)) = (v.book.best_bid(), v.book.best_ask()) {
                        println!("  {e:?} {m:?} top in USD: {:.2} / {:.2}  (native {} / {}, book ts age {} ms)", bb.0 * u, ba.0 * u, bb.0, ba.0, now_ms() - v.ts);
                    }
                }
            }
        }
    }
    if agg.usd(Exchange::Binance, Market::Perp) == 1.0 { bad.push("no USDT/USD rate received".into()); }
    let perps = agg.venues.keys().filter(|(_, m)| *m == Market::Perp).count();
    if perps < 9 { bad.push(format!("only {perps} perp venues reported")); }
    if bad.is_empty() { println!("\nOK"); } else { for b in &bad { println!("FAIL {b}"); } std::process::exit(1); }
}
