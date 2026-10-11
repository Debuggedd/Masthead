#!/bin/sh
# Builds "Masthead.app" (release binary + icon + Info.plist) into dist/.
# usage: scripts/bundle.sh [--install]   (--install copies it to ~/Applications)
set -e
cd "$(dirname "$0")/.."
cargo build --release
app="dist/Masthead.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/masthead "$app/Contents/MacOS/Masthead"

# Icon: every size iconutil wants, rendered from the SVG (or scaled from the PNG without rsvg-convert).
set_dir=$(mktemp -d)/icon.iconset
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((s * scale)); name=icon_${s}x${s}; [ $scale = 2 ] && name=${name}@2x
    if command -v rsvg-convert >/dev/null; then
      rsvg-convert -w $px -h $px assets/icon.svg -o "$set_dir/$name.png"
    else
      sips -z $px $px assets/icon.png --out "$set_dir/$name.png" >/dev/null
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
  <key>CFBundleName</key><string>Masthead</string>
  <key>CFBundleDisplayName</key><string>Masthead</string>
  <key>CFBundleIdentifier</key><string>io.github.debuggedd.masthead</string>
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

# Sign with the local "Masthead Local Signing" certificate when present: macOS privacy permissions
# (e.g. Documents access for kubeconfigs) are tied to the signer, so they survive rebuilds. An ad-hoc
# signature changes every build, so macOS would ask again after each one.
identity=$(security find-identity -p codesigning 2>/dev/null | awk '/"Masthead Local Signing"/ {print $2; exit}')
codesign --force --deep -s "${identity:--}" "$app"
echo "$app"

if [ "$1" = "--install" ]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/Masthead.app"
  cp -R "$app" "$HOME/Applications/"
  echo "installed: $HOME/Applications/Masthead.app"
fi
