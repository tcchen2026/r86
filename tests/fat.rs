#![cfg(feature = "host")]

use fatfs::{FatType, FormatVolumeOptions};
use r86::disk::Disk;
use r86::fat::{self, Partition, Volume};
use std::io::{Read, Seek, SeekFrom, Write};

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn sector(disk: &mut Disk, lba: u64) -> [u8; 512] {
    let mut bytes = [0; 512];
    disk.read(lba, &mut bytes).unwrap();
    bytes
}

fn formatted(kind: FatType) -> (Disk, Volume) {
    let sectors = match kind {
        FatType::Fat12 => 4096,
        FatType::Fat16 => 16384,
        FatType::Fat32 => 81920,
    };
    let mut disk = Disk::memory(vec![0; sectors as usize * 512], true);
    let partition = Partition::new(&mut disk, 0, sectors).unwrap();
    fatfs::format_volume(
        partition,
        FormatVolumeOptions::new()
            .fat_type(kind)
            .bytes_per_sector(512)
            .heads(16)
            .sectors_per_track(63),
    )
    .unwrap();
    let volume = Volume {
        letter: 'C',
        fat: true,
        partition_type: 0,
        start_lba: 0,
        sectors,
    };
    (disk, volume)
}

#[test]
fn partition_reads_see_every_write_to_a_cached_sector() {
    let mut disk = Disk::memory(vec![0; 8 * 512], true);
    let mut p = Partition::new(&mut disk, 1, 4).unwrap();
    let mut read_at = |p: &mut Partition, at: u64| {
        let mut byte = [0];
        p.seek(SeekFrom::Start(at)).unwrap();
        p.read_exact(&mut byte).unwrap();
        byte[0]
    };
    assert_eq!(read_at(&mut p, 10), 0); // a small read caches its sector
    p.seek(SeekFrom::Start(10)).unwrap();
    p.write_all(&[7]).unwrap();
    assert_eq!(read_at(&mut p, 10), 7);
    p.seek(SeekFrom::Start(0)).unwrap();
    p.write_all(&[9; 1024]).unwrap(); // whole sectors over the cached one
    assert_eq!(read_at(&mut p, 10), 9);
    assert_eq!(read_at(&mut p, 1030), 0);
}

