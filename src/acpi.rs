//! Fixed ACPI 1.0 hardware for the single-CPU PIC machine.
//!
//! The FADT describes these ports; this is not a partial PIIX4 PCI function.
//! SeaBIOS relocates the tables through its fw_cfg table loader and reserves
//! their memory. Only S5 is advertised. There is no SMM: the firmware's enable
//! command switches SCI_EN directly. PM time shares the PIT oscillator (3 PM ticks per 16 device ticks).
//! The machine selects realtime or deterministic time; ACPI is independent of
//! instruction throughput.
//!
//! Register semantics: ACPI 6.5 §4.8 (legacy fixed registers). Loader wire
//! format: vendor/seabios/src/fw/romfile_loader.h; firmware/acpi/dsdt.asl owns
//! the AML. v86 acpi.js agrees on PM1 status write-one-to-clear and the 24-bit
//! timer; its timer status needs the enable bit and PM1 control is plain storage.

use alloc::{vec, vec::Vec};

pub(crate) const PM_BASE: u16 = 0x600;
const SMI_CMD: u16 = 0xb2;
const TIMER_STATUS: u16 = 1;
const POWER_BUTTON: u16 = 1 << 8;
const GLOBAL_LOCK: u16 = 1 << 5;
const RTC_EVENT: u16 = 1 << 10;
const SCI_EN: u16 = 1;
const SLP_TYP: u16 = 7 << 10;
const SLP_EN: u16 = 1 << 13;

#[derive(Debug)]
pub(crate) struct Acpi {
    enabled: bool,
    status: u16,
    event_enable: u16,
    control: u16,
    timer_epoch: u64,
    pub powered_off: bool,
}

impl Acpi {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            status: 0,
            event_enable: 0,
            control: 0,
            timer_epoch: 0,
            powered_off: false,
        }
    }

    fn ticks(step: u64) -> u64 {
        (step / 16) * 3 + (step % 16) * 3 / 16
    }

    pub fn reset_clock(&mut self, step: u64) {
        self.timer_epoch = Self::ticks(step) >> 23;
    }

    pub fn update(&mut self, step: u64) {
        if self.enabled {
            let epoch = Self::ticks(step) >> 23;
            // TMR_STS latches on either transition of the 24-bit timer's MSB.
            if epoch != self.timer_epoch {
                self.status |= TIMER_STATUS;
                self.timer_epoch = epoch;
            }
        }
    }

    pub fn next_event_step(&self) -> u64 {
        if self.control & SCI_EN == 0
            || self.event_enable & TIMER_STATUS == 0
            || self.status & TIMER_STATUS != 0
        {
            return u64::MAX;
        }
        ((self.timer_epoch + 1) << 23)
            .saturating_mul(16)
            .div_ceil(3)
    }

    pub fn irq(&self) -> bool {
        self.control & SCI_EN != 0 && self.status & self.event_enable != 0
    }

    pub fn power_button(&mut self) {
        if self.enabled && !self.powered_off {
            self.status |= POWER_BUTTON;
        }
    }

    pub fn contains_pm_ports(&self, port: u16, width: u16) -> bool {
        self.enabled
            && port >= PM_BASE
            && u32::from(port) + u32::from(width) <= u32::from(PM_BASE + 12)
    }

    pub fn read(&mut self, port: u16, step: u64) -> Option<u8> {
        if !self.enabled || !(PM_BASE..PM_BASE + 12).contains(&port) {
            return None;
        }
        self.update(step);
        let offset = port - PM_BASE;
        Some(match offset {
            0..=1 => (self.status >> (offset * 8)) as u8,
            2..=3 => (self.event_enable >> ((offset - 2) * 8)) as u8,
            4..=5 => (self.control >> ((offset - 4) * 8)) as u8,
            8..=11 => ((Self::ticks(step) & 0xffffff) >> ((offset - 8) * 8)) as u8,
            _ => 0,
        })
    }

    pub fn write(&mut self, port: u16, value: u8, step: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if port == SMI_CMD {
            match value {
                0xf1 => self.control |= SCI_EN,
                0xf0 => self.control &= !SCI_EN,
                _ => {}
            }
            return true;
        }
        if !(PM_BASE..PM_BASE + 12).contains(&port) {
            return false;
        }
        self.update(step);
        let offset = port - PM_BASE;
        let shift = (offset & 1) * 8;
        let bits = u16::from(value) << shift;
        match offset {
            0..=1 => self.status &= !bits, // write one to clear
            2..=3 => {
                self.event_enable = ((self.event_enable & !(0xff << shift)) | bits)
                    & (TIMER_STATUS | POWER_BUTTON | GLOBAL_LOCK | RTC_EVENT);
            }
            4..=5 => {
                // SCI_EN is read-only to OSPM; SLP_EN is a write-only strobe.
                // S5 uses type zero, also the value used by SeaBIOS APM.
                self.control =
                    (self.control & !(0xff << shift) | (bits & SLP_TYP)) | (self.control & SCI_EN);
                if offset == 5 && bits & SLP_EN != 0 && self.control & SLP_TYP == 0 {
                    self.powered_off = true;
                }
            }
            _ => {} // timer and unimplemented register bits are read-only
        }
        true
    }
}

