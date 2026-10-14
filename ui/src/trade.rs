//! Position insurance quotes (options hedge) for the order column, and the trade venue / book
//! click the native-panel bridge reads. Order entry and the account strip live in order.rs and
//! account.rs. Rule: the composite index never prices an order.
use super::engine::Engine;
use super::{fmt_px, fmt_qty, kit, t, theme::*};
use eframe::egui::{self, Align2, Sense, Ui};
use depth::agg::Agg;
use depth::insure::{self, Filter, Position as InsPos};
use depth::trade::{self, Position};
use depth::{now_ms, Exchange, Market, Side};

pub struct TradeView {
    pub ex: Exchange,
    /// price clicked in a single-venue order book or on the chart
    pub fill: Option<(Exchange, f64)>,
    ins_qty: String,
    /// insurance section expanded (opening it subscribes the option chains)
    ins_open: bool,
}

impl Default for TradeView {
    fn default() -> Self { TradeView { ex: Exchange::Bybit, fill: None, ins_qty: "1".into(), ins_open: false } }
}

impl TradeView {
    /// Cheapest option protection across option venues for the open position on this base
    /// (or a hypothetical long of `ins_qty` when there is none).
    fn insurance(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, pos: Option<Position>) {
        let mark = a.mid(Market::Perp).unwrap_or(0.0);
        let ip = match &pos {
            Some(p) => InsPos { side: p.side, qty: p.qty, entry: p.entry, mark },
            None => {
                kit::field(ui, t("ins.sim_qty"), &mut self.ins_qty, base, "ins_qty", false);
                InsPos { side: Side::Buy, qty: self.ins_qty.parse().unwrap_or(0.0), entry: mark, mark }
            }
        };
        if a.chains.is_empty() {
            kit::note(ui, t("ins.loading"), mu());
            eng.wants_options.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if ip.qty <= 0.0 || mark <= 0.0 { return; }
        let f = Filter { min_days: 2.0, max_days: 60.0, otm_max: 0.12 };
        // OKX option prices are already USD; the other venues quote USDT
        let all = insure::quotes(&a.chains, |e| if e == Exchange::Okx { 1.0 } else { a.usdt(e, Market::Perp) }, &ip, &f, now_ms());
        let mut best = insure::best_per_contract(&all);
        best.retain(|q| q.floor_dist >= 0.02);
        best.sort_by(|x, y| x.cost_apr.total_cmp(&y.cost_apr));
        kit::note(ui, &format!("{} {} {}  ·  {}", if ip.side == Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(ip.qty), base, t("ins.hint")), mu());
        // table: venue | expiry | floor | cost, numbers right-aligned; max loss lives in the hover so
        // it fits the 300 px column
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let cols = [0.0, w * 0.27, w * 0.66, w];
        let (hr, _) = ui.allocate_exact_size(egui::vec2(w, 22.0), Sense::hover());
        let x0 = hr.left();
        kit::th(ui.painter(), egui::pos2(x0, hr.center().y), Align2::LEFT_CENTER, t("ins.venue"), None);
        for (i, k) in ["ins.exp", "ins.floor", "ins.cost"].into_iter().enumerate() {
            kit::th(ui.painter(), egui::pos2(x0 + cols[i + 1], hr.center().y), Align2::RIGHT_CENTER, t(k), None);
        }
        ui.painter().hline(hr.x_range(), hr.bottom(), egui::Stroke::new(1.0, line()));
        for q in best.iter().take(6) {
            let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 24.0), Sense::hover());
            let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
            let p = ui.painter();
            if hv > 0.0 { p.rect_filled(r, kit::R, hl().gamma_multiply(0.7 * hv)); }
            let y = r.center().y;
            icon(p, q.ex, egui::Rect::from_center_size(egui::pos2(x0 + 8.0, y), egui::vec2(14.0, 14.0)), false);
            p.text(egui::pos2(x0 + cols[1], y), Align2::RIGHT_CENTER, format!("{:.0}d", q.days), mono_m(11.5), fg());
            p.text(egui::pos2(x0 + cols[2], y), Align2::RIGHT_CENTER, format!("{:.0} −{:.1}%", q.strike, q.floor_dist * 100.0), mono_m(11.5), fg());
            p.text(egui::pos2(x0 + cols[3], y), Align2::RIGHT_CENTER, format!("{:.0}$ {:.1}%/y", q.cost, q.cost_apr * 100.0), mono_m(11.5), WARN);
            resp.on_hover_text(format!("{:?} {} · ask {} · {} {:.0}% · {} {:.0}$ · {}", q.ex, q.symbol, fmt_px(q.ask), t("ins.hedge"), q.hedge_ratio * 100.0, t("ins.maxloss"), q.max_loss, if q.fillable { t("ins.fillable") } else { t("ins.thin") }));
        }
        ui.add_space(8.0);
        kit::note(ui, t("ins.exec_note"), dim());
    }

    /// Position insurance (options hedge) for this symbol's position on the trade venue, collapsed
    /// by default: opening it is what subscribes the option chains.
    pub fn insurance_section(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str) {
        let symbol = trade::symbol(base);
        let pos = eng.account.lock().unwrap().positions.iter().find(|p| p.ex == self.ex && p.symbol == symbol).cloned();
        kit::panel_frame(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            // header row: title 13.5 semibold, chevron on the right; the whole row toggles
            let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), Sense::click());
            let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
            let open = ui.ctx().animate_bool_with_time(resp.id.with("open"), self.ins_open, 0.14);
            let p = ui.painter();
            p.text(r.left_center(), Align2::LEFT_CENTER, t("ins.title"), prop_s(13.5), fg_strong());
            // chevron: right-pointing when closed, rotating down when open
            let c = egui::pos2(r.right() - 6.0, r.center().y);
            let rot = egui::emath::Rot2::from_angle(open * std::f32::consts::FRAC_PI_2);
            let pts = [egui::vec2(-2.0, -4.0), egui::vec2(2.0, 0.0), egui::vec2(-2.0, 4.0)].map(|v| c + rot * v);
            p.add(egui::Shape::line(pts.to_vec(), egui::Stroke::new(1.4, lerp(mu(), fg_strong(), hv))));
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.ins_open = !self.ins_open; }
            if self.ins_open { self.insurance(ui, a, eng, base, pos); }
        });
    }
}
