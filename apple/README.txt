r86 for the Mac (Apple silicon, macOS 26)

SwiftUI app around the Rust r86 library. One process runs any number of VMs,
each on its own r86 core thread (src/frontend/apple/runner.rs). The C API is
src/frontend/apple/mod.rs, declared in r86/Bridge/r86.h.

Build (macOS 26, Xcode 26, rustup, XcodeGen):
  script/build_macos.sh          r86.app and apple/build/r86.dmg
  cd apple && xcodegen generate && open r86.xcodeproj    to work in Xcode
The "Rust library" build phase runs build-rust.sh, which builds
build/rust/libr86.a (cargo --features apple, staticlib). CI runs
script/build_macos.sh (.github/workflows/apple.yml) and uploads r86.dmg.

Contracts:
- Data: $HOME/r86/VMs, the handheld's folder and vm.toml layout. The app is
  sandboxed, so this is inside its container. CD images outside it are
  remembered as security-scoped bookmarks in each VM's bookmarks.plist.
- Firmware: bios.bin and vgabios-stdvga.bin from firmware/seabios, in the
  app's resources.
- Interpreter only. The Darwin JIT backend is future work.
- Input: physical key codes (keymap.rs), ⌘ only when the user passes it on,
  mouse captured by a click and released with ⌃⌥ (PS/2, relative).
- Display: the core thread renders RGBA at most every 16.7 ms; the window
  copies new frames on its display link into a CALayer.
- Sound: AVAudioEngine pulls 48 kHz stereo from r86 without blocking.
