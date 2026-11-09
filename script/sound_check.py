#!/usr/bin/env python3
"""Build the original SB16 bootsector and check its deterministic IRQ/WAV output.

Requires NASM and a built r86-run. Keeps generated media/logs in --output-dir.
No game, firmware or reference implementation is embedded in this script.
"""

import argparse
import hashlib
from pathlib import Path
import struct
import subprocess
import wave


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--bios", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    out = args.output_dir.resolve()
    out.mkdir(parents=True, exist_ok=True)
    boot, wav, log = (out / name for name in ("sound.bin", "sb16-golden.wav", "sb16-golden.log"))
    subprocess.run(["nasm", "-f", "bin", str(root / "tests/bootsectors/sound.asm"), "-o", str(boot)], check=True)
    command = [
        str(args.binary.resolve()), "--bootsector", str(boot),
        "--clock", "deterministic", "--no-cdrom", "--no-vga-bios",
        "--bios", str(args.bios.resolve()), "--mem-mib", "2",
        "--steps", "30000000", "--expect-serial", "R86_SOUND_OK",
        "--audio-wav", str(wav),
    ]
    with log.open("w") as output:
        subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, check=True)
    with wave.open(str(wav), "rb") as audio:
        assert (audio.getnchannels(), audio.getsampwidth(), audio.getframerate()) == (2, 2, 48000)
        frames = audio.getnframes()
        pcm = list(struct.iter_unpack("<hh", audio.readframes(frames)))
    assert frames == 48111, frames
    assert all(left == right for left, right in pcm), "mono samples must reach both outputs"
    assert {left for left, _ in pcm} == {-24576, 0, 24576}
    digest = hashlib.sha256(wav.read_bytes()).hexdigest()
    expected = "3b5efc5da1c89eea03940a77e5b0343f002705458b35d41267859fda5d377025"
    assert digest == expected, f"WAV changed: {digest}; inspect guest timing and PCM before updating the golden"
    print(f"SB16 IRQ and WAV passed: {frames} frames, SHA256 {digest}")


if __name__ == "__main__":
    main()
