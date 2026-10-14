//! Candle chart painted with egui: heatmap background, candles, liquidation bubbles, and
//! stacked sub-panes (volume, CVD, OI, funding predicted + settled, basis, long/short).
use super::{fmt_dp, fmt_px, kit, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui};
use depth::agg::{Agg, Bar, Heatmap};
use depth::{Exchange, Market};

const AXIS_W: f32 = 74.0;
const TIME_H: f32 = 20.0;
const SUB_H: f32 = 70.0;
/// Every chart interval (minutes); the first six sit in the toolbar, the rest in its menu.
const TFS: [(i64, &str); 12] = [(1, "1m"), (5, "5m"), (15, "15m"), (60, "1H"), (240, "4H"), (1440, "1D"),
    (3, "3m"), (30, "30m"), (120, "2H"), (360, "6H"), (720, "12H"), (10080, "1W")];
const TF_QUICK: usize = 6;

/// The venue-native history interval an interval is built from (venues serve 1/5/15/60/240m;
/// everything else is resampled from the largest of those that divides it).
pub fn native_tf(tf: i64) -> i64 { [240, 60, 15, 5, 1].into_iter().find(|n| tf % n == 0).unwrap_or(1) }

/// Bucket start for an interval: weeks start on Monday (the epoch was a Thursday), the rest on
/// UTC multiples of the interval.
fn bucket(t: i64, tf: i64) -> i64 {
    let ms = tf * 60_000;
    let off = if tf == 10080 { 4 * 86_400_000 } else { 0 };
    (t - off).div_euclid(ms) * ms + off
}

#[derive(Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
enum Scale { #[default] Linear, Log, Percent }

/// Price <-> y on the main plot; log scale maps ln(price).
#[derive(Clone, Copy)]
struct PScale { lo: f64, hi: f64, log: bool, r: Rect }
impl PScale {
    fn f(&self, v: f64) -> f64 { if self.log { v.max(1e-12).ln() } else { v } }
    fn y(&self, v: f64) -> f32 { let (a, b) = (self.f(self.lo), self.f(self.hi)); self.r.bottom() - ((self.f(v) - a) / (b - a)) as f32 * self.r.height() }
    fn v(&self, y: f32) -> f64 {
        let (a, b) = (self.f(self.lo), self.f(self.hi));
        let x = a + (self.r.bottom() - y) as f64 / self.r.height() as f64 * (b - a);
        if self.log { x.exp() } else { x }
    }
}

/// Native-period history up to the first full live bucket, then bars resampled from the live
/// 1m series; history CVD is shifted to meet the live curve.
fn stitch(hist: &[Bar], live: Vec<Bar>, first_live_1m: i64, tf: i64) -> Vec<Bar> {
    if hist.is_empty() { return live; }
    // first bucket the live series covers completely
    let b = bucket(first_live_1m, tf);
    let cut = if b == first_live_1m { b } else { b + tf * 60_000 };
    let live: Vec<Bar> = live.into_iter().filter(|b| b.t >= cut).collect();
    let mut out: Vec<Bar> = hist.iter().filter(|b| b.t < cut).cloned().collect();
    if let (Some(h), Some(l)) = (out.last(), live.first()) {
        let off = (l.cvd - (l.buy - l.sell)) - h.cvd;
        for b in &mut out { b.cvd += off; }
    }
    out.extend(live);
    out
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum Pane { Vol, Cvd, Oi, Funding, Basis, Ls, Premium, Ofi, BookFlow, Rsi, Macd, Delta }

impl Pane {
    fn key(self) -> &'static str {
        match self {
            Pane::Vol => "chart.vol", Pane::Cvd => "chart.cvd", Pane::Oi => "chart.oi",
            Pane::Funding => "chart.funding", Pane::Basis => "chart.basis", Pane::Ls => "chart.ls",
            Pane::Premium => "chart.premium", Pane::Ofi => "chart.ofi", Pane::BookFlow => "chart.bookflow",
            Pane::Rsi => "chart.rsi", Pane::Macd => "chart.macd", Pane::Delta => "chart.delta",
        }
    }
    fn perp_only(self) -> bool { matches!(self, Pane::Oi | Pane::Funding | Pane::Basis | Pane::Ls) }
}

/// Heatmap aggregated per chart bar: (bar start, first bin index, max base qty per bin).
#[derive(Default)]
struct HeatCache {
    tf: i64, market: Option<Market>, last_ts: i64, bin: f64,
    /// per bar: (open time, lowest bin, summed qty per bin, samples); drawn as the time-weighted
    /// mean, so liquidity that was pulled fades instead of staying lit for the whole bar
    bars: Vec<(i64, i64, Vec<f32>, u32)>,
    /// the latest column: the live bar shows the book as it is now, not what it was earlier
    cur: Vec<(i64, f32)>,
    /// bumped whenever `bars` changes; the GPU texture is rebuilt only then (about once a second)
    ver: u64,
    tex: Option<(u64, egui::TextureHandle, TexGeom)>,
}

/// Where the heatmap texture sits: first bar time, bar width (ms), lowest bin, bin count.
#[derive(Clone, Copy)]
struct TexGeom { t0: i64, ms: i64, lo: i64, bins: usize, cols: usize }

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum Tool { HLine, Trend, Ray, Fib, Rect, Channel, Text, Measure, Position }

impl Tool {
    const ALL: [Tool; 9] = [Tool::HLine, Tool::Trend, Tool::Ray, Tool::Fib, Tool::Rect, Tool::Channel, Tool::Text, Tool::Measure, Tool::Position];
    fn id(self) -> &'static str {
        match self {
            Tool::HLine => "hline", Tool::Trend => "trend", Tool::Ray => "ray", Tool::Fib => "fib", Tool::Rect => "rect",
            Tool::Channel => "channel", Tool::Text => "text", Tool::Measure => "measure", Tool::Position => "position",
        }
    }
    fn parse(s: &str) -> Option<Tool> { Tool::ALL.into_iter().find(|t| t.id() == s) }
    /// clicks needed to place it
    fn points(self) -> usize { match self { Tool::HLine | Tool::Text => 1, Tool::Channel => 3, _ => 2 } }
}

/// `hot` part of a locked drawing: right-click opens its menu, nothing moves it.
const LOCKED: u8 = 9;

/// What the right-click menu is for: a drawing (index, focus its text field), or the empty plot (point).
#[derive(Clone, Copy)]
enum Menu { Drawing(usize, bool), Chart((i64, f64)) }

/// Bar replay: bars after `at` are hidden (None = picking the start bar); while playing it steps
/// `SPEEDS[speed]` bars per second.
#[derive(Clone, Copy)]
struct Replay { at: Option<i64>, playing: bool, speed: usize, last: f64 }
const SPEEDS: [f64; 4] = [1.0, 3.0, 10.0, 30.0];
impl Replay { fn new(at: Option<i64>, now: f64) -> Self { Replay { at, playing: false, speed: 0, last: now } } }

/// Indicator inputs (TradingView defaults).
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Ind { pub rsi: usize, pub macd: [usize; 3], pub bb_n: usize, pub bb_k: f64 }
impl Default for Ind { fn default() -> Self { Ind { rsi: 14, macd: [12, 26, 9], bb_n: 20, bb_k: 2.0 } } }

/// A price line from the account: entry of a position or a resting order.
/// Drawn dashed with a tag at the left edge ("tag | sub") and the price on the axis.
#[derive(Clone)]
pub struct ChartLine { pub price: f64, pub tag: String, pub sub: String, pub col: Color32, pub act: Option<LineAct> }

/// What dragging an account line changes: a resting limit order, or a TP / SL trigger (venue, id).
#[derive(Clone, PartialEq, Debug)]
pub enum LineAct { Order(Exchange, String), TpSl(Exchange, String) }

/// Requests from the chart to the app: move an order / trigger, or put a price into the order panel.
#[derive(Clone, Debug)]
pub enum ChartAct { Move(LineAct, f64), Fill(f64) }

/// One moving average line: period, color (0xRRGGBB), exponential instead of simple, shown.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MaLine { pub n: usize, pub col: u32, pub ema: bool, pub on: bool }

fn default_mas() -> Vec<MaLine> {
    vec![MaLine { n: 7, col: 0xf0b90b, ema: false, on: true }, MaLine { n: 25, col: 0xe05bc4, ema: false, on: true }, MaLine { n: 99, col: 0x9b8cf0, ema: false, on: true }]
}

fn rgb(c: u32) -> Color32 { Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8) }

/// Exponential moving average (seeded with the SMA of the first n values, as TradingView does).
fn ema(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let k = 2.0 / (n as f64 + 1.0);
    let mut prev: Option<f64> = None;
    vals.iter().enumerate().map(|(i, v)| {
        prev = match prev {
            Some(p) => Some(p + k * (v - p)),
            None if i + 1 >= n => Some(vals[i + 1 - n..=i].iter().sum::<f64>() / n as f64),
            None => None,
        };
        prev
    }).collect()
}

/// RSI with Wilder smoothing, seeded with the plain average of the first n changes (TradingView's ta.rsi).
fn rsi(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let (mut up, mut dn) = (0.0, 0.0);
    let k = n as f64;
    (0..vals.len()).map(|i| {
        if i == 0 { return None; }
        let d = vals[i] - vals[i - 1];
        let (u, w) = (d.max(0.0), (-d).max(0.0));
        if i <= n { up += u / k; dn += w / k; } else { up = (up * (k - 1.0) + u) / k; dn = (dn * (k - 1.0) + w) / k; }
        (i >= n).then(|| if dn == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + up / dn) })
    }).collect()
}

/// MACD line, signal and histogram (fast, slow, signal EMAs); the signal starts once the MACD has n values.
fn macd(vals: &[f64], fast: usize, slow: usize, sig: usize) -> Vec<Option<(f64, f64, f64)>> {
    let (f, s) = (ema(vals, fast), ema(vals, slow));
    let line: Vec<Option<f64>> = f.iter().zip(&s).map(|(a, b)| Some((*a)? - (*b)?)).collect();
    let start = line.iter().position(Option::is_some).unwrap_or(vals.len());
    let defined: Vec<f64> = line[start..].iter().map(|v| v.unwrap_or(0.0)).collect();
    let sg = ema(&defined, sig);
    (0..vals.len()).map(|i| { let m = line[i]?; let g = sg[i.checked_sub(start)?]?; Some((m, g, m - g)) }).collect()
}

/// Bollinger bands: (middle SMA, half width = k population standard deviations).
fn bollinger(vals: &[f64], n: usize, k: f64) -> Vec<Option<(f64, f64)>> {
    (0..vals.len()).map(|i| {
        let w = vals.get((i + 1).checked_sub(n)?..=i)?;
        let m = w.iter().sum::<f64>() / n as f64;
        Some((m, k * (w.iter().map(|v| (v - m).powi(2)).sum::<f64>() / n as f64).sqrt()))
    }).collect()
}

/// Fibonacci retracement levels drawn between a drawing's two points (0 at the second point).
const FIB: [f64; 7] = [0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0];

/// Simple moving average of `vals` over `n` (None until n values are in).
/// 1.2K / 3.4M / 1.1B for wall sizes.
fn fmt_usd_short(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.0}") }
}

fn sma(vals: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(vals.len());
    let mut sum = 0.0;
    for (i, v) in vals.iter().enumerate() {
        sum += v;
        if i >= n { sum -= vals[i - n]; }
        out.push((i + 1 >= n).then_some(sum / n as f64));
    }
    out
}

/// Time left until a bar closes: "mm:ss", or "h:mm:ss" from an hour on.
fn countdown(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

fn dashed(painter: &egui::Painter, x0: f32, x1: f32, y: f32, col: Color32) {
    painter.extend(egui::Shape::dashed_line(&[pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, col), 4.0, 3.0));
}

/// Grid line, TradingView-style: 1 px dots, faint (dashes, not circles: cheap to tessellate).
fn grid_line(painter: &egui::Painter, a: Pos2, b: Pos2) {
    painter.extend(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, line()), 1.5, 2.5));
}

/// Axis label color for ordinary levels and times (round price levels use `mu()`).
fn axis_c() -> Color32 { lerp(dim(), mu(), 0.5) }

/// A user drawing anchored in (bar time ms, price), so it survives timeframe changes.
/// `col` 0xRRGGBB (0 = the accent), `dash` 0 solid / 1 dashed / 2 dotted; `c` is a channel's third
/// point (on its parallel line), `text` a note's text; `alert` = armed price alert with this sound.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Drawing {
    tool: Tool, a: (i64, f64), b: (i64, f64),
    #[serde(default)] col: u32, #[serde(default)] dash: u8,
    #[serde(default)] c: (i64, f64), #[serde(default)] text: String, #[serde(default)] locked: bool,
    #[serde(default)] alert: Option<String>,
}

impl Drawing {
    fn new(tool: Tool, a: (i64, f64), b: (i64, f64)) -> Self { Drawing { tool, a, b, col: 0, dash: 0, c: a, text: String::new(), locked: false, alert: None } }
    fn color(&self) -> Color32 { if self.col == 0 { accent() } else { rgb(self.col) } }
    /// the a-b line's price at time t (None when a and b share a time)
    fn line_at(&self, t: i64) -> Option<f64> {
        let (ta, tb) = (self.a.0, self.b.0);
        (ta != tb).then(|| self.a.1 + (self.b.1 - self.a.1) * (t - ta) as f64 / (tb - ta) as f64)
    }
    /// a channel's parallel line, as a price offset from the a-b line
    fn offset(&self) -> f64 { self.c.1 - self.line_at(self.c.0).unwrap_or(self.a.1) }
    /// third point stored halfway along the parallel line, so its handle sits there
    fn with_c(mut self, c: (i64, f64)) -> Self {
        let off = c.1 - self.line_at(c.0).unwrap_or(self.a.1);
        let mt = (self.a.0 + self.b.0) / 2;
        self.c = (mt, self.line_at(mt).unwrap_or(self.a.1) + off);
        self
    }
    /// A drawing from its placed points: a channel's third point goes on its parallel line, a
    /// position tool (entry, stop) gets a 2R target.
    fn placed(tool: Tool, pts: &[(i64, f64)]) -> Self {
        let mut d = Drawing::new(tool, pts[0], *pts.get(1).unwrap_or(&pts[0]));
        if let Some(c) = pts.get(2) { d = d.with_c(*c); }
        if tool == Tool::Position { d.c = (d.a.0, d.a.1 * 3.0 - d.b.1 * 2.0); }
        d
    }
    fn alertable(&self) -> bool { matches!(self.tool, Tool::HLine | Tool::Trend | Tool::Ray) }
    /// the line's price at time t, for alerts: a trend line only within its two points, a ray from its start on
    fn level_at(&self, t: i64) -> Option<f64> {
        match self.tool {
            Tool::HLine => Some(self.a.1),
            Tool::Trend if t >= self.a.0.min(self.b.0) && t <= self.a.0.max(self.b.0) => self.line_at(t),
            Tool::Ray if (t - self.a.0).signum() * (self.b.0 - self.a.0).signum() >= 0 => self.line_at(t),
            _ => None,
        }
    }
}

/// Swatches of the drawing context menu (TradingView's default palette row).
const DRAW_COLORS: [u32; 8] = [0x2962ff, 0x089981, 0xf23645, 0xff9800, 0xffeb3b, 0x9c27b0, 0x00bcd4, 0x787b86];

/// Persisted in settings.json (see ui::Settings); runtime-only fields are skipped.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Chart {
    pub tf: i64,
    pub src: Option<Exchange>,
    heat: bool,
    /// estimated liquidation map, drawn as a profile on the right edge (perp)
    liqmap: bool,
    panes: Vec<(Pane, bool)>,
    bar_w: f32,
    /// drawings per base asset
    drawings: std::collections::HashMap<String, Vec<Drawing>>,
    /// moving averages on the price chart (master toggle, then each line)
    ma: bool,
    mas: Vec<MaLine>,
    /// magnet: the crosshair's price snaps to the hovered bar's open / high / low / close
    magnet: bool,
    /// auto support / resistance from clustered swing highs and lows
    sr: bool,
    /// large resting orders in the aggregated book (liquidity walls)
    walls: bool,
    /// perp mark price line (the price liquidations and unrealized PnL use)
    mark: bool,
    /// closes through the value area edges or a high-volume node in the last bars
    breakouts: bool,
    /// session VWAP with sigma bands
    vwap: bool,
    /// Bollinger bands on the close
    bb: bool,
    /// footprint: traded volume per price inside each bar (text cells when zoomed in, a mini profile otherwise)
    fp: bool,
    /// footprint style: 0 = cluster bars (the cells replace the candle body), 1 = side clusters beside a full candle
    fp_style: u8,
    /// trades of at least `whale_usd` as bubbles on the candles
    whales: bool,
    whale_usd: f64,
    /// indicator inputs
    ind: Ind,
    /// drawings hidden (alerts still fire)
    hide: bool,
    /// sound of new alerts
    alert_sound: String,
    /// overlay defaults generation: settings from before 2 get the calmer defaults once
    #[serde(default)]
    style: u32,
    /// expected-move cone from the volatility model (+-1 / +-2 sigma over the next bars)
    cone: bool,
    scale: Scale,
    /// positions / orders of the current symbol, set by the app every frame
    #[serde(skip)] pub lines: Vec<ChartLine>,
    #[serde(skip)] base_unit: String,
    /// manual price range (None = fit the visible bars): set by dragging the price axis,
    /// then vertical drags on the plot pan it; "auto" or a double-click returns to fitting
    #[serde(skip)] y_range: Option<(f64, f64)>,
    #[serde(skip)] liq: LiqCache,
    /// bars scrolled away from the live edge (0 = latest at the right)
    #[serde(skip)] right: f32,
    #[serde(skip)] cache: HeatCache,
    /// last heatmap column time the walls were computed for, and the (bid, ask) walls shown
    #[serde(skip)] walls_at: (i64, Vec<(f64, f64)>, Vec<(f64, f64)>),
    #[serde(skip)] tool: Option<Tool>,
    /// points placed so far of the drawing in progress
    #[serde(skip)] pending: Vec<(i64, f64)>,
    /// drawing part under the pointer last frame (index, 0 = point a, 1 = point b, 2 = whole), and
    /// the one being dragged: a drag that starts on a drawing edits it instead of panning
    #[serde(skip)] hot: Option<(usize, u8)>,
    #[serde(skip)] drag: Option<(usize, u8)>,
    /// pan velocity after a drag is released (bars per second), decaying
    #[serde(skip)] fling: f32,
    /// auto-fit price range as currently shown (base, interval, source, lo, hi), easing toward the target
    #[serde(skip)] eased: Option<((String, i64, Option<Exchange>), f64, f64)>,
    /// drawing edits: the last committed list (base, drawings), undo and redo stacks of earlier ones
    #[serde(skip)] committed: Option<(String, Vec<Drawing>)>,
    #[serde(skip)] undo: Vec<Vec<Drawing>>,
    #[serde(skip)] redo: Vec<Vec<Drawing>>,
    /// open right-click menu and where it was opened
    #[serde(skip)] menu: Option<(Menu, Pos2)>,
    #[serde(skip)] replay: Option<Replay>,
    /// last price seen per base, to detect alert crossings
    #[serde(skip)] alert_px: std::collections::HashMap<String, f64>,
    /// alerts that fired since the host last asked: (title, body)
    #[serde(skip)] pub fired: Vec<(String, String)>,
    /// banner of the last alert and until when it shows
    #[serde(skip)] toast: Option<(String, std::time::Instant)>,
    /// one cell of a multi-chart layout: the legend names the interval
    #[serde(skip)] pub multi: bool,
    /// account line under the pointer last frame, the one being dragged, and a move awaiting
    /// confirmation (what, label, from, to, where)
    #[serde(skip)] line_hot: Option<usize>,
    #[serde(skip)] line_drag: Option<usize>,
    #[serde(skip)] pending_move: Option<(LineAct, String, f64, f64, Pos2)>,
    /// requests for the app, drained each frame
    #[serde(skip)] pub actions: Vec<ChartAct>,
    /// set by the app: confirm before moving orders; keys present (order tools shown); account equity
    /// (USD) for the position tool's size; whether the trading venue amends in place
    #[serde(skip)] pub confirm: bool,
    #[serde(skip)] pub trading: bool,
    #[serde(skip)] pub equity: Option<f64>,
    #[serde(skip)] pub amends: bool,
    /// position tool: share of equity risked between entry and stop (percent)
    risk_pct: f64,
    /// set by the app: its chart layout; the toolbar's choice goes back through `layout_req`
    #[serde(skip)] pub layout: String,
    #[serde(skip)] pub layout_req: Option<String>,
}

impl Default for Chart {
    fn default() -> Self {
        Chart {
            tf: 1, src: None, heat: true, liqmap: false, liq: LiqCache::default(), right: 0.0, bar_w: 7.0, cache: HeatCache::default(), walls_at: Default::default(),
            drawings: Default::default(), tool: None, pending: vec![], hot: None, drag: None, fling: 0.0, eased: None, committed: None, undo: vec![], redo: vec![], menu: None, replay: None, alert_px: Default::default(), fired: vec![], toast: None, multi: false, line_hot: None, line_drag: None, pending_move: None, actions: vec![], confirm: true, trading: false, equity: None, amends: false, risk_pct: 1.0, layout: "1".into(), layout_req: None, ind: Ind::default(), hide: false, alert_sound: "depth:chime".into(), ma: false, mas: default_mas(), magnet: true, sr: true, walls: false, mark: true, breakouts: true, vwap: true, bb: false, fp: false, fp_style: 0, whales: false, whale_usd: 100_000.0, cone: false, style: 2, lines: vec![], base_unit: String::new(), scale: Scale::Linear, y_range: None,
            panes: vec![(Pane::Vol, true), (Pane::Cvd, true), (Pane::Oi, true), (Pane::Funding, true), (Pane::Basis, false), (Pane::Ls, false), (Pane::Premium, false), (Pane::Ofi, false), (Pane::BookFlow, false), (Pane::Rsi, false), (Pane::Macd, false), (Pane::Delta, false)],
        }
    }
}

/// Resample one-minute bars into `tf`-minute bars.
pub fn resample(bars: &[Bar], tf: i64) -> Vec<Bar> {
    if tf == 1 { return bars.to_vec(); }
    let mut out: Vec<Bar> = vec![];
    for b in bars {
        let t = bucket(b.t, tf);
        match out.last_mut() {
            Some(o) if o.t == t => {
                o.h = o.h.max(b.h); o.l = o.l.min(b.l); o.c = b.c;
                o.vol += b.vol; o.buy += b.buy; o.sell += b.sell;
                o.liq_long += b.liq_long; o.liq_short += b.liq_short;
                o.flow += b.flow;
                o.cvd = b.cvd;
                if b.oi.is_some() { o.oi = b.oi; }
                if b.funding_h.is_some() { o.funding_h = b.funding_h; }
                if b.funding_settled.is_some() { o.funding_settled = b.funding_settled; }
                if b.basis_bps.is_some() { o.basis_bps = b.basis_bps; }
                if b.ls.is_some() { o.ls = b.ls; }
            }
            _ => out.push(Bar { t, ..b.clone() }),
        }
    }
    out
}

