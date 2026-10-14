//! Order entry for the egui app: a port of the SwiftUI host's OrderPanel, OrderConfirm,
//! LeverageSheet and OrderTpSl. It reads the same state document (`App::state_json`) and sends the
//! same ops (`App::call`), so routing, validation and placement are identical in both hosts.
//! Prices always come from the trade venue's own book (`trade.bid/ask`), never the composite.
use super::kit::{self, Look, Tone, H, H_LG, H_SM};
use super::{fmt_dp, fmt_px, fmt_qty, l, lf, theme::*};
use eframe::egui::{self, Align2, Color32, Rect, Response, RichText, Sense, Stroke, Ui};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Sends one op to the app (`App::call`) and returns its reply.
pub type Call<'a> = dyn FnMut(Value) -> Result<Value, String> + 'a;

#[derive(Clone, Copy, PartialEq)]
enum Kind { Limit, Market, Stop, Trailing, Scaled, Twap }

impl Kind {
    fn arg(self) -> &'static str {
        match self { Kind::Limit => "limit", Kind::Market => "market", Kind::Stop => "stop", Kind::Trailing => "trailing", Kind::Scaled => "scaled", Kind::Twap => "twap" }
    }
}

/// A previewed order: the exact preview args (re-sent to `place`) and the route plan.
struct Ticket { pos: String, close: bool, args: Value, plan: Value, tp: Option<f64>, sl: Option<f64>, trigger: &'static str, err: Option<String> }

impl Ticket {
    /// buy = open long or close short
    fn buy(&self) -> bool { (self.pos == "long") != self.close }
    fn title(&self) -> &'static str { title(&self.pos, self.close) }
}

fn title(pos: &str, close: bool) -> &'static str {
    match (pos, close) { ("long", false) => l("Open Long"), ("short", false) => l("Open Short"), ("long", true) => l("Close Long"), _ => l("Close Short") }
}

/// A scaled / TWAP request waiting for confirmation.
struct Algo { op: &'static str, args: Value, title: String, message: String }

/// TP/SL attached to an opening order: set for the whole position once that venue's position grew
/// past its size at placement (or given up after two minutes without a resting order).
struct Waiting { ex: String, symbol: String, pos: String, base_qty: f64, tp: Option<f64>, sl: Option<f64>, trigger: &'static str, until: Instant }

/// One staged TP/SL level of the position sheet; `pct` = amount is % of the position.
struct Level { tp: bool, price: String, amount: String, pct: bool }

/// The TP/SL sheet of one position (whole position or staged levels); shared with the account strip.
pub(crate) struct TpSlSheet { ex: String, symbol: String, side: String, partial: bool, tp: String, sl: String, last: bool, levels: Vec<Level>, err: Option<String> }

pub struct OrderPanel {
    close: bool,
    kind: Kind,
    price: String,
    qty: String,
    pct: f32,
    bbo_on: bool,
    bbo_queue: bool,
    bbo_level: u64,
    tpsl_on: bool,
    tp: String,
    sl: String,
    trig_last: bool,
    /// side of the last Buy / Sell click: Enter in a field repeats it
    last_pos: Option<String>,
    error: Option<String>,
    ticket: Option<Ticket>,
    algo: Option<Algo>,
    /// limit time in force: gtc, ioc, fok, post
    tif: &'static str,
    trigger: String,
    trigger_last: bool,
    stop_limit: bool,
    callback: String,
    activation: String,
    scaled_from: String,
    scaled_to: String,
    scaled_count: u32,
    scaled_skew: f64,
    scaled_post: bool,
    twap_minutes: String,
    twap_slices: String,
    twap_slip: String,
    twap_limit: String,
    twap_native: bool,
    twap_random: bool,
    /// leverage sheet open, with the value being chosen
    lev: Option<f64>,
    waiting: Vec<Waiting>,
    sheet: Option<TpSlSheet>,
    /// last symbol / venue / book click seen, to react to changes
    seen: (String, String, Option<(String, f64)>),
    /// set when the panel wants the settings dialog: 2 = trading, 3 = API keys
    pub open_settings: Option<u8>,
    /// set when the panel wants the transfer dialog for this venue
    pub open_transfer: Option<String>,
}

impl Default for OrderPanel {
    fn default() -> Self {
        OrderPanel {
            close: false, kind: Kind::Limit, price: String::new(), qty: String::new(), pct: 0.0, bbo_on: false, bbo_queue: false, bbo_level: 1,
            tpsl_on: false, tp: String::new(), sl: String::new(), trig_last: false, last_pos: None, error: None, ticket: None, algo: None, tif: "gtc",
            trigger: String::new(), trigger_last: false, stop_limit: false, callback: "1.0".into(), activation: String::new(),
            scaled_from: String::new(), scaled_to: String::new(), scaled_count: 5, scaled_skew: 0.0, scaled_post: true,
            twap_minutes: "15".into(), twap_slices: "10".into(), twap_slip: "10".into(), twap_limit: String::new(), twap_native: true, twap_random: false,
            lev: None, waiting: vec![], sheet: None, seen: Default::default(), open_settings: None, open_transfer: None,
        }
    }
}

// ---- number input (same rules as the Swift OrderNum) ----

/// "." decimal; commas only as thousands groups ("1,234.5"): a decimal comma ("0,5") is refused.
pub fn parse(s: &str) -> Option<f64> {
    let t = s.trim();
    let t = if t.contains(',') {
        let (int, frac) = t.split_once('.').map_or((t, None), |(a, b)| (a, Some(b)));
        let groups: Vec<&str> = int.trim_start_matches(['+', '-']).split(',').collect();
        let ok = groups.len() > 1 && (1..=3).contains(&groups[0].len()) && groups[0].chars().all(|c| c.is_ascii_digit())
            && groups[1..].iter().all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit())) && frac.is_none_or(|f| f.chars().all(|c| c.is_ascii_digit()));
        if !ok { return None; }
        t.replace(',', "")
    } else { t.to_string() };
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn decimals(step: Option<f64>) -> Option<usize> { step.filter(|s| *s > 0.0).map(|s| (-s.log10() - 1e-9).ceil().max(0.0) as usize) }

/// price for an input box: tick decimals when the venue rules are known
fn px_str(v: f64, tick: Option<f64>) -> String {
    let a = v.abs();
    let d = decimals(tick).unwrap_or(if a >= 10_000.0 { 1 } else if a >= 100.0 { 2 } else if a >= 1.0 { 4 } else { 6 });
    format!("{v:.d$}")
}

/// size floored to the venue's step (never rounds up past the max)
fn qty_str(v: f64, step: Option<f64>) -> String {
    if let (Some(s), Some(d)) = (step.filter(|s| *s > 0.0), decimals(step)) { return format!("{:.d$}", ((v / s) + 1e-9).floor() * s); }
    let mut out = format!("{:.6}", (v * 1e6).floor() / 1e6);
    while out.contains('.') && (out.ends_with('0') || out.ends_with('.')) { out.pop(); }
    out
}

pub(crate) fn usd(v: f64) -> String { fmt_dp(v, 2) }
pub(crate) fn signed(v: f64) -> String { format!("{}{}", if v >= 0.0 { "+" } else { "" }, fmt_dp(v, 2)) }
fn f(v: &Value) -> Option<f64> { v.as_f64() }
fn s(v: &Value) -> &str { v.as_str().unwrap_or_default() }

/// TP above / SL below the reference for longs, the reverse for shorts.
fn tpsl_check(long: bool, r: f64, ref_name: &str, tp: Option<f64>, sl: Option<f64>) -> Option<String> {
    if let Some(tp) = tp { if tp <= 0.0 || (if long { tp <= r } else { tp >= r }) { return Some(format!("{} {ref_name} ({})", if long { l("Take-profit must be above") } else { l("Take-profit must be below") }, fmt_px(r))); } }
    if let Some(sl) = sl { if sl <= 0.0 || (if long { sl >= r } else { sl <= r }) { return Some(format!("{} {ref_name} ({})", if long { l("Stop-loss must be below") } else { l("Stop-loss must be above") }, fmt_px(r))); } }
    None
}

/// Plan leg kind ("market", "limit 123.4", "post 1", "ioc 2", "bbo queue 5") as UI text.
fn kind_text(k: &str) -> String {
    let p: Vec<&str> = k.split(' ').collect();
    let px = p.get(1).and_then(|x| x.parse().ok()).map(fmt_px).unwrap_or_default();
    match p[0] {
        "market" => l("Market").into(),
        "limit" => format!("{} {px}", l("Limit")),
        "post" => format!("{} {px}", l("Post Only")),
        "ioc" => format!("{} {px}", l("IOC limit")),
        "bbo" => format!("BBO {} {}", if p.get(1) == Some(&"queue") { l("Queue") } else { l("Counterparty") }, p.get(2).unwrap_or(&"")),
        _ => k.into(),
    }
}

// ---- widgets: everything is kit.rs; the few primitives the kit has no equivalent for live here ----

/// Account card helpers kept for account.rs (it imports them from here); new code calls kit directly.
pub(crate) fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    kit::panel_frame(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        add(ui)
    }).inner
}

