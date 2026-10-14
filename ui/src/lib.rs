//! egui terminal: header (asset, market mode, cross-venue stats, venue scope), chart or option
//! view, order book / tape, venue table and status bar.
mod account;
mod book;
mod chart;
mod order;
pub(crate) mod kit;
mod engine;
mod options;
mod picker;
mod top;
mod venues;
pub mod theme;
mod trade;
mod settings;
mod native;

use eframe::egui::{self, Color32, Frame, Margin, RichText, Ui};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use depth::agg::Agg;
use depth::{now_ms, Exchange, Market};
use theme::*;

const MODES: [(Market, &str); 4] = [(Market::Spot, "mode.spot"), (Market::Margin, "mode.margin"), (Market::Perp, "mode.perp"), (Market::Option, "mode.option")];

/// UI languages: (code, native name). Strings in assets/i18n/<code>.txt, English fallback.
pub const LANGS: [(&str, &str); 11] = [
    ("en", "English"),
    ("zh-CN", "简体中文"),
    ("zh-TW", "繁體中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("es", "Español"),
    ("pt", "Português"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("ru", "Русский"),
    ("vi", "Tiếng Việt"),
];
const LANG_FILES: [&str; 11] = [
    include_str!("../../assets/i18n/en.txt"),
    include_str!("../../assets/i18n/zh-CN.txt"),
    include_str!("../../assets/i18n/zh-TW.txt"),
    include_str!("../../assets/i18n/ja.txt"),
    include_str!("../../assets/i18n/ko.txt"),
    include_str!("../../assets/i18n/es.txt"),
    include_str!("../../assets/i18n/pt.txt"),
    include_str!("../../assets/i18n/fr.txt"),
    include_str!("../../assets/i18n/de.txt"),
    include_str!("../../assets/i18n/ru.txt"),
    include_str!("../../assets/i18n/vi.txt"),
];
static LANG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Index into LANGS; unknown codes fall back to English ("zh" is the old Simplified Chinese code).
pub fn set_lang(code: &str) {
    let code = if code == "zh" { "zh-CN" } else { code };
    LANG.store(LANGS.iter().position(|(c, _)| *c == code).unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}
pub fn lang() -> usize { LANG.load(std::sync::atomic::Ordering::Relaxed) }
pub fn lang_code() -> &'static str { LANGS[lang()].0 }

fn strings(src: &'static str) -> HashMap<&'static str, &'static str> {
    src.lines().filter(|l| !l.trim_start().starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).filter(|(_, v)| !v.is_empty()).collect()
}

/// Native-panel strings ("English = translation", app/Resources/<lang>-*.txt), shared with the
/// SwiftUI host so the egui ports of its panels read the same translations.
macro_rules! native_strings {
    ($($l:literal),*) => { [$( ($l, concat!(
        include_str!(concat!("../../app/Resources/", $l, "-account.txt")), "\n",
        include_str!(concat!("../../app/Resources/", $l, "-order.txt")), "\n",
        include_str!(concat!("../../app/Resources/", $l, "-panels.txt")), "\n",
        include_str!(concat!("../../app/Resources/", $l, "-top.txt")))) ),*] };
}
const NATIVE_FILES: [(&str, &str); 10] = native_strings!("zh-CN", "zh-TW", "ja", "ko", "es", "pt", "fr", "de", "ru", "vi");

/// `en` (an English UI string of the native panels) in the current language.
pub fn l(en: &'static str) -> &'static str {
    static MAPS: OnceLock<HashMap<&'static str, HashMap<&'static str, &'static str>>> = OnceLock::new();
    let m = MAPS.get_or_init(|| NATIVE_FILES.iter().map(|(code, src)| {
        let mut out = HashMap::new();
        for (k, v) in src.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).filter(|(_, v)| !v.is_empty()) {
            out.entry(k).or_insert(v);
        }
        (*code, out)
    }).collect());
    m.get(lang_code()).and_then(|t| t.get(en)).copied().unwrap_or(en)
}

/// `l(fmt)` with printf-style placeholders (%@ %d %s %.Nf %g) filled from `args` in order, as the
/// Swift panels' `String(format:)` does.
pub fn lf(fmt: &'static str, args: &[String]) -> String {
    let src = l(fmt);
    let (mut out, mut it, mut chars) = (String::new(), args.iter(), src.chars().peekable());
    while let Some(c) = chars.next() {
        if c != '%' { out.push(c); continue; }
        if chars.peek() == Some(&'%') { chars.next(); out.push('%'); continue; }
        // skip flags / width / precision up to the conversion letter
        while let Some(&d) = chars.peek() { chars.next(); if d.is_ascii_alphabetic() || d == '@' { break; } }
        if let Some(a) = it.next() { out.push_str(a); }
    }
    out
}

/// UI string for `key` in the current language, else English, else the key.
pub fn t(key: &'static str) -> &'static str {
    static MAPS: OnceLock<Vec<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let m = MAPS.get_or_init(|| LANG_FILES.iter().map(|f| strings(f)).collect());
    m[lang()].get(key).or_else(|| m[0].get(key)).copied().unwrap_or(key)
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

/// Display time zone as minutes from UTC (Settings > General); i64::MIN follows the system.
static TZ_MIN: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
pub fn set_tz(min: Option<i32>) { TZ_MIN.store(min.map_or(i64::MIN, i64::from), std::sync::atomic::Ordering::Relaxed); }

/// UTC offset of the chosen display time zone in ms (the system's, read once, by default).
pub fn local_offset_ms() -> i64 {
    static O: OnceLock<i64> = OnceLock::new();
    match TZ_MIN.load(std::sync::atomic::Ordering::Relaxed) {
        i64::MIN => *O.get_or_init(depth::sys::utc_offset_ms),
        m => m * 60_000,
    }
}

/// "UTC", "UTC+8", "UTC+5:30" for an offset in minutes.
pub fn tz_label(min: i64) -> String {
    if min == 0 { return "UTC".into(); }
    let (s, a) = (if min < 0 { '-' } else { '+' }, min.abs());
    if a % 60 == 0 { format!("UTC{s}{}", a / 60) } else { format!("UTC{s}{}:{:02}", a / 60, a % 60) }
}

pub fn hms(ms: i64) -> String {
    let s = (ms + local_offset_ms()).rem_euclid(86_400_000) / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// Persisted UI state: settings.json in `sys::data_dir()`.
/// Written when it changes (checked once a second); env overrides (T1_BASE, ...) win on load.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Settings { route: Option<depth::route::Policy>, prefs: Option<settings::Prefs>, lang: Option<String>, base: Option<String>, mode: Option<Market>, chart: Option<chart::Chart>, layout: Option<String>, cell: Option<usize>, charts: Option<Vec<(usize, chart::Chart)>>, trade_ex: Option<Exchange>, off: Option<Vec<Exchange>>, auto: Option<HashMap<Exchange, engine::AutoTopUp>>, order_panel: Option<bool> }

#[derive(serde::Serialize)]
struct SettingsRef<'a> { route: &'a depth::route::Policy, prefs: &'a settings::Prefs, lang: &'static str, base: &'a str, mode: Market, chart: &'a chart::Chart, layout: &'a str, cell: usize, charts: &'a [(usize, chart::Chart)], trade_ex: Exchange, off: Vec<Exchange>, auto: std::collections::BTreeMap<String, engine::AutoTopUp>, order_panel: bool }

fn settings_path() -> Option<std::path::PathBuf> {
    Some(depth::sys::data_dir()?.join("settings.json"))
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
    route: depth::route::Policy,
    /// actions from the native panels, applied at the next frame
    pending_base: Option<String>,
    pending_mode: Option<Market>,
    pending_restart: bool,
    /// order book column width (points), so the native header above it lines up
    book_w: f32,
    /// chart layout ("1", "2", "2v", "3", "4"), the active cell (the toolbar drives `chart`, shown
    /// there) and the other cells' charts with their slot
    pub(crate) layout: String,
    cell: usize,
    others: Vec<(usize, chart::Chart)>,
    /// egui ports of the native panels: their state document (when built, stale) and the order panel
    st: (std::sync::Arc<serde_json::Value>, Instant, bool),
    order: order::OrderPanel,
    /// a venue switched in the status bar, applied once the agg lock is free: (venue, on)
    venue_toggle: Option<(Exchange, bool)>,
    /// order panel shown (Perp; Cmd-Opt-I or the header button), persisted
    order_panel: bool,
    /// header price flash; the book column's Venues / Quant pages
    top: top::TopBar,
    venue_pages: venues::VenuePages,
    account: account::AccountPanel,
}

/// Cell rects of a chart layout: "2" side by side, "2v" stacked, "3" one left and two right,
/// "4" a 2x2 grid; anything else is one chart.
fn layout_cells(layout: &str, r: egui::Rect) -> Vec<egui::Rect> {
    let g = 6.0;
    let (l, rt) = (egui::Rect::from_min_max(r.min, egui::pos2(r.center().x - g / 2.0, r.max.y)), egui::Rect::from_min_max(egui::pos2(r.center().x + g / 2.0, r.min.y), r.max));
    let split = |x: egui::Rect| [egui::Rect::from_min_max(x.min, egui::pos2(x.max.x, x.center().y - g / 2.0)), egui::Rect::from_min_max(egui::pos2(x.min.x, x.center().y + g / 2.0), x.max)];
    match layout {
        "2" => vec![l, rt],
        "2v" => split(r).to_vec(),
        "3" => { let [b, c] = split(rt); vec![l, b, c] }
        "4" => { let ([a, c], [b, d]) = (split(l), split(rt)); vec![a, b, c, d] }
        _ => vec![r],
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self { Self::with_ctx(&cc.egui_ctx, false) }

    /// Build the app on any egui context (eframe window, or the SwiftUI host's Metal view).
    pub fn with_ctx(ctx: &egui::Context, hosted: bool) -> Self {
        let cc = Cc { egui_ctx: ctx.clone() };
        theme::set_native(hosted);
        // history of symbols not viewed for a month, old coin logos
        std::thread::spawn(|| depth::sys::prune_cache(30));
        theme::install(&cc.egui_ctx);
        #[cfg(feature = "hotpatch")]
        {
            let c = cc.egui_ctx.clone();
            subsecond::register_handler(std::sync::Arc::new(move || c.request_repaint()));
        }
        let env = |k: &str| std::env::var(k).ok();
        let saved = settings_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let st: Settings = serde_json::from_str(&saved).unwrap_or_default();
        set_lang(&env("T1_LANG").or(st.lang.clone()).unwrap_or_default());
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
        if let Some(ex) = st.trade_ex.filter(|e| depth::trade::TRADABLE.contains(e)) { trade.ex = ex; }
        // T1_ACC_TAB=<n>: open the account strip on tab n (screenshots)
        let mut picker = picker::Picker::default();
        picker.open = env("T1_PICK").is_some();
        // T1_BOOK_PANE=<0..3>: book, trades, venues, quant (screenshots, as in the SwiftUI host)
        let mut book = book::BookView::default();
        match env("T1_BOOK_PANE").and_then(|s| s.parse::<u8>().ok()) { Some(1) => book.native_set(&serde_json::json!({"trades": true})), Some(p @ 2..=3) => book.page = p, _ => {} }
        if let Some(tf) = env("T1_TF").and_then(|s| s.parse().ok()) { chart.tf = tf; }
        let layout = env("T1_LAYOUT").or(st.layout).unwrap_or_else(|| "1".into());
        App {
            eng, prefs, settings: Default::default(), saved: (saved, Instant::now()), mode, chart, book, opt: Default::default(), picker, trade,
            rate: (Instant::now(), 0, 0.0),
            shot: env("T1_SHOT").map(|p| (p, env("T1_SHOT_AFTER").and_then(|s| s.parse().ok()).unwrap_or(30.0))),
            shot_sent: false, started: Instant::now(), last_frame: Instant::now(), hosted,
            route: st.route.clone().unwrap_or_default(), pending_base: None, pending_mode: None, pending_restart: false, book_w: 0.0,
            layout, cell: st.cell.unwrap_or(0), others: st.charts.unwrap_or_default(),
            st: (Default::default(), Instant::now(), true), order: Default::default(), venue_toggle: None, order_panel: st.order_panel.unwrap_or(true), top: Default::default(), venue_pages: Default::default(),
            account: Default::default(),
        }
    }

    fn header(&mut self, ui: &mut Ui, a: &Agg, st: &serde_json::Value, new_base: &mut Option<String>, new_mode: &mut Option<Market>) {
        // the header is the title bar: drag empty space to move, double-click to maximize
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        // own window only: the SwiftUI host's title bar handles dragging and zooming
        if !self.hosted {
            let bar = ui.interact(ui.max_rect(), egui::Id::new("titlebar"), egui::Sense::click_and_drag());
            if bar.drag_started_by(egui::PointerButton::Primary) { ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag); }
            if bar.double_clicked() { ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)); }
        }
        // title bar, 46 px: lights, pair, market pills, headline price ... clock, panel toggle, settings
        ui.horizontal(|ui| {
            ui.set_height(46.0);
            ui.spacing_mut().item_spacing.x = 14.0;
            // hosted: macOS draws the traffic lights over this space
            if self.hosted { ui.add_space(56.0); } else { window_buttons(ui, maximized); }
            let tickers = self.eng.tickers.lock().unwrap().clone();
            if let Some(b) = self.picker.show(ui, &self.eng.base, &tickers) { if b != self.eng.base { *new_base = Some(b); } }
            let names: Vec<&str> = MODES.iter().map(|(_, k)| t(k)).collect();
            let sel = MODES.iter().position(|(m, _)| *m == self.mode).unwrap_or(0);
            if let Some(i) = top::pills(ui, "modes", &names, sel, 26.0, 13.0) { *new_mode = Some(MODES[i].0); }
            let book_market = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
            self.top.price(ui, a.mid(book_market));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if kit::icon_button(ui, 30.0, false, top::gear).on_hover_text(t("set.title")).clicked() { self.settings.open = true; }
                // show / hide the order panel (the Swift toolbar's sidebar.trailing button); Perp only
                let tip = l("Show or hide the order panel (Option-Command-I)");
                if self.mode == Market::Perp {
                    let on = self.order_panel;
                    if kit::icon_button(ui, 30.0, false, |p, r, c| top::panel_glyph(p, r, c, on)).on_hover_text(tip).clicked() { self.order_panel = !on; }
                } else {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    top::panel_glyph(ui.painter(), r, dim().gamma_multiply(0.6), false);
                }
                ui.add_space(6.0);
                top::TopBar::clock(ui);
            });
        });
        // stats strip, 44 px, hairline below; scrolls when the window is narrow
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add_space(5.0);
        egui::ScrollArea::horizontal().id_salt("hdr_stats").auto_shrink([false, true])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| ui.horizontal(|ui| { ui.add_space(4.0); top::TopBar::stats(ui, st, self.mode); }));
        ui.add_space(6.0);
        let w = ui.ctx().content_rect().x_range();
        ui.painter().hline(w, ui.min_rect().bottom() + 0.5, egui::Stroke::new(1.0, line().gamma_multiply(0.7)));
    }
    /// Keyboard shortcuts of the SwiftUI menus (App.swift): Cmd-, settings, Cmd-Opt-I order panel,
    /// Cmd-K pair, Cmd-[ / Cmd-] favorites, Cmd-D star, Cmd-1..4 market, Ctrl-1..6 interval.
    /// Never while a text field has focus or a dialog is open.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() { return; }
        use egui::{Key, Modifiers as M};
        let k = |m: M, key: Key| ctx.input_mut(|i| i.consume_key(m, key));
        if k(M::COMMAND, Key::Comma) { self.settings.open = true; }
        if self.settings.open { return; }
        if k(M::COMMAND | M::ALT, Key::I) { self.order_panel = !self.order_panel; }
        // Cmd-K, Cmd-[ / Cmd-], Cmd-D are the pair picker's own (picker.rs)
        const NUM: [Key; 6] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6];
        for (i, (m, _)) in MODES.iter().enumerate() {
            if k(M::COMMAND, NUM[i]) && self.mode != *m { self.pending_mode = Some(*m); }
        }
        for (i, tf) in [1, 5, 15, 60, 240, 1440].into_iter().enumerate() {
            if k(M::CTRL, NUM[i]) { self.chart.tf = tf; }
        }
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

    /// Bottom status bar (venue switches connect / disconnect only that venue).
    fn status(&mut self, ui: &mut Ui, stats: &engine::Stats) {
        let now = Instant::now();
        if now.duration_since(self.rate.0) >= Duration::from_secs(1) {
            // saturating: switching the base asset resets the engine's counters
            self.rate.2 = stats.total.saturating_sub(self.rate.1) as f64 / now.duration_since(self.rate.0).as_secs_f64();
            self.rate = (now, stats.total, self.rate.2);
        }
        // hairline on top, across the window
        let top = ui.max_rect().top() - 0.5;
        ui.painter().hline(ui.ctx().content_rect().x_range(), top, egui::Stroke::new(1.0, line().gamma_multiply(0.7)));
        let txt = |s: String, col: Color32| RichText::new(s).font(mono(11.5)).color(col);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let nowms = now_ms();
            for e in Exchange::ALL {
                let last = stats.venue.iter().filter(|((x, _), _)| *x == e).map(|(_, v)| v.1).max();
                let alive = last.is_some_and(|l| nowms - l < 5_000);
                let on = !self.eng.off.contains(&e);
                let lat = stats.latency(e);
                let lat_col = match lat { Some(l) if l < 150.0 => up(), Some(l) if l < 400.0 => WARN, Some(_) => dn(), None => dim() };
                let resp = status_venue(ui, e, on, alive, lat, lat_col)
                    .on_hover_text(format!("{e:?}: {}", if on { t("status.scope_on") } else { t("status.scope_off") }));
                // switching a venue off disconnects only that venue; on connects only it
                // applied after the frame's agg lock is released (set_venue takes it): doing it here deadlocked
                if resp.clicked() && (!on || self.eng.off.len() + 1 < Exchange::ALL.len()) { self.venue_toggle = Some((e, !on)); }
            }
            // metrics on the right (laid out right to left)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // language switch (rightmost, persisted)
                let cur = lang();
                let g = ui.painter().layout_no_wrap(LANGS[cur].1.to_string(), prop(11.5), mu());
                let (r, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 26.0, 20.0), egui::Sense::click());
                let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
                let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || open, 0.1);
                if hv > 0.0 { ui.painter().rect_filled(r, kit::R, hl().gamma_multiply(0.7 * hv)); }
                let col = lerp(mu(), fg(), hv);
                ui.painter().galley(egui::pos2(r.left() + 7.0, r.center().y - g.size().y / 2.0), g, col);
                chevron(ui, egui::Rect::from_center_size(egui::pos2(r.right() - 9.0, r.center().y), egui::vec2(8.0, 8.0)), &ui.visuals().widgets.inactive, open);
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                egui::Popup::from_toggle_button_response(&resp).frame(top::popup_frame(ui, 4)).align(egui::RectAlign::TOP_END).show(|ui| {
                    ui.set_min_width(150.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, (code, name)) in LANGS.iter().enumerate() {
                        let (r, row) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
                        let on = i == cur;
                        if on || row.hovered() { ui.painter().rect_filled(r, kit::R, hl()); }
                        if on { ui.painter().text(egui::pos2(r.left() + 13.0, r.center().y), egui::Align2::CENTER_CENTER, "✓", prop(12.0), accent()); }
                        ui.painter().text(egui::pos2(r.left() + 28.0, r.center().y), egui::Align2::LEFT_CENTER, *name, prop(12.5), if on { fg_strong() } else { fg() });
                        if row.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { set_lang(code); }
                    }
                });
                ui.label(txt(format!("{} {}", tz_label(local_offset_ms() / 60_000), hms(now_ms())), mu()));
                if !stats.hist_errors.is_empty() {
                    ui.label(txt(format!("{} errors", stats.hist_errors.len()), dn())).on_hover_text(stats.hist_errors.join("\n"));
                }
                let h = if stats.hist_pending == 0 { t("status.history_done").to_string() } else { format!("{}/{}", stats.hist_total - stats.hist_pending, stats.hist_total) };
                ui.label(txt(format!("{} {h}", t("status.history")), if stats.hist_pending == 0 { mu() } else { WARN }));
                let bl = stats.backlog;
                ui.label(txt(format!("{} {bl}", t("status.backlog")), if bl > 10_000 { dn() } else if bl > 1_000 { WARN } else { mu() }));
                if let Some(mb) = stats.rss_mb {
                    ui.label(txt(format!("{} {mb} MB", t("status.mem")), if mb > 1500 { dn() } else if mb > 800 { WARN } else { mu() }));
                }
                ui.label(txt(format!("{} {}", fmt_dp(self.rate.2, 0), t("status.msgs")), mu()));
            });
        });
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
        let s = SettingsRef { route: &self.route, prefs: &self.prefs, lang: lang_code(), base: &self.eng.base, mode: self.mode, chart: &self.chart, layout: &self.layout, cell: self.cell, charts: &self.others, trade_ex: self.trade.ex, off, auto, order_panel: self.order_panel };
        let Ok(json) = serde_json::to_string_pretty(&s) else { return };
        if json == self.saved.0 { return; }
        let Some(p) = settings_path() else { return };
        let tmp = p.with_extension("json.tmp");
        let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &p).is_ok();
        if ok { self.saved.0 = json; } else { eprintln!("settings: cannot write {}", p.display()); }
    }

    /// The state document the SwiftUI host polls, for the egui ports of its panels: rebuilt at most
    /// 4 times a second, or right after an op changed something.
    fn state_doc(&mut self) -> std::sync::Arc<serde_json::Value> {
        if self.st.2 || self.st.1.elapsed() > Duration::from_millis(250) {
            self.st = (std::sync::Arc::new(serde_json::from_str(&self.state_json()).unwrap_or_default()), Instant::now(), false);
        }
        self.st.0.clone()
    }

    /// Price alerts: live prices of every base with an armed alert (the one on screen from the
    /// chart's own source, others from `Engine::watch`), checked for crossings.
    pub(crate) fn alerts_tick(&mut self, a: &depth::agg::Agg) {
        let base = self.eng.base.clone();
        let mut bases = self.chart.alert_bases();
        bases.retain(|b| *b != base);
        self.eng.watch(&bases);
        let mut px = self.eng.watched.lock().unwrap().clone();
        if let Some(p) = self.chart.price(a, self.mode) { px.insert(base, p); }
        self.chart.check_alerts(&px);
        // the egui window has no notification API of its own: macOS notifications through osascript
        // (the SwiftUI host drains `fired` itself and posts them natively)
        if !self.hosted && cfg!(target_os = "macos") {
            for (title, body) in self.chart.fired.drain(..) {
                let q = |s: &str| s.replace('\\', "").replace('"', "'");
                let script = format!("display notification \"{}\" with title \"Depth\" subtitle \"{}\"", q(&body), q(&title));
                if let Ok(mut c) = std::process::Command::new("osascript").arg("-e").arg(script).spawn() { std::thread::spawn(move || { let _ = c.wait(); }); }
            }
        }
    }

    /// Orders and triggers dragged on a chart, and prices put into the order panel from it.
    fn chart_actions(&mut self, ctx: &egui::Context) {
        let mut acts = std::mem::take(&mut self.chart.actions);
        for o in self.others.iter_mut() { acts.append(&mut o.1.actions); }
        for act in acts {
            match act {
                chart::ChartAct::Fill(px) => self.trade.fill = Some((self.trade.ex, px)),
                chart::ChartAct::Move(chart::LineAct::Order(ex, id), px) => {
                    let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == ex && o.id == id).cloned();
                    if let Some(o) = o { self.eng.replace_order(&o, px, ctx); }
                }
                chart::ChartAct::Move(chart::LineAct::TpSl(ex, id), px) => {
                    let mut acc = self.eng.account.lock().unwrap();
                    let Some(tp) = acc.tpsl.iter().find(|x| x.ex == ex && x.id == id).cloned() else { continue };
                    // a TP / SL dragged past the price would fire at once: refuse it here, as the order panel does
                    let mark = acc.positions.iter().find(|p| p.ex == ex && p.symbol == tp.symbol && p.side == tp.pos).map_or(0.0, |p| p.mark);
                    match depth::trade::tpsl::validate(tp.pos, mark, tp.take_profit, px) {
                        Err(e) => acc.note(format!("FAIL move {ex:?} {} {}: {e:#}", tp.symbol, if tp.take_profit { "TP" } else { "SL" }), false),
                        Ok(()) => { drop(acc); self.eng.move_tpsl(&tp, px, ctx); }
                    }
                }
            }
        }
    }

    /// Charts for every cell of the layout: new cells copy the active chart with another interval.
    fn fill_cells(&mut self, n: usize) {
        if self.cell >= n {
            if let Some(o) = self.others.iter_mut().find(|o| o.0 == 0) { std::mem::swap(&mut self.chart, &mut o.1); o.0 = self.cell; }
            self.cell = 0;
        }
        for k in (0..n).filter(|k| *k != self.cell) {
            if self.others.iter().any(|o| o.0 == k) { continue; }
            let used: Vec<i64> = std::iter::once(self.chart.tf).chain(self.others.iter().map(|o| o.1.tf)).collect();
            let mut c: chart::Chart = serde_json::to_value(&self.chart).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            c.tf = [240, 60, 15, 1440, 5, 1].into_iter().find(|tf| !used.contains(tf)).unwrap_or(60);
            self.others.push((k, c));
        }
    }

    /// The chart area: one chart, or the layout's cells (a press in a cell makes it the active one).
    fn charts(&mut self, ui: &mut Ui, a: &depth::agg::Agg) {
        let base = self.eng.base.clone();
        self.chart.layout = self.layout.clone();
        for o in self.others.iter_mut() { o.1.layout = self.layout.clone(); if let Some(l) = o.1.layout_req.take() { self.layout = l; } }
        if let Some(l) = self.chart.layout_req.take() { self.layout = l; }
        let cells = layout_cells(&self.layout, ui.available_rect_before_wrap());
        if cells.len() == 1 { self.chart.multi = false; return self.chart.show(ui, a, self.mode, &base); }
        self.fill_cells(cells.len());
        if let Some(p) = ui.input(|i| i.pointer.primary_pressed().then(|| i.pointer.interact_pos()).flatten()) {
            if let Some(k) = cells.iter().position(|r| r.contains(p)).filter(|k| *k != self.cell) {
                if let Some(o) = self.others.iter_mut().find(|o| o.0 == k) { std::mem::swap(&mut self.chart, &mut o.1); o.0 = self.cell; self.cell = k; }
            }
        }
        for (k, r) in cells.iter().enumerate() {
            let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("cell", k)).max_rect(*r));
            child.set_clip_rect(r.intersect(ui.clip_rect()));
            if k == self.cell {
                self.chart.multi = true;
                self.chart.show(&mut child, a, self.mode, &base);
            } else if let Some(o) = self.others.iter_mut().find(|o| o.0 == k) {
                let c = &mut o.1;
                c.multi = true;
                c.lines = self.chart.lines.clone();
                (c.confirm, c.trading, c.equity, c.amends) = (self.chart.confirm, self.chart.trading, self.chart.equity, self.chart.amends);
                c.adopt_drawings(&self.chart);
                c.show(&mut child, a, self.mode, &base);
                self.chart.adopt_drawings(c);
            }
            let col = if k == self.cell { accent().linear_multiply(0.7) } else { line() };
            ui.painter().rect_stroke(*r, 4, egui::Stroke::new(1.0, col), egui::StrokeKind::Outside);
        }
    }

    pub fn frame(&mut self, ui: &mut Ui) {
        set_light(ui.ctx().theme() == egui::Theme::Light);
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
        if !self.hosted { self.resize_edges(ui); self.shortcuts(&ctx); }
        let (mut new_base, mut new_mode, mut restart) = (None, None, false);
        // T1_CYCLE=<s>: switch base every <s> seconds (diagnostics for resubscription leaks)
        if let Some(every) = std::env::var("T1_CYCLE").ok().and_then(|s| s.parse::<u64>().ok()) {
            let n = self.started.elapsed().as_secs() / every;
            let want = ["BTC", "ETH", "SOL"][n as usize % 3];
            if self.eng.base != want { new_base = Some(want.to_string()); eprintln!("[cycle] -> {want}"); }
        }
        {
            // the header's stats come from the state document, built before the agg lock (it takes it itself)
            let hdr_st = if self.hosted { Default::default() } else { self.state_doc() };
            let agg = self.eng.agg.clone();
            let mut a = agg.lock().unwrap();
            let stats_arc = self.eng.stats.clone();
            let stats = stats_arc.lock().unwrap();
            // panels are rounded cards on the darker base: 8 px black gutters, a faint hairline edge
            // (none hosted: the native window shows through transparent panels)
            let edge_c = if theme::native() { Color32::TRANSPARENT } else { line().gamma_multiply(0.8) };
            let card = |m: i8| Frame::new().fill(panel()).corner_radius(kit::R).stroke(egui::Stroke::new(1.0, edge_c))
                .inner_margin(Margin::symmetric(m, 5)).outer_margin(Margin::same(4));
            // title bar + stats strip, status bar: on the base, 14 px sides, 4 px towards the cards
            let flat = |top: i8, bottom: i8| Frame::new().fill(bg()).inner_margin(Margin { left: 14, right: 14, top, bottom });

            if !self.hosted {
            egui::Panel::top("hdr").show_separator_line(false).frame(flat(0, 4))
                .show(ui, |ui| self.header(ui, &a, &hdr_st, &mut new_base, &mut new_mode));
            egui::Panel::bottom("status").show_separator_line(false).frame(flat(4, 0)).exact_size(30.0).show(ui, |ui| self.status(ui, &stats));
            drop(stats);
            // ops from the account strip and the order panel take the agg lock themselves
            // (route planning reads books): never hold it across them
            drop(a);
            let st = self.state_doc();
            let mut ap = std::mem::take(&mut self.account);
            egui::Panel::bottom("account").show_separator_line(false).frame(card(12)).resizable(true).default_size(230.0)
                .show(ui, |ui| ap.show(ui, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r }));
            if let Some(tab) = ap.open_settings.take() { self.settings.open_tab(tab); }
            if self.mode == Market::Perp && self.order_panel {
                let mut op = std::mem::take(&mut self.order);
                egui::Panel::right("order").show_separator_line(false).frame(Frame::new().fill(bg()).inner_margin(Margin::symmetric(4, 4))).resizable(false).exact_size(300.0)
                    .show(ui, |ui| egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                        op.show(ui, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r });
                        ui.add_space(8.0);
                        let g = agg.lock().unwrap();
                        let base = self.eng.base.clone();
                        self.trade.insurance_section(ui, &g, &self.eng, &base);
                    }));
                if let Some(tab) = op.open_settings.take() { self.settings.open_tab(tab); }
                if let Some(ex) = op.open_transfer.take() { ap.open_transfer(&ex, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r }); }
                // a limit close from the account strip, once the panel shows that symbol on that venue
                // (given up after 10 s, so a stale request never fills the form later)
                if let Some((p, at)) = ap.prefill.take() {
                    if st["trade"]["symbol"] == p["symbol"] && st["trade"]["venue"] == p["ex"] { op.prefill_close(&p, &st); }
                    else if at.elapsed() < Duration::from_secs(10) { ap.prefill = Some((p, at)); }
                }
                self.order = op;
            }
            self.account = ap;
            a = agg.lock().unwrap();
            } else { drop(stats); }
            // the book's rows are drawn here at display rate; hosted, its header (and the Venues / Quant
            // pages) are native views laid over a fixed 300 pt column
            if self.mode != Market::Option {
                let panel = egui::Panel::right("book").show_separator_line(theme::native()).frame(card(8));
                let panel = if self.hosted { panel.resizable(false).exact_size(300.0) } else { panel.resizable(true).default_size(320.0) };
                let pane = if self.hosted { 0 } else { self.book.pane() };
                let r = if pane >= 2 {
                    // the venue_share op takes the agg lock itself
                    drop(a);
                    let st = self.state_doc();
                    let mut vp = std::mem::take(&mut self.venue_pages);
                    let r = panel.show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        self.book.tabs(ui);
                        ui.horizontal(|ui| {
                            ui.set_height(26.0);
                            if pane == 2 { ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| vp.window_picker(ui)); }
                        });
                        ui.add_space(2.0);
                        if pane == 2 { vp.venues(ui, &mut |v| self.call(&ctx, &v)); } else { vp.quant(ui, &st["quant"]); }
                    });
                    self.venue_pages = vp;
                    a = agg.lock().unwrap();
                    r
                } else {
                    panel.show(ui, |ui| self.book.show(ui, &a, self.mode, &self.eng.venues().into_iter().collect(), &self.eng.base.clone()))
                };
                self.book_w = r.response.rect.width();
                if let Some(c) = self.book.clicked.take() { self.trade.fill = Some(c); }
            }
            // positions and resting orders of the traded symbol, drawn on the chart
            {
                let sym = depth::trade::symbol(&self.eng.base);
                let acc = self.eng.account.lock().unwrap();
                let mut lines: Vec<chart::ChartLine> = acc.positions.iter().filter(|p| p.symbol == sym).map(|p| chart::ChartLine {
                    price: p.entry, col: if p.side == depth::Side::Buy { up() } else { dn() },
                    tag: format!("{:?} {} {}", p.ex, if p.side == depth::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(p.qty)),
                    sub: format!("{:+.2}", p.upnl), act: None,
                }).collect();
                lines.extend(acc.orders.iter().filter(|o| o.symbol == sym && o.price > 0.0).map(|o| chart::ChartLine {
                    price: o.price, col: if o.side == depth::Side::Buy { up() } else { dn() }.linear_multiply(0.75),
                    tag: format!("{:?} {}", o.ex, o.kind), sub: format!("{} {}", if o.side == depth::Side::Buy { t("tr.buy") } else { t("tr.sell") }, fmt_qty(o.qty - o.filled)),
                    // only plain limit orders are dragged: a stop or post-only order would come back as something else
                    act: o.kind.eq_ignore_ascii_case("limit").then(|| chart::LineAct::Order(o.ex, o.id.clone())),
                }));
                // liquidation price of each position (none on Binance Portfolio Margin: account-level uniMMR)
                lines.extend(acc.positions.iter().filter(|p| p.symbol == sym).filter_map(|p| p.liq.filter(|l| *l > 0.0).map(|l| chart::ChartLine {
                    price: l, col: theme::WARN,
                    tag: format!("{:?} Liq.", p.ex), sub: format!("{} {}", if p.side == depth::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(p.qty)), act: None,
                })));
                // take-profit / stop-loss triggers
                lines.extend(acc.tpsl.iter().filter(|x| x.symbol == sym).map(|x| chart::ChartLine {
                    price: x.trigger_px, col: if x.take_profit { up() } else { dn() }.linear_multiply(0.8),
                    tag: format!("{:?} {}", x.ex, if x.take_profit { "TP" } else { "SL" }),
                    sub: x.qty.map(fmt_qty).unwrap_or_else(|| t("tr.whole").to_string()),
                    act: Some(chart::LineAct::TpSl(x.ex, x.id.clone())),
                }));
                self.chart.lines = lines;
                self.chart.confirm = self.prefs.confirm;
                self.chart.trading = acc.keys.contains_key(&self.trade.ex) && acc.can_trade.get(&self.trade.ex) != Some(&false);
                self.chart.equity = acc.balances.get(&self.trade.ex).map(|b| b.equity).filter(|e| *e > 0.0);
                self.chart.amends = depth::trade::can_amend(self.trade.ex);
            }
            self.alerts_tick(&a);
            egui::CentralPanel::default().frame(card(10)).show(ui, |ui| {
                if self.mode == Market::Option { self.opt.show(ui, &a) } else { self.charts(ui, &a) }
            });
            self.chart_actions(&ctx);
        }
        if let Some(b) = self.pending_base.take() { new_base = Some(b); }
        if let Some(m) = self.pending_mode.take() { new_mode = Some(m); }
        restart |= std::mem::take(&mut self.pending_restart);
        if let Some((e, on)) = self.venue_toggle.take() {
            self.eng.set_venue(e, on, &ctx);
            if !on {
                if self.chart.src == Some(e) { self.chart.src = None; }
                if self.book.src == Some(e) { self.book.src = None; }
            }
        }
        if new_base.is_some() || restart {
            let switched = new_base.is_some();
            let b = new_base.unwrap_or_else(|| self.eng.base.clone());
            self.eng.start(&b, &ctx);
            if self.mode != Market::Perp && self.mode != Market::Spot { self.eng.ensure(self.mode); }
            let off = self.eng.off.clone();
            for c in std::iter::once(&mut self.chart).chain(self.others.iter_mut().map(|o| &mut o.1)) {
                if switched { c.reset_view(); }
                if switched || c.src.is_some_and(|e| off.contains(&e)) { c.src = None; }
            }
            if switched || self.book.src.is_some_and(|e| self.eng.off.contains(&e)) { self.book.src = None; }
            self.rate = (Instant::now(), 0, 0.0);
        }
        if let Some(m) = new_mode { self.mode = m; self.eng.ensure(m); }
        // T1_SETTINGS=<tab>: open the settings dialog once (screenshots)
        if let Some(n) = std::env::var("T1_SETTINGS").ok().and_then(|s| s.parse().ok()) { if !self.shot_sent && self.started.elapsed().as_secs() < 2 { self.settings.open_tab(n); } }
        if !self.hosted && self.settings.open {
            // ops take the agg lock themselves: nothing is held here
            let st = self.state_doc();
            let mut sv = std::mem::take(&mut self.settings);
            sv.show(&ctx, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r });
            self.settings = sv;
        }
        self.eng.ensure_tf(chart::native_tf(self.chart.tf) as u32, &ctx);
        let n = layout_cells(&self.layout, egui::Rect::NOTHING).len();
        let tfs: Vec<i64> = self.others.iter().filter(|o| o.0 < n).map(|o| o.1.tf).collect();
        for tf in tfs { self.eng.ensure_tf(chart::native_tf(tf) as u32, &ctx); }
        if self.eng.wants_options.swap(false, std::sync::atomic::Ordering::Relaxed) { self.eng.ensure(Market::Option); }
    }
}

