//! Bybit v5 private API: unified account, linear USDT perps (`category=linear`), options, wallets /
//! transfers and the private stream, behind the functions trade.rs dispatches to. Unified symbols
//! ("BTCUSDT") and base-unit sizes in and out.
use super::*;

pub(super) fn bybit_url() -> &'static str { if testnet() { "https://api-testnet.bybit.com" } else { "https://api.bybit.com" } }

/// v5 signature: HMAC_SHA256(secret, timestamp + key + recv_window + (query | json body))
pub fn sign(k: &Keys, ts: i64, recv: u32, payload: &str) -> String { hmac_hex(&k.secret, &format!("{ts}{}{recv}{payload}", k.key)) }

pub(super) async fn bybit(k: &Keys, get: bool, path: &str, params: Value) -> Result<Value> {
    let ts = crate::now_ms();
    let recv = if get { RECV_READ } else { RECV_WRITE };
    let (payload, req) = if get {
        let q = params.as_object().map(|o| o.iter().map(|(a, b)| format!("{a}={}", b.as_str().map(String::from).unwrap_or(b.to_string()))).collect::<Vec<_>>().join("&")).unwrap_or_default();
        (q.clone(), ws::http().get(format!("{}{path}?{q}", bybit_url())))
    } else {
        let b = params.to_string();
        (b.clone(), ws::http().post(format!("{}{path}", bybit_url())).header("Content-Type", "application/json").body(b))
    };
    let url = format!("{}{path}", bybit_url());
    ws::check_paused(&url)?;
    let r = req.header("X-BAPI-API-KEY", &k.key).header("X-BAPI-TIMESTAMP", ts.to_string())
        .header("X-BAPI-RECV-WINDOW", recv.to_string()).header("X-BAPI-SIGN", sign(k, ts, recv, &payload)).send().await?;
    let st = r.status().as_u16();
    let body = r.text().await?;
    // Bybit answers an IP ban with 403
    ws::note_limit(&url, if st == 403 { 418 } else { st }, None, &body);
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("bybit {path}: HTTP {st}: {}", &body[..body.len().min(200)]))?;
    if v["retCode"].as_i64() != Some(0) { bail!("bybit {path}: {} ({})", v["retMsg"].as_str().unwrap_or("?"), v["retCode"]); }
    Ok(v["result"].clone())
}

pub async fn rules(symbol: &str) -> Result<Rules> {
    let v = ws::get_json(&format!("{}/v5/market/instruments-info?category=linear&symbol={symbol}", bybit_url())).await?;
    let i = &v["result"]["list"][0];
    Ok(Rules { tick: n(&i["priceFilter"]["tickSize"]), step: n(&i["lotSizeFilter"]["qtyStep"]), min_qty: n(&i["lotSizeFilter"]["minOrderQty"]),
               min_notional: crate::opt_num(&i["lotSizeFilter"]["minNotionalValue"]).unwrap_or(0.0) })
}

pub async fn mode(k: &Keys, symbol: &str) -> Result<Mode> {
    // per-symbol query returns one row per positionIdx even when flat: 1/2 means hedge
    let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "symbol": symbol})).await?;
    let hedge = v["list"].as_array().into_iter().flatten().any(|p| p["positionIdx"].as_i64().is_some_and(|i| i > 0));
    Ok(if hedge { Mode::Hedge } else { Mode::OneWay })
}

pub async fn max_leverage(symbol: &str) -> Result<f64> {
    let v = ws::get_json(&format!("{}/v5/market/instruments-info?category=linear&symbol={symbol}", bybit_url())).await?;
    crate::opt_num(&v["result"]["list"][0]["leverageFilter"]["maxLeverage"]).ok_or_else(|| anyhow!("bybit: no max leverage for {symbol}"))
}

pub async fn set_leverage(k: &Keys, symbol: &str, lev: u32) -> Result<()> {
    let l = lev.to_string();
    match bybit(k, false, "/v5/position/set-leverage", json!({"category": "linear", "symbol": symbol, "buyLeverage": l, "sellLeverage": l})).await {
        // 110043: already at this leverage
        Err(e) if format!("{e:#}").contains("110043") => Ok(()),
        r => r.map(|_| ()),
    }
}

pub async fn leverage(k: &Keys, symbol: &str) -> Result<f64> {
    let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "symbol": symbol})).await?;
    v["list"][0]["leverage"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("bybit: no leverage for {symbol}"))
}

/// Bybit v5 order body. Hedge: positionIdx 1 = long, 2 = short (reduceOnly allowed);
/// one-way: positionIdx 0.
pub fn bybit_body(req: &OrderReq, mode: Mode, qty: &str, px: Option<&str>, ref_px: f64, tick: f64) -> Value {
    let idx = match (mode, req.pos) { (Mode::OneWay, _) => 0, (Mode::Hedge, Side::Buy) => 1, (Mode::Hedge, Side::Sell) => 2 };
    let mut b = json!({"category": "linear", "symbol": req.symbol, "side": side_str(Exchange::Bybit, req.side()), "qty": qty,
                       "positionIdx": idx, "reduceOnly": req.close});
    if let Some(id) = &req.client_id { b["orderLinkId"] = id.clone().into(); }
    match req.kind {
        Kind::Market => { b["orderType"] = "Market".into(); }
        Kind::Limit { tif, .. } => {
            b["orderType"] = "Limit".into();
            b["price"] = px.unwrap_or_default().into();
            b["timeInForce"] = match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::PostOnly => "PostOnly", Tif::Fok => "FOK" }.into();
        }
        // conditional: triggerDirection 1 = fires when price rises to the trigger, 2 = falls to it
        Kind::Stop { trigger, limit, by_mark } => {
            b["orderType"] = if limit.is_some() { "Limit" } else { "Market" }.into();
            if limit.is_some() { b["price"] = px.unwrap_or_default().into(); b["timeInForce"] = "GTC".into(); }
            b["triggerPrice"] = fmt_step(trigger, tick, false).into();
            b["triggerDirection"] = if trigger > ref_px { 1 } else { 2 }.into();
            b["triggerBy"] = if by_mark { "MarkPrice" } else { "LastPrice" }.into();
        }
        // a trailing stop is a position setting on Bybit (see place_trailing); never sent as an order
        Kind::Trailing { .. } => { b["orderType"] = "Market".into(); }
        // no price: Bybit takes it from its book (bboSideType / bboLevel)
        Kind::Bbo { queue, level } => {
            b["orderType"] = "Limit".into();
            b["timeInForce"] = "GTC".into();
            b["bboSideType"] = if queue { "Queue" } else { "Counterparty" }.into();
            b["bboLevel"] = level.to_string().into();
        }
    }
    // whole-position TP/SL that the venue attaches to the position at fill (trade::place refuses it on a close)
    if let Some(a) = req.attach.filter(|_| !req.close) {
        let by = match a.trigger { tpsl::Trigger::Mark => "MarkPrice", tpsl::Trigger::Last => "LastPrice" };
        b["tpslMode"] = "Full".into();
        for (px, p, f) in [(a.tp, "tp", "takeProfit"), (a.sl, "sl", "stopLoss")] {
            let Some(px) = px else { continue };
            b[f] = fmt_step(px, tick, false).into();
            b[format!("{p}TriggerBy")] = by.into();
            b[format!("{p}OrderType")] = "Market".into();
        }
    }
    b
}