fn nice(x: f64) -> f64 { depth::agg::nice(x.max(1e-12)) }

/// Finite min/max of `vals`, widened to at least ±0.1% of its magnitude (a single bar's high and
/// low can differ only by float noise), then padded by `pad` of the span.
fn padded_range(vals: impl Iterator<Item = f64>, pad: f64, zero: bool) -> (f64, f64) {
    let (mut lo, mut hi) = vals.filter(|v| v.is_finite()).fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(v), h.max(v)));
    if lo > hi { lo = 0.0; hi = 1.0; }
    if zero { lo = lo.min(0.0); hi = hi.max(0.0); }
    let min_span = (lo.abs().max(hi.abs()) * 2e-3).max(1e-9);
    if hi - lo < min_span { let m = (hi + lo) / 2.0; lo = m - min_span / 2.0; hi = m + min_span / 2.0; }
    let p = (hi - lo) * pad;
    (lo - p, hi + p)
}

/// Grid values between lo and hi at a "nice" step, about `target` lines, never more than 50.
/// (A `while v < hi { v += step }` loop never ends once step is below v's float resolution.)
fn grid(lo: f64, hi: f64, target: f64) -> Vec<f64> {
    let step = nice((hi - lo) / target);
    let first = (lo / step).ceil();
    (0..50).map(|i| (first + i as f64) * step).take_while(|v| *v < hi).collect()
}

impl HeatCache {
    /// Incrementally fold new heatmap columns into per-bar summed-liquidity rows.
    fn update(&mut self, h: &Heatmap, tf: i64, market: Market) {
        if self.tf != tf || self.market != Some(market) || self.bin != h.bin {
            *self = HeatCache { tf, market: Some(market), bin: h.bin, ..Default::default() };
        }
        let ms = tf * 60_000;
        let since = self.last_ts;
        for (ts, col) in h.cols.iter().filter(|(ts, _)| *ts > since) {
            // guard: a column spanning absurdly many bins would allocate gigabytes
            if let (Some(a), Some(b)) = (col.iter().map(|c| c.0).min(), col.iter().map(|c| c.0).max()) { if b - a > 20_000 { continue; } }
            let t = ts.div_euclid(ms) * ms;
            if self.bars.last().is_none_or(|b| b.0 != t) {
                let lo = col.iter().map(|c| c.0).min().unwrap_or(0) - 50;
                self.bars.push((t, lo, vec![], 0));
            }
            let (_, lo, v, n) = self.bars.last_mut().unwrap();
            if col.iter().any(|c| (c.0 - *lo).abs() > 20_000) { continue; }
            for &(bin, q) in col {
                if bin < *lo {
                    let grow = (*lo - bin) as usize;
                    v.splice(0..0, std::iter::repeat_n(0.0, grow));
                    *lo = bin;
                }
                let i = (bin - *lo) as usize;
                if i >= v.len() { v.resize(i + 1, 0.0); }
                v[i] += q;
            }
            *n += 1;
            self.cur.clone_from(col);
            self.last_ts = *ts;
            self.ver += 1;
        }
        if self.bars.len() > 2000 { self.bars.drain(..self.bars.len() - 2000); }
    }

    /// The heatmap as one RGBA texture (columns = bars, rows = price bins, top = highest price),
    /// re-uploaded only when new samples arrived. Drawn as a single textured rect per frame.
    fn texture(&mut self, ctx: &egui::Context) -> Option<(egui::TextureId, TexGeom)> {
        if let Some((v, t, g)) = &self.tex { if *v == self.ver { return Some((t.id(), *g)); } }
        let (first, last) = (self.bars.first()?, self.bars.last()?);
        let ms = self.tf * 60_000;
        let lo = self.bars.iter().map(|b| b.1).min()?;
        let hi = self.bars.iter().map(|b| b.1 + b.2.len() as i64).max()?;
        let (cols, bins) = (((last.0 - first.0) / ms + 1) as usize, (hi - lo).max(1) as usize);
        if cols * bins > 8_000_000 { return None; }
        let last_i = self.bars.len() - 1;
        let vals: Vec<(i64, i64, Vec<f32>)> = self.bars.iter().enumerate().map(|(i, (t, blo, v, n))| {
            if i == last_i {
                let mut w = vec![0.0; v.len()];
                for &(b, q) in &self.cur { if let Some(x) = usize::try_from(b - blo).ok().and_then(|k| w.get_mut(k)) { *x = q; } }
                (*t, *blo, w)
            } else { (*t, *blo, v.iter().map(|q| q / (*n).max(1) as f32).collect()) }
        }).collect();
        let mut qs: Vec<f32> = vals.iter().flat_map(|b| b.2.iter().copied().filter(|q| *q > 0.0)).collect();
        if qs.is_empty() { return None; }
        qs.sort_by(f32::total_cmp);
        let qmax = qs[(qs.len() * 97 / 100).min(qs.len() - 1)].max(1e-9);
        let mut img = egui::ColorImage::new([cols, bins], vec![Color32::TRANSPARENT; cols * bins]);
        for (bt, blo, v) in &vals {
            let x = ((bt - first.0) / ms) as usize;
            for (k, q) in v.iter().enumerate() {
                if *q <= 0.0 { continue; }
                let row = bins - 1 - (blo + k as i64 - lo) as usize;
                img.pixels[row * cols + x] = heat_color((q / qmax).min(1.0).sqrt());
            }
        }
        let geom = TexGeom { t0: first.0, ms, lo, bins, cols };
        let t = match self.tex.take() {
            Some((_, mut t, _)) => { t.set(img, egui::TextureOptions::NEAREST); t }
            None => ctx.load_texture("heatmap", img, egui::TextureOptions::NEAREST),
        };
        let id = t.id();
        self.tex = Some((self.ver, t, geom));
        Some((id, geom))
    }
}

/// Estimated liquidation levels left open after `bars`. Model: each bar's traded volume opens
/// longs and shorts at its typical price, spread over LEVERAGE; a level is wiped once price trades
/// through it. Calibrated against 90 days of Gate per-5m liquidation sizes (BTC/ETH/SOL, tuned on
/// the first 60 days, scored on the last 30): out of sample the swept mass explains liquidations
/// beyond what a 3h breakout alone does (incremental R2 0.10 on log size). Volume beat OI increases
/// (0.067); adding OI, taker-side splits or time decay did not help.
/// ponytail: one leverage mix for every venue and asset, no position data; re-fit against venue
/// liquidation history (Gate contract_stats) if venues ever publish leverage distributions.
const LEVERAGE: [(f64, f64); 6] = [(3.0, 0.15), (5.0, 0.2), (10.0, 0.25), (20.0, 0.2), (50.0, 0.15), (100.0, 0.05)];
const MMR: f64 = 0.005;

/// Price of bin i is (lo + i) * bin; quantities in base units.
struct LiqMap { bin: f64, lo: i64, long: Vec<f32>, short: Vec<f32> }

fn liq_levels(bars: &[Bar]) -> Option<LiqMap> {
    let last = bars.last()?;
    let bin = nice(last.c * 5e-4);
    let (plo, phi) = bars.iter().fold((f64::MAX, f64::MIN), |(l, h), b| (l.min(b.l), h.max(b.h)));
    if !(plo > 0.0 && phi.is_finite()) { return None; }
    let lo = (plo * (1.0 - 1.0 / LEVERAGE[0].0) / bin).floor() as i64;
    let hi = (phi * (1.0 + 1.0 / LEVERAGE[0].0) / bin).ceil() as i64;
    let bins = (hi - lo + 1) as usize;
    if bins > 200_000 { return None; }
    let (mut long, mut short) = (vec![0f32; bins], vec![0f32; bins]);
    let at = |p: f64| ((p / bin).round() as i64 - lo).clamp(0, bins as i64 - 1) as usize;
    for b in bars {
        // price traded through: longs at or above the low and shorts at or below the high are gone
        long[at(b.l)..].iter_mut().for_each(|q| *q = 0.0);
        short[..=at(b.h)].iter_mut().for_each(|q| *q = 0.0);
        if b.vol > 0.0 {
            let p = (b.h + b.l + b.c) / 3.0;
            for (lev, w) in LEVERAGE {
                long[at(p * (1.0 - 1.0 / lev + MMR))] += (b.vol * w / 2.0) as f32;
                short[at(p * (1.0 + 1.0 / lev - MMR))] += (b.vol * w / 2.0) as f32;
            }
        }
    }
    Some(LiqMap { bin, lo, long, short })
}

/// A volume level: centre price, half-width (one profile bin), strength (volume / POC volume).
#[derive(Clone, Copy, Debug)]
struct SrLevel { px: f64, half: f64, score: f64 }

/// Volume at price from 1m bars: each bar's volume spread evenly over its high-low range (the
/// standard approximation without tick data). Bin i covers [lo + i*bin, lo + (i+1)*bin).
struct Profile { lo: f64, bin: f64, vol: Vec<f64> }

impl Profile {
    fn build(bars: &[Bar], lo: f64, hi: f64, bins: usize) -> Option<Profile> {
        if !(hi > lo) || bins == 0 { return None; }
        let bin = (hi - lo) / bins as f64;
        let mut vol = vec![0.0; bins];
        for b in bars.iter().filter(|b| b.vol > 0.0 && b.h >= lo && b.l <= hi) {
            // the high is exclusive: a bar ending exactly on a bin edge does not spill into the next bin
            let (a, z) = (((b.l.max(lo) - lo) / bin) as usize, ((((b.h.min(hi) - lo) / bin) - 1e-9).max(0.0) as usize).min(bins - 1));
            let n = (z - a.min(z) + 1) as f64;
            for v in &mut vol[a.min(z)..=z] { *v += b.vol / n; }
        }
        vol.iter().any(|v| *v > 0.0).then_some(Profile { lo, bin, vol })
    }
    fn px(&self, i: usize) -> f64 { self.lo + (i as f64 + 0.5) * self.bin }
    fn poc(&self) -> usize { (0..self.vol.len()).max_by(|a, b| self.vol[*a].total_cmp(&self.vol[*b])).unwrap_or(0) }
    /// Value area: grow from the POC toward the larger neighbour until 70% of the volume is inside.
    fn value_area(&self) -> (usize, usize) {
        let total: f64 = self.vol.iter().sum();
        let (mut a, mut z) = (self.poc(), self.poc());
        let mut inside = self.vol[a];
        while inside < total * 0.7 && (a > 0 || z + 1 < self.vol.len()) {
            let down = if a > 0 { self.vol[a - 1] } else { -1.0 };
            let up = if z + 1 < self.vol.len() { self.vol[z + 1] } else { -1.0 };
            if up >= down { z += 1; inside += up; } else { a -= 1; inside += down; }
        }
        (a, z)
    }
    /// High-volume nodes: local maxima (over +-3 bins) holding at least 40% of the POC's volume.
    fn hvns(&self) -> Vec<SrLevel> {
        let max = self.vol[self.poc()];
        let n = self.vol.len();
        (0..n).filter(|&i| {
            let (a, z) = (i.saturating_sub(3), (i + 4).min(n));
            // on a flat top only its first bin counts
            self.vol[i] >= 0.4 * max && self.vol[a..z].iter().all(|v| *v <= self.vol[i]) && self.vol[a..i].iter().all(|v| *v < self.vol[i])
        }).map(|i| SrLevel { px: self.px(i), half: self.bin, score: self.vol[i] / max }).collect()
    }
}

/// Session VWAP from 00:00 UTC over 1m bars, with the volume-weighted standard deviation:
/// (bar time, vwap, sigma) after each bar.
fn session_vwap(bars: &[Bar]) -> Vec<(i64, f64, f64)> {
    let Some(last) = bars.last() else { return vec![] };
    let day0 = last.t - last.t.rem_euclid(86_400_000);
    let (mut pv, mut v, mut p2v) = (0.0, 0.0, 0.0);
    bars.iter().filter(|b| b.t >= day0 && b.vol > 0.0).map(|b| {
        let p = (b.h + b.l + b.c) / 3.0;
        pv += p * b.vol; v += b.vol; p2v += p * p * b.vol;
        let m = pv / v;
        (b.t, m, (p2v / v - m * m).max(0.0).sqrt())
    }).collect()
}

/// Levels closed through within the last `within` bars: (level, upward, failed). A break that
/// was closed back through the other way inside the window is reported as the original break,
/// failed (a fakeout / trap), rather than as a fresh break the other way.
fn broken_levels(bars: &[Bar], levels: &[SrLevel], within: usize) -> Vec<(SrLevel, bool, bool)> {
    let n = bars.len();
    if n < 2 { return vec![]; }
    let from = n.saturating_sub(within + 1);
    levels.iter().filter_map(|l| {
        let ev: Vec<bool> = (from + 1..n).filter_map(|k| {
            let (a, b) = (bars[k - 1].c, bars[k].c);
            if a < l.px - l.half && b > l.px + l.half { Some(true) } else if a > l.px + l.half && b < l.px - l.half { Some(false) } else { None }
        }).collect();
        let last = *ev.last()?;
        Some(if ev.len() > 1 { (*l, !last, true) } else { (*l, last, false) })
    }).take(2).collect()
}

#[derive(Default)]
struct LiqCache {
    /// (tf, bar count, last bar time, last volume bits): recomputed only when one of them changes
    key: (i64, usize, i64, u64),
    map: Option<LiqMap>,
}

impl LiqCache {
    fn get(&mut self, bars: &[Bar], tf: i64) -> Option<&LiqMap> {
        let last = bars.last()?;
        let key = (tf, bars.len(), last.t, last.vol.to_bits());
        if key != self.key { self.key = key; self.map = liq_levels(bars); }
        self.map.as_ref()
    }
}

/// Best bid/ask for the price line: the venue's own book when one venue is charted; for the
/// aggregated (index) chart, the tightest venue half-spread around the index (the same top the
/// mid-aligned aggregated book shows). Display only.
fn bid_ask(a: &Agg, src: Option<Exchange>, m: Market, index: f64) -> Option<(f64, f64)> {
    let top = |v: &depth::agg::Venue| match (v.book.best_bid(), v.book.best_ask()) {
        (Some(b), Some(k)) => Some((b.0, k.0)),
        _ => v.bbo.map(|[b, _, k, _]| (b, k)),
    }.filter(|(b, k)| *b > 0.0 && k > b);
    if let Some(e) = src { return top(a.venues.get(&(e, m))?); }
    let hs = a.venues.iter().filter(|((_, vm), _)| *vm == m).filter_map(|(_, v)| top(v)).map(|(b, k)| (k - b) / (k + b)).fold(f64::INFINITY, f64::min);
    hs.is_finite().then(|| (index * (1.0 - hs), index * (1.0 + hs)))
}

/// Fractional bar index of time `t` (bars may have gaps; beyond either end extrapolate by `ms`).
fn idx_of(bars: &[Bar], t: i64, ms: i64) -> f32 {
    let i = bars.partition_point(|b| b.t <= t);
    if i == 0 { return (t - bars[0].t) as f32 / ms as f32; }
    let b = &bars[i - 1];
    (i - 1) as f32 + ((t - b.t) as f32 / ms as f32).min(if i < bars.len() { 0.999 } else { f32::MAX })
}

/// Time at fractional bar index `x`.
fn time_of(bars: &[Bar], x: f32, ms: i64) -> i64 {
    let i = (x.floor().max(0.0) as usize).min(bars.len() - 1);
    bars[i].t + ((x - i as f32) * ms as f32) as i64
}

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    (a + ab * t - p).length()
}

struct Frame { first: f32, right_edge: f32, bar_w: f32, plot: Rect }
impl Frame {
    fn x(&self, i: f32) -> f32 { self.plot.right() - (self.right_edge - i) * self.bar_w - self.bar_w / 2.0 }
    fn idx(&self, x: f32) -> f32 { self.right_edge - (self.plot.right() - x - self.bar_w / 2.0) / self.bar_w }
}

fn y_of(v: f64, lo: f64, hi: f64, r: Rect) -> f32 { r.bottom() - ((v - lo) / (hi - lo)) as f32 * r.height() }

fn fmt_big(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.2}") }
}

