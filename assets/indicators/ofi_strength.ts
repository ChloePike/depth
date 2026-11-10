/// <reference path="./depth.d.ts" />
// Order flow imbalance against traded volume: how hard the book leans, per unit traded.
// Edit and save: Depth reloads it. Turn it on with the chart's Scripts button.
export const meta = { name: "OFI strength" };

export default function (d: Data): Plot[] {
  const ofi = ta.ema(d.ofi, 20);
  const vol = ta.ema(d.vol, 20);
  return [plot(ofi.map((x, i) => (vol[i] > 0 ? x / vol[i] : NaN)), { title: "OFI / vol", style: "hist" })];
}
