/// <reference path="./depth.d.ts" />
// Funding against its own recent range: |z| above 2 marks a crowded side (perp). The predicted rate where the
// chart has it (live), else the last settled one (venue history).
export const meta = { name: "Funding z-score" };

export default function (d: Data): Plot[] {
  const n = 200;
  const f = d.funding.map((x, i) => (Number.isFinite(x) ? x : d.funding_settled[i]));
  const z = ta.zscore(f, n);
  const two = z.map(() => 2), minusTwo = z.map(() => -2);
  return [
    plot(z, { title: "z", style: "hist" }),
    plot(two, { title: "+2", color: "muted" }),
    plot(minusTwo, { title: "-2", color: "muted" }),
  ];
}
