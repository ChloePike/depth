//! Spot trading: Bybit v5 (`category=spot`, unified account) and Binance spot (`/api/v3`, the SPOT
//! wallet; Portfolio Margin users move USDT there first, see `transfer`). Unified symbols
//! ("BTCUSDT") and base-unit sizes, like the perp API. Orders are plain buy / sell: no position,
//! leverage or reduce-only. Margin orders (Margin mode) go through the same functions with an
//! `Effect`: Bybit unified spot with `isLeverage`, Binance cross margin (`/sapi/v1/margin`, Portfolio
//! Margin `/papi/v1/margin`) with `sideEffectType`.
//! ponytail: Bybit and Binance only; the other venues' spot order entry goes behind these functions.
use super::{binance_at, bybit, fmt_step, n, side_str, testnet, Keys, Kind, OpenOrder, Rules, Tif};
use crate::{ws, Exchange, Side};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const VENUES: &[Exchange] = &[Exchange::Bybit, Exchange::Binance];

fn binance_spot() -> &'static str { if testnet() { "https://testnet.binance.vision" } else { "https://api.binance.com" } }
fn bybit_base() -> &'static str { super::bybit_url() }

/// One coin on the venue's spot wallet (Bybit: the unified account), base units; `borrowed` is the
/// margin loan (margin wallets and the Bybit unified account only).
#[derive(Clone, Debug, PartialEq)]
pub struct Bal { pub coin: String, pub free: f64, pub locked: f64, pub borrowed: f64 }

/// What a margin order does with loans: nothing, borrow what it needs, or repay debt with what it gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect { Plain, Borrow, Repay }

/// Binance cross-margin API: (host, path prefix); Portfolio Margin accounts trade margin through papi.
async fn binance_margin(k: &Keys) -> Result<(&'static str, &'static str)> {
    Ok(if super::binance_pm(k).await? { (super::papi_url(), "/papi/v1/margin") } else { (binance_spot(), "/sapi/v1/margin") })
}

pub async fn rules(ex: Exchange, symbol: &str) -> Result<Rules> {
    match ex {
        Exchange::Bybit => {
            let v = ws::get_json(&format!("{}/v5/market/instruments-info?category=spot&symbol={symbol}", bybit_base())).await?;
            let i = &v["result"]["list"][0];
            if i.is_null() { bail!("{symbol} is not listed on Bybit spot"); }
            Ok(Rules { tick: n(&i["priceFilter"]["tickSize"]), step: n(&i["lotSizeFilter"]["basePrecision"]), min_qty: n(&i["lotSizeFilter"]["minOrderQty"]),
                       min_notional: n(&i["lotSizeFilter"]["minOrderAmt"]) })
        }
        Exchange::Binance => {
            let v = ws::get_json(&format!("{}/api/v3/exchangeInfo?symbol={symbol}", binance_spot())).await?;
            let s = &v["symbols"][0];
            if s.is_null() { bail!("{symbol} is not listed on Binance spot"); }
            let f = |t: &str| s["filters"].as_array().into_iter().flatten().find(|x| x["filterType"] == t).cloned().unwrap_or_default();
            let notional = crate::opt_num(&f("NOTIONAL")["minNotional"]).or(crate::opt_num(&f("MIN_NOTIONAL")["minNotional"])).unwrap_or(0.0);
            Ok(Rules { tick: n(&f("PRICE_FILTER")["tickSize"]), step: n(&f("LOT_SIZE")["stepSize"]), min_qty: n(&f("LOT_SIZE")["minQty"]), min_notional: notional })
        }
        _ => bail!("{ex:?}: spot trading not supported yet"),
    }
}

