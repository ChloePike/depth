//! Command bar (hotkey, default "/"): one line of words, each switching one thing, applied together
//! on Enter: a coin ("eth", "ethusdt"), a market ("perp", "spot", "margin", "opt"), an interval
//! ("1m" .. "1d"), a trading venue ("bybit"), or a page ("keys", "hotkeys", "settings").
//! Live preview under the line; an unknown word keeps the bar open and is named.
//! Words match loosely (prefix, contains, letters in order, one typo); the word being typed lists
//! what it can mean (coins with price, 24h change and volume), arrows pick, Enter or a click applies.
use super::hotkeys::{self, Act, Binding};
use super::{fmt_px, kit, l, theme::*, top::big};
use crate::engine::Ticker;
use depth::{Exchange, Market};
use eframe::egui::{self, pos2, vec2, Align2, Key, Modifiers, Rect, RichText, Sense};

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd { Base(String), Mode(Market), Tf(i64), Venue(Exchange), Settings(u8) }

impl Cmd {
    fn label(&self) -> String {
        match self {
            Cmd::Base(b) => format!("{b}/USDT"),
            Cmd::Mode(m) => format!("{m:?}"),
            Cmd::Tf(tf) => TFS.iter().find(|t| t.1 == *tf).map_or(String::new(), |t| t.0.to_uppercase()),
            Cmd::Venue(e) => match e { Exchange::Okx => "OKX".into(), Exchange::Mexc => "MEXC".into(), _ => format!("{e:?}") },
            Cmd::Settings(n) => l(["General", "Appearance", "Trading", "API Keys", "Hotkeys", "About"][*n as usize]).to_string(),
        }
    }
}

/// The chart's intervals (chart.rs TFS).
const TFS: [(&str, i64); 12] = [("1m", 1), ("5m", 5), ("15m", 15), ("1h", 60), ("4h", 240), ("1d", 1440),
    ("3m", 3), ("30m", 30), ("2h", 120), ("6h", 360), ("12h", 720), ("1w", 10080)];

/// Every word besides coins, in the order an exact match wins: interval, market, venue, page.
fn keywords() -> Vec<(String, Cmd)> {
    let mut k: Vec<(String, Cmd)> = TFS.iter().map(|t| (t.0.to_uppercase(), Cmd::Tf(t.1))).collect();
    for (w, m) in [("spot", Market::Spot), ("margin", Market::Margin), ("perp", Market::Perp), ("option", Market::Option), ("opt", Market::Option), ("opts", Market::Option),
        ("options", Market::Option), ("perps", Market::Perp), ("swap", Market::Perp), ("futures", Market::Perp)] { k.push((w.to_uppercase(), Cmd::Mode(m))); }
    // trading venues only: the word sets the order panel's venue
    k.extend(depth::trade::TRADABLE.map(|e| (format!("{e:?}").to_uppercase(), Cmd::Venue(e))));
    for (w, n) in [("settings", 0), ("prefs", 0), ("appearance", 1), ("theme", 1), ("trading", 2), ("routing", 2), ("demo", 2), ("keys", 3), ("api", 3), ("hotkeys", 4), ("shortcuts", 4)] {
        k.push((w.to_uppercase(), Cmd::Settings(n)));
    }
    k
}

/// How `w` matches `name` (both upper case): 0 exact, 1 prefix, 2 contains, 3 its letters in order,
/// 4 one typo (a letter wrong, missing, extra or swapped); None = no match.
fn score(w: &str, name: &str) -> Option<u8> {
    let (a, b) = (w.as_bytes(), name.as_bytes());
    if a == b { return Some(0); }
    if b.starts_with(a) { return Some(1); }
    if a.len() < 2 { return None; }
    if name.contains(w) { return Some(2); }
    let mut it = b.iter();
    if a.iter().all(|c| it.any(|n| n == c)) { return Some(3); }
    (a.len() >= 3 && one_typo(a, b)).then_some(4)
}

