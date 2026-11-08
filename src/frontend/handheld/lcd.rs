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

use super::device::{Device, KeyEvent};
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
    fn menu_keys(&self) -> &'static str {
        "Fn+Sym"
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

pub struct Keyboard {
    devices: Vec<File>,
}

impl Keyboard {
    /// Open `paths`, or every input device the kernel lists with a keyboard
    /// handler. Devices are shared, not grabbed: the launcher watches ESC.
    pub fn open(paths: &[PathBuf]) -> Result<Self, String> {
        let paths = if paths.is_empty() {
            keyboard_devices()
        } else {
            paths.to_vec()
        };
        let devices: Vec<File> = paths
            .iter()
            .filter_map(|path| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                    .map_err(|err| eprintln!("r86: cannot open {}: {err}", path.display()))
                    .ok()
            })
            .collect();
        if devices.is_empty() {
            return Err("No keyboard input device could be opened".into());
        }
        Ok(Self { devices })
    }

    /// Sleep until input arrives, a signal is delivered, or the timeout ends.
    pub fn wait(&self, timeout_ms: i32) {
        let mut fds: Vec<libc::pollfd> = self
            .devices
            .iter()
            .map(|file| libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid, exclusively borrowed pollfd array.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    }

    /// Append pending key events without blocking.
    pub fn read(&mut self, out: &mut Vec<KeyEvent>) {
        const SIZE: usize = std::mem::size_of::<InputEvent>();
        let mut buffer = [0u8; SIZE * 32];
        for device in &mut self.devices {
            loop {
                let len = match device.read(&mut buffer) {
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
                    if event.kind == EV_KEY {
                        out.push(KeyEvent {
                            code: event.code,
                            value: event.value,
                        });
                    }
                }
            }
        }
    }
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