/// macOS-style close / minimize / zoom drawn by the app (the window has no native title bar).
fn window_buttons(ui: &mut Ui, maximized: bool) {
    let cols = [(Color32::from_rgb(0xff, 0x5f, 0x57), egui::ViewportCommand::Close),
                (Color32::from_rgb(0xfe, 0xbc, 0x2e), egui::ViewportCommand::Minimized(true)),
                (Color32::from_rgb(0x28, 0xc8, 0x40), egui::ViewportCommand::Maximized(!maximized))];
    // 12 px lights, 8 px apart
    let (rect, _) = ui.allocate_exact_size(egui::vec2(52.0, 20.0), egui::Sense::hover());
    let group_hover = ui.rect_contains_pointer(rect);
    for (i, (c, cmd)) in cols.into_iter().enumerate() {
        let center = egui::pos2(rect.left() + 6.0 + i as f32 * 20.0, rect.center().y);
        let r = egui::Rect::from_center_size(center, egui::vec2(14.0, 14.0));
        let resp = ui.interact(r, ui.id().with(("winbtn", i)), egui::Sense::click());
        ui.painter().circle(center, 6.0, c, egui::Stroke::new(0.5, Color32::from_black_alpha(40)));
        if group_hover {
            let s = egui::Stroke::new(1.1, Color32::from_black_alpha(150));
            let d = 2.6;
            match i {
                0 => { ui.painter().line_segment([center - egui::vec2(d, d), center + egui::vec2(d, d)], s); ui.painter().line_segment([center + egui::vec2(-d, d), center + egui::vec2(d, -d)], s); }
                1 => { ui.painter().line_segment([center - egui::vec2(d + 0.5, 0.0), center + egui::vec2(d + 0.5, 0.0)], s); }
                _ => { ui.painter().line_segment([center - egui::vec2(d, -d), center + egui::vec2(d, -d)], s); ui.painter().line_segment([center + egui::vec2(-d, -d), center + egui::vec2(-d, d)], s); }
            }
        }
        if resp.clicked() { ui.ctx().send_viewport_cmd(cmd); }
    }
}

