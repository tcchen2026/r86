//! A VM's current-run log, `<logs>/<VM folder>.log`. Events stay in memory
//! while the VM runs; a fatal error writes its report immediately. Starting
//! the VM (its lock held) deletes the previous log.
//!
//! Every build records each new undefined-opcode address once, at its first
//! step, without draining Remote's queue. A clean exit writes the events if
//! any opcode was recorded. Machine bounds these records across guest resets.
//!
//! Only cfg(debug_assertions) builds also poll native-JIT clear counters and
//! write their events plus a summary on clean exit. Normal release has no JIT
//! clear counters or polling. Debug clear records count executed instructions
//! (no halted-time skips); their event prefixes use the Machine step observed
//! by the frontend. Clears between polls are combined.
//! After four lines in a burst, one combined count per second is kept; pending
//! counts flush at the next interval or on exit.
//!
//! A run belongs to its VM thread (runner.rs or the handheld session). The
//! panic hook retains buffered events; debug builds also include the most
//! recent counters. Release panics abort after the hook.

use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;
#[cfg(debug_assertions)]
use std::time::Duration;
use std::time::Instant;

use r86::machine::Machine;
#[cfg(debug_assertions)]
use r86::machine::{JitCacheCounters, JitCacheWork};

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
    /// Addresses already logged; Machine retains at most UNDEFINED_OPCODE_LIMIT.
    undefined_opcodes: BTreeSet<u32>,
    /// The run's first fatal error wrote its report.
    reported: Cell<bool>,
    #[cfg(debug_assertions)]
    jit: Option<JitLog>,
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
        env!("R86_VERSION"),
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
            undefined_opcodes: BTreeSet::new(),
            reported: Cell::new(false),
            #[cfg(debug_assertions)]
            jit: None,
        })
    });
}

/// Records an event of this thread's run, in memory.
pub fn event(step: u64, text: &str) {
    RUN.with(|run| {
        if let Some(run) = run.borrow_mut().as_mut() {
            run.event(step, text);
        }
    });
}

/// Poll after a frontend slice and before ending the session. Read the retained
/// opcode records: Remote independently consumes their notification queue.
pub fn poll(machine: &Machine) {
    RUN.with(|run| {
        if let Some(run) = run.borrow_mut().as_mut() {
            // The retained records only grow during a run, including resets.
            if machine.undefined_opcodes().count() == run.undefined_opcodes.len() {
                return;
            }
            for opcode in machine.undefined_opcodes() {
                if run.undefined_opcodes.insert(opcode.address) {
                    run.event(opcode.step, &opcode.to_string());
                }
            }
        }
    });
    #[cfg(debug_assertions)]
    poll_jit(machine);
}

/// Backends without these cache causes return None and emit no JIT diagnostics.
#[cfg(debug_assertions)]
fn poll_jit(machine: &Machine) {
    let Some(counters) = machine.jit_cache_counters() else {
        return;
    };
    let step = machine.step_count();
    let lines = RUN.with(|run| {
        let mut run = run.borrow_mut();
        let Some(run) = run.as_mut() else {
            return Vec::new();
        };
        let elapsed = run.started.elapsed();
        run.jit
            .get_or_insert_with(JitLog::default)
            .observe(counters, step, elapsed)
    });
    for line in lines {
        event(step, &line);
    }
}

#[cfg(debug_assertions)]
fn finish_jit() {
    let (step, lines) = RUN.with(|run| {
        let mut run = run.borrow_mut();
        run.as_mut()
            .and_then(|run| run.jit.as_mut())
            .map_or((0, Vec::new()), |jit| (jit.step, jit.finish()))
    });
    for line in lines {
        event(step, &line);
    }
}

#[cfg(debug_assertions)]
const CLEAR_NAMES: [&str; 4] = [
    "code-area-full",
    "page-record-limit",
    "no-free-table",
    "external-reset",
];

#[cfg(debug_assertions)]
#[derive(Default)]
struct JitLog {
    latest: JitCacheCounters,
    reported_causes: [u64; 4],
    reported_work: JitCacheWork,
    step: u64,
    window: Duration,
    lines: u32,
    limited: bool,
    finished: bool,
}

