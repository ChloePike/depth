//! Order book (cross-venue USD or one venue) and the trade tape.
use super::{fmt_dp, fmt_px, fmt_qty, kit, l, step_dp, t, theme::*};
use eframe::egui::{self, pos2, vec2, Align2, Rect, Sense, Stroke, Ui};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use depth::agg::{Agg, Level};
use depth::{Exchange, Market, Side};

/// level / print row height, header row, mid row (with its 4 px margins), ratio block
const ROW: f32 = 21.0;
const HEAD: f32 = 18.0;
const MID_H: f32 = 38.0;
const RATIO_H: f32 = 36.0;
/// height of the native header laid over the book column (points)
pub const NATIVE_HEADER: f32 = 62.0;

const GROUPS: [f64; 5] = [1.0, 2.0, 5.0, 10.0, 50.0];

/// `quote`: sizes in quote currency (size x price) instead of base.
pub struct BookView { trades: bool,

    /// 2 venues, 3 quant (own window only); otherwise `trades` picks book or tape
    pub page: u8, pub src: Option<Exchange>, group: usize, pub clicked: Option<(Exchange, f64)>, quote: bool,
    /// tape filter: index into MIN_USD
    min_usd: usize,
    /// levels last computed: (when, what they were computed for, bids, asks). The rows repaint every
    /// frame from this; the cross-venue aggregation runs at most every REFRESH.
    cache: Option<(Instant, (Option<Exchange>, Market, usize, u64), Vec<Level>, Vec<Level>)>,
    /// levels a taker trade printed into: (ask side, price bin) -> when lit
    flash: HashMap<(bool, i64), Instant>,
    /// newest trade time already checked for hits, per venue
    seen: HashMap<Exchange, i64>,
    /// last mid and whether its last move was up (mid row color)
    last_mid: (f64, bool) }

/// How long an eaten level stays lit, fading out; at most one flash per level per FLASH_GAP.
const FLASH: Duration = Duration::from_millis(400);
const FLASH_GAP: Duration = Duration::from_millis(300);

/// How often the book's levels are recomputed: fast enough to feel live, slow enough to read
/// (and the aggregation across ten venues is not free).
const REFRESH: Duration = Duration::from_millis(100);

