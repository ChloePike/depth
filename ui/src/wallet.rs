//! Assets tab: every account per venue (loaded on demand), the transfer dialog, and the
//! auto top-up rule. Transfers never leave the venue (no withdrawals exist in this app).
use super::engine::Engine;
use super::{fmt_dp, fmt_qty, t, theme::*};
use eframe::egui::{self, RichText, Ui};
use terminal_one::trade::{self, Wallet};
use terminal_one::{now_ms, Exchange};

struct Xfer { ex: Exchange, from: String, to: String, coin: String, amount: String }

#[derive(Default)]
pub struct WalletView { xfer: Option<Xfer> }

fn acct_name(id: &str) -> String {
    let k = match id {
        "UNIFIED" => "wal.UNIFIED", "FUND" => "wal.FUND", "EARN" => "wal.EARN", "PM" => "wal.PM",
        "USDM" => "wal.USDM", "SPOT" => "wal.SPOT", "FUNDING" => "wal.FUNDING", _ => return id.to_string(),
    };
    t(k).to_string()
}

/// Short reason for an account that could not be read (raw text in the hover).
fn note_hint(n: &str) -> &'static str {
    if n.contains("10005") || n.contains("Permission") || n.contains("-2015") || n.contains("permission") { t("wal.no_perm") } else { t("wal.read_fail") }
}

