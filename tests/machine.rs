#![cfg(feature = "host")]

use r86::FirmwareDisplay;
use r86::machine::{Machine, PerfMode, TraceMode};

fn bootsector(bytes: &[u8], trace: TraceMode, perf: PerfMode) -> Machine {
    let mut machine = Machine::new_firmware(2 * 1024 * 1024, FirmwareDisplay::Serial).unwrap();
    machine.load_bootsector(bytes).unwrap();
    machine.enter_bootsector();
    machine.set_diagnostics(trace, perf);
    machine
}

#[test]
fn embedded_machine_runs_vga_serial_and_keyboard_without_a_frontend() {
    let mut machine = bootsector(
        include_bytes!("bootsectors/ws_ps2_vga.bin"),
        TraceMode::Off,
        PerfMode::Disabled,
    );
    assert!(machine.run_steps(10_000).unwrap().stop.is_none());
    assert!(
        machine
            .serial_output()
            .windows(13)
            .any(|s| s == b"R86_SYN_READY")
    );
    assert!(machine.text_snapshot().contains("R86_SYN_VGA"));

    machine.inject_ascii(b"a");
    assert!(machine.run_steps(10_000).unwrap().stop.is_none());
    let serial = String::from_utf8_lossy(machine.serial_output());
    assert!(serial.contains("K1E K9E "), "{serial}");
    assert_eq!(machine.step_count(), 20_000);
}

#[test]
fn batched_and_instrumented_execution_deliver_the_same_guest_fault() {
    let mut image = [0u8; 512];
    // Set the real-mode #DE vector, then divide by zero. The handler reports
    // through COM1 and halts. Both execution paths must build the same frame.
    let code = [
        0xfa, 0xbc, 0x00, 0x70, // cli; mov sp, 0x7000
        0xc7, 0x06, 0x00, 0x00, 0x40, 0x7c, // mov word [0], 0x7c40
        0xc7, 0x06, 0x02, 0x00, 0x00, 0x00, // mov word [2], 0
        0x0f, 0x01, 0x1e, 0x50, 0x7c, // lidt [0x7c50]
        0x31, 0xc0, 0xf6, 0xf0, // xor ax, ax; div al
    ];
    image[..code.len()].copy_from_slice(&code);
    image[0x40..0x47].copy_from_slice(&[0xba, 0xf8, 0x03, 0xb0, b'F', 0xee, 0xf4]);
    image[0x50..0x56].copy_from_slice(&[0xff, 0x03, 0, 0, 0, 0]);
    image[510..].copy_from_slice(&[0x55, 0xaa]);

    let mut reference = bootsector(&image, TraceMode::Off, PerfMode::Disabled);
    reference.run_steps(256).unwrap();
    assert_eq!(reference.serial_output(), b"F");
    assert!(reference.cpu().halted);

    for trace in [TraceMode::Off, TraceMode::Minimal, TraceMode::Full] {
        for perf in [PerfMode::Disabled, PerfMode::Enabled] {
            let mut machine = bootsector(&image, trace, perf);
            machine.run_steps(256).unwrap();
            assert_eq!(machine.cpu().regs, reference.cpu().regs);
            assert_eq!(machine.cpu().eip, reference.cpu().eip);
            assert_eq!(
                machine.cpu().effective_eflags(),
                reference.cpu().effective_eflags()
            );
            assert_eq!(machine.cpu().segments(), reference.cpu().segments());
            assert_eq!(machine.cpu().halted, reference.cpu().halted);
            assert_eq!(machine.ram(), reference.ram());
            assert_eq!(machine.serial_output(), reference.serial_output());
            assert_eq!(machine.step_count(), reference.step_count());
        }
    }
}
