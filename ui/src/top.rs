//! Header pieces for the egui app: a port of the SwiftUI host's TopBar (composite price with flash,
//! the order venue's mark / index / funding countdown, 24h stats, open interest, cross-venue extras)
//! and MarketClock (stock market sessions). Stats read the app's state document ("header").
use super::{fmt_dp, fmt_px, l, theme::*};
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
pub struct TopBar { last: Option<f64>, dir: i8, flash: Option<Instant> }

const FLASH: f32 = 0.45;

impl TopBar {
    /// Composite index price with a direction arrow, flashing on every change; fixed width so a
    /// tick never shifts the rest of the header.
    pub fn price(&mut self, ui: &mut Ui, price: Option<f64>) {
        if let (Some(o), Some(n)) = (self.last, price) {
            if n != o { self.dir = if n > o { 1 } else { -1 }; self.flash = Some(Instant::now()); }
        }
        if price.is_some() { self.last = price; }
        let col = match self.dir { 1 => up(), -1 => dn(), _ => fg() };
        let (r, resp) = ui.allocate_exact_size(vec2(150.0, 28.0), Sense::hover());
        let p = ui.painter();
        if let Some(t) = self.flash {
            let k = 1.0 - t.elapsed().as_secs_f32() / FLASH;
            if k > 0.0 {
                let g = p.layout_no_wrap(price.map(fmt_px).unwrap_or("–".into()), display(20.0), col);
                p.rect_filled(Rect::from_min_size(r.min, vec2(g.size().x + 26.0, r.height())), 6, col.gamma_multiply(0.22 * k));
                // bounded fade: ~30 fps for at most FLASH seconds after a tick, never every frame
                ui.ctx().request_repaint_after(Duration::from_millis(33));
            } else { self.flash = None; }
        }
        let g = p.layout_no_wrap(price.map(fmt_px).unwrap_or("–".into()), display(20.0), col);
        let tx = r.left() + 6.0;
        let w = g.size().x;
        p.galley(pos2(tx, r.center().y - g.size().y / 2.0), g, col);
        if self.dir != 0 {
            // arrow: shaft and head
            let x = tx + w + 9.0;
            let (y0, y1) = if self.dir > 0 { (r.center().y + 5.0, r.center().y - 5.0) } else { (r.center().y - 5.0, r.center().y + 5.0) };
            let s = egui::Stroke::new(1.8, col);
            p.line_segment([pos2(x, y0), pos2(x, y1)], s);
            let h = if self.dir > 0 { 3.5 } else { -3.5 };
            p.line_segment([pos2(x - 3.5, y1 + h), pos2(x, y1)], s);
            p.line_segment([pos2(x + 3.5, y1 + h), pos2(x, y1)], s);
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
        ui.spacing_mut().item_spacing.x = 24.0;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        if perp {
            stat(ui, l("Mark"), px("mark"), fg(), None).on_hover_text(format!("{venue} {}", l("mark price")));
            stat(ui, l("Index"), px("index"), fg(), None).on_hover_text(format!("{venue} {}", l("index price")));
            let iv = f("funding_interval_h").map(|x| format!(" ({x}h)")).unwrap_or_default();
            let rate = f("funding_rate");
            let cd = f("next_funding_ms").map(|n| left(n as i64 - depth::now_ms())).unwrap_or("–".into());
            let r = ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 1.0);
                ui.label(egui::RichText::new(format!("{}{iv} / {}", l("Funding"), l("Countdown"))).font(prop(10.5)).color(dim()));
                ui.horizontal(|ui| {
                    let rc = match rate { Some(x) if x >= 0.0 => WARN, Some(_) => dn(), None => fg() };
                    ui.label(egui::RichText::new(rate.map(|x| format!("{:.4}%", x * 100.0)).unwrap_or("–".into())).font(mono(12.0)).color(rc));
                    ui.label(egui::RichText::new("/").font(mono(12.0)).color(dim()));
                    ui.label(egui::RichText::new(cd).font(mono(12.0)).color(fg()));
                });
            }).response;
            r.on_hover_text(format!("{venue} · {}", l("rate per settlement")));
            // the countdown ticks once a second
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        stat(ui, l("24h Change"), f("chg24").map(|x| format!("{x:+.2}%")).unwrap_or("–".into()), sign(f("chg24")), None);
        stat(ui, l("24h High"), px("high24"), fg(), None);
        stat(ui, l("24h Low"), px("low24"), fg(), None);
        let vtip = l("Summed across Binance, Bybit, OKX and Hyperliquid");
        stat(ui, &format!("{} ({base})", l("24h Vol")), f("vol24_base").map(big).unwrap_or("–".into()), fg(), None).on_hover_text(vtip);
        stat(ui, &format!("{} (USD)", l("24h Vol")), f("vol24_usd").map(big).unwrap_or("–".into()), fg(), None).on_hover_text(vtip);
        if perp {
            stat(ui, l("Open Interest"), f("oi_usd").map(|x| format!("${}", big(x))).unwrap_or("–".into()), fg(), f("oi").map(|x| format!("{} {base}", big(x))))
                .on_hover_text(l("Sum of every connected perp venue"));
            let fp = f("funding_pred_bph");
            let apr = fp.map(|x| format!(" · {} {:+.2}%", l("APR"), x / 1e4 * 24.0 * 365.0 * 100.0)).unwrap_or_default();
            stat(ui, l("Pred. funding (all)"), fp.map(|x| format!("{x:+.4} bp/h")).unwrap_or("–".into()), sign(fp), None)
                .on_hover_text(format!("{}{apr}", l("Open-interest-weighted across venues, per hour")));
            stat(ui, l("Basis"), f("basis_bps").map(|x| format!("{x:+.2} bp")).unwrap_or("–".into()), sign(f("basis_bps")), None);
        }
        if mode != Market::Option {
            stat(ui, l("CVD"), f("cvd").map(|x| signed(x, 1)).unwrap_or("–".into()), sign(f("cvd")), None);
        }
    }

    /// The next stock market to open with a countdown; click for every market's session.
    pub fn clock(ui: &mut Ui) {
        let now = depth::now_ms();
        let next = MARKETS.iter().map(|m| (m, m.status(now))).filter(|x| !x.1 .0).min_by_key(|x| x.1 .1);
        let text = match next { Some((m, (_, t))) => format!("{} {} {}", m.short, l("opens"), left(t - now)), None => l("Markets").to_string() };
        let g = ui.painter().layout_no_wrap(text, mono(11.5), mu());
        let (r, resp) = ui.allocate_exact_size(g.size() + vec2(16.0, 10.0), Sense::click());
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        if resp.hovered() || open { ui.painter().rect_filled(r, 5, hl()); }
        let col = if resp.hovered() || open { fg() } else { mu() };
        ui.painter().galley(r.center() - g.size() / 2.0, g, col);
        let resp = resp.on_hover_text(l("Stock market sessions")).on_hover_cursor(egui::CursorIcon::PointingHand);
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        egui::Popup::from_toggle_button_response(&resp)
            .frame(egui::Frame::new().fill(popup_bg()).stroke(egui::Stroke::new(1.0, line())).corner_radius(8).inner_margin(12).shadow(ui.visuals().popup_shadow))
            .align(egui::RectAlign::BOTTOM_END)
            .show(|ui| {
                ui.set_width(290.0);
                for (i, m) in MARKETS.iter().enumerate() {
                    if i > 0 { ui.add_space(2.0); }
                    let (open, until) = m.status(now);
                    let (r, _) = ui.allocate_exact_size(vec2(290.0, 34.0), Sense::hover());
                    let p = ui.painter();
                    let cy = r.center().y;
                    p.circle_filled(pos2(r.left() + 4.0, cy), 3.5, if open { up() } else { dim() });
                    p.text(pos2(r.left() + 16.0, cy - 7.0), Align2::LEFT_CENTER, l(m.name), prop(12.5), fg());
                    p.text(pos2(r.left() + 16.0, cy + 8.0), Align2::LEFT_CENTER, m.short, prop(10.5), dim());
                    p.text(pos2(r.left() + 138.0, cy), Align2::LEFT_CENTER, if open { l("Closes in") } else { l("Opens in") }, prop(11.5), mu());
                    p.text(pos2(r.right(), cy), Align2::RIGHT_CENTER, left(until - now), mono(12.0), if open { fg() } else { mu() });
                }
            });
    }
}

/// One header stat: caption over value (and an optional muted extra).
fn stat(ui: &mut Ui, caption: &str, value: String, col: Color32, extra: Option<String>) -> egui::Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(5.0, 1.0);
        ui.label(egui::RichText::new(caption).font(prop(10.5)).color(dim()));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(value).font(mono(12.0)).color(col));
            if let Some(x) = extra { ui.label(egui::RichText::new(x).font(mono(12.0)).color(mu())); }
        });
    }).response
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
