//! A VM's log for its current run, `<logs>/<VM folder>.log`, written only on
//! a fatal error, so a running VM never writes to the user's disk. Starting
//! the VM (its lock held) deletes the previous run's log. Until then the
//! run's recent events stay here in memory and the guest's serial output in
//! the Machine; a fatal error writes one report: r86 and the VM, the error,
//! the machine's state, the events and the serial output.
//!
//! A run belongs to the thread that runs the VM (runner.rs's core thread,
//! the handheld's session), where its panics also run the panic hook.
//! Release builds abort on panic, so the hook writes what it can reach
//! without the Machine: the panic, a backtrace and the events.

use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Instant;

use r86::machine::Machine;

use super::vm_store::Vm;

/// Events kept for a report; older ones are dropped.
const EVENTS: usize = 64;
/// The end of the guest's serial output in a report.
const SERIAL: usize = 16 * 1024;

struct Run {
    path: PathBuf,
    /// r86, the host and the VM.
    header: String,
    started: Instant,
    events: VecDeque<String>,
    /// The run's first fatal error wrote its report.
    reported: Cell<bool>,
}

thread_local! {
    static RUN: RefCell<Option<Run>> = const { RefCell::new(None) };
}

/// Starts this thread's run of `vm`, whose lock the caller holds, deleting
/// the previous run's log.
pub fn start(logs: &Path, vm: &Vm) {
    install_panic_hook();
    let path = logs.join(format!("{}.log", file_name(&vm.directory)));
    // Usually there is none.
    let _ = std::fs::remove_file(&path);
    let exe = std::env::current_exe().map_or_else(|_| "r86".into(), |exe| file_name(&exe));
    let header = format!(
        "r86 {} ({exe}, {} {})\n\
         VM {}: {}, {} MiB, engine {} ({}), disk {}, CD {}, network {}\n\
         Started {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        vm.name,
        vm.os,
        vm.memory,
        vm.engine,
        if vm.jit_enabled() {
            "JIT"
        } else {
            "interpreter"
        },
        file_name(&vm.disk),
        vm.iso.as_deref().map_or_else(|| "empty".into(), file_name),
        if vm.network { "on" } else { "off" },
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
    );
    RUN.with(|run| {
        *run.borrow_mut() = Some(Run {
            path,
            header,
            started: Instant::now(),
            events: VecDeque::new(),
            reported: Cell::new(false),
        })
    });
}

/// Records an event of this thread's run, in memory.
pub fn event(step: u64, text: &str) {
    RUN.with(|run| {
        if let Some(run) = run.borrow_mut().as_mut() {
            if run.events.len() == EVENTS {
                run.events.pop_front();
            }
            let at = run.started.elapsed().as_secs_f64();
            run.events
                .push_back(format!("{at:9.3} s  step {step:>12}  {text}"));
        }
    });
}

/// Writes the report of the fatal `error` that ends this thread's run, with
/// its machine's state if the machine was built; `eip` is the failing
/// instruction's, when known. The run's first report stands. Returns the
/// log's path.
pub fn fatal(error: &str, machine: Option<&mut Machine>, eip: Option<u32>) -> Option<PathBuf> {
    RUN.with(|run| {
        let run = run.borrow();
        let run = run.as_ref()?;
        if run.reported.get() {
            return Some(run.path.clone());
        }
        let mut text = run.head(error);
        let tail = machine.map(|machine| machine_report(&mut text, machine, eip));
        run.events_into(&mut text);
        text.push_str(&tail.unwrap_or_default());
        run.write(&text)
    })
}

/// Ends this thread's run; later panics on the thread are not its.
pub fn end() {
    RUN.with(|run| run.borrow_mut().take());
}

impl Run {
    fn head(&self, error: &str) -> String {
        let seconds = self.started.elapsed().as_secs_f64();
        format!("{}Failed after {seconds:.1} s: {error}\n", self.header)
    }

    fn events_into(&self, text: &mut String) {
        let _ = writeln!(text, "\nEvents, the last {EVENTS}:");
        for event in &self.events {
            let _ = writeln!(text, "  {event}");
        }
    }

    fn write(&self, text: &str) -> Option<PathBuf> {
        let written = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&self.path, text));
        match written {
            Ok(()) => {
                self.reported.set(true);
                Some(self.path.clone())
            }
            Err(err) => {
                eprintln!("r86: cannot write {}: {err}", self.path.display());
                None
            }
        }
    }
}

/// Appends the machine's state to `text`, with the code at `eip` or else
/// the CPU's; returns the long sections that go after the events: the
/// serial output and the whole CPU.
fn machine_report(text: &mut String, machine: &mut Machine, eip: Option<u32>) -> String {
    let eip = eip.unwrap_or(machine.cpu().eip);
    let _ = writeln!(
        text,
        "\nAt step {}: {}\n{}",
        machine.step_count(),
        machine.cpu_summary(),
        machine.code_bytes_report(eip)
    );
    let screen = machine.text_snapshot();
    if !screen.trim().is_empty() {
        let _ = writeln!(text, "\nScreen:\n{}", screen.trim_end());
    }
    let mut pages = machine.bus_warning_pages().peekable();
    if pages.peek().is_some() {
        let _ = writeln!(text, "\nUnclaimed bus accesses, by page:");
        for page in pages {
            let _ = writeln!(
                text,
                "  {} {:#x}: {} accesses, the last a {} of {:#x} ({:#04x}) at step {}{}",
                page.space.name(),
                page.address & !4095,
                page.count,
                if page.write { "write" } else { "read" },
                page.address,
                page.value,
                page.step,
                page.instruction
                    .map_or_else(String::new, |at| format!(" from {at:#x}")),
            );
        }
    }
    let mut opcodes = machine.undefined_opcodes().peekable();
    if opcodes.peek().is_some() {
        let _ = writeln!(text, "\nUndefined opcodes outside the CPU model:");
        for opcode in opcodes {
            let _ = writeln!(text, "  {opcode}");
        }
    }
    if let Some(shutdown) = machine.last_cpu_shutdown() {
        let _ = writeln!(
            text,
            "\nLast CPU shutdown (reset): step {}, {:?}",
            shutdown.step, shutdown.error
        );
    }
    let jit = machine.jit_report();
    if !jit.is_empty() {
        let _ = write!(text, "\n{jit}");
    }
    let serial = machine.serial_output();
    let serial = String::from_utf8_lossy(&serial[serial.len().saturating_sub(SERIAL)..]);
    let mut tail = String::new();
    if !serial.is_empty() {
        let _ = writeln!(tail, "\nSerial output, the last {SERIAL} bytes:\n{serial}");
    }
    let _ = writeln!(tail, "\nCPU:\n{:#?}", machine.cpu());
    tail
}

/// Before the default hook, writes the panicking thread's run report.
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = RUN.try_with(|run| {
                if let Ok(run) = run.try_borrow()
                    && let Some(run) = run.as_ref()
                    && !run.reported.get()
                {
                    let mut text = run.head(&format!("r86 internal error, {info}"));
                    let _ = write!(text, "\nBacktrace:\n{}\n", Backtrace::force_capture());
                    run.events_into(&mut text);
                    run.write(&text);
                }
            });
            default(info);
        }));
    });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
