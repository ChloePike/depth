//! Rebindable keyboard shortcuts (Settings > Hotkeys). A binding is the raw modifiers plus an egui
//! key name, matched exactly and never on key repeat, so holding a trading key sends one order.
//! Bindings fire only while no text field has focus and no dialog is open (App::shortcuts).
use eframe::egui::{self, Key, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Act {
    // trading (perp order panel): preset notional at the touch or at market; cancel / flatten the symbol
    BuyBid, SellAsk, BuyMarket, SellMarket, CancelAll, Flatten,
    // navigation
    Picker, PrevFav, NextFav, Star, PrevVenue, NextVenue,
    Spot, Margin, Perp, Option,
    Tf1m, Tf5m, Tf15m, Tf1h, Tf4h, Tf1d,
    OrderPanel, Settings,
}

impl Act {
    pub const ALL: [Act; 24] = [Act::BuyBid, Act::SellAsk, Act::BuyMarket, Act::SellMarket, Act::CancelAll, Act::Flatten,
        Act::Picker, Act::PrevFav, Act::NextFav, Act::Star, Act::PrevVenue, Act::NextVenue,
        Act::Spot, Act::Margin, Act::Perp, Act::Option, Act::Tf1m, Act::Tf5m, Act::Tf15m, Act::Tf1h, Act::Tf4h, Act::Tf1d,
        Act::OrderPanel, Act::Settings];

    pub fn trading(self) -> bool { matches!(self, Act::BuyBid | Act::SellAsk | Act::BuyMarket | Act::SellMarket | Act::CancelAll | Act::Flatten) }

    /// English label (translated with `l`).
    pub fn name(self) -> &'static str {
        match self {
            Act::BuyBid => "Buy: limit at the bid", Act::SellAsk => "Sell: limit at the ask",
            Act::BuyMarket => "Buy at market", Act::SellMarket => "Sell at market",
            Act::CancelAll => "Cancel all orders of this symbol on the trading venue", Act::Flatten => "Close all positions of this symbol on the trading venue at market",
            Act::Picker => "Choose pair", Act::PrevFav => "Previous favorite", Act::NextFav => "Next favorite", Act::Star => "Star the current pair",
            Act::PrevVenue => "Previous trading venue", Act::NextVenue => "Next trading venue",
            Act::Spot => "Spot mode", Act::Margin => "Margin mode", Act::Perp => "Perp mode", Act::Option => "Options mode",
            Act::Tf1m => "1 minute", Act::Tf5m => "5 minutes", Act::Tf15m => "15 minutes", Act::Tf1h => "1 hour", Act::Tf4h => "4 hours", Act::Tf1d => "1 day",
            Act::OrderPanel => "Show / hide the order panel", Act::Settings => "Settings",
        }
    }
}

/// Raw modifiers (bit 1 cmd/meta, 2 ctrl, 4 alt, 8 shift) and the egui key name.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Combo { pub mods: u8, pub key: String }

impl Combo {
    fn new(m: Modifiers, key: Key) -> Self {
        Combo { mods: m.mac_cmd as u8 | (m.ctrl as u8) << 1 | (m.alt as u8) << 2 | (m.shift as u8) << 3, key: key.name().to_string() }
    }

