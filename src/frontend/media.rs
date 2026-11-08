//! Virtual media for remote consoles, as on a server BMC. A client uploads
//! either one ISO image or files and folders that become an in-memory CD
//! (ISO 9660 level-1 names for DOS, Joliet long names for Windows). Uploads
//! fill host memory, or with a save directory (the handheld's iso/ folder) an
//! ISO streams to a file there; the machine owner inserts the finished image.
//! The handheld's import page saves disk and CD images the same way.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use r86::iso9660::{IsoBuilder, IsoTime};

/// Largest image one upload may stage. An in-memory image stays in host
/// memory while inserted.
pub const MEDIA_LIMIT: u64 = 1 << 30;
const SECTOR: u64 = 2048;
/// Disk image extensions an import accepts, as the New VM wizard lists them.
#[allow(dead_code)] // The handheld's import page; r86-run has none.
const DISK_EXTS: [&str; 3] = ["vhd", "img", "raw"];

/// What the handheld's import page receives.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageKind {
    Cd,
    Disk,
}

#[allow(dead_code)]
impl ImageKind {
    /// The page's name for it, in the import hello.
    pub fn name(self) -> &'static str {
        match self {
            ImageKind::Cd => "iso",
            ImageKind::Disk => "disk",
        }
    }
}

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
        let kind = match dir {
            Some(dir) => Kind::Saved {
                size,
                written: 0,
                part: Part::create(dir, name, "iso", size)?,
            },
            None if size > MEDIA_LIMIT => return Err(limit_error()),
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

    /// A hard disk image of `size` bytes (.vhd, .img or .raw), streamed into `dir`.
    #[allow(dead_code)]
    pub fn disk(owner: usize, name: &str, size: u64, dir: &Path) -> Result<Self, String> {
        if size == 0 || size % 512 != 0 {
            return Err("A disk image is a whole number of 512-byte sectors.".into());
        }
        let ext = DISK_EXTS
            .into_iter()
            .find(|ext| split_ext(name, ext).is_some())
            .ok_or("Choose a .vhd, .img or .raw disk image.")?;
        Ok(Self {
            owner,
            name: name.into(),
            kind: Kind::Saved {
                size,
                written: 0,
                part: Part::create(dir, name, ext, size)?,
            },
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
                part.write(chunk)?;
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
                        let mut part = Part::create(dir, &self.name, "iso", data.len() as u64)?;
                        part.write(&data)?;
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

/// An image received as `upload-N.part`. Finished, it is named after the
/// sender's file and the start of its SHA-1 (`NAME-1a2b3c4d.iso`): sending the
/// same image again replaces its identical copy, never a different image.
/// Dropped unfinished, it is deleted.
struct Part {
    file: fs::File,
    path: PathBuf,
    stem: String,
    ext: &'static str,
    sha1: Sha1,
}

impl Part {
    fn create(dir: &Path, name: &str, ext: &'static str, size: u64) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(save_error)?;
        check_space(dir, size)?;
        // A unique name: replacing an upload drops the old part after the
        // new one exists.
        static PARTS: AtomicU32 = AtomicU32::new(0);
        let path = dir.join(format!(
            "upload-{}.part",
            PARTS.fetch_add(1, Ordering::Relaxed)
        ));
        let file = fs::File::create(&path).map_err(save_error)?;
        Ok(Self {
            file,
            path,
            stem: safe_stem(name, ext),
            ext,
            sha1: Sha1::new(),
        })
    }

    fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.sha1.update(chunk);
        self.file.write_all(chunk).map_err(save_error)
    }

    fn finish(self) -> Result<Image, String> {
        self.file.sync_all().map_err(save_error)?;
        let name = format!("{}-{}.{}", self.stem, &self.sha1.hex()[..8], self.ext);
        let dest = self.path.with_file_name(name);
        fs::rename(&self.path, &dest).map_err(save_error)?;
        let file = fs::File::open(&dest).map_err(save_error)?;
        Ok(Image::File(file, dest))
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

/// `name` without a final `.ext` (any case), if it has one.
fn split_ext<'a>(name: &'a str, ext: &str) -> Option<&'a str> {
    let stem = name.get(..name.len().checked_sub(ext.len() + 1)?)?;
    let tail = &name[stem.len()..];
    (tail.starts_with('.') && tail[1..].eq_ignore_ascii_case(ext)).then_some(stem)
}

/// The sender's file name without folders or `.ext`, safe on FAT and exFAT
/// (the TrimUI's SD card): characters those reject become `_`.
fn safe_stem(name: &str, ext: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let stem: String = split_ext(base, ext)
        .unwrap_or(base)
        .chars()
        .map(|c| {
            if c.is_control() || "\"*:<>?|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    match stem.trim_matches([' ', '.']) {
        "" => "image".into(),
        stem => stem.into(),
    }
}

/// Fails before the upload when `dir` cannot hold `size` more bytes.
fn check_space(dir: &Path, size: u64) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: `path` is NUL-terminated and `stat` is a writable statvfs.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(save_error(std::io::Error::last_os_error()));
    }
    let free = stat.f_bavail as u64 * stat.f_frsize as u64;
    if size > free {
        return Err(format!(
            "Not enough space on this device: {} MiB free.",
            free >> 20
        ));
    }
    Ok(())
}

/// SHA-1 (FIPS 180-4), naming saved images by content.
#[derive(Clone)]
struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    used: usize,
    bytes: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            block: [0; 64],
            used: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.bytes += data.len() as u64;
        while !data.is_empty() {
            let n = (64 - self.used).min(data.len());
            self.block[self.used..self.used + n].copy_from_slice(&data[..n]);
            self.used += n;
            data = &data[n..];
            if self.used == 64 {
                self.compress();
                self.used = 0;
            }
        }
    }

    fn hex(&self) -> String {
        let mut last = self.clone();
        let bits = self.bytes * 8;
        last.update(&[0x80]);
        while last.used != 56 {
            last.update(&[0]);
        }
        last.update(&bits.to_be_bytes());
        last.state
            .iter()
            .map(|word| format!("{word:08x}"))
            .collect()
    }

    fn compress(&mut self) {
        let mut w = [0u32; 80];
        for (i, word) in self.block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t.wrapping_add(word));
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(value);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_fips_vectors_across_chunk_boundaries() {
        let digest = |data: &[u8], chunk: usize| {
            let mut sha1 = Sha1::new();
            data.chunks(chunk.max(1)).for_each(|part| sha1.update(part));
            sha1.hex()
        };
        assert_eq!(digest(b"", 1), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            digest(b"abc", 1),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let two_blocks = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            digest(two_blocks, 7),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            digest(&million, 4099),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn saved_images_are_named_by_sender_name_and_content() {
        let dir = std::env::temp_dir().join(format!("r86-media-{}", std::process::id()));
        let save = |name: &str, data: &[u8]| -> Result<String, String> {
            let mut upload = Upload::disk(0, name, data.len() as u64, &dir)?;
            upload.write(data)?;
            match upload.finish(None)? {
                Image::File(_, path) => {
                    Ok(path.file_name().unwrap().to_string_lossy().into_owned())
                }
                Image::Bytes(_) => unreachable!(),
            }
        };
        let sector = [7u8; 512];
        // SHA-1 of 512 bytes of 7 starts with these 8 digits.
        let tag = &{
            let mut sha1 = Sha1::new();
            sha1.update(&sector);
            sha1.hex()
        }[..8];
        assert_eq!(
            save("C:\\guests\\Win: 98?.VHD", &sector),
            Ok(format!("Win_ 98_-{tag}.vhd"))
        );
        assert_eq!(
            save("  ..hidden.img", &sector),
            Ok(format!("hidden-{tag}.img"))
        );
        assert!(save("notes.txt", &sector).is_err());
        assert!(save("odd.img", &sector[..100]).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "no part is left");
        fs::remove_dir_all(&dir).unwrap();
    }
}
