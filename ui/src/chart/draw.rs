//! User drawings: tools, the drawing model with its price alerts, placing / editing / painting
//! them and their right-click menu.
use super::*;
use super::chrome::{menu_rows, menu_sep};
use super::fmt::fmt_qty_chart;

pub(super) use charting::draw::{position_qty, Drawing, Menu, Tool, DRAW_COLORS, LOCKED};

impl Chart {
    /// Records each finished drawing edit (a whole drag is one step) and applies Cmd+Z / Cmd+Shift+Z.
    fn undo_redo(&mut self, ui: &Ui, base: &str) {
        let cur = self.drawings.get(base).cloned().unwrap_or_default();
        match &self.committed {
            Some((b, prev)) if b == base => {
                if *prev != cur && self.drag.is_none() {
                    self.undo.push(prev.clone());
                    if self.undo.len() > 100 { self.undo.remove(0); }
                    self.redo.clear();
                    self.committed = Some((base.to_string(), cur.clone()));
                }
            }
            // another symbol: its history starts fresh
            _ => { self.undo.clear(); self.redo.clear(); self.committed = Some((base.to_string(), cur.clone())); }
        }
        let (undo, redo) = ui.input(|i| {
            let z = i.modifiers.command && i.key_pressed(egui::Key::Z);
            (z && !i.modifiers.shift, z && i.modifiers.shift)
        });
        let (from, to) = if undo { (&mut self.undo, &mut self.redo) } else if redo { (&mut self.redo, &mut self.undo) } else { return };
        let Some(prev) = from.pop() else { return };
        to.push(cur);
        self.drawings.insert(base.to_string(), prev.clone());
        self.committed = Some((base.to_string(), prev));
        self.hot = None;
        self.menu = None;
    }