#[cfg(debug_assertions)]
impl JitLog {
    fn observe(&mut self, counters: JitCacheCounters, step: u64, elapsed: Duration) -> Vec<String> {
        self.latest = counters;
        self.step = step;
        if self.latest.causes == self.reported_causes {
            return Vec::new();
        }
        if elapsed.saturating_sub(self.window) >= Duration::from_secs(1) {
            self.window = elapsed;
            self.lines = 0;
        }
        if self.lines >= if self.limited { 1 } else { 4 } {
            self.limited = true;
            return Vec::new();
        }
        self.pending().into_iter().collect()
    }

    fn pending(&mut self) -> Option<String> {
        let causes = core::array::from_fn::<_, 4, _>(|i| {
            self.latest.causes[i].saturating_sub(self.reported_causes[i])
        });
        let count: u64 = causes.iter().sum();
        if count == 0 {
            return None;
        }
        let work = self.latest.last_clear;
        let previous = self.reported_work;
        let cause = if count == 1 {
            format!(
                "jit clear cause={}",
                CLEAR_NAMES[causes.iter().position(|&n| n != 0).unwrap()]
            )
        } else {
            format!(
                "jit clears coalesced count={count} code-area-full={} page-record-limit={} no-free-table={} external-reset={}",
                causes[0], causes[1], causes[2], causes[3]
            )
        };
        let line = format!(
            "{cause} executed_since={} compiled={} code_bytes={} pages_recorded={}",
            work.steps.saturating_sub(previous.steps),
            work.compiled_blocks
                .saturating_sub(previous.compiled_blocks),
            work.code_bytes.saturating_sub(previous.code_bytes),
            work.recorded_pages.saturating_sub(previous.recorded_pages)
        );
        self.reported_causes = self.latest.causes;
        self.reported_work = work;
        self.lines += 1;
        Some(line)
    }

    fn summary(&self) -> String {
        let c = self.latest.causes;
        format!(
            "jit summary code-area-full={} page-record-limit={} no-free-table={} external-reset={} compiled={} executed_steps={}",
            c[0], c[1], c[2], c[3], self.latest.current.compiled_blocks, self.latest.current.steps
        )
    }

    fn finish(&mut self) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut lines: Vec<_> = self.pending().into_iter().collect();
        lines.push(self.summary());
        lines
    }
}

/// Writes the report of the fatal `error` that ends this thread's run, with
/// its machine's state if the machine was built; `eip` is the failing
/// instruction's, when known. The run's first report stands. Returns the
/// log's path.
pub fn fatal(error: &str, machine: Option<&mut Machine>, eip: Option<u32>) -> Option<PathBuf> {
    if let Some(machine) = machine.as_deref() {
        poll(machine);
    }
    #[cfg(debug_assertions)]
    finish_jit();
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
    #[cfg(debug_assertions)]
    finish_jit();
    let run = RUN.with(|run| run.borrow_mut().take());
    if let Some(run) = run
        && run.save_on_clean_exit()
        && !run.reported.get()
    {
        let mut text = format!(
            "{}Stopped after {:.1} s\n",
            run.header,
            run.started.elapsed().as_secs_f64()
        );
        run.events_into(&mut text);
        run.write(&text);
    }
}

impl Run {
    fn event(&mut self, step: u64, text: &str) {
        if self.events.len() == EVENTS {
            self.events.pop_front();
        }
        let at = self.started.elapsed().as_secs_f64();
        self.events
            .push_back(format!("{at:9.3} s  step {step:>12}  {text}"));
    }

    fn save_on_clean_exit(&self) -> bool {
        #[cfg(debug_assertions)]
        if self.jit.is_some() {
            return true;
        }
        !self.undefined_opcodes.is_empty()
    }

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
                    #[cfg(debug_assertions)]
                    if let Some(jit) = &run.jit
                        && !jit.finished
                    {
                        let _ = writeln!(text, "{} (last frontend poll)", jit.summary());
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn start_test(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("r86-run-log-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        RUN.with(|run| {
            *run.borrow_mut() = Some(Run {
                path: path.clone(),
                header: "Test run\n".into(),
                started: Instant::now(),
                events: VecDeque::new(),
                undefined_opcodes: BTreeSet::new(),
                reported: Cell::new(false),
                #[cfg(debug_assertions)]
                jit: None,
            });
        });
        path
    }

    fn undefined_opcode(machine: &mut Machine, offset: usize) -> r86::machine::UndefinedOpcode {
        let mut sector = [0x90; 512];
        // SSE3 MOVSLDUP is outside this CPU model; an ordinary UD2 is not logged.
        sector[offset..offset + 4].copy_from_slice(&[0xf3, 0x0f, 0x12, 0xc1]);
        sector[510..].copy_from_slice(&[0x55, 0xaa]);
        machine.load_bootsector(&sector).unwrap();
        machine.enter_bootsector();
        machine.run_steps(offset as u64 + 1).unwrap();
        *machine
            .undefined_opcodes()
            .find(|opcode| opcode.address == 0x7c00 + offset as u32)
            .unwrap()
    }