#[test]
fn partition_partial_io_preserves_neighbors_and_bounds() {
    let mut disk = Disk::memory(vec![0xa5; 5 * 512], true);
    {
        let mut p = Partition::new(&mut disk, 1, 3).unwrap();
        p.seek(SeekFrom::Start(509)).unwrap();
        p.write_all(&[0x31; 520]).unwrap();
        p.seek(SeekFrom::Current(-520)).unwrap();
        let mut data = [0; 520];
        p.read_exact(&mut data).unwrap();
        assert_eq!(data, [0x31; 520]);
        assert_eq!(p.seek(SeekFrom::End(0)).unwrap(), 1536);
        assert_eq!(p.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(p.write(&[1]).unwrap(), 0);
        assert!(p.seek(SeekFrom::End(1)).is_err());
        assert!(p.seek(SeekFrom::Start(u64::MAX)).is_err());
        assert!(p.seek(SeekFrom::Current(i64::MIN)).is_err());
        assert_eq!(p.stream_position().unwrap(), 1536);
    }
    let mut bytes = vec![0; 5 * 512];
    disk.read(0, &mut bytes).unwrap();
    assert!(bytes[..512 + 509].iter().all(|&b| b == 0xa5));
    assert!(bytes[512 + 509..512 + 1029].iter().all(|&b| b == 0x31));
    assert!(bytes[512 + 1029..].iter().all(|&b| b == 0xa5));
    assert!(Partition::new(&mut disk, 4, 2).is_err());
    let mut readonly = Disk::memory(bytes, false);
    let mut p = Partition::new(&mut readonly, 1, 3).unwrap();
    assert_eq!(
        p.write(&[0]).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

fn partition_entry(bytes: &mut [u8], slot: usize, active: bool, kind: u8, start: u32, size: u32) {
    let at = 446 + slot * 16;
    bytes[at] = if active { 0x80 } else { 0 };
    bytes[at + 4] = kind;
    put32(bytes, at + 8, start);
    put32(bytes, at + 12, size);
    bytes[510..].copy_from_slice(&[0x55, 0xaa]);
}

fn partitioned() -> Disk {
    let mut disk = Disk::memory(vec![0; 100 * 512], true);
    let mut mbr = [0; 512];
    partition_entry(&mut mbr, 0, false, 0x06, 4, 2);
    partition_entry(&mut mbr, 1, false, 0x07, 8, 2);
    partition_entry(&mut mbr, 2, true, 0x0e, 12, 2);
    partition_entry(&mut mbr, 3, false, 0x0f, 20, 50);
    disk.write(0, &mbr).unwrap();
    let mut ebr = [0; 512];
    partition_entry(&mut ebr, 0, false, 0x06, 1, 2);
    partition_entry(&mut ebr, 1, false, 0x0f, 10, 40);
    disk.write(20, &ebr).unwrap();
    let mut ebr = [0; 512];
    partition_entry(&mut ebr, 0, false, 0x83, 1, 2);
    disk.write(30, &ebr).unwrap();
    disk
}

#[test]
fn discovery_orders_primaries_and_relative_logicals_and_marks_non_fat() {
    let mut disk = partitioned();
    let v = fat::volumes(&mut disk);
    assert_eq!(
        v.iter()
            .map(|v| (v.letter, v.start_lba, v.fat))
            .collect::<Vec<_>>(),
        [
            ('C', 12, true),
            ('D', 4, true),
            ('E', 8, false),
            ('F', 21, true),
            ('G', 31, false)
        ]
    );
    assert_eq!(
        v.iter().map(|v| v.partition_type).collect::<Vec<_>>(),
        [0x0e, 6, 7, 6, 0x83]
    );
    assert!(
        fat::mount(&mut disk, &v[2])
            .err()
            .unwrap()
            .contains("not FAT")
    );
    // Discovery only reads the partition table; mounting rechecks the actual BPB.
    assert!(fat::mount(&mut disk, &v[0]).is_err());
}

#[test]
fn fat12_in_fat16_partition_uses_cluster_count() {
    const START: u32 = 63;
    const SECTORS: u32 = 4096;
    let mut disk = Disk::memory(vec![0; (START + SECTORS) as usize * 512], true);
    let mut mbr = [0; 512];
    partition_entry(&mut mbr, 0, true, 0x0e, START, SECTORS);
    disk.write(0, &mbr).unwrap();
    fatfs::format_volume(
        Partition::new(&mut disk, u64::from(START), u64::from(SECTORS)).unwrap(),
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat12)
            .bytes_per_sector(512)
            .bytes_per_cluster(512),
    )
    .unwrap();
    let volumes = fat::volumes(&mut disk);
    assert_eq!(volumes.len(), 1);
    let volume = &volumes[0];
    assert_eq!(volume.letter, 'C');
    assert!(volume.fat);
    assert_eq!(volume.partition_type, 0x0e);

    let payload = [0x5a; 1537]; // Multiple FAT12 entries, including odd/even packed pairs.
    let fs = fat::mount(&mut disk, volume).unwrap();
    assert_eq!(fs.fat_type(), FatType::Fat12);
    fs.root_dir()
        .create_file("TEST.BIN")
        .unwrap()
        .write_all(&payload)
        .unwrap();
    fs.unmount().unwrap();

    let fs = fat::mount(&mut disk, volume).unwrap();
    let mut contents = Vec::new();
    fs.root_dir()
        .open_file("TEST.BIN")
        .unwrap()
        .read_to_end(&mut contents)
        .unwrap();
    assert_eq!(contents, payload);
    fs.root_dir().remove("TEST.BIN").unwrap();
    fs.unmount().unwrap();
    let fs = fat::mount(&mut disk, volume).unwrap();
    assert!(fs.root_dir().open_file("TEST.BIN").is_err());
    fs.unmount().unwrap();
}

#[test]
fn malformed_partitions_and_cyclic_ebrs_are_not_exposed() {
    let mut disk = partitioned();
    let mut ebr = sector(&mut disk, 30);
    partition_entry(&mut ebr, 1, false, 0x0f, 10, 40); // self link relative to base 20
    disk.write(30, &ebr).unwrap();
    assert_eq!(fat::volumes(&mut disk).len(), 3);
    let mut mbr = sector(&mut disk, 0);
    partition_entry(&mut mbr, 0, false, 6, 98, 3); // beyond disk
    partition_entry(&mut mbr, 1, false, 7, 12, 2); // overlaps active primary
    disk.write(0, &mbr).unwrap();
    assert!(fat::volumes(&mut disk).is_empty());
    mbr[510..].fill(0);
    disk.write(0, &mbr).unwrap();
    assert!(fat::volumes(&mut disk).is_empty());
}

#[test]
fn mount_rejects_bad_bpb_arithmetic_type_and_capacity() {
    let (mut disk, volume) = formatted(FatType::Fat16);
    let original = sector(&mut disk, 0);
    let corruptions: &[(usize, &[u8])] = &[
        (11, &[0, 0]),
        (13, &[0]),
        (13, &[3]),
        (14, &[0, 0]),
        (16, &[0]),
        (16, &[255]),
        (22, &[0xff, 0xff]),
        (510, &[0, 0]),
    ];
    for &(at, data) in corruptions {
        let mut bad = original;
        bad[at..at + data.len()].copy_from_slice(data);
        disk.write(0, &bad).unwrap();
        assert!(
            fat::mount(&mut disk, &volume).is_err(),
            "accepted corrupt BPB at {at}"
        );
    }
    disk.write(0, &original).unwrap();
    let too_short = Volume {
        sectors: 1,
        ..volume
    };
    assert!(
        fat::mount(&mut disk, &too_short)
            .err()
            .unwrap()
            .contains("capacity")
    );
    let outside = Volume {
        start_lba: u64::MAX,
        ..volume
    };
    assert!(fat::mount(&mut disk, &outside).is_err());
    fat::mount(&mut disk, &volume).unwrap().unmount().unwrap();
    // LBA filesystem access does not depend on current CHS geometry fields.
    let mut no_chs = original;
    no_chs[24..28].fill(0);
    disk.write(0, &no_chs).unwrap();
    fat::mount(&mut disk, &volume).unwrap().unmount().unwrap();

    let (mut disk, volume) = formatted(FatType::Fat32);
    let original = sector(&mut disk, 0);
    for (at, data) in [(36, 0xffff_ffffu32), (44, 1), (48, 0xffff)] {
        let mut bad = original;
        put32(&mut bad, at, data);
        disk.write(0, &bad).unwrap();
        assert!(fat::mount(&mut disk, &volume).is_err());
    }
    let mut bad = original;
    put16(&mut bad, 40, 0x82); // nonmirrored FAT index 2, but only two FATs
    disk.write(0, &bad).unwrap();
    assert!(fat::mount(&mut disk, &volume).is_err());
    disk.write(0, &original).unwrap();
    let mut info = sector(&mut disk, 1);
    put32(&mut info, 488, 0xffff_fffe);
    put32(&mut info, 492, 0xffff_fffe);
    disk.write(1, &info).unwrap();
    // FSInfo counts are hints; the library discards stale values on mount.
    fat::mount(&mut disk, &volume).unwrap().unmount().unwrap();
}

#[test]
fn vendored_mount_rejects_overflow_before_using_metadata() {
    let (mut disk, _) = formatted(FatType::Fat32);
    let boot = sector(&mut disk, 0);
    let info = sector(&mut disk, 1);
    for fat_sectors in [u32::MAX, 0x10_0000] {
        let mut bytes = vec![0; 1024];
        bytes[..512].copy_from_slice(&boot);
        bytes[512..].copy_from_slice(&info);
        put32(&mut bytes, 32, 0x1000_0000);
        put32(&mut bytes, 36, fat_sectors);
        assert!(
            fatfs::FileSystem::new(std::io::Cursor::new(bytes), fat::deterministic_options())
                .is_err()
        );
    }
}

#[test]
fn extreme_file_seeks_return_normally() {
    let (mut disk, volume) = formatted(FatType::Fat16);
    let fs = fat::mount(&mut disk, &volume).unwrap();
    let root = fs.root_dir();
    let mut file = root.create_file("FILE.BIN").unwrap();
    file.write_all(&[0; 100]).unwrap();
    assert_eq!(file.seek(SeekFrom::Current(i64::MAX)).unwrap(), 100);
    assert!(file.seek(SeekFrom::Current(i64::MIN)).is_err());
    assert_eq!(file.stream_position().unwrap(), 100);
}

#[test]
fn fat12_near_type_boundary_excludes_reserved_cluster_numbers() {
    let sectors = 4141;
    let mut disk = Disk::memory(vec![0; sectors as usize * 512], true);
    fatfs::format_volume(
        Partition::new(&mut disk, 0, sectors).unwrap(),
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat12)
            .bytes_per_sector(512)
            .bytes_per_cluster(512)
            .max_root_dir_entries(512),
    )
    .unwrap();
    let boot = sector(&mut disk, 0);
    let physical = sectors
        - u64::from(word(&boot, 14))
        - u64::from(boot[16]) * u64::from(word(&boot, 22))
        - (u64::from(word(&boot, 17)) * 32).div_ceil(512);
    assert_eq!(physical, 4084);
    let volume = Volume {
        letter: 'C',
        fat: true,
        partition_type: 0,
        start_lba: 0,
        sectors,
    };
    let fs = fat::mount(&mut disk, &volume).unwrap();
    assert_eq!(fs.stats().unwrap().total_clusters(), 4078);
    assert_eq!(fs.stats().unwrap().free_clusters(), 4078);
    {
        let root = fs.root_dir();
        let mut file = root.create_file("FILL.BIN").unwrap();
        let expected = vec![0x67; 4078 * 512];
        file.write_all(&expected).unwrap();
        assert!(file.write(&[1]).is_err());
        assert_eq!(fs.stats().unwrap().free_clusters(), 0);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        file.flush().unwrap();
    }
    fs.unmount().unwrap();
    // Reserved-number tail entries are marked bad, not offered as free data.
    let mut table = vec![0; usize::from(word(&boot, 22)) * 512];
    disk.read(u64::from(word(&boot, 14)), &mut table).unwrap();
    for cluster in 0xff0..physical as usize + 2 {
        let packed = word(&table, cluster + cluster / 2);
        let value = if cluster & 1 == 0 {
            packed & 0xfff
        } else {
            packed >> 4
        };
        assert_eq!(value, 0xff7);
    }
}

#[test]
fn default_mount_uses_current_local_timestamps() {
    let (mut disk, volume) = formatted(FatType::Fat16);
    let fs = fat::mount_with_options(&mut disk, &volume, fat::deterministic_options()).unwrap();
    {
        let mut file = fs.root_dir().create_file("OLD.TXT").unwrap();
        file.write_all(b"fixed creation time").unwrap();
        file.flush().unwrap();
    }
    fs.unmount().unwrap();

    let before = chrono::Local::now().naive_local();
    let fs = fat::mount(&mut disk, &volume).unwrap();
    {
        let root = fs.root_dir();
        let dir = root.create_dir("Current local time").unwrap();
        let mut file = dir.create_file("New local timestamp.txt").unwrap();
        file.write_all(b"local creation and modification time")
            .unwrap();
        file.flush().unwrap();
        let mut old = root.open_file("OLD.TXT").unwrap();
        old.write_all(b"local modification time").unwrap();
        old.flush().unwrap();
    }
    fs.unmount().unwrap();
    let after = chrono::Local::now().naive_local();
    // FAT modification times round down to even seconds. Read the actual local
    // clock on both sides without changing the process-wide timezone.
    let lower = before.min(after) - chrono::Duration::seconds(2);
    let upper = before.max(after) + chrono::Duration::seconds(2);
    let assert_current = |value: fatfs::DateTime| {
        let time = chrono::NaiveDate::from_ymd_opt(
            i32::from(value.date.year),
            u32::from(value.date.month),
            u32::from(value.date.day),
        )
        .unwrap()
        .and_hms_milli_opt(
            u32::from(value.time.hour),
            u32::from(value.time.min),
            u32::from(value.time.sec),
            u32::from(value.time.millis),
        )
        .unwrap();
        assert!(
            (lower..=upper).contains(&time),
            "{time} outside {lower}..={upper}"
        );
    };
    let fs = fat::mount(&mut disk, &volume).unwrap();
    {
        let root = fs.root_dir();
        let entries = root.iter().map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            if entry.is_dir() {
                assert_current(entry.created());
                assert_current(entry.modified());
                let file = entry
                    .to_dir()
                    .iter()
                    .map(Result::unwrap)
                    .find(|entry| !entry.is_dir())
                    .unwrap();
                assert_current(file.created());
                assert_current(file.modified());
            } else {
                assert_eq!(
                    entry.created().date,
                    fatfs::Date {
                        year: 2026,
                        month: 1,
                        day: 1
                    }
                );
                assert_current(entry.modified());
            }
        }
    }
    fs.unmount().unwrap();
}

