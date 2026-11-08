#!/usr/bin/env python3
"""Compare r86 and tiny386 generated SoftMMU code.

The report is intentionally based on linked release binaries. Source-level
similarity is not evidence that LLVM and GCC produced comparable hot paths.
Instruction and memory-access counts are approximate static metrics; runtime
profiles and boot timings remain the performance gates.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import pathlib
import re
import subprocess
from collections.abc import Callable, Iterable


@dataclasses.dataclass(frozen=True)
class Symbol:
    address: int
    size: int
    kind: str
    name: str


@dataclasses.dataclass(frozen=True)
class FunctionMetrics:
    instructions: int
    branches: int
    conditional_branches: int
    calls: int
    returns: int
    indirect_transfers: int
    memory_operands: int
    probable_loads: int
    probable_stores: int


FUNCTIONS = {
    "r86": (
        ("interpreter", "x86_core::interp::Interpreter::step"),
        ("batch_runner", "x86_core::interp::Interpreter::run_batch_inner"),
        ("two_byte_cold", "x86_core::interp::execute_0f"),
        ("scalar_io_cold", "x86_core::interp::execute_scalar_io"),
        ("lazy_flag", "x86_core::cpu::Cpu::flag"),
        ("segment_update", "x86_core::cpu::Cpu::update_flat_segment_mask"),
        ("linear_read8", "x86_core::mmu::read_linear_u8_at_priv"),
        ("linear_read16", "x86_core::mmu::read_linear_u16_at_priv"),
        ("linear_read32", "x86_core::mmu::read_linear_u32_at_priv"),
        ("cached_read32", "x86_core::mmu::try_cached_linear_read_u32"),
        ("linear_write8", "x86_core::mmu::write_linear_u8_at_priv"),
        ("linear_write16", "x86_core::mmu::write_linear_u16_at_priv"),
        ("linear_write32", "x86_core::mmu::write_linear_u32"),
        ("fetch_slow", "x86_core::interp::fetch_u8_slow"),
        ("fetch_miss", "x86_core::interp::fetch_u8_miss"),
        ("address_decode", "x86_core::interp::decode_mem_addr"),
        ("address_decode_fast", "x86_core::interp::decode_mem_addr32_nosib"),
        ("address_decode_slow", "x86_core::interp::decode_mem_addr_slow"),
        ("read_mem16", "x86_core::interp::read_mem_u16_at"),
        ("read_mem32", "x86_core::interp::read_mem_u32_at"),
        ("write_mem16", "x86_core::interp::write_mem_u16_at"),
        ("stack_push32", "x86_core::interp::push_u32"),
        ("stack_pop32", "x86_core::interp::pop_u32"),
        ("group81", "x86_core::interp::group_81"),
        ("group3_byte", "x86_core::interp::group_f6"),
        ("rep_movs", "x86_core::interp::movs"),
        ("runner", "r86_run::run_steps_no_trace_no_perf"),
        ("step_wrapper", "r86_run::step_guest_once_no_trace_no_perf"),
    ),
    "tiny386": (
        ("interpreter", "cpu_exec1"),
        ("step_wrapper", "cpui386_step"),
        ("soft_tlb", "translate_lpgno"),
        ("linear_translate", "translate_laddr"),
        ("segmented_translate", "translate"),
        ("segmented_read_translate", "translate.constprop.0"),
        ("segmented_write_translate", "translate.constprop.1"),
        ("effective_addr", "modsib"),
        ("effective_addr_read32", "__GE_helper32"),
        ("lazy_carry", "get_CF.part.0"),
        ("lazy_overflow", "get_OF"),
        ("flags_materialize", "refresh_flags"),
        ("load16", "load16"),
        ("load32", "load32"),
        ("store16", "store16"),
        ("store32", "store32"),
        ("fetch8", "fetch8"),
        ("fetch16", "fetch16"),
        ("fetch32", "fetch32"),
        ("machine_step", "pc_step"),
    ),
}

ENGINE_FUNCTIONS = {
    "r86": "batch_runner",
    "tiny386": "interpreter",
}

ENGINE_REGION_NAMES = (
    "loop entry through ordinary dispatch",
    "successful completion fast path",
)

HOT_ENGINE_LIMIT = 256 * 1024
FIRST_PARTY_LIMIT = 1024 * 1024
EXPECTED_INLINED_R86_LABELS = {
    "address_decode",
    "address_decode_fast",
    "interpreter",
    "cached_read32",
    "lazy_flag",
    "linear_write8",
    "linear_write16",
    "linear_write32",
    "runner",
    "step_wrapper",
}


def run(args: list[str], *, stdout=None) -> str:
    result = subprocess.run(
        args,
        check=True,
        text=True,
        stdout=stdout if stdout is not None else subprocess.PIPE,
    )
    return "" if stdout is not None else result.stdout


def parse_symbols(binary: pathlib.Path) -> list[Symbol]:
    output = run(["nm", "-S", "-n", "-C", "--defined-only", str(binary)])
    symbols: list[Symbol] = []
    pattern = re.compile(
        r"^(?P<address>[0-9a-fA-F]+)\s+(?P<size>[0-9a-fA-F]+)\s+"
        r"(?P<kind>\S)\s+(?P<name>.*)$"
    )
    for line in output.splitlines():
        match = pattern.match(line)
        if not match:
            continue
        symbols.append(
            Symbol(
                address=int(match.group("address"), 16),
                size=int(match.group("size"), 16),
                kind=match.group("kind"),
                name=match.group("name"),
            )
        )
    return symbols


def text_size(binary: pathlib.Path) -> int:
    output = run(["size", "-A", str(binary)])
    for line in output.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[0] == ".text":
            return int(fields[1])
    raise RuntimeError(f"no .text section found in {binary}")


def text_section(binary: pathlib.Path) -> tuple[int, int]:
    output = run(["size", "-A", str(binary)])
    for line in output.splitlines():
        fields = line.split()
        if len(fields) >= 3 and fields[0] == ".text":
            return int(fields[2]), int(fields[1])
    raise RuntimeError(f"no .text section found in {binary}")


def sha256(binary: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with binary.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_r86_name(name: str) -> str:
    """Compare the single-package build with the saved pre-refactor binaries."""
    return (
        name.replace("r86::x86::", "x86_core::")
        .replace("r86::devices::", "pc_devices::")
        .replace("r86::machine::", "r86_run::")
        .replace("r86::", "r86_run::")
    )


def namespace_text_size(symbols: Iterable[Symbol], namespace: str) -> int:
    ranges = sorted(
        (symbol.address, symbol.address + symbol.size)
        for symbol in symbols
        if symbol.size
        and symbol.kind.lower() == "t"
        and namespace in canonical_r86_name(symbol.name)
    )
    if not ranges:
        return 0
    total = 0
    start, end = ranges[0]
    for next_start, next_end in ranges[1:]:
        if next_start <= end:
            end = max(end, next_end)
        else:
            total += end - start
            start, end = next_start, next_end
    return total + end - start


R86_TEXT_CATEGORIES = (
    "x86-core",
    "r86-run",
    "pc-devices",
    "compression",
    "backtrace/demangling",
    "Rust std/core/alloc",
    "runtime/other symbols",
    "padding/unattributed",
)

R86_FIRST_PARTY_CATEGORIES = (
    "x86-core",
    "r86-run",
    "pc-devices",
)

TINY_TEXT_CATEGORIES = (
    "CPU interpreter",
    "FPU/SIMD",
    "machine/runner",
    "VGA",
    "storage/PCI",
    "IRQ/timer/input",
    "audio/network/misc",
    "frontend/config",
    "runtime/other symbols",
    "padding/unattributed",
)

R86_CORE_NAMESPACES = (
    ("interp", "x86_core::interp::"),
    ("mmu", "x86_core::mmu::"),
    ("mem", "x86_core::mem::"),
    ("cpu", "x86_core::cpu::"),
    ("addr", "x86_core::addr::"),
)


def r86_text_category(name: str) -> str:
    name = canonical_r86_name(name)
    if "x86_core::" in name:
        return "x86-core"
    if "r86_run::" in name:
        return "r86-run"
    if "pc_devices::" in name:
        return "pc-devices"
    if any(
        marker in name
        for marker in (
            "adler2::",
            "crc32fast::",
            "flate2::",
            "miniz_oxide::",
            "simd_adler32::",
        )
    ):
        return "compression"
    if any(
        marker in name
        for marker in (
            "addr2line::",
            "gimli::",
            "object::",
            "rustc_demangle::",
            "std::backtrace_rs::",
        )
    ):
        return "backtrace/demangling"
    if any(marker in name for marker in ("alloc::", "core::", "std::")):
        return "Rust std/core/alloc"
    return "runtime/other symbols"


def source_files_by_address(
    binary: pathlib.Path, symbols: Iterable[Symbol]
) -> dict[int, str]:
    addresses = sorted(
        {
            symbol.address
            for symbol in symbols
            if symbol.size and symbol.kind.lower() == "t"
        }
    )
    output = run(
        ["addr2line", "-e", str(binary), *(f"0x{address:x}" for address in addresses)]
    )
    source_lines = output.splitlines()
    if len(source_lines) != len(addresses):
        raise RuntimeError(
            "addr2line returned an unexpected number of source records: "
            f"{len(source_lines)} != {len(addresses)}"
        )
    return dict(zip(addresses, source_lines, strict=True))


def tiny_text_category(source_line: str) -> str:
    source_path = source_line.split(":", 1)[0]
    basename = pathlib.Path(source_path).name
    if basename == "i386.c":
        return "CPU interpreter"
    if basename in ("fpu.c", "simd.inc"):
        return "FPU/SIMD"
    if basename in ("pc.c", "main.c"):
        return "machine/runner"
    if basename == "vga.c":
        return "VGA"
    if basename in ("ide.c", "fdc.c", "pci.c"):
        return "storage/PCI"
    if basename in ("i8259.c", "i8254.c", "i8257.c", "i8042.c"):
        return "IRQ/timer/input"
    if basename in (
        "misc.c",
        "adlib.c",
        "fmopl.c",
        "ne2000.c",
        "sb16.c",
        "pcspk.c",
        "win32.c",
    ):
        return "audio/network/misc"
    if basename in ("ini.c", "osd.c", "microui.c"):
        return "frontend/config"
    return "runtime/other symbols"


def categorized_text_sizes(
    symbols: Iterable[Symbol],
    text_start: int,
    text_len: int,
    categories: tuple[str, ...],
    classify: Callable[[Symbol], str],
) -> dict[str, int]:
    text_end = text_start + text_len
    events: dict[int, list[tuple[str, int]]] = {
        text_start: [],
        text_end: [],
    }
    for symbol in symbols:
        if not symbol.size or symbol.kind.lower() != "t":
            continue
        start = max(text_start, symbol.address)
        end = min(text_end, symbol.address + symbol.size)
        if start >= end:
            continue
        category = classify(symbol)
        events.setdefault(start, []).append((category, 1))
        events.setdefault(end, []).append((category, -1))

    totals = {category: 0 for category in categories}
    active = {category: 0 for category in categories[:-1]}
    points = sorted(events)
    cursor = points[0]
    for point in points:
        if point > cursor:
            category = next(
                (name for name in categories[:-1] if active[name]),
                "padding/unattributed",
            )
            totals[category] += point - cursor
        for category, delta in events[point]:
            active[category] += delta
        cursor = point
    return totals


def categorized_r86_text_sizes(
    symbols: Iterable[Symbol], text_start: int, text_len: int
) -> dict[str, int]:
    return categorized_text_sizes(
        symbols,
        text_start,
        text_len,
        R86_TEXT_CATEGORIES,
        lambda symbol: r86_text_category(symbol.name),
    )


def find_symbol(symbols: Iterable[Symbol], needle: str) -> Symbol | None:
    candidates = [symbol for symbol in symbols if symbol.size]
    exact = [symbol for symbol in candidates if canonical_r86_name(symbol.name) == needle]
    if exact:
        return max(exact, key=lambda symbol: symbol.size)
    return None


def disassemble(binary: pathlib.Path, symbol: Symbol) -> str:
    return run(
        [
            "objdump",
            "-d",
            "-Mintel",
            "--no-show-raw-insn",
            f"--start-address={symbol.address}",
            f"--stop-address={symbol.address + symbol.size}",
            str(binary),
        ]
    )


@dataclasses.dataclass(frozen=True)
class Instruction:
    address: int
    mnemonic: str
    operands: str


def parse_instructions(disassembly: str) -> list[Instruction]:
    pattern = re.compile(
        r"^\s*(?P<address>[0-9a-fA-F]+):\s+"
        r"(?P<mnemonic>[A-Za-z0-9_.]+)(?:\s+(?P<operands>.*))?$"
    )
    result = []
    for line in disassembly.splitlines():
        match = pattern.match(line)
        if match:
            result.append(
                Instruction(
                    address=int(match.group("address"), 16),
                    mnemonic=match.group("mnemonic").lower(),
                    operands=(match.group("operands") or "").strip(),
                )
            )
    return result


def direct_target(instruction: Instruction) -> int | None:
    match = re.match(r"(?:0x)?([0-9a-fA-F]+)(?:\s|$)", instruction.operands)
    return int(match.group(1), 16) if match else None


def detect_engine_regions(
    parent: Symbol, disassembly: str
) -> tuple[tuple[str, int, int], ...]:
    instructions = parse_instructions(disassembly)
    address_index = {
        instruction.address: index for index, instruction in enumerate(instructions)
    }
    target_counts: dict[int, int] = {}
    loop_candidates: list[tuple[int, int, int]] = []

    for index, instruction in enumerate(instructions):
        target = direct_target(instruction)
        if target is None or not instruction.mnemonic.startswith("j"):
            continue
        target_counts[target] = target_counts.get(target, 0) + 1
        if (
            instruction.mnemonic != "jmp"
            and parent.address <= target < parent.address + 0x1000
            and target < instruction.address
        ):
            prior = instructions[max(0, index - 4) : index]
            if any(item.mnemonic in ("add", "sub", "inc", "dec") for item in prior):
                loop_candidates.append((instruction.address - target, index, target))

    loop_candidates = [
        candidate
        for candidate in loop_candidates
        if candidate[2] in address_index
        and any(
            "qword ptr" in item.operands.lower()
            and item.mnemonic in ("add", "inc")
            and (item.mnemonic == "inc" or item.operands.endswith(",0x1"))
            for item in instructions[
                address_index[candidate[2]] : address_index[candidate[2]] + 40
            ]
        )
    ]
    if not loop_candidates:
        raise RuntimeError(f"cannot locate loop-back edge in {parent.name}")
    _, loop_branch_index, loop_start = max(loop_candidates)

    dispatch_end = None
    loop_start_index = address_index[loop_start]
    for instruction in instructions[loop_start_index : loop_start_index + 128]:
        if instruction.mnemonic == "jmp" and direct_target(instruction) is None:
            next_index = address_index[instruction.address] + 1
            dispatch_end = (
                instructions[next_index].address
                if next_index < len(instructions)
                else parent.address + parent.size
            )
            break
    if dispatch_end is None:
        raise RuntimeError(f"cannot locate ordinary dispatch in {parent.name}")

    loop_branch = instructions[loop_branch_index]
    nearby_targets = [
        target
        for target, count in target_counts.items()
        if count > 1 and loop_branch.address - 0x200 <= target <= loop_branch.address
    ]
    if not nearby_targets:
        raise RuntimeError(f"cannot locate completion convergence in {parent.name}")
    completion_start = max(
        nearby_targets,
        key=lambda target: (target_counts[target], target),
    )
    completion_end = (
        instructions[loop_branch_index + 1].address
        if loop_branch_index + 1 < len(instructions)
        else parent.address + parent.size
    )
    return (
        (ENGINE_REGION_NAMES[0], loop_start, dispatch_end),
        (ENGINE_REGION_NAMES[1], completion_start, completion_end),
    )


def is_indirect_operand(operand: str) -> bool:
    operand = operand.strip()
    if not operand:
        return False
    target = operand.split("#", 1)[0].strip()
    return not bool(re.match(r"^(?:0x)?[0-9a-fA-F]+(?:\s|$)", target))


def metrics(disassembly: str) -> FunctionMetrics:
    instruction_re = re.compile(
        r"^\s*[0-9a-fA-F]+:\s+(?P<mnemonic>[A-Za-z0-9_.]+)(?:\s+(?P<operands>.*))?$"
    )
    instructions = 0
    branches = 0
    conditional_branches = 0
    calls = 0
    returns = 0
    indirect_transfers = 0
    memory_operands = 0
    probable_loads = 0
    probable_stores = 0

    for line in disassembly.splitlines():
        match = instruction_re.match(line)
        if not match:
            continue
        instructions += 1
        mnemonic = match.group("mnemonic").lower()
        operands = (match.group("operands") or "").strip()

        is_call = mnemonic.startswith("call")
        is_return = mnemonic.startswith("ret")
        is_jump = mnemonic.startswith("j") or mnemonic.startswith("loop")
        if is_call or is_return or is_jump:
            branches += 1
        if is_jump and not mnemonic.startswith("jmp"):
            conditional_branches += 1
        if is_call:
            calls += 1
        if is_return:
            returns += 1
        if (is_call or mnemonic.startswith("jmp")) and is_indirect_operand(operands):
            indirect_transfers += 1

        if "[" not in operands:
            continue
        memory_operands += 1
        first_operand = operands.split(",", 1)[0]
        if "[" in first_operand:
            probable_stores += 1
        if "," not in operands or "[" in operands.split(",", 1)[1]:
            probable_loads += 1

    return FunctionMetrics(
        instructions=instructions,
        branches=branches,
        conditional_branches=conditional_branches,
        calls=calls,
        returns=returns,
        indirect_transfers=indirect_transfers,
        memory_operands=memory_operands,
        probable_loads=probable_loads,
        probable_stores=probable_stores,
    )


def write_full_objdump(binary: pathlib.Path, output: pathlib.Path) -> None:
    with output.open("w", encoding="utf-8") as stream:
        run(["objdump", "-Cd", "-Mintel", str(binary)], stdout=stream)


def format_bytes(value: int) -> str:
    if value >= 1024 * 1024:
        return f"{value / (1024 * 1024):.2f} MiB"
    if value >= 1024:
        return f"{value / 1024:.1f} KiB"
    return f"{value} B"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r86", type=pathlib.Path, default="target/release/r86-run")
    parser.add_argument(
        "--tiny386",
        type=pathlib.Path,
        default="refs/tiny386/tiny386_headless",
    )
    parser.add_argument(
        "--out-dir",
        type=pathlib.Path,
        default="tmp/perf/softmmu-objdump",
    )
    parser.add_argument("--skip-full-objdump", action="store_true")
    parser.add_argument(
        "--check",
        action="store_true",
        help="return nonzero when a code-size gate fails",
    )
    args = parser.parse_args()

    binaries = {"r86": args.r86.resolve(), "tiny386": args.tiny386.resolve()}
    for binary in binaries.values():
        if not binary.is_file():
            parser.error(f"binary not found: {binary}")

    args.out_dir.mkdir(parents=True, exist_ok=True)
    symbols_by_binary = {
        name: parse_symbols(binary) for name, binary in binaries.items()
    }
    text_sections = {name: text_section(binary) for name, binary in binaries.items()}
    text_sizes = {name: section[1] for name, section in text_sections.items()}
    binary_hashes = {name: sha256(binary) for name, binary in binaries.items()}
    r86_execution_core_size = namespace_text_size(
        symbols_by_binary["r86"], "x86_core::"
    )
    r86_text_categories = categorized_r86_text_sizes(
        symbols_by_binary["r86"], *text_sections["r86"]
    )
    tiny_source_files = source_files_by_address(
        binaries["tiny386"], symbols_by_binary["tiny386"]
    )
    tiny_text_categories = categorized_text_sizes(
        symbols_by_binary["tiny386"],
        *text_sections["tiny386"],
        TINY_TEXT_CATEGORIES,
        lambda symbol: tiny_text_category(tiny_source_files[symbol.address]),
    )
    r86_core_module_sizes = {
        label: namespace_text_size(symbols_by_binary["r86"], namespace)
        for label, namespace in R86_CORE_NAMESPACES
    }
    r86_first_party_size = sum(
        r86_text_categories[name] for name in R86_FIRST_PARTY_CATEGORIES
    )
    categorized_total = sum(r86_text_categories.values())
    if categorized_total != text_sizes["r86"]:
        raise RuntimeError(
            "r86 text attribution does not cover .text exactly: "
            f"{categorized_total} != {text_sizes['r86']}"
        )
    tiny_categorized_total = sum(tiny_text_categories.values())
    if tiny_categorized_total != text_sizes["tiny386"]:
        raise RuntimeError(
            "tiny386 text attribution does not cover .text exactly: "
            f"{tiny_categorized_total} != {text_sizes['tiny386']}"
        )
    core_module_total = sum(r86_core_module_sizes.values())
    if core_module_total > r86_execution_core_size:
        raise RuntimeError(
            "x86-core module ranges overlap or exceed the namespace total: "
            f"{core_module_total} > {r86_execution_core_size}"
        )
    r86_core_module_sizes["other"] = r86_execution_core_size - core_module_total

    r86_size_layers = {
        "CPU/FPU/MMU core": r86_text_categories["x86-core"],
        "machine/runner/devices/features": sum(
            r86_text_categories[name]
            for name in ("r86-run", "pc-devices", "compression")
        ),
        "linked language/runtime": sum(
            r86_text_categories[name]
            for name in (
                "backtrace/demangling",
                "Rust std/core/alloc",
                "runtime/other symbols",
            )
        ),
        "padding/unattributed": r86_text_categories["padding/unattributed"],
    }
    tiny_size_layers = {
        "CPU/FPU/MMU core": sum(
            tiny_text_categories[name] for name in ("CPU interpreter", "FPU/SIMD")
        ),
        "machine/runner/devices/features": sum(
            tiny_text_categories[name]
            for name in (
                "machine/runner",
                "VGA",
                "storage/PCI",
                "IRQ/timer/input",
                "audio/network/misc",
                "frontend/config",
            )
        ),
        "linked language/runtime": tiny_text_categories["runtime/other symbols"],
        "padding/unattributed": tiny_text_categories["padding/unattributed"],
    }
    if sum(r86_size_layers.values()) != text_sizes["r86"]:
        raise RuntimeError("r86 comparable size layers do not cover .text")
    if sum(tiny_size_layers.values()) != text_sizes["tiny386"]:
        raise RuntimeError("tiny386 comparable size layers do not cover .text")

    if not args.skip_full_objdump:
        for name, binary in binaries.items():
            write_full_objdump(binary, args.out_dir / f"{name}.objdump.txt")

    rows: list[tuple[str, str, Symbol, FunctionMetrics]] = []
    function_symbols: dict[tuple[str, str], Symbol] = {}
    missing: list[tuple[str, str, str]] = []
    for binary_name, functions in FUNCTIONS.items():
        binary = binaries[binary_name]
        symbols = symbols_by_binary[binary_name]
        for label, needle in functions:
            symbol = find_symbol(symbols, needle)
            if symbol is None:
                missing.append((binary_name, label, needle))
                continue
            function_symbols[(binary_name, label)] = symbol
            assembly = disassemble(binary, symbol)
            (args.out_dir / f"{binary_name}-{label}.asm").write_text(
                assembly, encoding="utf-8"
            )
            rows.append((binary_name, label, symbol, metrics(assembly)))

    region_rows: list[tuple[str, str, int, int, FunctionMetrics]] = []
    for binary_name, function_label in ENGINE_FUNCTIONS.items():
        binary = binaries[binary_name]
        parent = function_symbols.get((binary_name, function_label))
        if parent is None:
            continue
        parent_assembly = disassemble(binary, parent)
        for region_name, start, end in detect_engine_regions(parent, parent_assembly):
            assembly = disassemble(
                binary,
                Symbol(start, end - start, "t", f"{parent.name}+{start - parent.address:#x}"),
            )
            artifact_label = region_name.replace(" ", "-")
            (args.out_dir / f"{binary_name}-{artifact_label}.asm").write_text(
                assembly, encoding="utf-8"
            )
            region_rows.append(
                (binary_name, region_name, start, end, metrics(assembly))
            )

    report = args.out_dir / "report.md"
    with report.open("w", encoding="utf-8") as stream:
        stream.write("# SoftMMU Objdump Alignment Report\n\n")
        stream.write(
            "Generated from linked release binaries. Counts are static and "
            "approximate.\n\n"
        )
        stream.write("## Binary Identity\n\n")
        stream.write("| Binary | Path | SHA-256 |\n|---|---|---|\n")
        for name in ("r86", "tiny386"):
            stream.write(
                f"| {name} | `{binaries[name]}` | `{binary_hashes[name]}` |\n"
            )
        stream.write("\n")
        stream.write("## Text Size\n\n")
        stream.write("| Binary | `.text` bytes | Human |\n")
        stream.write("|---|---:|---:|\n")
        for name in ("r86", "tiny386"):
            stream.write(
                f"| {name} | {text_sizes[name]} | {format_bytes(text_sizes[name])} |\n"
            )
        ratio = text_sizes["r86"] / text_sizes["tiny386"]
        stream.write(f"\nOverall `.text` ratio, r86/tiny386: **{ratio:.2f}x**.\n\n")

        stream.write("## r86 Text Attribution\n\n")
        stream.write("| Symbol attribution | Bytes | Human | Share |\n")
        stream.write("|---|---:|---:|---:|\n")
        for category in R86_TEXT_CATEGORIES:
            size = r86_text_categories[category]
            stream.write(
                f"| {category} | {size} | {format_bytes(size)} | "
                f"{size / text_sizes['r86']:.1%} |\n"
            )
        stream.write(
            "\nAttribution uses unique linked address intervals and demangled symbol names. "
            "Generic monomorphizations are attributed from the name that remains in the "
            "linked binary; this is a size diagnosis, not a linker provenance map.\n\n"
        )

        stream.write("### x86-core Breakdown\n\n")
        stream.write("| Module namespace | Bytes | Human | Share of x86-core |\n")
        stream.write("|---|---:|---:|---:|\n")
        for label in (*[item[0] for item in R86_CORE_NAMESPACES], "other"):
            size = r86_core_module_sizes[label]
            if label == "other" and size == 0:
                continue
            stream.write(
                f"| `{label}` | {size} | {format_bytes(size)} | "
                f"{size / r86_execution_core_size:.1%} |\n"
            )
        stream.write(
            "\nThese module rows are also unions of unique linked symbol ranges; "
            "they sum to the x86-core total above.\n\n"
        )

        stream.write("## tiny386 Text Attribution\n\n")
        stream.write("| Source attribution | Bytes | Human | Share |\n")
        stream.write("|---|---:|---:|---:|\n")
        for category in TINY_TEXT_CATEGORIES:
            size = tiny_text_categories[category]
            stream.write(
                f"| {category} | {size} | {format_bytes(size)} | "
                f"{size / text_sizes['tiny386']:.1%} |\n"
            )
        stream.write(
            "\nThe tiny386 attribution maps each linked text symbol back to its "
            "DWARF source file with `addr2line`, then unions address intervals. "
            "Its libc implementation is dynamically linked and is therefore not "
            "part of this `.text` section.\n\n"
        )

        text_delta = text_sizes["r86"] - text_sizes["tiny386"]
        stream.write("## Whole-Text Delta by Comparable Layer\n\n")
        stream.write(
            "| Comparable layer | r86 bytes | tiny386 bytes | Delta | "
            "Share of total delta |\n"
        )
        stream.write("|---|---:|---:|---:|---:|\n")
        for layer in r86_size_layers:
            r86_size = r86_size_layers[layer]
            tiny_size = tiny_size_layers[layer]
            delta = r86_size - tiny_size
            stream.write(
                f"| {layer} | {r86_size} | {tiny_size} | {delta:+d} | "
                f"{delta / text_delta:.1%} |\n"
            )
        stream.write(
            f"\nThe complete `.text` difference is **{text_delta} bytes "
            f"({format_bytes(text_delta)})**. Nearly half comes from linked "
            "language/runtime text, and about four-fifths comes from runtime plus "
            "non-core machine/runner/features. The hot instruction-engine size is "
            "not the main source of the whole-binary difference.\n\n"
        )

        stream.write("## Selected Functions\n\n")
        stream.write(
            "| Binary | Path | Bytes | Instructions | Branches | Cond. branches | "
            "Calls | Indirect transfers | Memory operands | Loads | Stores |\n"
        )
        stream.write("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n")
        for binary_name, label, symbol, item in rows:
            stream.write(
                f"| {binary_name} | {label} | {symbol.size} | {item.instructions} | "
                f"{item.branches} | {item.conditional_branches} | {item.calls} | "
                f"{item.indirect_transfers} | {item.memory_operands} | "
                f"{item.probable_loads} | {item.probable_stores} |\n"
            )

        r86_interpreter = next(
            (row for row in rows if row[0] == "r86" and row[1] == "interpreter"),
            None,
        )
        r86_batch = next(
            (row for row in rows if row[0] == "r86" and row[1] == "batch_runner"),
            None,
        )
        # The opcode body lives in run_batch_inner, shared by step/run_batch.
        # Count its full linked range, including the cold opcode arms.
        r86_engine = r86_batch or r86_interpreter
        tiny_interpreter = next(
            (row for row in rows if row[0] == "tiny386" and row[1] == "interpreter"),
            None,
        )
        if r86_engine and tiny_interpreter:
            byte_ratio = r86_engine[2].size / tiny_interpreter[2].size
            instruction_ratio = (
                r86_engine[3].instructions
                / tiny_interpreter[3].instructions
            )
            stream.write(
                "\nInterpreter ratios, r86/tiny386: "
                f"**{byte_ratio:.2f}x bytes**, **{instruction_ratio:.2f}x static instructions**.\n"
            )

        region_lookup = {
            (binary_name, region_name): (start, end, item)
            for binary_name, region_name, start, end, item in region_rows
        }
        stream.write("\n## Common Loop, One-to-One\n\n")
        stream.write(
            "| Generated-code region | r86 linked half-open range | r86 instructions | "
            "tiny386 linked half-open range | tiny386 instructions |\n"
        )
        stream.write("|---|---|---:|---|---:|\n")
        for region_name in ENGINE_REGION_NAMES:
            r86_region = region_lookup.get(("r86", region_name))
            tiny_region = region_lookup.get(("tiny386", region_name))
            if not r86_region or not tiny_region:
                continue
            r86_start, r86_end, r86_item = r86_region
            tiny_start, tiny_end, tiny_item = tiny_region
            stream.write(
                f"| {region_name} | `[0x{r86_start:x}, 0x{r86_end:x})` | "
                f"{r86_item.instructions} | `[0x{tiny_start:x}, 0x{tiny_end:x})` | "
                f"{tiny_item.instructions} |\n"
            )
        r86_region_total = sum(
            item.instructions
            for binary_name, _, _, _, item in region_rows
            if binary_name == "r86"
        )
        tiny_region_total = sum(
            item.instructions
            for binary_name, _, _, _, item in region_rows
            if binary_name == "tiny386"
        )
        stream.write(
            "\nOn the ordinary fast path, the selected common envelope is "
            f"**{r86_region_total} instructions for r86 versus "
            f"{tiny_region_total} for tiny386**. The corresponding `.asm` files "
            "are emitted beside this report. These are selected executed paths, not "
            "whole-function averages.\n"
        )

        engine_size = r86_engine[2].size if r86_engine else 0
        engine_pass = bool(r86_engine) and engine_size <= HOT_ENGINE_LIMIT
        first_party_pass = r86_first_party_size <= FIRST_PARTY_LIMIT
        stream.write("\n## Code-Size Gates\n\n")
        stream.write("| Gate | Observed | Limit | Result |\n")
        stream.write("|---|---:|---:|---|\n")
        stream.write(
            f"| linked hot instruction engine | {engine_size} B "
            f"({format_bytes(engine_size)}) | <= {HOT_ENGINE_LIMIT} B "
            f"(256 KiB) | **{'PASS' if engine_pass else 'FAIL'}** |\n"
        )
        stream.write(
            f"| linked first-party r86 function text | {r86_first_party_size} B "
            f"({format_bytes(r86_first_party_size)}) | <= {FIRST_PARTY_LIMIT} B "
            f"(1 MiB) | **{'PASS' if first_party_pass else 'FAIL'}** |\n"
        )
        stream.write(
            "\nFirst-party text is the union attributed to the x86 core, "
            "machine/frontend, and device modules. The report retains the old "
            "`x86-core`, `r86-run`, and `pc-devices` category labels for comparison "
            "with saved workspace binaries; `r86::*` symbols and generic "
            "instances using them are included. Linked Rust/compiler runtime, compression, other "
            "third-party code, and padding are reported above but excluded from "
            "the 1 MiB gate. The limits are ceilings, not size budgets.\n"
        )

        stream.write("\n## Side-by-Side Alignment\n\n")
        stream.write(
            "| Path | r86 bytes | tiny386 bytes | Byte ratio | r86 instructions | "
            "tiny386 instructions | Instruction ratio |\n"
        )
        stream.write("|---|---:|---:|---:|---:|---:|---:|\n")
        comparisons = [("instruction engine", r86_engine, tiny_interpreter)]
        if r86_interpreter and r86_batch:
            tiny_wrapper = next(
                (
                    row
                    for row in rows
                    if row[0] == "tiny386" and row[1] == "step_wrapper"
                ),
                None,
            )
            comparisons.insert(0, ("dispatch boundary", r86_batch, tiny_wrapper))
        selected_rows = {(row[0], row[1]): row for row in rows}
        comparisons.extend(
            (
                label,
                selected_rows.get(("r86", r86_label)),
                selected_rows.get(("tiny386", tiny_label)),
            )
            for label, r86_label, tiny_label in (
                ("byte fetch hit", "fetch_slow", "fetch8"),
                ("32-bit cached data-read leaf", "linear_read32", "load32"),
                ("32-bit data write", "linear_write32", "store32"),
                ("32-bit address decoder", "address_decode_fast", "effective_addr"),
                ("32-bit memory-read wrapper", "read_mem32", "effective_addr_read32"),
            )
        )
        for path, r86_row, tiny_row in comparisons:
            if not r86_row or not tiny_row:
                continue
            stream.write(
                f"| {path} | {r86_row[2].size} | {tiny_row[2].size} | "
                f"{r86_row[2].size / tiny_row[2].size:.2f}x | "
                f"{r86_row[3].instructions} | {tiny_row[3].instructions} | "
                f"{r86_row[3].instructions / tiny_row[3].instructions:.2f}x |\n"
            )
        stream.write(
            "\nThe helper rows compare generated shapes, not semantic equivalence. "
            "In particular, r86 keeps x86 fault, privilege, A/D, and tracked-write "
            "work that tiny386 may perform in a different helper or omit.\n"
        )

        if r86_engine and tiny_interpreter:
            stream.write("\n## Instruction Engine Diff\n\n")
            stream.write("| Metric | r86 | tiny386 | Delta | Ratio |\n")
            stream.write("|---|---:|---:|---:|---:|\n")
            engine_metrics = (
                ("bytes", r86_engine[2].size, tiny_interpreter[2].size),
                ("instructions", r86_engine[3].instructions, tiny_interpreter[3].instructions),
                ("branches", r86_engine[3].branches, tiny_interpreter[3].branches),
                (
                    "conditional branches",
                    r86_engine[3].conditional_branches,
                    tiny_interpreter[3].conditional_branches,
                ),
                ("calls", r86_engine[3].calls, tiny_interpreter[3].calls),
                (
                    "indirect transfers",
                    r86_engine[3].indirect_transfers,
                    tiny_interpreter[3].indirect_transfers,
                ),
                (
                    "memory operands",
                    r86_engine[3].memory_operands,
                    tiny_interpreter[3].memory_operands,
                ),
                (
                    "probable loads",
                    r86_engine[3].probable_loads,
                    tiny_interpreter[3].probable_loads,
                ),
                (
                    "probable stores",
                    r86_engine[3].probable_stores,
                    tiny_interpreter[3].probable_stores,
                ),
            )
            for metric_name, r86_value, tiny_value in engine_metrics:
                stream.write(
                    f"| {metric_name} | {r86_value} | {tiny_value} | "
                    f"{r86_value - tiny_value:+d} | {r86_value / tiny_value:.2f}x |\n"
                )
            stream.write(
                "\nThis is the generated-code alignment check: both linked binaries are "
                "disassembled in the same run, then their engine size, control transfers, "
                "and approximate memory traffic are compared. Runtime gates remain "
                "authoritative because static counts do not encode branch frequency or "
                "cache behavior.\n"
            )

        if r86_engine is r86_batch and r86_engine is not None:
            stream.write(
                "\nThe instruction body lives in `Interpreter::run_batch_inner`; "
                "that symbol, including its cold opcode arms, is "
                "classified as the instruction engine above.\n"
            )

        stream.write(
            "\nThe MMU rows are intentionally not collapsed into one ratio: r86's "
            "`linear_read*` and `linear_write*` retain segmentation-independent x86 "
            "fault and tracked-write semantics, while tiny386 splits translation from "
            "its physical `load*`/`store*` helpers. Compare those rows as a pipeline.\n"
        )

        reportable_missing = [
            item
            for item in missing
            if not (item[0] == "r86" and item[1] in EXPECTED_INLINED_R86_LABELS)
        ]
        expected_inlined = [
            item
            for item in missing
            if item[0] == "r86" and item[1] in EXPECTED_INLINED_R86_LABELS
        ]
        if expected_inlined:
            labels = ", ".join(f"`{label}`" for _, label, _ in expected_inlined)
            stream.write(
                "\nExpected standalone symbols absent after inlining/fusion: "
                f"{labels}. Exact-name lookup is deliberate: the report does not "
                "substitute a similarly named cold helper. The hot-engine gate counts "
                "code in `run_batch_inner`; the namespace gate counts unique linked "
                "ranges still attributed to `x86_core::*`.\n"
            )
        if reportable_missing:
            stream.write("\n## Missing Symbols\n\n")
            for binary_name, label, needle in reportable_missing:
                stream.write(f"- `{binary_name}:{label}` matching `{needle}`\n")

        stream.write("\n## Largest Text Symbols\n\n")
        for name in ("r86", "tiny386"):
            stream.write(f"### {name}\n\n")
            stream.write("| Bytes | Symbol |\n|---:|---|\n")
            text_symbols = [
                symbol
                for symbol in symbols_by_binary[name]
                if symbol.kind.lower() == "t" and symbol.size
            ]
            for symbol in sorted(text_symbols, key=lambda item: item.size, reverse=True)[:15]:
                stream.write(f"| {symbol.size} | `{symbol.name}` |\n")
            stream.write("\n")

    print(report)
    if args.check and (
        not engine_pass
        or not first_party_pass
        or reportable_missing
    ):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
