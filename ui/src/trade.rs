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
    /// options-hedge dialog opened from a position row: (venue, unified symbol)
    pub ins_pos: Option<(Exchange, String)>,
    /// quote row picked to buy: (option venue, option symbol)
    ins_pick: Option<(Exchange, String)>,
}

impl Default for TradeView {
    fn default() -> Self { TradeView { ex: Exchange::Bybit, fill: None, ins_qty: "1".into(), ins_open: false, ins_pos: None, ins_pick: None } }
}

/// The position's own mark and entry on its venue, in USDT like the option asks (USD / USDC
/// venues converted as their books are).
fn pos_px(p: &Position, a: &Agg) -> (f64, f64) {
    let k = a.usdt(p.ex, Market::Perp);
    (p.mark * k, p.entry * k)
}

impl TradeView {
    /// Cheapest option protection across option venues for the open position on this base
    /// (or a hypothetical long of `ins_qty` when there is none).
    fn insurance(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, pos: Option<Position>) {
        // the floor and max loss are measured from the position's own mark on its venue; the composite
        // only stands in when there is no position (or the venue reports no mark yet)
        let comp = a.mid(Market::Perp).unwrap_or(0.0);
        let ip = match &pos {
            Some(p) => { let (m, e) = pos_px(p, a); InsPos { side: p.side, qty: p.qty, entry: e, mark: if m > 0.0 { m } else { comp } } }
            None => {
                kit::field(ui, t("ins.sim_qty"), &mut self.ins_qty, base, "ins_qty", false);
                InsPos { side: Side::Buy, qty: self.ins_qty.parse().unwrap_or(0.0), entry: comp, mark: comp }
            }
        };
        let mark = ip.mark;
        if a.chains.is_empty() {
            kit::note(ui, t("ins.loading"), mu());
            eng.wants_options.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if ip.qty <= 0.0 || mark <= 0.0 { return; }
        let f = Filter { min_days: 2.0, max_days: 60.0, otm_max: 0.12 };
        // OKX and Deribit option prices are already USD; the other venues quote USDT. A Deribit inverse option pays
        // its intrinsic in coin at expiry, worth exactly the USD intrinsic, so payoff and max loss stay in USD
        let all = insure::quotes(&a.chains, |e| if matches!(e, Exchange::Okx | Exchange::Deribit) { 1.0 } else { a.usdt(e, Market::Perp) }, &ip, &f, now_ms());
        let mut best = insure::best_per_contract(&all);
        best.retain(|q| q.floor_dist >= 0.02);
        best.sort_by(|x, y| x.cost_apr.total_cmp(&y.cost_apr));
        let marked = pos.as_ref().filter(|p| p.mark > 0.0).map(|p| format!("  ·  {:?} {} {}", p.ex, t("acc.mark"), fmt_px(mark))).unwrap_or_default();
        kit::note(ui, &format!("{} {} {}{marked}  ·  {}", if ip.side == Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(ip.qty), base, t("ins.hint")), mu());
        // protection already held on any venue: listed, and only the uncovered rest is bought
        let held: Vec<trade::OptPos> = if pos.is_some() {
            eng.account.lock().unwrap().opts.iter().filter(|o| o.base == base && o.call != (ip.side == Side::Buy) && o.side == Side::Buy).cloned().collect()
        } else { vec![] };
        let rest = ip.qty - held.iter().map(|o| o.qty).sum::<f64>();
        for o in &held {
            kit::note(ui, &format!("{} · {:?} {} {} {} · {} {base}", t("ins.covered"), o.ex, o.expiry, if o.call { "C" } else { "P" }, fmt_px(o.strike), fmt_qty(o.qty)), up());
        }
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
            // a real position can buy the row: click prices it on the venue's ask side
            let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 24.0), if pos.is_some() { Sense::click() } else { Sense::hover() });
            let picked = self.ins_pick.as_ref().is_some_and(|(e, s)| *e == q.ex && **s == *q.symbol);
            let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || picked, 0.1);
            let p = ui.painter();
            if hv > 0.0 { p.rect_filled(r, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
            if resp.clicked() {
                self.ins_pick = Some((q.ex, q.symbol.to_string()));
                if trade::OPTION_VENUES.contains(&q.ex) && rest > 0.0 { eng.plan_option(q.ex, q.symbol.to_string(), rest, ui.ctx()); }
            }
            let resp = if pos.is_some() { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
            let y = r.center().y;
            icon(p, q.ex, egui::Rect::from_center_size(egui::pos2(x0 + 8.0, y), egui::vec2(14.0, 14.0)), false);
            p.text(egui::pos2(x0 + cols[1], y), Align2::RIGHT_CENTER, format!("{:.0}d", q.days), mono_m(11.5), fg());
            p.text(egui::pos2(x0 + cols[2], y), Align2::RIGHT_CENTER, format!("{:.0} −{:.1}%", q.strike, q.floor_dist * 100.0), mono_m(11.5), fg());
            p.text(egui::pos2(x0 + cols[3], y), Align2::RIGHT_CENTER, format!("{:.0}$ {:.1}%/y", q.cost, q.cost_apr * 100.0), mono_m(11.5), WARN);
            let mut tip = format!("{:?} {} · ask {} · {} {:.0}% · {}\n{} {} · {} {:.0}$ ({} {:.0}$)", q.ex, q.symbol, fmt_px(q.ask), t("ins.hedge"), q.hedge_ratio * 100.0,
                if q.fillable { t("ins.fillable") } else { t("ins.thin") }, t("ins.cap"), fmt_px(q.strike), t("ins.maxloss"), q.max_loss, t("ins.cost"), q.cost);
            // the hedge only pays if the perp survives down to the strike: options on another venue
            // add no margin to the perp's account
            if let Some(lq) = pos.as_ref().and_then(|p| p.liq).filter(|lq| if ip.side == Side::Buy { *lq > q.strike } else { *lq < q.strike }) {
                tip += &format!("\n⚠ {} {}", t("ins.liq_warn"), fmt_px(lq));
            }
            resp.on_hover_text(tip);
        }
        ui.add_space(8.0);
        let picked = self.ins_pick.as_ref().and_then(|(e, s)| best.iter().find(|q| q.ex == *e && *q.symbol == **s)).cloned();
        match (picked, &pos) {
            (Some(_), Some(_)) if rest <= 1e-9 => kit::note(ui, t("ins.full"), WARN),
            (Some(q), Some(_)) => { ui.spacing_mut().item_spacing.y = 8.0; self.ins_confirm(ui, eng, &q, &ip, base); }
            _ => kit::note(ui, t("ins.exec_note"), dim()),
        }
    }

    /// Confirm block for the picked quote: the venue's real ask side swept for the size, one limit IOC.
    fn ins_confirm(&mut self, ui: &mut Ui, eng: &Engine, q: &insure::Quote, ip: &InsPos, base: &str) {
        if !trade::OPTION_VENUES.contains(&q.ex) { kit::note(ui, t("ins.venue_soon"), WARN); return; }
        let (plan, keyed) = { let a = eng.account.lock().unwrap(); (a.opt_plan.clone(), a.keys.contains_key(&q.ex)) };
        if !keyed { kit::note(ui, t("ins.no_keys"), WARN); return; }
        let plan = match plan { Some(((e, s), r)) if e == q.ex && s == *q.symbol => r, _ => { kit::note(ui, t("ins.pricing"), mu()); return; } };
        let p = match plan { Ok(p) => p, Err(e) => { kit::note(ui, &e, WARN); return; } };
        let gap = if ip.side == Side::Buy { ip.mark - q.strike } else { q.strike - ip.mark };
        // Bybit quotes USDT; Deribit's inverse options are priced and paid in their coin (`p.usd` = its index)
        let coin = super::options::coin_quote(p.ex, &p.symbol);
        let px = |v: f64| match coin { Some(c) => format!("{v:.4} {c}"), None => fmt_px(v) };
        kit::group(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            kit::kv(ui, &format!("{:?}", q.ex), p.symbol.clone(), fg_strong());
            kit::kv(ui, t("ins.limit"), px(p.limit), fg());
            kit::kv(ui, t("ins.qty"), format!("{} {base}", fmt_qty(p.qty)), fg());
            kit::kv(ui, t("ins.avg"), px(p.avg), fg());
            kit::kv(ui, t("ins.total"), super::options::amount(p.cost, p.ex, &p.symbol, Some(p.usd)), WARN);
            kit::kv(ui, t("ins.maxloss"), format!("{:.0}$", max_loss(gap, ip.qty, &p)), fg());
        });
        if p.fill + 1e-9 < p.qty { kit::note(ui, &format!("{} {} / {} {base}", t("ins.partial"), fmt_qty(p.fill), fmt_qty(p.qty)), WARN); }
        if let Some(c) = coin { kit::note(ui, &t("ins.coin_note").replace("{coin}", c), WARN); }
        kit::note(ui, t("ins.untested"), WARN);
        let (mut cancel, mut buy) = (false, false);
        ui.columns(2, |c| {
            cancel = kit::button(&mut c[0], t("ins.cancel"), kit::Look::Secondary, kit::H, Some(f32::INFINITY), true).clicked();
            buy = kit::button(&mut c[1], t("ins.buy"), kit::Look::Fill(up()), kit::H, Some(f32::INFINITY), true).clicked();
        });
        if cancel { self.ins_pick = None; }
        if buy { eng.buy_option(p, ui.ctx()); self.ins_pick = None; }
    }

    /// Options-hedge dialog for one position (any venue), quotes from every option venue.
    pub fn insure_modal(&mut self, ctx: &egui::Context, a: &Agg, eng: &Engine) {
        let Some((ex, symbol)) = self.ins_pos.clone() else { return };
        let base = symbol.strip_suffix("USDT").unwrap_or(&symbol).to_string();
        let pos = eng.account.lock().unwrap().positions.iter().find(|p| p.ex == ex && p.symbol == symbol).cloned();
        // the position was closed meanwhile
        if pos.is_none() { self.ins_pos = None; return; }
        let r = super::order::dialog("insure").show(ctx, |ui| {
            ui.set_width(400.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            super::order::dialog_title(ui, &format!("{ex:?} · {symbol}"), t("ins.title"), None);
            // the engine is still switching to this base: its marks and chains are another asset's
            if eng.base != base { kit::note(ui, t("ins.loading"), mu()); } else { self.insurance(ui, a, eng, &base, pos); }
        });
        if r.should_close() { self.ins_pos = None; self.ins_pick = None; }
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

/// Loss from the position's mark down to the floor plus the premium in USD (a coin premium at the plan's index).
fn max_loss(gap: f64, qty: f64, p: &trade::OptBuy) -> f64 { gap * qty + p.cost * p.usd }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_loss_counts_a_coin_premium_in_usd() {
        let p = trade::OptBuy { ex: Exchange::Deribit, symbol: "BTC-6NOV26-76000-P".into(), qty: 1.0, limit: 0.012, tick: 0.0001, step: 0.1, fill: 1.0, avg: 0.012, cost: 0.012, usd: 80000.0 };
        // 4000 down to the floor plus 0.012 BTC at 80000 = 960 $, never 4000 + 0.012
        assert!((max_loss(4000.0, 1.0, &p) - 4960.0).abs() < 1e-9);
        assert_eq!(max_loss(4000.0, 1.0, &trade::OptBuy { ex: Exchange::Bybit, cost: 900.0, usd: 1.0, ..p }), 4900.0);
    }

    #[test]
    fn position_prices_are_in_usdt_of_its_venue() {
        let mut a = Agg::default();
        a.set_fx("USDT", 1.0);
        a.set_fx("USDC", 0.998);
        let pos = |ex| Position { ex, symbol: "BTCUSDT".into(), side: Side::Buy, qty: 1.0, entry: 100.0, mark: 100.0, liq: None, upnl: 0.0, lev: 1.0, margin: 0.0, cross: None };
        let (m, e) = pos_px(&pos(Exchange::Hyperliquid), &a);
        assert!((m - 99.8).abs() < 1e-9 && (e - 99.8).abs() < 1e-9);
        assert_eq!(pos_px(&pos(Exchange::Bybit), &a), (100.0, 100.0));
    }
}
