//! Palette, venue colors, fonts (SF Pro / SF Mono / PingFang SC from the system) and style.
use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Painter, Rect, TextStyle, TextureHandle};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use terminal_one::Exchange;

const BG_C: Color32 = Color32::from_rgb(0x16, 0x16, 0x17);
const PANEL_C: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x20);
/// translucent white overlays: they read the same on the solid egui panel and on the native window
pub const PANEL2: Color32 = Color32::from_rgba_premultiplied(13, 13, 13, 13);
pub const HL: Color32 = Color32::from_rgba_premultiplied(23, 23, 23, 23);
pub const LINE: Color32 = Color32::from_rgba_premultiplied(28, 28, 28, 28);
pub const GRID: Color32 = Color32::from_rgba_premultiplied(14, 14, 14, 14);
pub const FG: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const MU: Color32 = Color32::from_rgb(0x9b, 0x9b, 0x9b);
pub const DIM: Color32 = Color32::from_rgb(0x6e, 0x6e, 0x6e);
/// opaque fill for labels painted over the chart
pub const LABEL_BG: Color32 = Color32::from_rgb(0x26, 0x26, 0x28);

static NATIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Hosted in the SwiftUI window: egui leaves its backgrounds transparent and the native window
/// (with its wallpaper tinting) shows through.
pub fn set_native(on: bool) { NATIVE.store(on, Relaxed); }
pub fn native() -> bool { NATIVE.load(Relaxed) }
pub fn bg() -> Color32 { if native() { Color32::TRANSPARENT } else { BG_C } }
pub fn panel() -> Color32 { if native() { Color32::TRANSPARENT } else { PANEL_C } }
/// User-adjustable colors (Settings > Appearance), packed 0xRRGGBB; read every frame.
static ACCENT_C: AtomicU32 = AtomicU32::new(0x4c9eeb);
static UP_C: AtomicU32 = AtomicU32::new(0x30d158);
static DN_C: AtomicU32 = AtomicU32::new(0xff453a);
fn unpack(v: u32) -> Color32 { Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8) }
pub fn pack(c: Color32) -> u32 { (c.r() as u32) << 16 | (c.g() as u32) << 8 | c.b() as u32 }
pub fn accent() -> Color32 { unpack(ACCENT_C.load(Relaxed)) }
/// Rising / buy color (green by default; red when the user picks red-up).
pub fn up() -> Color32 { unpack(UP_C.load(Relaxed)) }
pub fn dn() -> Color32 { unpack(DN_C.load(Relaxed)) }
pub fn set_palette(accent: Color32, up: Color32, dn: Color32) { ACCENT_C.store(pack(accent), Relaxed); UP_C.store(pack(up), Relaxed); DN_C.store(pack(dn), Relaxed); }
pub const WARN: Color32 = Color32::from_rgb(0xe8, 0xa3, 0x3d);
pub const PURPLE: Color32 = Color32::from_rgb(0x9b, 0x8c, 0xf0);

pub fn ex_color(e: Exchange) -> Color32 {
    match e {
        Exchange::Binance => Color32::from_rgb(0xe0, 0xb9, 0x3a),
        Exchange::Bybit => Color32::from_rgb(0xf0, 0x8a, 0x3c),
        Exchange::Bitget => Color32::from_rgb(0x2b, 0xc4, 0xc8),
        Exchange::Okx => Color32::from_rgb(0xd8, 0xdd, 0xe3),
        Exchange::Mexc => Color32::from_rgb(0x3d, 0x7f, 0xe8),
        Exchange::Coinbase => Color32::from_rgb(0x55, 0x8c, 0xff),
        Exchange::Kraken => Color32::from_rgb(0x8f, 0x6c, 0xf2),
        Exchange::Gate => Color32::from_rgb(0x2e, 0xbd, 0x85),
        Exchange::Hyperliquid => Color32::from_rgb(0x6f, 0xe0, 0xc8),
        Exchange::Lighter => Color32::from_rgb(0xe8, 0x6a, 0xb8),
    }
}