/// fw_cfg files, built once with byte offsets rather than native struct layout.
/// The FACS is first and 64-byte aligned. All pointers are relocated by BIOS
/// before checksumming; no guest RAM address is guessed by the emulator.
pub(crate) fn firmware_files() -> [(&'static str, Vec<u8>); 3] {
    const TABLES: &str = "etc/acpi/tables";
    const RSDP: &str = "etc/acpi/rsdp";
    const FADT: usize = 64;
    const DSDT: usize = FADT + 116;
    const AML: &[u8] = include_bytes!("../firmware/acpi/dsdt.aml");
    let rsdt = DSDT + AML.len();
    let mut tables = vec![0; rsdt + 40];
    tables[..4].copy_from_slice(b"FACS");
    put32(&mut tables, 4, 64);
    header(&mut tables[FADT..DSDT], b"FACP");
    put32(&mut tables, FADT + 40, DSDT as u32);
    tables[FADT + 46] = 9; // SCI on the legacy slave PIC
    put32(&mut tables, FADT + 48, u32::from(SMI_CMD));
    tables[FADT + 52] = 0xf1;
    tables[FADT + 53] = 0xf0;
    put32(&mut tables, FADT + 56, u32::from(PM_BASE));
    put32(&mut tables, FADT + 64, u32::from(PM_BASE + 4));
    put32(&mut tables, FADT + 76, u32::from(PM_BASE + 8));
    tables[FADT + 88] = 4; // PM1 event block: two status + two enable bytes
    tables[FADT + 89] = 2;
    tables[FADT + 91] = 4;
    tables[FADT + 96..FADT + 100].fill(0xff); // no C2/C3
    tables[FADT + 108] = 0x32; // RTC century register
    put32(&mut tables, FADT + 112, (1 << 5) | (1 << 6)); // no sleep button/RTC wake
    tables[DSDT..rsdt].copy_from_slice(AML);
    header(&mut tables[rsdt..], b"RSDT");
    put32(&mut tables, rsdt + 36, FADT as u32);
    let mut rsdp = vec![0; 20];
    rsdp[..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"R86   ");
    put32(&mut rsdp, 16, rsdt as u32);

    let mut loader = Vec::new();
    for (file, align, zone) in [(TABLES, 64, 1), (RSDP, 16, 2)] {
        let mut command = loader_command(1, file);
        put32(&mut command, 60, align);
        command[64] = zone;
        loader.extend_from_slice(&command);
    }
    for (dest, offset) in [
        (TABLES, FADT + 36),
        (TABLES, FADT + 40),
        (TABLES, rsdt + 36),
        (RSDP, 16),
    ] {
        let mut command = loader_command(2, dest);
        command[60..60 + TABLES.len()].copy_from_slice(TABLES.as_bytes());
        put32(&mut command, 116, offset as u32);
        command[120] = 4;
        loader.extend_from_slice(&command);
    }
    for (file, start, len, checksum) in [
        (TABLES, FADT, 116, FADT + 9),
        (TABLES, rsdt, 40, rsdt + 9),
        (RSDP, 0, 20, 8),
    ] {
        let mut command = loader_command(3, file);
        put32(&mut command, 60, checksum as u32);
        put32(&mut command, 64, start as u32);
        put32(&mut command, 68, len as u32);
        loader.extend_from_slice(&command);
    }
    [(TABLES, tables), (RSDP, rsdp), ("etc/table-loader", loader)]
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn header(table: &mut [u8], signature: &[u8; 4]) {
    table[..4].copy_from_slice(signature);
    put32(table, 4, table.len() as u32);
    table[8] = 1;
    table[10..16].copy_from_slice(b"R86   ");
    table[16..24].copy_from_slice(b"R86PC   ");
    put32(table, 24, 1);
    table[28..32].copy_from_slice(b"R86 ");
    put32(table, 32, 1);
}

fn loader_command(kind: u32, file: &str) -> [u8; 128] {
    let mut command = [0; 128];
    put32(&mut command, 0, kind);
    command[4..4 + file.len()].copy_from_slice(file.as_bytes());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_wrap_and_power_button_latch_until_acknowledged() {
        let mut acpi = Acpi::new(true);
        let edge = ((1u64 << 23) * 16).div_ceil(3);
        acpi.write(PM_BASE + 2, 1, 0);
        acpi.write(PM_BASE + 3, 1, 0);
        acpi.update(edge);
        assert!(!acpi.irq()); // legacy mode
        acpi.write(SMI_CMD, 0xf1, edge);
        assert!(acpi.irq()); // the event already happened before enable
        acpi.power_button();
        acpi.write(PM_BASE, 1, edge);
        assert!(acpi.irq()); // acknowledging timer must not consume button
        acpi.write(PM_BASE + 1, 1, edge);
        assert!(!acpi.irq());
        let next = acpi.next_event_step();
        acpi.update(next - 1);
        assert!(!acpi.irq());
        assert_eq!(acpi.read(PM_BASE + 10, next - 1), Some(0xff));
        acpi.update(next);
        assert_eq!(acpi.read(PM_BASE + 10, next), Some(0));
        assert!(acpi.irq()); // 24-bit wrap is also an MSB transition
        acpi.write(SMI_CMD, 0xf0, next);
        assert!(!acpi.irq());
        assert_eq!(acpi.read(PM_BASE, next), Some(1));
    }

    #[test]
    fn only_s5_enable_strobe_cuts_power() {
        let mut acpi = Acpi::new(true);
        acpi.write(PM_BASE + 4, 1, 0);
        assert_eq!(acpi.read(PM_BASE + 4, 0), Some(0)); // SCI_EN is read-only
        acpi.write(SMI_CMD, 0xf1, 0);
        acpi.write(PM_BASE + 5, 0x24, 0); // unadvertised sleep type 1
        assert!(!acpi.powered_off);
        assert_eq!(acpi.read(PM_BASE + 5, 0), Some(4)); // SLP_EN reads as zero
        acpi.write(PM_BASE + 5, 0, 0); // selecting S5 alone is not a request
        assert!(!acpi.powered_off);
        acpi.write(PM_BASE + 4, 0, 0);
        assert_eq!(acpi.read(PM_BASE + 4, 0), Some(1));
        acpi.write(PM_BASE + 5, 0x20, 0);
        assert!(acpi.powered_off);
    }
}
