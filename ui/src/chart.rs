//! Candle chart painted with egui: heatmap background, candles, liquidation bubbles, and
//! stacked sub-panes (volume, CVD, OI, funding predicted + settled, basis, long/short).
use super::{fmt_dp, fmt_px, kit, l, local_offset_ms, t, theme};
// colours, fonts and radius from the style the app sets each frame (`charting::style::set`)
use charting::style::*;
// venue colours and logos stay the app's
use theme::{ex_color, icon};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui};
use depth::agg::{Agg, Bar};
use depth::{Exchange, Market};

mod chrome;
mod draw;
mod fmt;
mod heat;
// leaf code lives in the `charting` crate; the old module paths stay as aliases
use charting::ta as ind;
use charting::coords::{bucket, grid, nice, padded_range, BarAxis as Frame, PriceScale as PScale};
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


#[derive(Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
enum Scale { #[default] Linear, Log, Percent }


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
fn grid_line(painter: &egui::Painter, a: Pos2, b: Pos2) {
    painter.extend(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, line()), 1.5, 2.5));
}

/// Axis label color for ordinary levels and times (round price levels use `mu()`).
fn axis_c() -> Color32 { lerp(dim(), mu(), 0.5) }

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
    /// alerts that fired since the app last notified: (title, body)
    #[serde(skip)] pub fired: Vec<(String, String)>,
    /// banner of the last alert and until when it shows
    #[serde(skip)] toast: Option<(String, std::time::Instant, Color32)>,
    /// one cell of a multi-chart layout: the legend names the interval
    #[serde(skip)] pub multi: bool,
    /// account line under the pointer last frame, the one being dragged, and a move awaiting
    /// confirmation (what, label, from, to, where)
    #[serde(skip)] line_hot: Option<usize>,
    #[serde(skip)] line_drag: Option<usize>,
    /// a move (Some(new price)) or cancel (None) of an account line waiting for confirmation
    #[serde(skip)] pending_move: Option<(LineAct, String, f64, Option<f64>, Pos2)>,
    /// requests for the app, drained each frame
    #[serde(skip)] pub actions: Vec<ChartAct>,
    /// (native interval, oldest bar) an `Older` request went out for; cleared when the view leaves the left edge
    #[serde(skip)] older_req: Option<(i64, i64)>,
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
            tf: 1, src: None, heat: true, liqmap: false, liq: LiqCache::default(), right: 0.0, bar_w: 7.0, cache: HeatCache::default(), walls_at: Default::default(),
            drawings: Default::default(), tool: None, pending: vec![], hot: None, drag: None, fling: 0.0, eased: None, committed: None, undo: vec![], redo: vec![], menu: None, replay: None, alert_px: Default::default(), fired: vec![], toast: None, multi: false, line_hot: None, line_drag: None, pending_move: None, actions: vec![], older_req: None, confirm: true, trading: false, equity: None, amends: false, risk_pct: 1.0, layout: "1".into(), layout_req: None, scripts: vec![], script_out: vec![], ind: Ind::default(), hide: false, alert_sound: "depth:chime".into(), ma: false, mas: default_mas(), magnet: true, sr: true, walls: true, mark: true, breakouts: true, vwap: true, bb: false, fp: false, fp_style: 0, whales: false, fills_on: true, algos: true, whale_usd: 100_000.0, cone: false, poly: false, odds: vec![], mark_ex: None, style: 2, lines: vec![], wants_options: false, fills: vec![], base_unit: String::new(), scale: Scale::Linear, y_range: None,
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



