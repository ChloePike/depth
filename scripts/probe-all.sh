#!/usr/bin/env bash
# Live regression: probe every exchange x market in parallel and print a result matrix.
# usage: scripts/probe-all.sh [seconds=20]
set -u
cd "$(dirname "$0")/.."
secs=${1:-20}
out=$(mktemp -d)
cargo build -q --release --bin probe || exit 1
bin=target/release/probe

for ex in binance bybit bitget okx mexc coinbase kraken gate hyperliquid lighter; do
  for m in spot margin perp option; do
    t=$secs; [ "$m" = option ] && t=$((secs + 10))
    ( "$bin" "$ex" "$m" BTC USDT "$t" >"$out/$ex.$m" 2>&1; echo "exit=$?" >>"$out/$ex.$m" ) &
  done
done
wait

printf '%-12s %-8s %-6s %s\n' EXCHANGE MARKET RESULT EVENTS
fail=0
for ex in binance bybit bitget okx mexc coinbase kraken gate hyperliquid lighter; do
  for m in spot margin perp option; do
    f="$out/$ex.$m"
    code=$(sed -n 's/^exit=//p' "$f")
    case "$code" in 0) r=OK ;; 2) r=n/a ;; *) r=FAIL; fail=1 ;; esac
    ev=$(awk '/^== /{s=1;next} s && /^[a-z]+ +[0-9]+ /{printf "%s:%s ", $1, $2}' "$f")
    printf '%-12s %-8s %-6s %s\n' "$ex" "$m" "$r" "$ev"
    [ "$r" = FAIL ] && grep '^FAIL' "$f" | sed 's/^/             /'
  done
done

# Startup REST history (spot + perp); n/a where the venue has no such market.
cargo build -q --release --bin hist || exit 1
for ex in binance bybit bitget okx mexc coinbase kraken gate hyperliquid lighter; do
  for m in spot perp; do
    ( target/release/hist "$ex" "$m" BTC >"$out/h.$ex.$m" 2>&1; echo "exit=$?" >>"$out/h.$ex.$m" ) &
  done
done
wait
printf '\n%-12s %-8s %-6s %s\n' EXCHANGE HISTORY RESULT SERIES
for ex in binance bybit bitget okx mexc coinbase kraken gate hyperliquid lighter; do
  for m in spot perp; do
    f="$out/h.$ex.$m"
    r=OK; grep -q '^OK' "$f" || r=FAIL
    [ "$ex.$m" = lighter.spot ] && r=n/a
    ser=$(awk '/^(klines|oi|funding|long\/short|taker) /{printf "%s:%s ", $1, $2}' "$f")
    printf '%-12s %-8s %-6s %s\n' "$ex" "$m" "$r" "$ser"
    [ "$r" = FAIL ] && { fail=1; grep '^FAIL' "$f" | sed 's/^/             /'; }
  done
done
rm -rf "$out"
exit $fail
