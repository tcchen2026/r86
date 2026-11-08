#!/usr/bin/env python3
"""Exercise the shared WebSocket console through actual TCP connections.

Needs websockets. --browser additionally needs Playwright and local Chromium;
it verifies canvas painting and real DOM keyboard/pointer events. These tests
assert guest-observed bytes and failure snapshots, not frontend internals.
"""
import argparse
import contextlib
import json
from pathlib import Path
import re
import subprocess
import time
import urllib.request

from ws_cli import WsClient, parse_frame, synthetic_smoke


@contextlib.contextmanager
def server(binary, bootsector, directory, *extra, firmware=False):
    log = directory / (bootsector.stem + '.log')
    with log.open('w') as out:
        entry = ['--iso', str(bootsector), '--cmdline', 'loglevel=3 nozswap'] if firmware else ['--bootsector', str(bootsector)]
        process = subprocess.Popen([str(binary), *entry, '--clock', 'deterministic',
                                    '--ws-listen', '127.0.0.1:0', '--ws-slice-steps', '10000',
                                    *extra], stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                match = re.search(r'http://(127\.0\.0\.1:\d+)/', log.read_text())
                if match:
                    yield 'ws://' + match[1] + '/ws', 'http://' + match[1] + '/', process
                    return
                if process.poll() is not None:
                    raise AssertionError(log.read_text())
                time.sleep(.02)
            raise AssertionError('server did not start: ' + log.read_text())
        finally:
            process.terminate()
            process.wait(timeout=10)


@contextlib.contextmanager
def client(url):
    c = WsClient(url, 10)
    try:
        yield c
    finally:
        c.close()


def ok(c, cmd, **args):
    result = c.request(dict(cmd=cmd, **args))
    assert result.get('ok'), result
    return result


def serial(c):
    return bytes.fromhex(ok(c, 'logs', limit=65536)['serial_hex']).decode('ascii')


def frame(c, **args):
    reply = ok(c, 'frame', **args)
    if reply['changed']:
        opcode, payload = c.recv_frame()
        assert opcode == 2
        parse_frame(payload)
    return reply


def wait_busy(c):
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if ok(c, 'status')['execution']['busy']:
            return
        time.sleep(.001)
    raise AssertionError('finite job did not start')


def shared_console(binary, root, directory, browser):
    with server(binary, root/'tests/bootsectors/ws_ps2_vga.bin', directory, '--ws-record', str(directory/'input.jsonl')) as (url, http, _):
        with client(url) as a, client(url) as b:
            args = argparse.Namespace(timeout_steps=1000000, slice_steps=10000)
            synthetic_smoke(a, args)
            assert b'canvas' in urllib.request.urlopen(http).read()  # HTTP works with live WS peers.
            try:
                urllib.request.urlopen(http + 'not-a-file')
            except urllib.error.HTTPError as e:
                assert e.code == 404
            assert ok(b, 'status')['step'] == ok(a, 'status')['step']
            assert serial(a) == serial(b)  # Polling does not consume another reader's log.
            first = frame(a)
            assert frame(b)['generation'] == first['generation']
            assert not frame(a, since=first['generation'])['changed']
            before = len(serial(b))
            ok(a, 'keydown', key='controlleft')
            ok(b, 'keydown', key='ctrl')  # Same physical key, two owners.
            ok(a, 'release-input')
            ok(b, 'run-steps', steps=10000)
            assert serial(b)[before:] == 'K1D '
            ok(b, 'keyup', key='ctrl')
            ok(b, 'run-steps', steps=10000)
            assert serial(b)[before:] == 'K1D K9D '
            before = len(serial(b))
            with client(url) as c:
                ok(c, 'keydown', key='a')
            # A response after reconnect is ordered after the close event.
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and not serial(b)[before:].endswith('K9E '):
                ok(b, 'run-steps', steps=10000)
            assert serial(b)[before:] == 'K1E K9E '
            before = len(serial(b))
            ok(a, 'mouse-button', button='left', down=True)
            ok(b, 'mouse-button', button='left', down=True)
            ok(a, 'release-input')
            ok(b, 'run-steps', steps=10000)
            ok(b, 'mouse-move', dx=4, dy=-2)
            ok(b, 'run-steps', steps=10000)
            ok(b, 'release-input')
            ok(b, 'run-steps', steps=10000)
            assert serial(b)[before:] == 'M09 M00 M00 M09 M04 M02 M08 M00 M00 ', serial(b)[before:]
            # A huge finite request yields to a second debugger and can be cancelled.
            a.send_json({'cmd':'run-steps', 'steps':10**12})
            wait_busy(b)
            ok(b, 'pause')
            opcode, payload = a.recv_frame()
            assert opcode == 1 and json.loads(payload)['error'] == 'cancelled by pause'
            frozen = ok(a, 'status')['step']
            assert ok(b, 'status')['step'] == frozen
            # Abandoning a long job must not strand the VM or hold keys.
            with client(url) as c:
                c.send_json({'cmd':'run-steps', 'steps':10**12})
                wait_busy(b)
            deadline = time.monotonic() + 2
            while ok(b, 'status')['execution']['busy'] and time.monotonic() < deadline:
                time.sleep(.01)
            assert not ok(b, 'status')['execution']['busy']
            ok(a, 'resume')
            step = ok(b, 'status')['step']
            assert ok(b, 'status')['step'] > step
            ok(b, 'pause')
            if browser:
                browser_console(http, b, directory)
        recorded = [json.loads(line) for line in (directory/'input.jsonl').read_text().splitlines()]
        assert all(a['step'] <= b['step'] for a,b in zip(recorded,recorded[1:]))
        control = [e['cmd'] for e in recorded if e.get('key') in ('ctrl', 'controlleft')]
        assert control[:2] == ['keydown', 'keyup'], control
        print('PASS shared HTTP/WS, independent logs/frames, combined input, recording, cancellable jobs')


def queued_text(binary, root, directory):
    record = directory / 'queued-input.jsonl'
    bootsector = root / 'tests/bootsectors/ws_ps2_vga.bin'
    with server(binary, bootsector, directory, '--ws-record', str(record)) as (url, _, _):
        with client(url) as a, client(url) as b:
            ok(a, 'wait-serial', text='R86_SYN_READY ', timeout_steps=100000)
            before = serial(a)
            frozen = ok(a, 'status')['step']
            accepted = ok(a, 'type', text='a' * 200 + '\n')
            assert ok(b, 'status')['step'] == frozen
            assert serial(b) == before
            ok(b, 'run-steps', steps=3000000)
            expected = 'K1E K9E ' * 200 + 'K1C K9C '
            received = serial(b)[len(before):]
            assert received == expected, (len(received), len(expected), received[-80:])
            assert accepted['queued_bytes'] == 201 and 'applied_step' not in accepted, accepted
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 0

            # Admission is atomic: reject unsupported text and an overfull queue
            # before emitting any key. A second client's cancellation is local.
            for text in ('a\u00e9', 'a\x00', 'x' * 4097):
                assert not a.request(dict(cmd='type', text=text))['ok']
            assert ok(a, 'status')['execution']['pending_text_bytes'] == 0
            ok(a, 'type', text='x' * 4096)
            assert not b.request(dict(cmd='type', text='b'))['ok']
            ok(b, 'release-input')
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 4096
            ok(a, 'release-input')
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 0

            # Existing symbolic key ownership takes precedence over typed text.
            # Whole text commands stay in order; disconnect releases physical
            # holds but preserves a one-shot client's accepted text command.
            before = serial(a)
            ok(a, 'keydown', key='leftshift')
            with client(url) as c:
                ok(c, 'type', text='B!\t')
            ok(b, 'type', text='a\n')
            ok(b, 'run-steps', steps=100000)
            assert serial(b)[len(before):] == 'K2A '
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 5
            ok(a, 'keyup', key='leftshift')
            ok(b, 'run-steps', steps=100000)
            expected = ('K2A KAA K2A K30 KB0 KAA K2A K02 K82 KAA '
                        'K0F K8F K1E K9E K1C K9C ')
            assert serial(b)[len(before):] == expected, serial(b)[len(before):]
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 0
            serial_hex = ok(b, 'logs', limit=65536)['serial_hex']
            steps = ok(b, 'status')['step']

            # Replay actual deliveries, not the original large admission.
            events = [json.loads(line) for line in record.read_text().splitlines()]
            typed = [e for e in events if e['cmd'] == 'type']
            assert ''.join(e['text'] for e in typed) == 'a' * 200 + '\nB!\ta\n'
            assert all(len(e['text']) == 1 for e in typed)
            assert all(a['step'] < b['step'] for a, b in zip(typed, typed[1:]))
            replay = directory / 'queued-replay.jsonl'
            replay.write_text(record.read_text())
            replayed = subprocess.run(
                [str(binary), '--bootsector', str(bootsector), '--clock', 'deterministic',
                 '--steps', str(steps), '--replay', str(replay)],
                capture_output=True, text=True, timeout=30, check=True)
            assert bytes.fromhex(serial_hex).decode('ascii') in replayed.stdout, replayed.stdout[-2000:]

            # Explicit reset must not replay queued pre-reset input.
            ok(a, 'type', text='x' * 100)
            ok(b, 'reset')
            assert ok(b, 'status')['execution']['pending_text_bytes'] == 0
            ok(b, 'wait-serial', text='R86_SYN_READY ', timeout_steps=100000)
            assert not ok(b, 'diagnostics')['failures']
    print('PASS long text, bounded admission, pause, multi-client ownership and exact input replay')


def browser_console(http, debugger, directory):
    from playwright.sync_api import sync_playwright
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path='/usr/bin/chromium', headless=True,
                                    args=['--no-sandbox'])
        page = browser.new_page(viewport={'width':1100, 'height':900})
        errors = []
        page.on('pageerror', lambda e: errors.append(str(e)))
        page.goto(http)
        page.wait_for_function("document.querySelector('#state').textContent.startsWith('paused')")
        page.wait_for_function("document.querySelector('#screen').getContext('2d').getImageData(0,0,1,1).data[3] === 255")
        page.click('#resume')
        page.wait_for_function("document.querySelector('#state').textContent.startsWith('running')")
        before = len(serial(debugger))
        page.click('#screen')
        page.wait_for_function("document.pointerLockElement === document.querySelector('#screen')")
        page.keyboard.press('ArrowUp')
        page.keyboard.press('F8')
        page.keyboard.press('Shift+KeyA')
        page.mouse.move(560, 390)
        page.mouse.down()
        page.mouse.up()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            observed = serial(debugger)[before:]
            if 'K2A K1E K9E KAA ' in observed and 'M' in observed:
                break
            page.wait_for_timeout(20)
        observed = serial(debugger)[before:]
        assert 'KE0 K48 KE0 KC8 ' in observed, observed
        assert 'K42 KC2 ' in observed, observed
        assert 'K2A K1E K9E KAA ' in observed, observed
        assert 'M' in observed, observed
        before_repeat = len(serial(debugger))
        page.keyboard.down('KeyB')
        page.keyboard.down('KeyB')  # Browser repeat event -> another make, one break.
        page.keyboard.up('KeyB')
        deadline = time.monotonic() + 3
        while 'K30 K30 KB0 ' not in serial(debugger)[before_repeat:] and time.monotonic() < deadline:
            page.wait_for_timeout(20)
        assert 'K30 K30 KB0 ' in serial(debugger)[before_repeat:]
        # Focus loss releases a held key, even without a browser keyup event.
        page.keyboard.down('Control')
        page.evaluate("window.dispatchEvent(new Event('blur'))")
        deadline = time.monotonic() + 3
        while not serial(debugger).endswith('K1D K9D ') and time.monotonic() < deadline:
            page.wait_for_timeout(20)
        assert serial(debugger).endswith('K1D K9D '), serial(debugger)[before:]
        page.keyboard.up('Control')
        page.evaluate('document.exitPointerLock()')
        page.click('#pause')
        page.wait_for_function("document.querySelector('#state').textContent.startsWith('paused')")
        page.screenshot(path=str(directory/'console.png'))
        with page.expect_download() as download:
            page.click('#diag')
        assert 'cpu_dump' in Path(download.value.path()).read_text()
        assert not errors, errors
        browser.close()
    print('PASS Chromium display, keyboard, pointer, blur release and diagnostic UI')


