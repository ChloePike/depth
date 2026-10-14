//! Account strip for the egui app: a port of the SwiftUI host's AccountPanel, AccountTables and
//! AccountAssets (positions, open orders, TP/SL, signals, history, order log, assets and the
//! transfer sheet). It reads the same state document (`App::state_json`) and sends the same ops
//! (`App::call`) as the native panels, so both hosts act identically.
use super::order::{big_button, card, leak, note, parse, tpsl_modal, usd, Call, TpSlSheet};
use super::{fmt_dp, fmt_px, hms, l, theme::*};
use depth::Exchange;
use eframe::egui::{self, Color32, Rect, RichText, Ui};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tab { Positions, Orders, TpSl, Signals, OrderHist, TradeHist, PosHist, Log, Assets }

const TABS: [Tab; 9] = [Tab::Positions, Tab::Orders, Tab::TpSl, Tab::Signals, Tab::OrderHist, Tab::TradeHist, Tab::PosHist, Tab::Log, Tab::Assets];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Positions => l("Positions"), Tab::Orders => l("Open Orders"), Tab::TpSl => l("TP/SL"), Tab::Signals => l("Signals"),
            Tab::OrderHist => l("Order History"), Tab::TradeHist => l("Trade History"), Tab::PosHist => l("Position History"),
            Tab::Log => l("Order Log"), Tab::Assets => l("Assets"),
        }
    }
    fn history(self) -> bool { matches!(self, Tab::OrderHist | Tab::TradeHist | Tab::PosHist) }
}

/// A destructive position action waiting for confirmation.
enum PosAct { Market(Value, f64), Limit(Value, f64, f64), Reverse(Value), CloseAll(Vec<Value>) }

/// Something a table cell or menu asked for, run after the frame's tables are drawn.
enum Act { Op(Value), Goto(String), Copy(String), Confirm(PosAct), TpSl(Value), Share(Value), Prefill(Value), Transfer(String), Keys }

/// PnL card options (what the copied text reveals).
struct Share { p: Value, amount: bool, prices: bool, copied: bool }

/// Transfer sheet state (mirror of AccountTransferSheet).
struct Xfer { ex: String, from: String, to: String, coin: String, amount: String, auto: (bool, f64, f64), auto_loaded: bool, err: Option<String> }

pub struct AccountPanel {
    pub tab: Tab,
    hide_others: bool,
    hist_requested: bool,
    /// (column, descending) per sortable table
    sort_orders: (usize, bool),
    sort_tpsl: (usize, bool),
    sort_oh: (usize, bool),
    sort_fills: (usize, bool),
    sort_closed: (usize, bool),
    sel_orders: Vec<String>,
    sel_tpsl: Vec<String>,
    /// per-position inline limit close price, only once edited (else it follows the mark)
    close_px: HashMap<String, String>,
    confirm: Option<PosAct>,
    share: Option<Share>,
    sheet: Option<TpSlSheet>,
    xfer: Option<Xfer>,
    wallets_requested: Vec<String>,
    error: Option<String>,
    xfer_shot: bool,
    /// set when the strip wants the settings dialog (3 = API keys)
    pub open_settings: Option<u8>,
    /// a position to limit-close in the order panel, once it shows that symbol
    pub prefill: Option<(Value, std::time::Instant)>,
}

impl Default for AccountPanel {
    fn default() -> Self {
        AccountPanel {
            // T1_ACC_TAB=<n>: open the strip on tab n (screenshots)
            tab: std::env::var("T1_ACC_TAB").ok().and_then(|s| s.parse::<usize>().ok()).and_then(|n| TABS.get(n).copied()).unwrap_or(Tab::Positions), hide_others: false, hist_requested: false,
            sort_orders: (0, true), sort_tpsl: (0, false), sort_oh: (0, true), sort_fills: (0, true), sort_closed: (0, true),
            sel_orders: vec![], sel_tpsl: vec![], close_px: HashMap::new(),
            confirm: None, share: None, sheet: None, xfer: None, wallets_requested: vec![], error: None, xfer_shot: false,
            open_settings: None, prefill: None,
        }
    }
}

// ---- formatting (the Swift `Fmt`) ----

fn s(v: &Value) -> &str { v.as_str().unwrap_or_default() }
fn f(v: &Value) -> f64 { v.as_f64().unwrap_or(0.0) }
fn qty(v: f64) -> String { let a = v.abs(); fmt_dp(v, if a >= 1000.0 { 1 } else if a >= 1.0 { 3 } else { 5 }) }
fn signed(v: f64) -> String { signed_d(v, 2) }
fn signed_d(v: f64, d: usize) -> String { if v.abs() < 0.5 * 10f64.powi(-(d as i32)) { fmt_dp(0.0, d) } else { format!("{}{}", if v > 0.0 { "+" } else { "" }, fmt_dp(v, d)) } }
fn pnl_col(v: f64) -> Color32 { if v >= 0.0 { up() } else { dn() } }
/// "MM-dd HH:mm:ss" in the display time zone
fn time(ms: i64) -> String {
    let d = super::chart_date(ms);
    let (m, day) = d.split_once('/').unwrap_or(("", ""));
    format!("{m:0>2}-{day:0>2} {}", hms(ms))
}
fn base_of(symbol: &str) -> &str { symbol.strip_suffix("USDT").unwrap_or(symbol) }
fn pos_key(p: &Value) -> String { format!("{}|{}|{}", s(&p["ex"]), s(&p["symbol"]), s(&p["side"])) }
fn row_key(v: &Value) -> String { format!("{}|{}", s(&v["ex"]), s(&v["id"])) }
fn side_name(side: &str) -> &'static str {
    match side { "buy" => l("Buy"), "sell" => l("Sell"), "long" => l("Long"), _ => l("Short") }
}
fn wallet_name(id: &str) -> String {
    match id {
        "UNIFIED" => l("Unified Trading"), "FUND" | "FUNDING" => l("Funding"), "EARN" => l("Earn (Flexible)"), "PM" => l("Portfolio Margin"),
        "USDM" => l("USDS-M Futures"), "SPOT" => l("Spot"), _ => return id.to_string(),
    }.to_string()
}
fn note_hint(n: &str) -> &'static str {
    if ["10005", "Permission", "-2015", "permission"].iter().any(|k| n.contains(k)) { l("API key lacks read permission") } else { l("Read failed") }
}
fn movable_usd(w: &Value) -> f64 {
    w["coins"].as_array().map_or(0.0, |cs| cs.iter().filter(|c| f(&c["free"]) > 0.0).map(|c| if f(&c["qty"]) > 0.0 { f(&c["usd"]) * f(&c["free"]) / f(&c["qty"]) } else { 0.0 }).sum())
}

// ---- painting helpers ----

/// Text at the vertical center of `r`, left or right aligned; returns its rect.
fn txt(ui: &Ui, r: Rect, text: impl ToString, font: egui::FontId, col: Color32, right: bool) -> Rect {
    let (pos, al) = if right { (r.right_center(), egui::Align2::RIGHT_CENTER) } else { (r.left_center(), egui::Align2::LEFT_CENTER) };
    ui.painter().text(pos, al, text.to_string(), font, col)
}

/// Two stacked lines (value, detail) in a cell.
fn two(ui: &Ui, r: Rect, a: (String, Color32), b: Option<(String, Color32)>, right: bool) {
    let Some(b) = b else { txt(ui, r, a.0, mono(12.0), a.1, right); return };
    let (top, bot) = halves(r);
    txt(ui, top, a.0, mono(12.0), a.1, right);
    txt(ui, bot, b.0, mono(10.5), b.1, right);
}

/// Line rects of a two-line cell (value above, detail below the center).
fn halves(r: Rect) -> (Rect, Rect) {
    let c = r.center().y;
    (Rect::from_center_size(egui::pos2(r.center().x, c - 8.0), egui::vec2(r.width(), 16.0)), Rect::from_center_size(egui::pos2(r.center().x, c + 9.0), egui::vec2(r.width(), 14.0)))
}

/// Pieces of text in different colors, right-aligned at `r`'s right edge.
fn rich_right(ui: &Ui, r: Rect, parts: &[(String, Color32)], font: egui::FontId) -> f32 {
    let mut x = r.right();
    for (t, c) in parts.iter().rev() {
        x -= ui.painter().text(egui::pos2(x, r.center().y), egui::Align2::RIGHT_CENTER, t, font.clone(), *c).width();
    }
    r.right() - x
}

/// Small tinted label ("Long 10x", "Reduce", "Take Profit").
fn badge(ui: &Ui, pos: egui::Pos2, text: &str, col: Color32) -> Rect {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(10.5), col);
    let r = Rect::from_min_size(egui::pos2(pos.x, pos.y - g.size().y / 2.0 - 2.0), g.size() + egui::vec2(10.0, 4.0));
    ui.painter().rect_filled(r, 3, col.linear_multiply(0.16));
    ui.painter().galley(r.min + egui::vec2(5.0, 2.0), g, col);
    r
}

