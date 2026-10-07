//! Live connector check: `probe <exchange> <market> <base> [quote] [seconds]`.
//! Prints the first sample of every event kind, then a summary with sanity checks.
//! Exits non-zero if a required event kind is missing or a check fails.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use terminal_one::*;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: probe <exchange> <spot|margin|perp|option> <base> [quote=USDT] [seconds=20]";
    let ex = a.first().and_then(|s| Exchange::parse(s)).expect(usage);
    let market = a.get(1).and_then(|s| Market::parse(s)).expect(usage);
    let base = a.get(2).expect(usage).to_uppercase();
    let quote = a.get(3).map(|s| s.to_uppercase()).unwrap_or("USDT".into());
    let secs: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(20);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tasks = ex::spawn(ex, &Sub { market, base, quote }, tx);
    if tasks.is_empty() {
        println!("{ex:?} {market:?}: unsupported");
        std::process::exit(2);
    }

    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    let mut last: HashMap<&str, Msg> = HashMap::new();
    let mut books: HashMap<String, Book> = HashMap::new();
    let mut symbols = HashSet::new();
    let mut bad: Vec<String> = vec![];
    let mut lag = (0i64, 0usize);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);

    while let Ok(Some(m)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let k = m.ev.kind();
        let n = count.entry(k).or_default();
        if *n == 0 { println!("first {k:<8} {} {:?}", m.symbol, m.ev); }
        *n += 1;
        symbols.insert(m.symbol.to_string());
        if k != "optinfo" && k != "ls" && k != "oi" { lag.0 += m.recv - m.ts; lag.1 += 1; }
        match &m.ev {
            Event::Book { snapshot, bids, asks } => {
                let b = books.entry(m.symbol.to_string()).or_default();
                b.apply(*snapshot, bids, asks);
                if let (Some(bb), Some(ba)) = (b.best_bid(), b.best_ask()) {
                    if bb.0 >= ba.0 && bad.len() < 5 { bad.push(format!("crossed book {} bid {} >= ask {}", m.symbol, bb.0, ba.0)); }
                }
            }
            Event::Bbo { bid, ask, .. } if *bid > 0.0 && *ask > 0.0 && bid > ask && bad.len() < 5 => {
                bad.push(format!("crossed bbo {} bid {bid} > ask {ask}", m.symbol));
            }
            _ => {}
        }
        last.insert(k, m);
    }

    println!("\n== {ex:?} {market:?} over {secs}s, {} symbols", symbols.len());
    for (k, n) in &count { println!("{k:<8} {n:>7}  last: {:?}", last[k].ev); }
    if lag.1 > 0 { println!("avg exchange->local lag: {} ms", lag.0 / lag.1 as i64); }

    let required: &[&str] = match market {
        Market::Spot | Market::Margin => &["trade", "book", "bbo"],
        Market::Perp => &["trade", "book", "bbo", "mark"],
        Market::Option => &["greeks", "mark", "optinfo"],
    };
    for r in required { if !count.contains_key(r) { bad.push(format!("missing {r}")); } }

    // Book top must sit near the BBO and trades near the book (single-symbol markets).
    if market != Market::Option {
        if let (Some(b), Some(Msg { ev: Event::Bbo { bid, ask, .. }, symbol, .. })) = (books.values().next(), last.get("bbo")) {
            let mid = (bid + ask) / 2.0;
            if let (Some(bb), Some(ba)) = (b.best_bid(), b.best_ask()) {
                println!("book {symbol}: {} x {} levels, top {} / {}", b.depth().0, b.depth().1, bb.0, ba.0);
                if ((bb.0 + ba.0) / 2.0 - mid).abs() / mid > 0.002 { bad.push(format!("book mid {} far from bbo mid {mid}", (bb.0 + ba.0) / 2.0)); }
            }
            if let Some(Msg { ev: Event::Trade { px, .. }, .. }) = last.get("trade") {
                if (px - mid).abs() / mid > 0.01 { bad.push(format!("trade {px} far from bbo mid {mid}")); }
            }
        }
    }

    if bad.is_empty() {
        println!("OK");
    } else {
        for b in &bad { println!("FAIL {b}"); }
        std::process::exit(1);
    }
}
