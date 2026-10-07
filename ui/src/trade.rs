//! Order entry (venue raw prices only), positions / orders / log, and position insurance quotes.
//! Rule: the composite index never prices an order; everything here reads the chosen venue's book.
use super::engine::Engine;
use super::{fmt_dp, fmt_px, fmt_qty, hms, t, theme::*};
use eframe::egui::{self, Color32, RichText, Ui};
use terminal_one::agg::Agg;
use terminal_one::insure::{self, Filter, Position as InsPos};
use terminal_one::trade::{self, bbo_levels, Kind, OrderReq, Position, Tif};
use terminal_one::{now_ms, Exchange, Market, Side};


#[derive(PartialEq, Clone, Copy)]
enum OType { Limit, Market, Post }

struct Pending { ex: Exchange, req: OrderReq, ref_px: f64 }

pub struct TradeView {
    pub ex: Exchange,
    close: bool,
    otype: OType,
    price: String,
    qty: String,
    /// price clicked in a single-venue order book
    pub fill: Option<(Exchange, f64)>,
    confirm: Option<Pending>,
    tab: u8,
    ins_qty: String,
    wallet: super::wallet::WalletView,
    /// size slider position 0..=1 (share of the max openable / closable size)
    pct: f32,
    /// BBO switch on the limit price, and its book side (queue = own side) and level
    bbo_on: bool,
    /// set when the panel asks for the settings dialog (trading tab)
    pub open_settings: bool,
    /// history was requested at least once this session
    hist_requested: bool,
    /// from Settings: show the confirmation dialog before sending
    pub confirm_orders: bool,
    bbo_queue: bool,
    bbo_level: u8,
}

impl Default for TradeView {
    fn default() -> Self {
        TradeView { ex: Exchange::Bybit, close: false, otype: OType::Limit, price: String::new(), qty: String::new(), fill: None, confirm: None, tab: 0, ins_qty: "1".into(), wallet: Default::default(), pct: 0.0, open_settings: false, hist_requested: false, confirm_orders: true, bbo_on: false, bbo_queue: false, bbo_level: 1 }
    }
}

fn venue_bbo(a: &Agg, ex: Exchange) -> Option<(f64, f64)> {
    let v = a.venues.get(&(ex, Market::Perp))?;
    match (v.book.best_bid(), v.book.best_ask()) {
        (Some(b), Some(k)) => Some((b.0, k.0)),
        _ => v.bbo.map(|[b, _, k, _]| (b, k)),
    }
}

/// Bordered input: label inside on the left, right-aligned mono value, unit on the right;
/// the border turns accent while focused.
fn input_box(ui: &mut Ui, label: &str, text: &mut String, unit: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    let id = ui.id().with(("input", label));
    let focused = ui.memory(|m| m.has_focus(id));
    ui.painter().rect(rect, 4, PANEL2, egui::Stroke::new(1.0, if focused { accent() } else { LINE }), egui::StrokeKind::Inside);
    ui.painter().text(rect.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, label, prop(12.0), DIM);
    let unit_w = ui.painter().text(rect.right_center() - egui::vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, unit, prop(11.5), MU).width();
    let edit = egui::Rect::from_min_max(egui::pos2(rect.left() + 56.0, rect.top() + 4.0), egui::pos2(rect.right() - unit_w - 18.0, rect.bottom() - 4.0));
    ui.put(edit, egui::TextEdit::singleline(text).id(id).frame(egui::Frame::NONE).font(mono(13.5)).horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center))
}

/// Size slider 0..=1 with notches at quarters; returns true when dragged/clicked.
fn size_slider(ui: &mut Ui, v: &mut f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::click_and_drag());
    let track = egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.center().y - 2.0), egui::pos2(rect.right() - 6.0, rect.center().y + 2.0));
    let p = ui.painter();
    p.rect_filled(track, 2, HL);
    let x = track.left() + track.width() * v.clamp(0.0, 1.0);
    p.rect_filled(egui::Rect::from_min_max(track.min, egui::pos2(x, track.max.y)), 2, accent());
    for q in 0..=4 {
        let qx = track.left() + track.width() * q as f32 / 4.0;
        let on = *v >= q as f32 / 4.0 - 1e-4;
        let c = egui::pos2(qx, track.center().y);
        p.add(egui::Shape::convex_polygon(vec![c + egui::vec2(0.0, -5.0), c + egui::vec2(5.0, 0.0), c + egui::vec2(0.0, 5.0), c + egui::vec2(-5.0, 0.0)],
            if on { accent() } else { PANEL2 }, egui::Stroke::new(1.5, if on { accent() } else { DIM })));
    }
    p.circle(egui::pos2(x, track.center().y), 7.0, FG, egui::Stroke::new(2.0, accent()));
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.clicked()) {
        let mut f = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
        // snap to the notches when close
        for q in [0.0, 0.25, 0.5, 0.75, 1.0] { if (f - q).abs() < 0.03 { f = q; } }
        *v = f;
        return true;
    }
    false
}

fn big_button(ui: &mut Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let col = if !enabled { fill.linear_multiply(0.3) } else if resp.is_pointer_button_down_on() { fill.linear_multiply(0.8) } else if resp.hovered() { fill.gamma_multiply(1.15) } else { fill };
    ui.painter().rect_filled(rect, 4, col);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, prop(14.5), if enabled { Color32::WHITE } else { Color32::from_white_alpha(110) });
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Small read-only chip (position mode, leverage).
fn chip(ui: &mut Ui, text: &str, tip: &str) {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(11.0), MU);
    let (r, resp) = ui.allocate_exact_size(g.size() + egui::vec2(12.0, 6.0), egui::Sense::hover());
    ui.painter().rect(r, 3, PANEL2, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    ui.painter().galley(r.center() - g.size() / 2.0, g, MU);
    if !tip.is_empty() { resp.on_hover_text(tip); }
}

