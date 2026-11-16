# Plan: Wasm JIT for iOS through a web view

Status 2026-11-16: the user chose to build the iOS feature without waiting
for G0 ("maybe just impl the ios app"). G0 stays open as a measurement, not a
gate; G2 and G3 stand. Contract as built: `apple/README.txt` (Fast mode),
`r86/Bridge/r86.h`, `src/frontend/web_host.rs` (protocol in its header).
- Act 0 done: `web/bench.*`, `web/worker.mjs`, `script/wasm_serve.cjs`.
  Headless Chromium on the x86_64 dev PC, ReactOS to the language dialog:
  Wasm interpreter 106 busy MIPS, Wasm JIT 328. Not phone evidence.
- Act 1 done (Linux, headless Chromium; G1 passed except read cost):
  Windows 2000 from the frozen seed reaches the desktop through the bridge
  in 41.4 s; 924 modules, 0.22 s in module construction, 93% of steps
  native, JIT cache 77% of 128 MiB with no evictions at the first desktop
  (95% in another run; steady state unmeasured). Disk reads: 932 requests,
  4.6 s; the floor is Chromium's synchronous XHR, about 3 ms per request
  (server side 0.5 ms). Next step if a phone shows the same: asynchronous
  read-ahead of the next chunk. Worker cache: 256 KiB chunks, 64 MiB.
- Act 2 Swift: written by Lead, never compiled or run (no Apple toolchain on
  the dev PC). G2 and the web view staying live while off screen
  (`WebEngine.park`) need a device.
- Fast mode is a Settings toggle, off by default, until sound, network and
  CD change exist (Act 3) and G3 has phone numbers. With it on, Windows
  3.1/9x/ME presets on auto also use it; their 16-bit code runs as
  interpreted Wasm (G3 decides).
Lead owns this file.

## Goal and non-goals

iOS has no JIT entitlement. WebKit's web-content process has one, so Wasm in
a `WKWebView` runs as native arm64. Run the existing x86-to-Wasm JIT
(`src/x86/jit/wasm/`, Node-only today) there for iPhone and iPad.

- The app stays native: UI, VM library, disk files, network, sound.
- The web view holds CPU, guest RAM, device register models and the screen
  (canvas). Port I/O never crosses the process boundary.
- Only blocks, packets, input events and status cross it.
- The native interpreter stays for Lockdown Mode, failed probes, watchOS and
  guests the web engine does not win on.
- Not in scope: macOS (has the A64 JIT), watchOS (no web view), threads,
  storing translations, a general browser frontend.

## Design

```
Swift UI ── C API ── Rust session (native)             WKWebView
                      loopback server 127.0.0.1    ←→   page: canvas, probe
                      /<token>/app/*   static           worker: r86-jit.wasm
                      /<token>/disk/N  blocks  ←─ sync XHR (Range / POST)
                      /<token>/ctl     WebSocket: input, pause, power →
                                       ← status, steps, sound, packets
```

1. Bridge = loopback HTTP and WebSocket served by Rust (`src/frontend/ws.rs`
   already serves the web console in the iOS app). Reasons: a worker's
   synchronous XHR keeps IDE I/O synchronous without Asyncify; bodies are
   binary; the bridge is Rust and testable on Linux in Chromium. The Swift
   side only hosts a `WKWebView`.
2. Disk files and formats (dynamic VHD, frozen) stay in `disk_files.rs`. A
   write returns after the native write returns. The worker caches reads and
   drops cached chunks on write.
3. The C API keeps its calls. For a web session Rust forwards keys, pointer,
   pause and power over `/ctl`; Swift swaps the guest layer for the web view.
4. Access: bind 127.0.0.1 only, a random 128-bit token in every path.
5. Engine choice at VM start: probe page (Wasm present, tail calls compile,
   short timed loop). Fail → native interpreter and a one-time dialog:
   Lockdown Mode text with the per-app exemption guide when
   `WKWebpagePreferences.isLockdownModeEnabled`, plain "fast mode
   unavailable" otherwise. Never suggest turning Lockdown Mode off.
6. Guest RAM in the web engine is capped by the Wasm memory (512 MiB guest,
   1 GiB module today). Larger VMs use the native interpreter.
7. Web-content process death = guest power cut; the session ends with a log.

## Decision tree

