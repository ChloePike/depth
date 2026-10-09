//! Order book (cross-venue USD or one venue) and the trade tape.
use super::{fmt_dp, fmt_px, fmt_qty, step_dp, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Rect, RichText, Sense, Ui};
use std::collections::HashSet;
use terminal_one::agg::{Agg, Level};
use terminal_one::{Exchange, Market, Side};

const ROW: f32 = 18.0;
/// height of the native header laid over the book column (points)
pub const NATIVE_HEADER: f32 = 62.0;

const GROUPS: [f64; 5] = [1.0, 2.0, 5.0, 10.0, 50.0];

/// `quote`: sizes in quote currency (size x price) instead of base.
pub struct BookView { trades: bool, pub src: Option<Exchange>, group: usize, pub clicked: Option<(Exchange, f64)>, quote: bool,
    /// levels last computed: (when, what they were computed for, bids, asks). The rows repaint every
    /// frame from this; the cross-venue aggregation runs at most every REFRESH.
    cache: Option<(std::time::Instant, (Option<Exchange>, Market, usize, u64), Vec<Level>, Vec<Level>)> }

/// How often the book's levels are recomputed: fast enough to feel live, slow enough to read
/// (and the aggregation across ten venues is not free).
const REFRESH: std::time::Duration = std::time::Duration::from_millis(100);

impl Default for BookView { fn default() -> Self { BookView { trades: false, src: None, group: 1, clicked: None, quote: false, cache: None } } }

/// Quote amounts: 12.3K / 4.56M style, whole units below a thousand.
fn fmt_big_book(v: f64) -> String {
    if v >= 1e6 { format!("{:.2}M", v / 1e6) } else if v >= 1e3 { format!("{:.1}K", v / 1e3) } else { format!("{v:.0}") }
}

/// Group one venue's native book into `bin`-wide levels.
fn group(levels: impl Iterator<Item = (f64, f64)>, bin: f64, up: bool, ex: Exchange, n: usize) -> Vec<Level> {
    let mut out: Vec<Level> = vec![];
    for (p, q) in levels {
        let px = if up { (p / bin).ceil() } else { (p / bin).floor() } * bin;
        match out.last_mut() {
            Some(l) if (l.px - px).abs() < bin * 1e-6 => { l.qty += q; l.by[0].1 += q; }
            _ => { if out.len() == n { break; } out.push(Level { px, qty: q, by: vec![(ex, q)] }); }
        }
    }
    out
}

impl BookView {
    /// Book controls for the native header.
    pub fn native_json(&self, a: &Agg, market: Market) -> serde_json::Value {
        let market = if market == Market::Margin { Market::Spot } else { market };
        let mut vs: Vec<String> = a.venues.keys().filter(|(_, m)| *m == market).map(|(e, _)| format!("{e:?}")).collect();
        vs.sort();
        let mid = match self.src { None => a.mid(market), Some(e) => a.venues.get(&(e, market)).and_then(|v| v.mid()) };
        let groups: Vec<String> = mid.map(|m| { let b = terminal_one::agg::nice(m * 1e-5); GROUPS.iter().map(|g| fmt_dp(b * g, step_dp(b * g))).collect() }).unwrap_or_default();
        serde_json::json!({"trades": self.trades, "quote": self.quote, "src": self.src.map(|e| format!("{e:?}")), "venues": vs, "group": self.group, "groups": groups})
    }

