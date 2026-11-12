#!/usr/bin/env python3
"""Reference command-line client for the r86-run WebSocket harness.

This file is both a test driver and a compact protocol reference for a browser
or desktop Web UI. It uses the Python ``websockets`` package for RFC 6455
transport handling and keeps the r86 protocol logic in this file.

Server entry point
==================

Start the emulator with WebSocket enabled, for example:

    target/release/r86-run \
        --ws-listen 127.0.0.1:8765 --ws-slice-steps 1000000

For fast device testing, use the checked-in synthetic bootsector:

    target/release/r86-run \
        --bootsector tests/bootsectors/ws_ps2_vga.bin \
        --ws-listen 127.0.0.1:8765 --ws-slice-steps 10000

The default ``--clock realtime`` runs the CPU freely while RTC, PIT, ACPI PM,
IDE completion and the 1 GHz TSC follow monotonic host time. RTC starts at the
host local date and accepts guest calendar writes. HLT yields instead of skipping
wall time. This is common machine behavior, independent of the frontend.
``--clock deterministic`` selects instruction/idle ticks for reproducible tests
and replay. Guest-visible input is applied at committed instruction boundaries;
acknowledgements record their step. Replaying a realtime input log alone cannot
reproduce the original device interrupt timing.

Transport
=========

The same port serves the embedded browser console at http://HOST:PORT/ and
plain RFC 6455 WebSocket at /ws (legacy / also works). This client
uses ``websockets.sync.client`` and does not use TLS or protocol extensions.

Text frames carry UTF-8 JSON request/response objects. Server binary frames
carry raw RGBA screenshots or display frames; client binary frames (at most
1 MiB) carry virtual-media upload data or remote-CD read replies.

Connection lifecycle
====================

On connect, the server immediately sends a JSON hello text frame. A Web UI
should read and store that object before sending commands. The hello/status
objects advertise capabilities, current step, frame size/mode details, and
device counters when available. This script preserves the hello in
``WsClient.hello``.

JSON command model
==================

Each client request is one JSON text frame with a ``cmd`` field. The server
replies with one JSON text frame. Wait for its complete reply (including any
binary frame) before sending another request on the same connection. Request
pipelining is rejected. Up to 32 connections share one machine; HTTP and slow
WebSocket readers do not hold the machine thread, except that remote CD fetches
wait synchronously for their source. The dedicated remote-CD connection uses
the server-initiated read protocol described under Virtual media. Common response fields:

    ok: bool                  true on success
    type: str                 response type, often matching the command
    step: int                 cumulative execution budget (includes idle ticks only
                              with the deterministic clock)
    clock: str                status: "realtime" or "deterministic"
    error: str                present when ok is false

Guest reset requests reboot the machine and continue the current run budget.
``status`` includes ``reset_count``. Steps and serial transcripts remain
monotonic across resets; the CPU, device controllers, and boot inputs restart.
An x86 triple fault shuts down the CPU and resets the board. Execution continues,
and a ``cpu-shutdown`` event records its instruction step. Diagnostics retain the
pre-reset cause, CPU/VGA register dumps and last 16 KiB of serial in a failure
snapshot with ``execution.stop.kind: "triple-fault"`` and ``reset: true``.
``diagnostics.current.cpu_shutdown`` also exposes the latest such frozen record
(or null), even after subsequent manual resets. This record has no post-reset
``status`` object; its CPU dump is the state at shutdown.

Like a server BMC, the WebSocket server outlives the guest. ACPI S5 and BIOS APM
shutdown stop execution and flush writable disks; ``status`` then has
``powered_off: true``, run responses report ``stop.kind: "powered-off"`` with
``ok: true``, and further run requests return that stop with zero progress.
The final screen, logs, diagnostics, virtual media and disk files stay
available. ``power-on`` boots a powered-off VM and resumes it; ``reset`` boots
it and stays paused. ``power-button`` latches an ACPI button event for the
guest OS (graceful shutdown): run the guest to let it save data and enter S5.
``power-off`` cuts power from the host at once, without warning the guest, and
flushes disks. Replay applies recorded power-off/power-on events.

Interactive operation and multiple debuggers
============================================

The server starts paused. ``resume`` runs continuously, independently of display
polls and client connections; ``pause`` stops at a committed slice boundary and
flushes disks. Any client may operate the VM. Keyboard/button state is the union
of all clients' held inputs. Disconnect or ``release-input`` releases only that
client's keys/buttons. Raw scancodes bypass that tracking; their caller must send
matching breaks. ``quit`` disconnects this client, never exits the server.

Finite run/wait commands require a paused, idle machine. In realtime mode,
HLT yields to host scheduling while the execution job waits for device/input
events; it is not a stop/error response. They yield between
slices so other clients can read logs, request frames, inject input, or pause to
cancel the job. Only one execution job is active at a time. Wall time schedules
host work; the selected machine clock determines device timing. Concurrent commands
are serialized; --ws-record records the resulting guest input order for replay.
``reset`` reboots and leaves the VM paused; prior crash evidence is retained.
``status.rtc.next_tick`` is a deadline in 19,090,912 Hz device-clock ticks, not
an instruction index. Realtime RTC continues across debugger pauses; missed
periodic interrupts coalesce, rather than replaying a storm of old interrupts.
Deterministic clock/device state advances only with execution requests.

``logs`` accepts independent ``serial_from`` byte and ``event_from`` sequence
offsets. Advance to ``serial_next`` / ``event_next`` after each reply. Serial
output remains available across guest resets; r86 keeps at least its last
64 KiB, and an older ``serial_from`` resumes at the oldest kept byte (the
reply's ``serial_from``). Reads are bounded by ``limit``
(default 8192, max 65536 bytes); events retain the last 256 entries, with
``event_first`` explicitly reporting the oldest available sequence. Readers do
not consume each other's data. ``logs --follow`` emits incremental JSON records.

Unclaimed I/O and physical MMIO reads return all ones; writes are ignored.
``logs.events`` includes ``kind: "unmapped-bus"`` diagnostics, limited to the
first 1000 byte transactions per accessed 4 KiB page (port I/O and physical
memory separately) AND per originating instruction's linear page (CS.base +
starting EIP, including prefixes). Either counter exceeding 1000 suppresses
the record. Wide bus accesses may split into byte transactions. The 1000th
record announces muting. ``diagnostics.current.bus`` reports target ``pages``
and ``instruction_pages`` with cumulative and suppressed counts, even after
logging stops. ``muted`` means that page has reached its own limit; a record
may also be suppressed by the other page's limit. Guest resets preserve both.
This policy does not suppress CPU exceptions or recover from device errors.

``diagnostics`` returns a read-only current snapshot and up to eight retained
failure snapshots: complete CPU state, device status, stop/error cause, VGA text,
VGA register state (``vga_dump``, excluding VRAM),
last 16 KiB of serial, the configured instruction trace and a ``jit`` report
(empty if disabled), including native execution counts.
Apple builds append process-wide host instructions/cycles and guest steps to
the trace text; zero counters mean unavailable (for example, a virtualized host).
The macOS JIT report also includes write/execute permission transitions and their
rate since arena creation. These counters are sampled only for diagnostics.
With the interpreter, use --minimal-ring or --debug-ring at startup to retain execution history;
the ARM64 JIT rejects those tracing modes. All execution commands
use this failure path, independently of guest OS or installation stage. Invalid
commands are reported as command errors, not guest crashes. An OS blue screen
that keeps executing can be inspected/preserved with diagnostics and screenshot;
there is no OS-specific crash detector. Host process aborts, SIGKILL and host
failure terminate WebSocket too; in-memory evidence cannot survive those events.
Save diagnostics/logs on a separate client when durable capture is needed.

``memory`` reads physical RAM only (max 64 KiB). Normal diagnostics do not perform
MMU/MMIO reads or change A/D bits. The legacy explicit ``guest-bytes`` probe uses
the MMU and may update A/D bits; it is not used by automatic crash capture.
Its ``eip`` and ``start`` are CS-relative offsets; bytes are read at the current
CS base plus ``start``, with 32-bit wrapping, through the diagnostic MMU path.

Commands implemented by the current harness:

    {"cmd":"hello"}
    {"cmd":"status"}
    {"cmd":"resume"}
    {"cmd":"pause"}
    {"cmd":"reset"}
    {"cmd":"release-input"}
    {"cmd":"logs", "serial_from": optional N, "event_from": optional N,
     "limit": optional N}
    {"cmd":"diagnostics"}
    {"cmd":"memory", "address": PHYSICAL_ADDRESS, "length": optional N}
    {"cmd":"guest-bytes"}
    {"cmd":"frame", "since": optional GENERATION}
    {"cmd":"power-button"}
    {"cmd":"run-steps", "steps": N}
    {"cmd":"run-until-step", "step": ABS_STEP}
    {"cmd":"wait-text", "text": TEXT,
     "timeout_steps": N, "slice_steps": optional N}
    {"cmd":"wait-serial", "text": TEXT,
     "timeout_steps": N, "slice_steps": optional N}
    {"cmd":"type", "text": TEXT}
    {"cmd":"keydown", "key": KEY}
    {"cmd":"keyup", "key": KEY}
    {"cmd":"send-scancodes", "hex": HEX_BYTES}
    {"cmd":"mouse-move", "dx": DX, "dy": DY}
    {"cmd":"mouse-button", "button": "left|right|middle", "down": BOOL}
    {"cmd":"usb-tablet", "x": X, "y": Y, "buttons": optional MASK, "wheel": optional N}
    {"cmd":"pointer", "x": X, "y": Y, "buttons": optional MASK, "wheel": optional N}
    {"cmd":"text-snapshot"}
    {"cmd":"hash-frame"}
    {"cmd":"screenshot"}
    {"cmd":"wait-frame-stable", "count": N,
     "timeout_steps": N, "slice_steps": optional N}
    {"cmd":"power-on"}
    {"cmd":"power-off"}
    {"cmd":"files-drives"}
    {"cmd":"files-list", "path": "C:/DIR"}
    {"cmd":"files-get", "path": "C:/DIR/NAME.EXT"}
    {"cmd":"files-put", "path": "C:/DIR/NAME.EXT", "size": BYTES}
    {"cmd":"files-mkdir", "path": "C:/DIR"}
    {"cmd":"files-delete", "path": "C:/DIR"}
    {"cmd":"files-rename", "path": "C:/DIR/OLD.EXT", "to": "NEW.EXT"}
    {"cmd":"media"}
    {"cmd":"media-begin", "kind": "iso", "name": NAME, "size": BYTES}
    {"cmd":"media-begin", "kind": "files", "name": VOLUME_LABEL}
    {"cmd":"media-file", "path": "DIR/NAME.EXT", "size": BYTES}
    {"cmd":"media-dir", "path": "DIR/EMPTY"}
    {"cmd":"media-insert"}
    {"cmd":"media-cancel"}
    {"cmd":"media-eject"}
    {"cmd":"media-remote", "name": NAME, "size": BYTES}
    {"cmd":"media-tools"}
    {"cmd":"quit"}

Disk files
==========

While the VM is powered off, ``files-*`` commands manage the FAT drives of its
first hard disk, lettered C:, D: ... as DOS does (a running guest caches its
filesystem, so they fail while it is on). Paths are ``LETTER:/DIR/NAME``.
``files-drives`` returns ``drives`` (``letter``, ``fat``, ``bytes`` and, for
FAT drives, ``type``, ``label``, ``total``, ``free`` or ``error``).
``files-list`` returns ``items`` (``name``, ``short`` 8.3 name, ``dir``,
``size``, ``date``). ``files-get`` replies with ``size`` and ``binary: true``,
then one binary frame holding the file. ``files-put`` announces a file; send
its bytes as binary frames (one request each, at most 1 MiB), answered by
``{"type":"files-data","ok":true,"received":N}``. The last one also has
``saved: true``: the file is written (missing folders are created) and the disk
flushed. ``files-delete`` removes folders recursively; ``files-rename`` renames
within the same folder. Each command mounts the drive, does one operation and
unmounts it.

Virtual media
=============

The CD drive works like a server BMC's virtual media. ``media`` reports
``drive``, ``inserted``, ``tray_open``, ``locked`` (by the guest), ``name``
(only for a disc inserted through this server; null otherwise), ``remote``, ``tools``, ``saves``, ``limit`` and
``upload`` (null or ``kind``, ``name``, ``mine``, ``received``, ``files``).
Every other media command replies with the same object.
``saves`` is true when uploaded discs are saved as the VM's CD setting (the
handheld's save directory), false otherwise. The page uploads ISOs when it is
true and uses remote CD when it is false.

On-demand CD: open a dedicated connection and send
``{"cmd":"media-remote","name":NAME,"size":BYTES}``. BYTES must be a nonzero
multiple of 2048 within host/ATAPI capacity; the upload ``limit`` does not apply.
Insertion uses the same guest lock, busy-drive and UNIT ATTENTION rules as an
upload insert. Each insertion reports medium-absent then medium-changed on
successive commands other than INQUIRY, REQUEST SENSE, GET EVENT STATUS and
GET CONFIGURATION. Those failing commands advance the sequence; REQUEST SENSE
reports the last error and does not gate a retry. Polling guests notice even a
direct replacement or an eject/insert between polls. The reply has ``remote:true``
while the disc is inserted. Keep this connection open and serve each ``{"type":"media-read","id":N,
"offset":O,"length":L}`` with one binary message of exactly L bytes, or
``{"cmd":"media-read-error","id":N,"error":TEXT}``. Replies have no ack;
only one read is outstanding. Use a fresh connection for each insertion.
``media-eject`` on any connection uses the ordinary guest lock/busy rules.
Connection loss, a bad reply, a read error or a 10-second timeout ejects even a
locked disc; an active read fails with ATAPI MEDIUM ERROR. No reads are retried.

The server fetches aligned 256 KiB chunks (short final chunk) into a 64 MiB FIFO
cache. A fetch blocks the emulator core, including frame/input/audio service;
the page's main connection stays independent. ``remote_stats`` is null before
the first remote insertion, then reports ``requests``, successful ``chunks``
and ``bytes``, total ``stall_us`` and ``max_stall_us``. These counters include
fetch waits, survive eject, and reset on the next remote insertion. The CLI
``media-remote PATH`` serves a local image until interrupted.

``{"cmd":"media-tools"}`` inserts the server's public tools ISO with the name
``r86 tools``, under the same insert rules. It is file-backed and read on demand.
``--tools-iso PATH`` selects it; the default is ``r86_tools.iso`` beside the
executable. Missing is allowed at startup. Hello and media report ``tools:true``
when that file exists. The handheld uses the same executable-relative default.

An upload belongs to the connection that sent ``media-begin``; one runs at a
time across all clients, and its connection's disconnect discards it. Use a
separate connection so frame polling continues. Kind ``iso`` stages one ISO
image of ``size`` bytes (a multiple of 2048). Kind ``files`` builds a new disc:
``media-file`` announces each file, ``media-dir`` adds an empty directory, and
parent directories appear automatically. After ``media-begin`` (iso) or each
``media-file``, send exactly the announced bytes as binary frames; each frame
is one request, answered by ``{"type":"media-data","ok":true,"received":N}``.
The built disc has ISO 9660 level-1 names for DOS (MSCDEX) and Joliet names
for Windows. Uploads stay in host memory, up to ``limit`` bytes. The
handheld instead saves each inserted disc in its iso/ folder as
NAME-XXXXXXXX.iso, XXXXXXXX being the start of its SHA-1 (an ISO image streams
there while uploading), and keeps it as the VM's CD setting.

Import mode
-----------

With no VM running, the handheld's New VM wizard receives one disk or CD image
through the same page and port. Its hello has ``"import": "iso"`` or
``"import": "disk"`` instead of ``capabilities``, and only these requests work:

    {"cmd":"import-begin", "name": NAME, "size": BYTES}
    {"cmd":"import-end"}
    {"cmd":"import-cancel"}

After ``import-begin``, send exactly BYTES bytes as binary frames, each answered
by ``{"type":"import-data","ok":true,"received":N}``. ``import-end`` replies
``{"type":"import","ok":true,"name":SAVED}``. A CD image is whole 2048-byte
sectors; a disk image is a .vhd, .img or .raw file of whole 512-byte sectors.
The device saves it in its iso/ or vhd/ folder as NAME-XXXXXXXX.EXT, named as
above, and the wizard continues with it. Disconnecting discards an unfinished
image.

``media-insert`` completes the upload and changes the disc; the guest sees a
medium change (UNIT ATTENTION). If the guest has locked the drive or a read is
in flight, it fails and keeps the upload for another ``media-insert``.
``media-eject`` removes the disc. ``--ws-record`` logs both; replay applies
ejects and stops at an upload, remote or tools insert, because the image is not
recorded. Remote read traffic is not part of the replay stream.

Input semantics
===============

``type`` queues US-layout printable ASCII, tab and CR/LF (each CR/LF is Enter).
Unsupported characters or more than 4096 pending text bytes are rejected before
any input is accepted. The acknowledgement reports ``queued_bytes`` and the
global ``pending_text_bytes``, not an ``applied_step``. The latter count also
appears in ``execution`` status. It counts text waiting to enter the keyboard,
not application-level consumption. Commands append in acceptance order.

While execution runs or a debugger job advances it, one character is sent per
slice after the previous keyboard bytes drain. Disabled scanning, controller
commands and held symbolic keys defer text delivery. No text is sent while
paused, and a type request never advances execution itself. Accepted text
survives disconnect, so a one-shot CLI can queue text then run a separate wait.
``release-input`` cancels that client's pending text and releases its holds;
reset, power-off and emulator failure cancel all pending text. Cancellation is
reported in the shared event log. Recording logs each character at its actual
delivery step so replay reproduces device input rather than queue admission.

``keydown`` and ``keyup`` are layout-independent symbolic key events.
``send-scancodes`` injects raw set-2 bytes immediately, without bulk-text pacing;
it is a diagnostic primitive and remains subject to device queue capacity.

Mouse motion uses screen coordinates: positive dx moves right and positive dy
moves down. The emulator-side PS/2 core converts that to PS/2 packet sign
conventions. Each session latches its own mouse buttons; motion carries the union of all
clients' buttons until their matching button-up or disconnect. Symbolic keys
include left/right modifiers, arrows, navigation keys, F1-F12 and the keypad.

``pointer`` injects absolute input for the magic instruction guest tool. X/Y
are 0..65535, buttons are 0..7 (left/right/middle), and wheel is -32768..32767.
Its acknowledgement includes ``guest_pointer_active``. This diagnostic command
uses one input source; it does not combine buttons or release on disconnect.
Queue-full errors require retrying the same transition. Input is replayable.
The guest call protocol is defined in ``src/machine/magic.rs``.

``usb-tablet`` requires the opt-in ``--usb-tablet`` / TOML ``usb-tablet = true``.
X and Y are absolute coordinates 0..32767 (left/top to right/bottom); normalize
against the guest display, excluding host letterboxing. Buttons use bits 0/1/2
for left/right/middle and default to 0; wheel is -127..127 and defaults to 0
(positive scrolls up). Invalid values or a disabled tablet return an error.
Tablet buttons are combined across clients, independently of PS/2 buttons;
release-input or disconnect releases that client's tablet buttons at the latest
absolute position. Keep one connection open for a drag. Accepted input is
recorded with its applied step and can be replayed with the tablet enabled.
SDL, handheld and browser pointer UI wiring is separate from this command.

Display and binary frame format
===============================

``hash-frame`` returns a JSON object with the current frame hash. ``screenshot``
first returns a JSON acknowledgement and then sends one binary frame.
``frame`` is the cheaper polling command: its JSON contains ``generation`` and
``changed``; a binary frame follows only when changed is true. Pass the last
received generation as ``since``; omit it for a full frame, including after a
reconnect. All viewers share the rendered cache; one viewer cannot clear another
viewer's pending update. Dirty frames are converted only when queried, with no
hashing on this path. The Web UI polls at most 10 times per second while the
guest runs, once per second while paused, and not while powered off.
The binary header's step is the time of capture; the JSON step is the current
VM time. No display frames are pushed unsolicited.

Binary frame layout is little-endian and defined by ``FRAME_HEADER``:

    u32 magic      bytes "R86F"
    u16 version    currently 1
    u16 type       1 means RGBA
    u64 step       committed guest step when rendered
    u32 width
    u32 height
    u32 stride     bytes per row
    u32 format     1 means byte order R,G,B,A
    u8  pixels[]   exactly height * stride bytes

The pixel format is defined by byte position, not by host integer endianness:
pixels are R, G, B, A bytes in ascending memory order. The PPM writer below
drops alpha and writes RGB rows.

Web UI implementation notes
===========================

For a basic browser UI:

1. Open ``new WebSocket("ws://127.0.0.1:8765")`` and set
   ``binaryType = "arraybuffer"``.
2. Treat the first text message as hello.
3. Send JSON commands matching the schema above.
4. For screenshots/frames, parse the fixed binary header with a DataView using
   little-endian reads, then blit RGBA bytes to an ImageData-backed canvas.
5. Drive deterministic tests with explicit ``run-steps`` or wait commands.
   Do not use animation timing as guest-visible timing.
6. Surface ``step``, frame hash, text snapshot, serial output, and PS/2 drop
   counters in debug UI. Those are the fastest signals when a display/input
   regression appears.

For CI, prefer small scripts built on this client:

    script/ws_cli.py synthetic-smoke
    script/ws_cli.py wait-text 'tc@box:~$' --timeout-steps 3000000000
    script/ws_cli.py type "echo R86_WS_'TTY_OK'\\n"
    script/ws_cli.py wait-text R86_WS_TTY_OK --timeout-steps 200000000
"""
import argparse
import json
import os
from pathlib import Path
import struct
import sys
import time
import urllib.parse

