//! An embeddable PC interpreter with independent device and instruction clocks. Frontends own files, event
//! loops, and presentation; `Machine` owns CPU, memory, and guest device state.

mod bus_diagnostics;
mod clock;
mod magic;
#[cfg(test)]
mod sound_tests;
#[cfg(test)]
mod usb_tests;
use bus_diagnostics::BusDiagnostics;
pub use bus_diagnostics::{BUS_PAGE_LOG_LIMIT, BusCounter, BusSpace, BusWarning};
use clock::{Clock, TICKS_PER_SECOND};

use crate::acpi::Acpi;
use crate::devices::audio::Sound;
use crate::devices::firmware::FirmwareShadow;
use crate::devices::ne2000::{IRQ as NET_IRQ, Ne2000};
use crate::devices::pic::Pic;
#[cfg(test)]
use crate::devices::pit::CLOCK_DIVISOR as PIT_OSCILLATOR_DIVISOR;
use crate::devices::pit::Pit;
use crate::devices::usb::{IRQ as USB_IRQ, TABLET_MAX, Uhci};
use crate::devices::{DeviceEffect, I8042, PcVga, Ps2ButtonState, Ps2Key};
#[cfg(feature = "host")]
use crate::linux_boot::{BootConfig, DirectLinuxBoot, LoadedBoot};
pub use crate::run_devices::CdromState;
use crate::run_devices::{FirmwareDisplay, FwCfgDevice, IdeBus, IdeIrq, PciConfig};
#[cfg(test)]
use crate::x86::cpu::EFLAGS_IF;
use crate::x86::cpu::{SegmentCache, SegmentReg};
use crate::x86::mem::{
    ram_read_u16, ram_read_u32, ram_read_u64, ram_write_u16, ram_write_u32, ram_write_u64,
};
use crate::x86::mmu::{self, AccessKind};
use crate::x86::{
    BackingToken, BatchStepStop, BatchStop, Bus, Cpu, FrameRoleFlags, Interpreter, LinearAddr,
    MemError, PageTableFrameRole, PhysAddr, PhysFrame, PhysRegion, PhysRegionKind, PhysRegionMap,
    PhysWrite, Reg32, StepError, StepOutcome, WriteOrigin, deliver_exception,
    deliver_external_interrupt, deliver_software_interrupt_event,
};
use alloc::{
    boxed::Box,
    collections::VecDeque,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt::Write as _;

/// Frontends enable the JIT by default only on ARM64 Linux; ARMv6K
/// builds select it explicitly.
pub const DEFAULT_JIT: bool = cfg!(all(
    feature = "jit",
    target_arch = "aarch64",
    target_os = "linux"
));
const GUEST_PAGE_SIZE: usize = 4096;
/// Guest serial output retained for frontends (at least this many bytes).
const SERIAL_KEEP: usize = 64 * 1024;
// Match the selected CPU model's PAE/PSE36 physical address width.
const PC_PHYS_LIMIT: u64 = 1 << 36;
const A20_BIT: u64 = 1 << 20;
const FDC_MSR_RQM: u8 = 0x80;
const FDC_MSR_DIO: u8 = 0x40;
const FDC_MSR_BUSY: u8 = 0x10;
const CMOS_REG_A: u8 = 0x0a;
const CMOS_REG_B: u8 = 0x0b;
const CMOS_REG_C: u8 = 0x0c;
const CMOS_REG_D: u8 = 0x0d;
const CMOS_REG_B_PIE: u8 = 0x40;
const RTC_BASE_HZ: u64 = 32_768;

/// SeaBIOS tries the other attached device if the preferred one cannot boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootOrder {
    DiskFirst,
    CdromFirst,
}

#[derive(Clone, Copy, Debug)]
struct RtcCalendar {
    second: u8,
    minute: u8,
    hour: u8,
    weekday: u8,
    day: u8,
    month: u8,
    year: u32,
}

fn pending_external_irq(cpu: &Cpu, bus: &HostBus) -> Option<u8> {
    cpu.maskable_interrupts_enabled()
        .then(|| bus.pic.pending_irq())
        .flatten()
}

fn poll_pending_external_irq<P: PerfSink>(
    step: u64,
    cpu: &Cpu,
    bus: &mut HostBus,
    perf: &mut P,
) -> Option<u8> {
    perf.external_irq_check();
    bus.current_step = step;
    if cpu.halted || step >= bus.next_attention_step {
        bus.sample_device_clock(step);
    }
    if bus.refresh_interrupt_attention(step) {
        perf.full_device_scan();
    }
    pending_external_irq(cpu, bus)
}

#[inline(always)]
fn interrupt_attention_needed(
    step: u64,
    was_interruptible: bool,
    cpu: &Cpu,
    bus: &HostBus,
) -> bool {
    // With no device event due, an IF transition only needs the poll when
    // the PIC already has a deliverable request. The cached deadline still
    // bounds execution while interrupts are masked or the PIC is quiet.
    bus.interrupt_attention_due(step)
        || (!was_interruptible && pending_external_irq(cpu, bus).is_some())
}

fn deliver_irq(cpu: &mut Cpu, bus: &mut HostBus, return_eip: u32) -> Result<(), String> {
    let vector = bus.pic.acknowledge();
    let result = deliver_external_interrupt(cpu, bus, vector, return_eip);
    finish_event_delivery(cpu, bus, result)
}

fn finish_event_delivery(
    cpu: &Cpu,
    bus: &mut HostBus,
    result: Result<(), StepError>,
) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(error @ StepError::TripleFault { .. }) => {
            // The board converts CPU shutdown into reset. Capture the fault
            // before RAM/devices/CPU are reinitialized, including a bounded
            // serial tail. This survives later manual and guest resets.
            bus.last_cpu_shutdown = Some(CpuShutdown {
                step: bus.current_step,
                error,
                cpu_dump: format!("{cpu:#?}"),
                vga_dump: bus.vga.register_report(),
                serial: bus.serial[bus.serial.len().saturating_sub(16384)..].to_vec(),
            });
            bus.reset_requested = true;
            bus.set_attention();
            Ok(())
        }
        Err(error) => Err(format!(
            "emulator error during x86 event delivery: {error:?}"
        )),
    }
}

fn enter_bootsector(cpu: &mut Cpu) {
    let real_seg = SegmentCache {
        selector: 0,
        base: 0,
        limit: 0xffff,
        flags: 0x0093,
    };
    cpu.reset_bios();
    cpu.eip = 0x0000_7c00;
    for seg in [
        SegmentReg::Es,
        SegmentReg::Cs,
        SegmentReg::Ss,
        SegmentReg::Ds,
        SegmentReg::Fs,
        SegmentReg::Gs,
    ] {
        cpu.set_seg_cache(seg, real_seg);
    }
    cpu.set_reg(Reg32::Edx, 0x80);
    cpu.invalidate_translation_context();
}

#[derive(Clone, Copy)]
pub enum TraceMode {
    Off,
    Minimal,
    Full,
}

#[derive(Clone, Copy)]
pub enum PerfMode {
    Disabled,
    Enabled,
}

enum TraceStore {
    None(NullTrace),
    Minimal(MinimalTraceRing),
    Full(TraceRing),
}

impl TraceStore {
    fn report(&self) -> String {
        match self {
            Self::None(trace) => trace.report(),
            Self::Minimal(trace) => trace.report(),
            Self::Full(trace) => trace.report(),
        }
    }
}

enum PerfStore {
    Disabled,
    Enabled(PerfCounters),
}

trait PerfSink {
    #[inline(always)]
    fn guest_instruction(&mut self) {}
    #[inline(always)]
    fn minimal_trace_push(&mut self) {}
    #[inline(always)]
    fn full_trace_push(&mut self) {}
    #[inline(always)]
    fn external_irq_check(&mut self) {}
    #[inline(always)]
    fn full_device_scan(&mut self) {}
    #[inline(always)]
    fn actual_irq_delivery(&mut self) {}
    #[inline(always)]
    fn halted_exit(&mut self) {}
    #[inline(always)]
    fn reset_exit(&mut self) {}
    #[inline(always)]
    fn stopped_exit(&mut self) {}
    #[inline(always)]
    fn batch_budget_exit(&mut self) {}
    #[inline(always)]
    fn batch_stop_exit(&mut self) {}
    #[inline(always)]
    fn attention_irq_poll(&mut self) {}
}

struct NoPerfCounters;

impl PerfSink for NoPerfCounters {}

#[derive(Default)]
struct PerfCounters {
    guest_instructions: u64,
    minimal_trace_pushes: u64,
    full_trace_pushes: u64,
    external_irq_checks: u64,
    full_device_scans: u64,
    actual_irq_deliveries: u64,
    halted_exits: u64,
    reset_exits: u64,
    stopped_exits: u64,
    batch_budget_exits: u64,
    batch_stop_exits: u64,
    attention_irq_polls: u64,
}

impl PerfCounters {
    fn report(&self, step: u64) -> String {
        format!(
            "perf counters: step={} guest_instructions={} minimal_trace_pushes={} full_trace_pushes={} external_irq_checks={} full_device_scans={} actual_irq_deliveries={} halted_exits={} reset_exits={} stopped_exits={} batch_budget_exits={} batch_stop_exits={} attention_irq_polls={}",
            step,
            self.guest_instructions,
            self.minimal_trace_pushes,
            self.full_trace_pushes,
            self.external_irq_checks,
            self.full_device_scans,
            self.actual_irq_deliveries,
            self.halted_exits,
            self.reset_exits,
            self.stopped_exits,
            self.batch_budget_exits,
            self.batch_stop_exits,
            self.attention_irq_polls
        )
    }
}

impl PerfSink for PerfCounters {
    #[inline(always)]
    fn guest_instruction(&mut self) {
        self.guest_instructions = self.guest_instructions.wrapping_add(1);
    }

    #[inline(always)]
    fn minimal_trace_push(&mut self) {
        self.minimal_trace_pushes = self.minimal_trace_pushes.wrapping_add(1);
    }

    #[inline(always)]
    fn full_trace_push(&mut self) {
        self.full_trace_pushes = self.full_trace_pushes.wrapping_add(1);
    }

    #[inline(always)]
    fn external_irq_check(&mut self) {
        self.external_irq_checks = self.external_irq_checks.wrapping_add(1);
    }

    #[inline(always)]
    fn full_device_scan(&mut self) {
        self.full_device_scans = self.full_device_scans.wrapping_add(1);
    }

    #[inline(always)]
    fn actual_irq_delivery(&mut self) {
        self.actual_irq_deliveries = self.actual_irq_deliveries.wrapping_add(1);
    }

    #[inline(always)]
    fn halted_exit(&mut self) {
        self.halted_exits = self.halted_exits.wrapping_add(1);
    }

    #[inline(always)]
    fn reset_exit(&mut self) {
        self.reset_exits = self.reset_exits.wrapping_add(1);
    }

    #[inline(always)]
    fn stopped_exit(&mut self) {
        self.stopped_exits = self.stopped_exits.wrapping_add(1);
    }

    #[inline(always)]
    fn batch_budget_exit(&mut self) {
        self.batch_budget_exits = self.batch_budget_exits.wrapping_add(1);
    }

    #[inline(always)]
    fn batch_stop_exit(&mut self) {
        self.batch_stop_exits = self.batch_stop_exits.wrapping_add(1);
    }

    #[inline(always)]
    fn attention_irq_poll(&mut self) {
        self.attention_irq_polls = self.attention_irq_polls.wrapping_add(1);
    }
}

trait TraceSink {
    fn push<P: PerfSink>(&mut self, step: u64, cpu: &Cpu, ram: &[u8], perf: &mut P);
}

pub struct Machine {
    cpu: Cpu,
    bus: HostBus,
    trace: TraceStore,
    perf: PerfStore,
    step: u64,
    boots: u64,
    boot_image: Option<BootImage>,
    roms: Vec<(usize, Vec<u8>)>,
    #[cfg(any(
        all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
        all(feature = "jit", target_arch = "arm")
    ))]
    jit: Option<crate::x86::jit::runtime::Engine>,
    #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
    chip_jit: Option<crate::chipjit::runtime::Engine>,
    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
    wasm_jit: Option<crate::x86::jit::wasm::runtime::Engine>,
}

enum BootImage {
    Bios,
    Sector(Box<[u8; 512]>),
    #[cfg(feature = "host")]
    Linux {
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        cmdline: String,
        host_decompress_kernel: bool,
        host_decompress_initrd: bool,
        overlay_files: Vec<crate::initrd_overlay::InitrdFile>,
    },
    #[cfg(not(feature = "host"))]
    Preloaded,
}

impl Machine {
    /// Creates the small direct-boot PC, without firmware-only devices.
    pub fn new(ram_bytes: usize) -> Self {
        Self::from_parts(
            Cpu::new(),
            HostBus::new(ram_bytes),
            TraceMode::Off,
            PerfMode::Disabled,
        )
    }

    /// Runs the production PC against RAM the bare-metal host preloaded.
    #[cfg(not(feature = "host"))]
    pub fn from_preloaded(cpu: Cpu, ram: &'static mut [u8]) -> Self {
        let regions = PhysRegionMap::ram(ram.len() as u64).expect("RAM region map fits");
        let mut machine = Self::from_parts(
            cpu,
            HostBus::with_regions(ram, regions, false, FirmwareDisplay::Serial),
            TraceMode::Off,
            PerfMode::Disabled,
        );
        machine.boot_image = Some(BootImage::Preloaded);
        machine
    }

    fn boot_reload_is_external(&self) -> bool {
        #[cfg(not(feature = "host"))]
        if matches!(self.boot_image, Some(BootImage::Preloaded)) {
            return true;
        }
        false
    }

    pub fn new_firmware(ram_bytes: usize, display: FirmwareDisplay) -> Result<Self, String> {
        Ok(Self::from_parts(
            Cpu::new(),
            HostBus::new_firmware_with_display(ram_bytes, display)?,
            TraceMode::Off,
            PerfMode::Disabled,
        ))
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }
    pub fn ram(&self) -> &[u8] {
        &self.bus.ram
    }
    pub fn step_count(&self) -> u64 {
        self.step
    }
    pub fn reset_count(&self) -> u64 {
        self.boots
    }
    pub fn last_cpu_shutdown(&self) -> Option<&CpuShutdown> {
        self.bus.last_cpu_shutdown.as_ref()
    }
    pub fn vga(&self) -> &PcVga {
        &self.bus.vga
    }
    pub fn firmware_mode(&self) -> bool {
        self.bus.firmware_mode
    }
    pub fn set_boot_order(&mut self, order: BootOrder) {
        self.bus.boot_order = order;
    }
    pub fn powered_off(&self) -> bool {
        self.bus.acpi.powered_off
    }
    /// Cut power from the host side, like holding the power button: the guest
    /// gets no warning. Disks are flushed; `reboot` powers the machine on again.
    pub fn power_off(&mut self) -> Result<(), String> {
        self.bus.acpi.powered_off = true;
        self.flush_disks()
    }
    /// The first hard disk. Change it only while powered off: a running guest
    /// caches its filesystem.
    pub fn hdd_mut(&mut self) -> Option<&mut crate::disk::Disk> {
        self.bus.ide.first_hdd_mut()
    }
    /// Latch the ACPI fixed power-button event. The guest decides when to shut
    /// down; this does not cut power or bypass filesystem shutdown.
    pub fn press_power_button(&mut self) {
        self.bus.acpi.power_button();
        self.bus.set_attention();
    }
    /// The retained tail of the guest's COM1 and debug-port output: at least
    /// the last 64 KiB, ending at absolute position `serial_end`.
    pub fn serial_output(&self) -> &[u8] {
        &self.bus.serial
    }
    /// Bytes the guest has written to COM1 and the debug ports in total.
    pub fn serial_end(&self) -> u64 {
        self.bus.serial_dropped + self.bus.serial.len() as u64
    }
    /// Retained output from absolute position `from` on, and the position
    /// after it. A position older than the retained tail starts at its head.
    pub fn serial_since(&self, from: u64) -> (&[u8], u64) {
        let skip = from.saturating_sub(self.bus.serial_dropped);
        let skip = skip.min(self.bus.serial.len() as u64) as usize;
        (&self.bus.serial[skip..], self.serial_end())
    }

    /// Cumulative absent-device accesses, one latest record per address page.
    pub fn bus_warning_pages(&self) -> impl Iterator<Item = &BusWarning> {
        self.bus.diagnostics.pages.values()
    }

    /// Origin-page counts; empty without `bus-instruction-context` (enabled by
    /// `host`). Target-page counts and the bounded warning queue remain available.
    pub fn bus_warning_instruction_pages(&self) -> impl Iterator<Item = (&u32, &BusCounter)> {
        self.bus.diagnostics.instructions.iter()
    }

    /// The frontend drains this bounded queue into its shared log transcript.
    pub fn take_bus_warning(&mut self) -> Option<BusWarning> {
        self.bus.diagnostics.pending.pop_front()
    }

    pub fn load_bios_rom(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.clear_jit_code()?;
        self.cpu.invalidate_translation_context();
        self.bus.load_bios_rom(bytes)?;
        self.roms.push((0x10_0000 - bytes.len(), bytes.to_vec()));
        self.boot_image = Some(BootImage::Bios);
        Ok(())
    }

    /// C0000 is this PC's VGA ROM: also expose its source through fw_cfg for
    /// firmware that clears the legacy window before option-ROM discovery.
    pub fn load_option_rom(&mut self, base: usize, bytes: &[u8]) -> Result<(), String> {
        self.clear_jit_code()?;
        self.cpu.invalidate_translation_context();
        self.bus.load_option_rom(base, bytes)?;
        self.roms.push((base, bytes.to_vec()));
        Ok(())
    }

    pub fn load_bootsector(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.clear_jit_code()?;
        self.cpu.invalidate_translation_context();
        self.bus.load_bootsector(bytes)?;
        self.boot_image = Some(BootImage::Sector(Box::new(bytes.try_into().unwrap())));
        Ok(())
    }

    pub fn reset_bios(&mut self) {
        self.cpu.reset_bios();
    }
    pub fn enter_bootsector(&mut self) {
        enter_bootsector(&mut self.cpu);
    }

    /// Reboot with the original boot inputs and current disk contents. The
    /// host step counter and serial transcript span resets; CPU time restarts.
    pub fn reboot(&mut self) -> Result<(), String> {
        if self.boot_reload_is_external() {
            return Err("preloaded boot assets must be restored by the bare-metal host".into());
        }
        self.clear_jit_code()?;
        let image = self
            .boot_image
            .as_ref()
            .ok_or("reset requested without a boot image")?;
        self.bus.reset(self.step);
        self.cpu = Cpu::new();
        for (base, bytes) in &self.roms {
            self.bus.ram[*base..*base + bytes.len()].copy_from_slice(bytes);
        }
        match image {
            BootImage::Bios => self.cpu.reset_bios(),
            BootImage::Sector(bytes) => {
                self.bus.ram[0x7c00..0x7e00].copy_from_slice(bytes.as_slice());
                enter_bootsector(&mut self.cpu);
            }
            #[cfg(feature = "host")]
            BootImage::Linux {
                kernel,
                initrd,
                cmdline,
                host_decompress_kernel,
                host_decompress_initrd,
                overlay_files,
            } => {
                self.bus.ram.fill(0);
                let mem_size = self.bus.ram.len() as u32;
                let boot = DirectLinuxBoot::load(
                    &mut self.bus.ram,
                    kernel,
                    initrd,
                    &BootConfig {
                        mem_size,
                        cmdline,
                        host_decompress_kernel: *host_decompress_kernel,
                        host_decompress_initrd: *host_decompress_initrd,
                        overlay_files,
                        dump_initrd_path: None,
                    },
                )?;
                self.cpu.enter_protected_flat(boot.entry, boot.boot_params);
            }
            #[cfg(not(feature = "host"))]
            BootImage::Preloaded => unreachable!(),
        }
        self.boots += 1;
        Ok(())
    }

    #[cfg(feature = "host")]
    pub fn load_linux(
        &mut self,
        kernel: &[u8],
        initrd: &[u8],
        config: &BootConfig<'_>,
    ) -> Result<LoadedBoot, String> {
        self.clear_jit_code()?;
        self.cpu.invalidate_translation_context();
        let boot = DirectLinuxBoot::load(&mut self.bus.ram, kernel, initrd, config)?;
        self.cpu.enter_protected_flat(boot.entry, boot.boot_params);
        self.boot_image = Some(BootImage::Linux {
            kernel: kernel.to_vec(),
            initrd: initrd.to_vec(),
            cmdline: config.cmdline.to_string(),
            host_decompress_kernel: config.host_decompress_kernel,
            host_decompress_initrd: config.host_decompress_initrd,
            overlay_files: config.overlay_files.to_vec(),
        });
        Ok(boot)
    }

    pub fn attach_primary_cdrom(&mut self, image: Vec<u8>) {
        self.bus.attach_primary_cdrom(image);
    }

    pub fn attach_secondary_cdrom(&mut self, image: Vec<u8>) {
        self.bus.attach_secondary_cdrom(image);
    }
    /// Attach a host ISO file as the optical drive; sectors are read on demand.
    #[cfg(feature = "host")]
    pub fn attach_file_cdrom(&mut self, file: std::fs::File, primary: bool) -> std::io::Result<()> {
        self.bus.ide.attach_file_cdrom(file, primary)
    }
    #[cfg(feature = "host")]
    pub fn change_file_cdrom(&mut self, file: std::fs::File) -> Result<(), String> {
        self.bus.ide.change_file_cdrom(file)
    }
    pub fn cdrom_state(&self) -> Option<CdromState> {
        self.bus.ide.cdrom_state()
    }
    /// Change the attached optical drive's medium. Empty bytes eject the disc.
    pub fn change_cdrom(&mut self, image: Vec<u8>) -> Result<(), &'static str> {
        self.bus.ide.change_cdrom(image)
    }
    /// Whether `change_cdrom` would accept a new medium now.
    pub fn cdrom_change_allowed(&self) -> Result<(), &'static str> {
        self.bus.ide.cdrom_change_allowed()
    }
    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    pub fn attach_browser_cdrom(&mut self, drive: u32, bytes: usize, primary: bool) {
        self.bus.ide.attach_browser_cdrom(drive, bytes, primary);
    }
    pub fn attach_hdd(&mut self, image: Vec<u8>, writable: bool) {
        self.bus.attach_hdd(image, writable);
    }

    /// Attach an already-open sector device as primary master. Its ownership
    /// (including any host file lock) stays with the machine across guest resets.
    pub fn attach_disk(&mut self, disk: crate::disk::Disk) {
        self.bus.ide.attach_primary_master_disk(disk);
    }

    /// Call before dropping a machine with writable host disks. Guest ATA FLUSH
    /// uses the same sync path; Drop cannot report a persistence failure.
    pub fn flush_disks(&mut self) -> Result<(), String> {
        self.bus
            .ide
            .flush_disks()
            .map_err(|error| error.to_string())
    }

    /// Attach the fixed PCI RTL8029 before boot. Host networking is separate.
    pub fn attach_network(&mut self) {
        self.bus.net = Some(Ne2000::default());
        self.bus.pci.net_present = true;
        let elcr = self.bus.pic.read(0x4d1);
        self.bus.pic.write(0x4d1, elcr | (1 << (NET_IRQ - 8)));
        self.bus.sync_pci_irq();
    }

    /// Opt in before boot: PIIX3 UHCI and one full-speed HID absolute tablet.
    pub fn attach_usb_tablet(&mut self) {
        self.bus.usb = Some(Uhci::default());
        self.bus.pci.usb_present = true;
        let elcr = self.bus.pic.read(0x4d1);
        self.bus.pic.write(0x4d1, elcr | (1 << (USB_IRQ - 8)));
        self.bus.set_attention();
    }

    /// Absolute coordinates 0..=TABLET_MAX; buttons are left/right/middle bits
    /// 0/1/2. Wheel is signed HID motion (-127..=127). Call at a slice boundary.
    pub fn inject_usb_tablet(
        &mut self,
        x: u16,
        y: u16,
        buttons: u8,
        wheel: i8,
    ) -> Result<(), String> {
        if x > TABLET_MAX || y > TABLET_MAX || buttons & !7 != 0 || wheel == i8::MIN {
            return Err("USB tablet input outside logical range".into());
        }
        self.bus.advance_usb(self.bus.clock.ticks(self.step));
        let usb = self.bus.usb.as_mut().ok_or("USB tablet is disabled")?;
        usb.input(x, y, buttons, wheel);
        self.bus.sync_pci_irq();
        self.bus.set_attention();
        Ok(())
    }

    /// External input at a frontend slice boundary. False means keep queued.
    pub fn receive_network_frame(&mut self, frame: &[u8]) -> bool {
        let accepted = self.bus.net.as_mut().is_some_and(|nic| nic.receive(frame));
        self.bus.sync_pci_irq();
        accepted
    }

    pub fn take_network_transmit(&mut self) -> Option<Vec<u8>> {
        let frame = self.bus.net.as_mut()?.take_transmit();
        self.bus.sync_pci_irq();
        frame
    }

    /// Enable fixed-rate stereo delivery; guest sound hardware is always present.
    pub fn enable_audio(&mut self) {
        self.bus.sound.enable(self.bus.clock.ticks(self.step));
        self.bus.set_attention();
    }

    /// Stop delivery (muted output); the guest sees no difference.
    pub fn disable_audio(&mut self) {
        self.bus.sound.disable();
    }

    pub fn drain_audio(&mut self, out: &mut Vec<i16>) {
        self.bus
            .sound
            .advance(self.bus.clock.ticks(self.step), &self.bus.ram);
        self.bus.sound.drain(out);
        self.bus.sync_sound_irq();
    }

    pub fn audio_dropped_frames(&self) -> u64 {
        self.bus.sound.dropped_frames()
    }

    pub fn queue_serial_input(&mut self, bytes: &[u8]) {
        self.bus.queue_serial_input(bytes);
    }

    pub fn inject_ascii(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.bus.ps2.inject_ascii(byte);
        }
        self.bus.drain_ps2_effects();
    }

    pub fn keyboard_ready_for_text(&self) -> bool {
        self.bus.ps2.keyboard_ready_for_text()
    }

    pub fn inject_key(&mut self, key: Ps2Key, down: bool) {
        self.bus.ps2.inject_key(key, down);
        self.bus.drain_ps2_effects();
    }

    pub fn inject_scancode_set2(&mut self, bytes: &[u8]) {
        self.bus.ps2.inject_scancode_set2(bytes);
        self.bus.drain_ps2_effects();
    }

    pub fn inject_mouse(&mut self, dx: i16, dy: i16, buttons: Ps2ButtonState) {
        self.bus.ps2.inject_mouse(dx, dy, buttons);
        self.bus.drain_ps2_effects();
    }

    pub fn clear_frame_dirty(&mut self) {
        self.bus.vga.clear_dirty();
        self.bus.sync_ioram(&mut self.cpu);
    }
    pub fn clear_frame_dirty_for(&mut self, viewer: crate::devices::Viewer) {
        self.bus.vga.clear_frame_dirty_for(viewer);
        self.bus.sync_ioram(&mut self.cpu);
    }
    pub fn text_snapshot(&self) -> String {
        vga_text_snapshot_string(&self.bus)
    }

    pub fn set_diagnostics(&mut self, trace: TraceMode, perf: PerfMode) {
        self.trace = match trace {
            TraceMode::Off => TraceStore::None(NullTrace),
            TraceMode::Minimal => TraceStore::Minimal(MinimalTraceRing::new()),
            TraceMode::Full => TraceStore::Full(TraceRing::new()),
        };
        self.perf = match perf {
            PerfMode::Disabled => PerfStore::Disabled,
            PerfMode::Enabled => PerfStore::Enabled(PerfCounters::default()),
        };
    }

    /// Select wall-time devices before starting the CPU. Instruction budgets
    /// then count CPU work only; an idle CPU yields instead of advancing time.
    #[cfg(any(
        feature = "host",
        all(feature = "web-frontend", target_arch = "wasm32")
    ))]
    pub fn use_realtime_clock(&mut self) -> Result<(), String> {
        if self.step != 0 {
            return Err("select the clock before running the machine".into());
        }
        #[cfg(feature = "host")]
        {
            // The RTC stores a wall calendar without a timezone. Seed local
            // time once; monotonic ticks and guest writes advance it afterward.
            let local = chrono::Local::now();
            self.bus.magic.utc_seconds = u64::try_from(local.timestamp())
                .map_err(|_| "host time predates 1970".to_string())?;
            self.bus.magic.utc_offset = local.offset().local_minus_utc();
            self.bus.rtc_epoch_seconds =
                u64::try_from(local.naive_local().and_utc().timestamp())
                    .map_err(|_| "host RTC: local date predates 1970".to_string())?;
        }
        #[cfg(all(
            not(feature = "host"),
            feature = "web-frontend",
            target_arch = "wasm32"
        ))]
        {
            self.bus.rtc_epoch_seconds = crate::browser::epoch_seconds();
            self.bus.magic.utc_seconds = self.bus.rtc_epoch_seconds;
        }
        self.bus.rtc_epoch_tick = 0;
        self.bus.clock = Clock::realtime();
        self.bus.set_attention();
        Ok(())
    }

    pub fn uses_realtime_clock(&self) -> bool {
        self.bus.clock.is_realtime()
    }

    /// Absolute input for the magic instruction pointer command. See its
    /// protocol for queue capacity and retry behavior.
    pub fn inject_pointer(
        &mut self,
        x: u16,
        y: u16,
        buttons: u8,
        wheel: i16,
    ) -> Result<(), String> {
        self.bus
            .magic
            .pointer
            .inject(x, y, buttons, wheel, self.bus.clock.ticks(self.step))
            .map_err(String::from)
    }

    /// Frontends suppress PS/2 injection and capture while the tool is polling.
    pub fn guest_pointer_active(&self) -> bool {
        self.bus
            .magic
            .pointer
            .active(self.bus.clock.ticks(self.step))
    }

    /// Host wait after HLT, bounded so UI input remains responsive. Never used
    /// to limit a runnable CPU. Device deadlines use oscillator ticks.
    #[cfg(feature = "host")]
    pub fn halted_wait_duration(&self) -> std::time::Duration {
        self.bus.clock.wait_duration(self.bus.next_device_event())
    }

    /// Advance a deterministic HLT wait to the next device event or the caller's
    /// budget. The latter can represent replay input even with all timers off.
    pub fn advance_halted(&mut self, budget: u64) -> bool {
        if self.powered_off() || !self.cpu.halted || self.bus.clock.is_realtime() || budget == 0 {
            return false;
        }
        let wake = self
            .bus
            .next_attention_step
            .min(self.step.saturating_add(budget));
        let elapsed = wake.saturating_sub(self.step);
        self.step += elapsed;
        self.cpu.advance_tsc(elapsed);
        true
    }

    /// Read diagnostic code bytes at a linear address. A CPU copy retains the
    /// normal MMU's A/D effects without changing the current CPU state.
    pub fn guest_code_bytes(&mut self, start: u32, out: &mut [Option<u8>], stop_on_fault: bool) {
        let mut probe = self.cpu.clone();
        out.fill(None);
        for (offset, byte) in out.iter_mut().enumerate() {
            *byte = mmu::read_linear_u8(
                &mut probe,
                &mut self.bus,
                LinearAddr(start.wrapping_add(offset as u32)),
                AccessKind::Execute,
            )
            .ok();
            if stop_on_fault && byte.is_none() {
                break;
            }
        }
    }

    pub fn guest_stack_words(&mut self, out: &mut [Option<u32>]) {
        let mut probe = self.cpu.clone();
        let start = self.cpu.reg(Reg32::Esp);
        for (index, word) in out.iter_mut().enumerate() {
            *word = mmu::read_linear_u32(
                &mut probe,
                &mut self.bus,
                LinearAddr(start.wrapping_add(index as u32 * 4)),
                AccessKind::Read,
            )
            .ok();
        }
    }

    pub fn device_status(&self) -> DeviceStatus {
        let bus = &self.bus;
        DeviceStatus {
            pic1_mask: bus.pic.mask(false),
            pic2_mask: bus.pic.mask(true),
            pic1_in_service: bus.pic.in_service(false),
            pic2_in_service: bus.pic.in_service(true),
            pic1_vector_base: bus.pic.vector_base(false),
            pic2_vector_base: bus.pic.vector_base(true),
            ps2_irq1: bus.pic.line(1),
            ps2_irq12: bus.pic.line(12),
            fdc_irq6: bus.fdc.irq_asserted(),
            rtc_irq8: bus.rtc_irq8,
            cmos_status_a: bus.rtc_status_a(),
            cmos_status_b: bus.cmos_status_b,
            cmos_status_c: bus.cmos_status_c,
            rtc_next_tick: bus.rtc_next_tick,
            keyboard_dropped: bus.ps2.keyboard_dropped(),
            mouse_dropped: bus.ps2.mouse_dropped(),
            fdc_dor: bus.fdc.dor,
            fdc_status: bus.fdc.main_status(),
            reset_requested: bus.reset_requested,
        }
    }

    fn from_parts(cpu: Cpu, bus: HostBus, trace_mode: TraceMode, perf_mode: PerfMode) -> Self {
        let mut machine = Self {
            cpu,
            bus,
            trace: TraceStore::None(NullTrace),
            perf: PerfStore::Disabled,
            step: 0,
            boots: 0,
            boot_image: None,
            roms: Vec::new(),
            #[cfg(any(
                all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
                all(feature = "jit", target_arch = "arm")
            ))]
            jit: None,
            #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
            chip_jit: None,
            #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
            wasm_jit: None,
        };
        machine.set_diagnostics(trace_mode, perf_mode);
        machine
    }

    /// Select the optional ARM64 Linux or ARMv6K engine. Unsupported hosts fail explicitly;
    /// selecting JIT must never silently select the interpreter instead.
    pub fn set_jit_enabled(&mut self, enabled: bool) -> Result<(), String> {
        self.clear_jit_code()?;
        #[cfg(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        ))]
        {
            self.jit = if enabled {
                Some(crate::x86::jit::runtime::Engine::new()?)
            } else {
                None
            };
            Ok(())
        }
        #[cfg(not(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        )))]
        if enabled {
            Err(
                "the JIT engine requires an ARM64 Linux or 32-bit ARM build with --features jit"
                    .into(),
            )
        } else {
            Ok(())
        }
    }

    #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
    pub fn set_chip_jit_enabled(&mut self, enabled: bool) -> Result<(), String> {
        #[cfg(feature = "r86chip-native")]
        if enabled {
            return Err(
                "build without r86chip-native to run the separate B1 JIT experiment".into(),
            );
        }
        if enabled
            && !matches!(
                (&self.trace, &self.perf),
                (TraceStore::None(_), PerfStore::Disabled)
            )
        {
            return Err(
                "B1 JIT requires instruction tracing/perf diagnostics to be disabled".into(),
            );
        }
        self.clear_jit_code()?;
        self.chip_jit = enabled.then(crate::chipjit::runtime::Engine::new);
        Ok(())
    }

    #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
    pub fn chip_jit_stats(&self) -> Option<&crate::chipjit::runtime::Stats> {
        self.chip_jit.as_ref().map(|engine| &engine.stats)
    }

    pub fn jit_report(&self) -> String {
        #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
        if let Some(jit) = &self.wasm_jit {
            return jit.report();
        }
        #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
        if let Some(jit) = &self.chip_jit {
            return format!("chipjit: {:?}\n", jit.stats);
        }
        #[cfg(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        ))]
        if let Some(jit) = &self.jit {
            return jit.report();
        }
        String::new()
    }

    /// Drain diagnostic observations between serialized Wasm core calls.
    #[cfg(all(feature = "wasm-jit", feature = "jit-profile", target_arch = "wasm32"))]
    pub fn take_wasm_jit_trace(&mut self) -> String {
        self.wasm_jit
            .as_mut()
            .map_or_else(String::new, |jit| jit.take_trace())
    }

    fn clear_jit_code(&mut self) -> Result<(), String> {
        #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
        if let Some(jit) = &mut self.wasm_jit {
            jit.clear(&mut self.bus);
        }
        #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
        if let Some(jit) = &mut self.chip_jit {
            jit.clear(&mut self.bus);
        }
        #[cfg(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        ))]
        if let Some(jit) = &mut self.jit {
            jit.clear(&mut self.bus)?;
        }
        #[cfg(feature = "jit")]
        {
            self.bus.jit_code_pages = None;
        }
        Ok(())
    }

    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
    pub fn configure_wasm_jit(&mut self, mib: u32, threshold: u32) -> Result<(), String> {
        if !(40..=1024).contains(&mib) || !(1..=65534).contains(&threshold) {
            return Err(
                "Wasm JIT requires a 40..1024 MiB cache and heat threshold 1..65534".into(),
            );
        }
        if !matches!(
            (&self.trace, &self.perf),
            (TraceStore::None(_), PerfStore::Disabled)
        ) {
            return Err("Wasm JIT requires instruction tracing/perf to be disabled".into());
        }
        self.clear_jit_code()?;
        self.wasm_jit = Some(crate::x86::jit::wasm::runtime::Engine::new(
            mib as usize * 1024 * 1024,
            threshold as u16,
        ));
        Ok(())
    }

    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
    pub fn complete_wasm_jit(&mut self, request: u32, function: u32) -> bool {
        self.wasm_jit
            .as_mut()
            .is_some_and(|jit| jit.complete(&mut self.cpu, &mut self.bus, request, function))
    }

    pub fn step_once(&mut self) -> Result<GuestStepStatus, String> {
        self.bus.sample_device_clock(self.step);
        self.bus.sync_ioram(&mut self.cpu);
        if self.powered_off() {
            self.flush_disks()?;
            return Ok(GuestStepStatus::PoweredOff);
        }
        let status = match (&mut self.trace, &mut self.perf) {
            (TraceStore::None(trace), PerfStore::Disabled) => {
                let mut perf = NoPerfCounters;
                step_guest_once(&mut self.cpu, &mut self.bus, trace, &mut perf, self.step)?
            }
            (TraceStore::None(trace), PerfStore::Enabled(perf)) => {
                step_guest_once(&mut self.cpu, &mut self.bus, trace, perf, self.step)?
            }
            (TraceStore::Minimal(trace), PerfStore::Disabled) => {
                let mut perf = NoPerfCounters;
                step_guest_once(&mut self.cpu, &mut self.bus, trace, &mut perf, self.step)?
            }
            (TraceStore::Minimal(trace), PerfStore::Enabled(perf)) => {
                step_guest_once(&mut self.cpu, &mut self.bus, trace, perf, self.step)?
            }
            (TraceStore::Full(trace), PerfStore::Disabled) => {
                let mut perf = NoPerfCounters;
                step_guest_once(&mut self.cpu, &mut self.bus, trace, &mut perf, self.step)?
            }
            (TraceStore::Full(trace), PerfStore::Enabled(perf)) => {
                step_guest_once(&mut self.cpu, &mut self.bus, trace, perf, self.step)?
            }
        };
        if let PerfStore::Enabled(perf) = &mut self.perf {
            perf.guest_instruction();
        }
        self.step = self.step.wrapping_add(1);
        if matches!(status, GuestStepStatus::PoweredOff) {
            self.flush_disks()?;
        }
        if matches!(status, GuestStepStatus::Reset) && !self.boot_reload_is_external() {
            self.reboot()?;
            return Ok(GuestStepStatus::Continue);
        }
        Ok(status)
    }

    pub fn run_steps(&mut self, budget: u64) -> Result<RunReport, String> {
        self.run_steps_impl::<false>(budget)
    }

    /// Run until the first newly emitted serial output, a machine stop, or the
    /// budget. Serial matchers can observe exact I/O boundaries without forcing
    /// every ordinary instruction through `step_once` (or bypassing the JIT).
    pub fn run_until_serial_output(&mut self, budget: u64) -> Result<RunReport, String> {
        self.run_steps_impl::<true>(budget)
    }

    fn run_steps_impl<const YIELD_SERIAL: bool>(
        &mut self,
        budget: u64,
    ) -> Result<RunReport, String> {
        self.bus.sample_device_clock(self.step);
        let start_step = self.step;
        if self.powered_off() {
            self.flush_disks()?;
            return Ok(RunReport {
                start_step,
                end_step: start_step,
                ran: 0,
                stop: Some(GuestStepStatus::PoweredOff),
            });
        }
        let mut ran = 0;
        loop {
            let report = self.run_until_stop::<YIELD_SERIAL>(budget - ran)?;
            ran += report.ran;
            if matches!(report.stop, Some(GuestStepStatus::Reset))
                && !self.boot_reload_is_external()
            {
                self.reboot()?;
            } else {
                if matches!(report.stop, Some(GuestStepStatus::PoweredOff)) {
                    self.flush_disks()?;
                }
                return Ok(RunReport {
                    start_step,
                    end_step: self.step,
                    ran,
                    stop: report.stop,
                });
            }
        }
    }

    fn run_until_stop<const YIELD_SERIAL: bool>(
        &mut self,
        budget: u64,
    ) -> Result<RunReport, String> {
        self.bus.sync_ioram(&mut self.cpu);
        #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
        if self.chip_jit.is_some()
            && !matches!(
                (&self.trace, &self.perf),
                (TraceStore::None(_), PerfStore::Disabled)
            )
        {
            return Err("B1 JIT instruction tracing/perf diagnostics are not implemented".into());
        }
        #[cfg(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        ))]
        if self.jit.is_some()
            && !matches!(
                (&self.trace, &self.perf),
                (TraceStore::None(_), PerfStore::Disabled)
            )
        {
            return Err("JIT instruction tracing and interpreter perf counters are not implemented; select --cpu-engine interp for those diagnostics".into());
        }
        match (&mut self.trace, &mut self.perf) {
            (TraceStore::None(_), PerfStore::Disabled) => {
                run_steps_no_trace_no_perf::<YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    &mut self.step,
                    budget,
                    #[cfg(any(
                        all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
                        all(feature = "jit", target_arch = "arm")
                    ))]
                    self.jit.as_mut(),
                    #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
                    self.chip_jit.as_mut(),
                    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
                    self.wasm_jit.as_mut(),
                )
            }
            (TraceStore::None(trace), PerfStore::Enabled(perf)) => {
                run_steps_inner::<_, _, YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    trace,
                    perf,
                    &mut self.step,
                    budget,
                )
            }
            (TraceStore::Minimal(trace), PerfStore::Disabled) => {
                let mut perf = NoPerfCounters;
                run_steps_inner::<_, _, YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    trace,
                    &mut perf,
                    &mut self.step,
                    budget,
                )
            }
            (TraceStore::Minimal(trace), PerfStore::Enabled(perf)) => {
                run_steps_inner::<_, _, YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    trace,
                    perf,
                    &mut self.step,
                    budget,
                )
            }
            (TraceStore::Full(trace), PerfStore::Disabled) => {
                let mut perf = NoPerfCounters;
                run_steps_inner::<_, _, YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    trace,
                    &mut perf,
                    &mut self.step,
                    budget,
                )
            }
            (TraceStore::Full(trace), PerfStore::Enabled(perf)) => {
                run_steps_inner::<_, _, YIELD_SERIAL>(
                    &mut self.cpu,
                    &mut self.bus,
                    trace,
                    perf,
                    &mut self.step,
                    budget,
                )
            }
        }
    }

    pub fn trace_report(&self) -> String {
        self.trace.report()
    }

    pub fn perf_report(&self) -> Option<String> {
        match &self.perf {
            PerfStore::Enabled(perf) => Some(perf.report(self.step)),
            PerfStore::Disabled => None,
        }
    }
}

/// A diagnostic snapshot, collected only when requested by the frontend.
pub struct DeviceStatus {
    pub pic1_mask: u8,
    pub pic2_mask: u8,
    pub pic1_in_service: u8,
    pub pic2_in_service: u8,
    pub pic1_vector_base: u8,
    pub pic2_vector_base: u8,
    pub rtc_irq8: bool,
    pub cmos_status_a: u8,
    pub cmos_status_b: u8,
    pub cmos_status_c: u8,
    pub rtc_next_tick: u64,
    pub ps2_irq1: bool,
    pub ps2_irq12: bool,
    pub keyboard_dropped: u64,
    pub mouse_dropped: u64,
    pub fdc_irq6: bool,
    pub fdc_dor: u8,
    pub fdc_status: u8,
    pub reset_requested: bool,
}

/// Frozen evidence from the most recent triple fault, retained across reset.
/// No native CPU/Rust layout is exposed as a snapshot ABI.
pub struct CpuShutdown {
    pub step: u64,
    pub error: StepError,
    pub cpu_dump: String,
    pub vga_dump: String,
    pub serial: Vec<u8>,
}

fn run_steps_no_trace_no_perf<const YIELD_SERIAL: bool>(
    cpu: &mut Cpu,
    bus: &mut HostBus,
    step: &mut u64,
    budget: u64,
    #[cfg(any(
        all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
        all(feature = "jit", target_arch = "arm")
    ))]
    mut jit: Option<&mut crate::x86::jit::runtime::Engine>,
    #[cfg(all(feature = "r86chip", target_arch = "riscv32"))] mut chip_jit: Option<
        &mut crate::chipjit::runtime::Engine,
    >,
    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))] mut wasm_jit: Option<
        &mut crate::x86::jit::wasm::runtime::Engine,
    >,
) -> Result<RunReport, String> {
    let start_step = *step;
    let serial_start = if YIELD_SERIAL { bus.serial.len() } else { 0 };
    let mut ran = 0u64;
    let mut stop = None;
    while ran < budget {
        let was_halted = cpu.halted;
        #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
        let batch = if let Some(jit) = wasm_jit.as_deref_mut() {
            jit.run_batch(cpu, bus, *step, budget - ran)?
        } else {
            Interpreter::run_batch(cpu, bus, *step, budget - ran)
        };
        #[cfg(any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        ))]
        let batch = if let Some(jit) = jit.as_deref_mut() {
            jit.run_batch(cpu, bus, *step, budget - ran)?
        } else {
            Interpreter::run_batch(cpu, bus, *step, budget - ran)
        };
        #[cfg(all(feature = "r86chip", target_arch = "riscv32"))]
        let batch = if let Some(jit) = chip_jit.as_deref_mut() {
            jit.run_batch(cpu, bus, *step, budget - ran)
        } else {
            Interpreter::run_batch(cpu, bus, *step, budget - ran)
        };
        #[cfg(not(any(
            all(feature = "wasm-jit", target_arch = "wasm32"),
            all(feature = "r86chip", target_arch = "riscv32"),
            any(
                all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
                all(feature = "jit", target_arch = "arm")
            )
        )))]
        let batch = Interpreter::run_batch(cpu, bus, *step, budget - ran);
        if was_halted {
            // The interpreter executes no instruction while halted; the
            // machine's deterministic clock still advances during this tick.
            cpu.advance_tsc(batch.ran);
        }
        *step = (*step).wrapping_add(batch.ran);
        ran = ran.wrapping_add(batch.ran);
        let status = match batch.stop {
            BatchStop::Budget => break,
            BatchStop::Attention {
                instruction_step,
                was_interruptible,
            } => handle_running_attention(
                cpu,
                bus,
                &mut NoPerfCounters,
                instruction_step,
                was_interruptible,
            )?,
            BatchStop::Step {
                instruction_step,
                instruction_eip,
                was_interruptible,
                reason,
            } => {
                let result = match reason {
                    BatchStepStop::Halted => Ok(StepOutcome::Halted),
                    BatchStepStop::Attention => unreachable!(),
                    BatchStepStop::Error(err) => Err(err),
                };
                handle_non_running_step(
                    cpu,
                    bus,
                    &mut NoPerfCounters,
                    instruction_step,
                    instruction_eip,
                    was_interruptible,
                    result,
                )?
            }
        };
        if !matches!(status, GuestStepStatus::Continue) {
            if matches!(status, GuestStepStatus::Halted)
                && fast_forward_halted(cpu, bus, step, &mut ran, budget)
            {
                continue;
            }
            stop = Some(status);
            break;
        }
        if YIELD_SERIAL && bus.serial.len() != serial_start {
            break;
        }
    }
    Ok(RunReport {
        start_step,
        end_step: *step,
        ran,
        stop,
    })
}

fn run_steps_inner<T: TraceSink, P: PerfSink, const YIELD_SERIAL: bool>(
    cpu: &mut Cpu,
    bus: &mut HostBus,
    trace: &mut T,
    perf: &mut P,
    step: &mut u64,
    budget: u64,
) -> Result<RunReport, String> {
    let start_step = *step;
    let serial_start = if YIELD_SERIAL { bus.serial.len() } else { 0 };
    let mut ran = 0u64;
    let mut stop = None;
    while ran < budget {
        let status = step_guest_once(cpu, bus, trace, perf, *step)?;
        *step = (*step).wrapping_add(1);
        perf.guest_instruction();
        ran = ran.wrapping_add(1);
        if !matches!(status, GuestStepStatus::Continue) {
            if matches!(status, GuestStepStatus::Halted)
                && fast_forward_halted(cpu, bus, step, &mut ran, budget)
            {
                continue;
            }
            stop = Some(status);
            break;
        }
        if YIELD_SERIAL && bus.serial.len() != serial_start {
            break;
        }
    }
    if stop.is_some() {
        perf.batch_stop_exit();
    } else if ran == budget {
        perf.batch_budget_exit();
    }
    Ok(RunReport {
        start_step,
        end_step: *step,
        ran,
        stop,
    })
}

#[derive(Debug)]
pub struct RunReport {
    pub start_step: u64,
    pub end_step: u64,
    pub ran: u64,
    pub stop: Option<GuestStepStatus>,
}

#[derive(Debug)]
pub enum GuestStepStatus {
    Continue,
    Halted,
    Reset,
    PoweredOff,
    Stopped { message: String, eip: Option<u32> },
}

fn fast_forward_halted(
    cpu: &mut Cpu,
    bus: &HostBus,
    step: &mut u64,
    ran: &mut u64,
    budget: u64,
) -> bool {
    if *ran >= budget || bus.clock.is_realtime() {
        return false;
    }
    let wake_step = bus.next_attention_step;
    if wake_step == u64::MAX {
        return false;
    }
    if wake_step <= *step {
        return true;
    }
    let remaining = budget - *ran;
    let delta = wake_step - *step;
    let elapsed = delta.min(remaining);
    *step += elapsed;
    *ran += elapsed;
    cpu.advance_tsc(elapsed);
    true
}

fn handle_running_attention<P: PerfSink>(
    cpu: &mut Cpu,
    bus: &mut HostBus,
    perf: &mut P,
    step: u64,
    was_interruptible: bool,
) -> Result<GuestStepStatus, String> {
    if bus.reset_requested {
        perf.reset_exit();
        return Ok(GuestStepStatus::Reset);
    }
    if bus.acpi.powered_off {
        return Ok(GuestStepStatus::PoweredOff);
    }
    if interrupt_attention_needed(step, was_interruptible, cpu, bus) {
        perf.attention_irq_poll();
        if poll_pending_external_irq(step, cpu, bus, perf).is_some() {
            let return_eip = cpu.eip;
            perf.actual_irq_delivery();
            deliver_irq(cpu, bus, return_eip)?;
            if bus.reset_requested {
                perf.reset_exit();
                return Ok(GuestStepStatus::Reset);
            }
            bus.refresh_interrupt_attention(step);
        }
    }
    Ok(GuestStepStatus::Continue)
}

#[cold]
fn handle_non_running_step<P: PerfSink>(
    cpu: &mut Cpu,
    bus: &mut HostBus,
    perf: &mut P,
    step: u64,
    step_eip: u32,
    was_interruptible: bool,
    result: Result<StepOutcome, StepError>,
) -> Result<GuestStepStatus, String> {
    bus.current_step = step;
    match result {
        Ok(StepOutcome::Running | StepOutcome::Attention) => unreachable!(),
        Ok(StepOutcome::Halted) => {
            if poll_pending_external_irq(step, cpu, bus, perf).is_some() {
                cpu.halted = false;
                let return_eip = cpu.eip;
                perf.actual_irq_delivery();
                deliver_irq(cpu, bus, return_eip)?;
                bus.refresh_interrupt_attention(step);
            } else {
                bus.refresh_interrupt_attention(step);
                perf.halted_exit();
                return Ok(GuestStepStatus::Halted);
            }
        }
        Err(StepError::SoftwareInterrupt {
            vector,
            return_eip,
            fault_eip,
        }) => {
            let result = deliver_software_interrupt_event(cpu, bus, vector, return_eip, fault_eip);
            finish_event_delivery(cpu, bus, result)?;
        }
        Err(err) => {
            if let Some((vector, code, eip)) = err.exception(step_eip) {
                let result = deliver_exception(cpu, bus, vector, code, eip);
                finish_event_delivery(cpu, bus, result)?;
            } else {
                perf.stopped_exit();
                return Ok(GuestStepStatus::Stopped {
                    eip: error_eip(&err),
                    message: format!("{err:?}"),
                });
            }
        }
    }
    handle_running_attention(cpu, bus, perf, step, was_interruptible)
}

fn step_guest_once<T: TraceSink, P: PerfSink>(
    cpu: &mut Cpu,
    bus: &mut HostBus,
    trace: &mut T,
    perf: &mut P,
    step: u64,
) -> Result<GuestStepStatus, String> {
    bus.current_step = step;
    let step_eip = cpu.eip;
    let was_interruptible = cpu.maskable_interrupts_enabled();
    if cpu.halted {
        cpu.advance_tsc(1);
    }
    trace.push(step, cpu, &bus.ram, perf);
    let result = Interpreter::step(cpu, bus);
    if matches!(result, Ok(StepOutcome::Running | StepOutcome::Attention)) {
        handle_running_attention(cpu, bus, perf, step, was_interruptible)
    } else {
        handle_non_running_step(cpu, bus, perf, step, step_eip, was_interruptible, result)
    }
}

struct NullTrace;

impl NullTrace {
    fn report(&self) -> String {
        "guest instruction trace disabled; re-run with --minimal-ring or --debug-ring\n".to_string()
    }
}

impl TraceSink for NullTrace {
    #[inline(always)]
    fn push<P: PerfSink>(&mut self, _step: u64, _cpu: &Cpu, _ram: &[u8], _perf: &mut P) {}
}

#[derive(Clone, Copy)]
struct MinimalTraceEntry {
    step: u64,
    cs: u16,
    eip: u32,
}

struct MinimalTraceRing {
    entries: [MinimalTraceEntry; 32],
    next: usize,
    full: bool,
}

impl MinimalTraceRing {
    fn new() -> Self {
        let zero = MinimalTraceEntry {
            step: 0,
            cs: 0,
            eip: 0,
        };
        Self {
            entries: [zero; 32],
            next: 0,
            full: false,
        }
    }

    fn report(&self) -> String {
        let mut out = String::new();
        let count = if self.full {
            self.entries.len()
        } else {
            self.next
        };
        let start = if self.full { self.next } else { 0 };
        let _ = writeln!(out, "last {count} guest instruction addresses before stop:");
        for i in 0..count {
            let entry = self.entries[(start + i) % self.entries.len()];
            let _ = writeln!(
                out,
                "  step={} cs={:#06x} eip={:#010x}",
                entry.step, entry.cs, entry.eip
            );
        }
        let _ = writeln!(
            out,
            "re-run with --debug-ring for full register and guest-byte trace"
        );
        out
    }
}

impl TraceSink for MinimalTraceRing {
    #[inline(always)]
    fn push<P: PerfSink>(&mut self, step: u64, cpu: &Cpu, _ram: &[u8], perf: &mut P) {
        perf.minimal_trace_push();
        self.entries[self.next] = MinimalTraceEntry {
            step,
            cs: cpu.seg(SegmentReg::Cs).selector,
            eip: cpu.eip,
        };
        self.next = (self.next + 1) % self.entries.len();
        self.full |= self.next == 0;
    }
}

#[derive(Clone, Copy)]
struct TraceEntry {
    step: u64,
    cs: u16,
    eip: u32,
    linear: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
    esi: u32,
    edi: u32,
    ebp: u32,
    esp: u32,
    eflags: u32,
    bytes: [u8; 6],
    byte_len: usize,
}

struct TraceRing {
    entries: [TraceEntry; 16],
    next: usize,
    full: bool,
}

impl TraceRing {
    fn new() -> Self {
        let zero = TraceEntry {
            step: 0,
            cs: 0,
            eip: 0,
            linear: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            esi: 0,
            edi: 0,
            ebp: 0,
            esp: 0,
            eflags: 0,
            bytes: [0; 6],
            byte_len: 0,
        };
        Self {
            entries: [zero; 16],
            next: 0,
            full: false,
        }
    }

    fn push_full(&mut self, step: u64, cpu: &Cpu, ram: &[u8]) {
        let mut bytes = [0; 6];
        let cs = cpu.seg(SegmentReg::Cs);
        let linear = cs.base.wrapping_add(cpu.eip);
        let start = linear as usize;
        let byte_len = ram
            .get(start..start.saturating_add(bytes.len()).min(ram.len()))
            .map(|src| {
                bytes[..src.len()].copy_from_slice(src);
                src.len()
            })
            .unwrap_or(0);
        self.entries[self.next] = TraceEntry {
            step,
            cs: cs.selector,
            eip: cpu.eip,
            linear,
            eax: cpu.reg(Reg32::Eax),
            ebx: cpu.reg(Reg32::Ebx),
            ecx: cpu.reg(Reg32::Ecx),
            edx: cpu.reg(Reg32::Edx),
            esi: cpu.reg(Reg32::Esi),
            edi: cpu.reg(Reg32::Edi),
            ebp: cpu.reg(Reg32::Ebp),
            esp: cpu.reg(Reg32::Esp),
            eflags: cpu.effective_eflags(),
            bytes,
            byte_len,
        };
        self.next = (self.next + 1) % self.entries.len();
        self.full |= self.next == 0;
    }

    fn report(&self) -> String {
        let mut out = String::new();
        let count = if self.full {
            self.entries.len()
        } else {
            self.next
        };
        let start = if self.full { self.next } else { 0 };
        let _ = writeln!(out, "last {count} guest instructions before stop:");
        for i in 0..count {
            let entry = self.entries[(start + i) % self.entries.len()];
            let _ = writeln!(
                out,
                "  step={} cs={:#06x} eip={:#010x} linear={:#010x} bytes={} eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x} esi={:#010x} edi={:#010x} ebp={:#010x} esp={:#010x} eflags={:#010x}",
                entry.step,
                entry.cs,
                entry.eip,
                entry.linear,
                format_trace_bytes(&entry),
                entry.eax,
                entry.ebx,
                entry.ecx,
                entry.edx,
                entry.esi,
                entry.edi,
                entry.ebp,
                entry.esp,
                entry.eflags
            );
        }
        out
    }
}

impl TraceSink for TraceRing {
    #[inline(always)]
    fn push<P: PerfSink>(&mut self, step: u64, cpu: &Cpu, ram: &[u8], perf: &mut P) {
        perf.full_trace_push();
        self.push_full(step, cpu, ram);
    }
}

fn format_trace_bytes(entry: &TraceEntry) -> String {
    let mut out = String::new();
    for (index, byte) in entry.bytes[..entry.byte_len].iter().enumerate() {
        if index != 0 {
            out.push(' ');
        }
        use core::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn error_eip(err: &StepError) -> Option<u32> {
    match *err {
        StepError::UnsupportedOpcode { eip, .. }
        | StepError::UnsupportedOpcode2 { eip, .. }
        | StepError::UnsupportedModRm { eip, .. }
        | StepError::InvalidOpcode { eip }
        | StepError::DivideError { eip }
        | StepError::DeviceNotAvailable { eip }
        | StepError::BoundRangeExceeded { eip }
        | StepError::GeneralProtection { eip, .. }
        | StepError::InvalidTss { eip, .. }
        | StepError::StackFault { eip, .. }
        | StepError::SegmentNotPresent { eip, .. }
        | StepError::TripleFault { eip, .. }
        | StepError::TaskSwitchTrap { eip }
        | StepError::TaskSwitchFault { eip, .. }
        | StepError::TaskSwitchPageFault { eip, .. } => Some(eip),
        StepError::SoftwareInterrupt { fault_eip, .. } => Some(fault_eip),
        StepError::FaultOutOfRange { .. }
        | StepError::FaultMmio { .. }
        | StepError::FaultIoUnhandled { .. }
        | StepError::PageFault(_)
        | StepError::MemOutOfRange { .. }
        | StepError::MemMmio { .. }
        | StepError::MemIoUnhandled { .. } => None,
    }
}

fn vga_text_snapshot_string(bus: &HostBus) -> String {
    let snap = bus.vga.text_snapshot();
    let mut out = String::new();
    for row in 0..snap.rows as usize {
        if row != 0 {
            out.push('\n');
        }
        let line_start = out.len();
        if let Some(bytes) = snap.line_bytes(row) {
            for &byte in bytes {
                if (0x20..=0x7e).contains(&byte) {
                    out.push(byte as char);
                } else {
                    out.push(' ');
                }
            }
        }
        while out.len() > line_start && out.as_bytes().last() == Some(&b' ') {
            out.pop();
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TrackedWriteStats {
    total: u64,
    cpu: u64,
    page_walk: u64,
    device: u64,
    loader: u64,
    debug: u64,
    page_table_frame: u64,
}

// An enhanced AT controller with no attached floppy drives. Keep discovery,
// configuration and command/result/IRQ handshakes; media commands terminate
// with an error, never a fake transfer. No motor/seek timer or DMA engine is
// needed for this machine. Unsupported extended commands return invalid status.
// tiny386 supplies the minimal model; v86 floppy.js resolves result handshakes
// (result-phase IRQ clearing, four reset-sense replies) and Intel 82077AA
// sections 2.2/5.3 configuration retention across reset, which v86 keeps
// regardless of LOCK.
#[derive(Clone, Debug)]
struct NullFdc {
    dor: u8,
    dsr: u8,
    ccr: u8,
    command: [u8; 9],
    command_len: usize,
    command_needed: usize,
    result: [u8; 16],
    result_pos: usize,
    result_len: usize,
    reset_sense: u8,
    status0: u8,
    cylinder: [u8; 4],
    specify: [u8; 2],
    config: u8,
    pretrk: u8,
    perpendicular: u8,
    locked: bool,
    // Pending completion, before the DOR interrupt-enable gate.
    irq6: bool,
}

impl NullFdc {
    fn new() -> Self {
        Self {
            dor: 0x0c,
            dsr: 0,
            ccr: 0,
            command: [0; 9],
            command_len: 0,
            command_needed: 0,
            result: [0; 16],
            result_pos: 0,
            result_len: 0,
            reset_sense: 0,
            status0: 0x80,
            cylinder: [0; 4],
            specify: [0; 2],
            config: 0x20,
            pretrk: 0,
            perpendicular: 0,
            locked: false,
            irq6: false,
        }
    }

    fn irq_asserted(&self) -> bool {
        self.irq6 && self.dor & 0x0c == 0x0c
    }

    fn read_u8(&mut self, port: u16) -> Option<u8> {
        let value = match port {
            0x3f0 | 0x3f1 => 0xff,
            0x3f2 => self.dor,
            0x3f3 => 0x20,
            0x3f4 => self.main_status(),
            0x3f5 => self.read_data(),
            0x3f7 => 0x80, // No disk can clear the disk-change input.
            _ => return None,
        };
        Some(value)
    }

    fn write_u8(&mut self, port: u16, value: u8) -> bool {
        match port {
            0x3f0 | 0x3f1 | 0x3f3 => true,
            0x3f2 => {
                let was_normal = (self.dor & 0x04) != 0;
                let normal = (value & 0x04) != 0;
                self.dor = value;
                if !normal {
                    self.clear_command_and_result();
                    self.irq6 = false;
                    self.reset_sense = 0;
                    self.status0 = 0x80;
                } else if !was_normal {
                    self.start_reset();
                }
                true
            }
            0x3f4 => {
                self.dsr = value & 0x7f;
                // DSR resets the core, not the external motor/IRQ/drive latch.
                // Holding DOR reset still takes precedence over this pulse.
                if value & 0x80 != 0 && self.dor & 4 != 0 {
                    self.start_reset();
                }
                true
            }
            0x3f5 => {
                self.write_data(value);
                true
            }
            0x3f7 => {
                self.ccr = value & 0x03;
                true
            }
            _ => false,
        }
    }

    fn main_status(&self) -> u8 {
        if self.dor & 4 == 0 {
            FDC_MSR_BUSY
        } else if self.result_pos < self.result_len {
            FDC_MSR_RQM | FDC_MSR_DIO | FDC_MSR_BUSY
        } else if self.command_len != 0 {
            FDC_MSR_RQM | FDC_MSR_BUSY
        } else {
            FDC_MSR_RQM
        }
    }

    fn read_data(&mut self) -> u8 {
        if self.result_pos >= self.result_len {
            return 0xff;
        }
        let value = self.result[self.result_pos];
        self.result_pos += 1;
        if self.result_pos >= self.result_len {
            self.result_pos = 0;
            self.result_len = 0;
            // v86/QEMU lower IRQ when the complete result has been consumed.
            // PIC acknowledgement alone cannot acknowledge the device.
            self.irq6 = false;
        }
        value
    }

    fn write_data(&mut self, value: u8) {
        if self.dor & 4 == 0 || self.result_pos < self.result_len {
            return;
        }
        if self.command_len == 0 {
            self.command[0] = value;
            self.command_len = 1;
            self.command_needed = Self::command_size(value);
        } else if self.command_len < self.command.len() {
            self.command[self.command_len] = value;
            self.command_len += 1;
        }
        if self.command_len >= self.command_needed {
            self.execute_command();
        }
    }

    fn command_size(command: u8) -> usize {
        match Self::command_code(command) {
            0x03 | 0x0f => 3,
            0x04 | 0x07 | 0x0a | 0x12 => 2,
            0x05 | 0x06 | 0x09 | 0x0c | 0x11 | 0x19 | 0x1d => 9,
            0x0d => 6,
            0x13 => 4,
            _ => 1,
        }
    }

    fn command_code(command: u8) -> u8 {
        // Only data operations have MT/MFM/SK flags. Do not let an unknown
        // opcode alias VERSION, SENSE INTERRUPT or an absolute SEEK.
        match command {
            0x03 | 0x04 | 0x07 | 0x08 | 0x0e | 0x0f | 0x10 | 0x12 | 0x13 | 0x14 | 0x94 => command,
            _ => match command & 0x1f {
                0x05 | 0x09 if command & 0x20 == 0 => command & 0x1f,
                0x06 | 0x0c | 0x11 | 0x19 | 0x1d => command & 0x1f,
                0x0a | 0x0d if command & 0xa0 == 0 => command & 0x1f,
                _ => 0xff,
            },
        }
    }

    fn execute_command(&mut self) {
        let base = Self::command_code(self.command[0]);
        match base {
            0x03 => {
                self.specify = [self.command[1], self.command[2]];
                self.enter_idle();
            }
            0x12 => {
                let mode = self.command[1];
                if mode & 0x80 != 0 {
                    self.perpendicular = mode & 0x3f;
                } else {
                    self.perpendicular = (self.perpendicular & 0x3c) | (mode & 3);
                }
                self.enter_idle();
            }
            0x13 => {
                self.config = self.command[2] & 0x7f;
                self.pretrk = self.command[3];
                self.enter_idle();
            }
            0x04 => {
                // Enhanced ST3 bits 3/5 are fixed ones (also v86). An absent
                // drive supplies no TRACK0 or write-protect signal; v86 sets
                // TRACK0 at track zero even without media.
                self.set_result(&[0x28 | (self.command[1] & 7)]);
            }
            0x07 => {
                let drive = self.command[1] & 0x03;
                self.cylinder[drive as usize] = 0;
                self.status0 = 0x70 | drive; // Seek end, equipment check, abnormal.
                self.reset_sense = 0;
                self.irq6 = true;
                self.enter_idle();
            }
            0x08 => {
                let (drive, status0) = if self.reset_sense > 0 {
                    let drive = 4 - self.reset_sense;
                    self.reset_sense -= 1;
                    (drive, 0xc0 | drive)
                } else if self.irq6 {
                    (self.status0 & 0x03, self.status0)
                } else {
                    self.set_result(&[0x80]);
                    return;
                };
                self.irq6 = false;
                self.set_result(&[status0, self.cylinder[drive as usize]]);
            }
            0x0e => {
                let result = [
                    self.cylinder[0],
                    self.cylinder[1],
                    self.cylinder[2],
                    self.cylinder[3],
                    self.specify[0],
                    self.specify[1],
                    0, // No sectors: there is no drive/media geometry.
                    (u8::from(self.locked) << 7) | self.perpendicular,
                    self.config,
                    self.pretrk,
                ];
                self.set_result(&result);
            }
            0x0f => {
                let drive = self.command[1] & 0x03;
                let head = (self.command[1] >> 2) & 1;
                // SEEK counts step pulses; unlike recalibration, it does not
                // require TRACK0 feedback from a drive (also v86, whose
                // RECALIBRATE also succeeds without media).
                self.cylinder[drive as usize] = self.command[2];
                self.status0 = 0x20 | (head << 2) | drive;
                self.reset_sense = 0;
                self.irq6 = true;
                self.enter_idle();
            }
            0x10 => {
                self.set_result(&[0x90]);
            }
            0x14 | 0x94 => {
                self.locked = base & 0x80 != 0;
                self.set_result(&[u8::from(self.locked) << 4]);
            }
            0x05 | 0x06 | 0x09 | 0x0a | 0x0c | 0x0d | 0x11 | 0x19 | 0x1d => {
                let drive = self.command[1] & 0x03;
                let head = (self.command[1] >> 2) & 1;
                self.status0 = 0x40 | (head << 2) | drive;
                let [c, h, r, n] = match base {
                    0x0a => [self.cylinder[drive as usize], head, 1, 2],
                    0x0d => [self.cylinder[drive as usize], head, 1, self.command[2]],
                    _ => [
                        self.command[2],
                        self.command[3],
                        self.command[4],
                        self.command[5],
                    ],
                };
                self.set_result(&[self.status0, 0x04, 0, c, h, r, n]);
                self.reset_sense = 0;
                self.irq6 = true;
            }
            _ => {
                self.set_result(&[0x80]);
            }
        }
    }

    fn set_result(&mut self, bytes: &[u8]) {
        self.result[..bytes.len()].copy_from_slice(bytes);
        self.result_pos = 0;
        self.result_len = bytes.len();
        self.enter_idle();
    }

    fn enter_idle(&mut self) {
        self.command_len = 0;
        self.command_needed = 0;
    }

    fn clear_command_and_result(&mut self) {
        self.command_len = 0;
        self.command_needed = 0;
        self.result_pos = 0;
        self.result_len = 0;
    }

    fn start_reset(&mut self) {
        self.clear_command_and_result();
        self.cylinder = [0; 4];
        // Software reset preserves SPECIFY and the perpendicular drive bits.
        // LOCK protects EFIFO/FIFOTHR/PRETRK, not EIS/POLL or GAP/WGATE.
        self.perpendicular &= 0x3c;
        if self.locked {
            self.config &= 0x2f;
        } else {
            self.config = 0x20;
            self.pretrk = 0;
        }
        self.reset_sense = 4;
        self.status0 = 0xc0;
        self.irq6 = true;
    }
}

#[cfg(feature = "host")]
type MachineRam = Vec<u8>;
#[cfg(not(feature = "host"))]
type MachineRam = &'static mut [u8];

fn allocate_ram(size: usize) -> MachineRam {
    #[cfg(feature = "host")]
    {
        vec![0; size]
    }
    #[cfg(not(feature = "host"))]
    {
        alloc::boxed::Box::leak(vec![0; size].into_boxed_slice())
    }
}

pub(crate) struct HostBus {
    ram: MachineRam,
    #[cfg(feature = "jit")]
    jit_code_pages: Option<crate::x86::jit::validity::CodePages>,
    regions: PhysRegionMap<8>,
    frame_roles: Vec<FrameRoleFlags>,
    #[cfg_attr(not(test), allow(dead_code))]
    tracked_writes: TrackedWriteStats,
    diagnostics: BusDiagnostics,
    firmware_mode: bool,
    acpi: Acpi,
    boot_order: BootOrder,
    vga: PcVga,
    ioram_generation: (u64, u64),
    ps2: I8042,
    pic: Pic,
    ide: IdeBus,
    pci: PciConfig,
    net: Option<Ne2000>,
    usb: Option<Uhci>,
    sound: Sound,
    fw_cfg: FwCfgDevice,
    // COM1 is a 16450-style console: host input is queued and TX completes
    // immediately. FCR is absent; the queue is not an advertised UART FIFO.
    serial: Vec<u8>,
    /// Bytes dropped from the front of `serial` (see `push_serial`).
    serial_dropped: u64,
    serial_rx: VecDeque<u8>,
    serial_dll: u8,
    serial_dlh: u8,
    serial_ier: u8,
    serial_lcr: u8,
    serial_mcr: u8,
    serial_msr_delta: u8,
    serial_scratch: u8,
    serial_tx_interrupt: bool,
    pit: Pit,
    port61: u8,
    port92: u8,
    cmos_index: u8,
    cmos_status_a: u8,
    cmos_status_b: u8,
    cmos_status_c: u8,
    cmos_alarm: [u8; 3],
    rtc_irq8: bool,
    rtc_next_tick: u64,
    rtc_last_tick: u64,
    rtc_periodic_epoch: u64,
    fdc: NullFdc,
    reset_requested: bool,
    physical_mapping_changed: bool,
    last_cpu_shutdown: Option<CpuShutdown>,
    current_step: u64,
    clock: Clock,
    magic: magic::State,
    rtc_epoch_seconds: u64,
    rtc_epoch_tick: u64,
    rtc_frozen: Option<RtcCalendar>,
    attention: bool,
    next_attention_step: u64,
    next_device_tick: u64,
}

impl HostBus {
    // PC open-bus semantics: no responder drives an all-ones read; writes
    // have no device side effects (also v86 io.js). This applies only at
    // unclaimed bus dispatch exits, never as recovery from a device error.
    #[cold]
    fn unclaimed_io(&mut self, port: u16, write: bool, value: u8) {
        self.diagnostics.record(
            BusSpace::Io,
            u64::from(port),
            write,
            value,
            self.current_step,
        );
    }

    // tiny386 changes memory mappings at PCI configuration writes. Our CPU
    // caches hold direct backing pointers, so retire their proofs at that
    // same boundary; ordinary RAM hits stay unchanged.
    fn physical_mapping_changed(&mut self) {
        self.physical_mapping_changed = true;
    }

    fn set_a20(&mut self, enabled: bool) {
        let next = (self.port92 & !2) | (u8::from(enabled) << 1);
        if next != self.port92 {
            self.port92 = next;
            self.physical_mapping_changed();
        }
    }

    // A20 gates CPU physical cycles, including page walks, before device decode.
    // Bochs pc_system.cc clears only bit 20; higher address bits remain intact.
    // Raw RAM access by loaders/debuggers (and any future DMA) is independent.
    #[inline]
    fn cpu_phys_addr(&self, addr: PhysAddr) -> PhysAddr {
        if self.port92 & 2 != 0 {
            addr
        } else {
            PhysAddr(addr.0 & !A20_BIT)
        }
    }

    #[inline]
    fn cpu_phys_range(&self, addr: PhysAddr, len: u32) -> Option<PhysAddr> {
        if self.port92 & 2 != 0 {
            return Some(addr);
        }
        // Crossing either edge of an odd MiB breaks physical contiguity.
        if (addr.0 & (A20_BIT - 1)) + u64::from(len) > A20_BIT {
            return None;
        }
        Some(PhysAddr(addr.0 & !A20_BIT))
    }

    #[cold]
    fn unclaimed_io_wide(&mut self, port: u16, width: u16, write: bool, value: u32) {
        // Keep byte/page diagnostic accounting without invoking any byte I/O
        // handler. An unsupported width must have no partial device effects.
        for byte in 0..width {
            self.unclaimed_io(port.wrapping_add(byte), write, (value >> (byte * 8)) as u8);
        }
    }

    #[cold]
    fn unclaimed_memory(&mut self, addr: PhysAddr, write: bool, value: u8) -> Result<(), MemError> {
        // This PC exposes a 36-bit physical bus. An invalid host-side address
        // remains an error; backing/index and implemented-device failures do too.
        if addr.0 >= PC_PHYS_LIMIT {
            return Err(MemError::OutOfRange { addr });
        }
        self.diagnostics
            .record(BusSpace::Mmio, addr.0, write, value, self.current_step);
        Ok(())
    }

    fn new(size: usize) -> Self {
        Self::with_regions(
            allocate_ram(size),
            PhysRegionMap::ram(size as u64).expect("RAM region map fits"),
            false,
            FirmwareDisplay::Serial,
        )
    }

    #[cfg(test)]
    fn new_firmware(size: usize) -> Result<Self, String> {
        Self::new_firmware_with_display(size, FirmwareDisplay::Serial)
    }

    fn new_firmware_with_display(size: usize, display: FirmwareDisplay) -> Result<Self, String> {
        Self::firmware_with_ram(allocate_ram(size), display)
    }

    fn firmware_with_ram(ram: MachineRam, display: FirmwareDisplay) -> Result<Self, String> {
        let size = ram.len();
        if size < 0x0010_0000 {
            return Err("--mem-mib must provide at least 1 MiB for firmware boot".to_string());
        }
        let mut regions = PhysRegionMap::new();
        regions
            .push(PhysRegion::ram(PhysAddr(0), 0x000a_0000))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::mmio(PhysAddr(0x000a_0000), 0x0002_0000))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::rom(PhysAddr(0x000c_0000), 0x0004_0000))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::ram(
                PhysAddr(0x0010_0000),
                size as u64 - 0x0010_0000,
            ))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::fb_dirty_tracked(
                PhysAddr(0xe000_0000),
                0x0100_0000,
            ))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::mmio(PhysAddr(0xfec0_0000), 0x0000_1000))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::mmio(PhysAddr(0xfee0_0000), 0x0000_1000))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        regions
            .push(PhysRegion::rom(
                PhysAddr(FirmwareShadow::HIGH_BASE),
                FirmwareShadow::HIGH_LEN,
            ))
            .map_err(|err| format!("failed to build firmware region map: {err:?}"))?;
        Ok(Self::with_regions(ram, regions, true, display))
    }

    fn with_regions(
        ram: MachineRam,
        regions: PhysRegionMap<8>,
        firmware_mode: bool,
        display: FirmwareDisplay,
    ) -> Self {
        let size = ram.len();
        let frame_count = size.saturating_add(GUEST_PAGE_SIZE - 1) / GUEST_PAGE_SIZE;
        let mut bus = Self::with_buffers(
            ram,
            regions,
            firmware_mode,
            display,
            PcVga::new(),
            vec![FrameRoleFlags::RAM; frame_count],
        );
        if firmware_mode {
            bus.pci.firmware.initialize(&bus.ram);
        }
        bus
    }

    fn with_buffers(
        ram: MachineRam,
        regions: PhysRegionMap<8>,
        firmware_mode: bool,
        display: FirmwareDisplay,
        mut vga: PcVga,
        frame_roles: Vec<FrameRoleFlags>,
    ) -> Self {
        let size = ram.len();
        vga.set_memory_decode(crate::devices::vga::DEFAULT_LFB_BASE, true, firmware_mode);
        Self {
            ram,
            #[cfg(feature = "jit")]
            jit_code_pages: None,
            regions,
            frame_roles,
            tracked_writes: TrackedWriteStats::default(),
            diagnostics: BusDiagnostics::default(),
            firmware_mode,
            acpi: Acpi::new(firmware_mode),
            boot_order: BootOrder::DiskFirst,
            vga,
            ioram_generation: (0, 0),
            ps2: I8042::new(),
            pic: Pic::new(),
            ide: if firmware_mode {
                IdeBus::new_enabled()
            } else {
                IdeBus::new_absent()
            },
            pci: PciConfig::new(),
            net: None,
            usb: None,
            sound: Sound::default(),
            fw_cfg: if firmware_mode {
                FwCfgDevice::new_qemu_with_display(size, display)
            } else {
                FwCfgDevice::absent()
            },
            serial: Vec::new(),
            serial_dropped: 0,
            serial_rx: VecDeque::new(),
            serial_dll: 0,
            serial_dlh: 0,
            serial_ier: 0,
            serial_lcr: 0,
            serial_mcr: 0,
            serial_msr_delta: 0,
            serial_scratch: 0,
            serial_tx_interrupt: false,
            pit: Pit::new(0),
            port61: 0,
            port92: 0x02,
            cmos_index: 0,
            cmos_status_a: 0x26,
            cmos_status_b: 0x02, // PC power-on default: BCD, 24-hour
            cmos_status_c: 0,
            cmos_alarm: [0; 3],
            rtc_irq8: false,
            rtc_next_tick: u64::MAX,
            rtc_last_tick: 0,
            rtc_periodic_epoch: 0,
            fdc: NullFdc::new(),
            reset_requested: false,
            physical_mapping_changed: false,
            last_cpu_shutdown: None,
            current_step: 0,
            clock: Clock::default(),
            magic: magic::State::default(),
            rtc_epoch_seconds: 1_767_225_600, // 2026-01-01 UTC for deterministic runs
            rtc_epoch_tick: 0,
            rtc_frozen: None,
            attention: true,
            next_attention_step: 0,
            next_device_tick: 0,
        }
    }

    fn reset(&mut self, step: u64) {
        self.sound.advance(self.clock.ticks(step), &self.ram);
        let ram = core::mem::take(&mut self.ram);
        let mut frame_roles = core::mem::take(&mut self.frame_roles);
        frame_roles.fill(FrameRoleFlags::RAM);
        let vga = self.vga.take_for_reset();
        let mut fresh = Self::with_buffers(
            ram,
            self.regions.clone(),
            self.firmware_mode,
            FirmwareDisplay::Serial,
            vga,
            frame_roles,
        );
        fresh.pci.firmware = core::mem::take(&mut self.pci.firmware);
        fresh.pci.firmware.reset();
        // Retain media and immutable firmware configuration without copying
        // disk images or RAM. Pending transfers and controller state are reset.
        core::mem::swap(&mut fresh.ide, &mut self.ide);
        fresh.ide.reset();
        if self.net.is_some() {
            fresh.net = Some(Ne2000::default());
            fresh.pci.net_present = true;
            fresh.pic.write(0x4d1, 1 << (NET_IRQ - 8));
        }
        if self.usb.is_some() {
            fresh.usb = Some(Uhci::default());
            fresh.pci.usb_present = true;
            let elcr = fresh.pic.read(0x4d1);
            fresh.pic.write(0x4d1, elcr | (1 << (USB_IRQ - 8)));
        }
        core::mem::swap(&mut fresh.fw_cfg, &mut self.fw_cfg);
        fresh.fw_cfg.reset();
        fresh.serial = core::mem::take(&mut self.serial);
        fresh.serial_dropped = self.serial_dropped;
        fresh.diagnostics = core::mem::take(&mut self.diagnostics);
        fresh.last_cpu_shutdown = self.last_cpu_shutdown.take();
        fresh.current_step = step;
        fresh.boot_order = self.boot_order;
        fresh.clock = core::mem::take(&mut self.clock);
        fresh.clock.sample();
        fresh.clock.write_tsc(0);
        fresh.magic.utc_seconds = self.magic.utc_seconds;
        fresh.magic.utc_offset = self.magic.utc_offset;
        fresh.rtc_epoch_seconds = self.rtc_epoch_seconds;
        fresh.rtc_epoch_tick = self.rtc_epoch_tick;
        fresh.rtc_frozen = self.rtc_frozen;
        fresh.cmos_status_a = self.cmos_status_a;
        fresh.cmos_status_b = self.cmos_status_b & !0x70;
        fresh.cmos_alarm = self.cmos_alarm;
        fresh.rtc_periodic_epoch = self.rtc_periodic_epoch;
        let ticks = fresh.clock.ticks(step);
        fresh.rtc_last_tick = ticks;
        fresh.acpi.reset_clock(ticks);
        fresh.pit = Pit::new(ticks);
        core::mem::swap(&mut fresh.sound, &mut self.sound);
        fresh.sound.reset(ticks);
        fresh.next_attention_step = step;
        *self = fresh;
    }

    fn attach_primary_cdrom(&mut self, iso: Vec<u8>) {
        self.ide.attach_primary_master_cdrom(iso);
    }

    fn attach_secondary_cdrom(&mut self, iso: Vec<u8>) {
        self.ide.attach_secondary_master_cdrom(iso);
    }

    fn attach_hdd(&mut self, image: Vec<u8>, writable: bool) {
        self.ide.attach_primary_master_hdd(image, writable);
    }

    fn drive_irq(&mut self, irq: u8, high: bool) {
        let high = high
            || (self.sound.irq() && self.sound.irq_mask() & (1 << irq) != 0)
            || (irq == USB_IRQ
                && self.pci.usb_irq_enabled()
                && self.usb.as_ref().is_some_and(Uhci::irq));
        if self.pic.set_irq(irq, high) {
            self.set_attention();
        }
    }

    fn sync_ide_irqs(&mut self) {
        self.drive_irq(14, self.ide.irq_asserted(IdeIrq::Primary));
        self.drive_irq(15, self.ide.irq_asserted(IdeIrq::Secondary));
    }

    /// IRQ 11 is shared by the NIC and the USB controller (PIRQC/PIRQD);
    /// drive_irq adds the USB level.
    fn sync_pci_irq(&mut self) {
        self.drive_irq(NET_IRQ, self.net.as_ref().is_some_and(Ne2000::irq));
    }

    fn sync_sound_irq(&mut self) {
        for line in [5, 7, 9, 10] {
            self.drive_irq(line, line == 9 && self.acpi.irq());
        }
    }

    fn sync_device_irqs(&mut self) {
        self.sync_sound_irq();
        self.sync_pci_irq();
        self.sync_ide_irqs();
        self.drive_irq(4, self.serial_irq_asserted());
        self.drive_irq(6, self.fdc.irq_asserted());
        self.drive_irq(8, self.rtc_irq8);
        self.drive_irq(9, self.acpi.irq());
    }

    fn set_attention(&mut self) {
        self.attention = true;
    }

    fn interrupt_attention_due(&self, step: u64) -> bool {
        self.attention || step >= self.next_attention_step
    }

    fn update_pit0_irq(&mut self, ticks: u64) {
        if self.pit.advance(ticks) {
            // A complete low pulse can occur between scheduler visits. Replay
            // one rising edge for the 8259's IRR latch, then retain the real pin
            // level. Missed periods coalesce; they do not become an IRQ queue.
            self.drive_irq(0, false);
            self.drive_irq(0, true);
        }
        self.drive_irq(0, self.pit.irq_level());
    }

    fn ide_bus_master_contains(&self, port: u16, width: u16) -> bool {
        self.pci
            .ide_bus_master_base()
            .is_some_and(|base| u32::from(port.wrapping_sub(base)) + u32::from(width) <= 16)
    }

    fn ide_bus_master_read_u8(&self, port: u16) -> Option<u8> {
        // BAR4 is 16-byte aligned; its low four port bits select the register.
        self.ide_bus_master_contains(port, 1)
            .then(|| self.ide.read_bus_master_u8((port & 15) as u8))
    }

    fn ide_bus_master_write_u8(&mut self, port: u16, value: u8) -> bool {
        if !self.ide_bus_master_contains(port, 1) {
            return false;
        }
        self.ide.write_bus_master_u8((port & 15) as u8, value);
        true
    }

    fn vga_lfb_enabled_contains(&self, addr: PhysAddr, len: u32) -> bool {
        self.pci.vga_mem_enabled()
            && PcVga::contains_lfb(addr.as_u64(), len, self.pci.vga_lfb_base())
    }

    fn classify_phys_kind(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        if self.vga_lfb_enabled_contains(addr, len) {
            return PhysRegionKind::FbDirtyTracked;
        }
        self.regions.classify(addr, len)
    }

    fn device_ticks(&self) -> u64 {
        self.clock.ticks(self.current_step)
    }

    fn sample_device_clock(&mut self, step: u64) {
        self.clock.sample();
        if self.clock.ticks(step) >= self.next_device_tick {
            self.set_attention();
        }
    }

    fn next_device_event(&self) -> u64 {
        self.pit
            .next_irq_tick()
            .min(self.rtc_next_tick)
            .min(self.acpi.next_event_step())
            .min(self.ide.next_event_step())
            .min(self.sound.next_event())
            .min(self.usb.as_ref().map_or(u64::MAX, Uhci::next_event))
    }

    fn advance_usb(&mut self, ticks: u64) {
        let Some(usb) = self.usb.as_mut() else {
            return;
        };
        usb.advance(
            ticks,
            &mut self.ram,
            self.pci.usb_bus_master(),
            |addr, len| {
                // PCI DMA uses physical RAM without CPU A20 translation. The
                // device checks its windows before notifying and committing.
                #[cfg(feature = "jit")]
                if let Some(pages) = self.jit_code_pages.as_mut() {
                    pages.write(addr as usize, usize::from(len));
                }
                #[cfg(test)]
                {
                    self.tracked_writes.total = self.tracked_writes.total.wrapping_add(1);
                    self.tracked_writes.device = self.tracked_writes.device.wrapping_add(1);
                    if ((addr >> 12)..=((u64::from(addr) + u64::from(len) - 1) >> 12) as u32)
                        .any(|frame| self.frame_roles[frame as usize].is_page_table())
                    {
                        self.tracked_writes.page_table_frame =
                            self.tracked_writes.page_table_frame.wrapping_add(1);
                    }
                }
                let _ = (addr, len);
            },
        );
    }

    fn usb_read(&mut self, port: u16, width: u8) -> Option<u32> {
        let offset = self.pci.usb_port(port, width)?;
        self.advance_usb(self.device_ticks());
        self.sync_pci_irq();
        Some(self.usb.as_ref()?.read(offset, width))
    }

    fn usb_write(&mut self, port: u16, width: u8, value: u32) -> bool {
        let Some(offset) = self.pci.usb_port(port, width) else {
            return false;
        };
        let now = self.device_ticks();
        self.advance_usb(now);
        if let Some(usb) = self.usb.as_mut() {
            usb.write(offset, width, value, now);
        }
        self.sync_pci_irq();
        self.set_attention();
        true
    }

    /// CPU IF/shadow transitions poll the latched PIC request. Devices need
    /// service only after a source change or their next deadline.
    /// Keep that deadline in oscillator ticks: realtime sampling and ISA waits
    /// can reach it without reaching the instruction polling boundary.
    fn refresh_interrupt_attention(&mut self, step: u64) -> bool {
        let ticks = self.clock.ticks(step);
        let scan = self.attention || ticks >= self.next_device_tick;
        if scan {
            self.update_pit0_irq(ticks);
            self.update_rtc(ticks);
            self.acpi.update(ticks);
            self.ide.advance(ticks);
            self.sound.advance(ticks, &self.ram);
            self.advance_usb(ticks);
            self.sync_device_irqs();
            self.next_device_tick = self.next_device_event();
        }
        // Early IRQ polls must not postpone the next realtime sample: a guest
        // can execute IRET/port instructions more often than that boundary.
        if !self.clock.is_realtime() || step >= self.next_attention_step {
            self.next_attention_step = self.clock.poll_step(step, self.next_device_tick);
        }
        // If IF or the interrupt shadow blocks delivery, the CPU transition
        // check polls the latched PIC request when the CPU becomes interruptible.
        self.attention = false;
        scan
    }

    fn drain_ps2_effects(&mut self) {
        let mut effects = Vec::new();
        self.ps2.drain_effects(&mut |effect| effects.push(effect));
        for effect in effects {
            match effect {
                DeviceEffect::IrqLevel {
                    line: line @ (1 | 12),
                    level,
                } => {
                    self.drive_irq(line, level);
                }
                DeviceEffect::A20(enabled) => {
                    self.set_a20(enabled);
                }
                DeviceEffect::ResetRequest => {
                    self.reset_requested = true;
                    self.set_attention();
                }
                DeviceEffect::IrqLevel { .. } => {}
            }
        }
    }

    fn load_bios_rom(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > 0x0004_0000 {
            return Err(format!(
                "BIOS must be 1..=256 KiB, got {} bytes",
                bytes.len()
            ));
        }
        let start = 0x0010_0000usize - bytes.len();
        self.ram
            .get_mut(start..start + bytes.len())
            .ok_or_else(|| "BIOS does not fit below 1 MiB".to_string())?
            .copy_from_slice(bytes);
        self.pci.firmware.load_rom(start, bytes);
        Ok(())
    }

    fn load_option_rom(&mut self, base: usize, bytes: &[u8]) -> Result<(), String> {
        if base < 0x000c_0000 || base >= 0x000f_0000 {
            return Err(format!(
                "option ROM base must be in 0xc0000..0xeffff, got {base:#x}"
            ));
        }
        let end = base
            .checked_add(bytes.len())
            .ok_or_else(|| "option ROM address overflow".to_string())?;
        if end > 0x000f_0000 {
            return Err(format!(
                "option ROM at {base:#x} with {} bytes crosses 0xf0000",
                bytes.len()
            ));
        }
        self.ram
            .get_mut(base..end)
            .ok_or_else(|| "option ROM does not fit in RAM backing".to_string())?
            .copy_from_slice(bytes);
        self.pci.firmware.load_rom(base, bytes);
        if base == 0x000c_0000 {
            // SeaBIOS 1.17 clears the legacy ROM area before discovery. Keep
            // the source in fw_cfg so it can deploy the VGA device's ROM.
            self.fw_cfg.set_vga_rom(bytes);
        }
        Ok(())
    }

    fn load_bootsector(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() != 512 {
            return Err(format!(
                "bootsector must be exactly 512 bytes, got {}",
                bytes.len()
            ));
        }
        if bytes[510] != 0x55 || bytes[511] != 0xaa {
            return Err("bootsector is missing 0x55aa signature".to_string());
        }
        self.ram
            .get_mut(0x7c00..0x7c00 + 512)
            .ok_or_else(|| "bootsector load address does not fit RAM".to_string())?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn ram_byte_index(&self, addr: PhysAddr) -> Result<usize, MemError> {
        match self.classify_phys_kind(addr, 1) {
            PhysRegionKind::Ram => self.backing_index_for_len(addr, 1),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Err(MemError::Mmio { addr }),
            PhysRegionKind::Rom | PhysRegionKind::Hole | PhysRegionKind::Mixed => {
                Err(MemError::OutOfRange { addr })
            }
        }
    }

    fn backing_byte_index(&self, addr: PhysAddr) -> Result<usize, MemError> {
        match self.classify_phys_kind(addr, 1) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => self.backing_index_for_len(addr, 1),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Err(MemError::Mmio { addr }),
            PhysRegionKind::Hole | PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn backing_index_for_len(&self, addr: PhysAddr, len: usize) -> Result<usize, MemError> {
        let index = usize::try_from(addr.as_u64()).map_err(|_| MemError::OutOfRange { addr })?;
        let Some(end) = index.checked_add(len) else {
            return Err(MemError::OutOfRange { addr });
        };
        if end <= self.ram.len() {
            Ok(index)
        } else {
            Err(MemError::OutOfRange { addr })
        }
    }

    #[inline]
    fn fast_ram_index_for_len(&self, addr: PhysAddr, len: usize) -> Option<usize> {
        let start = usize::try_from(addr.as_u64()).ok()?;
        let end = start.checked_add(len)?;
        if end > self.ram.len() {
            return None;
        }
        if !self.firmware_mode || end <= 0x000a_0000 || start >= 0x0010_0000 {
            Some(start)
        } else {
            None
        }
    }

    #[inline]
    fn read_backing_u16(&self, index: usize) -> u16 {
        // The caller validated index..index+2 against self.ram.
        unsafe { ram_read_u16(self.ram.as_ptr().add(index)) }
    }

    #[inline]
    fn read_backing_u32(&self, index: usize) -> u32 {
        // The caller validated index..index+4 against self.ram.
        unsafe { ram_read_u32(self.ram.as_ptr().add(index)) }
    }

    #[inline]
    fn read_backing_u64(&self, index: usize) -> u64 {
        // The caller validated index..index+8 against self.ram.
        unsafe { ram_read_u64(self.ram.as_ptr().add(index)) }
    }

    #[inline]
    fn write_backing_u16(&mut self, index: usize, value: u16) {
        #[cfg(feature = "jit")]
        self.jit_backing_write(index, 2);
        // The caller validated index..index+2 against self.ram.
        unsafe {
            ram_write_u16(self.ram.as_mut_ptr().add(index), value);
        }
    }

    #[inline]
    fn write_backing_u32(&mut self, index: usize, value: u32) {
        #[cfg(feature = "jit")]
        self.jit_backing_write(index, 4);
        // The caller validated index..index+4 against self.ram.
        unsafe {
            ram_write_u32(self.ram.as_mut_ptr().add(index), value);
        }
    }

    #[inline]
    fn write_backing_u64(&mut self, index: usize, value: u64) {
        #[cfg(feature = "jit")]
        self.jit_backing_write(index, 8);
        // The caller validated index..index+8 against self.ram.
        unsafe {
            ram_write_u64(self.ram.as_mut_ptr().add(index), value);
        }
    }

    #[cfg(feature = "jit")]
    #[inline]
    fn jit_backing_write(&mut self, index: usize, len: usize) {
        if let Some(pages) = &mut self.jit_code_pages {
            pages.write(index, len);
        }
    }

    #[cfg(feature = "jit")]
    pub(crate) fn jit_rechecks_enabled(&self) -> bool {
        // PCI framebuffer decode may overlay ordinary RAM. Decline comparison
        // witnesses while any such overlay is possible; the shared walker owns
        // that physical access. Code write watches still deny writable
        // probe results.
        self.jit_code_pages.is_some()
            && self.port92 & 2 != 0
            && (!self.pci.vga_mem_enabled() || self.pci.vga_lfb_base() >= self.ram.len() as u64)
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        all(feature = "jit", target_arch = "aarch64", target_os = "linux")
    ))]
    pub(crate) fn jit_code_epoch(&self) -> u64 {
        self.jit_code_pages
            .as_ref()
            .map_or(0, |pages| pages.epoch())
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        all(feature = "jit", target_arch = "aarch64", target_os = "linux")
    ))]
    pub(crate) fn jit_single_retired_version(&self, previous_epoch: u64) -> Option<*const u64> {
        self.jit_code_pages
            .as_ref()
            .and_then(|pages| pages.single_retired_version(previous_epoch))
    }

    #[cfg(any(
        all(feature = "wasm-jit", target_arch = "wasm32"),
        any(
            all(feature = "r86chip", target_arch = "riscv32"),
            any(
                all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
                all(feature = "jit", target_arch = "arm")
            )
        )
    ))]
    pub(crate) fn jit_release_code_pages(&mut self) {
        if let Some(pages) = &mut self.jit_code_pages {
            pages.clear();
        }
    }

    #[cfg(any(
        all(feature = "wasm-jit", target_arch = "wasm32"),
        any(
            all(feature = "r86chip", target_arch = "riscv32"),
            any(
                all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
                all(feature = "jit", target_arch = "arm")
            )
        )
    ))]
    pub(crate) fn jit_code_stamp(
        &mut self,
        cpu: &mut Cpu,
        token: BackingToken,
    ) -> (u64, u64, *const u64) {
        let offset = (token.0 as usize) - self.ram.as_ptr() as usize;
        let pages = self
            .jit_code_pages
            .get_or_insert_with(|| crate::x86::jit::validity::CodePages::new(self.ram.len()));
        let (version, promoted) = pages.watch(offset >> 12);
        if promoted {
            cpu.translation_cache
                .revoke_writable_aliases(self.ram.as_ptr() as u64 + (offset & !4095) as u64);
        }
        (offset as u64, version, pages.version_ptr(offset >> 12))
    }

    #[cfg(all(feature = "wasm-jit", target_arch = "wasm32"))]
    pub(crate) fn jit_backing_offset(&self, token: BackingToken) -> u32 {
        // The fetch-cache hit already certifies backing in this stable RAM.
        // Counting a cold entry does not yet promote/watch the code page.
        (token.0 as usize - self.ram.as_ptr() as usize) as u32
    }

    #[cfg(any(
        all(feature = "wasm-jit", target_arch = "wasm32"),
        any(
            all(feature = "jit", target_arch = "aarch64", target_os = "linux"),
            all(feature = "jit", target_arch = "arm")
        )
    ))]
    pub(crate) fn jit_code_bytes(&self, backing: u64, len: usize) -> &[u8] {
        // The execute-cache hit already approved this backing window. Borrowing
        // it avoids allocating/copying a second guest-byte buffer on compilation.
        &self.ram[backing as usize..backing as usize + len]
    }

    #[inline]
    fn write_firmware(&mut self, addr: PhysAddr, width: u8, value: u64) -> bool {
        let Some(written) = self
            .pci
            .firmware
            .memory_write(&mut self.ram, addr.0, width, value)
        else {
            return false;
        };
        #[cfg(feature = "jit")]
        if written {
            self.jit_backing_write(addr.0 as usize, usize::from(width));
        }
        #[cfg(not(feature = "jit"))]
        let _ = written;
        true
    }

    fn repeat_write_len(width: u8, units: u32) -> Option<u32> {
        u32::from(width).checked_mul(units)
    }

    // Physical bulk operations are already bounded by the caller's guest
    // pages. Device grants must also fit a complete reported physical page;
    // ordinary RAM retains its existing larger-range operations.
    // Bulk destination probes exclude Rom. For the remaining RAM and VGA
    // ranges, read/write grants have identical admission; write grants add
    // dirty tracking. No mapping change can intervene between probe and commit.
    fn direct_range(&mut self, addr: PhysAddr, len: usize, write: bool) -> Option<*mut u8> {
        let addr = self.cpu_phys_range(addr, u32::try_from(len).ok()?)?;
        if len == 0 {
            return None;
        }
        if (addr.0 & 4095) + len as u64 <= 4096 {
            if let Some(page) = self.ioram_page(PhysAddr(addr.0 & !4095), write) {
                return Some((page.0 + (addr.0 & 4095)) as *mut u8);
            }
        }
        let kind = self.classify_phys_kind(addr, len as u32);
        if !write && kind == PhysRegionKind::Rom {
            return self
                .pci
                .firmware
                .read_range(&self.ram, addr.0, len)
                .map(|ptr| ptr as *mut u8);
        }
        let index = if write && kind == PhysRegionKind::Ram {
            self.fast_ram_index_for_len(addr, len)?
        } else if !write && matches!(kind, PhysRegionKind::Ram | PhysRegionKind::Rom) {
            self.fast_ram_index_for_len(addr, len)?
        } else {
            return None;
        };
        Some(unsafe { self.ram.as_mut_ptr().add(index) })
    }

    fn notify_direct_write(&mut self, ptr: *mut u8, len: usize) {
        #[cfg(feature = "jit")]
        {
            let offset = (ptr as usize).wrapping_sub(self.ram.as_ptr() as usize);
            if offset < self.ram.len() {
                self.jit_backing_write(offset, len);
            }
        }
        #[cfg(not(feature = "jit"))]
        let _ = (ptr, len);
    }

    fn phys_ranges_overlap(a: PhysAddr, b: PhysAddr, len: u32) -> bool {
        if len == 0 {
            return false;
        }
        let Some(a_end) = a.as_u64().checked_add(u64::from(len) - 1) else {
            return true;
        };
        let Some(b_end) = b.as_u64().checked_add(u64::from(len) - 1) else {
            return true;
        };
        a.as_u64() <= b_end && b.as_u64() <= a_end
    }

    fn check_write_range(&self, write: PhysWrite) -> Result<(), MemError> {
        match self.classify_phys_kind(write.addr, u32::from(write.len)) {
            PhysRegionKind::Ram => Ok(()),
            // A/D updates are physical stores too; ROM may ignore them.
            PhysRegionKind::Rom if write.origin == WriteOrigin::PageWalk => Ok(()),
            PhysRegionKind::Hole | PhysRegionKind::Mixed
                if matches!(
                    write.origin,
                    WriteOrigin::Cpu | WriteOrigin::Device | WriteOrigin::PageWalk
                ) && write.len != 0
                    && write
                        .addr
                        .0
                        .checked_add(u64::from(write.len))
                        .is_some_and(|end| end <= PC_PHYS_LIMIT) =>
            {
                // CPU stores still receive segmentation/paging checks before
                // reaching the bus. Split commits dispatch each physical byte.
                Ok(())
            }
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked | PhysRegionKind::Rom
                if matches!(write.origin, WriteOrigin::Cpu | WriteOrigin::Device) =>
            {
                Ok(())
            }
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked | PhysRegionKind::Rom => {
                Err(MemError::OutOfRange { addr: write.addr })
            }
            PhysRegionKind::Hole | PhysRegionKind::Mixed => {
                Err(MemError::OutOfRange { addr: write.addr })
            }
        }
    }

    #[inline]
    fn before_decoded_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        if matches!(write.origin, WriteOrigin::Cpu | WriteOrigin::PageWalk)
            && self
                .fast_ram_index_for_len(write.addr, usize::from(write.len))
                .is_some()
        {
            self.record_tracked_write(write);
            return Ok(());
        }
        self.check_write_range(write)?;
        self.record_tracked_write(write);
        Ok(())
    }

    #[cfg(test)]
    fn frame_index(&self, frame: PhysFrame) -> Option<usize> {
        let index = usize::try_from(frame.0).ok()?;
        if index < self.frame_roles.len() {
            Some(index)
        } else {
            None
        }
    }

    #[inline(always)]
    #[cfg(test)]
    fn write_touches_page_table_frame(&self, write: PhysWrite) -> bool {
        if write.len == 0 {
            return false;
        }
        let start = PhysFrame::containing(write.addr);
        let Some(end_addr) = write.addr.as_u64().checked_add(u64::from(write.len) - 1) else {
            return true;
        };
        let end = PhysFrame::containing(PhysAddr(end_addr));
        if start == end {
            let index = start.0 as usize;
            return index < self.frame_roles.len() && self.frame_roles[index].is_page_table();
        }
        let mut frame = start.0;
        while frame <= end.0 {
            if let Some(index) = self.frame_index(PhysFrame(frame)) {
                if self.frame_roles[index].is_page_table() {
                    return true;
                }
            }
            frame += 1;
        }
        false
    }

    fn record_tracked_write(&mut self, write: PhysWrite) {
        #[cfg(not(test))]
        {
            let _ = write;
        }
        #[cfg(test)]
        {
            self.tracked_writes.total = self.tracked_writes.total.wrapping_add(1);
            match write.origin {
                WriteOrigin::Cpu => {
                    self.tracked_writes.cpu = self.tracked_writes.cpu.wrapping_add(1)
                }
                WriteOrigin::PageWalk => {
                    self.tracked_writes.page_walk = self.tracked_writes.page_walk.wrapping_add(1)
                }
                WriteOrigin::Device => {
                    self.tracked_writes.device = self.tracked_writes.device.wrapping_add(1)
                }
                WriteOrigin::Loader => {
                    self.tracked_writes.loader = self.tracked_writes.loader.wrapping_add(1)
                }
                WriteOrigin::Debug => {
                    self.tracked_writes.debug = self.tracked_writes.debug.wrapping_add(1)
                }
            }
            if self.write_touches_page_table_frame(write) {
                self.tracked_writes.page_table_frame =
                    self.tracked_writes.page_table_frame.wrapping_add(1);
            }
        }
    }

    fn queue_serial_input(&mut self, bytes: &[u8]) {
        if self.serial_mcr & 0x10 != 0 {
            return; // Local loopback disconnects the external RX pin.
        }
        self.serial_rx.extend(bytes.iter().copied());
        self.drive_irq(4, self.serial_irq_asserted());
        if !bytes.is_empty() {
            self.set_attention();
        }
    }

    fn serial_irq_asserted(&self) -> bool {
        let rx_pending = (self.serial_ier & 0x01) != 0 && !self.serial_rx.is_empty();
        let tx_pending = (self.serial_ier & 0x02) != 0 && self.serial_tx_interrupt;
        let modem_pending = (self.serial_ier & 0x08) != 0 && self.serial_msr_delta != 0;
        (self.serial_mcr & 0x08) != 0 && (rx_pending || tx_pending || modem_pending)
    }

    fn rtc_period_ticks(&self) -> u64 {
        let mut rate = self.cmos_status_a & 0x0f;
        if rate == 0 {
            return u64::MAX;
        }
        if rate <= 2 {
            rate += 7;
        }
        let hz = RTC_BASE_HZ >> u32::from(rate - 1);
        if hz == 0 {
            u64::MAX
        } else {
            (TICKS_PER_SECOND / hz).max(1)
        }
    }

    // Practical AT RTC contract: v86's calendar, periodic/alarm/update sources
    // and read-to-clear C register, driven by our existing device clock. Unlike
    // an else-if event chain, simultaneous sources are all visible. The small
    // mask/flag rule: enables gate IRQ8, not the flags; v86 rtc.js sets
    // periodic/update flags only when enabled.
    // This is not a crystal/divider simulation; oscillator test modes, SQW and
    // automatic daylight-saving adjustments are outside this clock model.
    fn refresh_rtc_irq(&mut self, ticks: u64) {
        self.rtc_irq8 = self.cmos_status_b & self.cmos_status_c & 0x70 != 0;
        self.cmos_status_c = (self.cmos_status_c & 0x70) | if self.rtc_irq8 { 0x80 } else { 0 };
        self.drive_irq(8, self.rtc_irq8);

        let pending = self.cmos_status_b & !self.cmos_status_c;
        let next = |epoch: u64, period: u64| {
            epoch.saturating_add((ticks.saturating_sub(epoch) / period + 1).saturating_mul(period))
        };
        self.rtc_next_tick = u64::MAX;
        if pending & CMOS_REG_B_PIE != 0 && self.cmos_status_a & 15 != 0 {
            self.rtc_next_tick = next(self.rtc_periodic_epoch, self.rtc_period_ticks());
        }
        if pending & 0x30 != 0 && self.cmos_status_b & 0x80 == 0 {
            self.rtc_next_tick = self
                .rtc_next_tick
                .min(next(self.rtc_epoch_tick, TICKS_PER_SECOND));
        }
    }

    fn update_rtc(&mut self, ticks: u64) {
        let previous = self.rtc_last_tick;
        if self.cmos_status_a & 15 != 0 {
            let period = self.rtc_period_ticks();
            if ticks.saturating_sub(self.rtc_periodic_epoch) / period
                > previous.saturating_sub(self.rtc_periodic_epoch) / period
            {
                self.cmos_status_c |= 0x40;
            }
        }
        let before = previous.saturating_sub(self.rtc_epoch_tick) / TICKS_PER_SECOND;
        let after = ticks.saturating_sub(self.rtc_epoch_tick) / TICKS_PER_SECOND;
        if self.cmos_status_b & 0x80 == 0 && after > before {
            self.cmos_status_c |= 0x10;
            if self.cmos_status_c & 0x20 == 0 {
                // Alarm patterns repeat daily. A delayed poll must not lose an
                // alarm crossed while paused; at most one day's seconds need
                // checking. Normal service checks exactly one second.
                for offset in 1..=(after - before).min(86_400) {
                    let second = (self.rtc_epoch_seconds + before + offset) % 86_400;
                    let time = [
                        self.rtc_encode((second % 60) as u8),
                        self.rtc_encode((second / 60 % 60) as u8),
                        self.rtc_encode_hour((second / 3600) as u8),
                    ];
                    if self
                        .cmos_alarm
                        .iter()
                        .zip(time)
                        .all(|(&alarm, value)| alarm & 0xc0 == 0xc0 || alarm == value)
                    {
                        self.cmos_status_c |= 0x20;
                        break;
                    }
                }
            }
        }
        self.rtc_last_tick = ticks;
        self.refresh_rtc_irq(ticks);
    }

    fn rtc_status_a(&self) -> u8 {
        // Use v86's practical 1 ms UIP window, independent of port-read count
        // and frontend rendering. SET keeps the calendar stable and UIP low.
        let phase = self.device_ticks().saturating_sub(self.rtc_epoch_tick) % TICKS_PER_SECOND;
        let uip =
            self.cmos_status_b & 0x80 == 0 && phase >= TICKS_PER_SECOND - TICKS_PER_SECOND / 1000;
        self.cmos_status_a | if uip { 0x80 } else { 0 }
    }

    fn rtc_calendar(&self) -> RtcCalendar {
        if let Some(calendar) = self.rtc_frozen {
            return calendar;
        }
        let elapsed = self.device_ticks().saturating_sub(self.rtc_epoch_tick) / TICKS_PER_SECOND;
        Self::rtc_calendar_from_seconds(self.rtc_epoch_seconds.saturating_add(elapsed))
    }

    fn rtc_calendar_from_seconds(elapsed_seconds: u64) -> RtcCalendar {
        let second = (elapsed_seconds % 60) as u8;
        let elapsed_minutes = elapsed_seconds / 60;
        let minute = (elapsed_minutes % 60) as u8;
        let elapsed_hours = elapsed_minutes / 60;
        let hour = (elapsed_hours % 24) as u8;
        let mut elapsed_days = elapsed_hours / 24;
        let weekday = ((4 + elapsed_days) % 7 + 1) as u8;
        let mut year = 1970;
        loop {
            let days = if Self::rtc_is_leap_year(year) {
                366
            } else {
                365
            };
            if elapsed_days < days {
                break;
            }
            elapsed_days -= days;
            year += 1;
        }
        let mut month = 1u8;
        loop {
            let days = Self::rtc_days_in_month(year, month);
            if elapsed_days < u64::from(days) {
                break;
            }
            elapsed_days -= u64::from(days);
            month += 1;
        }
        RtcCalendar {
            second,
            minute,
            hour,
            weekday,
            day: elapsed_days as u8 + 1,
            month,
            year,
        }
    }

    fn rtc_set_calendar(&mut self, time: RtcCalendar) {
        if self.rtc_frozen.is_some() {
            self.rtc_frozen = Some(time);
        } else if (1970..=9999).contains(&time.year)
            && (1..=12).contains(&time.month)
            && (1..=Self::rtc_days_in_month(time.year, time.month)).contains(&time.day)
            && time.hour < 24
            && time.minute < 60
            && time.second < 60
        {
            let mut days = 0u64;
            for year in 1970..time.year {
                days += if Self::rtc_is_leap_year(year) {
                    366
                } else {
                    365
                };
            }
            for month in 1..time.month {
                days += u64::from(Self::rtc_days_in_month(time.year, month));
            }
            days += u64::from(time.day - 1);
            self.rtc_epoch_seconds =
                ((days * 24 + u64::from(time.hour)) * 60 + u64::from(time.minute)) * 60
                    + u64::from(time.second);
            self.rtc_epoch_tick = self.device_ticks();
        }
    }

    fn rtc_decode(&self, value: u8) -> u8 {
        if self.cmos_status_b & 4 != 0 {
            value
        } else {
            (value >> 4) * 10 + (value & 15)
        }
    }

    fn rtc_is_leap_year(year: u32) -> bool {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    }

    fn rtc_days_in_month(year: u32, month: u8) -> u8 {
        match month {
            2 if Self::rtc_is_leap_year(year) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    fn rtc_encode(&self, value: u8) -> u8 {
        if (self.cmos_status_b & 0x04) != 0 {
            value
        } else {
            ((value / 10) << 4) | (value % 10)
        }
    }

    fn rtc_encode_hour(&self, hour: u8) -> u8 {
        if (self.cmos_status_b & 0x02) != 0 {
            return self.rtc_encode(hour);
        }
        let pm = hour >= 12;
        let hour12 = match hour % 12 {
            0 => 12,
            value => value,
        };
        self.rtc_encode(hour12) | if pm { 0x80 } else { 0 }
    }

    fn cmos_read(&mut self) -> u8 {
        self.update_rtc(self.device_ticks());
        let mem_kib = (self.ram.len() / 1024).min(u32::MAX as usize) as u32;
        let base_kib = 640u16;
        let ext_kib = mem_kib.saturating_sub(1024).min(0xffff) as u16;
        let ext_64k = mem_kib.saturating_sub(16 * 1024) / 64;
        let ext_64k = ext_64k.min(0xffff) as u16;
        let time = self.rtc_calendar();
        match self.cmos_index & 0x7f {
            0x00 => self.rtc_encode(time.second),
            0x02 => self.rtc_encode(time.minute),
            0x04 => self.rtc_encode_hour(time.hour),
            0x06 => self.rtc_encode(time.weekday),
            0x07 => self.rtc_encode(time.day),
            0x08 => self.rtc_encode(time.month),
            0x09 => self.rtc_encode((time.year % 100) as u8),
            index @ (1 | 3 | 5) => self.cmos_alarm[usize::from(index / 2)],
            CMOS_REG_A => self.rtc_status_a(),
            CMOS_REG_B => self.cmos_status_b,
            CMOS_REG_C => {
                let value = self.cmos_status_c;
                self.cmos_status_c = 0;
                self.refresh_rtc_irq(self.device_ticks());
                self.set_attention();
                value
            }
            CMOS_REG_D => 0x80,
            0x10 => 0,
            0x12 => 0,
            0x14 => 0x02,
            0x15 => base_kib as u8,
            0x16 => (base_kib >> 8) as u8,
            0x17 | 0x30 => ext_kib as u8,
            0x18 | 0x31 => (ext_kib >> 8) as u8,
            0x19 | 0x1a => 47,
            0x2d => 0,
            0x2e | 0x2f => 0,
            0x32 => self.rtc_encode((time.year / 100) as u8),
            0x34 => ext_64k as u8,
            0x35 => (ext_64k >> 8) as u8,
            0x38 => 0,
            // SeaBIOS src/boot.c: low nibble first, 2 = disk, 3 = CD-ROM.
            0x3d => match self.boot_order {
                BootOrder::DiskFirst => 0x32,
                BootOrder::CdromFirst => 0x23,
            },
            0x5b..=0x5d => 0,
            _ => 0,
        }
    }

    fn cmos_write(&mut self, value: u8) {
        let ticks = self.device_ticks();
        self.update_rtc(ticks);
        match self.cmos_index & 0x7f {
            index @ (1 | 3 | 5) => self.cmos_alarm[usize::from(index / 2)] = value,
            CMOS_REG_A => {
                if (self.cmos_status_a ^ value) & 15 != 0 {
                    self.rtc_periodic_epoch = ticks;
                }
                self.cmos_status_a = value & 0x7f;
            }
            CMOS_REG_B => {
                let previous = self.cmos_status_b;
                if previous & 0x80 == 0 && value & 0x80 != 0 {
                    self.rtc_frozen = Some(self.rtc_calendar());
                }
                self.cmos_status_b = value & if value & 0x80 != 0 { 0xe7 } else { 0xf7 };
                if previous & 0x80 != 0 && value & 0x80 == 0 {
                    if let Some(time) = self.rtc_frozen.take() {
                        self.rtc_set_calendar(time);
                    }
                }
            }
            index @ (0x00 | 0x02 | 0x04 | 0x06..=0x09 | 0x32) => {
                let mut time = self.rtc_calendar();
                let decoded = self.rtc_decode(value);
                match index {
                    0x00 => time.second = decoded,
                    0x02 => time.minute = decoded,
                    0x04 => {
                        time.hour = if self.cmos_status_b & 2 != 0 {
                            decoded
                        } else {
                            self.rtc_decode(value & 0x7f) % 12
                                + if value & 0x80 != 0 { 12 } else { 0 }
                        }
                    }
                    0x06 => time.weekday = decoded,
                    0x07 => time.day = decoded,
                    0x08 => time.month = decoded,
                    0x09 => time.year = time.year / 100 * 100 + u32::from(decoded),
                    0x32 => time.year = u32::from(decoded) * 100 + time.year % 100,
                    _ => unreachable!(),
                }
                self.rtc_set_calendar(time);
            }
            CMOS_REG_C | CMOS_REG_D => {}
            _ => {}
        }
        self.refresh_rtc_irq(ticks);
        self.set_attention();
    }

    fn serial_read(&mut self, port: u16) -> u8 {
        let offset = port - 0x3f8;
        let value = match offset {
            0 => {
                if (self.serial_lcr & 0x80) != 0 {
                    self.serial_dll
                } else {
                    self.serial_rx.pop_front().unwrap_or(0)
                }
            }
            1 => {
                if (self.serial_lcr & 0x80) != 0 {
                    self.serial_dlh
                } else {
                    self.serial_ier
                }
            }
            2 => self.serial_iir(),
            3 => self.serial_lcr,
            4 => self.serial_mcr,
            5 => 0x60 | u8::from(!self.serial_rx.is_empty()),
            6 => {
                let status = self.serial_modem_inputs() | self.serial_msr_delta;
                self.serial_msr_delta = 0;
                status
            }
            7 => self.serial_scratch,
            _ => 0,
        };
        self.drive_irq(4, self.serial_irq_asserted());
        if matches!(offset, 0 | 1 | 2 | 4 | 6) {
            self.set_attention();
        }
        value
    }

    /// Guest output is kept for frontends to read by absolute position; a
    /// guest writing without end must not grow it without end. Dropping
    /// half at once keeps pushes amortized O(1).
    fn push_serial(&mut self, value: u8) {
        if self.serial.len() == 2 * SERIAL_KEEP {
            self.serial.drain(..SERIAL_KEEP);
            self.serial_dropped += SERIAL_KEEP as u64;
        }
        self.serial.push(value);
    }

    fn serial_write(&mut self, port: u16, value: u8) {
        match port - 0x3f8 {
            0 => {
                if (self.serial_lcr & 0x80) != 0 {
                    self.serial_dll = value;
                } else {
                    if self.serial_mcr & 0x10 != 0 {
                        self.serial_rx.push_back(value);
                    } else {
                        self.push_serial(value);
                    }
                    if (self.serial_ier & 0x02) != 0 {
                        self.serial_tx_interrupt = true;
                    }
                }
            }
            1 => {
                if (self.serial_lcr & 0x80) != 0 {
                    self.serial_dlh = value;
                } else {
                    let old_ier = self.serial_ier;
                    self.serial_ier = value & 0x0f;
                    if (self.serial_ier & 0x02) != 0 && (old_ier & 0x02) == 0 {
                        self.serial_tx_interrupt = true;
                    } else if (self.serial_ier & 0x02) == 0 {
                        self.serial_tx_interrupt = false;
                    }
                }
            }
            2 => {}
            3 => self.serial_lcr = value,
            4 => {
                let old_inputs = self.serial_modem_inputs();
                self.serial_mcr = value & 0x1f;
                let new_inputs = self.serial_modem_inputs();
                // CTS/DSR/DCD latch either edge; RI latches its trailing edge.
                self.serial_msr_delta |= ((old_inputs ^ new_inputs) >> 4) & 0x0b;
                self.serial_msr_delta |= ((old_inputs & !new_inputs) >> 4) & 0x04;
            }
            7 => self.serial_scratch = value,
            _ => {}
        }
        self.drive_irq(4, self.serial_irq_asserted());
        self.set_attention();
    }

    fn serial_iir(&mut self) -> u8 {
        if (self.serial_ier & 0x01) != 0 && !self.serial_rx.is_empty() {
            0x04
        } else if (self.serial_ier & 0x02) != 0 && self.serial_tx_interrupt {
            self.serial_tx_interrupt = false;
            0x02
        } else if (self.serial_ier & 0x08) != 0 && self.serial_msr_delta != 0 {
            0x00 // Only MSR acknowledges modem changes.
        } else {
            0x01
        }
    }

    fn serial_modem_inputs(&self) -> u8 {
        if self.serial_mcr & 0x10 == 0 {
            0xb0 // Connected terminal: CTS/DSR/DCD asserted, RI clear.
        } else {
            // Local loopback: RTS -> CTS, DTR -> DSR, OUT1 -> RI, OUT2 -> DCD.
            ((self.serial_mcr & 2) << 3)
                | ((self.serial_mcr & 1) << 5)
                | ((self.serial_mcr & 0x0c) << 4)
        }
    }
}

impl Bus for HostBus {
    fn magic_instruction(&mut self, cpu: &mut Cpu) {
        self.magic.call(cpu, self.device_ticks());
    }

    #[inline(always)]
    fn current_instruction(&self) -> u64 {
        self.current_step
    }

    #[inline(always)]
    fn begin_instruction(&mut self, _instruction_count: u64, _tsc: u64) {}

    // Without origin diagnostics, use Bus's no-op so the caller's CS.base+EIP
    // calculation also disappears. No runtime switch belongs in this hot path.
    #[cfg(feature = "bus-instruction-context")]
    #[inline(always)]
    fn instruction_address(&mut self, linear: u32) {
        self.diagnostics.instruction = Some(linear);
    }

    #[inline(always)]
    fn begin_io_instruction(&mut self, instruction_count: u64, port: u16) {
        self.current_step = instruction_count;
        let ide = self.pci.ide_port_enabled(port) || self.ide_bus_master_contains(port, 1);
        // One sample for the whole instruction, including wide PM reads and
        // REP port transfers. Ordinary VGA, PCI address and debug ports do not
        // observe time. USB/sound advance themselves in their port handlers.
        if ide
            || matches!(port, 0x40..=0x43 | 0x61 | 0x71 | 0x3ba | 0x3da | 0xcfc..=0xcff)
            || Sound::contains(port)
            || self.acpi.contains_pm_ports(port, 1)
            || self.pci.usb_port(port, 1).is_some()
        {
            self.sample_device_clock(instruction_count);
        }
        if ide {
            self.ide.advance(self.device_ticks());
            self.sync_ide_irqs();
        }
    }

    fn read_timestamp_counter(&mut self, instruction_tsc: u64) -> u64 {
        self.clock.read_tsc(instruction_tsc)
    }

    fn write_timestamp_counter(&mut self, value: u64) {
        self.clock.write_tsc(value);
    }

    #[inline(always)]
    fn instruction_attention_due(&self, instruction_count: u64) -> bool {
        self.reset_requested || self.interrupt_attention_due(instruction_count)
    }

    #[inline(always)]
    fn batch_attention_limit(&self, start: u64, budget: u64) -> Option<(u64, bool)> {
        if budget == 0 {
            return Some((0, false));
        }
        if self.attention || self.next_attention_step <= start {
            return Some((1, true));
        }
        let distance = self.next_attention_step - start;
        if distance < budget {
            Some((distance + 1, true))
        } else {
            Some((budget, false))
        }
    }

    #[inline(always)]
    fn batch_interrupts_quiet(&self) -> bool {
        !self.attention && self.pic.pending_irq().is_none()
    }

    #[inline(always)]
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        let addr = self.cpu_phys_addr(addr);
        if let Some(value) = self.vga.memory_read(addr.0, 1) {
            return Ok(value as u8);
        }
        if let Some(value) = self.pci.firmware.memory_read(&self.ram, addr.0, 1) {
            return Ok(value as u8);
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 1) {
            return Ok(self.ram[index]);
        }
        match self.classify_phys_kind(addr, 1) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => {
                let index = self.backing_byte_index(addr)?;
                Ok(self.ram[index])
            }
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked | PhysRegionKind::Hole => {
                self.unclaimed_memory(addr, false, 0xff)?;
                Ok(0xff)
            }
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    #[inline(always)]
    fn read_cached_backing_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index < self.ram.len());
        // The linear translation cache admits ordinary RAM only.
        Ok(unsafe { *self.ram.as_ptr().add(index) })
    }

    #[inline(always)]
    fn cached_backing_token(&self, addr: PhysAddr) -> BackingToken {
        let index = self.cpu_phys_addr(addr).0 as usize;
        debug_assert!(index < self.ram.len());
        // Guest RAM is fixed-size while the CPU runs. Fetch-cursor bounds keep
        // every token advance inside the page and segment approved at fill.
        BackingToken(unsafe { self.ram.as_ptr().add(index) } as u64)
    }

    fn fetch_backing_token(&self, addr: PhysAddr) -> Option<BackingToken> {
        let addr = self.cpu_phys_addr(addr);
        let page = PhysAddr(addr.0 & !4095);
        if !matches!(
            self.classify_phys_kind(page, 4096),
            PhysRegionKind::Ram | PhysRegionKind::Rom
        ) || (self.pci.firmware.contains_memory(page.0, 4096)
            && !self.pci.firmware.shadow_reads(page.0, 4096))
        {
            return None;
        }
        Some(self.cached_backing_token(addr))
    }

    #[cfg(feature = "jit")]
    fn jit_page_table_source(&self, addr: PhysAddr) -> Option<BackingToken> {
        let page = PhysAddr(addr.0 & !4095);
        if !self.jit_rechecks_enabled() || self.fast_ram_index_for_len(page, 4096).is_none() {
            return None;
        }
        // This is the same fixed ordinary-RAM range proof used by bulk RAM
        // commits. Firmware holes are excluded, and the check above excludes
        // a relocated PCI framebuffer overlay before issuing the token.
        Some(self.cached_backing_token(addr))
    }

    #[cfg(feature = "jit")]
    fn jit_uncached_data_source(&mut self, addr: PhysAddr, write: bool) -> Option<BackingToken> {
        if !self.jit_rechecks_enabled() {
            return None;
        }
        let page = self.ioram_page(PhysAddr(addr.0 & !4095), write)?;
        Some(BackingToken(page.0 + (addr.0 & 4095)))
    }

    #[inline(always)]
    fn cached_backing_phys(&self, token: BackingToken) -> PhysAddr {
        let base = self.ram.as_ptr() as u64;
        let offset = token.0.wrapping_sub(base);
        if offset < self.ram.len() as u64 {
            PhysAddr(offset)
        } else {
            PhysAddr(self.vga.backing_phys(token.0))
        }
    }

    #[inline(always)]
    fn read_backing_token_u8(&mut self, token: BackingToken) -> u8 {
        unsafe { *(token.0 as *const u8) }
    }

    #[inline(always)]
    fn read_phys_u16(&mut self, addr: PhysAddr) -> Result<u16, MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 2) else {
            let lo = self.read_phys_u8(addr)? as u16;
            let hi = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u16;
            return Ok(lo | (hi << 8));
        };
        if let Some(value) = self.vga.memory_read(addr.0, 2) {
            return Ok(value as u16);
        }
        if let Some(value) = self.pci.firmware.memory_read(&self.ram, addr.0, 2) {
            return Ok(value as u16);
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 2) {
            return Ok(self.read_backing_u16(index));
        }
        match self.classify_phys_kind(addr, 2) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => {
                let index = self.backing_index_for_len(addr, 2)?;
                Ok(self.read_backing_u16(index))
            }
            _ => {
                let lo = self.read_phys_u8(addr)? as u16;
                let hi = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u16;
                Ok(lo | (hi << 8))
            }
        }
    }

    #[inline(always)]
    fn read_cached_backing_u16(&mut self, addr: PhysAddr) -> Result<u16, MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index + 2 <= self.ram.len());
        Ok(self.read_backing_u16(index))
    }

    #[inline(always)]
    fn read_backing_token_u16(&mut self, token: BackingToken) -> u16 {
        unsafe { ram_read_u16(token.0 as *const u8) }
    }

    #[inline(always)]
    fn read_phys_u32(&mut self, addr: PhysAddr) -> Result<u32, MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 4) else {
            let lo = self.read_phys_u16(addr)? as u32;
            let hi = self.read_phys_u16(PhysAddr(addr.0.wrapping_add(2)))? as u32;
            return Ok(lo | (hi << 16));
        };
        if let Some(value) = self.vga.memory_read(addr.0, 4) {
            return Ok(value as u32);
        }
        if let Some(value) = self.pci.firmware.memory_read(&self.ram, addr.0, 4) {
            return Ok(value as u32);
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 4) {
            return Ok(self.read_backing_u32(index));
        }
        match self.classify_phys_kind(addr, 4) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => {
                let index = self.backing_index_for_len(addr, 4)?;
                Ok(self.read_backing_u32(index))
            }
            _ => {
                let b0 = self.read_phys_u8(addr)? as u32;
                let b1 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u32;
                let b2 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(2)))? as u32;
                let b3 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(3)))? as u32;
                Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
            }
        }
    }

    #[inline(always)]
    fn read_cached_backing_u32(&mut self, addr: PhysAddr) -> Result<u32, MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index + 4 <= self.ram.len());
        Ok(self.read_backing_u32(index))
    }

    #[inline(always)]
    fn read_backing_token_u32(&mut self, token: BackingToken) -> u32 {
        unsafe { ram_read_u32(token.0 as *const u8) }
    }

    #[inline(always)]
    fn read_phys_u64(&mut self, addr: PhysAddr) -> Result<u64, MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 8) else {
            let lo = self.read_phys_u32(addr)? as u64;
            let hi = self.read_phys_u32(PhysAddr(addr.0.wrapping_add(4)))? as u64;
            return Ok(lo | (hi << 32));
        };
        if let Some(value) = self.vga.memory_read(addr.0, 8) {
            return Ok(value as u64);
        }
        if let Some(value) = self.pci.firmware.memory_read(&self.ram, addr.0, 8) {
            return Ok(value as u64);
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 8) {
            return Ok(self.read_backing_u64(index));
        }
        match self.classify_phys_kind(addr, 8) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => {
                let index = self.backing_index_for_len(addr, 8)?;
                Ok(self.read_backing_u64(index))
            }
            _ => {
                let lo = self.read_phys_u32(addr)? as u64;
                let hi = self.read_phys_u32(PhysAddr(addr.0.wrapping_add(4)))? as u64;
                Ok(lo | (hi << 32))
            }
        }
    }

    #[inline(always)]
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        let addr = self.cpu_phys_addr(addr);
        if self.pci.firmware.contains_memory(addr.0, 1) {
            return Ok(());
        }
        match self.classify_phys_kind(addr, 1) {
            PhysRegionKind::Ram | PhysRegionKind::Rom => self.backing_byte_index(addr).map(|_| ()),
            PhysRegionKind::Mmio if PcVga::contains_legacy(addr.as_u64()) => Ok(()),
            PhysRegionKind::Mmio => Ok(()),
            PhysRegionKind::FbDirtyTracked
                if PcVga::contains_lfb(addr.as_u64(), 1, self.pci.vga_lfb_base())
                    && self.pci.vga_mem_enabled() =>
            {
                Ok(())
            }
            PhysRegionKind::FbDirtyTracked => Ok(()),
            PhysRegionKind::Hole if addr.0 < PC_PHYS_LIMIT => Ok(()),
            PhysRegionKind::Hole | PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    #[inline(always)]
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        let addr = self.cpu_phys_addr(addr);
        if self.vga.memory_write(addr.0, 1, value as u64) {
            return Ok(());
        }
        if self.write_firmware(addr, 1, value as u64) {
            return Ok(());
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 1) {
            #[cfg(feature = "jit")]
            self.jit_backing_write(index, 1);
            self.ram[index] = value;
            return Ok(());
        }
        match self.classify_phys_kind(addr, 1) {
            PhysRegionKind::Ram => {
                let index = self.ram_byte_index(addr)?;
                #[cfg(feature = "jit")]
                self.jit_backing_write(index, 1);
                self.ram[index] = value;
                Ok(())
            }
            PhysRegionKind::Rom => Ok(()),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked | PhysRegionKind::Hole => {
                self.unclaimed_memory(addr, true, value)
            }
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    #[inline(always)]
    fn write_cached_ram_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index < self.ram.len());
        #[cfg(feature = "jit")]
        self.jit_backing_write(index, 1);
        // The MMU only calls this after a RAM translation has been approved.
        unsafe {
            *self.ram.as_mut_ptr().add(index) = value;
        }
        Ok(())
    }

    #[inline(always)]
    fn write_backing_token_u8(&mut self, token: BackingToken, value: u8) -> Result<(), MemError> {
        // Writable tokens exclude watched code; promotion revokes every old
        // writable alias. The MMU qualifies both pages of a split token store.
        // Physical/device writes retain their code-version notification.
        unsafe {
            *(token.0 as *mut u8) = value;
        }
        Ok(())
    }

    fn write_phys_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 2) else {
            self.write_phys_u8(addr, value as u8)?;
            return self.write_phys_u8(PhysAddr(addr.0.wrapping_add(1)), (value >> 8) as u8);
        };
        if self.vga.memory_write(addr.0, 2, value as u64) {
            return Ok(());
        }
        if self.write_firmware(addr, 2, value as u64) {
            return Ok(());
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 2) {
            self.write_backing_u16(index, value);
            return Ok(());
        }
        match self.classify_phys_kind(addr, 2) {
            PhysRegionKind::Ram => {
                let index = self.backing_index_for_len(addr, 2)?;
                self.write_backing_u16(index, value);
                Ok(())
            }
            PhysRegionKind::Rom => Ok(()),
            _ => {
                self.write_phys_u8(addr, value as u8)?;
                self.write_phys_u8(PhysAddr(addr.0.wrapping_add(1)), (value >> 8) as u8)
            }
        }
    }

    #[inline(always)]
    fn write_cached_ram_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index + 2 <= self.ram.len());
        self.write_backing_u16(index, value);
        Ok(())
    }

    #[inline(always)]
    fn write_backing_token_u16(&mut self, token: BackingToken, value: u16) -> Result<(), MemError> {
        unsafe {
            ram_write_u16(token.0 as *mut u8, value);
        }
        Ok(())
    }

    fn write_phys_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 4) else {
            self.write_phys_u16(addr, value as u16)?;
            return self.write_phys_u16(PhysAddr(addr.0.wrapping_add(2)), (value >> 16) as u16);
        };
        if self.vga.memory_write(addr.0, 4, value as u64) {
            return Ok(());
        }
        if self.write_firmware(addr, 4, value as u64) {
            return Ok(());
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 4) {
            self.write_backing_u32(index, value);
            return Ok(());
        }
        match self.classify_phys_kind(addr, 4) {
            PhysRegionKind::Ram => {
                let index = self.backing_index_for_len(addr, 4)?;
                self.write_backing_u32(index, value);
                Ok(())
            }
            PhysRegionKind::Rom => Ok(()),
            _ => {
                self.write_phys_u16(addr, value as u16)?;
                self.write_phys_u16(PhysAddr(addr.0.wrapping_add(2)), (value >> 16) as u16)
            }
        }
    }

    #[inline(always)]
    fn write_cached_ram_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        let index = addr.as_u64() as usize;
        debug_assert!(index + 4 <= self.ram.len());
        self.write_backing_u32(index, value);
        Ok(())
    }

    #[inline(always)]
    fn write_backing_token_u32(&mut self, token: BackingToken, value: u32) -> Result<(), MemError> {
        unsafe {
            ram_write_u32(token.0 as *mut u8, value);
        }
        Ok(())
    }

    fn write_phys_u64(&mut self, addr: PhysAddr, value: u64) -> Result<(), MemError> {
        let Some(addr) = self.cpu_phys_range(addr, 8) else {
            self.write_phys_u32(addr, value as u32)?;
            return self.write_phys_u32(PhysAddr(addr.0.wrapping_add(4)), (value >> 32) as u32);
        };
        if self.vga.memory_write(addr.0, 8, value as u64) {
            return Ok(());
        }
        if self.write_firmware(addr, 8, value as u64) {
            return Ok(());
        }
        if let Some(index) = self.fast_ram_index_for_len(addr, 8) {
            self.write_backing_u64(index, value);
            return Ok(());
        }
        match self.classify_phys_kind(addr, 8) {
            PhysRegionKind::Ram => {
                let index = self.backing_index_for_len(addr, 8)?;
                self.write_backing_u64(index, value);
                Ok(())
            }
            PhysRegionKind::Rom => Ok(()),
            _ => {
                self.write_phys_u32(addr, value as u32)?;
                self.write_phys_u32(PhysAddr(addr.0.wrapping_add(4)), (value >> 32) as u32)
            }
        }
    }

    fn phys_ram_repeat_write_supported(&mut self, addr: PhysAddr, len: u32) -> bool {
        let Some(addr) = self.cpu_phys_range(addr, len) else {
            return false;
        };
        self.direct_range(addr, len as usize, false).is_some()
            && (self.classify_phys_kind(addr, len) != PhysRegionKind::Rom)
            || self.vga.contains_memory(addr.0, len)
    }

    fn write_phys_ram_repeat(
        &mut self,
        addr: PhysAddr,
        width: u8,
        value: u32,
        units: u32,
    ) -> Result<(), MemError> {
        let len = Self::repeat_write_len(width, units).ok_or(MemError::OutOfRange { addr })?;
        if !matches!(width, 1 | 2 | 4) {
            return Err(MemError::OutOfRange { addr });
        }
        let addr = self
            .cpu_phys_range(addr, len)
            .ok_or(MemError::OutOfRange { addr })?;
        if let Some(ptr) = self.direct_range(addr, len as usize, true) {
            self.notify_direct_write(ptr, len as usize);
            // The RAM proof or device grant owns these plain host bytes.
            let out = unsafe { core::slice::from_raw_parts_mut(ptr, len as usize) };
            if width == 1 {
                out.fill(value as u8);
            } else {
                let bytes = value.to_le_bytes();
                for unit in out.chunks_exact_mut(width as usize) {
                    unit.copy_from_slice(&bytes[..width as usize]);
                }
            }
            return Ok(());
        }
        if self.vga.memory_fill(addr.0, width, value, units) {
            return Ok(());
        }
        Err(MemError::OutOfRange { addr })
    }

    fn phys_ram_copy_supported(&mut self, src: PhysAddr, dst: PhysAddr, len: u32) -> bool {
        let (Some(src), Some(dst)) = (self.cpu_phys_range(src, len), self.cpu_phys_range(dst, len))
        else {
            return false;
        };
        let from = self.direct_range(src, len as usize, false);
        let to = self.direct_range(dst, len as usize, false);
        let writable = self.classify_phys_kind(dst, len) != PhysRegionKind::Rom;
        if let (Some(from), Some(to)) = (from, to) {
            return writable
                && !Self::phys_ranges_overlap(PhysAddr(from as u64), PhysAddr(to as u64), len);
        }
        (from.is_some() || self.vga.contains_memory(src.0, len))
            && ((to.is_some() && writable) || self.vga.contains_memory(dst.0, len))
            && !Self::phys_ranges_overlap(src, dst, len)
    }

    fn copy_phys_to_ram(
        &mut self,
        src: PhysAddr,
        dst: PhysAddr,
        len: u32,
        width: u8,
    ) -> Result<(), MemError> {
        if !matches!(width, 1 | 2 | 4)
            || len % u32::from(width) != 0
            || !self.phys_ram_copy_supported(src, dst, len)
        {
            return Err(MemError::OutOfRange { addr: dst });
        }
        let (src, dst) = (self.cpu_phys_addr(src), self.cpu_phys_addr(dst));
        let from = self.direct_range(src, len as usize, false);
        let to = self.direct_range(dst, len as usize, true);
        if (from.is_none() || to.is_none())
            && self.vga.contains_memory(src.0, len)
            && self.vga.contains_memory(dst.0, len)
        {
            // Device aliases may overlap in backing even when their physical
            // addresses differ. Keep element/latch ordering inside the owner.
            assert!(
                self.vga
                    .memory_copy(src.0, dst.0, width, len / u32::from(width))
            );
            return Ok(());
        }
        if let Some(to) = to {
            self.notify_direct_write(to, len as usize);
            if let Some(from) = from {
                // Capability qualification rejected aliases and overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(from, to, len as usize);
                }
            } else {
                let out = unsafe { core::slice::from_raw_parts_mut(to, len as usize) };
                assert!(self.vga.memory_read_bytes(src.0, out));
            }
        } else if let Some(from) = from {
            let bytes = unsafe { core::slice::from_raw_parts(from, len as usize) };
            assert!(self.vga.memory_write_bytes(dst.0, bytes));
        } else {
            assert!(
                self.vga
                    .memory_copy(src.0, dst.0, width, len / u32::from(width))
            );
        }
        Ok(())
    }

    fn io_read_to_ram_supported(&self, port: u16, width: u8, dst: PhysAddr, units: u32) -> bool {
        let Some(len) = Self::repeat_write_len(width, units) else {
            return false;
        };
        let Some(dst) = self.cpu_phys_range(dst, len) else {
            return false;
        };
        matches!(width, 1 | 2 | 4)
            && self.fast_ram_index_for_len(dst, len as usize).is_some()
            && self.pci.ide_port_enabled(port)
            && self.ide.read_to_slice_supported(port, len as usize)
    }

    fn io_read_to_ram(
        &mut self,
        port: u16,
        width: u8,
        dst: PhysAddr,
        units: u32,
    ) -> Result<bool, MemError> {
        if !self.io_read_to_ram_supported(port, width, dst, units) {
            return Ok(false);
        }
        let len = Self::repeat_write_len(width, units).ok_or(MemError::OutOfRange { addr: dst })?;
        let dst_index = self.backing_index_for_len(self.cpu_phys_addr(dst), len as usize)?;
        let end = dst_index + len as usize;
        #[cfg(feature = "jit")]
        self.jit_backing_write(dst_index, len as usize);
        let handled = self.ide.read_to_slice(port, &mut self.ram[dst_index..end]);
        self.sync_ide_irqs();
        if handled {
            self.set_attention();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn classify_phys(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        self.cpu_phys_range(addr, len)
            .map_or(PhysRegionKind::Mixed, |addr| {
                self.classify_phys_kind(addr, len)
            })
    }

    fn note_page_table_frame(
        &mut self,
        frame: PhysFrame,
        role: PageTableFrameRole,
    ) -> Result<bool, MemError> {
        let frame = PhysFrame::containing(self.cpu_phys_addr(frame.base()));
        // Software TLB entries may retain a translation and its permissions
        // until eviction or an architectural invalidation. Ordinary PTE stores
        // are not INVLPG. This also keeps their RAM stores on the normal path.
        // v86 cpu.rs agrees. The standalone tlb_lifetime ROM checks remapping,
        // cached write permissions and Accessed-bit clearing.
        let _ = (frame, role);
        Ok(false)
    }

    #[inline(always)]
    fn before_phys_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        // Bus commits use CPU address decoding regardless of notification origin.
        // Loaders/debuggers that need raw RAM use the separate backing accessors.
        if let Some(addr) = self.cpu_phys_range(write.addr, u32::from(write.len)) {
            return self.before_decoded_write(PhysWrite { addr, ..write });
        }
        // A crossing write has first < write.len, so both lengths fit u8.
        let first = (A20_BIT - (write.addr.0 & (A20_BIT - 1))) as u8;
        self.before_decoded_write(PhysWrite::new(
            write.origin,
            self.cpu_phys_addr(write.addr),
            first,
        ))?;
        self.before_decoded_write(PhysWrite::new(
            write.origin,
            self.cpu_phys_addr(PhysAddr(write.addr.0 + u64::from(first))),
            write.len - first,
        ))
    }

    #[inline(always)]
    fn before_cached_ram_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        self.record_tracked_write(write);
        Ok(())
    }

    fn linear_translation_cache_enabled(&self) -> bool {
        true
    }

    fn ioram_page(&mut self, page: PhysAddr, write: bool) -> Option<BackingToken> {
        let page = self.cpu_phys_range(page, 4096)?;
        let ptr = if self.vga.contains_memory(page.0, 4096) {
            self.vga.direct_page(page.0, write)
        } else if !write || self.cached_write_translation_allowed(page) {
            self.pci.firmware.direct_page(&mut self.ram, page.0, write)
        } else {
            None
        };
        ptr.map(|ptr| BackingToken(ptr as u64))
    }

    fn sync_ioram(&mut self, cpu: &mut Cpu) {
        let generation = (
            self.vga.mapping_generation(),
            self.pci.firmware.mapping_generation(),
        );
        if self.ioram_generation != generation {
            if self.ioram_generation.1 != generation.1 {
                // PAM can change instruction bytes without a RAM store. Retire
                // fetch cursors and every native mapping proof as well as data.
                cpu.invalidate_translation_context();
            } else {
                cpu.invalidate_ioram_translations();
            }
            self.ioram_generation = generation;
        }
    }

    fn device_write_width_supported(&self, addr: PhysAddr, len: u32) -> bool {
        self.cpu_phys_range(addr, len).is_some_and(|addr| {
            self.vga.contains_memory(addr.0, len) || self.pci.firmware.contains_memory(addr.0, len)
        })
    }

    fn copy_backing_tokens(&mut self, src: BackingToken, dst: BackingToken, len: u32) -> bool {
        if Self::phys_ranges_overlap(PhysAddr(src.0), PhysAddr(dst.0), len) {
            return false;
        }
        // A writable cache grant excludes code watches and tracked tables.
        // Device grants have already dirtied the destination page.
        unsafe {
            core::ptr::copy_nonoverlapping(src.0 as *const u8, dst.0 as *mut u8, len as usize);
        }
        true
    }

    fn fill_backing_token(&mut self, dst: BackingToken, width: u8, value: u32, units: u32) -> bool {
        let len = u32::from(width) * units;
        let out = unsafe { core::slice::from_raw_parts_mut(dst.0 as *mut u8, len as usize) };
        if width == 1 {
            out.fill(value as u8);
        } else {
            let bytes = value.to_le_bytes();
            for unit in out.chunks_exact_mut(width as usize) {
                unit.copy_from_slice(&bytes[..width as usize]);
            }
        }
        true
    }

    fn take_physical_mapping_change(&mut self) -> bool {
        core::mem::take(&mut self.physical_mapping_changed)
    }

    // Ordinary PTE stores are not architectural TLB invalidations. The shared
    // software caches retain their entries until eviction/CRx/INVLPG.

    #[inline(always)]
    fn cached_ram_write_needs_fetch_invalidation(&self) -> bool {
        // Backing tokens are pointers into fixed guest RAM. Fetches through an
        // existing token observe stores directly. Compiled code has its own
        // physical content version, separate from these translation lifetimes.
        false
    }

    #[inline(always)]
    fn cached_ram_write_needs_dynamic_translation_flush(&self) -> bool {
        false
    }

    #[inline(always)]
    fn cached_write_translation_allowed(&self, phys: PhysAddr) -> bool {
        let index = PhysFrame::containing(self.cpu_phys_addr(phys)).0 as usize;
        #[cfg(feature = "jit")]
        if self
            .jit_code_pages
            .as_ref()
            .is_some_and(|pages| index < self.frame_roles.len() && pages.watched(index))
        {
            return false;
        }
        index < self.frame_roles.len()
    }

    fn io_read_u8(&mut self, port: u16) -> Result<u8, MemError> {
        if let Some(value) = self.usb_read(port, 1) {
            return Ok(value as u8);
        }
        if Sound::contains(port) {
            self.clock.io_wait(Sound::io_wait_ticks(port));
            self.sound.advance(self.device_ticks(), &self.ram);
            let value = self.sound.read(port);
            self.sync_sound_irq();
            self.set_attention();
            return Ok(value);
        }
        if let Some(offset) = self.pci.net_port(port)
            && let Some(value) = self.net.as_mut().and_then(|nic| nic.read(offset, 1))
        {
            self.sync_pci_irq();
            return Ok(value as u8);
        }
        if let Some(value) = self.acpi.read(port, self.device_ticks()) {
            self.drive_irq(9, self.acpi.irq());
            return Ok(value);
        }
        if self.pci.ide_port_enabled(port)
            && let Some(value) = self.ide.read_u8(port)
        {
            self.sync_ide_irqs();
            self.set_attention();
            return Ok(value);
        }
        if let Some(value) = self.fdc.read_u8(port) {
            self.drive_irq(6, self.fdc.irq_asserted());
            return Ok(value);
        }
        if let Some(value) = self.ide_bus_master_read_u8(port) {
            return Ok(value);
        }
        match port {
            0xf0..=0xff | 0x480..=0x48f => Ok(0),
            0x20 | 0x21 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1 => {
                self.set_attention();
                Ok(self.pic.read(port))
            }
            0x40..=0x43 => {
                self.update_pit0_irq(self.device_ticks());
                Ok(self.pit.read((port - 0x40) as u8))
            }
            0x70 => Ok(self.cmos_index),
            0x71 => Ok(self.cmos_read()),
            0x60 => {
                let value = self.ps2.pio_read(0x60).unwrap_or(0);
                self.drain_ps2_effects();
                Ok(value)
            }
            0x61 => {
                self.update_pit0_irq(self.device_ticks());
                Ok(self.port61 | self.pit.port61_status())
            }
            0x64 => {
                let value = self.ps2.pio_read(0x64).unwrap_or(0);
                self.drain_ps2_effects();
                Ok(value)
            }
            0x92 => Ok(self.port92),
            0x279 | 0xa79 => Ok(0),
            0x3f8..=0x3ff => Ok(self.serial_read(port)),
            0x402 => Ok(0xe9),
            0xcf8..=0xcfb => Ok((self.pci.addr() >> ((port - 0xcf8) * 8)) as u8),
            0xcfc..=0xcff => {
                let value = self.pci.read_data_u32();
                Ok((value >> ((port - 0xcfc) * 8)) as u8)
            }
            0x511 => Ok(self.fw_cfg.read_data()),
            0x512..=0x513 => Ok(0),
            0x514..=0x51b => Ok(0),
            0x51c..=0x51f => Ok(0),
            0x3c0..=0x3df | 0x3b0..=0x3bf if self.pci.vga_io_enabled() => {
                Ok(self.vga.io_read_u8(port, self.device_ticks()).unwrap_or(0))
            }
            _ => {
                self.unclaimed_io(port, false, 0xff);
                Ok(0xff)
            }
        }
    }

    fn io_read_u16(&mut self, port: u16) -> Result<u16, MemError> {
        if let Some(value) = self.usb_read(port, 2) {
            return Ok(value as u16);
        }
        if let Some(offset) = self.pci.net_port(port)
            && let Some(value) = self.net.as_mut().and_then(|nic| nic.read(offset, 2))
        {
            self.sync_pci_irq();
            return Ok(value as u16);
        }
        if self.pci.ide_port_enabled(port)
            && let Some(value) = self.ide.read_u16(port)
        {
            self.sync_ide_irqs();
            self.set_attention();
            return Ok(value);
        }
        match port {
            0xcf8 | 0xcfa => Ok((self.pci.addr() >> ((port - 0xcf8) * 8)) as u16),
            0xcfc | 0xcfe => {
                let value = self.pci.read_data_u32();
                Ok((value >> ((port - 0xcfc) * 8)) as u16)
            }
            0x1ce | 0x1cf if self.pci.vga_io_enabled() => {
                Ok(self.vga.io_read_u16(port, self.device_ticks()).unwrap_or(0))
            }
            // Only this complete register window permits consecutive byte
            // accesses. Other widths dispatch explicitly, like tiny386/v86;
            // unsupported cycles must not invoke arbitrary adjacent devices.
            _ if self.acpi.contains_pm_ports(port, 2) || self.ide_bus_master_contains(port, 2) => {
                let lo = self.io_read_u8(port)? as u16;
                let hi = self.io_read_u8(port + 1)? as u16;
                Ok(lo | (hi << 8))
            }
            _ => {
                self.unclaimed_io_wide(port, 2, false, u32::from(u16::MAX));
                Ok(u16::MAX)
            }
        }
    }

    fn io_read_u32(&mut self, port: u16) -> Result<u32, MemError> {
        if let Some(value) = self.usb_read(port, 4) {
            return Ok(value as u32);
        }
        if let Some(offset) = self.pci.net_port(port)
            && let Some(value) = self.net.as_mut().and_then(|nic| nic.read(offset, 4))
        {
            self.sync_pci_irq();
            return Ok(value as u32);
        }
        if self.pci.ide_port_enabled(port)
            && let Some(value) = self.ide.read_u32(port)
        {
            self.sync_ide_irqs();
            self.set_attention();
            return Ok(value);
        }
        match port {
            0xcf8 => Ok(self.pci.addr()),
            0xcfc => Ok(self.pci.read_data_u32()),
            _ if self.acpi.contains_pm_ports(port, 4) || self.ide_bus_master_contains(port, 4) => {
                Ok(u32::from_le_bytes([
                    self.io_read_u8(port)?,
                    self.io_read_u8(port + 1)?,
                    self.io_read_u8(port + 2)?,
                    self.io_read_u8(port + 3)?,
                ]))
            }
            _ => {
                self.unclaimed_io_wide(port, 4, false, u32::MAX);
                Ok(u32::MAX)
            }
        }
    }

    fn io_write_u8(&mut self, port: u16, value: u8) -> Result<(), MemError> {
        if self.usb_write(port, 1, value as u32) {
            return Ok(());
        }
        if Sound::contains(port) {
            self.clock.io_wait(Sound::io_wait_ticks(port));
            self.sound.advance(self.device_ticks(), &self.ram);
            self.sound.write(port, value);
            self.sync_sound_irq();
            self.set_attention();
            return Ok(());
        }
        if let Some(offset) = self.pci.net_port(port)
            && self
                .net
                .as_mut()
                .is_some_and(|nic| nic.write(offset, 1, value as u32))
        {
            self.sync_pci_irq();
            return Ok(());
        }
        self.set_attention();
        if self.acpi.write(port, value, self.device_ticks()) {
            self.drive_irq(9, self.acpi.irq());
            return Ok(());
        }
        if self.pci.ide_port_enabled(port) && self.ide.write_u8(port, value) {
            self.sync_ide_irqs();
            return Ok(());
        }
        if self.fdc.write_u8(port, value) {
            self.drive_irq(6, self.fdc.irq_asserted());
            return Ok(());
        }
        if self.ide_bus_master_write_u8(port, value) {
            return Ok(());
        }
        match port {
            0xf0..=0xff | 0x480..=0x48f => Ok(()),
            0x20 | 0x21 | 0xa0 | 0xa1 | 0x4d0 | 0x4d1 => {
                self.pic.write(port, value);
                Ok(())
            }
            0x40..=0x43 => {
                self.update_pit0_irq(self.device_ticks());
                self.pit.write((port - 0x40) as u8, value);
                self.drive_irq(0, self.pit.irq_level());
                Ok(())
            }
            0x3f8..=0x3ff => {
                self.serial_write(port, value);
                Ok(())
            }
            0x61 => {
                self.update_pit0_irq(self.device_ticks());
                self.pit.set_gate2(value & 1 != 0);
                // The upper nibble is read-only status, with no parity error
                // source in this board. Bits 0..3 retain the AT control latch.
                self.port61 = value & 0x0f;
                Ok(())
            }
            0x70 => {
                self.cmos_index = value;
                Ok(())
            }
            0x71 => {
                self.cmos_write(value);
                Ok(())
            }
            0x60 | 0x64 => {
                let _ = self.ps2.pio_write(port, value);
                self.drain_ps2_effects();
                Ok(())
            }
            0x3c0..=0x3df | 0x3b0..=0x3bf if self.pci.vga_io_enabled() => {
                let _ = self.vga.io_write_u8(port, value, self.device_ticks());
                Ok(())
            }
            0x402 => {
                self.push_serial(value);
                Ok(())
            }
            0xcf8..=0xcfb => {
                let shift = (port - 0xcf8) * 8;
                let mask = !(0xffu32 << shift);
                self.pci
                    .write_addr((self.pci.addr() & mask) | (u32::from(value) << shift));
                Ok(())
            }
            0xcfc..=0xcff => {
                self.advance_usb(self.device_ticks());
                if self.pci.write_data_u8((port - 0xcfc) as u8, value) {
                    self.physical_mapping_changed();
                }
                self.vga.set_memory_decode(
                    self.pci.vga_lfb_base(),
                    self.pci.vga_mem_enabled(),
                    self.firmware_mode,
                );
                self.advance_usb(self.device_ticks());
                self.sync_pci_irq();
                Ok(())
            }
            0x511 => {
                self.fw_cfg.write_data(value);
                Ok(())
            }
            0x512..=0x51f => Ok(()),
            0x92 => {
                // Reset is a pulse, not a latched register bit.
                self.set_a20(value & 2 != 0);
                self.port92 = value & !0x01;
                if (value & 0x01) != 0 {
                    self.reset_requested = true;
                }
                Ok(())
            }
            0x8900 => {
                self.push_serial(value);
                Ok(())
            }
            0x279 | 0xa79 => Ok(()),
            _ => {
                self.unclaimed_io(port, true, value);
                Ok(())
            }
        }
    }

    fn io_write_u16(&mut self, port: u16, value: u16) -> Result<(), MemError> {
        if self.usb_write(port, 2, value as u32) {
            return Ok(());
        }
        if let Some(offset) = self.pci.net_port(port)
            && self
                .net
                .as_mut()
                .is_some_and(|nic| nic.write(offset, 2, value as u32))
        {
            self.sync_pci_irq();
            return Ok(());
        }
        self.set_attention();
        if self.pci.ide_port_enabled(port) && self.ide.write_u16(port, value) {
            self.sync_ide_irqs();
            return Ok(());
        }
        match port {
            0xcf8 | 0xcfa => {
                let shift = (port - 0xcf8) * 8;
                self.pci.write_addr(
                    (self.pci.addr() & !(0xffff << shift)) | (u32::from(value) << shift),
                );
                Ok(())
            }
            0xcfc | 0xcfe => {
                self.advance_usb(self.device_ticks());
                if self.pci.write_data_u16((port - 0xcfc) as u8, value) {
                    self.physical_mapping_changed();
                }
                self.vga.set_memory_decode(
                    self.pci.vga_lfb_base(),
                    self.pci.vga_mem_enabled(),
                    self.firmware_mode,
                );
                self.advance_usb(self.device_ticks());
                self.sync_pci_irq();
                Ok(())
            }
            0x510 => {
                self.fw_cfg.select(value);
                Ok(())
            }
            0x1ce | 0x1cf | 0x3b0..=0x3be | 0x3c0..=0x3de if self.pci.vga_io_enabled() => {
                let _ = self.vga.io_write_u16(port, value, self.device_ticks());
                Ok(())
            }
            _ if self.acpi.contains_pm_ports(port, 2) || self.ide_bus_master_contains(port, 2) => {
                self.io_write_u8(port, value as u8)?;
                self.io_write_u8(port + 1, (value >> 8) as u8)
            }
            _ => {
                self.unclaimed_io_wide(port, 2, true, u32::from(value));
                Ok(())
            }
        }
    }

    fn io_write_u32(&mut self, port: u16, value: u32) -> Result<(), MemError> {
        if self.usb_write(port, 4, value) {
            return Ok(());
        }
        if let Some(offset) = self.pci.net_port(port)
            && self
                .net
                .as_mut()
                .is_some_and(|nic| nic.write(offset, 4, value))
        {
            self.sync_pci_irq();
            return Ok(());
        }
        self.set_attention();
        if self.pci.ide_port_enabled(port) && self.ide.write_u32(port, value) {
            self.sync_ide_irqs();
            return Ok(());
        }
        match port {
            0xcf8 => {
                self.pci.write_addr(value);
                Ok(())
            }
            0xcfc => {
                self.advance_usb(self.device_ticks());
                if self.pci.write_data_u32(value) {
                    self.physical_mapping_changed();
                }
                self.vga.set_memory_decode(
                    self.pci.vga_lfb_base(),
                    self.pci.vga_mem_enabled(),
                    self.firmware_mode,
                );
                self.advance_usb(self.device_ticks());
                self.sync_pci_irq();
                Ok(())
            }
            _ if self.acpi.contains_pm_ports(port, 4) || self.ide_bus_master_contains(port, 4) => {
                for (byte, value) in value.to_le_bytes().into_iter().enumerate() {
                    self.io_write_u8(port + byte as u16, value)?;
                }
                Ok(())
            }
            _ => {
                self.unclaimed_io_wide(port, 4, true, value);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod host_bus_tests {
    extern crate std;

    use super::*;
    use crate::x86::cpu::{CR0_PE, CR0_PG};

    const PTE_PRESENT: u32 = 1 << 0;
    const PTE_WRITE: u32 = 1 << 1;
    const PTE_USER: u32 = 1 << 2;
    const PTE_ACCESSED: u32 = 1 << 5;
    const PTE_DIRTY: u32 = 1 << 6;

    #[test]
    fn segmented_code_trace_reads_cs_base_and_wraps_linear_address() {
        for (selector, base, eip, flags, linear) in [
            (0x100, 0x1000, 0x40, 0x0093, 0x1040),
            (8, 0xffff_f000, 0x1010, 0xc09b, 0x10),
        ] {
            let mut cpu = Cpu::new();
            cpu.eip = eip;
            cpu.set_seg_cache(
                SegmentReg::Cs,
                SegmentCache::new(selector, base, u32::MAX, flags),
            );
            let mut ram = vec![0; 0x4000];
            ram[eip as usize..eip as usize + 6].fill(0xcc);
            ram[linear..linear + 6].copy_from_slice(&[0x90, 1, 2, 3, 4, 5]);
            let mut trace = TraceRing::new();
            trace.push_full(7, &cpu, &ram);
            assert!(trace.report().contains(&format!(
                "step=7 cs={selector:#06x} eip={eip:#010x} linear={linear:#010x} \
                 bytes=90 01 02 03 04 05"
            )));
        }
    }

    #[test]
    fn segmented_code_probe_retains_linear_paged_addressing() {
        let mut m = Machine::new(0x8000);
        m.cpu.enter_protected_flat(0x20, 0);
        m.cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache::new(8, 0x4000, u32::MAX, 0xc09b),
        );
        m.cpu.cr0 |= CR0_PG;
        m.cpu.cr3 = 0x1000;
        write_ram_u32(&mut m.bus, 0x1000, 0x2003);
        write_ram_u32(&mut m.bus, 0x2010, 0x3003);
        m.bus.ram[0x20..0x24].fill(0xcc);
        m.bus.ram[0x4020..0x4024].fill(0xcc);
        m.bus.ram[0x3020..0x3024].copy_from_slice(&[0x66, 0x90, 0xf4, 0x90]);

        let mut bytes = [None; 4];
        m.guest_code_bytes(0x4020, &mut bytes, true);
        assert_eq!(bytes, [Some(0x66), Some(0x90), Some(0xf4), Some(0x90)]);
        assert_eq!(m.cpu.eip, 0x20);
    }

    #[cfg(all(feature = "jit", target_arch = "arm"))]
    fn arm_compare_runs(
        expected: &mut Machine,
        native: &mut Machine,
        budgets: &[u64],
        label: &str,
    ) {
        for (round, &budget) in budgets.iter().enumerate() {
            let start = expected.cpu.eip;
            let reference = expected
                .run_steps(budget)
                .unwrap_or_else(|e| panic!("{label}: interpreter from {start:#x}: {e}"));
            let actual = native
                .run_steps(budget)
                .unwrap_or_else(|e| panic!("{label}: native from {start:#x}: {e}"));
            let at = format!(
                "{label} round {round} budget {budget} eip {:#x}",
                expected.cpu.eip
            );
            assert_eq!(actual.ran, reference.ran, "{at}");
            assert_eq!(native.cpu.eip, expected.cpu.eip, "{at}");
            assert_eq!(native.cpu.regs, expected.cpu.regs, "{at}");
            assert_eq!(
                native.cpu.effective_eflags(),
                expected.cpu.effective_eflags(),
                "{at}"
            );
            assert_eq!(native.cpu.tsc(), expected.cpu.tsc(), "{at}");
            assert!(native.ram() == expected.ram(), "{at}: RAM differs");
        }
    }

    #[cfg(all(feature = "jit", target_arch = "arm"))]
    #[test]
    fn arm_jit_matches_interpreter_across_budgets_memory_and_code_changes() {
        fn machine(jit: bool) -> Machine {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.regs[0] = 0x7fff_fffc;
            cpu.regs[1] = 1;
            let mut bus = HostBus::new(0x10000);
            bus.ram[0x1000..0x100a].copy_from_slice(&[
                0x01, 0xc8, // add eax,ecx
                0x41, // inc ecx
                0xa3, 0x00, 0x20, 0x00, 0x00, // mov [2000h],eax
                0xeb, 0xf6, // jmp 1000h
            ]);
            let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
            m.set_jit_enabled(jit).unwrap();
            m
        }
        let mut expected = machine(false);
        let mut native = machine(true);
        for budget in [0, 1, 2, 3, 31, 63, 127, 257, 1_050_000] {
            arm_compare_runs(&mut expected, &mut native, &[budget], "loop");
            // A host write must retire a native block containing these bytes.
            let changed = expected.bus.ram[0x1002] ^ 8; // inc ecx <-> dec ecx
            expected
                .bus
                .write_phys_u8(PhysAddr(0x1002), changed)
                .unwrap();
            native.bus.write_phys_u8(PhysAddr(0x1002), changed).unwrap();
        }
        let stats = &native.jit.as_ref().unwrap().stats;
        assert!(stats.native_steps > 1_000_000, "{stats:?}");
        assert!(stats.invalidations > 0, "{stats:?}");
    }

    /// Opt-in firmware boot comparison: interpreter versus ARMv6K JIT in
    /// lockstep. Set R86_LOCKSTEP_ISO to attach a CD; R86_LOCKSTEP_STEPS
    /// bounds the run. On divergence it reports the 16-step window.
    #[cfg(all(feature = "jit", target_arch = "arm"))]
    #[test]
    #[ignore = "boots vendored SeaBIOS for minutes under QEMU"]
    fn arm_jit_firmware_lockstep() {
        let make = |jit: bool| {
            let mut m = Machine::new_firmware(128 << 20, FirmwareDisplay::Serial).unwrap();
            m.load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
                .unwrap();
            m.load_option_rom(
                0xc0000,
                include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
            )
            .unwrap();
            if let Some(path) = std::env::var_os("R86_LOCKSTEP_ISO") {
                m.attach_primary_cdrom(std::fs::read(path).unwrap());
            }
            m.reset_bios();
            m.set_jit_enabled(jit).unwrap();
            m
        };
        let total: u64 = std::env::var("R86_LOCKSTEP_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000_000);
        let same = |a: &Machine, b: &Machine| {
            a.cpu.regs == b.cpu.regs
                && a.cpu.eip == b.cpu.eip
                && a.cpu.effective_eflags() == b.cpu.effective_eflags()
                && a.cpu.tsc() == b.cpu.tsc()
                && a.serial_output() == b.serial_output()
        };
        const CHUNK: u64 = 100_000;
        let mut expected = make(false);
        let mut native = make(true);
        let mut done = 0;
        let mut failed = None;
        while done < total {
            expected.run_steps(CHUNK).unwrap();
            native.run_steps(CHUNK).unwrap();
            if !same(&expected, &native)
                || (done / CHUNK).is_multiple_of(64) && expected.ram() != native.ram()
            {
                failed = Some(done);
                break;
            }
            done += CHUNK;
        }
        std::eprintln!(
            "serial: {}",
            String::from_utf8_lossy(
                &native.serial_output()[native.serial_output().len().saturating_sub(400)..]
            )
        );
        std::eprintln!("{}", native.jit_report());
        let Some(start) = failed else { return };
        // Replay to the last matching chunk, then narrow to 16 steps.
        let mut expected = make(false);
        let mut native = make(true);
        for _ in 0..start / CHUNK {
            expected.run_steps(CHUNK).unwrap();
            native.run_steps(CHUNK).unwrap();
        }
        assert!(same(&expected, &native), "replay diverged earlier");
        loop {
            let before = (
                expected.cpu.eip,
                expected.cpu.regs,
                expected.cpu.effective_eflags(),
            );
            let code: Vec<u8> = (0..24)
                .map(|i| {
                    let linear = expected
                        .cpu
                        .seg(SegmentReg::Cs)
                        .base
                        .wrapping_add(before.0 + i);
                    expected.bus.ram.get(linear as usize).copied().unwrap_or(0)
                })
                .collect();
            expected.run_steps(16).unwrap();
            native.run_steps(16).unwrap();
            if !same(&expected, &native) || expected.ram() != native.ram() {
                let first = expected
                    .ram()
                    .iter()
                    .zip(native.ram())
                    .position(|(a, b)| a != b);
                panic!(
                    "diverged after {:#x} {:08x?} eflags {:#x}\ncode {code:02x?}\nexpected eip {:#x} {:08x?} {:#x}\nnative   eip {:#x} {:08x?} {:#x}\nfirst RAM difference {first:x?}",
                    before.0,
                    before.1,
                    before.2,
                    expected.cpu.eip,
                    expected.cpu.regs,
                    expected.cpu.effective_eflags(),
                    native.cpu.eip,
                    native.cpu.regs,
                    native.cpu.effective_eflags()
                );
            }
        }
    }

    /// Random straight-line integer code with forward branches, calls, stack
    /// pairs, RMW memory and FS accesses, looping over a small data region.
    #[cfg(all(feature = "jit", target_arch = "arm"))]
    fn arm_random_program(seed: u32, supervisor: bool) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9) | 1;
        let mut next = move |n: u32| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state % n
        };
        const FREE: [u8; 4] = [0, 1, 2, 7];
        const ANY: [u8; 6] = [0, 1, 2, 3, 6, 7];
        const BYTE: [u8; 6] = [0, 1, 2, 4, 5, 6];
        let mut code = Vec::new();
        for _ in 0..48 {
            let r = FREE[next(4) as usize];
            let s = ANY[next(6) as usize];
            let b = BYTE[next(6) as usize];
            let op = (next(8) as u8) << 3;
            let memory = |reg: u8, choice: u32| {
                [
                    0x40 | reg << 3 | if choice & 1 == 0 { 3 } else { 6 },
                    (choice >> 1) as u8 & 0x7f,
                ]
            };
            let m = memory(r, next(256));
            let mb = memory(b, next(256));
            match next(40) {
                0..=3 => code.extend([op | 3, 0xc0 | r << 3 | s]),
                4 => code.extend([op | 1, 0xc0 | s << 3 | r]),
                5 => code.extend([op | 2, 0xc0 | b << 3 | BYTE[next(6) as usize]]),
                6 => code.extend([0x66, op | 3, 0xc0 | r << 3 | s]),
                7 => code.extend([op | 3, m[0], m[1]]),
                8 => code.extend([op | 1, m[0], m[1]]),
                9 => code.extend([op, mb[0], mb[1]]),
                10 => code.extend([0x83, m[0] & !0x38 | op, m[1], next(256) as u8]),
                11 => code.extend([0x80, m[0] & !0x38 | op, m[1], next(256) as u8]),
                12 => code.extend([0x8b, m[0], m[1]]),
                13 => code.extend([0x89, m[0], m[1]]),
                14 => code.extend([0x8a, mb[0], mb[1]]),
                15 => code.extend([0x66, 0x89, m[0], m[1]]),
                16 => {
                    code.push(0xb8 | r);
                    code.extend(next(u32::MAX).to_le_bytes());
                }
                17 => code.push(0x40 | r | (next(2) as u8) << 3),
                18 => code.extend([0xff, m[0] & !0x38 | (next(2) as u8) << 3, m[1]]),
                19 => code.extend([0xfe, mb[0] & !0x38, mb[1]]),
                20 => {
                    let group = [0, 1, 4, 5, 7][next(5) as usize];
                    let shift = [4, 5, 7][next(3) as usize];
                    match next(7) {
                        0 => code.extend([0xc1, 0xc0 | group << 3 | r, next(32) as u8]),
                        1 => code.extend([0xd3, 0xc0 | group << 3 | r]),
                        2 => code.extend([0xd3, m[0] & !0x38 | shift << 3, m[1]]),
                        3 => code.extend([0xc0, mb[0] & !0x38 | shift << 3, mb[1], next(32) as u8]),
                        4 => code.extend([0xd2, 0xc0 | shift << 3 | b]),
                        5 => code.extend([0x66, 0xc1, 0xc0 | shift << 3 | r, next(32) as u8]),
                        _ => {
                            let op = [0xa4, 0xac, 0xa5, 0xad][next(4) as usize];
                            code.extend([0x0f, op, 0xc0 | s << 3 | r]);
                            if op & 1 == 0 {
                                code.push(next(32) as u8);
                            }
                        }
                    }
                }
                21 => code.extend([0x0f, 0xaf, 0xc0 | r << 3 | s]),
                22 => code.extend([0x6b, m[0], m[1], next(256) as u8]),
                23 => code.extend([0xf7, 0xe0 | (next(2) as u8) << 3 | s]),
                24 => code.extend([0x0f, [0xb6, 0xbe, 0xb7, 0xbf][next(4) as usize], m[0], m[1]]),
                25 => code.extend([0x50 | s, 0x58 | r]),
                26 => code.extend([
                    0x39,
                    0xc0 | s << 3 | r,
                    0x70 | next(16) as u8,
                    2,
                    0x01,
                    0xc8,
                ]),
                27 => code.extend([
                    0x0f,
                    0x90 | next(16) as u8,
                    0xc0 | [0, 1, 2][next(3) as usize],
                ]),
                28 => code.extend([0x0f, 0x40 | next(16) as u8, m[0], m[1]]),
                29 => code.extend(if next(2) == 0 {
                    [0x87, m[0], m[1]]
                } else {
                    [0x86, mb[0], mb[1]]
                }),
                30 => code.extend([0x0f, [0xc1, 0xb1][next(2) as usize], m[0], m[1]]),
                31 => code.extend([0x0f, 0xba, 0xe0 | r, next(64) as u8]),
                32 => code.extend([
                    0x0f,
                    0xba,
                    m[0] & !0x38 | (5 + next(3) as u8) << 3,
                    m[1],
                    next(32) as u8,
                ]),
                33 => code.extend([0x0f, [0xbc, 0xbd][next(2) as usize], 0xc0 | r << 3 | s]),
                34 => code.push([0xf8, 0xf9, 0xf5, 0x99, 0x98][next(5) as usize]),
                35 => code.extend([0x8d, 0x44 | r << 3, 0x73, next(256) as u8]),
                36 => code.extend([0xe8, 0, 0, 0, 0]), // patched to the subroutine below
                37 => code.extend([0x64, 0x8b, m[0], m[1]]),
                38 => {
                    // MOVS with either direction from ESI's area to EDI's.
                    code.extend([[0xfc, 0xfd][next(2) as usize], 0xbf]);
                    code.extend((0x8100 + next(0x400)).to_le_bytes());
                    match next(3) {
                        0 => code.push(0xa4),
                        1 => code.push(0xa5),
                        _ => code.extend([0x66, 0xa5]),
                    }
                    code.extend([0xfc, 0x0f, 0xc8 | r]);
                }
                // MOV EAX,CR3; MOV CR3,EAX: later accesses miss and refill.
                39 if supervisor && next(4) == 0 => {
                    code.extend([0x0f, 0x20, 0xd8, 0x0f, 0x22, 0xd8])
                }
                // A store into the watched code page retires its blocks.
                39 if next(4) == 0 => code.extend([0xa3, 0x00, 0x18, 0x00, 0x00]),
                _ => code.extend([0xf7, [0xd0, 0xd8][next(2) as usize] | r]),
            }
            if code.ends_with(&[0xe8, 0, 0, 0, 0]) {
                code.truncate(code.len() - 5);
                code.push(0xe8);
                code.extend([0xff; 4]); // marker
            }
        }
        // dec ebp; jnz start; jmp $; subroutine: add eax,ecx; ret
        code.push(0x4d);
        let back = -(code.len() as i32 + 6);
        code.extend([0x0f, 0x85]);
        code.extend(back.to_le_bytes());
        code.extend([0xeb, 0xfe]);
        let subroutine = code.len();
        code.extend([0x01, 0xc8, 0xc3]);
        let mut at = 0;
        while at + 5 <= subroutine {
            if code[at] == 0xe8 && code[at + 1..at + 5] == [0xff; 4] {
                let rel = subroutine as i32 - (at as i32 + 5);
                code[at + 1..at + 5].copy_from_slice(&rel.to_le_bytes());
                at += 5;
            } else {
                at += 1;
            }
        }
        code
    }

    #[cfg(all(feature = "jit", target_arch = "arm"))]
    #[test]
    fn arm_jit_random_programs_match_interpreter() {
        let (mut exits, mut invalidations) = (0, 0);
        for seed in 0..24u32 {
            let paging = seed % 3 != 0;
            let user = seed % 3 == 2;
            let code = arm_random_program(seed, !user);
            let make = |jit: bool| {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                if user {
                    cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(0x1b, 0xc0fb));
                    cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::flat(0x23, 0xc0f3));
                }
                if paging {
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x20000;
                }
                let mut random = seed.wrapping_add(0x1234_5678);
                for reg in [0, 1, 2, 7] {
                    random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    cpu.regs[reg] = random;
                }
                cpu.regs[3] = 0x8000;
                cpu.regs[4] = 0x9ff0;
                cpu.regs[5] = 1000;
                cpu.regs[6] = 0x8800;
                cpu.replace_eflags(0x202 | (seed.wrapping_mul(0x155) & 0x8d5));
                let mut bus = HostBus::new(0x40000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x20000, 0x21000 | 7);
                for page in 0..64 {
                    write_ram_u32(&mut bus, 0x21000 + page * 4, (page as u32) << 12 | 7);
                }
                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                for (i, byte) in bus.ram[0x8000..0x8900].iter_mut().enumerate() {
                    *byte = (i as u32).wrapping_mul(seed | 0x51) as u8;
                }
                let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                m.set_jit_enabled(jit).unwrap();
                m
            };
            let mut expected = make(false);
            let mut native = make(true);
            arm_compare_runs(
                &mut expected,
                &mut native,
                &[1, 2, 3, 5, 8, 13, 100, 1000, 4096, 20_000, 40_000, 100_000],
                &format!("seed {seed} paging {paging} user {user}"),
            );
            let stats = &native.jit.as_ref().unwrap().stats;
            assert!(stats.native_steps > 100_000, "seed {seed}: {stats:?}");
            exits += stats.memory_exits;
            invalidations += stats.invalidations + stats.revalidations;
            // Blocks chain natively; only interpreter handoffs return.
            assert!(stats.invocations * 10 < stats.native_steps, "{stats:?}");
        }
        // CR3 reloads exercise refills; code-page stores retire versions,
        // after which unchanged blocks are revalidated.
        assert!(
            exits > 1000 && invalidations > 100,
            "{exits} {invalidations}"
        );
    }

    /// A call target is remapped to other code. INVLPG and a non-global CR3
    /// reload must retire the chained entry; a global one survives the reload.
    #[cfg(all(feature = "jit", target_arch = "arm"))]
    #[test]
    fn arm_jit_chains_follow_remapped_code() {
        for (global, invlpg) in [(false, true), (false, false), (true, false)] {
            let g = if global { 0x100 } else { 0 };
            let mut code = alloc::vec![0xbd, 0xe8, 0x03, 0x00, 0x00]; // mov ebp,1000
            code.extend([0xe8, 0xf6, 0x1f, 0x00, 0x00, 0x4d, 0x75, 0xf8]); // call 0x3000; dec; jnz
            code.extend([0xc7, 0x05, 0x0c, 0x10, 0x02, 0x00]); // mov [0x2100c], B
            code.extend((0x5007u32 | g).to_le_bytes());
            code.extend(if invlpg {
                [0x0f, 0x01, 0x3d, 0x00, 0x30, 0x00, 0x00] // invlpg [0x3000]
            } else {
                [0x0f, 0x20, 0xd9, 0x0f, 0x22, 0xd9, 0x90] // mov ecx,cr3; mov cr3,ecx
            });
            code.extend([0xbd, 0x32, 0x00, 0x00, 0x00]);
            code.extend([0xe8, 0xd8, 0x1f, 0x00, 0x00, 0x4d, 0x75, 0xf8]);
            code.extend([0x0f, 0x01, 0x3d, 0x00, 0x30, 0x00, 0x00]);
            code.extend([0xbd, 0x32, 0x00, 0x00, 0x00]);
            code.extend([0xe8, 0xc4, 0x1f, 0x00, 0x00, 0x4d, 0x75, 0xf8]);
            code.extend([0xeb, 0xfe]);
            let make = |jit: bool| {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0x20000;
                if global {
                    cpu.cr4 |= crate::x86::cpu::CR4_PGE;
                }
                cpu.regs[0] = 0;
                cpu.regs[4] = 0x9ff0;
                let mut bus = HostBus::new(0x40000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x20000, 0x21000 | 7);
                for page in 0..64 {
                    write_ram_u32(&mut bus, 0x21000 + page * 4, (page as u32) << 12 | 7);
                }
                write_ram_u32(&mut bus, 0x2100c, 0x4007 | g);
                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                bus.ram[0x4000..0x4004].copy_from_slice(&[0x83, 0xc0, 0x01, 0xc3]);
                bus.ram[0x5000..0x5006].copy_from_slice(&[0x05, 0x00, 0x01, 0x00, 0x00, 0xc3]);
                let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                m.set_jit_enabled(jit).unwrap();
                m
            };
            let mut expected = make(false);
            let mut native = make(true);
            let label = format!("global {global} invlpg {invlpg}");
            arm_compare_runs(&mut expected, &mut native, &[7, 100, 1000, 10_000], &label);
            let middle = if global { 50 } else { 50 * 0x100 };
            assert_eq!(native.cpu.regs[0], 1000 + middle + 50 * 0x100, "{label}");
            let stats = &native.jit.as_ref().unwrap().stats;
            assert!(stats.native_steps > 3000, "{label}: {stats:?}");
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_matches_interpreter_across_budgets_memory_boundaries_and_code_changes() {
        fn machine(jit: bool) -> Machine {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.regs[0] = 0x7fff_fffc;
            cpu.regs[1] = 1;
            let mut bus = HostBus::new(0x10000);
            bus.ram[0x1000..0x100a].copy_from_slice(&[
                0x01, 0xc8, // add eax,ecx
                0x41, // inc ecx
                0xa3, 0x00, 0x20, 0x00, 0x00, // mov [2000h],eax
                0xeb, 0xf6, // jmp 1000h
            ]);
            let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
            m.set_jit_enabled(jit).unwrap();
            m
        }
        let mut expected = machine(false);
        let mut native = machine(true);
        for budget in [0, 1, 2, 3, 31, 63, 127, 257, 1_050_000] {
            let reference = expected.run_steps(budget).unwrap();
            let actual = native.run_steps(budget).unwrap();
            assert_eq!(actual.ran, reference.ran);
            assert_eq!(native.cpu.eip, expected.cpu.eip);
            assert_eq!(native.cpu.regs, expected.cpu.regs);
            assert_eq!(
                native.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(native.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(native.ram(), expected.ram());
            // A host/device write does not invalidate the interpreter's live-byte
            // fetch cache, but must invalidate a JIT block containing these bytes.
            let changed = expected.bus.ram[0x1002] ^ 8; // inc ecx <-> dec ecx
            expected
                .bus
                .write_phys_u8(PhysAddr(0x1002), changed)
                .unwrap();
            native.bus.write_phys_u8(PhysAddr(0x1002), changed).unwrap();
        }
        let stats = &native.jit.as_ref().unwrap().stats;
        assert!(stats.native_steps > 1000);
        assert!(stats.native_memory_writes > 1000);
        assert!(stats.content_changes > 0);
        assert!(stats.linked_edges > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_code_writes_retain_only_identical_native_dependencies() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x4000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            bus.ram[0x1000..0x1006].copy_from_slice(&[0x40, 0xe9, 0x39, 0, 0, 0]);
            // A linked target straddles a coverage word; another fills a word.
            bus.ram[0x103f..0x104a].copy_from_slice(&[0x81, 0xc3, 1, 0, 0, 0, 0xe9, 0x36, 0, 0, 0]);
            for at in (0x1080..0x10cb).step_by(3) {
                bus.ram[at..at + 3].copy_from_slice(&[0x83, 0xc1, 1]);
            }
            bus.ram[0x10cb..0x10d0].copy_from_slice(&[0xe9, 0x30, 0xff, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..10 {
            let writes: &[(u64, u8)] = match phase {
                1 => &[(0x1800, 0x55)],              // unrelated data in the code page
                2 => &[(0x1041, 1)],                 // unchanged value
                3 => &[(0x1041, 0xfe), (0x1041, 1)], // restored before execution
                4 => &[(0x1041, 2)], // change a direct-linked target, not its caller
                5 => &[(0x10a0, 5)], // change a byte within a fully covered word
                6 => &[(0x1800, 0x4f), (0x1801, 0xeb), (0x1802, 0xfd)],
                7 => &[(0x1800, 0x47)], // code added after earlier revalidation
                8 => &[(0x1ffd, 0x46), (0x1ffe, 0xeb), (0x1fff, 0xfd)],
                9 => &[(0x1fff, 0xfe)], // last byte of the physical code page
                _ => &[],
            };
            for bus in [&mut reference_bus, &mut native_bus] {
                for &(address, value) in writes {
                    bus.write_phys_u8(PhysAddr(address), value).unwrap();
                }
            }
            for cpu in [&mut expected, &mut actual] {
                cpu.eip = match phase {
                    6 | 7 => 0x1800,
                    8 | 9 => 0x1ffd,
                    _ => 0x1000,
                };
            }
            let reused = engine.stats.content_reuses;
            let changed = engine.stats.content_changes;
            for budget in [1, 3, 2048] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase}");
                assert_eq!(actual.regs, expected.regs, "phase={phase}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
                steps += native.ran;
            }
            if matches!(phase, 2..=3) {
                assert!(engine.stats.content_reuses > reused);
                assert_eq!(engine.stats.content_changes, changed);
            }
            if matches!(phase, 4 | 5 | 7 | 9) {
                assert!(engine.stats.content_changes > changed, "phase={phase}");
            }
        }
        assert!(engine.stats.native_steps > 10_000);
        assert!(engine.stats.linked_edges > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_negative_entries_recheck_changed_prefix_opcode_and_modrm() {
        for long_prefix in [false, true] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                let mut bus = HostBus::new(0x4000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                bus.ram[0x1000] = 0x43; // native INC before the unsupported entry
                let end = if long_prefix {
                    bus.ram[0x1001..0x100d].fill(0x3e);
                    bus.ram[0x100d..0x1010].copy_from_slice(&[0x0f, 0x31, 0x90]);
                    0x1010 // RDTSC, NOP; replace with a 15-byte MOVZX
                } else {
                    bus.ram[0x1001..0x1004].copy_from_slice(&[0xf3, 0x40, 0x90]);
                    0x1004 // REP INC is interpreted; DS override is native
                };
                bus.ram[end..end + 2].copy_from_slice(&[0xeb, (0x1000 - end as i32 - 2) as u8]);
                (cpu, bus)
            };
            let (mut expected, mut reference_bus) = make();
            let (mut actual, mut native_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            let mut steps = 0;
            for phase in 0..4 {
                let writes: &[(u64, u8)] = match phase {
                    1 => &[(0x1800, 0x55)],
                    2 => &[(0x1801, 0x66)],
                    3 if long_prefix => &[(0x100e, 0xb6), (0x100f, 0xc0)],
                    3 => &[(0x1001, 0x3e)],
                    _ => &[],
                };
                for bus in [&mut reference_bus, &mut native_bus] {
                    for &(address, value) in writes {
                        bus.write_phys_u8(PhysAddr(address), value).unwrap();
                    }
                }
                expected.eip = 0x1000;
                actual.eip = 0x1000;
                let compiled = engine.stats.compiled_blocks;
                let interpreted = engine.stats.interpreted_steps;
                let reused = engine.stats.content_reuses;
                for budget in [1, 3, 2048] {
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
                    let native = engine
                        .run_batch(&mut actual, &mut native_bus, steps, budget)
                        .unwrap();
                    assert_eq!(native, reference, "long={long_prefix} phase={phase}");
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(native_bus.ram, reference_bus.ram);
                    steps += native.ran;
                }
                if phase == 2 {
                    assert!(engine.stats.content_reuses > reused);
                    assert_eq!(engine.stats.compiled_blocks, compiled);
                    assert!(engine.stats.interpreted_steps > interpreted + 500);
                } else if phase == 3 {
                    assert!(engine.stats.content_changes > 0);
                    assert!(engine.stats.compiled_blocks > compiled);
                    assert!(engine.stats.interpreted_steps < interpreted + 32);
                }
            }
            assert!(engine.stats.native_steps > 4000);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_smc_revokes_an_existing_writable_alias_before_compiling_the_page() {
        let make = |jit| {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.regs[3] = 0x6008; // Warm the store on data before targeting code.
            let mut bus = HostBus::new(0x10000);
            bus.ram[0x1000..0x100f].copy_from_slice(&[
                0x90, // fill execute translation before native entry
                0xb8, 0, 0, 0, 0, // mov eax,0
                0xc6, 0x03, 0x48, // mov byte [ebx],48h
                0x40, // decoded INC must not execute after the store
                0xa3, 0x00, 0x60, 0, 0, // publish eax
            ]);
            bus.ram[0x100f..0x1011].copy_from_slice(&[0xeb, 0xf0]); // back to 1001h
            let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
            m.set_jit_enabled(jit).unwrap();
            mmu::write_linear_u32(&mut m.cpu, &mut m.bus, LinearAddr(0x1020), 0x9090_9090).unwrap();
            assert!(
                m.cpu
                    .cached_current_write_translation(LinearAddr(0x1000))
                    .is_some()
            );
            m
        };
        let mut reference = make(false);
        let mut native = make(true);
        for (phase, budget) in [20, 20, 1, 3, 100].into_iter().enumerate() {
            if phase == 1 {
                // Execute the already compiled store with a code-page target.
                // It must exit before replacing the following INC with DEC.
                for machine in [&mut reference, &mut native] {
                    machine.cpu.regs[3] = 0x1009;
                    machine.cpu.eip = 0x1001;
                }
            }
            reference.run_steps(budget).unwrap();
            native.run_steps(budget).unwrap();
            assert_eq!(native.cpu.regs, reference.cpu.regs);
            assert_eq!(native.cpu.eip, reference.cpu.eip);
            assert_eq!(
                native.cpu.effective_eflags(),
                reference.cpu.effective_eflags()
            );
            assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
            assert_eq!(native.ram(), reference.ram());
        }
        assert!(
            native
                .cpu
                .cached_current_write_translation(LinearAddr(0x1000))
                .is_none()
        );
        assert_eq!(&native.ram()[0x6000..0x6004], &u32::MAX.to_le_bytes());
        assert!(native.jit.as_ref().unwrap().stats.memory_exits > 0);
        assert!(native.jit.as_ref().unwrap().stats.content_changes > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_lazy_tables_reclaim_only_for_another_native_page() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for page in [1, 2] {
                let start = page * 4096;
                bus.ram[start..start + 3].copy_from_slice(&[0x40, 0xeb, 0xfd]); // INC; loop
            }
            for page in 3..16 {
                let start = page * 4096;
                bus.ram[start..start + 2].copy_from_slice(&[0x0f, 0x31]); // interpreted RDTSC
            }
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::with_limits(64 * 1024, 1).unwrap();
        let mut runs = alloc::vec![(0x1000, 128)];
        for page in 3..16 {
            runs.extend(core::iter::repeat_n((page * 4096, 1), 8));
        }
        runs.extend([(0x2000, 128), (0x1000, 128)]);
        let mut steps = 0;
        for (index, (eip, budget)) in runs.iter().copied().enumerate() {
            expected.eip = eip;
            actual.eip = eip;
            let reference =
                Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
            let native = engine
                .run_batch(&mut actual, &mut native_bus, steps, budget)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(native_bus.ram, reference_bus.ram);
            if index < runs.len() - 2 {
                assert_eq!(engine.stats.table_clears, 0, "cold pages consumed a table");
            }
            steps += native.ran;
        }
        assert_eq!(engine.stats.table_clears, 2);
        assert_eq!(engine.stats.capacity_clears, 0);
        assert!(engine.stats.native_steps > 300);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_capacity_reclamation_rewatches_the_block_being_published() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x4000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            // Cross a code page in both directions while repeatedly replacing
            // the source. Reclamation must retire every linked destination.
            bus.ram[0x1000..0x1006].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x2000..0x2006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::with_capacity(4096).unwrap();
        for round in 0..128 {
            let opcode = if round & 1 == 0 { 0x40 } else { 0x48 };
            for bus in [&mut reference_bus, &mut native_bus] {
                bus.write_phys_u8(PhysAddr(0x1000), opcode).unwrap();
            }
            let reference =
                Interpreter::run_batch(&mut expected, &mut reference_bus, round * 64, 64);
            let native = engine
                .run_batch(&mut actual, &mut native_bus, round * 64, 64)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(native_bus.ram, reference_bus.ram);
        }
        assert!(engine.stats.arena_clears > 1);
        assert!(engine.stats.native_steps > 4000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_reclaimed_code_can_be_written_as_data_and_promoted_again() {
        let make = |jit| {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x10000);
            bus.ram[0x1000..0x1003].copy_from_slice(&[0x40, 0xeb, 0xfd]); // inc eax; loop
            bus.ram[0x2000..0x2005].copy_from_slice(&[0x89, 0x03, 0x40, 0xeb, 0xfb]); // store; inc; loop
            let mut machine = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
            machine.set_jit_enabled(jit).unwrap();
            machine
        };
        let mut reference = make(false);
        let mut native = make(true);
        for phase in 0..6 {
            if phase == 1 {
                // Exercise the same retirement path used for arena capacity,
                // without discarding the bus's stable version allocation.
                native.jit.as_mut().unwrap().clear(&mut native.bus).unwrap();
            }
            for machine in [&mut reference, &mut native] {
                machine.cpu.eip = if phase % 2 == 0 { 0x1000 } else { 0x2000 };
                machine.cpu.regs[3] = 0x1000;
                if phase == 2 || phase == 4 {
                    // The old code page has become writable data. Reinstall
                    // code through the device path, leaving its TLB alias live.
                    let opcode = if phase == 2 { 0x40 } else { 0x48 };
                    for (offset, byte) in [opcode, 0xeb, 0xfd].into_iter().enumerate() {
                        machine
                            .bus
                            .write_phys_u8(PhysAddr(0x1000 + offset as u64), byte)
                            .unwrap();
                    }
                }
            }
            let writes_before = native.jit.as_ref().unwrap().stats.native_memory_writes;
            let exits_before = native.jit.as_ref().unwrap().stats.memory_exits;
            reference.run_steps(200).unwrap();
            native.run_steps(200).unwrap();
            assert_eq!(native.cpu.regs, reference.cpu.regs, "phase {phase}");
            assert_eq!(native.cpu.eip, reference.cpu.eip);
            assert_eq!(
                native.cpu.effective_eflags(),
                reference.cpu.effective_eflags()
            );
            assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
            assert_eq!(native.ram(), reference.ram());
            if phase % 2 == 1 {
                assert!(native.jit.as_ref().unwrap().stats.native_memory_writes > writes_before);
                assert!(
                    native
                        .cpu
                        .cached_current_write_translation(LinearAddr(0x1000))
                        .is_some()
                );
                if phase > 1 {
                    // The first write retires live code through the checked
                    // path; subsequent stores may use the page as native data.
                    assert!(native.jit.as_ref().unwrap().stats.memory_exits > exits_before);
                }
            } else if phase >= 2 {
                assert!(
                    native
                        .cpu
                        .cached_current_write_translation(LinearAddr(0x1000))
                        .is_none()
                );
                assert_eq!(
                    native.jit.as_ref().unwrap().stats.native_memory_writes,
                    writes_before
                );
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_arithmetic_commits_values_and_flags_for_each_width() {
        for bits in [8, 16, 32] {
            for group in 0..7 {
                let make = |jit| {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.regs[0] = 0x8000_ffff;
                    cpu.regs[2] = 0x6000;
                    cpu.replace_eflags(0x803);
                    let mut bus = HostBus::new(0x10000);
                    write_ram_u32(&mut bus, 0x6000, 0x7fff_ff7f);
                    let mut code = Vec::new();
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([group * 8 + u8::from(bits != 8), 0x02]); // op [edx],al/ax/eax
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([if bits == 8 { 0xfe } else { 0xff }, 0x02]); // inc [edx]
                    code.push(0x40); // inc eax: vary the next operand, preserve CF
                    let delta = -(code.len() as i8 + 2);
                    code.extend([0xeb, delta as u8]);
                    bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                    let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                    m.set_jit_enabled(jit).unwrap();
                    m
                };
                let mut reference = make(false);
                let mut native = make(true);
                for _ in 0..100 {
                    reference.run_steps(7).unwrap();
                    native.run_steps(7).unwrap();
                    assert_eq!(native.cpu.regs, reference.cpu.regs, "{bits} {group}");
                    assert_eq!(
                        native.cpu.effective_eflags(),
                        reference.cpu.effective_eflags(),
                        "{bits} {group}"
                    );
                    assert_eq!(native.cpu.eip, reference.cpu.eip);
                    assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
                    assert_eq!(native.ram(), reference.ram());
                }
                assert!(native.jit.as_ref().unwrap().stats.native_memory_writes > 100);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_full_multiply_reads_memory_before_committing_the_accumulator_pair() {
        for bits in [8, 16, 32] {
            for signed in [false, true] {
                let make = |jit| {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.regs[0] = 0x8000_ffff;
                    cpu.regs[2] = 0xabcd_ffff;
                    cpu.replace_eflags(0x8d7);
                    let mut bus = HostBus::new(0x10000);
                    write_ram_u32(&mut bus, 0x6000, 0x8000_ff7f);
                    let mut code = vec![0x40];
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([
                        if bits == 8 { 0xf6 } else { 0xf7 },
                        if signed { 0x2d } else { 0x25 },
                    ]);
                    code.extend(0x6000u32.to_le_bytes());
                    // Consume the preserved parity, then traverse a direct
                    // edge; destination registers are inputs on the next pass.
                    code.extend([0x0f, 0x9a, 0xc3]);
                    let delta = -(code.len() as i8 + 2);
                    code.extend([0xeb, delta as u8]);
                    bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                    let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                    m.set_jit_enabled(jit).unwrap();
                    m
                };
                let mut reference = make(false);
                let mut native = make(true);
                for _ in 0..100 {
                    reference.run_steps(7).unwrap();
                    native.run_steps(7).unwrap();
                    assert_eq!(native.cpu.regs, reference.cpu.regs, "{bits} {signed}");
                    assert_eq!(
                        native.cpu.effective_eflags(),
                        reference.cpu.effective_eflags()
                    );
                    assert_eq!(native.cpu.eip, reference.cpu.eip);
                    assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
                    assert_eq!(native.ram(), reference.ram());
                }
                assert!(native.jit.as_ref().unwrap().stats.native_memory_reads > 100);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_ram_reads_addressing_segments_and_stack_returns_match_interpreter() {
        for (stack, leave16) in [(false, false), (true, false), (true, true)] {
            let make = |jit| {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs = [0, 0, 0x6000, 0, 0x7000, 0x6100, 0, 0];
                if stack {
                    cpu.regs[4] = 0x17000;
                    cpu.regs[5] = 0x16100;
                }
                cpu.set_seg_cache(
                    SegmentReg::Fs,
                    SegmentCache::new(0x18, 0x200, u32::MAX, 0xc092),
                );
                let mut bus = HostBus::new(0x20000);
                for index in 0x6000..0x6300 {
                    bus.ram[index] = (index * 17) as u8;
                }
                if stack {
                    bus.ram[0x1000..0x1008]
                        .copy_from_slice(&[0xe8, 0x0b, 0, 0, 0, 0x40, 0xeb, 0xf8]);
                    let mut callee = Vec::new();
                    if leave16 {
                        callee.push(0x66);
                    }
                    callee.extend([0x55, 0x89, 0xe5, 0x50, 0x41, 0x5a]);
                    if leave16 {
                        callee.push(0x66);
                    }
                    callee.extend([0xc9, 0xc3]);
                    bus.ram[0x1010..0x1010 + callee.len()].copy_from_slice(&callee);
                } else {
                    let mut code = vec![
                        0x8b, 0x02, // mov eax,[edx]
                        0x03, 0x04, 0x8a, // add eax,[edx+ecx*4]
                        0x0f, 0xbe, 0x5a, 3, // movsx ebx,byte [edx+3]
                        0x0f, 0xb7, 0x75, 0xfe, // movzx esi,word [ebp-2]
                        0x8d, 0x7c, 0x8a, 0xfc, // lea edi,[edx+ecx*4-4]
                        0x66, 0x83, 0x7a, 2, 0xff, // cmp word [edx+2],-1
                        0x66, 0x83, 0x7a, 2, 0, // cmp word [edx+2],0
                        0x0f, 0xba, 0x22, 0xff, // bt dword [edx],255 (bit 31)
                        0x0f, 0xaf, 0x32, // imul esi,dword [edx]
                        0x66, 0x0f, 0xaf, 0x5a, 2, // imul bx,word [edx+2]
                        0x6b, 0x7a, 4, 0xfe, // imul edi,dword [edx+4],-2
                        0x66, 0x69, 0x1a, 0xff, 0x7f, // imul bx,word [edx],32767
                        0x0f, 0x92, 0x42, 4, // setb byte [edx+4]
                        0x0f, 0x9a, 0x42, 5, // setp byte [edx+5], preserved PF
                        0x64, 0x8b, 0x1a, // mov ebx,fs:[edx], non-flat segment
                        0x83, 0xc1, 1, // add ecx,1
                        0x83, 0xe1, 7, // and ecx,7
                    ];
                    let displacement = -(code.len() as i8 + 2);
                    code.extend([0xeb, displacement as u8]);
                    bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                }
                let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                m.set_jit_enabled(jit).unwrap();
                m
            };
            let mut expected = make(false);
            let mut native = make(true);
            for budget in [1, 2, 3, 5, 7, 31, 63, 10_000] {
                expected.run_steps(budget).unwrap();
                native.run_steps(budget).unwrap();
                assert_eq!(
                    native.cpu.eip, expected.cpu.eip,
                    "stack={stack} budget={budget}"
                );
                assert_eq!(native.cpu.regs, expected.cpu.regs);
                assert_eq!(
                    native.cpu.effective_eflags(),
                    expected.cpu.effective_eflags()
                );
                assert_eq!(native.cpu.tsc(), expected.cpu.tsc());
                assert_eq!(native.ram(), expected.ram());
            }
            assert!(native.jit.as_ref().unwrap().stats.native_memory_reads > 1000);
            if !stack {
                assert!(native.jit.as_ref().unwrap().stats.native_memory_writes > 1000);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_data_segment_loads_preserve_descriptors_and_fault_order() {
        for (segment, prefix) in [
            (SegmentReg::Es, 0x26),
            (SegmentReg::Ds, 0x3e),
            (SegmentReg::Fs, 0x64),
            (SegmentReg::Gs, 0x65),
        ] {
            for cpl in [0, 3] {
                for source in 0..4 {
                    let memory = source == 1;
                    let stack = source >= 2;
                    let operand16 = source == 2;
                    let ldt = matches!(segment, SegmentReg::Fs | SegmentReg::Gs);
                    let selector = 0x10 | cpl | if ldt { 4 } else { 0 };
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.set_seg_cache(
                            SegmentReg::Cs,
                            SegmentCache::flat(8 | cpl, 0xc09b | (cpl << 5)),
                        );
                        cpu.set_seg_cache(
                            SegmentReg::Ss,
                            SegmentCache::flat(0x10 | cpl, 0xc093 | (cpl << 5)),
                        );
                        cpu.cr0 |= CR0_PG;
                        cpu.cr3 = 0x4000;
                        cpu.gdtr = crate::x86::cpu::TableReg {
                            base: 0x7000,
                            limit: 0x17,
                        };
                        cpu.ldtr = 0x28;
                        cpu.ldtr_cache = SegmentCache::new(0x28, 0x7000, 0x17, 0x82);
                        cpu.regs[1] = 0xbeef_0000 | u32::from(selector);
                        cpu.regs[2] = 0x2000;
                        cpu.regs[6] = 0x6100;
                        cpu.replace_eflags(3);
                        let mut bus = HostBus::new(0x18000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                        for (page, backing) in [
                            (1, 0x3000),
                            (2, 0xa000),
                            (6, 0x8000),
                            (7, 0x9000),
                            (8, 0xb000),
                        ] {
                            write_ram_u32(&mut bus, 0x5000 + page * 4, backing | 7);
                        }
                        let mut code = vec![0x40]; // INC's flags must survive the helper or a miss.
                        if stack {
                            if operand16 {
                                code.push(0x66);
                            }
                            code.extend_from_slice(match segment {
                                SegmentReg::Es => &[0x07][..],
                                SegmentReg::Ds => &[0x1f][..],
                                SegmentReg::Fs => &[0x0f, 0xa1][..],
                                SegmentReg::Gs => &[0x0f, 0xa9][..],
                                _ => unreachable!(),
                            });
                            // Reuse the same selector on each loop without
                            // changing flags or issuing a stack write.
                            code.extend([0x8d, 0x64, 0x24, if operand16 { 0xfe } else { 0xfc }]);
                        } else if memory {
                            // Source uses SS even while the destination DS/ES/FS/GS changes.
                            code.extend([0x36, 0x66, 0x8e, 2 | (segment as u8) << 3]);
                        } else {
                            code.extend([0x8e, 0xc1 | (segment as u8) << 3]);
                        }
                        code.extend([
                            prefix,
                            0x89,
                            0x06, // mov segment:[esi],eax
                            prefix,
                            0x8b,
                            0x1e, // mov ebx,segment:[esi]
                            0x8c,
                            0xc7 | (segment as u8) << 3, // mov edi,segment
                        ]);
                        let back = -(code.len() as i8 + 2);
                        code.extend([0xeb, back as u8]);
                        bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                        (cpu, bus)
                    };
                    let (mut expected, mut reference_bus) = make();
                    let (mut actual, mut native_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    let mut steps = 0;
                    for phase in 0..15 {
                        // Reload the same selector after editing its descriptor.
                        // The successor already has direct links under the old
                        // flat proof; neither selector equality nor a stale view
                        // may suppress the new base/limit/permission effects.
                        let null = phase == 4;
                        let split = phase == 11;
                        for (cpu, bus) in [
                            (&mut expected, &mut reference_bus),
                            (&mut actual, &mut native_bus),
                        ] {
                            cpu.eip = 0x1000;
                            cpu.regs[4] = 0x8ffe;
                            cpu.cr2 = 0xdead_beef;
                            cpu.regs[1] = 0xbeef_0000 | u32::from(if null { 0 } else { selector });
                            if phase == 13 {
                                cpu.regs[1] |= 3; // RPL3 cannot load a DPL0 data segment.
                            }
                            write_ram_u32(bus, 0xa000, cpu.regs[1]);
                            bus.ram[0xbffe..0xc000]
                                .copy_from_slice(&(cpu.regs[1] as u16).to_le_bytes());
                            let base = if phase == 1 { 0x200u32 } else { 0 };
                            let high = match phase {
                                2 => 0x00cf_9100,       // Read-only data: loading succeeds, store faults.
                                3 => 0x0040_9300,       // Byte-granular limit below ESI.
                                6 => 0x00cf_8900,       // System descriptor is not a data segment.
                                7 => 0x00cf_1300,       // Not present.
                                10 | 14 => 0x00cf_9200, // Unset descriptor A requires ordinary semantics.
                                _ => 0x00cf_9300,
                            } | if phase == 13 { 0 } else { u32::from(cpl) << 13 };
                            let offset = if split { 0xffc } else { 0x10 };
                            write_ram_u32(
                                bus,
                                0x9000 + offset,
                                (base << 16) | if phase == 3 { 0x60ff } else { 0xffff },
                            );
                            write_ram_u32(bus, if split { 0xb000 } else { 0x9014 }, high);
                            let table_base = if split { 0x7fec } else { 0x7000 };
                            cpu.gdtr.base = table_base;
                            cpu.ldtr_cache.base = table_base;
                            cpu.gdtr.limit = if phase == 12 { 0x16 } else { 0x17 };
                            cpu.ldtr_cache.limit = u32::from(cpu.gdtr.limit);
                            if (8..=11).contains(&phase) {
                                write_ram_u32(bus, 0x501c, if phase <= 9 { 0 } else { 0x9000 | 7 });
                                write_ram_u32(bus, 0x5008, if phase == 9 { 0 } else { 0xa000 | 7 });
                                write_ram_u32(
                                    bus,
                                    0x5020,
                                    if stack && phase == 9 { 0 } else { 0xb000 | 7 },
                                );
                                cpu.invalidate_translation_context();
                            }
                            if phase == 14 {
                                cpu.cr0 |= crate::x86::cpu::CR0_WP;
                                write_ram_u32(bus, 0x501c, 0x9000 | 5);
                                cpu.invalidate_translation_context();
                            }
                        }
                        let before = engine.stats.native_segment_loads;
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut reference_bus, steps, 128);
                        let native = engine
                            .run_batch(&mut actual, &mut native_bus, steps, 128)
                            .unwrap();
                        assert_eq!(
                            native, reference,
                            "{segment:?} cpl={cpl} source={source} phase={phase}"
                        );
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.cr2, expected.cr2);
                        for index in 0..6 {
                            let reg = SegmentReg::from_modrm_reg(index);
                            assert_eq!(actual.seg(reg), expected.seg(reg));
                        }
                        assert_eq!(native_bus.ram, reference_bus.ram);
                        if matches!(phase, 0 | 1 | 5 | 10 | 11) {
                            assert_eq!(native.stop, BatchStop::Budget);
                            if phase <= 5 || phase == 10 {
                                assert!(
                                    engine.stats.native_segment_loads > before,
                                    "{segment:?} phase={phase}"
                                );
                            }
                        } else {
                            assert!(matches!(native.stop, BatchStop::Step { .. }));
                            if phase == 8 || phase == 9 {
                                // A memory source faults before the descriptor fetch.
                                assert_eq!(
                                    actual.cr2,
                                    if phase == 9 && stack {
                                        0x8ffe
                                    } else if phase == 9 && memory {
                                        0x2000
                                    } else {
                                        0x7010
                                    }
                                );
                            }
                            if phase == 14 {
                                assert_eq!(actual.cr2, 0x7015);
                                assert_eq!(actual.regs[4], 0x8ffe);
                                assert_eq!(native_bus.ram[0x9015] & 1, 0);
                            }
                        }
                        if phase >= 6 && phase != 10 {
                            assert_eq!(engine.stats.native_segment_loads, before);
                        }
                        steps += native.ran;
                    }
                    assert!(engine.stats.linked_edges > 0);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_segment_pushes_store_only_a_word_and_commit_esp_after_faults() {
        for (segment, opcode) in [
            (SegmentReg::Es, &[0x06][..]),
            (SegmentReg::Ds, &[0x1e][..]),
            (SegmentReg::Fs, &[0x0f, 0xa0][..]),
            (SegmentReg::Gs, &[0x0f, 0xa8][..]),
        ] {
            for operand16 in [false, true] {
                for user in [false, true] {
                    for case in 0..5 {
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            if user {
                                cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(11, 0xc0fb));
                                cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::flat(19, 0xc0f3));
                            }
                            if case == 3 {
                                let mut ss = cpu.seg(SegmentReg::Ss);
                                ss.flags &= !0x4000; // A 16-bit stack uses shared semantics.
                                cpu.set_seg_cache(SegmentReg::Ss, ss);
                            }
                            cpu.set_seg_cache(
                                segment,
                                SegmentCache::new(if case == 4 { 0 } else { 0xbeef }, 0, 0, 0),
                            );
                            cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                            cpu.cr3 = 0x4000;
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                            write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                            write_ram_u32(&mut bus, 0x5018, 0x8000 | if case == 2 { 5 } else { 7 });
                            let mut code = vec![0x90];
                            if operand16 {
                                code.push(0x66);
                            }
                            code.extend_from_slice(opcode);
                            code.push(0xf4);
                            bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut expected_bus) = make();
                        let (mut actual, mut actual_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        for round in 0..4 {
                            let destination = if case == 1 { 0x6fff } else { 0x6ffe };
                            let high = if case == 3 { 0x1234_0000 } else { 0 };
                            let esp = high | (destination + if operand16 { 2 } else { 4 });
                            for (cpu, bus) in [
                                (&mut expected, &mut expected_bus),
                                (&mut actual, &mut actual_bus),
                            ] {
                                cpu.eip = 0x1000;
                                cpu.cr2 = 0xdead_beef;
                                cpu.regs[4] = esp;
                                cpu.replace_eflags(0x28d7);
                                bus.ram[0x8ffc..0x9004].fill(0x5a);
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                round * 2,
                                2,
                            );
                            let before = engine.stats.native_memory_writes;
                            let result = engine
                                .run_batch(&mut actual, &mut actual_bus, round * 2, 2)
                                .unwrap();
                            assert_eq!(
                                result, reference,
                                "segment={segment:?} operand16={operand16} user={user} case={case}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), 0x28d7);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                            let faults = matches!(case, 1 | 2);
                            assert_eq!(
                                actual.regs[4],
                                if faults { esp } else { high | destination }
                            );
                            assert_eq!(&actual_bus.ram[0x9000..0x9002], &[0x5a, 0x5a]);
                            if !faults {
                                assert_eq!(
                                    &actual_bus.ram[0x8ffe..0x9000],
                                    if case == 4 { &[0, 0] } else { &[0xef, 0xbe] }
                                );
                            }
                            if round == 3 && matches!(case, 0 | 4) {
                                assert_eq!(engine.stats.native_memory_writes, before + 1);
                            }
                            if case == 3 {
                                assert_eq!(engine.stats.native_memory_writes, before);
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_linked_code_obeys_changed_segment_permissions_and_stack_width() {
        for (segment, prefix) in [
            (SegmentReg::Ds, 0x3e),
            (SegmentReg::Es, 0x26),
            (SegmentReg::Ss, 0x36),
        ] {
            for change in 0..if segment == SegmentReg::Ss { 6 } else { 5 } {
                // Loading SS rejects null/read-only descriptors before its
                // hidden cache changes; data segments may hold either form.
                if segment == SegmentReg::Ss && matches!(change, 2 | 3) {
                    continue;
                }
                let flat = SegmentCache::flat(0x10, 0xc093);
                let changed = match change {
                    0 => SegmentCache::new(0x10, 0x200, u32::MAX, 0xc093),
                    1 => SegmentCache::new(0x10, 0, 0x5fff, 0xc093),
                    2 => SegmentCache::new(0x10, 0, u32::MAX, 0xc091),
                    3 => SegmentCache::new(0, 0, 0, 0),
                    4 => SegmentCache::new(0x10, 0, u32::MAX, 0xc097),
                    _ => SegmentCache::new(0x10, 0, u32::MAX, 0x8093),
                };
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    cpu.regs[2] = 0x6000;
                    cpu.regs[4] = 0x16004;
                    cpu.replace_eflags(3);
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    write_ram_u32(&mut bus, 0x5008, 0x6000 | 7);
                    let mut code = vec![0x40]; // INC commits before a segment/page fault.
                    if segment == SegmentReg::Ss {
                        code.extend([0x50, 0x5b]); // PUSH EAX; POP EBX, using SS.B.
                    } else {
                        code.extend([prefix, 0x89, 0x02, prefix, 0x8b, 0x1a]);
                    }
                    let next = 0x1000 + code.len() as u32 + 5;
                    code.push(0xe9);
                    code.extend(0x1020u32.wrapping_sub(next).to_le_bytes());
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    bus.ram[0x3020..0x3025].copy_from_slice(&[0xe9, 0xdb, 0x0f, 0, 0]);
                    bus.ram[0x6000..0x6006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
                    (cpu, bus)
                };
                let (mut expected, mut reference_bus) = make();
                let (mut actual, mut native_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                let mut steps = 0;
                for phase in 0..5 {
                    let bad_segment = phase == 1 || phase == 3;
                    let unmapped = phase == 3;
                    for (cpu, bus) in [
                        (&mut expected, &mut reference_bus),
                        (&mut actual, &mut native_bus),
                    ] {
                        cpu.eip = 0x1000;
                        cpu.regs[4] = 0x16004;
                        cpu.cr2 = 0xdead_beef;
                        cpu.set_seg_cache(segment, if bad_segment { changed } else { flat });
                        // Phase 1 changes only the segment: old direct and
                        // cross-page links must already stop using its proof.
                        // Phase 3 also challenges the execute-walk recheck.
                        if phase != 1 {
                            write_ram_u32(bus, 0x5018, if unmapped { 0 } else { 0x8000 | 7 });
                            write_ram_u32(bus, 0x5058, if unmapped { 0 } else { 0x9000 | 7 });
                            cpu.invalidate_translation_context();
                        }
                    }
                    let before_native = engine.stats.native_steps;
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut reference_bus, steps, 128);
                    let native = engine
                        .run_batch(&mut actual, &mut native_bus, steps, 128)
                        .unwrap();
                    assert_eq!(
                        native, reference,
                        "{segment:?} change={change} phase={phase}"
                    );
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(native_bus.ram, reference_bus.ram);
                    if bad_segment && (1..=4).contains(&change) {
                        assert_eq!(native.ran, 2, "{segment:?} change={change} phase={phase}");
                        assert!(matches!(
                            native.stop,
                            BatchStop::Step {
                                instruction_eip: 0x1001,
                                ..
                            }
                        ));
                        assert_eq!(actual.cr2, 0xdead_beef); // Segment fault before paging.
                    } else if unmapped {
                        assert_eq!(native.ran, 2);
                        assert_eq!(
                            actual.cr2,
                            if change == 5 {
                                0x6000
                            } else if segment == SegmentReg::Ss {
                                0x16200
                            } else {
                                0x6200
                            }
                        );
                    } else {
                        assert_eq!(native.stop, BatchStop::Budget);
                        if !bad_segment {
                            assert!(engine.stats.native_steps - before_native > 80);
                        }
                    }
                    steps += native.ran;
                }
                assert!(engine.stats.linked_edges > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_fs_gs_addresses_follow_base_changes_wrapping_and_operand_widths() {
        for (segment, prefix) in [(SegmentReg::Fs, 0x64), (SegmentReg::Gs, 0x65)] {
            for bits in [8, 16, 32] {
                let make = |jit| {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.regs[2] = 0x6100;
                    let mut bus = HostBus::new(0x10000);
                    for index in 0x6000..0x6500 {
                        bus.ram[index] = (index * 17) as u8;
                    }
                    let mut code = vec![prefix];
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([if bits == 8 { 0x8a } else { 0x8b }, 0x02]);
                    code.push(0x40); // inc eax
                    code.push(prefix);
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([if bits == 8 { 0x88 } else { 0x89 }, 0x42, 8]);
                    let delta = -(code.len() as i8 + 2);
                    code.extend([0xeb, delta as u8]);
                    bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                    let mut machine =
                        Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                    machine.set_jit_enabled(jit).unwrap();
                    machine
                };
                let mut reference = make(false);
                let mut native = make(true);
                for (base, limit) in [
                    (0x100, u32::MAX),
                    (0x203, 0x6110),
                    (0xffff_ff00, 0x610b),
                    (0, u32::MAX),
                ] {
                    for machine in [&mut reference, &mut native] {
                        machine
                            .cpu
                            .set_seg_cache(segment, SegmentCache::new(0x18, base, limit, 0xc092));
                    }
                    let prior_reads = native.jit.as_ref().unwrap().stats.native_memory_reads;
                    let prior_writes = native.jit.as_ref().unwrap().stats.native_memory_writes;
                    for budget in [1, 2, 3, 7, 31, 4000] {
                        reference.run_steps(budget).unwrap();
                        native.run_steps(budget).unwrap();
                        assert_eq!(
                            native.cpu.regs, reference.cpu.regs,
                            "{segment:?} {bits} {base:x}"
                        );
                        assert_eq!(native.cpu.eip, reference.cpu.eip);
                        assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
                        assert_eq!(
                            native.cpu.effective_eflags(),
                            reference.cpu.effective_eflags()
                        );
                        assert_eq!(native.ram(), reference.ram());
                    }
                    let stats = &native.jit.as_ref().unwrap().stats;
                    assert!(stats.native_memory_reads - prior_reads > 900);
                    assert!(stats.native_memory_writes - prior_writes > 900);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_based_segments_check_offset_limits_and_permissions_before_paging() {
        for (base, limit, access, offset, write, page_fault) in [
            (0x203, u32::MAX, 0xc092, u32::MAX - 1, false, false),
            (0x200, 0x20, 0xc092, 0x20, false, false),
            (0x200, u32::MAX, 0xc090, 0x6000, true, false),
            (0, u32::MAX, 0xc096, 0x6000, false, false),
            (0x200, u32::MAX, 0xc092, 0x7000, false, true),
            (0x200, u32::MAX, 0xc092, 0x6dfe, false, true),
            (0x200, u32::MAX, 0xc092, 0xffff_fe00, false, true),
        ] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs[0] = 0x7fff_ffff;
                cpu.regs[2] = offset;
                cpu.replace_eflags(3);
                cpu.set_seg_cache(SegmentReg::Fs, SegmentCache::new(0x18, base, limit, access));
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0x4000;
                cpu.cr2 = 0xdead_beef;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                bus.ram[0x3000..0x3007].copy_from_slice(&[
                    0x90,
                    0x40,
                    0x64,
                    if write { 0x89 } else { 0x8b },
                    0x02,
                    0x31,
                    0xc0,
                ]);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..3 {
                expected.eip = 0x1000;
                actual.eip = 0x1000;
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, round * 3, 50);
                let result = engine
                    .run_batch(&mut actual, &mut actual_bus, round * 3, 50)
                    .unwrap();
                assert_eq!(
                    result, reference,
                    "base={base:x} offset={offset:x} flags={access:x}"
                );
                assert_eq!(result.ran, 3);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                if page_fault {
                    assert_eq!(
                        actual.cr2,
                        if offset == 0x6dfe {
                            0x7000
                        } else {
                            base.wrapping_add(offset)
                        }
                    );
                } else {
                    assert_eq!(actual.cr2, 0xdead_beef);
                    assert!(matches!(
                        result.stop,
                        BatchStop::Step {
                            instruction_eip: 0x1002,
                            reason: BatchStepStop::Error(StepError::GeneralProtection {
                                code: 0,
                                ..
                            }),
                            ..
                        }
                    ));
                }
            }
            assert!(engine.stats.native_steps > 0);
            assert!(engine.stats.memory_exits > 0);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_shifts_match_widths_masked_counts_and_committed_flags() {
        for bits in [8, 16, 32] {
            for group in [2, 3, 4, 5, 7] {
                for (count, immediate) in [0, 1, 2, 7, 8, 9, 16, 17, 31, 32, 33, 255]
                    .into_iter()
                    .flat_map(|count| [(count, false), (count, true)])
                {
                    let make = |jit| {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.regs[0] = 0x8000_ff7f;
                        cpu.regs[1] = count;
                        cpu.regs[2] = 0x6000;
                        cpu.replace_eflags(0x8d7);
                        let mut bus = HostBus::new(0x10000);
                        write_ram_u32(&mut bus, 0x6000, 0x8000_ff7f);
                        let mut code = vec![];
                        if bits == 16 {
                            code.push(0x66);
                        }
                        let opcode = if immediate { 0xc0 } else { 0xd2 };
                        code.extend([opcode + u8::from(bits != 8), (group << 3) | 2]);
                        if immediate {
                            code.push(count as u8);
                        }
                        code.extend([0x40, 0x89, 0x02]); // vary EAX; reset [EDX]
                        let delta = -(code.len() as i8 + 2);
                        code.extend([0xeb, delta as u8]);
                        bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                        let mut machine =
                            Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                        machine.set_jit_enabled(jit).unwrap();
                        machine
                    };
                    let mut expected = make(false);
                    let mut native = make(true);
                    for budget in [1, 2, 3, 7, 1001] {
                        expected.run_steps(budget).unwrap();
                        native.run_steps(budget).unwrap();
                        assert_eq!(native.cpu.regs, expected.cpu.regs);
                        assert_eq!(
                            native.cpu.effective_eflags(),
                            expected.cpu.effective_eflags(),
                            "bits={bits} group={group} count={count}"
                        );
                        assert_eq!(native.cpu.eip, expected.cpu.eip);
                        assert_eq!(native.cpu.tsc(), expected.cpu.tsc());
                        assert_eq!(native.ram(), expected.ram());
                    }
                    assert!(native.jit.as_ref().unwrap().stats.native_memory_writes > 200);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_scalar_strings_direction_pushflags_and_sign_extension_match_interpreter() {
        for bits in [8, 16, 32] {
            let make = |jit| {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs[0] = 0x8000_8000;
                cpu.regs[4] = 0x9000;
                cpu.replace_eflags(0x28d7);
                cpu.set_seg_cache(
                    SegmentReg::Fs,
                    SegmentCache::new(0x18, 0x6000, 0xff, 0xc092),
                );
                let mut bus = HostBus::new(0x10000);
                for (i, byte) in bus.ram[0x6000..0x8000].iter_mut().enumerate() {
                    *byte = (i as u8).wrapping_mul(17).wrapping_add(0x7f);
                }
                let mut code = vec![0xbe, 0x40, 0, 0, 0, 0xbf, 0x40, 0x70, 0, 0];
                for direction in [0xfc, 0xfd] {
                    code.extend([0x40, direction]); // Live arithmetic flags must survive CLD/STD.
                    for opcode in [
                        0x99,
                        0x9c,
                        0x5b,
                        if bits == 8 { 0xac } else { 0xad },
                        if bits == 8 { 0xaa } else { 0xab },
                        if bits == 8 { 0xae } else { 0xaf },
                        0x9c,
                        0x5a,
                    ] {
                        if matches!(opcode, 0xac | 0xad) {
                            code.push(0x64);
                        }
                        if bits == 16 {
                            code.push(0x66);
                        }
                        code.push(opcode);
                    }
                }
                code.extend([0xeb, (-(code.len() as i8 + 2)) as u8]);
                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                let mut machine = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
                machine.set_jit_enabled(jit).unwrap();
                machine
            };
            let mut reference = make(false);
            let mut native = make(true);
            for budget in [1, 2, 3, 7, 11, 19, 10001] {
                reference.run_steps(budget).unwrap();
                native.run_steps(budget).unwrap();
                assert_eq!(
                    native.cpu.regs, reference.cpu.regs,
                    "bits={bits} budget={budget}"
                );
                assert_eq!(native.cpu.eip, reference.cpu.eip);
                assert_eq!(
                    native.cpu.effective_eflags(),
                    reference.cpu.effective_eflags()
                );
                assert_eq!(native.cpu.tsc(), reference.cpu.tsc());
                assert_eq!(native.ram(), reference.ram());
            }
            let stats = &native.jit.as_ref().unwrap().stats;
            assert!(stats.native_steps > 9000);
            assert!(stats.native_memory_reads > 1000);
            assert!(stats.native_memory_writes > 1000);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_popflags_and_cli_preserve_privilege_and_interrupt_boundaries() {
        for bits in [16, 32] {
            for cpl in [0, 1, 3] {
                for iopl in [0, 1, 3] {
                    for value in [2u32, 0x8d7, 0x24_7ed7, 0x1b_02d7, 0x102] {
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.set_seg_cache(
                                SegmentReg::Cs,
                                SegmentCache::new(8 | cpl, 0, u32::MAX, 0xc09b | (cpl << 5)),
                            );
                            cpu.set_seg_cache(
                                SegmentReg::Ss,
                                SegmentCache::new(16 | cpl, 0, u32::MAX, 0xc093 | (cpl << 5)),
                            );
                            cpu.regs[4] = 0x9000;
                            cpu.replace_eflags((iopl << 12) | 0x202);
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            let mut code = vec![0x90];
                            if bits == 16 {
                                code.push(0x66);
                            }
                            code.push(0x68);
                            code.extend_from_slice(&value.to_le_bytes()[..bits / 8]);
                            for opcode in [0x9d, 0x9c, 0x5b] {
                                if bits == 16 {
                                    code.push(0x66);
                                }
                                code.push(opcode);
                            }
                            code.extend([0xfa, 0xfb, 0x47]); // CLI/STI observe the new IOPL.
                            code.extend([0xeb, (-(code.len() as i8 + 2)) as u8]);
                            bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut expected_bus) = make();
                        let (mut actual, mut actual_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        let mut steps = 0;
                        for _ in 0..20 {
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                steps,
                                101,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut actual_bus, steps, 101)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "bits={bits} cpl={cpl} iopl={iopl} value={value:x}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                            steps += native.ran;
                            // Retry faulting CLI from the start to warm the same
                            // translation; no privilege exception is swallowed.
                            if matches!(native.stop, BatchStop::Step { .. }) {
                                expected.eip = 0x1000;
                                actual.eip = 0x1000;
                            }
                        }
                        if value & 0x100 == 0 {
                            assert!(engine.stats.native_steps > 0);
                            assert!(engine.stats.native_memory_reads > 0);
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_scalar_io_preserves_width_permissions_faults_and_attention() {
        for opcode in [0xe4u8, 0xe5, 0xe6, 0xe7, 0xec, 0xed, 0xee, 0xef] {
            for operand16 in [false, true] {
                // Ring 0, IOPL bypass, allowed/denied bitmap, bitmap #PF,
                // and an absent device port after a successful permission check.
                for mode in 0..6 {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        let cpl = if mode == 0 { 0 } else { 3 };
                        cpu.set_seg_cache(
                            SegmentReg::Cs,
                            SegmentCache::new(8 | cpl, 0, u32::MAX, 0xc09b | (cpl << 5)),
                        );
                        cpu.cr0 |= CR0_PG;
                        cpu.cr3 = 0x4000;
                        cpu.tr = 0x28;
                        cpu.tr_cache = SegmentCache::new(0x28, 0x6000, 0x3000, 0x8b);
                        let mut bus = HostBus::new(0x10000);
                        write_ram_u32(&mut bus, 0x4000, 0x5007);
                        write_ram_u32(&mut bus, 0x5004, 0x3007);
                        if mode != 4 {
                            write_ram_u32(&mut bus, 0x5018, 0x6003);
                        }
                        bus.ram[0x6066..0x6068].copy_from_slice(&0x80u16.to_le_bytes());
                        let port = if mode == 5 {
                            0x56
                        } else if opcode & 8 == 0 {
                            0x87
                        } else {
                            0xcf9
                        };
                        cpu.regs[2] = 0xabcd_0000 | port;
                        if mode == 3 {
                            // A wide IN/OUT must check the last port too, even
                            // when it crosses into the next permission byte.
                            let width = if opcode & 1 == 0 {
                                1
                            } else if operand16 {
                                2
                            } else {
                                4
                            };
                            let denied = port + width - 1;
                            bus.ram[0x6080 + (denied >> 3) as usize] |= 1 << (denied & 7);
                        }
                        let mut code = vec![0x43, 0x64]; // INC EBX; FS prefix.
                        if operand16 {
                            code.push(0x66);
                        }
                        code.push(opcode);
                        if opcode & 8 == 0 {
                            code.push(port as u8);
                        }
                        bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                        cpu.set_tsc(u64::MAX - 4);
                        (cpu, bus)
                    };
                    let (mut expected, mut expected_bus) = make();
                    let (mut actual, mut actual_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    let mut steps = 301;
                    for budget in [1, 2, 8, 1, 2, 8, 8, 8] {
                        for cpu in [&mut expected, &mut actual] {
                            cpu.eip = 0x1000;
                            cpu.regs[0] = 0xaabb_ccdd;
                            cpu.regs[3] = 0x7fff_ffff;
                            cpu.replace_eflags(0x203 | if mode == 1 { 0x3000 } else { 0 });
                        }
                        for part in 0..if budget == 1 { 2 } else { 1 } {
                            for bus in [&mut expected_bus, &mut actual_bus] {
                                bus.attention = false;
                                bus.next_attention_step = u64::MAX;
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                steps,
                                budget,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut actual_bus, steps, budget)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "opcode={opcode:x} operand16={operand16} mode={mode} budget={budget} part={part}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.segment_override, expected.segment_override);
                            assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual_bus.current_step, expected_bus.current_step);
                            assert_eq!(actual_bus.attention, expected_bus.attention);
                            assert_eq!(actual_bus.pci.addr(), expected_bus.pci.addr());
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                            steps += native.ran;
                        }
                        if mode == 3 || mode == 4 {
                            assert_eq!(actual.regs[0], 0xaabb_ccdd);
                        } else if mode == 5 {
                            let value = if opcode & 2 != 0 {
                                // OUT leaves EAX unchanged.
                                0xaabb_ccdd
                            } else if opcode & 1 == 0 {
                                // Byte IN ignores an operand-size prefix.
                                0xaabb_ccff
                            } else if operand16 {
                                0xaabb_ffff
                            } else {
                                u32::MAX
                            };
                            assert_eq!(
                                actual.regs[0], value,
                                "opcode={opcode:x} operand16={operand16} budget={budget} eip={:#x} cr2={:#x}",
                                actual.eip, actual.cr2
                            );
                        }
                    }
                    assert!(engine.stats.native_steps > 0);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_sti_preserves_privilege_shadow_and_following_faults() {
        for cpl in [0, 1, 3] {
            for iopl in [0, 1, 3] {
                for control in [0, crate::x86::cpu::CR4_PVI] {
                    for initial in [0x8d7, 0xad7, 0x10_08d7] {
                        for next in [
                            &[0x90][..],
                            &[0xfa],
                            &[0xfb],
                            &[0xf4],
                            &[0xa1, 0, 0x60, 0, 0],
                        ] {
                            let make = || {
                                let mut cpu = Cpu::new();
                                cpu.enter_protected_flat(0x1000, 0);
                                cpu.set_seg_cache(
                                    SegmentReg::Cs,
                                    SegmentCache::new(8 | cpl, 0, u32::MAX, 0xc09b | (cpl << 5)),
                                );
                                cpu.cr0 |= CR0_PG;
                                cpu.cr3 = 0x4000;
                                cpu.cr4 = control;
                                let mut bus = HostBus::new(0x10000);
                                bus.attention = false;
                                bus.next_attention_step = u64::MAX;
                                write_ram_u32(&mut bus, 0x4000, 0x5007);
                                write_ram_u32(&mut bus, 0x5004, 0x3007);
                                let mut code = vec![0x43, 0x66, 0xfb]; // INC EBX; STI.
                                code.extend_from_slice(next);
                                code.push(0x46);
                                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                                (cpu, bus)
                            };
                            let (mut expected, mut expected_bus) = make();
                            let (mut actual, mut actual_bus) = make();
                            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                            let mut steps = 0;
                            for round in 0..5 {
                                for cpu in [&mut expected, &mut actual] {
                                    cpu.eip = 0x1000;
                                    cpu.halted = false;
                                    cpu.replace_eflags(initial | (iopl << 12));
                                    cpu.restore_interrupt_shadow(false);
                                }
                                let interpreted = engine.stats.interpreted_steps;
                                // End at STI to retain its shadow, then check
                                // expiry with both exact and larger budgets.
                                let following_budget = if round % 2 != 0
                                    && u32::from(cpl) <= iopl
                                    && initial & 0x200 == 0
                                {
                                    101
                                } else {
                                    // Virtual STI sets VIF without a shadow;
                                    // cold JIT exits need not match a full batch.
                                    1
                                };
                                for (phase, budget) in [2, following_budget].into_iter().enumerate()
                                {
                                    let reference = Interpreter::run_batch(
                                        &mut expected,
                                        &mut expected_bus,
                                        steps,
                                        budget,
                                    );
                                    let native = engine
                                        .run_batch(&mut actual, &mut actual_bus, steps, budget)
                                        .unwrap();
                                    assert_eq!(
                                        native, reference,
                                        "cpl={cpl} iopl={iopl} cr4={control:x} flags={initial:x} next={next:x?} round={round} budget={budget}"
                                    );
                                    assert_eq!(actual.eip, expected.eip);
                                    assert_eq!(actual.regs, expected.regs);
                                    assert_eq!(
                                        actual.effective_eflags(),
                                        expected.effective_eflags()
                                    );
                                    assert_eq!(
                                        actual.interrupt_shadow(),
                                        expected.interrupt_shadow()
                                    );
                                    assert_eq!(
                                        actual.maskable_interrupts_enabled(),
                                        expected.maskable_interrupts_enabled()
                                    );
                                    assert_eq!(actual.tsc(), expected.tsc());
                                    assert_eq!(actual.cr2, expected.cr2);
                                    assert_eq!(actual_bus.ram, expected_bus.ram);
                                    steps += native.ran;
                                    if phase == 0 && u32::from(cpl) <= iopl {
                                        assert_eq!(actual.eip, 0x1003);
                                        assert_eq!(
                                            actual.interrupt_shadow(),
                                            initial & 0x200 == 0,
                                            "cpl={cpl} iopl={iopl} cr4={control:x} flags={initial:x} next={next:x?} round={round} result={native:?}"
                                        );
                                        assert_ne!(actual.effective_eflags() & 0x200, 0);
                                        if round >= 3 {
                                            assert_eq!(engine.stats.interpreted_steps, interpreted);
                                        }
                                    }
                                    if matches!(native.stop, BatchStop::Step { .. }) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_shadowed_native_entries_keep_one_instruction_and_partial_faults() {
        // Entries first compiled under a shadow contain one instruction.
        // Alternate ordinary reuse with shadowed visits, including a suffix
        // consuming the first instruction's flags.
        let cases: &[(&[u8], bool)] = &[
            (&[0xe8, 0xfb, 0x0f, 0, 0], true), // CALL 2000h.
            (&[0xff, 0xd0], true),             // CALL EAX.
            (&[0xc3], true),
            (&[0xc2, 8, 0], true),
            (&[0xeb, 0], true),
            (&[0xa1, 0, 0x60, 0, 0, 0xf4], true),
            (&[0xa1, 0, 0x60, 0, 0, 0x43, 0xf4], true),
            (&[0x83, 0xe8, 0xff, 0x83, 0xd2, 0, 0xf4], true), // SUB EAX,-1; ADC EDX,0; HLT.
            (&[0xa1, 0, 0x70, 0, 0, 0xf4], false),            // Unmapped load.
            (&[0xf3, 0xa4, 0xf4], false),                     // Copy three bytes, then fault.
            (&[0xf2, 0xa6, 0xf4], false),                     // Compare three bytes, then fault.
            (&[0x0f, 0xa0, 0xf4], true),                      // PUSH FS.
            (&[0x0f, 0xa1, 0xf4], true),                      // POP null FS.
        ];
        for &(code, completes_natively) in cases {
            for flags in [2, 0x202] {
                for budget in [1, 17] {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                        cpu.cr3 = 0x4000;
                        let mut bus = HostBus::new(0x20000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        for (addr, value) in [
                            (0x4000, 0x5007),
                            (0x5004, 0x3007),
                            (0x5008, 0x6007),
                            (0x5018, 0x8007),
                            (0x5020, 0xa007),
                            (0x5024, 0xb007),
                        ] {
                            write_ram_u32(&mut bus, addr, value);
                        }
                        bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(code);
                        bus.ram[0x6000] = 0xf4;
                        bus.ram[0x8000..0x8004].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
                        bus.ram[0x8ffd..0x9000].copy_from_slice(&[0x71, 0x82, 0x93]);
                        write_ram_u32(&mut bus, 0xb800, 0x2000);
                        (cpu, bus)
                    };
                    let (mut expected, mut expected_bus) = make();
                    let (mut actual, mut actual_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    let mut steps = 0;
                    for round in 0..10 {
                        let shadowed = !(6..8).contains(&round);
                        for cpu in [&mut expected, &mut actual] {
                            cpu.eip = 0x1000;
                            cpu.halted = false;
                            cpu.regs = [0x2000, 4, 0, 0, 0x9800, 0, 0x6ffd, 0x8ffd];
                            if code.starts_with(&[0x0f, 0xa1]) {
                                cpu.regs[4] = 0x97fc;
                            }
                            cpu.replace_eflags(flags);
                            cpu.restore_interrupt_shadow(shadowed);
                        }
                        for bus in [&mut expected_bus, &mut actual_bus] {
                            bus.next_attention_step = if round % 2 == 0 { u64::MAX } else { steps };
                        }
                        let before = engine.stats.native_steps;
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                        let result = engine
                            .run_batch(&mut actual, &mut actual_bus, steps, budget)
                            .unwrap();
                        assert_eq!(
                            result, reference,
                            "code={code:x?} flags={flags:x} budget={budget} round={round}"
                        );
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                        assert_eq!(
                            actual.was_interruptible_before_instruction(),
                            expected.was_interruptible_before_instruction()
                        );
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.cr2, expected.cr2);
                        assert_eq!(actual_bus.ram, expected_bus.ram);
                        if completes_natively && shadowed && round >= 3 {
                            assert_eq!(engine.stats.native_steps, before + 1);
                        }
                        steps += result.ran;
                    }
                    if matches!(code.get(1), Some(0xa4 | 0xa6)) {
                        assert!(engine.stats.partial_repeats > 0);
                        if code[1] == 0xa4 {
                            assert_eq!(&actual_bus.ram[0xaffd..0xb000], &[0x71, 0x82, 0x93]);
                        }
                        assert_eq!(actual.cr2, 0x7000);
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_shadowed_instructions_keep_budget_attention_and_fault_boundaries() {
        for code16 in [false, true] {
            for flags in [2, 0x202] {
                for code in [&[0x90][..], &[0xfa], &[0xfb], &[0xf4], &[0x0f, 0xff]] {
                    for budget in [1, 2, 31] {
                        for deadline in [0, 1, u64::MAX] {
                            let make = || {
                                let mut cpu = Cpu::new();
                                cpu.enter_protected_flat(0x1000, 0);
                                if code16 {
                                    cpu.set_seg_cache(
                                        SegmentReg::Cs,
                                        SegmentCache::new(8, 0, u32::MAX, 0x809b),
                                    );
                                }
                                cpu.replace_eflags(flags);
                                // MOV/POP SS can establish a shadow with IF=0.
                                cpu.restore_interrupt_shadow(true);
                                let mut bus = HostBus::new(0x10000);
                                bus.attention = false;
                                bus.next_attention_step = deadline;
                                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(code);
                                (cpu, bus)
                            };
                            let (mut expected, mut expected_bus) = make();
                            let (mut actual, mut actual_bus) = make();
                            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                            let reference =
                                Interpreter::run_batch(&mut expected, &mut expected_bus, 0, budget);
                            let result = engine
                                .run_batch(&mut actual, &mut actual_bus, 0, budget)
                                .unwrap();
                            assert_eq!(
                                result, reference,
                                "code16={code16} flags={flags:x} code={code:x?} budget={budget} deadline={deadline}"
                            );
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                            assert_eq!(actual.halted, expected.halted);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_popflags_fault_keeps_stack_flags_and_split_read_effects() {
        for bits in [16, 32] {
            for split in [false, true] {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    cpu.regs[4] = 0x6000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                    write_ram_u32(&mut bus, 0x501c, 0x9000 | 7);
                    write_ram_u32(&mut bus, 0x8000, 2);
                    let code: &[u8] = if bits == 16 {
                        &[0x90, 0x40, 0x66, 0x9d, 0xf4]
                    } else {
                        &[0x90, 0x40, 0x9d, 0xf4]
                    };
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(code);
                    (cpu, bus)
                };
                let (mut expected, mut reference_bus) = make();
                let (mut actual, mut native_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                // Establish native coverage before injecting the fault. Cold
                // admission is allowed to interpret the first few visits.
                let mut step = 0;
                for _ in 0..4 {
                    for cpu in [&mut actual, &mut expected] {
                        cpu.eip = 0x1000;
                        cpu.regs[4] = 0x6000;
                    }
                    assert_eq!(
                        engine
                            .run_batch(&mut actual, &mut native_bus, step, 3)
                            .unwrap(),
                        Interpreter::run_batch(&mut expected, &mut reference_bus, step, 3)
                    );
                    step += 3;
                }
                assert!(engine.stats.native_steps > 0);
                for (cpu, bus) in [
                    (&mut actual, &mut native_bus),
                    (&mut expected, &mut reference_bus),
                ] {
                    cpu.eip = 0x1000;
                    cpu.regs[0] = 0x7fff_ffff;
                    cpu.regs[4] = if split { 0x7000 - bits / 16 } else { 0x6000 };
                    cpu.replace_eflags(0x203);
                    write_ram_u32(bus, 0x5018, if split { 0x8000 | 7 } else { 0 });
                    write_ram_u32(bus, 0x501c, 0);
                    cpu.invalidate_translation_page(LinearAddr(0x6000));
                    cpu.invalidate_translation_page(LinearAddr(0x7000));
                }
                let reference = Interpreter::run_batch(&mut expected, &mut reference_bus, step, 10);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, step, 10)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.cr2, if split { 0x7000 } else { 0x6000 });
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
                assert_eq!(actual.regs[0], 0x8000_0000);
                assert_ne!(actual.effective_eflags() & (1 << 11), 0);
                assert_eq!(
                    actual.regs[4],
                    if split { 0x7000 - bits / 16 } else { 0x6000 }
                );
                assert!(engine.stats.native_steps > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_scalar_string_faults_preserve_indices_accumulator_and_flags() {
        for opcode in [0xa6, 0xac, 0xad, 0xaa, 0xab, 0xae, 0xaf] {
            for limit in [0x100, u32::MAX] {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.regs[0] = 0x8000_0080;
                    cpu.regs[6] = 0x6000;
                    cpu.regs[7] = 0x6000;
                    cpu.replace_eflags(0xc93);
                    for seg in [SegmentReg::Ds, SegmentReg::Es] {
                        cpu.set_seg_cache(seg, SegmentCache::new(0x18, 0, limit, 0xc092));
                    }
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    cpu.cr2 = 0xdead_beef;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    bus.ram[0x3000..0x3005].copy_from_slice(&[0x90, 0x41, opcode, 0x31, 0xc0]);
                    (cpu, bus)
                };
                let (mut expected, mut reference_bus) = make();
                let (mut actual, mut native_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..3 {
                    expected.eip = 0x1000;
                    actual.eip = 0x1000;
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 50);
                    let result = engine
                        .run_batch(&mut actual, &mut native_bus, round * 3, 50)
                        .unwrap();
                    assert_eq!(result, reference);
                    assert_eq!(result.ran, 3);
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(
                        actual.cr2,
                        if limit == u32::MAX {
                            0x6000
                        } else {
                            0xdead_beef
                        }
                    );
                    assert_eq!(native_bus.ram, reference_bus.ram);
                }
                assert!(engine.stats.memory_exits > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_repeated_strings_preserve_count_zero_conditions_and_tick_chunks() {
        for bits in [8u8, 16, 32] {
            for opcode in [0xa6, 0xaa, 0xac, 0xae] {
                if opcode == 0xa6 && bits != 8 {
                    continue;
                }
                for prefix in [0xf2, 0xf3] {
                    for count in [0, 1, 17, 1024, 4096, 4097] {
                        for backward in [false, true] {
                            let make = || {
                                let mut cpu = Cpu::new();
                                cpu.enter_protected_flat(0x1000, 0);
                                cpu.replace_eflags(0x8d7 | if backward { 0x400 } else { 0 });
                                if count == 0 {
                                    for seg in [SegmentReg::Ds, SegmentReg::Es] {
                                        cpu.set_seg_cache(seg, SegmentCache::new(0, 0, 0, 0));
                                    }
                                }
                                let mut bus = HostBus::new(0x40000);
                                bus.attention = false;
                                bus.next_attention_step = u64::MAX;
                                bus.ram[0x10000..].fill(0x7f);
                                let mut code = vec![0x90];
                                if bits == 16 {
                                    code.push(0x66);
                                }
                                code.extend([prefix, opcode + u8::from(bits != 8), 0xf4]);
                                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                                (cpu, bus)
                            };
                            let (mut expected, mut reference_bus) = make();
                            let (mut actual, mut native_bus) = make();
                            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                            for round in 0..3 {
                                for (cpu, bus) in [
                                    (&mut expected, &mut reference_bus),
                                    (&mut actual, &mut native_bus),
                                ] {
                                    cpu.eip = 0x1000;
                                    cpu.regs[0] = 0x7f7f_7f7f;
                                    cpu.regs[1] = count;
                                    cpu.regs[6] = 0x20000;
                                    cpu.regs[7] = 0x30000;
                                    // Bulk STOS does not fill the scalar TLB.
                                    if count != 0 && round == 2 {
                                        mmu::read_linear_u8(
                                            cpu,
                                            bus,
                                            LinearAddr(0x20000),
                                            AccessKind::Read,
                                        )
                                        .unwrap();
                                        mmu::write_linear_u8(cpu, bus, LinearAddr(0x30000), 0x7f)
                                            .unwrap();
                                    }
                                }
                                let reference = Interpreter::run_batch(
                                    &mut expected,
                                    &mut reference_bus,
                                    round * 2,
                                    2,
                                );
                                let native = engine
                                    .run_batch(&mut actual, &mut native_bus, round * 2, 2)
                                    .unwrap();
                                assert_eq!(
                                    native, reference,
                                    "bits={bits} op={opcode:x} prefix={prefix:x} count={count} backward={backward}"
                                );
                                assert_eq!(actual.regs, expected.regs);
                                assert_eq!(actual.eip, expected.eip);
                                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                                assert_eq!(actual.tsc(), expected.tsc());
                                assert_eq!(native_bus.ram, reference_bus.ram);
                            }
                            if count != 0 && count * u32::from(bits / 8) <= 4096 && !backward {
                                assert!(
                                    engine.stats.native_memory_reads
                                        + engine.stats.native_memory_writes
                                        > 0
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_division_preserves_page_fault_priority_and_dividend_on_error() {
        for case in 0..7 {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0x4000;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                if case != 2 {
                    write_ram_u32(&mut bus, 0x5018, 0x8000 | 5);
                }
                let address: u32 = if case == 3 { 0x6ffe } else { 0x6000 };
                let operand = match case {
                    1 => 0,
                    5 => u32::MAX,
                    6 => (-3i32) as u32,
                    _ => 3,
                };
                write_ram_u32(&mut bus, 0x8000, operand);
                let mut code = vec![0x47, 0xf7, if case >= 5 { 0x3d } else { 0x35 }];
                code.extend(address.to_le_bytes());
                code.extend([0x31, 0xdb, 0xf4]);
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut reference_bus) = make();
            let (mut actual, mut native_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..3 {
                for cpu in [&mut expected, &mut actual] {
                    cpu.eip = 0x1000;
                    cpu.regs[0] = match case {
                        5 => 0,
                        6 => (-7i32) as u32,
                        _ => 17,
                    };
                    cpu.regs[2] = match case {
                        4 => 3,
                        5 => 0x80000000,
                        6 => u32::MAX,
                        _ => 0,
                    };
                    cpu.regs[7] = 0x7fffffff;
                    cpu.replace_eflags(0x8d7);
                }
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 3);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, round * 3, 3)
                    .unwrap();
                assert_eq!(native, reference, "case={case}");
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(native_bus.ram, reference_bus.ram);
            }
            assert!(engine.stats.native_invocations > 0, "case={case}");
            if (1..=5).contains(&case) {
                assert!(
                    engine.stats.division_exits > 0,
                    "case={case}: {:?}",
                    engine.stats
                );
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_repeated_strings_resume_page_misses_without_extending_attempt_budget() {
        for opcode in [0xa4, 0xa6, 0xaa, 0xac, 0xae] {
            for width in [1u32, 2, 4] {
                if opcode == 0xa6 && width != 1 {
                    continue;
                }
                for warm_all in [false, true] {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        let mut bus = HostBus::new(0x40000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        bus.ram[0x10000..].fill(0x7f);
                        let mut code = vec![0x90];
                        if width == 2 {
                            code.push(0x66);
                        }
                        code.extend([0xf3, opcode + u8::from(width != 1), 0xf4]);
                        bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                        (cpu, bus)
                    };
                    let (mut expected, mut reference_bus) = make();
                    let (mut actual, mut native_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    for round in 0..4 {
                        for (cpu, bus) in [
                            (&mut expected, &mut reference_bus),
                            (&mut actual, &mut native_bus),
                        ] {
                            cpu.eip = 0x1000;
                            cpu.regs[0] = 0x7f7f_7f7f;
                            cpu.regs[1] = 4100;
                            // Word/dword elements eventually straddle a page.
                            cpu.regs[6] = 0x10ff9;
                            cpu.regs[7] = 0x20ff9;
                            cpu.invalidate_translation_context();
                            for page in 0..if warm_all { 6 } else { 1 } {
                                mmu::read_linear_u8(
                                    cpu,
                                    bus,
                                    LinearAddr(0x10000 + page * 4096),
                                    AccessKind::Read,
                                )
                                .unwrap();
                                mmu::write_linear_u8(
                                    cpu,
                                    bus,
                                    LinearAddr(0x20000 + page * 4096),
                                    0x7f,
                                )
                                .unwrap();
                            }
                        }
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut reference_bus, round * 2, 2);
                        let native = engine
                            .run_batch(&mut actual, &mut native_bus, round * 2, 2)
                            .unwrap();
                        assert_eq!(
                            native, reference,
                            "opcode={opcode:x} width={width} warm_all={warm_all}"
                        );
                        assert_eq!(actual.regs[1], 4);
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(native_bus.ram, reference_bus.ram);
                    }
                    // Byte elements can refill a cold unpaged translation in
                    // native code. Split word/dword elements still exercise
                    // the partial-REP continuation and its original budget.
                    if width != 1 {
                        assert!(
                            engine.stats.partial_repeats > 0,
                            "opcode={opcode:x} width={width}: {:?}",
                            engine.stats
                        );
                    }
                    assert!(
                        engine.stats.native_memory_reads + engine.stats.native_memory_writes > 0
                    );
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cmpsb_preserves_sub_flags_source_overrides_and_early_stops() {
        for prefix in [None, Some(0xf2), Some(0xf3)] {
            for (lhs, rhs) in [(0x80u8, 1u8), (0, 0xff), (0x10, 1), (0x55, 0x55)] {
                for (segment, base) in [
                    (SegmentReg::Ds, 0),
                    (SegmentReg::Fs, 0x5000),
                    (SegmentReg::Gs, 0),
                ] {
                    for backward in [false, true] {
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.set_seg_cache(segment, SegmentCache::flat(0x18, 0xc093));
                            let mut cache = cpu.seg(segment);
                            cache.base = base;
                            cpu.set_seg_cache(segment, cache);
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            let mut code = vec![0x90];
                            if segment != SegmentReg::Ds {
                                code.push(if segment == SegmentReg::Fs {
                                    0x64
                                } else {
                                    0x65
                                });
                            }
                            code.extend(prefix);
                            code.extend([0xa6, 0xf4]);
                            bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                            bus.ram[0x5f00..0x6100].fill(lhs);
                            bus.ram[0x7f00..0x8100].fill(rhs);
                            (cpu, bus)
                        };
                        let (mut expected, mut expected_bus) = make();
                        let (mut actual, mut actual_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        for round in 0..4 {
                            for cpu in [&mut expected, &mut actual] {
                                cpu.eip = 0x1000;
                                cpu.regs[1] = 17;
                                cpu.regs[6] = 0x6000 - base;
                                cpu.regs[7] = 0x8000;
                                cpu.replace_eflags(0x28d7 | if backward { 0x400 } else { 0 });
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                round * 2,
                                2,
                            );
                            let before = engine.stats.native_memory_reads;
                            let result = engine
                                .run_batch(&mut actual, &mut actual_bus, round * 2, 2)
                                .unwrap();
                            assert_eq!(result, reference);
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.tsc(), expected.tsc());
                            let done = if prefix == Some(if lhs == rhs { 0xf3 } else { 0xf2 }) {
                                17
                            } else {
                                1
                            };
                            assert_eq!(
                                actual.regs[1],
                                if prefix.is_some() { 17 - done } else { 17 }
                            );
                            let delta = if backward {
                                0u32.wrapping_sub(done)
                            } else {
                                done
                            };
                            assert_eq!(actual.regs[6], (0x6000 - base).wrapping_add(delta));
                            assert_eq!(actual.regs[7], 0x8000u32.wrapping_add(delta));
                            let difference = lhs.wrapping_sub(rhs);
                            let flags = u32::from(lhs < rhs)
                                | (u32::from(difference.count_ones() % 2 == 0) << 2)
                                | (u32::from((lhs ^ rhs ^ difference) & 0x10 != 0) << 4)
                                | (u32::from(difference == 0) << 6)
                                | (u32::from(difference & 0x80 != 0) << 7)
                                | (u32::from((lhs ^ rhs) & (lhs ^ difference) & 0x80 != 0) << 11);
                            assert_eq!(actual.effective_eflags() & 0x8d5, flags);
                            if round == 3 && (prefix.is_none() || (!backward && base == 0)) {
                                assert_eq!(
                                    engine.stats.native_memory_reads - before,
                                    2 * u64::from(done)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cmpsb_page_fault_order_privilege_and_partial_budget() {
        // Unequal page offsets exercise each bound independently. The long
        // case enters a present page with A clear after native progress; its
        // shared tail must still stop at 4096 total elements in this one tick.
        for prefix in [0xf2, 0xf3] {
            for user in [false, true] {
                for case in 0..7 {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        if user {
                            cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(11, 0xc0fb));
                            for seg in [SegmentReg::Ds, SegmentReg::Es, SegmentReg::Ss] {
                                cpu.set_seg_cache(seg, SegmentCache::flat(19, 0xc0f3));
                            }
                        }
                        cpu.set_seg_cache(SegmentReg::Fs, cpu.seg(SegmentReg::Ds));
                        cpu.cr0 |= CR0_PG;
                        cpu.cr3 = 0x4000;
                        let mut bus = HostBus::new(0x20000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        // Source override must also survive the shared tail.
                        bus.ram[0x3000..0x3005].copy_from_slice(&[0x90, 0x64, prefix, 0xa6, 0xf4]);
                        (cpu, bus)
                    };
                    let (mut expected, mut expected_bus) = make();
                    let (mut actual, mut actual_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    for round in 0..4 {
                        for (cpu, bus) in [
                            (&mut expected, &mut expected_bus),
                            (&mut actual, &mut actual_bus),
                        ] {
                            cpu.eip = 0x1000;
                            cpu.cr2 = 0xdead_beef;
                            cpu.regs[1] = if case == 6 { 4100 } else { 17 };
                            cpu.regs[6] = if case == 4 { 0x7000 } else { 0x6ffd };
                            cpu.regs[7] = if case == 1 { 0xafff } else { 0xaffb };
                            cpu.replace_eflags(0x28d7);
                            cpu.invalidate_translation_context();
                            write_ram_u32(bus, 0x4000, 0x5000 | 0x27);
                            write_ram_u32(bus, 0x5004, 0x3000 | 0x27);
                            write_ram_u32(bus, 0x5018, 0x8000 | 0x27);
                            write_ram_u32(bus, 0x501c, if case == 6 { 0x9000 | 7 } else { 0 });
                            write_ram_u32(
                                bus,
                                0x5028,
                                0xc000 | if case == 3 { 0x23 } else { 0x27 },
                            );
                            write_ram_u32(bus, 0x502c, if case == 6 { 0xd000 | 7 } else { 0 });
                            write_ram_u32(bus, 0x5030, if case == 6 { 0xe000 | 7 } else { 0 });
                            bus.ram[0x8000..0xa000].fill(0x80);
                            bus.ram[0xc000..0xf000].fill(if prefix == 0xf3 { 0x80 } else { 1 });
                            if case == 2 || case == 5 {
                                // Stop on the final source byte of this page.
                                bus.ram[0xcffd] = if prefix == 0xf3 { 1 } else { 0x80 };
                            }
                            if case == 4 {
                                cpu.set_seg_cache(SegmentReg::Es, SegmentCache::new(0, 0, 0, 0));
                            }
                            if case == 5 {
                                // The next source is present but supervisor-only.
                                write_ram_u32(bus, 0x501c, 0x9000 | 3);
                            }
                        }
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut expected_bus, round * 2, 2);
                        let before = engine.stats.native_memory_reads;
                        let partial_before = engine.stats.partial_repeats;
                        let result = engine
                            .run_batch(&mut actual, &mut actual_bus, round * 2, 2)
                            .unwrap();
                        assert_eq!(
                            result, reference,
                            "prefix={prefix:x} user={user} case={case} round={round}"
                        );
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.cr2, expected.cr2);
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual_bus.ram, expected_bus.ram);
                        let done = match case {
                            1 => 1,
                            3 if user => 0,
                            4 => 0,
                            6 => 4096,
                            _ => 3,
                        };
                        assert_eq!(actual.regs[1], if case == 6 { 4 } else { 17 - done });
                        assert_eq!(
                            actual.cr2,
                            match case {
                                1 => 0xb000,
                                3 if user => 0xaffb,
                                2 | 5 | 6 => 0xdead_beef,
                                _ => 0x7000,
                            }
                        );
                        if round == 3 && done != 0 {
                            assert!(engine.stats.native_memory_reads > before);
                            if !matches!(case, 2 | 5) {
                                assert_eq!(engine.stats.partial_repeats, partial_before + 1);
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_repeated_string_page_faults_and_early_scans_match_committed_progress() {
        let scans = [1u32, 2, 4].into_iter().flat_map(|width| {
            [0xf2, 0xf3]
                .into_iter()
                .map(move |prefix| (if width == 1 { 0xae } else { 0xaf }, width, prefix))
        });
        for (opcode, width, prefix) in [(0xab, 4, 0xf3), (0xad, 4, 0xf3)].into_iter().chain(scans) {
            for early_stop in [false, true] {
                if early_stop && opcode & 0xfe != 0xae {
                    continue;
                }
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    cpu.cr2 = 0xdead_beef;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                    bus.ram[0x8000..0x9000].fill(0x7f);
                    let mut code = vec![0x90, 0x40];
                    if width == 2 {
                        code.push(0x66);
                    }
                    code.extend_from_slice(&[prefix, opcode, 0x31, 0xc0]);
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut reference_bus) = make();
                let (mut actual, mut native_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..3 {
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs[0] = if (prefix == 0xf3) != early_stop {
                            0x7f7f_7f7e
                        } else {
                            0
                        };
                        cpu.regs[1] = 4;
                        cpu.regs[6] = 0x7000 - 2 * width;
                        cpu.regs[7] = 0x7000 - 2 * width;
                    }
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 3);
                    let actual_batch = engine
                        .run_batch(&mut actual, &mut native_bus, round * 3, 3)
                        .unwrap();
                    assert_eq!(actual_batch, reference);
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.regs[1], if early_stop { 3 } else { 2 });
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, if early_stop { 0xdead_beef } else { 0x7000 });
                    assert_eq!(native_bus.ram, reference_bus.ram);
                }
                if early_stop {
                    assert!(engine.stats.native_memory_reads > 0);
                } else {
                    assert!(engine.stats.memory_exits > 0);
                }
                assert!(engine.stats.native_steps > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_movs_preserves_width_overlap_count_zero_and_repeat_tick_boundaries() {
        for bits in [8u8, 16, 32] {
            for prefix in [None, Some(0xf2), Some(0xf3)] {
                for count in [0, 1, 2, 17, 512, 4096, 4097] {
                    for (backward, overlap) in [(false, false), (false, true), (true, true)] {
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.replace_eflags(0x28d7 | if backward { 0x400 } else { 0 });
                            if prefix.is_some() && count == 0 {
                                // Zero count must skip even unusable segments.
                                cpu.set_seg_cache(SegmentReg::Es, SegmentCache::new(0, 0, 0, 0));
                            }
                            let mut bus = HostBus::new(0x30000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            for (i, byte) in bus.ram[0xc000..].iter_mut().enumerate() {
                                *byte = (i as u8).wrapping_mul(17).wrapping_add(5);
                            }
                            let mut code = vec![0x90];
                            if bits == 16 {
                                code.push(0x66);
                            }
                            code.extend(prefix);
                            code.push(if bits == 8 { 0xa4 } else { 0xa5 });
                            code.push(0xf4);
                            bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut reference_bus) = make();
                        let (mut actual, mut native_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        for round in 0..3 {
                            for cpu in [&mut expected, &mut actual] {
                                cpu.eip = 0x1000;
                                cpu.regs[1] = count;
                                cpu.regs[6] = 0x11040;
                                cpu.regs[7] = if overlap {
                                    0x11040 + u32::from(bits / 8)
                                } else {
                                    0x21040
                                };
                            }
                            if round == 2 && (prefix.is_none() || count != 0) {
                                // Bulk interpreter copies deliberately bypass
                                // the software TLB. Check that cold miss above,
                                // then warm it through ordinary data accesses
                                // so the final round exercises native copying.
                                for (cpu, bus) in [
                                    (&mut expected, &mut reference_bus),
                                    (&mut actual, &mut native_bus),
                                ] {
                                    let src = LinearAddr(cpu.regs[6]);
                                    let dst = LinearAddr(cpu.regs[7]);
                                    mmu::read_linear_u8(cpu, bus, src, AccessKind::Read).unwrap();
                                    let byte = mmu::read_linear_u8(cpu, bus, dst, AccessKind::Read)
                                        .unwrap();
                                    mmu::write_linear_u8(cpu, bus, dst, byte).unwrap();
                                }
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut reference_bus,
                                round * 2,
                                2,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut native_bus, round * 2, 2)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "bits={bits} prefix={prefix:?} count={count} backward={backward} overlap={overlap}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(native_bus.ram, reference_bus.ram);
                        }
                        if prefix.is_some() && count == 0 {
                            assert_eq!(engine.stats.native_memory_reads, 0);
                            assert_eq!(engine.stats.native_memory_writes, 0);
                        } else if !backward && (prefix.is_none() || count <= 512) {
                            assert!(
                                engine.stats.native_memory_reads > 0,
                                "bits={bits} prefix={prefix:?} count={count} overlap={overlap}: {:?}",
                                engine.stats
                            );
                            assert!(engine.stats.native_memory_writes > 0);
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_rep_movs_faults_keep_committed_elements_flags_and_one_attempted_tick() {
        for case in 0..3 {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                cpu.regs[0] = 0x7fff_ffff;
                cpu.replace_eflags(3);
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                write_ram_u32(&mut bus, 0x5028, 0xc000 | if case == 1 { 5 } else { 7 });
                for (i, byte) in bus.ram[0x8000..0x9000].iter_mut().enumerate() {
                    *byte = (i as u8).wrapping_mul(17).wrapping_add(5);
                }
                bus.ram[0xc000..0xd000].fill(0x77);
                bus.ram[0x3000..0x3006].copy_from_slice(&[
                    0x90, 0x40, // NOP; INC EAX (flags must survive the fault)
                    0xf3, 0xa5, // REP MOVSD
                    0x31, 0xc0, // future flag writer
                ]);
                (cpu, bus)
            };
            let (mut expected, mut reference_bus) = make();
            let (mut actual, mut native_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..3 {
                for cpu in [&mut expected, &mut actual] {
                    cpu.eip = 0x1000;
                    cpu.regs[1] = 4;
                    cpu.regs[6] = if case == 0 { 0x6ff8 } else { 0x6000 };
                    cpu.regs[7] = if case == 2 { 0xaff8 } else { 0xa000 };
                }
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 50);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, round * 3, 50)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(native.ran, 3);
                assert!(matches!(
                    native.stop,
                    BatchStop::Step {
                        instruction_eip: 0x1002,
                        ..
                    }
                ));
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.regs[1], if case == 1 { 4 } else { 2 });
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.cr2, [0x7000, 0xa000, 0xb000][case]);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(native_bus.ram, reference_bus.ram);
            }
            assert!(engine.stats.native_steps > 0);
            assert!(engine.stats.memory_exits > 0);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_zero_memory_shift_reads_a_readonly_page_without_changing_flags() {
        for (bits, group, count, immediate) in [
            (32, 5, 0, false),
            (32, 5, 32, false),
            (32, 5, 0, true),
            (32, 5, 32, true),
            (8, 2, 9, false),
            (8, 3, 9, true),
            (16, 2, 17, true),
            (16, 3, 17, false),
        ] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                cpu.regs[0] = 0x7fff_ffff;
                cpu.regs[1] = count;
                cpu.replace_eflags(3);
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                write_ram_u32(&mut bus, 0x5018, 0x8000 | 5);
                write_ram_u32(&mut bus, 0x8000, 0x8000_0001);
                let mut code = vec![0x90, 0x40]; // NOP; INC EAX
                if bits == 16 {
                    code.push(0x66);
                }
                code.extend([
                    (if immediate { 0xc0 } else { 0xd2 }) + u8::from(bits != 8),
                    group << 3 | 5,
                    0,
                    0x60,
                    0,
                    0,
                ]); // shift/rotate [6000h],imm8/CL
                if immediate {
                    code.push(count as u8);
                }
                code.push(0xf4);
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut reference_bus) = make();
            let (mut actual, mut native_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..3 {
                expected.eip = 0x1000;
                actual.eip = 0x1000;
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 3);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, round * 3, 3)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(native.stop, BatchStop::Budget);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
            }
            assert!(engine.stats.native_steps > 0);
            assert!(engine.stats.memory_exits > 0);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_leave_fault_preserves_stack_frame_flags_and_prior_progress() {
        for operand16 in [false, true] {
            for split in [false, true] {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    cpu.regs[0] = 0x7fff_ffff;
                    cpu.regs[4] = 0x9000;
                    cpu.regs[5] = if split { 0x6fff } else { 0x6000 };
                    cpu.replace_eflags(3);
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    if split {
                        write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                    }
                    let mut code = vec![0x90, 0x40];
                    if operand16 {
                        code.push(0x66);
                    }
                    code.extend([0xc9, 0x31, 0xc0]);
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut reference_bus) = make();
                let (mut actual, mut native_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..3 {
                    expected.eip = 0x1000;
                    actual.eip = 0x1000;
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut reference_bus, round * 3, 50);
                    let native = engine
                        .run_batch(&mut actual, &mut native_bus, round * 3, 50)
                        .unwrap();
                    assert_eq!(native, reference);
                    assert_eq!(native.ran, 3);
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.regs[4], 0x9000);
                    assert_eq!(actual.regs[5], if split { 0x6fff } else { 0x6000 });
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.cr2, if split { 0x7000 } else { 0x6000 });
                    assert_eq!(native_bus.ram, reference_bus.ram);
                }
                assert!(engine.stats.native_steps > 0);
                assert!(engine.stats.memory_exits > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_fault_exit_preserves_prior_flags_ticks_and_split_read_ad_effects() {
        for split in [false, true] {
            for (opcode, immediate, bytes) in [
                (&[0x8b, 0x15][..], &[][..], 4),         // mov edx,[abs]
                (&[0x0f, 0xba, 0x25][..], &[31][..], 4), // bt [abs],31
                (&[0x0f, 0xaf, 0x15][..], &[][..], 4),   // imul edx,[abs]
                (&[0x69, 0x15][..], &[0xff, 0xff, 0xff, 0x7f][..], 4),
                (&[0x66, 0x6b, 0x15][..], &[0xfe][..], 2),
                (&[0x83, 0x3d][..], &[0][..], 4), // cmp dword [abs],0
                (&[0xf6, 0x25][..], &[][..], 1),  // mul byte [abs]
                (&[0xf6, 0x2d][..], &[][..], 1),  // imul byte [abs]
                (&[0x66, 0xf7, 0x25][..], &[][..], 2),
                (&[0x66, 0xf7, 0x2d][..], &[][..], 2),
                (&[0xf7, 0x25][..], &[][..], 4),
                (&[0xf7, 0x2d][..], &[][..], 4),
                (&[0x0f, 0x44, 0x15][..], &[][..], 4), // false CMOVZ still faults
                (&[0x66, 0x0f, 0x44, 0x15][..], &[][..], 2),
                (&[0x0f, 0x45, 0x15][..], &[][..], 4), // true CMOVNZ also cannot commit
            ] {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.regs[0] = 0x7fff_ffff;
                    cpu.replace_eflags(3); // INC must preserve CF
                    cpu.cr0 |= CR0_PG;
                    cpu.cr3 = 0x4000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                    write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                    if split {
                        write_ram_u32(&mut bus, 0x5018, 0x8000 | 7);
                    }
                    let address: u32 = if split { 0x7000 - bytes / 2 } else { 0x6000 };
                    let mut code = vec![0x90, 0x40]; // nop; inc eax
                    code.extend_from_slice(opcode);
                    code.extend(address.to_le_bytes());
                    code.extend_from_slice(immediate);
                    code.extend([0x31, 0xc0]); // must not erase INC's flags before the fault
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..3 {
                    expected.eip = 0x1000;
                    actual.eip = 0x1000;
                    let reference = Interpreter::run_batch(
                        &mut expected,
                        &mut expected_bus,
                        10 + round * 3,
                        50,
                    );
                    let result = engine
                        .run_batch(&mut actual, &mut actual_bus, 10 + round * 3, 50)
                        .unwrap();
                    assert_eq!(result, reference);
                    assert_eq!(result.ran, 3);
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, if split { 0x7000 } else { 0x6000 });
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                }
                assert!(engine.stats.native_steps > 0);
                assert!(engine.stats.memory_exits > 0);
            }
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_read_fill_grants_write_only_after_dirty_permission_and_code_checks() {
        use crate::x86::mmu::AccessPriv;
        for user in [false, true] {
            for wp in [false, true] {
                for pde_write in [false, true] {
                    for pte_write in [false, true] {
                        for dirty in [false, true] {
                            for watched in [false, true] {
                                let mut cpu = Cpu::new();
                                cpu.enter_protected_flat(0x1000, 0);
                                cpu.cr0 |= CR0_PG | if wp { crate::x86::cpu::CR0_WP } else { 0 };
                                cpu.cr3 = 0x4000;
                                let mut bus = HostBus::new(0x10000);
                                let mut pages =
                                    crate::x86::jit::validity::CodePages::new(bus.ram.len());
                                if watched {
                                    pages.watch(9);
                                }
                                bus.jit_code_pages = Some(pages);
                                let pde = 0x5025 | u32::from(pde_write) * 2;
                                let pte = 0x9025 | u32::from(pte_write) * 2 | u32::from(dirty) * 64;
                                write_ram_u32(&mut bus, 0x4000, pde);
                                write_ram_u32(&mut bus, 0x5018, pte);
                                write_ram_u32(&mut bus, 0x9000, 42);
                                let priv_ = if user {
                                    AccessPriv::User
                                } else {
                                    AccessPriv::Supervisor
                                };
                                assert_eq!(
                                    crate::x86::mmu::read_linear_u32_at_priv(
                                        &mut cpu,
                                        &mut bus,
                                        LinearAddr(0x6000),
                                        crate::x86::AccessKind::Read,
                                        priv_
                                    )
                                    .unwrap(),
                                    42
                                );
                                let permitted = (pde_write && pte_write) || (!user && !wp);
                                assert_eq!(
                                    cpu.cached_write_translation(LinearAddr(0x6000), user)
                                        .is_some(),
                                    dirty && permitted && !watched
                                );
                                assert_eq!(
                                    bus.read_phys_u32(PhysAddr(0x5018)).unwrap(),
                                    pte,
                                    "a read must not set D"
                                );
                                let write = crate::x86::mmu::write_linear_u32_at_priv(
                                    &mut cpu,
                                    &mut bus,
                                    LinearAddr(0x6000),
                                    77,
                                    priv_,
                                );
                                assert_eq!(write.is_ok(), permitted);
                                assert_eq!(
                                    bus.read_phys_u32(PhysAddr(0x9000)).unwrap(),
                                    if permitted { 77 } else { 42 }
                                );
                                assert_eq!(
                                    bus.read_phys_u32(PhysAddr(0x5018)).unwrap(),
                                    if permitted { pte | 64 } else { pte }
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_unpaged_refill_respects_backing_code_watches_and_a20() {
        for write in [false, true] {
            for a20 in [false, true] {
                for watched in [false, true] {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    // PAE has no effect while CR0.PG is clear.
                    cpu.cr4 |= crate::x86::cpu::CR4_PAE;
                    let mut bus = HostBus::new_firmware(0x200000).unwrap();
                    if !a20 {
                        bus.port92 &= !2;
                    }
                    let mut pages = crate::x86::jit::validity::CodePages::new(bus.ram.len());
                    if watched {
                        pages.watch(6);
                    }
                    bus.jit_code_pages = Some(pages);
                    for address in [0x6004, 0xa0000, 0xc0000, 0x200000] {
                        let token = mmu::try_resident_data_translation(
                            &mut cpu,
                            &mut bus,
                            LinearAddr(address),
                            write,
                        );
                        assert_eq!(
                            token.is_some(),
                            address == 0x6004 && a20 && (!write || !watched)
                        );
                        if let Some(token) = token {
                            assert_eq!(bus.cached_backing_phys(token), PhysAddr(0x6004));
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_ram_refill_checks_permissions_ad_decode_and_code_without_side_effects() {
        for user in [false, true] {
            for wp in [false, true] {
                for write in [false, true] {
                    for case in 0..14 {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.cr0 |= CR0_PG;
                        if wp {
                            cpu.cr0 |= crate::x86::cpu::CR0_WP;
                        }
                        if user {
                            let mut cs = cpu.seg(SegmentReg::Cs);
                            cs.selector |= 3;
                            cpu.set_seg_cache(SegmentReg::Cs, cs);
                        }
                        cpu.cr3 = 0x4000;
                        cpu.cr2 = 0x1234_5678;
                        let mut bus = HostBus::new(0x10000);
                        let mut pages = crate::x86::jit::validity::CodePages::new(bus.ram.len());
                        let mut pde = 0x5027;
                        let mut pte = 0xc067;
                        match case {
                            1 => pde &= !1,
                            2 => pte &= !1,
                            3 => pde &= !0x20,
                            4 => pte &= !0x20,
                            5 => pte &= !0x40,
                            6 => pde &= !2,
                            7 => pte &= !2,
                            8 => pde &= !4,
                            9 => pte &= !4,
                            10 => {
                                pages.watch(12);
                            }
                            11 => pte = 0xa0067, // Legacy VGA, not ordinary RAM.
                            12 => cpu.cr4 |= crate::x86::cpu::CR4_PAE,
                            13 => {
                                cpu.cr4 |= crate::x86::cpu::CR4_PSE;
                                pde |= 0x80;
                            }
                            _ => {}
                        }
                        bus.jit_code_pages = Some(pages);
                        write_ram_u32(&mut bus, 0x4000, pde);
                        write_ram_u32(&mut bus, 0x501c, pte);
                        let before = bus.ram.to_vec();
                        let expected = match case {
                            1..=4 | 11..=13 => false,
                            5 | 10 => !write,
                            6 | 7 => !write || !user && !wp,
                            8 | 9 => !user,
                            _ => true,
                        };
                        let token = mmu::try_resident_data_translation(
                            &mut cpu,
                            &mut bus,
                            LinearAddr(0x7004),
                            write,
                        );
                        assert_eq!(
                            token.is_some(),
                            expected,
                            "case={case} user={user} wp={wp} write={write}"
                        );
                        assert_eq!(bus.ram, before);
                        assert_eq!(cpu.cr2, 0x1234_5678);
                        assert_eq!(cpu.eip, 0x1000);
                        assert_eq!(cpu.tsc(), 0);
                        if let Some(token) = token {
                            assert_eq!(bus.cached_backing_phys(token), PhysAddr(0xc004));
                            assert_eq!(
                                cpu.cached_read_translation(LinearAddr(0x7004), user),
                                Some(token)
                            );
                            if write {
                                assert_eq!(
                                    cpu.cached_write_translation(LinearAddr(0x7004), user),
                                    Some(token)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_refills_preserve_live_state_across_read_write_collisions() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
            cpu.cr3 = 0x4000;
            cpu.regs = [0xffff_0001, 0, 0xdead_beef, 0, 0x9000, 0, 0, 0];
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (addr, value) in [
                (0x4000, 0x5027),
                (0x4008, 0x6027),
                (0x5004, 0x3027),
                (0x501c, 0xc067),
                (0x601c, 0xd067),
                (0xc000, 0x8765_4321),
                (0xd000, 0x1357_9bdf),
            ] {
                write_ram_u32(&mut bus, addr, value);
            }
            let code = [
                0x8b, 0x1d, 0, 0x70, 0, 0, // MOV EBX,[7000].
                0x01, 0xd8, // ADD EAX,EBX.
                0x8b, 0x0d, 0, 0x70, 0x80, 0, // MOV ECX,[807000], same TLB slot.
                0x11, 0xc8, // ADC EAX,ECX uses carry live across the helper.
                0x01, 0x15, 4, 0x70, 0, 0, // ADD [7004],EDX, write collision.
                0x01, 0x15, 4, 0x70, 0x80,
                0,    // A second writer sustains translation-cache collision pressure.
                0x47, // INC EDI.
                0xeb, 0xe1,
            ];
            bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 20_001);
        let native = engine
            .run_batch(&mut actual, &mut actual_bus, 0, 20_001)
            .unwrap();
        assert_eq!(native, reference);
        assert_eq!(actual.regs, expected.regs);
        assert_eq!(actual.eip, expected.eip);
        assert_eq!(actual.tsc(), expected.tsc());
        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
        assert_eq!(actual_bus.ram, expected_bus.ram);
        assert!(engine.stats.native_steps > 19_000);
        assert!(engine.stats.refilled_data > 4000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_stack_runs_keep_individual_commits_widths_and_esp_sources() {
        for (mut program, fault_addr) in [
            (
                vec![
                    0x50, 0x54, 0x66, 0x53, 0x51, // PUSH EAX, ESP, BX, ECX.
                    0x5a, 0x66, 0x5e, 0x5d, 0x5f, // POP EDX, SI, EBP, EDI.
                    0x40,
                ],
                0x6fff,
            ),
            (
                vec![
                    0x50, 0x89, 0xe5, 0x9c, // PUSH EAX; MOV EBP,ESP; PUSHFD.
                    0x41, 0x54, 0x31, 0xff, 0x66,
                    0x53, // INC ECX; PUSH ESP; XOR EDI,EDI; PUSH BX.
                    0x66, 0x5a, 0x89, 0xf6, 0x5f, 0x4f, 0x59, 0x5d, 0x40,
                ],
                0x6fff,
            ),
            (
                vec![
                    0x50, 0x89, 0xe2, 0x83, 0xec, 4, // PUSH EAX; MOV EDX,ESP; SUB ESP,4.
                    0x54, 0x59, 0x89, 0xd4, 0x5f,
                    0x40, // PUSH ESP; POP ECX; MOV ESP,EDX; POP EDI.
                ],
                0x6ffb,
            ),
            (
                vec![
                    0x50, 0x8b, 0x3d, 0, 0x60, 0,
                    0, // An unrelated load replaces the native page bias.
                    0x54, 0x5b, 0x5f, 0x40,
                ],
                0x6fff,
            ),
            (
                vec![
                    0x54, 0x5c, // PUSH ESP; POP ESP restores the original value.
                    0x50, 0x54, 0x5c, 0x5b, // Same pair below an earlier push.
                    0x66, 0x54, 0x66, 0x5c, // PUSH SP; POP SP retains merge order.
                    0x40,
                ],
                0x6fff,
            ),
        ] {
            let back = (0u8).wrapping_sub(program.len() as u8 + 2);
            program.extend([0xeb, back]);
            for initial_sp in [0, 0x6ff0, 0x7007, 0x7010] {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    cpu.regs = [0x12345678, 0x9abcdef0, 0, 0xfaceb00c, initial_sp, 0, 0, 0];
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    for (addr, value) in [
                        (0x4000, 0x5007),
                        (0x5004, 0x3007),
                        (0x5018, 0xa007),
                        (0x501c, 0xb007),
                        (0x4ffc, 0x8007), // ESP=0 pushes wrap into the top page.
                        (0x8ffc, 0xb007),
                    ] {
                        write_ram_u32(&mut bus, addr, value);
                    }
                    bus.ram[0x3000..0x3000 + program.len()].copy_from_slice(&program);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                let mut steps = 0;
                for phase in 0..3 {
                    if phase == 1 {
                        // Stack aliases a watched code page, without changing the
                        // instructions at the start of that physical page.
                        for bus in [&mut expected_bus, &mut actual_bus] {
                            write_ram_u32(bus, 0x5018, 0x3027);
                        }
                    }
                    if phase == 2 {
                        for bus in [&mut expected_bus, &mut actual_bus] {
                            write_ram_u32(bus, 0x5018, 0xa025);
                        }
                    }
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs[4] = if phase == 2 { 0x7007 } else { initial_sp };
                        cpu.invalidate_non_global_translations();
                    }
                    for budget in [4001, 3, 2, 101] {
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                        let native = engine
                            .run_batch(&mut actual, &mut actual_bus, steps, budget)
                            .unwrap();
                        assert_eq!(native, reference, "sp={initial_sp:x} phase={phase}");
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.cr2, expected.cr2);
                        assert_eq!(actual_bus.ram, expected_bus.ram);
                        steps += native.ran;
                        if phase == 2 {
                            break;
                        }
                    }
                }
                assert!(
                    engine.stats.native_steps > 1000,
                    "sp={initial_sp:x}: {:?}",
                    engine.stats
                );
                assert_eq!(actual.cr2, fault_addr);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_stack_transfers_keep_prior_push_pop_commits_before_later_faults() {
        for narrow in [false, true] {
            for write in [false, true] {
                let width = if narrow { 2 } else { 4 };
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5027);
                    write_ram_u32(&mut bus, 0x5004, 0x3027);
                    let mut code = if narrow { vec![0x66] } else { vec![] };
                    code.push(if write { 0x50 } else { 0x58 });
                    if write {
                        code.push(0xe8); // PUSH AX/EAX; CALL back to the start.
                        let displacement = 0u32.wrapping_sub(code.len() as u32 + 4);
                        code.extend(displacement.to_le_bytes());
                    } else {
                        code.extend([0xc2, 8, 0]); // POP AX/EAX; RET 8.
                    }
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                let mut steps = 0;
                for fault in [false, true] {
                    let stack_base = if fault { 0x7000 } else { 0x6800 };
                    let physical = if fault { 0xb000 } else { 0xa800 };
                    for round in 0..4 {
                        for bus in [&mut expected_bus, &mut actual_bus] {
                            write_ram_u32(
                                bus,
                                0x5018,
                                if fault && write { 0xa025 } else { 0xa067 },
                            );
                            write_ram_u32(bus, 0x501c, if fault && !write { 0 } else { 0xb067 });
                            write_ram_u32(bus, physical, 0x1000);
                            bus.ram[physical - width as usize..physical]
                                .copy_from_slice(&0x12345678u32.to_le_bytes()[..width as usize]);
                        }
                        for cpu in [&mut expected, &mut actual] {
                            cpu.eip = 0x1000;
                            cpu.regs = [
                                0xdeadbeef,
                                0,
                                0,
                                0,
                                if write {
                                    stack_base + width
                                } else {
                                    stack_base - width
                                },
                                0,
                                0,
                                0,
                            ];
                            cpu.invalidate_non_global_translations();
                        }
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut expected_bus, steps, 2);
                        let native = engine
                            .run_batch(&mut actual, &mut actual_bus, steps, 2)
                            .unwrap();
                        assert_eq!(
                            native, reference,
                            "narrow={narrow} write={write} fault={fault} round={round}"
                        );
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual.cr2, expected.cr2);
                        assert_eq!(actual_bus.ram, expected_bus.ram);
                        steps += native.ran;
                    }
                }
                assert!(engine.stats.native_steps > 0);
                assert_eq!(actual.regs[4], 0x7000);
                assert_eq!(actual.cr2, if write { 0x6ffc } else { 0x7000 });
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_rechecks_changed_roots_against_current_tables_and_ad_bits() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (addr, value) in [
                (0x4000, 0x5027),
                (0x8000, 0x5027),
                (0x5004, 0x3027),
                (0x5018, 0x7067),
            ] {
                write_ram_u32(&mut bus, addr, value);
            }
            bus.ram[0x3000..0x300e].copy_from_slice(&[
                0x90, // Refetching code establishes the current ordinary RAM root.
                0xa1, 0x00, 0x60, 0, 0,    // MOV EAX,[6000h].
                0x40, // INC EAX.
                0xa3, 0x00, 0x60, 0, 0, // MOV [6000h],EAX.
                0xeb, 0xf2,
            ]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..6 {
            for bus in [&mut expected_bus, &mut actual_bus] {
                if phase == 2 {
                    write_ram_u32(bus, 0x5018, 0x9067);
                }
                if phase == 3 {
                    write_ram_u32(bus, 0x5018, 0x9007);
                }
                if phase == 4 {
                    write_ram_u32(bus, 0x8000, 0x5025);
                }
            }
            for cpu in [&mut expected, &mut actual] {
                cpu.eip = 0x1000;
                cpu.cr3 = if phase == 0 { 0x4000 } else { 0x8000 };
                cpu.invalidate_non_global_translations();
                if phase == 5 {
                    cpu.cr0 |= crate::x86::cpu::CR0_WP;
                    cpu.invalidate_translation_context();
                }
            }
            let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, steps, 1000);
            let native = engine
                .run_batch(&mut actual, &mut actual_bus, steps, 1000)
                .unwrap();
            assert_eq!(native, reference, "phase={phase}");
            assert_eq!(actual.regs, expected.regs, "phase={phase}");
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(actual.cr2, expected.cr2);
            assert_eq!(actual_bus.ram, expected_bus.ram, "phase={phase}");
            if phase == 1 {
                assert!(engine.stats.rechecked_data > 0);
            }
            if phase < 5 {
                assert_eq!(native.ran, 1000);
            }
            steps += native.ran;
        }
        assert_eq!(actual.cr2, 0x6000);
        assert!(engine.stats.native_steps > 4000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_code_rechecks_require_current_mapping_accessed_and_content() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (addr, value) in [
                (0x4000, 0x5027),
                (0x8000, 0x5027),
                (0x5004, 0x3027),
                (0x5008, 0x6027),
            ] {
                write_ram_u32(&mut bus, addr, value);
            }
            bus.ram[0x3000..0x3006].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x6000..0x6006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            bus.ram[0x7000..0x7006].copy_from_slice(&[0x4a, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..6 {
            for cpu in [&mut expected, &mut actual] {
                cpu.eip = 0x1000;
                cpu.cr3 = if phase == 0 { 0x4000 } else { 0x8000 };
                cpu.invalidate_non_global_translations();
            }
            for bus in [&mut expected_bus, &mut actual_bus] {
                match phase {
                    2 => bus.write_phys_u8(PhysAddr(0x6000), 0x49).unwrap(),
                    3 => write_ram_u32(bus, 0x5008, 0x7027),
                    4 => write_ram_u32(bus, 0x5008, 0x7007),
                    5 => write_ram_u32(bus, 0x5008, 0),
                    _ => {}
                }
            }
            for budget in [101, 2001] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase}");
                assert_eq!(actual.regs, expected.regs, "phase={phase}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(actual_bus.ram, expected_bus.ram, "phase={phase}");
                steps += native.ran;
                if phase == 5 {
                    break;
                }
            }
            if phase == 1 {
                assert!(engine.stats.rechecked_code > 0);
            }
        }
        assert_eq!(actual.cr2, 0x2000);
        assert!(engine.stats.native_steps > 9000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_resident_fetch_reenters_after_interpretation_and_keeps_fault_order() {
        for cpl in [0, 3] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1ffd, 0);
                cpu.set_seg_cache(
                    SegmentReg::Cs,
                    SegmentCache::flat(8 | cpl, 0xc09b | (cpl << 5)),
                );
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0x4000;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                // RDTSC; MOV EAX,imm32 across the code-page boundary; then a
                // native load, INC EDX and JMP back to the interpreted RDTSC.
                bus.ram[0x3ffd..0x4000].copy_from_slice(&[0x0f, 0x31, 0xb8]);
                bus.ram[0x6000..0x6009]
                    .copy_from_slice(&[0x78, 0x56, 0x34, 0x12, 0x8b, 0x0e, 0x42, 0xeb, 0xf4]);
                bus.ram[0x7000..0x7009]
                    .copy_from_slice(&[0xef, 0xbe, 0xad, 0xde, 0x8b, 0x0e, 0x42, 0xeb, 0xf4]);
                write_ram_u32(&mut bus, 0x8000, 0x1234_abcd);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            let mut steps = 0;
            for phase in 0..9 {
                for _ in 0..if phase == 0 { 4 } else { 1 } {
                    for budget in [0, 1, 2, 3, 6] {
                        for attention in [false, true] {
                            for bus in [&mut expected_bus, &mut actual_bus] {
                                write_ram_u32(
                                    bus,
                                    0x4000,
                                    if phase == 6 { 0x5023 } else { 0x5027 },
                                );
                                write_ram_u32(
                                    bus,
                                    0x5004,
                                    match phase {
                                        5 => 0x3023, // Supervisor execute page.
                                        7 => 0x3007, // Missing A bit needs the walker.
                                        _ => 0x3027,
                                    },
                                );
                                write_ram_u32(
                                    bus,
                                    0x5008,
                                    match phase {
                                        2 | 7 => 0x6007,
                                        3 => 0x7027, // Changed second code page.
                                        4 => 0,      // Fault while fetching MOV's immediate.
                                        _ => 0x6027,
                                    },
                                );
                                write_ram_u32(bus, 0x5018, if phase == 8 { 0 } else { 0x8027 });
                                bus.next_attention_step =
                                    if attention { steps + 1 } else { u64::MAX };
                            }
                            for cpu in [&mut expected, &mut actual] {
                                cpu.eip = 0x1ffd;
                                cpu.regs = [0; 8];
                                cpu.regs[6] = 0x6000;
                                cpu.replace_eflags(0x202);
                                cpu.invalidate_non_global_translations();
                            }
                            let before = engine.stats.native_steps;
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                steps,
                                budget,
                            );
                            let result = engine
                                .run_batch(&mut actual, &mut actual_bus, steps, budget)
                                .unwrap();
                            assert_eq!(
                                result, reference,
                                "cpl={cpl} phase={phase} budget={budget} attention={attention}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                            if phase == 1 && budget == 6 && !attention {
                                assert_eq!(engine.stats.native_steps - before, 3);
                            }
                            steps += result.ran;
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_interpreter_fetch_handoff_obeys_remapping_content_and_page_faults() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (addr, value) in [(0x4000, 0x5027), (0x8000, 0x5027), (0x5004, 0x3027)] {
                write_ram_u32(&mut bus, addr, value);
            }
            // INC ESI; RDTSC (interpreter boundary); JMP back.
            bus.ram[0x3000..0x3005].copy_from_slice(&[0x46, 0x0f, 0x31, 0xeb, 0xfb]);
            bus.ram[0x6000..0x6005].copy_from_slice(&[0x47, 0x0f, 0x31, 0xeb, 0xfb]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (phase, shadowed) in (0..6).flat_map(|phase| [(phase, false), (phase, true)]) {
            for cpu in [&mut expected, &mut actual] {
                cpu.eip = 0x1000;
                cpu.restore_interrupt_shadow(shadowed);
                // Eviction alone does not architecturally invalidate a native
                // execute translation. The bytes must still be read afresh.
                cpu.fetch_cache.invalidate();
                cpu.fetch_cursor.invalidate();
                if phase == 2 {
                    cpu.cr3 = 0x8000;
                    cpu.invalidate_non_global_translations();
                }
                if phase >= 3 {
                    cpu.invalidate_translation_page(LinearAddr(0x1000));
                }
            }
            for bus in [&mut expected_bus, &mut actual_bus] {
                match phase {
                    3 => write_ram_u32(bus, 0x5004, 0x6007),
                    4 => bus.write_phys_u8(PhysAddr(0x6000), 0x4f).unwrap(),
                    5 => write_ram_u32(bus, 0x5004, 0),
                    _ => {}
                }
            }
            for budget in [1, 2, 3, 17, 1000] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, steps, budget)
                    .unwrap();
                assert_eq!(
                    native, reference,
                    "phase={phase} shadowed={shadowed} budget={budget}"
                );
                assert_eq!(actual.regs, expected.regs, "phase={phase}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.interrupt_shadow(), expected.interrupt_shadow());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(actual_bus.ram, expected_bus.ram, "phase={phase}");
                steps += native.ran;
                if phase == 5 {
                    break;
                }
            }
        }
        assert_eq!(actual.cr2, 0x1000);
        assert!(engine.stats.native_steps > 2000);
        assert!(engine.stats.interpreted_steps > 1000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_overlapping_entries_preserve_instruction_boundaries_and_fault_progress() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1001, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            cpu.eflags |= 1;
            cpu.regs[2] = u32::MAX;
            cpu.regs[6] = 0x2000;
            cpu.regs[7] = 0x2010;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            write_ram_u32(&mut bus, 0x4000, 0x5007);
            write_ram_u32(&mut bus, 0x5004, 0x3007);
            write_ram_u32(&mut bus, 0x5008, 0x6007);
            write_ram_u32(&mut bus, 0x6000, 0x1234_5678);
            write_ram_u32(&mut bus, 0x7000, 0x8765_4321);
            bus.ram[0x3000..0x300d].copy_from_slice(&[
                0xb8, 0x40, 0x41, 0x90, 0x90, // MOV immediate contains INC/NOP entries
                0x8b, 0x1e, // MOV EBX,[ESI]
                0x11, 0xd0, // ADC EAX,EDX consumes carry from the entering stream
                0x89, 0x07, // MOV [EDI],EAX
                0xeb, 0xf8, // JMP 1005h
            ]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..4 {
            if phase == 1 {
                // Change both an embedded instruction and the MOV immediate.
                for bus in [&mut expected_bus, &mut actual_bus] {
                    bus.write_phys_u8(PhysAddr(0x3002), 0x49).unwrap();
                }
            }
            if phase >= 2 {
                let pte = if phase == 2 { 0 } else { 0x7007 };
                write_ram_u32(&mut expected_bus, 0x5008, pte);
                write_ram_u32(&mut actual_bus, 0x5008, pte);
                expected.invalidate_translation_page(LinearAddr(0x2000));
                actual.invalidate_translation_page(LinearAddr(0x2000));
            }
            // Compile the embedded stream first. The outer MOV must consume
            // its complete immediate, while both streams may join at 1005h.
            for entry in [0x1001, 0x1000, 0x1002] {
                expected.eip = entry;
                actual.eip = entry;
                for budget in [1, 3, 13, 127] {
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                    let native = engine
                        .run_batch(&mut actual, &mut actual_bus, steps, budget)
                        .unwrap();
                    assert_eq!(native, reference, "phase={phase} entry={entry:x}");
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                    steps += native.ran;
                    if !matches!(native.stop, BatchStop::Budget) {
                        assert_eq!(phase, 2);
                        assert!(matches!(
                            native.stop,
                            BatchStop::Step {
                                instruction_eip: 0x1005,
                                reason: BatchStepStop::Error(StepError::PageFault(_)),
                                ..
                            }
                        ));
                        assert_eq!(actual.cr2, 0x2000);
                        break;
                    }
                }
            }
        }
        assert!(engine.stats.native_steps > 500);
        assert!(engine.stats.linked_edges > 0);
        assert!(engine.stats.content_changes > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_linked_backedges_obey_mapping_changes_code_versions_and_budgets() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
            write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
            write_ram_u32(&mut bus, 0x5008, 0x6000 | 7);
            write_ram_u32(&mut bus, 0x5014, 0x5000 | 7); // guest alias of the PT
            bus.ram[0x3000..0x3006].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x6000..0x6006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            bus.ram[0x7000..0x7006].copy_from_slice(&[0x49, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..5 {
            if phase == 1 {
                // A native link can outlive every ordinary execute-cache entry.
                // Remapping still has to expire it when that cache is empty.
                expected.fetch_cache.invalidate();
                expected.fetch_cursor.invalidate();
                actual.fetch_cache.invalidate();
                actual.fetch_cursor.invalidate();
            }
            if phase == 3 {
                // Arena reclamation must discard links before reusing addresses.
                engine.clear(&mut native_bus).unwrap();
            }
            if phase == 1 || phase == 3 || phase == 4 {
                let pte = match phase {
                    1 => 0x7000 | 7,
                    3 => 0x6000 | 7,
                    _ => 0,
                };
                mmu::write_linear_u32(&mut expected, &mut reference_bus, LinearAddr(0x5008), pte)
                    .unwrap();
                mmu::write_linear_u32(&mut actual, &mut native_bus, LinearAddr(0x5008), pte)
                    .unwrap();
                // The PTE store alone may leave a cached mapping in either
                // backend. Architectural INVLPG must also retire native links
                // after the ordinary execute cache has already been evicted.
                expected.invalidate_translation_page(LinearAddr(0x2000));
                actual.invalidate_translation_page(LinearAddr(0x2000));
            } else if phase == 2 {
                // A device write changes content without changing the mapping.
                reference_bus.write_phys_u8(PhysAddr(0x7000), 0x43).unwrap();
                native_bus.write_phys_u8(PhysAddr(0x7000), 0x43).unwrap();
            }
            expected.eip = 0x1000;
            actual.eip = 0x1000;
            for budget in [1, 3, 19, 2000] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase} budget={budget}");
                assert_eq!(actual.regs, expected.regs, "phase={phase} budget={budget}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(native_bus.ram, reference_bus.ram);
                steps += native.ran;
                if !matches!(native.stop, BatchStop::Budget) {
                    break;
                }
            }
        }
        assert!(engine.stats.native_steps > 7000);
        assert!(engine.stats.native_invocations < 100);
        assert_eq!(actual.cr2, 0x2000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cross_page_targets_obey_code_changes_after_dispatch_collision() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x404000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            bus.ram[0x1000..0x1006].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x2000..0x2006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            // The same dispatch slot as 2000h, but different code and backing.
            bus.ram[0x402000..0x402003].copy_from_slice(&[0x42, 0xeb, 0xfd]);
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for phase in 0..9 {
            for cpu in [&mut expected, &mut actual] {
                cpu.eip = if phase == 1 { 0x402000 } else { 0x1000 };
            }
            if (2..5).contains(&phase) {
                // Mutate the evicted target, then mutate its replacement again.
                // The old caller remains live through both content versions.
                let opcode = if phase & 1 == 0 { 0x49 } else { 0x41 };
                for bus in [&mut reference_bus, &mut native_bus] {
                    bus.write_phys_u8(PhysAddr(0x2000), opcode).unwrap();
                }
            } else if (5..7).contains(&phase) {
                // Renew the target after a byte-identical write, then a write
                // outside its instructions; both retain the actual code.
                for bus in [&mut reference_bus, &mut native_bus] {
                    let address = if phase == 5 { 0x2000 } else { 0x2fff };
                    bus.write_phys_u8(PhysAddr(address), 0x49).unwrap();
                }
            } else if phase == 7 {
                // Two distinct retirement notifications before re-entry must
                // invalidate both pages, not just the last reported write.
                for bus in [&mut reference_bus, &mut native_bus] {
                    bus.write_phys_u8(PhysAddr(0x1000), 0x48).unwrap();
                    bus.write_phys_u8(PhysAddr(0x2000), 0x41).unwrap();
                }
            } else if phase == 8 {
                // One bulk notification can retire multiple watched pages.
                // Stage the new image in ordinary data, then use the bus copy.
                for bus in [&mut reference_bus, &mut native_bus] {
                    bus.ram.copy_within(0x1000..0x3000, 0x5000);
                    bus.ram[0x5000] = 0x40;
                    bus.ram[0x6000] = 0x49;
                    bus.copy_phys_to_ram(PhysAddr(0x5000), PhysAddr(0x1000), 8192, 1)
                        .unwrap();
                }
            }
            for budget in [1, 3, 2000] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase} budget={budget}");
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
                steps += native.ran;
            }
        }
        assert!(engine.stats.native_steps > 8000);
        assert!(engine.stats.content_reuses > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cross_page_edges_revalidate_privilege_and_changed_cs_base() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (at, value) in [
                (0x4000, 0x5007),
                (0x5004, 0x3007),
                (0x5008, 0x6003), // Initial target is supervisor-only.
                (0x5044, 0x3007), // Same source backing at another CS base.
                (0x5048, 0x7007), // Same guest target PC, different code.
                (0x4ffc, 0x8007),
                (0x8ffc, 0x3007), // Source at linear fffff000h.
                (0x5000, 0x7007), // Target wraps to linear zero.
            ] {
                write_ram_u32(&mut bus, at, value);
            }
            bus.ram[0x3000..0x3006].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x6000..0x6006].copy_from_slice(&[0x41, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            bus.ram[0x7000..0x7006].copy_from_slice(&[0x42, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (cpl, base) in [
            (0, 0),
            (0, 0),
            (3, 0),
            (0, 0),
            (0, 0x10000),
            (3, 0x10000),
            (0, 0xffff_e000),
            (3, 0xffff_e000),
            (0, 0),
        ] {
            for cpu in [&mut expected, &mut actual] {
                let mut cs = SegmentCache::flat(8 | cpl, if cpl == 3 { 0xc0fb } else { 0xc09b });
                cs.base = base;
                cpu.set_seg_cache(SegmentReg::Cs, cs);
                cpu.eip = 0x1000;
            }
            let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, steps, 2000);
            let native = engine
                .run_batch(&mut actual, &mut actual_bus, steps, 2000)
                .unwrap();
            assert_eq!(native, reference, "cpl={cpl} base={base:x}");
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(actual.cr2, expected.cr2);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual_bus.ram, expected_bus.ram);
            if cpl == 3 && base == 0 {
                assert_eq!(actual.cr2, 0x2000);
                assert!(!matches!(native.stop, BatchStop::Budget));
            } else {
                assert_eq!(native.ran, 2000);
            }
            steps += native.ran;
        }
        assert!(engine.stats.native_steps > 8000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_repeated_reads_obey_invlpg_cr3_and_privilege_changes() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (at, value) in [
                (0x4000, 0x5007),
                (0x5004, 0x3007),
                (0x5008, 0x6003),
                (0x8000, 0x9007),
                (0x9004, 0x3007),
                (0x9008, 0x7007),
                (0x6000, 0x1111_1111),
                (0x7000, 0x2222_2222),
            ] {
                write_ram_u32(&mut bus, at, value);
            }
            bus.ram[0x3000..0x300d].copy_from_slice(&[
                0x8b, 0x1d, 0, 0x20, 0, 0, // MOV EBX,[0x2000]
                0x01, 0xd8, // ADD EAX,EBX
                0xe9, 0xf3, 0xff, 0xff, 0xff, // JMP 0x1000
            ]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (phase, (cpl, cr3)) in [
            (0, 0x4000),
            (0, 0x4000),
            (3, 0x4000),
            (3, 0x8000),
            (0, 0x4000),
        ]
        .into_iter()
        .enumerate()
        {
            for (cpu, bus) in [
                (&mut expected, &mut expected_bus),
                (&mut actual, &mut actual_bus),
            ] {
                if phase == 1 {
                    write_ram_u32(bus, 0x5008, 0x7003);
                    cpu.invalidate_translation_page(crate::x86::LinearAddr(0x2000));
                }
                if cpu.cr3 != cr3 {
                    cpu.cr3 = cr3;
                    cpu.invalidate_translation_context();
                }
                cpu.set_seg_cache(
                    SegmentReg::Cs,
                    SegmentCache::flat(8 | cpl, if cpl == 3 { 0xc0fb } else { 0xc09b }),
                );
                cpu.eip = 0x1000;
            }
            let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, steps, 300);
            let native = engine
                .run_batch(&mut actual, &mut actual_bus, steps, 300)
                .unwrap();
            assert_eq!(native, reference, "phase {phase}");
            assert_eq!(actual.regs, expected.regs, "phase {phase}");
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(actual.cr2, expected.cr2);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual_bus.ram, expected_bus.ram);
            if phase == 2 {
                assert_eq!(actual.cr2, 0x2000);
                assert!(!matches!(native.stop, BatchStop::Budget));
            } else {
                assert_eq!(native.ran, 300);
            }
            steps += native.ran;
        }
        assert!(engine.stats.native_steps > 1000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_global_code_survives_cr3_but_obeys_invlpg_pge_and_content_writes() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr4 = crate::x86::cpu::CR4_PGE;
            cpu.cr3 = 0x4000;
            let mut bus = HostBus::new(0x20000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            for (addr, value) in [
                (0x4000, 0x5007),
                (0x8000, 0xa007),
                (0x5004, 0x3107),
                (0x5008, 0x6007),
                (0xa004, 0x7107),
                (0xa008, 0x9007),
            ] {
                write_ram_u32(&mut bus, addr, value);
            }
            bus.ram[0x3000..0x3006].copy_from_slice(&[0x46, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x7000..0x7006].copy_from_slice(&[0x45, 0xe9, 0xfa, 0x0f, 0, 0]);
            bus.ram[0x6000..0x6006].copy_from_slice(&[0x47, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            bus.ram[0x9000..0x9006].copy_from_slice(&[0x4f, 0xe9, 0xfa, 0xef, 0xff, 0xff]);
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (phase, cr3) in [0x4000, 0x8000, 0x4000, 0x8000, 0x8000, 0x4000]
            .into_iter()
            .enumerate()
        {
            for cpu in [&mut expected, &mut actual] {
                if phase == 4 {
                    // The pending global INVLPG must survive a subsequent CR3
                    // notification before the engine consumes either one.
                    cpu.invalidate_translation_page(LinearAddr(0x1000));
                }
                cpu.cr3 = cr3;
                cpu.invalidate_non_global_translations();
                if phase == 5 {
                    cpu.cr4 = 0;
                    cpu.invalidate_translation_context();
                }
                cpu.eip = 0x1000;
            }
            if phase == 3 {
                expected_bus.write_phys_u8(PhysAddr(0x3000), 0x4e).unwrap();
                actual_bus.write_phys_u8(PhysAddr(0x3000), 0x4e).unwrap();
            }
            for budget in [3, 101, 2001] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase}");
                assert_eq!(actual.regs, expected.regs, "phase={phase}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                steps += native.ran;
            }
            if phase < 4 {
                assert_eq!(actual.regs[5], 0);
            }
            if phase == 4 {
                assert!(actual.regs[5] > 0);
            }
        }
        assert!(engine.stats.native_steps > 10_000);
        assert!(engine.stats.global_link_retags > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_large_page_invlpg_retires_all_compiled_subpages() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x401000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr4 = crate::x86::cpu::CR4_PSE | crate::x86::cpu::CR4_PGE;
            cpu.cr3 = 0x8000;
            let mut bus = HostBus::new(0x800000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            write_ram_u32(&mut bus, 0x8004, 0x187);
            for backing in [0usize, 0x400000] {
                for (offset, target, opcode) in [(0x1000, 0x3000, 0x40), (0x3000, 0x1000, 0x41)] {
                    let at = backing + offset;
                    bus.ram[at] = opcode + if backing == 0 { 0 } else { 2 };
                    bus.ram[at + 1] = 0xe9;
                    let displacement = (target as u32).wrapping_sub(offset as u32 + 6);
                    bus.ram[at + 2..at + 6].copy_from_slice(&displacement.to_le_bytes());
                }
            }
            (cpu, bus)
        };
        let (mut expected, mut expected_bus) = make();
        let (mut actual, mut actual_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (phase, backing) in [0, 0x400000, 0].into_iter().enumerate() {
            if phase != 0 {
                for (cpu, bus) in [
                    (&mut expected, &mut expected_bus),
                    (&mut actual, &mut actual_bus),
                ] {
                    write_ram_u32(bus, 0x8004, backing | 0x187);
                    // Invalidate an unused 4 KiB piece of the same large page.
                    // Both compiled endpoints must lose their old mapping.
                    cpu.invalidate_translation_page(LinearAddr(0x402123));
                }
            }
            let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, steps, 2000);
            let native = engine
                .run_batch(&mut actual, &mut actual_bus, steps, 2000)
                .unwrap();
            assert_eq!(native, reference, "phase={phase}");
            assert_eq!(actual.regs, expected.regs, "phase={phase}");
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(actual.tsc(), expected.tsc());
            assert_eq!(actual.cr2, expected.cr2);
            assert_eq!(actual_bus.ram, expected_bus.ram);
            steps += native.ran;
        }
        assert_eq!(&actual.regs[..4], &[1000, 1000, 500, 500]);
        assert!(engine.stats.native_steps > 5000);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_page_entries_follow_cs_rebasing_revalidation_and_content_changes() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            let mut bus = HostBus::new(0x10000);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            bus.ram[0x1000..0x1004].copy_from_slice(&[0x90, 0x40, 0xeb, 4]);
            bus.ram[0x1008..0x100b].copy_from_slice(&[0x43, 0xeb, 5]);
            bus.ram[0x1010..0x1013].copy_from_slice(&[0x41, 0xeb, 0xee]);
            (cpu, bus)
        };
        let (mut expected, mut reference_bus) = make();
        let (mut actual, mut native_bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        let mut steps = 0;
        for (phase, base) in [0u32, 0x123, 0x3abc, 0xffff_f123, 0x123]
            .into_iter()
            .enumerate()
        {
            for cpu in [&mut expected, &mut actual] {
                cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::new(8, base, u32::MAX, 0xc09b));
                cpu.eip = 0x1000u32.wrapping_sub(base);
            }
            if phase == 3 {
                reference_bus.write_phys_u8(PhysAddr(0x1008), 0x4b).unwrap();
                native_bus.write_phys_u8(PhysAddr(0x1008), 0x4b).unwrap();
            }
            for budget in [1, 3, 1001, 2003] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, steps, budget);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, steps, budget)
                    .unwrap();
                assert_eq!(native, reference, "phase={phase} budget={budget}");
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
                steps += native.ran;
                // Simulate a context invalidation with unchanged code mapping.
                // Every page must be revalidated before any of its entries runs.
                expected.invalidate_translation_context();
                actual.invalidate_translation_context();
            }
        }
        assert!(engine.stats.native_steps > 10_000);
        assert!(engine.stats.content_changes > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_exchange_families_preserve_write_faults_and_address_aliases() {
        for bits in [8, 16, 32] {
            for opcode in [0x86, 0xb0, 0xc0] {
                // Aligned/unaligned RAM, a split write, a missing second page,
                // and a read-only destination (including failed CMPXCHG).
                for shape in 0..5 {
                    for equal in [false, true] {
                        let address = if shape >= 2 && shape <= 3 && bits != 8 {
                            0x7001 - bits / 8
                        } else {
                            0x6000 + u32::from(shape != 0)
                        };
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                            cpu.cr3 = 0x4000;
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                            write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                            write_ram_u32(
                                &mut bus,
                                0x5018,
                                0x8000 | if shape == 4 { 5 } else { 7 },
                            );
                            if shape != 3 {
                                write_ram_u32(&mut bus, 0x501c, 0x9000 | 7);
                            }
                            let mut code = vec![0x41, 0xf0]; // INC ECX; LOCK op [EBX], EBX/BL/BX
                            if bits == 16 {
                                code.push(0x66);
                            }
                            if opcode != 0x86 {
                                code.push(0x0f);
                            }
                            code.extend([opcode + u8::from(bits != 8), 0x1b]);
                            code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                            bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut reference_bus) = make();
                        let (mut actual, mut native_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        // The first visit may only establish execute access;
                        // subsequent visits must reach native admission too.
                        for round in 0..4 {
                            for (cpu, bus) in [
                                (&mut expected, &mut reference_bus),
                                (&mut actual, &mut native_bus),
                            ] {
                                cpu.eip = 0x1000;
                                cpu.regs = [0x89ab_cdef, 0x7fff_ffff, 0, address, 0, 0, 0, 0];
                                if !equal {
                                    cpu.regs[0] ^= 1;
                                }
                                cpu.replace_eflags(0x8d7);
                                bus.ram[0x8000..0xa000].fill(0xa5);
                                let physical = address as usize + 0x2000;
                                bus.ram[physical..physical + (bits / 8) as usize].copy_from_slice(
                                    &0x89ab_cdefu32.to_le_bytes()[..(bits / 8) as usize],
                                );
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut reference_bus,
                                round * 3,
                                3,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut native_bus, round * 3, 3)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "bits={bits} op={opcode:x} shape={shape} equal={equal}"
                            );
                            assert_eq!(
                                actual.regs, expected.regs,
                                "bits={bits} op={opcode:x} shape={shape} equal={equal}"
                            );
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(native_bus.ram, reference_bus.ram);
                        }
                        assert!(engine.stats.native_steps > 0);
                        if shape <= 1 {
                            assert!(engine.stats.native_memory_writes > 0);
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_ioram_bank_mode_and_dirty_changes_match_interpretation() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.set_reg(Reg32::Esi, 0xa2001);
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            enable_test_vbe(&mut bus);
            // INC EAX; MOV [ESI],EAX; MOV EBX,[ESI]; JMP start.
            bus.ram[0x1000..0x1007].copy_from_slice(&[0x40, 0x89, 0x06, 0x8b, 0x1e, 0xeb, 0xf9]);
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            (cpu, bus)
        };
        let (mut expected, mut reference) = make();
        let (mut actual, mut bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        for phase in 0..5u64 {
            for b in [&mut reference, &mut bus] {
                b.vga.clear_dirty();
                if phase == 1 {
                    b.io_write_u16(0x1ce, 5).unwrap();
                    b.io_write_u16(0x1cf, 1).unwrap();
                } else if phase == 2 {
                    b.io_write_u16(0x1ce, 4).unwrap();
                    b.io_write_u16(0x1cf, 0).unwrap();
                    b.io_write_u16(0x3c4, 0x0e04).unwrap();
                } else if phase == 3 {
                    b.io_write_u16(0x3c4, 0x0502).unwrap();
                } else if phase == 4 {
                    b.io_write_u16(0x3c4, 0x0604).unwrap();
                }
                b.attention = false;
            }
            let result = Interpreter::run_batch(&mut expected, &mut reference, phase * 2000, 2000);
            let native = engine
                .run_batch(&mut actual, &mut bus, phase * 2000, 2000)
                .unwrap();
            assert_eq!(native, result);
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            for offset in (0..0x20000).step_by(4) {
                let addr = PhysAddr(0xe0000000 + offset);
                assert_eq!(
                    bus.read_phys_u32(addr).unwrap(),
                    reference.read_phys_u32(addr).unwrap()
                );
            }
            for page in 0..32 {
                assert_eq!(bus.vga.dirty_page(page), reference.vga.dirty_page(page));
            }
        }
        assert!(engine.stats.native_steps > 1000);
        assert!(engine.stats.native_memory_writes > 0);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_framebuffer_accesses_preserve_bytes_dirty_pages_faults_and_pci_decode() {
        use crate::devices::vga::DEFAULT_LFB_BASE;
        for case in 0..16 {
            let shape = case % 4;
            // Keep the same number of loop iterations with the added prefix.
            let budget = if case & 8 == 0 { 1001 } else { 1401 };
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                let mut bus = HostBus::new_firmware(0x200000).unwrap();
                enable_test_vbe(&mut bus);
                bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
                bus.io_write_u32(0xcfc, 2).unwrap();
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                for (at, word) in [
                    (0x4000, 0x5027),
                    (0x5004, 0x3027),
                    (
                        0x5018,
                        DEFAULT_LFB_BASE as u32 + 0x4000 | if shape == 3 { 0x65 } else { 0x67 },
                    ),
                    (
                        0x501c,
                        if shape == 2 {
                            0
                        } else {
                            DEFAULT_LFB_BASE as u32 + 0x5067
                        },
                    ),
                ] {
                    write_ram_u32(&mut bus, at, word);
                }
                let mut code = Vec::new();
                if case & 8 != 0 {
                    // An initial read proof must not authorize the following
                    // write or omit its dirty notification, including RO PTEs.
                    // NOP gives even a faulting first read a native prefix;
                    // SS keeps its read proof separate from the DS store range.
                    code.extend([0x90, 0x36, 0x8b, 0x1e]);
                }
                code.extend([0x40, 0x89, 0x06]);
                if case & 4 != 0 {
                    code.push(0x36); // SS reads need their own segment qualification.
                }
                code.extend([0x8b, 0x1e, 0x31, 0xd9]);
                code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for phase in 0..5 {
                for (cpu, bus) in [
                    (&mut expected, &mut expected_bus),
                    (&mut actual, &mut actual_bus),
                ] {
                    cpu.eip = 0x1000;
                    cpu.regs[6] = if shape == 1 || shape == 2 {
                        0x6fff
                    } else {
                        0x6001
                    };
                    if phase == 1 {
                        // No page-table edit/INVLPG: an old VRAM pointer must
                        // not survive a BAR change in an ordinary RAM TLB row.
                        bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
                        bus.io_write_u32(0xcfc, 0xfd00_0000).unwrap();
                    } else if phase == 2 {
                        write_ram_u32(
                            bus,
                            0x5018,
                            0xfd00_4000 | if shape == 3 { 0x65 } else { 0x67 },
                        );
                        if shape != 2 {
                            write_ram_u32(bus, 0x501c, 0xfd00_5067);
                        }
                        cpu.invalidate_translation_page(LinearAddr(0x6000));
                        cpu.invalidate_translation_page(LinearAddr(0x7000));
                    } else if phase >= 3 {
                        bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
                        bus.io_write_u32(0xcfc, if phase == 3 { 0 } else { 2 })
                            .unwrap();
                    }
                    bus.attention = false;
                    bus.vga.clear_dirty();
                }
                let reference = Interpreter::run_batch(
                    &mut expected,
                    &mut expected_bus,
                    phase * budget,
                    budget,
                );
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, phase * budget, budget)
                    .unwrap();
                assert_eq!(native, reference, "shape={shape} phase={phase}");
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                for offset in 0x4000..0x6004 {
                    assert_eq!(
                        actual_bus
                            .vga
                            .lfb_read_u8(DEFAULT_LFB_BASE + offset, DEFAULT_LFB_BASE),
                        expected_bus
                            .vga
                            .lfb_read_u8(DEFAULT_LFB_BASE + offset, DEFAULT_LFB_BASE)
                    );
                }
                assert_eq!(
                    actual_bus.vga.whole_frame_dirty(),
                    expected_bus.vga.whole_frame_dirty()
                );
                for page in 0..actual_bus.vga.vram_len() / 4096 {
                    assert_eq!(
                        actual_bus.vga.dirty_page(page),
                        expected_bus.vga.dirty_page(page)
                    );
                }
            }
            assert!(
                engine.stats.native_steps > 0,
                "case={case} {:?}",
                engine.stats
            );
            if shape == 0 {
                assert!(engine.stats.native_memory_reads > 500);
                assert!(engine.stats.native_memory_writes > 500);
                assert!(engine.stats.refilled_data > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_segment_reads_refresh_selectors_and_store_only_a_word() {
        for operand16 in [false, true] {
            for shape in 0..4 {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    for (at, word) in [
                        (0x4000, 0x5007),
                        (0x5004, 0x3007),
                        (0x5018, 0x8000 | if shape == 3 { 5 } else { 7 }),
                        (0x501c, if shape == 2 { 0 } else { 0x9007 }),
                    ] {
                        write_ram_u32(&mut bus, at, word);
                    }
                    bus.ram[0x8000..0xa000].fill(0xa5);
                    let mut code = vec![0x41];
                    if operand16 {
                        code.push(0x66);
                    }
                    code.extend([0x8c, 0xe8]); // MOV EAX/AX,GS.
                    if operand16 {
                        code.push(0x66);
                    }
                    code.extend([0x8c, 0x2b]); // MOV word [EBX],GS, in either size.
                    code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for (phase, selector) in [0x18, 0xfffc, 0, 0x38].into_iter().enumerate() {
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs[0] = 0x1234_5678;
                        cpu.regs[3] = if (1..=2).contains(&shape) {
                            0x6fff
                        } else {
                            0x6001
                        };
                        let mut gs = cpu.seg(SegmentReg::Gs);
                        gs.selector = selector;
                        cpu.set_seg_cache(SegmentReg::Gs, gs);
                        cpu.replace_eflags(0x8d7);
                    }
                    let reference = Interpreter::run_batch(
                        &mut expected,
                        &mut expected_bus,
                        phase as u64 * 31,
                        31,
                    );
                    let native = engine
                        .run_batch(&mut actual, &mut actual_bus, phase as u64 * 31, 31)
                        .unwrap();
                    assert_eq!(native, reference);
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(
                        actual.regs[0],
                        if operand16 { 0x1234_0000 } else { 0 } | u32::from(selector)
                    );
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                }
                assert!(engine.stats.native_steps > 0);
                if shape == 0 {
                    assert!(engine.stats.native_memory_writes > 0);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_stack_read_page_survives_refills_and_checks_boundaries_and_reentry() {
        for address in [0, 0x6000, 0x6ffb, 0x6ffc, 0x6ffe, 0xffff_fffc, 0xffff_fffe] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                let mut bus = HostBus::new(0x20000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                for (at, word) in [
                    (0x4000, 0x5027),
                    (0x4ffc, 0x6027),
                    (0x5000, 0x8067),
                    (0x5004, 0x3027),
                    (0x5018, 0x8067),
                    (0x501c, 0x9067),
                    (0x5040, 0x10067),
                    (0x5044, 0x11067),
                    (0x6ffc, 0x8067),
                ] {
                    write_ram_u32(&mut bus, at, word);
                }
                for at in 0x8000..0x20000 {
                    bus.ram[at] = (at ^ (at >> 8)) as u8;
                }
                let mut code = vec![
                    0x8b, 0x45, 0, // MOV EAX,[EBP].
                    0x8b, 0x16, // MOV EDX,[ESI]; cold refill uses caller-saved registers.
                    0x81, 0xf6, 0, 0x10, 0, 0,    // XOR ESI,0x1000.
                    0x45, // INC EBP; prevents a static shared-range proof.
                    0x8b, 0x5d, 0,    // MOV EBX,[EBP]; checks width at page end.
                    0x4d, // DEC EBP.
                    0x41, // INC ECX.
                ];
                code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for phase in 0..5 {
                for (cpu, bus) in [
                    (&mut expected, &mut expected_bus),
                    (&mut actual, &mut actual_bus),
                ] {
                    cpu.eip = 0x1000;
                    cpu.regs = [0, 0, 0, 0, 0, address, 0x10000, 0];
                    cpu.replace_eflags(0x8d7);
                    let mut ss = cpu.seg(SegmentReg::Ss);
                    ss.base = if phase == 2 { 0x1000 } else { 0 };
                    ss.limit = if phase == 3 { 2 } else { u32::MAX };
                    cpu.set_seg_cache(SegmentReg::Ss, ss);
                    // Changing the mapping followed by architectural
                    // invalidation must discard any register-resident page.
                    let mapping = if phase & 1 == 0 { 0x8067 } else { 0xa067 };
                    for pte in [0x5000, 0x5018, 0x6ffc] {
                        write_ram_u32(bus, pte, mapping);
                    }
                    cpu.invalidate_translation_context();
                }
                let native_steps = engine.stats.native_steps;
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, phase * 103, 103);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, phase * 103, 103)
                    .unwrap();
                assert_eq!(native, reference, "address={address:x} phase={phase}");
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.cr2, expected.cr2);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                if phase == 0 && address <= 0x6ffb {
                    assert!(engine.stats.native_steps > native_steps + 10);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_split_ranges_preserve_both_page_permissions_and_backing() {
        for user in [false, true] {
            for shape in 0..9 {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    if user {
                        let mut cs = cpu.seg(SegmentReg::Cs);
                        cs.selector |= 3;
                        cpu.set_seg_cache(SegmentReg::Cs, cs);
                    }
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    let second = match shape {
                        1 => 0xb067, // Noncontiguous RAM.
                        2 => 0,      // Missing second page.
                        3 => 0x9065, // Read-only second page.
                        4 => 0x9007, // A/D established by ordinary execution.
                        6 => 0x9063, // Supervisor-only second page.
                        8 => 0x8067, // Both linear pages alias the first page.
                        _ => 0x9067,
                    };
                    for (at, word) in [
                        (0x4000, 0x5027),
                        (0x5004, 0x3027),
                        (0x5018, if shape == 7 { 0x8065 } else { 0x8067 }),
                        (0x501c, second),
                    ] {
                        write_ram_u32(&mut bus, at, word);
                    }
                    for at in 0x8000..0xc000 {
                        bus.ram[at] = (at as u8).wrapping_mul(13);
                    }
                    let code = [
                        0x47, // INC EDI commits before any memory fault.
                        0x8b, 0x1e, // MOV EBX,[ESI], split scalar read.
                        0x01, 0x46, 4, // ADD [ESI+4],EAX, same range.
                        0x8b, 0x56, 8, // MOV EDX,[ESI+8].
                        0xeb, 0xf5, // JMP start.
                    ];
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    if shape == 5 {
                        let mut pages = crate::x86::jit::validity::CodePages::new(bus.ram.len());
                        pages.watch(9);
                        bus.jit_code_pages = Some(pages);
                    }
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..4 {
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs = [0x7fff_ffff, 0x1234, 0x5678, 0x9abc, 0xf000, 0, 0x6ffd, 0];
                        cpu.replace_eflags(0x8d7);
                    }
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut expected_bus, round * 60, 60);
                    let result = engine
                        .run_batch(&mut actual, &mut actual_bus, round * 60, 60)
                        .unwrap();
                    assert_eq!(result, reference, "shape={shape} user={user}");
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                }
                assert!(engine.stats.native_steps > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_ranges_keep_fault_order_and_address_register_dependencies() {
        let programs: &[&[u8]] = &[
            &[
                0x00, 0x17, // ADD [EDI],DL; upper EDX bits must not affect byte flags.
                0x66, 0x11, 0x56, 4, // ADC [ESI+4],DX.
                0x18, 0x56, 7, // SBB [ESI+7],DL.
                0x66, 0x39, 0x56, 8, // CMP [ESI+8],DX.
                0x84, 0x17, // TEST [EDI],DL.
            ],
            &[
                0xbe, 0, 0x61, 0, 0, // MOV ESI,6100h.
                0xb8, 0xff, 0xff, 0, 0, // MOV EAX,ffffh.
                0x0f, 0xb6, 0xcc, // MOVZX ECX,AH: maximum byte index.
                0x8b, 0x1c, 0x4e, // MOV EBX,[ESI+ECX*2].
                0x89, 0x5c, 0x4e, 4, // MOV [ESI+ECX*2+4],EBX.
                0x8b, 0x54, 0x4e, 8, // MOV EDX,[ESI+ECX*2+8].
            ],
            &[
                0xbe, 0, 0x6f, 0, 0, // The byte index can cross into page 7.
                0xb8, 0xff, 0, 0, 0, 0x0f, 0xb6, 0xc8, 0x8b, 0x1c, 0x4e, 0x89, 0x5c, 0x4e, 4,
            ],
            &[
                0xbe, 0, 0xff, 0xff, 0xff, // Possible 32-bit wrap within range.
                0xb8, 0xff, 0, 0, 0, 0x0f, 0xb6, 0xc8, 0x8b, 0x1c, 0x4e, 0x89, 0x5c, 0x4e, 4,
            ],
            &[
                0xbe, 0xf0, 0xff, 0xff, 0xff, 0xb8, 0xff, 0, 0, 0, 0x0f, 0xb6, 0xc8, 0x8d, 0x7c,
                0x4e, 32, // LEA's complete interval wraps to page 0.
                0x8b, 0x1f, 0x89, 0x5f, 4,
            ],
            &[
                0xbe, 0, 0x61, 0, 0, 0x8b, 0x06, 0x66, 0xbe, 0xfc,
                0x7f, // MOV SI,7ffch replaces low half.
                0x8b, 0x1e, 0x8b, 0x56, 4,
            ],
            &[
                0xbe, 0, 0x61, 0, 0, 0x8b, 0x36, // A load replaces the constant.
                0xd1, 0xe6, // SHL ESI,1 is a separate unmodelled write.
                0x8b, 0x1e,
            ],
            &[
                0xbe, 0xff, 0xff, 0xff, 0xff, 0x81, 0xe6, 0xff, 0, 0,
                0, // AND ESI,ffh bounds the value.
                0x8d, 0xbe, 0, 0x60, 0, 0, // LEA EDI,[ESI+6000h].
                0x8b, 0x1f, 0x89, 0x5f, 4,
            ],
            &[
                0xbf, 0xfe, 0x6f, 0, 0, // Constant initial index crosses a page.
                0xb9, 8, 0, 0, 0, 0xb8, 0x5a, 0, 0, 0, 0xfc, 0xf3,
                0xaa, // REP STOSB must check each new page.
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x43, // INC EBX, retaining the checked RAM bias.
                0x89, 0x46, 4, // MOV [ESI+4],EAX.
                0x01, 0x56, 8, // ADD [ESI+8],EDX.
                0x8b, 0x4e, 0xfc, // MOV ECX,[ESI-4].
                0x3b, 0x4e, 12, // CMP ECX,[ESI+12].
            ],
            &[0x8b, 0x06, 0x66, 0x46, 0x8b, 0x1e], // Partial base update.
            &[0x8b, 0x04, 0xbe, 0x47, 0x89, 0x04, 0xbe], // Index update.
            &[0x8b, 0x06, 0x8b, 0x76, 4, 0x8b, 0x16], // Load changes base.
            &[
                0x8b, 0x06, // Two independently checked pages, interleaved.
                0x8b, 0x0f, 0x89, 0x4e, 4, 0x89, 0x47, 4, 0x3b, 0x46, 8,
            ],
            &[0x8b, 0x06, 0x8b, 0x37, 0x8b, 0x1e], // Other page changes the base.
            &[
                0x8b, 0x06, 0xc1, 0xe8, 3, // MOV EAX,[ESI]; SHR EAX,3.
                0x8b, 0x4e, 4, 0xc1, 0xc1, 5, // MOV ECX,[ESI+4]; ROL ECX,5.
                0x03, 0x56, 8, // ADD EDX,[ESI+8].
                0x0f, 0xac, 0xd0, 1, // SHRD EAX,EDX,1.
                0x89, 0x46, 12,
            ],
            &[0x8b, 0x06, 0xd1, 0xe6, 0x8b, 0x1e], // SHL ESI changes the base.
            &[0x8b, 0x06, 0x66, 0xd1, 0xee, 0x8b, 0x1e], // SHR SI partially changes it.
            &[0x8b, 0x06, 0xd3, 0xee, 0x8b, 0x1e], // CL=0 preserves the base.
            &[
                0x50, 0xc1, 0xc2, 3, 0x52, // PUSH EAX; ROL EDX,3; PUSH EDX.
                0x0f, 0xad, 0xc2, // SHRD EDX,EAX,CL; CL=0.
                0x5b, 0x59,
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x50, // PUSH EAX, keeping the ESI range.
                0x8b, 0x4e, 4, // MOV ECX,[ESI+4].
                0x66, 0x51, // PUSH CX, with its own stack commit.
                0x8b, 0x56, 8, // MOV EDX,[ESI+8].
                0x66, 0x5b, // POP BX.
                0xff, 0x36, // PUSH [ESI], a separate read and stack write.
                0x89, 0x46, 12, // MOV [ESI+12],EAX.
                0x59, 0x58, // Restore the original stack pointer.
            ],
            &[0x8b, 0x06, 0x9c, 0x8b, 0x4e, 4, 0x5b, 0x8b, 0x56, 8],
            &[0x8b, 0x06, 0x50, 0x5e, 0x8b, 0x5e, 4], // POP ESI changes the base.
            &[
                0x89, 0xf5, 0x8b, 0x45, 0,    // MOV EBP,ESI; MOV EAX,[EBP].
                0xc9, // LEAVE changes both EBP and ESP.
                0x8b, 0x5d, 4,
            ],
            &[
                0x8b, 0x44, 0x24, 0xf8, // MOV EAX,[ESP-8].
                0x50, // Changes the addressing register.
                0x8b, 0x4c, 0x24, 0xfc, // MOV ECX,[ESP-4].
                0x5b,
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x83, 0xc6, 4, // ADD ESI,4.
                0x89, 0x46, 12, // MOV [ESI+12],EAX.
                0x83, 0xee, 8, // SUB ESI,8.
                0x8b, 0x5e, 4,    // MOV EBX,[ESI+4].
                0x46, // INC ESI.
                0x8d, 0x76, 19, // LEA ESI,[ESI+19].
                0x89, 0x16, // MOV [ESI],EDX.
            ],
            &[
                0x8b, 0x04, 0xbe, // MOV EAX,[ESI+EDI*4].
                0x83, 0xc7, 2, // ADD EDI,2.
                0x89, 0x04, 0xbe, // MOV [ESI+EDI*4],EAX.
                0x4f, // DEC EDI.
                0x8b, 0x1c, 0xbe, // MOV EBX,[ESI+EDI*4].
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x8b, 0x1f, // MOV EBX,[EDI], another page between fields.
                0x89, 0x9e, 0x80, 0, 0, 0, // MOV [ESI+128],EBX.
                0x03, 0x96, 0, 1, 0, 0, // ADD EDX,[ESI+256].
                0x39, 0x06, // CMP [ESI],EAX, after the distant write.
            ],
            &[
                0x8a, 0x06, // MOV AL,[ESI].
                0x66, 0x8b, 0x5e, 3, // MOV BX,[ESI+3].
                0x89, 0x96, 1, 1, 0, 0, // MOV [ESI+257],EDX.
                0x03, 0x86, 1, 1, 0, 0, // ADD EAX,[ESI+257].
                0x66, 0x89, 0x46, 5, // MOV [ESI+5],AX.
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x8b, 0x1f, // MOV EBX,[EDI], interleaved page.
                0x8b, 0x8e, 1, 1, 0, 0, // MOV ECX,[ESI+257].
                0x89, 0x8e, 0xb9, 0x0b, 0, 0, // MOV [ESI+3001],ECX.
                0x39, 0x06, // CMP [ESI],EAX.
            ],
            &[
                0x8b, 0x06, // MOV EAX,[ESI].
                0x83, 0xc8, 0xff, 0x01, 0xd0, // Set CF with -1 + EDX.
                0x89, 0x46, 4, // MOV [ESI+4],EAX preserves that carry.
                0x11, 0xc3, // ADC EBX,EAX consumes it.
                0x8b, 0x4e, 8,    // MOV ECX,[ESI+8].
                0x47, // INC EDI preserves the new carry.
                0x89, 0x5e, 12, // MOV [ESI+12],EBX.
                0x11, 0xca, // ADC EDX,ECX, then overwrite status flags.
                0x31, 0xc0,
            ],
        ];
        for (program, bytes) in programs.iter().enumerate() {
            for shape in 0..6 {
                let address = match shape {
                    0 | 4 => 0x6010,
                    1 => 0x6011,
                    2 | 3 => 0x6ff8,
                    _ => 0xffff_fff8,
                };
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    for (at, word) in [
                        (0x4000, 0x5007),
                        (0x4ffc, 0x6007),
                        (0x5004, 0x3007),
                        (0x5000, 0x9007),
                        (0x5018, 0x8000 | if shape == 4 { 5 } else { 7 }),
                        (0x501c, if shape == 3 { 0 } else { 0x9007 }),
                        (0x6ffc, 0x8007),
                    ] {
                        write_ram_u32(&mut bus, at, word);
                    }
                    for at in (0x8000..0xa000).step_by(4) {
                        write_ram_u32(&mut bus, at, 0x6020);
                    }
                    let mut code = vec![0x45]; // Commit before any guest fault.
                    code.extend_from_slice(bytes);
                    code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                for round in 0..4 {
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs = [0, 0, 0x7fff_ffff, 0, 0, 0, address, 0];
                        cpu.replace_eflags(0x8d7);
                    }
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut expected_bus, round * 41, 41);
                    let native = engine
                        .run_batch(&mut actual, &mut actual_bus, round * 41, 41)
                        .unwrap();
                    assert_eq!(
                        native, reference,
                        "program={program} shape={shape} round={round}"
                    );
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                }
                assert!(engine.stats.native_steps > 0);
                if shape <= 1 {
                    assert!(
                        engine.stats.native_memory_reads + engine.stats.native_memory_writes > 0,
                        "program={program} shape={shape}"
                    );
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cross_page_calls_preserve_return_addresses_and_budget_boundaries() {
        for base in [0, 0x10000] {
            for mismatch in [false, true] {
                for form in 0..3 {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.cr0 |= CR0_PG;
                        cpu.cr3 = 0x1e000;
                        let mut cs = cpu.seg(SegmentReg::Cs);
                        cs.base = base;
                        cpu.set_seg_cache(SegmentReg::Cs, cs);
                        cpu.regs[4] = 0x9000;
                        cpu.regs[6] = if form == 1 { 0x2000 } else { 0x8000 };
                        cpu.replace_eflags(0x8d7);
                        let mut bus = HostBus::new(0x20000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        write_ram_u32(&mut bus, 0x1e000, 0x1f027);
                        for page in 0..32 {
                            write_ram_u32(
                                &mut bus,
                                0x1f000 + page * 4,
                                ((page as u32) << 12) | 0x27,
                            );
                        }
                        write_ram_u32(&mut bus, 0x8000, 0x2000);
                        // After a mapping change, the first NOP refills this
                        // execute page before the native CALL rechecks its target.
                        let mut caller = match form {
                            0 => vec![0x90, 0xe8, 0xfa, 0x0f, 0, 0], // CALL 2000.
                            1 => vec![0x90, 0xff, 0xd6],             // CALL ESI.
                            _ => vec![0x90, 0xff, 0x16],             // CALL [ESI].
                        };
                        caller.extend([0x40, 0x43]); // INC EAX; INC EBX.
                        caller.extend([0xeb, (-(caller.len() as i8) - 2) as u8]);
                        let callee: &[u8] = if mismatch {
                            &[0x83, 0x04, 0x24, 1, 0x41, 0xc3] // Skip INC EAX.
                        } else {
                            &[0x41, 0xc3]
                        };
                        let at = base as usize + 0x1000;
                        bus.ram[at..at + caller.len()].copy_from_slice(&caller);
                        for target in [0x2000, 0x3000] {
                            let at = base as usize + target;
                            bus.ram[at..at + callee.len()].copy_from_slice(callee);
                        }
                        (cpu, bus)
                    };
                    let (mut expected, mut expected_bus) = make();
                    let (mut actual, mut actual_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    let mut steps = 0;
                    for budget in [1, 2, 3, 7, 127, 10_001] {
                        if budget == 127 {
                            // Reuse one indirect call site with a new destination.
                            if form == 1 {
                                expected.regs[6] = 0x3000;
                                actual.regs[6] = 0x3000;
                            } else if form == 2 {
                                write_ram_u32(&mut expected_bus, 0x8000, 0x3000);
                                write_ram_u32(&mut actual_bus, 0x8000, 0x3000);
                            }
                        } else if budget == 10_001 {
                            // Enter the caller first so its next CALL must recheck
                            // the warm callee after an execute-context change.
                            for cpu in [&mut expected, &mut actual] {
                                cpu.eip = 0x1000;
                                cpu.regs[4] = 0x9000;
                                cpu.invalidate_non_global_translations();
                            }
                        }
                        let reference =
                            Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                        let native = engine
                            .run_batch(&mut actual, &mut actual_bus, steps, budget)
                            .unwrap();
                        assert_eq!(
                            native, reference,
                            "base={base:x} mismatch={mismatch} form={form}"
                        );
                        assert_eq!(actual.regs, expected.regs);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.tsc(), expected.tsc());
                        assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                        assert_eq!(actual_bus.ram, expected_bus.ram);
                        steps += budget;
                    }
                    assert!(engine.stats.native_steps > 9000);
                    assert!(
                        engine.stats.rechecked_code > 0,
                        "base={base:x} mismatch={mismatch} form={form} {:?}",
                        engine.stats
                    );
                    if mismatch {
                        assert_eq!(actual.regs[0], 0);
                        assert_eq!(engine.stats.cached_returns, 0);
                    } else {
                        assert!(engine.stats.cached_returns > 1000);
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_return_cache_handles_deep_recursion_and_modified_return_addresses() {
        for mismatch in [false, true] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs[4] = 0x9000;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                let code: &[u8] = if mismatch {
                    &[
                        0xe8, 4, 0, 0, 0, // CALL function.
                        0x40, 0x43, 0xeb, 0xf7, // INC EAX; INC EBX; loop.
                        0x83, 0x04, 0x24, 1,
                        0xc3, // Skip INC EAX by editing the return address.
                    ]
                } else {
                    &[
                        0xb9, 0x2c, 1, 0, 0, // ECX=300, deeper than the cache.
                        0xe8, 3, 0, 0, 0, // CALL function.
                        0x40, 0xeb, 0xf3, // INC EAX; loop.
                        0x49, 0x74, 5, // DEC ECX; JZ return.
                        0xe8, 0xf8, 0xff, 0xff, 0xff, // Recursive call.
                        0xc3,
                    ]
                };
                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(code);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            let mut start = 0;
            for budget in [11, 501, 40_001] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, start, budget);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, start, budget)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                start += budget;
            }
            assert!(engine.stats.native_steps > 39_000);
            if mismatch {
                assert_eq!(actual.regs[0], 0);
                assert!(actual.regs[3] > 1000);
                assert_eq!(engine.stats.cached_returns, 0);
            } else {
                assert!(engine.stats.cached_returns > 1000);
                assert!(engine.stats.call_overflows > 0);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_returns_to_zero_without_a_native_call() {
        for code in [&[0x6a, 0, 0xc3][..], &[0x66, 0x6a, 0, 0x66, 0xc3][..]] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs[4] = 0x9000;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                // PUSH 0; RET has no preceding CALL. EIP zero is ordinary code:
                // INC EAX; JMP 1000h. Both stack operand widths must work.
                bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(code);
                bus.ram[..6].copy_from_slice(&[0x40, 0xe9, 0xfa, 0x0f, 0, 0]);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            let mut start = 0;
            for budget in [1, 2, 7, 10_001] {
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, start, budget);
                let native = engine
                    .run_batch(&mut actual, &mut actual_bus, start, budget)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual_bus.ram, expected_bus.ram);
                start += budget;
            }
            assert!(actual.regs[0] > 2000);
            assert!(engine.stats.native_steps > 9000);
            assert_eq!(engine.stats.cached_returns, 0);
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_bit_strings_preserve_signed_offsets_splits_and_write_faults() {
        for bits in [16u8, 32] {
            for (kind, opcode) in [0xa3, 0xab, 0xb3, 0xbb].into_iter().enumerate() {
                for shape in 0..5 {
                    for (immediate, index) in [
                        (false, u32::MAX),
                        (false, 0xffff_8000),
                        (false, 65),
                        (true, 255),
                    ] {
                        let target: u32 = if (2..=3).contains(&shape) {
                            0x7001 - u32::from(bits / 8)
                        } else {
                            0x6000 + shape.min(1)
                        };
                        let signed = if bits == 16 {
                            index as i16 as i32
                        } else {
                            index as i32
                        };
                        let displacement = if immediate {
                            0
                        } else {
                            (signed >> bits.trailing_zeros()) * i32::from(bits / 8)
                        };
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                            cpu.cr3 = 0x4000;
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            for (addr, value) in [
                                (0x4000, 0x5007),
                                (0x5004, 0x3007),
                                (0x5018, 0x8000 | if shape == 4 { 5 } else { 7 }),
                            ] {
                                write_ram_u32(&mut bus, addr, value);
                            }
                            if shape != 3 {
                                write_ram_u32(&mut bus, 0x501c, 0x9007);
                            }
                            let mut code = vec![0x47];
                            if kind != 0 {
                                code.push(0xf0);
                            }
                            if bits == 16 {
                                code.push(0x66);
                            }
                            code.extend([
                                0x0f,
                                if immediate { 0xba } else { opcode },
                                if immediate {
                                    ((4 + kind as u8) << 3) | 3
                                } else {
                                    0x0b
                                },
                            ]);
                            if immediate {
                                code.push(index as u8);
                            }
                            code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                            bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut expected_bus) = make();
                        let (mut actual, mut actual_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        for round in 0..4 {
                            for (cpu, bus) in [
                                (&mut expected, &mut expected_bus),
                                (&mut actual, &mut actual_bus),
                            ] {
                                cpu.eip = 0x1000;
                                cpu.regs = [
                                    0xdead_beef,
                                    index,
                                    0,
                                    target.wrapping_sub(displacement as u32),
                                    0,
                                    0,
                                    0,
                                    0x7fff_ffff,
                                ];
                                cpu.replace_eflags(0x8d7);
                                write_ram_u32(bus, target as usize + 0x2000, 0xfedc_ba98);
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                round * 3,
                                3,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut actual_bus, round * 3, 3)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "bits={bits} kind={kind} shape={shape} imm={immediate} index={index:x}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                        }
                        assert!(engine.stats.native_steps > 0);
                        if shape <= 1 {
                            assert!(engine.stats.native_memory_reads > 0);
                            if kind != 0 {
                                assert!(engine.stats.native_memory_writes > 0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bit_strings_use_signed_register_offsets_and_mask_immediate_indices() {
        for bits in [16u8, 32] {
            for address16 in [false, true] {
                for (index, delta, bit) in if bits == 16 {
                    [
                        (0xa5a5_ffff, -2, 15),
                        (0xa5a5_fff0, -2, 0),
                        (0xa5a5_0010, 2, 0),
                        (0, 0, 0),
                    ]
                } else {
                    [
                        (u32::MAX, -4, 31),
                        (0xffff_ffdf, -8, 31),
                        (32, 4, 0),
                        (0, 0, 0),
                    ]
                } {
                    for immediate in [false, true] {
                        for (kind, opcode) in [0xa3, 0xab, 0xb3, 0xbb].into_iter().enumerate() {
                            let base: u32 = if address16 { 2 } else { 0x8000 };
                            let target = if immediate {
                                base
                            } else {
                                base.wrapping_add_signed(delta)
                                    & if address16 { 0xffff } else { u32::MAX }
                            };
                            let mask = 1u32 << if immediate { bits - 1 } else { bit };
                            let mut bus = HostBus::new(0x20000);
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.regs[1] = index;
                            cpu.replace_eflags(0x8d6);
                            let mut code = Vec::new();
                            if bits == 16 {
                                code.push(0x66);
                            }
                            if address16 {
                                code.push(0x67);
                            }
                            code.extend([
                                0x0f,
                                if immediate { 0xba } else { opcode },
                                (if immediate {
                                    (4 + kind as u8) << 3
                                } else {
                                    1 << 3
                                }) | if address16 { 6 } else { 5 },
                            ]);
                            code.extend_from_slice(
                                &base.to_le_bytes()[..if address16 { 2 } else { 4 }],
                            );
                            if immediate {
                                code.push(255);
                            }
                            bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                            let old = 0x5a5a_0000 | mask;
                            write_ram_u32(&mut bus, target as usize, old);
                            Interpreter::step(&mut cpu, &mut bus).unwrap();
                            let actual = u32::from_le_bytes(
                                bus.ram[target as usize..target as usize + 4]
                                    .try_into()
                                    .unwrap(),
                            );
                            assert_eq!(
                                actual,
                                if kind >= 2 { old & !mask } else { old },
                                "bits={bits} a16={address16} index={index:x} imm={immediate} kind={kind}"
                            );
                            assert_eq!(cpu.effective_eflags() & 0x8d5, 0x8d5);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bit_modify_write_fault_keeps_flags_and_memory() {
        for bits in [16, 32] {
            for opcode in [0xab, 0xb3, 0xbb] {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                cpu.regs[1] = 0;
                cpu.replace_eflags(0x8d6); // CF=0 while the old memory bit is one.
                let mut bus = HostBus::new(0x10000);
                for (addr, value) in [
                    (0x4000, 0x5007),
                    (0x5004, 0x3007),
                    (0x5018, 0x8005),
                    (0x8000, 1),
                ] {
                    write_ram_u32(&mut bus, addr, value);
                }
                let mut code = Vec::new();
                if bits == 16 {
                    code.push(0x66);
                }
                code.extend([0x0f, opcode, 0x0d, 0, 0x60, 0, 0]);
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                let fault = Interpreter::step(&mut cpu, &mut bus)
                    .unwrap_err()
                    .page_fault()
                    .unwrap();
                assert_eq!(fault.code, 3);
                assert_eq!(fault.linear.0, 0x6000);
                assert_eq!(cpu.effective_eflags() & 0x8d5, 0x8d4);
                assert_eq!(&bus.ram[0x8000..0x8004], &1u32.to_le_bytes());
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_cmpxchg8b_preserves_other_flags_aliases_and_write_faults() {
        for prefixes in [&[][..], &[0x66][..], &[0xf0][..], &[0xf0, 0x66][..]] {
            for shape in 0..5 {
                for equal in [false, true] {
                    for base in [0, 6] {
                        let address: u32 = if (2..=3).contains(&shape) {
                            0x6ffd
                        } else {
                            0x6000 + shape.min(1)
                        };
                        let make = || {
                            let mut cpu = Cpu::new();
                            cpu.enter_protected_flat(0x1000, 0);
                            cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                            cpu.cr3 = 0x4000;
                            let mut bus = HostBus::new(0x10000);
                            bus.attention = false;
                            bus.next_attention_step = u64::MAX;
                            write_ram_u32(&mut bus, 0x4000, 0x5007);
                            write_ram_u32(&mut bus, 0x5004, 0x3007);
                            write_ram_u32(
                                &mut bus,
                                0x5018,
                                0x8000 | if shape == 4 { 5 } else { 7 },
                            );
                            if shape != 3 {
                                write_ram_u32(&mut bus, 0x501c, 0x9007);
                            }
                            let mut code = vec![0x47]; // INC EDI, flags live through CMPXCHG8B.
                            code.extend_from_slice(prefixes);
                            code.extend([0x0f, 0xc7, 0x08 | base]);
                            code.extend([0xeb, (-(code.len() as i8) - 2) as u8]);
                            bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                            (cpu, bus)
                        };
                        let (mut expected, mut expected_bus) = make();
                        let (mut actual, mut actual_bus) = make();
                        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                        for round in 0..4 {
                            for (cpu, bus) in [
                                (&mut expected, &mut expected_bus),
                                (&mut actual, &mut actual_bus),
                            ] {
                                cpu.eip = 0x1000;
                                cpu.regs = [
                                    address,
                                    0x0123_4567,
                                    0x89ab_cdef,
                                    0xfedc_ba98,
                                    0,
                                    0,
                                    address,
                                    if round & 1 == 0 {
                                        0x7fff_ffff
                                    } else {
                                        0xffff_ffff
                                    },
                                ];
                                cpu.replace_eflags(0x8d7);
                                write_ram_u32(
                                    bus,
                                    address as usize + 0x2000,
                                    address ^ u32::from(!equal),
                                );
                                write_ram_u32(bus, address as usize + 0x2004, 0x89ab_cdef);
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                round * 3,
                                3,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut actual_bus, round * 3, 3)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "prefix={prefixes:x?} shape={shape} equal={equal} base={base}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                        }
                        assert!(engine.stats.native_steps > 0);
                        if shape <= 1 {
                            assert!(engine.stats.native_memory_writes > 0);
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_retained_kernel_tokens_still_fault_in_user_mode() {
        for (write, user_code) in [(false, true), (true, true), (false, false)] {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                cpu.regs[0] = 0x1234;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | if user_code { 7 } else { 3 });
                write_ram_u32(&mut bus, 0x5018, 0x8000 | 3); // kernel data
                let code = [
                    0x43,
                    if write { 0xa3 } else { 0xa1 },
                    0,
                    0x60,
                    0,
                    0,
                    0xeb,
                    0xf8,
                ];
                bus.ram[0x3000..0x3008].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut reference_bus) = make();
            let (mut actual, mut native_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            let mut steps = 0;
            for (phase, cpl) in [0, 0, 3, 3, 0, 3, 0].into_iter().enumerate() {
                if phase == 4 {
                    // Retire code while both privilege tables can retain links.
                    reference_bus.write_phys_u8(PhysAddr(0x3000), 0x4b).unwrap();
                    native_bus.write_phys_u8(PhysAddr(0x3000), 0x4b).unwrap();
                }
                for cpu in [&mut expected, &mut actual] {
                    cpu.set_seg_cache(
                        SegmentReg::Cs,
                        SegmentCache::flat(8 | cpl, if cpl == 3 { 0xc0fb } else { 0xc09b }),
                    );
                    cpu.set_seg_cache(
                        SegmentReg::Ds,
                        SegmentCache::flat(16 | cpl, if cpl == 3 { 0xc0f3 } else { 0xc093 }),
                    );
                    cpu.eip = 0x1000;
                }
                let before_data = native_bus.ram[0x8000..0x8004].to_vec();
                let reference =
                    Interpreter::run_batch(&mut expected, &mut reference_bus, steps, 60);
                let native = engine
                    .run_batch(&mut actual, &mut native_bus, steps, 60)
                    .unwrap();
                assert_eq!(native, reference);
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(native_bus.ram, reference_bus.ram);
                if cpl == 3 {
                    assert_eq!(native.ran, if user_code { 2 } else { 1 });
                    assert_eq!(actual.cr2, if user_code { 0x6000 } else { 0x1000 });
                    assert_eq!(&native_bus.ram[0x8000..0x8004], &before_data);
                    assert!(!matches!(native.stop, BatchStop::Budget));
                } else {
                    assert_eq!(native.ran, 60);
                    assert_eq!(native.stop, BatchStop::Budget);
                }
                steps += native.ran;
            }
            assert!(engine.stats.native_steps > 150);
            if user_code {
                assert!(engine.stats.memory_exits >= 3);
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_memory_exits_preserve_flags_before_later_flag_writes() {
        let writers: &[&[u8]] = &[
            &[0x40],                   // INC EAX, retaining incoming CF.
            &[0x48],                   // DEC EAX.
            &[0x44],                   // INC ESP before a faultable PUSH.
            &[0x83, 0xc4, 0x10],       // ADD ESP,16 before a PUSH changes ESP.
            &[0x83, 0xc0, 0x11],       // ADD EAX,17.
            &[0x83, 0xe8, 0xff],       // SUB EAX,-1.
            &[0x04, 0x11],             // ADD AL,17.
            &[0x80, 0xec, 0x11],       // SUB AH,17.
            &[0x66, 0x83, 0xc0, 0xff], // ADD AX,-1.
            &[0x31, 0xc0],             // XOR EAX,EAX.
            &[0x83, 0xe0, 0xf0],       // AND EAX,-16.
            &[0x09, 0xd8],             // OR EAX,EBX.
            &[0x39, 0xd8],             // CMP EAX,EBX.
            &[0x84, 0xdc],             // TEST AH,BL.
        ];
        for &writer in writers {
            for mode in 0..7 {
                let make = || {
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                    cpu.cr3 = 0x4000;
                    let mut bus = HostBus::new(0x10000);
                    bus.attention = false;
                    bus.next_attention_step = u64::MAX;
                    write_ram_u32(&mut bus, 0x4000, 0x5027);
                    write_ram_u32(&mut bus, 0x5004, 0x3027);
                    write_ram_u32(&mut bus, 0x5028, if mode == 6 { 0xa025 } else { 0xa067 });
                    if mode != 1 {
                        write_ram_u32(&mut bus, 0x5018, 0x8027);
                    }
                    write_ram_u32(
                        &mut bus,
                        0x501c,
                        if matches!(mode, 2 | 4..=6) {
                            0x9025
                        } else {
                            0x9067
                        },
                    );
                    bus.ram[0x8000..0xa000].fill(0xa5);
                    let mut code = writer.to_vec();
                    code.extend([0x8b, 0x16]); // MOV EDX,[ESI].
                    // Changing a source register before a later memory fault
                    // must preserve the earlier arithmetic instruction's flags.
                    if mode == 4 {
                        code.extend([0x89, 0xd0]); // MOV EAX,EDX.
                    }
                    if mode >= 5 {
                        code.push(0x52); // PUSH EDX: fault here or after ESP changes.
                    }
                    code.extend([0x89, 0x17]); // MOV [EDI],EDX.
                    code.extend([0x31, 0xdb, 0xf4]); // XOR EBX,EBX; HLT.
                    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                    (cpu, bus)
                };
                let (mut expected, mut expected_bus) = make();
                let (mut actual, mut actual_bus) = make();
                let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                let mut steps = 0;
                for value in [
                    0,
                    1,
                    0xf,
                    0x10,
                    0x7f,
                    0x80,
                    0xff,
                    0x7fff,
                    0x8000,
                    0x7fff_ffff,
                    0x8000_0000,
                    u32::MAX,
                ] {
                    for cpu in [&mut expected, &mut actual] {
                        cpu.eip = 0x1000;
                        cpu.regs = [
                            value,
                            7,
                            0x1234,
                            !value,
                            0xa100,
                            0,
                            if mode == 3 { 0x6ffe } else { 0x6000 },
                            0x7000,
                        ];
                        cpu.replace_eflags(0x202 | (value & 0x8d5));
                    }
                    let budget = if mode >= 4 { 5 } else { 4 };
                    let reference =
                        Interpreter::run_batch(&mut expected, &mut expected_bus, steps, budget);
                    let native = engine
                        .run_batch(&mut actual, &mut actual_bus, steps, budget)
                        .unwrap();
                    assert_eq!(
                        native, reference,
                        "writer={writer:x?} mode={mode} value={value:x}"
                    );
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(
                        actual.effective_eflags(),
                        expected.effective_eflags(),
                        "writer={writer:x?} mode={mode} value={value:x}"
                    );
                    assert_eq!(actual.tsc(), expected.tsc());
                    assert_eq!(actual.cr2, expected.cr2);
                    assert_eq!(actual_bus.ram, expected_bus.ram);
                    steps += native.ran;
                }
                assert!(engine.stats.native_steps > 5);
                if matches!(mode, 1..=3) {
                    assert!(engine.stats.memory_exits > 0);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_forward_comparisons_preserve_fault_flags_and_skipped_memory_effects() {
        for opcode in [0x39, 0x85] {
            for condition in 0..16 {
                for mode in 0..3 {
                    let make = || {
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0x1000, 0);
                        cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                        cpu.cr3 = 0x4000;
                        let mut bus = HostBus::new(0x10000);
                        bus.attention = false;
                        bus.next_attention_step = u64::MAX;
                        write_ram_u32(&mut bus, 0x4000, 0x5027);
                        write_ram_u32(&mut bus, 0x5004, 0x3027);
                        if mode != 0 {
                            write_ram_u32(&mut bus, 0x5018, 0x8067);
                        }
                        write_ram_u32(&mut bus, 0x501c, if mode == 1 { 0x9025 } else { 0x9067 });
                        write_ram_u32(&mut bus, 0x8000, 0x1234abcd);
                        let code = [
                            opcode,
                            0xd8, // CMP/TEST EAX,EBX.
                            0x70 | condition,
                            6, // Taken: skip both accesses and XOR.
                            0x8b,
                            0x16, // MOV EDX,[ESI].
                            0x89,
                            0x17, // MOV [EDI],EDX.
                            0x31,
                            0xc9, // XOR ECX,ECX.
                            0xf4,
                        ];
                        bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                        (cpu, bus)
                    };
                    let (mut expected, mut expected_bus) = make();
                    let (mut actual, mut actual_bus) = make();
                    let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
                    let mut steps = 0;
                    for value in [0, 1, 0x7fff_ffff, 0x8000_0000, u32::MAX] {
                        for budget in [2, 3, 5, 6] {
                            for cpu in [&mut expected, &mut actual] {
                                cpu.eip = 0x1000;
                                cpu.halted = false;
                                cpu.regs =
                                    [value, 7, 0xa5a5, 0xa5a5a5a5, 0xa100, 0, 0x6000, 0x7000];
                                cpu.replace_eflags(0x202 | (value & 0x8d5));
                            }
                            let reference = Interpreter::run_batch(
                                &mut expected,
                                &mut expected_bus,
                                steps,
                                budget,
                            );
                            let native = engine
                                .run_batch(&mut actual, &mut actual_bus, steps, budget)
                                .unwrap();
                            assert_eq!(
                                native, reference,
                                "opcode={opcode:x} condition={condition} mode={mode} value={value:x} budget={budget}"
                            );
                            assert_eq!(actual.regs, expected.regs);
                            assert_eq!(actual.eip, expected.eip);
                            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                            assert_eq!(actual.tsc(), expected.tsc());
                            assert_eq!(actual.cr2, expected.cr2);
                            assert_eq!(actual_bus.ram, expected_bus.ram);
                            steps += native.ran;
                        }
                    }
                    assert!(engine.stats.native_steps > 0);
                }
            }
        }
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_write_faults_preserve_flags_stack_and_uncommitted_bytes() {
        for operation in 0..10 {
            let make = || {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.regs[0] = 0xdead_beef;
                cpu.regs[1] = 1;
                cpu.regs[3] = 0x7fff_ffff;
                cpu.regs[4] = 0x6002;
                cpu.replace_eflags(3);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                let mut bus = HostBus::new(0x10000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                write_ram_u32(&mut bus, 0x4000, 0x5000 | 7);
                write_ram_u32(&mut bus, 0x5004, 0x3000 | 7);
                if !matches!(operation, 1..=3) {
                    write_ram_u32(&mut bus, 0x5018, 0x8000 | 5); // present, user, read-only
                } else {
                    write_ram_u32(&mut bus, 0x5014, 0x8000 | 7); // writable first half of split store
                }
                bus.ram[0x8000..0x9000].fill(0xa5);
                let mut code = vec![0x90, 0x43]; // nop; inc ebx
                match operation {
                    0 => code.extend([0x01, 0x05, 0, 0x60, 0, 0]), // add [6000h],eax
                    1 => code.extend([0xa3, 0xfe, 0x5f, 0, 0]),    // mov [5ffeh],eax
                    2 => code.push(0x50),                          // push eax at 5ffeh
                    3 => code.extend([0xe8, 0, 0, 0, 0]),          // call with the same split stack
                    4 => code.extend([0x11, 0x05, 0, 0x60, 0, 0]), // adc [6000h],eax
                    5 => code.extend([0x19, 0x05, 0, 0x60, 0, 0]), // sbb [6000h],eax
                    6 => code.extend([0xd3, 0x2d, 0, 0x60, 0, 0]), // shr [6000h],cl
                    7 => code.extend([0xd3, 0x25, 0, 0x60, 0, 0]), // shl [6000h],cl
                    8 => code.extend([0xd3, 0x3d, 0, 0x60, 0, 0]), // sar [6000h],cl
                    9 => code.extend([0x0f, 0x92, 0x05, 0, 0x60, 0, 0]), // setb [6000h]
                    _ => unreachable!(),
                }
                code.extend([0x31, 0xdb]); // future XOR must not suppress prior flags
                bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(&code);
                (cpu, bus)
            };
            let (mut expected, mut expected_bus) = make();
            let (mut actual, mut actual_bus) = make();
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..3 {
                expected.eip = 0x1000;
                actual.eip = 0x1000;
                let reference =
                    Interpreter::run_batch(&mut expected, &mut expected_bus, round * 3, 50);
                let result = engine
                    .run_batch(&mut actual, &mut actual_bus, round * 3, 50)
                    .unwrap();
                assert_eq!(result, reference, "operation={operation}");
                assert_eq!(result.ran, 3);
                assert_eq!(actual.effective_eflags(), expected.effective_eflags());
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.regs[4], 0x6002);
                assert_eq!(actual.tsc(), expected.tsc());
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(actual.cr2, 0x6000);
                assert_eq!(actual_bus.ram, expected_bus.ram);
                assert!(
                    actual_bus.ram[0x8000..0x9000]
                        .iter()
                        .all(|&byte| byte == 0xa5)
                );
            }
            assert!(engine.stats.native_steps > 0);
            assert!(engine.stats.memory_exits > 0);
        }
    }

    fn write_ram_u32(bus: &mut HostBus, addr: usize, value: u32) {
        bus.ram[addr..addr + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn paging_cpu(cr3: u32) -> Cpu {
        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = cr3;
        cpu
    }

    #[test]
    fn absent_ports_float_at_all_guest_io_widths() {
        for port in [0x134u32, 0x56, 0x1234, 0xfe00] {
            let mut m = Machine::new(0x4000);
            m.cpu.enter_protected_flat(0x1000, 0);
            let mut code = vec![0xba]; // MOV EDX,port
            code.extend_from_slice(&port.to_le_bytes());
            code.extend_from_slice(&[
                0xec, 0xa2, 0x00, 0x20, 0, 0, // IN AL; save byte
                0x66, 0xed, 0x66, 0xa3, 0x02, 0x20, 0, 0, // IN AX; save word
                0xed, 0xa3, 0x04, 0x20, 0, 0, // IN EAX; save dword
                0x31, 0xc0, 0xee, 0x66, 0xef, 0xef, // zero OUT at all widths
                0xed, 0xa3, 0x08, 0x20, 0, 0, // writes must not latch
                0xf4,
            ]);
            m.bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
            m.run_steps(100).unwrap();
            assert!(m.cpu.halted);
            assert_eq!(
                &m.ram()[0x2000..0x200c],
                &[255, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]
            );
            assert!(m.serial_output().is_empty());
        }
    }

    #[test]
    fn absent_memory_preserves_ram_boundaries_and_guest_page_permissions() {
        use crate::x86::mem;
        let mut bus = HostBus::new(0x4000);
        // A store crossing installed RAM still commits its backed bytes.
        mem::write_phys_u32(&mut bus, PhysAddr(0x3ffe), 0x12345678).unwrap();
        assert_eq!(&bus.ram[0x3ffe..], &[0x78, 0x56]);
        assert_eq!(bus.read_phys_u32(PhysAddr(0x3ffe)).unwrap(), 0xffff5678);
        assert_eq!(bus.read_phys_u64(PhysAddr(0x8000)).unwrap(), u64::MAX);
        assert_eq!(bus.read_phys_u8(PhysAddr(1 << 32)), Ok(0xff));
        assert!(
            bus.diagnostics
                .pages
                .contains_key(&(BusSpace::Mmio, 1 << 32))
        );
        assert!(bus.read_phys_u8(PhysAddr(PC_PHYS_LIMIT)).is_err());
        // Host loaders must still catch incorrect image bounds.
        assert!(
            bus.before_phys_write(PhysWrite::new(WriteOrigin::Loader, PhysAddr(0x4000), 1))
                .is_err()
        );

        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0x1000, 0);
        cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
        cpu.cr3 = 0x2000;
        write_ram_u32(&mut bus, 0x2000, 0x3003);
        write_ram_u32(&mut bus, 0x3010, 0xfee00003);
        let address = LinearAddr(0x4000);
        assert_eq!(
            mmu::read_linear_u32(&mut cpu, &mut bus, address, AccessKind::Read).unwrap(),
            u32::MAX
        );
        mmu::write_linear_u32(&mut cpu, &mut bus, address, 0).unwrap();
        let accesses = bus.diagnostics.pages[&(BusSpace::Mmio, 0xfee00000)].count;
        write_ram_u32(&mut bus, 0x3010, 0xfee00001); // present, read-only
        cpu.invalidate_translation_context();
        assert_eq!(
            mmu::write_linear_u32(&mut cpu, &mut bus, address, 0),
            Err(mmu::Fault::Page(mmu::PageFault {
                linear: address,
                code: 3
            }))
        );
        write_ram_u32(&mut bus, 0x3010, 0); // not present
        cpu.invalidate_translation_context();
        assert_eq!(
            mmu::read_linear_u32(&mut cpu, &mut bus, address, AccessKind::Read),
            Err(mmu::Fault::Page(mmu::PageFault {
                linear: address,
                code: 0
            }))
        );
        assert_eq!(
            bus.diagnostics.pages[&(BusSpace::Mmio, 0xfee00000)].count,
            accesses
        );
    }

    #[test]
    fn bus_log_limit_is_per_accessed_page_and_survives_reset() {
        let mut m = Machine::new(0x1000);
        for count in 1..=1005 {
            // Neither instruction page reaches 1000; the target alone mutes.
            m.bus
                .instruction_address(if count <= 500 { 0x8000 } else { 0x9000 });
            // Different addresses and access directions share the page budget.
            if count % 2 == 0 {
                m.bus.io_write_u8(0x1ffe, 0).unwrap();
            } else {
                assert_eq!(m.bus.io_read_u8(0x1234).unwrap(), 0xff);
            }
            if count <= 1000 {
                assert_eq!(m.take_bus_warning().unwrap().count, count);
            }
            assert!(m.take_bus_warning().is_none());
        }
        m.bus.reset(50);
        m.bus.instruction_address(0xa000);
        m.bus.io_read_u8(0x1fff).unwrap();
        assert!(m.take_bus_warning().is_none());
        assert_eq!(m.bus_warning_pages().next().unwrap().count, 1006);
        // Neither a different port page nor the same numbered physical page
        // inherits the muted I/O page's count.
        m.bus.io_read_u8(0x2000).unwrap();
        m.bus.read_phys_u8(PhysAddr(0x1234)).unwrap();
        assert_eq!(m.take_bus_warning().unwrap().count, 1);
        let memory = m.take_bus_warning().unwrap();
        assert_eq!((memory.space, memory.count), (BusSpace::Mmio, 1));
        for _ in 0..2000 {
            m.bus.read_phys_u8(PhysAddr(0x1234)).unwrap();
        }
        assert_eq!(core::iter::from_fn(|| m.take_bus_warning()).count(), 256);
        assert_eq!(
            m.bus_warning_pages()
                .find(|p| p.space == BusSpace::Mmio)
                .unwrap()
                .count,
            2001
        );
        // Origin diagnostics can also mute a fresh target after that source's
        // MMIO traffic. Without them, each new target keeps its own log budget.
        m.bus.io_read_u8(0x3000).unwrap();
        #[cfg(feature = "bus-instruction-context")]
        {
            assert!(m.take_bus_warning().is_none());
            let fresh = &m.bus.diagnostics.pages[&(BusSpace::Io, 0x3000)];
            assert_eq!((fresh.count, fresh.logged), (1, 0));
        }
        #[cfg(not(feature = "bus-instruction-context"))]
        {
            let fresh = m.take_bus_warning().unwrap();
            assert_eq!((fresh.count, fresh.logged), (1, 1));
            assert_eq!(fresh.instruction, None);
            assert_eq!(fresh.instruction_count, 0);
            assert!(m.bus_warning_instruction_pages().next().is_none());
        }
    }

    #[test]
    fn bus_warning_instruction_context_follows_build_setting() {
        let mut m = Machine::new(0x4000);
        m.cpu.enter_protected_flat(0xfff, 0);
        m.cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache::new(8, 0x1000, u32::MAX, 0xc09b),
        );
        m.cpu.regs[2] = 0x1234;
        // Operand-size prefix is on the previous page from the IN opcode.
        m.bus.ram[0x1fff..0x2002].copy_from_slice(&[0x66, 0xed, 0xf4]);
        m.run_steps(10).unwrap();
        assert!(m.cpu.halted);
        assert_eq!(m.cpu.regs[0], 0xffff);
        let warning = m.take_bus_warning().unwrap();
        #[cfg(feature = "bus-instruction-context")]
        {
            assert_eq!(warning.instruction, Some(0x1fff));
            let (page, counts) = m.bus_warning_instruction_pages().next().unwrap();
            assert_eq!((*page, counts.count), (0x1000, 2));
        }
        #[cfg(not(feature = "bus-instruction-context"))]
        {
            assert_eq!(warning.instruction, None);
            assert_eq!(warning.instruction_count, 0);
            assert!(m.bus_warning_instruction_pages().next().is_none());
        }
    }

    #[test]
    fn host_bus_classifies_guest_ram_and_holes() {
        let mut bus = HostBus::new(0x2000);

        assert_eq!(bus.classify_phys(PhysAddr(0x100), 4), PhysRegionKind::Ram);
        assert_eq!(
            bus.classify_phys(PhysAddr(0x1fff), 2),
            PhysRegionKind::Mixed
        );
        assert_eq!(bus.read_phys_u8(PhysAddr(0x2000)), Ok(0xff));
    }

    #[test]
    fn firmware_regions_keep_rom_and_vga_out_of_ram_classification() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.load_bios_rom(&[0xea, 0x5b, 0xe0, 0x00, 0xf0]).unwrap();
        bus.load_option_rom(0xc0000, &[0x55, 0xaa, 1]).unwrap();

        assert_eq!(bus.classify_phys(PhysAddr(0x7c00), 1), PhysRegionKind::Ram);
        assert_eq!(
            bus.classify_phys(PhysAddr(0xb8000), 1),
            PhysRegionKind::Mmio
        );
        assert_eq!(
            bus.classify_phys(PhysAddr(0xe000_0000), 1),
            PhysRegionKind::FbDirtyTracked
        );
        assert_eq!(bus.classify_phys(PhysAddr(0xc0000), 1), PhysRegionKind::Rom);
        assert_eq!(
            bus.classify_phys(PhysAddr(0xffdff124), 4),
            PhysRegionKind::Rom
        );
        assert_eq!(bus.read_phys_u8(PhysAddr(0xffffb)).unwrap(), 0xea);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xffdffffb)).unwrap(), 0xea);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xc0000)).unwrap(), 0x55);
        let option_rom_prefix = bus.read_phys_u32(PhysAddr(0xc0000)).unwrap();
        bus.write_phys_u32(PhysAddr(0xc0000), 0x1234_5678).unwrap();
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xc0000)).unwrap(),
            option_rom_prefix
        );

        bus.before_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0xb8000), 1))
            .unwrap();
        assert_eq!(bus.read_phys_u8(PhysAddr(0xa0000)).unwrap(), 0xff);
        bus.write_phys_u8(PhysAddr(0xa0000), 0x5a).unwrap();
        bus.write_phys_u8(PhysAddr(0xb8000), b'V').unwrap();
        assert_eq!(bus.vga.text_byte(0), Some(b'V'));
    }

    #[test]
    fn firmware_pam_pci_lanes_widths_and_a20() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.load_bios_rom(&vec![0x5a; 0x40000]).unwrap();
        bus.ram[0xc0000..0x100000].fill(0xa5);
        bus.io_write_u32(0xcf8, 0x80000058).unwrap();
        bus.io_write_u32(0xcfc, u32::MAX).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0x77777000);
        bus.io_write_u16(0xcfe, 0x1234).unwrap();
        bus.io_write_u8(0xcfd, 0x31).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0x12343000);
        bus.io_write_u32(0xcf8, 0x8000005c).unwrap();
        bus.io_write_u32(0xcfc, 0x76543210).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0x76543210);
        for state in 0..4 {
            bus.ram[0xe9910..0xe9918].fill(0xa5);
            let generation = bus.pci.firmware.mapping_generation();
            bus.io_write_u8(0xcff, state).unwrap();
            bus.io_write_u8(0xcff, state).unwrap();
            assert_eq!(bus.pci.firmware.mapping_generation(), generation + 2);
            let byte = if state & 1 != 0 { 0xa5 } else { 0x5a };
            assert_eq!(bus.read_phys_u8(PhysAddr(0xe9910)).unwrap(), byte);
            assert_eq!(
                bus.read_phys_u16(PhysAddr(0xe9910)).unwrap(),
                u16::from_le_bytes([byte; 2])
            );
            assert_eq!(
                bus.read_phys_u32(PhysAddr(0xe9910)).unwrap(),
                u32::from_le_bytes([byte; 4])
            );
            assert_eq!(
                bus.read_phys_u64(PhysAddr(0xe9910)).unwrap(),
                u64::from_le_bytes([byte; 8])
            );
            bus.write_phys_u8(PhysAddr(0xe9910), 0x11).unwrap();
            bus.write_phys_u16(PhysAddr(0xe9911), 0x3322).unwrap();
            bus.write_phys_u32(PhysAddr(0xe9913), 0x77665544).unwrap();
            let expected = if state & 2 != 0 {
                &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77][..]
            } else {
                &[0xa5; 7]
            };
            assert_eq!(&bus.ram[0xe9910..0xe9917], expected);
            bus.write_phys_u64(PhysAddr(0xe9910), 0x8877665544332211)
                .unwrap();
            assert_eq!(
                bus.read_phys_u64(PhysAddr(0xfffe9910)).unwrap(),
                0x5a5a5a5a5a5a5a5a
            );
            bus.write_phys_u64(PhysAddr(0xfffe9910), 0).unwrap();
            assert_eq!(
                bus.read_phys_u64(PhysAddr(0xfffe9910)).unwrap(),
                0x5a5a5a5a5a5a5a5a
            );
            bus.set_a20(false);
            assert_eq!(
                bus.read_phys_u64(PhysAddr(0x1e9910)),
                bus.read_phys_u64(PhysAddr(0xe9910))
            );
            assert_eq!(
                bus.read_phys_u64(PhysAddr(0xfffe9910)).unwrap(),
                0x5a5a5a5a5a5a5a5a
            );
            bus.set_a20(true);
        }
        bus.ram[0xfffff] = 0x11;
        bus.ram[0x100000..0x100003].copy_from_slice(&[0x22, 0x33, 0x44]);
        assert_eq!(bus.read_phys_u32(PhysAddr(0xfffff)).unwrap(), 0x44332211);
        bus.write_phys_u32(PhysAddr(0xfffff), 0x88776655).unwrap();
        assert_eq!(&bus.ram[0xfffff..0x100003], &[0x55, 0x66, 0x77, 0x88]);
        bus.reset(0);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xfffff)).unwrap(), 0x5a);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xffffffff)).unwrap(), 0x5a);
    }

    #[test]
    fn firmware_pam_revokes_data_and_fetch_backing() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.load_option_rom(0xc8000, &[0xb8, 0x11, 0, 0, 0])
            .unwrap();
        bus.ram[0xc8001] = 0x22;
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0xc8000, 0);
        let data = LinearAddr(0xe9910);
        bus.io_write_u32(0xcf8, 0x8000005c).unwrap();
        for state in [3, 2, 0, 1, 3] {
            bus.io_write_u8(0xcff, state).unwrap();
            bus.sync_ioram(&mut cpu);
            assert!(cpu.cached_write_translation(data, false).is_none());
            mmu::write_linear_u32(&mut cpu, &mut bus, data, 0x12345678).unwrap();
            assert_eq!(
                cpu.cached_write_translation(data, false).is_some(),
                state == 3
            );
            assert_eq!(
                mmu::read_linear_u32(&mut cpu, &mut bus, data, AccessKind::Read).unwrap(),
                if state & 1 != 0 { 0x12345678 } else { 0 }
            );
        }
        bus.io_write_u32(0xcf8, 0x80000058).unwrap();
        for state in [1, 0, 3, 2] {
            cpu.eip = 0xc8000;
            bus.io_write_u8(0xcff, state).unwrap();
            assert_eq!(
                Interpreter::step(&mut cpu, &mut bus),
                Ok(StepOutcome::Running)
            );
            assert_eq!(
                cpu.reg(Reg32::Eax),
                if state & 1 != 0 { 0x22 } else { 0x11 }
            );
        }
        cpu.eip = 0xfffc8000;
        assert_eq!(
            Interpreter::step(&mut cpu, &mut bus),
            Ok(StepOutcome::Running)
        );
        assert_eq!(cpu.reg(Reg32::Eax), 0x11);
    }

    #[test]
    fn firmware_pam_page_walk_ad_stores_obey_write_enable() {
        let mut rom = vec![0; 8192];
        rom[..4].copy_from_slice(&0xe9003u32.to_le_bytes());
        rom[0x1008..0x100c].copy_from_slice(&0x3003u32.to_le_bytes());
        for state in 0..4 {
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            bus.load_option_rom(0xe8000, &rom).unwrap();
            bus.pci.firmware.write_pam(0x5f, state);
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.cr0 |= CR0_PG;
            cpu.cr3 = 0xe8000;
            bus.sync_ioram(&mut cpu);
            mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x2000), 0x12345678).unwrap();
            assert_eq!(bus.read_phys_u32(PhysAddr(0x3000)).unwrap(), 0x12345678);
            let ad = state & 2 != 0;
            assert_eq!(
                u32::from_le_bytes(bus.ram[0xe8000..0xe8004].try_into().unwrap()),
                0xe9003 | if ad { 0x20 } else { 0 }
            );
            assert_eq!(
                u32::from_le_bytes(bus.ram[0xe9008..0xe900c].try_into().unwrap()),
                0x3003 | if ad { 0x60 } else { 0 }
            );
            assert_eq!(bus.read_phys_u32(PhysAddr(0xfffe9008)).unwrap(), 0x3003);
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn firmware_pam_watched_code_excludes_writable_grants() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.pci.firmware.write_pam(0x5f, 3);
        let mut pages = crate::x86::jit::validity::CodePages::new(bus.ram.len());
        pages.watch(0xe9);
        bus.jit_code_pages = Some(pages);
        assert!(bus.ioram_page(PhysAddr(0xe9000), false).is_some());
        assert!(bus.ioram_page(PhysAddr(0xe9000), true).is_none());
        bus.write_phys_u32(PhysAddr(0xe9910), 0x12345678).unwrap();
        assert_eq!(&bus.ram[0xe9910..0xe9914], &0x12345678u32.to_le_bytes());
        assert!(!bus.jit_code_pages.as_ref().unwrap().watched(0xe9));
        assert!(bus.ioram_page(PhysAddr(0xe9000), true).is_some());
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_firmware_pam_changes_match_interpretation() {
        let make = || {
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0xc8000, 0);
            cpu.set_reg(Reg32::Esi, 0xe9910);
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            // ROM increments EDX; shadow increments EAX. Both store/read data.
            bus.load_option_rom(0xc8000, &[0x42, 0x89, 0x06, 0x8b, 0x1e, 0xeb, 0xf9])
                .unwrap();
            bus.ram[0xc8000] = 0x40;
            bus.attention = false;
            bus.next_attention_step = u64::MAX;
            (cpu, bus)
        };
        let (mut expected, mut reference) = make();
        let (mut actual, mut bus) = make();
        let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
        for (phase, state) in [3, 2, 0, 1, 3].into_iter().enumerate() {
            for b in [&mut reference, &mut bus] {
                b.io_write_u32(0xcf8, 0x80000058).unwrap();
                b.io_write_u8(0xcff, u8::from(phase & 1 == 0)).unwrap();
                b.io_write_u32(0xcf8, 0x8000005c).unwrap();
                b.io_write_u8(0xcff, state).unwrap();
                b.attention = false;
            }
            let result =
                Interpreter::run_batch(&mut expected, &mut reference, phase as u64 * 2000, 2000);
            let native = engine
                .run_batch(&mut actual, &mut bus, phase as u64 * 2000, 2000)
                .unwrap();
            assert_eq!(native, result);
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.effective_eflags(), expected.effective_eflags());
            assert_eq!(bus.ram, reference.ram);
        }
        assert!(engine.stats.native_steps > 1000);
        assert!(engine.stats.native_memory_writes > 0);
    }

    #[test]
    fn mmx_movq_legacy_vga_retains_read_latches_and_planar_writes() {
        use crate::devices::vga::DEFAULT_LFB_BASE;
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.ram[0x1000..0x1006].copy_from_slice(&[0x0f, 0x6f, 0x06, 0x0f, 0x7f, 0x07]);
        bus.io_write_u16(0x3c4, 0x0604).unwrap();
        bus.io_write_u16(0x3c4, 0x0f02).unwrap();
        bus.io_write_u16(0x3ce, 0x0506).unwrap();
        bus.io_write_u16(0x3ce, 0x0005).unwrap();
        bus.io_write_u16(0x3ce, 0x0204).unwrap(); // Read plane 2; all four planes latch.
        for i in 0..8 {
            for plane in 0..4 {
                assert!(bus.vga.lfb_write_u8(
                    DEFAULT_LFB_BASE + (0x400 + i) * 4 + plane,
                    DEFAULT_LFB_BASE,
                    (plane * 0x10 + i) as u8,
                ));
            }
        }
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0x1000, 0);
        cpu.set_reg(Reg32::Esi, 0xa0400);
        cpu.set_reg(Reg32::Edi, 0xa0800);
        assert_eq!(
            Interpreter::step(&mut cpu, &mut bus),
            Ok(StepOutcome::Running)
        );
        assert_eq!(cpu.fpu.mmx(0), 0x2726_2524_2322_2120);
        bus.io_write_u16(0x3ce, 0x0105).unwrap(); // Write mode 1 copies the last read's latches.
        assert_eq!(
            Interpreter::step(&mut cpu, &mut bus),
            Ok(StepOutcome::Running)
        );
        for i in 0..8 {
            for plane in 0..4 {
                assert_eq!(
                    bus.vga
                        .lfb_read_u8(DEFAULT_LFB_BASE + (0x800 + i) * 4 + plane, DEFAULT_LFB_BASE,),
                    Some((plane * 0x10 + 7) as u8)
                );
            }
        }
    }

    fn enable_test_vbe(bus: &mut HostBus) {
        for (index, value) in [(1, 640), (2, 480), (3, 8), (4, 0x81)] {
            bus.io_write_u16(0x1ce, index).unwrap();
            bus.io_write_u16(0x1cf, value).unwrap();
        }
        bus.io_write_u16(0x3ce, 0x0506).unwrap();
    }

    #[test]
    fn ioram_cache_revocation_preserves_ram_and_remarks_presented_pages() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        enable_test_vbe(&mut bus);
        let mut cpu = Cpu::new();
        bus.sync_ioram(&mut cpu);
        let ram = LinearAddr(0x4000);
        let bank = LinearAddr(0xa2001);
        mmu::write_linear_u32(&mut cpu, &mut bus, ram, 0x12345678).unwrap();
        mmu::write_linear_u32(&mut cpu, &mut bus, bank, 0xaabbccdd).unwrap();
        assert!(cpu.cached_write_translation(bank, false).is_some());
        assert!(cpu.cached_write_translation(ram, false).is_some());
        bus.vga.clear_dirty();
        bus.sync_ioram(&mut cpu);
        assert!(cpu.cached_write_translation(bank, false).is_none());
        assert!(cpu.cached_write_translation(ram, false).is_some());
        assert!(!bus.vga.dirty_page(2));
        mmu::write_linear_u16(&mut cpu, &mut bus, bank, 0x1122).unwrap();
        assert!(bus.vga.dirty_page(2));
        bus.io_write_u16(0x1ce, 5).unwrap();
        bus.io_write_u16(0x1cf, 1).unwrap();
        bus.sync_ioram(&mut cpu);
        assert!(cpu.cached_write_translation(bank, false).is_none());
        assert_eq!(
            mmu::read_linear_u32(&mut cpu, &mut bus, bank, AccessKind::Read).unwrap(),
            0
        );
        assert!(cpu.cached_read_translation(bank, false).is_some());
        mmu::write_linear_u32(&mut cpu, &mut bus, bank, 0x87654321).unwrap();
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xe000_2001)).unwrap(),
            0xaabb1122
        );
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xe001_2001)).unwrap(),
            0x87654321
        );
        bus.io_write_u16(0x1ce, 4).unwrap();
        bus.io_write_u16(0x1cf, 0).unwrap();
        bus.sync_ioram(&mut cpu);
        assert!(cpu.cached_write_translation(bank, false).is_none());
        assert!(cpu.cached_write_translation(ram, false).is_some());
        assert!(bus.ioram_page(PhysAddr(0xa2000), true).is_none());
        assert!(bus.ioram_page(PhysAddr(0xe000_2000), true).is_none());
    }

    #[test]
    fn ioram_address16_rep_preserves_wrap_budget_and_high_register_halves() {
        use crate::x86::cpu::SegmentCache;
        for mode in 0..3 {
            for width in [1u32, 2, 4] {
                for fill in [false, true] {
                    let mut bus = HostBus::new_firmware(2 << 20).unwrap();
                    bus.attention = false;
                    if mode == 1 {
                        enable_test_vbe(&mut bus);
                        bus.io_write_u16(0x1ce, 5).unwrap();
                        bus.io_write_u16(0x1cf, 2).unwrap();
                    } else {
                        bus.io_write_u16(0x3c4, if mode == 0 { 0x0e04 } else { 0x0604 })
                            .unwrap();
                        bus.io_write_u16(0x3c4, 0x0f02).unwrap();
                        bus.io_write_u16(0x3ce, 0x0506).unwrap();
                        bus.io_write_u16(0x3ce, 0x0005).unwrap();
                    }
                    for offset in 0..65536usize {
                        bus.ram[0x20000 + offset] = (offset ^ (offset >> 8)) as u8;
                    }
                    let mut code = vec![0x67, 0xf3];
                    if width == 2 {
                        code.push(0x66);
                    }
                    code.push(if fill {
                        if width == 1 { 0xaa } else { 0xab }
                    } else if width == 1 {
                        0xa4
                    } else {
                        0xa5
                    });
                    bus.ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0x1000, 0);
                    for (seg, base) in [(SegmentReg::Ds, 0x20000), (SegmentReg::Es, 0xa0000)] {
                        cpu.set_seg_cache(
                            seg,
                            SegmentCache {
                                base,
                                ..cpu.seg(seg)
                            },
                        );
                    }
                    cpu.set_reg(Reg32::Esi, 0xaaaa_fff0);
                    cpu.set_reg(Reg32::Edi, 0xbbbb_fff0);
                    cpu.set_reg(Reg32::Ecx, 0xcccc_1004);
                    cpu.set_reg(Reg32::Eax, 0x7351_2917);
                    for completed in [4096u32, 4100] {
                        assert_eq!(
                            Interpreter::step(&mut cpu, &mut bus),
                            Ok(StepOutcome::Running)
                        );
                        let index = 0xfff0u32.wrapping_add(completed * width) & 65535;
                        assert_eq!(cpu.reg(Reg32::Edi), 0xbbbb_0000 | index);
                        assert_eq!(
                            cpu.reg(Reg32::Esi),
                            if fill {
                                0xaaaa_fff0
                            } else {
                                0xaaaa_0000 | index
                            }
                        );
                        assert_eq!(cpu.reg(Reg32::Ecx), 0xcccc_0000 | (4100 - completed));
                        assert_eq!(
                            cpu.eip,
                            if completed == 4096 {
                                0x1000
                            } else {
                                0x1000 + code.len() as u32
                            }
                        );
                        for byte in 0..completed * width {
                            let at = (0xfff0 + byte) & 65535;
                            let expected = if fill {
                                (0x7351_2917u32 >> ((byte % width) * 8)) as u8
                            } else {
                                bus.ram[0x20000 + at as usize]
                            };
                            assert_eq!(
                                bus.read_phys_u8(PhysAddr(0xa0000 + u64::from(at))).unwrap(),
                                expected,
                                "mode={mode} width={width} fill={fill} byte={byte}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ioram_split_faults_do_not_dirty_or_commit_the_first_page() {
        for width in [2, 4] {
            for present in [false, true] {
                let mut bus = HostBus::new_firmware(2 << 20).unwrap();
                enable_test_vbe(&mut bus);
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0, 0);
                cpu.cr0 |= CR0_PG | crate::x86::cpu::CR0_WP;
                cpu.cr3 = 0x4000;
                write_ram_u32(&mut bus, 0x4000, 0x5007);
                write_ram_u32(&mut bus, 0x5018, 0xe0002007);
                write_ram_u32(&mut bus, 0x501c, if present { 0xe0003005 } else { 0 });
                bus.sync_ioram(&mut cpu);
                bus.vga.clear_dirty();
                bus.sync_ioram(&mut cpu);
                let result = if width == 2 {
                    mmu::write_linear_u16(&mut cpu, &mut bus, LinearAddr(0x6fff), 0x1234)
                } else {
                    mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x6ffd), 0x12345678)
                };
                assert!(matches!(result, Err(mmu::Fault::Page(_))));
                assert_eq!(cpu.cr2, 0x7000);
                assert_eq!(bus.read_phys_u32(PhysAddr(0xe0002ffc)).unwrap(), 0);
                assert!(!bus.vga.dirty_page(2));
                assert!(!bus.vga.dirty_page(3));
            }
        }
    }

    #[test]
    fn rep_movsd_lfb_routes_preserve_data_registers_and_dirty_pages() {
        for base in [0xe000_0000u32, 0xfd00_0000, 0x0100_0000] {
            for (src_lfb, dst_lfb) in [(false, true), (true, false), (true, true)] {
                let mut bus = HostBus::new_firmware(32 << 20).unwrap();
                bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
                bus.io_write_u32(0xcfc, base).unwrap();
                bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
                bus.io_write_u32(0xcfc, 2).unwrap();
                bus.ram[0x1000..0x1002].copy_from_slice(&[0xf3, 0xa5]);
                let src = if src_lfb { base + 0x1ff8 } else { 0x5ff8 };
                let dst = if dst_lfb { base + 0x4ff8 } else { 0x8ff8 };
                let data: Vec<u8> = (0..32).map(|i| (i * 7 + 3) as u8).collect();
                for (i, &byte) in data.iter().enumerate() {
                    bus.write_phys_u8(PhysAddr(u64::from(src) + i as u64), byte)
                        .unwrap();
                }
                bus.vga.clear_dirty();
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0);
                cpu.set_reg(Reg32::Esi, src);
                cpu.set_reg(Reg32::Edi, dst);
                cpu.set_reg(Reg32::Ecx, 8);
                assert_eq!(
                    Interpreter::step(&mut cpu, &mut bus),
                    Ok(StepOutcome::Running)
                );
                for (i, &byte) in data.iter().enumerate() {
                    assert_eq!(
                        bus.read_phys_u8(PhysAddr(u64::from(dst) + i as u64))
                            .unwrap(),
                        byte
                    );
                }
                assert_eq!(cpu.reg(Reg32::Esi), src + 32);
                assert_eq!(cpu.reg(Reg32::Edi), dst + 32);
                assert_eq!(cpu.reg(Reg32::Ecx), 0);
                assert_eq!(cpu.eip, 0x1002);
                for page in 0..6 {
                    assert_eq!(
                        bus.vga.dirty_page(page),
                        dst_lfb && (page == 4 || page == 5)
                    );
                }
                if dst_lfb && base == 0x0100_0000 {
                    assert_eq!(&bus.ram[dst as usize..dst as usize + 32], &[0; 32]);
                }
            }
        }
    }

    #[test]
    fn rep_movsd_lfb_overlap_preserves_forward_order_and_a20_aliases() {
        let base = 0xe000_0000u32;
        for a20 in [true, false] {
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            bus.io_write_u8(0x92, if a20 { 2 } else { 0 }).unwrap();
            bus.ram[0x1000..0x1002].copy_from_slice(&[0xf3, 0xa5]);
            for i in 0..5 {
                bus.write_phys_u32(PhysAddr(u64::from(base + 0x400 + i * 4)), i + 1)
                    .unwrap();
            }
            bus.vga.clear_dirty();
            let src = base + 0x400 + if a20 { 0 } else { 0x100000 };
            let dst = base + 0x404;
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.set_reg(Reg32::Esi, src);
            cpu.set_reg(Reg32::Edi, dst);
            cpu.set_reg(Reg32::Ecx, 4);
            assert_eq!(
                Interpreter::step(&mut cpu, &mut bus),
                Ok(StepOutcome::Running)
            );
            for i in 0..5 {
                assert_eq!(
                    bus.read_phys_u32(PhysAddr(u64::from(base + 0x400 + i * 4)))
                        .unwrap(),
                    1
                );
            }
            assert_eq!(cpu.reg(Reg32::Esi), src + 16);
            assert_eq!(cpu.reg(Reg32::Edi), dst + 16);
            assert_eq!(cpu.reg(Reg32::Ecx), 0);
            assert!(bus.vga.dirty_page(0));
        }
    }

    #[test]
    fn rep_movsd_lfb_decode_changes_reveal_underlying_ram() {
        let old = 0x0100_0000u32;
        let new = 0xfd00_0000u32;
        let mut bus = HostBus::new_firmware(32 << 20).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
        bus.io_write_u32(0xcfc, old).unwrap();
        bus.ram[0x1000..0x1002].copy_from_slice(&[0xf3, 0xa5]);
        bus.ram[old as usize + 0x2000..old as usize + 0x2010].fill(0x77);
        assert!(
            bus.vga
                .lfb_write_bytes(u64::from(old + 0x2000), u64::from(old), &[0x55; 16])
        );
        for (bar, enabled, source, expected) in [
            (old, 0, old, 0x77),
            (new, 2, old, 0x77),
            (new, 2, new, 0x55),
        ] {
            bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
            bus.io_write_u32(0xcfc, bar).unwrap();
            bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
            bus.io_write_u32(0xcfc, enabled).unwrap();
            bus.vga.clear_dirty();
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0x1000, 0);
            cpu.set_reg(Reg32::Esi, source + 0x2000);
            cpu.set_reg(Reg32::Edi, 0x8000);
            cpu.set_reg(Reg32::Ecx, 4);
            assert_eq!(
                Interpreter::step(&mut cpu, &mut bus),
                Ok(StepOutcome::Running)
            );
            assert_eq!(&bus.ram[0x8000..0x8010], &[expected; 16]);
            assert!(!bus.vga.whole_frame_dirty());
        }
    }

    #[test]
    fn firmware_vga_lfb_follows_pci_bar_relocation() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
        bus.io_write_u32(0xcfc, 0xfd00_0000).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
        bus.io_write_u32(0xcfc, 0x0000_0002).unwrap();

        assert_eq!(
            bus.classify_phys(PhysAddr(0xfd00_0000), 4),
            PhysRegionKind::FbDirtyTracked
        );
        bus.before_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0xfd00_0000), 4))
            .unwrap();
        bus.write_phys_u32(PhysAddr(0xfd00_0000), 0x4433_2211)
            .unwrap();
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xfd00_0000)).unwrap(),
            0x4433_2211
        );

        assert!(bus.phys_ram_repeat_write_supported(PhysAddr(0xfd00_0004), 8));
        bus.write_phys_ram_repeat(PhysAddr(0xfd00_0004), 4, 0xaabb_ccdd, 2)
            .unwrap();
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xfd00_0004)).unwrap(),
            0xaabb_ccdd
        );
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0xfd00_0008)).unwrap(),
            0xaabb_ccdd
        );
    }

    #[test]
    fn bios_reset_uses_legacy_reset_vector_state() {
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0x1234, 0);
        cpu.reset_bios();

        assert_eq!(cpu.eip, 0xfff0);
        assert_eq!(cpu.cr0, 0x10);
        assert_eq!(cpu.seg(SegmentReg::Cs).selector, 0xf000);
        assert_eq!(cpu.seg(SegmentReg::Cs).base, 0xf0000);
    }

    #[test]
    fn firmware_port92_reset_exits_instruction_batch_before_next_instruction() {
        let mut machine = Machine::new_firmware(2 * 1024 * 1024, FirmwareDisplay::Vga).unwrap();
        machine.cpu.enter_protected_flat(0x1000, 0);
        machine.bus.ram[0x1000..0x1009].copy_from_slice(&[
            0xb0, 0x03, // mov al,3: keep A20 enabled and request reset
            0xe6, 0x92, // out 92h,al
            0xb8, 0x78, 0x56, 0x34, 0x12, // must not execute
        ]);
        let report = machine.run_until_stop::<false>(10).unwrap();
        assert!(matches!(report.stop, Some(GuestStepStatus::Reset)));
        assert_eq!(machine.cpu.eip, 0x1004);
        assert_eq!(machine.bus.io_read_u8(0x92).unwrap(), 0x02);
        assert!(machine.bus.reset_requested);
    }

    #[test]
    fn serial_output_keeps_a_bounded_tail_at_absolute_positions() {
        let mut m = Machine::new_firmware(2 * 1024 * 1024, FirmwareDisplay::Vga).unwrap();
        for i in 0..2 * SERIAL_KEEP + 3 {
            m.bus.io_write_u8(0x402, i as u8).unwrap();
        }
        let end = 2 * SERIAL_KEEP as u64 + 3;
        assert_eq!(m.serial_end(), end);
        assert_eq!(m.serial_output().len(), SERIAL_KEEP + 3);
        let (bytes, next) = m.serial_since(end - 2);
        assert_eq!(
            (bytes, next),
            (&[(end - 2) as u8, (end - 1) as u8][..], end)
        );
        // A reader that fell behind resumes at the oldest kept byte.
        let (bytes, _) = m.serial_since(0);
        assert_eq!(bytes.len(), SERIAL_KEEP + 3);
        assert_eq!(bytes[0], SERIAL_KEEP as u8);
    }

    #[test]
    fn guest_reset_reloads_rom_and_resets_devices_in_each_execution_mode() {
        for trace in [TraceMode::Off, TraceMode::Minimal, TraceMode::Full] {
            for (port, value) in [(0x92, 3), (0x64, 0xfe)] {
                let mut m = Machine::new_firmware(2 * 1024 * 1024, FirmwareDisplay::Vga).unwrap();
                m.load_bios_rom(&[0xf4; 16]).unwrap();
                m.load_option_rom(0xc0000, &[0x55, 0xaa]).unwrap();
                m.bus.ram[0xffff0] = 0xcc;
                m.bus.ram[0xc0000] = 0xcc;
                m.bus.ram[0x12345] = 0x5a;
                m.bus.io_write_u8(0x21, 0).unwrap();
                m.bus.pic.pulse(1);
                assert_eq!(m.bus.pic.acknowledge(), 0x21);
                m.bus.serial.push(b'A');
                m.queue_serial_input(b"discard");
                m.bus.io_write_u8(0x3fc, 0x1a).unwrap();
                m.bus.io_write_u8(0x3f9, 0x08).unwrap();
                m.bus.port61 = 3;
                m.cpu.enter_protected_flat(0x1000, 0);
                m.bus.ram[0x1000..0x1004].copy_from_slice(&[0xb0, value, 0xe6, port]);
                m.set_diagnostics(trace, PerfMode::Disabled);
                let report = m.run_steps(2).unwrap();
                assert!(report.stop.is_none());
                assert_eq!(m.reset_count(), 1);
                assert_eq!(m.step_count(), 2);
                assert_eq!(m.cpu.eip, 0xfff0);
                assert_eq!(m.cpu.cr0, 0x10);
                assert_eq!(m.cpu.tsc(), 0);
                assert_eq!(m.bus.ram[0xffff0], 0xf4);
                assert_eq!(&m.bus.ram[0xc0000..0xc0002], &[0x55, 0xaa]);
                assert_eq!(m.bus.ram[0x12345], 0x5a);
                assert_eq!(m.bus.pic.in_service(false) | m.bus.pic.in_service(true), 0);
                assert_eq!(m.bus.port61, 0);
                assert_eq!(m.bus.serial, b"A");
                assert!(m.bus.serial_rx.is_empty());
                assert_eq!(m.bus.io_read_u8(0x3fc).unwrap(), 0);
                assert_eq!(m.bus.io_read_u8(0x3fe).unwrap(), 0xb0);
                assert_eq!(m.bus.io_read_u8(0x3fa).unwrap(), 1);
                assert!(!m.bus.reset_requested);
                assert_eq!(m.bus.pit.next_irq_tick(), u64::MAX);
                assert!(matches!(m.step_once().unwrap(), GuestStepStatus::Halted));
            }
        }
    }

    #[test]
    fn triple_fault_resets_board_and_retains_pre_reset_diagnostics() {
        for trace in [TraceMode::Off, TraceMode::Minimal, TraceMode::Full] {
            let mut m = Machine::new_firmware(2 * 1024 * 1024, FirmwareDisplay::Vga).unwrap();
            m.load_bios_rom(&[0xf4; 16]).unwrap();
            m.cpu.enter_protected_flat(0x1000, 0);
            m.cpu.idtr.limit = 0;
            m.bus.ram[0x1000..0x1002].copy_from_slice(&[0x0f, 0x0b]); // UD2, no usable IDT.
            m.bus.serial.extend_from_slice(b"before shutdown");
            m.set_diagnostics(trace, PerfMode::Disabled);
            assert!(m.run_steps(1).unwrap().stop.is_none());
            assert_eq!(m.reset_count(), 1);
            assert_eq!(m.cpu.eip, 0xfff0);
            let shutdown = m.last_cpu_shutdown().unwrap();
            assert_eq!(
                shutdown.error,
                StepError::TripleFault {
                    eip: 0x1000,
                    vector: 13,
                    code: 0x43
                }
            );
            assert!(shutdown.cpu_dump.contains("eip: 4096"));
            assert_eq!(shutdown.serial, b"before shutdown");
            let dump = shutdown.cpu_dump.clone();
            let serial = shutdown.serial.clone();
            assert!(matches!(m.step_once().unwrap(), GuestStepStatus::Halted));
            m.reboot().unwrap();
            assert_eq!(m.last_cpu_shutdown().unwrap().cpu_dump, dump);
            assert_eq!(m.last_cpu_shutdown().unwrap().serial, serial);
        }
    }

    #[test]
    #[cfg(feature = "host")]
    fn direct_linux_reset_restores_boot_inputs_after_guest_overwrites() {
        use crate::linux_boot::{BOOT_PARAMS_ADDR, CMDLINE_ADDR, KERNEL_LOAD_ADDR};
        let mut kernel = vec![0; 0x5000];
        kernel[0x1f1] = 31;
        kernel[0x202..0x206].copy_from_slice(b"HdrS");
        kernel[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        kernel[0x4000..0x4004].copy_from_slice(&[0xb0, 3, 0xe6, 0x92]);
        let mut m = Machine::new(64 * 1024 * 1024);
        let boot = m
            .load_linux(
                &kernel,
                &[0x5a; 4096],
                &BootConfig {
                    mem_size: 64 * 1024 * 1024,
                    cmdline: "console=ttyS0 test",
                    host_decompress_kernel: false,
                    host_decompress_initrd: false,
                    overlay_files: &[],
                    dump_initrd_path: None,
                },
            )
            .unwrap();
        let params = m.ram()[BOOT_PARAMS_ADDR as usize..BOOT_PARAMS_ADDR as usize + 4096].to_vec();
        m.bus.ram[BOOT_PARAMS_ADDR as usize..BOOT_PARAMS_ADDR as usize + 4096].fill(0xcc);
        m.bus.ram[CMDLINE_ADDR as usize] = 0;
        m.bus.ram[boot.initrd_load_addr as usize] = 0;
        m.bus.ram[0x1234] = 0xcc;
        let report = m.run_steps(2).unwrap();
        assert!(report.stop.is_none());
        assert_eq!(m.reset_count(), 1);
        assert_eq!(m.cpu.eip, KERNEL_LOAD_ADDR);
        assert_eq!(m.cpu.tsc(), 0);
        assert_eq!(m.cpu.reg(Reg32::Esi), BOOT_PARAMS_ADDR);
        assert_eq!(
            &m.ram()[BOOT_PARAMS_ADDR as usize..BOOT_PARAMS_ADDR as usize + 4096],
            params
        );
        assert_eq!(
            &m.ram()[CMDLINE_ADDR as usize..CMDLINE_ADDR as usize + 19],
            b"console=ttyS0 test\0"
        );
        assert_eq!(m.ram()[boot.initrd_load_addr as usize], 0x5a);
        assert_eq!(m.ram()[0x1234], 0);
    }

    #[test]
    fn firmware_port92_a20_register_write_does_not_request_reset() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        for value in [0, 2, 0, 2] {
            bus.io_write_u8(0x92, value).unwrap();
            assert_eq!(bus.io_read_u8(0x92).unwrap(), value);
            assert!(!bus.reset_requested);
        }
    }

    #[test]
    fn a20_himem_cmps_and_warm_fetch_follow_both_gate_controls() {
        use crate::x86::cpu::EFLAGS_ZF;

        for (port, values) in [(0x92, [2_u8, 0, 2]), (0x64, [0xdf, 0xdd, 0xdf])] {
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            let mut cpu = Cpu::new();
            cpu.set_seg_cache(
                SegmentReg::Cs,
                SegmentCache {
                    selector: 0xffff,
                    base: 0xffff0,
                    limit: 0xffff,
                    flags: 0x009b,
                },
            );
            for (seg, selector) in [(SegmentReg::Ds, 0), (SegmentReg::Es, 0xffff)] {
                cpu.set_seg_cache(
                    seg,
                    SegmentCache {
                        selector,
                        base: u32::from(selector) << 4,
                        limit: u32::MAX,
                        flags: 0xc093,
                    },
                );
            }
            bus.ram[0x10..0x20].fill(0x11);
            bus.ram[0x100010..0x100020].fill(0x22);
            for (base, marker) in [(0x1000, 0x33), (0x101000, 0x44)] {
                bus.ram[base..base + 7].copy_from_slice(&[
                    0xe6, port, // out gate,al; the fetch cursor is already warm
                    0xf3, 0x66, 0xa7, // HIMEMX: repe cmpsd, 16-bit SI/DI/CX
                    0xb3, marker, // mov bl,marker identifies the fetched backing
                ]);
            }
            for (index, value) in values.into_iter().enumerate() {
                let enabled = index != 1;
                cpu.eip = 0x1010;
                cpu.set_reg(Reg32::Eax, u32::from(value));
                cpu.set_reg(Reg32::Ecx, 4);
                cpu.set_reg(Reg32::Esi, 0x10);
                cpu.set_reg(Reg32::Edi, 0x20);
                assert_eq!(
                    Interpreter::step(&mut cpu, &mut bus),
                    Ok(StepOutcome::Running)
                );
                assert_eq!(
                    Interpreter::step(&mut cpu, &mut bus),
                    Ok(StepOutcome::Running)
                );
                assert_eq!(cpu.flag(EFLAGS_ZF), !enabled);
                assert_eq!(cpu.reg(Reg32::Ecx), if enabled { 3 } else { 0 });
                assert_eq!(cpu.reg(Reg32::Esi), if enabled { 0x14 } else { 0x20 });
                assert_eq!(cpu.reg(Reg32::Edi), if enabled { 0x24 } else { 0x30 });
                assert_eq!(
                    Interpreter::step(&mut cpu, &mut bus),
                    Ok(StepOutcome::Running)
                );
                assert_eq!(cpu.reg(Reg32::Ebx) as u8, if enabled { 0x44 } else { 0x33 });
                assert_eq!(
                    bus.io_read_u8(0x92).unwrap() & 2,
                    if enabled { 2 } else { 0 }
                );
                assert!(!bus.reset_requested);
            }
        }
    }

    #[test]
    fn a20_scalar_writes_split_original_addresses_at_each_mib_edge() {
        let mut bus = HostBus::new(5 << 20);
        bus.io_write_u8(0x92, 0).unwrap();
        let value = 0x8877_6655_4433_2211_u64;
        for addr in [0x100010, 0x300010, 0xfffff, 0x1fffff, 0x2fffff, 0x3fffff] {
            bus.ram.fill(0xa5);
            crate::x86::mem::write_phys_u64(&mut bus, PhysAddr(addr), value).unwrap();
            assert_eq!(bus.read_phys_u64(PhysAddr(addr)).unwrap(), value);
            for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
                let raw = addr + offset as u64;
                let mapped = raw & !A20_BIT;
                assert_eq!(bus.ram[mapped as usize], byte, "address={raw:x}");
                if raw != mapped {
                    assert_eq!(bus.ram[raw as usize], 0xa5, "unmasked address={raw:x}");
                }
            }
        }
        // Bookkeeping must observe the actual low page, including a split tail.
        bus.frame_roles[0].insert(FrameRoleFlags::TABLE);
        let watched = bus.tracked_writes.page_table_frame;
        crate::x86::mem::write_phys_u16(&mut bus, PhysAddr(0xfffff), 0x1234).unwrap();
        assert_eq!(bus.ram[0], 0x12);
        assert_eq!(bus.tracked_writes.page_table_frame, watched + 1);
    }

    #[test]
    fn a20_debug_write_notifies_the_backing_it_changes() {
        let mut bus = HostBus::new(2 << 20);
        bus.io_write_u8(0x92, 0).unwrap();
        bus.frame_roles[0].insert(FrameRoleFlags::TABLE);
        crate::x86::mem::write_phys_u32_with_origin(
            &mut bus,
            PhysAddr(0x100010),
            0x1234_5678,
            WriteOrigin::Debug,
        )
        .unwrap();
        assert_eq!(bus.read_backing_u32(0x10), 0x1234_5678);
        assert_eq!(bus.read_backing_u32(0x100010), 0);
        assert_eq!(bus.tracked_writes.debug, 1);
        assert_eq!(bus.tracked_writes.page_table_frame, 1);
    }

    #[test]
    fn a20_bulk_transfers_preserve_alias_overlap_and_pio_destinations() {
        let mut bus = HostBus::new_firmware(4 << 20).unwrap();
        let mut cpu = Cpu::new();
        bus.io_write_u8(0x92, 0).unwrap();
        bus.ram[0x3000..0x3003].copy_from_slice(&[0xf3, 0x66, 0xa5]); // rep movsd
        cpu.eip = 0x3000;
        cpu.set_seg_cache(
            SegmentReg::Ds,
            SegmentCache {
                selector: 0xffff,
                base: 0xffff0,
                limit: 0xffff,
                flags: 0x0093,
            },
        );
        cpu.set_reg(Reg32::Esi, 0x110); // raw 0x100100 aliases 0x100
        cpu.set_reg(Reg32::Edi, 0x104);
        cpu.set_reg(Reg32::Ecx, 4);
        write_ram_u32(&mut bus, 0x100, 0x1234_5678);
        bus.ram[0x104..0x114].fill(0xa5);
        assert!(!bus.phys_ram_copy_supported(PhysAddr(0x100100), PhysAddr(0x104), 16));
        assert_eq!(
            Interpreter::step(&mut cpu, &mut bus),
            Ok(StepOutcome::Running)
        );
        assert_eq!(
            &bus.ram[0x100..0x114],
            &0x1234_5678_u32.to_le_bytes().repeat(5)
        );

        assert!(bus.phys_ram_copy_supported(PhysAddr(0x100100), PhysAddr(0x100200), 16));
        bus.copy_phys_to_ram(PhysAddr(0x100100), PhysAddr(0x100200), 16, 1)
            .unwrap();
        assert_eq!(&bus.ram[0x200..0x210], &bus.ram[0x100..0x110]);
        bus.write_phys_ram_repeat(PhysAddr(0x100300), 4, 0x9876_5432, 4)
            .unwrap();
        assert_eq!(
            &bus.ram[0x300..0x310],
            &0x9876_5432_u32.to_le_bytes().repeat(4)
        );
        for addr in [0xfffff, 0x1fffff] {
            assert!(!bus.phys_ram_repeat_write_supported(PhysAddr(addr), 2));
            assert!(!bus.phys_ram_copy_supported(PhysAddr(addr), PhysAddr(0x200), 2));
            assert!(!bus.phys_ram_copy_supported(PhysAddr(0x200), PhysAddr(addr), 2));
        }
        bus.attach_hdd(vec![0; 512], false);
        bus.io_write_u8(0x1f7, 0xec).unwrap(); // IDENTIFY through CPU REP INS bulk path
        assert!(!bus.io_read_to_ram_supported(0x1f0, 2, PhysAddr(0x1fffff), 1));
        assert!(bus.io_read_to_ram(0x1f0, 2, PhysAddr(0x100400), 4).unwrap());
        assert_eq!(&bus.ram[0x400..0x402], &0x0040_u16.to_le_bytes());
        assert_eq!(&bus.ram[0x100400..0x100408], &[0; 8]);
    }

    #[test]
    fn a20_masks_page_walk_reads_ad_writes_and_cached_stores() {
        let mut bus = HostBus::new(4 << 20);
        let mut cpu = paging_cpu(0x101000);
        bus.io_write_u8(0x92, 0).unwrap();
        write_ram_u32(&mut bus, 0x1000, 0x102003);
        write_ram_u32(&mut bus, 0x2010, 0x103003);
        write_ram_u32(&mut bus, 0x3000, 0x1234_5678);
        bus.frame_roles[1].insert(FrameRoleFlags::DIRECTORY);
        bus.frame_roles[2].insert(FrameRoleFlags::TABLE);
        assert_eq!(
            mmu::read_linear_u32(&mut cpu, &mut bus, LinearAddr(0x4000), AccessKind::Read).unwrap(),
            0x1234_5678
        );
        for value in [0x8877_6655, 0xaabb_ccdd] {
            mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x4000), value).unwrap();
            assert_eq!(bus.read_backing_u32(0x3000), value);
            assert_eq!(
                mmu::read_linear_u32(&mut cpu, &mut bus, LinearAddr(0x4000), AccessKind::Read)
                    .unwrap(),
                value
            );
        }
        assert_eq!(bus.read_backing_u32(0x1000), 0x102023); // PDE accessed
        assert_eq!(bus.read_backing_u32(0x2010), 0x103063); // PTE accessed/dirty
        assert_eq!(bus.read_backing_u32(0x101000), 0);
        assert_eq!(bus.read_backing_u32(0x102010), 0);
        assert_eq!(bus.read_backing_u32(0x103000), 0);
        assert_eq!(bus.tracked_writes.page_table_frame, 3);
    }

    #[test]
    fn interrupt_attention_starts_due_then_schedules_timer_boundary() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();

        assert!(bus.interrupt_attention_due(0));
        bus.io_write_u8(0x43, 0x30).unwrap(); // mode 0, full 16-bit count
        bus.io_write_u8(0x40, 0).unwrap();
        bus.io_write_u8(0x40, 0).unwrap();
        let due = 65_537 * PIT_OSCILLATOR_DIVISOR; // includes the load clock
        bus.refresh_interrupt_attention(0);

        assert!(!bus.interrupt_attention_due(1));
        assert!(!bus.interrupt_attention_due(due - 1));
        assert!(bus.interrupt_attention_due(due));
    }

    #[test]
    fn halted_fast_forward_that_exhausts_budget_is_not_a_guest_stop() {
        let mut bus = HostBus::new(0x4000);
        let mut cpu = Cpu::new();
        cpu.set_tsc(0x1234);
        bus.next_attention_step = 100;
        let mut step = 10;
        let mut ran = 10;

        assert!(fast_forward_halted(&mut cpu, &bus, &mut step, &mut ran, 50));
        assert_eq!(step, 50);
        assert_eq!(ran, 50);
        assert_eq!(cpu.tsc(), 0x1234 + 40);
    }

    #[test]
    fn halted_machine_time_matches_single_step_traced_and_batched_execution() {
        fn machine(trace: TraceMode) -> Machine {
            let mut bus = HostBus::new(0x10000);
            bus.ram[..2].copy_from_slice(&[0xf4, 0x90]);
            // Keep a future hardware event: an unprogrammed PIT no longer
            // invents a periodic IRQ to make a quiescent HLT consume its budget.
            bus.io_write_u8(0x43, 0x30).unwrap();
            bus.io_write_u8(0x40, 0).unwrap();
            bus.io_write_u8(0x40, 0).unwrap();
            let mut cpu = Cpu::new();
            cpu.set_tsc(u64::MAX - 10);
            Machine::from_parts(cpu, bus, trace, PerfMode::Disabled)
        }
        let mut single = machine(TraceMode::Off);
        for _ in 0..250 {
            assert!(matches!(
                single.step_once().unwrap(),
                GuestStepStatus::Halted
            ));
        }
        for trace in [TraceMode::Off, TraceMode::Minimal, TraceMode::Full] {
            let mut batched = machine(trace);
            for _ in 0..5 {
                let report = batched.run_steps(50).unwrap();
                assert_eq!(report.ran, 50);
                assert!(report.stop.is_none());
            }
            assert_eq!(batched.cpu.tsc(), single.cpu.tsc());
            assert_eq!(batched.step_count(), single.step_count());
            assert_eq!(batched.cpu.eip, 1);
            assert!(batched.cpu.halted);
        }
        assert_eq!(single.cpu.tsc(), 239);
    }

    #[test]
    fn serial_observation_yields_at_the_output_instruction_in_each_mode() {
        for trace in [TraceMode::Off, TraceMode::Minimal, TraceMode::Full] {
            for perf in [PerfMode::Disabled, PerfMode::Enabled] {
                let mut bus = HostBus::new(0x10000);
                bus.ram[..10].copy_from_slice(&[
                    0xba, 0xf8, 0x03, // mov dx,3f8h
                    0xb0, b'A', // mov al,'A'
                    0xee, // out dx,al
                    0x90, // nop
                    0x40, // inc ax ('B')
                    0xee, // out dx,al
                    0x90, // nop
                ]);
                let mut m = Machine::from_parts(Cpu::new(), bus, trace, perf);
                let empty = m.run_until_serial_output(0).unwrap();
                assert_eq!(empty.ran, 0);
                let first = m.run_until_serial_output(100).unwrap();
                assert_eq!(first.ran, 3);
                assert!(first.stop.is_none());
                assert_eq!(m.cpu.eip, 6);
                assert_eq!(m.serial_output(), b"A");
                let second = m.run_until_serial_output(100).unwrap();
                assert_eq!(second.ran, 3);
                assert_eq!(m.cpu.eip, 9);
                assert_eq!(m.serial_output(), b"AB");
                assert_eq!(m.cpu.tsc(), 6);
                assert_eq!(m.run_steps(1).unwrap().ran, 1);
                assert_eq!(m.cpu.eip, 10);
            }
        }
    }

    #[test]
    fn pit_irq_latches_until_interrupts_are_enabled() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        let mut cpu = Cpu::new();
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.io_write_u8(0x43, 0x10).unwrap(); // mode 0, LSB-only
        bus.io_write_u8(0x40, 3).unwrap();
        bus.refresh_interrupt_attention(4 * PIT_OSCILLATOR_DIVISOR);

        assert_eq!(bus.pic.pending_irq(), Some(0));
        assert!(!bus.attention);
        assert_ne!(pending_external_irq(&cpu, &bus), Some(0));

        cpu.eflags |= EFLAGS_IF;
        assert_eq!(pending_external_irq(&cpu, &bus), Some(0));
    }

    #[test]
    fn pit_one_shot_does_not_repeat_and_elapsed_periods_coalesce_in_pic() {
        let mut bus = HostBus::new(0x1000);
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.io_write_u8(0x43, 0x10).unwrap();
        bus.io_write_u8(0x40, 3).unwrap();
        bus.begin_io_instruction(4 * PIT_OSCILLATOR_DIVISOR, 0x71);
        bus.update_pit0_irq(bus.device_ticks());
        assert_eq!(bus.pic.pending_irq(), Some(0));
        assert_eq!(bus.pic.acknowledge(), 0x20);
        bus.io_write_u8(0x20, 0x20).unwrap();
        bus.begin_io_instruction(70_000 * PIT_OSCILLATOR_DIVISOR, 0x71);
        bus.update_pit0_irq(bus.device_ticks());
        assert_eq!(bus.pic.pending_irq(), None, "mode 0 never becomes periodic");

        bus.io_write_u8(0x43, 0x14).unwrap(); // rate mode, count 4
        bus.io_write_u8(0x40, 4).unwrap();
        // Both samples see OUT high, with many complete low pulses between.
        bus.begin_io_instruction(74_001 * PIT_OSCILLATOR_DIVISOR, 0x71);
        bus.update_pit0_irq(bus.device_ticks());
        assert_eq!(bus.pic.pending_irq(), Some(0));
        bus.io_write_u8(0x43, 0).unwrap(); // latching must not withdraw the IRQ
        assert_eq!(bus.pic.pending_irq(), Some(0));
        assert_eq!(bus.pic.acknowledge(), 0x20);
        bus.io_write_u8(0x20, 0x20).unwrap();
        bus.update_pit0_irq(bus.device_ticks());
        assert_eq!(
            bus.pic.pending_irq(),
            None,
            "no replay queue or duplicate edge"
        );
        bus.begin_io_instruction(74_005 * PIT_OSCILLATOR_DIVISOR, 0x71);
        bus.update_pit0_irq(bus.device_ticks());
        assert_eq!(bus.pic.pending_irq(), Some(0));
    }

    #[test]
    fn pit_refresh_status_is_read_only_and_follows_channel_one_edges() {
        let mut bus = HostBus::new(0x1000);
        bus.io_write_u8(0x43, 0x54).unwrap(); // channel 1, mode 2, LSB
        bus.io_write_u8(0x41, 4).unwrap();
        bus.io_write_u8(0x61, 0xff).unwrap();
        let before = bus.io_read_u8(0x61).unwrap();
        assert_eq!(before & 0xcf, 0x0f); // parity/error bits cannot be written
        bus.begin_io_instruction(5 * PIT_OSCILLATOR_DIVISOR, 0x71);
        assert_eq!(bus.io_read_u8(0x61).unwrap() ^ before, 0x10);
        assert_eq!(bus.io_read_u8(0x61).unwrap() ^ before, 0x10);
        bus.begin_io_instruction(9 * PIT_OSCILLATOR_DIVISOR, 0x71);
        assert_eq!(bus.io_read_u8(0x61).unwrap(), before);
    }

    #[test]
    fn pending_irq_is_rescanned_on_interruptibility_transition_not_every_instruction() {
        let mut bus = HostBus::new(0x4000);
        let mut cpu = Cpu::new();
        let mut trace = NullTrace;
        let mut perf = PerfCounters::default();

        cpu.eip = 0x1000;
        cpu.set_reg(Reg32::Esp, 0x2000);
        cpu.eflags &= !EFLAGS_IF;
        cpu.idtr.limit = 0x03ff;
        bus.ram[0x1000..0x1003].copy_from_slice(&[0x90, 0xfb, 0x90]); // nop; sti; nop
        bus.ram[0x0080..0x0084].copy_from_slice(&[0x00, 0x30, 0x00, 0x00]);
        bus.ram[0x3000] = 0x90;
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.pic.pulse(0);
        bus.attention = true;

        assert!(matches!(
            step_guest_once(&mut cpu, &mut bus, &mut trace, &mut perf, 0).unwrap(),
            GuestStepStatus::Continue
        ));
        assert_eq!(perf.external_irq_checks, 1);
        assert_eq!(bus.pic.pending_irq(), Some(0));
        assert!(!bus.attention);

        assert!(matches!(
            step_guest_once(&mut cpu, &mut bus, &mut trace, &mut perf, 1).unwrap(),
            GuestStepStatus::Continue
        ));
        assert_eq!(perf.external_irq_checks, 1);
        assert!(cpu.interrupt_shadow());

        assert!(matches!(
            step_guest_once(&mut cpu, &mut bus, &mut trace, &mut perf, 2).unwrap(),
            GuestStepStatus::Continue
        ));
        assert_eq!(perf.external_irq_checks, 2);
        assert_eq!(perf.full_device_scans, 1);
        assert_eq!(bus.pic.pending_irq(), None);
        assert_eq!(cpu.eip, 0x3000);
    }

    #[test]
    fn latched_irq_waits_for_one_instruction_after_sti_or_ss_load() {
        for prefix in [&[0xfb][..], &[0x8e, 0xd0][..], &[0x17][..]] {
            for trace in [TraceMode::Off, TraceMode::Minimal] {
                for perf in [PerfMode::Disabled, PerfMode::Enabled] {
                    for shadow_budget in [1, 2] {
                        let mut bus = HostBus::new(0x4000);
                        let mut cpu = Cpu::new();
                        cpu.eip = 0x1000;
                        cpu.set_reg(Reg32::Esp, 0x2000);
                        if prefix != [0xfb] {
                            cpu.eflags |= EFLAGS_IF;
                        }
                        cpu.idtr.limit = 0x03ff;
                        bus.ram[0x1000..0x1000 + prefix.len()].copy_from_slice(prefix);
                        bus.ram[0x1000 + prefix.len()..0x1002 + prefix.len()]
                            .copy_from_slice(&[0x43, 0x41]); // inc bx; inc cx
                        bus.ram[0x0080..0x0084].copy_from_slice(&[0x00, 0x30, 0x00, 0x00]);
                        bus.ram[0x3000] = 0xf4; // IRQ handler: hlt
                        bus.io_write_u8(0x21, 0xfe).unwrap();
                        bus.pic.pulse(0);
                        // The PIC is already latched; no bus attention is needed
                        // to find it when the shadow expires.
                        bus.refresh_interrupt_attention(0);
                        let mut m = Machine::from_parts(cpu, bus, trace, perf);
                        assert_eq!(m.run_steps(1).unwrap().ran, 1);
                        assert!(m.cpu.interrupt_shadow());
                        assert_eq!(m.cpu.eip, 0x1000 + prefix.len() as u32);
                        assert_eq!(m.run_steps(shadow_budget).unwrap().ran, shadow_budget);
                        if shadow_budget == 1 {
                            assert_eq!(m.cpu.eip, 0x3000);
                            assert_eq!(m.cpu.tsc(), 2);
                            assert!(!m.cpu.halted);
                            assert_eq!(m.run_steps(1).unwrap().ran, 1);
                        }
                        assert_eq!(m.cpu.eip, 0x3001);
                        assert!(m.cpu.halted);
                        assert_eq!(m.cpu.reg(Reg32::Ebx), 1);
                        assert_eq!(m.cpu.reg(Reg32::Ecx), 0);
                        let sp = m.cpu.reg(Reg32::Esp) as usize;
                        let return_ip =
                            u16::from_le_bytes(m.bus.ram[sp..sp + 2].try_into().unwrap());
                        assert_eq!(return_ip, 0x1001 + prefix.len() as u16);
                        assert_eq!(m.cpu.tsc(), 3);
                        assert_eq!(m.bus.pic.pending_irq(), None);
                    }
                }
            }
        }
    }

    #[test]
    fn quiet_shadow_runs_the_following_instruction_in_the_same_batch() {
        for prefix in [&[0xfb][..], &[0x8e, 0xd0][..], &[0x17][..]] {
            for following in [&[0x90, 0x43][..], &[0xf4], &[0xe6, 0x80], &[0x0f, 0xff]] {
                let mut cpu = Cpu::new();
                cpu.eip = 0x1000;
                cpu.set_reg(Reg32::Esp, 0x3000);
                if prefix != [0xfb] {
                    cpu.eflags |= EFLAGS_IF;
                }
                let mut bus = HostBus::new(0x4000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                bus.ram[0x1000..0x1000 + prefix.len()].copy_from_slice(prefix);
                bus.ram[0x1000 + prefix.len()..0x1000 + prefix.len() + following.len()]
                    .copy_from_slice(following);
                let batch = Interpreter::run_batch(&mut cpu, &mut bus, 0, 3);
                let ran = if following[0] == 0x90 { 3 } else { 2 };
                assert_eq!(batch.ran, ran);
                assert_eq!(cpu.tsc(), ran);
                assert!(!cpu.interrupt_shadow());
                match following[0] {
                    0x90 => {
                        assert_eq!(batch.stop, BatchStop::Budget);
                        assert_eq!(cpu.reg(Reg32::Ebx), 1);
                    }
                    0xf4 => assert!(matches!(
                        batch.stop,
                        BatchStop::Step {
                            instruction_step: 1,
                            reason: BatchStepStop::Halted,
                            ..
                        }
                    )),
                    0xe6 => assert!(matches!(
                        batch.stop,
                        BatchStop::Attention {
                            instruction_step: 1,
                            ..
                        }
                    )),
                    _ => assert!(matches!(batch.stop, BatchStop::Step {
                        instruction_step: 1, instruction_eip, reason: BatchStepStop::Error(_), ..
                    } if instruction_eip == 0x1000 + prefix.len() as u32)),
                }
            }
        }
    }

    #[test]
    fn shadow_stays_visible_when_the_next_instruction_cannot_run_quietly() {
        for prefix in [&[0xfb][..], &[0x8e, 0xd0][..], &[0x17][..]] {
            // Short caller budget, due event, unserviced bus change, pending PIC.
            for guard in 0..4 {
                let mut cpu = Cpu::new();
                cpu.eip = 0x1000;
                cpu.set_reg(Reg32::Esp, 0x3000);
                if prefix != [0xfb] {
                    cpu.eflags |= EFLAGS_IF;
                }
                let mut bus = HostBus::new(0x4000);
                bus.ram[0x1000..0x1000 + prefix.len()].copy_from_slice(prefix);
                bus.ram[0x1000 + prefix.len()] = 0x43;
                bus.io_write_u8(0x21, 0xfe).unwrap();
                if guard == 3 {
                    bus.pic.pulse(0);
                }
                bus.attention = guard == 2;
                bus.next_attention_step = if guard == 1 { 0 } else { u64::MAX };
                let budget = if guard == 0 { 1 } else { 3 };
                let batch = Interpreter::run_batch(&mut cpu, &mut bus, 0, budget);
                assert_eq!(batch.ran, 1);
                assert!(matches!(
                    batch.stop,
                    BatchStop::Attention {
                        instruction_step: 0,
                        ..
                    }
                ));
                assert!(cpu.interrupt_shadow());
                assert_eq!(cpu.reg(Reg32::Ebx), 0);
                assert_eq!(cpu.tsc(), 1);
            }
        }
    }

    #[test]
    fn quiet_sti_delivers_a_timer_irq_after_the_shadowed_instruction() {
        let mut cpu = Cpu::new();
        cpu.eip = 0x1000;
        cpu.set_reg(Reg32::Esp, 0x3000);
        cpu.idtr.limit = 0x03ff;
        let mut bus = HostBus::new(0x4000);
        bus.ram[0x1000..0x1003].copy_from_slice(&[0xfb, 0x43, 0x41]); // sti; inc bx; inc cx
        bus.ram[0x80..0x84].copy_from_slice(&[0, 0x20, 0, 0]);
        bus.ram[0x2000] = 0xf4;
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.io_write_u8(0x43, 0x10).unwrap();
        bus.io_write_u8(0x40, 3).unwrap();
        bus.refresh_interrupt_attention(0);
        let start = bus.next_attention_step - 1;
        assert!(bus.batch_interrupts_quiet());
        cpu.set_tsc(start);
        let mut m = Machine::from_parts(cpu, bus, TraceMode::Off, PerfMode::Disabled);
        m.step = start;
        assert_eq!(m.run_steps(3).unwrap().ran, 3);
        assert_eq!(m.cpu.eip, 0x2001);
        assert!(m.cpu.halted);
        assert_eq!(m.cpu.reg(Reg32::Ebx), 1);
        assert_eq!(m.cpu.reg(Reg32::Ecx), 0);
        let sp = m.cpu.reg(Reg32::Esp) as usize;
        assert_eq!(&m.bus.ram[sp..sp + 2], &[0x02, 0x10]);
        assert_eq!(m.cpu.tsc(), start + 3);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_quiet_sti_continues_to_normal_halt_io_and_fault_boundaries() {
        for following in [&[0x90, 0x43][..], &[0xf4], &[0xe6, 0x80], &[0x0f, 0xff]] {
            let mut bus = HostBus::new(0x10000);
            bus.ram[0x1000] = 0xfb;
            bus.ram[0x1001..0x1001 + following.len()].copy_from_slice(following);
            let mut engine = crate::x86::jit::runtime::Engine::new().unwrap();
            for round in 0..6 {
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0x1000, 0x8000);
                bus.attention = false;
                bus.next_attention_step = u64::MAX;
                let native_before = engine.stats.native_steps;
                let expected = if following[0] == 0x90 { 3 } else { 2 };
                let mut ran = 0;
                let mut calls = 0;
                let batch = loop {
                    let batch = engine.run_batch(&mut cpu, &mut bus, ran, 3 - ran).unwrap();
                    ran += batch.ran;
                    calls += 1;
                    if ran == expected {
                        break batch;
                    }
                    // Cold interpretation can retain STI and shadow-expiry
                    // exits; the compiled path below must stay in one call.
                    assert!(matches!(batch.stop, BatchStop::Attention { .. }));
                    assert_eq!(cpu.interrupt_shadow(), ran == 1);
                };
                assert_eq!(cpu.tsc(), expected);
                assert!(!cpu.interrupt_shadow());
                match following[0] {
                    0x90 => {
                        assert_eq!(batch.stop, BatchStop::Budget);
                        assert_eq!(cpu.reg(Reg32::Ebx), 1);
                    }
                    0xf4 => assert!(matches!(
                        batch.stop,
                        BatchStop::Step {
                            instruction_step: 1,
                            reason: BatchStepStop::Halted,
                            ..
                        }
                    )),
                    0xe6 => assert!(matches!(
                        batch.stop,
                        BatchStop::Attention {
                            instruction_step: 1,
                            ..
                        }
                    )),
                    _ => assert!(matches!(
                        batch.stop,
                        BatchStop::Step {
                            instruction_step: 1,
                            instruction_eip: 0x1001,
                            reason: BatchStepStop::Error(_),
                            ..
                        }
                    )),
                }
                if round == 5 {
                    assert_eq!(calls, 1);
                    assert!(engine.stats.native_steps > native_before);
                }
            }
        }
    }

    #[test]
    fn interrupt_device_deadline_tracks_io_waits_and_timer_reprogramming() {
        let mut bus = HostBus::new(0x4000);
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        let mut perf = PerfCounters::default();
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.io_write_u8(0x43, 0x10).unwrap(); // one-shot, LSB-only
        bus.io_write_u8(0x40, 20).unwrap();
        assert_eq!(
            poll_pending_external_irq(0, &cpu, &mut bus, &mut perf),
            None
        );

        // An early IF transition neither raises an IRQ nor advances devices.
        assert_eq!(
            poll_pending_external_irq(1, &cpu, &mut bus, &mut perf),
            None
        );
        assert_eq!(perf.full_device_scans, 1);
        bus.clock.io_wait(2 * PIT_OSCILLATOR_DIVISOR);
        assert_eq!(
            poll_pending_external_irq(1, &cpu, &mut bus, &mut perf),
            None
        );
        let original_due = 19 * PIT_OSCILLATOR_DIVISOR;
        assert_eq!(bus.next_attention_step, original_due);

        // Reprogramming must replace the cached deadline even before it is due.
        bus.io_write_u8(0x40, 3).unwrap();
        assert_eq!(
            poll_pending_external_irq(1, &cpu, &mut bus, &mut perf),
            None
        );
        let due = 4 * PIT_OSCILLATOR_DIVISOR; // PIT load is on the next clock
        assert!(due < original_due);
        assert_eq!(
            poll_pending_external_irq(due - 1, &cpu, &mut bus, &mut perf),
            None
        );
        assert_eq!(
            poll_pending_external_irq(due, &cpu, &mut bus, &mut perf),
            Some(0)
        );
        assert_eq!(perf.full_device_scans, 3);
    }

    #[cfg(feature = "host")]
    #[test]
    fn realtime_early_irq_polls_do_not_postpone_clock_sampling() {
        let mut bus = HostBus::new(0x4000);
        bus.clock = Clock::realtime();
        let cpu = Cpu::new();
        let mut perf = PerfCounters::default();
        poll_pending_external_irq(0, &cpu, &mut bus, &mut perf);
        let deadline = bus.next_attention_step;
        for step in [1, deadline / 2, deadline - 1] {
            bus.set_attention(); // e.g. an EOI, even while IF is clear
            poll_pending_external_irq(step, &cpu, &mut bus, &mut perf);
            assert_eq!(bus.next_attention_step, deadline);
        }
        poll_pending_external_irq(deadline, &cpu, &mut bus, &mut perf);
        assert_eq!(bus.next_attention_step, 2 * deadline);
    }

    #[test]
    fn rtc_rate_thirteen_matches_125ms_on_emulated_pit_clock() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.cmos_status_a = 0x2d;

        assert_eq!(bus.rtc_period_ticks(), TICKS_PER_SECOND / 8);
    }

    #[test]
    #[cfg(feature = "host")]
    fn rtc_clock_starts_at_host_local_date() {
        use chrono::{Datelike, Timelike};

        let fields = |time: chrono::DateTime<chrono::Local>| {
            [
                (time.year() / 100) as u8,
                (time.year() % 100) as u8,
                time.month() as u8,
                time.day() as u8,
                time.hour() as u8,
                time.minute() as u8,
                time.second() as u8,
            ]
        };
        let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Vga).unwrap();
        let before = fields(chrono::Local::now());
        machine.use_realtime_clock().unwrap();
        machine.bus.io_write_u8(0x70, CMOS_REG_B).unwrap();
        machine.bus.io_write_u8(0x71, 0x86).unwrap(); // SET, binary, 24-hour
        let actual = [0x32, 9, 8, 7, 4, 2, 0].map(|index| {
            machine.bus.io_write_u8(0x70, index).unwrap();
            machine.bus.io_read_u8(0x71).unwrap()
        });
        let after = fields(chrono::Local::now());
        assert!(
            before <= actual && actual <= after,
            "CMOS {actual:?} is outside host local interval {before:?}..={after:?}"
        );
    }

    #[test]
    fn rtc_clock_advances_from_deterministic_guest_steps() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.cmos_status_b = 0x06;

        bus.current_step = TICKS_PER_SECOND - 1;
        bus.cmos_index = 0x00;
        assert_eq!(bus.cmos_read(), 0);

        bus.current_step = TICKS_PER_SECOND;
        assert_eq!(bus.cmos_read(), 1);

        bus.current_step = TICKS_PER_SECOND * (24 * 60 * 60 + 62);
        bus.cmos_index = 0x00;
        assert_eq!(bus.cmos_read(), 2);
        bus.cmos_index = 0x02;
        assert_eq!(bus.cmos_read(), 1);
        bus.cmos_index = 0x07;
        assert_eq!(bus.cmos_read(), 2);
    }

    #[test]
    fn rtc_clock_encodes_bcd_and_century_when_binary_mode_is_clear() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.cmos_status_b = 0x02;
        bus.current_step = TICKS_PER_SECOND * (12 * 60 * 60 + 34 * 60 + 56);

        bus.cmos_index = 0x00;
        assert_eq!(bus.cmos_read(), 0x56);
        bus.cmos_index = 0x02;
        assert_eq!(bus.cmos_read(), 0x34);
        bus.cmos_index = 0x04;
        assert_eq!(bus.cmos_read(), 0x12);
        bus.cmos_index = 0x09;
        assert_eq!(bus.cmos_read(), 0x26);
        bus.cmos_index = 0x32;
        assert_eq!(bus.cmos_read(), 0x20);
    }

    #[test]
    fn rtc_set_freezes_registers_and_guest_date_survives_reset() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        let write = |bus: &mut HostBus, index, value| {
            bus.io_write_u8(0x70, index).unwrap();
            bus.io_write_u8(0x71, value).unwrap();
        };
        write(&mut bus, CMOS_REG_B, 0x82); // SET, BCD, 24-hour
        for (index, value) in [
            (0, 0x59),
            (2, 0x59),
            (4, 0x23),
            (7, 0x28),
            (8, 0x02),
            (9, 0),
            (0x32, 0x20),
        ] {
            write(&mut bus, index, value);
        }
        bus.current_step = TICKS_PER_SECOND * 10;
        bus.cmos_index = 0;
        assert_eq!(bus.cmos_read(), 0x59, "SET must freeze clock updates");
        write(&mut bus, CMOS_REG_B, 2);
        bus.current_step += TICKS_PER_SECOND * 2;
        let time = bus.rtc_calendar();
        assert_eq!(
            (
                time.year,
                time.month,
                time.day,
                time.hour,
                time.minute,
                time.second
            ),
            (2000, 2, 29, 0, 0, 1)
        );
        bus.reset(bus.current_step);
        let time = bus.rtc_calendar();
        assert_eq!(
            (time.year, time.month, time.day, time.second),
            (2000, 2, 29, 1)
        );
    }

    #[test]
    fn pit_channel_two_terminal_count_depends_on_time_not_poll_count() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.io_write_u8(0x61, 0).unwrap();
        bus.io_write_u8(0x43, 0xb0).unwrap(); // channel 2, L/H, mode 0
        bus.io_write_u8(0x42, 100).unwrap();
        bus.io_write_u8(0x42, 0).unwrap();
        bus.io_write_u8(0x61, 1).unwrap();
        for _ in 0..8 {
            assert_eq!(bus.io_read_u8(0x61).unwrap() & 0x20, 0);
        }
        bus.begin_io_instruction(101 * PIT_OSCILLATOR_DIVISOR - 1, 0x71);
        assert_eq!(bus.io_read_u8(0x61).unwrap() & 0x20, 0);
        bus.begin_io_instruction(101 * PIT_OSCILLATOR_DIVISOR, 0x71);
        assert_eq!(bus.io_read_u8(0x61).unwrap() & 0x20, 0x20);
    }

    #[test]
    fn rtc_periodic_irq_uses_cmos_rate_not_irq0_period() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.current_step = 10;
        bus.cmos_index = CMOS_REG_A;
        bus.cmos_write(0x2d);
        bus.cmos_index = CMOS_REG_B;
        bus.cmos_write(CMOS_REG_B_PIE);

        let due_step = 10 + TICKS_PER_SECOND / 8;
        assert_eq!(bus.rtc_next_tick, due_step);
        bus.update_rtc(due_step - 1);
        assert!(!bus.rtc_irq8);

        bus.update_rtc(due_step);
        assert!(bus.rtc_irq8);
        assert_eq!(bus.cmos_status_c & 0xc0, 0xc0);
    }

    #[test]
    fn rtc_alarm_and_update_survive_delayed_service_and_rate_zero() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        let write = |bus: &mut HostBus, index, value| {
            bus.io_write_u8(0x70, index).unwrap();
            bus.io_write_u8(0x71, value).unwrap();
        };
        write(&mut bus, CMOS_REG_A, 0x20); // no periodic source
        write(&mut bus, 1, 1); // alarm at 00:00:01
        write(&mut bus, 3, 0);
        write(&mut bus, 5, 0);
        write(&mut bus, CMOS_REG_B, 0x32); // AIE, UIE, BCD, 24h
        assert_eq!(bus.next_device_event(), TICKS_PER_SECOND);
        // A host pause crosses the alarm. It must still be latched on resume.
        bus.begin_io_instruction(3 * TICKS_PER_SECOND, 0x71);
        bus.cmos_index = CMOS_REG_C;
        assert_eq!(bus.cmos_read(), 0xb0);
        assert!(!bus.pic.line(8));
        assert_eq!(bus.cmos_read(), 0);
        write(&mut bus, 1, 0xc0); // wildcard seconds, once per second
        bus.begin_io_instruction(4 * TICKS_PER_SECOND, 0x71);
        bus.cmos_index = CMOS_REG_C;
        assert_eq!(bus.cmos_read(), 0xb0);
        write(&mut bus, CMOS_REG_B, 0x82); // SET clears UIE and stops updates
        bus.begin_io_instruction(8 * TICKS_PER_SECOND, 0x71);
        bus.cmos_index = 0;
        assert_eq!(bus.cmos_read(), 4);
        bus.cmos_index = CMOS_REG_C;
        assert_eq!(bus.cmos_read(), 0);
    }

    #[test]
    fn rtc_sources_latch_independently_and_masks_only_control_irq() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.cmos_index = CMOS_REG_A;
        bus.cmos_write(0x2d); // 8 Hz
        for index in [1, 3, 5] {
            bus.cmos_index = index;
            bus.cmos_write(0xc0);
        }
        bus.begin_io_instruction(TICKS_PER_SECOND, 0x71);
        bus.cmos_index = CMOS_REG_C;
        assert_eq!(bus.cmos_read(), 0x70, "masked events remain observable");
        assert!(!bus.pic.line(8));
        bus.begin_io_instruction(2 * TICKS_PER_SECOND, 0x71);
        bus.cmos_index = CMOS_REG_B;
        bus.cmos_write(0x72);
        assert!(bus.pic.line(8), "unmasking a latched event raises IRQ8");
        bus.cmos_write(0x32); // disabling PIE leaves the other sources active
        assert!(bus.pic.line(8));
        bus.cmos_index = CMOS_REG_C;
        assert_eq!(bus.cmos_read(), 0xf0);
        assert!(!bus.pic.line(8));
    }

    #[test]
    fn rtc_uip_depends_on_time_and_set_not_port_reads() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.begin_io_instruction(TICKS_PER_SECOND - TICKS_PER_SECOND / 2000, 0x71);
        bus.cmos_index = CMOS_REG_A;
        for _ in 0..4 {
            assert_ne!(bus.cmos_read() & 0x80, 0);
        }
        bus.cmos_index = CMOS_REG_B;
        bus.cmos_write(0x92); // SET also clears UIE
        assert_eq!(bus.cmos_read() & 0x10, 0);
        bus.cmos_index = CMOS_REG_A;
        assert_eq!(bus.cmos_read() & 0x80, 0);
        bus.cmos_index = CMOS_REG_B;
        bus.cmos_write(2);
        bus.begin_io_instruction(2 * TICKS_PER_SECOND, 0x71);
        bus.cmos_index = 0;
        assert_eq!(bus.cmos_read(), 1);
    }

    #[test]
    fn serial_input_and_io_writes_raise_attention() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.refresh_interrupt_attention(0);
        assert!(!bus.attention);

        bus.queue_serial_input(b"x");
        assert!(bus.attention);

        bus.refresh_interrupt_attention(0);
        bus.attention = false;
        bus.io_write_u8(0x21, 0xfe).unwrap();
        assert!(bus.attention);
    }

    #[test]
    fn software_pte_stores_preserve_cached_mapping_until_invalidation() {
        let mut bus = HostBus::new(0x10000);
        for (address, value) in [
            (0x1000, 0x2000 | 7),
            (0x2000, 0x3000 | 7),
            (0x2008, 0x2000 | 7),
        ] {
            write_ram_u32(&mut bus, address, value);
        }
        bus.ram[0x3000] = 0x5a;
        bus.ram[0x6000] = 0x6b;
        let mut cpu = paging_cpu(0x1000);
        cpu.cr0 |= crate::x86::cpu::CR0_WP;
        assert_eq!(
            mmu::read_linear_u8(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read).unwrap(),
            0x5a
        );
        // Change the PTE through its ordinary writable RAM alias. Both the
        // cached physical page and cached permissions survive until explicit
        // invalidation, as checked independently by tlb_lifetime.asm.
        mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x2000), 0x6000 | 7).unwrap();
        assert_eq!(
            mmu::read_linear_u8(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read).unwrap(),
            0x5a
        );
        cpu.invalidate_translation_context();
        assert_eq!(
            mmu::read_linear_u8(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read).unwrap(),
            0x6b
        );
        mmu::write_linear_u8(&mut cpu, &mut bus, LinearAddr(0), 0x6c).unwrap();
        mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x2000), 0x6000 | 5).unwrap();
        mmu::write_linear_u8(&mut cpu, &mut bus, LinearAddr(0), 0x6d).unwrap();
        assert_eq!(bus.ram[0x6000], 0x6d);
        cpu.invalidate_translation_context();
        assert!(mmu::write_linear_u8(&mut cpu, &mut bus, LinearAddr(0), 0x6e).is_err());
        assert_eq!(bus.ram[0x6000], 0x6d);
        assert_eq!(bus.ram[0x3000], 0x5a);
    }

    #[test]
    fn software_page_walk_sets_accessed_without_setting_dirty() {
        let mut bus = HostBus::new(0x5000);
        write_ram_u32(
            &mut bus,
            0x1000,
            0x2000 | PTE_PRESENT | PTE_WRITE | PTE_USER,
        );
        write_ram_u32(
            &mut bus,
            0x2000,
            0x3000 | PTE_PRESENT | PTE_WRITE | PTE_USER,
        );
        bus.ram[0x3000] = 0x5a;

        let mut cpu = paging_cpu(0x1000);
        let value =
            mmu::read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 3)
                .unwrap();

        assert_eq!(value, 0x5a);
        assert_eq!(
            u32::from_le_bytes(bus.ram[0x1000..0x1004].try_into().unwrap()),
            0x2000 | PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED
        );
        assert_eq!(
            u32::from_le_bytes(bus.ram[0x2000..0x2004].try_into().unwrap()),
            0x3000 | PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED
        );
        assert_eq!(bus.tracked_writes.page_walk, 2);
    }

    #[test]
    fn cpu_linear_write_records_cpu_origin_without_ad_noise() {
        let mut bus = HostBus::new(0x5000);
        write_ram_u32(
            &mut bus,
            0x1000,
            0x2000 | PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
        );
        write_ram_u32(
            &mut bus,
            0x2000,
            0x3000 | PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED | PTE_DIRTY,
        );

        let mut cpu = paging_cpu(0x1000);
        mmu::write_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x0000_0123), 0xc7, 3).unwrap();

        assert_eq!(bus.ram[0x3123], 0xc7);
        assert_eq!(bus.tracked_writes.cpu, 1);
        assert_eq!(bus.tracked_writes.page_walk, 0);
        assert_eq!(bus.tracked_writes.page_table_frame, 0);
    }

    #[test]
    fn pic_initialization_words_are_not_interpreted_as_masks() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x20, 0x11).unwrap();
        bus.io_write_u8(0xa0, 0x11).unwrap();
        bus.io_write_u8(0x21, 0x20).unwrap();
        bus.io_write_u8(0xa1, 0x28).unwrap();
        bus.io_write_u8(0x21, 0x04).unwrap();
        bus.io_write_u8(0xa1, 0x02).unwrap();
        bus.io_write_u8(0x21, 0x01).unwrap();
        bus.io_write_u8(0xa1, 0x01).unwrap();

        assert_eq!(bus.pic.vector_base(false), 0x20);
        assert_eq!(bus.pic.vector_base(true), 0x28);
        assert_eq!(bus.pic.mask(false), 0);
        assert_eq!(bus.pic.mask(true), 0);

        bus.io_write_u8(0x21, 0xfe).unwrap();
        assert_eq!(bus.pic.mask(false), 0xfe);
    }

    #[test]
    fn pic_eoi_reenables_in_service_irq0() {
        let mut bus = HostBus::new(1024 * 1024);
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        bus.io_write_u8(0x21, 0xfe).unwrap();
        bus.pic.pulse(0);
        assert_eq!(bus.pic.acknowledge(), 0x20);
        bus.pic.pulse(0);

        assert_ne!(pending_external_irq(&cpu, &bus), Some(0));

        bus.io_write_u8(0x20, 0x20).unwrap();

        assert_eq!(bus.pic.in_service(false) & 1, 0);
        assert_eq!(pending_external_irq(&cpu, &bus), Some(0));
    }

    #[test]
    fn ps2_polled_command_reply_while_inhibited_does_not_latch_irq1() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u8(0x21, 0xff).unwrap();
        bus.io_write_u8(0x64, 0x60).unwrap();
        bus.io_write_u8(0x60, 0x71).unwrap(); // IRQ1 enabled, keyboard inhibited.
        bus.io_write_u8(0x64, 0x20).unwrap();
        assert_eq!(bus.io_read_u8(0x64).unwrap() & 1, 1);
        assert_eq!(bus.io_read_u8(0x60).unwrap(), 0x71);
        assert_eq!(bus.io_read_u8(0x64).unwrap() & 1, 0);
        bus.io_write_u8(0x64, 0xae).unwrap(); // Reopen the now-empty keyboard link.
        bus.io_write_u8(0x21, !0x06).unwrap(); // Keyboard and slave cascade.
        assert_eq!(bus.pic.pending_irq(), None);

        bus.ps2.inject_key(Ps2Key::A, true);
        bus.drain_ps2_effects();
        assert_eq!(bus.pic.pending_irq(), Some(1));
        assert_eq!(bus.pic.acknowledge(), 0x21);
        assert_eq!(bus.io_read_u8(0x60).unwrap(), 0x1e);
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_eq!(bus.pic.pending_irq(), None);

        // Auxiliary controller replies retain their IRQ while links are inhibited.
        bus.io_write_u8(0xa1, !0x10).unwrap();
        bus.io_write_u8(0x64, 0x60).unwrap();
        bus.io_write_u8(0x60, 0x73).unwrap();
        bus.io_write_u8(0x64, 0xd3).unwrap();
        bus.io_write_u8(0x60, 0x5a).unwrap();
        assert_eq!(bus.pic.pending_irq(), Some(12));
        assert_eq!(bus.pic.acknowledge(), 0x2c);
        assert_eq!(bus.io_read_u8(0x60).unwrap(), 0x5a);
    }

    #[test]
    fn ps2_each_output_byte_reasserts_its_edge_triggered_irq() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u8(0x21, !0x06).unwrap(); // Keyboard and slave cascade.
        bus.io_write_u8(0xa1, !0x10).unwrap(); // Auxiliary device.
        bus.ps2
            .inject_scancode_set2(&[0xe0, 0x72, 0xe0, 0xf0, 0x72]);
        bus.drain_ps2_effects();
        for byte in [0xe0, 0x50, 0xe0, 0xd0] {
            assert_eq!(bus.pic.pending_irq(), Some(1));
            assert_eq!(bus.pic.acknowledge(), 0x21);
            assert_eq!(bus.io_read_u8(0x60).unwrap(), byte);
            bus.io_write_u8(0x20, 0x20).unwrap();
        }
        assert_eq!(bus.pic.pending_irq(), None);
        assert_eq!(bus.io_read_u8(0x64).unwrap() & 1, 0);

        // The same OBF handshake applies to every byte of a mouse reply.
        bus.io_write_u8(0x64, 0xd4).unwrap();
        bus.io_write_u8(0x60, 0xf2).unwrap();
        for byte in [0xfa, 0x00] {
            assert_eq!(bus.pic.pending_irq(), Some(12));
            assert_eq!(bus.pic.acknowledge(), 0x2c);
            assert_eq!(bus.io_read_u8(0x60).unwrap(), byte);
            bus.io_write_u8(0xa0, 0x20).unwrap();
            bus.io_write_u8(0x20, 0x20).unwrap();
        }
        assert_eq!(bus.pic.pending_irq(), None);
        assert_eq!(bus.io_read_u8(0x64).unwrap() & 0x21, 0);
    }

    #[test]
    fn pic_irq15_isr_read_distinguishes_real_and_spurious_completions() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        let mut cpu = Cpu::new();
        cpu.idtr.limit = 0x03ff;
        cpu.set_reg(Reg32::Esp, 0x8000);
        bus.attach_secondary_cdrom(vec![0xaa; 2048]);
        bus.io_write_u8(0x21, !(1 << 2)).unwrap();
        bus.io_write_u8(0xa1, !(1 << 7)).unwrap();

        for _ in 0..2 {
            cpu.eflags |= EFLAGS_IF;
            bus.io_write_u8(0x177, 0xa1).unwrap();
            assert_eq!(pending_external_irq(&cpu, &bus), Some(15));
            bus.io_write_u8(0xa0, 0x0a).unwrap();
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0x80); // IRR
            assert_eq!(bus.io_read_u8(0x20).unwrap() & 4, 4); // cascade
            deliver_irq(&mut cpu, &mut bus, 0).unwrap();
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0); // request acknowledged

            bus.io_write_u8(0xa0, 0x0b).unwrap();
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0x80); // real IRQ15
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0x80); // read is not EOI
            bus.io_write_u8(0xa0, 0x08).unwrap(); // RR=0 retains selection
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0x80);
            // An OCW3 with ESMM/SMM set is not an OCW2 EOI.
            bus.io_write_u8(0xa0, 0x68).unwrap();
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0x80);

            bus.io_read_u8(0x177).unwrap(); // deassert device INTRQ
            bus.io_write_u8(0xa0, 0x67).unwrap(); // slave specific EOI
            assert_eq!(bus.io_read_u8(0xa0).unwrap(), 0); // now spurious
            bus.io_write_u8(0x20, 0x0b).unwrap();
            assert_eq!(bus.io_read_u8(0x20).unwrap(), 4);
            bus.io_write_u8(0x20, 0x62).unwrap(); // master cascade EOI
            assert_eq!(bus.io_read_u8(0x20).unwrap(), 0);
            bus.io_write_u8(0x20, 0x0a).unwrap();
        }
    }

    #[test]
    fn ide_irq_edge_latches_until_pic_ack_without_level_redelivery() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        bus.io_write_u8(0x21, !(1 << 2)).unwrap();
        bus.io_write_u8(0xa1, !(1 << 6)).unwrap();
        bus.attach_primary_cdrom(vec![0xaa; 4 * 2048]);

        bus.io_write_u8(0x1f7, 0xa1).unwrap(); // IDENTIFY PACKET
        assert_eq!(pending_external_irq(&cpu, &bus), Some(14));

        let _ = bus.io_read_u8(0x1f7).unwrap(); // Device line drops; PIC IRR remains.
        assert!(!bus.ide.irq_asserted(IdeIrq::Primary));
        assert_eq!(pending_external_irq(&cpu, &bus), Some(14));

        assert_eq!(bus.pic.acknowledge(), 0x2e);
        bus.io_write_u8(0xa0, 0x20).unwrap();
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_ne!(pending_external_irq(&cpu, &bus), Some(14));

        bus.io_write_u8(0x1f7, 0xa1).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), Some(14));
        assert_eq!(bus.pic.acknowledge(), 0x2e);
        bus.io_write_u8(0xa0, 0x20).unwrap();
        bus.io_write_u8(0x20, 0x20).unwrap();
        let _ = bus.io_read_u8(0x3f6).unwrap(); // Alternate status leaves the line high.
        assert_ne!(pending_external_irq(&cpu, &bus), Some(14));

        let _ = bus.io_read_u8(0x1f7).unwrap(); // Command-block status drops the line.
        assert!(!bus.ide.irq_asserted(IdeIrq::Primary));
        bus.io_write_u8(0x1f7, 0xa1).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), Some(14));
    }

    #[test]
    fn ide_nien_masks_the_pin_without_acknowledging_completion() {
        for (status, control, irq, command) in [(0x1f7, 0x3f6, 14, 0xec), (0x177, 0x376, 15, 0xa1)]
        {
            let mut bus = HostBus::new_firmware(2 << 20).unwrap();
            bus.attach_hdd(vec![0; 512], true);
            bus.attach_secondary_cdrom(vec![0; 2048]);
            let mut cpu = Cpu::new();
            cpu.eflags |= EFLAGS_IF;
            bus.io_write_u8(0x21, !(1 << 2)).unwrap();
            bus.io_write_u8(0xa1, !(1 << (irq - 8))).unwrap();

            bus.io_write_u8(control, 2).unwrap();
            bus.io_write_u8(status, command).unwrap();
            assert_ne!(bus.io_read_u8(control).unwrap() & 8, 0); // alternate status
            assert!(!bus.pic.line(irq));
            assert_eq!(pending_external_irq(&cpu, &bus), None);

            bus.io_write_u8(control, 0).unwrap();
            assert!(bus.pic.line(irq), "unmask a completed command on IRQ{irq}");
            assert_eq!(pending_external_irq(&cpu, &bus), Some(irq));
            bus.io_write_u8(control, 2).unwrap();
            assert!(!bus.pic.line(irq), "nIEN must deassert the pin immediately");
            // Masking the device cannot erase the edge already latched by PIC.
            assert_eq!(pending_external_irq(&cpu, &bus), Some(irq));
            assert_eq!(bus.pic.acknowledge(), 0x20 + irq);
            bus.io_write_u8(0xa0, 0x20).unwrap();
            bus.io_write_u8(0x20, 0x20).unwrap();
            assert_eq!(pending_external_irq(&cpu, &bus), None);

            // No ATA status acknowledgement yet: unmask asserts a new edge.
            bus.io_write_u8(control, 0).unwrap();
            assert_eq!(pending_external_irq(&cpu, &bus), Some(irq));
            assert_eq!(bus.pic.acknowledge(), 0x20 + irq);
            bus.io_read_u8(status).unwrap();
            assert!(!bus.pic.line(irq));
            bus.io_write_u8(0xa0, 0x20).unwrap();
            bus.io_write_u8(0x20, 0x20).unwrap();

            // Polling normal status while masked consumes the device request.
            bus.io_write_u8(control, 2).unwrap();
            bus.io_write_u8(status, command).unwrap();
            bus.io_read_u8(status).unwrap();
            bus.io_write_u8(control, 0).unwrap();
            assert!(!bus.pic.line(irq));
            assert_eq!(pending_external_irq(&cpu, &bus), None);
        }
    }

    #[test]
    fn unsupported_port_widths_leave_byte_devices_untouched() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.attach_hdd(vec![0; 512], false);
        bus.io_write_u8(0x1f7, 0xec).unwrap();
        bus.ps2.inject_key(Ps2Key::A, true);
        bus.drain_ps2_effects();
        // Invalid widths must not clear a status IRQ, drain a key, rewrite PIC
        // masks, or execute only the low byte of a controller command.
        assert_eq!(bus.io_read_u16(0x1f7).unwrap(), 0xffff);
        assert_eq!(bus.io_read_u32(0x1f6).unwrap(), u32::MAX);
        assert!(bus.pic.line(14));
        assert_eq!(bus.io_read_u16(0x60).unwrap(), 0xffff);
        bus.io_write_u32(0x64, 0xfe).unwrap();
        assert!(!bus.reset_requested);
        assert_eq!(bus.io_read_u8(0x60).unwrap(), 0x1e);
        bus.io_write_u8(0x21, 0x5a).unwrap();
        bus.io_write_u16(0x21, 0).unwrap();
        assert_eq!(bus.io_read_u8(0x21).unwrap(), 0x5a);
        bus.io_write_u16(0x1f7, 0xff).unwrap();
        assert_eq!(bus.io_read_u16(0x1f0).unwrap(), 0x0040);
        assert_ne!(bus.io_read_u8(0x1f7).unwrap() & 8, 0);
        assert!(!bus.pic.line(14));
    }

    #[test]
    fn vbe_word_ports_do_not_have_byte_or_dword_aliases() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.io_write_u16(0x1ce, 1).unwrap();
        bus.io_write_u16(0x1cf, 800).unwrap();
        assert_eq!(bus.io_read_u16(0x1cf).unwrap(), 800);
        for port in [0x1ce, 0x1cf, 0x1d0] {
            assert_eq!(bus.io_read_u8(port).unwrap(), 0xff);
            assert_eq!(bus.io_read_u32(port).unwrap(), u32::MAX);
            bus.io_write_u8(port, 0x42).unwrap();
            bus.io_write_u32(port, 0xdead_beef).unwrap();
        }
        assert_eq!(bus.io_read_u16(0x1ce).unwrap(), 1);
        assert_eq!(bus.io_read_u16(0x1cf).unwrap(), 800);
        bus.io_write_u16(0x1d0, 0x1234).unwrap();
        assert_eq!(bus.io_read_u16(0x1cf).unwrap(), 800);
        // Word index/data pairs remain useful on the ordinary VGA registers.
        bus.io_write_u16(0x3c4, 0x0b02).unwrap();
        assert_eq!(bus.io_read_u8(0x3c5).unwrap(), 0x0b);
        bus.io_write_u32(0x3c4, 0x0000_0402).unwrap();
        assert_eq!(bus.io_read_u8(0x3c5).unwrap(), 0x0b);
        // An unsupported fw_cfg width must not consume the data stream.
        bus.io_write_u16(0x510, 0).unwrap();
        assert_eq!(bus.io_read_u8(0x510).unwrap(), 0xff); // Selector is write-only.
        assert_eq!(bus.io_read_u16(0x510).unwrap(), 0xffff);
        assert_eq!(bus.io_read_u32(0x510).unwrap(), u32::MAX);
        assert_eq!(bus.io_read_u16(0x511).unwrap(), 0xffff);
        assert_eq!(bus.io_read_u8(0x511).unwrap(), b'Q');
        bus.io_write_u32(0x510, 1).unwrap();
        assert_eq!(bus.io_read_u8(0x511).unwrap(), b'E');
    }

    #[test]
    fn wide_register_cycles_do_not_spill_across_windows() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
        bus.io_write_u16(0xcfb, 0xff00).unwrap();
        assert_eq!(bus.io_read_u32(0xcf8).unwrap(), 0x8000_1010);
        let bar = bus.io_read_u32(0xcfc).unwrap();
        bus.io_write_u16(0xcff, 0x1234).unwrap();
        assert_eq!(bus.io_read_u16(0xcff).unwrap(), u16::MAX);
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), bar);

        // The byte before the PM block must not let a partial wide write
        // acknowledge a latched power event inside the block.
        bus.acpi.power_button();
        bus.io_write_u32(0x5ff, 0x0001_0000).unwrap();
        assert_eq!(bus.io_read_u16(0x600).unwrap(), 0x100);
        bus.io_write_u16(0x600, 0x100).unwrap();
        assert_eq!(bus.io_read_u16(0x600).unwrap(), 0);
        assert_eq!(bus.io_read_u32(0x608).unwrap(), 0);
        assert_eq!(bus.io_read_u32(0x609).unwrap(), u32::MAX);

        // The bus-master window also claims only complete cycles.
        assert_eq!(bus.io_read_u32(0xc00c).unwrap(), 0);
        assert_eq!(bus.io_read_u32(0xc00d).unwrap(), u32::MAX);
        assert_eq!(bus.io_read_u16(0xc00f).unwrap(), u16::MAX);
    }

    #[test]
    fn pci_config_claims_only_implemented_capabilities() {
        let mut bus = HostBus::new(1024 * 1024);
        for (bdf, id) in [
            (0, 0x1237_8086),
            (0x800, 0x7000_8086),
            (0x900, 0x7010_8086),
            (0x1000, 0x1111_1234),
        ] {
            bus.io_write_u32(0xcf8, 0x8000_0000 | bdf).unwrap();
            assert_eq!(bus.io_read_u32(0xcfc).unwrap(), id);
            bus.io_write_u32(0xcfc, 0).unwrap();
            assert_eq!(bus.io_read_u32(0xcfc).unwrap(), id);
            bus.io_write_u32(0xcf8, 0x8000_0004 | bdf).unwrap();
            let status = bus.io_read_u16(0xcfe).unwrap();
            assert_eq!(status & 0x10, 0, "no capability chain on {bdf:x}");
            bus.io_write_u16(0xcfe, 0xffff).unwrap();
            assert_eq!(bus.io_read_u16(0xcfe).unwrap(), status);
            for offset in [0x34, 0x80, 0xfc] {
                bus.io_write_u32(0xcf8, 0x8000_0000 | bdf | offset).unwrap();
                assert_eq!(
                    bus.io_read_u32(0xcfc).unwrap(),
                    0,
                    "reserved {bdf:x}:{offset:x}"
                );
                bus.io_write_u32(0xcfc, u32::MAX).unwrap();
                assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0);
            }
        }
        for (bdf, command) in [(0x900, 5), (0x1000, 3)] {
            bus.io_write_u32(0xcf8, 0x8000_0004 | bdf).unwrap();
            bus.io_write_u32(0xcfc, u32::MAX).unwrap();
            assert_eq!(bus.io_read_u16(0xcfc).unwrap(), command);
            bus.io_write_u8(0xcfd, 0xff).unwrap();
            assert_eq!(bus.io_read_u16(0xcfc).unwrap(), command);
        }
        bus.io_write_u32(0xcf8, 0x8000_093c).unwrap();
        assert_eq!(bus.io_read_u8(0xcfd).unwrap(), 0); // Fixed ISA IRQ14/15, no INTA.
        for absent in [0x0000_1000, 0x8001_1000, 0x8000_1800, 0x8000_1100] {
            bus.io_write_u32(0xcf8, absent).unwrap();
            bus.io_write_u32(0xcfc, 0).unwrap();
            assert_eq!(bus.io_read_u32(0xcfc).unwrap(), u32::MAX);
        }
    }

    #[test]
    fn pci_bars_decode_the_address_the_guest_reads_back() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u32(0xcf8, 0x8000_0920).unwrap();
        bus.io_write_u32(0xcfc, 0x0010_d001).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0x0010_d001);
        assert_eq!(bus.io_read_u8(0xd002).unwrap(), 0xff); // No truncation alias.
        bus.io_write_u16(0xcfe, 0).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xd001);
        assert_eq!(bus.io_read_u8(0xd002).unwrap(), 0);
        bus.io_write_u32(0xcfc, u32::MAX).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xffff_fff1);
        assert_eq!(bus.io_read_u8(0xd002).unwrap(), 0xff);
        bus.io_write_u16(0xcfc, 0xc001).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xffff_c001);
        assert_eq!(bus.io_read_u8(0xc002).unwrap(), 0xff);
        bus.io_write_u16(0xcfe, 0).unwrap();
        assert_eq!(bus.io_read_u8(0xc002).unwrap(), 0);

        bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
        bus.io_write_u16(0xcfc, 3).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
        bus.io_write_u32(0xcfc, 0xfd00_0000).unwrap();
        bus.write_phys_u8(PhysAddr(0xfd00_0000), 0x5a).unwrap();
        bus.io_write_u32(0xcfc, u32::MAX).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xff00_0008);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xfd00_0000)).unwrap(), 0xff);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xff00_0000)).unwrap(), 0x5a);
        bus.io_write_u8(0xcff, 0xfc).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xfc00_0008);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xfc00_0000)).unwrap(), 0x5a);
    }

    #[test]
    fn pci_ide_decode_controls_preserve_the_transfer_and_pending_irq() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.attach_hdd(vec![0; 512], false);
        bus.attach_secondary_cdrom(vec![0; 2048]);
        bus.io_write_u32(0xcf8, 0x8000_0940).unwrap();
        bus.io_write_u32(0xcfc, 0xffff_ffff).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xf3ff_f3ff);
        bus.io_write_u32(0xcfc, 0x8000_8000).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_0944).unwrap();
        bus.io_write_u32(0xcfc, 0xffff_ffa5).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xa5);
        for (data, control, irq, identify, config_lane) in [
            (0x1f0, 0x3f6, 14, 0xec, 0xcfd),
            (0x170, 0x376, 15, 0xa1, 0xcff),
        ] {
            bus.io_write_u8(data + 7, identify).unwrap();
            assert!(bus.pic.line(irq));
            bus.io_write_u32(0xcf8, 0x8000_0940).unwrap();
            bus.io_write_u8(config_lane, 0).unwrap();
            assert_eq!(bus.io_read_u8(data + 7).unwrap(), 0xff);
            assert_eq!(bus.io_read_u32(data).unwrap(), 0xffff_ffff);
            assert!(bus.pic.line(irq)); // An unclaimed read cannot acknowledge it.
            bus.io_write_u8(control, 4).unwrap(); // Disabled SRST must not reset it.
            assert!(!bus.io_read_to_ram(data, 2, PhysAddr(0x8000), 4).unwrap());
            bus.io_write_u8(config_lane, 0x80).unwrap();
            assert_ne!(bus.io_read_u8(control).unwrap() & 8, 0); // Still DRQ.
            bus.io_read_u8(data + 7).unwrap();
            assert!(!bus.pic.line(irq));
            assert!(bus.io_read_to_ram(data, 2, PhysAddr(0x8000), 4).unwrap());
            assert_eq!(
                u16::from_le_bytes(bus.ram[0x8000..0x8002].try_into().unwrap()),
                if identify == 0xec { 0x0040 } else { 0x85c0 }
            );

            // PCI I/O enable also gates both channels, independently of IDETIM.
            bus.io_write_u32(0xcf8, 0x8000_0904).unwrap();
            bus.io_write_u8(0xcfc, 4).unwrap();
            assert_eq!(bus.io_read_u8(control).unwrap(), 0xff);
            bus.io_write_u8(0xcfc, 5).unwrap();
            assert_ne!(bus.io_read_u8(control).unwrap() & 8, 0);
        }
    }

    #[test]
    fn pci_vga_command_controls_legacy_ports_and_aperture() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
        bus.io_write_u16(0xcfc, 3).unwrap();
        bus.io_write_u16(0x3c4, 0x0202).unwrap(); // Sequencer plane mask = 2.
        assert_eq!(bus.io_read_u8(0x3c5).unwrap(), 2);
        bus.io_write_u16(0xcfc, 2).unwrap(); // Memory stays decoded, I/O stops.
        assert_eq!(bus.io_read_u16(0x3c4).unwrap(), 0xffff);
        bus.io_write_u16(0x3c4, 0x0f02).unwrap(); // Must not modify sequencer.
        bus.io_write_u16(0xcfc, 3).unwrap();
        assert_eq!(bus.io_read_u8(0x3c5).unwrap(), 2);
        bus.io_write_u16(0x3c4, 0x0f02).unwrap();
        bus.write_phys_u8(PhysAddr(0xb8000), b'V').unwrap();
        assert_eq!(bus.read_phys_u8(PhysAddr(0xb8000)).unwrap(), b'V');
        bus.io_write_u16(0xcfc, 1).unwrap(); // I/O stays decoded, memory stops.
        assert_eq!(bus.io_read_u8(0x3c5).unwrap(), 0x0f);
        assert_eq!(bus.read_phys_u8(PhysAddr(0xb8000)).unwrap(), 0xff);
        bus.write_phys_u8(PhysAddr(0xb8000), b'X').unwrap();
        bus.io_write_u16(0xcfc, 3).unwrap();
        assert_eq!(bus.read_phys_u8(PhysAddr(0xb8000)).unwrap(), b'V');
    }

    #[test]
    fn pci_config_address_port_reads_back_last_dword_write() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u32(0xcf8, 0x8000_0010).unwrap();

        assert_eq!(bus.io_read_u32(0xcf8).unwrap(), 0x8000_0010);
        assert_eq!(bus.io_read_u8(0xcfb).unwrap(), 0x80);

        bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
        bus.io_write_u16(0xcfc, 0x0002).unwrap();
        assert_eq!(bus.io_read_u16(0xcfc).unwrap(), 0x0002);
    }

    #[test]
    fn pio_completion_has_independent_ata_and_bus_master_acknowledgements() {
        let mut bus = HostBus::new_firmware(2 << 20).unwrap();
        bus.attach_hdd(vec![0; 512], false);
        bus.attach_secondary_cdrom(vec![0; 2048]);
        for (data, control, irq, bm) in [(0x1f0, 0x3f6, 14, 0xc000), (0x170, 0x376, 15, 0xc008)] {
            // ReactOS 0.4.16 programs PIO mode with SET FEATURES, then checks
            // BM status before accepting its IRQ. No DMA transfer is involved.
            bus.io_write_u8(control, 2).unwrap(); // nIEN masks the device pin.
            bus.io_write_u8(data + 1, 3).unwrap();
            bus.io_write_u8(data + 2, 8).unwrap();
            bus.io_write_u8(data + 7, 0xef).unwrap();
            assert!(!bus.pic.line(irq));
            assert_eq!(bus.io_read_u8(bm + 2).unwrap() & 4, 0);
            bus.io_write_u8(control, 0).unwrap();
            assert!(bus.pic.line(irq));
            assert_eq!(bus.io_read_u8(bm + 2).unwrap() & 4, 4);
            assert_eq!(bus.io_read_u8(control).unwrap() & 0x81, 0); // Done, no error.
            assert!(bus.pic.line(irq)); // Alternate status does not acknowledge.

            bus.io_write_u8(bm + 2, 0).unwrap();
            assert_eq!(bus.io_read_u8(bm + 2).unwrap() & 4, 4);
            bus.io_write_u8(bm + 2, 4).unwrap();
            assert_eq!(bus.io_read_u8(bm + 2).unwrap() & 4, 0);
            assert!(bus.pic.line(irq)); // BM acknowledgement does not lower INTRQ.
            bus.io_read_u8(data + 7).unwrap();
            assert!(!bus.pic.line(irq));

            bus.io_write_u8(data + 7, 0xef).unwrap();
            bus.io_read_u8(data + 7).unwrap(); // ATA acknowledgement keeps BM latch.
            assert!(!bus.pic.line(irq));
            assert_eq!(bus.io_read_u16(bm + 2).unwrap() & 4, 4);
            assert_eq!(bus.io_read_u32(bm).unwrap() & 0x0004_0000, 0x0004_0000);
            bus.io_write_u8(control, 4).unwrap(); // Device reset is not PCI reset.
            bus.io_write_u8(control, 0).unwrap();
            assert_eq!(bus.io_read_u8(bm + 2).unwrap() & 4, 4);
        }

        // BAR relocation and wide acknowledgements preserve channel isolation.
        bus.io_write_u32(0xcf8, 0x8000_0920).unwrap();
        bus.io_write_u32(0xcfc, 0x0000_d001).unwrap();
        assert_eq!(bus.io_read_u8(0xc002).unwrap(), 0xff);
        bus.io_write_u16(0xd002, 4).unwrap();
        assert_eq!(bus.io_read_u8(0xd002).unwrap() & 4, 0);
        assert_eq!(bus.io_read_u8(0xd00a).unwrap() & 4, 4);
        bus.io_write_u32(0xd008, 0x0004_0000).unwrap();
        assert_eq!(bus.io_read_u8(0xd00a).unwrap() & 4, 0);
    }

    fn floppy_command(bus: &mut HostBus, bytes: &[u8]) {
        for &byte in bytes {
            assert_eq!(bus.io_read_u8(0x3f4).unwrap() & 0xc0, 0x80);
            bus.io_write_u8(0x3f5, byte).unwrap();
        }
    }

    fn floppy_result(bus: &mut HostBus, expected: &[u8]) {
        for &byte in expected {
            assert_eq!(bus.io_read_u8(0x3f4).unwrap(), 0xd0);
            assert_eq!(bus.io_read_u8(0x3f5).unwrap(), byte);
        }
        assert_eq!(bus.io_read_u8(0x3f4).unwrap(), 0x80);
    }

    #[test]
    fn floppy_irq_mask_and_result_ack_allow_consecutive_completions() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u8(0x21, !0x40).unwrap();

        // The device completion survives DOR masking, independently of PIC EOI.
        bus.io_write_u8(0x3f2, 0x04).unwrap();
        floppy_command(&mut bus, &[0x4a, 0]); // READ ID, no drive attached.
        assert_eq!(bus.pic.pending_irq(), None);
        bus.io_write_u8(0x3f2, 0x0c).unwrap();
        for attempt in 0..2 {
            if attempt != 0 {
                floppy_command(&mut bus, &[0x4a, 0]);
            }
            assert_eq!(bus.pic.pending_irq(), Some(6));
            assert_eq!(bus.pic.acknowledge(), 0x26);
            bus.io_write_u8(0x20, 0x20).unwrap();
            floppy_result(&mut bus, &[0x40, 4, 0, 0, 0, 1, 2]);
            assert_eq!(bus.pic.pending_irq(), None);
        }
    }

    #[test]
    fn floppy_reset_sense_and_seek_report_the_completed_drive() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u8(0x3f2, 0).unwrap();
        bus.io_write_u8(0x3f5, 0x10).unwrap(); // Held reset cannot execute VERSION.
        assert_eq!(bus.io_read_u8(0x3f4).unwrap() & 0x80, 0);
        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0xff);
        bus.io_write_u8(0x3f2, 0x0c).unwrap();
        for drive in 0..4 {
            floppy_command(&mut bus, &[8]);
            floppy_result(&mut bus, &[0xc0 | drive, 0]);
        }
        floppy_command(&mut bus, &[8]);
        floppy_result(&mut bus, &[0x80]); // No phantom second result byte.

        floppy_command(&mut bus, &[0x0f, 2, 37]);
        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0xff); // No result phase yet.
        // DOR still selects drive 0; SENSE must report the seek on drive 2.
        floppy_command(&mut bus, &[8]);
        floppy_result(&mut bus, &[0x22, 37]);
        floppy_command(&mut bus, &[7, 1]); // No TRACK0 signal from an absent drive.
        floppy_command(&mut bus, &[8]);
        floppy_result(&mut bus, &[0x71, 0]);
        floppy_command(&mut bus, &[4, 1]);
        floppy_result(&mut bus, &[0x29]); // Enhanced ST3 constants, no TRACK0.
        assert_eq!(bus.io_read_u8(0x3f7).unwrap() & 0x80, 0x80);

        // A new completion replaces residual reset polling status.
        bus.io_write_u8(0x3f4, 0x80).unwrap();
        floppy_command(&mut bus, &[0x0f, 3, 11]);
        floppy_command(&mut bus, &[8]);
        floppy_result(&mut bus, &[0x23, 11]);
        floppy_command(&mut bus, &[8]);
        floppy_result(&mut bus, &[0x80]);
    }

    #[test]
    fn floppy_fifo_direction_and_invalid_opcodes_do_not_destroy_results() {
        let mut bus = HostBus::new(1024 * 1024);
        floppy_command(&mut bus, &[0x4a, 0]);
        bus.io_write_u8(0x3f5, 0x10).unwrap(); // Wrong direction: preserve READ ID.
        floppy_result(&mut bus, &[0x40, 4, 0, 0, 0, 1, 2]);

        floppy_command(&mut bus, &[3, 0xdf]);
        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0xff);
        floppy_command(&mut bus, &[2]); // Complete interrupted SPECIFY collection.
        floppy_command(&mut bus, &[0x10]);
        floppy_result(&mut bus, &[0x90]);
        // Flag bits belong to particular commands, not a universal low-five mask.
        for invalid in [0x30, 0x88, 0x25, 0x8f] {
            floppy_command(&mut bus, &[invalid]);
            floppy_result(&mut bus, &[0x80]);
        }
    }

    #[test]
    fn floppy_configuration_lock_and_reset_are_visible_through_dumpreg() {
        let mut bus = HostBus::new(1024 * 1024);
        floppy_command(&mut bus, &[3, 0xdf, 2]);
        floppy_command(&mut bus, &[0x13, 0, 0x6b, 17]);
        floppy_command(&mut bus, &[0x12, 0x8f]);
        floppy_command(&mut bus, &[0x12, 1]); // Without OW, preserve drive enables.
        floppy_command(&mut bus, &[0x94]);
        floppy_result(&mut bus, &[0x10]);
        floppy_command(&mut bus, &[0x0e]);
        floppy_result(&mut bus, &[0, 0, 0, 0, 0xdf, 2, 0, 0x8d, 0x6b, 17]);

        // DSR reset keeps DOR selection/motor/IRQ mask. LOCK protects FIFO fields.
        bus.io_write_u8(0x3f2, 0x25).unwrap();
        bus.io_write_u8(0x3f4, 0x80).unwrap();
        assert_eq!(bus.io_read_u8(0x3f2).unwrap(), 0x25);
        floppy_command(&mut bus, &[0x0e]);
        floppy_result(&mut bus, &[0, 0, 0, 0, 0xdf, 2, 0, 0x8c, 0x2b, 17]);
        floppy_command(&mut bus, &[0x14]);
        floppy_result(&mut bus, &[0]);
        bus.io_write_u8(0x3f2, 0).unwrap();
        bus.io_write_u8(0x3f4, 0x80).unwrap(); // DOR reset takes precedence.
        assert_eq!(bus.io_read_u8(0x3f2).unwrap(), 0);
        bus.io_write_u8(0x3f2, 0x0c).unwrap();
        floppy_command(&mut bus, &[0x0e]);
        floppy_result(&mut bus, &[0, 0, 0, 0, 0xdf, 2, 0, 0x0c, 0x20, 0]);
    }

    #[test]
    fn floppy_controller_absent_drive_probe_is_benign() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x3f2, 0).unwrap();
        bus.io_write_u8(0x3f2, 0x0c).unwrap();

        assert_eq!(bus.io_read_u8(0x3f2).unwrap(), 0x0c);
        assert_eq!(bus.io_read_u8(0x3f4).unwrap(), 0x80);
        assert!(bus.fdc.irq_asserted());

        bus.io_write_u8(0x3f5, 0x08).unwrap();

        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0xc0);
        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0x00);
        assert!(!bus.fdc.irq_asserted());
    }

    #[test]
    fn floppy_version_command_reports_enhanced_controller() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x3f5, 0x10).unwrap();

        assert_eq!(bus.io_read_u8(0x3f4).unwrap(), 0xd0);
        assert_eq!(bus.io_read_u8(0x3f5).unwrap(), 0x90);
        assert_eq!(bus.io_read_u8(0x3f4).unwrap(), 0x80);
    }

    #[test]
    fn rtc_periodic_irq_raises_irq8_and_reg_c_clears_it() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.current_step = 100;
        bus.io_write_u8(0x70, 0x0b).unwrap();
        bus.io_write_u8(0x71, 0x46).unwrap();
        bus.begin_io_instruction(100 + TICKS_PER_SECOND / 1024, 0x71);
        bus.update_rtc(bus.device_ticks());

        assert!(bus.rtc_irq8);
        assert_eq!(bus.cmos_status_c & 0xc0, 0xc0);

        bus.io_write_u8(0x70, 0x0c).unwrap();

        assert_eq!(bus.io_read_u8(0x71).unwrap() & 0xc0, 0xc0);
        assert!(!bus.rtc_irq8);
        assert_eq!(bus.cmos_status_c, 0);
    }

    #[test]
    fn non_console_legacy_serial_ports_look_absent() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x2f8, 0).unwrap();

        assert_eq!(bus.io_read_u8(0x2f9).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x3f8 + 5).unwrap(), 0x60);
    }

    #[test]
    fn serial_input_sets_data_ready_and_reads_fifo_order() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.queue_serial_input(b"abc");

        assert_eq!(bus.io_read_u8(0x3f8 + 5).unwrap(), 0x61);
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'a');
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'b');
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'c');
        assert_eq!(bus.io_read_u8(0x3f8 + 5).unwrap(), 0x60);
    }

    #[test]
    fn serial_dlab_keeps_divisor_separate_from_data_and_ier() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x3f8 + 3, 0x80).unwrap();
        bus.io_write_u8(0x3f8, 0x34).unwrap();
        bus.io_write_u8(0x3f8 + 1, 0x12).unwrap();
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), 0x34);
        assert_eq!(bus.io_read_u8(0x3f8 + 1).unwrap(), 0x12);

        bus.io_write_u8(0x3f8 + 3, 0x03).unwrap();
        bus.io_write_u8(0x3f8 + 1, 0x0f).unwrap();
        bus.queue_serial_input(b"x");

        assert_eq!(bus.io_read_u8(0x3f8 + 1).unwrap(), 0x0f);
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'x');
        assert!(bus.serial.is_empty());
    }

    #[test]
    fn serial_loopback_returns_test_bytes_without_terminal_output() {
        let mut bus = HostBus::new(1024 * 1024);
        bus.io_write_u8(0x3fb, 0x03).unwrap(); // eight data bits, DLAB clear
        bus.io_write_u8(0x3fc, 0x1b).unwrap(); // local loopback, OUT2, RTS, DTR
        bus.io_write_u8(0x3f9, 0x03).unwrap(); // RX and TX interrupts
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x02); // initial THRE

        for byte in [0xae, 0x00, 0xff] {
            bus.io_write_u8(0x3f8, byte).unwrap();
            assert_eq!(bus.io_read_u8(0x3fd).unwrap(), 0x61);
            assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x04); // RX before TX
            assert_eq!(bus.io_read_u8(0x3f8).unwrap(), byte);
            assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x02);
            assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x01);
            assert!(!bus.pic.line(4));
        }
        assert!(
            bus.serial.is_empty(),
            "self-test bytes must not reach the terminal"
        );

        // The external RX pin is disconnected during local loopback.
        bus.queue_serial_input(b"x");
        assert_eq!(bus.io_read_u8(0x3fd).unwrap(), 0x60);
        bus.io_write_u8(0x3fc, 0x08).unwrap();
        bus.queue_serial_input(b"y");
        bus.io_write_u8(0x3f8, b'Z').unwrap();
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'y');
        assert_eq!(bus.serial, b"Z");
    }

    #[test]
    fn serial_modem_probe_latches_changes_until_msr_acknowledges_them() {
        let mut bus = HostBus::new(1024 * 1024);
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        bus.io_write_u8(0x21, 0xef).unwrap();
        bus.io_write_u8(0x3fb, 0x03).unwrap();
        assert_eq!(bus.io_read_u8(0x3fe).unwrap(), 0xb0); // connected terminal

        // A standard UART probe loops RTS/OUT2 back to CTS/DCD. DSR falls.
        bus.io_write_u8(0x3fc, 0x1a).unwrap();
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x01); // IER still masks it
        assert!(!bus.pic.line(4));
        bus.io_write_u8(0x3f9, 0x08).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.pic.acknowledge(), 0x24);

        // Higher-priority RX/TX service must leave the modem cause intact.
        bus.io_write_u8(0x3f9, 0x0b).unwrap();
        bus.io_write_u8(0x3f8, 0x5a).unwrap();
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x04);
        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), 0x5a);
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x02);
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x00);
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0x00); // IIR does not ack MSI
        assert!(bus.pic.line(4));
        assert_eq!(bus.io_read_u8(0x3fe).unwrap(), 0x92);
        assert_eq!(bus.io_read_u8(0x3fe).unwrap(), 0x90);
        assert!(!bus.pic.line(4));
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), None);
        bus.io_write_u8(0x3f9, 0x08).unwrap();

        // Deltas survive masking; RI records only its trailing edge.
        for (mcr, msr) in [(0x1f, 0xf2), (0x1b, 0xb4), (0x18, 0x83)] {
            bus.io_write_u8(0x3f9, 0).unwrap();
            bus.io_write_u8(0x3fc, mcr).unwrap();
            assert!(!bus.pic.line(4));
            bus.io_write_u8(0x3f9, 0x08).unwrap();
            assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
            assert_eq!(bus.pic.acknowledge(), 0x24);
            assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0);
            assert_eq!(bus.io_read_u8(0x3fe).unwrap(), msr);
            bus.io_write_u8(0x20, 0x20).unwrap();
            assert_eq!(pending_external_irq(&cpu, &bus), None);
            bus.io_write_u8(0x3fc, mcr).unwrap(); // unchanged lines are not events
            assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 1);
        }

        bus.io_write_u8(0x3fc, 0x10).unwrap(); // OUT2 gates the physical IRQ only
        assert_eq!(bus.io_read_u8(0x3fa).unwrap(), 0);
        assert_eq!(pending_external_irq(&cpu, &bus), None);
        bus.io_write_u8(0x3fc, 0x18).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.pic.acknowledge(), 0x24);
        assert_eq!(bus.io_read_u8(0x3fe).unwrap(), 0x88);
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), None);
        bus.io_write_u8(0x3fc, 0x08).unwrap(); // reconnect the terminal
        assert_eq!(bus.io_read_u8(0x3fe).unwrap(), 0xb3);
    }

    #[test]
    fn serial_receive_irq_requires_ier_out2_pic_unmasked_and_clears_after_read() {
        let mut bus = HostBus::new(1024 * 1024);
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        bus.io_write_u8(0x21, 0xef).unwrap();

        bus.queue_serial_input(b"x");
        bus.io_write_u8(0x3f8 + 1, 0x01).unwrap();
        assert_ne!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.io_read_u8(0x3f8 + 2).unwrap(), 0x04);

        bus.io_write_u8(0x3f8 + 4, 0x08).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.io_read_u8(0x3f8 + 2).unwrap(), 0x04);

        assert_eq!(bus.io_read_u8(0x3f8).unwrap(), b'x');
        assert_eq!(bus.io_read_u8(0x3f8 + 2).unwrap(), 0x01);
        assert!(!bus.pic.line(4));
        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.pic.acknowledge(), 0x24);
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), None);
    }

    #[test]
    fn serial_tx_empty_interrupt_follows_ier_and_iir_ack() {
        let mut bus = HostBus::new(1024 * 1024);
        let mut cpu = Cpu::new();
        cpu.eflags |= EFLAGS_IF;
        bus.io_write_u8(0x21, 0xef).unwrap();

        bus.io_write_u8(0x3f8 + 4, 0x08).unwrap();
        bus.io_write_u8(0x3f8 + 1, 0x02).unwrap();

        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.io_read_u8(0x3f8 + 2).unwrap(), 0x02);
        assert!(!bus.pic.line(4));
        assert_eq!(bus.pic.acknowledge(), 0x24);
        bus.io_write_u8(0x20, 0x20).unwrap();
        assert_eq!(pending_external_irq(&cpu, &bus), None);

        bus.io_write_u8(0x3f8, b'Z').unwrap();
        assert_eq!(bus.serial, b"Z");
        assert_eq!(pending_external_irq(&cpu, &bus), Some(4));
        assert_eq!(bus.io_read_u8(0x3f8 + 2).unwrap(), 0x02);
    }

    #[test]
    fn pio_write_completion_wakes_a_halted_guest_at_the_device_deadline() {
        let mut m = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
        m.attach_hdd(vec![0; 512], true);
        let mut boot = [0; 512];
        boot[..4].copy_from_slice(&[0xfb, 0xf4, 0xfa, 0xf4]); // STI; HLT; CLI; HLT
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        m.load_bootsector(&boot).unwrap();
        m.enter_bootsector();
        m.cpu.idtr.limit = 0x03ff;
        m.bus.io_write_u8(0x21, 0xfb).unwrap();
        m.bus.io_write_u8(0xa1, 0xbf).unwrap();
        let vector = usize::from(m.bus.pic.vector_base(true) + 6);
        m.bus.ram[vector * 4..vector * 4 + 4].copy_from_slice(&[0, 0x80, 0, 0]);
        let handler = [
            0xba, 0xf7, 0x01, 0xec, // acknowledge ATA status
            0xb0, 0x20, 0xe6, 0xa0, 0xe6, 0x20, // both PIC EOIs
            0xba, 0xf8, 0x03, 0xb0, b'W', 0xee, // observable IRQ marker
            0xba, 0x04, 0x06, 0xb8, 0x00, 0x20, 0xef, // ACPI S5
        ];
        m.bus.ram[0x8000..0x8000 + handler.len()].copy_from_slice(&handler);
        m.bus.io_write_u8(0x1f6, 0xe0).unwrap();
        m.bus.io_write_u8(0x1f2, 1).unwrap();
        m.bus.io_write_u8(0x1f3, 0).unwrap();
        m.bus.io_write_u8(0x1f7, 0x30).unwrap();
        for _ in 0..256 {
            m.bus.io_write_u16(0x1f0, 0x1234).unwrap();
        }
        let deadline = m.bus.ide.next_event_step();
        assert!(deadline > 2 && deadline < 100_000);
        let report = m.run_steps(100_000).unwrap();
        assert!(
            matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
            "{report:?}"
        );
        assert_eq!(m.serial_output(), b"W");
        assert!(m.step_count() >= deadline);
        assert_eq!(m.bus.pic.in_service(false) | m.bus.pic.in_service(true), 0);
    }

    #[test]
    fn ide_channels_look_absent_for_initramfs_boot() {
        let mut bus = HostBus::new(1024 * 1024);

        bus.io_write_u8(0x1f2, 0).unwrap();

        assert_eq!(bus.io_read_u8(0x1f2).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x1e7).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x1ef).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x36e).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x3e6).unwrap(), 0xff);
        assert_eq!(bus.io_read_u8(0x3f6).unwrap(), 0xff);
    }

    #[test]
    fn websocket_text_snapshot_uses_guest_vga_cells() {
        let mut bus = HostBus::new_firmware(2 * 1024 * 1024).unwrap();
        for (index, byte) in b"tc@box:~$".iter().copied().enumerate() {
            bus.write_phys_u8(PhysAddr(0xb8000 + (index as u64 * 2)), byte)
                .unwrap();
        }

        assert!(vga_text_snapshot_string(&bus).contains("tc@box:~$"));
    }
}

#[cfg(test)]
mod pci_decode_cache_tests {
    use super::*;

    fn engines() -> &'static [bool] {
        if cfg!(all(
            feature = "jit",
            target_arch = "aarch64",
            target_os = "linux"
        )) {
            &[false, true]
        } else {
            &[false]
        }
    }

    fn machine(jit: bool) -> Machine {
        let mut m = Machine::new(32 << 20);
        m.cpu.enter_protected_flat(0x1000, 0);
        m.set_jit_enabled(jit).unwrap();
        m
    }

    fn imm(code: &mut Vec<u8>, op: u8, value: u32) {
        code.push(op);
        code.extend_from_slice(&value.to_le_bytes());
    }

    fn code_prefix() -> Vec<u8> {
        // Exercise native execution too when running this test on ARM64.
        vec![0xb9, 32, 0, 0, 0, 0x49, 0x75, 0xfd] // MOV ECX,32; DEC/JNZ
    }

    fn vga_word(m: &mut Machine, offset: u64, value: u32) {
        for (byte, value) in value.to_le_bytes().into_iter().enumerate() {
            assert!(
                m.bus
                    .vga
                    .lfb_write_u8(0xe000_0000 + offset + byte as u64, 0xe000_0000, value)
            );
        }
    }

    fn run(m: &mut Machine, code: &[u8], start: usize) {
        m.bus.ram[start..start + code.len()].copy_from_slice(code);
        m.cpu.eip = start as u32;
        m.run_steps(2000).unwrap();
        assert!(m.cpu.halted);
        #[cfg(all(feature = "jit", target_arch = "aarch64", target_os = "linux"))]
        if let Some(jit) = &m.jit {
            assert!(jit.stats.native_steps > 0);
            std::println!("PCI decode native steps: {}", jit.stats.native_steps);
        }
    }

    #[test]
    fn guest_pci_decode_writes_replace_cached_ram_reads_and_stores() {
        for &jit in engines() {
            // Relocate an enabled BAR by dword OUT, or enable an already
            // relocated BAR by byte/word OUT to the PCI command register.
            for width in [4, 1, 2] {
                let mut m = machine(jit);
                let address = 0x0100_2000u32;
                let (config, value) = if width == 4 {
                    (0x80001010, 0x01000000)
                } else {
                    m.bus.pci.write_addr(0x80001004);
                    m.bus.pci.write_data_u32(1); // Memory decoder disabled.
                    m.bus.pci.write_addr(0x80001010);
                    m.bus.pci.write_data_u32(0x01000000);
                    (0x80001004, 3)
                };
                vga_word(&mut m, 0x2000, 0xa5);
                let mut code = code_prefix();
                imm(&mut code, 0xb8, 0x11223344);
                imm(&mut code, 0xa3, address); // Prime the writable RAM token.
                imm(&mut code, 0xa1, address); // Prime the read token.
                imm(&mut code, 0xa3, 0x3000);
                imm(&mut code, 0xba, 0xcf8);
                imm(&mut code, 0xb8, config);
                code.push(0xef);
                imm(&mut code, 0xba, 0xcfc);
                imm(&mut code, 0xb8, value);
                if width == 2 {
                    code.push(0x66);
                }
                code.push(if width == 1 { 0xee } else { 0xef });
                imm(&mut code, 0xa1, address);
                imm(&mut code, 0xa3, 0x3004);
                imm(&mut code, 0xb8, 0x55667788);
                imm(&mut code, 0xa3, address); // Must reach VGA, not old RAM.
                code.push(0xf4);
                run(&mut m, &code, 0x1000);
                assert_eq!(&m.ram()[0x3000..0x3004], &0x11223344u32.to_le_bytes());
                assert_eq!(
                    &m.ram()[0x3004..0x3008],
                    &0xa5u32.to_le_bytes(),
                    "width={width} jit={jit}"
                );
                assert_eq!(
                    &m.ram()[address as usize..address as usize + 4],
                    &0x11223344u32.to_le_bytes()
                );
                assert_eq!(
                    m.bus.read_phys_u32(PhysAddr(u64::from(address))).unwrap(),
                    0x55667788
                );
            }
        }
    }

    #[test]
    fn rep_outs_rechecks_its_source_after_each_pci_decode_change() {
        for &jit in engines() {
            let mut m = machine(jit);
            let address = 0x0100_2000u32;
            m.bus.ram[address as usize..address as usize + 4]
                .copy_from_slice(&0x01000000u32.to_le_bytes());
            m.bus.ram[address as usize + 4..address as usize + 8]
                .copy_from_slice(&0x02000000u32.to_le_bytes());
            vga_word(&mut m, 0x2004, 0x03000000);
            let mut code = code_prefix();
            imm(&mut code, 0xba, 0xcf8);
            imm(&mut code, 0xb8, 0x80001010);
            code.push(0xef);
            imm(&mut code, 0xba, 0xcfc);
            imm(&mut code, 0xbe, address);
            imm(&mut code, 0xb9, 2);
            code.extend_from_slice(&[0xf3, 0x6f, 0xf4]); // REP OUTSD; HLT
            run(&mut m, &code, 0x1000);
            assert_eq!(
                m.bus.pci.vga_lfb_base(),
                0x03000000,
                "second element must come from VGA"
            );
            assert_eq!(m.cpu.reg(Reg32::Esi), address + 8);
            assert_eq!(m.cpu.reg(Reg32::Ecx), 0);
        }
    }

    #[test]
    fn pci_decode_change_replaces_the_next_instruction_fetch() {
        for &jit in engines() {
            let mut m = machine(jit);
            let start = 0x0100_2000;
            let mut code = code_prefix();
            imm(&mut code, 0xba, 0xcf8);
            imm(&mut code, 0xb8, 0x80001010);
            code.push(0xef);
            imm(&mut code, 0xba, 0xcfc);
            imm(&mut code, 0xb8, 0x01000000);
            code.push(0xef);
            let next = code.len();
            imm(&mut code, 0xb8, 0x11223344);
            code.push(0xf4);
            let mut replacement = vec![0xb8];
            replacement.extend_from_slice(&0xaabbccddu32.to_le_bytes());
            replacement.push(0xf4);
            for (byte, value) in replacement.into_iter().enumerate() {
                assert!(m.bus.vga.lfb_write_u8(
                    0xe000_2000 + (next + byte) as u64,
                    0xe000_0000,
                    value
                ));
            }
            run(&mut m, &code, start);
            assert_eq!(
                m.cpu.reg(Reg32::Eax),
                0xaabbccdd,
                "old RAM opcodes must not survive the OUT"
            );
        }
    }
}

#[cfg(test)]
mod network_board_tests {
    use super::*;
    fn config(bus: &mut HostBus, addr: u32) -> u32 {
        bus.io_write_u32(0xcf8, addr).unwrap();
        bus.io_read_u32(0xcfc).unwrap()
    }
    #[test]
    fn network_pci_bar_decode_and_level_irq() {
        let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
        assert_eq!(config(&mut machine.bus, 0x8000_1800), u32::MAX);
        machine.attach_network();
        let bus = &mut machine.bus;
        assert_eq!(config(bus, 0x8000_1800), 0x8029_10ec);
        assert_eq!(config(bus, 0x8000_1808), 0x0200_0000);
        assert_eq!(config(bus, 0x8000_183c), 0x10b);
        assert_eq!(bus.io_read_u8(0x4d1).unwrap() & 8, 8);
        config(bus, 0x8000_1810);
        bus.io_write_u32(0xcfc, u32::MAX).unwrap();
        assert_eq!(bus.io_read_u32(0xcfc).unwrap(), 0xffff_ffe1);
        assert_eq!(bus.io_read_u8(0xd000).unwrap(), 0xff);
        bus.io_write_u32(0xcfc, 0xd001).unwrap();
        assert_eq!(bus.io_read_u8(0xd000).unwrap(), 0x21);
        bus.io_write_u8(0xd00f, 0x40).unwrap(); // unmask remote DMA completion
        bus.io_write_u8(0xd000, 0x0a).unwrap(); // zero-length DMA
        assert!(bus.pic.line(NET_IRQ));
        bus.io_write_u8(0xd007, 0x40).unwrap();
        assert!(!bus.pic.line(NET_IRQ));
        config(bus, 0x8000_1804);
        bus.io_write_u16(0xcfc, 0).unwrap();
        assert_eq!(bus.io_read_u8(0xd000).unwrap(), 0xff);
        bus.io_write_u16(0xcfc, 1).unwrap();
        assert_eq!(bus.io_read_u8(0xd000).unwrap(), 0x0a);
        config(bus, 0x8000_1810);
        bus.io_write_u32(0xcfc, 0x1_d001).unwrap();
        assert_eq!(bus.io_read_u8(0xd000).unwrap(), 0xff); // high BAR cannot alias
    }
}