/// Size and price strings on the venue grid; refuses sizes under the minimum size or order value.
pub fn checked(kind: Kind, qty: f64, r: &Rules, ref_px: f64) -> Result<(String, Option<String>)> {
    if !(r.step > 0.0 && r.tick > 0.0) { bail!("no trading rules"); }
    let q = fmt_step(qty, r.step, true);
    let qf: f64 = q.parse()?;
    if qf <= 0.0 || qf < r.min_qty { bail!("size {qf} below minimum {}", r.min_qty); }
    let px = match kind { Kind::Limit { price, .. } => Some(fmt_step(price, r.tick, false)), Kind::Market => None, _ => bail!("spot takes limit and market orders only") };
    let value = qf * px.as_deref().and_then(|p| p.parse().ok()).unwrap_or(ref_px);
    if r.min_notional > 0.0 && value < r.min_notional { bail!("order value {value:.2} below minimum {}", r.min_notional); }
    Ok((q, px))
}

pub fn bybit_body(symbol: &str, side: Side, kind: Kind, qty: &str, px: Option<&str>, borrow: bool) -> Value {
    // marketUnit: a market buy is sized in quote by default; ours are always base units
    let mut b = json!({"category": "spot", "symbol": symbol, "side": side_str(Exchange::Bybit, side), "qty": qty, "marketUnit": "baseCoin"});
    if borrow { b["isLeverage"] = 1.into(); }
    match kind {
        Kind::Limit { tif, .. } => {
            b["orderType"] = "Limit".into();
            b["price"] = px.unwrap_or_default().into();
            b["timeInForce"] = match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::Fok => "FOK", Tif::PostOnly => "PostOnly" }.into();
        }
        _ => b["orderType"] = "Market".into(),
    }
    b
}

pub fn binance_params(symbol: &str, side: Side, kind: Kind, qty: &str, px: Option<&str>) -> Vec<(&'static str, String)> {
    let mut p = vec![("symbol", symbol.to_string()), ("side", side_str(Exchange::Binance, side).to_string()), ("quantity", qty.to_string())];
    match kind {
        // post-only is its own order type on Binance spot
        Kind::Limit { tif: Tif::PostOnly, .. } => { p.push(("type", "LIMIT_MAKER".into())); p.push(("price", px.unwrap_or_default().into())); }
        Kind::Limit { tif, .. } => {
            p.push(("type", "LIMIT".into()));
            p.push(("timeInForce", match tif { Tif::Ioc => "IOC", Tif::Fok => "FOK", _ => "GTC" }.into()));
            p.push(("price", px.unwrap_or_default().into()));
        }
        _ => p.push(("type", "MARKET".into())),
    }
    p
}

pub fn effect_str(e: Effect) -> &'static str { match e { Effect::Plain => "NO_SIDE_EFFECT", Effect::Borrow => "MARGIN_BUY", Effect::Repay => "AUTO_REPAY" } }

/// Spot order, or a margin order when `margin` is set.
#[allow(clippy::too_many_arguments)]
pub async fn place(ex: Exchange, k: &Keys, symbol: &str, side: Side, kind: Kind, qty: f64, r: &Rules, ref_px: f64, margin: Option<Effect>) -> Result<String> {
    let (q, px) = checked(kind, qty, r, ref_px)?;
    match ex {
        Exchange::Bybit => {
            let id = bybit(k, false, "/v5/order/create", bybit_body(symbol, side, kind, &q, px.as_deref(), margin == Some(Effect::Borrow))).await?["orderId"].as_str().unwrap_or_default().to_string();
            // ponytail: Bybit has no repay-on-fill flag; repays the liability once from what is free right
            // after placing (a market order has filled by then, a resting limit has not)
            if margin == Some(Effect::Repay) {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let coin = if side == Side::Buy { symbol.strip_suffix("USDT").unwrap_or(symbol) } else { "USDT" };
                bybit(k, false, "/v5/account/quick-repayment", json!({"coin": coin})).await.map_err(|e| anyhow!("order {id} placed, repay failed: {e:#}"))?;
            }
            Ok(id)
        }
        Exchange::Binance => {
            let mut p = binance_params(symbol, side, kind, &q, px.as_deref());
            let (host, path) = match margin {
                None => (binance_spot(), "/api/v3/order".to_string()),
                Some(e) => { p.push(("sideEffectType", effect_str(e).into())); let (h, pre) = binance_margin(k).await?; (h, format!("{pre}/order")) }
            };
            Ok(binance_at(k, host, reqwest::Method::POST, &path, &p).await?["orderId"].to_string())
        }
        _ => bail!("{ex:?}: spot trading not supported yet"),
    }
}

