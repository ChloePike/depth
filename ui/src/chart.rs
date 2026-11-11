//! Candle chart painted with egui: heatmap background, candles, liquidation bubbles, and
//! stacked sub-panes (volume, CVD, OI, funding predicted + settled, basis, long/short).
use super::{fmt_dp, fmt_px, kit, l, local_offset_ms, t, theme};
// colours, fonts and radius from the style the app sets each frame (`charting::style::set`)
use charting::style::*;
// venue colours and logos stay the app's
use theme::{ex_color, icon};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui};
use charting::Z;
use depth::agg::{Agg, Bar};
use depth::{Exchange, Market};

mod chrome;
mod draw;
mod fmt;
mod heat;
// leaf code lives in the `charting` crate; the old module paths stay as aliases
use charting::ta as ind;
use charting::coords::{bucket, nice, BarAxis as Frame, PriceScale as PScale};
use charting::layout::{place, try_place};
mod levels;
mod liq;
mod pane;
mod plot;
mod toolbar;

use draw::{Drawing, Menu, Tool, LOCKED};
pub(crate) use fmt::fmt_usd_short;
pub use fmt::local_date;
use heat::HeatCache;
use liq::LiqCache;
use pane::Pane;

pub(crate) const AXIS_W: f32 = 74.0;
pub(crate) const TIME_H: f32 = 20.0;
const SUB_H: f32 = 70.0;
/// Every chart interval (minutes); the first six sit in the toolbar, the rest in its menu.
const TFS: [(i64, &str); 12] = [(1, "1m"), (5, "5m"), (15, "15m"), (60, "1H"), (240, "4H"), (1440, "1D"),
    (3, "3m"), (30, "30m"), (120, "2H"), (360, "6H"), (720, "12H"), (10080, "1W")];
const TF_QUICK: usize = 6;

/// The venue-native history interval an interval is built from (venues serve 1/5/15/60/240m;
/// everything else is resampled from the largest of those that divides it).
pub fn native_tf(tf: i64) -> i64 { [240, 60, 15, 5, 1].into_iter().find(|n| tf % n == 0).unwrap_or(1) }

// the widget's scale serializes as the app's own enum did ("Linear", "Log", "Percent"): settings.json loads as before
use charting::Scale;


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
/// Banner on top of the chart for 8 s with a coloured dot.
impl Chart { pub fn toast(&mut self, msg: String, dot: Color32) { self.toast = Some((msg, std::time::Instant::now() + std::time::Duration::from_secs(8), dot)); } }

#[derive(Clone)]
pub struct ChartLine { pub price: f64, pub ex: Option<Exchange>, pub tag: String, pub sub: String, pub col: Color32, pub act: Option<LineAct> }

/// What dragging an account line changes: a resting limit order, a TP / SL trigger (venue, id), or a
/// field of the order being typed in the panel.
#[derive(Clone, PartialEq, Debug)]
pub enum LineAct { Order(Exchange, String), TpSl(Exchange, String), Preview(&'static str) }

/// Requests from the chart to the app: move an order / trigger, put a price into the order panel,
/// a position tool's whole order (entry, stop, target and the size risking its share of equity), or the
/// native interval whose history page before the oldest loaded bar is wanted (the view reached it).
#[derive(Clone, Debug)]
pub enum ChartAct { Move(LineAct, f64), Cancel(LineAct), Fill(f64), Position { entry: f64, stop: f64, target: f64, qty: Option<f64> }, Older(i64) }

/// One moving average line: period, color (0xRRGGBB), exponential instead of simple, shown.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MaLine { pub n: usize, pub col: u32, pub ema: bool, pub on: bool }

fn default_mas() -> Vec<MaLine> {
    vec![MaLine { n: 7, col: 0xf0b90b, ema: false, on: true }, MaLine { n: 25, col: 0xe05bc4, ema: false, on: true }, MaLine { n: 99, col: 0x9b8cf0, ema: false, on: true }]
}

fn rgb(c: u32) -> Color32 { Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8) }

fn dashed(painter: &egui::Painter, x0: f32, x1: f32, y: f32, col: Color32) {
    painter.extend(egui::Shape::dashed_line(&[pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, col), 4.0, 3.0));
}

/// Grid line, TradingView-style: 1 px dots, faint (dashes, not circles: cheap to tessellate).
pub(super) fn grid_line(painter: &egui::Painter, a: Pos2, b: Pos2) {
    painter.extend(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, line()), 1.5, 2.5));
}