fn icon_png(e: Exchange) -> &'static [u8] {
    match e {
        Exchange::Binance => include_bytes!("../../assets/icons/binance.png"),
        Exchange::Bybit => include_bytes!("../../assets/icons/bybit.png"),
        Exchange::Bitget => include_bytes!("../../assets/icons/bitget.png"),
        Exchange::Okx => include_bytes!("../../assets/icons/okx.png"),
        Exchange::Mexc => include_bytes!("../../assets/icons/mexc.png"),
        Exchange::Coinbase => include_bytes!("../../assets/icons/coinbase.png"),
        Exchange::Kraken => include_bytes!("../../assets/icons/kraken.png"),
        Exchange::Gate => include_bytes!("../../assets/icons/gate.png"),
        Exchange::Hyperliquid => include_bytes!("../../assets/icons/hyperliquid.png"),
        Exchange::Lighter => include_bytes!("../../assets/icons/lighter.png"),
    }
}

fn decode_png(bytes: &[u8]) -> Option<egui::ColorImage> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    // palette (indexed) and low-bit images expand to 8-bit RGB(A) / gray
    d.set_transformations(png::Transformations::EXPAND);
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        _ => return None,
    };
    Some(egui::ColorImage::from_rgba_unmultiplied([info.width as usize, info.height as usize], &rgba))
}

static ICONS: OnceLock<HashMap<Exchange, TextureHandle>> = OnceLock::new();

/// Exchange logos (assets/icons, from CoinGecko) as round textures.
pub fn load_icons(ctx: &egui::Context) {
    let m = Exchange::ALL.iter().filter_map(|e| {
        let img = decode_png(icon_png(*e))?;
        Some((*e, ctx.load_texture(format!("icon-{e:?}"), img, egui::TextureOptions::LINEAR)))
    }).collect();
    let _ = ICONS.set(m);
}

/// Paint an exchange logo as a circle in `rect`; greyed when `dim`. Falls back to a color dot.
pub fn icon(p: &Painter, e: Exchange, rect: Rect, dim: bool) {
    let tint = if dim { Color32::from_gray(70) } else { Color32::WHITE };
    match ICONS.get().and_then(|m| m.get(&e)) {
        Some(t) => {
            let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            p.add(egui::epaint::RectShape::filled(rect, rect.width() / 2.0, tint).with_texture(t.id(), uv));
        }
        None => { p.circle_filled(rect.center(), rect.width() / 2.0, if dim { DIM } else { ex_color(e) }); }
    }
}

enum Coin { Loading, Ready(TextureHandle), Missing }

static COINS: std::sync::Mutex<Option<HashMap<String, Coin>>> = std::sync::Mutex::new(None);
static COIN_TX: OnceLock<std::sync::mpsc::Sender<(String, egui::Context)>> = OnceLock::new();

/// Coin logos come from Binance's logo CDN on first use, are cached on disk
/// (~/Library/Caches/TerminalOne/coins; an empty file marks "no logo"), and decode on one
/// background thread. Never blocks the frame.
fn coin_fetcher() -> &'static std::sync::mpsc::Sender<(String, egui::Context)> {
    COIN_TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(String, egui::Context)>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("coin icon runtime");
            let dir = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Caches/TerminalOne/coins"));
            while let Ok((sym, ctx)) = rx.recv() {
                let path = dir.as_ref().map(|d| d.join(format!("{sym}.png")));
                let bytes = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
                    Some(b) => b,
                    None => {
                        let url = format!("https://bin.bnbstatic.com/static/assets/logos/{sym}.png");
                        let got = rt.block_on(async {
                            let r = terminal_one::ws::http().get(&url).send().await.ok()?;
                            if !r.status().is_success() { return Some(vec![]); }
                            r.bytes().await.ok().map(|b| b.to_vec())
                        });
                        // network errors are not cached (retried next launch); a 404 is
                        let Some(b) = got else { set_coin(&sym, Coin::Missing); continue };
                        if let Some(p) = &path { if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); } let _ = std::fs::write(p, &b); }
                        b
                    }
                };
                let state = decode_png(&bytes).map(|img| Coin::Ready(ctx.load_texture(format!("coin-{sym}"), img, egui::TextureOptions::LINEAR))).unwrap_or(Coin::Missing);
                set_coin(&sym, state);
                ctx.request_repaint();
            }
        });
        tx
    })
}

