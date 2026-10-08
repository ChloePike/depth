//! Candle chart painted with egui: heatmap background, candles, liquidation bubbles, and
//! stacked sub-panes (volume, CVD, OI, funding predicted + settled, basis, long/short).
use super::{fmt_px, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui};
use terminal_one::agg::{Agg, Bar, Heatmap};
use terminal_one::{Exchange, Market};

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
enum Pane { Vol, Cvd, Oi, Funding, Basis, Ls, Premium }

impl Pane {
    fn key(self) -> &'static str {
        match self {
            Pane::Vol => "chart.vol", Pane::Cvd => "chart.cvd", Pane::Oi => "chart.oi",
            Pane::Funding => "chart.funding", Pane::Basis => "chart.basis", Pane::Ls => "chart.ls",
            Pane::Premium => "chart.premium",
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
enum Tool { HLine, Trend, Ray }

/// A price line from the account: entry of a position or a resting order.
/// Drawn dashed with a tag at the left edge ("tag | sub") and the price on the axis.
pub struct ChartLine { pub price: f64, pub tag: String, pub sub: String, pub col: Color32 }

const MA: [(usize, Color32); 3] = [(7, Color32::from_rgb(0xf0, 0xb9, 0x0b)), (25, Color32::from_rgb(0xe0, 0x5b, 0xc4)), (99, Color32::from_rgb(0x9b, 0x8c, 0xf0))];

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

/// A user drawing anchored in (bar time ms, price), so it survives timeframe changes.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Drawing { tool: Tool, a: (i64, f64), b: (i64, f64) }

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
    /// MA(7/25/99) on the price chart
    ma: bool,
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
    #[serde(skip)] tool: Option<Tool>,
    /// first point of a two-point drawing in progress
    #[serde(skip)] pending: Option<(i64, f64)>,
}

impl Default for Chart {
    fn default() -> Self {
        Chart {
            tf: 1, src: None, heat: true, liqmap: false, liq: LiqCache::default(), right: 0.0, bar_w: 7.0, cache: HeatCache::default(),
            drawings: Default::default(), tool: None, pending: None, ma: false, sr: true, walls: false, mark: true, breakouts: true, vwap: true, cone: false, style: 2, lines: vec![], base_unit: String::new(), scale: Scale::Linear, y_range: None,
            panes: vec![(Pane::Vol, true), (Pane::Cvd, true), (Pane::Oi, true), (Pane::Funding, true), (Pane::Basis, false), (Pane::Ls, false), (Pane::Premium, false)],
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

fn nice(x: f64) -> f64 { terminal_one::agg::nice(x.max(1e-12)) }

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

/// Estimated liquidation levels left open after `bars`. Model: every OI increase opens a long
/// and a short at the bar's typical price, spread over LEVERAGE; a level is wiped when price
/// trades through it, and OI decreases shrink every level proportionally.
/// ponytail: fixed leverage mix and maintenance margin, no per-venue position data; refine with
/// venue leverage/position distributions if they ever become available.
const LEVERAGE: [(f64, f64); 4] = [(10.0, 0.3), (25.0, 0.3), (50.0, 0.25), (100.0, 0.15)];
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
    let mut prev_oi: Option<f64> = None;
    let at = |p: f64| ((p / bin).round() as i64 - lo).clamp(0, bins as i64 - 1) as usize;
    for b in bars {
        if let (Some(oi), Some(p0)) = (b.oi, prev_oi) {
            let d = oi - p0;
            if d > 0.0 {
                let p = (b.h + b.l + b.c) / 3.0;
                for (lev, w) in LEVERAGE {
                    long[at(p * (1.0 - 1.0 / lev + MMR))] += (d * w) as f32;
                    short[at(p * (1.0 + 1.0 / lev - MMR))] += (d * w) as f32;
                }
            } else if p0 > 0.0 {
                let k = (oi / p0).max(0.0) as f32;
                long.iter_mut().chain(short.iter_mut()).for_each(|q| *q *= k);
            }
        }
        if b.oi.is_some() { prev_oi = b.oi; }
        // price traded through: longs at or above the low and shorts at or below the high are gone
        long[at(b.l)..].iter_mut().for_each(|q| *q = 0.0);
        short[..=at(b.h)].iter_mut().for_each(|q| *q = 0.0);
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
    /// (tf, bar count, last bar time, last OI bits): recomputed only when one of them changes
    key: (i64, usize, i64, u64),
    map: Option<LiqMap>,
}

impl LiqCache {
    fn get(&mut self, bars: &[Bar], tf: i64) -> Option<&LiqMap> {
        let last = bars.last()?;
        let key = (tf, bars.len(), last.t, last.oi.unwrap_or(0.0).to_bits());
        if key != self.key { self.key = key; self.map = liq_levels(bars); }
        self.map.as_ref()
    }
}

/// Best bid/ask for the price line: the venue's own book when one venue is charted; for the
/// aggregated (index) chart, the tightest venue half-spread around the index (the same top the
/// mid-aligned aggregated book shows). Display only.
fn bid_ask(a: &Agg, src: Option<Exchange>, m: Market, index: f64) -> Option<(f64, f64)> {
    let top = |v: &terminal_one::agg::Venue| match (v.book.best_bid(), v.book.best_ask()) {
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
        if !self.panes.iter().any(|p| p.0 == Pane::Premium) { self.panes.push((Pane::Premium, false)); }
        let smarket = if market == Market::Margin { Market::Spot } else { market };
        // hosted: the native toolbar above the chart drives these through native_set
        if !native() { self.toolbar(ui, a, smarket, base); }
        self.base_unit = base.to_string();
        let Some(series) = a.series.get(&(self.src, smarket)) else {
            ui.centered_and_justified(|ui| ui.label(t("chart.loading")));
            return;
        };
        let raw: Vec<Bar> = series.bars.iter().cloned().collect();
        let mut bars = resample(&raw, self.tf);
        // native-interval history, resampled when the interval is built from a smaller one
        let nt = native_tf(self.tf);
        let tf_hist = (nt > 1).then(|| a.tf.get(&(nt as u32)).and_then(|s| s.bars(self.src, smarket))).flatten()
            .map(|h| if nt == self.tf { h.to_vec() } else { resample(h, self.tf) });
        let loading_tf = nt > 1 && tf_hist.is_none();
        if let (Some(h), Some(first)) = (tf_hist.as_deref(), raw.first()) { bars = stitch(h, bars, first.t, self.tf); }
        if bars.is_empty() { ui.centered_and_justified(|ui| ui.label(t("chart.empty"))); return; }

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
        if resp.dragged() && !on_axis { self.right += resp.drag_delta().x / self.bar_w; }
        if resp.hover_pos().is_some_and(|p| p.x > main.right() && p.y < main.bottom()) { ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical); }
        // wheel: over the price axis it scales prices, over the plot it zooms time
        let mut y_wheel = 0.0f32;
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            let over_axis = resp.hover_pos().is_some_and(|p| p.x > main.right() && p.y < main.bottom());
            if dy != 0.0 { if over_axis { y_wheel = dy; } else { self.bar_w = (self.bar_w * (dy * 0.003).exp()).clamp(2.0, 40.0); } }
        }
        if resp.double_clicked() { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; }
        let n_vis = plot_w / self.bar_w;
        self.right = self.right.clamp(-n_vis * 0.5, bars.len() as f32);
        let right_edge = bars.len() as f32 - 1.0 + 4.0 - self.right;
        let fr = Frame { first: right_edge - n_vis, right_edge, bar_w: self.bar_w, plot: main };
        let i0 = fr.first.floor().max(0.0) as usize;
        let i1 = (right_edge.ceil() as usize).min(bars.len() - 1);
        if i0 > i1 { return; }
        let vis = &bars[i0..=i1];

        // price scale
        let (alo, ahi) = padded_range(vis.iter().flat_map(|b| [b.l, b.h]), 0.06, false);
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
            } else if self.y_range.is_some() {
                let shift = dy / ph * (hi - lo);
                (lo, hi) = (lo + shift, hi + shift);
                self.y_range = Some((lo, hi));
            }
        }
        if !(hi > lo && lo.is_finite() && hi.is_finite()) { (lo, hi) = (alo, ahi); self.y_range = None; }
        // legend rows sit above the plot: OHLC, MAs, then the overlays' key values when any is on
        let overlays = self.sr || self.vwap || self.cone || (self.mark && smarket == Market::Perp);
        let main_plot = Rect::from_min_max(pos2(main.left(), main.top() + if overlays { 66.0 } else { 48.0 }), pos2(main.right(), main.bottom() - 6.0));
        let ps = PScale { lo, hi, log: self.scale == Scale::Log && lo > 0.0, r: main_plot };
        let y = |v: f64| ps.y(v);
        // percent scale: labels relative to the first visible close
        let base_px = vis.first().map_or(1.0, |b| b.c);
        let pct = self.scale == Scale::Percent;
        // as many decimals as the grid step needs (83,000 not 83,000.0; 0.4512 when the step is 0.0005)
        let gv: Vec<f64> = grid(lo, hi, (main_plot.height() as f64 / 60.0).max(2.0));
        let gdp = gv.windows(2).map(|w| (w[1] - w[0]).abs()).fold(f64::MAX, f64::min);
        let gdp = if gdp.is_finite() && gdp > 0.0 { super::step_dp(gdp) } else { 2 };
        let axis_label = |v: f64| if pct { format!("{:+.2}%", (v / base_px - 1.0) * 100.0) } else { super::fmt_dp(v, gdp) };

