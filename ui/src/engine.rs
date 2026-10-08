//! Background data engine: one tokio runtime per subscribed base asset, feeding a shared `Agg`.
//! Switching the base drops the whole runtime, which cancels every connector task (some
//! connectors spawn nested tasks, so aborting handles one by one would leak).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::VecDeque;
use terminal_one::trade::{self, Balance, Keys, Mode, OpenOrder, OrderReq, Position, Rules};
use terminal_one::{agg::Agg, ex, now_ms, Event, Exchange, History, Market, Msg, Sub};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub const HISTORY_MINUTES: usize = 1000;
const HISTORY_MARKETS: [Market; 2] = [Market::Spot, Market::Perp];

/// Private account state for venues with Keychain keys, refreshed every 2s.
#[derive(Default)]
pub struct Account {
    pub keys: HashMap<Exchange, Keys>,
    pub positions: Vec<Position>,
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
    /// client-side execution jobs (TWAP), newest last
    pub algos: Vec<AlgoJob>,
    /// venue maximum leverage per (venue, symbol), read when the leverage sheet opens
    pub max_levs: HashMap<(Exchange, String), f64>,
    /// order / trade / closed-PnL history per venue with fetch time; loaded on demand
    pub history: HashMap<Exchange, (trade::History, i64)>,
    pub history_loading: std::collections::HashSet<Exchange>,
    pub transferring: bool,
    /// auto top-up rules per venue (persisted in settings.json) and the last time one fired
    pub auto: HashMap<Exchange, AutoTopUp>,
    auto_last: HashMap<Exchange, i64>,
}

/// Keep the margin account's available balance topped up: when it drops below `min` USD, move
/// USDT from the venue's other accounts (Bybit FUND, EARN; Binance SPOT, FUNDING, EARN) up to
/// `target`. At most once a minute per venue; every transfer lands in the order log.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AutoTopUp { pub enabled: bool, pub min: f64, pub target: f64 }

impl Account {
    fn note(&mut self, msg: String, ok: bool) {
        if !ok { eprintln!("[trade] {msg}"); }
        self.log.push_back((now_ms(), msg, ok));
        if self.log.len() > 200 { self.log.pop_front(); }
    }
}

