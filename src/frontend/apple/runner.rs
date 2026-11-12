//! One running VM: its core thread and the state the app thread may touch.
//! The core thread owns the Machine, disks, network and VM lock. The app
//! queues input and commands, copies the latest frame and pulls audio.

use std::collections::VecDeque;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use r86::devices::{Ps2ButtonState, RenderMode};
use r86::machine::{GuestStepStatus, Machine};
use r86::network::UserNetwork;

use super::keys::{self, InputKey};
use super::vm_store::Vm;

const SLICE_STEPS: u64 = 200_000;
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
/// About a quarter second of 48 kHz stereo; older samples are dropped.
const AUDIO_SAMPLES: usize = 48_000 / 4 * 2;

pub enum Command {
    Pause(bool),
    Reset,
    PowerButton,
    CtrlAltDel,
    /// Release held keys and buttons, as when the window loses focus.
    ReleaseInput,
    Key(InputKey, bool),
    Mouse(i32, i32, Ps2ButtonState),
    Cd(Option<PathBuf>),
    Volume(u8),
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    Running = 0,
    Paused = 1,
    Off = 2,
    Failed = 3,
}

#[derive(Default)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub serial: u64,
}

pub struct Status {
    pub state: State,
    /// Guest instructions per second over the last second.
    pub ips: u64,
    pub messages: VecDeque<String>,
}

#[derive(Default)]
struct Input {
    commands: Vec<Command>,
    stop: bool,
}

pub struct Shared {
    input: Mutex<Input>,
    wake: Condvar,
    pub frame: Mutex<Frame>,
    pub audio: Mutex<VecDeque<i16>>,
    pub status: Mutex<Status>,
}

