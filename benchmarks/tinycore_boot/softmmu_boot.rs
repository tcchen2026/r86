//! Batched userspace baseline, observing the marker at 4096-step boundaries.
use r86::initrd_overlay::InitrdFile;
use r86::iso9660::Iso9660;
use r86::linux_boot::BootConfig;
use r86::machine::Machine;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).unwrap();
    let iso = Iso9660::open(&bytes).unwrap();
    let kernel = iso.read_file("boot/vmlinuz").unwrap();
    let initrd = iso.read_file("boot/core.gz").unwrap();
    let bootlocal = args
        .get(4)
        .map(String::as_str)
        .unwrap_or("echo R86_BOOTLOCAL_OK");
    let overlay = [
        InitrdFile::new(
            "opt/bootlocal.sh",
            0o755,
            format!("#!/bin/sh\n{{\n{bootlocal}\n}} >/dev/ttyS0 2>&1\n").into_bytes(),
        )
        .unwrap(),
        InitrdFile::new(
            "opt/bootsync.sh",
            0o755,
            b"#!/bin/sh\n/usr/bin/sethostname box\n/opt/bootlocal.sh\n".to_vec(),
        )
        .unwrap(),
    ];
    let mut machine = Machine::new(256 * 1024 * 1024);
    machine.load_linux(&kernel, &initrd, &BootConfig {
        mem_size: 256 * 1024 * 1024,
        cmdline: "console=ttyS0,115200n8 earlyprintk=serial,ttyS0,115200 loglevel=7 nozswap",
        host_decompress_kernel: true,
        host_decompress_initrd: true,
        overlay_files: &overlay,
        dump_initrd_path: None,
    }).unwrap();
    let marker = b"R86_BOOTLOCAL_OK";
    let mut observed = 0;
    let limit = args
        .get(2)
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(1_300_000_000);
    while machine.step_count() < limit {
        let report = machine
            .run_steps((limit - machine.step_count()).min(4096))
            .unwrap();
        assert!(report.stop.is_none(), "{:?}", report.stop);
        let serial = machine.serial_output();
        if serial[observed..]
            .windows(marker.len())
            .any(|s| s == marker)
        {
            break;
        }
        observed = serial.len().saturating_sub(marker.len() - 1);
    }
    if args.get(2).is_none() {
        assert!(
            machine
                .serial_output()
                .windows(marker.len())
                .any(|s| s == marker),
            "marker not reached"
        );
    }
    println!("softmmu.stop.steps={}", machine.step_count());
    println!("softmmu.stop.tsc={}", machine.cpu().tsc());
    println!("softmmu.stop.resets={}", machine.reset_count());
    println!("softmmu.stop.eip={:#010x}", machine.cpu().eip);
    println!(
        "serial.output={}",
        String::from_utf8_lossy(machine.serial_output())
    );
}
