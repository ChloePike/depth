//! Polymarket crypto price markets as a read-only reference: "above K on <date>" (where the price
//! settles) and "reach / dip to K by <date>" (touch) odds for one coin, from the public Gamma API.
//! The Yes share price (0..1) is the market's probability. No trading: Polymarket orders need a
//! Polygon wallet and are geo-restricted.
use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind { Above, Below, Reach, Dip }

#[derive(Clone, Debug)]
pub struct Odds { pub kind: Kind, pub strike: f64, pub p: f64, pub end_ms: i64 }

impl Kind {
    pub fn touch(self) -> bool { matches!(self, Kind::Reach | Kind::Dip) }
}

/// Polymarket's name for a coin: its questions say "Bitcoin", not "BTC".
/// ponytail: hand-kept list of the coins with price markets (2026-10); an unlisted coin is searched
/// by its ticker (works for XRP, BNB, SUI, ASTER), a new one named differently needs a row here
const NAMES: &[(&str, &str)] = &[
    ("BTC", "Bitcoin"), ("ETH", "Ethereum"), ("SOL", "Solana"), ("DOGE", "Dogecoin"), ("HYPE", "Hyperliquid"),
    ("ENA", "Ethena"), ("LINK", "Chainlink"), ("ZEC", "Zcash"), ("XPL", "Plasma"), ("UNI", "Uniswap"),
    ("LIT", "Lighter"), ("XMR", "Monero"), ("PUMP", "Pump.fun"), ("AVAX", "Avalanche"), ("ADA", "Cardano"),
];

pub fn name(base: &str) -> &str { NAMES.iter().find(|(b, _)| *b == base).map_or(base, |(_, n)| n) }

/// Open strike markets of `base`: two searches ("<name> above", "<name> hit"), about 1 MB raw each.
pub async fn fetch(base: &str) -> Result<Vec<Odds>> {
    let n = name(base);
    let mut out: Vec<Odds> = vec![];
    for q in ["above", "hit"] {
        let url = format!("https://gamma-api.polymarket.com/public-search?q={n}%20{q}&limit_per_type=50&events_status=active&search_profiles=false&search_tags=false");
        let v = crate::ws::get_json_slow(&url, 30).await?;
        for o in parse(&v, n, crate::now_ms()) {
            if !out.iter().any(|x| x.kind == o.kind && x.strike == o.strike && x.end_ms == o.end_ms) { out.push(o); }
        }
    }
    Ok(out)
}

/// Strike markets on `name` from a public-search reply: open, not yet ended, Yes/No.
pub fn parse(v: &Value, name: &str, now: i64) -> Vec<Odds> {
    let name = name.to_lowercase();
    let mut out = vec![];
    for e in v["events"].as_array().into_iter().flatten() {
        for m in e["markets"].as_array().into_iter().flatten() {
            if m["closed"].as_bool() != Some(false) { continue; }
            let q = m["question"].as_str().unwrap_or_default().to_lowercase();
            let Some((kind, strike)) = question(&q, &name) else { continue };
            // outcomes are a JSON array encoded as a string
            let yes = m["outcomes"].as_str().and_then(|s| serde_json::from_str::<Vec<String>>(s).ok()).and_then(|v| v.into_iter().next());
            if yes.as_deref() != Some("Yes") { continue; }
            // the Yes book's mid; an empty or wide book (no bid, 0.01 / 0.88) says nothing: skipped
            let (Some(bid), Some(ask)) = (m["bestBid"].as_f64(), m["bestAsk"].as_f64()) else { continue };
            if !(ask >= bid && ask - bid <= 0.1) { continue; }
            let p = (bid + ask) / 2.0;
            let end_ms = crate::iso_ms(m["endDate"].as_str().unwrap_or_default());
            if end_ms > now && (0.0..=1.0).contains(&p) { out.push(Odds { kind, strike, p, end_ms }); }
        }
    }
    out
}

