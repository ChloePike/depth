//! Settings dialog (gear in the title bar): language, appearance, trading, about.
//! `Prefs` persist in settings.json; `apply` pushes them into the palette, zoom and style.
use super::{lang, set_lang, t, theme::*, tz_label, LANGS};
use eframe::egui::{self, Color32, RichText, Ui};
use std::collections::BTreeMap;
use std::sync::Mutex;
use terminal_one::{trade, Exchange};

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: ThemePref,
    /// display time zone, minutes from UTC; None follows the system
    pub tz_min: Option<i32>,
    pub accent: [u8; 3],
    /// red for rising prices (East Asian convention) instead of green
    pub red_up: bool,
    /// UI zoom, 0.85..=1.3
    pub zoom: f32,
    /// widget corner radius, px
    pub radius: u8,
    /// show the confirmation dialog before sending an order
    pub confirm: bool,
    /// taker / maker fee in percent per venue (drives fee estimates and, later, routing)
    pub fees: BTreeMap<String, (f64, f64)>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { theme: ThemePref::System, tz_min: None, accent: [0x4c, 0x9e, 0xeb], red_up: false, zoom: 1.0, radius: 5, confirm: true, fees: BTreeMap::new() }
    }
}

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref { #[default] System, Dark, Light }

// macOS dark-mode systemGreen / systemRed, so egui and SwiftUI share one red and green
const GREEN: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
const RED: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
const ACCENTS: [(&str, [u8; 3]); 6] = [("Blue", [0x4c, 0x9e, 0xeb]), ("Gold", [0xf0, 0xb9, 0x0b]), ("Violet", [0x9b, 0x8c, 0xf0]),
    ("Teal", [0x2e, 0xc4, 0xb6]), ("Orange", [0xf5, 0x8a, 0x3d]), ("Pink", [0xe0, 0x5b, 0xc4])];
const DEFAULT_FEES: (f64, f64) = (0.055, 0.02);

static FEES: Mutex<BTreeMap<String, (f64, f64)>> = Mutex::new(BTreeMap::new());

/// Taker / maker fee as a fraction for `ex` (user setting, else 0.055% / 0.02%).
pub fn fee(ex: Exchange) -> (f64, f64) {
    let (t, m) = FEES.lock().unwrap().get(&format!("{ex:?}")).copied().unwrap_or(DEFAULT_FEES);
    (t / 100.0, m / 100.0)
}

impl Prefs {
    pub fn apply(&self, ctx: &egui::Context) {
        let [r, g, b] = self.accent;
        let (u, d) = if self.red_up { (RED, GREEN) } else { (GREEN, RED) };
        set_palette(Color32::from_rgb(r, g, b), u, d);
        super::set_tz(self.tz_min);
        ctx.set_theme(match self.theme { ThemePref::System => egui::ThemePreference::System, ThemePref::Dark => egui::ThemePreference::Dark, ThemePref::Light => egui::ThemePreference::Light });
        ctx.set_zoom_factor(self.zoom.clamp(0.85, 1.3));
        let rad = self.radius.min(12);
        ctx.all_styles_mut(|s| {
            for w in [&mut s.visuals.widgets.noninteractive, &mut s.visuals.widgets.inactive, &mut s.visuals.widgets.hovered, &mut s.visuals.widgets.active, &mut s.visuals.widgets.open] {
                w.corner_radius = rad.into();
            }
            s.visuals.selection.bg_fill = Color32::from_rgb(r, g, b).linear_multiply(0.35);
            s.visuals.widgets.active.bg_stroke.color = Color32::from_rgb(r, g, b);
        });
        *FEES.lock().unwrap() = self.fees.clone();
    }
}

#[derive(Default)]
pub struct SettingsView {
    pub open: bool, tab: u8, keys: Option<Vec<(Exchange, bool)>>,
    /// API key form: venue being edited and its three fields (never pre-filled with secrets)
    edit: Option<(Exchange, String, String, String)>,
    key_err: Option<String>,
    /// bytes in the cache + log folders, measured once per visit of the About tab
    cache: Option<u64>,
}

impl SettingsView {
    pub fn open_tab(&mut self, tab: u8) { self.open = true; self.tab = tab; }

    /// Returns a newly chosen trading venue.
    pub fn show(&mut self, ctx: &egui::Context, prefs: &mut Prefs, trade_ex: Exchange, eng: &mut super::engine::Engine) -> Option<Exchange> {
        if !self.open { return None; }
        let before = prefs.clone();
        let mut venue = None;
        const TABS: [&str; 5] = ["set.general", "set.appearance", "set.trading", "set.api", "set.about"];
        let frame = egui::Frame::new().fill(panel()).corner_radius(12).stroke(egui::Stroke::new(1.0, line())).inner_margin(0);
        let m = egui::Modal::new(egui::Id::new("settings")).frame(frame).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.set_height(500.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                // sidebar
                let (side, _) = ui.allocate_exact_size(egui::vec2(180.0, 500.0), egui::Sense::hover());
                ui.painter().rect_filled(side, egui::CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 }, label_bg());
                ui.painter().text(side.left_top() + egui::vec2(20.0, 26.0), egui::Align2::LEFT_CENTER, t("set.title"), prop(15.0), fg());
                for (i, k) in TABS.into_iter().enumerate() {
                    let r = egui::Rect::from_min_size(side.left_top() + egui::vec2(10.0, 52.0 + i as f32 * 32.0), egui::vec2(160.0, 28.0));
                    let resp = ui.interact(r, ui.id().with(("set_tab", i)), egui::Sense::click());
                    let on = self.tab == i as u8;
                    if on {
                        ui.painter().rect_filled(r, 6, hl());
                        ui.painter().rect_filled(egui::Rect::from_min_size(r.left_top() + egui::vec2(0.0, 7.0), egui::vec2(3.0, 14.0)), 2, accent());
                    } else if resp.hovered() { ui.painter().rect_filled(r, 6, hl().linear_multiply(0.5)); }
                    ui.painter().text(r.left_center() + egui::vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, t(k), prop(12.5), if on { fg() } else { mu() });
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.tab = i as u8; self.keys = None; self.cache = None; }
                }
                ui.painter().vline(side.right(), side.y_range(), egui::Stroke::new(1.0, line()));
                // content
                ui.vertical(|ui| {
                    ui.set_width(540.0);
                    egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 20, top: 16, bottom: 16 }).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t(TABS[self.tab as usize % 5])).font(prop(16.0)).strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // close (x)
                                let (r, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
                                if resp.hovered() { ui.painter().rect_filled(r, 5, hl()); }
                                let (c, d) = (r.center(), 5.0);
                                for (a, b) in [(egui::vec2(-d, -d), egui::vec2(d, d)), (egui::vec2(-d, d), egui::vec2(d, -d))] {
                                    ui.painter().line_segment([c + a, c + b], egui::Stroke::new(1.5, mu()));
                                }
                                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.open = false; }
                            });
                        });
                        egui::ScrollArea::vertical().max_height(440.0).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(8.0, 2.0);
                            match self.tab {
                                0 => general(ui, prefs),
                                1 => appearance(ui, prefs),
                                2 => venue = trading(ui, prefs, trade_ex),
                                3 => self.api_keys(ui, eng),
                                // Keychain lookups spawn `security`: once per visit, never per frame
                                _ => {
                                    about(ui, self.keys.get_or_insert_with(|| trade::TRADABLE.into_iter().map(|e| (e, trade::keychain(e).is_some())).collect()));
                                    let bytes = *self.cache.get_or_insert_with(terminal_one::sys::cache_bytes);
                                    group(ui, t("set.storage"), |ui| {
                                        setting_row(ui, t("set.cache"), t("set.cache_desc"), |ui| {
                                            if ui.button(t("set.cache_clear")).clicked() { terminal_one::sys::clear_cache(); self.cache = None; }
                                            ui.label(RichText::new(format!("{:.1} MB", bytes as f64 / 1e6)).font(mono(11.5)).color(mu()));
                                        });
                                    });
                                }
                            }
                        });
                    });
                });
            });
        });
        if m.should_close() { self.open = false; }
        if *prefs != before { prefs.apply(ctx); }
        venue
    }
}