fn one_typo(a: &[u8], b: &[u8]) -> bool {
    let p = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[p..], &b[p..]);
    !(a.is_empty() && b.is_empty())
        && (a.get(1..) == b.get(1..) || a.get(1..) == Some(b) || Some(a) == b.get(1..)
            || (a.len() >= 2 && b.len() >= 2 && a[0] == b[1] && a[1] == b[0] && a[2..] == b[2..]))
}

/// Rows listed for the word being typed.
const SHOWN: usize = 8;

/// What one word can mean, best first (at most SHOWN): by `score`, keywords before coins, coins by
/// 24h volume. A keyword needs three letters unless exact, so a stray key never switches the venue.
fn candidates(word: &str, tickers: &[Ticker]) -> Vec<Cmd> {
    let w = word.to_uppercase();
    let coin = w.strip_suffix("/USDT").or(w.strip_suffix("USDT")).filter(|c| !c.is_empty()).unwrap_or(&w);
    let mut c: Vec<(u8, u8, f64, Cmd)> = keywords().into_iter()
        .filter_map(|(k, cmd)| score(&w, &k).filter(|s| *s == 0 || w.len() >= 3).map(|s| (s, 0, 0.0, cmd)))
        .chain(tickers.iter().filter_map(|t| score(coin, &t.base).map(|s| (s, 1, -t.quote_vol, Cmd::Base(t.base.clone())))))
        .collect();
    c.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)).then(x.2.total_cmp(&y.2)));
    let mut out: Vec<Cmd> = Vec::with_capacity(SHOWN);
    for (.., cmd) in c {
        if out.len() == SHOWN { break; }
        if !out.contains(&cmd) { out.push(cmd); }
    }
    out
}

/// Words to commands: each word takes its best candidate, the word being typed (no space after it)
/// its `pick`-th, the row selected under the line. Err names the first word nothing matches.
pub fn parse(text: &str, tickers: &[Ticker], pick: usize) -> Result<Vec<Cmd>, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let typing = !text.ends_with(char::is_whitespace);
    words.iter().enumerate().map(|(i, w)| {
        let c = candidates(w, tickers);
        let k = if typing && i + 1 == words.len() { pick } else { 0 };
        c.get(k).or(c.first()).cloned().ok_or_else(|| w.to_string())
    }).collect()
}

const ROW_H: f32 = 28.0;

/// One candidate row: icon and name; a coin adds its last price, 24h change and 24h volume.
fn row(ui: &mut egui::Ui, c: &Cmd, sel: bool, tickers: &[Ticker]) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let hv = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.1);
    if sel { ui.painter().rect_filled(r, kit::rad(), hl()); } else if hv > 0.0 { ui.painter().rect_filled(r, kit::rad(), hl().gamma_multiply(0.6 * hv)); }
    let y = r.center().y;
    let ic = Rect::from_center_size(pos2(r.left() + 16.0, y), vec2(18.0, 18.0));
    let name = if sel { fg_strong() } else { fg() };
    match c {
        Cmd::Base(b) => {
            coin_icon(ui, b, ic);
            let p = ui.painter();
            let x = p.text(pos2(r.left() + 34.0, y), Align2::LEFT_CENTER, b, prop_m(12.5), name).right();
            p.text(pos2(x + 1.0, y), Align2::LEFT_CENTER, "/USDT", prop(12.0), dim());
            if let Some(t) = tickers.iter().find(|t| &t.base == b) {
                p.text(pos2(r.left() + 290.0, y), Align2::RIGHT_CENTER, fmt_px(t.last), mono_m(12.5), fg());
                p.text(pos2(r.left() + 370.0, y), Align2::RIGHT_CENTER, format!("{:+.2}%", t.chg_pct), mono_m(12.5), if t.chg_pct >= 0.0 { up() } else { dn() });
                p.text(pos2(r.right() - 8.0, y), Align2::RIGHT_CENTER, big(t.quote_vol), mono(12.0), mu());
            }
        }
        _ => {
            if let Cmd::Venue(e) = c { icon(ui.painter(), *e, ic, false); }
            ui.painter().text(pos2(r.left() + 34.0, y), Align2::LEFT_CENTER, c.label(), prop_m(12.5), name);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Default)]
pub struct CommandBar { pub open: bool, text: String,
    /// selected row, and the text it was chosen for (a new text selects the first row)
    sel: usize, seen: String }

