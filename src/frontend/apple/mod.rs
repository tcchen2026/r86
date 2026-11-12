//! C API for the r86 Mac app (apple/). The authoritative declarations are in
//! apple/r86/Bridge/r86.h; keep both in step.
//!
//! - Library calls take the library root ($HOME/r86/VMs) or a VM directory
//!   and return NULL on success or an error string. Lists are JSON.
//! - A running VM is an opaque `R86Vm` owning one core thread. Any thread may
//!   call the r86_vm_* functions; the audio callback only reads audio.
//! - Strings are UTF-8. Every returned string is freed with r86_string_free.

#[path = "../guest_os.rs"]
#[allow(dead_code)] // Shared with the other frontends.
mod guest_os;
mod keymap;
#[path = "../keys.rs"]
#[allow(dead_code)]
mod keys;
mod runner;
#[path = "../vm_store.rs"]
#[allow(dead_code)]
mod vm_store;

use std::ffi::{CStr, CString, c_char};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use r86::devices::Ps2ButtonState;
use runner::{Command, Running};
use vm_store::Vm;

pub struct R86Vm(Running);

#[repr(C)]
pub struct R86Status {
    /// 0 running, 1 paused, 2 off, 3 failed.
    pub state: u32,
    pub instructions_per_second: u64,
}

#[repr(C)]
pub struct R86Frame {
    pub width: u32,
    pub height: u32,
    pub serial: u64,
}

/// # Safety
/// `s` is NULL or a valid NUL-terminated string.
unsafe fn text(s: *const c_char) -> Option<String> {
    // SAFETY: the caller passes NULL or a valid C string.
    (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

fn out(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " "))
        .expect("NUL bytes were replaced")
        .into_raw()
}

fn result(r: Result<(), String>) -> *mut c_char {
    r.err().map_or(std::ptr::null_mut(), out)
}

fn json(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('"');
    for c in s.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(quoted, "\\u{:04x}", c as u32);
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn path_json(path: Option<&Path>) -> String {
    path.map_or("null".into(), |p| json(&p.to_string_lossy()))
}

fn vm_json(vm: &Vm) -> String {
    format!(
        concat!(
            "{{\"directory\":{},\"name\":{},\"os\":{},\"memoryMiB\":{},\"disk\":{},",
            "\"diskMiB\":{},\"iso\":{},\"cdrom\":{},\"bootOrder\":{},\"scale\":{},",
            "\"volume\":{},\"network\":{},\"notes\":{},\"engine\":{}}}"
        ),
        json(&vm.directory.to_string_lossy()),
        json(&vm.name),
        json(&vm.os),
        vm.memory,
        json(&vm.disk.to_string_lossy()),
        vm.disk_mib,
        path_json(vm.iso.as_deref()),
        vm.cdrom,
        json(&vm.boot_order),
        json(&vm.scale),
        vm.volume,
        vm.network,
        json(&vm.notes),
        json(if vm.engine == "interp" {
            "interpreter"
        } else {
            &vm.engine
        }),
    )
}

/// The VMs under `root` as a JSON array; unreadable ones are
/// `{"directory": ..., "error": ...}`.
///
/// # Safety
/// `root` is a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_library_list(root: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let root = PathBuf::from(unsafe { text(root) }.unwrap_or_default());
    let items: Vec<String> = vm_store::list(&root)
        .iter()
        .map(|vm| match vm {
            Ok(vm) => vm_json(vm),
            Err(err) => format!("{{\"error\":{}}}", json(err)),
        })
        .collect();
    out(format!("[{}]", items.join(",")))
}

/// OS presets as a JSON array of {name, memoryMiB, diskMiB}.
#[unsafe(no_mangle)]
pub extern "C" fn r86_presets() -> *mut c_char {
    let items: Vec<String> = guest_os::PRESETS
        .iter()
        .map(|p| {
            format!(
                "{{\"name\":{},\"memoryMiB\":{},\"diskMiB\":{}}}",
                json(p.name),
                p.memory_mib,
                p.disk_mib
            )
        })
        .collect();
    out(format!("[{}]", items.join(",")))
}