impl Default for BookView { fn default() -> Self { BookView { trades: false, page: 0, src: None, group: 1, clicked: None, quote: false, min_usd: 0, cache: None, flash: Default::default(), seen: Default::default(), last_mid: (0.0, true) } } }

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
        let groups: Vec<String> = mid.map(|m| { let b = depth::agg::nice(m * 1e-5); GROUPS.iter().map(|g| fmt_dp(b * g, step_dp(b * g))).collect() }).unwrap_or_default();
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
        let base_bin = depth::agg::nice(mid * 1e-5);
        let bin = base_bin * GROUPS[self.group.min(GROUPS.len() - 1)];
        depth::ex::hyperliquid::set_book_group(bin);
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
        let mid = match self.src { None => a.mid(market), Some(e) => a.venues.get(&(e, market)).and_then(|v| v.mid()) };
        // hosted: tabs, unit, source and grouping are native controls (native_set)
        if !native() {
            if self.tabs(ui) { return; }
            self.controls(ui, a, market, base, mid);
        }
        if self.trades {
            let r = ui.available_rect_before_wrap();
            ui.allocate_rect(r, Sense::hover());
            return self.tape(ui, r, a, market, scope, false);
        }

        let Some(mid) = mid else {
            ui.label(t("chart.loading"));
            return;
        };
        let base_bin = depth::agg::nice(mid * 1e-5);
        let bin = base_bin * GROUPS[self.group];
        // last move of the mid colors the mid row
        if (mid - self.last_mid.0).abs() > bin * 1e-6 { self.last_mid = (mid, mid >= self.last_mid.0); }

        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        // depth on top, the (filterable) tape under it: both at a glance, as on a venue's own page
        let split = (full.top() + full.height() * 0.62).round();
        let rect = Rect::from_min_max(full.min, pos2(full.right(), split));
        self.tape(ui, Rect::from_min_max(pos2(full.left(), split), full.max), a, market, scope, true);
        let painter = ui.painter_at(rect.expand2(vec2(2.0, 0.0)));
        // rows stretch to fill the column: header, mid line and ratio bar are fixed, the rest is levels
        let fixed = HEAD + MID_H + RATIO_H;
        let n = (((rect.height() - fixed) / 2.0 / ROW).floor() as usize).max(3);
        let rh = ((rect.height() - fixed) / (2 * n) as f32).max(ROW * 0.8);
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
            let now = Instant::now();
            // other source / market / grouping: the lit bins mean something else now
            if self.cache.as_ref().is_some_and(|(_, k, _, _)| (k.0, k.1, k.3) != (key.0, key.1, key.3)) { self.flash.clear(); }
            self.hits(a, market, scope, mid, bin, now);
            self.cache = Some((now, key, b, k));
        }
        let (bids, asks) = self.cache.as_ref().map(|(_, _, b, k)| (b.clone(), k.clone())).unwrap_or_default();
        // repaint when the next refresh is due, even if nothing else asks for a frame
        ui.ctx().request_repaint_after(REFRESH);

        // columns as the design: price left, size and total right-aligned (76 / 70 wide); the
        // aggregate adds a narrow per-venue split on the far right
        let split_w = if self.src.is_none() { 24.0 } else { 0.0 };
        let x_px = rect.left() + 2.0;
        let x_tot = rect.right() - 2.0 - split_w;
        let x_size = x_tot - 70.0;
        let hy = rect.top() + HEAD / 2.0 - 1.0;
        overline_at(&painter, pos2(x_px, hy), Align2::LEFT_CENTER, t("book.price"));
        overline_at(&painter, pos2(x_size, hy), Align2::RIGHT_CENTER, t("book.size"));
        overline_at(&painter, pos2(x_tot, hy), Align2::RIGHT_CENTER, t("book.total"));
        if bids.is_empty() && asks.is_empty() {
            // a mid without levels: depth streams are down (the status bar shows which venues)
            painter.text(rect.center(), Align2::CENTER_CENTER, t("book.waiting"), prop(12.0), mu());
        }

        let quote = self.quote;
        let size = |l: &Level| if quote { l.qty * l.px } else { l.qty };
        let cum = |ls: &[Level]| -> Vec<f64> { ls.iter().scan(0.0, |s, l| { *s += size(l); Some(*s) }).collect() };
        let (cb, ca) = (cum(&bids), cum(&asks));
        let max = cb.last().copied().unwrap_or(0.0).max(ca.last().copied().unwrap_or(0.0)).max(1e-12);
        // one decimal count per column, so sizes line up instead of switching precision row to row
        let lmax = bids.iter().chain(&asks).map(size).fold(0.0, f64::max);
        let (fs, ft) = (col_fmt(lmax, quote), col_fmt(max, quote));
        let dp = step_dp(bin);
        let (bar_l, bar_r) = (rect.left() - 2.0, x_tot + 2.0);
        let flash = &self.flash;
        // L3 overlay: the individual orders of the venues that have an order-by-order feed (Lighter
        // exact, OKX inferred), mid-aligned onto the rows as the aggregated book is; a single
        // venue shows its own orders at raw prices
        let mut blocks: HashMap<(bool, i64), Vec<(f64, depth::l3::Src, Exchange, i64, bool)>> = HashMap::new();
        let feeds: Vec<(Exchange, Market)> = match self.src {
            None => [(Exchange::Lighter, Market::Perp), (Exchange::Okx, market), (Exchange::Hyperliquid, Market::Perp)].into_iter().filter(|(e, m)| *m == market && scope.contains(e)).collect(),
            Some(e @ (Exchange::Lighter | Exchange::Hyperliquid)) => vec![(e, Market::Perp)].into_iter().filter(|(_, m)| *m == market).collect(),
            Some(e @ Exchange::Okx) => vec![(e, market)],
            _ => vec![],
        };
        for (e, m) in feeds {
            let Some(b) = a.l3.get(&(e, m)) else { continue };
            let k = if self.src.is_some() { 1.0 } else { match a.venues.get(&(e, m)).and_then(|v| v.book_mid()) { Some(vm) if vm > 0.0 => mid / vm, _ => continue } };
            for (ask, levels) in [(true, b.levels(false, 300)), (false, b.levels(true, 300))] {
                for (px, orders) in levels {
                    let p = px * k;
                    let key = if ask { (p / bin).ceil() } else { (p / bin).floor() } as i64;
                    blocks.entry((ask, key)).or_default().extend(orders.iter().map(|o| (o.qty, o.src, e, o.since, o.since_known)));
                }
            }
        }
        let hover = ui.input(|i| i.pointer.hover_pos());
        // strings for the block tooltip (the row closure's `l` is a level)
        let (s_exact, s_inf, s_split, s_rest) = (l("exact"), l("inferred"), l("split estimate"), l("resting"));
        let tip: std::cell::RefCell<Option<String>> = Default::default();
        let now = depth::now_ms();
        let row = |y: f32, l: &Level, c: f64, is_ask: bool| {
            let r = Rect::from_min_size(pos2(rect.left() - 2.0, y), vec2(rect.width() + 4.0, rh));
            let col = if is_ask { dn() } else { up() };
            // cumulative depth: a soft side-tinted fill growing from the right
            let w = ((c / max) as f32 * (bar_r - bar_l)).max(2.0);
            painter.rect_filled(Rect::from_min_max(pos2(bar_r - w, r.top() + 2.0), pos2(bar_r, r.bottom() - 2.0)), 3, col.gamma_multiply(0.12));
            // L3: a 4 px strip along the bottom of the row (the numbers stay clean): the level's
            // known orders from the right edge, front of the queue outermost, on the level-size
            // scale. Orders under 4 px merge into one segment; split estimates (unknown which
            // orders) are one faint segment, never fake individual blocks.
            if let Some(os) = blocks.get(&(is_ask, (l.px / bin).round() as i64)) {
                let qmax = (if quote { lmax / l.px.max(1e-12) } else { lmax }).max(1e-12);
                let span = bar_r - bar_l;
                let (y0, y1) = (r.bottom() - 6.0, r.bottom() - 2.0);
                // segments: (width, alpha, tooltip source index or none)
                let mut segs: Vec<(f32, f32, Option<usize>)> = vec![];
                let mut small = 0.0f32;
                let mut split = 0.0f32;
                for (i, (q, src, ..)) in os.iter().enumerate() {
                    let w = (q / qmax) as f32 * span;
                    match src {
                        depth::l3::Src::Split => { if small > 0.0 { segs.push((small, 0.35, None)); small = 0.0; } split += w; }
                        _ => {
                            if split > 0.0 { segs.push((split, 0.14, None)); split = 0.0; }
                            if w < 4.0 { small += w; if small >= 4.0 { segs.push((small, 0.35, None)); small = 0.0; } }
                            else { segs.push((w, if matches!(src, depth::l3::Src::Exact) { 0.9 } else { 0.6 }, Some(i))); }
                        }
                    }
                }
                if small > 0.0 { segs.push((small.max(2.0), 0.35, None)); }
                if split > 0.0 { segs.push((split, 0.14, None)); }
                let mut x = bar_r;
                for (w, a, idx) in segs {
                    if x <= bar_l { break; }
                    let b = Rect::from_min_max(pos2((x - w + 1.5).max(bar_l), y0), pos2(x, y1));
                    painter.rect_filled(b, 1, col.gamma_multiply(a));
                    if let (Some(i), Some(h)) = (idx, hover) {
                        if Rect::from_min_max(pos2(b.left(), r.top()), pos2(b.right(), r.bottom())).contains(h) {
                            painter.rect_filled(b, 1, fg_strong().gamma_multiply(0.5));
                            let (q, src, e, since, known) = &os[i];
                            let age = (now - since).max(0) / 1000;
                            let age = if age < 60 { format!("{age}s") } else if age < 3600 { format!("{}m {:02}s", age / 60, age % 60) } else { format!("{}h {:02}m", age / 3600, age % 3600 / 60) };
                            let how = match src { depth::l3::Src::Exact => s_exact, depth::l3::Src::Inferred => s_inf, depth::l3::Src::Split => s_split };
                            *tip.borrow_mut() = Some(format!("{e:?} · {}\n{s_rest} {}{age} · {how}", qty_sig(*q), if *known { "" } else { "> " }));
                        }
                    }
                    x -= w;
                }
            }
            let cy = r.center().y;
            painter.text(pos2(x_px, cy), Align2::LEFT_CENTER, fmt_dp(l.px, dp), mono_m(12.0), col);
            painter.text(pos2(x_size, cy), Align2::RIGHT_CENTER, fs(size(l)), mono(12.0), fg());
            painter.text(pos2(x_tot, cy), Align2::RIGHT_CENTER, ft(c), mono(12.0), mu());
            if split_w > 0.0 {
                // per-venue split of this level, venue colors, rounded ends
                let (mut x, total, bw) = (x_tot + 6.0, l.qty.max(1e-12), split_w - 6.0);
                let back = Rect::from_min_size(pos2(x, cy - 2.0), vec2(bw, 4.0));
                for (e, q) in &l.by {
                    let ww = (q / total) as f32 * bw;
                    painter.with_clip_rect(back).rect_filled(Rect::from_min_size(pos2(x, cy - 2.0), vec2(ww, 4.0)), 0, ex_color(*e));
                    x += ww;
                }
            }
        };
        let top = rect.top() + HEAD;
        let mut rows_at: Vec<(Rect, f64)> = vec![];
        for (i, l) in asks.iter().enumerate() {
            let y = top + (n - 1 - i) as f32 * rh;
            row(y, l, ca[i], true);
            rows_at.push((Rect::from_min_size(pos2(rect.left() - 2.0, y), vec2(rect.width() + 4.0, rh)), l.px));
        }
        // mid row: 17 semibold price in the direction of its last move, spread caption right
        let my = top + n as f32 * rh;
        let (bb, ba) = (bids.first().map(|l| l.px), asks.first().map(|l| l.px));
        let mr = Rect::from_min_size(pos2(rect.left(), my + 4.0), vec2(rect.width(), MID_H - 8.0));
        painter.rect_filled(mr, 5, panel2());
        let g = painter.layout_no_wrap(fmt_px(mid), mono_s(17.0), if self.last_mid.1 { up() } else { dn() });
        let gy = mr.center().y - g.size().y / 2.0;
        let base_y = gy + g.size().y * 0.78;
        painter.galley(pos2(mr.left() + 8.0, gy), g, fg());
        if let (Some(b), Some(k)) = (bb, ba) {
            // baseline-aligned with the price
            let s = painter.layout_no_wrap(format!("{} {:.1}bp", t("book.spread"), (k / b - 1.0) * 1e4), mono(11.5), mu());
            painter.galley(pos2(mr.right() - 8.0 - s.size().x, base_y - s.size().y * 0.78), s, mu());
        }
        for (i, l) in bids.iter().enumerate() {
            let y = my + MID_H + i as f32 * rh;
            row(y, l, cb[i], false);
            rows_at.push((Rect::from_min_size(pos2(rect.left() - 2.0, y), vec2(rect.width() + 4.0, rh)), l.px));
        }
        if let Some(t) = tip.into_inner() {
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), ui.id().with("l3tip"), egui::PopupAnchor::Pointer)
                .show(|ui| { ui.label(egui::RichText::new(t).font(mono(11.5))); });
        }
        // click-to-fill only on a single venue's raw book (aligned aggregate prices are not executable)
        if let Some(e) = self.src {
            for (i, (r, px)) in rows_at.iter().enumerate() {
                let resp = ui.interact(*r, ui.id().with(("bkrow", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.hovered() { painter.rect_filled(*r, 3, hl().gamma_multiply(0.6)); }
                if resp.clicked() { self.clicked = Some((e, *px)); }
            }
        }

        // buy/sell ratio of the shown depth: captions over a 4 px bar split by a 2 px gap
        let (sb, sa) = (cb.last().copied().unwrap_or(0.0), ca.last().copied().unwrap_or(0.0));
        if sb + sa > 0.0 {
            let y = rect.bottom() - RATIO_H + 8.0;
            let w = rect.width() - 4.0;
            let fb = (sb / (sb + sa)) as f32;
            painter.text(pos2(x_px, y + 6.0), Align2::LEFT_CENTER, format!("B {:.1}%", fb * 100.0), mono_s(11.0), up());
            painter.text(pos2(rect.right() - 2.0, y + 6.0), Align2::RIGHT_CENTER, format!("{:.1}% S", (1.0 - fb) * 100.0), mono_s(11.0), dn());
            let by = y + 17.0;
            let wb = ((w - 2.0) * fb).clamp(0.0, w - 2.0);
            if wb > 0.0 { painter.rect_filled(Rect::from_min_size(pos2(x_px, by), vec2(wb, 4.0)), 2, up()); }
            if w - 2.0 - wb > 0.0 { painter.rect_filled(Rect::from_min_max(pos2(x_px + wb + 2.0, by), pos2(x_px + w, by + 4.0)), 2, dn()); }
        }
    }

    /// Light the levels taker trades printed into since the last refresh: a buy ate an ask, a sell
    /// a bid. Trades map to row bins with the rows' own grouping (the aggregate shifts each venue
    /// onto the composite mid, as `book_where` does). At most one flash per level per FLASH_GAP.
    fn hits(&mut self, a: &Agg, market: Market, scope: &HashSet<Exchange>, mid: f64, bin: f64, now: Instant) {
        let mut newest = self.seen.clone();
        for x in a.trades.iter().filter(|x| x.market == market && scope.contains(&x.ex) && self.src.is_none_or(|e| e == x.ex)) {
            let seen = newest.entry(x.ex).or_insert(i64::MIN);
            *seen = (*seen).max(x.ts);
            // a venue's first refresh only marks its backlog as seen
            let Some(prev) = self.seen.get(&x.ex) else { continue };
            if x.ts <= *prev { continue; }
            let k = if self.src.is_some() { 1.0 } else {
                match a.venues.get(&(x.ex, market)).and_then(|v| v.book_mid()) { Some(vm) => mid / vm, None => continue }
            };
            let ask = x.side == Side::Buy;
            let p = x.px * k / bin;
            let id = if ask { p.ceil() } else { p.floor() } as i64;
            if self.flash.get(&(ask, id)).is_none_or(|t| now.duration_since(*t) >= FLASH_GAP) { self.flash.insert((ask, id), now); }
        }
        self.seen = newest;
        self.flash.retain(|_, t| now.duration_since(*t) < FLASH);
    }

    /// Shown page: 0 book, 1 trades, 2 venues, 3 quant, 4 L3.
    pub fn pane(&self) -> u8 { if self.page > 1 { self.page } else { self.trades as u8 } }

    /// Tab bar: Book / Trades / Venues / Quant as compact pills over a hairline that spans the
    /// column. True when a page other than book / trades is shown (its content is drawn by the
    /// caller, without the agg lock).
    pub fn tabs(&mut self, ui: &mut Ui) -> bool {
        let sp = ui.spacing().item_spacing.y;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        let mut x = row.left();
        for (i, name) in [(0u8, l("Book")), (1, l("Trades")), (2, l("Venues")), (3, l("Quant"))] {
            let r = pill(ui, pos2(x, row.top()), name, self.pane() == i, ("book_tab", i));
            x = r.rect.right() + 2.0;
            if r.clicked() && self.pane() != i {
                if i < 2 { self.trades = i == 1; self.page = 0; } else { self.page = i; }
                ui.ctx().request_repaint();
            }
        }
        // the panel's 8 px margin: the hairline runs edge to edge, 6 px under the pills
        let y = row.bottom() + 6.5;
        ui.painter().with_clip_rect(ui.clip_rect().expand2(vec2(8.0, 0.0))).hline(row.left() - 8.0..=row.right() + 8.0, y, Stroke::new(1.0, line()));
        ui.add_space((15.0 - sp).max(0.0));
        self.page == 2 || self.page == 3
    }

    /// The order-by-order feeds (Lighter REST polls, Hyperliquid's l2Book) run while the book is on screen.
    pub fn wants_l3(&self) -> bool { !self.trades && self.page < 2 && matches!(self.src, None | Some(Exchange::Lighter | Exchange::Hyperliquid)) }

    /// Filter row of Book and Trades (26 px controls, 6 px apart): source (all venues or one),
    /// price grouping (book only) and size unit, as the native header's `bookRight`.
    fn controls(&mut self, ui: &mut Ui, a: &Agg, market: Market, base: &str, mid: Option<f64>) {
        let sp = ui.spacing().item_spacing.y;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::right_to_left(egui::Align::Center)));
        c.spacing_mut().item_spacing.x = 6.0;
        let mut unit = self.quote as usize;
        if seg_fit(&mut c, &mut unit, &[(0, base), (1, "USDT")]) { self.quote = unit == 1; }
        if !self.trades {
            if let Some(mid) = mid {
                let base_bin = depth::agg::nice(mid * 1e-5);
                let bin = base_bin * GROUPS[self.group];
                let group = &mut self.group;
                dropdown(&mut c, 56.0, &fmt_dp(bin, step_dp(bin)), mono_m(12.0), None, l("Price grouping"), |ui| {
                    for (i, g) in GROUPS.iter().enumerate() {
                        if menu_item(ui, &fmt_dp(base_bin * g, step_dp(base_bin * g)), *group == i, None) { *group = i; }
                    }
                });
            }
        }
        let cur = self.src.map(|e| format!("{e:?}")).unwrap_or_else(|| l("All venues").to_string());
        let tip = if self.src.is_none() { l("All venues, mid-aligned (view only)") } else { l("This venue's raw book (click a level to fill the price)") };
        let src = &mut self.src;
        let w = c.available_width();
        dropdown(&mut c, w, &cur, prop_m(12.0), *src, tip, |ui| {
            if menu_item(ui, l("All venues"), src.is_none(), None) { *src = None; }
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
            ui.painter().hline(r.x_range().shrink(6.0), r.center().y, Stroke::new(1.0, line()));
            let mut vs: Vec<Exchange> = a.venues.keys().filter(|(_, m)| *m == market).map(|(e, _)| *e).collect();
            vs.sort_by_key(|e| format!("{e:?}"));
            for e in vs { if menu_item(ui, &format!("{e:?}"), *src == Some(e), Some(e)) { *src = Some(e); } }
        });
        ui.add_space((8.0 - sp).max(0.0));
    }

    /// The trade tape in `rect`: a filter row (minimum USDT notional), a header, then the newest
    /// prints. Fills of one taker order arrive merged (see `Agg::on`); prints of at least `BIG_USD`
    /// come from the longer `Agg::big` tape, so a whale filter still shows minutes of history.
    /// `rule`: a hairline over it (under the book).
    fn tape(&mut self, ui: &mut Ui, rect: Rect, a: &Agg, market: Market, scope: &HashSet<Exchange>, rule: bool) {
        let painter = ui.painter_at(rect.expand2(vec2(2.0, 0.0)));
        let rect = if rule {
            ui.painter().with_clip_rect(ui.clip_rect().expand2(vec2(8.0, 0.0))).hline(rect.left() - 8.0..=rect.right() + 8.0, rect.top() + 0.5, Stroke::new(1.0, line()));
            Rect::from_min_max(pos2(rect.left(), rect.top() + 8.0), rect.max)
        } else { rect };
        // filter pills: "MIN  All 10K 50K 250K 1M"
        let cy = rect.top() + 11.0;
        let mut x = overline_at(&painter, pos2(rect.left() + 2.0, cy), Align2::LEFT_CENTER, t("book.min")).right() + 8.0;
        for (i, (v, label)) in MIN_USD.iter().enumerate() {
            let label = if *v == 0.0 { t("book.all") } else { label };
            let on = self.min_usd == i;
            let r = pill_sized(ui, pos2(x, cy - 11.0), label, on, ("tape_min", i), 22.0, 12.0);
            if r.clicked() { self.min_usd = i; }
            x = r.rect.right() + 2.0;
        }
        let min = MIN_USD[self.min_usd].0;
        let src = if min >= depth::agg::BIG_USD { &a.big } else { &a.trades };
        // header, then rows of ROW
        let hy = rect.top() + 22.0 + 6.0 + HEAD / 2.0 - 1.0;
        let x_px = rect.left() + 2.0;
        let x_time = rect.right() - 26.0;
        let x_size = x_time - 70.0;
        overline_at(&painter, pos2(x_px, hy), Align2::LEFT_CENTER, t("book.price"));
        overline_at(&painter, pos2(x_size, hy), Align2::RIGHT_CENTER, t("book.size"));
        overline_at(&painter, pos2(x_time, hy), Align2::RIGHT_CENTER, t("book.time"));
        let body = Rect::from_min_max(pos2(rect.left(), rect.top() + 28.0 + HEAD), rect.max);
        let usd = |x: &depth::agg::Tick| x.px * x.qty * a.usdt(x.ex, x.market);
        let rows: Vec<_> = src.iter().rev()
            .filter(|x| x.market == market && scope.contains(&x.ex) && self.src.is_none_or(|e| e == x.ex) && usd(x) >= min)
            .take((body.height() / ROW).max(0.0) as usize).collect();
        if rows.is_empty() {
            painter.text(body.center_top() + vec2(0.0, 24.0), Align2::CENTER_CENTER, t("book.no_trades"), prop(12.0), dim());
            return;
        }
        // highlight: the top decile of what is shown, and anything of at least $100K
        let big = {
            let mut q: Vec<f64> = rows.iter().map(|x| usd(x)).collect();
            q.sort_by(f64::total_cmp);
            q.get(q.len() * 9 / 10).copied().unwrap_or(f64::MAX).min(100_000.0)
        };
        let quote = self.quote;
        let amt = |x: &depth::agg::Tick| if quote { usd(x) } else { x.qty };
        let fs = col_fmt(rows.iter().map(|x| amt(x)).fold(0.0, f64::max), quote);
        for (i, x) in rows.iter().enumerate() {
            let y = body.top() + (i as f32 + 0.5) * ROW;
            let col = if x.side == Side::Buy { up() } else { dn() };
            let whale = usd(x) >= big;
            if whale {
                // soft bar by notional, log-scaled from $10K to $10M
                let k = ((usd(x).max(1e4).log10() - 4.0) / 3.0).clamp(0.1, 1.0) as f32;
                let w = (body.width() + 4.0) * k;
                painter.rect_filled(Rect::from_min_size(pos2(body.left() - 2.0, y - ROW / 2.0 + 2.0), vec2(w, ROW - 4.0)), 3, col.gamma_multiply(0.12));
            }
            painter.text(pos2(x_px, y), Align2::LEFT_CENTER, fmt_px(x.px), mono_m(12.0), col);
            painter.text(pos2(x_size, y), Align2::RIGHT_CENTER, fs(amt(x)), if whale { mono_m(12.0) } else { mono(12.0) }, if whale { fg_strong() } else { fg() });
            painter.text(pos2(x_time, y), Align2::RIGHT_CENTER, super::hms(x.ts), mono(11.5), mu());
            icon(&painter, x.ex, Rect::from_center_size(pos2(rect.right() - 7.0, y), vec2(13.0, 13.0)), false);
        }
    }
}

