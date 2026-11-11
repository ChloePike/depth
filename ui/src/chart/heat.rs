//! Book heatmap behind the candles: per-bar liquidity, its texture and colour ramp, and the
//! backdrop phase (grid, watermark, heatmap).
use super::*;
use depth::agg::Heatmap;

/// Heatmap aggregated per chart bar: (bar start, first bin index, max base qty per bin).
#[derive(Default)]
pub(super) struct HeatCache {
    tf: i64, market: Option<Market>, last_ts: i64, bin: f64,
    /// per bar: (open time, lowest bin, summed qty per bin, samples); drawn as the time-weighted
    /// mean, so liquidity that was pulled fades instead of staying lit for the whole bar
    bars: Vec<(i64, i64, Vec<f32>, u32)>,
    /// the latest column: the live bar shows the book as it is now, not what it was earlier
    cur: Vec<(i64, f32)>,
    /// bumped whenever `bars` changes; the GPU texture is rebuilt only then (about once a second)
    ver: u64,
    tex: Option<(u64, egui::TextureHandle, TexGeom)>,
    /// the book right now for the depth profile at the right edge: (when read, bin, [(price, qty, ask)])
    depth: Option<(std::time::Instant, f64, Vec<(f64, f32, bool)>)>,
}

/// Where the heatmap texture sits: first bar time, bar width (ms), lowest bin, bin count.
#[derive(Clone, Copy)]
struct TexGeom { t0: i64, ms: i64, lo: i64, bins: usize, cols: usize }

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

    /// The heatmap as one RGBA texture (columns = bars, rows = price bins, top = highest price), re-uploaded only when
    /// new samples arrived. Drawn as a single textured rect per frame. (Thin 2 px lines per bin, as CoinGlass draws its
    /// finer bins, made a comb of BTC's $15-25 bins: nearly every bin holds orders, 2026-11-11.)
    fn texture(&mut self, ctx: &egui::Context) -> Option<(egui::TextureId, TexGeom)> {
        if let Some((v, t, g)) = &self.tex { if *v == self.ver { return Some((t.id(), *g)); } }
        // a texture side over the GPU limit (8192 here) is a wgpu validation panic: keep the newest
        // `side` columns and merge price bins `step` to a row
        let side = ctx.input(|i| i.max_texture_side).max(1);
        let ms = self.tf * 60_000;
        let last = self.bars.last()?;
        let t0 = self.bars.iter().map(|b| b.0).find(|t| (last.0 - t) / ms < side as i64)?;
        let bars = || self.bars.iter().filter(move |b| b.0 >= t0);
        let lo = bars().map(|b| b.1).min()?;
        let hi = bars().map(|b| b.1 + b.2.len() as i64).max()?;
        let (cols, bins) = (((last.0 - t0) / ms + 1) as usize, (hi - lo).max(1) as usize);
        let step = bins.div_ceil(side);
        let rows = bins.div_ceil(step);
        if cols * rows > 8_000_000 { return None; }
        let last_i = self.bars.len() - 1;
        let vals: Vec<(i64, i64, Vec<f32>)> = self.bars.iter().enumerate().filter(|(_, b)| b.0 >= t0).map(|(i, (t, blo, v, n))| {
            if i == last_i {
                let mut w = vec![0.0; v.len()];
                for &(b, q) in &self.cur { if let Some(x) = usize::try_from(b - blo).ok().and_then(|k| w.get_mut(k)) { *x = q; } }
                (*t, *blo, w)
            } else { (*t, *blo, v.iter().map(|q| q / (*n).max(1) as f32).collect()) }
        }).collect();
        if vals.iter().all(|b| b.2.iter().all(|q| *q <= 0.0)) { return None; }
        let mut img = egui::ColorImage::new([cols, rows], vec![Color32::TRANSPARENT; cols * rows]);
        for (bt, blo, v) in &vals {
            let x = ((bt - t0) / ms) as usize;
            // each bin against its price neighbours (`local_median`): what stands out of the book around it lights up
            let meds = local_median(v, NEIGHBOURS);
            for (k, q) in v.iter().enumerate() {
                if *q <= 0.0 { continue; }
                let med = meds[k];
                let (i, c) = (((rows - 1 - (blo + k as i64 - lo) as usize / step) * cols + x), heat_color(heat_level(*q, med)));
                // merged bins keep the strongest (alpha rises with the level)
                if c.a() >= img.pixels[i].a() { img.pixels[i] = c; }
            }
        }
        let geom = TexGeom { t0, ms, lo, bins: rows * step, cols };
        let t = match self.tex.take() {
            Some((_, mut t, _)) => { t.set(img, egui::TextureOptions::NEAREST); t }
            None => ctx.load_texture("heatmap", img, egui::TextureOptions::NEAREST),
        };
        let id = t.id();
        self.tex = Some((self.ver, t, geom));
        Some((id, geom))
    }
}

