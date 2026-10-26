//! Toolbar and menu primitives painted locally: pills, buttons, menu rows, glyphs.
// ---------------------------------------------------------------------- chrome primitives
// Local to the chart (the kit has no pill / menu row / painted glyph yet).
use super::*;

/// Interval pill (design: 26 px, 12.5; selected = hl fill, semibold, strong text).
pub(super) fn pill(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = if on { prop_s(12.5) } else { prop_m(12.5) };
    let w = if text.is_empty() { 22.0 } else { ui.painter().layout_no_wrap(text.to_string(), prop_s(12.5), fg()).size().x + 16.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
    if on { ui.painter().rect_filled(r, kit::rad(), hl()); } else if hv > 0.0 { ui.painter().rect_filled(r, kit::rad(), hl().gamma_multiply(0.6 * hv)); }
    if !text.is_empty() { ui.painter().text(r.center(), Align2::CENTER_CENTER, text, font, if on { fg_strong() } else { lerp(mu(), fg(), hv) }); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tb { Outline, Ghost, Tint }

/// Toolbar button, 28 px: outlined (menus), text only, or accent-tinted (armed alerts, replay on);
/// `badge` > 0 adds a count pill, `chev` a down chevron.
pub(super) fn tb_button(ui: &mut Ui, text: &str, look: Tb, badge: usize, chev: bool) -> egui::Response {
    let font = if look == Tb::Tint { prop_s(12.0) } else { prop_m(12.5) };
    let g = ui.painter().layout_no_wrap(text.to_string(), font, fg());
    let bg = (badge > 0 && look != Tb::Tint).then(|| ui.painter().layout_no_wrap(badge.to_string(), mono_s(11.0), fg()));
    let w = g.size().x + 20.0 + bg.as_ref().map_or(0.0, |b| b.size().x + 16.0) + if chev { 14.0 } else { 0.0 } + if look == Tb::Tint && badge > 0 { 14.0 } else { 0.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, kit::H), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    let (fill, stroke, col) = match look {
        Tb::Outline => (lerp(panel2(), hl(), hv), Stroke::new(1.0, lerp(line(), edge(), hv)), lerp(fg(), fg_strong(), hv)),
        Tb::Ghost => (hl().gamma_multiply(0.7 * hv), Stroke::NONE, lerp(mu(), fg_strong(), hv)),
        Tb::Tint => (accent().gamma_multiply(0.16 + 0.06 * hv), Stroke::NONE, accent()),
    };
    p.rect(r, kit::rad(), fill, stroke, egui::StrokeKind::Inside);
    let mut x = r.left() + 10.0;
    p.text(pos2(x, r.center().y), Align2::LEFT_CENTER, text, if look == Tb::Tint { prop_s(12.0) } else { prop_m(12.5) }, col);
    x += g.size().x;
    if look == Tb::Tint && badge > 0 { p.text(pos2(x + 5.0, r.center().y), Align2::LEFT_CENTER, badge.to_string(), mono_s(12.0), col); }
    if let Some(b) = bg {
        let br = Rect::from_min_size(pos2(x + 6.0, r.center().y - 9.0), vec2(b.size().x + 12.0, 18.0));
        p.rect_filled(br, 5, accent().gamma_multiply(0.2));
        p.text(br.center(), Align2::CENTER_CENTER, badge.to_string(), mono_s(11.0), lerp(accent(), Color32::WHITE, 0.35));
    }
    if chev { chevron_down(p, pos2(r.right() - 13.0, r.center().y), mu()); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small down chevron centered on `c`.
pub(super) fn chevron_down(p: &egui::Painter, c: Pos2, col: Color32) {
    p.add(egui::Shape::line(vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)], Stroke::new(1.3, col)));
}

/// Vertical hairline between toolbar groups.
pub(super) fn vsep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 18.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, line()));
}

/// Menu body (design: popups radius 6 with a soft shadow come from the style): 28 px rows.
pub(super) fn menu_rows(ui: &mut Ui, w: f32) {
    // a cap as well: anything that fills the available width would otherwise take the screen
    ui.set_min_width(w);
    ui.set_max_width(w + 120.0);
    let s = ui.spacing_mut();
    s.interact_size.y = 26.0;
    s.button_padding = vec2(8.0, 0.0);
    s.item_spacing.y = 2.0;
    s.icon_spacing = 8.0;
}

