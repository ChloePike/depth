//! Custom indicators: TypeScript (or JavaScript) files in `<data dir>/indicators`, edited in any editor (`depth.d.ts`
//! beside them gives the types) and reloaded on save. oxc strips the types, QuickJS runs the module: its default export
//! gets the chart's bars as whole columns (Float64Array, NaN = missing) and returns plots. One call per chart update,
//! never one per bar, so the interpreter is not the bottleneck.
use std::{cell::RefCell, collections::HashMap, path::{Path, PathBuf}, sync::{Arc, Mutex}, time::{Duration, Instant, SystemTime}};
use depth::agg::Bar;
use rquickjs::{Array, Context, Ctx, Function, Module, Object, Runtime, TypedArray, Value};

/// One run (and one load) may take this long before QuickJS interrupts it: a script never freezes the chart.
const BUDGET: Duration = Duration::from_millis(50);
const MEM: usize = 64 << 20;
/// Deep recursion throws in JS instead of overflowing the app's native stack.
const STACK: usize = 512 << 10;
/// Plots a script may return; more are an error, not a frozen chart.
const MAX_PLOTS: usize = 16;
/// The forming bar re-runs the scripts at most this often; a new bar runs them at once.
const LIVE_EVERY: Duration = Duration::from_secs(1);
const SCAN_EVERY: Duration = Duration::from_secs(1);

pub struct Plot { pub title: String, pub color: Option<String>, pub hist: bool, pub vals: Vec<f64> }

/// A script's output for one chart: one value per chart bar in every plot.
pub struct Out { pub name: String, pub plots: Vec<Plot>, pub err: Option<String> }

struct Script { mtime: SystemTime, ver: u64, name: String, js: Result<(Runtime, Context), String>, deadline: Arc<Mutex<Instant>> }

#[derive(Default)]
struct Engine {
    scripts: HashMap<String, Script>,
    scanned: Option<Instant>,
    ver: u64,
    /// (script, chart key) -> (script ver, bars signature, run at, output)
    cache: HashMap<(String, String), (u64, (usize, i64, i64), u64, Instant, Arc<Out>)>,
}

thread_local! { static ENGINE: RefCell<Engine> = RefCell::default(); }

/// The indicators folder; created on first use with the type declarations and one example.
pub fn dir() -> Option<PathBuf> {
    let d = depth::sys::data_dir()?.join("indicators");
    if !d.exists() {
        std::fs::create_dir_all(&d).ok()?;
        for (f, src) in EXAMPLES { let _ = std::fs::write(d.join(f), src); }
    }
    // always ours: a newer build's types replace the old ones
    if std::fs::read_to_string(d.join("depth.d.ts")).ok().as_deref() != Some(DTS) { let _ = std::fs::write(d.join("depth.d.ts"), DTS); }
    Some(d)
}

/// The scripts found (file stem, display name, load error), by name.
pub fn list() -> Vec<(String, String, Option<String>)> {
    ENGINE.with_borrow_mut(|e| {
        e.scan();
        let mut v: Vec<_> = e.scripts.iter().map(|(k, s)| (k.clone(), s.name.clone(), s.js.as_ref().err().cloned())).collect();
        v.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        v
    })
}

/// Script `stem` over `bars` (the chart's, oldest first); `key` names the chart's data (base, interval, source, market)
/// so the same bars are not recomputed every frame. None when the file is gone.
pub fn run(stem: &str, key: &str, bars: &[Bar]) -> Option<Arc<Out>> {
    ENGINE.with_borrow_mut(|e| {
        e.scan();
        let s = e.scripts.get(stem)?;
        let (Some(first), Some(last)) = (bars.first(), bars.last()) else { return None };
        let sig = (bars.len(), first.t, last.t);
        let live = last.c.to_bits() ^ last.vol.to_bits() ^ last.flow.ofi.to_bits();
        let ck = (stem.to_string(), key.to_string());
        if let Some((ver, sg, lv, at, out)) = e.cache.get(&ck) {
            if *ver == s.ver && *sg == sig && (*lv == live || at.elapsed() < LIVE_EVERY) { return Some(out.clone()); }
        }
        let out = Arc::new(match &s.js {
            Err(err) => Out { name: s.name.clone(), plots: vec![], err: Some(err.clone()) },
            Ok((_, ctx)) => {
                *s.deadline.lock().unwrap() = Instant::now() + BUDGET;
                match ctx.with(|ctx| call(&ctx, bars).map_err(|x| describe(&ctx, x))) {
                    Ok(plots) => Out { name: s.name.clone(), plots, err: None },
                    Err(err) => Out { name: s.name.clone(), plots: vec![], err: Some(err) },
                }
            }
        });
        e.cache.insert(ck, (s.ver, sig, live, Instant::now(), out.clone()));
        Some(out)
    })
}

