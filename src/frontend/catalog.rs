//! Free operating systems offered by the new-VM wizard, and the background
//! job that prepares one: install FreeDOS from r86's DOS kit onto a new disk,
//! or download a live CD, verify its SHA-256 and unpack it.
//!
//! Downloads use the system `curl` (TLS, redirects, resume) and `sha256sum`;
//! the CardputerZero package depends on both.
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use flate2::read::DeflateDecoder;

use super::vm_store::Vm;

pub struct FreeOs {
    pub title: &'static str,
    pub detail: &'static str,
    /// Suggested VM name.
    pub name: &'static str,
    /// guest_os preset name.
    pub preset: &'static str,
    pub memory_mib: u32,
    pub disk_mib: u64,
    /// Shown before downloading; 0 when the OS ships with r86.
    pub download_mib: u64,
    pub media: Media,
}

pub enum Media {
    /// FreeDOS installed from the DOS kit in r86's share directory onto a new
    /// disk of the chosen size.
    FreeDos,
    /// A CD image inside a zip archive, attached to a VM with a blank disk.
    ZippedIso {
        url: &'static str,
        zip: &'static str,
        zip_size: u64,
        zip_sha256: &'static str,
        iso: &'static str,
    },
}

pub const CATALOG: &[FreeOs] = &[
    FreeOs {
        title: "ReactOS 0.4.15",
        detail: "Windows-like, live CD",
        name: "ReactOS",
        preset: "ReactOS",
        memory_mib: 128,
        disk_mib: 4096,
        download_mib: 82,
        media: Media::ZippedIso {
            url: "https://sourceforge.net/projects/reactos/files/ReactOS/0.4.15/ReactOS-0.4.15-release-1-gdbb43bbaeb2-x86-live.zip/download",
            zip: "ReactOS-0.4.15-release-1-gdbb43bbaeb2-x86-live.zip",
            zip_size: 85_540_881,
            zip_sha256: "ab5e4a1686b0c6d8c61b5c536dd94de36befe1d2345cec1b865f3ca43b2b2e54",
            iso: "ReactOS-0.4.15-release-1-gdbb43bbaeb2-x86-live.iso",
        },
    },
    FreeOs {
        title: "FreeDOS",
        detail: "ready to use",
        name: "FreeDOS",
        preset: "DOS",
        memory_mib: 32,
        disk_mib: 512,
        download_mib: 0,
        media: Media::FreeDos,
    },
];

#[derive(Default)]
struct State {
    stage: Mutex<String>,
    done: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

impl State {
    fn stage(&self, stage: &str, total: u64) {
        *self.stage.lock().unwrap() = stage.into();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
    fn check(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            Err("Cancelled.".into())
        } else {
            Ok(())
        }
    }
}

/// A preparation running on a worker thread. The UI polls it.
pub struct Job {
    state: Arc<State>,
    handle: Option<JoinHandle<Result<Vm, String>>>,
}

impl Job {
    /// (stage, bytes done, bytes total or 0 when unknown).
    pub fn status(&self) -> (String, u64, u64) {
        (
            self.state.stage.lock().unwrap().clone(),
            self.state.done.load(Ordering::Relaxed),
            self.state.total.load(Ordering::Relaxed),
        )
    }

    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.state.cancel.load(Ordering::Relaxed)
    }

    pub fn finished(&mut self) -> Option<Result<Vm, String>> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.handle
                .take()?
                .join()
                .unwrap_or_else(|_| Err("The preparation thread failed.".into())),
        )
    }
}

/// Create `vm` (memory, name and library already set) with `os` installed
/// or its CD attached. Downloads are cached in `cache` for reuse.
pub fn install(os: &'static FreeOs, vm: Vm, share: PathBuf, cache: PathBuf) -> Job {
    spawn(move |worker| {
        let mut vm = vm;
        match &os.media {
            Media::FreeDos => plant(&mut vm, "freedos", &share, worker)?,
            Media::ZippedIso {
                url,
                zip,
                zip_size,
                zip_sha256,
                iso,
            } => {
                fs::create_dir_all(&cache)
                    .map_err(|err| format!("Cannot create {}: {err}", cache.display()))?;
                let iso_path = cache.join(iso);
                if !iso_path.is_file() {
                    let zip_path = cache.join(zip);
                    if !zip_path.is_file() {
                        worker.stage("Downloading", *zip_size);
                        download(url, &zip_path, worker)?;
                    }
                    worker.stage("Checking the download", 0);
                    verify(&zip_path, *zip_size, zip_sha256)?;
                    worker.stage("Unpacking", 0);
                    unzip(&zip_path, iso, &iso_path, worker)?;
                    let _ = fs::remove_file(&zip_path);
                }
                vm.iso = Some(iso_path);
                worker.stage("Creating the VM", 0);
                vm.create_blank()?;
            }
        }
        Ok(vm)
    })
}