/// The preset name an ISO image looks like, or "" if unknown.
///
/// # Safety
/// `path` is a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_detect_iso(path: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let path = PathBuf::from(unsafe { text(path) }.unwrap_or_default());
    let name = guest_os::detect_iso(&path)
        .ok()
        .flatten()
        .map_or("", |p| p.name);
    out(name.into())
}

/// `base`, or `base 2`, ... if that name is taken in `root`.
///
/// # Safety
/// Both arguments are valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_library_suggest_name(
    root: *const c_char,
    base: *const c_char,
) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let (root, base) = unsafe { (text(root), text(base)) };
    out(vm_store::suggested_name(
        Path::new(&root.unwrap_or_default()),
        &base.unwrap_or_default(),
    ))
}

/// Create `root/name` with a new dynamic VHD and an optional CD image.
///
/// # Safety
/// `root`, `name` and `os` are valid C strings; `iso` is NULL or one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_library_create(
    root: *const c_char,
    name: *const c_char,
    os: *const c_char,
    memory_mib: u32,
    disk_mib: u64,
    iso: *const c_char,
) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let (root, name, os, iso) = unsafe { (text(root), text(name), text(os), text(iso)) };
    let preset = guest_os::by_name(&os.unwrap_or_default());
    let mut vm = Vm::new(
        Path::new(&root.unwrap_or_default()),
        name.unwrap_or_default(),
        preset,
    );
    vm.engine = "auto".into();
    vm.memory = memory_mib;
    vm.disk_mib = disk_mib;
    vm.iso = iso.filter(|p| !p.is_empty()).map(PathBuf::from);
    if vm.iso.is_some() {
        vm.boot_order = "cdrom".into();
    }
    result(vm.create_blank())
}

/// Change one setting of a stopped VM. Keys and values as in vm.toml:
/// os, memory-mib, boot-order, iso ("" ejects), cdrom, scale, volume,
/// network, notes, cpu-engine (auto | jit | interpreter; legacy interp accepted).
///
/// # Safety
/// All arguments are valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_library_set(
    directory: *const c_char,
    key: *const c_char,
    value: *const c_char,
) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let (directory, key, value) = unsafe { (text(directory), text(key), text(value)) };
    let (key, value) = (key.unwrap_or_default(), value.unwrap_or_default());
    result(
        Vm::load(PathBuf::from(directory.unwrap_or_default())).and_then(|mut vm| {
            let number = || value.parse::<u32>().map_err(|_| format!("Invalid {key}"));
            let flag = || value.parse::<bool>().map_err(|_| format!("Invalid {key}"));
            match key.as_str() {
                "os" => vm.os = guest_os::by_name(&value).name.into(),
                "memory-mib" => vm.memory = number()?,
                "boot-order" => vm.boot_order = value,
                "iso" => vm.iso = (!value.is_empty()).then(|| PathBuf::from(&value)),
                "cdrom" => vm.cdrom = flag()?,
                "scale" => vm.scale = value,
                "volume" => vm.volume = number()?.min(100) as u8,
                "network" => vm.network = flag()?,
                "notes" => vm.notes = value,
                "cpu-engine" => vm.engine = value,
                _ => return Err(format!("Unknown setting {key}")),
            }
            if vm.iso.is_some() {
                vm.cdrom = true;
            }
            vm.save()
        }),
    )
}

/// Delete a stopped VM's directory, including a disk stored inside it.
///
/// # Safety
/// `directory` is a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_library_delete(directory: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let directory = PathBuf::from(unsafe { text(directory) }.unwrap_or_default());
    result(Vm::load(directory).and_then(|vm| vm.delete()))
}

/// # Safety
/// `s` is NULL or a string returned by this library, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: `s` came from CString::into_raw in this library.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Start the VM in `directory` with ROMs from `firmware`. On failure returns
/// NULL and stores an error string in `*error`.
///
/// # Safety
/// `directory` and `firmware` are valid C strings; `error` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_start(
    directory: *const c_char,
    firmware: *const c_char,
    error: *mut *mut c_char,
) -> *mut R86Vm {
    // SAFETY: forwarded caller contract.
    let (directory, firmware) = unsafe { (text(directory), text(firmware)) };
    let started = Vm::load(PathBuf::from(directory.unwrap_or_default()))
        .and_then(|vm| Running::start(vm, PathBuf::from(firmware.unwrap_or_default())));
    match started {
        Ok(running) => Box::into_raw(Box::new(R86Vm(running))),
        Err(err) => {
            // SAFETY: the caller passes a writable pointer.
            unsafe { *error = out(err) };
            std::ptr::null_mut()
        }
    }
}