impl Engine {
    /// Picks up new, changed and deleted files, at most once a second.
    fn scan(&mut self) {
        if self.scanned.is_some_and(|t| t.elapsed() < SCAN_EVERY) { return; }
        self.scanned = Some(Instant::now());
        let Some(d) = dir() else { return };
        let mut seen = vec![];
        for p in std::fs::read_dir(&d).into_iter().flatten().flatten().map(|f| f.path()) {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if n.ends_with(".d.ts") || !(n.ends_with(".ts") || n.ends_with(".js")) { continue; }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            seen.push(stem.clone());
            if self.scripts.get(&stem).is_some_and(|s| s.mtime == mtime) { continue; }
            self.ver += 1;
            self.scripts.insert(stem.clone(), load(&p, &stem, mtime, self.ver));
        }
        self.scripts.retain(|k, _| seen.contains(k));
        self.cache.retain(|(k, _), _| seen.contains(k));
    }
}

fn load(path: &Path, stem: &str, mtime: SystemTime, ver: u64) -> Script {
    let deadline = Arc::new(Mutex::new(Instant::now() + BUDGET));
    let mut name = stem.to_string();
    let js = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|src| strip(&src, path)).and_then(|js| {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(MEM);
        rt.set_max_stack_size(STACK);
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > *d.lock().unwrap())));
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        ctx.with(|ctx| {
            let r = (|| -> rquickjs::Result<Option<String>> {
                ctx.eval::<(), _>(PRELUDE)?;
                let (m, p) = Module::declare(ctx.clone(), stem, js)?.eval()?;
                p.finish::<()>()?;
                let ns = m.namespace()?;
                let f: Function = ns.get("default")?;
                ctx.globals().set("__main", f)?;
                Ok(ns.get::<_, Option<Object>>("meta")?.and_then(|m| m.get::<_, Option<String>>("name").ok().flatten()))
            })();
            r.map_err(|x| describe(&ctx, x))
        }).map(|n| { if let Some(n) = n { name = n; } (rt, ctx) })
    });
    Script { mtime, ver, name, js, deadline }
}

/// TypeScript -> JavaScript: types stripped, nothing else lowered (QuickJS runs ES2023).
fn strip(src: &str, path: &Path) -> Result<String, String> {
    use oxc::{allocator::Allocator, codegen::Codegen, parser::Parser, semantic::SemanticBuilder, span::SourceType, transformer::{TransformOptions, Transformer}};
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, src, SourceType::ts()).parse();
    if let Some(e) = ret.diagnostics.into_iter().next() { return Err(diag(src, &e)); }
    let mut program = ret.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let t = Transformer::new(&alloc, path, &TransformOptions::default()).build_with_scoping(scoping, &mut program);
    if let Some(e) = t.diagnostics.into_iter().next() { return Err(e.to_string()); }
    Ok(Codegen::new().build(&program).code)
}

/// A diagnostic as "message (line n)".
fn diag(src: &str, e: &oxc::diagnostics::OxcDiagnostic) -> String {
    let at = e.labels.as_ref().first().map(|l| src[..(l.offset() as usize).min(src.len())].lines().count().max(1));
    match at { Some(n) => format!("{} (line {n})", e.message), None => e.message.to_string() }
}

/// The columns the script sees, in `depth.d.ts` order.
const COLS: [(&str, fn(&Bar) -> f64); 21] = [
    ("t", |b| b.t as f64), ("open", |b| b.o), ("high", |b| b.h), ("low", |b| b.l), ("close", |b| b.c),
    ("vol", |b| b.vol), ("buy", |b| b.buy), ("sell", |b| b.sell), ("cvd", |b| b.cvd),
    ("oi", |b| b.oi.unwrap_or(f64::NAN)), ("funding", |b| b.funding_h.unwrap_or(f64::NAN)),
    ("funding_settled", |b| b.funding_settled.unwrap_or(f64::NAN)), ("basis", |b| b.basis_bps.unwrap_or(f64::NAN)),
    ("ls", |b| b.ls.unwrap_or(f64::NAN)), ("ofi", |b| b.flow.ofi), ("add_bid", |b| b.flow.add_bid),
    ("add_ask", |b| b.flow.add_ask), ("rm_bid", |b| b.flow.rm_bid), ("rm_ask", |b| b.flow.rm_ask),
    ("liq_long", |b| b.liq_long), ("liq_short", |b| b.liq_short),
];