pub async fn cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str, margin: bool) -> Result<()> {
    match ex {
        Exchange::Bybit => { bybit(k, false, "/v5/order/cancel", json!({"category": "spot", "symbol": symbol, "orderId": id})).await?; }
        Exchange::Binance => {
            let (host, path) = if margin { let (h, pre) = binance_margin(k).await?; (h, format!("{pre}/order")) } else { (binance_spot(), "/api/v3/order".to_string()) };
            binance_at(k, host, reqwest::Method::DELETE, &path, &[("symbol", symbol.into()), ("orderId", id.into())]).await?;
        }
        _ => bail!("{ex:?}: spot trading not supported yet"),
    }
    Ok(())
}

fn binance_order(ex: Exchange, o: &Value) -> OpenOrder {
    OpenOrder {
        ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].to_string(),
        side: if o["side"] == "SELL" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["origQty"]), filled: n(&o["executedQty"]),
        tif: o["timeInForce"].as_str().unwrap_or_default().into(), kind: o["type"].as_str().unwrap_or("LIMIT").to_lowercase(), reduce_only: false, ts: n(&o["time"]) as i64, pos: None,
    }
}

/// Resting margin orders on `symbol` (Binance: weight 10, papi 5). Bybit margin orders are spot
/// orders with `isLeverage`: they come with `open_orders`.
pub async fn margin_orders(ex: Exchange, k: &Keys, symbol: &str) -> Result<Vec<OpenOrder>> {
    match ex {
        Exchange::Binance => {
            let (host, pre) = binance_margin(k).await?;
            let v = binance_at(k, host, reqwest::Method::GET, &format!("{pre}/openOrders"), &[("symbol", symbol.into())]).await?;
            Ok(v.as_array().into_iter().flatten().map(|o| binance_order(ex, o)).collect())
        }
        _ => Ok(vec![]),
    }
}

/// Coins on the margin account with their loans. Bybit: the unified wallet (same as spot).
pub async fn margin_balances(ex: Exchange, k: &Keys) -> Result<Vec<Bal>> {
    match ex {
        Exchange::Binance if super::binance_pm(k).await? => {
            let v = binance_at(k, super::papi_url(), reqwest::Method::GET, "/papi/v1/balance", &[]).await?;
            Ok(v.as_array().into_iter().flatten().map(|b| Bal { coin: b["asset"].as_str().unwrap_or_default().into(), free: n(&b["crossMarginFree"]),
                locked: n(&b["crossMarginLocked"]), borrowed: n(&b["crossMarginBorrowed"]) }).filter(|b| b.free + b.locked + b.borrowed > 0.0).collect())
        }
        Exchange::Binance => {
            let v = binance_at(k, binance_spot(), reqwest::Method::GET, "/sapi/v1/margin/account", &[]).await?;
            Ok(v["userAssets"].as_array().into_iter().flatten().map(|b| Bal { coin: b["asset"].as_str().unwrap_or_default().into(), free: n(&b["free"]),
                locked: n(&b["locked"]), borrowed: n(&b["borrowed"]) }).filter(|b| b.free + b.locked + b.borrowed > 0.0).collect())
        }
        _ => balances(ex, k).await,
    }
}

