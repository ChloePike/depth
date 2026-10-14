//! Pair picker: a port of the SwiftUI host's SymbolPicker. USDT perpetuals (engine tickers),
//! search ranked exact > prefix > contains, sortable columns, favorites and recently opened pairs
//! (favorites.txt / recent.txt under ~/Library/Application Support/Depth). Click picks; arrows move
//! the selection, Enter picks it (or the first match), Escape closes. ⌘K opens, ⌘[ ⌘] step through
//! favorites, ⌘D stars the current pair.
use super::{fmt_px, l, theme::*, top::big};
use crate::engine::Ticker;
use eframe::egui::{self, pos2, vec2, Align2, Key, Modifiers, Rect, RichText, Sense, Ui};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
enum Col { Pair, Last, Chg, Vol }

pub struct Picker {
    pub open: bool,
    query: String,
    favs_only: bool,
    /// in the order they were starred (⌘[ ⌘] step through them in that order)
    favs: Vec<String>,
    /// most recent first, at most 10
    recent: Vec<String>,
    sort: (Col, bool),
    /// keyboard selection, and the query it was made for
    sel: Option<String>,
    seen_query: String,
    /// rows view: scroll offset and height last frame; a keyboard move scrolls the selection into view
    view: (f32, f32),
    scroll_to: Option<usize>,
    was_open: bool,
}

const ROW_H: f32 = 26.0;

fn path(name: &str) -> Option<PathBuf> { Some(depth::sys::data_dir()?.join(name)) }
fn load(name: &str) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(path(name)?).ok()?;
    Some(s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}
fn store(name: &str, v: &[String]) {
    let Some(p) = path(name) else { return };
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(p, v.join("\n"));
}

impl Default for Picker {
    fn default() -> Self {
        let favs = load("favorites.txt").unwrap_or_else(|| ["BTC", "ETH", "SOL"].into_iter().map(String::from).collect());
        Picker { open: false, query: String::new(), favs_only: false, favs, recent: load("recent.txt").unwrap_or_default(),
            sort: (Col::Vol, false), sel: None, seen_query: String::new(), view: (0.0, 0.0), scroll_to: None, was_open: false }
    }
}

/// Search key: upper case, without "USDT" and "/".
fn norm(q: &str) -> String { q.trim().to_uppercase().replace("USDT", "").replace('/', "") }

/// Rows for a query: matches only (favorites only when asked and not searching), column sort,
/// then ranked exact > prefix > contains (a stable sort keeps the column order within each rank).
fn rank<'a>(tickers: &'a [Ticker], query: &str, favs: Option<&[String]>, sort: (Col, bool)) -> Vec<&'a Ticker> {
    let q = norm(query);
    let mut rows: Vec<&Ticker> = tickers.iter()
        .filter(|t| (q.is_empty() || t.base.contains(&q)) && (!q.is_empty() || favs.is_none_or(|f| f.contains(&t.base)))).collect();
    rows.sort_by(|a, b| {
        let o = match sort.0 { Col::Pair => a.base.cmp(&b.base), Col::Last => a.last.total_cmp(&b.last), Col::Chg => a.chg_pct.total_cmp(&b.chg_pct), Col::Vol => a.quote_vol.total_cmp(&b.quote_vol) };
        if sort.1 { o } else { o.reverse() }
    });
    let r = |b: &str| if q.is_empty() || b == q { 0 } else if b.starts_with(&q) { 1 } else { 2 };
    rows.sort_by_key(|t| r(&t.base));
    rows
}

impl Picker {
    fn toggle_fav(&mut self, b: &str) {
        if let Some(i) = self.favs.iter().position(|f| f == b) { self.favs.remove(i); } else { self.favs.push(b.to_string()); }
        store("favorites.txt", &self.favs);
    }

    /// Remember a pair switch (current pair kept second).
    fn opened(&mut self, b: &str, cur: &str) {
        let mut r = vec![b.to_string()];
        for x in std::iter::once(cur.to_string()).chain(self.recent.iter().cloned()) { if !r.contains(&x) { r.push(x); } }
        r.truncate(10);
        self.recent = r;
        store("recent.txt", &self.recent);
    }

