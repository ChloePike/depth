//! Bridge for the native (SwiftUI) panels: one JSON state snapshot the host polls, and JSON
//! actions it sends. egui keeps drawing the chart, book and option chain; everything else
//! (title bar, order entry, account tables, settings, status bar) is SwiftUI.
use super::App;
use eframe::egui;
use serde_json::{json, Value};
use std::time::Instant;
use terminal_one::trade::{self, Kind, OrderReq, Tif};
use terminal_one::{now_ms, route, Exchange, Market, Side};

fn ex_name(e: Exchange) -> String { format!("{e:?}") }
fn parse_ex(v: &Value) -> Option<Exchange> { Exchange::parse(v.as_str()?) }
fn side_name(s: Side) -> &'static str { if s == Side::Buy { "long" } else { "short" } }

impl App {
    /// Everything the native panels show, as one JSON document (polled ~5x a second).
    pub fn state_json(&mut self) -> String {
        let a = self.eng.agg.lock().unwrap();
        let st = self.eng.stats.lock().unwrap();
        let book_market = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
        let now = Instant::now();
        if now.duration_since(self.rate.0).as_secs_f64() >= 1.0 {
            self.rate.2 = st.total.saturating_sub(self.rate.1) as f64 / now.duration_since(self.rate.0).as_secs_f64();
            self.rate = (now, st.total, self.rate.2);
        }
        let (oi, oi_usd) = a.oi_total();
        let basis = {
            let mut v: Vec<f64> = a.venues.keys().filter(|(_, m)| *m == Market::Perp).filter_map(|(e, _)| a.basis_bps(*e)).collect();
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied()
        };
        let nowms = now_ms();
        let venues: Vec<Value> = Exchange::ALL.iter().map(|&e| {
            let last = st.venue.iter().filter(|((x, _), _)| *x == e).map(|(_, v)| v.1).max();
            json!({"ex": ex_name(e), "on": !self.eng.off.contains(&e), "alive": last.is_some_and(|l| nowms - l < 5_000), "lat_ms": st.latency(e)})
        }).collect();

        let ex = self.trade.ex;
        let symbol = trade::symbol(&self.eng.base);
        let bbo = a.venues.get(&(ex, Market::Perp)).and_then(|v| match (v.book.best_bid(), v.book.best_ask()) {
            (Some(b), Some(k)) => Some((b.0, k.0)),
            _ => v.bbo.map(|[b, _, k, _]| (b, k)),
        });
        // the order venue's own perp: mark, index, funding (Binance-style header), plus 24h stats of the base
        let tv = a.venues.get(&(ex, Market::Perp));
        let fund = self.eng.funding.lock().unwrap().get(&(ex, symbol.clone())).copied();
        let tk = self.eng.tickers.lock().unwrap().iter().find(|t| t.base == self.eng.base).cloned();
        let header = json!({
            "price": a.mid(book_market), "oi": oi, "oi_usd": oi_usd,
            "venue": ex_name(ex), "mark": tv.and_then(|v| v.mark), "index": tv.and_then(|v| v.index),
            // live predicted rate per settlement when the interval is known, else per hour
            "funding_rate": tv.and_then(|v| v.funding_h).map(|h| h * fund.map_or(1.0, |f| f.interval_h)).or(fund.map(|f| f.rate)),
            "funding_interval_h": fund.map(|f| f.interval_h), "next_funding_ms": tv.and_then(|v| v.next_funding_ms).or(fund.map(|f| f.next_ms)),
            "high24": tk.as_ref().map(|t| t.high).filter(|x| *x > 0.0), "low24": tk.as_ref().map(|t| t.low).filter(|x| *x > 0.0),
            "vol24_base": tk.as_ref().map(|t| t.base_vol), "vol24_usd": tk.as_ref().map(|t| t.quote_vol), "chg24": tk.as_ref().map(|t| t.chg_pct),
            "funding_pred_bph": a.funding_oi_weighted().map(|f| f * 1e4), "funding_settled_bph": a.funding_settled_oi_weighted().map(|f| f * 1e4),
            "basis_bps": basis, "cvd": a.cvd_total(book_market),
        });
        let stats = json!({
            "msgs_per_s": self.rate.2, "mem_mb": st.rss_mb, "backlog": st.backlog,
            "history": [st.hist_total.saturating_sub(st.hist_pending), st.hist_total], "errors": st.hist_errors.len(), "hist_errors": st.hist_errors,
        });
        let push_ms = st.latency(ex);
        drop(st);
        drop(a);

        let acc = self.eng.account.lock().unwrap();
        let has_key = acc.keys.contains_key(&ex);
        let key = (ex, symbol.clone());
        let mode = acc.modes.get(&key).copied().flatten();
        let rules = acc.rules.get(&key).copied();
        let bal = acc.balances.get(&ex).cloned();
        let mode_s = mode.map(|m| if m == trade::Mode::Hedge { "hedge" } else { "oneway" });
        let (fee_t, fee_m) = super::settings::fee(ex);
        let trade_v = json!({
            "venue": ex_name(ex), "symbol": symbol, "has_key": has_key, "verified": trade::VERIFIED.contains(&ex),
            "mode": mode_s, "lev": acc.levs.get(&key),
            "available": bal.as_ref().map(|b| b.available), "equity": bal.as_ref().map(|b| b.equity),
            "uni_mmr": bal.as_ref().and_then(|b| b.uni_mmr), "mm_rate": bal.as_ref().and_then(|b| b.mm_rate),
            "bid": bbo.map(|b| b.0), "ask": bbo.map(|b| b.1),
            "tick": rules.map(|r| r.tick), "step": rules.map(|r| r.step), "min_qty": rules.map(|r| r.min_qty), "min_notional": rules.map(|r| r.min_notional),
            "bbo_levels": trade::bbo_levels(ex), "caps": trade::caps(ex), "native_twap": trade::native_twap(ex), "push_ms": push_ms, "rtt_ms": acc.order_rtt.get(&ex),
            "fee": {"taker": fee_t, "maker": fee_m},
            "smart": self.route.smart, "error": acc.errors.get(&ex),
        });
        let funds = self.eng.funding.lock().unwrap().clone();
        let positions: Vec<Value> = acc.positions.iter().map(|p| {
            let f = funds.get(&(p.ex, p.symbol.clone()));
            // a positive rate: longs pay shorts
            let pay = f.map(|f| -(if p.side == Side::Buy { 1.0 } else { -1.0 }) * p.qty * p.mark * f.rate);
            json!({
                "ex": ex_name(p.ex), "symbol": p.symbol, "side": side_name(p.side), "qty": p.qty, "entry": p.entry, "mark": p.mark,
                "liq": p.liq, "upnl": p.upnl, "lev": p.lev, "margin": p.margin, "cross": p.cross,
                "funding_rate": f.map(|f| f.rate), "funding_interval_h": f.map(|f| f.interval_h), "next_funding_ms": f.map(|f| f.next_ms), "funding_est": pay,
            })
        }).collect();
        let orders: Vec<Value> = acc.orders.iter().map(|o| json!({
            "ex": ex_name(o.ex), "symbol": o.symbol, "id": o.id, "side": if o.side == Side::Buy { "buy" } else { "sell" }, "price": o.price, "qty": o.qty,
            "filled": o.filled, "kind": o.kind, "reduce_only": o.reduce_only, "ts": o.ts, "pos": o.pos.map(side_name),
        })).collect();
        let tpsl: Vec<Value> = acc.tpsl.iter().map(|t| json!({
            "ex": ex_name(t.ex), "id": t.id, "symbol": t.symbol, "pos": side_name(t.pos), "take_profit": t.take_profit, "trigger_px": t.trigger_px,
            "qty": t.qty, "trigger": if matches!(t.trigger, trade::tpsl::Trigger::Last) { "last" } else { "mark" },
        })).collect();
        let mut hist = (vec![], vec![], vec![], i64::MAX);
        for (_, (h, ts)) in acc.history.iter() {
            hist.0.extend(h.orders.iter().map(|o| json!({"ex": ex_name(o.ex), "symbol": o.symbol, "side": if o.side == Side::Buy { "buy" } else { "sell" }, "kind": o.kind,
                "price": o.price, "avg": o.avg, "qty": o.qty, "filled": o.filled, "status": o.status, "ts": o.ts})));
            hist.1.extend(h.fills.iter().map(|f| json!({"ex": ex_name(f.ex), "symbol": f.symbol, "side": if f.side == Side::Buy { "buy" } else { "sell" },
                "price": f.price, "qty": f.qty, "fee": f.fee, "realized": f.realized, "ts": f.ts})));
            hist.2.extend(h.closed.iter().map(|c| json!({"ex": ex_name(c.ex), "symbol": c.symbol, "long": c.long, "qty": c.qty, "entry": c.entry, "exit": c.exit, "pnl": c.pnl, "ts": c.ts})));
            hist.3 = hist.3.min(*ts);
        }
        let by_ts = |v: &mut Vec<Value>| v.sort_by(|a, b| b["ts"].as_i64().cmp(&a["ts"].as_i64()));
        by_ts(&mut hist.0);
        by_ts(&mut hist.1);
        by_ts(&mut hist.2);
        let wallets: Vec<Value> = acc.keys.keys().map(|e| {
            let w = acc.wallets.get(e);
            json!({
                "ex": ex_name(*e), "loading": acc.wallets_loading.contains(e), "updated_ms": w.map(|w| w.2),
                "notes": w.map(|w| w.1.clone()).unwrap_or_default(),
                "accounts": w.map(|w| w.0.iter().map(|x| json!({"id": x.id, "usd": x.usd,
                    "coins": x.coins.iter().map(|c| json!({"coin": c.coin, "qty": c.qty, "free": c.free, "usd": c.usd, "product": c.product})).collect::<Vec<_>>()})).collect::<Vec<_>>()).unwrap_or_default(),
                "auto": acc.auto.get(e).map(|r| json!({"enabled": r.enabled, "min": r.min, "target": r.target})),
            })
        }).collect();
        let mut balances: Vec<Value> = acc.balances.iter().map(|(e, b)| json!({
            "ex": ex_name(*e), "equity": b.equity, "available": b.available, "uni_mmr": b.uni_mmr, "mm_rate": b.mm_rate, "maint_margin": b.maint_margin, "adj_equity": b.adj_equity,
        })).collect();
        balances.sort_by(|a, b| a["ex"].as_str().cmp(&b["ex"].as_str()));
        let algos: Vec<Value> = acc.algos.iter().rev().map(|j| json!({
            "id": j.id, "ex": ex_name(j.ex), "symbol": j.symbol, "buy": j.buy, "close": j.close, "total": j.total, "sent": j.sent,
            "slices": j.slices, "done": j.done, "started_ms": j.started_ms, "end_ms": j.end_ms, "status": j.status, "cancelled": j.cancelled,
            "native": j.venue_id.is_some(),
        })).collect();
        let log: Vec<Value> = acc.log.iter().rev().take(200).map(|(ts, m, ok)| json!({"ts": ts, "msg": m, "ok": ok})).collect();
        let tests = self.eng.key_tests.lock().unwrap().clone();
        let keys: Vec<Value> = trade::KEY_VENUES.iter().map(|&e| {
            let tail: Option<String> = acc.keys.get(&e).map(|k| k.key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect());
            let (lk, ls, lx) = trade::key_fields(e);
            json!({"ex": ex_name(e), "configured": tail.is_some(), "tail": tail, "verified": trade::VERIFIED.contains(&e),
                   "test": tests.get(&e).map(|(ok, m)| json!({"ok": ok, "msg": m})), "labels": [lk, ls, lx]})
        }).collect();
        let pm = trade::is_pm();
        let transferring = acc.transferring;
        drop(acc);
        if has_key && mode.is_none() { self.eng.detect_mode(ex, &symbol); }

        // quant: signals feed and the models over the aggregated perp series
        let signals: Vec<Value> = self.eng.signals.lock().unwrap().items.iter().take(100).map(|g| json!({
            "ts": g.ts, "kind": g.kind, "severity": g.severity, "dir": g.dir, "title": g.title, "detail": g.detail,
        })).collect();
        let quant = {
            let ag = self.eng.agg.lock().unwrap();
            let m = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
            let bars: Vec<terminal_one::agg::Bar> = ag.series.get(&(None, m)).map(|s| s.bars.iter().cloned().collect()).unwrap_or_default();
            let vm = terminal_one::quant::vol_model(&bars);
            let mid = ag.mid(m).unwrap_or(0.0);
            let (b, k) = ag.book_where(m, terminal_one::agg::nice(mid.max(1e-9) * 1e-4), 0.005, true, |_| true);
            let usd = |ls: &[terminal_one::agg::Level]| ls.iter().map(|l| l.qty * l.px).sum::<f64>();
            let carry: Vec<Value> = ag.venues.iter().filter(|((_, vm2), _)| *vm2 == Market::Perp).map(|((e, _), v)| json!({
                "ex": ex_name(*e), "funding_apr": v.funding_h.map(|f| f * 24.0 * 365.0 * 100.0), "basis_bps": ag.basis_bps(*e),
            })).collect();
            json!({
                "vol": vm.map(|v| json!({"sigma_1h": v.sigma_1h, "sigma_24h": v.sigma_24h, "percentile": v.percentile,
                    "range_1h": [mid * (1.0 - v.sigma_1h), mid * (1.0 + v.sigma_1h)], "range_24h": [mid * (1.0 - v.sigma_24h), mid * (1.0 + v.sigma_24h)]})),
                "pressure": terminal_one::quant::pressure(usd(&b), usd(&k), &bars, 5),
                "book_bid_usd": usd(&b), "book_ask_usd": usd(&k), "carry": carry,
            })
        };
        let (chart, book) = {
            let ag = self.eng.agg.lock().unwrap();
            let mut b = self.book.native_json(&ag, self.mode);
            b["width"] = self.book_w.into();
            (self.chart.native_json(&ag, self.mode, &self.eng.base), b)
        };
        json!({
            "base": self.eng.base, "mode": format!("{:?}", self.mode), "lang_zh": super::is_zh(),
            "header": header, "venues": venues, "stats": stats, "trade": trade_v,
            "positions": positions, "orders": orders, "tpsl": tpsl,
            "history": {"orders": hist.0, "fills": hist.1, "closed": hist.2, "updated_ms": (hist.3 != i64::MAX).then_some(hist.3),
                        "loading": !self.eng.account.lock().unwrap().history_loading.is_empty()},
            "wallets": wallets, "balances": balances, "algos": algos, "transferring": transferring, "binance_pm": pm, "log": log, "keys": keys,
            "prefs": serde_json::to_value(&self.prefs).unwrap_or(Value::Null),
            "route": serde_json::to_value(&self.route).unwrap_or(Value::Null),
            "chart": chart, "book": book, "signals": signals, "quant": quant,
            "book_click": self.trade.fill.map(|(e, px)| json!({"ex": ex_name(e), "px": px})),
            "tradable": trade::TRADABLE.iter().map(|e| ex_name(*e)).collect::<Vec<_>>(),
        }).to_string()
    }