fn set_coin(sym: &str, c: Coin) { COINS.lock().unwrap().get_or_insert_with(Default::default).insert(sym.to_string(), c); }

/// Paint a coin logo (base asset, e.g. "BTC") as a circle; a lettered disc until/unless it loads.
pub fn coin_icon(ui: &egui::Ui, sym: &str, rect: Rect) {
    let tex = {
        let mut g = COINS.lock().unwrap();
        let m = g.get_or_insert_with(Default::default);
        match m.get(sym) {
            Some(Coin::Ready(t)) => Some(t.id()),
            Some(_) => None,
            None => { m.insert(sym.to_string(), Coin::Loading); let _ = coin_fetcher().send((sym.to_string(), ui.ctx().clone())); None }
        }
    };
    let p = ui.painter();
    match tex {
        Some(id) => { p.add(egui::epaint::RectShape::filled(rect, rect.width() / 2.0, Color32::WHITE).with_texture(id, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)))); }
        None => {
            p.circle_filled(rect.center(), rect.width() / 2.0, HL);
            p.text(rect.center(), egui::Align2::CENTER_CENTER, sym.chars().next().unwrap_or('?'), prop(rect.height() * 0.55), MU);
        }
    }
}

/// Logo followed by a label, as one inline widget.
pub fn icon_label(ui: &mut egui::Ui, e: Exchange, text: impl Into<egui::WidgetText>, dim: bool) -> egui::Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        icon(ui.painter(), e, r, dim);
        ui.label(text)
    }).inner
}

/// Combo box chevron (replaces egui's filled triangle).
pub fn chevron(ui: &egui::Ui, rect: Rect, _: &egui::style::WidgetVisuals, open: bool) {
    let c = rect.center();
    let (w, h) = (3.5, 2.0);
    let pts = if open { [c + egui::vec2(-w, h), c + egui::vec2(0.0, -h), c + egui::vec2(w, h)] } else { [c + egui::vec2(-w, -h), c + egui::vec2(0.0, h), c + egui::vec2(w, -h)] };
    ui.painter().add(egui::Shape::line(pts.to_vec(), egui::Stroke::new(1.4, MU)));
}

/// Secondary action as accent text (no box); underlined on hover.
pub fn link(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let r = ui.add(egui::Label::new(egui::RichText::new(text).font(prop(12.0)).color(accent())).sense(egui::Sense::click()));
    if r.hovered() { ui.painter().hline(r.rect.x_range(), r.rect.bottom(), egui::Stroke::new(1.0, accent())); }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------------------------------------------------------------- controls
// Painted controls with one look: inset fields on bg(), accent for "on", 1px LINE borders.

/// iOS-style switch; returns true when toggled.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(34.0, 19.0), egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, *on);
    let p = ui.painter();
    let track = Color32::from_rgb(0x2a, 0x31, 0x3b).lerp_to_gamma(accent(), t);
    p.rect_filled(r, 10, track);
    let x = egui::lerp((r.left() + 9.5)..=(r.right() - 9.5), t);
    p.circle_filled(egui::pos2(x, r.center().y), 7.5, Color32::WHITE);
    let clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    if clicked { *on = !*on; }
    clicked
}