/// Turn the VM off now (no guest shutdown), flush its disks and free it.
///
/// # Safety
/// `vm` came from r86_vm_start and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_free(vm: *mut R86Vm) {
    // SAFETY: caller contract; dropping stops and joins the core thread.
    drop(unsafe { Box::from_raw(vm) });
}

/// 0 pause, 1 resume, 2 reset, 3 power button, 4 Ctrl+Alt+Del,
/// 5 release held keys and buttons.
///
/// # Safety
/// `vm` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_command(vm: *const R86Vm, command: u32) {
    let command = match command {
        0 => Command::Pause(true),
        1 => Command::Pause(false),
        2 => Command::Reset,
        3 => Command::PowerButton,
        4 => Command::CtrlAltDel,
        5 => Command::ReleaseInput,
        _ => return,
    };
    // SAFETY: caller contract.
    unsafe { &*vm }.0.send(command);
}

/// A macOS virtual key code (kVK_*) pressed or released.
///
/// # Safety
/// `vm` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_key(vm: *const R86Vm, mac_key_code: u16, down: bool) {
    if let Some(key) = keymap::mac_key(mac_key_code) {
        // SAFETY: caller contract.
        unsafe { &*vm }.0.send(Command::Key(key, down));
    }
}

/// Relative motion in guest pixels (y grows downwards) and buttons:
/// bit 0 left, bit 1 right, bit 2 middle.
///
/// # Safety
/// `vm` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_mouse(vm: *const R86Vm, dx: i32, dy: i32, buttons: u32) {
    let buttons = Ps2ButtonState {
        left: buttons & 1 != 0,
        right: buttons & 2 != 0,
        middle: buttons & 4 != 0,
    };
    // SAFETY: caller contract.
    unsafe { &*vm }.0.send(Command::Mouse(dx, dy, buttons));
}

/// Insert a CD image, or eject with NULL. The outcome arrives as a message.
///
/// # Safety
/// `vm` is a live handle; `path` is NULL or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_change_cd(vm: *const R86Vm, path: *const c_char) {
    // SAFETY: caller contract.
    let path = unsafe { text(path) }.map(PathBuf::from);
    unsafe { &*vm }.0.send(Command::Cd(path));
}

/// Playback level in percent; 0 mutes and stops the sound mixer. Saved.
///
/// # Safety
/// `vm` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_set_volume(vm: *const R86Vm, percent: u32) {
    // SAFETY: caller contract.
    unsafe { &*vm }
        .0
        .send(Command::Volume(percent.min(100) as u8));
}

/// # Safety
/// `vm` is a live handle; `status` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_status(vm: *const R86Vm, status: *mut R86Status) {
    // SAFETY: caller contract.
    let shared = &unsafe { &*vm }.0.shared;
    let s = runner::lock(&shared.status);
    // SAFETY: caller contract.
    unsafe {
        *status = R86Status {
            state: s.state as u32,
            instructions_per_second: s.ips,
        }
    };
}

/// The next message for the user (CD changes, errors), or NULL.
///
/// # Safety
/// `vm` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_take_message(vm: *const R86Vm) -> *mut c_char {
    // SAFETY: caller contract.
    let shared = &unsafe { &*vm }.0.shared;
    runner::lock(&shared.status)
        .messages
        .pop_front()
        .map_or(std::ptr::null_mut(), out)
}

/// Copy the latest frame (RGBA, `width * 4` bytes per row) into `pixels` if
/// its serial differs from `have_serial`. Always fills `frame`; returns true
/// only when it copied, which needs `capacity >= width * height * 4`.
///
/// # Safety
/// `vm` is a live handle; `pixels` has `capacity` writable bytes; `frame`
/// is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_frame(
    vm: *const R86Vm,
    have_serial: u64,
    pixels: *mut u8,
    capacity: usize,
    frame: *mut R86Frame,
) -> bool {
    // SAFETY: caller contract.
    let shared = &unsafe { &*vm }.0.shared;
    let f = runner::lock(&shared.frame);
    // SAFETY: caller contract.
    unsafe {
        *frame = R86Frame {
            width: f.width,
            height: f.height,
            serial: f.serial,
        }
    };
    if f.serial == have_serial || f.rgba.len() > capacity || f.rgba.is_empty() {
        return false;
    }
    // SAFETY: `pixels` has at least `capacity >= f.rgba.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(f.rgba.as_ptr(), pixels, f.rgba.len()) };
    true
}

