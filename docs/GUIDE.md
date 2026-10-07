# Depth user guide

## 1. Install

1. Download `Depth-macos-arm64.zip` from the [latest release](https://github.com/ChloePike/depth/releases/latest)
   and unzip it. Requires macOS 26 on Apple silicon.
2. Move `Depth.app` to `/Applications`.
3. First launch: the build is not notarized, so **right-click Depth.app → Open → Open**. Later launches
   open normally. (Or run `xattr -dr com.apple.quarantine /Applications/Depth.app` once.)

Depth connects straight to the exchanges from your Mac. If an exchange is not reachable from your
network or region, its row in the status bar stays red and the other venues keep working.

## 2. The window

```
┌ toolbar: pair · composite price ───── Spot | Margin | Perpetual | Options ───── settings · order panel ┐
│ market strip: mark, index, funding + countdown, 24h range and volume, open interest, basis, CVD         │
├──────────────────────────────────────────────────────────┬─────────────┬───────────────────────────────┤
│ chart toolbar: interval · source · indicators · drawing  │ book header │ order panel (Perpetual)       │
│ chart                                                    │ order book  │ account card                  │
├──────────────────────────────────────────────────────────┴─────────────┴───────────────────────────────┤
│ positions · open orders · TP/SL · signals · history · log · assets                                     │
├─────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ status bar: venues with latency (click to disconnect), msg/s, memory, UTC clock, language                │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Pair**: click the pair in the toolbar, type to search (every coin listed on Binance, Bybit, OKX or
  Hyperliquid), Enter to open. ★ keeps a pair in Favorites.
- **Market**: Spot / Margin / Perpetual / Options in the toolbar.
- **Venues**: click a venue in the status bar to disconnect it (it really disconnects; click again to
  reconnect). Latency is colored green < 150 ms, orange < 400 ms, red above.
- **Order panel**: toolbar button or ⌥⌘I. **Settings**: gear or ⌘,.
- Drag the handle between the chart and the bottom strip to resize it. Layout is remembered.

## 3. Reading the market

**Price**: the toolbar price is a composite index (each venue's mid weighted by its last hour of
volume). It is a reference: orders are always priced from the chosen venue's own book.

**Chart**
- Interval: 1m … 1W; the `⋯` menu has the rest. Source: *Aggregate* (all venues) or one venue.
- Scroll or pinch to zoom time, drag to pan, drag the price axis to scale price, double-click it or
  press `A` to return to auto scale; `log` and `%` switch the price scale. *Latest* jumps to the live edge.
- Drawing tools: horizontal line, trend line, ray; the trash can clears them (per pair).
- Your positions, resting orders, TP/SL triggers and liquidation prices are drawn as labelled lines.

**Indicators** (Indicators menu; the third legend row shows each one's color and current value)

| Indicator | What it tells you |
|---|---|
| Volume profile | Volume traded at each price in the visible range (left side). **POC** = most traded price, **VAH / VAL** = edges of the 70% value area, **S / R** = high-volume nodes below / above price. Prices tend to stall at high-volume nodes and move fast through thin areas. |
| Breakouts | A closed candle through VAH, VAL or a node is tagged *Broke up / Broke down*. Only closed candles count. |
| Session VWAP ±1σ ±2σ | Volume-weighted average price since 00:00 UTC, with bands. Above VWAP buyers have paid up today; the outer bands are stretched. |
| Expected move cone | ±1σ / ±2σ range for the next 30 bars from the volatility model. |
| Mark price | The price liquidations and unrealized PnL use (OI-weighted across venues on the aggregate). |
| Book heatmap | Resting liquidity over time, recorded live since launch (best on 1m). Bright = large orders. |
| Liquidity walls | Unusually large resting orders within 3% of price, with their size. |
| Liquidation map | Estimated liquidation levels from open interest changes and typical leverage (an estimate). |
| Panes | Volume, CVD (buy minus sell volume), open interest, funding, basis, long/short ratio. |

**Order book column**
- *Book*: aggregated book (all venues, aligned on each venue's mid, view only) or one venue's raw book
  (click a level to put its price in the order panel). Menus: source, price grouping, size unit.
- *Trades*: the live tape with the venue of each print; large prints are highlighted.
- *Venues*: each venue's share of traded volume (5m / 1h / 24h) and of resting depth within ±1%.
- *Quant*: volatility forecast (1h / 24h expected move and regime), buy/sell pressure, carry
  (annualized funding and basis per venue).

**Signals** (bottom strip, *Signals* tab): volume spikes, liquidation cascades, open interest vs price
divergence, crowded funding, spot vs perp flow, basis extremes, whale prints and venues trading away
from the index. Thresholds adapt to each coin's own history; a kind repeats at most every 10 minutes
unless it gets stronger. Signals describe what is happening; they are not trade recommendations.

## 4. Connecting an exchange account

Settings → **API Keys**. Keys are stored in the macOS Keychain, never in a file.

1. Create the key on the exchange. **Start read-only** to see positions and balances; add *trade*
   permission only when you want to place orders. **Never enable withdrawals.** Restrict the key to
   your IP if the exchange offers it.
2. Paste the fields Depth asks for:

   | Venue | Fields |
   |---|---|
   | Binance, Bybit, Gate, Kraken | API key, secret |
   | OKX, Bitget | API key, secret, passphrase |
   | Hyperliquid | account address (0x…), API wallet private key (create an API wallet in the Hyperliquid app; it cannot withdraw) |
   | Lighter | `account index:API key index`, API key private key |

3. *Save*, then *Test*. The result appears next to the venue.

Binance Portfolio Margin and Bybit Unified accounts are detected automatically.
Bybit and Binance are verified on live accounts; the other venues are marked *untested*: try them with
the minimum size first.

## 5. Trading

**Order panel** (Perpetual)
1. The header shows the venue (or *Smart* routing), your position mode and leverage for the pair.
   Mode and leverage are read from the exchange; change them there.
2. *Open* or *Close*, then *Limit*, *Market* or *Post Only*.
3. Price: typed, or click a price in a single-venue book. **BBO** (inside the price field, where the
   venue supports it): the venue prices the order on arrival at the opposite side (*Counterparty*)
   or your own side (*Queue*), level 1/5/…
4. Size in the coin, or drag the slider (100% = available margin × leverage).
5. Optional *TP/SL*: applied to the whole position once the order fills.
6. *Open Long* / *Open Short*. A confirmation shows the route, each leg's venue, size, price, fee and
   slippage. Confirm with Enter.

**Smart routing** (Settings → Trading): splits a large order across venues by their own books and fees,
within your slippage and split limits. *Fixed* sends everything to one venue.

**Positions tab**
- Size (notional and coin), entry, mark, liquidation price, margin (cross / isolated), PnL and ROE,
  estimated next funding payment and countdown.
- *Market* / *Limit* close with the price and size fields on the row (defaults: mark, whole size).
- ✎ next to TP/SL: whole-position or staged take-profit / stop-loss.
- *Reverse*: closes at market and opens the same size on the other side (two market orders).
- *Close All Positions*, *Hide other symbols*. The share button makes a PnL image to copy or save.
- Click a symbol to open its chart. Every order asks for confirmation.

**Assets and transfers**: *Assets* tab or the account card's *Transfer*: move funds between a venue's
own accounts (spot, futures, funding, earn redemption). Auto top-up can refill the trading account
from spot when available margin falls below a threshold.

## 6. Settings

General (language: English / 中文, also in the status bar) · Appearance (accent color, green-up or
red-up, interface size) · Trading (routing, slippage and split limits, confirmation, fees per venue) ·
API Keys · About.

## 7. Troubleshooting

| Problem | What to do |
|---|---|
| A venue stays red in the status bar | The exchange is unreachable from your network or region, or blocked; others keep working. |
| *Rate limited, paused until …* | Depth paused requests to that host after a rate-limit answer and resumes by itself. Don't restart repeatedly. |
| Account data missing | Settings → API Keys → *Test*. Check the key's permissions and IP restriction. |
| Heatmap empty on 4H / 1D | It is recorded live since launch; use 1m or wait. |
| Something crashed or froze | Attach `~/Library/Logs/TerminalOne/panic.log` to a [bug report](https://github.com/ChloePike/depth/issues/new/choose) (remove anything personal). |

**Where data lives**: settings `~/Library/Application Support/TerminalOne/`, caches
`~/Library/Caches/TerminalOne/`, logs `~/Library/Logs/TerminalOne/`, keys in the Keychain under
`terminal-one`. To uninstall, delete the app and those folders, and remove the `terminal-one` items in
Keychain Access.
