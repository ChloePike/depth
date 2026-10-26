//! Venues and Quant pages of the book column. Venues polls the `venue_share` op once a second;
//! Quant reads the state document's "quant" object.
use super::{book::{overline_at, seg_fit}, fmt_px, kit, l, lf, theme::*, top::big};
use crate::order::Call;
use depth::Exchange;
use eframe::egui::{self, pos2, vec2, Align2, Color32, Rect, Sense, Stroke, Ui};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const ROW: f32 = 24.0;
const WINDOWS: [(u64, &str); 3] = [(5, "5m"), (60, "1h"), (1440, "24h")];

pub struct VenuePages {
    /// volume window in minutes
    pub window: u64,
    reply: Value,
    /// last poll: when, for which window
    polled: Option<(Instant, u64)>,
}

impl Default for VenuePages { fn default() -> Self { VenuePages { window: 60, reply: Value::Null, polled: None } } }

struct Row { ex: String, quote: String, bid: f64, ask: f64, vol: f64, buy: f64, sell: f64, prem: Option<f64>, norm: Option<f64>, stale: bool }

fn f(v: &Value) -> Option<f64> { v.as_f64() }

/// Flat section: overline title with a right-aligned caption, its content, then a hairline across
/// the column before the next one (sections are separated by rules, not boxed).
fn card(ui: &mut Ui, title: &str, sub: &str, add: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
        overline_at(ui.painter(), pos2(r.left() + 2.0, r.center().y), Align2::LEFT_CENTER, title);
        if !sub.is_empty() { ui.painter().text(pos2(r.right() - 2.0, r.center().y), Align2::RIGHT_CENTER, sub, mono(11.0), dim()); }
        add(ui);
    });
    ui.add_space(8.0);
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().with_clip_rect(ui.clip_rect().expand2(vec2(8.0, 0.0))).hline(r.left() - 8.0..=r.right() + 8.0, r.center().y, Stroke::new(1.0, line()));
    ui.add_space(6.0);
}

/// Venue row: hover fill, logo, name (12 medium); returns the rect for the caller's right-hand columns.
fn venue_row(ui: &mut Ui, ex: &str) -> (Rect, egui::Response) {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::hover());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    if hv > 0.0 { ui.painter().rect_filled(r, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
    let r = r.shrink2(vec2(2.0, 0.0));
    if let Some(e) = Exchange::parse(ex) { icon(ui.painter(), e, Rect::from_center_size(pos2(r.left() + 7.0, r.center().y), vec2(14.0, 14.0)), false); }
    ui.painter().text(pos2(r.left() + 21.0, r.center().y), Align2::LEFT_CENTER, ex, prop_m(12.0), fg());
    (r, resp)
}

/// Wrapped muted footnote.
fn note(ui: &mut Ui, text: &str) { kit::note(ui, text, dim()); }

/// Thin capsule track (percentile, pressure): 4 px, radius 2.
fn track_rect(ui: &mut Ui) -> Rect {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let r = r.shrink2(vec2(2.0, 0.0));
    ui.painter().rect_filled(r, 2, hl());
    r
}

/// Label left (12 grey), state right (12 semibold in `col`), in a 22 px row.
fn state_row(ui: &mut Ui, k: &str, v: &str, col: Color32, h: f32, font: egui::FontId) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let r = r.shrink2(vec2(2.0, 0.0));
    ui.painter().text(r.left_center(), Align2::LEFT_CENTER, k, prop(12.0), mu());
    ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, v, font, col);
}

/// Small clock glyph (stale venue).
fn clock_glyph(p: &egui::Painter, c: egui::Pos2, col: Color32) {
    let s = Stroke::new(1.2, col);
    p.circle_stroke(c, 4.5, s);
    p.line_segment([c, c + vec2(0.0, -3.0)], s);
    p.line_segment([c, c + vec2(2.2, 0.0)], s);
}

impl VenuePages {
    /// 5m / 1h / 24h picker for the column header's second row.
    pub fn window_picker(&mut self, ui: &mut Ui) {
        let r = ui.scope(|ui| { seg_fit(ui, &mut self.window, &WINDOWS); }).response;
        r.on_hover_text(l("Volume window"));
    }

