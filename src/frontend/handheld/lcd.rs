//! Linux framebuffer output and evdev keyboard input for the real device.
//!
//! The framebuffer is written with pwrite, one run of changed rows at a time.
//! SPI panels (the CardputerZero ST7789) refresh only written ranges, so
//! unchanged rows cost no bus time. A regular file is accepted in place of a
//! device for host testing; it is treated as a 320x170 RGB565 panel.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cardputer;
use super::device::{Device, KeyEvent};
use super::keymap;
use super::ui::Canvas;

/// The Linux framebuffer LCD and evdev keyboards.
pub struct Lcd {
    pub fb: Framebuffer,
    pub keyboard: Keyboard,
}

impl Device for Lcd {
    fn size(&self) -> (i32, i32) {
        (self.fb.width, self.fb.height)
    }
    fn present(&mut self, canvas: &Canvas) -> Result<(), String> {
        self.fb.present(canvas).map_err(|err| err.to_string())
    }
    fn wait(&mut self, timeout_ms: i32) {
        self.keyboard.wait(timeout_ms);
    }
    fn read(&mut self, out: &mut Vec<KeyEvent>) {
        self.keyboard.read(out);
    }
    fn pointer_motion(&mut self) -> (i32, i32) {
        self.keyboard.pointer_motion()
    }
    fn menu_keys(&self) -> &'static str {
        "Fn+Esc"
    }
    fn help(&self) -> &'static [(&'static str, &'static str)] {
        if self.keyboard.cardputer() {
            &[
                ("Sym+F / Sym+X", "mouse up / down"),
                ("Sym+Z / Sym+C", "mouse left / right"),
                ("Sym+Enter", "left click"),
                ("Sym+Space", "right click"),
                ("Fn+Esc", "r86 menu"),
            ]
        } else {
            &[("Ctrl+Alt+M", "r86 menu")]
        }
    }
}

pub struct Framebuffer {
    file: File,
    pub width: i32,
    pub height: i32,
    stride: usize,
    bytes_per_pixel: usize,
    shadow: Vec<u8>,
    staged: Vec<u8>,
}

impl Framebuffer {
    /// Open `path`, or find the CardputerZero LCD: `/dev/fb_lcd` (udev link),
    /// then an ST7789 entry in /proc/fb, then `/dev/fb0`.
    pub fn open(path: Option<&Path>) -> Result<Self, String> {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => panel().unwrap_or_else(|| PathBuf::from("/dev/fb0")),
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|err| format!("Cannot open framebuffer {}: {err}", path.display()))?;
        let (width, height, stride, bits) = if file.metadata().map_err(|e| e.to_string())?.is_file()
        {
            file.set_len(320 * 170 * 2).map_err(|e| e.to_string())?;
            (320, 170, 640, 16)
        } else {
            geometry(&path)?
        };
        if !matches!(bits, 16 | 32) {
            return Err(format!(
                "{} uses {bits} bits per pixel; RGB565 or XRGB8888 is required",
                path.display()
            ));
        }
        let len = stride * height as usize;
        Ok(Self {
            file,
            width,
            height,
            stride,
            bytes_per_pixel: bits / 8,
            // Unknown panel contents: force the first present to write all rows.
            shadow: vec![0xa5; len],
            staged: vec![0; len],
        })
    }

    pub fn present(&mut self, canvas: &Canvas) -> io::Result<()> {
        let width = canvas.width.min(self.width) as usize;
        let row_bytes = width * self.bytes_per_pixel;
        for y in 0..canvas.height.min(self.height) as usize {
            let src = &canvas.pixels[y * canvas.width as usize..][..width];
            let dst = &mut self.staged[y * self.stride..][..row_bytes];
            if self.bytes_per_pixel == 2 {
                for (out, &p) in dst.chunks_exact_mut(2).zip(src) {
                    out.copy_from_slice(&p.to_le_bytes());
                }
            } else {
                for (out, &p) in dst.chunks_exact_mut(4).zip(src) {
                    let r = u32::from(p >> 11) * 255 / 31;
                    let g = u32::from((p >> 5) & 63) * 255 / 63;
                    let b = u32::from(p & 31) * 255 / 31;
                    out.copy_from_slice(&(r << 16 | g << 8 | b).to_le_bytes());
                }
            }
        }
        let rows = self.height as usize;
        let changed = |y: usize, a: &[u8], b: &[u8]| {
            a[y * self.stride..][..row_bytes] != b[y * self.stride..][..row_bytes]
        };
        let mut y = 0;
        while y < rows {
            if !changed(y, &self.staged, &self.shadow) {
                y += 1;
                continue;
            }
            let start = y;
            while y < rows && changed(y, &self.staged, &self.shadow) {
                y += 1;
            }
            let range = start * self.stride..y * self.stride;
            self.file
                .write_all_at(&self.staged[range.clone()], range.start as u64)?;
            self.shadow[range.clone()].copy_from_slice(&self.staged[range]);
        }
        Ok(())
    }
}

