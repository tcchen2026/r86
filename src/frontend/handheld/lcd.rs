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
use super::sdl;
use super::ui::{self, Canvas, Font, Rect};

const HDMI_STATUS: &str = "/sys/class/drm/card0-HDMI-A-1/status";

/// The Linux framebuffer LCD and evdev keyboards.
pub struct Lcd {
    fb: Framebuffer,
    keyboard: Keyboard,
    connector: Option<PathBuf>,
    hdmi: Option<sdl::Window>,
    notice: Option<Canvas>,
    next_hdmi_check: Instant,
    disconnected: bool,
}

impl Lcd {
    pub fn new(fb: Framebuffer, keyboard: Keyboard) -> Self {
        Self {
            fb,
            keyboard,
            connector: (panel().is_some() && Path::new(HDMI_STATUS).exists())
                .then(|| PathBuf::from(HDMI_STATUS)),
            hdmi: None,
            notice: None,
            next_hdmi_check: Instant::now(),
            disconnected: false,
        }
    }

    fn connected(&self) -> bool {
        self.connector
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .is_some_and(|status| status.trim() == "connected")
    }

    fn disconnect(&mut self) {
        self.hdmi = None;
        self.notice = None;
        self.disconnected = true;
    }

    fn display_result(&mut self, result: Result<(), String>) {
        if let Err(err) = result {
            eprintln!("r86: HDMI: {err}");
            self.disconnect();
        }
    }

    fn present_notice(&mut self) -> Result<(), String> {
        if let Some(notice) = &self.notice {
            self.fb.present(notice).map_err(|err| err.to_string())?;
        }
        Ok(())
    }
}

