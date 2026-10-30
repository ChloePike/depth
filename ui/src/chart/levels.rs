//! Volume profile, high-volume nodes, breakouts and session VWAP, and the context levels drawn
//! from them (with the expected-move cone and liquidity walls).
use super::*;

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
    let last_c = bars[n - 1].c;
    // (index of the latest cross, level, broke up, failed); most recent break first
    let mut v: Vec<(usize, SrLevel, bool, bool)> = levels.iter().filter_map(|l| {
        let ev: Vec<(usize, bool)> = (from + 1..n).filter_map(|k| {
            let (a, b) = (bars[k - 1].c, bars[k].c);
            if a < l.px - l.half && b > l.px + l.half { Some((k, true)) } else if a > l.px + l.half && b < l.px - l.half { Some((k, false)) } else { None }
        }).collect();
        let &(k, up) = ev.last()?;
        // crossed back through: the earlier break failed; one break whose last close is already
        // back on the side it came from (inside the level's band) has failed too
        Some(if ev.len() > 1 { (k, *l, !up, true) } else { (k, *l, up, if up { last_c < l.px } else { last_c > l.px }) })
    }).collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|(_, l, up, failed)| (l, up, failed)).collect()
}

/// A strike as the market names it: "80,000", "3.5", not fmt_px's padded "88.0000".
fn strike_txt(v: f64) -> String {
    let s = fmt_px(v);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

impl Chart {
    /// Context levels on the price plot: volume profile (POC, value area, S / R), breakouts, the
    /// expected-move cone, session VWAP and liquidity walls.
    pub(super) fn levels(&mut self, cx: &Cx, mut taken: &mut Vec<Rect>, legend3: &mut Vec<(String, String, Color32)>) {
        let Cx { a, smarket, live, raw, bars, vis, i0, main, main_plot, fr, ps, lo, hi, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
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
            // name@price small at the plot's left edge (the account line labels start right of these)
            for (px, tag, col) in tagged {
                if !(px > lo && px < hi) { continue; }
                let g = painter.layout_no_wrap(format!("{tag}@{}", fmt_px(px)), prop(9.5), col);
                let r = Rect::from_min_size(pos2(main.left() + 4.0, y(px) - 13.0), vec2(g.size().x + 6.0, 12.0));
                painter.rect_filled(r, 2, label_bg().gamma_multiply(0.8));
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
                // TradingView-style level: a thin line (dashed when the break failed), a small solid
                // name@price label at the right edge of the plot; nothing over the candles
                let ly = y(l.px);
                let clip = painter.with_clip_rect(main_plot);
                let st = Stroke::new(1.0, col.gamma_multiply(0.75));
                if failed { clip.extend(egui::Shape::dashed_line(&[pos2(main.left(), ly), pos2(main.right(), ly)], st, 5.0, 4.0)); }
                else { clip.hline(main.left()..=main.right(), ly, st); }
                let key = match (is_up, failed) { (true, false) => "chart.broke_up", (false, false) => "chart.broke_down", (true, true) => "chart.failed_up", (false, true) => "chart.failed_down" };
                let g = painter.layout_no_wrap(format!("{}@{}", t(key), fmt_px(l.px)), prop_s(10.5), Color32::WHITE);
                let r = place(Rect::from_min_size(pos2(main.right() - g.size().x - 14.0, ly - 9.0), g.size() + vec2(10.0, 4.0)), &mut taken);
                painter.rect_filled(r, 2, col.gamma_multiply(0.9));
                painter.galley(r.min + vec2(5.0, 2.0), g, Color32::WHITE);
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
        // Polymarket odds (reference only): the nearest settle ladder and touch window, a short dotted
        // line and a small label per strike on screen; labels that would overlap others are dropped
        if self.poly && !self.odds.is_empty() {
            let (settle, touch) = depth::poly::pick(&self.odds, depth::now_ms());
            let violet = Color32::from_rgb(0x9b, 0x87, 0xf5);
            let x0 = main.right() - main.width() * 0.22;
            for o in settle.iter().chain(&touch).filter(|o| o.strike > lo && o.strike < hi) {
                let ly = y(o.strike);
                painter.extend(egui::Shape::dashed_line(&[pos2(x0, ly), pos2(main.right(), ly)], Stroke::new(1.0, violet.gamma_multiply(0.5)), 1.5, 3.0));
                let sym = match o.kind { depth::poly::Kind::Above => ">", depth::poly::Kind::Below => "<", depth::poly::Kind::Reach => "↑", depth::poly::Kind::Dip => "↓" };
                let g = painter.layout_no_wrap(format!("PM {sym}{} {:.0}% {}", strike_txt(o.strike), o.p * 100.0, local_date(o.end_ms)), mono_m(10.0), violet);
                if let Some(r) = try_place(Rect::from_min_size(pos2(x0 - g.size().x - 10.0, ly - 8.0), g.size() + vec2(8.0, 3.0)), &mut taken) {
                    painter.rect(r, 2, panel().gamma_multiply(0.9), Stroke::new(1.0, violet.gamma_multiply(0.5)), egui::StrokeKind::Inside);
                    painter.galley(r.min + vec2(4.0, 1.5), g, violet);
                }
            }
            let mut v = vec![];
            if let (Some(m), Some(o)) = (depth::poly::median(&settle), settle.first()) { v.push(format!("{} {} {}", local_date(o.end_ms), super::l("median"), fmt_px(m))); }
            if let Some(o) = touch.first() { v.push(format!("{} {}", super::l("touch by"), local_date(o.end_ms))); }
            if !v.is_empty() { legend3.push(("Polymarket".into(), v.join("  ·  "), violet)); }
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
                    if let Some(r) = (m > lo && m < hi).then(|| try_place(Rect::from_min_size(pos2(main.right() + 2.0, y(m) - 9.0), vec2(AXIS_W - 4.0, 18.0)), &mut taken)).flatten() {
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
                        painter.extend(egui::Shape::dashed_line(&[pos2(x0, ly), pos2(main.right(), ly)], Stroke::new(1.0, col.gamma_multiply(0.55)), 2.0, 3.0));
                        // a small outlined label at the line's right end (walls are context, not signals)
                        let tag = format!("{} ${}", if ask { "Ask wall" } else { "Bid wall" }, fmt_usd_short(qty * px));
                        let g = painter.layout_no_wrap(tag, mono_m(10.0), col);
                        let r = place(Rect::from_min_size(pos2(main.right() - g.size().x - 14.0, ly - 8.0), g.size() + vec2(8.0, 3.0)), &mut taken);
                        painter.rect(r, 2, panel().gamma_multiply(0.9), Stroke::new(1.0, col.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                        painter.galley(r.min + vec2(4.0, 1.5), g, col);
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
        // broke down, then a close back above the level (still inside its band): failed, not "broke down"
        let d = broken_levels(&[c(0, 103.0), c(1, 101.0), c(2, 102.2)], &lv, 10);
        assert!(d.len() == 1 && !d[0].1 && d[0].2);
        // the most recent break comes first, whatever the level order
        let two = [SrLevel { px: 100.0, half: 0.4, score: 1.0 }, SrLevel { px: 102.0, half: 0.4, score: 1.0 }];
        let r = broken_levels(&[c(0, 99.0), c(1, 101.0), c(2, 103.0)], &two, 10);
        assert!(r.len() == 2 && r[0].0.px == 102.0);
    }
}
