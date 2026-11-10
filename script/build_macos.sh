#!/bin/sh
# Build the r86 Mac app on an Apple silicon Mac (macOS 26, Xcode 26).
# Needs rustup and XcodeGen (brew install xcodegen). Output:
#   apple/build/derived/Build/Products/Release/r86.app  (ad-hoc signed, sandboxed)
#   apple/build/r86.dmg
# CI runs the same script (.github/workflows/apple.yml).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/apple/build"

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo "build_macos.sh: needs an Apple silicon Mac" >&2; exit 1; }
for tool in xcodegen xcodebuild cargo; do
    command -v "$tool" >/dev/null || { echo "build_macos.sh: $tool not found" >&2; exit 1; }
done

cd "$root/apple"
xcodegen generate --quiet
xcodebuild -quiet -project r86.xcodeproj -scheme r86 -configuration Release \
    -derivedDataPath "$out/derived" build

app="$out/derived/Build/Products/Release/r86.app"
codesign --verify --strict "$app"

rm -rf "$out/dmg" "$out/r86.dmg"
mkdir -p "$out/dmg"
cp -R "$app" "$out/dmg/"
ln -s /Applications "$out/dmg/Applications"
hdiutil create -quiet -volname r86 -srcfolder "$out/dmg" -format UDZO "$out/r86.dmg"
rm -rf "$out/dmg"
echo "$app"
echo "$out/r86.dmg"
