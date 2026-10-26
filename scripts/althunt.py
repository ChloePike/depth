#!/usr/bin/env python3
"""Alt hunter backtest: which operator-coin signatures precede big moves on Binance USD-M alt perps,
how early, and how often they fire on ordinary coins. Stdlib only.

Data comes from Binance's public archive (data.binance.vision: 1h klines, premium index, funding,
5m metrics, spot klines, book depth), never from fapi or any account endpoint, plus Gate's public
archive (download.gatedata.org, 1m futures candles) for the cross-venue check. Every file is cached
under ~/Library/Caches/Depth/althunt (Depth's clearable cache) and never downloaded twice; a 404 leaves
a marker. At most --jobs (default 6) requests are in flight; 403/418/429/451 stops the run.

    python3 scripts/althunt.py fetch     # universe listing + downloads (resumable)
    python3 scripts/althunt.py scan      # universe scan: firings, precision vs base rates, OOS split
    python3 scripts/althunt.py study     # event study on the documented cases + cross-venue lead
    python3 scripts/althunt.py report [out.md]   # markdown from scan.json + study.json
    python3 scripts/althunt.py selftest  # no-lookahead and helper checks

Options: --from 2024-11 --to 2026-09 (archive months), --sample 150 (coins with 5m metrics; OI and
long/short detectors run on this random sample only), --seed 7, --jobs 6, --procs N.

Method (no lookahead: a state at hour i reads bars closed by the end of hour i only):
  * every detector is evaluated on each coin's closed 1h bars; thresholds are relative to the coin's
    own trailing 30d (percentiles) where the research gave them that way;
  * a firing is a true state at least 24h after the coin's previous firing of the same detector;
  * outcome = max move from the firing close within 1/4/24/72h (high/low of the following bars);
  * precision = share of firings followed by >= 30% within 24h in the play's direction, against the
    base rate of all coin-hours and a volatility-matched base rate (same 7d realized-vol bucket);
  * one threshold per detector is chosen on data before 2025-07-01 (best Wilson lower bound) and
    reported separately on later data.
"""
import argparse, bisect, calendar, csv, gzip, http.client, io, json, math, os, pickle, random, re
import statistics, sys, threading, time, zipfile
from collections import defaultdict, deque
from concurrent.futures import ThreadPoolExecutor
from multiprocessing import Pool
from pathlib import Path

CACHE = Path(os.environ.get("ALTHUNT_CACHE", Path.home() / "Library/Caches/Depth/althunt"))
RAW, WORK = CACHE / "raw", CACHE / "work"
BV, S3, GATE = "data.binance.vision", "s3-ap-northeast-1.amazonaws.com", "download.gatedata.org"
HOUR, DAY = 3_600_000, 86_400_000
REPO = Path(__file__).resolve().parent.parent


def ms(d): return calendar.timegm(time.strptime(d[:10], "%Y-%m-%d")) * 1000
def day(t): return time.strftime("%Y-%m-%d", time.gmtime(t / 1000))
def stamp(t): return time.strftime("%Y-%m-%d %H:%M", time.gmtime(t / 1000))


SPLIT = ms("2025-07-01")           # thresholds are chosen before, results reported after
SCAN_FROM, SCAN_TO = ms("2024-12-01"), ms("2026-10-01")
MIN_AGE = 168                      # hours of history before any detector is evaluated
COOLDOWN = 24                      # hours between two firings of one detector on one coin
VOL_EDGES = [0.004, 0.006, 0.008, 0.011, 0.015, 0.02, 0.03, 0.045, 0.065, 0.1]   # 7d sd of 1h returns
HORIZONS = (1, 4, 24, 72, 168)
# (direction, horizon h, threshold): precision targets. 30% is the play target: a typical alt moves
# 5-10% a day, so 30% in 24h is a 3-5 sigma day, the scale of every documented case, and leaves room
# for ~2% slippage each way on a thin book.
TG = [("up", 1, .3), ("up", 4, .3), ("up", 24, .3), ("up", 72, .3), ("dn", 1, .3), ("dn", 4, .3),
      ("dn", 24, .3), ("dn", 72, .3), ("any", 24, .3), ("up", 24, .15), ("dn", 24, .15),
      ("up", 24, .5), ("dn", 24, .5), ("up", 168, .5), ("dn", 168, .5)]
TGI = {t: k for k, t in enumerate(TG)}
EXCLUDE = {"BTC", "ETH", "USDC", "FDUSD", "TUSD", "BUSD", "DAI", "USDP", "USDE", "USD1", "RLUSD", "XUSD",
           "BTCDOM", "DEFI", "FOOTBALL", "BLUEBIRD", "PAXG", "XAUT", "XAU", "XAG"}

# ---------------------------------------------------------------------------------------------- cases
# name, Binance symbol, direction, window start, window end. The move is pinned from 1h bars inside
# the window: up = the highest high, t0 = the lowest low before it (the move's start); dn = the lowest
# low, t0 = the highest high before it (the top). Sources: alt-research.json (30 + 17 cases).
CASES = [
    ("MOVE listing dump", "MOVEUSDT", "dn", "2024-12-09", "2024-12-31"),
    ("SHELL listing dump", "SHELLUSDT", "dn", "2025-02-13", "2025-03-12"),
    ("GPS listing dump", "GPSUSDT", "dn", "2025-03-05", "2025-03-12"),
    ("ACT flash crash", "ACTUSDT", "dn", "2025-03-30", "2025-04-02"),
    ("OM collapse", "OMUSDT", "dn", "2025-04-11", "2025-04-15"),
    ("VOXEL (Bitget anomaly)", "VOXELUSDT", "up", "2025-04-17", "2025-04-21"),
    ("ALPACA delisting squeeze", "ALPACAUSDT", "up", "2025-04-22", "2025-05-01"),
    ("ZKJ collapse", "ZKJUSDT", "dn", "2025-06-13", "2025-06-17"),
    ("MYX leg 1", "MYXUSDT", "up", "2025-07-28", "2025-08-09"),
    ("MEMEFI delisting squeeze", "MEMEFIUSDT", "up", "2025-08-06", "2025-08-12"),
    ("MYX leg 2 squeeze", "MYXUSDT", "up", "2025-09-01", "2025-09-12"),
    ("BAKE delisting squeeze", "BAKEUSDT", "up", "2025-09-04", "2025-09-12"),
    ("HIFI delisting squeeze", "HIFIUSDT", "up", "2025-09-04", "2025-09-12"),
    ("SLF delisting squeeze", "SLFUSDT", "up", "2025-09-04", "2025-09-12"),
    ("AIA listing pump", "AIAUSDT", "up", "2025-09-18", "2025-10-06"),
    ("COAI pump", "COAIUSDT", "up", "2025-09-27", "2025-10-13"),
    ("AIA second rally", "AIAUSDT", "up", "2025-11-01", "2025-11-09"),
    ("MMT listing pump", "MMTUSDT", "up", "2025-11-04", "2025-11-05"),
    ("MMT listing dump", "MMTUSDT", "dn", "2025-11-04", "2025-11-07"),
    ("POPCAT (HL wall pull)", "POPCATUSDT", "dn", "2025-11-11", "2025-11-13"),
    ("PIPPIN squeeze", "PIPPINUSDT", "up", "2025-11-20", "2025-12-11"),
    ("BEAT first leg", "BEATUSDT", "up", "2025-12-01", "2025-12-31"),
    ("RIVER squeeze", "RIVERUSDT", "up", "2025-12-15", "2026-01-27"),
    ("LIGHT crash 1", "LIGHTUSDT", "dn", "2025-12-19", "2025-12-23"),
    ("LIGHT pump 2", "LIGHTUSDT", "up", "2025-12-30", "2026-01-02"),
    ("LIGHT crash 2", "LIGHTUSDT", "dn", "2026-01-02", "2026-01-04"),
    ("BROCCOLI714 spike", "BROCCOLI714USDT", "up", "2025-12-31", "2026-01-02"),
    ("TANSSI delisting (weak)", "TANSSIUSDT", "up", "2026-01-10", "2026-01-21"),
    ("SIREN squeeze", "SIRENUSDT", "up", "2026-03-08", "2026-03-23"),
    ("TRADOOR pump", "TRADOORUSDT", "up", "2026-03-01", "2026-04-23"),
    ("STO float squeeze", "STOUSDT", "up", "2026-03-30", "2026-04-08"),
    ("FARTCOIN (HL attack)", "FARTCOINUSDT", "up", "2026-04-08", "2026-04-10"),
    ("RAVE pump", "RAVEUSDT", "up", "2026-04-05", "2026-04-19"),
    ("RAVE collapse", "RAVEUSDT", "dn", "2026-04-17", "2026-04-21"),
    ("TRADOOR collapse", "TRADOORUSDT", "dn", "2026-04-22", "2026-04-25"),
    ("BEAT second leg", "BEATUSDT", "up", "2026-06-05", "2026-06-12"),
    ("SIREN collapse", "SIRENUSDT", "dn", "2026-06-11", "2026-06-16"),
    ("TRB pump (2023)", "TRBUSDT", "up", "2023-12-27", "2024-01-01"),
    ("TRB dump (2023)", "TRBUSDT", "dn", "2023-12-31", "2024-01-02"),
]
NOT_TESTABLE = [  # documented cases the Binance archive cannot test before the event
    ("JELLYJELLY squeeze 2025-03-26", "Hyperliquid perp; Binance listed its perp only after the squeeze"),
    ("XPL pre-market sweep 2025-08-26", "Hyperliquid pre-market hyperp; no archive history"),
    ("XPL on Aster 2025-09-25", "Aster config error; no Aster archive"),
    ("KOGE / BR (Alpha pools)", "on-chain PancakeSwap pools; LP removal is on-chain data"),
    ("USDe/wBETH/BNSOL, ATOM 2025-10-10", "Binance spot depeg / 8 ms print; not an operator move"),
    ("TAIKO Korean cage pump 2026-06", "KRW spot venues; no perp signal"),
]

# ------------------------------------------------------------------------------------------ detectors
# family: (play direction, kind, needs, definition, threshold grid). needs "oi" = 5m metrics (OI and
# long/short ratios), evaluated on the random sample only; "spot" = a Binance spot market.
FAMS = {
    "negfund_up": ("up", "detector", "", "settled hourly funding minus the market median <= min(-thr, coin p5 30d) "
                   "for >= 3 settlements in a row, and 24h return >= max(10%, 2 daily sd)",
                   [{"thr": 5e-5}, {"thr": 2e-4}, {"thr": 5e-4}]),
    "negfund_oi": ("up", "detector", "oi", "negfund_up and 24h OI change >= +15% (shorts adding)",
                   [{"thr": 5e-5}, {"thr": 2e-4}, {"thr": 5e-4}]),
    "cut_neg": ("up", "detector", "", "funding interval shortened at a settlement, negative rate (floor hit)", [{}]),
    "cut_pos": ("dn", "detector", "", "funding interval shortened at a settlement, positive rate (cap hit)", [{}]),
    "oi_stack": ("up", "detector", "oi", "72h OI change >= g with |72h return| <= 5%, funding <= default "
                 "(0.00125%/h) and 72h taker buy share < tk", [{"g": .25, "tk": .5}, {"g": .4, "tk": .5}, {"g": .4, "tk": .45}]),
    "top_oi_down": ("dn", "detector", "oi", "7d run-up >= g, the 7d high made within the last 4h, OI down in >= 3 "
                    "of the last 4 hours and <= -10% over 4h", [{"g": .3}, {"g": .5}, {"g": 1.0}]),
    "drift": ("up", "detector", "", "1h return > mean + z sd of the previous 12 hours and 1h quote volume > m x "
              "their mean (Kamps)", [{"z": 2, "m": 2}, {"z": 3, "m": 3}, {"z": 4, "m": 5}]),
    "staircase": ("up", "detector", "", ">= 4 of the last 5 days up, close drawdown from the running high <= 10% "
                  "over 5 days, 5d return >= g, last 60h volume below the 60h before", [{"g": .15}, {"g": .3}, {"g": .5}]),
    "wicks2": ("any", "detector", "", ">= 2 upper-wick and >= 2 lower-wick bars in the last 24h (wick >= 2x body, "
               "range >= m x the coin's median 1h range)", [{"m": 2}, {"m": 3}, {"m": 4}]),
    "ls_squeeze": ("up", "detector", "oi", "global account L/S <= coin p_lo (30d), top-trader position L/S >= p_hi "
                   "and funding < 0", [{"lo": .05, "hi": .95}, {"lo": .1, "hi": .9}, {"lo": .2, "hi": .8}]),
    "ls_top": ("dn", "detector", "oi", "global account L/S >= coin p_hi (30d) while top-trader position L/S fell "
               "3 hours in a row", [{"hi": .8}, {"hi": .9}, {"hi": .95}]),
    "retrace_short": ("dn", "detector", "", "spike-retrace (Odaily's surviving setup, at 1h): 96h run-up >= g with the "
                      "96h high inside the last 12h, then an hour with taker sells > buys closing below the previous "
                      "3 hours' lows", [{"g": .2}, {"g": .5}, {"g": 1.0}]),
    "prem_disc": ("up", "detector", "","premium index <= min(-thr, coin p5 30d) for 2 hourly closes while the "
                  "index rose over 24h", [{"thr": .002}, {"thr": .003}, {"thr": .006}]),
    "prem_top": ("dn", "detector", "", "premium index >= max(thr, coin p99 30d) with funding >= coin p95 and > 0",
                 [{"thr": .005}, {"thr": .01}, {"thr": .02}]),
    "perp_dom": ("any", "detector", "spot", "24h perp / Binance spot quote volume >= max(R, coin p95 30d)",
                 [{"R": 10}, {"R": 15}, {"R": 30}]),
    "vol_oi": ("any", "detector", "oi", "24h quote volume / OI value >= m x coin 30d median", [{"m": 3}, {"m": 5}, {"m": 10}]),
    "B_gainer": ("up", "baseline", "", "naive: 24h return >= g", [{"g": .15}, {"g": .25}, {"g": .4}]),
    "B_volspike": ("up", "baseline", "", "naive: 1h quote volume >= m x coin 30d median", [{"m": 5}, {"m": 10}, {"m": 20}]),
    "B_runup": ("dn", "baseline", "", "naive: 7d run-up from the 7d low >= g", [{"g": .5}, {"g": 1.0}, {"g": 2.0}]),
    # structural states, used only as AND filters on the naive triggers (never alerts by themselves)
    "P_nospot": ("any", "prior", "", "no Binance spot market traded in the last 24h", [{}]),
    "P_repeat": ("any", "prior", "", ">= 2 completed episodes (+50% within 96h, then -30% from the peak within 7d) "
                 "in the last 90 days", [{}]),
    "P_young": ("any", "prior", "", "perp listed less than 30 days ago", [{}]),
}
SINGLES = [(f, g) for f in FAMS for g in range(len(FAMS[f][4]))]
ALERTS = [f for f in FAMS if FAMS[f][1] != "prior"]