fn call<'js>(ctx: &Ctx<'js>, bars: &[Bar]) -> rquickjs::Result<Vec<Plot>> {
    let d = Object::new(ctx.clone())?;
    for (k, f) in COLS { d.set(k, TypedArray::<f64>::new(ctx.clone(), bars.iter().map(f).collect::<Vec<_>>())?)?; }
    let main: Function = ctx.globals().get("__main")?;
    let r: Value = main.call((d,))?;
    // one plot or a list of them
    let list: Vec<Object> = match r.as_array() { Some(a) => a.iter::<Object>().collect::<rquickjs::Result<_>>()?, None => vec![Object::from_value(r)?] };
    if list.len() > MAX_PLOTS { return Err(rquickjs::Exception::throw_range(ctx, &format!("{} plots, at most {MAX_PLOTS}", list.len()))); }
    list.into_iter().enumerate().map(|(i, p)| {
        let mut vals = numbers(p.get("values")?)?;
        // a shorter series ends at the last bar
        if vals.len() < bars.len() { let mut v = vec![f64::NAN; bars.len() - vals.len()]; v.append(&mut vals); vals = v; }
        vals.drain(..vals.len() - bars.len());
        Ok(Plot { title: p.get::<_, Option<String>>("title")?.unwrap_or_else(|| if i == 0 { String::new() } else { format!("#{}", i + 1) }),
            color: p.get("color")?, hist: p.get::<_, Option<String>>("style")?.as_deref() == Some("hist"), vals })
    }).collect()
}

/// A plain array, or anything `Array.from` takes (typed arrays); non-numbers are NaN.
fn numbers(v: Value) -> rquickjs::Result<Vec<f64>> {
    let a: Array = Array::from_value(v.clone()).or_else(|_| v.ctx().globals().get::<_, Object>("Array")?.get::<_, Function>("from")?.call((v,)))?;
    Ok(a.iter::<Value>().map(|x| x.ok().and_then(|x| x.as_number()).unwrap_or(f64::NAN)).collect())
}

/// An error as one readable line: the JS exception's message and where it was thrown.
fn describe(ctx: &Ctx, e: rquickjs::Error) -> String {
    if !e.is_exception() { return e.to_string(); }
    let v = ctx.catch();
    match v.as_exception() {
        Some(x) => {
            let at = x.stack().and_then(|s| s.lines().find(|l| l.contains(':')).map(|l| l.trim().to_string()));
            let msg = x.message().unwrap_or_else(|| "error".into());
            if msg == "interrupted" { return format!("stopped: over {} ms", BUDGET.as_millis()); }
            match at { Some(a) => format!("{msg} {a}"), None => msg }
        }
        None => format!("{:?}", v),
    }
}

