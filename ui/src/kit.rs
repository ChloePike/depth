//! The component kit: every panel draws its controls through these, so sizes, weights, colors
//! and motion are the same everywhere (design: "Depth UI design" canvas, TradingView-style
//! neutral black). Sizes: compact controls 24 px, default 28 px, primary actions 36 px; radius
//! 4 on controls and panels, 6 on dialogs, 8 on menus; one hairline (`line()`); hover / selection fade in
//! 100-160 ms. Text: labels 12 regular grey, values 12.5 medium, titles 13.5 semibold.
use super::theme::*;
use eframe::egui::{self, Align2, Color32, Rect, Response, Sense, Stroke, Ui};

/// Control heights.
pub const H_SM: f32 = 24.0;
pub const H: f32 = 28.0;
pub const H_LG: f32 = 36.0;
pub const R: u8 = 4;

/// Button looks: `Fill(c)` solid (buy / sell / accent), `Secondary` outlined, `Ghost` text only
/// until hovered, `Danger` tinted red, `Link` accent text.
#[derive(Clone, Copy, PartialEq)]
pub enum Look { Fill(Color32), Secondary, Ghost, Danger, Link }

/// A button of `look`, `h` high, `w` wide (None = fits its text, f32::INFINITY = all the width
/// left). Disabled when the ui is disabled or `enabled` is false.
pub fn button(ui: &mut Ui, text: &str, look: Look, h: f32, w: Option<f32>, enabled: bool) -> Response {
    let enabled = enabled && ui.is_enabled();
    let font = match look { Look::Fill(_) => prop_s(if h >= H_LG { 13.5 } else { 12.5 }), Look::Link => prop_m(12.5), _ => prop_m(12.5) };
    let g = ui.painter().layout_no_wrap(text.to_string(), font.clone(), fg());
    let width = match w { Some(x) if x.is_finite() => x, Some(_) => ui.available_width(), None => g.size().x + 20.0 };
    let (r, resp) = ui.allocate_exact_size(egui::vec2(width, h), if enabled { Sense::click() } else { Sense::hover() });
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), enabled && resp.hovered(), 0.1);
    let down = enabled && resp.is_pointer_button_down_on();
    let p = ui.painter();
    let (fill, stroke, text_c) = match look {
        Look::Fill(c) => (if down { c.gamma_multiply(0.85) } else { lerp(c, c.gamma_multiply(1.15), hv) }, Stroke::NONE, Color32::WHITE),
        Look::Secondary => (lerp(panel2(), hl(), hv), Stroke::new(1.0, lerp(line(), edge(), hv)), lerp(fg(), fg_strong(), hv)),
        Look::Ghost => (hl().gamma_multiply(0.7 * hv), Stroke::NONE, lerp(mu(), fg_strong(), hv)),
        Look::Danger => (dn().gamma_multiply(0.14 + 0.08 * hv), Stroke::NONE, lerp(dn(), dn().gamma_multiply(1.25), hv)),
        Look::Link => (Color32::TRANSPARENT, Stroke::NONE, lerp(accent(), accent().gamma_multiply(1.2), hv)),
    };
    let a = if enabled { 1.0 } else { 0.35 };
    p.rect(r, R, fill.gamma_multiply(a), stroke, egui::StrokeKind::Inside);
    let g = p.layout_no_wrap(text.to_string(), font, text_c.gamma_multiply(if enabled { 1.0 } else { 0.5 }));
    p.galley(r.center() - g.size() / 2.0, g, text_c);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Icon button: `size` square, the glyph painted by `paint(painter, rect, color)`; `on` = active tool.
