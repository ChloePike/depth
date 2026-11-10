/// <reference path="./depth.d.ts" />
// Shorts liquidated minus longs liquidated, summed over n bars (base units): positive = shorts being squeezed.
export const meta = { name: "Liquidation skew" };

export default function (d: Data): Plot[] {
  const n = 10;
  const net = d.liq_short.map((x, i) => x - d.liq_long[i]);
  const sum = ta.sma(net, n).map((x) => x * n);
  return [plot(sum, { title: "shorts - longs", style: "hist" })];
}