/// Create `vm` on a new disk with FreeDOS set up to run the Windows setup
/// (`os_id` "win31", "win95", "win98" or "winme") from the VM's CD on first
/// boot.
pub fn install_windows(os_id: &'static str, vm: Vm, share: PathBuf) -> Job {
    spawn(move |worker| {
        let mut vm = vm;
        plant(&mut vm, os_id, &share, worker)?;
        Ok(vm)
    })
}

fn spawn(work: impl FnOnce(&State) -> Result<Vm, String> + Send + 'static) -> Job {
    let state = Arc::new(State::default());
    let worker = state.clone();
    Job {
        state,
        handle: Some(std::thread::spawn(move || work(&worker))),
    }
}

/// Create `vm` with a new `vm.disk_mib` disk holding FreeDOS from the kit.
fn plant(vm: &mut Vm, os_id: &str, share: &Path, state: &State) -> Result<(), String> {
    state.stage("Installing FreeDOS", 0);
    vm.create_with(|vm| {
        r86::dos_disk::create(&vm.disk, vm.disk_mib, os_id, share, |done, total| {
            state.done.store(done, Ordering::Relaxed);
            state.total.store(total, Ordering::Relaxed);
        })
    })
}

fn download(url: &str, path: &Path, state: &State) -> Result<(), String> {
    let part = path.with_extension("part");
    // -C - resumes an interrupted download of the same file.
    let mut child = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--retry", "3", "--continue-at", "-", "--output"])
        .arg(&part)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Cannot run curl: {err}"))?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            break status;
        }
        if state.check().is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return state.check();
        }
        let size = fs::metadata(&part).map_or(0, |m| m.len());
        state.done.store(size, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        let mut error = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut error);
        }
        return Err(format!(
            "Download failed. Check the network connection.\n{}",
            error.trim()
        ));
    }
    fs::rename(&part, path).map_err(|err| err.to_string())
}

fn verify(path: &Path, size: u64, sha256: &str) -> Result<(), String> {
    let actual = fs::metadata(path).map_err(|err| err.to_string())?.len();
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|err| format!("Cannot run sha256sum: {err}"))?;
    let digest = String::from_utf8_lossy(&output.stdout);
    if actual != size || digest.split_whitespace().next() != Some(sha256) {
        let _ = fs::remove_file(path);
        return Err("The download is damaged and was deleted. Try again.".into());
    }
    Ok(())
}

/// Copy `from` to `to` in 1 MiB chunks, honoring cancellation.
fn copy(mut from: impl Read, to: &Path, state: &State) -> Result<u64, String> {
    let mut out =
        File::create(to).map_err(|err| format!("Cannot create {}: {err}", to.display()))?;
    let mut buffer = vec![0; 1 << 20];
    let mut total = 0;
    loop {
        state.check()?;
        let len = from.read(&mut buffer).map_err(|err| err.to_string())?;
        if len == 0 {
            break;
        }
        out.write_all(&buffer[..len])
            .map_err(|err| err.to_string())?;
        total += len as u64;
        state.done.store(total, Ordering::Relaxed);
    }
    out.sync_all().map_err(|err| err.to_string())?;
    Ok(total)
}

/// Extract one stored or deflated member of a zip archive, checking its CRC.
fn unzip(zip: &Path, member: &str, to: &Path, state: &State) -> Result<(), String> {
    let mut file = File::open(zip).map_err(|err| err.to_string())?;
    let entry = find_member(&mut file, member)?;
    state.total.store(entry.size, Ordering::Relaxed);
    file.seek(SeekFrom::Start(entry.offset))
        .map_err(|err| err.to_string())?;
    let mut header = [0u8; 30];
    file.read_exact(&mut header)
        .map_err(|err| err.to_string())?;
    if header[..4] != *b"PK\x03\x04" {
        return Err("Invalid zip archive".into());
    }
    let skip = u16::from_le_bytes([header[26], header[27]]) as i64
        + u16::from_le_bytes([header[28], header[29]]) as i64;
    file.seek(SeekFrom::Current(skip))
        .map_err(|err| err.to_string())?;
    let data = CrcReader {
        inner: io::BufReader::new(file).take(entry.compressed),
        crc: flate2::Crc::new(),
    };
    let part = to.with_extension("part");
    let (written, crc) = match entry.method {
        0 => {
            let mut data = data;
            let written = copy(&mut data, &part, state)?;
            (written, data.crc.sum())
        }
        8 => {
            let mut inflate = CrcReader {
                inner: DeflateDecoder::new(data),
                crc: flate2::Crc::new(),
            };
            let written = copy(&mut inflate, &part, state)?;
            (written, inflate.crc.sum())
        }
        method => return Err(format!("Unsupported zip compression method {method}")),
    };
    if written != entry.size || crc != entry.crc {
        let _ = fs::remove_file(&part);
        return Err("The archive is damaged. Delete it and try again.".into());
    }
    fs::rename(&part, to).map_err(|err| err.to_string())
}