    #[test]
    fn opcodes_are_logged_once_without_consuming_remote_queue() {
        let path = start_test("opcodes");
        let mut machine = Machine::new(0x10000);
        let first = undefined_opcode(&mut machine, 1);
        poll(&machine);
        poll(&machine);
        assert_eq!(machine.take_undefined_opcode(), Some(first));
        assert!(machine.take_undefined_opcode().is_none());

        // Repeats update only the count; resets keep the record and log identity.
        machine.reboot().unwrap();
        machine.run_steps(2).unwrap();
        assert_eq!(machine.undefined_opcodes().next().unwrap().count, 2);
        poll(&machine);

        // A newly recorded lower address must be found even if Remote polls first.
        let second = undefined_opcode(&mut machine, 0);
        assert_eq!(machine.take_undefined_opcode(), Some(second));
        poll(&machine);
        poll(&machine);
        RUN.with(|run| {
            let run = run.borrow();
            let events = &run.as_ref().unwrap().events;
            assert_eq!(events.len(), 2);
            for (event, opcode) in events.iter().zip([first, second]) {
                assert!(event.ends_with(&format!("step {:>12}  {opcode}", opcode.step)));
            }
        });
        assert!(!path.exists(), "events stay in memory until the run ends");
        end();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("#UD cs=").count(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clean_exit_saves_opcodes_or_debug_jit_and_preserves_fatal_report() {
        for opcode in [false, true] {
            for native in [false, true] {
                let path = start_test("clean-exit");
                if opcode {
                    let mut machine = Machine::new(0x10000);
                    undefined_opcode(&mut machine, 0);
                    poll(&machine);
                }
                #[cfg(debug_assertions)]
                if native {
                    RUN.with(|run| {
                        run.borrow_mut().as_mut().unwrap().jit = Some(JitLog::default())
                    });
                }
                end();
                assert_eq!(path.exists(), opcode || (cfg!(debug_assertions) && native));
                if path.exists() {
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert_eq!(text.contains("#UD cs="), opcode);
                    assert_eq!(
                        text.contains("jit summary"),
                        cfg!(debug_assertions) && native
                    );
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        let path = start_test("fatal-exit");
        let mut machine = Machine::new(0x10000);
        undefined_opcode(&mut machine, 0);
        assert_eq!(
            fatal("test failure", Some(&mut machine), None),
            Some(path.clone())
        );
        let fatal_text = std::fs::read_to_string(&path).unwrap();
        end();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fatal_text);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cache_log_keeps_causes_work_and_summary_when_rate_limited() {
        let mut log = JitLog::default();
        let mut counters = JitCacheCounters::default();
        for clear in 1..=7 {
            counters.causes[3] = clear;
            counters.last_clear = JitCacheWork {
                steps: clear * 100,
                compiled_blocks: clear * 2,
                code_bytes: clear * 32,
                recorded_pages: clear,
            };
            counters.current = counters.last_clear;
            let lines = log.observe(counters, clear * 150, Duration::from_millis(clear * 10));
            if clear <= 4 {
                assert_eq!(
                    lines,
                    [
                        "jit clear cause=external-reset executed_since=100 compiled=2 code_bytes=32 pages_recorded=1"
                    ]
                );
            } else {
                assert!(lines.is_empty());
            }
        }
        let lines = log.observe(counters, 1100, Duration::from_secs(1));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("coalesced count=3"));
        assert!(lines[0].ends_with("executed_since=300 compiled=6 code_bytes=96 pages_recorded=3"));
        counters.causes[0] = 1;
        counters.causes[2] = 1;
        counters.last_clear.steps += 200;
        counters.current = counters.last_clear;
        let lines = log.observe(counters, 1400, Duration::from_millis(1010));
        assert!(lines.is_empty());
        let lines = log.finish();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(
            "count=2 code-area-full=1 page-record-limit=0 no-free-table=1 external-reset=0"
        ));
        assert_eq!(
            lines[1],
            "jit summary code-area-full=1 page-record-limit=0 no-free-table=1 external-reset=7 compiled=14 executed_steps=900"
        );
        assert!(log.finish().is_empty());
    }
}
