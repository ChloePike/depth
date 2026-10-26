//! Deribit (JSON-RPC over wss://www.deribit.com/ws/api/v2, REST under /api/v2). No streams yet.
use crate::*;
use tokio::task::JoinHandle;

// ponytail: options chain, perps and dated futures come in later batches; no tasks until then
pub fn spawn(_sub: &Sub, _tx: Tx) -> Vec<JoinHandle<()>> { vec![] }

pub async fn history(sub: &Sub, minutes: usize) -> anyhow::Result<History> { history_tf(sub, 1, minutes).await }

pub async fn history_tf(_sub: &Sub, _tf: u32, _bars: usize) -> anyhow::Result<History> { Ok(History::default()) }
