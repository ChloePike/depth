//! Local SQLite store (`depth.db` in `sys::data_dir()`): the market history cache (`hist`,
//! clearable from Settings), the account's own fills (`fills`, kept: venues only serve recent
//! ones) and the demo account's event log (`demo_events`, kept). One connection behind a mutex;
//! every call is a few rows, fine from async tasks.

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
             CREATE INDEX IF NOT EXISTS fills_sym ON fills (symbol, ts);
             CREATE TABLE IF NOT EXISTS demo_events (id TEXT PRIMARY KEY, device TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL,
                 kind TEXT NOT NULL, body TEXT NOT NULL, UNIQUE (device, seq));
             CREATE INDEX IF NOT EXISTS demo_order ON demo_events (ts, device, seq);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
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

/// Insert fills; ones already stored (same venue and fill id) are left as they are. `demo`: they come
/// from the demo account (the keys they were read with), whatever the mode is by the time they arrive.
pub fn fills_put(fills: &[Fill], demo: bool) {
    // demo fills live in the demo log only, never among the real ones
    if demo || crate::demo::on() { return; }
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
    // demo mode reads the demo account's fills, never the real ones
    if crate::demo::on() { return crate::demo::fills(symbol, since); }
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

// ---------------------------------------------------------------- demo account event log

/// One stored event of the demo account (`demo::Ev`): `body` is its JSON, `kind` the tag. The log
/// is append-only (a reset is an event too) and Clear Cache leaves it, like `fills`. Sync-ready: a
/// server would keep the same rows keyed (device, seq); a client inserts the rows past its
/// per-device cursor here and folds again.
#[derive(Clone, Debug, PartialEq)]
pub struct DemoRow { pub id: String, pub device: String, pub seq: u64, pub ts: i64, pub kind: String, pub body: String }

/// Store events; rows already there (same id, or same device and seq) are skipped. Returns how
/// many were written: fewer than given for fresh events means the store failed.
pub fn demo_append(rows: &[DemoRow]) -> usize {
    let Some(mut c) = db() else { return 0 };
    let Ok(tx) = c.transaction() else { return 0 };
    let mut n = 0;
    {
        let Ok(mut st) = tx.prepare_cached("INSERT OR IGNORE INTO demo_events (id, device, seq, ts, kind, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)") else { return 0 };
        for r in rows { n += st.execute(params![r.id, r.device, r.seq as i64, r.ts, r.kind, r.body]).unwrap_or(0); }
    }
    if tx.commit().is_err() { return 0; }
    n
}

/// Every stored event in replay order (ts, device, seq).
pub fn demo_rows() -> Vec<DemoRow> {
    let Some(c) = db() else { return vec![] };
    let Ok(mut st) = c.prepare_cached("SELECT id, device, seq, ts, kind, body FROM demo_events ORDER BY ts, device, seq") else { return vec![] };
    st.query_map([], |r| Ok(DemoRow { id: r.get(0)?, device: r.get(1)?, seq: r.get::<_, i64>(2)? as u64, ts: r.get(3)?, kind: r.get(4)?, body: r.get(5)? }))
        .map(|rows| rows.flatten().collect()).unwrap_or_default()
}

/// Highest seq stored for `device`, 0 when none.
pub fn demo_last_seq(device: &str) -> u64 {
    db().and_then(|c| c.query_row("SELECT MAX(seq) FROM demo_events WHERE device = ?1", [device], |r| r.get::<_, Option<i64>>(0)).ok().flatten()).unwrap_or(0) as u64
}

/// This installation's id in the demo log, made once. It is kept in this database next to the
/// sequence numbers it scopes, so the two can never part (a restored log keeps its numbering);
/// "local" without a database.
pub fn device_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let Some(c) = db() else { return "local".into() };
        let get = || c.query_row("SELECT value FROM meta WHERE key = 'device'", [], |r| r.get::<_, String>(0)).ok();
        get().unwrap_or_else(|| {
            let id = crate::trade::uuid_v4();
            let _ = c.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('device', ?1)", [&id]);
            get().unwrap_or(id)
        })
    }).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("depth-db-{}", std::process::id()));
        unsafe { std::env::set_var("T1_DB", dir.join("t.db")); }
        let f = |id: &str, ts| Fill { ex: Exchange::Bybit, id: id.into(), symbol: "XUSDT".into(), side: Side::Sell, price: 2.0, qty: 3.0, fee: 0.1, realized: None, ts };
        fills_put(&[f("a", 10), f("b", 20), f("a", 10)], false);
        assert_eq!(fills("XUSDT", 0).len(), 2);
        assert_eq!(fills("XUSDT", 15).len(), 1);
        assert_eq!(fills_last(Exchange::Bybit, "XUSDT"), Some(20));
        hist_put("k", b"abc");
        assert_eq!(hist_get("k").as_deref(), Some(&b"abc"[..]));
        assert_eq!(hist_clear(None), 3);
        assert!(hist_get("k").is_none());
        // demo log: idempotent appends, replay order, per-device cursor, stable device id
        let row = |id: &str, device: &str, seq, ts| DemoRow { id: id.into(), device: device.into(), seq, ts, kind: "Reset".into(), body: "{}".into() };
        assert_eq!(demo_append(&[row("e2", "d", 2, 20), row("e1", "d", 1, 10)]), 2);
        assert_eq!(demo_append(&[row("e1", "d", 1, 10), row("other", "d", 2, 30)]), 0);
        assert_eq!(demo_append(&[row("x1", "b", 1, 10)]), 1);
        assert_eq!(demo_rows().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["x1", "e1", "e2"]);
        assert_eq!((demo_last_seq("d"), demo_last_seq("b"), demo_last_seq("none")), (2, 1, 0));
        let id = device_id();
        assert_eq!((id.len(), device_id()), (36, id));
        // demo mode keeps its fills out of the real table
        crate::demo::set_on(true);
        fills_put(&[f("demo", 30)], false);
        // and reads only the demo account's (a fresh one here: none)
        assert!(fills("XUSDT", 0).is_empty());
        crate::demo::set_on(false);
        assert_eq!(fills("XUSDT", 0).len(), 2);
        // read with demo keys, arriving after demo mode was switched off: still not real
        fills_put(&[f("late-demo", 40)], true);
        assert_eq!(fills("XUSDT", 0).len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
