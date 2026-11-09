use crate::disk::{Disk, DiskError};
use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

const ATA_ERR_ABORT: u8 = 0x04;
const ATA_STATUS_ERR: u8 = 0x01;
const ATA_STATUS_DRQ: u8 = 0x08;
const ATA_STATUS_DSC: u8 = 0x10;
const ATA_STATUS_READY: u8 = 0x40;
const ATA_STATUS_BUSY: u8 = 0x80;

// One deterministic 100-us PIO sector service interval, in the machine's
// 16 * 1,193,182 Hz device clock. REP OUTSW transfers its entire
// buffer in one interpreter step; completion must still occur asynchronously.
// The device stays BSY until the next DRQ/completion interrupt; v86 ide.js has
// no timed BSY interval there.
const PIO_WRITE_STEPS: u64 = (16 * 1_193_182_u64).div_ceil(10_000);

const ATA_CMD_DEVICE_RESET: u8 = 0x08;
const ATA_CMD_READ_SECTORS: u8 = 0x20;
const ATA_CMD_WRITE_SECTORS: u8 = 0x30;
const ATA_CMD_READ_VERIFY: u8 = 0x40;
const ATA_CMD_EXEC_DIAG: u8 = 0x90;
const ATA_CMD_INIT_DEV_PARAMS: u8 = 0x91;
const ATA_CMD_PACKET: u8 = 0xa0;
const ATA_CMD_IDENTIFY_PACKET: u8 = 0xa1;
const ATA_CMD_FLUSH_CACHE: u8 = 0xe7;
const ATA_CMD_IDENTIFY: u8 = 0xec;
const ATA_CMD_SET_FEATURES: u8 = 0xef;

const ATAPI_PACKET_LEN: usize = 12;
const ATAPI_SECTOR_SIZE: usize = 2048;

const FW_CFG_SIGNATURE: u16 = 0x00;
const FW_CFG_ID: u16 = 0x01;
const FW_CFG_NOGRAPHIC: u16 = 0x04;
const FW_CFG_NB_CPUS: u16 = 0x05;
const FW_CFG_NUMA: u16 = 0x0d;
const FW_CFG_BOOT_MENU: u16 = 0x0e;
const FW_CFG_MAX_CPUS: u16 = 0x0f;
const FW_CFG_FILE_DIR: u16 = 0x19;
const FW_CFG_FILE_FIRST: u16 = 0x20;
const FW_CFG_IRQ0_OVERRIDE: u16 = 0x8002;
const FW_CFG_ACPI_TABLES: u16 = 0x8000;
const FW_CFG_SMBIOS_ENTRIES: u16 = 0x8001;
const FW_CFG_E820_TABLE: u16 = 0x8003;
const FW_CFG_VERSION: u32 = 0x01;

#[derive(Clone, Debug)]
pub struct FwCfgDevice {
    enabled: bool,
    entries: Vec<FwCfgEntry>,
    selected: u16,
    offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareDisplay {
    Serial,
    Vga,
}

impl FwCfgDevice {
    pub fn reset(&mut self) {
        self.selected = 0xffff;
        self.offset = 0;
    }

    pub fn absent() -> Self {
        Self {
            enabled: false,
            entries: Vec::new(),
            selected: 0xffff,
            offset: 0,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new_qemu(mem_size: usize) -> Self {
        Self::new_qemu_with_display(mem_size, FirmwareDisplay::Serial)
    }

    pub fn new_qemu_with_display(mem_size: usize, display: FirmwareDisplay) -> Self {
        let mut dev = Self::absent();
        dev.enabled = true;
        dev.add_entry(FW_CFG_SIGNATURE, b"QEMU".to_vec());
        dev.add_entry(FW_CFG_ID, FW_CFG_VERSION.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_NOGRAPHIC, 0u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_NB_CPUS, 1u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_MAX_CPUS, 1u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_NUMA, vec![0; 16]);
        dev.add_entry(FW_CFG_BOOT_MENU, 0u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_IRQ0_OVERRIDE, vec![0]);
        dev.add_entry(FW_CFG_ACPI_TABLES, 0u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_SMBIOS_ENTRIES, 0u16.to_le_bytes().to_vec());
        dev.add_entry(FW_CFG_E820_TABLE, 0u32.to_le_bytes().to_vec());

        dev.add_file("etc/e820", e820_table(mem_size));
        if display == FirmwareDisplay::Serial {
            dev.add_file("etc/sercon-port", 0x03f8u16.to_le_bytes().to_vec());
            dev.add_file("etc/screen-and-debug", 1u32.to_le_bytes().to_vec());
        }
        dev.add_file("etc/threads", 0u32.to_le_bytes().to_vec());
        dev.add_file("etc/boot-menu-wait", 0u32.to_le_bytes().to_vec());
        // An empty VM stays at SeaBIOS's "No bootable device" screen until
        // the user restarts it, instead of immediately rebooting in a loop.
        dev.add_file("etc/boot-fail-wait", u32::MAX.to_le_bytes().to_vec());
        dev.add_file("etc/s3-resume-vga-init", 0u32.to_le_bytes().to_vec());
        dev.add_file("etc/reserved-memory-end", 0u64.to_le_bytes().to_vec());
        for (name, data) in crate::acpi::firmware_files() {
            dev.add_file(name, data);
        }
        dev.rebuild_file_dir();
        dev
    }

    pub fn select(&mut self, key: u16) {
        if self.enabled {
            self.selected = key;
            self.offset = 0;
        }
    }

    pub fn read_data(&mut self) -> u8 {
        if !self.enabled {
            return 0;
        }
        let Some(index) = self.entry_index(self.selected) else {
            return 0;
        };
        let value = self.entries[index]
            .data
            .get(self.offset)
            .copied()
            .unwrap_or(0);
        self.offset = self.offset.saturating_add(1);
        value
    }

    pub fn write_data(&mut self, _value: u8) {}

    /// SeaBIOS loads this PCI device's option ROM after clearing C0000-EFFFF.
    /// Supplying it through fw_cfg avoids inventing a second PCI ROM aperture.
    /// See vendor/seabios/src/optionroms.c: init_pcirom / vgarom_setup.
    pub fn set_vga_rom(&mut self, bytes: &[u8]) {
        const NAME: &str = "pci1234,1111.rom";
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.name.as_deref() == Some(NAME))
        {
            entry.data = bytes.to_vec();
        } else {
            self.add_file(NAME, bytes.to_vec());
        }
        self.rebuild_file_dir();
    }

    #[cfg(test)]
    fn file_select(&self, name: &str) -> Option<u16> {
        self.entries.iter().find_map(|entry| {
            if entry.name.as_deref() == Some(name) {
                Some(entry.key)
            } else {
                None
            }
        })
    }

    fn add_entry(&mut self, key: u16, data: Vec<u8>) {
        if let Some(index) = self.entry_index(key) {
            self.entries[index].data = data;
            self.entries[index].name = None;
            return;
        }
        self.entries.push(FwCfgEntry {
            key,
            name: None,
            data,
        });
    }

    fn add_file(&mut self, name: &str, data: Vec<u8>) {
        let key = FW_CFG_FILE_FIRST
            + self
                .entries
                .iter()
                .filter(|entry| entry.name.is_some())
                .count() as u16;
        self.entries.push(FwCfgEntry {
            key,
            name: Some(name.to_string()),
            data,
        });
    }

    fn rebuild_file_dir(&mut self) {
        let files: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.name.is_some())
            .cloned()
            .collect();
        let mut dir = Vec::with_capacity(4 + files.len() * 64);
        dir.extend_from_slice(&(files.len() as u32).to_be_bytes());
        for file in files {
            dir.extend_from_slice(&(file.data.len() as u32).to_be_bytes());
            dir.extend_from_slice(&file.key.to_be_bytes());
            dir.extend_from_slice(&0u16.to_be_bytes());
            let mut name = [0u8; 56];
            if let Some(file_name) = file.name {
                let bytes = file_name.as_bytes();
                let len = bytes.len().min(name.len().saturating_sub(1));
                name[..len].copy_from_slice(&bytes[..len]);
            }
            dir.extend_from_slice(&name);
        }
        self.add_entry(FW_CFG_FILE_DIR, dir);
    }