impl SettingsView {
    /// API keys per venue, stored in the macOS Keychain. Secrets are write-only here: the form
    /// never shows a stored secret, only the key's last characters.
    fn api_keys(&mut self, ui: &mut Ui, eng: &mut super::engine::Engine) {
        ui.add_space(4.0);
        ui.label(RichText::new(t("set.api_note")).font(prop(11.0)).color(dim()));
        let have: std::collections::HashMap<Exchange, String> = eng.account.lock().unwrap().keys.iter()
            .map(|(e, k)| (*e, k.key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect())).collect();
        let tests = eng.key_tests.lock().unwrap().clone();
        let live: Vec<Exchange> = trade::TRADABLE.to_vec();
        let verified: Vec<Exchange> = trade::VERIFIED.to_vec();
        group(ui, t("set.api"), |ui| {
            for (i, ex) in trade::KEY_VENUES.into_iter().enumerate() {
                if i > 0 { row_sep(ui); }
                ui.horizontal(|ui| {
                    ui.set_min_height(34.0);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    icon(ui.painter(), ex, r, false);
                    ui.label(RichText::new(format!("{ex:?}")).font(prop(12.5)));
                    if !verified.contains(&ex) { ui.label(RichText::new(t("set.untested")).font(prop(10.5)).color(WARN)).on_hover_text(t("set.untested_tip")); }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match have.get(&ex) {
                            Some(tail) => {
                                if link(ui, t("set.api_remove")).clicked() { trade::delete_keys(ex); eng.set_keys(ex, None); self.keys = None; }
                                ui.add_space(8.0);
                                if link(ui, t("set.api_test")).clicked() { if let Some(k) = eng.account.lock().unwrap().keys.get(&ex).cloned() { eng.test_keys(ex, k, ui.ctx()); } }
                                ui.add_space(8.0);
                                if link(ui, t("set.api_replace")).clicked() { self.edit = Some((ex, String::new(), String::new(), String::new())); self.key_err = None; }
                                ui.add_space(8.0);
                                ui.label(RichText::new(format!("{} ••••{tail}", t("set.key_ok"))).font(mono(11.0)).color(up()));
                            }
                            None => { if link(ui, t("set.api_add")).clicked() { self.edit = Some((ex, String::new(), String::new(), String::new())); self.key_err = None; } }
                        }
                    });
                });
                if let Some((ok, msg)) = tests.get(&ex) {
                    ui.label(RichText::new(msg).font(prop(10.5)).color(if *ok { up() } else { dn() }));
                }
                // inline form under the venue being edited
                let editing = self.edit.as_ref().is_some_and(|e| e.0 == ex);
                if editing {
                    let (kl, sl, xl) = trade::key_fields(ex);
                    let mut save = false;
                    let mut cancel = false;
                    if let Some((_, k, s, x)) = self.edit.as_mut() {
                        egui::Frame::new().fill(bg()).corner_radius(6).inner_margin(10).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for (label, v, secret) in [(kl, &mut *k, false), (sl, &mut *s, true)] {
                                ui.label(RichText::new(label).font(prop(11.0)).color(dim()));
                                ui.add(egui::TextEdit::singleline(v).password(secret).font(mono(12.0)).desired_width(f32::INFINITY));
                            }
                            if let Some(xl) = xl {
                                ui.label(RichText::new(xl).font(prop(11.0)).color(dim()));
                                ui.add(egui::TextEdit::singleline(x).password(true).font(mono(12.0)).desired_width(f32::INFINITY));
                            }
                            ui.add_space(4.0);
                            ui.label(RichText::new(t("set.api_perm")).font(prop(10.5)).color(WARN));
                            ui.horizontal(|ui| {
                                if ui.button(t("tr.cancel")).clicked() { cancel = true; }
                                if ui.add(egui::Button::new(RichText::new(t("set.api_save")).strong()).fill(accent().linear_multiply(0.5))).clicked() { save = true; }
                            });
                        });
                    }
                    if let Some(e) = &self.key_err { ui.label(RichText::new(e).font(prop(10.5)).color(dn())); }
                    if cancel { self.edit = None; self.key_err = None; }
                    if save {
                        let (_, k, s, x) = self.edit.clone().unwrap();
                        let xo = (!x.trim().is_empty()).then_some(x.as_str());
                        match trade::save_keys(ex, &k, &s, xo) {
                            Ok(()) => {
                                let keys = trade::keychain(ex);
                                if let Some(kk) = keys.clone() { eng.test_keys(ex, kk, ui.ctx()); }
                                if live.contains(&ex) { eng.set_keys(ex, keys); }
                                self.edit = None;
                                self.key_err = None;
                                self.keys = None;
                            }
                            Err(e) => self.key_err = Some(format!("{e:#}")),
                        }
                    }
                }
            }
        });
    }
}

