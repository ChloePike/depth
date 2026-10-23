//! Background data engine: one tokio runtime per subscribed base asset, feeding a shared `Agg`.
//! Switching the base drops the whole runtime, which cancels every connector task (some
//! connectors spawn nested tasks, so aborting handles one by one would leak).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::VecDeque;
use depth::trade::{self, Balance, Keys, Mode, OpenOrder, OrderReq, Position, Rules};
use depth::{agg::Agg, ex, now_ms, Event, Exchange, History, Market, Msg, Sub};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub const HISTORY_MINUTES: usize = 1000;
const HISTORY_MARKETS: [Market; 2] = [Market::Spot, Market::Perp];

/// Private account state for venues with Keychain keys, refreshed every 2s.
#[derive(Default)]
pub struct Account {
    pub keys: HashMap<Exchange, Keys>,
    /// protection buy priced for the insurance dialog: (venue, option symbol) -> plan or error
    pub opt_plan: Option<((Exchange, String), Result<trade::OptBuy, String>)>,
    /// per keyed venue: may the key trade (false = read-only); absent = unknown / not asked
    pub can_trade: HashMap<Exchange, bool>,
    pub positions: Vec<Position>,
    /// held options (bought protection), every venue
    pub opts: Vec<trade::OptPos>,
    /// spot: resting orders and wallet coins per venue (trade::spot::VENUES)
    pub spot_orders: Vec<trade::OpenOrder>,
    pub spot_bal: HashMap<Exchange, Vec<trade::spot::Bal>>,
    /// margin (Margin mode): Binance cross-margin orders of `margin_sym` (Bybit's come with spot_orders),
    /// margin wallet coins with loans per venue
    pub margin_orders: Vec<trade::OpenOrder>,
    pub margin_bal: HashMap<Exchange, Vec<trade::spot::Bal>>,
    pub margin_sym: Option<String>,
    /// resting option orders (option venues), refreshed on resync and after our own option orders
    pub opt_orders: Vec<trade::OpenOrder>,
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
    /// funding paid / received since open per open position (venue, symbol, side); see `funding_due`
    pub funding_paid: HashMap<(Exchange, String, depth::Side), FundingSlot>,
    /// client-side execution jobs (TWAP), newest last
    pub algos: Vec<AlgoJob>,
    /// venue maximum leverage per (venue, symbol), read when the leverage sheet opens
    pub max_levs: HashMap<(Exchange, String), f64>,
    /// order / trade / closed-PnL history per venue with fetch time; loaded on demand
    pub history: HashMap<Exchange, (trade::History, i64)>,
    pub history_loading: std::collections::HashSet<Exchange>,
    /// bumped when own fills were stored (the chart re-reads them from the db); (venue, symbol)
    /// pairs already back-filled over REST this session
    pub fills_ver: u64,
    /// live fills (fill sound + toast) and the newest one: pushed fills on Binance / Bybit, a
    /// pushed position size change on the other venues
    pub fill_seq: u64,
    pub last_fill: Option<FillNote>,
    /// when each venue's private stream last (re)subscribed: position pushes right after are its
    /// snapshot, not fills
    pub resync_ms: HashMap<Exchange, i64>,
    pub fills_done: std::collections::HashSet<(Exchange, String)>,
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
    pub(crate) fn note(&mut self, msg: String, ok: bool) {
        if !ok { eprintln!("[trade] {msg}"); }
        self.log.push_back((now_ms(), msg, ok));
        if self.log.len() > 200 { self.log.pop_front(); }
    }
}

/// A fill for the toast: venue, symbol, side, size, price when the venue said it.
#[derive(Clone, Debug)]
pub struct FillNote { pub ex: Exchange, pub symbol: String, pub buy: bool, pub qty: f64, pub price: Option<f64> }

/// One USDT perpetual for the symbol picker.
#[derive(Clone, Debug)]
#[derive(serde::Serialize, serde::Deserialize)]
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
/// Asks the venue once whether the key may trade (read-only keys disable the order buttons).
fn check_perms(rt: &tokio::runtime::Runtime, account: &Arc<Mutex<Account>>, ex: Exchange, k: Keys) {
    let account = account.clone();
    rt.spawn(async move {
        match trade::can_trade(ex, &k).await {
            Ok(Some(ok)) => { account.lock().unwrap().can_trade.insert(ex, ok); }
            Ok(None) => {}
            Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} key permissions: {e:#}\n")),
        }
    });
}

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
    let what = format!("{ex:?} {} {} {} {} {}", req.symbol, if req.close { "close" } else { "open" }, if req.pos == depth::Side::Buy { "long" } else { "short" }, req.qty, req.kind.label());
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

/// Funding-since-open of one position: the last result (None until fetched, or on venues that do not
/// report it), the size it was fetched at, when it was fetched and when it is due again (local ms).
#[derive(Clone, Debug)]
pub struct FundingSlot { pub val: Option<trade::FundingPaid>, qty: f64, at: i64, due: i64 }

/// Positions whose funding-since-open is due: never fetched, size changed (at most once per 60 s), or
/// ~20 s after a settlement. Marks them fetched now and drops the slots of closed positions.
/// Each comes with the open time an earlier fetch found for it (kept while the position lives).
fn funding_due(a: &mut Account, fund: &HashMap<(Exchange, String), VenueFunding>, now: i64) -> Vec<(Keys, Position, Option<i64>)> {
    let live: std::collections::HashSet<_> = a.positions.iter().map(|p| (p.ex, p.symbol.clone(), p.side)).collect();
    a.funding_paid.retain(|k, _| live.contains(k));
    let mut out = vec![];
    for p in a.positions.iter().filter(|p| p.qty > 0.0) {
        let Some(k) = a.keys.get(&p.ex) else { continue };
        let key = (p.ex, p.symbol.clone(), p.side);
        let old = a.funding_paid.get(&key);
        if old.is_some_and(|s| now < s.due && (s.qty == p.qty || now - s.at < 60_000)) { continue; }
        let slot = FundingSlot { val: old.and_then(|s| s.val), qty: p.qty, at: now, due: next_settle(fund.get(&(p.ex, p.symbol.clone())), now) + 20_000 };
        let known = slot.val.as_ref().filter(|v| !v.lower && v.since > 0).map(|v| v.since);
        a.funding_paid.insert(key, slot);
        out.push((k.clone(), p.clone(), known));
    }
    out
}