pub fn icon_button(ui: &mut Ui, size: f32, on: bool, paint: impl FnOnce(&egui::Painter, Rect, Color32)) -> Response {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(size, size), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    if on { p.rect_filled(r, R, hl()); } else if hv > 0.0 { p.rect_filled(r, R, hl().gamma_multiply(0.7 * hv)); }
    paint(p, r, if on { accent() } else { lerp(mu(), fg_strong(), hv) });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Text field in a well (28 px): label inside on the left (12 grey), value right-aligned (13.5 medium
/// numbers), unit on the right; accent border with a soft ring when focused, red when `error`.
/// Returns the edit response and whether Enter was pressed.
pub fn field(ui: &mut Ui, label: &str, text: &mut String, unit: &str, id: impl std::hash::Hash + std::fmt::Debug, error: bool) -> (Response, bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), H), Sense::hover());
    let eid = ui.id().with(("kit_field", id));
    let focused = ui.memory(|m| m.has_focus(eid));
    let f = ui.ctx().animate_bool_with_time(eid.with("f"), focused, 0.12);
    let p = ui.painter();
    if f > 0.0 { p.rect_filled(rect.expand(2.5 * f), R + 2, accent().gamma_multiply(0.14 * f)); }
    let border = if error { dn() } else { lerp(line(), accent(), f) };
    p.rect(rect, R, if ui.is_enabled() { well() } else { panel2() }, Stroke::new(1.0, border), egui::StrokeKind::Inside);
    let lw = p.text(rect.left_center() + egui::vec2(10.0, 0.0), Align2::LEFT_CENTER, label, prop(12.0), mu()).width();
    let uw = p.text(rect.right_center() - egui::vec2(10.0, 0.0), Align2::RIGHT_CENTER, unit, prop(12.0), dim()).width();
    let edit = Rect::from_min_max(egui::pos2(rect.left() + lw + 18.0, rect.top() + 4.0), egui::pos2(rect.right() - uw - 18.0, rect.bottom() - 4.0));
    let r = ui.put(edit, egui::TextEdit::singleline(text).id(eid).frame(egui::Frame::NONE).font(mono_m(13.5)).text_color(fg_strong())
        .horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center));
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    (r, enter)
}

/// Segmented control filling the width; the selected thumb slides. Returns true when changed.
pub fn segments<T: PartialEq + Copy>(ui: &mut Ui, sel: &mut T, items: &[(T, &str)], h: f32) -> bool {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::hover());
    ui.painter().rect_filled(rect, R + 1, panel2());
    let w = (rect.width() - 4.0) / items.len() as f32;
    let cell = |i: f32| Rect::from_min_size(egui::pos2(rect.left() + 2.0 + w * i, rect.top() + 2.0), egui::vec2(w, rect.height() - 4.0));
    if let Some(k) = items.iter().position(|(v, _)| *v == *sel) {
        let x = ui.ctx().animate_value_with_time(ui.id().with(("kit_seg", items.len(), items[0].1)), k as f32, 0.15);
        let r = cell(x);
        ui.painter().rect_filled(r.translate(egui::vec2(0.0, 1.0)), R, Color32::from_black_alpha(60));
        ui.painter().rect_filled(r, R, hl());
    }
    let mut changed = false;
    for (i, (v, name)) in items.iter().enumerate() {
        let r = cell(i as f32);
        let resp = ui.interact(r, ui.id().with(("kit_seg_i", name, i)), Sense::click());
        let on = *sel == *v;
        let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.15);
        let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
        let font = if on { prop_s(12.5) } else { prop_m(12.5) };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *name, font, lerp(mu(), fg_strong(), t.max(hv * 0.7)));
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { *sel = *v; changed = true; }
    }
    changed
}