/// Status bar venue: state dot (green streaming, red silent, grey switched off), name, push latency.
fn status_venue(ui: &mut Ui, e: Exchange, on: bool, alive: bool, lat: Option<f64>, lat_col: Color32) -> egui::Response {
    let name = ui.painter().layout_no_wrap(format!("{e:?}"), prop(11.5), if on { mu() } else { dim() });
    let ms_col = if !on { dim() } else if alive { lat_col } else { dn() };
    let ms = ui.painter().layout_no_wrap(if on { lat.map(|l| format!("{l:.0}ms")).unwrap_or("–".into()) } else { "off".into() }, mono(11.5), ms_col);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(12.0 + name.size().x + 5.0 + ms.size().x, 18.0), egui::Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    if hv > 0.0 { p.rect_filled(rect.expand2(egui::vec2(5.0, 1.0)), 3, hl().gamma_multiply(0.7 * hv)); }
    let y = rect.center().y;
    p.circle_filled(egui::pos2(rect.left() + 3.0, y), 3.0, if !on { dim() } else if alive { up() } else { dn() });
    let x = rect.left() + 12.0;
    p.galley(egui::pos2(x, y - name.size().y / 2.0), name.clone(), mu());
    p.galley(egui::pos2(x + name.size().x + 5.0, y - ms.size().y / 2.0), ms, mu());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