/// Order size with enough decimals to show it (0.00012 stays visible, 12.5 does not grow digits).
fn qty_sig(q: f64) -> String { let d = if q > 0.0 { (3.0 - q.log10().floor()).clamp(0.0, 8.0) as usize } else { 2 }; fmt_dp(q, d) }


/// Overline at `pos`: 10.5 semibold caps, dim, 0.06 em tracking. Returns its rect.
pub(super) fn overline_at(p: &egui::Painter, pos: egui::Pos2, align: Align2, text: &str) -> Rect {
    let mut job = egui::text::LayoutJob::default();
    job.append(&text.to_uppercase(), 0.0, egui::TextFormat { font_id: prop_s(10.5), color: dim(), extra_letter_spacing: 0.6, ..Default::default() });
    let g = p.layout_job(job);
    let r = align.anchor_size(pos, g.size());
    p.galley(r.min, g, dim());
    r
}

/// Compact pill tab at `at` (26 px, 12.5): selected = hl solid and semibold, others grey medium;
/// the fill fades in.
fn pill(ui: &mut Ui, at: egui::Pos2, text: &str, on: bool, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Response {
    pill_sized(ui, at, text, on, id, 26.0, 12.5)
}

fn pill_sized(ui: &mut Ui, at: egui::Pos2, text: &str, on: bool, id: impl std::hash::Hash + std::fmt::Debug, h: f32, size: f32) -> egui::Response {
    let w = ui.painter().layout_no_wrap(text.to_string(), prop_s(size), fg()).size().x + if h > 24.0 { 20.0 } else { 14.0 };
    let r = Rect::from_min_size(at, vec2(w, h));
    let resp = ui.interact(r, ui.id().with(id), Sense::click());
    let s = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.15);
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.1);
    let fill = if s > 0.0 { hl().gamma_multiply(s.max(hv * 0.5)) } else { hl().gamma_multiply(0.5 * hv) };
    if fill.a() > 0 { ui.painter().rect_filled(r, kit::R, fill); }
    let font = if on { prop_s(size) } else { prop_m(size) };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, font, lerp(mu(), fg_strong(), s.max(hv * 0.6)));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Segmented control (`kit::segments`) sized to its labels instead of the whole row, 26 px.