/// Persisted in settings.json (see ui::Settings); runtime-only fields are skipped.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Chart {
    pub tf: i64,
    pub src: Option<Exchange>,
    heat: bool,
    /// estimated liquidation map, drawn as a profile on the right edge (perp)
    liqmap: bool,
    #[serde(deserialize_with = "crate::lenient_vec")]
    panes: Vec<(Pane, bool)>,
    bar_w: f32,
    /// drawings per base asset
    #[serde(deserialize_with = "crate::lenient_map_vec")]
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
    /// own fills as B / S tags (perp)
    fills_on: bool,
    /// likely algos (algoscan): slicer clips as diamonds, icebergs as dotted levels
    algos: bool,
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
    /// Polymarket odds at their strikes (read-only reference)
    pub poly: bool,
    scale: Scale,
    /// positions / orders of the current symbol, set by the app every frame
    #[serde(skip)] pub lines: Vec<ChartLine>,
    /// an option-data pane is on: the app subscribes the option chains
    #[serde(skip)] pub wants_options: bool,
    /// Polymarket odds of the current base, set by the app every frame while `poly`
    #[serde(skip)] pub odds: Vec<depth::poly::Odds>,
    /// the order venue (the header's mark): the aggregate chart's Mark line is that venue's mark, like the header
    #[serde(skip)] pub mark_ex: Option<Exchange>,
    /// own fills of the current symbol (perp), oldest first, set by the app when they change
    #[serde(skip)] pub fills: Vec<depth::trade::Fill>,
    #[serde(skip)] base_unit: String,
    /// the chart widget: time view (offset from the live edge, zoom, glide), manual price range, eased auto-fit;
    /// `bar_w` and `scale` above are its persisted part, copied in and out every frame
    #[serde(skip)] view: charting::Chart,
    /// the chart menu asked for a view reset, the "A" button for auto-fit (applied after the widget's frame)
    #[serde(skip)] reset_req: bool,
    #[serde(skip)] auto_req: bool,
    #[serde(skip)] liq: LiqCache,
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
    /// drawing edits: the last committed list (base, drawings), undo and redo stacks of earlier ones
    #[serde(skip)] committed: Option<(String, Vec<Drawing>)>,
    #[serde(skip)] undo: Vec<Vec<Drawing>>,
    #[serde(skip)] redo: Vec<Vec<Drawing>>,
    /// open right-click menu and where it was opened
    #[serde(skip)] menu: Option<(Menu, Pos2)>,
    #[serde(skip)] replay: Option<Replay>,
    /// last price seen per base, to detect alert crossings
    #[serde(skip)] alert_px: std::collections::HashMap<String, f64>,
    /// alerts that fired since the app last notified: (title, body)
    #[serde(skip)] pub fired: Vec<(String, String)>,
    /// banner of the last alert and until when it shows
    #[serde(skip)] toast: Option<(String, std::time::Instant, Color32)>,
    /// one cell of a multi-chart layout: the legend names the interval
    #[serde(skip)] pub multi: bool,
    /// a layout cell's own symbol ("" = the active one); the active cell's is always ""
    pub sym: String,
    /// the cell's pair button was clicked this frame, and where it sits (a press there does not activate the cell)
    #[serde(skip)] pub pick: bool,
    #[serde(skip)] pub pick_rect: Rect,
    /// account line under the pointer last frame, the one being dragged, and a move awaiting
    /// confirmation (what, label, from, to, where)
    #[serde(skip)] line_hot: Option<usize>,
    #[serde(skip)] line_drag: Option<usize>,
    /// a move (Some(new price)) or cancel (None) of an account line waiting for confirmation
    #[serde(skip)] pending_move: Option<(LineAct, String, f64, Option<f64>, Pos2)>,
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
    /// custom indicators shown as panes: file stems in the indicators folder (`crate::script`)
    scripts: Vec<String>,
    /// their output for this frame's bars, `Pane::Script(i)` = `script_out[i]`
    #[serde(skip)] script_out: Vec<std::sync::Arc<crate::script::Out>>,
}