/// A dedicated small LCD: `/dev/fb_lcd` (udev link) or an ST7789 in /proc/fb.
pub fn panel() -> Option<PathBuf> {
    let link = PathBuf::from("/dev/fb_lcd");
    if link.exists() {
        return Some(link);
    }
    // /proc/fb lines look like "1 panel-mipi-dbid" or "0 fb_st7789v".
    if let Ok(text) = fs::read_to_string("/proc/fb") {
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(index), Some(name)) = (parts.next(), parts.next())
                && (name.contains("st7789") || name.contains("mipi-dbi"))
            {
                return Some(PathBuf::from(format!("/dev/fb{index}")));
            }
        }
    }
    None
}

/// Geometry from sysfs: (width, height, stride bytes, bits per pixel).
fn geometry(path: &Path) -> Result<(i32, i32, usize, usize), String> {
    let device = fs::canonicalize(path).map_err(|err| err.to_string())?;
    let name = device.file_name().ok_or("Invalid framebuffer path")?;
    let sysfs = Path::new("/sys/class/graphics").join(name);
    let read = |field: &str| {
        fs::read_to_string(sysfs.join(field))
            .map(|text| text.trim().to_string())
            .map_err(|err| format!("Cannot read {}: {err}", sysfs.join(field).display()))
    };
    let size = read("virtual_size")?;
    let (w, h) = size.split_once(',').ok_or("Invalid framebuffer size")?;
    let parse = |text: &str| {
        text.parse::<usize>()
            .map_err(|_| format!("Invalid value {text}"))
    };
    let (w, h) = (parse(w)?, parse(h)?);
    let bits = parse(&read("bits_per_pixel")?)?;
    let stride = match read("stride") {
        Ok(stride) => parse(&stride)?,
        Err(_) => w * bits / 8,
    };
    Ok((w as i32, h as i32, stride, bits))
}

/// The kernel's struct input_event: a timeval of two longs, then type, code
/// and value. This layout holds on 32- and 64-bit Linux.
#[repr(C)]
struct InputEvent {
    time: [libc::c_ulong; 2],
    kind: u16,
    code: u16,
    value: i32,
}

const EV_KEY: u16 = 1;
const EV_REP: u32 = 20;
/// Held keys repeat like a PC keyboard's typematic on devices without it.
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(80);

struct Input {
    file: File,
    /// The CardputerZero keyboard, translated for the PC guest.
    cardputer: Option<cardputer::Keys>,
    /// The kernel does not autorepeat this device's keys; r86 does.
    repeats: bool,
}

pub struct Keyboard {
    devices: Vec<Input>,
    /// The repeating key and when it next repeats.
    repeat: Option<(u16, Instant)>,
}

impl Keyboard {
    /// Whether a CardputerZero keyboard (with Sym mouse keys) is open.
    fn cardputer(&self) -> bool {
        self.devices.iter().any(|input| input.cardputer.is_some())
    }

    /// Open `paths`, or every input device the kernel lists with a keyboard
    /// handler. Devices are shared, not grabbed: the launcher watches ESC.
    pub fn open(paths: &[PathBuf]) -> Result<Self, String> {
        let paths = if paths.is_empty() {
            keyboard_devices()
        } else {
            paths.to_vec()
        };
        let devices: Vec<Input> = paths
            .iter()
            .filter_map(|path| {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                    .map_err(|err| eprintln!("r86: cannot open {}: {err}", path.display()))
                    .ok()?;
                // /dev/input/eventN describes itself under /sys/class/input/eventN/device.
                let sysfs = path
                    .file_name()
                    .map(|name| Path::new("/sys/class/input").join(name).join("device"));
                let read = |field: &str| {
                    sysfs
                        .as_ref()
                        .and_then(|dir| fs::read_to_string(dir.join(field)).ok())
                        .unwrap_or_default()
                };
                let ev = u32::from_str_radix(read("capabilities/ev").trim(), 16).unwrap_or(0);
                Some(Input {
                    file,
                    cardputer: (read("name").trim() == cardputer::NAME)
                        .then(cardputer::Keys::default),
                    repeats: sysfs.is_some() && ev & 1 << EV_REP == 0,
                })
            })
            .collect();
        if devices.is_empty() {
            return Err("No keyboard input device could be opened".into());
        }
        Ok(Self {
            devices,
            repeat: None,
        })
    }

