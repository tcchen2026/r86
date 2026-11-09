//! Host FAT access through the emulator's sector storage. Partition offsets use
//! 512-byte disk sectors; BPB logical sectors may be larger. Mount validates BPB
//! geometry and partition bounds before constructing the filesystem. Vendored
//! fatfs also checks BPB arithmetic; issues 56/72 informed this review.
//! These checks do not establish allocation-chain or directory consistency.

use crate::disk::{Disk, DiskError, SECTOR_SIZE};
use fatfs::{Date, DateTime, FatType, FileSystem, FsOptions, Time, TimeProvider};
use std::collections::BTreeSet;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// A data partition. Letters identify entries on this disk, starting at C.
/// With mixed filesystem types, a guest's own letter assignment may differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Volume {
    pub letter: char,
    pub fat: bool,
    /// MBR type byte, or zero for a filesystem without a partition table.
    pub partition_type: u8,
    pub start_lba: u64,
    pub sectors: u64,
}

/// Bounded byte access to a disk partition, without an additional write cache.
/// `Write::flush` has no buffered work. After unmounting fatfs, call
/// `Disk::flush` explicitly when durable storage is required.
pub struct Partition<'a> {
    disk: &'a mut Disk,
    start_lba: u64,
    length: u64,
    position: u64,
    /// The last sector a partial access touched, as on disk. FAT and directory
    /// entries are read a few bytes at a time, so walking a cluster chain reads
    /// each sector many times; writes go through and keep this current.
    cached: Option<(u64, [u8; SECTOR_SIZE])>,
}

fn io_error(error: DiskError) -> io::Error {
    match error {
        DiskError::Io(error) => error,
        DiskError::ReadOnly => io::Error::new(io::ErrorKind::PermissionDenied, error),
        _ => io::Error::other(error),
    }
}

impl<'a> Partition<'a> {
    pub fn new(disk: &'a mut Disk, start_lba: u64, sectors: u64) -> io::Result<Self> {
        if sectors == 0 || start_lba > disk.sectors() || sectors > disk.sectors() - start_lba {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "partition exceeds disk",
            ));
        }
        let length = sectors.checked_mul(SECTOR_SIZE as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "partition byte length overflows",
            )
        })?;
        Ok(Self {
            disk,
            start_lba,
            length,
            position: 0,
            cached: None,
        })
    }

    fn available(&self, requested: usize) -> usize {
        (self.length - self.position).min(requested as u64) as usize
    }

    /// One sector's bytes, from the cache or the disk.
    fn sector(&mut self, sector: u64) -> io::Result<[u8; SECTOR_SIZE]> {
        if let Some((cached, bytes)) = self.cached
            && cached == sector
        {
            return Ok(bytes);
        }
        let mut bytes = [0; SECTOR_SIZE];
        self.disk.read(sector, &mut bytes).map_err(io_error)?;
        self.cached = Some((sector, bytes));
        Ok(bytes)
    }
}

impl Read for Partition<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.available(buf.len());
        if count == 0 {
            return Ok(0);
        }
        let sector = self.start_lba + self.position / SECTOR_SIZE as u64;
        let offset = self.position as usize % SECTOR_SIZE;
        let count = if offset == 0 && count >= SECTOR_SIZE {
            let count = count / SECTOR_SIZE * SECTOR_SIZE;
            self.disk
                .read(sector, &mut buf[..count])
                .map_err(io_error)?;
            count
        } else {
            let count = count.min(SECTOR_SIZE - offset);
            let bytes = self.sector(sector)?;
            buf[..count].copy_from_slice(&bytes[offset..offset + count]);
            count
        };
        self.position += count as u64;
        Ok(count)
    }
}

