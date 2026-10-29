//! Pair picker: a centred sheet of the USDT perpetuals (engine tickers) of every venue switched on,
//! categories (crypto, stocks, ETFs, commodities, FX, pre-IPO from `depth::alias`), tickers or company names,
//! search (base, real ticker, company name, other venues' names) ranked exact > prefix > contains,
//! sortable columns, favorites and recently opened pairs
//! (favorites.txt / recent.txt under ~/Library/Application Support/Depth). Click picks; arrows move
//! the selection, Enter picks it (or the first match), Escape closes. Hotkeys (default ⌘K, ⌘[ ⌘], ⌘D)
//! open it, step through favorites and star the current pair.
use super::{fmt_px, kit, l, theme::*, top, top::big};
use crate::engine::Ticker;
use crate::hotkeys::Act;
use depth::alias::{self, Cat};
use eframe::egui::{self, pos2, vec2, Align2, Key, Modifiers, Rect, RichText, Sense, Ui};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
enum Col { Pair, Last, Chg, Vol, Oi, Fund }

pub struct Picker {
    pub open: bool,
    query: String,
    favs_only: bool,
    /// in the order they were starred (the favorite hotkeys step through them in that order)
    favs: Vec<String>,
    /// most recent first, at most 10
    recent: Vec<String>,
    sort: (Col, bool),
    /// category filter (None = all) and company names instead of tickers
    cat: Option<Cat>,
    names: bool,
    /// keyboard selection, and the query it was made for
    sel: Option<String>,
    seen_query: String,
    /// rows view: scroll offset and height last frame; a keyboard move scrolls the selection into view
    view: (f32, f32),
    scroll_to: Option<usize>,
    was_open: bool,
    /// picker hotkeys pressed this frame (App::shortcuts)
    pub hot: Vec<Act>,
    /// current key of each picker action, for the hints ("" = unbound)
    pub keys: [String; 4],
}

const ROW_H: f32 = 42.0;
/// sheet content width and the column anchors (market left edge, Last / 24h / Volume right edges)
const W: f32 = 880.0;
const X_PAIR: f32 = 64.0;
const X_LAST: f32 = 440.0;
const X_CHG: f32 = 540.0;
const X_VOL: f32 = 640.0;
const X_OI: f32 = 730.0;
const X_FUND: f32 = 830.0;
const CATS: [(Option<Cat>, &str); 7] = [(None, "All"), (Some(Cat::Crypto), "Crypto"), (Some(Cat::Stock), "Stocks"), (Some(Cat::Etf), "ETFs & Indices"),
    (Some(Cat::Commodity), "Commodities"), (Some(Cat::Fx), "FX"), (Some(Cat::PreIpo), "Pre-IPO")];

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
        Picker { hot: vec![], keys: Default::default(), open: false, query: String::new(), favs_only: false, favs, recent: load("recent.txt").unwrap_or_default(),
            sort: (Col::Vol, false), cat: None, names: false, sel: None, seen_query: String::new(), view: (0.0, 0.0), scroll_to: None, was_open: false }
    }
}

/// Search key: upper case, without "USDT" and "/".
fn norm(q: &str) -> String { q.trim().to_uppercase().replace("USDT", "").replace('/', "") }

