# Contributing to Depth

Thanks for helping. A few rules keep the codebase consistent:

- **English only** in code: identifiers, comments, log and error strings. UI text goes through the
  translation files (`assets/i18n/*.txt`, `app/Resources/zh-*.txt`).
- **Connectors stay thin**: one file per exchange in `src/ex/`, `serde_json::Value` parsing, every
  payload converted to `Event`. Sizes in base units, funding as an hourly rate, open interest one-sided.
- **No REST polling for account data.** Private state comes from WebSocket pushes; every REST call goes
  through the per-host rate-limit pause in `src/ws.rs`. Getting a user's IP banned is the worst bug.
- **Trading code** never sends an order or a signed request that has not been checked read-only
  first; new venues start in the "untested" list (`trade::VERIFIED`).
- A connector is not done until `cargo run --bin probe -- <exchange> <market> BTC` streams live data.
  After touching `src/lib.rs` or `src/ws.rs`, run `scripts/probe-all.sh 20`.
- Keep changes small and tested: `cargo test --workspace` must pass.

Build the app with `scripts/bundle.sh` (macOS 26, Rust stable, Swift 6).

By contributing you agree that your contribution is licensed under the GPL-3.0-or-later.
