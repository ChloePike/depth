//! Hunter page (title bar pill after the markets): the market-wide scanner's table (engine `Hunt`,
//! rebuilt every 5 s in the background whatever is on screen), its filters, alert rules and log.
//! Nine columns; what they leave out (5 m, OI 15 m, liquidations, who moved first, the gap's legs in
//! full, the venues) is in the row's tooltip. Facts only: the Operator column classifies coins (it
//! does not time moves). A row or an alert line opens that coin in Perpetual. Nothing here asks for
//! repaints: the scanner does, once per rebuild.
use super::{hms, kit, local_offset_ms, t, theme::*, top::{self, big, signed}};
use crate::engine::Hunt;
use depth::hunter::{is_major, Alerts, Op, Row, LIQ_VENUES, VENUES};
use depth::{now_ms, Exchange};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Rect, RichText, Sense, Stroke, Ui};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Col { Coin, Op, M15, H1, VMult, Fund, Oi1h, Gap, Vol }

/// (column, title key, header tooltip key, width)
const COLS: [(Col, &str, &str, f32); 9] = [
    (Col::Coin, "hun.coin", "hun.tip_coin", 150.0), (Col::Op, "hun.op", "hun.tip_op", 96.0),
    (Col::M15, "hun.15m", "hun.tip_chg", 72.0), (Col::H1, "hun.1h", "hun.tip_chg", 72.0),
    (Col::VMult, "hun.vmult", "hun.tip_vmult", 72.0), (Col::Fund, "hun.funding", "hun.tip_fund", 124.0),
    (Col::Oi1h, "hun.oi1h", "hun.tip_oi", 80.0), (Col::Gap, "hun.gap", "hun.tip_gap", 170.0), (Col::Vol, "hun.vol24", "hun.tip_vol", 90.0),
];
/// alert rules: (label key, unit), in `Alerts::rules` order
const RULES: [(&str, &str); 4] = [("hun.r_move", "%"), ("hun.r_vmult", "×"), ("hun.r_fund", "%/h"), ("hun.r_gap", "bp")];
const ROW_H: f32 = 28.0;
const SIDE: f32 = 270.0;
/// Bumped when the defaults change: a page saved under an older one gets the new ones once (`preset_once`).
const PRESET: u8 = 1;
/// operators first (scored before collecting), then the largest
const SORT: (Col, bool) = (Col::Op, false);
/// width of the gap cell's two legs (logo, market, ">", logo, market)
const LEGS_W: f32 = 62.0;
/// a mover in the header: moved this much (%) in the last hour
const MOVER: f64 = 5.0;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HunterView {
    /// the page is shown in place of the chart, book and order panel
    pub open: bool,
    /// alert rules, the minimum 24 h volume and whether the big coins show (all the table's filters too), copied to the engine
    pub alerts: Alerts,
    /// venues whose coins are hidden
    pub hidden: Vec<Exchange>,
    /// lowest operator-type score shown (0 = every coin)
    pub op_min: f64,
    #[serde(deserialize_with = "lenient_sort")]
    sort: (Col, bool),
    /// the defaults generation this was saved under (0: none yet)
    #[serde(default = "no_preset")]
    preset: u8,
    #[serde(skip)] query: String,
    /// typed text of the volume floor (M USD), the four thresholds and the score floor
    #[serde(skip)] drafts: Option<[String; 6]>,
    /// the top rows last sent to the engine (`Hunt::want`)
    #[serde(skip)] want: Vec<String>,
}

fn no_preset() -> u8 { 0 }

/// A sort column an older or newer build does not know costs only the sort.
fn lenient_sort<'de, D: serde::Deserializer<'de>>(d: D) -> Result<(Col, bool), D::Error> { Ok(serde::Deserialize::deserialize(d).unwrap_or(SORT)) }

