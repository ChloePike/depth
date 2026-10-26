//! User drawings: tools, the drawing model with its price alerts, placing / editing / painting
//! them and their right-click menu.
use super::*;
use super::chrome::{menu_rows, menu_sep};
use super::fmt::{fmt_qty_chart, span};

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) enum Tool { HLine, Trend, Ray, Fib, Rect, Channel, Text, Measure, Position }

impl Tool {
    const ALL: [Tool; 9] = [Tool::HLine, Tool::Trend, Tool::Ray, Tool::Fib, Tool::Rect, Tool::Channel, Tool::Text, Tool::Measure, Tool::Position];
    pub(super) fn id(self) -> &'static str {
        match self {
            Tool::HLine => "hline", Tool::Trend => "trend", Tool::Ray => "ray", Tool::Fib => "fib", Tool::Rect => "rect",
            Tool::Channel => "channel", Tool::Text => "text", Tool::Measure => "measure", Tool::Position => "position",
        }
    }
    pub(super) fn parse(s: &str) -> Option<Tool> { Tool::ALL.into_iter().find(|t| t.id() == s) }
    /// clicks needed to place it
    fn points(self) -> usize { match self { Tool::HLine | Tool::Text => 1, Tool::Channel => 3, _ => 2 } }
}

/// `hot` part of a locked drawing: right-click opens its menu, nothing moves it.
pub(super) const LOCKED: u8 = 9;

/// What the right-click menu is for: a drawing (index, focus its text field), or the empty plot (point).
#[derive(Clone, Copy)]
pub(super) enum Menu { Drawing(usize, bool), Chart((i64, f64)) }

/// Fibonacci retracement levels drawn between a drawing's two points (0 at the second point).
const FIB: [f64; 7] = [0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0];

/// A user drawing anchored in (bar time ms, price), so it survives timeframe changes.
/// `col` 0xRRGGBB (0 = the accent), `dash` 0 solid / 1 dashed / 2 dotted; `c` is a channel's third
/// point (on its parallel line), `text` a note's text; `alert` = armed price alert with this sound.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Drawing {
    pub(super) tool: Tool, a: (i64, f64), b: (i64, f64),
    #[serde(default)] col: u32, #[serde(default)] dash: u8,
    #[serde(default)] c: (i64, f64), #[serde(default)] text: String, #[serde(default)] locked: bool,
    #[serde(default)] pub(super) alert: Option<String>,
}

impl Drawing {
    fn new(tool: Tool, a: (i64, f64), b: (i64, f64)) -> Self { Drawing { tool, a, b, col: 0, dash: 0, c: a, text: String::new(), locked: false, alert: None } }
    fn color(&self) -> Color32 { if self.col == 0 { accent() } else { rgb(self.col) } }
    /// the a-b line's price at time t (None when a and b share a time)
    fn line_at(&self, t: i64) -> Option<f64> {
        let (ta, tb) = (self.a.0, self.b.0);
        (ta != tb).then(|| self.a.1 + (self.b.1 - self.a.1) * (t - ta) as f64 / (tb - ta) as f64)
    }
    /// a channel's parallel line, as a price offset from the a-b line
    fn offset(&self) -> f64 { self.c.1 - self.line_at(self.c.0).unwrap_or(self.a.1) }
    /// third point stored halfway along the parallel line, so its handle sits there
    fn with_c(mut self, c: (i64, f64)) -> Self {
        let off = c.1 - self.line_at(c.0).unwrap_or(self.a.1);
        let mt = (self.a.0 + self.b.0) / 2;
        self.c = (mt, self.line_at(mt).unwrap_or(self.a.1) + off);
        self
    }
    /// A drawing from its placed points: a channel's third point goes on its parallel line, a
    /// position tool (entry, stop) gets a 2R target.
    fn placed(tool: Tool, pts: &[(i64, f64)]) -> Self {
        let mut d = Drawing::new(tool, pts[0], *pts.get(1).unwrap_or(&pts[0]));
        if let Some(c) = pts.get(2) { d = d.with_c(*c); }
        if tool == Tool::Position { d.c = (d.a.0, d.a.1 * 3.0 - d.b.1 * 2.0); }
        d
    }
    fn alertable(&self) -> bool { matches!(self.tool, Tool::HLine | Tool::Trend | Tool::Ray) }
    /// the line's price at time t, for alerts: a trend line only within its two points, a ray from its start on
    pub(super) fn level_at(&self, t: i64) -> Option<f64> {
        match self.tool {
            Tool::HLine => Some(self.a.1),
            Tool::Trend if t >= self.a.0.min(self.b.0) && t <= self.a.0.max(self.b.0) => self.line_at(t),
            Tool::Ray if (t - self.a.0).signum() * (self.b.0 - self.a.0).signum() >= 0 => self.line_at(t),
            _ => None,
        }
    }
}

