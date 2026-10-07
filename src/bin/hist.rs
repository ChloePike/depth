//! History check: `hist <exchange> <spot|margin|perp> <base> [quote=USDT] [minutes=1000]`.
//! Fetches the startup REST history and validates it. Non-zero exit on failure.
use terminal_one::*;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: hist <exchange> <spot|margin|perp> <base> [quote=USDT] [bars=1000] [tf minutes=1]";
    let ex = a.first().and_then(|s| Exchange::parse(s)).expect(usage);
    let market = a.get(1).and_then(|s| Market::parse(s)).expect(usage);
    let base = a.get(2).expect(usage).to_uppercase();
    let quote = a.get(3).map(|s| s.to_uppercase()).unwrap_or("USDT".into());
    let minutes: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let tf: u32 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);
    let step = tf as i64 * 60_000;

    let t0 = std::time::Instant::now();
    let h = match ex::history_tf(ex, &Sub { market, base, quote }, tf, minutes).await {
        Ok(h) => h,
        Err(e) => { println!("FAIL history error: {e:#}"); std::process::exit(1) }
    };
    let now = now_ms();
    let mut bad = vec![];
    let age = |t: i64| format!("{:.1}h ago", (now - t) as f64 / 3.6e6);
    println!("{ex:?} {market:?} {tf}m history in {:?}", t0.elapsed());

    let k = &h.klines;
    if let (Some(f), Some(l)) = (k.first(), k.last()) {
        let gaps = k.windows(2).filter(|w| w[1].t - w[0].t != step).count();
        if k.iter().any(|x| x.t % step != 0) { bad.push(format!("klines not aligned to {tf}m")); }
        if gaps > 0 { bad.push(format!("{gaps} gaps between {tf}m klines")); }
        let with_buy = k.iter().filter(|x| x.buy.is_some()).count();
        println!("klines   {:>5}  {} .. {}  gaps {gaps}  with taker-buy {with_buy}", k.len(), age(f.t), age(l.t));
        println!("         last {:?}", l);
        if k.windows(2).any(|w| w[1].t <= w[0].t) { bad.push("klines not strictly ascending".into()); }
        if now - l.t > step + 2 * 60_000 { bad.push(format!("last kline is {} old", age(l.t))); }
        if k.len() < minutes * 9 / 10 && market != Market::Option { bad.push(format!("only {} klines for {minutes} requested", k.len())); }
        if k.iter().any(|x| !(x.l <= x.o.min(x.c) && x.h >= x.o.max(x.c) && x.l > 0.0 && x.vol >= 0.0)) { bad.push("kline with invalid OHLC/volume".into()); }
        if k.iter().any(|x| x.buy.is_some_and(|b| b < 0.0 || b > x.vol * 1.0001)) { bad.push("taker-buy outside 0..vol".into()); }
    } else {
        bad.push("no klines".into());
    }
    for (name, s) in [("oi", &h.oi), ("funding", &h.funding), ("long/short", &h.long_short)] {
        if let (Some(f), Some(l)) = (s.first(), s.last()) {
            println!("{name:<8} {:>5}  {} .. {}  last {:?}", s.len(), age(f.0), age(l.0), l.1);
            if s.windows(2).any(|w| w[1].0 < w[0].0) { bad.push(format!("{name} not ascending")); }
        } else {
            println!("{name:<8}     0");
        }
    }
    if let Some(l) = h.taker.last() { println!("taker    {:>5}  last {:?}", h.taker.len(), l); }
    if h.funding.iter().any(|(_, r)| r.abs() > 0.003) { bad.push("funding magnitude > 0.3%/h: not converted to hourly?".into()); }
    if market == Market::Perp && h.oi.iter().any(|(_, x)| *x <= 0.0) { bad.push("non-positive OI".into()); }

    if bad.is_empty() { println!("OK"); } else { for b in &bad { println!("FAIL {b}"); } std::process::exit(1); }
}
