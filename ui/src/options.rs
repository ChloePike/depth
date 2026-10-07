//! Option view: venue + expiry selector, IV smile, ATM term structure across venues, T-chain.
use super::{fmt_px, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Rect, RichText, Sense, Stroke, Ui};
use terminal_one::agg::{Agg, Opt};
use terminal_one::{now_ms, Exchange};

#[derive(Default)]
pub struct OptView { venue: Option<Exchange>, expiry: Option<i64> }

fn days(exp: i64) -> f64 { (exp - now_ms()) as f64 / 86_400_000.0 }

impl OptView {
    pub fn show(&mut self, ui: &mut Ui, a: &Agg) {
        let mut venues: Vec<Exchange> = a.chains.keys().copied().collect();
        venues.sort_by_key(|e| format!("{e:?}"));
        if venues.is_empty() { ui.centered_and_justified(|ui| ui.label(t("chart.loading"))); return; }
        let venue = *self.venue.get_or_insert(if venues.contains(&Exchange::Bybit) { Exchange::Bybit } else { venues[0] });
        let chain = &a.chains[&venue];
        let exps: Vec<_> = chain.expiries().into_iter().filter(|e| e.expiry_ms > now_ms()).collect();

        let exp = self.expiry.filter(|x| exps.iter().any(|e| e.expiry_ms == *x)).or_else(|| exps.iter().find(|e| days(e.expiry_ms) > 1.0).or(exps.first()).map(|e| e.expiry_ms));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for e in &venues {
                // icon + name as one tab
                let r = tab(ui, &format!("      {e:?}"), venue == *e, 12.5);
                icon(ui.painter(), *e, Rect::from_center_size(pos2(r.rect.left() + 13.0, r.rect.center().y - 1.0), vec2(13.0, 13.0)), venue != *e);
                if r.clicked() { self.venue = Some(*e); self.expiry = None; }
            }
            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new(t("opt.expiry")).font(prop(11.0)).color(DIM));
            for e in exps.iter().take(16) {
                let d = days(e.expiry_ms);
                let label = if d < 1.0 { format!("{:.0}h", d * 24.0) } else { format!("{d:.0}{}", t("opt.days")) };
                let r = tab(ui, &label, exp == Some(e.expiry_ms), 12.0)
                    .on_hover_text(format!("{} {}   {} {}", t("opt.forward"), e.forward.map(fmt_px).unwrap_or("-".into()), t("opt.atm_iv"), e.atm_iv.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or("-".into())));
                if r.clicked() { self.expiry = Some(e.expiry_ms); }
            }
        });
        ui.add_space(4.0);
        let Some(exp) = exp else { return };
        let info = exps.iter().find(|e| e.expiry_ms == exp).unwrap();
        let fwd = info.forward.unwrap_or(info.atm_strike);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(t("opt.forward")).font(prop(11.0)).color(DIM));
            ui.label(RichText::new(fmt_px(fwd)).font(mono(12.5)).color(FG))
                .on_hover_text(if info.forward_src == "venue" { t("opt.src_venue") } else { t("opt.src_parity") });
            ui.add_space(14.0);
            ui.label(RichText::new(t("opt.atm_iv")).font(prop(11.0)).color(DIM));
            ui.label(RichText::new(info.atm_iv.map(|v| format!("{:.2}%", v * 100.0)).unwrap_or("-".into())).font(mono(12.5)).color(WARN));
        });
        ui.add_space(4.0);

        let mut opts: Vec<&Opt> = chain.opts.values().filter(|o| o.has_info && o.expiry_ms == exp && (o.strike / fwd - 1.0).abs() < 0.25).collect();
        opts.sort_by(|x, y| x.strike.total_cmp(&y.strike));

        // smile + term structure
        let top = ui.available_rect_before_wrap();
        let h = (top.height() * 0.38).clamp(140.0, 260.0);
        let smile = Rect::from_min_size(top.min, vec2(top.width() * 0.6 - 6.0, h));
        let term = Rect::from_min_max(pos2(smile.right() + 12.0, top.top()), pos2(top.right(), top.top() + h));
        ui.allocate_rect(Rect::from_min_size(top.min, vec2(top.width(), h)), Sense::hover());
        self.smile(ui, smile, &opts, fwd);
        self.term(ui, term, a);
        ui.add_space(8.0);
        self.chain(ui, &opts, fwd);
    }

    fn smile(&self, ui: &Ui, r: Rect, opts: &[&Opt], fwd: f64) {
        let p = ui.painter_at(r);
        p.rect_filled(r, 3, bg());
        p.text(r.left_top() + vec2(8.0, 10.0), Align2::LEFT_CENTER, t("opt.smile"), prop(11.5), MU);
        let pts: Vec<(f64, f64, bool)> = opts.iter().filter_map(|o| Some((o.strike, o.iv.filter(|v| *v > 0.0)?, o.call))).collect();
        if pts.len() < 2 { return; }
        let (klo, khi) = pts.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.0), h.max(x.0)));
        let (vlo, vhi) = pts.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.1), h.max(x.1)));
        let (vlo, vhi) = (vlo - (vhi - vlo) * 0.1, vhi + (vhi - vlo) * 0.1 + 1e-9);
        let plot = r.shrink2(vec2(36.0, 22.0));
        let xy = |k: f64, v: f64| pos2(plot.left() + ((k - klo) / (khi - klo)) as f32 * plot.width(), plot.bottom() - ((v - vlo) / (vhi - vlo)) as f32 * plot.height());
        if fwd > klo && fwd < khi {
            let x = xy(fwd, vlo).x;
            p.vline(x, plot.top()..=plot.bottom(), Stroke::new(1.0, DIM));
            p.text(pos2(x, plot.bottom() + 10.0), Align2::CENTER_CENTER, fmt_px(fwd), mono(10.0), MU);
        }
        for call in [true, false] {
            let line: Vec<_> = pts.iter().filter(|x| x.2 == call).map(|x| xy(x.0, x.1)).collect();
            let col = if call { up() } else { dn() };
            for q in &line { p.circle_filled(*q, 2.2, col); }
            if line.len() > 1 { p.add(egui::Shape::line(line, Stroke::new(1.2, col.linear_multiply(0.8)))); }
        }
        p.text(pos2(r.right() - 6.0, plot.top()), Align2::RIGHT_CENTER, format!("{:.0}%", vhi * 100.0), mono(10.0), DIM);
        p.text(pos2(r.right() - 6.0, plot.bottom()), Align2::RIGHT_CENTER, format!("{:.0}%", vlo * 100.0), mono(10.0), DIM);
        p.text(pos2(r.left() + 120.0, r.top() + 10.0), Align2::LEFT_CENTER, t("opt.calls"), prop(11.0), up());
        p.text(pos2(r.left() + 170.0, r.top() + 10.0), Align2::LEFT_CENTER, t("opt.puts"), prop(11.0), dn());
    }

    fn term(&self, ui: &Ui, r: Rect, a: &Agg) {
        let p = ui.painter_at(r);
        p.rect_filled(r, 3, bg());
        p.text(r.left_top() + vec2(8.0, 10.0), Align2::LEFT_CENTER, t("opt.term"), prop(11.5), MU);
        let lines: Vec<(Exchange, Vec<(f64, f64)>)> = a.chains.iter().map(|(e, c)| {
            (*e, c.expiries().iter().filter_map(|x| { let d = days(x.expiry_ms); (d > 0.05).then_some((d, x.atm_iv?)) }).filter(|x| x.1 > 0.0).collect())
        }).collect();
        let all: Vec<(f64, f64)> = lines.iter().flat_map(|l| l.1.iter().copied()).collect();
        if all.len() < 2 { return; }
        let (dlo, dhi) = (all.iter().map(|x| x.0).fold(f64::MAX, f64::min).ln(), all.iter().map(|x| x.0).fold(f64::MIN, f64::max).ln());
        let (vlo, vhi) = all.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.1), h.max(x.1)));
        let (vlo, vhi) = (vlo - (vhi - vlo) * 0.1, vhi + (vhi - vlo) * 0.1 + 1e-9);
        let plot = r.shrink2(vec2(30.0, 22.0));
        let xy = |d: f64, v: f64| pos2(plot.left() + ((d.ln() - dlo) / (dhi - dlo).max(1e-9)) as f32 * plot.width(), plot.bottom() - ((v - vlo) / (vhi - vlo)) as f32 * plot.height());
        let mut lx = r.left() + 90.0;
        for (e, l) in &lines {
            let pts: Vec<_> = l.iter().map(|x| xy(x.0, x.1)).collect();
            for q in &pts { p.circle_filled(*q, 2.0, ex_color(*e)); }
            if pts.len() > 1 { p.add(egui::Shape::line(pts, Stroke::new(1.2, ex_color(*e)))); }
            lx = p.text(pos2(lx, r.top() + 10.0), Align2::LEFT_CENTER, format!("{e:?}"), prop(10.5), ex_color(*e)).right() + 8.0;
        }
        for d in [1.0f64, 7.0, 30.0, 90.0, 180.0, 365.0] {
            if d.ln() < dlo || d.ln() > dhi { continue; }
            p.text(pos2(xy(d, vlo).x, plot.bottom() + 10.0), Align2::CENTER_CENTER, format!("{d}d"), mono(10.0), DIM);
        }
        p.text(pos2(r.right() - 4.0, plot.top()), Align2::RIGHT_CENTER, format!("{:.0}%", vhi * 100.0), mono(10.0), DIM);
        p.text(pos2(r.right() - 4.0, plot.bottom()), Align2::RIGHT_CENTER, format!("{:.0}%", vlo * 100.0), mono(10.0), DIM);
    }

    fn chain(&self, ui: &mut Ui, opts: &[&Opt], fwd: f64) {
        let mut strikes: Vec<f64> = opts.iter().map(|o| o.strike).collect();
        strikes.dedup();
        let atm = strikes.iter().copied().min_by(|a, b| (a - fwd).abs().total_cmp(&(b - fwd).abs()));
        let find = |k: f64, call: bool| opts.iter().find(|o| o.strike == k && o.call == call);
        let c = |ui: &mut Ui, s: String, col: egui::Color32| { ui.label(RichText::new(s).font(mono(11.5)).color(col)); };
        // fixed decimals per magnitude so a column lines up (USD premiums)
        let px = |v: Option<f64>| v.map(|x| super::fmt_dp(x, if x >= 1.0 { 2 } else { 4 })).unwrap_or_else(|| "-".into());
        // eleven columns spread over the full width
        let col_w = ((ui.available_width() - 10.0 * 16.0) / 11.0).max(60.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("chain").striped(true).spacing([16.0, 3.0]).min_col_width(col_w).show(ui, |ui| {
                for k in ["opt.bid", "opt.ask", "opt.mark", "opt.iv", "opt.delta", "opt.strike", "opt.delta", "opt.iv", "opt.mark", "opt.bid", "opt.ask"] {
                    ui.label(RichText::new(t(k)).font(prop(11.0)).color(DIM));
                }
                ui.end_row();
                for k in strikes {
                    let is_atm = Some(k) == atm;
                    for (call, rev) in [(true, false), (false, true)] {
                        let o = find(k, call);
                        let itm = if call { k < fwd } else { k > fwd };
                        let base = if itm { FG } else { MU };
                        let (bid, ask) = ((px(o.and_then(|o| o.bid.map(|b| b.0))), up()), (px(o.and_then(|o| o.ask.map(|b| b.0))), dn()));
                        let mark = (px(o.and_then(|o| o.mark)), base);
                        let iv = (o.and_then(|o| o.iv).map(|v| format!("{:.1}%", v * 100.0)).unwrap_or("-".into()), WARN);
                        // no greeks yet: a dash, not a fake zero
                        let delta = (o.filter(|o| o.iv.is_some() || o.delta != 0.0).map(|o| format!("{:+.3}", o.delta)).unwrap_or("-".into()), base);
                        // mirrored around the strike, but bid always left of ask (matches the header)
                        let cells = if rev { vec![delta, iv, mark, bid, ask] } else { vec![bid, ask, mark, iv, delta] };
                        for (s, col) in cells { c(ui, s, col); }
                        if call {
                            let s = RichText::new(fmt_px(k)).font(mono(12.0)).strong().color(if is_atm { accent() } else { FG });
                            ui.label(if is_atm { s.background_color(PANEL2) } else { s });
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }
}