// The helpers every script sees. Plain JS over whole columns: an EMA of 10,000 bars is well under a millisecond here.
// ponytail: ta.* in JS; move the hot ones to Rust functions if a profile ever shows them
const PRELUDE: &str = r#"
const out = n => new Float64Array(n).fill(NaN);
const fin = Number.isFinite;
function ewm(a, k) { const o = out(a.length); let e = NaN; for (let i = 0; i < a.length; i++) { const x = a[i]; if (!fin(x)) continue; e = Number.isNaN(e) ? x : e + k * (x - e); o[i] = e; } return o; }
function roll(a, n, f) { const o = out(a.length); for (let i = n - 1; i < a.length; i++) { let ok = true; for (let j = i - n + 1; j <= i; j++) if (!fin(a[j])) { ok = false; break; } if (ok) o[i] = f(a, i - n + 1, i); } return o; }
globalThis.ta = {
  sma(a, n) { return roll(a, n, (a, s, e) => { let t = 0; for (let j = s; j <= e; j++) t += a[j]; return t / n; }); },
  ema(a, n) { return ewm(a, 2 / (n + 1)); },
  rma(a, n) { return ewm(a, 1 / n); },
  stdev(a, n) { return roll(a, n, (a, s, e) => { let m = 0; for (let j = s; j <= e; j++) m += a[j]; m /= n; let v = 0; for (let j = s; j <= e; j++) v += (a[j] - m) ** 2; return Math.sqrt(v / n); }); },
  highest(a, n) { return roll(a, n, (a, s, e) => { let m = -Infinity; for (let j = s; j <= e; j++) m = Math.max(m, a[j]); return m; }); },
  lowest(a, n) { return roll(a, n, (a, s, e) => { let m = Infinity; for (let j = s; j <= e; j++) m = Math.min(m, a[j]); return m; }); },
  change(a, n = 1) { const o = out(a.length); for (let i = n; i < a.length; i++) o[i] = a[i] - a[i - n]; return o; },
  cum(a) { const o = out(a.length); let s = 0; for (let i = 0; i < a.length; i++) { if (fin(a[i])) s += a[i]; o[i] = s; } return o; },
  rsi(a, n) {
    const d = this.change(a), u = this.rma(d.map(x => fin(x) ? Math.max(x, 0) : NaN), n), w = this.rma(d.map(x => fin(x) ? Math.max(-x, 0) : NaN), n);
    return u.map((x, i) => w[i] === 0 ? 100 : 100 - 100 / (1 + x / w[i]));
  },
  zscore(a, n) { const m = this.sma(a, n), s = this.stdev(a, n); return a.map((x, i) => s[i] > 0 ? (x - m[i]) / s[i] : NaN); },
};
globalThis.plot = (values, opts = {}) => ({ ...opts, values });
"#;

const DTS: &str = r##"// Types for Depth custom indicators. Written by Depth: edits here are replaced on the next start.
// A script is a module whose default export takes the chart's bars and returns plots:
//
//   /// <reference path="./depth.d.ts" />
//   export const meta = { name: "My indicator" };
//   export default function (d: Data): Plot[] { return [plot(ta.ema(d.close, 20))]; }
//
// Every column has one value per chart bar, oldest first, at the chart's interval and source (one venue or the
// aggregate). NaN = no data for that bar. Missing values in a plot leave a gap.

interface Data {
  /** bar open time, ms since 1970 (UTC) */ t: Float64Array;
  open: Float64Array; high: Float64Array; low: Float64Array; close: Float64Array;
  /** traded volume, base units; buy / sell = taker (aggressor) side */ vol: Float64Array; buy: Float64Array; sell: Float64Array;
  /** cumulative volume delta, base units */ cvd: Float64Array;
  /** open interest, base units, one side (perp) */ oi: Float64Array;
  /** predicted funding rate per HOUR (perp; 0.0001 = 1 bp) */ funding: Float64Array;
  /** last settled funding rate per hour (perp) */ funding_settled: Float64Array;
  /** perp vs spot, bp */ basis: Float64Array;
  /** accounts long / short ratio */ ls: Float64Array;
  /** top-of-book order flow imbalance, base units */ ofi: Float64Array;
  /** liquidity added / removed within 0.25% of mid, base units */ add_bid: Float64Array; add_ask: Float64Array; rm_bid: Float64Array; rm_ask: Float64Array;
  /** liquidated longs / shorts, base units */ liq_long: Float64Array; liq_short: Float64Array;
}

type Series = ArrayLike<number>;

interface PlotOptions {
  title?: string;
  /** "#rrggbb", or a theme color: "up" | "down" | "accent" | "warn" | "purple" | "fg" | "muted" */
  color?: string;
  /** "line" (default) or "hist" (bars from zero, up / down colored unless a color is given) */
  style?: "line" | "hist";
}

interface Plot extends PlotOptions { values: Series; }

declare const ta: {
  sma(a: Series, n: number): Float64Array;
  ema(a: Series, n: number): Float64Array;
  /** Wilder's moving average (RSI, ATR) */ rma(a: Series, n: number): Float64Array;
  stdev(a: Series, n: number): Float64Array;
  highest(a: Series, n: number): Float64Array;
  lowest(a: Series, n: number): Float64Array;
  /** a[i] - a[i - n] */ change(a: Series, n?: number): Float64Array;
  /** running sum, NaN counted as 0 */ cum(a: Series): Float64Array;
  rsi(a: Series, n: number): Float64Array;
  /** (a - sma) / stdev over n */ zscore(a: Series, n: number): Float64Array;
};

declare function plot(values: Series, opts?: PlotOptions): Plot;
"##;

