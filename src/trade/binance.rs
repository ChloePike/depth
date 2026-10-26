//! Binance USD-M futures private API (Portfolio Margin auto-detected: /fapi paths map to /papi/v1/um),
//! native TWAP, wallets / transfers and the user data stream, behind the functions trade.rs dispatches
//! to. Unified symbols ("BTCUSDT") and base-unit sizes in and out.
use super::*;

pub(super) fn binance_url() -> &'static str { if testnet() { "https://testnet.binancefuture.com" } else { "https://fapi.binance.com" } }

pub(super) fn papi_url() -> &'static str { "https://papi.binance.com" }

/// Portfolio Margin accounts reject /fapi and trade USD-M through /papi/v1/um. Detected once per
/// process from /papi/v1/account; only a definite answer is cached (network errors retry).
static BINANCE_PM: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

pub(super) async fn binance_pm(k: &Keys) -> Result<bool> {
    if testnet() { return Ok(false); }
    if let Some(pm) = *BINANCE_PM.lock().unwrap() { return Ok(pm); }
    let pm = match binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/account", &[]).await {
        Ok(_) => true,
        // only a real API refusal means "not Portfolio Margin"; network or rate-limit errors retry later
        Err(e) if e.downcast_ref::<reqwest::Error>().is_some() || format!("{e}").contains("rate limited") => return Err(e),
        Err(_) => false,
    };
    *BINANCE_PM.lock().unwrap() = Some(pm);
    Ok(pm)
}

