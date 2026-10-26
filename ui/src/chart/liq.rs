//! Estimated liquidation levels: the model, its time x price heatmap and the right-edge profile.
use super::*;

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

/// Price of bin i is (lo + i) * bin; quantities in base units. `segs`: every level's open mass over
/// time, for the time x price heatmap (a level is drawn from when it formed until price swept it).
struct LiqMap { bin: f64, lo: i64, long: Vec<f32>, short: Vec<f32>, segs: Vec<LiqSeg> }

/// One level's mass between two changes: bin index, side, [t0, t1) in ms (t1 = MAX while open).
struct LiqSeg { i: usize, t0: i64, t1: i64, q: f32 }

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
    // the open segment per bin and side (long: 0..bins, short: bins..2 bins in `open`)
    let mut open: Vec<Option<usize>> = vec![None; 2 * bins];
    let mut segs: Vec<LiqSeg> = vec![];
    let at = |p: f64| ((p / bin).round() as i64 - lo).clamp(0, bins as i64 - 1) as usize;
    for b in bars {
        // price traded through: longs at or above the low and shorts at or below the high are gone
        let (lk, sk) = (at(b.l), at(b.h));
        for (k, q) in long.iter_mut().enumerate().skip(lk) { if *q > 0.0 { *q = 0.0; if let Some(s) = open[k].take() { segs[s].t1 = b.t; } } }
        for (k, q) in short.iter_mut().enumerate().take(sk + 1) { if *q > 0.0 { *q = 0.0; if let Some(s) = open[bins + k].take() { segs[s].t1 = b.t; } } }
        if b.vol > 0.0 {
            let p = (b.h + b.l + b.c) / 3.0;
            // formed by the end of this minute (a sweep acts from its start)
            let t = b.t + 59_999;
            for (lev, w) in LEVERAGE {
                let q = (b.vol * w / 2.0) as f32;
                for (k, side, o) in [(at(p * (1.0 - 1.0 / lev + MMR)), &mut long, 0), (at(p * (1.0 + 1.0 / lev - MMR)), &mut short, bins)] {
                    side[k] += q;
                    if let Some(s) = open[o + k] { segs[s].t1 = t; }
                    open[o + k] = Some(segs.len());
                    segs.push(LiqSeg { i: o + k, t0: t, t1: i64::MAX, q: side[k] });
                }
            }
        }
    }
    Some(LiqMap { bin, lo, long, short, segs })
}

impl LiqMap {
    /// Open mass (longs + shorts) per cell: `cols` columns of `ms` from `t0` (each column shows the
    /// state at its end), `rows` rows from the bottom; `row` maps a price to its (fractional, unclamped)
    /// row, any scale. A level fills the rows of its whole bin, so neighbours join into bands.
    fn grid(&self, t0: i64, ms: i64, cols: usize, rows: usize, row: impl Fn(f64) -> f32) -> Vec<f32> {
        let mut g = vec![0f32; cols * rows];
        let n = self.long.len();
        if cols == 0 || rows == 0 { return g; }
        let span = cols as i64 * ms;
        for s in &self.segs {
            let px = (self.lo + (s.i % n) as i64) as f64 * self.bin;
            let (a, b) = (row(px - self.bin / 2.0), row(px + self.bin / 2.0));
            let (a, b) = (a.min(b), a.max(b));
            if b < 0.0 || a >= rows as f32 { continue; }
            let (r0, r1) = (a.max(0.0) as usize, (b.ceil() as usize).clamp(1, rows) - 1);
            // the bin's mass spread over its rows, so a zoomed-in bin is not brighter than a small one
            let q = s.q / (r1 - r0 + 1) as f32;
            // column c (end te = t0 + (c+1) ms - 1) shows the segment when t0_seg <= te < t1_seg
            let c0 = ((s.t0 - t0 + 1).max(0) + ms - 1) / ms - 1;
            let c1 = (s.t1.min(t0 + span) - t0).div_euclid(ms) - 1;
            for c in c0.max(0)..=c1.min(cols as i64 - 1) { for r in r0.max(0)..=r1.max(r0) { g[c as usize * rows + r] += q; } }
        }
        g
    }
}

#[derive(Default)]
pub(super) struct LiqCache {
    /// (tf, bar count, last bar time, last volume bits): recomputed only when one of them changes,
    /// at most once a second (every trade changes the live bar's volume)
    key: (i64, usize, i64, u64),
    at: Option<std::time::Instant>,
    map: Option<LiqMap>,
    ver: u64,
    /// heatmap texture and what it was drawn for (map version, view)
    tex: Option<((u64, i64, i64, usize, u64, u64, usize), egui::TextureHandle)>,
}

