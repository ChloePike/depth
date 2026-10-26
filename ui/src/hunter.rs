//! Hunter page (title bar pill after the markets): the market-wide scanner's table (engine `Hunt`,
//! rebuilt every 5 s in the background whatever is on screen), its filters, alert rules and log.
//! Facts only for now: the Hit rate column waits for the detectors' backtest; the Operator column
//! classifies coins (it does not time moves). A row click opens that coin in Perpetual. Nothing here
//! asks for repaints: the scanner does, once per rebuild.
use super::{hms, kit, t, theme::*, top::{big, signed}};
use crate::engine::Hunt;
use depth::hunter::{Alerts, Op, Row, LIQ_VENUES, VENUES};
use depth::{now_ms, Exchange};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Rect, RichText, Sense, Stroke, Ui};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Col { Coin, M5, M15, H1, VMult, Vol, Fund, Oi15, Oi1h, Liq, Gap, Lead, Op, Hit }

/// (column, title key, header tooltip key, width)
const COLS: [(Col, &str, &str, f32); 14] = [
    (Col::Coin, "hun.coin", "hun.tip_coin", 150.0),
    (Col::M5, "hun.5m", "hun.tip_chg", 66.0), (Col::M15, "hun.15m", "hun.tip_chg", 66.0), (Col::H1, "hun.1h", "hun.tip_chg", 66.0),
    (Col::VMult, "hun.vmult", "hun.tip_vmult", 66.0), (Col::Vol, "hun.vol24", "hun.tip_vol", 84.0), (Col::Fund, "hun.funding", "hun.tip_fund", 116.0),
    (Col::Oi15, "hun.oi15", "hun.tip_oi", 72.0), (Col::Oi1h, "hun.oi1h", "hun.tip_oi", 72.0), (Col::Liq, "hun.liq15", "hun.tip_liq", 84.0),
    (Col::Gap, "hun.gap", "hun.tip_gap", 150.0), (Col::Lead, "hun.lead", "hun.tip_lead", 100.0),
    (Col::Op, "hun.op", "hun.tip_op", 84.0), (Col::Hit, "hun.hit", "hun.tip_hit", 66.0),
];
/// alert rules: (label key, unit), in `Alerts::rules` order
const RULES: [(&str, &str); 4] = [("hun.r_move", "%"), ("hun.r_vmult", "×"), ("hun.r_fund", "%/h"), ("hun.r_gap", "bp")];
const ROW_H: f32 = 28.0;
const SIDE: f32 = 270.0;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HunterView {
    /// the page is shown in place of the chart, book and order panel
    pub open: bool,
    /// alert rules and the minimum 24 h volume (the table's filter too), copied to the engine
    pub alerts: Alerts,
    /// venues whose coins are hidden
    pub hidden: Vec<Exchange>,
    /// lowest operator-type score shown (0 = every coin)
    pub op_min: f64,
    sort: (Col, bool),
    #[serde(skip)] query: String,
    /// typed text of the volume floor (M USD), the four thresholds and the score floor
    #[serde(skip)] drafts: Option<[String; 6]>,
    /// the top rows last sent to the engine (`Hunt::want`)
    #[serde(skip)] want: Vec<String>,
}

impl Default for HunterView {
    fn default() -> Self { HunterView { open: false, alerts: Alerts::default(), hidden: vec![], op_min: 0.0, sort: (Col::Vol, false), query: String::new(), drafts: None, want: vec![] } }
}

fn key(r: &Row, c: Col) -> Option<f64> {
    match c {
        Col::M5 => r.chg[0], Col::M15 => r.chg[1], Col::H1 => r.chg[2], Col::VMult => r.vmult, Col::Vol => Some(r.vol24),
        Col::Fund => r.fund.map(|f| f.1), Col::Oi15 => r.oi_chg[0], Col::Oi1h => r.oi_chg[1], Col::Liq => r.liq15,
        Col::Gap => r.gap.map(|g| g.bp), Col::Lead => r.lead.map(|l| l.secs), Col::Op => score(r), Col::Coin | Col::Hit => None,
    }
}