/// Segmented pill group sized to its labels; returns true when the selection changed.
pub fn segment(ui: &mut egui::Ui, labels: &[&str], sel: &mut usize) -> bool {
    let gal: Vec<_> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), prop(12.0), FG)).collect();
    let w: f32 = gal.iter().map(|g| g.size().x + 22.0).sum::<f32>() + 4.0;
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::hover());
    ui.painter().rect(r, 7, bg(), egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    let mut x = r.left() + 2.0;
    let mut changed = false;
    for (i, g) in gal.into_iter().enumerate() {
        let cell = egui::Rect::from_min_size(egui::pos2(x, r.top() + 2.0), egui::vec2(g.size().x + 22.0, r.height() - 4.0));
        x = cell.right();
        let resp = ui.interact(cell, ui.id().with(("seg", labels[i])), egui::Sense::click());
        let on = *sel == i;
        if on { ui.painter().rect_filled(cell, 5, HL); ui.painter().rect_stroke(cell, 5, egui::Stroke::new(1.0, Color32::from_rgba_premultiplied(46, 46, 46, 46)), egui::StrokeKind::Inside); }
        else if resp.hovered() { ui.painter().rect_filled(cell, 5, HL.linear_multiply(0.5)); }
        let col = if on { FG } else { MU };
        let g = ui.painter().layout_no_wrap(labels[i].to_string(), prop(12.0), col);
        ui.painter().galley(cell.center() - g.size() / 2.0, g, col);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { *sel = i; changed = true; }
    }
    changed
}

/// Slider: thin track, accent fill, round knob, value text on the right.
pub fn slider(ui: &mut egui::Ui, v: &mut f32, lo: f32, hi: f32, step: f32, fmt: impl Fn(f32) -> String) -> bool {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(170.0, 20.0), egui::Sense::click_and_drag());
    let track = egui::Rect::from_min_max(egui::pos2(r.left() + 7.0, r.center().y - 2.0), egui::pos2(r.right() - 7.0, r.center().y + 2.0));
    let f = ((*v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let x = track.left() + track.width() * f;
    let p = ui.painter();
    p.rect_filled(track, 2, Color32::from_rgb(0x2a, 0x31, 0x3b));
    p.rect_filled(egui::Rect::from_min_max(track.min, egui::pos2(x, track.max.y)), 2, accent());
    p.circle(egui::pos2(x, track.center().y), 7.0, Color32::WHITE, egui::Stroke::new(2.0, accent()));
    let mut changed = false;
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.clicked()) {
        let raw = lo + ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0) * (hi - lo);
        let nv = ((raw / step).round() * step).clamp(lo, hi);
        if nv != *v { *v = nv; changed = true; }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    ui.add_sized(egui::vec2(48.0, 20.0), egui::Label::new(egui::RichText::new(fmt(*v)).font(mono(11.5)).color(FG)));
    changed
}

/// Small inset numeric field with a suffix; edits commit when the text parses.
pub fn num_field(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, v: &mut f64, decimals: usize, suffix: &str) -> bool {
    let (r, _) = ui.allocate_exact_size(egui::vec2(88.0, 24.0), egui::Sense::hover());
    let eid = ui.id().with(("num", id));
    let focused = ui.memory(|m| m.has_focus(eid));
    ui.painter().rect(r, 5, bg(), egui::Stroke::new(1.0, if focused { accent() } else { LINE }), egui::StrokeKind::Inside);
    let sw = ui.painter().text(r.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, suffix, prop(11.0), DIM).width();
    let mut text = ui.data_mut(|d| d.get_temp::<String>(eid)).filter(|_| focused).unwrap_or_else(|| format!("{:.*}", decimals, v));
    let resp = ui.put(egui::Rect::from_min_max(egui::pos2(r.left() + 6.0, r.top() + 2.0), egui::pos2(r.right() - sw - 12.0, r.bottom() - 2.0)),
        egui::TextEdit::singleline(&mut text).id(eid).frame(egui::Frame::NONE).font(mono(12.0)).horizontal_align(egui::Align::RIGHT).vertical_align(egui::Align::Center));
    let mut changed = false;
    if resp.changed() {
        if let Ok(x) = text.trim().parse::<f64>() { if x != *v { *v = x; changed = true; } }
    }
    ui.data_mut(|d| d.insert_temp(eid, text));
    changed
}