from websockets.exceptions import ConnectionClosed
from websockets.sync.client import connect as ws_connect


FRAME_MAGIC = b"R86F"
FRAME_VERSION = 1
FRAME_TYPE_RGBA = 1
PIXEL_FORMAT_RGBA = 1
FRAME_HEADER = struct.Struct("<IHHQIIII")

HELP_EPILOG = """\
Protocol summary:
  text frames are UTF-8 JSON commands/responses with a required "cmd" field.
  binary screenshot frames use header <IHHQIIII followed by RGBA bytes:
  magic "R86F", version 1, type 1, step, width, height, stride, format 1.
  Requires the Python websockets package.
  See the module docstring at the top of this file for the full Web UI contract.
"""


class WsError(RuntimeError):
    """Protocol, validation, and harness errors surfaced as CLI failures."""

    pass


class WsClient:
    """Small client tailored to the r86-run WebSocket test protocol.

    The class deliberately exposes only the protocol pieces a harness needs:
    connect, read hello, send one JSON request, receive one JSON response, and
    receive an optional binary frame. RFC 6455 details are delegated to the
    ``websockets`` library.
    """

    def __init__(self, url, timeout_sec):
        parsed = urllib.parse.urlparse(url)
        if parsed.scheme != "ws":
            raise WsError("only ws:// URLs are supported")
        if not parsed.hostname:
            raise WsError("websocket URL needs a host")
        self.timeout_sec = timeout_sec
        self.conn = ws_connect(
            url,
            open_timeout=timeout_sec,
            close_timeout=timeout_sec,
            max_size=16 * 1024 * 1024,
            compression=None,
        )
        opcode, payload = self.recv_frame()
        if opcode != 0x1:
            raise WsError("server did not send hello JSON")
        self.hello = json.loads(payload.decode("utf-8"))

    def close(self):
        """Best-effort WebSocket close."""

        try:
            self.conn.close()
        except Exception:
            pass

    def request(self, obj):
        """Send one JSON command and return the next JSON response object."""

        self.send_json(obj)
        while True:
            opcode, payload = self.recv_frame()
            if opcode != 0x1:
                raise WsError(f"expected JSON text response, got opcode {opcode:#x}")
            msg = json.loads(payload.decode("utf-8"))
            return msg

    def send_json(self, obj):
        """Encode and send a compact UTF-8 JSON text frame."""

        data = json.dumps(obj, separators=(",", ":"), ensure_ascii=False)
        self.conn.send(data)

    def recv_frame(self):
        """Receive one complete WebSocket message as a text or binary frame."""

        try:
            message = self.conn.recv(timeout=self.timeout_sec)
        except ConnectionClosed as exc:
            raise WsError(f"websocket closed by server: {exc}") from exc
        if isinstance(message, str):
            return 0x1, message.encode("utf-8")
        if isinstance(message, bytes):
            return 0x2, message
        raise WsError(f"unsupported websocket message type {type(message).__name__}")


