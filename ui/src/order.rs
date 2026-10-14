//! Order entry for the egui app: a port of the SwiftUI host's OrderPanel, OrderConfirm,
//! LeverageSheet and OrderTpSl. It reads the same state document (`App::state_json`) and sends the
//! same ops (`App::call`), so routing, validation and placement are identical in both hosts.
//! Prices always come from the trade venue's own book (`trade.bid/ask`), never the composite.
use super::{fmt_dp, fmt_px, fmt_qty, l, lf, theme::*};
use eframe::egui::{self, Color32, RichText, Ui};
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

/// The TP/SL sheet of one position (whole position or staged levels).
struct TpSlSheet { ex: String, symbol: String, side: String, partial: bool, tp: String, sl: String, last: bool, levels: Vec<Level>, err: Option<String> }

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

fn usd(v: f64) -> String { fmt_dp(v, 2) }
fn signed(v: f64) -> String { format!("{}{}", if v >= 0.0 { "+" } else { "" }, fmt_dp(v, 2)) }
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

// ---- widgets ----

/// Bordered input with the label inside on the left, right-aligned digits and a unit; Enter
/// returns true.
fn field(ui: &mut Ui, label: &str, text: &mut String, unit: &str, id: &str) -> (egui::Response, bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let eid = ui.id().with(("field", id));
    let focused = ui.memory(|m| m.has_focus(eid));
    ui.painter().rect(rect, 5, panel2(), egui::Stroke::new(1.0, if focused { accent() } else { line() }), egui::StrokeKind::Inside);
    let lw = ui.painter().text(rect.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, label, prop(12.0), dim()).width();
    let uw = ui.painter().text(rect.right_center() - egui::vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, unit, prop(11.5), mu()).width();
    let edit = egui::Rect::from_min_max(egui::pos2(rect.left() + lw + 18.0, rect.top() + 4.0), egui::pos2(rect.right() - uw - 18.0, rect.bottom() - 4.0));
    let r = ui.put(edit, egui::TextEdit::singleline(text).id(eid).frame(egui::Frame::NONE).font(mono(13.5)).horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center));
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    (r, enter)
}

/// Full-width segmented control.
fn segments<T: PartialEq + Copy>(ui: &mut Ui, sel: &mut T, items: &[(T, &str)]) -> bool {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 6, panel2());
    let w = rect.width() / items.len() as f32;
    let mut changed = false;
    for (i, (v, name)) in items.iter().enumerate() {
        let r = egui::Rect::from_min_size(egui::pos2(rect.left() + w * i as f32, rect.top()), egui::vec2(w, rect.height())).shrink(2.0);
        let resp = ui.interact(r, ui.id().with(("seg", name, i)), egui::Sense::click());
        let on = *sel == *v;
        if on { ui.painter().rect(r, 5, hl(), egui::Stroke::new(1.0, edge()), egui::StrokeKind::Inside); }
        else if resp.hovered() { ui.painter().rect_filled(r, 5, hl().linear_multiply(0.5)); }
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, *name, prop(12.5), if on { fg() } else { mu() });
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { *sel = *v; changed = true; }
    }
    changed
}

/// Size slider 0..=1 with notches at quarters; returns true when moved.
fn slider(ui: &mut Ui, v: &mut f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width() - 44.0, 22.0), egui::Sense::click_and_drag());
    let track = egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.center().y - 2.0), egui::pos2(rect.right() - 6.0, rect.center().y + 2.0));
    let p = ui.painter();
    p.rect_filled(track, 2, hl());
    let x = track.left() + track.width() * v.clamp(0.0, 1.0);
    p.rect_filled(egui::Rect::from_min_max(track.min, egui::pos2(x, track.max.y)), 2, accent());
    for q in 0..=4 {
        let c = egui::pos2(track.left() + track.width() * q as f32 / 4.0, track.center().y);
        let on = *v >= q as f32 / 4.0 - 1e-4;
        p.add(egui::Shape::convex_polygon(vec![c + egui::vec2(0.0, -5.0), c + egui::vec2(5.0, 0.0), c + egui::vec2(0.0, 5.0), c + egui::vec2(-5.0, 0.0)],
            if on { accent() } else { panel2() }, egui::Stroke::new(1.5, if on { accent() } else { dim() })));
    }
    p.circle(egui::pos2(x, track.center().y), 7.0, fg(), egui::Stroke::new(2.0, accent()));
    p.text(egui::pos2(rect.right() + 42.0, rect.center().y), egui::Align2::RIGHT_CENTER, format!("{:.0}%", *v * 100.0), mono(11.5), mu());
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.clicked()) {
        let mut x = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
        for q in [0.0, 0.25, 0.5, 0.75, 1.0] { if (x - q).abs() < 0.03 { x = q; } }
        *v = x;
        return true;
    }
    false
}

fn big_button(ui: &mut Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let col = if !enabled { fill.linear_multiply(0.3) } else if resp.is_pointer_button_down_on() { fill.linear_multiply(0.8) } else if resp.hovered() { fill.gamma_multiply(1.15) } else { fill };
    ui.painter().rect_filled(rect, 6, col);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, prop(14.5), if enabled { Color32::WHITE } else { Color32::from_white_alpha(110) });
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn chip(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(11.5), if enabled { fg() } else { mu() });
    let (r, resp) = ui.allocate_exact_size(g.size() + egui::vec2(16.0, 8.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    ui.painter().rect(r, 4, if resp.hovered() && enabled { hl() } else { panel2() }, egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside);
    ui.painter().galley(r.center() - g.size() / 2.0, g, if enabled { fg() } else { mu() });
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn kv(ui: &mut Ui, k: &str, v: String, col: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(k).font(prop(11.5)).color(dim()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(v).font(mono(11.5)).color(col)); });
    });
}

fn note(ui: &mut Ui, text: &str, col: Color32) {
    ui.add(egui::Label::new(RichText::new(text).font(prop(11.0)).color(col)).wrap());
}

