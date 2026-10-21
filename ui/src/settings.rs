//! Settings dialog (gear in the title bar, Cmd-,): egui port of SettingsView.swift / SettingsKeys.swift.
//! Like the order panel it reads the JSON state document and writes through the native ops
//! (`set_prefs` / `set_route` with the whole object, `set_lang`, `save_keys`, ...), so there is no Save.
//! `Prefs` persist in settings.json; `apply` pushes them into the palette, zoom and style.
use super::{kit, l, theme::*, tz_label};
use depth::{route::Policy, trade, ws::ProxyMode, Exchange};
use eframe::egui::{self, Color32, RichText, Ui};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

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
    /// Hyperliquid's order-by-order inference from Flowscan's book (third party) instead of the public l2Book
    pub hl_flowscan: bool,
    /// the trades tape under the order book
    pub book_tape: bool,
    /// outbound proxy for every connection; `proxy_url` is used in Custom mode
    pub proxy: ProxyMode,
    pub proxy_url: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { theme: ThemePref::System, tz_min: None, accent: [0x4c, 0x9e, 0xeb], red_up: false, zoom: 1.0, radius: 5, confirm: true, fees: BTreeMap::new(), hl_flowscan: false, book_tape: true, proxy: ProxyMode::None, proxy_url: String::new() }
    }
}

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref { #[default] System, Dark, Light }

// TradingView-style teal / coral (calmer than the system neon pair); SwiftUI (T1.green / T1.red) uses the same
const GREEN: Color32 = Color32::from_rgb(0x26, 0xa6, 0x9a);
const RED: Color32 = Color32::from_rgb(0xef, 0x53, 0x50);
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
        depth::ws::set_proxy(self.proxy, &self.proxy_url);
    }

    /// Theme, accent, price colors, size and radius are all at their defaults.
    fn default_look(&self) -> bool {
        let d = Prefs::default();
        self.theme == d.theme && self.accent == d.accent && self.red_up == d.red_up && self.zoom == d.zoom && self.radius == d.radius
    }
}

/// Sidebar panes (painted monochrome glyphs, design: neutral sidebar).
const PANES: [&str; 5] = ["General", "Appearance", "Trading", "API Keys", "About"];
const W: f32 = 800.0;
const H: f32 = 580.0;
const SIDE: f32 = 196.0;

type Call<'a> = super::order::Call<'a>;

#[derive(Default)]
pub struct SettingsView {
    pub open: bool, tab: u8,
    /// bytes in the cache + log folders, measured once per visit of General
    cache: Option<Option<i64>>,
    /// API key form: venue being edited and its three fields (never pre-filled with secrets)
    edit: Option<String>, key: String, secret: String, extra: String,
    key_err: Option<String>,
    /// venue waiting for the Remove confirmation
    removing: Option<String>,
    app_icon: Option<egui::TextureHandle>,
    /// custom proxy URL being typed (committed on Enter / focus loss, so streams restart once)
    proxy_url: Option<String>,
}

impl SettingsView {
    pub fn open_tab(&mut self, tab: u8) { self.open = true; self.tab = tab.min(4); self.cache = None; }