pub(super) fn seg_fit<T: PartialEq + Copy>(ui: &mut Ui, sel: &mut T, items: &[(T, &str)]) -> bool {
    let w = items.iter().map(|(_, s)| ui.painter().layout_no_wrap(s.to_string(), prop_s(12.5), fg()).size().x).fold(0.0, f32::max) + 18.0;
    ui.allocate_ui(vec2(w * items.len() as f32 + 4.0, 26.0), |ui| kit::segments(ui, sel, items, 26.0)).inner
}

/// Dropdown button (26 px, panel2 + hairline, 12 medium text left, chevron right, optional venue
/// logo) opening a popup menu filled by `add`.
fn dropdown(ui: &mut Ui, w: f32, text: &str, font: egui::FontId, logo: Option<Exchange>, tip: &str, add: impl FnOnce(&mut Ui)) {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || open, 0.1);
    let p = ui.painter();
    p.rect(r, kit::R, lerp(panel2(), hl(), hv * 0.5), Stroke::new(1.0, lerp(line(), edge(), hv)), egui::StrokeKind::Inside);
    let cy = r.center().y;
    let mut x = r.left() + 8.0;
    if let Some(e) = logo { icon(p, e, Rect::from_center_size(pos2(x + 7.0, cy), vec2(14.0, 14.0)), false); x += 20.0; }
    p.with_clip_rect(r.shrink(1.0)).text(pos2(x, cy), Align2::LEFT_CENTER, text, font, lerp(fg(), fg_strong(), hv));
    let c = pos2(r.right() - 11.0, cy);
    let (cw, ch) = (3.5, 2.0);
    let pts = if open { vec![c + vec2(-cw, ch), c + vec2(0.0, -ch), c + vec2(cw, ch)] } else { vec![c + vec2(-cw, -ch), c + vec2(0.0, ch), c + vec2(cw, -ch)] };
    p.add(egui::Shape::line(pts, Stroke::new(1.3, mu())));
    let resp = resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
    egui::Popup::from_toggle_button_response(&resp)
        .frame(egui::Frame::new().fill(popup_bg()).stroke(Stroke::new(1.0, line())).corner_radius(6).inner_margin(4).shadow(ui.visuals().popup_shadow))
        .align(egui::RectAlign::BOTTOM_START)
        .gap(4.0)
        .width(w.max(140.0))
        .show(|ui| {
            // menu rows fill the width they get: cap it, or they stretch across the window
            ui.set_min_width(w.max(140.0) - 8.0);
            ui.set_max_width(w.max(140.0) - 8.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        });
}