/// Raw Portfolio Margin account (diagnostics: `account BTC pm`).
pub async fn pm_account(k: &Keys) -> Result<Value> { binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/account", &[]).await }

/// The Portfolio Margin equivalent of a USD-M endpoint.
fn pm_path(fapi: &str) -> &str {
    match fapi {
        "/fapi/v3/account" => "/papi/v1/account",
        "/fapi/v3/positionRisk" => "/papi/v1/um/positionRisk",
        "/fapi/v1/openOrders" => "/papi/v1/um/openOrders",
        "/fapi/v1/allOpenOrders" => "/papi/v1/um/allOpenOrders",
        "/fapi/v1/order" => "/papi/v1/um/order",
        "/fapi/v1/positionSide/dual" => "/papi/v1/um/positionSide/dual",
        "/fapi/v1/symbolConfig" => "/papi/v1/um/symbolConfig",
        "/fapi/v1/algoOrder" => "/papi/v1/um/algo/order",
        "/fapi/v1/openAlgoOrders" => "/papi/v1/um/algo/openAlgoOrders",
        "/fapi/v1/leverage" => "/papi/v1/um/leverage",
        "/fapi/v1/leverageBracket" => "/papi/v1/um/leverageBracket",
        p => p,
    }
}

async fn binance(k: &Keys, method: reqwest::Method, path: &str, params: &[(&str, String)]) -> Result<Value> {
    if binance_pm(k).await? { binance_at(k, papi_url(), method, pm_path(path), params).await }
    else { binance_at(k, binance_url(), method, path, params).await }
}

pub(super) async fn binance_at(k: &Keys, base: &str, method: reqwest::Method, path: &str, params: &[(&str, String)]) -> Result<Value> {
    super::no_demo(k)?;
    let mut q: Vec<String> = params.iter().map(|(a, b)| format!("{a}={b}")).collect();
    q.push(format!("recvWindow={}", if method == reqwest::Method::GET { RECV_READ } else { RECV_WRITE }));
    q.push(format!("timestamp={}", crate::now_ms()));
    let q = q.join("&");
    let url = format!("{base}{path}?{q}&signature={}", hmac_hex(&k.secret, &q));
    ws::check_paused(&url)?;
    let r = ws::http().request(method, &url).header("X-MBX-APIKEY", &k.key).send().await?;
    let st = r.status();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let body = r.text().await?;
    ws::note_limit(&url, st.as_u16(), retry_after, &body);
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !st.is_success() { bail!("binance {path}: {} ({})", v["msg"].as_str().unwrap_or("?"), v["code"]); }
    Ok(v)
}

pub async fn rules(symbol: &str) -> Result<Rules> {
    let v = ws::get_json(&format!("{}/fapi/v1/exchangeInfo", binance_url())).await?;
    let s = v["symbols"].as_array().into_iter().flatten().find(|s| s["symbol"] == symbol).ok_or_else(|| anyhow!("{symbol} not listed"))?;
    let f = |t: &str| s["filters"].as_array().into_iter().flatten().find(|x| x["filterType"] == t).cloned().unwrap_or_default();
    Ok(Rules { tick: n(&f("PRICE_FILTER")["tickSize"]), step: n(&f("LOT_SIZE")["stepSize"]), min_qty: n(&f("LOT_SIZE")["minQty"]), min_notional: n(&f("MIN_NOTIONAL")["notional"]) })
}

/// Binance USD-M order params. Hedge: positionSide LONG/SHORT and no reduceOnly (rejected there);
/// one-way: reduceOnly marks a close.
pub fn binance_params(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>) -> Vec<(&'static str, String)> {
    let mut p = vec![("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into())];
    if let Some(id) = &req.client_id { p.push(("newClientOrderId", id.clone())); }
    match req.kind {
        Kind::Market => p.push(("type", "MARKET".into())),
        Kind::Limit { tif, .. } => {
            p.push(("type", "LIMIT".into()));
            p.push(("price", px.unwrap_or_default().into()));
            p.push(("timeInForce", match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::PostOnly => "GTX", Tif::Fok => "FOK" }.into()));
        }
        // conditional and trailing orders go to the algo service (binance_algo_params)
        Kind::Stop { .. } | Kind::Trailing { .. } => p.push(("type", "MARKET".into())),
        // no price: Binance takes it from its book (priceMatch; cannot be sent with a price)
        Kind::Bbo { queue, level } => {
            p.push(("type", "LIMIT".into()));
            p.push(("timeInForce", "GTC".into()));
            let side = if queue { "QUEUE" } else { "OPPONENT" };
            p.push(("priceMatch", if level <= 1 { side.to_string() } else { format!("{side}_{level}") }));
        }
    }
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    p
}

/// Binance algo-order params for conditional and trailing orders (`POST /fapi/v1/algoOrder`).
/// STOP / TAKE_PROFIT follows from the trigger's side of the current price and the order side:
/// a buy triggered above the price is a stop, below it a take-profit (the reverse for a sell).
pub fn binance_algo_params(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>, ref_px: f64, tick: f64) -> Result<Vec<(&'static str, String)>> {
    let buy = req.side() == Side::Buy;
    let mut p = vec![("algoType", "CONDITIONAL".to_string()), ("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into())];
    match req.kind {
        Kind::Stop { trigger, limit, by_mark } => {
            let stop = (trigger > ref_px) == buy;
            let t = match (stop, limit.is_some()) { (true, false) => "STOP_MARKET", (false, false) => "TAKE_PROFIT_MARKET", (true, true) => "STOP", (false, true) => "TAKE_PROFIT" };
            p.push(("type", t.into()));
            p.push(("triggerPrice", fmt_step(trigger, tick, false)));
            p.push(("workingType", if by_mark { "MARK_PRICE" } else { "CONTRACT_PRICE" }.into()));
            if limit.is_some() { p.push(("price", px.unwrap_or_default().into())); p.push(("timeInForce", "GTC".into())); }
        }
        Kind::Trailing { callback_pct, activation } => {
            p.push(("type", "TRAILING_STOP_MARKET".into()));
            p.push(("callbackRate", format!("{callback_pct:.1}")));
            if let Some(a) = activation { p.push(("activatePrice", fmt_step(a, tick, false))); }
        }
        _ => bail!("not a conditional order"),
    }
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    if let Some(id) = &req.client_id { p.push(("clientAlgoId", id.clone())); }
    Ok(p)
}

/// Binance futures TWAP params. Hedge: positionSide, never reduceOnly; one-way close: reduceOnly.
pub fn binance_twap_params(req: &OrderReq, mode: Mode, qty: &str, minutes: u32, limit: Option<&str>, client_id: &str) -> Vec<(&'static str, String)> {
    let mut p = vec![("symbol", req.symbol.clone()), ("side", side_str(Exchange::Binance, req.side()).into()), ("quantity", qty.into()),
                     ("duration", (minutes as u64 * 60).to_string()), ("clientAlgoId", client_id.into())];
    match mode {
        Mode::Hedge => p.push(("positionSide", if req.pos == Side::Buy { "LONG" } else { "SHORT" }.into())),
        Mode::OneWay => if req.close { p.push(("reduceOnly", "true".into())); },
    }
    if let Some(l) = limit { p.push(("limitPrice", l.into())); }
    p
}

pub async fn place_twap(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode, minutes: u32, limit: Option<f64>) -> Result<String> {
    let qty = fmt_step(req.qty, r.step, true);
    let notional = qty.parse::<f64>().unwrap_or(0.0) * limit.unwrap_or(ref_px);
    if notional < 1_000.0 { bail!("binance: a TWAP needs at least 1,000 USDT notional (this is {notional:.0})"); }
    let id = format!("t1twap{}", crate::now_ms());
    let lim = limit.map(|l| fmt_step(l, r.tick, false));
    let v = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/algo/futures/newOrderTwap", &binance_twap_params(req, mode, &qty, minutes, lim.as_deref(), &id)).await?;
    if v["success"] != true { bail!("binance TWAP: {}", v["msg"].as_str().unwrap_or("rejected")); }
    Ok(v["clientAlgoId"].as_str().unwrap_or(&id).to_string())
}

pub async fn cancel_twap(k: &Keys, _symbol: &str, id: &str) -> Result<()> {
    let v = binance_at(k, sapi_url(), reqwest::Method::DELETE, "/sapi/v1/algo/futures/order", &[("clientAlgoId", id.into())]).await?;
    if v["success"] == false { bail!("binance TWAP cancel: {}", v["msg"].as_str().unwrap_or("rejected")); }
    Ok(())
}

pub async fn mode(k: &Keys, _symbol: &str) -> Result<Mode> {
    let v = binance(k, reqwest::Method::GET, "/fapi/v1/positionSide/dual", &[]).await?;
    Ok(if v["dualSidePosition"].as_bool() == Some(true) { Mode::Hedge } else { Mode::OneWay })
}

pub async fn max_leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let v = binance(k, reqwest::Method::GET, "/fapi/v1/leverageBracket", &[("symbol", symbol.into())]).await?;
    // USD-M returns an array of {symbol, brackets}; Portfolio Margin the same shape
    let row = if v.is_array() { v[0].clone() } else { v };
    crate::opt_num(&row["brackets"][0]["initialLeverage"]).ok_or_else(|| anyhow!("binance: no leverage brackets for {symbol}"))
}

pub async fn set_leverage(k: &Keys, symbol: &str, lev: u32) -> Result<()> {
    binance(k, reqwest::Method::POST, "/fapi/v1/leverage", &[("symbol", symbol.into()), ("leverage", lev.to_string())]).await.map(|_| ())
}

pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let v = binance(k, reqwest::Method::GET, "/fapi/v1/symbolConfig", &[("symbol", symbol.into())]).await?;
    v[0]["leverage"].as_f64().ok_or_else(|| anyhow!("binance: no leverage for {symbol}"))
}

pub async fn place_algo(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode, qty: &str, px: Option<&str>) -> Result<String> {
    tpsl::binance_place(k, &binance_algo_params(req, mode, &qty, px.as_deref(), ref_px, r.tick)?).await
}

pub async fn place(k: &Keys, req: &OrderReq, _r: &Rules, _ref_px: f64, mode: Mode, qty: &str, px: Option<&str>) -> Result<String> {
    Ok(binance(k, reqwest::Method::POST, "/fapi/v1/order", &binance_params(req, mode, &qty, px.as_deref())).await?["orderId"].to_string())
}

pub async fn can_trade(k: &Keys) -> Result<Option<bool>> {
    let v = binance_at(k, sapi_url(), reqwest::Method::GET, "/sapi/v1/account/apiRestrictions", &[]).await?;
    let flag = if binance_pm(k).await? { "enablePortfolioMarginTrading" } else { "enableFutures" };
    Ok(v[flag].as_bool())
}

/// Modify (PUT /fapi/v1/order, PM /papi/v1/um/order): LIMIT only; side, quantity (the new total) and
/// price every time. A quantity at or below what filled would cancel the order (`amend_checked`
/// refuses it first). reduceOnly=true on a reduce-only order skips the venue's min-notional check as
/// `checked` does (documented on USD-M only: PM gets no reduceOnly, the venue may refuse a small one).
pub async fn amend(k: &Keys, o: &OpenOrder, px: &str, qty: &str) -> Result<()> {
    // the algo service has no modify
    if o.trigger.is_some() { bail!("binance: a conditional or trailing order cannot be amended; cancel it and place a new one"); }
    let mut p = vec![("symbol", o.symbol.clone()), ("orderId", o.id.clone()), ("side", side_str(Exchange::Binance, o.side).into()), ("quantity", qty.into()), ("price", px.into())];
    if o.reduce_only && !binance_pm(k).await? { p.push(("reduceOnly", "true".into())); }
    binance(k, reqwest::Method::PUT, "/fapi/v1/order", &p).await?;
    Ok(())
}

pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    binance(k, reqwest::Method::DELETE, "/fapi/v1/order", &[("symbol", symbol.into()), ("orderId", id.into())]).await?;
    Ok(())
}

/// Algo-service rows (trigger Some: conditional and trailing orders) cancel by algoId, the rest by orderId.
pub async fn cancel_order(k: &Keys, o: &OpenOrder) -> Result<()> {
    if o.trigger.is_some() { tpsl::binance_cancel(k, &o.id).await } else { cancel(k, &o.symbol, &o.id).await }
}

/// Distinct symbols in first-seen order, with how many of the orders each has.
fn by_symbol<'a>(orders: impl Iterator<Item = &'a OpenOrder>) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = vec![];
    for o in orders { match out.iter_mut().find(|(s, _)| *s == o.symbol) { Some(e) => e.1 += 1, None => out.push((o.symbol.clone(), 1)) } }
    out
}