    /// "⌃⌥⇧⌘K" on macOS, "Ctrl+Alt+Shift+K" elsewhere.
    pub fn label(&self) -> String {
        let key = Key::from_name(&self.key).map_or(self.key.clone(), |k| k.symbol_or_name().to_string());
        let mac = cfg!(target_os = "macos");
        let mut s = String::new();
        for (bit, m, o) in [(2, "⌃", "Ctrl+"), (4, "⌥", "Alt+"), (8, "⇧", "Shift+"), (1, "⌘", "Win+")] {
            if self.mods & bit != 0 { s.push_str(if mac { m } else { o }); }
        }
        s + &key
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Binding { pub act: Act, pub key: Option<Combo> }

/// The platform's command key: ⌘ on macOS, Ctrl elsewhere.
fn cmd() -> Modifiers { if cfg!(target_os = "macos") { Modifiers::MAC_CMD } else { Modifiers::CTRL } }

/// The shortcuts the app always had; trading keys start unbound (opt in, after setting a size).
pub fn defaults() -> Vec<Binding> {
    let c = cmd();
    let k = |m: Modifiers, key: Key| Some(Combo::new(m, key));
    Act::ALL.iter().map(|&act| Binding { act, key: match act {
        Act::Picker => k(c, Key::K), Act::PrevFav => k(c, Key::OpenBracket), Act::NextFav => k(c, Key::CloseBracket), Act::Star => k(c, Key::D),
        Act::Spot => k(c, Key::Num1), Act::Margin => k(c, Key::Num2), Act::Perp => k(c, Key::Num3), Act::Option => k(c, Key::Num4),
        Act::Tf1m => k(Modifiers::CTRL, Key::Num1), Act::Tf5m => k(Modifiers::CTRL, Key::Num2), Act::Tf15m => k(Modifiers::CTRL, Key::Num3),
        Act::Tf1h => k(Modifiers::CTRL, Key::Num4), Act::Tf4h => k(Modifiers::CTRL, Key::Num5), Act::Tf1d => k(Modifiers::CTRL, Key::Num6),
        Act::OrderPanel => k(c | Modifiers::ALT, Key::I), Act::Settings => k(c, Key::Comma),
        _ => None,
    } }).collect()
}

/// Bindings as saved, completed with any action added since (at its default).
pub fn complete(saved: &[Binding]) -> Vec<Binding> {
    defaults().into_iter().map(|d| saved.iter().find(|b| b.act == d.act).cloned().unwrap_or(d)).collect()
}

/// Actions whose key went down this frame (not repeats); their key events are consumed.
pub fn pressed(ctx: &egui::Context, binds: &[Binding]) -> Vec<Act> {
    let mut out = vec![];
    ctx.input_mut(|i| i.events.retain(|e| {
        let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = e else { return true };
        let c = Combo::new(*modifiers, *key);
        match binds.iter().find(|b| b.key.as_ref() == Some(&c)) {
            Some(b) => { if !repeat { out.push(b.act); } false }
            None => true,
        }
    }));
    out
}

/// What a key press does while a binding is being recorded.
pub enum Rec { Cancel, Clear, Set(Combo) }

/// The next key pressed with its modifiers, for recording a binding: Escape cancels, a bare
/// Backspace / Delete clears. Lone modifier presses are not key events in egui, so they wait.
pub fn record(ctx: &egui::Context) -> Option<Rec> {
    ctx.input_mut(|i| {
        let mut got = None;
        i.events.retain(|e| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
            got.get_or_insert(match key {
                Key::Escape => Rec::Cancel,
                Key::Backspace | Key::Delete if modifiers.is_none() => Rec::Clear,
                _ => Rec::Set(Combo::new(*modifiers, *key)),
            });
            false
        });
        got
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_one_default_and_no_duplicate_keys() {
        let d = defaults();
        assert_eq!(d.len(), Act::ALL.len());
        let keys: Vec<&Combo> = d.iter().filter_map(|b| b.key.as_ref()).collect();
        for (i, k) in keys.iter().enumerate() { assert!(!keys[i + 1..].contains(k), "duplicate default {}", k.label()); }
        assert!(d.iter().filter(|b| b.act.trading()).all(|b| b.key.is_none()), "trading keys start unbound");
    }

    #[test]
    fn saved_bindings_keep_new_actions() {
        let saved = vec![Binding { act: Act::Picker, key: None }];
        let c = complete(&saved);
        assert_eq!(c.len(), Act::ALL.len());
        assert!(c.iter().find(|b| b.act == Act::Picker).unwrap().key.is_none());
        assert!(c.iter().find(|b| b.act == Act::Settings).unwrap().key.is_some());
    }
}
