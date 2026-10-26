//! Sub-panes under the price plot (volume, CVD, OI, funding, oscillators, premium, options),
//! the time axis, price labels and the crosshair.
use super::*;
use super::fmt::{fmt_big, fmt_time, fmt_time_full};
use super::ind::{macd, rsi, sma};

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) enum Pane { Vol, Cvd, Oi, Funding, Basis, Ls, Premium, Ofi, BookFlow, Rsi, Macd, Delta, Iv, Skew }

impl Pane {
    pub(super) fn key(self) -> &'static str {
        match self {
            Pane::Vol => "chart.vol", Pane::Cvd => "chart.cvd", Pane::Oi => "chart.oi",
            Pane::Funding => "chart.funding", Pane::Basis => "chart.basis", Pane::Ls => "chart.ls",
            Pane::Premium => "chart.premium", Pane::Ofi => "chart.ofi", Pane::BookFlow => "chart.bookflow",
            Pane::Rsi => "chart.rsi", Pane::Macd => "chart.macd", Pane::Delta => "chart.delta",
            Pane::Iv => "chart.iv", Pane::Skew => "chart.skew",
        }
    }
    pub(super) fn perp_only(self) -> bool { matches!(self, Pane::Oi | Pane::Funding | Pane::Basis | Pane::Ls) }
}

fn y_of(v: f64, lo: f64, hi: f64, r: Rect) -> f32 { r.bottom() - ((v - lo) / (hi - lo)) as f32 * r.height() }

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

impl Chart {
    /// Sub-panes, time axis, scale toggles, price labels and the crosshair; returns (pointer over
    /// the charts, hovered bar index).
    pub(super) fn axes(&mut self, ui: &Ui, cx: &Cx, panes: Vec<Pane>, taken: Vec<Rect>) -> (Option<Pos2>, usize) {
        let Cx { a, smarket, live, editing, bars, vis, i0, i1, rect, plot_w, main, main_plot, fr, ps, resp, painter, gv, gdp_step, gdp, base_px, pct, .. } = *cx;
        let y = |v: f64| ps.y(v);
        let axis_label = |v: f64| if pct { format!("{:+.2}%", (v / base_px - 1.0) * 100.0) } else { super::fmt_dp(v, gdp) };
        let round = |v: f64| { let k = v / (gdp_step * 5.0); (k - k.round()).abs() < 1e-6 };
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
        let pane_scales: Vec<(Pane, Rect, Option<(f64, f64)>)> = pane_rects.iter().map(|(p, r)| (*p, *r, self.draw_pane(&painter, *p, *r, &bars, i0, i1, &fr, hi_idx, &prem, &a.opt_min))).collect();

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
        (hover, hi_idx)
    }

    #[allow(clippy::too_many_arguments)]
    /// Returns the value range the pane was drawn with (for the crosshair), None when it drew nothing.
    fn draw_pane(&self, painter: &egui::Painter, p: Pane, r: Rect, bars: &[Bar], i0: usize, i1: usize, fr: &Frame, hi: usize, prem: &[Prem], opt: &std::collections::BTreeMap<i64, depth::agg::OptStat>) -> Option<(f64, f64)> {
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
            // option-implied, sampled live from the chains (saved across restarts): the last minute
            // sample inside each bar, in vol points
            Pane::Iv | Pane::Skew => {
                let ms = self.tf * 60_000;
                let get = |b: &Bar| opt.range(b.t..b.t + ms).next_back().and_then(|(_, s)| if p == Pane::Iv { s.iv30 } else { s.rr25 }).map(|v| v * 100.0);
                let vals: Vec<(usize, f64)> = vis.iter().enumerate().filter_map(|(k, b)| Some((k, get(b)?))).collect();
                if vals.is_empty() { label(t("chart.opt_loading").to_string(), dim(), lx); return None; }
                let (lo, hi) = range(&vals.iter().map(|x| x.1).collect::<Vec<_>>(), p == Pane::Skew);
                zero_line(lo, hi);
                let col = if p == Pane::Iv { WARN } else if get(hb).unwrap_or(0.0) >= 0.0 { up() } else { dn() };
                line(vals, lo, hi, col, false);
                if let Some(v) = get(hb) { label(if p == Pane::Iv { format!("{v:.2}%") } else { format!("{v:+.2}") }, col, lx); }
                axis(lo, hi, &|v| format!("{v:.1}"));
            }
        }
        scale.get()
    }
}
