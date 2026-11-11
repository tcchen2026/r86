#!/bin/sh
# Build the r86 Mac app on an Apple silicon Mac (macOS 26) with the Xcode
# command line tools (xcode-select --install) and rustup; no Xcode project.
# Output:
#   apple/build/r86.app  (ad-hoc signed, Hardened Runtime, sandboxed)
#   apple/build/r86.dmg
# CI runs the same script (.github/workflows/apple.yml).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
apple="$root/apple"
out="$apple/build"
app="$out/r86.app"
version=0.1
build=1

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo "build_macos.sh: needs an Apple silicon Mac" >&2; exit 1; }
for tool in swiftc cargo codesign hdiutil plutil; do
    command -v "$tool" >/dev/null || { echo "build_macos.sh: $tool not found" >&2; exit 1; }
done

"$apple/build-rust.sh"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# Relative source paths contain no spaces, even when the checkout path does.
cd "$apple"
set --
for source in $(find r86 -name '*.swift' | sort); do
    set -- "$@" "$source"
done
swiftc -O -whole-module-optimization -parse-as-library -swift-version 5 \
    -target arm64-apple-macos26.0 -module-name r86 \
    -import-objc-header r86/Bridge/r86-Bridging-Header.h \
    -L build/rust -lr86 -liconv -framework CoreFoundation \
    -o "$app/Contents/MacOS/r86" "$@"

cp "$root/firmware/seabios/bios.bin" "$root/firmware/seabios/vgabios-stdvga.bin" \
    "$app/Contents/Resources/"
printf 'APPL????' >"$app/Contents/PkgInfo"
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>r86</string>
	<key>CFBundleExecutable</key>
	<string>r86</string>
	<key>CFBundleIdentifier</key>
	<string>io.github.tcchen2026.r86</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>r86</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>MacOSX</string>
	</array>
	<key>CFBundleVersion</key>
	<string>$build</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key>
	<string>26.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Apache-2.0; see the bundled licenses</string>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

codesign --force --sign - --options runtime --timestamp=none \
    --entitlements "$apple/r86/r86.entitlements" "$app"
codesign --verify --strict "$app"

rm -rf "$out/dmg" "$out/r86.dmg"
mkdir -p "$out/dmg"
cp -R "$app" "$out/dmg/"
ln -s /Applications "$out/dmg/Applications"
hdiutil create -quiet -volname r86 -srcfolder "$out/dmg" -format UDZO "$out/r86.dmg"
rm -rf "$out/dmg"
echo "$app"
echo "$out/r86.dmg"
