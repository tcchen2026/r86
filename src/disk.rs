//! Sector storage used directly by the IDE controller. Memory disks also work
//! in no_std builds; host files are never loaded wholesale into guest RAM.
use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
use core::fmt;

#[cfg(feature = "host")]
mod file;

pub const SECTOR_SIZE: usize = 512;
/// The controller exposes LBA28, not LBA48.
pub const MAX_SECTORS: u64 = 1 << 28;

#[derive(Debug)]
pub enum DiskError {
    OutOfRange,
    ReadOnly,
    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    BrowserIo,
    #[cfg(feature = "host")]
    Io(std::io::Error),
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => f.write_str("disk sector range is out of bounds or unaligned"),
            Self::ReadOnly => f.write_str("disk is read-only"),
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Self::BrowserIo => {
                f.write_str("browser storage operation failed; see storage diagnostics")
            }
            #[cfg(feature = "host")]
            Self::Io(error) => error.fmt(f),
        }
    }
}

#[cfg(feature = "host")]
impl std::error::Error for DiskError {}

#[cfg(feature = "host")]
impl From<std::io::Error> for DiskError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
enum Storage {
    Memory(Vec<u8>),
    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    Browser(u32),
    #[cfg(feature = "host")]
    File(file::FileDisk),
}

#[derive(Debug)]
pub struct Disk {
    storage: Storage,
    sectors: u64,
    writable: bool,
    frozen: bool,
    changes: BTreeMap<u64, Box<[u8; SECTOR_SIZE]>>,
}

impl Disk {
    /// The JS host validates the backing descriptor before attaching it;
    /// `drive` identifies that backing for the lifetime of this instance.
    #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
    pub fn browser(drive: u32, sectors: u64, writable: bool) -> Result<Self, DiskError> {
        if sectors == 0 || sectors > MAX_SECTORS {
            return Err(DiskError::OutOfRange);
        }
        Ok(Self {
            storage: Storage::Browser(drive),
            sectors,
            writable,
            frozen: false,
            changes: BTreeMap::new(),
        })
    }

    pub fn memory(image: Vec<u8>, writable: bool) -> Self {
        Self {
            sectors: (image.len() / SECTOR_SIZE) as u64,
            storage: Storage::Memory(image),
            writable,
            frozen: false,
            changes: BTreeMap::new(),
        }
    }

    /// Open a raw image or dynamic VHD. Writable opens take an exclusive file
    /// lock; read-only opens take a shared lock. An invalid .vhd is never raw.
    #[cfg(feature = "host")]
    pub fn open(path: impl AsRef<std::path::Path>, writable: bool) -> Result<Self, DiskError> {
        let (file, sectors) = file::FileDisk::open(path.as_ref(), writable)?;
        Ok(Self {
            storage: Storage::File(file),
            sectors,
            writable,
            frozen: false,
            changes: BTreeMap::new(),
        })
    }

    /// Make a read-only base writable through an in-memory sector overlay.
    /// Changes live with this Disk, including across guest resets, and vanish
    /// when it is dropped. There is no size cap beyond the disk's capacity.
    /// File callers must use `Disk::open(path, false)` to retain a shared lock.
    pub fn frozen(mut self) -> Self {
        assert!(!self.writable, "frozen disks require a read-only base");
        self.frozen = true;
        self.writable = true;
        self
    }

    /// Create an empty dynamic VHD without overwriting an existing path.
    /// `size` is the exact virtual capacity in bytes (a multiple of 512).
    #[cfg(feature = "host")]
    pub fn create_vhd(path: impl AsRef<std::path::Path>, size: u64) -> Result<(), DiskError> {
        file::FileDisk::create_vhd(path.as_ref(), size)?;
        Ok(())
    }

    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    fn check_range(&self, sector: u64, bytes: usize) -> Result<(), DiskError> {
        if bytes % SECTOR_SIZE != 0
            || sector > self.sectors
            || (bytes / SECTOR_SIZE) as u64 > self.sectors - sector
        {
            return Err(DiskError::OutOfRange);
        }
        Ok(())
    }

    pub fn read(&mut self, sector: u64, data: &mut [u8]) -> Result<(), DiskError> {
        self.check_range(sector, data.len())?;
        match &mut self.storage {
            Storage::Memory(image) => {
                let start = sector as usize * SECTOR_SIZE;
                data.copy_from_slice(&image[start..start + data.len()]);
            }
            #[cfg(feature = "host")]
            Storage::File(file) => file.read(sector, data)?,
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Storage::Browser(drive) => {
                crate::browser::read(*drive, sector * SECTOR_SIZE as u64, data)?
            }
        }
        if !self.changes.is_empty() {
            let end = sector + (data.len() / SECTOR_SIZE) as u64;
            for (&lba, bytes) in self.changes.range(sector..end) {
                let start = (lba - sector) as usize * SECTOR_SIZE;
                data[start..start + SECTOR_SIZE].copy_from_slice(bytes.as_ref());
            }
        }
        Ok(())
    }

    /// A failed multi-sector call may have completed a prefix. The IDE device
    /// calls this once per sector so its task file can report that progress.
    pub fn write(&mut self, sector: u64, data: &[u8]) -> Result<(), DiskError> {
        self.check_range(sector, data.len())?;
        if !self.writable {
            return Err(DiskError::ReadOnly);
        }
        if self.frozen {
            for (index, bytes) in data.chunks_exact(SECTOR_SIZE).enumerate() {
                self.changes
                    .entry(sector + index as u64)
                    .or_insert_with(|| Box::new([0; SECTOR_SIZE]))
                    .copy_from_slice(bytes);
            }
            return Ok(());
        }
        match &mut self.storage {
            Storage::Memory(image) => {
                let start = sector as usize * SECTOR_SIZE;
                image[start..start + data.len()].copy_from_slice(data);
            }
            #[cfg(feature = "host")]
            Storage::File(file) => file.write(sector, data)?,
            #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
            Storage::Browser(drive) => {
                crate::browser::write(*drive, sector * SECTOR_SIZE as u64, data)?
            }
        }
        Ok(())
    }

