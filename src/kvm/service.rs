use crate::monitor::abi::{
    ABI_VERSION, HOST_COMMAND_A_OFFSET, HOST_COMMAND_B_OFFSET, HOST_COMMAND_C_OFFSET,
    HOST_COMMAND_D_OFFSET, HOST_COMMAND_FLAGS_OFFSET, HOST_COMMAND_KIND_OFFSET,
    HOST_COMMAND_SEQ_OFFSET, HOST_COMMAND_SIZE_OFFSET, HostCommandKindV1, HostCommandV1,
    HostResponseV1, MAILBOX_COMMAND_OFFSET, MAILBOX_COMMAND_SEQ_OFFSET, MAILBOX_REQ_SEQ_OFFSET,
    MAILBOX_REQUEST_OFFSET, MAILBOX_RESP_SEQ_OFFSET, MAILBOX_RESPONSE_OFFSET, MonitorRequestKindV1,
    MonitorRequestV1, REQUEST_A_OFFSET, REQUEST_B_OFFSET, REQUEST_C_OFFSET, REQUEST_D_OFFSET,
    REQUEST_FLAGS_OFFSET, REQUEST_KIND_OFFSET, REQUEST_SEQ_OFFSET, REQUEST_SIZE_OFFSET,
    RESPONSE_A_OFFSET, RESPONSE_B_OFFSET, RESPONSE_C_OFFSET, RESPONSE_RESERVED_OFFSET,
    RESPONSE_SEQ_OFFSET, RESPONSE_SIZE_OFFSET, RESPONSE_STATUS_OFFSET, RESPONSE_VALUE_OFFSET,
    ResponseStatusV1, VIRTUAL_TIMER_PERIOD_TICKS,
};
use std::sync::atomic::{Ordering, fence};

use crate::kvm::backing::SlotBacking;
use crate::kvm::boot::SHARED_MAILBOX_OFFSET;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::memslot::{
    LEGACY_VGA_BASE, LEGACY_VGA_SIZE, MemslotKind, MemslotPlan, PlannedMemslot,
};