impl CommandBar {
    /// Opens the bar with `text` typed (T1_COMMAND screenshots).
    pub fn open_with(&mut self, text: &str) { self.open = true; self.text = text.to_string(); }

    /// The bar while open; the commands once Enter (GO) or a market key is pressed on a line that
    /// parses. A market key finishes the line like Bloomberg's yellow sector keys: `eth` + PERP.
    pub fn show(&mut self, ctx: &egui::Context, tickers: &[Ticker], binds: &[Binding]) -> Option<Vec<Cmd>> {
        if !self.open { return None; }
        let mut out = None;
        let markets: Vec<Binding> = binds.iter().filter(|b| matches!(b.act, Act::Spot | Act::Margin | Act::Perp | Act::Option)).cloned().collect();
        let market = hotkeys::pressed(ctx, &markets).first().map(|a| match a { Act::Spot => Market::Spot, Act::Margin => Market::Margin, Act::Perp => Market::Perp, _ => Market::Option });
        let m = egui::Modal::new(egui::Id::new("command_bar")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            // arrows move the row selection, taken before the line sees them
            let (down, up_k) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp)));
            let id = egui::Id::new("command_text");
            let r = ui.add(egui::TextEdit::singleline(&mut self.text).id(id).font(mono_m(15.0)).desired_width(f32::INFINITY)
                .hint_text(RichText::new("eth perp 1h bybit").color(dim())).margin(egui::vec2(10.0, 8.0)));
            r.request_focus();
            let word = if self.text.ends_with(char::is_whitespace) { None } else { self.text.split_whitespace().last() };
            let rows = word.map_or(vec![], |w| candidates(w, tickers));
            if self.text != self.seen { self.seen = self.text.clone(); self.sel = 0; }
            if !rows.is_empty() {
                if down { self.sel = (self.sel + 1).min(rows.len() - 1); }
                if up_k { self.sel = self.sel.saturating_sub(1); }
            }
            let mut clicked = None;
            if !rows.is_empty() {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, c) in rows.iter().enumerate() { if row(ui, c, i == self.sel, tickers).clicked() { clicked = Some(i); } }
                });
            }
            let parsed = parse(&self.text, tickers, clicked.unwrap_or(self.sel));
            match &parsed {
                Ok(cs) if cs.is_empty() => kit::note(ui, l("A coin, a market (perp, spot, margin, opt), an interval (1m … 1d), a venue, or keys / hotkeys / settings. Enter applies, Escape closes."), mu()),
                Ok(cs) => kit::note(ui, &cs.iter().map(Cmd::label).collect::<Vec<_>>().join("  ·  "), accent()),
                Err(w) => kit::note(ui, &format!("{}: {w}", l("Unknown")), dn()),
            }
            if let (Some(m), Ok(mut cs)) = (market, parsed.clone()) {
                cs.retain(|c| !matches!(c, Cmd::Mode(_)));
                cs.push(Cmd::Mode(m));
                out = Some(cs);
            } else if clicked.is_some() || ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Ok(cs) = parsed { if !cs.is_empty() { out = Some(cs); } }
            }
        });
        if out.is_some() || m.should_close() { self.open = false; self.text.clear(); }
        out
    }
}