/// One USDT perpetual for the symbol picker.
#[derive(Clone, Debug)]
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
async fn place_logged(account: &Arc<Mutex<Account>>, ex: Exchange, req: &OrderReq, ref_px: f64) -> Option<String> {
    let k = account.lock().unwrap().keys.get(&ex).cloned()?;
    let key = (ex, req.symbol.clone());
    let cached = account.lock().unwrap().rules.get(&key).copied();
    let rules = match cached { Some(r) => Ok(r), None => trade::rules(ex, &req.symbol).await };
    let cached_mode = account.lock().unwrap().modes.get(&key).copied().flatten();
    let mode = match cached_mode { Some(m) => Ok(m), None => trade::mode(ex, &k, &req.symbol).await };
    let t0 = std::time::Instant::now();
    let res = match (rules, mode) {
        (Ok(r), Ok(m)) => {
            {
                let mut a = account.lock().unwrap();
                a.rules.insert(key.clone(), r);
                a.modes.insert(key, Some(m));
            }
            trade::place(ex, &k, req, &r, ref_px, m).await
        }
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    let rtt = t0.elapsed().as_secs_f64() * 1e3;
    let mut a = account.lock().unwrap();
    let what = format!("{ex:?} {} {} {} {} {}", req.symbol, if req.close { "close" } else { "open" }, if req.pos == terminal_one::Side::Buy { "long" } else { "short" }, req.qty, req.kind.label());
    match res {
        Ok(id) => {
            let r = a.order_rtt.entry(ex).or_insert(rtt);
            *r += (rtt - *r) * 0.3;
            a.note(format!("OK {what} id {id} ({rtt:.0} ms)"), true);
            Some(id)
        }
        Err(e) => { a.note(format!("FAIL {what}: {e:#}"), false); None }
    }
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
    /// symbol universe for the picker, by 24h quote volume (kept across base switches)
    pub tickers: Arc<Mutex<Vec<Ticker>>>,
    /// (venue, "BTCUSDT") -> funding, for every listed perp (positions in symbols not on screen)
    pub funding: Arc<Mutex<HashMap<(Exchange, String), VenueFunding>>>,
    /// detected signals for the current base (quant::detect every 5 s, per-kind cooldown)
    pub signals: Arc<Mutex<terminal_one::quant::Feed>>,
    /// private trading state (kept across base switches)
    pub account: Arc<Mutex<Account>>,
    /// set by the insurance panel: option chains are needed
    pub wants_options: std::sync::atomic::AtomicBool,
    /// venues switched off in the status bar: never connected (applied on `start`)
    pub off: std::collections::HashSet<Exchange>,
    /// private account streams live on their own runtime so base switches don't drop them
    acct_rt: Option<tokio::runtime::Runtime>,
    /// private-stream sender and running stream per venue (replaced when keys change)
    acct_tx: Option<tokio::sync::mpsc::UnboundedSender<(Exchange, trade::AccEvent)>>,
    streams: HashMap<Exchange, tokio::task::JoinHandle<()>>,
    /// result of the last "test connection" per venue: (ok, message)
    pub key_tests: Arc<Mutex<HashMap<Exchange, (bool, String)>>>,
}

impl Engine {
    pub fn new() -> Self {
        Engine { agg: Default::default(), stats: Default::default(), base: String::new(), rt: None, tx: None, started: vec![], tfs: vec![], tickers: Default::default(), funding: Default::default(), signals: Default::default(), account: {
            let mut a = Account::default();
            for ex in trade::TRADABLE { if let Some(k) = trade::keychain(ex) { a.keys.insert(ex, k); } }
            Arc::new(Mutex::new(a))
        }, wants_options: Default::default(), off: Default::default(), acct_rt: None, acct_tx: None, streams: HashMap::new(), key_tests: Default::default() }
    }

    /// (Re)subscribe everything for `base`: spot + perp live and history, USDT/USD, heatmap sampling.
    pub fn start(&mut self, base: &str, ctx: &eframe::egui::Context) {
        if let Some(rt) = self.rt.take() { rt.shutdown_background(); }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
        self.base = base.to_uppercase();
        self.agg = Arc::new(Mutex::new(Agg::default()));
        self.stats = Arc::new(Mutex::new(Stats { hist_pending: self.venues().len() * HISTORY_MARKETS.len(), hist_total: self.venues().len() * HISTORY_MARKETS.len(), ..Default::default() }));
        self.started = vec![];
        self.tfs = vec![];
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
            let pid = std::process::id().to_string();
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let out = tokio::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().await;
                let mb = out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok()).map(|kb| kb / 1024);
                stats_m.lock().unwrap().rss_mb = mb;
            }
        });

        // signals: one detection pass every 5 s over complete 1m bars of the aggregated series
        self.signals = Arc::new(Mutex::new(Default::default()));
        let (agg_s, feed, base_s) = (self.agg.clone(), self.signals.clone(), self.base.clone());
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let now = terminal_one::now_ms();
                let found = {
                    let a = agg_s.lock().unwrap();
                    let bars = |m: Market| a.series.get(&(None, m)).map(|s| s.bars.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
                    let (perp, spot) = (bars(Market::Perp), bars(Market::Spot));
                    let trades: Vec<(i64, f64, bool)> = a.trades.iter().filter(|t| t.market == Market::Perp)
                        .map(|t| (t.ts, t.px * t.qty * a.usd(t.ex, t.market), t.side == terminal_one::Side::Buy)).collect();
                    let index = a.mid(Market::Perp);
                    let dislocation: Vec<(String, f64)> = a.venues.iter().filter(|((_, m), _)| *m == Market::Perp)
                        .filter_map(|((e, m), v)| Some((format!("{e:?}"), (v.mid()? * a.usd(*e, *m) / index? - 1.0) * 1e4))).collect();
                    let input = terminal_one::quant::Input {
                        perp: terminal_one::quant::complete(&perp, now), spot: terminal_one::quant::complete(&spot, now),
                        trades: &trades, dislocation: &dislocation, base: &base_s,
                    };
                    terminal_one::quant::detect(&input, now)
                };
                let mut f = feed.lock().unwrap();
                for sg in found { f.offer(sg, 600_000); }
            }
        });

        let tickers = self.tickers.clone();
        let funding = self.funding.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let (t, f) = all_tickers().await;
                if !t.is_empty() { *tickers.lock().unwrap() = t; }
                if !f.is_empty() { funding.lock().unwrap().extend(f); }
            }
        });

        let agg = self.agg.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop { tick.tick().await; agg.lock().unwrap().sample_heatmap(now_ms()); }
        });

        self.tx = Some(tx);
        self.rt = Some(rt);
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
        for (ex, k) in &keys { self.streams.insert(*ex, rt.spawn(trade::stream(*ex, k.clone(), tx.clone()))); }
        self.acct_tx = Some(tx.clone());
        let (account, ctx) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
            let mut buf = Vec::with_capacity(1024);
            let mut dirty: std::collections::HashSet<Exchange> = Default::default();
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
                        {
                            let mut a = account.lock().unwrap();
                            for (ex, ev) in buf.drain(..) {
                                match ev {
                                    trade::AccEvent::Resync => resync.push(ex),
                                    trade::AccEvent::BalanceDirty => { dirty.insert(ex); }
                                    ev => {
                                        let wallet = matches!(ev, trade::AccEvent::Wallet { .. });
                                        changed |= apply(&mut a, ex, ev);
                                        // run after the lock is released: spawn_top_up locks the account itself
                                        if wallet { if let Some(need) = auto_due(&mut a, ex) { top_ups.push((ex, need)); } }
                                    }
                                }
                            }
                        }
                        for ex in resync { let acc = account.clone(); tokio::spawn(async move { refresh_venue(&acc, ex).await }); }
                        for (ex, need) in top_ups { spawn_top_up(&account, ex, need, &ctx); }
                        if changed { ctx.request_repaint(); }
                    }
                    _ = bal_tick.tick() => {
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
                        for ex in venues { let acc = account.clone(); tokio::spawn(async move { refresh_venue(&acc, ex).await }); }
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
        for e in self.venues() { ex::spawn(e, &self.sub(market), tx.clone()); }
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
    pub fn submit(&self, ex: Exchange, req: OrderReq, ref_px: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            place_logged(&account, ex, &req, ref_px).await;
            ctx.request_repaint();
        });
    }

    /// Scaled (ladder) order: `n` limit orders from `from` to `to`, sizes tilted by `skew`.
    pub fn submit_scaled(&self, ex: Exchange, base: OrderReq, from: f64, to: f64, n: usize, skew: f64, post_only: bool, ctx: &eframe::egui::Context) -> Result<usize, String> {
        let rules = self.account.lock().unwrap().rules.get(&(ex, base.symbol.clone())).copied().ok_or("venue rules not loaded yet: try again in a second")?;
        let legs = terminal_one::algo::scaled(base.qty, from, to, n, skew, rules.tick, rules.step);
        if legs.is_empty() { return Err("nothing to place: check size, prices and count".into()); }
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return Err("not connected".into()) };
        let count = legs.len();
        let tif = if post_only { trade::Tif::PostOnly } else { trade::Tif::Gtc };
        rt.spawn(async move {
            // one after another: venues rate-limit bursts of new orders per second
            for (px, q) in legs {
                let req = OrderReq { kind: trade::Kind::Limit { price: px, tif }, qty: q, client_id: None, ..base.clone() };
                place_logged(&account, ex, &req, px).await;
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
        let sizes = terminal_one::algo::twap_slices(base.qty, slices, rules.step);
        if sizes.is_empty() || sizes.iter().any(|q| *q < rules.min_qty) { return Err(format!("each slice must be at least {} (try fewer slices)", rules.min_qty)); }
        let (Some(rt), account, agg, ctx) = (self.acct_rt.as_ref(), self.account.clone(), self.agg.clone(), ctx.clone()) else { return Err("not connected".into()) };
        let id = terminal_one::now_ms() as u64;
        let gap = Duration::from_secs_f64(minutes * 60.0 / sizes.len() as f64);
        let buy = base.side() == terminal_one::Side::Buy;
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy, close: base.close, total: base.qty, sent: 0.0, slices: sizes.len(), done: 0,
            started_ms: terminal_one::now_ms(), end_ms: terminal_one::now_ms() + (minutes * 60_000.0) as i64, status: "running".into(), cancelled: false, venue_id: None,
        });
        rt.spawn(async move {
            let mut i = 0;
            while i < sizes.len() {
                if account.lock().unwrap().algos.iter().any(|j| j.id == id && j.cancelled) { break; }
                // the venue's own book; stale (pair switched away, venue down) means wait
                let bbo = {
                    let a = agg.lock().unwrap();
                    a.venues.get(&(ex, terminal_one::Market::Perp)).filter(|v| terminal_one::now_ms() - v.ts < 5_000)
                        .and_then(|v| match (v.book.best_bid(), v.book.best_ask()) { (Some(b), Some(k)) => Some((b.0, k.0)), _ => v.bbo.map(|[b, _, k, _]| (b, k)) })
                };
                let px = bbo.and_then(|(b, k)| terminal_one::algo::twap_price(buy, b, k, max_slip_bps, limit));
                let Some(px) = px else {
                    set_status(&account, id, if bbo.is_none() { "waiting: no fresh book for this pair (keep it open)" } else { "waiting: price beyond your limit" });
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                };
                set_status(&account, id, "running");
                let req = OrderReq { kind: trade::Kind::Limit { price: px, tif: trade::Tif::Ioc }, qty: sizes[i], client_id: None, ..base.clone() };
                place_logged(&account, ex, &req, px).await;
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
        let id = terminal_one::now_ms() as u64;
        let now = terminal_one::now_ms();
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy: base.side() == terminal_one::Side::Buy, close: base.close, total: base.qty, sent: 0.0, slices: 0, done: 0,
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
            let mut a = account.lock().unwrap();
            let what = format!("{ex:?} {} TWAP {} {} over {minutes} min", base.symbol, if base.side() == terminal_one::Side::Buy { "buy" } else { "sell" }, base.qty);
            match res {
                Ok(vid) => {
                    a.note(format!("OK {what} id {vid}"), true);
                    if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.venue_id = Some(vid); j.status = format!("running on {ex:?}"); }
                }
                Err(e) => {
                    a.note(format!("FAIL {what}: {e:#}"), false);
                    if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = format!("failed: {e:#}"); j.cancelled = true; j.end_ms = terminal_one::now_ms(); }
                }
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
                    Ok(()) => { a.note(format!("OK {ex:?} {symbol} TWAP {vid} cancelled"), true); if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = "cancelled".into(); j.end_ms = terminal_one::now_ms(); } }
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
            let res = trade::cancel(o.ex, &k, &o.symbol, &o.id).await;
            let ok = res.is_ok();
            account.lock().unwrap().note(match res { Ok(()) => format!("OK cancel {:?} {} {}", o.ex, o.symbol, o.id), Err(e) => format!("FAIL cancel {:?} {}: {e:#}", o.ex, o.id) }, ok);
            ctx.request_repaint();
        });
    }

    /// Set / replace whole-position TP and/or SL (None leaves a side as is), or add one partial
    /// level (`partial = Some(qty)`); the result lands in the order log, the stream updates state.
    #[allow(clippy::too_many_arguments)]
    pub fn set_tpsl(&self, ex: Exchange, symbol: String, pos: terminal_one::Side, tp: Option<f64>, sl: Option<f64>, partial: Option<f64>, trigger: trade::tpsl::Trigger, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (account, ctx) = (self.account.clone(), ctx.clone());
        rt.spawn(async move {
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
            let what = format!("{ex:?} {symbol} {} TP {} SL {}{}", if pos == terminal_one::Side::Buy { "long" } else { "short" },
                tp.map(|x| x.to_string()).unwrap_or("-".into()), sl.map(|x| x.to_string()).unwrap_or("-".into()), partial.map(|q| format!(" size {q}")).unwrap_or_default());
            let mut a = account.lock().unwrap();
            match res { Ok(_) => a.note(format!("OK TP/SL {what}"), true), Err(e) => a.note(format!("FAIL TP/SL {what}: {e:#}"), false) }
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
        if let (Some(k), Some(rt), Some(tx)) = (keys, self.acct_rt.as_ref(), self.acct_tx.clone()) {
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
                    Ok(h) => { a.history.insert(ex, (h, now_ms())); }
                    Err(e) => a.note(format!("{ex:?} history: {e:#}"), false),
                }
                a.history_loading.remove(&ex);
                ctx.request_repaint();
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
                    Ok(lines) => for l in lines { a.note(format!("OK {l}"), true); },
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

fn cache_path(ex: Exchange, sub: &Sub, tf: u32) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Caches/TerminalOne/history")
        .join(&sub.base).join(format!("{ex:?}-{:?}-{tf}m.json", sub.market)))
}

/// History from the local cache plus only the candles since its last bar; a full fetch when
/// there is no usable cache. On fetch failure the cache alone is used. The result is written back.
async fn cached_history(ex: Exchange, sub: &Sub, tf: u32, stats: &Arc<Mutex<Stats>>) -> History {
    let path = cache_path(ex, sub, tf);
    let cached: Option<History> = path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
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
    if let Some(p) = path.filter(|_| !h.klines.is_empty()) {
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        if let Ok(b) = serde_json::to_vec(&h) { let _ = std::fs::write(p, b); }
    }
    h
}

/// REST snapshot of one venue's positions, open orders and balance (replaces that venue's rows).
async fn refresh_venue(account: &Arc<Mutex<Account>>, ex: Exchange) {
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (p, o, b) = tokio::join!(trade::positions(ex, &k), trade::open_orders(ex, &k), trade::balance(ex, &k));
    // TP/SL snapshot (Binance: 40 weight for the account; snapshots only run on resync / 5 min)
    if matches!(ex, Exchange::Bybit | Exchange::Binance) {
        if let Ok(t) = trade::tpsl::list(ex, &k).await {
            let mut a = account.lock().unwrap();
            a.tpsl.retain(|x| x.ex != ex);
            a.tpsl.extend(t);
        }
    }
    let mut a = account.lock().unwrap();
    match (p, o, b) {
        (Ok(p), Ok(o), Ok(b)) => {
            a.positions.retain(|x| x.ex != ex);
            a.positions.extend(p);
            a.orders.retain(|x| x.ex != ex);
            a.orders.extend(o);
            a.balances.insert(ex, b);
            a.errors.remove(&ex);
        }
        (p, o, b) => {
            let e = [p.err(), o.err(), b.err()].into_iter().flatten().next().map(|e| format!("{e:#}")).unwrap_or_default();
            if a.errors.get(&ex) != Some(&e) { a.note(format!("{ex:?} account: {e}"), false); }
            a.errors.insert(ex, e);
        }
    }
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
                Ok(lines) => { for l in lines { a.note(format!("AUTO {l}"), true); } left -= amt; }
                Err(e) => a.note(format!("AUTO FAIL {ex:?} {src} -> {margin} {amt:.2} USDT: {e:#}"), false),
            }
        }
        if left >= 1.0 { account.lock().unwrap().note(format!("AUTO {ex:?}: {left:.2} USDT short, nothing left to move"), false); }
        reload_wallets(&account, ex, &k).await;
        ctx.request_repaint();
    });
}