        // grid + price axis
        for v in gv.iter().copied() {
            let yy = y(v);
            painter.hline(main.left()..=main.right(), yy, Stroke::new(1.0, GRID));
            painter.text(pos2(main.right() + 8.0, yy), Align2::LEFT_CENTER, axis_label(v), mono(11.0), MU);
        }
        // watermark: the instrument, faint, behind everything
        let wm = format!("{base}USDT · {}", self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| t("src.agg").to_string()));
        // hosted: the toolbar already names the market; the watermark is just noise behind the candles
        if !native() { painter.text(main_plot.center(), Align2::CENTER_CENTER, wm, prop(44.0), Color32::from_white_alpha(9)); }
        else { painter.text(pos2(main_plot.left() + 14.0, main_plot.bottom() - 12.0), Align2::LEFT_BOTTOM, "Depth", prop(30.0), Color32::from_white_alpha(14)); }

        // one textured quad: x by bar index, y by price of the top/bottom bin edges
        let quad = |tex: egui::TextureId, g: TexGeom, bin: f64| {
            let i0f = ((g.t0 - bars[0].t) / g.ms) as f32;
            let r = Rect::from_min_max(
                pos2(fr.x(i0f) - self.bar_w / 2.0, y((g.lo + g.bins as i64) as f64 * bin - bin / 2.0)),
                pos2(fr.x(i0f + g.cols as f32 - 1.0) + self.bar_w / 2.0, y(g.lo as f64 * bin - bin / 2.0)));
            painter.with_clip_rect(main_plot).image(tex, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        };
        // heatmap (cross-venue book, USD) behind candles
        if self.heat && self.src.is_none() {
            if let Some(h) = a.heat.get(&smarket) {
                self.cache.update(h, self.tf, smarket);
                // recorded live from the book: on long intervals it starts as the current bar only
                if let Some((tex, g)) = self.cache.texture(ui.ctx()) { quad(tex, g, self.cache.bin); }
            }
        }
        // estimated liquidation map: horizontal profile on the right edge of the price plot,
        // longs (below price) red, shorts (above) green, merged into ~2px rows
        if self.liqmap && smarket == Market::Perp {
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
                    let w = 90.0;
                    let clip = painter.with_clip_rect(main_plot);
                    for (k, (l, s)) in rows {
                        let (q, col) = if l >= s { (l, dn()) } else { (s, up()) };
                        let (y0, y1) = (y((k + 1) as f64 * row), y(k as f64 * row));
                        let len = w * (q / qmax).sqrt();
                        clip.rect_filled(Rect::from_min_max(pos2(main.right() - len, y0), pos2(main.right(), y1.max(y0 + 1.0))), 0, col.linear_multiply(0.12 + 0.38 * q / qmax));
                    }
                }
            }
        }

        // label layout: the last-price axis tag and the bid/ask box are placed first; every other
        // label moves up or down until it overlaps none of them (place())
        let last_bar = bars.last().unwrap();
        let last_ly = y(last_bar.c);
        let ba = bid_ask(a, self.src, smarket, last_bar.c);
        let ba_rect = ba.map(|(bid, ask)| {
            let (ka, kb) = (t("tr.ask"), t("tr.bid"));
            let lw = painter.layout_no_wrap(ka.to_string(), prop(10.0), FG).size().x.max(painter.layout_no_wrap(kb.to_string(), prop(10.0), FG).size().x);
            let vw = painter.layout_no_wrap(fmt_px(ask.max(bid)), mono(10.0), FG).size().x;
            let w = lw + vw + 16.0;
            Rect::from_min_size(pos2(main.right() - w - 4.0, last_ly - 15.0), vec2(w, 30.0))
        });
        let mut taken: Vec<Rect> = vec![Rect::from_min_size(pos2(main.right() + 2.0, last_ly - 10.0), vec2(AXIS_W - 4.0, 34.0))];
        // third legend row: each overlay's name, colour and current value, so no line needs guessing
        let mut legend3: Vec<(String, String, Color32)> = vec![];
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
                let col = if i == p.poc() { Color32::from_rgb(0xf0, 0xb9, 0x0b) } else if (va0..=va1).contains(&i) { accent() } else { MU };
                // anchored on the left: the right edge already carries the book depth and liquidation profiles
                clip.rect_filled(Rect::from_min_max(pos2(main.left(), y0 + 0.5), pos2(main.left() + len, (y1 - 0.5).max(y0 + 1.0))), 0, col.linear_multiply(0.14));
            }
            let poc = p.px(p.poc());
            let gold = Color32::from_rgb(0xf0, 0xb9, 0x0b);
            legend3.push((t("chart.poc").into(), fmt_px(poc), gold));
            legend3.push((t("chart.va").into(), format!("{} – {}", fmt_px(p.lo + va0 as f64 * p.bin), fmt_px(p.lo + (va1 + 1) as f64 * p.bin)), accent()));
            clip.hline(main.left()..=main.right(), y(poc), Stroke::new(1.2, gold.linear_multiply(0.85)));
            let mut tagged = vec![(poc, format!("POC {}", fmt_px(poc)), gold)];
            for (px, k) in [(p.lo + (va1 + 1) as f64 * p.bin, "VAH"), (p.lo + va0 as f64 * p.bin, "VAL")] {
                painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), y(px)), pos2(main.right(), y(px))], Stroke::new(1.0, accent().linear_multiply(0.7)), 5.0, 4.0));
                tagged.push((px, format!("{k} {}", fmt_px(px)), accent()));
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
                tagged.push((l.px, format!("{k} {}", fmt_px(l.px)), col));
            }
            for (px, tag, col) in tagged {
                if !(px > lo && px < hi) { continue; }
                let g = painter.layout_no_wrap(tag, mono(10.5), col);
                let r = place(Rect::from_min_size(pos2(main.left() + w + 8.0, y(px) - 9.0), vec2(g.size().x + 10.0, 16.0)), &mut taken);
                painter.rect_filled(r, 3, LABEL_BG);
                painter.galley(r.center() - g.size() / 2.0, g, col);
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
                painter.rect_filled(r, 3, LABEL_BG);
                painter.rect_stroke(r, 3, Stroke::new(1.0, col), egui::StrokeKind::Inside);
                painter.galley(r.min + vec2(5.0, 2.0), g, col);
            }
        }
        // expected move: EWMA volatility of 1m returns, widened with sqrt(time) over the next 30 bars
        if self.cone {
            if let (Some(vm), Some(lb)) = (terminal_one::quant::vol_model(&raw), bars.last()) {
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
                        painter.text(r.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, fmt_px(m), mono(10.5), Color32::BLACK);
                    }
                }
            }
        }
        // liquidity walls: aggregated resting size far above the book's typical level, within 3%
        if self.walls && self.src.is_none() {
            if let Some(mid) = a.mid(smarket) {
                let bin = nice(mid * 2e-4);
                let (bids, asks) = a.book_where(smarket, bin, 0.03, true, |_| true);
                let mut qs: Vec<f64> = bids.iter().chain(&asks).map(|l| l.qty).collect();
                qs.sort_by(f64::total_cmp);
                if let Some(&med) = qs.get(qs.len() / 2) {
                    for (side, ask) in [(&bids, false), (&asks, true)] {
                        let mut big: Vec<&terminal_one::agg::Level> = side.iter().filter(|l| l.qty > med * 5.0).collect();
                        big.sort_by(|x, z| z.qty.total_cmp(&x.qty));
                        for l in big.into_iter().take(2).filter(|l| l.px > lo && l.px < hi) {
                            let col = if ask { dn() } else { up() };
                            let ly = y(l.px);
                            let x0 = main.right() - main.width() * 0.35;
                            painter.extend(egui::Shape::dotted_line(&[pos2(x0, ly), pos2(main.right(), ly)], col.linear_multiply(0.8), 5.0, 1.2));
                            let tag = format!("{} ${}", if ask { "Ask wall" } else { "Bid wall" }, fmt_usd_short(l.qty * l.px));
                            let g = painter.layout_no_wrap(tag, mono(10.5), col);
                            let r = place(Rect::from_min_size(pos2(x0 + 2.0, ly - 15.0), g.size() + vec2(6.0, 2.0)), &mut taken);
                            painter.rect_filled(r, 2, LABEL_BG.gamma_multiply(0.85));
                            painter.galley(r.min + vec2(3.0, 1.0), g, col);
                        }
                    }
                }
            }
        }

        // candles
        let bw = (self.bar_w * 0.7).max(1.0);
        for (k, b) in vis.iter().enumerate() {
            let x = fr.x((i0 + k) as f32);
            let col = if b.c >= b.o { up() } else { dn() };
            painter.vline(x, y(b.h)..=y(b.l), Stroke::new(1.0, col));
            let (top, bot) = (y(b.o.max(b.c)), y(b.o.min(b.c)));
            painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, top), pos2(x + bw / 2.0, bot.max(top + 1.0))), 0, col);
        }

        // moving averages of the close, computed over every loaded bar so the left edge is right
        let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
        let mas: Vec<Vec<Option<f64>>> = if self.ma { MA.iter().map(|(n, _)| sma(&closes, *n)).collect() } else { vec![] };
        for (m, (_, col)) in mas.iter().zip(MA) {
            let pts: Vec<Pos2> = (i0..=i1).filter_map(|i| Some(pos2(fr.x(i as f32), y(m[i]?)))).collect();
            if pts.len() > 1 { painter.with_clip_rect(main_plot).add(egui::Shape::line(pts, Stroke::new(1.2, col))); }
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
            painter.line_segment([pos2(x, yy), pos2(end, yy)], Stroke::new(1.0, MU));
            painter.text(pos2(end + if left { -3.0 } else { 3.0 }, yy), align, fmt_px(v), mono(10.5), FG);
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

        // account lines: positions and resting orders of this symbol
        for l in &self.lines {
            if !(l.price > lo && l.price < hi) { continue; }
            let ly = y(l.price);
            dashed(&painter, main.left(), main.right(), ly, l.col.linear_multiply(0.8));
            let g1 = painter.layout_no_wrap(l.tag.clone(), prop(11.0), Color32::WHITE);
            let g2 = painter.layout_no_wrap(l.sub.clone(), mono(11.0), l.col);
            let x0 = main.left() + 6.0;
            let both = place(Rect::from_min_size(pos2(x0, ly - 10.0), vec2(g1.size().x + g2.size().x + 28.0, 20.0)), &mut taken);
            let r1 = Rect::from_min_size(both.min, vec2(g1.size().x + 14.0, 20.0));
            let r2 = Rect::from_min_size(pos2(r1.right(), both.top()), vec2(g2.size().x + 14.0, 20.0));
            painter.rect_filled(r1.union(r2), 3, LABEL_BG);
            painter.rect_filled(r1, egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 }, l.col);
            painter.rect_stroke(r1.union(r2), 3, Stroke::new(1.0, l.col), egui::StrokeKind::Inside);
            painter.galley(r1.center() - g1.size() / 2.0, g1, Color32::WHITE);
            painter.galley(r2.center() - g2.size() / 2.0, g2, l.col);
            let ax = place(Rect::from_min_size(pos2(main.right() + 2.0, ly - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken);
            painter.rect_filled(ax, 3, LABEL_BG);
            painter.rect_stroke(ax, 3, Stroke::new(1.0, l.col), egui::StrokeKind::Inside);
            painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(l.price), mono(11.0), l.col);
        }

        // last price: dashed line; axis tag with the bar-close countdown; bid/ask box beside it
        let last = last_bar;
        let ly = last_ly;
        let lc = if last.c >= last.o { up() } else { dn() };
        dashed(&painter, main.left(), main.right(), ly, lc.linear_multiply(0.7));
        let lab = Rect::from_min_size(pos2(main.right() + 2.0, ly - 10.0), vec2(AXIS_W - 4.0, 34.0));
        painter.rect_filled(lab, 3, lc);
        painter.text(pos2(lab.left() + 6.0, ly), Align2::LEFT_CENTER, fmt_px(last.c), mono(11.5), Color32::WHITE);
        let left_ms = last.t + self.tf * 60_000 - terminal_one::now_ms();
        painter.text(pos2(lab.left() + 6.0, ly + 15.0), Align2::LEFT_CENTER, countdown(left_ms), mono(10.5), Color32::from_white_alpha(210));
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

        // mark price: the charted venue's, or the OI-weighted mark across perp venues (USD) on the aggregate
        if self.mark && smarket == Market::Perp {
            let mark = match self.src {
                Some(e) => a.venues.get(&(e, Market::Perp)).and_then(|v| v.mark),
                None => {
                    let (mut num, mut den) = (0.0, 0.0);
                    for ((e, m), v) in a.venues.iter().filter(|((_, m), _)| *m == Market::Perp) {
                        if let (Some(mk), Some(oi)) = (v.mark, v.oi) { let w = oi.max(1e-9); num += mk * a.usd(*e, *m) * w; den += w; }
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
                painter.text(ax.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, fmt_px(mk), mono(10.5), Color32::WHITE);
                painter.text(pos2(main.right() - 4.0, my - 7.0), Align2::RIGHT_CENTER, t("chart.mark"), prop(10.0), PURPLE);
            }
        }

        self.drawings_ui(ui, &resp, &painter, &bars, &fr, main, ps, base);

        // sub panes
        let mut top = main.bottom();
        let mut pane_rects = vec![];
        for p in &panes {
            let r = Rect::from_min_size(pos2(rect.left(), top), vec2(plot_w, SUB_H));
            painter.hline(rect.left()..=rect.right(), r.top(), Stroke::new(1.0, LINE));
            pane_rects.push((*p, r));
            top += SUB_H;
        }

        // hover index
        let hover = resp.hover_pos().filter(|p| p.x < main.right());
        let hi_idx = hover.map(|p| fr.idx(p.x).round().clamp(i0 as f32, i1 as f32) as usize).unwrap_or(bars.len() - 1);
        let prem = if panes.contains(&Pane::Premium) { premium_lines(a, smarket, vis, self.tf * 60_000, hi_idx.checked_sub(i0)) } else { vec![] };
        for (p, r) in &pane_rects { self.draw_pane(&painter, *p, *r, vis, i0, &fr, &bars[hi_idx], &prem); }

        // time axis
        let ty = top + TIME_H / 2.0;
        painter.hline(rect.left()..=rect.right(), top, Stroke::new(1.0, LINE));
        let every = [1usize, 2, 3, 5, 10, 15, 30, 60, 120, 240, 480].into_iter().find(|k| *k as f32 * self.bar_w >= 90.0).unwrap_or(960);
        for (k, b) in vis.iter().enumerate() {
            if ((b.t / (self.tf * 60_000)) as usize) % every != 0 { continue; }
            let x = fr.x((i0 + k) as f32);
            painter.vline(x, main.top()..=top, Stroke::new(1.0, GRID));
            painter.text(pos2(x, ty), Align2::CENTER_CENTER, fmt_time(b.t, every as i64 * self.tf >= 1440), mono(10.5), MU);
        }
        painter.vline(main.right(), rect.top()..=rect.bottom(), Stroke::new(1.0, LINE));
        // scale toggles in the corner under the price axis: % and log (click again for linear)
        let corner = Rect::from_min_max(pos2(main.right() + 1.0, top + 1.0), pos2(rect.right(), rect.bottom()));
        let ar = Rect::from_min_size(pos2(corner.left() + 50.0, corner.top() + 1.0), vec2(18.0, corner.height() - 2.0));
        let auto_resp = ui.interact(ar, ui.id().with("auto"), Sense::click());
        let auto = self.y_range.is_none();
        if auto || auto_resp.hovered() { painter.rect_filled(ar, 3, HL); }
        painter.text(ar.center(), Align2::CENTER_CENTER, "A", mono(10.5), if auto { accent() } else { MU });
        if auto_resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(t("chart.auto_tip")).clicked() { self.y_range = None; }
        for (i, (sc, lab)) in [(Scale::Percent, "%"), (Scale::Log, "log")].into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(corner.left() + 2.0 + i as f32 * 22.0, corner.top() + 1.0), vec2(20.0 + i as f32 * 6.0, corner.height() - 2.0));
            let rs = ui.interact(r, ui.id().with(("scale", i)), Sense::click());
            let on = self.scale == sc;
            if on || rs.hovered() { painter.rect_filled(r, 3, HL); }
            painter.text(r.center(), Align2::CENTER_CENTER, lab, mono(10.5), if on { accent() } else { MU });
            if rs.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.scale = if on { Scale::Linear } else { sc }; }
        }

        // crosshair
        if let Some(p) = hover {
            let x = fr.x(hi_idx as f32);
            // dashed and faint: the crosshair must not hide the candles and lines under it
            let cross = Stroke::new(1.0, MU.linear_multiply(0.55));
            painter.extend(egui::Shape::dashed_line(&[pos2(x, rect.top()), pos2(x, top)], cross, 4.0, 4.0));
            if main_plot.contains(p) {
                painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), p.y), pos2(main.right(), p.y)], cross, 4.0, 4.0));
                let v = ps.v(p.y);
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, p.y - 9.0), vec2(AXIS_W - 2.0, 18.0));
                painter.rect_filled(lab, 2, HL);
                painter.text(lab.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, axis_label(v), mono(11.0), FG);
            }
            let tl = Rect::from_center_size(pos2(x, ty), vec2(110.0, TIME_H - 2.0));
            painter.rect_filled(tl, 2, HL);
            painter.text(tl.center(), Align2::CENTER_CENTER, fmt_time_full(bars[hi_idx].t), mono(10.5), FG);
        }

        // legend: row 1 time + OHLC + change + range (labels muted, values colored), row 2 MAs
        let b = &bars[hi_idx];
        let chg = (b.c / b.o - 1.0) * 100.0;
        let range = if b.l > 0.0 { (b.h / b.l - 1.0) * 100.0 } else { 0.0 };
        let c = if b.c >= b.o { up() } else { dn() };
        let kv = |x: f32, yy: f32, k: &str, v: String, col: Color32| -> f32 {
            let r = painter.text(pos2(x, yy), Align2::LEFT_CENTER, k, prop(11.5), DIM);
            painter.text(pos2(r.right() + 5.0, yy), Align2::LEFT_CENTER, v, mono(11.5), col).right() + 12.0
        };
        let y1 = main.top() + 12.0;
        let mut lx = painter.text(pos2(main.left() + 8.0, y1), Align2::LEFT_CENTER, fmt_time_full(b.t), mono(11.5), MU).right() + 12.0;
        for (k, v) in [("chart.o", b.o), ("chart.h", b.h), ("chart.l", b.l), ("chart.c", b.c)] { lx = kv(lx, y1, t(k), fmt_px(v), c); }
        lx = kv(lx, y1, t("chart.chg"), format!("{chg:+.2}%"), c);
        kv(lx, y1, t("chart.range"), format!("{range:.2}%"), c);
        let y2 = main.top() + 30.0;
        let mut lx = main.left() + 8.0;
        for (m, (n, col)) in mas.iter().zip(MA) {
            lx = kv(lx, y2, &format!("MA({n})"), m[hi_idx].map(fmt_px).unwrap_or("-".into()), col);
        }
        if lmax > 0.0 {
            kv(lx, y2, t("chart.liq"), format!("{} / {}", fmt_big(b.liq_long), fmt_big(b.liq_short)), DIM);
        }
        let y3 = main.top() + 48.0;
        let mut lx = main.left() + 8.0;
        for (k, v, col) in &legend3 {
            painter.circle_filled(pos2(lx + 3.0, y3), 3.0, *col);
            lx = kv(lx + 10.0, y3, k, v.clone(), *col);
        }
        if loading_tf {
            painter.text(pos2(main.left() + 8.0, main.top() + 76.0), Align2::LEFT_CENTER, t("chart.loading"), prop(11.0), WARN);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_pane(&self, painter: &egui::Painter, p: Pane, r: Rect, vis: &[Bar], i0: usize, fr: &Frame, hb: &Bar, prem: &[Prem]) {
        // the top 18px hold the legend, so lines never run through it
        let inner = Rect::from_min_max(pos2(r.left(), r.top() + 18.0), pos2(r.right(), r.bottom() - 4.0));
        let label = |s: String, col: Color32, x: f32| painter.text(pos2(x, r.top() + 9.0), Align2::LEFT_CENTER, s, mono(10.5), col).right() + 10.0;
        // pane name in the UI font, values in mono
        let mut lx = painter.text(pos2(r.left() + 8.0, r.top() + 9.0), Align2::LEFT_CENTER, t(p.key()), prop(11.5), MU).right() + 10.0;
        let axis = |lo: f64, hi: f64, fmt: &dyn Fn(f64) -> String| {
            painter.text(pos2(r.right() + 6.0, inner.top() + 4.0), Align2::LEFT_CENTER, fmt(hi), mono(10.0), DIM);
            painter.text(pos2(r.right() + 6.0, inner.bottom() - 4.0), Align2::LEFT_CENTER, fmt(lo), mono(10.0), DIM);
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
        let zero_line = |lo: f64, hi: f64| { if lo < 0.0 && hi > 0.0 { painter.hline(r.left()..=r.right(), y_of(0.0, lo, hi, inner), Stroke::new(1.0, LINE)); } };
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
                        painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, base - h), pos2(x + bw / 2.0, base)), 0, DIM);
                    }
                }
                let vols: Vec<f64> = vis.iter().map(|b| b.vol).collect();
                // volume averages follow the MA toggle: off by default, the bars speak for themselves
                for (n, col) in [(5usize, accent()), (10, dn())].into_iter().filter(|_| self.ma) {
                    let m = sma(&vols, n);
                    line(m.iter().enumerate().filter_map(|(k, v)| Some((k, (*v)?))).collect(), 0.0, max, col.linear_multiply(0.9), false);
                }
                lx = label(format!("{} {}", fmt_big(hb.vol), self.base_unit), FG, lx);
                lx = label(format!("≈{} USDT", fmt_big(hb.vol * hb.c)), MU, lx);
                if hb.buy + hb.sell > 0.0 { label(format!("B {} / S {}", fmt_big(hb.buy), fmt_big(hb.sell)), MU, lx); }
                axis(0.0, max, &fmt_big);
            }
            Pane::Cvd | Pane::Oi | Pane::Ls | Pane::Basis => {
                let get = |b: &Bar| -> Option<f64> { match p { Pane::Cvd => Some(b.cvd), Pane::Oi => b.oi, Pane::Ls => b.ls, _ => b.basis_bps } };
                let vals: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, get(b)?))).collect();
                if vals.is_empty() { return; }
                let (lo, hi) = range(&vals.iter().map(|x| x.1).collect::<Vec<_>>(), p == Pane::Basis);
                zero_line(lo, hi);
                let col = match p { Pane::Cvd => accent(), Pane::Oi => WARN, Pane::Ls => PURPLE, _ => FG };
                line(vals, lo, hi, col, false);
                let fmt: &dyn Fn(f64) -> String = match p { Pane::Ls => &|v| format!("{v:.3}"), Pane::Basis => &|v| format!("{v:+.2}bp"), _ => &fmt_big };
                if let Some(v) = get(hb) { label(fmt(v), col, lx); }
                axis(lo, hi, fmt);
            }
            Pane::Premium => {
                // each venue against the composite; the axis ignores the 2% most extreme points so one
                // stale feed cannot flatten the rest
                let mut all: Vec<f64> = prem.iter().flat_map(|l| l.vals.iter().flatten().copied()).collect();
                if all.is_empty() { return; }
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
            Pane::Funding => {
                let pred: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, b.funding_h? * 1e4))).collect();
                let sett: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, b.funding_settled? * 1e4))).collect();
                let all: Vec<f64> = pred.iter().chain(sett.iter()).map(|x| x.1).collect();
                if all.is_empty() { return; }
                let (lo, hi) = range(&all, true);
                zero_line(lo, hi);
                line(sett, lo, hi, WARN, true);
                line(pred, lo, hi, accent(), false);
                if let Some(v) = hb.funding_h { lx = label(format!("{} {:+.4}", t("chart.funding_pred"), v * 1e4), accent(), lx); }
                if let Some(v) = hb.funding_settled { label(format!("{} {:+.4} bp/h", t("chart.funding_settled"), v * 1e4), WARN, lx); }
                axis(lo, hi, &|v| format!("{v:+.3}"));
            }
        }
    }

    /// Place, draw and delete drawings. Click places points for the armed tool, right-click
    /// deletes the drawing under the pointer, Escape disarms.
    #[allow(clippy::too_many_arguments)]
    fn drawings_ui(&mut self, ui: &Ui, resp: &egui::Response, painter: &egui::Painter, bars: &[Bar], fr: &Frame, main: Rect, ps: PScale, base: &str) {
        let ms = self.tf * 60_000;
        let plot = ps.r;
        let y = |v: f64| ps.y(v);
        let price = |py: f32| ps.v(py);
        let to_screen = |(t, p): (i64, f64)| pos2(fr.x(idx_of(bars, t, ms)), y(p));
        let to_point = |p: Pos2| (time_of(bars, fr.idx(p.x), ms), price(p.y));
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) { self.tool = None; self.pending = None; }

        let list = self.drawings.entry(base.to_string()).or_default();
        let ptr = resp.hover_pos().filter(|p| plot.contains(*p));
        // screen segment of each drawing: rays run to the plot edge, horizontal lines span it
        let seg = |d: &Drawing| -> (Pos2, Pos2) {
            let (a, b) = (to_screen(d.a), to_screen(d.b));
            match d.tool {
                Tool::HLine => (pos2(main.left(), a.y), pos2(main.right(), a.y)),
                Tool::Trend => (a, b),
                Tool::Ray => { let dir = b - a; let k = if dir.length() > 0.0 { 4000.0 / dir.length() } else { 0.0 }; (a, a + dir * k) }
            }
        };
        let near = ptr.and_then(|p| list.iter().enumerate().map(|(i, d)| { let (a, b) = seg(d); (i, dist_to_segment(p, a, b)) })
            .filter(|(_, dd)| *dd < 6.0).min_by(|x, z| x.1.total_cmp(&z.1)).map(|(i, _)| i));

        if let (Some(tool), true, Some(p)) = (self.tool, resp.clicked(), resp.interact_pointer_pos().filter(|p| plot.contains(*p))) {
            let pt = to_point(p);
            match (tool, self.pending) {
                (Tool::HLine, _) => { list.push(Drawing { tool, a: pt, b: pt }); self.tool = None; }
                (_, None) => self.pending = Some(pt),
                (_, Some(a)) => { list.push(Drawing { tool, a, b: pt }); self.tool = None; self.pending = None; }
            }
        } else if resp.secondary_clicked() {
            if let Some(i) = near { list.remove(i); }
        }

        let clip = painter.with_clip_rect(plot);
        for (i, d) in list.iter().enumerate() {
            let (a, b) = seg(d);
            let w = if near == Some(i) { 2.0 } else { 1.2 };
            clip.line_segment([a, b], Stroke::new(w, accent()));
            if d.tool != Tool::HLine { for p in [to_screen(d.a), to_screen(d.b)] { clip.circle_filled(p, 2.5, accent()); } }
            if d.tool == Tool::HLine {
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, a.y - 9.0), vec2(AXIS_W - 2.0, 18.0));
                painter.rect_filled(lab, 2, accent());
                painter.text(lab.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, fmt_px(d.a.1), mono(11.0), Color32::BLACK);
            }
        }
        // preview of a drawing in progress
        if let (Some(tool), Some(p)) = (self.tool, ptr) {
            let s = Stroke::new(1.0, accent().linear_multiply(0.6));
            match (tool, self.pending) {
                (Tool::HLine, _) => { clip.hline(main.left()..=main.right(), p.y, s); }
                (_, Some(a)) => { clip.line_segment([to_screen(a), p], s); }
                _ => {}
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if near.is_some() && self.tool.is_none() { resp.clone().on_hover_text(t("draw.delete_tip")); }
    }

    /// Back to auto-fit at the live edge: a manual price range from one symbol is meaningless for
    /// another (BTC at 80k vs a coin at 0.3 would leave the chart empty).
    pub fn reset_view(&mut self) { self.y_range = None; self.right = 0.0; self.pending = None; self.liq = LiqCache::default(); self.cache = HeatCache::default(); }

    /// Chart controls for the native toolbar.
    pub fn native_json(&self, a: &Agg, market: Market, base: &str) -> serde_json::Value {
        let market = if market == Market::Margin { Market::Spot } else { market };
        let mut venues: Vec<String> = a.series.keys().filter(|(e, m)| *m == market && e.is_some()).filter_map(|(e, _)| e.map(|e| format!("{e:?}"))).collect();
        venues.sort();
        let tool = self.tool.map(|t| match t { Tool::HLine => "hline", Tool::Trend => "trend", Tool::Ray => "ray" });
        serde_json::json!({
            "tf": self.tf, "tfs": TFS.iter().map(|(m, l)| serde_json::json!([m, l])).collect::<Vec<_>>(), "quick": TF_QUICK,
            "src": self.src.map(|e| format!("{e:?}")), "venues": venues,
            "panes": self.panes.iter().filter(|(p, _)| !p.perp_only() || market == Market::Perp)
                .map(|(p, on)| serde_json::json!({"key": p.key(), "label": t(p.key()), "on": on})).collect::<Vec<_>>(),
            "ma": self.ma, "heat": self.heat, "liqmap": self.liqmap, "sr": self.sr, "walls": self.walls, "mark": self.mark, "breakouts": self.breakouts, "vwap": self.vwap, "cone": self.cone, "perp": market == Market::Perp,
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
        if v.get("tool").is_some() {
            self.tool = match v["tool"].as_str() { Some("hline") => Some(Tool::HLine), Some("trend") => Some(Tool::Trend), Some("ray") => Some(Tool::Ray), _ => None };
            self.pending = None;
        }
        if v["clear_drawings"].as_bool() == Some(true) { self.drawings.remove(base); }
        if v["reset_view"].as_bool() == Some(true) { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; }
    }

    fn toolbar(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (m, l) in &TFS[..TF_QUICK] {
                if tab(ui, l, self.tf == *m, 12.0).clicked() { self.tf = *m; }
            }
            // a menu interval in use shows as a selected tab after the quick ones
            if let Some((_, l)) = TFS[TF_QUICK..].iter().find(|(m, _)| *m == self.tf) { tab(ui, l, true, 12.0); }
            ui.menu_button(RichText::new("▾").color(MU), |ui| {
                ui.set_min_width(90.0);
                for (m, l) in &TFS[TF_QUICK..] { if ui.selectable_label(self.tf == *m, *l).clicked() { self.tf = *m; ui.close(); } }
            });
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(6.0);
            ui.separator();
            let mut venues: Vec<Exchange> = a.series.keys().filter(|(e, m)| *m == market && e.is_some()).filter_map(|(e, _)| *e).collect();
            venues.sort_by_key(|e| format!("{e:?}"));
            let cur = self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| t("src.agg").to_string());
            egui::ComboBox::from_id_salt("chart_src").icon(chevron).selected_text(cur).width(96.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.src, None, t("src.agg"));
                for e in venues { ui.selectable_value(&mut self.src, Some(e), format!("{e:?}")); }
            });
            // indicators: sub-panes and overlays in one menu instead of a row of toggles
            let n_on = self.panes.iter().filter(|(p, on)| *on && (!p.perp_only() || market == Market::Perp)).count()
                + self.heat as usize + self.ma as usize + (self.liqmap && market == Market::Perp) as usize;
            ui.menu_button(format!("{} {n_on} ▾", t("chart.indicators")), |ui| {
                ui.set_min_width(150.0);
                for (p, on) in self.panes.iter_mut() {
                    if p.perp_only() && market != Market::Perp { continue; }
                    ui.checkbox(on, t(p.key()));
                }
                ui.separator();
                ui.checkbox(&mut self.ma, "MA (7 / 25 / 99)");
                ui.checkbox(&mut self.heat, t("chart.heatmap"));
                if market == Market::Perp { ui.checkbox(&mut self.liqmap, t("chart.liqmap")).on_hover_text(t("chart.liqmap_tip")); }
            });
            ui.separator();
            for (tool, key) in [(Tool::HLine, "draw.hline"), (Tool::Trend, "draw.trend"), (Tool::Ray, "draw.ray")] {
                if tool_button(ui, Some(tool), self.tool == Some(tool)).on_hover_text(t(key)).clicked() {
                    self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
                    self.pending = None;
                }
            }
            let n = self.drawings.get(base).map_or(0, |v| v.len());
            if n > 0 && tool_button(ui, None, false).on_hover_text(format!("{} ({n})", t("draw.clear"))).clicked() { self.drawings.remove(base); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.right.abs() > 0.5 || self.bar_w != 7.0 {
                    if tab(ui, t("chart.reset"), false, 11.5).clicked() { self.right = 0.0; self.bar_w = 7.0; }
                }
            });
        });
    }
}

