//! egui terminal: header (asset, market mode, cross-venue stats, venue scope), chart or option
//! view, order book / tape, venue table and status bar.
mod book;
mod chart;
mod engine;
mod options;
mod picker;
pub mod theme;
mod trade;
mod wallet;
mod settings;
mod native;

use eframe::egui::{self, Color32, Frame, Margin, RichText, Ui};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use terminal_one::agg::Agg;
use terminal_one::{now_ms, Exchange, Market};
use theme::*;

const MODES: [(Market, &str); 4] = [(Market::Spot, "mode.spot"), (Market::Margin, "mode.margin"), (Market::Perp, "mode.perp"), (Market::Option, "mode.option")];

/// UI language: English unless switched to Chinese (status bar, persisted in settings).
static ZH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn set_zh(on: bool) { ZH.store(on, std::sync::atomic::Ordering::Relaxed); }
pub fn is_zh() -> bool { ZH.load(std::sync::atomic::Ordering::Relaxed) }

fn strings(src: &'static str) -> HashMap<&'static str, &'static str> {
    src.lines().filter(|l| !l.trim_start().starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).collect()
}

/// UI string for `key` from assets/i18n/{en,zh-CN}.txt (the other language, then the key, if missing).
pub fn t(key: &'static str) -> &'static str {
    static EN: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    static ZHM: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let en = EN.get_or_init(|| strings(include_str!("../../assets/i18n/en.txt")));
    let zh = ZHM.get_or_init(|| strings(include_str!("../../assets/i18n/zh-CN.txt")));
    let (a, b) = if is_zh() { (zh, en) } else { (en, zh) };
    a.get(key).or_else(|| b.get(key)).copied().unwrap_or(key)
}

/// Local "M/D" for a timestamp (history tables).
pub fn chart_date(ms: i64) -> String { chart::local_date(ms) }

pub fn fmt_px(v: f64) -> String {
    let a = v.abs();
    fmt_dp(v, if a >= 10_000.0 { 1 } else if a >= 100.0 { 2 } else if a >= 1.0 { 4 } else { 6 })
}

/// Decimals needed to tell `step`-spaced prices apart (book levels, grouping).
pub fn step_dp(step: f64) -> usize {
    (0..10).find(|d| { let x = step * 10f64.powi(*d as i32); (x - x.round()).abs() < 1e-6 * x.max(1.0) }).unwrap_or(10)
}

/// `d` decimals with thousands separators.
pub fn fmt_dp(v: f64, d: usize) -> String {
    let s = format!("{v:.d$}");
    // thousands separators on the integer part
    let (int, frac) = s.split_once('.').map(|(i, f)| (i, Some(f))).unwrap_or((&s, None));
    let (sign, digits) = int.strip_prefix('-').map(|d| ("-", d)).unwrap_or(("", int));
    let mut g = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { g.push(','); }
        g.push(ch);
    }
    match frac { Some(f) => format!("{sign}{g}.{f}"), None => format!("{sign}{g}") }
}

pub fn fmt_qty(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e4 { format!("{:.1}K", v / 1e3) } else if a >= 100.0 { format!("{v:.1}") } else { format!("{v:.3}") }
}

/// Local UTC offset in ms, read once from `date +%z` (std has no timezone API).
pub fn local_offset_ms() -> i64 {
    static O: OnceLock<i64> = OnceLock::new();
    *O.get_or_init(|| {
        let s = std::process::Command::new("date").arg("+%z").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let n: i64 = s.get(1..).and_then(|x| x.parse().ok()).unwrap_or(0);
        let ms = (n / 100 * 60 + n % 100) * 60_000;
        if s.starts_with('-') { -ms } else { ms }
    })
}

