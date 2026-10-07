//! Take-profit / stop-loss for Bybit v5 linear and Binance USD-M (plain and Portfolio Margin).
//!
//! Two kinds, both closing a position (never opening one):
//! - whole position: one TP and/or one SL that closes the entire position when triggered
//!   (Bybit `tpslMode=Full`, Binance `closePosition=true`);
//! - partial / staged: any number of TP/SL levels with their own size (Bybit `tpslMode=Partial`,
//!   Binance reduce-only conditional orders with a quantity).
//!
//! Venue mapping (verified 2026-10 against bybit-exchange/docs and the official Binance SDK sources):
//! - Bybit: `POST /v5/position/trading-stop` sets / replaces (Full) or adds (Partial) TP/SL, a price
//!   of "0" cancels the Full one. The resulting orders are conditional orders (`stopOrderType`
//!   TakeProfit / StopLoss / PartialTakeProfit / PartialStopLoss, status Untriggered) listed by
//!   `GET /v5/order/realtime` and cancelled by `POST /v5/order/cancel`.
//! - Binance: since the late-2025 migration USD-M conditional orders are "algo orders":
//!   `POST|DELETE /fapi/v1/algoOrder`, `GET /fapi/v1/openAlgoOrders`; Portfolio Margin:
//!   `POST|DELETE /papi/v1/um/algo/order`, `GET /papi/v1/um/algo/openAlgoOrders`. There is no
//!   modify: a whole-position TP/SL is replaced by cancel + place (Binance allows only one
//!   closePosition order per direction and type, -4130), restoring the old one if placing fails.
//!
//! Live updates (wiring belongs in trade.rs streams, parsers live here):
//! - Bybit `order.linear` rows have the REST shape: `bybit_row(o)`; open while `Untriggered`.
//! - Binance user data `{"e":"ALGO_UPDATE"}` (USD-M payload in `o`, PM in `ao` with `fs`):
//!   `binance_algo_update(v)`; open while `X == "NEW"`.
use super::{bybit, binance_at, binance_pm, binance_url, fmt_step, papi_url, rules, Keys, Mode, Rules};
use crate::{Exchange, Side};
use anyhow::{anyhow, bail, Result};
use reqwest::Method;
use serde_json::{json, Value};

/// Price the trigger compares against. Mark is the default (wicks on the last price don't fire it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trigger { #[default] Mark, Last }

/// One active TP/SL. `pos` is the position side it protects (Buy = long); `qty` None = whole position.
#[derive(Clone, Debug, PartialEq)]
pub struct TpSl { pub id: String, pub ex: Exchange, pub symbol: String, pub pos: Side, pub take_profit: bool, pub trigger_px: f64, pub qty: Option<f64>, pub trigger: Trigger }

fn n(v: &Value) -> f64 { crate::num(v) }
fn opposite(s: Side) -> Side { if s == Side::Buy { Side::Sell } else { Side::Buy } }

/// Rejects a TP/SL on the wrong side of the current mark (or last) price: a long's TP must be
/// above it and its SL below; a short's the other way round. Call with the live price before sending.
pub fn validate(pos: Side, mark: f64, take_profit: bool, trigger_px: f64) -> Result<()> {
    if !(trigger_px.is_finite() && trigger_px > 0.0 && mark.is_finite() && mark > 0.0) { bail!("invalid trigger {trigger_px} / mark {mark}"); }
    let above = (pos == Side::Buy) == take_profit;
    if trigger_px == mark || (trigger_px > mark) != above {
        bail!("{} of a {} must be {} the current price {mark}", if take_profit { "take-profit" } else { "stop-loss" },
              if pos == Side::Buy { "long" } else { "short" }, if above { "above" } else { "below" });
    }
    Ok(())
}

fn price(r: &Rules, px: f64) -> Result<String> {
    if !(px.is_finite() && px > 0.0) { bail!("invalid trigger price {px}"); }
    Ok(fmt_step(px, r.tick, false))
}

fn size(r: &Rules, qty: f64) -> Result<String> {
    let s = fmt_step(qty, r.step, true);
    let q: f64 = s.parse()?;
    if q <= 0.0 || q < r.min_qty { bail!("size {q} below minimum {}", r.min_qty); }
    Ok(s)
}