/// "label ........ value" row.
fn kv(ui: &mut Ui, k: &str, v: String, col: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(k).font(prop(11.0)).color(DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(v).font(mono(11.0)).color(col)); });
    });
}

/// One table cell: text + color; numbers are mono and right-aligned by their column.
struct Cell { s: String, col: Color32, mono: bool, ex: Option<Exchange> }
impl Cell {
    fn t(s: String, col: Color32) -> Self { Cell { s, col, mono: false, ex: None } }
    fn n(s: String) -> Self { Cell { s, col: FG, mono: true, ex: None } }
    fn c(s: String, col: Color32) -> Self { Cell { s, col, mono: true, ex: None } }
    /// symbol with the coin logo and a small venue logo
    fn sym(ex: Exchange, symbol: &str) -> Self { Cell { s: symbol.to_string(), col: FG, mono: false, ex: Some(ex) } }
}

fn side_cell(side: Side, pos: Option<Side>) -> Cell {
    let buy = side == Side::Buy;
    let p = match pos { Some(Side::Buy) => format!(" · {}", t("tr.long_s")), Some(Side::Sell) => format!(" · {}", t("tr.short_s")), None => String::new() };
    Cell::t(format!("{}{p}", if buy { t("tr.buy") } else { t("tr.sell") }), if buy { up() } else { dn() })
}