pub fn hms(ms: i64) -> String {
    let s = (ms + local_offset_ms()).rem_euclid(86_400_000) / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// Persisted UI state: ~/Library/Application Support/TerminalOne/settings.json.
/// Written when it changes (checked once a second); env overrides (T1_BASE, ...) win on load.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Settings { route: Option<terminal_one::route::Policy>, prefs: Option<settings::Prefs>, lang: Option<String>, base: Option<String>, mode: Option<Market>, chart: Option<chart::Chart>, trade_ex: Option<Exchange>, off: Option<Vec<Exchange>>, auto: Option<HashMap<Exchange, engine::AutoTopUp>> }

#[derive(serde::Serialize)]
struct SettingsRef<'a> { route: &'a terminal_one::route::Policy, prefs: &'a settings::Prefs, lang: &'static str, base: &'a str, mode: Market, chart: &'a chart::Chart, trade_ex: Exchange, off: Vec<Exchange>, auto: std::collections::BTreeMap<String, engine::AutoTopUp> }

fn settings_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/TerminalOne/settings.json"))
}

pub struct App {
    eng: engine::Engine,
    prefs: settings::Prefs,
    settings: settings::SettingsView,
    /// last settings JSON written, and when it was last checked
    saved: (String, Instant),
    mode: Market,
    chart: chart::Chart,
    book: book::BookView,
    opt: options::OptView,
    picker: picker::Picker,
    trade: trade::TradeView,
    rate: (Instant, u64, f64),
    /// screenshot request from env: (path, after seconds)
    shot: Option<(String, f64)>,
    shot_sent: bool,
    started: Instant,
    last_frame: Instant,
    /// hosted inside the SwiftUI app: the host owns the window (no own title bar buttons,
    /// drag, resize edges) and paces frames with its display link (no frame-cap sleep)
    hosted: bool,
    /// order routing policy (Settings > Trading), persisted
    route: terminal_one::route::Policy,
    /// actions from the native panels, applied at the next frame
    pending_base: Option<String>,
    pending_mode: Option<Market>,
    pending_restart: bool,
    /// order book column width (points), so the native header above it lines up
    book_w: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self { Self::with_ctx(&cc.egui_ctx, false) }

    /// Build the app on any egui context (eframe window, or the SwiftUI host's Metal view).
    pub fn with_ctx(ctx: &egui::Context, hosted: bool) -> Self {
        let cc = Cc { egui_ctx: ctx.clone() };
        theme::set_native(hosted);
        theme::install(&cc.egui_ctx);
        #[cfg(feature = "hotpatch")]
        {
            let c = cc.egui_ctx.clone();
            subsecond::register_handler(std::sync::Arc::new(move || c.request_repaint()));
        }
        let env = |k: &str| std::env::var(k).ok();
        let saved = settings_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let st: Settings = serde_json::from_str(&saved).unwrap_or_default();
        set_zh(env("T1_LANG").or(st.lang.clone()).is_some_and(|l| l.starts_with("zh")));
        let prefs = st.prefs.clone().unwrap_or_default();
        prefs.apply(&cc.egui_ctx);
        let mode = env("T1_MODE").and_then(|m| Market::parse(&m)).or(st.mode).unwrap_or(Market::Perp);
        let mut eng = engine::Engine::new();
        eng.account.lock().unwrap().auto = st.auto.clone().unwrap_or_default();
        eng.start_account(&cc.egui_ctx);
        eng.off = st.off.unwrap_or_default().into_iter().collect();
        if eng.off.len() >= Exchange::ALL.len() { eng.off.clear(); }
        eng.start(&env("T1_BASE").or(st.base).unwrap_or("BTC".into()), &cc.egui_ctx);
        if matches!(mode, Market::Margin | Market::Option) { eng.ensure(mode); }
        let mut chart = st.chart.unwrap_or_default();
        let mut trade = trade::TradeView::default();
        if let Some(ex) = st.trade_ex.filter(|e| terminal_one::trade::TRADABLE.contains(e)) { trade.ex = ex; }
        // T1_ACC_TAB=<n>: open the account strip on tab n (screenshots)
        if let Some(n) = env("T1_ACC_TAB").and_then(|s| s.parse().ok()) { trade.set_tab(n); }
        let mut picker = picker::Picker::default();
        picker.open = env("T1_PICK").is_some();
        if let Some(tf) = env("T1_TF").and_then(|s| s.parse().ok()) { chart.tf = tf; }
        App {
            eng, prefs, settings: Default::default(), saved: (saved, Instant::now()), mode, chart, book: Default::default(), opt: Default::default(), picker, trade,
            rate: (Instant::now(), 0, 0.0),
            shot: env("T1_SHOT").map(|p| (p, env("T1_SHOT_AFTER").and_then(|s| s.parse().ok()).unwrap_or(30.0))),
            shot_sent: false, started: Instant::now(), last_frame: Instant::now(), hosted,
            route: st.route.clone().unwrap_or_default(), pending_base: None, pending_mode: None, pending_restart: false, book_w: 0.0,
        }
    }

