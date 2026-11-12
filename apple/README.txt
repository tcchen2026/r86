r86 for the Mac (Apple silicon, macOS 26)

SwiftUI app around the Rust r86 library. One process runs multiple VMs,
each on its own r86 core thread (src/frontend/apple/runner.rs). The C API is
src/frontend/apple/mod.rs, declared in r86/Bridge/r86.h.

Build (macOS 26, Xcode command line tools, rustup; no Xcode project):
  script/build_macos.sh          apple/build/r86.app and apple/build/r86.dmg
The script runs build-rust.sh (build/rust/libr86.a: cargo --features apple,jit,
staticlib), compiles every Swift file under r86/ with one swiftc call, writes
Info.plist, copies the firmware and signs ad hoc with r86/r86.entitlements
and the Hardened Runtime. Xcode can open the apple/ folder for editing. CI
runs the same script (.github/workflows/apple.yml) and uploads r86.dmg.

Contracts:
- Data: $HOME/r86/VMs, the handheld's folder and vm.toml layout. The app is
  sandboxed, so this is inside its container. CD images outside it are
  remembered as security-scoped bookmarks in each VM's bookmarks.plist.
- Firmware: bios.bin and vgabios-stdvga.bin from firmware/seabios, in the
  app's resources.
- Apple silicon macOS builds include the A64 JIT, using MAP_JIT and only the
  allow-jit Hardened Runtime exception. iOS/watchOS remain interpreter-only.
  Native JIT/ABI tests and a deterministic ROM boot have passed on macOS CI;
  the workflow also gates the ReactOS complete language dialog. Hosted macOS
  runners have SIP off: signed boots and MAP_JIT allocation probes there are
  observations, not code-signing enforcement evidence. Real-Mac checks use
  script/apple_jit_check.py --require-signing-enforcement with SIP enabled.
- New VMs use cpu-engine="auto": Windows (win31/95/98/me/2000/xp) and ReactOS
  select JIT when available; other presets select the interpreter. All 16-bit
  code still uses the shared interpreter. Explicit choices are "jit" and
  "interpreter"; existing "interp" settings remain readable. iOS/watchOS run
  "jit" VMs (the handheld's default) interpreted. --no-jit forces
  interpretation without saving that override.
  The C API exposes "engine" in library JSON and accepts the "cpu-engine" key
  in r86_library_set; the Swift engine picker is separate work.
- Each core thread requests user-interactive QoS before constructing Machine.
  This is a scheduling request, not a guarantee of performance-core placement.
- macOS reserves one process-wide MAP_JIT region with 16 fixed 320 MiB slots
  (256 MiB code + 64 MiB entry tables each), 5 GiB of address space.
  Small hosts retain their smaller active code/table layout. Stopping a VM
  returns its slot for reuse; the region is never unmapped or decommitted.
  Untouched pages are demand allocated; touched backing remains until process
  exit. Sixteen active JIT VMs exhaust the pool; a seventeenth reports a startup
  error. Direct branches beyond their encoding's reach stay unchained; the
  diagnostic report counts out-of-range link attempts. Linux retains 128 MiB
  of code per VM; hosts below 1 GiB RAM retain 32 MiB.
  This follows Apple's one-region contract. CI tests pool reuse/isolation;
  code-signing enforcement and the app lifecycle still need real-Mac checks.
- Input: physical key codes (keymap.rs), ⌘ only when the user passes it on,
  mouse captured by a click and released with ⌃⌥ (PS/2, relative).
- Display: the core thread renders RGBA at most every 16.7 ms; the window
  copies new frames on its display link into a CALayer.
- Sound: AVAudioEngine pulls 48 kHz stereo from r86 without blocking.
