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
mod spot;
mod settings;
mod hotkeys;
mod command;
mod kbd;
mod tray;
mod native;
mod hunter;

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

/// Panel strings ("English = translation", assets/i18n/panels/<lang>-*.txt), looked up by `l`.
macro_rules! panel_strings {
    ($($l:literal),*) => { [$( ($l, concat!(
        include_str!(concat!("../../assets/i18n/panels/", $l, "-account.txt")), "\n",
        include_str!(concat!("../../assets/i18n/panels/", $l, "-order.txt")), "\n",
        include_str!(concat!("../../assets/i18n/panels/", $l, "-panels.txt")), "\n",
        include_str!(concat!("../../assets/i18n/panels/", $l, "-top.txt")))) ),*] };
}
const PANEL_FILES: [(&str, &str); 10] = panel_strings!("zh-CN", "zh-TW", "ja", "ko", "es", "pt", "fr", "de", "ru", "vi");

/// `en` (an English UI string of the panels) in the current language.
pub fn l(en: &'static str) -> &'static str {
    static MAPS: OnceLock<HashMap<&'static str, HashMap<&'static str, &'static str>>> = OnceLock::new();
    let m = MAPS.get_or_init(|| PANEL_FILES.iter().map(|(code, src)| {
        let mut out = HashMap::new();
        for (k, v) in src.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).filter(|(_, v)| !v.is_empty()) {
            out.entry(k).or_insert(v);
        }
        (*code, out)
    }).collect());
    m.get(lang_code()).and_then(|t| t.get(en)).copied().unwrap_or(en)
}

/// `l(fmt)` with printf-style placeholders (%@ %d %s %.Nf %g) filled from `args` in order.
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

/// A price for the menu bar: about five significant digits (81,730 · 4,125.5 · 78.52 · 0.4512).
fn tray_px(v: f64) -> String { let a = v.abs(); if a >= 10_000.0 { fmt_dp(v, 0) } else if a >= 1_000.0 { fmt_dp(v, 1) } else if a >= 1.0 { fmt_dp(v, 2) } else { format!("{v:.4}") } }

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
    let mut s = format!("{v:.d$}");
    // a value that rounds to zero prints without a sign ("-0.00" from -0.0 or -0.001)
    if s.starts_with('-') && s.bytes().all(|c| matches!(c, b'-' | b'0' | b'.')) { s.remove(0); }
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
/// Read by `load_settings`, key by key.
#[derive(Default)]
struct Settings { route: Option<depth::route::Policy>, prefs: Option<settings::Prefs>, lang: Option<String>, base: Option<String>, mode: Option<Market>, chart: Option<chart::Chart>, layout: Option<String>, cell: Option<usize>, charts: Option<Vec<(usize, chart::Chart)>>, trade_ex: Option<Exchange>, off: Option<Vec<Exchange>>, auto: Option<HashMap<Exchange, engine::AutoTopUp>>, order_panel: Option<bool>, window: Option<[f32; 4]>, book: Option<book::BookView>,
    /// the daily loss stop's baseline: (local day number, equity per venue at its start)
    risk_day: Option<(i64, Vec<(Exchange, f64)>)>,
    /// TP/SL still waiting for their entry order (engine::TpSlWait)
    tpsl_wait: Option<Vec<engine::TpSlWait>>,
    hunter: Option<hunter::HunterView> }

#[derive(serde::Serialize)]
struct SettingsRef<'a> { route: &'a depth::route::Policy, prefs: &'a settings::Prefs, lang: &'static str, base: &'a str, mode: Market, chart: &'a chart::Chart, layout: &'a str, cell: usize, charts: &'a [(usize, chart::Chart)], trade_ex: Exchange, off: Vec<Exchange>, auto: std::collections::BTreeMap<String, engine::AutoTopUp>, order_panel: bool, window: Option<[f32; 4]>, book: &'a book::BookView, risk_day: (i64, Vec<(Exchange, f64)>), tpsl_wait: Vec<engine::TpSlWait>, hunter: &'a hunter::HunterView }

fn settings_path() -> Option<std::path::PathBuf> {
    Some(depth::sys::data_dir()?.join("settings.json"))
}

/// The window's last position and size [x, y, w, h] (points), for the plain app to open where it was.
pub fn saved_window() -> Option<[f32; 4]> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(settings_path()?).ok()?).ok()?;
    let w: [f32; 4] = serde_json::from_value(v.get("window")?.clone()).ok()?;
    Some(w).filter(|w| w.iter().all(|v| v.is_finite()) && w[2] >= 600.0 && w[3] >= 400.0)
}

/// One Depth per user (`sys::lock_instance`): false when another one runs, after saying so in the
/// saved language; the caller then exits before it opens a window.
pub fn single_instance() -> bool {
    if depth::sys::lock_instance() { return true; }
    let v: Option<serde_json::Value> = settings_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    set_lang(v.as_ref().and_then(|v| v["lang"].as_str()).unwrap_or_default());
    depth::sys::notify(t("run.title"), t("run.body"));
    false
}

thread_local! { static DROPPED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }

/// A list that keeps the entries that parse (one an older build does not know is dropped, not the list).
pub(crate) fn lenient_vec<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let all: Vec<serde_json::Value> = serde::Deserialize::deserialize(d)?;
    let n = all.len();
    let ok: Vec<T> = all.into_iter().filter_map(|x| serde_json::from_value(x).ok()).collect();
    DROPPED.with(|c| c.set(c.get() + (n - ok.len()) as u32));
    Ok(ok)
}

/// A map whose entries that parse are kept (a venue key an older build does not know is dropped).
pub(crate) fn lenient_map<'de, D: serde::Deserializer<'de>, K: serde::de::DeserializeOwned + std::hash::Hash + Eq, V: serde::de::DeserializeOwned>(d: D) -> Result<HashMap<K, V>, D::Error> {
    let all: serde_json::Map<String, serde_json::Value> = serde::Deserialize::deserialize(d)?;
    let n = all.len();
    let ok: HashMap<K, V> = all.into_iter().filter_map(|(k, v)| Some((serde_json::from_value(k.into()).ok()?, serde_json::from_value(v).ok()?))).collect();
    DROPPED.with(|c| c.set(c.get() + (n - ok.len()) as u32));
    Ok(ok)
}

/// Lists per key (drawings per base asset), each list read with `lenient_vec`.
pub(crate) fn lenient_map_vec<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(d: D) -> Result<HashMap<String, Vec<T>>, D::Error> {
    let all: HashMap<String, serde_json::Value> = serde::Deserialize::deserialize(d)?;
    // a value that is not a list is lost too: count it as dropped
    let lost = || { DROPPED.with(|c| c.set(c.get() + 1)); vec![] };
    Ok(all.into_iter().map(|(k, v)| (k, lenient_vec(v).unwrap_or_else(|_| lost()))).collect())
}

/// settings.json read key by key: what this build cannot parse (a newer build's enum variant or
/// field type, a hand edit) costs only that value, never the whole file. `lossy` = something was
/// dropped or is unknown here, so saving would lose it (with_ctx keeps a settings.json.bad copy).
struct Doc { v: serde_json::Value, lossy: bool }

impl Doc {
    fn take(&mut self, k: &str) -> Option<serde_json::Value> { self.v.as_object_mut()?.remove(k).filter(|x| !x.is_null()) }
    fn keep<T, E: std::fmt::Display>(&mut self, k: &str, r: Result<T, E>) -> Option<T> {
        r.map_err(|e| { eprintln!("[settings] {k} ignored: {e}"); self.lossy = true; }).ok()
    }
    fn get<T: serde::de::DeserializeOwned>(&mut self, k: &str) -> Option<T> { let x = self.take(k)?; self.keep(k, serde_json::from_value(x)) }
    fn list<T: serde::de::DeserializeOwned>(&mut self, k: &str) -> Option<Vec<T>> { let x = self.take(k)?; self.keep(k, lenient_vec(x)) }
    /// A struct read key by key: a key `T` rejects keeps its default; keys `T` does not have are noted.
    fn obj<T: serde::Serialize + serde::de::DeserializeOwned + Default>(&mut self, k: &str) -> Option<T> {
        let x = self.take(k)?;
        let keys: Vec<String> = x.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let t = match T::deserialize(&x) {
            Ok(t) => t,
            Err(e) => {
                let mut base = serde_json::to_value(T::default()).ok()?;
                let Some(m) = x.as_object() else { return self.keep(k, Err(e)) };
                for (key, val) in m {
                    let Some(slot) = base.get_mut(key) else { continue };
                    let old = std::mem::replace(slot, val.clone());
                    if let Err(e) = T::deserialize(&base) { eprintln!("[settings] {k}.{key} ignored: {e}"); self.lossy = true; base[key] = old; }
                }
                T::deserialize(&base).ok()?
            }
        };
        let out = serde_json::to_value(&t).ok()?;
        if keys.iter().any(|key| out.get(key).is_none()) { eprintln!("[settings] {k} has keys this build does not know"); self.lossy = true; }
        Some(t)
    }
}