// ---------------------------------------------------------------- Bybit

/// `/v5/position/trading-stop` body. `size` Some = Partial (adds a level of that size to each
/// given side), None = Full (sets / replaces the whole-position TP/SL). A price "0" cancels that side.
pub fn bybit_body(symbol: &str, pos: Side, mode: Mode, tp: Option<&str>, sl: Option<&str>, size: Option<&str>, trigger: Trigger) -> Value {
    let idx = match (mode, pos) { (Mode::OneWay, _) => 0, (Mode::Hedge, Side::Buy) => 1, (Mode::Hedge, Side::Sell) => 2 };
    let by = match trigger { Trigger::Mark => "MarkPrice", Trigger::Last => "LastPrice" };
    let mut b = json!({"category": "linear", "symbol": symbol, "positionIdx": idx, "tpslMode": if size.is_some() { "Partial" } else { "Full" }});
    for (px, p) in [(tp, "tp"), (sl, "sl")] {
        let Some(px) = px else { continue };
        b[if p == "tp" { "takeProfit" } else { "stopLoss" }] = px.into();
        if px == "0" { continue; }
        b[format!("{p}TriggerBy")] = by.into();
        b[format!("{p}OrderType")] = "Market".into();
        if let Some(s) = size { b[format!("{p}Size")] = s.into(); }
    }
    b
}

/// A Bybit order row (REST `/v5/order/realtime` or the `order.linear` push) as a TP/SL, with
/// whether it is still active. None for anything that is not a position TP/SL.
pub fn bybit_row(o: &Value) -> Option<(TpSl, bool)> {
    let take_profit = match o["stopOrderType"].as_str()? {
        "TakeProfit" | "PartialTakeProfit" => true, "StopLoss" | "PartialStopLoss" => false, _ => return None,
    };
    let side = if o["side"] == "Sell" { Side::Sell } else { Side::Buy };
    let pos = match o["positionIdx"].as_i64() { Some(1) => Side::Buy, Some(2) => Side::Sell, _ => opposite(side) };
    Some((TpSl {
        id: o["orderId"].as_str()?.into(), ex: Exchange::Bybit, symbol: o["symbol"].as_str().unwrap_or_default().into(), pos, take_profit,
        trigger_px: n(&o["triggerPrice"]), qty: (o["tpslMode"] != "Full").then(|| n(&o["qty"])),
        // IndexPrice (only settable elsewhere) is reported as Mark, its closest sibling
        trigger: if o["triggerBy"] == "LastPrice" { Trigger::Last } else { Trigger::Mark },
    }, o["orderStatus"] == "Untriggered"))
}

// ---------------------------------------------------------------- Binance

/// Algo-order params for one TP/SL. `qty` None = closePosition (whole position). Hedge mode sends
/// positionSide and never reduceOnly (rejected there); one-way marks a sized order reduceOnly.
pub fn binance_params(symbol: &str, pos: Side, mode: Mode, take_profit: bool, px: &str, qty: Option<&str>, trigger: Trigger) -> Vec<(&'static str, String)> {
    let mut p = vec![("algoType", "CONDITIONAL".into()), ("symbol", symbol.into()),
        ("side", if pos == Side::Buy { "SELL" } else { "BUY" }.into()),
        ("type", if take_profit { "TAKE_PROFIT_MARKET" } else { "STOP_MARKET" }.into()),
        ("triggerPrice", px.into()),
        ("workingType", match trigger { Trigger::Mark => "MARK_PRICE", Trigger::Last => "CONTRACT_PRICE" }.into())];
    if mode == Mode::Hedge { p.push(("positionSide", if pos == Side::Buy { "LONG" } else { "SHORT" }.into())); }
    match qty {
        None => p.push(("closePosition", "true".into())),
        Some(q) => {
            p.push(("quantity", q.into()));
            if mode == Mode::OneWay { p.push(("reduceOnly", "true".into())); }
        }
    }
    p
}