/// One bulk call per symbol; a refused symbol is reported and the others still go out. `sent` counts
/// the given orders of each accepted symbol (the venue does not say how many it cancelled).
async fn per_symbol<F, Fut>(rep: &mut CancelReport, groups: Vec<(String, usize)>, mut call: F)
where F: FnMut(String) -> Fut, Fut: std::future::Future<Output = Result<()>> {
    for (sym, n) in groups {
        match call(sym.clone()).await { Ok(()) => rep.sent += n, Err(e) => rep.failed.push(format!("binance {sym} all open orders: {e:#}")) }
    }
}

/// Every resting order of each symbol in `orders` (DELETE allOpenOrders, weight 1 per symbol; PM
/// /papi/v1/um/allOpenOrders; symbols not in `orders` are not touched), then each algo row
/// (conditional / trailing) by algoId, weight 1 each. TP/SL stay (tpsl::cancel_all): the venue's
/// per-symbol algo cancel-all would take them too.
pub async fn cancel_all(k: &Keys, orders: &[OpenOrder]) -> Result<CancelReport> {
    let mut rep = CancelReport::default();
    per_symbol(&mut rep, by_symbol(orders.iter().filter(|o| o.trigger.is_none())), |s| async move {
        binance(k, reqwest::Method::DELETE, "/fapi/v1/allOpenOrders", &[("symbol", s)]).await.map(|_| ())
    }).await;
    for o in orders.iter().filter(|o| o.trigger.is_some()) { rep.add(format!("{} {}", o.symbol, o.id), tpsl::binance_cancel(k, &o.id).await); }
    Ok(rep)
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    let ex = Exchange::Binance;
    let v = binance(k, reqwest::Method::GET, "/fapi/v3/positionRisk", &[]).await?;
    Ok(v.as_array().into_iter().flatten().filter(|p| n(&p["positionAmt"]) != 0.0).map(|p| {
        let amt = n(&p["positionAmt"]);
        // Portfolio Margin rows carry leverage instead of initialMargin
        let margin = crate::opt_num(&p["initialMargin"]).unwrap_or_else(|| { let l = n(&p["leverage"]); if l > 0.0 { n(&p["notional"]).abs() / l } else { 0.0 } });
        // hedge mode reports LONG / SHORT rows; one-way reports BOTH with a signed amount
        let side = match p["positionSide"].as_str() { Some("LONG") => Side::Buy, Some("SHORT") => Side::Sell, _ => if amt < 0.0 { Side::Sell } else { Side::Buy } };
        Position { ex, symbol: p["symbol"].as_str().unwrap_or_default().into(), side,
            qty: amt.abs(), entry: n(&p["entryPrice"]), mark: n(&p["markPrice"]), liq: crate::opt_num(&p["liquidationPrice"]).filter(|x| *x > 0.0),
            upnl: n(&p["unRealizedProfit"]), lev: if margin > 0.0 { n(&p["notional"]).abs() / margin } else { 0.0 }, margin,
            // v3 USD-M rows: isolatedWallet > 0 is isolated; Portfolio Margin is always cross
            cross: Some(is_pm() || match p["marginType"].as_str() { Some(m) => m != "isolated", None => n(&p["isolatedWallet"]) == 0.0 }) }
    }).collect())
}

/// One REST order row (`openOrders`, `order` query).
fn binance_order(o: &Value) -> OpenOrder {
    OpenOrder {
        ex: Exchange::Binance, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].to_string(),
        side: if o["side"] == "SELL" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["origQty"]), filled: n(&o["executedQty"]),
        tif: o["timeInForce"].as_str().unwrap_or_default().into(), kind: o["type"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: o["time"].as_i64().unwrap_or(0),
        pos: match o["positionSide"].as_str() { Some("LONG") => Some(Side::Buy), Some("SHORT") => Some(Side::Sell), _ => None },
        client_id: opt_id(&o["clientOrderId"]), trigger: None,
    }
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let v = binance(k, reqwest::Method::GET, "/fapi/v1/openOrders", &[]).await?;
    let mut out: Vec<OpenOrder> = v.as_array().into_iter().flatten().map(binance_order).collect();
    // conditional entries and trailing stops live in the algo service (its TP/SL rows go to tpsl::list,
    // which shares this fetch)
    out.extend(tpsl::binance_algo_rows(k).await?.iter().filter_map(tpsl::binance_algo_order));
    Ok(out)
}

