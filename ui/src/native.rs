//! Bridge for the native (SwiftUI) panels: one JSON state snapshot the host polls, and JSON
//! actions it sends. egui keeps drawing the chart, book and option chain; everything else
//! (title bar, order entry, account tables, settings, status bar) is SwiftUI.
use super::{l, App};
use eframe::egui;
use serde_json::{json, Value};
use std::time::Instant;
use depth::trade::{self, Kind, OrderReq, Tif};
use depth::{now_ms, route, Exchange, Market, Side};

pub(crate) fn ex_name(e: Exchange) -> String { format!("{e:?}") }
fn parse_ex(v: &Value) -> Option<Exchange> { Exchange::parse(v.as_str()?) }
fn side_name(s: Side) -> &'static str { if s == Side::Buy { "long" } else { "short" } }

impl App {
    /// Everything the native panels show, as one JSON document (polled ~5x a second).
    pub fn state_json(&mut self) -> String {
        // alerts are checked here too: the host polls this twice a second even when the chart is hidden
        { let agg = self.eng.agg.clone(); let g = agg.lock().unwrap(); self.alerts_tick(&g); }
        let fired: Vec<Value> = self.chart.fired.drain(..).map(|(title, body)| json!({"title": title, "body": body})).collect();
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

        let mut acc = self.eng.account.lock().unwrap();
        let day = super::engine::day_pnl(&mut acc, nowms, super::local_offset_ms());
        let risk = json!({"limits": acc.limits, "day_pnl": day,
            "blocked": acc.limits.daily_loss_usdt > 0.0 && day.is_some_and(|d| d <= -acc.limits.daily_loss_usdt)});
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
            "max_lev": acc.max_levs.get(&key), "tick": rules.map(|r| r.tick), "step": rules.map(|r| r.step), "min_qty": rules.map(|r| r.min_qty), "min_notional": rules.map(|r| r.min_notional),
            "bbo_levels": trade::bbo_levels(ex), "caps": trade::caps(ex), "native_twap": trade::native_twap(ex), "push_ms": push_ms, "rtt_ms": acc.order_rtt.get(&ex),
            "fee": {"taker": fee_t, "maker": fee_m},
            "smart": self.route.smart, "error": acc.errors.get(&ex),
            "read_only": acc.can_trade.get(&ex) == Some(&false),
        });
        let funds = self.eng.funding.lock().unwrap().clone();
        let opt_positions: Vec<Value> = acc.opts.iter().map(|o| json!({
            "ex": ex_name(o.ex), "symbol": o.symbol, "base": o.base, "expiry": o.expiry, "strike": o.strike, "call": o.call,
            "side": side_name(o.side), "qty": o.qty, "entry": o.avg, "mark": o.mark, "upnl": o.upnl, "option": true,
        })).collect();
        let positions: Vec<Value> = acc.positions.iter().map(|p| {
            let f = funds.get(&(p.ex, p.symbol.clone()));
            let mode = acc.modes.get(&(p.ex, p.symbol.clone())).copied().flatten().map(|m| if m == trade::Mode::Hedge { "hedge" } else { "oneway" });
            // a positive rate: longs pay shorts
            let pay = f.map(|f| -(if p.side == Side::Buy { 1.0 } else { -1.0 }) * p.qty * p.mark * f.rate);
            // funding since open (received positive); since 0 = open time unknown, lower = the position is older than since
            let paid = acc.funding_paid.get(&(p.ex, p.symbol.clone(), p.side)).and_then(|s| s.val);
            json!({
                "ex": ex_name(p.ex), "symbol": p.symbol, "side": side_name(p.side), "qty": p.qty, "entry": p.entry, "mark": p.mark, "mode": mode,
                "liq": p.liq, "upnl": p.upnl, "lev": p.lev, "margin": p.margin, "cross": p.cross,
                "funding_rate": f.map(|f| f.rate), "funding_interval_h": f.map(|f| f.interval_h), "next_funding_ms": f.map(|f| f.next_ms), "funding_est": pay,
                "funding_paid": paid.map(|x| x.amount), "funding_since": paid.map(|x| x.since).filter(|t| *t > 0), "funding_since_lower": paid.map(|x| x.lower),
            })
        }).collect();
        let order_json = |o: &trade::OpenOrder, market: &str| json!({
            "ex": ex_name(o.ex), "symbol": o.symbol, "id": o.id, "side": if o.side == Side::Buy { "buy" } else { "sell" }, "price": o.price, "qty": o.qty,
            "filled": o.filled, "kind": o.kind, "tif": o.tif, "reduce_only": o.reduce_only, "ts": o.ts, "pos": o.pos.map(side_name), "spot": market != "perp", "market": market,
        });
        let orders: Vec<Value> = acc.orders.iter().map(|o| order_json(o, "perp")).chain(acc.spot_orders.iter().map(|o| order_json(o, "spot")))
            .chain(acc.margin_orders.iter().map(|o| order_json(o, "margin"))).collect();
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
        let kill = json!({
            "plan": super::engine::kill_plan(&acc).iter().map(|(e, o, t, j, p)| json!({"ex": ex_name(*e), "orders": o, "tpsl": t, "algos": j, "positions": p})).collect::<Vec<_>>(),
            "report": acc.kill.as_ref().map(|r| json!({"started_ms": r.started_ms, "venues": r.venues.iter().map(|(e, lines, done)| json!({"ex": ex_name(*e), "done": done,
                "lines": lines.iter().map(|(m, ok)| json!({"msg": m, "ok": ok})).collect::<Vec<_>>()})).collect::<Vec<_>>()})),
        });
        let tpsl_wait: Vec<Value> = acc.tpsl_wait.iter().map(|w| json!({"ex": ex_name(w.ex), "symbol": w.symbol, "pos": side_name(w.pos), "tp": w.tp, "sl": w.sl,
            "trigger": if w.trigger == trade::tpsl::Trigger::Last { "last" } else { "mark" }, "client_id": w.client_id, "order_id": w.order_id, "filled": w.filled_ms.is_some(), "created_ms": w.created_ms})).collect();
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
            let bars: Vec<depth::agg::Bar> = ag.series.get(&(None, m)).map(|s| s.bars.iter().cloned().collect()).unwrap_or_default();
            let vm = depth::quant::vol_model(&bars);
            let mid = ag.mid(m).unwrap_or(0.0);
            let (b, k) = ag.book_where(m, depth::agg::nice(mid.max(1e-9) * 1e-4), 0.005, true, |_| true);
            let usd = |ls: &[depth::agg::Level]| ls.iter().map(|l| l.qty * l.px).sum::<f64>();
            let carry: Vec<Value> = ag.venues.iter().filter(|((_, vm2), _)| *vm2 == Market::Perp).map(|((e, _), v)| json!({
                "ex": ex_name(*e), "funding_apr": v.funding_h.map(|f| f * 24.0 * 365.0 * 100.0), "basis_bps": ag.basis_bps(*e),
            })).collect();
            json!({
                "vol": vm.map(|v| json!({"sigma_1h": v.sigma_1h, "sigma_24h": v.sigma_24h, "percentile": v.percentile,
                    "range_1h": [mid * (1.0 - v.sigma_1h), mid * (1.0 + v.sigma_1h)], "range_24h": [mid * (1.0 - v.sigma_24h), mid * (1.0 + v.sigma_24h)]})),
                "pressure": depth::quant::pressure(usd(&b), usd(&k), &bars, 5),
                "book_bid_usd": usd(&b), "book_ask_usd": usd(&k), "carry": carry,
            })
        };
        let (chart, book) = {
            let ag = self.eng.agg.lock().unwrap();
            let mut b = self.book.native_json(&ag, self.mode);
            b["width"] = self.book_w.into();
            let mut c = self.chart.native_json(&ag, self.mode, &self.eng.base);
            c["layout"] = self.layout.clone().into();
            (c, b)
        };
        json!({
            "base": self.eng.base, "mode": format!("{:?}", self.mode), "lang": super::lang_code(), "langs": super::LANGS,
            "header": header, "venues": venues, "stats": stats, "trade": trade_v,
            "positions": positions, "opt_positions": opt_positions, "orders": orders, "tpsl": tpsl, "tpsl_wait": tpsl_wait,
            "history": {"orders": hist.0, "fills": hist.1, "closed": hist.2, "updated_ms": (hist.3 != i64::MAX).then_some(hist.3),
                        "loading": !self.eng.account.lock().unwrap().history_loading.is_empty()},
            "wallets": wallets, "balances": balances, "algos": algos, "transferring": transferring, "binance_pm": pm, "log": log, "keys": keys,
            "prefs": serde_json::to_value(&self.prefs).unwrap_or(Value::Null),
            "route": serde_json::to_value(&self.route).unwrap_or(Value::Null),
            "chart": chart, "book": book, "fired": fired, "signals": signals, "quant": quant,
            "book_click": self.trade.fill.map(|(e, px)| json!({"ex": ex_name(e), "px": px})),
            "tradable": trade::TRADABLE.iter().map(|e| ex_name(*e)).collect::<Vec<_>>(),
            "risk": risk, "kill": kill,
        }).to_string()
    }

    /// One action from the native panels; returns {"ok": true, ...} or {"ok": false, "error": ...}.
    pub fn call_json(&mut self, ctx: &egui::Context, req: &str) -> String {
        let r = serde_json::from_str::<Value>(req).map_err(|e| e.to_string()).and_then(|v| self.call(ctx, &v));
        match r { Ok(v) => { let mut v = v; v["ok"] = true.into(); v.to_string() } Err(e) => json!({"ok": false, "error": e}).to_string() }
    }

    pub(crate) fn call(&mut self, ctx: &egui::Context, v: &Value) -> Result<Value, String> {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let f = |k: &str| v[k].as_f64();
        let exv = || parse_ex(&v["ex"]).ok_or("unknown venue");
        match v["op"].as_str().unwrap_or_default() {
            "set_base" => { let b = s("base").to_uppercase(); if !b.is_empty() && b != self.eng.base { self.pending_base = Some(b); } }
            "set_mode" => { if let Some(m) = Market::parse(&s("mode")) { self.pending_mode = Some(m); } }
            "toggle_venue" => {
                let e = exv()?;
                let on = self.eng.off.contains(&e);
                if !on && self.eng.off.len() + 1 >= Exchange::ALL.len() { return Err("keep at least one venue".into()); }
                // only this venue connects / disconnects; the others keep streaming
                self.eng.set_venue(e, on, ctx);
                if !on { if self.chart.src == Some(e) { self.chart.src = None; } if self.book.src == Some(e) { self.book.src = None; } }
            }
            "set_trade_venue" => { let e = exv()?; if !trade::TRADABLE.contains(&e) { return Err("not tradable".into()); } self.trade.ex = e; self.route.fixed = e; }
            "cache_size" => return Ok(json!({"bytes": depth::sys::cache_bytes()})),
            "clear_cache" => return Ok(json!({"freed": depth::sys::clear_cache(), "bytes": depth::sys::cache_bytes()})),
            "set_lang" => super::set_lang(v["lang"].as_str().unwrap_or("en")),
            "set_prefs" => {
                let old = (self.prefs.proxy, self.prefs.proxy_url.clone());
                self.prefs = super::settings::merge_prefs(&self.prefs, &v["prefs"])?;
                self.prefs.apply(ctx);
                self.eng.account.lock().unwrap().limits = self.prefs.risk;
                // every stream reconnects through the new route now; private ones re-snapshot
                if old != (self.prefs.proxy, self.prefs.proxy_url.clone()) {
                    self.pending_restart = true;
                    let keys: Vec<_> = self.eng.account.lock().unwrap().keys.iter().map(|(e, k)| (*e, k.clone())).collect();
                    for (e, k) in keys { self.eng.set_keys(e, Some(k)); }
                }
            }
            // the daily loss stop counts from now
            "risk_reset" => self.eng.account.lock().unwrap().day_start.1.clear(),
            "set_route" => { self.route = serde_json::from_value(v["route"].clone()).map_err(|e| e.to_string())?; }
            "tickers" => {
                let t = self.eng.tickers.lock().unwrap();
                return Ok(json!({"tickers": t.iter().map(|x| json!({"base": x.base, "last": x.last, "chg_pct": x.chg_pct, "quote_vol": x.quote_vol})).collect::<Vec<_>>()}));
            }
            "preview" | "place" => {
                let req = self.order_req(v)?;
                let plan = self.plan_at(&req, None, v["tpsl"] == true)?;
                if v["op"] == "place" {
                    // the user confirmed a previewed plan: refuse if the venues or sizes moved since
                    if let Some(exp) = v["expect"].as_array() {
                        let same = exp.len() == plan.legs.len() && exp.iter().zip(&plan.legs).all(|(e, l)|
                            e["ex"].as_str() == Some(ex_name(l.ex).as_str()) && e["qty"].as_f64().is_some_and(|q| (q - l.req.qty).abs() <= 1e-9 * q.abs().max(1.0)));
                        if !same { return Err("the route changed since the preview; review it again".into()); }
                    }
                    // TP/SL: attached to the order where the venue takes it (protected at fill), else set by the
                    // engine once this very order has filled (`TpSlWait`)
                    let trigger = if s("trigger") == "last" { trade::tpsl::Trigger::Last } else { trade::tpsl::Trigger::Mark };
                    let attach = (f("tp").is_some() || f("sl").is_some()).then_some(trade::tpsl::Attach { tp: f("tp"), sl: f("sl"), trigger }).filter(|_| !req.close);
                    for leg in &plan.legs {
                        let mut r = leg.req.clone();
                        match attach {
                            Some(at) if trade::caps(leg.ex).attach_tpsl => r.attach = Some(at),
                            Some(at) => { if let Some(w) = super::engine::TpSlWait::new(leg.ex, &r, at, now_ms()) { self.eng.account.lock().unwrap().tpsl_wait.push(w); } }
                            None => {}
                        }
                        self.eng.submit(leg.ex, r, leg.ref_px, ctx);
                    }
                }
                return Ok(plan_json(&plan));
            }
            "cancel" if v["spot"] == true => {
                let (e, id) = (exv()?, s("id"));
                let (o, margin) = {
                    let a = self.eng.account.lock().unwrap();
                    let find = |v: &[trade::OpenOrder]| v.iter().find(|o| o.ex == e && o.id == id).cloned();
                    match find(&a.spot_orders) { Some(o) => (o, false), None => (find(&a.margin_orders).ok_or("order not found")?, true) }
                };
                self.eng.spot_cancel(&o, margin, ctx);
            }
            "cancel" => {
                let (e, id) = (exv()?, s("id"));
                let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == e && o.id == id).cloned().ok_or("order not found")?;
                self.eng.cancel(&o, ctx);
            }
            "move_order" => {
                let (e, id) = (exv()?, s("id"));
                let o = self.eng.account.lock().unwrap().orders.iter().find(|o| o.ex == e && o.id == id).cloned().ok_or("order not found")?;
                self.eng.replace_order(&o, f("price").filter(|p| *p > 0.0).ok_or("no price")?, ctx);
            }
            "close_position" => {
                let (e, symbol) = (exv()?, s("symbol"));
                let pos = if s("side") == "short" { Side::Sell } else { Side::Buy };
                let p = self.eng.account.lock().unwrap().positions.iter().find(|p| p.ex == e && p.symbol == symbol && p.side == pos).cloned().ok_or("position not found")?;
                let qty = f("qty").unwrap_or(p.qty).min(p.qty);
                let kind = match f("price") { Some(px) => Kind::Limit { price: px, tif: Tif::Gtc }, None => Kind::Market };
                self.close(e, OrderReq { symbol, pos, close: true, kind, qty, client_id: None, attach: None }, p.mark, ctx);
            }
            "reverse_position" => {
                // the same size on the other side, on the position's venue: one-way, one market order of
                // twice the size (the venue nets it, no flat window); hedge or mode unknown, close then open
                let (e, symbol) = (exv()?, s("symbol"));
                let pos = if s("side") == "short" { Side::Sell } else { Side::Buy };
                let (p, mode) = {
                    let a = self.eng.account.lock().unwrap();
                    let p = a.positions.iter().find(|p| p.ex == e && p.symbol == symbol && p.side == pos).cloned().ok_or("position not found")?;
                    (p, a.modes.get(&(e, symbol.clone())).copied().flatten())
                };
                self.eng.submit_seq(e, super::engine::reverse_reqs(mode, &p), p.mark, ctx);
            }
            "set_tpsl" => {
                let pos = if s("pos") == "short" { Side::Sell } else { Side::Buy };
                let trigger = if s("trigger") == "last" { trade::tpsl::Trigger::Last } else { trade::tpsl::Trigger::Mark };
                self.eng.set_tpsl(exv()?, s("symbol"), pos, f("tp"), f("sl"), f("partial_qty"), trigger, ctx);
            }
            "kill" => {
                // closes of the symbol on screen through the slippage cap: the planner prices them from each venue's own book
                let sym = trade::symbol(&self.eng.base);
                let ps: Vec<trade::Position> = self.eng.account.lock().unwrap().positions.iter().filter(|p| p.symbol == sym && p.qty > 0.0).cloned().collect();
                let capped = ps.iter().filter_map(|p| {
                    let req = OrderReq { symbol: p.symbol.clone(), pos: p.side, close: true, kind: Kind::Market, qty: p.qty, client_id: None, attach: None };
                    let leg = self.plan_at(&req, Some(p.ex), false).ok()?.legs.into_iter().next()?;
                    Some((p.ex, leg.req, leg.ref_px))
                }).collect();
                self.eng.kill(capped, ctx);
            }
            "cancel_tpsl_wait" => {
                let (e, cid) = (exv()?, s("client_id"));
                self.eng.account.lock().unwrap().tpsl_wait.retain(|w| !(w.ex == e && w.client_id == cid));
            }
            "cancel_tpsl" => {
                let (e, id) = (exv()?, s("id"));
                let t = self.eng.account.lock().unwrap().tpsl.iter().find(|t| t.ex == e && t.id == id).cloned().ok_or("TP/SL not found")?;
                self.eng.cancel_tpsl(&t, ctx);
            }
            "chart" => {
                if let Some(l) = v["layout"].as_str() { self.layout = l.to_string(); }
                let b = self.eng.base.clone();
                self.chart.native_set(v, &b);
            }
            "book" => self.book.native_set(v),
            "book_levels" => {
                let scope = self.eng.venues().into_iter().collect();
                let n = v["n"].as_u64().unwrap_or(20).clamp(3, 50) as usize;
                return Ok(self.book.native_levels(&self.eng.agg.lock().unwrap(), self.mode, &scope, n));
            }
            "venue_share" => {
                // per venue: resting depth within +-1% of its own mid (USDT) and traded volume over the window
                let a = self.eng.agg.lock().unwrap();
                let m = match self.mode { Market::Margin | Market::Option => Market::Spot, m => m };
                let mins = v["minutes"].as_u64().unwrap_or(60).clamp(1, 1440) as usize;
                let mut rows: Vec<Value> = a.venues.iter().filter(|((_, vm), _)| *vm == m).filter_map(|((e, vm), ven)| {
                    let mid = ven.mid()?;
                    let k = a.usdt(*e, *vm);
                    let (lo, hi) = (mid * 0.99, mid * 1.01);
                    let bid: f64 = ven.book.bids().take_while(|(p, _)| *p >= lo).map(|(p, q)| p * q * k).sum();
                    let ask: f64 = ven.book.asks().take_while(|(p, _)| *p <= hi).map(|(p, q)| p * q * k).sum();
                    let (vol, buy, sell) = a.series.get(&(Some(*e), *vm)).map(|s| s.bars.iter().rev().take(mins)
                        .fold((0.0, 0.0, 0.0), |acc, b| (acc.0 + b.vol * mid * k, acc.1 + b.buy * mid * k, acc.2 + b.sell * mid * k))).unwrap_or_default();
                    let c = super::theme::ex_color(*e);
                    // premium over the composite now, and this venue's normal premium (last hour's median)
                    let prem = a.mid(*vm).map(|ix| (mid * k / ix - 1.0) * 1e4);
                    let hist: Vec<f64> = a.premium_bps(*e, *vm).iter().rev().take(60).map(|x| x.1).collect();
                    let norm = depth::quant::robust(&hist).map(|x| x.0);
                    let top = ven.bbo.map(|b| (b[0], b[2])).or_else(|| Some((ven.book.best_bid()?.0, ven.book.best_ask()?.0)));
                    Some(json!({"ex": ex_name(*e), "color": format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b()),
                        "bid_usd": bid, "ask_usd": ask, "vol_usd": vol, "buy_usd": buy, "sell_usd": sell,
                        "quote": depth::agg::quote_of(*e, *vm), "bid": top.map(|t| t.0), "ask": top.map(|t| t.1),
                        "prem_bps": prem, "norm_bps": norm, "stale": !a.live(ven, *vm)}))
                }).collect();
                rows.sort_by(|x, y| y["vol_usd"].as_f64().unwrap_or(0.0).total_cmp(&x["vol_usd"].as_f64().unwrap_or(0.0)));
                // executable cross: same quote only, fresh books only, net of both taker fees
                let tops: Vec<depth::quant::Top> = rows.iter().filter(|r| r["stale"] == false).filter_map(|r| {
                    let ex = r["ex"].as_str()?;
                    Some(depth::quant::Top { ex, quote: r["quote"].as_str()?, bid: r["bid"].as_f64()?, ask: r["ask"].as_f64()?,
                        taker: Exchange::parse(ex).map(|e| super::settings::fee(e).0).unwrap_or(0.0005) })
                }).collect();
                let arb = depth::quant::best_cross(&tops).map(|(i, j, gross, net)| json!({
                    "buy": tops[i].ex, "sell": tops[j].ex, "ask": tops[i].ask, "bid": tops[j].bid, "quote": tops[i].quote, "gross_bps": gross, "net_bps": net}));
                return Ok(json!({"rows": rows, "minutes": mins, "arb": arb}));
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
            "leverage_max" => self.eng.load_max_leverage(self.trade.ex, trade::symbol(&self.eng.base), ctx),
            "set_leverage" => {
                let lev = v["leverage"].as_u64().ok_or("leverage")? as u32;
                self.eng.set_leverage(self.trade.ex, trade::symbol(&self.eng.base), lev, ctx);
            }
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
        Ok(OrderReq { symbol: trade::symbol(&self.eng.base), pos, close: v["close"].as_bool().unwrap_or(false), kind, qty, client_id: None, attach: None })
    }

    /// A close on the position's venue. At market it goes through the planner's slippage cap (a
    /// walk costing more than the cap, or running out of book, becomes an IOC limit at the touch ± the
    /// cap); without a usable local book for it (another symbol, stale book, rules not loaded) it goes
    /// out as a plain market close and the order log says so. Limit closes go out as they are.
    fn close(&self, e: Exchange, req: OrderReq, mark: f64, ctx: &egui::Context) {
        if req.kind == Kind::Market {
            let why = if req.symbol != trade::symbol(&self.eng.base) { "no live book for this symbol".to_string() } else {
                match self.plan_at(&req, Some(e), false) {
                    Ok(p) if !p.legs.is_empty() => { for leg in p.legs { self.eng.submit(leg.ex, leg.req, leg.ref_px, ctx); } return; }
                    Ok(_) => "nothing to close".into(),
                    Err(err) => err,
                }
            };
            self.eng.account.lock().unwrap().note(format!("NOTE {e:?} {}: {why}; market close sent without a slippage cap", req.symbol), true);
        }
        self.eng.submit(e, req, mark, ctx);
    }

    /// Route through the planner (fixed venue = one leg) from local books and account state;
    /// `fixed` pins one venue whatever the routing policy, `tpsl` keeps to venues that take TP/SL.
    pub(crate) fn plan_at(&self, req: &OrderReq, fixed: Option<Exchange>, tpsl: bool) -> Result<route::RoutePlan, String> {
        let a = self.eng.agg.lock().unwrap();
        let st = self.eng.stats.lock().unwrap();
        let venues: Vec<route::VenueSnap> = trade::TRADABLE.iter().filter_map(|&e| {
            let v = a.venues.get(&(e, Market::Perp))?;
            Some(route::VenueSnap { ex: e, bids: v.book.bids().take(50).collect(), asks: v.book.asks().take(50).collect(), ts_ms: v.fresh, lat_ms: st.latency(e), fx: a.usdt(e, Market::Perp) })
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
        for (e, why) in tpsl_excluded(&trade::TRADABLE, tpsl) { snap.unavailable.entry(e).or_insert(why); }
        snap.positions = acc.positions.iter().filter(|p| &p.symbol == sym).cloned().collect();
        drop(acc);
        let mut policy = self.route.clone();
        // conditional and trailing orders wait on one venue: never split or routed elsewhere
        if matches!(req.kind, Kind::Stop { .. } | Kind::Trailing { .. }) { policy.smart = false; }
        if let Some(e) = fixed { (policy.smart, policy.fixed) = (false, e); } else if !policy.smart { policy.fixed = self.trade.ex; }
        for e in trade::TRADABLE { let (t, m) = super::settings::fee(e); policy.fees.insert(e, (t, m)); }
        // staleness is judged on local receive time of the last book/BBO (v.ts follows every event, incl. polled
        // long/short rows stamped minutes back)
        let plan = route::plan(req, &venues, &snap, &policy, now_ms(), now_ms() as u64).map_err(|e| format!("{e:#}"))?;
        // risk limits, so the preview shows the block before anything is confirmed
        super::engine::check_plan(&mut self.eng.account.lock().unwrap(), &plan.legs).map_err(|e| format!("{} {e}", l("Risk limit:")))?;
        Ok(plan)
    }
}

/// Venues an order with TP/SL must not be routed to: those without TP/SL support.
fn tpsl_excluded(venues: &[Exchange], tpsl: bool) -> Vec<(Exchange, String)> {
    venues.iter().filter(|e| tpsl && !trade::caps(**e).tpsl).map(|e| (*e, "no TP/SL on this venue".to_string())).collect()
}

fn plan_json(p: &route::RoutePlan) -> Value {
    json!({
        "legs": p.legs.iter().map(|l| json!({
            "ex": ex_name(l.ex), "pos": side_name(l.req.pos), "close": l.req.close, "qty": l.req.qty,
            "kind": l.req.kind.label(),
            "ref_px": l.ref_px, "est_px": l.est_px, "est_fee": l.est_fee, "slip_bps": l.slip_bps, "avail_after": l.avail_after, "client_id": l.req.client_id,
            "fx": l.fx, "quote": depth::agg::quote_of(l.ex, Market::Perp),
        })).collect::<Vec<_>>(),
        "excluded": p.excluded.iter().map(|(e, r)| json!({"ex": ex_name(*e), "reason": r})).collect::<Vec<_>>(),
        "clamped_from": p.clamped_from, "total_qty": p.total_qty, "vwap": p.vwap, "fees": p.fees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpsl_orders_skip_venues_without_tpsl() {
        assert!(tpsl_excluded(&trade::TRADABLE, false).is_empty());
        let out: Vec<Exchange> = tpsl_excluded(&trade::TRADABLE, true).into_iter().map(|x| x.0).collect();
        assert!(!out.contains(&Exchange::Bybit) && !out.contains(&Exchange::Binance), "{out:?}");
        for e in [Exchange::Okx, Exchange::Bitget, Exchange::Gate, Exchange::Kraken, Exchange::Hyperliquid, Exchange::Lighter] { assert!(out.contains(&e), "{e:?}"); }
    }
}