impl Device for Lcd {
    fn size(&self) -> (i32, i32) {
        (self.fb.width, self.fb.height)
    }
    fn present(&mut self, canvas: &Canvas) -> Result<(), String> {
        if let Some(hdmi) = &mut self.hdmi {
            let result = hdmi.present(canvas);
            self.display_result(result);
        }
        self.fb.present(canvas).map_err(|err| err.to_string())
    }
    fn present_vm(&mut self, canvas: &Canvas) -> Result<(), String> {
        if let Some(hdmi) = &mut self.hdmi {
            let result = hdmi.present(canvas);
            self.display_result(result);
            self.present_notice()
        } else {
            self.fb.present(canvas).map_err(|err| err.to_string())
        }
    }
    fn guest_layer(&self) -> bool {
        self.hdmi.is_some()
    }
    fn present_frame(
        &mut self,
        rgba: &[u8],
        size: (i32, i32),
        _stretch: bool,
        view: Rect,
        ui: &Canvas,
        ui_area: Option<Rect>,
    ) -> Result<(), String> {
        let result = self
            .hdmi
            .as_mut()
            .expect("HDMI guest layer")
            .present_frame(rgba, size, false, view, ui, ui_area);
        self.display_result(result);
        self.present_notice()
    }
    fn hdmi(&self) -> Option<bool> {
        self.connector.as_ref().map(|_| self.hdmi.is_some())
    }
    fn set_hdmi(&mut self, enabled: bool, vm_name: &str) -> Result<(), String> {
        if !enabled {
            self.hdmi = None;
            self.notice = None;
        } else if self.hdmi.is_none() {
            if !self.connected() {
                return Err("No HDMI display".into());
            }
            let window = sdl::Window::external()?;
            let mut notice = Canvas::new(self.fb.width, self.fb.height);
            notice.fill(ui::BG);
            ui::header(&mut notice, &format!("r86: {vm_name}"), "");
            let y = (notice.height - Font::Normal.height()) / 2;
            notice.text_centered(y, "Showing on HDMI", ui::TEXT, Font::Normal);
            ui::footer(&mut notice, "Fn+Esc: r86 menu");
            self.notice = Some(notice);
            self.hdmi = Some(window);
            self.next_hdmi_check = Instant::now() + Duration::from_secs(1);
        }
        self.disconnected = false;
        Ok(())
    }
    fn take_display_disconnect(&mut self) -> bool {
        std::mem::take(&mut self.disconnected)
    }
    fn wait(&mut self, timeout_ms: i32) {
        // evdev is the keyboard source; poll SDL's desktop mouse promptly too.
        self.keyboard.wait(if self.hdmi.is_some() {
            timeout_ms.min(16)
        } else {
            timeout_ms
        });
    }
    fn read(&mut self, out: &mut Vec<KeyEvent>) {
        self.keyboard.read(out);
        if let Some(hdmi) = &mut self.hdmi {
            hdmi.read(out);
            if hdmi.closed() {
                self.disconnect();
            }
        }
        let now = Instant::now();
        if self.hdmi.is_some() && now >= self.next_hdmi_check {
            self.next_hdmi_check = now + Duration::from_secs(1);
            if !self.connected() {
                self.disconnect();
            }
        }
    }
    fn pointer_motion(&mut self) -> (i32, i32) {
        let (x, y) = self.keyboard.pointer_motion();
        let (dx, dy) = self.hdmi.as_mut().map_or((0, 0), Device::pointer_motion);
        (x + dx, y + dy)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run alone on a disposable desktop (e.g. Xvfb). Uses a file framebuffer
    /// and connector status; the window, renderer and event queue are SDL's.
    #[test]
    #[ignore = "requires SDL2 and a Wayland or X11 desktop"]
    fn hdmi_desktop_lifecycle() {
        #[repr(C, align(8))]
        struct Event([u8; 64]);
        // SAFETY: SDL2's event ABI is a 56-byte, 8-byte-aligned union.
        // Keep the library open until the last event and window are gone.
        let lib = unsafe { libc::dlopen(c"libSDL2-2.0.so.0".as_ptr(), libc::RTLD_NOW) };
        assert!(!lib.is_null());
        let push: unsafe extern "C" fn(*mut Event) -> i32 = unsafe {
            let symbol = libc::dlsym(lib, c"SDL_PushEvent".as_ptr());
            assert!(!symbol.is_null());
            std::mem::transmute(symbol)
        };
        let send = |kind: u32, fields: &[(usize, i32)]| {
            let mut event = Event([0; 64]);
            event.0[..4].copy_from_slice(&kind.to_ne_bytes());
            for &(offset, value) in fields {
                event.0[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
            }
            assert_eq!(unsafe { push(&mut event) }, 1);
        };
        let dir = std::env::temp_dir().join(format!("r86-hdmi-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let fb = dir.join("fb");
        let input = dir.join("input");
        let status = dir.join("status");
        fs::write(&fb, []).unwrap();
        fs::write(&input, []).unwrap();
        fs::write(&status, "disconnected\n").unwrap();
        let mut lcd = Lcd::new(
            Framebuffer::open(Some(&fb)).unwrap(),
            Keyboard::open(&[input]).unwrap(),
        );
        lcd.connector = Some(status.clone());
        assert_eq!(lcd.set_hdmi(true, "Test VM"), Err("No HDMI display".into()));
        assert!(!lcd.guest_layer());
        fs::write(&status, "connected\n").unwrap();
        lcd.set_hdmi(true, "Test VM").unwrap();
        assert_eq!(lcd.size(), (320, 170));
        assert!(lcd.guest_layer());
        let mut canvas = Canvas::new(320, 170);
        let view = Rect {
            x: 0,
            y: 0,
            w: 320,
            h: 170,
        };
        let mut guest = [255, 0, 0, 255].repeat(640 * 480);
        lcd.present_frame(&guest, (640, 480), true, view, &canvas, None)
            .unwrap();
        let notice = fs::read(&fb).unwrap();
        guest.copy_from_slice(&[0, 255, 0, 255].repeat(640 * 480));
        lcd.present_frame(&guest, (640, 480), false, view, &canvas, None)
            .unwrap();
        assert_eq!(fs::read(&fb).unwrap(), notice, "guest must not reach LCD");
        canvas.fill(ui::PANEL);
        lcd.present(&canvas).unwrap();
        let menu: Vec<u8> = canvas.pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
        assert_eq!(fs::read(&fb).unwrap(), menu, "menus must reach LCD");
        lcd.present_vm(&canvas).unwrap();
        assert_eq!(fs::read(&fb).unwrap(), notice, "resume restores the notice");

        let mut events = Vec::new();
        lcd.read(&mut events);
        events.clear();
        lcd.pointer_motion();
        send(0x300, &[(16, 4)]); // SDL A down/up: evdev owns the keyboard.
        send(0x301, &[(16, 4)]);
        send(0x651, &[(12, 1)]); // Controller A also must be ignored.
        send(0x400, &[(28, 60), (32, -40)]);
        send(0x401, &[(16, 1)]); // Left mouse down/up.
        send(0x402, &[(16, 1)]);
        lcd.read(&mut events);
        assert_eq!(
            events.iter().map(|e| (e.code, e.value)).collect::<Vec<_>>(),
            [(keymap::BTN_LEFT, 1), (keymap::BTN_LEFT, 0)]
        );
        let (dx, dy) = lcd.pointer_motion();
        assert!(dx > 0 && dy < 0, "desktop mouse motion: {dx},{dy}");

        fs::write(&status, "disconnected\n").unwrap();
        lcd.next_hdmi_check = Instant::now();
        lcd.read(&mut Vec::new());
        assert_eq!(lcd.hdmi(), Some(false));
        assert!(lcd.take_display_disconnect());
        assert!(!lcd.take_display_disconnect());
        lcd.present_vm(&canvas).unwrap();
        assert_eq!(fs::read(&fb).unwrap(), menu, "LCD resumes drawing");
        fs::write(&status, "connected\n").unwrap();
        for (kind, fields) in [(0x200, vec![(12, 14)]), (0x100, vec![])] {
            lcd.set_hdmi(true, "Test VM").unwrap();
            send(kind, &fields); // Window close, then SDL compositor-loss quit.
            lcd.read(&mut Vec::new());
            assert_eq!(lcd.hdmi(), Some(false));
            assert!(lcd.take_display_disconnect());
            assert!(!super::super::device::stop_requested());
        }
        lcd.set_hdmi(true, "Test VM").unwrap();
        lcd.set_hdmi(false, "Test VM").unwrap();
        assert_eq!(lcd.hdmi(), Some(false));
        assert!(!lcd.take_display_disconnect());
        drop(lcd);
        unsafe { libc::dlclose(lib) };
        fs::remove_dir_all(dir).unwrap();
    }
}