/// Bordered small button in `r`.
fn button(ui: &mut Ui, r: Rect, id: impl std::hash::Hash + std::fmt::Debug, text: &str, col: Color32) -> egui::Response {
    let resp = ui.interact(r, ui.id().with(id), egui::Sense::click());
    ui.painter().rect(r, 5, if resp.hovered() { hl() } else { panel2() }, egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside);
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, text, prop(11.5), col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Accent text action painted at `pos` (left-center); returns its response.
fn link_at(ui: &mut Ui, pos: egui::Pos2, id: impl std::hash::Hash + std::fmt::Debug, text: &str, col: Color32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(11.5), col);
    let r = Rect::from_min_size(egui::pos2(pos.x, pos.y - g.size().y / 2.0), g.size());
    let resp = ui.interact(r.expand(2.0), ui.id().with(id), egui::Sense::click());
    ui.painter().galley(r.min, g, col);
    if resp.hovered() { ui.painter().hline(r.x_range(), r.bottom(), egui::Stroke::new(1.0, col)); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Icon button: `kind` 0 = share (box + arrow), 1 = edit (pencil), 2 = refresh.
fn icon_btn(ui: &mut Ui, c: egui::Pos2, id: impl std::hash::Hash + std::fmt::Debug, kind: u8) -> egui::Response {
    let r = Rect::from_center_size(c, egui::vec2(18.0, 18.0));
    let resp = ui.interact(r, ui.id().with(id), egui::Sense::click());
    let p = ui.painter();
    if resp.hovered() { p.rect_filled(r, 4, hl()); }
    let st = egui::Stroke::new(1.3, if resp.hovered() { fg() } else { mu() });
    match kind {
        0 => {
            p.line(vec![c + egui::vec2(-4.0, -1.0), c + egui::vec2(-4.0, 5.0), c + egui::vec2(4.0, 5.0), c + egui::vec2(4.0, -1.0)], st);
            p.line_segment([c + egui::vec2(0.0, 2.0), c + egui::vec2(0.0, -6.0)], st);
            p.line(vec![c + egui::vec2(-3.0, -3.0), c + egui::vec2(0.0, -6.0), c + egui::vec2(3.0, -3.0)], st);
        }
        1 => {
            p.line_segment([c + egui::vec2(-4.0, 4.0), c + egui::vec2(3.5, -3.5)], egui::Stroke::new(2.0, st.color));
            p.line_segment([c + egui::vec2(-5.0, 6.0), c + egui::vec2(5.0, 6.0)], st);
        }
        _ => {
            let pts: Vec<egui::Pos2> = (0..=10).map(|i| { let a = 0.6 + i as f32 * 0.5; c + egui::vec2(a.cos(), a.sin()) * 5.0 }).collect();
            let end = *pts.last().unwrap();
            p.line(pts, st);
            p.line(vec![end + egui::vec2(-3.0, -1.0), end, end + egui::vec2(1.0, 3.0)], st);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Spinner painted from the clock: animates at the app's normal repaint rate, never forces frames.
fn spinner(ui: &Ui, c: egui::Pos2, r: f32) {
    let t = (depth::now_ms() % 1000) as f32 / 1000.0 * std::f32::consts::TAU;
    let pts: Vec<egui::Pos2> = (0..=12).map(|i| { let a = t + i as f32 * 0.38; c + egui::vec2(a.cos(), a.sin()) * r }).collect();
    ui.painter().line(pts, egui::Stroke::new(1.6, mu()));
}

/// Coin logo with the venue logo on its corner, then the symbol (click opens that market).
fn symbol_cell(ui: &mut Ui, r: Rect, ex: &str, symbol: &str, key: &str, acts: &mut Vec<Act>) -> f32 {
    let base = base_of(symbol);
    let c = egui::pos2(r.left() + 8.0, r.center().y);
    coin_icon(ui, base, Rect::from_center_size(c, egui::vec2(16.0, 16.0)));
    if let Some(e) = Exchange::parse(ex) {
        ui.painter().circle_filled(c + egui::vec2(6.0, 5.0), 5.5, panel());
        icon(ui.painter(), e, Rect::from_center_size(c + egui::vec2(6.0, 5.0), egui::vec2(9.0, 9.0)), false);
    }
    let tr = ui.painter().text(egui::pos2(r.left() + 23.0, r.center().y), egui::Align2::LEFT_CENTER, symbol, prop(12.0), fg());
    let hit = Rect::from_min_max(egui::pos2(r.left(), r.center().y - 9.0), egui::pos2(tr.right(), r.center().y + 9.0));
    let resp = ui.interact(hit, ui.id().with(("sym", key)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!("{ex} · {} — {}", l("Perp"), l("click to open this market")));
    if resp.hovered() { ui.painter().hline(tr.x_range(), tr.bottom(), egui::Stroke::new(1.0, mu())); }
    if resp.clicked() { acts.push(Act::Goto(base.to_string())); }
    tr.right()
}

/// "Est. PnL" bubble under `anchor`, above everything (rows below must not cover it).
fn pnl_bubble(ui: &Ui, anchor: Rect, pnl: f64) {
    let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("est_pnl")));
    let (k, v) = (p.layout_no_wrap(l("Est. PnL").into(), prop(10.5), dim()), p.layout_no_wrap(format!("{} USDT", signed(pnl)), mono(12.0), pnl_col(pnl)));
    let size = egui::vec2(k.size().x.max(v.size().x) + 16.0, k.size().y + v.size().y + 10.0);
    let r = Rect::from_min_size(egui::pos2(anchor.center().x - size.x / 2.0, anchor.bottom() + 6.0), size);
    let tip = egui::pos2(anchor.center().x, anchor.bottom() + 1.0);
    p.add(egui::Shape::convex_polygon(vec![tip, tip + egui::vec2(5.0, 5.5), tip + egui::vec2(-5.0, 5.5)], popup_bg(), egui::Stroke::NONE));
    p.rect(r, 6, popup_bg(), egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside);
    let vy = r.top() + 5.0 + k.size().y;
    p.galley(egui::pos2(r.left() + 8.0, r.top() + 5.0), k, dim());
    p.galley(egui::pos2(r.left() + 8.0, vy), v, fg());
}

/// Inset text field inside a table cell.
fn cell_field(ui: &mut Ui, r: Rect, text: &mut String, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Response {
    let eid = ui.id().with(id);
    let focused = ui.memory(|m| m.has_focus(eid));
    ui.painter().rect(r, 5, bg(), egui::Stroke::new(1.0, if focused { accent() } else { line() }), egui::StrokeKind::Inside);
    ui.put(r.shrink2(egui::vec2(6.0, 2.0)), egui::TextEdit::singleline(text).id(eid).frame(egui::Frame::NONE).font(mono(11.5)).horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center))
}

/// Centered empty state: title, description, optional button.
fn empty_state(ui: &mut Ui, title: &str, desc: &str, button: Option<&str>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.22).clamp(10.0, 40.0));
        ui.label(RichText::new(title).font(prop(14.0)).color(fg()).strong());
        if !desc.is_empty() {
            ui.add_space(4.0);
            ui.allocate_ui(egui::vec2(ui.available_width().min(480.0), 60.0), |ui| ui.vertical_centered(|ui| ui.add(egui::Label::new(RichText::new(desc).font(prop(12.0)).color(dim())).wrap())));
        }
        if let Some(b) = button {
            ui.add_space(8.0);
            clicked = ui.add(egui::Button::new(RichText::new(b).font(prop(12.0))).corner_radius(6).min_size(egui::vec2(120.0, 26.0))).clicked();
        }
    });
    clicked
}

/// A piece of the summary line: text, or a venue logo.
/// Text, a venue logo, or a risk gauge (0..1).
enum Seg { T(String, egui::FontId, Color32), I(Exchange), G(f32) }

/// Lays `segs` out left to right as one group (gap after labels / logos); false, and nothing
/// drawn, when the space left in the line is too small for it.
fn segs_group(ui: &mut Ui, segs: &[Seg], tip: &str) -> bool {
    let gals: Vec<Option<std::sync::Arc<egui::Galley>>> = segs.iter().map(|g| match g { Seg::T(t, f, c) => Some(ui.painter().layout_no_wrap(t.clone(), f.clone(), *c)), _ => None }).collect();
    let gap = 4.0;
    let w: f32 = gals.iter().map(|g| g.as_ref().map_or(12.0, |g| g.size().x) + gap).sum::<f32>() - gap;
    if w + 14.0 > ui.available_width() { return false; }
    ui.add_space(10.0);
    let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 16.0), egui::Sense::hover());
    let mut x = r.left();
    for (sg, g) in segs.iter().zip(gals) {
        match (sg, g) {
            (Seg::G(risk), _) => { risk_gauge(ui.painter(), egui::pos2(x + 7.0, r.center().y + 4.5), 6.5, *risk); x += 12.0 + gap; }
            (Seg::I(e), _) => { icon(ui.painter(), *e, Rect::from_center_size(egui::pos2(x + 6.0, r.center().y), egui::vec2(12.0, 12.0)), false); x += 12.0 + gap; }
            (Seg::T(_, _, c), Some(g)) => { let gw = g.size().x; ui.painter().galley(egui::pos2(x, r.center().y - g.size().y / 2.0), g, *c); x += gw + gap; }
            _ => {}
        }
    }
    if !tip.is_empty() { resp.on_hover_text(tip); }
    true
}

// ---- table ----

#[derive(Clone, Copy)]
enum Key { No, F(&'static str), S(&'static str), G(fn(&Value) -> f64) }

/// Column: header, minimum width, right-aligned (numbers), shares spare width, sort key.
struct Col { name: &'static str, w: f32, right: bool, grow: bool, key: Key }
fn col(name: &'static str, w: f32, right: bool, key: Key) -> Col { Col { name, w, right, grow: true, key } }

fn cmp(k: Key, a: &Value, b: &Value) -> Ordering {
    match k {
        Key::No => Ordering::Equal,
        Key::F(n) => f(&a[n]).total_cmp(&f(&b[n])),
        Key::S(n) => s(&a[n]).cmp(s(&b[n])),
        Key::G(g) => g(a).total_cmp(&g(b)),
    }
}

/// What the table asks its caller to draw.
enum Part<'v> { Head(usize, Rect), Row(Rect, &'v Value), Cell(usize, Rect, &'v Value), Menu(Vec<&'v Value>) }

const PAD: f32 = 12.0;

/// Dense painted table: pinned header (click to sort), row hover, optional multi-selection (click,
/// cmd/shift-click), right-click menu, hairlines; scrolls sideways when narrower than its columns.
#[allow(clippy::too_many_arguments)]
fn table<'v>(ui: &mut Ui, id: &str, cols: &[Col], mut rows: Vec<&'v Value>, row_h: f32, empty: &str,
             mut sort: Option<&mut (usize, bool)>, mut sel: Option<&mut Vec<String>>, f: &mut dyn FnMut(&mut Ui, Part<'v>)) {
    if let Some((c, desc)) = sort.as_deref().copied() {
        let k = cols[c.min(cols.len() - 1)].key;
        rows.sort_by(|a, b| { let o = cmp(k, a, b); if desc { o.reverse() } else { o } });
    }
    if let Some(sel) = sel.as_deref_mut() { sel.retain(|k| rows.iter().any(|r| row_key(r) == *k)); }
    let min: f32 = cols.iter().map(|c| c.w).sum::<f32>() + 2.0 * PAD;
    let total = ui.available_width().max(min);
    let growers = cols.iter().filter(|c| c.grow).count().max(1) as f32;
    let extra = (total - min) / growers;
    let mut xs = vec![];
    let mut x = PAD;
    for c in cols { let w = c.w + if c.grow { extra } else { 0.0 }; xs.push((x, w)); x += w; }
    let cell_rect = |row: Rect, i: usize| { let (x, w) = xs[i]; Rect::from_min_size(egui::pos2(row.left() + x, row.top()), egui::vec2(w, row.height())).shrink2(egui::vec2(7.0, 0.0)) };
    egui::ScrollArea::horizontal().id_salt((id, "h")).auto_shrink([false, false]).show(ui, |ui| {
        ui.set_min_width(total);
        let (hr, _) = ui.allocate_exact_size(egui::vec2(total, 26.0), egui::Sense::hover());
        for (i, c) in cols.iter().enumerate() {
            let r = cell_rect(hr, i);
            let on = sort.as_deref().is_some_and(|s| s.0 == i);
            let arrow = match sort.as_deref() { Some(&(si, d)) if si == i => if d { " ↓" } else { " ↑" }, _ => "" };
            let tr = txt(ui, r, format!("{}{arrow}", c.name), prop(11.0), if on { fg() } else { dim() }, c.right);
            if !matches!(c.key, Key::No) {
                if let Some(srt) = sort.as_deref_mut() {
                    if ui.interact(tr.expand(3.0), ui.id().with((id, "sort", i)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        *srt = if srt.0 == i { (i, !srt.1) } else { (i, false) };
                    }
                }
            }
            f(ui, Part::Head(i, r));
        }
        ui.painter().hline(hr.x_range(), hr.bottom(), egui::Stroke::new(1.0, line()));
        if rows.is_empty() {
            ui.add_space(24.0);
            ui.allocate_ui(egui::vec2(ui.available_width().min(total), 20.0), |ui| ui.vertical_centered(|ui| ui.label(RichText::new(empty).font(prop(12.0)).color(dim()))));
            return;
        }
        egui::ScrollArea::vertical().id_salt((id, "v")).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let v = rows[i];
                let (r, resp) = ui.allocate_exact_size(egui::vec2(total, row_h), egui::Sense::click());
                let k = row_key(v);
                // cells draw in a child: widgets placed with `put` must not move this ui's cursor
                let ui = &mut ui.new_child(egui::UiBuilder::new().id_salt((id, &k, i)).max_rect(r));
                f(ui, Part::Row(r, v));
                let selected = sel.as_deref().is_some_and(|s| s.contains(&k));
                if selected { ui.painter().rect_filled(r, 4, accent().linear_multiply(0.16)); }
                else if resp.hovered() { ui.painter().rect_filled(r, 4, hl().linear_multiply(0.55)); }
                if let Some(sel) = sel.as_deref_mut() {
                    if resp.clicked() {
                        if ui.input(|i| i.modifiers.command || i.modifiers.shift) { if let Some(p) = sel.iter().position(|x| *x == k) { sel.remove(p); } else { sel.push(k.clone()); } }
                        else { sel.clear(); sel.push(k.clone()); }
                    }
                    if resp.secondary_clicked() && !sel.contains(&k) { sel.clear(); sel.push(k.clone()); }
                }
                resp.context_menu(|ui| {
                    let picked: Vec<&Value> = match sel.as_deref() { Some(s) if s.contains(&k) => rows.iter().filter(|r| s.contains(&row_key(r))).copied().collect(), _ => vec![v] };
                    f(ui, Part::Menu(picked));
                });
                for c in 0..cols.len() { f(ui, Part::Cell(c, cell_rect(r, c), v)); }
                ui.painter().hline(r.x_range(), r.bottom() - 0.5, egui::Stroke::new(1.0, line().linear_multiply(0.5)));
            }
        });
    });
}