impl Chart {
    pub fn show(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str) {
        // one-time move to the calmer defaults (fewer overlays on by default); each stays a toggle
        if self.style < 2 { self.ma = false; self.walls = false; self.cone = false; self.style = 2; }
        // panes added after a layout was saved
        for p in [Pane::Premium, Pane::Ofi, Pane::BookFlow, Pane::Rsi, Pane::Macd, Pane::Delta] { if !self.panes.iter().any(|x| x.0 == p) { self.panes.push((p, false)); } }
        let smarket = if market == Market::Margin { Market::Spot } else { market };
        // hosted: the native toolbar above the chart drives these through native_set
        if !native() { self.toolbar(ui, a, smarket, base); }
        self.base_unit = base.to_string();
        let Some(series) = a.series.get(&(self.src, smarket)) else {
            ui.centered_and_justified(|ui| ui.label(t("chart.loading")));
            return;
        };
        let mut raw: Vec<Bar> = series.bars.iter().cloned().collect();
        let mut bars = resample(&raw, self.tf);
        // native-interval history, resampled when the interval is built from a smaller one
        let nt = native_tf(self.tf);
        let tf_hist = (nt > 1).then(|| a.tf.get(&(nt as u32)).and_then(|s| s.bars(self.src, smarket))).flatten()
            .map(|h| if nt == self.tf { h.to_vec() } else { resample(h, self.tf) });
        let loading_tf = nt > 1 && tf_hist.is_none();
        if let (Some(h), Some(first)) = (tf_hist.as_deref(), raw.first()) { bars = stitch(h, bars, first.t, self.tf); }
        // a cross-venue bar opened by trades before any index price exists has no OHLC yet: a zero
        // low would stretch the price axis to 0
        bars.retain(|b| b.l > 0.0 && b.c > 0.0);
        if bars.is_empty() { ui.centered_and_justified(|ui| ui.label(t("chart.empty"))); return; }
        // bar replay: advance while playing, then hide every bar after the replay bar
        let now = ui.input(|i| i.time);
        let mut next_t = None;
        if let Some(r) = self.replay.as_mut() {
            if let (Some(mut at), true) = (r.at, r.playing) {
                let step = 1.0 / SPEEDS[r.speed];
                while now - r.last >= step {
                    r.last += step;
                    match bars.iter().find(|b| b.t > at) { Some(b) => at = b.t, None => { r.playing = false; break; } }
                }
                r.at = Some(at);
                // one repaint per step, not every frame
                if r.playing { ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64((r.last + step - now).max(0.0))); }
            } else { r.last = now; }
            next_t = r.at.and_then(|at| bars.iter().find(|b| b.t > at).map(|b| b.t));
        }
        // live-only overlays (book, liquidation map, account lines, bid / ask) are off in a replay
        let live = !self.replay.is_some_and(|r| r.at.is_some());
        if let Some(at) = self.replay.and_then(|r| r.at) {
            bars.truncate(bars.partition_point(|b| b.t <= at).max(1));
            let end = bars.last().map_or(at, |b| b.t) + self.tf * 60_000;
            raw.truncate(raw.partition_point(|b| b.t < end));
        }

        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, panel());

        let panes: Vec<Pane> = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || smarket == Market::Perp)).map(|(p, _)| *p).collect();
        let subs_h = panes.len() as f32 * SUB_H;
        let main_h = (rect.height() - TIME_H - subs_h).max(120.0);
        let plot_w = rect.width() - AXIS_W;
        let main = Rect::from_min_size(rect.min, vec2(plot_w, main_h));

        // interaction (panning stays on while a drawing tool is armed: placing is click-only)
        // a drag that starts on the price axis scales prices instead of panning time
        let on_axis = resp.interact_pointer_pos().is_some_and(|p| p.x > main.right() && p.y < main.bottom());
        // a drag that starts on a drawing (hit-tested last frame) moves it instead of the chart
        if resp.drag_started() {
            // an account line (hit-tested last frame) wins over drawings
            self.line_drag = if on_axis || self.tool.is_some() { None } else { self.line_hot };
            self.drag = if on_axis || self.line_drag.is_some() { None } else { self.hot.filter(|h| h.1 != LOCKED) };
            self.fling = 0.0;
        }
        let editing = self.drag.is_some() || self.line_drag.is_some();
        let dt = ui.input(|i| i.stable_dt).clamp(0.001, 0.05);
        if resp.dragged() && !on_axis && !editing { self.right += resp.drag_delta().x / self.bar_w; }
        if resp.drag_stopped() {
            // released while moving: keep gliding (TradingView-style inertia)
            let vx = ui.input(|i| i.pointer.velocity().x);
            self.fling = if !editing && !on_axis && vx.abs() > 150.0 { vx / self.bar_w } else { 0.0 };
            self.drag = None;
        } else if !resp.dragged() && self.fling != 0.0 {
            self.right += self.fling * dt;
            self.fling *= (-dt * 4.0).exp();
            // repaints only while gliding: under half a bar per second it stops
            if self.fling.abs() < 0.5 { self.fling = 0.0 } else { ui.ctx().request_repaint(); }
        }
        if resp.hover_pos().is_some_and(|p| p.x > main.right() && p.y < main.bottom()) { ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical); }
        // wheel over the price axis scales prices; over the plot vertical scroll / pinch zooms time
        // around the pointer (the bar under it stays put) and horizontal scroll pans
        let mut y_wheel = 0.0f32;
        if let (true, Some(hp)) = (resp.hovered(), resp.hover_pos()) {
            let (d, pinch) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if hp.x > main.right() && hp.y < main.bottom() { y_wheel = d.y; } else {
                let bw = (self.bar_w * (d.y * 0.003).exp() * pinch).clamp(1.5, if self.fp { 160.0 } else { 60.0 });
                if bw != self.bar_w {
                    let edge = bars.len() as f32 + 3.0 - self.right;
                    let idx = edge - (main.right() - hp.x - self.bar_w / 2.0) / self.bar_w;
                    let edge2 = idx + (main.right() - hp.x - bw / 2.0) / bw;
                    self.right = bars.len() as f32 + 3.0 - edge2;
                    self.bar_w = bw;
                }
                if d.x != 0.0 { self.right += d.x / self.bar_w; self.fling = 0.0; }
            }
        }
        if resp.double_clicked() && self.hot.is_none() { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; }
        let n_vis = plot_w / self.bar_w;
        self.right = self.right.clamp(-n_vis * 0.5, bars.len() as f32);
        let right_edge = bars.len() as f32 - 1.0 + 4.0 - self.right;
        let fr = Frame { first: right_edge - n_vis, right_edge, bar_w: self.bar_w, plot: main };
        let i0 = fr.first.floor().max(0.0) as usize;
        let i1 = (right_edge.ceil() as usize).min(bars.len() - 1);
        if i0 > i1 { return; }
        let vis = &bars[i0..=i1];

        // price scale: auto-fit eases toward the visible range (a new high slides the scale instead
        // of snapping it); another symbol / interval / source, or a jump of more than the range, snaps
        let (tlo, thi) = padded_range(vis.iter().flat_map(|b| [b.l, b.h]), 0.06, false);
        let ease_key = (base.to_string(), self.tf, self.src);
        let (alo, ahi) = match &self.eased {
            Some((k, lo0, hi0)) if *k == ease_key && (tlo - lo0).abs().max((thi - hi0).abs()) < (thi - tlo) => {
                let f = 1.0 - (-dt as f64 * 14.0).exp();
                let (l2, h2) = (lo0 + (tlo - lo0) * f, hi0 + (thi - hi0) * f);
                // repaint only while still moving visibly (under a hundredth of a pixel: done)
                if ((l2 - tlo).abs() + (h2 - thi).abs()) / (thi - tlo).max(1e-12) > 1e-4 { ui.ctx().request_repaint(); (l2, h2) } else { (tlo, thi) }
            }
            _ => (tlo, thi),
        };
        self.eased = Some((ease_key, alo, ahi));
        let (mut lo, mut hi) = self.y_range.unwrap_or((alo, ahi));
        if y_wheel != 0.0 {
            let (mid, half) = ((lo + hi) / 2.0, (hi - lo) / 2.0 * (-y_wheel as f64 * 0.003).exp());
            (lo, hi) = (mid - half, mid + half);
            self.y_range = Some((lo, hi));
        }
        if resp.dragged() {
            let (dy, ph) = (resp.drag_delta().y as f64, (main.height() - 54.0).max(50.0) as f64);
            if on_axis {
                // drag down stretches the range (candles shrink), up compresses it, around the middle
                let (mid, half) = ((lo + hi) / 2.0, (hi - lo) / 2.0 * (dy * 0.006).exp());
                (lo, hi) = (mid - half, mid + half);
                self.y_range = Some((lo, hi));
            } else if self.y_range.is_some() && !editing {
                let shift = dy / ph * (hi - lo);
                (lo, hi) = (lo + shift, hi + shift);
                self.y_range = Some((lo, hi));
            }
        }
        if !(hi > lo && lo.is_finite() && hi.is_finite()) { (lo, hi) = (alo, ahi); self.y_range = None; }
        // legend rows sit above the plot: OHLC, MAs, then the overlays' key values when any is on
        let overlays = self.sr || self.vwap || self.cone || self.bb || ((self.mark || self.liqmap) && smarket == Market::Perp);
        let main_plot = Rect::from_min_max(pos2(main.left(), main.top() + if overlays && self.ma && self.mas.iter().any(|m| m.on) { 70.0 } else if overlays { 52.0 } else { 48.0 }), pos2(main.right(), main.bottom() - 6.0));
        let ps = PScale { lo, hi, log: self.scale == Scale::Log && lo > 0.0, r: main_plot };
        let y = |v: f64| ps.y(v);
        // percent scale: labels relative to the first visible close
        let base_px = vis.first().map_or(1.0, |b| b.c);
        let pct = self.scale == Scale::Percent;
        // as many decimals as the grid step needs (83,000 not 83,000.0; 0.4512 when the step is 0.0005)
        let gv: Vec<f64> = grid(lo, hi, (main_plot.height() as f64 / 60.0).max(2.0));
        let gdp_step = gv.windows(2).map(|w| (w[1] - w[0]).abs()).fold(f64::MAX, f64::min);
        let gdp = if gdp_step.is_finite() && gdp_step > 0.0 { super::step_dp(gdp_step) } else { 2 };
        let axis_label = |v: f64| if pct { format!("{:+.2}%", (v / base_px - 1.0) * 100.0) } else { super::fmt_dp(v, gdp) };

        // grid + price axis (TradingView: faint dotted grid; round levels a shade brighter)
        let round = |v: f64| { let k = v / (gdp_step * 5.0); (k - k.round()).abs() < 1e-6 };
        for v in gv.iter().copied() {
            let yy = y(v);
            grid_line(&painter, pos2(main.left(), yy), pos2(main.right(), yy));
        }
        // watermark: the Depth mark and wordmark, faint, bottom-left of the plot (as TradingView does)
        {
            let logo = super::theme::app_logo(ui.ctx());
            let r = Rect::from_min_size(pos2(main_plot.left() + 14.0, main_plot.bottom() - 40.0), vec2(26.0, 26.0));
            painter.image(logo.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::from_white_alpha(70));
            painter.text(pos2(r.right() + 8.0, r.center().y), Align2::LEFT_CENTER, "Depth", super::theme::prop_s(19.0), fg().gamma_multiply(0.22));
        }

        // one textured quad: x by bar index, y by price of the top/bottom bin edges
        let quad = |tex: egui::TextureId, g: TexGeom, bin: f64| {
            let i0f = ((g.t0 - bars[0].t) / g.ms) as f32;
            let r = Rect::from_min_max(
                pos2(fr.x(i0f) - self.bar_w / 2.0, y((g.lo + g.bins as i64) as f64 * bin - bin / 2.0)),
                pos2(fr.x(i0f + g.cols as f32 - 1.0) + self.bar_w / 2.0, y(g.lo as f64 * bin - bin / 2.0)));
            painter.with_clip_rect(main_plot).image(tex, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        };
        // heatmap (cross-venue book, USDT) behind candles
        // the footprint replaces the book heatmap: two volume colorings on top of each other read as noise
        if self.heat && self.src.is_none() && live && !(self.fp && a.fp.contains_key(&(self.src, smarket))) {
            if let Some(h) = a.heat.get(&smarket) {
                self.cache.update(h, self.tf, smarket);
                // recorded live from the book: on long intervals it starts as the current bar only
                if let Some((tex, g)) = self.cache.texture(ui.ctx()) { quad(tex, g, self.cache.bin); }
            }
        }
        // estimated liquidation map: horizontal profile on the right edge of the price plot,
        // longs (below price) red, shorts (above) green, merged into ~2px rows
        let mut liq_legend: Option<Color32> = None;
        if self.liqmap && smarket == Market::Perp && live {
            // estimated from the 1m series whatever the interval: finer OI steps, same levels
            if let Some(m) = self.liq.get(&raw, 1) {
                let row = ((hi - lo) / main_plot.height() as f64 * 2.0).max(m.bin);
                let mut rows: std::collections::BTreeMap<i64, (f32, f32)> = Default::default();
                for (i, (l, s)) in m.long.iter().zip(&m.short).enumerate() {
                    let px = (m.lo + i as i64) as f64 * m.bin;
                    if px < lo || px > hi || (*l == 0.0 && *s == 0.0) { continue; }
                    let r = rows.entry((px / row).floor() as i64).or_default();
                    r.0 += l; r.1 += s;
                }
                let qmax = rows.values().map(|(l, s)| l.max(*s)).fold(0.0, f32::max);
                if qmax > 0.0 {
                    // one heat color for both sides (dim amber -> bright yellow): shorts are liquidated
                    // above the price, longs below; up / down colors here read as "who is long"
                    let w = 90.0;
                    let clip = painter.with_clip_rect(main_plot);
                    let (cold, hot) = (Color32::from_rgb(0x8a, 0x4b, 0x0f), Color32::from_rgb(0xff, 0xd6, 0x0a));
                    let ptr = resp.hover_pos().filter(|p| p.x > main.right() - w && p.x < main.right() && main_plot.contains(*p));
                    let mut tip = None;
                    for (k, (l, s)) in rows {
                        let q = l.max(s);
                        let (y0, y1) = (y((k + 1) as f64 * row), y(k as f64 * row));
                        let len = w * (q / qmax).sqrt();
                        let r = Rect::from_min_max(pos2(main.right() - len, y0), pos2(main.right(), y1.max(y0 + 1.0)));
                        let t = q / qmax;
                        clip.rect_filled(r, 0, lerp(cold, hot, t).gamma_multiply(0.35 + 0.55 * t));
                        if ptr.is_some_and(|p| p.y >= y0 && p.y <= y1.max(y0 + 1.0)) { tip = Some(((k as f64 + 0.5) * row, l, s)); }
                    }
                    if let (Some((px, l, s)), Some(p)) = (tip, ptr) {
                        let txt = format!("{}  {}\n{} ${}   {} ${}", super::l("Est. liquidations at"), fmt_px(px), super::l("Longs"), fmt_usd_short(l as f64 * px), super::l("Shorts"), fmt_usd_short(s as f64 * px));
                        let g = painter.layout(txt, mono(11.0), fg(), 260.0);
                        let b = Rect::from_min_size(pos2(main.right() - w - g.size().x - 22.0, p.y - g.size().y / 2.0 - 6.0), g.size() + vec2(12.0, 12.0));
                        painter.rect(b, 5, popup_bg(), Stroke::new(1.0, line()), egui::StrokeKind::Inside);
                        painter.galley(b.min + vec2(6.0, 6.0), g, fg());
                    }
                    liq_legend = Some(hot);
                }
            }
        }

        // label layout: the last-price axis tag and the bid/ask box are placed first; every other
        // label moves up or down until it overlaps none of them (place())
        let last_bar = bars.last().unwrap();
        let last_ly = y(last_bar.c);
        let ba = if live { bid_ask(a, self.src, smarket, last_bar.c) } else { None };
        let ba_rect = ba.map(|(bid, ask)| {
            let (ka, kb) = (t("tr.ask"), t("tr.bid"));
            let lw = painter.layout_no_wrap(ka.to_string(), prop(10.0), fg()).size().x.max(painter.layout_no_wrap(kb.to_string(), prop(10.0), fg()).size().x);
            let vw = painter.layout_no_wrap(fmt_px(ask.max(bid)), mono(10.0), fg()).size().x;
            let w = lw + vw + 16.0;
            Rect::from_min_size(pos2(main.right() - w - 4.0, last_ly - 15.0), vec2(w, 30.0))
        });
        let mut taken: Vec<Rect> = vec![Rect::from_min_size(pos2(main.right() + 2.0, last_ly - 10.0), vec2(AXIS_W - 4.0, 34.0))];
        // third legend row: each overlay's name, colour and current value, so no line needs guessing
        let mut legend3: Vec<(String, String, Color32)> = vec![];
        if let Some(c) = liq_legend { legend3.push((super::l("Liquidation map").into(), super::l("above: shorts · below: longs").into(), c)); }
        if let Some(r) = ba_rect { taken.push(r.expand(2.0)); }
        let place = |r: Rect, taken: &mut Vec<Rect>| -> Rect {
            let h = r.height() + 2.0;
            let cand = (0..12).map(|k| { let d = ((k + 1) / 2) as f32 * h * if k % 2 == 1 { 1.0 } else { -1.0 }; r.translate(vec2(0.0, d)) })
                .find(|c| taken.iter().all(|t| !t.intersects(*c))).unwrap_or(r);
            taken.push(cand);
            cand
        };

        // volume profile of the visible range (1m bars): histogram on the right, POC, value area,
        // high-volume nodes as support / resistance, and closes through them as breakouts
        let t_first = vis.first().map_or(0, |b| b.t);
        let vis_raw: Vec<Bar> = raw.iter().filter(|b| b.t >= t_first).cloned().collect();
        // bins sit on a fixed price grid (about 0.05% of price) over the data's own range, so the
        // levels do not move when the chart rescales or scrolls a little
        let prof = if self.sr || self.breakouts {
            let (dlo, dhi) = vis_raw.iter().fold((f64::MAX, f64::MIN), |(l, h), b| (l.min(b.l), h.max(b.h)));
            let bin = nice(vis_raw.last().map_or(1.0, |b| b.c) * 5e-4);
            let (glo, ghi) = ((dlo / bin).floor() * bin, (dhi / bin).ceil() * bin);
            let n = ((ghi - glo) / bin).round().clamp(1.0, 2000.0) as usize;
            Profile::build(&vis_raw, glo, glo + n as f64 * bin, n)
        } else { None };
        if let (true, Some(p)) = (self.sr, prof.as_ref()) {
            let max = p.vol[p.poc()];
            let (va0, va1) = p.value_area();
            let clip = painter.with_clip_rect(main_plot);
            let w = (main.width() * 0.18).min(160.0);
            for (i, v) in p.vol.iter().enumerate() {
                if *v <= 0.0 { continue; }
                let (y0, y1) = (y(p.lo + (i + 1) as f64 * p.bin), y(p.lo + i as f64 * p.bin));
                let len = w * (*v / max) as f32;
                let col = if i == p.poc() { Color32::from_rgb(0xf0, 0xb9, 0x0b) } else if (va0..=va1).contains(&i) { accent() } else { mu() };
                // anchored on the left: the right edge already carries the book depth and liquidation profiles
                clip.rect_filled(Rect::from_min_max(pos2(main.left(), y0 + 0.5), pos2(main.left() + len, (y1 - 0.5).max(y0 + 1.0))), 0, col.linear_multiply(0.14));
            }
            let poc = p.px(p.poc());
            let gold = Color32::from_rgb(0xf0, 0xb9, 0x0b);
            legend3.push((t("chart.poc").into(), fmt_px(poc), gold));
            legend3.push((t("chart.va").into(), format!("{} – {}", fmt_px(p.lo + va0 as f64 * p.bin), fmt_px(p.lo + (va1 + 1) as f64 * p.bin)), accent()));
            clip.hline(main.left()..=main.right(), y(poc), Stroke::new(1.2, gold.linear_multiply(0.85)));
            let mut tagged = vec![(poc, "POC".to_string(), gold)];
            for (px, k) in [(p.lo + (va1 + 1) as f64 * p.bin, "VAH"), (p.lo + va0 as f64 * p.bin, "VAL")] {
                painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), y(px)), pos2(main.right(), y(px))], Stroke::new(1.0, accent().linear_multiply(0.7)), 5.0, 4.0));
                tagged.push((px, k.to_string(), accent()));
            }
            // the two strongest nodes away from the POC (and not just broken: the breakout tag says it):
            // support below the last price, resistance above
            let last = bars.last().map_or(0.0, |b| b.c);
            let closed = &bars[..bars.len().saturating_sub(1)];
            let broken: Vec<f64> = if self.breakouts { broken_levels(closed, &p.hvns(), 10).into_iter().map(|(l, _, _)| l.px).collect() } else { vec![] };
            let mut nodes: Vec<SrLevel> = p.hvns().into_iter().filter(|l| (l.px - poc).abs() > p.bin * 3.0 && !broken.contains(&l.px)).collect();
            nodes.sort_by(|a, b| b.score.total_cmp(&a.score));
            for l in nodes.into_iter().take(2) {
                let (col, k) = if l.px > last { (dn(), "R") } else { (up(), "S") };
                clip.hline(main.left()..=main.right(), y(l.px), Stroke::new(1.0, col.linear_multiply(0.35 + 0.4 * l.score as f32)));
                tagged.push((l.px, k.to_string(), col));
            }
            // TradingView-style: the price sits in a tag on the axis, the name small at the plot's left
            // edge, so neither covers the candles
            for (px, tag, col) in tagged {
                if !(px > lo && px < hi) { continue; }
                let g = painter.layout_no_wrap(tag, prop(9.5), col);
                let r = Rect::from_min_size(pos2(main.left() + 4.0, y(px) - 13.0), vec2(g.size().x + 6.0, 12.0));
                painter.rect_filled(r, 2, label_bg().gamma_multiply(0.8));
                painter.galley(r.center() - g.size() / 2.0, g, col);
                let ax = place(Rect::from_min_size(pos2(main.right() + 2.0, y(px) - 8.0), vec2(AXIS_W - 4.0, 16.0)), &mut taken);
                painter.rect_filled(ax, 2, col.linear_multiply(0.85));
                painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(px), mono_m(11.0), Color32::BLACK);
            }
        }
        if let (true, Some(p)) = (self.breakouts, prof.as_ref()) {
            let (va0, va1) = p.value_area();
            let mut levels = p.hvns();
            levels.push(SrLevel { px: p.lo + (va1 + 1) as f64 * p.bin, half: p.bin, score: 1.0 });
            levels.push(SrLevel { px: p.lo + va0 as f64 * p.bin, half: p.bin, score: 1.0 });
            // only the most recent break: older ones are history the candles already show
            // closed bars only: the live bar's close is the moving price and would flip the verdict
            let closed = &bars[..bars.len().saturating_sub(1)];
            for (l, is_up, failed) in broken_levels(closed, &levels, 10).into_iter().take(1) {
                if !(l.px > lo && l.px < hi) { continue; }
                // a failed break up traps longs (bearish), a failed break down traps shorts: color by that
                let col = if is_up != failed { up() } else { dn() };
                let ly = y(l.px);
                let clip = painter.with_clip_rect(main_plot);
                if failed { clip.extend(egui::Shape::dashed_line(&[pos2(main.left(), ly), pos2(main.right(), ly)], Stroke::new(1.2, col), 6.0, 4.0)); }
                else { clip.hline(main.left()..=main.right(), ly, Stroke::new(1.5, col)); }
                let key = match (is_up, failed) { (true, false) => "chart.broke_up", (false, false) => "chart.broke_down", (true, true) => "chart.failed_up", (false, true) => "chart.failed_down" };
                let tag = format!("{} {} {}", if failed { "✕" } else if is_up { "↑" } else { "↓" }, t(key), fmt_px(l.px));
                let g = painter.layout_no_wrap(tag, mono(10.5), col);
                let r = place(Rect::from_min_size(pos2(main.center().x, ly - 9.0), g.size() + vec2(10.0, 4.0)), &mut taken);
                painter.rect_filled(r, 3, label_bg());
                painter.rect_stroke(r, 3, Stroke::new(1.0, col), egui::StrokeKind::Inside);
                painter.galley(r.min + vec2(5.0, 2.0), g, col);
            }
        }
        // expected move: EWMA volatility of 1m returns, widened with sqrt(time) over the next 30 bars
        if self.cone {
            if let (Some(vm), Some(lb)) = (depth::quant::vol_model(&raw), bars.last()) {
                let n = 30;
                let x0 = fr.x((bars.len() - 1) as f32);
                let at = |k: usize, d: f64| { let s = vm.sigma_1m * ((k as i64 * self.tf) as f64).sqrt(); pos2(fr.x((bars.len() - 1 + k) as f32), y(lb.c * (1.0 + d * s))) };
                let clip = painter.with_clip_rect(main_plot);
                for (d, a) in [(2.0, 0.06), (1.0, 0.10)] {
                    let mut pts: Vec<Pos2> = (0..=n).map(|k| at(k, d)).collect();
                    pts.extend((0..=n).rev().map(|k| at(k, -d)));
                    clip.add(egui::Shape::convex_polygon(pts, accent().linear_multiply(a), Stroke::NONE));
                }
                for d in [2.0, 1.0, -1.0, -2.0] {
                    clip.add(egui::Shape::line((0..=n).map(|k| at(k, d)).collect(), Stroke::new(0.8, accent().linear_multiply(0.45))));
                }
                legend3.push((t("chart.exp_move").into(), format!("1h ±{:.2}%  24h ±{:.2}%  ({} {:.0}%)", vm.sigma_1h * 100.0, vm.sigma_24h * 100.0, t("chart.vol_pct"), vm.percentile * 100.0), accent()));
                let end = at(n, 1.0);
                if end.x > x0 + 40.0 {
                    let s = vm.sigma_1m * ((n as i64 * self.tf) as f64).sqrt();
                    clip.text(pos2(end.x - 4.0, at(n, 2.0).y - 8.0), Align2::RIGHT_BOTTOM, format!("±1σ {:.2}%", s * 100.0), mono(10.0), accent());
                }
            }
        }

        // session VWAP (from 00:00 UTC, 1m bars) with +-1 / +-2 sigma bands, sampled at each chart bar's close
        if self.vwap {
            let vw = session_vwap(&raw);
            if !vw.is_empty() {
                let ms = self.tf * 60_000;
                let mut lines: [Vec<Pos2>; 5] = Default::default();
                for (k, b) in vis.iter().enumerate() {
                    let end = b.t + ms;
                    let Some(&(_, m, sd)) = vw.iter().rev().find(|(t, _, _)| *t < end) else { continue };
                    if vw[0].0 >= end { continue; }
                    let x = fr.x((i0 + k) as f32);
                    for (j, d) in [-2.0, -1.0, 0.0, 1.0, 2.0].iter().enumerate() { lines[j].push(pos2(x, y(m + d * sd))); }
                }
                let clip = painter.with_clip_rect(main_plot);
                let cyan = Color32::from_rgb(0x4f, 0xc3, 0xf7);
                for (j, pts) in lines.into_iter().enumerate() {
                    if pts.len() < 2 { continue; }
                    let (wdt, a) = match j { 2 => (1.5, 0.9), 1 | 3 => (0.8, 0.32), _ => (0.8, 0.18) };
                    clip.add(egui::Shape::line(pts, Stroke::new(wdt, cyan.linear_multiply(a))));
                }
                if let Some(&(_, m, sd)) = vw.last() {
                    legend3.push(("VWAP".into(), format!("{}  ±1σ {} – {}", fmt_px(m), fmt_px(m - sd), fmt_px(m + sd)), cyan));
                    if m > lo && m < hi {
                        let r = place(Rect::from_min_size(pos2(main.right() + 2.0, y(m) - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken);
                        painter.rect_filled(r, 3, cyan.linear_multiply(0.85));
                        painter.text(r.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(m), mono_m(11.0), Color32::BLACK);
                    }
                }
            }
        }
        // liquidity walls: resting size that stood far above the book's typical level for most of
        // the last minute (heatmap columns), so walls neither flicker nor include flashed orders
        if self.walls && self.src.is_none() && live {
            if let (Some(mid), Some(h)) = (a.mid(smarket), a.heat.get(&smarket)) {
                let ts = h.cols.back().map_or(0, |c| c.0);
                if ts != self.walls_at.0 {
                    let prev: Vec<f64> = self.walls_at.1.iter().chain(&self.walls_at.2).map(|w| w.0).collect();
                    let (b, k) = h.walls(mid, 60, &prev);
                    self.walls_at = (ts, b, k);
                }
                for (side, ask) in [(&self.walls_at.1, false), (&self.walls_at.2, true)] {
                    for &(px, qty) in side.iter().filter(|(p, _)| *p > lo && *p < hi) {
                        let col = if ask { dn() } else { up() };
                        let ly = y(px);
                        let x0 = main.right() - main.width() * 0.35;
                        painter.extend(egui::Shape::dotted_line(&[pos2(x0, ly), pos2(main.right(), ly)], col.linear_multiply(0.8), 5.0, 1.2));
                        let tag = format!("{} ${}", if ask { "Ask wall" } else { "Bid wall" }, fmt_usd_short(qty * px));
                        let g = painter.layout_no_wrap(tag, mono(10.5), col);
                        let r = place(Rect::from_min_size(pos2(x0 + 2.0, ly - 15.0), g.size() + vec2(6.0, 2.0)), &mut taken);
                        painter.rect_filled(r, 2, label_bg().gamma_multiply(0.85));
                        painter.galley(r.min + vec2(3.0, 1.0), g, col);
                    }
                }
            }
        }

        // footprint (live trades only), cluster bars: each bar is a column of cells, one per price
        // row, filled by that row's taker delta (green buys / red sells, stronger = more one-sided)
        // and labelled with its traded value once wide enough; the row with the most volume is
        // marked; total value above the column, delta below; the candle is a strip at the left
        let foot = a.fp.get(&(self.src, smarket)).filter(|f| self.fp && f.bin > 0.0 && self.bar_w >= 4.0);
        let fp_on = foot.is_some();
        let fp_text = fp_on && self.bar_w >= 42.0;
        if let Some(f) = foot {
            let ms = self.tf * 60_000;
            // rows are a 1-2-5 multiple of the recorded bin: 16 px with numbers, 6 px without
            let px_h = main_plot.height() as f64 / (hi - lo);
            let need = if fp_text { 16.0 } else { 6.0 } / (f.bin * px_h);
            let mult = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0].into_iter().find(|m| *m >= need).unwrap_or(5000.0);
            let row = f.bin * mult;
            let clip = painter.with_clip_rect(main_plot);
            let gold = Color32::from_rgb(0xf0, 0xb9, 0x0b);
            for (k, b) in vis.iter().enumerate() {
                let mut rows: std::collections::BTreeMap<i64, (f64, f64)> = Default::default();
                for (key, (bu, se)) in f.range(b.t, b.t + ms) {
                    let r = rows.entry((key as f64 * f.bin / row).floor() as i64).or_default();
                    r.0 += bu; r.1 += se;
                }
                if rows.is_empty() { continue; }
                // value in quote (USDT), at each row's middle price
                let val = |r: i64, q: f64| q * (r as f64 + 0.5) * row;
                let dmax = rows.iter().map(|(r, (bu, se))| val(*r, (bu - se).abs())).fold(0.0, f64::max).max(1e-12);
                let poc = rows.iter().max_by(|x, z| (x.1.0 + x.1.1).total_cmp(&(z.1.0 + z.1.1))).map(|(r, _)| *r);
                let x = fr.x((i0 + k) as f32);
                let (cx0, cx1) = if self.fp_style == 1 { (x + self.bar_w * 0.02, x + self.bar_w * 0.46) } else { (x - self.bar_w * 0.36, x + self.bar_w * 0.44) };
                let (mut top, mut bot) = (f32::MAX, f32::MIN);
                for (r, (bu, se)) in &rows {
                    let (y0, y1) = (y((*r + 1) as f64 * row), y(*r as f64 * row));
                    let cell = Rect::from_min_max(pos2(cx0, y0 + 0.5), pos2(cx1, (y1 - 0.5).max(y0 + 1.5)));
                    top = top.min(cell.top());
                    bot = bot.max(cell.bottom());
                    let d = bu - se;
                    let strength = (val(*r, d.abs()) / dmax).sqrt() as f32;
                    clip.rect_filled(cell, 1, if d >= 0.0 { up() } else { dn() }.linear_multiply(0.12 + 0.5 * strength));
                    if Some(*r) == poc { clip.rect_filled(Rect::from_min_max(cell.left_top(), pos2(cell.left() + 2.5, cell.bottom())), 0, gold); }
                    if fp_text && cell.height() >= 12.0 {
                        clip.text(cell.center(), Align2::CENTER_CENTER, fmt_fp(val(*r, bu + se)), mono((cell.height() * 0.62).clamp(9.0, 11.5)), Color32::from_white_alpha(235));
                    }
                }
                if self.bar_w >= 26.0 && top < bot {
                    let tot: f64 = rows.iter().map(|(r, (bu, se))| val(*r, bu + se)).sum();
                    let d: f64 = rows.iter().map(|(r, (bu, se))| val(*r, bu - se)).sum();
                    let fs = if self.bar_w >= 42.0 { 10.5 } else { 9.0 };
                    clip.text(pos2((cx0 + cx1) / 2.0, top - 3.0), Align2::CENTER_BOTTOM, fmt_fp(tot), mono(fs), mu());
                    clip.text(pos2((cx0 + cx1) / 2.0, bot + 3.0), Align2::CENTER_TOP, fmt_fp_signed(d), mono(fs), if d >= 0.0 { up() } else { dn() });
                }
            }
        }

        // candles
        // with the footprint: a strip left of the column (style 0), or a normal candle left of the cells (style 1)
        let (bw, shift) = match (fp_on, self.fp_style) {
            (false, _) => ((self.bar_w * 0.7).max(1.0), 0.0),
            (true, 1) => ((self.bar_w * 0.3).max(1.0), self.bar_w * 0.2),
            (true, _) => ((self.bar_w * 0.06).clamp(1.5, 4.0), self.bar_w * 0.42),
        };
        for (k, b) in vis.iter().enumerate() {
            let x = fr.x((i0 + k) as f32) - shift;
            let col = if b.c >= b.o { up() } else { dn() };
            painter.vline(x, y(b.h)..=y(b.l), Stroke::new(1.0, col));
            let (top, bot) = (y(b.o.max(b.c)), y(b.o.min(b.c)));
            painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, top), pos2(x + bw / 2.0, bot.max(top + 1.0))), 0, col);
        }

        // moving averages of the close, computed over every loaded bar so the left edge is right
        let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
        let lines: Vec<MaLine> = if self.ma { self.mas.iter().filter(|m| m.on && m.n >= 1).copied().collect() } else { vec![] };
        let mas: Vec<(Vec<Option<f64>>, MaLine)> = lines.iter().map(|m| (if m.ema { ema(&closes, m.n) } else { sma(&closes, m.n) }, *m)).collect();
        for (m, l) in &mas {
            let col = rgb(l.col);
            let pts: Vec<Pos2> = (i0..=i1).filter_map(|i| Some(pos2(fr.x(i as f32), y(m[i]?)))).collect();
            if pts.len() > 1 { painter.with_clip_rect(main_plot).add(egui::Shape::line(pts, Stroke::new(1.2, col))); }
        }
        if self.bb {
            let bb = bollinger(&closes, self.ind.bb_n.max(1), self.ind.bb_k);
            if let Some((m, w)) = bb.last().copied().flatten() {
                legend3.push((format!("BB {} {}", self.ind.bb_n, self.ind.bb_k), format!("{} – {} – {}", fmt_px(m - w), fmt_px(m), fmt_px(m + w)), rgb(0x2962ff)));
            }
            let pts = |f: &dyn Fn(f64, f64) -> f64| -> Vec<Pos2> { (i0..=i1).filter_map(|i| { let (m, w) = bb[i]?; Some(pos2(fr.x(i as f32), y(f(m, w)))) }).collect() };
            let (up_, mid, lo_) = (pts(&|m, w| m + w), pts(&|m, _| m), pts(&|m, w| m - w));
            let clip = painter.with_clip_rect(main_plot);
            let band = rgb(0x2962ff);
            // fill as one quad per bar step: a single concave polygon would tessellate wrongly
            for k in 1..up_.len().min(lo_.len()) {
                clip.add(egui::Shape::convex_polygon(vec![up_[k - 1], up_[k], lo_[k], lo_[k - 1]], band.gamma_multiply(0.04), Stroke::NONE));
            }
            for (p, c) in [(up_, band), (lo_, band), (mid, rgb(0xff6d00))] { if p.len() > 1 { clip.add(egui::Shape::line(p, Stroke::new(1.0, c))); } }
        }

        // highest high / lowest low of the visible range, Binance-style arrow labels
        let hi_k = (0..vis.len()).max_by(|x, z| vis[*x].h.total_cmp(&vis[*z].h));
        let lo_k = (0..vis.len()).min_by(|x, z| vis[*x].l.total_cmp(&vis[*z].l));
        for (k, v, above) in [(hi_k, vis.iter().map(|b| b.h).fold(f64::MIN, f64::max), true), (lo_k, vis.iter().map(|b| b.l).fold(f64::MAX, f64::min), false)] {
            let Some(k) = k else { continue };
            let x = fr.x((i0 + k) as f32);
            let yy = y(v) + if above { -8.0 } else { 8.0 };
            // point the label toward the chart's middle so it never runs off the right edge
            let left = x > main.center().x;
            let (end, align) = if left { (x - 22.0, Align2::RIGHT_CENTER) } else { (x + 22.0, Align2::LEFT_CENTER) };
            painter.line_segment([pos2(x, yy), pos2(end, yy)], Stroke::new(1.0, mu()));
            painter.text(pos2(end + if left { -3.0 } else { 3.0 }, yy), align, fmt_px(v), mono(10.5), fg());
        }

        // liquidation bubbles: longs liquidated under the candle, shorts above
        let lmax = vis.iter().map(|b| b.liq_long.max(b.liq_short)).fold(0.0, f64::max);
        if lmax > 0.0 {
            for (k, b) in vis.iter().enumerate() {
                let x = fr.x((i0 + k) as f32);
                for (q, py, col, dir) in [(b.liq_long, b.l, dn(), 1.0), (b.liq_short, b.h, up(), -1.0)] {
                    if q <= 0.0 { continue; }
                    let r = 3.0 + 11.0 * (q / lmax).sqrt() as f32;
                    painter.circle_filled(pos2(x, y(py) + dir * (r + 3.0)), r, col.linear_multiply(0.55));
                }
            }
        }

        // whale trades: one bubble per taker order (fills merged), area ~ USD size, at its own price
        if self.whales && !fp_text {
            let ms = self.tf * 60_000;
            let big: Vec<(f32, f64, f64, bool)> = a.big.iter()
                .filter(|t| t.market == smarket && self.src.is_none_or(|e| e == t.ex))
                .filter_map(|t| {
                    let k = a.usdt(t.ex, t.market);
                    let usd = t.px * t.qty * k;
                    let i = idx_of(&bars, t.ts, ms);
                    (usd >= self.whale_usd && i >= i0 as f32 - 0.5 && i <= i1 as f32 + 0.5).then(|| (i, if self.src.is_some() { t.px } else { t.px * k }, usd, t.side == depth::Side::Buy))
                }).collect();
            let top = big.iter().map(|b| b.2).fold(0.0, f64::max);
            let clip = painter.with_clip_rect(main_plot);
            for (i, px, usd, buy) in big {
                let r = 3.0 + 13.0 * (usd / top).sqrt() as f32;
                let c = if buy { up() } else { dn() };
                clip.circle(pos2(fr.x(i.round()), y(px)), r, c.linear_multiply(0.35), Stroke::new(1.0, c));
            }
        }

        // account lines: positions and resting orders of this symbol
        let mut line_hot = None;
        let hover_p = resp.hover_pos().filter(|p| main_plot.contains(*p));
        for (li, l) in self.lines.iter().enumerate().filter(|_| live) {
            if !(l.price > lo && l.price < hi) { continue; }
            let ly = y(l.price);
            if let (Some(_), Some(p), None) = (&l.act, hover_p, self.tool) { if (p.y - ly).abs() < 5.0 { line_hot = Some(li); } }
            dashed(&painter, main.left(), main.right(), ly, l.col.linear_multiply(0.8));
            let g1 = painter.layout_no_wrap(l.tag.clone(), prop(11.0), Color32::WHITE);
            let g2 = painter.layout_no_wrap(l.sub.clone(), mono(11.0), l.col);
            let x0 = main.left() + 6.0;
            let both = place(Rect::from_min_size(pos2(x0, ly - 10.0), vec2(g1.size().x + g2.size().x + 28.0, 20.0)), &mut taken);
            let r1 = Rect::from_min_size(both.min, vec2(g1.size().x + 14.0, 20.0));
            let r2 = Rect::from_min_size(pos2(r1.right(), both.top()), vec2(g2.size().x + 14.0, 20.0));
            painter.rect_filled(r1.union(r2), 3, label_bg());
            painter.rect_filled(r1, egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 }, l.col);
            painter.rect_stroke(r1.union(r2), 3, Stroke::new(1.0, l.col), egui::StrokeKind::Inside);
            painter.galley(r1.center() - g1.size() / 2.0, g1, Color32::WHITE);
            painter.galley(r2.center() - g2.size() / 2.0, g2, l.col);
            let ax = place(Rect::from_min_size(pos2(main.right() + 2.0, ly - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken);
            painter.rect_filled(ax, 3, label_bg());
            painter.rect_stroke(ax, 3, Stroke::new(1.0, l.col), egui::StrokeKind::Inside);
            painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(l.price), mono_m(11.0), l.col);
            if l.act.is_some() && hover_p.is_some_and(|p| both.contains(p)) { line_hot = Some(li); }
        }
        // dragging an order / TP / SL line: a ghost follows the pointer; release moves it (after a
        // confirmation when order confirmations are on)
        let ghost = |px: f64, col: Color32| {
            let gy = y(px);
            painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), gy), pos2(main.right(), gy)], Stroke::new(1.5, col), 6.0, 3.0));
            let ax = Rect::from_min_size(pos2(main.right() + 2.0, gy - 9.0), vec2(AXIS_W - 4.0, 18.0));
            painter.rect_filled(ax, 3, col);
            painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(px), mono(11.0), Color32::WHITE);
        };
        if let Some(li) = self.line_drag {
            match (self.lines.get(li), resp.interact_pointer_pos()) {
                (Some(l), Some(p)) if l.act.is_some() => {
                    let np = ps.v(p.y.clamp(main_plot.top(), main_plot.bottom()));
                    ghost(np, l.col);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    if resp.drag_stopped() {
                        if (y(np) - y(l.price)).abs() > 2.0 {
                            let act = l.act.clone().unwrap();
                            if self.confirm { self.pending_move = Some((act, l.tag.clone(), l.price, np, p)); } else { self.actions.push(ChartAct::Move(act, np)); }
                        }
                        self.line_drag = None;
                    }
                }
                _ => self.line_drag = None,
            }
        } else if line_hot.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            resp.clone().on_hover_text(t("tr.drag_line"));
        }
        self.line_hot = line_hot;
        if let Some((act, tag, from, to, at)) = self.pending_move.clone() {
            ghost(to, accent());
            let (mut open, mut done) = (true, None);
            egui::Popup::menu(&resp).id(ui.id().with("move_confirm")).at_position(at).open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.label(RichText::new(format!("{tag}:  {} → {}", fmt_px(from), fmt_px(to))).font(mono_s(12.5)).color(fg_strong()));
                // only Bybit / Binance orders are amended in place; elsewhere the order is cancelled and placed again
                if matches!(act, LineAct::Order(..)) && !self.amends { kit::note(ui, t("tr.move_replace"), WARN); }
                ui.horizontal(|ui| {
                    if kit::button(ui, t("tr.move"), kit::Look::Fill(accent()), kit::H, None, true).clicked() { done = Some(true); ui.close(); }
                    if kit::button(ui, t("tr.cancel_action"), kit::Look::Secondary, kit::H, None, true).clicked() { done = Some(false); ui.close(); }
                });
            });
            if done == Some(true) { self.actions.push(ChartAct::Move(act, to)); }
            if !open || done.is_some() { self.pending_move = None; }
        }

        // last price: dashed line; axis tag with the bar-close countdown; bid/ask box beside it
        let last = last_bar;
        let ly = last_ly;
        let lc = if last.c >= last.o { up() } else { dn() };
        dashed(&painter, main.left(), main.right(), ly, lc.linear_multiply(0.7));
        let lab = Rect::from_min_size(pos2(main.right() + 2.0, ly - 10.0), vec2(AXIS_W - 4.0, 34.0));
        painter.rect_filled(lab, 3, lc);
        painter.text(pos2(lab.left() + 6.0, ly), Align2::LEFT_CENTER, fmt_px(last.c), mono_s(11.5), Color32::WHITE);
        let left_ms = last.t + self.tf * 60_000 - depth::now_ms();
        if live { painter.text(pos2(lab.left() + 6.0, ly + 15.0), Align2::LEFT_CENTER, countdown(left_ms), mono_m(10.5), Color32::from_white_alpha(210)); }
        if let (Some((bid, ask)), Some(bx)) = (ba, ba_rect) {
            let (ka, kb) = (t("tr.ask"), t("tr.bid"));
            let w = bx.width();
            for (i, (k, v, col)) in [(ka, ask, dn()), (kb, bid, up())].into_iter().enumerate() {
                let r = Rect::from_min_size(pos2(bx.left(), bx.top() + i as f32 * 15.0), vec2(w, 15.0));
                painter.rect_filled(r.shrink2(vec2(0.0, 0.5)), 2, col.linear_multiply(0.85));
                painter.text(r.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, k, prop(10.0), Color32::WHITE);
                painter.text(r.right_center() - vec2(5.0, 0.0), Align2::RIGHT_CENTER, fmt_px(v), mono(10.0), Color32::WHITE);
            }
        }

        // mark price: the charted venue's, or the OI-weighted mark across perp venues (USDT) on the aggregate
        if self.mark && smarket == Market::Perp && live {
            let mark = match self.src {
                Some(e) => a.venues.get(&(e, Market::Perp)).and_then(|v| v.mark),
                None => {
                    let (mut num, mut den) = (0.0, 0.0);
                    for ((e, m), v) in a.venues.iter().filter(|((_, m), _)| *m == Market::Perp) {
                        if let (Some(mk), Some(oi)) = (v.mark, v.oi) { let w = oi.max(1e-9); num += mk * a.usdt(*e, *m) * w; den += w; }
                    }
                    (den > 0.0).then(|| num / den)
                }
            };
            if let Some(mk) = mark { legend3.push((t("chart.mark").into(), fmt_px(mk), PURPLE)); }
            if let Some(mk) = mark.filter(|m| *m > lo && *m < hi) {
                let my = y(mk);
                painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), my), pos2(main.right(), my)], Stroke::new(1.0, PURPLE.linear_multiply(0.75)), 2.0, 3.0));
                let ax = place(Rect::from_min_size(pos2(main.right() + 2.0, my - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken);
                painter.rect_filled(ax, 3, PURPLE.linear_multiply(0.9));
                painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(mk), mono_m(11.0), Color32::WHITE);
                painter.text(pos2(main.right() - 4.0, my - 7.0), Align2::RIGHT_CENTER, t("chart.mark"), prop(10.0), PURPLE);
            }
        }

        self.drawings_ui(ui, &resp, &painter, &bars, &fr, main, ps, base);

        // sub panes
        let mut top = main.bottom();
        let mut pane_rects = vec![];
        for p in &panes {
            let r = Rect::from_min_size(pos2(rect.left(), top), vec2(plot_w, SUB_H));
            painter.hline(rect.left()..=rect.right(), r.top(), Stroke::new(1.0, line()));
            pane_rects.push((*p, r));
            top += SUB_H;
        }

        // hover index
        let hover = resp.hover_pos().filter(|p| p.x < main.right());
        let hi_idx = hover.map(|p| fr.idx(p.x).round().clamp(i0 as f32, i1 as f32) as usize).unwrap_or(bars.len() - 1);
        let prem = if panes.contains(&Pane::Premium) { premium_lines(a, smarket, vis, self.tf * 60_000, hi_idx.checked_sub(i0)) } else { vec![] };
        let pane_scales: Vec<(Pane, Rect, Option<(f64, f64)>)> = pane_rects.iter().map(|(p, r)| (*p, *r, self.draw_pane(&painter, *p, *r, &bars, i0, i1, &fr, hi_idx, &prem))).collect();

        // time axis
        let ty = top + TIME_H / 2.0;
        painter.hline(rect.left()..=rect.right(), top, Stroke::new(1.0, line()));
        let every = [1usize, 2, 3, 5, 10, 15, 30, 60, 120, 240, 480].into_iter().find(|k| *k as f32 * self.bar_w >= 90.0).unwrap_or(960);
        for (k, b) in vis.iter().enumerate() {
            if ((b.t / (self.tf * 60_000)) as usize) % every != 0 { continue; }
            let x = fr.x((i0 + k) as f32);
            grid_line(&painter, pos2(x, main.top()), pos2(x, top));
            painter.text(pos2(x, ty), Align2::CENTER_CENTER, fmt_time(b.t, every as i64 * self.tf >= 1440), mono(11.5), axis_c());
        }
        painter.vline(main.right(), rect.top()..=rect.bottom(), Stroke::new(1.0, line()));
        // scale toggles in the corner under the price axis: % and log (click again for linear)
        let corner = Rect::from_min_max(pos2(main.right() + 1.0, top + 1.0), pos2(rect.right(), rect.bottom()));
        let ar = Rect::from_min_size(pos2(corner.left() + 50.0, corner.top() + 1.0), vec2(18.0, corner.height() - 2.0));
        let auto_resp = ui.interact(ar, ui.id().with("auto"), Sense::click());
        let auto = self.y_range.is_none();
        if auto || auto_resp.hovered() { painter.rect_filled(ar, 3, hl()); }
        painter.text(ar.center(), Align2::CENTER_CENTER, "A", prop_m(11.0), if auto { accent() } else { mu() });
        if auto_resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(t("chart.auto_tip")).clicked() { self.y_range = None; }
        for (i, (sc, lab)) in [(Scale::Percent, "%"), (Scale::Log, "log")].into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(corner.left() + 2.0 + i as f32 * 22.0, corner.top() + 1.0), vec2(20.0 + i as f32 * 6.0, corner.height() - 2.0));
            let rs = ui.interact(r, ui.id().with(("scale", i)), Sense::click());
            let on = self.scale == sc;
            if on || rs.hovered() { painter.rect_filled(r, 3, hl()); }
            painter.text(r.center(), Align2::CENTER_CENTER, lab, prop_m(11.0), if on { accent() } else { mu() });
            if rs.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.scale = if on { Scale::Linear } else { sc }; }
        }

        // price labels last: a level hidden by an axis tag is skipped, not half covered
        for v in gv.iter().copied() {
            let yy = y(v);
            if taken.iter().any(|t| t.y_range().intersects(egui::Rangef::new(yy - 6.0, yy + 6.0)) && t.left() > main.right()) { continue; }
            painter.text(pos2(main.right() + 8.0, yy), Align2::LEFT_CENTER, axis_label(v), mono(11.5), if round(v) { mu() } else { axis_c() });
        }

        // crosshair
        if let Some(p) = hover {
            let x = fr.x(hi_idx as f32);
            // dashed and faint: the crosshair must not hide the candles and lines under it
            let cross = Stroke::new(1.0, mu().linear_multiply(0.55));
            painter.extend(egui::Shape::dashed_line(&[pos2(x, rect.top()), pos2(x, top)], cross, 4.0, 4.0));
            if main_plot.contains(p) {
                // magnet: within 12px of the hovered bar's open / high / low / close the line sits on it
                let hb = &bars[hi_idx];
                let py = if self.magnet {
                    [hb.o, hb.h, hb.l, hb.c].into_iter().map(|v| y(v)).filter(|yy| (yy - p.y).abs() < 12.0)
                        .min_by(|a, b| (a - p.y).abs().total_cmp(&(b - p.y).abs())).unwrap_or(p.y)
                } else { p.y };
                painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), py), pos2(main.right(), py)], cross, 4.0, 4.0));
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, py - 9.0), vec2(AXIS_W - 2.0, 18.0));
                painter.rect_filled(lab, 2, cross_bg());
                painter.text(lab.left_center() + vec2(7.0, 0.0), Align2::LEFT_CENTER, axis_label(ps.v(py)), mono_m(11.5), Color32::WHITE);
                // TradingView's "+": puts the crosshair price into the order panel
                if self.trading && live && !editing {
                    let pb = Rect::from_min_size(pos2(main.right() - 22.0, py - 9.0), vec2(20.0, 18.0));
                    let pr = ui.interact(pb, ui.id().with("fill_px"), Sense::click());
                    painter.rect_filled(pb, 3, if pr.hovered() { accent() } else { cross_bg() });
                    painter.text(pb.center(), Align2::CENTER_CENTER, "+", prop(14.0), Color32::WHITE);
                    if pr.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(t("tr.fill_px")).clicked() { self.actions.push(ChartAct::Fill(ps.v(py))); }
                }
            }
            // over a sub-pane: its own horizontal line and value on the axis
            for (_, r, sc) in &pane_scales {
                let (Some((plo, phi)), true) = (sc, r.contains(p)) else { continue };
                let inner = Rect::from_min_max(pos2(r.left(), r.top() + 18.0), pos2(r.right(), r.bottom() - 4.0));
                if !(p.y >= inner.top() && p.y <= inner.bottom()) { continue; }
                painter.extend(egui::Shape::dashed_line(&[pos2(r.left(), p.y), pos2(r.right(), p.y)], cross, 4.0, 4.0));
                let v = plo + (inner.bottom() - p.y) as f64 / inner.height() as f64 * (phi - plo);
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, p.y - 8.0), vec2(AXIS_W - 2.0, 16.0));
                painter.rect_filled(lab, 2, cross_bg());
                painter.text(lab.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_big(v), mono_m(11.0), Color32::WHITE);
            }
            let g = painter.layout_no_wrap(fmt_time_full(bars[hi_idx].t), mono_m(11.5), Color32::WHITE);
            let tl = Rect::from_center_size(pos2(x, ty), vec2(g.size().x + 16.0, TIME_H - 2.0));
            painter.rect_filled(tl, 2, cross_bg());
            painter.galley(tl.center() - g.size() / 2.0, g, Color32::WHITE);
        }

        // legend: row 1 time + OHLC + change + range (labels muted, values colored), row 2 MAs
        let b = &bars[hi_idx];
        let chg = (b.c / b.o - 1.0) * 100.0;
        let range = if b.l > 0.0 { (b.h / b.l - 1.0) * 100.0 } else { 0.0 };
        let c = if b.c >= b.o { up() } else { dn() };
        // design: text-only rows, 12 medium, grey keys, colored tabular values, 14 px apart
        let kv = |x: f32, yy: f32, k: &str, v: String, col: Color32| -> f32 {
            let r = painter.text(pos2(x, yy), Align2::LEFT_CENTER, k, prop_m(12.0), dim());
            painter.text(pos2(r.right() + 4.0, yy), Align2::LEFT_CENTER, v, mono_m(12.0), col).right() + 14.0
        };
        let y1 = main.top() + 14.0;
        let mut lx = main.left() + 6.0;
        if self.multi {
            let tf = TFS.iter().find(|(m, _)| *m == self.tf).map_or("", |x| x.1);
            lx = painter.text(pos2(lx, y1), Align2::LEFT_CENTER, tf, prop_s(12.5), fg_strong()).right() + 12.0;
        }
        lx = painter.text(pos2(lx, y1), Align2::LEFT_CENTER, fmt_time_full(b.t), mono_m(12.0), mu()).right() + 14.0;
        for (k, v) in [("chart.o", b.o), ("chart.h", b.h), ("chart.l", b.l), ("chart.c", b.c)] { lx = kv(lx, y1, t(k), fmt_px(v), c); }
        lx = kv(lx, y1, t("chart.chg"), format!("{chg:+.2}%"), c);
        kv(lx, y1, t("chart.range"), format!("{range:.2}%"), c);
        let y2 = main.top() + 33.0;
        let mut lx = main.left() + 6.0;
        for (m, l) in &mas {
            lx = kv(lx, y2, &format!("{}({})", if l.ema { "EMA" } else { "MA" }, l.n), m[hi_idx].map(fmt_px).unwrap_or("-".into()), rgb(l.col));
        }
        if lmax > 0.0 {
            kv(lx, y2, t("chart.liq"), format!("{} / {}", fmt_big(b.liq_long), fmt_big(b.liq_short)), dim());
        }
        // the overlay row moves up when there is no MA / liquidation row
        let y3 = main.top() + if mas.is_empty() && lmax <= 0.0 { 33.0 } else { 52.0 };
        let mut lx = main.left() + 6.0;
        for (k, v, col) in &legend3 {
            painter.circle_filled(pos2(lx + 3.0, y3), 3.0, *col);
            lx = kv(lx + 10.0, y3, k, v.clone(), *col);
        }
        if loading_tf {
            painter.text(pos2(main.left() + 6.0, main.top() + 80.0), Align2::LEFT_CENTER, t("chart.loading"), prop_m(12.0), WARN);
        }
        self.replay_ui(ui, &resp, &painter, main_plot, &fr, hover.map(|_| bars[hi_idx].t), next_t, now);
        if let Some((msg, until)) = self.toast.clone() {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if !left.is_zero() {
                // slides down and fades in over 250 ms, fades out over the last 400 ms
                let age = 8.0 - left.as_secs_f32();
                let t = (age / 0.25).min(1.0).min(left.as_secs_f32() / 0.4);
                let ease = 1.0 - (1.0 - t).powi(3);
                // design banner: popup surface, accent dot, 12.5 semibold
                let g = painter.layout_no_wrap(msg, prop_s(12.5), fg_strong().gamma_multiply(ease));
                let r = Rect::from_center_size(pos2(main_plot.center().x, main_plot.top() + 6.0 + 14.0 * ease), g.size() + vec2(42.0, 20.0));
                painter.add(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(((if light() { 40.0 } else { 115.0 }) * ease) as u8) }.as_shape(r, 6));
                painter.rect(r, 6, popup_bg().gamma_multiply(ease), Stroke::new(1.0, line().gamma_multiply(ease)), egui::StrokeKind::Inside);
                painter.circle_filled(pos2(r.left() + 16.0, r.center().y), 4.0, accent().gamma_multiply(ease));
                painter.galley(pos2(r.left() + 30.0, r.center().y - g.size().y / 2.0), g, fg_strong().gamma_multiply(ease));
                if t < 1.0 { ui.ctx().request_repaint(); } else { ui.ctx().request_repaint_after(left.saturating_sub(std::time::Duration::from_millis(400))); }
            } else { self.toast = None; }
        }
    }

    /// Replay: picking the start bar (vertical line under the pointer, click to start), then the
    /// play / step / speed / exit bar at the bottom of the plot; Right arrow steps one bar.
    #[allow(clippy::too_many_arguments)]
    fn replay_ui(&mut self, ui: &mut Ui, resp: &egui::Response, painter: &egui::Painter, plot: Rect, fr: &Frame, hover_t: Option<i64>, next_t: Option<i64>, now: f64) {
        let Some(r) = self.replay.as_mut() else { return };
        if r.at.is_none() {
            if let (Some(p), Some(ht)) = (resp.hover_pos().filter(|p| plot.contains(*p)), hover_t) {
                let x = fr.x(fr.idx(p.x).round());
                painter.vline(x, plot.top()..=plot.bottom(), Stroke::new(1.5, accent()));
                painter.rect_filled(Rect::from_min_max(pos2(x, plot.top()), plot.right_bottom()), 0, panel().gamma_multiply(0.6));
                if resp.clicked() { r.at = Some(ht); r.last = now; self.right = 0.0; }
            }
        } else if resp.hovered() && ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            if let Some(n) = next_t { r.at = Some(n); }
        }
        // a floating control bar, styled as a popup (radius 6, hairline, soft shadow)
        let w = if r.at.is_some() { 252.0 } else { painter.layout_no_wrap(t("replay.pick").to_string(), prop_m(12.5), fg()).size().x + 64.0 };
        let bar = Rect::from_center_size(pos2(plot.center().x, plot.bottom() - 26.0), vec2(w, 36.0));
        painter.add(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if light() { 32 } else { 120 }) }.as_shape(bar, 6));
        painter.rect(bar, 6, popup_bg(), Stroke::new(1.0, line()), egui::StrokeKind::Inside);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(4.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 2.0;
        let mut exit = false;
        if r.at.is_some() {
            let playing = r.playing;
            if kit::icon_button(&mut child, kit::H, false, |p, rr, c| {
                let m = rr.center();
                if playing { for dx in [-2.5f32, 2.5] { p.rect_filled(Rect::from_center_size(m + vec2(dx, 0.0), vec2(2.5, 10.0)), 1, c); } }
                else { p.add(egui::Shape::convex_polygon(vec![m + vec2(-3.5, -5.5), m + vec2(5.5, 0.0), m + vec2(-3.5, 5.5)], c, Stroke::NONE)); }
            }).on_hover_text(t("replay.play")).clicked() { r.playing = !r.playing; r.last = now; }
            if kit::icon_button(&mut child, kit::H, false, |p, rr, c| {
                let m = rr.center();
                p.add(egui::Shape::convex_polygon(vec![m + vec2(-4.5, -5.0), m + vec2(3.0, 0.0), m + vec2(-4.5, 5.0)], c, Stroke::NONE));
                p.rect_filled(Rect::from_center_size(m + vec2(4.5, 0.0), vec2(2.0, 10.0)), 0, c);
            }).on_hover_text(t("replay.step")).clicked() { if let Some(n) = next_t { r.at = Some(n); } }
            if kit::button(&mut child, &format!("{}x", SPEEDS[r.speed]), kit::Look::Ghost, kit::H, Some(40.0), true).on_hover_text(t("replay.speed")).clicked() { r.speed = (r.speed + 1) % SPEEDS.len(); }
            child.add_space(6.0);
            child.label(RichText::new(t("replay.title")).font(prop_m(12.5)).color(mu()));
        } else {
            child.add_space(8.0);
            child.label(RichText::new(t("replay.pick")).font(prop_m(12.5)).color(fg()));
        }
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let x = kit::icon_button(ui, kit::H, false, |p, rr, c| {
                let m = rr.center();
                p.line_segment([m + vec2(-4.0, -4.0), m + vec2(4.0, 4.0)], Stroke::new(1.4, c));
                p.line_segment([m + vec2(-4.0, 4.0), m + vec2(4.0, -4.0)], Stroke::new(1.4, c));
            });
            if x.on_hover_text(t("replay.exit")).clicked() { exit = true; }
        });
        if exit { self.replay = None; }
    }

    #[allow(clippy::too_many_arguments)]
    /// Returns the value range the pane was drawn with (for the crosshair), None when it drew nothing.
    fn draw_pane(&self, painter: &egui::Painter, p: Pane, r: Rect, bars: &[Bar], i0: usize, i1: usize, fr: &Frame, hi: usize, prem: &[Prem]) -> Option<(f64, f64)> {
        let (vis, hb) = (&bars[i0..=i1], &bars[hi]);
        // the top 18px hold the legend, so lines never run through it
        let inner = Rect::from_min_max(pos2(r.left(), r.top() + 18.0), pos2(r.right(), r.bottom() - 4.0));
        let label = |s: String, col: Color32, x: f32| painter.text(pos2(x, r.top() + 10.0), Align2::LEFT_CENTER, s, mono_m(11.5), col).right() + 12.0;
        // design: pane name 12 medium grey, values tabular
        let mut lx = painter.text(pos2(r.left() + 6.0, r.top() + 10.0), Align2::LEFT_CENTER, t(p.key()), prop_m(12.0), mu()).right() + 8.0;
        let scale = std::cell::Cell::new(None);
        let axis = |lo: f64, hi: f64, fmt: &dyn Fn(f64) -> String| {
            scale.set(Some((lo, hi)));
            painter.text(pos2(r.right() + 8.0, inner.top() + 4.0), Align2::LEFT_CENTER, fmt(hi), mono(11.0), axis_c());
            painter.text(pos2(r.right() + 8.0, inner.bottom() - 4.0), Align2::LEFT_CENTER, fmt(lo), mono(11.0), axis_c());
        };
        let line = |vals: Vec<(usize, f64)>, lo: f64, hi: f64, col: Color32, step: bool| {
            let mut pts: Vec<Pos2> = vec![];
            for (k, v) in vals {
                let pt = pos2(fr.x((i0 + k) as f32), y_of(v, lo, hi, inner));
                if step { if let Some(prev) = pts.last().copied() { pts.push(pos2(pt.x, prev.y)); } }
                pts.push(pt);
            }
            if pts.len() > 1 { painter.add(egui::Shape::line(pts, Stroke::new(1.3, col))); }
        };
        let range = |vals: &[f64], zero: bool| padded_range(vals.iter().copied(), 0.08, zero);
        let zero_line = |lo: f64, hi: f64| { if lo < 0.0 && hi > 0.0 { painter.hline(r.left()..=r.right(), y_of(0.0, lo, hi, inner), Stroke::new(1.0, super::theme::line())); } };
        match p {
            Pane::Vol => {
                let max = vis.iter().map(|b| b.vol).fold(0.0, f64::max).max(1e-12);
                let bw = (self.bar_w * 0.7).max(1.0);
                for (k, b) in vis.iter().enumerate() {
                    let x = fr.x((i0 + k) as f32);
                    let h = (b.vol / max) as f32 * inner.height();
                    let base = inner.bottom();
                    if b.buy + b.sell > 0.0 {
                        let hb = h * (b.buy / (b.buy + b.sell)) as f32;
                        painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, base - hb), pos2(x + bw / 2.0, base)), 0, up().linear_multiply(0.6));
                        painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, base - h), pos2(x + bw / 2.0, base - hb)), 0, dn().linear_multiply(0.6));
                    } else {
                        painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, base - h), pos2(x + bw / 2.0, base)), 0, dim());
                    }
                }
                let vols: Vec<f64> = vis.iter().map(|b| b.vol).collect();
                // volume averages follow the MA toggle: off by default, the bars speak for themselves
                for (n, col) in [(5usize, accent()), (10, dn())].into_iter().filter(|_| self.ma) {
                    let m = sma(&vols, n);
                    line(m.iter().enumerate().filter_map(|(k, v)| Some((k, (*v)?))).collect(), 0.0, max, col.linear_multiply(0.9), false);
                }
                lx = label(format!("{} {}", fmt_big(hb.vol), self.base_unit), fg(), lx);
                lx = label(format!("≈{} USDT", fmt_big(hb.vol * hb.c)), mu(), lx);
                if hb.buy + hb.sell > 0.0 { label(format!("B {} / S {}", fmt_big(hb.buy), fmt_big(hb.sell)), mu(), lx); }
                axis(0.0, max, &fmt_big);
            }
            Pane::Cvd | Pane::Oi | Pane::Ls | Pane::Basis => {
                let get = |b: &Bar| -> Option<f64> { match p { Pane::Cvd => Some(b.cvd), Pane::Oi => b.oi, Pane::Ls => b.ls, _ => b.basis_bps } };
                let vals: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, get(b)?))).collect();
                if vals.is_empty() { return None; }
                let (lo, hi) = range(&vals.iter().map(|x| x.1).collect::<Vec<_>>(), p == Pane::Basis);
                zero_line(lo, hi);
                let col = match p { Pane::Cvd => accent(), Pane::Oi => WARN, Pane::Ls => PURPLE, _ => fg() };
                line(vals, lo, hi, col, false);
                let fmt: &dyn Fn(f64) -> String = match p { Pane::Ls => &|v| format!("{v:.3}"), Pane::Basis => &|v| format!("{v:+.2}bp"), _ => &fmt_big };
                if let Some(v) = get(hb) { label(fmt(v), col, lx); }
                axis(lo, hi, fmt);
            }
            Pane::Premium => {
                // each venue against the composite; the axis ignores the 2% most extreme points so one
                // stale feed cannot flatten the rest
                let mut all: Vec<f64> = prem.iter().flat_map(|l| l.vals.iter().flatten().copied()).collect();
                if all.is_empty() { return None; }
                all.sort_by(f64::total_cmp);
                let n = all.len();
                let (lo, hi) = range(&[all[n / 50], all[(n - 1) - n / 50]], true);
                zero_line(lo, hi);
                for l in prem {
                    let mut pts: Vec<Pos2> = vec![];
                    let flush = |pts: &mut Vec<Pos2>| {
                        let pts = std::mem::take(pts);
                        if pts.len() > 1 { painter.add(egui::Shape::line(pts, Stroke::new(if l.odd { 1.6 } else { 1.0 }, l.col.linear_multiply(if l.odd { 1.0 } else { 0.55 })))); }
                    };
                    for (k, v) in l.vals.iter().enumerate() {
                        match v {
                            Some(v) => pts.push(pos2(fr.x((i0 + k) as f32), y_of(v.clamp(lo, hi), lo, hi, inner))),
                            None => flush(&mut pts),
                        }
                    }
                    flush(&mut pts);
                }
                // legend: venues furthest from their usual premium first; "usual" only when it matters
                let mut leg: Vec<&Prem> = prem.iter().filter(|l| l.hover.is_some()).collect();
                leg.sort_by(|x, y| y.dev().abs().total_cmp(&x.dev().abs()));
                for l in leg {
                    let v = l.hover.unwrap_or(0.0);
                    let s = if l.odd { format!("{} {v:+.1} ({:+.1} vs usual)", l.name, l.dev()) } else { format!("{} {v:+.1}", l.name) };
                    if lx > r.right() - 60.0 { break; }
                    lx = label(s, l.col, lx);
                }
                axis(lo, hi, &|v| format!("{v:+.1}bp"));
            }
            // inferred from L2 book updates (live only): OFI is top-of-book pressure, book flow is the
            // net liquidity each side gained within 0.25% of mid (adds minus cancels)
            // per-bar taker delta (buys minus sells) has the same histogram shape
            Pane::Ofi | Pane::BookFlow | Pane::Delta => {
                let net = |b: &Bar| match p { Pane::Ofi => b.flow.ofi, Pane::Delta => b.buy - b.sell, _ => { let (nb, na) = b.book_net(); nb - na } };
                let vals: Vec<f64> = vis.iter().map(net).collect();
                let max = vals.iter().fold(0.0f64, |m, v| m.max(v.abs())).max(1e-12);
                let (lo, hi) = (-max * 1.08, max * 1.08);
                let zy = y_of(0.0, lo, hi, inner);
                painter.hline(r.left()..=r.right(), zy, Stroke::new(1.0, super::theme::line()));
                let bw = (self.bar_w * 0.7).max(1.0);
                for (k, v) in vals.iter().enumerate().filter(|(_, v)| **v != 0.0) {
                    let x = fr.x((i0 + k) as f32);
                    let col = if *v > 0.0 { up() } else { dn() }.linear_multiply(0.7);
                    painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, zy.min(y_of(*v, lo, hi, inner))), pos2(x + bw / 2.0, zy.max(y_of(*v, lo, hi, inner)))), 0, col);
                }
                let v = net(hb);
                lx = label(format!("{v:+.2} {}", self.base_unit), if v >= 0.0 { up() } else { dn() }, lx);
                if p == Pane::BookFlow {
                    let (nb, na) = hb.book_net();
                    label(format!("{} {:+.1} / {} {:+.1}", t("tr.bid"), nb, t("tr.ask"), na), mu(), lx);
                }
                axis(lo, hi, &fmt_big);
            }
            // oscillators run over every loaded bar so the left edge is warmed up
            Pane::Rsi => {
                let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
                let v = rsi(&closes, self.ind.rsi.max(1));
                let (lo, hi_) = (0.0, 100.0);
                let (y70, y30) = (y_of(70.0, lo, hi_, inner), y_of(30.0, lo, hi_, inner));
                painter.rect_filled(Rect::from_x_y_ranges(r.left()..=r.right(), y70..=y30), 0, PURPLE.gamma_multiply(0.07));
                for yy in [y70, y30, y_of(50.0, lo, hi_, inner)] { painter.hline(r.left()..=r.right(), yy, Stroke::new(1.0, super::theme::line())); }
                line((i0..=i1).filter_map(|i| Some((i - i0, v[i]?))).collect(), lo, hi_, PURPLE, false);
                if let Some(x) = v[hi] { label(format!("{}  {x:.2}", self.ind.rsi), PURPLE, lx); }
                axis(lo, hi_, &|x| format!("{x:.0}"));
            }
            Pane::Macd => {
                let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
                let [mf, ms, mg] = self.ind.macd.map(|x| x.max(1));
                let m = macd(&closes, mf, ms, mg);
                let seen: Vec<(f64, f64, f64)> = m[i0..=i1].iter().flatten().copied().collect();
                if seen.is_empty() { return None; }
                let max = seen.iter().fold(0.0f64, |a, (l, s, h)| a.max(l.abs()).max(s.abs()).max(h.abs())).max(1e-12);
                let (lo, hi_) = (-max * 1.08, max * 1.08);
                let zy = y_of(0.0, lo, hi_, inner);
                painter.hline(r.left()..=r.right(), zy, Stroke::new(1.0, super::theme::line()));
                let bw = (self.bar_w * 0.7).max(1.0);
                // histogram: bright while growing away from zero, faded while shrinking (TradingView)
                for i in i0..=i1 {
                    let Some((_, _, h)) = m[i] else { continue };
                    let prev = i.checked_sub(1).and_then(|j| m[j]).map_or(h, |x| x.2);
                    let col = if h >= 0.0 { up() } else { dn() }.gamma_multiply(if h.abs() >= prev.abs() { 0.8 } else { 0.4 });
                    let (x, yh) = (fr.x(i as f32), y_of(h, lo, hi_, inner));
                    painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, zy.min(yh)), pos2(x + bw / 2.0, zy.max(yh))), 0, col);
                }
                line((i0..=i1).filter_map(|i| Some((i - i0, m[i]?.0))).collect(), lo, hi_, rgb(0x2962ff), false);
                line((i0..=i1).filter_map(|i| Some((i - i0, m[i]?.1))).collect(), lo, hi_, rgb(0xff6d00), false);
                if let Some((l, s, h)) = m[hi] {
                    let f = |v: f64| fmt_dp(v, (3.0 - max.log10().floor()).clamp(0.0, 8.0) as usize);
                    lx = label(format!("{mf} {ms} {mg}"), mu(), lx);
                    lx = label(f(h), if h >= 0.0 { up() } else { dn() }, lx);
                    lx = label(f(l), rgb(0x2962ff), lx);
                    label(f(s), rgb(0xff6d00), lx);
                }
                axis(lo, hi_, &|v| fmt_dp(v, (3.0 - max.log10().floor()).clamp(0.0, 8.0) as usize));
            }
            Pane::Funding => {
                let pred: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, b.funding_h? * 1e4))).collect();
                let sett: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, b.funding_settled? * 1e4))).collect();
                let all: Vec<f64> = pred.iter().chain(sett.iter()).map(|x| x.1).collect();
                if all.is_empty() { return None; }
                let (lo, hi) = range(&all, true);
                zero_line(lo, hi);
                line(sett, lo, hi, WARN, true);
                line(pred, lo, hi, accent(), false);
                if let Some(v) = hb.funding_h { lx = label(format!("{} {:+.4}", t("chart.funding_pred"), v * 1e4), accent(), lx); }
                if let Some(v) = hb.funding_settled { label(format!("{} {:+.4} bp/h", t("chart.funding_settled"), v * 1e4), WARN, lx); }
                axis(lo, hi, &|v| format!("{v:+.3}"));
            }
        }
        scale.get()
    }

    /// Records each finished drawing edit (a whole drag is one step) and applies Cmd+Z / Cmd+Shift+Z.
    fn undo_redo(&mut self, ui: &Ui, base: &str) {
        let cur = self.drawings.get(base).cloned().unwrap_or_default();
        match &self.committed {
            Some((b, prev)) if b == base => {
                if *prev != cur && self.drag.is_none() {
                    self.undo.push(prev.clone());
                    if self.undo.len() > 100 { self.undo.remove(0); }
                    self.redo.clear();
                    self.committed = Some((base.to_string(), cur.clone()));
                }
            }
            // another symbol: its history starts fresh
            _ => { self.undo.clear(); self.redo.clear(); self.committed = Some((base.to_string(), cur.clone())); }
        }
        let (undo, redo) = ui.input(|i| {
            let z = i.modifiers.command && i.key_pressed(egui::Key::Z);
            (z && !i.modifiers.shift, z && i.modifiers.shift)
        });
        let (from, to) = if undo { (&mut self.undo, &mut self.redo) } else if redo { (&mut self.redo, &mut self.undo) } else { return };
        let Some(prev) = from.pop() else { return };
        to.push(cur);
        self.drawings.insert(base.to_string(), prev.clone());
        self.committed = Some((base.to_string(), prev));
        self.hot = None;
        self.menu = None;
    }

    /// Place, draw, edit and delete drawings. Click places points for the armed tool (one for a
    /// horizontal line or a note, three for a channel, two otherwise); drag a handle to move that
    /// point or the body to move all of it; right-click opens the drawing's menu (color, style,
    /// alert, lock, delete) or, on the empty plot, the chart menu; Delete removes; Escape disarms.
    #[allow(clippy::too_many_arguments)]
    fn drawings_ui(&mut self, ui: &Ui, resp: &egui::Response, painter: &egui::Painter, bars: &[Bar], fr: &Frame, main: Rect, ps: PScale, base: &str) {
        let ms = self.tf * 60_000;
        let plot = ps.r;
        let y = |v: f64| ps.y(v);
        let to_screen = |(t, p): (i64, f64)| pos2(fr.x(idx_of(bars, t, ms)), y(p));
        let to_point = |p: Pos2| (time_of(bars, fr.idx(p.x), ms), ps.v(p.y));
        let typing = ui.ctx().egui_wants_keyboard_input();
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) { self.tool = None; self.pending.clear(); }
        if !typing { self.undo_redo(ui, base); }

        let ptr = resp.hover_pos().filter(|p| plot.contains(*p));
        let (hidden, sound, equity, risk_pct) = (self.hide, self.alert_sound.clone(), self.equity, self.risk_pct);
        let list = self.drawings.entry(base.to_string()).or_default();
        // a channel's middle handle: halfway along its parallel line
        let mid_c = |d: &Drawing| ((d.a.0 + d.b.0) / 2, (d.a.1 + d.b.1) / 2.0 + d.offset());

        // editing: the part grabbed at drag start follows the pointer
        if let (Some((i, part)), true) = (self.drag, resp.dragged()) {
            if let Some(d) = list.get_mut(i) {
                let delta = resp.drag_delta();
                let mv = |pt: (i64, f64)| to_point(to_screen(pt) + delta);
                match part {
                    0 => d.a = mv(d.a),
                    1 => d.b = mv(d.b),
                    3 if d.tool == Tool::Position => d.c.1 = mv((d.a.0, d.c.1)).1,
                    3 => { let m = mid_c(d); d.c = (m.0, mv(m).1); }
                    _ => { d.a = mv(d.a); d.b = mv(d.b); if matches!(d.tool, Tool::Channel | Tool::Position) { d.c = mv(d.c); } }
                }
                if d.tool == Tool::Position { d.c.0 = d.a.0; }
                if matches!(d.tool, Tool::HLine | Tool::Text) { d.b = d.a; }
            }
        }

        // a position tool's horizontal extent (at least 120 px wide)
        let pos_x = |d: &Drawing| { let (x0, x1) = (to_screen(d.a).x, to_screen(d.b).x); (x0.min(x1), x0.max(x1).max(x0.min(x1) + 120.0)) };
        // screen segments of a drawing (for drawing and hit-testing)
        let segs = |d: &Drawing| -> Vec<(Pos2, Pos2)> {
            let (a, b) = (to_screen(d.a), to_screen(d.b));
            match d.tool {
                Tool::HLine => vec![(pos2(main.left(), a.y), pos2(main.right(), a.y))],
                Tool::Trend | Tool::Measure => vec![(a, b)],
                Tool::Ray => { let dir = b - a; let k = if dir.length() > 0.0 { 4000.0 / dir.length() } else { 0.0 }; vec![(a, a + dir * k)] }
                Tool::Rect => { let r = Rect::from_two_pos(a, b); vec![(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] }
                Tool::Fib => FIB.iter().map(|f| { let yy = y(d.b.1 + (d.a.1 - d.b.1) * f); (pos2(a.x.min(b.x), yy), pos2(a.x.max(b.x), yy)) }).collect(),
                Tool::Channel => { let o = d.offset(); vec![(a, b), (to_screen((d.a.0, d.a.1 + o)), to_screen((d.b.0, d.b.1 + o)))] }
                Tool::Text => vec![],
                Tool::Position => { let (x0, x1) = pos_x(d); [d.a.1, d.b.1, d.c.1].iter().map(|p| (pos2(x0, y(*p)), pos2(x1, y(*p)))).collect() }
            }
        };
        let note = |d: &Drawing| painter.layout_no_wrap(if d.text.is_empty() { t("draw.text_hint").to_string() } else { d.text.clone() }, prop(13.0), d.color());
        let note_rect = |d: &Drawing, g: &egui::Galley| { let a = to_screen(d.a); Rect::from_min_size(pos2(a.x, a.y - g.size().y), g.size()).expand(4.0) };
        let handles = |d: &Drawing| -> Vec<(u8, Pos2)> {
            match d.tool {
                Tool::HLine | Tool::Text => vec![],
                Tool::Channel => vec![(0, to_screen(d.a)), (1, to_screen(d.b)), (3, to_screen(mid_c(d)))],
                Tool::Position => { let x1 = pos_x(d).1; vec![(0, to_screen(d.a)), (1, pos2(x1, y(d.b.1))), (3, pos2(to_screen(d.a).x, y(d.c.1)))] }
                _ => vec![(0, to_screen(d.a)), (1, to_screen(d.b))],
            }
        };
        // part under the pointer: a handle first, then the nearest line (or a box's inside); locked
        // drawings only answer right-clicks
        let hot = ptr.filter(|_| self.tool.is_none() && !hidden).and_then(|p| {
            for (i, d) in list.iter().enumerate().rev().filter(|(_, d)| !d.locked) {
                for (k, q) in handles(d) { if (q - p).length() < 7.0 { return Some((i, k)); } }
            }
            list.iter().enumerate().filter_map(|(i, d)| {
                let dist = segs(d).iter().map(|(a, b)| dist_to_segment(p, *a, *b)).fold(f32::MAX, f32::min);
                let inside = match d.tool {
                    Tool::Rect | Tool::Measure => Rect::from_two_pos(to_screen(d.a), to_screen(d.b)).contains(p),
                    Tool::Text => note_rect(d, &note(d)).contains(p),
                    Tool::Position => { let (x0, x1) = pos_x(d); Rect::from_x_y_ranges(x0..=x1, y(d.b.1).min(y(d.c.1))..=y(d.b.1).max(y(d.c.1))).contains(p) }
                    _ => false,
                };
                (dist < 6.0 || inside).then_some((i, if inside { 5.9 } else { dist }))
            }).min_by(|x, z| x.1.total_cmp(&z.1)).map(|(i, _)| (i, if list[i].locked { LOCKED } else { 2 }))
        });
        let hot = self.drag.or(hot);

        let click = resp.interact_pointer_pos().filter(|p| plot.contains(*p));
        if let (Some(tool), true, Some(p)) = (self.tool, resp.clicked(), click) {
            self.pending.push(to_point(p));
            if self.pending.len() >= tool.points() {
                let pts = std::mem::take(&mut self.pending);
                list.push(Drawing::placed(tool, &pts));
                self.tool = None;
                // a note is typed right away
                if tool == Tool::Text { self.menu = Some((Menu::Drawing(list.len() - 1, true), p)); }
            }
        } else if resp.secondary_clicked() {
            if self.tool.is_some() { self.tool = None; self.pending.clear(); }
            else if let Some(p) = click {
                self.menu = Some((match hot { Some((i, _)) => Menu::Drawing(i, false), None => Menu::Chart(to_point(p)) }, p));
            }
        } else if let (Some((i, part)), None, false) = (hot, self.drag, typing) {
            if part != LOCKED && ui.input(|x| x.key_pressed(egui::Key::Delete) || x.key_pressed(egui::Key::Backspace)) {
                list.remove(i);
                self.hot = None;
                self.menu = None;
                return;
            }
        }

        // the right-click menu (egui's menu popup, so the sound submenu does not close it)
        if let Some((m, at)) = self.menu {
            let mut open = true;
            let mut del = false;
            egui::Popup::menu(resp).id(ui.id().with("chart_menu")).at_position(at).open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                menu_rows(ui, 220.0);
                match m {
                    Menu::Drawing(i, focus) => {
                        let Some(d) = list.get_mut(i) else { ui.close(); return };
                        if d.tool == Tool::Text {
                            let r = ui.add(egui::TextEdit::singleline(&mut d.text).hint_text(t("draw.text_hint")).desired_width(200.0));
                            if focus { r.request_focus(); }
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) { ui.close(); }
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for c in DRAW_COLORS {
                                let (r, sw) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                                ui.painter().rect_filled(r.shrink(1.0), 3, rgb(c));
                                if d.color() == rgb(c) { ui.painter().rect_stroke(r, 3, Stroke::new(1.5, fg()), egui::StrokeKind::Inside); }
                                if sw.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { d.col = c; }
                            }
                        });
                        if !matches!(d.tool, Tool::Text | Tool::Measure) {
                            ui.horizontal(|ui| {
                                for (k, lab) in [(0u8, "draw.solid"), (1, "draw.dashed"), (2, "draw.dotted")] {
                                    if ui.selectable_label(d.dash == k, t(lab)).clicked() { d.dash = k; }
                                }
                            });
                        }
                        if d.alertable() {
                            menu_sep(ui);
                            let mut on = d.alert.is_some();
                            if ui.checkbox(&mut on, t("alert.on")).on_hover_text(t("alert.tip")).changed() { d.alert = on.then(|| sound.clone()); }
                            if let Some(s) = &mut d.alert { sound_menu(ui, s); }
                        }
                        if d.tool == Tool::Position {
                            menu_sep(ui);
                            ui.horizontal(|ui| { ui.label(t("pos.risk")); ui.add(egui::DragValue::new(&mut self.risk_pct).range(0.05..=100.0).speed(0.05).suffix("%")); });
                            if self.trading && ui.button(t("pos.use")).clicked() { self.actions.push(ChartAct::Fill(d.a.1)); ui.close(); }
                        }
                        menu_sep(ui);
                        ui.checkbox(&mut d.locked, t("draw.lock"));
                        if ui.button(t("draw.delete")).clicked() { del = true; ui.close(); }
                        if focus { self.menu = Some((Menu::Drawing(i, false), at)); }
                    }
                    Menu::Chart((ts, px)) => {
                        if ui.button(format!("{}  {}", t("alert.add_at"), fmt_px(px))).clicked() {
                            let mut d = Drawing::new(Tool::HLine, (ts, px), (ts, px));
                            d.alert = Some(sound.clone());
                            list.push(d);
                            ui.close();
                        }
                        if ui.button(format!("{}  {}", t("draw.hline_at"), fmt_px(px))).clicked() { list.push(Drawing::new(Tool::HLine, (ts, px), (ts, px))); ui.close(); }
                        if ui.button(t("replay.from_here")).clicked() { self.replay = Some(Replay::new(Some(ts), ui.input(|i| i.time))); ui.close(); }
                        menu_sep(ui);
                        if ui.button(t("chart.reset")).clicked() { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; ui.close(); }
                    }
                }
            });
            if del { if let Some((Menu::Drawing(i, _), _)) = self.menu { if i < list.len() { list.remove(i); } } }
            if !open || del { self.menu = None; }
        }
        if hidden { self.hot = None; return; }

        let clip = painter.with_clip_rect(plot);
        let draw = |d: &Drawing, emph: bool, alpha: f32| {
            let col = d.color().linear_multiply(alpha);
            let s = Stroke::new(if emph { 2.0 } else { 1.2 }, col);
            let seg = |p0: Pos2, p1: Pos2, s: Stroke| match d.dash {
                0 => { clip.line_segment([p0, p1], s); }
                k => { let (on, off) = if k == 1 { (6.0, 4.0) } else { (1.5, 3.0) }; clip.extend(egui::Shape::dashed_line(&[p0, p1], s, on, off)); }
            };
            let (a, b) = (to_screen(d.a), to_screen(d.b));
            match d.tool {
                Tool::Rect => {
                    let r = Rect::from_two_pos(a, b);
                    clip.rect_filled(r, 0, col.linear_multiply(0.12));
                    for (p0, p1) in segs(d) { seg(p0, p1, s); }
                }
                Tool::Fib => {
                    let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
                    let px = |f: f64| d.b.1 + (d.a.1 - d.b.1) * f;
                    let fib_col = |f: f64| match f { f if f == 0.5 => fg().linear_multiply(0.7), f if f == 0.618 => WARN, f if f == 0.0 || f == 1.0 => mu(), _ => accent() };
                    for w in FIB.windows(2) {
                        clip.rect_filled(Rect::from_two_pos(pos2(x0, y(px(w[0]))), pos2(x1, y(px(w[1])))), 0, fib_col(w[1]).linear_multiply(0.06 * alpha));
                    }
                    for f in FIB {
                        let yy = y(px(f));
                        seg(pos2(x0, yy), pos2(x1, yy), Stroke::new(s.width, fib_col(f).linear_multiply(alpha)));
                        clip.text(pos2(x0 + 3.0, yy - 1.0), Align2::LEFT_BOTTOM, format!("{f} ({})", fmt_px(px(f))), mono(10.0), fib_col(f).linear_multiply(alpha));
                    }
                    clip.extend(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, mu().linear_multiply(0.6 * alpha)), 4.0, 4.0));
                }
                Tool::Channel => {
                    let l = segs(d);
                    let (p, q) = (l[0], l[1]);
                    clip.add(egui::Shape::convex_polygon(vec![p.0, p.1, q.1, q.0], col.linear_multiply(0.08), Stroke::NONE));
                    seg(p.0, p.1, s);
                    seg(q.0, q.1, s);
                    clip.extend(egui::Shape::dashed_line(&[p.0 + (q.0 - p.0) / 2.0, p.1 + (q.1 - p.1) / 2.0], Stroke::new(1.0, col.linear_multiply(0.6)), 4.0, 4.0));
                }
                Tool::Measure => {
                    // TradingView's measure: change, percent, bars and time between the two points
                    let r = Rect::from_two_pos(a, b);
                    let dp = d.b.1 - d.a.1;
                    let mc = if dp >= 0.0 { up() } else { dn() }.linear_multiply(alpha);
                    clip.rect_filled(r, 0, mc.linear_multiply(0.15));
                    clip.extend([egui::Shape::line_segment([pos2(r.center().x, a.y), pos2(r.center().x, b.y)], Stroke::new(1.2, mc)),
                                 egui::Shape::line_segment([pos2(a.x, r.center().y), pos2(b.x, r.center().y)], Stroke::new(1.2, mc))]);
                    let n = ((d.b.0 - d.a.0) as f64 / ms as f64).round() as i64;
                    let pct = if d.a.1 != 0.0 { dp / d.a.1 * 100.0 } else { 0.0 };
                    let txt = format!("{}{} ({pct:+.2}%)\n{n} {} · {}", if dp >= 0.0 { "+" } else { "" }, fmt_px(dp), t("draw.bars"), span(n.abs() * ms));
                    let g = painter.layout(txt, mono(11.0), Color32::WHITE, 260.0);
                    let below = b.y >= a.y;
                    let lr = Rect::from_min_size(pos2(r.center().x - g.size().x / 2.0 - 6.0, if below { r.bottom() + 6.0 } else { r.top() - g.size().y - 14.0 }), g.size() + vec2(12.0, 8.0));
                    // kept inside the plot so neither line is clipped
                    let lr = lr.translate(vec2(0.0, (plot.top() + 2.0 - lr.top()).max(0.0) + (plot.bottom() - 2.0 - lr.bottom()).min(0.0)));
                    clip.rect_filled(lr, 4, mc.linear_multiply(0.9));
                    clip.galley(lr.min + vec2(6.0, 4.0), g, Color32::WHITE);
                }
                Tool::Position => {
                    // TradingView's long / short position: reward box to the target, risk box to the
                    // stop, and the size that risks `risk_pct` of equity between entry and stop
                    let (x0, x1) = pos_x(d);
                    let (ye, ys, yt) = (y(d.a.1), y(d.b.1), y(d.c.1));
                    clip.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(yt)..=ye.max(yt)), 0, up().linear_multiply(0.16 * alpha));
                    clip.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(ys)..=ye.max(ys)), 0, dn().linear_multiply(0.16 * alpha));
                    for (yy, c) in [(ye, fg()), (ys, dn()), (yt, up())] { clip.hline(x0..=x1, yy, Stroke::new(1.2, c.linear_multiply(alpha))); }
                    let pct = |p: f64| if d.a.1 != 0.0 { (p / d.a.1 - 1.0) * 100.0 } else { 0.0 };
                    let risk = (d.a.1 - d.b.1).abs();
                    let rr = if risk > 0.0 { (d.c.1 - d.a.1).abs() / risk } else { 0.0 };
                    let long = d.c.1 > d.a.1;
                    let size = match (equity, risk > 0.0) {
                        (Some(eq), true) => { let usd = eq * risk_pct / 100.0; format!("{} {base} · {risk_pct}% = ${}", fmt_qty_chart(usd / risk), fmt_usd_short(usd)) }
                        _ => format!("{risk_pct}%"),
                    };
                    let tag = |yy: f64, txt: String, c: Color32, below: bool| {
                        let g = painter.layout_no_wrap(txt, mono(10.5), Color32::WHITE);
                        let r = Rect::from_min_size(pos2(x0 + 4.0, if below { yy as f32 + 3.0 } else { yy as f32 - g.size().y - 7.0 }), g.size() + vec2(8.0, 4.0));
                        clip.rect_filled(r, 3, c.linear_multiply(0.9 * alpha));
                        clip.galley(r.min + vec2(4.0, 2.0), g, Color32::WHITE);
                    };
                    tag(yt as f64, format!("{} {} ({:+.2}%)  R:R {rr:.2}", t("pos.target"), fmt_px(d.c.1), pct(d.c.1)), up(), !long);
                    tag(ys as f64, format!("{} {} ({:+.2}%)", t("pos.stop"), fmt_px(d.b.1), pct(d.b.1)), dn(), long);
                    tag(ye as f64, format!("{} {} {} · {size}", if long { t("pos.long") } else { t("pos.short") }, fmt_px(d.a.1), t("pos.qty")), label_bg(), !long);
                }
                Tool::Text => {
                    let g = note(d);
                    let r = note_rect(d, &g);
                    if emph { clip.rect_stroke(r, 3, Stroke::new(1.0, col.linear_multiply(0.6)), egui::StrokeKind::Inside); }
                    clip.galley(r.min + vec2(4.0, 4.0), g, col);
                }
                _ => for (p0, p1) in segs(d) { seg(p0, p1, s); },
            }
            // an armed alert: bell tag where the line meets the axis side of the plot
            if let (true, Some(lv)) = (d.alert.is_some(), d.level_at(bars.last().map_or(0, |b| b.t))) {
                let ay = y(lv);
                let g = painter.layout_no_wrap(format!("⏰ {}", fmt_px(lv)), mono(10.5), Color32::WHITE);
                let r = Rect::from_min_size(pos2(main.right() - g.size().x - 14.0, ay - 9.0), vec2(g.size().x + 10.0, 18.0));
                clip.rect_filled(r, 3, col);
                clip.galley(r.min + vec2(5.0, (18.0 - g.size().y) / 2.0), g, Color32::WHITE);
            }
            if d.tool == Tool::HLine {
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, a.y - 9.0), vec2(AXIS_W - 2.0, 18.0));
                painter.rect_filled(lab, 2, d.color());
                painter.text(lab.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, fmt_px(d.a.1), mono(11.0), Color32::BLACK);
            } else if emph && !d.locked {
                // handles show on hover: hollow, as in TradingView
                for (_, p) in handles(d) { clip.circle(p, 4.0, panel2(), Stroke::new(1.5, d.color())); }
            }
        };
        for (i, d) in list.iter().enumerate() { draw(d, hot.is_some_and(|h| h.0 == i), 1.0); }
        // preview of a drawing in progress
        if let (Some(tool), Some(p)) = (self.tool, ptr) {
            let pt = to_point(p);
            let mut pts = self.pending.clone();
            pts.push(pt);
            if tool == Tool::Channel && pts.len() == 2 { pts.push(pt); }
            let pv = (pts.len() >= tool.points().min(2)).then(|| Drawing::placed(tool, &pts));
            if let Some(d) = pv { draw(&d, false, 0.6); }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        match hot {
            Some((_, 0 | 1 | 3)) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
            Some((_, LOCKED)) => {}
            Some(_) => { ui.ctx().set_cursor_icon(egui::CursorIcon::Move); if self.drag.is_none() && self.menu.is_none() { resp.clone().on_hover_text(t("draw.delete_tip")); } }
            None => {}
        }
        self.hot = hot;
    }

    /// Multi-chart layout: every cell shows the same drawings and alerts; an edit in one reaches all.
    pub fn adopt_drawings(&mut self, from: &Chart) {
        if self.drawings != from.drawings { self.drawings = from.drawings.clone(); }
        self.hide = from.hide;
    }

    /// Bases with an armed alert.
    pub fn alert_bases(&self) -> Vec<String> {
        self.drawings.iter().filter(|(_, v)| v.iter().any(|d| d.alert.is_some())).map(|(b, _)| b.clone()).collect()
    }

    /// The chart's live price for alerts: the composite, or the charted venue's own mid.
    pub fn price(&self, a: &Agg, market: Market) -> Option<f64> {
        let m = if market == Market::Margin || market == Market::Option { Market::Spot } else { market };
        match self.src { None => a.mid(m), Some(e) => a.venues.get(&(e, m)).and_then(|v| v.mid()) }
    }

    /// Fires every armed alert whose line the price crossed since the last check (once: the alert
    /// disarms, the line stays). `px` holds the latest price per base.
    pub fn check_alerts(&mut self, px: &std::collections::HashMap<String, f64>) {
        let ts = depth::now_ms();
        for (base, list) in self.drawings.iter_mut() {
            let Some(&p) = px.get(base) else { continue };
            let Some(prev) = self.alert_px.insert(base.clone(), p) else { continue };
            for d in list.iter_mut() {
                let (Some(sound), Some(lv)) = (d.alert.clone(), d.level_at(ts)) else { continue };
                if (prev < lv) == (p < lv) { continue; }
                d.alert = None;
                let title = format!("{base} {} {}", if p >= lv { "↑" } else { "↓" }, fmt_px(lv));
                let body = format!("{} {}", t("alert.crossed"), fmt_px(p));
                play_sound(&sound);
                self.toast = Some((format!("⏰ {title} · {body}"), std::time::Instant::now() + std::time::Duration::from_secs(8)));
                self.fired.push((title, body));
            }
        }
    }

    /// Back to auto-fit at the live edge: a manual price range from one symbol is meaningless for
    /// another (BTC at 80k vs a coin at 0.3 would leave the chart empty).
    pub fn reset_view(&mut self) { self.y_range = None; self.right = 0.0; self.pending.clear(); self.liq = LiqCache::default(); self.cache = HeatCache::default(); }

    /// Chart controls for the native toolbar.
    pub fn native_json(&self, a: &Agg, market: Market, base: &str) -> serde_json::Value {
        let market = if market == Market::Margin { Market::Spot } else { market };
        let mut venues: Vec<String> = a.series.keys().filter(|(e, m)| *m == market && e.is_some()).filter_map(|(e, _)| e.map(|e| format!("{e:?}"))).collect();
        venues.sort();
        let tool = self.tool.map(Tool::id);
        // armed alerts, the current symbol first
        let mut alerts: Vec<serde_json::Value> = self.drawings.iter().flat_map(|(b, l)| l.iter().enumerate().filter_map(move |(i, d)| {
            let s = d.alert.as_ref()?;
            Some(serde_json::json!({"base": b, "i": i, "px": d.level_at(depth::now_ms()), "kind": d.tool.id(), "sound": s, "sound_name": sound_name(s)}))
        })).collect();
        alerts.sort_by(|x, y| (x["base"].as_str() != Some(base), x["base"].as_str()).cmp(&(y["base"].as_str() != Some(base), y["base"].as_str())));
        serde_json::json!({
            "tf": self.tf, "tfs": TFS.iter().map(|(m, l)| serde_json::json!([m, l])).collect::<Vec<_>>(), "quick": TF_QUICK,
            "src": self.src.map(|e| format!("{e:?}")), "venues": venues,
            "panes": self.panes.iter().filter(|(p, _)| !p.perp_only() || market == Market::Perp)
                .map(|(p, on)| serde_json::json!({"key": p.key(), "label": t(p.key()), "on": on})).collect::<Vec<_>>(),
            "ma": self.ma, "heat": self.heat, "liqmap": self.liqmap, "sr": self.sr, "walls": self.walls, "mark": self.mark, "breakouts": self.breakouts, "vwap": self.vwap, "bb": self.bb, "cone": self.cone, "perp": market == Market::Perp,
            "mas": self.mas.iter().map(|m| serde_json::json!({"n": m.n, "col": format!("#{:06x}", m.col), "ema": m.ema, "on": m.on})).collect::<Vec<_>>(), "magnet": self.magnet,
            "ind": self.ind, "hide": self.hide, "fp": self.fp, "fp_style": self.fp_style, "whales": self.whales, "alert_sound": self.alert_sound,
            "replay": match self.replay { None => "off", Some(Replay { at: None, .. }) => "pick", Some(_) => "on" },
            "sounds": sounds(),
            "alerts": alerts,
            "tool": tool, "drawings": self.drawings.get(base).map_or(0, |v| v.len()), "panned": self.right.abs() > 0.5 || self.bar_w != 7.0,
        })
    }

    /// Apply one change from the native toolbar: {tf}, {src: venue|null}, {pane: key, on}, {ma|heat|liqmap: bool},
    /// {tool: "hline"|"trend"|"ray"|null}, {clear_drawings: true}, {reset_view: true}.
    pub fn native_set(&mut self, v: &serde_json::Value, base: &str) {
        if let Some(tf) = v["tf"].as_i64() { if TFS.iter().any(|(m, _)| *m == tf) { self.tf = tf; } }
        if v.get("src").is_some() { self.src = v["src"].as_str().and_then(Exchange::parse); }
        if let Some(k) = v["pane"].as_str() { for (p, on) in self.panes.iter_mut() { if p.key() == k { *on = v["on"].as_bool().unwrap_or(!*on); } } }
        if let Some(b) = v["ma"].as_bool() { self.ma = b; }
        if let Some(b) = v["heat"].as_bool() { self.heat = b; }
        if let Some(b) = v["liqmap"].as_bool() { self.liqmap = b; }
        if let Some(b) = v["sr"].as_bool() { self.sr = b; }
        if let Some(b) = v["walls"].as_bool() { self.walls = b; }
        if let Some(b) = v["mark"].as_bool() { self.mark = b; }
        if let Some(b) = v["breakouts"].as_bool() { self.breakouts = b; }
        if let Some(b) = v["vwap"].as_bool() { self.vwap = b; }
        if let Some(b) = v["cone"].as_bool() { self.cone = b; }
        if let Some(b) = v["bb"].as_bool() { self.bb = b; }
        if let Some(b) = v["magnet"].as_bool() { self.magnet = b; }
        if let Some(b) = v["hide"].as_bool() { self.hide = b; }
        if let Some(b) = v["fp"].as_bool() { self.fp = b; }
        if let Some(n) = v["fp_style"].as_u64() { self.fp_style = (n as u8).min(1); }
        if let Some(b) = v["whales"].as_bool() { self.whales = b; }
        // {ind: {rsi?, macd?: [f, s, g], bb_n?, bb_k?}}
        let ind = &v["ind"];
        if let Some(n) = ind["rsi"].as_u64() { self.ind.rsi = (n as usize).clamp(1, 500); }
        if let Some(m) = ind["macd"].as_array() { for (k, x) in m.iter().take(3).enumerate() { if let Some(n) = x.as_u64() { self.ind.macd[k] = (n as usize).clamp(1, 500); } } }
        if let Some(n) = ind["bb_n"].as_u64() { self.ind.bb_n = (n as usize).clamp(1, 500); }
        if let Some(k) = ind["bb_k"].as_f64() { self.ind.bb_k = k.clamp(0.1, 10.0); }
        if let Some(b) = v["replay"].as_bool() { self.replay = b.then(|| Replay::new(None, 0.0)); }
        if let Some(s) = v["alert_sound"].as_str() { self.alert_sound = s.to_string(); play_sound(s); }
        if let Some(s) = v["play"].as_str() { play_sound(s); }
        // {alert: {base, i, sound?: id, off?: true}}
        let al = &v["alert"];
        if let (Some(b), Some(i)) = (al["base"].as_str(), al["i"].as_u64()) {
            if let Some(d) = self.drawings.get_mut(b).and_then(|l| l.get_mut(i as usize)) {
                if let Some(s) = al["sound"].as_str() { d.alert = Some(s.to_string()); play_sound(s); }
                if al["off"].as_bool() == Some(true) { d.alert = None; }
            }
        }
        // {ma_line: i, n?, col?: "#rrggbb", ema?, on?}
        if let Some(m) = v["ma_line"].as_u64().and_then(|i| self.mas.get_mut(i as usize)) {
            if let Some(n) = v["n"].as_u64() { m.n = (n as usize).clamp(1, 1000); }
            if let Some(c) = v["col"].as_str().and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok()) { m.col = c & 0xffffff; }
            if let Some(b) = v["ema"].as_bool() { m.ema = b; }
            if let Some(b) = v["on"].as_bool() { m.on = b; }
        }
        if v.get("tool").is_some() {
            self.tool = v["tool"].as_str().and_then(Tool::parse);
            self.pending.clear();
        }
        if v["clear_drawings"].as_bool() == Some(true) { self.drawings.remove(base); }
        if v["reset_view"].as_bool() == Some(true) { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; }
    }

    fn toolbar(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str) {
        // design: a 40 px bar over a hairline; compact interval pills, outlined menus, tools on a strip
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
        ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, line()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(0.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 2.0;
        for (m, l) in &TFS[..TF_QUICK] {
            if pill(ui, l, self.tf == *m).clicked() { self.tf = *m; }
        }
        // a menu interval in use shows as a selected pill after the quick ones
        if let Some((_, l)) = TFS[TF_QUICK..].iter().find(|(m, _)| *m == self.tf) { pill(ui, l, true); }
        let more = pill(ui, "", false);
        chevron_down(ui.painter(), more.rect.center(), mu());
        egui::Popup::menu(&more).show(|ui| {
            menu_rows(ui, 120.0);
            for (m, l) in &TFS[TF_QUICK..] { if menu_row(ui, self.tf == *m, l, None).clicked() { self.tf = *m; ui.close(); } }
        });
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(6.0);
        vsep(ui);
        let mut venues: Vec<Exchange> = a.series.keys().filter(|(e, m)| *m == market && e.is_some()).filter_map(|(e, _)| *e).collect();
        venues.sort_by_key(|e| format!("{e:?}"));
        let cur = self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| t("src.agg").to_string());
        let src = tb_button(ui, &cur, Tb::Outline, 0, true);
        egui::Popup::menu(&src).show(|ui| {
            menu_rows(ui, 180.0);
            menu_head(ui, super::l("Source"));
            if menu_row(ui, self.src.is_none(), t("src.agg"), None).clicked() { self.src = None; ui.close(); }
            for e in venues { if menu_row(ui, self.src == Some(e), &format!("{e:?}"), None).clicked() { self.src = Some(e); ui.close(); } }
        });
        self.indicators_menu(ui, market);
        self.layout_menu(ui);
        self.alerts_menu(ui, base);
        if tb_button(ui, t("replay.title"), if self.replay.is_some() { Tb::Tint } else { Tb::Ghost }, 0, false).on_hover_text(super::l("Bar replay: pick a bar, then play forward")).clicked() {
            self.replay = if self.replay.is_some() { None } else { Some(Replay::new(None, 0.0)) };
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // the drawing tools on one subtle strip; the everyday ones as buttons, the rest behind "⋯"
            let more = [(Tool::Channel, "draw.channel"), (Tool::Text, "draw.text"), (Tool::Measure, "draw.measure"), (Tool::Position, "draw.position")];
            let in_more = more.iter().find(|(k, _)| self.tool == Some(*k)).map(|x| x.0);
            let n = self.drawings.get(base).map_or(0, |v| v.len());
            egui::Frame::new().fill(panel2()).corner_radius(5).inner_margin(2).show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if n > 0 {
                    if tool_button(ui, Glyph::Trash, false).on_hover_text(format!("{} ({n})", t("draw.clear"))).clicked() { self.drawings.remove(base); }
                    if tool_button(ui, Glyph::Eye(self.hide), self.hide).on_hover_text(super::l("Hide drawings")).clicked() { self.hide = !self.hide; }
                    let (r, _) = ui.allocate_exact_size(vec2(5.0, 16.0), Sense::hover());
                    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, line()));
                }
                // the "more" button shows the armed tool from its menu
                let mb = tool_button(ui, in_more.map_or(Glyph::More, Glyph::Tool), in_more.is_some()).on_hover_text(super::l("More drawing tools"));
                egui::Popup::menu(&mb).show(|ui| {
                    menu_rows(ui, 180.0);
                    for (tool, key) in more {
                        let on = self.tool == Some(tool);
                        if menu_row(ui, on, t(key), Some(Glyph::Tool(tool))).clicked() { self.tool = if on { None } else { Some(tool) }; self.pending.clear(); ui.close(); }
                    }
                });
                for (tool, key) in [(Tool::Rect, "draw.rect"), (Tool::Fib, "draw.fib"), (Tool::Ray, "draw.ray"), (Tool::Trend, "draw.trend"), (Tool::HLine, "draw.hline")] {
                    if tool_button(ui, Glyph::Tool(tool), self.tool == Some(tool)).on_hover_text(t(key)).clicked() {
                        self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
                        self.pending.clear();
                    }
                }
            });
            if (self.right.abs() > 0.5 || self.bar_w != 7.0 || self.y_range.is_some()) && tb_button(ui, t("chart.reset"), Tb::Ghost, 0, false).on_hover_text(super::l("Back to the live edge and default zoom")).clicked() {
                self.right = 0.0; self.bar_w = 7.0; self.y_range = None;
            }
        });
    }

    /// Sub-panes, overlays (with their inputs) and levels in one menu, grouped like the native one.
    fn indicators_menu(&mut self, ui: &mut Ui, market: Market) {
        let perp = market == Market::Perp;
        let n_on = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || perp)).count()
            + [self.heat, self.ma, self.bb, self.liqmap && perp, self.sr, self.breakouts, self.vwap, self.cone, self.walls, self.mark && perp].iter().filter(|x| **x).count();
        let resp = tb_button(ui, t("chart.indicators"), Tb::Outline, n_on, false);
        egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            menu_rows(ui, 260.0);
            let indent = |ui: &mut Ui| ui.add_space(26.0);
            menu_head(ui, super::l("Panes"));
            for (p, on) in self.panes.iter_mut() {
                if p.perp_only() && !perp { continue; }
                ui.checkbox(on, t(p.key()));
            }
            ui.horizontal(|ui| {
                indent(ui);
                ui.label(RichText::new("RSI").font(prop(12.0)).color(mu()));
                ui.add(egui::DragValue::new(&mut self.ind.rsi).range(1..=500).speed(0.3));
                ui.add_space(6.0);
                ui.label(RichText::new("MACD").font(prop(12.0)).color(mu()));
                for x in self.ind.macd.iter_mut() { ui.add(egui::DragValue::new(x).range(1..=500).speed(0.3)); }
            });
            menu_sep(ui);
            menu_head(ui, super::l("Overlays"));
            ui.checkbox(&mut self.ma, super::l("Moving averages"));
            if self.ma {
                for m in self.mas.iter_mut() {
                    ui.horizontal(|ui| {
                        indent(ui);
                        ui.checkbox(&mut m.on, "");
                        let mut c = rgb(m.col);
                        if ui.color_edit_button_srgba(&mut c).changed() { m.col = super::theme::pack(c); }
                        ui.add(egui::DragValue::new(&mut m.n).range(1..=1000).speed(0.3));
                        if ui.selectable_label(!m.ema, "SMA").clicked() { m.ema = false; }
                        if ui.selectable_label(m.ema, "EMA").clicked() { m.ema = true; }
                    });
                }
            }
            ui.checkbox(&mut self.bb, super::l("Bollinger bands"));
            if self.bb {
                ui.horizontal(|ui| {
                    indent(ui);
                    ui.add(egui::DragValue::new(&mut self.ind.bb_n).range(1..=500).speed(0.3));
                    ui.add(egui::DragValue::new(&mut self.ind.bb_k).range(0.1..=10.0).speed(0.05).fixed_decimals(1).suffix(" σ"));
                });
            }
            ui.checkbox(&mut self.fp, super::l("Footprint (traded value per price)")).on_hover_text(super::l("Live trades since Depth started; zoom in to read the numbers"));
            if self.fp {
                ui.horizontal(|ui| {
                    indent(ui);
                    ui.selectable_value(&mut self.fp_style, 0, super::l("Cluster bars"));
                    ui.selectable_value(&mut self.fp_style, 1, super::l("Side clusters"));
                });
            }
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.whales, super::l("Whale trades"));
                ui.add_enabled(self.whales, egui::DragValue::new(&mut self.whale_usd).range(10_000.0..=50_000_000.0).speed(5_000.0).prefix("≥ $").custom_formatter(|v, _| fmt_usd_short(v)));
            });
            ui.checkbox(&mut self.heat, super::l("Book heatmap"));
            if perp { ui.checkbox(&mut self.liqmap, super::l("Liquidation map")).on_hover_text(t("chart.liqmap_tip")); }
            menu_sep(ui);
            menu_head(ui, super::l("Levels"));
            ui.checkbox(&mut self.sr, super::l("Volume profile (POC · value area · HVN)"));
            ui.checkbox(&mut self.breakouts, super::l("Breakouts (closes through VAH / VAL / HVN)"));
            ui.checkbox(&mut self.vwap, super::l("Session VWAP ±1σ ±2σ"));
            ui.checkbox(&mut self.cone, super::l("Expected move cone (volatility model)"));
            ui.checkbox(&mut self.walls, super::l("Liquidity walls"));
            if perp { ui.checkbox(&mut self.mark, super::l("Mark price")); }
            menu_sep(ui);
            ui.checkbox(&mut self.magnet, super::l("Magnet crosshair"));
        });
    }

    /// Chart layout (the app owns the cells: the choice goes out through `layout_req`).
    fn layout_menu(&mut self, ui: &mut Ui) {
        const LAYOUTS: [(&str, &str); 5] = [("1", "One chart"), ("2", "Two side by side"), ("2v", "Two stacked"), ("3", "Three"), ("4", "Four")];
        let layout = self.layout.clone();
        let resp = kit::icon_button(ui, kit::H, false, |p, r, col| layout_glyph(p, Rect::from_center_size(r.center(), vec2(16.0, 12.0)), &layout, col));
        egui::Popup::menu(&resp.on_hover_text(super::l("Chart layout: the toolbar drives the highlighted chart"))).show(|ui| {
            menu_rows(ui, 200.0);
            menu_head(ui, super::l("Layout"));
            for (id, name) in LAYOUTS {
                if menu_row(ui, self.layout == id, super::l(name), Some(Glyph::Layout(id))).clicked() { self.layout_req = Some(id.to_string()); ui.close(); }
            }
        });
    }

    /// Armed alerts on every symbol (sound, remove) and the sound of new ones.
    fn alerts_menu(&mut self, ui: &mut Ui, base: &str) {
        let mut rows: Vec<(String, usize, f64, String)> = self.drawings.iter().flat_map(|(b, l)| l.iter().enumerate().filter_map(move |(i, d)| {
            Some((b.clone(), i, d.level_at(depth::now_ms())?, d.alert.clone()?))
        })).collect();
        rows.sort_by(|x, y| (x.0 != base, &x.0).cmp(&(y.0 != base, &y.0)));
        let resp = tb_button(ui, super::l("Alerts"), if rows.is_empty() { Tb::Ghost } else { Tb::Tint }, rows.len(), false).on_hover_text(super::l("Price alerts"));
        egui::Popup::menu(&resp).show(|ui| {
            menu_rows(ui, 260.0);
            menu_head(ui, super::l("Price alerts"));
            if rows.is_empty() {
                ui.horizontal(|ui| { ui.add_space(8.0); ui.label(RichText::new(super::l("Right-click the chart or a line to add an alert")).font(prop(12.0)).color(dim())); });
            }
            for (b, i, px, snd) in rows {
                ui.menu_button(format!("{b}  {}  ·  {}", fmt_px(px), sound_name(&snd)), |ui| {
                    menu_rows(ui, 160.0);
                    if let Some(d) = self.drawings.get_mut(&b).and_then(|l| l.get_mut(i)) {
                        if let Some(s) = d.alert.as_mut() {
                            for (id, name) in sounds() { if menu_row(ui, *s == id, &name, None).clicked() { play_sound(&id); *s = id; } }
                        }
                        menu_sep(ui);
                        if ui.button(RichText::new(super::l("Remove alert")).color(dn())).clicked() { d.alert = None; ui.close(); }
                    }
                });
            }
            menu_sep(ui);
            ui.menu_button(super::l("Sound of new alerts"), |ui| {
                menu_rows(ui, 160.0);
                for (id, name) in sounds() { if menu_row(ui, self.alert_sound == id, &name, None).clicked() { play_sound(&id); self.alert_sound = id; } }
            });
        });
    }
}