/// One frame of `Chart::show`, shared by its paint phases: data, geometry, price scale and grid.
struct Cx<'a> {
    a: &'a Agg, market: Market, smarket: Market, live: bool, editing: bool, loading_tf: bool,
    raw: &'a [Bar], bars: &'a [Bar], vis: &'a [Bar], i0: usize, i1: usize,
    rect: Rect, plot_w: f32, main: Rect, main_plot: Rect, fr: &'a Frame, ps: PScale, lo: f64, hi: f64,
    resp: &'a egui::Response, painter: &'a egui::Painter,
    /// price grid values, their step and decimals; percent labels are relative to `base_px`
    gv: &'a [f64], gdp_step: f64, gdp: usize, base_px: f64, pct: bool,
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

        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, panel());

        let mut panes: Vec<Pane> = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || smarket == Market::Perp)).map(|(p, _)| *p).collect();
        // custom indicators under the built-in panes; a deleted file just drops out
        let key = format!("{}|{}|{:?}|{:?}", self.base_unit, self.tf, self.src, smarket);
        self.script_out = self.scripts.iter().filter_map(|s| crate::script::run(s, &key, &bars)).collect();
        panes.extend((0..self.script_out.len()).map(|i| Pane::Script(i as u16)));
        let subs_h = panes.len() as f32 * SUB_H;
        let main_h = (rect.height() - TIME_H - subs_h).max(120.0);
        let plot_w = rect.width() - AXIS_W;
        let main = Rect::from_min_size(rect.min, vec2(plot_w, main_h));

        let (on_axis, editing, dt, y_wheel) = self.pan_zoom(ui, &resp, &bars, main);
        let n_vis = plot_w / self.bar_w;
        self.right = self.right.clamp(-n_vis * 0.5, bars.len() as f32);
        let right_edge = bars.len() as f32 - 1.0 + 4.0 - self.right;
        let fr = Frame { first: right_edge - n_vis, right_edge, bar_w: self.bar_w, plot: main };
        // the view reached the oldest loaded bar: ask for the page before it, once per oldest bar (and not
        // while the interval's history is still loading)
        if fr.first >= 1.0 { self.older_req = None; } else if !loading_tf {
            let req = (native_tf(self.tf), bars[0].t);
            if self.older_req != Some(req) { self.older_req = Some(req); self.actions.push(ChartAct::Older(req.0)); }
        }
        let i0 = fr.first.floor().max(0.0) as usize;
        let i1 = (right_edge.ceil() as usize).min(bars.len() - 1);
        if i0 > i1 { return; }
        let vis = &bars[i0..=i1];

        let (lo, hi) = self.price_range(ui, &resp, vis, main, base, on_axis, editing, dt, y_wheel);
        // legend rows sit above the plot: OHLC, MAs, then the overlays' key values when any is on
        let overlays = self.sr || self.vwap || self.cone || self.poly || self.bb || ((self.mark || self.liqmap) && smarket == Market::Perp);
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
        let cx = Cx { a, market, smarket, live, editing, loading_tf, raw: &raw, bars: &bars, vis, i0, i1, rect, plot_w, main, main_plot, fr: &fr, ps, lo, hi, resp: &resp, painter: &painter, gv: &gv, gdp_step, gdp, base_px, pct };

        // the liquidation map is opaque (Hyperdash style): it goes first, so the grid and the book heatmap stay on top of it
        let liq_legend = self.liq_layer(ui, &cx);
        self.backdrop(ui, &cx);

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
        if let Some((c, usd)) = liq_legend {
            // the colour scale in words: dark purple $0, pale yellow this much (Hyperdash draws it as a bar)
            let scale = usd.map_or(String::new(), |u| format!(" · $0 – ${}", fmt_usd_short(u)));
            legend3.push((super::l("Liquidation map").into(), format!("{}{scale}", super::l("above: shorts · below: longs")), c));
        }
        if let Some(r) = ba_rect { taken.push(r.expand(2.0)); }
        // the tags that matter most are reserved before any context level: mark price, then the
        // account's own position / order lines (drawn later, in their reserved slots)
        let mark = if self.mark && smarket == Market::Perp && live {
            match self.src {
                Some(e) => a.venues.get(&(e, Market::Perp)).and_then(|v| v.mark),
                // the aggregate chart: the order venue's mark, the one the header shows (user choice 2026-11-03; an
                // OI-weighted mean across venues sat a few bp off it, Hyperliquid / Lighter premiums pulling it)
                None => self.mark_ex.and_then(|e| a.venues.get(&(e, Market::Perp))).and_then(|v| v.mark),
            }
        } else { None };
        let mark_ax = mark.filter(|m| *m > lo && *m < hi).map(|mk| place(Rect::from_min_size(pos2(main.right() + 2.0, y(mk) - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken));

        self.levels(&cx, &mut taken, &mut legend3);
        let (fp_text, mas, lmax) = self.candles(&cx, &mut legend3);
        self.markers(ui, &cx, fp_text, ba_rect);
        self.account_lines(ui, &cx, &mut taken);
        self.last_price(&cx, last_bar, last_ly, ba, ba_rect, mark, mark_ax, &mut legend3);

        self.drawings_ui(ui, &resp, &painter, &bars, &fr, main, ps, base);

        let (hover, hi_idx) = self.axes(ui, &cx, panes, taken);
        self.legend(&cx, hi_idx, mas, lmax, legend3);
        self.replay_ui(ui, &resp, &painter, main_plot, &fr, hover.map(|_| bars[hi_idx].t), next_t, now);
        self.paint_toast(ui, &cx);
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

    /// Pan / zoom / fling and who gets a drag; returns (drag on the price axis, editing a line or
    /// drawing, frame time, wheel delta over the price axis).
    fn pan_zoom(&mut self, ui: &Ui, resp: &egui::Response, bars: &[Bar], main: Rect) -> (bool, bool, f32, f32) {
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
        // pan, glide, wheel zoom around the pointer, horizontal scroll, double-click reset (not on a drawing)
        let mut tv = charting::interact::TimeView { right: self.right, bar_w: self.bar_w, fling: self.fling };
        let (dt, y_wheel, reset) = charting::interact::pan_zoom(ui, resp, main, bars.len(), &mut tv, if self.fp { 160.0 } else { 60.0 }, editing, on_axis, self.hot.is_none());
        (self.right, self.bar_w, self.fling) = (tv.right, tv.bar_w, tv.fling);
        if reset { self.y_range = None; }
        // a released drag ends a drawing's move
        if resp.drag_stopped() { self.drag = None; }
        (on_axis, editing, dt, y_wheel)
    }

    /// The price range shown: the eased auto-fit, or the manual range the wheel and drags set.
    #[allow(clippy::too_many_arguments)]
    fn price_range(&mut self, ui: &Ui, resp: &egui::Response, vis: &[Bar], main: Rect, base: &str, on_axis: bool, editing: bool, dt: f32, y_wheel: f32) -> (f64, f64) {
        // auto-fit toward the visible range (eased; another symbol / interval / source snaps), or the manual range
        let target = padded_range(vis.iter().flat_map(|b| [b.l, b.h]), 0.06, false);
        let key = (base.to_string(), self.tf, self.src);
        charting::interact::price_range(ui, resp, main, target, key, &mut self.eased, &mut self.y_range, on_axis, editing, dt, y_wheel)
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

    /// The chart's live price for alerts: the composite, or the charted venue's own mid.
    pub fn price(&self, a: &Agg, market: Market) -> Option<f64> {
        let m = if market == Market::Margin || market == Market::Option { Market::Spot } else { market };
        match self.src { None => a.mid(m), Some(e) => a.venues.get(&(e, m)).and_then(|v| v.mid()) }
    }

    /// Back to auto-fit at the live edge: a manual price range from one symbol is meaningless for
    /// another (BTC at 80k vs a coin at 0.3 would leave the chart empty).
    pub fn reset_view(&mut self) { self.y_range = None; self.right = 0.0; self.pending.clear(); self.liq = LiqCache::default(); self.cache = HeatCache::default(); }

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

    #[test]
    fn sound_ids_resolve_to_files() {
        assert!(sound_path("depth:bell").is_some_and(|p| p.ends_with("sounds/bell.wav")));
        assert!(sound_path("none").is_none() && sound_path("depth:nope").is_none());
        // the system sounds exist on macOS only
        assert_eq!(sound_path("sys:Glass").is_some(), cfg!(target_os = "macos"));
    }
}
