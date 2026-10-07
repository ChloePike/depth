//! WebSocket session loop (reconnect, app-level ping, idle timeout) and REST polling.
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep, timeout};
use yawc::{frame::{Frame as WsFrame, OpCode}, Options, WebSocket};

pub enum Frame<'a> { Text(&'a str), Binary(&'a [u8]) }

pub struct Spec {
    pub name: String,
    pub url: String,
    /// messages sent in order right after connecting
    pub subs: Vec<String>,
    /// app-level heartbeat: (interval, message)
    pub ping: Option<(Duration, String)>,
    /// reconnect if nothing arrives for this long
    pub idle: Duration,
    /// send `subs[0]` (a login), wait for the server's first reply, then send the rest:
    /// venues reject subscriptions that arrive before the login is acknowledged
    pub login_first: bool,
    /// with `login_first`: build extra messages from the first reply (e.g. sign a challenge);
    /// they are sent before the remaining `subs`
    pub on_login: Option<Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>>,
}

impl Spec {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Spec { name: name.into(), url: url.into(), subs: vec![], ping: None, idle: Duration::from_secs(60), login_first: false, on_login: None }
    }
    pub fn sub(mut self, s: impl Into<String>) -> Self { self.subs.push(s.into()); self }
    pub fn ping(mut self, every: Duration, msg: impl Into<String>) -> Self { self.ping = Some((every, msg.into())); self }
    pub fn login_first(mut self) -> Self { self.login_first = true; self }
    /// login_first + messages derived from the first reply (challenge / response handshakes)
    pub fn on_login(mut self, f: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self { self.login_first = true; self.on_login = Some(Arc::new(f)); self }
}

/// Runs forever, reconnecting with exponential backoff. The same handler is reused across
/// reconnects; connectors with state must reset it when a new snapshot arrives.
pub async fn run(spec: Spec, mut handler: impl FnMut(Frame) + Send) {
    let mut backoff = 1u64;
    loop {
        let started = Instant::now();
        match session(&spec, &mut |f| { handler(f); true }).await {
            Ok(()) => eprintln!("[{}] connection closed, reconnecting", spec.name),
            Err(e) => eprintln!("[{}] {e:#}, reconnecting", spec.name),
        }
        if started.elapsed() > Duration::from_secs(60) { backoff = 1; }
        sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Always offers permessage-deflate; servers that do not support it simply decline
/// (measured on Binance depth: ~3x fewer bytes on the wire).
/// Message limit is 64 MiB: full-book snapshots (e.g. Coinbase BTC-USD level2) exceed yawc's 1 MiB default.
/// One connection: returns when the server closes or the handler returns false; the caller
/// decides whether and how to reconnect (private streams re-snapshot first).
pub async fn once(spec: &Spec, mut handler: impl FnMut(Frame) -> bool + Send) -> Result<()> { session(spec, &mut handler).await }

async fn session(spec: &Spec, handler: &mut (impl FnMut(Frame) -> bool + Send)) -> Result<()> {
    let opts = Options::default().with_low_latency_compression().with_limits(64 << 20, 64 << 20);
    let mut ws = timeout(Duration::from_secs(15), WebSocket::connect(spec.url.parse()?).with_options(opts)).await??;
    let mut rest = &spec.subs[..];
    if spec.login_first && !rest.is_empty() {
        ws.send(WsFrame::text(rest[0].clone())).await?;
        rest = &rest[1..];
        // the login reply goes to the handler too (it reports auth failures)
        loop {
            let f = timeout(Duration::from_secs(10), ws.next()).await.map_err(|_| anyhow::anyhow!("no login reply"))?.ok_or_else(|| anyhow::anyhow!("closed during login"))?;
            match f.opcode() {
                OpCode::Text => {
                    // with on_login, replies that produce nothing (greetings like Kraken's
                    // {"event":"info"}) are passed on and the wait continues
                    let out = spec.on_login.as_ref().map(|cb| cb(f.as_str()));
                    if !handler(Frame::Text(f.as_str())) { return Ok(()); }
                    match out {
                        Some(msgs) if msgs.is_empty() => continue,
                        Some(msgs) => { for m in msgs { ws.send(WsFrame::text(m)).await?; } break; }
                        None => break,
                    }
                }
                OpCode::Binary => { if !handler(Frame::Binary(f.payload())) { return Ok(()); } break; }
                OpCode::Close => return Ok(()),
                _ => {}
            }
        }
    }
    for s in rest { ws.send(WsFrame::text(s.clone())).await?; }
    let (every, ping) = spec.ping.clone().unwrap_or((Duration::from_secs(3600), String::new()));
    let mut tick = interval(every);
    tick.tick().await;
    loop {
        tokio::select! {
            m = timeout(spec.idle, ws.next()) => {
                let Ok(m) = m else { bail!("no data for {}s", spec.idle.as_secs()) };
                let Some(f) = m else { return Ok(()) };
                let keep = match f.opcode() {
                    OpCode::Text => handler(Frame::Text(f.as_str())),
                    OpCode::Binary => handler(Frame::Binary(f.payload())),
                    OpCode::Close => { eprintln!("[{}] closed by server", spec.name); return Ok(()) }
                    _ => true, // pings are answered by yawc
                };
                if !keep { return Ok(()) }
            }
            _ = tick.tick(), if !ping.is_empty() => ws.send(WsFrame::text(ping.clone())).await?,
        }
    }
}

pub fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(10))
        // proxies drop idle connections silently; reusing one stalls a request until timeout
        .pool_idle_timeout(Duration::from_secs(30)).tcp_keepalive(Duration::from_secs(15)).user_agent("terminal-one/0.1").build().unwrap())
}

