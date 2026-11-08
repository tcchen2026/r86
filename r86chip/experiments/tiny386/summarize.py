#!/usr/bin/env python3
"""Compare measured RV32IM instruction counts; never infer FPGA cycles/MIPS.

The loop comparison requires identical guest results at both endpoints. The
full-system comparison instead requires the same input RAM and both assertion
markers: different CPU/device implementations need not execute the same trace.
All native retirement, including firmware startup and services, counts in the
full-system ratio. Only the synthetic loops subtract their common startup.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def read(path):
    return json.loads(path.read_text())


def require_software(sample):
    for field in ('chip_extensions', 'bank_enters', 'jit_native_steps',
                  'jit_invocations', 'jit_compiled_blocks', 'xalu_ops',
                  'tlb_lookups', 'tlb_fills', 'xld_ops', 'xea_ops'):
        assert sample[field] == 0, (field, sample[field])
    assert sample['x86_completed'] > 0
    assert 'Breakpoint' in sample['trap'], sample


def loops(r86_dir, tiny_dir):
    reports = [read(p / 'report.json') for p in (r86_dir, tiny_dir)]
    assert reports[0]['iss_sha256'] == reports[1]['iss_sha256']
    assert reports[0]['cases'].keys() == reports[1]['cases'].keys()
    result = {'cases': {}, 'iss_sha256': reports[0]['iss_sha256'],
              'firmware_sha256': dict(zip(('r86', 'tiny386'),
                                         (r['firmware_sha256'] for r in reports)))}
    native_totals, guest_totals = [0, 0], [0, 0]
    for name in reports[0]['cases']:
        samples = [r['cases'][name]['samples'] for r in reports]
        assert all(len(pair) == 2 for pair in samples)
        for index in range(2):
            for pair in samples:
                sample = pair[index]
                require_software(sample)
                assert sample['status'] == 4
                assert sample['x86_steps'] == (10000, 43000)[index]
                assert sample['x86_completed'] == sample['x86_steps']
            for field in ('x86_steps', 'x86_completed', 'eip', 'eflags',
                          'x86_regs', 'guest_ram_fnv64', 'x86_fault_attempts',
                          'x86_repeat_chunks', 'x86_halted_polls'):
                assert samples[0][index][field] == samples[1][index][field], (name, field)
        row = {'guest_sha256': sha256(r86_dir / name / 'guest.bin'),
               'guest_state_equal_at_both_endpoints': True}
        assert row['guest_sha256'] == sha256(tiny_dir / name / 'guest.bin')
        for index, engine in enumerate(('r86', 'tiny386')):
            before, after = samples[index]
            native = after['retired_native'] - before['retired_native']
            guest = after['x86_completed'] - before['x86_completed']
            row[engine] = {'retired_native': native, 'completed_x86': guest,
                           'native_per_x86': native / guest}
            native_totals[index] += native
            guest_totals[index] += guest
        row['r86_native_reduction_fraction'] = 1 - row['r86']['native_per_x86'] / row['tiny386']['native_per_x86']
        result['cases'][name] = row
    result['equal_loop_mix'] = {
        engine: {'retired_native': native, 'completed_x86': guest,
                 'native_per_x86': native / guest}
        for engine, native, guest in zip(('r86', 'tiny386'), native_totals, guest_totals)}
    result['scope'] = ('Synthetic paging-on loops, each 32 selected operations plus a jump. '
                       'Subtract 10,000 from 43,000 completed x86 instructions. '
                       'Tiny386 uses its CPU-only driver; r86 retains PC service overhead. '
                       'The equal loop mix is not an OS workload average.')
    return result


def full(r86_dir, tiny_dir):
    configs = [struct.unpack('<8I', (p / 'config.bin').read_bytes())
               for p in (r86_dir, tiny_dir)]
    assert configs[0][:5] == configs[1][:5]
    assert configs[0][7] == configs[1][7] == 1  # Lua, no JIT/boundary tracing.
    ram_hashes = [sha256(p / 'guest.ram') for p in (r86_dir, tiny_dir)]
    assert ram_hashes[0] == ram_hashes[1]
    result = {'input_ram_sha256': ram_hashes[0], 'engines': {}}
    for engine, directory in zip(('r86', 'tiny386'), (r86_dir, tiny_dir)):
        sample = read(directory / 'iss-run.json')
        require_software(sample)
        assert sample['status'] == 3 and sample['boot_marker'] and sample['lua_marker'], sample
        serial = (directory / 'iss-serial.log').read_bytes()
        assert b'R86_CHIP_TINYCORE_OK' in serial and b'R86_CHIP_LUA_OK' in serial
        counts = {key: sample[key] for key in (
            'retired_native', 'x86_steps', 'x86_completed', 'x86_fault_attempts',
            'x86_repeat_chunks', 'x86_halted_polls', 'status', 'boot_marker', 'lua_marker')}
        counts['native_per_x86'] = sample['retired_native'] / sample['x86_completed']
        counts['serial_sha256'] = hashlib.sha256(serial).hexdigest()
        result['engines'][engine] = counts
    r86, tiny = (result['engines'][name] for name in ('r86', 'tiny386'))
    result['r86_native_per_x86_reduction_fraction'] = 1 - r86['native_per_x86'] / tiny['native_per_x86']
    result['scope'] = ('Same direct-kernel RAM, TinyCore boot and Lua assertion endpoint; '
                       'startup, decoding, software flags/TLBs, faults and PC services included. '
                       'Same 19,090,912 Hz deterministic instruction clock; different CPU models, '
                       'device implementations and batch sizes can change the guest trace. '
                       'REP counts once on completion, not once per element. '
                       'Marker is observed at a firmware batch boundary.')
    return result


def acceleration(software_dir, accelerated_dir):
    """Compare the same r86 source with only its leaf primitives selected."""
    assert (software_dir / 'config.bin').read_bytes() == (accelerated_dir / 'config.bin').read_bytes()
    assert sha256(software_dir / 'guest.ram') == sha256(accelerated_dir / 'guest.ram')
    before, after = (read(p / 'iss-run.json') for p in (software_dir, accelerated_dir))
    require_software(before)
    assert before['status'] == after['status'] == 3
    assert after['boot_marker'] and after['lua_marker'] and after['chip_extensions'] == 1
    assert after['jit_native_steps'] == after['bank_enters'] == 0
    assert 'Breakpoint' in after['trap']
    fields = ('x86_steps', 'x86_completed', 'eip', 'eflags', 'x86_regs',
              'tsc', 'guest_ram_fnv64', 'x86_fault_attempts',
              'x86_repeat_chunks', 'x86_halted_polls')
    for field in fields:
        assert before[field] == after[field], field
    assert (software_dir / 'iss-serial.log').read_bytes() == (accelerated_dir / 'iss-serial.log').read_bytes()
    return {
        'scope': 'Same r86 source, input RAM, config, completed guest count, final state and serial output. All native startup/services included.',
        'guest_state_equal': True,
        'software_retired_native': before['retired_native'],
        'accelerated_retired_native': after['retired_native'],
        'completed_x86': after['x86_completed'],
        'software_native_per_x86': before['retired_native'] / before['x86_completed'],
        'accelerated_native_per_x86': after['retired_native'] / after['x86_completed'],
        'native_reduction_fraction': 1 - after['retired_native'] / before['retired_native'],
        'remaining_reduction_to_20': 1 - 20 * after['x86_completed'] / after['retired_native'],
        'accelerated_counters': {key: after[key] for key in (
            'xalu_ops', 'tlb_lookups', 'tlb_hits', 'tlb_fills',
            'xld_ops', 'xld_hits', 'xea_ops', 'xea_hits')},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--r86-loops', type=Path, required=True)
    parser.add_argument('--tiny386-loops', type=Path, required=True)
    parser.add_argument('--r86-full', type=Path)
    parser.add_argument('--tiny386-full', type=Path)
    parser.add_argument('--accelerated-full', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if bool(args.r86_full) != bool(args.tiny386_full):
        parser.error('provide both full-system result directories')
    if args.accelerated_full and not args.r86_full:
        parser.error('--accelerated-full requires the full-system comparison')
    report = {'scope': __doc__, 'loops': loops(args.r86_loops, args.tiny386_loops)}
    if args.r86_full:
        report['full'] = full(args.r86_full, args.tiny386_full)
    if args.accelerated_full:
        report['acceleration'] = acceleration(args.r86_full, args.accelerated_full)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