/// Rows for a query: matches only (favorites only when asked and not searching), in the category,
/// column sort, then ranked exact > prefix > contains (a stable sort keeps the column order within each rank).
fn rank<'a>(tickers: &'a [Ticker], query: &str, favs: Option<&[String]>, cat: Option<Cat>, sort: (Col, bool)) -> Vec<&'a Ticker> {
    let q = norm(query);
    // best of the base, its real ticker and company name and its other-venue names (BB finds BBX,
    // GOLD finds XAU, APPLE finds AAPL); 3 = no match
    let m = |b: &str| if q.is_empty() || b == q { 0 } else if b.starts_with(&q) { 1 } else if b.contains(&q) { 2 } else { 3 };
    let r = |t: &Ticker| {
        let (tick, name, _) = alias::display(&t.base);
        alias::synonyms(&t.base).map(m).fold(m(&t.base).min(m(tick)).min(name.map_or(3, |n| m(&n.to_uppercase()))), u8::min)
    };
    let mut rows: Vec<&Ticker> = tickers.iter()
        .filter(|t| cat.is_none_or(|c| alias::display(&t.base).2 == c))
        .filter(|t| r(t) < 3 && (!q.is_empty() || favs.is_none_or(|f| f.contains(&t.base)))).collect();
    rows.sort_by(|a, b| {
        let o = match sort.0 { Col::Pair => a.base.cmp(&b.base), Col::Last => a.last.total_cmp(&b.last), Col::Chg => a.chg_pct.total_cmp(&b.chg_pct), Col::Vol => a.quote_vol.total_cmp(&b.quote_vol),
            Col::Oi => a.oi_usd.total_cmp(&b.oi_usd), Col::Fund => a.funding_h.unwrap_or(f64::MIN).total_cmp(&b.funding_h.unwrap_or(f64::MIN)) };
        if sort.1 { o } else { o.reverse() }
    });
    rows.sort_by_key(|t| r(t));
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
        // hotkeys (App::shortcuts): picker, previous / next favorite, star the current pair
        let hot = std::mem::take(&mut self.hot);
        let has = |a: Act| hot.contains(&a);
        let (k, prev, next, star) = (has(Act::Picker), has(Act::PrevFav), has(Act::NextFav), has(Act::Star));
        if star { self.toggle_fav(base); }
        if (prev || next) && !self.favs.is_empty() {
            let n = self.favs.len();
            let i = self.favs.iter().position(|f| f == base).map_or(0, |i| (i + if next { 1 } else { n - 1 }) % n);
            chosen = Some(self.favs[i].clone());
        }

        let btn = pair_button(ui, base, self.open).on_hover_text(if self.keys[0].is_empty() { l("Choose pair").to_string() } else { format!("{} ({})", l("Choose pair"), self.keys[0]) });
        if btn.clicked() || k { self.open = !self.open; }
        // opened by the button, the hotkey or T1_PICK
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
        // pinned near the top: the sheet keeps its place as the rows change
        let id = egui::Id::new("picker");
        let top_y = ui.ctx().content_rect().height() * 0.1;
        let m = egui::Modal::new(id).area(egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, top_y))).show(ui.ctx(), |ui| {
            ui.set_width(W);
            ui.spacing_mut().item_spacing.y = 10.0;
            // search field (kit::field look: well, hairline, accent border and soft ring when focused)
            let (fr, _) = ui.allocate_exact_size(vec2(W, 36.0), Sense::hover());
            let fid = ui.id().with("pick_q");
            let f = ui.ctx().animate_bool_with_time(fid.with("f"), ui.memory(|m| m.has_focus(fid)), 0.12);
            if f > 0.0 { ui.painter().rect_filled(fr.expand(2.5 * f), kit::rad() + 2, accent().gamma_multiply(0.14 * f)); }
            ui.painter().rect(fr, kit::rad(), well(), egui::Stroke::new(1.0, lerp(line(), accent(), f)), egui::StrokeKind::Inside);
            search_glyph(ui.painter(), pos2(fr.left() + 15.0, fr.center().y), mu());
            let hint = format!("{} ({})", l("Search pairs"), tickers.len());
            let q = ui.put(Rect::from_min_max(pos2(fr.left() + 30.0, fr.top() + 4.0), pos2(fr.right() - 10.0, fr.bottom() - 4.0)),
                egui::TextEdit::singleline(&mut self.query).id(fid).frame(egui::Frame::NONE).hint_text(RichText::new(hint).color(dim()))
                    .font(prop_m(14.0)).text_color(fg_strong()).vertical_align(egui::Align::Center));
            // focus once when opened; re-requesting every frame fights row clicks and forces a repaint every frame
            if just_opened { q.request_focus(); }
            let searching = !norm(&self.query).is_empty();

            // Favorites / All (ignored while searching), Tickers / Names
            ui.horizontal(|ui| {
                let i = if self.favs_only { 0 } else { 1 };
                ui.add_enabled_ui(!searching, |ui| {
                    if let Some(j) = top::pills(ui, "pick_favs", &[l("Favorites"), l("All")], i, 24.0, 12.5) { self.favs_only = j == 0; }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(j) = top::pills(ui, "pick_names", &[l("Tickers"), l("Names")], self.names as usize, 24.0, 12.5) { self.names = j == 1; }
                    let [_, p, n, d] = &self.keys;
                    ui.label(RichText::new(format!("{p} {n}  {}  ·  {d}  {}  ", l("favorites"), l("star"))).font(prop(11.0)).color(dim()));
                });
            });
            // categories
            let labels: Vec<&str> = CATS.iter().map(|(_, n)| l(n)).collect();
            let ci = CATS.iter().position(|(c, _)| *c == self.cat).unwrap_or(0);
            if let Some(j) = top::pills(ui, "pick_cat", &labels, ci, 24.0, 12.5) { self.cat = CATS[j].0; self.scroll_to = Some(0); }

            // recently opened pairs
            let recent: Vec<String> = self.recent.iter().filter(|b| b.as_str() != base).take(8).cloned().collect();
            if !recent.is_empty() && !searching {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(l("Recent").to_uppercase()).font(prop_s(10.5)).color(dim()).extra_letter_spacing(0.6));
                    for b in &recent {
                        let g = ui.painter().layout_no_wrap(alias::display(b).0.to_string(), prop_m(12.0), fg());
                        let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 32.0, kit::H_SM), Sense::click());
                        let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
                        ui.painter().rect(r, kit::rad(), lerp(panel2(), hl(), hv), egui::Stroke::new(1.0, lerp(line(), edge(), hv)), egui::StrokeKind::Inside);
                        coin_icon(ui, b, Rect::from_center_size(pos2(r.left() + 13.0, r.center().y), vec2(14.0, 14.0)));
                        ui.painter().galley(pos2(r.left() + 24.0, r.center().y - g.size().y / 2.0), g, fg());
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { chosen = Some(b.clone()); }
                    }
                });
            }

            let rows = rank(tickers, &self.query, self.favs_only.then_some(self.favs.as_slice()), self.cat, self.sort);
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

            // column headers, 11 medium grey (click sorts; again reverses)
            let (hr, _) = ui.allocate_exact_size(vec2(W, 22.0), Sense::hover());
            let cols = [(Col::Pair, l("Market"), hr.left() + X_PAIR, false), (Col::Last, l("Last"), hr.left() + X_LAST, true), (Col::Chg, l("24h Chg"), hr.left() + X_CHG, true), (Col::Vol, l("Volume"), hr.left() + X_VOL, true),
                (Col::Oi, l("Open interest"), hr.left() + X_OI, true), (Col::Fund, l("Funding /h"), hr.left() + X_FUND, true)];
            for (c, name, x, right) in cols {
                let on = self.sort.0 == c;
                let sorted = on.then_some(self.sort.1);
                let label = match sorted { Some(true) => format!("{name} ↑"), Some(false) => format!("{name} ↓"), None => name.to_string() };
                let w = ui.painter().layout_no_wrap(label, prop_m(11.0), fg()).size().x;
                let r = if right { Rect::from_min_max(pos2(x - w, hr.top()), pos2(x, hr.bottom())) } else { Rect::from_min_size(pos2(x, hr.top()), vec2(w, hr.height())) };
                let resp = ui.interact(r.expand2(vec2(4.0, 0.0)), ui.id().with(("pick_sort", name)), Sense::click());
                let (pos, al) = if right { (r.right_center(), Align2::RIGHT_CENTER) } else { (r.left_center(), Align2::LEFT_CENTER) };
                if on { kit::th(ui.painter(), pos, al, name, sorted); } else { ui.painter().text(pos, al, name, prop_m(11.0), if resp.hovered() { fg() } else { mu() }); }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    // a new column starts A-Z for names, largest first for numbers
                    self.sort = if on { (c, !self.sort.1) } else { (c, c == Col::Pair) };
                }
            }
            ui.painter().hline(egui::Rangef::new(hr.left() - 8.0, hr.right() + 8.0), hr.bottom() + 0.5, egui::Stroke::new(1.0, line()));

            if rows.is_empty() {
                let msg = if tickers.is_empty() { l("Loading…") } else if self.favs_only && !searching { l("No favorites yet: star a pair in All") } else { l("No matching pairs") };
                let (r, _) = ui.allocate_exact_size(vec2(W, 120.0), Sense::hover());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, msg, prop(12.5), mu());
                return;
            }
            let mut toggle = None;
            // keep the keyboard selection in view
            let mut sa = egui::ScrollArea::vertical().id_salt("pick_rows").max_height(480.0).min_scrolled_height(480.0).auto_shrink([false, true]);
            if let Some(j) = self.scroll_to.take() {
                let (top, bot) = (j as f32 * ROW_H, (j + 1) as f32 * ROW_H);
                let (off, h) = self.view;
                if top < off { sa = sa.vertical_scroll_offset(top); } else if h > 0.0 && bot > off + h { sa = sa.vertical_scroll_offset(bot - h); }
            }
            // virtualized: only the visible rows are laid out and painted (and fetch logos);
            // show_rows counts item spacing into the row pitch, so it must be zero here
            ui.spacing_mut().item_spacing.y = 0.0;
            let names = self.names;
            let out = sa.show_rows(ui, ROW_H, rows.len(), |ui, range| {
                for (i, t) in range.clone().zip(&rows[range]) {
                    let (rect, resp) = ui.allocate_exact_size(vec2(W, ROW_H), Sense::click());
                    let sel = self.sel.as_deref() == Some(t.base.as_str());
                    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
                    if sel { ui.painter().rect_filled(rect, kit::rad(), hl()); } else if hv > 0.0 { ui.painter().rect_filled(rect, kit::rad(), hl().gamma_multiply(0.6 * hv)); }
                    let y = rect.center().y;
                    ui.painter().text(pos2(rect.left() + 18.0, y), Align2::RIGHT_CENTER, (i + 1).to_string(), mono(11.0), dim());
                    coin_icon(ui, &t.base, Rect::from_center_size(pos2(rect.left() + 40.0, y), vec2(24.0, 24.0)));
                    let (tick, name, _) = alias::display(&t.base);
                    // main line: ticker (or the company name), the sub line: venue logos and the other one
                    let (main, sub) = match name { Some(n) if names => (n.to_string(), tick), _ => (format!("{tick}/USDT"), name.unwrap_or("")) };
                    let p = ui.painter().with_clip_rect(Rect::from_min_max(rect.min, pos2(rect.left() + X_LAST - 110.0, rect.bottom())).intersect(ui.clip_rect()));
                    let (x0, y1, y2) = (rect.left() + X_PAIR, y - 8.0, y + 9.0);
                    let mx = p.text(pos2(x0, y1), Align2::LEFT_CENTER, &main, prop_m(13.0), if sel { fg_strong() } else { fg() }).right();
                    if t.base == base { check(&p, pos2(mx + 10.0, y1), accent()); }
                    let mut x = x0;
                    for &e in &t.venues { icon(&p, e, Rect::from_min_size(pos2(x, y2 - 6.0), vec2(12.0, 12.0)), false); x += 14.0; }
                    p.text(pos2(x + 4.0, y2), Align2::LEFT_CENTER, sub, prop(11.0), mu());
                    let p = ui.painter();
                    p.text(pos2(rect.left() + X_LAST, y), Align2::RIGHT_CENTER, fmt_px(t.last), mono_m(13.0), fg());
                    p.text(pos2(rect.left() + X_CHG, y), Align2::RIGHT_CENTER, format!("{:+.2}%", t.chg_pct), mono_m(13.0), if t.chg_pct >= 0.0 { up() } else { dn() });
                    p.text(pos2(rect.left() + X_VOL, y), Align2::RIGHT_CENTER, format!("${}", big(t.quote_vol)), mono(12.5), fg());
                    p.text(pos2(rect.left() + X_OI, y), Align2::RIGHT_CENTER, if t.oi_usd > 0.0 { format!("${}", big(t.oi_usd)) } else { "—".into() }, mono(12.5), fg());
                    // hourly rate, APR below
                    match t.funding_h {
                        Some(f) => {
                            p.text(pos2(rect.left() + X_FUND, y1), Align2::RIGHT_CENTER, format!("{:+.4}%", f * 100.0), mono_m(12.5), if f >= 0.0 { up() } else { dn() });
                            p.text(pos2(rect.left() + X_FUND, y2), Align2::RIGHT_CENTER, format!("APR {:+.2}%", f * 100.0 * 24.0 * 365.0), mono(11.0), mu());
                        }
                        None => { p.text(pos2(rect.left() + X_FUND, y), Align2::RIGHT_CENTER, "—", mono(12.5), dim()); }
                    }
                    let fav = self.favs.contains(&t.base);
                    let star = Rect::from_center_size(pos2(rect.right() - 22.0, y), vec2(24.0, 24.0));
                    let star_hover = resp.hovered() && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| star.contains(p));
                    ui.painter().text(star.center(), Align2::CENTER_CENTER, if fav { "★" } else { "☆" }, prop(15.0), if fav { WARN } else if star_hover { fg() } else { dim() });
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        if star_hover { toggle = Some(t.base.clone()); } else { chosen = Some(t.base.clone()); }
                    }
                }
            });
            self.view = (out.state.offset.y, out.inner_rect.height());
            if let Some(b) = toggle { self.toggle_fav(&b); }
            // footer: rows shown and their volume
            ui.add_space(8.0);
            let vol: f64 = rows.iter().map(|t| t.quote_vol).sum();
            let oi: f64 = rows.iter().map(|t| t.oi_usd).sum();
            ui.label(RichText::new(format!("{} {}  ·  ${} {}  ·  ${} {}", rows.len(), l("markets"), big(vol), l("24h volume"), big(oi), l("Open interest"))).font(prop(11.5)).color(mu()));
        });
        // close on choice, Escape or a click outside the sheet
        if chosen.is_some() || m.should_close() { self.open = false; self.was_open = false; }
        if let Some(b) = &chosen { let b = b.clone(); self.opened(&b, base); }
        chosen
    }
}