/// Rate-limit pauses per host (epoch ms). While a host is paused no request leaves for it:
/// requests during a Binance IP ban extend the ban (2 minutes, doubling, up to 3 days).
static PAUSED: std::sync::Mutex<Option<std::collections::HashMap<String, i64>>> = std::sync::Mutex::new(None);

/// Pauses are persisted so a restarted process (or the CLI tools) respects a ban too.
fn pauses_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/TerminalOne/rate-pauses.json"))
}

fn with_pauses<T>(f: impl FnOnce(&mut std::collections::HashMap<String, i64>) -> T) -> T {
    let mut g = PAUSED.lock().unwrap();
    let m = g.get_or_insert_with(|| pauses_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    f(m)
}

fn host_of(url: &str) -> String { reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default() }

/// Fails fast while `url`'s host is paused after a rate-limit answer.
pub fn check_paused(url: &str) -> Result<()> {
    let h = host_of(url);
    let until = with_pauses(|m| m.get(&h).copied()).unwrap_or(0);
    let left = until - crate::now_ms();
    if left > 0 { bail!("{h}: rate limited, paused for {}s more", left / 1000 + 1) }
    Ok(())
}

/// Record a rate-limit answer (429, 418, Binance -1003) and pause the host: until the
/// "banned until <ms>" in the body, else Retry-After, else 60s (429) / 10 min (418).
pub fn note_limit(url: &str, status: u16, retry_after: Option<u64>, body: &str) -> bool {
    if !(status == 429 || status == 418 || body.contains("-1003")) { return false; }
    let now = crate::now_ms();
    let until = body.split("banned until ").nth(1).and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<i64>().ok())
        .or(retry_after.map(|s| now + s as i64 * 1000))
        .unwrap_or(now + if status == 418 { 600_000 } else { 60_000 });
    let h = host_of(url);
    eprintln!("[rate limit] {h}: HTTP {status}, pausing {}s", (until - now) / 1000);
    with_pauses(|m| {
        let e = m.entry(h).or_insert(0);
        *e = (*e).max(until);
        m.retain(|_, t| *t > now);
        if let Some(p) = pauses_path() {
            if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
            let _ = std::fs::write(p, serde_json::to_vec(m).unwrap_or_default());
        }
    });
    true
}

/// Sends `req` and parses JSON. A 429 with a short Retry-After (startup history bursts) is
/// retried up to 3 times; the host stays paused meanwhile so other callers back off too.
/// A 418 (ban) is never retried.
async fn send_json(url: &str, what: &str, req: impl Fn() -> reqwest::RequestBuilder) -> Result<serde_json::Value> {
    for attempt in 0..4u32 {
        check_paused(url)?;
        let r = req().send().await?;
        let st = r.status();
        let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let body = r.text().await?;
        if note_limit(url, st.as_u16(), retry_after.or(Some(1 << attempt)).filter(|_| st.as_u16() == 429), &body) {
            let wait = retry_after.unwrap_or(1 << attempt);
            if st.as_u16() == 429 && attempt < 3 && wait <= 10 { sleep(Duration::from_secs(wait)).await; continue; }
        }
        if !st.is_success() { bail!("{what} -> {st}: {}", &body[..body.len().min(200)]) }
        return Ok(serde_json::from_str(&body)?);
    }
    bail!("{what}: still rate limited after retries")
}

pub async fn get_json(url: &str) -> Result<serde_json::Value> {
    send_json(url, &format!("GET {url}"), || http().get(url)).await
}

pub async fn post_json(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    send_json(url, &format!("POST {url}"), || http().post(url).json(body)).await
}

/// calls f every `every`; errors are logged, polling continues
pub async fn poll<F, Fut>(name: String, every: Duration, mut f: F)
where F: FnMut() -> Fut, Fut: std::future::Future<Output = Result<()>> {
    let mut tick = interval(every);
    loop {
        tick.tick().await;
        if let Err(e) = f().await { eprintln!("[{name}] {e:#}"); }
    }
}

/// ws::run that drops the session and reconnects when the handler returns false (sequence gap).
pub async fn run_resync<H: FnMut(Frame) -> bool + Send>(spec: impl Fn() -> Spec, mk: impl Fn() -> H) {
    loop {
        let gap = Arc::new(tokio::sync::Notify::new());
        let (g, mut h) = (gap.clone(), mk());
        let s = spec();
        let name = s.name.clone();
        tokio::select! {
            _ = run(s, move |f| if !h(f) { g.notify_one() }) => {}
            _ = gap.notified() => eprintln!("[{name}] sequence gap, resubscribing"),
        }
    }
}