    /// Each venue's premium over the composite, its share of traded volume over the window and of
    /// resting depth within ±1% of its own mid.
    pub fn venues(&mut self, ui: &mut Ui, call: &mut Call) {
        if self.polled.is_none_or(|(t, w)| w != self.window || t.elapsed() >= Duration::from_secs(1)) {
            if let Ok(r) = call(json!({"op": "venue_share", "minutes": self.window})) { self.reply = r; }
            self.polled = Some((Instant::now(), self.window));
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let rows: Vec<Row> = self.reply["rows"].as_array().map(|a| a.iter().map(|r| Row {
            ex: r["ex"].as_str().unwrap_or_default().into(), quote: r["quote"].as_str().unwrap_or_default().into(),
            bid: f(&r["bid_usd"]).unwrap_or(0.0), ask: f(&r["ask_usd"]).unwrap_or(0.0), vol: f(&r["vol_usd"]).unwrap_or(0.0),
            buy: f(&r["buy_usd"]).unwrap_or(0.0), sell: f(&r["sell_usd"]).unwrap_or(0.0),
            prem: f(&r["prem_bps"]), norm: f(&r["norm_bps"]), stale: r["stale"] == true,
        }).collect()).unwrap_or_default();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
            ui.add_space(4.0);
            if rows.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| ui.label(egui::RichText::new(l("Loading…")).font(prop(12.0)).color(dim())));
                return;
            }
            self.prices(ui, &rows);
            let w = WINDOWS.iter().find(|w| w.0 == self.window).map_or("1h", |w| w.1);
            let mut by_vol: Vec<&Row> = rows.iter().collect();
            by_vol.sort_by(|a, b| b.vol.total_cmp(&a.vol));
            let vol: f64 = rows.iter().map(|r| r.vol).sum();
            share(ui, l("Volume share"), &format!("{} {w} · ${}", l("Traded"), big(vol)), &by_vol, vol, |r| r.vol,
                |r| { let b = r.buy + r.sell; if b > 0.0 { format!("{} {:.0}%", l("buy"), r.buy / b * 100.0) } else { String::new() } });
            let mut by_depth: Vec<&Row> = rows.iter().collect();
            by_depth.sort_by(|a, b| (b.bid + b.ask).total_cmp(&(a.bid + a.ask)));
            let depth: f64 = rows.iter().map(|r| r.bid + r.ask).sum();
            share(ui, l("Book share"), &format!("{} · ${}", l("Depth ±1%"), big(depth)), &by_depth, depth, |r| r.bid + r.ask,
                |r| { let d = r.bid + r.ask; if d > 0.0 { format!("{} {:.0}%", l("bid"), r.bid / d * 100.0) } else { String::new() } });
        });
    }

    /// Executable spread between venues, then each venue's premium over the composite next to its usual level.
    fn prices(&self, ui: &mut Ui, rows: &[Row]) {
        let arb = &self.reply["arb"];
        card(ui, l("Price spread"), l("vs composite · usual"), |ui| {
            ui.add_space(2.0);
            match f(&arb["net_bps"]) {
                Some(net) if net > 0.0 => {
                    let s = |k: &str| arb[k].as_str().unwrap_or_default().to_string();
                    let text = lf("Buy %@ %@, sell %@ %@: %+.1f bp after fees", &[s("buy"), fmt_px(f(&arb["ask"]).unwrap_or(0.0)), s("sell"), fmt_px(f(&arb["bid"]).unwrap_or(0.0)), format!("{net:+.1}")]);
                    ui.add(egui::Label::new(egui::RichText::new(format!("↔  {text}")).font(prop_m(12.0)).color(up())).wrap())
                        .on_hover_text(lf("%@ books only, gross %+.1f bp, taker fees from Settings → Trading. Top of book only: size and latency decide whether it fills.",
                            &[s("quote"), format!("{:+.1}", f(&arb["gross_bps"]).unwrap_or(0.0))]));
                }
                net => {
                    let text = net.map(|n| lf("No executable spread (best %+.1f bp after fees)", &[format!("{n:+.1}")])).unwrap_or_else(|| l("No executable spread").into());
                    ui.add(egui::Label::new(egui::RichText::new(text).font(prop(12.0)).color(mu())).wrap());
                }
            }
            ui.add_space(2.0);
            let mut list: Vec<&Row> = rows.iter().filter(|r| r.prem.is_some()).collect();
            list.sort_by(|a, b| b.prem.unwrap_or(0.0).total_cmp(&a.prem.unwrap_or(0.0)));
            for r in list {
                let p = r.prem.unwrap_or(0.0);
                // far from its own usual premium: highlight (the usual offset itself is structure, not signal)
                let off = r.norm.is_some_and(|n| (p - n).abs() >= 5.0);
                let (rect, resp) = venue_row(ui, &r.ex);
                let painter = ui.painter();
                let nx = painter.layout_no_wrap(r.ex.clone(), prop_m(12.0), fg()).size().x;
                let qx = painter.text(pos2(rect.left() + 26.0 + nx, rect.center().y), Align2::LEFT_CENTER, &r.quote, prop(11.0), dim()).right();
                if r.stale { clock_glyph(painter, pos2(qx + 9.0, rect.center().y), WARN); }
                let col = if off { if p > r.norm.unwrap_or(0.0) { up() } else { dn() } } else { fg() };
                painter.text(rect.right_center(), Align2::RIGHT_CENTER, format!("{p:+.1} bp"), mono_m(12.0), col);
                painter.text(pos2(rect.right() - 70.0, rect.center().y), Align2::RIGHT_CENTER, r.norm.map(|n| format!("{n:+.1}")).unwrap_or("–".into()), mono(11.5), mu());
                if r.stale { resp.on_hover_text(l("No update for 10 s")); }
            }
        });
    }

    /// Statistical models for the current market: volatility forecast, book / flow pressure, carry.
    pub fn quant(&self, ui: &mut Ui, q: &Value) {
        egui::ScrollArea::vertical().auto_shrink([false; 2]).scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
            ui.add_space(4.0);
            card(ui, l("Volatility model"), "", |ui| {
                let v = &q["vol"];
                if let (Some(s1), Some(s24), Some(pc)) = (f(&v["sigma_1h"]), f(&v["sigma_24h"]), f(&v["percentile"])) {
                    let range = |k: &str| format!("{} – {}", fmt_px(f(&v[k][0]).unwrap_or(0.0)), fmt_px(f(&v[k][1]).unwrap_or(0.0)));
                    kv(ui, l("Expected move 1h"), &format!("±{:.2}%", s1 * 100.0), Some(&range("range_1h")));
                    kv(ui, l("Expected move 24h"), &format!("±{:.2}%", s24 * 100.0), Some(&range("range_24h")));
                    let (name, col) = if pc < 0.2 { (l("Compressed"), accent()) } else if pc > 0.8 { (l("Expanded"), WARN) } else { (l("Normal"), fg()) };
                    state_row(ui, l("Volatility regime"), name, col, 22.0, prop_s(12.0));
                    let t = track_rect(ui);
                    ui.painter().rect_filled(Rect::from_min_size(t.min, vec2((t.width() * pc.clamp(0.0, 1.0) as f32).max(4.0), t.height())), 2, if pc > 0.8 { WARN } else { accent() });
                    ui.add_space(2.0);
                    note(ui, &lf("%.0f%% of the last day's hours were calmer", &[format!("{:.0}", pc * 100.0)]));
                } else {
                    ui.label(egui::RichText::new(l("Needs an hour of data")).font(prop(12.0)).color(mu()));
                }
                note(ui, l("EWMA of 1m returns, 60 min half-life"));
            });
            card(ui, l("Pressure"), "", |ui| {
                let p = f(&q["pressure"]).unwrap_or(0.0);
                let (name, col) = if p > 20.0 { (l("Buyers in control"), up()) } else if p < -20.0 { (l("Sellers in control"), dn()) } else { (l("Balanced"), fg()) };
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
                let r = r.shrink2(vec2(2.0, 0.0));
                ui.painter().text(r.left_center(), Align2::LEFT_CENTER, name, prop_s(13.0), col);
                ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, format!("{p:+.0}"), display_s(20.0), if p >= 0.0 { up() } else { dn() });
                // centered gauge: buyers fill right, sellers left
                let t = track_rect(ui);
                let half = t.width() / 2.0;
                let w = half * (p.abs().min(100.0) / 100.0) as f32;
                let fill = if p >= 0.0 { Rect::from_min_max(pos2(t.center().x, t.top()), pos2(t.center().x + w, t.bottom())) } else { Rect::from_min_max(pos2(t.center().x - w, t.top()), pos2(t.center().x, t.bottom())) };
                ui.painter().rect_filled(fill, 2, if p >= 0.0 { up() } else { dn() });
                ui.painter().vline(t.center().x, t.top() - 3.0..=t.bottom() + 3.0, Stroke::new(1.0, mu()));
                ui.add_space(4.0);
                kv(ui, l("Bids / asks ±0.5%"), &format!("${} / ${}", big(f(&q["book_bid_usd"]).unwrap_or(0.0)), big(f(&q["book_ask_usd"]).unwrap_or(0.0))), None);
                note(ui, l("Book depth ±0.5% and 5 min aggressor flow; describes now, not a forecast"));
            });
            card(ui, l("Carry"), "", |ui| {
                let mut carry: Vec<&Value> = q["carry"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
                carry.sort_by(|a, b| f(&b["funding_apr"]).unwrap_or(0.0).total_cmp(&f(&a["funding_apr"]).unwrap_or(0.0)));
                if !carry.is_empty() {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
                    overline_at(ui.painter(), pos2(r.right() - 72.0, r.center().y), Align2::RIGHT_CENTER, l("APR"));
                    overline_at(ui.painter(), pos2(r.right() - 2.0, r.center().y), Align2::RIGHT_CENTER, l("Basis"));
                }
                for c in carry {
                    let (r, _) = venue_row(ui, c["ex"].as_str().unwrap_or_default());
                    let apr = f(&c["funding_apr"]);
                    let p = ui.painter();
                    p.text(pos2(r.right() - 70.0, r.center().y), Align2::RIGHT_CENTER, apr.map(|a| format!("{a:+.1}%")).unwrap_or("–".into()), mono_m(12.0),
                        if apr.unwrap_or(0.0) >= 0.0 { up() } else { dn() });
                    p.text(r.right_center(), Align2::RIGHT_CENTER, f(&c["basis_bps"]).map(|b| format!("{b:+.1} bp")).unwrap_or("–".into()), mono(12.0), fg());
                }
                note(ui, l("Predicted funding, annualized; basis: perp over spot"));
            });
        });
    }
}

