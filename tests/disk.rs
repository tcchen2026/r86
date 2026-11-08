#![cfg(feature = "host")]
//! Storage tests use a flat byte model, real x86 PIO, host-enforced I/O failure,
//! and (explicitly invoked) QEMU. They do not use the VHD parser to manufacture
//! expected data or equate a successful write with a successful persistence test.
use r86::FirmwareDisplay;
use r86::disk::{Disk, DiskError};
use r86::machine::Machine;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: usize = 1024 * 1024;
const BLOCK: usize = 2 * MIB;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "r86-disk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn contents(path: &Path, size: usize) -> Vec<u8> {
    let mut disk = Disk::open(path, false).unwrap();
    assert_eq!(disk.sectors(), (size / 512) as u64);
    let mut bytes = vec![0xcc; size];
    disk.read(0, &mut bytes).unwrap();
    bytes
}

#[test]
fn sparse_disk_matches_flat_model_across_rewrites_and_reopens() {
    let tmp = Scratch::new();
    let path = tmp.path("model.vhd");
    let size = 9 * MIB; // partial last VHD block
    Disk::create_vhd(&path, size as u64).unwrap();
    assert!(fs::metadata(&path).unwrap().len() < 8192);
    let mut model = vec![0; size];
    assert_eq!(contents(&path, size), model);
    let mut random = 0x9e37_79b9u32;
    for phase in 0..3 {
        let mut disk = Disk::open(&path, true).unwrap();
        // Boundary sectors, same bitmap byte, a bitmap-byte boundary, a block
        // crossing and the final virtual sector, plus deterministic scattered I/O.
        let mut spans = vec![(0, 1), (1, 1), (7, 2), (4095, 3), (size / 512 - 1, 1)];
        for _ in 0..48 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            spans.push((
                (random as usize % (size / 512 - 17)),
                1 + (random as usize % 16),
            ));
        }
        for (index, (lba, count)) in spans.into_iter().enumerate() {
            let data: Vec<_> = (0..count * 512)
                .map(|i| (i ^ (i >> 8) ^ index ^ (phase * 73)) as u8)
                .collect();
            disk.write(lba as u64, &data).unwrap();
            model[lba * 512..(lba + count) * 512].copy_from_slice(&data);
        }
        disk.flush().unwrap();
        drop(disk);
        assert_eq!(contents(&path, size), model, "phase {phase}");
    }
    // Virtual capacity must not cause file growth; one far-away sector costs
    // one block, even on filesystems which cannot make holes in raw images.
    let huge = tmp.path("large.vhd");
    Disk::create_vhd(&huge, 128 * 1024 * MIB as u64).unwrap();
    let before = fs::metadata(&huge).unwrap().len();
    assert!(before < MIB as u64);
    let mut disk = Disk::open(&huge, true).unwrap();
    disk.write(disk.sectors() - 1, &[0x5a; 512]).unwrap();
    disk.flush().unwrap();
    assert!(fs::metadata(&huge).unwrap().len() - before <= (BLOCK + 4096) as u64);
    drop(disk);
    let mut disk = Disk::open(&huge, false).unwrap();
    let mut tail = [0xcc; 1024];
    disk.read(disk.sectors() - 2, &mut tail).unwrap();
    assert_eq!(&tail[..512], &[0; 512]);
    assert_eq!(&tail[512..], &[0x5a; 512]);
}

fn guest(path: &Path, writable: bool) -> Machine {
    let mut machine = Machine::new_firmware(2 * MIB, FirmwareDisplay::Serial).unwrap();
    machine.attach_disk(Disk::open(path, writable).unwrap());
    machine
        .load_bootsector(include_bytes!("bootsectors/vhd_pio.bin"))
        .unwrap();
    machine.enter_bootsector();
    machine.run_steps(100_000).unwrap();
    assert!(machine.cpu().halted, "PIO probe did not finish");
    machine
}