/// Defaults: an order-flow set. Heatmap + liquidity walls (where size rests), VWAP with bands and the
/// volume profile / auto S/R (where it traded), mark, breakouts and algo footprints; panes Volume,
/// CVD and OI. No moving averages or Bollinger (they repeat VWAP), no funding pane (the header shows
/// the predicted rate), so the price plot keeps most of the height.
impl Default for Chart {
    fn default() -> Self {
        Chart {
            tf: 1, src: None, heat: true, liqmap: false, liq: LiqCache::default(), view: charting::Chart::default(), reset_req: false, auto_req: false, bar_w: 7.0, cache: HeatCache::default(), walls_at: Default::default(),
            drawings: Default::default(), tool: None, pending: vec![], hot: None, drag: None, committed: None, undo: vec![], redo: vec![], menu: None, replay: None, alert_px: Default::default(), fired: vec![], toast: None, multi: false, sym: String::new(), pick: false, pick_rect: Rect::NOTHING, line_hot: None, line_drag: None, pending_move: None, actions: vec![], confirm: true, trading: false, equity: None, amends: false, risk_pct: 1.0, layout: "1".into(), layout_req: None, scripts: vec![], script_out: vec![], ind: Ind::default(), hide: false, alert_sound: "depth:chime".into(), ma: false, mas: default_mas(), magnet: true, sr: true, walls: true, mark: true, breakouts: true, vwap: true, bb: false, fp: false, fp_style: 0, whales: false, fills_on: true, algos: true, whale_usd: 100_000.0, cone: false, poly: false, odds: vec![], mark_ex: None, style: 2, lines: vec![], wants_options: false, fills: vec![], base_unit: String::new(), scale: Scale::Linear,
            panes: vec![(Pane::Vol, true), (Pane::Cvd, true), (Pane::Oi, true), (Pane::Funding, false), (Pane::Basis, false), (Pane::Ls, false), (Pane::Premium, false), (Pane::Ofi, false), (Pane::BookFlow, false), (Pane::Rsi, false), (Pane::Macd, false), (Pane::Delta, false), (Pane::Iv, false), (Pane::Skew, false)],
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



/// One paint phase of `Chart::show`: the frame's data and the widget's geometry.
struct Cx<'a> {
    a: &'a Agg, market: Market, smarket: Market, live: bool, editing: bool, loading_tf: bool,
    raw: &'a [Bar], bars: &'a [Bar], vis: &'a [Bar], i0: usize, i1: usize,
    /// the whole widget, the price area (legend rows and plot), the plot under the legend, the top of the time axis
    rect: Rect, main: Rect, main_plot: Rect, time_top: f32,
    fr: &'a Frame, ps: PScale, lo: f64, hi: f64,
    resp: &'a egui::Response,
    /// paints anywhere in the widget (the price axis column included)
    painter: &'a egui::Painter,
    /// the price grid's values and the bars of the time labels (the dotted grid)
    grid: &'a [f64], times: &'a [usize],
}

/// What a frame charts, besides the widget's geometry.
struct Data<'a> { a: &'a Agg, market: Market, smarket: Market, live: bool, loading_tf: bool, raw: &'a [Bar], bars: &'a [Bar], base: &'a str }

impl Data<'_> {
    fn cx<'b>(&'b self, f: &'b charting::Frame, editing: bool) -> Cx<'b> {
        let time_top = f.panes.last().map_or(f.plot.bottom(), |p| p.1.bottom());
        Cx { a: self.a, market: self.market, smarket: self.smarket, live: self.live, editing, loading_tf: self.loading_tf,
            raw: self.raw, bars: self.bars, vis: &self.bars[f.i0..=f.i1], i0: f.i0, i1: f.i1,
            rect: f.rect, main: f.legend.union(f.plot), main_plot: f.plot, time_top,
            fr: &f.axis, ps: f.scale, lo: f.scale.lo, hi: f.scale.hi, resp: f.response, painter: &f.full, grid: &f.grid, times: &f.times }
    }
}

/// The app's layers as the widget's overlay, with what one depth hands the next.
struct Ov<'a> {
    ch: &'a mut Chart, d: Data<'a>, panes: Vec<Pane>,
    /// auto-fit on (the "A" button lit)
    auto: bool,
    /// axis tags and labels placed so far (reserved in the widget's layout once the plot is painted)
    taken: Vec<Rect>,
    /// third legend row: each overlay's name, value and colour
    legend3: Vec<(String, String, Color32)>,
    ba: Option<(f64, f64)>, ba_rect: Option<Rect>, mark: Option<f64>, mark_ax: Option<Rect>,
    fp_text: bool, mas: Vec<(Vec<Option<f64>>, MaLine)>, lmax: f64,
    /// hovered bar (else the newest), the premium pane's lines, each pane's value range (for the crosshair)
    hi_idx: usize, prem: Vec<pane::Prem>, ranges: Vec<(Rect, Option<(f64, f64)>)>,
    /// the replay bar is drawn after the widget (it needs `&mut Ui`): plot, bar axis, hovered bar's time
    replay_at: Option<(Rect, Frame, Option<i64>)>,
}