/// settings.json as text plus whether it exists but could not be read. A file that is not UTF-8
/// (UTF-16 from a Windows editor) comes through lossy, which `load_settings` flags.
fn read_settings(p: &std::path::Path) -> (String, bool) {
    match std::fs::read(p) {
        Ok(b) => (String::from_utf8_lossy(&b).into_owned(), false),
        Err(e) => (String::new(), e.kind() != std::io::ErrorKind::NotFound),
    }
}

/// Copy settings.json to settings.json.<ext>; false (said on stderr) when the copy failed.
fn keep_copy(p: &std::path::Path, ext: &str) -> bool {
    std::fs::copy(p, p.with_extension(format!("json.{ext}"))).map_err(|e| eprintln!("[settings] cannot keep a copy of {} as .{ext}: {e}", p.display())).is_ok()
}

/// Write settings.json (temp file + rename, so a crash cannot truncate it). The first write of a launch
/// keeps the file it replaces as settings.json.prev: nested fields this build does not know (a newer
/// build's) and lists it cannot read are dropped on load without any other trace.
fn write_settings(p: &std::path::Path, json: &str, prev_kept: &mut bool) -> bool {
    if !std::mem::replace(prev_kept, true) && p.exists() { keep_copy(p, "prev"); }
    let tmp = p.with_extension("json.tmp");
    p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, p).is_ok()
}

fn load_settings(text: &str) -> (Settings, bool) {
    DROPPED.with(|c| c.set(0));
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let mut d = Doc { lossy: !v.is_object() && !text.trim().is_empty(), v };
    let s = Settings {
        route: d.obj("route"), prefs: d.obj("prefs"), lang: d.get("lang"), base: d.get("base"), mode: d.get("mode"), chart: d.obj("chart"), layout: d.get("layout"), cell: d.get("cell"),
        charts: d.list("charts"), trade_ex: d.get("trade_ex"), off: d.list("off"),
        auto: d.take("auto").and_then(|x| { let r = lenient_map(x); d.keep("auto", r) }),
        order_panel: d.get("order_panel"), window: d.get("window"), book: d.obj("book"), risk_day: d.get("risk_day"), tpsl_wait: d.list("tpsl_wait"), hunter: d.obj("hunter"),
    };
    // keys left over belong to a newer build
    let unknown = d.v.as_object().is_some_and(|m| !m.is_empty());
    (s, d.lossy || unknown || DROPPED.with(|c| c.get()) > 0)
}

pub struct App {
    eng: engine::Engine,
    prefs: settings::Prefs,
    settings: settings::SettingsView,
    command: command::CommandBar,
    /// coin and market before the last switch (BACK goes there, and BACK again comes back)
    back: Option<(String, Market)>,
    /// the keys and commands sheet (HELP)
    help: bool,
    /// status for a Depth keyboard's lights (raw HID thread)
    kbd: kbd::Kbd,
    /// menu bar / notification area item (Settings > General)
    tray: Option<tray::Tray>,
    /// the tray popover while open: where (points), whether it has had focus (losing it closes it)
    pop: Option<(egui::Pos2, bool)>,
    /// last settings JSON written, and when it was last checked
    saved: (String, Instant),
    /// settings.json exists but could not be read at start: nothing is saved until it reads
    unreadable: bool,
    /// settings.json.prev written this launch (`write_settings`)
    prev_kept: bool,
    /// own window's position and size [x, y, w, h] (kept while maximized / full screen)
    window: Option<[f32; 4]>,
    mode: Market,
    chart: chart::Chart,
    book: book::BookView,
    opt: options::OptView,
    picker: picker::Picker,
    trade: trade::TradeView,
    spot: spot::SpotPanel,
    rate: (Instant, u64, f64),
    /// screenshot request from env: (path, after seconds)
    shot: Option<(String, f64)>,
    shot_sent: bool,
    started: Instant,
    last_frame: Instant,
    /// order routing policy (Settings > Trading), persisted
    route: depth::route::Policy,
    /// actions from the panels, applied at the next frame
    pending_base: Option<String>,
    pending_mode: Option<Market>,
    pending_restart: bool,
    /// chart layout ("1", "2", "2v", "3", "4"), the active cell (the toolbar drives `chart`, shown
    /// there) and the other cells' charts with their slot
    pub(crate) layout: String,
    cell: usize,
    others: Vec<(usize, chart::Chart)>,
    /// (symbol, account fills version) the chart's own fills were read from the db for
    fills_key: (String, u64),
    /// time of the newest trade log entry already toasted
    log_seen: i64,
    /// account.fill_seq already sounded
    fill_seen: u64,
    /// the panels' state document (when built, stale) and the order panel
    st: (std::sync::Arc<serde_json::Value>, Instant, bool),
    order: order::OrderPanel,
    /// trading hotkeys pressed this frame, run by the perp order panel
    hot_trade: Vec<hotkeys::Act>,
    /// a venue switched in the status bar, applied once the agg lock is free: (venue, on)
    venue_toggle: Option<(Exchange, bool)>,
    /// every listed coin's logo queued once the ticker list is known (theme::preload_coins)
    coins_preloaded: bool,
    /// order panel shown (Perp; Cmd-Opt-I or the header button), persisted
    order_panel: bool,
    /// header price flash; the book column's Venues / Quant pages
    top: top::TopBar,
    venue_pages: venues::VenuePages,
    account: account::AccountPanel,
    /// the Hunter page (title bar pill after the markets) and its rules, persisted
    hunter: hunter::HunterView,
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
    pub fn new(cc: &eframe::CreationContext) -> Self {
        // history of symbols not viewed for a month, old coin logos
        std::thread::spawn(|| depth::sys::prune_cache(30));
        theme::install(&cc.egui_ctx);
        #[cfg(feature = "hotpatch")]
        {
            let c = cc.egui_ctx.clone();
            subsecond::register_handler(std::sync::Arc::new(move || c.request_repaint()));
        }
        let env = |k: &str| std::env::var(k).ok();
        let (saved, unreadable) = settings_path().map_or((String::new(), false), |p| read_settings(&p));
        let (st, lossy) = load_settings(&saved);
        // part of the file was dropped (or is newer than this build): keep a copy before the first save rewrites it
        // (screenshot runs never write settings, and never save either)
        if (lossy || unreadable) && env("T1_SHOT").is_none() && let Some(p) = settings_path() && keep_copy(&p, "bad") { eprintln!("[settings] original kept in {}.bad", p.display()); }
        set_lang(&env("T1_LANG").or(st.lang.clone()).unwrap_or_default());
        let prefs = st.prefs.clone().unwrap_or_default();
        prefs.apply(&cc.egui_ctx);
        let mode = env("T1_MODE").and_then(|m| Market::parse(&m)).or(st.mode).unwrap_or(Market::Perp);
        // before the engine picks its active keys (T1_DEMO=1: demo account for screenshots)
        depth::demo::set_on(prefs.demo || env("T1_DEMO").is_some());
        let mut eng = engine::Engine::new();
        {
            let mut a = eng.account.lock().unwrap();
            a.auto = st.auto.clone().unwrap_or_default();
            a.limits = prefs.risk;
            if let Some((d, v)) = st.risk_day.clone() { a.day_start = (d, v.into_iter().collect()); }
            // checked against each venue after its first snapshot
            a.tpsl_wait = st.tpsl_wait.clone().unwrap_or_default().into_iter().map(|w| engine::TpSlWait { restored: true, ..w }).collect();
        }
        eng.start_account(&cc.egui_ctx);
        eng.off = st.off.unwrap_or_default().into_iter().collect();
        if eng.off.len() >= Exchange::ALL.len() { eng.off.clear(); }
        // the scanner first, so the picker's Binance stream rides on its connection from the start
        eng.set_hunter(prefs.hunter, &cc.egui_ctx);
        let mut hunter = st.hunter.unwrap_or_default();
        hunter.preset_once();
        hunter.open |=env("T1_MODE").as_deref() == Some("hunter");
        hunter.sync(&eng.hunt);
        eng.start(&env("T1_BASE").or(st.base).unwrap_or("BTC".into()), &cc.egui_ctx);
        if matches!(mode, Market::Margin | Market::Option) { eng.ensure(mode); }
        let mut chart = st.chart.unwrap_or_default();
        let mut trade = trade::TradeView::default();
        if let Some(ex) = st.trade_ex.filter(|e| depth::trade::TRADABLE.contains(e)) { trade.ex = ex; }
        // T1_ACC_TAB=<n>: open the account strip on tab n (screenshots)
        let mut picker = picker::Picker::default();
        picker.open = env("T1_PICK").is_some();
        // T1_BOOK_PANE=<0..4>: book, trades, venues, quant, L3 (screenshots)
        let mut book = st.book.unwrap_or_default();
        match env("T1_BOOK_PANE").and_then(|s| s.parse::<u8>().ok()) { Some(1) => book.trades = true, Some(p @ 2..=3) => book.page = p, _ => {} }
        if let Some(tf) = env("T1_TF").and_then(|s| s.parse().ok()) { chart.tf = tf; }
        let layout = env("T1_LAYOUT").or(st.layout).unwrap_or_else(|| "1".into());
        App {
            eng, prefs, settings: Default::default(), command: Default::default(), back: None, help: false, kbd: kbd::Kbd::start(), tray: None, pop: None, saved: (saved, Instant::now()), unreadable, prev_kept: false, window: st.window, mode, chart, book, opt: Default::default(), picker, trade, spot: Default::default(),
            rate: (Instant::now(), 0, 0.0),
            shot: env("T1_SHOT").map(|p| (p, env("T1_SHOT_AFTER").and_then(|s| s.parse().ok()).unwrap_or(30.0))),
            shot_sent: false, started: Instant::now(), last_frame: Instant::now(),
            route: st.route.clone().unwrap_or_default(), pending_base: None, pending_mode: None, pending_restart: false,
            layout, cell: st.cell.unwrap_or(0), others: st.charts.unwrap_or_default(), fills_key: Default::default(), log_seen: now_ms(), fill_seen: 0,
            st: (Default::default(), Instant::now(), true), order: Default::default(), hot_trade: vec![], venue_toggle: None, coins_preloaded: false, order_panel: st.order_panel.unwrap_or(true), top: Default::default(), venue_pages: Default::default(),
            account: Default::default(), hunter,
        }
    }