/// Bybit trailing stop: a position setting (`/v5/position/trading-stop`), so only for closing.
/// The distance is the callback percent of the current price, in price units.
pub fn bybit_trailing_body(req: &OrderReq, mode: Mode, ref_px: f64, tick: f64) -> Result<Value> {
    let Kind::Trailing { callback_pct, activation } = req.kind else { bail!("not a trailing order") };
    if !req.close { bail!("bybit: a trailing stop protects an open position: use it from Close"); }
    let idx = match (mode, req.pos) { (Mode::OneWay, _) => 0, (Mode::Hedge, Side::Buy) => 1, (Mode::Hedge, Side::Sell) => 2 };
    let mut b = json!({"category": "linear", "symbol": req.symbol, "positionIdx": idx, "tpslMode": "Full",
                       "trailingStop": fmt_step(ref_px * callback_pct / 100.0, tick, false)});
    if let Some(a) = activation { b["activePrice"] = fmt_step(a, tick, false).into(); }
    Ok(b)
}

pub async fn place_trailing(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode) -> Result<String> {
    bybit(k, false, "/v5/position/trading-stop", bybit_trailing_body(req, mode, ref_px, r.tick)?).await?;
    Ok("trailing".into())
}

pub async fn place(k: &Keys, req: &OrderReq, r: &Rules, ref_px: f64, mode: Mode, qty: &str, px: Option<&str>) -> Result<String> {
    Ok(bybit(k, false, "/v5/order/create", bybit_body(req, mode, &qty, px.as_deref(), ref_px, r.tick)).await?["orderId"].as_str().unwrap_or_default().to_string())
}

pub async fn option_rules(symbol: &str) -> Result<Rules> {
    let i = ws::get_json(&format!("{}/v5/market/instruments-info?category=option&symbol={symbol}", bybit_url())).await?;
    let i = &i["result"]["list"][0];
    let r = Rules { tick: n(&i["priceFilter"]["tickSize"]), step: n(&i["lotSizeFilter"]["qtyStep"]), min_qty: n(&i["lotSizeFilter"]["minOrderQty"]), min_notional: 0.0 };
    if !(r.tick > 0.0 && r.step > 0.0) { bail!("{symbol}: no trading rules"); }
    Ok(r)
}

pub async fn option_plan(symbol: &str, qty: f64) -> Result<OptBuy> {
    let ex = Exchange::Bybit;
    let Rules { tick, step, min_qty: min, .. } = super::option_rules(ex, symbol).await?;
    let b = ws::get_json(&format!("{}/v5/market/orderbook?category=option&symbol={symbol}&limit=25", bybit_url())).await?;
    let asks: Vec<(f64, f64)> = b["result"]["a"].as_array().into_iter().flatten().map(|l| (n(&l[0]), n(&l[1]))).filter(|(p, q)| *p > 0.0 && *q > 0.0).collect();
    let qty = ceil_step(qty, step, min);
    let (limit, fill, avg) = sweep(&asks, qty).ok_or_else(|| anyhow!("{symbol}: no asks"))?;
    Ok(OptBuy { ex, symbol: symbol.into(), qty, limit, tick, step, fill, avg, cost: fill * avg })
}

/// (base, expiry, strike, call) from a Bybit option symbol.
pub fn bybit_opt_parts(s: &str) -> Option<(String, String, f64, bool)> {
    let p: Vec<&str> = s.split('-').collect();
    if p.len() < 4 { return None; }
    Some((p[0].into(), p[1].into(), p[2].parse().ok()?, p[3] == "C"))
}

/// One Bybit option position row (REST list or `position.option` push); size 0 = closed.
fn bybit_opt_pos(p: &Value) -> Option<OptPos> {
    let symbol = p["symbol"].as_str()?;
    let (base, expiry, strike, call) = bybit_opt_parts(symbol)?;
    Some(OptPos { ex: Exchange::Bybit, symbol: symbol.into(), base, expiry, strike, call,
        side: if p["side"] == "Sell" { Side::Sell } else { Side::Buy }, qty: n(&p["size"]), avg: n(&p["avgPrice"]), mark: n(&p["markPrice"]), upnl: n(&p["unrealisedPnl"]) })
}

pub async fn option_positions(k: &Keys) -> Result<Vec<OptPos>> {
    let v = bybit(k, true, "/v5/position/list", json!({"category": "option"})).await?;
    Ok(v["list"].as_array().into_iter().flatten().filter_map(bybit_opt_pos).filter(|p| p.qty > 0.0).collect())
}