/// The title bar's pair button: 30 px, outlined; coin icon, "BTC/USDT" 14 semibold, chevron.
fn pair_button(ui: &mut Ui, base: &str, open: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(format!("{base}/USDT"), prop_s(14.0), fg_strong());
    let (r, resp) = ui.allocate_exact_size(vec2(6.0 + 20.0 + 8.0 + g.size().x + 8.0 + 10.0 + 10.0, 30.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || open, 0.1);
    let p = ui.painter();
    p.rect(r, kit::rad() + 1, lerp(panel2(), hl(), hv), egui::Stroke::new(1.0, lerp(line(), edge(), hv)), egui::StrokeKind::Inside);
    coin_icon(ui, base, Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), vec2(20.0, 20.0)));
    let p = ui.painter();
    let tx = r.left() + 34.0;
    p.galley(pos2(tx, r.center().y - g.size().y / 2.0), g.clone(), fg_strong());
    // chevron, flipped while the picker is open
    let c = pos2(tx + g.size().x + 13.0, r.center().y);
    let (w, h) = (3.0, if open { -1.5 } else { 1.5 });
    p.add(egui::Shape::line(vec![c + vec2(-w, -h), c + vec2(0.0, h), c + vec2(w, -h)], egui::Stroke::new(1.5, lerp(mu(), fg(), hv))));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Check mark beside the current pair.