impl Default for HunterView {
    fn default() -> Self { HunterView { open: false, alerts: Alerts::default(), hidden: vec![], op_min: 0.0, sort: SORT, preset: PRESET, query: String::new(), drafts: None, want: vec![] } }
}

fn key(r: &Row, c: Col) -> Option<f64> {
    match c {
        Col::M15 => r.chg[1], Col::H1 => r.chg[2], Col::VMult => r.vmult, Col::Vol => Some(r.vol24), Col::Fund => r.fund.map(|f| f.1),
        Col::Oi1h => r.oi_chg[1], Col::Gap => r.gap.map(|g| g.bp), Col::Op => score(r), Col::Coin => None,
    }
}

fn score(r: &Row) -> Option<f64> { match &r.op { Some(Op::Score { score, .. }) => Some(*score), _ => None } }

/// Rows shown: above the volume floor, the big coins only when asked for, listed on a venue not
/// hidden, matching the text, scored at least `op_min` (when above 0); sorted by the column (blanks
/// last either way, ties and blanks by volume).
fn visible<'a>(rows: &'a [Row], a: &Alerts, hidden: &[Exchange], query: &str, op_min: f64, sort: (Col, bool)) -> Vec<&'a Row> {
    let q = query.trim().to_uppercase();
    let mut v: Vec<&Row> = rows.iter().filter(|r| r.vol24 >= a.min_vol && (a.majors || !is_major(&r.base)) && r.venues.iter().any(|e| !hidden.contains(e)) && r.base.to_uppercase().contains(&q)
        && (op_min <= 0.0 || score(r).is_some_and(|x| x >= op_min))).collect();
    v.sort_by(|a, b| {
        let by_vol = || b.vol24.total_cmp(&a.vol24);
        if sort.0 == Col::Coin { let o = a.base.cmp(&b.base); return if sort.1 { o } else { o.reverse() }; }
        match (key(a, sort.0), key(b, sort.0)) {
            (Some(x), Some(y)) => (if sort.1 { x.total_cmp(&y) } else { y.total_cmp(&x) }).then_with(by_vol),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => by_vol(),
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

    /// A page saved before the current defaults gets them once: filters, alert rules and sort are replaced.
    pub fn preset_once(&mut self) {
        if self.preset < PRESET { *self = HunterView { open: self.open, ..Default::default() }; }
    }

    /// The page in the space left in `ui`; returns the coin clicked (a row or an alert line). `on`:
    /// the scanner runs; `off`: venues switched off in the status bar.
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
        let vis = visible(&rows, &a, &self.hidden, &self.query, op_min, self.sort);
        // the top rows' coins are first in line for the hourly history and get Binance's OI history
        let want: Vec<String> = vis.iter().take(30).map(|r| r.base.clone()).collect();
        if want != self.want { hunt.lock().unwrap().want = want.clone(); self.want = want; }
        let side = Rect::from_min_max(pos2(full.right() - SIDE, full.top()), full.max);
        let main = Rect::from_min_max(full.min, pos2(side.left() - 14.0, full.bottom()));
        let mut s = ui.new_child(egui::UiBuilder::new().id_salt("hun_side").max_rect(side));
        s.set_clip_rect(side.intersect(ui.clip_rect()));
        let from_log = self.side(&mut s, &log);
        ui.painter().vline(side.left() - 7.0, full.y_range(), Stroke::new(1.0, line()));
        let mut m = ui.new_child(egui::UiBuilder::new().id_salt("hun_main").max_rect(main));
        m.set_clip_rect(main.intersect(ui.clip_rect()));
        let picked = self.table(&mut m, &vis, rows.is_empty(), at, &venue, off);
        if self.alerts != a { self.sync(hunt); }
        picked.or(from_log)
    }

    /// The header's stats strip on this page, in place of the current coin's market stats.
    pub fn stats(&self, ui: &mut Ui, hunt: &Mutex<Hunt>, off: &HashSet<Exchange>) {
        let now = now_ms();
        let day = (now + local_offset_ms()).div_euclid(86_400_000) * 86_400_000 - local_offset_ms();
        let (rows, today, venue) = {
            let h = hunt.lock().unwrap();
            (h.rows.clone(), h.log.iter().filter(|x| x.0 >= day).count(), h.venue.clone())
        };
        let a = &self.alerts;
        let on: Vec<Exchange> = VENUES.into_iter().filter(|e| !off.contains(e)).collect();
        let live = on.iter().filter(|e| venue.get(*e).is_some_and(|(ts, err)| err.is_none() && now - ts <= 90_000)).count();
        let shown: Vec<&Row> = rows.iter().filter(|r| r.vol24 >= a.min_vol && (a.majors || !is_major(&r.base))).collect();
        let movers = shown.iter().filter(|r| r.chg[2].is_some_and(|x| x.abs() >= MOVER)).count();
        let ready = shown.iter().filter(|r| score(r).is_some()).count();
        let collecting = shown.iter().filter(|r| matches!(r.op, Some(Op::Collecting(_)))).count();
        ui.spacing_mut().item_spacing.x = 28.0;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        top::stat(ui, t("hun.st_coins"), &[(rows.len().to_string(), fg())]).on_hover_text(t("hun.tip_st_coins"));
        top::stat(ui, t("hun.st_venues"), &[(format!("{live}/{}", on.len()), if live == on.len() { fg() } else { WARN })]).on_hover_text(t("hun.tip_st_venues"));
        top::stat(ui, t("hun.st_movers"), &[(movers.to_string(), if movers > 0 { accent() } else { fg() })]).on_hover_text(t("hun.tip_st_movers"));
        top::stat(ui, t("hun.st_alerts"), &[(today.to_string(), if today > 0 { accent() } else { fg() })]).on_hover_text(t("hun.tip_st_alerts"));
        top::stat(ui, t("hun.st_scores"), &[(ready.to_string(), fg()), ("/".into(), dim()), (collecting.to_string(), mu())]).on_hover_text(t("hun.tip_st_scores"));
    }

    /// Alert rules (switch, threshold) and the alert log, newest first, one line each; returns the
    /// coin of a clicked line.
    fn side(&mut self, ui: &mut Ui, log: &[(i64, String, String)]) -> Option<String> {
        ui.spacing_mut().item_spacing.y = 4.0;
        kit::overline(ui, t("hun.alerts")).on_hover_text(t("hun.alerts_tip"));
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
        ui.add_space(8.0);
        kit::overline(ui, t("hun.log"));
        if log.is_empty() { kit::note(ui, t("hun.no_alerts"), dim()); return None; }
        let mut picked = None;
        egui::ScrollArea::vertical().id_salt("hun_log").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (ts, coin, text) in log { if log_line(ui, *ts, coin, text).clicked() { picked = Some(coin.clone()); } }
        });
        picked
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
            if chip_toggle(ui, t("hun.majors"), self.alerts.majors).on_hover_text(t("hun.tip_majors")).clicked() { self.alerts.majors = !self.alerts.majors; }
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

        // columns scaled to the width, clear of the rows' scroll bar
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
                if on { kit::th(ui.painter(), pos, al, t(title), Some(self.sort.1)); } else { ui.painter().text(pos, al, t(title), prop_m(11.0), if resp.hovered() { fg() } else { mu() }); }
                // a new column starts A-Z for names, largest first for numbers
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { self.sort = if on { (*c, !self.sort.1) } else { (*c, *c == Col::Coin) }; }
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
                    // one tooltip: the Operator cell explains its traits, anywhere else the facts the columns leave out
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_ui(|ui| {
                        ui.set_max_width(380.0);
                        let col = ui.input(|i| i.pointer.hover_pos()).and_then(|p| xs.iter().rposition(|x| p.x >= rect.left() + x)).and_then(|k| COLS.get(k)).map(|c| c.0);
                        let tip = match (col, &r.op) { (Some(Col::Op), Some(o)) => op_tip(o), _ => row_tip(r) };
                        ui.label(RichText::new(tip).font(prop(12.0)).color(fg()));
                    });
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

/// A text chip that switches, like the venue chips: bright while on.
fn chip_toggle(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), prop_m(12.0), fg());
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 22.0, kit::H_SM), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    p.rect(r, kit::rad(), if on { lerp(panel2(), hl(), hv) } else { hl().gamma_multiply(0.5 * hv) }, Stroke::new(1.0, if on { lerp(line(), edge(), hv) } else { line() }), egui::StrokeKind::Inside);
    p.text(r.center(), Align2::CENTER_CENTER, text, prop_m(12.0), if on { fg_strong() } else { dim() });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One alert line: time (dim), coin (bold), the message cut to the width; the whole text on hover.