    fn header(&mut self, ui: &mut Ui, a: &Agg, new_base: &mut Option<String>, new_mode: &mut Option<Market>) {
        // the header is the title bar: drag empty space to move, double-click to maximize
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        // own window only: the SwiftUI host's title bar handles dragging and zooming
        if !self.hosted {
            let bar = ui.interact(ui.max_rect(), egui::Id::new("titlebar"), egui::Sense::click_and_drag());
            if bar.drag_started_by(egui::PointerButton::Primary) { ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag); }
            if bar.double_clicked() { ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)); }
        }
        ui.horizontal(|ui| {
            // hosted: macOS draws the traffic lights over this space
            if self.hosted { ui.add_space(70.0); } else { window_buttons(ui, maximized); }
            ui.add_space(10.0);
            let tickers = self.eng.tickers.lock().unwrap().clone();
            if let Some(b) = self.picker.show(ui, &self.eng.base, &tickers) { if b != self.eng.base { *new_base = Some(b); } }
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.x = 0.0;
            for (m, k) in MODES {
                if tab(ui, t(k), self.mode == m, 13.0).clicked() && self.mode != m { *new_mode = Some(m); }
            }
            ui.spacing_mut().item_spacing.x = 18.0;
            ui.add_space(18.0);
            let book_market = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
            let last = a.mid(book_market);
            ui.label(RichText::new(last.map(fmt_px).unwrap_or("-".into())).font(mono(20.0)).color(FG));
            let stat = |ui: &mut Ui, k: &'static str, v: String, col: Color32| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(t(k)).font(prop(10.5)).color(DIM));
                    ui.label(RichText::new(v).font(mono(12.0)).color(col));
                }).response
            };
            let sc = |v: Option<f64>| match v { Some(x) if x > 0.0 => up(), Some(x) if x < 0.0 => dn(), _ => FG };
            if self.mode == Market::Perp {
                let (oi, oi_usd) = a.oi_total();
                stat(ui, "hdr.oi", format!("{} ({:.2}B$)", fmt_qty(oi), oi_usd / 1e9), FG);
                let (fp, fs) = (a.funding_oi_weighted(), a.funding_settled_oi_weighted());
                stat(ui, "hdr.funding_pred", fp.map(|f| format!("{:+.4}bp/h", f * 1e4)).unwrap_or("-".into()), sc(fp))
                    .on_hover_text(fp.map(|f| format!("{} {:+.2}%", t("hdr.apr"), f * 24.0 * 365.0 * 100.0)).unwrap_or_default());
                stat(ui, "hdr.funding_settled", fs.map(|f| format!("{:+.4}bp/h", f * 1e4)).unwrap_or("-".into()), sc(fs));
                let b = {
                    let mut v: Vec<f64> = a.venues.keys().filter(|(_, m)| *m == Market::Perp).filter_map(|(e, _)| a.basis_bps(*e)).collect();
                    v.sort_by(f64::total_cmp);
                    v.get(v.len() / 2).copied()
                };
                stat(ui, "hdr.basis", b.map(|x| format!("{x:+.2}bp")).unwrap_or("-".into()), sc(b));
            }
            if self.mode != Market::Option {
                let c = a.cvd_total(book_market);
                stat(ui, "hdr.cvd", format!("{c:+.1}"), sc(Some(c)));
            }
            // settings gear at the far right of the title bar
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(6.0);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
                let p = ui.painter();
                if resp.hovered() { p.rect_filled(r, 5, HL); }
                let col = if resp.hovered() { FG } else { MU };
                let c = r.center();
                for k in 0..8 {
                    let a = k as f32 * std::f32::consts::FRAC_PI_4;
                    let d = egui::vec2(a.cos(), a.sin());
                    p.line_segment([c + d * 5.5, c + d * 8.0], egui::Stroke::new(2.2, col));
                }
                p.circle_stroke(c, 5.5, egui::Stroke::new(1.6, col));
                p.circle_filled(c, 1.8, col);
                if resp.on_hover_text(t("set.title")).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.settings.open = true; }
            });

        });
    }

    /// Borderless window: let the pointer resize from any edge or corner.
    fn resize_edges(&self, ui: &Ui) {
        let ctx = ui.ctx();
        let r = ctx.content_rect();
        let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
        let m = 5.0;
        let (l, rr, tp, b) = (p.x - r.left() < m, r.right() - p.x < m, p.y - r.top() < m, r.bottom() - p.y < m);
        use egui::{viewport::ResizeDirection as D, CursorIcon as C};
        let dir = match (l, rr, tp, b) {
            (true, _, true, _) => Some((D::NorthWest, C::ResizeNwSe)), (_, true, true, _) => Some((D::NorthEast, C::ResizeNeSw)),
            (true, _, _, true) => Some((D::SouthWest, C::ResizeNeSw)), (_, true, _, true) => Some((D::SouthEast, C::ResizeNwSe)),
            (true, ..) => Some((D::West, C::ResizeHorizontal)), (_, true, ..) => Some((D::East, C::ResizeHorizontal)),
            (_, _, true, _) => Some((D::North, C::ResizeVertical)), (_, _, _, true) => Some((D::South, C::ResizeVertical)),
            _ => None,
        };
        if let Some((d, c)) = dir {
            ctx.set_cursor_icon(c);
            if ctx.input(|i| i.pointer.primary_pressed()) { ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d)); }
        }
    }

    /// Bottom status bar; returns true when a venue was switched on/off (engine must restart).
    fn status(&mut self, ui: &mut Ui, stats: &engine::Stats) -> bool {
        let mut restart = false;
        let now = Instant::now();
        if now.duration_since(self.rate.0) >= Duration::from_secs(1) {
            // saturating: switching the base asset resets the engine's counters
            self.rate.2 = stats.total.saturating_sub(self.rate.1) as f64 / now.duration_since(self.rate.0).as_secs_f64();
            self.rate = (now, stats.total, self.rate.2);
        }
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let nowms = now_ms();
            for e in Exchange::ALL {
                let last = stats.venue.iter().filter(|((x, _), _)| *x == e).map(|(_, v)| v.1).max();
                let alive = last.is_some_and(|l| nowms - l < 5_000);
                let on = !self.eng.off.contains(&e);
                let lat = stats.latency(e);
                let lat_col = match lat { Some(l) if l < 150.0 => up(), Some(l) if l < 400.0 => WARN, Some(_) => dn(), None => DIM };
                let resp = status_venue(ui, e, on, alive, lat, lat_col)
                    .on_hover_text(format!("{e:?}: {}", if on { t("status.scope_on") } else { t("status.scope_off") }));
                // switching a venue off disconnects it: the engine restarts without it
                if resp.clicked() {
                    if on && self.eng.off.len() + 1 < Exchange::ALL.len() { self.eng.off.insert(e); restart = true; }
                    else if !on { self.eng.off.remove(&e); restart = true; }
                }
            }
            ui.separator();
            ui.label(RichText::new(format!("{:.0} {}", self.rate.2, t("status.msgs"))).font(mono(10.5)).color(MU));
            if let Some(mb) = stats.rss_mb {
                ui.label(RichText::new(format!("{} {mb}MB", t("status.mem"))).font(mono(10.5)).color(if mb > 1500 { dn() } else if mb > 800 { WARN } else { DIM }));
            }
            let bl = stats.backlog;
            ui.label(RichText::new(format!("{} {bl}", t("status.backlog"))).font(mono(10.5)).color(if bl > 10_000 { dn() } else if bl > 1_000 { WARN } else { DIM }));
            let h = if stats.hist_pending == 0 { t("status.history_done").to_string() } else { format!("{}/{}", stats.hist_total - stats.hist_pending, stats.hist_total) };
            ui.label(RichText::new(format!("{} {h}", t("status.history"))).font(prop(10.5)).color(if stats.hist_pending == 0 { MU } else { WARN }));
            if !stats.hist_errors.is_empty() {
                ui.label(RichText::new(format!("{} errors", stats.hist_errors.len())).font(mono(10.5)).color(dn())).on_hover_text(stats.hist_errors.join("\n"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // language switch (rightmost, persisted)
                if link(ui, t("lang.switch")).clicked() { set_zh(!is_zh()); }
                ui.add_space(10.0);
                ui.label(RichText::new(format!("UTC {}", { let s = now_ms() / 1000 % 86_400; format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) })).font(mono(10.5)).color(MU));
            });
        });
        restart
    }

    /// T1_SHOT=<png path> [T1_SHOT_AFTER=<s>]: save one screenshot and exit (used for visual checks).
    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, after)) = self.shot.clone() else { return };
        if !self.shot_sent && self.started.elapsed().as_secs_f64() > after {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.shot_sent = true;
        }
        let img = ctx.input(|i| i.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
        if let Some(img) = img {
            let f = std::fs::File::create(&path).expect("screenshot file");
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), img.size[0] as u32, img.size[1] as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
            enc.write_header().and_then(|mut w| w.write_image_data(&bytes)).expect("png");
            std::process::exit(0);
        }
    }
}

