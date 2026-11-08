//! Small Wasm ABI for web/wasm.mjs, currently driven by script/wasm_cli.cjs.
//! One JS host owns one machine and serializes all exports, including while
//! Asyncify has suspended Rust.
//! Pointers refer to this instance's memory; JS refreshes views after every
//! export that can allocate. Optional guest translations are ordinary Wasm
//! modules, published by the host only between serialized core invocations.
//! Text kind 5 drains bounded JIT trace JSON in wasm-jit + jit-profile builds;
//! kind 4 is the cumulative report. Neither endpoint changes guest state.

#[cfg(target_arch = "wasm32")]
mod wasm {
    use r86::FirmwareDisplay;
    use r86::devices::ps2::Ps2ButtonState;
    use r86::disk::Disk;
    use r86::machine::{BootOrder, GuestStepStatus, Machine};

    pub struct WebMachine {
        machine: Machine,
        buffer: Vec<u8>,
        output: String,
        pixels: Vec<u8>,
        error: String,
        async_stack: Vec<u32>,
    }

    // This ABI is private to the worker. It owns the handle and may not call
    // back into it from an import, a message handler, or a pending I/O promise.
    unsafe fn state<'a>(handle: *mut WebMachine) -> &'a mut WebMachine {
        unsafe { &mut *handle }
    }

