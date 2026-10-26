//! One module per exchange; each exposes `spawn(sub, tx)`.
pub mod binance;
pub mod bitget;
pub mod bybit;
pub mod coinbase;
pub mod deribit;
pub mod gate;
pub mod hyperliquid;
pub mod kraken;
pub mod lighter;
pub mod mexc;
pub mod okx;

use crate::{Exchange, History, Sub, Tx};
use tokio::task::JoinHandle;

/// `sub` in the venue's own naming (`crate::alias`: XAU is "xyz:GOLD" on Hyperliquid); None when
/// the venue's same-name market is a different asset, so the venue is left out.
pub fn venue_sub(ex: Exchange, sub: &Sub) -> Option<Sub> {
    crate::alias::venue_base(ex, sub.market, &sub.base).map(|base| Sub { base, ..sub.clone() })
}

/// Starts all tasks needed to stream `sub` from `ex`. Empty if the market is unsupported there.
pub fn spawn(ex: Exchange, sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
    let Some(sub) = &venue_sub(ex, sub) else { return vec![] };
    match ex {
        Exchange::Binance => binance::spawn(sub, tx),
        Exchange::Bybit => bybit::spawn(sub, tx),
        Exchange::Bitget => bitget::spawn(sub, tx),
        Exchange::Okx => okx::spawn(sub, tx),
        Exchange::Mexc => mexc::spawn(sub, tx),
        Exchange::Coinbase => coinbase::spawn(sub, tx),
        Exchange::Kraken => kraken::spawn(sub, tx),
        Exchange::Gate => gate::spawn(sub, tx),
        Exchange::Hyperliquid => hyperliquid::spawn(sub, tx),
        Exchange::Lighter => lighter::spawn(sub, tx),
        Exchange::Deribit => deribit::spawn(sub, tx),
    }
}

/// Recent REST history (about `minutes` one-minute bars) for seeding the chart.
pub async fn history(ex: Exchange, sub: &Sub, minutes: usize) -> anyhow::Result<History> {
    let Some(sub) = &venue_sub(ex, sub) else { return Ok(History::default()) };
    match ex {
        Exchange::Binance => binance::history(sub, minutes).await,
        Exchange::Bybit => bybit::history(sub, minutes).await,
        Exchange::Bitget => bitget::history(sub, minutes).await,
        Exchange::Okx => okx::history(sub, minutes).await,
        Exchange::Mexc => mexc::history(sub, minutes).await,
        Exchange::Coinbase => coinbase::history(sub, minutes).await,
        Exchange::Kraken => kraken::history(sub, minutes).await,
        Exchange::Gate => gate::history(sub, minutes).await,
        Exchange::Hyperliquid => hyperliquid::history(sub, minutes).await,
        Exchange::Lighter => lighter::history(sub, minutes).await,
        Exchange::Deribit => deribit::history(sub, minutes).await,
    }
}

/// Recent REST history at `tf`-minute resolution (`bars` candles), for chart timeframes above 1m.
/// OI / long-short / taker series use the venue's closest period not finer than needed.
pub async fn history_tf(ex: Exchange, sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> { history_until(ex, sub, tf, bars, None).await }

/// `history_tf` for the `bars` candles ending at the one containing `until` ms (None = now): the page
/// before data already loaded. A venue that cannot reach that far back returns less, or nothing.
pub async fn history_until(ex: Exchange, sub: &Sub, tf: u32, bars: usize, until: Option<i64>) -> anyhow::Result<History> {
    let Some(sub) = &venue_sub(ex, sub) else { return Ok(History::default()) };
    match ex {
        Exchange::Binance => binance::history_until(sub, tf, bars, until).await,
        Exchange::Bybit => bybit::history_until(sub, tf, bars, until).await,
        Exchange::Bitget => bitget::history_until(sub, tf, bars, until).await,
        Exchange::Okx => okx::history_until(sub, tf, bars, until).await,
        Exchange::Mexc => mexc::history_until(sub, tf, bars, until).await,
        Exchange::Coinbase => coinbase::history_until(sub, tf, bars, until).await,
        Exchange::Kraken => kraken::history_until(sub, tf, bars, until).await,
        Exchange::Gate => gate::history_until(sub, tf, bars, until).await,
        Exchange::Hyperliquid => hyperliquid::history_until(sub, tf, bars, until).await,
        Exchange::Lighter => lighter::history_until(sub, tf, bars, until).await,
        Exchange::Deribit => deribit::history_until(sub, tf, bars, until).await,
    }
}
