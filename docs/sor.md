# Smart Order Router — design (draft, not implemented)

Pure planner, no network: `route::plan(req, venues, acct, policy, now) -> RoutePlan { legs, excluded, clamped_from }`.
Fixed-venue mode is the same path with one leg. Zero REST in the hot path (books, balances, positions,
leverage, rules are all local).

## Opening
- Score each eligible venue by effective price for the size: walk its OWN book (never the composite) + taker fee
  (maker kinds: price - maker fee). Eligible = keys, no error, book fresh (< 1500 ms), not crossed,
  rules/mode/leverage cached, margin covers min size.
- Market: best effective price; split across venues only if the walk costs > split_bps (2 bp) on the best venue
  or its margin cap < size; merged fee-adjusted ladder decides leg sizes. Never split below $500 notional or when
  a leg would fall under min qty / min notional (fold into the other leg).
- Limit / Post-only / BBO: no split except on margin cap. Post-only checks the venue's own BBO; BBO level must be
  valid for the venue (Bybit 1-5, Binance 1/5/10/20).
- Margin cap per venue: available * leverage / price * 0.98.

## Closing
Goes to venues holding that side; largest/best first, each leg <= the position there, total clamped to the sum.

## Safety
- Slippage cap: a market leg whose walk exceeds max_slip_bps (10) becomes an IOC limit at touch +/- cap.
- Dispersion guard: a venue > 30 bp off the best is refused.
- Client order ids (Bybit orderLinkId, Binance newClientOrderId) `t1-{plan}-{leg}` are REQUIRED before SOR:
  a timed-out leg is reconciled from the private stream, never blindly retried.
- No auto-retry of rejected legs; offer "retry remainder" which re-plans on a fresh snapshot.
- Confirm dialog shows every leg (venue, side, qty, price/kind, fee, slippage, available after), excluded
  venues with reasons, clamps; the plan is recomputed on Confirm and refused if it changed.

## Settings
Smart / Fixed venue; allow split; max legs (2); max slippage bp (10); split threshold bp (2);
min split notional ($500); max book age (1500 ms); taker/maker fee per venue (editable: they drive every route).
Order panel under SOR: no venue switch, summed available and max size, one-line route preview.

## Tests to pin
Cheaper-after-fees wins; split on thin book; no split under min notional; small leg folds; margin cap split +
clamp; stale / crossed books excluded with reason; post-only never crosses; BBO level per venue; slippage cap ->
IOC; closes only on holding venues and never over-close; fixed policy == single leg; rounding dust to largest
leg; dispersion guard.