    /// Levels for the native book: `n` per side, best first, already grouped / aligned / in the chosen unit;
    /// or the latest trades when the trades tab is open.
    pub fn native_levels(&self, a: &Agg, market: Market, scope: &HashSet<Exchange>, n: usize) -> serde_json::Value {
        use serde_json::json;
        let market = if market == Market::Margin { Market::Spot } else { market };
        if self.trades {
            let rows: Vec<_> = a.trades.iter().rev()
                .filter(|x| x.market == market && scope.contains(&x.ex) && self.src.is_none_or(|e| e == x.ex))
                .take(n * 2).collect();
            let big = { let mut q: Vec<f64> = rows.iter().map(|x| x.qty).collect(); q.sort_by(f64::total_cmp); q.get(q.len() * 9 / 10).copied().unwrap_or(f64::MAX) };
            return json!({"trades": rows.iter().map(|x| json!({"px": fmt_px(x.px), "qty": fmt_qty(x.qty), "buy": x.side == Side::Buy,
                "big": x.qty >= big, "t": super::hms(x.ts), "ex": format!("{:?}", x.ex)})).collect::<Vec<_>>()});
        }
        let Some(mid) = (match self.src { None => a.mid(market), Some(e) => a.venues.get(&(e, market)).and_then(|v| v.mid()) }) else { return json!({}) };
        let base_bin = terminal_one::agg::nice(mid * 1e-5);
        let bin = base_bin * GROUPS[self.group.min(GROUPS.len() - 1)];
        terminal_one::ex::hyperliquid::set_book_group(bin);
        let (bids, asks) = match self.src {
            None => {
                let (b, k) = a.book_where(market, bin, 0.03, true, |e| scope.contains(&e));
                (b.into_iter().take(n).collect::<Vec<_>>(), k.into_iter().take(n).collect::<Vec<_>>())
            }
            Some(e) => match a.venues.get(&(e, market)) {
                Some(v) => (group(v.book.bids(), bin, false, e, n), group(v.book.asks(), bin, true, e, n)),
                None => (vec![], vec![]),
            },
        };
        let quote = self.quote;
        let size = |l: &Level| if quote { l.qty * l.px } else { l.qty };
        let f = |x: f64| if quote { fmt_big_book(x) } else { fmt_qty(x) };
        let dp = step_dp(bin);
        let side = |ls: &[Level]| {
            let mut c = 0.0;
            ls.iter().map(|l| {
                c += size(l);
                let tot = l.qty.max(1e-12);
                json!({"px": l.px, "p": fmt_dp(l.px, dp), "s": f(size(l)), "q": size(l), "t": f(c), "c": c,
                       "by": l.by.iter().map(|(e, q)| { let k = ex_color(*e); json!([format!("{e:?}"), q / tot, format!("#{:02x}{:02x}{:02x}", k.r(), k.g(), k.b())]) }).collect::<Vec<_>>()})
            }).collect::<Vec<_>>()
        };
        let (b, k) = (side(&bids), side(&asks));
        let spread = match (bids.first(), asks.first()) { (Some(b), Some(k)) => Some((k.px / b.px - 1.0) * 1e4), _ => None };
        json!({"bids": b, "asks": k, "mid": fmt_px(mid), "spread_bp": spread, "clickable": self.src.map(|e| format!("{e:?}"))})
    }

    /// {trades: bool}, {quote: bool}, {src: venue|null}, {group: index}
    pub fn native_set(&mut self, v: &serde_json::Value) {
        if let Some(b) = v["trades"].as_bool() { self.trades = b; }
        if let Some(b) = v["quote"].as_bool() { self.quote = b; }
        if v.get("src").is_some() { self.src = v["src"].as_str().and_then(Exchange::parse); }
        if let Some(g) = v["group"].as_u64() { self.group = (g as usize).min(GROUPS.len() - 1); }
    }