    /// Place, draw, edit and delete drawings. Click places points for the armed tool (one for a
    /// horizontal line or a note, three for a channel, two otherwise); drag a handle to move that
    /// point or the body to move all of it; right-click opens the drawing's menu (color, style,
    /// alert, lock, delete) or, on the empty plot, the chart menu; Delete removes; Escape disarms.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn drawings_ui(&mut self, ui: &Ui, resp: &egui::Response, painter: &egui::Painter, bars: &[Bar], fr: &Frame, main: Rect, ps: PScale, base: &str) {
        let ms = self.tf * 60_000;
        let plot = ps.r;
        let y = |v: f64| ps.y(v);
        let to_screen = |(t, p): (i64, f64)| pos2(fr.x(idx_of(bars, t, ms)), y(p));
        let to_point = |p: Pos2| (time_of(bars, fr.idx(p.x), ms), ps.v(p.y));
        let typing = ui.ctx().egui_wants_keyboard_input();
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) { self.tool = None; self.pending.clear(); }
        if !typing { self.undo_redo(ui, base); }

        let ptr = resp.hover_pos().filter(|p| plot.contains(*p));
        let (hidden, sound, equity, risk_pct) = (self.hide, self.alert_sound.clone(), self.equity, self.risk_pct);
        let list = self.drawings.entry(base.to_string()).or_default();
        let geom = charting::draw::Geom { to_screen: &to_screen, y: &y, main };

        // editing: the part grabbed at drag start follows the pointer
        if let (Some((i, part)), true) = (self.drag, resp.dragged()) {
            if let Some(d) = list.get_mut(i) {
                let delta = resp.drag_delta();
                d.drag_part(part, |pt: (i64, f64)| to_point(to_screen(pt) + delta));
            }
        }

        let note = |d: &Drawing| charting::draw::note(painter, d);
        let note_rect = |d: &Drawing, gl: &egui::Galley| charting::draw::note_rect(d, &geom, gl);
        // part under the pointer: a handle first, then the nearest line (or a box's inside); locked drawings only answer
        // right-clicks
        let hot = ptr.filter(|_| self.tool.is_none() && !hidden).and_then(|p| charting::draw::hit(list, p, &geom, &|d| note_rect(d, &note(d))));
        let hot = self.drag.or(hot);

        let click = resp.interact_pointer_pos().filter(|p| plot.contains(*p));
        if let (Some(tool), true, Some(p)) = (self.tool, resp.clicked(), click) {
            self.pending.push(to_point(p));
            if self.pending.len() >= tool.points() {
                let pts = std::mem::take(&mut self.pending);
                list.push(Drawing::placed(tool, &pts));
                self.tool = None;
                // a note is typed right away
                if tool == Tool::Text { self.menu = Some((Menu::Drawing(list.len() - 1, true), p)); }
            }
        } else if resp.secondary_clicked() {
            if self.tool.is_some() { self.tool = None; self.pending.clear(); }
            else if let Some(p) = click {
                self.menu = Some((match hot { Some((i, _)) => Menu::Drawing(i, false), None => Menu::Chart(to_point(p)) }, p));
            }
        } else if let (Some((i, part)), None, false) = (hot, self.drag, typing) {
            if part != LOCKED && ui.input(|x| x.key_pressed(egui::Key::Delete) || x.key_pressed(egui::Key::Backspace)) {
                list.remove(i);
                self.hot = None;
                self.menu = None;
                return;
            }
        }

        self.drawing_menu(ui, resp, base, sound);
        let list = self.drawings.entry(base.to_string()).or_default();
        if hidden { self.hot = None; return; }

        let clip = painter.with_clip_rect(plot);
        let (fqty, fusd) = (|q: f64| fmt_qty_chart(q), |v: f64| fmt_usd_short(v));
        let pcx = charting::draw::PaintCx { clip: &clip, painter, plot, axis_w: AXIS_W, ms, base: &self.base_unit, equity, risk_pct, fmt_px: &fmt_px, fmt_qty: &fqty, fmt_usd: &fusd };
        let draw = |d: &Drawing, emph: bool, alpha: f32| {
            charting::draw::paint(&pcx, &geom, d, emph, alpha);
            let col = d.color().linear_multiply(alpha);
            // an armed alert: bell tag where the line meets the axis side of the plot
            if let (true, Some(lv)) = (d.meta.is_some(), d.level_at(bars.last().map_or(0, |b| b.t))) {
                let ay = y(lv);
                let g = painter.layout_no_wrap(format!("⏰ {}", fmt_px(lv)), mono(10.5), Color32::WHITE);
                let r = Rect::from_min_size(pos2(main.right() - g.size().x - 14.0, ay - 9.0), vec2(g.size().x + 10.0, 18.0));
                clip.rect_filled(r, 3, col);
                clip.galley(r.min + vec2(5.0, (18.0 - g.size().y) / 2.0), g, Color32::WHITE);
            }
        };
        for (i, d) in list.iter().enumerate() { draw(d, hot.is_some_and(|h| h.0 == i), 1.0); }
        // preview of a drawing in progress
        if let (Some(tool), Some(p)) = (self.tool, ptr) {
            let pt = to_point(p);
            let mut pts = self.pending.clone();
            pts.push(pt);
            if tool == Tool::Channel && pts.len() == 2 { pts.push(pt); }
            let pv = (pts.len() >= tool.points().min(2)).then(|| Drawing::placed(tool, &pts));
            if let Some(d) = pv { draw(&d, false, 0.6); }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        match hot {
            Some((_, 0 | 1 | 3)) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
            Some((_, LOCKED)) => {}
            Some(_) => { ui.ctx().set_cursor_icon(egui::CursorIcon::Move); if self.drag.is_none() && self.menu.is_none() { resp.clone().on_hover_text(t("draw.delete_tip")); } }
            None => {}
        }
        self.hot = hot;
    }

    /// The right-click menu: a drawing's colour, style, alert, lock and delete, or (on the empty
    /// plot) an alert or line at the price, replay from there, view reset.
    fn drawing_menu(&mut self, ui: &Ui, resp: &egui::Response, base: &str, sound: String) {
        // the right-click menu (egui's menu popup, so the sound submenu does not close it)
        if let Some((m, at)) = self.menu {
            let list = self.drawings.entry(base.to_string()).or_default();
            let mut open = true;
            let mut del = false;
            egui::Popup::menu(resp).id(ui.id().with("chart_menu")).at_position(at).open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                menu_rows(ui, 220.0);
                match m {
                    Menu::Drawing(i, focus) => {
                        let Some(d) = list.get_mut(i) else { ui.close(); return };
                        if d.tool == Tool::Text {
                            let r = ui.add(egui::TextEdit::singleline(&mut d.text).hint_text(t("draw.text_hint")).desired_width(200.0));
                            if focus { r.request_focus(); }
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) { ui.close(); }
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for c in DRAW_COLORS {
                                let (r, sw) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                                ui.painter().rect_filled(r.shrink(1.0), 3, rgb(c));
                                if d.color() == rgb(c) { ui.painter().rect_stroke(r, 3, Stroke::new(1.5, fg()), egui::StrokeKind::Inside); }
                                if sw.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { d.col = c; }
                            }
                        });
                        if !matches!(d.tool, Tool::Text | Tool::Measure) {
                            ui.horizontal(|ui| {
                                for (k, lab) in [(0u8, "draw.solid"), (1, "draw.dashed"), (2, "draw.dotted")] {
                                    if ui.selectable_label(d.dash == k, t(lab)).clicked() { d.dash = k; }
                                }
                            });
                        }
                        if d.alertable() {
                            menu_sep(ui);
                            let mut on = d.meta.is_some();
                            if ui.checkbox(&mut on, t("alert.on")).on_hover_text(t("alert.tip")).changed() { d.meta = on.then(|| sound.clone()); }
                            if let Some(s) = &mut d.meta { sound_menu(ui, s); }
                        }
                        if d.tool == Tool::Position {
                            menu_sep(ui);
                            ui.horizontal(|ui| { ui.label(t("pos.risk")); ui.add(egui::DragValue::new(&mut self.risk_pct).range(0.05..=100.0).speed(0.05).suffix("%")); });
                            if self.trading && ui.button(t("pos.use")).clicked() {
                                self.actions.push(ChartAct::Position { entry: d.a.1, stop: d.b.1, target: d.c.1, qty: position_qty(self.equity, self.risk_pct, d.a.1, d.b.1) });
                                ui.close();
                            }
                        }
                        menu_sep(ui);
                        ui.checkbox(&mut d.locked, t("draw.lock"));
                        if ui.button(t("draw.delete")).clicked() { del = true; ui.close(); }
                        if focus { self.menu = Some((Menu::Drawing(i, false), at)); }
                    }
                    Menu::Chart((ts, px)) => {
                        if ui.button(format!("{}  {}", t("alert.add_at"), fmt_px(px))).clicked() {
                            let mut d = Drawing::new(Tool::HLine, (ts, px), (ts, px));
                            d.meta = Some(sound.clone());
                            list.push(d);
                            ui.close();
                        }
                        if ui.button(format!("{}  {}", t("draw.hline_at"), fmt_px(px))).clicked() { list.push(Drawing::new(Tool::HLine, (ts, px), (ts, px))); ui.close(); }
                        if ui.button(t("replay.from_here")).clicked() { self.replay = Some(Replay::new(Some(ts), ui.input(|i| i.time))); ui.close(); }
                        menu_sep(ui);
                        if ui.button(t("chart.reset")).clicked() { self.right = 0.0; self.bar_w = 7.0; self.y_range = None; ui.close(); }
                    }
                }
            });
            if del { if let Some((Menu::Drawing(i, _), _)) = self.menu { if i < list.len() { list.remove(i); } } }
            if !open || del { self.menu = None; }
        }
    }

    /// Multi-chart layout: every cell shows the same drawings and alerts; an edit in one reaches all.
    pub fn adopt_drawings(&mut self, from: &Chart) {
        if self.drawings != from.drawings { self.drawings = from.drawings.clone(); }
        self.hide = from.hide;
    }

    /// Bases with an armed alert.
    pub fn alert_bases(&self) -> Vec<String> {
        self.drawings.iter().filter(|(_, v)| v.iter().any(|d| d.meta.is_some())).map(|(b, _)| b.clone()).collect()
    }

    /// Fires every armed alert whose line the price crossed since the last check (once: the alert
    /// disarms, the line stays). `px` holds the latest price per base.
    pub fn check_alerts(&mut self, px: &std::collections::HashMap<String, f64>) {
        let ts = depth::now_ms();
        for (base, list) in self.drawings.iter_mut() {
            let Some(&p) = px.get(base) else { continue };
            let Some(prev) = self.alert_px.insert(base.clone(), p) else { continue };
            for d in list.iter_mut() {
                let (Some(sound), Some(lv)) = (d.meta.clone(), d.level_at(ts)) else { continue };
                if (prev < lv) == (p < lv) { continue; }
                d.meta = None;
                let title = format!("{base} {} {}", if p >= lv { "↑" } else { "↓" }, fmt_px(lv));
                let body = format!("{} {}", t("alert.crossed"), fmt_px(p));
                play_sound(&sound);
                self.toast = Some((format!("⏰ {title} · {body}"), std::time::Instant::now() + std::time::Duration::from_secs(8), accent()));
                self.fired.push((title, body));
            }
        }
    }
}

/// Size (base units) that loses `risk_pct` of `equity` between `entry` and `stop`; None without
/// equity or a stop distance.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_fire_once_on_crossing() {
        let mut c = Chart::default();
        let mut d = Drawing::new(Tool::HLine, (0, 100.0), (0, 100.0));
        d.meta = Some("none".into());
        c.drawings.insert("BTC".into(), vec![d]);
        let px = |p: f64| std::collections::HashMap::from([("BTC".to_string(), p)]);
        c.check_alerts(&px(99.0));
        c.check_alerts(&px(99.5));
        assert!(c.fired.is_empty());
        c.check_alerts(&px(100.0));
        assert_eq!(c.fired.len(), 1);
        assert!(c.drawings["BTC"][0].meta.is_none());
        c.check_alerts(&px(98.0));
        assert_eq!(c.fired.len(), 1);
        assert_eq!(c.alert_bases(), Vec::<String>::new());
    }
}