/// Slider 0..=1 (Components spec: 2 px track, diamond notches at quarters, 12 px white knob with an
/// accent ring), `right` printed after it in 12 grey numbers; snaps near the notches. True when moved.
fn slider(ui: &mut Ui, v: &mut f32, right: &str) -> bool {
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), Sense::hover());
    let rect = Rect::from_min_max(row.min, egui::pos2(row.right() - if right.is_empty() { 0.0 } else { 44.0 }, row.bottom()));
    let resp = ui.interact(rect, ui.id().with(("slider", right.is_empty())), Sense::click_and_drag());
    let tr = Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.center().y - 1.0), egui::pos2(rect.right() - 6.0, rect.center().y + 1.0));
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || resp.dragged(), 0.1);
    let p = ui.painter();
    p.rect_filled(tr, 1, track());
    let x = tr.left() + tr.width() * v.clamp(0.0, 1.0);
    p.rect_filled(Rect::from_min_max(tr.min, egui::pos2(x, tr.max.y)), 1, accent());
    for q in 0..=4 {
        let c = egui::pos2(tr.left() + tr.width() * q as f32 / 4.0, tr.center().y);
        let on = *v >= q as f32 / 4.0 - 1e-4;
        p.add(egui::Shape::convex_polygon(vec![c + egui::vec2(0.0, -4.0), c + egui::vec2(4.0, 0.0), c + egui::vec2(0.0, 4.0), c + egui::vec2(-4.0, 0.0)],
            if on { accent() } else { track() }, Stroke::NONE));
    }
    let k = egui::pos2(x, tr.center().y);
    if hv > 0.0 { p.circle_filled(k, 6.0 + 4.0 * hv, accent().gamma_multiply(0.18 * hv)); }
    p.circle(k, 6.0, Color32::WHITE, Stroke::new(1.0, accent()));
    if !right.is_empty() { p.text(row.right_center(), Align2::RIGHT_CENTER, right, mono_m(12.0), mu()); }
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.clicked()) {
        let mut x = ((pos.x - tr.left()) / tr.width()).clamp(0.0, 1.0);
        for q in [0.0, 0.25, 0.5, 0.75, 1.0] { if (x - q).abs() < 0.03 { x = q; } }
        *v = x;
        return true;
    }
    false
}

/// Compact toggle pill (24 px): selected = hl solid, 600, strong; otherwise grey, hl fading in on hover.
fn pill(ui: &mut Ui, text: &str, on: bool) -> Response {
    let font = if on { prop_s(12.0) } else { prop_m(12.0) };
    let g = ui.painter().layout_no_wrap(text.to_string(), prop_s(12.0), fg());
    let (r, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 16.0, H_SM), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.12);
    let fill = if t > 0.0 { hl().gamma_multiply(t.max(0.6 * hv)) } else { hl().gamma_multiply(0.6 * hv) };
    ui.painter().rect_filled(r, kit::R, fill);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, font, lerp(mu(), fg_strong(), t.max(hv * 0.7)));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "label ........ [controls]" row: label 12 grey on the left, `add` laid out from the right edge.
fn row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    ui.horizontal(|ui| {
        ui.set_min_height(H_SM);
        ui.label(RichText::new(label).font(prop(12.0)).color(mu()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add).inner
    })
}

/// A labelled switch row (replaces checkboxes): label 12.5 in fg, switch on the right.
fn switch_row(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    ui.horizontal(|ui| {
        ui.set_min_height(H_SM);
        ui.add(egui::Label::new(RichText::new(label).font(prop(12.0)).color(fg())).wrap());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::switch(ui, on); });
    }).response
}

/// Secondary button that reads as active (accent border) when `on`.
fn secondary(ui: &mut Ui, text: &str, h: f32, w: Option<f32>, on: bool, enabled: bool) -> Response {
    let r = kit::button(ui, text, Look::Secondary, h, w, enabled);
    if on { ui.painter().rect_stroke(r.rect, kit::R, Stroke::new(1.0, accent()), egui::StrokeKind::Inside); }
    r
}

/// Thin progress bar (3 px, accent on the track grey).
fn progress(ui: &mut Ui, frac: f32, col: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), Sense::hover());
    ui.painter().rect_filled(r, 1, track());
    ui.painter().rect_filled(Rect::from_min_size(r.min, egui::vec2(r.width() * frac.clamp(0.0, 1.0), r.height())), 1, col);
}

/// Dialog frame: popup surface, radius 6, hairline, soft shadow, 20 px padding.
fn dialog(id: &str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(id)).backdrop_color(Color32::from_black_alpha(140)).frame(egui::Frame::new().fill(popup_bg()).corner_radius(6)
        .stroke(Stroke::new(1.0, line())).inner_margin(20).shadow(egui::Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(110) }))
}

/// Dialog title row: 16 semibold, a grey subtitle beside it, an optional tag on the right.
fn dialog_title(ui: &mut Ui, title: &str, sub: &str, tag: Option<(&str, Tone)>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(title).font(prop_s(16.0)).color(fg_strong()));
        if !sub.is_empty() { ui.label(RichText::new(sub).font(prop(12.5)).color(mu())); }
        if let Some((t, tone)) = tag { ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::chip(ui, t, tone); }); }
    });
}

/// Dialog footer: Cancel (secondary) and the primary action side by side; returns (cancel, ok).
fn dialog_buttons(ui: &mut Ui, cancel: &str, ok: &str, col: Color32, enabled: bool) -> (bool, bool) {
    ui.add_space(4.0);
    let (mut c, mut o) = (false, false);
    ui.columns(2, |cols| {
        c = kit::button(&mut cols[0], cancel, Look::Secondary, H_LG, Some(f32::INFINITY), true).clicked();
        o = kit::button(&mut cols[1], ok, Look::Fill(col), H_LG, Some(f32::INFINITY), enabled).clicked();
    });
    (c, o)
}

/// Two "label .... value" cells side by side, 8 px apart (under the buy / sell buttons: 11.5, label
/// dim; `big` in dialogs: label 12 grey, value 12.5); values tabular, right-aligned in their colour.
fn pair_row(ui: &mut Ui, cells: [(&str, String, Color32); 2], big: bool) {
    let (h, gap, lf, vf, lc) = if big { (18.0, 16.0, prop(12.0), mono_m(12.5), mu()) } else { (16.0, 8.0, prop(11.5), mono_m(11.5), dim()) };
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::hover());
    let w = (r.width() - gap) / 2.0;
    for (i, (k, v, c)) in cells.into_iter().enumerate() {
        let x = r.left() + i as f32 * (w + gap);
        ui.painter().text(egui::pos2(x, r.center().y), Align2::LEFT_CENTER, k, lf.clone(), lc);
        ui.painter().text(egui::pos2(x + w, r.center().y), Align2::RIGHT_CENTER, v, vf.clone(), c);
    }
}

/// The two sides' facts under the buy / sell buttons: (label, buy-side value, sell-side value)
/// rows; the buy column flush left, the sell column flush right, label grey right before its value.
fn side_facts(ui: &mut Ui, rows: &[(&str, String, String, &str)]) {
    for (k, a, b, tip) in rows {
        // the two sides share a row; when they would touch (long values in a narrow column) they
        // take a row each, the sell side right-aligned
        let w = |t: &str, f: egui::FontId| ui.painter().layout_no_wrap(t.to_string(), f, fg()).size().x;
        let need = 2.0 * w(k, prop(11.5)) + w(a, mono_m(11.5)) + w(b, mono_m(11.5)) + 22.0;
        let two = need > ui.available_width();
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), if two { 34.0 } else { 17.0 }), Sense::hover());
        if !tip.is_empty() { resp.on_hover_text(*tip); }
        let p = ui.painter();
        let (y0, y1) = if two { (r.top() + 8.5, r.top() + 25.5) } else { (r.center().y, r.center().y) };
        let kw = p.text(egui::pos2(r.left(), y0), Align2::LEFT_CENTER, *k, prop(11.5), mu()).width();
        p.text(egui::pos2(r.left() + kw + 5.0, y0), Align2::LEFT_CENTER, a, mono_m(11.5), fg());
        let vw = p.text(egui::pos2(r.right(), y1), Align2::RIGHT_CENTER, b, mono_m(11.5), fg()).width();
        p.text(egui::pos2(r.right() - vw - 5.0, y1), Align2::RIGHT_CENTER, *k, prop(11.5), mu());
    }
}

/// Small close glyph button (painted, no font dependency).
fn close_x(ui: &mut Ui, size: f32) -> Response {
    kit::icon_button(ui, size, false, |p, r, c| {
        let (c0, d) = (r.center(), 3.5);
        p.line_segment([c0 + egui::vec2(-d, -d), c0 + egui::vec2(d, d)], Stroke::new(1.3, c));
        p.line_segment([c0 + egui::vec2(-d, d), c0 + egui::vec2(d, -d)], Stroke::new(1.3, c));
    })
}

/// Tighter rows inside a dialog group (kit::kv rows follow interact_size).
fn tight(ui: &mut Ui) { ui.spacing_mut().interact_size.y = 18.0; ui.spacing_mut().item_spacing.y = 6.0; }

/// Overline caption above a group in a dialog.
fn caption(ui: &mut Ui, text: &str) { ui.add_space(2.0); kit::overline(ui, text); }