    pub fn show(&mut self, ctx: &egui::Context, st: &Value, call: &mut Call) {
        if !self.open { return; }
        // design: a popup surface (radius 6, hairline, soft wide shadow)
        let frame = egui::Frame::new().fill(panel_c()).corner_radius(6).stroke(egui::Stroke::new(1.0, line())).inner_margin(0)
            .shadow(egui::Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(if light() { 50 } else { 150 }) });
        let m = egui::Modal::new(egui::Id::new("settings")).frame(frame).show(ctx, |ui| {
            ui.set_width(W);
            ui.set_height(H);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                self.sidebar(ui);
                ui.vertical(|ui| {
                    ui.set_width(W - SIDE);
                    // header: page title, Done; a hairline under it
                    let (hr, _) = ui.allocate_exact_size(egui::vec2(W - SIDE, 56.0), egui::Sense::hover());
                    ui.painter().hline(hr.x_range(), hr.bottom() - 0.5, egui::Stroke::new(1.0, line()));
                    let mut head = ui.new_child(egui::UiBuilder::new().max_rect(hr.shrink2(egui::vec2(24.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                    head.label(RichText::new(l(PANES[self.tab as usize])).font(prop_s(16.0)).color(fg_strong()));
                    head.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if kit::button(ui, l("Done"), kit::Look::Fill(accent()), kit::H, Some(72.0), true).clicked() { self.open = false; }
                    });
                    egui::ScrollArea::vertical().auto_shrink([false; 2]).max_height(H - 56.0).show(ui, |ui| {
                        egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 24, top: 4, bottom: 20 }).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
                            match self.tab {
                                0 => self.general(ui, st, call),
                                1 => appearance(ui, st, call),
                                2 => trading(ui, st, call),
                                3 => self.api_keys(ui, st, call),
                                _ => self.about(ui),
                            }
                        });
                    });
                });
            });
        });
        if m.should_close() && self.removing.is_none() { self.open = false; }
        self.confirm_remove(ctx, call);
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let (side, _) = ui.allocate_exact_size(egui::vec2(SIDE, H), egui::Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(side, egui::CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, panel2());
        p.vline(side.right(), side.y_range(), egui::Stroke::new(1.0, line()));
        p.text(side.left_top() + egui::vec2(20.0, 28.0), egui::Align2::LEFT_CENTER, l("Settings").to_uppercase(), prop_s(10.5), dim());
        for (i, title) in PANES.into_iter().enumerate() {
            let r = egui::Rect::from_min_size(side.left_top() + egui::vec2(10.0, 46.0 + i as f32 * 32.0), egui::vec2(SIDE - 20.0, kit::H + 2.0));
            let resp = ui.interact(r, ui.id().with(("set_tab", i)), egui::Sense::click());
            let on = self.tab == i as u8;
            let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
            if on { p.rect_filled(r, kit::R, hl()); } else if hv > 0.0 { p.rect_filled(r, kit::R, hl().gamma_multiply(0.6 * hv)); }
            let col = if on { fg_strong() } else { lerp(mu(), fg(), hv) };
            pane_icon(&p, egui::Rect::from_center_size(r.left_center() + egui::vec2(18.0, 0.0), egui::vec2(16.0, 16.0)), i, if on { accent() } else { col });
            p.text(r.left_center() + egui::vec2(36.0, 0.0), egui::Align2::LEFT_CENTER, l(title), if on { prop_s(12.5) } else { prop_m(12.5) }, col);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { self.tab = i as u8; self.cache = None; self.close_form(); }
        }
    }

    fn close_form(&mut self) { self.edit = None; self.key.clear(); self.secret.clear(); self.extra.clear(); self.key_err = None; }

    // ------------------------------------------------------------------ General

    fn general(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        let cur = st["lang"].as_str().unwrap_or("en");
        let langs: Vec<(&str, &str)> = st["langs"].as_array().map(|a| a.iter().filter_map(|x| Some((x[0].as_str()?, x[1].as_str()?))).collect()).unwrap_or_default();
        section(ui, "", l("Interface language. Numbers, symbols and codes are not translated."), |ui| {
            row(ui, l("Language"), |ui| {
                let name = langs.iter().find(|x| x.0 == cur).map_or(cur, |x| x.1);
                egui::ComboBox::from_id_salt("set.lang").icon(chevron).width(200.0).selected_text(name).show_ui(ui, |ui| {
                    for (code, name) in &langs {
                        if ui.selectable_label(*code == cur, *name).clicked() && *code != cur { let _ = call(json!({"op": "set_lang", "lang": code})); }
                    }
                });
            });
        });
        let mut p = prefs(st);
        let before = p.clone();
        section(ui, "", l("Chart axis, tables and the status bar clock."), |ui| {
            row(ui, l("Time zone"), |ui| {
                let sys = depth::sys::utc_offset_ms() / 60_000;
                let name = |z: Option<i32>| z.map_or_else(|| format!("{} ({})", l("System"), tz_label(sys)), |m| tz_label(m.into()));
                egui::ComboBox::from_id_salt("set.tz").icon(chevron).width(200.0).selected_text(name(p.tz_min)).height(320.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut p.tz_min, None, name(None));
                    for m in TZS { ui.selectable_value(&mut p.tz_min, Some(m), tz_label(m.into())); }
                });
            });
        });
        section(ui, l("Order book"), "", |ui| {
            row(ui, l("Trades under the book"), |ui| { kit::switch(ui, &mut p.book_tape); });
        });
        section(ui, l("Data sources"), l("Flowscan runs a Hyperliquid node and publishes its book with order counts about every 130 ms (the public feed: every few seconds), which makes the order-by-order estimate in the book much closer. It is a third-party, undocumented service and may stop working; when it fails Depth retries, and turning this off goes back to the public feed."), |ui| {
            row(ui, l("Hyperliquid orders via Flowscan"), |ui| { kit::switch(ui, &mut p.hl_flowscan); });
        });
        let draft = self.proxy_url.get_or_insert_with(|| p.proxy_url.clone());
        let err = (p.proxy == ProxyMode::Custom).then(|| depth::ws::check_proxy(draft).err()).flatten();
        section(ui, l("Network"), err.as_deref().unwrap_or(l("Market data, history and trading all go through this. WebSockets need a SOCKS5 proxy: System uses the system SOCKS proxy, Custom takes socks5://host:port. Changing it reconnects.")), |ui| {
            row(ui, l("Proxy"), |ui| {
                seg(ui, &mut p.proxy, &[(ProxyMode::None, l("No proxy")), (ProxyMode::System, l("System")), (ProxyMode::Custom, l("Custom"))]);
            });
            if p.proxy == ProxyMode::Custom {
                row(ui, l("Address"), |ui| {
                    let r = ui.add(egui::TextEdit::singleline(draft).hint_text("socks5://127.0.0.1:7890").font(mono_m(12.5)).desired_width(220.0).margin(egui::vec2(10.0, 7.0)));
                    if r.lost_focus() && err.is_none() { p.proxy_url = draft.trim().to_string(); }
                });
            }
        });
        set_prefs(&before, &p, call);
        if self.cache.is_none() { self.cache = Some(call(json!({"op": "cache_size"})).ok().and_then(|v| v["bytes"].as_i64())); }
        section(ui, l("Storage"), l("Chart history, coin logos and crash logs. History not viewed for 30 days is removed automatically."), |ui| {
            row(ui, l("Cache and logs"), |ui| {
                if kit::button(ui, l("Clear Cache"), kit::Look::Secondary, kit::H, None, true).clicked() { self.cache = Some(call(json!({"op": "clear_cache"})).ok().and_then(|v| v["bytes"].as_i64())); }
                let s = self.cache.flatten().map_or("–".into(), |b| format!("{:.1} MB", b as f64 / 1e6));
                ui.label(RichText::new(s).font(mono_m(12.5)).color(mu()));
            });
        });
    }

    // ------------------------------------------------------------------ API keys

    /// API keys per venue, stored in the Keychain by the engine. Secrets are write-only: the form
    /// never shows a stored secret (only the key's last four characters) and clears itself.
    fn api_keys(&mut self, ui: &mut Ui, st: &Value, call: &mut Call) {
        ui.add_space(8.0);
        // the withdrawal warning is the first thing on the page, in a tinted callout
        egui::Frame::new().fill(WARN.gamma_multiply(0.10)).stroke(egui::Stroke::new(1.0, WARN.gamma_multiply(0.35))).corner_radius(kit::R)
            .inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    shield(ui.painter(), r);
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(l("Never enable withdrawals on these keys. Trading needs trade permission, transfers need transfer permission. Start with a read-only key."))
                        .font(prop_m(12.5)).color(fg())).wrap());
                });
            });
        footer(ui, l("Stored in the macOS Keychain (service \"depth\"), never written to a file."));
        let keys = st["keys"].as_array().cloned().unwrap_or_default();
        section(ui, l("Venues"), "", |ui| {
            for (i, k) in keys.iter().enumerate() {
                let Some(name) = k["ex"].as_str() else { continue };
                let Some(ex) = Exchange::parse(name) else { continue };
                if i > 0 { row_sep(ui); }
                ui.horizontal(|ui| {
                    ui.set_min_height(42.0);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    icon(ui.painter(), ex, r, false);
                    ui.add_space(8.0);
                    ui.label(RichText::new(name).font(prop_s(12.5)).color(fg_strong()));
                    if !k["verified"].as_bool().unwrap_or(true) {
                        ui.add_space(6.0);
                        kit::chip(ui, &l("Untested").to_uppercase(), kit::Tone::Warn).on_hover_text(l("Implemented and unit-tested, but not yet run against a live account. Test with a read-only key first, then trade the minimum size."));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if k["configured"].as_bool() == Some(true) {
                            if kit::button(ui, l("Remove"), kit::Look::Danger, kit::H_SM + 2.0, None, true).clicked() { self.removing = Some(name.to_string()); }
                            if kit::button(ui, l("Replace"), kit::Look::Secondary, kit::H_SM + 2.0, None, true).clicked() { self.close_form(); self.edit = Some(name.to_string()); }
                            if kit::button(ui, l("Test"), kit::Look::Secondary, kit::H_SM + 2.0, None, true).on_hover_text(l("Read-only check: balances and permissions")).clicked() {
                                if let Err(e) = call(json!({"op": "test_keys", "ex": name})) { self.key_err = Some(e); }
                            }
                            ui.add_space(6.0);
                            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.add_space(4.0);
                                ui.label(RichText::new(format!("✓ {} ••••{}", l("Configured"), k["tail"].as_str().unwrap_or(""))).font(mono_m(11.5)).color(up()));
                                if let Some(msg) = k["test"]["msg"].as_str() {
                                    let (txt, col) = if msg == "…" { (l("Testing…").to_string(), mu()) } else { (msg.to_string(), if k["test"]["ok"].as_bool() == Some(true) { up() } else { dn() }) };
                                    ui.add(egui::Label::new(RichText::new(&txt).font(prop(11.0)).color(col)).truncate()).on_hover_text(txt);
                                }
                            });
                        } else {
                            if kit::button(ui, l("Add…"), kit::Look::Secondary, kit::H_SM + 2.0, None, true).clicked() { self.close_form(); self.edit = Some(name.to_string()); }
                            ui.label(RichText::new(l("Not configured")).font(prop(12.0)).color(dim()));
                        }
                    });
                });
                if self.edit.as_deref() == Some(name) { self.key_form(ui, ex, name, call); }
                else if self.removing.is_none() && self.edit.is_none() {
                    if let Some(e) = &self.key_err { ui.label(RichText::new(e).font(prop(11.0)).color(dn())); }
                }
            }
        });
    }

    fn key_form(&mut self, ui: &mut Ui, ex: Exchange, name: &str, call: &mut Call) {
        let (kl, sl, xl) = trade::key_fields(ex);
        // OKX and Bitget keys are unusable without their passphrase
        let needs_extra = matches!(ex, Exchange::Okx | Exchange::Bitget);
        let (mut save, mut cancel) = (false, false);
        egui::Frame::new().fill(well()).stroke(egui::Stroke::new(1.0, line())).corner_radius(kit::R).inner_margin(12).outer_margin(egui::Margin { left: 0, right: 0, top: 0, bottom: 10 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            let field = |ui: &mut Ui, label: &str, v: &mut String, secret: bool| {
                ui.label(RichText::new(label).font(prop(12.0)).color(mu()));
                ui.add(egui::TextEdit::singleline(v).password(secret).font(mono_m(12.5)).desired_width(f32::INFINITY).margin(egui::vec2(10.0, 7.0)));
                ui.add_space(4.0);
            };
            field(ui, l(kl), &mut self.key, false);
            field(ui, l(sl), &mut self.secret, true);
            if let Some(xl) = xl { field(ui, l(xl), &mut self.extra, true); }
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                warn_triangle(ui.painter(), r);
                ui.label(RichText::new(l("Never enable withdrawal permission.")).font(prop_s(12.0)).color(WARN));
            });
            if let Some(e) = &self.key_err { ui.add(egui::Label::new(RichText::new(e).font(prop(11.5)).color(dn())).wrap()); }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = !self.key.trim().is_empty() && !self.secret.is_empty() && !(needs_extra && self.extra.is_empty());
                if kit::button(ui, l("Save to Keychain"), kit::Look::Fill(accent()), kit::H, None, ok).clicked() { save = true; }
                if kit::button(ui, l("Cancel"), kit::Look::Secondary, kit::H, None, true).clicked() { cancel = true; }
            });
        });
        if cancel { self.close_form(); }
        if save {
            let r = call(json!({"op": "save_keys", "ex": name, "key": self.key.trim(), "secret": self.secret.trim(), "extra": self.extra.trim()}));
            match r {
                // the engine tests a newly saved key right away ("Testing…" until it answers)
                Ok(_) => self.close_form(),
                Err(e) => { self.key_err = Some(if e.is_empty() { l("Failed").into() } else { e }); self.secret.clear(); self.extra.clear(); }
            }
        }
    }

    fn confirm_remove(&mut self, ctx: &egui::Context, call: &mut Call) {
        let Some(ex) = self.removing.clone() else { return };
        let frame = egui::Frame::new().fill(popup_bg()).corner_radius(6).stroke(egui::Stroke::new(1.0, line())).inner_margin(20);
        let mut done = false;
        let m = egui::Modal::new(egui::Id::new("remove_key")).frame(frame).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.label(RichText::new(l("Remove API key?")).font(prop_s(13.5)).color(fg_strong()));
                ui.add(egui::Label::new(RichText::new(format!("{ex}: {}", l("The key is deleted from the Keychain and the venue disconnects from your account."))).font(prop(12.0)).color(mu())).wrap());
                ui.add_space(6.0);
            });
            ui.columns(2, |c| {
                if kit::button(&mut c[0], l("Cancel"), kit::Look::Secondary, kit::H, Some(f32::INFINITY), true).clicked() { done = true; }
                if kit::button(&mut c[1], l("Remove"), kit::Look::Fill(dn()), kit::H, Some(f32::INFINITY), true).clicked() {
                    if let Err(e) = call(json!({"op": "delete_keys", "ex": ex})) { self.key_err = Some(e); }
                    if self.edit.as_deref() == Some(ex.as_str()) { self.close_form(); }
                    done = true;
                }
            });
        });
        if done || m.should_close() { self.removing = None; }
    }

    // ------------------------------------------------------------------ About

    fn about(&mut self, ui: &mut Ui) {
        let tex = self.app_icon.get_or_insert_with(|| {
            let img = decode_png(include_bytes!("../../assets/appicon/icon-1024.png")).unwrap_or_default();
            ui.ctx().load_texture("appicon", img, egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR })
        }).clone();
        section(ui, "", l("Local multi-exchange trading terminal. Runs entirely on this Mac: no relay, no server."), |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(76.0);
                ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(56.0, 56.0)));
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new("Depth").font(prop_s(16.0)).color(fg_strong()));
                    ui.add_space(3.0);
                    ui.label(RichText::new(format!("{} {}", l("Version"), env!("CARGO_PKG_VERSION"))).font(mono_m(12.0)).color(mu()));
                });
            });
        });
        section(ui, l("Data"), "", |ui| {
            let paths = [(l("Settings"), depth::sys::data_dir()), (l("Cache"), depth::sys::cache_dir())];
            for (title, p) in paths {
                let Some(p) = p else { continue };
                row(ui, title, |ui| {
                    let exists = p.exists();
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), if exists { egui::Sense::click() } else { egui::Sense::hover() });
                    let c = r.center();
                    ui.painter().circle_filled(c, 7.5, if exists && resp.hovered() { accent() } else if exists { mu() } else { dim().linear_multiply(0.5) });
                    let s = egui::Stroke::new(1.5, panel_c());
                    ui.painter().line_segment([c - egui::vec2(3.5, 0.0), c + egui::vec2(3.5, 0.0)], s);
                    ui.painter().line_segment([c + egui::vec2(0.5, -3.0), c + egui::vec2(3.5, 0.0)], s);
                    ui.painter().line_segment([c + egui::vec2(0.5, 3.0), c + egui::vec2(3.5, 0.0)], s);
                    if resp.on_hover_text(l("Show in Finder")).clicked() { reveal(&p); }
                    let home = std::env::var("HOME").unwrap_or_default();
                    let mut s = p.display().to_string();
                    if !home.is_empty() && s.starts_with(&home) { s = format!("~{}", &s[home.len()..]); }
                    ui.add(egui::Label::new(RichText::new(&s).font(prop(12.5)).color(mu())).truncate()).on_hover_text(p.display().to_string());
                });
                row_sep(ui);
            }
            row(ui, l("API keys"), |ui| { ui.label(RichText::new(l("macOS Keychain (depth)")).font(prop(12.5)).color(mu())); });
        });
    }
}