/// Offsets in use somewhere, minutes from UTC.
const TZS: [i32; 30] = [-600, -540, -480, -420, -360, -300, -240, -180, -120, -60, 0, 60, 120, 180, 210, 240, 270, 300, 330, 345, 360, 390, 420, 480, 525, 540, 570, 600, 660, 720];

fn general(ui: &mut Ui, p: &mut Prefs) {
    group(ui, t("set.language"), |ui| {
        setting_row(ui, t("set.language"), t("set.language_desc"), |ui| {
            let mut l = lang();
            egui::ComboBox::from_id_salt("set.lang").selected_text(LANGS[l].1).show_ui(ui, |ui| {
                for (i, (_, name)) in LANGS.iter().enumerate() { ui.selectable_value(&mut l, i, *name); }
            });
            if l != lang() { set_lang(LANGS[l].0); }
        });
        row_sep(ui);
        setting_row(ui, t("set.tz"), t("set.tz_desc"), |ui| {
            let sys = terminal_one::sys::utc_offset_ms() / 60_000;
            let name = |z: Option<i32>| z.map_or_else(|| format!("{} ({})", t("set.tz_system"), tz_label(sys)), |m| tz_label(m.into()));
            egui::ComboBox::from_id_salt("set.tz").selected_text(name(p.tz_min)).height(320.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut p.tz_min, None, name(None));
                for m in TZS { ui.selectable_value(&mut p.tz_min, Some(m), tz_label(m.into())); }
            });
        });
    });
}

