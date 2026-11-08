#!/usr/bin/env python3
"""Two OS boots: SeaBIOS/CD boots Linux to write a dynamic VHD and power off;
a new direct-kernel process mounts it read-only and compares every file byte.
Uses the ordinary interpreter and a guest ext2 filesystem, without snapshots.
QEMU then decodes the disk; host e2fsck and debugfs check the filesystem and file.
Outputs and disk images stay in the given directory; existing disks are refused.
"""
import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/r86-run"))
    parser.add_argument("--iso", type=Path, default=Path("refs/TinyCore-current.iso"))
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    tools = {}
    for name in ("qemu-img", "e2fsck", "debugfs", "bsdtar"):
        found = shutil.which(name) or f"/usr/sbin/{name}"
        if not Path(found).is_file():
            parser.error(f"required independent verifier not found: {name}")
        tools[name] = found
    out = args.output_dir.resolve()
    out.mkdir(parents=True, exist_ok=True)
    disk = out / "filesystem.vhd"
    if disk.exists():
        parser.error(f"refusing to reuse existing disk: {disk}")
    binary = str(args.binary.resolve())
    subprocess.run([binary, "--create-vhd", str(disk), "--size-mib", "32"], check=True)
    payload = out / "payload.bin"
    payload.write_bytes(bytes((i * 37 ^ (i >> 7) ^ (i >> 15)) & 255
                             for i in range(3 * 1024 * 1024 + 137)))
    common = [binary, "--iso", str(args.iso.resolve()), "--hdd", str(disk),
              "--host-decompress-kernel", "--steps", "3500000000",
              "--cmdline", "console=ttyS0,115200n8 loglevel=7 nozswap base norestore noswap",
              "--initrd-file", f"{payload}:tmp/vhd-payload"]
    setup = """set -eu
dev=/dev/sda
[ -b "$dev" ] || dev=/dev/hda
[ -b "$dev" ]
mkdir -p /mnt/vhd
"""
    write = setup + """printf 'n\\np\\n1\\n\\n\\nw\\n' | fdisk "$dev"
mkfs.ext2 -F "${dev}1"
mount -t ext2 "${dev}1" /mnt/vhd
cp /tmp/vhd-payload /mnt/vhd/payload.tmp
mv /mnt/vhd/payload.tmp /mnt/vhd/payload
sync
cmp /tmp/vhd-payload /mnt/vhd/payload
umount /mnt/vhd
echo R86_VHD_WRITE_OK
poweroff -f
"""
    read = setup + """mount -r -t ext2 "${dev}1" /mnt/vhd
cmp /tmp/vhd-payload /mnt/vhd/payload
umount /mnt/vhd
echo R86_VHD_REOPEN_OK
"""
    for image in ('vmlinuz', 'core.gz'):
        with (out / image).open('wb') as stream:
            subprocess.run(['bsdtar', '-xOf', str(args.iso), f'boot/{image}'], stdout=stream, check=True)
    for name, command, extra, marker in [
        ("write", write, ["--hdd-writable"], "R86_VHD_WRITE_OK"),
        ("reopen", read, ["--boot-linux-kernel", str(out / "vmlinuz"), "--boot-linux-initrd", str(out / "core.gz")], "R86_VHD_REOPEN_OK"),
    ]:
        log = out / f"{name}.log"
        before = hashlib.sha256(disk.read_bytes()).digest()
        print(f"{name}: {log}", flush=True)
        with log.open("wb") as stream:
            endpoint = [] if name == "write" else ["--expect-serial", marker]
            result = subprocess.run(common + extra + ["--bootlocal-command", command] + endpoint,
                                    stdout=stream, stderr=subprocess.STDOUT, timeout=300)
        if result.returncode != 0:
            raise SystemExit(f"{name} failed ({result.returncode}); inspect {log}")
        transcript = log.read_text(errors="replace")
        if marker not in transcript or (name == "write" and "guest powered off after" not in transcript):
            raise SystemExit(f"{name} did not reach its guest completion/shutdown endpoint; inspect {log}")
        if any(message in transcript for message in
               ("IDE disk I/O failed", "I/O error", "failed command:", "lost interrupt",
                "ACPI Error", "ACPI Warning")):
            raise SystemExit(f"{name} had a disk error despite its marker; inspect {log}")
        if name == "reopen" and hashlib.sha256(disk.read_bytes()).digest() != before:
            raise SystemExit("read-only reopen changed the disk image")
        if "inode tracked as orphan!" in transcript:
            print("guest ext4 orphan diagnostic present; retained in log (see design/DISKS.md)", flush=True)
        print(f"{marker}; VHD file length {disk.stat().st_size} bytes", flush=True)

    raw = out / "independent.raw"
    subprocess.run([tools["qemu-img"], "convert", "-f", "vpc", "-O", "raw",
                    str(disk), str(raw)], check=True)
    contents = raw.read_bytes()
    entry = contents[446:462]
    start = int.from_bytes(entry[8:12], "little") * 512
    end = start + int.from_bytes(entry[12:16], "little") * 512
    if not 0 < start < end <= len(contents):
        raise SystemExit("invalid guest-created MBR partition")
    partition = out / "partition.raw"
    partition.write_bytes(contents[start:end])
    subprocess.run([tools["e2fsck"], "-fn", str(partition)], check=True)
    extracted = out / "independent-payload.bin"
    subprocess.run([tools["debugfs"], "-R", "dump /payload independent-payload.bin",
                    str(partition)], cwd=out, check=True)
    if extracted.read_bytes() != payload.read_bytes():
        raise SystemExit("independent filesystem reader returned different bytes")
    print("QEMU + e2fsck + debugfs: filesystem and complete file match", flush=True)


if __name__ == "__main__":
    main()