def key(f, g): return f"{f}:{g}"
def play(f): return {"up": ("up", 24, .3), "dn": ("dn", 24, .3), "any": ("any", 24, .3)}[FAMS[f][0]]

# ------------------------------------------------------------------------------------------- network


class Blocked(Exception):
    pass


class Net:
    """Kept-alive HTTPS per thread and host; counts bytes; never retries 403/418/429/451."""

    def __init__(self):
        self.tl, self.lock, self.bytes, self.files, self.miss = threading.local(), threading.Lock(), 0, 0, 0
        self.stop = threading.Event()
        self.offline = False   # --offline: analysis on what is cached, no request at all

    def get(self, host, path):
        for k in range(6):
            if self.stop.is_set():
                raise Blocked("stopped")
            conns = self.tl.__dict__
            c = conns.get(host) or conns.setdefault(host, http.client.HTTPSConnection(host, timeout=60))
            try:
                c.request("GET", path, headers={"User-Agent": "depth-althunt/1"})
                r = c.getresponse()
                body = r.read()
            except (OSError, http.client.HTTPException):
                c.close()
                conns.pop(host, None)
                time.sleep(min(30, 2 ** k))
                continue
            if r.status in (200, 404):
                return r.status, body
            if r.status in (403, 418, 429, 451):
                self.stop.set()
                raise Blocked(f"{host} answered HTTP {r.status} for {path}: stopping all downloads")
            time.sleep(min(30, 2 ** k))
        raise RuntimeError(f"giving up on https://{host}{path}")


NET = Net()


