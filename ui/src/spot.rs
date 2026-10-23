//! Spot and margin order entry for the order column in Spot / Margin mode (Bybit, Binance): buy /
//! sell, limit, post-only or market, sized in base units or in USDT. Prices come from the chosen
//! venue's own spot book, never the composite index; a click in the book or on the chart sets the
//! price. The confirm dialog (when on in Settings) shows exactly what is sent; the symbol's resting
//! orders are listed under the ticket with a cancel each.
use super::engine::Engine;
use super::kit::{self, Look, H, H_LG, H_SM};
use super::hotkeys::Act;
use super::order::{dialog, dialog_title};
use super::{fmt_px, fmt_qty, t, theme::*};
use depth::agg::Agg;
use depth::trade::{self, spot::Effect, Kind, OpenOrder, Tif};
use depth::{Exchange, Market, Side};
use eframe::egui::{self, Align2, RichText, Sense, Ui};

struct Ticket { ex: Exchange, symbol: String, buy: bool, kind: Kind, qty: f64, ref_px: f64, margin: Option<Effect> }

#[derive(Clone, Copy, PartialEq)]
enum Type { Limit, Post, Market }

pub struct SpotPanel {
    ex: Exchange,
    buy: bool,
    ty: Type,
    /// margin mode: what the order does with loans
    effect: Effect,
    /// limit price text; follows the venue's touch until edited
    px: String,
    px_edited: bool,
    qty: String,
    /// size typed in USDT instead of the base coin
    in_quote: bool,
    confirm: Option<Ticket>,
    /// venues whose balances were asked for once per mode (later updates come with resyncs and orders)
    asked: Vec<(Exchange, bool)>,
    /// last book / chart click seen (None until the first frame), so each click sets the price once
    last_fill: Option<Option<(Exchange, f64)>>,
    /// when this panel last sent something: order-log entries since then show under the ticket
    sent_ms: i64,
}

impl Default for SpotPanel {
    fn default() -> Self {
        SpotPanel { ex: Exchange::Bybit, buy: true, ty: Type::Limit, effect: Effect::Borrow, px: String::new(), px_edited: false, qty: String::new(), in_quote: false,
                    confirm: None, asked: vec![], last_fill: None, sent_ms: 0 }
    }
}

fn parse(s: &str) -> Option<f64> { s.trim().replace(',', "").parse().ok().filter(|x: &f64| x.is_finite() && *x > 0.0) }
fn trim(q: f64, dp: usize) -> String { format!("{q:.dp$}").trim_end_matches('0').trim_end_matches('.').to_string() }

/// Buy / sell as two halves, the chosen one filled green or red.
pub(crate) fn side_toggle(ui: &mut Ui, buy: &mut bool, labels: (&str, &str)) -> bool {
    let mut changed = false;
    ui.columns(2, |c| {
        for (i, (v, label, col)) in [(true, labels.0, up()), (false, labels.1, dn())].into_iter().enumerate() {
            let look = if *buy == v { Look::Fill(col) } else { Look::Secondary };
            if kit::button(&mut c[i], label, look, H, Some(f32::INFINITY), true).clicked() && *buy != v { *buy = v; changed = true; }
        }
    });
    changed
}

