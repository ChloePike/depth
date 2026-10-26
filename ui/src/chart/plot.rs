//! The price plot's own layers: footprint, candles and averages, trade markers, account lines,
//! last price and mark, the legend and the toast banner.
use super::*;
use super::fmt::{countdown, fmt_big, fmt_fp, fmt_fp_signed, fmt_time_full};
use super::ind::{bollinger, ema, sma};

impl Chart {
    /// Footprint, candles, moving averages and Bollinger bands, high / low labels, liquidation
    /// bubbles; returns (footprint numbers shown, MA lines, largest liquidation) for later layers.
    pub(super) fn candles(&self, cx: &Cx, legend3: &mut Vec<(String, String, Color32)>) -> (bool, Vec<(Vec<Option<f64>>, MaLine)>, f64) {
        let Cx { a, smarket, bars, vis, i0, i1, main, main_plot, fr, ps, lo, hi, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // footprint (live trades only), cluster bars: each bar is a column of cells, one per price
        // row, filled by that row's taker delta (green buys / red sells, stronger = more one-sided)
        // and labelled with its traded value once wide enough; the row with the most volume is
        // marked; total value above the column, delta below; the candle is a strip at the left
        let foot = a.fp.get(&(self.src, smarket)).filter(|f| self.fp && f.bin > 0.0 && self.bar_w >= 4.0);
        let fp_on = foot.is_some();
        let fp_text = fp_on && self.bar_w >= 42.0;
        if let Some(f) = foot {
            let ms = self.tf * 60_000;
            // rows are a 1-2-5 multiple of the recorded bin: 16 px with numbers, 6 px without
            let px_h = main_plot.height() as f64 / (hi - lo);
            let need = if fp_text { 16.0 } else { 6.0 } / (f.bin * px_h);
            let mult = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0].into_iter().find(|m| *m >= need).unwrap_or(5000.0);
            let row = f.bin * mult;
            let clip = painter.with_clip_rect(main_plot);
            let gold = Color32::from_rgb(0xf0, 0xb9, 0x0b);
            for (k, b) in vis.iter().enumerate() {
                let mut rows: std::collections::BTreeMap<i64, (f64, f64)> = Default::default();
                for (key, (bu, se)) in f.range(b.t, b.t + ms) {
                    let r = rows.entry((key as f64 * f.bin / row).floor() as i64).or_default();
                    r.0 += bu; r.1 += se;
                }
                if rows.is_empty() { continue; }
                // value in quote (USDT), at each row's middle price
                let val = |r: i64, q: f64| q * (r as f64 + 0.5) * row;
                let dmax = rows.iter().map(|(r, (bu, se))| val(*r, (bu - se).abs())).fold(0.0, f64::max).max(1e-12);
                let poc = rows.iter().max_by(|x, z| (x.1.0 + x.1.1).total_cmp(&(z.1.0 + z.1.1))).map(|(r, _)| *r);
                let x = fr.x((i0 + k) as f32);
                let (cx0, cx1) = if self.fp_style == 1 { (x + self.bar_w * 0.02, x + self.bar_w * 0.46) } else { (x - self.bar_w * 0.36, x + self.bar_w * 0.44) };
                let (mut top, mut bot) = (f32::MAX, f32::MIN);
                for (r, (bu, se)) in &rows {
                    let (y0, y1) = (y((*r + 1) as f64 * row), y(*r as f64 * row));
                    let cell = Rect::from_min_max(pos2(cx0, y0 + 0.5), pos2(cx1, (y1 - 0.5).max(y0 + 1.5)));
                    top = top.min(cell.top());
                    bot = bot.max(cell.bottom());
                    let d = bu - se;
                    let strength = (val(*r, d.abs()) / dmax).sqrt() as f32;
                    clip.rect_filled(cell, 1, if d >= 0.0 { up() } else { dn() }.linear_multiply(0.12 + 0.5 * strength));
                    if Some(*r) == poc { clip.rect_filled(Rect::from_min_max(cell.left_top(), pos2(cell.left() + 2.5, cell.bottom())), 0, gold); }
                    if fp_text && cell.height() >= 12.0 {
                        clip.text(cell.center(), Align2::CENTER_CENTER, fmt_fp(val(*r, bu + se)), mono((cell.height() * 0.62).clamp(9.0, 11.5)), Color32::from_white_alpha(235));
                    }
                }
                if self.bar_w >= 26.0 && top < bot {
                    let tot: f64 = rows.iter().map(|(r, (bu, se))| val(*r, bu + se)).sum();
                    let d: f64 = rows.iter().map(|(r, (bu, se))| val(*r, bu - se)).sum();
                    let fs = if self.bar_w >= 42.0 { 10.5 } else { 9.0 };
                    clip.text(pos2((cx0 + cx1) / 2.0, top - 3.0), Align2::CENTER_BOTTOM, fmt_fp(tot), mono(fs), mu());
                    clip.text(pos2((cx0 + cx1) / 2.0, bot + 3.0), Align2::CENTER_TOP, fmt_fp_signed(d), mono(fs), if d >= 0.0 { up() } else { dn() });
                }
            }
        }

        // candles
        // with the footprint: a strip left of the column (style 0), or a normal candle left of the cells (style 1)
        let (bw, shift) = match (fp_on, self.fp_style) {
            (false, _) => ((self.bar_w * 0.7).max(1.0), 0.0),
            (true, 1) => ((self.bar_w * 0.3).max(1.0), self.bar_w * 0.2),
            (true, _) => ((self.bar_w * 0.06).clamp(1.5, 4.0), self.bar_w * 0.42),
        };
        for (k, b) in vis.iter().enumerate() {
            let x = fr.x((i0 + k) as f32) - shift;
            let col = if b.c >= b.o { up() } else { dn() };
            painter.vline(x, y(b.h)..=y(b.l), Stroke::new(1.0, col));
            let (top, bot) = (y(b.o.max(b.c)), y(b.o.min(b.c)));
            painter.rect_filled(Rect::from_min_max(pos2(x - bw / 2.0, top), pos2(x + bw / 2.0, bot.max(top + 1.0))), 0, col);
        }

        // moving averages of the close, computed over every loaded bar so the left edge is right
        let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
        let lines: Vec<MaLine> = if self.ma { self.mas.iter().filter(|m| m.on && m.n >= 1).copied().collect() } else { vec![] };
        let mas: Vec<(Vec<Option<f64>>, MaLine)> = lines.iter().map(|m| (if m.ema { ema(&closes, m.n) } else { sma(&closes, m.n) }, *m)).collect();
        for (m, l) in &mas {
            let col = rgb(l.col);
            let pts: Vec<Pos2> = (i0..=i1).filter_map(|i| Some(pos2(fr.x(i as f32), y(m[i]?)))).collect();
            if pts.len() > 1 { painter.with_clip_rect(main_plot).add(egui::Shape::line(pts, Stroke::new(1.2, col))); }
        }
        if self.bb {
            let bb = bollinger(&closes, self.ind.bb_n.max(1), self.ind.bb_k);
            if let Some((m, w)) = bb.last().copied().flatten() {
                legend3.push((format!("BB {} {}", self.ind.bb_n, self.ind.bb_k), format!("{} – {} – {}", fmt_px(m - w), fmt_px(m), fmt_px(m + w)), rgb(0x2962ff)));
            }
            let pts = |f: &dyn Fn(f64, f64) -> f64| -> Vec<Pos2> { (i0..=i1).filter_map(|i| { let (m, w) = bb[i]?; Some(pos2(fr.x(i as f32), y(f(m, w)))) }).collect() };
            let (up_, mid, lo_) = (pts(&|m, w| m + w), pts(&|m, _| m), pts(&|m, w| m - w));
            let clip = painter.with_clip_rect(main_plot);
            let band = rgb(0x2962ff);
            // fill as one quad per bar step: a single concave polygon would tessellate wrongly
            for k in 1..up_.len().min(lo_.len()) {
                clip.add(egui::Shape::convex_polygon(vec![up_[k - 1], up_[k], lo_[k], lo_[k - 1]], band.gamma_multiply(0.04), Stroke::NONE));
            }
            for (p, c) in [(up_, band), (lo_, band), (mid, rgb(0xff6d00))] { if p.len() > 1 { clip.add(egui::Shape::line(p, Stroke::new(1.0, c))); } }
        }

        // highest high / lowest low of the visible range, Binance-style arrow labels
        let hi_k = (0..vis.len()).max_by(|x, z| vis[*x].h.total_cmp(&vis[*z].h));
        let lo_k = (0..vis.len()).min_by(|x, z| vis[*x].l.total_cmp(&vis[*z].l));
        for (k, v, above) in [(hi_k, vis.iter().map(|b| b.h).fold(f64::MIN, f64::max), true), (lo_k, vis.iter().map(|b| b.l).fold(f64::MAX, f64::min), false)] {
            let Some(k) = k else { continue };
            let x = fr.x((i0 + k) as f32);
            let yy = y(v) + if above { -8.0 } else { 8.0 };
            // point the label toward the chart's middle so it never runs off the right edge
            let left = x > main.center().x;
            let (end, align) = if left { (x - 22.0, Align2::RIGHT_CENTER) } else { (x + 22.0, Align2::LEFT_CENTER) };
            painter.line_segment([pos2(x, yy), pos2(end, yy)], Stroke::new(1.0, mu()));
            painter.text(pos2(end + if left { -3.0 } else { 3.0 }, yy), align, fmt_px(v), mono(10.5), fg());
        }

        // liquidation bubbles: longs liquidated under the candle, shorts above
        let lmax = vis.iter().map(|b| b.liq_long.max(b.liq_short)).fold(0.0, f64::max);
        if lmax > 0.0 {
            for (k, b) in vis.iter().enumerate() {
                let x = fr.x((i0 + k) as f32);
                for (q, py, col, dir) in [(b.liq_long, b.l, dn(), 1.0), (b.liq_short, b.h, up(), -1.0)] {
                    if q <= 0.0 { continue; }
                    let r = 3.0 + 11.0 * (q / lmax).sqrt() as f32;
                    painter.circle_filled(pos2(x, y(py) + dir * (r + 3.0)), r, col.linear_multiply(0.55));
                }
            }
        }
        (fp_text, mas, lmax)
    }

