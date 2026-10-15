//! Header pieces for the egui app: a port of the SwiftUI host's TopBar (composite price with flash,
//! the order venue's mark / index / funding countdown, 24h stats, open interest, cross-venue extras)
//! and MarketClock (stock market sessions). Stats read the app's state document ("header").
use super::{fmt_dp, fmt_px, kit, l, theme::*};
use depth::Market;
use eframe::egui::{self, pos2, vec2, Align2, Color32, Rect, Sense, Ui};
use serde_json::Value;
use std::time::{Duration, Instant};

const DAY: i64 = 86_400_000;

/// 12.3K / 4.56M / 7.89B (the Swift `Fmt.big`).
pub fn big(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 { format!("{:.2}B", v / 1e9) } else if a >= 1e6 { format!("{:.2}M", v / 1e6) } else if a >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.2}") }
}

/// Sign-prefixed with `d` decimals; anything that rounds to zero prints as an unsigned zero.
pub fn signed(v: f64, d: usize) -> String {
    if v.abs() < 0.5 * 10f64.powi(-(d as i32)) { return fmt_dp(0.0, d); }
    format!("{}{}", if v > 0.0 { "+" } else { "" }, fmt_dp(v, d))
}

/// 1d 02:03:04 / 02:03:04
pub fn left(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let hms = format!("{:02}:{:02}:{:02}", s / 3600 % 24, s % 3600 / 60, s % 60);
    if s >= 86_400 { format!("{}d {hms}", s / 86_400) } else { hms }
}

// ---- calendar (proleptic Gregorian, days since 1970-01-01) ----

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}
fn year_of(days: i64) -> i64 {
    // first guess from the mean year length, then correct across the boundary
    let mut y = 1970 + (days as f64 / 365.2425).floor() as i64;
    if days_from_civil(y, 1, 1) > days { y -= 1; } else if days_from_civil(y + 1, 1, 1) <= days { y += 1; }
    y
}
/// 0 = Sunday
fn weekday(days: i64) -> i64 { (days + 4).rem_euclid(7) }
fn nth_sunday(y: i64, m: i64, n: i64) -> i64 { let f = days_from_civil(y, m, 1); f + (7 - weekday(f)) % 7 + 7 * (n - 1) }
fn last_sunday(y: i64, m: i64) -> i64 { let l = days_from_civil(y, m + 1, 1) - 1; l - weekday(l) }

#[derive(Clone, Copy)]
enum Dst { No, Us, Eu }

/// Regular trading sessions of the big stock markets, in each exchange's local time.
/// ponytail: weekdays only, public holidays and half days are not known; add a holiday table if it matters.
pub struct StockMarket { pub name: &'static str, pub short: &'static str, std_min: i64, dst: Dst, sessions: &'static [(i64, i64)] }

pub const MARKETS: [StockMarket; 6] = [
    StockMarket { name: "New York", short: "NYSE", std_min: -300, dst: Dst::Us, sessions: &[(570, 960)] },
    StockMarket { name: "London", short: "LSE", std_min: 0, dst: Dst::Eu, sessions: &[(480, 990)] },
    StockMarket { name: "Frankfurt", short: "Xetra", std_min: 60, dst: Dst::Eu, sessions: &[(540, 1050)] },
    StockMarket { name: "Tokyo", short: "TSE", std_min: 540, dst: Dst::No, sessions: &[(540, 690), (750, 930)] },
    StockMarket { name: "Hong Kong", short: "HKEX", std_min: 480, dst: Dst::No, sessions: &[(570, 720), (780, 960)] },
    StockMarket { name: "Shanghai", short: "SSE", std_min: 480, dst: Dst::No, sessions: &[(570, 690), (780, 900)] },
];

impl StockMarket {
    /// UTC offset in ms at `utc` (US: 2nd Sunday of March to 1st Sunday of November at 02:00 local;
    /// EU: last Sunday of March to last Sunday of October at 01:00 UTC).
    fn offset_ms(&self, utc: i64) -> i64 {
        let std = self.std_min * 60_000;
        let y = year_of(utc.div_euclid(DAY));
        let on = match self.dst {
            Dst::No => false,
            Dst::Us => (nth_sunday(y, 3, 2) * DAY + 7_200_000 - std..nth_sunday(y, 11, 1) * DAY + 7_200_000 - std - 3_600_000).contains(&utc),
            Dst::Eu => (last_sunday(y, 3) * DAY + 3_600_000..last_sunday(y, 10) * DAY + 3_600_000).contains(&utc),
        };
        std + if on { 3_600_000 } else { 0 }
    }

