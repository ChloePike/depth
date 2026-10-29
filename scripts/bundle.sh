#!/bin/sh
# Build the egui app (ui/src/main.rs) and install it as a macOS app bundle.
# APP=<path> overrides the destination (default: /Applications/Depth.app).
# Keys and folders of the old "Terminal One" build are migrated on first launch (sys.rs).
set -e
cd "$(dirname "$0")/.."
cargo build --release -p t1-ui --bin depth
BIN=target/release/depth
APP="${APP:-/Applications/Depth.app}"
# the app was called Terminal One before: remove that copy so only one is installed
[ "$APP" = "/Applications/Depth.app" ] && rm -rf "/Applications/Terminal One.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Depth"
cp assets/icons/*.png "$APP/Contents/Resources/"
# app icon (rendered by scripts/make-icon.swift, converted with iconutil)
cp assets/appicon/Depth.icns "$APP/Contents/Resources/Depth.icns"
# About panel: build number = commit count (CI checks out the full history), credits with the commit and date;
# no colour in the RTF, so the panel draws it in its own label colour (readable in dark mode)
BUILD=$(git rev-list --count HEAD 2>/dev/null || echo 0)
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
DATE=$(git log -1 --format=%cs 2>/dev/null || date +%F)
cat > "$APP/Contents/Resources/Credits.rtf" <<RTF
{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0\\fswiss Helvetica;}}\\f0\\fs20\\qc
Crypto trading terminal for 11 exchanges in one window:\\line
Binance, Bybit, OKX, Bitget, Gate, MEXC, Coinbase, Kraken, Hyperliquid, Lighter, Deribit.\\line
Order book, heatmap, chart, options and trading, running on this Mac.\\par\\par
Build $COMMIT, $DATE\\par
{\\field{\\*\\fldinst HYPERLINK "https://github.com/ChloePike/depth"}{\\fldrslt github.com/ChloePike/depth}}\\par\\par
{\\fs16 GPL-3.0-or-later. Inter typeface: SIL Open Font License 1.1. Exchange logos: CoinGecko.}\\par
}
RTF
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Depth</string>
  <key>CFBundleDisplayName</key><string>Depth</string>
  <key>CFBundleIdentifier</key><string>local.depth</string>
  <key>CFBundleExecutable</key><string>Depth</string>
  <key>CFBundleIconFile</key><string>Depth</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.4.2</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>NSHumanReadableCopyright</key><string>© 2026 Chloe Pike. GPL-3.0-or-later.</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$APP"
echo "installed $APP"
