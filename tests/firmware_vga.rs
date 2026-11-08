#![cfg(feature = "host")]
//! Boot the real firmware: a preloaded ROM alone does not establish that the
//! BIOS discovers it, installs INT 10h, or programs a visible graphics mode.
use r86::FirmwareDisplay;
use r86::disk::Disk;
use r86::machine::{GuestStepStatus, Machine};

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn seabios_discovers_vga_rom_and_draws_after_reset() {
    let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
    let mut disk = vec![0; 8 << 20];
    disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_vga.bin"));
    m.attach_disk(Disk::memory(disk, false));
    m.load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
        .unwrap();
    m.load_option_rom(
        0xc0000,
        include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
    )
    .unwrap();
    m.reset_bios();
    for boot in 0..2 {
        if boot != 0 {
            m.reboot().unwrap();
        }
        let start = m.serial_output().len();
        let report = m.run_steps(100_000_000).unwrap();
        let log = String::from_utf8_lossy(&m.serial_output()[start..]);
        assert!(
            matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
            "{report:?}\n{log}"
        );
        assert!(log.contains("R86_VGA_BIOS_OK"), "{log}");
        assert!(!log.contains("R86_VGA_BIOS_ERROR"), "{log}");
        let info = m.vga().mode_info();
        assert_eq!((info.width, info.height), (640, 480));
        let mut pixels = vec![0; 640 * 480 * 4];
        m.vga().render_rgba(&mut pixels).unwrap();
        let pos = (10 * 640 + 10) * 4;
        assert_eq!(&pixels[pos..pos + 4], &[170, 0, 0, 255]);
        assert_eq!(&pixels[pos + 4..pos + 8], &[0, 0, 0, 255]);
        let copied = (20 * 640 + 26) * 4;
        assert_eq!(&pixels[copied..copied + 4], &[170, 0, 0, 255]);
        assert_eq!(&pixels[copied + 4..copied + 8], &[0, 0, 0, 255]);
    }
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn seabios_vbe_800x600_planar_and_rgb565() {
    // These are different memory models, not repeated timing samples. The
    // guest uses BIOS mode discovery/setting and the A000 banked aperture;
    // checking the final corners catches bad pitch, plane routing and banks.
    for (mode, bpp, level) in [(0x102u16, 4, 170), (0x114, 16, 255)] {
        let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
        let mut disk = vec![0; 8 << 20];
        disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_vbe.bin"));
        disk[0x1f0..0x1f2].copy_from_slice(&mode.to_le_bytes());
        m.attach_disk(Disk::memory(disk, false));
        m.load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
            .unwrap();
        m.load_option_rom(
            0xc0000,
            include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
        )
        .unwrap();
        m.reset_bios();
        let report = m.run_steps(100_000_000).unwrap();
        let log = String::from_utf8_lossy(m.serial_output());
        assert!(
            matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
            "{report:?}\n{log}"
        );
        assert!(log.contains("R86_VBE_OK"), "mode={mode:x}\n{log}");
        assert!(!log.contains("R86_VBE_ERROR"), "mode={mode:x}\n{log}");
        let info = m.vga().mode_info();
        assert_eq!((info.width, info.height, info.bpp), (800, 600, bpp));
        let mut pixels = vec![0; 800 * 600 * 4];
        m.vga().render_rgba(&mut pixels).unwrap();
        for (x, y, color) in [
            (0, 0, [level, 0, 0, 255]),
            (799, 0, [0, level, 0, 255]),
            (0, 599, [0, 0, level, 255]),
            (799, 599, [255; 4]),
        ] {
            let pos = (y * 800 + x) * 4;
            assert_eq!(&pixels[pos..pos + 4], &color, "mode={mode:x} at {x},{y}");
        }
        if bpp == 16 {
            assert_framebuffer_resource(&m);
        }
    }
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn handheld_vga_bios_offers_the_cardputer_320x170_mode() {
    // Only the handheld ROM lists mode 1A0h; the guest queries, sets and
    // draws it through the BIOS, so the mode reaches drivers such as VBEMP.
    let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
    let mut disk = vec![0; 8 << 20];
    disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_vbe_handheld.bin"));
    m.attach_disk(Disk::memory(disk, false));
    m.load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
        .unwrap();
    m.load_option_rom(
        0xc0000,
        include_bytes!("../firmware/seabios/vgabios-handheld.bin"),
    )
    .unwrap();
    m.reset_bios();
    let report = m.run_steps(100_000_000).unwrap();
    let log = String::from_utf8_lossy(m.serial_output());
    assert!(
        matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
        "{report:?}\n{log}"
    );
    assert!(log.contains("R86_VBE_OK"), "{log}");
    let info = m.vga().mode_info();
    assert_eq!((info.width, info.height, info.bpp), (320, 170, 16));
    let mut pixels = vec![0; 320 * 170 * 4];
    m.vga().render_rgba(&mut pixels).unwrap();
    for (x, y, color) in [
        (0, 0, [255, 0, 0, 255]),
        (319, 0, [0, 255, 0, 255]),
        (0, 169, [0, 0, 255, 255]),
        (319, 169, [255; 4]),
    ] {
        let pos = (y * 320 + x) * 4;
        assert_eq!(&pixels[pos..pos + 4], &color, "at {x},{y}");
    }
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn seabios_vbe_eight_bit_dac_matches_its_advertised_capability() {
    let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
    let mut disk = vec![0; 8 << 20];
    disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_dac.bin"));
    m.attach_disk(Disk::memory(disk, false));
    m.load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
        .unwrap();
    m.load_option_rom(
        0xc0000,
        include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
    )
    .unwrap();
    m.reset_bios();
    let report = m.run_steps(100_000_000).unwrap();
    let log = String::from_utf8_lossy(m.serial_output());
    assert!(
        matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
        "{report:?}\n{log}"
    );
    assert!(
        log.contains("R86_DAC_OK") && !log.contains("ERROR"),
        "{log}"
    );
    let info = m.vga().mode_info();
    assert_eq!((info.width, info.height, info.bpp), (800, 600, 8));
    let mut pixels = vec![0; 800 * 600 * 4];
    m.vga().render_rgba(&mut pixels).unwrap();
    for (x, y, color) in [
        (0, 0, [0x80, 0xa5, 0xff, 255]),
        (799, 0, [0x12, 0x34, 0x56, 255]),
        (0, 599, [0x9a, 0xfe, 0x34, 255]),
        (799, 599, [255; 4]),
    ] {
        let pos = (y * 800 + x) * 4;
        assert_eq!(&pixels[pos..pos + 4], &color, "at {x},{y}");
    }
}

fn assert_framebuffer_resource(m: &Machine) {
    // Check the firmware's *actual* BAR allocation, VBE mode information and
    // relocated ACPI table together. Rendering through A000 alone missed an
    // ACPI window that made Windows reject an otherwise successful VBE mode.
    let ram = m.ram();
    let word = |pos| u32::from_le_bytes(ram[pos..pos + 4].try_into().unwrap());
    let bar = word(0x9100) & !15;
    let size = (!(word(0x9104) & !15)).wrapping_add(1);
    assert_eq!(word(0x9028), bar, "VBE framebuffer differs from PCI BAR0");
    assert!(size.is_power_of_two() && size >= 800 * 600 * 2);
    let end = bar.checked_add(size - 1).unwrap();
    let rsdp = (0xe0000..0x100000)
        .step_by(16)
        .find(|&p| &ram[p..p + 8] == b"RSD PTR ")
        .unwrap();
    let rsdt = word(rsdp + 16) as usize;
    let fadt = word(rsdt + 36) as usize;
    let dsdt = word(fadt + 40) as usize;
    assert_eq!(&ram[dsdt..dsdt + 4], b"DSDT");
    let aml = &ram[dsdt + 36..dsdt + word(dsdt + 4) as usize];
    // The static PCI0 _CRS uses ACPI DWord Address Space descriptors: tag,
    // payload length, memory type, general flags, then granularity/min/max/
    // translation/length. Decode the resource rather than matching its address.
    let covered = aml.windows(26).any(|d| {
        if d[..5] != [0x87, 23, 0, 0, 0x0c] {
            return false;
        }
        let value = |p| u32::from_le_bytes(d[p..p + 4].try_into().unwrap());
        value(10) <= bar && end <= value(14) && value(18) == 0
    });
    assert!(
        covered,
        "ACPI PCI0 _CRS excludes framebuffer {bar:#x}..={end:#x}"
    );
}