/// HELP: every key binding and the command words, in one sheet.
pub fn help(ctx: &egui::Context, open: &mut bool, binds: &[Binding]) {
    if !*open { return; }
    let m = egui::Modal::new(egui::Id::new("help_sheet")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.spacing_mut().item_spacing.y = 6.0;
        kit::title(ui, l("Keys and commands"));
        kit::note(ui, l("Type a coin and press GO (Enter) or a market key: eth PERP. A line can also carry a market, an interval (1m … 1d), a venue, or keys / hotkeys / settings. Escape closes."), mu());
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            egui::Grid::new("help_grid").num_columns(2).spacing([24.0, 6.0]).striped(false).show(ui, |ui| {
                for b in binds.iter().filter(|b| b.key.is_some()) {
                    ui.label(RichText::new(l(b.act.name())).font(prop(12.5)).color(fg()));
                    ui.label(RichText::new(b.key.as_ref().map_or(String::new(), |c| c.label())).font(mono_m(12.5)).color(accent()));
                    ui.end_row();
                }
            });
        });
    });
    if m.should_close() { *open = false; }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tk(b: &str, v: f64) -> Ticker { Ticker { base: b.into(), last: 1.0, chg_pct: 0.0, quote_vol: v, high: 0.0, low: 0.0, base_vol: 0.0 } }
    fn base(b: &str) -> Cmd { Cmd::Base(b.into()) }

    #[test]
    fn words_become_commands() {
        let t = vec![tk("BTC", 9.0), tk("ETH", 5.0), tk("1INCH", 1.0)];
        assert_eq!(parse("eth perp 1h bybit", &t, 0), Ok(vec![base("ETH"), Cmd::Mode(Market::Perp), Cmd::Tf(60), Cmd::Venue(Exchange::Bybit)]));
        assert_eq!(parse("ETHUSDT opt", &t, 0), Ok(vec![base("ETH"), Cmd::Mode(Market::Option)]));
        assert_eq!(parse("1inch 1d", &t, 0), Ok(vec![base("1INCH"), Cmd::Tf(1440)]));
        assert_eq!(parse("keys", &t, 0), Ok(vec![Cmd::Settings(3)]));
        assert_eq!(parse("demo", &t, 0), Ok(vec![Cmd::Settings(2)]));
        assert_eq!(parse("eth nope", &t, 0), Err("nope".into()));
        assert_eq!(parse("  ", &t, 0), Ok(vec![]));
        assert_eq!(parse("3m 1w", &t, 0), Ok(vec![Cmd::Tf(3), Cmd::Tf(10080)]));
        assert_eq!(parse("mexc", &t, 0), Err("mexc".into()));
    }

    #[test]
    fn loose_matches() {
        let t = vec![tk("SOXL", 2.0), tk("SOL", 9.0), tk("SOLV", 1.0), tk("BTC", 10.0), tk("ETH", 5.0), tk("WETH", 7.0), tk("PERP", 3.0), tk("OP", 4.0)];
        // exact, then prefix by volume, then contains; keywords need three letters unless exact
        assert_eq!(candidates("so", &t), vec![base("SOL"), base("SOXL"), base("SOLV")]);
        assert_eq!(candidates("eth", &t), vec![base("ETH"), base("WETH")]);
        assert_eq!(candidates("sox", &t), vec![base("SOXL"), base("SOL")]);
        assert_eq!(candidates("b", &t), vec![base("BTC")]);
        // letters in order, one typo (wrong, missing, extra, swapped)
        assert_eq!(candidates("sxl", &t)[0], base("SOXL"));
        assert_eq!(candidates("btx", &t)[0], base("BTC"));
        assert_eq!(candidates("bt", &t)[0], base("BTC"));
        assert_eq!(candidates("eht", &t)[0], base("ETH"));
        assert_eq!(candidates("btcc", &t)[0], base("BTC"));
        // keywords: exact beats a same-name coin; a prefix of three letters; a typo
        assert_eq!(candidates("perp", &t)[0], Cmd::Mode(Market::Perp));
        assert_eq!(candidates("op", &t)[0], base("OP"));
        assert_eq!(candidates("byb", &t), vec![Cmd::Venue(Exchange::Bybit)]);
        assert_eq!(candidates("bybt", &t)[0], Cmd::Venue(Exchange::Bybit));
        assert_eq!(candidates("hyper", &t), vec![Cmd::Venue(Exchange::Hyperliquid)]);
        // the word being typed takes the picked row; finished words their best match
        assert_eq!(parse("so", &t, 1), Ok(vec![base("SOXL")]));
        assert_eq!(parse("so ", &t, 1), Ok(vec![base("SOL")]));
        assert_eq!(parse("sox perp", &t, 0), Ok(vec![base("SOXL"), Cmd::Mode(Market::Perp)]));
        assert_eq!(parse("so", &t, 7), Ok(vec![base("SOL")]));
        assert!(!one_typo(b"BTC", b"BTC"));
        assert!(!one_typo(b"BTC", b"ETH"));
    }
}