pub struct Running {
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking core thread reports itself; its data stays usable.
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Running {
    /// Lock the VM, build its machine and start the core thread.
    pub fn start(vm: Vm, firmware: PathBuf) -> Result<Self, String> {
        let lock_file = super::vm_store::lock_vm(&vm.directory)?;
        let no_jit = std::env::args().any(|arg| arg == "--no-jit");
        let shared = Arc::new(Shared {
            input: Mutex::default(),
            wake: Condvar::new(),
            frame: Mutex::default(),
            audio: Mutex::default(),
            status: Mutex::new(Status {
                state: State::Running,
                ips: 0,
                messages: VecDeque::new(),
            }),
        });
        let thread_shared = shared.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("r86 core".into())
            .spawn(move || {
                // MAP_JIT ownership starts here, including partial startup
                // failures. Machine/Engine never move between app/core threads.
                let core = (|| -> Result<Core, String> {
                    set_core_qos()?;
                    let mut machine =
                        launch_config(&vm, no_jit).machine(&firmware, "vgabios-stdvga.bin")?;
                    let network = if vm.network {
                        machine.attach_network();
                        Some(UserNetwork::new(&[]).map_err(|e| format!("network: {e}"))?)
                    } else {
                        None
                    };
                    if vm.volume > 0 {
                        machine.enable_audio();
                    }
                    Ok(Core {
                        vm,
                        machine,
                        network,
                        shared: thread_shared,
                        paused: false,
                        buttons: Ps2ButtonState::default(),
                        held: Vec::new(),
                        samples: Vec::new(),
                    })
                })();
                match core {
                    Ok(core) => {
                        if ready.send(Ok(())).is_ok() {
                            core.run(lock_file);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .map_err(|err| format!("Cannot start the VM thread: {err}"))?;
        // Preserve synchronous launch errors without constructing native state
        // on the app thread. Stop/reset/media commands already use Shared.input.
        if let Err(error) = started
            .recv()
            .unwrap_or_else(|_| Err("VM thread failed during startup".into()))
        {
            let _ = thread.join();
            return Err(error);
        }
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: Command) {
        lock(&self.shared.input).commands.push(command);
        self.shared.wake.notify_one();
    }

    /// Turn off now: stop the core thread, which flushes disks and unlocks.
    pub fn stop(&mut self) {
        lock(&self.shared.input).stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// Runtime overrides must not change the preference saved by media/volume edits.
fn launch_config(vm: &Vm, no_jit: bool) -> Vm {
    let mut config = vm.clone();
    if no_jit {
        config.engine = "interpreter".into();
    }
    config
}

fn set_core_qos() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: changes only this core thread's requested scheduling class.
        let error = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if error != 0 {
            return Err(format!(
                "Cannot set the VM thread's user-interactive QoS: {}",
                std::io::Error::from_raw_os_error(error)
            ));
        }
    }
    Ok(())
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::super::guest_os;
    use super::*;

    #[test]
    fn engine_override_preserves_the_saved_preference() {
        let mut vm = Vm::new(
            std::path::Path::new("unused"),
            "VM".into(),
            guest_os::by_name("Windows 98"),
        );
        for engine in ["auto", "jit", "interpreter", "interp"] {
            vm.engine = engine.into();
            assert_eq!(launch_config(&vm, false).engine, engine);
            assert!(!launch_config(&vm, true).jit_enabled());
            assert_eq!(vm.engine, engine);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn core_thread_requests_user_interactive_qos() {
        std::thread::spawn(|| {
            set_core_qos().unwrap();
            let mut priority = 1;
            let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
            // SAFETY: current pthread handle and matching writable scalars.
            let error = unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut priority)
            };
            assert_eq!(error, 0);
            assert_eq!(
                class as u32,
                libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE as u32
            );
            assert_eq!(priority, 0);
        })
        .join()
        .unwrap();
    }
}

struct Core {
    vm: Vm,
    machine: Machine,
    network: Option<UserNetwork>,
    shared: Arc<Shared>,
    paused: bool,
    buttons: Ps2ButtonState,
    /// Keys the app holds down in the guest, released on pause and reset.
    held: Vec<InputKey>,
    samples: Vec<i16>,
}

impl Core {
    fn run(mut self, lock_file: File) {
        let shared = self.shared.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.execute()));
        let (state, message) = match result {
            Ok(Ok(())) => (State::Off, None),
            Ok(Err(err)) => (State::Failed, Some(err)),
            Err(panic) => {
                let text = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                (State::Failed, Some(format!("r86 internal error: {text}")))
            }
        };
        let flushed = self.machine.flush_disks().err();
        drop(lock_file);
        let mut status = lock(&shared.status);
        status.state = state;
        status.ips = 0;
        status.messages.extend(message);
        status
            .messages
            .extend(flushed.map(|err| format!("Disk flush failed: {err}")));
    }

    fn execute(&mut self) -> Result<(), String> {
        let mut next_frame = Instant::now();
        let mut second = (Instant::now(), self.machine.step_count());
        loop {
            let commands = {
                let mut input = lock(&self.shared.input);
                if input.stop {
                    return Ok(());
                }
                std::mem::take(&mut input.commands)
            };
            for command in commands {
                self.apply(command)?;
            }
            if self.paused {
                self.publish_state(State::Paused, 0);
                let input = lock(&self.shared.input);
                if input.commands.is_empty() && !input.stop {
                    drop(
                        self.shared
                            .wake
                            .wait_timeout(input, Duration::from_millis(200)),
                    );
                }
                second = (Instant::now(), self.machine.step_count());
                continue;
            }
            if let Some(network) = &mut self.network {
                network
                    .poll(&mut self.machine)
                    .map_err(|e| format!("network: {e}"))?;
            }
            let report = self.machine.run_steps(SLICE_STEPS)?;
            self.pump_audio();
            let now = Instant::now();
            if now >= next_frame && self.machine.vga().whole_frame_dirty() {
                self.publish_frame();
                next_frame = now + FRAME_INTERVAL;
            }
            if now.duration_since(second.0) >= Duration::from_secs(1) {
                let steps = self.machine.step_count();
                let elapsed = now.duration_since(second.0).as_secs_f64();
                self.publish_state(State::Running, ((steps - second.1) as f64 / elapsed) as u64);
                second = (now, steps);
            }
            match report.stop {
                Some(GuestStepStatus::Halted) => {
                    let wait = self.machine.halted_wait_duration();
                    let wait = self.network.as_ref().map_or(wait, |n| n.limit_wait(wait));
                    let input = lock(&self.shared.input);
                    if input.commands.is_empty() && !input.stop {
                        drop(self.shared.wake.wait_timeout(input, wait));
                    }
                }
                Some(GuestStepStatus::PoweredOff) => return Ok(()),
                Some(GuestStepStatus::Stopped { message, eip }) => {
                    let at = eip.map_or(String::new(), |eip| format!(" at EIP {eip:#010x}"));
                    return Err(format!("The VM stopped{at}: {message}"));
                }
                _ => {}
            }
        }
    }

    fn apply(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Pause(paused) => {
                self.paused = paused;
                if paused {
                    self.release_input();
                }
            }
            Command::Reset => {
                self.machine.reboot()?;
                self.held.clear();
                self.buttons = Ps2ButtonState::default();
            }
            Command::PowerButton => self.machine.press_power_button(),
            Command::CtrlAltDel => {
                for name in ["controlleft", "altleft", "delete"] {
                    keys::inject_named_key(&mut self.machine, name, true)?;
                }
                for name in ["delete", "altleft", "controlleft"] {
                    keys::inject_named_key(&mut self.machine, name, false)?;
                }
            }
            Command::ReleaseInput => self.release_input(),
            Command::Key(key, down) => {
                let held = self.held.iter().position(|k| *k == key);
                match (down, held) {
                    (true, None) => self.held.push(key),
                    (false, Some(index)) => {
                        self.held.remove(index);
                    }
                    // A release the guest never saw pressed stays unsent.
                    (false, None) => return Ok(()),
                    (true, Some(_)) => {}
                }
                keys::inject_input_key(&mut self.machine, key, down);
            }
            Command::Mouse(mut dx, mut dy, buttons) => {
                self.buttons = buttons;
                // PS/2 packets carry 9-bit deltas; the device splits further.
                loop {
                    let (x, y) = (dx.clamp(-255, 255), dy.clamp(-255, 255));
                    self.machine.inject_mouse(x as i16, y as i16, buttons);
                    (dx, dy) = (dx - x, dy - y);
                    if dx == 0 && dy == 0 {
                        break;
                    }
                }
            }
            Command::Cd(path) => {
                let result = match &path {
                    Some(path) => File::open(path)
                        .map_err(|err| format!("Cannot open {}: {err}", path.display()))
                        .and_then(|file| self.machine.change_file_cdrom(file)),
                    None => self
                        .machine
                        .change_cdrom(Vec::new())
                        .map_err(str::to_string),
                };
                let message = match result {
                    Ok(()) => {
                        self.vm.cdrom = true;
                        self.vm.iso = path;
                        // This thread holds the VM lock.
                        self.vm.save_locked()?;
                        if self.vm.iso.is_some() {
                            "CD inserted".into()
                        } else {
                            "CD ejected".into()
                        }
                    }
                    Err(err) => err,
                };
                lock(&self.shared.status).messages.push_back(message);
            }
            Command::Volume(volume) => {
                let was = self.vm.volume > 0;
                self.vm.volume = volume.min(100);
                // Enabling restarts the stream: only on leaving mute.
                match (was, self.vm.volume > 0) {
                    (false, true) => self.machine.enable_audio(),
                    (true, false) => {
                        self.machine.disable_audio();
                        lock(&self.shared.audio).clear();
                    }
                    _ => {}
                }
                self.vm.save_locked()?;
            }
        }
        Ok(())
    }

    /// Release everything the app holds in the guest, as on focus loss.
    fn release_input(&mut self) {
        for key in std::mem::take(&mut self.held) {
            keys::inject_input_key(&mut self.machine, key, false);
        }
        if self.buttons != Ps2ButtonState::default() {
            self.buttons = Ps2ButtonState::default();
            self.machine.inject_mouse(0, 0, self.buttons);
        }
    }

    fn pump_audio(&mut self) {
        if self.vm.volume == 0 {
            return;
        }
        self.machine.drain_audio(&mut self.samples);
        if self.samples.is_empty() {
            return;
        }
        if self.vm.volume < 100 {
            let volume = i32::from(self.vm.volume);
            for sample in &mut self.samples {
                *sample = (i32::from(*sample) * volume / 100) as i16;
            }
        }
        let mut audio = lock(&self.shared.audio);
        audio.extend(&self.samples);
        let excess = audio.len().saturating_sub(AUDIO_SAMPLES);
        audio.drain(..excess);
    }

    fn publish_frame(&mut self) {
        let info = self.machine.vga().mode_info();
        let (width, height) = (u32::from(info.width), u32::from(info.height));
        let mut frame = lock(&self.shared.frame);
        frame.rgba.resize(width as usize * height as usize * 4, 0);
        if info.mode == RenderMode::Blank
            || self.machine.vga().render_rgba(&mut frame.rgba).is_none()
        {
            frame.rgba.fill(0);
        }
        frame.width = width;
        frame.height = height;
        frame.serial += 1;
        drop(frame);
        self.machine.clear_frame_dirty();
    }

    fn publish_state(&self, state: State, ips: u64) {
        let mut status = lock(&self.shared.status);
        status.state = state;
        status.ips = ips;
    }
}