/// Key on the left (12 grey), value right-aligned (12.5 medium numbers) with a smaller line under it.
fn kv(ui: &mut Ui, k: &str, v: &str, sub: Option<&str>) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), if sub.is_some() { 36.0 } else { 22.0 }), Sense::hover());
    let r = r.shrink2(vec2(2.0, 0.0));
    let p = ui.painter();
    p.text(pos2(r.left(), r.top() + 11.0), Align2::LEFT_CENTER, k, prop(12.0), mu());
    p.text(pos2(r.right(), r.top() + 11.0), Align2::RIGHT_CENTER, v, mono_m(12.5), fg_strong());
    if let Some(s) = sub { p.text(pos2(r.right(), r.top() + 27.0), Align2::RIGHT_CENTER, s, mono(11.0), dim()); }
}

/// One share section: a stacked bar of every venue, then a row per venue (extra text, share %).
fn share(ui: &mut Ui, title: &str, sub: &str, list: &[&Row], total: f64, value: impl Fn(&Row) -> f64, extra: impl Fn(&Row) -> String) {
    card(ui, title, sub, |ui| {
        // stacked share bar: 4 px, 2 px gaps between venues, as the book's ratio bar
        ui.add_space(2.0);
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
        let bar = bar.shrink2(vec2(2.0, 0.0));
        ui.painter().rect_filled(bar, 2, hl());
        if total > 0.0 {
            let shown: Vec<(&Row, f32)> = list.iter().map(|r| (*r, (value(r) / total) as f32 * bar.width())).filter(|x| x.1 >= 1.0).collect();
            let mut x = bar.left();
            for (i, (r, w)) in shown.iter().enumerate() {
                let last = i + 1 == shown.len();
                let col = Exchange::parse(&r.ex).map(ex_color).unwrap_or(mu());
                ui.painter().rect_filled(Rect::from_min_max(pos2(x, bar.top()), pos2((x + w - if last { 0.0 } else { 2.0 }).max(x + 1.0), bar.bottom())), 2, col);
                x += w;
            }
        }
        ui.add_space(2.0);
        for r in list {
            let v = value(r);
            let (rect, resp) = venue_row(ui, &r.ex);
            let p = ui.painter();
            p.text(rect.right_center(), Align2::RIGHT_CENTER, format!("{:.1}%", if total > 0.0 { v / total * 100.0 } else { 0.0 }), mono_m(12.0), fg());
            p.text(pos2(rect.right() - 56.0, rect.center().y), Align2::RIGHT_CENTER, extra(r), mono(11.5), mu());
            resp.on_hover_text(format!("${}", big(v)));
        }
    });
}
