// Types for Depth custom indicators. Written by Depth: edits here are replaced on the next start.
// A script is a module whose default export takes the chart's bars and returns plots:
//
//   /// <reference path="./depth.d.ts" />
//   export const meta = { name: "My indicator" };
//   export default function (d: Data): Plot[] { return [plot(ta.ema(d.close, 20))]; }
//
// Every column has one value per chart bar, oldest first, at the chart's interval and source (one venue or the
// aggregate). NaN = no data for that bar. Missing values in a plot leave a gap.

interface Data {
  /** bar open time, ms since 1970 (UTC) */ t: Float64Array;
  open: Float64Array; high: Float64Array; low: Float64Array; close: Float64Array;
  /** traded volume, base units; buy / sell = taker (aggressor) side */ vol: Float64Array; buy: Float64Array; sell: Float64Array;
  /** cumulative volume delta, base units */ cvd: Float64Array;
  /** open interest, base units, one side (perp) */ oi: Float64Array;
  /** predicted funding rate per HOUR (perp; 0.0001 = 1 bp) */ funding: Float64Array;
  /** last settled funding rate per hour (perp) */ funding_settled: Float64Array;
  /** perp vs spot, bp */ basis: Float64Array;
  /** accounts long / short ratio */ ls: Float64Array;
  /** top-of-book order flow imbalance, base units */ ofi: Float64Array;
  /** liquidity added / removed within 0.25% of mid, base units */ add_bid: Float64Array; add_ask: Float64Array; rm_bid: Float64Array; rm_ask: Float64Array;
  /** liquidated longs / shorts, base units */ liq_long: Float64Array; liq_short: Float64Array;
}

type Series = ArrayLike<number>;

interface PlotOptions {
  title?: string;
  /** "#rrggbb", or a theme color: "up" | "down" | "accent" | "warn" | "purple" | "fg" | "muted" */
  color?: string;
  /** "line" (default) or "hist" (bars from zero, up / down colored unless a color is given) */
  style?: "line" | "hist";
}

interface Plot extends PlotOptions { values: Series; }

declare const ta: {
  sma(a: Series, n: number): Float64Array;
  ema(a: Series, n: number): Float64Array;
  /** Wilder's moving average (RSI, ATR) */ rma(a: Series, n: number): Float64Array;
  stdev(a: Series, n: number): Float64Array;
  highest(a: Series, n: number): Float64Array;
  lowest(a: Series, n: number): Float64Array;
  /** a[i] - a[i - n] */ change(a: Series, n?: number): Float64Array;
  /** running sum, NaN counted as 0 */ cum(a: Series): Float64Array;
  rsi(a: Series, n: number): Float64Array;
  /** (a - sma) / stdev over n */ zscore(a: Series, n: number): Float64Array;
};

declare function plot(values: Series, opts?: PlotOptions): Plot;
