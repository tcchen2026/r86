//! Dynamic VHD v1, independently implemented from Microsoft's 2006-10-18
//! Virtual Hard Disk Image Format specification. All metadata is big endian.
//!
//! Ownership: one locked File, one BAT, one cached block bitmap. A clear bitmap
//! bit reads as zero; newly allocated payloads are also physically zero-valued
//! as required by the format (including readers which ignore the bitmap).
//!
//! Allocation appends after the old footer, retaining it as harmless padding.
//! A new zero-filled block and trailing footer are synced before publishing its
//! BAT entry. Sector writes put data before the bitmap bit; FLUSH syncs both.
//! This avoids exposing uninitialized blocks or old footer bytes as sectors.
//! VHD has no journal: arbitrary power loss can still tear metadata or
//! sector overwrites. On a write/sync error, stop using this handle; continuing
//! with possibly stale cached metadata would conceal a storage failure.
use super::{MAX_SECTORS, SECTOR_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: u32 = 2 * 1024 * 1024;
const FREE: u32 = u32::MAX;
const VERSION: u32 = 0x0001_0000;

#[derive(Debug)]
pub(super) struct FileDisk {
    file: File,
    vhd: Option<Vhd>,
    failed: Option<String>,
}

impl Drop for FileDisk {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this descriptor before exec.
        // Release our lock explicitly instead of waiting for its last copy to
        // close. This is lock cleanup only; callers must explicitly flush I/O.
        let _ = self.file.unlock();
    }
}

