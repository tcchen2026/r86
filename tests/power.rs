#![cfg(feature = "host")]
//! Shutdown tests cross the CPU/device boundary. The firmware gate boots an
//! actual disk through SeaBIOS, writes it through INT 13h, reboots and invokes
//! APM; it never reaches into controller state to simulate guest progress.
use r86::FirmwareDisplay;
use r86::disk::Disk;
use r86::machine::{BootOrder, GuestStepStatus, Machine};

#[test]
fn power_button_wakes_halted_guest_via_sci_and_guest_requests_shutdown() {
    let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    machine
        .load_bootsector(include_bytes!("bootsectors/acpi_button.bin"))
        .unwrap();
    machine.enter_bootsector();
    machine.run_steps(2000).unwrap();
    assert!(machine.cpu().halted);
    machine.press_power_button();
    assert!(!machine.powered_off());
    let report = machine.run_steps(1000).unwrap();
    assert!(
        matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
        "{report:?}"
    );
    assert_eq!(machine.serial_output(), b"P");
}

#[test]
fn shutdown_stops_at_the_out_instruction_and_cannot_resume() {
    let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    let mut boot = [0; 512];
    let code = [
        0xba, 0x04, 0x06, // mov dx,0x604
        0xb8, 0x00, 0x20, // mov ax,0x2000
        0xef, // out dx,ax: ACPI S5
        0xb0, b'X', 0xba, 0xf8, 0x03, 0xee, // must never print X
    ];
    boot[..code.len()].copy_from_slice(&code);
    boot[510..].copy_from_slice(&[0x55, 0xaa]);
    machine.load_bootsector(&boot).unwrap();
    machine.enter_bootsector();
    let result = machine.run_steps(1000).unwrap();
    assert!(matches!(result.stop, Some(GuestStepStatus::PoweredOff)));
    assert_eq!(result.ran, 3);
    assert_eq!(machine.cpu().eip, 0x7c07);
    assert!(machine.serial_output().is_empty());
    let step = machine.step_count();
    assert_eq!(machine.run_steps(1000).unwrap().ran, 0);
    assert!(!machine.advance_halted(1000));
    assert!(matches!(
        machine.step_once().unwrap(),
        GuestStepStatus::PoweredOff
    ));
    assert_eq!(machine.step_count(), step);
    assert_eq!(machine.cpu().eip, 0x7c07);
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test power -- --include-ignored"]
fn empty_cdrom_waits_at_firmware_prompt_without_rebooting() {
    let mut machine = Machine::new_firmware(32 << 20, FirmwareDisplay::Vga).unwrap();
    machine.attach_primary_cdrom(Vec::new());
    machine
        .load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
        .unwrap();
    machine
        .load_option_rom(
            0xc0000,
            include_bytes!("../firmware/seabios/vgabios-stdvga.bin"),
        )
        .unwrap();
    machine.reset_bios();
    // Advance past device discovery and the boot failure, then observe more
    // timer interrupts. The prior zero retry delay repeatedly reset the PC.
    for _ in 0..100 {
        machine.run_steps(1_000_000).unwrap();
        machine.advance_halted(1_000_000);
    }
    let serial = String::from_utf8_lossy(machine.serial_output());
    assert!(serial.contains("MEDIUM NOT PRESENT"), "{serial}");
    assert!(serial.contains("No bootable device."), "{serial}");
    assert!(!serial.contains("Rebooting."), "{serial}");
    assert_eq!(machine.reset_count(), 0);
}

fn firmware(disk: Disk, cd: bool, order: BootOrder) -> Machine {
    let mut machine = Machine::new_firmware(32 << 20, FirmwareDisplay::Serial).unwrap();
    machine.attach_disk(disk);
    if cd {
        machine.attach_secondary_cdrom(boot_cd());
    }
    machine.set_boot_order(order);
    machine
        .load_bios_rom(include_bytes!("../firmware/seabios/bios.bin"))
        .unwrap();
    machine.reset_bios();
    machine
}

fn off(machine: &mut Machine, marker: &str) {
    let report = machine.run_steps(500_000_000).unwrap();
    let serial = String::from_utf8_lossy(machine.serial_output());
    assert!(
        matches!(report.stop, Some(GuestStepStatus::PoweredOff)),
        "{report:?}\n{serial}"
    );
    assert!(serial.contains(marker), "{serial}");
    assert!(!serial.contains("R86_APM_ERROR"), "{serial}");
}

#[test]
#[ignore = "boots vendored SeaBIOS; cargo test --release --test power -- --include-ignored"]
fn seabios_boot_order_apm_shutdown_and_vhd_reboot_reopen() {
    let dir = std::env::temp_dir().join(format!("r86-power-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("boot.vhd");
    Disk::create_vhd(&path, 8 << 20).unwrap();
    let mut disk = Disk::open(&path, true).unwrap();
    disk.write(0, include_bytes!("bootsectors/apm_disk.bin"))
        .unwrap();
    disk.flush().unwrap();

    let mut machine = firmware(disk, true, BootOrder::CdromFirst);
    off(&mut machine, "R86_CD_BOOT");
    assert_eq!(machine.reset_count(), 0);
    assert!(!String::from_utf8_lossy(machine.serial_output()).contains("R86_APM_FIRST"));
    drop(machine);

    let mut machine = firmware(Disk::open(&path, true).unwrap(), true, BootOrder::DiskFirst);
    off(&mut machine, "R86_APM_REBOOT");
    assert_eq!(machine.reset_count(), 1);
    assert!(String::from_utf8_lossy(machine.serial_output()).contains("R86_APM_FIRST"));
    assert!(!String::from_utf8_lossy(machine.serial_output()).contains("R86_CD_BOOT"));
    // Inspect what SeaBIOS actually installed, not the unrelocated host template.
    let ram = machine.ram();
    let rsdp = (0xe0000..0x100000)
        .step_by(16)
        .find(|&pos| &ram[pos..pos + 8] == b"RSD PTR ")
        .unwrap();
    assert_eq!(
        ram[rsdp..rsdp + 20]
            .iter()
            .fold(0u8, |a, b| a.wrapping_add(*b)),
        0
    );
    let word = |pos| u32::from_le_bytes(ram[pos..pos + 4].try_into().unwrap()) as usize;
    let rsdt = word(rsdp + 16);
    let fadt = word(rsdt + 36);
    let facs = word(fadt + 36);
    let dsdt = word(fadt + 40);
    for (pos, sig) in [(rsdt, b"RSDT"), (fadt, b"FACP"), (dsdt, b"DSDT")] {
        assert_eq!(&ram[pos..pos + 4], sig);
        assert_eq!(
            ram[pos..pos + word(pos + 4)]
                .iter()
                .fold(0u8, |a, b| a.wrapping_add(*b)),
            0
        );
    }
    assert_eq!(&ram[facs..facs + 4], b"FACS");
    assert_eq!(facs & 63, 0);
    drop(machine);

    // Remove the CD and make the disk read-only: the boot must use the state
    // persisted by the previous guest, with no host injection or repair.
    let before = std::fs::read(&path).unwrap();
    let mut machine = firmware(
        Disk::open(&path, false).unwrap(),
        false,
        BootOrder::DiskFirst,
    );
    off(&mut machine, "R86_APM_REOPEN");
    assert_eq!(machine.reset_count(), 0);
    drop(machine);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut disk = Disk::open(&path, false).unwrap();
    let mut state = [0; 512];
    disk.read(1, &mut state).unwrap();
    assert_eq!(&state[..2], &[2, 0]);
    assert!(state[2..].iter().all(|&b| b == 0));
    drop(disk);
    std::fs::remove_dir_all(dir).unwrap();
}

fn boot_cd() -> Vec<u8> {
    // Minimal El Torito no-emulation image: validation entry, default entry,
    // then one x86 boot sector. Its marker distinguishes both boot priorities.
    let mut iso = vec![0; 24 * 2048];
    let record = &mut iso[17 * 2048..18 * 2048];
    record[1..7].copy_from_slice(b"CD001\x01");
    record[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
    record[71..75].copy_from_slice(&19u32.to_le_bytes());
    let catalog = &mut iso[19 * 2048..20 * 2048];
    catalog[0] = 1;
    catalog[28..32].copy_from_slice(&[0xaa, 0x55, 0x55, 0xaa]);
    catalog[32] = 0x88;
    catalog[38] = 1;
    catalog[40..44].copy_from_slice(&20u32.to_le_bytes());
    let mut code = vec![0xba, 0xf8, 0x03]; // COM1
    for &b in b"R86_CD_BOOT\n" {
        code.extend_from_slice(&[0xb0, b, 0xee]);
    }
    // APM shutdown through SeaBIOS, the same ABI used by legacy operating systems.
    code.extend_from_slice(&[0xb8, 0x07, 0x53, 0xbb, 1, 0, 0xb9, 3, 0, 0xcd, 0x15, 0xf4]);
    iso[20 * 2048..20 * 2048 + code.len()].copy_from_slice(&code);
    iso
}