fn appearance(ui: &mut Ui, p: &mut Prefs) {
    group(ui, t("set.colors"), |ui| {
        setting_row(ui, t("set.theme"), t("set.theme_desc"), |ui| {
            let mut sel = p.theme as usize;
            if segment(ui, &[t("set.theme_system"), t("set.theme_dark"), t("set.theme_light")], &mut sel) {
                p.theme = [ThemePref::System, ThemePref::Dark, ThemePref::Light][sel];
            }
        });
        row_sep(ui);
        setting_row(ui, t("set.accent"), t("set.accent_desc"), |ui| {
            ui.color_edit_button_srgb(&mut p.accent);
            ui.add_space(8.0);
            for (name, c) in ACCENTS.iter().rev() {
                let col = Color32::from_rgb(c[0], c[1], c[2]);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                ui.painter().circle_filled(r.center(), 8.0, col);
                if p.accent == *c { ui.painter().circle_stroke(r.center(), 10.5, egui::Stroke::new(1.5, fg())); }
                if resp.on_hover_text(*name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { p.accent = *c; }
            }
        });
        row_sep(ui);
        setting_row(ui, t("set.updown"), t("set.updown_desc"), |ui| {
            let mut sel = p.red_up as usize;
            if segment(ui, &[t("set.green_up"), t("set.red_up")], &mut sel) { p.red_up = sel == 1; }
        });
    });
    group(ui, t("set.layout"), |ui| {
        setting_row(ui, t("set.zoom"), t("set.zoom_desc"), |ui| {
            let mut z = p.zoom;
            if slider(ui, &mut z, 0.85, 1.3, 0.05, |v| format!("{:.0}%", v * 100.0)) { p.zoom = z; }
        });
        row_sep(ui);
        setting_row(ui, t("set.radius"), "", |ui| {
            let mut r = p.radius as f32;
            if slider(ui, &mut r, 0.0, 12.0, 1.0, |v| format!("{v:.0} px")) { p.radius = r as u8; }
        });
    });
    ui.add_space(10.0);
    if link(ui, t("set.reset")).clicked() { *p = Prefs { fees: p.fees.clone(), confirm: p.confirm, ..Prefs::default() }; }
}

fn trading(ui: &mut Ui, p: &mut Prefs, cur: Exchange) -> Option<Exchange> {
    let mut out = None;
    group(ui, t("set.routing"), |ui| {
        setting_row(ui, t("set.routing_mode"), t("set.smart_tip"), |ui| {
            let mut sel = 1usize;
            ui.add_enabled_ui(false, |ui| segment(ui, &[t("set.smart"), t("set.fixed")], &mut sel));
        });
        row_sep(ui);
        setting_row(ui, t("set.fixed_venue"), t("set.fixed_desc"), |ui| {
            egui::ComboBox::from_id_salt("set_venue").icon(chevron).width(170.0).selected_text(format!("{cur:?}")).show_ui(ui, |ui| {
                for ex in trade::TRADABLE {
                    let tag = if trade::VERIFIED.contains(&ex) { String::new() } else { format!("   {}", t("set.untested")) };
                    if ui.selectable_label(ex == cur, format!("{ex:?}{tag}")).clicked() { out = Some(ex); }
                }
            });
        });
    });
    group(ui, t("set.orders"), |ui| {
        setting_row(ui, t("set.confirm"), t("set.confirm_desc"), |ui| { toggle(ui, &mut p.confirm); });
    });
    group(ui, t("set.fees"), |ui| {
        for (i, ex) in trade::TRADABLE.into_iter().enumerate() {
            if i > 0 { row_sep(ui); }
            let k = format!("{ex:?}");
            let (mut tk, mut mk) = p.fees.get(&k).copied().unwrap_or(DEFAULT_FEES);
            ui.horizontal(|ui| {
                ui.set_min_height(34.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                icon(ui.painter(), ex, r, false);
                ui.label(RichText::new(&k).font(prop(12.5)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let b = num_field(ui, (&k, "m"), &mut mk, 3, "%");
                    ui.label(RichText::new(t("set.maker")).font(prop(11.0)).color(dim()));
                    ui.add_space(10.0);
                    let a = num_field(ui, (&k, "t"), &mut tk, 3, "%");
                    ui.label(RichText::new(t("set.taker")).font(prop(11.0)).color(dim()));
                    if a || b { p.fees.insert(k.clone(), (tk.clamp(-0.05, 0.2), mk.clamp(-0.05, 0.2))); }
                });
            });
        }
    });
    ui.add_space(4.0);
    ui.label(RichText::new(t("set.fees_note")).font(prop(10.5)).color(dim()));
    out
}

fn about(ui: &mut Ui, keys: &[(Exchange, bool)]) {
    group(ui, t("set.keys"), |ui| {
        for (i, &(ex, has)) in keys.iter().enumerate() {
            if i > 0 { row_sep(ui); }
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                icon(ui.painter(), ex, r, false);
                ui.label(RichText::new(format!("{ex:?}")).font(prop(12.5)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(if has { t("set.key_ok") } else { t("set.key_missing") }).color(if has { up() } else { dim() }));
                });
            });
        }
    });
    group(ui, t("set.paths"), |ui| {
        for (i, p) in [terminal_one::sys::data_dir(), terminal_one::sys::cache_dir()].into_iter().flatten().enumerate() {
            if i > 0 { row_sep(ui); }
            ui.horizontal(|ui| { ui.set_min_height(28.0); ui.label(RichText::new(p.display().to_string()).font(mono(10.5)).color(mu())); });
        }
    });
    group(ui, t("set.version"), |ui| {
        ui.horizontal(|ui| { ui.set_min_height(28.0); ui.label(RichText::new(format!("Depth {}", env!("CARGO_PKG_VERSION"))).color(mu())); });
    });
}