    /// One action from the native panels; returns {"ok": true, ...} or {"ok": false, "error": ...}.
    pub fn call_json(&mut self, ctx: &egui::Context, req: &str) -> String {
        let r = serde_json::from_str::<Value>(req).map_err(|e| e.to_string()).and_then(|v| self.call(ctx, &v));
        match r { Ok(v) => { let mut v = v; v["ok"] = true.into(); v.to_string() } Err(e) => json!({"ok": false, "error": e}).to_string() }
    }

    fn call(&mut self, ctx: &egui::Context, v: &Value) -> Result<Value, String> {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let f = |k: &str| v[k].as_f64();
        let exv = || parse_ex(&v["ex"]).ok_or("unknown venue");
        match v["op"].as_str().unwrap_or_default() {
            "set_base" => { let b = s("base").to_uppercase(); if !b.is_empty() && b != self.eng.base { self.pending_base = Some(b); } }
            "set_mode" => { if let Some(m) = Market::parse(&s("mode")) { self.pending_mode = Some(m); } }
            "toggle_venue" => {
                let e = exv()?;
                if !self.eng.off.remove(&e) { if self.eng.off.len() + 1 >= Exchange::ALL.len() { return Err("keep at least one venue".into()); } self.eng.off.insert(e); }
                self.pending_restart = true;
            }
            "set_trade_venue" => { let e = exv()?; if !trade::TRADABLE.contains(&e) { return Err("not tradable".into()); } self.trade.ex = e; self.route.fixed = e; }
            "set_lang" => super::set_zh(v["zh"].as_bool().unwrap_or(false)),
            "set_prefs" => { self.prefs = serde_json::from_value(v["prefs"].clone()).map_err(|e| e.to_string())?; self.prefs.apply(ctx); }
            "set_route" => { self.route = serde_json::from_value(v["route"].clone()).map_err(|e| e.to_string())?; }
            "tickers" => {
                let t = self.eng.tickers.lock().unwrap();
                return Ok(json!({"tickers": t.iter().map(|x| json!({"base": x.base, "last": x.last, "chg_pct": x.chg_pct, "quote_vol": x.quote_vol})).collect::<Vec<_>>()}));
            }
            "preview" | "place" => {
                let req = self.order_req(v)?;
                let plan = self.plan(&req)?;
                if v["op"] == "place" {
                    // the user confirmed a previewed plan: refuse if the venues or sizes moved since
                    if let Some(exp) = v["expect"].as_array() {
                        let same = exp.len() == plan.legs.len() && exp.iter().zip(&plan.legs).all(|(e, l)|
                            e["ex"].as_str() == Some(ex_name(l.ex).as_str()) && e["qty"].as_f64().is_some_and(|q| (q - l.req.qty).abs() <= 1e-9 * q.abs().max(1.0)));
                        if !same { return Err("the route changed since the preview; review it again".into()); }
                    }
                    for leg in &plan.legs { self.eng.submit(leg.ex, leg.req.clone(), leg.ref_px, ctx); }
                }
                return Ok(plan_json(&plan));
            }
            "cancel" => {
                let (e, id) = (exv()?, s("id"));
                let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == e && o.id == id).cloned().ok_or("order not found")?;
                self.eng.cancel(&o, ctx);
            }
            "close_position" => {
                let (e, symbol) = (exv()?, s("symbol"));
                let pos = if s("side") == "short" { Side::Sell } else { Side::Buy };
                let p = self.eng.account.lock().unwrap().positions.iter().find(|p| p.ex == e && p.symbol == symbol && p.side == pos).cloned().ok_or("position not found")?;
                let qty = f("qty").unwrap_or(p.qty).min(p.qty);
                let kind = match f("price") { Some(px) => Kind::Limit { price: px, tif: Tif::Gtc }, None => Kind::Market };
                self.eng.submit(e, OrderReq { symbol, pos, close: true, kind, qty, client_id: None }, p.mark, ctx);
            }
            "reverse_position" => {
                // close at market, then open the same size on the other side, both on the position's venue
                // ponytail: two independent market orders; a venue-native reverse (one-way: one order of 2x)
                // would remove the window where only the close has filled
                let (e, symbol) = (exv()?, s("symbol"));
                let pos = if s("side") == "short" { Side::Sell } else { Side::Buy };
                let p = self.eng.account.lock().unwrap().positions.iter().find(|p| p.ex == e && p.symbol == symbol && p.side == pos).cloned().ok_or("position not found")?;
                let other = if pos == Side::Buy { Side::Sell } else { Side::Buy };
                self.eng.submit(e, OrderReq { symbol: symbol.clone(), pos, close: true, kind: Kind::Market, qty: p.qty, client_id: None }, p.mark, ctx);
                self.eng.submit(e, OrderReq { symbol, pos: other, close: false, kind: Kind::Market, qty: p.qty, client_id: None }, p.mark, ctx);
            }
            "set_tpsl" => {
                let pos = if s("pos") == "short" { Side::Sell } else { Side::Buy };
                let trigger = if s("trigger") == "last" { trade::tpsl::Trigger::Last } else { trade::tpsl::Trigger::Mark };
                self.eng.set_tpsl(exv()?, s("symbol"), pos, f("tp"), f("sl"), f("partial_qty"), trigger, ctx);
            }
            "cancel_tpsl" => {
                let (e, id) = (exv()?, s("id"));
                let t = self.eng.account.lock().unwrap().tpsl.iter().find(|t| t.ex == e && t.id == id).cloned().ok_or("TP/SL not found")?;
                self.eng.cancel_tpsl(&t, ctx);
            }
            "chart" => { let b = self.eng.base.clone(); self.chart.native_set(v, &b); }
            "book" => self.book.native_set(v),
            "book_levels" => {
                let scope = self.eng.venues().into_iter().collect();
                let n = v["n"].as_u64().unwrap_or(20).clamp(3, 50) as usize;
                return Ok(self.book.native_levels(&self.eng.agg.lock().unwrap(), self.mode, &scope, n));
            }
            "venue_share" => {
                // per venue: resting depth within +-1% of its own mid (USD) and traded volume over the window
                let a = self.eng.agg.lock().unwrap();
                let m = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
                let mins = v["minutes"].as_u64().unwrap_or(60).clamp(1, 1440) as usize;
                let mut rows: Vec<Value> = a.venues.iter().filter(|((_, vm), _)| *vm == m).filter_map(|((e, vm), ven)| {
                    let mid = ven.mid()?;
                    let k = a.usd(*e, *vm);
                    let (lo, hi) = (mid * 0.99, mid * 1.01);
                    let bid: f64 = ven.book.bids().take_while(|(p, _)| *p >= lo).map(|(p, q)| p * q * k).sum();
                    let ask: f64 = ven.book.asks().take_while(|(p, _)| *p <= hi).map(|(p, q)| p * q * k).sum();
                    let (vol, buy, sell) = a.series.get(&(Some(*e), *vm)).map(|s| s.bars.iter().rev().take(mins)
                        .fold((0.0, 0.0, 0.0), |acc, b| (acc.0 + b.vol * mid * k, acc.1 + b.buy * mid * k, acc.2 + b.sell * mid * k))).unwrap_or_default();
                    let c = super::theme::ex_color(*e);
                    Some(json!({"ex": ex_name(*e), "color": format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b()),
                        "bid_usd": bid, "ask_usd": ask, "vol_usd": vol, "buy_usd": buy, "sell_usd": sell}))
                }).collect();
                rows.sort_by(|x, y| y["vol_usd"].as_f64().unwrap_or(0.0).total_cmp(&x["vol_usd"].as_f64().unwrap_or(0.0)));
                return Ok(json!({"rows": rows, "minutes": mins}));
            }
            "place_scaled" => {
                let req = self.order_req(&json!({"pos": v["pos"], "close": v["close"], "qty": v["qty"], "kind": "market"}))?;
                let (from, to) = (f("from").ok_or("from price")?, f("to").ok_or("to price")?);
                let n = v["count"].as_u64().unwrap_or(5).clamp(2, 50) as usize;
                let ex = self.trade.ex;
                let count = self.eng.submit_scaled(ex, req, from, to, n, f("skew").unwrap_or(0.0), v["post_only"].as_bool().unwrap_or(false), ctx)?;
                return Ok(json!({"orders": count}));
            }
            "start_twap" => {
                let req = self.order_req(&json!({"pos": v["pos"], "close": v["close"], "qty": v["qty"], "kind": "market"}))?;
                let ex = self.trade.ex;
                let minutes = f("minutes").ok_or("duration")?;
                let id = if v["native"].as_bool() == Some(true) && trade::native_twap(ex).is_some() {
                    let ref_px = self.eng.agg.lock().unwrap().venues.get(&(ex, Market::Perp)).and_then(|v| v.mid()).unwrap_or(0.0);
                    self.eng.start_native_twap(ex, req, minutes.round() as u32, f("limit").filter(|l| *l > 0.0), v["randomize"].as_bool().unwrap_or(false), ref_px, ctx)?
                } else {
                    self.eng.start_twap(ex, req, minutes, v["slices"].as_u64().unwrap_or(10) as usize, f("max_slip_bps").unwrap_or(10.0), f("limit").filter(|l| *l > 0.0), ctx)?
                };
                return Ok(json!({"id": id}));
            }
            "cancel_algo" => self.eng.cancel_algo(v["id"].as_u64().ok_or("id")?),
            "book_fill" => { if let (Some(e), Some(px)) = (parse_ex(&v["ex"]), v["px"].as_f64()) { self.trade.fill = Some((e, px)); } }
            "load_history" => self.eng.load_history(ctx),
            "load_wallets" => self.eng.load_wallets(exv()?, ctx),
            "transfer" => {
                if self.eng.account.lock().unwrap().transferring { return Err("a transfer is already running".into()); }
                let amount = f("amount").ok_or("amount")?;
                self.eng.transfer(exv()?, s("from"), s("to"), s("coin"), amount, v["product"].as_str().map(String::from), ctx);
            }
            "set_auto" => {
                let r = super::engine::AutoTopUp { enabled: v["enabled"].as_bool().unwrap_or(false), min: f("min").unwrap_or(0.0), target: f("target").unwrap_or(0.0) };
                self.eng.account.lock().unwrap().auto.insert(exv()?, r);
            }
            "save_keys" => {
                let e = exv()?;
                let extra = s("extra");
                trade::save_keys(e, &s("key"), &s("secret"), (!extra.trim().is_empty()).then_some(extra.as_str())).map_err(|e| format!("{e:#}"))?;
                let k = trade::keychain(e);
                if let Some(kk) = k.clone() { self.eng.test_keys(e, kk, ctx); }
                self.eng.set_keys(e, k);
            }
            "delete_keys" => { let e = exv()?; trade::delete_keys(e); self.eng.set_keys(e, None); }
            "test_keys" => { let e = exv()?; let k = self.eng.account.lock().unwrap().keys.get(&e).cloned().ok_or("no keys")?; self.eng.test_keys(e, k, ctx); }
            other => return Err(format!("unknown op {other:?}")),
        }
        Ok(json!({}))
    }

    /// The order the panel describes: pos long/short, close, kind limit/market/post/bbo.
    fn order_req(&self, v: &Value) -> Result<OrderReq, String> {
        let pos = if v["pos"] == "short" { Side::Sell } else { Side::Buy };
        let qty = v["qty"].as_f64().filter(|q| *q > 0.0).ok_or("size must be positive")?;
        let price = v["price"].as_f64().unwrap_or(0.0);
        // limit time in force: gtc (default), ioc, fok, post (post-only / ALO)
        let tif = match v["tif"].as_str().unwrap_or("gtc") { "ioc" => Tif::Ioc, "fok" => Tif::Fok, "post" => Tif::PostOnly, _ => Tif::Gtc };
        let kind = match v["kind"].as_str().unwrap_or("limit") {
            "market" => Kind::Market,
            "post" => Kind::Limit { price, tif: Tif::PostOnly },
            "bbo" => Kind::Bbo { queue: v["bbo_queue"].as_bool().unwrap_or(false), level: v["bbo_level"].as_u64().unwrap_or(1) as u8 },
            "stop" => Kind::Stop {
                trigger: v["trigger"].as_f64().filter(|t| *t > 0.0).ok_or("trigger price must be positive")?,
                limit: v["price"].as_f64().filter(|p| *p > 0.0),
                by_mark: v["trigger_by"].as_str() != Some("last"),
            },
            "trailing" => Kind::Trailing {
                callback_pct: v["callback_pct"].as_f64().filter(|c| (0.1..=10.0).contains(c)).ok_or("callback must be 0.1% to 10%")?,
                activation: v["activation"].as_f64().filter(|a| *a > 0.0),
            },
            _ => Kind::Limit { price, tif },
        };
        if matches!(kind, Kind::Limit { .. }) && !(price > 0.0) { return Err("price must be positive".into()); }
        Ok(OrderReq { symbol: trade::symbol(&self.eng.base), pos, close: v["close"].as_bool().unwrap_or(false), kind, qty, client_id: None })
    }

    /// Route through the planner (fixed venue = one leg) from local books and account state.
    fn plan(&self, req: &OrderReq) -> Result<route::RoutePlan, String> {
        let a = self.eng.agg.lock().unwrap();
        let st = self.eng.stats.lock().unwrap();
        let venues: Vec<route::VenueSnap> = trade::TRADABLE.iter().filter_map(|&e| {
            let v = a.venues.get(&(e, Market::Perp))?;
            Some(route::VenueSnap { ex: e, bids: v.book.bids().take(50).collect(), asks: v.book.asks().take(50).collect(), ts_ms: v.ts, lat_ms: st.latency(e) })
        }).collect();
        drop(st);
        drop(a);
        let acc = self.eng.account.lock().unwrap();
        let sym = &req.symbol;
        let mut snap = route::AcctSnap::default();
        for (&e, _) in acc.keys.iter() {
            if let Some(b) = acc.balances.get(&e) { snap.available.insert(e, b.available); }
            if let Some(l) = acc.levs.get(&(e, sym.clone())) { snap.lev.insert(e, *l); }
            if let Some(r) = acc.rules.get(&(e, sym.clone())) { snap.rules.insert(e, *r); }
            if let Some(m) = acc.modes.get(&(e, sym.clone())).copied().flatten() { snap.mode.insert(e, m); }
            if let Some(err) = acc.errors.get(&e) { snap.unavailable.insert(e, err.clone()); }
        }
        for e in trade::TRADABLE { if !acc.keys.contains_key(&e) { snap.unavailable.insert(e, "no API key".into()); } }
        snap.positions = acc.positions.iter().filter(|p| &p.symbol == sym).cloned().collect();
        drop(acc);
        let mut policy = self.route.clone();
        // conditional and trailing orders wait on one venue: never split or routed elsewhere
        if matches!(req.kind, Kind::Stop { .. } | Kind::Trailing { .. }) { policy.smart = false; }
        if !policy.smart { policy.fixed = self.trade.ex; }
        for e in trade::TRADABLE { let (t, m) = super::settings::fee(e); policy.fees.insert(e, (t, m)); }
        // venue clocks may run a little behind ours; staleness is judged on exchange timestamps
        route::plan(req, &venues, &snap, &policy, now_ms(), now_ms() as u64).map_err(|e| format!("{e:#}"))
    }
}

fn plan_json(p: &route::RoutePlan) -> Value {
    json!({
        "legs": p.legs.iter().map(|l| json!({
            "ex": ex_name(l.ex), "pos": side_name(l.req.pos), "close": l.req.close, "qty": l.req.qty,
            "kind": l.req.kind.label(),
            "ref_px": l.ref_px, "est_px": l.est_px, "est_fee": l.est_fee, "slip_bps": l.slip_bps, "avail_after": l.avail_after, "client_id": l.req.client_id,
        })).collect::<Vec<_>>(),
        "excluded": p.excluded.iter().map(|(e, r)| json!({"ex": ex_name(*e), "reason": r})).collect::<Vec<_>>(),
        "clamped_from": p.clamped_from, "total_qty": p.total_qty, "vwap": p.vwap, "fees": p.fees,
    })
}