/// Pull up to `frames` 48 kHz stereo frames (interleaved i16) for playback.
/// Never blocks; returns the number of frames written. Callers fill the
/// rest with silence.
///
/// # Safety
/// `vm` is a live handle; `samples` has room for `frames * 2` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn r86_vm_audio_read(
    vm: *const R86Vm,
    samples: *mut i16,
    frames: usize,
) -> usize {
    // SAFETY: caller contract.
    let shared = &unsafe { &*vm }.0.shared;
    let Ok(mut audio) = shared.audio.try_lock() else {
        return 0;
    };
    let count = (audio.len() / 2).min(frames);
    // SAFETY: room for `frames * 2 >= count * 2` values.
    let out = unsafe { std::slice::from_raw_parts_mut(samples, count * 2) };
    for (slot, sample) in out.iter_mut().zip(audio.drain(..count * 2)) {
        *slot = sample;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_choices_round_trip_through_the_c_api() {
        let root = std::env::temp_dir().join(format!("r86-apple-engine-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let root_c = CString::new(root.to_str().unwrap()).unwrap();
        let directory = root.join("VM");
        let directory_c = CString::new(directory.to_str().unwrap()).unwrap();
        // SAFETY: all arguments below are live C strings or the documented NULL.
        unsafe {
            assert!(
                r86_library_create(
                    root_c.as_ptr(),
                    c"VM".as_ptr(),
                    c"Windows 98".as_ptr(),
                    16,
                    1,
                    std::ptr::null(),
                )
                .is_null()
            );
            assert_eq!(Vm::load(directory.clone()).unwrap().engine, "auto");
            for engine in ["jit", "interpreter", "interp", "auto"] {
                let value = CString::new(engine).unwrap();
                assert!(
                    r86_library_set(directory_c.as_ptr(), c"cpu-engine".as_ptr(), value.as_ptr())
                        .is_null()
                );
                let vm = Vm::load(directory.clone()).unwrap();
                assert_eq!(vm.engine, engine);
                assert_eq!(
                    vm.jit_enabled(),
                    r86::machine::DEFAULT_JIT && matches!(engine, "jit" | "auto")
                );
                let listed = CString::from_raw(r86_library_list(root_c.as_ptr()));
                let shown = if engine == "interp" {
                    "interpreter"
                } else {
                    engine
                };
                assert!(
                    listed
                        .to_str()
                        .unwrap()
                        .contains(&format!("\"engine\":\"{shown}\""))
                );
            }
            let error = r86_library_set(
                directory_c.as_ptr(),
                c"cpu-engine".as_ptr(),
                c"unknown".as_ptr(),
            );
            assert!(!error.is_null());
            assert!(
                CString::from_raw(error)
                    .to_str()
                    .unwrap()
                    .contains("Unknown CPU engine")
            );
            assert_eq!(Vm::load(directory.clone()).unwrap().engine, "auto");
            let guard = vm_store::lock_vm(&directory).unwrap();
            let error = r86_library_set(
                directory_c.as_ptr(),
                c"cpu-engine".as_ptr(),
                c"jit".as_ptr(),
            );
            assert!(!error.is_null());
            r86_string_free(error);
            assert_eq!(Vm::load(directory.clone()).unwrap().engine, "auto");
            drop(guard);
        }
        // Exercise every current preset by its public name, including unknown OSes.
        let mut vm = Vm::load(directory).unwrap();
        for os in [
            "Windows 3.1",
            "Windows 95",
            "Windows 98",
            "Windows ME",
            "Windows 2000",
            "Windows XP",
            "ReactOS",
        ] {
            vm.os = os.into();
            assert_eq!(vm.jit_enabled(), r86::machine::DEFAULT_JIT, "{os}");
        }
        for os in ["DOS", "Linux", "Other", "unknown"] {
            vm.os = os.into();
            assert!(!vm.jit_enabled(), "{os}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