#[test]
fn x86_pio_flush_then_reopen_preserves_data_and_zero_neighbours() {
    let tmp = Scratch::new();
    for name in ["guest.vhd", "guest.raw"] {
        let path = tmp.path(name);
        if name.ends_with("vhd") {
            Disk::create_vhd(&path, (8 * MIB) as u64).unwrap();
        } else {
            fs::File::create(&path)
                .unwrap()
                .set_len((8 * MIB) as u64)
                .unwrap();
        }
        // The guest itself issues FLUSH before W; no host flush helper here.
        let machine = guest(&path, true);
        assert_eq!(machine.serial_output(), b"W");
        drop(machine);
        let machine = guest(&path, false);
        assert_eq!(machine.serial_output(), b"R");
        drop(machine);
        let mut expected = vec![0; 8 * MIB];
        for (lba, pattern) in [(4095, [0x3c, 0x6a]), (4096, [0x91, 0xb7])] {
            for word in expected[lba * 512..(lba + 1) * 512].chunks_exact_mut(2) {
                word.copy_from_slice(&pattern);
            }
        }
        assert_eq!(contents(&path, expected.len()), expected);
    }
}

#[test]
fn read_only_bounds_and_exclusive_ownership_preserve_the_image() {
    let tmp = Scratch::new();
    let path = tmp.path("protected.vhd");
    Disk::create_vhd(&path, (4 * MIB) as u64).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(Disk::create_vhd(&path, MIB as u64).is_err()); // never truncate an existing disk
    assert_eq!(guest(&path, false).serial_output(), b"E");
    let mut disk = Disk::open(&path, false).unwrap();
    assert!(matches!(disk.write(0, &[1; 512]), Err(DiskError::ReadOnly)));
    assert!(Disk::open(&path, true).is_err());
    drop(disk);
    let mut disk = Disk::open(&path, true).unwrap();
    assert!(Disk::open(&path, false).is_err());
    assert!(Disk::open(&path, true).is_err());
    assert!(matches!(
        disk.write(disk.sectors() - 1, &[1; 1024]),
        Err(DiskError::OutOfRange)
    ));
    assert!(matches!(
        disk.read(u64::MAX, &mut [0; 512]),
        Err(DiskError::OutOfRange)
    ));
    drop(disk);
    assert_eq!(fs::read(path).unwrap(), before);
}

// Independent byte-level corruptions target trust-boundary invariants, not
// every parser branch. Checksums are repaired only when testing deeper fields.
fn repair_header(bytes: &mut [u8]) {
    bytes[548..552].fill(0);
    let sum = !bytes[512..1536].iter().map(|&x| u32::from(x)).sum::<u32>();
    bytes[548..552].copy_from_slice(&sum.to_be_bytes());
}

#[test]
fn corrupt_mapping_cannot_alias_metadata_or_another_block() {
    let tmp = Scratch::new();
    let path = tmp.path("corrupt.vhd");
    Disk::create_vhd(&path, (8 * MIB) as u64).unwrap();
    let mut disk = Disk::open(&path, true).unwrap();
    disk.write(0, &[0x81; 512]).unwrap();
    disk.write(4096, &[0x42; 512]).unwrap();
    disk.flush().unwrap();
    drop(disk);
    let valid = fs::read(&path).unwrap();
    for damage in 0..6 {
        let mut bytes = valid.clone();
        match damage {
            0 => bytes[1536..1540].copy_from_slice(&1u32.to_be_bytes()), // header alias
            1 => {
                let entry = bytes[1536..1540].to_vec();
                bytes[1540..1544].copy_from_slice(&entry);
            }
            2 => {
                bytes.truncate(bytes.len() - 1024);
                let end = bytes.len();
                bytes[end - 512..].copy_from_slice(&valid[..512]);
            }
            3 => {
                bytes[528..536].copy_from_slice(&u64::MAX.to_be_bytes());
                repair_header(&mut bytes);
            }
            4 => {
                bytes[540..544].copy_from_slice(&u32::MAX.to_be_bytes());
                repair_header(&mut bytes);
            }
            5 => bytes[548] ^= 1,
            _ => unreachable!(),
        }
        fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(Disk::open(&path, true), Err(DiskError::Io(error))
            if error.kind() == std::io::ErrorKind::InvalidData),
            "corruption {damage} was not rejected as invalid metadata"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "open modified corrupt image {damage}"
        );
    }
    // The spec's leading-footer mirror is useful after a torn tail write.
    let mut bytes = valid;
    let tail = bytes.len() - 512;
    bytes[tail + 64] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert_eq!(&contents(&path, 8 * MIB)[..512], &[0x81; 512]);
    bytes[64] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(Disk::open(&path, false).is_err());
}