    /// Whale trades, likely algos and own fills.
    pub(super) fn markers(&self, ui: &Ui, cx: &Cx, fp_text: bool, ba_rect: Option<Rect>) {
        let Cx { a, market, smarket, bars, i0, i1, main_plot, fr, ps, resp, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // whale trades: one bubble per taker order (fills merged), area ~ USD size, at its own price
        if self.whales && !fp_text {
            let ms = self.tf * 60_000;
            let big: Vec<(f32, f64, f64, bool)> = a.big.iter()
                .filter(|t| t.market == smarket && self.src.is_none_or(|e| e == t.ex))
                .filter_map(|t| {
                    let k = a.usdt(t.ex, t.market);
                    let usd = t.px * t.qty * k;
                    let i = idx_of(&bars, t.ts, ms);
                    (usd >= self.whale_usd && i >= i0 as f32 - 0.5 && i <= i1 as f32 + 0.5).then(|| (i, if self.src.is_some() { t.px } else { t.px * k }, usd, t.side == depth::Side::Buy))
                }).collect();
            let top = big.iter().map(|b| b.2).fold(0.0, f64::max);
            let clip = painter.with_clip_rect(main_plot);
            for (i, px, usd, buy) in big {
                let r = 3.0 + 13.0 * (usd / top).sqrt() as f32;
                let c = if buy { up() } else { dn() };
                clip.circle(pos2(fr.x(i.round()), y(px)), r, c.linear_multiply(0.35), Stroke::new(1.0, c));
            }
        }

        // likely algos (engine pass every 5 s): an iceberg as a dotted level from its first fill with a
        // small square at its start, every slicer clip as a small diamond at its own price; nothing
        // over the bid/ask box (the names live in the hover)
        if self.algos && !fp_text {
            let ms = self.tf * 60_000;
            // the last 30 min of detections; a running iceberg extends to the right edge, a finished one
            // ends at its last fill
            let (clips, ices) = (a.scan.seen.0.values(), a.scan.seen.1.values());
            let live: std::collections::HashSet<String> = a.scan.found.1.iter().map(depth::algoscan::ice_key).collect();
            let k = |e: Exchange| if self.src.is_some() { 1.0 } else { a.usdt(e, smarket) };
            let mine = |e: Exchange, m: Market| m == smarket && self.src.is_none_or(|s| s == e);
            let clip = painter.with_clip_rect(main_plot);
            let hp = resp.hover_pos().filter(|p| main_plot.contains(*p));
            let mut tip = None;
            for i in ices.filter(|i| mine(i.ex, i.market)) {
                let (py, col) = (y(i.px * k(i.ex)), if i.bid { up() } else { dn() });
                let x0 = fr.x(idx_of(&bars, i.first, ms)).max(main_plot.left());
                let x1 = if live.contains(&depth::algoscan::ice_key(i)) { main_plot.right() } else { fr.x(idx_of(&bars, i.last, ms)).max(x0 + 24.0) };
                if x1 < main_plot.left() { continue; }
                dashed(&clip, x0, x1, py, col);
                let sq = Rect::from_center_size(pos2(x0, py), vec2(7.0, 7.0));
                if !ba_rect.is_some_and(|b| b.intersects(sq)) { clip.rect(sq, 1, col, Stroke::new(1.0, bg()), egui::StrokeKind::Outside); }
                if hp.is_some_and(|p| p.x >= x0 && p.x <= x1 && (p.y - py).abs() <= 4.0) {
                    tip = Some(format!("{:?} · {} @ {}\n{} {} · {} {} · {} {}", i.ex, t(if i.bid { "algo.ice_bid" } else { "algo.ice_ask" }), fmt_px(i.px),
                        t("algo.traded"), crate::fmt_qty(i.traded), t("algo.shown"), crate::fmt_qty(i.shown_max), i.fills, t("algo.fills")));
                }
            }
            for c in clips.filter(|c| mine(c.ex, c.market)) {
                let col = if c.buy { up() } else { dn() };
                for &(ts, px) in &c.pts {
                    let i = idx_of(&bars, ts, ms);
                    if i < i0 as f32 - 0.5 || i > i1 as f32 + 0.5 { continue; }
                    let p = pos2(fr.x(i), y(px * k(c.ex)));
                    if ba_rect.is_some_and(|b| b.expand(3.0).contains(p)) { continue; }
                    clip.add(egui::Shape::convex_polygon(vec![p + vec2(0.0, -3.5), p + vec2(3.5, 0.0), p + vec2(0.0, 3.5), p + vec2(-3.5, 0.0)], col, Stroke::new(1.0, bg())));
                    if hp.is_some_and(|h| h.distance(p) <= 6.0) {
                        tip = Some(format!("{:?} · {}\n{} · {:.0}s · {} {}", c.ex, t(if c.buy { "algo.slicer_buy" } else { "algo.slicer_sell" }), crate::fmt_qty(c.qty), c.period_ms / 1e3, c.count, t("algo.clips")));
                    }
                }
            }
            if let Some(s) = tip {
                egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), ui.id().with("algotip"), egui::PopupAnchor::Pointer)
                    .show(|ui| ui.label(egui::RichText::new(s).font(mono(11.5)).color(fg())));
            }
        }