```
Act 0 harness ──► G0 on the user's iPhone (Safari)
  JIT module fails to load ──► fix if one cause and cheap, else STOP
  R = Safari Wasm JIT busy MIPS / native app MIPS, same ISO, busy boot phase
  R < 1.0 ............ STOP. Keep nothing but the report.
  1.0 ≤ R < 1.5 ...... STOP and report; user decides on tiering work
                       (module size, admission threshold) before any bridge.
  R ≥ 1.5 ............ Act 1
Act 1 bridge (Linux, Chromium) ──► G1
  Windows 2000 frozen seed boots through the bridge, write check passes
  sync read cost > 1 ms per 64 KiB ──► larger chunks / read-ahead, re-measure
  fail ──► fix; no Swift work before G1
Act 2 iOS shell (Lead, user's Mac and phone) ──► G2
  loopback page or sync XHR refused by WKWebView
      ──► same handlers behind WKURLSchemeHandler (r86://)
          still no sync XHR ──► Asyncify build (cost measured first)
G3 on the phone: boot time, web JIT vs native interpreter
  Windows 2000 / XP faster ──► "auto" selects web JIT for them
  Windows 98 slower (16-bit share runs as Wasm interpreter)
      ──► "auto" keeps Windows 9x native; explicit "jit" still allowed
Act 3 parity ──► ship
```

## Act 0: measure in Safari (Intern, Linux only)

Brief: `tmp/wkjit/task-w0.txt`. Output: `tmp/wkjit/REPORT.md`.

- 0.1 `script/wasm_serve.cjs`: Node static server with `Range`, no
  dependencies. Serves `web/`, `web/dist/`, the two ROMs and one media file.
  `POST /result` stores the page's JSON under `tmp/wkjit/results/`.
- 0.2 `web/worker.mjs`: hosts `WasmPc` in a Worker. Disk host = synchronous
  XHR `Range` reads with an aligned chunk cache. This is the product's disk
  path in miniature.
- 0.3 `web/bench.html`: probe, then ReactOS boot to the language dialog
  (`benchmarks/reactos_livecd_boot_to_langsel/langsel.rgb`) once per engine,
  each in a fresh Worker. Reports per engine: wall ms, steps, ms inside disk
  reads, MIPS = steps / (wall − disk), frame match, JIT module statistics,
  any error text, user agent. Shows the numbers large and posts them.
- 0.4 Check: headless Chromium on the dev PC completes both engines with the
  expected frame. Chromium timings are not evidence for the gate.

G0 needs the user: open the page in Safari on the iPhone; boot the same ISO
in the r86 app and read its MIPS during the busy part of the boot.

## Act 1: bridge (Intern, after G0)

- 1.1 `src/frontend/web_host.rs`: token paths, static app files, block
  read/write/flush on the VM's `Disk`, `/ctl` WebSocket. Reuse `ws.rs`.
- 1.2 `web/vm.html` and worker product mode: OffscreenCanvas, paced run
  slices that yield for messages, write-through disk, status every 500 ms.
- 1.3 `r86-run --web-host`: serve one VM for Chromium on Linux; check script
  boots the frozen Windows 2000 seed and runs the disk write check.

Sketch:

```
GET  /<token>/disk/0   Range: bytes=o-(o+l-1)   → 206, l bytes
POST /<token>/disk/0?o=<offset>                 body → 204 after the write
POST /<token>/disk/0/flush                      → 204
WS   /<token>/ctl      text JSON both ways; binary = sound or packets
```

## Act 2: iOS shell (Lead writes Swift; build on the user's Mac)

- `GuestScreen` hosts a `WKWebView` for web sessions; input still goes
  through the C API.
- C API: start reports the engine and the page URL.
- Probe, fallback and dialog (Design 5). `NSAllowsLocalNetworking`.
- `webViewWebContentProcessDidTerminate` ends the session.

## Act 3: parity (after G3)

The web machine has no sound output and no network backend today
(`src/frontend/web.rs`). Add both over `/ctl` to the native `AVAudioEngine`
path and `network.rs`. Then MIPS display, background pause, memory limits,
and the dialog's Settings path checked on a real phone.

## Open facts to verify, in order of need

- Safari tiering of many small generated modules (G0 answers).
- Wasm tail calls on the app's minimum iOS (probe reports).
- Loopback HTTP and synchronous XHR from a Worker inside WKWebView (G2).
- Web-content memory limit against 128–512 MiB guests (G0 and G3 report).
- The exact Settings path for the Lockdown Mode exemption (Act 3).