/// Bins on each side a bin is compared with (`local_median`).
const NEIGHBOURS: usize = 10;

/// Per bin, the median of the resting bins within `w` bins of it (itself included). Against the whole column's median
/// instead, the dense book next to the price stood out of the thin 2% edges: a solid yellow band at the price, 2026-11-11.
fn local_median(v: &[f32], w: usize) -> Vec<f32> {
    let mut buf = Vec::with_capacity(2 * w + 1);
    (0..v.len()).map(|k| {
        buf.clear();
        buf.extend(v[k.saturating_sub(w)..(k + w + 1).min(v.len())].iter().copied().filter(|q| *q > 0.0));
        if buf.is_empty() { return 0.0; }
        let m = buf.len() / 2;
        *buf.select_nth_unstable_by(m, f32::total_cmp).1
    }).collect()
}

/// How far a bin stands out of the book around it (`med`: the median of its price neighbours) as 0..1 on a log scale: half the median 0, the
/// median 0.2 (a faint purple wherever orders rest, as CoinGlass shows the book), 3x about 0.5 (still purple: neighbouring
/// bins vary 1-3x minute to minute, and green there read as a checkerboard), WALL_X times it 1. A cut-off at 2x hid the
/// smoother minute columns entirely, 2026-11-11. Against the map's top level instead, BTC's $20 bins summed over 12 venues were all near the top: one yellow
/// block, walls invisible (2026-11-11). Fades in, no threshold (a threshold flickered bar to bar).
const WALL_X: f32 = 16.0;
fn heat_level(q: f32, med: f32) -> f32 { ((2.0 * q / med.max(1e-9)).ln() / (2.0 * WALL_X).ln()).clamp(0.0, 1.0) }

