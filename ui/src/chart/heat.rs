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

    /// The heatmap as one RGBA texture (columns = bars, rows = price bins, top = highest price),
    /// re-uploaded only when new samples arrived. Drawn as a single textured rect per frame.
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
        let mut qs: Vec<f32> = vals.iter().flat_map(|b| b.2.iter().copied().filter(|q| *q > 0.0)).collect();
        if qs.is_empty() { return None; }
        qs.sort_by(f32::total_cmp);
        let qmax = qs[(qs.len() * 97 / 100).min(qs.len() - 1)].max(1e-9);
        let mut img = egui::ColorImage::new([cols, rows], vec![Color32::TRANSPARENT; cols * rows]);
        for (bt, blo, v) in &vals {
            let x = ((bt - t0) / ms) as usize;
            for (k, q) in v.iter().enumerate() {
                if *q <= 0.0 { continue; }
                let (i, c) = (((rows - 1 - (blo + k as i64 - lo) as usize / step) * cols + x), heat_color((q / qmax).min(1.0).sqrt()));
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

impl Chart {
    /// Everything behind the candles: grid lines, the watermark and the cross-venue book heatmap.
    pub(super) fn backdrop(&mut self, ui: &Ui, cx: &Cx) {
        let Cx { a, smarket, live, bars, main, main_plot, fr, ps, painter, gv, .. } = *cx;
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
