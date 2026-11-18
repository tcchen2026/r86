#![cfg(feature = "host")]
//! Boot the real firmware: a preloaded ROM alone does not establish that the
//! BIOS discovers it, installs INT 10h, or programs a visible graphics mode.
use r86::FirmwareDisplay;
use r86::disk::Disk;
use r86::machine::{GuestStepStatus, Machine};

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn seabios_shows_the_engine_on_post_splash_and_restored_text() {
    // Optional captures let a reviewer inspect the actual firmware pixels.
    fn capture(machine: &Machine, name: &str) {
        let Some(dir) = std::env::var_os("R86_FIRMWARE_CAPTURE_DIR") else {
            return;
        };
        let mode = machine.vga().mode_info();
        let mut rgba = vec![0; usize::from(mode.width) * usize::from(mode.height) * 4];
        machine.vga().render_rgba(&mut rgba).unwrap();
        let mut ppm = format!("P6\n{} {}\n255\n", mode.width, mode.height).into_bytes();
        for pixel in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(std::path::Path::new(&dir).join(name), ppm).unwrap();
    }

    for splash in [false, true] {
        let mut machine = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
        machine
            .load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
            .unwrap();
        machine
            .load_option_rom(
                0xc0000,
                include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
            )
            .unwrap();
        if splash {
            machine.enable_boot_splash();
        }
        machine.reset_bios();
        let label = machine.engine_label();
        let mut post_seen = false;
        let mut splash_seen = false;
        let mut booted = false;
        while machine.step_count() < 150_000_000 {
            machine.run_steps(10_000).unwrap();
            let mode = machine.vga().mode_info();
            let text = machine.text_snapshot();
            if mode.mode == r86::devices::RenderMode::Text {
                let row = text.lines().next().unwrap_or("");
                if row.starts_with("SeaBIOS") && row.ends_with(label) {
                    assert_eq!(row.find(label), Some(80 - label.len()));
                    if !post_seen && !splash {
                        capture(&machine, "e2-post.ppm");
                    }
                    post_seen = true;
                }
                if text.contains("No bootable device") {
                    assert_eq!(row.find(label), Some(80 - label.len()), "{text}");
                    booted = true;
                    break;
                }
            } else if splash && !splash_seen && (mode.width, mode.height) == (320, 200) {
                let mut rgba = vec![0; 320 * 200 * 4];
                machine.vga().render_rgba(&mut rgba).unwrap();
                // The bar is drawn after both the label and the menu hint.
                if rgba[(160 * 320 + 60) * 4..][..3] == [0, 0, 0] {
                    continue;
                }
                let lit = |x: usize, y: usize| rgba[(y * 320 + x) * 4..][..3] != [0, 0, 0];
                let left = (40 - label.len()) * 8;
                assert!((0..8).all(|y| (0..left).all(|x| !lit(x, y))));
                for (index, byte) in label.bytes().enumerate() {
                    let ink = (0..8).any(|y| (0..8).any(|x| lit(left + index * 8 + x, y)));
                    assert_eq!(ink, byte != b' ', "label cell {index}");
                }
                capture(&machine, "e2-splash.ppm");
                splash_seen = true;
            }
        }
        assert!(post_seen && booted, "{}", machine.text_snapshot());
        assert_eq!(splash_seen, splash);
    }
}

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
fn vga_bios_offers_the_cardputer_640x340_modes() {
    for (mode, bpp) in [(0x1a0u16, 16u16), (0x1a3, 32)] {
        let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
        let mut disk = vec![0; 8 << 20];
        disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_vbe_640x340.bin"));
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
        assert!(log.contains("R86_VBE_OK"), "{log}");
        // INT 10h 4F01h left the ModeInfoBlock at 0000:9000.
        let vbe = &m.ram()[0x9000..0x9100];
        let word = |i| u16::from_le_bytes([vbe[i], vbe[i + 1]]);
        let pitch = 640 * (bpp / 8);
        assert_eq!(
            (word(0x12), word(0x14), u16::from(vbe[0x19])),
            (640, 340, bpp)
        );
        assert_eq!((word(0x10), word(0x32)), (pitch, pitch));
        assert_ne!(word(0) & 0x80, 0, "VBE linear framebuffer attribute");
        assert_ne!(u32::from_le_bytes(vbe[0x28..0x2c].try_into().unwrap()), 0);
        let info = m.vga().mode_info();
        assert_eq!(
            (info.width, info.height, info.bpp, info.stride),
            (640, 340, bpp, u32::from(pitch))
        );
        let mut pixels = vec![0; 640 * 340 * 4];
        m.vga().render_rgba(&mut pixels).unwrap();
        for (x, y, color) in [
            (0, 0, [255, 0, 0, 255]),
            (639, 0, [0, 255, 0, 255]),
            (0, 339, [0, 0, 255, 255]),
            (639, 339, [255; 4]),
        ] {
            let pos = (y * 640 + x) * 4;
            assert_eq!(&pixels[pos..pos + 4], &color, "at {x},{y}");
        }
    }
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test firmware_vga -- --include-ignored"]
fn vga_bios_offers_the_duo_936x666_modes() {
    // The guest enumerates each mode, queries its geometry/depth/pitch, sets
    // it and draws through A000. The bottom corners span two banks.
    for (mode, bpp) in [(0x1a1u16, 16u8), (0x1a2, 32)] {
        let mut m = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
        let mut disk = vec![0; 8 << 20];
        disk[..512].copy_from_slice(include_bytes!("bootsectors/firmware_vbe_936x666.bin"));
        disk[0x1f0..0x1f2].copy_from_slice(&mode.to_le_bytes());
        disk[0x1f2] = bpp;
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
            "mode={mode:x} {report:?}\n{log}"
        );
        assert!(log.contains("R86_VBE_OK"), "mode={mode:x}\n{log}");
        assert!(!log.contains("R86_VBE_ERROR"), "mode={mode:x}\n{log}");
        let info = m.vga().mode_info();
        assert_eq!(
            (info.width, info.height, info.bpp),
            (936, 666, u16::from(bpp))
        );
        let mut pixels = vec![0; 936 * 666 * 4];
        m.vga().render_rgba(&mut pixels).unwrap();
        for (x, y, color) in [
            (0, 0, [255, 0, 0, 255]),
            (935, 0, [0, 255, 0, 255]),
            (0, 665, [0, 0, 255, 255]),
            (935, 665, [255; 4]),
        ] {
            let pos = (y * 936 + x) * 4;
            assert_eq!(&pixels[pos..pos + 4], &color, "mode={mode:x} at {x},{y}");
        }
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