def tinycore_browser(binary, iso, directory):
    from playwright.sync_api import sync_playwright
    with server(binary, iso, directory, '--ws-slice-steps', '1000000', firmware=True) as (url, http, _), client(url) as debugger, sync_playwright() as p:
        browser = p.chromium.launch(executable_path='/usr/bin/chromium', headless=True, args=['--no-sandbox'])
        page = browser.new_page(viewport={'width':1100, 'height':900})
        errors = []
        page.on('pageerror', lambda e: errors.append(str(e)))
        page.goto(http)
        page.wait_for_function("!document.querySelector('#resume').disabled")
        page.click('#resume')

        def wait_text(needle, timeout=150):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                text = ok(debugger, 'text-snapshot')['text']
                if needle in text:
                    return
                assert ok(debugger, 'status')['execution']['running']
                page.wait_for_timeout(250)
            raise AssertionError((needle, text))

        wait_text('tc@box:~$')
        page.click('#screen')
        # Quotes prevent the expected marker appearing in the command itself.
        page.keyboard.type("echo r86web'shellok'", delay=25)
        page.keyboard.press('Enter')
        wait_text('r86webshellok', 30)
        page.screenshot(path=str(directory/'tinycore-browser.png'))
        count = ok(debugger, 'status')['reset_count']
        page.keyboard.type('sudo reboot', delay=25)
        page.keyboard.press('Enter')
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and ok(debugger, 'status')['reset_count'] == count:
            page.wait_for_timeout(100)
        assert ok(debugger, 'status')['reset_count'] > count
        wait_text('tc@box:~$')
        page.keyboard.type("echo r86web'rebootok'", delay=25)
        page.keyboard.press('Enter')
        wait_text('r86webrebootok', 30)
        assert not errors, errors
        ok(debugger, 'pause')
        browser.close()
    print('PASS Chromium TinyCore firmware boot, physical keyboard, guest reboot and shell recovery')