def cached(host, path):
    """Local copy of https://host/path (downloaded once); None when the file does not exist."""
    p = RAW / host / path
    if p.exists():
        return p
    miss = p.with_name(p.name + ".404")
    if miss.exists() or NET.offline:
        return None
    st, body = NET.get(host, "/" + path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with NET.lock:
        if st == 404:
            NET.miss += 1
        else:
            NET.bytes += len(body)
            NET.files += 1
    if st == 404:
        miss.touch()
        return None
    tmp = p.with_name(p.name + ".part")
    tmp.write_bytes(body)
    tmp.replace(p)
    return p


def download(jobs, n_jobs, label):
    jobs = [j for j in dict.fromkeys(jobs) if not (RAW / j[0] / j[1]).exists()
            and not (RAW / j[0] / (j[1] + ".404")).exists()]
    print(f"{label}: {len(jobs)} files to download{' (skipped: --offline)' if NET.offline and jobs else ''}", flush=True)
    if not jobs or NET.offline:
        return
    t0, done = time.time(), 0
    with ThreadPoolExecutor(n_jobs) as ex:
        futs = [ex.submit(cached, h, p) for h, p in jobs]
        try:
            for f in futs:
                f.result()
                done += 1
                if done % 500 == 0 or done == len(jobs):
                    print(f"  {done}/{len(jobs)}  {NET.bytes / 1e6:.0f} MB  {NET.miss} missing  "
                          f"{time.time() - t0:.0f}s", flush=True)
        except Blocked as e:
            NET.stop.set()
            ex.shutdown(cancel_futures=True)
            sys.exit(str(e))


def s3_list(prefix, delim=False):
    """Keys (or sub-prefixes with delim) under prefix in the archive bucket."""
    out, marker = [], ""
    while True:
        q = "prefix=" + prefix + ("&delimiter=/" if delim else "") + (f"&marker={marker}" if marker else "")
        st, body = NET.get(S3, "/data.binance.vision?" + q)
        x = body.decode()
        got = re.findall(r"<Prefix>([^<]+)</Prefix>", x)[1:] if delim else re.findall(r"<Key>([^<]+)</Key>", x)
        out += got
        if "<IsTruncated>true</IsTruncated>" not in x or not got:
            return out
        m = re.search(r"<NextMarker>([^<]+)</NextMarker>", x)
        marker = m[1] if m else got[-1]

# --------------------------------------------------------------------------------------------- files


def months(a, b):
    y, m = map(int, a.split("-")[:2])
    out = []
    while f"{y:04d}-{m:02d}" <= b[:7]:
        out.append(f"{y:04d}-{m:02d}")
        y, m = (y + 1, 1) if m == 12 else (y, m + 1)
    return out


def kl(s, m, iv="1h"): return BV, f"data/futures/um/monthly/klines/{s}/{iv}/{s}-{iv}-{m}.zip"
def pk(s, m): return BV, f"data/futures/um/monthly/premiumIndexKlines/{s}/1h/{s}-1h-{m}.zip"
def fr(s, m): return BV, f"data/futures/um/monthly/fundingRate/{s}/{s}-fundingRate-{m}.zip"
def skl(s, m): return BV, f"data/spot/monthly/klines/{s}/1h/{s}-1h-{m}.zip"
def mt(s, d): return BV, f"data/futures/um/daily/metrics/{s}/{s}-metrics-{d}.zip"
def bd(s, d): return BV, f"data/futures/um/daily/bookDepth/{s}/{s}-bookDepth-{d}.zip"
def gk(base, m): ym = m.replace("-", ""); return GATE, f"futures_usdt/candlesticks_1m/{ym}/{base}_USDT-{ym}.csv.gz"
def local(job): return RAW / job[0] / job[1]


def zrows(p):
    if str(p).endswith(".gz"):
        txt = gzip.open(p).read().decode()
    else:
        with zipfile.ZipFile(p) as z:
            txt = z.read(z.namelist()[0]).decode()
    rows = list(csv.reader(io.StringIO(txt)))
    return rows[1:] if rows and rows[0] and not rows[0][0][:1].isdigit() else rows


def t13(x):
    t = int(x)
    return t // 1000 if t > 10 ** 14 else t


def tradfi():
    try:
        return set(re.findall(r'\("([A-Z0-9]+)", Exchange::', (REPO / "src/alias.rs").read_text()))
    except OSError:
        return set()


def universe(relist=False):
    f = WORK / "universe.json"
    if f.exists() and not relist:
        return json.loads(f.read_text())
    excl = EXCLUDE | tradfi()
    perp = defaultdict(list)
    for k in s3_list("data/futures/um/monthly/fundingRate/"):
        m = re.match(r".*/fundingRate/([A-Z0-9]+)/\1-fundingRate-(\d{4}-\d{2})\.zip$", k)
        if m and m[1].endswith("USDT") and m[1][:-4] not in excl:
            perp[m[1]].append(m[2])
    spot = sorted(p.rstrip("/").split("/")[-1] for p in s3_list("data/spot/monthly/klines/", delim=True))
    u = {"listed": stamp(time.time() * 1000), "perp": {s: sorted(v) for s, v in sorted(perp.items())}, "spot": spot}
    WORK.mkdir(parents=True, exist_ok=True)
    f.write_text(json.dumps(u))
    return u


def spot_of(sym, spots):
    if sym in spots:
        return sym
    m = re.match(r"(1000000|100000|10000|1000|1M)(.+)", sym)
    return m[2] if m and m[2] in spots else None


def life(sym):
    d = RAW / BV / f"data/futures/um/monthly/klines/{sym}/1h"
    fs = sorted(d.glob("*.zip")) if d.exists() else []
    if not fs:
        return None
    a, b = zrows(fs[0]), zrows(fs[-1])
    return (t13(a[0][0]), t13(b[-1][0])) if a and b else None


def case_span(c):
    return ms(c[3]) - 100 * DAY, ms(c[4]) + 8 * DAY


def days(a, b):
    return [day(t) for t in range(a // DAY * DAY, b + 1, DAY)]


def sample_set(u, args):
    f = WORK / "sample.json"
    if f.exists():
        s = json.loads(f.read_text())
        if s["seed"] == args.seed and s["n"] == args.sample and s["from"] == args.mfrom and s["to"] == args.mto:
            return set(s["syms"])
    pool = sorted(s for s, v in u["perp"].items() if any(args.mfrom <= m <= args.mto for m in v))
    syms = sorted(random.Random(args.seed).sample(pool, min(args.sample, len(pool))))
    f.write_text(json.dumps({"seed": args.seed, "n": args.sample, "from": args.mfrom, "to": args.mto, "syms": syms}))
    return set(syms)


def cmd_fetch(args):
    u = universe(args.relist)
    spots = set(u["spot"])
    perp = u["perp"]
    print(f"universe: {len(perp)} USDT perps in the archive (listed {u['listed']}), {len(spots)} spot symbols")
    only = set(args.only.split(",")) if args.only else None
    if only:
        perp = {s: v for s, v in perp.items() if s in only}
    extra = defaultdict(set)
    for c in CASES:
        a, b = case_span(c)
        extra[c[1]] |= set(months(day(a), day(b)))
    missing = sorted({c[1] for c in CASES if c[1] not in u["perp"]})
    if missing:
        print("case symbols not in the archive:", ", ".join(missing))
    jobs = []
    for s, ms_ in perp.items():
        sp = spot_of(s, spots)
        for m in ms_:
            if args.mfrom <= m <= args.mto or m in extra.get(s, ()):
                jobs += [kl(s, m), pk(s, m), fr(s, m)] + ([skl(sp, m)] if sp else [])
    download(jobs, args.jobs, "tier 1 (1h klines, premium, funding, spot 1h)")
    smp = sample_set(u, args)
    y, m = map(int, args.mto.split("-"))
    lo, hi = ms(args.mfrom + "-01"), ms(f"{y + m // 12:04d}-{m % 12 + 1:02d}-01") - 1
    jobs = []
    for s in sorted(smp & set(perp)):
        lf = life(s)
        if lf:
            jobs += [mt(s, d) for d in days(max(lo, lf[0]), min(hi, lf[1]))]
    for c in CASES:
        lf = life(c[1]) if c[1] in perp else None
        if lf:
            a, b = case_span(c)
            jobs += [mt(c[1], d) for d in days(max(a, lf[0]), min(b, lf[1]))]
    download(jobs, args.jobs, f"tier 2 (5m metrics: {len(smp)} sampled coins + case windows)")
    download([j for j in xv_jobs(u) if not only or any(f"/{s}" in j[1] or f"/{s[:-4]}_USDT-" in j[1] for s in only)], args.jobs,
             "cross-venue (Gate + Binance 1m candles)")
    print(f"downloaded this run: {NET.files} files, {NET.bytes / 1e6:.1f} MB; cache {du(RAW)[0] / 1e6:.0f} MB in {du(RAW)[1]} files")


def du(p):
    fs = [f for f in p.rglob("*") if f.is_file() and not f.name.endswith(".404")] if p.exists() else []
    return sum(f.stat().st_size for f in fs), len(fs)

# -------------------------------------------------------------------------------------------- series


def series(sym):
    """Hourly arrays for one perp, aligned on the hour (index 0 = first kline hour), rebuilt from the
    cached zips on every run (about 0.3 s for a coin with a year of 5m metrics)."""
    roots = [f"data/futures/um/monthly/klines/{sym}/1h", f"data/futures/um/monthly/premiumIndexKlines/{sym}/1h",
             f"data/futures/um/monthly/fundingRate/{sym}", f"data/futures/um/daily/metrics/{sym}"]
    u = json.loads((WORK / "universe.json").read_text())
    sp = spot_of(sym, set(u["spot"]))
    if sp:
        roots.append(f"data/spot/monthly/klines/{sp}/1h")
    files = {r: sorted((RAW / BV / r).glob("*.zip")) if (RAW / BV / r).exists() else [] for r in roots}
    k = {}
    for f in files[roots[0]]:
        for r in zrows(f):
            v = (float(r[1]), float(r[2]), float(r[3]), float(r[4]), float(r[7]), float(r[10]))
            if v[4] > 0 or v[1] != v[2]:   # a settled / delisted perp keeps printing flat zero-volume bars
                k[t13(r[0]) // HOUR] = v
    if not k:
        return None
    h0, h1 = min(k), max(k)
    n = h1 - h0 + 1
    S = {"sym": sym, "h0": h0, "n": n, "spot": bool(sp)}
    for j, name in enumerate(("o", "h", "l", "c", "qv", "tb")):
        a = [None] * n
        for hk, v in k.items():
            a[hk - h0] = v[j]
        S[name] = a

    def put(name, hk, v):
        if 0 <= hk - h0 < n:
            S[name][hk - h0] = v
    for name in ("prem", "oi", "oiv", "ta", "tp", "ga", "tk", "sqv"):
        S[name] = [None] * n
    for f in files[roots[1]]:
        for r in zrows(f):
            put("prem", t13(r[0]) // HOUR, float(r[4]))
    fund = []
    for f in files[roots[2]]:
        for r in zrows(f):
            if len(r) >= 3 and r[2] != "":
                fund.append((int(r[0]), float(r[1]) if r[1] else None, float(r[2])))
            elif len(r) == 2:
                fund.append((int(r[0]), None, float(r[1])))
    fund.sort()
    for j, (t, ih, rate) in enumerate(fund):   # older files carry no interval: infer it from the gap
        if ih is None:
            gap = (t - fund[j - 1][0]) / HOUR if j else 8
            fund[j] = (t, max(1, round(gap)), rate)
    S["fund"] = fund
    datecache = {}
    for f in files[roots[3]]:
        for r in zrows(f):
            try:
                d = datecache.get(r[0][:10]) or datecache.setdefault(r[0][:10], ms(r[0][:10]))
                hk = (d + int(r[0][11:13]) * HOUR) // HOUR
                oi = float(r[2])
                if oi <= 0:
                    continue   # junk rows (after delisting, lost buckets) are missing data, not zero
                vals = (oi, float(r[3]), float(r[4] or "nan"), float(r[5] or "nan"), float(r[6] or "nan"),
                        float(r[7] or "nan"))
            except (ValueError, IndexError):
                continue
            for name, v in zip(("oi", "oiv", "ta", "tp", "ga", "tk"), vals):
                put(name, hk, v if v == v and v > 0 else None)
    for f in files.get(roots[4], []) if sp else []:
        for r in zrows(f):
            put("sqv", t13(r[0]) // HOUR, float(r[7]))
    return S


def med_funding(u, procs):
    """Cross-sectional median of hourly funding at every 8h settlement boundary (all coins settle
    there whatever their interval), carried forward; removes market-wide funding regimes."""
    f = WORK / "medfund.json"
    nfiles = len(list((RAW / BV / "data/futures/um/monthly/fundingRate").glob("*/*.zip")))
    if f.exists():
        d = json.loads(f.read_text())
        if d["nfiles"] == nfiles:
            return {int(k): v for k, v in d["med"].items()}
    slots = defaultdict(list)
    with Pool(procs) as p:
        for vals in p.imap_unordered(_fund_slots, list(u["perp"]), chunksize=8):
            for s, v in vals:
                slots[s].append(v)
    med = {s: statistics.median(v) for s, v in slots.items() if len(v) >= 20}
    f.write_text(json.dumps({"nfiles": nfiles, "med": med}))
    return med


def _fund_slots(sym):
    fs = sorted((RAW / BV / f"data/futures/um/monthly/fundingRate/{sym}").glob("*.zip"))
    fund = sorted((int(r[0]), float(r[1]) if len(r) >= 3 and r[1] else 8.0, float(r[-1])) for f in fs for r in zrows(f)
                  if r and r[-1] != "")
    out, j, last = [], 0, None
    if not fund:
        return out
    slot = 8 * HOUR
    for b in range(fund[0][0] // slot * slot + slot, fund[-1][0] + 1, slot):
        while j < len(fund) and fund[j][0] <= b + 60_000:
            last = fund[j][2] / fund[j][1]
            j += 1
        if last is not None:
            out.append((b // slot, last))
    return out

# ------------------------------------------------------------------------------------------ features


class Roll:
    """Values of the trailing `span` (same unit as t), sorted for percentiles; query before add()."""
    __slots__ = ("span", "least", "q", "s")

    def __init__(self, span, least):
        self.span, self.least, self.q, self.s = span, least, deque(), []

    def trim(self, t):
        while self.q and self.q[0][0] <= t - self.span:
            self.s.pop(bisect.bisect_left(self.s, self.q.popleft()[1]))

    def add(self, t, x):
        if x is not None:
            self.q.append((t, x))
            bisect.insort(self.s, x)

    def pct(self, p):
        n = len(self.s)
        return self.s[min(n - 1, int(p * n))] if n >= self.least else None


def prefix(a):
    p, n = [0.0], [0]
    for x in a:
        p.append(p[-1] + (x if x is not None else 0.0))
        n.append(n[-1] + (x is not None))
    return p, n


def slide(a, w, big):
    """out[i] = max (big) / min of a[i-w+1..i], ignoring None."""
    out, dq = [None] * len(a), deque()
    for i, x in enumerate(a):
        if x is not None:
            while dq and (a[dq[-1]] <= x if big else a[dq[-1]] >= x):
                dq.pop()
            dq.append(i)
        while dq and dq[0] <= i - w:
            dq.popleft()
        out[i] = a[dq[0]] if dq else None
    return out


def fwd(a, w, big):
    """out[i] = max (big) / min of a[i+1..i+w] (partial at the end); None when nothing follows."""
    n, out, dq = len(a), [None] * len(a), deque()
    for i in range(n - 2, -1, -1):
        x = a[i + 1]
        if x is not None:
            while dq and (a[dq[-1]] <= x if big else a[dq[-1]] >= x):
                dq.pop()
            dq.append(i + 1)
        while dq and dq[0] > i + w:
            dq.popleft()
        out[i] = a[dq[0]] if dq else None
    return out


class Feat:
    pass


def features(S, med, outcomes=True):
    n, h0 = S["n"], S["h0"]
    o, h, l, c, qv, tb = S["o"], S["h"], S["l"], S["c"], S["qv"], S["tb"]
    F = Feat()
    F.n, F.h0, F.o, F.h, F.l, F.c, F.qv, F.tb, F.sym = n, h0, o, h, l, c, qv, tb, S["sym"]
    F.prem, F.oi, F.oiv, F.tp, F.ga, F.sqv = S["prem"], S["oi"], S["oiv"], S["tp"], S["ga"], S["sqv"]
    r1 = [None] * n
    for i in range(1, n):
        if c[i] is not None and c[i - 1]:
            r1[i] = c[i] / c[i - 1] - 1
    F.r1 = r1
    P1, N1 = prefix(r1)
    P2, _ = prefix([x * x if x is not None else None for x in r1])
    F.Pq, F.Nq = prefix(qv)
    F.Pt, _ = prefix(tb)
    F.Ps, F.Ns = prefix(S["sqv"])

    def sd(i, w):     # sd of r1 over [i-w+1, i]
        a = max(0, i - w + 1)
        k = N1[i + 1] - N1[a]
        if k < max(3, w // 2):
            return None
        m = (P1[i + 1] - P1[a]) / k
        return math.sqrt(max((P2[i + 1] - P2[a]) / k - m * m, 0.0))
    F.sd = sd
    F.mean = lambda i, w: ((P1[i + 1] - P1[max(0, i - w + 1)]) / max(1, N1[i + 1] - N1[max(0, i - w + 1)]))
    F.sd168 = [sd(i, 168) for i in range(n)]
    F.sd720 = [sd(i, 720) for i in range(n)]
    F.lo168, F.hi168 = slide(l, 168, False), slide(h, 168, True)
    F.lo96 = slide(l, 96, False)
    # trailing 30d percentiles (excluding the current hour)
    rq, rr, rp, rg, rt, rs, rv = (Roll(720, 168) for _ in range(7))
    F.qvmed, F.rngmed, F.pp05, F.pp99, F.ratio, F.rp95, F.volOI, F.vomed = ([None] * n for _ in range(8))
    GP = (.05, .1, .2, .8, .9, .95)
    F.gap = {p: [None] * n for p in GP}
    F.tpp = {p: [None] * n for p in GP}
    for i in range(n):
        for r in (rq, rr, rp, rg, rt, rs, rv):
            r.trim(i)
        F.qvmed[i] = rq.pct(.5)
        F.rngmed[i] = rr.pct(.5)
        F.pp05[i], F.pp99[i] = rp.pct(.05), rp.pct(.99)
        for p in GP:
            F.gap[p][i], F.tpp[p][i] = rg.pct(p), rt.pct(p)
        a = max(0, i - 23)
        p24 = F.Pq[i + 1] - F.Pq[a] if F.Nq[i + 1] - F.Nq[a] >= 20 else None
        s24 = F.Ps[i + 1] - F.Ps[a] if F.Ns[i + 1] - F.Ns[a] >= 20 else None
        F.ratio[i] = p24 / s24 if p24 is not None and s24 else None
        F.rp95[i] = rs.pct(.95)
        F.volOI[i] = p24 / F.oiv[i] if p24 is not None and F.oiv[i] else None
        F.vomed[i] = rv.pct(.5)
        rq.add(i, qv[i])
        rr.add(i, (h[i] - l[i]) / c[i] if c[i] else None)
        rp.add(i, F.prem[i])
        rg.add(i, F.ga[i])
        rt.add(i, F.tp[i])
        rs.add(i, F.ratio[i])
        rv.add(i, F.volOI[i])
    # funding: per settlement, then carried to each hour close T_i = (h0 + i + 1) h
    fund = S["fund"]
    thrs = sorted({p["thr"] for f in ("negfund_up", "negfund_oi") for p in FAMS[f][4]})
    rf = Roll(30 * DAY, 21)
    per = []
    cnt = {t: 0 for t in thrs}
    prev = None
    for t, ih, rate in fund:
        fh = rate / ih
        fx = fh - med.get((t - 60_000) // (8 * HOUR), 1.25e-5)
        rf.trim(t)
        p5, p95 = rf.pct(.05), rf.pct(.95)
        for th in thrs:
            cnt[th] = ((cnt[th] or 0) + 1 if fx <= min(-th, p5) else 0) if p5 is not None else None
        cut = 0
        if prev is not None and ih < prev[1]:
            r = rate if abs(rate) >= abs(prev[2]) else prev[2]
            cut = 1 if r > 0 else -1 if r < 0 else 0
        per.append((t, fh, fx, p95, dict(cnt), cut))
        rf.add(t, fx)
        prev = (t, ih, rate)
    F.fh, F.fx, F.fp95, F.cut = [None] * n, [None] * n, [None] * n, [0] * n
    F.fcnt = {t: [None] * n for t in thrs}
    j = 0
    for i in range(n):
        T = (h0 + i + 1) * HOUR
        while j < len(per) and per[j][0] <= T:
            if per[j][5] and per[j][0] > T - HOUR:
                F.cut[i] = per[j][5]
            j += 1
        if j:
            t, fh, fx, p95, cn, _ = per[j - 1]
            F.fh[i], F.fx[i], F.fp95[i] = fh, fx, p95
            for th in thrs:
                F.fcnt[th][i] = cn[th]
    # wicks per multiplier: counts of qualifying bars in the last 24h
    F.wu, F.wl = {}, {}
    for m in {p["m"] for p in FAMS["wicks2"][4]}:
        fu, fl = [None] * n, [None] * n
        for i in range(n):
            if c[i] is None or F.rngmed[i] is None or not c[i]:
                continue
            body, rng = abs(c[i] - o[i]), (h[i] - l[i]) / c[i]
            big = rng >= m * F.rngmed[i]
            fu[i] = big and h[i] - max(o[i], c[i]) >= 2 * body
            fl[i] = big and min(o[i], c[i]) - l[i] >= 2 * body
        F.wu[m], F.wl[m] = prefix([1.0 if x else 0.0 if x is not None else None for x in fu])[0], \
            prefix([1.0 if x else 0.0 if x is not None else None for x in fl])[0]
    # repeat offender: completed episodes (>= +50% within 96h, then -30% from the peak within 7d) in 90d
    done, i = [], 0
    while i < n:
        if c[i] is not None and F.lo96[i] and c[i] / F.lo96[i] - 1 >= 0.5:
            peak = h[i]
            for k in range(i + 1, min(n, i + 169)):
                if h[k] is None:
                    continue
                peak = max(peak, h[k])
                if l[k] <= 0.7 * peak:
                    done.append(k)
                    break
            i += 168
        else:
            i += 1
    F.rep = [0] * n
    for k in done:
        for x in range(k, min(n, k + 2160)):
            F.rep[x] += 1
    if outcomes:
        F.fmax = {w: fwd(h, w, True) for w in HORIZONS}
        F.fmin = {w: fwd(l, w, False) for w in HORIZONS}
    return F


def ret(F, i, k):
    if i < k:
        return None
    a, b = F.c[i], F.c[i - k]
    return a / b - 1 if a is not None and b else None


def rules(F):
    """{(family, grid index): state(i) -> True / False / None (inputs missing)}."""
    c, h, l, qv, oi = F.c, F.h, F.l, F.qv, F.oi
    R = {}

    def och(i, k):
        if i < k or oi[i] is None or not oi[i - k]:
            return None
        return oi[i] / oi[i - k] - 1

    def negfund(cnt, need_oi):
        def fn(i):
            k = cnt[i]
            if k is None:
                return None
            r, s = ret(F, i, 24), F.sd720[i]
            if r is None or s is None:
                return None
            if k < 3 or r < max(0.10, 2 * s * 24 ** .5):
                return False
            if need_oi:
                x = och(i, 24)
                return None if x is None else x >= 0.15
            return True
        return fn
    for g, p in enumerate(FAMS["negfund_up"][4]):
        R[("negfund_up", g)] = negfund(F.fcnt[p["thr"]], False)
        R[("negfund_oi", g)] = negfund(F.fcnt[p["thr"]], True)
    R[("cut_neg", 0)] = lambda i: (F.cut[i] < 0) if F.fh[i] is not None else None
    R[("cut_pos", 0)] = lambda i: (F.cut[i] > 0) if F.fh[i] is not None else None

    def oi_stack(p):
        def fn(i):
            x, r = och(i, 72), ret(F, i, 72)
            if x is None or r is None or F.fh[i] is None:
                return None
            if x < p["g"] or abs(r) > 0.05 or F.fh[i] > 1.25e-5:
                return False
            a = i - 71
            q = F.Pq[i + 1] - F.Pq[a]
            return q > 0 and (F.Pt[i + 1] - F.Pt[a]) / q < p["tk"]
        return fn

    def top_oi_down(p):
        def fn(i):
            if i < 4 or c[i] is None or not F.lo168[i]:
                return None
            top = max((x for x in h[i - 3:i + 1] if x is not None), default=None)
            if top is None or F.hi168[i] is None:
                return None
            if c[i] / F.lo168[i] - 1 < p["g"] or top < F.hi168[i]:
                return False
            x = och(i, 4)
            if x is None or any(oi[j] is None for j in range(i - 4, i + 1)):
                return None
            return x <= -0.10 and sum(oi[j] < oi[j - 1] for j in range(i - 3, i + 1)) >= 3
        return fn

    def drift(p):
        def fn(i):
            if i < 13 or F.r1[i] is None or qv[i] is None:
                return None
            s = F.sd(i - 1, 12)
            a = i - 12
            nq = F.Nq[i] - F.Nq[a]
            if s is None or nq < 10:
                return None
            return F.r1[i] > F.mean(i - 1, 12) + p["z"] * s and qv[i] > p["m"] * (F.Pq[i] - F.Pq[a]) / nq
        return fn

    def staircase(p):
        def fn(i):
            if i < 120 or any(c[i - 24 * k] is None for k in range(6)):
                return None
            up = sum(c[i - 24 * k] > c[i - 24 * (k + 1)] for k in range(5))
            if up < 4 or c[i] / c[i - 120] - 1 < p["g"]:
                return False
            if F.Pq[i + 1] - F.Pq[i - 59] >= F.Pq[i - 59] - F.Pq[i - 119]:
                return False
            run, dd = 0.0, 0.0
            for j in range(i - 119, i + 1):
                if c[j] is not None:
                    run = max(run, c[j])
                    dd = max(dd, 1 - c[j] / run)
            return dd <= 0.10
        return fn

    def wicks(p):
        wu, wl = F.wu[p["m"]], F.wl[p["m"]]

        def fn(i):
            if F.rngmed[i] is None:
                return None
            a = max(0, i - 23)
            return wu[i + 1] - wu[a] >= 2 and wl[i + 1] - wl[a] >= 2
        return fn

    def ls_squeeze(p):
        def fn(i):
            lo, hi = F.gap[p["lo"]][i], F.tpp[p["hi"]][i]
            if F.ga[i] is None or F.tp[i] is None or lo is None or hi is None or F.fh[i] is None:
                return None
            return F.ga[i] <= lo and F.tp[i] >= hi and F.fh[i] < 0
        return fn

    def ls_top(p):
        def fn(i):
            hi = F.gap[p["hi"]][i]
            tp = F.tp[i - 3:i + 1] if i >= 3 else []
            if F.ga[i] is None or hi is None or len(tp) < 4 or None in tp:
                return None
            return F.ga[i] >= hi and tp[3] < tp[2] < tp[1] < tp[0]
        return fn

    def retrace(p):
        def fn(i):
            if i < 96 or c[i] is None or qv[i] is None or not F.lo96[i] or None in l[i - 3:i]:
                return None
            hi96 = max((x for x in h[i - 95:i + 1] if x is not None), default=None)
            if hi96 is None or hi96 / F.lo96[i] - 1 < p["g"]:
                return False
            if max(h[j] or 0 for j in range(i - 11, i + 1)) < hi96:
                return False
            return qv[i] > 0 and F.tb[i] / qv[i] < 0.5 and c[i] < min(l[i - 3:i])
        return fn

    def prem_disc(p):
        def fn(i):
            pr = F.prem
            if i < 24 or None in (pr[i], pr[i - 1], pr[i - 24], F.pp05[i], F.pp05[i - 1], c[i], c[i - 24]):
                return None
            if not (pr[i] <= min(-p["thr"], F.pp05[i]) and pr[i - 1] <= min(-p["thr"], F.pp05[i - 1])):
                return False
            return (c[i] / (1 + pr[i])) / (c[i - 24] / (1 + pr[i - 24])) > 1
        return fn

    def prem_top(p):
        def fn(i):
            if F.prem[i] is None or F.pp99[i] is None or F.fx[i] is None or F.fp95[i] is None:
                return None
            return F.prem[i] >= max(p["thr"], F.pp99[i]) and F.fx[i] >= F.fp95[i] and F.fh[i] > 0
        return fn

    def perp_dom(p):
        return lambda i: None if F.ratio[i] is None or F.rp95[i] is None else F.ratio[i] >= max(p["R"], F.rp95[i])

    def vol_oi(p):
        return lambda i: None if F.volOI[i] is None or F.vomed[i] is None else F.volOI[i] >= p["m"] * F.vomed[i]

    def gainer(p):
        def fn(i):
            r = ret(F, i, 24)
            return None if r is None else r >= p["g"]
        return fn

    def volspike(p):
        return lambda i: None if qv[i] is None or not F.qvmed[i] else qv[i] >= p["m"] * F.qvmed[i]

    def runup(p):
        return lambda i: None if c[i] is None or not F.lo168[i] else c[i] / F.lo168[i] - 1 >= p["g"]
    R[("P_nospot", 0)] = lambda i: None if c[i] is None else F.Ns[i + 1] - F.Ns[max(0, i - 23)] == 0
    R[("P_repeat", 0)] = lambda i: F.rep[i] >= 2
    old = getattr(F, "old", False)   # listed before the downloaded window: age unknown, not young
    R[("P_young", 0)] = lambda i: not old and i < 30 * 24
    make = {"oi_stack": oi_stack, "top_oi_down": top_oi_down, "drift": drift, "staircase": staircase,
            "wicks2": wicks, "ls_squeeze": ls_squeeze, "ls_top": ls_top, "retrace_short": retrace, "prem_disc": prem_disc,
            "prem_top": prem_top, "perp_dom": perp_dom, "vol_oi": vol_oi, "B_gainer": gainer,
            "B_volspike": volspike, "B_runup": runup}
    for fam, mk in make.items():
        for g, p in enumerate(FAMS[fam][4]):
            R[(fam, g)] = mk(p)
    return R

# -------------------------------------------------------------------------------------------- outcomes


def outcome(F, i):
    """(max up at 1/4/24/72h, max down at 1/4/24/72h, close return at 24h) from the close of bar i."""
    c = F.c[i]
    up = [F.fmax[w][i] / c - 1 if F.fmax[w][i] is not None else None for w in HORIZONS]
    dn = [1 - F.fmin[w][i] / c if F.fmin[w][i] is not None else None for w in HORIZONS]
    j = min(F.n - 1, i + 24)
    r24 = F.c[j] / c - 1 if F.c[j] is not None and j > i else None
    return tuple(up + dn + [r24])


def path(F, i, tgt=0.3, stop=0.15):
    """Bracket trade from the firing close, 24h max: (long reached +tgt before -stop, short reached -tgt
    before +stop, long return, short return), returns = +tgt, -stop or the last close if neither hit.
    A bar touching both levels counts as stopped (the order inside one hour is unknown). No costs."""
    c = last = F.c[i]
    lo = sh = None
    for j in range(i + 1, min(F.n, i + 25)):
        if F.h[j] is None:
            continue
        up, dn = F.h[j] / c - 1, 1 - F.l[j] / c
        last = F.c[j]
        if lo is None and (dn >= stop or up >= tgt):
            lo = -stop if dn >= stop else tgt
        if sh is None and (up >= stop or dn >= tgt):
            sh = -stop if up >= stop else tgt
        if lo is not None and sh is not None:
            break
    r = last / c - 1
    return (lo == tgt, sh == tgt, lo if lo is not None else r, sh if sh is not None else -r)


HI = {w: k for k, w in enumerate(HORIZONS)}
NH = len(HORIZONS)
# firing tuple: sym, T, period, bucket, insample, outcome (max up / down per horizon, 24h close return),
# path (long win, short win, long bracket return, short bracket return), liquidity (4)
OUT, R24, WIN, BR, LIQ = slice(5, 6 + 2 * NH), 5 + 2 * NH, 6 + 2 * NH, 8 + 2 * NH, 10 + 2 * NH


def hit(o, tg):
    d, w, thr = tg
    u, v = o[HI[w]], o[NH + HI[w]]
    if d == "up":
        return u is not None and u >= thr
    if d == "dn":
        return v is not None and v >= thr
    return (u is not None and u >= thr) or (v is not None and v >= thr)


def bucket(x):
    return -1 if x is None else bisect.bisect(VOL_EDGES, x)


def liquidity(F, i):
    """(24h quote volume, median 1h quote volume, median |1h return| per $1M, Corwin-Schultz spread)."""
    a = max(1, i - 23)
    qs = [F.qv[j] for j in range(a, i + 1) if F.qv[j] is not None]
    imp = [abs(F.r1[j]) / (F.qv[j] / 1e6) for j in range(a, i + 1) if F.r1[j] is not None and F.qv[j]]
    k = 3 - 2 * math.sqrt(2)
    cs = []
    for j in range(a, i + 1):
        h1, l1, h2, l2 = F.h[j - 1], F.l[j - 1], F.h[j], F.l[j]
        if None in (h1, l1, h2, l2) or min(l1, l2) <= 0:
            continue
        b = math.log(h1 / l1) ** 2 + math.log(h2 / l2) ** 2
        g = math.log(max(h1, h2) / min(l1, l2)) ** 2
        al = (math.sqrt(2 * b) - math.sqrt(b)) / k - math.sqrt(g / k)
        cs.append(max(0.0, 2 * (math.exp(al) - 1) / (1 + math.exp(al))))
    md = lambda v: statistics.median(v) if v else None
    return (sum(qs), md(qs), md(imp), md(cs))

# ------------------------------------------------------------------------------------------------ scan

G = {}


def _init(med, smp, old):
    G["med"], G["smp"], G["old"] = med, smp, old


def scan_one(job):
    """One coin: base-rate counters, priors and firings for the requested keys."""
    sym, keys, base = job
    S = series(sym)
    if not S:
        return None
    F = features(S, G["med"])
    F.old = sym in G["old"]
    R = rules(F)
    insample = sym in G["smp"]
    singles = sorted({s for k in keys for s in (k if isinstance(k[0], tuple) else (k,))})
    singles = [s for s in singles if insample or FAMS[s[0]][2] != "oi"]
    keys = [k for k in keys if all(s in singles for s in (k if isinstance(k[0], tuple) else (k,)))]
    last_true = {s: -10 ** 9 for s in singles}
    last_fire = {k: -10 ** 9 for k in keys}
    fires = defaultdict(list)
    cnt = defaultdict(lambda: [0] * (len(TG) + 1))   # (pop, period, bucket) -> [hours, hits per target]
    pri = defaultdict(lambda: [0] * (len(TG) + 1))   # (prior, value, period) -> same
    old = sym in G["old"]
    end = F.h0 + F.n   # first hour after the last bar: a perp delisted before the data ends
    delisted = end * HOUR < SCAN_TO - 3 * DAY
    for i in range(F.n):
        st = {s: R[s](i) for s in singles}
        for s, v in st.items():
            if v:
                last_true[s] = i
        T = (F.h0 + i + 1) * HOUR
        if i < MIN_AGE or not SCAN_FROM <= T < SCAN_TO or F.c[i] is None or F.fmax[1][i] is None:
            continue
        per = "train" if T < SPLIT else "test"
        b = bucket(F.sd168[i])
        o = None
        if base:
            o = outcome(F, i)
            hs = [hit(o, t) for t in TG]
            pops = ("all", "sample") if insample else ("all",)
            for pop in pops:
                row = cnt[(pop, per, b)]
                row[0] += 1
                for k, x in enumerate(hs):
                    row[k + 1] += x
            a = max(0, i - 23)
            nospot = F.Ns[i + 1] - F.Ns[a] == 0
            age = 9999 if old else i // 24
            flags = (("no_spot", nospot), ("perp_dom15", nospot or (F.ratio[i] or 0) >= 15),
                     ("repeat2", F.rep[i] >= 2), ("age", "7-30d" if age < 30 else "30-90d" if age < 90 else ">90d"),
                     ("delist7", delisted and end - (F.h0 + i) <= 7 * 24))
            for name, v in flags:
                row = pri[(name, v, per)]
                row[0] += 1
                for k, x in enumerate(hs):
                    row[k + 1] += x
        for k in keys:
            if isinstance(k[0], tuple):   # AND combo: every part true within the last 24h, one true now
                ok = any(st[s] for s in k) and all(i - last_true[s] < COOLDOWN for s in k)
            else:
                ok = st[k]
            if ok and i - last_fire[k] >= COOLDOWN:
                last_fire[k] = i
                o = o or outcome(F, i)
                fires[k].append((sym, T, per, b, insample) + o + path(F, i) + liquidity(F, i))
    return dict(fires), dict(cnt), dict(pri)


def run_scan(syms, keys, base, args, med, smp, old):
    out_f, out_c, out_p = defaultdict(list), defaultdict(lambda: [0] * (len(TG) + 1)), defaultdict(lambda: [0] * (len(TG) + 1))
    t0 = time.time()
    with Pool(args.procs, initializer=_init, initargs=(med, smp, old)) as p:
        for n, res in enumerate(p.imap_unordered(scan_one, [(s, keys, base) for s in syms], chunksize=2)):
            if res:
                f, c, pr = res
                for k, v in f.items():
                    out_f[k] += v
                for dst, src in ((out_c, c), (out_p, pr)):
                    for k, v in src.items():
                        row = dst[k]
                        for j, x in enumerate(v):
                            row[j] += x
            if (n + 1) % 50 == 0:
                print(f"  {n + 1}/{len(syms)} coins  {time.time() - t0:.0f}s", flush=True)
    return out_f, out_c, out_p


def rates(cnt):
    """base[(pop, period)] -> (hours, [rate per target]); vm[(pop, period, bucket)] -> [rate per target]."""
    base, vm = {}, {}
    for (pop, per, b), row in cnt.items():
        vm[(pop, per, b)] = [x / row[0] for x in row[1:]] if row[0] else [0] * len(TG)
        tot = base.setdefault((pop, per), [0] * (len(TG) + 1))
        for j, x in enumerate(row):
            tot[j] += x
    return {k: (v[0], [x / v[0] for x in v[1:]]) for k, v in base.items() if v[0]}, vm


def ztest(k1, n1, k2, n2):
    """Two-proportion z statistic of k1/n1 over k2/n2 (0 when either side is empty)."""
    if not n1 or not n2:
        return 0.0
    pp = (k1 + k2) / (n1 + n2)
    se = math.sqrt(pp * (1 - pp) * (1 / n1 + 1 / n2))
    return (k1 / n1 - k2 / n2) / se if se else 0.0


def wilson(k, n, z=1.96):
    if n == 0:
        return (None, None, None)
    p = k / n
    d = 1 + z * z / n
    m = (p + z * z / (2 * n)) / d
    w = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (p, max(0.0, m - w), min(1.0, m + w))


CRASHES = [(ms("2025-04-06"), ms("2025-04-09")), (ms("2025-10-10"), ms("2025-10-12"))]   # market-wide


def summarize(fs, per, tg, pop, base, vm, ex_crash=False):
    fs = [x for x in fs if x[2] == per and (pop == "all" or x[4])
          and not (ex_crash and any(a <= x[1] < b for a, b in CRASHES))]
    n = len(fs)
    hours, br = base.get((pop, per), (0, [0] * len(TG)))
    j = TGI[tg]
    k = sum(hit(x[OUT], tg) for x in fs)
    p, lo, hi = wilson(k, n)
    exp = sum(vm.get((pop, per, x[3]), br)[j] for x in fs) / n if n else None
    r24 = [x[R24] for x in fs if x[R24] is not None]
    win = bk = None
    if n and tg[0] in ("up", "dn"):
        s = tg[0] == "dn"
        win = sum(x[WIN + s] for x in fs) / n
        bk = sum(x[BR + s] for x in fs) / n
    return {"n": n, "k": k, "p": p, "lo": lo, "hi": hi, "base": br[j], "exp": exp,
            "lift": p / br[j] if n and br[j] else None, "lift_vm": p / exp if n and exp else None,
            "per_cm": n / (hours / 720) if hours else None, "coins": len({x[0] for x in fs}),
            "med_r24": statistics.median(r24) if r24 else None, "win": win, "bracket": bk}


def select(fam, fires, base, vm, tg=None):
    """Grid index with the best train Wilson lower bound (>= 10 firings), else the loosest one."""
    tg = tg or play(fam)
    pop = "sample" if FAMS[fam][2] == "oi" else "all"
    best, bi = None, None
    for g in range(len(FAMS[fam][4])):
        s = summarize(fires.get((fam, g), []), "train", tg, pop, base, vm)
        if s["n"] >= 10 and (best is None or s["lo"] > best):
            best, bi = s["lo"], g
    if bi is None:
        bi = max(range(len(FAMS[fam][4])), key=lambda g: len(fires.get((fam, g), [])))
    return bi


def cmd_scan(args):
    u = universe()
    med = med_funding(u, args.procs)
    smp = sample_set(u, args)
    syms = [s for s, v in u["perp"].items() if any(args.mfrom <= m <= args.mto for m in v)]
    old = {s for s in syms if u["perp"][s][0] < args.mfrom}
    if args.only:
        syms = [s for s in syms if s in args.only.split(",")]
    singles = [s for s in SINGLES if FAMS[s[0]][1] != "prior"]
    print(f"scan pass 1: {len(syms)} coins ({len(smp & set(syms))} with metrics), {len(singles)} detector settings")
    fires, cnt, pri = run_scan(syms, singles, True, args, med, smp, old)
    base, vm = rates(cnt)
    sel = {f: select(f, fires, base, vm) for f in FAMS}
    # pass 2: AND combos of the best detectors (train lower bound) per direction, at their thresholds
    ranked = defaultdict(list)
    for f, g in sel.items():
        if FAMS[f][1] == "detector":
            pop = "sample" if FAMS[f][2] == "oi" else "all"
            s = summarize(fires.get((f, g), []), "train", play(f), pop, base, vm)
            if s["n"] >= 10:
                ranked[FAMS[f][0]].append((s["lo"], f))
    combos = []
    for d, v in ranked.items():
        top = [f for _, f in sorted(v, reverse=True)[:3]]
        combos += [tuple((f, sel[f]) for f in pair) for pair in
                   [(a, b) for x, a in enumerate(top) for b in top[x + 1:]] + ([tuple(top)] if len(top) == 3 else [])]
    # cross-direction pairs that the research proposes: squeeze funding + OI, squeeze funding + L/S
    for pair in (("negfund_up", "oi_stack"), ("negfund_up", "ls_squeeze"), ("drift", "perp_dom"), ("drift", "negfund_up")):
        t = tuple((f, sel[f]) for f in pair)
        if t not in combos:
            combos.append(t)
    # incremental value: does a signature (or a structural prior) raise the naive trigger's precision?
    naive = {"up": ("B_gainer", sel["B_gainer"]), "dn": ("B_runup", sel["B_runup"])}
    for f in FAMS:
        if FAMS[f][1] == "detector" and FAMS[f][0] in naive:
            combos.append((naive[FAMS[f][0]], (f, sel[f])))
    for b in naive.values():
        combos += [(b, ("P_nospot", 0)), (b, ("P_repeat", 0)), (b, ("P_young", 0)),
                   (b, ("P_nospot", 0), ("P_repeat", 0))]
    print(f"scan pass 2: {len(combos)} AND combinations")
    cf, _, _ = run_scan(syms, combos, False, args, med, smp, old)
    fires.update(cf)
    out = {"args": vars(args), "coins": len(syms), "sample": sorted(smp & set(syms)), "sel": sel,
           "base": {f"{k[0]}|{k[1]}": v for k, v in base.items()},
           "vm": {f"{k[0]}|{k[1]}|{k[2]}": v for k, v in vm.items()},
           "pri": {f"{k[0]}|{k[1]}|{k[2]}": v for k, v in pri.items()},
           "combos": [[list(p) for p in c] for c in combos]}
    (WORK / "fires.pkl").write_bytes(pickle.dumps(dict(fires), protocol=pickle.HIGHEST_PROTOCOL))
    out["depth"] = depth_check(fires, sel, base, vm, args)
    (WORK / "scan.json").write_text(json.dumps(out))
    print(f"scan written: {WORK / 'scan.json'}")


def depth_check(fires, sel, base, vm, args):
    """Real +-2% book depth (bookDepth archive) at a sample of firings that hit their play target."""
    picks = []
    for f, g in sel.items():
        if FAMS[f][1] != "detector":
            continue
        fs = [x for x in fires.get((f, g), []) if hit(x[OUT], play(f))]
        random.Random(1).shuffle(fs)
        picks += [(f, x) for x in fs[:12]]
    jobs = [bd(x[0], day(x[1])) for _, x in picks]
    download(jobs, args.jobs, "book depth at actionable firings")
    out = []
    for f, x in picks:
        p = cached(*bd(x[0], day(x[1])))
        if not p:
            continue
        snap, full = {}, None   # cumulative notional within -2% / +2% of the latest snapshot <= firing
        for r in zrows(p):
            try:
                t = ms(r[0][:10]) + int(r[0][11:13]) * HOUR + int(r[0][14:16]) * 60_000
                pc = float(r[1])
            except (ValueError, IndexError):
                continue
            if t > x[1]:
                break
            if pc in (-2, 2):
                if snap.get("t") != t:
                    snap = {"t": t}
                snap[pc] = float(r[3])
                if -2 in snap and 2 in snap:
                    full = dict(snap)
        if full:
            out.append({"fam": f, "sym": x[0], "t": stamp(x[1]), "bid2": full[-2], "ask2": full[2],
                        "qv24": x[LIQ], "qvh": x[LIQ + 1]})
    return out

# ----------------------------------------------------------------------------------------------- study


def pin(F, c):
    """(t0 index, extreme index, move from the t0 close) for a case window. The extreme is the highest
    high (up) / lowest low (dn) in the window; t0 is the last hour before it that still traded within
    10% of the base (lowest low before the extreme) / top (highest high before it): the decisive leg's start."""
    a = max(0, ms(c[3]) // HOUR - F.h0)
    b = min(F.n - 1, (ms(c[4]) + DAY) // HOUR - F.h0 - 1)
    if b <= a:
        return None
    idx = [i for i in range(a, b + 1) if F.c[i] is not None]
    if not idx:
        return None
    if c[2] == "up":
        ext = max(idx, key=lambda i: F.h[i])
        base = min(F.l[i] for i in idx if i <= ext)
        t0 = max(i for i in idx if i <= ext and F.l[i] <= base * 1.1)
        mag = F.h[ext] / F.c[t0] - 1
    else:
        ext = min(idx, key=lambda i: F.l[i])
        top = max(F.h[i] for i in idx if i <= ext)
        t0 = max(i for i in idx if i <= ext and F.h[i] >= top * 0.9)
        mag = 1 - F.l[ext] / F.c[t0]
    return t0, ext, mag


def cmd_study(args):
    u = universe()
    med = med_funding(u, args.procs)
    res = []
    for c in CASES:
        name, sym, d = c[:3]
        S = series(sym) if sym in u["perp"] else None
        row = {"case": name, "sym": sym, "dir": d, "window": f"{c[3]}..{c[4]}"}
        res.append(row)
        if not S:
            row["status"] = "no Binance USD-M archive data"
            continue
        F = features(S, med, outcomes=False)
        F.old = u["perp"][sym][0] < args.mfrom
        pn = pin(F, c)
        if not pn:
            row["status"] = "perp not trading in the window"
            continue
        t0, ext, mag = pn
        T = lambda i: (F.h0 + i + 1) * HOUR
        row.update({"t0": stamp(T(t0)), "ext": stamp(T(ext)), "mag": mag, "age_h": t0,
                    "test": T(t0) >= SPLIT, "has_oi": any(F.oi[i] for i in range(max(0, t0 - 72), t0 + 1))})
        R = rules(F)
        det = {}
        for s, fn in R.items():
            pre = [i for i in range(max(0, t0 - 72), t0 + 1)]
            st = [fn(i) for i in pre]
            ev = sum(x is not None for x in st) >= len(pre) / 2
            first = next((i for i, x in zip(pre, st) if x), None)
            mv = next((i for i in range(t0 + 1, ext + 1) if fn(i)), None)
            rem = None
            if mv is not None:
                rem = F.h[ext] / F.c[mv] - 1 if d == "up" else 1 - F.l[ext] / F.c[mv]
            # placebo: share of 72h blocks in [t0-90d, t0-14d] with a true state
            blocks = ev_blocks = 0
            for e in range(t0 - 90 * 24 + 72, t0 - 14 * 24, 72):
                if e - 72 < 0:
                    continue
                v = [fn(i) for i in range(e - 72, e + 1)]
                if sum(x is not None for x in v) >= 36:
                    ev_blocks += 1
                    blocks += any(v)
            det[key(*s)] = {"ev": ev, "lead": t0 - first if first is not None else None,
                            "inmove_h": mv - t0 if mv is not None else None, "remain": rem,
                            "placebo": blocks / ev_blocks if ev_blocks else None, "placebo_n": ev_blocks}
        row["det"] = det
        print(f"  {name:28s} t0 {row['t0']}  {'+' if d == 'up' else '-'}{mag:.0%}  age {t0 // 24}d", flush=True)
    xv = xv_study(u, res, args)
    (WORK / "study.json").write_text(json.dumps({"cases": res, "xv": xv}))
    print(f"study written: {WORK / 'study.json'}")

# -------------------------------------------------------------------------------------- cross venue


def xv_cases(u, res=None):
    """Cases with >= 7 days of Binance history and t0 inside the scan window: (case, sym, t0 ms)."""
    out = []
    if res:
        for r in res:
            if r.get("t0") and r.get("age_h", 0) >= MIN_AGE and SCAN_FROM <= ms(r["t0"]) + int(r["t0"][11:13]) * HOUR < SCAN_TO:
                out.append((r["case"], r["sym"], ms(r["t0"]) + int(r["t0"][11:13]) * HOUR))
        return out
    for c in CASES:   # before the study ran: month of the window start
        if SCAN_FROM <= ms(c[3]) < SCAN_TO and c[1] in u["perp"]:
            out.append((c[0], c[1], ms(c[3])))
    return out


def xv_months(t):
    return sorted({day(t - 11 * DAY)[:7], day(t)[:7]})


def xv_controls(u, sym, t, k=2):
    case_syms = {c[1] for c in CASES}
    pool = sorted(s for s, v in u["perp"].items() if day(t)[:7] in v and s not in case_syms and not s.startswith("1000"))
    return random.Random(sym + day(t)).sample(pool, min(len(pool), 8)), k


def xv_jobs(u):
    jobs = []
    for _, sym, t in xv_cases(u):
        for m in xv_months(t):
            jobs += [gk(sym[:-4], m), kl(sym, m, "1m")]
    return jobs


def minute_series(sym, m, gate):
    p = cached(*(gk(sym[:-4], m) if gate else kl(sym, m, "1m")))
    if not p:
        return {}
    if gate:
        return {int(r[0]) * 1000 // 60_000: float(r[2]) for r in zrows(p) if len(r) >= 6 and float(r[2]) > 0}
    return {t13(r[0]) // 60_000: float(r[4]) for r in zrows(p)}


def leadlag(b, g, lo, hi):
    """Hourly states over [lo, hi) from 6h windows of 1m returns: (hour ms, corr(G_t, B_t+1) - corr(B_t, G_t+1),
    corr(G_t, B_t+1), median |G/B - 1|)."""
    def corr(x, y):
        n = len(x)
        if n < 100:
            return None
        mx, my = sum(x) / n, sum(y) / n
        sx = math.sqrt(sum((a - mx) ** 2 for a in x))
        sy = math.sqrt(sum((a - my) ** 2 for a in y))
        return sum((a - mx) * (c - my) for a, c in zip(x, y)) / (sx * sy) if sx and sy else None
    out = []
    for e in range(lo // HOUR * 60 + 360, hi // 60_000, 60):
        rb, rg, gap = {}, {}, []
        for t in range(e - 361, e):
            if t in b and t + 1 in b and t in g and t + 1 in g:
                rb[t + 1], rg[t + 1] = b[t + 1] / b[t] - 1, g[t + 1] / g[t] - 1
                gap.append(abs(g[t + 1] / b[t + 1] - 1))
        ts = [t for t in rb if t + 1 in rb]
        cgb = corr([rg[t] for t in ts], [rb[t + 1] for t in ts])
        cbg = corr([rb[t] for t in ts], [rg[t + 1] for t in ts])
        if cgb is None or cbg is None:
            continue
        out.append((e * 60_000, cgb - cbg, cgb, statistics.median(gap)))
    return out


def xv_states(ll):
    """gate_lead: Gate leads by >= 0.1 correlation and corr >= 0.15 in two consecutive windows;
    dispersion: median Gate/Binance gap >= 3x its trailing 7d median and >= 0.5%."""
    st, gaps = [], deque()
    prev = False
    for t, d, cgb, gap in ll:
        lead = d >= 0.1 and cgb >= 0.15
        while gaps and gaps[0][0] <= t - 7 * DAY:
            gaps.popleft()
        m = statistics.median(x for _, x in gaps) if len(gaps) >= 48 else None
        disp = m is not None and gap >= max(0.005, 3 * m)
        st.append((t, lead and prev, disp))
        prev = lead
        gaps.append((t, gap))
    return st


def xv_study(u, res, args):
    rows = []
    jobs, plan = [], []
    for case, sym, t0 in xv_cases(u, res):
        ctrl, k = xv_controls(u, sym, t0)
        plan.append((case, sym, t0, ctrl, k))
        for m in xv_months(t0):
            jobs += [gk(sym[:-4], m), kl(sym, m, "1m")]
    download(jobs, args.jobs, "cross-venue case candles")
    for case, sym, t0, ctrl, k in plan:
        b, g = {}, {}
        for m in xv_months(t0):
            b.update(minute_series(sym, m, False))
            g.update(minute_series(sym, m, True))
        if not g:
            rows.append({"case": case, "sym": sym, "status": "no Gate perp candles"})
            continue
        st = xv_states(leadlag(b, g, t0 - 11 * DAY, t0 + HOUR))
        pre = [x for x in st if t0 - 72 * HOUR <= x[0] <= t0]
        r = {"case": case, "sym": sym, "hours": len(pre),
             "lead_any": any(x[1] for x in pre), "disp_any": any(x[2] for x in pre),
             "lead_share": sum(x[1] for x in pre) / len(pre) if pre else None,
             "disp_share": sum(x[2] for x in pre) / len(pre) if pre else None, "controls": []}
        got = 0
        for cs in ctrl:   # matched-month controls with both venues; a 404 costs one request
            if got >= k:
                break
            m = day(t0)[:7]
            gc = minute_series(cs, m, True)
            if not gc:
                continue
            bc = minute_series(cs, m, False)
            sc = xv_states(leadlag(bc, gc, ms(m + "-01"), ms(m + "-01") + 30 * DAY))
            blocks = [sc[j:j + 72] for j in range(7 * 24, len(sc) - 72, 72)]
            if not blocks:
                continue
            got += 1
            r["controls"].append({"sym": cs, "blocks": len(blocks),
                                  "lead_rate": sum(any(x[1] for x in bl) for bl in blocks) / len(blocks),
                                  "disp_rate": sum(any(x[2] for x in bl) for bl in blocks) / len(blocks)})
        rows.append(r)
        print(f"  xv {case:28s} gate lead {r['lead_any']}  dispersion {r['disp_any']}  controls {len(r['controls'])}", flush=True)
    return rows

# ---------------------------------------------------------------------------------------------- report


def pct(x, d=0):
    return "-" if x is None else f"{100 * x:.{d}f}%"


def num(x, d=1):
    return "-" if x is None else f"{x:.{d}f}"


def money(x):
    if x is None:
        return "-"
    return f"${x / 1e6:.1f}M" if x >= 1e6 else f"${x / 1e3:.0f}k"


def cmd_report(args):
    sc = json.loads((WORK / "scan.json").read_text())
    stf = WORK / "study.json"
    st = json.loads(stf.read_text()) if stf.exists() else {"cases": [], "xv": []}
    fires = pickle.loads((WORK / "fires.pkl").read_bytes())
    base = {tuple(k.split("|")): v for k, v in sc["base"].items()}
    vm = {}
    for k, v in sc["vm"].items():
        a, b, c = k.split("|")
        vm[(a, b, int(c))] = v
    sel = {f: int(g) for f, g in sc["sel"].items()}
    cases = [c for c in st["cases"] if "det" in c]
    L = []
    w = L.append
    sumy = lambda k, per, tg, pop: summarize(fires.get(k, []), per, tg, pop, base, vm)
    pop_of = lambda f: "sample" if FAMS[f][2] == "oi" else "all"

    def recall(f, g, test=None):
        rel = [c for c in cases if (test is None or c["test"] == test) and (FAMS[f][0] == "any" or c["dir"] == FAMS[f][0])]
        ev = [c for c in rel if c["det"][key(f, g)]["ev"]]
        leads = [c["det"][key(f, g)]["lead"] for c in ev if c["det"][key(f, g)]["lead"] is not None]
        return len(leads), len(ev), (statistics.median(leads) if leads else None)

    def best_baseline(tg, per, pop):
        bests = []
        for b in ("B_gainer", "B_volspike", "B_runup"):
            g = select(b, fires, base, vm, tg)
            s = sumy((b, g), per, tg, pop)
            bests.append((s["p"] or 0, b, s["n"]))
        return max(bests)

    def beats(te, bp, bn_n):
        """One-sided two-proportion z-test, detector precision > baseline precision at 95%."""
        return ztest(te["k"], te["n"], bp * bn_n, bn_n) >= 1.645

    verdict = {}
    rows = []
    for f, (d, kind, needs, desc, grid) in ((f, FAMS[f]) for f in ALERTS):
        g = sel[f]
        tg = play(f)
        tr, te = sumy((f, g), "train", tg, pop_of(f)), sumy((f, g), "test", tg, pop_of(f))
        rk_tr, rn_tr, _ = recall(f, g, False)
        rk_te, rn_te, _ = recall(f, g, True)
        _, _, lead = recall(f, g)
        bp, bn, bnn = best_baseline(tg, "test", pop_of(f))
        over_base = te["lo"] is not None and te["exp"] is not None and te["lo"] > te["exp"]
        if kind == "baseline":
            v = "baseline"
        elif te["n"] < 20 or te["coins"] < 5:
            v = f"insufficient data ({te['n']} test firings on {te['coins']} coins)"
        elif over_base and beats(te, bp, bnn):
            v = "SHIP"
        elif over_base:
            v = f"beats the vol-matched base rate, not naive {bn}"
        else:
            v = "drop"
        verdict[f] = v
        rows.append((f, g, d, kind, needs, tr, te, f"{rk_tr}/{rn_tr}, {rk_te}/{rn_te}", lead, v, bp, bn))

    u = universe()
    w("# Alt hunter backtest: operator-coin signatures on Binance USD-M alt perps\n")
    w(f"Generated by `scripts/althunt.py` on {stamp(time.time() * 1000)} UTC. Train = hours before 2025-07-01 "
      f"(thresholds chosen there), test = 2025-07-01 .. 2026-09-30 (reported, never tuned). Universe: "
      f"{sc['coins']} Binance USD-M USDT alt perps that traded in the window (delisted ones included; BTC, ETH, "
      f"stable, index and tradfi perps excluded); OI / long-short detectors on a random sample of "
      f"{len(sc['sample'])} coins (seed {sc['args']['seed']}). Cache: {du(RAW)[0] / 1e6:.0f} MB in {du(RAW)[1]:,} archive files under "
      f"`~/Library/Caches/Depth/althunt`.\n")
    w("Play target: max move from the firing hour's close of >= 30% within 24h in the play's direction "
      "(up for pump / squeeze detectors, down for top / dump detectors, either for regime detectors). "
      "Precision is the share of firings that reached it; *base* is the rate over all coin-hours of the same "
      "population and period; *exp* is the volatility-matched base rate (same 7d realized-vol bucket as each "
      "firing), so lift_vm > 1 means the detector knows more than \"this coin is volatile\". A firing is a true "
      "state >= 24h after that detector's previous firing on the coin. Intervals are Wilson 95%.\n")
    w("Verdict rule (test period only): SHIP = >= 20 firings on >= 5 coins, Wilson lower bound above the "
      "vol-matched base rate, and precision above the best naive baseline on the same target (one-sided "
      "two-proportion z >= 1.645). Naive baselines: chase the 24h gainer, a 1h volume spike, or (down) "
      "anything that ran up over 7 days; each baseline's threshold is chosen on train like the detectors'.\n")
    w("## Headline (play target, selected threshold)\n")
    w("| detector | dir | thr | cases hit / evaluable (train, test) | median lead h | test firings / coin-month | "
      "train prec (n) | test prec [95% CI] (n firings / coins) | test base | test exp (vol-matched) | "
      "test lift / lift_vm | test target before stop / bracket mean | best naive (test) | verdict |")
    w("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for f, g, d, kind, needs, tr, te, rc, lead, v, bp, bn in rows:
        grid = FAMS[f][4][g]
        w(f"| `{f}`{' (OI sample)' if needs == 'oi' else ''} | {d} | {json.dumps(grid) if grid else '-'} | "
          f"{rc} | {num(lead, 0)} | {num(te['per_cm'], 2)} | {pct(tr['p'], 1)} ({tr['n']}) | "
          f"{pct(te['p'], 1)} [{pct(te['lo'], 1)}, {pct(te['hi'], 1)}] ({te['n']} / {te['coins']}) | "
          f"{pct(te['base'], 2)} | {pct(te['exp'], 2)} | {num(te['lift'])} / {num(te['lift_vm'])} | "
          f"{pct(te['win'], 1)} / {pct(te['bracket'], 1)} | {pct(bp, 1)} {bn} | {v} |")
    w("")
    w("Cases hit = the state was true at least once in [t0 - 72h, t0] (t0 = the move's start pinned from 1h "
      "bars); evaluable = the detector had its inputs for at least half of that window. Lead = t0 minus the "
      "first true hour in the window. *Target before stop* = share of firings where a position in the play's "
      "direction, opened at the firing close, reached +30% before moving 15% against it within 24h (a bar "
      "touching both counts as stopped); *bracket mean* = that trade's average return (+30%, -15%, or the 24h "
      "close when neither level is hit), before fees, funding and slippage. Blank for `any` detectors.\n")
    w("## All targets on test data (selected threshold)\n")
    w("Cells: precision (lift over the vol-matched base rate).\n")
    w("| detector | n | up 30% 1h | up 30% 4h | up 30% 24h | up 30% 72h | down 30% 4h | down 30% 24h | down 30% 72h | "
      "up 15% 24h | median 24h close return | play target without the 2025-10-10 crash days (n) |")
    w("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for f in ALERTS:
        g = sel[f]
        cells = [sumy((f, g), "test", t, pop_of(f)) for t in
                 (("up", 1, .3), ("up", 4, .3), ("up", 24, .3), ("up", 72, .3), ("dn", 4, .3), ("dn", 24, .3),
                  ("dn", 72, .3), ("up", 24, .15))]
        xc = summarize(fires.get((f, g), []), "test", play(f), pop_of(f), base, vm, ex_crash=True)
        w(f"| `{f}` | {cells[0]['n']} | " + " | ".join(f"{pct(x['p'], 1)} ({num(x['lift_vm'])}x)" for x in cells) +
          f" | {pct(cells[0]['med_r24'], 1)} | {pct(xc['p'], 1)} ({xc['n']}) |")
    w("")
    w("Base rates over all coin-hours (cells: rate):\n")
    w("| population / period | coin-hours | up 30% 24h | down 30% 24h | either 30% 24h | up 15% 24h | up 50% 24h | up 30% 72h |")
    w("|---|---|---|---|---|---|---|---|")
    for (pop, per), (hours, r) in sorted(base.items()):
        w(f"| {pop} / {per} | {hours:,} | {pct(r[TGI[('up', 24, .3)]], 2)} | {pct(r[TGI[('dn', 24, .3)]], 2)} | "
          f"{pct(r[TGI[('any', 24, .3)]], 2)} | {pct(r[TGI[('up', 24, .15)]], 2)} | {pct(r[TGI[('up', 24, .5)]], 2)} | "
          f"{pct(r[TGI[('up', 72, .3)]], 2)} |")
    w("")
    w("## Funding extremes and 7-day outcomes (the research's near-miss test)\n")
    w("Every funding-interval cut is a venue-certified cap / floor hit (Binance moves a symbol to 1h on a cap "
      "hit since 2025-05-02). Share followed by a +50% / -50% move within 7 days, against all coin-hours.\n")
    w("| detector | period | firings | up 50% 7d | down 50% 7d | base up 50% 7d | base down 50% 7d |")
    w("|---|---|---|---|---|---|---|")
    for f in ("cut_neg", "cut_pos", "negfund_up", "prem_disc"):
        for per in ("train", "test"):
            u7, d7 = (sumy((f, sel[f]), per, t, "all") for t in (("up", 168, .5), ("dn", 168, .5)))
            w(f"| `{f}` | {per} | {u7['n']} | {pct(u7['p'], 1)} | {pct(d7['p'], 1)} | {pct(u7['base'], 2)} | "
              f"{pct(d7['base'], 2)} |")
    w("")
    w("## Detector definitions (1h bars, coin's own trailing 30d unless stated)\n")
    w("| detector | kind | play | inputs | definition | grid |")
    w("|---|---|---|---|---|---|")
    for f, (d, kind, needs, desc, grid) in FAMS.items():
        inp = {"oi": "5m metrics (OI, L/S)", "spot": "spot klines", "": "klines, premium, funding"}[needs]
        w(f"| `{f}` | {kind} | {d} | {inp} | {desc} | {'; '.join(json.dumps(p) for p in grid) or '-'} |")
    w("")
    w("## Threshold grid (train / test precision on the play target, firings)\n")
    w("| detector | setting | train prec (n) | test prec (n) | test lift_vm |")
    w("|---|---|---|---|---|")
    for f in ALERTS:
        for g, p in enumerate(FAMS[f][4]):
            tr, te = sumy((f, g), "train", play(f), pop_of(f)), sumy((f, g), "test", play(f), pop_of(f))
            w(f"| `{f}`{' *' if g == sel[f] else ''} | {json.dumps(p) if p else '-'} | {pct(tr['p'], 1)} ({tr['n']}) | "
              f"{pct(te['p'], 1)} ({te['n']}) | {num(te['lift_vm'])} |")
    w("\n`*` = chosen on train.\n")
    def split(k, b, per, tg, pop):
        """(hits, n) of the combo vs of the naive trigger's firings with no combo firing within 24h."""
        cf = [x for x in fires.get(k, []) if x[2] == per and (pop == "all" or x[4])]
        near = defaultdict(list)
        for x in cf:
            near[x[0]].append(x[1])
        bf = [x for x in fires.get(b, []) if x[2] == per and (pop == "all" or x[4])
              and not any(abs(x[1] - t) <= 24 * HOUR for t in near.get(x[0], ()))]
        return (sum(hit(x[OUT], tg) for x in cf), len(cf)), (sum(hit(x[OUT], tg) for x in bf), len(bf))

    w("## AND combinations (parts true within 24h of each other, selected thresholds)\n")
    w("Rows starting with a naive trigger answer the incremental question: among that trigger's firings, are "
      "the ones where the signature (or structural prior) also holds better than the ones where it does not? "
      "*Without* = the naive trigger's firings with no combo firing on the coin within 24h; *adds* = one-sided "
      "two-proportion z >= 1.645 in that period.\n")
    w("| combination | target | train prec (n) | test prec [95% CI] (n) | test exp | test lift_vm | firings / coin-month | "
      "test bracket mean | train: with vs without (adds) | test: with vs without (adds) |")
    w("|---|---|---|---|---|---|---|---|---|---|")
    adds = {}
    for cb in sc["combos"]:
        k = tuple((f, g) for f, g in cb)
        dirs = {FAMS[f][0] for f, _ in k}
        tg = ("up", 24, .3) if "up" in dirs else ("dn", 24, .3) if "dn" in dirs else ("any", 24, .3)
        pop = "sample" if any(FAMS[f][2] == "oi" for f, _ in k) else "all"
        tr, te = sumy(k, "train", tg, pop), sumy(k, "test", tg, pop)
        inc = []
        if FAMS[k[0][0]][1] == "baseline":
            for per in ("train", "test"):
                (k1, n1), (k2, n2) = split(k, k[0], per, tg, pop)
                ok = ztest(k1, n1, k2, n2) >= 1.645
                adds[(k, per)] = ok
                inc.append(f"{pct(k1 / n1 if n1 else None, 1)} ({n1}) vs {pct(k2 / n2 if n2 else None, 1)} ({n2})"
                           f"{' yes' if ok else ''}")
        inc += [""] * (2 - len(inc))
        w(f"| {' + '.join(f'`{f}`' for f, _ in k)} | {tg[0]} | {pct(tr['p'], 1)} ({tr['n']}) | {pct(te['p'], 1)} "
          f"[{pct(te['lo'], 1)}, {pct(te['hi'], 1)}] ({te['n']}) | {pct(te['exp'], 2)} | {num(te['lift_vm'])} | "
          f"{num(te['per_cm'], 3)} | {pct(te['bracket'], 1)} | {inc[0]} | {inc[1]} |")
    w("")
    both = [k for (k, per), ok in adds.items() if per == "test" and ok and adds.get((k, "train"))]
    w("Filters that add to the naive trigger in both train and test: " +
      (", ".join(" + ".join(f"`{f}`" for f, _ in k) for k in both) or "none") + ".\n")
    w("## Classification priors (share of coin-hours followed by a 30% move within 24h)\n")
    w("| prior | value | train hours | train up / down | test hours | test up / down |")
    w("|---|---|---|---|---|---|")
    pri = defaultdict(dict)
    for k, v in sc["pri"].items():
        name, val, per = k.split("|")
        pri[(name, val)][per] = v
    for (name, val), d in sorted(pri.items()):
        cells = []
        for per in ("train", "test"):
            v = d.get(per)
            if not v or not v[0]:
                cells += ["0", "-"]
                continue
            cells += [f"{v[0]:,}", f"{pct(v[1 + TGI[('up', 24, .3)]] / v[0], 2)} / {pct(v[1 + TGI[('dn', 24, .3)]] / v[0], 2)}"]
        w(f"| {name} | {val} | " + " | ".join(cells) + " |")
    w("")
    w("## Event study: documented cases\n")
    w("t0 = the decisive leg's start pinned from 1h bars: the last hour before the extreme that still traded within "
      "10% of the base (up) / top (down); move = extreme vs the t0 close. Cells: lead in hours before t0 when the "
      "selected detector fired in [t0-72h, t0]; `m+Nh` = first fired N hours after t0 (inside the move) with the "
      "move still left to the extreme in brackets; `.` = evaluable, no firing; `n/a` = inputs missing; blank = "
      "the detector plays the other direction. For down cases a firing before t0 is a short opened before the "
      "top, i.e. early.\n")
    dets = ALERTS
    w("| case | dir | t0 (UTC) | move | age d | " + " | ".join(f"`{f}`" for f in dets) + " |")
    w("|---|---|---|---|---|" + "---|" * len(dets))
    for c in st["cases"]:
        if "det" not in c:
            w(f"| {c['case']} | {c['dir']} | - | - | - | {c.get('status', '')} |" + " |" * (len(dets) - 1))
            continue
        cells = []
        for f in dets:
            x = c["det"][key(f, sel[f])]
            if FAMS[f][0] not in ("any", c["dir"]):
                cells.append("")
            elif not x["ev"]:
                cells.append("n/a")
            elif x["lead"] is not None:
                cells.append(f"{x['lead']}h")
            elif x["inmove_h"] is not None:
                cells.append(f"m+{x['inmove_h']}h ({pct(x['remain'])})")
            else:
                cells.append(".")
        w(f"| {c['case']}{' (test)' if c['test'] else ''} | {c['dir']} | {c['t0']} | {pct(c['mag'])} | "
          f"{c['age_h'] // 24} | " + " | ".join(cells) + " |")
    w("")
    w("Placebo (same coin, 72h blocks in [t0-90d, t0-14d]): share of blocks where the selected detector fired. "
      "A recall that is no higher than the placebo rate means the detector describes the coin, not the event.\n")
    w("| detector | cases hit / evaluable (all) | mean placebo block rate on the same coins | "
      "cases first firing inside the move | median h after t0 | median move left to the extreme |")
    w("|---|---|---|---|---|---|")
    for f in dets:
        k = key(f, sel[f])
        rel = [c for c in cases if FAMS[f][0] == "any" or c["dir"] == FAMS[f][0]]
        ev = [c for c in rel if c["det"][k]["ev"]]
        hits = sum(c["det"][k]["lead"] is not None for c in ev)
        pl = [c["det"][k]["placebo"] for c in ev if c["det"][k]["placebo"] is not None]
        mv = [c["det"][k] for c in ev if c["det"][k]["lead"] is None and c["det"][k]["inmove_h"] is not None]
        lo_hi = wilson(hits, len(ev))
        md = lambda v: statistics.median(v) if v else None
        w(f"| `{f}` | {hits}/{len(ev)} ({pct(lo_hi[0])}, CI {pct(lo_hi[1])}-{pct(lo_hi[2])}) | "
          f"{pct(statistics.mean(pl)) if pl else '-'} | {len(mv)} | {num(md([x['inmove_h'] for x in mv]), 0)} | "
          f"{pct(md([x['remain'] for x in mv]))} |")
    w("")
    w("## Liquidity at firings that reached the target (test + train, selected thresholds)\n")
    w("| detector | hits | median 24h quote volume | median 1h quote volume | median |1h move| per $1M | "
      "median Corwin-Schultz spread (1h bars) | 1% of an hour's volume |")
    w("|---|---|---|---|---|---|---|")
    for f in ALERTS:
        fs = [x for x in fires.get((f, sel[f]), []) if hit(x[OUT], play(f))]
        if not fs:
            continue
        md = lambda j: statistics.median([x[j] for x in fs if x[j] is not None]) if any(x[j] is not None for x in fs) else None
        w(f"| `{f}` | {len(fs)} | {money(md(LIQ))} | {money(md(LIQ + 1))} | {pct(md(LIQ + 2), 2)} | {pct(md(LIQ + 3), 2)} | "
          f"{money(md(LIQ + 1) * 0.01 if md(LIQ + 1) else None)} |")
    if sc.get("depth"):
        dd = sc["depth"]
        w("")
        w(f"Real book depth (Binance bookDepth archive, nearest snapshot before the firing) at {len(dd)} sampled "
          f"actionable firings: median +-2% depth bid {money(statistics.median(x['bid2'] for x in dd))} / ask "
          f"{money(statistics.median(x['ask2'] for x in dd))}; 10th percentile bid "
          f"{money(sorted(x['bid2'] for x in dd)[len(dd) // 10])}.\n")
        by = defaultdict(list)
        for x in dd:
            by[x["fam"]].append(x)
        w("| detector | sampled | median bid depth 2% | median ask depth 2% |")
        w("|---|---|---|---|")
        for f, v in sorted(by.items()):
            w(f"| `{f}` | {len(v)} | {money(statistics.median(x['bid2'] for x in v))} | "
              f"{money(statistics.median(x['ask2'] for x in v))} |")
    w("")
    w("## Cross-venue lead (Gate perp vs Binance perp, 1m candles)\n")
    xv = st.get("xv", [])
    if xv:
        w("| case | Gate leads in [t0-72h, t0] | share of hours | dispersion spike | controls (same month): lead / dispersion rate per 72h block |")
        w("|---|---|---|---|---|")
        for r in xv:
            if "status" in r:
                w(f"| {r['case']} | {r['status']} | | | |")
                continue
            cc = "; ".join(f"{x['sym']} {pct(x['lead_rate'])} / {pct(x['disp_rate'])}" for x in r["controls"]) or "-"
            w(f"| {r['case']} | {r['lead_any']} | {pct(r['lead_share'])} | {r['disp_any']} ({pct(r['disp_share'])}) | {cc} |")
        ok = [r for r in xv if "status" not in r]
        ctl = [x for r in ok for x in r["controls"]]
        if ok and ctl:
            w("")
            w(f"Gate-lead fired before {sum(r['lead_any'] for r in ok)}/{len(ok)} cases vs a mean control rate of "
              f"{pct(statistics.mean(x['lead_rate'] for x in ctl))} per 72h block; dispersion before "
              f"{sum(r['disp_any'] for r in ok)}/{len(ok)} vs {pct(statistics.mean(x['disp_rate'] for x in ctl))}.\n")
    w("## Verdicts\n")
    for f, v in verdict.items():
        w(f"- `{f}`: {v}")
    w("")
    w("## Not testable from the archive\n")
    for a, b in NOT_TESTABLE:
        w(f"- {a}: {b}")
    out = Path(args.out) if args.out else WORK / "althunt-report.md"
    out.write_text("\n".join(L) + "\n")
    print(f"report written: {out}")

# --------------------------------------------------------------------------------------------- selftest


def synthetic(n=1500, seed=3):
    rnd = random.Random(seed)
    S = {"sym": "TESTUSDT", "h0": ms("2025-01-01") // HOUR, "n": n, "spot": True}
    c, cols = 1.0, {k: [] for k in ("o", "h", "l", "c", "qv", "tb", "prem", "oi", "oiv", "ta", "tp", "ga", "tk", "sqv")}
    oi = 1e6
    for i in range(n):
        o = c
        c = max(1e-6, c * math.exp(rnd.gauss(0.0005, 0.02) + (0.3 if i == 900 else 0)))
        q = 1e5 * math.exp(rnd.gauss(0, 0.7))
        oi *= math.exp(rnd.gauss(0, 0.03))
        for k, v in (("o", o), ("h", max(o, c) * (1 + abs(rnd.gauss(0, .01)))), ("l", min(o, c) * (1 - abs(rnd.gauss(0, .01)))),
                     ("c", c), ("qv", q), ("tb", q * rnd.uniform(.3, .7)), ("prem", rnd.gauss(0, .003)), ("oi", oi),
                     ("oiv", oi * c), ("ta", rnd.uniform(.5, 2)), ("tp", rnd.uniform(.5, 2)), ("ga", rnd.uniform(.3, 3)),
                     ("tk", rnd.uniform(.5, 2)), ("sqv", q / rnd.uniform(1, 40))):
            cols[k].append(v)
    S.update(cols)
    S["fund"] = [((S["h0"] + i) * HOUR, 8 if i < 1000 else 1, rnd.gauss(-0.0005, 0.002)) for i in range(0, n, 8)]
    S["fund"] += [((S["h0"] + i) * HOUR + 1, 1, -0.02) for i in range(1001, 1010)]
    S["fund"].sort()
    return S


def cmd_selftest(_args):
    a = [3, 1, None, 4, 1, 5, 9, 2, 6]
    for w_ in (1, 2, 4):
        for i in range(len(a)):
            v = [x for x in a[i + 1:i + 1 + w_] if x is not None]
            assert fwd(a, w_, True)[i] == (max(v) if v else None), (w_, i)
            assert fwd(a, w_, False)[i] == (min(v) if v else None)
            v = [x for x in a[max(0, i - w_ + 1):i + 1] if x is not None]
            assert slide(a, w_, True)[i] == (max(v) if v else None)
    r = Roll(5, 1)
    for t in range(10):
        r.trim(t)
        if t >= 5:
            assert r.s == sorted(range(t - 4, t)), (t, r.s)
        r.add(t, t)
    p, lo, hi = wilson(5, 10)
    assert abs(p - .5) < 1e-9 and 0.18 < lo < 0.24 and 0.76 < hi < 0.82
    P = Feat()   # bracket: +30% hit first for the long; the short is stopped at +15% in the same bar
    P.n, P.c, P.h, P.l = 4, [1.0, 1.1, 1.2, 1.0], [1.0, 1.12, 1.35, 1.2], [1.0, 0.95, 1.1, 0.9]
    assert path(P, 0) == (True, False, 0.3, -0.15), path(P, 0)
    P.h, P.l = [1.0, 1.05, 1.05, 1.05], [1.0, 0.9, 0.9, 0.95]   # neither level: exit at the last close
    assert path(P, 0)[2:] == (0.0, 0.0), path(P, 0)
    # no lookahead: perturbing every bar after k must not change any state at or before k
    S = synthetic()
    base = rules(features(S, {}))
    k = 1100
    S2 = json.loads(json.dumps(S))
    rnd = random.Random(9)
    for name in ("o", "h", "l", "c", "qv", "tb", "prem", "oi", "oiv", "ta", "tp", "ga", "tk", "sqv"):
        for i in range(k + 1, S2["n"]):
            S2[name][i] = S2[name][i] * rnd.uniform(0.5, 2)
    S2["fund"] = [x if x[0] <= (S2["h0"] + k + 1) * HOUR else (x[0], x[1], -x[2] * 3) for x in S2["fund"]]
    pert = rules(features(S2, {}))
    fired = 0
    for s in base:
        for i in range(k + 1):
            assert base[s](i) == pert[s](i), f"lookahead in {s} at {i}"
            fired += bool(base[s](i))
    assert fired > 50, "synthetic series should fire some detectors"
    F = features(S, {})
    assert any(F.cut[i] for i in range(995, 1010)), "interval cut not seen"
    print(f"selftest ok ({fired} true states checked for lookahead)")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["fetch", "scan", "study", "report", "selftest"])
    ap.add_argument("out", nargs="?")
    ap.add_argument("--from", dest="mfrom", default="2024-11")
    ap.add_argument("--to", dest="mto", default="2026-09")
    ap.add_argument("--sample", type=int, default=150)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--jobs", type=int, default=6)
    ap.add_argument("--procs", type=int, default=max(1, (os.cpu_count() or 2) - 2))
    ap.add_argument("--only", help="comma-separated symbols (scan subset, for testing)")
    ap.add_argument("--relist", action="store_true", help="refresh the archive listing")
    ap.add_argument("--offline", action="store_true", help="use cached files only, no network")
    args = ap.parse_args()
    NET.offline = args.offline
    args.jobs = max(1, min(args.jobs, 6))
    WORK.mkdir(parents=True, exist_ok=True)
    {"fetch": cmd_fetch, "scan": cmd_scan, "study": cmd_study, "report": cmd_report, "selftest": cmd_selftest}[args.cmd](args)


if __name__ == "__main__":
    main()