/// Card: rounded panel with a hairline, like the native glass cards.
fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new().fill(panel2()).stroke(egui::Stroke::new(1.0, line())).corner_radius(10).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        add(ui)
    }).inner
}

fn checkbox(ui: &mut Ui, on: &mut bool, text: &str) -> bool { ui.checkbox(on, RichText::new(text).font(prop(12.0))).changed() }

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
        {
            card(ui, |ui| {
                self.header(ui, t, &venue, smart, has_key, call);
                if segments(ui, &mut self.close, &[(false, l("Open")), (true, l("Close"))]) { (self.pct, self.qty, self.error) = (0.0, String::new(), None); }
                self.type_tabs(ui, caps);
                ui.separator();
                // available margin, with the transfer shortcut
                ui.horizontal(|ui| {
                    ui.label(RichText::new(l("Avbl")).font(prop(12.0)).color(dim()));
                    ui.label(RichText::new(f(&t["available"]).map(|a| format!("{} USDT", usd(a))).unwrap_or("–".into())).font(mono(12.0)));
                    if has_key && link(ui, "⇄").on_hover_text(l("Move funds between accounts on this venue")).clicked() { self.open_transfer = Some(venue.clone()); }
                });
                enter |= self.kind_fields(ui, t, &venue, &bbo_levels, use_bbo, tick);
                let (r, e) = field(ui, l("Size"), &mut self.qty, &base, "qty");
                enter |= e;
                if r.changed() { self.pct = if max_qty > 0.0 { (parse(&self.qty).unwrap_or(0.0) / max_qty).min(1.0) as f32 } else { 0.0 }; }
                ui.horizontal(|ui| {
                    if slider(ui, &mut self.pct) { self.qty = if self.pct > 0.0 { qty_str(max_qty * self.pct as f64, step) } else { String::new() }; }
                }).response.on_hover_text(if self.close { l("Share of the position") } else { l("Share of available margin × leverage") });
                ui.separator();
                if !self.close && matches!(self.kind, Kind::Limit | Kind::Market) { enter |= self.tpsl_section(ui); }
                self.algo_jobs(ui, st, call);
                // the two sides, with what each can open / close and the margin it costs
                let (l1, l2) = if self.close { (l("Close Short"), l("Close Long")) } else { (l("Open Long"), l("Open Short")) };
                let (p1, p2) = if self.close { ("short", "long") } else { ("long", "short") };
                let mut go = None;
                ui.columns(2, |c| {
                    if big_button(&mut c[0], l1, up(), can_trade) { go = Some(p1); }
                    if big_button(&mut c[1], l2, dn(), can_trade) { go = Some(p2); }
                    for (i, pos) in [(0, p1), (1, p2)] {
                        let cu = &mut c[i];
                        cu.spacing_mut().item_spacing.y = 2.0;
                        if self.close { kv(cu, l("Max"), format!("{} {base}", fmt_qty(held(pos))), mu()); }
                        else {
                            kv(cu, l("Cost"), format!("{} USDT", usd(qty_v * ref_px / lev)), mu());
                            kv(cu, l("Max"), format!("{} {base}", fmt_qty(max_qty)), mu());
                        }
                    }
                });
                if read_only { note(ui, &format!("🔒 {}", l("This API key is read-only: trading is disabled. Allow trading for the key on the exchange to place orders.")), WARN); }
                if let Some(p) = go { self.submit(p, st, ref_px, use_bbo, typed_price, call); }
                self.summary(ui, t, &base, qty_v * ref_px, use_bbo, has_key, qty_v);
                if let Some(e) = &self.error { note(ui, &format!("⛔ {e}"), dn()); }
                let mine: Vec<&Waiting> = self.waiting.iter().filter(|w| w.symbol == symbol).collect();
                if !mine.is_empty() {
                    ui.label(RichText::new(l("TP/SL waiting for fill")).font(prop(11.5)).color(dim()));
                    let mut drop = None;
                    for (i, w) in mine.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} · TP {} · SL {}", w.ex, w.tp.map(fmt_px).unwrap_or("–".into()), w.sl.map(fmt_px).unwrap_or("–".into()))).font(mono(11.0)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if link(ui, "✕").on_hover_text(l("Forget this TP/SL")).clicked() { drop = Some(i); } });
                        });
                    }
                    if let Some(i) = drop { let id = (mine[i].ex.clone(), mine[i].until); self.waiting.retain(|w| (w.ex.clone(), w.until) != id); }
                }
            });
            ui.add_space(8.0);
            for p in &positions { self.position_card(ui, p, st, call); ui.add_space(8.0); }
            self.account_card(ui, st, t, &venue, has_key);
        }
        if enter {
            match self.last_pos.clone() {
                Some(p) => self.submit(&p, st, ref_px, use_bbo, typed_price, call),
                None => self.error = Some(l("Press Buy or Sell once; Enter then repeats that side.").into()),
            }
        }
        self.confirm_modal(ui, st, call);
        self.algo_modal(ui, t, call);
        self.leverage_modal(ui, t, call);
        self.tpsl_modal(ui, st, call);
    }

    /// Position mode, leverage (opens the sheet), untested warning, routing / venue menu.
    fn header(&mut self, ui: &mut Ui, t: &Value, venue: &str, smart: bool, has_key: bool, call: &mut Call) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let Some(ex) = depth::Exchange::parse(venue) {
                let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                icon(ui.painter(), ex, r, false);
            }
            // mode and leverage are only known with keys
            if has_key {
                let mode = match s(&t["mode"]) { "hedge" => Some(l("Hedge")), "" => None, _ => Some(l("One-Way")) };
                if let Some(m) = mode { chip(ui, m, false).on_hover_text(l("Position mode for this symbol (change it on the exchange)")); }
                let lv = f(&t["lev"]).map(|v| format!("{v}x")).unwrap_or(l("Leverage").into());
                if chip(ui, &lv, true).on_hover_text(l("Change leverage for this symbol")).clicked() {
                    self.lev = Some(f(&t["lev"]).unwrap_or(10.0));
                    let _ = call(json!({"op": "leverage_max"}));
                }
            }
            if t["verified"] == false { ui.label(RichText::new("⚠").color(WARN)).on_hover_text(l("Order placement on this venue is not yet verified on a live account: start with the minimum size.")); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = if smart { format!("⑂ {}", l("Smart")) } else { format!("{venue} ▾") };
                ui.menu_button(RichText::new(name).font(prop(12.5)).color(accent()), |ui| {
                    if ui.button(l("Order routing…")).clicked() { self.open_settings = Some(2); ui.close(); }
                    if ui.button(l("API keys…")).clicked() { self.open_settings = Some(3); ui.close(); }
                }).response.on_hover_text(format!("{} {} · {} {}", l("Push"), ms(f(&t["push_ms"])), l("RTT"), ms(f(&t["rtt_ms"]))));
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
            let w = ui.available_width() - 90.0;
            ui.allocate_ui(egui::vec2(w, 28.0), |ui| {
                // a More type selected: no segment is lit; clicking one switches back
                let mut k = (!in_more).then_some(self.kind);
                let items: Vec<(Option<Kind>, &str)> = main.iter().map(|(k, n)| (Some(*k), *n)).collect();
                if segments(ui, &mut k, &items) { if let Some(k) = k { self.kind = k; } }
            });
            // "More" in the same 28px pill as the segments beside it
            let label = if in_more { more.iter().find(|m| m.0 == self.kind).map_or(l("More"), |m| m.1) } else { l("More") };
            let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
            ui.painter().rect(r, 6, if in_more { hl() } else if resp.hovered() { hl().linear_multiply(0.6) } else { panel2() }, egui::Stroke::new(1.0, if in_more { accent() } else { line() }), egui::StrokeKind::Inside);
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, format!("{label} ▾"), prop(12.0), if in_more { accent() } else { fg() });
            egui::Popup::menu(&resp.on_hover_cursor(egui::CursorIcon::PointingHand)).show(|ui| {
                for (k, name) in &more { if ui.selectable_label(self.kind == *k, *name).clicked() { self.kind = *k; ui.close(); } }
            });
        });
    }

    /// The kind-specific inputs between "Avbl" and the size; true when Enter was pressed.
    fn kind_fields(&mut self, ui: &mut Ui, t: &Value, venue: &str, bbo_levels: &[u64], use_bbo: bool, _tick: Option<f64>) -> bool {
        let mut enter = false;
        match self.kind {
            Kind::Limit | Kind::Market => {
                if self.kind == Kind::Market {
                    let mut m = l("Market price").to_string();
                    ui.add_enabled_ui(false, |ui| field(ui, l("Price"), &mut m, "", "mkt"));
                } else {
                    ui.horizontal(|ui| {
                        let w = ui.available_width() - if bbo_levels.is_empty() { 0.0 } else { 54.0 };
                        ui.allocate_ui(egui::vec2(w, 30.0), |ui| {
                            if use_bbo {
                                let name = |q: bool, lv: u64| format!("{} {lv}", if q { l("Queue") } else { l("Counterparty") });
                                ui.spacing_mut().interact_size.y = 30.0;
                                egui::ComboBox::from_id_salt("bbo_pick").width(w).selected_text(name(self.bbo_queue, self.bbo_level)).show_ui(ui, |ui| {
                                    for q in [false, true] {
                                        ui.label(RichText::new(if q { l("Queue") } else { l("Counterparty") }).small().color(dim()));
                                        for lv in bbo_levels { if ui.selectable_label(self.bbo_queue == q && self.bbo_level == *lv, name(q, *lv)).clicked() { self.bbo_queue = q; self.bbo_level = *lv; } }
                                    }
                                }).response.on_hover_text(if self.bbo_queue { l("Own side of the book: rests as a maker at the best price.") } else { l("Opposite side of the book: fills like a taker at the price the venue sees on arrival.") });
                            } else {
                                enter |= field(ui, l("Price"), &mut self.price, "USDT", "price").1;
                            }
                        });
                        if !bbo_levels.is_empty() {
                            let b = egui::Button::new(RichText::new("BBO").font(prop(11.5)).color(if self.bbo_on { accent() } else { mu() }))
                                .fill(if self.bbo_on { hl() } else { panel2() }).stroke(egui::Stroke::new(1.0, if self.bbo_on { accent() } else { line() })).corner_radius(5).min_size(egui::vec2(48.0, 30.0));
                            if ui.add(b).on_hover_text(l("Priced by the venue's book when the order arrives (Counterparty: opposite side; Queue: own side; number: book level)")).clicked() { self.bbo_on = !self.bbo_on; }
                        }
                    });
                    // the venue's own touch, click to use it
                    ui.horizontal(|ui| {
                        for (k, v, col) in [(l("Bid"), f(&t["bid"]), up()), (l("Ask"), f(&t["ask"]), dn())] {
                            let Some(v) = v else { continue };
                            if ui.add(egui::Button::new(RichText::new(format!("{k} {}", fmt_px(v))).font(mono(10.5)).color(col)).fill(panel2()).corner_radius(4)).clicked() && !use_bbo {
                                self.price = px_str(v, f(&t["tick"]));
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(l("Time in force")).font(prop(12.0)).color(dim()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let short = |k: &str| match k { "ioc" => "IOC", "fok" => "FOK", "post" => l("Post only"), _ => "GTC" };
                            ui.add_enabled_ui(!use_bbo, |ui| {
                                ui.menu_button(RichText::new(if use_bbo { "GTC" } else { short(self.tif) }).font(prop(12.0)), |ui| {
                                    let mut opts = vec![("gtc", l("GTC · good till cancelled")), ("ioc", l("IOC · fill what's there, cancel the rest"))];
                                    if t["caps"]["fok"] == true { opts.push(("fok", l("FOK · fill completely or cancel"))); }
                                    opts.push(("post", l("Post only (ALO) · maker or cancel")));
                                    for (k, name) in opts { if ui.selectable_label(self.tif == k, name).clicked() { self.tif = k; ui.close(); } }
                                });
                            }).response.on_hover_text(if use_bbo { l("BBO orders are good till cancelled") } else { l("How long the order may rest on the book") });
                        });
                    });
                }
            }
            Kind::Stop => {
                enter |= field(ui, l("Trigger price"), &mut self.trigger, "USDT", "trigger").1;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(l("Trigger by")).font(prop(12.0)).color(dim()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.selectable_value(&mut self.trigger_last, true, l("Last"));
                        ui.selectable_value(&mut self.trigger_last, false, l("Mark"));
                    });
                }).response.on_hover_text(l("Which price must reach the trigger: mark (harder to spike) or last trade"));
                checkbox(ui, &mut self.stop_limit, l("Limit order when triggered"));
                if self.stop_limit { enter |= field(ui, l("Limit price"), &mut self.price, "USDT", "stop_px").1; }
                note(ui, l("Fires once the trigger is reached: above the price it acts as a buy stop / sell take-profit, below it as a sell stop / buy take-profit."), mu());
            }
            Kind::Trailing => {
                ui.horizontal(|ui| {
                    ui.allocate_ui(egui::vec2(ui.available_width() - 40.0, 30.0), |ui| enter |= field(ui, l("Callback rate"), &mut self.callback, "%", "cb").1);
                    ui.menu_button("▾", |ui| { for v in ["0.5", "1.0", "2.0", "5.0"] { if ui.button(format!("{v}%")).clicked() { self.callback = v.into(); ui.close(); } } });
                });
                enter |= field(ui, l("Activation price (optional)"), &mut self.activation, "USDT", "act").1;
                note(ui, if venue == "Bybit" { l("Bybit trails an open position: use it from Close. It closes the whole position at market once price retraces by the callback from its best level.") }
                    else { l("A market order once price retraces by the callback from its best level since activation.") }, mu());
            }
            Kind::Scaled => {
                ui.columns(2, |c| {
                    enter |= field(&mut c[0], l("From price"), &mut self.scaled_from, "", "from").1;
                    enter |= field(&mut c[1], l("To price"), &mut self.scaled_to, "", "to").1;
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(l("Orders")).font(prop(12.0)).color(dim()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.add(egui::DragValue::new(&mut self.scaled_count).range(2..=50)); });
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(l("Size distribution")).font(prop(12.0)).color(dim()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(if self.scaled_skew < -0.05 { l("Larger first") } else if self.scaled_skew > 0.05 { l("Larger last") } else { l("Equal") }).font(prop(12.0)));
                    });
                });
                ui.add(egui::Slider::new(&mut self.scaled_skew, -1.0..=1.0).step_by(0.25).show_value(false));
                checkbox(ui, &mut self.scaled_post, l("Post only (maker or cancel)"));
            }
            Kind::Twap => {
                let range: Option<(i64, i64)> = t["native_twap"].as_array().filter(|a| a.len() == 2).map(|a| (a[0].as_i64().unwrap_or(0), a[1].as_i64().unwrap_or(0)));
                if let Some((lo, hi)) = range {
                    checkbox(ui, &mut self.twap_native, &lf("Run on %@ (keeps running when Depth is closed)", &[venue.into()]));
                    ui.label("").on_hover_text(lf("%@ slices the order itself; duration %d to %d minutes.", &[venue.into(), lo.to_string(), hi.to_string()]));
                }
                if range.is_some() && self.twap_native {
                    enter |= field(ui, l("Duration (min)"), &mut self.twap_minutes, "", "tm").1;
                    if venue == "Binance" {
                        enter |= field(ui, l("Price limit (optional)"), &mut self.twap_limit, "USDT", "tl").1;
                        note(ui, l("Binance runs the TWAP: at least 1,000 USDT notional, 5 minutes to 24 hours. Cancel it here or in the Binance app."), mu());
                    } else {
                        checkbox(ui, &mut self.twap_random, l("Randomize slice timing"));
                        note(ui, l("Hyperliquid runs the TWAP: a slice every 30 seconds, at most 3% slippage each. Cancel it here or on Hyperliquid."), mu());
                    }
                } else {
                    ui.columns(2, |c| {
                        enter |= field(&mut c[0], l("Duration (min)"), &mut self.twap_minutes, "", "tm").1;
                        enter |= field(&mut c[1], l("Slices"), &mut self.twap_slices, "", "ts").1;
                    });
                    ui.columns(2, |c| {
                        enter |= field(&mut c[0], l("Max slippage (bp)"), &mut self.twap_slip, "", "tsl").1;
                        enter |= field(&mut c[1], l("Price limit (optional)"), &mut self.twap_limit, "", "tl").1;
                    });
                    let gap = match (parse(&self.twap_minutes), parse(&self.twap_slices)) {
                        (Some(m), Some(n)) if m > 0.0 && n > 0.0 => { let sec = m * 60.0 / n; if sec >= 60.0 { format!("{:.1} min", sec / 60.0) } else { format!("{sec:.0} s") } }
                        _ => "–".into(),
                    };
                    note(ui, &lf("One IOC order every %@ at the venue's best price ± your slippage; slices wait while the price is beyond your limit. Keep this pair open while it runs.", &[gap]), mu());
                }
            }
        }
        enter
    }

    fn tpsl_section(&mut self, ui: &mut Ui) -> bool {
        let mut enter = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(l("TP/SL")).font(prop(12.5)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { toggle(ui, &mut self.tpsl_on); });
        }).response.on_hover_text(l("Attach a take-profit / stop-loss to the position this order opens"));
        if self.tpsl_on {
            segments(ui, &mut self.trig_last, &[(false, l("Mark")), (true, l("Last"))]);
            enter |= field(ui, l("Take profit"), &mut self.tp, "USDT", "tp").1;
            enter |= field(ui, l("Stop loss"), &mut self.sl, "USDT", "sl").1;
            note(ui, l("Applies to the whole position once the order fills."), mu());
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
        ui.label(RichText::new(l("Running algorithms")).font(prop(11.5)).color(dim()));
        for j in jobs {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("TWAP {} {}", if j["buy"] == true { l("buy") } else { l("sell") }, fmt_qty(f(&j["total"]).unwrap_or(0.0)))).strong());
                ui.label(RichText::new(format!("{} · {}", s(&j["ex"]), s(&j["symbol"]))).small().color(dim()));
                if j["cancelled"] != true && s(&j["status"]) != "done" {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if link(ui, l("Cancel")).clicked() { let _ = call(json!({"op": "cancel_algo", "id": j["id"]})); }
                    });
                }
            });
            let (started, end) = (f(&j["started_ms"]).unwrap_or(0.0), f(&j["end_ms"]).unwrap_or(0.0));
            if j["native"] == true {
                let frac = ((now - started) / (end - started).max(1.0)).clamp(0.0, 1.0) as f32;
                ui.add(egui::ProgressBar::new(frac).desired_height(4.0));
                ui.label(RichText::new(format!("{} · {}", l(leak(s(&j["status"]))), if frac >= 1.0 { l("time elapsed: check fills in Trade History").to_string() } else { lf("%.0f%% of the time", &[format!("{:.0}", frac * 100.0)]) })).small().color(dim()));
            } else {
                let (done, slices) = (f(&j["done"]).unwrap_or(0.0), f(&j["slices"]).unwrap_or(1.0).max(1.0));
                ui.add(egui::ProgressBar::new((done / slices) as f32).desired_height(4.0));
                let status = s(&j["status"]);
                ui.label(RichText::new(format!("{done:.0}/{slices:.0} · {} {} · {}", fmt_qty(f(&j["sent"]).unwrap_or(0.0)), l("sent"), l(leak(status)))).small()
                    .color(if status.starts_with("waiting") || status.starts_with("failed") { WARN } else { dim() }));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn summary(&self, ui: &mut Ui, t: &Value, base: &str, notional: f64, use_bbo: bool, has_key: bool, qty_v: f64) {
        let taker = matches!(self.kind, Kind::Market | Kind::Twap | Kind::Trailing) || (self.kind == Kind::Stop && !self.stop_limit)
            || (use_bbo && !self.bbo_queue) || (self.kind == Kind::Limit && matches!(self.tif, "ioc" | "fok"));
        let rate = f(&t["fee"][if taker { "taker" } else { "maker" }]).unwrap_or(0.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {:.3}%", if taker { l("Taker") } else { l("Maker") }, rate * 100.0)).font(prop(11.0)).color(dim()));
            if notional > 0.0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} USDT · {} ≈ {}", usd(notional), l("fee"), fmt_dp(notional * rate, 3))).font(mono(11.0)).color(dim()));
                });
            }
        });
        if t["lev"].is_null() && has_key && !self.close { note(ui, &format!("⚠ {}", l("Leverage unknown: max size and cost assume 1x.")), WARN); }
        if let Some(m) = f(&t["min_qty"]).filter(|m| qty_v > 0.0 && qty_v < *m) { note(ui, &format!("⚠ {} {} {base}", l("Below the minimum size"), fmt_qty(m)), WARN); }
        else if let Some(m) = f(&t["min_notional"]).filter(|m| notional > 0.0 && notional < *m) { note(ui, &format!("⚠ {} {} USDT", l("Below the minimum order value"), usd(m)), WARN); }
    }

    /// This symbol's position on the trade venue: size, prices, uPnL / ROE, TP/SL and actions.
    fn position_card(&mut self, ui: &mut Ui, p: &Value, st: &Value, call: &mut Call) {
        let long = s(&p["side"]) == "long";
        let (qty, entry, mark, upnl, lev) = (f(&p["qty"]).unwrap_or(0.0), f(&p["entry"]).unwrap_or(0.0), f(&p["mark"]).unwrap_or(0.0), f(&p["upnl"]).unwrap_or(0.0), f(&p["lev"]).unwrap_or(0.0));
        let margin = f(&p["margin"]).filter(|m| *m > 0.0).unwrap_or(entry * qty / lev.max(1.0));
        let base = s(&st["base"]);
        let tpsl: Vec<&Value> = st["tpsl"].as_array().map(|a| a.iter().filter(|x| x["ex"] == p["ex"] && x["symbol"] == p["symbol"] && x["pos"] == p["side"]).collect()).unwrap_or_default();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(if long { l("Long") } else { l("Short") }).strong().color(if long { up() } else { dn() }));
                if lev > 0.0 { ui.label(RichText::new(format!("{lev}x")).font(mono(11.5)).color(dim())); }
                ui.label(RichText::new(format!("{} · {}", s(&p["ex"]), s(&p["symbol"]))).font(prop(11.5)).color(dim()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let c = if upnl >= 0.0 { up() } else { dn() };
                    ui.label(RichText::new(if margin > 0.0 { format!("{:+.2}%", upnl / margin * 100.0) } else { "–".into() }).font(mono(11.0)).color(c));
                    ui.label(RichText::new(signed(upnl)).font(mono(13.0)).strong().color(c)).on_hover_text(l("Unrealized PnL (USDT) and return on margin"));
                });
            });
            kv(ui, l("Size"), format!("{} {base}", fmt_qty(qty)), fg());
            kv(ui, l("Entry / Mark"), format!("{} / {}", fmt_px(entry), fmt_px(mark)), fg());
            kv(ui, l("Liq. price"), f(&p["liq"]).map(fmt_px).unwrap_or("–".into()), WARN);
            kv(ui, l("Margin"), format!("{} USDT", usd(margin)), fg());
            let one = |tp: bool| { let v: Vec<f64> = tpsl.iter().filter(|x| (x["take_profit"] == true) == tp).filter_map(|x| f(&x["trigger_px"])).collect(); if v.is_empty() { "–".to_string() } else { format!("{}{}", fmt_px(v[0]), if v.len() > 1 { format!(" +{}", v.len() - 1) } else { String::new() }) } };
            kv(ui, l("TP / SL"), format!("{} / {}", one(true), one(false)), fg());
            ui.columns(3, |c| {
                if c[0].add_sized([c[0].available_width(), 26.0], egui::Button::new(RichText::new(l("Market")).color(dn()))).on_hover_text(l("Close the whole position at market (asks for confirmation)")).clicked() {
                    // destructive: always confirmed, whatever the preference
                    let args = json!({"pos": p["side"], "close": true, "kind": "market", "qty": qty});
                    let mut req = args.clone();
                    req["op"] = "preview".into();
                    match call(req) {
                        Ok(plan) if plan["legs"].as_array().is_some_and(|a| !a.is_empty()) => self.ticket = Some(Ticket { pos: s(&p["side"]).into(), close: true, args, plan, tp: None, sl: None, trigger: "mark", err: None }),
                        Ok(_) => self.error = Some(l("No venue can take this order").into()),
                        Err(e) => self.error = Some(e),
                    }
                }
                if c[1].add_sized([c[1].available_width(), 26.0], egui::Button::new(l("Limit"))).on_hover_text(l("Fill the form with a limit close at the venue's best price")).clicked() {
                    // prefill: close tab, limit at the venue's own touch on the passive side
                    let t = &st["trade"];
                    (self.close, self.kind, self.bbo_on) = (true, Kind::Limit, false);
                    if let Some(px) = f(&t[if long { "ask" } else { "bid" }]) { self.price = px_str(px, f(&t["tick"])); }
                    self.qty = qty_str(qty, f(&t["step"]));
                    self.last_pos = Some(s(&p["side"]).into());
                }
                if c[2].add_sized([c[2].available_width(), 26.0], egui::Button::new(l("TP/SL"))).on_hover_text(l("Whole-position or staged take-profit / stop-loss")).clicked() {
                    self.sheet = Some(TpSlSheet { ex: s(&p["ex"]).into(), symbol: s(&p["symbol"]).into(), side: s(&p["side"]).into(), partial: false, tp: String::new(), sl: String::new(), last: false,
                        levels: vec![Level { tp: true, price: String::new(), amount: "50".into(), pct: true }, Level { tp: false, price: String::new(), amount: "100".into(), pct: true }], err: None });
                }
            });
        });
    }

    /// The trade venue's account: margin health, equity, uPnL, keys shortcut.
    fn account_card(&mut self, ui: &mut Ui, st: &Value, t: &Value, venue: &str, has_key: bool) {
        let b = st["balances"].as_array().and_then(|a| a.iter().find(|b| s(&b["ex"]) == venue));
        if b.is_none() && has_key { return; }
        card(ui, |ui| {
            let Some(b) = b else {
                ui.label(RichText::new(format!("🔑 {}", l("No API key for this venue"))).color(WARN));
                if ui.button(l("Add API keys")).clicked() { self.open_settings = Some(3); }
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(l("Account")).strong().font(prop(13.5)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if link(ui, &format!("⇄ {}", l("Transfer"))).on_hover_text(l("Move funds between this venue's accounts")).clicked() { self.open_transfer = Some(venue.to_string()); }
                });
            });
            ui.separator();
            if let Some(u) = f(&b["uni_mmr"]) {
                kv(ui, l("UniMMR"), fmt_dp(u, 2), if u < 1.5 { dn() } else if u < 3.0 { WARN } else { up() });
            } else if let Some(r) = f(&b["mm_rate"]) {
                kv(ui, l("Margin ratio"), format!("{:.2}%", r * 100.0), if r > 0.7 { dn() } else if r > 0.4 { WARN } else { up() });
            }
            if let Some(m) = f(&b["maint_margin"]) { kv(ui, l("Maintenance margin"), format!("{} USD", usd(m)), fg()); }
            if let Some(e) = f(&b["adj_equity"]) { kv(ui, l("Adjusted equity"), format!("{} USD", usd(e)), fg()); }
            kv(ui, l("Equity"), format!("{} USD", usd(f(&b["equity"]).unwrap_or(0.0))), fg());
            let upnl: f64 = st["positions"].as_array().map(|a| a.iter().filter(|p| s(&p["ex"]) == venue).filter_map(|p| f(&p["upnl"])).sum()).unwrap_or(0.0);
            kv(ui, l("Unrealized PnL"), format!("{} USD", signed(upnl)), if upnl > 0.0 { up() } else if upnl < 0.0 { dn() } else { fg() });
            if !has_key && ui.button(l("Add API keys")).clicked() { self.open_settings = Some(3); }
            if let Some(e) = t["error"].as_str() { note(ui, &format!("⛔ {e}"), dn()); }
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
        let m = egui::Modal::new(egui::Id::new("order_confirm")).show(ui.ctx(), |ui| {
            ui.set_width(440.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{base}USDT")).strong().font(prop(15.0)));
                ui.label(RichText::new(l("Perp")).color(dim()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(tk.title()).strong().color(col)); });
            });
            ui.separator();
            let legs = tk.plan["legs"].as_array().cloned().unwrap_or_default();
            ui.label(RichText::new(if legs.len() > 1 { format!("{} · {} {}", l("Route"), legs.len(), l("legs")) } else { l("Route").into() }).color(dim()));
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for lg in &legs {
                    let buy = (s(&lg["pos"]) == "long") != (lg["close"] == true);
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(s(&lg["ex"])).strong());
                            ui.label(RichText::new(if buy { l("Buy") } else { l("Sell") }).strong().color(if buy { up() } else { dn() }));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(format!("{} {base}", fmt_qty(f(&lg["qty"]).unwrap_or(0.0)))).font(mono(12.0))); });
                        });
                        ui.columns(2, |c| {
                            kv(&mut c[0], l("Order"), kind_text(s(&lg["kind"])), fg());
                            kv(&mut c[1], l("Est. price"), fmt_px(f(&lg["est_px"]).unwrap_or(0.0)), fg());
                            kv(&mut c[0], l("Est. fee"), fmt_dp(f(&lg["est_fee"]).unwrap_or(0.0), 3), mu());
                            let slip = f(&lg["slip_bps"]).unwrap_or(0.0);
                            kv(&mut c[1], l("Slippage"), format!("{slip:.1} bp"), if slip > 5.0 { WARN } else { mu() });
                        });
                        let after = f(&lg["avail_after"]).unwrap_or(0.0);
                        kv(ui, l("Available after"), format!("{} USDT", usd(after)), if after < 0.0 { dn() } else { mu() });
                    });
                }
                if let Some(ex) = tk.plan["excluded"].as_array().filter(|a| !a.is_empty()) {
                    ui.label(RichText::new(l("Not used")).color(dim()));
                    for x in ex { kv(ui, s(&x["ex"]), s(&x["reason"]).into(), mu()); }
                }
            });
            ui.separator();
            let (total, vwap) = (f(&tk.plan["total_qty"]).unwrap_or(0.0), f(&tk.plan["vwap"]).unwrap_or(0.0));
            if let Some(c) = f(&tk.plan["clamped_from"]) {
                note(ui, &format!("⚠ {} {} {} {} {base} ({})", l("Size reduced from"), fmt_qty(c), l("to"), fmt_qty(total), if tk.close { l("position size") } else { l("margin limit") }), WARN);
            }
            kv(ui, l("Size"), format!("{} {base}", fmt_qty(total)), fg());
            kv(ui, l("Est. average price"), fmt_px(vwap), fg());
            kv(ui, l("Order value"), format!("{} USDT", usd(total * vwap)), fg());
            kv(ui, l("Est. fees"), format!("{} USDT", fmt_dp(f(&tk.plan["fees"]).unwrap_or(0.0), 3)), fg());
            if tk.tp.is_some() || tk.sl.is_some() {
                note(ui, &format!("🎯 {} {} · {} {} ({}): {}", l("TP"), tk.tp.map(fmt_px).unwrap_or("–".into()), l("SL"), tk.sl.map(fmt_px).unwrap_or("–".into()),
                    if tk.trigger == "last" { l("Last") } else { l("Mark") }, l("set for the whole position once the order has filled and the position exists.")), accent());
            }
            let unverified = legs.iter().any(|lg| st["keys"].as_array().is_some_and(|k| k.iter().any(|k| k["ex"] == lg["ex"] && k["verified"] == false)));
            if unverified { note(ui, &format!("⚠ {}", l("Order placement on this venue is not yet verified on a live account: start with the minimum size.")), WARN); }
            note(ui, l("Prices and sizes are rounded to the venue's tick and step."), mu());
            if let Some(e) = &tk.err { note(ui, &format!("⛔ {e}"), dn()); }
            ui.add_space(6.0);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 34.0], egui::Button::new(l("Cancel"))).clicked() || c[0].input(|i| i.key_pressed(egui::Key::Escape)) { done = Some(false); }
                if big_button(&mut c[1], &format!("{} {}", l("Confirm"), tk.title()), col, true) || c[1].input(|i| i.key_pressed(egui::Key::Enter)) { done = Some(true); }
            });
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
        egui::Modal::new(egui::Id::new("algo_confirm")).show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new(&a.title).strong().font(prop(15.0)));
            note(ui, &a.message, fg());
            if t["verified"] == false { note(ui, &format!("⚠ {}", l("This venue is untested: start with the minimum size.")), WARN); }
            ui.add_space(6.0);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 32.0], egui::Button::new(l("Cancel"))).clicked() { done = Some(false); }
                if big_button(&mut c[1], l("Confirm"), accent(), true) { done = Some(true); }
            });
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
        egui::Modal::new(egui::Id::new("leverage")).show(ui.ctx(), |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new(l("Leverage")).strong().font(prop(15.0)));
            kv(ui, l("Symbol"), format!("{} · {}", s(&t["symbol"]), s(&t["venue"])), fg());
            kv(ui, l("Current"), f(&t["lev"]).map(|v| format!("{v}x")).unwrap_or("–".into()), fg());
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(l("Leverage"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label("x"); ui.add(egui::DragValue::new(&mut lev).range(1.0..=max.unwrap_or(200.0)).speed(0.5).fixed_decimals(0)); });
            });
            match max {
                Some(m) => {
                    ui.add(egui::Slider::new(&mut lev, 1.0..=m.max(1.0)).step_by(1.0).show_value(false));
                    ui.horizontal_wrapped(|ui| {
                        for v in [1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0, 125.0].into_iter().filter(|v| *v <= m) {
                            if ui.selectable_label(lev == v, format!("{v}x")).clicked() { lev = v; }
                        }
                    });
                }
                None => { ui.horizontal(|ui| { ui.spinner(); ui.label(RichText::new(l("Reading the venue's limit…")).color(dim())); }); }
            }
            note(ui, l("Changes the setting on the exchange for this symbol (both sides). Higher leverage moves the liquidation price closer; the venue may refuse it for a large position."), mu());
            ui.add_space(6.0);
            let ok = lev >= 1.0 && max.is_none_or(|m| lev <= m) && Some(lev.round()) != f(&t["lev"]);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 32.0], egui::Button::new(l("Cancel"))).clicked() { done = Some(false); }
                if big_button(&mut c[1], &lf("Set %gx", &[format!("{}", lev.round())]), accent(), ok) { done = Some(true); }
            });
        });
        self.lev = Some(lev);
        match done {
            Some(true) => { if let Err(e) = call(json!({"op": "set_leverage", "leverage": lev.round() as i64})) { self.error = Some(e); } self.lev = None; }
            Some(false) => self.lev = None,
            None => {}
        }
    }

    /// TP/SL for an existing position: whole position, or staged levels (one `set_tpsl` with
    /// `partial_qty` each); lists its live TP/SL with cancel.
    fn tpsl_modal(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let Some(sh) = self.sheet.as_mut() else { return };
        let Some(p) = st["positions"].as_array().and_then(|a| a.iter().find(|p| s(&p["ex"]) == sh.ex && s(&p["symbol"]) == sh.symbol && s(&p["side"]) == sh.side)).cloned() else { self.sheet = None; return };
        let long = sh.side == "long";
        let sign = if long { 1.0 } else { -1.0 };
        let (qty, entry, mark) = (f(&p["qty"]).unwrap_or(0.0), f(&p["entry"]).unwrap_or(0.0), f(&p["mark"]).unwrap_or(0.0));
        let base = sh.symbol.trim_end_matches("USDT").to_string();
        let step = if sh.symbol == s(&st["trade"]["symbol"]) { f(&st["trade"]["step"]) } else { None };
        let qty_of = |lv: &Level| parse(&lv.amount).filter(|a| *a > 0.0).and_then(|a| parse(&qty_str(if lv.pct { qty * a.min(100.0) / 100.0 } else { a }, step)));
        let existing: Vec<Value> = st["tpsl"].as_array().map(|a| a.iter().filter(|x| s(&x["ex"]) == sh.ex && s(&x["symbol"]) == sh.symbol && s(&x["pos"]) == sh.side).cloned().collect()).unwrap_or_default();
        let (mut done, mut apply, mut cancel) = (false, false, None);
        egui::Modal::new(egui::Id::new("tpsl_sheet")).show(ui.ctx(), |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} · {}", sh.ex, sh.symbol)).strong().font(prop(15.0)));
                ui.label(RichText::new(l("TP/SL")).color(dim()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(if long { l("Long") } else { l("Short") }).strong().color(if long { up() } else { dn() })); });
            });
            kv(ui, l("Size"), format!("{} {base}", fmt_qty(qty)), fg());
            kv(ui, l("Entry / Mark"), format!("{} / {}", fmt_px(entry), fmt_px(mark)), fg());
            kv(ui, l("Liq. price"), f(&p["liq"]).map(fmt_px).unwrap_or("–".into()), WARN);
            ui.separator();
            segments(ui, &mut sh.partial, &[(false, l("Entire position")), (true, l("Partial (staged)"))]);
            segments(ui, &mut sh.last, &[(false, l("Mark")), (true, l("Last"))]);
            let pnl = |ui: &mut Ui, px: Option<f64>, q: f64| {
                if let Some(px) = px.filter(|v| *v > 0.0) {
                    let v = (px - entry) * q * sign;
                    kv(ui, l("Est. PnL"), format!("{} USDT ({:+.2}%)", signed(v), (px / entry.max(1e-12) - 1.0) * sign * 100.0), if v >= 0.0 { up() } else { dn() });
                }
            };
            if !sh.partial {
                apply |= field(ui, l("Take profit"), &mut sh.tp, "USDT", "stp").1;
                pnl(ui, parse(&sh.tp), qty);
                apply |= field(ui, l("Stop loss"), &mut sh.sl, "USDT", "ssl").1;
                pnl(ui, parse(&sh.sl), qty);
                note(ui, l("Closes the whole position at market when triggered, including size added later."), mu());
            } else {
                let mut remove = None;
                for (i, lv) in sh.levels.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut lv.tp, true, l("TP"));
                        ui.selectable_value(&mut lv.tp, false, l("SL"));
                        ui.add(egui::TextEdit::singleline(&mut lv.price).hint_text(l("Price")).desired_width(130.0).font(mono(12.5)).horizontal_align(egui::Align::RIGHT));
                        ui.add(egui::TextEdit::singleline(&mut lv.amount).hint_text(l("Size")).desired_width(70.0).font(mono(12.5)).horizontal_align(egui::Align::RIGHT));
                        if ui.selectable_label(lv.pct, "%").clicked() && !lv.pct { lv.pct = true; lv.amount.clear(); }
                        if ui.selectable_label(!lv.pct, &base).on_hover_text(l("Size as % of the position, or in base units")).clicked() && lv.pct { lv.pct = false; lv.amount.clear(); }
                        if link(ui, "⊖").clicked() { remove = Some(i); }
                    });
                    if let (Some(px), Some(q)) = (parse(&lv.price), qty_of(lv)) {
                        kv(ui, &format!("{} {base}", fmt_qty(q)), format!("{} USDT", signed((px - entry) * q * sign)), if (px - entry) * sign >= 0.0 { up() } else { dn() });
                    }
                }
                if let Some(i) = remove { sh.levels.remove(i); }
                if sh.levels.len() < 10 {
                    ui.horizontal(|ui| {
                        if ui.button(format!("+ {}", l("Take-profit level"))).clicked() { sh.levels.push(Level { tp: true, price: String::new(), amount: String::new(), pct: true }); }
                        if ui.button(format!("+ {}", l("Stop-loss level"))).clicked() { sh.levels.push(Level { tp: false, price: String::new(), amount: String::new(), pct: true }); }
                    });
                }
                note(ui, l("Each level closes its size when triggered. Take-profit levels and stop levels each add up to at most the position."), mu());
            }
            if !existing.is_empty() {
                ui.separator();
                ui.label(RichText::new(l("Active")).color(dim()));
                for r in &existing {
                    let tp = r["take_profit"] == true;
                    let above = tp == long;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(if tp { l("TP") } else { l("SL") }).strong().color(if tp { up() } else { dn() }));
                        ui.label(RichText::new(format!("{} {} {}", if s(&r["trigger"]) == "last" { l("Last") } else { l("Mark") }, if above { "≥" } else { "≤" }, fmt_px(f(&r["trigger_px"]).unwrap_or(0.0)))).font(mono(12.0)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(RichText::new(l("Cancel")).color(dn())).clicked() { cancel = Some(r.clone()); }
                            ui.label(RichText::new(f(&r["qty"]).map(|q| format!("{} {base}", fmt_qty(q))).unwrap_or(l("Entire position").into())).color(dim()));
                        });
                    });
                }
            }
            if let Some(e) = &sh.err { note(ui, &format!("⛔ {e}"), dn()); }
            ui.add_space(6.0);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 32.0], egui::Button::new(l("Close"))).clicked() { done = true; }
                if big_button(&mut c[1], l("Confirm"), accent(), true) { apply = true; }
            });
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
        if done { self.sheet = None; }
    }
}

fn ms(v: Option<f64>) -> String { v.map(|x| format!("{x:.0}ms")).unwrap_or("–".into()) }

/// Algo status strings come from the engine at runtime; `l` wants a static key.
/// ponytail: leaks one copy per distinct status text (a handful of fixed strings in practice).
fn leak(s: &str) -> &'static str {
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