def unmapped_bus(binary, directory):
    # Probe one absent port 1500 times, then another target page once.
    # Real guest execution must continue; both observers see the same logs.
    program = bytes.fromhex('fa31c08ed8ba3412b9dc05ece2fdba4523ecf4ebfd')
    path = directory / 'unmapped-bus.bin'
    path.write_bytes(program + bytes(510-len(program)) + b'\x55\xaa')
    with server(binary, path, directory) as (url, _, _), client(url) as a, client(url) as b:
        cursor = 0
        warnings = []
        for _ in range(9):
            run = a.request({'cmd':'run-steps', 'steps':400})
            # The fixture ends at CLI; HLT with no timer programmed. Its final
            # short run is a normal halt, not a failed absent-port access.
            halted = run.get('stop') == {'kind':'halted'}
            assert run.get('ok') or halted, run
            logs = ok(a, 'logs', event_from=cursor)
            assert ok(b, 'logs', event_from=cursor)['events'] == logs['events']
            warnings.extend(e for e in logs['events'] if e['kind'] == 'unmapped-bus')
            cursor = logs['event_next']
            if halted:
                break
        diagnostic = ok(b, 'diagnostics')
        assert not diagnostic['failures']
        pages = {(p['space'], p['page']): p for p in diagnostic['current']['bus']['pages']}
        assert pages['io', 0x1000]['count'] == 1500, pages
        assert pages['io', 0x1000]['suppressed'] == 500
        assert pages['io', 0x1000]['muted']
        assert pages['io', 0x2000]['count'] == 1
        assert pages['io', 0x2000]['suppressed'] == 1  # Source alone is muted.
        assert not pages['io', 0x2000]['muted']
        sources = diagnostic['current']['bus']['instruction_pages']
        assert sources == [{'page':0x7000, 'count':1501, 'suppressed':501, 'muted':True}], sources
        assert len(warnings) == 1000, len(warnings)
        assert sum('further logs muted by page limit' in e['message'] for e in warnings) == 1
        ok(a, 'reset')
        run = a.request({'cmd':'run-steps', 'steps':10000})
        assert run.get('stop') == {'kind':'halted'}, run
        pages = ok(b, 'diagnostics')['current']['bus']['pages']
        page = next(p for p in pages if p['space'] == 'io' and p['page'] == 0x1000)
        assert page['count'] == 3000 and page['suppressed'] == 2000
        assert all('page=0x1000 ' not in e['message'] for e in ok(b, 'logs', event_from=cursor)['events'])
    print('PASS absent bus reads, per-page muting, reset persistence and independent log readers')