    /// Successful writes reach durable host storage when this returns Ok.
    /// As with a real disk, this relies on the host honoring its sync contract.
    /// Frozen disks instead succeed without flushing the base: their writes
    /// are never durable and remain in memory only until this Disk is dropped.
    pub fn flush(&mut self) -> Result<(), DiskError> {
        if self.frozen {
            return Ok(());
        }
        #[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
        if let Storage::Browser(drive) = &self.storage {
            crate::browser::flush(*drive)?;
        }
        #[cfg(feature = "host")]
        if self.writable {
            if let Storage::File(file) = &mut self.storage {
                file.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "host")]
impl Drop for Disk {
    fn drop(&mut self) {
        if self.frozen {
            use std::io::Write;
            let sectors = self.changes.len();
            let _ = writeln!(
                std::io::stderr(),
                "frozen disk: {sectors} sectors ({:.2} MiB) of changes discarded",
                sectors as f64 * SECTOR_SIZE as f64 / (1024.0 * 1024.0)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn frozen_readback_preserves_base_and_range_checks() {
        let base: Vec<_> = (0..4).flat_map(|i| [i; SECTOR_SIZE]).collect();
        let mut disk = Disk::memory(base.clone(), false).frozen();
        assert!(disk.writable());
        assert_eq!(disk.sectors(), 4);
        let mut data = vec![0; base.len()];
        disk.read(0, &mut data).unwrap();
        assert_eq!(data, base);
        disk.write(2, &[0xa5; 2 * SECTOR_SIZE]).unwrap();
        disk.write(2, &[0xb4; SECTOR_SIZE]).unwrap();
        disk.flush().unwrap();
        disk.read(0, &mut data).unwrap();
        assert_eq!(&data[..2 * SECTOR_SIZE], &base[..2 * SECTOR_SIZE]);
        assert_eq!(
            &data[2 * SECTOR_SIZE..3 * SECTOR_SIZE],
            &[0xb4; SECTOR_SIZE]
        );
        assert_eq!(&data[3 * SECTOR_SIZE..], &[0xa5; SECTOR_SIZE]);
        let mut middle = [0; 2 * SECTOR_SIZE];
        disk.read(1, &mut middle).unwrap();
        assert_eq!(&middle, &data[SECTOR_SIZE..3 * SECTOR_SIZE]);
        for (sector, len) in [(3, 1024), (4, 512), (u64::MAX, 512), (0, 511)] {
            assert!(matches!(
                disk.write(sector, &vec![0; len]),
                Err(DiskError::OutOfRange)
            ));
            assert!(matches!(
                disk.read(sector, &mut vec![0; len]),
                Err(DiskError::OutOfRange)
            ));
        }
        disk.write(4, &[]).unwrap();
        disk.read(4, &mut []).unwrap();
        let mut unchanged = vec![0; base.len()];
        disk.read(0, &mut unchanged).unwrap();
        assert_eq!(unchanged, data);
        let Storage::Memory(image) = &disk.storage else {
            panic!("expected memory base");
        };
        assert_eq!(image, &base);
    }

    #[cfg(feature = "host")]
    #[test]
    fn frozen_files_share_base_with_independent_changes_and_exclude_writers() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("r86-frozen-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        for name in ["disk.raw", "disk.vhd"] {
            let path = dir.join(name);
            if name.ends_with("vhd") {
                Disk::create_vhd(&path, 4 * SECTOR_SIZE as u64).unwrap();
                let mut seed = Disk::open(&path, true).unwrap();
                seed.write(0, &[0x3c; 4 * SECTOR_SIZE]).unwrap();
                seed.flush().unwrap();
            } else {
                fs::write(&path, [0x3c; 4 * SECTOR_SIZE]).unwrap();
            }
            let before = fs::read(&path).unwrap();
            let mut first = Disk::open(&path, false).unwrap().frozen();
            let mut second = Disk::open(&path, false).unwrap().frozen();
            assert!(Disk::open(&path, true).is_err());
            first.write(1, &[0xa5; 2 * SECTOR_SIZE]).unwrap();
            first.flush().unwrap();
            let mut data = [0; 4 * SECTOR_SIZE];
            first.read(0, &mut data).unwrap();
            assert_eq!(&data[..SECTOR_SIZE], &[0x3c; SECTOR_SIZE]);
            assert_eq!(
                &data[SECTOR_SIZE..3 * SECTOR_SIZE],
                &[0xa5; 2 * SECTOR_SIZE]
            );
            assert_eq!(&data[3 * SECTOR_SIZE..], &[0x3c; SECTOR_SIZE]);
            second.read(0, &mut data).unwrap();
            assert_eq!(data, [0x3c; 4 * SECTOR_SIZE]);
            drop(first);
            assert!(Disk::open(&path, true).is_err());
            drop(second);
            let mut fresh = Disk::open(&path, false).unwrap().frozen();
            fresh.read(0, &mut data).unwrap();
            assert_eq!(data, [0x3c; 4 * SECTOR_SIZE]);
            drop(fresh);
            let writer = Disk::open(&path, true).unwrap();
            assert!(Disk::open(&path, false).is_err());
            drop(writer);
            assert_eq!(fs::read(&path).unwrap(), before);
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(dir).unwrap();
    }
}