/// A queried order (`/fapi/v1/order`): open while NEW / PARTIALLY_FILLED.
fn binance_found(o: &Value) -> Found {
    match o["status"].as_str() {
        Some("NEW" | "PARTIALLY_FILLED") => Found::Open(binance_order(o)),
        s => Found::Done { id: o["orderId"].to_string(), filled: crate::opt_num(&o["executedQty"]).unwrap_or(0.0), status: s.unwrap_or_default().into() },
    }
}

/// A queried algo order (`algoOrder`): open while NEW.
fn binance_algo_found(o: &Value) -> Option<Found> {
    Some(match o["algoStatus"].as_str() {
        Some("NEW") => Found::Open(tpsl::binance_algo_as_order(o)?),
        s => Found::Done { id: o["algoId"].to_string(), filled: crate::opt_num(&o["executedQty"]).unwrap_or(0.0), status: s.unwrap_or_default().into() },
    })
}

/// Our order by client id, one call each, weight 1: `/fapi/v1/order?origClientOrderId=` (PM
/// /papi/v1/um/order); when that does not know it (-2013) the algo service by clientAlgoId
/// (conditional and trailing orders; PM /papi/v1/um/algo/algoOrder). Any other answer is an error.
pub async fn order_by_client_id(k: &Keys, symbol: &str, cid: &str) -> Result<Option<Found>> {
    let unknown = |e: &anyhow::Error| format!("{e:#}").contains("(-2013)");
    match binance(k, reqwest::Method::GET, "/fapi/v1/order", &[("symbol", symbol.into()), ("origClientOrderId", cid.into())]).await {
        Ok(v) => return Ok(Some(binance_found(&v))),
        Err(e) if !unknown(&e) => return Err(e),
        Err(_) => {}
    }
    let (base, path) = if binance_pm(k).await? { (papi_url(), "/papi/v1/um/algo/algoOrder") } else { (binance_url(), "/fapi/v1/algoOrder") };
    match binance_at(k, base, reqwest::Method::GET, path, &[("clientAlgoId", cid.into())]).await {
        Ok(v) => Ok(binance_algo_found(&v)),
        Err(e) if unknown(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let v = binance(k, reqwest::Method::GET, "/fapi/v3/account", &[]).await?;
    // Portfolio Margin (/papi/v1/account) vs USD-M (/fapi/v3/account) field names
    Ok(if v.get("accountEquity").is_some() {
        Balance { equity: n(&v["accountEquity"]), available: n(&v["totalAvailableBalance"]), uni_mmr: crate::opt_num(&v["uniMMR"]), mm_rate: None,
            maint_margin: crate::opt_num(&v["accountMaintMargin"]), adj_equity: crate::opt_num(&v["actualEquity"]) }
    } else {
        Balance { equity: n(&v["totalMarginBalance"]), available: n(&v["availableBalance"]), ..Default::default() }
    })
}

fn binance_ws() -> &'static str { if testnet() { "wss://stream.binancefuture.com" } else { "wss://fstream.binance.com" } }

/// listenKey create (POST) / keepalive (PUT): API key header only, no signature.
pub(super) async fn listen_key(k: &Keys, pm: bool, method: reqwest::Method) -> Result<String> {
    super::no_demo(k)?;
    let url = if pm { format!("{}/papi/v1/listenKey", papi_url()) } else { format!("{}/fapi/v1/listenKey", binance_url()) };
    ws::check_paused(&url)?;
    let r = ws::http().request(method, &url).header("X-MBX-APIKEY", &k.key).send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    ws::note_limit(&url, st, None, &body);
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !(200..300).contains(&st) { bail!("binance listenKey: {} ({})", v["msg"].as_str().unwrap_or(&body), v["code"]); }
    Ok(v["listenKey"].as_str().unwrap_or_default().to_string())
}

pub async fn stream(k: Keys, tx: AccTx) {
    if k.demo { return; }
    let ex = Exchange::Binance;
    // mark prices of every USD-M symbol, for live upnl of any open position (public, no weight)
    // ponytail: the all-symbol array (~600 rows/s); subscribe per position symbol if bandwidth matters
    let txm = tx.clone();
    tokio::spawn(ws::run(ws::Spec::new("binance marks", format!("{}/market/ws/!markPrice@arr@1s", binance_ws())), move |f| {
        let ws::Frame::Text(s) = f else { return };
        let Ok(v) = serde_json::from_str::<Value>(s) else { return };
        for m in v.as_array().into_iter().flatten() {
            let _ = txm.send((ex, AccEvent::Mark { symbol: m["s"].as_str().unwrap_or_default().into(), mark: n(&m["p"]) }));
        }
    }));
    loop {
        let mut up = false;
        let res: Result<()> = async {
            let pm = binance_pm(&k).await?;
            let key = listen_key(&k, pm, reqwest::Method::POST).await?;
            // Portfolio Margin: /pm/ws/<key>; USD-M: the routed /private endpoint (unrouted /ws
            // URLs only carry public data since the 2026 split)
            let url = if pm { format!("{}/pm/ws/{key}", binance_ws()) } else { format!("{}/private/ws?listenKey={key}", binance_ws()) };
            up = true;
            let _ = tx.send((ex, AccEvent::Resync));
            let k2 = k.clone();
            let keepalive = async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
                    if let Err(e) = listen_key(&k2, pm, reqwest::Method::PUT).await { eprintln!("[binance account] keepalive: {e:#}"); }
                }
            };
            let spec = ws::Spec { idle: std::time::Duration::from_secs(3600), ..ws::Spec::new("binance account", url) };
            let session = ws::once(&spec, |f| {
                let ws::Frame::Text(s) = f else { return true };
                let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
                let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
                // Portfolio Margin also streams cross-margin / coin-M events; only USD-M is traded here
                if v["fs"].as_str().is_some_and(|fs| fs != "UM") { return true; }
                match v["e"].as_str() {
                    Some("ACCOUNT_UPDATE") => {
                        for p in v["a"]["P"].as_array().into_iter().flatten() {
                            let amt = n(&p["pa"]);
                            let ps = p["ps"].as_str().unwrap_or("BOTH");
                            let side = match ps { "LONG" => Side::Buy, "SHORT" => Side::Sell, _ => if amt < 0.0 { Side::Sell } else { Side::Buy } };
                            send(AccEvent::Position { one_way: ps == "BOTH", p: Position {
                                ex, symbol: p["s"].as_str().unwrap_or_default().into(), side, qty: amt.abs(), entry: n(&p["ep"]),
                                mark: 0.0, liq: None, upnl: n(&p["up"]), lev: 0.0, margin: 0.0, cross: if is_pm() { Some(true) } else { p["mt"].as_str().map(|m| m != "isolated") } } });
                        }
                        send(AccEvent::BalanceDirty);
                    }
                    Some("ORDER_TRADE_UPDATE") => {
                        let o = &v["o"];
                        let id = o["i"].to_string();
                        if o["x"] == "TRADE" {
                            send(AccEvent::Fill(Fill {
                                ex, id: o["t"].to_string(), symbol: o["s"].as_str().unwrap_or_default().into(), side: if o["S"] == "SELL" { Side::Sell } else { Side::Buy },
                                price: n(&o["L"]), qty: n(&o["l"]), fee: n(&o["n"]), realized: crate::opt_num(&o["rp"]), ts: o["T"].as_i64().unwrap_or(0),
                            }));
                        }
                        if let Some(l) = binance_liquidation(o) { send(l); }
                        if matches!(o["X"].as_str(), Some("NEW" | "PARTIALLY_FILLED")) {
                            send(AccEvent::Order(OpenOrder {
                                ex, symbol: o["s"].as_str().unwrap_or_default().into(), id, side: if o["S"] == "SELL" { Side::Sell } else { Side::Buy },
                                price: n(&o["p"]), qty: n(&o["q"]), filled: n(&o["z"]), tif: o["f"].as_str().unwrap_or_default().into(), kind: o["o"].as_str().unwrap_or_default().into(),
                                reduce_only: o["R"].as_bool().unwrap_or(false), ts: o["T"].as_i64().unwrap_or(0),
                                pos: match o["ps"].as_str() { Some("LONG") => Some(Side::Buy), Some("SHORT") => Some(Side::Sell), _ => None },
                                client_id: opt_id(&o["c"]), trigger: None,
                            }));
                        } else {
                            send(AccEvent::OrderDone { id });
                        }
                    }
                    Some("balanceUpdate" | "outboundAccountPosition") => send(AccEvent::BalanceDirty),
                    Some("ALGO_UPDATE") => if let Some((t, active)) = tpsl::binance_algo_update(&v) {
                        send(if active { AccEvent::TpSl(t) } else { AccEvent::TpSlDone { id: t.id } });
                    } else if let Some((o, open)) = tpsl::binance_algo_push_order(&v) {
                        send(if open { AccEvent::Order(o) } else { AccEvent::OrderDone { id: o.id } });
                    },
                    Some("listenKeyExpired") => return false,
                    _ => {}
                }
                true
            });
            tokio::select! { r = session => r, _ = keepalive => Ok(()) }
        }.await;
        if let Err(e) = res { eprintln!("[binance account] {e:#}"); }
        // once per session that announced itself with Resync (a failing listenKey sends nothing)
        if up { let _ = tx.send((ex, AccEvent::Down)); }
        // a paused host (rate limit) fails fast: wait long enough not to spin
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// One of our positions closed by the venue: an ORDER_TRADE_UPDATE execution (TRADE, or CALCULATED,
/// Binance's liquidation execution) of a liquidation or ADL order: type / original type LIQUIDATION,
/// or the venue's client ids "autoclose-..." / "adl_autoclose". One event per execution; `side` is the
/// position's (hedge: positionSide; one-way: the opposite of the closing order's side).
fn binance_liquidation(o: &Value) -> Option<AccEvent> {
    let c = o["c"].as_str().unwrap_or_default();
    let liq = o["o"] == "LIQUIDATION" || o["ot"] == "LIQUIDATION" || c.starts_with("autoclose-") || c.starts_with("adl_autoclose");
    if !liq || !matches!(o["x"].as_str(), Some("TRADE" | "CALCULATED")) { return None; }
    let px = |f: &str| crate::opt_num(&o[f]).filter(|x| *x > 0.0);
    Some(AccEvent::Liquidated { symbol: o["s"].as_str()?.into(),
        side: match o["ps"].as_str() { Some("LONG") => Side::Buy, Some("SHORT") => Side::Sell, _ => if o["S"] == "SELL" { Side::Buy } else { Side::Sell } },
        qty: px("l").or(px("z")).unwrap_or(0.0), price: px("L").or(px("ap")).unwrap_or(0.0) })
}

pub async fn fills_window(k: &Keys, symbol: &str, base: &str, path: &str, t0: i64, t1: i64, out: &mut Vec<Fill>) -> Result<()> {
    let ex = Exchange::Binance;
    // pages of 1000 (weight 5 each), the next one from the last fill's time (repeats are deduped by id)
    let mut s = t0;
    loop {
        let t = binance_at(k, base, reqwest::Method::GET, path, &[("symbol", symbol.to_string()), ("startTime", s.to_string()), ("endTime", t1.to_string()), ("limit", "1000".into())]).await?;
        let rows: Vec<Fill> = t.as_array().into_iter().flatten().map(|f| Fill {
            ex, id: f["id"].to_string(), symbol: symbol.into(), side: if f["side"] == "SELL" { Side::Sell } else { Side::Buy },
            price: n(&f["price"]), qty: n(&f["qty"]), fee: n(&f["commission"]), realized: crate::opt_num(&f["realizedPnl"]), ts: f["time"].as_i64().unwrap_or(0),
        }).collect();
        let last = rows.iter().map(|f| f.ts).max().unwrap_or(s);
        let full = rows.len() >= 1000;
        out.extend(rows);
        if !full || last <= s { break; }
        s = last;
    }
    Ok(())
}

pub async fn history(k: &Keys, symbols: &[String], side: impl Fn(&Value) -> Side, h: &mut History) -> Result<()> {
    let ex = Exchange::Binance;
    let pm = binance_pm(k).await?;
    let (base, (orders, trades, income)) = if pm { (papi_url(), ("/papi/v1/um/allOrders", "/papi/v1/um/userTrades", "/papi/v1/um/income")) }
        else { (binance_url(), ("/fapi/v1/allOrders", "/fapi/v1/userTrades", "/fapi/v1/income")) };
    for sym in symbols.iter().take(6) {
        let q = [("symbol", sym.clone()), ("limit", "50".to_string())];
        let o = binance_at(k, base, reqwest::Method::GET, orders, &q).await?;
        h.orders.extend(o.as_array().into_iter().flatten().map(|o| HistOrder {
            ex, symbol: sym.clone(), side: side(&o["side"]), kind: o["type"].as_str().unwrap_or_default().into(), price: n(&o["price"]), avg: n(&o["avgPrice"]),
            qty: n(&o["origQty"]), filled: n(&o["executedQty"]), status: o["status"].as_str().unwrap_or_default().into(), ts: o["time"].as_i64().unwrap_or(0),
        }));
        let t = binance_at(k, base, reqwest::Method::GET, trades, &q).await?;
        h.fills.extend(t.as_array().into_iter().flatten().map(|f| Fill {
            ex, id: f["id"].to_string(), symbol: sym.clone(), side: side(&f["side"]), price: n(&f["price"]), qty: n(&f["qty"]), fee: n(&f["commission"]),
            realized: crate::opt_num(&f["realizedPnl"]), ts: f["time"].as_i64().unwrap_or(0),
        }));
    }
    // closes reconstructed from fills that realized PnL (side, size, exit VWAP, entry implied);
    // symbols whose fills weren't fetched fall back to the income ledger (PnL only)
    h.closed = closes_from_fills(&h.fills);
    let c = binance_at(k, base, reqwest::Method::GET, income, &[("incomeType", "REALIZED_PNL".into()), ("limit", "100".into())]).await?;
    let rows: Vec<ClosedPnl> = c.as_array().into_iter().flatten().map(|c| ClosedPnl {
        ex, symbol: c["symbol"].as_str().unwrap_or_default().into(), long: None, qty: None, entry: None, exit: None, pnl: n(&c["income"]), ts: c["time"].as_i64().unwrap_or(0),
    }).filter(|c| !symbols.contains(&c.symbol)).collect();
    h.closed.extend(merge_same_second(rows));
    Ok(())
}

/// Binance userTrades rows as signed fills, newest first. Hedge mode: only the position's own
/// positionSide counts (one-way rows say BOTH).
pub(super) fn binance_fills(rows: &Value, side: Side) -> Vec<(i64, f64)> {
    let want = if side == Side::Buy { "LONG" } else { "SHORT" };
    let mut v: Vec<(i64, i64, f64)> = rows.as_array().into_iter().flatten()
        .filter(|f| matches!(f["positionSide"].as_str(), None | Some("BOTH")) || f["positionSide"] == want)
        .map(|f| (f["time"].as_i64().unwrap_or(0), f["id"].as_i64().unwrap_or(0), if f["side"] == "SELL" { -n(&f["qty"]) } else { n(&f["qty"]) })).collect();
    v.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    v.into_iter().map(|(t, _, q)| (t, q)).collect()
}

pub async fn funding_paid(k: &Keys, p: &Position, known: Option<i64>, cur: f64, now: i64) -> Result<Option<FundingPaid>> {
    let pm = binance_pm(k).await?;
    let (base, trades, income) = if pm { (papi_url(), "/papi/v1/um/userTrades", "/papi/v1/um/income") } else { (binance_url(), "/fapi/v1/userTrades", "/fapi/v1/income") };
    // the fill walk back to the open, a 7-day window at a time (the most userTrades allows, weight 5
    // each), up to 26 weeks; a full page continues inside the same window
    let (since, lower) = match known {
        Some(t) => (t, false),
        None => {
            let mut fills: Vec<(i64, f64)> = vec![];
            let mut found = None;
            'w: for w in 0..26 {
                let lo = now - (w + 1) * WEEK_MS + 1;
                let mut hi = now - w * WEEK_MS;
                loop {
                    let t = binance_at(k, base, reqwest::Method::GET, trades, &[("symbol", p.symbol.clone()), ("startTime", lo.to_string()),
                        ("endTime", hi.to_string()), ("limit", "1000".into())]).await?;
                    let page = binance_fills(&t, p.side);
                    let full = t.as_array().is_some_and(|a| a.len() == 1000);
                    let oldest = t.as_array().and_then(|a| a.iter().filter_map(|f| f["time"].as_i64()).min());
                    fills.extend(page);
                    if let Some((t, false)) = open_time(cur, &fills) { found = Some(t); break 'w; }
                    match oldest { Some(o) if full && o > lo => hi = o - 1, _ => break }
                }
            }
            match found { Some(t) => (t, false), None => (fills.last().map_or(now - 26 * WEEK_MS, |f| f.0), true) }
        }
    };
    // ponytail: hedge mode sums both sides' FUNDING_FEE rows (income rows carry no positionSide)
    let (mut amount, mut start) = (0.0, since);
    for _ in 0..5 {
        let v = binance_at(k, base, reqwest::Method::GET, income, &[("symbol", p.symbol.clone()), ("incomeType", "FUNDING_FEE".into()),
            ("startTime", start.to_string()), ("limit", "1000".into())]).await?;
        let rows = v.as_array().cloned().unwrap_or_default();
        // income is signed for the account: positive = received
        amount += rows.iter().map(|r| n(&r["income"])).sum::<f64>();
        match rows.iter().filter_map(|r| r["time"].as_i64()).max() { Some(t) if rows.len() == 1000 => start = t + 1, _ => break }
    }
    Ok(Some(FundingPaid { amount, since, lower }))
}

pub async fn wallets(k: &Keys, add: &mut impl FnMut(&str, Result<Vec<Coin>>, &std::collections::HashMap<String, f64>)) {
    let px = binance_usd_prices().await.unwrap_or_default();
    let pm = binance_pm(k).await.unwrap_or(false);
    if pm {
        let r = binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/balance", &[]).await.map(|v| {
            v.as_array().into_iter().flatten().map(|c| Coin {
                coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["totalWalletBalance"]), free: n(&c["crossMarginFree"]), ..Default::default()
            }).collect::<Vec<_>>()
        });
        // what can actually leave PM for the stablecoins is the margin max-withdraw
        let r = match r {
            Ok(mut cs) => {
                for c in cs.iter_mut().filter(|c| matches!(c.coin.as_str(), "USDT" | "USDC") && c.qty > 0.0) {
                    if let Ok(v) = binance_at(k, papi_url(), reqwest::Method::GET, "/papi/v1/margin/maxWithdraw", &[("asset", c.coin.clone())]).await { c.free = n(&v["amount"]); }
                }
                Ok(cs)
            }
            Err(e) => Err(e),
        };
        add("PM", r, &px);
    } else {
        let r = binance_at(k, binance_url(), reqwest::Method::GET, "/fapi/v3/account", &[]).await.map(|v| {
            v["assets"].as_array().into_iter().flatten().map(|c| Coin {
                coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), free: n(&c["maxWithdrawAmount"]), ..Default::default()
            }).collect()
        });
        add("USDM", r, &px);
    }
    let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v3/asset/getUserAsset", &[]).await.map(|v| {
        v.as_array().into_iter().flatten().map(|c| Coin {
            coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["free"]) + n(&c["locked"]), free: n(&c["free"]), ..Default::default()
        }).collect()
    });
    add("SPOT", r, &px);
    let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/asset/get-funding-asset", &[]).await.map(|v| {
        v.as_array().into_iter().flatten().map(|c| Coin {
            coin: c["asset"].as_str().unwrap_or_default().into(), qty: n(&c["free"]) + n(&c["locked"]) + n(&c["freeze"]), free: n(&c["free"]), ..Default::default()
        }).collect()
    });
    add("FUNDING", r, &px);
    let r = binance_at(k, sapi_url(), reqwest::Method::GET, "/sapi/v1/simple-earn/flexible/position", &[("size", "100".into())]).await.map(|v| {
        v["rows"].as_array().into_iter().flatten().map(|c| {
            let amt = n(&c["totalAmount"]);
            let can = c["canRedeem"].as_bool().unwrap_or(false);
            Coin { coin: c["asset"].as_str().unwrap_or_default().into(), qty: amt, free: if can { amt } else { 0.0 },
                product: can.then(|| c["productId"].as_str().unwrap_or_default().to_string()), ..Default::default() }
        }).collect()
    });
    add("EARN", r, &px);
}