def crashes(binary, directory):
    # Unsupported execution stops the guest but keeps the server and evidence.
    programs = {'unsupported': bytes.fromhex('0fc8')}
    for name, program in programs.items():
        path = directory / (name+'.bin')
        path.write_bytes(program + bytes(510-len(program)) + b'\x55\xaa')
        with server(binary, path, directory, '--minimal-ring') as (url, _, _), client(url) as a, client(url) as b:
            ok(a, 'type', text='a' * 100)
            ok(a, 'resume')
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                d = ok(b, 'diagnostics')
                if d['failures']:
                    break
                time.sleep(.01)
            assert len(d['failures']) == 1, d
            assert d['current']['execution']['pending_text_bytes'] == 0
            snapshot = d['failures'][0]
            assert snapshot['execution']['stop']['kind'] in ('error', 'stopped'), snapshot
            before = ok(a, 'status')['step']
            assert ok(b, 'status')['step'] == before
            ok(a, 'reset')
            assert ok(b, 'diagnostics')['failures'][0] == snapshot
            response = a.request({'cmd':'run-steps', 'steps':1000})
            assert not response['ok'], response
            assert len(ok(b, 'diagnostics')['failures']) == 2
            assert len([e for e in ok(a, 'logs')['events'] if e['kind']=='crash']) == 2
        print('PASS retained crash through resume, reset and stepping:', name)

    # COM1 'T', LIDT with a zero limit, UD2: #UD -> #GP -> #DF -> shutdown.
    # The PC resets and keeps running; debuggers retain the pre-reset evidence.
    program = bytes.fromhex('fa31c08ed8b054baf803ee0f011e127c0f0b') + bytes(6)
    path = directory / 'triple-fault.bin'
    path.write_bytes(program + bytes(510-len(program)) + b'\x55\xaa')
    with server(binary, path, directory, '--minimal-ring') as (url, _, _), client(url) as a, client(url) as b:
        ok(a, 'type', text='a' * 100)
        ok(a, 'run-steps', steps=10)
        assert ok(b, 'status')['reset_count'] == 1
        assert ok(b, 'status')['execution']['pending_text_bytes'] == 0
        d = ok(b, 'diagnostics')
        snapshot = d['failures'][-1]
        assert snapshot['execution']['stop']['kind'] == 'triple-fault', snapshot
        assert snapshot['reset'] and bytes.fromhex(snapshot['serial_hex']) == b'T'
        assert 'eip: 31760' in snapshot['cpu_dump'], snapshot
        assert ok(a, 'diagnostics')['failures'][-1] == snapshot
        ok(a, 'reset')
        assert ok(b, 'diagnostics')['failures'][-1] == snapshot
        ok(a, 'resume')
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            state = ok(b, 'status')
            if state['reset_count'] >= 3:
                break
            time.sleep(.01)
        assert state['reset_count'] >= 3 and state['execution']['running'], state
        ok(a, 'pause')
        assert all(f['execution']['stop']['kind'] == 'triple-fault'
                   for f in ok(b, 'diagnostics')['failures'])
        assert any(e['kind'] == 'cpu-shutdown' for e in ok(b, 'logs')['events'])
    print('PASS triple-fault board reset and shared pre-reset diagnostics')