/// Swatches of the drawing context menu (TradingView's default palette row).
const DRAW_COLORS: [u32; 8] = [0x2962ff, 0x089981, 0xf23645, 0xff9800, 0xffeb3b, 0x9c27b0, 0x00bcd4, 0x787b86];

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    (a + ab * t - p).length()
}

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
        // a channel's middle handle: halfway along its parallel line
        let mid_c = |d: &Drawing| ((d.a.0 + d.b.0) / 2, (d.a.1 + d.b.1) / 2.0 + d.offset());

        // editing: the part grabbed at drag start follows the pointer
        if let (Some((i, part)), true) = (self.drag, resp.dragged()) {
            if let Some(d) = list.get_mut(i) {
                let delta = resp.drag_delta();
                let mv = |pt: (i64, f64)| to_point(to_screen(pt) + delta);
                match part {
                    0 => d.a = mv(d.a),
                    1 => d.b = mv(d.b),
                    3 if d.tool == Tool::Position => d.c.1 = mv((d.a.0, d.c.1)).1,
                    3 => { let m = mid_c(d); d.c = (m.0, mv(m).1); }
                    _ => { d.a = mv(d.a); d.b = mv(d.b); if matches!(d.tool, Tool::Channel | Tool::Position) { d.c = mv(d.c); } }
                }
                if d.tool == Tool::Position { d.c.0 = d.a.0; }
                if matches!(d.tool, Tool::HLine | Tool::Text) { d.b = d.a; }
            }
        }

        // a position tool's horizontal extent (at least 120 px wide)
        let pos_x = |d: &Drawing| { let (x0, x1) = (to_screen(d.a).x, to_screen(d.b).x); (x0.min(x1), x0.max(x1).max(x0.min(x1) + 120.0)) };
        // screen segments of a drawing (for drawing and hit-testing)
        let segs = |d: &Drawing| -> Vec<(Pos2, Pos2)> {
            let (a, b) = (to_screen(d.a), to_screen(d.b));
            match d.tool {
                Tool::HLine => vec![(pos2(main.left(), a.y), pos2(main.right(), a.y))],
                Tool::Trend | Tool::Measure => vec![(a, b)],
                Tool::Ray => { let dir = b - a; let k = if dir.length() > 0.0 { 4000.0 / dir.length() } else { 0.0 }; vec![(a, a + dir * k)] }
                Tool::Rect => { let r = Rect::from_two_pos(a, b); vec![(r.left_top(), r.right_top()), (r.right_top(), r.right_bottom()), (r.right_bottom(), r.left_bottom()), (r.left_bottom(), r.left_top())] }
                Tool::Fib => FIB.iter().map(|f| { let yy = y(d.b.1 + (d.a.1 - d.b.1) * f); (pos2(a.x.min(b.x), yy), pos2(a.x.max(b.x), yy)) }).collect(),
                Tool::Channel => { let o = d.offset(); vec![(a, b), (to_screen((d.a.0, d.a.1 + o)), to_screen((d.b.0, d.b.1 + o)))] }
                Tool::Text => vec![],
                Tool::Position => { let (x0, x1) = pos_x(d); [d.a.1, d.b.1, d.c.1].iter().map(|p| (pos2(x0, y(*p)), pos2(x1, y(*p)))).collect() }
            }
        };
        let note = |d: &Drawing| painter.layout_no_wrap(if d.text.is_empty() { t("draw.text_hint").to_string() } else { d.text.clone() }, prop(13.0), d.color());
        let note_rect = |d: &Drawing, g: &egui::Galley| { let a = to_screen(d.a); Rect::from_min_size(pos2(a.x, a.y - g.size().y), g.size()).expand(4.0) };
        let handles = |d: &Drawing| -> Vec<(u8, Pos2)> {
            match d.tool {
                Tool::HLine | Tool::Text => vec![],
                Tool::Channel => vec![(0, to_screen(d.a)), (1, to_screen(d.b)), (3, to_screen(mid_c(d)))],
                Tool::Position => { let x1 = pos_x(d).1; vec![(0, to_screen(d.a)), (1, pos2(x1, y(d.b.1))), (3, pos2(to_screen(d.a).x, y(d.c.1)))] }
                _ => vec![(0, to_screen(d.a)), (1, to_screen(d.b))],
            }
        };
        // part under the pointer: a handle first, then the nearest line (or a box's inside); locked
        // drawings only answer right-clicks
        let hot = ptr.filter(|_| self.tool.is_none() && !hidden).and_then(|p| {
            for (i, d) in list.iter().enumerate().rev().filter(|(_, d)| !d.locked) {
                for (k, q) in handles(d) { if (q - p).length() < 7.0 { return Some((i, k)); } }
            }
            list.iter().enumerate().filter_map(|(i, d)| {
                let dist = segs(d).iter().map(|(a, b)| dist_to_segment(p, *a, *b)).fold(f32::MAX, f32::min);
                let inside = match d.tool {
                    Tool::Rect | Tool::Measure => Rect::from_two_pos(to_screen(d.a), to_screen(d.b)).contains(p),
                    Tool::Text => note_rect(d, &note(d)).contains(p),
                    Tool::Position => { let (x0, x1) = pos_x(d); Rect::from_x_y_ranges(x0..=x1, y(d.b.1).min(y(d.c.1))..=y(d.b.1).max(y(d.c.1))).contains(p) }
                    _ => false,
                };
                (dist < 6.0 || inside).then_some((i, if inside { 5.9 } else { dist }))
            }).min_by(|x, z| x.1.total_cmp(&z.1)).map(|(i, _)| (i, if list[i].locked { LOCKED } else { 2 }))
        });
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
        let draw = |d: &Drawing, emph: bool, alpha: f32| {
            let col = d.color().linear_multiply(alpha);
            let s = Stroke::new(if emph { 2.0 } else { 1.2 }, col);
            let seg = |p0: Pos2, p1: Pos2, s: Stroke| match d.dash {
                0 => { clip.line_segment([p0, p1], s); }
                k => { let (on, off) = if k == 1 { (6.0, 4.0) } else { (1.5, 3.0) }; clip.extend(egui::Shape::dashed_line(&[p0, p1], s, on, off)); }
            };
            let (a, b) = (to_screen(d.a), to_screen(d.b));
            match d.tool {
                Tool::Rect => {
                    let r = Rect::from_two_pos(a, b);
                    clip.rect_filled(r, 0, col.linear_multiply(0.12));
                    for (p0, p1) in segs(d) { seg(p0, p1, s); }
                }
                Tool::Fib => {
                    let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
                    let px = |f: f64| d.b.1 + (d.a.1 - d.b.1) * f;
                    let fib_col = |f: f64| match f { f if f == 0.5 => fg().linear_multiply(0.7), f if f == 0.618 => WARN, f if f == 0.0 || f == 1.0 => mu(), _ => accent() };
                    for w in FIB.windows(2) {
                        clip.rect_filled(Rect::from_two_pos(pos2(x0, y(px(w[0]))), pos2(x1, y(px(w[1])))), 0, fib_col(w[1]).linear_multiply(0.06 * alpha));
                    }
                    for f in FIB {
                        let yy = y(px(f));
                        seg(pos2(x0, yy), pos2(x1, yy), Stroke::new(s.width, fib_col(f).linear_multiply(alpha)));
                        clip.text(pos2(x0 + 3.0, yy - 1.0), Align2::LEFT_BOTTOM, format!("{f} ({})", fmt_px(px(f))), mono(10.0), fib_col(f).linear_multiply(alpha));
                    }
                    clip.extend(egui::Shape::dashed_line(&[a, b], Stroke::new(1.0, mu().linear_multiply(0.6 * alpha)), 4.0, 4.0));
                }
                Tool::Channel => {
                    let l = segs(d);
                    let (p, q) = (l[0], l[1]);
                    clip.add(egui::Shape::convex_polygon(vec![p.0, p.1, q.1, q.0], col.linear_multiply(0.08), Stroke::NONE));
                    seg(p.0, p.1, s);
                    seg(q.0, q.1, s);
                    clip.extend(egui::Shape::dashed_line(&[p.0 + (q.0 - p.0) / 2.0, p.1 + (q.1 - p.1) / 2.0], Stroke::new(1.0, col.linear_multiply(0.6)), 4.0, 4.0));
                }
                Tool::Measure => {
                    // TradingView's measure: change, percent, bars and time between the two points
                    let r = Rect::from_two_pos(a, b);
                    let dp = d.b.1 - d.a.1;
                    let mc = if dp >= 0.0 { up() } else { dn() }.linear_multiply(alpha);
                    clip.rect_filled(r, 0, mc.linear_multiply(0.15));
                    clip.extend([egui::Shape::line_segment([pos2(r.center().x, a.y), pos2(r.center().x, b.y)], Stroke::new(1.2, mc)),
                                 egui::Shape::line_segment([pos2(a.x, r.center().y), pos2(b.x, r.center().y)], Stroke::new(1.2, mc))]);
                    let n = ((d.b.0 - d.a.0) as f64 / ms as f64).round() as i64;
                    let pct = if d.a.1 != 0.0 { dp / d.a.1 * 100.0 } else { 0.0 };
                    let txt = format!("{}{} ({pct:+.2}%)\n{n} {} · {}", if dp >= 0.0 { "+" } else { "" }, fmt_px(dp), t("draw.bars"), span(n.abs() * ms));
                    let g = painter.layout(txt, mono(11.0), Color32::WHITE, 260.0);
                    let below = b.y >= a.y;
                    let lr = Rect::from_min_size(pos2(r.center().x - g.size().x / 2.0 - 6.0, if below { r.bottom() + 6.0 } else { r.top() - g.size().y - 14.0 }), g.size() + vec2(12.0, 8.0));
                    // kept inside the plot so neither line is clipped
                    let lr = lr.translate(vec2(0.0, (plot.top() + 2.0 - lr.top()).max(0.0) + (plot.bottom() - 2.0 - lr.bottom()).min(0.0)));
                    clip.rect_filled(lr, 4, mc.linear_multiply(0.9));
                    clip.galley(lr.min + vec2(6.0, 4.0), g, Color32::WHITE);
                }
                Tool::Position => {
                    // TradingView's long / short position: reward box to the target, risk box to the
                    // stop, and the size that risks `risk_pct` of equity between entry and stop
                    let (x0, x1) = pos_x(d);
                    let (ye, ys, yt) = (y(d.a.1), y(d.b.1), y(d.c.1));
                    clip.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(yt)..=ye.max(yt)), 0, up().linear_multiply(0.16 * alpha));
                    clip.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(ys)..=ye.max(ys)), 0, dn().linear_multiply(0.16 * alpha));
                    for (yy, c) in [(ye, fg()), (ys, dn()), (yt, up())] { clip.hline(x0..=x1, yy, Stroke::new(1.2, c.linear_multiply(alpha))); }
                    let pct = |p: f64| if d.a.1 != 0.0 { (p / d.a.1 - 1.0) * 100.0 } else { 0.0 };
                    let risk = (d.a.1 - d.b.1).abs();
                    let rr = if risk > 0.0 { (d.c.1 - d.a.1).abs() / risk } else { 0.0 };
                    let long = d.c.1 > d.a.1;
                    let size = match position_qty(equity, risk_pct, d.a.1, d.b.1) {
                        Some(q) => format!("{} {base} · {risk_pct}% = ${}", fmt_qty_chart(q), fmt_usd_short(q * risk)),
                        None => format!("{risk_pct}%"),
                    };
                    let tag = |yy: f64, txt: String, c: Color32, below: bool| {
                        let g = painter.layout_no_wrap(txt, mono(10.5), Color32::WHITE);
                        let r = Rect::from_min_size(pos2(x0 + 4.0, if below { yy as f32 + 3.0 } else { yy as f32 - g.size().y - 7.0 }), g.size() + vec2(8.0, 4.0));
                        clip.rect_filled(r, 3, c.linear_multiply(0.9 * alpha));
                        clip.galley(r.min + vec2(4.0, 2.0), g, Color32::WHITE);
                    };
                    tag(yt as f64, format!("{} {} ({:+.2}%)  R:R {rr:.2}", t("pos.target"), fmt_px(d.c.1), pct(d.c.1)), up(), !long);
                    tag(ys as f64, format!("{} {} ({:+.2}%)", t("pos.stop"), fmt_px(d.b.1), pct(d.b.1)), dn(), long);
                    tag(ye as f64, format!("{} {} {} · {size}", if long { t("pos.long") } else { t("pos.short") }, fmt_px(d.a.1), t("pos.qty")), label_bg(), !long);
                }
                Tool::Text => {
                    let g = note(d);
                    let r = note_rect(d, &g);
                    if emph { clip.rect_stroke(r, 3, Stroke::new(1.0, col.linear_multiply(0.6)), egui::StrokeKind::Inside); }
                    clip.galley(r.min + vec2(4.0, 4.0), g, col);
                }
                _ => for (p0, p1) in segs(d) { seg(p0, p1, s); },
            }
            // an armed alert: bell tag where the line meets the axis side of the plot
            if let (true, Some(lv)) = (d.alert.is_some(), d.level_at(bars.last().map_or(0, |b| b.t))) {
                let ay = y(lv);
                let g = painter.layout_no_wrap(format!("⏰ {}", fmt_px(lv)), mono(10.5), Color32::WHITE);
                let r = Rect::from_min_size(pos2(main.right() - g.size().x - 14.0, ay - 9.0), vec2(g.size().x + 10.0, 18.0));
                clip.rect_filled(r, 3, col);
                clip.galley(r.min + vec2(5.0, (18.0 - g.size().y) / 2.0), g, Color32::WHITE);
            }
            if d.tool == Tool::HLine {
                let lab = Rect::from_min_size(pos2(main.right() + 1.0, a.y - 9.0), vec2(AXIS_W - 2.0, 18.0));
                painter.rect_filled(lab, 2, d.color());
                painter.text(lab.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, fmt_px(d.a.1), mono(11.0), Color32::BLACK);
            } else if emph && !d.locked {
                // handles show on hover: hollow, as in TradingView
                for (_, p) in handles(d) { clip.circle(p, 4.0, panel2(), Stroke::new(1.5, d.color())); }
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
                            let mut on = d.alert.is_some();
                            if ui.checkbox(&mut on, t("alert.on")).on_hover_text(t("alert.tip")).changed() { d.alert = on.then(|| sound.clone()); }
                            if let Some(s) = &mut d.alert { sound_menu(ui, s); }
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
                            d.alert = Some(sound.clone());
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
        self.drawings.iter().filter(|(_, v)| v.iter().any(|d| d.alert.is_some())).map(|(b, _)| b.clone()).collect()
    }

    /// Fires every armed alert whose line the price crossed since the last check (once: the alert
    /// disarms, the line stays). `px` holds the latest price per base.
    pub fn check_alerts(&mut self, px: &std::collections::HashMap<String, f64>) {
        let ts = depth::now_ms();
        for (base, list) in self.drawings.iter_mut() {
            let Some(&p) = px.get(base) else { continue };
            let Some(prev) = self.alert_px.insert(base.clone(), p) else { continue };
            for d in list.iter_mut() {
                let (Some(sound), Some(lv)) = (d.alert.clone(), d.level_at(ts)) else { continue };
                if (prev < lv) == (p < lv) { continue; }
                d.alert = None;
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
pub(super) fn position_qty(equity: Option<f64>, risk_pct: f64, entry: f64, stop: f64) -> Option<f64> {
    let risk = (entry - stop).abs();
    equity.filter(|_| risk > 0.0).map(|eq| eq * risk_pct / 100.0 / risk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_qty_matches_label() {
        // 1 % of 10,000 is 100 USDT risked over a 5 USDT stop distance: 20 coins, either side
        assert_eq!(position_qty(Some(10_000.0), 1.0, 100.0, 95.0), Some(20.0));
        assert_eq!(position_qty(Some(10_000.0), 0.5, 100.0, 104.0), Some(12.5));
        assert_eq!(position_qty(None, 1.0, 100.0, 95.0), None);
        assert_eq!(position_qty(Some(10_000.0), 1.0, 100.0, 100.0), None);
    }

    #[test]
    fn alerts_fire_once_on_crossing() {
        let mut c = Chart::default();
        let mut d = Drawing::new(Tool::HLine, (0, 100.0), (0, 100.0));
        d.alert = Some("none".into());
        c.drawings.insert("BTC".into(), vec![d]);
        let px = |p: f64| std::collections::HashMap::from([("BTC".to_string(), p)]);
        c.check_alerts(&px(99.0));
        c.check_alerts(&px(99.5));
        assert!(c.fired.is_empty());
        c.check_alerts(&px(100.0));
        assert_eq!(c.fired.len(), 1);
        assert!(c.drawings["BTC"][0].alert.is_none());
        c.check_alerts(&px(98.0));
        assert_eq!(c.fired.len(), 1);
        assert_eq!(c.alert_bases(), Vec::<String>::new());
        // a ray alerts from its start on, a trend line only between its points
        let r = Drawing::new(Tool::Ray, (1000, 10.0), (2000, 20.0));
        assert_eq!((r.level_at(500), r.level_at(3000)), (None, Some(30.0)));
        let t = Drawing { tool: Tool::Trend, ..r.clone() };
        assert_eq!((t.level_at(1500), t.level_at(3000)), (Some(15.0), None));
        // a channel keeps its third point halfway along the parallel line
        let ch = Drawing::new(Tool::Channel, (0, 10.0), (100, 20.0)).with_c((100, 25.0));
        assert_eq!((ch.c, ch.offset()), ((50, 20.0), 5.0));
    }
}