        // own fills: one B / S tag per bar and side at the size-weighted price, B under it, S over it
        if self.fills_on && market == Market::Perp && !self.fills.is_empty() {
            let ms = self.tf * 60_000;
            // (bar, buy) -> (sum px*qty, qty, fee, count, venues)
            let mut g: std::collections::BTreeMap<(i64, bool), (f64, f64, f64, usize, Vec<Exchange>)> = Default::default();
            for f in &self.fills {
                let i = idx_of(&bars, f.ts, ms);
                if i < i0 as f32 - 0.5 || i > i1 as f32 + 0.5 { continue; }
                let px = if self.src.is_some() { f.price } else { f.price * a.usdt(f.ex, Market::Perp) };
                let e = g.entry((i.round() as i64, f.side == depth::Side::Buy)).or_default();
                (e.0, e.1, e.2, e.3) = (e.0 + px * f.qty, e.1 + f.qty, e.2 + f.fee, e.3 + 1);
                if !e.4.contains(&f.ex) { e.4.push(f.ex); }
            }
            let clip = painter.with_clip_rect(main_plot);
            let mut tip = None;
            for ((i, buy), (pq, q, fee, cnt, exs)) in g {
                let vwap = pq / q.max(1e-12);
                let (x, py) = (fr.x(i as f32), y(vwap));
                let col = if buy { up() } else { dn() };
                // a 3 px pointer at the price, the 14 px tag beyond it
                let dir = if buy { 1.0 } else { -1.0 };
                let tag = Rect::from_center_size(pos2(x, py + dir * 12.0), vec2(14.0, 14.0));
                clip.add(egui::Shape::convex_polygon(vec![pos2(x, py + dir * 2.0), pos2(x - 4.0, py + dir * 5.5), pos2(x + 4.0, py + dir * 5.5)], col, Stroke::NONE));
                clip.rect_filled(tag, 3, col);
                clip.text(tag.center(), Align2::CENTER_CENTER, if buy { "B" } else { "S" }, prop_s(10.0), Color32::WHITE);
                if resp.hover_pos().is_some_and(|p| tag.expand(2.0).contains(p)) {
                    let venues = exs.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(", ");
                    tip = Some(format!("{venues} · {} {}\n@ {} · {cnt}× · fee {}", if buy { t("tr.buy") } else { t("tr.sell") }, crate::fmt_qty(q), fmt_px(vwap), crate::fmt_qty(fee)));
                }
            }
            if let Some(s) = tip {
                egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), ui.id().with("filltip"), egui::PopupAnchor::Pointer)
                    .show(|ui| ui.label(egui::RichText::new(s).font(mono(11.5)).color(fg())));
            }
        }
    }

    /// Account lines (positions, orders, TP / SL): drag to move, x to cancel, and the confirmation.
    pub(super) fn account_lines(&mut self, ui: &Ui, cx: &Cx, mut taken: &mut Vec<Rect>) {
        let Cx { live, main, main_plot, ps, lo, hi, resp, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // account lines: positions and resting orders of this symbol
        let mut line_hot = None;
        let hover_p = resp.hover_pos().filter(|p| main_plot.contains(*p));
        for (li, l) in self.lines.iter().enumerate().filter(|_| live) {
            if !(l.price > lo && l.price < hi) { continue; }
            let ly = y(l.price);
            if let (Some(_), Some(p), None) = (&l.act, hover_p, self.tool) { if (p.y - ly).abs() < 5.0 { line_hot = Some(li); } }
            dashed(&painter, main.left(), main.right(), ly, l.col.linear_multiply(0.8));
            let g1 = painter.layout_no_wrap(l.tag.clone(), prop(11.0), Color32::WHITE);
            let g2 = painter.layout_no_wrap(l.sub.clone(), mono(11.0), l.col);
            // right of the POC / VAH / S tags at the left edge, so the two never stack
            let x0 = main.left() + 80.0;
            // venue logo in front of the tag (the tag may be empty: logo only)
            let lw = if l.ex.is_some() { if l.tag.is_empty() { 14.0 } else { 18.0 } } else { 0.0 };
            let w1 = lw + g1.size().x + if l.tag.is_empty() { 8.0 } else { 14.0 };
            // resting orders and TP / SL get a cancel box at the right end
            let xw = if matches!(l.act, Some(LineAct::Order(..) | LineAct::TpSl(..))) { 20.0 } else { 0.0 };
            let both = place(Rect::from_min_size(pos2(x0, ly - 10.0), vec2(w1 + g2.size().x + 14.0 + xw, 20.0)), &mut taken);
            let r1 = Rect::from_min_size(both.min, vec2(w1, 20.0));
            let r2 = Rect::from_min_size(pos2(r1.right(), both.top()), vec2(g2.size().x + 14.0 + xw, 20.0));
            painter.rect_filled(r1.union(r2), 3, label_bg());
            let xr = Rect::from_min_max(pos2(r2.right() - xw, r2.top()), r2.max);
            let x_hot = xw > 0.0 && hover_p.is_some_and(|p| xr.contains(p));
            if xw > 0.0 {
                if x_hot { painter.rect_filled(xr.shrink(2.0), 2, l.col.gamma_multiply(0.25)); ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                let (c, d) = (xr.center(), 3.5);
                for (a, b) in [(vec2(-d, -d), vec2(d, d)), (vec2(-d, d), vec2(d, -d))] { painter.line_segment([c + a, c + b], Stroke::new(1.3, l.col)); }
                if x_hot && resp.clicked() {
                    let act = l.act.clone().unwrap();
                    if self.confirm { self.pending_move = Some((act, format!("{} {}", l.tag, l.sub).trim().to_string(), l.price, None, xr.center())); } else { self.actions.push(ChartAct::Cancel(act)); }
                }
            }
            painter.rect_filled(r1, egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 }, l.col);
            painter.rect_stroke(r1.union(r2), 3, Stroke::new(1.0, l.col), egui::StrokeKind::Inside);
            if let Some(e) = l.ex { icon(&painter, e, Rect::from_center_size(pos2(r1.left() + 4.0 + 7.0, r1.center().y), vec2(14.0, 14.0)), false); }
            painter.galley(pos2(r1.left() + (if l.ex.is_some() { 4.0 + lw } else { 7.0 }), r1.center().y - g1.size().y / 2.0), g1, Color32::WHITE);
            painter.galley(pos2(r2.left() + 7.0, r2.center().y - g2.size().y / 2.0), g2, l.col);
            // the price is in the label (@price): no axis tag, they piled up on the right
            if l.act.is_some() && !x_hot && hover_p.is_some_and(|p| both.contains(p)) { line_hot = Some(li); }
        }
        // dragging an order / TP / SL line: a ghost follows the pointer; release moves it (after a
        // confirmation when order confirmations are on)
        let ghost = |px: f64, col: Color32| {
            let gy = y(px);
            painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), gy), pos2(main.right(), gy)], Stroke::new(1.5, col), 6.0, 3.0));
            let ax = Rect::from_min_size(pos2(main.right() + 2.0, gy - 9.0), vec2(AXIS_W - 4.0, 18.0));
            painter.rect_filled(ax, 3, col);
            painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(px), mono(11.0), Color32::WHITE);
        };
        if let Some(li) = self.line_drag {
            match (self.lines.get(li), resp.interact_pointer_pos()) {
                (Some(l), Some(p)) if l.act.is_some() => {
                    let np = ps.v(p.y.clamp(main_plot.top(), main_plot.bottom()));
                    ghost(np, l.col);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    if resp.drag_stopped() {
                        if (y(np) - y(l.price)).abs() > 2.0 {
                            let act = l.act.clone().unwrap();
                            if self.confirm && !matches!(act, LineAct::Preview(_)) { self.pending_move = Some((act, l.tag.clone(), l.price, Some(np), p)); } else { self.actions.push(ChartAct::Move(act, np)); }
                        }
                        self.line_drag = None;
                    }
                }
                _ => self.line_drag = None,
            }
        } else if line_hot.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            resp.clone().on_hover_text(t("tr.drag_line"));
        }
        self.line_hot = line_hot;
        if let Some((act, tag, from, to, at)) = self.pending_move.clone() {
            if let Some(to) = to { ghost(to, accent()); }
            let (mut open, mut done) = (true, None);
            egui::Popup::menu(&resp).id(ui.id().with("move_confirm")).at_position(at).open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let what = match to { Some(to) => format!("{tag}:  {} → {}", fmt_px(from), fmt_px(to)), None => tag.clone() };
                ui.label(RichText::new(what).font(mono_s(12.5)).color(fg_strong()));
                // only Bybit / Binance orders are amended in place; elsewhere the order is cancelled and placed again
                if to.is_some() && matches!(act, LineAct::Order(..)) && !self.amends { kit::note(ui, t("tr.move_replace"), WARN); }
                ui.horizontal(|ui| {
                    let (ok, col) = if to.is_some() { (t("tr.move"), accent()) } else { (t("tr.cancel_order"), dn()) };
                    if kit::button(ui, ok, kit::Look::Fill(col), kit::H, None, true).clicked() { done = Some(true); ui.close(); }
                    if kit::button(ui, t("tr.cancel_action"), kit::Look::Secondary, kit::H, None, true).clicked() { done = Some(false); ui.close(); }
                });
            });
            if done == Some(true) { self.actions.push(match to { Some(to) => ChartAct::Move(act, to), None => ChartAct::Cancel(act) }); }
            if !open || done.is_some() { self.pending_move = None; }
        }
    }

    /// Last price with the bar-close countdown and the bid / ask box, then the mark price.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn last_price(&self, cx: &Cx, last_bar: &Bar, last_ly: f32, ba: Option<(f64, f64)>, ba_rect: Option<Rect>, mark: Option<f64>, mark_ax: Option<Rect>, legend3: &mut Vec<(String, String, Color32)>) {
        let Cx { live, main, ps, painter, .. } = *cx;
        let y = |v: f64| ps.y(v);
        // last price: dashed line; axis tag with the bar-close countdown; bid/ask box beside it
        let last = last_bar;
        let ly = last_ly;
        let lc = if last.c >= last.o { up() } else { dn() };
        dashed(&painter, main.left(), main.right(), ly, lc.linear_multiply(0.7));
        let lab = Rect::from_min_size(pos2(main.right() + 2.0, ly - 10.0), vec2(AXIS_W - 4.0, 34.0));
        painter.rect_filled(lab, 3, lc);
        painter.text(pos2(lab.left() + 6.0, ly), Align2::LEFT_CENTER, fmt_px(last.c), mono_s(11.5), Color32::WHITE);
        let left_ms = last.t + self.tf * 60_000 - depth::now_ms();
        if live { painter.text(pos2(lab.left() + 6.0, ly + 15.0), Align2::LEFT_CENTER, countdown(left_ms), mono_m(10.5), Color32::from_white_alpha(210)); }
        if let (Some((bid, ask)), Some(bx)) = (ba, ba_rect) {
            let (ka, kb) = (t("tr.ask"), t("tr.bid"));
            let w = bx.width();
            for (i, (k, v, col)) in [(ka, ask, dn()), (kb, bid, up())].into_iter().enumerate() {
                let r = Rect::from_min_size(pos2(bx.left(), bx.top() + i as f32 * 15.0), vec2(w, 15.0));
                painter.rect_filled(r.shrink2(vec2(0.0, 0.5)), 2, col.linear_multiply(0.85));
                painter.text(r.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, k, prop(10.0), Color32::WHITE);
                painter.text(r.right_center() - vec2(5.0, 0.0), Align2::RIGHT_CENTER, fmt_px(v), mono(10.0), Color32::WHITE);
            }
        }

        // mark price: the charted venue's, or the OI-weighted mark across perp venues (USDT) on the aggregate
        if let Some(mk) = mark { legend3.push((t("chart.mark").into(), fmt_px(mk), PURPLE)); }
        if let (Some(mk), Some(ax)) = (mark, mark_ax) {
            let my = y(mk);
            painter.extend(egui::Shape::dashed_line(&[pos2(main.left(), my), pos2(main.right(), my)], Stroke::new(1.0, PURPLE.linear_multiply(0.75)), 2.0, 3.0));
            painter.rect_filled(ax, 3, PURPLE.linear_multiply(0.9));
            painter.text(ax.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, fmt_px(mk), mono_m(11.0), Color32::WHITE);
        }
    }

    /// Legend rows above the plot: the hovered bar's OHLC, MAs and liquidations, overlay values.
    pub(super) fn legend(&self, cx: &Cx, hi_idx: usize, mas: Vec<(Vec<Option<f64>>, MaLine)>, lmax: f64, legend3: Vec<(String, String, Color32)>) {
        let Cx { loading_tf, bars, main, painter, .. } = *cx;
        // legend: row 1 time + OHLC + change + range (labels muted, values colored), row 2 MAs
        let b = &bars[hi_idx];
        let chg = (b.c / b.o - 1.0) * 100.0;
        let range = if b.l > 0.0 { (b.h / b.l - 1.0) * 100.0 } else { 0.0 };
        let c = if b.c >= b.o { up() } else { dn() };
        // design: text-only rows, 12 medium, grey keys, colored tabular values, 14 px apart
        let kv = |x: f32, yy: f32, k: &str, v: String, col: Color32| -> f32 {
            let r = painter.text(pos2(x, yy), Align2::LEFT_CENTER, k, prop_m(12.0), dim());
            painter.text(pos2(r.right() + 4.0, yy), Align2::LEFT_CENTER, v, mono_m(12.0), col).right() + 14.0
        };
        let y1 = main.top() + 14.0;
        let mut lx = main.left() + 6.0;
        if self.multi {
            let tf = TFS.iter().find(|(m, _)| *m == self.tf).map_or("", |x| x.1);
            lx = painter.text(pos2(lx, y1), Align2::LEFT_CENTER, tf, prop_s(12.5), fg_strong()).right() + 12.0;
        }
        lx = painter.text(pos2(lx, y1), Align2::LEFT_CENTER, fmt_time_full(b.t), mono_m(12.0), mu()).right() + 14.0;
        for (k, v) in [("chart.o", b.o), ("chart.h", b.h), ("chart.l", b.l), ("chart.c", b.c)] { lx = kv(lx, y1, t(k), fmt_px(v), c); }
        lx = kv(lx, y1, t("chart.chg"), format!("{chg:+.2}%"), c);
        kv(lx, y1, t("chart.range"), format!("{range:.2}%"), c);
        let y2 = main.top() + 33.0;
        let mut lx = main.left() + 6.0;
        for (m, l) in &mas {
            lx = kv(lx, y2, &format!("{}({})", if l.ema { "EMA" } else { "MA" }, l.n), m[hi_idx].map(fmt_px).unwrap_or("-".into()), rgb(l.col));
        }
        if lmax > 0.0 {
            kv(lx, y2, t("chart.liq"), format!("{} / {}", fmt_big(b.liq_long), fmt_big(b.liq_short)), dim());
        }
        // the overlay row moves up when there is no MA / liquidation row
        let y3 = main.top() + if mas.is_empty() && lmax <= 0.0 { 33.0 } else { 52.0 };
        let mut lx = main.left() + 6.0;
        for (k, v, col) in &legend3 {
            painter.circle_filled(pos2(lx + 3.0, y3), 3.0, *col);
            lx = kv(lx + 10.0, y3, k, v.clone(), *col);
        }
        if loading_tf {
            painter.text(pos2(main.left() + 6.0, main.top() + 80.0), Align2::LEFT_CENTER, t("chart.loading"), prop_m(12.0), WARN);
        }
    }

    /// The toast banner at the top of the plot (alerts, venue answers, fills).
    pub(super) fn paint_toast(&mut self, ui: &Ui, cx: &Cx) {
        let Cx { main_plot, painter, .. } = *cx;
        if let Some((msg, until, dot)) = self.toast.clone() {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if !left.is_zero() {
                // slides down and fades in over 250 ms, fades out over the last 400 ms
                let age = 8.0 - left.as_secs_f32();
                let t = (age / 0.25).min(1.0).min(left.as_secs_f32() / 0.4);
                let ease = 1.0 - (1.0 - t).powi(3);
                // design banner: popup surface, accent dot, 12.5 semibold
                let g = painter.layout_no_wrap(msg, prop_s(12.5), fg_strong().gamma_multiply(ease));
                let r = Rect::from_center_size(pos2(main_plot.center().x, main_plot.top() + 6.0 + 14.0 * ease), g.size() + vec2(42.0, 20.0));
                painter.add(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(((if light() { 40.0 } else { 115.0 }) * ease) as u8) }.as_shape(r, 6));
                painter.rect(r, 6, popup_bg().gamma_multiply(ease), Stroke::new(1.0, line().gamma_multiply(ease)), egui::StrokeKind::Inside);
                painter.circle_filled(pos2(r.left() + 16.0, r.center().y), 4.0, dot.gamma_multiply(ease));
                painter.galley(pos2(r.left() + 30.0, r.center().y - g.size().y / 2.0), g, fg_strong().gamma_multiply(ease));
                if t < 1.0 { ui.ctx().request_repaint(); } else { ui.ctx().request_repaint_after(left.saturating_sub(std::time::Duration::from_millis(400))); }
            } else { self.toast = None; }
        }
    }
}