    fn entry_index(&self, key: u16) -> Option<usize> {
        self.entries.iter().position(|entry| entry.key == key)
    }
}

#[derive(Clone, Debug)]
struct FwCfgEntry {
    key: u16,
    name: Option<String>,
    data: Vec<u8>,
}

fn e820_table(mem_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    push_e820(&mut out, 0, 0x000a_0000, 1);
    let high_len = (mem_size as u64).saturating_sub(0x0010_0000);
    if high_len != 0 {
        push_e820(&mut out, 0x0010_0000, high_len, 1);
    }
    out
}

fn push_e820(out: &mut Vec<u8>, addr: u64, len: u64, kind: u32) {
    out.extend_from_slice(&addr.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
}

#[derive(Clone, Debug)]
pub struct PciConfig {
    addr: u32,
    ide_command: u16,
    ide_bus_master_bar4: u32,
    ide_timing: u32,
    ide_slave_timing: u8,
    vga_command: u16,
    vga_bar0: u32,
    pub net_present: bool,
    net_command: u16,
    net_bar0: u32,
}

impl PciConfig {
    pub fn new() -> Self {
        Self {
            addr: 0,
            ide_command: 0x0005,
            ide_bus_master_bar4: 0x0000_c001,
            ide_timing: 0x8000_8000,
            ide_slave_timing: 0,
            // The board starts with its VGA decoded, including direct boots
            // without PCI firmware setup. Guests can subsequently disable it.
            vga_command: 3,
            vga_bar0: 0xe000_0000,
            net_present: false,
            net_command: 1,
            net_bar0: 0xc101,
        }
    }

    pub fn addr(&self) -> u32 {
        self.addr
    }

    pub fn write_addr(&mut self, value: u32) {
        self.addr = value;
    }

    pub fn read_data_u32(&self) -> u32 {
        if (self.addr & 0x8000_0000) == 0 {
            return 0xffff_ffff;
        }
        let bus = ((self.addr >> 16) & 0xff) as u8;
        let dev = ((self.addr >> 11) & 0x1f) as u8;
        let func = ((self.addr >> 8) & 0x07) as u8;
        let reg = (self.addr & 0xfc) as u16;
        if bus != 0 {
            return 0xffff_ffff;
        }
        let mut value = 0u32;
        for byte in 0..4 {
            value |= u32::from(self.read_config_u8(dev, func, reg + byte)) << (byte * 8);
        }
        value
    }

    /// Return whether this write changes physical memory decoding. The board
    /// must retire CPU backing pointers before the next guest memory access.
    pub fn write_data_u32(&mut self, value: u32) -> bool {
        self.write_data_masked(value, u32::MAX)
    }

    /// The host bridge supplies the byte lane within CONFIG_DATA. Do not turn a
    /// partial config write into a read/modify/write of read-only/status bits.
    pub fn write_data_u8(&mut self, lane: u8, value: u8) -> bool {
        let shift = u32::from(lane) * 8;
        self.write_data_masked(u32::from(value) << shift, 0xff << shift)
    }

    pub fn write_data_u16(&mut self, lane: u8, value: u16) -> bool {
        let shift = u32::from(lane) * 8;
        self.write_data_masked(u32::from(value) << shift, 0xffff << shift)
    }

    fn write_data_masked(&mut self, value: u32, lanes: u32) -> bool {
        if (self.addr & 0x8000_0000) == 0 {
            return false;
        }
        let bus = ((self.addr >> 16) & 0xff) as u8;
        let dev = ((self.addr >> 11) & 0x1f) as u8;
        let func = ((self.addr >> 8) & 0x07) as u8;
        let reg = (self.addr & 0xfc) as u16;
        if bus != 0 {
            return false;
        }
        let merge = |old: u32| (old & !lanes) | (value & lanes);
        // BAR size/type bits are ordinary hardwired bits, not a separate probe
        // mode. Configuration readback and decoding always use the same value.
        // IDE has legacy PIO ports and a BM BAR with interrupt status;
        // VGA has I/O/memory decode, no bus master, IRQ or capability list.
        match (dev, func, reg) {
            (3, 0, 0x04) if self.net_present => {
                self.net_command = merge(u32::from(self.net_command)) as u16 & 1
            }
            (3, 0, 0x10) if self.net_present => self.net_bar0 = (merge(self.net_bar0) & !31) | 1,
            (1, 1, 0x04) => self.ide_command = merge(u32::from(self.ide_command)) as u16 & 5,
            (1, 1, 0x20) => {
                self.ide_bus_master_bar4 = (merge(self.ide_bus_master_bar4) & !15) | 1;
            }
            // PIIX3 IDETIM: bit 15 enables each legacy channel. Firmware writes
            // this before probing, and OS drivers use it to detect disabled
            // channels. PIO timing selections are retained without simulating
            // electrical bus cycles. Bits 10/11 are reserved in each word.
            (1, 1, 0x40) => self.ide_timing = merge(self.ide_timing) & 0xf3ff_f3ff,
            (1, 1, 0x44) => self.ide_slave_timing = merge(u32::from(self.ide_slave_timing)) as u8,
            (2, 0, 0x04) => {
                let old = self.vga_command;
                self.vga_command = merge(u32::from(old)) as u16 & 3;
                return (old ^ self.vga_command) & 2 != 0;
            }
            (2, 0, 0x10) => {
                let old = self.vga_bar0;
                self.vga_bar0 = merge(self.vga_bar0) & 0xff00_0000;
                return self.vga_mem_enabled() && old != self.vga_bar0;
            }
            _ => {}
        }
        false
    }

    pub fn vga_lfb_base(&self) -> u64 {
        u64::from(self.vga_bar0 & 0xff00_0000)
    }

    pub fn net_port(&self, port: u16) -> Option<u8> {
        if !self.net_present || self.net_command & 1 == 0 {
            return None;
        }
        let base = self.net_bar0 & !31;
        let offset = u32::from(port).checked_sub(base)?;
        (base != 0 && offset < 32).then_some(offset as u8)
    }

    pub fn vga_mem_enabled(&self) -> bool {
        (self.vga_command & 0x0002) != 0
    }

    pub fn ide_bus_master_base(&self) -> Option<u16> {
        if (self.ide_command & 0x0001) == 0 {
            return None;
        }
        let base = self.ide_bus_master_bar4 & !15;
        // The x86 port space is 16-bit; a high PCI I/O address must not alias a
        // low port. Preserve that address in config space until software moves it.
        (base != 0 && base <= u32::from(u16::MAX)).then_some(base as u16)
    }

    pub fn ide_port_enabled(&self, port: u16) -> bool {
        let enable = match port {
            0x1f0..=0x1f7 | 0x3f6 => 1 << 15,
            0x170..=0x177 | 0x376 => 1 << 31,
            _ => return false,
        };
        self.ide_command & 1 != 0 && self.ide_timing & enable != 0
    }

    pub fn vga_io_enabled(&self) -> bool {
        (self.vga_command & 0x0001) != 0
    }

    fn read_config_u8(&self, dev: u8, func: u8, offset: u16) -> u8 {
        match (dev, func) {
            (0, 0) => match offset {
                0x00 => 0x86,
                0x01 => 0x80,
                0x02 => 0x37,
                0x03 => 0x12,
                0x04 => 0x07,
                0x05 => 0x00,
                0x06 => 0x00,
                0x07 => 0x00,
                0x08 => 0x02,
                0x09 => 0x00,
                0x0a => 0x00,
                0x0b => 0x06,
                0x0e => 0x00,
                0x2c => 0xf4,
                0x2d => 0x1a,
                0x2e => 0x00,
                0x2f => 0x11,
                _ => 0,
            },
            (1, 0) => match offset {
                // Fixed board wiring, matching SeaBIOS piix_isa_bridge_setup.
                0x60 | 0x61 => 10,
                0x62 | 0x63 => 11,
                0x00 => 0x86,
                0x01 => 0x80,
                0x02 => 0x00,
                0x03 => 0x70,
                0x04 => 0x07,
                0x05 => 0x00,
                0x06 => 0x00,
                0x07 => 0x00,
                0x08 => 0x00,
                0x09 => 0x00,
                0x0a => 0x01,
                0x0b => 0x06,
                0x0e => 0x80,
                _ => 0,
            },
            (1, 1) => match offset {
                0x00 => 0x86,
                0x01 => 0x80,
                0x02 => 0x10,
                0x03 => 0x70,
                0x04 => self.ide_command as u8,
                0x05 => (self.ide_command >> 8) as u8,
                0x06 => 0x80,
                0x07 => 0x02,
                0x08 => 0x00,
                0x09 => 0x80,
                0x0a => 0x01,
                0x0b => 0x01,
                0x0e => 0x00,
                0x20..=0x23 => (self.ide_bus_master_bar4 >> ((offset - 0x20) * 8)) as u8,
                0x10..=0x1f => 0x00,
                0x3c => 14,
                0x3d => 0, // Compatibility-mode channels use ISA IRQ14/15.
                0x40..=0x43 => (self.ide_timing >> ((offset - 0x40) * 8)) as u8,
                0x44 => self.ide_slave_timing,
                _ => 0,
            },
            (2, 0) => match offset {
                0x00 => 0x34,
                0x01 => 0x12,
                0x02 => 0x11,
                0x03 => 0x11,
                0x04 => self.vga_command as u8,
                0x05 => (self.vga_command >> 8) as u8,
                0x06 => 0x00,
                0x07 => 0x00,
                0x08 => 0x00,
                0x09 => 0x00,
                0x0a => 0x00,
                0x0b => 0x03,
                0x0e => 0x00,
                0x10..=0x13 => {
                    let value = self.vga_bar0 | 0x0000_0008;
                    (value >> ((offset - 0x10) * 8)) as u8
                }
                0x2c => 0xf4,
                0x2d => 0x1a,
                0x2e => 0x00,
                0x2f => 0x11,
                0x3c => 0x00,
                0x3d => 0x00,
                _ => 0,
            },
            (3, 0) if self.net_present => match offset {
                0 => 0xec,
                1 => 0x10,
                2 => 0x29,
                3 => 0x80,
                4 => self.net_command as u8,
                0x0b => 2,
                0x10..=0x13 => (self.net_bar0 >> ((offset - 0x10) * 8)) as u8,
                0x3c => crate::devices::ne2000::IRQ,
                0x3d => 1,
                _ => 0,
            },
            _ => 0xff,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeIrq {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdromState {
    pub inserted: bool,
    pub tray_open: bool,
    pub locked: bool,
}

#[derive(Debug)]
pub struct IdeBus {
    enabled: bool,
    primary: IdeChannel,
    secondary: IdeChannel,
}

impl IdeBus {
    pub fn reset(&mut self) {
        for channel in [&mut self.primary, &mut self.secondary] {
            let drives = core::mem::replace(&mut channel.drives, [None, None]);
            *channel = IdeChannel::new();
            channel.drives = drives;
            for drive in channel.drives.iter_mut().flatten() {
                drive.reset_control();
            }
            channel.reset_signature();
        }
    }

    pub fn new_absent() -> Self {
        Self {
            enabled: false,
            primary: IdeChannel::new(),
            secondary: IdeChannel::new(),
        }
    }

    pub fn new_enabled() -> Self {
        let mut bus = Self::new_absent();
        bus.enabled = true;
        bus
    }

    pub fn attach_primary_master_hdd(&mut self, image: Vec<u8>, writable: bool) {
        self.attach_primary_master_disk(Disk::memory(image, writable));
    }

    pub fn attach_primary_master_disk(&mut self, disk: Disk) {
        self.enabled = true;
        self.primary.drives[0] = Some(IdeDrive::hdd(disk));
        self.primary.reset_signature();
    }

    pub fn flush_disks(&mut self) -> Result<(), DiskError> {
        let mut result = Ok(());
        for channel in [&mut self.primary, &mut self.secondary] {
            for drive in channel.drives.iter_mut().flatten() {
                if let IdeDriveKind::Hdd { disk, .. } = &mut drive.kind {
                    if let Err(error) = disk.flush() {
                        result = Err(error);
                    }
                }
            }
        }
        result
    }

    /// The first hard disk, for host-side file management while powered off.
    pub fn first_hdd_mut(&mut self) -> Option<&mut Disk> {
        [&mut self.primary, &mut self.secondary]
            .into_iter()
            .flat_map(|channel| channel.drives.iter_mut().flatten())
            .find_map(|drive| match &mut drive.kind {
                IdeDriveKind::Hdd { disk, .. } => Some(disk),
                _ => None,
            })
    }

    pub fn attach_primary_master_cdrom(&mut self, iso: Vec<u8>) {
        self.attach_cdrom(CdromImage::Memory(iso.into()), true);
    }

    #[cfg(all(feature = "monitor", not(feature = "host")))]
    pub fn attach_primary_master_cdrom_borrowed(&mut self, iso: &'static [u8]) {
        self.enabled = true;
        self.primary.drives[0] = Some(IdeDrive {
            kind: IdeDriveKind::Cdrom {
                iso: CdromImage::Memory(alloc::borrow::Cow::Borrowed(iso)),
                tray_open: false,
                locked: false,
                media_event: 0,
            },
            sense_key: 0,
            asc: 0,
        });
        self.primary.reset_signature();
    }

    pub fn attach_secondary_master_cdrom(&mut self, iso: Vec<u8>) {
        self.attach_cdrom(CdromImage::Memory(iso.into()), false);
    }

    /// Host ISO files are read per ATAPI transfer, never loaded wholesale.
    #[cfg(feature = "host")]
    pub fn attach_file_cdrom(&mut self, file: std::fs::File, primary: bool) -> std::io::Result<()> {
        self.attach_cdrom(CdromImage::file(file)?, primary);
        Ok(())
    }

    fn attach_cdrom(&mut self, iso: CdromImage, primary: bool) {
        self.enabled = true;
        let channel = if primary {
            &mut self.primary
        } else {
            &mut self.secondary
        };
        channel.drives[0] = Some(IdeDrive::cdrom(iso));
        channel.reset_signature();
    }

    // The PC frontends configure one optical drive, on either IDE channel.
    pub fn cdrom_state(&self) -> Option<CdromState> {
        for channel in [&self.primary, &self.secondary] {
            for drive in channel.drives.iter().flatten() {
                if let IdeDriveKind::Cdrom {
                    iso,
                    tray_open,
                    locked,
                    ..
                } = &drive.kind
                {
                    return Some(CdromState {
                        inserted: !tray_open && iso.len() >= ATAPI_SECTOR_SIZE,
                        tray_open: *tray_open,
                        locked: *locked,
                    });
                }
            }
        }
        None
    }

    /// Replace the medium without resetting the drive or its IDE channel.
    /// An empty image removes the disc and leaves the tray open.
    pub fn change_cdrom(&mut self, image: Vec<u8>) -> Result<(), &'static str> {
        let empty = image.is_empty();
        self.change_cdrom_image(CdromImage::Memory(image.into()), empty)
    }

    /// Insert a host ISO file under the same rules as `change_cdrom`.
    #[cfg(feature = "host")]
    pub fn change_file_cdrom(&mut self, file: std::fs::File) -> Result<(), String> {
        let iso = CdromImage::file(file).map_err(|error| error.to_string())?;
        self.change_cdrom_image(iso, false).map_err(str::to_string)
    }

    /// Whether `change_cdrom` would accept a new medium now. Lets a caller keep
    /// an uploaded image when the guest refuses the change.
    pub fn cdrom_change_allowed(&self) -> Result<(), &'static str> {
        for channel in [&self.primary, &self.secondary] {
            for drive in channel.drives.iter().flatten() {
                let IdeDriveKind::Cdrom {
                    tray_open, locked, ..
                } = &drive.kind
                else {
                    continue;
                };
                if *locked && !*tray_open {
                    return Err(
                        "The guest has locked the CD/DVD drive. Eject or unmount the disc in the guest first.",
                    );
                }
                // An in-flight PIO reply references the current ISO. Do not
                // replace it mid-command or return sectors from two discs.
                if channel.status & (ATA_STATUS_BUSY | ATA_STATUS_DRQ) != 0 {
                    return Err(
                        "The CD/DVD drive is busy. Wait for the current read to finish and try again.",
                    );
                }
                return Ok(());
            }
        }
        Err("This virtual machine has no CD/DVD drive.")
    }

    fn change_cdrom_image(&mut self, image: CdromImage, empty: bool) -> Result<(), &'static str> {
        self.cdrom_change_allowed()?;
        for channel in [&mut self.primary, &mut self.secondary] {
            for drive in channel.drives.iter_mut().flatten() {
                let IdeDriveKind::Cdrom {
                    iso,
                    tray_open,
                    media_event,
                    ..
                } = &mut drive.kind
                else {
                    continue;
                };
                *iso = image;
                *tray_open = empty;
                *media_event = if empty { 3 } else { 2 };
                // tiny386 ide_change_cd also reports UNIT ATTENTION / medium
                // changed. Keep our existing polled ATAPI media-event contract;
                // a host tray operation is not a guest command completion IRQ.
                drive.sense_key = 6;
                drive.asc = 0x28;
                return Ok(());
            }
        }
        unreachable!("cdrom_change_allowed found the drive")
    }

    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    pub fn attach_browser_cdrom(&mut self, drive: u32, bytes: usize, primary: bool) {
        self.attach_cdrom(CdromImage::Browser { drive, bytes }, primary);
    }

    pub fn irq_asserted(&self, irq: IdeIrq) -> bool {
        let channel = match irq {
            IdeIrq::Primary => &self.primary,
            IdeIrq::Secondary => &self.secondary,
        };
        self.enabled && channel.irq_pending && channel.control & 0x06 == 0
    }

    // PIO drivers also use BM status to identify their interrupt. The latch
    // survives ATA status reads and is acknowledged separately by writing 1.
    // Consulted tiny386's IRQ pin handling, then v86 IDEChannel.push_irq /
    // dma_write_status, which agree on both rules.
    // DMA is not advertised by either drive; its engine remains inactive.
    pub fn read_bus_master_u8(&self, offset: u8) -> u8 {
        match offset {
            2 => u8::from(self.primary.bus_master_irq) << 2,
            10 => u8::from(self.secondary.bus_master_irq) << 2,
            _ => 0,
        }
    }

    pub fn write_bus_master_u8(&mut self, offset: u8, value: u8) {
        if value & 4 != 0 {
            match offset {
                2 => self.primary.bus_master_irq = false,
                10 => self.secondary.bus_master_irq = false,
                _ => {}
            }
        }
    }

    pub fn advance(&mut self, step: u64) {
        self.primary.advance(step);
        self.secondary.advance(step);
    }

    pub fn next_event_step(&self) -> u64 {
        self.primary
            .write_complete_step
            .min(self.secondary.write_complete_step)
    }

    pub fn read_u8(&mut self, port: u16) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        let (channel, offset, alt) = self.channel_for_port_mut(port)?;
        if alt {
            Some(channel.alt_status())
        } else {
            Some(channel.read_reg(offset))
        }
    }

    pub fn read_u16(&mut self, port: u16) -> Option<u16> {
        if !self.enabled {
            return None;
        }
        // Only the data FIFO has wide cycles. Probing task-file byte handlers
        // first can acknowledge an IRQ or issue a command before returning
        // unhandled. Match tiny386/v86's per-width dispatch instead.
        Some(self.data_channel_for_port_mut(port)?.read_data_u16())
    }

    pub fn read_u32(&mut self, port: u16) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        Some(self.data_channel_for_port_mut(port)?.read_data_u32())
    }

    pub fn read_to_slice_supported(&self, port: u16, len: usize) -> bool {
        if !self.enabled || len == 0 {
            return false;
        }
        self.data_channel_for_port(port)
            .is_some_and(|channel| channel.can_read_data_bytes(len))
    }

    pub fn read_to_slice(&mut self, port: u16, dst: &mut [u8]) -> bool {
        if !self.enabled || dst.is_empty() {
            return false;
        }
        let Some(channel) = self.data_channel_for_port_mut(port) else {
            return false;
        };
        channel.read_data_to_slice(dst)
    }

    pub fn write_u8(&mut self, port: u16, value: u8) -> bool {
        if !self.enabled {
            return false;
        }
        let Some((channel, offset, alt)) = self.channel_for_port_mut(port) else {
            return false;
        };
        if alt {
            channel.write_control(value);
        } else {
            channel.write_reg(offset, value);
        }
        true
    }

    pub fn write_u16(&mut self, port: u16, value: u16) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(channel) = self.data_channel_for_port_mut(port) else {
            return false;
        };
        channel.write_data_u16(value);
        true
    }