pub const COM1_BASE: u16 = 0x03f8;
pub const COM1_END: u16 = COM1_BASE + 7;
const VGA_TEXT_BUFFER_OFFSET: usize = 0x0001_8000;
const VGA_TEXT_BYTES: usize = 80 * 25 * 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailboxRequestSnapshot {
    pub req_seq: u32,
    pub request: MonitorRequestV1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostServiceOutcome {
    pub seq: u32,
    pub kind: u32,
    pub status: u32,
    pub value: u64,
    pub requests_serviced: u64,
    pub serial_output_len: usize,
    pub halted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorStopAck {
    pub seq: u32,
    pub steps: u64,
    pub eip: u32,
    pub eflags: u32,
    pub phys_writes: u32,
    pub mmio_writes: u32,
    pub pio_outs: u32,
    pub irq_polls: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorPanic {
    pub seq: u32,
    pub esr: u64,
    pub far: u64,
    pub elr: u64,
    pub sp: u64,
}

pub const MONITOR_REQUEST_KIND_SLOTS: usize = 16;
pub const MONITOR_HOT_PIO_SLOTS: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PioPortCounter {
    pub port: u16,
    pub width: u8,
    pub is_write: bool,
    pub count: u64,
    pub last_c: u32,
}

impl PioPortCounter {
    const fn matches(self, port: u16, width: u8, is_write: bool) -> bool {
        self.count != 0 && self.port == port && self.width == width && self.is_write == is_write
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorHostRequestStats {
    pub total: u64,
    pub ok: u64,
    pub unsupported: u64,
    pub fault: u64,
    pub bad_packet: u64,
    pub last_seq: u32,
    pub last_kind: u32,
    pub last_status: u32,
    pub last_a: u64,
    pub last_b: u64,
    pub last_c: u64,
    pub last_d: u64,
    pub kind_counts: [u64; MONITOR_REQUEST_KIND_SLOTS],
    pub hot_pio: [PioPortCounter; MONITOR_HOT_PIO_SLOTS],
}

impl MonitorHostRequestStats {
    fn record(&mut self, request: MonitorRequestV1, status: u32) {
        self.total = self.total.wrapping_add(1);
        self.last_seq = request.seq;
        self.last_kind = request.kind;
        self.last_status = status;
        self.last_a = request.a;
        self.last_b = request.b;
        self.last_c = request.c;
        self.last_d = request.d;

        if let Ok(index) = usize::try_from(request.kind) {
            if let Some(slot) = self.kind_counts.get_mut(index) {
                *slot = slot.wrapping_add(1);
            }
        }

        match status {
            value if value == ResponseStatusV1::Ok as u32 => self.ok = self.ok.wrapping_add(1),
            value if value == ResponseStatusV1::Unsupported as u32 => {
                self.unsupported = self.unsupported.wrapping_add(1)
            }
            value if value == ResponseStatusV1::Fault as u32 => {
                self.fault = self.fault.wrapping_add(1)
            }
            value if value == ResponseStatusV1::BadPacket as u32 => {
                self.bad_packet = self.bad_packet.wrapping_add(1)
            }
            _ => {}
        }

        if request.kind == MonitorRequestKindV1::PioIn as u32
            || request.kind == MonitorRequestKindV1::PioOut as u32
        {
            if let (Some(port), Some(width)) = (checked_u16(request.a), checked_u8(request.b)) {
                self.record_pio(
                    port,
                    width,
                    request.kind == MonitorRequestKindV1::PioOut as u32,
                    request.c as u32,
                );
            }
        }
    }

    fn record_pio(&mut self, port: u16, width: u8, is_write: bool, last_c: u32) {
        for slot in &mut self.hot_pio {
            if slot.matches(port, width, is_write) {
                slot.count = slot.count.wrapping_add(1);
                slot.last_c = last_c;
                return;
            }
        }

        if let Some(slot) = self.hot_pio.iter_mut().find(|slot| slot.count == 0) {
            *slot = PioPortCounter {
                port,
                width,
                is_write,
                count: 1,
                last_c,
            };
            return;
        }

        let mut min_index = 0;
        let mut min_count = self.hot_pio[0].count;
        for index in 1..self.hot_pio.len() {
            if self.hot_pio[index].count < min_count {
                min_index = index;
                min_count = self.hot_pio[index].count;
            }
        }
        self.hot_pio[min_index] = PioPortCounter {
            port,
            width,
            is_write,
            count: min_count.saturating_add(1),
            last_c,
        };
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorHardMmuStats {
    pub parts_seen: u32,
    pub last_seq: u32,
    pub cr0: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub eflags: u32,
    pub steps: u64,
    pub bus_native_read_attempts: u32,
    pub bus_native_read_hits: u32,
    pub bus_native_read_aborts: u32,
    pub bus_native_read_descriptor_aborts: u32,
    pub bus_native_read_translation_aborts: u32,
    pub bus_native_read_access_flag_aborts: u32,
    pub bus_native_read_permission_aborts: u32,
    pub bus_page_table_frames: u32,
    pub bus_phys_reads: u32,
    pub bus_tracked_writes: u32,
    pub shadow_translations: u32,
    pub shadow_eligible_4k: u32,
    pub shadow_read_fills: u32,
    pub shadow_write_fills: u32,
    pub shadow_upgrades: u32,
    pub shadow_stage1_installs: u32,
    pub shadow_soft_unsupported_mode: u32,
    pub shadow_soft_large_or_direct: u32,
    pub shadow_soft_non_ram: u32,
    pub shadow_soft_pt_protected: u32,
    pub shadow_errors: u32,
    pub shadow_stage1_flushes: u32,
    pub shadow_page_table_write_flushes: u32,
    pub shadow_control_flushes: u32,
    pub shadow_invlpg_flushes: u32,
    pub shadow_native_read_attempts: u32,
    pub shadow_native_read_hits: u32,
    pub shadow_native_read_aborts: u32,
    pub shadow_native_read_descriptor_aborts: u32,
    pub shadow_native_read_translation_aborts: u32,
    pub shadow_native_read_access_flag_aborts: u32,
    pub shadow_native_read_permission_aborts: u32,
    pub context_hits: u32,
    pub context_misses: u32,
    pub context_evictions: u32,
    pub cpu_fetch_translation_hits: u32,
    pub cpu_fetch_translation_fills: u32,
    pub cpu_fetch_translation_invalidations: u32,
    pub cpu_linear_translation_hits: u32,
    pub cpu_linear_translation_fills: u32,
    pub cpu_linear_translation_invalidations: u32,
}

impl MonitorHardMmuStats {
    pub const fn is_empty(self) -> bool {
        self.parts_seen == 0
    }

    fn record_packet(&mut self, seq: u32, request: MonitorRequestV1) -> bool {
        let Some(part) = checked_u32(request.a) else {
            return false;
        };
        if part >= 8 {
            return false;
        }
        self.last_seq = seq;
        self.parts_seen |= 1_u32 << part;
        match part {
            0 => {
                self.cr0 = low_u32(request.b);
                self.cr3 = high_u32(request.b);
                self.cr4 = low_u32(request.c);
                self.eflags = high_u32(request.c);
                self.steps = request.d;
            }
            1 => {
                self.bus_native_read_attempts = low_u32(request.b);
                self.bus_native_read_hits = high_u32(request.b);
                self.bus_native_read_aborts = low_u32(request.c);
                self.bus_page_table_frames = high_u32(request.c);
                self.bus_phys_reads = low_u32(request.d);
                self.bus_tracked_writes = high_u32(request.d);
            }
            2 => {
                self.shadow_translations = low_u32(request.b);
                self.shadow_eligible_4k = high_u32(request.b);
                self.shadow_read_fills = low_u32(request.c);
                self.shadow_write_fills = high_u32(request.c);
                self.shadow_upgrades = low_u32(request.d);
                self.shadow_stage1_installs = high_u32(request.d);
            }
            3 => {
                self.shadow_soft_unsupported_mode = low_u32(request.b);
                self.shadow_soft_large_or_direct = high_u32(request.b);
                self.shadow_soft_non_ram = low_u32(request.c);
                self.shadow_soft_pt_protected = high_u32(request.c);
                self.shadow_errors = low_u32(request.d);
                self.shadow_stage1_flushes = high_u32(request.d);
            }
            4 => {
                self.shadow_page_table_write_flushes = low_u32(request.b);
                self.shadow_control_flushes = high_u32(request.b);
                self.shadow_invlpg_flushes = low_u32(request.c);
                self.shadow_native_read_attempts = high_u32(request.c);
                self.shadow_native_read_hits = low_u32(request.d);
                self.shadow_native_read_aborts = high_u32(request.d);
            }
            5 => {
                self.bus_native_read_descriptor_aborts = low_u32(request.b);
                self.shadow_native_read_descriptor_aborts = high_u32(request.b);
                self.bus_native_read_translation_aborts = low_u32(request.c);
                self.shadow_native_read_translation_aborts = high_u32(request.c);
                self.bus_native_read_access_flag_aborts = low_u32(request.d);
                self.shadow_native_read_access_flag_aborts = high_u32(request.d);
            }
            6 => {
                self.bus_native_read_permission_aborts = low_u32(request.b);
                self.shadow_native_read_permission_aborts = high_u32(request.b);
                self.context_hits = low_u32(request.c);
                self.context_misses = high_u32(request.c);
                self.context_evictions = low_u32(request.d);
            }
            7 => {
                self.cpu_fetch_translation_hits = low_u32(request.b);
                self.cpu_fetch_translation_fills = high_u32(request.b);
                self.cpu_fetch_translation_invalidations = low_u32(request.c);
                self.cpu_linear_translation_hits = high_u32(request.c);
                self.cpu_linear_translation_fills = low_u32(request.d);
                self.cpu_linear_translation_invalidations = high_u32(request.d);
            }
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorHostService {
    pio: PcPioBus,
    machine_boot_reload: bool,
    last_req_seq: u32,
    host_command_seq: u32,
    requests_serviced: u64,
    halted: bool,
    halt_seq: Option<u32>,
    stop_ack: Option<MonitorStopAck>,
    panic: Option<MonitorPanic>,
    hardmmu_stats: MonitorHardMmuStats,
    request_stats: MonitorHostRequestStats,
}

impl MonitorHostService {
    pub fn new() -> Self {
        Self {
            pio: PcPioBus::new(),
            machine_boot_reload: false,
            last_req_seq: 0,
            host_command_seq: 0,
            requests_serviced: 0,
            halted: false,
            halt_seq: None,
            stop_ack: None,
            panic: None,
            hardmmu_stats: MonitorHardMmuStats::default(),
            request_stats: MonitorHostRequestStats::default(),
        }
    }

    pub fn pio(&self) -> &PcPioBus {
        &self.pio
    }

    pub fn enable_machine_boot_reload(&mut self) {
        self.machine_boot_reload = true;
    }

    pub fn pio_mut(&mut self) -> &mut PcPioBus {
        &mut self.pio
    }

    pub fn from_pio_snapshot(snapshot: PcPioBusSnapshot) -> Result<Self> {
        Ok(Self {
            pio: PcPioBus::from_snapshot(snapshot)?,
            machine_boot_reload: false,
            last_req_seq: 0,
            host_command_seq: 0,
            requests_serviced: 0,
            halted: false,
            halt_seq: None,
            stop_ack: None,
            panic: None,
            hardmmu_stats: MonitorHardMmuStats::default(),
            request_stats: MonitorHostRequestStats::default(),
        })
    }

    pub fn pio_snapshot(&self) -> PcPioBusSnapshot {
        self.pio.snapshot()
    }

    pub fn requests_serviced(&self) -> u64 {
        self.requests_serviced
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn halt_seq(&self) -> Option<u32> {
        self.halt_seq
    }

    pub fn stop_ack(&self) -> Option<MonitorStopAck> {
        self.stop_ack
    }

    pub fn panic(&self) -> Option<MonitorPanic> {
        self.panic
    }

    pub fn hardmmu_stats(&self) -> Option<MonitorHardMmuStats> {
        if self.hardmmu_stats.is_empty() {
            None
        } else {
            Some(self.hardmmu_stats)
        }
    }

    pub const fn request_stats(&self) -> MonitorHostRequestStats {
        self.request_stats
    }

    pub fn serial_output(&self) -> &[u8] {
        self.pio.serial_output()
    }

    pub fn vga_text_bytes(&self) -> &[u8] {
        self.pio.vga_text_bytes()
    }

    pub fn service_doorbell(
        &mut self,
        plan: &MemslotPlan,
        backing: &mut [SlotBacking],
    ) -> Result<HostServiceOutcome> {
        let mailbox = mailbox_bytes_mut(plan, backing)?;
        self.service_mailbox(mailbox)
    }

    pub fn publish_stop_command(
        &mut self,
        plan: &MemslotPlan,
        backing: &mut [SlotBacking],
    ) -> Result<u32> {
        let mailbox = mailbox_bytes_mut(plan, backing)?;
        self.publish_stop_command_to_mailbox(mailbox)
    }

    pub fn publish_stop_command_to_mailbox(&mut self, mailbox: &mut [u8]) -> Result<u32> {
        let seq = self.next_host_command_seq();
        let command = HostCommandV1 {
            kind: HostCommandKindV1::Stop as u32,
            size: core::mem::size_of::<HostCommandV1>() as u32,
            seq,
            flags: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        };
        write_host_command_to_mailbox(mailbox, seq, &command)?;
        Ok(seq)
    }

    pub fn service_mailbox(&mut self, mailbox: &mut [u8]) -> Result<HostServiceOutcome> {
        let snapshot = read_mailbox_request_from_bytes(mailbox)?;
        let response = self.handle_request(&snapshot);
        write_response_to_mailbox(mailbox, &response)?;
        self.requests_serviced = self.requests_serviced.wrapping_add(1);
        self.request_stats.record(snapshot.request, response.status);
        Ok(HostServiceOutcome {
            seq: response.seq,
            kind: snapshot.request.kind,
            status: response.status,
            value: response.value,
            requests_serviced: self.requests_serviced,
            serial_output_len: self.serial_output().len(),
            halted: self.halted,
        })
    }

    fn next_host_command_seq(&mut self) -> u32 {
        self.host_command_seq = self.host_command_seq.wrapping_add(1);
        if self.host_command_seq == 0 {
            self.host_command_seq = 1;
        }
        self.host_command_seq
    }

    fn handle_request(&mut self, snapshot: &MailboxRequestSnapshot) -> HostResponseV1 {
        let request = snapshot.request;
        let seq = request.seq;
        if snapshot.req_seq == 0
            || snapshot.req_seq != seq
            || request.size != core::mem::size_of::<MonitorRequestV1>() as u32
            || request.flags != 0
            || (self.last_req_seq != 0 && seq <= self.last_req_seq)
        {
            self.last_req_seq = self.last_req_seq.max(seq);
            return HostResponseV1::bad_packet(seq);
        }
        self.last_req_seq = seq;

        match request.kind {
            value if value == MonitorRequestKindV1::ReloadBoot as u32 => {
                if request.a | request.b | request.c | request.d != 0 {
                    HostResponseV1::bad_packet(seq)
                } else if self.machine_boot_reload {
                    // The VMM must complete the reload before resuming KVM_RUN.
                    HostResponseV1::ok(seq, 0)
                } else {
                    HostResponseV1::unsupported(seq)
                }
            }
            value if value == MonitorRequestKindV1::Hello as u32 => {
                if request.a == ABI_VERSION as u64 {
                    HostResponseV1::ok(seq, 0)
                } else {
                    HostResponseV1::bad_packet(seq)
                }
            }
            value if value == MonitorRequestKindV1::PioIn as u32 => {
                let Some(port) = checked_u16(request.a) else {
                    return HostResponseV1::bad_packet(seq);
                };
                let Some(width) = checked_u8(request.b) else {
                    return HostResponseV1::bad_packet(seq);
                };
                match self.pio.read_at(port, width, request.c) {
                    PioResult::Ok(value) => HostResponseV1::ok(seq, u64::from(value)),
                    PioResult::Unsupported => HostResponseV1::unsupported(seq),
                }
            }
            value if value == MonitorRequestKindV1::PioOut as u32 => {
                let Some(port) = checked_u16(request.a) else {
                    return HostResponseV1::bad_packet(seq);
                };
                let Some(width) = checked_u8(request.b) else {
                    return HostResponseV1::bad_packet(seq);
                };
                let Some(value) = checked_u32(request.c) else {
                    return HostResponseV1::bad_packet(seq);
                };
                match self.pio.write(port, width, value) {
                    PioResult::Ok(_) => HostResponseV1::ok(seq, 0),
                    PioResult::Unsupported => HostResponseV1::unsupported(seq),
                }
            }
            value if value == MonitorRequestKindV1::MmioRead as u32 => {
                let Some(width) = checked_u8(request.b) else {
                    return HostResponseV1::bad_packet(seq);
                };
                match self.pio.mmio_read(request.a, width) {
                    Some(value) => HostResponseV1::ok(seq, value),
                    None => HostResponseV1::unsupported(seq),
                }
            }
            value if value == MonitorRequestKindV1::MmioWrite as u32 => {
                let Some(width) = checked_u8(request.b) else {
                    return HostResponseV1::bad_packet(seq);
                };
                if self.pio.mmio_write(request.a, width, request.c) {
                    HostResponseV1::ok(seq, 0)
                } else {
                    HostResponseV1::unsupported(seq)
                }
            }
            value if value == MonitorRequestKindV1::TimerPoll as u32 => {
                let mut response =
                    HostResponseV1::ok(seq, u64::from(self.pio.pending_irq_mask(request.a)));
                response.a = self.pio.next_timer_vtime(request.a).unwrap_or(0);
                response
            }
            value if value == MonitorRequestKindV1::Inta as u32 => {
                HostResponseV1::ok(seq, u64::from(self.pio.acknowledge_irq(request.a)))
            }
            value if value == MonitorRequestKindV1::Halt as u32 => {
                self.halted = true;
                self.halt_seq = Some(seq);
                HostResponseV1::ok(seq, 0)
            }
            value if value == MonitorRequestKindV1::Panic as u32 => {
                self.halted = true;
                self.halt_seq = Some(seq);
                self.panic = Some(MonitorPanic {
                    seq,
                    esr: request.a,
                    far: request.b,
                    elr: request.c,
                    sp: request.d,
                });
                self.pio.append_serial_output(b"R86_MONITOR_PANIC\n");
                HostResponseV1::ok(seq, 0)
            }
            value if value == MonitorRequestKindV1::SerialFlush as u32 => {
                let Some(len) = usize::try_from(request.a).ok().filter(|len| *len <= 24) else {
                    return HostResponseV1::bad_packet(seq);
                };
                let mut bytes = [0_u8; 24];
                bytes[0..8].copy_from_slice(&request.b.to_le_bytes());
                bytes[8..16].copy_from_slice(&request.c.to_le_bytes());
                bytes[16..24].copy_from_slice(&request.d.to_le_bytes());
                self.pio.append_serial_output(&bytes[..len]);
                HostResponseV1::ok(seq, self.pio.serial_output().len() as u64)
            }
            value if value == MonitorRequestKindV1::StopAck as u32 => {
                self.stop_ack = Some(MonitorStopAck {
                    seq,
                    steps: request.a,
                    eip: low_u32(request.b),
                    eflags: high_u32(request.b),
                    phys_writes: low_u32(request.c),
                    mmio_writes: high_u32(request.c),
                    pio_outs: low_u32(request.d),
                    irq_polls: high_u32(request.d),
                });
                HostResponseV1::ok(seq, 0)
            }
            value if value == MonitorRequestKindV1::HardMmuStats as u32 => {
                if self.hardmmu_stats.record_packet(seq, request) {
                    HostResponseV1::ok(seq, u64::from(self.hardmmu_stats.parts_seen))
                } else {
                    HostResponseV1::bad_packet(seq)
                }
            }
            _ => HostResponseV1::unsupported(seq),
        }
    }
}

impl Default for MonitorHostService {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PioResult {
    Ok(u32),
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Uart8250Snapshot {
    pub output: Vec<u8>,
    pub input: Vec<u8>,
    pub dll: u8,
    pub dlh: u8,
    pub ier: u8,
    pub lcr: u8,
    pub mcr: u8,
    pub scratch: u8,
    pub tx_interrupt: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcPioBusSnapshot {
    pub serial: Uart8250Snapshot,
    pub vga: Vec<u8>,
    pub pci_config_addr: u32,
    pub pit0_reload: u16,
    pub pit0_counter: u16,
    pub pit0_tmp_low: u8,
    pub pit0_read_high: bool,
    pub pit0_write_high: bool,
    pub port61: u8,
    pub pit2_reload: u16,
    pub pit2_counter: u16,
    pub pit2_status_reads: u32,
    pub pit2_tmp_low: u8,
    pub pit2_read_high: bool,
    pub pit2_write_high: bool,
    pub pic1_mask: u8,
    pub pic2_mask: u8,
    pub pic1_vector_base: u8,
    pub pic2_vector_base: u8,
    pub pic1_in_service: u8,
    pub pic2_in_service: u8,
    pub pic1_init_step: u8,
    pub pic2_init_step: u8,
    pub pic1_expect_icw4: bool,
    pub pic2_expect_icw4: bool,
    pub cmos_index: u8,
    pub reset_requested: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcPioBus {
    serial: Uart8250,
    vga: Vec<u8>,
    pci_config_addr: u32,
    pit0_reload: u16,
    pit0_counter: u16,
    pit0_tmp_low: u8,
    pit0_read_high: bool,
    pit0_write_high: bool,
    port61: u8,
    pit2_reload: u16,
    pit2_counter: u16,
    pit2_status_reads: u32,
    pit2_tmp_low: u8,
    pit2_read_high: bool,
    pit2_write_high: bool,
    pic1_mask: u8,
    pic2_mask: u8,
    pic1_vector_base: u8,
    pic2_vector_base: u8,
    pic1_in_service: u8,
    pic2_in_service: u8,
    pic1_init_step: u8,
    pic2_init_step: u8,
    pic1_expect_icw4: bool,
    pic2_expect_icw4: bool,
    cmos_index: u8,
    reset_requested: bool,
}

impl PcPioBus {
    pub fn new() -> Self {
        Self {
            serial: Uart8250::new(),
            vga: vec![0; LEGACY_VGA_SIZE as usize],
            pci_config_addr: 0,
            pit0_reload: 0xffff,
            pit0_counter: 0xffff,
            pit0_tmp_low: 0,
            pit0_read_high: false,
            pit0_write_high: false,
            port61: 0,
            pit2_reload: 0xffff,
            pit2_counter: 0xffff,
            pit2_status_reads: 0,
            pit2_tmp_low: 0,
            pit2_read_high: false,
            pit2_write_high: false,
            pic1_mask: 0xff,
            pic2_mask: 0xff,
            pic1_vector_base: 0x20,
            pic2_vector_base: 0x28,
            pic1_in_service: 0,
            pic2_in_service: 0,
            pic1_init_step: 0,
            pic2_init_step: 0,
            pic1_expect_icw4: false,
            pic2_expect_icw4: false,
            cmos_index: 0,
            reset_requested: false,
        }
    }

    pub fn snapshot(&self) -> PcPioBusSnapshot {
        PcPioBusSnapshot {
            serial: self.serial.snapshot(),
            vga: self.vga.clone(),
            pci_config_addr: self.pci_config_addr,
            pit0_reload: self.pit0_reload,
            pit0_counter: self.pit0_counter,
            pit0_tmp_low: self.pit0_tmp_low,
            pit0_read_high: self.pit0_read_high,
            pit0_write_high: self.pit0_write_high,
            port61: self.port61,
            pit2_reload: self.pit2_reload,
            pit2_counter: self.pit2_counter,
            pit2_status_reads: self.pit2_status_reads,
            pit2_tmp_low: self.pit2_tmp_low,
            pit2_read_high: self.pit2_read_high,
            pit2_write_high: self.pit2_write_high,
            pic1_mask: self.pic1_mask,
            pic2_mask: self.pic2_mask,
            pic1_vector_base: self.pic1_vector_base,
            pic2_vector_base: self.pic2_vector_base,
            pic1_in_service: self.pic1_in_service,
            pic2_in_service: self.pic2_in_service,
            pic1_init_step: self.pic1_init_step,
            pic2_init_step: self.pic2_init_step,
            pic1_expect_icw4: self.pic1_expect_icw4,
            pic2_expect_icw4: self.pic2_expect_icw4,
            cmos_index: self.cmos_index,
            reset_requested: self.reset_requested,
        }
    }

    pub fn from_snapshot(snapshot: PcPioBusSnapshot) -> Result<Self> {
        if snapshot.vga.len() != LEGACY_VGA_SIZE as usize {
            return Err(KvmError::BadArgument(format!(
                "snapshot VGA state has {} bytes, expected {}",
                snapshot.vga.len(),
                LEGACY_VGA_SIZE
            )));
        }
        Ok(Self {
            serial: Uart8250::from_snapshot(snapshot.serial),
            vga: snapshot.vga,
            pci_config_addr: snapshot.pci_config_addr,
            pit0_reload: snapshot.pit0_reload,
            pit0_counter: snapshot.pit0_counter,
            pit0_tmp_low: snapshot.pit0_tmp_low,
            pit0_read_high: snapshot.pit0_read_high,
            pit0_write_high: snapshot.pit0_write_high,
            port61: snapshot.port61,
            pit2_reload: snapshot.pit2_reload,
            pit2_counter: snapshot.pit2_counter,
            pit2_status_reads: snapshot.pit2_status_reads,
            pit2_tmp_low: snapshot.pit2_tmp_low,
            pit2_read_high: snapshot.pit2_read_high,
            pit2_write_high: snapshot.pit2_write_high,
            pic1_mask: snapshot.pic1_mask,
            pic2_mask: snapshot.pic2_mask,
            pic1_vector_base: snapshot.pic1_vector_base,
            pic2_vector_base: snapshot.pic2_vector_base,
            pic1_in_service: snapshot.pic1_in_service,
            pic2_in_service: snapshot.pic2_in_service,
            pic1_init_step: snapshot.pic1_init_step,
            pic2_init_step: snapshot.pic2_init_step,
            pic1_expect_icw4: snapshot.pic1_expect_icw4,
            pic2_expect_icw4: snapshot.pic2_expect_icw4,
            cmos_index: snapshot.cmos_index,
            reset_requested: snapshot.reset_requested,
        })
    }

    pub fn read(&mut self, port: u16, width: u8) -> PioResult {
        self.read_at(port, width, 0)
    }

    pub fn read_at(&mut self, port: u16, width: u8, vtime: u64) -> PioResult {
        match width {
            1 => self
                .read_u8_at(port, vtime)
                .map_or(PioResult::Unsupported, |value| {
                    PioResult::Ok(u32::from(value))
                }),
            2 => {
                let Some(lo) = self.read_u8_at(port, vtime) else {
                    return PioResult::Unsupported;
                };
                let Some(hi) = self.read_u8_at(port.wrapping_add(1), vtime) else {
                    return PioResult::Unsupported;
                };
                PioResult::Ok(u32::from(lo) | (u32::from(hi) << 8))
            }
            4 => {
                if port == 0xcf8 {
                    return PioResult::Ok(self.pci_config_addr);
                }
                if port == 0xcfc {
                    return PioResult::Ok(self.pci_config_read_u32(port));
                }
                let Some(lo) = read_u16_from_pio(self, port, vtime) else {
                    return PioResult::Unsupported;
                };
                let Some(hi) = read_u16_from_pio(self, port.wrapping_add(2), vtime) else {
                    return PioResult::Unsupported;
                };
                PioResult::Ok(u32::from(lo) | (u32::from(hi) << 16))
            }
            _ => PioResult::Unsupported,
        }
    }

    pub fn write(&mut self, port: u16, width: u8, value: u32) -> PioResult {
        match width {
            1 => {
                if self.write_u8(port, value as u8) {
                    PioResult::Ok(0)
                } else {
                    PioResult::Unsupported
                }
            }
            2 => {
                if self.write_u8(port, value as u8)
                    && self.write_u8(port.wrapping_add(1), (value >> 8) as u8)
                {
                    PioResult::Ok(0)
                } else {
                    PioResult::Unsupported
                }
            }
            4 => {
                if port == 0xcf8 {
                    self.pci_config_addr = value;
                    return PioResult::Ok(0);
                }
                if port == 0xcfc {
                    return PioResult::Ok(0);
                }
                if self.write_u8(port, value as u8)
                    && self.write_u8(port.wrapping_add(1), (value >> 8) as u8)
                    && self.write_u8(port.wrapping_add(2), (value >> 16) as u8)
                    && self.write_u8(port.wrapping_add(3), (value >> 24) as u8)
                {
                    PioResult::Ok(0)
                } else {
                    PioResult::Unsupported
                }
            }
            _ => PioResult::Unsupported,
        }
    }

    pub fn queue_serial_input(&mut self, bytes: &[u8]) {
        self.serial.queue_input(bytes);
    }

    pub fn serial_output(&self) -> &[u8] {
        self.serial.output()
    }

    pub fn append_serial_output(&mut self, bytes: &[u8]) {
        self.serial.append_output(bytes);
    }

    pub fn vga_bytes(&self) -> &[u8] {
        &self.vga
    }

    pub fn vga_text_bytes(&self) -> &[u8] {
        let start = core::cmp::min(self.vga.len(), VGA_TEXT_BUFFER_OFFSET);
        let end = core::cmp::min(self.vga.len(), VGA_TEXT_BUFFER_OFFSET + VGA_TEXT_BYTES);
        &self.vga[start..end]
    }

    pub fn reset_requested(&self) -> bool {
        self.reset_requested
    }

    pub fn pending_irq_mask(&self, vtime: u64) -> u8 {
        let mut mask = 0_u8;
        if self.timer_interrupt_due(vtime) {
            mask |= 1 << 0;
        }
        if self.serial_interrupt_due() {
            mask |= 1 << 4;
        }
        mask
    }

    pub fn next_timer_vtime(&self, vtime: u64) -> Option<u64> {
        if !self.pic_irq_available(0) {
            return None;
        }
        let period = VIRTUAL_TIMER_PERIOD_TICKS;
        Some(((vtime / period).saturating_add(1)).saturating_mul(period))
    }

    pub fn acknowledge_irq(&mut self, vtime: u64) -> u8 {
        let pending = self.pending_irq_mask(vtime);
        for irq in 0..8 {
            if (pending & (1 << irq)) != 0 {
                self.pic1_in_service |= 1 << irq;
                return self.pic1_vector_base.wrapping_add(irq);
            }
        }
        0xff
    }

    pub fn mmio_read(&self, gpa: u64, width: u8) -> Option<u64> {
        let offset = self.vga_offset(gpa, width)?;
        read_le_width(&self.vga[offset..], width)
    }

    pub fn mmio_write(&mut self, gpa: u64, width: u8, value: u64) -> bool {
        let Some(offset) = self.vga_offset(gpa, width) else {
            return false;
        };
        write_le_width(&mut self.vga[offset..], width, value)
    }

    fn vga_offset(&self, gpa: u64, width: u8) -> Option<usize> {
        if !matches!(width, 1 | 2 | 4 | 8) {
            return None;
        }
        let end = gpa.checked_add(u64::from(width))?;
        let vga_end = LEGACY_VGA_BASE.checked_add(LEGACY_VGA_SIZE)?;
        if gpa < LEGACY_VGA_BASE || end > vga_end {
            return None;
        }
        usize::try_from(gpa - LEGACY_VGA_BASE).ok()
    }

    fn timer_interrupt_due(&self, vtime: u64) -> bool {
        vtime != 0 && vtime % VIRTUAL_TIMER_PERIOD_TICKS == 0 && self.pic_irq_available(0)
    }

    fn serial_interrupt_due(&self) -> bool {
        self.pic_irq_available(4) && self.serial.irq_asserted()
    }

    fn pic_irq_available(&self, irq: u8) -> bool {
        irq < 8 && (self.pic1_mask & (1 << irq)) == 0 && (self.pic1_in_service & (1 << irq)) == 0
    }

    fn read_u8_at(&mut self, port: u16, vtime: u64) -> Option<u8> {
        match port {
            0x20 | 0xa0 => Some(0),
            0x21 => Some(self.pic1_mask),
            0x40 => Some(Self::pit_read(
                &mut self.pit0_counter,
                &mut self.pit0_read_high,
            )),
            0x42 => Some(Self::pit_read(
                &mut self.pit2_counter,
                &mut self.pit2_read_high,
            )),
            0x60 => Some(0),
            0x61 => {
                self.pit2_status_reads = self.pit2_status_reads.wrapping_add(1);
                let pit2_out = if (self.port61 & 1) != 0 && self.pit2_status_reads >= 20_000 {
                    0x20
                } else {
                    0
                };
                Some((self.port61 & !0x20) | pit2_out)
            }
            0x64 => Some(0),
            0x71 => Some(self.cmos_read()),
            0x80..=0x8f => Some(0),
            0x160..=0x177 | 0x1e0..=0x1ff | 0x360..=0x377 | 0x3e0..=0x3ef | 0x3f6 => Some(0xff),
            0x279 | 0xa79 => Some(0),
            0x2e8..=0x2ef | 0x2f8..=0x2ff => Some(0xff),
            COM1_BASE..=COM1_END => Some(self.serial.read(port - COM1_BASE)),
            0xcf8..=0xcfb => Some((self.pci_config_addr >> ((port - 0xcf8) * 8)) as u8),
            0xcfc..=0xcff => {
                let value = self.pci_config_read_u32(port);
                Some((value >> ((port - 0xcfc) * 8)) as u8)
            }
            0xc000..=0xcfff => Some(self.acpi_read_u8(port, vtime)),
            0x3c0..=0x3df => Some(0),
            0x203..=0x3ff => Some(0xff),
            _ => None,
        }
    }

    fn write_u8(&mut self, port: u16, value: u8) -> bool {
        match port {
            0x20 => {
                Self::pic_command_write(
                    &mut self.pic1_init_step,
                    &mut self.pic1_expect_icw4,
                    &mut self.pic1_in_service,
                    value,
                );
                true
            }
            0xa0 => {
                Self::pic_command_write(
                    &mut self.pic2_init_step,
                    &mut self.pic2_expect_icw4,
                    &mut self.pic2_in_service,
                    value,
                );
                true
            }
            0x21 => {
                Self::pic_data_write(
                    &mut self.pic1_mask,
                    &mut self.pic1_vector_base,
                    &mut self.pic1_init_step,
                    self.pic1_expect_icw4,
                    value,
                );
                true
            }
            0xa1 => {
                Self::pic_data_write(
                    &mut self.pic2_mask,
                    &mut self.pic2_vector_base,
                    &mut self.pic2_init_step,
                    self.pic2_expect_icw4,
                    value,
                );
                true
            }
            0x40 => {
                Self::pit_write(
                    &mut self.pit0_reload,
                    &mut self.pit0_counter,
                    &mut self.pit0_tmp_low,
                    &mut self.pit0_read_high,
                    &mut self.pit0_write_high,
                    value,
                );
                true
            }
            0x42 => {
                Self::pit_write(
                    &mut self.pit2_reload,
                    &mut self.pit2_counter,
                    &mut self.pit2_tmp_low,
                    &mut self.pit2_read_high,
                    &mut self.pit2_write_high,
                    value,
                );
                if !self.pit2_write_high {
                    self.pit2_status_reads = 0;
                }
                true
            }
            0x43 => {
                match (value >> 6) & 0x3 {
                    0 => {
                        self.pit0_write_high = false;
                        self.pit0_read_high = false;
                    }
                    2 => {
                        self.pit2_write_high = false;
                        self.pit2_read_high = false;
                        self.pit2_status_reads = 0;
                    }
                    _ => {}
                }
                true
            }
            0x60 => true,
            0x61 => {
                self.port61 = value;
                self.pit2_status_reads = 0;
                true
            }
            0x64 => {
                if value == 0xfe {
                    self.reset_requested = true;
                }
                true
            }
            0x70 => {
                self.cmos_index = value;
                true
            }
            0x71 => true,
            0x80..=0x8f => true,
            0x160..=0x177 | 0x1e0..=0x1ff | 0x360..=0x377 | 0x3e0..=0x3ef | 0x3f6 => true,
            0x279 | 0xa79 => true,
            0x2e8..=0x2ef | 0x2f8..=0x2ff => true,
            COM1_BASE..=COM1_END => {
                self.serial.write(port - COM1_BASE, value);
                true
            }
            0xcf8..=0xcfb => {
                let shift = u32::from(port - 0xcf8) * 8;
                self.pci_config_addr &= !(0xff_u32 << shift);
                self.pci_config_addr |= u32::from(value) << shift;
                true
            }
            0xcfc..=0xcff => true,
            0xc000..=0xcfff => true,
            0x3c0..=0x3df => true,
            _ => false,
        }
    }

    fn acpi_read_u8(&self, port: u16, vtime: u64) -> u8 {
        let offset = port & 0x3f;
        match offset {
            0x08..=0x0b => {
                let pm_ticks = vtime.saturating_mul(3_579_545) / 40_000_000;
                ((pm_ticks & 0x00ff_ffff) >> (u32::from(offset & 0x03) * 8)) as u8
            }
            _ => 0,
        }
    }

    fn pci_config_read_u32(&self, port: u16) -> u32 {
        if (self.pci_config_addr & 0x8000_0000) == 0 || !(0xcfc..=0xcff).contains(&port) {
            return 0xffff_ffff;
        }
        0xffff_ffff
    }

    fn pit_read(counter: &mut u16, read_high: &mut bool) -> u8 {
        let value = if *read_high {
            (*counter >> 8) as u8
        } else {
            *counter as u8
        };
        if *read_high {
            *counter = (*counter).wrapping_sub(97).max(1);
        }
        *read_high = !*read_high;
        value
    }

    fn pit_write(
        reload: &mut u16,
        counter: &mut u16,
        tmp_low: &mut u8,
        read_high: &mut bool,
        write_high: &mut bool,
        value: u8,
    ) {
        if *write_high {
            *reload = ((value as u16) << 8) | u16::from(*tmp_low);
            *counter = if *reload == 0 { 0xffff } else { *reload };
        } else {
            *tmp_low = value;
        }
        *write_high = !*write_high;
        *read_high = false;
    }

    fn cmos_read(&self) -> u8 {
        match self.cmos_index & 0x7f {
            0x00 => 0,
            0x02 => 0,
            0x04 => 0,
            0x06 => 1,
            0x07 => 1,
            0x08 => 1,
            0x09 => 26,
            0x0a => 0x26,
            0x0b => 0x06,
            0x0c => 0,
            0x0d => 0x80,
            _ => 0,
        }
    }

    fn pic_command_write(
        init_step: &mut u8,
        expect_icw4: &mut bool,
        in_service: &mut u8,
        value: u8,
    ) {
        if (value & 0x10) != 0 {
            *init_step = 1;
            *expect_icw4 = (value & 0x01) != 0;
        } else if (value & 0x20) != 0 {
            if (value & 0x40) != 0 {
                *in_service &= !(1 << (value & 7));
            } else if *in_service != 0 {
                *in_service &= in_service.wrapping_sub(1);
            }
        }
    }

    fn pic_data_write(
        mask: &mut u8,
        vector_base: &mut u8,
        init_step: &mut u8,
        expect_icw4: bool,
        value: u8,
    ) {
        match *init_step {
            0 => *mask = value,
            1 => {
                *vector_base = value & 0xf8;
                *init_step = 2;
            }
            2 => {
                *init_step = if expect_icw4 { 3 } else { 0 };
            }
            _ => {
                *init_step = 0;
            }
        }
    }
}

impl Default for PcPioBus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Uart8250 {
    output: Vec<u8>,
    input: std::collections::VecDeque<u8>,
    dll: u8,
    dlh: u8,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scratch: u8,
    tx_interrupt: bool,
}

impl Uart8250 {
    fn new() -> Self {
        Self {
            output: Vec::new(),
            input: std::collections::VecDeque::new(),
            dll: 0,
            dlh: 0,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scratch: 0,
            tx_interrupt: false,
        }
    }

    fn snapshot(&self) -> Uart8250Snapshot {
        Uart8250Snapshot {
            output: self.output.clone(),
            input: self.input.iter().copied().collect(),
            dll: self.dll,
            dlh: self.dlh,
            ier: self.ier,
            lcr: self.lcr,
            mcr: self.mcr,
            scratch: self.scratch,
            tx_interrupt: self.tx_interrupt,
        }
    }

    fn from_snapshot(snapshot: Uart8250Snapshot) -> Self {
        Self {
            output: snapshot.output,
            input: snapshot.input.into(),
            dll: snapshot.dll,
            dlh: snapshot.dlh,
            ier: snapshot.ier,
            lcr: snapshot.lcr,
            mcr: snapshot.mcr,
            scratch: snapshot.scratch,
            tx_interrupt: snapshot.tx_interrupt,
        }
    }

    fn queue_input(&mut self, bytes: &[u8]) {
        self.input.extend(bytes.iter().copied());
    }

    fn output(&self) -> &[u8] {
        &self.output
    }

    fn append_output(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    fn read(&mut self, offset: u16) -> u8 {
        match offset & 7 {
            0 => {
                if self.dlab() {
                    self.dll
                } else {
                    self.input.pop_front().unwrap_or(0)
                }
            }
            1 => {
                if self.dlab() {
                    self.dlh
                } else {
                    self.ier
                }
            }
            2 => self.iir(),
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60 | u8::from(!self.input.is_empty()),
            6 => 0xb0,
            7 => self.scratch,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u16, value: u8) {
        match offset & 7 {
            0 => {
                if self.dlab() {
                    self.dll = value;
                } else {
                    self.output.push(value);
                    if (self.ier & 0x02) != 0 {
                        self.tx_interrupt = true;
                    }
                }
            }
            1 => {
                if self.dlab() {
                    self.dlh = value;
                } else {
                    let old_ier = self.ier;
                    self.ier = value & 0x0f;
                    if (self.ier & 0x02) != 0 && (old_ier & 0x02) == 0 {
                        self.tx_interrupt = true;
                    } else if (self.ier & 0x02) == 0 {
                        self.tx_interrupt = false;
                    }
                }
            }
            2 => {}
            3 => self.lcr = value,
            4 => self.mcr = value & 0x1f,
            7 => self.scratch = value,
            _ => {}
        }
    }

    fn dlab(&self) -> bool {
        (self.lcr & 0x80) != 0
    }

    fn iir(&mut self) -> u8 {
        if (self.ier & 0x01) != 0 && !self.input.is_empty() {
            0x04
        } else if (self.ier & 0x02) != 0 && self.tx_interrupt {
            self.tx_interrupt = false;
            0x02
        } else {
            0x01
        }
    }

    fn irq_asserted(&self) -> bool {
        let rx_pending = (self.ier & 0x01) != 0 && !self.input.is_empty();
        let tx_pending = (self.ier & 0x02) != 0 && self.tx_interrupt;
        (self.mcr & 0x08) != 0 && (rx_pending || tx_pending)
    }
}

impl Default for Uart8250 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn read_mailbox_request_from_bytes(mailbox: &[u8]) -> Result<MailboxRequestSnapshot> {
    let request = mailbox
        .get(MAILBOX_REQUEST_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox request exceeds backing"))?;
    Ok(MailboxRequestSnapshot {
        req_seq: read_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET)?,
        request: MonitorRequestV1 {
            kind: read_u32(request, REQUEST_KIND_OFFSET)?,
            size: read_u32(request, REQUEST_SIZE_OFFSET)?,
            seq: read_u32(request, REQUEST_SEQ_OFFSET)?,
            flags: read_u32(request, REQUEST_FLAGS_OFFSET)?,
            a: read_u64(request, REQUEST_A_OFFSET)?,
            b: read_u64(request, REQUEST_B_OFFSET)?,
            c: read_u64(request, REQUEST_C_OFFSET)?,
            d: read_u64(request, REQUEST_D_OFFSET)?,
        },
    })
}

pub fn write_request_to_mailbox(
    mailbox: &mut [u8],
    req_seq: u32,
    request: &MonitorRequestV1,
) -> Result<()> {
    let request_bytes = mailbox
        .get_mut(MAILBOX_REQUEST_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox request exceeds backing"))?;
    write_u32(request_bytes, REQUEST_KIND_OFFSET, request.kind)?;
    write_u32(request_bytes, REQUEST_SIZE_OFFSET, request.size)?;
    write_u32(request_bytes, REQUEST_SEQ_OFFSET, request.seq)?;
    write_u32(request_bytes, REQUEST_FLAGS_OFFSET, request.flags)?;
    write_u64(request_bytes, REQUEST_A_OFFSET, request.a)?;
    write_u64(request_bytes, REQUEST_B_OFFSET, request.b)?;
    write_u64(request_bytes, REQUEST_C_OFFSET, request.c)?;
    write_u64(request_bytes, REQUEST_D_OFFSET, request.d)?;
    write_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET, req_seq)
}

pub fn write_host_command_to_mailbox(
    mailbox: &mut [u8],
    command_seq: u32,
    command: &HostCommandV1,
) -> Result<()> {
    let command_bytes = mailbox
        .get_mut(MAILBOX_COMMAND_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox command exceeds backing"))?;
    write_u32(command_bytes, HOST_COMMAND_KIND_OFFSET, command.kind)?;
    write_u32(command_bytes, HOST_COMMAND_SIZE_OFFSET, command.size)?;
    write_u32(command_bytes, HOST_COMMAND_SEQ_OFFSET, command.seq)?;
    write_u32(command_bytes, HOST_COMMAND_FLAGS_OFFSET, command.flags)?;
    write_u64(command_bytes, HOST_COMMAND_A_OFFSET, command.a)?;
    write_u64(command_bytes, HOST_COMMAND_B_OFFSET, command.b)?;
    write_u64(command_bytes, HOST_COMMAND_C_OFFSET, command.c)?;
    write_u64(command_bytes, HOST_COMMAND_D_OFFSET, command.d)?;
    fence(Ordering::Release);
    write_u32_volatile(mailbox, MAILBOX_COMMAND_SEQ_OFFSET, command_seq)
}

fn write_response_to_mailbox(mailbox: &mut [u8], response: &HostResponseV1) -> Result<()> {
    let response_bytes = mailbox
        .get_mut(MAILBOX_RESPONSE_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox response exceeds backing"))?;
    write_u32(response_bytes, RESPONSE_STATUS_OFFSET, response.status)?;
    write_u32(response_bytes, RESPONSE_SIZE_OFFSET, response.size)?;
    write_u32(response_bytes, RESPONSE_SEQ_OFFSET, response.seq)?;
    write_u32(response_bytes, RESPONSE_RESERVED_OFFSET, response.reserved)?;
    write_u64(response_bytes, RESPONSE_VALUE_OFFSET, response.value)?;
    write_u64(response_bytes, RESPONSE_A_OFFSET, response.a)?;
    write_u64(response_bytes, RESPONSE_B_OFFSET, response.b)?;
    write_u64(response_bytes, RESPONSE_C_OFFSET, response.c)?;
    fence(Ordering::Release);
    write_u32_volatile(mailbox, MAILBOX_RESP_SEQ_OFFSET, response.seq)
}

pub fn read_response_status_from_mailbox(mailbox: &[u8]) -> Result<u32> {
    let response = mailbox
        .get(MAILBOX_RESPONSE_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox response exceeds backing"))?;
    read_u32(response, RESPONSE_STATUS_OFFSET)
}

pub fn read_response_value_from_mailbox(mailbox: &[u8]) -> Result<u64> {
    let response = mailbox
        .get(MAILBOX_RESPONSE_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox response exceeds backing"))?;
    read_u64(response, RESPONSE_VALUE_OFFSET)
}

fn mailbox_bytes_mut<'a>(
    plan: &MemslotPlan,
    backing: &'a mut [SlotBacking],
) -> Result<&'a mut [u8]> {
    let shared = find_slot(plan, MemslotKind::Shared)?;
    let shared_backing = backing
        .iter_mut()
        .find(|backing| backing.slot == shared.slot)
        .ok_or(KvmError::MemslotPlan("shared backing not found"))?;
    let mailbox_start = usize::try_from(SHARED_MAILBOX_OFFSET)
        .map_err(|_| KvmError::MemslotPlan("mailbox offset too large"))?;
    shared_backing
        .memory
        .as_mut_slice()
        .get_mut(mailbox_start..)
        .ok_or(KvmError::MemslotPlan(
            "mailbox offset exceeds shared backing",
        ))
}

fn find_slot(plan: &MemslotPlan, kind: MemslotKind) -> Result<&PlannedMemslot> {
    plan.slots()
        .iter()
        .find(|slot| slot.kind == kind)
        .ok_or(KvmError::MemslotPlan("required memslot kind missing"))
}

fn read_u16_from_pio(bus: &mut PcPioBus, port: u16, vtime: u64) -> Option<u16> {
    let lo = u16::from(bus.read_u8_at(port, vtime)?);
    let hi = u16::from(bus.read_u8_at(port.wrapping_add(1), vtime)?);
    Some(lo | (hi << 8))
}

fn read_le_width(bytes: &[u8], width: u8) -> Option<u64> {
    let width = usize::from(width);
    let data = bytes.get(..width)?;
    let mut value = 0_u64;
    for (index, byte) in data.iter().copied().enumerate() {
        value |= u64::from(byte) << (index * 8);
    }
    Some(value)
}

fn write_le_width(bytes: &mut [u8], width: u8, value: u64) -> bool {
    let width = usize::from(width);
    let Some(data) = bytes.get_mut(..width) else {
        return false;
    };
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = (value >> (index * 8)) as u8;
    }
    true
}

fn checked_u8(value: u64) -> Option<u8> {
    u8::try_from(value).ok()
}

fn checked_u16(value: u64) -> Option<u16> {
    u16::try_from(value).ok()
}

fn checked_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

fn low_u32(value: u64) -> u32 {
    value as u32
}

fn high_u32(value: u64) -> u32 {
    (value >> 32) as u32
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or(KvmError::MemslotPlan("mailbox u32 read exceeds backing"))?;
    let mut value = [0_u8; 4];
    for (index, byte) in value.iter_mut().enumerate() {
        unsafe {
            *byte = data.as_ptr().add(index).read_volatile();
        }
    }
    Ok(u32::from_le_bytes(value))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or(KvmError::MemslotPlan("mailbox u64 read exceeds backing"))?;
    let mut value = [0_u8; 8];
    for (index, byte) in value.iter_mut().enumerate() {
        unsafe {
            *byte = data.as_ptr().add(index).read_volatile();
        }
    }
    Ok(u64::from_le_bytes(value))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let data = bytes
        .get_mut(offset..offset + 4)
        .ok_or(KvmError::MemslotPlan("mailbox u32 write exceeds backing"))?;
    for (index, byte) in value.to_le_bytes().iter().copied().enumerate() {
        unsafe {
            data.as_mut_ptr().add(index).write_volatile(byte);
        }
    }
    Ok(())
}

fn write_u32_volatile(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let data = bytes
        .get_mut(offset..offset + 4)
        .ok_or(KvmError::MemslotPlan("mailbox u32 write exceeds backing"))?;
    let ptr = data.as_mut_ptr().cast::<u32>();
    if (ptr as usize) % core::mem::align_of::<u32>() != 0 {
        return Err(KvmError::MemslotPlan("mailbox u32 write is unaligned"));
    }
    unsafe {
        ptr.write_volatile(value.to_le());
    }
    Ok(())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<()> {
    let data = bytes
        .get_mut(offset..offset + 8)
        .ok_or(KvmError::MemslotPlan("mailbox u64 write exceeds backing"))?;
    for (index, byte) in value.to_le_bytes().iter().copied().enumerate() {
        unsafe {
            data.as_mut_ptr().add(index).write_volatile(byte);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::abi::{MAILBOX_RESP_SEQ_OFFSET, MailboxV1, ResponseStatusV1};

    #[test]
    fn pc_pio_serial_defaults_match_8250_boot_expectations() {
        let mut bus = PcPioBus::new();

        assert_eq!(bus.read(COM1_BASE + 5, 1), PioResult::Ok(0x60));
        assert_eq!(bus.read(COM1_BASE + 6, 1), PioResult::Ok(0xb0));
        assert_eq!(bus.read(COM1_BASE + 2, 1), PioResult::Ok(0x01));

        assert_eq!(bus.write(COM1_BASE, 1, b'X' as u32), PioResult::Ok(0));
        assert_eq!(bus.serial_output(), b"X");
    }

    #[test]
    fn pc_pio_serial_dlab_keeps_divisor_separate_from_data() {
        let mut bus = PcPioBus::new();

        assert_eq!(bus.write(COM1_BASE + 3, 1, 0x80), PioResult::Ok(0));
        assert_eq!(bus.write(COM1_BASE, 1, 0x34), PioResult::Ok(0));
        assert_eq!(bus.write(COM1_BASE + 1, 1, 0x12), PioResult::Ok(0));
        assert_eq!(bus.read(COM1_BASE, 1), PioResult::Ok(0x34));
        assert_eq!(bus.read(COM1_BASE + 1, 1), PioResult::Ok(0x12));
        assert!(bus.serial_output().is_empty());

        assert_eq!(bus.write(COM1_BASE + 3, 1, 0x03), PioResult::Ok(0));
        assert_eq!(bus.write(COM1_BASE, 1, b'Z' as u32), PioResult::Ok(0));
        assert_eq!(bus.serial_output(), b"Z");
    }

    #[test]
    fn pc_pio_serial_receive_and_transmit_interrupt_ids_are_stable() {
        let mut bus = PcPioBus::new();

        bus.queue_serial_input(b"a");
        assert_eq!(bus.write(COM1_BASE + 1, 1, 0x01), PioResult::Ok(0));
        assert_eq!(bus.read(COM1_BASE + 2, 1), PioResult::Ok(0x04));
        assert_eq!(bus.read(COM1_BASE, 1), PioResult::Ok(b'a' as u32));
        assert_eq!(bus.read(COM1_BASE + 2, 1), PioResult::Ok(0x01));

        assert_eq!(bus.write(COM1_BASE + 1, 1, 0x02), PioResult::Ok(0));
        assert_eq!(bus.read(COM1_BASE + 2, 1), PioResult::Ok(0x02));
        assert_eq!(bus.read(COM1_BASE + 2, 1), PioResult::Ok(0x01));
    }

    #[test]
    fn pc_pio_initializes_pic_and_requests_controller_reset() {
        let mut bus = PcPioBus::new();

        assert_eq!(bus.write(0x20, 1, 0x11), PioResult::Ok(0));
        assert_eq!(bus.write(0x21, 1, 0x20), PioResult::Ok(0));
        assert_eq!(bus.write(0x21, 1, 0x04), PioResult::Ok(0));
        assert_eq!(bus.write(0x21, 1, 0x01), PioResult::Ok(0));
        assert_eq!(bus.read(0x21, 1), PioResult::Ok(0xff));

        assert_eq!(bus.write(0x64, 1, 0xfe), PioResult::Ok(0));
        assert!(bus.reset_requested());
    }

    #[test]
    fn pc_pio_acpi_pm_timer_advances_at_its_nominal_frequency() {
        let mut bus = PcPioBus::new();

        assert_eq!(bus.read_at(0xc008, 4, 0), PioResult::Ok(0));
        assert_eq!(bus.read_at(0xc008, 4, 40_000_000), PioResult::Ok(3_579_545));
        assert_eq!(bus.read_at(0xc00a, 1, 40_000_000), PioResult::Ok(0x36));
    }

    #[test]
    fn pc_pio_vga_mmio_is_byte_addressable_without_ram_memslot() {
        let mut bus = PcPioBus::new();
        let text_base = LEGACY_VGA_BASE + VGA_TEXT_BUFFER_OFFSET as u64;

        assert!(bus.mmio_write(text_base, 1, b'D' as u64));
        assert!(bus.mmio_write(text_base + 1, 1, 0x07));
        assert_eq!(bus.mmio_read(text_base, 2), Some(0x0744));
        assert_eq!(bus.vga_text_bytes()[0], b'D');
        assert_eq!(bus.mmio_read(LEGACY_VGA_BASE - 1, 1), None);
        assert!(!bus.mmio_write(LEGACY_VGA_BASE + LEGACY_VGA_SIZE, 1, 0));
    }

    #[test]
    fn pc_pio_timer_poll_and_inta_follow_pic_mask_and_in_service() {
        let mut bus = PcPioBus::new();

        assert_eq!(bus.pending_irq_mask(100_000), 0);
        assert_eq!(bus.write(0x21, 1, 0xfe), PioResult::Ok(0));
        assert_eq!(bus.pending_irq_mask(123_456), 0);
        assert_eq!(bus.next_timer_vtime(123_456), Some(200_000));
        assert_eq!(bus.pending_irq_mask(100_000), 1);
        assert_eq!(bus.acknowledge_irq(100_000), 0x20);
        assert_eq!(bus.pending_irq_mask(200_000), 0);
        assert_eq!(bus.write(0x20, 1, 0x20), PioResult::Ok(0));
        assert_eq!(bus.pending_irq_mask(200_000), 1);
    }

    #[test]
    fn pc_pio_serial_irq_requires_mcr_out2_and_pic_unmask() {
        let mut bus = PcPioBus::new();

        bus.queue_serial_input(b"x");
        assert_eq!(bus.write(COM1_BASE + 1, 1, 0x01), PioResult::Ok(0));
        assert_eq!(bus.write(0x21, 1, 0xef), PioResult::Ok(0));
        assert_eq!(bus.pending_irq_mask(1) & (1 << 4), 0);
        assert_eq!(bus.write(COM1_BASE + 4, 1, 0x08), PioResult::Ok(0));
        assert_eq!(bus.pending_irq_mask(1) & (1 << 4), 1 << 4);
        assert_eq!(bus.acknowledge_irq(1), 0x24);
    }

    #[test]
    fn reload_boot_requires_enabled_service_and_valid_fresh_packet() {
        let mut mailbox = vec![0; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let mut request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::ReloadBoot as u32,
            seq: 1,
            ..MonitorRequestV1::NONE
        };
        write_request_to_mailbox(&mut mailbox, request.seq, &request).unwrap();
        assert_eq!(
            service.service_mailbox(&mut mailbox).unwrap().status,
            ResponseStatusV1::Unsupported as u32
        );
        service.enable_machine_boot_reload();
        request.seq = 2;
        request.a = 1;
        write_request_to_mailbox(&mut mailbox, request.seq, &request).unwrap();
        assert_eq!(
            service.service_mailbox(&mut mailbox).unwrap().status,
            ResponseStatusV1::BadPacket as u32
        );
        request.seq = 3;
        request.a = 0;
        write_request_to_mailbox(&mut mailbox, request.seq, &request).unwrap();
        assert_eq!(
            service.service_mailbox(&mut mailbox).unwrap().status,
            ResponseStatusV1::Ok as u32
        );
        assert_eq!(
            service.service_mailbox(&mut mailbox).unwrap().status,
            ResponseStatusV1::BadPacket as u32
        );
        assert!(!service.halted());
    }

    #[test]
    fn service_mailbox_publishes_serial_response() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let request = MonitorRequestV1::pio_out(1, COM1_BASE, 1, b'R' as u32, 99);
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.seq, 1);
        assert_eq!(service.serial_output(), b"R");
        assert_eq!(
            read_u32(&mailbox, MAILBOX_RESP_SEQ_OFFSET).unwrap(),
            request.seq
        );
        assert_eq!(
            read_response_status_from_mailbox(&mailbox).unwrap(),
            ResponseStatusV1::Ok as u32
        );
    }

    #[test]
    fn service_mailbox_returns_pio_in_value() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let request = MonitorRequestV1::pio_in(1, COM1_BASE + 5, 1, 123);
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.value, 0x60);
        assert_eq!(read_response_value_from_mailbox(&mailbox).unwrap(), 0x60);
    }

    #[test]
    fn service_mailbox_timer_poll_reports_pending_and_next_deadline() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();

        assert_eq!(service.pio_mut().write(0x21, 1, 0xfe), PioResult::Ok(0));
        let request = MonitorRequestV1::timer_poll(1, 123_456, true);
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.value, 0);
        let response = mailbox.get(MAILBOX_RESPONSE_OFFSET..).unwrap();
        assert_eq!(read_u64(response, RESPONSE_A_OFFSET).unwrap(), 200_000);

        let request = MonitorRequestV1::timer_poll(2, 200_000, false);
        write_request_to_mailbox(&mut mailbox, 2, &request).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.value, 1);
    }

    #[test]
    fn service_mailbox_routes_vga_mmio_requests() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let text_base = LEGACY_VGA_BASE + VGA_TEXT_BUFFER_OFFSET as u64;
        let write = MonitorRequestV1::mmio(
            1,
            MonitorRequestKindV1::MmioWrite,
            text_base,
            1,
            b'V' as u64,
        );
        write_request_to_mailbox(&mut mailbox, 1, &write).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let read = MonitorRequestV1::mmio(2, MonitorRequestKindV1::MmioRead, text_base, 1, 0);
        write_request_to_mailbox(&mut mailbox, 2, &read).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.value, b'V' as u64);
        assert_eq!(service.vga_text_bytes()[0], b'V');

        // A qword must survive both mailbox directions without losing its high half.
        let value = 0xfedc_ba98_7654_3210;
        let write =
            MonitorRequestV1::mmio(3, MonitorRequestKindV1::MmioWrite, text_base + 1, 8, value);
        write_request_to_mailbox(&mut mailbox, 3, &write).unwrap();
        assert_eq!(
            service.service_mailbox(&mut mailbox).unwrap().status,
            ResponseStatusV1::Ok as u32
        );
        let read = MonitorRequestV1::mmio(4, MonitorRequestKindV1::MmioRead, text_base + 1, 8, 0);
        write_request_to_mailbox(&mut mailbox, 4, &read).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.value, value);
        assert_eq!(&service.vga_text_bytes()[1..9], &value.to_le_bytes());
    }

    #[test]
    fn service_mailbox_serial_flush_appends_packet_bytes() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let mut bytes = [0_u8; 24];
        bytes[..21].copy_from_slice(b"R86_KVM_PIO_SMOKE_OK\n");
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::SerialFlush as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 1,
            flags: 0,
            a: 21,
            b: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            c: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            d: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        };
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(outcome.value, 21);
        assert_eq!(service.serial_output(), b"R86_KVM_PIO_SMOKE_OK\n");
    }

    #[test]
    fn host_service_publishes_stop_command_with_release_sequence() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();

        let seq = service
            .publish_stop_command_to_mailbox(&mut mailbox)
            .unwrap();
        let command = &mailbox[MAILBOX_COMMAND_OFFSET..];

        assert_eq!(seq, 1);
        assert_eq!(read_u32(&mailbox, MAILBOX_COMMAND_SEQ_OFFSET).unwrap(), seq);
        assert_eq!(
            read_u32(command, HOST_COMMAND_KIND_OFFSET).unwrap(),
            HostCommandKindV1::Stop as u32
        );
        assert_eq!(
            read_u32(command, HOST_COMMAND_SIZE_OFFSET).unwrap(),
            core::mem::size_of::<HostCommandV1>() as u32
        );
        assert_eq!(read_u32(command, HOST_COMMAND_SEQ_OFFSET).unwrap(), seq);
    }

    #[test]
    fn service_mailbox_records_request_statuses_and_hot_pio_ports() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();

        for seq in 1..=3 {
            let request = MonitorRequestV1::pio_in(seq, 0xc00a, 1, u64::from(seq));
            write_request_to_mailbox(&mut mailbox, seq, &request).unwrap();
            let outcome = service.service_mailbox(&mut mailbox).unwrap();
            assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        }

        let request = MonitorRequestV1::pio_out(4, COM1_BASE, 1, b'X' as u32, 0);
        write_request_to_mailbox(&mut mailbox, 4, &request).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let request = MonitorRequestV1::pio_in(5, 0xffff, 1, 0);
        write_request_to_mailbox(&mut mailbox, 5, &request).unwrap();
        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Unsupported as u32);

        let stats = service.request_stats();
        assert_eq!(stats.total, 5);
        assert_eq!(stats.ok, 4);
        assert_eq!(stats.unsupported, 1);
        assert_eq!(stats.kind_counts[MonitorRequestKindV1::PioIn as usize], 4);
        assert_eq!(stats.kind_counts[MonitorRequestKindV1::PioOut as usize], 1);
        assert_eq!(stats.last_seq, 5);
        assert_eq!(stats.last_kind, MonitorRequestKindV1::PioIn as u32);
        assert_eq!(stats.last_status, ResponseStatusV1::Unsupported as u32);
        assert_eq!(stats.last_a, 0xffff);
        assert!(stats.hot_pio.iter().any(|counter| {
            *counter
                == PioPortCounter {
                    port: 0xc00a,
                    width: 1,
                    is_write: false,
                    count: 3,
                    last_c: 3,
                }
        }));
        assert!(stats.hot_pio.iter().any(|counter| {
            *counter
                == PioPortCounter {
                    port: COM1_BASE,
                    width: 1,
                    is_write: true,
                    count: 1,
                    last_c: b'X' as u32,
                }
        }));
    }

    #[test]
    fn service_mailbox_records_stop_ack_state() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::StopAck as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 7,
            flags: 0,
            a: 1234,
            b: 0x0000_0202_0010_0000,
            c: 0x0012_3000_8000_0011,
            d: 0x0000_0030_0000_0000,
        };
        write_request_to_mailbox(&mut mailbox, 7, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert_eq!(
            service.stop_ack(),
            Some(MonitorStopAck {
                seq: 7,
                steps: 1234,
                eip: 0x0010_0000,
                eflags: 0x0000_0202,
                phys_writes: 0x8000_0011,
                mmio_writes: 0x0012_3000,
                pio_outs: 0,
                irq_polls: 0x0000_0030,
            })
        );
        assert!(!service.halted());
    }

    #[test]
    fn service_mailbox_records_hardmmu_stats_parts() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let cpu_packet = MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 7,
            flags: 0,
            a: 0,
            b: 0x0012_3000_8000_0011,
            c: 0x0000_0202_0000_0030,
            d: 1234,
        };
        write_request_to_mailbox(&mut mailbox, 7, &cpu_packet).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let native_packet = MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 8,
            flags: 0,
            a: 4,
            b: 0x0000_0017_0000_0016,
            c: 0x0000_0019_0000_0018,
            d: 0x0000_001b_0000_001a,
        };
        write_request_to_mailbox(&mut mailbox, 8, &native_packet).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let abort_packet = MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 9,
            flags: 0,
            a: 5,
            b: 0x0000_001e_0000_001d,
            c: 0x0000_0022_0000_001f,
            d: 0x0000_0023_0000_0020,
        };
        write_request_to_mailbox(&mut mailbox, 9, &abort_packet).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let permission_abort_packet = MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 10,
            flags: 0,
            a: 6,
            b: 0x0000_0024_0000_0021,
            c: 0,
            d: 0,
        };
        write_request_to_mailbox(&mut mailbox, 10, &permission_abort_packet).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        let cache_packet = MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 11,
            flags: 0,
            a: 7,
            b: 0x0000_0026_0000_0025,
            c: 0x0000_0028_0000_0027,
            d: 0x0000_002a_0000_0029,
        };
        write_request_to_mailbox(&mut mailbox, 11, &cache_packet).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();
        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);

        assert_eq!(
            service.hardmmu_stats(),
            Some(MonitorHardMmuStats {
                parts_seen: (1 << 0) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7),
                last_seq: 11,
                cr0: 0x8000_0011,
                cr3: 0x0012_3000,
                cr4: 0x0000_0030,
                eflags: 0x0000_0202,
                steps: 1234,
                shadow_page_table_write_flushes: 22,
                shadow_control_flushes: 23,
                shadow_invlpg_flushes: 24,
                shadow_native_read_attempts: 25,
                shadow_native_read_hits: 26,
                shadow_native_read_aborts: 27,
                bus_native_read_descriptor_aborts: 29,
                shadow_native_read_descriptor_aborts: 30,
                bus_native_read_translation_aborts: 31,
                shadow_native_read_translation_aborts: 34,
                bus_native_read_access_flag_aborts: 32,
                shadow_native_read_access_flag_aborts: 35,
                bus_native_read_permission_aborts: 33,
                shadow_native_read_permission_aborts: 36,
                cpu_fetch_translation_hits: 37,
                cpu_fetch_translation_fills: 38,
                cpu_fetch_translation_invalidations: 39,
                cpu_linear_translation_hits: 40,
                cpu_linear_translation_fills: 41,
                cpu_linear_translation_invalidations: 42,
                ..MonitorHardMmuStats::default()
            })
        );
    }

    #[test]
    fn service_mailbox_marks_bad_packet_without_panicking() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let mut request = MonitorRequestV1::pio_out(1, COM1_BASE, 1, b'R' as u32, 0);
        request.size = 4;
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::BadPacket as u32);
        assert!(service.serial_output().is_empty());
    }

    #[test]
    fn service_mailbox_records_monitor_panic_and_halts() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::Panic as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 3,
            flags: 0,
            a: 0x25_000000,
            b: 0x1000_0000,
            c: 0x2000_0000,
            d: 0x3000_0000,
        };
        write_request_to_mailbox(&mut mailbox, 3, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert!(service.halted());
        assert_eq!(service.halt_seq(), Some(3));
        assert_eq!(
            service.panic(),
            Some(MonitorPanic {
                seq: 3,
                esr: 0x25_000000,
                far: 0x1000_0000,
                elr: 0x2000_0000,
                sp: 0x3000_0000,
            })
        );
        assert_eq!(service.serial_output(), b"R86_MONITOR_PANIC\n");
    }

    #[test]
    fn service_mailbox_halt_sets_terminal_state() {
        let mut mailbox = vec![0_u8; core::mem::size_of::<MailboxV1>()];
        let mut service = MonitorHostService::new();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::Halt as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq: 1,
            flags: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        };
        write_request_to_mailbox(&mut mailbox, 1, &request).unwrap();

        let outcome = service.service_mailbox(&mut mailbox).unwrap();

        assert_eq!(outcome.status, ResponseStatusV1::Ok as u32);
        assert!(outcome.halted);
        assert_eq!(service.halt_seq(), Some(1));
    }
}