impl Write for Partition<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let count = self.available(buf.len());
        if count == 0 {
            return Ok(0);
        }
        let sector = self.start_lba + self.position / SECTOR_SIZE as u64;
        let offset = self.position as usize % SECTOR_SIZE;
        let count = if offset == 0 && count >= SECTOR_SIZE {
            let count = count / SECTOR_SIZE * SECTOR_SIZE;
            if self.cached.is_some_and(|(cached, _)| {
                (sector..sector + (count / SECTOR_SIZE) as u64).contains(&cached)
            }) {
                self.cached = None;
            }
            self.disk.write(sector, &buf[..count]).map_err(io_error)?;
            count
        } else {
            let count = count.min(SECTOR_SIZE - offset);
            let mut bytes = self.sector(sector)?;
            bytes[offset..offset + count].copy_from_slice(&buf[..count]);
            // Unknown contents if the write fails part way.
            self.cached = None;
            self.disk.write(sector, &bytes).map_err(io_error)?;
            self.cached = Some((sector, bytes));
            count
        };
        self.position += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Partition<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.length) + i128::from(n),
        };
        if position < 0 || position > i128::from(self.length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside partition",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

#[derive(Debug)]
struct FixedTime;

impl TimeProvider for FixedTime {
    fn get_current_date(&self) -> Date {
        Date {
            year: 2026,
            month: 1,
            day: 1,
        }
    }

    fn get_current_date_time(&self) -> DateTime {
        DateTime {
            date: self.get_current_date(),
            time: Time {
                hour: 0,
                min: 0,
                sec: 0,
                millis: 0,
            },
        }
    }
}

/// Fixed 2026-01-01 timestamps for reproducible disk planting.
pub fn deterministic_options() -> FsOptions {
    FsOptions::new().time_provider(&FixedTime)
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn dword(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
}

fn read_sector(disk: &mut Disk, sector: u64) -> Result<[u8; 512], String> {
    let mut bytes = [0; 512];
    disk.read(sector, &mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn validate(disk: &mut Disk, start: u64, sectors: u64) -> Result<FatType, String> {
    if sectors == 0 || start > disk.sectors() || sectors > disk.sectors() - start {
        return Err("FAT partition exceeds disk".into());
    }
    let boot = read_sector(disk, start)?;
    if boot[510..] != [0x55, 0xaa] {
        return Err("FAT boot signature is missing".into());
    }
    let bps = word(&boot, 11);
    let spc = u64::from(boot[13]);
    if !(512..=4096).contains(&bps)
        || !bps.is_power_of_two()
        || !(1..=128).contains(&spc)
        || !spc.is_power_of_two()
    {
        return Err("invalid FAT sector or cluster size".into());
    }
    let reserved = word(&boot, 14);
    let fats = u64::from(boot[16]);
    let roots = word(&boot, 17);
    let total16 = word(&boot, 19);
    let total32 = dword(&boot, 32);
    if reserved == 0 || !(1..=2).contains(&fats) || (total16 == 0) == (total32 == 0) {
        return Err("invalid FAT reserved, table or sector count".into());
    }
    let total = total16 + total32;
    if u128::from(total) * u128::from(bps) > u128::from(sectors) * 512 {
        return Err("FAT volume exceeds partition capacity".into());
    }
    let fat16 = word(&boot, 22);
    let fat32 = fat16 == 0;
    let fat_sectors = if fat32 { dword(&boot, 36) } else { fat16 };
    let root_sectors = (roots * 32).div_ceil(bps);
    let data_start = reserved + fats * fat_sectors + root_sectors;
    // These expressions are u32 operations inside fatfs 0.3.6. Validate them
    // in u64 before mounting, including its multiplication before division.
    let fat_bits = fat_sectors * bps * 8;
    if fat_sectors == 0
        || data_start >= total
        || data_start > u64::from(u32::MAX)
        || fat_bits > u64::from(u32::MAX)
    {
        return Err("invalid FAT metadata size or arithmetic overflow".into());
    }
    let clusters = (total - data_start) / spc;
    let (kind, bits) = if clusters < 4085 {
        (FatType::Fat12, 12)
    } else if clusters < 65525 {
        (FatType::Fat16, 16)
    } else {
        (FatType::Fat32, 32)
    };
    if clusters == 0
        || clusters > 0x0fff_fff4
        || fat32 != (kind == FatType::Fat32)
        || fat_bits / bits < clusters + 2
    {
        return Err("invalid FAT type, cluster count or table capacity".into());
    }
    if fat32 {
        let flags = word(&boot, 40);
        let root = dword(&boot, 44);
        let info = word(&boot, 48);
        let backup = word(&boot, 50);
        if roots != 0
            || total16 != 0
            || word(&boot, 42) != 0
            || !(2..clusters + 2).contains(&root)
            || (flags & 0x80 != 0 && flags & 0xf >= fats)
            || info == 0
            || info >= reserved
            || backup >= reserved
        {
            return Err("invalid FAT32 root, active table or reserved metadata".into());
        }
        let info = read_sector(disk, start + info * (bps / 512))?;
        if dword(&info, 0) != 0x4161_5252
            || dword(&info, 484) != 0x6141_7272
            || dword(&info, 508) != 0xaa55_0000
        {
            return Err("invalid FAT32 FSInfo".into());
        }
        // Free-count and next-cluster hints may be stale after guest writes.
        // fatfs validates/discards them; they do not define the volume layout.
    } else if roots == 0 {
        return Err("FAT12/16 has no root directory".into());
    }
    Ok(kind)
}

/// Mount a validated FAT volume using the host's current local time for writes.
/// Unmount, then flush `Disk` for durability.
pub fn mount<'a>(disk: &'a mut Disk, volume: &Volume) -> Result<FileSystem<Partition<'a>>, String> {
    mount_with_options(disk, volume, FsOptions::new())
}

/// Mount with explicit FAT options, including a caller-selected time provider.
/// Partition and BPB validation is identical to `mount`.
pub fn mount_with_options<'a>(
    disk: &'a mut Disk,
    volume: &Volume,
    options: FsOptions,
) -> Result<FileSystem<Partition<'a>>, String> {
    if !volume.fat {
        return Err("selected partition is not FAT".into());
    }
    // The BPB cluster count determines FAT12/16/32; the MBR only identifies FAT.
    validate(disk, volume.start_lba, volume.sectors)?;
    let partition =
        Partition::new(disk, volume.start_lba, volume.sectors).map_err(|e| e.to_string())?;
    FileSystem::new(partition, options).map_err(|e| format!("mounting FAT volume: {e}"))
}