#[test]
#[cfg(target_os = "linux")]
fn host_write_failure_reaches_the_guest_without_losing_completed_sectors() {
    const CHILD: &str = "R86_DISK_FAILURE_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let path = PathBuf::from(path);
        let length = if path.extension().unwrap() == "raw" {
            BLOCK as u64
        } else {
            fs::metadata(&path).unwrap().len()
        };
        // A child-only kernel limit makes the second block allocation fail
        // with EFBIG. This exercises actual OS I/O, without filling a host disk
        // or installing an artificial File mock in production code.
        unsafe {
            let mut limit = core::mem::zeroed::<libc::rlimit>();
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
            limit.rlim_cur = length;
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        let mut machine = guest(&path, true);
        assert_eq!(machine.serial_output(), b"E");
        assert!(
            machine.flush_disks().is_err(),
            "a failed disk must not report a successful flush"
        );
        drop(machine);
        let bytes = contents(&path, 8 * MIB);
        assert_eq!(&bytes[..512], &[0xd3; 512]); // earlier successful FLUSH
        assert_eq!(&bytes[4095 * 512..4096 * 512], [0x3c, 0x6a].repeat(256));
        assert!(bytes[4096 * 512..].iter().all(|&x| x == 0));
        return;
    }
    let tmp = Scratch::new();
    for name in ["full.vhd", "full.raw"] {
        let path = tmp.path(name);
        if name.ends_with("vhd") {
            Disk::create_vhd(&path, (8 * MIB) as u64).unwrap();
        } else {
            fs::File::create(&path)
                .unwrap()
                .set_len((8 * MIB) as u64)
                .unwrap();
        }
        let mut disk = Disk::open(&path, true).unwrap();
        disk.write(0, &[0xd3; 512]).unwrap();
        disk.flush().unwrap();
        drop(disk);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_write_failure_reaches_the_guest_without_losing_completed_sectors",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn checked(command: &mut Command) {
    let output = command
        .output()
        .expect("install qemu-img and qemu-io for this test");
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "independent interoperability check: requires qemu-img and qemu-io"]
fn qemu_and_r86_exchange_dynamic_vhd_images() {
    let tmp = Scratch::new();
    for creator in ["r86", "qemu"] {
        let path = tmp.path(&format!("{creator}.vhd"));
        let raw = tmp.path(&format!("{creator}.raw"));
        let size = 8 * MIB;
        if creator == "r86" {
            Disk::create_vhd(&path, size as u64).unwrap();
        } else {
            checked(
                Command::new("qemu-img")
                    .args([
                        "create",
                        "-f",
                        "vpc",
                        "-o",
                        "subformat=dynamic,force_size=on",
                    ])
                    .arg(&path)
                    .arg(size.to_string()),
            );
        }
        checked(
            Command::new("qemu-io")
                .args([
                    "-f",
                    "vpc",
                    "-c",
                    "write -P 0x6d 2096640 1024",
                    "-c",
                    "write -P 0xee 3584 1024",
                    "-c",
                    "flush",
                ])
                .arg(&path),
        );
        let mut expected = vec![0; size];
        expected[2096640..2097664].fill(0x6d);
        expected[3584..4608].fill(0xee);
        assert_eq!(contents(&path, size), expected);
        let mut disk = Disk::open(&path, true).unwrap();
        disk.write(8, &[0x21; 512]).unwrap();
        disk.write(8191, &[0x73; 1024]).unwrap();
        disk.write((size / 512 - 1) as u64, &[0x19; 512]).unwrap();
        disk.flush().unwrap();
        drop(disk);
        expected[4096..4608].fill(0x21);
        expected[8191 * 512..8193 * 512].fill(0x73);
        expected[size - 512..].fill(0x19);
        checked(
            Command::new("qemu-img")
                .args(["convert", "-f", "vpc", "-O", "raw"])
                .arg(&path)
                .arg(&raw),
        );
        assert_eq!(
            fs::read(raw).unwrap(),
            expected,
            "independent reader of {creator} image"
        );
    }
}
