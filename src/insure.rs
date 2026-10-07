//! Position insurance: protect a perp position with options bought on any option venue.
//! Long -> buy puts, short -> buy calls. Every quote is priced on the venue's real ask
//! (never mark or index), converted to USD, for the full position size.
use crate::agg::Chain;
use crate::{Exchange, Side};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct Position {
    /// Buy = long, Sell = short
    pub side: Side,
    /// base units
    pub qty: f64,
    pub entry: f64,
    /// current price of the underlying (USD)
    pub mark: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Filter {
    pub min_days: f64,
    pub max_days: f64,
    /// strikes from `mark * (1 - otm_max)` (puts) up to ATM; mirrored for calls
    pub otm_max: f64,
}

impl Default for Filter {
    fn default() -> Self { Filter { min_days: 1.0, max_days: 120.0, otm_max: 0.20 } }
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub ex: Exchange,
    pub symbol: Arc<str>,
    pub expiry_ms: i64,
    pub days: f64,
    pub strike: f64,
    pub call: bool,
    /// USD per 1 base unit of underlying
    pub ask: f64,
    pub ask_qty: f64,
    pub iv: Option<f64>,
    pub delta: f64,
    /// premium for the whole position, USD
    pub cost: f64,
    /// cost / position notional at mark
    pub cost_pct: f64,
    /// cost_pct annualized over the days to expiry
    pub cost_apr: f64,
    /// worst-case loss until expiry measured from `entry`, premium included, USD
    pub max_loss: f64,
    /// distance from mark to the strike (protection floor), fraction
    pub floor_dist: f64,
    /// price at expiry where the insured position breaks even
    pub breakeven: f64,
    /// |delta| x qty: how much of the position the option offsets right now
    pub hedge_ratio: f64,
    /// the best ask alone can fill the whole size
    pub fillable: bool,
}

/// All protection quotes for `pos` across `chains`, cheapest first within each
/// (expiry, strike) and then by expiry and protection level.
/// `usd` converts a venue's option prices to USD (USDT-quoted venues).
pub fn quotes(chains: &HashMap<Exchange, Chain>, usd: impl Fn(Exchange) -> f64, pos: &Position, f: &Filter, now_ms: i64) -> Vec<Quote> {
    let long = pos.side == Side::Buy;
    let notional = pos.qty * pos.mark;
    let mut out = vec![];
    for (ex, chain) in chains {
        let k = usd(*ex);
        for (sym, o) in &chain.opts {
            if !o.has_info || o.call == long { continue; }
            let days = (o.expiry_ms - now_ms) as f64 / 86_400_000.0;
            if days < f.min_days || days > f.max_days { continue; }
            let m = o.strike / pos.mark;
            let in_range = if long { m >= 1.0 - f.otm_max && m <= 1.0 + 0.02 } else { m <= 1.0 + f.otm_max && m >= 1.0 - 0.02 };
            let Some((ask, ask_qty)) = o.ask.filter(|(a, _)| *a > 0.0) else { continue };
            if !in_range { continue; }
            let ask = ask * k;
            let cost = ask * pos.qty;
            let cost_pct = cost / notional;
            let gap = if long { (pos.entry - o.strike).max(0.0) } else { (o.strike - pos.entry).max(0.0) };
            out.push(Quote {
                ex: *ex, symbol: sym.clone(), expiry_ms: o.expiry_ms, days, strike: o.strike, call: o.call,
                ask, ask_qty, iv: o.ask_iv.or(o.iv), delta: o.delta,
                cost, cost_pct, cost_apr: cost_pct * 365.0 / days.max(1e-9),
                max_loss: gap * pos.qty + cost,
                floor_dist: (o.strike / pos.mark - 1.0).abs(),
                breakeven: if long { pos.entry + ask } else { pos.entry - ask },
                hedge_ratio: o.delta.abs(),
                fillable: ask_qty >= pos.qty,
            });
        }
    }
    out.sort_by(|a, b| a.expiry_ms.cmp(&b.expiry_ms)
        .then(a.floor_dist.total_cmp(&b.floor_dist))
        .then(a.cost.total_cmp(&b.cost)));
    out
}

/// Cheapest venue for each (expiry day, strike): the comparison the aggregator exists for.
pub fn best_per_contract(q: &[Quote]) -> Vec<Quote> {
    let mut best: HashMap<(i64, i64), Quote> = HashMap::new();
    for x in q {
        let key = (x.expiry_ms / 86_400_000, (x.strike * 100.0) as i64);
        match best.get(&key) { Some(b) if b.cost <= x.cost => {} _ => { best.insert(key, x.clone()); } }
    }
    let mut v: Vec<Quote> = best.into_values().collect();
    v.sort_by(|a, b| a.expiry_ms.cmp(&b.expiry_ms).then(a.floor_dist.total_cmp(&b.floor_dist)));
    v
}

/// P&L at expiry for underlying price `px`: (unprotected, protected with `q`), USD.
pub fn payoff(pos: &Position, q: Option<&Quote>, px: f64) -> (f64, f64) {
    let dir = if pos.side == Side::Buy { 1.0 } else { -1.0 };
    let raw = (px - pos.entry) * dir * pos.qty;
    let Some(q) = q else { return (raw, raw) };
    let intrinsic = if q.call { (px - q.strike).max(0.0) } else { (q.strike - px).max(0.0) };
    (raw, raw + intrinsic * pos.qty - q.cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::Opt;

    fn chain(opts: &[(&str, f64, bool, f64, f64)]) -> Chain {
        let mut c = Chain::default();
        for (s, strike, call, ask, qty) in opts {
            c.opts.insert((*s).into(), Opt {
                has_info: true, expiry_ms: 10 * 86_400_000, strike: *strike, call: *call,
                ask: Some((*ask, *qty)), delta: if *call { 0.4 } else { -0.4 }, ..Default::default()
            });
        }
        c
    }

    #[test]
    fn long_buys_cheapest_put_and_caps_loss() {
        let mut chains = HashMap::new();
        chains.insert(Exchange::Bybit, chain(&[("BY-P95", 95.0, false, 2.0, 5.0), ("BY-C105", 105.0, true, 1.0, 5.0)]));
        chains.insert(Exchange::Okx, chain(&[("OKX-P95", 95.0, false, 1.8, 0.5), ("OKX-P70", 70.0, false, 0.1, 9.0)]));
        let pos = Position { side: Side::Buy, qty: 2.0, entry: 100.0, mark: 100.0 };
        let q = quotes(&chains, |e| if e == Exchange::Okx { 1.0 } else { 0.999 }, &pos, &Filter::default(), 0);
        // calls and the 70 strike (30% OTM) are excluded
        assert_eq!(q.len(), 2);
        let best = best_per_contract(&q);
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].ex, Exchange::Okx);
        assert!((best[0].cost - 3.6).abs() < 1e-9);
        assert!(!best[0].fillable);
        // max loss = (100 - 95) * 2 + 3.6
        assert!((best[0].max_loss - 13.6).abs() < 1e-9);
        assert!((best[0].cost_apr - 0.018 * 36.5).abs() < 1e-9);
        // at expiry the protected loss never exceeds max_loss
        let (raw, hedged) = payoff(&pos, Some(&best[0]), 50.0);
        assert_eq!(raw, -100.0);
        assert!((hedged + 13.6).abs() < 1e-9);
        let (_, up) = payoff(&pos, Some(&best[0]), 120.0);
        assert!((up - (40.0 - 3.6)).abs() < 1e-9);
    }

    #[test]
    fn short_buys_calls() {
        let mut chains = HashMap::new();
        chains.insert(Exchange::Gate, chain(&[("G-C110", 110.0, true, 1.5, 9.0), ("G-P90", 90.0, false, 1.0, 9.0)]));
        let pos = Position { side: Side::Sell, qty: 1.0, entry: 100.0, mark: 100.0 };
        let q = quotes(&chains, |_| 1.0, &pos, &Filter::default(), 0);
        assert_eq!(q.len(), 1);
        assert!(q[0].call);
        assert!((q[0].max_loss - 11.5).abs() < 1e-9);
        assert!((q[0].breakeven - 98.5).abs() < 1e-9);
    }
}
