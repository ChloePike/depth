//! Order entry for the egui app: a port of the SwiftUI host's OrderPanel, OrderConfirm,
//! LeverageSheet and OrderTpSl. It reads the same state document (`App::state_json`) and sends the
//! same ops (`App::call`), so routing, validation and placement are identical in both hosts.
//! Prices always come from the trade venue's own book (`trade.bid/ask`), never the composite.
use super::kit::{self, Look, Tone, H, H_LG, H_SM};
use super::{fmt_dp, fmt_px, fmt_qty, l, lf, theme::*};
use eframe::egui::{self, Align2, Color32, Rect, Response, RichText, Sense, Stroke, Ui};
use crate::hotkeys::Act;
use serde_json::{json, Value};

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
#[derive(Clone)]
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
    sheet: Option<TpSlSheet>,
    /// last symbol / venue / book click seen, to react to changes
    seen: (String, String, Option<(String, f64)>),
    /// set when the panel wants the settings dialog: 2 = trading, 3 = API keys
    pub open_settings: Option<u8>,
    /// set when the panel wants the transfer dialog for this venue
    pub open_transfer: Option<String>,
    /// the order being typed, for the chart: (price, field it came from, tag, sub, colour); `tick` formats a dragged price
    pub(crate) preview: Vec<(f64, &'static str, &'static str, String, Color32)>,
    tick: Option<f64>,
    /// positions waiting for the flatten hotkey's confirmation
    flatten: Option<Vec<Value>>,
    /// size typed in USDT (notional) instead of the base coin
    qty_usdt: bool,
    /// the keyboard dial: moves the limit price (else the size), and a size step waiting for the panel
    dial_price: bool,
    dial_size: bool,
    /// when this panel last sent something: order-log entries since then show under the buttons
    sent_ms: i64,
    /// the last routed order sent, for "retry remainder"
    last: Option<Ticket>,
}

impl Default for OrderPanel {
    fn default() -> Self {
        OrderPanel {
            close: false, kind: Kind::Limit, price: String::new(), qty: String::new(), pct: 0.0, bbo_on: false, bbo_queue: false, bbo_level: 1,
            tpsl_on: false, tp: String::new(), sl: String::new(), trig_last: false, last_pos: None, error: None, ticket: None, algo: None, tif: "gtc",
            trigger: String::new(), trigger_last: false, stop_limit: false, callback: "1.0".into(), activation: String::new(),
            scaled_from: String::new(), scaled_to: String::new(), scaled_count: 5, scaled_skew: 0.0, scaled_post: true,
            twap_minutes: "15".into(), twap_slices: "10".into(), twap_slip: "10".into(), twap_limit: String::new(), twap_native: true, twap_random: false,
            lev: None, sheet: None, seen: Default::default(), open_settings: None, open_transfer: None, preview: vec![], tick: None, flatten: None, qty_usdt: false, dial_price: false, dial_size: false, sent_ms: 0, last: None,
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

/// "Sent, waiting for the venue…" until the first answer to what a panel sent at `sent_ms` (`log`:
/// the order log, newest first); the answers themselves are toasts (`log_toast`).
pub(crate) fn results(ui: &mut Ui, sent_ms: i64, log: &[(i64, &str, bool)]) {
    if sent_ms == 0 || depth::now_ms() - sent_ms > 20_000 || log.iter().any(|x| x.0 >= sent_ms) { return; }
    kit::note(ui, l("Sent, waiting for the venue…"), mu());
}

/// One order log line as a toast: "OK Bybit BTCUSDT open long 0.01 limit 81900 id 123 (45 ms)" ->
/// "Bybit BTCUSDT open long 0.01 limit 81900 · 45 ms"; a failure keeps the venue's error.
pub(crate) fn log_toast(m: &str, ok: bool) -> String {
    if !ok { return m.strip_prefix("FAIL ").unwrap_or(m).to_string(); }
    let body = m.strip_prefix("OK ").unwrap_or(m);
    let (what, rest) = body.split_once(" id ").unwrap_or((body, ""));
    match rest.rsplit_once('(').map(|(_, t)| t.trim_end_matches(')')) { Some(ms) => format!("{what} · {ms}"), None => what.to_string() }
}

/// Market-closes each position (the venue's own reduce-only close); the last error is kept.
fn flatten(ps: &[Value], call: &mut Call, err: &mut Option<String>) {
    for p in ps {
        if let Err(e) = call(json!({"op": "close_position", "ex": p["ex"], "symbol": p["symbol"], "side": p["side"]})) { *err = Some(e); }
    }
}

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
    ui.painter().rect_filled(r, kit::rad(), fill);
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
    if on { ui.painter().rect_stroke(r.rect, kit::rad(), Stroke::new(1.0, accent()), egui::StrokeKind::Inside); }
    r
}

/// Thin progress bar (3 px, accent on the track grey).
fn progress(ui: &mut Ui, frac: f32, col: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), Sense::hover());
    ui.painter().rect_filled(r, 1, track());
    ui.painter().rect_filled(Rect::from_min_size(r.min, egui::vec2(r.width() * frac.clamp(0.0, 1.0), r.height())), 1, col);
}

/// Dialog frame: popup surface, radius 6, hairline, soft shadow, 20 px padding.
pub(crate) fn dialog(id: &str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(id)).backdrop_color(Color32::from_black_alpha(140)).frame(egui::Frame::new().fill(popup_bg()).corner_radius(6)
        .stroke(Stroke::new(1.0, line())).inner_margin(20).shadow(egui::Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(110) }))
}

/// Dialog title row: 16 semibold, a grey subtitle beside it, an optional tag on the right.
pub(crate) fn dialog_title(ui: &mut Ui, title: &str, sub: &str, tag: Option<(&str, Tone)>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(title).font(prop_s(16.0)).color(fg_strong()));
        if !sub.is_empty() { ui.label(RichText::new(sub).font(prop(12.5)).color(mu())); }
        if let Some((t, tone)) = tag { ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::chip(ui, t, tone); }); }
    });
}