#[test]
fn long_names_roundtrip_with_dirty_flags_and_fixed_dates() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let (mut disk, volume) = formatted(kind);
        if kind == FatType::Fat32 {
            assert_eq!(sector(&mut disk, 1), sector(&mut disk, 7));
        }
        let mut boot = sector(&mut disk, 0);
        let flags_at = if kind == FatType::Fat32 { 65 } else { 37 };
        boot[flags_at] |= 1;
        disk.write(0, &boot).unwrap();
        if kind != FatType::Fat12 {
            let fat_start = u64::from(word(&boot, 14));
            let fat_size = if kind == FatType::Fat32 {
                u64::from(u32::from_le_bytes(boot[36..40].try_into().unwrap()))
            } else {
                u64::from(word(&boot, 22))
            };
            for copy in 0..2 {
                let lba = fat_start + copy * fat_size;
                let mut table = sector(&mut disk, lba);
                if kind == FatType::Fat32 {
                    table[7] &= !0x08;
                } else {
                    table[3] &= !0x80;
                }
                disk.write(lba, &table).unwrap();
            }
        }
        let fs = fat::mount_with_options(&mut disk, &volume, fat::deterministic_options()).unwrap();
        assert_eq!(fs.fat_type(), kind);
        {
            let root = fs.root_dir();
            let dir = root.create_dir("A long directory name").unwrap();
            let mut file = dir.create_file("A long filename.txt").unwrap();
            file.write_all(b"persisted contents\r\n").unwrap();
            file.flush().unwrap();
        }
        fs.unmount().unwrap();
        disk.flush().unwrap();
        if kind == FatType::Fat32 {
            assert_eq!(sector(&mut disk, 1), sector(&mut disk, 7));
        }
        assert_eq!(sector(&mut disk, 0)[flags_at] & 1, 1, "mount is not fsck");
        // Inspect the ordinary LFN mkdir's physical dot entries independently.
        let reserved = u64::from(word(&boot, 14));
        let fat_size = if kind == FatType::Fat32 {
            u64::from(u32::from_le_bytes(boot[36..40].try_into().unwrap()))
        } else {
            u64::from(word(&boot, 22))
        };
        let fixed_root = (u64::from(word(&boot, 17)) * 32).div_ceil(512);
        let data_start = reserved + u64::from(boot[16]) * fat_size + fixed_root;
        let root_sector = if kind == FatType::Fat32 {
            let first = u32::from_le_bytes(boot[44..48].try_into().unwrap());
            data_start + u64::from(first - 2) * u64::from(boot[13])
        } else {
            reserved + u64::from(boot[16]) * fat_size
        };
        let root_bytes = sector(&mut disk, root_sector);
        let entry = root_bytes
            .chunks_exact(32)
            .find(|e| e[11] & 0x10 != 0)
            .unwrap();
        let first = u32::from(word(entry, 26)) | (u32::from(word(entry, 20)) << 16);
        let dots = sector(
            &mut disk,
            data_start + u64::from(first - 2) * u64::from(boot[13]),
        );
        assert_eq!(&dots[..11], b".          ");
        assert_eq!(dots[11], 0x10);
        assert_eq!(&dots[32..43], b"..         ");
        assert_eq!(dots[43], 0x10);
        assert_eq!(word(&dots, 32 + 20), 0);
        assert_eq!(word(&dots, 32 + 26), 0);
        if kind != FatType::Fat12 {
            let table = sector(&mut disk, u64::from(word(&boot, 14)));
            assert_eq!(
                if kind == FatType::Fat32 {
                    table[7] & 8
                } else {
                    table[3] & 0x80
                },
                0
            );
        }
        let discovered = fat::volumes(&mut disk);
        assert_eq!(discovered, [volume]);
        let fs = fat::mount_with_options(&mut disk, &discovered[0], fat::deterministic_options())
            .unwrap();
        {
            let root = fs.root_dir();
            let entry = root.iter().next().unwrap().unwrap();
            assert!(entry.is_dir());
            assert_eq!(entry.file_name(), "A long directory name");
            assert_eq!(
                entry.created().date,
                fatfs::Date {
                    year: 2026,
                    month: 1,
                    day: 1
                }
            );
            let mut file = root
                .open_file("A long directory name/A long filename.txt")
                .unwrap();
            let mut contents = String::new();
            file.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "persisted contents\r\n");
        }
        fs.unmount().unwrap();
    }
}