fn score(r: &Row) -> Option<f64> { match &r.op { Some(Op::Score { score, .. }) => Some(*score), _ => None } }

/// Rows shown: above the volume floor, listed on a venue not hidden, matching the text, scored at
/// least `op_min` (when above 0); sorted by the column (blanks last either way, then by volume).
fn visible<'a>(rows: &'a [Row], min_vol: f64, hidden: &[Exchange], query: &str, op_min: f64, sort: (Col, bool)) -> Vec<&'a Row> {
    let q = query.trim().to_uppercase();
    let mut v: Vec<&Row> = rows.iter().filter(|r| r.vol24 >= min_vol && r.venues.iter().any(|e| !hidden.contains(e)) && r.base.to_uppercase().contains(&q)
        && (op_min <= 0.0 || score(r).is_some_and(|x| x >= op_min))).collect();
    v.sort_by(|a, b| {
        if sort.0 == Col::Coin { let o = a.base.cmp(&b.base); return if sort.1 { o } else { o.reverse() }; }
        match (key(a, sort.0), key(b, sort.0)) {
            (Some(x), Some(y)) => if sort.1 { x.total_cmp(&y) } else { y.total_cmp(&x) },
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.vol24.total_cmp(&a.vol24),
        }
    });
    v
}

/// A number as typed into a field: "5", "0.05", "1,000".
fn typed(s: &str) -> Option<f64> { s.replace(',', "").trim().parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0) }
fn shown(x: f64) -> String { format!("{}", (x * 1e6).round() / 1e6) }
fn tag(spot: bool) -> &'static str { if spot { "S" } else { "P" } }
fn mkt(spot: bool) -> &'static str { t(if spot { "hun.spot" } else { "hun.perp" }) }

impl HunterView {
    /// The rules into the engine (at start; the page does it after every edit).
    pub fn sync(&self, hunt: &Mutex<Hunt>) { hunt.lock().unwrap().alerts = self.alerts; }

    /// The page in the space left in `ui`; returns the coin clicked. `on`: the scanner runs;
    /// `off`: venues switched off in the status bar.
    pub fn show(&mut self, ui: &mut Ui, hunt: &Mutex<Hunt>, on: bool, off: &HashSet<Exchange>) -> Option<String> {
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        if !on {
            ui.painter().text(full.center(), Align2::CENTER_CENTER, t("hun.off"), prop(13.0), mu());
            return None;
        }
        let (rows, at, log, venue) = {
            let h = hunt.lock().unwrap();
            (h.rows.clone(), h.at, h.log.iter().rev().take(100).cloned().collect::<Vec<_>>(), h.venue.clone())
        };
        let a = self.alerts;
        let op_min = self.op_min;
        self.drafts.get_or_insert_with(|| [shown(a.min_vol / 1e6), shown(a.rules[0].1), shown(a.rules[1].1), shown(a.rules[2].1), shown(a.rules[3].1), shown(op_min)]);
        let vis = visible(&rows, a.min_vol, &self.hidden, &self.query, op_min, self.sort);
        // the top rows' coins get Binance's hourly history while they are still collecting
        let want: Vec<String> = vis.iter().take(30).map(|r| r.base.clone()).collect();
        if want != self.want { hunt.lock().unwrap().want = want.clone(); self.want = want; }
        let side = Rect::from_min_max(pos2(full.right() - SIDE, full.top()), full.max);
        let main = Rect::from_min_max(full.min, pos2(side.left() - 14.0, full.bottom()));
        let mut s = ui.new_child(egui::UiBuilder::new().id_salt("hun_side").max_rect(side));
        s.set_clip_rect(side.intersect(ui.clip_rect()));
        self.side(&mut s, &log);
        ui.painter().vline(side.left() - 7.0, full.y_range(), Stroke::new(1.0, line()));
        let mut m = ui.new_child(egui::UiBuilder::new().id_salt("hun_main").max_rect(main));
        m.set_clip_rect(main.intersect(ui.clip_rect()));
        let picked = self.table(&mut m, &vis, rows.is_empty(), at, &venue, off);
        if self.alerts != a { self.sync(hunt); }
        picked
    }