/// "will the price of bitcoin be above $78,000 on october 31?" -> (Above, 78000);
/// "will hyperliquid dip to $50 by ...", "... reach / hit $150k ..." -> touch. The keyword must follow the
/// coin's name (or "<name> be"), so "hyperliquid hip-3 open interest hit $5b" is not a price market.
fn question(q: &str, name: &str) -> Option<(Kind, f64)> {
    let after = q.split_once(name)?.1;
    let after = after.strip_prefix(" be").unwrap_or(after);
    let (kind, rest) = [(" above $", Kind::Above), (" below $", Kind::Below), (" reach $", Kind::Reach), (" hit $", Kind::Reach), (" dip to $", Kind::Dip), (" fall to $", Kind::Dip)]
        .iter().find_map(|(k, kind)| after.strip_prefix(k).map(|r| (*kind, r)))?;
    let lead = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.').count();
    let num: String = rest[..lead].chars().filter(|c| *c != ',').collect();
    let mult = match rest[lead..].chars().next() { Some('k') => 1e3, Some('m') => 1e6, Some('b') => 1e9, _ => 1.0 };
    let v = num.trim_end_matches('.').parse::<f64>().ok()? * mult;
    (v > 0.0).then_some((kind, v))
}

/// What the chart shows: the nearest settle ladder ending at least an hour out, and the nearest touch
/// window ending at least a day out, each with 3+ strikes priced 2..98% (the rest is noise).
pub fn pick(odds: &[Odds], now: i64) -> (Vec<&Odds>, Vec<&Odds>) {
    let side = |touch: bool, min_ms: i64| -> Vec<&Odds> {
        let live: Vec<&Odds> = odds.iter().filter(|o| o.kind.touch() == touch && o.end_ms >= now + min_ms && o.p > 0.02 && o.p < 0.98).collect();
        let mut ends: Vec<i64> = live.iter().map(|o| o.end_ms).collect();
        ends.sort();
        ends.dedup();
        let end = ends.into_iter().find(|e| live.iter().filter(|o| o.end_ms == *e).count() >= 3);
        let mut v: Vec<&Odds> = live.into_iter().filter(|o| Some(o.end_ms) == end).collect();
        v.sort_by(|a, b| a.strike.total_cmp(&b.strike));
        v
    };
    (side(false, 3_600_000), side(true, 86_400_000))
}

/// Settle price the "above" ladder puts at 50%: linear between the two strikes around it.
pub fn median(settle: &[&Odds]) -> Option<f64> {
    let above: Vec<&&Odds> = settle.iter().filter(|o| o.kind == Kind::Above).collect();
    above.windows(2).find(|w| w[0].p >= 0.5 && w[1].p < 0.5).map(|w| {
        let (a, b) = (w[0], w[1]);
        a.strike + (a.p - 0.5) / (a.p - b.p) * (b.strike - a.strike)
    })
}

/// The same event priced by options (risk-neutral, lognormal at the strike's IV): forward `f`, strike `k`,
/// `iv` annualised, `t` years. Settle = N(d2) (N(-d2) below). Touch uses the driftless reflection rule:
/// P(touch) = 2 P(end beyond K), capped at 1.
pub fn implied(kind: Kind, f: f64, k: f64, iv: f64, t: f64) -> Option<f64> {
    if !(f > 0.0 && k > 0.0 && iv > 0.0 && t > 0.0) { return None; }
    let sd = iv * t.sqrt();
    let up = ncdf(((f / k).ln() - sd * sd / 2.0) / sd);
    Some(match kind {
        Kind::Above => up,
        Kind::Below => 1.0 - up,
        Kind::Reach => if k <= f { 1.0 } else { (2.0 * up).min(1.0) },
        Kind::Dip => if k >= f { 1.0 } else { (2.0 * (1.0 - up)).min(1.0) },
    })
}

/// Standard normal CDF (Abramowitz-Stegun 26.2.17, error < 7.5e-8).
pub fn ncdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let poly = t * (0.319381530 + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    let tail = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt() * poly;
    if x >= 0.0 { 1.0 - tail } else { tail }
}