/// Bybit option order body: limit only (options take no market orders through this path).
pub fn bybit_opt_body(symbol: &str, side: Side, qty: &str, px: &str, tif: Tif, reduce_only: bool) -> Value {
    // orderLinkId is mandatory for Bybit options
    json!({"category": "option", "symbol": symbol, "side": side_str(Exchange::Bybit, side), "orderType": "Limit",
        "timeInForce": match tif { Tif::Gtc => "GTC", Tif::Ioc => "IOC", Tif::Fok => "FOK", Tif::PostOnly => "PostOnly" },
        "qty": qty, "price": px, "reduceOnly": reduce_only, "orderLinkId": format!("t1opt{}", crate::now_ms())})
}

pub async fn option_order(k: &Keys, symbol: &str, side: Side, q: &str, px: &str, tif: Tif, reduce_only: bool) -> Result<String> {
    Ok(bybit(k, false, "/v5/order/create", bybit_opt_body(symbol, side, &q, &px, tif, reduce_only)).await?["orderId"].as_str().unwrap_or_default().to_string())
}

pub async fn option_cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    bybit(k, false, "/v5/order/cancel", json!({"category": "option", "symbol": symbol, "orderId": id})).await?; Ok(())
}

/// Rows per `/v5/order/realtime` page (the maximum; the default is 20).
const PAGE: usize = 50;

/// One page of `/v5/order/realtime` and the cursor of the next (only while the page is full).
fn realtime_page(v: &Value) -> (Vec<Value>, Option<String>) {
    let rows = v["list"].as_array().cloned().unwrap_or_default();
    let next = v["nextPageCursor"].as_str().filter(|c| !c.is_empty() && rows.len() >= PAGE).map(String::from);
    (rows, next)
}

/// Every open order of a category (conditional / TP/SL rows included), all pages: 50 per request,
/// linear without a symbol is scoped by settleCoin. Snapshots only (resync, reconcile, own TP/SL adds).
// ponytail: stops at 20 pages (1000 rows); raise it if an account really rests more than that
pub(super) async fn realtime(k: &Keys, category: &str, symbol: Option<&str>) -> Result<Vec<Value>> {
    paged(20, |o| o["orderId"].to_string(), |cursor| async move {
        let mut q = json!({"category": category, "limit": PAGE.to_string()});
        match symbol { Some(s) => q["symbol"] = s.into(), None if category == "linear" => q["settleCoin"] = "USDT".into(), None => {} }
        if let Some(c) = cursor { q["cursor"] = c.into(); }
        Ok(realtime_page(&bybit(k, true, "/v5/order/realtime", q).await?))
    }).await
}

pub async fn option_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let ex = Exchange::Bybit;
    let rows = realtime(k, "option", None).await?;
    Ok(rows.iter().map(|o| OpenOrder {
        ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].as_str().unwrap_or_default().into(),
        side: if o["side"] == "Sell" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]),
        tif: o["timeInForce"].as_str().unwrap_or_default().into(), kind: o["orderType"].as_str().unwrap_or("Limit").to_lowercase(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: n(&o["createdTime"]) as i64, pos: None, client_id: None, trigger: None,
    }).collect())
}

pub async fn can_trade(k: &Keys) -> Result<Option<bool>> {
    // readOnly 1 = read-only key; linear perps need the ContractTrade permission
    let v = bybit(k, true, "/v5/user/query-api", json!({})).await?;
    let ro = v["readOnly"].as_i64() == Some(1);
    let perms = v["permissions"]["ContractTrade"].as_array().is_some_and(|a| !a.is_empty());
    Ok(Some(!ro && perms))
}

pub async fn amend(k: &Keys, o: &OpenOrder, px: String) -> Result<()> {
    bybit(k, false, "/v5/order/amend", json!({"category": "linear", "symbol": o.symbol, "orderId": o.id, "price": px})).await?;
    Ok(())
}

pub async fn cancel(k: &Keys, symbol: &str, id: &str) -> Result<()> {
    bybit(k, false, "/v5/order/cancel", json!({"category": "linear", "symbol": symbol, "orderId": id})).await?;
    Ok(())
}

/// `/v5/order/cancel-all` body: the whole USDT linear account, one order class: resting orders
/// (`Order`), or conditional orders except TP/SL and trailing stops (`StopOrder` + `Stop`).
pub(super) fn cancel_all_body(conditional: bool) -> Value {
    let mut b = json!({"category": "linear", "settleCoin": "USDT", "orderFilter": if conditional { "StopOrder" } else { "Order" }});
    if conditional { b["stopOrderType"] = "Stop".into(); }
    b
}

/// Order ids a cancel-all answer lists as cancelled.
fn cancelled_ids(v: &Value) -> Result<Vec<String>> {
    if v["success"] == "0" { bail!("not accepted (success 0)"); }
    Ok(v["list"].as_array().into_iter().flatten().filter_map(|o| o["orderId"].as_str().map(String::from)).collect())
}

/// Given rows the cancel-all calls did not report cancelled that still need a single cancel: trigger
/// rows (trailing stops, which no bulk filter reaches without taking TP/SL too), and every plain
/// row when the resting-order call failed (plain rows missing from a good answer already filled).
fn leftovers<'a>(orders: &'a [OpenOrder], done: &'a std::collections::HashSet<String>, plain_ok: bool) -> impl Iterator<Item = &'a OpenOrder> {
    orders.iter().filter(move |o| !done.contains(&o.id) && (o.trigger.is_some() || !plain_ok))
}