    fn header(&mut self, ui: &mut Ui, a: &Agg, st: &serde_json::Value, new_base: &mut Option<String>, new_mode: &mut Option<Market>) {
        // the header is the title bar: drag empty space to move, double-click to maximize
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        let bar = ui.interact(ui.max_rect(), egui::Id::new("titlebar"), egui::Sense::click_and_drag());
        if bar.drag_started_by(egui::PointerButton::Primary) { ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag); }
        if bar.double_clicked() { ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)); }
        // title bar, 46 px: lights, pair, market pills, headline price ... clock, panel toggle, settings
        ui.horizontal(|ui| {
            ui.set_height(46.0);
            ui.spacing_mut().item_spacing.x = 14.0;
            // macOS: traffic lights on the left; Windows / Linux get their buttons on the right
            if cfg!(target_os = "macos") { window_buttons(ui, maximized); }
            let tickers = self.eng.tickers.lock().unwrap().clone();
            if !self.coins_preloaded && !tickers.is_empty() {
                let mut by_vol: Vec<_> = tickers.iter().map(|t| (t.quote_vol, t.base.clone())).collect();
                by_vol.sort_by(|a, b| b.0.total_cmp(&a.0));
                theme::preload_coins(ui.ctx(), by_vol.into_iter().map(|(_, b)| b));
                self.coins_preloaded = true;
            }
            // the Hunter page is about every coin: no coin picker and no headline price there
            let hunting = self.hunter.open;
            if !hunting { if let Some(b) = self.picker.show(ui, &self.eng.base, &tickers) { if b != self.eng.base { *new_base = Some(b); } } }
            // the markets, then the Hunter page (not a market: the chart keeps its market under it)
            let names: Vec<&str> = MODES.iter().map(|(_, k)| t(k)).chain([t("mode.hunter")]).collect();
            let sel = if self.hunter.open { MODES.len() } else { MODES.iter().position(|(m, _)| *m == self.mode).unwrap_or(0) };
            match top::pills(ui, "modes", &names, sel, 26.0, 13.0) {
                Some(i) if i == MODES.len() => self.hunter.open = true,
                Some(i) => *new_mode = Some(MODES[i].0),
                None => {}
            }
            if depth::demo::on() { kit::chip(ui, "DEMO", kit::Tone::Warn).on_hover_text(l("Simulated account: no real order is sent")); }
            let book_market = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
            if !hunting { self.top.price(ui, a.mid(book_market)); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if !cfg!(target_os = "macos") { window_controls_right(ui, maximized); }
                if kit::icon_button(ui, 30.0, false, top::gear).on_hover_text(t("set.title")).clicked() { self.settings.open = true; }
                // show / hide the order panel; Perp and Spot
                let tip = l("Show or hide the order panel (Option-Command-I)");
                if matches!(self.mode, Market::Perp | Market::Spot) {
                    let on = self.order_panel;
                    if kit::icon_button(ui, 30.0, false, |p, r, c| top::panel_glyph(p, r, c, on)).on_hover_text(tip).clicked() { self.order_panel = !on; }
                } else {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    top::panel_glyph(ui.painter(), r, dim().gamma_multiply(0.6), false);
                }
                ui.add_space(6.0);
                top::TopBar::clock(ui);
                top::TopBar::divider(ui);
                top::TopBar::open_now(ui);
                top::TopBar::divider(ui);
                let fng = self.eng.fng.lock().unwrap().clone();
                self.top.panic(ui, a, fng);
            });
        });
        // stats strip, 44 px, hairline below; scrolls when the window is narrow
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add_space(5.0);
        egui::ScrollArea::horizontal().id_salt("hdr_stats").auto_shrink([false, true])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| ui.horizontal(|ui| {
                ui.add_space(4.0);
                if self.hunter.open { self.hunter.stats(ui, &self.eng.hunt, &self.eng.off); } else { top::TopBar::stats(ui, st, self.mode); }
            }));
        ui.add_space(6.0);
        let w = ui.ctx().content_rect().x_range();
        ui.painter().hline(w, ui.min_rect().bottom() + 0.5, egui::Stroke::new(1.0, line().gamma_multiply(0.7)));
    }
    /// Keyboard shortcuts (Settings > Hotkeys). Never while a
    /// text field has focus or a dialog (settings, order confirmation, ...) is open.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        use hotkeys::Act;
        // a trading key not taken by a panel last frame (none on screen) is dropped, never sent later
        self.hot_trade.clear();
        let binds = hotkeys::complete(&self.prefs.hotkeys);
        let key = |a: Act| binds.iter().find(|b| b.act == a).and_then(|b| b.key.as_ref()).map_or(String::new(), |c| c.label());
        self.picker.keys = [key(Act::Picker), key(Act::PrevFav), key(Act::NextFav), key(Act::Star)];
        if ctx.egui_wants_keyboard_input() || ctx.memory(|m| m.top_modal_layer().is_some()) { return; }
        let acts = hotkeys::pressed(ctx, &binds);
        if acts.contains(&Act::Command) { ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_)))); }
        // Bloomberg: typing anywhere starts the command line (letters and digits; the key's text is kept)
        else if !self.command.open {
            let typed = ctx.input_mut(|i| {
                let t = i.events.iter().find_map(|e| match e { egui::Event::Text(t) if t.chars().all(|c| c.is_ascii_alphanumeric()) => Some(t.clone()), _ => None });
                if t.is_some() { i.events.retain(|e| !matches!(e, egui::Event::Text(_))); }
                t
            });
            if let Some(t) = typed { self.command.open_with(&t); }
        }
        for a in acts {
            match a {
                Act::Settings => self.settings.open = true,
                Act::Command => self.command.open = true,
                Act::Help => self.help = true,
                Act::Back => if let Some((b, m)) = self.back.clone() {
                    if b != self.eng.base { self.pending_base = Some(b); }
                    if m != self.mode { self.pending_mode = Some(m); }
                },
                Act::OrderPanel => self.order_panel = !self.order_panel,
                // in every mode, never through the order panel; it always asks first
                Act::Kill => self.account.kill_prompt = true,
                Act::Picker | Act::PrevFav | Act::NextFav | Act::Star => self.picker.hot.push(a),
                Act::Spot | Act::Margin | Act::Perp | Act::Option => {
                    let m = match a { Act::Spot => Market::Spot, Act::Margin => Market::Margin, Act::Perp => Market::Perp, _ => Market::Option };
                    if self.mode != m { self.pending_mode = Some(m); }
                }
                Act::Tf1m => self.chart.tf = 1, Act::Tf5m => self.chart.tf = 5, Act::Tf15m => self.chart.tf = 15,
                Act::Tf1h => self.chart.tf = 60, Act::Tf4h => self.chart.tf = 240, Act::Tf1d => self.chart.tf = 1440,
                Act::PrevVenue | Act::NextVenue => {
                    let all = depth::trade::TRADABLE;
                    let i = all.iter().position(|e| *e == self.trade.ex).unwrap_or(0);
                    let n = all.len();
                    let e = all[(i + if a == Act::NextVenue { 1 } else { n - 1 }) % n];
                    let _ = self.call(ctx, &serde_json::json!({"op": "set_trade_venue", "ex": native::ex_name(e)}));
                }
                _ => self.hot_trade.push(a),
            }
        }
    }

    /// The menu bar / tray item: created when the setting is on (not in screenshot runs), fed the
    /// title and the position lines, and its clicks handled.
    fn tray_tick(&mut self, ctx: &egui::Context, a: &depth::agg::Agg) {
        // screenshot runs leave the menu bar alone unless T1_TRAY is set (to check the item itself)
        let want = self.prefs.tray && (std::env::var_os("T1_SHOT").is_none() || std::env::var_os("T1_TRAY").is_some());
        if !want { self.tray = None; return; }
        if self.tray.is_none() {
            self.tray = tray::Tray::new();
            if std::env::var_os("T1_TRAY").is_some() { eprintln!("[tray] created: {}", self.tray.is_some()); }
        }
        let Some(item) = self.tray.as_mut() else { return };
        let m = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
        let chg = self.eng.tickers.lock().unwrap().iter().find(|t| t.base == self.eng.base).map(|t| t.chg_pct);
        let sym = depth::trade::symbol(&self.eng.base);
        let acc = self.eng.account.lock().unwrap();
        let here: f64 = acc.positions.iter().filter(|p| p.symbol == sym).map(|p| p.upnl).sum();
        // short: a crowded menu bar pushes long items over the app menus. The change and the exact
        // figures are in the popover and the tooltip.
        let mut title = vec![(format!("{} {}", self.eng.base, a.mid(m).map(tray_px).unwrap_or("–".into())), None)];
        if acc.positions.iter().any(|p| p.symbol == sym) { title.push((format!(" {}{}", if here < 0.0 { "−" } else { "+" }, chart::fmt_usd_short(here.abs())), Some(if here < 0.0 { dn() } else { up() }))); }
        let mut lines: Vec<String> = acc.positions.iter().map(|p| {
            let roe = if p.margin > 0.0 { format!(" ({:+.1}%)", p.upnl / p.margin * 100.0) } else { String::new() };
            format!("{} {} {} · {}{roe}", p.symbol.trim_end_matches("USDT"), if p.side == depth::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(p.qty), order::signed(p.upnl))
        }).collect();
        if acc.positions.len() > 1 { lines.push(format!("uPnL {}", order::signed(acc.positions.iter().map(|p| p.upnl).sum()))); }
        drop(acc);
        if let Some(c) = chg { lines.insert(0, format!("{} {} {c:+.2}%", self.eng.base, a.mid(m).map(fmt_px).unwrap_or("–".into()))); }
        item.update(title.clone(), lines);
        let act = item.poll();
        // the popover's content: the coin, its last hour, every position
        let spark: Vec<f64> = a.series.get(&(None, m)).map(|s| s.bars.iter().rev().take(60).map(|b| b.c).collect::<Vec<_>>().into_iter().rev().collect()).unwrap_or_default();
        let acc = self.eng.account.lock().unwrap();
        let rows: Vec<(String, bool, f64, f64, Option<f64>)> = acc.positions.iter()
            .map(|p| (p.symbol.trim_end_matches("USDT").to_string(), p.side == depth::Side::Buy, p.qty, p.upnl, (p.margin > 0.0).then(|| p.upnl / p.margin * 100.0))).collect();
        drop(acc);
        const PW: f32 = 320.0;
        let ph = 212.0 + rows.len().min(6) as f32 * 40.0 + if rows.is_empty() { 0.0 } else { 34.0 };
        match act {
            Some(tray::TrayAct::Open) => self.raise(ctx),
            Some(tray::TrayAct::Quit) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Some(tray::TrayAct::Popover { x, y, w, h }) => {
                if self.pop.take().is_none() {
                    // under the menu bar item on macOS, over the taskbar icon on Windows
                    let ppp = ctx.native_pixels_per_point().unwrap_or(1.0) as f64;
                    let cx = ((x + w / 2.0) / ppp) as f32;
                    let top = if cfg!(target_os = "macos") { ((y + h) / ppp) as f32 + 6.0 } else { (y / ppp) as f32 - 6.0 - ph };
                    self.pop = Some((egui::pos2(cx - PW / 2.0, top), false));
                }
            }
            None => {}
        }
        // T1_POPOVER=x,y: open the popover there once, without taking focus (screenshot runs)
        let test = std::env::var("T1_POPOVER").ok().and_then(|v| v.split_once(',').and_then(|(x, y)| Some(egui::pos2(x.parse().ok()?, y.parse().ok()?))));
        if let (Some(p), None) = (test, self.pop) { self.pop = Some((p, false)); }
        let Some((pos, had_focus)) = self.pop else { return };
        let vb = egui::ViewportBuilder::default().with_title("Depth").with_position(pos).with_inner_size([PW, ph])
            .with_resizable(false).with_always_on_top().with_taskbar(false).with_active(test.is_none())
            // macOS: a real window with its titlebar hidden keeps the system's rounded corners and shadow
            .with_decorations(cfg!(target_os = "macos")).with_titlebar_shown(false).with_title_shown(false)
            .with_titlebar_buttons_shown(false).with_fullsize_content_view(true);
        let base = self.eng.base.clone();
        let (price, mkt) = (a.mid(m), self.mode);
        let mut out: Option<bool> = None; // Some(true): open Depth, Some(false): just close
        let mut focused = had_focus;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("tray_popover"), vb, |root, _| {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(popup_bg()).inner_margin(egui::Margin::symmetric(16, 14))).show(root, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{base}/USDT")).font(prop_s(13.5)).color(fg_strong()));
                    ui.label(RichText::new(t(MODES.iter().find(|x| x.0 == mkt).map_or("mode.perp", |x| x.1))).font(prop(12.0)).color(mu()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(c) = chg { kit::chip(ui, &format!("{c:+.2}%"), if c >= 0.0 { kit::Tone::Up } else { kit::Tone::Down }); }
                    });
                });
                let col = match chg { Some(c) if c < 0.0 => dn(), Some(_) => up(), None => fg_strong() };
                ui.label(RichText::new(price.map(fmt_px).unwrap_or("–".into())).font(display_s(28.0)).color(col));
                // the last hour as a line with a soft fill, coloured by its own direction
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
                if spark.len() > 1 {
                    let (lo, hi) = spark.iter().fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
                    let span = (hi - lo).max(hi.abs() * 1e-9).max(1e-12);
                    let pts: Vec<egui::Pos2> = spark.iter().enumerate().map(|(i, v)| egui::pos2(r.left() + r.width() * i as f32 / (spark.len() - 1) as f32, r.bottom() - 2.0 - (r.height() - 4.0) * ((v - lo) / span) as f32)).collect();
                    let c = if spark.last() >= spark.first() { up() } else { dn() };
                    let mut fill = pts.clone();
                    fill.push(egui::pos2(r.right(), r.bottom()));
                    fill.push(egui::pos2(r.left(), r.bottom()));
                    let mesh: Vec<egui::Shape> = pts.windows(2).map(|w| egui::Shape::convex_polygon(vec![w[0], w[1], egui::pos2(w[1].x, r.bottom()), egui::pos2(w[0].x, r.bottom())], c.gamma_multiply(0.10), egui::Stroke::NONE)).collect();
                    ui.painter().extend(mesh);
                    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.5, c)));
                }
                ui.label(RichText::new(t("tray.last_hour")).font(prop(10.5)).color(dim()));
                if !rows.is_empty() {
                    ui.add_space(4.0);
                    ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, line()));
                    ui.add_space(6.0);
                    for (sym, long, qty, upnl, roe) in rows.iter().take(6) {
                        let pc = if *upnl >= 0.0 { up() } else { dn() };
                        ui.horizontal(|ui| {
                            ui.set_min_height(34.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 1.0;
                                ui.label(RichText::new(sym).font(prop_s(13.0)).color(fg_strong()));
                                ui.label(RichText::new(format!("{} {}", if *long { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(*qty))).font(mono(11.0)).color(if *long { up() } else { dn() }));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                                        ui.label(RichText::new(order::signed(*upnl)).font(mono_s(13.0)).color(pc));
                                        if let Some(r) = roe { ui.label(RichText::new(format!("{r:+.2}%")).font(mono(11.0)).color(pc)); }
                                    });
                                });
                            });
                        });
                    }
                    let total: f64 = rows.iter().map(|r| r.3).sum();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("uPnL").font(prop(12.0)).color(mu()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(order::signed(total)).font(mono_s(13.5)).color(if total >= 0.0 { up() } else { dn() }));
                        });
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let w = (ui.available_width() - 8.0) / 2.0;
                    if kit::button(ui, t("tray.open"), kit::Look::Fill(accent()), kit::H, Some(w), true).clicked() { out = Some(true); }
                    if kit::button(ui, t("tray.quit"), kit::Look::Secondary, kit::H, Some(w), true).clicked() { ui.ctx().send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close); }
                });
            });
            let (esc, close, f) = root.ctx().input(|i| (i.key_pressed(egui::Key::Escape), i.viewport().close_requested(), i.viewport().focused));
            if esc || close { out = Some(false); }
            // like a popover: a click elsewhere (focus lost after it had it) closes it
            match f { Some(true) => focused = true, Some(false) if focused => { out.get_or_insert(false); } _ => {} }
        });
        match out {
            Some(open) => { self.pop = None; if open { self.raise(ctx); } }
            None => self.pop = Some((pos, focused)),
        }
    }

    /// Bring the main window up (from the tray).
    fn raise(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// What a Depth keyboard's lights show: live (messages arriving), a position in the current
    /// symbol (blinking past -10 % ROE), resting orders, a confirmation waiting or an order rejected
    /// in the last 10 s, the order panel's Close tab, the market and the chart interval.
    fn kbd_state(&self) -> kbd::State {
        let sym = depth::trade::symbol(&self.eng.base);
        let mut f = 0;
        if self.rate.2 > 0.0 && self.rate.0.elapsed() < Duration::from_secs(3) { f |= kbd::LIVE; }
        {
            let acc = self.eng.account.lock().unwrap();
            let ps: Vec<_> = acc.positions.iter().filter(|p| p.symbol == sym).collect();
            if !ps.is_empty() { f |= kbd::POS; }
            let (u, m) = ps.iter().fold((0.0, 0.0), |a, p| (a.0 + p.upnl, a.1 + p.margin));
            if m > 0.0 && u / m < -0.10 { f |= kbd::LOSS; }
            if acc.orders.iter().any(|o| o.symbol == sym) { f |= kbd::ORDERS; }
            if acc.log.back().is_some_and(|(ts, _, ok)| !ok && now_ms() - ts < 10_000) { f |= kbd::ATTN; }
        }
        if self.order.waiting_confirm() { f |= kbd::ATTN; }
        if self.order.closing() { f |= kbd::CLOSE; }
        let market = MODES.iter().position(|(m, _)| *m == self.mode).unwrap_or(0) as u8;
        let tf = [1, 5, 15, 60, 240, 1440].iter().position(|t| *t == self.chart.tf).map_or(0xFF, |i| i as u8);
        kbd::State { flags: f, market, tf }
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
            // metrics on the right (laid out right to left)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // language switch (rightmost, persisted)
                let cur = lang();
                let g = ui.painter().layout_no_wrap(LANGS[cur].1.to_string(), prop(11.5), mu());
                let (r, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 26.0, 20.0), egui::Sense::click());
                let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
                let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || open, 0.1);
                if hv > 0.0 { ui.painter().rect_filled(r, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
                let col = lerp(mu(), fg(), hv);
                ui.painter().galley(egui::pos2(r.left() + 7.0, r.center().y - g.size().y / 2.0), g, col);
                chevron(ui, egui::Rect::from_center_size(egui::pos2(r.right() - 9.0, r.center().y), egui::vec2(8.0, 8.0)), &ui.visuals().widgets.inactive, open);
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                egui::Popup::from_toggle_button_response(&resp).frame(top::popup_frame(ui, 4)).align(egui::RectAlign::TOP_END).show(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, (code, name)) in LANGS.iter().enumerate() {
                        // fixed width: available_width here is the whole status bar
                        let (r, row) = ui.allocate_exact_size(egui::vec2(160.0, 28.0), egui::Sense::click());
                        let on = i == cur;
                        if on || row.hovered() { ui.painter().rect_filled(r, kit::rad(), hl()); }
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
                // venues take what the metrics leave; too narrow for names: logo + latency only
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    let full: f32 = Exchange::ALL.iter().map(|e| ui.painter().layout_no_wrap(format!("{e:?}"), prop(11.5), mu()).size().x + 95.0).sum();
                    let names = ui.available_width() >= full;
                    let nowms = now_ms();
                    for e in Exchange::ALL {
                        let last = stats.venue.iter().filter(|((x, _), _)| *x == e).map(|(_, v)| v.1).max();
                        let alive = last.is_some_and(|l| nowms - l < 5_000);
                        let on = !self.eng.off.contains(&e);
                        let lat = stats.latency(e);
                        let lat_col = match lat { Some(l) if l < 150.0 => up(), Some(l) if l < 400.0 => WARN, Some(_) => dn(), None => dim() };
                        // Deribit streams BTC / ETH options only: with nothing to stream here it is left out, not shown as down
                        if e == Exchange::Deribit && !depth::ex::deribit::streams(self.mode, &self.eng.base) { continue; }
                        let resp = status_venue(ui, e, on, alive, lat, lat_col, names)
                            .on_hover_text(format!("{e:?}: {}", if on { t("status.scope_on") } else { t("status.scope_off") }));
                        // switching a venue off disconnects only that venue; on connects only it
                        // applied after the frame's agg lock is released (set_venue takes it): doing it here deadlocked
                        if resp.clicked() && (!on || self.eng.off.len() + 1 < Exchange::ALL.len()) { self.venue_toggle = Some((e, !on)); }
                    }
                });
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

impl eframe::App for App {
    /// the gutters between cards show the window clear color: the darkest base
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] { bg().to_normalized_gamma_f32() }

    /// Screenshot runs: `T1_HOVER=x,y` (points) parks the pointer there, so tooltips can be checked.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.shot.is_none() { return; }
        let Some((x, y)) = std::env::var("T1_HOVER").ok().and_then(|v| { let (a, b) = v.split_once(',')?; Some((a.parse().ok()?, b.parse().ok()?)) }) else { return };
        raw.events.push(egui::Event::PointerMoved(egui::pos2(x, y)));
    }

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
        let (auto, risk_day, tpsl_wait) = {
            let a = self.eng.account.lock().unwrap();
            let mut day: Vec<(Exchange, f64)> = a.day_start.1.iter().map(|(e, x)| (*e, *x)).collect();
            day.sort_by_key(|x| x.0 as u8);
            (a.auto.iter().map(|(e, r)| (format!("{e:?}"), *r)).collect(), (a.day_start.0, day), a.tpsl_wait.clone())
        };
        let s = SettingsRef { route: &self.route, prefs: &self.prefs, lang: lang_code(), base: &self.eng.base, mode: self.mode, chart: &self.chart, layout: &self.layout, cell: self.cell, charts: &self.others, trade_ex: self.trade.ex, off, auto, order_panel: self.order_panel, window: self.window, book: &self.book, risk_day, tpsl_wait, hunter: &self.hunter };
        let Ok(json) = serde_json::to_string_pretty(&s) else { return };
        if json == self.saved.0 { return; }
        let Some(p) = settings_path() else { return };
        // saving now would replace the real file with defaults: wait until it reads, and keep it as .bad first
        if self.unreadable {
            if !(std::fs::read(&p).is_ok() && keep_copy(&p, "bad")) { return; }
            self.unreadable = false;
        }
        let ok = write_settings(&p, &json, &mut self.prev_kept);
        if ok { self.saved.0 = json; } else { eprintln!("settings: cannot write {}", p.display()); }
    }

    /// The state document the panels read (`state_json`): rebuilt at most 4 times a second, or right
    /// after an op changed something.
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
        // demo positions and orders off screen keep a price for marks, triggers and liquidation
        if depth::demo::on() { bases.extend(depth::demo::bases()); bases.sort(); bases.dedup(); }
        bases.retain(|b| *b != base);
        self.eng.watch(&bases);
        let mut px = self.eng.watched.lock().unwrap().clone();
        if let Some(p) = self.chart.price(a, self.mode) { px.insert(base, p); }
        self.chart.check_alerts(&px);
        // the egui window has no notification API of its own: the OS notification through sys::notify
        for (title, body) in self.chart.fired.drain(..) { depth::sys::notify(&title, &body); }
    }

    /// Orders and triggers dragged on a chart, and prices put into the order panel from it.
    fn chart_actions(&mut self, ctx: &egui::Context) {
        let mut acts = std::mem::take(&mut self.chart.actions);
        for o in self.others.iter_mut() { acts.append(&mut o.1.actions); }
        for act in acts {
            match act {
                chart::ChartAct::Fill(px) => self.trade.fill = Some((self.trade.ex, px)),
                chart::ChartAct::Older(tf) => self.eng.ensure_older(tf as u32, ctx),
                chart::ChartAct::Position { entry, stop, target, qty } => {
                    // the whole order into the perp panel: limit at the entry, the size, TP at the target, SL at the
                    // stop. The last state document: the agg lock is held here, a fresh one would take it again
                    self.order_panel = true;
                    if self.mode != Market::Perp { self.pending_mode = Some(Market::Perp); }
                    let st = self.st.0.clone();
                    self.order.prefill_position(entry, stop, target, qty, &st);
                }
                chart::ChartAct::Move(chart::LineAct::Order(ex, id), px) => {
                    let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == ex && o.id == id).cloned();
                    if let Some(o) = o { self.eng.replace_order(&o, px, ctx); }
                }
                chart::ChartAct::Move(chart::LineAct::Preview(field), px) => self.order.set_preview(field, px),
                chart::ChartAct::Cancel(chart::LineAct::Order(ex, id)) => {
                    let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == ex && o.id == id).cloned();
                    if let Some(o) = o { self.eng.cancel(&o, ctx); }
                }
                chart::ChartAct::Cancel(chart::LineAct::TpSl(ex, id)) => {
                    let x = self.eng.account.lock().unwrap().tpsl.iter().find(|x| x.ex == ex && x.id == id).cloned();
                    if let Some(x) = x { self.eng.cancel_tpsl(&x, ctx); }
                }
                chart::ChartAct::Cancel(_) => {}
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
                c.fills = self.chart.fills.clone();
                (c.confirm, c.trading, c.equity, c.amends) = (self.chart.confirm, self.chart.trading, self.chart.equity, self.chart.amends);
                c.adopt_drawings(&self.chart);
                c.show(&mut child, a, self.mode, &base);
                self.chart.adopt_drawings(c);
            }
            let col = if k == self.cell { accent().linear_multiply(0.7) } else { line() };
            ui.painter().rect_stroke(*r, 4, egui::Stroke::new(1.0, col), egui::StrokeKind::Outside);
        }
    }

    fn frame(&mut self, ui: &mut Ui) {
        set_light(ui.ctx().theme() == egui::Theme::Light);
        // Frame cap: macOS stops vsync-throttling occluded / off-space windows, and then every
        // repaint request renders immediately, piling up GPU command buffers (GBs within
        // seconds). The cap bounds that regardless of vsync.
        // 120 fps (ProMotion) while focused, 30 fps in the background
        let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
        let budget = Duration::from_micros(if focused { 8_333 } else { 33_333 });
        let since = self.last_frame.elapsed();
        if since < budget { std::thread::sleep(budget - since); }
        self.last_frame = Instant::now();
        let ctx = ui.ctx().clone();
        // live data does not request repaints itself: this timer is the chart / book refresh rate.
        // 30 Hz while focused (10 Hz read as a stuttering, late candle), 10 Hz in the background.
        // The Hunter page shows no chart or book: once a second (its clock), the scanner repaints on new rows
        let hunting = self.hunter.open;
        ctx.request_repaint_after(Duration::from_millis(if hunting { 1000 } else if focused { 33 } else { 100 }));
        let scan = self.prefs.hunter;
        if self.eng.hunter_on() != scan { self.eng.set_hunter(scan, &ctx); }
        // alerts reach the OS only when Settings says so (the page's log always gets them)
        { let mut h = self.eng.hunt.lock().unwrap(); if h.notify != self.prefs.hunter_notify { h.notify = self.prefs.hunter_notify; } }
        // T1_SPIN: repaint every frame (diagnostics: worst case of constant mouse movement)
        if std::env::var("T1_SPIN").is_ok() { ctx.request_repaint(); }
        self.screenshot(&ctx);
        let (outer, inner, max, full) = ui.ctx().input(|i| { let v = i.viewport(); (v.outer_rect, v.inner_rect, v.maximized, v.fullscreen) });
        if let (Some(o), Some(r), false, false) = (outer, inner, max == Some(true), full == Some(true)) { self.window = Some([o.min.x, o.min.y, r.width(), r.height()]); }
        self.save_settings();
        self.kbd.set(self.kbd_state());
        self.resize_edges(ui);
        self.shortcuts(&ctx);
        let (mut new_base, mut new_mode, mut restart) = (None, None, false);
        // T1_CYCLE=<s>: switch base every <s> seconds (diagnostics for resubscription leaks)
        if let Some(every) = std::env::var("T1_CYCLE").ok().and_then(|s| s.parse::<u64>().ok()) {
            let n = self.started.elapsed().as_secs() / every;
            let want = ["BTC", "ETH", "SOL"][n as usize % 3];
            if self.eng.base != want { new_base = Some(want.to_string()); eprintln!("[cycle] -> {want}"); }
        }
        {
            // the header's stats come from the state document, built before the agg lock (it takes it itself)
            let hdr_st = self.state_doc();
            let agg = self.eng.agg.clone();
            let mut a = agg.lock().unwrap();
            let stats_arc = self.eng.stats.clone();
            let stats = stats_arc.lock().unwrap();
            // panels are rounded cards on the darker base: 8 px black gutters, a faint hairline edge
            let edge_c = line().gamma_multiply(0.8);
            // terminal style: no gutters, the cards' own hairlines (overlapping by one pixel) separate them
            let gap: i8 = if theme::term() { 0 } else { 4 };
            let card = |m: i8| Frame::new().fill(panel()).corner_radius(kit::rad()).stroke(egui::Stroke::new(1.0, if theme::term() { line() } else { edge_c }))
                .inner_margin(Margin::symmetric(m, 5)).outer_margin(Margin::same(gap));
            // title bar + stats strip, status bar: on the base, 14 px sides, 4 px towards the cards
            let flat = |top: i8, bottom: i8| Frame::new().fill(bg()).inner_margin(Margin { left: 14, right: 14, top, bottom });

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
            if let Some(p) = ap.insure.take() { self.trade.ins_pos = Some(p); }
            if self.trade.ins_pos.is_some() { let g = agg.lock().unwrap(); self.trade.insure_modal(&ctx, &g, &self.eng); }
            // trading hotkeys go to the order panel on screen; none shown (or options): dropped, never queued for later
            let hot = std::mem::take(&mut self.hot_trade);
            if self.mode == Market::Perp && self.order_panel && !hunting {
                let mut op = std::mem::take(&mut self.order);
                egui::Panel::right("order").show_separator_line(false).frame(Frame::new().fill(bg()).inner_margin(Margin::symmetric(4, 4))).resizable(false).exact_size(300.0)
                    .show(ui, |ui| egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                        op.show(ui, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r });
                        let usdt = self.prefs.hot_usdt;
                        for a in hot.iter().copied() { op.hotkey(a, usdt, &st, &mut |v| { let r = self.call(&ctx, &v); self.st.2 = true; r }); }
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
            if matches!(self.mode, Market::Spot | Market::Margin | Market::Option) && self.order_panel && !hunting {
                egui::Panel::right("spot_order").show_separator_line(false).frame(Frame::new().fill(bg()).inner_margin(Margin::symmetric(4, 4))).resizable(false).exact_size(300.0)
                    .show(ui, |ui| egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                        let g = agg.lock().unwrap();
                        let base = self.eng.base.clone();
                        if self.mode == Market::Option { self.opt.ticket(ui, &g, &self.eng, &base, self.prefs.confirm); }
                        else {
                            self.spot.show(ui, &g, &self.eng, &base, self.mode, self.trade.fill, self.prefs.confirm);
                            for a in hot.iter().copied() { self.spot.hotkey(a, self.prefs.hot_usdt, self.prefs.hot_loan, &g, &self.eng, &base, self.mode, self.prefs.confirm, ui.ctx()); }
                        }
                    }));
            }
            self.account = ap;
            a = agg.lock().unwrap();
            if self.mode != Market::Option && !hunting {
                let panel = egui::Panel::right("book").show_separator_line(false).frame(card(8)).resizable(true).default_size(320.0);
                let pane = self.book.pane();
                // the L3 page starts the exact order feeds (REST polls) only while someone looks at it
                if self.mode == Market::Perp && self.book.wants_l3() { self.eng.wants_l3.store(true, std::sync::atomic::Ordering::Relaxed); }
                if pane == 2 || pane == 3 {
                    // the venue_share op takes the agg lock itself
                    drop(a);
                    let st = self.state_doc();
                    let mut vp = std::mem::take(&mut self.venue_pages);
                    panel.show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        self.book.tabs(ui);
                        // the window picker row belongs to Venues only (Quant has no window)
                        if pane == 2 {
                            ui.horizontal(|ui| {
                                ui.set_height(26.0);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| vp.window_picker(ui));
                            });
                        }
                        ui.add_space(2.0);
                        if pane == 2 { vp.venues(ui, &mut |v| self.call(&ctx, &v)); } else { vp.quant(ui, &st["quant"]); }
                    });
                    self.venue_pages = vp;
                    a = agg.lock().unwrap();
                } else {
                    panel.show(ui, |ui| self.book.show(ui, &a, self.mode, &self.eng.venues().into_iter().collect(), &self.eng.base.clone()));
                }
                if let Some(c) = self.book.clicked.take() { self.trade.fill = Some(c); }
            }
            // positions and resting orders of the traded symbol, drawn on the chart
            {
                let sym = depth::trade::symbol(&self.eng.base);
                // own fills: re-read from the db when the symbol changes or new ones were stored
                // (backfill_fills locks the account itself: before `acc` below)
                let ver = self.eng.account.lock().unwrap().fills_ver;
                if self.mode == Market::Perp && (self.fills_key.0 != sym || self.fills_key.1 != ver) {
                    if self.fills_key.0 != sym { self.eng.backfill_fills(&sym, ui.ctx()); }
                    self.fills_key = (sym.clone(), ver);
                    self.chart.fills = depth::db::fills(&sym, now_ms() - 30 * 86_400_000);
                }
                let acc = self.eng.account.lock().unwrap();
                // venue answers (orders, cancels, moves) toast on the chart, a failure before a success;
                // the full line stays in Account > Order Log. A live fill plays a sound and toasts too.
                let fresh: Vec<_> = acc.log.iter().rev().take_while(|(ts, ..)| *ts > self.log_seen).collect();
                if let Some((_, m, ok)) = fresh.iter().find(|x| !x.2).or(fresh.first()) { self.chart.toast(order::log_toast(m, *ok), if *ok { up() } else { dn() }); }
                if let Some((ts, ..)) = acc.log.back() { self.log_seen = self.log_seen.max(*ts); }
                // fill_seq counts live pushes only (REST back-fills bypass it): every bump is a new fill
                if acc.fill_seq != self.fill_seen {
                    if let Some(f) = &acc.last_fill {
                        chart::play_sound(&self.prefs.fill_sound);
                        let px = f.price.map(|x| format!("@{}", fmt_px(x))).unwrap_or_default();
                        self.chart.toast(format!("{:?} {} {} {}{px}", f.ex, f.symbol, if f.buy { t("tr.buy") } else { t("tr.sell") }, fmt_qty(f.qty)), if f.buy { up() } else { dn() });
                    }
                    self.fill_seen = acc.fill_seq;
                }
                let mut lines: Vec<chart::ChartLine> = acc.positions.iter().filter(|p| p.symbol == sym).map(|p| chart::ChartLine {
                    price: p.entry, ex: Some(p.ex), col: if p.side == depth::Side::Buy { up() } else { dn() },
                    tag: if p.side == depth::Side::Buy { t("tr.long_s") } else { t("tr.short_s") }.to_string(),
                    sub: format!("{}@{} · {:+.2}", fmt_qty(p.qty), fmt_px(p.entry), p.upnl), act: None,
                }).collect();
                lines.extend(acc.orders.iter().filter(|o| o.symbol == sym && o.price > 0.0).map(|o| chart::ChartLine {
                    price: o.price, col: if o.side == depth::Side::Buy { up() } else { dn() }.linear_multiply(0.75),
                    // logo + size@price; the order type only when it is not a plain limit
                    ex: Some(o.ex), tag: if o.kind.eq_ignore_ascii_case("limit") { String::new() } else { o.kind.clone() },
                    sub: format!("{}{}@{}", if o.side == depth::Side::Buy { "+" } else { "-" }, fmt_qty(o.qty - o.filled), fmt_px(o.price)),
                    // only plain limit orders are dragged: a stop or post-only order would come back as something else
                    act: (o.kind.eq_ignore_ascii_case("limit") && matches!(o.tif.to_ascii_lowercase().as_str(), "" | "gtc" | "goodtillcancel")).then(|| chart::LineAct::Order(o.ex, o.id.clone())),
                }));
                // liquidation price of each position (none on Binance Portfolio Margin: account-level uniMMR)
                lines.extend(acc.positions.iter().filter(|p| p.symbol == sym).filter_map(|p| p.liq.filter(|l| *l > 0.0).map(|l| chart::ChartLine {
                    price: l, ex: Some(p.ex), col: theme::WARN,
                    tag: "Liq.".into(), sub: format!("@{}", fmt_px(l)), act: None,
                })));
                // take-profit / stop-loss triggers
                lines.extend(acc.tpsl.iter().filter(|x| x.symbol == sym).map(|x| chart::ChartLine {
                    price: x.trigger_px, ex: Some(x.ex), col: if x.take_profit { up() } else { dn() }.linear_multiply(0.8),
                    tag: if x.take_profit { "TP" } else { "SL" }.into(),
                    sub: format!("{}@{}", x.qty.map(fmt_qty).unwrap_or_else(|| t("tr.whole").to_string()), fmt_px(x.trigger_px)),
                    act: Some(chart::LineAct::TpSl(x.ex, x.id.clone())),
                }));
                // the order being typed in the panel (perp), draggable to change its price
                if self.mode == Market::Perp && self.order_panel {
                    lines.extend(self.order.preview.iter().map(|(px, field, tag, sub, col)| chart::ChartLine {
                        price: *px, ex: None, col: *col, tag: tag.to_string(), sub: sub.clone(), act: Some(chart::LineAct::Preview(field)),
                    }));
                }
                self.chart.lines = lines;
                self.chart.confirm = self.prefs.confirm;
                self.chart.trading = acc.keys.contains_key(&self.trade.ex) && acc.can_trade.get(&self.trade.ex) != Some(&false);
                self.chart.equity = acc.balances.get(&self.trade.ex).map(|b| b.equity).filter(|e| *e > 0.0);
                self.chart.amends = depth::trade::can_amend(self.trade.ex);
            }
            self.alerts_tick(&a);
            self.tray_tick(&ctx, &a);
            egui::CentralPanel::default().frame(card(10)).show(ui, |ui| {
                if hunting {
                    // a coin picked there (row or alert line) opens in Perpetual, on 5 minute bars with the OI and funding panes
                    if let Some(b) = self.hunter.show(ui, &self.eng.hunt, self.eng.hunter_on(), &self.eng.off) {
                        if b != self.eng.base { self.pending_base = Some(b); }
                        self.pending_mode = Some(Market::Perp);
                        self.chart.hunter_view();
                    }
                } else if self.mode == Market::Option { self.opt.show(ui, &a) } else { self.charts(ui, &a) }
            });
            self.chart_actions(&ctx);
        }
        if let Some(b) = self.pending_base.take() { new_base = Some(b); }
        if let Some(m) = self.pending_mode.take() { new_mode = Some(m); }
        if new_base.as_ref().is_some_and(|b| *b != self.eng.base) || new_mode.is_some_and(|m| m != self.mode) { self.back = Some((self.eng.base.clone(), self.mode)); }
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
        // a market chosen (pill, hotkey, command, a Hunter row) leaves the Hunter page
        if let Some(m) = new_mode { self.mode = m; self.eng.ensure(m); self.hunter.open = false; }
        // T1_SETTINGS=<tab>: open the settings dialog once (screenshots)
        if let Some(n) = std::env::var("T1_SETTINGS").ok().and_then(|s| s.parse().ok()) { if !self.shot_sent && self.started.elapsed().as_secs() < 2 { self.settings.open_tab(n); } }
        // T1_COMMAND=<text>: open the command bar with that text once (screenshots)
        if let Ok(t) = std::env::var("T1_COMMAND") { if !self.shot_sent && self.started.elapsed().as_secs() < 2 && !self.command.open { self.command.open_with(&t); } }
        command::help(&ctx, &mut self.help, &hotkeys::complete(&self.prefs.hotkeys));
        if self.command.open {
            let tickers = self.eng.tickers.lock().unwrap().clone();
            let binds = hotkeys::complete(&self.prefs.hotkeys);
            for c in self.command.show(&ctx, &tickers, &binds).unwrap_or_default() {
                match c {
                    command::Cmd::Base(b) => if b != self.eng.base { self.pending_base = Some(b) },
                    command::Cmd::Mode(m) => if m != self.mode { self.pending_mode = Some(m) },
                    command::Cmd::Tf(tf) => self.chart.tf = tf,
                    command::Cmd::Venue(e) => { let _ = self.call(&ctx, &serde_json::json!({"op": "set_trade_venue", "ex": native::ex_name(e)})); }
                    command::Cmd::Settings(n) => self.settings.open_tab(n),
                }
            }
        }
        if self.settings.open {
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
        self.book.show_tape = self.prefs.book_tape;
        if self.eng.l3_flowscan != self.prefs.hl_flowscan {
            self.eng.l3_flowscan = self.prefs.hl_flowscan;
            // a running Hyperliquid L3 feed switches source now
            self.eng.wants_l3.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // option panes on the chart need the chains too (ensure is a no-op once started)
        if self.eng.wants_options.swap(false, std::sync::atomic::Ordering::Relaxed) || self.chart.wants_options { self.eng.ensure(Market::Option); }
        if self.eng.wants_l3.swap(false, std::sync::atomic::Ordering::Relaxed) { self.eng.ensure_l3(); }
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

/// Windows / Linux minimize, maximize (restore when maximized) and close, flush with the window's
/// top right corner (the window has no native title bar). Reserves their width in the header's
/// right-to-left row so the items there stop short of them.
fn window_controls_right(ui: &mut Ui, maximized: bool) {
    let win = ui.ctx().content_rect();
    ui.add_space((3.0 * 46.0 - (win.right() - ui.max_rect().right())).max(0.0));
    let cmds = [egui::ViewportCommand::Minimized(true), egui::ViewportCommand::Maximized(!maximized), egui::ViewportCommand::Close];
    for (i, cmd) in cmds.into_iter().enumerate() {
        let r = egui::Rect::from_min_size(egui::pos2(win.right() - 46.0 * (3 - i) as f32, win.top()), egui::vec2(46.0, 30.0));
        let resp = ui.interact(r, ui.id().with(("winctl", i)), egui::Sense::click());
        let red = i == 2 && resp.hovered();
        if resp.hovered() { ui.painter().rect_filled(r, 0, if red { Color32::from_rgb(0xe8, 0x11, 0x23) } else { hl() }); }
        let (c, s, p) = (r.center(), egui::Stroke::new(1.0, if red { Color32::WHITE } else { fg() }), ui.painter());
        match i {
            0 => { p.hline(c.x - 5.0..=c.x + 5.0, c.y, s); }
            1 if maximized => {
                p.rect_stroke(egui::Rect::from_min_size(c + egui::vec2(-5.0, -2.0), egui::vec2(7.0, 7.0)), 0, s, egui::StrokeKind::Inside);
                p.line_segment([c + egui::vec2(-2.0, -2.0), c + egui::vec2(-2.0, -5.0)], s);
                p.line_segment([c + egui::vec2(-2.0, -5.0), c + egui::vec2(5.0, -5.0)], s);
                p.line_segment([c + egui::vec2(5.0, -5.0), c + egui::vec2(5.0, 2.0)], s);
                p.line_segment([c + egui::vec2(5.0, 2.0), c + egui::vec2(2.0, 2.0)], s);
            }
            1 => { p.rect_stroke(egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0)), 0, s, egui::StrokeKind::Inside); }
            _ => { p.line_segment([c - egui::vec2(5.0, 5.0), c + egui::vec2(5.0, 5.0)], s); p.line_segment([c + egui::vec2(-5.0, 5.0), c + egui::vec2(5.0, -5.0)], s); }
        }
        if resp.clicked() { ui.ctx().send_viewport_cmd(cmd); }
    }
}