/// Resting spot orders (Binance: all symbols, weight 80; read on resync, reconcile and after our own
/// orders only).
pub async fn open_orders(ex: Exchange, k: &Keys) -> Result<Vec<OpenOrder>> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/order/realtime", json!({"category": "spot", "limit": "50"})).await?;
            Ok(v["list"].as_array().into_iter().flatten().map(|o| OpenOrder {
                ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].as_str().unwrap_or_default().into(),
                side: if o["side"] == "Sell" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]),
                tif: o["timeInForce"].as_str().unwrap_or_default().into(), kind: o["orderType"].as_str().unwrap_or("Limit").to_lowercase(), reduce_only: false, ts: n(&o["createdTime"]) as i64, pos: None,
            }).collect())
        }
        Exchange::Binance => {
            let v = binance_at(k, binance_spot(), reqwest::Method::GET, "/api/v3/openOrders", &[]).await?;
            Ok(v.as_array().into_iter().flatten().map(|o| binance_order(ex, o)).collect())
        }
        _ => Ok(vec![]),
    }
}

/// Coins with a balance. Bybit: the unified account's wallet (free = wallet - locked).
/// ponytail: Bybit unified USDT that backs perp margin is counted as free; the venue refuses a buy
/// beyond what is really available.
pub async fn balances(ex: Exchange, k: &Keys) -> Result<Vec<Bal>> {
    match ex {
        Exchange::Bybit => {
            let v = bybit(k, true, "/v5/account/wallet-balance", json!({"accountType": "UNIFIED"})).await?;
            Ok(v["list"][0]["coin"].as_array().into_iter().flatten().map(|c| {
                let (wallet, locked) = (n(&c["walletBalance"]), n(&c["locked"]));
                Bal { coin: c["coin"].as_str().unwrap_or_default().into(), free: (wallet - locked).max(0.0), locked, borrowed: n(&c["borrowAmount"]) }
            }).filter(|b| b.free + b.locked + b.borrowed > 0.0).collect())
        }
        Exchange::Binance => {
            let v = binance_at(k, binance_spot(), reqwest::Method::GET, "/api/v3/account", &[("omitZeroBalances", "true".into())]).await?;
            Ok(v["balances"].as_array().into_iter().flatten().map(|b| Bal { coin: b["asset"].as_str().unwrap_or_default().into(), free: n(&b["free"]), locked: n(&b["locked"]), borrowed: 0.0 })
                .filter(|b| b.free + b.locked > 0.0).collect())
        }
        _ => Err(anyhow!("{ex:?}: spot trading not supported yet")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_orders_round_and_refuse_dust() {
        let r = Rules { tick: 0.01, step: 0.000001, min_qty: 0.000048, min_notional: 5.0 };
        let (q, px) = checked(Kind::Limit { price: 81_000.004, tif: Tif::Gtc }, 0.0012345678, &r, 81_000.0).unwrap();
        assert_eq!((q.as_str(), px.as_deref()), ("0.001234", Some("81000.00")));
        assert!(checked(Kind::Market, 0.00005, &r, 81_000.0).is_err());
        assert!(checked(Kind::Stop { trigger: 1.0, limit: None, by_mark: true }, 1.0, &r, 1.0).is_err());
        let b = bybit_body("BTCUSDT", Side::Buy, Kind::Market, "0.001", None, false);
        assert_eq!((b["category"].as_str(), b["orderType"].as_str(), b["marketUnit"].as_str()), (Some("spot"), Some("Market"), Some("baseCoin")));
        assert!(b.get("isLeverage").is_none() && bybit_body("BTCUSDT", Side::Buy, Kind::Market, "0.001", None, true)["isLeverage"] == 1);
        let p = binance_params("BTCUSDT", Side::Sell, Kind::Limit { price: 1.0, tif: Tif::PostOnly }, "1", Some("1.00"));
        assert!(p.contains(&("type", "LIMIT_MAKER".into())) && !p.iter().any(|x| x.0 == "timeInForce"));
    }
}