    /// Sleep until input arrives, a signal is delivered, a held key repeats,
    /// or the timeout ends.
    pub fn wait(&self, timeout_ms: i32) {
        let mut timeout = timeout_ms;
        if let Some((_, at)) = self.repeat {
            let until = at.saturating_duration_since(Instant::now()).as_millis();
            timeout = timeout.min(until as i32);
        }
        if self.moving() {
            timeout = timeout.min(10);
        }
        let mut fds: Vec<libc::pollfd> = self
            .devices
            .iter()
            .map(|input| libc::pollfd {
                fd: input.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid, exclusively borrowed pollfd array.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    }

    /// Append pending key events without blocking.
    pub fn read(&mut self, out: &mut Vec<KeyEvent>) {
        const SIZE: usize = std::mem::size_of::<InputEvent>();
        let mut buffer = [0u8; SIZE * 32];
        for input in &mut self.devices {
            let first = out.len();
            loop {
                let len = match input.file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(len) => len,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                for chunk in buffer[..len - len % SIZE].chunks_exact(SIZE) {
                    // SAFETY: the kernel writes whole input_event records and
                    // every bit pattern is a valid InputEvent.
                    let event: InputEvent =
                        unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                    match &mut input.cardputer {
                        Some(keys) => keys.event(event.kind, event.code, event.value, out),
                        None if event.kind == EV_KEY => out.push(KeyEvent {
                            code: event.code,
                            value: event.value,
                        }),
                        None => {}
                    }
                }
            }
            if input.repeats {
                for event in &out[first..] {
                    match event.value {
                        1 if repeats(event.code) => {
                            self.repeat = Some((event.code, Instant::now() + REPEAT_DELAY));
                        }
                        0 if self.repeat.is_some_and(|(code, _)| code == event.code) => {
                            self.repeat = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        let now = Instant::now();
        if let Some((code, at)) = self.repeat
            && now >= at
        {
            out.push(KeyEvent { code, value: 2 });
            self.repeat = Some((code, now + REPEAT_INTERVAL));
        }
    }

    fn moving(&self) -> bool {
        self.devices.iter().any(|input| {
            input
                .cardputer
                .as_ref()
                .is_some_and(cardputer::Keys::moving)
        })
    }

    /// Pointer motion from keyboard mouse keys since the last call.
    pub fn pointer_motion(&mut self) -> (i32, i32) {
        self.devices
            .iter_mut()
            .filter_map(|input| input.cardputer.as_mut())
            .map(cardputer::Keys::pointer_motion)
            .fold((0, 0), |(x, y), (dx, dy)| (x + dx, y + dy))
    }
}

/// Modifiers, mouse buttons and r86's own keys do not repeat.
fn repeats(code: u16) -> bool {
    !matches!(
        code,
        keymap::LEFT_SHIFT
            | keymap::RIGHT_SHIFT
            | keymap::LEFT_CTRL
            | keymap::RIGHT_CTRL
            | keymap::LEFT_ALT
            | keymap::RIGHT_ALT
            | keymap::FN
            | keymap::SYM
            | keymap::BTN_LEFT
            | keymap::BTN_RIGHT
            | keymap::MENU
            | keymap::OSK
    )
}

fn keyboard_devices() -> Vec<PathBuf> {
    // Blocks of "N: Name=...", "H: Handlers=sysrq kbd event0 leds", ...
    let text = fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.strip_prefix("H: Handlers="))
        .filter(|handlers| handlers.split_whitespace().any(|h| h == "kbd"))
        .filter_map(|handlers| handlers.split_whitespace().find(|h| h.starts_with("event")))
        .map(|event| Path::new("/dev/input").join(event))
        .collect()
}
