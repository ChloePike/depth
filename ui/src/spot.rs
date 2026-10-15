//! Spot order entry for the order column in Spot mode (Bybit, Binance): buy / sell, limit or
//! market, sized in base units. Prices come from the chosen venue's own spot book, never the
//! composite index; the confirm dialog shows exactly what is sent.
use super::engine::Engine;
use super::kit::{self, Look, H, H_LG};
use super::order::{dialog, dialog_title};
use super::{fmt_px, fmt_qty, t, theme::*};
use depth::agg::Agg;
use depth::trade::{self, Kind, Tif};
use depth::{Exchange, Market, Side};
use eframe::egui::{self, Ui};

struct Ticket { ex: Exchange, symbol: String, buy: bool, kind: Kind, qty: f64, ref_px: f64 }

pub struct SpotPanel {
    ex: Exchange,
    buy: bool,
    market: bool,
    /// limit price text; follows the venue's touch until edited
    px: String,
    px_edited: bool,
    qty: String,
    confirm: Option<Ticket>,
    /// venues whose spot balances were asked for once (later updates come with resyncs and orders)
    asked: Vec<Exchange>,
}

impl Default for SpotPanel {
    fn default() -> Self { SpotPanel { ex: Exchange::Bybit, buy: true, market: false, px: String::new(), px_edited: false, qty: String::new(), confirm: None, asked: vec![] } }
}

fn parse(s: &str) -> Option<f64> { s.trim().replace(',', "").parse().ok().filter(|x: &f64| x.is_finite() && *x > 0.0) }