def decode_text(text):
    """Decode CLI escape sequences so '\\n' can be passed from the shell."""

    return text.encode("utf-8").decode("unicode_escape")


def print_json(obj):
    """Print one stable JSON object for shell scripts and CI logs."""

    print(json.dumps(obj, sort_keys=True, ensure_ascii=False))


def parse_frame(payload):
    """Parse an r86 binary RGBA frame and validate its fixed header."""

    if len(payload) < FRAME_HEADER.size:
        raise WsError("binary frame is shorter than the r86 header")
    magic, version, frame_type, step, width, height, stride, fmt = FRAME_HEADER.unpack_from(payload)
    if magic.to_bytes(4, "little") != FRAME_MAGIC:
        raise WsError("bad r86 frame magic")
    if version != FRAME_VERSION or frame_type != FRAME_TYPE_RGBA or fmt != PIXEL_FORMAT_RGBA:
        raise WsError("unsupported r86 frame format")
    pixels = payload[FRAME_HEADER.size :]
    expected = height * stride
    if len(pixels) != expected:
        raise WsError(f"bad r86 frame payload length: got {len(pixels)}, expected {expected}")
    return {
        "step": step,
        "width": width,
        "height": height,
        "stride": stride,
        "format": fmt,
        "pixels": pixels,
    }