/// Next funding settlement after `now`: the listed time rolled forward by the interval when stale;
/// venues without a listed rate assume the common 00/08/16 UTC schedule.
fn next_settle(f: Option<&VenueFunding>, now: i64) -> i64 {
    let (next, iv) = f.map_or((0, 8 * 3_600_000), |f| (f.next_ms, ((f.interval_h * 3_600_000.0) as i64).max(3_600_000)));
    if next > now { next } else { now + iv - (now - next).rem_euclid(iv) }
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
    /// live connector tasks per venue, so one venue can be switched off or on alone
    venue_tasks: HashMap<Exchange, Vec<tokio::task::JoinHandle<()>>>,
    /// symbol universe for the picker, by 24h quote volume (kept across base switches)
    pub tickers: Arc<Mutex<Vec<Ticker>>>,
    /// (venue, "BTCUSDT") -> funding, for every listed perp (positions in symbols not on screen)
    pub funding: Arc<Mutex<HashMap<(Exchange, String), VenueFunding>>>,
    /// alternative.me crypto Fear & Greed (0 fear .. 100 greed, daily) and its label
    pub fng: Arc<Mutex<Option<(f64, String)>>>,
    /// detected signals for the current base (quant::detect every 5 s, per-kind cooldown)
    pub signals: Arc<Mutex<depth::quant::Feed>>,
    /// private trading state (kept across base switches)
    pub account: Arc<Mutex<Account>>,
    /// set by the insurance panel: option chains are needed
    pub wants_options: std::sync::atomic::AtomicBool,
    /// set by the L3 view: start the exact order feeds (Lighter's order snapshots are REST polls,
    /// so they run only once someone looks)
    pub wants_l3: std::sync::atomic::AtomicBool,
    l3_started: bool,
    /// Hyperliquid's L3 feed in use (public l2Book, or Flowscan when `l3_flowscan`)
    hl_l3: Option<(bool, tokio::task::JoinHandle<()>)>,
    pub l3_flowscan: bool,
    /// venues switched off in the status bar: never connected (applied on `start`)
    pub off: std::collections::HashSet<Exchange>,
    /// private account streams live on their own runtime so base switches don't drop them
    acct_rt: Option<tokio::runtime::Runtime>,
    /// private-stream sender and running stream per venue (replaced when keys change)
    acct_tx: Option<tokio::sync::mpsc::UnboundedSender<(Exchange, trade::AccEvent)>>,
    streams: HashMap<Exchange, tokio::task::JoinHandle<()>>,
    /// result of the last "test connection" per venue: (ok, message)
    pub key_tests: Arc<Mutex<HashMap<Exchange, (bool, String)>>>,
    /// price alerts on symbols not on screen: one venue's spot stream per base, on its own runtime
    watch_rt: Option<tokio::runtime::Runtime>,
    watching: HashMap<String, Vec<tokio::task::JoinHandle<()>>>,
    /// last trade price per watched base (USDT)
    pub watched: Arc<Mutex<HashMap<String, f64>>>,
}

impl Engine {
    pub fn new() -> Self {
        Engine { agg: Default::default(), stats: Default::default(), base: String::new(), rt: None, tx: None, started: vec![], tfs: vec![], venue_tasks: HashMap::new(), tickers: Default::default(), funding: Default::default(), fng: Default::default(), signals: Default::default(), account: {
            let mut a = Account::default();
            for ex in trade::TRADABLE { if let Some(k) = trade::keychain(ex) { a.keys.insert(ex, k); } }
            Arc::new(Mutex::new(a))
        }, wants_options: Default::default(), wants_l3: Default::default(), l3_started: false, hl_l3: None, l3_flowscan: false, off: Default::default(), acct_rt: None, acct_tx: None, streams: HashMap::new(), key_tests: Default::default(), watch_rt: None, watching: HashMap::new(), watched: Default::default() }
    }