fn broken_file() -> (Disk, Volume, u64, usize, u16) {
    let (mut disk, volume) = formatted(FatType::Fat16);
    let fs = fat::mount(&mut disk, &volume).unwrap();
    {
        let root = fs.root_dir();
        let mut file = root.create_file("BROKEN.BIN").unwrap();
        file.write_all(&[0x42; 100]).unwrap();
        file.flush().unwrap();
    }
    fs.unmount().unwrap();
    let boot = sector(&mut disk, 0);
    let fat = u64::from(word(&boot, 14));
    let root = fat + u64::from(boot[16]) * u64::from(word(&boot, 22));
    let directory = sector(&mut disk, root);
    let at = directory
        .chunks_exact(32)
        .position(|e| &e[..11] == b"BROKEN  BIN")
        .unwrap()
        * 32;
    let first = word(&directory, at + 26);
    (disk, volume, root, at, first)
}

#[test]
fn bad_cluster_numbers_return_errors_on_read_and_write() {
    for first in [0, 1] {
        let (mut disk, volume, root, at, _) = broken_file();
        let mut directory = sector(&mut disk, root);
        put16(&mut directory, at + 26, first); // absent or reserved, not file data
        disk.write(root, &directory).unwrap();
        let fs = fat::mount(&mut disk, &volume).unwrap();
        let root = fs.root_dir();
        let mut file = root.open_file("BROKEN.BIN").unwrap();
        assert!(file.read(&mut [0; 1]).is_err());
        assert!(file.write(&[1]).is_err());
    }
}

#[test]
fn invalid_chain_errors_propagate_from_remove_and_truncate() {
    for remove in [false, true] {
        let (mut disk, volume, _, _, first) = broken_file();
        let boot = sector(&mut disk, 0);
        let fat = u64::from(word(&boot, 14));
        let mut table = sector(&mut disk, fat);
        put16(&mut table, usize::from(first) * 2, 0xefff);
        disk.write(fat, &table).unwrap();
        let fs = fat::mount(&mut disk, &volume).unwrap();
        let root = fs.root_dir();
        if remove {
            assert!(root.remove("BROKEN.BIN").is_err());
        } else {
            let mut file = root.open_file("BROKEN.BIN").unwrap();
            file.seek(SeekFrom::Start(1)).unwrap();
            assert!(file.truncate().is_err());
        }
    }
}
