#!/usr/bin/env bash
# Builds PowerMeter.app (Universal: Apple Silicon + Intel) into dist/.
#   ./scripts/bundle_macos.sh            # universal
#   ./scripts/bundle_macos.sh --native   # only the current Mac's architecture
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/PowerMeter.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [[ "${1:-}" == "--native" ]]; then
  cargo build --release
  cp target/release/powermeter "$APP/Contents/MacOS/PowerMeter"
else
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  lipo -create -output "$APP/Contents/MacOS/PowerMeter" \
    target/aarch64-apple-darwin/release/powermeter \
    target/x86_64-apple-darwin/release/powermeter
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PowerMeter</string>
  <key>CFBundleDisplayName</key><string>PowerMeter</string>
  <key>CFBundleIdentifier</key><string>de.meb99.powermeter</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>PowerMeter</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict>
</plist>
PLIST

# Ad-hoc signature so Apple Silicon runs it without complaints.
codesign --force --deep --sign - "$APP"
echo "Fertig: $APP"
