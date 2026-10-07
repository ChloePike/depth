#!/bin/sh
# Build the SwiftUI app (app/) around the Rust staticlib (ffi/) and install it.
# APP=<path> overrides the destination (default: /Applications/Depth.app).
# The bundle id, Keychain service and settings folder keep their old internal names
# (local.terminal-one / terminal-one / TerminalOne) so keys, layout and favorites survive the rename.
set -e
cd "$(dirname "$0")/.."
CARGO_TARGET_DIR=target/main cargo build --release -p t1-ffi
# SwiftPM does not track the staticlib: without this it keeps the old binary and the old Rust code
rm -f app/.build/release/TerminalOne
(cd app && swift build -c release)
APP="${APP:-/Applications/Depth.app}"
# the app was called Terminal One before: remove that copy so only one is installed
[ "$APP" = "/Applications/Depth.app" ] && rm -rf "/Applications/Terminal One.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp app/.build/release/TerminalOne "$APP/Contents/MacOS/TerminalOne"
cp assets/icons/*.png "$APP/Contents/Resources/"
# app icon (rendered by scripts/make-icon.swift, converted with iconutil)
cp assets/appicon/Depth.icns "$APP/Contents/Resources/Depth.icns"
cp app/Resources/zh-*.txt "$APP/Contents/Resources/" 2>/dev/null || true
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Depth</string>
  <key>CFBundleDisplayName</key><string>Depth</string>
  <key>CFBundleIdentifier</key><string>local.terminal-one</string>
  <key>CFBundleExecutable</key><string>TerminalOne</string>
  <key>CFBundleIconFile</key><string>Depth</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key><string>2</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$APP"
echo "installed $APP"
