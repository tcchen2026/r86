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

const USAGE: &str = "usage: r86-handheld [--fb PATH] [--input PATH]... [--data DIR] [--share DIR]

--fb       framebuffer device; a regular file acts as a 320x170 RGB565 panel for
           host testing (default: /dev/fb_lcd or an ST7789 in /proc/fb, else
           full screen through the system's libSDL2, else /dev/fb0)
--input    evdev device, repeatable (default: every keyboard in /proc/bus/input/devices)
--data     r86's folder (default: ~/r86): VMs/ holds the VMs; the disk and CD
           choosers start in vhd/ and iso/; catalog downloads go to iso/
--share    firmware and bundled images (default: beside the executable or ../share/r86)

The handheld-sdl build shows a desktop window instead of --fb/--input and
keeps its data and share folders under ./tmp/handheld.";

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
    data: Option<PathBuf>,
    share: Option<PathBuf>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        fb: None,
        inputs: Vec::new(),
        data: None,
        share: None,
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
            "--data" => options.data = Some(value()?),
            "--share" => options.share = Some(value()?),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(options)
}

/// Offer a guest host RAM less 128 MiB for the host system and r86 itself
/// (192 MiB on the CardputerZero's 352 MiB).
fn memory_limit() -> u32 {
    let total_kib = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .unwrap_or(1 << 20);
    ((total_kib / 1024).saturating_sub(128) as u32).max(16)
}

/// The desktop simulator keeps everything under ./tmp/handheld.
const SIMULATOR: bool = cfg!(feature = "handheld-sdl");

fn open_device(options: &Options) -> Result<Box<dyn Device>, String> {
    if SIMULATOR {
        return Ok(Box::new(sdl::Window::simulator(3)?));
    }
    let lcd = || -> Result<Box<dyn Device>, String> {
        let lcd = lcd::Lcd::new(
            Framebuffer::open(options.fb.as_deref())?,
            Keyboard::open(&options.inputs)?,
        );
        if lcd.hdmi().is_some()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
            && let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
            && let Some(socket) = sdl::wayland_socket(std::path::Path::new(&runtime))
        {
            // SAFETY: device discovery runs at startup, before SDL or any
            // download/remote threads. libwayland needs this environment key;
            // the SDL video driver itself is selected later with SDL_SetHint.
            unsafe { std::env::set_var("WAYLAND_DISPLAY", socket) };
        }
        Ok(Box::new(lcd))
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
    let simulator = PathBuf::from("tmp/handheld");
    let data = match (&options.data, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => simulator.clone(),
        (None, false) => {
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join("r86")
        }
    };
    let share = match (&options.share, SIMULATOR) {
        (Some(dir), _) => dir.clone(),
        (None, true) => simulator.join("share"),
        (None, false) => vm_store::firmware_dir()?,
    };
    // vm.toml reads relative paths against the VM's folder, so the disks and
    // discs it stores from these folders need absolute paths.
    let absolute = |dir: PathBuf| {
        std::path::absolute(&dir).map_err(|err| format!("Cannot resolve {}: {err}", dir.display()))
    };
    let (data, share) = (absolute(data)?, absolute(share)?);
    let (library, vhd, iso) = (data.join("VMs"), data.join("vhd"), data.join("iso"));
    for dir in [&library, &vhd, &iso] {
        fs::create_dir_all(dir).map_err(|err| format!("Cannot create {}: {err}", dir.display()))?;
    }
    device::install_stop_signals();
    let mut device = open_device(&options)?;
    let (width, height) = device.size();
    let mut canvas = ui::Canvas::new(width, height);
    let ctx = Ctx {
        library,
        share,
        vhd,
        iso,
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