    /// Button showing the current pair (and the global shortcuts); returns a newly chosen base asset.
    pub fn show(&mut self, ui: &mut Ui, base: &str, tickers: &[Ticker]) -> Option<String> {
        let mut chosen: Option<String> = None;
        // shortcuts: ⌘K picker, ⌘[ / ⌘] previous / next favorite, ⌘D star the current pair
        let (k, prev, next, star) = ui.input_mut(|i| (i.consume_key(Modifiers::COMMAND, Key::K), i.consume_key(Modifiers::COMMAND, Key::OpenBracket),
            i.consume_key(Modifiers::COMMAND, Key::CloseBracket), i.consume_key(Modifiers::COMMAND, Key::D)));
        if star { self.toggle_fav(base); }
        if (prev || next) && !self.favs.is_empty() {
            let n = self.favs.len();
            let i = self.favs.iter().position(|f| f == base).map_or(0, |i| (i + if next { 1 } else { n - 1 }) % n);
            chosen = Some(self.favs[i].clone());
        }

        let btn = ui.add(egui::Button::new(RichText::new(format!("       {base}/USDT  ▾")).font(prop(15.0)).strong()).min_size(vec2(120.0, 28.0)))
            .on_hover_text(format!("{} (⌘K)", l("Choose pair")));
        coin_icon(ui, base, Rect::from_center_size(pos2(btn.rect.left() + 18.0, btn.rect.center().y), vec2(18.0, 18.0)));
        if btn.clicked() || k { self.open = !self.open; }
        // opened by the button, ⌘K or T1_PICK
        let just_opened = self.open && !self.was_open;
        self.was_open = self.open;
        if just_opened {
            self.query.clear();
            self.seen_query.clear();
            self.sel = Some(base.to_string());
            self.scroll_to = None;
        }
        if !self.open {
            if let Some(b) = &chosen { let b = b.clone(); self.opened(&b, base); }
            return chosen;
        }

        // keys for the list, taken before the search field sees them
        let (down, up_k, enter) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::Enter)));
        let area = egui::Area::new(egui::Id::new("picker")).order(egui::Order::Foreground).fixed_pos(btn.rect.left_bottom() + vec2(0.0, 6.0));
        let inner = area.show(ui.ctx(), |ui| {
            egui::Frame::new().fill(popup_bg()).stroke(egui::Stroke::new(1.0, line())).corner_radius(10).inner_margin(12).shadow(ui.visuals().popup_shadow).show(ui, |ui| {
                ui.set_width(480.0);
                ui.spacing_mut().item_spacing.y = 8.0;
                // search field
                let (fr, _) = ui.allocate_exact_size(vec2(480.0, 30.0), Sense::hover());
                let fid = ui.id().with("pick_q");
                let focused = ui.memory(|m| m.has_focus(fid));
                ui.painter().rect(fr, 6, panel2(), egui::Stroke::new(1.0, if focused { accent() } else { line() }), egui::StrokeKind::Inside);
                search_glyph(ui.painter(), pos2(fr.left() + 15.0, fr.center().y), dim());
                let q = ui.put(Rect::from_min_max(pos2(fr.left() + 28.0, fr.top() + 4.0), pos2(fr.right() - 8.0, fr.bottom() - 4.0)),
                    egui::TextEdit::singleline(&mut self.query).id(fid).frame(egui::Frame::NONE).hint_text(l("Search pairs")).font(prop(13.0)).vertical_align(egui::Align::Center));
                // focus once when opened; re-requesting every frame fights row clicks and forces a repaint every frame
                if just_opened { q.request_focus(); }
                let searching = !norm(&self.query).is_empty();

                // Favorites / All (ignored while searching) and the shortcut hint
                ui.horizontal(|ui| {
                    let mut i = if self.favs_only { 0 } else { 1 };
                    ui.add_enabled_ui(!searching, |ui| { if segment(ui, &[l("Favorites"), l("All")], &mut i) { self.favs_only = i == 0; } });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("⌘[ ⌘]  {}  ·  ⌘D  {}", l("favorites"), l("star"))).font(prop(10.5)).color(dim()));
                    });
                });

                // recently opened pairs
                let recent: Vec<String> = self.recent.iter().filter(|b| b.as_str() != base).take(6).cloned().collect();
                if !recent.is_empty() && !searching {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(l("Recent")).font(prop(11.5)).color(mu()));
                        for b in &recent {
                            let g = ui.painter().layout_no_wrap(b.clone(), prop(11.5), fg());
                            let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 32.0, 22.0), Sense::click());
                            ui.painter().rect(r, 5, if resp.hovered() { hl() } else { panel2() }, egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside);
                            coin_icon(ui, b, Rect::from_center_size(pos2(r.left() + 13.0, r.center().y), vec2(13.0, 13.0)));
                            ui.painter().galley(pos2(r.left() + 23.0, r.center().y - g.size().y / 2.0), g, fg());
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { chosen = Some(b.clone()); }
                        }
                    });
                }

                let rows = rank(tickers, &self.query, self.favs_only.then_some(self.favs.as_slice()), self.sort);
                if self.query != self.seen_query { self.seen_query = self.query.clone(); self.sel = rows.first().map(|t| t.base.clone()); self.scroll_to = Some(0); }
                if (down || up_k) && !rows.is_empty() {
                    let i = self.sel.as_ref().and_then(|s| rows.iter().position(|t| &t.base == s));
                    let j = match (i, down) { (Some(i), true) => (i + 1).min(rows.len() - 1), (Some(i), false) => i.saturating_sub(1), (None, true) => 0, (None, false) => rows.len() - 1 };
                    self.sel = Some(rows[j].base.clone());
                    self.scroll_to = Some(j);
                }
                if enter {
                    chosen = self.sel.as_ref().and_then(|s| rows.iter().find(|t| &t.base == s)).or(rows.first()).map(|t| t.base.clone());
                }

                // column headers (click sorts; again reverses)
                let (hr, _) = ui.allocate_exact_size(vec2(480.0, 18.0), Sense::hover());
                let cols = [(Col::Pair, l("Pair"), hr.left() + 46.0, false), (Col::Last, l("Last"), hr.left() + 300.0, true), (Col::Chg, l("24h Chg"), hr.left() + 390.0, true), (Col::Vol, l("Volume"), hr.right() - 8.0, true)];
                for (c, name, x, right) in cols {
                    let on = self.sort.0 == c;
                    let g = ui.painter().layout_no_wrap(name.to_string(), prop(11.0), if on { fg() } else { dim() });
                    let w = g.size().x + 12.0;
                    let r = if right { Rect::from_min_max(pos2(x - w, hr.top()), pos2(x, hr.bottom())) } else { Rect::from_min_size(pos2(x, hr.top()), vec2(w, hr.height())) };
                    let resp = ui.interact(r, ui.id().with(("pick_sort", name)), Sense::click());
                    let col = if on || resp.hovered() { fg() } else { dim() };
                    let tx = if right { r.right() - 12.0 - g.size().x } else { r.left() };
                    ui.painter().galley(pos2(tx, r.center().y - g.size().y / 2.0), g, col);
                    if on {
                        // sort direction caret
                        let cx = tx + r.width() - 6.0;
                        let (y0, y1) = if self.sort.1 { (r.center().y + 2.0, r.center().y - 2.0) } else { (r.center().y - 2.0, r.center().y + 2.0) };
                        ui.painter().add(egui::Shape::line(vec![pos2(cx - 3.0, y0), pos2(cx, y1), pos2(cx + 3.0, y0)], egui::Stroke::new(1.3, fg())));
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        // a new column starts A-Z for names, largest first for numbers
                        self.sort = if on { (c, !self.sort.1) } else { (c, c == Col::Pair) };
                    }
                }
                ui.painter().hline(hr.x_range(), hr.bottom() + 3.0, egui::Stroke::new(1.0, line()));

                if rows.is_empty() {
                    let msg = if tickers.is_empty() { l("Loading…") } else if self.favs_only && !searching { l("No favorites yet: star a pair in All") } else { l("No matching pairs") };
                    let (r, _) = ui.allocate_exact_size(vec2(480.0, 120.0), Sense::hover());
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, msg, prop(12.5), mu());
                    return;
                }
                let mut toggle = None;
                // keep the keyboard selection in view
                let mut sa = egui::ScrollArea::vertical().id_salt("pick_rows").max_height(400.0).min_scrolled_height(400.0).auto_shrink([false, true]);
                if let Some(j) = self.scroll_to.take() {
                    let (top, bot) = (j as f32 * ROW_H, (j + 1) as f32 * ROW_H);
                    let (off, h) = self.view;
                    if top < off { sa = sa.vertical_scroll_offset(top); } else if h > 0.0 && bot > off + h { sa = sa.vertical_scroll_offset(bot - h); }
                }
                // virtualized: only the visible rows are laid out and painted (and fetch logos);
                // show_rows counts item spacing into the row pitch, so it must be zero here
                ui.spacing_mut().item_spacing.y = 0.0;
                let out = sa.show_rows(ui, ROW_H, rows.len(), |ui, range| {
                    for t in &rows[range] {
                        let (rect, resp) = ui.allocate_exact_size(vec2(480.0, ROW_H), Sense::click());
                        let sel = self.sel.as_deref() == Some(t.base.as_str());
                        if sel { ui.painter().rect_filled(rect, 5, accent().gamma_multiply(0.22)); } else if resp.hovered() { ui.painter().rect_filled(rect, 5, hl()); }
                        let y = rect.center().y;
                        let fav = self.favs.contains(&t.base);
                        let star = Rect::from_center_size(pos2(rect.left() + 11.0, y), vec2(18.0, 18.0));
                        let star_hover = resp.hovered() && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| star.contains(p));
                        ui.painter().text(star.center(), Align2::CENTER_CENTER, if fav { "★" } else { "☆" }, prop(13.0), if fav { WARN } else if star_hover { mu() } else { dim() });
                        coin_icon(ui, &t.base, Rect::from_center_size(pos2(rect.left() + 34.0, y), vec2(16.0, 16.0)));
                        let p = ui.painter();
                        let bx = p.text(pos2(rect.left() + 48.0, y), Align2::LEFT_CENTER, &t.base, prop(12.5), fg()).right();
                        let ux = p.text(pos2(bx, y), Align2::LEFT_CENTER, "USDT", prop(12.5), dim()).right();
                        if t.base == base { p.text(pos2(ux + 6.0, y), Align2::LEFT_CENTER, "✓", prop(11.0), mu()); }
                        p.text(pos2(rect.left() + 300.0, y), Align2::RIGHT_CENTER, fmt_px(t.last), mono(12.0), fg());
                        p.text(pos2(rect.left() + 390.0, y), Align2::RIGHT_CENTER, format!("{:+.2}%", t.chg_pct), mono(12.0), if t.chg_pct >= 0.0 { up() } else { dn() });
                        p.text(pos2(rect.right() - 8.0, y), Align2::RIGHT_CENTER, big(t.quote_vol), mono(11.5), mu());
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            if star_hover { toggle = Some(t.base.clone()); } else { chosen = Some(t.base.clone()); }
                        }
                    }
                });
                self.view = (out.state.offset.y, out.inner_rect.height());
                if let Some(b) = toggle { self.toggle_fav(&b); }
            });
        });
        // close on choice, Escape or a click outside the button and the popup
        let clicked_outside = ui.input(|i| i.pointer.any_click())
            && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| !inner.response.rect.contains(p) && !btn.rect.contains(p));
        if chosen.is_some() || clicked_outside || ui.input(|i| i.key_pressed(Key::Escape)) { self.open = false; self.was_open = false; }
        if let Some(b) = &chosen { let b = b.clone(); self.opened(&b, base); }
        chosen
    }
}