def power_off(binary, root, directory):
    bootsector = root/'tests/bootsectors/acpi_button.bin'
    # Start paused so the button is acknowledged before either execution path
    # reaches S5. The server outlives the guest, like a BMC.
    for command in ({'cmd': 'resume'}, {'cmd': 'run-steps', 'steps': 10000}):
        with server(binary, bootsector, directory) as (url, _, process), client(url) as c:
            result = c.request({'cmd': 'run-steps', 'steps': 2000})
            assert result['stop']['kind'] == 'halted', result
            assert ok(c, 'status')['cpu']['halted']
            ok(c, 'power-button')
            ok(c, **command)
            deadline = time.monotonic() + 5
            while not ok(c, 'status')['powered_off'] and time.monotonic() < deadline:
                time.sleep(.01)
            assert ok(c, 'status')['powered_off'] and process.poll() is None
            assert 'guest powered off' in (directory/'acpi_button.log').read_text()
            ok(c, 'power-on')
            state = ok(c, 'status')
            assert not state['powered_off'] and state['execution']['running'], state
            ok(c, 'power-off')
            assert ok(c, 'status')['powered_off']
    print('PASS ACPI and host power-off keep the server up; power-on boots again')

    with server(binary, bootsector, directory) as (url, _, process), client(url) as c:
        ok(c, 'resume')
        deadline = time.monotonic() + 3
        while not ok(c, 'status')['cpu']['halted'] and time.monotonic() < deadline:
            time.sleep(.001)
        assert ok(c, 'status')['cpu']['halted']
        ok(c, 'type', text='a' * 100)
        ok(c, 'power-button')
        while not ok(c, 'status')['powered_off'] and time.monotonic() < deadline:
            time.sleep(.001)
        state = ok(c, 'status')
        assert state['powered_off'] and not state['execution']['running'], state
        assert state['execution']['stop']['kind'] == 'powered-off'
        assert state['execution']['pending_text_bytes'] == 0
        assert not c.request(dict(cmd='type', text='b'))['ok']
        assert not ok(c, 'diagnostics')['failures']
        assert ok(c, 'run-steps', steps=10000)['ran'] == 0
        assert ok(c, 'status')['step'] == state['step']
        assert process.poll() is None
        ok(c, 'reset')
        assert not ok(c, 'status')['powered_off']
        result = c.request({'cmd': 'run-steps', 'steps': 2000})
        assert result['ran'] > 0 and result['stop']['kind'] == 'halted', result
    print('PASS retained power-off keeps diagnostics, freezes execution, and permits reset')


