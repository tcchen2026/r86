//! r86-handheld: VM library, new-VM wizard and VM display for Linux handhelds:
//! a small framebuffer LCD with a keyboard (the M5Stack CardputerZero, 320x170
//! ST7789), or any screen the firmware's SDL2 drives, with a game controller
//! (TrimUI). See ports/ for packaging.
#[path = "../catalog.rs"]
mod catalog;
#[path = "../disk_files.rs"]
mod disk_files;
#[path = "../guest_os.rs"]
mod guest_os;
#[allow(dead_code)] // Shared with r86-run; not every helper is used here.
#[path = "../keys.rs"]
mod keys;
#[path = "../media.rs"]
mod media;
#[allow(dead_code)] // Shared with r86-run's command line.
#[path = "../protocol.rs"]
mod protocol;
#[path = "../vm_store.rs"]
mod vm_store;
#[path = "../ws.rs"]
mod ws;
#[allow(dead_code)] // r86-run's server loop and debugger jobs are unused here.
#[path = "../ws_frontend.rs"]
mod ws_frontend;

mod cardputer;
mod device;
mod keymap;
mod lcd;
mod osk;
mod screens;
mod sdl;
mod session;
mod text80;
mod ui;
mod vgafont;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use device::{Device, stop_requested};
use lcd::{Framebuffer, Keyboard};
use screens::{Ctx, Home, Message, Outcome, Stack};
use session::End;

const USAGE: &str =
    "usage: r86-handheld [--fb PATH] [--input PATH]... [--library DIR] [--share DIR]
                    [--cache DIR] [--home DIR]

--fb       framebuffer device; a regular file acts as a 320x170 RGB565 panel for
           host testing (default: /dev/fb_lcd or an ST7789 in /proc/fb, else
           full screen through the system's libSDL2, else /dev/fb0)
--input    evdev device, repeatable (default: every keyboard in /proc/bus/input/devices)
--library  VM folder (default: ~/r86-VMs)
--share    firmware and bundled images (default: beside the executable or ../share/r86)
--cache    downloads (default: $XDG_CACHE_HOME/r86 or ~/.cache/r86)
--home     where file choosers start (default: $HOME)

The handheld-sdl build shows a desktop window instead of --fb/--input and
keeps its library, cache and share folders under ./tmp/handheld.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("r86-handheld: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    fb: Option<PathBuf>,
    inputs: Vec<PathBuf>,
    library: Option<PathBuf>,
    share: Option<PathBuf>,
    cache: Option<PathBuf>,
    home: Option<PathBuf>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        fb: None,
        inputs: Vec::new(),
        library: None,
        share: None,
        cache: None,
        home: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .map(PathBuf::from)
                .ok_or(format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--fb" => options.fb = Some(value()?),
            "--input" => options.inputs.push(value()?),
            "--library" => options.library = Some(value()?),
            "--share" => options.share = Some(value()?),
            "--cache" => options.cache = Some(value()?),
            "--home" => options.home = Some(value()?),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(options)
}

/// Offer at most half of host RAM to a guest.
fn memory_limit() -> u32 {
    let total_kib = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .unwrap_or(1 << 20);
    ((total_kib / 1024 / 2) as u32).max(16)
}

/// The desktop simulator keeps everything under ./tmp/handheld.
const SIMULATOR: bool = cfg!(feature = "handheld-sdl");

fn open_device(options: &Options) -> Result<Box<dyn Device>, String> {
    if SIMULATOR {
        return Ok(Box::new(sdl::Window::simulator(3)?));
    }
    let lcd = || -> Result<Box<dyn Device>, String> {
        Ok(Box::new(lcd::Lcd {
            fb: Framebuffer::open(options.fb.as_deref())?,
            keyboard: Keyboard::open(&options.inputs)?,
        }))
    };
    // A dedicated panel (the CardputerZero's LCD) wins over SDL, which would
    // pick another display such as HDMI.
    if options.fb.is_some() || lcd::panel().is_some() {
        return lcd();
    }
    match sdl::Window::full_screen() {
        Ok(window) => Ok(Box::new(window)),
        Err(sdl) => lcd().map_err(|fb| format!("{sdl}; {fb}")),
    }
}

fn run() -> Result<(), String> {
    let options = options()?;
    let data = PathBuf::from("tmp/handheld");
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    let home = match (&options.home, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => std::env::current_dir().map_err(|err| err.to_string())?,
        (None, false) => user_home.clone().ok_or("HOME is not set")?,
    };
    let library = match (&options.library, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => data.join("r86-VMs"),
        (None, false) => vm_store::default_root()?,
    };
    let share = match (&options.share, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => data.join("share"),
        (None, false) => vm_store::firmware_dir()?,
    };
    let cache = match (&options.cache, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => data.join("cache"),
        (None, false) => std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| user_home.map(|home| home.join(".cache")))
            .ok_or("HOME is not set")?
            .join("r86"),
    };
    device::install_stop_signals();
    let mut device = open_device(&options)?;
    let (width, height) = device.size();
    let mut canvas = ui::Canvas::new(width, height);
    let ctx = Ctx {
        library,
        share,
        cache,
        home,
        memory_limit: memory_limit(),
        menu_keys: device.menu_keys(),
        help: device.help(),
    };
    let mut stack = Stack::new(Box::new(Home::new(&ctx)));
    let mut events = Vec::new();
    let mut shift = false;
    let mut dirty = true;
    while !stop_requested() {
        if dirty || stack.animated() {
            stack.draw(&mut canvas, &ctx);
            device.present(&canvas)?;
            dirty = false;
        }
        device.wait(if stack.animated() { 100 } else { 1000 });
        let mut outcome = stack.tick(&ctx);
        dirty |= stack.take_changed();
        device.read(&mut events);
        for event in events.drain(..) {
            if matches!(event.code, keymap::LEFT_SHIFT | keymap::RIGHT_SHIFT) {
                shift = event.value != 0;
            }
            if outcome.is_some() || event.value == 0 {
                continue;
            }
            if let Some(key) = keymap::ui_key(event.code, shift) {
                dirty = true;
                outcome = stack.key(key, &ctx);
            }
        }
        match outcome {
            Some(Outcome::Exit) => break,
            Some(Outcome::Start(vm, remote)) => {
                let name = vm.name.clone();
                let end = session::run(*vm, &ctx, device.as_mut(), &mut canvas, remote);
                // Keys pressed for the guest must not act on the library.
                device.read(&mut events);
                events.clear();
                shift = false;
                stack.home(&ctx);
                match end {
                    Ok(End::Signal) => break,
                    Ok(End::PoweredOff | End::TurnedOff) => {}
                    Err(err) => {
                        eprintln!("r86-handheld: {name}: {err}");
                        stack.push(Box::new(Message {
                            title: format!("{name} stopped"),
                            text: err,
                        }));
                    }
                }
                dirty = true;
            }
            Some(Outcome::Action(_) | Outcome::Closed) | None => {}
        }
    }
    // Leave a blank panel; the launcher redraws its home screen.
    canvas.fill(ui::BLACK);
    device.present(&canvas)
}