impl OrderPanel {
    /// The whole order column. `st` is the app's state document, `call` sends ops.
    pub fn show(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let t = &st["trade"];
        let (venue, symbol, base) = (s(&t["venue"]).to_string(), s(&t["symbol"]).to_string(), s(&st["base"]).to_string());
        let (tick, step) = (f(&t["tick"]), f(&t["step"]));
        let (bid, ask) = (f(&t["bid"]), f(&t["ask"]));
        let smart = st["route"]["smart"].as_bool().unwrap_or(false);
        let has_key = t["has_key"].as_bool().unwrap_or(false);
        // a read-only key can show the account but never send an order
        let read_only = t["read_only"] == true && !smart;
        let can_trade = (has_key && !read_only) || smart;
        let caps = &t["caps"];
        let bbo_levels: Vec<u64> = t["bbo_levels"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();

        // react to a new symbol, venue or book click (the Swift onChange handlers)
        let click = st["book_click"].as_object().map(|c| (s(&c["ex"]).to_string(), c["px"].as_f64().unwrap_or(0.0)));
        if self.seen.0 != symbol {
            self.price = bid.map(|b| px_str(b, tick)).unwrap_or_default();
            (self.qty, self.pct, self.tp, self.sl, self.error) = (String::new(), 0.0, String::new(), String::new(), None);
        }
        if self.seen.1 != venue && ((self.kind == Kind::Stop && caps["stop"] != true) || (self.kind == Kind::Trailing && caps["trailing"] != true)) { self.kind = Kind::Limit; }
        if click != self.seen.2 {
            if let Some((ex, px)) = &click { if *ex == venue { self.kind = Kind::Limit; self.bbo_on = false; self.price = px_str(*px, tick); } }
        }
        self.seen = (symbol.clone(), venue.clone(), click);
        if self.price.is_empty() { if let Some(b) = bid { self.price = px_str(b, tick); } }
        if !bbo_levels.contains(&self.bbo_level) { self.bbo_level = bbo_levels.first().copied().unwrap_or(1); }
        self.check_waiting(st, call);

        let use_bbo = self.kind == Kind::Limit && self.bbo_on && !bbo_levels.is_empty();
        let typed_price = (self.kind == Kind::Limit && !use_bbo) || (self.kind == Kind::Stop && self.stop_limit);
        let mid = match (bid, ask) { (Some(b), Some(a)) => Some((b + a) / 2.0), _ => bid.or(ask) };
        let ref_px = match self.kind {
            Kind::Stop => (if self.stop_limit { parse(&self.price) } else { None }).or(parse(&self.trigger)).or(mid).unwrap_or(0.0),
            Kind::Scaled => match (parse(&self.scaled_from), parse(&self.scaled_to)) { (Some(a), Some(b)) => (a + b) / 2.0, _ => mid.unwrap_or(0.0) },
            _ => (if typed_price { parse(&self.price) } else { None }).or(mid).unwrap_or(0.0),
        };
        let lev = f(&t["lev"]).unwrap_or(1.0).max(1.0);
        let qty_v = parse(&self.qty).unwrap_or(0.0);
        let positions: Vec<&Value> = st["positions"].as_array().map(|a| a.iter().filter(|p| s(&p["symbol"]) == symbol && (smart || s(&p["ex"]) == venue)).collect()).unwrap_or_default();
        let held = |side: &str| positions.iter().filter(|p| s(&p["side"]) == side).map(|p| f(&p["qty"]).unwrap_or(0.0)).sum::<f64>();
        let max_qty = if self.close { held("long").max(held("short")) } else if ref_px > 0.0 { f(&t["available"]).unwrap_or(0.0) * lev / ref_px } else { 0.0 };

        let mut enter = false;
        card(ui, |ui| {
            self.header(ui, t, &venue, smart, has_key, call);
            // the venue's own touch on its own row: bid left, spread centre, ask right; a click
            // puts that price in the limit field
            if let (Some(b), Some(k)) = (bid, ask) {
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::hover());
                ui.painter().rect_filled(r, kit::R, panel2());
                let half = r.width() / 2.0;
                for (i, (lab, v, col)) in [(l("Bid"), b, up()), (l("Ask"), k, dn())].into_iter().enumerate() {
                    let cell = Rect::from_min_size(egui::pos2(r.left() + half * i as f32, r.top()), egui::vec2(half, r.height()));
                    let resp = ui.interact(cell, ui.id().with(("touch", i)), Sense::click());
                    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
                    if hv > 0.0 { ui.painter().rect_filled(cell.shrink(2.0), kit::R, hl().gamma_multiply(0.7 * hv)); }
                    let (al, x) = if i == 0 { (Align2::LEFT_CENTER, cell.left() + 10.0) } else { (Align2::RIGHT_CENTER, cell.right() - 10.0) };
                    let w = ui.painter().text(egui::pos2(x, cell.center().y), al, lab, prop(11.0), dim()).width();
                    let vx = if i == 0 { x + w + 5.0 } else { x - w - 5.0 };
                    ui.painter().text(egui::pos2(vx, cell.center().y), al, fmt_px(v), mono_m(12.5), col);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(l("Use this price")).clicked() && self.kind != Kind::Market {
                        if self.kind != Kind::Stop { self.kind = Kind::Limit; }
                        (self.bbo_on, self.price) = (false, px_str(v, tick));
                    }
                }
                let mid = (b + k) / 2.0;
                if mid > 0.0 { ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("{:.1}bp", (k - b) / mid * 1e4), mono(11.0), dim()); }
            }
            if kit::segments(ui, &mut self.close, &[(false, l("Open")), (true, l("Close"))], 28.0) { (self.pct, self.qty, self.error) = (0.0, String::new(), None); }
            self.type_tabs(ui, caps);
            // available margin, with the transfer shortcut
            ui.horizontal(|ui| {
                ui.set_min_height(18.0);
                ui.label(RichText::new(l("Available")).font(prop(12.0)).color(mu()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if has_key && kit::button(ui, "↔", Look::Link, 18.0, Some(18.0), true).on_hover_text(l("Move funds between accounts on this venue")).clicked() { self.open_transfer = Some(venue.clone()); }
                    ui.label(RichText::new(f(&t["available"]).map(|a| format!("{} USDT", usd(a))).unwrap_or("–".into())).font(mono_m(12.0)).color(fg()));
                });
            });
            enter |= self.kind_fields(ui, t, &venue, &bbo_levels, use_bbo, tick);
            let (r, e) = kit::field(ui, l("Size"), &mut self.qty, &base, "qty", false);
            enter |= e;
            if r.changed() { self.pct = if max_qty > 0.0 { (parse(&self.qty).unwrap_or(0.0) / max_qty).min(1.0) as f32 } else { 0.0 }; }
            let pct = format!("{:.0}%", self.pct * 100.0);
            ui.scope(|ui| {
                if slider(ui, &mut self.pct, &pct) { self.qty = if self.pct > 0.0 { qty_str(max_qty * self.pct as f64, step) } else { String::new() }; }
            }).response.on_hover_text(if self.close { l("Share of the position") } else { l("Share of available margin × leverage") });
            if !self.close && matches!(self.kind, Kind::Limit | Kind::Market) { enter |= self.tpsl_section(ui); }
            self.algo_jobs(ui, st, call);
            // the two sides, with what each can open / close and the margin it costs
            let (l1, l2) = if self.close { (l("Close Short"), l("Close Long")) } else { (l("Open Long"), l("Open Short")) };
            let (p1, p2) = if self.close { ("short", "long") } else { ("long", "short") };
            let mut go = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let w = (ui.available_width() - 8.0) / 2.0;
                if kit::button(ui, l1, Look::Fill(up()), 32.0, Some(w), can_trade).clicked() { go = Some(p1); }
                if kit::button(ui, l2, Look::Fill(dn()), 32.0, Some(w), can_trade).clicked() { go = Some(p2); }
            });
            // per side, Binance-style: "label value" pairs, the buy side flush left, the sell side
            // flush right; est. liquidation (isolated estimate), margin, and what can be opened
            ui.spacing_mut().item_spacing.y = 3.0;
            if self.close {
                side_facts(ui, &[(l("Max"), format!("{} {base}", fmt_qty(held(p1))), format!("{} {base}", fmt_qty(held(p2))), "")]);
            } else {
                // a unified / portfolio-margin account liquidates on the whole account (uniMMR,
                // margin ratio): no per-order price exists, so none is shown. Otherwise an
                // isolated-margin estimate with a 0.5 % maintenance rate, marked ≈.
                let bal = st["balances"].as_array().and_then(|a| a.iter().find(|b| s(&b["ex"]) == venue));
                let acct_wide = bal.is_some_and(|b| !b["uni_mmr"].is_null() || !b["mm_rate"].is_null());
                let liq = |long: bool| if !acct_wide && qty_v > 0.0 && ref_px > 0.0 {
                    let m = 1.0 / lev - 0.005;
                    format!("≈{} USDT", fmt_px(ref_px * if long { 1.0 - m } else { 1.0 + m }))
                } else { "-- USDT".into() };
                let liq_tip = if acct_wide { l("Unified account: liquidation is decided on the whole account (UniMMR / margin ratio in the account card), not per order.") }
                    else { l("Estimate for an isolated position at this leverage with a 0.5% maintenance rate; the venue's own figure can differ.") };
                let cost = format!("{} USDT", usd(qty_v * ref_px / lev));
                let can = format!("{} USDT", usd(max_qty * ref_px));
                side_facts(ui, &[(l("Est. liq. price"), liq(true), liq(false), liq_tip), (l("Margin"), cost.clone(), cost, ""), (l("Max open"), can.clone(), can, "")]);
            }
            ui.spacing_mut().item_spacing.y = 6.0;
            if read_only { kit::note(ui, l("This API key is read-only: trading is disabled. Allow trading for the key on the exchange to place orders."), WARN); }
            if let Some(p) = go { self.submit(p, st, ref_px, use_bbo, typed_price, call); }
            self.summary(ui, t, &base, qty_v * ref_px, use_bbo, has_key, qty_v);
            if let Some(e) = &self.error { kit::note(ui, e, dn()); }
            let mine: Vec<&Waiting> = self.waiting.iter().filter(|w| w.symbol == symbol).collect();
            if !mine.is_empty() {
                kit::overline(ui, l("TP/SL waiting for fill"));
                let mut drop = None;
                for (i, w) in mine.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} · TP {} · SL {}", w.ex, w.tp.map(fmt_px).unwrap_or("–".into()), w.sl.map(fmt_px).unwrap_or("–".into()))).font(mono_m(11.5)).color(fg()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if close_x(ui, 20.0).on_hover_text(l("Forget this TP/SL")).clicked() { drop = Some(i); }
                        });
                    });
                }
                if let Some(i) = drop { let id = (mine[i].ex.clone(), mine[i].until); self.waiting.retain(|w| (w.ex.clone(), w.until) != id); }
            }
        });
        ui.add_space(8.0);
        self.account_card(ui, st, t, &venue, has_key);
        if enter {
            match self.last_pos.clone() {
                Some(p) => self.submit(&p, st, ref_px, use_bbo, typed_price, call),
                None => self.error = Some(l("Press Buy or Sell once; Enter then repeats that side.").into()),
            }
        }
        self.confirm_modal(ui, st, call);
        self.algo_modal(ui, t, call);
        self.leverage_modal(ui, t, call);
        tpsl_modal(&mut self.sheet, ui, st, call);
    }

    /// Position mode, leverage (opens the sheet), untested warning, routing / venue menu.
    fn header(&mut self, ui: &mut Ui, t: &Value, venue: &str, smart: bool, has_key: bool, call: &mut Call) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.set_min_height(H_SM);
            if let Some(ex) = depth::Exchange::parse(venue) {
                let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
                icon(ui.painter(), ex, r, false);
            }
            // mode and leverage are only known with keys
            if has_key {
                let mode = match s(&t["mode"]) { "hedge" => Some(l("Hedge")), "" => None, _ => Some(l("One-Way")) };
                if let Some(m) = mode { kit::chip(ui, m, Tone::Neutral).on_hover_text(l("Position mode for this symbol (change it on the exchange)")); }
                let lv = f(&t["lev"]).map(|v| format!("{v}x")).unwrap_or(l("Leverage").into());
                let r = kit::chip(ui, &lv, Tone::Outline);
                let r = ui.interact(r.rect, ui.id().with("lev_chip"), Sense::click());
                if r.hovered() { ui.painter().rect_stroke(r.rect, 3, Stroke::new(1.0, edge()), egui::StrokeKind::Inside); }
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(l("Change leverage for this symbol")).clicked() {
                    self.lev = Some(f(&t["lev"]).unwrap_or(10.0));
                    let _ = call(json!({"op": "leverage_max"}));
                }
            }
            if t["verified"] == false { kit::chip(ui, l("Untested"), Tone::Warn).on_hover_text(l("Order placement on this venue is not yet verified on a live account: start with the minimum size.")); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = if smart { format!("{} ▾", l("Smart")) } else { format!("{venue} ▾") };
                let r = kit::button(ui, &name, Look::Link, H_SM, None, true)
                    .on_hover_text(format!("{} {} · {} {}", l("Push"), ms(f(&t["push_ms"])), l("RTT"), ms(f(&t["rtt_ms"]))));
                egui::Popup::menu(&r).show(|ui| {
                    if ui.button(l("Order routing…")).clicked() { self.open_settings = Some(2); ui.close(); }
                    if ui.button(l("API keys…")).clicked() { self.open_settings = Some(3); ui.close(); }
                });
            });
        });
    }

    /// Limit / Market / Stop as segments, Trailing / Scaled / TWAP behind "More".
    fn type_tabs(&mut self, ui: &mut Ui, caps: &Value) {
        let mut main: Vec<(Kind, &str)> = vec![(Kind::Limit, l("Limit")), (Kind::Market, l("Market"))];
        if caps["stop"] == true { main.push((Kind::Stop, l("Stop"))); }
        let mut more: Vec<(Kind, &str)> = vec![];
        if caps["trailing"] == true { more.push((Kind::Trailing, l("Trailing Stop"))); }
        more.extend([(Kind::Scaled, l("Scaled")), (Kind::Twap, l("TWAP"))]);
        let in_more = more.iter().any(|m| m.0 == self.kind);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let w = ui.available_width() - 82.0;
            ui.allocate_ui(egui::vec2(w, 28.0), |ui| {
                // a More type selected: no segment is lit; clicking one switches back
                let mut k = (!in_more).then_some(self.kind);
                let items: Vec<(Option<Kind>, &str)> = main.iter().map(|(k, n)| (Some(*k), *n)).collect();
                if kit::segments(ui, &mut k, &items, 28.0) { if let Some(k) = k { self.kind = k; } }
            });
            let label = if in_more { more.iter().find(|m| m.0 == self.kind).map_or(l("More"), |m| m.1) } else { l("More") };
            let r = secondary(ui, &format!("{label} ▾"), 28.0, Some(f32::INFINITY), in_more, true);
            egui::Popup::menu(&r).show(|ui| {
                for (k, name) in &more { if ui.selectable_label(self.kind == *k, *name).clicked() { self.kind = *k; ui.close(); } }
            });
        });
    }

    /// The kind-specific inputs between "Available" and the size; true when Enter was pressed.
    fn kind_fields(&mut self, ui: &mut Ui, t: &Value, venue: &str, bbo_levels: &[u64], use_bbo: bool, _tick: Option<f64>) -> bool {
        let mut enter = false;
        match self.kind {
            Kind::Limit | Kind::Market => {
                if self.kind == Kind::Market {
                    let mut m = l("Market price").to_string();
                    ui.add_enabled_ui(false, |ui| kit::field(ui, l("Price"), &mut m, "", "mkt", false));
                } else {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let w = ui.available_width() - if bbo_levels.is_empty() { 0.0 } else { 54.0 };
                        ui.allocate_ui(egui::vec2(w, 28.0), |ui| {
                            if use_bbo {
                                let name = |q: bool, lv: u64| format!("{} {lv}", if q { l("Queue") } else { l("Counterparty") });
                                let r = kit::button(ui, &format!("BBO · {} ▾", name(self.bbo_queue, self.bbo_level)), Look::Secondary, 28.0, Some(f32::INFINITY), true)
                                    .on_hover_text(if self.bbo_queue { l("Own side of the book: rests as a maker at the best price.") } else { l("Opposite side of the book: fills like a taker at the price the venue sees on arrival.") });
                                egui::Popup::menu(&r).show(|ui| {
                                    for q in [false, true] {
                                        kit::overline(ui, if q { l("Queue") } else { l("Counterparty") });
                                        for lv in bbo_levels { if ui.selectable_label(self.bbo_queue == q && self.bbo_level == *lv, name(q, *lv)).clicked() { self.bbo_queue = q; self.bbo_level = *lv; } }
                                    }
                                });
                            } else {
                                enter |= kit::field(ui, l("Price"), &mut self.price, "USDT", "price", false).1;
                            }
                        });
                        if !bbo_levels.is_empty() && secondary(ui, "BBO", 28.0, Some(ui.available_width()), self.bbo_on, true)
                            .on_hover_text(l("Priced by the venue's book when the order arrives (Counterparty: opposite side; Queue: own side; number: book level)")).clicked() { self.bbo_on = !self.bbo_on; }
                    });
                    row(ui, l("Time in force"), |ui| {
                        let short = |k: &str| match k { "ioc" => "IOC", "fok" => "FOK", "post" => l("Post only"), _ => "GTC" };
                        let r = kit::button(ui, &format!("{} ▾", if use_bbo { "GTC" } else { short(self.tif) }), Look::Ghost, H_SM, None, !use_bbo)
                            .on_hover_text(if use_bbo { l("BBO orders are good till cancelled") } else { l("How long the order may rest on the book") });
                        egui::Popup::menu(&r).show(|ui| {
                            let mut opts = vec![("gtc", l("GTC · good till cancelled")), ("ioc", l("IOC · fill what's there, cancel the rest"))];
                            if t["caps"]["fok"] == true { opts.push(("fok", l("FOK · fill completely or cancel"))); }
                            opts.push(("post", l("Post only (ALO) · maker or cancel")));
                            for (k, name) in opts { if ui.selectable_label(self.tif == k, name).clicked() { self.tif = k; ui.close(); } }
                        });
                    });
                }
            }
            Kind::Stop => {
                enter |= kit::field(ui, l("Trigger price"), &mut self.trigger, "USDT", "trigger", false).1;
                row(ui, l("Trigger by"), |ui| {
                    ui.allocate_ui(egui::vec2(128.0, H_SM), |ui| kit::segments(ui, &mut self.trigger_last, &[(false, l("Mark")), (true, l("Last"))], H_SM));
                }).response.on_hover_text(l("Which price must reach the trigger: mark (harder to spike) or last trade"));
                switch_row(ui, &mut self.stop_limit, l("Limit order when triggered"));
                if self.stop_limit { enter |= kit::field(ui, l("Limit price"), &mut self.price, "USDT", "stop_px", false).1; }
                kit::note(ui, l("Fires once the trigger is reached: above the price it acts as a buy stop / sell take-profit, below it as a sell stop / buy take-profit."), mu());
            }
            Kind::Trailing => {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.allocate_ui(egui::vec2(ui.available_width() - 34.0, 28.0), |ui| enter |= kit::field(ui, l("Callback rate"), &mut self.callback, "%", "cb", false).1);
                    let r = kit::button(ui, "▾", Look::Secondary, 28.0, Some(28.0), true);
                    egui::Popup::menu(&r).show(|ui| { for v in ["0.5", "1.0", "2.0", "5.0"] { if ui.selectable_label(self.callback == v, format!("{v}%")).clicked() { self.callback = v.into(); ui.close(); } } });
                });
                enter |= kit::field(ui, l("Activation price (optional)"), &mut self.activation, "USDT", "act", false).1;
                kit::note(ui, if venue == "Bybit" { l("Bybit trails an open position: use it from Close. It closes the whole position at market once price retraces by the callback from its best level.") }
                    else { l("A market order once price retraces by the callback from its best level since activation.") }, mu());
            }
            Kind::Scaled => {
                ui.columns(2, |c| {
                    enter |= kit::field(&mut c[0], l("From price"), &mut self.scaled_from, "", "from", false).1;
                    enter |= kit::field(&mut c[1], l("To price"), &mut self.scaled_to, "", "to", false).1;
                });
                row(ui, l("Orders"), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if kit::button(ui, "+", Look::Secondary, H_SM, Some(H_SM), self.scaled_count < 50).clicked() { self.scaled_count += 1; }
                    ui.add_sized([28.0, H_SM], egui::Label::new(RichText::new(self.scaled_count.to_string()).font(mono_m(12.5)).color(fg_strong())));
                    if kit::button(ui, "−", Look::Secondary, H_SM, Some(H_SM), self.scaled_count > 2).clicked() { self.scaled_count -= 1; }
                });
                row(ui, l("Size distribution"), |ui| {
                    ui.label(RichText::new(if self.scaled_skew < -0.05 { l("Larger first") } else if self.scaled_skew > 0.05 { l("Larger last") } else { l("Equal") }).font(prop_m(12.0)).color(fg()));
                });
                // skew -1..=1 in steps of 0.25 on the 0..=1 slider
                let mut x = ((self.scaled_skew + 1.0) / 2.0) as f32;
                if slider(ui, &mut x, "") { self.scaled_skew = (x as f64 * 8.0).round() / 4.0 - 1.0; }
                switch_row(ui, &mut self.scaled_post, l("Post only (maker or cancel)"));
            }
            Kind::Twap => {
                let range: Option<(i64, i64)> = t["native_twap"].as_array().filter(|a| a.len() == 2).map(|a| (a[0].as_i64().unwrap_or(0), a[1].as_i64().unwrap_or(0)));
                if let Some((lo, hi)) = range {
                    switch_row(ui, &mut self.twap_native, &lf("Run on %@ (keeps running when Depth is closed)", &[venue.into()]))
                        .on_hover_text(lf("%@ slices the order itself; duration %d to %d minutes.", &[venue.into(), lo.to_string(), hi.to_string()]));
                }
                if range.is_some() && self.twap_native {
                    enter |= kit::field(ui, l("Duration (min)"), &mut self.twap_minutes, "", "tm", false).1;
                    if venue == "Binance" {
                        enter |= kit::field(ui, l("Price limit (optional)"), &mut self.twap_limit, "USDT", "tl", false).1;
                        kit::note(ui, l("Binance runs the TWAP: at least 1,000 USDT notional, 5 minutes to 24 hours. Cancel it here or in the Binance app."), mu());
                    } else {
                        switch_row(ui, &mut self.twap_random, l("Randomize slice timing"));
                        kit::note(ui, l("Hyperliquid runs the TWAP: a slice every 30 seconds, at most 3% slippage each. Cancel it here or on Hyperliquid."), mu());
                    }
                } else {
                    ui.columns(2, |c| {
                        enter |= kit::field(&mut c[0], l("Duration (min)"), &mut self.twap_minutes, "", "tm", false).1;
                        enter |= kit::field(&mut c[1], l("Slices"), &mut self.twap_slices, "", "ts", false).1;
                    });
                    ui.columns(2, |c| {
                        enter |= kit::field(&mut c[0], l("Max slippage (bp)"), &mut self.twap_slip, "", "tsl", false).1;
                        enter |= kit::field(&mut c[1], l("Price limit (optional)"), &mut self.twap_limit, "", "tl", false).1;
                    });
                    let gap = match (parse(&self.twap_minutes), parse(&self.twap_slices)) {
                        (Some(m), Some(n)) if m > 0.0 && n > 0.0 => { let sec = m * 60.0 / n; if sec >= 60.0 { format!("{:.1} min", sec / 60.0) } else { format!("{sec:.0} s") } }
                        _ => "–".into(),
                    };
                    kit::note(ui, &lf("One IOC order every %@ at the venue's best price ± your slippage; slices wait while the price is beyond your limit. Keep this pair open while it runs.", &[gap]), mu());
                }
            }
        }
        enter
    }

    fn tpsl_section(&mut self, ui: &mut Ui) -> bool {
        let mut enter = false;
        ui.horizontal(|ui| {
            ui.set_min_height(H_SM);
            ui.label(RichText::new(l("TP / SL")).font(prop_m(12.5)).color(fg()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::switch(ui, &mut self.tpsl_on); });
        }).response.on_hover_text(l("Attach a take-profit / stop-loss to the position this order opens"));
        if self.tpsl_on {
            row(ui, l("Trigger by"), |ui| {
                ui.allocate_ui(egui::vec2(128.0, H_SM), |ui| kit::segments(ui, &mut self.trig_last, &[(false, l("Mark")), (true, l("Last"))], H_SM));
            });
            enter |= kit::field(ui, l("Take profit"), &mut self.tp, "USDT", "tp", false).1;
            enter |= kit::field(ui, l("Stop loss"), &mut self.sl, "USDT", "sl", false).1;
            kit::note(ui, l("Applies to the whole position once the order fills."), mu());
        }
        enter
    }

    /// Running TWAP jobs (and finished ones for a minute) with progress and cancel.
    fn algo_jobs(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let now = depth::now_ms() as f64;
        let jobs: Vec<&Value> = st["algos"].as_array().map(|a| a.iter().filter(|j| {
            let end = f(&j["end_ms"]).unwrap_or(0.0);
            let done = j["cancelled"] == true || s(&j["status"]) == "done" || (j["native"] == true && now > end + 600_000.0);
            !done || now - end < 60_000.0
        }).collect()).unwrap_or_default();
        if jobs.is_empty() { return; }
        kit::overline(ui, l("Running algorithms"));
        for j in jobs {
            ui.horizontal(|ui| {
                let buy = j["buy"] == true;
                ui.label(RichText::new(format!("TWAP {} {}", if buy { l("buy") } else { l("sell") }, fmt_qty(f(&j["total"]).unwrap_or(0.0)))).font(prop_s(12.5)).color(if buy { up() } else { dn() }));
                ui.label(RichText::new(format!("{} · {}", s(&j["ex"]), s(&j["symbol"]))).font(prop(11.5)).color(mu()));
                if j["cancelled"] != true && s(&j["status"]) != "done" {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if kit::button(ui, l("Cancel"), Look::Danger, H_SM, None, true).clicked() { let _ = call(json!({"op": "cancel_algo", "id": j["id"]})); }
                    });
                }
            });
            let (started, end) = (f(&j["started_ms"]).unwrap_or(0.0), f(&j["end_ms"]).unwrap_or(0.0));
            if j["native"] == true {
                let frac = ((now - started) / (end - started).max(1.0)).clamp(0.0, 1.0) as f32;
                progress(ui, frac, accent());
                ui.label(RichText::new(format!("{} · {}", l(leak(s(&j["status"]))), if frac >= 1.0 { l("time elapsed: check fills in Trade History").to_string() } else { lf("%.0f%% of the time", &[format!("{:.0}", frac * 100.0)]) })).font(prop(11.5)).color(mu()));
            } else {
                let (done, slices) = (f(&j["done"]).unwrap_or(0.0), f(&j["slices"]).unwrap_or(1.0).max(1.0));
                progress(ui, (done / slices) as f32, accent());
                let status = s(&j["status"]);
                ui.label(RichText::new(format!("{done:.0}/{slices:.0} · {} {} · {}", fmt_qty(f(&j["sent"]).unwrap_or(0.0)), l("sent"), l(leak(status)))).font(mono(11.5))
                    .color(if status.starts_with("waiting") || status.starts_with("failed") { WARN } else { mu() }));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn summary(&self, ui: &mut Ui, t: &Value, base: &str, notional: f64, use_bbo: bool, has_key: bool, qty_v: f64) {
        let taker = matches!(self.kind, Kind::Market | Kind::Twap | Kind::Trailing) || (self.kind == Kind::Stop && !self.stop_limit)
            || (use_bbo && !self.bbo_queue) || (self.kind == Kind::Limit && matches!(self.tif, "ioc" | "fok"));
        let (mk, tk) = (f(&t["fee"]["maker"]).unwrap_or(0.0), f(&t["fee"]["taker"]).unwrap_or(0.0));
        let rate = if taker { tk } else { mk };
        ui.horizontal(|ui| {
            // both rates; the one this order pays is brighter
            ui.spacing_mut().item_spacing.x = 4.0;
            for (name, r, on) in [(l("Maker"), mk, !taker), (l("Taker"), tk, taker)] {
                ui.label(RichText::new(name).font(prop(11.0)).color(if on { mu() } else { dim() }));
                ui.label(RichText::new(format!("{:.3}%", r * 100.0)).font(mono_m(11.0)).color(if on { fg() } else { dim() }));
                ui.add_space(6.0);
            }
            if notional > 0.0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} USDT · {} ≈ {}", usd(notional), l("fee"), fmt_dp(notional * rate, 3))).font(mono(11.0)).color(dim()));
                });
            }
        });
        if t["lev"].is_null() && has_key && !self.close { kit::note(ui, l("Leverage unknown: max size and cost assume 1x."), WARN); }
        if let Some(m) = f(&t["min_qty"]).filter(|m| qty_v > 0.0 && qty_v < *m) { kit::note(ui, &format!("{} {} {base}", l("Below the minimum size"), fmt_qty(m)), WARN); }
        else if let Some(m) = f(&t["min_notional"]).filter(|m| notional > 0.0 && notional < *m) { kit::note(ui, &format!("{} {} USDT", l("Below the minimum order value"), usd(m)), WARN); }
    }

    /// Limit close of a position in the form: close tab, limit at the venue's own touch on the
    /// passive side, the whole size, and Enter repeats that side.
    pub(crate) fn prefill_close(&mut self, p: &Value, st: &Value) {
        let t = &st["trade"];
        (self.close, self.kind, self.bbo_on) = (true, Kind::Limit, false);
        if let Some(px) = f(&t[if s(&p["side"]) == "long" { "ask" } else { "bid" }]) { self.price = px_str(px, f(&t["tick"])); }
        self.qty = qty_str(f(&p["qty"]).unwrap_or(0.0), f(&t["step"]));
        self.last_pos = Some(s(&p["side"]).into());
    }

    /// The trade venue's account: margin health, equity, uPnL, keys shortcut.
    fn account_card(&mut self, ui: &mut Ui, st: &Value, t: &Value, venue: &str, has_key: bool) {
        let b = st["balances"].as_array().and_then(|a| a.iter().find(|b| s(&b["ex"]) == venue));
        if b.is_none() && has_key { return; }
        card(ui, |ui| {
            let Some(b) = b else {
                kit::title(ui, l("No API key for this venue"));
                if kit::button(ui, l("Add API keys"), Look::Secondary, H, None, true).clicked() { self.open_settings = Some(3); }
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(l("Account")).font(prop_s(14.0)).color(fg_strong()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kit::button(ui, &format!("↔ {}", l("Transfer")), Look::Link, H_SM, None, true).on_hover_text(l("Move funds between this venue's accounts")).clicked() { self.open_transfer = Some(venue.to_string()); }
                });
            });
            // the account-level number that liquidates a unified account, with a risk gauge beside it
            let risk_row = |ui: &mut Ui, k: &str, v: String, col: Color32, risk: f32, tip: &str| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(k).font(prop_m(12.0)).color(mu()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(v).font(mono_s(16.0)).color(col));
                        let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 14.0), Sense::hover());
                        risk_gauge(ui.painter(), egui::pos2(r.center().x, r.bottom() - 2.0), 8.0, risk);
                    });
                }).response.on_hover_text(tip);
            };
            if let Some(u) = f(&b["uni_mmr"]) {
                risk_row(ui, l("UniMMR"), fmt_dp(u, 2), if u < 1.5 { dn() } else if u < 3.0 { WARN } else { up() }, mmr_risk(u), l("Portfolio Margin: the whole account is liquidated when UniMMR falls to 1.05"));
            } else if let Some(r) = f(&b["mm_rate"]) {
                risk_row(ui, l("Margin ratio"), format!("{:.2}%", r * 100.0), if r > 0.7 { dn() } else if r > 0.4 { WARN } else { up() }, r as f32, l("Maintenance margin / margin balance; liquidation at 100%"));
            }
            if let Some(m) = f(&b["maint_margin"]) { kit::kv(ui, l("Maintenance margin"), format!("{} USD", usd(m)), fg()); }
            if let Some(e) = f(&b["adj_equity"]) { kit::kv(ui, l("Adjusted equity"), format!("{} USD", usd(e)), fg()); }
            kit::kv(ui, l("Equity"), format!("{} USD", usd(f(&b["equity"]).unwrap_or(0.0))), fg());
            let upnl: f64 = st["positions"].as_array().map(|a| a.iter().filter(|p| s(&p["ex"]) == venue).filter_map(|p| f(&p["upnl"])).sum()).unwrap_or(0.0);
            kit::kv(ui, l("Unrealized PnL"), format!("{} USD", signed(upnl)), if upnl > 0.0 { up() } else if upnl < 0.0 { dn() } else { fg() });
            if !has_key && kit::button(ui, l("Add API keys"), Look::Secondary, H, None, true).clicked() { self.open_settings = Some(3); }
            if let Some(e) = t["error"].as_str() { kit::note(ui, e, dn()); }
        });
    }

    /// Preview first (validates and routes), then the confirm modal, or place directly when
    /// confirmations are off.
    fn submit(&mut self, pos: &str, st: &Value, ref_px: f64, use_bbo: bool, typed_price: bool, call: &mut Call) {
        let t = &st["trade"];
        self.error = None;
        self.last_pos = Some(pos.into());
        let qty = parse(&self.qty).unwrap_or(0.0);
        if qty <= 0.0 { self.error = Some(l("Enter a size").into()); return; }
        let mut args = json!({"pos": pos, "close": self.close, "kind": if use_bbo { "bbo" } else { self.kind.arg() }, "qty": qty, "tif": self.tif});
        let side_word = |buy: &'static str, long: &'static str, sell: &'static str, short: &'static str| if pos == "long" { if self.close { l(buy) } else { l(long) } } else if self.close { l(sell) } else { l(short) };
        let base = s(&st["base"]).to_string();
        let venue = s(&t["venue"]).to_string();
        match self.kind {
            Kind::Scaled => {
                let (Some(a), Some(b)) = (parse(&self.scaled_from).filter(|v| *v > 0.0), parse(&self.scaled_to).filter(|v| *v > 0.0)) else { self.error = Some(l("Enter both prices").into()); return };
                (args["from"], args["to"], args["count"], args["skew"], args["post_only"]) = (a.into(), b.into(), self.scaled_count.into(), self.scaled_skew.into(), self.scaled_post.into());
                self.algo = Some(Algo { op: "place_scaled", args, title: l("Place scaled orders?").into(),
                    message: lf("%d %@ limit orders from %@ to %@, %@ %@ in total on %@.", &[self.scaled_count.to_string(), side_word("buy", "long", "sell", "short").into(), fmt_px(a), fmt_px(b), fmt_qty(qty), base, venue]) });
                return;
            }
            Kind::Twap => {
                let (Some(m), Some(n)) = (parse(&self.twap_minutes).filter(|v| *v > 0.0), self.twap_slices.trim().parse::<u64>().ok().filter(|n| *n > 0)) else { self.error = Some(l("Enter a duration and a number of slices").into()); return };
                (args["minutes"], args["slices"], args["max_slip_bps"]) = (m.into(), n.into(), parse(&self.twap_slip).unwrap_or(10.0).into());
                let limit = parse(&self.twap_limit).filter(|v| *v > 0.0);
                if let Some(lim) = limit { args["limit"] = lim.into(); }
                let range = t["native_twap"].as_array().filter(|a| a.len() == 2).map(|a| (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0)));
                let nat = range.is_some() && self.twap_native;
                (args["native"], args["randomize"]) = (nat.into(), self.twap_random.into());
                if let (true, Some((lo, hi))) = (nat, range) { if m < lo || m > hi { self.error = Some(lf("%@ TWAP duration must be %d to %d minutes", &[venue, lo.to_string(), hi.to_string()])); return; } }
                let lim = limit.map(|v| format!(", {} {}", l("limit"), fmt_px(v))).unwrap_or_default();
                let message = if nat { lf("%@ %@ %@ over %@ min, run by %@%@.", &[side_word("Buy", "Long", "Sell", "Short").into(), fmt_qty(qty), base, format!("{m:.0}"), venue, lim]) }
                    else { lf("%@ %@ %@ on %@ in %d slices over %@ min, at most %@ bp slippage per slice%@.", &[side_word("Buy", "Long", "Sell", "Short").into(), fmt_qty(qty), base, venue, n.to_string(), format!("{m:.0}"), self.twap_slip.clone(), lim]) };
                self.algo = Some(Algo { op: "start_twap", args, title: l("Start TWAP?").into(), message });
                return;
            }
            Kind::Stop => {
                let Some(tr) = parse(&self.trigger).filter(|v| *v > 0.0) else { self.error = Some(l("Enter a trigger price").into()); return };
                (args["trigger"], args["trigger_by"]) = (tr.into(), (if self.trigger_last { "last" } else { "mark" }).into());
            }
            Kind::Trailing => {
                let Some(cb) = parse(&self.callback).filter(|v| (0.1..=10.0).contains(v)) else { self.error = Some(l("Callback must be 0.1% to 10%").into()); return };
                args["callback_pct"] = cb.into();
                if let Some(a) = parse(&self.activation).filter(|v| *v > 0.0) { args["activation"] = a.into(); }
            }
            _ => {}
        }
        if typed_price {
            let Some(p) = parse(&self.price).filter(|v| *v > 0.0) else { self.error = Some(l("Enter a price").into()); return };
            args["price"] = p.into();
        }
        if use_bbo { (args["bbo_queue"], args["bbo_level"]) = (self.bbo_queue.into(), self.bbo_level.into()); }
        let (mut tp, mut sl) = (None, None);
        if self.tpsl_on && !self.close {
            (tp, sl) = (parse(&self.tp), parse(&self.sl));
            if tp.is_none() && sl.is_none() { self.error = Some(l("Enter a take-profit or stop-loss price, or turn TP/SL off").into()); return; }
            if let Some(e) = tpsl_check(pos == "long", ref_px, l("the order price"), tp, sl) { self.error = Some(e); return; }
        }
        let mut req = args.clone();
        req["op"] = "preview".into();
        let plan = match call(req) { Ok(p) => p, Err(e) => { self.error = Some(e); return } };
        let legs = plan["legs"].as_array().map_or(0, |a| a.len());
        if legs == 0 {
            let ex: Vec<String> = plan["excluded"].as_array().map(|a| a.iter().map(|x| format!("{}: {}", s(&x["ex"]), s(&x["reason"]))).collect()).unwrap_or_default();
            self.error = Some(if ex.is_empty() { l("No venue can take this order").into() } else { ex.join("\n") });
            return;
        }
        let tk = Ticket { pos: pos.into(), close: self.close, args, plan, tp, sl, trigger: if self.trig_last { "last" } else { "mark" }, err: None };
        if st["prefs"]["confirm"].as_bool().unwrap_or(true) { self.ticket = Some(tk); }
        else if let Err(e) = self.place(&tk, st, call) { self.error = Some(e); }
    }

    /// Send a previewed ticket; its TP/SL waits for each leg's position to exist.
    fn place(&mut self, tk: &Ticket, st: &Value, call: &mut Call) -> Result<(), String> {
        let mut args = tk.args.clone();
        args["op"] = "place".into();
        args["expect"] = tk.plan["legs"].as_array().map(|a| a.iter().map(|l| json!({"ex": l["ex"], "qty": l["qty"]})).collect::<Vec<_>>()).unwrap_or_default().into();
        let plan = call(args)?;
        if tk.tp.is_some() || tk.sl.is_some() {
            let symbol = s(&st["trade"]["symbol"]).to_string();
            let mut exs: Vec<String> = plan["legs"].as_array().map(|a| a.iter().map(|l| s(&l["ex"]).to_string()).collect()).unwrap_or_default();
            exs.dedup();
            for ex in exs {
                let base_qty = st["positions"].as_array().and_then(|a| a.iter().find(|p| s(&p["ex"]) == ex && s(&p["symbol"]) == symbol && s(&p["side"]) == tk.pos)).and_then(|p| f(&p["qty"])).unwrap_or(0.0);
                self.waiting.push(Waiting { ex, symbol: symbol.clone(), pos: tk.pos.clone(), base_qty, tp: tk.tp, sl: tk.sl, trigger: tk.trigger, until: Instant::now() + Duration::from_secs(120) });
            }
        }
        Ok(())
    }

    /// Applies waiting TP/SL once the position grew; drops them after the window without a resting order.
    fn check_waiting(&mut self, st: &Value, call: &mut Call) {
        let positions = st["positions"].as_array().cloned().unwrap_or_default();
        let orders = st["orders"].as_array().cloned().unwrap_or_default();
        self.waiting.retain(|w| {
            let q = positions.iter().find(|p| s(&p["ex"]) == w.ex && s(&p["symbol"]) == w.symbol && s(&p["side"]) == w.pos).and_then(|p| f(&p["qty"])).unwrap_or(0.0);
            if q > w.base_qty * (1.0 + 1e-9) + 1e-12 {
                let mut a = json!({"op": "set_tpsl", "ex": w.ex, "symbol": w.symbol, "pos": w.pos, "trigger": w.trigger});
                if let Some(tp) = w.tp { a["tp"] = tp.into(); }
                if let Some(sl) = w.sl { a["sl"] = sl.into(); }
                let _ = call(a);
                return false;
            }
            !(Instant::now() > w.until && !orders.iter().any(|o| s(&o["ex"]) == w.ex && s(&o["symbol"]) == w.symbol))
        });
    }

    fn confirm_modal(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let Some(tk) = &self.ticket else { return };
        let base = s(&st["base"]).to_string();
        let col = if tk.buy() { up() } else { dn() };
        let mut done = None;
        let m = dialog("order_confirm").show(ui.ctx(), |ui| {
            ui.set_width(440.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            dialog_title(ui, &format!("{base}USDT"), l("Perp"), Some((tk.title(), if tk.buy() { Tone::Up } else { Tone::Down })));
            let legs = tk.plan["legs"].as_array().cloned().unwrap_or_default();
            caption(ui, &if legs.len() > 1 { format!("{} · {} {}", l("Route"), legs.len(), l("legs")) } else { l("Route").into() });
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for lg in &legs {
                    let buy = (s(&lg["pos"]) == "long") != (lg["close"] == true);
                    kit::group(ui, |ui| {
                        tight(ui);
                        ui.horizontal(|ui| {
                            if let Some(ex) = depth::Exchange::parse(s(&lg["ex"])) {
                                let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
                                icon(ui.painter(), ex, r, false);
                            }
                            ui.label(RichText::new(s(&lg["ex"])).font(prop_s(13.0)).color(fg_strong()));
                            kit::chip(ui, if buy { l("Buy") } else { l("Sell") }, if buy { Tone::Up } else { Tone::Down });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} {base}", fmt_qty(f(&lg["qty"]).unwrap_or(0.0)))).font(mono_s(13.0)).color(fg_strong()));
                            });
                        });
                        pair_row(ui, [(l("Order"), kind_text(s(&lg["kind"])), fg()), (l("Est. price"), fmt_px(f(&lg["est_px"]).unwrap_or(0.0)), fg())], true);
                        let slip = f(&lg["slip_bps"]).unwrap_or(0.0);
                        pair_row(ui, [(l("Est. fee"), fmt_dp(f(&lg["est_fee"]).unwrap_or(0.0), 3), fg()), (l("Slippage"), format!("{slip:.1} bp"), if slip > 5.0 { WARN } else { fg() })], true);
                        let after = f(&lg["avail_after"]).unwrap_or(0.0);
                        kit::kv(ui, l("Available after"), format!("{} USDT", usd(after)), if after < 0.0 { dn() } else { fg() });
                    });
                }
                if let Some(ex) = tk.plan["excluded"].as_array().filter(|a| !a.is_empty()) {
                    caption(ui, l("Not used"));
                    for x in ex { kit::kv(ui, s(&x["ex"]), s(&x["reason"]).into(), mu()); }
                }
            });
            let (total, vwap) = (f(&tk.plan["total_qty"]).unwrap_or(0.0), f(&tk.plan["vwap"]).unwrap_or(0.0));
            caption(ui, l("Order"));
            kit::group(ui, |ui| {
                tight(ui);
                kit::kv(ui, l("Size"), format!("{} {base}", fmt_qty(total)), fg_strong());
                kit::kv(ui, l("Est. average price"), fmt_px(vwap), fg_strong());
                kit::kv(ui, l("Order value"), format!("{} USDT", usd(total * vwap)), fg_strong());
                kit::kv(ui, l("Est. fees"), format!("{} USDT", fmt_dp(f(&tk.plan["fees"]).unwrap_or(0.0), 3)), fg());
                if tk.tp.is_some() || tk.sl.is_some() {
                    kit::kv(ui, &format!("TP / SL ({})", if tk.trigger == "last" { l("Last") } else { l("Mark") }),
                        format!("{} / {}", tk.tp.map(fmt_px).unwrap_or("–".into()), tk.sl.map(fmt_px).unwrap_or("–".into())), accent());
                }
            });
            if let Some(c) = f(&tk.plan["clamped_from"]) {
                kit::note(ui, &format!("{} {} {} {} {base} ({})", l("Size reduced from"), fmt_qty(c), l("to"), fmt_qty(total), if tk.close { l("position size") } else { l("margin limit") }), WARN);
            }
            if tk.tp.is_some() || tk.sl.is_some() { kit::note(ui, &format!("TP / SL: {}", l("set for the whole position once the order has filled and the position exists.")), mu()); }
            let unverified = legs.iter().any(|lg| st["keys"].as_array().is_some_and(|k| k.iter().any(|k| k["ex"] == lg["ex"] && k["verified"] == false)));
            if unverified { kit::note(ui, l("Order placement on this venue is not yet verified on a live account: start with the minimum size."), WARN); }
            kit::note(ui, l("Prices and sizes are rounded to the venue's tick and step."), dim());
            if let Some(e) = &tk.err { kit::note(ui, e, dn()); }
            let (c, o) = dialog_buttons(ui, l("Cancel"), &format!("{} {}", l("Confirm"), tk.title()), col, true);
            if c || ui.input(|i| i.key_pressed(egui::Key::Escape)) { done = Some(false); }
            if o || ui.input(|i| i.key_pressed(egui::Key::Enter)) { done = Some(true); }
        });
        if m.should_close() && done.is_none() { done = Some(false); }
        match done {
            Some(false) => self.ticket = None,
            Some(true) => {
                let tk = self.ticket.take().unwrap();
                if let Err(e) = self.place(&tk, st, call) { self.ticket = Some(Ticket { err: Some(e), ..tk }); }
            }
            None => {}
        }
    }

    fn algo_modal(&mut self, ui: &mut Ui, t: &Value, call: &mut Call) {
        let Some(a) = &self.algo else { return };
        let mut done = None;
        dialog("algo_confirm").show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            dialog_title(ui, &a.title, "", None);
            ui.add(egui::Label::new(RichText::new(&a.message).font(prop(13.0)).color(fg())).wrap());
            if t["verified"] == false { kit::note(ui, l("This venue is untested: start with the minimum size."), WARN); }
            let (c, o) = dialog_buttons(ui, l("Cancel"), l("Confirm"), accent(), true);
            if c { done = Some(false); }
            if o { done = Some(true); }
        });
        if let Some(go) = done {
            let a = self.algo.take().unwrap();
            if go {
                let mut req = a.args;
                req["op"] = a.op.into();
                if let Err(e) = call(req) { self.error = Some(e); }
            }
        }
    }

    /// Leverage for the current symbol on the trade venue: slider to the venue's maximum, presets,
    /// then a confirmed change on the exchange.
    fn leverage_modal(&mut self, ui: &mut Ui, t: &Value, call: &mut Call) {
        let Some(mut lev) = self.lev else { return };
        let max = f(&t["max_lev"]);
        let mut done = None;
        dialog("leverage").show(ui.ctx(), |ui| {
            ui.set_width(400.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            dialog_title(ui, l("Leverage"), &format!("{} · {}", s(&t["symbol"]), s(&t["venue"])), None);
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(l("Current")).font(prop(12.0)).color(mu()));
                    ui.label(RichText::new(f(&t["lev"]).map(|v| format!("{v}x")).unwrap_or("–".into())).font(mono_m(12.5)).color(fg()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let hi = max.unwrap_or(200.0);
                        if kit::button(ui, "+", Look::Secondary, H, Some(H), lev < hi).clicked() { lev = (lev.round() + 1.0).min(hi); }
                        ui.add_sized([64.0, H], egui::Label::new(RichText::new(format!("{}x", lev.round())).font(display_s(22.0)).color(fg_strong())));
                        if kit::button(ui, "−", Look::Secondary, H, Some(H), lev > 1.0).clicked() { lev = (lev.round() - 1.0).max(1.0); }
                    });
                });
                match max {
                    Some(m) => {
                        let m = m.max(1.0);
                        let mut x = if m > 1.0 { ((lev - 1.0) / (m - 1.0)) as f32 } else { 1.0 };
                        if slider(ui, &mut x, &format!("{m}x")) { lev = (1.0 + x as f64 * (m - 1.0)).round(); }
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for v in [1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0, 125.0].into_iter().filter(|v| *v <= m) {
                                if pill(ui, &format!("{v}x"), lev == v).clicked() { lev = v; }
                            }
                        });
                    }
                    None => { ui.horizontal(|ui| { ui.spinner(); ui.label(RichText::new(l("Reading the venue's limit…")).font(prop(12.0)).color(mu())); }); }
                }
            });
            kit::note(ui, l("Changes the setting on the exchange for this symbol (both sides). Higher leverage moves the liquidation price closer; the venue may refuse it for a large position."), mu());
            let ok = lev >= 1.0 && max.is_none_or(|m| lev <= m) && Some(lev.round()) != f(&t["lev"]);
            let (c, o) = dialog_buttons(ui, l("Cancel"), &lf("Set %gx", &[format!("{}", lev.round())]), accent(), ok);
            if c { done = Some(false); }
            if o { done = Some(true); }
        });
        self.lev = Some(lev);
        match done {
            Some(true) => { if let Err(e) = call(json!({"op": "set_leverage", "leverage": lev.round() as i64})) { self.error = Some(e); } self.lev = None; }
            Some(false) => self.lev = None,
            None => {}
        }
    }

}