/// Menu row: 28 px, check mark column, 12.5 text, optional venue logo; hover = hl. True on click.
fn menu_item(ui: &mut Ui, text: &str, on: bool, logo: Option<Exchange>) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() || on { p.rect_filled(r, kit::R, if on { hl() } else { hl().gamma_multiply(0.7) }); }
    let cy = r.center().y;
    if on { p.text(pos2(r.left() + 10.0, cy), Align2::LEFT_CENTER, "✓", prop(12.0), accent()); }
    let mut x = r.left() + 28.0;
    if let Some(e) = logo { icon(p, e, Rect::from_center_size(pos2(x + 7.0, cy), vec2(14.0, 14.0)), false); x += 20.0; }
    p.text(pos2(x, cy), Align2::LEFT_CENTER, text, if text.chars().next().is_some_and(|c| c.is_ascii_digit()) { mono(12.5) } else { prop(12.5) }, if on { fg_strong() } else { fg() });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Minimum USDT notional of the tape filter.
const MIN_USD: [(f64, &str); 5] = [(0.0, ""), (10_000.0, "10K"), (50_000.0, "50K"), (250_000.0, "250K"), (1_000_000.0, "1M")];

/// Formatter for one column: quote amounts as 12.3K / 4.56M, base sizes with the decimals the
/// column's largest value needs (3 significant digits on it), the same for every row.
fn col_fmt(max: f64, quote: bool) -> impl Fn(f64) -> String {
    let d = if max > 0.0 { (3.0 - max.log10().floor()).clamp(0.0, 4.0) as usize } else { 2 };
    move |v| if quote { fmt_big_book(v) } else { fmt_dp(v, d) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth::agg::Tick;

    #[test]
    fn trades_light_the_level_they_ate() {
        let mut a = Agg::default();
        let tick = |ts, px, side| Tick { ts, ex: Exchange::Binance, market: Market::Perp, px, qty: 1.0, side };
        let mut v = BookView { src: Some(Exchange::Binance), ..Default::default() };
        let scope: HashSet<Exchange> = [Exchange::Binance].into();
        let t0 = Instant::now();
        a.trades.push_back(tick(1, 100.3, Side::Buy));
        v.hits(&a, Market::Perp, &scope, 100.0, 1.0, t0);
        assert!(v.flash.is_empty(), "backlog at the first refresh does not flash");
        a.trades.push_back(tick(2, 100.3, Side::Buy));
        a.trades.push_back(tick(2, 100.3, Side::Sell));
        v.hits(&a, Market::Perp, &scope, 100.0, 1.0, t0);
        assert_eq!(v.flash.get(&(true, 101)), Some(&t0), "taker buy eats the ask bin (ceil)");
        assert_eq!(v.flash.get(&(false, 100)), Some(&t0), "taker sell eats the bid bin (floor)");
        let t1 = t0 + Duration::from_millis(100);
        a.trades.push_back(tick(3, 100.6, Side::Buy));
        v.hits(&a, Market::Perp, &scope, 100.0, 1.0, t1);
        assert_eq!(v.flash.get(&(true, 101)), Some(&t0), "throttled within FLASH_GAP");
        let t2 = t0 + Duration::from_millis(350);
        a.trades.push_back(tick(4, 100.6, Side::Buy));
        v.hits(&a, Market::Perp, &scope, 100.0, 1.0, t2);
        assert_eq!(v.flash.get(&(true, 101)), Some(&t2));
    }
}
