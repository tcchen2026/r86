#!/usr/bin/env python3
"""One interpreter/JIT calibration pair, with optional native compile profiling."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import selectors
import shlex
import socket
import struct
import subprocess
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
NODE_FLAGS = '--no-liftoff --no-wasm-inlining --wasm-num-compilation-tasks=1 --wasm-max-committed-code-mb=96'
MARKER = re.compile(r'L1 (P[1-4]) (START|END) ([0-9A-F]{16})')


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class LocalSocket:
    """Only the local r86 text protocol: one request and one response at a time."""
    def __init__(self, address, timeout):
        self.socket = socket.create_connection(address, timeout=timeout)
        self.socket.settimeout(timeout)
        self.buffer = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        self.socket.sendall((f'GET /ws HTTP/1.1\r\nHost: {address[0]}:{address[1]}\r\n'
                             'Upgrade: websocket\r\nConnection: Upgrade\r\n'
                             f'Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            self.buffer.extend(self.socket.recv(4096))
        header, tail = bytes(self.buffer).split(b'\r\n\r\n', 1)
        self.buffer = bytearray(tail)
        accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        if not header.startswith(b'HTTP/1.1 101 ') or accept not in header:
            raise RuntimeError('WebSocket handshake rejected')
        self.hello = self.receive()

    def read(self, n):
        while len(self.buffer) < n:
            part = self.socket.recv(max(4096, n-len(self.buffer)))
            if not part:
                raise RuntimeError('r86 connection closed')
            self.buffer.extend(part)
        result = bytes(self.buffer[:n])
        del self.buffer[:n]
        return result

    def receive(self):
        head, length = self.read(2)
        if head != 0x81 or length & 128:
            raise RuntimeError(f'Unexpected local frame: {head:#x}/{length:#x}')
        length = length if length < 126 else struct.unpack('!H' if length == 126 else '!Q', self.read(2 if length == 126 else 8))[0]
        if length > 16*1024*1024:
            raise RuntimeError('Oversized local response')
        return json.loads(self.read(length))

    def request(self, value):
        payload = json.dumps(value, separators=(',', ':')).encode()
        mask = os.urandom(4)
        n = len(payload)
        header = bytes([0x81, 0x80 | n]) if n < 126 else bytes([0x81, 0xfe])+struct.pack('!H', n)
        self.socket.sendall(header+mask+bytes(c ^ mask[i % 4] for i, c in enumerate(payload)))
        result = self.receive()
        if not result.get('ok', True):
            raise RuntimeError(json.dumps(result))
        return result

    def close(self):
        self.socket.close()


def counters(text):
    # Engine reports are Rust Debug strings; preserve the full original too.
    return {name: int(value) for name, value in re.findall(r'\b(\w+): (\d+)(?=[, }])', text)}


def stats_reply(client):
    reply = client.request({'cmd': 'diagnostics'})
    return reply.get('current', {}).get('jit', '')


def command_prefix(args):
    result = ['taskset', '-c', str(args.cpu)] if args.cpu is not None else []
    return result + shlex.split(args.prefix)


def native(args, engine, binary, guest, boot, label, reference=None):
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        address = reserve.getsockname()
    cmd = command_prefix(args) + [str(binary), '--bootsector', str(boot), '--hdd', str(guest),
        '--hdd-frozen', '--no-cdrom', '--no-vga-bios', '--mem-mib', '16', '--clock', 'deterministic',
        '--cpu-engine', engine, '--steps', str(args.steps), '--ws-listen', f'127.0.0.1:{address[1]}',
        '--ws-slice-steps', str(args.steps)]
    record = {'engine': engine, 'command': cmd, 'binary_sha256': sha(binary), 'phases': {}, 'serial': ''}
    client = None
    with (args.output/(label+'.log')).open('wb') as log:
        process = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic()+10
            while True:
                if process.poll() is not None:
                    raise RuntimeError(f'{label} exited before WebSocket startup; see {log.name}')
                try:
                    client = LocalSocket(address, args.timeout)
                    break
                except ConnectionRefusedError:
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(0.02)
            for phase in ['P1', 'P2', 'P3', 'P4']:
                start_request = ({'cmd': 'run-until-step', 'step': reference[phase]['start'], 'slice_steps': args.steps}
                    if reference else {'cmd': 'wait-serial', 'text': f'L1 {phase} START ',
                        'timeout_steps': args.steps, 'slice_steps': 1024})
                start = client.request(start_request)
                record['serial'] += bytes.fromhex(start.get('serial_hex', '')).decode('ascii')
                before_text = stats_reply(client) if engine == 'jit' else ''
                before = counters(before_text)
                if start['step'] >= args.steps:
                    raise RuntimeError('Global guest step limit reached')
                began = time.perf_counter_ns()
                end_request = ({'cmd': 'run-until-step', 'step': reference[phase]['end'], 'slice_steps': args.steps}
                    if reference else {'cmd': 'wait-serial', 'text': f'L1 {phase} END ',
                        'timeout_steps': args.steps-start['step'], 'slice_steps': 1024 if phase in ('P3', 'P4') else 100000})
                end = client.request(end_request)
                elapsed = time.perf_counter_ns()-began
                record['serial'] += bytes.fromhex(end.get('serial_hex', '')).decode('ascii')
                after_text = stats_reply(client) if engine == 'jit' else ''
                after = counters(after_text)
                record['phases'][phase] = {'wall_ns': elapsed, 'observed_start_step': start['step'],
                    'observed_end_step': end['step'], 'jit_before': before_text, 'jit_after': after_text,
                    'jit_delta': {key: after[key]-before.get(key, 0) for key in after}}
                (args.output/(label+'-partial.json')).write_text(json.dumps(record, indent=2)+'\n')
                print(f'{label} {phase}: {elapsed/1e6:.3f} ms', flush=True)
            end = client.request({'cmd': 'wait-serial', 'text': 'L1 DONE\n',
                'timeout_steps': args.steps-end['step'], 'slice_steps': 1024})
            record['serial'] += bytes.fromhex(end.get('serial_hex', '')).decode('ascii')
            record['steps'] = end['step']
            record['jit'] = stats_reply(client) if engine == 'jit' else ''
            client.request({'cmd': 'quit'})
            # quit closes the console client; the standalone server stays up.
            process.terminate()
            process.wait(timeout=5)
        finally:
            if client:
                client.close()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    return record


def wasm(args, engine, guest, boot, label):
    module = args.wasm_module
    report = args.output/(label+'-cli.json')
    cmd = command_prefix(args) + [args.node, *shlex.split(args.node_flags), '--import', (HERE/'observe.mjs').as_uri(), str(args.wasm_cli),
        '--wasm', str(module), '--bootsector', str(boot), '--hdd', str(guest), '--mem-mib', '16',
        '--clock', 'deterministic', '--cpu-engine', engine, '--jit-threshold', str(args.threshold),
        '--steps', str(args.steps), '--timeout', str(args.timeout), '--slice-steps', '4294967295',
        '--expect-serial', 'L1 DONE\n', '--report', str(report)]
    record = {'engine': engine, 'command': cmd, 'binary_sha256': sha(module), 'phases': {}, 'serial': ''}
    environment = os.environ.copy()
    environment['R86_LOOP_HOST_MODULE'] = str(args.wasm_cli.parent.parent/'web/wasm.mjs')
    process = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, 'stdout')
    selector.register(process.stderr, selectors.EVENT_READ, 'stderr')
    buffers = {'stdout': bytearray(), 'stderr': bytearray()}
    starts = {}
    deadline = time.monotonic()+args.timeout+15
    with (args.output/(label+'.log')).open('wb') as log:
        try:
            while selector.get_map():
                if time.monotonic() > deadline:
                    raise RuntimeError(f'{label} host timeout')
                for key, _ in selector.select(0.2):
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    log.write(data)
                    buffer = buffers[key.data]
                    buffer.extend(data)
                    while b'\n' in buffer:
                        line, _, rest = buffer.partition(b'\n')
                        buffer[:] = rest
                        text = line.decode('utf-8', errors='replace')
                        if key.data == 'stderr':
                            if text.startswith('L1_OBS '):
                                event = json.loads(text[7:])
                                phase = event['phase']
                                if event['edge'] == 'START':
                                    starts[phase] = event
                                else:
                                    start = starts[phase]
                                    before, after = counters(start['core']), counters(event['core'])
                                    elapsed = int(event['wall_ns'])
                                    record['phases'][phase] = {'wall_ns': elapsed,
                                        'observed_start_step': start['step'], 'observed_end_step': event['step'],
                                        'jit_before': start['core'], 'jit_after': event['core'],
                                        'jit_delta': {name: after[name]-before.get(name, 0) for name in after},
                                        'host_compile_ms': event['host']['compilation_latency_ms']-start['host']['compilation_latency_ms']}
                                    print(f'{label} {phase}: {elapsed/1e6:.3f} ms', flush=True)
                            continue
            process.wait(timeout=5)
            detail = json.loads(report.read_text())
            if process.returncode or not detail.get('matched'):
                raise RuntimeError(f'{label} failed; see {log.name}')
            record.update(serial=detail['serial'], steps=detail['steps'], jit=detail['jit_core'], host_jit=detail['jit'])
        finally:
            selector.close()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    return record


def finish_record(record):
    marks = {(phase, edge): int(tsc, 16) for phase, edge, tsc in MARKER.findall(record['serial'])}
    for phase, values in record['phases'].items():
        steps = marks[phase, 'END']-marks[phase, 'START']
        values['marker_start_step'] = marks[phase, 'START']
        values['marker_end_step'] = marks[phase, 'END']
        values['guest_steps'] = steps
        values['ns_per_step'] = values['wall_ns']/steps
    match = re.search(r'^L1 CHECKSUM ([0-9A-F]{8})$', record['serial'], re.M)
    if not match or len(record['phases']) != 4:
        raise RuntimeError('Missing phase or checksum')
    record['checksum'] = match[1]
    return record


def derive(runs, meta, threshold):
    if 'interp' not in runs or 'jit' not in runs:
        return {}
    i, j = runs['interp']['phases'], runs['jit']['phases']
    blocks = meta['blocks']
    ci = i['P2']['wall_ns']/(blocks*meta['page_iters'])
    cn = j['P2']['wall_ns']/(blocks*meta['page_iters'])
    cold = j['P4']['wall_ns']/(blocks*meta['fresh_pages']*meta['cold_iters'])
    count = blocks*meta['fresh_pages']
    compile_ns = j['P3']['wall_ns']/count-(threshold*cold+(meta['fresh_iters']-threshold)*cn)
    result = {'interpreter_ns_per_visit': ci, 'native_ns_per_visit': cn, 'cold_jit_ns_per_visit': cold,
        'threshold': threshold, 'fresh_block_instances': count, 'compile_ns_per_block_model': compile_ns,
        'break_even_visits_model': compile_ns/(cold-cn) if cold > cn else None,
        'model_warning': 'A weighted residual, not a compiler timer; phase overhead and actual admission affect it.'}
    counts = j['P3'].get('jit_delta', {})
    if counts.get('interpreted_steps') is not None:
        # Instruction-weighted effective cold visits include tiny driver/marker costs.
        effective = counts['interpreted_steps']/(meta['fresh_pages']*meta['chain_steps'])
        effective_compile = j['P3']['wall_ns']/count-(effective*cold+(meta['fresh_iters']-effective)*cn)
        result.update(effective_cold_visits=effective, compile_ns_per_block_effective=effective_compile,
            break_even_visits_effective=effective_compile/(cold-cn) if cold > cn else None)
    if compile_ns < 0:
        result['model_warning'] += ' Negative compile cost is not physical; this sample does not identify C.'
    profile = runs.get('profile', {}).get('phases', {}).get('P3', {}).get('jit_delta', {})
    compiled = profile.get('compiled_blocks', 0)
    if compiled:
        result['profile_compiled_blocks'] = compiled
        result['profile_compile_ns_per_compiled_block'] = profile.get('compile_nanoseconds', 0)/compiled
        result['profile_compile_ns_per_guest_block'] = profile.get('compile_nanoseconds', 0)/count
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, default=ROOT/'target/release/r86-run')
    p.add_argument('--profile-binary', type=Path)
    p.add_argument('--wasm', action='store_true')
    p.add_argument('--wasm-module', type=Path, default=ROOT/'web/dist/r86-jit.wasm')
    p.add_argument('--wasm-cli', type=Path, default=ROOT/'script/wasm_cli.cjs')
    p.add_argument('--node', default='node')
    p.add_argument('--node-flags', default=NODE_FLAGS)
    p.add_argument('--prefix', default='', help='e.g. qemu-aarch64; never interpreted by a shell')
    p.add_argument('--cpu', type=int)
    p.add_argument('--threshold', type=int)
    p.add_argument('--guest', type=Path, default=HERE/'loop.bin')
    p.add_argument('--quick', action='store_true', help='short function check; requires NASM, not a timing result')
    p.add_argument('--fresh-iters', type=int, help='assemble a variant; defaults to the committed guest')
    p.add_argument('--engines', choices=['both', 'interp', 'jit'], default='both')
    p.add_argument('--reference', type=Path, help='reuse interpreter endpoints/costs from this exact guest; no run is repeated')
    p.add_argument('--steps', type=int, default=4000000000)
    p.add_argument('--timeout', type=int, default=600)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    args.threshold = args.threshold if args.threshold is not None else (256 if args.wasm else 3)
    if args.wasm and args.fresh_iters is None:
        args.fresh_iters = 384
    for name in ['binary', 'wasm_module', 'wasm_cli', 'guest', 'output']:
        setattr(args, name, getattr(args, name).resolve())
    if args.profile_binary:
        if args.wasm:
            p.error('--profile-binary is a native-only cross-check')
        args.profile_binary = args.profile_binary.resolve()
    if args.output.exists() and any(args.output.iterdir()):
        p.error('--output must be empty')
    args.output.mkdir(parents=True, exist_ok=True)
    guest = args.guest
    if args.quick or args.fresh_iters is not None:
        guest = args.output/'loop.img'
        command = ['nasm', '-f', 'bin']
        if args.quick:
            command += ['-D', 'HOT_ITERS=2000', '-D', 'PAGE_ITERS=1000', '-D', 'FRESH_PAGES=4']
        if args.fresh_iters is not None:
            if args.fresh_iters < 1:
                p.error('--fresh-iters must be positive')
            command += ['-D', f'FRESH_ITERS={args.fresh_iters}']
        subprocess.run(command+[str(HERE/'loop.asm'), '-o', str(guest)], check=True)
    image = guest.read_bytes()
    header = struct.unpack_from('<8s8I', image, 448)
    if header[0] != b'R86LOOP1' or image[510:512] != b'\x55\xaa':
        p.error('invalid loop guest')
    meta = dict(zip(['hot_iters', 'page_iters', 'fresh_iters', 'fresh_pages', 'cold_iters', 'blocks', 'chain_steps', 'warm_iters'], header[1:]))
    boot = args.output/'boot.bin'
    boot.write_bytes(image[:512])
    result = {'schema': 1, 'host': platform.platform(), 'cpu': args.cpu, 'quick': args.quick,
        'mode': 'wasm' if args.wasm else 'native', 'guest': str(guest), 'guest_sha256': sha(guest),
        'guest_clock': 'deterministic', 'metadata': meta, 'runs': {}, 'completed': False}
    reference = None
    if args.reference:
        previous = json.loads(args.reference.read_text())
        if previous['guest_sha256'] != result['guest_sha256'] or previous['mode'] != result['mode']:
            p.error('--reference must use the identical guest and mode')
        result['runs']['interp'] = previous['runs']['interp']
        result['reference'] = str(args.reference.resolve())
    if not args.wasm and args.engines == 'jit' and not args.reference:
        p.error('native --engines jit needs --reference for exact marker endpoints')
    try:
        engines = ['interp', 'jit'] if args.engines == 'both' else [args.engines]
        for engine in engines:
            if engine == 'interp' and 'interp' in result['runs']:
                continue
            if not args.wasm and engine == 'jit':
                markers = {(phase, edge): int(value, 16) for phase, edge, value in MARKER.findall(result['runs']['interp']['serial'])}
                reference = {phase: {'start': markers[phase, 'START'], 'end': markers[phase, 'END']} for phase in ['P1','P2','P3','P4']}
            record = wasm(args, engine, guest, boot, engine) if args.wasm else native(args, engine, args.binary, guest, boot, engine, reference)
            result['runs'][engine] = finish_record(record)
            (args.output/'results.json').write_text(json.dumps(result, indent=2)+'\n')
        if args.profile_binary:
            result['runs']['profile'] = finish_record(native(args, 'jit', args.profile_binary, guest, boot, 'profile', reference))
        if len({r['checksum'] for r in result['runs'].values()}) != 1:
            raise RuntimeError('Interpreter/JIT checksum mismatch')
        phase_steps = [{phase: values['guest_steps'] for phase, values in r['phases'].items()} for r in result['runs'].values()]
        if any(steps != phase_steps[0] for steps in phase_steps):
            raise RuntimeError('Interpreter/JIT guest step mismatch')
        result['derived'] = derive(result['runs'], meta, args.threshold)
        result['completed'] = True
        print('engine phase guest_steps wall_ms ns/step')
        for engine, run in result['runs'].items():
            for phase, v in run['phases'].items():
                print(f"{engine:7} {phase} {v['guest_steps']:12d} {v['wall_ns']/1e6:10.3f} {v['ns_per_step']:9.3f}")
        print('checksum', next(iter(result['runs'].values()))['checksum'])
        print(json.dumps(result['derived'], indent=2))
    finally:
        result['seed_unchanged'] = sha(guest) == result['guest_sha256']
        (args.output/'results.json').write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
