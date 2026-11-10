/// <reference path="./depth.d.ts" />
// Open interest and price change over the last n bars, in %. Price up with OI down = shorts covering;
// price down with OI up = new shorts (perp).
export const meta = { name: "OI vs price" };

export default function (d: Data): Plot[] {
  const n = 24;
  const pct = (a: Float64Array) => ta.change(a, n).map((x, i) => (100 * x) / a[i - n]);
  return [
    plot(pct(d.close), { title: "price %", color: "fg" }),
    plot(pct(d.oi), { title: "OI %", color: "warn" }),
  ];
}