    /// Alert rules (switch, threshold) and the alert log, newest first.
    fn side(&mut self, ui: &mut Ui, log: &[(i64, String, String)]) {
        ui.spacing_mut().item_spacing.y = 8.0;
        kit::overline(ui, t("hun.alerts"));
        let d = self.drafts.as_mut().expect("drafts");
        for (i, (label, unit)) in RULES.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.set_height(kit::H);
                kit::switch(ui, &mut self.alerts.rules[i].0);
                let bad = typed(&d[i + 1]).is_none();
                let (r, _) = ui.allocate_ui(vec2(ui.available_width(), kit::H), |ui| kit::field(ui, t(label), &mut d[i + 1], unit, ("hun_rule", i), bad)).inner;
                if r.changed() { if let Some(x) = typed(&d[i + 1]) { self.alerts.rules[i].1 = x; } }
            });
        }
        kit::note(ui, t("hun.alerts_note"), dim());
        ui.add_space(6.0);
        kit::overline(ui, t("hun.log"));
        if log.is_empty() { kit::note(ui, t("hun.no_alerts"), dim()); return; }
        egui::ScrollArea::vertical().id_salt("hun_log").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for (ts, coin, text) in log {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(hms(*ts)).font(mono(11.5)).color(dim()));
                    ui.label(RichText::new(coin).font(prop_s(12.0)).color(fg_strong()));
                });
                kit::note(ui, text, fg());
            }
        });
    }

    /// Filters, header and the rows (virtualized); returns the coin clicked.
    fn table(&mut self, ui: &mut Ui, vis: &[&Row], loading: bool, at: i64, venue: &HashMap<Exchange, (i64, Option<String>)>, off: &HashSet<Exchange>) -> Option<String> {
        let now = now_ms();
        ui.horizontal(|ui| {
            ui.set_height(kit::H);
            ui.spacing_mut().item_spacing.x = 6.0;
            let d = self.drafts.as_mut().expect("drafts");
            let bad = typed(&d[0]).is_none();
            let (r, _) = ui.allocate_ui(vec2(196.0, kit::H), |ui| kit::field(ui, t("hun.min_vol"), &mut d[0], "M USD", "hun_minvol", bad)).inner;
            if r.changed() { if let Some(x) = typed(&d[0]) { self.alerts.min_vol = x * 1e6; } }
            ui.add_space(6.0);
            for e in VENUES {
                let hide = self.hidden.contains(&e);
                let (col, why) = match venue.get(&e) {
                    _ if off.contains(&e) => (dim(), t("hun.v_off").to_string()),
                    Some((_, Some(err))) => (WARN, err.clone()),
                    Some((ts, None)) if now - ts <= 90_000 => (up(), t("hun.v_ok").replace("{}", &((now - ts) / 1000).to_string())),
                    _ => (dim(), t("hun.v_wait").to_string()),
                };
                let note = match e { Exchange::Binance => t("hun.note_binance"), _ if !LIQ_VENUES.contains(&e) => t("hun.note_noliq"), _ => "" };
                let name = format!("{e:?}");
                let tip = [name.as_str(), t("hun.venues_tip"), why.as_str(), note].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n");
                if venue_chip(ui, e, !hide, col).on_hover_text(tip).clicked() {
                    if hide { self.hidden.retain(|x| *x != e); } else { self.hidden.push(e); }
                }
            }
            ui.add_space(6.0);
            ui.add_sized(vec2(150.0, kit::H), egui::TextEdit::singleline(&mut self.query).hint_text(RichText::new(t("hun.search")).color(dim())).font(prop_m(12.5)).margin(vec2(8.0, 6.0)));
            let d = self.drafts.as_mut().expect("drafts");
            let bad = typed(&d[5]).is_none();
            let (r, _) = ui.allocate_ui(vec2(132.0, kit::H), |ui| kit::field(ui, t("hun.op_min"), &mut d[5], "", "hun_opmin", bad)).inner;
            if r.changed() { if let Some(x) = typed(&d[5]) { self.op_min = x; } }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let age = if at > 0 { t("hun.status").replace("{n}", &vis.len().to_string()).replace("{s}", &((now - at) / 1000).to_string()) } else { String::new() };
                ui.label(RichText::new(age).font(prop(11.5)).color(dim()));
            });
        });
        ui.add_space(6.0);

        // columns scaled to the width (1174 px at 1:1), clear of the rows' scroll bar
        let total: f32 = COLS.iter().map(|c| c.3).sum();
        let w = ui.available_width() - 10.0;
        let mut xs = vec![0.0f32];
        for c in COLS { xs.push(xs.last().unwrap() + c.3 * w / total); }
        let mut picked = None;
        {
            let (hr, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
            for (k, (c, title, tip, _)) in COLS.iter().enumerate() {
                let cell = Rect::from_min_max(pos2(hr.left() + xs[k], hr.top()), pos2(hr.left() + xs[k + 1], hr.bottom()));
                let resp = ui.interact(cell, ui.id().with(("hun_th", k)), Sense::click()).on_hover_text(t(tip));
                let on = self.sort.0 == *c;
                let (pos, al) = if *c == Col::Coin { (cell.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER) } else { (cell.right_center() - vec2(8.0, 0.0), Align2::RIGHT_CENTER) };
                if on { kit::th(ui.painter(), pos, al, t(title), Some(self.sort.1)); } else { ui.painter().text(pos, al, t(title), prop_m(11.0), if resp.hovered() && *c != Col::Hit { fg() } else { mu() }); }
                // a new column starts A-Z for names, largest first for numbers
                if *c != Col::Hit && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.sort = if on { (*c, !self.sort.1) } else { (*c, *c == Col::Coin) }; }
            }
            ui.painter().hline(hr.x_range(), hr.bottom() + 0.5, Stroke::new(1.0, line()));
            if vis.is_empty() {
                let (r, _) = ui.allocate_exact_size(vec2(w, 140.0), Sense::hover());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, if loading { t("hun.loading") } else { t("hun.none") }, prop(12.5), mu());
                return None;
            }
            // show_rows counts item spacing into the row pitch, so it must be zero here
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().id_salt("hun_rows").auto_shrink([false, false]).show_rows(ui, ROW_H, vis.len(), |ui, range| {
                for &r in &vis[range] {
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::click());
                    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
                    if hv > 0.0 { ui.painter().rect_filled(rect, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
                    cells(ui, rect, r, &xs);
                    let col = ui.input(|i| i.pointer.hover_pos()).filter(|_| resp.hovered()).and_then(|p| xs.iter().rposition(|x| p.x >= rect.left() + x));
                    let tip = col.and_then(|k| COLS.get(k)).and_then(|c| row_tip(r, c.0));
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    let resp = match tip { Some(s) => resp.on_hover_text(s), None => resp };
                    if resp.clicked() { picked = Some(r.base.clone()); }
                }
            });
        }
        picked
    }
}

