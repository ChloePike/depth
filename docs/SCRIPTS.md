# Custom indicators (scripts)

Write your own chart panes in TypeScript (or plain JavaScript). A script gets the chart's bars as whole columns
(price, volume, CVD, open interest, funding, order flow, liquidations, ...) and returns one or more series to plot.
Depth reloads a script as soon as you save it.

## Getting started

1. In the chart toolbar, open **Scripts** and click **Open folder**. The folder is
   - macOS: `~/Library/Application Support/Depth/indicators`
   - Windows: `%APPDATA%\Depth\indicators`
   - Linux: `~/.config/depth/indicators`

   Depth creates it on first use with five examples and `depth.d.ts` (the types).
2. Tick a script in the **Scripts** menu: it appears as a pane under the chart. The choice is kept per chart.
3. Edit the file in any editor and save. VS Code (or any TypeScript-aware editor) completes `d.`, `ta.` and
   `plot` from `depth.d.ts`; keep the `/// <reference path="./depth.d.ts" />` line at the top for that.

A new `.ts` or `.js` file in the folder shows up in the menu within a second; a deleted one drops out.

## A script

```ts
/// <reference path="./depth.d.ts" />
export const meta = { name: "OFI strength" };            // the pane's title (default: the file name)

export default function (d: Data): Plot[] {
  const ofi = ta.ema(d.ofi, 20);
  const vol = ta.ema(d.vol, 20);
  return [plot(ofi.map((x, i) => (vol[i] > 0 ? x / vol[i] : NaN)), { title: "OFI / vol", style: "hist" })];
}
```

- The **default export** is called with `d`, the chart's bars, and returns a `Plot` or a list of them (at most 16).
- It runs over **whole columns**, not once per bar: `d.close` is a `Float64Array` with one value per bar, oldest
  first. Use ordinary array code (`map`, loops) and the `ta` helpers.
- It runs again when a new bar appears, and at most once a second while the last bar is forming.
- The bars are the chart's: its interval, and its source (one venue, or the cross-venue aggregate).

## Data

Every column has one value per bar. `NaN` means no data for that bar (and leaves a gap in a plot).

| Column | Meaning |
|---|---|
| `t` | Bar open time, ms since 1970 UTC |
| `open` `high` `low` `close` | Price (aggregate: the composite index) |
| `vol` `buy` `sell` | Traded volume, base units; `buy` / `sell` = taker side |
| `cvd` | Cumulative volume delta, base units |
| `oi` | Open interest, base units, one side (perpetuals) |
| `funding` | Predicted funding rate **per hour** (`0.0001` = 1 bp) |
| `funding_settled` | Last settled funding rate per hour |
| `basis` | Perpetual vs spot, bp |
| `ls` | Accounts long / short ratio |
| `ofi` | Top-of-book order flow imbalance, base units |
| `add_bid` `add_ask` `rm_bid` `rm_ask` | Liquidity added / removed within 0.25% of mid, base units |
| `liq_long` `liq_short` | Liquidated longs / shorts, base units |

Order flow (`ofi`, `add_*`, `rm_*`), liquidations and the predicted `funding` are recorded **live**: on bars from
before Depth was opened they are `0` or `NaN`, so those plots start at the right edge. `oi`, `funding_settled`,
`ls` and prices come with the venues' history.

## Helpers

| Function | Returns |
|---|---|
| `ta.sma(a, n)` `ta.ema(a, n)` | Simple / exponential moving average |
| `ta.rma(a, n)` | Wilder's moving average (as in RSI, ATR) |
| `ta.stdev(a, n)` | Rolling standard deviation |
| `ta.highest(a, n)` `ta.lowest(a, n)` | Rolling max / min |
| `ta.change(a, n = 1)` | `a[i] - a[i - n]` |
| `ta.cum(a)` | Running sum (`NaN` counted as 0) |
| `ta.rsi(a, n)` | RSI, 0..100 |
| `ta.zscore(a, n)` | `(a - sma) / stdev` over `n` bars |
| `plot(values, { title, color, style })` | A plot. `style`: `"line"` (default) or `"hist"` (bars from zero, green / red unless a color is set). `color`: `"#rrggbb"` or a theme color `"up"`, `"down"`, `"accent"`, `"warn"`, `"purple"`, `"fg"`, `"muted"` |

A rolling helper returns `NaN` until it has `n` values, and wherever its window holds a `NaN`. A plot shorter than
the bars is aligned to the last bar.

## Examples in the folder

| File | Shows |
|---|---|
| [`ofi_strength.ts`](../assets/indicators/ofi_strength.ts) | Order flow imbalance per unit of traded volume |
| [`funding_zscore.ts`](../assets/indicators/funding_zscore.ts) | Funding against its recent range, with ±2 lines |
| [`oi_vs_price.ts`](../assets/indicators/oi_vs_price.ts) | Open interest and price change over 24 bars, in % |
| [`liquidation_skew.ts`](../assets/indicators/liquidation_skew.ts) | Shorts minus longs liquidated over 10 bars |
| [`book_pressure.ts`](../assets/indicators/book_pressure.ts) | Net liquidity added near the price, bids minus asks |

The same files are in this repository under [assets/indicators](../assets/indicators), with
[depth.d.ts](../assets/indicators/depth.d.ts). They are yours to change; Depth writes them only when it creates the folder. `depth.d.ts` is Depth's and is
replaced when a new version changes it.

## Errors

A script that fails to compile or throws shows its error in its pane, and as a `!` with the message in the
**Scripts** menu. Fix the file and save.

## Limits and safety

Scripts run inside an embedded JavaScript engine (QuickJS) with no access to anything but the bars:

- no network, files, processes, other modules (`import`), keys, account or orders;
- one run may take 50 ms, then it is stopped (`stopped: over 50 ms`);
- 64 MB of memory and a bounded stack per script: deep recursion or runaway arrays end in an error, not a frozen
  or crashed app.

Still, only run scripts you have read or trust: a script cannot trade or read your keys, but it can show you
misleading numbers.

## How it works

Depth strips the TypeScript types (oxc) and runs the module in QuickJS; nothing is type-checked at run time.
Modern JavaScript (ES2023) works; there is no DOM and no Node API.