/// Written into a new indicators folder: one per kind of data only Depth has, as starting points.
const EXAMPLES: [(&str, &str); 5] = [("ofi_strength.ts", r#"/// <reference path="./depth.d.ts" />
// Order flow imbalance against traded volume: how hard the book leans, per unit traded.
// Edit and save: Depth reloads it. Turn it on with the chart's Scripts button.
export const meta = { name: "OFI strength" };

export default function (d: Data): Plot[] {
  const ofi = ta.ema(d.ofi, 20);
  const vol = ta.ema(d.vol, 20);
  return [plot(ofi.map((x, i) => (vol[i] > 0 ? x / vol[i] : NaN)), { title: "OFI / vol", style: "hist" })];
}
"#), ("funding_zscore.ts", r#"/// <reference path="./depth.d.ts" />
// Funding against its own recent range: |z| above 2 marks a crowded side (perp). The predicted rate where the
// chart has it (live), else the last settled one (venue history).
export const meta = { name: "Funding z-score" };

export default function (d: Data): Plot[] {
  const n = 200;
  const f = d.funding.map((x, i) => (Number.isFinite(x) ? x : d.funding_settled[i]));
  const z = ta.zscore(f, n);
  const two = z.map(() => 2), minusTwo = z.map(() => -2);
  return [
    plot(z, { title: "z", style: "hist" }),
    plot(two, { title: "+2", color: "muted" }),
    plot(minusTwo, { title: "-2", color: "muted" }),
  ];
}
"#), ("oi_vs_price.ts", r#"/// <reference path="./depth.d.ts" />
// Open interest and price change over the last n bars, in %. Price up with OI down = shorts covering;
// price down with OI up = new shorts (perp).
export const meta = { name: "OI vs price" };

export default function (d: Data): Plot[] {
  const n = 24;
  const pct = (a: Float64Array) => ta.change(a, n).map((x, i) => (100 * x) / a[i - n]);
  return [
    plot(pct(d.close), { title: "price %", color: "fg" }),
    plot(pct(d.oi), { title: "OI %", color: "warn" }),
  ];
}
"#), ("liquidation_skew.ts", r#"/// <reference path="./depth.d.ts" />
// Shorts liquidated minus longs liquidated, summed over n bars (base units): positive = shorts being squeezed.
export const meta = { name: "Liquidation skew" };

export default function (d: Data): Plot[] {
  const n = 10;
  const net = d.liq_short.map((x, i) => x - d.liq_long[i]);
  const sum = ta.sma(net, n).map((x) => x * n);
  return [plot(sum, { title: "shorts - longs", style: "hist" })];
}
"#), ("book_pressure.ts", r#"/// <reference path="./depth.d.ts" />
// Net liquidity each side gained near the price (added minus removed), bids minus asks, smoothed:
// resting buyers building up vs sellers pulling out.
export const meta = { name: "Book pressure" };

export default function (d: Data): Plot[] {
  const net = d.add_bid.map((x, i) => x - d.rm_bid[i] - (d.add_ask[i] - d.rm_ask[i]));
  return [
    plot(net, { title: "bar", style: "hist" }),
    plot(ta.ema(net, 20), { title: "EMA 20", color: "accent" }),
  ];
}
"#)];

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(n: usize) -> Vec<Bar> {
        (0..n).map(|i| {
            let x = i as f64;
            Bar { t: i as i64 * 60_000, c: 100.0 + x, vol: 2.0, oi: Some(1_000.0 + (x * 0.7).sin() * 50.0), funding_h: (i > 250).then(|| 1e-5 * (x * 0.3).cos()), funding_settled: Some(1e-5 * ((i / 8) as f64).sin()),
                  liq_long: (i % 3) as f64, liq_short: (i % 5) as f64,
                  flow: depth::agg::Flow { ofi: if i % 2 == 0 { 1.0 } else { -3.0 }, add_bid: 5.0 + x % 4.0, rm_bid: 3.0, add_ask: 4.0, rm_ask: 2.0 + x % 3.0 },
                  ..Default::default() }
        }).collect()
    }

    fn eval(src: &str, b: &[Bar]) -> Out {
        // one folder per call: tests run in parallel and several load the same example
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("depth-script-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("x.ts");
        std::fs::write(&p, src).unwrap();
        let s = load(&p, "x", SystemTime::UNIX_EPOCH, 1);
        let _ = std::fs::remove_dir_all(&d);
        match &s.js {
            Err(e) => Out { name: s.name.clone(), plots: vec![], err: Some(e.clone()) },
            Ok((_, ctx)) => {
                *s.deadline.lock().unwrap() = Instant::now() + BUDGET;
                match ctx.with(|ctx| call(&ctx, b).map_err(|x| describe(&ctx, x))) {
                    Ok(plots) => Out { name: s.name.clone(), plots, err: None },
                    Err(e) => Out { name: s.name.clone(), plots: vec![], err: Some(e) },
                }
            }
        }
    }

    #[test]
    fn typescript_runs_over_columns() {
        let o = eval(EXAMPLES[0].1, &bars(50));
        assert_eq!((o.name.as_str(), o.err.as_deref(), o.plots.len()), ("OFI strength", None, 1));
        let p = &o.plots[0];
        assert!(p.hist && p.vals.len() == 50 && p.title == "OFI / vol");
        // ofi alternates +1 / -3, vol 2: the EMA's ratio stays between -1.5 and 0.5
        assert!(p.vals.iter().all(|v| v.is_finite() && (-1.5..=0.5).contains(v)));
    }

    /// Every example a new folder gets loads and draws something on 300 bars.
    #[test]
    fn examples_run() {
        for (f, src) in EXAMPLES {
            let o = eval(src, &bars(300));
            assert_eq!(o.err, None, "{f}");
            assert!(!o.plots.is_empty() && o.plots.iter().all(|p| p.vals.len() == 300), "{f}");
            assert!(o.plots.iter().all(|p| p.vals.iter().any(|v| v.is_finite() && *v != 0.0)), "{f}: a plot is empty");
        }
    }

    /// What a hostile script might try: none of it reaches the app, its files, the network or the account, and
    /// none of it freezes or crashes the chart.
    #[test]
    fn scripts_are_sandboxed() {
        let b = bars(5);
        let err = |src: &str| eval(src, &b).err.unwrap_or_else(|| panic!("ran: {src}"));
        // no host modules, no network, no Node / Deno / browser globals
        err(r#"import * as os from "os"; export default () => [plot([os.exec(["ls"])])];"#);
        err(r#"import * as std from "std"; export default () => [plot([std.loadFile("/etc/passwd").length])];"#);
        err(r#"import x from "./other.js"; export default () => [plot([x])];"#);
        for g in ["fetch", "require", "process", "Deno", "XMLHttpRequest", "WebSocket", "os", "std"] {
            let o = eval(&format!("export default () => [plot([typeof {g} === 'undefined' ? 1 : 0])];"), &b);
            assert_eq!(o.plots[0].vals.last(), Some(&1.0), "{g} is reachable");
        }
        // resource bombs end in an error on that script only
        err("export default () => { const f = (n: number): number => f(n + 1) + 1; return [plot([f(0)])]; };");
        err("export default () => { const a = []; for (;;) a.push(new Float64Array(1 << 20)); };");
        err("export default () => { let s = 'x'; for (;;) s += s; };");
        assert!(err("export default () => { while (true) {} };").starts_with("stopped"));
        assert!(err("export default () => Array.from({ length: 1000 }, () => plot([1]));").contains("at most"));
        // a script that throws at load time is reported, not run
        err("throw new Error('boom'); export default () => [];");
    }

    #[test]
    fn ta_matches_by_hand() {
        let o = eval("export default (d: Data) => [plot(ta.sma(d.close, 3)), plot(ta.change(d.close, 2)), plot([1, 2])];", &bars(5));
        let v: Vec<Vec<f64>> = o.plots.iter().map(|p| p.vals.clone()).collect();
        assert!(v[0][..2].iter().all(|x| x.is_nan()));
        assert_eq!(v[0][2..], [101.0, 102.0, 103.0]);
        assert_eq!(v[1][2..], [2.0, 2.0, 2.0]);
        // a short plain array ends at the last bar
        assert!(v[2][..3].iter().all(|x| x.is_nan()) && v[2][3..] == [1.0, 2.0]);
    }

    #[test]
    fn errors_name_the_problem() {
        let e = eval("export default (d: Data) => { while (true) {} };", &bars(5)).err.unwrap();
        assert!(e.starts_with("stopped"), "{e}");
        let e = eval("export default (d: Data) => d.nope.map(x => x);", &bars(5)).err.unwrap();
        assert!(e.contains("nope") || e.contains("undefined"), "{e}");
        let e = eval("export default (d: Data => 1;", &bars(5)).err.unwrap();
        assert!(e.contains("line 1"), "{e}");
    }
}