#[derive(Clone, Copy)]
struct Part {
    start: u64,
    sectors: u64,
    kind: u8,
    active: bool,
}

fn part(bytes: &[u8], slot: usize) -> Option<Part> {
    let p = &bytes[446 + slot * 16..462 + slot * 16];
    if !matches!(p[0], 0 | 0x80) || p[4] == 0 || dword(p, 12) == 0 {
        return None;
    }
    Some(Part {
        start: dword(p, 8),
        sectors: dword(p, 12),
        kind: p[4],
        active: p[0] == 0x80,
    })
}

fn extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
}

fn is_fat(kind: u8) -> bool {
    matches!(
        kind,
        0x01 | 0x04 | 0x06 | 0x0b | 0x0c | 0x0e | 0x11 | 0x14 | 0x16 | 0x1b | 0x1c | 0x1e
    )
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn volume(p: Part) -> Volume {
    Volume {
        letter: 'C',
        fat: is_fat(p.kind),
        partition_type: p.kind,
        start_lba: p.start,
        sectors: p.sectors,
    }
}

fn logicals(disk: &mut Disk, container: Part) -> Result<Vec<Volume>, String> {
    let end = container.start + container.sectors;
    let mut cursor = container.start;
    let mut visited = BTreeSet::new();
    let mut ranges = Vec::new();
    let mut found = Vec::new();
    loop {
        // DOS has 24 hard-disk letters. Bound malformed chains as well as cycles.
        if visited.len() == 128
            || !visited.insert(cursor)
            || cursor < container.start
            || cursor >= end
            || ranges.iter().any(|&r| overlaps(r, (cursor, cursor + 1)))
        {
            return Err("invalid or cyclic extended partition chain".into());
        }
        ranges.push((cursor, cursor + 1));
        let ebr = read_sector(disk, cursor)?;
        if ebr[510..] != [0x55, 0xaa] || ebr[478..510].iter().any(|&b| b != 0) {
            return Err("invalid extended partition record".into());
        }
        if let Some(mut p) = part(&ebr, 0) {
            p.start += cursor;
            let range = (p.start, p.start + p.sectors);
            if extended(p.kind)
                || p.start <= cursor
                || range.1 > end
                || ranges.iter().any(|&r| overlaps(r, range))
            {
                return Err("logical partition exceeds or overlaps its container".into());
            }
            ranges.push(range);
            found.push(volume(p));
        } else if ebr[446..462].iter().any(|&b| b != 0) {
            return Err("malformed logical partition entry".into());
        }
        match part(&ebr, 1) {
            Some(p)
                if extended(p.kind) && p.start != 0 && p.start + p.sectors <= container.sectors =>
            {
                cursor = container.start + p.start;
            }
            None if ebr[462..478].iter().all(|&b| b == 0) => return Ok(found),
            _ => return Err("invalid extended partition link".into()),
        }
    }
}

/// Data partitions in primary/logical order: active primary, other primaries,
/// then extended logicals. Non-FAT entries remain visible with `fat == false`.
/// Letters are identifiers; mixed-type guest assignments can differ. An unreadable or
/// malformed entry is omitted; a broken extended chain exposes none of its
/// logicals. Up to 128 EBRs are examined and at most C through Z are returned.
/// A valid FAT filesystem without a partition table is exposed as C.
pub fn volumes(disk: &mut Disk) -> Vec<Volume> {
    let sectors = disk.sectors();
    if validate(disk, 0, sectors).is_ok() {
        return vec![Volume {
            letter: 'C',
            fat: true,
            partition_type: 0,
            start_lba: 0,
            sectors,
        }];
    }
    let Ok(mbr) = read_sector(disk, 0) else {
        return Vec::new();
    };
    if mbr[510..] != [0x55, 0xaa] {
        return Vec::new();
    }
    let parts: Vec<_> = (0..4).filter_map(|slot| part(&mbr, slot)).collect();
    let valid: Vec<_> = parts
        .iter()
        .enumerate()
        .filter_map(|(i, &p)| {
            let range = (p.start, p.start + p.sectors);
            if p.start == 0
                || range.1 > disk.sectors()
                || parts
                    .iter()
                    .enumerate()
                    .any(|(j, q)| i != j && overlaps(range, (q.start, q.start + q.sectors)))
            {
                None
            } else {
                Some(p)
            }
        })
        .collect();
    let mut found = Vec::new();
    for active in [true, false] {
        for &p in valid
            .iter()
            .filter(|p| p.active == active && !extended(p.kind))
        {
            found.push(volume(p));
        }
    }
    for p in valid.into_iter().filter(|p| extended(p.kind)) {
        if let Ok(logicals) = logicals(disk, p) {
            found.extend(logicals);
        }
    }
    found.truncate(24);
    for (index, v) in found.iter_mut().enumerate() {
        v.letter = (b'C' + index as u8) as char;
    }
    found
}