pub fn is_pm() -> bool { BINANCE_PM.lock().unwrap().unwrap_or(false) }

pub(super) fn binance_universal(from: &str, to: &str) -> Option<&'static str> {
    Some(match (from, to) {
        ("SPOT", "PM") => "MAIN_PORTFOLIO_MARGIN", ("PM", "SPOT") => "PORTFOLIO_MARGIN_MAIN",
        ("SPOT", "USDM") => "MAIN_UMFUTURE", ("USDM", "SPOT") => "UMFUTURE_MAIN",
        ("SPOT", "FUNDING") => "MAIN_FUNDING", ("FUNDING", "SPOT") => "FUNDING_MAIN",
        ("FUNDING", "USDM") => "FUNDING_UMFUTURE", ("USDM", "FUNDING") => "UMFUTURE_FUNDING",
        _ => return None,
    })
}

pub async fn transfer(k: &Keys, from: &str, to: &str, coin: &str, amt: String, product: Option<&str>, log: &mut Vec<String>) -> Result<()> {
    let mut at = from.to_string();
    if from == "EARN" {
        let pid = product.ok_or_else(|| anyhow!("not redeemable now"))?;
        let dest = if to == "FUNDING" { "FUND" } else { "SPOT" };
        binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/simple-earn/flexible/redeem",
            &[("productId", pid.into()), ("amount", amt.clone()), ("destAccount", dest.into())]).await?;
        at = if dest == "FUND" { "FUNDING".into() } else { "SPOT".into() };
        log.push(format!("Binance redeem {amt} {coin} -> {at}"));
        if at == to { return Ok(()); }
        // the redemption credits asynchronously; give it a moment before moving on
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let hops: Vec<(String, String)> = if binance_universal(&at, to).is_some() { vec![(at.clone(), to.into())] }
        else { vec![(at.clone(), "SPOT".into()), ("SPOT".into(), to.into())] };
    for (a, b) in hops {
        let ty = binance_universal(&a, &b).ok_or_else(|| anyhow!("no transfer route {a} -> {b}"))?;
        let r = binance_at(k, sapi_url(), reqwest::Method::POST, "/sapi/v1/asset/transfer",
            &[("type", ty.into()), ("asset", coin.into()), ("amount", amt.clone())]).await;
        match r {
            Ok(v) => log.push(format!("Binance {a} -> {b} {amt} {coin}: tran {}", v["tranId"])),
            Err(e) => bail!("{}{e:#} (funds are in {a})", log.iter().map(|l| format!("{l}; ")).collect::<String>()),
        }
    }
    Ok(())
}