impl SpotPanel {
    pub fn show(&mut self, ui: &mut Ui, a: &Agg, eng: &Engine, base: &str) {
        let symbol = trade::symbol(base);
        // the venue's own touch, native quote (USDT on both venues)
        let touch = a.venues.get(&(self.ex, Market::Spot)).and_then(|v| v.bbo.map(|b| (b[0], b[2])).or_else(|| Some((v.book.best_bid()?.0, v.book.best_ask()?.0))));
        let (keyed, bal) = {
            let acc = eng.account.lock().unwrap();
            (acc.keys.contains_key(&self.ex), acc.spot_bal.get(&self.ex).cloned())
        };
        if keyed && bal.is_none() && !self.asked.contains(&self.ex) { self.asked.push(self.ex); eng.spot_refresh(self.ex, ui.ctx()); }
        let free = |coin: &str| bal.as_ref().and_then(|b| b.iter().find(|x| x.coin == coin)).map_or(0.0, |x| x.free);
        kit::panel_frame(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.horizontal(|ui| {
                kit::title(ui, t("spot.title"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { kit::chip(ui, &format!("{base}/USDT"), kit::Tone::Neutral); });
            });
            let venues: Vec<(Exchange, &str)> = trade::spot::VENUES.iter().map(|e| (*e, if *e == Exchange::Bybit { "Bybit" } else { "Binance" })).collect();
            if kit::segments(ui, &mut self.ex, &venues, H) { self.px_edited = false; }
            let mut buy = self.buy;
            if kit::segments(ui, &mut buy, &[(true, t("spot.buy")), (false, t("spot.sell"))], H) { self.buy = buy; self.px_edited = false; }
            kit::segments(ui, &mut self.market, &[(false, t("spot.limit")), (true, t("spot.market"))], H);
            // passive default: join the bid when buying, the ask when selling
            let quote_px = touch.map(|(b, k)| if self.buy { b } else { k });
            if !self.px_edited { if let Some(p) = quote_px { self.px = fmt_px(p).replace(',', ""); } }
            if !self.market {
                let bad = parse(&self.px).is_none();
                let (r, _) = kit::field(ui, t("spot.price"), &mut self.px, "USDT", "spot_px", bad);
                if r.changed() { self.px_edited = true; }
            }
            let bad = !self.qty.is_empty() && parse(&self.qty).is_none();
            kit::field(ui, t("spot.qty"), &mut self.qty, base, "spot_qty", bad);
            // market orders: priced for the checks and the estimate at the touch they will take
            let px = if self.market { touch.map(|(b, k)| if self.buy { k } else { b }) } else { parse(&self.px) };
            let avail = if self.buy { free("USDT") } else { free(base) };
            // percentage of what is available: of USDT / price when buying, of the coin when selling
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = (ui.available_width() - 18.0) / 4.0;
                for pct in [25.0, 50.0, 75.0, 100.0] {
                    let on = keyed && avail > 0.0 && px.is_some();
                    if kit::button(ui, &format!("{pct:.0}%"), Look::Secondary, 24.0, Some(w), on).clicked() {
                        let q = if self.buy { avail * pct / 100.0 / px.unwrap_or(1.0) } else { avail * pct / 100.0 };
                        self.qty = format!("{q:.8}").trim_end_matches('0').trim_end_matches('.').to_string();
                    }
                }
            });
            let qty = parse(&self.qty);
            kit::kv(ui, t("spot.avail"), if keyed { format!("{} {}", fmt_qty(avail), if self.buy { "USDT" } else { base }) } else { "—".into() }, fg());
            kit::kv(ui, t("spot.total"), match (qty, px) { (Some(q), Some(p)) => format!("≈ {:.2} USDT", q * p), _ => "—".into() }, fg());
            if !keyed { kit::note(ui, t("spot.no_keys"), WARN); return; }
            let label = format!("{} {base}", if self.buy { t("spot.buy") } else { t("spot.sell") });
            let ok = qty.is_some() && px.is_some();
            if kit::button(ui, &label, Look::Fill(if self.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), ok).clicked() {
                let (q, p) = (qty.unwrap_or(0.0), px.unwrap_or(0.0));
                let kind = if self.market { Kind::Market } else { Kind::Limit { price: p, tif: Tif::Gtc } };
                self.confirm = Some(Ticket { ex: self.ex, symbol: symbol.clone(), buy: self.buy, kind, qty: q, ref_px: p });
            }
            if self.ex == Exchange::Binance { kit::note(ui, t("spot.binance_wallet"), dim()); }
        });
        self.confirm_dialog(ui, eng, base);
    }

    fn confirm_dialog(&mut self, ui: &mut Ui, eng: &Engine, base: &str) {
        let Some(tk) = &self.confirm else { return };
        let (mut cancel, mut send) = (false, false);
        let r = dialog("spot_confirm").show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            let side = if tk.buy { t("spot.buy") } else { t("spot.sell") };
            dialog_title(ui, &format!("{side} {base}"), &format!("{:?} · {}", tk.ex, t("spot.title")), Some((side, if tk.buy { kit::Tone::Up } else { kit::Tone::Down })));
            kit::group(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                kit::kv(ui, t("spot.type"), match tk.kind { Kind::Market => t("spot.market").to_string(), _ => format!("{} GTC", t("spot.limit")) }, fg());
                kit::kv(ui, t("spot.price"), match tk.kind { Kind::Limit { price, .. } => fmt_px(price), _ => format!("≈ {}", fmt_px(tk.ref_px)) }, fg());
                kit::kv(ui, t("spot.qty"), format!("{} {base}", fmt_qty(tk.qty)), fg_strong());
                kit::kv(ui, t("spot.total"), format!("≈ {:.2} USDT", tk.qty * tk.ref_px), WARN);
            });
            kit::note(ui, t("spot.untested"), WARN);
            ui.columns(2, |c| {
                cancel = kit::button(&mut c[0], t("ins.cancel"), Look::Secondary, H_LG, Some(f32::INFINITY), true).clicked();
                send = kit::button(&mut c[1], t("spot.send"), Look::Fill(if tk.buy { up() } else { dn() }), H_LG, Some(f32::INFINITY), true).clicked();
            });
        });
        if send {
            eng.spot_submit(tk.ex, tk.symbol.clone(), if tk.buy { Side::Buy } else { Side::Sell }, tk.kind, tk.qty, tk.ref_px, ui.ctx());
            self.qty.clear();
        }
        if send || cancel || r.should_close() { self.confirm = None; }
    }
}
