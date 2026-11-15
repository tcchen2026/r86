#!/usr/bin/env python3
"""Prepare a host PC reference and compare M33 text or a final RGBA frame.

Run pc.sh prepare first, then pc.sh jit. Optional PC_KEYS replays set-2
keyboard events at exact guest steps; !shot events produce a contact sheet.
Uses the existing WebSocket client. The disk stays frozen.
"""
import argparse
import hashlib
import json
import re
from pathlib import Path
import socket
import struct
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "script"))
from ws_cli import WsClient, parse_frame

PROMPT = "C:\\>"


def read_keys(path, limit):
    """Text ABI shared with runtime.rs: bounded, ordered steps and set-2 bytes."""
    data = path.read_bytes()
    if len(data) > 65536:
        raise ValueError("key script exceeds 64 KiB")
    events, names = [], set()
    previous, last_key = 0, None
    for line_number, line in enumerate(data.decode("ascii").splitlines(), 1):
        fields = line.split("#", 1)[0].split()
        if not fields:
            continue
        step = int(fields[0])
        if not previous <= step <= limit or len(fields) < 2:
            raise ValueError(f"key script line {line_number}: step/order")
        previous = step
        if fields[1] == "!shot":
            if (len(fields) != 3 or not re.fullmatch(r"[A-Za-z0-9_-]+", fields[2])
                    or fields[2] in names):
                raise ValueError(f"key script line {line_number}: shot name")
            names.add(fields[2])
            events.append((step, "shot", fields[2]))
        else:
            if (not 1 <= len(fields[1:]) <= 16
                    or any(not re.fullmatch(r"[0-9a-fA-F]{2}", b) for b in fields[1:])
                    or last_key is not None and step - last_key < 2_000_000):
                raise ValueError(f"key script line {line_number}: bytes/key spacing")
            last_key = step
            events.append((step, "key", " ".join(fields[1:])))
        if len(events) > 512:
            raise ValueError("key script exceeds 512 events")
    return data, events


