# Depth user guide

[中文版](GUIDE.zh-CN.md)

## 1. Install

| OS | File | Notes |
|---|---|---|
| macOS (Apple silicon) | `Depth-macos-arm64.zip` | Not notarized: first launch **right-click Depth.app → Open → Open** (or `xattr -dr com.apple.quarantine /Applications/Depth.app` once). |
| Windows x64 | `Depth-windows-x64.zip` | Unsigned: SmartScreen asks once. |
| Linux x64 | `Depth-linux-x64.tar.gz` | Needs a desktop with OpenGL / Vulkan. |

Downloads: [latest release](https://github.com/ChloePike/depth/releases/latest).
Depth connects straight to the exchanges from your computer. A venue your network or region cannot
reach stays red in the status bar; the others keep working.

## 2. The window

```
┌ title bar: pair · composite price · Spot | Margin | Perpetual | Options · clock · order panel · settings ┐
│ market strip: mark, index, funding + countdown, 24h change / range / volume, open interest, basis, CVD   │
├───────────────────────────────────────────────────────┬──────────────────┬──────────────────────────────┤
│ chart toolbar: interval · source · indicators ·       │ Book · Trades ·  │ order panel (Perpetual)       │
│ layout · alerts · replay · drawing tools              │ Venues · Quant   │ account card                  │
│ chart (one to four cells)                             │                  │ position insurance            │
├───────────────────────────────────────────────────────┴──────────────────┴──────────────────────────────┤
│ Positions · Open Orders · TP/SL · Signals · Order / Trade / Position History · Order Log · Assets        │
├─────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ status bar: venues with latency (click to disconnect / reconnect) · msg/s · memory · clock · language     │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Window**: drag the title bar to move, any edge to resize. Position and size are remembered, as are
  the chart, book choices, layout and every setting.
- **Pair**: click the pair (⌘K), type to search, Enter to open. ★ (⌘D) keeps it in Favorites, ⌘[ / ⌘]
  step through them. The list covers everything listed on Binance, Bybit, OKX and Hyperliquid and is
  cached, so it is there instantly at launch.
- **Stocks and commodities**: pick them by their Binance name (XAU, BZ, CL, AAPL…). Depth finds the
  same market on the other venues under their own names (Hyperliquid `xyz:GOLD`, Lighter `BRENTOIL`,
  MEXC `UKOIL`, `AAPLSTOCK`…) and aggregates them. A venue whose same-name market is a different
  asset (another currency, another scale) is left out.
- **Market**: Spot / Margin / Perpetual / Options (⌘1–⌘4).
- **Venues**: click a venue in the status bar to disconnect it; only that venue's connections close.
  Latency: green < 150 ms, orange < 400 ms, red above.
- **Order panel**: ⌥⌘I. **Settings**: ⌘,.

## 3. Chart

**Navigation**
- Interval buttons or ⌃1–⌃6 (1m, 5m, 15m, 1H, 4H, 1D); the chevron has the rest up to 1W.
- *Aggregate* = all venues (candles follow the composite index, volume and CVD from every venue's
  trades), or pick one venue for its raw prices.
- Scroll / pinch to zoom time, drag to pan, drag the price axis to scale, double-click to reset.
  `%` and `log` (corner under the axis) switch the price scale. *Latest* returns to the live edge.
- **Layout** (rectangle button): 1, 2 side by side, 2 stacked, 3 or 4 charts. Click a cell to make it
  the active one: the toolbar drives the highlighted chart. Drawings are shared per pair.

**Indicators** (the *Indicators* menu; inputs such as MA lengths, Bollinger and RSI / MACD periods are
edited inline; the legend's third row names each overlay with its current value)

| Indicator | What it shows |
|---|---|
| Moving averages | Up to three, configurable lengths. |
| Bollinger bands | Mean ± k standard deviations. |
| Session VWAP ±1σ ±2σ | Volume-weighted average since 00:00 UTC with bands. |
| Volume profile | Volume at price in the visible range (left side): **POC** most traded price, **VAH / VAL** edges of the 70% value area, **S / R** high-volume nodes below / above. |
| Breakouts | The most recent closed candle through VAH, VAL or a node: *Broke up / Broke down*; *Failed break* once price closes back on the side it came from. |
| Expected move cone | ±1σ / ±2σ range for the next 30 bars from the volatility model. |
| Mark price | Purple line and axis tag (OI-weighted across venues on the aggregate). |
| Book heatmap | Resting liquidity over time, recorded live since launch (best on 1m). |
| Liquidity walls | Resting size far above the book's typical level that has stood for most of the last minute. |
| Liquidation map | Estimated liquidation levels: a time × price heatmap (each level from when it formed until price swept it) and a profile at the right edge; shorts above price, longs below. Estimated from every venue's traded volume and a typical leverage mix, calibrated against real liquidation history. Hover the profile for sizes. |
| Footprint | Buy / sell volume per price inside each candle (*cluster bars* or *side clusters*). |
| Whale bubbles | Large prints on the candles, sized by value (threshold in the menu). |
| Panes | Volume, Delta, CVD, open interest, funding, basis, premium, order-flow imbalance, book flow, long / short ratio, RSI, MACD. |

**Axis tags**: last price (with the candle countdown) and bid / ask come first, then mark price and your
own position / order lines. Context levels (POC, VA, S/R, VWAP, breakouts) keep their tag only when it
fits next to its price; otherwise the line and the legend still show it.

**Drawing tools** (strip at the right of the toolbar): rectangle, Fibonacci, ray, trend line, horizontal
line; behind ⋯: channel, text, measure, long / short position tool. Esc cancels a tool.
Right-click a drawing for color, solid / dashed / dotted, price alert, lock and delete. ⌘Z / ⇧⌘Z undo
and redo; the eye hides all drawings, the trash can clears them (per pair).

**Price alerts**: right-click the chart (or a horizontal line / ray) → *Price alert*. The *Alerts* menu
lists every armed alert on every pair, with its sound (bundled or system sounds). A crossing plays the
sound and posts a notification; alerts on other pairs are watched in the background.

**Bar replay**: *Replay*, click the bar to start from, then play / pause, step with →, change speed.
Click *Replay* again to leave.

**Trading on the chart** (needs a trading key): your positions, orders, TP / SL triggers and
liquidation prices are labelled lines. Drag an order line to move the order (Bybit and Binance amend in
place; other venues cancel and re-place, the confirmation says so), drag a TP / SL line to move the
trigger. The **+** on the price axis puts that price into the order panel.

## 4. Order book column

- **Book**: all venues aggregated (each venue aligned on its own mid, view only) or one venue's raw
  book (click a level to put its price in the order panel). Menus: source, price grouping (steps of
  ten from the venue's finest, e.g. 0.1 / 1 / 10 / 100 for BTC), size in coin or USDT. Hover an
  aggregated level to see how its size splits across venues.
- **Orders (L3)**: the thin bar at the bottom of a level shows the individual orders resting there,
  when known: Lighter exact, OKX and Hyperliquid inferred from order counts (marked as such). Small
  orders are merged; hover a block for its size and age. Hyperliquid can use Flowscan's faster feed
  (Settings → General → Data sources; third-party, off by default).
- **Trades** under the book (switch in Settings → General → Order book): the live tape with each
  print's venue; *MIN* filters by value. Levels a trade eats flash briefly.
- **Venues**: each venue's share of traded volume and of resting depth within ±1%.
- **Quant**: volatility forecast and regime, buy / sell pressure, carry (annualized funding and basis).

## 5. Signals

*Signals* tab: volume spikes, liquidation cascades, OI vs price divergence, crowded funding, spot vs
perp flow, basis extremes, whale prints, venues trading away from the index. Thresholds adapt to each
coin's own history; a kind repeats at most every 10 minutes unless it gets stronger. They describe what
is happening; they are not trade recommendations.

## 6. Connecting an exchange account

Settings → **API Keys**. Keys are stored in the system keychain (macOS Keychain, Windows Credential
Manager, Linux Secret Service), never in a file.

1. Create the key on the exchange. **Start read-only**; add *trade* permission only to place orders.
   **Never enable withdrawals.** Restrict it to your IP where offered.
2. Paste the fields Depth asks for:

   | Venue | Fields |
   |---|---|
   | Binance, Bybit, Gate, Kraken | API key, secret |
   | OKX, Bitget | API key, secret, passphrase |
   | Hyperliquid | account address (0x…), API wallet private key (an API wallet cannot withdraw) |
   | Lighter | `account index:API key index`, API key private key |

3. *Save*, then *Test*.

A read-only key is detected (Binance, Bybit, OKX): the account shows, order buttons and chart trading
are disabled. Binance Portfolio Margin and Bybit Unified accounts are detected automatically.
Bybit and Binance are verified on live accounts; other venues are marked *untested*: start with the
minimum size.

## 7. Trading

**Order panel** (Perpetual)
1. Header: venue (or *Smart* routing), position mode, leverage. Below it the venue's own bid / ask and
   spread.
2. *Open* / *Close*; *Limit*, *Market*, *Stop*, or *More* (trailing stop, scaled, TWAP).
3. Price typed, clicked from a single-venue book, or **BBO**: the venue prices the order on arrival at
   the opposite side (*Counterparty*) or your own side (*Queue*), level 1 / 5 / …
4. Size in the coin, or the slider (100% = available margin × leverage).
5. Optional TP / SL, applied to the position once the order fills.
6. Under the buttons, per side: **Est. liq. price**, **Margin**, **Max open**, then maker and taker fee
   rates, order value and fee.
   - Isolated / ordinary accounts: estimate at this leverage with a 0.5% maintenance rate.
   - Binance Portfolio Margin / Bybit Unified: the price at which the **whole account** would be
     liquidated after this order (uniMMR 1.05 / margin rate 100%), your other positions unchanged.
   Both are marked ≈; the venue's own figure can differ.
7. *Open Long* / *Open Short* → confirmation with route, legs, size, price, fee and slippage → Enter.

Orders are always priced from the chosen venue's own book in its own quote, never from the composite
index. Markets a venue lists under another name (section 2) are not tradable from Depth yet: the order
is refused instead of going to the wrong instrument.

**Smart routing** (Settings → Trading): splits a large order across venues by their own books and fees
within your slippage and split limits; *Fixed* sends everything to one venue.

**Positions tab**
- Size, entry, mark, liquidation price, margin (cross / isolated), PnL and ROE.
- **Est. Funding**: current rate with countdown, next estimated payment.
- **Funding paid**: funding received (+) or paid (−) since the position opened, and since when.
  Binance and Bybit add up the venue's funding ledger back to the opening fill (looked up once, up to
  26 weeks back); OKX, Gate and Hyperliquid report it with the position. `≥` means the position is
  older than the fills the venue returns. Refreshed on connect, after a size change and after each
  settlement, never by polling.
- *Market* / *Limit* close on the row; ✎ for whole or staged TP / SL; *Reverse*; *Close All*;
  *Hide other symbols*; the share button makes a PnL card.

**Account card**: equity and available summed over funded venues (hover for each venue), the
uniMMR / margin-rate gauge per venue.

**Assets and transfers**: *Assets* tab or *Transfer*: move funds between a venue's own accounts (spot,
futures, funding, earn redemption). Auto top-up refills the trading account from spot below a
threshold (at most once a minute per venue).

**Position insurance**: hedge a perp with options (long → puts, short → calls), compared across Binance,
Bybit, OKX, Gate and Deribit and bought on Bybit or Deribit; costs use the venue's real ask. Deribit's
coin-settled options are priced and paid in BTC / ETH (the confirm shows the USD value beside it).

**Demo trading** (Settings → Trading → *Demo account*): every venue trades one simulated account
holding a shared balance (10,000 USDT; the *Starting balance* applies from the next *Reset*), no API
keys needed. A **DEMO** tag shows in the title bar, the order panel, the confirmation and the account
strip; nothing is sent to an exchange. Switching it reconnects only the account, never the market data.
- Market orders walk the venue's own live book (refused when that book is stale or too thin for the
  size). A resting limit fills only when a trade prints through its price: you are last in the queue
  and the print's size is ignored.
- Stops, trailing stops and TP / SL (Bybit, Binance) fire on the venue's mark or last price. Funding
  settles at the venue's own rate and interval while Depth runs. Liquidation at a flat 0.5%
  maintenance margin (cross: every position at once, no partial liquidation). Fees from the table in
  Settings → Trading.
- Not simulated: options, margin loans, transfers, venue-run TWAP, latency. A symbol not on screen is
  marked from a spot last price, and its resting limits fill only once it is on screen again.
- The account is an append-only event log in `depth.db` (`demo_events`); a reset is one more event.
  Demo fills never mix with your real ones.

## 8. Settings

General (language, time zone, cache, order book, data sources) · Appearance (theme, accent, green-up /
red-up, interface size, corner radius) · Trading (demo trading, routing, slippage and split limits,
confirmation, fees per venue) · API Keys · About.

## 9. Troubleshooting

| Problem | What to do |
|---|---|
| A venue stays red | The exchange is unreachable from your network or region; the others keep working. |
| *Rate limited, paused until …* | Depth paused that host after a rate-limit answer and resumes by itself. Don't restart repeatedly. |
| Account data missing | Settings → API Keys → *Test*; check permissions and the IP restriction. |
| Book heatmap empty on 4H / 1D | It is recorded live since launch; use 1m or wait. |
| A stock / commodity shows fewer venues than expected | The venue lists it under a name Depth does not know yet, or a different asset under the same name. Maintainers: `python3 scripts/aliases.py` regenerates the table. |
| Crash or freeze | Attach `~/Library/Logs/Depth/panic.log` to a [bug report](https://github.com/ChloePike/depth/issues/new/choose) (remove anything personal). |

**Where data lives** (macOS): settings `~/Library/Application Support/Depth/`, caches (history, pair
list, coin logos) `~/Library/Caches/Depth/`, logs `~/Library/Logs/Depth/`, keys in the Keychain under
`depth`. To uninstall, delete the app and those folders and remove the `depth` Keychain items.