fn sapi_url() -> &'static str { "https://api.binance.com" }

/// Spot last prices in USDT per asset from Binance (weight 4).
async fn binance_usd_prices() -> Result<std::collections::HashMap<String, f64>> {
    let v = ws::get_json(&format!("{}/api/v3/ticker/price", sapi_url())).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(|t| Some((t["symbol"].as_str()?.strip_suffix("USDT")?.to_string(), n(&t["price"])))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binance_twap_params_by_mode() {
        let get = |v: &[(&str, String)], k: &str| v.iter().find(|(a, _)| *a == k).map(|(_, b)| b.clone());
        let o = |pos: Side, close: bool| OrderReq { symbol: "BTCUSDT".into(), pos, close, kind: Kind::Market, qty: 1.0, client_id: None, attach: None };
        let h = binance_twap_params(&o(Side::Buy, true), Mode::Hedge, "0.5", 15, Some("90000"), "t1twap1");
        assert_eq!((get(&h, "side").as_deref(), get(&h, "positionSide").as_deref(), get(&h, "reduceOnly"), get(&h, "duration").as_deref(), get(&h, "limitPrice").as_deref()),
                   (Some("SELL"), Some("LONG"), None, Some("900"), Some("90000")));
        let w = binance_twap_params(&o(Side::Sell, true), Mode::OneWay, "0.5", 5, None, "t1twap2");
        assert_eq!((get(&w, "side").as_deref(), get(&w, "reduceOnly").as_deref(), get(&w, "positionSide")), (Some("BUY"), Some("true"), None));
        assert_eq!(native_twap(Exchange::Bybit), None);
        assert_eq!(native_twap(Exchange::Binance), Some((5, 1440)));
    }

    #[test]
    fn liquidation_updates() {
        // ORDER_TRADE_UPDATE `o` of a one-way long's liquidation fill (docs field set): the fill arm takes it (x TRADE) and it is a Liquidated for the long
        let o: Value = serde_json::from_str(r#"{"s":"BTCUSDT","c":"autoclose-1596107620040000020","S":"SELL","o":"LIQUIDATION","f":"IOC","q":"0.014","p":"9910","ap":"9910.5",
            "sp":"0","x":"TRADE","X":"FILLED","i":8886774,"l":"0.014","z":"0.014","L":"9910.5","N":"USDT","n":"0.5","T":1568879465650,"t":1863,"b":"0","a":"0",
            "m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIQUIDATION","ps":"BOTH","cp":false,"rp":"-120.5","pP":false,"si":0,"ss":0}"#).unwrap();
        assert_eq!(o["x"], "TRADE");
        assert!(matches!(binance_liquidation(&o), Some(AccEvent::Liquidated { ref symbol, side: Side::Buy, qty, price }) if symbol == "BTCUSDT" && qty == 0.014 && price == 9910.5));
        // ADL of a hedge short reported as CALCULATED with only cumulative / average figures
        let adl = json!({"s": "ETHUSDT", "c": "adl_autoclose", "S": "BUY", "o": "LIMIT", "ot": "LIMIT", "x": "CALCULATED", "X": "FILLED", "l": "0", "z": "2", "L": "0", "ap": "2500", "ps": "SHORT"});
        assert!(matches!(binance_liquidation(&adl), Some(AccEvent::Liquidated { side: Side::Sell, qty, price, .. }) if qty == 2.0 && price == 2500.0));
        // our own orders, and a liquidation order that is only acknowledged, are not liquidations
        let mut own = o.clone();
        for (k, v) in [("c", "t1m19a2"), ("o", "MARKET"), ("ot", "MARKET")] { own[k] = v.into(); }
        assert!(binance_liquidation(&own).is_none());
        let mut ack = o.clone();
        ack["x"] = "NEW".into();
        assert!(binance_liquidation(&ack).is_none());
    }

    #[test]
    fn found_by_client_id() {
        // GET /fapi/v1/order (docs fields): FILLED is done with its filled size, NEW is open
        let q: Value = serde_json::from_str(r#"{"avgPrice":"0.00000","clientOrderId":"t1m19a2","cumQuote":"0","executedQty":"0.004","orderId":1917641,"origQty":"0.40",
            "origType":"LIMIT","price":"95000","reduceOnly":false,"side":"BUY","positionSide":"BOTH","status":"FILLED","stopPrice":"0","closePosition":false,"symbol":"BTCUSDT",
            "time":1579276756075,"timeInForce":"GTC","type":"LIMIT","updateTime":1579276756075,"workingType":"CONTRACT_PRICE","priceProtect":false}"#).unwrap();
        let Found::Done { id, filled, status } = binance_found(&q) else { panic!("done") };
        assert_eq!((id.as_str(), filled, status.as_str()), ("1917641", 0.004, "FILLED"));
        let mut open = q.clone();
        open["status"] = "PARTIALLY_FILLED".into();
        let Found::Open(o) = binance_found(&open) else { panic!("open") };
        assert_eq!((o.id.as_str(), o.client_id.as_deref(), o.qty, o.filled, o.trigger), ("1917641", Some("t1m19a2"), 0.4, 0.004, None));
        // algo query: a reducing stop is still found as an open order (not filtered as TP/SL), a cancelled one is done
        let a = json!({"algoId": 2146760, "clientAlgoId": "t1m19a3", "algoType": "CONDITIONAL", "orderType": "STOP_MARKET", "symbol": "BTCUSDT", "side": "SELL",
            "positionSide": "BOTH", "quantity": "0.01", "algoStatus": "NEW", "triggerPrice": "90000", "price": "0", "reduceOnly": true, "createTime": 1750485492076u64});
        let Some(Found::Open(o)) = binance_algo_found(&a) else { panic!("open algo") };
        assert_eq!((o.id.as_str(), o.kind.as_str(), o.trigger, o.reduce_only), ("2146760", "stop_market", Some(90000.0), true));
        let mut c = a.clone();
        c["algoStatus"] = "CANCELED".into();
        assert!(matches!(binance_algo_found(&c), Some(Found::Done { ref id, ref status, .. }) if id == "2146760" && status == "CANCELED"));
    }

    #[test]
    fn cancel_all_groups_symbols_and_reports_each() {
        use futures_util::FutureExt;
        let row = |sym: &str, trigger: Option<f64>| OpenOrder { ex: Exchange::Binance, symbol: sym.into(), id: "1".into(), side: Side::Buy, price: 1.0, qty: 1.0, filled: 0.0,
            kind: "LIMIT".into(), tif: "GTC".into(), reduce_only: false, ts: 0, pos: None, client_id: None, trigger };
        let orders = [row("BTCUSDT", None), row("ETHUSDT", None), row("BTCUSDT", None), row("SOLUSDT", Some(150.0)), row("XRPUSDT", None)];
        let groups = by_symbol(orders.iter().filter(|o| o.trigger.is_none()));
        assert_eq!(groups, [("BTCUSDT".to_string(), 2), ("ETHUSDT".to_string(), 1), ("XRPUSDT".to_string(), 1)]);
        // ETH is refused: BTC and XRP still go out and count, ETH is named
        let mut rep = CancelReport::default();
        let calls = std::cell::RefCell::new(vec![]);
        per_symbol(&mut rep, groups, |s| { calls.borrow_mut().push(s.clone()); async move { if s == "ETHUSDT" { Err(anyhow!("binance /fapi/v1/allOpenOrders: busy (-1008)")) } else { Ok(()) } } })
            .now_or_never().unwrap();
        assert_eq!(calls.into_inner(), ["BTCUSDT", "ETHUSDT", "XRPUSDT"]);
        assert_eq!((rep.sent, rep.failed), (3, vec!["binance ETHUSDT all open orders: binance /fapi/v1/allOpenOrders: busy (-1008)".to_string()]));
        assert_eq!(pm_path("/fapi/v1/allOpenOrders"), "/papi/v1/um/allOpenOrders");
    }
}