    /// Open now (and when that session ends, UTC ms), or closed and when the next session starts.
    pub fn status(&self, now: i64) -> (bool, i64) {
        let off = self.offset_ms(now);
        let today = (now + off).div_euclid(DAY);
        for day in today..today + 8 {
            if matches!(weekday(day), 0 | 6) { continue; }
            for &(a, b) in self.sessions {
                // local wall time -> UTC with the offset in force then (a DST switch may lie between)
                let utc = |min: i64| { let lt = day * DAY + min * 60_000; lt - self.offset_ms(lt - off) };
                let (s, e) = (utc(a), utc(b));
                if now < s { return (false, s); }
                if now < e { return (true, e); }
            }
        }
        (false, now)
    }
}

/// Header state: last composite price with its tick direction and flash.
#[derive(Default)]
pub struct TopBar { last: Option<f64>, dir: i8, flash: Option<Instant>, panic: (i64, Option<depth::agg::Panic>) }

const FLASH: f32 = 0.45;

impl TopBar {
    /// Composite index price (22 display semibold) with a direction arrow, flashing on every
    /// change; fixed width so a tick never shifts the rest of the header.
    pub fn price(&mut self, ui: &mut Ui, price: Option<f64>) {
        if let (Some(o), Some(n)) = (self.last, price) {
            if n != o { self.dir = if n > o { 1 } else { -1 }; self.flash = Some(Instant::now()); }
        }
        if price.is_some() { self.last = price; }
        let col = match self.dir { 1 => up(), -1 => dn(), _ => fg_strong() };
        let (r, resp) = ui.allocate_exact_size(vec2(170.0, 30.0), Sense::hover());
        let p = ui.painter();
        let g = p.layout_no_wrap(price.map(fmt_px).unwrap_or("–".into()), display_s(22.0), col);
        let w = g.size().x;
        if let Some(t) = self.flash {
            let k = 1.0 - t.elapsed().as_secs_f32() / FLASH;
            if k > 0.0 {
                p.rect_filled(Rect::from_min_size(r.min, vec2(w + 30.0, r.height())), kit::R, col.gamma_multiply(0.18 * k));
                // bounded fade: ~30 fps for at most FLASH seconds after a tick, never every frame
                ui.ctx().request_repaint_after(Duration::from_millis(33));
            } else { self.flash = None; }
        }
        let tx = r.left() + 6.0;
        p.galley(pos2(tx, r.center().y - g.size().y / 2.0), g, col);
        if self.dir != 0 {
            // filled triangle, as the design's 14 px arrow glyph
            let (x, y) = (tx + w + 11.0, r.center().y + 1.0);
            let pts = if self.dir > 0 { vec![pos2(x - 4.5, y + 3.5), pos2(x + 4.5, y + 3.5), pos2(x, y - 4.0)] } else { vec![pos2(x - 4.5, y - 3.5), pos2(x + 4.5, y - 3.5), pos2(x, y + 4.0)] };
            p.add(egui::Shape::convex_polygon(pts, col, egui::Stroke::NONE));
        }
        resp.on_hover_text(l("Composite index (volume-weighted venue mids); reference only, orders use the venue's own book"));
    }