    pub fn show(&mut self, ui: &mut Ui, a: &Agg, market: Market, scope: &HashSet<Exchange>, base: &str) {
        let market = if market == Market::Margin { Market::Spot } else { market };
        // a side panel shrinks to its content's width; hold the width the user gave it
        ui.set_min_width(ui.available_width());
        // hosted: the native header sits over the top of this column
        if native() { ui.add_space(NATIVE_HEADER); }
        // hosted: tabs, unit, source and grouping are native controls (native_set)
        if !native() { ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if tab(ui, t("book.title"), !self.trades, 12.5).clicked() { self.trades = false; }
            if tab(ui, t("book.trades"), self.trades, 12.5).clicked() { self.trades = true; }
            // size unit: base coin or quote (USDT)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if tab(ui, "USDT", self.quote, 11.5).clicked() { self.quote = true; }
                if tab(ui, base, !self.quote, 11.5).clicked() { self.quote = false; }
            });
        }); ui.add_space(2.0); }
        if self.trades { return self.tape(ui, a, market, scope); }

        let Some(mid) = (match self.src { None => a.mid(market), Some(e) => a.venues.get(&(e, market)).and_then(|v| v.mid()) }) else {
            ui.label(t("chart.loading"));
            return;
        };
        let base_bin = terminal_one::agg::nice(mid * 1e-5);
        let bin = base_bin * GROUPS[self.group];
        if !native() { ui.horizontal(|ui| {
            let cur = self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| format!("{} USD", t("src.agg")));
            if self.src.is_none() { ui.label(RichText::new(t("book.aligned")).font(prop(10.5)).color(dim())).on_hover_text(t("book.aligned_tip")); }
            egui::ComboBox::from_id_salt("book_src").icon(chevron).selected_text(cur).width(120.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.src, None, format!("{} USD", t("src.agg")));
                let mut vs: Vec<Exchange> = a.venues.keys().filter(|(_, m)| *m == market).map(|(e, _)| *e).collect();
                vs.sort_by_key(|e| format!("{e:?}"));
                for e in vs { ui.selectable_value(&mut self.src, Some(e), format!("{e:?}")); }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("book_group").icon(chevron).selected_text(fmt_dp(bin, step_dp(bin))).width(70.0).show_ui(ui, |ui| {
                    for (i, g) in GROUPS.iter().enumerate() { ui.selectable_value(&mut self.group, i, fmt_dp(base_bin * g, step_dp(base_bin * g))); }
                });
                ui.label(egui::RichText::new(t("book.group")).color(mu()));
            });
        }); }

        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let painter = ui.painter_at(rect);
        let n = (((rect.height() - ROW * 3.5) / 2.0 / ROW).floor() as usize).max(3);
        let key = (self.src, market, n, bin.to_bits());
        let fresh = self.cache.as_ref().is_some_and(|(t, k, _, _)| *k == key && t.elapsed() < REFRESH);
        if !fresh {
            let (b, k) = match self.src {
                None => {
                    let (b, k) = a.book_where(market, bin, 0.03, true, |e| scope.contains(&e));
                    (b.into_iter().take(n).collect::<Vec<_>>(), k.into_iter().take(n).collect::<Vec<_>>())
                }
                Some(e) => match a.venues.get(&(e, market)) {
                    Some(v) => (group(v.book.bids(), bin, false, e, n), group(v.book.asks(), bin, true, e, n)),
                    None => (vec![], vec![]),
                },
            };
            self.cache = Some((std::time::Instant::now(), key, b, k));
        }
        let (bids, asks) = self.cache.as_ref().map(|(_, _, b, k)| (b.clone(), k.clone())).unwrap_or_default();
        // repaint when the next refresh is due, even if nothing else asks for a frame
        ui.ctx().request_repaint_after(REFRESH);

        // header
        let cols = [rect.left() + 8.0, rect.left() + rect.width() * 0.52, rect.right() - 40.0];
        let hy = rect.top() + ROW / 2.0;
        painter.text(pos2(cols[0], hy), Align2::LEFT_CENTER, t("book.price"), prop(11.0), dim());
        painter.text(pos2(cols[1], hy), Align2::RIGHT_CENTER, t("book.size"), prop(11.0), dim());
        painter.text(pos2(cols[2], hy), Align2::RIGHT_CENTER, t("book.total"), prop(11.0), dim());

        let quote = self.quote;
        let size = |l: &Level| if quote { l.qty * l.px } else { l.qty };
        let cum = |ls: &[Level]| -> Vec<f64> { ls.iter().scan(0.0, |s, l| { *s += size(l); Some(*s) }).collect() };
        let (cb, ca) = (cum(&bids), cum(&asks));
        let max = cb.last().copied().unwrap_or(0.0).max(ca.last().copied().unwrap_or(0.0)).max(1e-12);
        let dp = step_dp(bin);
        let row = |i: usize, y: f32, l: &Level, c: f64, is_ask: bool| {
            let r = Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), ROW));
            let w = (c / max) as f32 * (rect.width() - 36.0);
            let col = if is_ask { dn() } else { up() };
            painter.rect_filled(Rect::from_min_max(pos2(r.right() - 36.0 - w, r.top() + 1.0), pos2(r.right() - 36.0, r.bottom() - 1.0)), 0, col.linear_multiply(0.12));
            painter.text(pos2(cols[0], r.center().y), Align2::LEFT_CENTER, fmt_dp(l.px, dp), mono(11.5), col);
            let f = |x: f64| if quote { fmt_big_book(x) } else { fmt_qty(x) };
            painter.text(pos2(cols[1], r.center().y), Align2::RIGHT_CENTER, f(size(l)), mono(11.5), fg());
            painter.text(pos2(cols[2], r.center().y), Align2::RIGHT_CENTER, f(c), mono(11.0), mu());
            // per-venue split
            let (mut x, total) = (r.right() - 32.0, l.qty.max(1e-12));
            for (e, q) in &l.by {
                let ww = (q / total) as f32 * 26.0;
                painter.rect_filled(Rect::from_min_size(pos2(x, r.center().y - 2.5), vec2(ww, 5.0)), 0, ex_color(*e));
                x += ww;
            }
            let _ = i;
        };
        let top = rect.top() + ROW;
        let mut rows_at: Vec<(Rect, f64)> = vec![];
        for (i, l) in asks.iter().enumerate() {
            let y = top + (n - 1 - i) as f32 * ROW;
            row(i, y, l, ca[i], true);
            rows_at.push((Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), ROW)), l.px));
        }
        let my = top + n as f32 * ROW;
        let (bb, ba) = (bids.first().map(|l| l.px), asks.first().map(|l| l.px));
        painter.rect_filled(Rect::from_min_size(pos2(rect.left(), my), vec2(rect.width(), ROW * 1.5)), 0, panel2());
        painter.text(pos2(cols[0], my + ROW * 0.75), Align2::LEFT_CENTER, fmt_px(mid), display(16.0), fg());
        if let (Some(b), Some(k)) = (bb, ba) {
            painter.text(pos2(rect.right() - 8.0, my + ROW * 0.75), Align2::RIGHT_CENTER,
                format!("{} {:.1}bp", t("book.spread"), (k / b - 1.0) * 1e4), mono(11.0), mu());
        }
        for (i, l) in bids.iter().enumerate() {
            let y = my + ROW * 1.5 + i as f32 * ROW;
            row(i, y, l, cb[i], false);
            rows_at.push((Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), ROW)), l.px));
        }
        // click-to-fill only on a single venue's raw book (aligned aggregate prices are not executable)
        if let Some(e) = self.src {
            for (i, (r, px)) in rows_at.iter().enumerate() {
                let resp = ui.interact(*r, ui.id().with(("bkrow", i)), Sense::click());
                if resp.hovered() { painter.rect_stroke(*r, 0, egui::Stroke::new(1.0, line()), egui::StrokeKind::Inside); }
                if resp.clicked() { self.clicked = Some((e, *px)); }
            }
        }

        // buy/sell ratio of the shown depth
        let (sb, sa) = (cb.last().copied().unwrap_or(0.0), ca.last().copied().unwrap_or(0.0));
        if sb + sa > 0.0 {
            let y = my + ROW * 1.5 + n as f32 * ROW + 6.0;
            let w = rect.width() - 16.0;
            let wb = (sb / (sb + sa)) as f32 * w;
            painter.rect_filled(Rect::from_min_size(pos2(rect.left() + 8.0, y + 12.0), vec2(wb, 4.0)), 1, up());
            painter.rect_filled(Rect::from_min_size(pos2(rect.left() + 8.0 + wb, y + 12.0), vec2(w - wb, 4.0)), 1, dn());
            painter.text(pos2(rect.left() + 8.0, y + 4.0), Align2::LEFT_CENTER, format!("B {:.1}%", sb / (sb + sa) * 100.0), mono(10.5), up());
            painter.text(pos2(rect.right() - 8.0, y + 4.0), Align2::RIGHT_CENTER, format!("{:.1}% S", sa / (sb + sa) * 100.0), mono(10.5), dn());
        }
    }

    fn tape(&self, ui: &mut Ui, a: &Agg, market: Market, scope: &HashSet<Exchange>) {
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let painter = ui.painter_at(rect);
        let rows: Vec<_> = a.trades.iter().rev()
            .filter(|x| x.market == market && scope.contains(&x.ex) && self.src.is_none_or(|e| e == x.ex))
            .take((rect.height() / ROW) as usize).collect();
        let big = {
            let mut q: Vec<f64> = rows.iter().map(|x| x.qty).collect();
            q.sort_by(f64::total_cmp);
            q.get(q.len() * 9 / 10).copied().unwrap_or(f64::MAX)
        };
        for (i, x) in rows.iter().enumerate() {
            let y = rect.top() + (i as f32 + 0.5) * ROW;
            let col = if x.side == Side::Buy { up() } else { dn() };
            let f = if x.qty >= big { mono(11.5) } else { mono(11.0) };
            if x.qty >= big { painter.rect_filled(Rect::from_min_size(pos2(rect.left(), y - ROW / 2.0), vec2(rect.width(), ROW)), 0, col.linear_multiply(0.08)); }
            painter.text(pos2(rect.left() + 8.0, y), Align2::LEFT_CENTER, fmt_px(x.px), f.clone(), col);
            painter.text(pos2(rect.left() + rect.width() * 0.55, y), Align2::RIGHT_CENTER, fmt_qty(x.qty), f, if x.qty >= big { fg() } else { mu() });
            painter.text(pos2(rect.right() - 22.0, y), Align2::RIGHT_CENTER, super::hms(x.ts), mono(10.5), dim());
            icon(&painter, x.ex, Rect::from_center_size(pos2(rect.right() - 11.0, y), vec2(12.0, 12.0)), false);
        }
    }
}