fn menu_item(ui: &mut Ui, text: &str) -> bool { let c = ui.button(RichText::new(text).font(prop(12.0))).clicked(); if c { ui.close(); } c }

impl AccountPanel {
    /// The whole strip. `st` is the app's state document, `call` sends ops.
    pub fn show(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let mut acts = vec![];
        let arr = |k: &str| st[k].as_array().map_or(0, |a| a.len());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for t in TABS {
                let n = match t { Tab::Positions => Some(arr("positions")), Tab::Orders => Some(arr("orders")), Tab::TpSl => Some(arr("tpsl")), Tab::Signals => Some(arr("signals")), _ => None };
                let label = n.map_or(t.title().to_string(), |n| format!("{} ({n})", t.title()));
                if tab(ui, &label, self.tab == t, 12.5).clicked() { self.tab = t; }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                self.summary(ui, st);
                if self.tab == Tab::Positions && ui.available_width() > 140.0 {
                    ui.add_space(10.0);
                    ui.checkbox(&mut self.hide_others, RichText::new(l("Hide other symbols")).font(prop(11.5)).color(mu()));
                }
            });
        });
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, line()));
        ui.add_space(2.0);
        // history is fetched when first opened (never on a timer); Refresh re-fetches
        if self.tab.history() && !self.hist_requested {
            self.hist_requested = true;
            if st["history"]["updated_ms"].is_null() && st["history"]["loading"] != true { acts.push(Act::Op(json!({"op": "load_history"}))); }
        }
        let no_keys = st["keys"].as_array().is_none_or(|k| k.iter().all(|k| k["configured"] != true));
        if self.tab == Tab::Signals { self.signals(ui, st); }
        else if no_keys && self.tab != Tab::Log {
            if empty_state(ui, l("No API keys"), l("Add an API key to see positions, orders and balances."), Some(l("Open API Keys"))) { acts.push(Act::Keys); }
        } else {
            match self.tab {
                Tab::Positions => self.positions(ui, st, &mut acts),
                Tab::Orders => self.orders(ui, st, &mut acts),
                Tab::TpSl => self.tpsl(ui, st, &mut acts),
                Tab::OrderHist | Tab::TradeHist | Tab::PosHist => self.history(ui, st, &mut acts),
                Tab::Log => self.log(ui, st),
                Tab::Assets => self.assets(ui, st, &mut acts),
                Tab::Signals => {}
            }
        }
        // T1_OPEN_TRANSFER=1: open the transfer sheet once wallets are known (screenshots)
        if !self.xfer_shot && std::env::var("T1_OPEN_TRANSFER").is_ok() && arr("wallets") > 0 { self.xfer_shot = true; acts.push(Act::Transfer(String::new())); }
        self.run(ui.ctx(), acts, st, call);
        self.confirm_modal(ui, call);
        self.share_modal(ui);
        tpsl_modal(&mut self.sheet, ui, st, call);
        self.transfer_modal(ui, st, call);
        if let Some(e) = self.error.clone() {
            egui::Modal::new(egui::Id::new("acc_error")).show(ui.ctx(), |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new(l("Action failed")).strong().font(prop(14.0)));
                note(ui, &e, fg());
                ui.add_space(6.0);
                if big_button(ui, l("OK"), accent(), true) || ui.input(|i| i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Enter)) { self.error = None; }
            });
        }
    }

    fn run(&mut self, ctx: &egui::Context, acts: Vec<Act>, st: &Value, call: &mut Call) {
        for a in acts {
            match a {
                Act::Op(v) => { if let Err(e) = call(v) { self.error = Some(e); } }
                Act::Goto(base) => {
                    let _ = call(json!({"op": "set_base", "base": base}));
                    if s(&st["mode"]) != "Perp" { let _ = call(json!({"op": "set_mode", "mode": "Perp"})); }
                }
                Act::Copy(t) => ctx.copy_text(t),
                Act::Confirm(c) => self.confirm = Some(c),
                Act::TpSl(p) => self.sheet = Some(TpSlSheet::new(s(&p["ex"]), s(&p["symbol"]), s(&p["side"]))),
                Act::Share(p) => self.share = Some(Share { p, amount: true, prices: true, copied: false }),
                Act::Prefill(p) => {
                    // the order panel trades the current symbol on its venue: move both there first
                    let _ = call(json!({"op": "set_trade_venue", "ex": p["ex"]}));
                    let _ = call(json!({"op": "set_base", "base": base_of(s(&p["symbol"]))}));
                    if s(&st["mode"]) != "Perp" { let _ = call(json!({"op": "set_mode", "mode": "Perp"})); }
                    self.prefill = Some((p, std::time::Instant::now()));
                }
                Act::Transfer(ex) => self.open_transfer(&ex, st, call),
                Act::Keys => self.open_settings = Some(3),
            }
        }
    }

    /// Account equity, available and risk per keyed venue (what actually liquidates a unified
    /// account), then the position totals; always shown, on every tab. Called inside a
    /// right-to-left layout: groups are added from the right and dropped once they no longer fit.
    fn summary(&mut self, ui: &mut Ui, st: &Value) {
        let ps = st["positions"].as_array().cloned().unwrap_or_default();
        let mut bals: Vec<&Value> = st["balances"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        bals.sort_by(|a, b| s(&b["ex"]).cmp(s(&a["ex"])));
        let (k, v) = (|t: &str| Seg::T(t.to_string(), prop(11.0), dim()), |t: String, c: Color32| Seg::T(t, mono(12.0), c));
        for b in bals {
            let mut segs = vec![];
            if let Some(e) = Exchange::parse(s(&b["ex"])) { segs.push(Seg::I(e)); }
            segs.extend([k(l("Equity")), v(usd(f(&b["equity"])), fg()), k(l("Available")), v(usd(f(&b["available"])), fg())]);
            if let Some(m) = b["uni_mmr"].as_f64() { segs.extend([k("uniMMR"), Seg::G(mmr_risk(m)), v(fmt_dp(m, 2), if m > 1.5 { up() } else if m > 1.2 { WARN } else { dn() })]); }
            if let Some(r) = b["mm_rate"].as_f64() { segs.extend([k(l("MM rate")), Seg::G(r as f32), v(format!("{:.1}%", r * 100.0), if r < 0.5 { up() } else if r < 0.8 { WARN } else { dn() })]); }
            let mut tip = format!("{} · {} {} USD · {} {} USD", s(&b["ex"]), l("Equity"), usd(f(&b["equity"])), l("Available"), usd(f(&b["available"])));
            if let Some(m) = b["maint_margin"].as_f64() { tip += &format!("\n{} {} USD", l("Maintenance margin"), usd(m)); }
            if !b["uni_mmr"].is_null() { tip += "\n"; tip += l("Portfolio Margin maintenance ratio: below 1.20 margin call, below 1.05 the whole account is liquidated. Higher is safer."); }
            if !b["mm_rate"].is_null() { tip += "\n"; tip += l("Unified account maintenance margin rate: 100% triggers liquidation. Lower is safer."); }
            if !segs_group(ui, &segs, &tip) { return; }
        }
        let sum = |g: &dyn Fn(&Value) -> f64| ps.iter().map(g).sum::<f64>();
        let upnl = sum(&|p| f(&p["upnl"]));
        if !segs_group(ui, &[k(l("uPnL")), v(signed(upnl), if ps.is_empty() { fg() } else { pnl_col(upnl) })], "") { return; }
        if ps.is_empty() { return; }
        if !segs_group(ui, &[k(l("Margin")), v(usd(sum(&|p| f(&p["margin"]))), fg())], "") { return; }
        segs_group(ui, &[k(l("Value")), v(fmt_dp(sum(&|p| f(&p["qty"]) * f(&p["mark"])), 0), fg())], "");
    }

    // ---- positions ----

    fn positions(&mut self, ui: &mut Ui, st: &Value, acts: &mut Vec<Act>) {
        let cur = format!("{}USDT", s(&st["base"]));
        let mut rows: Vec<&Value> = st["positions"].as_array().map(|a| a.iter().filter(|p| !self.hide_others || s(&p["symbol"]) == cur).collect()).unwrap_or_default();
        // stable order: pushes reorder the engine's vector; ties broken by venue then side
        rows.sort_by(|a, b| (s(&a["symbol"]), s(&a["ex"]), s(&a["side"])).cmp(&(s(&b["symbol"]), s(&b["ex"]), s(&b["side"]))));
        let all: Vec<Value> = rows.iter().map(|p| (*p).clone()).collect();
        let pm = st["binance_pm"] == true;
        let tpsl = st["tpsl"].as_array().cloned().unwrap_or_default();
        let cols = [
            Col { name: l("Symbol"), w: 210.0, right: false, grow: false, key: Key::No },
            col(l("Size"), 130.0, true, Key::No), col(l("Entry"), 92.0, true, Key::No), col(l("Mark"), 92.0, true, Key::No), col(l("Liq. Price"), 92.0, true, Key::No),
            col(l("Margin"), 116.0, true, Key::No), col(l("PnL (ROE%)"), 140.0, true, Key::No), col(l("Close Position"), 200.0, false, Key::No),
            col(l("TP / SL"), 130.0, true, Key::No), col(l("Est. Funding"), 140.0, true, Key::No),
            Col { name: "", w: 78.0, right: true, grow: false, key: Key::No },
        ];
        let px_map = &mut self.close_px;
        let now = depth::now_ms();
        table(ui, "positions", &cols, rows, 50.0, l("No open positions"), None, None, &mut |ui, part| match part {
            Part::Head(7, r) => {
                let w = ui.painter().layout_no_wrap(l("Close Position").into(), prop(11.0), dim()).size().x;
                let resp = link_at(ui, egui::pos2(r.left() + w + 10.0, r.center().y), "close_all", l("Close All Positions"), if all.is_empty() { dim() } else { WARN })
                    .on_hover_text(l("Market-close every position listed here (asks to confirm)"));
                if resp.clicked() && !all.is_empty() { acts.push(Act::Confirm(PosAct::CloseAll(all.clone()))); }
            }
            Part::Head(..) => {}
            Part::Row(r, p) => {
                // the side at a glance: a bar on the leading edge and a soft fade across the symbol
                let c = if s(&p["side"]) == "long" { up() } else { dn() };
                let fade = Rect::from_min_size(r.min, egui::vec2(260.0, r.height()));
                let mut mesh = egui::Mesh::default();
                let (a, z) = (c.linear_multiply(0.16), Color32::TRANSPARENT);
                mesh.colored_vertex(fade.left_top(), a); mesh.colored_vertex(fade.right_top(), z);
                mesh.colored_vertex(fade.right_bottom(), z); mesh.colored_vertex(fade.left_bottom(), a);
                mesh.add_triangle(0, 1, 2); mesh.add_triangle(0, 2, 3);
                ui.painter().add(mesh);
                ui.painter().rect_filled(Rect::from_min_size(r.min, egui::vec2(3.0, r.height())), 0, c);
            }
            Part::Cell(i, r, p) => {
                let long = s(&p["side"]) == "long";
                let side = if long { up() } else { dn() };
                let (q, mark, margin, upnl) = (f(&p["qty"]), f(&p["mark"]), f(&p["margin"]), f(&p["upnl"]));
                let key = pos_key(p);
                let base = base_of(s(&p["symbol"]));
                match i {
                    0 => {
                        let x = symbol_cell(ui, r, s(&p["ex"]), s(&p["symbol"]), &key, acts);
                        let lev = f(&p["lev"]);
                        badge(ui, egui::pos2(x + 7.0, r.center().y), &format!("{}{}", if long { l("Long") } else { l("Short") }, if lev > 0.0 { format!(" {lev:.0}x") } else { String::new() }), side);
                    }
                    1 => two(ui, r, (format!("{} USDT", usd(q * mark)), side), Some((format!("{} {base}", qty(q)), dim())), true),
                    2 => { txt(ui, r, fmt_px(f(&p["entry"])), mono(12.0), fg(), true); }
                    3 => { txt(ui, r, fmt_px(mark), mono(12.0), fg(), true); }
                    4 => match p["liq"].as_f64() {
                        Some(lq) => { txt(ui, r, fmt_px(lq), mono(12.0), WARN, true); }
                        None => {
                            let tr = txt(ui, r, "—", mono(12.0), dim(), true);
                            ui.interact(tr.expand(4.0), ui.id().with(("liq", &key)), egui::Sense::hover()).on_hover_text(if pm && s(&p["ex"]) == "Binance" {
                                l("Portfolio Margin liquidates on the whole account's uniMMR; the exchange gives no per-position liquidation price.") } else { l("No liquidation price reported") });
                        }
                    },
                    5 => {
                        let cross = p["cross"].as_bool().map(|c| (if c { l("Cross") } else { l("Isolated") }.to_string(), dim()));
                        two(ui, r, (format!("{} USDT", usd(margin)), fg()), cross, true);
                    }
                    6 => {
                        if icon_btn(ui, egui::pos2(r.right() - 9.0, r.center().y), ("share", &key), 0).on_hover_text(l("Share this PnL as an image")).clicked() { acts.push(Act::Share(p.clone())); }
                        let roe = if margin > 0.0 { upnl / margin * 100.0 } else { 0.0 };
                        two(ui, Rect::from_min_max(r.min, egui::pos2(r.right() - 24.0, r.max.y)), (format!("{} USDT", signed(upnl)), pnl_col(upnl)), Some((format!("{}%", signed(roe)), pnl_col(upnl))), true);
                    }
                    7 => {
                        let dpx = { let a = mark.abs(); let d = if a >= 10_000.0 { 1 } else if a >= 100.0 { 2 } else if a >= 1.0 { 4 } else { 6 }; format!("{mark:.d$}") };
                        let mut px_t = px_map.get(&key).cloned().unwrap_or(dpx);
                        let y = r.center().y;
                        let m = link_at(ui, egui::pos2(r.left(), y), ("mkt", &key), l("Market"), WARN).on_hover_text(l("Close the whole position at market (asks to confirm)"));
                        let sep = ui.painter().text(egui::pos2(m.rect.right() + 6.0, y), egui::Align2::LEFT_CENTER, "|", prop(11.5), dim());
                        let lm = link_at(ui, egui::pos2(sep.right() + 6.0, y), ("lmt", &key), l("Limit"), WARN).on_hover_text(l("Limit-close the whole position at the price beside (asks to confirm)"));
                        let pr = Rect::from_min_size(egui::pos2(lm.rect.right() + 10.0, y - 11.0), egui::vec2(96.0, 22.0));
                        let fr = cell_field(ui, pr, &mut px_t, ("cpx", &key)).on_hover_text(l("Limit close price"));
                        if fr.changed() { px_map.insert(key.clone(), px_t.clone()); }
                        // estimated PnL of closing at that price, in a bubble under the field
                        if let Some(x) = parse(&px_t).filter(|x| *x > 0.0 && (fr.has_focus() || (fr.hovered() && px_map.contains_key(&key)))) {
                            pnl_bubble(ui, pr, (x - f(&p["entry"])) * q * if long { 1.0 } else { -1.0 });
                        }
                        if m.clicked() && q > 0.0 { acts.push(Act::Confirm(PosAct::Market(p.clone(), q))); }
                        if lm.clicked() && q > 0.0 { if let Some(x) = parse(&px_t).filter(|x| *x > 0.0) { acts.push(Act::Confirm(PosAct::Limit(p.clone(), x, q))); } }
                    }
                    8 => {
                        if icon_btn(ui, egui::pos2(r.right() - 9.0, r.center().y), ("tpsl", &key), 1).on_hover_text(l("Whole-position or staged TP/SL")).clicked() { acts.push(Act::TpSl(p.clone())); }
                        let tr = Rect::from_min_max(r.min, egui::pos2(r.right() - 24.0, r.max.y));
                        let mine: Vec<&Value> = tpsl.iter().filter(|t| t["ex"] == p["ex"] && t["symbol"] == p["symbol"] && t["pos"] == p["side"]).collect();
                        if mine.is_empty() { txt(ui, tr, "—", mono(12.0), dim(), true); } else {
                            let list = |tp: bool| { let mut v: Vec<f64> = mine.iter().filter(|t| (t["take_profit"] == true) == tp).map(|t| f(&t["trigger_px"])).collect(); v.sort_by(f64::total_cmp); v };
                            let (tps, sls) = (list(true), list(false));
                            let j = |v: &[f64]| if v.is_empty() { "—".to_string() } else { v.iter().map(|x| fmt_px(*x)).collect::<Vec<_>>().join(", ") };
                            let w = rich_right(ui, tr, &[(j(&tps), if tps.is_empty() { dim() } else { up() }), (" / ".into(), dim()), (j(&sls), if sls.is_empty() { dim() } else { dn() })], mono(12.0));
                            let tip = mine.iter().map(|t| format!("{} {} ({}) · {}", if t["take_profit"] == true { "TP" } else { "SL" }, fmt_px(f(&t["trigger_px"])), s(&t["trigger"]),
                                t["qty"].as_f64().map(qty).unwrap_or(l("entire position").into()))).collect::<Vec<_>>().join("\n");
                            ui.interact(Rect::from_min_max(egui::pos2(tr.right() - w, tr.top()), tr.max), ui.id().with(("tpt", &key)), egui::Sense::hover()).on_hover_text(tip);
                        }
                    }
                    9 => match p["funding_rate"].as_f64() {
                        Some(rate) => {
                            let cd = p["next_funding_ms"].as_i64().map(|n| { let sec = ((n - now) / 1000).max(0); format!("  {:02}:{:02}:{:02}", sec / 3600, sec % 3600 / 60, sec % 60) }).unwrap_or_default();
                            let (top, bot) = halves(r);
                            let est = p["funding_est"].as_f64();
                            let top = if est.is_some() { top } else { r };
                            rich_right(ui, top, &[(format!("{:+.4}%", rate * 100.0), pnl_col(rate)), (cd, mu())], mono(12.0));
                            if let Some(e) = est { txt(ui, bot, format!("{} {} USDT", l("next"), signed(e)), mono(10.5), pnl_col(e), true); }
                            if let Some(h) = p["funding_interval_h"].as_f64() { ui.interact(r, ui.id().with(("fund", &key)), egui::Sense::hover()).on_hover_text(format!("{} {h}h", l("Settles every"))); }
                        }
                        None => {
                            let tr = txt(ui, r, "—", mono(12.0), dim(), true);
                            ui.interact(tr.expand(4.0), ui.id().with(("fund", &key)), egui::Sense::hover()).on_hover_text(l("Funding is shown for Binance, Bybit and Hyperliquid positions"));
                        }
                    },
                    _ => {
                        let b = Rect::from_min_size(egui::pos2(r.right() - 66.0, r.center().y - 11.0), egui::vec2(66.0, 22.0));
                        if button(ui, b, ("rev", &key), l("Reverse"), fg()).on_hover_text(l("Close at market and open the same size on the other side (asks to confirm)")).clicked() { acts.push(Act::Confirm(PosAct::Reverse(p.clone()))); }
                    }
                }
            }
            Part::Menu(ps) => {
                let p = ps[0];
                if menu_item(ui, l("Close at Market…")) { acts.push(Act::Confirm(PosAct::Market(p.clone(), f(&p["qty"])))); }
                if menu_item(ui, l("Limit Close in Order Panel")) { acts.push(Act::Prefill(p.clone())); }
                if menu_item(ui, l("TP/SL…")) { acts.push(Act::TpSl(p.clone())); }
                if menu_item(ui, l("Share PnL…")) { acts.push(Act::Share(p.clone())); }
                ui.separator();
                if menu_item(ui, l("Copy Symbol")) { acts.push(Act::Copy(s(&p["symbol"]).into())); }
            }
        });
    }

    fn confirm_modal(&mut self, ui: &mut Ui, call: &mut Call) {
        let Some(a) = &self.confirm else { return };
        let ls = |p: &Value| if s(&p["side"]) == "long" { l("Long") } else { l("Short") };
        let (title, button_t, message) = match a {
            PosAct::Market(p, q) => (l("Close position at market?"), l("Close at Market"),
                format!("{} {} {} {} {} ≈ {} USDT\n{}", s(&p["ex"]), s(&p["symbol"]), ls(p), qty(*q), base_of(s(&p["symbol"])), fmt_dp(q * f(&p["mark"]), 0), l("A market order fills against the book; slippage applies."))),
            PosAct::Limit(p, x, q) => (l("Place limit close?"), l("Place Limit Close"), format!("{} {} {} {} {} @ {}", s(&p["ex"]), s(&p["symbol"]), ls(p), qty(*q), base_of(s(&p["symbol"])), fmt_px(*x))),
            PosAct::Reverse(p) => {
                let (b, q) = (base_of(s(&p["symbol"])), qty(f(&p["qty"])));
                let other = if s(&p["side"]) == "long" { l("Short") } else { l("Long") };
                (l("Reverse position?"), l("Reverse"), format!("{} {}: {} {} {q} {b}, {} {other} {q} {b}\n{}", s(&p["ex"]), s(&p["symbol"]), l("close"), ls(p), l("then open"), l("Two market orders; if the second fails you are left flat.")))
            }
            PosAct::CloseAll(ps) => (l("Close all positions at market?"), l("Close All"), ps.iter().map(|p| format!("{} {} {} {}", s(&p["ex"]), s(&p["symbol"]), ls(p), qty(f(&p["qty"])))).collect::<Vec<_>>().join("\n")),
        };
        let mut done = None;
        let m = egui::Modal::new(egui::Id::new("pos_confirm")).show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new(title).strong().font(prop(15.0)));
            ui.add_space(2.0);
            note(ui, &message, mu());
            ui.add_space(8.0);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 32.0], egui::Button::new(l("Cancel"))).clicked() || c[0].input(|i| i.key_pressed(egui::Key::Escape)) { done = Some(false); }
                if big_button(&mut c[1], button_t, dn(), true) { done = Some(true); }
            });
        });
        if m.should_close() && done.is_none() { done = Some(false); }
        let Some(go) = done else { return };
        let a = self.confirm.take().unwrap();
        if !go { return; }
        let close = |p: &Value, q: f64, px: Option<f64>| { let mut v = json!({"op": "close_position", "ex": p["ex"], "symbol": p["symbol"], "side": p["side"], "qty": q}); if let Some(x) = px { v["price"] = x.into(); } v };
        let ops = match a {
            PosAct::Market(p, q) => vec![close(&p, q, None)],
            PosAct::Limit(p, x, q) => vec![close(&p, q, Some(x))],
            PosAct::Reverse(p) => vec![json!({"op": "reverse_position", "ex": p["ex"], "symbol": p["symbol"], "side": p["side"]})],
            PosAct::CloseAll(ps) => ps.iter().map(|p| close(p, f(&p["qty"]), None)).collect(),
        };
        for v in ops { if let Err(e) = call(v) { self.error = Some(e); } }
    }

    /// PnL card: the result in big type, options for what to reveal, copy as text.
    // ponytail: text copy only; PNG export would need an image encoder path for the egui card
    fn share_modal(&mut self, ui: &mut Ui) {
        let Some(sh) = self.share.as_mut() else { return };
        let p = sh.p.clone();
        let (upnl, margin, lev) = (f(&p["upnl"]), f(&p["margin"]), f(&p["lev"]));
        let roe = if margin > 0.0 { upnl / margin * 100.0 } else { 0.0 };
        let long = s(&p["side"]) == "long";
        let (col, side) = (pnl_col(upnl), if long { up() } else { dn() });
        let mut close = false;
        let m = egui::Modal::new(egui::Id::new("pnl_share")).show(ui.ctx(), |ui| {
            ui.set_width(560.0);
            let (r, _) = ui.allocate_exact_size(egui::vec2(560.0, 300.0), egui::Sense::hover());
            let pnt = ui.painter().with_clip_rect(r);
            pnt.rect(r, 14, panel2(), egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside);
            // soft glow in the result's color from the lower right
            for k in 0..6 { pnt.circle_filled(r.right_bottom() - egui::vec2(60.0, 50.0), 220.0 - k as f32 * 34.0, col.linear_multiply(0.035)); }
            let at = |dx: f32, dy: f32| r.min + egui::vec2(dx, dy);
            pnt.text(at(26.0, 30.0), egui::Align2::LEFT_CENTER, "Depth", prop(17.0), fg());
            pnt.text(egui::pos2(r.right() - 26.0, r.top() + 30.0), egui::Align2::RIGHT_CENTER, time(depth::now_ms()), prop(12.0), dim());
            coin_icon(ui, base_of(s(&p["symbol"])), Rect::from_center_size(at(38.0, 92.0), egui::vec2(24.0, 24.0)));
            let sr = pnt.text(at(58.0, 92.0), egui::Align2::LEFT_CENTER, s(&p["symbol"]), prop(21.0), fg());
            let br = badge(ui, egui::pos2(sr.right() + 10.0, sr.center().y + 2.0), l("Perp"), mu());
            badge(ui, egui::pos2(br.right() + 6.0, sr.center().y + 2.0), &format!("{} {lev:.0}x", if long { l("Long") } else { l("Short") }), side);
            let pnt = ui.painter().with_clip_rect(r);
            pnt.text(at(24.0, 150.0), egui::Align2::LEFT_CENTER, format!("{}%", signed(roe)), display(56.0), col);
            if sh.amount { pnt.text(at(26.0, 196.0), egui::Align2::LEFT_CENTER, format!("{} USDT", signed(upnl)), mono(17.0), col.linear_multiply(0.9)); }
            if sh.prices {
                for (i, (k, v)) in [(l("Entry"), fmt_px(f(&p["entry"]))), (l("Mark"), fmt_px(f(&p["mark"]))), (l("Venue"), s(&p["ex"]).to_string())].into_iter().enumerate() {
                    let x = 26.0 + i as f32 * 150.0;
                    pnt.text(at(x, 246.0), egui::Align2::LEFT_CENTER, k, prop(11.0), dim());
                    pnt.text(at(x, 266.0), egui::Align2::LEFT_CENTER, v, mono(15.0), fg());
                }
            }
            ui.add_space(8.0);
            ui.checkbox(&mut sh.amount, l("Show PnL amount"));
            ui.checkbox(&mut sh.prices, l("Show entry and mark price"));
            ui.label(RichText::new(l("Size, margin and account balances are never on the card.")).font(prop(11.0)).color(dim()));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if sh.copied { ui.label(RichText::new(format!("✓ {}", l("Copied"))).color(mu())); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new(l("Copy Text")).color(Color32::WHITE)).fill(accent()).corner_radius(6).min_size(egui::vec2(110.0, 30.0))).clicked() {
                        let mut t = format!("{} {} {} {lev:.0}x  {}%", s(&p["symbol"]), l("Perp"), if long { l("Long") } else { l("Short") }, signed(roe));
                        if sh.amount { t += &format!(" ({} USDT)", signed(upnl)); }
                        if sh.prices { t += &format!("\n{} {} · {} {} · {}", l("Entry"), fmt_px(f(&p["entry"])), l("Mark"), fmt_px(f(&p["mark"])), s(&p["ex"])); }
                        ui.ctx().copy_text(t);
                        sh.copied = true;
                    }
                    if ui.add(egui::Button::new(l("Close")).corner_radius(6).min_size(egui::vec2(80.0, 30.0))).clicked() { close = true; }
                });
            });
        });
        if m.should_close() || close { self.share = None; }
    }

    // ---- open orders / TP/SL ----

    fn orders(&mut self, ui: &mut Ui, st: &Value, acts: &mut Vec<Act>) {
        let mut rows: Vec<&Value> = st["orders"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        rows.sort_by(|a, b| (b["ts"].as_i64(), s(&b["id"])).cmp(&(a["ts"].as_i64(), s(&a["id"]))));
        let cols = [
            col(l("Time"), 116.0, false, Key::F("ts")), col(l("Symbol"), 150.0, false, Key::S("symbol")), col(l("Side"), 90.0, false, Key::S("side")),
            col(l("Type"), 100.0, false, Key::S("kind")), col(l("Price"), 90.0, true, Key::F("price")), col(l("Amount"), 80.0, true, Key::F("qty")),
            col(l("Filled"), 100.0, true, Key::F("filled")), col(l("Value"), 90.0, true, Key::G(|o| f(&o["price"]) * f(&o["qty"]))), Col { name: "", w: 70.0, right: true, grow: false, key: Key::No },
        ];
        if self.sel_orders.len() > 1 {
            let picked: Vec<Value> = rows.iter().filter(|o| self.sel_orders.contains(&row_key(o))).map(|o| (*o).clone()).collect();
            ui.horizontal(|ui| {
                ui.add_space(PAD);
                ui.label(RichText::new(format!("{} {}", picked.len(), l("selected"))).font(prop(11.5)).color(mu()));
                if link(ui, l("Cancel Selected Orders")).clicked() { for o in &picked { acts.push(Act::Op(json!({"op": "cancel", "ex": o["ex"], "id": o["id"]}))); } }
            });
        }
        table(ui, "orders", &cols, rows, 30.0, l("No open orders"), Some(&mut self.sort_orders), Some(&mut self.sel_orders), &mut |ui, part| match part {
            Part::Cell(i, r, o) => match i {
                0 => { txt(ui, r, time(o["ts"].as_i64().unwrap_or(0)), mono(11.5), mu(), false); }
                1 => { symbol_cell(ui, r, s(&o["ex"]), s(&o["symbol"]), &row_key(o), acts); }
                2 => {
                    let buy = s(&o["side"]) == "buy";
                    let pos = o["pos"].as_str().map(|p| format!(" · {}", side_name(p))).unwrap_or_default();
                    txt(ui, r, format!("{}{pos}", side_name(s(&o["side"]))), prop(12.0), if buy { up() } else { dn() }, false);
                }
                3 => {
                    let tr = txt(ui, r, s(&o["kind"]), prop(12.0), fg(), false);
                    if o["reduce_only"] == true { badge(ui, egui::pos2(tr.right() + 6.0, r.center().y), l("Reduce"), mu()); }
                }
                4 => { txt(ui, r, fmt_px(f(&o["price"])), mono(12.0), fg(), true); }
                5 => { txt(ui, r, qty(f(&o["qty"])), mono(12.0), fg(), true); }
                6 => {
                    let (fl, q) = (f(&o["filled"]), f(&o["qty"]));
                    let mut parts = vec![(qty(fl), if fl > 0.0 { fg() } else { mu() })];
                    if q > 0.0 { parts.push((format!(" ({}%)", fmt_dp(fl / q * 100.0, 1)), mu())); }
                    rich_right(ui, r, &parts, mono(12.0));
                }
                7 => { txt(ui, r, usd(f(&o["price"]) * f(&o["qty"])), mono(12.0), mu(), true); }
                _ => {
                    let b = Rect::from_min_size(egui::pos2(r.right() - 58.0, r.center().y - 10.0), egui::vec2(58.0, 20.0));
                    if button(ui, b, ("cancel", row_key(o)), l("Cancel"), fg()).clicked() { acts.push(Act::Op(json!({"op": "cancel", "ex": o["ex"], "id": o["id"]}))); }
                }
            },
            Part::Menu(sel) => {
                if menu_item(ui, if sel.len() == 1 { l("Cancel Order") } else { l("Cancel Selected Orders") }) { for o in &sel { acts.push(Act::Op(json!({"op": "cancel", "ex": o["ex"], "id": o["id"]}))); } }
                if menu_item(ui, l("Copy Order ID")) { acts.push(Act::Copy(sel.iter().map(|o| s(&o["id"])).collect::<Vec<_>>().join("\n"))); }
            }
            _ => {}
        });
    }

    fn tpsl(&mut self, ui: &mut Ui, st: &Value, acts: &mut Vec<Act>) {
        let mut rows: Vec<&Value> = st["tpsl"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        rows.sort_by(|a, b| (s(&a["symbol"]), s(&a["ex"]), s(&a["id"])).cmp(&(s(&b["symbol"]), s(&b["ex"]), s(&b["id"]))));
        let cols = [
            col(l("Symbol"), 170.0, false, Key::S("symbol")), col(l("Position"), 80.0, false, Key::S("pos")), col(l("Type"), 100.0, false, Key::No),
            col(l("Trigger Price"), 100.0, true, Key::F("trigger_px")), col(l("Trigger"), 90.0, false, Key::No), col(l("Amount"), 100.0, true, Key::No),
            Col { name: "", w: 70.0, right: true, grow: false, key: Key::No },
        ];
        let cancel = |t: &Value| Act::Op(json!({"op": "cancel_tpsl", "ex": t["ex"], "id": t["id"]}));
        table(ui, "tpsl", &cols, rows, 30.0, l("No TP/SL orders"), Some(&mut self.sort_tpsl), Some(&mut self.sel_tpsl), &mut |ui, part| match part {
            Part::Cell(i, r, t) => match i {
                0 => { symbol_cell(ui, r, s(&t["ex"]), s(&t["symbol"]), &row_key(t), acts); }
                1 => { txt(ui, r, side_name(s(&t["pos"])), prop(12.0), if s(&t["pos"]) == "long" { up() } else { dn() }, false); }
                2 => { let tp = t["take_profit"] == true; badge(ui, r.left_center(), if tp { l("Take Profit") } else { l("Stop Loss") }, if tp { up() } else { dn() }); }
                3 => { txt(ui, r, fmt_px(f(&t["trigger_px"])), mono(12.0), fg(), true); }
                4 => { txt(ui, r, if s(&t["trigger"]) == "last" { l("Last price") } else { l("Mark price") }, prop(12.0), mu(), false); }
                5 => match t["qty"].as_f64() { Some(q) => { txt(ui, r, qty(q), mono(12.0), fg(), true); } None => { txt(ui, r, l("Entire position"), prop(12.0), mu(), true); } },
                _ => {
                    let b = Rect::from_min_size(egui::pos2(r.right() - 58.0, r.center().y - 10.0), egui::vec2(58.0, 20.0));
                    if button(ui, b, ("tcancel", row_key(t)), l("Cancel"), fg()).clicked() { acts.push(cancel(t)); }
                }
            },
            Part::Menu(sel) => { if menu_item(ui, if sel.len() == 1 { l("Cancel TP/SL") } else { l("Cancel Selected TP/SL") }) { for t in &sel { acts.push(cancel(t)); } } }
            _ => {}
        });
    }

    // ---- history ----

    fn history(&mut self, ui: &mut Ui, st: &Value, acts: &mut Vec<Act>) {
        let h = &st["history"];
        ui.horizontal(|ui| {
            ui.set_height(24.0);
            ui.add_space(PAD);
            ui.label(RichText::new(l("Last 7 days, fetched when opened")).font(prop(11.0)).color(dim()));
            if let Some(at) = h["updated_ms"].as_i64() {
                ui.label(RichText::new(format!("· {} {}s {}", l("updated"), ((depth::now_ms() - at) / 1000).max(0), l("ago"))).font(prop(11.0)).color(dim()));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(PAD);
                if h["loading"] == true {
                    ui.label(RichText::new(l("Loading…")).font(prop(11.0)).color(mu()));
                    let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    spinner(ui, r.center(), 5.0);
                } else if link(ui, &format!("↻ {}", l("Refresh"))).clicked() { acts.push(Act::Op(json!({"op": "load_history"}))); }
            });
        });
        let list = |k: &str| h[k].as_array().map(|a| a.iter().collect::<Vec<_>>()).unwrap_or_default();
        let time_c = |ui: &Ui, r: Rect, v: &Value| { txt(ui, r, time(v["ts"].as_i64().unwrap_or(0)), mono(11.5), mu(), false); };
        let side_c = |ui: &Ui, r: Rect, side: &str| { txt(ui, r, side_name(side), prop(12.0), if side == "buy" || side == "long" { up() } else { dn() }, false); };
        let num = |ui: &Ui, r: Rect, v: Option<f64>, fmt: fn(f64) -> String| match v.filter(|x| *x > 0.0) { Some(x) => { txt(ui, r, fmt(x), mono(12.0), fg(), true); } None => { txt(ui, r, "—", mono(12.0), dim(), true); } };
        match self.tab {
            Tab::OrderHist => {
                let cols = [
                    col(l("Time"), 116.0, false, Key::F("ts")), col(l("Symbol"), 150.0, false, Key::S("symbol")), col(l("Side"), 60.0, false, Key::S("side")),
                    col(l("Type"), 80.0, false, Key::S("kind")), col(l("Price"), 90.0, true, Key::F("price")), col(l("Avg. Price"), 90.0, true, Key::F("avg")),
                    col(l("Filled / Amount"), 130.0, true, Key::F("filled")), col(l("Status"), 100.0, true, Key::S("status")),
                ];
                table(ui, "order_hist", &cols, list("orders"), 28.0, l("No orders in the last 7 days"), Some(&mut self.sort_oh), None, &mut |ui, part| if let Part::Cell(i, r, o) = part {
                    match i {
                        0 => time_c(ui, r, o),
                        1 => { symbol_cell(ui, r, s(&o["ex"]), s(&o["symbol"]), &format!("oh{}{}", s(&o["symbol"]), o["ts"]), acts); }
                        2 => side_c(ui, r, s(&o["side"])),
                        3 => { txt(ui, r, s(&o["kind"]), prop(12.0), fg(), false); }
                        4 => num(ui, r, o["price"].as_f64(), fmt_px),
                        5 => num(ui, r, o["avg"].as_f64(), fmt_px),
                        6 => { txt(ui, r, format!("{} / {}", qty(f(&o["filled"])), qty(f(&o["qty"]))), mono(12.0), fg(), true); }
                        _ => {
                            let st = s(&o["status"]);
                            let u = st.to_uppercase();
                            let c = if u.contains("FILLED") && !u.contains("PARTIAL") { fg() } else if u.contains("CANCEL") || u.contains("EXPIRE") { dim() } else if u.contains("REJECT") { dn() } else { mu() };
                            txt(ui, r, st, prop(12.0), c, true);
                        }
                    }
                });
            }
            Tab::TradeHist => {
                let cols = [
                    col(l("Time"), 116.0, false, Key::F("ts")), col(l("Symbol"), 150.0, false, Key::S("symbol")), col(l("Side"), 60.0, false, Key::S("side")),
                    col(l("Price"), 90.0, true, Key::F("price")), col(l("Amount"), 80.0, true, Key::F("qty")), col(l("Value"), 100.0, true, Key::G(|x| f(&x["price"]) * f(&x["qty"]))),
                    col(l("Fee"), 80.0, true, Key::F("fee")), col(l("Realized PnL"), 100.0, true, Key::F("realized")),
                ];
                table(ui, "fills", &cols, list("fills"), 28.0, l("No trades in the last 7 days"), Some(&mut self.sort_fills), None, &mut |ui, part| if let Part::Cell(i, r, x) = part {
                    match i {
                        0 => time_c(ui, r, x),
                        1 => { symbol_cell(ui, r, s(&x["ex"]), s(&x["symbol"]), &format!("tf{}{}", s(&x["symbol"]), x["ts"]), acts); }
                        2 => side_c(ui, r, s(&x["side"])),
                        3 => { txt(ui, r, fmt_px(f(&x["price"])), mono(12.0), fg(), true); }
                        4 => { txt(ui, r, qty(f(&x["qty"])), mono(12.0), fg(), true); }
                        5 => { txt(ui, r, usd(f(&x["price"]) * f(&x["qty"])), mono(12.0), fg(), true); }
                        6 => { txt(ui, r, fmt_dp(f(&x["fee"]), 4), mono(12.0), mu(), true); }
                        _ => match x["realized"].as_f64().filter(|v| *v != 0.0) { Some(v) => { txt(ui, r, signed(v), mono(12.0), pnl_col(v), true); } None => { txt(ui, r, "—", mono(12.0), dim(), true); } },
                    }
                });
            }
            _ => {
                let cols = [
                    col(l("Closed"), 116.0, false, Key::F("ts")), col(l("Symbol"), 150.0, false, Key::S("symbol")),
                    col(l("Side"), 60.0, false, Key::G(|c| match c["long"].as_bool() { Some(true) => 1.0, Some(false) => 2.0, None => 0.0 })),
                    col(l("Amount"), 90.0, true, Key::F("qty")), col(l("Entry"), 90.0, true, Key::No), col(l("Exit"), 90.0, true, Key::No), col(l("Realized PnL"), 110.0, true, Key::F("pnl")),
                ];
                table(ui, "closed", &cols, list("closed"), 28.0, l("No closed positions in the last 7 days"), Some(&mut self.sort_closed), None, &mut |ui, part| if let Part::Cell(i, r, c) = part {
                    match i {
                        0 => time_c(ui, r, c),
                        1 => { symbol_cell(ui, r, s(&c["ex"]), s(&c["symbol"]), &format!("ph{}{}", s(&c["symbol"]), c["ts"]), acts); }
                        2 => match c["long"].as_bool() { Some(lg) => side_c(ui, r, if lg { "long" } else { "short" }), None => { txt(ui, r, "—", prop(12.0), dim(), false); } },
                        3 => num(ui, r, c["qty"].as_f64(), qty),
                        4 => num(ui, r, c["entry"].as_f64(), fmt_px),
                        5 => num(ui, r, c["exit"].as_f64(), fmt_px),
                        _ => { let v = f(&c["pnl"]); txt(ui, r, signed(v), mono(12.0), pnl_col(v), true); }
                    }
                });
            }
        }
    }

    // ---- order log / signals ----

    fn log(&mut self, ui: &mut Ui, st: &Value) {
        let rows = st["log"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() { empty_state(ui, l("No order activity this session"), "", None); return; }
        egui::ScrollArea::vertical().id_salt("order_log").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            for r in &rows {
                let ok = r["ok"] == true;
                ui.horizontal(|ui| {
                    ui.add_space(PAD);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let (ir, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::hover());
                    let c = ir.center();
                    if ok {
                        ui.painter().circle_stroke(c, 5.0, egui::Stroke::new(1.0, dim()));
                        ui.painter().line(vec![c + egui::vec2(-2.3, 0.2), c + egui::vec2(-0.6, 2.0), c + egui::vec2(2.5, -1.8)], egui::Stroke::new(1.2, dim()));
                    } else {
                        ui.painter().add(egui::Shape::convex_polygon(vec![c + egui::vec2(0.0, -5.5), c + egui::vec2(5.5, 4.5), c + egui::vec2(-5.5, 4.5)], dn(), egui::Stroke::NONE));
                        ui.painter().text(c + egui::vec2(0.0, 1.0), egui::Align2::CENTER_CENTER, "!", prop(8.5), Color32::WHITE);
                    }
                    ui.label(RichText::new(time(r["ts"].as_i64().unwrap_or(0))).font(mono(11.0)).color(dim()));
                    ui.add(egui::Label::new(RichText::new(s(&r["msg"])).font(mono(11.0)).color(if ok { fg() } else { dn() })).selectable(true));
                });
            }
        });
    }

    fn signals(&mut self, ui: &mut Ui, st: &Value) {
        let rows = st["signals"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            empty_state(ui, l("No signals yet"), l("Volume spikes, liquidation cascades, OI / price divergence, crowded funding, spot vs perp flow, basis, whale trades and venue dislocations appear here as they happen."), None);
            return;
        }
        egui::ScrollArea::vertical().id_salt("signals").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for g in &rows {
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
                let dir = g["dir"].as_i64().unwrap_or(0);
                let c = if dir > 0 { up() } else if dir < 0 { dn() } else { WARN };
                let ic = egui::pos2(r.left() + 24.0, r.top() + 17.0);
                let p = ui.painter();
                p.circle_filled(ic, 9.0, c);
                let w = egui::Stroke::new(1.6, Color32::WHITE);
                match dir.signum() {
                    1 => { p.line_segment([ic + egui::vec2(-3.5, 3.5), ic + egui::vec2(3.5, -3.5)], w); p.line(vec![ic + egui::vec2(-1.0, -3.5), ic + egui::vec2(3.5, -3.5), ic + egui::vec2(3.5, 1.0)], w); }
                    -1 => { p.line_segment([ic + egui::vec2(-3.5, -3.5), ic + egui::vec2(3.5, 3.5)], w); p.line(vec![ic + egui::vec2(-1.0, 3.5), ic + egui::vec2(3.5, 3.5), ic + egui::vec2(3.5, -1.0)], w); }
                    _ => { p.text(ic, egui::Align2::CENTER_CENTER, "!", prop(12.0), Color32::WHITE); }
                }
                let sev = g["severity"].as_i64().unwrap_or(1).clamp(0, 5) as usize;
                let tr = p.text(egui::pos2(r.left() + 42.0, r.top() + 16.0), egui::Align2::LEFT_CENTER, l(leak(s(&g["title"]))), prop(12.5), fg());
                p.text(egui::pos2(tr.right() + 7.0, tr.center().y), egui::Align2::LEFT_CENTER, "●".repeat(sev), prop(8.0), if sev >= 3 { dn() } else if sev == 2 { WARN } else { mu() });
                p.text(egui::pos2(r.left() + 42.0, r.top() + 33.0), egui::Align2::LEFT_CENTER, s(&g["detail"]), prop(11.5), dim());
                p.text(egui::pos2(r.right() - PAD, r.top() + 16.0), egui::Align2::RIGHT_CENTER, time(g["ts"].as_i64().unwrap_or(0)), mono(11.0), dim());
                p.hline(egui::Rangef::new(r.left() + PAD, r.right() - PAD), r.bottom() - 0.5, egui::Stroke::new(1.0, line().linear_multiply(0.5)));
            }
        });
    }

    // ---- assets ----

    fn assets(&mut self, ui: &mut Ui, st: &Value, acts: &mut Vec<Act>) {
        let wallets = st["wallets"].as_array().cloned().unwrap_or_default();
        // each venue's wallets are fetched once when the tab is first shown (Refresh re-fetches)
        for w in &wallets {
            let ex = s(&w["ex"]).to_string();
            if w["updated_ms"].is_null() && w["loading"] != true && !self.wallets_requested.contains(&ex) {
                acts.push(Act::Op(json!({"op": "load_wallets", "ex": ex})));
                self.wallets_requested.push(ex);
            }
        }
        egui::ScrollArea::vertical().id_salt("assets").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            let n = (((ui.available_width() - 2.0 * PAD + 10.0) / 430.0).floor() as usize).clamp(1, wallets.len().max(1));
            for chunk in wallets.chunks(n) {
                ui.horizontal(|ui| {
                    ui.add_space(PAD - 2.0);
                    ui.columns(n, |c| for (i, w) in chunk.iter().enumerate() { self.wallet_card(&mut c[i], w, acts); });
                });
                ui.add_space(10.0);
            }
        });
    }

    fn wallet_card(&self, ui: &mut Ui, w: &Value, acts: &mut Vec<Act>) {
        let ex = s(&w["ex"]);
        let accounts = w["accounts"].as_array().cloned().unwrap_or_default();
        card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if let Some(e) = Exchange::parse(ex) { let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover()); icon(ui.painter(), e, r, false); }
                ui.label(RichText::new(ex).font(prop(13.0)).strong().color(fg()));
                if !w["updated_ms"].is_null() {
                    ui.label(RichText::new(format!("≈ {} USD", usd(accounts.iter().map(|a| f(&a["usd"])).sum::<f64>().max(0.0)))).font(mono(12.5)).color(fg()));
                }
                if w["auto"]["enabled"] == true {
                    let t = format!("{} < {} → {}", l("Auto top-up"), fmt_dp(f(&w["auto"]["min"]), 0), fmt_dp(f(&w["auto"]["target"]), 0));
                    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.painter().layout_no_wrap(t.clone(), prop(10.5), up()).size().x + 10.0, 16.0), egui::Sense::hover());
                    badge(ui, r.left_center(), &t, up());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new(format!("⇄ {}", l("Transfer"))).font(prop(12.0))).corner_radius(6)).clicked() { acts.push(Act::Transfer(ex.to_string())); }
                    if w["loading"] == true {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                        spinner(ui, r.center(), 5.0);
                    } else if let Some(at) = w["updated_ms"].as_i64() {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                        if icon_btn(ui, r.center(), ("wref", ex), 2).on_hover_text(l("Refresh")).clicked() { acts.push(Act::Op(json!({"op": "load_wallets", "ex": ex}))); }
                        ui.label(RichText::new(format!("{}s", ((depth::now_ms() - at) / 1000).max(0))).font(mono(11.0)).color(dim()));
                    }
                });
            });
            let r = ui.available_rect_before_wrap();
            ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, line()));
            ui.add_space(3.0);
            if w["updated_ms"].is_null() {
                ui.label(RichText::new(if w["loading"] == true { l("Reading accounts…") } else { l("Not loaded") }).font(prop(11.5)).color(dim()));
            }
            for a in &accounts {
                let id = s(&a["id"]);
                let margin = depth::trade::MARGIN_IDS.contains(&id);
                // dust (under a cent) is not worth a slot
                let mut coins: Vec<&Value> = a["coins"].as_array().map(|c| c.iter().filter(|c| f(&c["usd"]) >= 0.01 || (f(&c["usd"]) == 0.0 && f(&c["qty"]) >= 1e-4)).collect()).unwrap_or_default();
                coins.sort_by(|x, y| f(&y["usd"]).total_cmp(&f(&x["usd"])));
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
                let p = ui.painter();
                p.circle_filled(egui::pos2(r.left() + 3.0, r.center().y), 2.5, if margin { accent() } else { dim() });
                p.text(egui::pos2(r.left() + 12.0, r.center().y), egui::Align2::LEFT_CENTER, wallet_name(id), prop(12.0), if margin { fg() } else { mu() });
                p.text(egui::pos2(r.left() + 236.0, r.center().y), egui::Align2::RIGHT_CENTER, usd(f(&a["usd"]).max(0.0)), mono(12.0), fg());
                let mut x = r.left() + 252.0;
                for c in coins.iter().take(3) {
                    let coin = s(&c["coin"]);
                    let start = x;
                    coin_icon(ui, coin, Rect::from_center_size(egui::pos2(x + 6.0, r.center().y), egui::vec2(12.0, 12.0)));
                    let p = ui.painter();
                    x = p.text(egui::pos2(x + 15.0, r.center().y), egui::Align2::LEFT_CENTER, qty(f(&c["qty"])), mono(11.0), mu()).right();
                    x = p.text(egui::pos2(x + 3.0, r.center().y), egui::Align2::LEFT_CENTER, coin, prop(11.0), dim()).right();
                    ui.interact(Rect::from_min_max(egui::pos2(start, r.top()), egui::pos2(x, r.bottom())), ui.id().with(("coin", ex, id, coin)), egui::Sense::hover())
                        .on_hover_text(format!("{coin} {} · {} {} · ≈{} USD", qty(f(&c["qty"])), l("free"), qty(f(&c["free"])), usd(f(&c["usd"]))));
                    x += 10.0;
                }
                if coins.len() > 3 { ui.painter().text(egui::pos2(x, r.center().y), egui::Align2::LEFT_CENTER, format!("+{}", coins.len() - 3), mono(11.0), dim()); }
            }
            for n in w["notes"].as_array().into_iter().flatten() {
                let n = s(n);
                let id = n.split(':').next().unwrap_or("");
                ui.label(RichText::new(format!("⚠ {}: {}", wallet_name(id), note_hint(n))).font(prop(11.0)).color(WARN)).on_hover_text(n);
            }
        });
    }

    // ---- transfer sheet ----

    /// Open the transfer sheet for `ex` (or the trade venue / first keyed venue); wallets load on demand.
    pub fn open_transfer(&mut self, ex: &str, st: &Value, call: &mut Call) {
        let wallets = st["wallets"].as_array().cloned().unwrap_or_default();
        let keyed: Vec<&str> = wallets.iter().map(|w| s(&w["ex"])).collect();
        let venue = s(&st["trade"]["venue"]);
        let Some(e) = keyed.iter().find(|k| k.eq_ignore_ascii_case(ex)).or_else(|| keyed.iter().find(|k| **k == venue)).or(keyed.first()).map(|e| e.to_string()) else { return };
        if let Some(w) = wallets.iter().find(|w| s(&w["ex"]) == e) { if w["updated_ms"].is_null() && w["loading"] != true { let _ = call(json!({"op": "load_wallets", "ex": e})); } }
        self.xfer = Some(Xfer { ex: e, from: String::new(), to: String::new(), coin: "USDT".into(), amount: String::new(), auto: (false, 0.0, 0.0), auto_loaded: false, err: None });
    }

    fn transfer_modal(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let Some(x) = self.xfer.as_mut() else { return };
        let pm = st["binance_pm"] == true;
        let wallets = st["wallets"].as_array().cloned().unwrap_or_default();
        let wallet = wallets.iter().find(|w| s(&w["ex"]) == x.ex).cloned();
        let ws: Vec<Value> = wallet.as_ref().and_then(|w| w["accounts"].as_array().cloned()).unwrap_or_default();
        let cur_auto = wallet.as_ref().map(|w| (w["auto"]["enabled"] == true, f(&w["auto"]["min"]), f(&w["auto"]["target"]))).unwrap_or((false, 0.0, 0.0));
        if !x.auto_loaded { x.auto_loaded = true; x.auto = cur_auto; }
        let exv = Exchange::parse(&x.ex);
        let dests_of = |from: &str| exv.map(|e| depth::trade::destinations(e, from, pm)).unwrap_or_default();
        // default: from the non-margin account with the most that can move, into the margin account
        if x.from.is_empty() && !ws.is_empty() {
            let margin = ws.iter().map(|w| s(&w["id"])).find(|id| depth::trade::MARGIN_IDS.contains(id)).unwrap_or("").to_string();
            let best = ws.iter().filter(|w| s(&w["id"]) != margin).max_by(|a, b| movable_usd(a).total_cmp(&movable_usd(b)));
            x.from = best.filter(|b| movable_usd(b) > 0.0).map(|b| s(&b["id"]).to_string()).unwrap_or(margin.clone());
            let d = dests_of(&x.from);
            x.to = if d.contains(&margin.as_str()) { margin } else { d.first().map(|s| s.to_string()).unwrap_or_default() };
        }
        let dests = dests_of(&x.from);
        if !x.from.is_empty() && !dests.contains(&x.to.as_str()) { x.to = dests.first().map(|s| s.to_string()).unwrap_or_default(); }
        let sources: Vec<&Value> = ws.iter().filter(|w| w["coins"].as_array().is_some_and(|c| c.iter().any(|c| f(&c["free"]) > 0.0))).collect();
        let coins: Vec<Value> = ws.iter().find(|w| s(&w["id"]) == x.from).and_then(|w| w["coins"].as_array()).map(|c| c.iter().filter(|c| f(&c["free"]) > 0.0).cloned().collect()).unwrap_or_default();
        if !coins.is_empty() && !coins.iter().any(|c| s(&c["coin"]) == x.coin) { x.coin = s(&coins[0]["coin"]).to_string(); }
        let coin = coins.iter().find(|c| s(&c["coin"]) == x.coin).cloned();
        let free = coin.as_ref().map_or(0.0, |c| f(&c["free"]));
        let amt = parse(&x.amount).unwrap_or(0.0);
        let valid = amt > 0.0 && amt <= free * (1.0 + 1e-9) && !x.to.is_empty() && coin.as_ref().is_some_and(|c| x.from != "EARN" || !c["product"].is_null());
        let busy = st["transferring"] == true;
        let (mut close, mut confirm, mut save, mut new_ex) = (false, false, false, None);
        let row = |ui: &mut Ui, label: &str, add: &mut dyn FnMut(&mut Ui)| {
            ui.horizontal(|ui| {
                ui.set_min_height(30.0);
                ui.label(RichText::new(label).font(prop(12.5)).color(fg()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
            });
        };
        let combo_w = 250.0;
        let m = egui::Modal::new(egui::Id::new("acc_transfer")).show(ui.ctx(), |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                if let Some(e) = exv { let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover()); icon(ui.painter(), e, r, false); }
                ui.label(RichText::new(l("Transfer")).strong().font(prop(15.0)));
            });
            ui.add_space(4.0);
            card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                row(ui, l("Exchange"), &mut |ui| {
                    egui::ComboBox::from_id_salt("xf_ex").icon(chevron).width(combo_w).selected_text(&x.ex).show_ui(ui, |ui| {
                        for w in &wallets { let e = s(&w["ex"]); if ui.selectable_label(e == x.ex, e).clicked() && e != x.ex { new_ex = Some(e.to_string()); } }
                    });
                });
                if wallet.as_ref().is_some_and(|w| w["updated_ms"].is_null()) {
                    ui.horizontal(|ui| { let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover()); spinner(ui, r.center(), 5.0); ui.label(RichText::new(l("Reading accounts…")).color(mu())); });
                }
                row(ui, l("From"), &mut |ui| {
                    egui::ComboBox::from_id_salt("xf_from").icon(chevron).width(combo_w).selected_text(if x.from.is_empty() { "—".to_string() } else { wallet_name(&x.from) }).show_ui(ui, |ui| {
                        for w in &sources { let id = s(&w["id"]); ui.selectable_value(&mut x.from, id.to_string(), format!("{}   {} USD", wallet_name(id), usd(f(&w["usd"])))); }
                    });
                });
                row(ui, l("To"), &mut |ui| {
                    egui::ComboBox::from_id_salt("xf_to").icon(chevron).width(combo_w).selected_text(if x.to.is_empty() { "—".to_string() } else { wallet_name(&x.to) }).show_ui(ui, |ui| {
                        for d in &dests { ui.selectable_value(&mut x.to, d.to_string(), wallet_name(d)); }
                    });
                });
                row(ui, l("Coin"), &mut |ui| {
                    egui::ComboBox::from_id_salt("xf_coin").icon(chevron).width(combo_w).selected_text(if x.coin.is_empty() { "—" } else { &x.coin }).show_ui(ui, |ui| {
                        for c in &coins { let n = s(&c["coin"]); ui.selectable_value(&mut x.coin, n.to_string(), format!("{n}   {}", qty(f(&c["free"])))); }
                    });
                });
                row(ui, l("Amount"), &mut |ui| {
                    if ui.small_button(l("Max")).clicked() { x.amount = format!("{free}"); }
                    let (r, _) = ui.allocate_exact_size(egui::vec2(150.0, 24.0), egui::Sense::hover());
                    let mut a = std::mem::take(&mut x.amount);
                    cell_field(ui, r, &mut a, "xf_amt");
                    x.amount = a;
                });
            });
            ui.add_space(2.0);
            ui.label(RichText::new(format!("{} {} {}", l("Available"), qty(free), x.coin)).font(prop(11.0)).color(dim()));
            if amt > free * (1.0 + 1e-9) { note(ui, l("Amount exceeds the available balance"), dn()); }
            // multi-step and slow routes are spelled out before confirming
            if x.from == "EARN" { note(ui, l("Redeems from Flexible Earn, usually instant; a redemption cannot be undone."), WARN); }
            if x.ex == "Binance" && matches!((x.from.as_str(), x.to.as_str()), ("FUNDING", "PM") | ("PM", "FUNDING") | ("EARN", "PM") | ("EARN", "USDM")) {
                note(ui, l("No direct route: moves to Spot first, then into the target account (two steps)."), WARN);
            }
            if x.from == "EARN" && coin.as_ref().is_some_and(|c| c["product"].is_null()) { note(ui, l("On-chain Earn must be redeemed on the exchange website (takes days)."), dn()); }
            if let Some(e) = &x.err { note(ui, e, dn()); }
            ui.add_space(10.0);
            ui.label(RichText::new(l("Auto top-up").to_uppercase()).font(prop(10.5)).color(dim()));
            card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                row(ui, l("Top up margin automatically"), &mut |ui| { toggle(ui, &mut x.auto.0); });
                row(ui, l("When available is below"), &mut |ui| { num_field(ui, "xf_min", &mut x.auto.1, 0, "USD"); });
                row(ui, l("Top up to"), &mut |ui| { num_field(ui, "xf_target", &mut x.auto.2, 0, "USD"); });
                ui.horizontal(|ui| ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = x.auto != cur_auto && !(x.auto.0 && x.auto.2 <= x.auto.1);
                    if ui.add_enabled(ok, egui::Button::new(l("Save Rule")).corner_radius(6)).clicked() { save = true; }
                }));
            });
            let src = if x.ex == "Bybit" { l("Funding → Flexible Earn") } else { l("Spot → Funding → Flexible Earn") };
            note(ui, &format!("{} {src}", l("At most once a minute per venue; every transfer is written to the order log. Sources in order:")), dim());
            if x.auto.0 && x.auto.2 <= x.auto.1 { note(ui, l("Target must be above the trigger"), dn()); }
            ui.add_space(10.0);
            ui.columns(2, |c| {
                if c[0].add_sized([c[0].available_width(), 32.0], egui::Button::new(l("Cancel"))).clicked() { close = true; }
                if big_button(&mut c[1], l("Confirm Transfer"), accent(), valid && !busy) { confirm = true; }
            });
        });
        if m.should_close() { close = true; }
        if let Some(e) = new_ex {
            let w = wallets.iter().find(|w| s(&w["ex"]) == e);
            if w.is_some_and(|w| w["updated_ms"].is_null() && w["loading"] != true) { let _ = call(json!({"op": "load_wallets", "ex": e})); }
            *x = Xfer { ex: e, from: String::new(), to: String::new(), coin: "USDT".into(), amount: String::new(), auto: (false, 0.0, 0.0), auto_loaded: false, err: None };
        }
        if save {
            if let Err(e) = call(json!({"op": "set_auto", "ex": x.ex, "enabled": x.auto.0, "min": x.auto.1, "target": x.auto.2})) { x.err = Some(e); }
        }
        if confirm {
            let mut args = json!({"op": "transfer", "ex": x.ex, "from": x.from, "to": x.to, "coin": x.coin, "amount": amt.min(free)});
            if let Some(p) = coin.as_ref().and_then(|c| c["product"].as_str()) { args["product"] = p.into(); }
            match call(args) { Ok(_) => close = true, Err(e) => x.err = Some(e) }
        }
        if close { self.xfer = None; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_like_the_swift_tables() {
        assert_eq!(qty(1234.56), "1,234.6");
        assert_eq!(qty(0.5), "0.50000");
        assert_eq!(signed_d(-0.001, 2), "0.00");
        assert_eq!(signed_d(1.5, 2), "+1.50");
        let rows = [json!({"ts": 2, "symbol": "B"}), json!({"ts": 1, "symbol": "A"})];
        assert_eq!(cmp(Key::F("ts"), &rows[0], &rows[1]), Ordering::Greater);
        assert_eq!(cmp(Key::S("symbol"), &rows[0], &rows[1]), Ordering::Greater);
    }
}
