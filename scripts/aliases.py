#!/usr/bin/env python3
"""Regenerate the cross-venue perp alias table in src/alias.rs.

The unified base is Binance's name (the picker's names). For every Binance perp that is not a coin
(stocks, ETFs, commodities, FX) this looks for the same market on each other venue under its
same name, an `XSTOCK` name or a known synonym (GOLD, BRENTOIL, WTI, ...), Hyperliquid also on its
`xyz:` dex, and keeps a candidate only when its live price is within 1.5 % of Binance's. A
same-name market at another price (Lighter's KRW-priced SAMSUNG, MEXC's crypto CAT) is written as
"not listed" so the venue is skipped instead of poisoning the aggregate.

It also writes NAMES: the real ticker, company name and category of every stock / ETF / commodity / FX /
pre-IPO market on Binance or MEXC, keyed by the picker's base (the unified name, else the venue's own like
KIOXIASTOCK). Category from Binance's underlyingType, refined by MEXC's zones (ETF / index); names from MEXC.

    python3 scripts/aliases.py        # rewrites the tables between the markers in src/alias.rs
    (behind a proxy: HTTPS_PROXY=http://host:port python3 scripts/aliases.py)
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
mexc_info = get("https://contract.mexc.com/api/v1/contract/detail")["data"]
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

# the exchange ticker where Binance renamed a stock to avoid a coin's name; MEXC names it lacks
REAL = {"BBX": "BB", "STXX": "STX", "QNTX": "QNT"}
NAME = {"AAPL": "Apple Inc.", "SPCX": "SpaceX", "CL": "WTI Crude Oil", "BZ": "Brent Crude Oil", "MOONSHOT": "Moonshot AI"}
unified = {(v, n): b for b, v, n in rows if n}
def mexc_cat(plates):
    z = {p.removeprefix("mc-trade-zone-") for p in plates}
    if "preipo" in z: return "PreIpo"
    if z & {"Commodities", "metals", "metalsfutures", "OIL"}: return "Commodity"
    if z & {"ETF", "stockindex"}: return "Etf"
    if "forex" in z: return "Fx"
    return "Stock"
names = {}
for d in mexc_info:
    if not d["symbol"].endswith("_USDT"): continue
    plates = d.get("conceptPlate") or []
    if not {"mc-trade-zone-tradfi", "mc-trade-zone-Stock"} & set(plates): continue
    b = unified.get(("Mexc", d["baseCoin"]), d["baseCoin"])
    fn = d.get("fn") or ""
    # "AAPL_USDT<perpetual in Chinese>Apple Inc.": the company name follows the Chinese word
    name = NAME.get(b, fn.split("\u6c38\u7eed", 1)[1].strip() if "\u6c38\u7eed" in fn else "")
    if not name.isascii(): name = ""
    names[b] = (REAL.get(b, b.removesuffix("STOCK") or b), name, mexc_cat(plates))
BIN = {"COMMODITY": "Commodity", "FX": "Fx", "PREMARKET": "PreIpo"}
for x in info["symbols"]:
    b = x["symbol"][:-4]
    if b not in tradfi or x["symbol"] != b + "USDT": continue
    t, n, c = names.get(b, (REAL.get(b, b), NAME.get(b, ""), "Stock"))
    names[b] = (t, n, BIN.get(x.get("underlyingType"), "Etf" if c == "Etf" else "Stock"))

def lit(x): return "None" if x is None else f'Some("{x}")'
def esc(x): return x.replace("\\", "\\\\").replace('"', '\\"')
p = Path(__file__).resolve().parent.parent / "src/alias.rs"
src = p.read_text()
def put_block(tag, body):
    global src
    src = re.sub(r"(// BEGIN GENERATED " + tag + r"[^\n]*\n).*?([ \t]*// END GENERATED " + tag + ")", lambda m: m.group(1) + body + "\n" + m.group(2), src, count=1, flags=re.S)
put_block("PERP", "\n".join(f'    ("{b}", Exchange::{v}, {lit(n)}),' for b, v, n in rows))
put_block("NAMES", "\n".join(f'    ("{b}", "{esc(t)}", "{esc(n)}", Cat::{c}),' for b, (t, n, c) in sorted(names.items())))
p.write_text(src)
print(f"{len(rows)} aliases, {len(names)} names written to {p}", file=sys.stderr)
