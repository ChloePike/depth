//! Pre-trade risk limits (Settings > Trading), checked before a perp order leaves (the engine's
//! `place_logged`) and in the route preview. Pure: the caller supplies the venue's own mark (never the
//! composite index), what is held and the day's PnL. Every limit is off at 0. Closes (reduce-only)
//! are never blocked: reducing risk must always be possible.
use crate::trade::{Kind, OrderReq};

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Limits {
    /// largest opening order, USDT notional
    pub max_order_usdt: f64,
    /// a limit price at most this far from the venue's own mark, either side (bp)
    pub band_bps: f64,
    /// largest position in one symbol after the order, USDT, every venue and both sides
    pub max_pos_usdt: f64,
    /// no opening order once the day's PnL (equity change since local midnight) is this far down, USDT
    pub daily_loss_usdt: f64,
}

/// What a check needs to know about the order's venue and the account.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    /// the venue's own mark, else its touch, in the venue's quote
    pub mark: Option<f64>,
    /// the venue's quote -> USDT
    pub fx: f64,
    /// what the account holds in the symbol now, USDT, every venue and both sides
    pub held_usdt: f64,
    /// equity change since local midnight over the keyed venues, USDT (None until known)
    pub day_pnl: Option<f64>,
}

/// Err with the reason (and the numbers) when `req` breaks a limit. `reduces` is the part of the
/// order that closes an opposite position first (one-way accounts), so only the rest is new exposure.
pub fn check(req: &OrderReq, reduces: f64, c: &Ctx, l: &Limits) -> Result<(), String> {
    if req.close { return Ok(()); }
    if let (true, Some(p)) = (l.daily_loss_usdt > 0.0, c.day_pnl) {
        if p <= -l.daily_loss_usdt { return Err(format!("daily loss stop: {p:.0} USDT today, the limit is -{:.0}; only closes are allowed until local midnight", l.daily_loss_usdt)); }
    }
    let mark = c.mark.filter(|m| *m > 0.0 && m.is_finite());
    if let (true, Kind::Limit { price, .. }) = (l.band_bps > 0.0, req.kind) {
        let Some(m) = mark else { return Err("no venue price to check the price band against".into()) };
        let off = (price / m - 1.0).abs() * 1e4;
        if off > l.band_bps { return Err(format!("price {price} is {off:.0} bp from the venue's mark {m}, outside the {:.0} bp band", l.band_bps)); }
    }
    if l.max_order_usdt <= 0.0 && l.max_pos_usdt <= 0.0 { return Ok(()); }
    let px = match req.kind { Kind::Limit { price, .. } => Some(price), Kind::Stop { trigger, limit, .. } => Some(limit.unwrap_or(trigger)), _ => mark };
    let Some(px) = px.filter(|p| *p > 0.0) else { return Err("no price to value the order against the limits".into()) };
    let reduces = reduces.clamp(0.0, req.qty);
    let usdt = |q: f64| q * px * c.fx;
    let open = usdt(req.qty - reduces);
    if l.max_order_usdt > 0.0 && open > l.max_order_usdt { return Err(format!("order {open:.0} USDT above the {:.0} USDT limit", l.max_order_usdt)); }
    // what it closes first leaves the position, what it opens joins it
    let after = c.held_usdt - usdt(reduces) + open;
    if l.max_pos_usdt > 0.0 && open > 0.0 && after > l.max_pos_usdt { return Err(format!("position would be {after:.0} USDT, above the {:.0} USDT limit", l.max_pos_usdt)); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trade::Tif;
    use crate::Side;

    fn req(kind: Kind, qty: f64, close: bool) -> OrderReq { OrderReq { symbol: "BTCUSDT".into(), pos: Side::Buy, close, kind, qty, client_id: None, attach: None } }
    fn lim(price: f64) -> Kind { Kind::Limit { price, tif: Tif::Gtc } }
    const CTX: Ctx = Ctx { mark: Some(100.0), fx: 1.0, held_usdt: 0.0, day_pnl: Some(0.0) };

    #[test]
    fn notional_cap_blocks_opens_only() {
        let l = Limits { max_order_usdt: 1_000.0, ..Default::default() };
        assert!(check(&req(Kind::Market, 9.0, false), 0.0, &CTX, &l).is_ok());
        let e = check(&req(Kind::Market, 11.0, false), 0.0, &CTX, &l).unwrap_err();
        assert!(e.contains("1100 USDT above the 1000"), "{e}");
        assert!(check(&req(Kind::Market, 50.0, true), 0.0, &CTX, &l).is_ok());
        // a one-way reverse of 2 x 8: 8 close the long, only 8 are new
        assert!(check(&req(Kind::Market, 16.0, false), 8.0, &CTX, &l).is_ok());
        assert!(check(&req(Kind::Market, 16.0, false), 4.0, &CTX, &l).is_err());
        // a limit is valued at its own price
        assert!(check(&req(lim(120.0), 9.0, false), 0.0, &CTX, &l).is_err());
    }

    #[test]
    fn band_both_sides_market_exempt() {
        let l = Limits { band_bps: 500.0, ..Default::default() };
        assert!(check(&req(lim(106.0), 1.0, false), 0.0, &CTX, &l).is_err());
        assert!(check(&req(lim(94.0), 1.0, false), 0.0, &CTX, &l).is_err());
        assert!(check(&req(lim(103.0), 1.0, false), 0.0, &CTX, &l).is_ok());
        assert!(check(&req(lim(97.0), 1.0, false), 0.0, &CTX, &l).is_ok());
        assert!(check(&req(Kind::Market, 1.0, false), 0.0, &CTX, &l).is_ok());
        // no venue price: refused rather than unchecked
        assert!(check(&req(lim(100.0), 1.0, false), 0.0, &Ctx { mark: None, ..CTX }, &l).is_err());
    }

    #[test]
    fn position_cap_counts_what_is_held() {
        let l = Limits { max_pos_usdt: 5_000.0, ..Default::default() };
        let c = Ctx { held_usdt: 4_500.0, ..CTX };
        assert!(check(&req(Kind::Market, 4.0, false), 0.0, &c, &l).is_ok());
        assert!(check(&req(Kind::Market, 6.0, false), 0.0, &c, &l).is_err());
        // reversing 45 long into 45 short keeps the position at 4500
        assert!(check(&req(Kind::Market, 90.0, false), 45.0, &c, &l).is_ok());
        // a reduction is never refused
        let over = Ctx { held_usdt: 9_000.0, ..CTX };
        assert!(check(&req(Kind::Market, 10.0, false), 10.0, &over, &l).is_ok());
    }

    #[test]
    fn daily_loss_blocks_opens_only() {
        let l = Limits { daily_loss_usdt: 200.0, ..Default::default() };
        let down = Ctx { day_pnl: Some(-200.0), ..CTX };
        assert!(check(&req(Kind::Market, 1.0, false), 0.0, &down, &l).unwrap_err().contains("daily loss"));
        assert!(check(&req(Kind::Market, 1.0, true), 0.0, &down, &l).is_ok());
        assert!(check(&req(Kind::Market, 1.0, false), 0.0, &Ctx { day_pnl: Some(-199.0), ..CTX }, &l).is_ok());
        assert!(check(&req(Kind::Market, 1.0, false), 0.0, &Ctx { day_pnl: None, ..CTX }, &l).is_ok());
    }

    #[test]
    fn zero_limits_check_nothing_and_fx_converts() {
        let off = Limits::default();
        assert!(check(&req(lim(1e9), 1e9, false), 0.0, &Ctx { mark: None, day_pnl: Some(-1e9), ..CTX }, &off).is_ok());
        // a USD venue: 10 at 100 USD is 1000.5 USDT
        let l = Limits { max_order_usdt: 1_000.0, ..Default::default() };
        assert!(check(&req(Kind::Market, 10.0, false), 0.0, &CTX, &l).is_ok());
        assert!(check(&req(Kind::Market, 10.0, false), 0.0, &Ctx { fx: 1.0005, ..CTX }, &l).is_err());
    }
}