struct Cc { egui_ctx: egui::Context }

impl eframe::App for App {
    /// the gutters between cards show the window clear color: the darkest base
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] { bg().to_normalized_gamma_f32() }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // with `--features hotpatch` under `dx serve --hotpatch`, edits to this crate swap in live
        #[cfg(feature = "hotpatch")]
        return subsecond::call(|| self.frame(ui));
        #[cfg(not(feature = "hotpatch"))]
        self.frame(ui)
    }
}

impl App {
    /// Write settings.json when it changed (checked at most once a second; never during
    /// screenshot runs, which use env overrides). Temp file + rename so a crash can't truncate it.
    fn save_settings(&mut self) {
        if self.shot.is_some() || self.saved.1.elapsed() < Duration::from_secs(1) { return; }
        self.saved.1 = Instant::now();
        let mut off: Vec<Exchange> = self.eng.off.iter().copied().collect();
        off.sort_by_key(|e| *e as u8);
        // sorted keys so an unchanged state serializes identically (no rewrite every second)
        let auto = self.eng.account.lock().unwrap().auto.iter().map(|(e, r)| (format!("{e:?}"), *r)).collect();
        let s = SettingsRef { route: &self.route, prefs: &self.prefs, lang: if is_zh() { "zh" } else { "en" }, base: &self.eng.base, mode: self.mode, chart: &self.chart, trade_ex: self.trade.ex, off, auto };
        let Ok(json) = serde_json::to_string_pretty(&s) else { return };
        if json == self.saved.0 { return; }
        let Some(p) = settings_path() else { return };
        let tmp = p.with_extension("json.tmp");
        let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &p).is_ok();
        if ok { self.saved.0 = json; } else { eprintln!("settings: cannot write {}", p.display()); }
    }

    pub fn frame(&mut self, ui: &mut Ui) {
        // Frame cap: macOS stops vsync-throttling occluded / off-space windows, and then every
        // repaint request renders immediately, piling up GPU command buffers (GBs within
        // seconds). The cap bounds that regardless of vsync.
        // 120 fps (ProMotion) while focused, 30 fps in the background
        let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
        let budget = Duration::from_micros(if focused { 8_333 } else { 33_333 });
        let since = self.last_frame.elapsed();
        if since < budget && !self.hosted { std::thread::sleep(budget - since); }
        self.last_frame = Instant::now();
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(100));
        // T1_SPIN: repaint every frame (diagnostics: worst case of constant mouse movement)
        if std::env::var("T1_SPIN").is_ok() { ctx.request_repaint(); }
        self.screenshot(&ctx);
        self.save_settings();
        if !self.hosted { self.resize_edges(ui); }
        let (mut new_base, mut new_mode, mut restart) = (None, None, false);
        // T1_CYCLE=<s>: switch base every <s> seconds (diagnostics for resubscription leaks)
        if let Some(every) = std::env::var("T1_CYCLE").ok().and_then(|s| s.parse::<u64>().ok()) {
            let n = self.started.elapsed().as_secs() / every;
            let want = ["BTC", "ETH", "SOL"][n as usize % 3];
            if self.eng.base != want { new_base = Some(want.to_string()); eprintln!("[cycle] -> {want}"); }
        }
        {
            let agg = self.eng.agg.clone();
            let a = agg.lock().unwrap();
            let stats_arc = self.eng.stats.clone();
            let stats = stats_arc.lock().unwrap();
            // panels are rounded cards on the darker base, separated by gutters instead of lines
            let card = |m: i8| Frame::new().fill(panel()).corner_radius(6).inner_margin(Margin::symmetric(m, 5)).outer_margin(Margin::same(2));
            let flat = |m: i8| Frame::new().fill(bg()).inner_margin(Margin::symmetric(m, 4));

            if !self.hosted {
            egui::Panel::top("hdr").show_separator_line(false).frame(Frame::new().fill(bg()).inner_margin(Margin::symmetric(10, 3)))
                .show(ui, |ui| self.header(ui, &a, &mut new_base, &mut new_mode));
            egui::Panel::bottom("status").show_separator_line(false).frame(flat(12)).exact_size(24.0).show(ui, |ui| restart = self.status(ui, &stats));
            drop(stats);
            // without keys the account strip is just its tab row and a hint
            let has_keys = !self.eng.account.lock().unwrap().keys.is_empty();
            let acc = egui::Panel::bottom("account").show_separator_line(false).frame(card(12));
            let acc = if has_keys { acc.resizable(true).default_size(210.0) } else { acc.resizable(false).exact_size(46.0) };
            acc.show(ui, |ui| self.trade.account(ui, &self.eng));
            if self.mode == Market::Perp {
                let lat = self.eng.stats.lock().unwrap().latency(self.trade.ex);
                egui::Panel::right("order").show_separator_line(false).frame(card(10)).resizable(false).exact_size(296.0)
                    .show(ui, |ui| egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| self.trade.panel(ui, &a, &self.eng, &self.eng.base.clone(), lat)));
            }
            } else { drop(stats); }
            // the book's rows are drawn here at display rate; hosted, its header (and the Venues / Quant
            // pages) are native views laid over a fixed 300 pt column
            if self.mode != Market::Option {
                let panel = egui::Panel::right("book").show_separator_line(theme::native()).frame(card(8));
                let panel = if self.hosted { panel.resizable(false).exact_size(300.0) } else { panel.resizable(true).default_size(320.0) };
                let r = panel
                    .show(ui, |ui| self.book.show(ui, &a, self.mode, &self.eng.venues().into_iter().collect(), &self.eng.base.clone()));
                self.book_w = r.response.rect.width();
                if let Some(c) = self.book.clicked.take() { self.trade.fill = Some(c); }
            }
            // positions and resting orders of the traded symbol, drawn on the chart
            {
                let sym = terminal_one::trade::symbol(&self.eng.base);
                let acc = self.eng.account.lock().unwrap();
                let mut lines: Vec<chart::ChartLine> = acc.positions.iter().filter(|p| p.symbol == sym).map(|p| chart::ChartLine {
                    price: p.entry, col: if p.side == terminal_one::Side::Buy { up() } else { dn() },
                    tag: format!("{:?} {} {}", p.ex, if p.side == terminal_one::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(p.qty)),
                    sub: format!("{:+.2}", p.upnl),
                }).collect();
                lines.extend(acc.orders.iter().filter(|o| o.symbol == sym && o.price > 0.0).map(|o| chart::ChartLine {
                    price: o.price, col: if o.side == terminal_one::Side::Buy { up() } else { dn() }.linear_multiply(0.75),
                    tag: format!("{:?} {}", o.ex, o.kind), sub: format!("{} {}", if o.side == terminal_one::Side::Buy { t("tr.buy") } else { t("tr.sell") }, fmt_qty(o.qty - o.filled)),
                }));
                // liquidation price of each position (none on Binance Portfolio Margin: account-level uniMMR)
                lines.extend(acc.positions.iter().filter(|p| p.symbol == sym).filter_map(|p| p.liq.filter(|l| *l > 0.0).map(|l| chart::ChartLine {
                    price: l, col: theme::WARN,
                    tag: format!("{:?} Liq.", p.ex), sub: format!("{} {}", if p.side == terminal_one::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(p.qty)),
                })));
                // take-profit / stop-loss triggers
                lines.extend(acc.tpsl.iter().filter(|x| x.symbol == sym).map(|x| chart::ChartLine {
                    price: x.trigger_px, col: if x.take_profit { up() } else { dn() }.linear_multiply(0.8),
                    tag: format!("{:?} {}", x.ex, if x.take_profit { "TP" } else { "SL" }),
                    sub: x.qty.map(fmt_qty).unwrap_or_else(|| t("tr.whole").to_string()),
                }));
                self.chart.lines = lines;
            }
            egui::CentralPanel::default().frame(card(10)).show(ui, |ui| {
                if self.mode == Market::Option { self.opt.show(ui, &a) } else { self.chart.show(ui, &a, self.mode, &self.eng.base.clone()) }
            });
        }
        if let Some(b) = self.pending_base.take() { new_base = Some(b); }
        if let Some(m) = self.pending_mode.take() { new_mode = Some(m); }
        restart |= std::mem::take(&mut self.pending_restart);
        if new_base.is_some() || restart {
            let switched = new_base.is_some();
            let b = new_base.unwrap_or_else(|| self.eng.base.clone());
            self.eng.start(&b, &ctx);
            if self.mode != Market::Perp && self.mode != Market::Spot { self.eng.ensure(self.mode); }
            if switched { self.chart.reset_view(); }
            if switched || self.chart.src.is_some_and(|e| self.eng.off.contains(&e)) { self.chart.src = None; }
            if switched || self.book.src.is_some_and(|e| self.eng.off.contains(&e)) { self.book.src = None; }
            self.rate = (Instant::now(), 0, 0.0);
        }
        if let Some(m) = new_mode { self.mode = m; self.eng.ensure(m); }
        if std::mem::take(&mut self.trade.open_settings) { self.settings.open_tab(2); }
        // T1_SETTINGS=<tab>: open the settings dialog once (screenshots)
        if let Some(n) = std::env::var("T1_SETTINGS").ok().and_then(|s| s.parse().ok()) { if !self.shot_sent && self.started.elapsed().as_secs() < 2 { self.settings.open_tab(n); } }
        if !self.hosted { if let Some(ex) = self.settings.show(&ctx, &mut self.prefs, self.trade.ex, &mut self.eng) { self.trade.ex = ex; self.trade.reset_price(); } }
        self.trade.confirm_orders = self.prefs.confirm;
        self.eng.ensure_tf(chart::native_tf(self.chart.tf) as u32, &ctx);
        if self.eng.wants_options.swap(false, std::sync::atomic::Ordering::Relaxed) { self.eng.ensure(Market::Option); }
    }
}