/// Dialog footer: Cancel (secondary) and the primary action side by side; returns (cancel, ok).
pub(crate) fn dialog_buttons(ui: &mut Ui, cancel: &str, ok: &str, col: Color32, enabled: bool) -> (bool, bool) {
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

/// The two sides' facts under the buy / sell buttons: (label, buy-side value, sell-side value, tip).
/// One "label .... value" row each; when the sides differ the value is "buy / sell", each in its
/// button's colour. Only if that cannot fit beside the label do the values drop to a second line,
/// still right-aligned, so the column never breaks into a stagger.
fn side_facts(ui: &mut Ui, rows: &[(&str, String, String, &str)]) {
    for (k, a, b, tip) in rows {
        let font = mono_m(11.5);
        let mut job = egui::text::LayoutJob::default();
        let fmt = |c: Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
        if a == b { job.append(a, 0.0, fmt(fg())); } else {
            job.append(a, 0.0, fmt(up()));
            job.append("  /  ", 0.0, fmt(dim()));
            job.append(b, 0.0, fmt(dn()));
        }
        let g = ui.painter().layout_job(job);
        let kw = ui.painter().layout_no_wrap(k.to_string(), prop(11.5), mu()).size().x;
        let two = kw + g.size().x + 12.0 > ui.available_width();
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), if two { 32.0 } else { 17.0 }), Sense::hover());
        if !tip.is_empty() { resp.on_hover_text(*tip); }
        let p = ui.painter();
        let y0 = if two { r.top() + 8.0 } else { r.center().y };
        let y1 = if two { r.bottom() - 8.0 } else { r.center().y };
        p.text(egui::pos2(r.left(), y0), Align2::LEFT_CENTER, *k, prop(11.5), mu());
        p.galley(egui::pos2(r.right() - g.size().x, y1 - g.size().y / 2.0), g, fg());
    }
}

