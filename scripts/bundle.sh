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
  <key>CFBundleShortVersionString</key><string>0.4.0</string>
  <key>CFBundleVersion</key><string>9</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$APP"
echo "installed $APP"
