#!/bin/sh
# Exercise Swift session/view teardown without booting a guest or opening disks.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
out=tmp/apple-session-check
mkdir -p "$out"
swiftc -O -whole-module-optimization -parse-as-library -swift-version 5 \
    -target arm64-apple-macos26.0 \
    -import-objc-header apple/r86/Bridge/r86-Bridging-Header.h \
    apple/r86/Engine/R86.swift apple/r86/Engine/Session.swift \
    apple/r86/Model/Machine.swift apple/r86/Model/Preferences.swift \
    apple/r86/Views/GuestView.swift tests/apple/session_lifetime.swift \
    -o "$out/session-lifetime"
"$out/session-lifetime"