/// Account-wide liquidation price of one symbol: equity `a0` and maintenance margin `mm0` now,
/// gross position in the symbol before the order (`gross0`), net / gross after (`net`, `gross`) the
/// order at price `r`, maintenance rate `mmr`; the account goes when equity / MM falls to `k`.
/// Solves a0 + net (P - r) = k (mm0 - gross0 r mmr + gross P mmr). None when no price on the
/// losing side reaches it (flat, or a hedge that does not lose).
#[allow(clippy::too_many_arguments)]
fn uni_liq(a0: f64, mm0: f64, gross0: f64, net: f64, gross: f64, r: f64, mmr: f64, k: f64) -> Option<f64> {
    let mm_other = (mm0 - gross0 * r * mmr).max(0.0);
    let den = net - k * gross * mmr;
    // flat (fully hedged): equity does not move with the price
    if den.abs() < 1e-12 || net.abs() < 1e-12 { return None; }
    let p = (k * mm_other - a0 + net * r) / den;
    (p > 0.0 && p.is_finite() && if net > 0.0 { p < r } else { p > r }).then_some(p)
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
        // a ticket opened by Enter in a field this frame must not be confirmed by that same Enter
        let had_ticket = self.ticket.is_some();
        let (venue, symbol, base) = (s(&t["venue"]).to_string(), s(&t["symbol"]).to_string(), s(&st["base"]).to_string());
        let (tick, step) = (f(&t["tick"]), f(&t["step"]));
        let (bid, ask) = (f(&t["bid"]), f(&t["ask"]));
        let smart = st["route"]["smart"].as_bool().unwrap_or(false);
        let has_key = t["has_key"].as_bool().unwrap_or(false);
        // a read-only key can show the account but never send an order
        let read_only = t["read_only"] == true && !smart;
        let can_trade = (has_key && !read_only) || smart;
        let caps = &t["caps"];
        // TP/SL only where the venue has it (smart routing then skips the venues without)
        let tpsl_ok = caps["tpsl"] == true || smart;
        let bbo_levels: Vec<u64> = t["bbo_levels"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();

        // react to a new symbol, venue or book click (the Swift onChange handlers)
        let click = st["book_click"].as_object().map(|c| (s(&c["ex"]).to_string(), c["px"].as_f64().unwrap_or(0.0)));
        if self.seen.0 != symbol {
            self.price = bid.map(|b| px_str(b, tick)).unwrap_or_default();
            (self.qty, self.pct, self.tp, self.sl, self.error, self.last_pos) = (String::new(), 0.0, String::new(), String::new(), None, None);
        }
        if self.seen.1 != venue && ((self.kind == Kind::Stop && caps["stop"] != true) || (self.kind == Kind::Trailing && caps["trailing"] != true)) { self.kind = Kind::Limit; }
        if click != self.seen.2 {
            if let Some((ex, px)) = &click { if *ex == venue { self.kind = Kind::Limit; self.bbo_on = false; self.price = px_str(*px, tick); } }
        }
        self.seen = (symbol.clone(), venue.clone(), click);
        if self.price.is_empty() { if let Some(b) = bid { self.price = px_str(b, tick); } }
        if !bbo_levels.contains(&self.bbo_level) { self.bbo_level = bbo_levels.first().copied().unwrap_or(1); }

        let use_bbo = self.kind == Kind::Limit && self.bbo_on && !bbo_levels.is_empty();
        let typed_price = (self.kind == Kind::Limit && !use_bbo) || (self.kind == Kind::Stop && self.stop_limit);
        let mid = match (bid, ask) { (Some(b), Some(a)) => Some((b + a) / 2.0), _ => bid.or(ask) };
        let ref_px = match self.kind {
            Kind::Stop => (if self.stop_limit { parse(&self.price) } else { None }).or(parse(&self.trigger)).or(mid).unwrap_or(0.0),
            Kind::Scaled => match (parse(&self.scaled_from), parse(&self.scaled_to)) { (Some(a), Some(b)) => (a + b) / 2.0, _ => mid.unwrap_or(0.0) },
            _ => (if typed_price { parse(&self.price) } else { None }).or(mid).unwrap_or(0.0),
        };
        let lev = f(&t["lev"]).unwrap_or(1.0).max(1.0);
        let qty_v = self.base_qty(ref_px, step);
        let positions: Vec<&Value> = st["positions"].as_array().map(|a| a.iter().filter(|p| s(&p["symbol"]) == symbol && (smart || s(&p["ex"]) == venue)).collect()).unwrap_or_default();
        let held = |side: &str| positions.iter().filter(|p| s(&p["side"]) == side).map(|p| f(&p["qty"]).unwrap_or(0.0)).sum::<f64>();
        let max_qty = if self.close { held("long").max(held("short")) } else if ref_px > 0.0 { f(&t["available"]).unwrap_or(0.0) * lev / ref_px } else { 0.0 };

        // preview lines: the prices this order would rest / trigger at, with the size and the distance from the touch
        self.tick = tick;
        let away = |p: f64| mid.map(|m| format!("{:+.2}%", (p / m - 1.0) * 100.0)).unwrap_or_default();
        let sub = |p: f64| if qty_v > 0.0 { format!("{}@{} · {}", fmt_qty(qty_v), fmt_px(p), away(p)) } else { format!("@{} · {}", fmt_px(p), away(p)) };
        let mut pv = vec![];
        let mut add = |field: &'static str, tag: &'static str, v: &str, col: Color32| if let Some(p) = parse(v).filter(|p| *p > 0.0) { pv.push((p, field, tag, sub(p), col)); };
        match self.kind {
            // the price field is pre-filled with the bid: no line until a size is typed
            Kind::Limit if !use_bbo && qty_v > 0.0 => add("price", l("Limit"), &self.price, accent()),
            Kind::Stop => {
                add("trigger", l("Stop"), &self.trigger, accent());
                if self.stop_limit { add("price", l("Limit"), &self.price, accent()); }
            }
            Kind::Scaled => { add("from", l("Scaled"), &self.scaled_from, accent()); add("to", l("Scaled"), &self.scaled_to, accent()); }
            _ => {}
        }
        if self.tpsl_on && tpsl_ok && !self.close && matches!(self.kind, Kind::Limit | Kind::Market) { add("tp", "TP", &self.tp, up()); add("sl", "SL", &self.sl, dn()); }
        self.preview = pv;

        let mut enter = false;
        card(ui, |ui| {
            self.header(ui, st, &venue, smart, has_key, call);
            // the venue's own touch on its own row: bid left, spread centre, ask right; a click
            // puts that price in the limit field
            if let (Some(b), Some(k)) = (bid, ask) {
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::hover());
                ui.painter().rect_filled(r, kit::rad(), panel2());
                let half = r.width() / 2.0;
                for (i, (lab, v, col)) in [(l("Bid"), b, up()), (l("Ask"), k, dn())].into_iter().enumerate() {
                    let cell = Rect::from_min_size(egui::pos2(r.left() + half * i as f32, r.top()), egui::vec2(half, r.height()));
                    let resp = ui.interact(cell, ui.id().with(("touch", i)), Sense::click());
                    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
                    if hv > 0.0 { ui.painter().rect_filled(cell.shrink(2.0), kit::rad(), hl().gamma_multiply(0.7 * hv)); }
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
            if kit::segments(ui, &mut self.close, &[(false, l("Open")), (true, l("Close"))], 28.0) { (self.pct, self.qty, self.error, self.last_pos) = (0.0, String::new(), None, None); }
            self.type_tabs(ui, caps);
            // available margin, with the transfer shortcut
            ui.horizontal(|ui| {
                ui.set_min_height(18.0);
                ui.label(RichText::new(l("Available")).font(prop(12.0)).color(mu()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if has_key && t["demo"] != true && kit::button(ui, "↔", Look::Link, 18.0, Some(18.0), true).on_hover_text(l("Move funds between accounts on this venue")).clicked() { self.open_transfer = Some(venue.clone()); }
                    ui.label(RichText::new(f(&t["available"]).map(|a| format!("{} USDT", usd(a))).unwrap_or("–".into())).font(mono_m(12.0)).color(fg()));
                });
            });
            enter |= self.kind_fields(ui, t, &venue, &bbo_levels, use_bbo, tick);
            // size in the coin or in USDT: the unit button switches and converts what is typed
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (r, e) = ui.allocate_ui(egui::vec2(ui.available_width() - 60.0, H), |ui| kit::field(ui, l("Size"), &mut self.qty, "", "qty", false)).inner;
                enter |= e;
                if r.changed() { self.pct = if max_qty > 0.0 { (self.base_qty(ref_px, step) / max_qty).min(1.0) as f32 } else { 0.0 }; }
                let unit = if self.qty_usdt { "USDT".to_string() } else { base.clone() };
                if kit::button(ui, &format!("{unit} ⇄"), Look::Secondary, H, Some(f32::INFINITY), true).on_hover_text(l("Size in the coin or in USDT")).clicked() {
                    let b = self.base_qty(ref_px, step);
                    self.qty_usdt = !self.qty_usdt;
                    self.qty = if b > 0.0 { self.qty_text(b, ref_px, step) } else { String::new() };
                }
            });
            if std::mem::take(&mut self.dial_size) { self.qty = if self.pct > 0.0 { self.qty_text(max_qty * self.pct as f64, ref_px, step) } else { String::new() }; }
            let pct = format!("{:.0}%", self.pct * 100.0);
            ui.scope(|ui| {
                if slider(ui, &mut self.pct, &pct) { self.qty = if self.pct > 0.0 { self.qty_text(max_qty * self.pct as f64, ref_px, step) } else { String::new() }; }
            }).response.on_hover_text(if self.close { l("Share of the position") } else { l("Share of available margin × leverage") });
            if !self.close && matches!(self.kind, Kind::Limit | Kind::Market) && (tpsl_ok || self.tpsl_on) { enter |= self.tpsl_section(ui, tpsl_ok); }
            self.algo_jobs(ui, st, call);
            // the two sides, with what each can open / close and the margin it costs
            let (l1, l2) = if self.close { (l("Close Short"), l("Close Long")) } else { (l("Open Long"), l("Open Short")) };
            let (p1, p2) = if self.close { ("short", "long") } else { ("long", "short") };
            let mut go = None;
            if !has_key && !smart {
                // no key: the way forward instead of two dead buttons
                if kit::button(ui, l("Add API keys"), Look::Fill(accent()), 32.0, Some(f32::INFINITY), true).clicked() { self.open_settings = Some(3); }
            } else {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let w = (ui.available_width() - 8.0) / 2.0;
                    if kit::button(ui, l1, Look::Fill(up()), 32.0, Some(w), can_trade).clicked() { go = Some(p1); }
                    if kit::button(ui, l2, Look::Fill(dn()), 32.0, Some(w), can_trade).clicked() { go = Some(p2); }
                });
            }
            self.results(ui, st, call);
            // per side, Binance-style: "label value" pairs, the buy side flush left, the sell side
            // flush right; est. liquidation (isolated estimate), margin, and what can be opened
            ui.spacing_mut().item_spacing.y = 3.0;
            if self.close {
                side_facts(ui, &[(l("Max"), format!("{} {base}", fmt_qty(held(p1))), format!("{} {base}", fmt_qty(held(p2))), "")]);
            } else {
                // a unified / portfolio-margin account liquidates on the whole account: the price of
                // this symbol at which the account hits its threshold after the order, every other
                // position held where it is (uni_liq). Otherwise an isolated-margin estimate. Both use
                // a 0.5 % maintenance rate for this symbol and are marked ≈.
                let bal = st["balances"].as_array().and_then(|a| a.iter().find(|b| s(&b["ex"]) == venue));
                let acct_wide = bal.is_some_and(|b| !b["uni_mmr"].is_null() || !b["mm_rate"].is_null());
                let (l0, s0) = (held("long"), held("short"));
                let liq = |long: bool| {
                    if qty_v <= 0.0 || ref_px <= 0.0 { return "--".into(); }
                    let px = if let Some(b) = bal.filter(|_| acct_wide) {
                        // Binance PM liquidates at uniMMR 1.05, Bybit unified at a 100 % margin rate
                        let k = if b["uni_mmr"].is_null() { 1.0 } else { 1.05 };
                        let (Some(a0), Some(mm0)) = (f(&b["adj_equity"]), f(&b["maint_margin"])) else { return "--".into() };
                        let (l1, s1) = if long { (l0 + qty_v, s0) } else { (l0, s0 + qty_v) };
                        uni_liq(a0, mm0, l0 + s0, l1 - s1, l1 + s1, ref_px, 0.005, k)
                    } else {
                        let m = 1.0 / lev - 0.005;
                        Some(ref_px * if long { 1.0 - m } else { 1.0 + m })
                    };
                    px.map_or_else(|| "--".into(), |p| format!("≈{}", fmt_px(p)))
                };
                let liq_tip = if acct_wide { l("Unified account: the price at which the whole account would be liquidated after this order, other positions unchanged; estimated with a 0.5% maintenance rate.") }
                    else { l("Estimate for an isolated position at this leverage with a 0.5% maintenance rate; the venue's own figure can differ.") };
                let cost = format!("{} USDT", usd(qty_v * ref_px / lev));
                // max open is an estimate: compact (14.5K), the exact figure on hover
                let can = format!("{} USDT", super::chart::fmt_usd_short(max_qty * ref_px));
                let exact = format!("{} USDT", usd(max_qty * ref_px));
                let liq_tip = format!("{} / {} · {liq_tip}", l("Long"), l("Short"));
                side_facts(ui, &[(l("Est. liq. price"), liq(true), liq(false), &liq_tip), (l("Margin"), cost.clone(), cost, ""), (l("Max open"), can.clone(), can, &exact)]);
            }
            ui.spacing_mut().item_spacing.y = 6.0;
            if read_only { kit::note(ui, l("This API key is read-only: trading is disabled. Allow trading for the key on the exchange to place orders."), WARN); }
            if st["risk"]["blocked"] == true && !self.close { kit::note(ui, l("Daily loss stop reached: only closes until local midnight (Settings > Trading)."), dn()); }
            if let Some(p) = go { self.submit(p, st, ref_px, use_bbo, typed_price, call); }
            self.summary(ui, t, &base, qty_v * ref_px, use_bbo, has_key, qty_v);
            if let Some(e) = &self.error { kit::note(ui, e, dn()); }
            self.position_rows(ui, st, &positions, can_trade, call);
            // TP/SL the engine sets once their entry order fills (kept across restarts)
            let mine: Vec<&Value> = st["tpsl_wait"].as_array().map(|a| a.iter().filter(|w| s(&w["symbol"]) == symbol).collect()).unwrap_or_default();
            if !mine.is_empty() {
                kit::overline(ui, l("TP/SL waiting for fill"));
                for w in mine {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} · TP {} · SL {}", s(&w["ex"]), f(&w["tp"]).map(fmt_px).unwrap_or("–".into()), f(&w["sl"]).map(fmt_px).unwrap_or("–".into()))).font(mono_m(11.5)).color(fg()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if close_x(ui, 20.0).on_hover_text(l("Forget this TP/SL")).clicked() { let _ = call(json!({"op": "cancel_tpsl_wait", "ex": w["ex"], "client_id": w["client_id"]})); }
                        });
                    });
                }
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
        self.confirm_modal(ui, st, call, had_ticket);
        self.algo_modal(ui, t, call);
        self.leverage_modal(ui, t, call);
        self.flatten_modal(ui, call);
        tpsl_modal(&mut self.sheet, ui, st, call);
    }

    /// Position mode, leverage (opens the sheet), untested warning, routing / venue menu.
    fn header(&mut self, ui: &mut Ui, st: &Value, venue: &str, smart: bool, has_key: bool, call: &mut Call) {
        let t = &st["trade"];
        let st_keys = st["keys"].as_array().cloned().unwrap_or_default();
        let tradable: Vec<String> = st["tradable"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
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
            if t["demo"] == true { kit::chip(ui, "DEMO", Tone::Warn).on_hover_text(l("Simulated account: no real order is sent")); }
            if t["verified"] == false { kit::chip(ui, l("Untested"), Tone::Warn).on_hover_text(l("Order placement on this venue is not yet verified on a live account: start with the minimum size.")); }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = if smart { format!("{} ▾", l("Smart")) } else { format!("{venue} ▾") };
                let r = kit::button(ui, &name, Look::Link, H_SM, None, true)
                    .on_hover_text(format!("{} {} · {} {}", l("Push"), ms(f(&t["push_ms"])), l("RTT"), ms(f(&t["rtt_ms"]))));
                egui::Popup::menu(&r).show(|ui| {
                    // the venue itself, one click (the preferred venue when routing is smart)
                    kit::overline(ui, if smart { l("Preferred venue") } else { l("Trading venue") });
                    let keyed: Vec<&str> = st_keys.iter().filter(|k| k["configured"] == true).filter_map(|k| k["ex"].as_str()).collect();
                    for ex in tradable {
                        let label = if keyed.contains(&ex.as_str()) { ex.clone() } else { format!("{ex}  · {}", l("no key")) };
                        if ui.selectable_label(ex == venue, label).clicked() { let _ = call(json!({"op": "set_trade_venue", "ex": ex})); ui.close(); }
                    }
                    ui.separator();
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
            ui.spacing_mut().item_spacing.x = 4.0;
            // the More button fits its label ("Trailing Stop ▾" is wider than "More ▾"); the segments get the rest
            let label = format!("{} ▾", if in_more { more.iter().find(|m| m.0 == self.kind).map_or(l("More"), |m| m.1) } else { l("More") });
            let mw = ui.painter().layout_no_wrap(label.clone(), prop_s(12.5), fg()).size().x + 20.0;
            ui.allocate_ui(egui::vec2(ui.available_width() - mw - 4.0, 28.0), |ui| {
                // a More type selected: no segment is lit; clicking one switches back
                let mut k = (!in_more).then_some(self.kind);
                let items: Vec<(Option<Kind>, &str)> = main.iter().map(|(k, n)| (Some(*k), *n)).collect();
                if kit::segments(ui, &mut k, &items, 28.0) { if let Some(k) = k { self.kind = k; } }
            });
            let r = kit::button(ui, &label, Look::Secondary, 28.0, Some(mw), true);
            // selected: drawn like a lit segment, not an outlined (focus-looking) button
            if in_more {
                ui.painter().rect_filled(r.rect, kit::rad(), hl());
                ui.painter().text(r.rect.center(), Align2::CENTER_CENTER, &label, prop_s(12.5), fg_strong());
            }
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

    /// TP/SL switch and prices; where the venue has no TP/SL only the switch (to turn it off) and a note.
    fn tpsl_section(&mut self, ui: &mut Ui, ok: bool) -> bool {
        let mut enter = false;
        ui.horizontal(|ui| {
            ui.set_min_height(H_SM);
            ui.label(RichText::new(l("TP / SL")).font(prop_m(12.5)).color(fg()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::switch(ui, &mut self.tpsl_on); });
        }).response.on_hover_text(l("Attach a take-profit / stop-loss to the position this order opens"));
        if self.tpsl_on && !ok { kit::note(ui, l("TP/SL is not available on this venue yet"), WARN); }
        if self.tpsl_on && ok {
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
        });
        // order value and fee on their own row: beside the rates they collide in a narrow column
        if notional > 0.0 {
            ui.horizontal(|ui| {
                ui.label(RichText::new(l("Order value")).font(prop(11.0)).color(mu()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} USDT · {} ≈ {}", usd(notional), l("fee"), fmt_dp(notional * rate, 3))).font(mono_m(11.0)).color(fg()));
                });
            });
        }
        if t["lev"].is_null() && has_key && !self.close { kit::note(ui, l("Leverage unknown: max size and cost assume 1x."), WARN); }
        if let Some(m) = f(&t["min_qty"]).filter(|m| qty_v > 0.0 && qty_v < *m) { kit::note(ui, &format!("{} {} {base}", l("Below the minimum size"), fmt_qty(m)), WARN); }
        else if let Some(m) = f(&t["min_notional"]).filter(|m| notional > 0.0 && notional < *m) { kit::note(ui, &format!("{} {} USDT", l("Below the minimum order value"), usd(m)), WARN); }
    }

    /// A trading hotkey. Buy / sell `usdt` notional (on the Close tab: closing the short / long; the
    /// venue clamps a close to the position) at the venue's own touch (a limit joining the
    /// queue) or at market, through the same preview and confirmation as the buttons, TP/SL off;
    /// cancel every order of the symbol on that venue; close every position of the symbol on that venue at market.
    pub(crate) fn hotkey(&mut self, a: Act, usdt: f64, st: &Value, call: &mut Call) {
        if self.ticket.is_some() || self.algo.is_some() || self.flatten.is_some() { return; }
        let t = &st["trade"];
        let symbol = s(&t["symbol"]).to_string();
        // only the panel's trading venue: a hedge held on another venue is never touched
        let venue = s(&t["venue"]).to_string();
        let here = |x: &&Value| s(&x["symbol"]) == symbol && s(&x["ex"]) == venue;
        self.error = None;
        match a {
            Act::OpenClose => { self.close = !self.close; (self.pct, self.qty, self.last_pos) = (0.0, String::new(), None); }
            Act::DialMode => self.dial_price = !self.dial_price,
            Act::DialUp | Act::DialDown => {
                let up = a == Act::DialUp;
                if self.dial_price {
                    // one tick of the venue per detent, from the price shown (or the touch)
                    let tick = f(&t["tick"]).filter(|x| *x > 0.0).unwrap_or(0.01);
                    let p = parse(&self.price).or(f(&t["bid"])).unwrap_or(0.0);
                    if p > 0.0 { (self.kind, self.bbo_on) = (if self.kind == Kind::Market { Kind::Limit } else { self.kind }, false); self.price = px_str((p + if up { tick } else { -tick }).max(tick), f(&t["tick"])); }
                } else {
                    // 5 % of what can be opened / closed per detent; the panel turns it into a size next frame
                    self.pct = (self.pct + if up { 0.05 } else { -0.05 }).clamp(0.0, 1.0);
                    self.dial_size = true;
                }
            }
            Act::CancelAll => {
                self.sent_ms = depth::now_ms();
                for o in st["orders"].as_array().into_iter().flatten().filter(here).filter(|o| o["spot"] != true) {
                    if let Err(e) = call(json!({"op": "cancel", "ex": o["ex"], "id": o["id"]})) { self.error = Some(e); }
                }
            }
            Act::Flatten => {
                let ps: Vec<Value> = st["positions"].as_array().into_iter().flatten().filter(here).cloned().collect();
                if ps.is_empty() { self.error = Some(l("No position in this symbol").into()); }
                else if st["prefs"]["confirm"].as_bool().unwrap_or(true) { self.flatten = Some(ps); }
                else { self.sent_ms = depth::now_ms(); flatten(&ps, call, &mut self.error); }
            }
            _ => {
                if usdt <= 0.0 { self.error = Some(l("Set the hotkey order size in Settings > Hotkeys first.").into()); return; }
                let buy = matches!(a, Act::BuyBid | Act::BuyMarket);
                let market = matches!(a, Act::BuyMarket | Act::SellMarket);
                let (bid, ask) = (f(&t["bid"]), f(&t["ask"]));
                let px = if market { bid.zip(ask).map(|(b, k)| (b + k) / 2.0) } else if buy { bid } else { ask };
                let Some(px) = px.filter(|p| *p > 0.0) else { self.error = Some(l("No price from this venue yet").into()); return };
                // the panel's Open / Close tab decides: on Close, buy closes the short and sell the long
                (self.kind, self.bbo_on, self.tpsl_on, self.pct) = (if market { Kind::Market } else { Kind::Limit }, false, false, 0.0);
                if !market { self.price = px_str(px, f(&t["tick"])); }
                (self.qty_usdt, self.qty) = (false, qty_str(usdt / px, f(&t["step"])));
                let pos = if buy != self.close { "long" } else { "short" };
                self.submit(pos, st, px, false, !market, call);
            }
        }
    }

    /// The positions of this symbol (on the trading venue; every venue when routing is smart) right
    /// under the ticket, so the result of an order is visible where it was placed: side, size,
    /// entry, liquidation, uPnL / ROE; Close fills the form with a limit close of the whole
    /// position at the venue's touch, Market closes at market (confirmed when confirmations are on).
    fn position_rows(&mut self, ui: &mut Ui, st: &Value, positions: &[&Value], can_trade: bool, call: &mut Call) {
        if positions.is_empty() { return; }
        ui.add_space(4.0);
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top() - 2.0, Stroke::new(1.0, line()));
        kit::overline(ui, l("Position"));
        let mut act: Option<(Value, bool)> = None;
        for p in positions {
            let long = s(&p["side"]) == "long";
            let (upnl, margin) = (f(&p["upnl"]).unwrap_or(0.0), f(&p["margin"]).unwrap_or(0.0));
            let col = if upnl > 0.0 { up() } else if upnl < 0.0 { dn() } else { fg() };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let side = format!("{}{}", if long { l("Long") } else { l("Short") }, f(&p["lev"]).map(|v| format!(" {v:.0}x")).unwrap_or_default());
                kit::chip(ui, &side, if long { Tone::Up } else { Tone::Down });
                ui.label(RichText::new(format!("{} {}", fmt_qty(f(&p["qty"]).unwrap_or(0.0)), s(&st["base"]))).font(mono_s(12.5)).color(fg_strong()));
                if positions.iter().any(|q| q["ex"] != p["ex"]) { ui.label(RichText::new(s(&p["ex"])).font(prop(11.0)).color(dim())); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(signed(upnl)).font(mono_s(12.5)).color(col));
                });
            });
            // entry and liquidation on the left, ROE under the PnL
            ui.horizontal(|ui| {
                let liq = f(&p["liq"]).filter(|x| *x > 0.0).map(|x| format!("   {} {}", l("Liq."), fmt_px(x))).unwrap_or_default();
                ui.label(RichText::new(format!("{} {}{liq}", l("Entry"), f(&p["entry"]).map(fmt_px).unwrap_or("–".into()))).font(mono(11.0)).color(mu()));
                if margin > 0.0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{:+.2}%", upnl / margin * 100.0)).font(mono_m(11.0)).color(col));
                    });
                }
            });
            // limit close (fills the form) and market close, half the width each
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let w = (ui.available_width() - 8.0) / 2.0;
                if kit::button(ui, l("Limit close"), Look::Secondary, H_SM, Some(w), can_trade).on_hover_text(l("Fill the form with a limit close of the whole position at the touch")).clicked() { act = Some(((*p).clone(), false)); }
                if kit::button(ui, l("Market close"), Look::Danger, H_SM, Some(w), can_trade).on_hover_text(l("Close this position at market")).clicked() { act = Some(((*p).clone(), true)); }
            });
        }
        match act {
            Some((p, true)) if st["prefs"]["confirm"].as_bool().unwrap_or(true) => self.flatten = Some(vec![p]),
            Some((p, true)) => { self.sent_ms = depth::now_ms(); flatten(&[p], call, &mut self.error); }
            Some((p, false)) => self.prefill_close(&p, st),
            None => {}
        }
    }

    fn flatten_modal(&mut self, ui: &mut Ui, call: &mut Call) {
        let Some(ps) = &self.flatten else { return };
        let mut done = None;
        dialog("flatten_confirm").show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            dialog_title(ui, if ps.len() == 1 { l("Close this position at market?") } else { l("Close all positions of this symbol on the trading venue at market?") }, "", None);
            for p in ps {
                let long = s(&p["side"]) == "long";
                kit::kv(ui, &format!("{} · {}", s(&p["ex"]), if long { l("Long") } else { l("Short") }), fmt_qty(f(&p["qty"]).unwrap_or(0.0)), if long { up() } else { dn() });
            }
            let (c, o) = dialog_buttons(ui, l("Cancel"), l("Close at market"), dn(), true);
            if c || ui.input(|i| i.key_pressed(egui::Key::Escape)) { done = Some(false); }
            if o { done = Some(true); }
        });
        if let Some(go) = done {
            let ps = self.flatten.take().unwrap();
            if go { self.sent_ms = depth::now_ms(); flatten(&ps, call, &mut self.error); }
        }
    }

    /// The size field in base units: a USDT size is divided by the order's reference price and
    /// floored to the venue's step.
    fn base_qty(&self, ref_px: f64, step: Option<f64>) -> f64 {
        let v = parse(&self.qty).unwrap_or(0.0);
        if !self.qty_usdt { return v; }
        if ref_px <= 0.0 { return 0.0; }
        parse(&qty_str(v / ref_px, step)).unwrap_or(0.0)
    }

    /// The size field's text for `base` units, in the field's unit.
    fn qty_text(&self, base: f64, ref_px: f64, step: Option<f64>) -> String {
        if self.qty_usdt { format!("{:.2}", base * ref_px) } else { qty_str(base, step) }
    }

    /// For the keyboard lights: an order / flatten confirmation is on screen; the Close tab is chosen.
    pub(crate) fn waiting_confirm(&self) -> bool { self.ticket.is_some() || self.algo.is_some() || self.flatten.is_some() }
    pub(crate) fn closing(&self) -> bool { self.close }

    /// A preview line dragged on the chart: that field takes the new price.
    pub(crate) fn set_preview(&mut self, field: &str, px: f64) {
        let v = px_str(px, self.tick);
        match field { "price" => self.price = v, "trigger" => self.trigger = v, "from" => self.scaled_from = v, "to" => self.scaled_to = v, "tp" => self.tp = v, "sl" => self.sl = v, _ => {} }
    }

    /// Limit close of a position in the form: close tab, limit at the venue's own touch on the
    /// passive side, the whole size, and Enter repeats that side.
    pub(crate) fn prefill_close(&mut self, p: &Value, st: &Value) {
        let t = &st["trade"];
        (self.close, self.kind, self.bbo_on) = (true, Kind::Limit, false);
        if let Some(px) = f(&t[if s(&p["side"]) == "long" { "ask" } else { "bid" }]) { self.price = px_str(px, f(&t["tick"])); }
        (self.qty_usdt, self.qty) = (false, qty_str(f(&p["qty"]).unwrap_or(0.0), f(&t["step"])));
        self.last_pos = Some(s(&p["side"]).into());
    }

    /// A chart position tool's order in the form: open tab, limit at the entry, the size floored to the
    /// venue's step (kept as typed when unknown), TP at the target and SL at the stop where the venue
    /// takes TP/SL, and Enter places that side (long when the target is above the entry).
    pub(crate) fn prefill_position(&mut self, entry: f64, stop: f64, target: f64, qty: Option<f64>, st: &Value) {
        let t = &st["trade"];
        let (tick, ok) = (f(&t["tick"]), t["caps"]["tpsl"] == true || st["route"]["smart"] == true);
        (self.close, self.kind, self.bbo_on, self.tif, self.pct) = (false, Kind::Limit, false, "gtc", 0.0);
        self.price = px_str(entry, tick);
        if let Some(q) = qty { (self.qty_usdt, self.qty) = (false, qty_str(q, f(&t["step"]))); }
        (self.tpsl_on, self.trig_last, self.tp, self.sl) = (ok, false, px_str(target, tick), px_str(stop, tick));
        self.last_pos = Some(if target > entry { "long" } else { "short" }.into());
        self.error = (!ok).then(|| l("TP/SL is not available on this venue yet").into());
    }

    /// Under the buttons: the last routed order sent from here, one row per leg with its state (sent,
    /// OK, live, filled, failed, unknown), and the failed part to route again once nothing is unknown;
    /// other orders: "Sent, waiting" until the venue answers.
    fn results(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let plan = st["plans"].as_array().and_then(|a| a.iter().rev().find(|p| self.sent_ms > 0 && p["ts"].as_i64().is_some_and(|t| t >= self.sent_ms - 2_000))).cloned();
        let Some(p) = plan else {
            let log: Vec<(i64, &str, bool)> = st["log"].as_array().map(|a| a.iter().map(|x| (x["ts"].as_i64().unwrap_or(0), s(&x["msg"]), x["ok"] == true)).collect()).unwrap_or_default();
            return results(ui, self.sent_ms, &log);
        };
        let legs = p["legs"].as_array().cloned().unwrap_or_default();
        for lg in &legs {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} · {}", s(&lg["ex"]), fmt_qty(f(&lg["qty"]).unwrap_or(0.0)))).font(mono_m(11.5)).color(fg()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, tone) = match s(&lg["state"]) {
                        "ok" => (l("Accepted"), Tone::Up), "live" => (l("Live"), Tone::Up), "filled" => (l("Filled"), Tone::Up),
                        "failed" => (l("Failed"), Tone::Down), "unknown" => (l("No answer"), Tone::Warn), _ => (l("Sent"), Tone::Neutral),
                    };
                    let detail = s(&lg["detail"]);
                    let r = kit::chip(ui, text, tone);
                    if !detail.is_empty() { r.on_hover_text(detail); }
                });
            });
        }
        if legs.iter().any(|lg| lg["state"] == "unknown") { kit::note(ui, l("No answer from the venue for a leg: it may be live. It is looked up by its client id, never sent again."), WARN); }
        if let Some(q) = f(&p["remainder"]) {
            let text = lf("Retry remainder (%@ %@)", &[fmt_qty(q), s(&st["base"]).to_string()]);
            if kit::button(ui, &text, Look::Secondary, H_SM, Some(f32::INFINITY), true).on_hover_text(l("Routes the part that failed again on fresh books, through the preview and the confirmation")).clicked() {
                if let Some(mut tk) = self.last.clone() {
                    tk.args["qty"] = q.into();
                    self.sent_ms = 0;
                    self.open_ticket(tk, st, call);
                }
            }
        }
    }

    /// The trade venue's account: margin health, equity, uPnL, keys shortcut.
    fn account_card(&mut self, ui: &mut Ui, st: &Value, t: &Value, venue: &str, has_key: bool) {
        let b = st["balances"].as_array().and_then(|a| a.iter().find(|b| s(&b["ex"]) == venue));
        if b.is_none() && has_key { return; }
        card(ui, |ui| {
            let Some(b) = b else {
                kit::title(ui, l("No API key for this venue"));
                kit::note(ui, l("Market data works without keys. A key with trade permission enables the order panel; a read-only key shows the account."), mu());
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(l("Account")).font(prop_s(14.0)).color(fg_strong()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if t["demo"] != true && kit::button(ui, &format!("↔ {}", l("Transfer")), Look::Link, H_SM, None, true).on_hover_text(l("Move funds between this venue's accounts")).clicked() { self.open_transfer = Some(venue.to_string()); }
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
        let qty = self.base_qty(ref_px, f(&t["step"]));
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
                // a stop on the wrong side of the market triggers at once (venues reject it)
                let buy = (pos == "long") != self.close;
                if let (Some(b), Some(a)) = (f(&t["bid"]), f(&t["ask"])) {
                    if buy && tr <= a { self.error = Some(format!("{} ({})", l("A buy stop must trigger above the ask"), fmt_px(a))); return; }
                    if !buy && tr >= b { self.error = Some(format!("{} ({})", l("A sell stop must trigger below the bid"), fmt_px(b))); return; }
                }
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
        // the section is only shown for limit / market: a switch left on under another kind is ignored
        if self.tpsl_on && !self.close && matches!(self.kind, Kind::Limit | Kind::Market) {
            if t["caps"]["tpsl"] != true && st["route"]["smart"] != true { self.error = Some(l("TP/SL is not available on this venue yet").into()); return; }
            (tp, sl) = (parse(&self.tp), parse(&self.sl));
            if tp.is_none() && sl.is_none() { self.error = Some(l("Enter a take-profit or stop-loss price, or turn TP/SL off").into()); return; }
            // routing then uses only venues that take TP/SL
            args["tpsl"] = true.into();
            if let Some(e) = tpsl_check(pos == "long", ref_px, l("the order price"), tp, sl) { self.error = Some(e); return; }
        }
        let tk = Ticket { pos: pos.into(), close: self.close, args, plan: Value::Null, tp, sl, trigger: if self.trig_last { "last" } else { "mark" }, err: None };
        self.open_ticket(tk, st, call);
    }

    /// Preview `tk.args` (validated and routed on the books now), then the confirm modal, or place at
    /// once when confirmations are off.
    fn open_ticket(&mut self, mut tk: Ticket, st: &Value, call: &mut Call) {
        let mut req = tk.args.clone();
        req["op"] = "preview".into();
        let plan = match call(req) { Ok(p) => p, Err(e) => { self.error = Some(e); return } };
        let legs = plan["legs"].as_array().map_or(0, |a| a.len());
        if legs == 0 {
            let ex: Vec<String> = plan["excluded"].as_array().map(|a| a.iter().map(|x| format!("{}: {}", s(&x["ex"]), s(&x["reason"]))).collect()).unwrap_or_default();
            self.error = Some(if ex.is_empty() { l("No venue can take this order").into() } else { ex.join("\n") });
            return;
        }
        tk.plan = plan;
        if st["prefs"]["confirm"].as_bool().unwrap_or(true) { self.ticket = Some(tk); }
        else if let Err(e) = self.place(&tk, call) { self.error = Some(e); }
    }

    /// Send a previewed ticket; the place op attaches its TP/SL to the order or has the engine set it
    /// once the order filled.
    fn place(&mut self, tk: &Ticket, call: &mut Call) -> Result<(), String> {
        let mut args = tk.args.clone();
        args["op"] = "place".into();
        args["expect"] = tk.plan["legs"].as_array().map(|a| a.iter().map(|l| json!({"ex": l["ex"], "qty": l["qty"]})).collect::<Vec<_>>()).unwrap_or_default().into();
        if let Some(v) = tk.tp { args["tp"] = v.into(); }
        if let Some(v) = tk.sl { args["sl"] = v.into(); }
        // its own key: `trigger` is a stop order's trigger price
        args["tpsl_trigger"] = tk.trigger.into();
        call(args)?;
        self.sent_ms = depth::now_ms();
        self.last = Some(tk.clone());
        Ok(())
    }

    fn confirm_modal(&mut self, ui: &mut Ui, st: &Value, call: &mut Call, enter_ok: bool) {
        let Some(tk) = &self.ticket else { return };
        let base = s(&st["base"]).to_string();
        let col = if tk.buy() { up() } else { dn() };
        let mut done = None;
        let m = dialog("order_confirm").show(ui.ctx(), |ui| {
            ui.set_width(440.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            dialog_title(ui, &format!("{base}USDT"), l("Perp"), Some((tk.title(), if tk.buy() { Tone::Up } else { Tone::Down })));
            if st["demo"] == true { kit::note(ui, l("DEMO account: this order is simulated. No real order is sent."), WARN); }
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
                        // the leg's own price in its venue's quote (USD / USDC named); the average below is USDT
                        let q = s(&lg["quote"]);
                        let est = format!("{}{}", fmt_px(f(&lg["est_px"]).unwrap_or(0.0)), if q.is_empty() || q == "USDT" { String::new() } else { format!(" {q}") });
                        pair_row(ui, [(l("Order"), kind_text(s(&lg["kind"])), fg()), (l("Est. price"), est, fg())], true);
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
                kit::kv(ui, l("Est. average price"), format!("{} USDT", fmt_px(vwap)), fg_strong());
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
            let crosses = f(&tk.args["price"]).filter(|_| tk.args["kind"] == "limit").is_some_and(|p| {
                let t = &st["trade"];
                if tk.buy() { f(&t["ask"]).is_some_and(|a| p >= a) } else { f(&t["bid"]).is_some_and(|b| p <= b) }
            });
            if crosses { kit::note(ui, l("The limit price crosses the book: the order fills at once as a taker."), WARN); }
            if tk.tp.is_some() || tk.sl.is_some() {
                // per venue: attached to the order (the venue protects the fill), or set by Depth after the fill
                let (mut on, mut after): (Vec<&str>, Vec<&str>) = (vec![], vec![]);
                for lg in &legs {
                    let ex = s(&lg["ex"]);
                    let attach = depth::Exchange::parse(ex).is_some_and(|e| depth::trade::caps(e).attach_tpsl);
                    let v = if attach { &mut on } else { &mut after };
                    if !v.contains(&ex) { v.push(ex); }
                }
                if !on.is_empty() { kit::note(ui, &format!("TP / SL: {}", lf("placed with the order on %@: the venue protects the position from the fill on.", &[on.join(", ")])), mu()); }
                if !after.is_empty() { kit::note(ui, &format!("TP / SL: {}", lf("set by Depth on %@ once this order has filled (kept across restarts).", &[after.join(", ")])), mu()); }
            }
            let unverified = legs.iter().any(|lg| st["keys"].as_array().is_some_and(|k| k.iter().any(|k| k["ex"] == lg["ex"] && k["verified"] == false)));
            if unverified { kit::note(ui, l("Order placement on this venue is not yet verified on a live account: start with the minimum size."), WARN); }
            kit::note(ui, l("Prices and sizes are rounded to the venue's tick and step."), dim());
            if let Some(e) = &tk.err { kit::note(ui, e, dn()); }
            let (c, o) = dialog_buttons(ui, l("Cancel"), &format!("{} {}{}", l("Confirm"), tk.title(), if st["demo"] == true { " · DEMO" } else { "" }), col, true);
            if c || ui.input(|i| i.key_pressed(egui::Key::Escape)) { done = Some(false); }
            if o || (enter_ok && ui.input(|i| i.key_pressed(egui::Key::Enter))) { done = Some(true); }
        });
        if m.should_close() && done.is_none() { done = Some(false); }
        match done {
            Some(false) => self.ticket = None,
            Some(true) => {
                let tk = self.ticket.take().unwrap();
                if let Err(e) = self.place(&tk, call) { self.ticket = Some(Ticket { err: Some(e), ..tk }); }
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
            if t["demo"] == true { kit::note(ui, l("DEMO account: this order is simulated. No real order is sent."), WARN); }
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
    #[test]
    fn uni_liq_matches_the_venue() {
        // the user's Binance PM account: equity 4994.04, MM 274.10, 287.03 HYPE long at mark 81.39;
        // Binance showed 64.84 for that position
        let p = super::uni_liq(4994.04, 274.10, 287.03, 287.03, 287.03, 81.39, 0.005, 1.05).unwrap();
        assert!((p - 64.84).abs() < 0.5, "{p}");
        // a short is liquidated above the price; a flat hedge never
        assert!(super::uni_liq(1000.0, 10.0, 0.0, -10.0, 10.0, 100.0, 0.005, 1.05).is_some_and(|p| p > 100.0));
        assert!(super::uni_liq(1000.0, 10.0, 20.0, 0.0, 20.0, 100.0, 0.005, 1.05).is_none());
    }

    use super::*;
    #[test]
    fn prefill_position_fills_the_form() {
        let st = |tpsl: bool| json!({"trade": {"tick": 0.1, "step": 0.001, "caps": {"tpsl": tpsl}}, "route": {"smart": false}});
        let mut p = OrderPanel { close: true, kind: Kind::Market, qty_usdt: true, ..Default::default() };
        p.prefill_position(100.0, 95.0, 110.0, Some(1.23456), &st(true));
        assert!(!p.close && p.kind == Kind::Limit && !p.qty_usdt && p.tpsl_on && p.error.is_none());
        assert_eq!((p.price.as_str(), p.qty.as_str(), p.tp.as_str(), p.sl.as_str(), p.last_pos.as_deref()), ("100.0", "1.234", "110.0", "95.0", Some("long")));
        // a short: target under the entry; no size known keeps the field
        p.prefill_position(100.0, 105.0, 90.0, None, &st(true));
        assert_eq!((p.qty.as_str(), p.tp.as_str(), p.sl.as_str(), p.last_pos.as_deref()), ("1.234", "90.0", "105.0", Some("short")));
        // a venue without TP/SL: entry and size only, and it says so
        p.prefill_position(100.0, 95.0, 110.0, Some(2.0), &st(false));
        assert!(!p.tpsl_on && p.error.is_some());
        assert_eq!(p.qty, "2.000");
    }

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
