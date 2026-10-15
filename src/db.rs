//! Local SQLite store (`depth.db` in `sys::data_dir()`): the market history cache (`hist`,
//! clearable from Settings) and the account's own fills (`fills`, kept: venues only serve recent
//! ones). One connection behind a mutex; every call is a few rows, fine from async tasks.

use crate::trade::Fill;
use crate::{Exchange, Side};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Mutex, MutexGuard, OnceLock};

static DB: OnceLock<Option<Mutex<Connection>>> = OnceLock::new();

fn db() -> Option<MutexGuard<'static, Connection>> {
    DB.get_or_init(|| {
        let path = std::env::var_os("T1_DB").map(Into::into).or_else(|| Some(crate::sys::data_dir()?.join("depth.db")))?;
        if let Some(d) = std::path::Path::new(&path).parent() { let _ = std::fs::create_dir_all(d); }
        let c = Connection::open(&path).map_err(|e| eprintln!("[db] {e}")).ok()?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS hist (key TEXT PRIMARY KEY, body BLOB NOT NULL, updated INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS fills (ex TEXT NOT NULL, id TEXT NOT NULL, symbol TEXT NOT NULL, buy INTEGER NOT NULL,
                 price REAL NOT NULL, qty REAL NOT NULL, fee REAL NOT NULL, realized REAL, ts INTEGER NOT NULL, PRIMARY KEY (ex, id));
             CREATE INDEX IF NOT EXISTS fills_sym ON fills (symbol, ts);",
        ).map_err(|e| eprintln!("[db] {e}")).ok()?;
        // the JSON files the history cache used before
        if let Some(d) = crate::sys::cache_dir() { let _ = std::fs::remove_dir_all(d.join("history")); }
        Some(Mutex::new(c))
    }).as_ref().map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
}

// ---------------------------------------------------------------- market history cache

pub fn hist_get(key: &str) -> Option<Vec<u8>> {
    db()?.query_row("SELECT body FROM hist WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
}

pub fn hist_put(key: &str, body: &[u8]) {
    if let Some(c) = db() {
        let _ = c.execute("INSERT OR REPLACE INTO hist (key, body, updated) VALUES (?1, ?2, ?3)", params![key, body, crate::now_ms()]);
    }
}

pub fn hist_bytes() -> u64 {
    db().and_then(|c| c.query_row("SELECT COALESCE(SUM(LENGTH(body)), 0) FROM hist", [], |r| r.get::<_, i64>(0)).ok()).unwrap_or(0) as u64
}

/// Drop the whole cache (`days` None) or entries not refreshed for `days`; returns the bytes freed.
pub fn hist_clear(days: Option<u64>) -> u64 {
    let Some(c) = db() else { return 0 };
    let cutoff = days.map_or(i64::MAX, |d| crate::now_ms() - d as i64 * 86_400_000);
    let n: i64 = c.query_row("SELECT COALESCE(SUM(LENGTH(body)), 0) FROM hist WHERE updated < ?1", [cutoff], |r| r.get(0)).unwrap_or(0);
    let _ = c.execute("DELETE FROM hist WHERE updated < ?1", [cutoff]);
    if n > 0 { let _ = c.execute_batch("VACUUM"); }
    n as u64
}

// ---------------------------------------------------------------- own fills

/// Insert fills; ones already stored (same venue and fill id) are left as they are.
pub fn fills_put(fills: &[Fill]) {
    let Some(mut c) = db() else { return };
    let Ok(tx) = c.transaction() else { return };
    {
        let Ok(mut st) = tx.prepare_cached("INSERT OR IGNORE INTO fills (ex, id, symbol, buy, price, qty, fee, realized, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)") else { return };
        for f in fills.iter().filter(|f| !f.id.is_empty()) {
            let _ = st.execute(params![format!("{:?}", f.ex), f.id, f.symbol, f.side == Side::Buy, f.price, f.qty, f.fee, f.realized, f.ts]);
        }
    }
    let _ = tx.commit();
}

/// Newest stored fill time of a venue's symbol.
pub fn fills_last(ex: Exchange, symbol: &str) -> Option<i64> {
    db()?.query_row("SELECT MAX(ts) FROM fills WHERE ex = ?1 AND symbol = ?2", params![format!("{ex:?}"), symbol], |r| r.get(0)).ok().flatten()
}

/// Every venue's fills of a symbol since `since` (ms), oldest first.
pub fn fills(symbol: &str, since: i64) -> Vec<Fill> {
    let Some(c) = db() else { return vec![] };
    let Ok(mut st) = c.prepare_cached("SELECT ex, id, buy, price, qty, fee, realized, ts FROM fills WHERE symbol = ?1 AND ts >= ?2 ORDER BY ts") else { return vec![] };
    st.query_map(params![symbol, since], |r| {
        let ex: String = r.get(0)?;
        let Some(ex) = Exchange::ALL.iter().copied().find(|e| format!("{e:?}") == ex) else { return Ok(None) };
        Ok(Some(Fill {
            ex, id: r.get(1)?, symbol: symbol.to_string(), side: if r.get(2)? { Side::Buy } else { Side::Sell },
            price: r.get(3)?, qty: r.get(4)?, fee: r.get(5)?, realized: r.get(6)?, ts: r.get(7)?,
        }))
    }).map(|rows| rows.flatten().flatten().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("depth-db-{}", std::process::id()));
        unsafe { std::env::set_var("T1_DB", dir.join("t.db")); }
        let f = |id: &str, ts| Fill { ex: Exchange::Bybit, id: id.into(), symbol: "XUSDT".into(), side: Side::Sell, price: 2.0, qty: 3.0, fee: 0.1, realized: None, ts };
        fills_put(&[f("a", 10), f("b", 20), f("a", 10)]);
        assert_eq!(fills("XUSDT", 0).len(), 2);
        assert_eq!(fills("XUSDT", 15).len(), 1);
        assert_eq!(fills_last(Exchange::Bybit, "XUSDT"), Some(20));
        hist_put("k", b"abc");
        assert_eq!(hist_get("k").as_deref(), Some(&b"abc"[..]));
        assert_eq!(hist_clear(None), 3);
        assert!(hist_get("k").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