/// Venue filter chip: logo and status dot (the name in its tooltip); dimmed while its coins are hidden.
fn venue_chip(ui: &mut Ui, e: Exchange, shown: bool, dot: Color32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(38.0, kit::H_SM), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    let y = r.center().y;
    p.rect(r, kit::rad(), if shown { lerp(panel2(), hl(), hv) } else { hl().gamma_multiply(0.5 * hv) }, Stroke::new(1.0, if shown { lerp(line(), edge(), hv) } else { line() }), egui::StrokeKind::Inside);
    icon(p, e, Rect::from_center_size(pos2(r.left() + 14.0, y), vec2(14.0, 14.0)), !shown);
    p.circle_filled(pos2(r.right() - 8.0, y), 3.0, dot);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One row's cells (column edges `xs`).
fn cells(ui: &Ui, rect: Rect, r: &Row, xs: &[f32]) {
    let y = rect.center().y;
    let (l, rt) = (|k: usize| rect.left() + xs[k] + 8.0, |k: usize| rect.left() + xs[k + 1] - 8.0);
    coin_icon(ui, &r.base, Rect::from_center_size(pos2(l(0) + 9.0, y), vec2(18.0, 18.0)));
    let p = ui.painter();
    let num = |k: usize, s: String, c: Color32| { p.text(pos2(rt(k), y), Align2::RIGHT_CENTER, s, mono_m(12.5), c); };
    let none = |k: usize| num(k, "—".into(), dim());
    let pct = |k: usize, x: Option<f64>| match x { Some(x) => num(k, format!("{}%", signed(x, 2)), if x > 0.0 { up() } else if x < 0.0 { dn() } else { fg() }), None => none(k) };
    let bx = p.text(pos2(l(0) + 24.0, y), Align2::LEFT_CENTER, &r.base, prop_m(12.5), fg_strong()).right();
    let vx = p.text(pos2(bx + 6.0, y), Align2::LEFT_CENTER, r.venues.len().to_string(), prop(11.0), dim()).right();
    if !r.odd.is_empty() { p.text(pos2(vx + 6.0, y), Align2::LEFT_CENTER, "?", prop_s(12.5), WARN); }
    for k in 0..3 { pct(k + 1, r.chg[k]); }
    match r.vmult { Some(x) => num(4, format!("{x:.1}×"), if x >= 3.0 { accent() } else { fg() }), None => none(4) }
    num(5, big(r.vol24), mu());
    match r.fund {
        Some((e, f)) => {
            icon(p, e, Rect::from_center_size(pos2(l(6) + 7.0, y), vec2(14.0, 14.0)), false);
            num(6, format!("{}%", signed(f * 100.0, 4)), if f > 0.0 { WARN } else if f < 0.0 { dn() } else { fg() });
        }
        None => none(6),
    }
    pct(7, r.oi_chg[0]);
    pct(8, r.oi_chg[1]);
    match r.liq15 { Some(x) if x > 0.0 => num(9, big(x), fg()), Some(_) => num(9, "0".into(), dim()), None => none(9) }
    match r.gap {
        Some(g) => {
            // hi > lo: logo and market of each
            let mut x = l(10);
            for (i, (e, spot)) in [g.hi, g.lo].into_iter().enumerate() {
                if i == 1 { x = p.text(pos2(x + 3.0, y), Align2::LEFT_CENTER, ">", prop(11.0), dim()).right() + 4.0; }
                icon(p, e, Rect::from_center_size(pos2(x + 7.0, y), vec2(14.0, 14.0)), false);
                x = p.text(pos2(x + 16.0, y), Align2::LEFT_CENTER, tag(spot), prop(10.5), dim()).right();
            }
            num(10, format!("{:.0} bp", g.bp), if g.bp >= 50.0 { WARN } else { fg() });
        }
        None => none(10),
    }
    match r.lead {
        Some(ld) => {
            icon(p, ld.ex, Rect::from_center_size(pos2(l(11) + 7.0, y), vec2(14.0, 14.0)), false);
            p.text(pos2(l(11) + 16.0, y), Align2::LEFT_CENTER, tag(ld.spot), prop(10.5), dim());
            num(11, format!("{:.0}s", ld.secs), fg());
        }
        None => none(11),
    }
    match &r.op {
        Some(Op::Score { score, .. }) => num(12, format!("{score:.0}"), if *score >= 60.0 { WARN } else { fg() }),
        Some(Op::Collecting(n)) => { p.text(pos2(rt(12), y), Align2::RIGHT_CENTER, t("hun.collecting").replace("{}", &n.to_string()), prop(11.0), dim()); }
        None => none(12),
    }
    none(13);
}

/// One scored trait of the operator-type score, for the tooltip.
fn op_part(k: &str, v: f64) -> String {
    let pct = || format!("{:.0}", v * 100.0);
    match k {
        "spikes" => t("hun.op_spikes").replace("{}", &pct()),
        "wicks" => t("hun.op_wicks").replace("{}", &pct()),
        "days" => t("hun.op_days").replace("{}", &format!("{v:.0}")),
        "new" => t("hun.op_new").replace("{}", &format!("{v:.0}")),
        "perp" if v.is_infinite() => t("hun.op_nospot").to_string(),
        "perp" => t("hun.op_perp").replace("{}", &format!("{v:.0}")),
        "runup" => t("hun.op_runup").replace("{}", &pct()),
        _ => k.to_string(),
    }
}

/// The hovered cell's details, where the cell shows less than it knows.
fn row_tip(r: &Row, c: Col) -> Option<String> {
    match c {
        Col::Coin => {
            let mut s = r.venues.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(" · ");
            if !r.odd.is_empty() { s += &format!("\n{} {}", t("hun.odd"), r.odd.iter().map(|(e, spot)| format!("{e:?} {}", mkt(*spot))).collect::<Vec<_>>().join(", ")); }
            Some(s)
        }
        Col::Fund => r.fund.map(|(e, f)| format!("{e:?} {}%/h · {}%/8h", signed(f * 100.0, 4), signed(f * 800.0, 4))),
        Col::Gap => r.gap.map(|g| format!("{:?} {} > {:?} {}: {:.1} bp", g.hi.0, mkt(g.hi.1), g.lo.0, mkt(g.lo.1), g.bp)),
        Col::Lead => r.lead.map(|l| t("hun.lead_row").replace("{v}", &format!("{:?} {}", l.ex, mkt(l.spot))).replace("{s}", &format!("{:.0}", l.secs))),
        Col::Op => r.op.as_ref().map(|o| match o {
            Op::Collecting(n) => format!("{}\n{}", t("hun.op_wait").replace("{}", &n.to_string()), t("hun.op_caveat")),
            Op::Score { parts, .. } => {
                let shown: Vec<String> = parts.iter().filter(|p| p.2).map(|p| format!("· {}", op_part(p.0, p.1))).collect();
                let head = t("hun.op_traits").replace("{k}", &shown.len().to_string()).replace("{n}", &parts.len().to_string());
                format!("{head}\n{}\n{}", shown.join("\n"), t("hun.op_caveat"))
            }
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(base: &str, vol24: f64, m5: Option<f64>, venues: Vec<Exchange>) -> Row {
        Row { base: base.into(), px: 1.0, chg: [m5, None, None], vmult: None, vol24, ps24: (vol24, 0.0), fund: None, oi_chg: [None; 2], liq15: None, gap: None, lead: None, venues, odd: vec![], flags: vec![],
            op: m5.map(|x| Op::Score { score: x.abs() * 10.0, parts: vec![] }) }
    }

    #[test]
    fn filters_and_sorts_with_blanks_last() {
        let rows = vec![row("AAA", 9e6, Some(-2.0), vec![Exchange::Gate]), row("BBB", 8e6, None, vec![Exchange::Binance]), row("CCC", 7e6, Some(3.0), vec![Exchange::Bybit]), row("DUST", 1e5, Some(50.0), vec![Exchange::Bybit])];
        let names = |v: Vec<&Row>| v.iter().map(|r| r.base.as_str()).collect::<Vec<_>>().join(",");
        assert_eq!(names(visible(&rows, 5e6, &[], "", 0.0, (Col::M5, false))), "CCC,AAA,BBB");
        assert_eq!(names(visible(&rows, 5e6, &[], "", 0.0, (Col::M5, true))), "AAA,CCC,BBB");
        assert_eq!(names(visible(&rows, 0.0, &[Exchange::Bybit], " a", 0.0, (Col::Coin, true))), "AAA");
        // a score floor drops the unscored; sorted by score, blanks last
        assert_eq!(names(visible(&rows, 0.0, &[], "", 25.0, (Col::Op, false))), "DUST,CCC");
        assert_eq!(names(visible(&rows, 5e6, &[], "", 0.0, (Col::Op, true))), "AAA,CCC,BBB");
        assert_eq!(typed("1,000.5"), Some(1000.5));
        assert_eq!((typed(""), shown(0.05), shown(5.0)), (None, "0.05".into(), "5".into()));
    }
}
