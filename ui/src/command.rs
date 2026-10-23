//! Command bar (hotkey, default "/"): one line of words, each switching one thing, applied together
//! on Enter: a coin ("eth", "ethusdt"), a market ("perp", "spot", "margin", "opt"), an interval
//! ("1m" .. "1d"), a trading venue ("bybit"), or a page ("keys", "hotkeys", "settings").
//! Live preview under the line; an unknown word keeps the bar open and is named.
use super::{kit, l, theme::*};
use depth::{Exchange, Market};
use eframe::egui::{self, RichText};

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd { Base(String), Mode(Market), Tf(i64), Venue(Exchange), Settings(u8) }

impl Cmd {
    fn label(&self) -> String {
        match self {
            Cmd::Base(b) => format!("{b}/USDT"),
            Cmd::Mode(m) => format!("{m:?}"),
            Cmd::Tf(tf) => TFS.iter().find(|t| t.1 == *tf).map_or(String::new(), |t| t.0.to_uppercase()),
            Cmd::Venue(e) => format!("{e:?}"),
            Cmd::Settings(n) => l(["General", "Appearance", "Trading", "API Keys", "Hotkeys", "About"][*n as usize]).to_string(),
        }
    }
}

const TFS: [(&str, i64); 6] = [("1m", 1), ("5m", 5), ("15m", 15), ("1h", 60), ("4h", 240), ("1d", 1440)];

/// Words to commands; `bases` are the listed coins (upper case). Err names the first unknown word.
pub fn parse(text: &str, bases: &[String]) -> Result<Vec<Cmd>, String> {
    text.split_whitespace().map(|w| {
        let lw = w.to_lowercase();
        if let Some(t) = TFS.iter().find(|t| t.0 == lw) { return Ok(Cmd::Tf(t.1)); }
        let mode = match lw.as_str() { "opt" | "opts" | "options" => Some(Market::Option), "perps" | "swap" | "futures" => Some(Market::Perp), _ => Market::parse(&lw) };
        if let Some(m) = mode { return Ok(Cmd::Mode(m)); }
        if let Some(e) = Exchange::parse(&lw) { return Ok(Cmd::Venue(e)); }
        let page = match lw.as_str() { "settings" | "prefs" => Some(0), "appearance" | "theme" => Some(1), "trading" | "routing" => Some(2), "keys" | "api" => Some(3), "hotkeys" | "shortcuts" => Some(4), _ => None };
        if let Some(n) = page { return Ok(Cmd::Settings(n)); }
        let up = w.to_uppercase();
        let coin = up.strip_suffix("/USDT").or(up.strip_suffix("USDT")).filter(|c| !c.is_empty()).unwrap_or(&up);
        if bases.iter().any(|b| b == coin) { return Ok(Cmd::Base(coin.to_string())); }
        Err(w.to_string())
    }).collect()
}

#[derive(Default)]
pub struct CommandBar { pub open: bool, text: String }

impl CommandBar {
    /// Opens the bar with `text` typed (T1_COMMAND screenshots).
    pub fn open_with(&mut self, text: &str) { self.open = true; self.text = text.to_string(); }

    /// The bar while open; the commands once Enter is pressed on a line that parses.
    pub fn show(&mut self, ctx: &egui::Context, bases: &[String]) -> Option<Vec<Cmd>> {
        if !self.open { return None; }
        let mut out = None;
        let m = egui::Modal::new(egui::Id::new("command_bar")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let id = egui::Id::new("command_text");
            let r = ui.add(egui::TextEdit::singleline(&mut self.text).id(id).font(mono_m(15.0)).desired_width(f32::INFINITY)
                .hint_text(RichText::new("eth perp 1h bybit").color(dim())).margin(egui::vec2(10.0, 8.0)));
            r.request_focus();
            let parsed = parse(&self.text, bases);
            match &parsed {
                Ok(cs) if cs.is_empty() => kit::note(ui, l("A coin, a market (perp, spot, margin, opt), an interval (1m … 1d), a venue, or keys / hotkeys / settings. Enter applies, Escape closes."), mu()),
                Ok(cs) => kit::note(ui, &cs.iter().map(Cmd::label).collect::<Vec<_>>().join("  ·  "), accent()),
                Err(w) => kit::note(ui, &format!("{}: {w}", l("Unknown")), dn()),
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Ok(cs) = parsed { if !cs.is_empty() { out = Some(cs); } }
            }
        });
        if out.is_some() || m.should_close() { self.open = false; self.text.clear(); }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_become_commands() {
        let bases = vec!["BTC".to_string(), "ETH".to_string(), "1INCH".to_string()];
        assert_eq!(parse("eth perp 1h bybit", &bases), Ok(vec![Cmd::Base("ETH".into()), Cmd::Mode(Market::Perp), Cmd::Tf(60), Cmd::Venue(Exchange::Bybit)]));
        assert_eq!(parse("ETHUSDT opt", &bases), Ok(vec![Cmd::Base("ETH".into()), Cmd::Mode(Market::Option)]));
        assert_eq!(parse("1inch 1d", &bases), Ok(vec![Cmd::Base("1INCH".into()), Cmd::Tf(1440)]));
        assert_eq!(parse("keys", &bases), Ok(vec![Cmd::Settings(3)]));
        assert_eq!(parse("eth nope", &bases), Err("nope".into()));
        assert_eq!(parse("  ", &bases), Ok(vec![]));
    }
}