// ---------------------------------------------------------------------- chrome primitives
// Local to the chart (the kit has no pill / menu row / painted glyph yet).

/// Interval pill (design: 26 px, 12.5; selected = hl fill, semibold, strong text).
fn pill(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = if on { prop_s(12.5) } else { prop_m(12.5) };
    let w = if text.is_empty() { 22.0 } else { ui.painter().layout_no_wrap(text.to_string(), prop_s(12.5), fg()).size().x + 16.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
    if on { ui.painter().rect_filled(r, kit::R, hl()); } else if hv > 0.0 { ui.painter().rect_filled(r, kit::R, hl().gamma_multiply(0.6 * hv)); }
    if !text.is_empty() { ui.painter().text(r.center(), Align2::CENTER_CENTER, text, font, if on { fg_strong() } else { lerp(mu(), fg(), hv) }); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy, PartialEq)]
enum Tb { Outline, Ghost, Tint }

/// Toolbar button, 28 px: outlined (menus), text only, or accent-tinted (armed alerts, replay on);
/// `badge` > 0 adds a count pill, `chev` a down chevron.
fn tb_button(ui: &mut Ui, text: &str, look: Tb, badge: usize, chev: bool) -> egui::Response {
    let font = if look == Tb::Tint { prop_s(12.0) } else { prop_m(12.5) };
    let g = ui.painter().layout_no_wrap(text.to_string(), font, fg());
    let bg = (badge > 0 && look != Tb::Tint).then(|| ui.painter().layout_no_wrap(badge.to_string(), mono_s(11.0), fg()));
    let w = g.size().x + 20.0 + bg.as_ref().map_or(0.0, |b| b.size().x + 16.0) + if chev { 14.0 } else { 0.0 } + if look == Tb::Tint && badge > 0 { 14.0 } else { 0.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, kit::H), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    let (fill, stroke, col) = match look {
        Tb::Outline => (lerp(panel2(), hl(), hv), Stroke::new(1.0, lerp(line(), edge(), hv)), lerp(fg(), fg_strong(), hv)),
        Tb::Ghost => (hl().gamma_multiply(0.7 * hv), Stroke::NONE, lerp(mu(), fg_strong(), hv)),
        Tb::Tint => (accent().gamma_multiply(0.16 + 0.06 * hv), Stroke::NONE, accent()),
    };
    p.rect(r, kit::R, fill, stroke, egui::StrokeKind::Inside);
    let mut x = r.left() + 10.0;
    p.text(pos2(x, r.center().y), Align2::LEFT_CENTER, text, if look == Tb::Tint { prop_s(12.0) } else { prop_m(12.5) }, col);
    x += g.size().x;
    if look == Tb::Tint && badge > 0 { p.text(pos2(x + 5.0, r.center().y), Align2::LEFT_CENTER, badge.to_string(), mono_s(12.0), col); }
    if let Some(b) = bg {
        let br = Rect::from_min_size(pos2(x + 6.0, r.center().y - 9.0), vec2(b.size().x + 12.0, 18.0));
        p.rect_filled(br, 5, accent().gamma_multiply(0.2));
        p.text(br.center(), Align2::CENTER_CENTER, badge.to_string(), mono_s(11.0), lerp(accent(), Color32::WHITE, 0.35));
    }
    if chev { chevron_down(p, pos2(r.right() - 13.0, r.center().y), mu()); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small down chevron centered on `c`.
fn chevron_down(p: &egui::Painter, c: Pos2, col: Color32) {
    p.add(egui::Shape::line(vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)], Stroke::new(1.3, col)));
}

/// Vertical hairline between toolbar groups.
fn vsep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 18.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, line()));
}