    /// Market strip: the order venue's mark / index / funding with countdown (perp), 24h change,
    /// range and volume, open interest, then the cross-venue extras.
    pub fn stats(ui: &mut Ui, st: &Value, mode: Market) {
        let h = &st["header"];
        let base = st["base"].as_str().unwrap_or_default();
        let venue = h["venue"].as_str().unwrap_or_default();
        let f = |k: &str| h[k].as_f64();
        let px = |k: &str| f(k).map(fmt_px).unwrap_or("–".into());
        let sign = |v: Option<f64>| match v { Some(x) if x > 0.0 => up(), Some(x) if x < 0.0 => dn(), _ => fg() };
        let perp = mode == Market::Perp;
        ui.spacing_mut().item_spacing.x = 28.0;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        if perp {
            stat(ui, l("Mark"), &[(px("mark"), fg())]).on_hover_text(format!("{venue} {}", l("mark price")));
            stat(ui, l("Index"), &[(px("index"), fg())]).on_hover_text(format!("{venue} {}", l("index price")));
            let iv = f("funding_interval_h").map(|x| format!(" ({x}h)")).unwrap_or_default();
            let rate = f("funding_rate");
            let cd = f("next_funding_ms").map(|n| left(n as i64 - depth::now_ms())).unwrap_or("–".into());
            let rc = match rate { Some(x) if x >= 0.0 => WARN, Some(_) => dn(), None => fg() };
            stat(ui, &format!("{}{iv} / {}", l("Funding"), l("Countdown")),
                &[(rate.map(|x| format!("{:.4}%", x * 100.0)).unwrap_or("–".into()), rc), ("·".into(), dim()), (cd, fg())])
                .on_hover_text(format!("{venue} · {}", l("rate per settlement")));
            // the countdown ticks once a second
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        stat(ui, l("24h Change"), &[(f("chg24").map(|x| format!("{x:+.2}%")).unwrap_or("–".into()), sign(f("chg24")))]);
        stat(ui, l("24h High"), &[(px("high24"), fg())]);
        stat(ui, l("24h Low"), &[(px("low24"), fg())]);
        let vtip = l("Summed across Binance, Bybit, OKX and Hyperliquid");
        stat(ui, &format!("{} ({base})", l("24h Vol")), &[(f("vol24_base").map(big).unwrap_or("–".into()), fg())]).on_hover_text(vtip);
        stat(ui, &format!("{} (USD)", l("24h Vol")), &[(f("vol24_usd").map(big).unwrap_or("–".into()), fg())]).on_hover_text(vtip);
        if perp {
            let mut oi = vec![(f("oi_usd").map(|x| format!("${}", big(x))).unwrap_or("–".into()), fg())];
            if let Some(x) = f("oi") { oi.push((format!("{} {base}", big(x)), mu())); }
            stat(ui, l("Open Interest"), &oi).on_hover_text(l("Sum of every connected perp venue"));
            let fp = f("funding_pred_bph");
            let apr = fp.map(|x| format!(" · {} {:+.2}%", l("APR"), x / 1e4 * 24.0 * 365.0 * 100.0)).unwrap_or_default();
            stat(ui, l("Pred. funding (all)"), &[(fp.map(|x| format!("{x:+.4} bp/h")).unwrap_or("–".into()), sign(fp))])
                .on_hover_text(format!("{}{apr}", l("Open-interest-weighted across venues, per hour")));
            stat(ui, l("Basis"), &[(f("basis_bps").map(|x| format!("{x:+.2} bp")).unwrap_or("–".into()), sign(f("basis_bps")))]);
        }
        if mode != Market::Option {
            stat(ui, l("CVD"), &[(f("cvd").map(|x| signed(x, 1)).unwrap_or("–".into()), sign(f("cvd")))]);
        }
    }

    /// Intraday panic index (own, cross-venue perps, recomputed every 5 s) next to the market-wide
    /// daily Fear & Greed from alternative.me (inverted so both read high = fear).
    pub fn panic(&mut self, ui: &mut Ui, a: &depth::agg::Agg, fng: Option<(f64, String)>) {
        let slot = depth::now_ms() / 5_000;
        if self.panic.0 != slot {
            let bars: Vec<depth::agg::Bar> = a.series.get(&(None, Market::Perp)).map(|s| s.bars.iter().cloned().collect()).unwrap_or_default();
            self.panic = (slot, depth::agg::panic(&bars));
        }
        let fear = |x: f64| if x >= 75.0 { dn() } else if x <= 25.0 { up() } else { fg() };
        let mut parts: Vec<(String, egui::FontId, Color32)> = vec![(l("Panic").into(), prop_m(12.0), mu())];
        let score = self.panic.1.as_ref().map(|p| p.score);
        parts.push((score.map(|x| format!("{x:.0}")).unwrap_or("–".into()), mono_m(12.0), score.map(fear).unwrap_or(dim())));
        if let Some((g, _)) = &fng {
            parts.push((l("Market").into(), prop_m(12.0), mu()));
            parts.push((format!("{:.0}", 100.0 - g), mono_m(12.0), fear(100.0 - g)));
        }
        let gs: Vec<_> = parts.into_iter().map(|(t, f, c)| ui.painter().layout_no_wrap(t, f, c)).collect();
        let w: f32 = gs.iter().map(|g| g.size().x).sum::<f32>() + 5.0 * (gs.len() - 1) as f32 + 4.0 * (gs.len() / 2 - 1) as f32;
        let (r, resp) = ui.allocate_exact_size(vec2(w + 16.0, 30.0), Sense::hover());
        let mut x = r.left() + 8.0;
        for (i, g) in gs.into_iter().enumerate() {
            let gw = g.size().x;
            ui.painter().galley(pos2(x, r.center().y - g.size().y / 2.0), g, fg());
            x += gw + 5.0 + if i % 2 == 1 { 4.0 } else { 0.0 };
        }
        let p = self.panic.1.as_ref();
        resp.on_hover_ui(|ui| {
            ui.set_max_width(300.0);
            ui.label(egui::RichText::new(l("Panic index, 0 calm to 100 panic")).font(prop_s(12.0)).color(fg_strong()));
            ui.label(egui::RichText::new(l("Cross-venue perps ranked against every minute of the loaded window (about 16 h at start, up to 7 days): it measures fear relative to the recent past, not an absolute level.")).font(prop(11.5)).color(mu()));
            ui.add_space(4.0);
            for (k, v) in p.map(|p| p.parts.as_slice()).unwrap_or_default() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(l(k)).font(prop(12.0)).color(fg()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(egui::RichText::new(format!("{:.0}", v * 100.0)).font(mono_m(12.0)).color(fear(v * 100.0))));
                });
            }
            if p.is_none() { ui.label(egui::RichText::new(l("Waiting for an hour of perp history")).font(prop(12.0)).color(dim())); }
            ui.add_space(4.0);
            let fg_txt = match &fng { Some((g, c)) => format!("{} {:.0} ({c}) · {}", l("Market (alternative.me, daily):"), 100.0 - g, l("Fear & Greed inverted")), None => l("Market index unavailable").into() };
            ui.label(egui::RichText::new(fg_txt).font(prop(11.5)).color(mu()));
        });
    }

    /// Stock markets open right now.
    pub fn open_now(ui: &mut Ui) {
        let now = depth::now_ms();
        let open: Vec<&str> = MARKETS.iter().filter(|m| m.status(now).0).map(|m| m.short).collect();
        let txt = if open.is_empty() { l("All closed").to_string() } else { open.join(" · ") };
        let lg = ui.painter().layout_no_wrap(l("Open").into(), prop_m(12.0), mu());
        let tg = ui.painter().layout_no_wrap(txt, prop_m(12.0), if open.is_empty() { dim() } else { fg() });
        let (r, resp) = ui.allocate_exact_size(vec2(lg.size().x + tg.size().x + 30.0, 30.0), Sense::hover());
        let p = ui.painter();
        let cy = r.center().y;
        p.circle_filled(pos2(r.left() + 10.0, cy), 3.0, if open.is_empty() { dim() } else { up() });
        let x = r.left() + 18.0;
        p.galley(pos2(x, cy - lg.size().y / 2.0), lg.clone(), mu());
        p.galley(pos2(x + lg.size().x + 5.0, cy - tg.size().y / 2.0), tg, fg());
        resp.on_hover_text(l("Stock markets in their regular session now"));
    }

    /// Thin vertical divider between header groups.
    pub fn divider(ui: &mut Ui) {
        let (r, _) = ui.allocate_exact_size(vec2(9.0, 30.0), Sense::hover());
        ui.painter().vline(r.center().x, r.shrink2(vec2(0.0, 8.0)).y_range(), egui::Stroke::new(1.0, line()));
    }

    /// The next stock market to open with a countdown; click for every market's session.
    pub fn clock(ui: &mut Ui) {
        let now = depth::now_ms();
        let next = MARKETS.iter().map(|m| (m, m.status(now))).filter(|x| !x.1 .0).min_by_key(|x| x.1 .1);
        let (label, cd) = match next { Some((m, (_, t))) => (format!("{} {}", m.short, l("opens")), left(t - now)), None => (l("Markets").to_string(), String::new()) };
        let lg = ui.painter().layout_no_wrap(label, prop_m(12.0), mu());
        let cg = ui.painter().layout_no_wrap(cd, mono_m(12.0), fg());
        let gap = if cg.size().x > 0.0 { 5.0 } else { 0.0 };
        let (r, resp) = ui.allocate_exact_size(vec2(lg.size().x + gap + cg.size().x + 16.0, 30.0), Sense::click());
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || open, 0.1);
        let p = ui.painter();
        if hv > 0.0 { p.rect_filled(r, kit::R, hl().gamma_multiply(0.7 * hv)); }
        let x = r.left() + 8.0;
        p.galley(pos2(x, r.center().y - lg.size().y / 2.0), lg.clone(), lerp(mu(), fg(), hv));
        p.galley(pos2(x + lg.size().x + gap, r.center().y - cg.size().y / 2.0), cg, fg());
        let resp = resp.on_hover_text(l("Stock market sessions")).on_hover_cursor(egui::CursorIcon::PointingHand);
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        egui::Popup::from_toggle_button_response(&resp)
            .frame(popup_frame(ui, 6))
            .align(egui::RectAlign::BOTTOM_END)
            .show(|ui| {
                ui.set_width(290.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                for m in MARKETS.iter() {
                    let (open, until) = m.status(now);
                    let (r, _) = ui.allocate_exact_size(vec2(290.0, 36.0), Sense::hover());
                    let p = ui.painter();
                    let cy = r.center().y;
                    p.circle_filled(pos2(r.left() + 10.0, cy), 3.0, if open { up() } else { dim() });
                    p.text(pos2(r.left() + 22.0, cy - 7.0), Align2::LEFT_CENTER, l(m.name), prop_m(12.5), fg());
                    p.text(pos2(r.left() + 22.0, cy + 8.0), Align2::LEFT_CENTER, m.short, prop(11.0), dim());
                    p.text(pos2(r.left() + 140.0, cy), Align2::LEFT_CENTER, if open { l("Closes in") } else { l("Opens in") }, prop(12.0), mu());
                    p.text(pos2(r.right() - 10.0, cy), Align2::RIGHT_CENTER, left(until - now), mono_m(12.5), if open { fg_strong() } else { mu() });
                }
            });
    }
}