impl WalletView {
    pub fn show(&mut self, ui: &mut Ui, eng: &Engine) {
        let (keys, wallets, loading) = {
            let a = eng.account.lock().unwrap();
            let mut keys: Vec<Exchange> = a.keys.keys().copied().collect();
            keys.sort_by_key(|e| format!("{e:?}"));
            (keys, a.wallets.clone(), a.wallets_loading.clone())
        };
        for ex in keys {
            // T1_XFER=1: open the transfer dialog once wallets are in (screenshots)
            if let Some((ws, _, _)) = wallets.get(&ex) {
                static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if std::env::var("T1_XFER").is_ok() && ex == Exchange::Binance && !SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) { self.open(ex, ws); }
            }
            let Some((ws, notes, at)) = wallets.get(&ex) else {
                if !loading.contains(&ex) { eng.load_wallets(ex, ui.ctx()); }
                ui.horizontal(|ui| { icon_label(ui, ex, format!("{ex:?}"), false); ui.label(RichText::new(t("wal.loading")).color(dim())); });
                continue;
            };
            // max(0): float dust can sum to -0.00
            let total: f64 = ws.iter().map(|w| w.usd).sum::<f64>().max(0.0);
            ui.horizontal(|ui| {
                icon_label(ui, ex, RichText::new(format!("{ex:?}")).strong(), false);
                ui.label(RichText::new(format!("≈ {} USD", fmt_dp(total, 2))).font(mono(12.0)));
                ui.label(RichText::new(format!("{} {}s", t("wal.updated"), (now_ms() - at) / 1000)).font(prop(10.5)).color(dim()));
                let busy = loading.contains(&ex);
                ui.add_space(8.0);
                if link(ui, t("wal.transfer")).clicked() { self.open(ex, ws); }
                if !busy && link(ui, t("wal.refresh")).clicked() { eng.load_wallets(ex, ui.ctx()); }
                let auto = eng.account.lock().unwrap().auto.get(&ex).copied().unwrap_or_default();
                if auto.enabled { ui.label(RichText::new(format!("{} < {} → {}", t("wal.auto_on"), auto.min, auto.target)).font(prop(10.5)).color(up())); }
            });
            egui::Grid::new(("wallets", ex as u8)).spacing([18.0, 4.0]).show(ui, |ui| {
                for w in ws {
                    ui.label(RichText::new(acct_name(&w.id)).color(if trade::MARGIN_IDS.contains(&w.id.as_str()) { fg() } else { mu() }));
                    ui.label(RichText::new(format!("{} USD", fmt_dp(w.usd.max(0.0), 2))).font(mono(11.5)));
                    // dust (under 1 cent) is not worth a slot
                    let coins = w.coins.iter().filter(|c| c.usd >= 0.01 || c.usd == 0.0 && c.qty >= 1e-4).take(5).map(|c| format!("{} {}", c.coin, fmt_qty(c.qty))).collect::<Vec<_>>().join("  ·  ");
                    ui.label(RichText::new(coins).font(mono(11.0)).color(mu()));
                    ui.end_row();
                }
            });
            for n in notes {
                let id = n.split(':').next().unwrap_or("");
                ui.label(RichText::new(format!("{}: {}", acct_name(id), note_hint(n))).font(prop(10.5)).color(WARN)).on_hover_text(n);
            }
            ui.add_space(6.0);
        }
    }

    /// Open the transfer dialog for `ex` (wallets are loaded first if needed).
    pub fn open_for(&mut self, ex: Exchange, eng: &Engine, ctx: &egui::Context) {
        let ws = eng.account.lock().unwrap().wallets.get(&ex).map(|w| w.0.clone());
        match ws {
            Some(ws) => self.open(ex, &ws),
            None => { eng.load_wallets(ex, ctx); self.xfer = Some(Xfer { ex, from: String::new(), to: String::new(), coin: "USDT".into(), amount: String::new() }); }
        }
    }

    fn open(&mut self, ex: Exchange, ws: &[Wallet]) {
        // default: first non-margin account holding something, into the margin account
        let margin = ws.iter().find(|w| trade::MARGIN_IDS.contains(&w.id.as_str())).map(|w| w.id.clone()).unwrap_or_default();
        // default source: the non-margin account with the most that can move
        let movable = |w: &&Wallet| w.coins.iter().filter(|c| c.free > 0.0).map(|c| if c.qty > 0.0 { c.usd * c.free / c.qty } else { 0.0 }).sum::<f64>();
        let from = ws.iter().filter(|w| w.id != margin).max_by(|a, b| movable(a).total_cmp(&movable(b))).filter(|w| movable(w) > 0.0)
            .map(|w| w.id.clone()).unwrap_or_else(|| margin.clone());
        self.xfer = Some(Xfer { ex, from, to: String::new(), coin: "USDT".into(), amount: String::new() });
    }

    pub fn modal(&mut self, ui: &mut Ui, eng: &Engine) {
        // opened before the wallets were in: pick the defaults once they arrive
        if let Some(x) = self.xfer.as_ref().filter(|x| x.from.is_empty()) {
            let ex = x.ex;
            let ws = eng.account.lock().unwrap().wallets.get(&ex).map(|w| w.0.clone());
            if let Some(ws) = ws { self.open(ex, &ws); }
        }
        let Some(x) = self.xfer.as_mut() else { return };
        let (ws, busy, mut auto) = {
            let a = eng.account.lock().unwrap();
            (a.wallets.get(&x.ex).map(|w| w.0.clone()).unwrap_or_default(), a.transferring, a.auto.get(&x.ex).copied().unwrap_or_default())
        };
        let pm = trade::is_pm();
        let mut close = false;
        let m = egui::Modal::new(egui::Id::new("transfer")).show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.horizontal(|ui| { icon_label(ui, x.ex, RichText::new(format!("{:?}", x.ex)).strong(), false); ui.label(RichText::new(t("wal.transfer")).strong()); });
            ui.separator();

            let sources: Vec<&Wallet> = ws.iter().filter(|w| w.coins.iter().any(|c| c.free > 0.0)).collect();
            let dests = trade::destinations(x.ex, &x.from, pm);
            if !dests.iter().any(|d| *d == x.to) { x.to = dests.first().map(|d| d.to_string()).unwrap_or_default(); }
            let from_w = ws.iter().find(|w| w.id == x.from);
            let coins: Vec<&trade::Coin> = from_w.map(|w| w.coins.iter().filter(|c| c.free > 0.0).collect()).unwrap_or_default();
            if !coins.iter().any(|c| c.coin == x.coin) { x.coin = coins.first().map(|c| c.coin.clone()).unwrap_or_default(); }
            let coin = coins.iter().find(|c| c.coin == x.coin).copied();
            let free = coin.map_or(0.0, |c| c.free);

            egui::Grid::new("xfer").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(RichText::new(t("wal.from")).color(mu()));
                egui::ComboBox::from_id_salt("xf_from").icon(chevron).width(200.0).selected_text(acct_name(&x.from)).show_ui(ui, |ui| {
                    for w in &sources { ui.selectable_value(&mut x.from, w.id.clone(), format!("{}   {} USD", acct_name(&w.id), fmt_dp(w.usd, 2))); }
                });
                ui.end_row();
                ui.label(RichText::new(t("wal.to")).color(mu()));
                egui::ComboBox::from_id_salt("xf_to").icon(chevron).width(200.0).selected_text(acct_name(&x.to)).show_ui(ui, |ui| {
                    for d in &dests { ui.selectable_value(&mut x.to, d.to_string(), acct_name(d)); }
                });
                ui.end_row();
                ui.label(RichText::new(t("wal.coin")).color(mu()));
                egui::ComboBox::from_id_salt("xf_coin").icon(chevron).width(200.0).selected_text(&x.coin).show_ui(ui, |ui| {
                    for c in &coins { ui.selectable_value(&mut x.coin, c.coin.clone(), format!("{}   {}", c.coin, fmt_qty(c.free))); }
                });
                ui.end_row();
                ui.label(RichText::new(t("wal.amount")).color(mu()));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut x.amount).desired_width(130.0).font(mono(12.5)));
                    if link(ui, t("wal.max")).clicked() { x.amount = format!("{free}"); }
                });
                ui.end_row();
            });
            ui.label(RichText::new(format!("{} {} {}", t("wal.free"), fmt_qty(free), x.coin)).font(prop(11.0)).color(dim()));
            // multi-step or slow routes are spelled out before confirming
            let via_spot = x.ex == Exchange::Binance && matches!((x.from.as_str(), x.to.as_str()), ("FUNDING", "PM") | ("PM", "FUNDING") | ("EARN", "PM") | ("EARN", "USDM"));
            if x.from == "EARN" { ui.label(RichText::new(t("wal.note_redeem")).font(prop(11.0)).color(WARN)); }
            if via_spot { ui.label(RichText::new(t("wal.note_via_spot")).font(prop(11.0)).color(WARN)); }
            if coin.is_some_and(|c| x.from == "EARN" && c.product.is_none()) { ui.label(RichText::new(t("wal.note_onchain")).font(prop(11.0)).color(dn())); }

            let amt: f64 = x.amount.trim().parse().unwrap_or(0.0);
            let valid = amt > 0.0 && amt <= free * (1.0 + 1e-9) && !x.to.is_empty() && !busy && coin.is_some_and(|c| x.from != "EARN" || c.product.is_some());
            ui.add_space(6.0);
            let ctx = ui.ctx().clone();
            ui.columns(2, |c| {
                if c[0].add(egui::Button::new(t("tr.cancel")).min_size(egui::vec2(c[0].available_width(), 30.0))).clicked() { close = true; }
                let label = if busy { t("wal.working") } else { t("wal.confirm") };
                if c[1].add_enabled(valid, egui::Button::new(RichText::new(label).strong()).fill(accent().linear_multiply(0.5)).min_size(egui::vec2(c[1].available_width(), 30.0))).clicked() {
                    eng.transfer(x.ex, x.from.clone(), x.to.clone(), x.coin.clone(), amt.min(free), coin.and_then(|c| c.product.clone()), &ctx);
                    close = true;
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.label(RichText::new(t("wal.auto_title")).strong());
            let before = auto;
            ui.checkbox(&mut auto.enabled, t("wal.auto_enable"));
            ui.horizontal(|ui| {
                ui.label(RichText::new(t("wal.auto_below")).color(mu()));
                ui.add(egui::DragValue::new(&mut auto.min).speed(10.0).range(0.0..=1e6).suffix(" USD"));
                ui.label(RichText::new(t("wal.auto_to")).color(mu()));
                ui.add(egui::DragValue::new(&mut auto.target).speed(10.0).range(0.0..=1e6).suffix(" USD"));
            });
            let src = if x.ex == Exchange::Bybit { t("wal.auto_src_bybit") } else { t("wal.auto_src_binance") };
            ui.label(RichText::new(format!("{}  {src}", t("wal.auto_note"))).font(prop(10.5)).color(dim()));
            if auto.enabled && auto.target <= auto.min { ui.label(RichText::new(t("wal.auto_bad")).font(prop(10.5)).color(dn())); }
            if auto != before { eng.account.lock().unwrap().auto.insert(x.ex, auto); }
        });
        if m.should_close() { close = true; }
        if close { self.xfer = None; }
    }
}