/// Menu body (design: popups radius 6 with a soft shadow come from the style): 28 px rows.
fn menu_rows(ui: &mut Ui, w: f32) {
    ui.set_min_width(w);
    let s = ui.spacing_mut();
    s.interact_size.y = 26.0;
    s.button_padding = vec2(8.0, 0.0);
    s.item_spacing.y = 2.0;
    s.icon_spacing = 8.0;
}

/// Section overline inside a menu.
fn menu_head(ui: &mut Ui, s: &str) {
    ui.add_space(2.0);
    ui.horizontal(|ui| { ui.add_space(8.0); kit::overline(ui, s); });
}

/// Hairline between menu sections.
fn menu_sep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 7.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, line()));
}

/// A 28 px menu row: check mark (or `glyph`) column, label; hover fill.
fn menu_row(ui: &mut Ui, on: bool, text: &str, glyph: Option<Glyph>) -> egui::Response {
    let tw = ui.painter().layout_no_wrap(text.to_string(), prop(12.5), fg()).size().x + 44.0;
    let (r, resp) = ui.allocate_exact_size(vec2(ui.min_rect().width().max(tw), 26.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.08);
    let p = ui.painter();
    if hv > 0.0 { p.rect_filled(r, kit::R, hl().gamma_multiply(hv)); }
    let lead = Rect::from_center_size(pos2(r.left() + 17.0, r.center().y), vec2(18.0, 18.0));
    match glyph {
        Some(g) => paint_glyph(p, lead, g, if on { accent() } else { mu() }),
        None if on => { p.add(egui::Shape::line(vec![lead.center() + vec2(-4.0, 0.0), lead.center() + vec2(-1.0, 3.0), lead.center() + vec2(4.5, -3.5)], Stroke::new(1.5, accent()))); }
        None => {}
    }
    p.text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, text, prop(12.5), if on || hv > 0.5 { fg_strong() } else { fg() });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Painted toolbar glyphs.
#[derive(Clone, Copy)]
enum Glyph { Tool(Tool), Trash, Eye(bool), More, Layout(&'static str) }

/// Drawing-tool button on the toolbar strip (design: 28 x 24; selected = hl fill, strong glyph).
fn tool_button(ui: &mut Ui, g: Glyph, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(28.0, 24.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hov"), resp.hovered() && !on, 0.1);
    let p = ui.painter();
    if on { p.rect_filled(r, kit::R, hl()); } else if hv > 0.0 { p.rect_filled(r, kit::R, hl().gamma_multiply(0.7 * hv)); }
    paint_glyph(p, r, g, if on { fg_strong() } else { lerp(mu(), fg_strong(), hv) });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Paints `g` centered in `r` (glyphs are ~14 px).
fn paint_glyph(p: &egui::Painter, r: Rect, g: Glyph, col: Color32) {
    let s = Stroke::new(1.3, col);
    let c = r.center();
    match g {
        Glyph::Tool(Tool::HLine) => {
            p.hline(egui::Rangef::new(c.x - 7.0, c.x + 7.0), c.y, s);
            p.circle_filled(c, 2.0, col);
        }
        Glyph::Tool(Tool::Trend) => {
            let (a, b) = (c + vec2(-6.0, 5.0), c + vec2(6.0, -5.0));
            p.line_segment([a, b], s);
            p.circle_stroke(a, 1.8, s);
            p.circle_stroke(b, 1.8, s);
        }
        Glyph::Tool(Tool::Ray) => {
            let (a, b) = (c + vec2(-6.0, 5.0), c + vec2(6.5, -5.5));
            p.line_segment([a, b], s);
            p.circle_stroke(a, 1.8, s);
            p.line_segment([b, b + vec2(-4.5, 0.3)], s);
            p.line_segment([b, b + vec2(-0.3, 4.5)], s);
        }
        Glyph::Tool(Tool::Fib) => {
            for (k, dy) in [-5.0f32, -1.7, 1.7, 5.0].into_iter().enumerate() { p.hline(egui::Rangef::new(c.x - 7.0, c.x + 7.0 - (k % 2) as f32 * 3.0), c.y + dy, Stroke::new(1.2, col)); }
        }
        Glyph::Tool(Tool::Rect) => { p.rect_stroke(Rect::from_center_size(c, vec2(13.0, 10.0)), 1, s, egui::StrokeKind::Middle); }
        Glyph::Tool(Tool::Channel) => {
            p.line_segment([c + vec2(-7.0, 2.0), c + vec2(5.0, -6.0)], s);
            p.line_segment([c + vec2(-5.0, 6.0), c + vec2(7.0, -2.0)], s);
        }
        Glyph::Tool(Tool::Text) => { p.text(c, Align2::CENTER_CENTER, "T", prop_m(13.5), col); }
        Glyph::Tool(Tool::Position) => {
            p.rect_stroke(Rect::from_min_max(c + vec2(-7.0, -6.0), c + vec2(7.0, 0.0)), 1, Stroke::new(1.2, up()), egui::StrokeKind::Middle);
            p.rect_stroke(Rect::from_min_max(c + vec2(-7.0, 0.0), c + vec2(7.0, 5.0)), 1, Stroke::new(1.2, dn()), egui::StrokeKind::Middle);
        }
        Glyph::Tool(Tool::Measure) => {
            p.vline(c.x, egui::Rangef::new(c.y - 6.5, c.y + 6.5), s);
            for d in [-1.0f32, 1.0] {
                let tip = c + vec2(0.0, 6.5 * d);
                p.line_segment([tip, tip + vec2(-3.0, -3.0 * d)], s);
                p.line_segment([tip, tip + vec2(3.0, -3.0 * d)], s);
            }
        }
        Glyph::Trash => {
            p.hline(egui::Rangef::new(c.x - 5.5, c.x + 5.5), c.y - 4.5, s);
            p.hline(egui::Rangef::new(c.x - 2.0, c.x + 2.0), c.y - 6.5, s);
            p.line_segment([c + vec2(-4.0, -4.5), c + vec2(-3.2, 6.0)], s);
            p.line_segment([c + vec2(4.0, -4.5), c + vec2(3.2, 6.0)], s);
            p.hline(egui::Rangef::new(c.x - 3.2, c.x + 3.2), c.y + 6.0, s);
        }
        Glyph::Eye(hidden) => {
            // almond outline from two arcs, the pupil; struck through when drawings are hidden
            let arc = |up: f32| (0..=12).map(|k| { let t = k as f32 / 12.0; pos2(c.x - 7.0 + 14.0 * t, c.y + up * 4.5 * (std::f32::consts::PI * t).sin()) }).collect::<Vec<_>>();
            p.add(egui::Shape::line(arc(-1.0), s));
            p.add(egui::Shape::line(arc(1.0), s));
            p.circle_stroke(c, 2.0, s);
            if hidden { p.line_segment([c + vec2(-6.0, 6.0), c + vec2(6.0, -6.0)], s); }
        }
        Glyph::More => { for dx in [-4.5f32, 0.0, 4.5] { p.circle_filled(c + vec2(dx, 0.0), 1.3, col); } }
        Glyph::Layout(id) => layout_glyph(p, Rect::from_center_size(c, vec2(15.0, 11.0)), id, col),
    }
}

/// Small picture of a chart layout ("1", "2", "2v", "3", "4") inside `r`.
fn layout_glyph(p: &egui::Painter, r: Rect, id: &str, col: Color32) {
    let s = Stroke::new(1.3, col);
    p.rect_stroke(r, 2, s, egui::StrokeKind::Inside);
    let (cx, cy) = (r.center().x, r.center().y);
    match id {
        "2" => { p.vline(cx, r.y_range(), s); }
        "2v" => { p.hline(r.x_range(), cy, s); }
        "3" => { p.vline(cx, r.y_range(), s); p.hline(cx..=r.right(), cy, s); }
        "4" => { p.vline(cx, r.y_range(), s); p.hline(r.x_range(), cy, s); }
        _ => {}
    }
}

/// Alert sounds: "none", macOS system sounds ("sys:Glass") and the bundled ones ("depth:chime").
const SYS_SOUNDS: [&str; 14] = ["Basso", "Blow", "Bottle", "Frog", "Funk", "Glass", "Hero", "Morse", "Ping", "Pop", "Purr", "Sosumi", "Submarine", "Tink"];
/// (id, i18n key, wav)
const OWN_SOUNDS: [(&str, &str, &[u8]); 4] = [
    ("chime", "sound.chime", include_bytes!("../../assets/sounds/chime.wav")), ("bell", "sound.bell", include_bytes!("../../assets/sounds/bell.wav")),
    ("alarm", "sound.alarm", include_bytes!("../../assets/sounds/alarm.wav")), ("pulse", "sound.pulse", include_bytes!("../../assets/sounds/pulse.wav")),
];

/// (id, display name) of every alert sound, bundled first.
pub fn sounds() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = OWN_SOUNDS.iter().map(|(k, key, _)| (format!("depth:{k}"), t(key).to_string())).collect();
    if cfg!(target_os = "macos") { v.extend(SYS_SOUNDS.iter().map(|n| (format!("sys:{n}"), n.to_string()))); }
    v.push(("none".into(), t("sound.none").to_string()));
    v
}

fn sound_name(id: &str) -> String { sounds().into_iter().find(|(k, _)| k == id).map_or_else(|| id.to_string(), |(_, n)| n) }

/// Plays an alert sound without blocking.
/// ponytail: macOS only (afplay); Windows / Linux stay silent until a host plays it natively.
pub fn play_sound(id: &str) {
    if !cfg!(target_os = "macos") { return; }
    let path = if let Some(n) = id.strip_prefix("sys:") {
        Some(std::path::PathBuf::from(format!("/System/Library/Sounds/{n}.aiff")))
    } else if let Some((k, _, wav)) = id.strip_prefix("depth:").and_then(|n| OWN_SOUNDS.iter().find(|(k, _, _)| *k == n)) {
        // written once to the cache folder: afplay needs a file
        depth::sys::cache_dir().map(|d| d.join("sounds").join(format!("{k}.wav"))).filter(|p| p.exists() || {
            p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(p, wav).is_ok()
        })
    } else { None };
    if let Some(p) = path {
        if let Ok(mut c) = std::process::Command::new("afplay").arg(p).spawn() { std::thread::spawn(move || { let _ = c.wait(); }); }
    }
}

/// Sound picker as a submenu (keeps the right-click menu open); choosing one plays it.
fn sound_menu(ui: &mut Ui, cur: &mut String) {
    ui.menu_button(format!("{}: {}", t("alert.sound"), sound_name(cur)), |ui| {
        for (id, name) in sounds() {
            if ui.selectable_label(*cur == id, name).clicked() { play_sound(&id); *cur = id; }
        }
    });
}

/// Footprint cell volume: 0.42 / 3.1 / 27 / 1.2K.
fn fmt_fp(v: f64) -> String {
    let a = v.abs();
    if a == 0.0 { "0".into() } else if a >= 1e6 { format!("{:.1}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else if a >= 100.0 { format!("{v:.0}") } else if a >= 10.0 { format!("{v:.1}") } else { format!("{v:.2}") }
}
fn fmt_fp_signed(v: f64) -> String { format!("{}{}", if v >= 0.0 { "+" } else { "" }, fmt_fp(v)) }

/// Order size with the decimals a size of that magnitude needs.
fn fmt_qty_chart(q: f64) -> String { let d = if q > 0.0 { (3.0 - q.log10().floor()).clamp(0.0, 6.0) as usize } else { 2 }; fmt_dp(q, d) }

/// "3d 4h", "2h 15m", "45m" for a duration in ms.
fn span(ms: i64) -> String {
    let m = ms / 60_000;
    let (d, h, mm) = (m / 1440, m / 60 % 24, m % 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {mm}m") } else { format!("{mm}m") }
}

/// One venue's line in the premium pane.
pub(crate) struct Prem {
    col: Color32, name: String,
    /// premium over the composite (bp) per visible bar
    vals: Vec<Option<f64>>,
    /// value at the hovered (or last) bar
    hover: Option<f64>,
    /// the venue's usual premium: median of its 1m history
    usual: f64,
    /// the hovered value is far outside the venue's own noise (same rule as the dislocation signal)
    odd: bool,
}

impl Prem { fn dev(&self) -> f64 { self.hover.map_or(0.0, |v| v - self.usual) } }

/// Per-venue premium over the composite for the visible bars (mean of the 1m premiums inside
/// each bar), its usual level and whether the hovered bar is abnormal.
fn premium_lines(a: &Agg, market: Market, vis: &[Bar], ms: i64, hk: Option<usize>) -> Vec<Prem> {
    let mut out: Vec<Prem> = a.venues.keys().filter(|(_, m)| *m == market).filter_map(|&(e, m)| {
        let p = a.premium_bps(e, m);
        let (usual, sd) = depth::quant::robust(&p.iter().map(|x| x.1).collect::<Vec<_>>())?;
        let map: std::collections::BTreeMap<i64, f64> = p.into_iter().collect();
        let vals: Vec<Option<f64>> = vis.iter().map(|b| {
            let (n, sum) = map.range(b.t..b.t + ms).fold((0usize, 0.0), |acc, (_, v)| (acc.0 + 1, acc.1 + v));
            (n > 0).then(|| sum / n as f64)
        }).collect();
        let hover = hk.and_then(|k| vals.get(k).copied().flatten());
        let odd = hover.is_some_and(|v| (v - usual).abs() >= 5.0 * sd.max(1.0));
        Some(Prem { col: ex_color(e), name: format!("{e:?}"), vals, hover, usual, odd })
    }).collect();
    out.sort_by(|x, y| x.name.cmp(&y.name));
    out
}

/// dark blue -> cyan -> yellow ramp for liquidity
fn heat_color(s: f32) -> Color32 {
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let (r, g, b) = if s < 0.5 {
        let t = s / 0.5;
        (lerp(20.0, 30.0, t), lerp(40.0, 150.0, t), lerp(90.0, 200.0, t))
    } else {
        let t = (s - 0.5) / 0.5;
        (lerp(30.0, 235.0, t), lerp(150.0, 200.0, t), lerp(200.0, 95.0, t))
    };
    // thin liquidity fades out completely, so only real walls light up (a floor here turned a
    // lone column into a solid block)
    let a = if s < 0.25 { 0.0 } else { 190.0 * ((s - 0.25) / 0.75).powf(1.3) };
    Color32::from_rgba_unmultiplied(r as u8, g as u8, b as u8, a as u8)
}

fn fmt_time(ms: i64, date: bool) -> String {
    let (d, h, m) = local_parts(ms);
    if date || (h == 0 && m == 0) { d } else { format!("{h:02}:{m:02}") }
}

pub fn local_date(ms: i64) -> String { local_parts(ms).0 }

fn fmt_time_full(ms: i64) -> String {
    let (d, h, m) = local_parts(ms);
    format!("{d} {h:02}:{m:02}")
}

/// (M/D, hour, minute) in local time
fn local_parts(ms: i64) -> (String, i64, i64) {
    let off = super::local_offset_ms();
    let t = ms + off;
    let days = t.div_euclid(86_400_000);
    let rem = t.rem_euclid(86_400_000);
    // civil from days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (format!("{m}/{d}"), rem / 3_600_000, rem % 3_600_000 / 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_seeds_with_sma_then_smooths() {
        let e = ema(&[1.0, 2.0, 3.0, 4.0], 3);
        assert_eq!(e[..2], [None, None]);
        assert_eq!(e[2], Some(2.0));
        // k = 2 / (3 + 1) = 0.5
        assert_eq!(e[3], Some(3.0));
        assert_eq!(Tool::parse("fib").map(Tool::id), Some("fib"));
    }

    #[test]
    fn alerts_fire_once_on_crossing() {
        let mut c = Chart::default();
        let mut d = Drawing::new(Tool::HLine, (0, 100.0), (0, 100.0));
        d.alert = Some("none".into());
        c.drawings.insert("BTC".into(), vec![d]);
        let px = |p: f64| std::collections::HashMap::from([("BTC".to_string(), p)]);
        c.check_alerts(&px(99.0));
        c.check_alerts(&px(99.5));
        assert!(c.fired.is_empty());
        c.check_alerts(&px(100.0));
        assert_eq!(c.fired.len(), 1);
        assert!(c.drawings["BTC"][0].alert.is_none());
        c.check_alerts(&px(98.0));
        assert_eq!(c.fired.len(), 1);
        assert_eq!(c.alert_bases(), Vec::<String>::new());
        // a ray alerts from its start on, a trend line only between its points
        let r = Drawing::new(Tool::Ray, (1000, 10.0), (2000, 20.0));
        assert_eq!((r.level_at(500), r.level_at(3000)), (None, Some(30.0)));
        let t = Drawing { tool: Tool::Trend, ..r.clone() };
        assert_eq!((t.level_at(1500), t.level_at(3000)), (Some(15.0), None));
        // a channel keeps its third point halfway along the parallel line
        let ch = Drawing::new(Tool::Channel, (0, 10.0), (100, 20.0)).with_c((100, 25.0));
        assert_eq!((ch.c, ch.offset()), ((50, 20.0), 5.0));
    }

    #[test]
    fn rsi_macd_bollinger() {
        let r = rsi(&[1.0, 2.0, 1.0, 2.0, 3.0], 2);
        assert_eq!(r[..2], [None, None]);
        assert_eq!(r[2], Some(50.0));
        // Wilder: up = (0.5 + 1) / 2, dn = 0.5 / 2
        assert_eq!(r[3], Some(75.0));
        assert_eq!(rsi(&[1.0, 2.0, 3.0], 2)[2], Some(100.0));
        let m = macd(&[5.0; 40], 12, 26, 9);
        assert!(m[32].is_none() && m[33] == Some((0.0, 0.0, 0.0)));
        let b = bollinger(&[1.0, 3.0, 1.0, 3.0], 2, 2.0);
        assert_eq!((b[0], b[1]), (None, Some((2.0, 2.0))));
    }

    #[test]
    fn heatmap_pulled_wall_fades() {
        let mut h = Heatmap { bin: 1.0, ..Default::default() };
        // bar 0: a wall at bin 100 for 1 of 4 samples; bar 1 (live): pulled
        for (ts, q) in [(1, 40.0), (15_000, 0.0), (30_000, 0.0), (45_000, 0.0), (60_000, 40.0), (61_000, 0.0)] {
            h.cols.push_back((ts, vec![(100, q), (101, 1.0)]));
        }
        let mut c = HeatCache::default();
        c.update(&h, 1, Market::Perp);
        let (_, lo, v, n) = &c.bars[0];
        assert_eq!(v[(100 - lo) as usize] / *n as f32, 10.0);
        assert_eq!(c.cur, vec![(100, 0.0), (101, 1.0)]);
    }

    #[test]
    fn profile_poc_value_area_and_vwap() {
        let bar = |t: i64, l: f64, h: f64, vol: f64| Bar { t, o: l, h, l, c: (l + h) / 2.0, vol, ..Default::default() };
        // heavy trade at 100..101, light tails up to 110
        let mut bars = vec![bar(0, 100.0, 101.0, 100.0), bar(1, 100.0, 101.0, 100.0)];
        bars.extend((0..10).map(|i| bar(2 + i, 101.0 + i as f64 * 0.9, 102.0 + i as f64 * 0.9, 2.0)));
        let p = Profile::build(&bars, 100.0, 110.0, 20).unwrap();
        assert!((p.px(p.poc()) - 100.5).abs() < 0.3);
        let (a, z) = p.value_area();
        let inside: f64 = p.vol[a..=z].iter().sum();
        assert!(inside >= 0.7 * p.vol.iter().sum::<f64>());
        assert!(z - a < 10, "value area should hug the heavy zone: {a}..{z}");
        assert!(p.hvns().iter().any(|l| (l.px - 100.5).abs() < 1.0));
        assert!(Profile::build(&[], 1.0, 2.0, 10).is_none());
        // VWAP of two equal-volume bars at typical prices 100 and 102 is 101, sigma 1
        let day = 86_400_000 * 20_000;
        let v = session_vwap(&[bar(day, 100.0, 100.0, 1.0), bar(day + 60_000, 102.0, 102.0, 1.0)]);
        assert!((v[1].1 - 101.0).abs() < 1e-9 && (v[1].2 - 1.0).abs() < 1e-9);
        // a level at 102 closed through upward on the last bar
        let lv = [SrLevel { px: 102.0, half: 0.4, score: 1.0 }];
        let c = |t: i64, c: f64| Bar { t, o: c, h: c, l: c, c, ..Default::default() };
        let b = broken_levels(&[c(0, 101.0), c(1, 103.0)], &lv, 10);
        assert!(b.len() == 1 && b[0].1 && !b[0].2);
        // broke up, then the last close is back below: failed
        let f = broken_levels(&[c(0, 101.0), c(1, 103.0), c(2, 101.0)], &lv, 10);
        assert!(f.len() == 1 && f[0].1 && f[0].2);
        assert!(broken_levels(&[c(0, 101.0), c(1, 102.1)], &lv, 10).is_empty());
    }

    #[test]
    fn liq_levels_open_and_sweep() {
        let bar = |t: i64, c: f64, vol: f64| Bar { t, o: c, h: c, l: c, c, vol, ..Default::default() };
        let total = |m: &LiqMap| m.long.iter().chain(&m.short).sum::<f32>();
        // 20 traded at 100: 10 of long levels below, 10 of short levels above
        let m = liq_levels(&[bar(0, 100.0, 20.0)]).unwrap();
        let px = |i: usize| (m.lo + i as i64) as f64 * m.bin;
        let below: f32 = m.long.iter().enumerate().filter(|(i, _)| px(*i) < 100.0).map(|(_, q)| q).sum();
        let above: f32 = m.short.iter().enumerate().filter(|(i, _)| px(*i) > 100.0).map(|(_, q)| q).sum();
        assert!((below - 10.0).abs() < 1e-3 && (above - 10.0).abs() < 1e-3, "{below} {above}");
        // a dip to 97 with no volume wipes the 50x/100x long levels (98.5, 99.5), keeps 3x..20x
        let t = total(&liq_levels(&[bar(0, 100.0, 20.0), Bar { l: 97.0, ..bar(1, 100.0, 0.0) }]).unwrap());
        assert!((t - (20.0 - 10.0 * 0.2)).abs() < 1e-3, "{t}");
    }

    #[test]
    fn sma_and_countdown() {
        assert_eq!(sma(&[1.0, 2.0, 3.0, 4.0], 2), vec![None, Some(1.5), Some(2.5), Some(3.5)]);
        assert_eq!(sma(&[5.0], 3), vec![None]);
        assert_eq!(countdown(65_000), "01:05");
        assert_eq!(countdown(3_725_000), "1:02:05");
        assert_eq!(countdown(-5), "00:00");
    }

    #[test]
    fn grid_is_bounded_for_float_noise_ranges() {
        // first aggregated bar: high and low differ only by float noise
        let (lo, hi) = padded_range([86000.0, 86000.0 + 1e-11].into_iter(), 0.06, false);
        assert!(hi - lo > 100.0, "range widened to ±0.1%: {lo}..{hi}");
        let g = grid(lo, hi, 8.0);
        assert!(!g.is_empty() && g.len() <= 50);
        // even a degenerate tiny range cannot loop forever
        assert!(grid(86000.0, 86000.0 + 1e-11, 8.0).len() <= 50);
        assert!(grid(0.0, 1.0, 1e9).len() <= 50);
        // non-finite inputs are ignored
        let (lo, hi) = padded_range([f64::NAN, 5.0, f64::INFINITY].into_iter(), 0.0, false);
        assert!(lo.is_finite() && hi.is_finite() && lo < 5.0 && hi > 5.0);
    }
}
