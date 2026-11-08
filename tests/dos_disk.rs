#![cfg(feature = "host")]
//! Read the public installer's output using an independent, read-only FAT walk.
//! Synthetic boot code checks byte preservation; real boot checks use the kit.
use r86::disk::Disk;
use r86::dos_disk;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: usize = 1024 * 1024;

struct Kit(PathBuf);

impl Kit {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "r86-dos-kit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("dos/boot")).unwrap();
        for profile in ["base", "freedos", "win31", "win95", "win98", "winme"] {
            fs::create_dir(path.join("dos").join(profile)).unwrap();
        }
        for (name, jump) in [("mbr", 0), ("fat16", 60), ("fat32", 88)] {
            let mut boot: Vec<_> = (0..512).map(|i| (i ^ (i >> 4)) as u8).collect();
            boot[..3].copy_from_slice(&[0xeb, jump, 0x90]);
            boot[510..].copy_from_slice(&[0x55, 0xaa]);
            fs::write(path.join(format!("dos/boot/{name}.bin")), boot).unwrap();
        }
        let kit = Self(path);
        kit.file("base", "KERNEL.SYS", b"synthetic boot target");
        kit
    }

    fn file(&self, profile: &str, name: &str, bytes: &[u8]) {
        let path = self.0.join("dos").join(profile).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn empty_dir(&self, profile: &str, name: &str) {
        fs::create_dir_all(self.0.join("dos").join(profile).join(name)).unwrap();
    }

    fn install(&self, name: &str, size: u64, profile: &str) -> (PathBuf, u64) {
        let path = self.0.join(name);
        let mut updates = Vec::new();
        dos_disk::create(&path, size, profile, &self.0, |done, total| {
            updates.push((done, total));
        })
        .unwrap();
        let total = updates[0].1;
        assert_eq!(updates[0], (0, total));
        assert_eq!(*updates.last().unwrap(), (total, total));
        assert!(
            updates
                .windows(2)
                .all(|w| w[0].0 < w[1].0 && w[0].1 == w[1].1)
        );
        (path, total)
    }

    fn rejected(&self, size: u64, profile: &str, text: &str) {
        let path = self.0.join("rejected.vhd");
        let error = dos_disk::create(&path, size, profile, &self.0, |_, _| {
            panic!("preflight failure must not report write progress")
        })
        .unwrap_err();
        assert!(error.contains(text), "{error:?} did not contain {text:?}");
        assert!(!path.exists(), "preflight failure created a disk");
    }
}

