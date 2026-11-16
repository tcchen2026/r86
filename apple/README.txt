r86 for the Mac (Apple silicon, macOS 26), iPhone and iPad (iOS 26), Apple Watch (watchOS 26)

SwiftUI app around the Rust r86 library. One process runs multiple VMs,
each on its own r86 core thread (src/frontend/runner.rs). The C API is
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
  app's resources, with the DOS kit dos.zip (tmp/freedos/share; CI fetches
  the pinned release) and licenses/. r86.icns is from script/build_icons.py.
- New Machine follows the handheld's wizard: a CD image, FreeDOS from the
  kit (r86_library_create_dos) or an existing disk (bookmarked like CDs),
  then OS, memory (to 3 GiB), disk size and name. Windows 3.1/9x/ME CDs
  install from FreeDOS planted on the new disk.
- Apple silicon macOS builds include the A64 JIT, using MAP_JIT and only the
  allow-jit Hardened Runtime exception. iOS and watchOS have no native JIT
  (iOS fast mode runs the Wasm JIT in a web view; see below).
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
- Tools (toolbar, also in the Machine menu), after the handheld's in-VM
  menu: Install r86 Tools, and remote management, the web console
  (ws_frontend::Remote) on the first free port of 127.0.0.1:8086-8095. Only
  this Mac can connect; it ends when the VM turns off, and the console's
  Power off turns the VM off. Listening needs the network.server entitlement.
- Logs: only a fatal error writes one, $HOME/r86/logs/<VM folder>.log, with
  the machine's state, the run's events and serial output kept in memory
  until then (src/frontend/run_log.rs). The VM window shows the error and
  Show Log.

iPhone and iPad (ios/, iOS 26; interpreter, or the Wasm JIT in fast mode)

The iOS app shares Engine/, Model/ and the New/Edit Machine sheets with the
Mac; its own UI is in ios/ (SwiftUI layout, UIKit for the screen, keys and
trackpad). One machine runs at a time.

Build (Apple silicon Mac, Xcode, rustup target aarch64-apple-ios-sim or
aarch64-apple-ios):
  script/build_ios.sh [simulator|device]   apple/build/ios-<kind>/r86.app
Device builds are signed with R86_IOS_IDENTITY and R86_IOS_PROFILE (set
R86_IOS_BUNDLE_ID to match) and packed as r86.ipa. The manual workflow
.github/workflows/ios.yml uploads a simulator build.

Contracts:
- Data: Documents/VMs and Documents/logs, shown in the Files app. Picked CD
  and disk images are bookmarked as on the Mac.
- Layout follows the window (RootView): an iPhone Duo folded like a laptop
  shows the guest above the fold and a deck below it; wide windows show the
  guest full screen under a floating bar; tall ones show the guest, a key
  row, a trackpad and the controls. The fold is the active horizontal
  division region of GeometryProxy.reservedRegions (iOS 27.1 SDK; the build
  defines R86_DUO only with that SDK).
- Deck: a 15-unit keyboard with an F-row; Fn gives Ins, PrtSc, Home, End,
  PgUp, PgDn and Ctrl+Alt+Del. Tap ▦ for a full trackpad, hold it for a
  trackpad while held.
- Recommended guest mode for a fresh r86 tools install (magic command 5):
  936x666 on iPhone (VBE 1A1h, half the Duo laptop half's 1878x1335 pixels),
  1024x768 on iPad.
- Keys are USB HID usages (UIKeyboardHIDUsage) for the hardware keyboard,
  the deck and the key row. On-screen modifiers latch for one key; a double
  tap locks them. Held keys repeat after 0.45 s at 30 per second. The key row
  rides on the system keyboard.
- Pointer: with the guest's r86 tools helper, touches, an iPad pointer and
  the wheel are absolute; without it, the trackpad sends relative PS/2. A
  tap within 0.5 s and 24 points of the last one clicks the same spot, so
  Windows sees a double-click.
- Background pauses the running machine; returning resumes it. If iOS ends
  the app meanwhile, the machine is off.
- Remote management (More menu) serves the web console on 0.0.0.0, the
  first free port of 8086-8095, for computers on the local network, while
  r86 is in the foreground. Each start makes a new 6-digit access code; the
  WebSocket needs it (?code=), the page asks for it, and the menu shows the
  address and code. Turning it on also shows the Local Network alert (a
  Bonjour browse for _r86._tcp): accepting connections needs no permission
  (TN3179), but guests on the user-mode network reaching LAN devices do.
- Fast mode (Settings › Speed, off by default; plan and open checks in
  PLAN_WEBJIT.md): a machine whose engine is jit, or auto with a Windows or
  ReactOS preset, and at most 512 MiB runs in a WKWebView, where WebKit
  compiles r86's x86-to-Wasm JIT. r86_vm_start_web serves the page
  (<app>/web/: vm.html, the Worker, r86-jit.wasm) and the disks from
  127.0.0.1 behind a random token (src/frontend/web_host.rs); the page runs
  CPU, memory and devices and draws the screen on a canvas. Input, pause
  and power use the same C calls, forwarded to the page. GuestUIView shows
  the web view under its touches (ios/Platform/WebEngine.swift). When the
  page's check fails (Lockdown Mode, no Wasm compiler, no page within 20 s)
  the machine starts again in the interpreter and an alert says why, once
  per launch. No sound, network, CD change or Library thumbnail yet. The
  Swift side has not been compiled or run on a device.
- Apple Watch: Machines › swipe, or More › Copy to Apple Watch, copies a
  turned-off machine to the Watch; the Machines list's Apple Watch section
  copies the Watch's machines back (watch/WatchLink.swift).

Apple Watch (watch/, watchOS 26 arm64: Series 9 and later, interpreter only)

Embedded in the iOS app as r86.app/Watch/r86.app and built with it
(script/build_ios.sh; Rust targets aarch64-apple-watchos and -sim). It shares
Engine/, Model/ and ios/Input/HID.swift; its UI is in watch/. One machine
runs at a time; there is no sound and no machine creation.

Contracts:
- Transfer (WatchLink, WatchConnectivity): one file transfer per machine, a
  clone of its disk image with its portable vm.toml in the metadata
  (r86_library_export: relative disk; no CD, BIOS or VGA BIOS paths). The
  receiver adopts it into a new folder (r86_library_import); nothing is
  overwritten. Running machines are not copied. The Watch publishes its
  machines' names in the application context; the iPhone asks for one by
  name, as a message or as user info delivered when r86 next runs on the
  Watch. The system continues transfers while the apps are suspended.
- Data: Documents/VMs and Documents/logs in the Watch app's container.
- Screen: the guest fitted to the display, 20 frames a second; 320x200,
  640x400 and 720x400 at 4:3. Recommended mode for a fresh r86 tools
  install: 640x480.
- Pointer: with r86 tools, absolute: tap clicks, a second tap within 0.5 s
  and 12 points double-clicks, drag drags, a long press right-clicks.
  Without them the screen is a trackpad (relative PS/2). The Digital Crown
  scrolls with r86 tools and presses Up/Down without them.
- Keys: ⋯ opens typing (system keyboard, Scribble or dictation; US layout),
  Return, Esc, Tab, Delete, Ctrl+Alt+Del and power.
- Background (the watch face) pauses the running machine; returning
  resumes it.