#[cfg(test)]
mod tests {
    #[test]
    fn option_implied() {
        assert!((ncdf(0.0) - 0.5).abs() < 1e-7);
        assert!((ncdf(1.96) - 0.975).abs() < 1e-4);
        assert!((ncdf(-1.0) + ncdf(1.0) - 1.0).abs() < 1e-7);
        // at the money: a touch is about twice as likely as finishing beyond
        let a = implied(Kind::Above, 100.0, 100.0, 0.5, 0.1).unwrap();
        assert!(a < 0.5 && a > 0.45);
        let r = implied(Kind::Reach, 100.0, 110.0, 0.5, 0.1).unwrap();
        assert!((r - 2.0 * implied(Kind::Above, 100.0, 110.0, 0.5, 0.1).unwrap()).abs() < 1e-12);
        assert_eq!(implied(Kind::Dip, 100.0, 120.0, 0.5, 0.1), Some(1.0));
        assert_eq!(implied(Kind::Above, 100.0, 100.0, 0.0, 0.1), None);
    }

    use super::*;

    #[test]
    fn questions_parse() {
        assert_eq!(question("will the price of bitcoin be above $78,000 on october 31?", "bitcoin"), Some((Kind::Above, 78000.0)));
        assert_eq!(question("will hyperliquid dip to $50 by december 31, 2026?", "hyperliquid"), Some((Kind::Dip, 50.0)));
        assert_eq!(question("will bitcoin hit $150k by june 30, 2026?", "bitcoin"), Some((Kind::Reach, 150000.0)));
        assert_eq!(question("will xrp reach $3.5?", "xrp"), Some((Kind::Reach, 3.5)));
        assert_eq!(question("will the price of bitcoin be between $76,000 and $78,000 on october 30?", "bitcoin"), None);
        assert_eq!(question("will hyperliquid hip-3 open interest hit $5b in 2026?", "hyperliquid"), None);
        assert_eq!(question("bitcoin up or down - october 30", "bitcoin"), None);
    }

    #[test]
    fn parse_pick_median() {
        let mk = |q: &str, p: f64, closed: bool| serde_json::json!({ "question": q, "closed": closed, "endDate": "2026-10-31T16:00:00Z",
            "outcomes": "[\"Yes\", \"No\"]", "bestBid": p - 0.005, "bestAsk": p + 0.005 });
        let v = serde_json::json!({ "events": [{ "markets": [
            mk("Will the price of Bitcoin be above $76,000 on October 31?", 0.89, false),
            mk("Will the price of Bitcoin be above $78,000 on October 31?", 0.58, false),
            mk("Will the price of Bitcoin be above $80,000 on October 31?", 0.16, false),
            mk("Will the price of Bitcoin be above $90,000 on October 31?", 0.006, false),
            mk("Will the price of Bitcoin be above $70,000 on October 31?", 0.9, true),
            mk("Will ETH/BTC reach $0.04?", 0.5, false),
            serde_json::json!({ "question": "Will Bitcoin dip to $60,000 by October 31?", "closed": false, "endDate": "2026-10-31T16:00:00Z",
                "outcomes": "[\"Yes\", \"No\"]", "bestBid": 0.01, "bestAsk": 0.88 }),
        ] }] });
        let now = crate::iso_ms("2026-10-30T00:00:00Z");
        let o = parse(&v, "Bitcoin", now);
        assert_eq!(o.len(), 4);
        let (settle, touch) = pick(&o, now);
        assert_eq!(settle.len(), 3);
        assert!(touch.is_empty());
        let m = median(&settle).unwrap();
        assert!((m - (78000.0 + 0.08 / 0.42 * 2000.0)).abs() < 1e-6);
        assert!(parse(&v, "Bitcoin", crate::iso_ms("2026-11-01T00:00:00Z")).is_empty());
    }
}