/// A Binance algo order row (REST `openAlgoOrders`) as a TP/SL. Only conditional TP/STOP orders
/// that reduce a position count: closePosition, reduceOnly, or in hedge mode the closing side.
pub fn binance_row(o: &Value) -> Option<TpSl> {
    if o["algoType"] != "CONDITIONAL" { return None; }
    let take_profit = match o["orderType"].as_str()? {
        "TAKE_PROFIT_MARKET" | "TAKE_PROFIT" => true, "STOP_MARKET" | "STOP" => false, _ => return None,
    };
    let side = if o["side"] == "SELL" { Side::Sell } else { Side::Buy };
    let close_all = o["closePosition"].as_bool() == Some(true);
    let (pos, reduces) = match o["positionSide"].as_str() {
        Some("LONG") => (Side::Buy, side == Side::Sell),
        Some("SHORT") => (Side::Sell, side == Side::Buy),
        _ => (opposite(side), close_all || o["reduceOnly"].as_bool() == Some(true)),
    };
    if !reduces { return None; }
    Some(TpSl {
        id: o["algoId"].to_string(), ex: Exchange::Binance, symbol: o["symbol"].as_str().unwrap_or_default().into(), pos, take_profit,
        trigger_px: n(&o["triggerPrice"]), qty: (!close_all).then(|| n(&o["quantity"])),
        trigger: if o["workingType"] == "MARK_PRICE" { Trigger::Mark } else { Trigger::Last },
    })
}

/// A user-data `ALGO_UPDATE` event (USD-M: `o`, Portfolio Margin: `ao`) as a TP/SL plus whether
/// it is still active (status NEW; CANCELED / TRIGGERING / TRIGGERED / FINISHED / REJECTED /
/// EXPIRED end it). The id is the same `algoId` the REST list returns.
pub fn binance_algo_update(v: &Value) -> Option<(TpSl, bool)> {
    if v["e"] != "ALGO_UPDATE" { return None; }
    let o = if v["ao"].is_object() { &v["ao"] } else { &v["o"] };
    let row = json!({"algoId": o["aid"], "algoType": o["at"], "orderType": o["o"], "symbol": o["s"], "side": o["S"], "positionSide": o["ps"],
                     "quantity": o["q"], "triggerPrice": o["tp"], "workingType": o["wt"], "closePosition": o["cp"], "reduceOnly": o["R"]});
    Some((binance_row(&row)?, o["X"] == "NEW"))
}

/// Algo-order endpoints, routed to /papi/v1/um/algo/* on Portfolio Margin accounts.
async fn binance_algo(k: &Keys, method: Method, fapi: &str, params: &[(&str, String)]) -> Result<Value> {
    if binance_pm(k).await? {
        let p = match fapi { "/fapi/v1/algoOrder" => "/papi/v1/um/algo/order", "/fapi/v1/openAlgoOrders" => "/papi/v1/um/algo/openAlgoOrders", p => p };
        binance_at(k, papi_url(), method, p, params).await
    } else {
        binance_at(k, binance_url(), method, fapi, params).await
    }
}

/// Weight 1 with a symbol, 40 without (on demand only: snapshots, never on a timer).
async fn binance_open(k: &Keys, symbol: Option<&str>) -> Result<Vec<TpSl>> {
    let q: Vec<(&str, String)> = symbol.map(|s| ("symbol", s.to_string())).into_iter().collect();
    let v = binance_algo(k, Method::GET, "/fapi/v1/openAlgoOrders", &q).await?;
    Ok(v.as_array().into_iter().flatten().filter(|o| o["algoStatus"].as_str().is_none_or(|s| s == "NEW")).filter_map(binance_row).collect())
}

/// Order-count 1 per 10s / 1m, IP weight 0 (USD-M); IP weight 1 (PM). Returns the algoId.
async fn binance_place(k: &Keys, p: &[(&str, String)]) -> Result<String> {
    Ok(binance_algo(k, Method::POST, "/fapi/v1/algoOrder", p).await?["algoId"].to_string())
}

/// IP weight 1.
async fn binance_cancel(k: &Keys, id: &str) -> Result<()> {
    binance_algo(k, Method::DELETE, "/fapi/v1/algoOrder", &[("algoId", id.into())]).await.map(|_| ())
}

// ---------------------------------------------------------------- venue-neutral API