def write_ppm(path, frame):
    """Write a parsed RGBA frame as a binary PPM image for dependency-free tests."""

    width = frame["width"]
    height = frame["height"]
    stride = frame["stride"]
    pixels = frame["pixels"]
    with open(path, "wb") as out:
        out.write(f"P6\n{width} {height}\n255\n".encode("ascii"))
        for row in range(height):
            src = pixels[row * stride : row * stride + width * 4]
            rgb = bytearray()
            for index in range(0, len(src), 4):
                rgb.extend(src[index : index + 3])
            out.write(rgb)


def build_parser():
    """Build the CLI parser.

    The CLI names map directly to JSON command names except for ``mouse``
    subcommands and ``synthetic-smoke``, which is a convenience macro command.
    """

    parser = argparse.ArgumentParser(
        description="WebSocket client for r86-run",
        epilog=HELP_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--url", default="ws://127.0.0.1:8765")
    parser.add_argument("--timeout-sec", type=float, default=7200.0)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("hello")
    sub.add_parser("status")
    sub.add_parser("power-button")
    for name in ("resume", "pause", "reset", "release-input", "diagnostics", "guest-bytes",
                 "media", "media-tools", "media-eject"):
        sub.add_parser(name)
    remote = sub.add_parser("media-remote", help="serve a local ISO on demand until interrupted")
    remote.add_argument("path")
    logs = sub.add_parser("logs")
    logs.add_argument("--serial-from", type=int, default=0)
    logs.add_argument("--event-from", type=int, default=0)
    logs.add_argument("--follow", action="store_true")
    memory = sub.add_parser("memory")
    memory.add_argument("address", type=lambda s: int(s, 0))
    memory.add_argument("--length", type=int, default=256)
    frame = sub.add_parser("frame")
    frame.add_argument("path")
    frame.add_argument("--since", type=int)

    run = sub.add_parser("run-steps")
    run.add_argument("steps", type=int)

    run_until = sub.add_parser("run-until-step")
    run_until.add_argument("step", type=int)

    wait_text = sub.add_parser("wait-text")
    wait_text.add_argument("text")
    wait_text.add_argument("--timeout-steps", type=int, default=250000)
    wait_text.add_argument("--slice-steps", type=int)

    wait_serial = sub.add_parser("wait-serial")
    wait_serial.add_argument("text")
    wait_serial.add_argument("--timeout-steps", type=int, default=250000)
    wait_serial.add_argument("--slice-steps", type=int)

    type_cmd = sub.add_parser("type")
    type_cmd.add_argument("text")

    keydown = sub.add_parser("keydown")
    keydown.add_argument("key")

    keyup = sub.add_parser("keyup")
    keyup.add_argument("key")

    sc = sub.add_parser("send-scancodes")
    sc.add_argument("hex")

    tablet = sub.add_parser("usb-tablet")
    tablet.add_argument("x", type=int)
    tablet.add_argument("y", type=int)
    tablet.add_argument("--buttons", type=int, default=0)
    tablet.add_argument("--wheel", type=int, default=0)

    pointer = sub.add_parser("pointer")
    pointer.add_argument("x", type=int)
    pointer.add_argument("y", type=int)
    pointer.add_argument("--buttons", type=int, default=0)
    pointer.add_argument("--wheel", type=int, default=0)

    mouse = sub.add_parser("mouse")
    mouse_sub = mouse.add_subparsers(dest="mouse_command", required=True)
    move = mouse_sub.add_parser("move")
    move.add_argument("dx", type=int)
    move.add_argument("dy", type=int)
    button = mouse_sub.add_parser("button")
    button.add_argument("button", choices=["left", "right", "middle"])
    button.add_argument("state", choices=["down", "up"])

    sub.add_parser("text-snapshot")
    sub.add_parser("hash-frame")

    screenshot = sub.add_parser("screenshot")
    screenshot.add_argument("path")

    stable = sub.add_parser("wait-frame-stable")
    stable.add_argument("count", type=int)
    stable.add_argument("--timeout-steps", type=int, default=250000)
    stable.add_argument("--slice-steps", type=int)

    synthetic = sub.add_parser("synthetic-smoke")
    synthetic.add_argument("--timeout-steps", type=int, default=1000000)
    synthetic.add_argument("--slice-steps", type=int, default=10000)

    sub.add_parser("quit")
    return parser


def command_object(args):
    """Translate parsed CLI arguments into one protocol JSON command object."""

    if args.command in ("resume", "pause", "reset", "release-input", "diagnostics", "guest-bytes",
                        "media", "media-tools", "media-eject"):
        return {"cmd": args.command}
    if args.command == "logs":
        return {"cmd": "logs", "serial_from": args.serial_from, "event_from": args.event_from}
    if args.command == "memory":
        return {"cmd": "memory", "address": args.address, "length": args.length}
    if args.command == "frame":
        return {"cmd": "frame", **({"since": args.since} if args.since is not None else {})}
    if args.command == "hello":
        return {"cmd": "hello"}
    if args.command == "status":
        return {"cmd": "status"}
    if args.command == "power-button":
        return {"cmd": "power-button"}
    if args.command == "run-steps":
        return {"cmd": "run-steps", "steps": args.steps}
    if args.command == "run-until-step":
        return {"cmd": "run-until-step", "step": args.step}
    if args.command == "wait-text":
        obj = {"cmd": "wait-text", "text": decode_text(args.text), "timeout_steps": args.timeout_steps}
        if args.slice_steps:
            obj["slice_steps"] = args.slice_steps
        return obj
    if args.command == "wait-serial":
        obj = {
            "cmd": "wait-serial",
            "text": decode_text(args.text),
            "timeout_steps": args.timeout_steps,
        }
        if args.slice_steps:
            obj["slice_steps"] = args.slice_steps
        return obj
    if args.command == "type":
        return {"cmd": "type", "text": decode_text(args.text)}
    if args.command == "keydown":
        return {"cmd": "keydown", "key": args.key}
    if args.command == "keyup":
        return {"cmd": "keyup", "key": args.key}
    if args.command == "send-scancodes":
        return {"cmd": "send-scancodes", "hex": args.hex}
    if args.command in ("usb-tablet", "pointer"):
        return {"cmd": args.command, "x": args.x, "y": args.y,
                "buttons": args.buttons, "wheel": args.wheel}
    if args.command == "mouse":
        if args.mouse_command == "move":
            return {"cmd": "mouse-move", "dx": args.dx, "dy": args.dy}
        return {"cmd": "mouse-button", "button": args.button, "down": args.state == "down"}
    if args.command == "text-snapshot":
        return {"cmd": "text-snapshot"}
    if args.command == "hash-frame":
        return {"cmd": "hash-frame"}
    if args.command == "screenshot":
        return {"cmd": "screenshot"}
    if args.command == "wait-frame-stable":
        obj = {
            "cmd": "wait-frame-stable",
            "count": args.count,
            "timeout_steps": args.timeout_steps,
        }
        if args.slice_steps:
            obj["slice_steps"] = args.slice_steps
        return obj
    if args.command == "synthetic-smoke":
        return {"cmd": "status"}
    if args.command == "quit":
        return {"cmd": "quit"}
    raise WsError(f"unknown command {args.command}")


def require_ok(response, label):
    """Raise a readable error when a multi-step smoke command fails."""

    if not response.get("ok", False):
        raise WsError(f"{label} failed: {response}")
    return response


def synthetic_smoke(client, args):
    """Run the checked-in bootsector's VGA, keyboard, and mouse smoke test.

    Expected guest behavior:

    * bootsector writes ``R86_SYN_VGA`` to VGA text memory;
    * bootsector initializes serial and PS/2 controller;
    * aux loopback and mouse enable produce ``M5A MFA``;
    * injected key ``a`` produces set-1 bytes ``K1E K9E`` after translation;
    * injected mouse delta (5, -3 screen convention) produces ``M08 M05 M03``.
    """

    out = {}
    wait = {
        "timeout_steps": args.timeout_steps,
        "slice_steps": args.slice_steps,
    }
    out["ready"] = require_ok(
        client.request({"cmd": "wait-serial", "text": "R86_SYN_READY ", **wait}), "ready"
    )
    require_ok(client.request({"cmd": "keydown", "key": "a"}), "keydown")
    require_ok(client.request({"cmd": "keyup", "key": "a"}), "keyup")
    out["keyboard"] = require_ok(
        client.request({"cmd": "wait-serial", "text": "K1E K9E ", **wait}), "keyboard"
    )
    require_ok(client.request({"cmd": "mouse-move", "dx": 5, "dy": -3}), "mouse-move")
    out["mouse"] = require_ok(
        client.request({"cmd": "wait-serial", "text": "M08 M05 M03 ", **wait}), "mouse"
    )
    out["frame"] = require_ok(client.request({"cmd": "hash-frame"}), "hash-frame")
    return {
        "ok": True,
        "type": "synthetic-smoke",
        "step": out["frame"]["step"],
        "hash": out["frame"]["hash"],
        "width": out["frame"]["width"],
        "height": out["frame"]["height"],
        "events": out,
    }


def serve_remote(client, path):
    """Serve one open ISO on this dedicated connection; never synthesize bytes."""
    with open(path, "rb") as image:
        size = os.fstat(image.fileno()).st_size
        if not size or size % 2048:
            raise WsError("remote ISO size must be a nonzero multiple of 2048")
        response = require_ok(client.request({
            "cmd": "media-remote", "name": Path(path).name, "size": size,
        }), "media-remote")
        print_json(response)
        sys.stdout.flush()
        client.timeout_sec = None  # An idle mounted disc has no lifetime limit.
        try:
            while True:
                opcode, payload = client.recv_frame()
                if opcode != 1:
                    raise WsError("expected media-read JSON")
                request = json.loads(payload)
                if request.get("type") != "media-read":
                    raise WsError(f"expected media-read, got {request}")
                ident, offset, length = (request.get(k) for k in ("id", "offset", "length"))
                if (any(type(n) is not int for n in (ident, offset, length)) or ident <= 0
                        or offset < 0 or not 0 < length <= 256 * 1024 or offset + length > size):
                    raise WsError(f"invalid media-read range: {request}")
                try:
                    image.seek(offset)
                    data = image.read(length)
                    if len(data) != length:
                        raise OSError(f"short ISO read: {len(data)} of {length} bytes")
                except OSError as exc:
                    client.send_json({"cmd": "media-read-error", "id": ident, "error": str(exc)})
                    raise
                client.conn.send(data)
        except KeyboardInterrupt:
            return 0


def main(argv):
    """CLI entry point used by both humans and test scripts."""

    args = build_parser().parse_args(argv)
    client = WsClient(args.url, args.timeout_sec)
    try:
        if args.command == "hello":
            print_json(client.hello)
            return 0
        if args.command == "media-remote":
            return serve_remote(client, args.path)
        if args.command == "synthetic-smoke":
            print_json(synthetic_smoke(client, args))
            return 0
        if args.command == "logs" and args.follow:
            try:
                while True:
                    response = client.request(command_object(args))
                    if not response.get("ok"):
                        print_json(response)
                        return 1
                    if response["serial_hex"] or response["events"] or args.event_from < response["event_first"]:
                        print_json(response)
                        sys.stdout.flush()
                    args.serial_from = response["serial_next"]
                    args.event_from = response["event_next"]
                    time.sleep(.25)
            except KeyboardInterrupt:
                return 0
        response = client.request(command_object(args))
        if response.get("ok") and (args.command == "screenshot" or (args.command == "frame" and response["changed"])):
            opcode, payload = client.recv_frame()
            if opcode != 0x2:
                raise WsError(f"expected screenshot binary frame, got opcode {opcode:#x}")
            frame = parse_frame(payload)
            write_ppm(args.path, frame)
            response["path"] = args.path
            response["bytes"] = len(payload)
        print_json(response)
        return 0 if response.get("ok", False) else 1
    finally:
        client.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (OSError, WsError, json.JSONDecodeError) as exc:
        print_json({"ok": False, "error": str(exc)})
        raise SystemExit(1)