/// One header stat: overline caption (10.5 semibold caps, dim) over 13 medium tabular values.
fn stat(ui: &mut Ui, caption: &str, values: &[(String, Color32)]) -> egui::Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(5.0, 3.0);
        ui.label(egui::RichText::new(caption.to_uppercase()).font(prop_s(10.5)).color(dim()).extra_letter_spacing(0.6));
        ui.horizontal(|ui| {
            for (v, c) in values { ui.label(egui::RichText::new(v).font(mono_m(13.0)).color(*c)); }
        });
    }).response
}

/// Popup / menu surface: radius 6, opaque popup fill, hairline, soft shadow.
pub fn popup_frame(ui: &Ui, margin: i8) -> egui::Frame {
    egui::Frame::new().fill(popup_bg()).stroke(egui::Stroke::new(1.0, line())).corner_radius(6).inner_margin(margin).shadow(ui.visuals().popup_shadow)
}

/// Pill segmented group sized to its labels (title bar markets, picker Favorites / All): a
/// 3 px well, `h` high cells, selected hl + semibold; the thumb slides. Returns the clicked index.
pub fn pills(ui: &mut Ui, id: &str, labels: &[&str], sel: usize, h: f32, size: f32) -> Option<usize> {
    let (pad, gap) = (3.0, 2.0);
    let w: Vec<f32> = labels.iter().map(|t| ui.painter().layout_no_wrap(t.to_string(), prop_s(size), fg()).size().x + 24.0).collect();
    let total = w.iter().sum::<f32>() + gap * (w.len().max(1) - 1) as f32 + 2.0 * pad;
    let (r, _) = ui.allocate_exact_size(vec2(total, h + 2.0 * pad), Sense::hover());
    let en = ui.is_enabled();
    ui.painter().rect_filled(r, kit::R + 1, panel2());
    let mut xs = Vec::with_capacity(w.len());
    let mut x = r.left() + pad;
    for wi in &w { xs.push(x); x += wi + gap; }
    let id = ui.id().with(("pills", id));
    if let (Some(&sx), Some(&sw)) = (xs.get(sel), w.get(sel)) {
        let tx = ui.ctx().animate_value_with_time(id.with("x"), sx, 0.15);
        let tw = ui.ctx().animate_value_with_time(id.with("w"), sw, 0.15);
        let thumb = Rect::from_min_size(pos2(tx, r.top() + pad), vec2(tw, h));
        ui.painter().rect_filled(thumb.translate(vec2(0.0, 1.0)), kit::R, Color32::from_black_alpha(if light() { 18 } else { 60 }));
        ui.painter().rect_filled(thumb, kit::R, if light() { Color32::WHITE } else { hl() });
    }
    let mut out = None;
    for (i, t) in labels.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(xs[i], r.top() + pad), vec2(w[i], h));
        let resp = ui.interact(cell, id.with(i), Sense::click());
        let on = i == sel;
        let k = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.15);
        let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
        let col = lerp(mu(), fg_strong(), k.max(hv * 0.7));
        ui.painter().text(cell.center(), Align2::CENTER_CENTER, *t, if on { prop_s(size) } else { prop_m(size) }, if en { col } else { col.gamma_multiply(0.5) });
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on { out = Some(i); }
    }
    out
}

