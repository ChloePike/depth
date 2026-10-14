//! Position insurance quotes (options hedge) for the order column, and the trade venue / book
//! click the native-panel bridge reads. Order entry and the account strip live in order.rs and
//! account.rs. Rule: the composite index never prices an order.
use super::engine::Engine;
use super::{fmt_px, fmt_qty, t, theme::*};
use eframe::egui::{self, RichText, Ui};
use depth::agg::Agg;
use depth::insure::{self, Filter, Position as InsPos};
use depth::trade::{self, Position};
use depth::{now_ms, Exchange, Market, Side};

pub struct TradeView {
    pub ex: Exchange,
    /// price clicked in a single-venue order book or on the chart
    pub fill: Option<(Exchange, f64)>,
    ins_qty: String,
}

impl Default for TradeView {
    fn default() -> Self { TradeView { ex: Exchange::Bybit, fill: None, ins_qty: "1".into() } }
}

/// Bordered input: label inside on the left, right-aligned mono value, unit on the right;
/// the border turns accent while focused.
fn input_box(ui: &mut Ui, label: &str, text: &mut String, unit: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    let id = ui.id().with(("input", label));
    let focused = ui.memory(|m| m.has_focus(id));
    ui.painter().rect(rect, 4, panel2(), egui::Stroke::new(1.0, if focused { accent() } else { line() }), egui::StrokeKind::Inside);
    ui.painter().text(rect.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, label, prop(12.0), dim());
    let unit_w = ui.painter().text(rect.right_center() - egui::vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, unit, prop(11.5), mu()).width();
    let edit = egui::Rect::from_min_max(egui::pos2(rect.left() + 56.0, rect.top() + 4.0), egui::pos2(rect.right() - unit_w - 18.0, rect.bottom() - 4.0));
    ui.put(edit, egui::TextEdit::singleline(text).id(id).frame(egui::Frame::NONE).font(mono(13.5)).horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center))
}

impl TradeView {
    /// Cheapest option protection across option venues for the open position on this base
    /// (or a hypothetical long of `ins_qty` when there is none).
    fn insurance(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, pos: Option<Position>) {
        let mark = a.mid(Market::Perp).unwrap_or(0.0);
        let ip = match &pos {
            Some(p) => InsPos { side: p.side, qty: p.qty, entry: p.entry, mark },
            None => {
                input_box(ui, t("ins.sim_qty"), &mut self.ins_qty, base);
                InsPos { side: Side::Buy, qty: self.ins_qty.parse().unwrap_or(0.0), entry: mark, mark }
            }
        };
        if a.chains.is_empty() {
            ui.label(RichText::new(t("ins.loading")).color(dim()).font(prop(11.0)));
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
        ui.label(RichText::new(format!("{} {} {}  {}", if ip.side == Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(ip.qty), base, t("ins.hint"))).color(dim()).font(prop(10.5)));
        egui::Grid::new("ins").striped(true).spacing([8.0, 3.0]).show(ui, |ui| {
            // max loss lives in the hover so the table fits the 300px panel
            for k in ["ins.venue", "ins.exp", "ins.floor", "ins.cost"] { ui.label(RichText::new(t(k)).color(dim()).font(prop(10.5))); }
            ui.end_row();
            for q in best.iter().take(6) {
                ui.horizontal(|ui| { let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover()); icon(ui.painter(), q.ex, r, false); });
                ui.label(RichText::new(format!("{:.0}d", q.days)).font(mono(10.5)));
                ui.label(RichText::new(format!("{:.0} -{:.1}%", q.strike, q.floor_dist * 100.0)).font(mono(10.5)));
                ui.label(RichText::new(format!("{:.0}$ {:.1}%/y", q.cost, q.cost_apr * 100.0)).font(mono(10.5)).color(WARN))
                    .on_hover_text(format!("{} · ask {} · {} {:.0}% · {} {:.0}$ · {}", q.symbol, fmt_px(q.ask), t("ins.hedge"), q.hedge_ratio * 100.0, t("ins.maxloss"), q.max_loss, if q.fillable { t("ins.fillable") } else { t("ins.thin") }));
                ui.end_row();
            }
        });
        ui.label(RichText::new(t("ins.exec_note")).color(dim()).font(prop(10.0)));
    }

    /// Position insurance (options hedge) for this symbol's position on the trade venue, collapsed
    /// by default: opening it is what subscribes the option chains.
    pub fn insurance_section(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str) {
        let symbol = trade::symbol(base);
        let pos = eng.account.lock().unwrap().positions.iter().find(|p| p.ex == self.ex && p.symbol == symbol).cloned();
        egui::Frame::new().fill(panel2()).stroke(egui::Stroke::new(1.0, line())).corner_radius(10).inner_margin(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::CollapsingHeader::new(RichText::new(t("ins.title")).strong()).id_salt("insurance").default_open(false)
                .show(ui, |ui| self.insurance(ui, a, eng, base, pos));
        });
    }
}
