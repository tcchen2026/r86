//! Sector storage used directly by the IDE controller. Memory disks also work
//! in no_std builds; host files are never loaded wholesale into guest RAM.
use alloc::vec::Vec;
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
        })
    }

    pub fn memory(image: Vec<u8>, writable: bool) -> Self {
        Self {
            sectors: (image.len() / SECTOR_SIZE) as u64,
            storage: Storage::Memory(image),
            writable,
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
        })
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
        Ok(())
    }

    /// A failed multi-sector call may have completed a prefix. The IDE device
    /// calls this once per sector so its task file can report that progress.
    pub fn write(&mut self, sector: u64, data: &[u8]) -> Result<(), DiskError> {
        self.check_range(sector, data.len())?;
        if !self.writable {
            return Err(DiskError::ReadOnly);
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
    pub fn flush(&mut self) -> Result<(), DiskError> {
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