/// Reveal `p` in the file manager.
fn reveal(p: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(p).spawn();
    // reap the child so it does not linger as a zombie
    if let Ok(mut c) = r { std::thread::spawn(move || { let _ = c.wait(); }); }
}

/// Offsets in use somewhere, minutes from UTC.
const TZS: [i32; 30] = [-600, -540, -480, -420, -360, -300, -240, -180, -120, -60, 0, 60, 120, 180, 210, 240, 270, 300, 330, 345, 360, 390, 420, 480, 525, 540, 570, 600, 660, 720];

fn prefs(st: &Value) -> Prefs { serde_json::from_value(st["prefs"].clone()).unwrap_or_default() }

fn set_prefs(before: &Prefs, p: &Prefs, call: &mut Call) {
    if p != before { let _ = call(json!({"op": "set_prefs", "prefs": p})); }
}

// ---------------------------------------------------------------------- Appearance

fn appearance(ui: &mut Ui, st: &Value, call: &mut Call) {
    let mut p = prefs(st);
    let before = p.clone();
    section(ui, l("Colors"), l("The accent marks selection and highlights in the chart and book. Price colors apply to candles, the order book, PnL and buy/sell buttons."), |ui| {
        row(ui, l("Theme"), |ui| {
            seg(ui, &mut p.theme, &[(ThemePref::System, l("System")), (ThemePref::Dark, l("Dark")), (ThemePref::Light, l("Light"))]);
        });
        row_sep(ui);
        row(ui, l("Accent color"), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.color_edit_button_srgb(&mut p.accent).on_hover_text(l("Custom"));
            ui.add_space(6.0);
            for (name, c) in ACCENTS.iter().rev() {
                let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                ui.painter().circle_filled(r.center(), 7.0, Color32::from_rgb(c[0], c[1], c[2]));
                if p.accent == *c { ui.painter().circle_stroke(r.center(), 9.5, egui::Stroke::new(2.0, fg())); }
                if resp.on_hover_text(l(name)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { p.accent = *c; }
            }
        });
        row_sep(ui);
        row(ui, l("Price colors"), |ui| {
            let name = |red: bool| if red { l("Red up, green down") } else { l("Green up, red down") };
            egui::ComboBox::from_id_salt("set.updown").icon(chevron).width(200.0).selected_text(name(p.red_up)).show_ui(ui, |ui| {
                for v in [false, true] { ui.selectable_value(&mut p.red_up, v, name(v)); }
            });
        });
    });
    section(ui, l("Chart and book"), l("Scales the chart, order book and option chain. The rest of the window follows the system text size."), |ui| {
        row(ui, l("Interface size"), |ui| {
            let mut z = p.zoom;
            if slider(ui, &mut z, 0.85, 1.3, 0.05, |v| format!("{:.0}%", v * 100.0)) { p.zoom = z; }
        });
        row_sep(ui);
        row(ui, l("Corner radius"), |ui| {
            let mut r = p.radius as f32;
            if slider(ui, &mut r, 0.0, 12.0, 1.0, |v| format!("{v:.0} pt")) { p.radius = r as u8; }
        });
    });
    section(ui, "", "", |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            if kit::button(ui, l("Restore Default Appearance"), kit::Look::Secondary, kit::H, None, !p.default_look()).clicked() {
                p = Prefs { tz_min: p.tz_min, confirm: p.confirm, fees: p.fees.clone(), ..Prefs::default() };
            }
        });
    });
    set_prefs(&before, &p, call);
}