/// Sets or replaces the whole-position TP and/or SL (None leaves that side as it is).
/// Bybit: one trading-stop call (10/s per UID). Binance: list the symbol (weight 1), then per side
/// cancel the old one (weight 1) and place the new one (order count 1); unchanged sides are skipped.
pub async fn set_position_tpsl(ex: Exchange, k: &Keys, symbol: &str, pos: Side, mode: Mode, tp: Option<f64>, sl: Option<f64>, trigger: Trigger) -> Result<()> {
    if tp.is_none() && sl.is_none() { bail!("nothing to set"); }
    let r = rules(ex, symbol).await?;
    let tp = tp.map(|p| price(&r, p)).transpose()?;
    let sl = sl.map(|p| price(&r, p)).transpose()?;
    match ex {
        Exchange::Bybit => { bybit(k, false, "/v5/position/trading-stop", bybit_body(symbol, pos, mode, tp.as_deref(), sl.as_deref(), None, trigger)).await?; }
        Exchange::Binance => {
            let old = binance_open(k, Some(symbol)).await?;
            for (take_profit, px) in [(true, tp), (false, sl)] {
                let Some(px) = px else { continue };
                let prev = old.iter().find(|t| t.pos == pos && t.take_profit == take_profit && t.qty.is_none());
                if let Some(o) = prev {
                    if o.trigger == trigger && fmt_step(o.trigger_px, r.tick, false) == px { continue; }
                    binance_cancel(k, &o.id).await?;
                }
                if let Err(e) = binance_place(k, &binance_params(symbol, pos, mode, take_profit, &px, None, trigger)).await {
                    let Some(o) = prev else { return Err(e) };
                    // the old one is gone: put it back so the position is not left unprotected
                    let back = binance_place(k, &binance_params(symbol, pos, mode, take_profit, &fmt_step(o.trigger_px, r.tick, false), None, o.trigger)).await;
                    bail!("{e:#}; previous {} at {} {}", if take_profit { "TP" } else { "SL" }, o.trigger_px,
                          match back { Ok(_) => "restored".to_string(), Err(b) => format!("NOT restored: {b:#}") });
                }
            }
        }
        _ => bail!("{ex:?} TP/SL not implemented"),
    }
    Ok(())
}

/// Removes the whole-position TP and/or SL. Partial levels stay (cancel them by id).
pub async fn clear_position_tpsl(ex: Exchange, k: &Keys, symbol: &str, pos: Side, mode: Mode, tp: bool, sl: bool) -> Result<()> {
    match ex {
        Exchange::Bybit => {
            if !tp && !sl { return Ok(()); }
            let body = bybit_body(symbol, pos, mode, tp.then_some("0"), sl.then_some("0"), None, Trigger::Mark);
            bybit(k, false, "/v5/position/trading-stop", body).await?;
        }
        Exchange::Binance => {
            for t in binance_open(k, Some(symbol)).await?.iter().filter(|t| t.pos == pos && t.qty.is_none() && if t.take_profit { tp } else { sl }) {
                binance_cancel(k, &t.id).await?;
            }
        }
        _ => bail!("{ex:?} TP/SL not implemented"),
    }
    Ok(())
}

/// Adds one staged TP or SL level of `qty` base units (reduce-only, market on trigger).
/// Returns its order id. Bybit's trading-stop does not return one, so it is looked up in the open
/// conditional orders of the symbol (one read, 50/s per UID); "" if that lookup fails — the
/// `order.linear` push carries it anyway.
pub async fn add_partial(ex: Exchange, k: &Keys, symbol: &str, pos: Side, mode: Mode, take_profit: bool, trigger_px: f64, qty: f64, trigger: Trigger) -> Result<String> {
    let r = rules(ex, symbol).await?;
    let (px, q) = (price(&r, trigger_px)?, size(&r, qty)?);
    match ex {
        Exchange::Bybit => {
            let (tp, sl) = if take_profit { (Some(px.as_str()), None) } else { (None, Some(px.as_str())) };
            bybit(k, false, "/v5/position/trading-stop", bybit_body(symbol, pos, mode, tp, sl, Some(&q), trigger)).await?;
            let (pxf, qf): (f64, f64) = (px.parse()?, q.parse()?);
            let v = bybit(k, true, "/v5/order/realtime", json!({"category": "linear", "symbol": symbol, "limit": "50"})).await.unwrap_or_default();
            // newest first: the first matching untriggered level is the one just added
            Ok(v["list"].as_array().into_iter().flatten().filter_map(bybit_row)
                .find(|(t, open)| *open && t.pos == pos && t.take_profit == take_profit && (t.trigger_px - pxf).abs() < r.tick / 2.0
                      && t.qty.is_some_and(|x| (x - qf).abs() < r.step / 2.0))
                .map(|(t, _)| t.id).unwrap_or_default())
        }
        Exchange::Binance => binance_place(k, &binance_params(symbol, pos, mode, take_profit, &px, Some(&q), trigger)).await,
        _ => bail!("{ex:?} TP/SL not implemented"),
    }
}