fn check(p: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    p.add(egui::Shape::line(vec![c + vec2(-3.5, 0.0), c + vec2(-1.0, 2.5), c + vec2(3.5, -2.5)], egui::Stroke::new(1.5, col)));
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
    fn tk(b: &str, v: f64) -> Ticker { Ticker { base: b.into(), last: 1.0, chg_pct: 0.0, quote_vol: v, high: 0.0, low: 0.0, base_vol: 0.0, venues: vec![], oi_usd: 0.0, funding_h: None } }

    #[test]
    fn ranked_search() {
        let t = vec![tk("ETHFI", 9.0), tk("ETH", 5.0), tk("WETH", 7.0), tk("BTC", 10.0)];
        let names = |r: Vec<&Ticker>| r.iter().map(|t| t.base.as_str()).collect::<Vec<_>>().join(",");
        // exact, then prefix, then contains; volume order inside each rank
        assert_eq!(names(rank(&t, "eth/usdt", None, None, (Col::Vol, false))), "ETH,ETHFI,WETH");
        // a search ignores the favorites filter; no search shows favorites only
        let favs = vec!["BTC".to_string()];
        assert_eq!(names(rank(&t, "eth", Some(&favs), None, (Col::Vol, false))), "ETH,ETHFI,WETH");
        assert_eq!(names(rank(&t, "", Some(&favs), None, (Col::Vol, false))), "BTC");
        assert_eq!(names(rank(&t, "", None, None, (Col::Pair, true))), "BTC,ETH,ETHFI,WETH");
        // other venues' names find the stock: BB = BlackBerry (BBX) before BounceBit's BB prefix matches
        let s = vec![tk("BB", 9.0), tk("BBX", 1.0), tk("XAU", 5.0), tk("TSLA", 3.0), tk("AAPL", 2.0)];
        assert_eq!(names(rank(&s, "gold", None, None, (Col::Vol, false))), "XAU");
        assert_eq!(names(rank(&s, "tesla", None, None, (Col::Vol, false))), "TSLA");
        assert_eq!(names(rank(&s, "bb", None, None, (Col::Vol, false))), "BB,BBX");
        // the company name finds it too; categories filter
        assert_eq!(names(rank(&s, "apple", None, None, (Col::Vol, false))), "AAPL");
        assert_eq!(names(rank(&s, "blackberry", None, None, (Col::Vol, false))), "BBX");
        assert_eq!(names(rank(&s, "", None, Some(Cat::Stock), (Col::Vol, false))), "TSLA,AAPL,BBX");
        assert_eq!(names(rank(&s, "", None, Some(Cat::Crypto), (Col::Vol, false))), "BB");
    }
}