impl TpSlSheet {
    pub(crate) fn new(ex: &str, symbol: &str, side: &str) -> Self {
        TpSlSheet { ex: ex.into(), symbol: symbol.into(), side: side.into(), partial: false, tp: String::new(), sl: String::new(), last: false,
            levels: vec![Level { tp: true, price: String::new(), amount: "50".into(), pct: true }, Level { tp: false, price: String::new(), amount: "100".into(), pct: true }], err: None }
    }
}

/// TP/SL for an existing position: whole position, or staged levels (one `set_tpsl` with
/// `partial_qty` each); lists its live TP/SL with cancel. Closes itself when the position is gone.
pub(crate) fn tpsl_modal(sheet: &mut Option<TpSlSheet>, ui: &mut Ui, st: &Value, call: &mut Call) {
        let Some(sh) = sheet.as_mut() else { return };
        let Some(p) = st["positions"].as_array().and_then(|a| a.iter().find(|p| s(&p["ex"]) == sh.ex && s(&p["symbol"]) == sh.symbol && s(&p["side"]) == sh.side)).cloned() else { *sheet = None; return };
        let long = sh.side == "long";
        let sign = if long { 1.0 } else { -1.0 };
        let (qty, entry, mark) = (f(&p["qty"]).unwrap_or(0.0), f(&p["entry"]).unwrap_or(0.0), f(&p["mark"]).unwrap_or(0.0));
        let base = sh.symbol.trim_end_matches("USDT").to_string();
        let step = if sh.symbol == s(&st["trade"]["symbol"]) { f(&st["trade"]["step"]) } else { None };
        let qty_of = |lv: &Level| parse(&lv.amount).filter(|a| *a > 0.0).and_then(|a| parse(&qty_str(if lv.pct { qty * a.min(100.0) / 100.0 } else { a }, step)));
        let existing: Vec<Value> = st["tpsl"].as_array().map(|a| a.iter().filter(|x| s(&x["ex"]) == sh.ex && s(&x["symbol"]) == sh.symbol && s(&x["pos"]) == sh.side).cloned().collect()).unwrap_or_default();
        let (mut done, mut apply, mut cancel) = (false, false, None);
        dialog("tpsl_sheet").show(ui.ctx(), |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            dialog_title(ui, &format!("{} · {}", sh.ex, sh.symbol), l("TP/SL"), Some((if long { l("Long") } else { l("Short") }, if long { Tone::Up } else { Tone::Down })));
            kit::group(ui, |ui| {
                tight(ui);
                kit::kv(ui, l("Size"), format!("{} {base}", fmt_qty(qty)), fg_strong());
                kit::kv(ui, l("Entry / Mark"), format!("{} / {}", fmt_px(entry), fmt_px(mark)), fg());
                kit::kv(ui, l("Liq. price"), f(&p["liq"]).map(fmt_px).unwrap_or("–".into()), WARN);
            });
            kit::segments(ui, &mut sh.partial, &[(false, l("Entire position")), (true, l("Partial (staged)"))], 32.0);
            row(ui, l("Trigger by"), |ui| {
                ui.allocate_ui(egui::vec2(128.0, H_SM), |ui| kit::segments(ui, &mut sh.last, &[(false, l("Mark")), (true, l("Last"))], H_SM));
            });
            let pnl = |ui: &mut Ui, px: Option<f64>, q: f64| {
                if let Some(px) = px.filter(|v| *v > 0.0) {
                    let v = (px - entry) * q * sign;
                    kit::kv(ui, l("Est. PnL"), format!("{} USDT ({:+.2}%)", signed(v), (px / entry.max(1e-12) - 1.0) * sign * 100.0), if v >= 0.0 { up() } else { dn() });
                }
            };
            if !sh.partial {
                apply |= kit::field(ui, l("Take profit"), &mut sh.tp, "USDT", "stp", false).1;
                pnl(ui, parse(&sh.tp), qty);
                apply |= kit::field(ui, l("Stop loss"), &mut sh.sl, "USDT", "ssl", false).1;
                pnl(ui, parse(&sh.sl), qty);
                kit::note(ui, l("Closes the whole position at market when triggered, including size added later."), mu());
            } else {
                let mut remove = None;
                for (i, lv) in sh.levels.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if pill(ui, l("TP"), lv.tp).clicked() { lv.tp = true; }
                        if pill(ui, l("SL"), !lv.tp).clicked() { lv.tp = false; }
                        ui.add_space(4.0);
                        ui.allocate_ui(egui::vec2(170.0, 32.0), |ui| kit::field(ui, l("Price"), &mut lv.price, "", ("lvp", i), false));
                        ui.allocate_ui(egui::vec2(92.0, 32.0), |ui| kit::field(ui, l("Size"), &mut lv.amount, "", ("lva", i), false));
                        if pill(ui, "%", lv.pct).clicked() && !lv.pct { lv.pct = true; lv.amount.clear(); }
                        if pill(ui, &base, !lv.pct).on_hover_text(l("Size as % of the position, or in base units")).clicked() && lv.pct { lv.pct = false; lv.amount.clear(); }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if close_x(ui, H_SM).clicked() { remove = Some(i); }
                        });
                    });
                    if let (Some(px), Some(q)) = (parse(&lv.price), qty_of(lv)) {
                        kit::kv(ui, &format!("{} {base}", fmt_qty(q)), format!("{} USDT", signed((px - entry) * q * sign)), if (px - entry) * sign >= 0.0 { up() } else { dn() });
                    }
                }
                if let Some(i) = remove { sh.levels.remove(i); }
                if sh.levels.len() < 10 {
                    ui.horizontal(|ui| {
                        if kit::button(ui, &format!("+ {}", l("Take-profit level")), Look::Secondary, H, None, true).clicked() { sh.levels.push(Level { tp: true, price: String::new(), amount: String::new(), pct: true }); }
                        if kit::button(ui, &format!("+ {}", l("Stop-loss level")), Look::Secondary, H, None, true).clicked() { sh.levels.push(Level { tp: false, price: String::new(), amount: String::new(), pct: true }); }
                    });
                }
                kit::note(ui, l("Each level closes its size when triggered. Take-profit levels and stop levels each add up to at most the position."), mu());
            }
            if !existing.is_empty() {
                caption(ui, l("Active"));
                kit::group(ui, |ui| {
                    tight(ui);
                    for r in &existing {
                        let tp = r["take_profit"] == true;
                        let above = tp == long;
                        ui.horizontal(|ui| {
                            kit::chip(ui, if tp { l("TP") } else { l("SL") }, if tp { Tone::Up } else { Tone::Down });
                            ui.label(RichText::new(format!("{} {} {}", if s(&r["trigger"]) == "last" { l("Last") } else { l("Mark") }, if above { "≥" } else { "≤" }, fmt_px(f(&r["trigger_px"]).unwrap_or(0.0)))).font(mono_m(12.5)).color(fg()));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if kit::button(ui, l("Cancel"), Look::Danger, H_SM, None, true).clicked() { cancel = Some(r.clone()); }
                                ui.label(RichText::new(f(&r["qty"]).map(|q| format!("{} {base}", fmt_qty(q))).unwrap_or(l("Entire position").into())).font(mono(12.0)).color(mu()));
                            });
                        });
                    }
                });
            }
            if let Some(e) = &sh.err { kit::note(ui, e, dn()); }
            let (c, o) = dialog_buttons(ui, l("Close"), l("Confirm"), accent(), true);
            if c { done = true; }
            if o { apply = true; }
        });
        if let Some(r) = cancel {
            if let Err(e) = call(json!({"op": "cancel_tpsl", "ex": r["ex"], "id": r["id"]})) { sh.err = Some(e); }
        }
        if apply {
            sh.err = None;
            let (rf, ref_name) = (mark, l("the mark price"));
            let base_args = json!({"op": "set_tpsl", "ex": sh.ex, "symbol": sh.symbol, "pos": sh.side, "trigger": if sh.last { "last" } else { "mark" }});
            let mut calls = vec![];
            if !sh.partial {
                let (tp, sl) = (parse(&sh.tp), parse(&sh.sl));
                if tp.is_none() && sl.is_none() { sh.err = Some(l("Enter a take-profit or stop-loss price").into()); return; }
                if let Some(e) = tpsl_check(long, rf, ref_name, tp, sl) { sh.err = Some(e); return; }
                let mut a = base_args.clone();
                if let Some(v) = tp { a["tp"] = v.into(); }
                if let Some(v) = sl { a["sl"] = v.into(); }
                calls.push(a);
            } else {
                if sh.levels.is_empty() { sh.err = Some(l("Add at least one level").into()); return; }
                let (mut st_tp, mut st_sl) = (0.0, 0.0);
                for (i, lv) in sh.levels.iter().enumerate() {
                    let tag = format!("{} {}: ", l("Level"), i + 1);
                    let Some(px) = parse(&lv.price) else { sh.err = Some(tag + l("enter a price")); return };
                    let Some(q) = qty_of(lv).filter(|q| *q > 0.0) else { sh.err = Some(tag + l("size is zero after rounding")); return };
                    if let Some(e) = tpsl_check(long, rf, ref_name, lv.tp.then_some(px), (!lv.tp).then_some(px)) { sh.err = Some(tag + &e); return; }
                    if lv.tp { st_tp += q } else { st_sl += q }
                    let mut a = base_args.clone();
                    a[if lv.tp { "tp" } else { "sl" }] = px.into();
                    a["partial_qty"] = q.into();
                    calls.push(a);
                }
                let cap = qty * (1.0 + 1e-9);
                if st_tp > cap { sh.err = Some(l("Take-profit levels add up to more than the position").into()); return; }
                if st_sl > cap { sh.err = Some(l("Stop-loss levels add up to more than the position").into()); return; }
            }
            let n = calls.len();
            for (i, a) in calls.into_iter().enumerate() {
                if let Err(e) = call(a) { sh.err = Some(format!("{i} / {n} {}. {e}", l("sent"))); return; }
            }
            done = true;
        }
        if done { *sheet = None; }
}

fn ms(v: Option<f64>) -> String { v.map(|x| format!("{x:.0}ms")).unwrap_or("–".into()) }

/// Algo status strings come from the engine at runtime; `l` wants a static key.
/// ponytail: leaks one copy per distinct status text (a handful of fixed strings in practice).
pub(crate) fn leak(s: &str) -> &'static str {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut v = SEEN.lock().unwrap();
    if let Some(x) = v.iter().find(|x| **x == s) { return x; }
    let x: &'static str = Box::leak(s.to_string().into_boxed_str());
    v.push(x);
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_parse_like_the_swift_panel() {
        assert_eq!(parse("1,234.5"), Some(1234.5));
        assert_eq!(parse("0,5"), None);
        assert_eq!(parse(" 83500 "), Some(83500.0));
        assert_eq!(qty_str(0.123456789, Some(0.001)), "0.123");
        assert_eq!(px_str(83500.12, Some(0.1)), "83500.1");
        assert_eq!(lf("%d %@ from %.1f", &["3".into(), "BTC".into(), "1.5".into()]), "3 BTC from 1.5");
    }
}