/// Every active TP/SL of the account in one request. Bybit: `/v5/order/realtime` (50/s per UID).
/// Binance: `openAlgoOrders` without symbol = weight 40: snapshot on (re)connect only, never a timer.
pub async fn list(ex: Exchange, k: &Keys) -> Result<Vec<TpSl>> {
    match ex {
        Exchange::Bybit => {
            // ponytail: one page of 50 rows (regular orders included); follow nextPageCursor if accounts hold more
            let v = bybit(k, true, "/v5/order/realtime", json!({"category": "linear", "settleCoin": "USDT", "limit": "50"})).await?;
            Ok(v["list"].as_array().into_iter().flatten().filter_map(bybit_row).filter(|(_, open)| *open).map(|(t, _)| t).collect())
        }
        Exchange::Binance => binance_open(k, None).await,
        _ => bail!("{ex:?} TP/SL not implemented"),
    }
}

/// Cancels one TP/SL by id (Bybit orderId: order cancel, 10/s per UID; Binance algoId: weight 1).
pub async fn cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Bybit => bybit(k, false, "/v5/order/cancel", json!({"category": "linear", "symbol": symbol, "orderId": id})).await.map(|_| ()),
        Exchange::Binance => binance_cancel(k, id).await,
        _ => Err(anyhow!("{ex:?} TP/SL not implemented")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(v: &[(&str, String)], k: &str) -> Option<String> { v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone()) }

    #[test]
    fn trigger_side_sanity() {
        assert!(validate(Side::Buy, 100.0, true, 110.0).is_ok());
        assert!(validate(Side::Buy, 100.0, true, 90.0).is_err());
        assert!(validate(Side::Buy, 100.0, false, 90.0).is_ok());
        assert!(validate(Side::Buy, 100.0, false, 110.0).is_err());
        assert!(validate(Side::Sell, 100.0, true, 90.0).is_ok());
        assert!(validate(Side::Sell, 100.0, true, 110.0).is_err());
        assert!(validate(Side::Sell, 100.0, false, 110.0).is_ok());
        assert!(validate(Side::Sell, 100.0, false, 90.0).is_err());
        assert!(validate(Side::Buy, 100.0, true, 100.0).is_err());
        assert!(validate(Side::Buy, f64::NAN, true, 110.0).is_err());
        assert!(validate(Side::Buy, 100.0, false, 0.0).is_err());
    }

    #[test]
    fn rounding() {
        let r = Rules { tick: 0.1, step: 0.001, min_qty: 0.001, min_notional: 5.0 };
        assert_eq!(price(&r, 86123.46).unwrap(), "86123.5");
        assert_eq!(size(&r, 0.3339).unwrap(), "0.333");
        assert!(size(&r, 0.0004).is_err());
        assert!(price(&r, -1.0).is_err());
    }

    #[test]
    fn bybit_bodies() {
        // whole position, hedge long, TP only on mark
        let b = bybit_body("BTCUSDT", Side::Buy, Mode::Hedge, Some("90000.0"), None, None, Trigger::Mark);
        assert_eq!(b, json!({"category": "linear", "symbol": "BTCUSDT", "positionIdx": 1, "tpslMode": "Full",
                             "takeProfit": "90000.0", "tpTriggerBy": "MarkPrice", "tpOrderType": "Market"}));
        // partial, one-way short SL on last
        let b = bybit_body("BTCUSDT", Side::Sell, Mode::OneWay, None, Some("95000.0"), Some("0.010"), Trigger::Last);
        assert_eq!(b, json!({"category": "linear", "symbol": "BTCUSDT", "positionIdx": 0, "tpslMode": "Partial",
                             "stopLoss": "95000.0", "slTriggerBy": "LastPrice", "slOrderType": "Market", "slSize": "0.010"}));
        assert_eq!(bybit_body("X", Side::Sell, Mode::Hedge, None, None, None, Trigger::Mark)["positionIdx"], 2);
        // clear: "0" only, no trigger / order type
        let b = bybit_body("BTCUSDT", Side::Buy, Mode::OneWay, Some("0"), Some("0"), None, Trigger::Mark);
        assert_eq!(b, json!({"category": "linear", "symbol": "BTCUSDT", "positionIdx": 0, "tpslMode": "Full", "takeProfit": "0", "stopLoss": "0"}));
    }

    #[test]
    fn binance_param_sets() {
        // whole position, hedge short SL: BUY on SHORT, closePosition, no quantity / reduceOnly
        let p = binance_params("BTCUSDT", Side::Sell, Mode::Hedge, false, "95000.0", None, Trigger::Mark);
        assert_eq!(get(&p, "algoType").as_deref(), Some("CONDITIONAL"));
        assert_eq!(get(&p, "side").as_deref(), Some("BUY"));
        assert_eq!(get(&p, "type").as_deref(), Some("STOP_MARKET"));
        assert_eq!(get(&p, "positionSide").as_deref(), Some("SHORT"));
        assert_eq!(get(&p, "closePosition").as_deref(), Some("true"));
        assert_eq!(get(&p, "workingType").as_deref(), Some("MARK_PRICE"));
        assert_eq!((get(&p, "quantity"), get(&p, "reduceOnly")), (None, None));
        // partial, one-way long TP on last: SELL, quantity + reduceOnly, no positionSide
        let p = binance_params("BTCUSDT", Side::Buy, Mode::OneWay, true, "90000.0", Some("0.010"), Trigger::Last);
        assert_eq!(get(&p, "side").as_deref(), Some("SELL"));
        assert_eq!(get(&p, "type").as_deref(), Some("TAKE_PROFIT_MARKET"));
        assert_eq!(get(&p, "quantity").as_deref(), Some("0.010"));
        assert_eq!(get(&p, "reduceOnly").as_deref(), Some("true"));
        assert_eq!(get(&p, "workingType").as_deref(), Some("CONTRACT_PRICE"));
        assert_eq!((get(&p, "positionSide"), get(&p, "closePosition")), (None, None));
        // partial in hedge: positionSide, never reduceOnly (rejected in hedge mode)
        let p = binance_params("BTCUSDT", Side::Buy, Mode::Hedge, true, "90000.0", Some("0.010"), Trigger::Mark);
        assert_eq!((get(&p, "positionSide").as_deref(), get(&p, "reduceOnly")), (Some("LONG"), None));
    }

    #[test]
    fn bybit_rows() {
        // fields as documented for /v5/order/realtime and the order.linear push
        let o = json!({"orderId": "fd43", "symbol": "ETHUSDT", "price": "0", "qty": "0.10", "side": "Sell", "positionIdx": 1,
            "orderStatus": "Untriggered", "orderType": "Market", "stopOrderType": "PartialTakeProfit", "triggerPrice": "2500.00",
            "triggerBy": "MarkPrice", "tpslMode": "Partial", "reduceOnly": true, "closeOnTrigger": true, "createdTime": "1684738540559"});
        let (t, open) = bybit_row(&o).unwrap();
        assert!(open);
        assert_eq!(t, TpSl { id: "fd43".into(), ex: Exchange::Bybit, symbol: "ETHUSDT".into(), pos: Side::Buy, take_profit: true,
                             trigger_px: 2500.0, qty: Some(0.1), trigger: Trigger::Mark });
        // whole-position SL of a one-way short (Buy order), triggered on last, now cancelled
        let o = json!({"orderId": "a1", "symbol": "BTCUSDT", "qty": "0", "side": "Buy", "positionIdx": 0, "orderStatus": "Cancelled",
            "stopOrderType": "StopLoss", "triggerPrice": "95000", "triggerBy": "LastPrice", "tpslMode": "Full"});
        let (t, open) = bybit_row(&o).unwrap();
        assert!(!open);
        assert_eq!((t.pos, t.take_profit, t.qty, t.trigger), (Side::Sell, false, None, Trigger::Last));
        // the documented sample (a plain limit order) is not a TP/SL
        assert!(bybit_row(&json!({"orderId": "x", "stopOrderType": "UNKNOWN", "orderStatus": "New"})).is_none());
        assert!(bybit_row(&json!({"orderId": "x", "stopOrderType": "TrailingStop"})).is_none());
    }

    #[test]
    fn binance_rows() {
        // documented openAlgoOrders sample: one-way SELL TAKE_PROFIT without reduceOnly is an
        // entry order (opens a short), not a TP/SL
        let doc: Value = serde_json::from_str(r#"[{"algoId":2148627,"clientAlgoId":"MRumok0dkhrP4kCm12AHaB","algoType":"CONDITIONAL","orderType":"TAKE_PROFIT","symbol":"BNBUSDT","side":"SELL","positionSide":"BOTH","timeInForce":"GTC","quantity":"0.01","algoStatus":"NEW","actualOrderId":"","actualPrice":"0.00000","triggerPrice":"750.000","price":"750.000","icebergQuantity":"null","tpTriggerPrice":"0.000","tpPrice":"0.000","slTriggerPrice":"0.000","slPrice":"0.000","tpOrderType":"","selfTradePreventionMode":"EXPIRE_MAKER","workingType":"CONTRACT_PRICE","priceMatch":"NONE","closePosition":false,"priceProtect":false,"reduceOnly":false,"createTime":1750514941540,"updateTime":1750514941540,"triggerTime":0,"goodTillDate":0}]"#).unwrap();
        assert!(binance_row(&doc[0]).is_none());
        let mut o = doc[0].clone();
        o["reduceOnly"] = true.into();
        assert_eq!(binance_row(&o).unwrap(), TpSl { id: "2148627".into(), ex: Exchange::Binance, symbol: "BNBUSDT".into(), pos: Side::Buy,
                                                    take_profit: true, trigger_px: 750.0, qty: Some(0.01), trigger: Trigger::Last });
        // hedge whole-position SL of a short
        let o = json!({"algoId": 7, "algoType": "CONDITIONAL", "orderType": "STOP_MARKET", "symbol": "BTCUSDT", "side": "BUY", "positionSide": "SHORT",
                       "quantity": "0", "triggerPrice": "95000", "workingType": "MARK_PRICE", "closePosition": true, "reduceOnly": false});
        let t = binance_row(&o).unwrap();
        assert_eq!((t.pos, t.take_profit, t.qty, t.trigger), (Side::Sell, false, None, Trigger::Mark));
        // hedge BUY on LONG opens: not a TP/SL
        let mut o2 = o.clone();
        o2["positionSide"] = "LONG".into();
        assert!(binance_row(&o2).is_none());
        // ALGO_UPDATE: USD-M payload in `o`, Portfolio Margin in `ao`
        let ev = |key: &str, x: &str| json!({"e": "ALGO_UPDATE", "T": 1, "E": 1, "fs": "UM", key: {"caid": "c", "aid": 7, "at": "CONDITIONAL", "o": "STOP_MARKET",
            "s": "BTCUSDT", "S": "BUY", "ps": "SHORT", "f": "GTC", "q": "0", "X": x, "tp": "95000", "p": "0", "wt": "MARK_PRICE", "cp": true, "R": false}});
        let (u, open) = binance_algo_update(&ev("o", "NEW")).unwrap();
        assert!(open);
        assert_eq!(u, t);
        let (u, open) = binance_algo_update(&ev("ao", "CANCELED")).unwrap();
        assert!(!open);
        assert_eq!(u.id, "7");
        assert!(binance_algo_update(&json!({"e": "ORDER_TRADE_UPDATE"})).is_none());
    }
}