#[derive(Debug)]
struct Vhd {
    footer: [u8; 512],
    table_offset: u64,
    bat: Vec<u32>,
    block_sectors: u64,
    bitmap: Vec<u8>,
    bitmap_block: Option<usize>,
    file_len: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn be32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn be64(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn put32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn checksum(data: &[u8], offset: usize) -> u32 {
    !data.iter().enumerate().fold(0u32, |sum, (i, &byte)| {
        sum + if (offset..offset + 4).contains(&i) {
            0
        } else {
            u32::from(byte)
        }
    })
}

fn read_at(file: &mut File, offset: u64, data: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(data)
}

fn write_at(file: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

fn capacity(size: u64) -> io::Result<u64> {
    if size == 0 || size % 512 != 0 || size / 512 > MAX_SECTORS {
        return Err(invalid(
            "disk capacity must be 512-byte aligned and between 512 bytes and 128 GiB (LBA28)",
        ));
    }
    Ok(size / 512)
}

fn valid_footer(footer: &[u8; 512]) -> bool {
    &footer[..8] == b"conectix" && checksum(footer, 64) == be32(footer, 64)
}

impl FileDisk {
    pub(super) fn open(path: &Path, writable: bool) -> io::Result<(Self, u64)> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        if writable {
            file.try_lock()
        } else {
            file.try_lock_shared()
        }
        .map_err(|error| {
            io::Error::other(format!("cannot lock disk {}: {error}", path.display()))
        })?;
        let mut disk = Self {
            file,
            vhd: None,
            failed: None,
        };
        let file = &mut disk.file;
        let len = file.metadata()?.len();
        if len < 512 || len % 512 != 0 {
            return Err(invalid("disk file is truncated or not sector aligned"));
        }
        let mut first = [0; 512];
        let mut last = [0; 512];
        read_at(file, 0, &mut first)?;
        read_at(file, len - 512, &mut last)?;
        let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        if &first[..8] == b"vhdxfile" || extension.eq_ignore_ascii_case("vhdx") {
            return Err(invalid("VHDX is not supported; use a dynamic VHD"));
        }
        let is_vhd = &first[..8] == b"conectix"
            || &last[..8] == b"conectix"
            || extension.eq_ignore_ascii_case("vhd");
        let (vhd, sectors) = if is_vhd {
            // The spec explicitly permits recovery from the leading mirror
            // when the trailing footer fails its checksum.
            let footer = if valid_footer(&last) {
                last
            } else if valid_footer(&first) {
                first
            } else {
                return Err(invalid("VHD has no valid footer (cookie/checksum)"));
            };
            let vhd = Vhd::open(file, footer, len)?;
            let sectors = capacity(be64(&vhd.footer, 48))?;
            (Some(vhd), sectors)
        } else {
            (None, capacity(len)?)
        };
        disk.vhd = vhd;
        Ok((disk, sectors))
    }

    pub(super) fn create_vhd(path: &Path, size: u64) -> io::Result<()> {
        let sectors = capacity(size)?;
        let entries = size.div_ceil(u64::from(BLOCK_SIZE)) as u32;
        let table_bytes = (u64::from(entries) * 4).div_ceil(512) * 512;
        let mut footer = [0; 512];
        footer[..8].copy_from_slice(b"conectix");
        put32(&mut footer, 8, 2);
        put32(&mut footer, 12, VERSION);
        put64(&mut footer, 16, 512);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        put32(
            &mut footer,
            24,
            now.as_secs().saturating_sub(946_684_800) as u32,
        );
        footer[28..32].copy_from_slice(b"r86 ");
        put32(&mut footer, 32, VERSION);
        footer[36..40].copy_from_slice(b"Wi2k");
        put64(&mut footer, 40, size);
        put64(&mut footer, 48, size);
        footer[56..60].copy_from_slice(&geometry(sectors));
        put32(&mut footer, 60, 3);
        // This is an image identity, not a secret. Time + PID + per-process
        // sequence avoids depending on guest entropy or a UUID library.
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let identity = now.as_nanos()
            ^ ((u128::from(std::process::id())) << 64)
            ^ (u128::from(SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) << 96);
        footer[68..84].copy_from_slice(&identity.to_be_bytes());
        let sum = checksum(&footer, 64);
        put32(&mut footer, 64, sum);
        let mut header = [0; 1024];
        header[..8].copy_from_slice(b"cxsparse");
        put64(&mut header, 8, u64::MAX);
        put64(&mut header, 16, 1536);
        put32(&mut header, 24, VERSION);
        put32(&mut header, 28, entries);
        put32(&mut header, 32, BLOCK_SIZE);
        let sum = checksum(&header, 36);
        put32(&mut header, 36, sum);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.try_lock().map_err(io::Error::other)?;
        let mut disk = Self {
            file,
            vhd: None,
            failed: None,
        };
        let file = &mut disk.file;
        write_at(file, 0, &footer)?;
        write_at(file, 512, &header)?;
        write_at(file, 1536, &vec![0xff; table_bytes as usize])?;
        write_at(file, 1536 + table_bytes, &footer)?;
        file.sync_all()
    }

    fn healthy(&self) -> io::Result<()> {
        match &self.failed {
            Some(message) => Err(io::Error::other(format!(
                "disk stopped after an I/O failure: {message}"
            ))),
            None => Ok(()),
        }
    }

    fn mutation_result(&mut self, result: io::Result<()>) -> io::Result<()> {
        if let Err(error) = &result {
            self.failed = Some(error.to_string());
        }
        result
    }

    pub(super) fn read(&mut self, sector: u64, data: &mut [u8]) -> io::Result<()> {
        self.healthy()?;
        match &mut self.vhd {
            Some(vhd) => vhd.read(&mut self.file, sector, data),
            None => read_at(&mut self.file, sector * 512, data),
        }
    }

    pub(super) fn write(&mut self, sector: u64, data: &[u8]) -> io::Result<()> {
        self.healthy()?;
        let result = match &mut self.vhd {
            Some(vhd) => vhd.write(&mut self.file, sector, data),
            None => write_at(&mut self.file, sector * 512, data),
        };
        self.mutation_result(result)
    }

    pub(super) fn flush(&mut self) -> io::Result<()> {
        self.healthy()?;
        let result = self.file.sync_all();
        self.mutation_result(result)
    }
}

impl Vhd {
    fn open(file: &mut File, footer: [u8; 512], file_len: u64) -> io::Result<Self> {
        if be32(&footer, 60) != 3 {
            return Err(invalid(
                "only dynamic VHD is supported (no fixed or differencing images)",
            ));
        }
        if be32(&footer, 12) != VERSION || !matches!(be32(&footer, 8), 2 | 3) {
            return Err(invalid("unsupported VHD footer version/features"));
        }
        let sectors = capacity(be64(&footer, 48))?;
        let header_offset = be64(&footer, 16);
        let data_end = file_len - 512;
        let mut extents = vec![(0, 512)];
        // Extents are validated before any metadata-sized allocation or seek.
        fn extent(
            extents: &mut Vec<(u64, u64)>,
            start: u64,
            size: u64,
            end: u64,
        ) -> io::Result<()> {
            if start % 512 != 0 || start > end || size > end - start {
                return Err(invalid("VHD metadata/block extent is outside the file"));
            }
            extents.push((start, start + size));
            Ok(())
        }
        extent(&mut extents, header_offset, 1024, data_end)?;
        let mut header = [0; 1024];
        read_at(file, header_offset, &mut header)?;
        if &header[..8] != b"cxsparse" || checksum(&header, 36) != be32(&header, 36) {
            return Err(invalid("invalid VHD sparse header cookie/checksum"));
        }
        if be32(&header, 24) != VERSION
            || be64(&header, 8) != u64::MAX
            || header[40..768].iter().any(|&byte| byte != 0)
        {
            return Err(invalid("unsupported VHD sparse header or parent metadata"));
        }
        let block_size = be32(&header, 32);
        // Bound memory at open. The commonly used 512 KiB and 2 MiB block sizes
        // are included; exotic sizes outside this range are explicitly rejected.
        if !block_size.is_power_of_two() || !(512 * 1024..=32 * 1024 * 1024).contains(&block_size) {
            return Err(invalid(
                "VHD block size must be a power of two from 512 KiB to 32 MiB",
            ));
        }
        let block_sectors = u64::from(block_size) / 512;
        let entries = be32(&header, 28) as usize;
        if entries as u64 != sectors.div_ceil(block_sectors) {
            return Err(invalid("VHD BAT count does not match the virtual capacity"));
        }
        let table_offset = be64(&header, 16);
        extent(
            &mut extents,
            table_offset,
            (entries as u64 * 4).div_ceil(512) * 512,
            data_end,
        )?;
        let mut table = vec![0; entries * 4];
        read_at(file, table_offset, &mut table)?;
        let bat: Vec<_> = table.chunks_exact(4).map(|bytes| be32(bytes, 0)).collect();
        let bitmap_bytes = block_sectors.div_ceil(8).div_ceil(512) * 512;
        for &block in &bat {
            if block != FREE {
                extent(
                    &mut extents,
                    u64::from(block) * 512,
                    bitmap_bytes + u64::from(block_size),
                    data_end,
                )?;
            }
        }
        extents.sort_unstable();
        if extents.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(invalid("VHD has overlapping metadata or allocated blocks"));
        }
        Ok(Self {
            footer,
            table_offset,
            bat,
            block_sectors,
            bitmap: vec![0; bitmap_bytes as usize],
            bitmap_block: None,
            file_len,
        })
    }

    fn load_bitmap(&mut self, file: &mut File, block: usize) -> io::Result<()> {
        if self.bitmap_block != Some(block) {
            self.bitmap_block = None;
            read_at(file, u64::from(self.bat[block]) * 512, &mut self.bitmap)?;
            self.bitmap_block = Some(block);
        }
        Ok(())
    }

    fn read(&mut self, file: &mut File, sector: u64, data: &mut [u8]) -> io::Result<()> {
        for (i, dst) in data.chunks_exact_mut(SECTOR_SIZE).enumerate() {
            let lba = sector + i as u64;
            let block = (lba / self.block_sectors) as usize;
            let within = (lba % self.block_sectors) as usize;
            if self.bat[block] == FREE {
                dst.fill(0);
                continue;
            }
            self.load_bitmap(file, block)?;
            if self.bitmap[within / 8] & (0x80 >> (within % 8)) == 0 {
                dst.fill(0);
            } else {
                read_at(
                    file,
                    u64::from(self.bat[block]) * 512
                        + self.bitmap.len() as u64
                        + within as u64 * 512,
                    dst,
                )?;
            }
        }
        Ok(())
    }

    fn write(&mut self, file: &mut File, sector: u64, data: &[u8]) -> io::Result<()> {
        for (i, src) in data.chunks_exact(SECTOR_SIZE).enumerate() {
            let lba = sector + i as u64;
            let block = (lba / self.block_sectors) as usize;
            let within = (lba % self.block_sectors) as usize;
            if self.bat[block] == FREE {
                self.allocate(file, block)?;
            }
            self.load_bitmap(file, block)?;
            let offset = u64::from(self.bat[block]) * 512;
            write_at(
                file,
                offset + self.bitmap.len() as u64 + within as u64 * 512,
                src,
            )?;
            let byte = within / 8;
            let bit = 0x80 >> (within % 8);
            if self.bitmap[byte] & bit == 0 {
                self.bitmap[byte] |= bit;
                write_at(file, offset + byte as u64, &self.bitmap[byte..byte + 1])?;
            }
        }
        Ok(())
    }

    fn allocate(&mut self, file: &mut File, block: usize) -> io::Result<()> {
        let start = self.file_len;
        if start / 512 >= u64::from(FREE) {
            return Err(invalid("VHD physical file exceeds BAT address space"));
        }
        let footer_offset = start + self.bitmap.len() as u64 + self.block_sectors * 512;
        // Extending a regular file guarantees zero reads from its new region,
        // with or without sparse-file support in the host filesystem.
        file.set_len(footer_offset + 512)?;
        write_at(file, footer_offset, &self.footer)?;
        file.sync_all()?;
        let entry = (start / 512) as u32;
        write_at(
            file,
            self.table_offset + block as u64 * 4,
            &entry.to_be_bytes(),
        )?;
        self.bat[block] = entry;
        self.file_len = footer_offset + 512;
        self.bitmap.fill(0);
        self.bitmap_block = Some(block);
        Ok(())
    }
}

/// CHS compatibility geometry from the VHD specification. Logical capacity is
/// still the exact footer current-size field, not this rounded legacy geometry.
fn geometry(mut sectors: u64) -> [u8; 4] {
    sectors = sectors.min(65535 * 16 * 255);
    let (heads, track) = if sectors >= 65535 * 16 * 63 {
        (16, 255)
    } else {
        let heads = (sectors / 17).div_ceil(1024).max(4);
        if heads <= 16 && sectors / 17 < heads * 1024 {
            (heads, 17)
        } else if sectors / 31 < 16 * 1024 {
            (16, 31)
        } else {
            (16, 63)
        }
    };
    let cylinders = (sectors / (heads * track)) as u16;
    [
        (cylinders >> 8) as u8,
        cylinders as u8,
        heads as u8,
        track as u8,
    ]
}