/// Settings gear (16 px): ring with eight teeth and a hub.
pub fn gear(p: &egui::Painter, r: Rect, col: Color32) {
    let c = r.center();
    p.circle_stroke(c, 4.6, egui::Stroke::new(1.4, col));
    p.circle_stroke(c, 1.7, egui::Stroke::new(1.3, col));
    for k in 0..8 {
        let d = egui::Vec2::angled(k as f32 * std::f32::consts::FRAC_PI_4);
        p.line_segment([c + d * 4.6, c + d * 7.0], egui::Stroke::new(2.1, col));
    }
}

/// Order panel toggle glyph: a window with its right column filled when the panel is shown.
pub fn panel_glyph(p: &egui::Painter, r: Rect, col: Color32, on: bool) {
    let b = Rect::from_center_size(r.center(), vec2(16.0, 13.0));
    p.rect_stroke(b, 2.5, egui::Stroke::new(1.4, col), egui::StrokeKind::Inside);
    let side = Rect::from_min_max(pos2(b.right() - 6.0, b.top() + 2.0), pos2(b.right() - 2.0, b.bottom() - 2.0));
    if on { p.rect_filled(side, 1, col.gamma_multiply(0.5)); } else { p.vline(side.left(), side.y_range(), egui::Stroke::new(1.2, col)); }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn utc(y: i64, m: i64, d: i64, h: i64, min: i64) -> i64 { days_from_civil(y, m, d) * DAY + (h * 60 + min) * 60_000 }

    #[test]
    fn market_sessions() {
        assert_eq!(weekday(days_from_civil(2026, 10, 14)), 3);
        let ny = &MARKETS[0];
        // EDT: 10:00 New York on a Wednesday, open until 16:00 (20:00 UTC)
        assert_eq!(ny.status(utc(2026, 10, 14, 14, 0)), (true, utc(2026, 10, 14, 20, 0)));
        // EST: 09:00 New York, opens 09:30 (14:30 UTC)
        assert_eq!(ny.status(utc(2026, 1, 15, 14, 0)), (false, utc(2026, 1, 15, 14, 30)));
        // Friday after the close: Monday's open
        assert_eq!(ny.status(utc(2026, 10, 16, 21, 0)), (false, utc(2026, 10, 19, 13, 30)));
        // Tokyo lunch break: 12:00 JST, afternoon session at 12:30 (03:30 UTC)
        assert_eq!(MARKETS[3].status(utc(2026, 10, 14, 3, 0)), (false, utc(2026, 10, 14, 3, 30)));
        // London BST in October, GMT after the last Sunday (25 Oct 2026)
        assert_eq!(MARKETS[1].status(utc(2026, 10, 14, 6, 0)), (false, utc(2026, 10, 14, 7, 0)));
        assert_eq!(MARKETS[1].status(utc(2026, 10, 26, 6, 0)), (false, utc(2026, 10, 26, 8, 0)));
        assert_eq!(left(DAY + 3_723_000), "1d 01:02:03");
        assert_eq!(signed(-0.04, 1), "0.0");
    }
}