/// Underlined tab (13, semibold when selected, grey medium otherwise); the line grows in.
pub fn tab(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let font = if selected { prop_s(13.0) } else { prop_m(13.0) };
    let g = ui.painter().layout_no_wrap(text.to_string(), prop_s(13.0), fg());
    let (rect, resp) = ui.allocate_exact_size(g.size() + egui::vec2(16.0, 12.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id.with("hv"), selected || resp.hovered(), 0.12);
    ui.painter().text(rect.center() - egui::vec2(0.0, 1.0), Align2::CENTER_CENTER, text, font, lerp(mu(), fg_strong(), h));
    let s = ui.ctx().animate_bool_with_time(resp.id.with("sel"), selected, 0.16);
    if s > 0.0 {
        let half = (rect.width() / 2.0 - 8.0) * s;
        ui.painter().hline(egui::Rangef::new(rect.center().x - half, rect.center().x + half), rect.bottom() - 1.0, Stroke::new(2.0, accent()));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Switch: 30 x 16, accent when on, the knob slides.
pub fn switch(ui: &mut Ui, on: &mut bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(30.0, 16.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.14);
    let p = ui.painter();
    p.rect_filled(r, 8, lerp(track(), accent(), t));
    let x = egui::lerp((r.left() + 8.0)..=(r.right() - 8.0), t);
    p.circle_filled(egui::pos2(x, r.center().y), 6.0, lerp(Color32::from_gray(184), Color32::WHITE, t));
    let clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    if clicked { *on = !*on; }
    clicked
}

/// Tone of a chip.
#[derive(Clone, Copy)]
pub enum Tone { Neutral, Outline, Up, Down, Warn, Accent }

/// Small tag: 20 px, radius 3, 11.5 medium (semibold for signal tones).
pub fn chip(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let (fill, stroke, col, font) = match tone {
        Tone::Neutral => (hl(), Stroke::NONE, fg(), prop_m(11.5)),
        Tone::Outline => (Color32::TRANSPARENT, Stroke::new(1.0, line()), fg(), mono_s(11.5)),
        Tone::Up => (up().gamma_multiply(0.14), Stroke::NONE, up(), prop_s(11.0)),
        Tone::Down => (dn().gamma_multiply(0.14), Stroke::NONE, dn(), prop_s(11.0)),
        Tone::Warn => (WARN.gamma_multiply(0.14), Stroke::NONE, WARN, prop_s(11.0)),
        Tone::Accent => (accent().gamma_multiply(0.16), Stroke::NONE, accent(), prop_s(11.0)),
    };
    let g = ui.painter().layout_no_wrap(text.to_string(), font, col);
    let (r, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 14.0, 20.0), Sense::hover());
    ui.painter().rect(r, 3, fill, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(r.center() - g.size() / 2.0, g, col);
    resp
}

/// Overline: 10.5 semibold caps, dim (section labels, stat captions).
pub fn overline(ui: &mut Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text.to_uppercase()).font(prop_s(10.5)).color(dim()))
}

/// Section title: 13.5 semibold.
pub fn title(ui: &mut Ui, text: &str) -> Response { ui.label(egui::RichText::new(text).font(prop_s(13.5)).color(fg_strong())) }

/// "label ........ value" row: label 12 grey, value 12.5 medium numbers in `col`.
pub fn kv(ui: &mut Ui, k: &str, v: String, col: Color32) -> Response {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(k).font(prop(12.0)).color(mu()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(egui::RichText::new(v).font(mono_m(12.5)).color(col)); });
    }).response
}

/// Hint / note text: 11.5, wraps.
pub fn note(ui: &mut Ui, text: &str, col: Color32) { ui.add(egui::Label::new(egui::RichText::new(text).font(prop(11.5)).color(col)).wrap()); }

/// Flat panel (radius 4, no border: the black gutters separate panels).
pub fn panel_frame(margin: i8) -> egui::Frame { egui::Frame::new().fill(panel()).corner_radius(R).inner_margin(egui::Margin::same(margin)) }

/// A group inside a panel: panel2 fill, hairline, radius 4.
pub fn group<T>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> T) -> T {
    egui::Frame::new().fill(panel2()).stroke(Stroke::new(1.0, line())).corner_radius(R).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        add(ui)
    }).inner
}

/// Table header cell text: 11 medium grey; `sorted` adds the arrow and brightens it.
pub fn th(p: &egui::Painter, pos: egui::Pos2, align: Align2, text: &str, sorted: Option<bool>) {
    let t = match sorted { Some(true) => format!("{text} ↑"), Some(false) => format!("{text} ↓"), None => text.to_string() };
    p.text(pos, align, t, prop_m(11.0), if sorted.is_some() { fg() } else { mu() });
}
