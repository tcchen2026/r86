//! Virtual media for remote consoles, as on a server BMC. A client uploads
//! either one ISO image or files and folders that become an in-memory CD
//! (ISO 9660 level-1 names for DOS, Joliet long names for Windows). Uploads
//! fill host memory, or with a save directory (the handheld's VM folder) an
//! ISO streams to a file there; the machine owner inserts the finished image.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use r86::iso9660::{IsoBuilder, IsoTime};

/// Largest image one upload may stage. An in-memory image stays in host
/// memory while inserted.
pub const MEDIA_LIMIT: u64 = 1 << 30;
const SECTOR: u64 = 2048;

pub struct Upload {
    /// Connection that started the upload; its disconnect cancels it.
    pub owner: usize,
    pub name: String,
    kind: Kind,
}

enum Kind {
    Iso {
        size: u64,
        data: Vec<u8>,
    },
    Saved {
        size: u64,
        written: u64,
        part: Part,
    },
    /// `open` counts bytes still due for the current file.
    Files {
        builder: IsoBuilder,
        open: u64,
    },
}

impl Upload {
    /// An ISO image of `size` bytes, held in memory or streamed into `dir`.
    pub fn iso(owner: usize, name: &str, size: u64, dir: Option<&Path>) -> Result<Self, String> {
        if size == 0 || size % SECTOR != 0 {
            return Err("An ISO image is a whole number of 2048-byte sectors.".into());
        }
        if size > MEDIA_LIMIT {
            return Err(limit_error());
        }
        let kind = match dir {
            Some(dir) => Kind::Saved {
                size,
                written: 0,
                part: Part::create(dir, name)?,
            },
            None => {
                let mut data = Vec::new();
                data.try_reserve_exact(size as usize)
                    .map_err(|_| "Not enough host memory for this image.".to_string())?;
                Kind::Iso { size, data }
            }
        };
        Ok(Self {
            owner,
            name: name.into(),
            kind,
        })
    }

    pub fn files(owner: usize, name: &str) -> Self {
        let name = if name.trim().is_empty() {
            "R86"
        } else {
            name.trim()
        };
        Self {
            owner,
            name: name.into(),
            kind: Kind::Files {
                builder: IsoBuilder::new(name, now()),
                open: 0,
            },
        }
    }

    pub fn begin_file(&mut self, path: &str, size: u64) -> Result<(), String> {
        let (builder, open) = self.files_mut()?;
        if builder.len() + size.div_ceil(SECTOR) * SECTOR > MEDIA_LIMIT {
            return Err(limit_error());
        }
        builder.begin_file(path, size)?;
        *open = size;
        Ok(())
    }

    pub fn add_dir(&mut self, path: &str) -> Result<(), String> {
        self.files_mut()?.0.add_dir(path)
    }

    /// Appends one binary chunk to the image or the current file.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        match &mut self.kind {
            Kind::Iso { size, data } => {
                if (data.len() + chunk.len()) as u64 > *size {
                    return Err("More data than the announced image size.".into());
                }
                data.extend_from_slice(chunk);
            }
            Kind::Saved {
                size,
                written,
                part,
            } => {
                if *written + chunk.len() as u64 > *size {
                    return Err("More data than the announced image size.".into());
                }
                part.file.write_all(chunk).map_err(save_error)?;
                *written += chunk.len() as u64;
            }
            Kind::Files { builder, open } => {
                if chunk.len() as u64 > *open {
                    return Err("More data than the announced file size.".into());
                }
                builder.write(chunk)?;
                *open -= chunk.len() as u64;
            }
        }
        Ok(())
    }

    /// Bytes received so far and the number of files.
    pub fn progress(&self) -> (u64, usize) {
        match &self.kind {
            Kind::Iso { data, .. } => (data.len() as u64, 1),
            Kind::Saved { written, .. } => (*written, 1),
            Kind::Files { builder, open } => (builder.len() - open, builder.files()),
        }
    }

    pub fn is_iso(&self) -> bool {
        !matches!(self.kind, Kind::Files { .. })
    }

    /// The complete image, once every announced byte has arrived. With
    /// `dir`, an image built from files is saved there too, freeing its memory.
    pub fn finish(self, dir: Option<&Path>) -> Result<Image, String> {
        match self.kind {
            Kind::Iso { size, data } if data.len() as u64 == size => Ok(Image::Bytes(data)),
            Kind::Saved {
                size,
                written,
                part,
            } if written == size => part.finish(),
            Kind::Files { builder, open: 0 } => {
                let data = builder.finish()?;
                match dir {
                    Some(dir) => {
                        let mut part = Part::create(dir, &self.name)?;
                        part.file.write_all(&data).map_err(save_error)?;
                        part.finish()
                    }
                    None => Ok(Image::Bytes(data)),
                }
            }
            _ => Err("The upload is incomplete.".into()),
        }
    }

    fn files_mut(&mut self) -> Result<(&mut IsoBuilder, &mut u64), String> {
        match &mut self.kind {
            Kind::Files {
                builder,
                open: open @ 0,
            } => Ok((builder, open)),
            Kind::Files { open, .. } => {
                Err(format!("The previous file still expects {open} bytes."))
            }
            Kind::Iso { .. } | Kind::Saved { .. } => {
                Err("This upload is an ISO image, not files.".into())
            }
        }
    }
}

/// A finished upload: an in-memory disc or a saved image file.
pub enum Image {
    Bytes(Vec<u8>),
    File(fs::File, PathBuf),
}

/// An image being written as `NAME.iso.part`; dropped unfinished, it is deleted.
struct Part {
    file: fs::File,
    path: PathBuf,
    dest: PathBuf,
}

impl Part {
    fn create(dir: &Path, name: &str) -> Result<Self, String> {
        // The browser's file name, without any path, always ending in .iso so
        // an upload can never replace the VM's disk or settings.
        let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
        if base.is_empty() || base.starts_with('.') || base.chars().any(char::is_control) {
            return Err("Name the image file.".into());
        }
        let file = if base.to_ascii_lowercase().ends_with(".iso") {
            base.to_string()
        } else {
            format!("{base}.iso")
        };
        let dest = dir.join(&file);
        let path = dir.join(format!("{file}.part"));
        let file = fs::File::create(&path).map_err(save_error)?;
        Ok(Self { file, path, dest })
    }

    fn finish(self) -> Result<Image, String> {
        self.file.sync_all().map_err(save_error)?;
        fs::rename(&self.path, &self.dest).map_err(save_error)?;
        let file = fs::File::open(&self.dest).map_err(save_error)?;
        Ok(Image::File(file, self.dest.clone()))
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        // After a finished rename the part no longer exists.
        let _ = fs::remove_file(&self.path);
    }
}

fn save_error(err: std::io::Error) -> String {
    format!("Cannot save the image on this device: {err}")
}

fn limit_error() -> String {
    format!("Virtual media is limited to {} MiB.", MEDIA_LIMIT >> 20)
}

/// Current UTC time for the CD's directory records.
fn now() -> IsoTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    let rem = secs % 86400;
    IsoTime {
        year,
        month,
        day,
        hour: (rem / 3600) as u8,
        minute: (rem / 60 % 60) as u8,
        second: (rem % 60) as u8,
    }
}
