//! Spot trading: Bybit v5 (`category=spot`, unified account) and Binance spot (`/api/v3`, the SPOT
//! wallet; Portfolio Margin users move USDT there first, see `transfer`). Unified symbols
//! ("BTCUSDT") and base-unit sizes, like the perp API. Orders are plain buy / sell: no position,
//! leverage or reduce-only.
//! ponytail: Bybit and Binance only; the other venues' spot order entry goes behind these functions.
use super::{binance_at, bybit, fmt_step, n, side_str, testnet, Keys, Kind, OpenOrder, Rules, Tif};
use crate::{ws, Exchange, Side};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const VENUES: &[Exchange] = &[Exchange::Bybit, Exchange::Binance];

fn binance_spot() -> &'static str { if testnet() { "https://testnet.binance.vision" } else { "https://api.binance.com" } }
fn bybit_base() -> &'static str { super::bybit_url() }

/// One coin on the venue's spot wallet (Bybit: the unified account), base units.
#[derive(Clone, Debug, PartialEq)]
pub struct Bal { pub coin: String, pub free: f64, pub locked: f64 }

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

pub fn bybit_body(symbol: &str, side: Side, kind: Kind, qty: &str, px: Option<&str>) -> Value {
    // marketUnit: a market buy is sized in quote by default; ours are always base units
    let mut b = json!({"category": "spot", "symbol": symbol, "side": side_str(Exchange::Bybit, side), "qty": qty, "marketUnit": "baseCoin"});
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

pub async fn place(ex: Exchange, k: &Keys, symbol: &str, side: Side, kind: Kind, qty: f64, r: &Rules, ref_px: f64) -> Result<String> {
    let (q, px) = checked(kind, qty, r, ref_px)?;
    match ex {
        Exchange::Bybit => Ok(bybit(k, false, "/v5/order/create", bybit_body(symbol, side, kind, &q, px.as_deref())).await?["orderId"].as_str().unwrap_or_default().to_string()),
        Exchange::Binance => Ok(binance_at(k, binance_spot(), reqwest::Method::POST, "/api/v3/order", &binance_params(symbol, side, kind, &q, px.as_deref())).await?["orderId"].to_string()),
        _ => bail!("{ex:?}: spot trading not supported yet"),
    }
}

pub async fn cancel(ex: Exchange, k: &Keys, symbol: &str, id: &str) -> Result<()> {
    match ex {
        Exchange::Bybit => { bybit(k, false, "/v5/order/cancel", json!({"category": "spot", "symbol": symbol, "orderId": id})).await?; }
        Exchange::Binance => { binance_at(k, binance_spot(), reqwest::Method::DELETE, "/api/v3/order", &[("symbol", symbol.into()), ("orderId", id.into())]).await?; }
        _ => bail!("{ex:?}: spot trading not supported yet"),
    }
    Ok(())
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
                kind: o["orderType"].as_str().unwrap_or("Limit").to_lowercase(), reduce_only: false, ts: n(&o["createdTime"]) as i64, pos: None,
            }).collect())
        }
        Exchange::Binance => {
            let v = binance_at(k, binance_spot(), reqwest::Method::GET, "/api/v3/openOrders", &[]).await?;
            Ok(v.as_array().into_iter().flatten().map(|o| OpenOrder {
                ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].to_string(),
                side: if o["side"] == "SELL" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["origQty"]), filled: n(&o["executedQty"]),
                kind: o["type"].as_str().unwrap_or("LIMIT").to_lowercase(), reduce_only: false, ts: n(&o["time"]) as i64, pos: None,
            }).collect())
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
                Bal { coin: c["coin"].as_str().unwrap_or_default().into(), free: (wallet - locked).max(0.0), locked }
            }).filter(|b| b.free + b.locked > 0.0).collect())
        }
        Exchange::Binance => {
            let v = binance_at(k, binance_spot(), reqwest::Method::GET, "/api/v3/account", &[("omitZeroBalances", "true".into())]).await?;
            Ok(v["balances"].as_array().into_iter().flatten().map(|b| Bal { coin: b["asset"].as_str().unwrap_or_default().into(), free: n(&b["free"]), locked: n(&b["locked"]) })
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
        let b = bybit_body("BTCUSDT", Side::Buy, Kind::Market, "0.001", None);
        assert_eq!((b["category"].as_str(), b["orderType"].as_str(), b["marketUnit"].as_str()), (Some("spot"), Some("Market"), Some("baseCoin")));
        let p = binance_params("BTCUSDT", Side::Sell, Kind::Limit { price: 1.0, tif: Tif::PostOnly }, "1", Some("1.00"));
        assert!(p.contains(&("type", "LIMIT_MAKER".into())) && !p.iter().any(|x| x.0 == "timeInForce"));
    }
}
