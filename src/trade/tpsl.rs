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
//! Live updates (wiring belongs in the trade/bybit.rs / trade/binance.rs streams, parsers live here):
//! - Bybit `order.linear` rows have the REST shape: `bybit_row(o)`; open while `Untriggered`.
//! - Binance user data `{"e":"ALGO_UPDATE"}` (USD-M payload in `o`, PM in `ao` with `fs`):
//!   `binance_algo_update(v)`; open while `X == "NEW"`.
use super::{bybit, binance_at, binance_pm, binance_url, fmt_step, papi_url, rules, CancelReport, Keys, Mode, OpenOrder, Rules};
use crate::{Exchange, Side};
use anyhow::{anyhow, bail, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Arc;

/// Price the trigger compares against. Mark is the default (wicks on the last price don't fire it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Trigger { #[default] Mark, Last }

/// Whole-position TP and/or SL sent with an opening order, so the position is protected the moment
/// it fills (Bybit takeProfit / stopLoss on the order). Only where `Caps.attach_tpsl`; `place` refuses it elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attach { pub tp: Option<f64>, pub sl: Option<f64>, pub trigger: Trigger }

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

/// Every other algo row (an opening STOP / TAKE_PROFIT, any TRAILING_STOP_MARKET) as an open order.
/// `trigger` is always Some on these rows (trailing: the activation price, 0 when none): that is
/// what routes their cancel to the algo endpoint. None for TP/SL rows (`binance_row`).
pub fn binance_algo_order(o: &Value) -> Option<OpenOrder> {
    if o["algoType"] != "CONDITIONAL" || binance_row(o).is_some() { return None; }
    let kind = o["orderType"].as_str().filter(|k| !k.is_empty())?.to_lowercase();
    let px = |f: &str| crate::opt_num(&o[f]).filter(|x| *x > 0.0);
    let trigger = if kind == "trailing_stop_market" { px("activatePrice").or(px("triggerPrice")) } else { px("triggerPrice") };
    Some(OpenOrder {
        ex: Exchange::Binance, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["algoId"].to_string(),
        side: if o["side"] == "SELL" { Side::Sell } else { Side::Buy }, price: px("price").unwrap_or(0.0), qty: px("quantity").unwrap_or(0.0), filled: 0.0, kind,
        tif: o["timeInForce"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: o["createTime"].as_i64().unwrap_or(0),
        pos: match o["positionSide"].as_str() { Some("LONG") => Some(Side::Buy), Some("SHORT") => Some(Side::Sell), _ => None },
        client_id: super::opt_id(&o["clientAlgoId"]), trigger: Some(trigger.unwrap_or(0.0)),
    })
}

/// A user-data `ALGO_UPDATE` event (USD-M: `o`, Portfolio Margin: `ao`) in the REST row shape, plus
/// whether it is still active (status NEW; CANCELED / TRIGGERING / TRIGGERED / FINISHED / REJECTED /
/// EXPIRED end it). The id is the same `algoId` the REST list returns.
fn binance_algo_push(v: &Value) -> Option<(Value, bool)> {
    if v["e"] != "ALGO_UPDATE" { return None; }
    let o = if v["ao"].is_object() { &v["ao"] } else { &v["o"] };
    let row = json!({"algoId": o["aid"], "algoType": o["at"], "orderType": o["o"], "symbol": o["s"], "side": o["S"], "positionSide": o["ps"],
                     "quantity": o["q"], "triggerPrice": o["tp"], "workingType": o["wt"], "closePosition": o["cp"], "reduceOnly": o["R"],
                     "price": o["p"], "timeInForce": o["f"], "clientAlgoId": o["caid"], "createTime": v["T"]});
    Some((row, o["X"] == "NEW"))
}

/// `ALGO_UPDATE` of a TP/SL, with whether it is still active.
pub fn binance_algo_update(v: &Value) -> Option<(TpSl, bool)> {
    let (row, active) = binance_algo_push(v)?;
    Some((binance_row(&row)?, active))
}

/// `ALGO_UPDATE` of any other algo order (`binance_algo_order`), with whether it is still open.
pub fn binance_algo_push_order(v: &Value) -> Option<(OpenOrder, bool)> {
    let (row, active) = binance_algo_push(v)?;
    Some((binance_algo_order(&row)?, active))
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

fn open_rows(v: Value) -> Vec<Value> {
    v.as_array().into_iter().flatten().filter(|o| o["algoStatus"].as_str().is_none_or(|s| s == "NEW")).cloned().collect()
}

/// The account's open algo orders (`openAlgoOrders` without symbol, weight 40). A snapshot reads
/// them twice back to back (open orders, then TP/SL), so one fetch is shared for 2 s; the async lock
/// makes a concurrent second caller wait for the first fetch instead of fetching again.
/// ponytail: one process-wide memo keyed by API key; changes inside the 2 s arrive as ALGO_UPDATE pushes
pub(super) async fn binance_algo_rows(k: &Keys) -> Result<Arc<Vec<Value>>> {
    static MEMO: tokio::sync::Mutex<Option<(String, std::time::Instant, Arc<Vec<Value>>)>> = tokio::sync::Mutex::const_new(None);
    let mut m = MEMO.lock().await;
    if let Some((key, at, rows)) = m.as_ref() { if *key == k.key && at.elapsed() < std::time::Duration::from_secs(2) { return Ok(rows.clone()); } }
    let rows = Arc::new(open_rows(binance_algo(k, Method::GET, "/fapi/v1/openAlgoOrders", &[]).await?));
    *m = Some((k.key.clone(), std::time::Instant::now(), rows.clone()));
    Ok(rows)
}

/// Weight 1 with a symbol (always fetched), 40 without (shared memo; on demand only: snapshots, never on a timer).
async fn binance_open(k: &Keys, symbol: Option<&str>) -> Result<Vec<TpSl>> {
    let rows = match symbol {
        Some(s) => Arc::new(open_rows(binance_algo(k, Method::GET, "/fapi/v1/openAlgoOrders", &[("symbol", s.to_string())]).await?)),
        None => binance_algo_rows(k).await?,
    };
    Ok(rows.iter().filter_map(binance_row).collect())
}

/// Order-count 1 per 10s / 1m, IP weight 0 (USD-M); IP weight 1 (PM). Returns the algoId.
pub(crate) async fn binance_place(k: &Keys, p: &[(&str, String)]) -> Result<String> {
    Ok(binance_algo(k, Method::POST, "/fapi/v1/algoOrder", p).await?["algoId"].to_string())
}

/// IP weight 1.
pub(super) async fn binance_cancel(k: &Keys, id: &str) -> Result<()> {
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
            let rows = bybit::realtime(k, "linear", Some(symbol)).await.unwrap_or_default();
            // newest first: the first matching untriggered level is the one just added
            Ok(rows.iter().filter_map(bybit_row)
                .find(|(t, open)| *open && t.pos == pos && t.take_profit == take_profit && (t.trigger_px - pxf).abs() < r.tick / 2.0
                      && t.qty.is_some_and(|x| (x - qf).abs() < r.step / 2.0))
                .map(|(t, _)| t.id).unwrap_or_default())
        }
        Exchange::Binance => binance_place(k, &binance_params(symbol, pos, mode, take_profit, &px, Some(&q), trigger)).await,
        _ => bail!("{ex:?} TP/SL not implemented"),
    }
}

/// Every active TP/SL of the account. Bybit: `/v5/order/realtime`, all pages of 50 rows (50/s per UID).
/// Binance: `openAlgoOrders` without symbol = weight 40: snapshot on (re)connect only, never a timer.
pub async fn list(ex: Exchange, k: &Keys) -> Result<Vec<TpSl>> {
    match ex {
        Exchange::Bybit => {
            // regular orders share the list; every page is read so a TP/SL behind 50+ resting orders still shows
            let rows = bybit::realtime(k, "linear", None).await?;
            Ok(rows.iter().filter_map(bybit_row).filter(|(_, open)| *open).map(|(t, _)| t).collect())
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

/// Cancels every given TP/SL, one request after another. By id on purpose: the bulk endpoints
/// (Bybit cancel-all StopOrder, Binance algoOpenOrders) also take conditional entries and trailing stops.
pub async fn cancel_all(ex: Exchange, k: &Keys, list: &[TpSl]) -> Result<CancelReport> {
    let mut rep = CancelReport::default();
    for t in list { rep.add(format!("{} {}", t.symbol, t.id), cancel(ex, k, &t.symbol, &t.id).await); }
    Ok(rep)
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
        // a TP/SL is never also an open order
        assert!(binance_algo_order(&o).is_none() && binance_algo_push_order(&ev("o", "NEW")).is_none());
    }

    #[test]
    fn binance_algo_entries_are_open_orders() {
        // the documented sample (an opening SELL TAKE_PROFIT) is an open order with its trigger and client id
        let doc: Value = serde_json::from_str(r#"{"algoId":2148627,"clientAlgoId":"MRumok0dkhrP4kCm12AHaB","algoType":"CONDITIONAL","orderType":"TAKE_PROFIT","symbol":"BNBUSDT","side":"SELL","positionSide":"BOTH","timeInForce":"GTC","quantity":"0.01","algoStatus":"NEW","actualOrderId":"","actualPrice":"0.00000","triggerPrice":"750.000","price":"750.000","icebergQuantity":"null","tpTriggerPrice":"0.000","tpPrice":"0.000","slTriggerPrice":"0.000","slPrice":"0.000","tpOrderType":"","selfTradePreventionMode":"EXPIRE_MAKER","workingType":"CONTRACT_PRICE","priceMatch":"NONE","closePosition":false,"priceProtect":false,"reduceOnly":false,"createTime":1750514941540,"updateTime":1750514941540,"triggerTime":0,"goodTillDate":0}"#).unwrap();
        let o = binance_algo_order(&doc).unwrap();
        assert_eq!((o.id.as_str(), o.symbol.as_str(), o.side, o.kind.as_str(), o.price, o.qty, o.trigger, o.client_id.as_deref(), o.tif.as_str(), o.reduce_only, o.pos, o.ts),
                   ("2148627", "BNBUSDT", Side::Sell, "take_profit", 750.0, 0.01, Some(750.0), Some("MRumok0dkhrP4kCm12AHaB"), "GTC", false, None, 1750514941540));
        // trailing stops are open orders even when they reduce; trigger is Some (activation, or 0 without one)
        let tr = json!({"algoId": 9, "algoType": "CONDITIONAL", "orderType": "TRAILING_STOP_MARKET", "symbol": "BTCUSDT", "side": "SELL", "positionSide": "LONG",
                        "quantity": "0.5", "price": "0", "triggerPrice": "0", "activatePrice": "95000", "callbackRate": "1.0", "reduceOnly": false});
        let t = binance_algo_order(&tr).unwrap();
        assert_eq!((t.kind.as_str(), t.trigger, t.price, t.pos), ("trailing_stop_market", Some(95000.0), 0.0, Some(Side::Buy)));
        let mut tr0 = tr.clone();
        tr0["activatePrice"] = Value::Null;
        assert_eq!(binance_algo_order(&tr0).unwrap().trigger, Some(0.0));
        // a hedge opening STOP_MARKET (BUY on LONG): open order, no TP/SL
        let st = json!({"algoId": 11, "algoType": "CONDITIONAL", "orderType": "STOP_MARKET", "symbol": "BTCUSDT", "side": "BUY", "positionSide": "LONG",
                        "quantity": "0.1", "price": "0.00", "triggerPrice": "101000", "workingType": "MARK_PRICE", "closePosition": false, "reduceOnly": false});
        assert!(binance_row(&st).is_none());
        assert_eq!(binance_algo_order(&st).map(|o| (o.kind, o.trigger, o.price)), Some(("stop_market".to_string(), Some(101000.0), 0.0)));
        // other algo types are neither
        assert!(binance_algo_order(&json!({"algoType": "TWAP", "orderType": "MARKET"})).is_none());
        // ALGO_UPDATE of a non-reducing order: open while NEW, done (by algoId) once cancelled
        let ev = |x: &str| json!({"e": "ALGO_UPDATE", "T": 1750515742297u64, "E": 1, "o": {"caid": "t1abc", "aid": 2148719, "at": "CONDITIONAL", "o": "TAKE_PROFIT",
            "s": "BNBUSDT", "S": "SELL", "ps": "BOTH", "f": "GTC", "q": "0.01", "X": x, "tp": "750", "p": "750", "wt": "CONTRACT_PRICE", "cp": false, "R": false}});
        let (o, open) = binance_algo_push_order(&ev("NEW")).unwrap();
        assert!(open);
        assert_eq!((o.id.as_str(), o.kind.as_str(), o.trigger, o.price, o.client_id.as_deref(), o.ts), ("2148719", "take_profit", Some(750.0), 750.0, Some("t1abc"), 1750515742297));
        let (o, open) = binance_algo_push_order(&ev("CANCELED")).unwrap();
        assert!(!open);
        assert_eq!(o.id, "2148719");
        assert!(binance_algo_update(&ev("NEW")).is_none());
    }
}