/// macOS-style close / minimize / zoom drawn by the app (the window has no native title bar).
fn window_buttons(ui: &mut Ui, maximized: bool) {
    let cols = [(Color32::from_rgb(0xff, 0x5f, 0x57), egui::ViewportCommand::Close),
                (Color32::from_rgb(0xfe, 0xbc, 0x2e), egui::ViewportCommand::Minimized(true)),
                (Color32::from_rgb(0x28, 0xc8, 0x40), egui::ViewportCommand::Maximized(!maximized))];
    let (rect, _) = ui.allocate_exact_size(egui::vec2(58.0, 20.0), egui::Sense::hover());
    let group_hover = ui.rect_contains_pointer(rect);
    for (i, (c, cmd)) in cols.into_iter().enumerate() {
        let center = egui::pos2(rect.left() + 7.0 + i as f32 * 20.0, rect.center().y);
        let r = egui::Rect::from_center_size(center, egui::vec2(13.0, 13.0));
        let resp = ui.interact(r, ui.id().with(("winbtn", i)), egui::Sense::click());
        ui.painter().circle_filled(center, 6.0, c);
        if group_hover {
            let s = egui::Stroke::new(1.2, Color32::from_black_alpha(150));
            let d = 2.8;
            match i {
                0 => { ui.painter().line_segment([center - egui::vec2(d, d), center + egui::vec2(d, d)], s); ui.painter().line_segment([center + egui::vec2(-d, d), center + egui::vec2(d, -d)], s); }
                1 => { ui.painter().line_segment([center - egui::vec2(d + 0.5, 0.0), center + egui::vec2(d + 0.5, 0.0)], s); }
                _ => { ui.painter().line_segment([center - egui::vec2(d, -d), center + egui::vec2(d, -d)], s); ui.painter().line_segment([center + egui::vec2(-d, -d), center + egui::vec2(-d, d)], s); }
            }
        }
        if resp.clicked() { ui.ctx().send_viewport_cmd(cmd); }
    }
}

