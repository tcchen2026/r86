#!/bin/sh
# Build the r86 iOS app (iPhone and iPad, iOS 26) on an Apple silicon Mac
# with Xcode's command line tools and rustup; no Xcode project.
#   script/build_ios.sh [simulator|device]    (default: simulator)
# Output: apple/build/ios-<kind>/r86.app; a signed device build adds r86.ipa.
#
# A simulator build is ad-hoc signed. A device build is signed when
# R86_IOS_IDENTITY (a codesigning identity) and R86_IOS_PROFILE (its
# .mobileprovision) are set; R86_IOS_BUNDLE_ID must match the profile
# (default io.github.tcchen2026.r86). The iPhone Duo's laptop posture needs
# the iOS 27.1 SDK; older SDKs build without it.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
apple="$root/apple"
kind=${1:-simulator}
version=0.1
build=1
bundle_id=${R86_IOS_BUNDLE_ID:-io.github.tcchen2026.r86}
case $kind in
    simulator)
        sdk=iphonesimulator
        target=aarch64-apple-ios-sim
        triple=arm64-apple-ios26.0-simulator
        platform=iPhoneSimulator
        ;;
    device)
        sdk=iphoneos
        target=aarch64-apple-ios
        triple=arm64-apple-ios26.0
        platform=iPhoneOS
        ;;
    *)
        echo "usage: $0 [simulator|device]" >&2
        exit 2
        ;;
esac
out="$apple/build/ios-$kind"
app="$out/r86.app"

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo "build_ios.sh: needs an Apple silicon Mac" >&2; exit 1; }
for tool in xcrun cargo codesign plutil; do
    command -v "$tool" >/dev/null || { echo "build_ios.sh: $tool not found" >&2; exit 1; }
done

"$apple/build-rust.sh" "$target"

# The hinge and reserved-region APIs arrived in the iOS 27.1 SDK.
sdk_version=$(xcrun --sdk "$sdk" --show-sdk-version)
major=${sdk_version%%.*}
minor=${sdk_version#*.}
minor=${minor%%.*}
duo=
if [ "$major" -gt 27 ] || { [ "$major" -eq 27 ] && [ "$minor" -ge 1 ]; }; then
    duo="-D R86_DUO"
fi
echo "iOS SDK $sdk_version${duo:+ (iPhone Duo laptop posture)}"

rm -rf "$app"
mkdir -p "$app"

# The engine, model and wizard sheets are shared with the Mac app.
cd "$apple"
set --
for source in $(find r86/Engine r86/Model ios -name '*.swift' | sort) \
    r86/Views/Components.swift r86/Views/NewMachineSheet.swift r86/Views/EditMachineSheet.swift; do
    set -- "$@" "$source"
done
# shellcheck disable=SC2086 # $duo is one optional flag pair
xcrun --sdk "$sdk" swiftc -O -whole-module-optimization -parse-as-library -swift-version 5 \
    -target "$triple" -module-name r86 $duo \
    -import-objc-header r86/Bridge/r86-Bridging-Header.h \
    -L "build/rust/$target" -lr86 -liconv -framework CoreFoundation \
    -o "$app/r86" "$@"

# The icon: one 1024 px image (script/build_icons.py); actool renders the
# sizes and writes the Info.plist keys for them.
xcrun --sdk "$sdk" actool ios/Assets.xcassets --compile "$app" --platform "$sdk" \
    --minimum-deployment-target 26.0 --target-device iphone --target-device ipad \
    --app-icon AppIcon --output-partial-info-plist "$out/assets.plist" >/dev/null

# Firmware, the DOS kit (script/build_freedos_image.py or the pinned
# release), the tools CD when this checkout has it, and the licenses.
cp "$root/firmware/seabios/bios.bin" "$root/firmware/seabios/vgabios-stdvga.bin" "$app/"
cp "$root/tmp/freedos/share/dos.zip" "$app/"
if [ -f "$root/assets/r86_tools.iso" ]; then
    cp "$root/assets/r86_tools.iso" "$app/"
else
    echo "assets/r86_tools.iso is missing: the app lacks Install r86 Tools" >&2
fi
"$root/ports/licenses.sh" "$app/licenses"
mkdir -p "$app/licenses/freedos"
cp "$root/assets/freedos.json" "$app/licenses/freedos/"

cat >"$app/Info.plist" <<PLIST
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
	<string>$bundle_id</string>
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
		<string>$platform</string>
	</array>
	<key>CFBundleVersion</key>
	<string>$build</string>
	<key>DTPlatformName</key>
	<string>$sdk</string>
	<key>ITSAppUsesNonExemptEncryption</key>
	<false/>
	<key>LSRequiresIPhoneOS</key>
	<true/>
	<key>LSSupportsOpeningDocumentsInPlace</key>
	<true/>
	<key>MinimumOSVersion</key>
	<string>26.0</string>
	<key>NSHumanReadableCopyright</key>
	<string>Apache-2.0; see the bundled licenses</string>
	<key>NSBonjourServices</key>
	<array>
		<string>_r86._tcp</string>
	</array>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Remote Management serves the web console to computers on your network, and guests on the user-mode network can reach devices on it.</string>
	<key>UIApplicationSceneManifest</key>
	<dict>
		<key>UIApplicationSupportsMultipleScenes</key>
		<false/>
	</dict>
	<key>UIDeviceFamily</key>
	<array>
		<integer>1</integer>
		<integer>2</integer>
	</array>
	<key>UIFileSharingEnabled</key>
	<true/>
	<key>UILaunchScreen</key>
	<dict/>
	<key>UIStatusBarHidden</key>
	<true/>
	<key>UISupportedInterfaceOrientations</key>
	<array>
		<string>UIInterfaceOrientationPortrait</string>
		<string>UIInterfaceOrientationPortraitUpsideDown</string>
		<string>UIInterfaceOrientationLandscapeLeft</string>
		<string>UIInterfaceOrientationLandscapeRight</string>
	</array>
	<key>UISupportedInterfaceOrientations~ipad</key>
	<array>
		<string>UIInterfaceOrientationPortrait</string>
		<string>UIInterfaceOrientationPortraitUpsideDown</string>
		<string>UIInterfaceOrientationLandscapeLeft</string>
		<string>UIInterfaceOrientationLandscapeRight</string>
	</array>
</dict>
</plist>
PLIST
/usr/libexec/PlistBuddy -c "Merge $out/assets.plist" "$app/Info.plist" >/dev/null
plutil -lint "$app/Info.plist" >/dev/null

if [ "$kind" = simulator ]; then
    codesign --force --sign - --timestamp=none "$app"
elif [ -n "${R86_IOS_IDENTITY:-}" ] && [ -n "${R86_IOS_PROFILE:-}" ]; then
    cp "$R86_IOS_PROFILE" "$app/embedded.mobileprovision"
    security cms -D -i "$R86_IOS_PROFILE" >"$out/profile.plist"
    /usr/libexec/PlistBuddy -x -c 'Print :Entitlements' "$out/profile.plist" >"$out/entitlements.plist"
    codesign --force --sign "$R86_IOS_IDENTITY" --entitlements "$out/entitlements.plist" \
        --timestamp=none "$app"
    rm -rf "$out/Payload" "$out/r86.ipa"
    mkdir -p "$out/Payload"
    cp -R "$app" "$out/Payload/"
    (cd "$out" && zip -qry r86.ipa Payload)
    rm -rf "$out/Payload"
    echo "$out/r86.ipa"
else
    echo "build_ios.sh: unsigned; set R86_IOS_IDENTITY and R86_IOS_PROFILE to install on a device" >&2
fi
echo "$app"
