//! One module per exchange; each exposes `spawn(sub, tx)`.
pub mod binance;
pub mod bitget;
pub mod bybit;
pub mod coinbase;
pub mod gate;
pub mod hyperliquid;
pub mod kraken;
pub mod lighter;
pub mod mexc;
pub mod okx;

use crate::{Exchange, History, Sub, Tx};
use tokio::task::JoinHandle;

/// Starts all tasks needed to stream `sub` from `ex`. Empty if the market is unsupported there.
pub fn spawn(ex: Exchange, sub: &Sub, tx: Tx) -> Vec<JoinHandle<()>> {
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
    }
}

/// Recent REST history (about `minutes` one-minute bars) for seeding the chart.
pub async fn history(ex: Exchange, sub: &Sub, minutes: usize) -> anyhow::Result<History> {
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
    }
}

/// Recent REST history at `tf`-minute resolution (`bars` candles), for chart timeframes above 1m.
/// OI / long-short / taker series use the venue's closest period not finer than needed.
pub async fn history_tf(ex: Exchange, sub: &Sub, tf: u32, bars: usize) -> anyhow::Result<History> {
    match ex {
        Exchange::Binance => binance::history_tf(sub, tf, bars).await,
        Exchange::Bybit => bybit::history_tf(sub, tf, bars).await,
        Exchange::Bitget => bitget::history_tf(sub, tf, bars).await,
        Exchange::Okx => okx::history_tf(sub, tf, bars).await,
        Exchange::Mexc => mexc::history_tf(sub, tf, bars).await,
        Exchange::Coinbase => coinbase::history_tf(sub, tf, bars).await,
        Exchange::Kraken => kraken::history_tf(sub, tf, bars).await,
        Exchange::Gate => gate::history_tf(sub, tf, bars).await,
        Exchange::Hyperliquid => hyperliquid::history_tf(sub, tf, bars).await,
        Exchange::Lighter => lighter::history_tf(sub, tf, bars).await,
    }
}