def realtime_wait(binary, root, directory):
    # This guest first runs busy, then sleeps through real PIT/RTC interrupts.
    # The wait command must survive HLT without switching to continuous resume.
    with server(binary, root/'tests/bootsectors/clock.bin', directory,
                '--clock', 'realtime', '--do-not-exit-when-poweroff') as (url, _, _), client(url) as c:
        assert ok(c, 'status')['clock'] == 'realtime'
        before = time.monotonic()
        ok(c, 'wait-serial', text='SBI', timeout_steps=1000000000000)
        elapsed = time.monotonic() - before
        assert 2.3 <= elapsed < 10, elapsed
        assert not ok(c, 'diagnostics')['failures']
    print('PASS realtime debugger wait survives HLT and device interrupts')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/r86-run'))
    parser.add_argument('--browser', action='store_true')
    parser.add_argument('--tinycore', type=Path, help='also boot this ISO in Chromium, type a shell command, and reboot')
    parser.add_argument('--output', type=Path, default=Path('tmp/ws-check'))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parents[1]
    shared_console(args.binary.resolve(), root, args.output, args.browser)
    queued_text(args.binary.resolve(), root, args.output)
    unmapped_bus(args.binary.resolve(), args.output)
    crashes(args.binary.resolve(), args.output)
    power_off(args.binary.resolve(), root, args.output)
    realtime_wait(args.binary.resolve(), root, args.output)
    if args.tinycore:
        tinycore_browser(args.binary.resolve(), args.tinycore.resolve(), args.output)


if __name__ == '__main__':
    main()