/// Apply one pushed account change; true when something visible changed.
fn apply(a: &mut Account, ex: Exchange, ev: trade::AccEvent) -> bool {
    use trade::AccEvent as E;
    match ev {
        E::Position { mut p, one_way } => {
            let same = |x: &Position| x.ex == ex && x.symbol == p.symbol && (one_way || x.side == p.side);
            let old = a.positions.iter().find(|x| same(x)).cloned();
            a.positions.retain(|x| !same(x));
            if p.qty > 0.0 {
                // fields a push leaves out keep their last known value
                if let Some(o) = old {
                    if p.mark == 0.0 { p.mark = o.mark; }
                    if p.liq.is_none() { p.liq = o.liq; }
                    if p.lev == 0.0 { p.lev = o.lev; }
                    if p.margin == 0.0 { p.margin = o.margin; }
                }
                if p.mark == 0.0 { p.mark = p.entry; }
                if p.lev > 0.0 { a.levs.insert((ex, p.symbol.clone()), p.lev); }
                a.positions.push(p);
            }
            true
        }
        E::Mark { symbol, mark } => {
            let mut changed = false;
            for p in a.positions.iter_mut().filter(|p| p.ex == ex && p.symbol == symbol) {
                let sign = if p.side == terminal_one::Side::Buy { 1.0 } else { -1.0 };
                p.mark = mark;
                p.upnl = (mark - p.entry) * p.qty * sign;
                changed = true;
            }
            changed
        }
        E::Order(o) => { a.orders.retain(|x| !(x.ex == ex && x.id == o.id)); a.orders.push(o); true }
        E::OrderDone { id } => { a.orders.retain(|x| !(x.ex == ex && x.id == id)); true }
        E::Wallet { equity, available } => {
            let b = a.balances.entry(ex).or_default();
            b.equity = equity;
            b.available = available;
            true
        }
        E::TpSl(t) => { a.tpsl.retain(|x| !(x.ex == ex && x.id == t.id)); a.tpsl.push(t); true }
        E::TpSlDone { id } => { a.tpsl.retain(|x| !(x.ex == ex && x.id == id)); true }
        E::Resync | E::BalanceDirty => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_one::Side;
    use trade::AccEvent as E;

    fn pos(side: Side, qty: f64) -> Position {
        Position { ex: Exchange::Binance, symbol: "BTCUSDT".into(), side, qty, entry: 100.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None }
    }

    #[test]
    fn pushes_merge_into_account_state() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        // REST snapshot row with fields a push does not carry
        a.positions.push(Position { liq: Some(80.0), lev: 10.0, margin: 50.0, mark: 101.0, ..pos(Side::Buy, 1.0) });
        // hedge push for the long keeps liq/lev/margin/mark, short added alongside
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 2.0), one_way: false });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 1.0), one_way: false });
        assert_eq!(a.positions.len(), 2);
        let long = a.positions.iter().find(|p| p.side == Side::Buy).unwrap();
        assert_eq!((long.qty, long.liq, long.lev, long.mark), (2.0, Some(80.0), 10.0, 101.0));
        // mark drives upnl on both sides
        apply(&mut a, ex, E::Mark { symbol: "BTCUSDT".into(), mark: 110.0 });
        let upnl: Vec<f64> = a.positions.iter().map(|p| p.upnl).collect();
        assert!(upnl.contains(&20.0) && upnl.contains(&-10.0), "{upnl:?}");
        // a one-way zero row clears the symbol on either side
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 0.0), one_way: true });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 0.0), one_way: true });
        assert!(a.positions.is_empty());
        // orders upsert by id and leave on done
        let o = OpenOrder { ex, symbol: "BTCUSDT".into(), id: "7".into(), side: Side::Buy, price: 99.0, qty: 1.0, filled: 0.0, kind: "LIMIT".into(), reduce_only: false, ts: 0, pos: None };
        apply(&mut a, ex, E::Order(o.clone()));
        apply(&mut a, ex, E::Order(OpenOrder { filled: 0.5, ..o }));
        assert_eq!(a.orders.len(), 1);
        assert_eq!(a.orders[0].filled, 0.5);
        apply(&mut a, ex, E::OrderDone { id: "7".into() });
        assert!(a.orders.is_empty());
    }
}