impl LiqCache {
    fn get(&mut self, bars: &[Bar], tf: i64) -> Option<&LiqMap> {
        let last = bars.last()?;
        let key = (tf, bars.len(), last.t, last.vol.to_bits());
        let stale = self.at.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) || key.1 != self.key.1 || key.0 != self.key.0;
        if key != self.key && (stale || self.map.is_none()) {
            self.key = key; self.map = liq_levels(bars); self.at = Some(std::time::Instant::now()); self.ver += 1;
        }
        self.map.as_ref()
    }

    /// The time x price heatmap texture for a view (rebuilt when the map or the view changes).
    /// Cells scale to the 99th percentile so one stacked level does not wash out the rest.
    #[allow(clippy::too_many_arguments)]
    /// `view` identifies the price mapping (range and scale) for the cache.
    fn texture(&mut self, ctx: &egui::Context, t0: i64, ms: i64, cols: usize, rows: usize, view: (u64, u64, u8), row: impl Fn(f64) -> f32) -> Option<egui::TextureId> {
        let k = (self.ver, t0, ms, cols, view.0, view.1 ^ ((view.2 as u64) << 60), rows);
        if let Some((kk, t)) = &self.tex { if *kk == k { return Some(t.id()); } }
        let m = self.map.as_ref()?;
        let g = m.grid(t0, ms, cols, rows, row);
        let mut nz: Vec<f32> = g.iter().copied().filter(|v| *v > 0.0).collect();
        if nz.is_empty() { self.tex = None; return None; }
        let p = (nz.len() * 995 / 1000).min(nz.len() - 1);
        let top = *nz.select_nth_unstable_by(p, f32::total_cmp).1;
        let (cold, hot) = (Color32::from_rgb(0x8a, 0x4b, 0x0f), Color32::from_rgb(0xff, 0xd6, 0x0a));
        let mut img = egui::ColorImage::filled([cols, rows], Color32::TRANSPARENT);
        for c in 0..cols {
            for r in 0..rows {
                let v = g[c * rows + r];
                if v <= 0.0 { continue; }
                // squared, like Coinglass: most of the map stays dark, only real clusters light up
                let t = (v / top).min(1.0).powi(2);
                if t < 0.02 { continue; }
                img.pixels[(rows - 1 - r) * cols + c] = lerp(cold, hot, t.sqrt()).gamma_multiply(0.04 + 0.86 * t);
            }
        }
        let tex = match self.tex.take() { Some((_, mut t)) => { t.set(img, egui::TextureOptions::NEAREST); t } None => ctx.load_texture("liqheat", img, egui::TextureOptions::NEAREST) };
        let id = tex.id();
        self.tex = Some((k, tex));
        Some(id)
    }
}

impl Chart {
    /// Liquidation map (perp, live): heatmap over the plot and the profile at its right edge;
    /// returns the legend colour when it drew.
    pub(super) fn liq_layer(&mut self, ui: &Ui, cx: &Cx) -> Option<Color32> {
        let Cx { smarket, live, raw, bars, i0, i1, main, main_plot, fr, ps, lo, hi, resp, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // estimated liquidation map: horizontal profile on the right edge of the price plot,
        // longs (below price) red, shorts (above) green, merged into ~2px rows
        let mut liq_legend: Option<Color32> = None;
        if self.liqmap && smarket == Market::Perp && live {
            // estimated from the 1m series whatever the interval: finer OI steps, same levels
            // time x price: each estimated level from when it formed until price swept it (2 px rows)
                if self.liq.get(&raw, 1).is_some() {
                let rows = ((main_plot.height() / 2.0) as usize).clamp(1, 1000);
                let ms = self.tf * 60_000;
                let (bot, h) = (main_plot.bottom(), main_plot.height());
                let row = |px: f64| (bot - y(px)) / h * rows as f32;
                if let Some(id) = self.liq.texture(ui.ctx(), bars[i0].t, ms, i1 - i0 + 1, rows, (lo.to_bits(), hi.to_bits(), self.scale as u8), row) {
                    let r = Rect::from_min_max(pos2(fr.x(i0 as f32) - self.bar_w / 2.0, main_plot.top()), pos2(fr.x(i1 as f32) + self.bar_w / 2.0, main_plot.bottom()));
                    painter.with_clip_rect(main_plot).image(id, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
            }
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
        liq_legend
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liq_levels_open_and_sweep() {
        let bar = |t: i64, c: f64, vol: f64| Bar { t, o: c, h: c, l: c, c, vol, ..Default::default() };
        // time x price grid: column 0 (first bar's minute) holds all 20; the second bar trades down
        // to 60 and sweeps every long level above it, so column 1 keeps only the shorts (10)
        {
            let m = liq_levels(&[bar(0, 100.0, 20.0), Bar { t: 60_000, o: 100.0, h: 100.0, l: 60.0, c: 100.0, vol: 0.0, ..Default::default() }]).unwrap();
            let g = m.grid(0, 60_000, 2, 1, |_| 0.5);
            assert!((g[0] - 20.0).abs() < 1e-3 && (g[1] - 10.0).abs() < 1e-3, "{g:?}");
        }
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
}