impl charting::Overlay for Ov<'_> {
    fn paint(&mut self, z: Z, f: &mut charting::Frame) {
        let editing = self.ch.drag.is_some() || self.ch.line_drag.is_some();
        match z {
            Z::Back => {
                // the layers size bars from this frame's zoom; a released drag ends a drawing's move
                self.ch.bar_w = f.axis.bar_w;
                if f.response.drag_stopped() { self.ch.drag = None; }
                let cx = self.d.cx(f, editing);
                cx.painter.rect_filled(cx.rect, 0, panel());
                // the liquidation map is opaque (Hyperdash style): it goes first, so the grid and the book heatmap stay on top of it
                if let Some((c, usd)) = self.ch.liq_layer(f.ui, &cx) {
                    // the colour scale in words: dark purple $0, pale yellow this much (Hyperdash draws it as a bar)
                    let scale = usd.map_or(String::new(), |u| format!(" · $0 – ${}", fmt_usd_short(u)));
                    self.legend3.push((super::l("Liquidation map").into(), format!("{}{scale}", super::l("above: shorts · below: longs")), c));
                }
                self.ch.backdrop(f.ui, &cx);
            }
            Z::BelowCandles => {
                let cx = self.d.cx(f, editing);
                let (a, smarket, main) = (cx.a, cx.smarket, cx.main);
                // label layout: the last-price axis tag and the bid/ask box are placed first; every other
                // label moves up or down until it overlaps none of them (place())
                let last = cx.bars.last().unwrap();
                let ly = cx.ps.y(last.c);
                self.ba = if cx.live { bid_ask(a, self.ch.src, smarket, last.c) } else { None };
                self.ba_rect = self.ba.map(|(bid, ask)| {
                    let (ka, kb) = (t("tr.ask"), t("tr.bid"));
                    let p = cx.painter;
                    let lw = p.layout_no_wrap(ka.to_string(), prop(10.0), fg()).size().x.max(p.layout_no_wrap(kb.to_string(), prop(10.0), fg()).size().x);
                    let vw = p.layout_no_wrap(fmt_px(ask.max(bid)), mono(10.0), fg()).size().x;
                    let w = lw + vw + 16.0;
                    Rect::from_min_size(pos2(main.right() - w - 4.0, ly - 15.0), vec2(w, 30.0))
                });
                self.taken = vec![Rect::from_min_size(pos2(main.right() + 2.0, ly - 10.0), vec2(AXIS_W - 4.0, 34.0))];
                if let Some(r) = self.ba_rect { self.taken.push(r.expand(2.0)); }
                // the tags that matter most are reserved before any context level: mark price, then the
                // account's own position / order lines (drawn later, in their reserved slots)
                self.mark = if self.ch.mark && smarket == Market::Perp && cx.live {
                    match self.ch.src {
                        Some(e) => a.venues.get(&(e, Market::Perp)).and_then(|v| v.mark),
                        // the aggregate chart: the order venue's mark, the one the header shows (user choice 2026-11-03; an
                        // OI-weighted mean across venues sat a few bp off it, Hyperliquid / Lighter premiums pulling it)
                        None => self.ch.mark_ex.and_then(|e| a.venues.get(&(e, Market::Perp))).and_then(|v| v.mark),
                    }
                } else { None };
                self.mark_ax = self.mark.filter(|m| *m > cx.lo && *m < cx.hi).map(|mk| place(Rect::from_min_size(pos2(main.right() + 2.0, cx.ps.y(mk) - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut self.taken));
                self.ch.levels(&cx, &mut self.taken, &mut self.legend3);
                self.fp_text = self.ch.candles(&cx);
            }
            Z::AboveCandles => {
                let cx = self.d.cx(f, editing);
                (self.mas, self.lmax) = self.ch.studies(&cx, &mut self.legend3);
                self.ch.markers(f.ui, &cx, self.fp_text, self.ba_rect);
                self.ch.account_lines(f.ui, &cx, &mut self.taken);
                let last = cx.bars.last().unwrap();
                self.ch.last_price(&cx, last, cx.ps.y(last.c), self.ba, self.ba_rect, self.mark, self.mark_ax, &mut self.legend3);
                self.ch.drawings_ui(f.ui, f.response, &f.full, cx.bars, &f.axis, cx.main, f.scale, self.d.base);
                self.hi_idx = f.hover_i.unwrap_or(cx.bars.len() - 1);
                self.prem = if self.panes.contains(&Pane::Premium) { pane::premium_lines(cx.a, cx.smarket, cx.vis, self.ch.tf * 60_000, self.hi_idx.checked_sub(cx.i0)) } else { vec![] };
                // the widget's price labels skip what the app placed on the axis
                for r in self.taken.drain(..) { f.reserve(r); }
            }
            Z::Top => {
                let cx = self.d.cx(f, editing);
                // the crosshair follows the pointer over the plot, the legend and the panes
                let hover = f.hover_i.and(f.response.hover_pos());
                self.ch.axis_chrome(f.ui, &cx, self.auto);
                self.ch.crosshair(f.ui, &cx, f, hover, self.hi_idx, &self.ranges);
                self.ch.legend(&cx, self.hi_idx, std::mem::take(&mut self.mas), self.lmax, std::mem::take(&mut self.legend3));
                self.ch.paint_toast(f.ui, &cx);
                self.replay_at = Some((cx.main_plot, *cx.fr, hover.map(|_| cx.bars[self.hi_idx].t)));
            }
        }
    }

    fn pane(&mut self, id: u32, p: &mut charting::PaneFrame) {
        let Some(&pn) = self.panes.get(id as usize) else { return };
        p.range = self.ch.draw_pane(&p.painter, pn, p.rect, self.d.bars, p.i0, p.i1, &p.axis, self.hi_idx, &self.prem, &self.d.a.opt_min);
        self.ranges.push((p.rect, p.range));
    }

    /// A drag that starts on an account line (hit-tested last frame) moves it, else one on a drawing edits it; not
    /// on the price axis, and no account line while a drawing tool is armed (panning stays on: placing is click-only).
    fn claims_drag(&mut self, _: Pos2, on_axis: bool) -> bool {
        let ch = &mut *self.ch;
        ch.line_drag = if on_axis || ch.tool.is_some() { None } else { ch.line_hot };
        ch.drag = if on_axis || ch.line_drag.is_some() { None } else { ch.hot.filter(|h| h.1 != LOCKED) };
        ch.line_drag.is_some() || ch.drag.is_some()
    }

    /// A double-click on a drawing does not reset the view.
    fn allows_reset(&mut self, _: Pos2) -> bool { self.ch.hot.is_none() }
}

impl Chart {
    pub fn show(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str) {
        // one-time move to the calmer defaults (fewer overlays on by default); each stays a toggle
        if self.style < 2 { self.ma = false; self.walls = false; self.cone = false; self.style = 2; }
        // panes added after a layout was saved
        for p in [Pane::Premium, Pane::Ofi, Pane::BookFlow, Pane::Rsi, Pane::Macd, Pane::Delta, Pane::Iv, Pane::Skew] { if !self.panes.iter().any(|x| x.0 == p) { self.panes.push((p, false)); } }
        let smarket = if market == Market::Margin { Market::Spot } else { market };
        self.wants_options = self.panes.iter().any(|(p, on)| *on && matches!(p, Pane::Iv | Pane::Skew));
        self.toolbar(ui, a, smarket, base);
        self.base_unit = base.to_string();
        let Some((raw, bars, loading_tf, now, next_t, live)) = self.load_bars(ui, a, smarket) else { return };

        let mut panes: Vec<Pane> = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || smarket == Market::Perp)).map(|(p, _)| *p).collect();
        // custom indicators under the built-in panes; a deleted file just drops out
        let key = format!("{}|{}|{:?}|{:?}", self.base_unit, self.tf, self.src, smarket);
        self.script_out = self.scripts.iter().filter_map(|s| crate::script::run(s, &key, &bars)).collect();
        panes.extend((0..self.script_out.len()).map(|i| Pane::Script(i as u16)));
        // legend rows sit above the plot: OHLC, MAs, then the overlays' key values when any is on
        let overlays = self.sr || self.vwap || self.cone || self.poly || self.bb || ((self.mark || self.liqmap) && smarket == Market::Perp);
        let inset = if overlays && self.ma && self.mas.iter().any(|m| m.on) { 70.0 } else if overlays { 52.0 } else { 48.0 };
        // the price plot keeps at least 120 points: panes that do not fit stay off (a small layout cell)
        panes.truncate(((ui.available_height() - TIME_H - inset - 120.0) / SUB_H).max(0.0) as usize);
        // the footprint paints its own bars (`candles`)
        let fp_on = self.fp && a.fp.get(&(self.src, smarket)).is_some_and(|f| f.bin > 0.0);

        let mut w = std::mem::take(&mut self.view);
        (w.view.bar_w, w.scale, w.inset_top, w.axis_w, w.time_h) = (self.bar_w, self.scale, inset, AXIS_W, TIME_H);
        w.panes = (0..panes.len() as u32).map(|i| (i, SUB_H)).collect();
        w.max_bar_w = if self.fp { 160.0 } else { 60.0 };
        // the app's own last price (countdown, bid / ask, mark), crosshair (magnet, "+"), footprint and dotted grid
        (w.last_price, w.crosshair, w.candles, w.grid) = (false, false, !fp_on, false);
        (w.fmt_price, w.fmt_time) = (Some(fmt_dp as fn(f64, usize) -> String), Some(fmt::axis_time as fn(i64, charting::TimeLabel) -> String));
        let candles: Vec<charting::Candle> = bars.iter().map(|b| charting::Candle { t: b.t, o: b.o, h: b.h, l: b.l, c: b.c, v: b.vol }).collect();
        // another symbol / interval / source snaps the auto-fit; the interval's history arriving asks for the older page again
        let key = format!("{base}|{}|{:?}|{loading_tf}", self.tf, self.src);
        let auto = w.y_range.is_none();
        let bar_ms = self.tf * 60_000;
        let mut ov = Ov { ch: self, d: Data { a, market, smarket, live, loading_tf, raw: &raw, bars: &bars, base }, panes, auto, taken: vec![], legend3: vec![],
            ba: None, ba_rect: None, mark: None, mark_ax: None, fp_text: false, mas: vec![], lmax: 0.0, hi_idx: bars.len() - 1, prem: vec![], ranges: vec![], replay_at: None };
        let r = w.show(ui, &charting::ChartData { candles: &candles, bar_ms, key: &key }, &mut ov);
        let replay_at = ov.replay_at.take();
        if std::mem::take(&mut self.reset_req) { w.reset(); }
        if std::mem::take(&mut self.auto_req) { w.y_range = None; }
        self.bar_w = w.view.bar_w;
        self.view = w;
        // the view reached the oldest loaded bar: ask for the page before it (not while the interval's history is still loading)
        if r.reached_start && !loading_tf { self.actions.push(ChartAct::Older(native_tf(self.tf))); }
        if let Some((plot, fr, hover_t)) = replay_at { self.replay_ui(ui, &r.response, plot, &fr, hover_t, next_t, now); }
    }

    /// The bars to chart: the live 1m series resampled to the interval, stitched to native-interval
    /// history and cut at the replay bar; None (after drawing a placeholder) when there is nothing.
    /// Returns (1m bars, chart bars, interval history loading, time, next replay bar, live).
    fn load_bars(&mut self, ui: &mut Ui, a: &Agg, smarket: Market) -> Option<(Vec<Bar>, Vec<Bar>, bool, f64, Option<i64>, bool)> {
        let Some(series) = a.series.get(&(self.src, smarket)) else {
            ui.centered_and_justified(|ui| ui.label(t("chart.loading")));
            return None;
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
        if bars.is_empty() { ui.centered_and_justified(|ui| ui.label(t("chart.empty"))); return None; }
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
        Some((raw, bars, loading_tf, now, next_t, live))
    }

    /// Replay: picking the start bar (vertical line under the pointer, click to start), then the
    /// play / step / speed / exit bar at the bottom of the plot; Right arrow steps one bar.
    #[allow(clippy::too_many_arguments)]
    fn replay_ui(&mut self, ui: &mut Ui, resp: &egui::Response, plot: Rect, fr: &Frame, hover_t: Option<i64>, next_t: Option<i64>, now: f64) {
        let painter = &ui.painter_at(resp.rect);
        let Some(r) = self.replay.as_mut() else { return };
        if r.at.is_none() {
            if let (Some(p), Some(ht)) = (resp.hover_pos().filter(|p| plot.contains(*p)), hover_t) {
                let x = fr.x(fr.idx(p.x).round());
                painter.vline(x, plot.top()..=plot.bottom(), Stroke::new(1.5, accent()));
                painter.rect_filled(Rect::from_min_max(pos2(x, plot.top()), plot.right_bottom()), 0, panel().gamma_multiply(0.6));
                if resp.clicked() { r.at = Some(ht); r.last = now; self.view.view.right = 0.0; }
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

    /// The chart's live price for alerts: the composite, or the charted venue's own mid.
    pub fn price(&self, a: &Agg, market: Market) -> Option<f64> {
        let m = if market == Market::Margin || market == Market::Option { Market::Spot } else { market };
        match self.src { None => a.mid(m), Some(e) => a.venues.get(&(e, m)).and_then(|v| v.mid()) }
    }

    /// Back to auto-fit at the live edge: a manual price range from one symbol is meaningless for
    /// another (BTC at 80k vs a coin at 0.3 would leave the chart empty).
    pub fn reset_view(&mut self) { self.view.y_range = None; self.view.view.right = 0.0; self.pending.clear(); self.liq = LiqCache::default(); self.cache = HeatCache::default(); }

    /// How a coin opens from the Hunter page: 5 minute bars with the open interest and funding panes
    /// (liquidations are always drawn on the candles). Other panes stay as the user set them.
    pub fn hunter_view(&mut self) {
        self.tf = 5;
        for (p, on) in self.panes.iter_mut() { if matches!(p, Pane::Oi | Pane::Funding) { *on = true; } }
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

/// The file behind a sound id: a macOS system sound, or a bundled one written once to the cache
/// folder (the players need a file). None for "none", an unknown id, a system sound off macOS.
fn sound_path(id: &str) -> Option<std::path::PathBuf> {
    if let Some(n) = id.strip_prefix("sys:") {
        cfg!(target_os = "macos").then(|| std::path::PathBuf::from(format!("/System/Library/Sounds/{n}.aiff")))
    } else if let Some((k, _, wav)) = id.strip_prefix("depth:").and_then(|n| OWN_SOUNDS.iter().find(|(k, _, _)| *k == n)) {
        depth::sys::cache_dir().map(|d| d.join("sounds").join(format!("{k}.wav"))).filter(|p| p.exists() || {
            p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(p, wav).is_ok()
        })
    } else { None }
}

/// Plays an alert sound without blocking (any OS, through `sys::play_sound_file`).
pub fn play_sound(id: &str) {
    if let Some(p) = sound_path(id) { depth::sys::play_sound_file(&p); }
}

/// Sound picker as a submenu (keeps the right-click menu open); choosing one plays it.
fn sound_menu(ui: &mut Ui, cur: &mut String) {
    ui.menu_button(format!("{}: {}", t("alert.sound"), sound_name(cur)), |ui| {
        for (id, name) in sounds() {
            if ui.selectable_label(*cur == id, name).clicked() { play_sound(&id); *cur = id; }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `show` on 300 one-minute bars, one 1200 x 800 frame per event batch.
    fn frames(ctx: &egui::Context, ch: &mut Chart, a: &Agg, batches: Vec<Vec<egui::Event>>) {
        for ev in batches {
            let input = egui::RawInput { events: ev, screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| ch.show(ui, a, Market::Spot, "BTC"));
            // no renderer: the texture uploads are dropped on purpose
            out.textures_delta.clear();
        }
    }
    fn agg() -> Agg {
        let mut a = Agg::default();
        let t0 = 1_794_182_400_000;
        let bars = (0..300).map(|i| { let c = 80_000.0 + (i as f64 * 0.1).sin() * 400.0; Bar { t: t0 + i * 60_000, o: c - 50.0, h: c + 100.0, l: c - 100.0, c, vol: 1.0, ..Default::default() } }).collect();
        a.series.insert((None, Market::Spot), depth::agg::Series { bars });
        a
    }
    fn mv(x: f32, y: f32) -> egui::Event { egui::Event::PointerMoved(pos2(x, y)) }
    fn btn(x: f32, y: f32, down: bool) -> egui::Event { egui::Event::PointerButton { pos: pos2(x, y), button: egui::PointerButton::Primary, pressed: down, modifiers: Default::default() } }

    #[test]
    fn the_widget_pans_asks_for_older_bars_and_resets() {
        let (a, ctx) = (agg(), egui::Context::default());
        let mut ch = Chart { heat: false, ..Default::default() };
        frames(&ctx, &mut ch, &a, vec![vec![], vec![]]);
        assert!(ch.actions.is_empty() && ch.view.view.right == 0.0);
        // a long drag right pans back to the oldest bar: one request for the page before it
        let (x0, y) = (300.0, 300.0);
        frames(&ctx, &mut ch, &a, vec![vec![mv(x0, y)], vec![btn(x0, y, true)], vec![mv(x0 + 400.0, y)], vec![mv(x0 + 800.0, y)], vec![btn(x0 + 800.0, y, false)], vec![], vec![]]);
        assert!(ch.view.view.right > 100.0, "{:?}", ch.view.view);
        assert_eq!(ch.actions.iter().filter(|x| matches!(x, ChartAct::Older(1))).count(), 1, "{:?}", ch.actions);
        // a double-click on the empty plot goes back to the live edge
        frames(&ctx, &mut ch, &a, vec![vec![mv(x0, y)], vec![btn(x0, y, true)], vec![btn(x0, y, false)], vec![btn(x0, y, true)], vec![btn(x0, y, false)], vec![]]);
        assert_eq!(ch.view.view.right, 0.0);
    }

    #[test]
    fn an_account_line_drag_moves_the_line_not_the_chart() {
        let (a, ctx) = (agg(), egui::Context::default());
        let price = 80_000.0;
        let line = ChartLine { price, ex: None, tag: "Limit".into(), sub: "1".into(), col: Color32::RED, act: Some(LineAct::Preview("px")) };
        let mut ch = Chart { heat: false, confirm: false, lines: vec![line], ..Default::default() };
        frames(&ctx, &mut ch, &a, vec![vec![]]);
        // find the line: the pointer over it makes it hot
        let x = 600.0;
        let y = (60..700).step_by(3).map(|y| y as f32).find(|y| { frames(&ctx, &mut ch, &a, vec![vec![mv(x, *y)]]); ch.line_hot.is_some() }).expect("line found");
        frames(&ctx, &mut ch, &a, vec![vec![btn(x, y, true)], vec![mv(x + 30.0, y + 20.0)], vec![mv(x + 60.0, y + 40.0)], vec![btn(x + 60.0, y + 40.0, false)], vec![]]);
        assert_eq!(ch.view.view.right, 0.0, "the chart did not pan");
        let moved: Vec<f64> = ch.actions.iter().filter_map(|x| match x { ChartAct::Move(LineAct::Preview("px"), p) => Some(*p), _ => None }).collect();
        assert!(moved.len() == 1 && moved[0] < price, "{:?}", ch.actions);
    }

    #[test]
    fn a_drawing_drag_edits_it_and_every_layer_paints() {
        let (a, ctx) = (agg(), egui::Context::default());
        // the replay bar writes in the app's fonts
        theme::install(&ctx);
        let mut ch = Chart { heat: false, ..Default::default() };
        let t = a.series[&(None, Market::Spot)].bars[200].t;
        ch.drawings.insert("BTC".into(), vec![Drawing::new(Tool::HLine, (t, 80_000.0), (t, 80_000.0))]);
        frames(&ctx, &mut ch, &a, vec![vec![]]);
        let x = 500.0;
        let y = (60..700).step_by(3).map(|y| y as f32).find(|y| { frames(&ctx, &mut ch, &a, vec![vec![mv(x, *y)]]); ch.hot.is_some() }).expect("drawing found");
        frames(&ctx, &mut ch, &a, vec![vec![btn(x, y, true)], vec![mv(x + 30.0, y - 20.0)], vec![mv(x + 60.0, y - 40.0)], vec![btn(x + 60.0, y - 40.0, false)], vec![]]);
        assert_eq!(ch.view.view.right, 0.0, "the chart did not pan");
        assert!(ch.drawings["BTC"][0].a.1 > 80_000.0, "{:?}", ch.drawings["BTC"][0].a);
        // every pane, overlay and scale, the footprint switch and replay's start picker, under the pointer
        for (p, on) in ch.panes.iter_mut() { *on = !matches!(p, Pane::Iv | Pane::Skew); }
        (ch.ma, ch.bb, ch.cone, ch.fp, ch.whales) = (true, true, true, true, true);
        for sc in [Scale::Log, Scale::Percent] {
            ch.scale = sc;
            frames(&ctx, &mut ch, &a, vec![vec![mv(x, 300.0)], vec![mv(x, 700.0)]]);
        }
        ch.replay = Some(Replay::new(None, 0.0));
        frames(&ctx, &mut ch, &a, vec![vec![mv(x, 130.0)], vec![btn(x, 130.0, true)], vec![btn(x, 130.0, false)], vec![]]);
        assert!(ch.replay.is_some_and(|r| r.at.is_some()), "the click picked the replay bar");
    }

    #[test]
    fn sound_ids_resolve_to_files() {
        assert!(sound_path("depth:bell").is_some_and(|p| p.ends_with("sounds/bell.wav")));
        assert!(sound_path("none").is_none() && sound_path("depth:nope").is_none());
        // the system sounds exist on macOS only
        assert_eq!(sound_path("sys:Glass").is_some(), cfg!(target_os = "macos"));
    }
}
