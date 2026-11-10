/// <reference path="./depth.d.ts" />
// Net liquidity each side gained near the price (added minus removed), bids minus asks, smoothed:
// resting buyers building up vs sellers pulling out.
export const meta = { name: "Book pressure" };

export default function (d: Data): Plot[] {
  const net = d.add_bid.map((x, i) => x - d.rm_bid[i] - (d.add_ask[i] - d.rm_ask[i]));
  return [
    plot(net, { title: "bar", style: "hist" }),
    plot(ta.ema(net, 20), { title: "EMA 20", color: "accent" }),
  ];
}