/// Magnifier glyph in the search field.
fn search_glyph(p: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    p.circle_stroke(c + vec2(-1.0, -1.0), 4.5, s);
    p.line_segment([c + vec2(2.3, 2.3), c + vec2(5.5, 5.5)], s);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tk(b: &str, v: f64) -> Ticker { Ticker { base: b.into(), last: 1.0, chg_pct: 0.0, quote_vol: v, high: 0.0, low: 0.0, base_vol: 0.0 } }

    #[test]
    fn ranked_search() {
        let t = vec![tk("ETHFI", 9.0), tk("ETH", 5.0), tk("WETH", 7.0), tk("BTC", 10.0)];
        let names = |r: Vec<&Ticker>| r.iter().map(|t| t.base.as_str()).collect::<Vec<_>>().join(",");
        // exact, then prefix, then contains; volume order inside each rank
        assert_eq!(names(rank(&t, "eth/usdt", None, (Col::Vol, false))), "ETH,ETHFI,WETH");
        // a search ignores the favorites filter; no search shows favorites only
        let favs = vec!["BTC".to_string()];
        assert_eq!(names(rank(&t, "eth", Some(&favs), (Col::Vol, false))), "ETH,ETHFI,WETH");
        assert_eq!(names(rank(&t, "", Some(&favs), (Col::Vol, false))), "BTC");
        assert_eq!(names(rank(&t, "", None, (Col::Pair, true))), "BTC,ETH,ETHFI,WETH");
    }
}