fn log_line(ui: &mut Ui, ts: i64, coin: &str, text: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    let p = ui.painter();
    if hv > 0.0 { p.rect_filled(rect, kit::rad(), hl().gamma_multiply(0.7 * hv)); }
    let y = rect.center().y;
    let time = hms(ts);
    let time = &time[..5];
    let x = p.text(pos2(rect.left() + 4.0, y), Align2::LEFT_CENTER, time, mono(11.5), dim()).right();
    let x = p.text(pos2(x + 8.0, y), Align2::LEFT_CENTER, coin, prop_s(12.0), fg_strong()).right() + 8.0;
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), prop(11.5), fg());
    job.wrap = egui::text::TextWrapping { max_width: (rect.right() - x - 4.0).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…'), ..Default::default() };
    let g = p.layout_job(job);
    p.galley(pos2(x, y - g.size().y / 2.0), g, fg());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{time} {coin} {text}"))
}

/// One row's cells (column edges `xs`), in `COLS` order.
fn cells(ui: &Ui, rect: Rect, r: &Row, xs: &[f32]) {
    let y = rect.center().y;
    let (l, rt) = (|k: usize| rect.left() + xs[k] + 8.0, |k: usize| rect.left() + xs[k + 1] - 8.0);
    coin_icon(ui, &r.base, Rect::from_center_size(pos2(l(0) + 9.0, y), vec2(18.0, 18.0)));
    let p = ui.painter();
    let numr = |k: usize, s: String, c: Color32| p.text(pos2(rt(k), y), Align2::RIGHT_CENTER, s, mono_m(12.5), c);
    let num = |k: usize, s: String, c: Color32| { numr(k, s, c); };
    let none = |k: usize| num(k, "—".into(), dim());
    let pct = |k: usize, x: Option<f64>| match x { Some(x) => num(k, format!("{}%", signed(x, 2)), if x > 0.0 { up() } else if x < 0.0 { dn() } else { fg() }), None => none(k) };
    let bx = p.text(pos2(l(0) + 24.0, y), Align2::LEFT_CENTER, &r.base, prop_m(12.5), fg_strong()).right();
    let vx = p.text(pos2(bx + 6.0, y), Align2::LEFT_CENTER, r.venues.len().to_string(), prop(11.0), dim()).right();
    if !r.odd.is_empty() { p.text(pos2(vx + 6.0, y), Align2::LEFT_CENTER, "?", prop_s(12.5), WARN); }
    match &r.op {
        Some(Op::Score { score, .. }) => num(1, format!("{score:.0}"), if *score >= 60.0 { WARN } else { fg() }),
        Some(Op::Collecting(n)) => { p.text(pos2(rt(1), y), Align2::RIGHT_CENTER, t("hun.collecting").replace("{}", &n.to_string()), prop(11.0), dim()); }
        None => none(1),
    }
    pct(2, r.chg[1]);
    pct(3, r.chg[2]);
    match r.vmult { Some(x) => num(4, format!("{x:.1}×"), if x >= 3.0 { accent() } else { fg() }), None => none(4) }
    match r.fund {
        Some((e, f)) => {
            // the venue's logo right before its rate
            let n = numr(5, format!("{}%", signed(f * 100.0, 4)), if f > 0.0 { WARN } else if f < 0.0 { dn() } else { fg() });
            icon(p, e, Rect::from_center_size(pos2(n.left() - 15.0, y), vec2(14.0, 14.0)), false);
        }
        None => none(5),
    }
    pct(6, r.oi_chg[1]);
    match r.gap {
        Some(g) => {
            // hi > lo: logo and market of each, right before the number
            let n = numr(7, format!("{:.0} bp", g.bp), if g.bp >= 50.0 { WARN } else { fg() });
            let mut x = n.left() - 12.0 - LEGS_W;
            for (i, (e, spot)) in [g.hi, g.lo].into_iter().enumerate() {
                if i == 1 { x = p.text(pos2(x + 3.0, y), Align2::LEFT_CENTER, ">", prop(11.0), dim()).right() + 4.0; }
                icon(p, e, Rect::from_center_size(pos2(x + 7.0, y), vec2(14.0, 14.0)), false);
                x = p.text(pos2(x + 16.0, y), Align2::LEFT_CENTER, tag(spot), prop(10.5), dim()).right();
            }
        }
        None => none(7),
    }
    num(8, big(r.vol24), mu());
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

/// The Operator cell's details: the traits that scored, or how much data is collected.
fn op_tip(o: &Op) -> String {
    match o {
        Op::Collecting(n) => format!("{}\n{}", t("hun.op_wait").replace("{}", &n.to_string()), t("hun.op_caveat")),
        Op::Score { parts, .. } => {
            let shown: Vec<String> = parts.iter().filter(|p| p.2).map(|p| format!("· {}", op_part(p.0, p.1))).collect();
            let head = t("hun.op_traits").replace("{k}", &shown.len().to_string()).replace("{n}", &parts.len().to_string());
            format!("{head}\n{}\n{}", shown.join("\n"), t("hun.op_caveat"))
        }
    }
}

/// The row's tooltip: what the nine columns leave out.
fn row_tip(r: &Row) -> String {
    let pct = |x: Option<f64>| x.map_or("—".to_string(), |x| format!("{}%", signed(x, 2)));
    let mut s = vec![
        format!("{}  {}", t("hun.5m"), pct(r.chg[0])),
        format!("{}  {}", t("hun.oi15"), pct(r.oi_chg[0])),
        format!("{}  {}", t("hun.liq15"), r.liq15.map_or("—".to_string(), |x| if x > 0.0 { format!("${}", big(x)) } else { "0".into() })),
    ];
    if let Some(l) = r.lead { s.push(t("hun.lead_row").replace("{v}", &format!("{:?} {}", l.ex, mkt(l.spot))).replace("{s}", &format!("{:.0}", l.secs))); }
    if let Some(g) = r.gap { s.push(format!("{}  {:?} {} > {:?} {}: {:.1} bp", t("hun.gap"), g.hi.0, mkt(g.hi.1), g.lo.0, mkt(g.lo.1), g.bp)); }
    s.push(format!("{}  {}", t("hun.venues"), r.venues.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(" · ")));
    if !r.odd.is_empty() { s.push(format!("{} {}", t("hun.odd"), r.odd.iter().map(|(e, spot)| format!("{e:?} {}", mkt(*spot))).collect::<Vec<_>>().join(", "))); }
    s.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(base: &str, vol24: f64, m15: Option<f64>, venues: Vec<Exchange>) -> Row {
        Row { base: base.into(), px: 1.0, chg: [None, m15, None], vmult: None, vol24, ps24: (vol24, 0.0), fund: None, oi_chg: [None; 2], liq15: None, gap: None, lead: None, venues, odd: vec![], flags: vec![],
            op: m15.map(|x| Op::Score { score: x.abs() * 10.0, parts: vec![] }) }
    }

    #[test]
    fn filters_and_sorts_with_blanks_last() {
        let rows = vec![row("AAA", 9e6, Some(-2.0), vec![Exchange::Gate]), row("BBB", 8e6, None, vec![Exchange::Binance]), row("CCC", 7e6, Some(3.0), vec![Exchange::Bybit]), row("DUST", 1e5, Some(50.0), vec![Exchange::Bybit])];
        let names = |v: Vec<&Row>| v.iter().map(|r| r.base.as_str()).collect::<Vec<_>>().join(",");
        let a = |min_vol: f64| Alerts { min_vol, ..Default::default() };
        assert_eq!(names(visible(&rows, &a(5e6), &[], "", 0.0, (Col::M15, false))), "CCC,AAA,BBB");
        assert_eq!(names(visible(&rows, &a(5e6), &[], "", 0.0, (Col::M15, true))), "AAA,CCC,BBB");
        assert_eq!(names(visible(&rows, &a(0.0), &[Exchange::Bybit], " a", 0.0, (Col::Coin, true))), "AAA");
        // a score floor drops the unscored; sorted by score, blanks last
        assert_eq!(names(visible(&rows, &a(0.0), &[], "", 25.0, (Col::Op, false))), "DUST,CCC");
        assert_eq!(names(visible(&rows, &a(5e6), &[], "", 0.0, (Col::Op, true))), "AAA,CCC,BBB");
        assert_eq!(typed("1,000.5"), Some(1000.5));
        assert_eq!((typed(""), shown(0.05), shown(5.0)), (None, "0.05".into(), "5".into()));
    }

    /// The default page: operators first, scored before collecting, ties by volume; the big coins
    /// only when asked for.
    #[test]
    fn default_order_puts_scored_operators_first() {
        let mut rows = vec![row("LOW", 6e6, Some(2.0), vec![Exchange::Gate]), row("COLLECT", 90e6, None, vec![Exchange::Gate]), row("HIGH", 5e6, Some(5.0), vec![Exchange::Gate]),
            row("TIE", 8e6, Some(2.0), vec![Exchange::Gate]), row("BTC", 900e6, Some(9.0), vec![Exchange::Gate])];
        rows[1].op = Some(Op::Collecting(3));
        let names = |v: Vec<&Row>| v.iter().map(|r| r.base.as_str()).collect::<Vec<_>>().join(",");
        let a = Alerts::default();
        assert_eq!(names(visible(&rows, &a, &[], "", 0.0, SORT)), "HIGH,TIE,LOW,COLLECT");
        assert_eq!(names(visible(&rows, &Alerts { majors: true, ..a }, &[], "", 0.0, SORT)), "BTC,HIGH,TIE,LOW,COLLECT");
    }

    /// A page saved before the presets gets them once; one saved under the current ones is kept.
    #[test]
    fn saved_pages_get_the_preset_once() {
        // an older save: a sort column that no longer exists, rules the user switched on by hand, no generation
        let old = serde_json::json!({"open": true, "alerts": {"min_vol": 1e6, "rules": [[true, 3.0], [false, 1.0], [false, 1.0], [false, 1.0]]}, "hidden": ["Okx"], "op_min": 40.0, "sort": ["Hit", true]});
        let mut v: HunterView = serde_json::from_value(old).unwrap();
        assert_eq!((v.preset, v.sort, v.alerts.min_vol, v.op_min), (0, SORT, 1e6, 40.0));
        v.preset_once();
        assert!(v.open && v.hidden.is_empty() && v.op_min == 0.0 && v.alerts == Alerts::default() && v.preset == PRESET);
        // now saved under the current preset: the user's edits stay
        v.alerts.min_vol = 2e6;
        v.sort = (Col::Vol, true);
        let mut again: HunterView = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
        again.preset_once();
        assert_eq!((again.alerts.min_vol, again.sort), (2e6, (Col::Vol, true)));
    }
}