// ---------------------------------------------------------------------- Trading

fn trading(ui: &mut Ui, st: &Value, call: &mut Call) {
    let mut r: Policy = serde_json::from_value(st["route"].clone()).unwrap_or_default();
    let r0 = r.clone();
    let mut p = prefs(st);
    let p0 = p.clone();
    let venue = st["trade"]["venue"].as_str().unwrap_or("").to_string();
    let tradable: Vec<String> = st["tradable"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let foot = if r.smart {
        l("Smart routing picks venues by fee-adjusted price from each venue's own book and can split large orders. The preferred venue drives the order panel's book and limits.")
    } else { l("Every order goes to the trading venue.") };
    section(ui, l("Order routing"), foot, |ui| {
        row(ui, l("Routing"), |ui| {
            seg(ui, &mut r.smart, &[(true, l("Smart")), (false, l("Fixed venue"))]);
        });
        row_sep(ui);
        row(ui, l(if r.smart { "Preferred venue" } else { "Trading venue" }), |ui| {
            let tag = |ex: &str| match Exchange::parse(ex) {
                Some(e) if !trade::VERIFIED.contains(&e) => format!("{ex} ({})", l("untested")),
                _ => ex.to_string(),
            };
            egui::ComboBox::from_id_salt("set.venue").icon(chevron).width(200.0).selected_text(tag(&venue)).show_ui(ui, |ui| {
                for ex in &tradable {
                    if ui.selectable_label(*ex == venue, tag(ex)).clicked() && *ex != venue { let _ = call(json!({"op": "set_trade_venue", "ex": ex})); }
                }
            });
        });
    });
    section(ui, l("Smart routing"), l("Each leg is priced on its venue's real bid/ask, never the composite index. Venues whose book is stale or too far from the others are skipped."), |ui| {
        ui.add_enabled_ui(r.smart, |ui| {
            row(ui, l("Allow split orders"), |ui| { kit::switch(ui, &mut r.allow_split); });
            row_sep(ui);
            let split = r.allow_split;
            ui.add_enabled_ui(split, |ui| {
                row(ui, l("Maximum legs"), |ui| { stepper(ui, &mut r.max_legs, 1, 4); });
                row_sep(ui);
                number(ui, l("Split when saving at least"), &mut r.split_bps, 1, "bp");
                row_sep(ui);
                number(ui, l("Minimum notional to split"), &mut r.min_split_notional, 0, "USDT");
            });
            row_sep(ui);
            number(ui, l("Maximum slippage"), &mut r.max_slip_bps, 1, "bp");
            row_sep(ui);
            number(ui, l("Maximum venue dispersion"), &mut r.max_disp_bps, 1, "bp");
            row_sep(ui);
            let mut ms = r.max_stale_ms as f64;
            if number(ui, l("Ignore books older than"), &mut ms, 0, "ms") { r.max_stale_ms = ms.round().max(0.0) as i64; }
        });
    });
    section(ui, l("Orders"), l("Shows an order summary before it is sent."), |ui| {
        row(ui, l("Confirm before sending orders"), |ui| { kit::switch(ui, &mut p.confirm); });
    });
    section(ui, l("Fees (your tier)"), l("Taker / maker in percent per fill. Used for fee estimates and routing; enter your VIP tier's rates."), |ui| {
        for (i, k) in tradable.iter().enumerate() {
            if i > 0 { row_sep(ui); }
            let (mut tk, mut mk) = p.fees.get(k).copied().unwrap_or(DEFAULT_FEES);
            ui.horizontal(|ui| {
                ui.set_min_height(36.0);
                if let Some(ex) = Exchange::parse(k) {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    icon(ui.painter(), ex, r, false);
                    ui.add_space(8.0);
                }
                ui.label(RichText::new(k).font(prop_m(12.5)).color(fg()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_sized([36.0, 20.0], egui::Label::new(RichText::new("%").font(prop(12.5)).color(mu())));
                    let b = num_field(ui, (k, "m"), &mut mk, 3, "");
                    ui.label(RichText::new("/").color(dim()));
                    let a = num_field(ui, (k, "t"), &mut tk, 3, "");
                    if a || b { p.fees.insert(k.clone(), (tk.clamp(-0.05, 0.2), mk.clamp(-0.05, 0.2))); }
                });
            });
        }
    });
    if r != r0 { let _ = call(json!({"op": "set_route", "route": r})); }
    set_prefs(&p0, &p, call);
}

/// Number row with a unit column (Swift `SettingsNumber`); true when the value changed.
fn number(ui: &mut Ui, title: &str, v: &mut f64, decimals: usize, unit: &str) -> bool {
    let mut changed = false;
    row(ui, title, |ui| {
        ui.add_sized([36.0, 20.0], egui::Label::new(RichText::new(unit).font(prop(12.5)).color(mu())));
        let mut x = *v;
        if num_field(ui, title, &mut x, decimals, "") && x.is_finite() && x >= 0.0 { *v = x; changed = true; }
    });
    changed
}

/// Value with − / + buttons (Swift `Stepper`).
fn stepper(ui: &mut Ui, v: &mut usize, lo: usize, hi: usize) {
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.spacing_mut().item_spacing.x = 4.0;
    if kit::button(ui, "+", kit::Look::Secondary, kit::H_SM, Some(26.0), *v < hi).clicked() { *v += 1; }
    if kit::button(ui, "−", kit::Look::Secondary, kit::H_SM, Some(26.0), *v > lo).clicked() { *v -= 1; }
    ui.add_space(8.0);
    ui.label(RichText::new(v.to_string()).font(mono_m(13.0)).color(fg_strong()));
}

// ---------------------------------------------------------------------- form pieces

/// Grouped form section: overline header, a panel2 group (radius 4, hairline), note below.
fn section(ui: &mut Ui, header: &str, foot: &str, rows: impl FnOnce(&mut Ui)) {
    ui.add_space(18.0);
    if !header.is_empty() {
        kit::overline(ui, header);
        ui.add_space(6.0);
    }
    egui::Frame::new().fill(panel2()).corner_radius(kit::R).stroke(egui::Stroke::new(1.0, line())).inner_margin(egui::Margin::symmetric(14, 2)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        rows(ui);
    });
    footer(ui, foot);
}

