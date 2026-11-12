#!/usr/bin/env python3
"""Mac CI gates with public GitHub check annotations for Intern/Lead review.

Native tests and the ROM compare JIT execution with the shared interpreter.
ReactOS uses the existing complete-language-dialog observer and golden frame.
Each run starts a fresh process/arena; no Windows media or writable disk is used.
Commands, logs and frames remain in --output for artifact upload on failure too.
Hosted macOS has SIP off: signing/region results there are observations only.
Use --require-signing-enforcement on a real Mac with SIP enabled for strict checks.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parent.parent
FRAME_HASH = "1fe40c340900687b5bcad3a5ee58672c0e9fccf140a31c8aa01774e73c2e997a"
# benchmarks/reactos_livecd_boot_to_langsel/README.md owns the input/endpoint.
REACTOS_URL = ("https://downloads.sourceforge.net/project/reactos/ReactOS/0.4.15/"
               "ReactOS-0.4.15-release-1-gdbb43bbaeb2-x86-live.zip")
REACTOS_ZIP_HASH = "ab5e4a1686b0c6d8c61b5c536dd94de36befe1d2345cec1b865f3ca43b2b2e54"
REACTOS_ISO_HASH = "ed048117879a9651c5a721d04a94447f37704fdb82770ce20936468c2cf9a56e"
REACTOS_FRAME_HASH = "eb1fb32ccab1c749c00f5321f3ecdd1aa3bf62b7802a70e60d6520c2cc109ef8"


def annotation(title, value, level="notice"):
    message = json.dumps(value, sort_keys=True)
    message = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::{level} title={title}::{message}", flush=True)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(command, log, timeout=180):
    command = list(map(str, command))
    log.with_suffix(".command.json").write_text(json.dumps(command, indent=2) + "\n")
    print("Running: " + json.dumps(command), flush=True)
    # Write directly to disk so a timeout retains the partial guest diagnostics.
    try:
        with log.open("w") as output:
            result = subprocess.run(command, cwd=ROOT, text=True, stdout=output,
                                    stderr=subprocess.STDOUT, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"{log.name}: timed out after {timeout}s; "
                           + log.read_text(errors="replace")[-4000:]) from error
    result.stdout = log.read_text(errors="replace")
    print(result.stdout, flush=True)
    return result


def check_status(result):
    # Check annotations are readable without log/artifact authentication.
    require(result.returncode == 0,
            f"command exited {result.returncode}: {result.args}; {result.stdout[-4000:]}")


def diagnostics(output, label, native_required, require_host_counters=False):
    native = re.search(r"native_steps: (\d+)", output)
    native_steps = int(native[1]) if native else 0
    require(not native_required or native_steps > 0, "native execution missing")
    if native_required:
        protection = re.search(
            r"jit protection: writes=(\d+) executes=(\d+) seconds=([\d.]+) "
            r"toggles_per_second=([\d.]+)", output)
        require(protection is not None, "Mac write/execute report missing")
        writes, executes = map(int, protection.group(1, 2))
        seconds, rate = map(float, protection.group(3, 4))
        require(min(writes, executes) > 0 and seconds > 0, "no protection transitions")
        links = re.search(r"jit links: out_of_range_attempts=(\d+)", output)
        require(links is not None, "Mac link-range report missing")
        annotation(f"{label} protection", dict(
            native_steps=native_steps, writes=writes, executes=executes,
            seconds=seconds, toggles_per_second=rate,
            out_of_range_link_attempts=int(links[1]),
            scope="arena lifetime, including compilation and observation"))
    counters = re.search(r"host counters: ([^\n]+)", output)
    require(counters is not None, "proc_pid_rusage report missing")
    counts = re.search(r"instructions=(\d+) cycles=(\d+)", counters[1])
    available = counts is not None and min(map(int, counts.groups())) > 0
    annotation(f"{label} host counters", dict(
        status="available" if available else "unavailable",
        report=counters[1], native_steps=native_steps,
        api="proc_pid_rusage(RUSAGE_INFO_V4)",
        note="process-wide; hosted VMs may return zero"))
    require(not require_host_counters or available, "Apple PMU counters unavailable")
    return native_steps


def native_tests(work):
    filters = ["x86::jit::a64::tests", "x86::jit::compile::tests", "x86::jit::code::",
               "jit_data_segment_loads", "jit_cross_page_calls", "jit_return_cache",
               "jit_returns_to_zero", "jit_refills_preserve", "jit_code_rechecks",
               "jit_stack_read_page", "apple::runner::tests", "apple::tests",
               "apple::vm_store::tests"]
    result = run(["cargo", "test", "--locked", "--release", "--no-default-features",
                  "--features", "apple,jit", "--lib", "--", *filters, "--test-threads=1"],
                 work / "native-tests.log", timeout=1200)
    summary = re.search(r"test result: (\w+)\. (\d+) passed; (\d+) failed; "
                        r"(\d+) ignored; (\d+) measured; (\d+) filtered out", result.stdout)
    if summary is None:
        check_status(result)
    require(summary is not None, "native test summary missing")
    passed, failed, ignored, measured, filtered = map(int, summary.groups()[1:])
    groups = {prefix: len(re.findall(r"^test " + re.escape(prefix) + r"\S* \.\.\. ok$",
                                     result.stdout, re.MULTILINE))
              for prefix in ("x86::jit::a64::tests::", "x86::jit::compile::tests::",
                             "x86::jit::code::", "machine::", "apple::")}
    annotation("Mac native tests", dict(result=summary[1], passed=passed, failed=failed,
               ignored=ignored, measured=measured, filtered=filtered, groups=groups,
               signing="ordinary Cargo executable, no Hardened Runtime entitlement"))
    check_status(result)
    require(passed > 0 and all(groups.values()), "a required native test group did not run")


def rom_command(binary, rom, engine):
    return [binary.resolve(), "--bios", rom, "--no-vga-bios", "--no-cdrom",
            "--mem-mib", "16", "--clock", "deterministic", "--steps", "10000000",
            "--expect-frame-hash", FRAME_HASH,
            *(["--no-jit"] if engine == "interp" else ["--cpu-engine", "jit"])]


def assemble_rom(work):
    rom = work / "guest.bin"
    subprocess.run(["nasm", "-f", "bin", ROOT / "tests/roms/wasm_jit_lfb.asm",
                    "-o", rom], check=True)
    return rom


def boot_rom(binary, work, label, require_host_counters=False):
    rom = assemble_rom(work)
    observations = []
    for engine in ("interp", "jit"):
        result = run(rom_command(binary, rom, engine), work / f"{label}-{engine}.log")
        check_status(result)
        require("RGBK" in result.stdout, "guest assertions failed")
        frame = re.search(r"frame hash at step (\d+): ([0-9a-f]{64}) (\d+x\d+)", result.stdout)
        require(frame is not None and frame[2] == FRAME_HASH, "ROM frame hash missing or wrong")
        native = diagnostics(result.stdout, f"{label} {engine}", engine == "jit",
                             require_host_counters)
        require(engine != "jit" or native > 3_600_000, "guest must execute native loops")
        observations.append((frame[1], frame[2], frame[3]))
        annotation(f"{label} {engine} boot", dict(
            clock="deterministic", guest_steps=int(frame[1]), frame_sha256=frame[2],
            dimensions=frame[3], native_steps=native, guest_marker="RGBK"))
    require(observations[0] == observations[1], "ROM engines disagree on steps/frame")


def sign(binary, work, name, entitlements):
    signed = work / name
    shutil.copy2(binary, signed)
    plist = work / f"{name}.plist"
    plist.write_bytes(plistlib.dumps(entitlements))
    subprocess.run(["codesign", "--force", "--sign", "-", "--options", "runtime",
                    "--timestamp=none", "--entitlements", plist, signed], check=True)
    subprocess.run(["codesign", "--verify", "--strict", signed], check=True)
    actual = subprocess.check_output(["codesign", "-d", "--entitlements", "-", signed])
    require(plistlib.loads(actual) == entitlements, "signed entitlements differ")
    flags = subprocess.run(["codesign", "-dv", signed], text=True,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=True).stdout
    require(re.search(r"flags=.*\(.*runtime.*\)", flags), "Hardened Runtime flag missing")
    annotation(f"{name} signature", dict(entitlements=entitlements, hardened_runtime=True,
                                         source_sha256=digest(binary),
                                         verification="signature metadata, not policy enforcement"))
    return signed


def signing_environment(work, label, strict):
    sip = run(["/usr/bin/csrutil", "status"], work / f"{label}-sip.log")
    enabled = sip.returncode == 0 and sip.stdout.strip() == "System Integrity Protection status: enabled."
    if "status: disabled" in sip.stdout:
        note = "runner does not enforce code signing (SIP off)"
    elif strict and enabled:
        note = "real-Mac check; enforcement also requires the negative control to pass"
    else:
        note = "observation only; signing enforcement not established"
    environment = dict(sip=sip.stdout.strip(), sip_enabled=enabled, strict=strict,
                       kernel=platform.release(), macos=platform.mac_ver()[0], note=note)
    annotation(f"{label} environment", environment)
    require(not strict or enabled, "--require-signing-enforcement requires a real Mac with SIP enabled")
    return environment


def observe_no_entitlement(result, environment, strict):
    error = re.search(r"[^\n]*JIT code mapping:[^\n]*", result.stdout)
    native = re.search(r"native_steps: (\d+)", result.stdout)
    guest_executed = "RGBK" in result.stdout or (native is not None and int(native[1]) > 0)
    denied = (result.returncode > 0 and error is not None
              and "mmap(MAP_JIT) failed" in error[0]
              and "com.apple.security.cs.allow-jit" in error[0]
              and not guest_executed and native is None)
    annotation("Hardened Runtime without allow-jit", dict(
        environment=environment, exit_code=result.returncode, arena_denied=denied,
        guest_executed=guest_executed, native_steps=int(native[1]) if native else None,
        diagnostic=error[0] if error else result.stdout[-2000:],
        enforcement_verified=strict and denied))
    if strict:
        require(denied, f"missing entitlement must fail clearly at arena creation, without a signal; "
                f"exit={result.returncode}; {result.stdout[-4000:]}")


def hardened(binary, work, require_host_counters, strict):
    environment = signing_environment(work, "Hardened Runtime", strict)
    allowed = sign(binary, work, "hardened-allow-jit", {"com.apple.security.cs.allow-jit": True})
    boot_rom(allowed, work, "hardened", require_host_counters)
    denied = sign(binary, work, "hardened-no-entitlement", {})
    result = run(rom_command(denied, assemble_rom(work), "jit"), work / "hardened-denied.log")
    observe_no_entitlement(result, environment, strict)


def read_regions(binary, work, label):
    result = run([binary.resolve()], work / f"{label}.log")
    check_status(result)
    report = re.search(r"^MAP_JIT_REGIONS (.+)$", result.stdout, re.MULTILINE)
    require(report is not None, "MAP_JIT lifetime report missing")
    values = json.loads(report[1])
    for key in ("first_errno", "first_release_errno", "sequential_errno", "concurrent_errno",
                "sequential_release_errno", "concurrent_release_errno"):
        require(key in values and (values[key] is None or type(values[key]) is int),
                f"invalid region outcome: {key}")
    return values


def regions(binary, work, strict):
    environment = signing_environment(work, "MAP_JIT regions", strict)
    signed = sign(binary, work, "regions-allow-jit", {"com.apple.security.cs.allow-jit": True})
    values = read_regions(signed, work, "regions-allow-jit")
    negative_errno = None
    if strict:
        negative = sign(binary, work, "regions-no-entitlement", {})
        negative_errno = read_regions(negative, work, "regions-no-entitlement")["first_errno"]
    outcomes = {}
    for name in ("first", "sequential", "concurrent"):
        error = values[f"{name}_errno"]
        outcomes[name] = dict(status="not_tested" if error is None else "ok" if error == 0 else "error",
                              errno=error, error=os.strerror(error) if error else None,
                              munmap_errno=values[f"{name}_release_errno"])
    annotation("MAP_JIT region lifetimes", dict(
        environment=environment, pid=values["pid"], bytes=values["bytes"], **outcomes,
        negative_control_errno=negative_errno,
        enforcement_verified=(strict and values["first_errno"] == 0
                              and negative_errno is not None and negative_errno > 0),
        scope="one process, one thread, syscall lifetime only; no guest or generated code",
        contract="observed leniency does not replace Apple's documented one-region limit"))
    # Policy denials are data for Lead's design decision, not a reason to turn
    # hosted observations into a policy gate. Failed teardown invalidates a probe.
    for name in ("first", "sequential", "concurrent"):
        if values[f"{name}_errno"] == 0:
            require(values[f"{name}_release_errno"] == 0, f"{name} munmap failed")
    if strict:
        require(values["first_errno"] == 0, "allow-jit did not permit the initial region")
        require(negative_errno is not None and negative_errno > 0,
                "region negative control did not establish code-signing enforcement")


def download_reactos(work):
    archive = work / "reactos-live.zip"
    result = run(["curl", "--fail", "--silent", "--show-error", "--location",
                  "--retry", "3", "--max-time", "300",
                  REACTOS_URL, "--output", archive], work / "download.log", timeout=1200)
    check_status(result)
    require(digest(archive) == REACTOS_ZIP_HASH, "ReactOS ZIP SHA256 mismatch")
    iso = work / "ReactOS.iso"
    with zipfile.ZipFile(archive) as bundle:
        members = [name for name in bundle.namelist() if name.lower().endswith(".iso")]
        require(len(members) == 1, "expected exactly one LiveCD ISO")
        with bundle.open(members[0]) as source, iso.open("wb") as dest:
            shutil.copyfileobj(source, dest)
    require(digest(iso) == REACTOS_ISO_HASH, "ReactOS ISO SHA256 mismatch")
    annotation("ReactOS input", dict(url=REACTOS_URL, zip_sha256=REACTOS_ZIP_HASH,
                                     iso_sha256=REACTOS_ISO_HASH))


def reactos(binary, iso, work, require_host_counters):
    require(digest(iso) == REACTOS_ISO_HASH, "ReactOS ISO SHA256 mismatch")
    frame = work / "reactos.ppm"
    firmware = ROOT / "firmware/seabios"
    result = run([binary.resolve(), iso.resolve(), firmware / "bios.bin",
                  firmware / "vgabios-stdvga.bin", "jit", frame, "600", "100", "realtime"],
                 work / "reactos.log", timeout=630)
    check_status(result)
    endpoint = re.search(r"REACTOS_LANGUAGE_DIALOG elapsed=([\d.]+) steps=(\d+) polls=(\d+)",
                         result.stdout)
    require(endpoint is not None, "complete ReactOS dialog was not reached")
    frame_hash = digest(frame)
    native = diagnostics(result.stdout, "ReactOS", True, require_host_counters)
    annotation("ReactOS language dialog", dict(
        clock="realtime", frame_ms=100, ram_mib=128, guest_steps=int(endpoint[2]),
        native_steps=native, elapsed=float(endpoint[1]), polls=int(endpoint[3]),
        frame_sha256=frame_hash, iso_sha256=REACTOS_ISO_HASH,
        bios_sha256=digest(firmware / "bios.bin"),
        vga_bios_sha256=digest(firmware / "vgabios-stdvga.bin"),
        note="single CI observation, not a performance comparison"))
    require(frame_hash == REACTOS_FRAME_HASH,
            f"complete ReactOS frame SHA256 mismatch: {frame_hash}, expected {REACTOS_FRAME_HASH}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "tmp/apple-jit-ci")
    parser.add_argument("--require-host-counters", action="store_true",
                        help="real-device check; hosted VM zero counters are normally unavailable")
    parser.add_argument("--require-signing-enforcement", action="store_true",
                        help="real Mac with SIP enabled: require no-entitlement denial; never enable in hosted CI")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("tests")
    commands.add_parser("download-reactos")
    for name in ("rom", "hardened", "regions", "reactos"):
        sub = commands.add_parser(name)
        sub.add_argument("binary", type=Path)
        if name == "reactos":
            sub.add_argument("iso", type=Path)
    args = parser.parse_args()
    work = args.output.resolve()
    work.mkdir(parents=True, exist_ok=True)
    require(sys.platform == "darwin", "these gates require a native Apple silicon Mac")
    if args.command == "tests":
        native_tests(work)
    elif args.command == "rom":
        boot_rom(args.binary, work, "adhoc", args.require_host_counters)
    elif args.command == "hardened":
        hardened(args.binary, work, args.require_host_counters, args.require_signing_enforcement)
    elif args.command == "regions":
        regions(args.binary, work, args.require_signing_enforcement)
    elif args.command == "download-reactos":
        download_reactos(work)
    else:
        reactos(args.binary, args.iso, work, args.require_host_counters)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError, zipfile.BadZipFile,
            json.JSONDecodeError) as error:
        annotation("Mac JIT gate failed", str(error), "error")
        sys.exit(1)