/// Every USDT/USDC-quoted base on the major venues (Binance futures + spot, Bybit linear, OKX swaps,
/// Hyperliquid perps), merged by base: price and 24h change from the first venue that lists it,
/// quote volume summed. A venue that fails is skipped.
async fn all_tickers() -> (Vec<Ticker>, HashMap<(Exchange, String), VenueFunding>) {
    use terminal_one::{num, ws};
    let ok = |b: &str| !b.is_empty() && !b.contains('_') && !b.contains('-') && b.is_ascii();
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
            if let Some(b) = x["symbol"].as_str().and_then(|s| s.strip_suffix("USDT")) { push(b, num(&x["lastPrice"]), num(&x["price24hPcnt"]) * 100.0, num(&x["turnover24h"]), num(&x["highPrice24h"]), num(&x["lowPrice24h"]), num(&x["volume24h"])); }
        },
        Err(e) => eprintln!("[tickers bybit] {e:#}"),
    }
    match ox {
        Ok(v) => for x in v["data"].as_array().into_iter().flatten() {
            if let Some(b) = x["instId"].as_str().and_then(|s| s.strip_suffix("-USDT-SWAP")) {
                let (l, o) = (num(&x["last"]), num(&x["open24h"]));
                // volCcy24h is in base units for USDT swaps
                push(b, l, if o > 0.0 { (l / o - 1.0) * 100.0 } else { 0.0 }, num(&x["volCcy24h"]) * l, num(&x["high24h"]), num(&x["low24h"]), num(&x["volCcy24h"]));
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
                if let Some(b) = u["name"].as_str() {
                    fund.insert((Exchange::Hyperliquid, format!("{b}USDT")), VenueFunding { rate: num(&c["funding"]), interval_h: 1.0, next_ms: (terminal_one::now_ms() / 3_600_000 + 1) * 3_600_000 });
                    push(b, l, if p > 0.0 { (l / p - 1.0) * 100.0 } else { 0.0 }, num(&c["dayNtlVlm"]), 0.0, 0.0, num(&c["dayBaseVlm"]));
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
