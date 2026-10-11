#!/bin/sh
# Builds the app bundle (release binary + icon + Info.plist) into dist/.
# usage: scripts/bundle.sh [--install]   (--install copies it to ~/Applications)
# Without MASTHEAD_RELEASE=1 (set by the release workflow) it's a dev build: "Masthead Dev.app" with its
# own bundle id and an orange icon, so it's easy to tell apart from an installed release.
set -e
cd "$(dirname "$0")/.."
if [ -n "$MASTHEAD_RELEASE" ]; then
  name="Masthead" id="io.github.debuggedd.masthead"
else
  name="Masthead Dev" id="io.github.debuggedd.masthead.dev"
fi
cargo build --release
app="dist/$name.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/masthead "$app/Contents/MacOS/Masthead"

# Icon: every size iconutil wants, rendered from the SVG (or scaled from the PNG without rsvg-convert).
# Dev builds swap the blue/purple glyph gradient for orange.
svg=$(mktemp).svg
if [ -n "$MASTHEAD_RELEASE" ]; then
  cp assets/icon.svg "$svg"
else
  sed 's/#7dcfff/#ffd27a/; s/#7aa2f7/#ff9e64/; s/#bb9af7/#f7768e/' assets/icon.svg > "$svg"
fi
set_dir=$(mktemp -d)/icon.iconset
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((s * scale)); file=icon_${s}x${s}; [ $scale = 2 ] && file=${file}@2x
    if command -v rsvg-convert >/dev/null; then
      rsvg-convert -w $px -h $px "$svg" -o "$set_dir/$file.png"
    else
      sips -z $px $px assets/icon.png --out "$set_dir/$file.png" >/dev/null
    fi
  done
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/AppIcon.icns"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleExecutable</key><string>Masthead</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

# Sign with the "Masthead Signing" certificate when present (your keychain, or the release workflow's):
# macOS privacy permissions (Documents access, controlling Ghostty) are tied to the signer, so they
# survive rebuilds and updates. An ad-hoc signature changes every build, so macOS would ask again each time.
identity=$(security find-identity -p codesigning 2>/dev/null | awk '/"Masthead Signing"/ {print $2; exit}')
codesign --force --deep -s "${identity:--}" "$app"
echo "$app"

if [ "$1" = "--install" ]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/$name.app"
  cp -R "$app" "$HOME/Applications/"
  echo "installed: $HOME/Applications/$name.app"
fi
