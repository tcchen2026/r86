//! Unix VGA text console. Host terminal bytes become PS/2 key taps; serial
//! debug output remains in Machine's transcript and never enters the screen.
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use r86::devices::{RenderMode, TextSnapshot};
use r86::machine::{GuestStepStatus, Machine};

static SIGNAL: AtomicI32 = AtomicI32::new(0);
extern "C" fn on_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

struct Terminal {
    saved: libc::termios,
    signals: Vec<(libc::c_int, libc::sigaction)>,
}

impl Terminal {
    fn open() -> Result<Self, String> {
        // The file descriptors belong to the process, and all libc output
        // buffers are initialized, correctly sized, and used only on success.
        unsafe {
            if libc::isatty(0) != 1 || libc::isatty(1) != 1 {
                return Err("--term requires a terminal on stdin and stdout".into());
            }
            let mut saved = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            let mut term = Self {
                saved,
                signals: Vec::new(),
            };
            SIGNAL.store(0, Ordering::Relaxed);
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut old = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) != 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
                term.signals.push((signal, old));
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            write_output("\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[0m\x1b[2J")?;
            Ok(term)
        }
    }

    fn size(&self) -> (u16, u16) {
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0
                && size.ws_col > 0
                && size.ws_row > 0
            {
                (size.ws_col, size.ws_row)
            } else {
                (80, 25)
            }
        }
    }

    fn read(&self, bytes: &mut [u8]) -> Result<usize, String> {
        unsafe {
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut fd, 1, 0) < 0 {
                let err = io::Error::last_os_error();
                return if err.kind() == io::ErrorKind::Interrupted {
                    Ok(0)
                } else {
                    Err(err.to_string())
                };
            }
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err("terminal disconnected".into());
            }
            if fd.revents & libc::POLLIN == 0 {
                return Ok(0);
            }
            let n = libc::read(0, bytes.as_mut_ptr().cast(), bytes.len());
            if n >= 0 {
                Ok(n as usize)
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    Ok(0)
                } else {
                    Err(err.to_string())
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Restore the exact original termios, including ISIG and input/output
        // translations. Do this before emitting diagnostics on the main path.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
        let _ = write_output("\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l");
        for (signal, old) in &self.signals {
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

fn write_output(text: &str) -> Result<(), String> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

pub fn run(
    machine: &mut Machine,
    steps: u64,
    do_not_exit_when_poweroff: bool,
    network: &mut Option<r86::network::UserNetwork>,
) -> Result<(), String> {
    let terminal = Terminal::open()?;
    let deadline = if steps == 0 {
        u64::MAX
    } else {
        machine.step_count().saturating_add(steps)
    };
    let mut input = Input::default();
    let mut taps = VecDeque::new();
    let mut reset_count = machine.reset_count();
    let mut screen = Screen::default();
    let mut next_frame = Instant::now();
    let mut size = (0, 0);
    while machine.step_count() < deadline {
        if machine.reset_count() != reset_count {
            input = Input::default();
            taps.clear();
            reset_count = machine.reset_count();
        }
        let signal = SIGNAL.load(Ordering::Relaxed);
        if signal != 0 {
            return Err(format!("terminal interrupted by signal {signal}"));
        }
        let mut bytes = [0; 1024];
        if input.pending.len() < 1024 {
            let n = terminal.read(&mut bytes)?;
            if n != 0 {
                input.feed(&bytes[..n]);
            }
        }
        // Parse host commands even while the keyboard is inhibited. Guest
        // taps wait here instead of overflowing the device queue or losing
        // modifiers while scanning is disabled. This is frontend buffering;
        // the core and ordinary physical key event path need no scheduler.
        let mut full = false;
        while let Some(key) = input.next() {
            match key {
                Key::Exit => return Ok(()),
                Key::Reboot => {
                    machine.reboot()?;
                    input = Input::default();
                    taps.clear();
                    reset_count = machine.reset_count();
                    break;
                }
                key if taps.len() < 4096 => taps.push_back(key),
                _ => full = true,
            }
        }
        if full {
            // A terminal stream has no request/ACK protocol. Ring its input
            // overflow bell for rejected new taps; retain accepted taps and
            // keep the host exit/reset commands usable at capacity.
            write_output("\x07")?;
        }
        if !machine.powered_off()
            && machine.keyboard_ready_for_text()
            && let Some(key) = taps.pop_front()
        {
            inject(machine, key);
        }
        if !machine.powered_off() {
            if let Some(net) = network {
                net.poll(machine).map_err(|e| format!("network: {e}"))?;
            }
            let report = machine.run_steps((deadline - machine.step_count()).min(250_000))?;
            if let Some(status) = report.stop {
                match status {
                    GuestStepStatus::Halted => {
                        let wait = machine.halted_wait_duration();
                        std::thread::sleep(
                            network.as_ref().map_or(wait, |net| net.limit_wait(wait)),
                        );
                    }
                    GuestStepStatus::Stopped { message, .. } => return Err(message),
                    GuestStepStatus::PoweredOff if !do_not_exit_when_poweroff => return Ok(()),
                    GuestStepStatus::PoweredOff => taps.clear(),
                    _ => unreachable!("Machine handles reset before returning"),
                }
            }
        }
        if Instant::now() >= next_frame {
            let new_size = terminal.size();
            if new_size != size {
                screen = Screen::default();
                size = new_size;
            }
            if machine.vga().whole_frame_dirty() || screen.mode.is_none() {
                let mode = machine.vga().mode_info().mode;
                let text = machine.vga().text_snapshot();
                write_output(&screen.render(mode, &text, size))?;
                machine.clear_frame_dirty();
            }
            next_frame = Instant::now() + Duration::from_millis(16);
        }
        if machine.powered_off() {
            std::thread::sleep(Duration::from_millis(16));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Screen {
    mode: Option<RenderMode>,
    previous: Option<TextSnapshot>,
}

impl Screen {
    fn render(&mut self, mode: RenderMode, text: &TextSnapshot, size: (u16, u16)) -> String {
        let full = self.mode != Some(mode)
            || self.previous.as_ref().is_none_or(|p| {
                p.cols != text.cols || p.rows != text.rows || p.blink_enabled != text.blink_enabled
            });
        self.mode = Some(mode);
        let mut out = String::new();
        if full {
            out.push_str("\x1b[?25l\x1b[0m\x1b[2J\x1b[H");
        }
        if mode != RenderMode::Text {
            if full && mode != RenderMode::Blank {
                out.push_str("Guest graphics mode; use --sdl2 to display it.");
            }
            self.previous = None;
            return out;
        }
        let mut attr = None;
        for row in 0..text.rows.min(size.1) {
            let mut consecutive = false;
            for col in 0..text.cols.min(size.0) {
                let index = usize::from(row * text.cols + col);
                let cell = text.raw_cells[index];
                if !full
                    && self
                        .previous
                        .as_ref()
                        .is_some_and(|p| p.raw_cells[index] == cell)
                {
                    consecutive = false;
                    continue;
                }
                if !consecutive {
                    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
                }
                if attr != Some(cell[1]) {
                    let a = cell[1];
                    let ansi = |v: u8| ((v & 1) << 2) | (v & 2) | ((v & 4) >> 2);
                    let fg = ansi(a) + if a & 8 != 0 { 90 } else { 30 };
                    let bg = ansi(a >> 4)
                        + if !text.blink_enabled && a & 0x80 != 0 {
                            100
                        } else {
                            40
                        };
                    let blink = if text.blink_enabled && a & 0x80 != 0 {
                        5
                    } else {
                        25
                    };
                    let _ = write!(out, "\x1b[0;{fg};{bg};{blink}m");
                    attr = Some(a);
                }
                out.push(cp437(cell[0]));
                consecutive = true;
            }
        }
        match text
            .cursor
            .filter(|(col, row)| *col < size.0 && *row < size.1)
        {
            Some((col, row)) => {
                let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
            }
            None => out.push_str("\x1b[?25l"),
        }
        self.previous = Some(text.clone());
        out
    }
}

fn cp437(byte: u8) -> char {
    const LOW: &str = " ☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼";
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■ ";
    match byte {
        0..=31 => LOW.chars().nth(byte as usize).unwrap(),
        127 => '⌂',
        128..=255 => HIGH.chars().nth(byte as usize - 128).unwrap(),
        _ => char::from(byte),
    }
}

#[derive(Debug, PartialEq)]
enum Key {
    Ascii(u8, u8),
    Scan(u8, bool, u8),
    Exit,
    Reboot,
}

#[derive(Default)]
struct Input {
    pending: Vec<u8>,
    escape_since: Option<Instant>,
    command: bool,
}

impl Input {
    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    fn next(&mut self) -> Option<Key> {
        loop {
            let byte = *self.pending.first()?;
            if self.command {
                self.pending.remove(0);
                self.command = false;
                return match byte {
                    b'q' => Some(Key::Exit),
                    b'r' => Some(Key::Reboot),
                    0x1d => Some(Key::Ascii(b']', 4)),
                    _ => None,
                };
            }
            if byte == 0x1d {
                self.pending.remove(0);
                self.command = true;
                continue;
            }
            if byte != 0x1b {
                self.pending.remove(0);
                return ascii(byte, 0);
            }
            let since = *self.escape_since.get_or_insert_with(Instant::now);
            if self.pending.len() == 1 {
                if since.elapsed() < Duration::from_millis(30) {
                    return None;
                }
                self.pending.remove(0);
                self.escape_since = None;
                return Some(Key::Scan(0x76, false, 0));
            }
            if !matches!(self.pending[1], b'[' | b'O') {
                let second = self.pending[1];
                self.pending.drain(..2);
                self.escape_since = None;
                return ascii(second, 2);
            }
            if let Some(end) = self.pending[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
            {
                let end = end + 2;
                let key = escape_key(&self.pending[2..=end]);
                self.pending.drain(..=end);
                self.escape_since = None;
                return key;
            }
            if self.pending.len() > 32 || since.elapsed() >= Duration::from_millis(30) {
                self.pending.clear();
                self.escape_since = None;
            }
            return None;
        }
    }
}

fn ascii(byte: u8, mods: u8) -> Option<Key> {
    Some(match byte {
        8 | 127 => Key::Scan(0x66, false, mods),
        9 | 10 | 13 | 32..=126 => Key::Ascii(byte, mods),
        0 => Key::Ascii(b' ', mods | 4),
        1..=26 => Key::Ascii(b'a' + byte - 1, mods | 4),
        28..=31 => Key::Ascii(b'\\' + byte - 28, mods | 4),
        _ => return None,
    })
}

fn escape_key(sequence: &[u8]) -> Option<Key> {
    let (&last, parameters) = sequence.split_last()?;
    let parameters = std::str::from_utf8(parameters).ok()?;
    let mut params = parameters.split(';');
    let number: u8 = params.next().unwrap_or("").parse().unwrap_or(1);
    let mods: u8 = match params.next() {
        None => 0,
        Some(n) => n.parse::<u8>().ok()?.checked_sub(1)?,
    };
    if mods > 7 || params.next().is_some() {
        return None;
    }
    let (code, ext) = match last {
        b'A' => (0x75, true),
        b'B' => (0x72, true),
        b'C' => (0x74, true),
        b'D' => (0x6b, true),
        b'H' => (0x6c, true),
        b'F' => (0x69, true),
        b'P' => (0x05, false),
        b'Q' => (0x06, false),
        b'R' => (0x04, false),
        b'S' => (0x0c, false),
        b'Z' => return Some(Key::Scan(0x0d, false, 1)),
        b'~' => match number {
            1 | 7 => (0x6c, true),
            2 => (0x70, true),
            3 => (0x71, true),
            4 | 8 => (0x69, true),
            5 => (0x7d, true),
            6 => (0x7a, true),
            11 => (0x05, false),
            12 => (0x06, false),
            13 => (0x04, false),
            14 => (0x0c, false),
            15 => (0x03, false),
            17 => (0x0b, false),
            18 => (0x83, false),
            19 => (0x0a, false),
            20 => (0x01, false),
            21 => (0x09, false),
            23 => (0x78, false),
            24 => (0x07, false),
            _ => return None,
        },
        _ => return None,
    };
    Some(Key::Scan(code, ext, mods))
}

fn inject(machine: &mut Machine, key: Key) {
    let mods = match key {
        Key::Ascii(_, m) | Key::Scan(_, _, m) => m,
        _ => return,
    };
    for (bit, code) in [(1, 0x12), (2, 0x11), (4, 0x14)] {
        if mods & bit != 0 {
            machine.inject_scancode_set2(&[code]);
        }
    }
    match key {
        Key::Ascii(byte, _) => machine.inject_ascii(&[byte]),
        Key::Scan(code, ext, _) => {
            if ext {
                machine.inject_scancode_set2(&[0xe0, code, 0xe0, 0xf0, code]);
            } else {
                machine.inject_scancode_set2(&[code, 0xf0, code]);
            }
        }
        _ => {}
    }
    for (bit, code) in [(4, 0x14), (2, 0x11), (1, 0x12)] {
        if mods & bit != 0 {
            machine.inject_scancode_set2(&[0xf0, code]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_survive_split_escape_sequences_and_preserve_guest_control_keys() {
        let mut input = Input::default();
        input.feed(b"\x1b[");
        assert_eq!(input.next(), None);
        input.feed(b"1;5A\x03\x1bx\x1d]\x1dq");
        assert_eq!(input.next(), Some(Key::Scan(0x75, true, 4)));
        assert_eq!(input.next(), Some(Key::Ascii(b'c', 4)));
        assert_eq!(input.next(), Some(Key::Ascii(b'x', 2)));
        assert_eq!(input.next(), None);
        assert_eq!(input.next(), Some(Key::Exit));
        input.feed(b"\x1b");
        input.escape_since = Some(Instant::now() - Duration::from_millis(40));
        assert_eq!(input.next(), Some(Key::Scan(0x76, false, 0)));
    }

    #[test]
    fn text_renderer_preserves_cp437_colors_cursor_and_changed_cells() {
        let mut text = TextSnapshot {
            cols: 2,
            rows: 1,
            cursor_offset: 1,
            cells: vec![32, 32],
            raw_cells: vec![[0xda, 0x1f], [0xc4, 0x1f]],
            cursor: Some((1, 0)),
            blink_enabled: true,
        };
        let mut screen = Screen::default();
        let first = screen.render(RenderMode::Text, &text, (80, 25));
        assert!(first.contains("┌─"));
        assert!(first.contains("0;97;44;25m"));
        assert!(first.contains("\x1b[1;2H\x1b[?25h"));
        assert!(
            !screen
                .render(RenderMode::Text, &text, (80, 25))
                .contains('┌')
        );
        text.raw_cells[1][0] = 0xbf;
        let changed = screen.render(RenderMode::Text, &text, (80, 25));
        assert!(changed.contains('┐'));
        assert!(!changed.contains('┌'));
        for byte in 0..=255 {
            assert!(!cp437(byte).is_control());
        }
    }
}