/// Status bar venue: state dot (green streaming, red silent, grey switched off), name, push latency.
#[allow(clippy::too_many_arguments)]
fn status_venue(ui: &mut Ui, e: Exchange, on: bool, alive: bool, lat: Option<f64>, lat_col: Color32, names: bool) -> egui::Response {
    let name = ui.painter().layout_no_wrap(if names { format!("{e:?}") } else { String::new() }, prop(11.5), if on { mu() } else { dim() });
    let ms_col = if !on { dim() } else if alive { lat_col } else { dn() };
    let ms = ui.painter().layout_no_wrap(if on { lat.map(|l| format!("{l:.0}ms")).unwrap_or("–".into()) } else { "off".into() }, mono(11.5), ms_col);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(30.0 + name.size().x + 5.0 + ms.size().x, 18.0), egui::Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    if hv > 0.0 { p.rect_filled(rect.expand2(egui::vec2(5.0, 1.0)), 3, hl().gamma_multiply(0.7 * hv)); }
    let y = rect.center().y;
    p.circle_filled(egui::pos2(rect.left() + 3.0, y), 3.0, if !on { dim() } else if alive { up() } else { dn() });
    theme::icon(p, e, egui::Rect::from_center_size(egui::pos2(rect.left() + 18.0, y), egui::vec2(12.0, 12.0)), !on);
    let x = rect.left() + 30.0;
    p.galley(egui::pos2(x, y - name.size().y / 2.0), name.clone(), mu());
    p.galley(egui::pos2(x + name.size().x + 5.0, y - ms.size().y / 2.0), ms, mu());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_prints_unsigned() {
        assert_eq!(fmt_dp(-0.0, 2), "0.00");
        assert_eq!(fmt_dp(-0.001, 2), "0.00");
        assert_eq!(fmt_dp(-0.01, 2), "-0.01");
        assert_eq!(fmt_dp(-1234.5, 1), "-1,234.5");
        assert_eq!(crate::order::signed(-0.0), "+0.00");
        assert_eq!(crate::order::signed(-0.004), "+0.00");
        assert_eq!(crate::order::signed(-2.0), "-2.00");
    }
    #[test]
    fn number_formats() {
        assert_eq!([2.0, 1.0, 0.5, 0.1, 0.004, 0.00001].map(step_dp), [0, 0, 1, 1, 3, 5]);
        assert_eq!(fmt_dp(86256.0, 0), "86,256");
        assert_eq!(fmt_dp(-1234567.891, 2), "-1,234,567.89");
        assert_eq!(fmt_px(120.204), "120.20");
        assert_eq!(fmt_px(0.10872), "0.108720");
    }

    #[test]
    fn settings_load_keeps_what_parses() {
        use serde_json::{json, to_value};
        let hk = to_value(&hotkeys::defaults()[0]).unwrap();
        let line = json!({"tool": "HLine", "a": [0, 100.0], "b": [0, 100.0]});
        let text = json!({
            "mode": "Futures", "base": "ETH", "off": ["Bybit", "Nope"], "window": [1.0, 2.0, 900.0, 700.0],
            "prefs": {"theme": "neon", "radius": 5, "hotkeys": [hk, {"act": "Nope", "key": null}]},
            "chart": {"drawings": {"BTC": [line, {"tool": "bogus", "a": [0, 1.0], "b": [0, 1.0]}]}},
        }).to_string();
        let (s, lossy) = load_settings(&text);
        assert!(lossy && s.mode.is_none());
        assert_eq!((s.base.as_deref(), s.off, s.window), (Some("ETH"), Some(vec![Exchange::Bybit]), Some([1.0, 2.0, 900.0, 700.0])));
        // the bad theme falls back to its default, the other prefs and the good list entries stay
        let p = s.prefs.unwrap();
        assert!(p.theme == settings::ThemePref::System && p.radius == 5 && p.hotkeys.len() == 1);
        assert_eq!(to_value(s.chart.unwrap()).unwrap()["drawings"]["BTC"].as_array().map(|a| a.len()), Some(1));
        // a file this build wrote itself loses nothing
        let ok = json!({
            "prefs": to_value(settings::Prefs::default()).unwrap(), "chart": to_value(chart::Chart::default()).unwrap(), "book": to_value(book::BookView::default()).unwrap(),
            "route": to_value(depth::route::Policy::default()).unwrap(), "off": ["Okx"], "charts": [], "auto": {"Bybit": {"enabled": true, "min": 1.0, "target": 2.0}}, "mode": "Perp",
        }).to_string();
        let (s, lossy) = load_settings(&ok);
        assert!(!lossy && s.prefs.is_some() && s.chart.is_some() && s.book.is_some() && s.route.is_some() && s.auto.is_some_and(|a| a.len() == 1));
        // a key only a newer build writes, broken JSON: flagged; no file yet: not
        assert!(load_settings(r#"{"base": "BTC", "from_the_future": 1}"#).1 && load_settings("{oops").1);
        assert!(!load_settings("").1);
        // drawings that are not a list lose them: flagged
        assert!(load_settings(r#"{"chart": {"drawings": {"BTC": "oops"}}}"#).1);
    }

    #[test]
    fn first_write_keeps_the_replaced_file_as_prev() {
        let dir = std::env::temp_dir().join(format!("depth-settings-prev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (p, prev) = (dir.join("settings.json"), dir.join("settings.json.prev"));
        let mut kept = false;
        // no file yet: nothing to keep
        assert!(write_settings(&p, "first", &mut kept) && !prev.exists());
        kept = false;
        std::fs::write(&p, "from a newer build").unwrap();
        assert!(write_settings(&p, "one", &mut kept) && write_settings(&p, "two", &mut kept));
        // later writes of the launch leave .prev alone
        assert_eq!((std::fs::read_to_string(&p).unwrap(), std::fs::read_to_string(&prev).unwrap()), ("two".into(), "from a newer build".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_file_is_never_read_as_empty() {
        let dir = std::env::temp_dir().join(format!("depth-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        assert_eq!(read_settings(&p), (String::new(), false));
        // UTF-16 from a Windows editor is not UTF-8: it reads lossy and is flagged, not taken for "no file"
        let utf16: Vec<u8> = "{\"base\": \"ETH\"}".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        std::fs::write(&p, &utf16).unwrap();
        let (text, unreadable) = read_settings(&p);
        assert!(!unreadable && !text.is_empty() && load_settings(&text).1);
        assert!(keep_copy(&p, "bad") && std::fs::read(dir.join("settings.json.bad")).unwrap() == utf16);
        // exists but cannot be read (a folder here): flagged; a copy that fails says so
        assert_eq!(read_settings(&dir), (String::new(), true));
        assert!(!keep_copy(&dir.join("nope.json"), "bad"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