/// Small painted icon button for drawing tools; `None` is the clear (trash) button.
fn tool_button(ui: &mut Ui, tool: Option<Tool>, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 22.0), Sense::click());
    let p = ui.painter();
    if on { p.rect_filled(r, 3, HL); } else if resp.hovered() { p.rect_filled(r, 3, HL.linear_multiply(0.6)); }
    let col = if on { accent() } else if resp.hovered() { FG } else { MU };
    let s = Stroke::new(1.4, col);
    let c = r.center();
    match tool {
        Some(Tool::HLine) => {
            p.hline(egui::Rangef::new(c.x - 8.0, c.x + 8.0), c.y, s);
            p.circle_filled(c, 2.2, col);
        }
        Some(Tool::Trend) => {
            let (a, b) = (c + vec2(-7.0, 6.0), c + vec2(7.0, -6.0));
            p.line_segment([a, b], s);
            p.circle_filled(a, 2.0, col);
            p.circle_filled(b, 2.0, col);
        }
        Some(Tool::Ray) => {
            let (a, b) = (c + vec2(-7.0, 6.0), c + vec2(8.0, -7.0));
            p.line_segment([a, b], s);
            p.circle_filled(a, 2.0, col);
            p.line_segment([b, b + vec2(-5.0, 0.5)], s);
            p.line_segment([b, b + vec2(-0.5, 5.0)], s);
        }
        None => {
            // trash can
            p.hline(egui::Rangef::new(c.x - 6.0, c.x + 6.0), c.y - 5.0, s);
            p.hline(egui::Rangef::new(c.x - 2.0, c.x + 2.0), c.y - 7.0, s);
            p.line_segment([c + vec2(-4.5, -5.0), c + vec2(-3.5, 6.0)], s);
            p.line_segment([c + vec2(4.5, -5.0), c + vec2(3.5, 6.0)], s);
            p.hline(egui::Rangef::new(c.x - 3.5, c.x + 3.5), c.y + 6.0, s);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
        let (usual, sd) = terminal_one::quant::robust(&p.iter().map(|x| x.1).collect::<Vec<_>>())?;
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
        (lerp(30.0, 250.0, t), lerp(150.0, 220.0, t), lerp(200.0, 80.0, t))
    };
    // thin liquidity fades out completely, so only real walls light up (a floor here turned a
    // lone column into a solid block)
    let a = if s < 0.25 { 0.0 } else { 235.0 * ((s - 0.25) / 0.75).powf(1.3) };
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
    fn liq_levels_open_sweep_and_shrink() {
        let bar = |t: i64, c: f64, oi: f64| Bar { t, o: c, h: c, l: c, c, oi: Some(oi), ..Default::default() };
        let total = |m: &LiqMap| m.long.iter().chain(&m.short).sum::<f32>();
        // OI up 10 at 100: long levels below, short levels above, totals 10 each side
        let m = liq_levels(&[bar(0, 100.0, 0.0), bar(1, 100.0, 10.0)]).unwrap();
        let px = |i: usize| (m.lo + i as i64) as f64 * m.bin;
        let below: f32 = m.long.iter().enumerate().filter(|(i, _)| px(*i) < 100.0).map(|(_, q)| q).sum();
        let above: f32 = m.short.iter().enumerate().filter(|(i, _)| px(*i) > 100.0).map(|(_, q)| q).sum();
        assert!((below - 10.0).abs() < 1e-3 && (above - 10.0).abs() < 1e-3, "{below} {above}");
        // OI halves: everything shrinks by half; then a dip to 97 wipes the 50x/100x long levels
        // (98.5, 99.5) and keeps 10x/25x (90.5, 96.5)
        let bars = [bar(0, 100.0, 0.0), bar(1, 100.0, 10.0), bar(2, 100.0, 5.0), Bar { l: 97.0, ..bar(3, 100.0, 5.0) }];
        assert!((total(&liq_levels(&bars[..3]).unwrap()) - 10.0).abs() < 1e-3);
        let t = total(&liq_levels(&bars).unwrap());
        assert!((t - (10.0 - 5.0 * 0.4)).abs() < 1e-3, "{t}");
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