    /// (Re)subscribe everything for `base`: spot + perp live and history, USDT/USD, heatmap sampling.
    pub fn start(&mut self, base: &str, ctx: &eframe::egui::Context) {
        if let Some(rt) = self.rt.take() { rt.shutdown_background(); }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
        self.base = base.to_uppercase();
        self.agg = Arc::new(Mutex::new(Agg::default()));
        self.agg.lock().unwrap().load_opt_hist(&self.base);
        self.stats = Arc::new(Mutex::new(Stats { hist_pending: self.venues().len() * HISTORY_MARKETS.len(), hist_total: self.venues().len() * HISTORY_MARKETS.len(), ..Default::default() }));
        self.started = vec![];
        self.tfs = vec![];
        self.venue_tasks.clear();
        self.l3_started = false;
        self.hl_l3 = None;
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
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let mb = tokio::task::spawn_blocking(depth::sys::rss_mb).await.ok().flatten();
                stats_m.lock().unwrap().rss_mb = mb;
            }
        });

        // signals: one detection pass every 5 s over complete 1m bars of the aggregated series
        self.signals = Arc::new(Mutex::new(Default::default()));
        let (agg_s, feed, base_s) = (self.agg.clone(), self.signals.clone(), self.base.clone());
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            // algo detections already reported (key -> last seen): each algo once while it runs, again after 30 min quiet
            let mut algos_seen: HashMap<String, i64> = HashMap::new();
            loop {
                tick.tick().await;
                let now = depth::now_ms();
                let algos = {
                    let mut a = agg_s.lock().unwrap();
                    let vols: Vec<f64> = a.series.get(&(None, Market::Perp)).map(|s| s.bars.iter().rev().take(120).map(|b| b.vol * b.c).collect()).unwrap_or_default();
                    let vol_1m = depth::quant::robust(&vols).map_or(0.0, |x| x.0);
                    let found = depth::algoscan::significant(&a.scan, now, vol_1m, |e, m| a.usdt(e, m),
                        |e, m, bid, px| a.venues.get(&(e, m)).is_some_and(|v| v.book.qty_at(bid, px) > 0.0));
                    let s = depth::algoscan::signals(&found, now, vol_1m, |e, m| a.usdt(e, m));
                    a.scan.remember(now, found);
                    s
                };
                algos_seen.retain(|_, t| now - *t < 30 * 60_000);
                let algos: Vec<_> = algos.into_iter().filter(|(k, _)| algos_seen.insert(k.clone(), now).is_none()).map(|(_, s)| s).collect();
                let found = {
                    let a = agg_s.lock().unwrap();
                    let bars = |m: Market| a.series.get(&(None, m)).map(|s| s.bars.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
                    let (perp, spot) = (bars(Market::Perp), bars(Market::Spot));
                    let trades: Vec<(i64, f64, bool)> = a.trades.iter().filter(|t| t.market == Market::Perp)
                        .map(|t| (t.ts, t.px * t.qty * a.usdt(t.ex, t.market), t.side == depth::Side::Buy)).collect();
                    let index = a.mid(Market::Perp);
                    let dislocation: Vec<(String, f64)> = a.venues.iter().filter(|((_, m), v)| *m == Market::Perp && a.live(v, *m))
                        .filter_map(|((e, m), v)| {
                            // measured against the venue's own normal premium (USD vs USDT perps sit a few bp apart
                            // all day), and only when that move is far outside its usual noise
                            let now_bp = (v.mid()? * a.usdt(*e, *m) / index? - 1.0) * 1e4;
                            let hist: Vec<f64> = a.premium_bps(*e, *m).iter().rev().take(60).map(|x| x.1).collect();
                            let (med, sd) = depth::quant::robust(&hist)?;
                            ((now_bp - med).abs() >= 5.0 * sd.max(1.0)).then(|| (format!("{e:?}"), now_bp - med))
                        }).collect();
                    let input = depth::quant::Input {
                        perp: depth::quant::complete(&perp, now), spot: depth::quant::complete(&spot, now),
                        trades: &trades, dislocation: &dislocation, base: &base_s,
                    };
                    depth::quant::detect(&input, now)
                };
                let mut f = feed.lock().unwrap();
                for sg in found { f.offer(sg, 600_000); }
                // already deduplicated per algo above
                for sg in algos { f.offer(sg, 0); }
            }
        });

        let tickers = self.tickers.clone();
        let funding = self.funding.clone();
        // the last list from disk first (the picker is usable at once), then refreshed every minute
        let cache = depth::sys::cache_dir().map(|d| d.join("tickers.json"));
        if let Some(t) = cache.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Vec<Ticker>>(&b).ok()) {
            let mut g = self.tickers.lock().unwrap();
            if g.is_empty() { *g = t; }
        }
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let (t, f) = all_tickers().await;
                if !t.is_empty() {
                    if let (Some(p), Ok(j)) = (&cache, serde_json::to_vec(&t)) { let _ = std::fs::write(p, j); }
                    *tickers.lock().unwrap() = t;
                }
                if !f.is_empty() { funding.lock().unwrap().extend(f); }
            }
        });

        // market-wide daily Fear & Greed, the absolute anchor next to the intraday panic index
        let fng = self.fng.clone();
        rt.spawn(depth::ws::poll("fear & greed".into(), Duration::from_secs(3600), move || {
            let fng = fng.clone();
            async move {
                let v = depth::ws::get_json("https://api.alternative.me/fng/?limit=1").await?;
                let d = &v["data"][0];
                if let Some(x) = d["value"].as_str().and_then(|x| x.parse().ok()) {
                    *fng.lock().unwrap() = Some((x, d["value_classification"].as_str().unwrap_or_default().to_string()));
                }
                Ok(())
            }
        }));

        // live last / 24h change between REST refreshes: Binance futures leads `all_tickers`, so its
        // all-market mini ticker (changed symbols, 1/s, no weight) updates the same rows in place
        // ponytail: always on (~a few KB/s deflated); gate on the picker being open if bandwidth matters
        let tickers = self.tickers.clone();
        rt.spawn(depth::ws::run(depth::ws::Spec::new("binance tickers", "wss://fstream.binance.com/market/ws/!miniTicker@arr"), move |f| {
            let depth::ws::Frame::Text(s) = f else { return };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { return };
            let num = depth::num;
            let upd: HashMap<&str, &serde_json::Value> = v.as_array().into_iter().flatten()
                .filter_map(|x| Some((x["s"].as_str()?.strip_suffix("USDT")?, x))).collect();
            for t in tickers.lock().unwrap().iter_mut() {
                let Some(x) = upd.get(t.base.as_str()) else { continue };
                let (c, o) = (num(&x["c"]), num(&x["o"]));
                if c > 0.0 && o > 0.0 { t.last = c; t.chg_pct = (c / o - 1.0) * 100.0; t.high = num(&x["h"]); t.low = num(&x["l"]); }
            }
        }));

        let agg = self.agg.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop { tick.tick().await; agg.lock().unwrap().sample_heatmap(now_ms()); }
        });

        self.tx = Some(tx);
        self.rt = Some(rt);
    }

    /// Keeps a live price for each base in `bases` (alerts on symbols not on screen): the first
    /// enabled of Binance / Bybit / OKX spot, trades only. Spot, because perp connectors also poll
    /// open interest over REST every few seconds per symbol.
    /// ponytail: spot last price (a few bp from the perp / composite), and a perp-only coin gets no
    /// price; a venue-native mini-ticker subscription would cover both.
    pub fn watch(&mut self, bases: &[String]) {
        self.watching.retain(|b, hs| {
            let keep = bases.contains(b);
            if !keep { for h in hs.iter() { h.abort(); } }
            keep
        });
        self.watched.lock().unwrap().retain(|b, _| bases.contains(b));
        let Some(ex) = [Exchange::Binance, Exchange::Bybit, Exchange::Okx].into_iter().find(|e| !self.off.contains(e)) else { return };
        let new: Vec<String> = bases.iter().filter(|b| !self.watching.contains_key(*b)).cloned().collect();
        for b in &new {
            let rt = self.watch_rt.get_or_insert_with(|| tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("watch runtime"));
            let _g = rt.enter();
            let (tx, mut rx) = unbounded_channel::<Msg>();
            let mut hs = ex::spawn(ex, &Sub { market: Market::Spot, base: b.clone(), quote: "USDT".into() }, tx);
            let (px, base) = (self.watched.clone(), b.clone());
            hs.push(rt.spawn(async move {
                while let Some(m) = rx.recv().await {
                    if let Event::Trade { px: p, .. } = m.ev { if p > 0.0 { px.lock().unwrap().insert(base.clone(), p); } }
                }
            }));
            self.watching.insert(b.clone(), hs);
        }
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
        for (ex, k) in &keys {
            check_perms(&rt, &self.account, *ex, k.clone());
            self.streams.insert(*ex, rt.spawn(trade::stream(*ex, k.clone(), tx.clone())));
        }
        self.acct_tx = Some(tx.clone());
        let (account, funding, ctx) = (self.account.clone(), self.funding.clone(), ctx.clone());
        rt.spawn(async move {
            let mut buf = Vec::with_capacity(1024);
            let mut dirty: std::collections::HashSet<Exchange> = Default::default();
            // spot orders / balances changed (Bybit order.spot push): one REST refetch per 3s at most
            let mut spot_dirty: std::collections::HashSet<Exchange> = Default::default();
            let mut last_spot: HashMap<Exchange, std::time::Instant> = HashMap::new();
            // venues whose position pushes carry no liquidation price (Binance, Kraken): positions are
            // refetched after a push, at most every 3s, so the venue's own figure replaces the old one
            let mut pos_dirty: std::collections::HashSet<Exchange> = Default::default();
            let mut last_pos: HashMap<Exchange, std::time::Instant> = HashMap::new();
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
                                    trade::AccEvent::Resync => { a.resync_ms.insert(ex, now_ms()); resync.push(ex); }
                                    trade::AccEvent::SpotDirty => { spot_dirty.insert(ex); }
                                    trade::AccEvent::BalanceDirty => {
                                        dirty.insert(ex);
                                        // equity moved (fill, funding, transfer): cross liquidation prices with it
                                        if matches!(ex, Exchange::Binance | Exchange::Kraken) { pos_dirty.insert(ex); }
                                    }
                                    ev => {
                                        if let trade::AccEvent::Position { p, .. } = &ev { if p.qty > 0.0 && p.liq.is_none() { pos_dirty.insert(ex); } }
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
                        // funding since open: in-memory check each tick, a REST fetch only when `funding_due` says so
                        let due = { let mut a = account.lock().unwrap(); let f = funding.lock().unwrap(); funding_due(&mut a, &f, now_ms()) };
                        for (k, p, known) in due {
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move {
                                let r = trade::funding_paid(p.ex, &k, &p, known).await;
                                if let Err(e) = &r { depth::sys::append_log("trade.log", &format!("{:?} {} funding paid: {e:#}\n", p.ex, p.symbol)); }
                                let mut a = acc.lock().unwrap();
                                let Some(s) = a.funding_paid.get_mut(&(p.ex, p.symbol.clone(), p.side)) else { return };
                                match r { Ok(v) => s.val = v, Err(_) => s.due = s.due.min(now_ms() + 300_000) }
                                ctx.request_repaint();
                            });
                        }
                        let due: Vec<Exchange> = pos_dirty.iter().copied().filter(|e| last_pos.get(e).is_none_or(|t| t.elapsed() >= Duration::from_secs(3))).collect();
                        for ex in due {
                            pos_dirty.remove(&ex);
                            last_pos.insert(ex, std::time::Instant::now());
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move {
                                let Some(k) = acc.lock().unwrap().keys.get(&ex).cloned() else { return };
                                if let Ok(p) = trade::positions(ex, &k).await {
                                    let mut a = acc.lock().unwrap();
                                    a.positions.retain(|x| x.ex != ex);
                                    a.positions.extend(p);
                                    ctx.request_repaint();
                                }
                            });
                        }
                        let due: Vec<Exchange> = spot_dirty.iter().copied().filter(|e| last_spot.get(e).is_none_or(|t| t.elapsed() >= Duration::from_secs(3))).collect();
                        for ex in due {
                            spot_dirty.remove(&ex);
                            last_spot.insert(ex, std::time::Instant::now());
                            let (acc, ctx) = (account.clone(), ctx.clone());
                            tokio::spawn(async move { refresh_spot(&acc, ex).await; ctx.request_repaint(); });
                        }
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
        for e in self.venues() {
            let hs = ex::spawn(e, &self.sub(market), tx.clone());
            self.venue_tasks.entry(e).or_default().extend(hs);
        }
    }

    /// Exact L3 feeds for the current base (Lighter perp), once.
    pub fn ensure_l3(&mut self) {
        let (Some(rt), Some(tx)) = (self.rt.as_ref(), self.tx.clone()) else { return };
        let _guard = rt.enter();
        let sub = self.sub(Market::Perp);
        // Lighter exact (REST order snapshots)
        if !self.l3_started && !self.off.contains(&Exchange::Lighter) {
            self.l3_started = true;
            if let Some(s) = depth::ex::venue_sub(Exchange::Lighter, &sub) {
                let h = depth::ex::lighter::spawn_l3(&s, tx.clone());
                self.venue_tasks.entry(Exchange::Lighter).or_default().push(h);
            }
        }
        // Hyperliquid inferred: Flowscan's fast book when chosen in Settings, else the public l2Book;
        // a changed choice replaces the running feed
        if self.hl_l3.as_ref().is_some_and(|(f, _)| *f != self.l3_flowscan) { if let Some((_, h)) = self.hl_l3.take() { h.abort(); } }
        if let (true, false, Some(s)) = (self.hl_l3.is_none(), self.off.contains(&Exchange::Hyperliquid), depth::ex::venue_sub(Exchange::Hyperliquid, &sub)) {
            let h = if self.l3_flowscan { depth::ex::hyperliquid::spawn_l3_flowscan(&s, tx) } else { depth::ex::hyperliquid::spawn_l3(&s, tx) };
            self.hl_l3 = Some((self.l3_flowscan, h));
        }
    }

    /// Switch one venue off (its connections close, its data leaves the cross-venue views) or on
    /// (it connects and loads its own history); every other venue keeps its connections.
    pub fn set_venue(&mut self, e: Exchange, on: bool, ctx: &eframe::egui::Context) {
        if on == !self.off.contains(&e) { return; }
        if !on {
            self.off.insert(e);
            for h in self.venue_tasks.remove(&e).unwrap_or_default() { h.abort(); }
            self.agg.lock().unwrap().drop_venue(e);
            if e == Exchange::Lighter { self.l3_started = false; }
            if e == Exchange::Hyperliquid { if let Some((_, h)) = self.hl_l3.take() { h.abort(); } }
            return;
        }
        self.off.remove(&e);
        let (Some(rt), Some(tx)) = (self.rt.as_ref(), self.tx.clone()) else { return };
        let _guard = rt.enter();
        {
            let mut a = self.agg.lock().unwrap();
            for m in HISTORY_MARKETS { a.expect_history(e, m); }
        }
        for market in HISTORY_MARKETS {
            let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
            rt.spawn(async move {
                let h = cached_history(e, &sub, 1, &stats).await;
                let mut a = agg.lock().unwrap();
                a.load_history(e, market, &h);
                a.finish_history();
                ctx.request_repaint();
            });
        }
        for &tf in &self.tfs {
            for market in HISTORY_MARKETS {
                let (agg, stats, ctx, sub) = (self.agg.clone(), self.stats.clone(), ctx.clone(), self.sub(market));
                rt.spawn(async move {
                    let h = cached_history(e, &sub, tf, &stats).await;
                    let mut a = agg.lock().unwrap();
                    a.load_history_tf(e, market, tf, &h);
                    a.finish_history_tf(tf);
                    ctx.request_repaint();
                });
            }
        }
        for &market in &self.started {
            let hs = ex::spawn(e, &self.sub(market), tx.clone());
            self.venue_tasks.entry(e).or_default().extend(hs);
        }
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

    /// Price a protection buy on the venue's ask side (REST book + rules), into `Account::opt_plan`.
    pub fn plan_option(&self, ex: Exchange, symbol: String, qty: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        account.lock().unwrap().opt_plan = None;
        rt.spawn(async move {
            let r = trade::option_plan(ex, &symbol, qty).await.map_err(|e| format!("{e:#}"));
            account.lock().unwrap().opt_plan = Some(((ex, symbol), r));
            ctx.request_repaint();
        });
    }

    /// Send a priced protection buy (limit IOC); the result lands in the account log.
    pub fn buy_option(&self, p: trade::OptBuy, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&p.ex).cloned() else { return };
            let res = trade::buy_option(&k, &p).await;
            let what = format!("{:?} {} buy {} limit {} IOC", p.ex, p.symbol, p.qty, p.limit);
            let mut a = account.lock().unwrap();
            match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) }
            ctx.request_repaint();
        });
    }

    /// Spot buy / sell on `ex` (limit or market, base units), on the account runtime; the result goes
    /// to the account log, then spot orders and balances are refetched (Binance has no spot push here).
    #[allow(clippy::too_many_arguments)]
    pub fn spot_submit(&self, ex: Exchange, symbol: String, side: depth::Side, kind: trade::Kind, qty: f64, ref_px: f64, margin: Option<trade::spot::Effect>, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            let key = (ex, format!("spot:{symbol}"));
            let cached = account.lock().unwrap().rules.get(&key).copied();
            let rules = match cached { Some(r) => Ok(r), None => trade::spot::rules(ex, &symbol).await };
            let res = match rules {
                Ok(r) => { account.lock().unwrap().rules.insert(key, r); trade::spot::place(ex, &k, &symbol, side, kind, qty, &r, ref_px, margin).await }
                Err(e) => Err(e),
            };
            let what = format!("{ex:?} {} {} {symbol} {qty} {}", if margin.is_some() { "margin" } else { "spot" }, if side == depth::Side::Buy { "buy" } else { "sell" }, kind.label());
            { let mut a = account.lock().unwrap(); match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) } }
            ctx.request_repaint();
            // a market order has filled by now; a limit shows as resting
            for wait in [500, 2_500] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                refresh_spot(&account, ex).await;
                ctx.request_repaint();
            }
        });
    }

    /// Fetch spot orders and balances for `ex` now (the spot panel's first look at a venue).
    pub fn spot_refresh(&self, ex: Exchange, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move { refresh_spot(&account, ex).await; ctx.request_repaint(); });
    }

    /// Cancel a spot order, or a Binance cross-margin order when `margin`.
    pub fn spot_cancel(&self, o: &trade::OpenOrder, margin: bool, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            let res = trade::spot::cancel(o.ex, &k, &o.symbol, &o.id, margin).await;
            { let mut a = account.lock().unwrap(); match res { Ok(()) => a.note(format!("OK {:?} spot cancel {} {}", o.ex, o.symbol, o.id), true), Err(e) => a.note(format!("FAIL {:?} spot cancel {} {}: {e:#}", o.ex, o.symbol, o.id), false) } }
            refresh_spot(&account, o.ex).await;
            ctx.request_repaint();
        });
    }

    /// One option limit order (rules fetched once per symbol); option orders and positions are
    /// refetched after it.
    #[allow(clippy::too_many_arguments)]
    pub fn opt_submit(&self, ex: Exchange, symbol: String, side: depth::Side, qty: f64, price: f64, tif: trade::Tif, reduce_only: bool, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
            let key = (ex, format!("opt:{symbol}"));
            let cached = account.lock().unwrap().rules.get(&key).copied();
            let rules = match cached { Some(r) => Ok(r), None => trade::option_rules(ex, &symbol).await };
            let res = match rules {
                Ok(r) => { account.lock().unwrap().rules.insert(key, r); trade::option_order(&k, ex, &symbol, side, qty, price, tif, reduce_only, &r).await }
                Err(e) => Err(e),
            };
            let what = format!("{ex:?} option {} {symbol} {qty} @ {price} {tif:?}{}", if side == depth::Side::Buy { "buy" } else { "sell" }, if reduce_only { " reduce-only" } else { "" });
            { let mut a = account.lock().unwrap(); match res { Ok(id) => a.note(format!("OK {what} id {id}"), true), Err(e) => a.note(format!("FAIL {what}: {e:#}"), false) } }
            ctx.request_repaint();
            for wait in [500, 2_500] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                refresh_opts(&account, ex).await;
                ctx.request_repaint();
            }
        });
    }

    pub fn opt_cancel(&self, o: &trade::OpenOrder, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            let res = trade::option_cancel(o.ex, &k, &o.symbol, &o.id).await;
            { let mut a = account.lock().unwrap(); match res { Ok(()) => a.note(format!("OK {:?} option cancel {} {}", o.ex, o.symbol, o.id), true), Err(e) => a.note(format!("FAIL {:?} option cancel {} {}: {e:#}", o.ex, o.symbol, o.id), false) } }
            refresh_opts(&account, o.ex).await;
            ctx.request_repaint();
        });
    }

    /// Fetch option orders and positions for `ex` now (the option ticket's first look at a venue).
    pub fn opt_refresh(&self, ex: Exchange, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx) = (self.acct_rt.as_ref(), self.account.clone(), ctx.clone()) else { return };
        rt.spawn(async move { refresh_opts(&account, ex).await; ctx.request_repaint(); });
    }

    /// Scaled (ladder) order: `n` limit orders from `from` to `to`, sizes tilted by `skew`.
    pub fn submit_scaled(&self, ex: Exchange, base: OrderReq, from: f64, to: f64, n: usize, skew: f64, post_only: bool, ctx: &eframe::egui::Context) -> Result<usize, String> {
        let rules = self.account.lock().unwrap().rules.get(&(ex, base.symbol.clone())).copied().ok_or("venue rules not loaded yet: try again in a second")?;
        let legs = depth::algo::scaled(base.qty, from, to, n, skew, rules.tick, rules.step);
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
        let sizes = depth::algo::twap_slices(base.qty, slices, rules.step);
        if sizes.is_empty() || sizes.iter().any(|q| *q < rules.min_qty) { return Err(format!("each slice must be at least {} (try fewer slices)", rules.min_qty)); }
        let (Some(rt), account, agg, ctx) = (self.acct_rt.as_ref(), self.account.clone(), self.agg.clone(), ctx.clone()) else { return Err("not connected".into()) };
        let id = depth::now_ms() as u64;
        let gap = Duration::from_secs_f64(minutes * 60.0 / sizes.len() as f64);
        let buy = base.side() == depth::Side::Buy;
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy, close: base.close, total: base.qty, sent: 0.0, slices: sizes.len(), done: 0,
            started_ms: depth::now_ms(), end_ms: depth::now_ms() + (minutes * 60_000.0) as i64, status: "running".into(), cancelled: false, venue_id: None,
        });
        rt.spawn(async move {
            let mut i = 0;
            while i < sizes.len() {
                if account.lock().unwrap().algos.iter().any(|j| j.id == id && j.cancelled) { break; }
                // the venue's own book; stale (pair switched away, venue down) means wait
                let bbo = {
                    let a = agg.lock().unwrap();
                    a.venues.get(&(ex, depth::Market::Perp)).filter(|v| depth::now_ms() - v.ts < 5_000)
                        .and_then(|v| match (v.book.best_bid(), v.book.best_ask()) { (Some(b), Some(k)) => Some((b.0, k.0)), _ => v.bbo.map(|[b, _, k, _]| (b, k)) })
                };
                let px = bbo.and_then(|(b, k)| depth::algo::twap_price(buy, b, k, max_slip_bps, limit));
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
        let id = depth::now_ms() as u64;
        let now = depth::now_ms();
        account.lock().unwrap().algos.push(AlgoJob {
            id, ex, symbol: base.symbol.clone(), buy: base.side() == depth::Side::Buy, close: base.close, total: base.qty, sent: 0.0, slices: 0, done: 0,
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
            let what = format!("{ex:?} {} TWAP {} {} over {minutes} min", base.symbol, if base.side() == depth::Side::Buy { "buy" } else { "sell" }, base.qty);
            match res {
                Ok(vid) => {
                    a.note(format!("OK {what} id {vid}"), true);
                    if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.venue_id = Some(vid); j.status = format!("running on {ex:?}"); }
                }
                Err(e) => {
                    a.note(format!("FAIL {what}: {e:#}"), false);
                    if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = format!("failed: {e:#}"); j.cancelled = true; j.end_ms = depth::now_ms(); }
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
                    Ok(()) => { a.note(format!("OK {ex:?} {symbol} TWAP {vid} cancelled"), true); if let Some(j) = a.algos.iter_mut().find(|j| j.id == id) { j.status = "cancelled".into(); j.end_ms = depth::now_ms(); } }
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

    /// Move a resting limit order (dragged on the chart): amended in place where the venue allows it
    /// (Bybit, Binance: same id, nothing in between); elsewhere cancelled, then the rest of it placed
    /// at `price` only once the cancel succeeded, so a failed cancel never leaves two orders.
    /// ponytail: OKX / Bitget / Gate / Kraken / Hyperliquid have amend endpoints too; wire them once
    /// those venues are verified on a real account.
    pub fn replace_order(&self, o: &OpenOrder, price: f64, ctx: &eframe::egui::Context) {
        let (Some(rt), account, ctx, o) = (self.rt.as_ref(), self.account.clone(), ctx.clone(), o.clone()) else { return };
        let opp = |s: depth::Side| if s == depth::Side::Buy { depth::Side::Sell } else { depth::Side::Buy };
        let req = OrderReq {
            symbol: o.symbol.clone(), close: o.reduce_only, kind: trade::Kind::Limit { price, tif: trade::Tif::Gtc }, qty: o.qty - o.filled, client_id: None,
            // the position the order acts on: its own side when opening, the other one when reducing
            pos: o.pos.unwrap_or(if o.reduce_only { opp(o.side) } else { o.side }),
        };
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&o.ex).cloned() else { return };
            if trade::can_amend(o.ex) {
                let cached = account.lock().unwrap().rules.get(&(o.ex, o.symbol.clone())).copied();
                let res = async {
                    let r = match cached { Some(r) => r, None => trade::rules(o.ex, &o.symbol).await? };
                    trade::amend(o.ex, &k, &o, price, &r).await
                }.await;
                let what = format!("{:?} {} {} {} -> {price}", o.ex, o.symbol, o.id, o.price);
                let ok = res.is_ok();
                account.lock().unwrap().note(match res { Ok(()) => format!("OK amend {what}"), Err(e) => format!("FAIL amend {what}: {e:#}") }, ok);
                ctx.request_repaint();
                return;
            }
            match trade::cancel(o.ex, &k, &o.symbol, &o.id).await {
                Ok(()) => { place_logged(&account, o.ex, &req, price).await; }
                Err(e) => account.lock().unwrap().note(format!("FAIL move {:?} {} {}: cancel: {e:#}", o.ex, o.symbol, o.id), false),
            }
            ctx.request_repaint();
        });
    }

    /// Move a TP / SL trigger (dragged on the chart): a whole-position one is replaced in place, a
    /// partial one is re-added at the new price after its cancel succeeded.
    pub fn move_tpsl(&self, t: &trade::tpsl::TpSl, price: f64, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let (account, ctx, t) = (self.account.clone(), ctx.clone(), t.clone());
        rt.spawn(async move {
            let Some(k) = account.lock().unwrap().keys.get(&t.ex).cloned() else { return };
            let cached = account.lock().unwrap().modes.get(&(t.ex, t.symbol.clone())).copied().flatten();
            let res = async {
                let m = match cached { Some(m) => m, None => trade::mode(t.ex, &k, &t.symbol).await? };
                let (tp, sl) = if t.take_profit { (Some(price), None) } else { (None, Some(price)) };
                match t.qty {
                    None => trade::tpsl::set_position_tpsl(t.ex, &k, &t.symbol, t.pos, m, tp, sl, t.trigger).await,
                    Some(q) => {
                        trade::tpsl::cancel(t.ex, &k, &t.symbol, &t.id).await?;
                        trade::tpsl::add_partial(t.ex, &k, &t.symbol, t.pos, m, t.take_profit, price, q, t.trigger).await.map(|_| ())
                    }
                }
            }.await;
            let what = format!("{:?} {} {} -> {price}", t.ex, t.symbol, if t.take_profit { "TP" } else { "SL" });
            let mut a = account.lock().unwrap();
            match res { Ok(()) => a.note(format!("OK move {what}"), true), Err(e) => a.note(format!("FAIL move {what}: {e:#}"), false) }
            ctx.request_repaint();
        });
    }

    /// Set / replace whole-position TP and/or SL (None leaves a side as is), or add one partial
    /// level (`partial = Some(qty)`); the result lands in the order log, the stream updates state.
    #[allow(clippy::too_many_arguments)]
    pub fn set_tpsl(&self, ex: Exchange, symbol: String, pos: depth::Side, tp: Option<f64>, sl: Option<f64>, partial: Option<f64>, trigger: trade::tpsl::Trigger, ctx: &eframe::egui::Context) {
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
            let what = format!("{ex:?} {symbol} {} TP {} SL {}{}", if pos == depth::Side::Buy { "long" } else { "short" },
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
        self.account.lock().unwrap().can_trade.remove(&ex);
        if let (Some(k), Some(rt), Some(tx)) = (keys, self.acct_rt.as_ref(), self.acct_tx.clone()) {
            check_perms(rt, &self.account, ex, k.clone());
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
                    Ok(h) => { a.history.insert(ex, (h, now_ms())); a.fills_ver += 1; }
                    Err(e) => a.note(format!("{ex:?} history: {e:#}"), false),
                }
                a.history_loading.remove(&ex);
                ctx.request_repaint();
            });
        }
    }

    /// Own fills of `symbol` missed while the app was closed: once per venue and symbol per
    /// session, from the newest stored fill (pushes keep it current afterwards).
    pub fn backfill_fills(&self, symbol: &str, ctx: &eframe::egui::Context) {
        let Some(rt) = self.acct_rt.as_ref() else { return };
        let todo: Vec<(Exchange, Keys)> = {
            let mut a = self.account.lock().unwrap();
            let ks: Vec<_> = a.keys.iter().filter(|(e, _)| matches!(e, Exchange::Bybit | Exchange::Binance)).map(|(e, k)| (*e, k.clone())).collect();
            ks.into_iter().filter(|(e, _)| a.fills_done.insert((*e, symbol.to_string()))).collect()
        };
        for (ex, k) in todo {
            let (account, ctx, symbol) = (self.account.clone(), ctx.clone(), symbol.to_string());
            rt.spawn(async move {
                let since = depth::db::fills_last(ex, &symbol).map_or(0, |t| t + 1);
                match trade::fills_since(ex, &k, &symbol, since).await {
                    Ok(f) => { depth::db::fills_put(&f); account.lock().unwrap().fills_ver += 1; ctx.request_repaint(); }
                    Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} {symbol} fills: {e:#}\n")),
                }
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

fn cache_key(ex: Exchange, sub: &Sub, tf: u32) -> String { format!("{}/{ex:?}-{:?}-{tf}m", sub.base, sub.market) }

/// History from the local cache plus only the candles since its last bar; a full fetch when
/// there is no usable cache. On fetch failure the cache alone is used. The result is written back.
async fn cached_history(ex: Exchange, sub: &Sub, tf: u32, stats: &Arc<Mutex<Stats>>) -> History {
    let key = cache_key(ex, sub, tf);
    let cached: Option<History> = depth::db::hist_get(&key).and_then(|b| serde_json::from_slice(&b).ok());
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
    if !h.klines.is_empty() { if let Ok(b) = serde_json::to_vec(&h) { depth::db::hist_put(&key, &b); } }
    h
}

/// REST snapshot of one venue's spot orders and coins (replaces that venue's rows). Errors (a key
/// without spot permission) go to the trade log only: they must not mark the venue's perp account.
async fn refresh_spot(account: &Arc<Mutex<Account>>, ex: Exchange) {
    if !trade::spot::VENUES.contains(&ex) { return; }
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (o, b) = tokio::join!(trade::spot::open_orders(ex, &k), trade::spot::balances(ex, &k));
    {
        let mut a = account.lock().unwrap();
        match o { Ok(o) => { a.spot_orders.retain(|x| x.ex != ex); a.spot_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} spot orders: {e:#}\n")) }
        match b { Ok(b) => { a.spot_bal.insert(ex, b); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} spot balances: {e:#}\n")) }
    }
    // margin: only once Margin mode has asked for a symbol (Binance margin orders are per symbol)
    let Some(sym) = account.lock().unwrap().margin_sym.clone() else { return };
    let (o, b) = tokio::join!(trade::spot::margin_orders(ex, &k, &sym), trade::spot::margin_balances(ex, &k));
    let mut a = account.lock().unwrap();
    match o { Ok(o) => { a.margin_orders.retain(|x| x.ex != ex); a.margin_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} margin orders: {e:#}\n")) }
    match b { Ok(b) => { a.margin_bal.insert(ex, b); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} margin balances: {e:#}\n")) }
}

/// REST snapshot of one venue's option orders and positions (option venues only).
async fn refresh_opts(account: &Arc<Mutex<Account>>, ex: Exchange) {
    if !trade::OPTION_VENUES.contains(&ex) { return; }
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (o, p) = tokio::join!(trade::option_orders(ex, &k), trade::option_positions(ex, &k));
    let mut a = account.lock().unwrap();
    match o { Ok(o) => { a.opt_orders.retain(|x| x.ex != ex); a.opt_orders.extend(o); } Err(e) => depth::sys::append_log("trade.log", &format!("{ex:?} option orders: {e:#}\n")) }
    if let Ok(p) = p { a.opts.retain(|x| x.ex != ex); a.opts.extend(p); }
}

/// REST snapshot of one venue's positions, open orders and balance (replaces that venue's rows).
async fn refresh_venue(account: &Arc<Mutex<Account>>, ex: Exchange) {
    refresh_spot(account, ex).await;
    let Some(k) = account.lock().unwrap().keys.get(&ex).cloned() else { return };
    let (p, o, b, op, oo) = tokio::join!(trade::positions(ex, &k), trade::open_orders(ex, &k), trade::balance(ex, &k), trade::option_positions(ex, &k), trade::option_orders(ex, &k));
    // TP/SL snapshot (Binance: 40 weight for the account; snapshots only run on resync / 5 min)
    if matches!(ex, Exchange::Bybit | Exchange::Binance) {
        if let Ok(t) = trade::tpsl::list(ex, &k).await {
            let mut a = account.lock().unwrap();
            a.tpsl.retain(|x| x.ex != ex);
            a.tpsl.extend(t);
        }
    }
    let mut a = account.lock().unwrap();
    // each part applies on its own: a failing orders or balance call must not freeze positions
    let mut err = None;
    match p { Ok(p) => { a.positions.retain(|x| x.ex != ex); a.positions.extend(p); } Err(e) => err = err.or(Some(e)) }
    match o { Ok(o) => { a.orders.retain(|x| x.ex != ex); a.orders.extend(o); } Err(e) => err = err.or(Some(e)) }
    // an account without option trading answers with an error: keep it out of the venue's status
    if let Ok(op) = op { a.opts.retain(|x| x.ex != ex); a.opts.extend(op); }
    if let Ok(oo) = oo { a.opt_orders.retain(|x| x.ex != ex); a.opt_orders.extend(oo); }
    match b { Ok(b) => { a.balances.insert(ex, b); } Err(e) => err = err.or(Some(e)) }
    match err {
        None => { a.errors.remove(&ex); }
        Some(e) => {
            let e = format!("{e:#}");
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
            // venues without a fill push: a size change is a fill (long = +, short = -)
            let signed = |x: &Position| if x.side == depth::Side::Buy { x.qty } else { -x.qty };
            let d = signed(&p) - old.as_ref().map_or(0.0, signed);
            if !matches!(ex, Exchange::Binance | Exchange::Bybit) && d.abs() > 1e-12 && a.resync_ms.get(&ex).is_some_and(|t| now_ms() - t > 5_000) {
                a.fill_seq += 1;
                a.last_fill = Some(FillNote { ex, symbol: p.symbol.clone(), buy: d > 0.0, qty: d.abs(), price: None });
            }
            if p.qty > 0.0 {
                // fields a push leaves out keep their last known value, except the liquidation price
                // and margin of a position whose size or entry changed: those belonged to the old
                // position (the venue's figure is refetched, see pos_dirty)
                if let Some(o) = old {
                    let same = o.qty == p.qty && o.entry == p.entry;
                    if p.mark == 0.0 { p.mark = o.mark; }
                    if p.liq.is_none() && same { p.liq = o.liq; }
                    if p.lev == 0.0 { p.lev = o.lev; }
                    if p.margin == 0.0 && same { p.margin = o.margin; }
                }
                if p.mark == 0.0 { p.mark = p.entry; }
                if p.margin == 0.0 && p.lev > 0.0 { p.margin = p.mark * p.qty / p.lev; }
                if p.lev > 0.0 { a.levs.insert((ex, p.symbol.clone()), p.lev); }
                a.positions.push(p);
            }
            true
        }
        E::Mark { symbol, mark } => {
            let mut changed = false;
            // equity change from this mark, applied below to the other cross positions' liquidation prices
            let mut moved = vec![];
            for (i, p) in a.positions.iter_mut().enumerate().filter(|(_, p)| p.ex == ex && p.symbol == symbol) {
                let sign = if p.side == depth::Side::Buy { 1.0 } else { -1.0 };
                let upnl = (mark - p.entry) * p.qty * sign;
                if p.cross == Some(true) { moved.push((i, upnl - p.upnl)); }
                p.mark = mark;
                p.upnl = upnl;
                // cross margin is notional at mark over leverage (what the venues show); isolated stays put
                if p.cross == Some(true) && p.lev > 0.0 { p.margin = mark * p.qty / p.lev; }
                changed = true;
            }
            // Venues' cross liquidation price of a position solves equity(P) = maintenance at fixed
            // other prices, so between the venue's own figures (refetched after each account push)
            // another position's PnL change d moves it by d / qty: down for a long, up for a short.
            // ponytail: ignores the maintenance-rate factor 1 / (1 - mmr) (~0.5%) and collateral
            // haircut moves; the next refetch replaces it with the venue's number.
            for (i, d) in moved {
                for (j, o) in a.positions.iter_mut().enumerate() {
                    if j == i || o.ex != ex || o.cross != Some(true) || o.qty <= 0.0 { continue }
                    let sign = if o.side == depth::Side::Buy { 1.0 } else { -1.0 };
                    // at or below zero the venue shows none
                    o.liq = o.liq.map(|l| l - d * sign / o.qty).filter(|l| *l > 0.0);
                }
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
        E::Fill(f) => {
            depth::db::fills_put(std::slice::from_ref(&f));
            a.fills_ver += 1;
            a.fill_seq += 1;
            a.last_fill = Some(FillNote { ex, symbol: f.symbol, buy: f.side == depth::Side::Buy, qty: f.qty, price: Some(f.price) });
            true
        }
        E::OptPos(p) => {
            a.opts.retain(|x| !(x.ex == ex && x.symbol == p.symbol));
            if p.qty > 0.0 { a.opts.push(p); }
            true
        }
        E::Resync | E::BalanceDirty | E::SpotDirty => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth::Side;
    use trade::AccEvent as E;

    fn pos(side: Side, qty: f64) -> Position {
        Position { ex: Exchange::Binance, symbol: "BTCUSDT".into(), side, qty, entry: 100.0, mark: 0.0, liq: None, upnl: 0.0, lev: 0.0, margin: 0.0, cross: None }
    }

    #[test]
    fn funding_fetch_only_when_due() {
        let mut a = Account::default();
        a.keys.insert(Exchange::Binance, Keys { key: "k".into(), secret: "s".into(), extra: None });
        a.positions.push(pos(Side::Buy, 1.0));
        let fund = HashMap::from([((Exchange::Binance, "BTCUSDT".to_string()), VenueFunding { rate: 0.0, interval_h: 8.0, next_ms: 1_000_000 })]);
        assert_eq!(funding_due(&mut a, &fund, 0).len(), 1);
        assert!(funding_due(&mut a, &fund, 1_000).is_empty());
        // size change: debounced 60 s from the last fetch
        a.positions[0].qty = 2.0;
        assert!(funding_due(&mut a, &fund, 30_000).is_empty());
        assert_eq!(funding_due(&mut a, &fund, 60_000).len(), 1);
        // 20 s after the settlement, then the next one (stale listed time rolled forward 8h)
        assert!(funding_due(&mut a, &fund, 1_019_000).is_empty());
        assert_eq!(funding_due(&mut a, &fund, 1_020_000).len(), 1);
        assert_eq!(a.funding_paid.values().next().unwrap().due, 1_000_000 + 8 * 3_600_000 + 20_000);
        // closed: slot dropped
        a.positions.clear();
        assert!(funding_due(&mut a, &fund, 1_030_000).is_empty() && a.funding_paid.is_empty());
    }

    #[test]
    fn cross_liq_follows_other_positions_pnl() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        let p = |sym: &str, side, qty, entry, liq| Position { ex, symbol: sym.into(), side, qty, entry, mark: entry, liq, upnl: 0.0, lev: 10.0, margin: 0.0, cross: Some(true) };
        a.positions.push(p("SOLUSDT", Side::Buy, 10.0, 100.0, Some(50.0)));
        a.positions.push(p("ETHUSDT", Side::Sell, 2.0, 3000.0, Some(4000.0)));
        // SOL +1 => +10 equity: the short ETH liq rises by 10 / 2, SOL's own liq is unchanged
        apply(&mut a, ex, E::Mark { symbol: "SOLUSDT".into(), mark: 101.0 });
        assert_eq!(a.positions[0].liq, Some(50.0));
        assert_eq!(a.positions[1].liq, Some(4005.0));
        // cross margin follows mark: 101 * 10 / 10
        assert_eq!(a.positions[0].margin, 101.0);
        // ETH +10 => short loses 20: SOL's long liq rises by 20 / 10
        apply(&mut a, ex, E::Mark { symbol: "ETHUSDT".into(), mark: 3010.0 });
        assert_eq!(a.positions[0].liq, Some(52.0));
    }

    #[test]
    fn pushes_merge_into_account_state() {
        let mut a = Account::default();
        let ex = Exchange::Binance;
        // REST snapshot row with fields a push does not carry
        a.positions.push(Position { liq: Some(80.0), lev: 10.0, margin: 50.0, mark: 101.0, ..pos(Side::Buy, 1.0) });
        // a push with the same size keeps liq/margin; a resized one drops them (they belonged to the
        // old position): liq until the refetch, margin recomputed at mark; short added alongside
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 1.0), one_way: false });
        assert_eq!((a.positions[0].liq, a.positions[0].margin), (Some(80.0), 50.0));
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 2.0), one_way: false });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 1.0), one_way: false });
        assert_eq!(a.positions.len(), 2);
        let long = a.positions.iter().find(|p| p.side == Side::Buy).unwrap();
        assert_eq!((long.qty, long.liq, long.lev, long.mark, long.margin), (2.0, None, 10.0, 101.0, 20.2));
        // mark drives upnl on both sides
        apply(&mut a, ex, E::Mark { symbol: "BTCUSDT".into(), mark: 110.0 });
        let upnl: Vec<f64> = a.positions.iter().map(|p| p.upnl).collect();
        assert!(upnl.contains(&20.0) && upnl.contains(&-10.0), "{upnl:?}");
        // a one-way zero row clears the symbol on either side
        apply(&mut a, ex, E::Position { p: pos(Side::Buy, 0.0), one_way: true });
        apply(&mut a, ex, E::Position { p: pos(Side::Sell, 0.0), one_way: true });
        assert!(a.positions.is_empty());
        // orders upsert by id and leave on done
        let o = OpenOrder { ex, symbol: "BTCUSDT".into(), id: "7".into(), side: Side::Buy, price: 99.0, qty: 1.0, filled: 0.0, kind: "LIMIT".into(), tif: String::new(), reduce_only: false, ts: 0, pos: None };
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
    use depth::{num, ws};
    let ok = |b: &str| !b.is_empty() && !b.contains('_') && !b.contains('-') && b.is_ascii();
    // a venue's own name back to the unified (Binance) one (Bybit AMDSTOCK -> AMD); None when its
    // same-name market is another asset (crate alias table), so it does not merge into that row
    let unify = |ex: Exchange, b: &str| { let u = depth::alias::unified_base(ex, b); depth::alias::venue_base(ex, Market::Perp, &u).is_some().then_some(u) };
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
            if let Some(b) = x["symbol"].as_str().and_then(|s| s.strip_suffix("USDT")).and_then(|b| unify(Exchange::Bybit, b)) { push(&b, num(&x["lastPrice"]), num(&x["price24hPcnt"]) * 100.0, num(&x["turnover24h"]), num(&x["highPrice24h"]), num(&x["lowPrice24h"]), num(&x["volume24h"])); }
        },
        Err(e) => eprintln!("[tickers bybit] {e:#}"),
    }
    match ox {
        Ok(v) => for x in v["data"].as_array().into_iter().flatten() {
            if let Some(b) = x["instId"].as_str().and_then(|s| s.strip_suffix("-USDT-SWAP")).and_then(|b| unify(Exchange::Okx, b)) {
                let (l, o) = (num(&x["last"]), num(&x["open24h"]));
                // volCcy24h is in base units for USDT swaps
                push(&b, l, if o > 0.0 { (l / o - 1.0) * 100.0 } else { 0.0 }, num(&x["volCcy24h"]) * l, num(&x["high24h"]), num(&x["low24h"]), num(&x["volCcy24h"]));
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
                if let Some(b) = u["name"].as_str().and_then(|b| unify(Exchange::Hyperliquid, b)) {
                    fund.insert((Exchange::Hyperliquid, format!("{b}USDT")), VenueFunding { rate: num(&c["funding"]), interval_h: 1.0, next_ms: (depth::now_ms() / 3_600_000 + 1) * 3_600_000 });
                    push(&b, l, if p > 0.0 { (l / p - 1.0) * 100.0 } else { 0.0 }, num(&c["dayNtlVlm"]), 0.0, 0.0, num(&c["dayBaseVlm"]));
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
