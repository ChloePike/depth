# Depth for Windows (WinUI 3 host)

Code-only WinUI 3 (C#, no .xaml) shell around the Rust terminal, mirroring the SwiftUI host in
`app/`. The chart, book rows and option chain are egui, rendered by wgpu (DX12) into a
`SwapChainPanel` through the C ABI in `ffi/t1.h` (`t1_ffi.dll`). Native panels read the JSON
snapshot from `t1_state` and send actions with `t1_call` (contract: `ui/src/native.rs`).

**This code has never been compiled.** It was written on macOS without a Windows toolchain;
expect a first round of compile fixes.

## Build (on Windows)

Requirements: Rust (MSVC toolchain), .NET 8 SDK, Windows 10 1809+.

```
cargo build --release -p t1-ffi
dotnet build win -c Release -p:Platform=x64
```

ARM64: `cargo build --release -p t1-ffi --target aarch64-pc-windows-msvc`, then
`dotnet build win -c Release -p:Platform=ARM64`. The project copies `t1_ffi.dll`, the venue
icons (`assets/icons` -> `Icons\`) and the translations (`app/Resources/*.txt` -> `Resources\`)
next to `Depth.exe`. Unpackaged and Windows App SDK self-contained: the output folder runs as is.

## Notes

- DPI: wgpu does not call `IDXGISwapChain2::SetMatrixTransform`, so a pixel-sized swap chain
  would show at 1 px = 1 DIP. `EguiPanel` lays the panel out `scale` times larger and shrinks it
  with a `1/scale` render transform (see the `ponytail:` comment there).
- Host-only UI state (panel sizes, favorites, recent pairs) lives in
  `%APPDATA%\TerminalOne\windows-host.json`; crashes of the host go to
  `%LOCALAPPDATA%\TerminalOne\Cache\Logs\host-crash.log` (Rust panics: `panic.log` there).
- Shortcuts (macOS Cmd -> Ctrl): Ctrl+, settings, Ctrl+K pair, Ctrl+[ / Ctrl+] favorites,
  Ctrl+D star, Ctrl+1..4 market, Ctrl+Shift+1..6 interval, Ctrl+Shift+I order panel.
