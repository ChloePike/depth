//! Read-only account check: `account [BASE=BTC]`. Prints instrument rules for every tradable
//! venue and, where Keychain keys exist, balance / positions / open orders. Never places orders.
use terminal_one::trade;

#[tokio::main]
async fn main() {
    let base = std::env::args().nth(1).unwrap_or("BTC".into()).to_uppercase();
    let sym = trade::symbol(&base);
    // `account BTC pm`: raw Portfolio Margin account fields (read-only, weight 20)
    if std::env::args().nth(2).as_deref() == Some("pm") {
        if let Some(k) = trade::keychain(terminal_one::Exchange::Binance) {
            match trade::pm_account(&k).await { Ok(v) => println!("{v:#}"), Err(e) => println!("FAIL {e:#}") }
        }
        return;
    }
    // `account BTC stream 30`: print private-stream events (marks counted, not printed)
    if std::env::args().nth(2).as_deref() == Some("stream") {
        let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(30);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for ex in trade::TRADABLE { if let Some(k) = trade::keychain(ex) { tokio::spawn(trade::stream(ex, k, tx.clone())); } }
        let end = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut marks = 0u64;
        while let Ok(Some((ex, ev))) = tokio::time::timeout_at(end, rx.recv()).await {
            match ev { trade::AccEvent::Mark { .. } => marks += 1, ev => println!("{ex:?} {ev:?}") }
        }
        println!("mark updates: {marks}");
        return;
    }
    println!("{}", if trade::testnet() { "TESTNET" } else { "MAINNET" });
    let mut bad = false;
    for ex in trade::TRADABLE {
        match trade::rules(ex, &sym).await {
            Ok(r) => println!("{ex:?} {sym} rules: tick {} step {} min_qty {} min_notional {}", r.tick, r.step, r.min_qty, r.min_notional),
            Err(e) => { println!("FAIL {ex:?} rules: {e:#}"); bad = true; }
        }
        let Some(k) = trade::keychain(ex) else { println!("{ex:?}: no keys in Keychain. Add with:\n{}", trade::keychain_hint(ex)); continue };
        match trade::balance(ex, &k).await {
            Ok(b) => {
                println!("{ex:?} balance: equity {:.2} available {:.2}", b.equity, b.available);
                let (ws, notes) = trade::wallets(ex, &k).await;
                for w in ws { println!("  {}: {:.2} USD  {}", w.id, w.usd, w.coins.iter().take(5).map(|c| format!("{} {} (free {})", c.coin, c.qty, c.free)).collect::<Vec<_>>().join(", ")); }
                for e in notes { println!("  WARN {e}"); }
            }
            Err(e) => { println!("FAIL {ex:?} balance: {e:#}"); bad = true; }
        }
        match trade::positions(ex, &k).await {
            Ok(ps) => { println!("{ex:?} positions: {}", ps.len()); for p in ps { println!("  {} {:?} {} @ {} mark {} upnl {:.2} liq {:?} lev {} margin {:.2}", p.symbol, p.side, p.qty, p.entry, p.mark, p.upnl, p.liq, p.lev, p.margin); } }
            Err(e) => { println!("FAIL {ex:?} positions: {e:#}"); bad = true; }
        }
        match trade::open_orders(ex, &k).await {
            Ok(os) => { println!("{ex:?} open orders: {}", os.len()); for o in os { println!("  {} {:?} {} @ {} ({})", o.symbol, o.side, o.qty, o.price, o.kind); } }
            Err(e) => { println!("FAIL {ex:?} open orders: {e:#}"); bad = true; }
        }
    }
    if bad { std::process::exit(1); }
}