fn footer(ui: &mut Ui, text: &str) {
    if text.is_empty() { return; }
    ui.add_space(6.0);
    kit::note(ui, text, dim());
}

/// Label on the left (12.5), control right-aligned; 40 px rows.
fn row(ui: &mut Ui, title: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(40.0);
        ui.label(RichText::new(title).font(prop(12.5)).color(fg()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// kit::segments at a fixed 220 px (it fills the width it is given).
fn seg<T: PartialEq + Copy>(ui: &mut Ui, sel: &mut T, items: &[(T, &str)]) {
    ui.allocate_ui(egui::vec2(220.0, kit::H), |ui| { kit::segments(ui, sel, items, kit::H); });
}

/// Sidebar glyph, stroked in `col` (16 px).
fn pane_icon(p: &egui::Painter, r: egui::Rect, i: usize, col: Color32) {
    let c = r.center();
    let s = egui::Stroke::new(1.4, col);
    let v = egui::vec2;
    match i {
        0 => {
            // gear: ring and eight teeth
            for k in 0..8 {
                let d = egui::Vec2::angled(k as f32 * std::f32::consts::FRAC_PI_4);
                p.line_segment([c + d * 5.0, c + d * 7.5], egui::Stroke::new(2.0, col));
            }
            p.circle_stroke(c, 4.6, s);
            p.circle_filled(c, 1.6, col);
        }
        1 => {
            // half-filled circle (theme)
            p.circle_stroke(c, 6.5, s);
            let pts: Vec<egui::Pos2> = (0..=16).map(|k| c + egui::Vec2::angled(-std::f32::consts::FRAC_PI_2 + k as f32 / 16.0 * std::f32::consts::PI) * 6.5).collect();
            p.add(egui::Shape::convex_polygon(pts, col, egui::Stroke::NONE));
        }
        2 => {
            p.add(egui::Shape::line(vec![c + v(-7.0, 5.0), c + v(-2.5, 0.0), c + v(0.5, 2.5), c + v(6.5, -4.5)], s));
            p.line_segment([c + v(6.5, -4.5), c + v(2.5, -4.5)], s);
            p.line_segment([c + v(6.5, -4.5), c + v(6.5, -0.5)], s);
        }
        3 => {
            p.circle_stroke(c + v(-3.5, 0.0), 3.4, s);
            p.line_segment([c + v(-0.1, 0.0), c + v(7.0, 0.0)], s);
            p.line_segment([c + v(4.5, 0.0), c + v(4.5, 3.0)], s);
            p.line_segment([c + v(6.8, 0.0), c + v(6.8, 2.5)], s);
        }
        _ => {
            p.circle_stroke(c, 6.8, s);
            p.circle_filled(c + v(0.0, -3.2), 1.1, col);
            p.line_segment([c + v(0.0, -1.0), c + v(0.0, 3.8)], egui::Stroke::new(1.6, col));
        }
    }
}

/// Orange shield with an exclamation mark (withdrawal warning).
fn shield(p: &egui::Painter, r: egui::Rect) {
    let (c, h) = (r.center(), r.height() / 2.0);
    let pts = vec![c + egui::vec2(0.0, -h), c + egui::vec2(h * 0.85, -h * 0.6), c + egui::vec2(h * 0.75, h * 0.2), c + egui::vec2(0.0, h),
                   c + egui::vec2(-h * 0.75, h * 0.2), c + egui::vec2(-h * 0.85, -h * 0.6)];
    p.add(egui::Shape::convex_polygon(pts, WARN, egui::Stroke::NONE));
    p.line_segment([c + egui::vec2(0.0, -h * 0.5), c + egui::vec2(0.0, h * 0.2)], egui::Stroke::new(2.0, Color32::WHITE));
    p.circle_filled(c + egui::vec2(0.0, h * 0.5), 1.2, Color32::WHITE);
}

fn warn_triangle(p: &egui::Painter, r: egui::Rect) {
    let (c, h) = (r.center(), r.height() / 2.0);
    p.add(egui::Shape::convex_polygon(vec![c + egui::vec2(0.0, -h), c + egui::vec2(h, h * 0.8), c + egui::vec2(-h, h * 0.8)], WARN, egui::Stroke::NONE));
    p.line_segment([c + egui::vec2(0.0, -h * 0.35), c + egui::vec2(0.0, h * 0.25)], egui::Stroke::new(1.6, Color32::WHITE));
    p.circle_filled(c + egui::vec2(0.0, h * 0.55), 1.0, Color32::WHITE);
}
