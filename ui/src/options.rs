//! Option view: venue + expiry selector, IV smile, ATM term structure across venues, T-chain, and
//! the order ticket in the right column. Clicking a bid sells at it, an ask buys at it, any other
//! cell picks the option at its mark. Orders are limit orders on the option venue's own book.
use super::engine::Engine;
use super::kit::{self, Look, H, H_LG, H_SM};
use super::{fmt_px, fmt_qty, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Rect, RichText, Sense, Stroke, Ui};
use depth::agg::{Agg, Opt};
use depth::trade::{self, OpenOrder, Tif};
use depth::{now_ms, Exchange, Side};

/// The option on the ticket.
#[derive(Clone, PartialEq)]
struct Pick { ex: Exchange, symbol: String }

struct OptTicket { ex: Exchange, symbol: String, buy: bool, px: f64, qty: f64, tif: Tif, reduce: bool }

pub struct OptView {
    venue: Option<Exchange>, expiry: Option<i64>,
    pick: Option<Pick>,
    buy: bool,
    px: String,
    qty: String,
    tif: Tif,
    reduce: bool,
    confirm: Option<OptTicket>,
    /// venues whose option orders were fetched once (later: resyncs and our own orders)
    asked: Vec<Exchange>,
}

impl Default for OptView {
    fn default() -> Self {
        OptView { venue: None, expiry: None, pick: None, buy: true, px: String::new(), qty: "1".into(), tif: Tif::Gtc, reduce: false, confirm: None, asked: vec![] }
    }
}

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
            ui.label(RichText::new(t("opt.expiry")).font(prop(11.0)).color(dim()));
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
            ui.label(RichText::new(t("opt.forward")).font(prop(11.0)).color(dim()));
            ui.label(RichText::new(fmt_px(fwd)).font(mono(12.5)).color(fg()))
                .on_hover_text(if info.forward_src == "venue" { t("opt.src_venue") } else { t("opt.src_parity") });
            ui.add_space(14.0);
            ui.label(RichText::new(t("opt.atm_iv")).font(prop(11.0)).color(dim()));
            ui.label(RichText::new(info.atm_iv.map(|v| format!("{:.2}%", v * 100.0)).unwrap_or("-".into())).font(mono(12.5)).color(WARN));
        });
        ui.add_space(4.0);

        let mut rows: Vec<(&str, &Opt)> = chain.opts.iter().filter(|(_, o)| o.has_info && o.expiry_ms == exp && (o.strike / fwd - 1.0).abs() < 0.25).map(|(k, o)| (&**k, o)).collect();
        rows.sort_by(|x, y| x.1.strike.total_cmp(&y.1.strike));
        let opts: Vec<&Opt> = rows.iter().map(|x| x.1).collect();

        // smile + term structure
        let top = ui.available_rect_before_wrap();
        let h = (top.height() * 0.38).clamp(140.0, 260.0);
        let smile = Rect::from_min_size(top.min, vec2(top.width() * 0.6 - 6.0, h));
        let term = Rect::from_min_max(pos2(smile.right() + 12.0, top.top()), pos2(top.right(), top.top() + h));
        ui.allocate_rect(Rect::from_min_size(top.min, vec2(top.width(), h)), Sense::hover());
        self.smile(ui, smile, &opts, fwd);
        self.term(ui, term, a);
        ui.add_space(8.0);
        if let Some((symbol, side, px)) = self.chain(ui, venue, &rows, fwd) {
            self.pick = Some(Pick { ex: venue, symbol });
            if let Some(b) = side { self.buy = b; }
            if let Some(p) = px { self.px = fmt_px(p).replace(',', ""); }
            self.reduce = false;
        }
    }

    fn smile(&self, ui: &Ui, r: Rect, opts: &[&Opt], fwd: f64) {
        let p = ui.painter_at(r);
        p.rect_filled(r, 3, bg());
        p.text(r.left_top() + vec2(8.0, 10.0), Align2::LEFT_CENTER, t("opt.smile"), prop(11.5), mu());
        let pts: Vec<(f64, f64, bool)> = opts.iter().filter_map(|o| Some((o.strike, o.iv.filter(|v| *v > 0.0)?, o.call))).collect();
        if pts.len() < 2 { return; }
        let (klo, khi) = pts.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.0), h.max(x.0)));
        let (vlo, vhi) = pts.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.1), h.max(x.1)));
        let (vlo, vhi) = (vlo - (vhi - vlo) * 0.1, vhi + (vhi - vlo) * 0.1 + 1e-9);
        let plot = r.shrink2(vec2(36.0, 22.0));
        let xy = |k: f64, v: f64| pos2(plot.left() + ((k - klo) / (khi - klo)) as f32 * plot.width(), plot.bottom() - ((v - vlo) / (vhi - vlo)) as f32 * plot.height());
        if fwd > klo && fwd < khi {
            let x = xy(fwd, vlo).x;
            p.vline(x, plot.top()..=plot.bottom(), Stroke::new(1.0, dim()));
            p.text(pos2(x, plot.bottom() + 10.0), Align2::CENTER_CENTER, fmt_px(fwd), mono(10.0), mu());
        }
        for call in [true, false] {
            let line: Vec<_> = pts.iter().filter(|x| x.2 == call).map(|x| xy(x.0, x.1)).collect();
            let col = if call { up() } else { dn() };
            for q in &line { p.circle_filled(*q, 2.2, col); }
            if line.len() > 1 { p.add(egui::Shape::line(line, Stroke::new(1.2, col.linear_multiply(0.8)))); }
        }
        p.text(pos2(r.right() - 6.0, plot.top()), Align2::RIGHT_CENTER, format!("{:.0}%", vhi * 100.0), mono(10.0), dim());
        p.text(pos2(r.right() - 6.0, plot.bottom()), Align2::RIGHT_CENTER, format!("{:.0}%", vlo * 100.0), mono(10.0), dim());
        p.text(pos2(r.left() + 120.0, r.top() + 10.0), Align2::LEFT_CENTER, t("opt.calls"), prop(11.0), up());
        p.text(pos2(r.left() + 170.0, r.top() + 10.0), Align2::LEFT_CENTER, t("opt.puts"), prop(11.0), dn());
    }

    fn term(&self, ui: &Ui, r: Rect, a: &Agg) {
        let p = ui.painter_at(r);
        p.rect_filled(r, 3, bg());
        p.text(r.left_top() + vec2(8.0, 10.0), Align2::LEFT_CENTER, t("opt.term"), prop(11.5), mu());
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
            p.text(pos2(xy(d, vlo).x, plot.bottom() + 10.0), Align2::CENTER_CENTER, format!("{d}d"), mono(10.0), dim());
        }
        p.text(pos2(r.right() - 4.0, plot.top()), Align2::RIGHT_CENTER, format!("{:.0}%", vhi * 100.0), mono(10.0), dim());
        p.text(pos2(r.right() - 4.0, plot.bottom()), Align2::RIGHT_CENTER, format!("{:.0}%", vlo * 100.0), mono(10.0), dim());
    }

    /// The chain grid; returns a click: (symbol, Some(buy) for an ask / bid cell, price to fill).
    fn chain(&self, ui: &mut Ui, venue: Exchange, rows: &[(&str, &Opt)], fwd: f64) -> Option<(String, Option<bool>, Option<f64>)> {
        let mut strikes: Vec<f64> = rows.iter().map(|o| o.1.strike).collect();
        strikes.dedup();
        let atm = strikes.iter().copied().min_by(|a, b| (a - fwd).abs().total_cmp(&(b - fwd).abs()));
        let find = |k: f64, call: bool| rows.iter().find(|o| o.1.strike == k && o.1.call == call);
        let picked = self.pick.as_ref().filter(|p| p.ex == venue).map(|p| p.symbol.as_str());
        let mut click = None;
        // every cell is clickable; the picked option's cells sit on a highlight
        let mut c = |ui: &mut Ui, s: String, col: egui::Color32, on: bool, act: Option<(String, Option<bool>, Option<f64>)>| {
            let txt = RichText::new(s).font(mono(11.5)).color(col);
            let r = ui.add(egui::Label::new(if on { txt.background_color(hl()) } else { txt }).sense(Sense::click()));
            if let Some(a) = act {
                let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                if r.clicked() { click = Some(a); }
            }
        };
        // fixed decimals per magnitude so a column lines up (USD premiums)
        let px = |v: Option<f64>| v.map(|x| super::fmt_dp(x, if x >= 1.0 { 2 } else { 4 })).unwrap_or_else(|| "-".into());
        // eleven columns spread over the full width
        let col_w = ((ui.available_width() - 10.0 * 16.0) / 11.0).max(60.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("chain").striped(true).spacing([16.0, 3.0]).min_col_width(col_w).show(ui, |ui| {
                for k in ["opt.bid", "opt.ask", "opt.mark", "opt.iv", "opt.delta", "opt.strike", "opt.delta", "opt.iv", "opt.mark", "opt.bid", "opt.ask"] {
                    ui.label(RichText::new(t(k)).font(prop(11.0)).color(dim()));
                }
                ui.end_row();
                for k in strikes {
                    let is_atm = Some(k) == atm;
                    for (call, rev) in [(true, false), (false, true)] {
                        let row = find(k, call);
                        let o = row.map(|r| r.1);
                        let sym = row.map(|r| r.0.to_string());
                        let on = sym.is_some() && sym.as_deref() == picked;
                        let itm = if call { k < fwd } else { k > fwd };
                        let base = if itm { fg() } else { mu() };
                        let (bid, ask) = ((px(o.and_then(|o| o.bid.map(|b| b.0))), up()), (px(o.and_then(|o| o.ask.map(|b| b.0))), dn()));
                        let mark = (px(o.and_then(|o| o.mark)), base);
                        let iv = (o.and_then(|o| o.iv).map(|v| format!("{:.1}%", v * 100.0)).unwrap_or("-".into()), WARN);
                        // no greeks yet: a dash, not a fake zero
                        let delta = (o.filter(|o| o.iv.is_some() || o.delta != 0.0).map(|o| format!("{:+.3}", o.delta)).unwrap_or("-".into()), base);
                        // mirrored around the strike, but bid always left of ask (matches the header)
                        // bid: sell at it, ask: buy at it, the rest: pick at the mark
                        let act = |side: Option<bool>, px: Option<f64>| sym.clone().map(|s| (s, side, px));
                        let (a_bid, a_ask) = (act(Some(false), o.and_then(|o| o.bid.map(|b| b.0))), act(Some(true), o.and_then(|o| o.ask.map(|b| b.0))));
                        let a_mark = act(None, o.and_then(|o| o.mark));
                        let cells = if rev { vec![(delta, a_mark.clone()), (iv, a_mark.clone()), (mark, a_mark), (bid, a_bid), (ask, a_ask)] }
                                    else { vec![(bid, a_bid), (ask, a_ask), (mark, a_mark.clone()), (iv, a_mark.clone()), (delta, a_mark)] };
                        for ((s, col), a) in cells { c(ui, s, col, on, a); }
                        if call {
                            let s = RichText::new(fmt_px(k)).font(mono(12.0)).strong().color(if is_atm { accent() } else { fg() });
                            ui.label(if is_atm { s.background_color(panel2()) } else { s });
                        }
                    }
                    ui.end_row();
                }
            });
        });
        click
    }

    /// Order ticket for the picked option, then this base's option positions and resting orders.
    pub fn ticket(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, confirm: bool) {
        let (keys, held, orders) = {
            let acc = eng.account.lock().unwrap();
            let held: Vec<trade::OptPos> = acc.opts.iter().filter(|o| o.base == base).cloned().collect();
            let orders: Vec<OpenOrder> = acc.opt_orders.iter().filter(|o| o.symbol.starts_with(&format!("{base}-"))).cloned().collect();
            (acc.keys.keys().copied().collect::<Vec<_>>(), held, orders)
        };
        for ex in trade::OPTION_VENUES {
            if keys.contains(ex) && !self.asked.contains(ex) { self.asked.push(*ex); eng.opt_refresh(*ex, ui.ctx()); }
        }
        kit::panel_frame(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            kit::title(ui, t("opt.order"));
            let Some(pk) = self.pick.clone() else { kit::note(ui, t("opt.pick_hint"), mu()); return; };
            let o = a.chains.get(&pk.ex).and_then(|c| c.opts.get(pk.symbol.as_str())).cloned().unwrap_or_default();
            ui.horizontal(|ui| {
                icon(ui.painter(), pk.ex, Rect::from_min_size(ui.cursor().min + vec2(0.0, 3.0), vec2(14.0, 14.0)), false);
                ui.add_space(18.0);
                ui.label(RichText::new(pk.symbol.trim_end_matches("-USDT")).font(mono_s(12.5)).color(fg_strong()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    kit::chip(ui, if o.call { t("opt.call") } else { t("opt.put") }, if o.call { kit::Tone::Up } else { kit::Tone::Down });
                    if o.expiry_ms > 0 { kit::chip(ui, &format!("{:.1}{}", days(o.expiry_ms), t("opt.days")), kit::Tone::Outline); }
                });
            });
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                let lvl = |l: Option<(f64, f64)>| l.map_or("—".into(), |(p, q)| format!("{} × {}", fmt_px(p), fmt_qty(q)));
                kit::kv(ui, t("opt.bid"), lvl(o.bid), up());
                kit::kv(ui, t("opt.ask"), lvl(o.ask), dn());
                kit::kv(ui, t("opt.mark"), o.mark.map_or("—".into(), fmt_px), fg());
                kit::kv(ui, "IV · Δ · Θ", format!("{} · {:+.3} · {:.2}", o.iv.map_or("—".into(), |v| format!("{:.1}%", v * 100.0)), o.delta, o.theta), mu());
            });
            super::spot::side_toggle(ui, &mut self.buy, (t("spot.buy"), t("spot.sell")));
            kit::segments(ui, &mut self.tif, &[(Tif::Gtc, t("spot.limit")), (Tif::PostOnly, t("spot.post")), (Tif::Ioc, "IOC")], H);
            let bad = parse(&self.px).is_none();
            kit::field(ui, t("spot.price"), &mut self.px, "USDT", "opt_px", bad);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = (ui.available_width() - 12.0) / 3.0;
                for (label, p) in [(t("opt.bid"), o.bid.map(|b| b.0)), (t("opt.mark"), o.mark), (t("opt.ask"), o.ask.map(|b| b.0))] {
                    if kit::button(ui, label, Look::Secondary, H_SM, Some(w), p.is_some()).clicked() { if let Some(p) = p { self.px = fmt_px(p).replace(',', ""); } }
                }
            });
            let bad = !self.qty.is_empty() && parse(&self.qty).is_none();
            kit::field(ui, t("spot.qty"), &mut self.qty, base, "opt_qty", bad);
            ui.horizontal(|ui| {
                kit::switch(ui, &mut self.reduce);
                ui.label(RichText::new(t("opt.reduce")).font(prop(12.0)).color(mu()));
            });
            let (px, qty) = (parse(&self.px), parse(&self.qty));
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                let prem = px.zip(qty).map(|(p, q)| p * q);
                kit::kv(ui, t(if self.buy { "opt.pay" } else { "opt.receive" }), prem.map_or("—".into(), |v| format!("{v:.2} USDT")), WARN);
                // a long option's breakeven at expiry: strike plus (call) or minus (put) the premium
                if let Some(p) = px.filter(|_| o.strike > 0.0) {
                    kit::kv(ui, t("opt.breakeven"), fmt_px(if o.call { o.strike + p } else { o.strike - p }), fg());
                }
                if self.buy { kit::kv(ui, t("ins.maxloss"), prem.map_or("—".into(), |v| format!("{v:.2} USDT")), fg()); }
            });
            if !self.buy && !self.reduce { kit::note(ui, t("opt.short_warn"), WARN); }
            let tradable = trade::OPTION_VENUES.contains(&pk.ex);
            let keyed = keys.contains(&pk.ex);
            if !tradable { kit::note(ui, t("opt.venue_soon"), WARN); }
            else if !keyed { kit::note(ui, t("ins.no_keys"), WARN); }
            let label = format!("{} {}", if self.buy { t("spot.buy") } else { t("spot.sell") }, qty.map(fmt_qty).unwrap_or_default());
            if kit::button(ui, &label, Look::Fill(if self.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), tradable && keyed && px.is_some() && qty.is_some()).clicked() {
                let tk = OptTicket { ex: pk.ex, symbol: pk.symbol.clone(), buy: self.buy, px: px.unwrap_or(0.0), qty: qty.unwrap_or(0.0), tif: self.tif, reduce: self.reduce };
                if confirm { self.confirm = Some(tk); } else { self.send(&tk, eng, ui.ctx()); }
            }
            self.held(ui, a, &held);
            self.open_orders(ui, eng, &orders);
        });
        self.confirm_dialog(ui, eng, base);
    }

    fn send(&mut self, tk: &OptTicket, eng: &Engine, ctx: &egui::Context) {
        eng.opt_submit(tk.ex, tk.symbol.clone(), if tk.buy { Side::Buy } else { Side::Sell }, tk.qty, tk.px, tk.tif, tk.reduce, ctx);
    }

    /// Held options of this base; a click loads the closing order (opposite side, reduce-only, at the touch).
    fn held(&mut self, ui: &mut Ui, a: &Agg, held: &[trade::OptPos]) {
        if held.is_empty() { return; }
        kit::overline(ui, t("opt.positions"));
        ui.spacing_mut().item_spacing.y = 0.0;
        for p in held {
            let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), Sense::click());
            let pnt = ui.painter();
            if resp.hovered() { pnt.rect_filled(r, kit::R, hl()); }
            let y = r.center().y;
            let long = p.side == Side::Buy;
            pnt.text(pos2(r.left() + 4.0, y), Align2::LEFT_CENTER, format!("{} {} {}", if long { "+" } else { "−" }, fmt_qty(p.qty), p.symbol.trim_end_matches("-USDT")), mono_m(11.0), if long { up() } else { dn() });
            pnt.text(pos2(r.right() - 4.0, y), Align2::RIGHT_CENTER, format!("{:+.2}", p.upnl), mono_m(11.0), if p.upnl >= 0.0 { up() } else { dn() });
            if resp.on_hover_text(format!("{} {}  {} {}\n{}", t("opt.avg"), fmt_px(p.avg), t("opt.mark"), fmt_px(p.mark), t("opt.close_hint"))).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                let o = a.chains.get(&p.ex).and_then(|c| c.opts.get(p.symbol.as_str()));
                self.pick = Some(Pick { ex: p.ex, symbol: p.symbol.clone() });
                self.buy = !long;
                self.reduce = true;
                self.qty = fmt_qty(p.qty).replace(',', "");
                if let Some(px) = o.and_then(|o| if long { o.bid } else { o.ask }).map(|l| l.0).or(Some(p.mark).filter(|m| *m > 0.0)) { self.px = fmt_px(px).replace(',', ""); }
            }
        }
        ui.spacing_mut().item_spacing.y = 10.0;
    }

    fn open_orders(&self, ui: &mut Ui, eng: &Engine, orders: &[OpenOrder]) {
        kit::overline(ui, &format!("{} ({})", t("spot.open_orders"), orders.len()));
        ui.spacing_mut().item_spacing.y = 0.0;
        for o in orders {
            let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), Sense::hover());
            let y = r.center().y;
            let col = if o.side == Side::Buy { up() } else { dn() };
            ui.painter().text(pos2(r.left(), y), Align2::LEFT_CENTER, format!("{} {}", if o.side == Side::Buy { "+" } else { "−" }, o.symbol.trim_end_matches("-USDT")), mono_m(11.0), col);
            ui.painter().text(pos2(r.right() - 26.0, y), Align2::RIGHT_CENTER, format!("{} @ {}", fmt_qty(o.qty - o.filled), fmt_px(o.price)), mono_m(11.0), fg());
            let x = Rect::from_center_size(pos2(r.right() - 9.0, y), vec2(18.0, 18.0));
            let resp = ui.interact(x, ui.id().with(("opt_cancel", &o.id)), Sense::click()).on_hover_text(t("spot.cancel"));
            ui.painter().text(x.center(), Align2::CENTER_CENTER, "✕", prop(12.0), if resp.hovered() { dn() } else { mu() });
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { eng.opt_cancel(o, ui.ctx()); }
        }
    }

    fn confirm_dialog(&mut self, ui: &mut Ui, eng: &Engine, base: &str) {
        let Some(tk) = self.confirm.take() else { return };
        let (mut cancel, mut send) = (false, false);
        let r = super::order::dialog("opt_confirm").show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            let side = if tk.buy { t("spot.buy") } else { t("spot.sell") };
            super::order::dialog_title(ui, &format!("{side} {}", tk.symbol.trim_end_matches("-USDT")), &format!("{:?} · {}", tk.ex, t("opt.order")), Some((side, if tk.buy { kit::Tone::Up } else { kit::Tone::Down })));
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                kit::kv(ui, t("spot.type"), format!("{} {}", t("spot.limit"), match tk.tif { Tif::PostOnly => "Post-only", Tif::Ioc => "IOC", Tif::Fok => "FOK", Tif::Gtc => "GTC" }), fg());
                kit::kv(ui, t("spot.price"), fmt_px(tk.px), fg());
                kit::kv(ui, t("spot.qty"), format!("{} {base}", fmt_qty(tk.qty)), fg_strong());
                if tk.reduce { kit::kv(ui, t("opt.reduce"), "✓".into(), fg()); }
                kit::kv(ui, t(if tk.buy { "opt.pay" } else { "opt.receive" }), format!("{:.2} USDT", tk.px * tk.qty), WARN);
            });
            kit::note(ui, t("ins.untested"), WARN);
            ui.columns(2, |c| {
                cancel = kit::button(&mut c[0], t("ins.cancel"), Look::Secondary, H_LG, Some(f32::INFINITY), true).clicked();
                send = kit::button(&mut c[1], t("spot.send"), Look::Fill(if tk.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), true).clicked();
            });
        });
        if send { self.send(&tk, eng, ui.ctx()); }
        if !(send || cancel || r.should_close()) { self.confirm = Some(tk); }
    }
}

fn parse(s: &str) -> Option<f64> { s.trim().replace(',', "").parse().ok().filter(|x: &f64| x.is_finite() && *x > 0.0) }
