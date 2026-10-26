//! The demo account against one venue's live public data, without the GUI:
//! `demo <exchange> <base> [seconds=60]`. Places a market buy of the venue minimum, buy limits one
//! tick and 0.3% under the bid, reduce-only sell limits as far over the ask, a stop-market and a TP/SL
//! where the venue has them, prints every push of the demo stream, then closes everything.
//! Exits non-zero if the market buy fills off the venue's own touch, a limit fills without a trade
//! printing through it, or cash does not equal start + realized - fees + funding. The event log
//! goes to a throwaway database (removed at the end), never depth.db.
use depth::trade::{self, tpsl, AccEvent, Keys, Kind, Mode, OrderReq, Tif};
use depth::{agg::Agg, demo, ex, now_ms, Event, Exchange, Market, Side, Sub};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::main]
async fn main() {
    // before anything opens the database
    let db = std::env::temp_dir().join(format!("depth-demo-check-{}.db", std::process::id()));
    unsafe { std::env::set_var("T1_DB", &db); }
    depth::ws::cli_proxy();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: demo <exchange> <base> [seconds=60]";
    let ex = a.first().and_then(|s| Exchange::parse(s)).filter(|e| trade::TRADABLE.contains(e)).expect(usage);
    let base = a.get(1).expect(usage).to_uppercase();
    let secs: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let symbol = trade::symbol(&base);
    let t0 = now_ms();
    let at = move || format!("{:>5.1}s", (now_ms() - t0) as f64 / 1e3);

    demo::set_on(true);
    let agg = Arc::new(Mutex::new(Agg::default()));
    demo::set_market(agg.clone(), &base, Default::default());
    // the venue's perp feed through the same hook as the engine's pump; every trade price kept
    let prints: Arc<Mutex<Vec<(i64, f64)>>> = Default::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _feed = ex::spawn(ex, &Sub { market: Market::Perp, base: base.clone(), quote: "USDT".into() }, tx);
    let (agg_p, prints_p) = (agg.clone(), prints.clone());
    tokio::spawn(async move {
        let mut buf = Vec::with_capacity(4096);
        while rx.recv_many(&mut buf, 4096).await > 0 {
            { let mut g = agg_p.lock().unwrap(); for m in &buf { g.on(m); } }
            let now = now_ms();
            prints_p.lock().unwrap().extend(buf.iter().filter_map(|m| match m.ev { Event::Trade { px, .. } => Some((now, px)), _ => None }));
            demo::on_batch(&buf);
            buf.clear();
        }
    });
    // the demo venue's private stream, printed
    let k = Keys::demo();
    let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(trade::stream(ex, k.clone(), atx));
    let fills: Arc<Mutex<Vec<trade::Fill>>> = Default::default();
    let fills_p = fills.clone();
    tokio::spawn(async move {
        let mut held = f64::NAN;
        while let Some((_, e)) = arx.recv().await {
            match e {
                AccEvent::Fill(f) => { println!("{} fill     {:?} {} @ {} fee {:.4}", at(), f.side, f.qty, f.price, f.fee); fills_p.lock().unwrap().push(f); }
                AccEvent::Order(o) => println!("{} order    {} {:?} {} @ {} {}{}", at(), o.kind, o.side, o.qty, o.price, o.tif, o.trigger.map(|t| format!(" trigger {t}")).unwrap_or_default()),
                AccEvent::OrderDone { id } => println!("{} done     {id}", at()),
                AccEvent::TpSl(t) => println!("{} tp/sl    {} at {}", at(), if t.take_profit { "TP" } else { "SL" }, t.trigger_px),
                AccEvent::Position { p, .. } if p.qty > 0.0 && p.qty != held => { held = p.qty; println!("{} position {:?} {} entry {:.2} mark {:.2} liq {:.2}", at(), p.side, p.qty, p.entry, p.mark, p.liq.unwrap_or(0.0)); }
                AccEvent::Liquidated { .. } | AccEvent::Resync => println!("{} {e:?}", at()),
                _ => {}
            }
        }
    });

    let mut bad: Vec<String> = vec![];
    let top = || {
        let g = agg.lock().unwrap();
        let v = g.venues.get(&(ex, Market::Perp)).filter(|v| now_ms() - v.fresh < 5_000)?;
        Some((v.book.best_bid()?.0, v.book.best_ask()?.0))
    };
    let mut book = None;
    for _ in 0..150 { if let Some(b) = top() { book = Some(b); break; } tokio::time::sleep(Duration::from_millis(100)).await; }
    let Some((bid, ask)) = book else { eprintln!("{ex:?} {symbol}: no live book in 15 s"); std::process::exit(2) };
    let r = match trade::rules(ex, &symbol).await { Ok(r) => r, Err(e) => { eprintln!("{ex:?} {symbol} rules: {e:#}"); std::process::exit(2) } };
    let qty = trade::ceil_step((r.min_notional * 1.05 / ask).max(r.min_qty), r.step, r.min_qty);
    println!("{} {ex:?} {symbol}: bid {bid} ask {ask}, tick {} step {} min {} (notional {}), size {qty}", at(), r.tick, r.step, r.min_qty, r.min_notional);
    let order = |pos, close, kind, qty| OrderReq { symbol: symbol.clone(), pos, close, kind, qty, client_id: None, attach: None };

    // market buy: every fill at the venue's own ask side, within a few ticks of the touch seen just before
    let (bid, ask) = top().unwrap_or((bid, ask));
    match trade::place(ex, &k, &order(Side::Buy, false, Kind::Market, qty), &r, ask, Mode::OneWay).await {
        Ok(id) => println!("{} market buy {qty} placed: {id} (ask {ask})", at()),
        Err(e) => bad.push(format!("market buy refused: {e:#}")),
    }
    let band = (ask * 5e-4).max(r.tick * 3.0);
    for f in trade::history(ex, &k, &[]).await.map(|h| h.fills).unwrap_or_default() {
        if (f.price - ask).abs() > band { bad.push(format!("market fill {} @ {} is {:.2} from the ask {ask}", f.qty, f.price, f.price - ask)); }
    }
    // resting limits one tick and 0.3% outside the touch (the sells only reduce the long)
    let px = |x: f64| trade::fmt_step(x, r.tick, false).parse::<f64>().unwrap();
    let limits = [(Side::Buy, px(bid - r.tick)), (Side::Buy, px(bid * 0.997)), (Side::Sell, px(ask + r.tick)), (Side::Sell, px(ask * 1.003))];
    let placed = now_ms();
    for (side, price) in limits {
        let req = order(Side::Buy, side == Side::Sell, Kind::Limit { price, tif: Tif::Gtc }, qty);
        match trade::place(ex, &k, &req, &r, ask, Mode::OneWay).await { Ok(id) => println!("{} {side:?} limit @ {price} placed: {id}", at()), Err(e) => bad.push(format!("{side:?} limit {price}: {e:#}")) }
    }
    if trade::caps(ex).stop {
        let stop = order(Side::Buy, true, Kind::Stop { trigger: trade::fmt_step(bid * 0.998, r.tick, false).parse().unwrap(), limit: None, by_mark: true }, qty);
        match trade::place(ex, &k, &stop, &r, ask, Mode::OneWay).await { Ok(id) => println!("{} stop {} placed: {id}", at(), stop.kind.label()), Err(e) => bad.push(format!("stop: {e:#}")) }
    }
    if trade::caps(ex).tpsl {
        match tpsl::set_position_tpsl(ex, &k, &symbol, Side::Buy, Mode::OneWay, Some(ask * 1.004), Some(bid * 0.996), tpsl::Trigger::Mark).await {
            Ok(()) => println!("{} TP/SL set", at()),
            Err(e) => bad.push(format!("TP/SL: {e:#}")),
        }
    }

    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(15).min(end - tokio::time::Instant::now())).await;
        let b = trade::balance(ex, &k).await.unwrap_or_default();
        println!("{} balance  equity {:.4} available {:.4}", at(), b.equity, b.available);
    }

    // a limit fills only once a print went strictly through it (between its placement and the fill)
    let prints = prints.lock().unwrap().clone();
    let (lo, hi) = prints.iter().filter(|(t, _)| *t >= placed).fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), &(_, p)| (l.min(p), h.max(p)));
    for f in fills.lock().unwrap().iter().filter(|f| limits.contains(&(f.side, f.price))) {
        let through = prints.iter().any(|&(t, p)| t >= placed && t <= f.ts && if f.side == Side::Buy { p < f.price } else { p > f.price });
        println!("{} {:?} limit {} filled {} after a print through it: {through}", at(), f.side, f.price, f.qty);
        if !through { bad.push(format!("{:?} limit {} filled without a print through it", f.side, f.price)); }
    }
    println!("{} prints since the limits: {} in {lo} .. {hi}", at(), prints.iter().filter(|(t, _)| *t >= placed).count());

    // the books balance: cash = start + realized - fees + funding of what is open
    let (h, b, ps) = (trade::history(ex, &k, &[]).await.unwrap_or_default(), trade::balance(ex, &k).await.unwrap_or_default(), trade::positions(ex, &k).await.unwrap_or_default());
    let funding: f64 = ps.iter().filter_map(|p| demo::funding_paid(ex, &p.symbol, p.side)).map(|f| f.amount).sum();
    let cash = b.equity - ps.iter().map(|p| p.upnl).sum::<f64>();
    let want = demo::START_BALANCE + h.fills.iter().map(|f| f.realized.unwrap_or(0.0) - f.fee).sum::<f64>() + funding;
    if (cash - want).abs() > 1e-6 { bad.push(format!("cash {cash} != start + realized - fees + funding {want}")); }
    println!("\n== {ex:?} {symbol} after {secs}s");
    for p in &ps { println!("position {:?} {} entry {:.2} mark {:.2} liq {:.2} upnl {:.4} margin {:.2}", p.side, p.qty, p.entry, p.mark, p.liq.unwrap_or(0.0), p.upnl, p.margin); }
    for o in trade::open_orders(ex, &k).await.unwrap_or_default() { println!("open     {} {:?} {} @ {}{}", o.kind, o.side, o.qty, o.price, o.trigger.map(|t| format!(" trigger {t}")).unwrap_or_default()); }
    for t in tpsl::list(ex, &k).await.unwrap_or_default() { println!("tp/sl    {} at {}", if t.take_profit { "TP" } else { "SL" }, t.trigger_px); }
    for o in h.orders.iter().rev() { println!("history  {} {:?} {} filled {} avg {:.2} {}", o.kind, o.side, o.qty, o.filled, o.avg, o.status); }
    println!("fills {}, fees {:.4}, realized {:.4}, funding {funding:.4}", h.fills.len(), h.fills.iter().map(|f| f.fee).sum::<f64>(), h.fills.iter().filter_map(|f| f.realized).sum::<f64>());
    println!("balance  equity {:.4} available {:.4} cash {cash:.4} (expected {want:.4})", b.equity, b.available);

    // close everything
    let open = trade::open_orders(ex, &k).await.unwrap_or_default();
    let _ = trade::cancel_all(ex, &k, &open).await;
    let _ = tpsl::cancel_all(ex, &k, &tpsl::list(ex, &k).await.unwrap_or_default()).await;
    for p in &ps {
        let (bid, ask) = top().unwrap_or((p.mark, p.mark));
        if let Err(e) = trade::place(ex, &k, &order(p.side, true, Kind::Market, p.qty), &r, if p.side == Side::Buy { bid } else { ask }, Mode::OneWay).await { bad.push(format!("close: {e:#}")); }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = trade::balance(ex, &k).await.unwrap_or_default();
    println!("closed   equity {:.4} ({:+.4} USDT over the run)", b.equity, b.equity - demo::START_BALANCE);
    for ext in ["", "-wal", "-shm"] { let _ = std::fs::remove_file(format!("{}{ext}", db.display())); }
    if bad.is_empty() { println!("OK"); return; }
    for b in &bad { println!("FAIL {b}"); }
    std::process::exit(1);
}
