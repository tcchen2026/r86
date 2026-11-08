#!/usr/bin/env python3
"""Exercise --term through a real PTY and pyte's independent VT parser.

Requires pyte and NASM; --direct also needs bsdtar. The synthetic guest checks colors, CP437, PS/2 taps,
resize repaint, queued paste through inhibited scanning, reset cancellation,
host controls while blocked, exit and signal restoration. --iso adds TinyCore shell
input, Ctrl-C, and a guest-initiated firmware reboot. Run from the repo root.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import termios
import time

import pyte


class Session:
    def __init__(self, binary, args, path):
        self.master, self.slave = pty.openpty()
        self.saved = termios.tcgetattr(self.slave)
        self.screen = pyte.Screen(80, 25)
        self.stream = pyte.ByteStream(self.screen)
        self.log = path.open('wb')
        self.resize(80, 25)
        self.proc = subprocess.Popen([str(binary), '--term', *args], stdin=self.slave,
                                     stdout=self.slave, stderr=self.slave)

    def resize(self, cols, rows):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        self.screen.resize(rows, cols)

    def pump(self, timeout=.05):
        if select.select([self.master], [], [], timeout)[0]:
            data = os.read(self.master, 65536)
            self.log.write(data)
            self.log.flush()
            self.stream.feed(data)

    def text(self):
        return '\n'.join(self.screen.display)

    def wait(self, text, timeout=120):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            if text in self.text():
                return
            if self.proc.poll() is not None:
                raise AssertionError(f'guest exited {self.proc.returncode}: {self.text()}')
        raise AssertionError(f'timed out waiting for {text!r}: {self.text()}')

    def send(self, data):
        os.write(self.master, data)

    def finish(self, expected=0):
        end = time.monotonic() + 10
        while self.proc.poll() is None and time.monotonic() < end:
            self.pump()
        assert self.proc.poll() == expected, self.proc.poll()
        assert termios.tcgetattr(self.slave) == self.saved, 'termios was not restored'

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        os.close(self.master)
        os.close(self.slave)
        self.log.close()


def queued_input(binary, out):
    sector = out / 'terminal-queue.bin'
    subprocess.run(['nasm', '-f', 'bin', 'tests/bootsectors/terminal_queue.asm',
                    '-o', str(sector)], check=True)
    image = bytearray(sector.read_bytes())
    for scenario in range(3):
        image[0x1f0] = scenario
        sector.write_bytes(image)
        session = Session(binary, ['--bootsector', str(sector), '--clock', 'realtime'],
                          out / f'queue-{scenario}.ansi')
        try:
            session.wait('R86_TERM_BLOCKED', timeout=10)
            if scenario == 1:
                # Host controls must bypass queued guest taps even when the
                # keyboard will never become ready.
                session.send(b'A' * 5000 + b'\x1dr')
                end = time.monotonic() + 10
                while not session.screen.display[1].startswith('2') and time.monotonic() < end:
                    session.pump()
                assert session.screen.display[1].startswith('2'), session.text()
                assert b'\x07' in (out / f'queue-{scenario}.ansi').read_bytes(), 'missing overflow bell'
                session.send(b'A' * 201 + b'\x1dq')
            else:
                session.send(b'A' * 201)
                if scenario == 2:
                    session.wait('R86_TERM_AFTER_RESET', timeout=10)
                    session.send(b'b')
                session.wait('R86_TERM_QUEUE_OK', timeout=10)
                session.send(b'\x1dq')
            session.finish()
            (out / f'queue-{scenario}-screen.txt').write_text(session.text())
        finally:
            session.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/r86-run'))
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--iso', type=Path)
    parser.add_argument('--direct', action='store_true', help='test direct Linux boot instead of firmware')
    args = parser.parse_args()
    out = args.output_dir
    out.mkdir(parents=True, exist_ok=True)
    sector = out / 'terminal.bin'
    subprocess.run(['nasm', '-f', 'bin', 'tests/bootsectors/terminal.asm', '-o', str(sector)], check=True)
    result = {}
    queued_input(args.binary, out)
    result['queued_paste_blocked_controls_reset_cancellation'] = 'passed'
    session = Session(args.binary, ['--bootsector', str(sector)], out / 'keys.ansi')
    try:
        session.wait('R86_TERM_READY ┌─┐')
        cell = session.screen.buffer[0][0]
        assert (cell.fg, cell.bg) == ('brightwhite', 'blue'), (cell.fg, cell.bg)
        session.send(b'A\x03\x1b[A\x1b[1;5C\x1b')
        session.wait('2A 1E 9E AA 1D 2E AE 9D E0 48 E0 C8 1D E0 4D E0 CD 9D 01 81')
        session.resize(100, 30)
        session.screen.reset()
        session.wait('R86_TERM_READY ┌─┐')
        session.send(b'\x1dr')
        end = time.monotonic() + 10
        while not session.screen.display[1].startswith('2') and time.monotonic() < end:
            session.pump()
        assert session.screen.display[1].startswith('2'), session.text()
        session.send(b'\x1dq')
        session.finish()
        result['keys_colors_cp437_resize_reset_exit'] = 'passed'
    finally:
        session.close()
    for sig in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT]:
        session = Session(args.binary, ['--bootsector', str(sector)], out / f'signal-{sig}.ansi')
        try:
            session.wait('R86_TERM_READY')
            session.proc.send_signal(sig)
            session.finish(expected=1)
            result[f'signal_{sig}_restores_termios'] = 'passed'
        finally:
            session.close()
    if args.iso:
        boot = []
        if args.direct:
            for image in ('vmlinuz', 'core.gz'):
                with (out / image).open('wb') as stream:
                    subprocess.run(['bsdtar', '-xOf', str(args.iso), f'boot/{image}'], stdout=stream, check=True)
            boot = ['--host-decompress-kernel', '--boot-linux-kernel', str(out / 'vmlinuz'),
                    '--boot-linux-initrd', str(out / 'core.gz')]
        session = Session(args.binary, [*boot, '--iso', str(args.iso)], out / 'firmware.ansi')
        try:
            session.wait('tc@box:~$')
            session.send(b"echo R86_TERM_'SHELL_OK'\r")
            session.wait('R86_TERM_SHELL_OK')
            session.send(b'sleep 100\r')
            # Let the guest enter sleep; Ctrl-C must go to the guest, not exit r86.
            end = time.monotonic() + 1
            while time.monotonic() < end:
                session.pump()
            session.send(b"\x03echo R86_CTRL_'C_OK'\r")
            session.wait('R86_CTRL_C_OK')
            session.send(b'clear; sudo reboot\r')
            if args.direct:
                session.screen.reset()
                session.wait('Core is distributed')
            else:
                session.wait('Decompressing')
            session.wait('tc@box:~$')
            session.send(b"echo R86_TERM_AFTER_'REBOOT'\r")
            session.wait('R86_TERM_AFTER_REBOOT')
            (out / 'firmware-screen.txt').write_text(session.text())
            session.send(b'\x1dq')
            session.finish()
            result['direct_shell_ctrl_c_guest_reboot' if args.direct else 'firmware_shell_ctrl_c_guest_reboot'] = 'passed'
        finally:
            session.close()
    (out / 'report.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