/// Section overline inside a menu.
pub(super) fn menu_head(ui: &mut Ui, s: &str) {
    ui.add_space(2.0);
    ui.horizontal(|ui| { ui.add_space(8.0); kit::overline(ui, s); });
}

/// Hairline between menu sections.
pub(super) fn menu_sep(ui: &mut Ui) {
    // as wide as the menu's content so far: available_width in a popup is the whole screen on
    // its first frame, and the popup keeps whatever width that frame took
    let (r, _) = ui.allocate_exact_size(vec2(ui.min_rect().width(), 7.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, line()));
}

/// A 28 px menu row: check mark (or `glyph`) column, label; hover fill.
pub(super) fn menu_row(ui: &mut Ui, on: bool, text: &str, glyph: Option<Glyph>) -> egui::Response {
    let tw = ui.painter().layout_no_wrap(text.to_string(), prop(12.5), fg()).size().x + 44.0;
    let (r, resp) = ui.allocate_exact_size(vec2(ui.min_rect().width().max(tw), 26.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.08);
    let p = ui.painter();
    if hv > 0.0 { p.rect_filled(r, kit::rad(), hl().gamma_multiply(hv)); }
    let lead = Rect::from_center_size(pos2(r.left() + 17.0, r.center().y), vec2(18.0, 18.0));
    match glyph {
        Some(g) => paint_glyph(p, lead, g, if on { accent() } else { mu() }),
        None if on => { p.add(egui::Shape::line(vec![lead.center() + vec2(-4.0, 0.0), lead.center() + vec2(-1.0, 3.0), lead.center() + vec2(4.5, -3.5)], Stroke::new(1.5, accent()))); }
        None => {}
    }
    p.text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, text, prop(12.5), if on || hv > 0.5 { fg_strong() } else { fg() });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Painted toolbar glyphs.
#[derive(Clone, Copy)]
pub(super) enum Glyph { Tool(Tool), Trash, Eye(bool), More, Layout(&'static str) }

/// Drawing-tool button on the toolbar strip (design: 28 x 24; selected = hl fill, strong glyph).
pub(super) fn tool_button(ui: &mut Ui, g: Glyph, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(28.0, 24.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hov"), resp.hovered() && !on, 0.1);
    let p = ui.painter();
    if on { p.rect_filled(r, kit::rad(), hl()); } else if hv > 0.0 { p.rect_filled(r, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
    paint_glyph(p, r, g, if on { fg_strong() } else { lerp(mu(), fg_strong(), hv) });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Paints `g` centered in `r` (glyphs are ~14 px).
fn paint_glyph(p: &egui::Painter, r: Rect, g: Glyph, col: Color32) {
    let s = Stroke::new(1.3, col);
    let c = r.center();
    match g {
        Glyph::Tool(Tool::HLine) => {
            p.hline(egui::Rangef::new(c.x - 7.0, c.x + 7.0), c.y, s);
            p.circle_filled(c, 2.0, col);
        }
        Glyph::Tool(Tool::Trend) => {
            let (a, b) = (c + vec2(-6.0, 5.0), c + vec2(6.0, -5.0));
            p.line_segment([a, b], s);
            p.circle_stroke(a, 1.8, s);
            p.circle_stroke(b, 1.8, s);
        }
        Glyph::Tool(Tool::Ray) => {
            let (a, b) = (c + vec2(-6.0, 5.0), c + vec2(6.5, -5.5));
            p.line_segment([a, b], s);
            p.circle_stroke(a, 1.8, s);
            p.line_segment([b, b + vec2(-4.5, 0.3)], s);
            p.line_segment([b, b + vec2(-0.3, 4.5)], s);
        }
        Glyph::Tool(Tool::Fib) => {
            for (k, dy) in [-5.0f32, -1.7, 1.7, 5.0].into_iter().enumerate() { p.hline(egui::Rangef::new(c.x - 7.0, c.x + 7.0 - (k % 2) as f32 * 3.0), c.y + dy, Stroke::new(1.2, col)); }
        }
        Glyph::Tool(Tool::Rect) => { p.rect_stroke(Rect::from_center_size(c, vec2(13.0, 10.0)), 1, s, egui::StrokeKind::Middle); }
        Glyph::Tool(Tool::Channel) => {
            p.line_segment([c + vec2(-7.0, 2.0), c + vec2(5.0, -6.0)], s);
            p.line_segment([c + vec2(-5.0, 6.0), c + vec2(7.0, -2.0)], s);
        }
        Glyph::Tool(Tool::Text) => { p.text(c, Align2::CENTER_CENTER, "T", prop_m(13.5), col); }
        Glyph::Tool(Tool::Position) => {
            p.rect_stroke(Rect::from_min_max(c + vec2(-7.0, -6.0), c + vec2(7.0, 0.0)), 1, Stroke::new(1.2, up()), egui::StrokeKind::Middle);
            p.rect_stroke(Rect::from_min_max(c + vec2(-7.0, 0.0), c + vec2(7.0, 5.0)), 1, Stroke::new(1.2, dn()), egui::StrokeKind::Middle);
        }
        Glyph::Tool(Tool::Measure) => {
            p.vline(c.x, egui::Rangef::new(c.y - 6.5, c.y + 6.5), s);
            for d in [-1.0f32, 1.0] {
                let tip = c + vec2(0.0, 6.5 * d);
                p.line_segment([tip, tip + vec2(-3.0, -3.0 * d)], s);
                p.line_segment([tip, tip + vec2(3.0, -3.0 * d)], s);
            }
        }
        Glyph::Trash => {
            p.hline(egui::Rangef::new(c.x - 5.5, c.x + 5.5), c.y - 4.5, s);
            p.hline(egui::Rangef::new(c.x - 2.0, c.x + 2.0), c.y - 6.5, s);
            p.line_segment([c + vec2(-4.0, -4.5), c + vec2(-3.2, 6.0)], s);
            p.line_segment([c + vec2(4.0, -4.5), c + vec2(3.2, 6.0)], s);
            p.hline(egui::Rangef::new(c.x - 3.2, c.x + 3.2), c.y + 6.0, s);
        }
        Glyph::Eye(hidden) => {
            // almond outline from two arcs, the pupil; struck through when drawings are hidden
            let arc = |up: f32| (0..=12).map(|k| { let t = k as f32 / 12.0; pos2(c.x - 7.0 + 14.0 * t, c.y + up * 4.5 * (std::f32::consts::PI * t).sin()) }).collect::<Vec<_>>();
            p.add(egui::Shape::line(arc(-1.0), s));
            p.add(egui::Shape::line(arc(1.0), s));
            p.circle_stroke(c, 2.0, s);
            if hidden { p.line_segment([c + vec2(-6.0, 6.0), c + vec2(6.0, -6.0)], s); }
        }
        Glyph::More => { for dx in [-4.5f32, 0.0, 4.5] { p.circle_filled(c + vec2(dx, 0.0), 1.3, col); } }
        Glyph::Layout(id) => layout_glyph(p, Rect::from_center_size(c, vec2(15.0, 11.0)), id, col),
    }
}

/// Small picture of a chart layout ("1", "2", "2v", "3", "4") inside `r`.
pub(super) fn layout_glyph(p: &egui::Painter, r: Rect, id: &str, col: Color32) {
    let s = Stroke::new(1.3, col);
    p.rect_stroke(r, 2, s, egui::StrokeKind::Inside);
    let (cx, cy) = (r.center().x, r.center().y);
    match id {
        "2" => { p.vline(cx, r.y_range(), s); }
        "2v" => { p.hline(r.x_range(), cy, s); }
        "3" => { p.vline(cx, r.y_range(), s); p.hline(cx..=r.right(), cy, s); }
        "4" => { p.vline(cx, r.y_range(), s); p.hline(r.x_range(), cy, s); }
        _ => {}
    }
}