/// Every resting and conditional order of the USDT linear account, given or not (two cancel-all
/// calls), then `leftovers` one by one. TP/SL stay (tpsl::cancel_all): the venue's only bulk filter
/// for them also takes conditional entries and trailing stops. A call cancels at most 500 orders
/// (picked at random); hitting that cap is reported as a failure line.
pub async fn cancel_all(k: &Keys, orders: &[OpenOrder]) -> Result<CancelReport> {
    let (mut rep, mut done, mut plain_ok) = (CancelReport::default(), std::collections::HashSet::new(), false);
    for conditional in [false, true] {
        let what = if conditional { "conditional orders" } else { "resting orders" };
        match bybit(k, false, "/v5/order/cancel-all", cancel_all_body(conditional)).await.and_then(|v| cancelled_ids(&v)) {
            Ok(ids) => {
                if ids.len() >= 500 { rep.failed.push(format!("bybit cancel-all {what}: 500 cancelled, the most one call takes; more may still be open")); }
                rep.sent += ids.len();
                done.extend(ids);
                plain_ok |= !conditional;
            }
            Err(e) => rep.failed.push(format!("bybit cancel-all {what}: {e:#}")),
        }
    }
    for o in leftovers(orders, &done, plain_ok) { rep.add(format!("{} {}", o.symbol, o.id), cancel(k, &o.symbol, &o.id).await); }
    Ok(rep)
}

pub async fn positions(k: &Keys) -> Result<Vec<Position>> {
    let v = bybit(k, true, "/v5/position/list", json!({"category": "linear", "settleCoin": "USDT"})).await?;
    Ok(v["list"].as_array().into_iter().flatten().filter(|p| n(&p["size"]) > 0.0).map(bybit_position).collect())
}

pub async fn open_orders(k: &Keys) -> Result<Vec<OpenOrder>> {
    let rows = realtime(k, "linear", None).await?;
    // TP/SL rows live in the same list; they are reported by tpsl::list instead
    Ok(rows.iter().filter(|o| tpsl::bybit_row(o).is_none()).map(bybit_order).collect())
}

/// An order row found by orderLinkId: open while New / PartiallyFilled / Untriggered.
fn bybit_found(o: &Value) -> Found {
    if matches!(o["orderStatus"].as_str(), Some("New" | "PartiallyFilled" | "Untriggered")) { return Found::Open(bybit_order(o)); }
    Found::Done { id: o["orderId"].as_str().unwrap_or_default().into(), filled: crate::opt_num(&o["cumExecQty"]).unwrap_or(0.0), status: o["orderStatus"].as_str().unwrap_or_default().into() }
}

/// Our order by orderLinkId, one call each (50/s per UID): realtime answers for open and the last
/// 500 closed orders (openOnly does not apply to an orderLinkId query), then history (7 days).
pub async fn order_by_client_id(k: &Keys, symbol: &str, cid: &str) -> Result<Option<Found>> {
    for path in ["/v5/order/realtime", "/v5/order/history"] {
        let v = bybit(k, true, path, json!({"category": "linear", "symbol": symbol, "orderLinkId": cid})).await?;
        if let Some(o) = v["list"].as_array().and_then(|l| l.first()) { return Ok(Some(bybit_found(o))); }
    }
    Ok(None)
}

/// Unified account totals from a wallet row (REST `wallet-balance` list[0] and the `wallet` push share the fields).
fn balance_of(a: &Value) -> Balance {
    Balance { equity: n(&a["totalEquity"]), available: n(&a["totalAvailableBalance"]), uni_mmr: None, mm_rate: crate::opt_num(&a["accountMMRate"]),
        maint_margin: crate::opt_num(&a["totalMaintenanceMargin"]), adj_equity: crate::opt_num(&a["totalMarginBalance"]) }
}

pub async fn balance(k: &Keys) -> Result<Balance> {
    let v = bybit(k, true, "/v5/account/wallet-balance", json!({"accountType": "UNIFIED"})).await?;
    Ok(balance_of(&v["list"][0]))
}

/// One Bybit v5 position row (REST list and the private `position` push share the shape).
/// A closed hedge-mode row has side "" and size 0; positionIdx says which side it was.
fn bybit_position(p: &Value) -> Position {
    let side = match (p["side"].as_str(), p["positionIdx"].as_i64()) { (Some("Sell"), _) | (_, Some(2)) => Side::Sell, _ => Side::Buy };
    Position {
        ex: Exchange::Bybit, symbol: p["symbol"].as_str().unwrap_or_default().into(), side,
        qty: n(&p["size"]), entry: crate::opt_num(&p["avgPrice"]).or(crate::opt_num(&p["entryPrice"])).unwrap_or(0.0), mark: n(&p["markPrice"]),
        liq: crate::opt_num(&p["liqPrice"]).filter(|x| *x > 0.0), upnl: n(&p["unrealisedPnl"]), lev: n(&p["leverage"]), margin: n(&p["positionIM"]),
        cross: p["tradeMode"].as_i64().map(|m| m == 0),
    }
}

fn bybit_order(o: &Value) -> OpenOrder {
    OpenOrder {
        ex: Exchange::Bybit, symbol: o["symbol"].as_str().unwrap_or_default().into(), id: o["orderId"].as_str().unwrap_or_default().into(),
        side: if o["side"] == "Sell" { Side::Sell } else { Side::Buy }, price: n(&o["price"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]),
        tif: o["timeInForce"].as_str().unwrap_or_default().into(), kind: o["orderType"].as_str().unwrap_or_default().into(), reduce_only: o["reduceOnly"].as_bool().unwrap_or(false), ts: n(&o["createdTime"]) as i64,
        pos: match o["positionIdx"].as_i64() { Some(1) => Some(Side::Buy), Some(2) => Some(Side::Sell), _ => None },
        client_id: opt_id(&o["orderLinkId"]), trigger: crate::opt_num(&o["triggerPrice"]).filter(|x| *x > 0.0),
    }
}