/// Status bar venue: logo (dim when excluded from the aggregate), name, push latency.
fn status_venue(ui: &mut Ui, e: Exchange, on: bool, alive: bool, lat: Option<f64>, lat_col: Color32) -> egui::Response {
    let name = ui.painter().layout_no_wrap(format!("{e:?}"), prop(10.5), if on { MU } else { DIM });
    let ms = ui.painter().layout_no_wrap(lat.map(|l| format!("{l:.0}ms")).unwrap_or("-".into()), mono(10.0), if alive { lat_col } else { dn() });
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0 + name.size().x + 4.0 + ms.size().x, 18.0), egui::Sense::click());
    if resp.hovered() { ui.painter().rect_filled(rect.expand(2.0), 3, HL); }
    icon(ui.painter(), e, egui::Rect::from_center_size(egui::pos2(rect.left() + 6.5, rect.center().y), egui::vec2(12.0, 12.0)), !on);
    let y = rect.center().y;
    let x = rect.left() + 17.0;
    ui.painter().galley(egui::pos2(x, y - name.size().y / 2.0), name.clone(), MU);
    ui.painter().galley(egui::pos2(x + name.size().x + 4.0, y - ms.size().y / 2.0), ms, MU);
    resp
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn number_formats() {
        assert_eq!([2.0, 1.0, 0.5, 0.1, 0.004, 0.00001].map(step_dp), [0, 0, 1, 1, 3, 5]);
        assert_eq!(fmt_dp(86256.0, 0), "86,256");
        assert_eq!(fmt_dp(-1234567.891, 2), "-1,234,567.89");
        assert_eq!(fmt_px(120.204), "120.20");
        assert_eq!(fmt_px(0.10872), "0.108720");
    }
}