/// Painted table: header row, hairline separators, row hover; columns are (header key, right
/// edge as share of the width, right-aligned). With `action`, each row gets a small button at
/// the far right; returns the clicked row.
fn table(ui: &mut Ui, cols: &[(&'static str, f32, bool)], rows: Vec<Vec<Cell>>, action: Option<&str>) -> Option<usize> {
    if rows.is_empty() { ui.add_space(10.0); ui.label(RichText::new(t("acc.empty")).color(DIM)); return None; }
    let w = ui.available_width();
    let (hdr, _) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
    let left = |i: usize| if i == 0 { hdr.left() + 8.0 } else { hdr.left() + cols[i - 1].1 * w + 8.0 };
    for (i, (k, edge, right)) in cols.iter().enumerate() {
        let (x, al) = if *right { (hdr.left() + edge * w - 8.0, egui::Align2::RIGHT_CENTER) } else { (left(i), egui::Align2::LEFT_CENTER) };
        ui.painter().text(egui::pos2(x, hdr.center().y), al, t(k), prop(11.0), DIM);
    }
    ui.painter().hline(hdr.x_range(), hdr.bottom(), egui::Stroke::new(1.0, LINE));
    let mut hit = None;
    for (ri, row) in rows.iter().enumerate() {
        let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
        let p = ui.painter();
        if resp.hovered() { p.rect_filled(r, 3, HL.linear_multiply(0.6)); }
        for (i, c) in row.iter().enumerate().take(cols.len()) {
            let (_, edge, right) = cols[i];
            let f = if c.mono { mono(11.5) } else { prop(12.0) };
            let mut x = left(i);
            if let Some(ex) = c.ex {
                let base = c.s.strip_suffix("USDT").unwrap_or(&c.s);
                coin_icon(ui, base, egui::Rect::from_center_size(egui::pos2(x + 8.0, r.center().y), egui::vec2(16.0, 16.0)));
                icon(p, ex, egui::Rect::from_center_size(egui::pos2(x + 14.0, r.center().y + 5.0), egui::vec2(8.0, 8.0)), false);
                x += 22.0;
            }
            if right { p.text(egui::pos2(r.left() + edge * w - 8.0, r.center().y), egui::Align2::RIGHT_CENTER, &c.s, f, c.col); }
            else { p.text(egui::pos2(x, r.center().y), egui::Align2::LEFT_CENTER, &c.s, f, c.col); }
        }
        if let Some(a) = action {
            let b = egui::Rect::from_min_size(egui::pos2(r.right() - 60.0, r.center().y - 9.0), egui::vec2(52.0, 18.0));
            let br = ui.interact(b, ui.id().with(("row_act", ri)), egui::Sense::click());
            p.rect(b, 4, if br.hovered() { HL } else { PANEL2 }, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
            p.text(b.center(), egui::Align2::CENTER_CENTER, a, prop(11.0), FG);
            if br.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { hit = Some(ri); }
        }
        p.hline(r.x_range(), r.bottom(), egui::Stroke::new(1.0, LINE.linear_multiply(0.5)));
    }
    hit
}

impl TradeView {
    /// Order entry panel for the current base asset's USDT perp.
    pub fn panel(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str, push_lat: Option<f64>) {
        let symbol = trade::symbol(base);
        let (has_key, bal, rtt, err, pos_long, pos_short, mode, lev) = {
            let acc = eng.account.lock().unwrap();
            let here = |s: Side| acc.positions.iter().find(|p| p.ex == self.ex && p.symbol == symbol && p.side == s).cloned();
            let key = (self.ex, symbol.clone());
            (acc.keys.contains_key(&self.ex), acc.balances.get(&self.ex).cloned(), acc.order_rtt.get(&self.ex).copied(), acc.errors.get(&self.ex).cloned(),
             here(Side::Buy), here(Side::Sell), acc.modes.get(&key).copied().flatten(), acc.levs.get(&key).copied())
        };
        if has_key && mode.is_none() { eng.detect_mode(self.ex, &symbol); }
        let pos_here = pos_long.clone().or(pos_short.clone());
        if let Some((ex, px)) = self.fill.take() {
            if ex == self.ex && self.otype != OType::Market { self.price = fmt_px(px).replace(',', ""); }
        }
        ui.spacing_mut().item_spacing.y = 4.0;

        // venue + account facts
        // the venue is a setting (fixed venue / routing), shown here read-only
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            icon(ui.painter(), self.ex, r, false);
            ui.label(RichText::new(format!("{:?}", self.ex)).strong().font(prop(13.0)));
            ui.label(RichText::new(t("tr.fixed")).font(prop(10.5)).color(DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if link(ui, t("tr.change")).clicked() { self.open_settings = true; } });
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if trade::testnet() { chip(ui, t("tr.testnet"), ""); }
            match mode { Some(trade::Mode::Hedge) => chip(ui, t("tr.hedge"), t("tr.mode_tip")), Some(trade::Mode::OneWay) => chip(ui, t("tr.oneway"), t("tr.mode_tip")), None => {} }
            if let Some(l) = lev { chip(ui, &format!("{l:.0}x"), t("tr.lev_tip")); }
            let lat = |v: Option<f64>| v.map(|x| format!("{x:.0}ms")).unwrap_or("-".into());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} {}  {} {}", t("tr.push"), lat(push_lat), t("tr.rtt"), lat(rtt))).font(mono(10.0)).color(DIM));
            });
        });
        if !has_key {
            // the form stays usable for preview; only submitting needs keys
            egui::CollapsingHeader::new(RichText::new(t("tr.nokey_title")).color(WARN).font(prop(11.5))).id_salt(("nokey", self.ex as u8)).default_open(false).show(ui, |ui| {
                ui.label(RichText::new(t("tr.nokey")).color(MU).font(prop(11.0)));
                let mut hint = trade::keychain_hint(self.ex);
                ui.add(egui::TextEdit::multiline(&mut hint).font(mono(10.0)).desired_rows(3).desired_width(ui.available_width()));
                ui.label(RichText::new(t("tr.nokey2")).color(DIM).font(prop(10.5)));
            });
        }
        if let Some(e) = &err { ui.label(RichText::new(e).color(dn()).font(prop(10.5))); }

        // open / close, then order type, as text tabs
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if tab(ui, t("tr.open"), !self.close, 13.5).clicked() { self.close = false; self.pct = 0.0; }
            if tab(ui, t("tr.close"), self.close, 13.5).clicked() { self.close = true; self.pct = 0.0; }
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (o, k) in [(OType::Limit, "tr.limit"), (OType::Market, "tr.market"), (OType::Post, "tr.post")] {
                if tab(ui, t(k), self.otype == o, 12.0).clicked() { self.otype = o; }
            }
        });

        // available margin, with a shortcut to the transfer dialog
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("tr.avail")).font(prop(11.0)).color(DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if has_key && ui.add(egui::Button::new(RichText::new(t("wal.transfer")).font(prop(11.0)).color(accent())).frame(false)).clicked() {
                    self.wallet.open_for(self.ex, eng, ui.ctx());
                }
                ui.label(RichText::new(bal.as_ref().map(|b| format!("{} USDT", fmt_dp(b.available, 2))).unwrap_or("-".into())).font(mono(11.5)).color(FG));
            });
        });

        let bbo = venue_bbo(a, self.ex);
        // like the venues: an empty limit price starts at the venue's own best bid
        let typed_price = matches!(self.otype, OType::Limit | OType::Post);
        if self.price.is_empty() && typed_price {
            if let Some((b, _)) = bbo { self.price = fmt_px(b).replace(',', ""); }
        }
        if !bbo_levels(self.ex).contains(&self.bbo_level) { self.bbo_level = 1; }
        // Binance-style BBO: a switch beside the limit price; when on, the price box becomes a
        // choice of book side and level and the venue prices the order when it arrives
        let use_bbo = self.otype == OType::Limit && self.bbo_on;
        if typed_price {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = ui.available_width() - 52.0;
                ui.allocate_ui_with_layout(egui::vec2(w, 28.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    if use_bbo {
                        let name = |q: bool, l: u8| format!("{} {l}", t(if q { "tr.bbo_queue" } else { "tr.bbo_opp" }));
                        ui.spacing_mut().interact_size.y = 28.0;
                        egui::ComboBox::from_id_salt("bbo_pick").icon(chevron).width(w).selected_text(RichText::new(name(self.bbo_queue, self.bbo_level)).font(prop(12.5)))
                            .show_ui(ui, |ui| {
                                for q in [false, true] {
                                    for l in bbo_levels(self.ex) {
                                        let on = self.bbo_queue == q && self.bbo_level == *l;
                                        if ui.selectable_label(on, name(q, *l)).on_hover_text(t(if q { "tr.bbo_queue_tip" } else { "tr.bbo_opp_tip" })).clicked() { self.bbo_queue = q; self.bbo_level = *l; }
                                    }
                                }
                            });
                    } else {
                        input_box(ui, t("tr.price"), &mut self.price, "USDT");
                    }
                });
                if self.otype == OType::Limit {
                    let on = self.bbo_on;
                    let b = egui::Button::new(RichText::new("BBO").font(prop(11.5)).color(if on { accent() } else { MU })).fill(if on { HL } else { PANEL2 })
                        .stroke(egui::Stroke::new(1.0, if on { accent() } else { LINE })).corner_radius(4).min_size(egui::vec2(46.0, 28.0));
                    if ui.add(b).on_hover_text(t("tr.bbo_tip")).clicked() { self.bbo_on = !on; }
                }
            });
            if let Some((b, k)) = bbo {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (lab, v, col) in [(t("tr.bid"), b, up()), (t("tr.ask"), k, dn())] {
                        let r = ui.add(egui::Button::new(RichText::new(format!("{lab} {}", fmt_px(v))).font(mono(10.5)).color(col)).fill(PANEL2).corner_radius(3));
                        if r.clicked() && !use_bbo { self.price = fmt_px(v).replace(',', ""); }
                    }
                });
            }
        } else {
            let mut m = t("tr.market_px").to_string();
            ui.add_enabled_ui(false, |ui| input_box(ui, t("tr.price"), &mut m, "USDT"));
        }
        let ref_px = match (self.otype, bbo) { (OType::Market, Some((b, k))) => (b + k) / 2.0, (_, Some((b, k))) if use_bbo => (b + k) / 2.0, _ => self.price.parse().unwrap_or(bbo.map(|(b, k)| (b + k) / 2.0).unwrap_or(0.0)) };
        let lev_v = lev.unwrap_or(1.0).max(1.0);
        // what 100% means: the larger position when closing, else available margin x leverage
        let max_qty = if self.close { pos_long.iter().chain(pos_short.iter()).map(|p| p.qty).fold(0.0, f64::max) }
            else { bal.as_ref().map_or(0.0, |b| b.available * lev_v / ref_px.max(1e-9)) };
        if input_box(ui, t("tr.qty"), &mut self.qty, base).changed() {
            self.pct = if max_qty > 0.0 { (self.qty.parse::<f64>().unwrap_or(0.0) / max_qty).clamp(0.0, 1.0) as f32 } else { 0.0 };
        }
        if size_slider(ui, &mut self.pct) { self.qty = if self.pct > 0.0 { format!("{:.6}", max_qty * self.pct as f64) } else { String::new() }; }

        let qty: f64 = self.qty.parse().unwrap_or(0.0);
        let notional = qty * ref_px;
        let (taker, maker) = super::settings::fee(self.ex);
        let fee = notional * if self.otype == OType::Market { taker } else { maker };
        let price: f64 = self.price.parse().unwrap_or(0.0);
        let valid = has_key && qty > 0.0 && (!typed_price || use_bbo || price > 0.0) && ref_px > 0.0;
        let kind = match self.otype {
            OType::Market => Kind::Market,
            OType::Limit if use_bbo => Kind::Bbo { queue: self.bbo_queue, level: self.bbo_level },
            OType::Limit => Kind::Limit { price, tif: Tif::Gtc },
            OType::Post => Kind::Limit { price, tif: Tif::PostOnly },
        };
        // open: long / short; close: the buy button closes the short, the sell button the long
        let (l1, l2, p1, p2) = if self.close { (t("tr.close_short"), t("tr.close_long"), Side::Sell, Side::Buy) } else { (t("tr.long"), t("tr.short"), Side::Buy, Side::Sell) };
        let close = self.close;
        ui.add_space(2.0);
        ui.columns(2, |c| {
            if big_button(&mut c[0], l1, up(), valid) { self.confirm = Some(Pending { ex: self.ex, req: OrderReq { symbol: symbol.clone(), pos: p1, close, kind, qty, client_id: None }, ref_px }); }
            if big_button(&mut c[1], l2, dn(), valid) { self.confirm = Some(Pending { ex: self.ex, req: OrderReq { symbol: symbol.clone(), pos: p2, close, kind, qty, client_id: None }, ref_px }); }
            // per side: what can be opened / closed, and the margin this order costs
            for (i, side) in [(0, p1), (1, p2)] {
                let can = if close { pos_long.iter().chain(pos_short.iter()).find(|p| p.side == side).map_or(0.0, |p| p.qty) } else { max_qty };
                let cu = &mut c[i];
                kv(cu, if close { t("tr.can_close") } else { t("tr.can_open") }, format!("{} {base}", fmt_qty(can)), MU);
                if !close { kv(cu, t("tr.cost"), format!("{} USDT", fmt_dp(notional / lev_v, 2)), MU); }
            }
        });
        kv(ui, t("tr.notional"), format!("{} USDT", fmt_dp(notional, 2)), MU);
        kv(ui, t("tr.fee"), format!("≈ {:.2} USDT", fee), MU);
        if lev.is_none() && has_key { ui.label(RichText::new(t("tr.lev_unknown")).font(prop(10.5)).color(WARN)); }

        // this symbol's positions on this venue
        for p in pos_long.iter().chain(pos_short.iter()) {
            ui.add_space(4.0);
            egui::Frame::new().fill(PANEL2).corner_radius(4).inner_margin(8).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let (sc, sl) = if p.side == Side::Buy { (up(), t("tr.long_s")) } else { (dn(), t("tr.short_s")) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{sl} {}", fmt_qty(p.qty))).font(mono(12.0)).color(sc).strong());
                    if p.lev > 0.0 { ui.label(RichText::new(format!("{:.0}x", p.lev)).font(mono(10.5)).color(DIM)); }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if link(ui, t("acc.close_mkt")).clicked() {
                            self.confirm = Some(Pending { ex: p.ex, ref_px: p.mark, req: OrderReq { symbol: p.symbol.clone(), pos: p.side, close: true, kind: Kind::Market, qty: p.qty, client_id: None } });
                        }
                        ui.label(RichText::new(format!("{:+.2}", p.upnl)).font(mono(12.0)).color(if p.upnl >= 0.0 { up() } else { dn() }));
                    });
                });
                ui.columns(3, |c| {
                    for (i, (k, v)) in [(t("acc.entry"), fmt_px(p.entry)), (t("acc.mark"), fmt_px(p.mark)), (t("acc.liq"), p.liq.map(fmt_px).unwrap_or("-".into()))].into_iter().enumerate() {
                        c[i].label(RichText::new(k).font(prop(10.0)).color(DIM));
                        c[i].label(RichText::new(v).font(mono(11.0)).color(if i == 2 { WARN } else { FG }));
                    }
                });
            });
        }
        self.confirm_modal(ui, eng);

        ui.add_space(8.0);
        ui.separator();
        // collapsed by default: opening it is what subscribes the option chains
        egui::CollapsingHeader::new(RichText::new(t("ins.title")).strong()).id_salt("insurance").default_open(false)
            .show(ui, |ui| self.insurance(ui, a, eng, base, pos_here));
    }

    fn confirm_modal(&mut self, ui: &mut Ui, eng: &Engine) {
        if !self.confirm_orders {
            if let Some(p) = self.confirm.take() { eng.submit(p.ex, p.req, p.ref_px, ui.ctx()); }
            return;
        }
        let Some(p) = &self.confirm else { return };
        let mut done = None;
        let m = egui::Modal::new(egui::Id::new("confirm_order")).show(ui.ctx(), |ui| {
            ui.set_width(320.0);
            let buy = p.req.side() == Side::Buy;
            let action = match (p.req.pos, p.req.close) { (Side::Buy, false) => t("tr.long"), (Side::Sell, false) => t("tr.short"), (Side::Buy, true) => t("tr.close_long"), (Side::Sell, true) => t("tr.close_short") };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{:?}", p.ex)).strong());
                ui.label(RichText::new(action).strong().color(if buy { up() } else { dn() }));
                ui.label(&p.req.symbol);
            });
            ui.separator();
            let px = match p.req.kind {
                Kind::Market => t("tr.market").to_string(),
                Kind::Limit { price, tif } => format!("{} {}", fmt_px(price), if tif == Tif::PostOnly { t("tr.post") } else { "" }),
                Kind::Bbo { queue, level } => format!("BBO · {} · {level}", t(if queue { "tr.bbo_queue" } else { "tr.bbo_opp" })),
            };
            egui::Grid::new("cf").num_columns(2).show(ui, |ui| {
                ui.label(RichText::new(t("tr.price")).color(MU)); ui.label(RichText::new(px).font(mono(12.0))); ui.end_row();
                ui.label(RichText::new(t("tr.qty")).color(MU)); ui.label(RichText::new(format!("{}", p.req.qty)).font(mono(12.0))); ui.end_row();
                ui.label(RichText::new(t("tr.notional")).color(MU)); ui.label(RichText::new(format!("{} USDT", fmt_dp(p.req.qty * p.ref_px, 2))).font(mono(12.0))); ui.end_row();
            });
            ui.label(RichText::new(t("tr.rounding")).color(DIM).font(prop(10.5)));
            if !trade::VERIFIED.contains(&p.ex) {
                ui.add_space(4.0);
                ui.label(RichText::new(t("tr.untested_warn")).color(WARN).font(prop(11.0)));
            }
            ui.add_space(6.0);
            ui.columns(2, |c| {
                if c[0].add(egui::Button::new(t("tr.cancel")).min_size(egui::vec2(c[0].available_width(), 30.0))).clicked() { done = Some(false); }
                if big_button(&mut c[1], t("tr.confirm"), if buy { up() } else { dn() }, true) { done = Some(true); }
            });
        });
        if m.should_close() && done.is_none() { done = Some(false); }
        if let Some(go) = done {
            let p = self.confirm.take().unwrap();
            if go { eng.submit(p.ex, p.req, p.ref_px, ui.ctx()); }
        }
    }

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
            ui.label(RichText::new(t("ins.loading")).color(DIM).font(prop(11.0)));
            eng.wants_options.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if ip.qty <= 0.0 || mark <= 0.0 { return; }
        let f = Filter { min_days: 2.0, max_days: 60.0, otm_max: 0.12 };
        // OKX option prices are already USD; the other venues quote USDT
        let all = insure::quotes(&a.chains, |e| if e == Exchange::Okx { 1.0 } else { a.usd(e, Market::Perp) }, &ip, &f, now_ms());
        let mut best = insure::best_per_contract(&all);
        best.retain(|q| q.floor_dist >= 0.02);
        best.sort_by(|x, y| x.cost_apr.total_cmp(&y.cost_apr));
        ui.label(RichText::new(format!("{} {} {}  {}", if ip.side == Side::Buy { t("tr.long_s") } else { t("tr.short_s") }, fmt_qty(ip.qty), base, t("ins.hint"))).color(DIM).font(prop(10.5)));
        egui::Grid::new("ins").striped(true).spacing([8.0, 3.0]).show(ui, |ui| {
            // max loss lives in the hover so the table fits the 300px panel
            for k in ["ins.venue", "ins.exp", "ins.floor", "ins.cost"] { ui.label(RichText::new(t(k)).color(DIM).font(prop(10.5))); }
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
        ui.label(RichText::new(t("ins.exec_note")).color(DIM).font(prop(10.0)));
    }

    pub fn set_tab(&mut self, n: u8) { self.tab = n; }

    /// Order / trade / position history (tabs 2..=4), from the engine's on-demand cache.
    fn history_tab(&mut self, ui: &mut Ui, eng: &Engine) {
        let (hist, loading) = {
            let a = eng.account.lock().unwrap();
            let mut h = trade::History::default();
            let mut at = i64::MAX;
            for (_, (x, ts)) in a.history.iter() {
                h.orders.extend(x.orders.iter().cloned());
                h.fills.extend(x.fills.iter().cloned());
                h.closed.extend(x.closed.iter().cloned());
                at = at.min(*ts);
            }
            h.orders.sort_by(|a, b| b.ts.cmp(&a.ts));
            h.fills.sort_by(|a, b| b.ts.cmp(&a.ts));
            h.closed.sort_by(|a, b| b.ts.cmp(&a.ts));
            ((h, (at != i64::MAX).then_some(at)), !a.history_loading.is_empty())
        };
        let (h, at) = hist;
        if !std::mem::replace(&mut self.hist_requested, true) && at.is_none() && !loading { eng.load_history(ui.ctx()); }
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("acc.hist_note")).font(prop(10.5)).color(DIM));
            if let Some(at) = at { ui.label(RichText::new(format!("· {} {}s", t("wal.updated"), (now_ms() - at) / 1000)).font(prop(10.5)).color(DIM)); }
            if loading { ui.label(RichText::new(t("wal.loading")).font(prop(10.5)).color(DIM)); }
            else if link(ui, t("wal.refresh")).clicked() { eng.load_history(ui.ctx()); }
        });
        let d = |ts: i64| format!("{} {}", super::chart_date(ts), super::hms(ts));
        match self.tab {
            2 => {
                let rows = h.orders.iter().map(|o| vec![
                    Cell::t(d(o.ts), MU), Cell::sym(o.ex, &o.symbol), side_cell(o.side, None), Cell::t(o.kind.clone(), FG), Cell::n(if o.price > 0.0 { fmt_px(o.price) } else { "-".into() }),
                    Cell::n(if o.avg > 0.0 { fmt_px(o.avg) } else { "-".into() }), Cell::n(format!("{} / {}", fmt_qty(o.filled), fmt_qty(o.qty))), Cell::t(o.status.clone(), MU),
                ]).collect();
                table(ui, &[("acc.time", 0.11, false), ("acc.symbol", 0.28, false), ("acc.side", 0.36, false), ("acc.type", 0.46, false), ("tr.price", 0.58, true), ("acc.avg", 0.69, true), ("acc.filled", 0.84, true), ("acc.status", 0.97, true)], rows, None);
            }
            3 => {
                let rows = h.fills.iter().map(|f| vec![
                    Cell::t(d(f.ts), MU), Cell::sym(f.ex, &f.symbol), side_cell(f.side, None), Cell::n(fmt_px(f.price)), Cell::n(fmt_qty(f.qty)),
                    Cell::n(fmt_dp(f.price * f.qty, 2)), Cell::n(format!("{:.4}", f.fee)),
                    f.realized.filter(|r| *r != 0.0).map(|r| Cell::c(format!("{r:+.2}"), if r >= 0.0 { up() } else { dn() })).unwrap_or(Cell::n("-".into())),
                ]).collect();
                table(ui, &[("acc.time", 0.11, false), ("acc.symbol", 0.28, false), ("acc.side", 0.36, false), ("tr.price", 0.48, true), ("acc.qty", 0.59, true), ("tr.notional", 0.71, true), ("tr.fee", 0.82, true), ("acc.realized", 0.97, true)], rows, None);
            }
            _ => {
                let rows = h.closed.iter().map(|c| vec![
                    Cell::t(d(c.ts), MU), Cell::sym(c.ex, &c.symbol),
                    match c.long { Some(l) => Cell::c(if l { t("tr.long_s") } else { t("tr.short_s") }.into(), if l { up() } else { dn() }), None => Cell::t("-".into(), DIM) },
                    Cell::n(c.qty.map(fmt_qty).unwrap_or("-".into())), Cell::n(c.entry.map(fmt_px).unwrap_or("-".into())), Cell::n(c.exit.map(fmt_px).unwrap_or("-".into())),
                    Cell::c(format!("{:+.2}", c.pnl), if c.pnl >= 0.0 { up() } else { dn() }),
                ]).collect();
                table(ui, &[("acc.time", 0.11, false), ("acc.symbol", 0.30, false), ("acc.side", 0.40, false), ("acc.qty", 0.54, true), ("acc.entry", 0.68, true), ("acc.exit", 0.82, true), ("acc.pnl_real", 0.97, true)], rows, None);
            }
        }
    }
    pub fn reset_price(&mut self) { self.price.clear(); }

    /// Positions as a painted table: two-line cells, numbers right-aligned, row hover,
    /// market close and limit close (prefills the order panel) per row.
    fn positions_table(&mut self, ui: &mut Ui, positions: &[Position], balances: &std::collections::HashMap<Exchange, trade::Balance>) {
        if positions.is_empty() { ui.add_space(12.0); ui.label(RichText::new(t("acc.no_positions")).color(DIM)); return; }
        // summary strip: totals across venues
        let upnl: f64 = positions.iter().map(|p| p.upnl).sum();
        let margin: f64 = positions.iter().map(|p| p.margin).sum();
        let value: f64 = positions.iter().map(|p| p.qty * p.mark).sum();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (k, v, c) in [(t("acc.total_upnl"), format!("{upnl:+.2} USDT"), if upnl >= 0.0 { up() } else { dn() }), (t("acc.total_margin"), format!("{} USDT", fmt_dp(margin, 2)), FG),
                              (t("acc.total_value"), format!("{} USDT", fmt_dp(value, 0)), FG)] {
                ui.label(RichText::new(k).font(prop(11.0)).color(DIM));
                ui.label(RichText::new(v).font(mono(11.5)).color(c));
                ui.add_space(12.0);
            }
        });
        // column right edges as shares of the width (the symbol column is left-aligned)
        const COLS: [(&str, f32); 8] = [("acc.symbol", 0.0), ("acc.qty", 0.30), ("acc.entry", 0.40), ("acc.mark", 0.50), ("acc.liq", 0.60), ("acc.margin", 0.72), ("acc.pnl", 0.86), ("", 1.0)];
        let w = ui.available_width();
        let (hdr, _) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
        let p = ui.painter();
        let colx = |i: usize| hdr.left() + COLS[i].1 * w - 8.0;
        for (i, (k, _)) in COLS.iter().enumerate() {
            if k.is_empty() { continue; }
            let (pos, al) = if i == 0 { (egui::pos2(hdr.left() + 8.0, hdr.center().y), egui::Align2::LEFT_CENTER) } else { (egui::pos2(colx(i), hdr.center().y), egui::Align2::RIGHT_CENTER) };
            p.text(pos, al, t(k), prop(11.0), DIM);
        }
        p.hline(hdr.x_range(), hdr.bottom(), egui::Stroke::new(1.0, LINE));
        for ps in positions {
            let (row, resp) = ui.allocate_exact_size(egui::vec2(w, 36.0), egui::Sense::hover());
            let p = ui.painter();
            if resp.hovered() { p.rect_filled(row, 4, HL.linear_multiply(0.6)); }
            let (y1, y2) = (row.center().y - 7.0, row.center().y + 8.0);
            let colx = |i: usize| row.left() + COLS[i].1 * w - 8.0;
            let num = |i: usize, y: f32, s: String, col: Color32, f: egui::FontId| { p.text(egui::pos2(colx(i), y), egui::Align2::RIGHT_CENTER, s, f, col); };
            let long = ps.side == Side::Buy;
            let sc = if long { up() } else { dn() };
            let base = ps.symbol.strip_suffix("USDT").unwrap_or(&ps.symbol);
            // symbol: coin logo, name, side/leverage badge; venue underneath
            coin_icon(ui, base, egui::Rect::from_center_size(egui::pos2(row.left() + 18.0, row.center().y), egui::vec2(20.0, 20.0)));
            let nr = p.text(egui::pos2(row.left() + 38.0, y1), egui::Align2::LEFT_CENTER, &ps.symbol, prop(12.5), FG);
            let badge = format!("{} {}", if long { t("tr.long_s") } else { t("tr.short_s") }, if ps.lev > 0.0 { format!("{:.0}x", ps.lev) } else { String::new() });
            let g = p.layout_no_wrap(badge, prop(10.5), sc);
            let br = egui::Rect::from_min_size(egui::pos2(nr.right() + 6.0, y1 - 8.0), g.size() + egui::vec2(10.0, 4.0));
            p.rect_filled(br, 3, sc.linear_multiply(0.15));
            p.galley(br.center() - g.size() / 2.0, g, sc);
            icon(p, ps.ex, egui::Rect::from_center_size(egui::pos2(row.left() + 44.0, y2), egui::vec2(11.0, 11.0)), false);
            p.text(egui::pos2(row.left() + 53.0, y2), egui::Align2::LEFT_CENTER, format!("{:?} · Perp", ps.ex), prop(10.5), DIM);
            // numbers
            num(1, y1, format!("{} {base}", fmt_qty(ps.qty)), FG, mono(12.0));
            num(1, y2, format!("≈{} USDT", fmt_dp(ps.qty * ps.mark, 0)), DIM, mono(10.5));
            num(2, row.center().y, fmt_px(ps.entry), FG, mono(12.0));
            num(3, row.center().y, fmt_px(ps.mark), FG, mono(12.0));
            let liq_txt = ps.liq.map(fmt_px).unwrap_or("—".into());
            num(4, row.center().y, liq_txt, if ps.liq.is_some() { WARN } else { DIM }, mono(12.0));
            let liq_rect = egui::Rect::from_min_max(egui::pos2(colx(3) + 8.0, row.top()), egui::pos2(colx(4) + 8.0, row.bottom()));
            if ps.liq.is_none() && balances.get(&ps.ex).is_some_and(|b| b.uni_mmr.is_some()) {
                ui.interact(liq_rect, ui.id().with(("liq", &ps.symbol, long)), egui::Sense::hover()).on_hover_text(t("acc.liq_pm"));
            }
            num(5, y1, fmt_dp(ps.margin, 2), FG, mono(12.0));
            num(5, y2, "USDT".into(), DIM, prop(10.5));
            let pc = if ps.upnl >= 0.0 { up() } else { dn() };
            num(6, y1, format!("{:+.2}", ps.upnl), pc, mono(12.0));
            if ps.margin > 0.0 { num(6, y2, format!("{:+.2}%", ps.upnl / ps.margin * 100.0), pc, mono(10.5)); }
            // actions
            let ax = colx(7);
            let mk = egui::Rect::from_min_size(egui::pos2(ax - 104.0, row.center().y - 10.0), egui::vec2(50.0, 20.0));
            let lm = egui::Rect::from_min_size(egui::pos2(ax - 50.0, row.center().y - 10.0), egui::vec2(50.0, 20.0));
            for (r, k, id) in [(mk, "acc.close_mkt_s", 0), (lm, "acc.close_lmt_s", 1)] {
                let resp = ui.interact(r, ui.id().with(("act", &ps.symbol, long, id)), egui::Sense::click());
                p.rect(r, 4, if resp.hovered() { HL } else { PANEL2 }, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
                p.text(r.center(), egui::Align2::CENTER_CENTER, t(k), prop(11.0), FG);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    if id == 0 {
                        self.confirm = Some(Pending { ex: ps.ex, ref_px: ps.mark, req: OrderReq { symbol: ps.symbol.clone(), pos: ps.side, close: true, kind: Kind::Market, qty: ps.qty, client_id: None } });
                    } else {
                        // limit close: hand it to the order panel (venue, close mode, size, mark as price)
                        self.ex = ps.ex;
                        self.close = true;
                        self.otype = OType::Limit;
                        self.bbo_on = false;
                        self.price = fmt_px(ps.mark).replace(',', "");
                        self.qty = format!("{}", ps.qty);
                    }
                }
            }
            p.hline(row.x_range(), row.bottom(), egui::Stroke::new(1.0, LINE.linear_multiply(0.6)));
        }
    }

    /// Positions, open orders and the order log across venues.
    pub fn account(&mut self, ui: &mut Ui, eng: &Engine) {
        let (mut positions, mut orders, log, any_key, balances) = {
            let acc = eng.account.lock().unwrap();
            (acc.positions.clone(), acc.orders.clone(), acc.log.iter().rev().take(100).cloned().collect::<Vec<_>>(), !acc.keys.is_empty(), acc.balances.clone())
        };
        // stable order: pushes reorder the underlying vectors, the table must not jump
        positions.sort_by(|a, b| (a.symbol.as_str(), format!("{:?}", a.ex), a.side as u8).cmp(&(b.symbol.as_str(), format!("{:?}", b.ex), b.side as u8)));
        orders.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.id.cmp(&b.id)));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let labels = [format!("{} ({})", t("acc.positions"), positions.len()), format!("{} ({})", t("acc.orders"), orders.len()),
                t("acc.order_hist").to_string(), t("acc.trade_hist").to_string(), t("acc.pos_hist").to_string(), t("acc.log").to_string(), t("acc.assets").to_string()];
            for (i, l) in labels.into_iter().enumerate() {
                if tab(ui, &l, self.tab == i as u8, 12.5).clicked() {
                    // history tabs fetch when opened (never on a timer)
                    if (2..=4).contains(&i) && self.tab != i as u8 { self.hist_requested = true; eng.load_history(ui.ctx()); }
                    self.tab = i as u8;
                }
            }
            if !any_key { ui.add_space(16.0); ui.label(RichText::new(t("acc.nokey")).font(prop(11.5)).color(DIM)); }
            // account-level risk: what actually triggers liquidation on unified accounts
            ui.add_space(24.0);
            let mut bs: Vec<_> = balances.iter().collect();
            bs.sort_by_key(|(e, _)| format!("{e:?}"));
            for (ex, b) in bs {
                if let Some(m) = b.uni_mmr {
                    let col = if m > 1.5 { up() } else if m > 1.2 { WARN } else { dn() };
                    ui.label(RichText::new(format!("  {ex:?} uniMMR ")).font(prop(11.0)).color(DIM));
                    ui.label(RichText::new(format!("{m:.2}")).font(mono(11.5)).color(col)).on_hover_text(t("acc.unimmr_tip"));
                }
                if let Some(r) = b.mm_rate {
                    let col = if r < 0.5 { up() } else if r < 0.8 { WARN } else { dn() };
                    ui.label(RichText::new(format!("  {ex:?} {} ", t("acc.mmr"))).font(prop(11.0)).color(DIM));
                    ui.label(RichText::new(format!("{:.1}%", r * 100.0)).font(mono(11.5)).color(col)).on_hover_text(t("acc.mmr_tip"));
                }
            }
        });
        if !any_key { return; }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            0 => self.positions_table(ui, &positions, &balances),
            1 => {
                let rows: Vec<Vec<Cell>> = orders.iter().map(|o| vec![
                    Cell::t(super::hms(o.ts), MU), Cell::sym(o.ex, &o.symbol), side_cell(o.side, o.pos), Cell::t(format!("{}{}", o.kind, if o.reduce_only { " · R" } else { "" }), FG),
                    Cell::n(fmt_px(o.price)), Cell::n(fmt_qty(o.qty)), Cell::n(fmt_qty(o.filled)),
                ]).collect();
                let cancel = table(ui, &[("acc.time", 0.08, false), ("acc.symbol", 0.26, false), ("acc.side", 0.36, false), ("acc.type", 0.48, false), ("tr.price", 0.62, true), ("acc.qty", 0.74, true), ("acc.filled", 0.86, true)], rows, Some(t("acc.cancel")));
                if let Some(i) = cancel { eng.cancel(&orders[i], ui.ctx()); }
            }
            2..=4 => self.history_tab(ui, eng),
            5 => {
                for (ts, msg, ok) in &log {
                    ui.label(RichText::new(format!("{}  {msg}", hms(*ts))).font(mono(11.0)).color(if *ok { MU } else { dn() }));
                }
            }
            6 => self.wallet.show(ui, eng),
            _ => {}
        });
        self.confirm_modal(ui, eng);
        self.wallet.modal(ui, eng);
    }
}