/// CoinGlass-like ramp for liquidity, no blue: near-black purple (almost transparent) -> purple -> green -> yellow.
fn heat_color(s: f32) -> Color32 {
    const S: [(f32, [u8; 4]); 5] = [(0.0, [30, 10, 50, 0]), (0.35, [80, 30, 120, 70]), (0.65, [40, 175, 105, 190]),
        (0.85, [175, 220, 60, 235]), (1.0, [255, 236, 90, 255])];
    let s = s.clamp(0.0, 1.0);
    let k = S.iter().position(|x| x.0 >= s).unwrap_or(S.len() - 1).max(1);
    let ((a, ca), (b, cb)) = (S[k - 1], S[k]);
    let t = (s - a) / (b - a);
    let m = |i: usize| (ca[i] as f32 + (cb[i] as f32 - ca[i] as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(m(0), m(1), m(2), m(3))
}

impl Chart {
    /// Everything behind the candles: grid lines, the watermark and the cross-venue book heatmap.
    pub(super) fn backdrop(&mut self, ui: &Ui, cx: &Cx) {
        let Cx { a, smarket, live, bars, main, main_plot, fr, ps, painter, gv, lo, hi, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // grid + price axis (TradingView: faint dotted grid; round levels a shade brighter)
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
            // up under the legend rows too, like the liquidation map
            painter.with_clip_rect(Rect::from_min_max(pos2(main_plot.left(), main.top()), main_plot.right_bottom())).image(tex, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        };
        // heatmap (cross-venue book, USDT) behind candles
        // the footprint replaces the book heatmap: two volume colorings on top of each other read as noise
        if self.heat && self.src.is_none() && live && !(self.fp && a.fp.contains_key(&(self.src, smarket))) {
            if let Some(h) = a.heat.get(&smarket) {
                self.cache.update(h, self.tf, smarket);
                // recorded live from the book: on long intervals it starts as the current bar only
                let bin = self.cache.bin;
                if let Some((tex, g)) = self.cache.texture(ui.ctx()) { quad(tex, g, bin); }
                // the book now as a profile at the right edge, asks above (down colour) and bids below (up colour), as
                // CoinGlass draws it; the liquidation map's own profile takes that place while it is on
                if !(self.liqmap && smarket == Market::Perp) && bin > 0.0 {
                    let fresh = self.cache.depth.as_ref().is_some_and(|(at, b, _)| *b == bin && at.elapsed() < std::time::Duration::from_millis(250));
                    if !fresh {
                        let (bids, asks) = a.book_where(smarket, bin, ((hi - lo) / ((lo + hi) / 2.0)).clamp(0.001, 0.05), true, |_| true);
                        let lv = bids.iter().map(|l| (l.px, l.qty as f32, false)).chain(asks.iter().map(|l| (l.px, l.qty as f32, true))).collect();
                        self.cache.depth = Some((std::time::Instant::now(), bin, lv));
                    }
                    if let Some((_, _, lv)) = &self.cache.depth {
                        // merged into rows of at least 2 px
                        let row = ((hi - lo) / main_plot.height() as f64 * 2.0).max(bin);
                        let mut rows: std::collections::BTreeMap<(i64, bool), f32> = Default::default();
                        for &(px, q, ask) in lv { if px >= lo && px <= hi { *rows.entry(((px / row).floor() as i64, ask)).or_default() += q; } }
                        let qmax = rows.values().copied().fold(0.0, f32::max);
                        if qmax > 0.0 {
                            let (w, clip) = (80.0, painter.with_clip_rect(main_plot));
                            for ((k, ask), q) in rows {
                                let (y0, y1) = (y((k + 1) as f64 * row), y(k as f64 * row));
                                let len = w * (q / qmax).sqrt();
                                let col = if ask { dn() } else { up() };
                                clip.rect_filled(Rect::from_min_max(pos2(main.right() - len, y0), pos2(main.right(), y1.max(y0 + 1.0))), 0, col.gamma_multiply(0.75));
                            }
                        }
                    }
                }
            }
        }
    }
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
    fn heatmap_texture_fits_the_gpu_side() {
        // price walked 15,000 bins across bars and the bars span a long gap in time: both sides
        // past the limit crashed wgpu (Dimension Y value 8884 exceeds the limit of 8192)
        let mut h = Heatmap { bin: 1.0, ..Default::default() };
        for i in 0..1500i64 { h.cols.push_back((i * 60_000 + i.min(1) * 600_000_000, vec![(i * 10, 5.0), (i * 10 + 1, 1.0)])); }
        let mut c = HeatCache::default();
        c.update(&h, 1, Market::Perp);
        let ctx = egui::Context::default();
        let side = ctx.input(|i| i.max_texture_side);
        let (_, g) = c.texture(&ctx).unwrap();
        let [w, hh] = c.tex.as_ref().unwrap().1.size();
        assert!(w <= side && hh <= side, "{w} x {hh} over {side}");
        // the quad still covers every bin of the kept bars
        assert!(g.lo + g.bins as i64 >= 1499 * 10 + 1 && g.cols == w);
    }
}

#[cfg(test)]
mod ramp_tests {
    use super::*;

    #[test]
    fn a_dense_book_at_the_price_does_not_light_up_only_a_wall_does() {
        // thin edges, a dense block in the middle (the book at the price), one wall 10x inside it
        let mut v = vec![1.0f32; 60];
        for x in v.iter_mut().take(40).skip(20) { *x = 10.0; }
        v[30] = 100.0;
        let m = local_median(&v, NEIGHBOURS);
        assert!(heat_level(v[25], m[25]) < 0.3, "the dense book stays dark: {}", heat_level(v[25], m[25]));
        assert!(heat_level(v[30], m[30]) > 0.75, "the wall lights up: {}", heat_level(v[30], m[30]));
    }

    #[test]
    fn heat_scale_fades_small_and_tops_out() {
        // the column's typical level faint, its 1-3x noise still purple (under the green from 0.55), a wall 16x the brightest
        assert_eq!(heat_level(0.0, 10.0), 0.0);
        let med = heat_level(10.0, 10.0);
        assert!(med > 0.15 && med < 0.25 && heat_color(med).a() > 0, "{med}");
        assert!(heat_level(30.0, 10.0) < 0.55, "{}", heat_level(30.0, 10.0));
        assert!((heat_level(160.0, 10.0) - 1.0).abs() < 1e-5 && heat_level(500.0, 10.0) == 1.0);
        assert_eq!(heat_color(0.0).a(), 0);
        assert!(heat_color(0.5).a() > heat_color(0.2).a() && heat_color(1.0).a() == 255);
        // no blue on top: the ramp ends yellow
        let c = heat_color(1.0);
        assert!(c.r() > c.b() && c.g() > c.b());
    }

}