pub async fn stream(k: Keys, tx: AccTx) {
    let ex = Exchange::Bybit;
    let url = if testnet() { "wss://stream-testnet.bybit.com/v5/private" } else { "wss://stream.bybit.com/v5/private" };
    loop {
        // auth: HMAC(secret, "GET/realtime" + expires), computed fresh for every connection
        let expires = crate::now_ms() + 10_000;
        let sig = hmac_hex(&k.secret, &format!("GET/realtime{expires}"));
        let spec = ws::Spec::new("bybit account", url)
            .sub(json!({"op": "auth", "args": [k.key, expires, sig]}).to_string())
            .sub(json!({"op": "subscribe", "args": ["position.linear", "order.linear", "execution.linear", "wallet"]}).to_string())
            // its own request: an account without options must not fail the main subscription
            .sub(json!({"op": "subscribe", "req_id": "opt", "args": ["position.option"]}).to_string())
            .sub(json!({"op": "subscribe", "req_id": "spot", "args": ["order.spot"]}).to_string())
            .ping(std::time::Duration::from_secs(20), r#"{"op":"ping"}"#);
        let (mut auth_failed, mut up) = (false, false);
        let r = ws::once(&spec, |f| {
            let ws::Frame::Text(s) = f else { return true };
            let Ok(v) = serde_json::from_str::<Value>(s) else { return true };
            let send = |e: AccEvent| { let _ = tx.send((ex, e)); };
            match (v["op"].as_str(), v["topic"].as_str()) {
                (Some("auth"), _) if v["success"] == false => { eprintln!("[bybit account] auth failed: {}", v["ret_msg"]); auth_failed = true; return false; }
                (Some("subscribe"), _) if v["success"] == true && v["req_id"] != "opt" && v["req_id"] != "spot" => { up = true; send(AccEvent::Resync) }
                (_, Some("order.spot")) => send(AccEvent::SpotDirty),
                (_, Some("position.option")) => for p in v["data"].as_array().into_iter().flatten().filter_map(bybit_opt_pos) { send(AccEvent::OptPos(p)); },
                (_, Some("position.linear")) => for p in v["data"].as_array().into_iter().flatten() {
                    send(AccEvent::Position { p: bybit_position(p), one_way: p["positionIdx"].as_i64() == Some(0) });
                },
                (_, Some("order.linear")) => for o in v["data"].as_array().into_iter().flatten() {
                    // TP/SL rows are kept apart from ordinary resting orders
                    if let Some((t, active)) = tpsl::bybit_row(o) {
                        send(if active { AccEvent::TpSl(t) } else { AccEvent::TpSlDone { id: o["orderId"].as_str().unwrap_or_default().into() } });
                        continue;
                    }
                    let open = matches!(o["orderStatus"].as_str(), Some("New" | "PartiallyFilled" | "Untriggered"));
                    send(if open { AccEvent::Order(bybit_order(o)) } else { AccEvent::OrderDone { id: o["orderId"].as_str().unwrap_or_default().into() } });
                },
                (_, Some("execution.linear")) => for f in v["data"].as_array().into_iter().flatten() {
                    if let Some(l) = bybit_liquidation(f) { send(l); }
                    if let Some(f) = bybit_fill(f) { send(AccEvent::Fill(f)); }
                },
                (_, Some("wallet")) => for w in v["data"].as_array().into_iter().flatten().filter(|w| w["accountType"] == "UNIFIED") {
                    send(AccEvent::Wallet(balance_of(w)));
                },
                _ => {}
            }
            true
        }).await;
        if let Err(e) = r { eprintln!("[bybit account] {e:#}"); }
        // once per lost session (a reconnect that never subscribes sends nothing)
        if up { let _ = tx.send((ex, AccEvent::Down)); }
        tokio::time::sleep(std::time::Duration::from_secs(if auth_failed { 60 } else { 2 })).await;
    }
}

/// One of our positions closed by the venue: a liquidation (BustTrade) or auto-deleveraging (AdlTrade)
/// execution, one event per execution. `side` is the position's (the execution closes it: the opposite side).
fn bybit_liquidation(f: &Value) -> Option<AccEvent> {
    if !matches!(f["execType"].as_str(), Some("BustTrade" | "AdlTrade")) { return None; }
    Some(AccEvent::Liquidated { symbol: f["symbol"].as_str()?.into(), side: if f["side"] == "Sell" { Side::Buy } else { Side::Sell },
        qty: crate::opt_num(&f["execQty"]).unwrap_or(0.0), price: crate::opt_num(&f["execPrice"]).unwrap_or(0.0) })
}

/// One Bybit execution (REST list or `execution.linear` push); funding / settlement rows are not fills.
fn bybit_fill(f: &Value) -> Option<Fill> {
    f["execType"].as_str().is_none_or(|t| t == "Trade").then(|| Fill {
        ex: Exchange::Bybit, id: f["execId"].as_str().unwrap_or_default().into(), symbol: f["symbol"].as_str().unwrap_or_default().into(),
        side: if f["side"] == "Sell" { Side::Sell } else { Side::Buy }, price: n(&f["execPrice"]), qty: n(&f["execQty"]),
        fee: n(&f["execFee"]), realized: crate::opt_num(&f["execPnl"]), ts: n(&f["execTime"]) as i64,
    })
}

pub async fn fills_window(k: &Keys, symbol: &str, t0: i64, t1: i64, out: &mut Vec<Fill>) -> Result<()> {
    let mut cursor = String::new();
    loop {
        let mut q = json!({"category": "linear", "symbol": symbol, "startTime": t0.to_string(), "endTime": t1.to_string(), "limit": "100"});
        if !cursor.is_empty() { q["cursor"] = cursor.clone().into(); }
        let e = bybit(k, true, "/v5/execution/list", q).await?;
        out.extend(e["list"].as_array().into_iter().flatten().filter_map(bybit_fill));
        cursor = e["nextPageCursor"].as_str().unwrap_or_default().into();
        if cursor.is_empty() || e["list"].as_array().is_none_or(|l| l.is_empty()) { break; }
    }
    Ok(())
}

pub async fn history(k: &Keys, side: impl Fn(&Value) -> Side, h: &mut History) -> Result<()> {
    let ex = Exchange::Bybit;
    let o = bybit(k, true, "/v5/order/history", json!({"category": "linear", "settleCoin": "USDT", "limit": "50"})).await?;
    h.orders = o["list"].as_array().into_iter().flatten().map(|o| HistOrder {
        ex, symbol: o["symbol"].as_str().unwrap_or_default().into(), side: side(&o["side"]), kind: o["orderType"].as_str().unwrap_or_default().into(),
        price: n(&o["price"]), avg: n(&o["avgPrice"]), qty: n(&o["qty"]), filled: n(&o["cumExecQty"]), status: o["orderStatus"].as_str().unwrap_or_default().into(), ts: n(&o["createdTime"]) as i64,
    }).collect();
    let e = bybit(k, true, "/v5/execution/list", json!({"category": "linear", "settleCoin": "USDT", "limit": "100"})).await?;
    h.fills = e["list"].as_array().into_iter().flatten().filter_map(bybit_fill).collect();
    let c = bybit(k, true, "/v5/position/closed-pnl", json!({"category": "linear", "limit": "100"})).await?;
    // the row's side is the closing order's side: Sell closed a long
    h.closed = c["list"].as_array().into_iter().flatten().map(|c| ClosedPnl {
        ex, symbol: c["symbol"].as_str().unwrap_or_default().into(), long: Some(side(&c["side"]) == Side::Sell), qty: crate::opt_num(&c["closedSize"]),
        entry: crate::opt_num(&c["avgEntryPrice"]), exit: crate::opt_num(&c["avgExitPrice"]), pnl: n(&c["closedPnl"]), ts: n(&c["updatedTime"]) as i64,
    }).collect();
    Ok(())
}

/// Bybit execution rows (newest first) as signed fills of the `side` position. Rows carry no
/// positionIdx, so closedSize splits each fill: a same-side fill adds execQty - closedSize, an
/// opposite one removes closedSize. That holds in hedge mode (a fill only opens or only closes, the
/// other side's fills count 0) and one-way (a flip closes closedSize, opens the rest).
pub(super) fn bybit_fills(rows: &[Value], side: Side) -> Vec<(i64, f64)> {
    rows.iter().filter(|f| !matches!(f["execType"].as_str(), Some("Funding" | "Settle"))).map(|f| {
        let (q, closed) = (n(&f["execQty"]), n(&f["closedSize"]));
        let same = (f["side"] == "Sell") == (side == Side::Sell);
        let delta = if same { q - closed } else { -closed };
        (n(&f["execTime"]) as i64, if side == Side::Buy { delta } else { -delta })
    }).collect()
}

pub async fn funding_paid(k: &Keys, p: &Position, known: Option<i64>, cur: f64, now: i64) -> Result<Option<FundingPaid>> {
    let mut fills = vec![];
    let mut found = known;
    'w: for w in 0..if known.is_some() { 0 } else { 4 } {
        let end = now - w * WEEK_MS;
        let mut cursor = String::new();
        for _ in 0..10 {
            let mut q = json!({"category": "linear", "symbol": p.symbol, "startTime": (end - WEEK_MS).to_string(), "endTime": end.to_string(), "limit": "100"});
            if !cursor.is_empty() { q["cursor"] = cursor.clone().into(); }
            let v = bybit(k, true, "/v5/execution/list", q).await?;
            let rows = v["list"].as_array().cloned().unwrap_or_default();
            fills.extend(bybit_fills(&rows, p.side));
            if let Some((t, false)) = open_time(cur, &fills) { found = Some(t); break 'w; }
            cursor = v["nextPageCursor"].as_str().unwrap_or_default().into();
            if cursor.is_empty() || rows.is_empty() { break; }
        }
    }
    let (since, lower) = match found { Some(t) => (t, false), None => (fills.last().map_or(now - 4 * WEEK_MS, |f| f.0), true) };
    let mut amount = 0.0;
    let mut start = since;
    while start < now {
        let end = (start + WEEK_MS).min(now);
        let mut cursor = String::new();
        for _ in 0..20 {
            let mut q = json!({"accountType": "UNIFIED", "category": "linear", "type": "SETTLEMENT", "startTime": start.to_string(), "endTime": end.to_string(), "limit": "50"});
            if let Some(b) = p.symbol.strip_suffix("USDT") { q["baseCoin"] = b.into(); }
            if !cursor.is_empty() { q["cursor"] = cursor.clone().into(); }
            let v = bybit(k, true, "/v5/account/transaction-log", q).await?;
            let rows = v["list"].as_array().cloned().unwrap_or_default();
            // `change` is the wallet movement (positive = received) whatever sign convention `funding`
            // uses; settlement rows carry no cashFlow or fee. Assumption: `side` is the position's
            // side (Buy = long), so hedge mode keeps its own side; rows without a side count.
            amount += rows.iter().filter(|r| r["symbol"] == p.symbol.as_str())
                .filter(|r| match r["side"].as_str() { Some("Buy") => p.side == Side::Buy, Some("Sell") => p.side == Side::Sell, _ => true })
                .map(|r| n(&r["change"])).sum::<f64>();
            cursor = v["nextPageCursor"].as_str().unwrap_or_default().into();
            if cursor.is_empty() || rows.is_empty() { break; }
        }
        start = end;
    }
    Ok(Some(FundingPaid { amount, since, lower }))
}

pub async fn wallets(k: &Keys, add: &mut impl FnMut(&str, Result<Vec<Coin>>, &std::collections::HashMap<String, f64>)) {
    let px = bybit_usd_prices().await.unwrap_or_default();
    // UNIFIED: holdings from wallet-balance, then transferable amounts 10 coins per query
    // (the coin-balance endpoint refuses to list a unified account wholesale)
    let r: Result<Vec<Coin>> = async {
        let v = bybit(k, true, "/v5/account/wallet-balance", json!({"accountType": "UNIFIED"})).await?;
        let mut cs: Vec<Coin> = v["list"][0]["coin"].as_array().into_iter().flatten().map(|c| Coin {
            coin: c["coin"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), ..Default::default()
        }).filter(|c| c.qty > 0.0).collect();
        let names: Vec<String> = cs.iter().map(|c| c.coin.clone()).collect();
        for chunk in names.chunks(10) {
            let v = bybit(k, true, "/v5/asset/transfer/query-account-coins-balance", json!({"accountType": "UNIFIED", "coin": chunk.join(",")})).await?;
            for b in v["balance"].as_array().into_iter().flatten() {
                if let Some(c) = cs.iter_mut().find(|c| b["coin"] == c.coin.as_str()) { c.free = n(&b["transferBalance"]); }
            }
        }
        Ok(cs)
    }.await;
    add("UNIFIED", r, &px);
    let r = bybit(k, true, "/v5/asset/transfer/query-account-coins-balance", json!({"accountType": "FUND"})).await.map(|v| {
        v["balance"].as_array().into_iter().flatten().map(|c| Coin {
            coin: c["coin"].as_str().unwrap_or_default().into(), qty: n(&c["walletBalance"]), free: n(&c["transferBalance"]), ..Default::default()
        }).collect()
    });
    add("FUND", r, &px);
    let mut earn: Result<Vec<Coin>> = Ok(vec![]);
    for cat in ["FlexibleSaving", "OnChain"] {
        match bybit(k, true, "/v5/earn/position", json!({"category": cat})).await {
            Ok(v) => if let Ok(list) = earn.as_mut() {
                for p in v["list"].as_array().into_iter().flatten() {
                    // on-chain redemptions take days and need a position id: shown, not redeemable here
                    let flex = cat == "FlexibleSaving";
                    let amt = n(&p["amount"]);
                    list.push(Coin { coin: p["coin"].as_str().unwrap_or_default().into(), qty: amt, free: if flex { amt } else { 0.0 },
                        product: flex.then(|| p["productId"].as_str().unwrap_or_default().to_string()), ..Default::default() });
                }
            },
            Err(e) => { earn = Err(e); break; }
        }
    }
    add("EARN", earn, &px);
}

pub async fn transfer(k: &Keys, from: &str, to: &str, coin: &str, amt: String, product: Option<&str>, log: &mut Vec<String>) -> Result<()> {
    if from == "EARN" {
        let pid = product.ok_or_else(|| anyhow!("not redeemable here (on-chain or unknown product)"))?;
        let link = format!("t1-{}", crate::now_ms());
        let v = bybit(k, false, "/v5/earn/place-order", json!({"category": "FlexibleSaving", "orderType": "Redeem", "accountType": to,
            "amount": amt, "coin": coin, "productId": pid, "orderLinkId": link})).await?;
        log.push(format!("Bybit redeem {amt} {coin} -> {to}: order {}", v["orderId"].as_str().unwrap_or("?")));
    } else {
        let id = uuid_v4();
        let v = bybit(k, false, "/v5/asset/transfer/inter-transfer", json!({"transferId": id, "coin": coin, "amount": amt,
            "fromAccountType": from, "toAccountType": to})).await?;
        let st = v["status"].as_str().unwrap_or("?");
        if st == "FAILED" { bail!("Bybit transfer {from} -> {to} {amt} {coin}: FAILED"); }
        log.push(format!("Bybit {from} -> {to} {amt} {coin}: {st}"));
    }
    Ok(())
}

/// Spot last prices in USDT per coin from Bybit, for valuing non-margin accounts.
async fn bybit_usd_prices() -> Result<std::collections::HashMap<String, f64>> {
    let v = ws::get_json(&format!("{}/v5/market/tickers?category=spot", bybit_url())).await?;
    Ok(v["result"]["list"].as_array().into_iter().flatten().filter_map(|t| {
        let coin = t["symbol"].as_str()?.strip_suffix("USDT")?;
        Some((coin.to_string(), n(&t["lastPrice"])))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bybit_option_symbol() {
        assert_eq!(bybit_opt_parts("HYPE-30OCT26-72-P-USDT"), Some(("HYPE".into(), "30OCT26".into(), 72.0, false)));
        assert_eq!(bybit_opt_parts("BTC-25DEC26-105000-C"), Some(("BTC".into(), "25DEC26".into(), 105000.0, true)));
        assert_eq!(bybit_opt_parts("BTCUSDT"), None);
    }

    #[test]
    fn bybit_signature_layout() {
        // reference computed independently with Python's hmac module
        let k = Keys { key: "XXXXKEY".into(), secret: "YYYYSECRET".into(), extra: None, demo: false };
        assert_eq!(sign(&k, 1_700_000_000_000, 5000, r#"{"category":"linear","symbol":"BTCUSDT"}"#),
                   "136ba24b5260b0829ea5217d7be22cff536e12d72edc8f9e72238c026fbaee4f");
    }

    #[test]
    fn realtime_pages_follow_the_cursor() {
        use futures_util::FutureExt;
        let rows = |a: usize, b: usize| (a..b).map(|i| json!({"orderId": i.to_string()})).collect::<Vec<_>>();
        // 120 open orders: two full pages with cursors, then a short page that ends the walk even though it names a cursor
        let server = |c: Option<String>| match c.as_deref() {
            None => json!({"list": rows(0, 50), "nextPageCursor": "page_token%3D1%26"}),
            Some("page_token%3D1%26") => json!({"list": rows(50, 100), "nextPageCursor": "page_token%3D2%26"}),
            Some("page_token%3D2%26") => json!({"list": rows(100, 120), "nextPageCursor": "page_token%3D3%26"}),
            Some(c) => panic!("unexpected cursor {c}"),
        };
        let all = paged(20, |o| o["orderId"].to_string(), |c| async move { Ok(realtime_page(&server(c))) }).now_or_never().unwrap().unwrap();
        assert_eq!(all.len(), 120);
        assert_eq!((all[0]["orderId"].as_str(), all[119]["orderId"].as_str()), (Some("0"), Some("119")));
        // a full page with an empty cursor is the last one; a missing list is an empty page
        assert_eq!(realtime_page(&json!({"list": rows(0, 50), "nextPageCursor": ""})).1, None);
        assert_eq!(realtime_page(&json!({})), (vec![], None));
    }

    #[test]
    fn cancel_all_scope_and_leftovers() {
        assert_eq!(cancel_all_body(false), json!({"category": "linear", "settleCoin": "USDT", "orderFilter": "Order"}));
        assert_eq!(cancel_all_body(true), json!({"category": "linear", "settleCoin": "USDT", "orderFilter": "StopOrder", "stopOrderType": "Stop"}));
        // documented answer shape; success "0" is a failure
        let ids = cancelled_ids(&json!({"list": [{"orderId": "1616024329462743808", "orderLinkId": "1616024329462743809"}, {"orderId": "b", "orderLinkId": ""}], "success": "1"})).unwrap();
        assert_eq!(ids, ["1616024329462743808", "b"]);
        assert!(cancelled_ids(&json!({"list": [], "success": "0"})).is_err());
        assert_eq!(cancelled_ids(&json!({"list": []})).unwrap(), Vec::<String>::new());
        let row = |id: &str, trigger: Option<f64>| OpenOrder { ex: Exchange::Bybit, symbol: "BTCUSDT".into(), id: id.into(), side: Side::Sell, price: 0.0, qty: 1.0, filled: 0.0,
            kind: "Market".into(), tif: String::new(), reduce_only: true, ts: 0, pos: None, client_id: None, trigger };
        let orders = [row("plain", None), row("cond", Some(90.0)), row("trail", Some(95.0)), row("filled", None)];
        let done: std::collections::HashSet<String> = ["plain".to_string(), "cond".to_string()].into();
        let ids = |plain_ok| leftovers(&orders, &done, plain_ok).map(|o| o.id.as_str()).collect::<Vec<_>>();
        // the trailing stop no bulk call reached goes one by one; a plain row missing from a good answer is gone already
        assert_eq!(ids(true), ["trail"]);
        // the resting-order call failed: its plain rows go one by one too
        assert_eq!(ids(false), ["trail", "filled"]);
    }

    #[test]
    fn liquidation_executions() {
        // execution.linear push row (docs fields) of a liquidated long: not a fill, one Liquidated for the long
        let bust = json!({"category": "linear", "symbol": "BTCUSDT", "execFee": "0.3", "execId": "7e2ae69c", "execPrice": "84000.5", "execQty": "0.25",
            "execType": "BustTrade", "execValue": "21000", "isMaker": false, "orderId": "1b4e", "orderLinkId": "", "orderPrice": "84000.5", "orderQty": "0.25",
            "orderType": "Market", "side": "Sell", "execTime": "1672364174443", "closedSize": "0.25", "execPnl": "-900"});
        assert!(bybit_fill(&bust).is_none());
        assert!(matches!(bybit_liquidation(&bust), Some(AccEvent::Liquidated { ref symbol, side: Side::Buy, qty, price }) if symbol == "BTCUSDT" && qty == 0.25 && price == 84000.5));
        // auto-deleveraged short (the closing execution buys)
        let adl = json!({"symbol": "ETHUSDT", "execType": "AdlTrade", "side": "Buy", "execQty": "1", "execPrice": "2500"});
        assert!(matches!(bybit_liquidation(&adl), Some(AccEvent::Liquidated { side: Side::Sell, .. })));
        // ordinary trades and funding rows are not liquidations
        for t in ["Trade", "Funding"] {
            let mut r = bust.clone();
            r["execType"] = t.into();
            assert!(bybit_liquidation(&r).is_none());
        }
    }

    #[test]
    fn found_by_order_link_id() {
        // /v5/order/realtime rows (docs fields): open while New, done with its status and fill otherwise
        let row = json!({"orderId": "fd4300ae", "orderLinkId": "t1a2l0", "symbol": "ETHUSDT", "price": "1600.00", "qty": "0.10", "side": "Buy", "positionIdx": 1,
            "orderStatus": "New", "orderType": "Limit", "timeInForce": "GTC", "cumExecQty": "0.00", "reduceOnly": false, "triggerPrice": "", "createdTime": "1684738540559"});
        let Found::Open(o) = bybit_found(&row) else { panic!("open") };
        assert_eq!((o.id.as_str(), o.client_id.as_deref(), o.pos, o.price, o.trigger), ("fd4300ae", Some("t1a2l0"), Some(Side::Buy), 1600.0, None));
        let mut done = row.clone();
        done["orderStatus"] = "PartiallyFilledCanceled".into();
        done["cumExecQty"] = "0.04".into();
        let Found::Done { id, filled, status } = bybit_found(&done) else { panic!("done") };
        assert_eq!((id.as_str(), filled, status.as_str()), ("fd4300ae", 0.04, "PartiallyFilledCanceled"));
        let mut cond = row.clone();
        cond["orderStatus"] = "Untriggered".into();
        assert!(matches!(bybit_found(&cond), Found::Open(_)));
    }

    #[test]
    fn wallet_push_carries_risk_fields() {
        // `wallet` topic row (docs shape), account with a position: the risk gauge fields ride along
        let push: Value = serde_json::from_str(r#"{"accountType":"UNIFIED","accountIMRate":"0.1","accountMMRate":"0.05","totalEquity":"10262.91","totalWalletBalance":"10300.5",
            "totalMarginBalance":"10262.5","totalAvailableBalance":"9000.25","totalPerpUPL":"-37.9","totalInitialMargin":"1000","totalMaintenanceMargin":"513.1","coin":[]}"#).unwrap();
        let b = balance_of(&push);
        assert_eq!((b.equity, b.available, b.mm_rate, b.maint_margin, b.adj_equity, b.uni_mmr), (10262.91, 9000.25, Some(0.05), Some(513.1), Some(10262.5), None));
        // fields the push leaves out stay None (the engine keeps the last value)
        let b = balance_of(&json!({"accountType": "UNIFIED", "totalEquity": "1", "totalAvailableBalance": "1", "accountMMRate": ""}));
        assert_eq!((b.mm_rate, b.maint_margin, b.adj_equity), (None, None, None));
    }
}