impl SpotPanel {
    /// `market` is Spot or Margin; `fill` is the last price clicked in the book or on the chart.
    pub fn show(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, market: Market, fill: Option<(Exchange, f64)>, confirm: bool) {
        let margin = market == Market::Margin;
        let symbol = trade::symbol(base);
        if self.last_fill != Some(fill) {
            if let Some((e, p)) = fill.filter(|_| self.last_fill.is_some()) {
                if trade::spot::VENUES.contains(&e) { self.ex = e; }
                self.px = fmt_px(p).replace(',', "");
                self.px_edited = true;
                if self.ty == Type::Market { self.ty = Type::Limit; }
            }
            self.last_fill = Some(fill);
        }
        // the venue's own touch, native quote (USDT on both venues)
        let touch = a.venues.get(&(self.ex, Market::Spot)).and_then(|v| v.bbo.map(|b| (b[0], b[2])).or_else(|| Some((v.book.best_bid()?.0, v.book.best_ask()?.0))));
        let (keyed, bal, orders) = {
            let mut acc = eng.account.lock().unwrap();
            if margin && acc.margin_sym.as_deref() != Some(&symbol) { acc.margin_sym = Some(symbol.clone()); self.asked.retain(|x| !x.1); }
            let bal = if margin { acc.margin_bal.get(&self.ex) } else { acc.spot_bal.get(&self.ex) }.cloned();
            // Bybit margin orders are spot orders with isLeverage: listed in both modes
            let mut orders: Vec<(OpenOrder, bool)> = acc.spot_orders.iter().filter(|o| o.ex == self.ex && o.symbol == symbol).map(|o| (o.clone(), false)).collect();
            if margin { orders.extend(acc.margin_orders.iter().filter(|o| o.ex == self.ex && o.symbol == symbol).map(|o| (o.clone(), true))); }
            (acc.keys.contains_key(&self.ex), bal, orders)
        };
        if keyed && bal.is_none() && !self.asked.contains(&(self.ex, margin)) { self.asked.push((self.ex, margin)); eng.spot_refresh(self.ex, ui.ctx()); }
        let coin = |c: &str| bal.as_ref().and_then(|b| b.iter().find(|x| x.coin == c)).cloned();
        let free = |c: &str| coin(c).map_or(0.0, |x| x.free);
        kit::panel_frame(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.horizontal(|ui| {
                kit::title(ui, t(if margin { "spot.margin_title" } else { "spot.title" }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if keyed && kit::button(ui, "↻", Look::Ghost, 20.0, Some(22.0), true).on_hover_text(t("spot.refresh")).clicked() { eng.spot_refresh(self.ex, ui.ctx()); }
                    kit::chip(ui, &format!("{base}/USDT"), kit::Tone::Neutral);
                });
            });
            let venues: Vec<(Exchange, &str)> = trade::spot::VENUES.iter().map(|e| (*e, if *e == Exchange::Bybit { "Bybit" } else { "Binance" })).collect();
            if kit::segments(ui, &mut self.ex, &venues, H) { self.px_edited = false; }
            if side_toggle(ui, &mut self.buy, (t("spot.buy"), t("spot.sell"))) { self.px_edited = false; }
            kit::segments(ui, &mut self.ty, &[(Type::Limit, t("spot.limit")), (Type::Post, t("spot.post")), (Type::Market, t("spot.market"))], H);
            if margin {
                kit::segments(ui, &mut self.effect, &[(Effect::Plain, t("spot.normal")), (Effect::Borrow, t("spot.borrow")), (Effect::Repay, t("spot.repay"))], H_SM);
            }
            // passive default: join the bid when buying, the ask when selling
            let quote_px = touch.map(|(b, k)| if self.buy { b } else { k });
            if !self.px_edited { if let Some(p) = quote_px { self.px = fmt_px(p).replace(',', ""); } }
            if self.ty != Type::Market {
                let bad = parse(&self.px).is_none();
                let (r, _) = kit::field(ui, t("spot.price"), &mut self.px, "USDT", "spot_px", bad);
                if r.changed() { self.px_edited = true; }
                // one-click prices from the venue's own touch
                if let Some((b, k)) = touch {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let w = (ui.available_width() - 12.0) / 3.0;
                        for (label, p) in [(t("opt.bid"), b), (t("spot.mid"), (b + k) / 2.0), (t("opt.ask"), k)] {
                            if kit::button(ui, label, Look::Secondary, H_SM, Some(w), true).on_hover_text(fmt_px(p)).clicked() {
                                self.px = fmt_px(p).replace(',', "");
                                self.px_edited = true;
                            }
                        }
                    });
                }
            }
            // market orders: priced for the checks and the estimate at the touch they will take
            let px = if self.ty == Type::Market { touch.map(|(b, k)| if self.buy { k } else { b }) } else { parse(&self.px) };
            let unit = if self.in_quote { "USDT" } else { base };
            let bad = !self.qty.is_empty() && parse(&self.qty).is_none();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = ui.available_width() - 62.0;
                ui.allocate_ui(egui::vec2(w, H), |ui| kit::field(ui, t("spot.qty"), &mut self.qty, "", "spot_qty", bad));
                // the unit toggle converts what is typed at the current price
                if kit::button(ui, unit, Look::Secondary, H, Some(56.0), true).on_hover_text(t("spot.unit_hint")).clicked() {
                    if let (Some(q), Some(p)) = (parse(&self.qty), px) { self.qty = if self.in_quote { trim(q / p, 8) } else { trim(q * p, 2) }; }
                    self.in_quote = !self.in_quote;
                }
            });
            // available: USDT when buying, the coin when selling; margin with Borrow adds nothing here
            // (the venue lends up to its own limit)
            let avail = if self.buy { free("USDT") } else { free(base) };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = (ui.available_width() - 18.0) / 4.0;
                for pct in [25.0, 50.0, 75.0, 100.0] {
                    let on = keyed && avail > 0.0 && px.is_some();
                    if kit::button(ui, &format!("{pct:.0}%"), Look::Secondary, H_SM, Some(w), on).clicked() {
                        let p = px.unwrap_or(1.0);
                        let base_q = if self.buy { avail * pct / 100.0 / p } else { avail * pct / 100.0 };
                        self.qty = if self.in_quote { trim(base_q * p, 2) } else { trim(base_q, 8) };
                    }
                }
            });
            // size in base units whatever was typed
            let qty = parse(&self.qty).and_then(|q| if self.in_quote { px.map(|p| q / p) } else { Some(q) });
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                let show = |c: &str| coin(c).map(|b| format!("{} {c}", fmt_qty(b.free))).unwrap_or_else(|| format!("0 {c}"));
                kit::kv(ui, t("spot.avail"), if keyed { show("USDT") } else { "—".into() }, if self.buy { fg_strong() } else { mu() });
                kit::kv(ui, "", if keyed { show(base) } else { "—".into() }, if self.buy { mu() } else { fg_strong() });
                if margin {
                    let debt: Vec<String> = [base, "USDT"].iter().filter_map(|c| coin(c).filter(|b| b.borrowed > 0.0).map(|b| format!("{} {c}", fmt_qty(b.borrowed)))).collect();
                    kit::kv(ui, t("spot.borrowed"), if debt.is_empty() { "—".into() } else { debt.join(" · ") }, if debt.is_empty() { mu() } else { WARN });
                    let apr = a.venues.get(&(self.ex, Market::Margin)).and_then(|v| v.borrow_apr.get(if self.buy { "USDT" } else { base }).copied());
                    kit::kv(ui, &format!("{} {}", t("spot.borrow_apr"), if self.buy { "USDT" } else { base }), apr.map_or("—".into(), |r| format!("{:.2}%", r * 100.0)), mu());
                }
                kit::kv(ui, t("spot.total"), match (qty, px) { (Some(q), Some(p)) => format!("≈ {:.2} USDT", q * p), _ => "—".into() }, fg());
                if self.in_quote { kit::kv(ui, t("spot.qty"), qty.map_or("—".into(), |q| format!("≈ {} {base}", fmt_qty(q))), mu()); }
            });
            if !keyed { kit::note(ui, t("spot.no_keys"), WARN); return; }
            let label = format!("{} {base}{}", if self.buy { t("spot.buy") } else { t("spot.sell") }, qty.map(|q| format!("  {}", fmt_qty(q))).unwrap_or_default());
            let ok = qty.is_some() && px.is_some();
            if kit::button(ui, &label, Look::Fill(if self.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), ok).clicked() {
                let (q, p) = (qty.unwrap_or(0.0), px.unwrap_or(0.0));
                let kind = match self.ty { Type::Market => Kind::Market, Type::Limit => Kind::Limit { price: p, tif: Tif::Gtc }, Type::Post => Kind::Limit { price: p, tif: Tif::PostOnly } };
                let tk = Ticket { ex: self.ex, symbol: symbol.clone(), buy: self.buy, kind, qty: q, ref_px: p, margin: margin.then_some(self.effect) };
                if confirm { self.confirm = Some(tk); } else { self.send(&tk, eng, ui.ctx()); }
            }
            if self.ex == Exchange::Binance && !margin { kit::note(ui, t("spot.binance_wallet"), dim()); }
            if margin && self.ex == Exchange::Bybit { kit::note(ui, t("spot.bybit_margin"), dim()); }
            {
                let acc = eng.account.lock().unwrap();
                let log: Vec<(i64, &str, bool)> = acc.log.iter().rev().map(|(ts, m, ok)| (*ts, m.as_str(), *ok)).collect();
                super::order::results(ui, self.sent_ms, &log);
            }
            self.orders(ui, eng, &orders);
        });
        self.confirm_dialog(ui, eng, base);
    }

    /// A trading hotkey in Spot / Margin mode, on this panel's venue: buy / sell `usdt` notional at
    /// the venue's own touch (a limit joining the queue) or at market, through the same confirmation
    /// as the button (margin orders keep the panel's loan setting); cancel the symbol's orders.
    /// Close-all has no spot meaning (it would sell the coin) and does nothing here.
    #[allow(clippy::too_many_arguments)]
    pub fn hotkey(&mut self, act: Act, usdt: f64, a: &Agg, eng: &Engine, base: &str, market: Market, confirm: bool, ctx: &egui::Context) {
        if self.confirm.is_some() { return; }
        let symbol = trade::symbol(base);
        let margin = market == Market::Margin;
        if act == Act::CancelAll {
            let acc = eng.account.lock().unwrap();
            let mut os: Vec<(OpenOrder, bool)> = acc.spot_orders.iter().filter(|o| o.ex == self.ex && o.symbol == symbol).map(|o| (o.clone(), false)).collect();
            if margin { os.extend(acc.margin_orders.iter().filter(|o| o.ex == self.ex && o.symbol == symbol).map(|o| (o.clone(), true))); }
            drop(acc);
            self.sent_ms = depth::now_ms();
            for (o, m) in os { eng.spot_cancel(&o, m, ctx); }
            return;
        }
        let buy = matches!(act, Act::BuyBid | Act::BuyMarket);
        let mkt = matches!(act, Act::BuyMarket | Act::SellMarket);
        if usdt <= 0.0 || !matches!(act, Act::BuyBid | Act::SellAsk | Act::BuyMarket | Act::SellMarket) { return; }
        let touch = a.venues.get(&(self.ex, Market::Spot)).and_then(|v| v.bbo.map(|b| (b[0], b[2])).or_else(|| Some((v.book.best_bid()?.0, v.book.best_ask()?.0))));
        let Some((b, k)) = touch else { return };
        // limit: join our own side; market: priced at the side it takes
        let p = if mkt == buy { k } else { b };
        let kind = if mkt { Kind::Market } else { Kind::Limit { price: p, tif: Tif::Gtc } };
        let tk = Ticket { ex: self.ex, symbol, buy, kind, qty: parse(&trim(usdt / p, 8)).unwrap_or(0.0), ref_px: p, margin: margin.then_some(self.effect) };
        if tk.qty <= 0.0 { return; }
        if confirm { self.confirm = Some(tk); } else { self.send(&tk, eng, ctx); }
    }

    fn send(&mut self, tk: &Ticket, eng: &Engine, ctx: &egui::Context) {
        self.sent_ms = depth::now_ms();
        eng.spot_submit(tk.ex, tk.symbol.clone(), if tk.buy { Side::Buy } else { Side::Sell }, tk.kind, tk.qty, tk.ref_px, tk.margin, ctx);
        self.qty.clear();
    }

    /// Resting orders of this symbol on this venue: side, price, filled / size, cancel.
    fn orders(&self, ui: &mut Ui, eng: &Engine, orders: &[(OpenOrder, bool)]) {
        ui.add_space(2.0);
        kit::overline(ui, &format!("{} ({})", t("spot.open_orders"), orders.len()));
        if orders.is_empty() { kit::note(ui, t("spot.no_orders"), dim()); return; }
        ui.spacing_mut().item_spacing.y = 0.0;
        for (o, margin) in orders {
            let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), Sense::hover());
            let p = ui.painter();
            let y = r.center().y;
            let col = if o.side == Side::Buy { up() } else { dn() };
            p.text(egui::pos2(r.left(), y), Align2::LEFT_CENTER, if o.side == Side::Buy { t("spot.buy") } else { t("spot.sell") }, prop_s(11.5), col);
            p.text(egui::pos2(r.left() + 44.0, y), Align2::LEFT_CENTER, if o.price > 0.0 { fmt_px(o.price) } else { o.kind.clone() }, mono_m(11.5), fg());
            p.text(egui::pos2(r.right() - 30.0, y), Align2::RIGHT_CENTER, format!("{} / {}", fmt_qty(o.filled), fmt_qty(o.qty)), mono_m(11.5), mu());
            let x = egui::Rect::from_center_size(egui::pos2(r.right() - 10.0, y), egui::vec2(20.0, 20.0));
            let resp = ui.interact(x, ui.id().with(("spot_cancel", &o.id)), Sense::click()).on_hover_text(t("spot.cancel"));
            ui.painter().text(x.center(), Align2::CENTER_CENTER, "✕", prop(12.0), if resp.hovered() { dn() } else { mu() });
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { eng.spot_cancel(o, *margin, ui.ctx()); }
        }
    }

    fn confirm_dialog(&mut self, ui: &mut Ui, eng: &Engine, base: &str) {
        let Some(tk) = self.confirm.take() else { return };
        let (mut cancel, mut send) = (false, false);
        let r = dialog("spot_confirm").show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            let side = if tk.buy { t("spot.buy") } else { t("spot.sell") };
            let what = if tk.margin.is_some() { t("spot.margin_title") } else { t("spot.title") };
            dialog_title(ui, &format!("{side} {base}"), &format!("{:?} · {what}", tk.ex), Some((side, if tk.buy { kit::Tone::Up } else { kit::Tone::Down })));
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                kit::kv(ui, t("spot.type"), match tk.kind { Kind::Market => t("spot.market").to_string(), Kind::Limit { tif: Tif::PostOnly, .. } => t("spot.post").to_string(), _ => format!("{} GTC", t("spot.limit")) }, fg());
                if let Some(e) = tk.margin { kit::kv(ui, t("spot.loan"), t(match e { Effect::Plain => "spot.normal", Effect::Borrow => "spot.borrow", Effect::Repay => "spot.repay" }).into(), fg()); }
                kit::kv(ui, t("spot.price"), match tk.kind { Kind::Limit { price, .. } => fmt_px(price), _ => format!("≈ {}", fmt_px(tk.ref_px)) }, fg());
                kit::kv(ui, t("spot.qty"), format!("{} {base}", fmt_qty(tk.qty)), fg_strong());
                kit::kv(ui, t("spot.total"), format!("≈ {:.2} USDT", tk.qty * tk.ref_px), WARN);
            });
            ui.label(RichText::new(t("spot.untested")).font(prop(11.5)).color(WARN));
            ui.columns(2, |c| {
                cancel = kit::button(&mut c[0], t("ins.cancel"), Look::Secondary, H_LG, Some(f32::INFINITY), true).clicked();
                send = kit::button(&mut c[1], t("spot.send"), Look::Fill(if tk.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), true).clicked();
            });
        });
        if send { self.send(&tk, eng, ui.ctx()); }
        if !(send || cancel || r.should_close()) { self.confirm = Some(tk); }
    }
}
