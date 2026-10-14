#!/usr/bin/env python3
"""Regenerate the cross-venue perp alias table in src/alias.rs.

The unified base is Binance's name (the picker's names). For every Binance perp that is not a coin
(stocks, ETFs, commodities, FX) this looks for the same market on each other venue under its
same name, an `XSTOCK` name or a known synonym (GOLD, BRENTOIL, WTI, ...), Hyperliquid also on its
`xyz:` dex, and keeps a candidate only when its live price is within 1.5 % of Binance's. A
same-name market at another price (Lighter's KRW-priced SAMSUNG, MEXC's crypto CAT) is written as
"not listed" so the venue is skipped instead of poisoning the aggregate.

    python3 scripts/aliases.py        # rewrites the table between the markers in src/alias.rs
"""
import json, re, sys, urllib.request
from pathlib import Path

def get(u, data=None):
    r = urllib.request.Request(u, data=json.dumps(data).encode() if data else None, headers={"content-type": "application/json", "user-agent": "depth"})
    return json.load(urllib.request.urlopen(r, timeout=20))

px = {}
def put(v, name, p):
    try:
        p = float(p)
        if p > 0: px.setdefault(v, {})[name] = p
    except (TypeError, ValueError): pass

info = get("https://fapi.binance.com/fapi/v1/exchangeInfo")
tradfi = sorted({s["symbol"][:-4] for s in info["symbols"] if s.get("underlyingType") not in ("COIN", "INDEX")
                 and s["status"] == "TRADING" and s["symbol"].endswith("USDT")})
for t in get("https://fapi.binance.com/fapi/v1/premiumIndex"):
    if t["symbol"].endswith("USDT"): put("Binance", t["symbol"][:-4], t["markPrice"])
for t in get("https://api.bybit.com/v5/market/tickers?category=linear")["result"]["list"]:
    if t["symbol"].endswith("USDT"): put("Bybit", t["symbol"][:-4], t["markPrice"])
for t in get("https://www.okx.com/api/v5/market/tickers?instType=SWAP")["data"]:
    if t["instId"].endswith("-USDT-SWAP"): put("Okx", t["instId"][:-10], t["last"])
for t in get("https://api.bitget.com/api/v2/mix/market/tickers?productType=USDT-FUTURES")["data"]:
    if t["symbol"].endswith("USDT"): put("Bitget", t["symbol"][:-4], t["markPrice"])
for t in get("https://api.gateio.ws/api/v4/futures/usdt/tickers"):
    if t["contract"].endswith("_USDT"): put("Gate", t["contract"][:-5], t["mark_price"])
for t in get("https://contract.mexc.com/api/v1/contract/ticker")["data"]:
    if t["symbol"].endswith("_USDT"): put("Mexc", t["symbol"][:-5], t["lastPrice"])
for dex in ("", "xyz"):
    m, c = get("https://api.hyperliquid.xyz/info", {"type": "metaAndAssetCtxs", **({"dex": dex} if dex else {})})
    for u, x in zip(m["universe"], c):
        if not u.get("isDelisted"): put("Hyperliquid", u["name"], x.get("markPx"))
for t in get("https://mainnet.zklighter.elliot.ai/api/v1/exchangeStats")["order_book_stats"]:
    put("Lighter", t["symbol"], t["last_trade_price"])

SYN = {
    "XAU": ["GOLD"], "XAG": ["SILVER"], "XPT": ["PLATINUM"], "XPD": ["PALLADIUM"],
    "BZ": ["BRENTOIL", "UKOIL"], "CL": ["WTI", "USOIL"], "COPPER": ["XCU"], "NATGAS": ["NG", "NGAS"],
    "SAMSUNG": ["SMSN", "SAMSUNGUSD"], "SKHYNIX": ["SKHX"], "HYUNDAI": ["HYUNDAIUSD"],
    "BBX": ["BB"], "QNTX": ["QNT"], "STXX": ["STX"], "TSLA": ["TESLA"], "NVDA": ["NVIDIA"],
    "HOOD": ["ROBINHOOD"], "COIN": ["COINBASE"], "NOK": ["NOKIA"], "GIGADEV": ["GIGADEVICE"],
    "HK0700": ["TENCENTHKD"], "HK1810": ["XIAOMIHKD"], "HK0992": ["LENOVOHKD"], "TQQQ": ["TQQQX"],
}
TOL = 0.015
rows = []
for b in tradfi:
    ref = px["Binance"].get(b)
    if not ref: continue
    for v, d in px.items():
        if v == "Binance": continue
        names = [b, b + "STOCK"] + SYN.get(b, [])
        names += [n + "STOCK" for n in SYN.get(b, [])]
        if v == "Hyperliquid": names = ["xyz:" + n for n in names]
        ok = [n for n in names if n in d and abs(d[n] / ref - 1) <= TOL]
        same = ("xyz:" + b) if v == "Hyperliquid" else b
        if ok:
            if ok[0] != b: rows.append((b, v, ok[0]))
        elif same in d:
            # listed under the same name, but another asset or another scale
            rows.append((b, v, None))

def lit(x): return "None" if x is None else f'Some("{x}")'
body = "\n".join(f'    ("{b}", Exchange::{v}, {lit(n)}),' for b, v, n in rows)
p = Path(__file__).resolve().parent.parent / "src/alias.rs"
src = p.read_text()
src = re.sub(r"(// BEGIN GENERATED[^\n]*\n).*?([ \t]*// END GENERATED)", lambda m: m.group(1) + body + "\n" + m.group(2), src, flags=re.S)
p.write_text(src)
print(f"{len(rows)} aliases written to {p}", file=sys.stderr)