/// A settings row inside a group: title (+ muted description) on the left, control on the right.
pub fn setting_row(ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(if desc.is_empty() { 34.0 } else { 44.0 });
        // the text column stops short of the control so a long description wraps instead of
        // running under it
        let text_w = (ui.available_width() * 0.55).max(160.0);
        ui.vertical(|ui| {
            ui.set_max_width(text_w);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(egui::RichText::new(title).font(prop(12.5)).color(FG));
            if !desc.is_empty() { ui.add(egui::Label::new(egui::RichText::new(desc).font(prop(10.5)).color(DIM)).wrap()); }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// Rounded group card for settings rows; rows are separated by hairlines.
pub fn group(ui: &mut egui::Ui, title: &str, rows: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(10.0);
    ui.label(egui::RichText::new(title.to_uppercase()).font(prop(10.5)).color(DIM));
    ui.add_space(3.0);
    egui::Frame::new().fill(PANEL2).corner_radius(8).stroke(egui::Stroke::new(1.0, LINE)).inner_margin(egui::Margin::symmetric(12, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        rows(ui);
    });
}

/// Hairline between rows of a group.
pub fn row_sep(ui: &mut egui::Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, LINE));
    ui.add_space(1.0);
}

/// Text tab: muted when idle, bright with an accent underline when selected, no fill.
/// The one toggle style used for modes, timeframes, panel tabs and expiries.
pub fn tab(ui: &mut egui::Ui, text: &str, selected: bool, size: f32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(size), FG);
    let (rect, resp) = ui.allocate_exact_size(g.size() + egui::vec2(12.0, 7.0), egui::Sense::click());
    let col = if selected || resp.hovered() { FG } else { MU };
    let g = ui.painter().layout_no_wrap(text.to_string(), prop(size), col);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, col);
    if selected { ui.painter().hline(egui::Rangef::new(rect.left() + 5.0, rect.right() - 5.0), rect.bottom() - 1.0, egui::Stroke::new(2.0, accent())); }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn mono(size: f32) -> FontId { FontId::monospace(size) }
pub fn prop(size: f32) -> FontId { FontId::proportional(size) }

/// Memory-map a system font for the life of the process: only the glyph pages actually used
/// become resident (PingFang.ttc is 75 MB; reading it would keep all of it in RAM).
fn map_font(path: &std::path::Path) -> Option<&'static [u8]> {
    let f = std::fs::File::open(path).ok()?;
    // SAFETY: system font files are read-only and not modified while the app runs.
    let m = unsafe { memmap2::Mmap::map(&f) }.ok()?;
    Some(&Box::leak(Box::new(m))[..])
}

