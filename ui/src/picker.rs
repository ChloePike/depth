//! Symbol picker: searchable list of USDT perpetuals (Binance futures 24h tickers, by volume),
//! with favorites persisted under ~/Library/Application Support/TerminalOne/favorites.txt.
use super::{fmt_px, t, theme::*};
use crate::engine::Ticker;
use eframe::egui::{self, Align2, RichText, Ui};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub struct Picker {
    pub open: bool,
    query: String,
    favs_only: bool,
    favs: BTreeSet<String>,
}

fn favs_path() -> Option<PathBuf> {
    Some(terminal_one::sys::data_dir()?.join("favorites.txt"))
}

impl Default for Picker {
    fn default() -> Self {
        let favs = favs_path().and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_else(|| ["BTC", "ETH", "SOL"].into_iter().map(String::from).collect());
        Picker { open: false, query: String::new(), favs_only: false, favs }
    }
}

impl Picker {
    fn save(&self) {
        let Some(p) = favs_path() else { return };
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, self.favs.iter().cloned().collect::<Vec<_>>().join("\n"));
    }

    /// Button showing the current pair; returns a newly chosen base asset.
    pub fn show(&mut self, ui: &mut Ui, base: &str, tickers: &[Ticker]) -> Option<String> {
        let btn = ui.add(egui::Button::new(RichText::new(format!("       {base}/USDT  ▾")).font(prop(15.0)).strong()).min_size(egui::vec2(120.0, 28.0)));
        coin_icon(ui, base, egui::Rect::from_center_size(egui::pos2(btn.rect.left() + 18.0, btn.rect.center().y), egui::vec2(18.0, 18.0)));
        let just_opened = btn.clicked() && !self.open;
        if btn.clicked() { self.open = !self.open; if self.open { self.query.clear(); } }
        if !self.open { return None; }
        let mut chosen = None;
        let area = egui::Area::new(egui::Id::new("picker")).order(egui::Order::Foreground).fixed_pos(btn.rect.left_bottom() + egui::vec2(0.0, 6.0));
        let inner = area.show(ui.ctx(), |ui| {
            egui::Frame::new().fill(panel2()).stroke(egui::Stroke::new(1.0, line())).corner_radius(6).inner_margin(10).show(ui, |ui| {
                ui.set_width(460.0);
                let q = ui.add(egui::TextEdit::singleline(&mut self.query).hint_text(t("pick.search")).desired_width(f32::INFINITY));
                // focus once when opened; re-requesting every frame fights row clicks and forces
                // a repaint every frame
                if just_opened { q.request_focus(); }
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.favs_only, t("pick.favs")).clicked() { self.favs_only = true; }
                    if ui.selectable_label(!self.favs_only, t("pick.all")).clicked() { self.favs_only = false; }
                });
                ui.separator();
                let query = self.query.trim().to_uppercase();
                let rows: Vec<&Ticker> = tickers.iter()
                    .filter(|x| query.is_empty() || x.base.contains(&query))
                    .filter(|x| !self.favs_only || self.favs.contains(&x.base)).collect();
                if rows.is_empty() { ui.label(RichText::new(t("pick.empty")).color(dim())); }
                if q.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) { chosen = rows.first().map(|x| x.base.clone()); }
                let mut toggle = None;
                // virtualized: only the visible rows are laid out and painted
                const ROW_H: f32 = 24.0;
                egui::ScrollArea::vertical().max_height(420.0).show_rows(ui, ROW_H, rows.len(), |ui, range| {
                    for x in &rows[range] {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
                        let p = ui.painter();
                        if resp.hovered() || x.base == base { p.rect_filled(rect, 3, hl()); }
                        let y = rect.center().y;
                        let fav = self.favs.contains(&x.base);
                        let star = egui::Rect::from_center_size(egui::pos2(rect.left() + 10.0, y), egui::vec2(16.0, 16.0));
                        p.text(star.center(), Align2::CENTER_CENTER, "★", prop(12.0), if fav { WARN } else { dim() });
                        coin_icon(ui, &x.base, egui::Rect::from_center_size(egui::pos2(rect.left() + 32.0, y), egui::vec2(16.0, 16.0)));
                        let p = ui.painter();
                        p.text(egui::pos2(rect.left() + 46.0, y), Align2::LEFT_CENTER, format!("{}USDT", x.base), prop(12.5), fg());
                        p.text(egui::pos2(rect.left() + 250.0, y), Align2::RIGHT_CENTER, fmt_px(x.last), mono(11.5), fg());
                        p.text(egui::pos2(rect.left() + 330.0, y), Align2::RIGHT_CENTER, format!("{:+.2}%", x.chg_pct), mono(11.5), if x.chg_pct >= 0.0 { up() } else { dn() });
                        let v = x.quote_vol;
                        let vs = if v >= 1e9 { format!("{:.2}B", v / 1e9) } else { format!("{:.1}M", v / 1e6) };
                        p.text(egui::pos2(rect.right() - 6.0, y), Align2::RIGHT_CENTER, vs, mono(11.0), mu());
                        if resp.clicked() {
                            if resp.interact_pointer_pos().is_some_and(|pt| star.contains(pt)) { toggle = Some(x.base.clone()); } else { chosen = Some(x.base.clone()); }
                        }
                    }
                });
                if let Some(b) = toggle {
                    if !self.favs.remove(&b) { self.favs.insert(b); }
                    self.save();
                }
            });
        });
        // close on choice, Escape or a click outside the button and the popup
        let clicked_outside = ui.input(|i| i.pointer.any_click())
            && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| !inner.response.rect.contains(p) && !btn.rect.contains(p));
        if chosen.is_some() || clicked_outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) { self.open = false; }
        chosen
    }
}
