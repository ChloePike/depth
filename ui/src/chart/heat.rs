//! Book heatmap behind the candles: per-bar liquidity, its texture and colour ramp, and the
//! backdrop phase (grid, watermark, heatmap).
use super::*;
pub(super) use charting::orderflow::heat::{BookHeat as HeatCache, TexGeom};

impl Chart {
    /// Everything behind the candles: grid lines, the watermark and the cross-venue book heatmap.
    pub(super) fn backdrop(&mut self, ui: &Ui, cx: &Cx) {
        let Cx { a, smarket, live, bars, main, main_plot, time_top, fr, ps, painter, grid, times, lo, hi, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // grid + price axis (TradingView: faint dotted grid; round levels a shade brighter)
        for v in grid.iter().copied() {
            let yy = y(v);
            grid_line(&painter, pos2(main.left(), yy), pos2(main.right(), yy));
        }
        // and one per time label, down through the panes
        for &i in times { let x = fr.x(i as f32); grid_line(painter, pos2(x, main.top()), pos2(x, time_top)); }
        // watermark: the Depth mark and wordmark, faint, bottom-left of the plot (as TradingView does)
        {
            let logo = super::theme::app_logo(ui.ctx());
            let r = Rect::from_min_size(pos2(main_plot.left() + 14.0, main_plot.bottom() - 40.0), vec2(26.0, 26.0));
            painter.image(logo.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::from_white_alpha(70));
            painter.text(pos2(r.right() + 8.0, r.center().y), Align2::LEFT_CENTER, "Depth", prop_s(19.0), fg().gamma_multiply(0.22));
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
                // the series: interval and market (a switch starts the bars over)
                let key = (self.tf as u64) << 8 | smarket as u64;
                self.cache.update(key, h.bin, 0, self.tf * 60_000, |since| h.cols.iter().filter(move |c| c.0 > since).cloned());
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
    use depth::agg::Heatmap;

    #[test]
    fn samples_fold_into_one_column_per_bar() {
        let mut h = Heatmap { bin: 1.0, ..Default::default() };
        for (ts, q) in [(1, 40.0), (15_000, 0.0), (30_000, 0.0), (45_000, 0.0), (60_000, 40.0), (61_000, 0.0)] {
            h.cols.push_back((ts, vec![(100, q), (101, 1.0)]));
        }
        let mut c = HeatCache::default();
        c.update(1, h.bin, 0, 60_000, |s| h.cols.iter().filter(move |c| c.0 > s).cloned());
        let (_, g) = c.texture(&egui::Context::default()).unwrap();
        assert_eq!(g.cols, 2);
    }
}