struct ZipEntry {
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

fn find_member(file: &mut File, member: &str) -> Result<ZipEntry, String> {
    let len = file.metadata().map_err(|err| err.to_string())?.len();
    let tail_len = len.min(65536 + 22);
    let mut tail = vec![0; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|err| err.to_string())?;
    let eocd = tail
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .ok_or("Invalid zip archive")?;
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let count = u16_at(&tail, eocd + 10) as usize;
    let size = u32_at(&tail, eocd + 12) as usize;
    let offset = u64::from(u32_at(&tail, eocd + 16));
    let mut directory = vec![0; size];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut directory))
        .map_err(|err| err.to_string())?;
    let mut pos = 0;
    for _ in 0..count {
        let d = directory
            .get(pos..pos + 46)
            .ok_or("Invalid zip directory")?;
        if d[..4] != *b"PK\x01\x02" {
            return Err("Invalid zip directory".into());
        }
        let name_len = u16_at(d, 28) as usize;
        let next = pos + 46 + name_len + u16_at(d, 30) as usize + u16_at(d, 32) as usize;
        let name = directory
            .get(pos + 46..pos + 46 + name_len)
            .unwrap_or_default();
        if name == member.as_bytes() {
            return Ok(ZipEntry {
                method: u16_at(d, 10),
                crc: u32_at(d, 16),
                compressed: u64::from(u32_at(d, 20)),
                size: u64::from(u32_at(d, 24)),
                offset: u64::from(u32_at(d, 42)),
            });
        }
        pos = next;
    }
    Err(format!("{member} is missing from the archive"))
}

struct CrcReader<R> {
    inner: R,
    crc: flate2::Crc,
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buf)?;
        self.crc.update(&buf[..len]);
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    #[test]
    fn deflated_zip_member_extracts_with_crc_check() {
        let dir = std::env::temp_dir().join(format!("r86-catalog-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut crc = flate2::Crc::new();
        crc.update(&payload);
        let name = b"disc.iso";
        let mut zip = Vec::new();
        let local = |zip: &mut Vec<u8>| {
            zip.extend_from_slice(b"PK\x03\x04");
            zip.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
            zip.extend_from_slice(&crc.sum().to_le_bytes());
            zip.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
            zip.extend_from_slice(&[0, 0]);
            zip.extend_from_slice(name);
        };
        local(&mut zip);
        zip.extend_from_slice(&compressed);
        let directory = zip.len();
        zip.extend_from_slice(b"PK\x01\x02");
        zip.extend_from_slice(&[20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
        zip.extend_from_slice(&crc.sum().to_le_bytes());
        zip.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&[0; 12]);
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(name);
        let directory_size = zip.len() - directory;
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        zip.extend_from_slice(&(directory_size as u32).to_le_bytes());
        zip.extend_from_slice(&(directory as u32).to_le_bytes());
        zip.extend_from_slice(&[0, 0]);
        let zip_path = dir.join("test.zip");
        fs::write(&zip_path, &zip).unwrap();
        let out = dir.join("disc.iso");
        let state = State::default();
        unzip(&zip_path, "disc.iso", &out, &state).unwrap();
        assert_eq!(fs::read(&out).unwrap(), payload);
        assert!(unzip(&zip_path, "other.iso", &out, &state).is_err());
        // A flipped data byte fails the CRC and leaves no output behind.
        zip[60] ^= 0x55;
        fs::write(&zip_path, &zip).unwrap();
        fs::remove_file(&out).unwrap();
        assert!(unzip(&zip_path, "disc.iso", &out, &state).is_err());
        assert!(!out.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