    #[link(wasm_import_module = "browser")]
    unsafe extern "C" {
        fn log(bytes: *const u8, len: usize);
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn r86_create(
        ram_mib: u32,
        sectors: f64,
        cd_bytes: u32,
        flags: u32,
    ) -> *mut WebMachine {
        std::panic::set_hook(Box::new(|info| {
            let message = info.to_string();
            unsafe { log(message.as_ptr(), message.len()) };
        }));
        if !(16..=512).contains(&ram_mib) || !sectors.is_finite() || sectors.fract() != 0.0 {
            return std::ptr::null_mut();
        }
        let display = if flags & 2 != 0 {
            FirmwareDisplay::Serial
        } else {
            FirmwareDisplay::Vga
        };
        let Ok(mut machine) = Machine::new_firmware(ram_mib as usize * 1024 * 1024, display) else {
            return std::ptr::null_mut();
        };
        if sectors != 0.0 {
            let Ok(disk) = Disk::browser(0, sectors as u64, flags & 8 == 0) else {
                return std::ptr::null_mut();
            };
            machine.attach_disk(disk);
        }
        machine.attach_browser_cdrom(1, cd_bytes as usize, sectors == 0.0);
        machine.set_boot_order(if flags & 1 != 0 {
            BootOrder::CdromFirst
        } else {
            BootOrder::DiskFirst
        });
        if flags & 4 == 0 && machine.use_realtime_clock().is_err() {
            return std::ptr::null_mut();
        }
        Box::into_raw(Box::new(WebMachine {
            machine,
            buffer: Vec::new(),
            output: String::new(),
            pixels: Vec::new(),
            error: String::new(),
            async_stack: Vec::new(),
        }))
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_buffer(handle: *mut WebMachine, len: u32) -> *mut u8 {
        if len > 1024 * 1024 {
            return std::ptr::null_mut();
        }
        let s = unsafe { state(handle) };
        s.buffer.resize(len as usize, 0);
        s.buffer.as_mut_ptr()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_load_rom(handle: *mut WebMachine, kind: u32) -> i32 {
        let s = unsafe { state(handle) };
        let result = match kind {
            0 => s.machine.load_bios_rom(&s.buffer),
            1 => s.machine.load_option_rom(0xc0000, &s.buffer),
            2 => s.machine.load_bootsector(&s.buffer),
            _ => Err("invalid ROM kind".into()),
        };
        match result {
            Ok(()) => {
                if kind == 2 {
                    s.machine.enter_bootsector();
                } else {
                    s.machine.reset_bios();
                }
                0
            }
            Err(error) => {
                s.error = error;
                -1
            }
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_run(
        handle: *mut WebMachine,
        budget: u32,
        until_serial: u32,
    ) -> i32 {
        let s = unsafe { state(handle) };
        let result = if until_serial != 0 {
            s.machine.run_until_serial_output(u64::from(budget))
        } else {
            s.machine.run_steps(u64::from(budget))
        };
        match result {
            Ok(report) => match report.stop {
                Some(GuestStepStatus::PoweredOff) => 2,
                Some(GuestStepStatus::Halted) => 1,
                Some(GuestStepStatus::Stopped { message, .. }) => {
                    s.error = message;
                    -1
                }
                _ => 0,
            },
            Err(error) => {
                s.error = error;
                -1
            }
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_control(handle: *mut WebMachine, action: u32) -> i32 {
        let s = unsafe { state(handle) };
        let result = match action {
            0 => {
                s.machine.press_power_button();
                Ok(())
            }
            1 => s.machine.reboot(),
            2 => s.machine.flush_disks(),
            _ => Err("unknown machine control".into()),
        };
        match result {
            Ok(()) => 0,
            Err(error) => {
                s.error = error;
                -1
            }
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_input(handle: *mut WebMachine, kind: u32) {
        let s = unsafe { state(handle) };
        match kind {
            0 => s.machine.inject_ascii(&s.buffer),
            1 => s.machine.inject_scancode_set2(&s.buffer),
            2 => s.machine.queue_serial_input(&s.buffer),
            _ => (),
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_mouse(handle: *mut WebMachine, dx: i32, dy: i32, buttons: u32) {
        unsafe { state(handle) }.machine.inject_mouse(
            dx.clamp(-32768, 32767) as i16,
            dy.clamp(-32768, 32767) as i16,
            Ps2ButtonState {
                left: buttons & 1 != 0,
                right: buttons & 2 != 0,
                middle: buttons & 4 != 0,
            },
        );
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_frame(handle: *mut WebMachine) -> *const u8 {
        let s = unsafe { state(handle) };
        if !s.pixels.is_empty() && !s.machine.vga().whole_frame_dirty() {
            return std::ptr::null();
        }
        let info = s.machine.vga().mode_info();
        s.pixels
            .resize(usize::from(info.width) * usize::from(info.height) * 4, 0);
        if s.machine.vga().render_rgba(&mut s.pixels).is_none() {
            return std::ptr::null();
        }
        s.machine.clear_frame_dirty();
        s.pixels.as_ptr()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_dimensions(handle: *mut WebMachine) -> u32 {
        let info = unsafe { state(handle) }.machine.vga().mode_info();
        u32::from(info.width) | (u32::from(info.height) << 16)
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_steps(handle: *mut WebMachine) -> f64 {
        unsafe { state(handle) }.machine.step_count() as f64
    }

    #[cfg(feature = "wasm-jit")]
    #[unsafe(no_mangle)]
    pub extern "C" fn r86_jit_slots() -> u32 {
        (r86::x86::jit::wasm::frame::REGION_SLOTS * r86::x86::jit::wasm::frame::GROUP_FUNCTIONS)
            as u32
    }
    #[cfg(feature = "wasm-jit")]
    #[unsafe(no_mangle)]
    pub extern "C" fn r86_jit_group_size() -> u32 {
        r86::x86::jit::wasm::frame::GROUP_FUNCTIONS as u32
    }

    #[cfg(feature = "wasm-jit")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_jit_configure(
        handle: *mut WebMachine,
        mib: u32,
        threshold: u32,
    ) -> i32 {
        let s = unsafe { state(handle) };
        match s.machine.configure_wasm_jit(mib, threshold) {
            Ok(()) => 0,
            Err(error) => {
                s.error = error;
                -1
            }
        }
    }

    #[cfg(feature = "wasm-jit")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_jit_complete(
        handle: *mut WebMachine,
        request: u32,
        function: u32,
    ) -> u32 {
        u32::from(
            unsafe { state(handle) }
                .machine
                .complete_wasm_jit(request, function),
        )
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_serial(handle: *mut WebMachine) -> *const u8 {
        unsafe { state(handle) }.machine.serial_output().as_ptr()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_serial_len(handle: *mut WebMachine) -> u32 {
        unsafe { state(handle) }.machine.serial_output().len() as u32
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_asyncify_data(handle: *mut WebMachine) -> *mut u32 {
        let s = unsafe { state(handle) };
        s.async_stack.resize(256 * 1024 + 2, 0);
        let ptr = s.async_stack.as_mut_ptr();
        s.async_stack[0] = unsafe { ptr.add(2) } as u32;
        s.async_stack[1] = unsafe { ptr.add(s.async_stack.len()) } as u32;
        ptr
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_text(handle: *mut WebMachine, kind: u32) -> *const u8 {
        let s = unsafe { state(handle) };
        s.output = match kind {
            0 => String::from_utf8_lossy(s.machine.serial_output()).into_owned(),
            1 => s.machine.text_snapshot(),
            2 => {
                let mut text = format!(
                    "{}\nSteps: {}\nResets: {}\nCPU: {:?}\n",
                    s.error,
                    s.machine.step_count(),
                    s.machine.reset_count(),
                    s.machine.cpu()
                );
                if let Some(shutdown) = s.machine.last_cpu_shutdown() {
                    use std::fmt::Write;
                    let _ = writeln!(
                        text,
                        "Last CPU shutdown at {}: {:?}\n{}\n{}",
                        shutdown.step, shutdown.error, shutdown.cpu_dump, shutdown.vga_dump
                    );
                }
                for warning in s.machine.bus_warning_pages() {
                    use std::fmt::Write;
                    let _ = writeln!(text, "{warning:?}");
                }
                text
            }
            4 => s.machine.jit_report(),
            #[cfg(all(feature = "wasm-jit", feature = "jit-profile"))]
            5 => s.machine.take_wasm_jit_trace(),
            _ => s.error.clone(),
        };
        s.output.as_ptr()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_text_len(handle: *mut WebMachine) -> u32 {
        unsafe { state(handle) }.output.len() as u32
    }
}

fn main() {}