    pub fn write_u32(&mut self, port: u16, value: u32) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(channel) = self.data_channel_for_port_mut(port) else {
            return false;
        };
        channel.write_data_u32(value);
        true
    }

    fn channel_for_port_mut(&mut self, port: u16) -> Option<(&mut IdeChannel, u8, bool)> {
        match port {
            0x1f0..=0x1f7 => Some((&mut self.primary, (port - 0x1f0) as u8, false)),
            0x170..=0x177 => Some((&mut self.secondary, (port - 0x170) as u8, false)),
            0x3f6 => Some((&mut self.primary, 7, true)),
            0x376 => Some((&mut self.secondary, 7, true)),
            _ => None,
        }
    }

    fn data_channel_for_port(&self, port: u16) -> Option<&IdeChannel> {
        match port {
            0x1f0 => Some(&self.primary),
            0x170 => Some(&self.secondary),
            _ => None,
        }
    }

    fn data_channel_for_port_mut(&mut self, port: u16) -> Option<&mut IdeChannel> {
        match port {
            0x1f0 => Some(&mut self.primary),
            0x170 => Some(&mut self.secondary),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct IdeChannel {
    drives: [Option<IdeDrive>; 2],
    selected: usize,
    error: u8,
    features: u8,
    sector_count: u8,
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    device_head: u8,
    status: u8,
    control: u8,
    data: Vec<u8>,
    data_pos: usize,
    atapi_reply: AtapiData,
    atapi_reply_pos: usize,
    packet: Vec<u8>,
    write_buf: Vec<u8>,
    hdd_bytes_remaining: usize,
    hdd_lba: u64,
    hdd_drive: usize,
    transfer_kind: IdeTransferKind,
    // Completion is latched independently of nIEN. Mask changes only drive
    // the pin; command-block status acknowledges it, alternate status does not.
    // Consulted tiny386 ide.c. v86 ide.js agrees on the status reads but does
    // not latch masked completions for later unmasking.
    irq_pending: bool,
    bus_master_irq: bool,
    step: u64,
    write_complete_step: u64,
}

impl IdeChannel {
    fn new() -> Self {
        Self {
            drives: core::array::from_fn(|_| None),
            selected: 0,
            error: 0x01,
            features: 0,
            sector_count: 1,
            lba_low: 1,
            lba_mid: 0,
            lba_high: 0,
            device_head: 0xa0,
            status: ATA_STATUS_READY | ATA_STATUS_DSC,
            control: 0,
            data: Vec::new(),
            data_pos: 0,
            atapi_reply: AtapiData::Reply(Vec::new()),
            atapi_reply_pos: 0,
            packet: Vec::new(),
            write_buf: Vec::new(),
            hdd_bytes_remaining: 0,
            hdd_lba: 0,
            hdd_drive: 0,
            transfer_kind: IdeTransferKind::None,
            irq_pending: false,
            bus_master_irq: false,
            step: 0,
            write_complete_step: u64::MAX,
        }
    }

    fn advance(&mut self, step: u64) {
        self.step = step;
        if self.write_complete_step == u64::MAX || step < self.write_complete_step {
            return;
        }
        self.write_complete_step = u64::MAX;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC;
        if self.hdd_bytes_remaining != 0 {
            self.status |= ATA_STATUS_DRQ;
        } else {
            self.transfer_kind = IdeTransferKind::None;
        }
        self.raise_irq();
    }

    fn selected_drive(&self) -> Option<&IdeDrive> {
        self.drives.get(self.selected)?.as_ref()
    }

    fn selected_drive_mut(&mut self) -> Option<&mut IdeDrive> {
        self.drives.get_mut(self.selected)?.as_mut()
    }

    fn clear_data_transfer(&mut self) {
        self.write_complete_step = u64::MAX;
        self.data.clear();
        self.data_pos = 0;
        self.atapi_reply.clear();
        self.atapi_reply_pos = 0;
    }

    fn reset_signature(&mut self) {
        if self.selected_drive().is_none() {
            self.reset_absent_state();
            return;
        }
        self.device_head &= 0xf0;
        self.error = 0x01;
        self.sector_count = 1;
        self.lba_low = 1;
        if matches!(
            self.selected_drive().map(|drive| &drive.kind),
            Some(IdeDriveKind::Cdrom { .. })
        ) {
            self.lba_mid = 0x14;
            self.lba_high = 0xeb;
        } else {
            self.lba_mid = 0;
            self.lba_high = 0;
        }
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        self.irq_pending = false;
    }

    fn reset_absent_state(&mut self) {
        self.error = 0;
        self.features = 0;
        self.sector_count = 0;
        self.lba_low = 0;
        self.lba_mid = 0;
        self.lba_high = 0;
        self.status = 0;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        self.irq_pending = false;
    }

    fn alt_status(&self) -> u8 {
        if self.selected_drive().is_none() {
            return 0;
        }
        self.status
    }

    fn read_reg(&mut self, offset: u8) -> u8 {
        if self.selected_drive().is_none() {
            return 0;
        }
        match offset {
            0 => self.read_data_u8(),
            1 => self.error,
            2 => self.sector_count,
            3 => self.lba_low,
            4 => self.lba_mid,
            5 => self.lba_high,
            6 => self.device_head,
            7 => {
                self.irq_pending = false;
                self.status
            }
            _ => 0xff,
        }
    }

    fn write_reg(&mut self, offset: u8, value: u8) {
        if self.status & ATA_STATUS_BUSY != 0 || self.control & 0x04 != 0 {
            return;
        }
        if offset != 6 && self.selected_drive().is_none() {
            return;
        }
        match offset {
            0 => self.write_data_u8(value),
            1 => self.features = value,
            2 => self.sector_count = value,
            3 => self.lba_low = value,
            4 => self.lba_mid = value,
            5 => self.lba_high = value,
            6 => {
                let selected = ((value >> 4) & 1) as usize;
                if selected != self.selected {
                    self.selected = selected;
                    self.reset_signature();
                }
                // Hosts may write DEVICE last when loading the task file.
                // Re-selecting this drive must preserve count and address;
                // resetting them here turns Linux's multi-sector I/O into a
                // single-sector command. Preserve the LBA/CHS head bits too.
                self.device_head = value;
            }
            7 => self.exec_command(value),
            _ => {}
        }
    }

    fn write_control(&mut self, value: u8) {
        let old = self.control;
        self.control = value;
        if (old & 0x04) == 0 && (value & 0x04) != 0 {
            for drive in self.drives.iter_mut().flatten() {
                drive.reset_control();
            }
            if self.selected_drive().is_some() {
                self.status = ATA_STATUS_BUSY;
            } else {
                self.status = 0;
            }
            self.clear_data_transfer();
            self.packet.clear();
            self.write_buf.clear();
            self.hdd_bytes_remaining = 0;
            self.transfer_kind = IdeTransferKind::None;
            self.irq_pending = false;
        } else if (old & 0x04) != 0 && (value & 0x04) == 0 {
            self.reset_signature();
        }
        if old & 0x02 != 0 && value & 0x06 == 0 && self.irq_pending {
            self.bus_master_irq = true;
        }
    }

    fn exec_command(&mut self, command: u8) {
        self.error = 0;
        self.irq_pending = false;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        match command {
            ATA_CMD_IDENTIFY => self.identify(false),
            ATA_CMD_IDENTIFY_PACKET => self.identify(true),
            ATA_CMD_PACKET => self.begin_packet(),
            ATA_CMD_READ_SECTORS => self.read_hdd(false),
            ATA_CMD_READ_VERIFY => self.read_hdd(true),
            ATA_CMD_WRITE_SECTORS => self.begin_hdd_write(),
            ATA_CMD_FLUSH_CACHE => self.flush_hdd(false),
            ATA_CMD_SET_FEATURES if self.features == 0x82 => self.flush_hdd(true),
            ATA_CMD_SET_FEATURES if self.features == 0x02 => {
                if let Some(IdeDrive {
                    kind: IdeDriveKind::Hdd { write_cache, .. },
                    ..
                }) = self.selected_drive_mut()
                {
                    *write_cache = true;
                    self.command_ok();
                } else {
                    self.abort();
                }
            }
            ATA_CMD_SET_FEATURES if self.features == 0x03 => {
                // This controller implements PIO only. Reject transfer modes
                // that IDENTIFY does not advertise instead of accepting DMA.
                let max_pio = match self.selected_drive().map(|drive| &drive.kind) {
                    Some(IdeDriveKind::Cdrom { .. }) => 3,
                    _ => 2,
                };
                if self.sector_count <= 1 || (0x08..=0x08 + max_pio).contains(&self.sector_count) {
                    self.command_ok();
                } else {
                    self.abort();
                }
            }
            ATA_CMD_INIT_DEV_PARAMS => self.init_hdd_geometry(),
            ATA_CMD_EXEC_DIAG => {
                self.reset_signature();
                self.raise_irq();
            }
            ATA_CMD_DEVICE_RESET => self.device_reset_command(),
            _ => self.abort(),
        }
    }

    fn device_reset_command(&mut self) {
        if !matches!(
            self.selected_drive().map(|drive| &drive.kind),
            Some(IdeDriveKind::Cdrom { .. })
        ) {
            self.abort();
            return;
        }
        self.selected_drive_mut().unwrap().reset_control();
        self.device_head &= 0xf0;
        self.sector_count = 1;
        self.lba_low = 1;
        self.lba_mid = 0x14;
        self.lba_high = 0xeb;
        self.status = 0;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        self.irq_pending = false;
    }

    fn command_ok(&mut self) {
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        self.raise_irq();
    }

    fn abort(&mut self) {
        self.error = ATA_ERR_ABORT;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_ERR;
        self.clear_data_transfer();
        self.packet.clear();
        self.write_buf.clear();
        self.hdd_bytes_remaining = 0;
        self.transfer_kind = IdeTransferKind::None;
        self.raise_irq();
    }

    fn identify(&mut self, packet: bool) {
        let Some(drive) = self.selected_drive() else {
            self.abort();
            return;
        };
        let is_packet = matches!(drive.kind, IdeDriveKind::Cdrom { .. });
        if packet != is_packet {
            self.abort();
            return;
        }
        self.start_read_buffer(drive.identify(), IdeTransferKind::Identify);
    }

    fn begin_packet(&mut self) {
        if self.features & 1 != 0
            || !matches!(
                self.selected_drive().map(|drive| &drive.kind),
                Some(IdeDriveKind::Cdrom { .. })
            )
        {
            self.abort();
            return;
        }
        self.packet.clear();
        self.clear_data_transfer();
        self.sector_count = 0x01;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_DRQ;
    }

    fn disk_error(&mut self, error: DiskError) {
        #[cfg(feature = "host")]
        if let DiskError::Io(ref source) = error {
            eprintln!("IDE disk I/O failed at LBA {}: {source}", self.hdd_lba);
        }
        self.abort();
        self.error = match error {
            DiskError::OutOfRange => 0x10, // ID not found
            DiskError::ReadOnly => ATA_ERR_ABORT,
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            DiskError::BrowserIo => 0x40,
            #[cfg(feature = "host")]
            DiskError::Io(_) => 0x40, // uncorrectable data / host I/O failure
        };
    }

    fn flush_hdd(&mut self, disable_cache: bool) {
        let Some(IdeDrive {
            kind: IdeDriveKind::Hdd {
                disk, write_cache, ..
            },
            ..
        }) = self.selected_drive_mut()
        else {
            self.abort();
            return;
        };
        match disk.flush() {
            Ok(()) => {
                if disable_cache {
                    *write_cache = false;
                }
                self.command_ok();
            }
            Err(error) => self.disk_error(error),
        }
    }

    fn init_hdd_geometry(&mut self) {
        let heads = (self.device_head & 15) + 1;
        let sectors = self.sector_count;
        if sectors == 0 || sectors > 63 {
            self.abort();
            return;
        }
        if let Some(IdeDrive {
            kind:
                IdeDriveKind::Hdd {
                    heads: h,
                    track_sectors,
                    ..
                },
            ..
        }) = self.selected_drive_mut()
        {
            *h = heads;
            *track_sectors = sectors;
            self.command_ok();
        } else {
            self.abort();
        }
    }

    fn begin_hdd_transfer(&mut self, write: bool) -> bool {
        let Some(IdeDrive {
            kind:
                IdeDriveKind::Hdd {
                    disk,
                    heads,
                    track_sectors,
                    ..
                },
            ..
        }) = self.selected_drive()
        else {
            self.abort();
            return false;
        };
        if write && !disk.writable() {
            self.disk_error(DiskError::ReadOnly);
            return false;
        }
        let lba = if self.device_head & 0x40 != 0 {
            self.current_lba()
        } else {
            let head = self.device_head & 15;
            if head >= *heads || self.lba_low == 0 || self.lba_low > *track_sectors {
                self.disk_error(DiskError::OutOfRange);
                return false;
            }
            let cylinder = u16::from_le_bytes([self.lba_mid, self.lba_high]);
            (u64::from(cylinder) * u64::from(*heads) + u64::from(head)) * u64::from(*track_sectors)
                + u64::from(self.lba_low - 1)
        };
        if lba >= disk.sectors() {
            self.disk_error(DiskError::OutOfRange);
            return false;
        }
        self.hdd_lba = lba;
        self.hdd_drive = self.selected;
        self.hdd_bytes_remaining = self.current_count() as usize * 512;
        true
    }

    fn read_hdd(&mut self, verify_only: bool) {
        if !self.begin_hdd_transfer(false) {
            return;
        }
        if verify_only {
            let mut sector = [0; 512];
            while self.hdd_bytes_remaining != 0 {
                let IdeDriveKind::Hdd { disk, .. } =
                    &mut self.drives[self.hdd_drive].as_mut().unwrap().kind
                else {
                    unreachable!()
                };
                if let Err(error) = disk.read(self.hdd_lba, &mut sector) {
                    self.disk_error(error);
                    return;
                }
                self.advance_hdd_sector();
            }
            self.command_ok();
        } else {
            self.read_next_hdd_sector();
        }
    }

    fn read_next_hdd_sector(&mut self) {
        let mut out = vec![0; 512];
        let IdeDriveKind::Hdd { disk, .. } =
            &mut self.drives[self.hdd_drive].as_mut().unwrap().kind
        else {
            unreachable!()
        };
        match disk.read(self.hdd_lba, &mut out) {
            Ok(()) => self.start_read_buffer(out, IdeTransferKind::HddRead),
            Err(error) => self.disk_error(error),
        }
    }

    fn begin_hdd_write(&mut self) {
        if !self.begin_hdd_transfer(true) {
            return;
        }
        self.transfer_kind = IdeTransferKind::HddWrite;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_DRQ;
    }

    fn commit_hdd_write_if_ready(&mut self) {
        if self.write_buf.len() < 512 {
            return;
        }
        let IdeDriveKind::Hdd {
            disk, write_cache, ..
        } = &mut self.drives[self.hdd_drive].as_mut().unwrap().kind
        else {
            unreachable!()
        };
        let result = disk
            .write(self.hdd_lba, &self.write_buf)
            .and_then(|()| if *write_cache { Ok(()) } else { disk.flush() });
        if let Err(error) = result {
            self.disk_error(error);
            return;
        }
        self.write_buf.clear();
        self.advance_hdd_sector();
        self.status = ATA_STATUS_BUSY;
        self.write_complete_step = self.step.saturating_add(PIO_WRITE_STEPS);
    }

    // READ/WRITE SECTORS transfers one sector per DRQ/IRQ phase. A reset or
    // error discards only the incomplete sector; completed sectors stay written.
    fn advance_hdd_sector(&mut self) {
        self.hdd_bytes_remaining -= 512;
        self.sector_count = (self.hdd_bytes_remaining / 512) as u8;
        self.hdd_lba += 1;
        if self.device_head & 0x40 != 0 {
            self.lba_low = self.hdd_lba as u8;
            self.lba_mid = (self.hdd_lba >> 8) as u8;
            self.lba_high = (self.hdd_lba >> 16) as u8;
            self.device_head = (self.device_head & 0xf0) | ((self.hdd_lba >> 24) as u8 & 15);
        } else {
            let IdeDriveKind::Hdd {
                heads,
                track_sectors,
                ..
            } = &self.drives[self.hdd_drive].as_ref().unwrap().kind
            else {
                unreachable!()
            };
            let track = self.hdd_lba / u64::from(*track_sectors);
            let cylinder = track / u64::from(*heads);
            self.lba_low = (self.hdd_lba % u64::from(*track_sectors)) as u8 + 1;
            self.device_head = (self.device_head & 0xf0) | (track % u64::from(*heads)) as u8;
            self.lba_mid = cylinder as u8;
            self.lba_high = (cylinder >> 8) as u8;
        }
    }

    fn exec_packet_if_ready(&mut self) {
        if self.packet.len() < ATAPI_PACKET_LEN {
            return;
        }
        let mut cdb = [0; ATAPI_PACKET_LEN];
        cdb.copy_from_slice(&self.packet[..ATAPI_PACKET_LEN]);
        self.packet.clear();
        let Some(drive) = self.selected_drive_mut() else {
            self.abort();
            return;
        };
        let reply = drive.exec_packet(cdb);
        match reply {
            PacketReply::Data(data) => self.start_packet_data(data),
            PacketReply::Ok => self.packet_ok(),
            PacketReply::CheckCondition { sense, asc } => self.packet_error(sense, asc),
        }
    }

    fn packet_error(&mut self, sense: u8, asc: u8) {
        if let Some(drive) = self.selected_drive_mut() {
            drive.sense_key = sense;
            drive.asc = asc;
        }
        self.clear_data_transfer();
        self.transfer_kind = IdeTransferKind::None;
        self.error = (sense << 4) | ATA_ERR_ABORT;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_ERR;
        self.sector_count = 0x03; // command completion, device to host
        self.raise_irq();
    }

    fn packet_ok(&mut self) {
        self.sector_count = 0x03;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC;
        self.clear_data_transfer();
        self.raise_irq();
    }

    fn start_packet_data(&mut self, data: AtapiData) {
        if data.len() == 0 {
            self.packet_ok();
            return;
        }
        self.atapi_reply = data;
        self.atapi_reply_pos = 0;
        self.start_next_atapi_chunk();
    }

    fn next_atapi_chunk_len(&self, remaining: usize) -> usize {
        let mut limit = usize::from(u16::from_le_bytes([self.lba_mid, self.lba_high]));
        if limit == 0xffff {
            limit -= 1;
        }
        if remaining > limit && (limit & 1) != 0 {
            limit -= 1;
        }
        remaining.min(limit)
    }

    fn start_next_atapi_chunk(&mut self) {
        let remaining = self.atapi_reply.len().saturating_sub(self.atapi_reply_pos);
        if remaining == 0 {
            self.packet_ok();
            return;
        }
        let len = self.next_atapi_chunk_len(remaining);
        if len == 0 {
            // A zero byte limit (or an odd limit rounded down to zero) cannot
            // transfer this reply. It must not discard data and claim success.
            self.packet_error(5, 0x24);
            return;
        }
        let start = self.atapi_reply_pos;
        let end = start + len;
        self.data.clear();
        match &self.atapi_reply {
            AtapiData::Reply(bytes) => self.data.extend_from_slice(&bytes[start..end]),
            AtapiData::Cdrom(range) => {
                let IdeDriveKind::Cdrom { iso, .. } =
                    &self.drives[self.selected].as_ref().unwrap().kind
                else {
                    unreachable!()
                };
                if iso
                    .read_into(range.start + start, len, &mut self.data)
                    .is_err()
                {
                    self.packet_error(3, 0x11); // MEDIUM ERROR / unrecovered read error
                    return;
                }
            }
        }
        self.data_pos = 0;
        self.atapi_reply_pos = end;
        self.lba_mid = len as u8;
        self.lba_high = (len >> 8) as u8;
        self.sector_count = 0x02;
        self.transfer_kind = IdeTransferKind::AtapiData;
        self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_DRQ;
        self.raise_irq();
    }

    fn start_read_buffer(&mut self, data: Vec<u8>, kind: IdeTransferKind) {
        self.data = data;
        self.data_pos = 0;
        self.transfer_kind = kind;
        if self.data.is_empty() {
            self.status = ATA_STATUS_READY | ATA_STATUS_DSC;
            self.transfer_kind = IdeTransferKind::None;
        } else {
            self.status = ATA_STATUS_READY | ATA_STATUS_DSC | ATA_STATUS_DRQ;
        }
        self.raise_irq();
    }

    fn raise_irq(&mut self) {
        self.irq_pending = true;
        if self.control & 0x06 == 0 {
            self.bus_master_irq = true;
        }
    }

    fn read_data_u8(&mut self) -> u8 {
        if self.selected_drive().is_none() || self.status & ATA_STATUS_DRQ == 0 {
            return 0;
        }
        let value = self.data.get(self.data_pos).copied().unwrap_or(0);
        if self.data_pos < self.data.len() {
            self.data_pos += 1;
        }
        self.finish_data_if_drained();
        value
    }

    fn read_data_u16(&mut self) -> u16 {
        let lo = self.read_data_u8() as u16;
        let hi = self.read_data_u8() as u16;
        lo | (hi << 8)
    }

    fn read_data_u32(&mut self) -> u32 {
        let lo = self.read_data_u16() as u32;
        let hi = self.read_data_u16() as u32;
        lo | (hi << 16)
    }

    fn can_read_data_bytes(&self, len: usize) -> bool {
        if self.selected_drive().is_none() {
            return false;
        }
        (self.status & ATA_STATUS_DRQ) != 0
            && self
                .data_pos
                .checked_add(len)
                .is_some_and(|end| end <= self.data.len())
    }

    fn read_data_to_slice(&mut self, dst: &mut [u8]) -> bool {
        if !self.can_read_data_bytes(dst.len()) {
            return false;
        }
        let end = self.data_pos + dst.len();
        dst.copy_from_slice(&self.data[self.data_pos..end]);
        self.data_pos = end;
        self.finish_data_if_drained();
        true
    }

    fn write_data_u8(&mut self, value: u8) {
        if self.selected_drive().is_none() {
            return;
        }
        if (self.status & ATA_STATUS_DRQ) == 0 {
            return;
        }
        if matches!(
            self.selected_drive().map(|drive| &drive.kind),
            Some(IdeDriveKind::Cdrom { .. })
        ) && self.hdd_bytes_remaining == 0
        {
            self.packet.push(value);
            self.exec_packet_if_ready();
        } else if self.transfer_kind == IdeTransferKind::HddWrite {
            self.write_buf.push(value);
            self.commit_hdd_write_if_ready();
        }
    }

    fn write_data_u16(&mut self, value: u16) {
        self.write_data_u8(value as u8);
        self.write_data_u8((value >> 8) as u8);
    }

    fn write_data_u32(&mut self, value: u32) {
        self.write_data_u16(value as u16);
        self.write_data_u16((value >> 16) as u16);
    }

    fn finish_data_if_drained(&mut self) {
        if self.data_pos >= self.data.len() {
            if self.transfer_kind == IdeTransferKind::HddRead {
                self.advance_hdd_sector();
                if self.hdd_bytes_remaining != 0 {
                    self.read_next_hdd_sector();
                    return;
                }
            }
            if self.transfer_kind == IdeTransferKind::AtapiData
                && self.atapi_reply_pos < self.atapi_reply.len()
            {
                self.start_next_atapi_chunk();
                return;
            }
            self.status &= !ATA_STATUS_DRQ;
            self.status |= ATA_STATUS_READY | ATA_STATUS_DSC;
            if self.transfer_kind == IdeTransferKind::AtapiData {
                self.sector_count = 0x03;
                self.atapi_reply.clear();
                self.atapi_reply_pos = 0;
                self.raise_irq();
            }
            self.transfer_kind = IdeTransferKind::None;
        }
    }

    fn current_lba(&self) -> u64 {
        let high = u64::from(self.device_head & 0x0f);
        (high << 24)
            | (u64::from(self.lba_high) << 16)
            | (u64::from(self.lba_mid) << 8)
            | u64::from(self.lba_low)
    }

    fn current_count(&self) -> u32 {
        if self.sector_count == 0 {
            256
        } else {
            u32::from(self.sector_count)
        }
    }
}

#[derive(Debug)]
struct IdeDrive {
    kind: IdeDriveKind,
    sense_key: u8,
    asc: u8,
}

impl IdeDrive {
    fn hdd(disk: Disk) -> Self {
        Self {
            kind: IdeDriveKind::Hdd {
                disk,
                write_cache: true,
                heads: 16,
                track_sectors: 63,
            },
            sense_key: 0,
            asc: 0,
        }
    }

    fn cdrom(iso: CdromImage) -> Self {
        Self {
            kind: IdeDriveKind::Cdrom {
                iso,
                tray_open: false,
                locked: false,
                media_event: 0,
            },
            sense_key: 0,
            asc: 0,
        }
    }

    fn reset_control(&mut self) {
        self.sense_key = 0;
        self.asc = 0;
        if let IdeDriveKind::Cdrom {
            locked,
            media_event,
            ..
        } = &mut self.kind
        {
            // Reset clears command state, but does not move the physical tray.
            *locked = false;
            *media_event = 0;
        }
    }

    fn identify(&self) -> Vec<u8> {
        let mut out = vec![0; 512];
        match &self.kind {
            IdeDriveKind::Hdd {
                disk,
                write_cache,
                heads,
                track_sectors,
            } => {
                let sectors = disk.sectors().min(crate::disk::MAX_SECTORS) as u32;
                let cylinders =
                    (sectors / (u32::from(*heads) * u32::from(*track_sectors))).min(16383) as u16;
                let chs_sectors =
                    u32::from(cylinders) * u32::from(*heads) * u32::from(*track_sectors);
                put_word(&mut out, 0, 0x0040);
                put_word(&mut out, 1, (sectors / (16 * 63)).min(16383) as u16);
                put_word(&mut out, 3, 16);
                put_word(&mut out, 6, 63);
                put_ata_string(&mut out, 10, 10, "R86HDD0000000001");
                put_word(&mut out, 47, 0x8000);
                put_word(&mut out, 49, 1 << 9);
                put_word(&mut out, 53, 1);
                put_word(&mut out, 54, cylinders);
                put_word(&mut out, 55, u16::from(*heads));
                put_word(&mut out, 56, u16::from(*track_sectors));
                put_word(&mut out, 57, chs_sectors as u16);
                put_word(&mut out, 58, (chs_sectors >> 16) as u16);
                put_word(&mut out, 60, sectors as u16);
                put_word(&mut out, 61, (sectors >> 16) as u16);
                put_word(&mut out, 80, 0x003e); // ATA-1 through ATA-5, PIO only
                put_word(&mut out, 82, 1 << 5); // write cache supported
                put_word(&mut out, 83, (1 << 14) | (1 << 12)); // valid, FLUSH CACHE
                put_word(&mut out, 84, 1 << 14);
                put_word(&mut out, 85, if *write_cache { 1 << 5 } else { 0 });
                put_word(&mut out, 86, 1 << 12);
                put_word(&mut out, 87, 1 << 14);
                put_ata_string(&mut out, 23, 4, "0.1");
                put_ata_string(&mut out, 27, 20, "R86 PIO HARDDISK");
            }
            IdeDriveKind::Cdrom { .. } => {
                put_word(&mut out, 0, 0x85c0);
                put_word(&mut out, 20, 3);
                put_word(&mut out, 21, 512);
                put_word(&mut out, 22, 4);
                put_ata_string(&mut out, 10, 10, "R86CD00000000001");
                put_ata_string(&mut out, 23, 4, "0.1");
                put_ata_string(&mut out, 27, 20, "R86 ATAPI CDROM");
                put_word(&mut out, 48, 1);
                put_word(&mut out, 49, 1 << 9);
                put_word(&mut out, 53, 3);
                // No multiword or Ultra DMA; BM status still reports PIO IRQs.
                put_word(&mut out, 63, 0);
                put_word(&mut out, 64, 1);
                put_word(&mut out, 65, 0x00b4);
                put_word(&mut out, 66, 0x00b4);
                put_word(&mut out, 67, 0x012c);
                put_word(&mut out, 68, 0x00b4);
                put_word(&mut out, 71, 30);
                put_word(&mut out, 72, 30);
                put_word(&mut out, 80, 0x001e);
                put_word(&mut out, 82, (1 << 9) | (1 << 4)); // DEVICE RESET and PACKET
                put_word(&mut out, 83, 0x4000);
                put_word(&mut out, 88, 0);
            }
        }
        out
    }

    fn exec_packet(&mut self, cdb: [u8; ATAPI_PACKET_LEN]) -> PacketReply {
        // Inquiry, sense and event/configuration queries remain usable while
        // a media-change condition is pending. REQUEST SENSE acknowledges it.
        if self.sense_key == 6 && !matches!(cdb[0], 0x03 | 0x12 | 0x46 | 0x4a) {
            return PacketReply::CheckCondition {
                sense: 6,
                asc: self.asc,
            };
        }
        if matches!(cdb[0], 0x00 | 0x25 | 0x28 | 0x2b | 0x43 | 0xa8 | 0xbe)
            && self.cdrom_sector_count() == 0
        {
            return PacketReply::CheckCondition {
                sense: 2,
                asc: 0x3a,
            };
        }
        match cdb[0] {
            0x00 => PacketReply::Ok,
            0x03 => {
                let mut out = vec![0; 18];
                out[0] = 0x70;
                out[2] = self.sense_key;
                out[7] = 10;
                out[12] = self.asc;
                self.sense_key = 0;
                self.asc = 0;
                out.truncate(usize::from(cdb[4]));
                PacketReply::Data(AtapiData::Reply(out))
            }
            0x12 => {
                if cdb[1] & 3 != 0 || cdb[2] != 0 {
                    // Standard inquiry only; never return it as a VPD page.
                    return PacketReply::CheckCondition {
                        sense: 5,
                        asc: 0x24,
                    };
                }
                let mut out = vec![0; 36];
                out[0] = 0x05;
                out[1] = 0x80;
                out[2] = 0x05;
                out[3] = 0x02;
                out[4] = 31;
                copy_ascii(&mut out[8..16], "R86");
                copy_ascii(&mut out[16..32], "R86 CD-ROM");
                copy_ascii(&mut out[32..36], "0.1");
                out.truncate(usize::from(cdb[4]));
                PacketReply::Data(AtapiData::Reply(out))
            }
            0x1b => self.start_stop_cdrom(cdb[4]),
            0x1e => {
                if cdb[4] & !1 != 0 {
                    return PacketReply::CheckCondition {
                        sense: 5,
                        asc: 0x24,
                    };
                }
                if let IdeDriveKind::Cdrom { locked, .. } = &mut self.kind {
                    *locked = cdb[4] & 1 != 0;
                }
                PacketReply::Ok
            }
            0x2b => {
                if be_u32(&cdb[2..6]) >= self.cdrom_sector_count() {
                    PacketReply::CheckCondition {
                        sense: 5,
                        asc: 0x21,
                    }
                } else {
                    // Immutable ISO access has no mechanical seek delay.
                    PacketReply::Ok
                }
            }
            0x25 => {
                let sectors = self.cdrom_sector_count();
                let last = sectors - 1;
                let mut out = vec![0; 8];
                put_be_u32(&mut out, 0, last);
                put_be_u32(&mut out, 4, ATAPI_SECTOR_SIZE as u32);
                PacketReply::Data(AtapiData::Reply(out))
            }
            0x28 => {
                let lba = be_u32(&cdb[2..6]);
                let count = u32::from(u16::from_be_bytes([cdb[7], cdb[8]]));
                self.read_cd_sectors(lba, count)
            }
            0xa8 => {
                let lba = be_u32(&cdb[2..6]);
                let count = be_u32(&cdb[6..10]);
                self.read_cd_sectors(lba, count)
            }
            0xbe => {
                let lba = be_u32(&cdb[2..6]);
                let count =
                    (u32::from(cdb[6]) << 16) | (u32::from(cdb[7]) << 8) | u32::from(cdb[8]);
                if count == 0 || (cdb[9] == 0 && cdb[10] == 0) {
                    return PacketReply::Ok;
                }
                // Mounted ISO media contains 2048-byte Mode 1 user data.
                // Raw sectors, audio and subchannels cannot be fabricated
                // from it; reject those requests rather than returning a
                // short successful transfer with the wrong sector format.
                if !matches!((cdb[1] >> 2) & 7, 0 | 2) || cdb[9] != 0x10 || cdb[10] != 0 {
                    return PacketReply::CheckCondition {
                        sense: 5,
                        asc: 0x24,
                    };
                }
                self.read_cd_sectors(lba, count)
            }
            0x43 => self.read_toc(cdb),
            0x46 => {
                if cdb[2] != 0 || cdb[3] != 0 {
                    return PacketReply::CheckCondition {
                        sense: 0x05,
                        asc: 0x24,
                    };
                }
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let current_profile = self.current_cd_profile();
                let mut out = vec![0; 20];
                put_be_u32(&mut out, 0, 16);
                put_be_u16(&mut out, 6, current_profile);
                out[10] = 0x03;
                out[11] = 8;
                put_be_u16(&mut out, 12, 0x0010);
                out[14] = u8::from(current_profile == 0x0010);
                put_be_u16(&mut out, 16, 0x0008);
                out[18] = u8::from(current_profile == 0x0008);
                out.truncate(alloc.min(out.len()));
                PacketReply::Data(AtapiData::Reply(out))
            }
            0x4a => {
                if (cdb[1] & 0x01) == 0 {
                    return PacketReply::CheckCondition {
                        sense: 0x05,
                        asc: 0x24,
                    };
                }
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let class_mask = cdb[4];
                let mut out = if (class_mask & 0x10) != 0 {
                    let mut out = vec![0; 8];
                    put_be_u16(&mut out, 0, 4);
                    out[2] = 0x04;
                    out[3] = 0x10;
                    out[5] = u8::from(self.cdrom_sector_count() != 0) << 1;
                    if let IdeDriveKind::Cdrom {
                        tray_open,
                        media_event,
                        ..
                    } = &mut self.kind
                    {
                        out[4] = *media_event;
                        out[5] |= u8::from(*tray_open);
                        // A header-only size query has not read the event.
                        if alloc >= 8 {
                            *media_event = 0;
                        }
                    }
                    out
                } else {
                    let mut out = vec![0; 4];
                    out[2] = 0x80;
                    out[3] = 0x10;
                    out
                };
                out.truncate(alloc.min(out.len()));
                PacketReply::Data(AtapiData::Reply(out))
            }
            0x1a => self.mode_sense_6(cdb),
            0x5a => self.mode_sense_10(cdb),
            0xac => self.get_performance(cdb),
            _ => PacketReply::CheckCondition {
                sense: 0x05,
                asc: 0x20,
            },
        }
    }

    fn start_stop_cdrom(&mut self, action: u8) -> PacketReply {
        if action & !3 != 0 {
            return PacketReply::CheckCondition {
                sense: 5,
                asc: 0x24,
            };
        }
        if action & 2 == 0 {
            // Start/stop is immediate for immutable media; no spindle scheduler.
            return PacketReply::Ok;
        }
        let IdeDriveKind::Cdrom {
            iso,
            tray_open,
            locked,
            media_event,
        } = &mut self.kind
        else {
            unreachable!()
        };
        let open = action & 1 == 0;
        if open && !*tray_open && *locked {
            return PacketReply::CheckCondition {
                sense: 2,
                asc: 0x53,
            };
        }
        if *tray_open != open {
            *tray_open = open;
            *media_event = if iso.len() < ATAPI_SECTOR_SIZE {
                0
            } else if open {
                3 // removal
            } else {
                2 // new media
            };
            if !open && iso.len() >= ATAPI_SECTOR_SIZE {
                self.sense_key = 6;
                self.asc = 0x28;
            }
        }
        PacketReply::Ok
    }

    fn read_toc(&self, cdb: [u8; ATAPI_PACKET_LEN]) -> PacketReply {
        // One data track/session in an ISO. Support modern and legacy format
        // fields, including a lead-out-only query; reject raw/audio TOC formats.
        let modern = cdb[2] & 15;
        let legacy = cdb[9] >> 6;
        let format = if modern == 0 { legacy } else { modern };
        if (modern != 0 && legacy != 0 && modern != legacy)
            || format > 1
            || (cdb[6] > 1 && !(format == 0 && cdb[6] == 0xaa))
        {
            return PacketReply::CheckCondition {
                sense: 5,
                asc: 0x24,
            };
        }
        let mut out = vec![0, 0, 1, 1];
        for (track, address) in [(1, 0), (0xaa, self.cdrom_sector_count())] {
            if (track == 1 && cdb[6] == 0xaa) || (track == 0xaa && format == 1) {
                continue;
            }
            out.extend_from_slice(&[0, if track == 1 { 0x14 } else { 0x16 }, track, 0]);
            if cdb[1] & 2 != 0 {
                let frames = u64::from(address) + 150;
                let Ok(minutes) = u8::try_from(frames / (60 * 75)) else {
                    return PacketReply::CheckCondition {
                        sense: 5,
                        asc: 0x24,
                    };
                };
                out.extend_from_slice(&[
                    0,
                    minutes,
                    ((frames / 75) % 60) as u8,
                    (frames % 75) as u8,
                ]);
            } else {
                out.extend_from_slice(&address.to_be_bytes());
            }
        }
        let len = (out.len() - 2) as u16;
        put_be_u16(&mut out, 0, len);
        out.truncate(usize::from(u16::from_be_bytes([cdb[7], cdb[8]])));
        PacketReply::Data(AtapiData::Reply(out))
    }

    fn mode_sense_6(&self, cdb: [u8; ATAPI_PACKET_LEN]) -> PacketReply {
        let alloc = usize::from(cdb[4]);
        let mut out = vec![0; 4];
        let pages = match self.mode_sense_pages(cdb[2]) {
            Ok(pages) => pages,
            Err(reply) => return reply,
        };
        out.extend_from_slice(&pages);
        out[0] = out.len().saturating_sub(1).min(u8::MAX as usize) as u8;
        out.truncate(alloc.min(out.len()));
        PacketReply::Data(AtapiData::Reply(out))
    }

    fn mode_sense_10(&self, cdb: [u8; ATAPI_PACKET_LEN]) -> PacketReply {
        let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
        let mut out = vec![0; 8];
        let pages = match self.mode_sense_pages(cdb[2]) {
            Ok(pages) => pages,
            Err(reply) => return reply,
        };
        out.extend_from_slice(&pages);
        let data_len = out.len().saturating_sub(2).min(u16::MAX as usize) as u16;
        put_be_u16(&mut out, 0, data_len);
        out.truncate(alloc.min(out.len()));
        PacketReply::Data(AtapiData::Reply(out))
    }

    fn mode_sense_pages(&self, page_control_and_code: u8) -> Result<Vec<u8>, PacketReply> {
        let page_control = page_control_and_code >> 6;
        if page_control == 3 {
            return Err(PacketReply::CheckCondition {
                sense: 0x05,
                asc: 0x39,
            });
        }
        if page_control != 0 {
            return Err(PacketReply::CheckCondition {
                sense: 0x05,
                asc: 0x24,
            });
        }

        let page = page_control_and_code & 0x3f;
        let mut pages = Vec::new();
        if page == 0x01 || page == 0x3f {
            pages.extend_from_slice(&[0x01, 0x06, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00]);
        }
        if page == 0x2a || page == 0x3f {
            let mut capabilities = [0u8; 20];
            capabilities[0] = 0x2a;
            capabilities[1] = 18;
            capabilities[2] = 0x0b; // data from CD-R/CD-RW/DVD-ROM images
            capabilities[6] = 0x29; // tray loading, eject, locking
            if matches!(self.kind, IdeDriveKind::Cdrom { locked: true, .. }) {
                capabilities[6] |= 2;
            }
            // No audio, Mode 2, raw/subchannel, writing or changer features.
            put_be_u16(&mut capabilities, 8, 706);
            put_be_u16(&mut capabilities, 12, 64); // at most 64 KiB per PIO phase
            put_be_u16(&mut capabilities, 14, 706);
            pages.extend_from_slice(&capabilities);
        }
        if pages.is_empty() {
            Err(PacketReply::CheckCondition {
                sense: 0x05,
                asc: 0x24,
            })
        } else {
            Ok(pages)
        }
    }

    fn get_performance(&self, cdb: [u8; ATAPI_PACKET_LEN]) -> PacketReply {
        let descriptors = u16::from_be_bytes([cdb[8], cdb[9]]) as usize;
        let alloc = 8usize.saturating_add(descriptors.saturating_mul(16));
        let mut out = vec![0; 8];
        put_be_u32(&mut out, 0, 4);
        out.truncate(alloc.min(out.len()));
        PacketReply::Data(AtapiData::Reply(out))
    }

    fn cdrom_sector_count(&self) -> u32 {
        match &self.kind {
            IdeDriveKind::Cdrom {
                iso,
                tray_open: false,
                ..
            } => (iso.len() / ATAPI_SECTOR_SIZE).min(u32::MAX as usize) as u32,
            _ => 0,
        }
    }

    fn current_cd_profile(&self) -> u16 {
        if self.cdrom_sector_count() == 0 {
            return 0;
        }
        match &self.kind {
            IdeDriveKind::Cdrom { iso, .. } => {
                const CD_MAX_BYTES: usize = 80 * 60 * 75 * ATAPI_SECTOR_SIZE;
                if iso.len() > CD_MAX_BYTES {
                    0x0010
                } else {
                    0x0008
                }
            }
            _ => 0,
        }
    }

    fn read_cd_sectors(&self, lba: u32, count: u32) -> PacketReply {
        if count == 0 {
            return PacketReply::Ok;
        }
        let sectors = self.cdrom_sector_count();
        if u64::from(lba) + u64::from(count) > u64::from(sectors) {
            return PacketReply::CheckCondition {
                sense: 5,
                asc: 0x21,
            };
        }
        // The validated extent fits in the immutable ISO, including on 32-bit
        // hosts. Keep only that extent; each PIO phase copies at most its byte
        // count. No arbitrary sector cap or allocation proportional to the
        // complete request. Consulted tiny386's ATAPI PIO/reply handling.
        let start = lba as usize * ATAPI_SECTOR_SIZE;
        let end = start + count as usize * ATAPI_SECTOR_SIZE;
        PacketReply::Data(AtapiData::Cdrom(start..end))
    }
}

#[derive(Debug)]
enum IdeDriveKind {
    Hdd {
        disk: Disk,
        write_cache: bool,
        heads: u8,
        track_sectors: u8,
    },
    Cdrom {
        iso: CdromImage,
        // The image remains mounted when the guest opens the tray. Closing it
        // reloads the same read-only media without host I/O or a second owner.
        tray_open: bool,
        locked: bool,
        media_event: u8,
    },
}

// Ordinary hosts keep the original Vec representation. Only the monitor can
// borrow immutable media from a fixed memslot instead of allocating a copy.
#[cfg(not(all(feature = "monitor", not(feature = "host"))))]
type CdromBacking = Vec<u8>;
#[cfg(all(feature = "monitor", not(feature = "host")))]
type CdromBacking = alloc::borrow::Cow<'static, [u8]>;

#[derive(Clone)]
enum CdromImage {
    Memory(CdromBacking),
    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    Browser {
        drive: u32,
        bytes: usize,
    },
    #[cfg(feature = "host")]
    File {
        file: std::sync::Arc<std::fs::File>,
        bytes: usize,
    },
}
impl CdromImage {
    #[cfg(feature = "host")]
    fn file(file: std::fs::File) -> std::io::Result<Self> {
        let bytes = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("ISO image is too large for this host"))?;
        Ok(Self::File {
            file: file.into(),
            bytes,
        })
    }

    fn len(&self) -> usize {
        match self {
            Self::Memory(bytes) => bytes.len(),
            #[cfg(feature = "host")]
            Self::File { bytes, .. } => *bytes,
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Self::Browser { bytes, .. } => *bytes,
        }
    }

    // ATAPI byte-count phases need not be sector-aligned. tiny386 uses its
    // block backend for optical reads too; browser media must not require a
    // second, whole-ISO allocation in Wasm memory.
    fn read_into(&self, offset: usize, len: usize, out: &mut Vec<u8>) -> Result<(), DiskError> {
        match self {
            Self::Memory(bytes) => out.extend_from_slice(&bytes[offset..offset + len]),
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Self::Browser { drive, .. } => {
                out.resize(len, 0);
                crate::browser::read(*drive, offset as u64, out)?;
            }
            #[cfg(feature = "host")]
            Self::File { file, .. } => {
                use std::io::{Read, Seek, SeekFrom};
                let start = out.len();
                out.resize(start + len, 0);
                let mut file = &**file;
                file.seek(SeekFrom::Start(offset as u64))?;
                file.read_exact(&mut out[start..])?;
            }
        }
        Ok(())
    }
}
impl core::fmt::Debug for CdromImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Diagnostic state includes every live controller field. Immutable ISO
        // inputs have separate SHA-256 identities; do not print hundreds of MiB
        // into the bounded state-export buffer. This hash is diagnostic only.
        let bytes = match self {
            Self::Memory(bytes) => bytes,
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Self::Browser { drive, bytes } => {
                return f
                    .debug_struct("BrowserCdrom")
                    .field("drive", drive)
                    .field("len", bytes)
                    .finish();
            }
            #[cfg(feature = "host")]
            Self::File { bytes, .. } => {
                return f.debug_struct("FileCdrom").field("len", bytes).finish();
            }
        };
        let digest = bytes.iter().fold(0xcbf29ce484222325_u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        f.debug_struct("CdromImage")
            .field("len", &bytes.len())
            .field("fnv1a", &digest)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdeTransferKind {
    None,
    Identify,
    HddRead,
    HddWrite,
    AtapiData,
}

#[derive(Clone, Debug)]
enum AtapiData {
    Reply(Vec<u8>),
    Cdrom(core::ops::Range<usize>),
}

impl AtapiData {
    fn len(&self) -> usize {
        match self {
            Self::Reply(bytes) => bytes.len(),
            Self::Cdrom(range) => range.end - range.start,
        }
    }

    fn clear(&mut self) {
        *self = Self::Reply(Vec::new());
    }
}

#[derive(Clone, Debug)]
enum PacketReply {
    Ok,
    Data(AtapiData),
    CheckCondition { sense: u8, asc: u8 },
}

fn put_word(out: &mut [u8], word: usize, value: u16) {
    let offset = word * 2;
    if offset + 2 <= out.len() {
        out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
}

fn put_ata_string(out: &mut [u8], word: usize, words: usize, value: &str) {
    let offset = word * 2;
    let len = words * 2;
    if offset + len > out.len() {
        return;
    }
    for byte in &mut out[offset..offset + len] {
        *byte = b' ';
    }
    let bytes = value.as_bytes();
    for index in 0..bytes.len().min(len) {
        let dst = offset + (index ^ 1);
        out[dst] = bytes[index];
    }
}

fn copy_ascii(out: &mut [u8], value: &str) {
    for byte in out.iter_mut() {
        *byte = b' ';
    }
    for (dst, src) in out.iter_mut().zip(value.as_bytes().iter().copied()) {
        *dst = src;
    }
}

fn put_be_u32(out: &mut [u8], offset: usize, value: u32) {
    if offset + 4 <= out.len() {
        out[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
}

fn put_be_u16(out: &mut [u8], offset: usize, value: u16) {
    if offset + 2 <= out.len() {
        out[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ATA task-file registers are byte ports even when they describe one
    // combined value (e.g. the ATAPI byte count at cylinder low/high).
    fn read_task_pair(ide: &mut IdeBus, port: u16) -> u16 {
        u16::from_le_bytes([ide.read_u8(port).unwrap(), ide.read_u8(port + 1).unwrap()])
    }

    #[test]
    fn unsupported_ide_widths_do_not_consume_status_or_issue_commands() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_primary_master_hdd(vec![0; 512], false);
        ide.write_u8(0x1f7, ATA_CMD_IDENTIFY);
        assert!(ide.irq_asserted(IdeIrq::Primary));
        assert_eq!(ide.read_u16(0x1f7), None);
        assert!(ide.irq_asserted(IdeIrq::Primary));
        assert_eq!(ide.read_u32(0x1f6), None);
        assert!(ide.irq_asserted(IdeIrq::Primary));
        assert!(!ide.write_u16(0x1f7, 0x00ff)); // An invalid command must not run.
        assert!(!ide.write_u32(0x3f6, 4)); // Nor a partial SRST.
        assert_eq!(
            ide.read_u8(0x3f6).unwrap() & (ATA_STATUS_DRQ | ATA_STATUS_BUSY | ATA_STATUS_ERR),
            ATA_STATUS_DRQ
        );
        assert_eq!(ide.read_u16(0x1f0), Some(0x0040)); // First IDENTIFY word survives.
        ide.read_u8(0x1f7);
        assert!(!ide.irq_asserted(IdeIrq::Primary));
    }

    #[test]
    fn pci_exposes_minimal_qemu_like_devices() {
        let mut pci = PciConfig::new();
        pci.write_addr(0x8000_0000);
        assert_eq!(pci.read_data_u32(), 0x1237_8086);
        pci.write_addr(0x8000_002c);
        assert_eq!(pci.read_data_u32(), 0x1100_1af4);
        pci.write_addr(0x8000_000c);
        assert_eq!((pci.read_data_u32() >> 16) & 0xff, 0x00);
        pci.write_addr(0x8000_080c);
        assert_eq!((pci.read_data_u32() >> 16) & 0xff, 0x80);
        pci.write_addr(0x8000_0900);
        assert_eq!(pci.read_data_u32(), 0x7010_8086);
        pci.write_addr(0x8000_0904);
        assert_eq!(pci.read_data_u32() & 0xffff, 0x0005);
        pci.write_addr(0x8000_090c);
        assert_eq!((pci.read_data_u32() >> 16) & 0xff, 0x00);
        pci.write_addr(0x8000_0908);
        assert_eq!((pci.read_data_u32() >> 8) & 0xff, 0x80);
        pci.write_addr(0x8000_0920);
        assert_eq!(pci.read_data_u32(), 0x0000_c001);
        assert_eq!(pci.ide_bus_master_base(), Some(0xc000));
        pci.write_data_u32(0xffff_ffff);
        assert_eq!(pci.read_data_u32(), 0xffff_fff1);
        pci.write_data_u32(0x0000_d001);
        assert_eq!(pci.ide_bus_master_base(), Some(0xd000));
        pci.write_addr(0x8000_093c);
        assert_eq!(pci.read_data_u32() & 0xffff, 0x000e);
        pci.write_addr(0x8000_1000);
        assert_eq!(pci.read_data_u32(), 0x1111_1234);
        pci.write_addr(0x8000_1010);
        assert_eq!(pci.read_data_u32(), 0xe000_0008);
        pci.write_data_u32(0xffff_ffff);
        assert_eq!(pci.read_data_u32(), 0xff00_0008);
        pci.write_data_u32(0xe000_0000);
        pci.write_addr(0x8000_103c);
        assert_eq!(pci.read_data_u32() & 0xffff, 0x0000);
        pci.write_addr(0x8000_1004);
        pci.write_data_u32(0x0000_0003);
        assert!(pci.vga_io_enabled());
        assert!(pci.vga_mem_enabled());
        pci.write_addr(0x8000_1800);
        assert_eq!(pci.read_data_u32(), 0xffff_ffff);
    }

    #[test]
    fn fw_cfg_exposes_qemu_signature_and_e820_file() {
        let mut fw = FwCfgDevice::new_qemu(128 * 1024 * 1024);
        fw.select(FW_CFG_SIGNATURE);
        assert_eq!(
            [
                fw.read_data(),
                fw.read_data(),
                fw.read_data(),
                fw.read_data()
            ],
            *b"QEMU"
        );

        fw.select(FW_CFG_FILE_DIR);
        let count = u32::from_be_bytes([
            fw.read_data(),
            fw.read_data(),
            fw.read_data(),
            fw.read_data(),
        ]);
        assert!(count >= 1);

        let e820 = fw.file_select("etc/e820").unwrap();
        fw.select(e820);
        let mut entry = [0u8; 20];
        for byte in &mut entry {
            *byte = fw.read_data();
        }
        assert_eq!(u64::from_le_bytes(entry[0..8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            0x000a_0000
        );
        assert_eq!(u32::from_le_bytes(entry[16..20].try_into().unwrap()), 1);
    }

    #[test]
    fn fw_cfg_vga_display_omits_serial_console_files() {
        let serial = FwCfgDevice::new_qemu_with_display(128 * 1024 * 1024, FirmwareDisplay::Serial);
        assert!(serial.file_select("etc/sercon-port").is_some());
        assert!(serial.file_select("etc/screen-and-debug").is_some());

        let vga = FwCfgDevice::new_qemu_with_display(128 * 1024 * 1024, FirmwareDisplay::Vga);
        assert!(vga.file_select("etc/sercon-port").is_none());
        assert!(vga.file_select("etc/screen-and-debug").is_none());
        assert!(vga.file_select("etc/e820").is_some());
    }

    #[test]
    fn absent_selected_ide_drive_reads_zero_and_ignores_commands() {
        let mut ide = IdeBus::new_enabled();

        assert_eq!(ide.read_u8(0x1f7), Some(0));
        assert_eq!(ide.read_u8(0x3f6), Some(0));
        assert_eq!(ide.read_u16(0x1f0), Some(0));

        ide.write_u8(0x1f6, 0xa0);
        ide.write_u8(0x1f7, ATA_CMD_IDENTIFY);
        assert_eq!(ide.read_u8(0x1f7), Some(0));
        assert!(!ide.irq_asserted(IdeIrq::Primary));

        ide.write_u8(0x1f6, 0xb0);
        ide.write_u8(0x1f7, ATA_CMD_IDENTIFY_PACKET);
        assert_eq!(ide.read_u8(0x1f1), Some(0));
        assert_eq!(ide.read_u8(0x1f2), Some(0));
        assert_eq!(ide.read_u8(0x1f3), Some(0));
        assert_eq!(ide.read_u8(0x1f4), Some(0));
        assert_eq!(ide.read_u8(0x1f5), Some(0));
        assert_eq!(ide.read_u8(0x1f6), Some(0));
        assert_eq!(ide.read_u8(0x1f7), Some(0));
        assert_eq!(ide.read_u8(0x3f6), Some(0));
        assert!(!ide.irq_asserted(IdeIrq::Primary));
    }

    #[test]
    fn ide_srst_holds_busy_and_clears_pending_interrupts() {
        for (status, control, command) in [
            (0x1f7, 0x3f6, ATA_CMD_IDENTIFY),
            (0x177, 0x376, ATA_CMD_IDENTIFY_PACKET),
        ] {
            let mut ide = IdeBus::new_enabled();
            ide.attach_primary_master_hdd(vec![0; 512], true);
            ide.attach_secondary_master_cdrom(vec![0; 2048]);
            let irq = if status == 0x1f7 {
                IdeIrq::Primary
            } else {
                IdeIrq::Secondary
            };
            ide.write_u8(status, command);
            assert!(ide.irq_asserted(irq));

            ide.write_u8(control, 6); // SRST with nIEN set
            assert_eq!(
                ide.read_u8(control).unwrap() & (ATA_STATUS_BUSY | ATA_STATUS_DRQ),
                ATA_STATUS_BUSY
            );
            ide.write_u8(status, command); // commands during reset must be ignored
            ide.advance(PIO_WRITE_STEPS * 2);
            assert_eq!(
                ide.read_u8(control).unwrap() & (ATA_STATUS_BUSY | ATA_STATUS_DRQ),
                ATA_STATUS_BUSY
            );
            assert_eq!(ide.read_u16(status - 7), Some(0));
            assert!(!ide.irq_asserted(irq));

            ide.write_u8(control, 0);
            assert_eq!(
                ide.read_u8(control),
                Some(ATA_STATUS_READY | ATA_STATUS_DSC)
            );
            assert!(!ide.irq_asserted(irq), "reset must discard the old request");
            assert_eq!(ide.read_u8(status - 5), Some(1)); // sector count signature
            ide.write_u8(status, command);
            assert!(ide.irq_asserted(irq));
            assert_ne!(ide.read_u8(status).unwrap() & ATA_STATUS_DRQ, 0);
        }
    }

    #[test]
    fn selecting_absent_slave_does_not_destroy_cdrom_master_signature() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE * 4]);

        ide.write_u8(0x176, 0xa0);
        assert_eq!(ide.read_u8(0x177), Some(ATA_STATUS_READY | ATA_STATUS_DSC));
        assert_eq!(ide.read_u8(0x174), Some(0x14));
        assert_eq!(ide.read_u8(0x175), Some(0xeb));

        ide.write_u8(0x176, 0xb0);
        assert_eq!(ide.read_u8(0x177), Some(0));
        assert_eq!(ide.read_u8(0x174), Some(0));
        assert_eq!(ide.read_u8(0x175), Some(0));
        ide.write_u8(0x177, ATA_CMD_IDENTIFY_PACKET);
        assert_eq!(ide.read_u8(0x177), Some(0));
        assert!(!ide.irq_asserted(IdeIrq::Secondary));

        ide.write_u8(0x176, 0xa0);
        assert_eq!(ide.read_u8(0x177), Some(ATA_STATUS_READY | ATA_STATUS_DSC));
        assert_eq!(ide.read_u8(0x174), Some(0x14));
        assert_eq!(ide.read_u8(0x175), Some(0xeb));
    }

    #[test]
    fn atapi_device_reset_reports_success_status_and_signature() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE * 4]);
        ide.write_u8(0x176, 0xa0);
        ide.write_u8(0x177, ATA_CMD_DEVICE_RESET);

        assert!(!ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x172), Some(1));
        assert_eq!(ide.read_u8(0x173), Some(1));
        assert_eq!(ide.read_u8(0x174), Some(0x14));
        assert_eq!(ide.read_u8(0x175), Some(0xeb));
        assert_eq!(ide.read_u8(0x177), Some(0));

        ide.write_u8(0x177, ATA_CMD_IDENTIFY_PACKET);
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
    }

    #[test]
    fn atapi_identify_and_read_capacity_work() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE * 4]);
        ide.write_u8(0x176, 0xa0);
        ide.write_u8(0x177, ATA_CMD_IDENTIFY_PACKET);
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
        assert_eq!(ide.read_u16(0x170).unwrap(), 0x85c0);
        for _ in 1..63 {
            let _ = ide.read_u16(0x170).unwrap();
        }
        assert_eq!(ide.read_u16(0x170).unwrap(), 0);
        for _ in 64..256 {
            let _ = ide.read_u16(0x170).unwrap();
        }

        ide.write_u8(0x177, ATA_CMD_PACKET);
        assert!(!ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
        assert!(!ide.irq_asserted(IdeIrq::Secondary));
        let packet = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for chunk in packet.chunks(2) {
            ide.write_u16(0x170, u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        assert!(ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
        assert!(!ide.irq_asserted(IdeIrq::Secondary));
        let mut data = [0u8; 8];
        for chunk in data.chunks_mut(2) {
            chunk.copy_from_slice(&ide.read_u16(0x170).unwrap().to_le_bytes());
        }
        assert!(ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x172).unwrap() & 0x03, 0x03);
        assert_eq!(u32::from_be_bytes(data[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 2048);
    }

    #[test]
    fn controller_reset_preserves_written_disk_and_discards_pending_transfer() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_primary_master_hdd(vec![0; 1024], true);
        ide.write_u8(0x1f6, 0xe0);
        ide.write_u8(0x1f2, 1);
        ide.write_u8(0x1f3, 0);
        ide.write_u8(0x1f7, ATA_CMD_WRITE_SECTORS);
        for _ in 0..256 {
            ide.write_u16(0x1f0, 0x5a3c);
        }
        assert_eq!(ide.read_u8(0x3f6), Some(ATA_STATUS_BUSY));
        assert!(!ide.irq_asserted(IdeIrq::Primary));
        ide.reset();
        ide.advance(PIO_WRITE_STEPS * 2);
        assert!(!ide.irq_asserted(IdeIrq::Primary));
        assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_DRQ, 0);
        ide.write_u8(0x1f6, 0xe0);
        ide.write_u8(0x1f2, 1);
        ide.write_u8(0x1f3, 0);
        ide.write_u8(0x1f7, ATA_CMD_READ_SECTORS);
        for _ in 0..256 {
            assert_eq!(ide.read_u16(0x1f0), Some(0x5a3c));
        }
    }

    #[test]
    fn ide_feature_errors_do_not_claim_unimplemented_operations() {
        for packet in [false, true] {
            let mut ide = IdeBus::new_enabled();
            if packet {
                ide.attach_primary_master_cdrom(vec![0; ATAPI_SECTOR_SIZE]);
            } else {
                ide.attach_primary_master_hdd(vec![0; 1024], true);
            }
            // APM, SATA features and read look-ahead have no advertised implementation.
            for feature in [0x05, 0x10, 0xaa, 0xff] {
                ide.write_u8(0x1f1, feature);
                ide.write_u8(0x1f7, ATA_CMD_SET_FEATURES);
                assert!(ide.irq_asserted(IdeIrq::Primary));
                assert_eq!(
                    ide.read_u8(0x1f7).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
                    ATA_STATUS_ERR
                );
                assert_eq!(ide.read_u8(0x1f1), Some(ATA_ERR_ABORT));
            }
            // The error must not poison subsequent supported commands.
            ide.write_u8(0x1f1, 3);
            ide.write_u8(0x1f2, 0x08);
            ide.write_u8(0x1f7, ATA_CMD_SET_FEATURES);
            assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
            ide.write_u8(0x1f4, 0xa5);
            ide.write_u8(0x1f5, 0x5a);
            ide.write_u8(0x1f7, ATA_CMD_EXEC_DIAG);
            assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
            assert_eq!(ide.read_u8(0x1f1), Some(1)); // diagnostic passed, not ATA error zero
            assert_eq!(
                read_task_pair(&mut ide, 0x1f4),
                if packet { 0xeb14 } else { 0 }
            );
        }
    }

    #[test]
    fn atapi_absent_media_and_unsupported_queries_report_sense() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(Vec::new());
        for opcode in [0x00, 0x25, 0x2b, 0x43] {
            let mut packet = [0; ATAPI_PACKET_LEN];
            packet[0] = opcode;
            atapi_failure(&mut ide, packet, 2, 0x3a);
        }
        let config = atapi_response(&mut ide, [0x46, 0, 0, 0, 0, 0, 0, 0, 20, 0, 0, 0]);
        assert_eq!(&config[6..8], &[0, 0]);
        let event = atapi_response(&mut ide, [0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8, 0, 0, 0]);
        assert_eq!(event[5] & 3, 0); // empty, closed tray
        assert_eq!(
            atapi_response(&mut ide, [0x12, 0, 0, 0, 36, 0, 0, 0, 0, 0, 0, 0])[0],
            5
        );
        atapi_failure(
            &mut ide,
            [0x12, 1, 0x80, 0, 36, 0, 0, 0, 0, 0, 0, 0],
            5,
            0x24,
        );
    }

    #[test]
    fn atapi_lock_eject_reload_and_reset_have_observable_media_state() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0x5a; ATAPI_SECTOR_SIZE * 4]);
        let event = [0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8, 0, 0, 0];
        let mode = [0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 28, 0, 0, 0];
        let lock = [0x1e, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let eject = [0x1b, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
        let load = [0x1b, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(atapi_response(&mut ide, event)[5] & 3, 2);
        atapi_response(&mut ide, lock);
        let caps = atapi_response(&mut ide, mode);
        assert_ne!(caps[14] & 2, 0); // lock state
        assert_eq!(caps[12] & 0x3f, 0); // no audio or Mode 2 emulation
        assert_eq!(caps[13], 0); // no raw audio/subchannel capabilities
        atapi_failure(&mut ide, eject, 2, 0x53);
        atapi_response(&mut ide, [0; ATAPI_PACKET_LEN]); // locked media still readable
        let mut allow = lock;
        allow[4] = 0;
        atapi_response(&mut ide, allow);
        atapi_response(&mut ide, eject);
        atapi_failure(&mut ide, [0; ATAPI_PACKET_LEN], 2, 0x3a);
        let mut header_only = event;
        header_only[8] = 4;
        atapi_response(&mut ide, header_only);
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[3, 1]);
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[0, 1]);
        ide.reset();
        atapi_failure(&mut ide, [0; ATAPI_PACKET_LEN], 2, 0x3a); // reset does not close tray
        atapi_response(&mut ide, load);
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[2, 2]);
        atapi_failure(&mut ide, [0; ATAPI_PACKET_LEN], 6, 0x28); // request sense acknowledges UA
        atapi_response(&mut ide, [0; ATAPI_PACKET_LEN]);
        let data = atapi_response(&mut ide, [0x28, 0, 0, 0, 0, 3, 0, 0, 1, 0, 0, 0]);
        assert_eq!(data, vec![0x5a; ATAPI_SECTOR_SIZE]);
        atapi_response(&mut ide, lock);
        ide.reset();
        assert_eq!(atapi_response(&mut ide, mode)[14] & 2, 0);
        atapi_response(&mut ide, eject); // reset cleared the lock
    }

    #[test]
    fn atapi_host_disc_changes_notify_the_guest_and_survive_reset() {
        let mut ide = IdeBus::new_enabled();
        assert!(ide.cdrom_state().is_none());
        assert!(ide.change_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE]).is_err());
        ide.attach_primary_master_hdd(vec![0x7b; 512], false);
        ide.attach_secondary_master_cdrom(Vec::new());
        ide.write_u8(0x1f2, 37);
        let event = [0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8, 0, 0, 0];
        let read = [0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
        ide.change_cdrom(vec![0xa5; ATAPI_SECTOR_SIZE * 4]).unwrap();
        assert_eq!(ide.read_u8(0x1f2), Some(37)); // no HDD/controller reset
        assert!(!ide.irq_asserted(IdeIrq::Secondary)); // not a completed command
        atapi_failure(&mut ide, [0; ATAPI_PACKET_LEN], 6, 0x28);
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[2, 2]);
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[0, 2]);
        assert_eq!(
            atapi_response(&mut ide, read),
            vec![0xa5; ATAPI_SECTOR_SIZE]
        );

        ide.change_cdrom(vec![0x5a; ATAPI_SECTOR_SIZE * 2]).unwrap();
        atapi_failure(&mut ide, read, 6, 0x28);
        let capacity = atapi_response(&mut ide, [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(u32::from_be_bytes(capacity[..4].try_into().unwrap()), 1);
        ide.reset();
        assert_eq!(
            atapi_response(&mut ide, read),
            vec![0x5a; ATAPI_SECTOR_SIZE]
        );

        ide.change_cdrom(Vec::new()).unwrap();
        assert_eq!(&atapi_response(&mut ide, event)[4..6], &[3, 1]);
        atapi_failure(&mut ide, read, 6, 0x28);
        atapi_failure(&mut ide, read, 2, 0x3a);
        ide.reset();
        atapi_response(&mut ide, [0x1b, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]);
        atapi_failure(&mut ide, read, 2, 0x3a); // ejected image cannot be reloaded

        let mut primary = IdeBus::new_enabled();
        primary.attach_primary_master_cdrom(Vec::new());
        primary.change_cdrom(vec![0; ATAPI_SECTOR_SIZE]).unwrap();
        assert!(primary.cdrom_state().unwrap().inserted);
        primary.change_cdrom(Vec::new()).unwrap();
        assert!(!primary.cdrom_state().unwrap().inserted);
    }

    #[test]
    fn atapi_host_change_preserves_locked_media_and_inflight_reads() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE * 4]);
        let mut lock = [0x1e, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        atapi_response(&mut ide, lock);
        assert!(ide.cdrom_state().unwrap().locked);
        assert!(ide.change_cdrom(Vec::new()).is_err());
        assert!(ide.change_cdrom(vec![0xbb; ATAPI_SECTOR_SIZE]).is_err());
        let read = [0x28, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(
            atapi_response(&mut ide, read),
            vec![0xaa; ATAPI_SECTOR_SIZE * 2]
        );
        lock[4] = 0;
        atapi_response(&mut ide, lock);
        start_atapi_command(&mut ide, read, 2048);
        assert!(ide.change_cdrom(vec![0xbb; ATAPI_SECTOR_SIZE]).is_err());
        // Both phases must still read the original image, without losing IRQs
        // or touching out-of-bounds data in the proposed, shorter replacement.
        for _ in 0..2 {
            assert!(ide.irq_asserted(IdeIrq::Secondary));
            assert_ne!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, 0);
            for _ in 0..1024 {
                assert_eq!(ide.read_u16(0x170), Some(0xaaaa));
            }
        }
        assert_eq!(
            ide.read_u8(0x177).unwrap() & (ATA_STATUS_DRQ | ATA_STATUS_ERR),
            0
        );
        ide.change_cdrom(vec![0xbb; ATAPI_SECTOR_SIZE]).unwrap();
        atapi_failure(&mut ide, [0; ATAPI_PACKET_LEN], 6, 0x28);
        let mut one = read;
        one[8] = 1;
        assert_eq!(atapi_response(&mut ide, one), vec![0xbb; ATAPI_SECTOR_SIZE]);
    }

    #[test]
    fn atapi_toc_describes_the_same_extent_as_read_capacity() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0; ATAPI_SECTOR_SIZE * 5000]);
        let capacity = atapi_response(&mut ide, [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let blocks = be_u32(&capacity[..4]) + 1;
        for msf in [false, true] {
            let mut toc = [0x43, u8::from(msf) << 1, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0];
            let all = atapi_response(&mut ide, toc);
            assert_eq!(all.len(), 20);
            assert_eq!(&all[..4], &[0, 18, 1, 1]);
            assert_eq!(all[6], 1);
            assert_eq!(all[14], 0xaa);
            let decode = |bytes: &[u8]| {
                if msf {
                    u32::from(bytes[1]) * 60 * 75 + u32::from(bytes[2]) * 75 + u32::from(bytes[3])
                        - 150
                } else {
                    be_u32(bytes)
                }
            };
            assert_eq!(decode(&all[8..12]), 0);
            assert_eq!(decode(&all[16..20]), blocks);
            toc[6] = 0xaa;
            let leadout = atapi_response(&mut ide, toc);
            assert_eq!(leadout.len(), 12);
            assert_eq!(&leadout[4..], &all[12..]);
        }
        let session = atapi_response(&mut ide, [0x43, 0, 1, 0, 0, 0, 1, 0, 64, 0, 0, 0]);
        assert_eq!(session.len(), 12);
        assert_eq!(&session[..4], &[0, 10, 1, 1]);
        assert_eq!(session[6], 1);
        atapi_failure(&mut ide, [0x43, 0, 0, 0, 0, 0, 2, 0, 64, 0, 0, 0], 5, 0x24);
        atapi_failure(&mut ide, [0x43, 0, 2, 0, 0, 0, 0, 0, 64, 0, 0, 0], 5, 0x24);
        atapi_failure(
            &mut ide,
            [0x2b, 0, 0, 0, 0x13, 0x88, 0, 0, 0, 0, 0, 0],
            5,
            0x21,
        );
    }

    fn atapi_response(ide: &mut IdeBus, packet: [u8; ATAPI_PACKET_LEN]) -> Vec<u8> {
        start_atapi_command(ide, packet, 2048);
        let mut data = Vec::new();
        loop {
            assert!(ide.irq_asserted(IdeIrq::Secondary));
            let status = ide.read_u8(0x177).unwrap();
            assert_eq!(
                status & (ATA_STATUS_BUSY | ATA_STATUS_ERR),
                0,
                "CDB {packet:x?}"
            );
            if status & ATA_STATUS_DRQ == 0 {
                assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
                return data;
            }
            assert_eq!(ide.read_u8(0x172).unwrap() & 3, 2);
            let len = read_task_pair(ide, 0x174) as usize;
            assert!(len > 0 && data.len() + len <= 4096);
            for pos in (0..len).step_by(2) {
                let word = ide.read_u16(0x170).unwrap().to_le_bytes();
                data.extend_from_slice(&word[..(len - pos).min(2)]);
            }
        }
    }

    fn atapi_failure(ide: &mut IdeBus, packet: [u8; ATAPI_PACKET_LEN], sense: u8, asc: u8) {
        start_atapi_command(ide, packet, 2048);
        assert!(ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(
            ide.read_u8(0x177).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
            ATA_STATUS_ERR,
            "CDB {packet:x?}"
        );
        assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
        let data = atapi_response(ide, [3, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!((data[2], data[12]), (sense, asc));
    }

    #[test]
    fn atapi_advertises_pio_and_rejects_dma_requests() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_primary_master_cdrom(vec![0; ATAPI_SECTOR_SIZE]);
        ide.write_u8(0x1f7, ATA_CMD_IDENTIFY_PACKET);
        let words: Vec<_> = (0..256).map(|_| ide.read_u16(0x1f0).unwrap()).collect();
        assert_eq!(words[49] & (1 << 8), 0);
        assert_eq!(words[53] & 4, 0);
        assert_eq!(words[62], 0);
        assert_eq!(words[63], 0);
        assert_eq!(words[88], 0);
        assert_eq!(words[64], 1);
        for mode in [0x10, 0x20, 0x21, 0x40, 0x45] {
            ide.write_u8(0x1f1, 0x03);
            ide.write_u8(0x1f2, mode);
            ide.write_u8(0x1f7, ATA_CMD_SET_FEATURES);
            assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
            assert_eq!(ide.read_u8(0x1f1), Some(ATA_ERR_ABORT));
        }
        ide.write_u8(0x1f2, 0x0b);
        ide.write_u8(0x1f7, ATA_CMD_SET_FEATURES);
        assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
        ide.write_u8(0x1f1, 1);
        ide.write_u8(0x1f7, ATA_CMD_PACKET);
        assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
        ide.write_u8(0x1f1, 0);
        ide.write_u8(0x1f7, ATA_CMD_PACKET);
        assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
    }

    #[test]
    fn atapi_get_configuration_and_event_status_work() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xaa; ATAPI_SECTOR_SIZE * 4]);
        ide.write_u8(0x176, 0xa0);

        let config = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x46, 0, 0, 0, 0, 0, 0, 0, 20, 0, 0, 0],
            20,
        );
        assert_eq!(u32::from_be_bytes(config[0..4].try_into().unwrap()), 16);
        assert_eq!(u16::from_be_bytes(config[6..8].try_into().unwrap()), 0x0008);
        assert_eq!(config[10], 0x03);
        assert_eq!(config[11], 8);
        assert_eq!(
            u16::from_be_bytes(config[12..14].try_into().unwrap()),
            0x0010
        );
        assert_eq!(
            u16::from_be_bytes(config[16..18].try_into().unwrap()),
            0x0008
        );
        assert_eq!(config[18], 1);

        let short_config = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x46, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0],
            8,
        );
        assert_eq!(short_config.len(), 8);
        assert_eq!(
            u16::from_be_bytes(short_config[6..8].try_into().unwrap()),
            0x0008
        );

        let no_event = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x4a, 1, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0],
            4,
        );
        assert_eq!(no_event, [0, 0, 0x80, 0x10]);

        let media_event = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8, 0, 0, 0],
            8,
        );
        assert_eq!(u16::from_be_bytes(media_event[0..2].try_into().unwrap()), 4);
        assert_eq!(media_event[2], 0x04);
        assert_eq!(media_event[3], 0x10);
        assert_eq!(media_event[4], 0);
        assert_eq!(media_event[5], 2); // media present, tray closed

        let mode6 = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x1a, 0x08, 0x2a, 0, 36, 0, 0, 0, 0, 0, 0, 0],
            24,
        );
        assert_eq!(mode6[0], 23);
        assert_eq!(mode6[3], 0);
        assert_eq!(mode6[4], 0x2a);
        assert_eq!(mode6[5], 0x12);

        let mode10 = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0x5a, 0x08, 0x2a, 0, 0, 0, 0, 0, 36, 0, 0, 0],
            28,
        );
        assert_eq!(u16::from_be_bytes(mode10[0..2].try_into().unwrap()), 26);
        assert_eq!(mode10[6], 0);
        assert_eq!(mode10[8], 0x2a);
        assert_eq!(mode10[9], 0x12);

        let performance = atapi_packet(
            &mut ide,
            0x170,
            0x177,
            [0xac, 0, 0, 0, 0, 0, 0, 0, 0, 200, 0, 0],
            8,
        );
        assert_eq!(u32::from_be_bytes(performance[0..4].try_into().unwrap()), 4);
    }

    #[test]
    fn atapi_large_reads_deliver_every_requested_sector() {
        let mut ide = IdeBus::new_enabled();
        let expected = |i: usize| ((i / ATAPI_SECTOR_SIZE) * 17 + i % 251) as u8;
        ide.attach_secondary_master_cdrom((0..130 * ATAPI_SECTOR_SIZE).map(expected).collect());
        for opcode in [0x28, 0xa8] {
            let mut packet = [0; ATAPI_PACKET_LEN];
            packet[0] = opcode;
            packet[5] = 1;
            packet[if opcode == 0x28 { 8 } else { 9 }] = 129;
            start_atapi_command(&mut ide, packet, 32768);
            let mut received = 0;
            loop {
                assert!(ide.irq_asserted(IdeIrq::Secondary));
                let status = ide.read_u8(0x177).unwrap();
                assert_eq!(status & (ATA_STATUS_BUSY | ATA_STATUS_ERR), 0);
                if status & ATA_STATUS_DRQ == 0 {
                    break;
                }
                assert_eq!(ide.read_u8(0x172).unwrap() & 3, 2);
                let len = usize::from(u16::from_le_bytes([
                    ide.read_u8(0x174).unwrap(),
                    ide.read_u8(0x175).unwrap(),
                ]));
                assert!(len > 0 && len <= 32768);
                for _ in 0..len / 2 {
                    for byte in ide.read_u16(0x170).unwrap().to_le_bytes() {
                        assert_eq!(byte, expected(ATAPI_SECTOR_SIZE + received));
                        received += 1;
                    }
                }
            }
            assert_eq!(received, 129 * ATAPI_SECTOR_SIZE, "opcode {opcode:x}");
            assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
        }
    }

    #[test]
    fn atapi_read_bounds_report_sense_instead_of_zero_filled_success() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0xa5; 2 * ATAPI_SECTOR_SIZE]);
        for (lba, count) in [(1u32, 2u32), (u32::MAX, 1), (0, u32::MAX)] {
            let mut packet = [0; ATAPI_PACKET_LEN];
            packet[0] = 0xa8;
            packet[2..6].copy_from_slice(&lba.to_be_bytes());
            packet[6..10].copy_from_slice(&count.to_be_bytes());
            start_atapi_command(&mut ide, packet, 2048);
            assert_eq!(
                ide.read_u8(0x177).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
                ATA_STATUS_ERR
            );
            assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3); // status phase, not CDB input
            assert_eq!(ide.read_u8(0x171).unwrap() >> 4, 5); // illegal request
            let sense = atapi_packet(
                &mut ide,
                0x170,
                0x177,
                [3, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0],
                18,
            );
            assert_eq!(sense[2], 5);
            assert_eq!(sense[12], 0x21); // logical block out of range
        }
        // A zero-block transfer has no data or media access, even at this LBA.
        start_atapi_command(
            &mut ide,
            [0xa8, 0, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0],
            2048,
        );
        assert_eq!(
            ide.read_u8(0x177).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
            0
        );
        assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);

        // READ CD must respect which sector fields the host requested.
        let mut packet = [0xbe, 0, 0, 0, 0, 0, 0, 0, 1, 0x10, 0, 0];
        start_atapi_command(&mut ide, packet, 2048);
        assert_eq!(read_task_pair(&mut ide, 0x174), 2048);
        for _ in 0..1024 {
            assert_eq!(ide.read_u16(0x170), Some(0xa5a5));
        }
        for fields in [0, 0xf8] {
            packet[9] = fields;
            start_atapi_command(&mut ide, packet, 2048);
            assert_eq!(
                ide.read_u8(0x177).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
                if fields == 0 { 0 } else { ATA_STATUS_ERR }
            );
            assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
        }
    }

    #[test]
    fn atapi_reply_length_respects_allocation_and_available_data() {
        let mut ide = IdeBus::new_enabled();
        ide.attach_secondary_master_cdrom(vec![0; ATAPI_SECTOR_SIZE]);
        for (opcode, available) in [(3, 18), (0x12, 36), (0x43, 20)] {
            for allocation in [0u16, 4, 255] {
                let mut packet = [0; ATAPI_PACKET_LEN];
                packet[0] = opcode;
                if opcode == 0x43 {
                    packet[7..9].copy_from_slice(&allocation.to_be_bytes());
                } else {
                    packet[4] = allocation as u8;
                }
                start_atapi_command(&mut ide, packet, 2048);
                let status = ide.read_u8(0x177).unwrap();
                assert_eq!(status & ATA_STATUS_ERR, 0);
                let len = allocation.min(available);
                if len == 0 {
                    assert_eq!(status & ATA_STATUS_DRQ, 0);
                } else {
                    assert_ne!(status & ATA_STATUS_DRQ, 0);
                    assert_eq!(read_task_pair(&mut ide, 0x174), len);
                    for _ in 0..len.div_ceil(2) {
                        ide.read_u16(0x170);
                    }
                    assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, 0);
                }
                assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
            }
        }
        for limit in [0, 1] {
            start_atapi_command(&mut ide, [0x12, 0, 0, 0, 36, 0, 0, 0, 0, 0, 0, 0], limit);
            assert_eq!(
                ide.read_u8(0x177).unwrap() & (ATA_STATUS_ERR | ATA_STATUS_DRQ),
                ATA_STATUS_ERR
            );
            assert_eq!(ide.read_u8(0x172).unwrap() & 3, 3);
        }
    }

    fn start_atapi_command(ide: &mut IdeBus, packet: [u8; ATAPI_PACKET_LEN], limit: u16) {
        ide.write_u8(0x174, limit as u8);
        ide.write_u8(0x175, (limit >> 8) as u8);
        ide.write_u8(0x177, ATA_CMD_PACKET);
        assert_ne!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, 0);
        for bytes in packet.chunks_exact(2) {
            ide.write_u16(0x170, u16::from_le_bytes([bytes[0], bytes[1]]));
        }
    }

    #[test]
    fn atapi_read_splits_on_byte_count_limit() {
        let mut ide = IdeBus::new_enabled();
        let iso: Vec<u8> = (0..ATAPI_SECTOR_SIZE * 2).map(|i| i as u8).collect();
        ide.attach_secondary_master_cdrom(iso);
        ide.write_u8(0x176, 0xa0);
        ide.write_u8(0x174, 8);
        ide.write_u8(0x175, 0);
        ide.write_u8(0x177, ATA_CMD_PACKET);
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
        let packet = [0x28u8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
        for chunk in packet.chunks(2) {
            ide.write_u16(0x170, u16::from_le_bytes([chunk[0], chunk[1]]));
        }

        assert_eq!(ide.read_u8(0x174), Some(8));
        assert_eq!(ide.read_u8(0x175), Some(0));
        let mut first = [0u8; 8];
        for chunk in first.chunks_mut(2) {
            chunk.copy_from_slice(&ide.read_u16(0x170).unwrap().to_le_bytes());
        }
        assert_eq!(first, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, ATA_STATUS_DRQ);
        assert_eq!(ide.read_u8(0x174), Some(8));

        for _ in 8..ATAPI_SECTOR_SIZE {
            let _ = ide.read_u8(0x170);
        }
        assert!(ide.irq_asserted(IdeIrq::Secondary));
        assert_eq!(ide.read_u8(0x172).unwrap() & 0x03, 0x03);
        assert_eq!(ide.read_u8(0x177).unwrap() & ATA_STATUS_DRQ, 0);
    }

    fn atapi_packet(
        ide: &mut IdeBus,
        data_port: u16,
        command_port: u16,
        packet: [u8; ATAPI_PACKET_LEN],
        read_len: usize,
    ) -> Vec<u8> {
        ide.write_u8(command_port, ATA_CMD_PACKET);
        assert_eq!(
            ide.read_u8(command_port).unwrap() & ATA_STATUS_DRQ,
            ATA_STATUS_DRQ
        );
        for chunk in packet.chunks(2) {
            ide.write_u16(data_port, u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        assert_eq!(
            ide.read_u8(command_port).unwrap() & ATA_STATUS_DRQ,
            ATA_STATUS_DRQ
        );
        let mut data = vec![0; read_len];
        for chunk in data.chunks_mut(2) {
            let word = ide.read_u16(data_port).unwrap().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        data
    }

    #[test]
    fn hdd_sector_progress_survives_reset_and_reports_end_of_disk() {
        fn command(ide: &mut IdeBus, lba: u8, count: u8, command: u8) {
            ide.write_u8(0x1f2, count);
            ide.write_u8(0x1f3, lba);
            ide.write_u8(0x1f4, 0);
            ide.write_u8(0x1f5, 0);
            // libata loads DEVICE after the other task-file registers.
            ide.write_u8(0x1f6, 0xe0);
            ide.write_u8(0x1f7, command);
        }
        let mut ide = IdeBus::new_enabled();
        ide.attach_primary_master_hdd(vec![0; 3 * 512], true);
        command(&mut ide, 0, 3, ATA_CMD_WRITE_SECTORS);
        assert!(!ide.irq_asserted(IdeIrq::Primary)); // first write DRQ is polled
        for _ in 0..256 {
            ide.write_u16(0x1f0, 0x1234);
        }
        assert_eq!(ide.read_u8(0x3f6), Some(ATA_STATUS_BUSY));
        assert!(!ide.irq_asserted(IdeIrq::Primary));
        ide.advance(PIO_WRITE_STEPS - 1);
        assert_eq!(ide.read_u8(0x1f7), Some(ATA_STATUS_BUSY));
        assert!(!ide.irq_asserted(IdeIrq::Primary));
        ide.advance(PIO_WRITE_STEPS);
        assert!(ide.irq_asserted(IdeIrq::Primary));
        assert_eq!(ide.read_u8(0x1f2), Some(2));
        assert_eq!(ide.read_u8(0x1f3), Some(1));
        assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_DRQ, 0);
        assert!(!ide.irq_asserted(IdeIrq::Primary));
        for _ in 0..255 {
            ide.write_u16(0x1f0, 0x5678);
        }
        // Interrupt a multi-sector command in the middle of sector 2.
        ide.write_u8(0x3f6, 4);
        ide.write_u8(0x3f6, 0);
        assert_eq!(ide.read_u16(0x1f0), Some(0));
        command(&mut ide, 0, 3, ATA_CMD_READ_SECTORS);
        for value in [0x1234, 0, 0] {
            assert!(ide.irq_asserted(IdeIrq::Primary));
            assert_eq!(
                ide.read_u8(0x1f7).unwrap() & (ATA_STATUS_DRQ | ATA_STATUS_ERR),
                ATA_STATUS_DRQ
            );
            for _ in 0..256 {
                assert_eq!(ide.read_u16(0x1f0), Some(value));
            }
        }
        assert_eq!(ide.read_u8(0x1f2), Some(0));
        assert_eq!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_DRQ, 0);
        // The valid prefix completes; a sector beyond capacity must fail,
        // leave the task file at that sector, and never grow the disk.
        command(&mut ide, 2, 2, ATA_CMD_WRITE_SECTORS);
        for _ in 0..2 {
            for _ in 0..256 {
                ide.write_u16(0x1f0, 0xabcd);
            }
            let deadline = ide.next_event_step();
            if deadline != u64::MAX {
                ide.advance(deadline);
            }
        }
        assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
        assert_eq!(ide.read_u8(0x1f1), Some(0x10));
        assert_eq!(ide.read_u8(0x1f2), Some(1));
        assert_eq!(ide.read_u8(0x1f3), Some(3));
        command(&mut ide, 2, 1, ATA_CMD_READ_SECTORS);
        for _ in 0..256 {
            assert_eq!(ide.read_u16(0x1f0), Some(0xabcd));
        }
        command(&mut ide, 3, 1, ATA_CMD_READ_SECTORS);
        assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
        command(&mut ide, 3, 1, ATA_CMD_READ_VERIFY);
        assert_ne!(ide.read_u8(0x1f7).unwrap() & ATA_STATUS_ERR, 0);
        // Legacy CHS addressing uses INITIALIZE DEVICE PARAMETERS, not the
        // VHD file's advisory geometry. C=1/H=0/S=1 maps to LBA 2 here.
        ide.write_u8(0x1f2, 2);
        ide.write_u8(0x1f6, 0xa0);
        ide.write_u8(0x1f7, ATA_CMD_INIT_DEV_PARAMS);
        ide.write_u8(0x1f2, 1);
        ide.write_u8(0x1f3, 1);
        ide.write_u8(0x1f4, 1);
        ide.write_u8(0x1f5, 0);
        ide.write_u8(0x1f6, 0xa0);
        ide.write_u8(0x1f7, ATA_CMD_READ_SECTORS);
        for _ in 0..256 {
            assert_eq!(ide.read_u16(0x1f0), Some(0xabcd));
        }
    }
}