def contact_sheet(out, shots):
    from PIL import Image, ImageDraw
    if not shots:
        return
    sheet = Image.new("RGB", (960, 268 * ((len(shots) + 2) // 3)), "white")
    draw = ImageDraw.Draw(sheet)
    for index, (step, name) in enumerate(shots):
        with Image.open(out / "shots" / f"{name}.png") as frame:
            thumb = frame.convert("RGB")
            thumb.thumbnail((320, 240))
            x, y = index % 3 * 320, index // 3 * 268
            sheet.paste(thumb, (x, y))
            draw.text((x + 4, y + 242), f"{name}  ({step:,})", fill="black")
    sheet.save(out / "contact.png")


def save_frame(out, name, width, height, pixels):
    from PIL import Image
    assert len(pixels) == width * height * 4, "frame length"
    (out / f"{name}.frame").write_bytes(struct.pack("<II", width, height) + pixels)
    Image.frombytes("RGBA", (width, height), pixels).save(out / f"{name}.png")


def read_frame(path):
    data = path.read_bytes()
    width, height = struct.unpack_from("<II", data)
    assert len(data) == 8 + width * height * 4, "frame length"
    return width, height, data[8:]


def frame_hash(width, height, pixels):
    # src/frontend/ws.rs: width, height, RGBA format tag, pixels.
    return hashlib.sha256(struct.pack("<III", width, height, 1) + pixels).hexdigest()


def prepare(out, host, limit, pass_rule, keys):
    if pass_rule == "frame" and limit is None:
        raise ValueError("frame reference requires --limit after observing the desktop")
    events, key_hash = [], None
    if keys is not None:
        if limit is None:
            raise ValueError("key script requires --limit")
        data, events = read_keys(keys, limit)
        (out / "keys.txt").write_bytes(data)
        key_hash = hashlib.sha256(data).hexdigest()
        (out / "shots").mkdir(exist_ok=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    command = [str(host), "--bios", "firmware/seabios/bios.bin",
               "--vga-bios", "firmware/seabios/vgabios-stdvga.bin",
               "--hdd", str(out / "disk.img"), "--hdd-frozen", "--no-cdrom",
               "--mem-mib", "16", "--clock", "deterministic", "--cpu-engine", "interp",
               "--ws-listen", f"127.0.0.1:{port}"]
    client = None
    with (out / "host.log").open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if process.poll() is not None:
                    raise RuntimeError("host exited; see host.log")
                try:
                    client = WsClient(f"ws://127.0.0.1:{port}/ws", 300)
                    break
                except OSError:
                    time.sleep(0.1)
            if client is None:
                raise RuntimeError("host WebSocket did not start")
            prompt_step = None
            if limit is None:
                found = client.request({"cmd": "wait-text", "text": PROMPT,
                                        "timeout_steps": 500_000_000, "slice_steps": 100_000})
                (out / "host-prompt.json").write_text(json.dumps(found, indent=2) + "\n")
                if not found.get("ok") or not found.get("matched"):
                    raise RuntimeError(f"DOS prompt not found: {found}")
                prompt_step = found["step"]
                limit = prompt_step + 1_000_000
            acknowledgements, shots = [], []
            for step, kind, value in events:
                result = client.request({"cmd": "run-until-step", "step": step})
                if not result.get("ok") or result["step"] != step:
                    raise RuntimeError(f"host stopped before key event: {result}")
                if kind == "key":
                    ack = client.request({"cmd": "send-scancodes", "hex": value})
                    if not ack.get("ok") or ack["step"] != step or ack["applied_step"] != step:
                        raise RuntimeError(f"key event step mismatch: {ack}")
                else:
                    ack = client.request({"cmd": "screenshot"})
                    if not ack.get("ok") or ack["step"] != step:
                        raise RuntimeError(f"shot step mismatch: {ack}")
                    opcode, payload = client.recv_frame()
                    assert opcode == 2
                    frame = parse_frame(payload)
                    save_frame(out / "shots", value, frame["width"], frame["height"], frame["pixels"])
                    shots.append((step, value))
                acknowledgements.append(ack)
            if events:
                (out / "events.json").write_text(json.dumps(acknowledgements, indent=2) + "\n")
                contact_sheet(out, shots)
            result = client.request({"cmd": "run-until-step", "step": limit})
            if not result.get("ok") or result["step"] != limit:
                raise RuntimeError(f"host stopped: {result}")
            snapshot = client.request({"cmd": "text-snapshot"})
            if not snapshot.get("ok") or (pass_rule == "dos" and PROMPT not in snapshot["text"]):
                raise RuntimeError(f"host lost prompt: {snapshot}")
            (out / "host-text.txt").write_text(snapshot["text"])
            shot = client.request({"cmd": "screenshot"})
            if not shot.get("ok"):
                raise RuntimeError(f"host screenshot failed: {shot}")
            opcode, payload = client.recv_frame()
            assert opcode == 2
            frame = parse_frame(payload)
            save_frame(out, "host", frame["width"], frame["height"], frame["pixels"])
            (out / "host-run.json").write_text(json.dumps({
                "command": command, "prompt_step": prompt_step, "limit": limit,
                "pass_rule": pass_rule, "snapshot": snapshot, "frame": shot,
                "keys_sha256": key_hash,
            }, indent=2) + "\n")
            print(f"host: prompt_step={prompt_step}; limit={limit}; frame={shot['hash']}")
        finally:
            if client is not None:
                client.close()
            process.terminate()
            process.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "config", "check"])
    parser.add_argument("--out", type=Path, default=ROOT / "tmp/cortex-m/pc")
    parser.add_argument("--host", type=Path)
    parser.add_argument("--engine", choices=["jit", "interp"], default="jit")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--keys", type=Path)
    parser.add_argument("--pass-rule", choices=["dos", "frame"], default="dos")
    args = parser.parse_args()
    out = args.out.resolve()
    if args.action == "prepare":
        prepare(out, args.host.resolve(), args.limit, args.pass_rule, args.keys)
        return
    reference = json.loads((out / "host-run.json").read_text())
    limit = reference["limit"]
    if args.action == "config":
        # abi.rs: PC bit 16; shared machine_abi.rs: JIT bit 2.
        flags = 16 | (2 if args.engine == "jit" else 0)
        if reference.get("keys_sha256"):
            data, _ = read_keys(out / "keys.txt", limit)
            assert hashlib.sha256(data).hexdigest() == reference["keys_sha256"], "reference keys changed"
            if args.keys is not None:
                assert args.keys.read_bytes() == data, "PC_KEYS differs from host reference"
            flags |= 32
        elif args.keys is not None:
            raise ValueError("prepare the host reference with PC_KEYS first")
        (out / "config.bin").write_bytes(struct.pack("<8I", 0x5238364d, 1, 0, 0,
                                                   16 << 20, limit & 0xffffffff,
                                                   limit >> 32, flags))
        return
    text = (out / f"{args.engine}.out").read_text().replace("\r\n", "\n")
    screen = text.split("screen:\n", 1)[1].split("\nend-screen\n", 1)[0]
    for line in text.splitlines():
        if line.startswith(("result:", "cost:", "memory:", "t32 jit:", "batch_modes:", "profile:", "jit:")):
            print(line)
    assert f"x86_steps={limit} " in text, "M33 did not reach the exact step limit"
    width, height, pixels = read_frame(out / f"{args.engine}.frame")
    save_frame(out, args.engine, width, height, pixels)
    actual_hash = frame_hash(width, height, pixels)
    comparison = {"hash": actual_hash, "width": width, "height": height}
    if (out / "host.frame").exists():
        w, h, expected = read_frame(out / "host.frame")
        different = None
        bbox = None
        if (w, h) == (width, height):
            different = [i for i in range(w * h)
                         if pixels[4*i:4*i+4] != expected[4*i:4*i+4]]
            if different:
                bbox = [min(i % w for i in different), min(i // w for i in different),
                        max(i % w for i in different) + 1, max(i // w for i in different) + 1]
        comparison.update(host_hash=frame_hash(w, h, expected),
                          equal=different == [],
                          different_pixels=None if different is None else len(different),
                          bbox_exclusive=bbox)
    (out / f"{args.engine}.comparison.json").write_text(json.dumps(comparison, indent=2) + "\n")
    print("frame: " + json.dumps(comparison))
    if reference.get("pass_rule", "dos") == "frame":
        assert comparison.get("equal"), "M33 and host frames differ (same step limit)"
        rule = "frame equals host"
    else:
        assert PROMPT in screen, "DOS prompt absent"
        assert screen == (out / "host-text.txt").read_text(), "M33 and host text differ"
        rule = "DOS prompt and text equal host"
    print(f"PASS pc {args.engine}: {rule} at {limit} steps")


if __name__ == "__main__":
    main()