impl Drop for Kit {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn read(disk: &mut Disk, sector: u64, sectors: u32) -> Vec<u8> {
    let mut bytes = vec![0xcc; sectors as usize * 512];
    disk.read(sector, &mut bytes).unwrap();
    bytes
}

struct Volume {
    disk: Disk,
    mbr: Vec<u8>,
    boot: Vec<u8>,
    fat: Vec<u8>,
    fat32: bool,
    spc: u32,
    data: u64,
    clusters: u32,
    root: Vec<u8>,
    seen: BTreeSet<u32>,
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<String>,
}

impl Volume {
    fn open(path: &Path, mib: u64, fat32: bool) -> Self {
        let mut disk = Disk::open(path, false).unwrap();
        assert_eq!(disk.sectors(), mib * 2048);
        let mbr = read(&mut disk, 0, 1);
        assert_eq!(&mbr[510..], &[0x55, 0xaa]);
        assert_eq!(mbr[446], 0x80);
        assert_eq!(mbr[450], if fat32 { 0x0c } else { 0x0e });
        assert_eq!(&mbr[462..510], &[0; 48]);
        let start = dword(&mbr, 454);
        let sectors = dword(&mbr, 458);
        assert_eq!(start, 63);
        assert_eq!(u64::from(start + sectors), mib * 2048 / 1008 * 1008);
        assert_eq!(&mbr[447..450], &[1, 1, 0]);
        if mib > 504 {
            assert_eq!(&mbr[451..454], &[15, 255, 255], "end CHS must saturate");
        }
        let boot = read(&mut disk, start as u64, 1);
        assert_eq!(&boot[510..], &[0x55, 0xaa]);
        assert_eq!(word(&boot, 11), 512);
        assert_eq!(boot[16], 2);
        assert_eq!(boot[21], 0xf8);
        assert_eq!(word(&boot, 24), 63);
        assert_eq!(word(&boot, 26), 16);
        assert_eq!(dword(&boot, 28), start);
        let total = if word(&boot, 19) != 0 {
            u32::from(word(&boot, 19))
        } else {
            dword(&boot, 32)
        };
        assert_eq!(total, sectors);
        let spc = u32::from(boot[13]);
        assert!(spc.is_power_of_two() && spc <= 64);
        let reserved = u32::from(word(&boot, 14));
        let fat_sectors = if fat32 {
            dword(&boot, 36)
        } else {
            u32::from(word(&boot, 22))
        };
        let root_sectors = (u32::from(word(&boot, 17)) * 32).div_ceil(512);
        let fat_start = start + reserved;
        let fat = read(&mut disk, fat_start as u64, fat_sectors);
        assert_eq!(
            fat,
            read(&mut disk, (fat_start + fat_sectors) as u64, fat_sectors)
        );
        let data_offset = reserved + 2 * fat_sectors + root_sectors;
        let clusters = (sectors - data_offset) / spc;
        if fat32 {
            assert!(clusters >= 65525);
            assert_eq!(word(&boot, 17), 0);
            assert_eq!(word(&boot, 19), 0);
            assert_eq!(word(&boot, 22), 0);
            assert_eq!(word(&boot, 40), 0); // mirrored FATs
            assert_eq!(word(&boot, 42), 0); // FAT32 version
            assert_eq!(word(&boot, 48), 1);
            assert_eq!(word(&boot, 50), 6);
            assert_eq!(boot, read(&mut disk, (start + 6) as u64, 1));
            let info = read(&mut disk, (start + 1) as u64, 1);
            assert_eq!(info, read(&mut disk, (start + 7) as u64, 1));
            assert_eq!(dword(&info, 0), 0x4161_5252);
            assert_eq!(dword(&info, 484), 0x6141_7272);
            assert_eq!(dword(&info, 508), 0xaa55_0000);
            let free: Vec<_> = (2..clusters + 2)
                .filter(|&c| dword(&fat, c as usize * 4) == 0)
                .collect();
            assert_eq!(dword(&info, 488), free.len() as u32);
            assert_eq!(dword(&info, 492), *free.first().unwrap_or(&u32::MAX));
            assert_eq!(dword(&fat, 0), 0x0fff_fff8);
            assert_eq!(dword(&fat, 4) & 0x0fff_ffff, 0x0fff_ffff);
        } else {
            assert!((4085..65525).contains(&clusters));
            assert_eq!(word(&boot, 17), 512);
            assert_eq!(word(&fat, 0), 0xfff8);
            assert_eq!(word(&fat, 2), 0xffff);
        }
        let root = if fat32 {
            Vec::new()
        } else {
            read(
                &mut disk,
                (fat_start + 2 * fat_sectors) as u64,
                root_sectors,
            )
        };
        let mut v = Self {
            disk,
            mbr,
            boot,
            fat,
            fat32,
            spc,
            data: u64::from(start + data_offset),
            clusters,
            root,
            seen: BTreeSet::new(),
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        if fat32 {
            assert_eq!(dword(&v.boot, 44), 2);
            v.root = v.chain(2);
        }
        let kernel = &v.root[..32];
        assert_eq!(&kernel[..11], b"KERNEL  SYS");
        assert_eq!(Self::first(kernel), if fat32 { 3 } else { 2 });
        let root = v.root.clone();
        let labels: Vec<_> = root
            .chunks_exact(32)
            .filter(|entry| entry[11] == 8)
            .collect();
        assert!(labels.is_empty(), "kernel is the first root entry");
        assert_eq!(&v.boot[if fat32 { 71 } else { 43 }..][..11], b"NO NAME    ");
        v.walk("", &root, 0, 0);
        let entry_size = if fat32 { 4 } else { 2 };
        for cluster in 2..clusters + 2 {
            let at = cluster as usize * entry_size;
            let allocated = if fat32 {
                dword(&v.fat, at) != 0
            } else {
                word(&v.fat, at) != 0
            };
            assert_eq!(
                allocated,
                v.seen.contains(&cluster),
                "lost cluster {cluster}"
            );
        }
        // Fresh disk padding beyond the partition remains zero.
        let tail = disk_tail(&mut v.disk);
        assert_eq!(tail, [0; 512]);
        v
    }

    fn first(entry: &[u8]) -> u32 {
        u32::from(word(entry, 26)) | (u32::from(word(entry, 20)) << 16)
    }

    fn chain(&mut self, mut cluster: u32) -> Vec<u8> {
        let mut result = Vec::new();
        if cluster == 0 {
            return result;
        }
        loop {
            assert!((2..self.clusters + 2).contains(&cluster));
            assert!(
                self.seen.insert(cluster),
                "shared or cyclic cluster {cluster}"
            );
            result.extend(read(
                &mut self.disk,
                self.data + u64::from(cluster - 2) * u64::from(self.spc),
                self.spc,
            ));
            let next = if self.fat32 {
                dword(&self.fat, cluster as usize * 4) & 0x0fff_ffff
            } else {
                u32::from(word(&self.fat, cluster as usize * 2))
            };
            if next >= if self.fat32 { 0x0fff_fff8 } else { 0xfff8 } {
                break;
            }
            cluster = next;
        }
        result
    }

    fn walk(&mut self, prefix: &str, bytes: &[u8], own: u32, parent: u32) {
        if !prefix.is_empty() {
            assert_eq!(&bytes[..11], b".          ");
            assert_eq!(Self::first(bytes), own);
            assert_eq!(&bytes[32..43], b"..         ");
            assert_eq!(Self::first(&bytes[32..]), parent);
        }
        for entry in bytes.chunks_exact(32) {
            if entry[0] == 0 {
                break;
            }
            if entry[0] == b'.' {
                continue;
            }
            if entry[11] == 8 || entry[11] == 0x0f {
                continue;
            }
            let base = std::str::from_utf8(&entry[..8]).unwrap().trim_end();
            let ext = std::str::from_utf8(&entry[8..11]).unwrap().trim_end();
            let name = format!(
                "{prefix}{base}{}",
                if ext.is_empty() {
                    String::new()
                } else {
                    format!(".{ext}")
                }
            );
            assert_eq!(word(entry, 16), 0x5c21); // 2026-01-01
            assert_eq!(word(entry, 18), word(entry, 24));
            assert_eq!(word(entry, 22), 0);
            let first = Self::first(entry);
            let mut body = self.chain(first);
            if entry[11] & 0x10 != 0 {
                assert_eq!(dword(entry, 28), 0);
                assert!(self.directories.insert(name.clone()));
                self.walk(&format!("{name}/"), &body, first, own);
            } else {
                let size = dword(entry, 28) as usize;
                assert_eq!(first == 0, size == 0);
                assert!(
                    body[size..].iter().all(|&b| b == 0),
                    "nonzero file slack: {name}"
                );
                body.truncate(size);
                assert!(self.files.insert(name, body).is_none());
            }
        }
    }
}

fn disk_tail(disk: &mut Disk) -> Vec<u8> {
    read(disk, disk.sectors() - 1, 1)
}

#[test]
fn fat16_and_fat32_recover_overlay_tree_and_file_bytes() {
    let kit = Kit::new();
    let large: Vec<_> = (0..2 * MIB + 137)
        .map(|i| (i ^ (i >> 7) ^ (i >> 16)) as u8)
        .collect();
    kit.file("base", "Aa.txt", b"base");
    kit.file("base", "NEST/KEEP.DAT", b"retained");
    kit.file("base", "NEST/DEEP/EMPTY.TXT", b"");
    kit.file("base", "NEST/BIG.BIN", &large);
    kit.empty_dir("base", "EMPTYDIR");
    for i in 0..150 {
        kit.file("base", &format!("MANY/F{i:03}.TXT"), &[i as u8]);
    }
    for profile in ["win31", "win98"] {
        kit.file(profile, "aa.TXT", b"overlay");
        kit.file(profile, "NEST/NEW.TXT", profile.as_bytes());
        kit.file(profile, "AUTOEXEC.BAT", b"@ECHO OFF\r\n");
    }
    for (profile, size, fat32) in [("win31", 8, false), ("win98", 33, true)] {
        let (path, total) = kit.install(&format!("{profile}.vhd"), size, profile);
        let v = Volume::open(&path, size, fat32);
        assert_eq!(
            total,
            v.files.values().map(|data| data.len() as u64).sum::<u64>()
        );
        assert_eq!(v.files["AA.TXT"], b"overlay");
        assert_eq!(v.files["NEST/KEEP.DAT"], b"retained");
        assert_eq!(v.files["NEST/NEW.TXT"], profile.as_bytes());
        assert_eq!(v.files["NEST/BIG.BIN"], large);
        assert!(v.files["NEST/DEEP/EMPTY.TXT"].is_empty());
        assert!(v.directories.contains("EMPTYDIR"));
        assert_eq!(v.files.len(), 157);
        let template = fs::read(
            kit.0
                .join(format!("dos/boot/fat{}.bin", if fat32 { 32 } else { 16 })),
        )
        .unwrap();
        let code = if fat32 { 90 } else { 62 };
        assert_eq!(&v.boot[code..], &template[code..]);
        assert_eq!(&v.boot[..3], &template[..3]);
        let template = fs::read(kit.0.join("dos/boot/mbr.bin")).unwrap();
        assert_eq!(&v.mbr[..440], &template[..440]);
        assert!(fs::metadata(path).unwrap().len() < 5 * MIB as u64);
    }
}

#[test]
fn selected_capacity_limits_and_large_geometry_stay_sparse() {
    let kit = Kit::new();
    for (profile, size, fat32) in [
        ("freedos", 3, false),
        ("win95", 2047, false),
        ("win98", 512, true),
        ("winme", 131072, true),
    ] {
        let (path, total) = kit.install(&format!("{profile}.vhd"), size, profile);
        let v = Volume::open(&path, size, fat32);
        assert_eq!(
            total,
            v.files.values().map(|data| data.len() as u64).sum::<u64>()
        );
        assert_eq!(v.files["KERNEL.SYS"], b"synthetic boot target");
        assert!(fs::metadata(path).unwrap().len() < 40 * MIB as u64);
    }
    kit.rejected(2, "freedos", "too small");
    kit.rejected(32, "win98", "too small");
    kit.rejected(2048, "win31", "invalid disk size");
    kit.rejected(131073, "winme", "invalid disk size");
    kit.rejected(u64::MAX, "winme", "invalid disk size");
    kit.rejected(512, "dos", "unsupported DOS kit");
}

#[test]
fn preflight_rejects_bad_names_collisions_conflicts_and_capacity() {
    let kit = Kit::new();
    kit.file("base", "BAD NAME.TXT", b"bad");
    kit.rejected(8, "win31", "8.3");
    fs::remove_file(kit.0.join("dos/base/BAD NAME.TXT")).unwrap();
    kit.file("base", "FILE.TXT", b"one");
    kit.file("base", "file.txt", b"two");
    kit.rejected(8, "win31", "duplicate DOS name");
    fs::remove_file(kit.0.join("dos/base/file.txt")).unwrap();
    kit.empty_dir("win31", "FILE.TXT");
    kit.rejected(8, "win31", "file/directory type");
    fs::remove_dir(kit.0.join("dos/win31/FILE.TXT")).unwrap();
    fs::File::create(kit.0.join("dos/base/TOOBIG.BIN"))
        .unwrap()
        .set_len(8 * MIB as u64)
        .unwrap();
    kit.rejected(8, "win31", "does not fit");
    fs::remove_file(kit.0.join("dos/base/TOOBIG.BIN")).unwrap();
    for i in 0..511 {
        kit.file("base", &format!("F{i:03}.TXT"), b"");
    }
    kit.rejected(8, "win31", "root directory");
    fs::remove_file(kit.0.join("dos/base/FILE.TXT")).unwrap();
    let (path, _) = kit.install("full-root.vhd", 8, "win31");
    assert_eq!(Volume::open(&path, 8, false).files.len(), 512);
}

#[test]
fn existing_destination_and_invalid_templates_are_untouched() {
    let kit = Kit::new();
    let path = kit.0.join("existing.vhd");
    fs::write(&path, b"keep this file").unwrap();
    assert!(
        dos_disk::create(&path, 8, "win31", &kit.0, |_, _| {})
            .unwrap_err()
            .contains("exists")
    );
    assert_eq!(fs::read(path).unwrap(), b"keep this file");
    fs::write(kit.0.join("dos/boot/fat16.bin"), [0; 512]).unwrap();
    kit.rejected(8, "win31", "boot signature");
}

#[test]
fn logical_sectors_are_reproducible_despite_host_file_order() {
    let kit = Kit::new();
    kit.file("base", "B.TXT", b"second");
    kit.file("base", "A.TXT", b"first");
    let (a, _) = kit.install("a.vhd", 3, "freedos");
    fs::remove_file(kit.0.join("dos/base/B.TXT")).unwrap();
    kit.file("base", "B.TXT", b"second");
    let (b, _) = kit.install("b.vhd", 3, "freedos");
    let mut a = Disk::open(a, false).unwrap();
    let mut b = Disk::open(b, false).unwrap();
    for sector in (0..a.sectors()).step_by(128) {
        let count = (a.sectors() - sector).min(128) as u32;
        assert_eq!(read(&mut a, sector, count), read(&mut b, sector, count));
    }
}

#[cfg(unix)]
#[test]
fn kit_symlinks_are_rejected_before_destination_creation() {
    use std::os::unix::fs::symlink;
    let kit = Kit::new();
    symlink("KERNEL.SYS", kit.0.join("dos/base/LINK.SYS")).unwrap();
    kit.rejected(8, "win31", "links");
    fs::remove_file(kit.0.join("dos/base/LINK.SYS")).unwrap();
    fs::remove_dir(kit.0.join("dos/win31")).unwrap();
    symlink("base", kit.0.join("dos/win31")).unwrap();
    kit.rejected(8, "win31", "not a link");
}