/// First PingFang.ttc under the system asset store (path contains a content hash).
fn pingfang() -> Option<&'static [u8]> {
    let root = std::path::Path::new("/System/Library/AssetsV2");
    for d in std::fs::read_dir(root).ok()?.flatten() {
        if !d.file_name().to_string_lossy().starts_with("com_apple_MobileAsset_Font") { continue; }
        for a in std::fs::read_dir(d.path()).ok()?.flatten() {
            if let Some(b) = map_font(&a.path().join("AssetData/PingFang.ttc")) { return Some(b); }
        }
    }
    None
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8], index: u32, families: &[FontFamily]| {
        let mut fd = FontData::from_static(bytes);
        fd.index = index;
        fonts.font_data.insert(name.into(), Arc::new(fd));
        for f in families { fonts.families.entry(f.clone()).or_default().push(name.into()); }
    };
    // Order matters: the first font with a glyph wins. SF first, CJK as fallback.
    let mut primary: Vec<String> = vec![];
    let sys = |p: &str| map_font(std::path::Path::new(p));
    if let Some(b) = sys("/System/Library/Fonts/SFNS.ttf") { add("sf", b, 0, &[]); primary.push("sf".into()); }
    let mut mono_first: Vec<String> = vec![];
    if let Some(b) = sys("/System/Library/Fonts/SFNSMono.ttf") { add("sfmono", b, 0, &[]); mono_first.push("sfmono".into()); }
    // PingFang SC Regular is face 3 of PingFang.ttc; Hiragino Sans GB is the fallback
    let cjk = if let Some(b) = pingfang() { add("cjk", b, 3, &[]); true }
        else if let Some(b) = sys("/System/Library/Fonts/Hiragino Sans GB.ttc") { add("cjk", b, 0, &[]); true } else { false };
    let p = fonts.families.entry(FontFamily::Proportional).or_default();
    for (i, n) in primary.iter().enumerate() { p.insert(i, n.clone()); }
    if cjk { p.insert(primary.len(), "cjk".into()); }
    let m = fonts.families.entry(FontFamily::Monospace).or_default();
    for (i, n) in mono_first.iter().enumerate() { m.insert(i, n.clone()); }
    if cjk { m.push("cjk".into()); }
    ctx.set_fonts(fonts);

    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (TextStyle::Heading, prop(15.0)),
        (TextStyle::Body, prop(12.5)),
        (TextStyle::Button, prop(12.5)),
        (TextStyle::Small, prop(11.0)),
        (TextStyle::Monospace, mono(12.0)),
    ].into();
    style.spacing.item_spacing = egui::vec2(6.0, 3.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.interact_size.y = 21.0;
    style.spacing.menu_margin = egui::Margin::same(6);
    // thin scrollbars that float over content instead of eating a gutter
    style.spacing.scroll = egui::style::ScrollStyle::floating();
    style.animation_time = 0.12;
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.override_text_color = Some(FG);
    v.panel_fill = panel();
    v.window_fill = PANEL2;
    v.extreme_bg_color = bg();
    v.faint_bg_color = PANEL2;
    v.selection.bg_fill = accent().gamma_multiply(0.35);
    v.selection.stroke.color = FG;
    v.window_stroke.color = LINE;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = 5.into();
        w.expansion = 0.0;
    }
    v.window_corner_radius = 10.into();
    v.menu_corner_radius = 8.into();
    // soft, wide shadows: popups and dialogs read as layers, not boxes
    v.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(140) };
    v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) };
    v.window_stroke = egui::Stroke::new(1.0, LINE);
    v.widgets.open.weak_bg_fill = HL;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgba_premultiplied(46, 46, 46, 46));
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent());
    v.striped = false;
    v.widgets.noninteractive.bg_stroke.color = LINE;
    v.widgets.noninteractive.fg_stroke.color = MU;
    v.widgets.inactive.weak_bg_fill = PANEL2;
    v.widgets.inactive.bg_fill = PANEL2;
    // a hairline so checkboxes and fields stay visible on PANEL2 surfaces (dialogs)
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    v.widgets.hovered.weak_bg_fill = HL;
    v.widgets.hovered.bg_fill = HL;
    v.widgets.active.weak_bg_fill = HL;
    // always dark, whatever the system appearance (a light system theme would otherwise swap
    // in egui's light widget visuals under our dark panels)
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_style_of(egui::Theme::Dark, style);
    load_icons(ctx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_palette_pngs() {
        // 1x1 indexed PNG whose only palette entry is pure red
        let mut buf = vec![];
        {
            let mut e = png::Encoder::new(&mut buf, 1, 1);
            e.set_color(png::ColorType::Indexed);
            e.set_depth(png::BitDepth::Eight);
            e.set_palette(vec![255u8, 0, 0]);
            e.write_header().unwrap().write_image_data(&[0]).unwrap();
        }
        let img = super::decode_png(&buf).expect("indexed png decodes");
        assert_eq!(img.pixels[0], eframe::egui::Color32::from_rgb(255, 0, 0));
    }
}
