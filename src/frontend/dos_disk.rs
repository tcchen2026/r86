//! Plant a bundled DOS tree in a new, sparse VHD using the shared FAT adapter.
//! Guest sectors and DOS dates are deterministic; the VHD container has its
//! own creation time and identity. The kit supplies FreeDOS bootstrap code.

use crate::disk::{Disk, MAX_SECTORS};
use crate::fat::{self, Partition, Volume};
use fatfs::{Dir, FatType, FormatVolumeOptions};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;
const PART_START: u32 = 63;
const CYLINDER: u32 = 16 * 63;
const ROOT_ENTRIES: usize = 512;
const CHUNK: usize = 64 * 1024;
const SERIAL: u32 = 0x5238_3644;

struct Source {
    path: PathBuf,
    size: Option<u32>, // None is a directory.
}

struct Entry {
    name: String,
    source: Source,
    children: Vec<usize>,
}

fn directory(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(format!(
            "{}: expected a directory, not a link",
            path.display()
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    let upper = name.to_ascii_uppercase();
    let (base, ext) = upper.split_once('.').unwrap_or((&upper, ""));
    let legal =
        |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit() || b"!#$%&'()-@^_`{}~".contains(&b);
    if base.is_empty()
        || base.len() > 8
        || ext.len() > 3
        || (upper.contains('.') && ext.is_empty())
        || !base.bytes().chain(ext.bytes()).all(legal)
    {
        return Err(format!("{name:?}: expected a DOS 8.3 name"));
    }
    Ok(())
}

fn read_tree(path: &Path, prefix: &str, tree: &mut BTreeMap<String, Source>) -> Result<(), String> {
    directory(path)?;
    for child in fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
        let child = child.map_err(|e| e.to_string())?;
        let path = child.path();
        let name = child.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("{}: expected an ASCII DOS name", path.display()))?;
        validate_name(name)?;
        let key = format!("{prefix}{}", name.to_ascii_uppercase());
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let size = if meta.is_file() {
            Some(
                u32::try_from(meta.len())
                    .map_err(|_| format!("{}: exceeds 4 GiB", path.display()))?,
            )
        } else if meta.is_dir() {
            None
        } else {
            return Err(format!(
                "{}: links and special files are not supported",
                path.display()
            ));
        };
        if tree
            .insert(
                key.clone(),
                Source {
                    path: path.clone(),
                    size,
                },
            )
            .is_some()
        {
            return Err(format!("{key}: duplicate DOS name"));
        }
        if size.is_none() {
            read_tree(&path, &format!("{key}/"), tree)?;
        }
    }
    Ok(())
}

fn entries(kit: &Path, os_id: &str) -> Result<Vec<Entry>, String> {
    let mut base = BTreeMap::new();
    let mut overlay = BTreeMap::new();
    read_tree(&kit.join("base"), "", &mut base)?;
    read_tree(&kit.join(os_id), "", &mut overlay)?;
    for (name, source) in overlay {
        if let Some(old) = base.get(&name) {
            if old.size.is_some() != source.size.is_some() {
                return Err(format!("{name}: overlay changes file/directory type"));
            }
        }
        base.insert(name, source);
    }
    let kernel = base
        .remove("KERNEL.SYS")
        .ok_or("DOS kit has no KERNEL.SYS")?;
    if !matches!(kernel.size, Some(1..)) {
        return Err("DOS kit KERNEL.SYS must be a nonempty file".into());
    }
    let mut result = vec![Entry {
        name: String::new(),
        source: Source {
            path: kit.to_owned(),
            size: None,
        },
        children: Vec::new(),
    }];
    let mut indices = BTreeMap::from([(String::new(), 0)]);
    for (path, source) in std::iter::once(("KERNEL.SYS".into(), kernel)).chain(base) {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", &path));
        let parent = indices[parent];
        let index = result.len();
        result[parent].children.push(index);
        result.push(Entry {
            name: name.to_owned(),
            source,
            children: Vec::new(),
        });
        indices.insert(path, index);
    }
    Ok(result)
}

// The bulk writer emits one SFN per validated name and two dot entries per
// subdirectory. A full directory needs no extra zero terminator slot.
fn directory_slots(entry: &Entry, root: bool) -> usize {
    entry.children.len() + if root { 0 } else { 2 }
}

fn clusters_needed(entries: &[Entry], cluster_bytes: u32, fat32: bool) -> u64 {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let bytes = if let Some(size) = entry.source.size {
                u64::from(size)
            } else if index == 0 && !fat32 {
                return 0; // FAT16 reserves its fixed root outside the cluster pool.
            } else {
                directory_slots(entry, index == 0) as u64 * 32
            };
            let count = bytes.div_ceil(u64::from(cluster_bytes));
            // The formatter already allocated the FAT32 root's first cluster.
            count - u64::from(index == 0 && fat32)
        })
        .sum()
}

fn template(path: &Path) -> Result<[u8; 512], String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() != 512 {
        return Err(format!(
            "{}: expected a 512-byte boot template",
            path.display()
        ));
    }
    let mut bytes = [0; 512];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes[510..] != [0x55, 0xaa] {
        return Err(format!("{}: missing boot signature", path.display()));
    }
    Ok(bytes)
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn chs(lba: u32) -> [u8; 3] {
    let lba = lba.min(1024 * CYLINDER - 1);
    let cylinder = lba / CYLINDER;
    let head = lba % CYLINDER / 63;
    let sector = lba % 63 + 1;
    [
        head as u8,
        (sector | ((cylinder >> 2) & 0xc0)) as u8,
        cylinder as u8,
    ]
}

fn mbr(mut boot: [u8; 512], sectors: u32, fat32: bool) -> [u8; 512] {
    boot[440..510].fill(0);
    put32(&mut boot, 440, SERIAL);
    boot[446] = 0x80;
    boot[447..450].copy_from_slice(&chs(PART_START));
    // EDD avoids depending on BIOS CHS translation above 504 MiB.
    boot[450] = if fat32 { 0x0c } else { 0x0e };
    boot[451..454].copy_from_slice(&chs(PART_START + sectors - 1));
    put32(&mut boot, 454, PART_START);
    put32(&mut boot, 458, sectors);
    boot
}

fn install_boot(
    disk: &mut Disk,
    mbr: &[u8; 512],
    code: &[u8; 512],
    fat32: bool,
) -> Result<(), String> {
    let mut boot = [0; 512];
    disk.read(u64::from(PART_START), &mut boot)
        .map_err(|e| format!("reading formatted DOS boot sector: {e}"))?;
    boot[..3].copy_from_slice(&code[..3]);
    let code_start = if fat32 { 90 } else { 62 };
    boot[code_start..].copy_from_slice(&code[code_start..]);
    disk.write(0, mbr)
        .and_then(|()| disk.write(u64::from(PART_START), &boot))
        .map_err(|e| format!("installing DOS boot code: {e}"))?;
    if fat32 {
        let backup = u16::from_le_bytes([boot[50], boot[51]]);
        disk.write(u64::from(PART_START) + u64::from(backup), &boot)
            .map_err(|e| format!("installing backup DOS boot code: {e}"))?;
    }
    Ok(())
}

struct CopyProgress<'a, F> {
    done: u64,
    total: u64,
    callback: &'a mut F,
}

fn write_tree<F: FnMut(u64, u64)>(
    directory: Dir<'_, Partition<'_>>,
    entries: &[Entry],
    parent: usize,
    chunk: &mut [u8],
    progress: &mut CopyProgress<'_, F>,
) -> Result<(), String> {
    // Source-tree validation established unique canonical 8.3 names. The new
    // directory has no other writer, so keep its cursor instead of rescanning.
    let mut directory = directory
        .into_short_name_writer()
        .map_err(|e| format!("opening empty DOS directory: {e}"))?;
    for &index in &entries[parent].children {
        let entry = &entries[index];
        let path = &entry.source.path;
        if let Some(size) = entry.source.size {
            let mut source = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if source.metadata().map_err(|e| e.to_string())?.len() != u64::from(size) {
                return Err(format!("{}: kit file changed size", path.display()));
            }
            let mut target = directory
                .create_file(&entry.name)
                .map_err(|e| format!("creating {}: {e}", path.display()))?;
            let mut remaining = size as usize;
            while remaining != 0 {
                let len = remaining.min(chunk.len());
                source
                    .read_exact(&mut chunk[..len])
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                target
                    .write_all(&chunk[..len])
                    .map_err(|e| format!("writing {}: {e}", path.display()))?;
                remaining -= len;
                progress.done += len as u64;
                if progress.done < progress.total {
                    (progress.callback)(progress.done, progress.total);
                }
            }
            // Report directory-entry write errors instead of relying on Drop.
            target
                .flush()
                .map_err(|e| format!("closing {}: {e}", path.display()))?;
        } else {
            let child = directory
                .create_dir(&entry.name)
                .map_err(|e| format!("creating {}: {e}", path.display()))?;
            write_tree(child, entries, index, chunk, progress)?;
        }
    }
    directory
        .finish()
        .map_err(|e| format!("closing DOS directory: {e}"))
}

/// Create an exact-size VHD from `share/dos/base` plus the selected overlay.
/// `freedos`, `win31`, and `win95` use FAT16 (3..=2047 MiB); `win98` and `winme`
/// use FAT32 (33..=131072 MiB). The kit must also fit. Generic `dos` is rejected.
///
/// Names, links, templates and source sizes are checked before creating the
/// destination, which must not exist. Exact capacity is checked against the
/// formatted filesystem; an insufficient-capacity result removes the new disk.
/// Kit files must remain unchanged until completion. Other I/O failures can
/// leave an incomplete destination for the caller to remove. Progress counts
/// source file bytes copied; completion follows unmount and a durable flush.
/// The callback does not support cancellation.
pub fn create(
    path: &Path,
    size_mib: u64,
    os_id: &str,
    share: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    let fat32 = match os_id {
        "freedos" | "win31" | "win95" => false,
        "win98" | "winme" => true,
        _ => return Err(format!("{os_id:?}: unsupported DOS kit")),
    };
    if size_mib == 0 || size_mib > if fat32 { MAX_SECTORS / 2048 } else { 2047 } {
        return Err(format!("invalid disk size for {os_id}: {size_mib} MiB"));
    }
    let minimum = if fat32 { 33 } else { 3 };
    if size_mib < minimum {
        return Err(format!(
            "disk is too small (minimum {minimum} MiB before kit files)"
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(format!("{}: destination already exists", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let sectors = (size_mib * 2048) as u32;
    let partition_sectors = sectors / CYLINDER * CYLINDER - PART_START;
    let kit = share.join("dos");
    directory(&kit)?;
    directory(&kit.join("boot"))?;
    let mbr = mbr(
        template(&kit.join("boot/mbr.bin"))?,
        partition_sectors,
        fat32,
    );
    let boot = template(&kit.join(if fat32 {
        "boot/fat32.bin"
    } else {
        "boot/fat16.bin"
    }))?;
    let entries = entries(&kit, os_id)?;
    if !fat32 && directory_slots(&entries[0], true) > ROOT_ENTRIES {
        return Err("FAT16 root directory exceeds 512 entries".into());
    }
    let total: u64 = entries
        .iter()
        .filter_map(|e| e.source.size)
        .map(u64::from)
        .sum();
    if total > u64::from(partition_sectors) * 512 {
        return Err("DOS kit does not fit the selected disk size".into());
    }
    let kind = if fat32 {
        FatType::Fat32
    } else {
        FatType::Fat16
    };
    let mut format = FormatVolumeOptions::new()
        .fat_type(kind)
        .bytes_per_sector(512)
        .total_sectors(partition_sectors)
        .hidden_sectors(PART_START)
        .max_root_dir_entries(ROOT_ENTRIES as u16)
        .fats(2)
        .media(0xf8)
        .sectors_per_track(63)
        .heads(16)
        .drive_num(0x80)
        .volume_id(SERIAL);
    if !fat32 {
        // Include the 3 MiB FAT16 case; fatfs's automatic choice uses FAT12
        // there. Grow clusters at powers of two to stay below 65525 clusters.
        let spc = partition_sectors.div_ceil(65536).next_power_of_two();
        format = format.bytes_per_cluster(spc * 512);
    }
    Disk::create_vhd(path, size_mib * MIB).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut disk = Disk::open(path, true).map_err(|e| format!("{}: {e}", path.display()))?;
    fatfs::format_volume(
        Partition::new(
            &mut disk,
            u64::from(PART_START),
            u64::from(partition_sectors),
        )
        .map_err(|e| e.to_string())?,
        format,
    )
    .map_err(|e| format!("formatting DOS partition: {e}"))?;
    install_boot(&mut disk, &mbr, &boot, fat32)?;
    let volume = Volume {
        letter: 'C',
        fat: true,
        partition_type: if fat32 { 0x0c } else { 0x0e },
        start_lba: u64::from(PART_START),
        sectors: u64::from(partition_sectors),
    };
    let filesystem = fat::mount_with_options(&mut disk, &volume, fat::deterministic_options())?;
    let stats = filesystem
        .stats()
        .map_err(|e| format!("checking DOS capacity: {e}"))?;
    if clusters_needed(&entries, stats.cluster_size(), fat32) > u64::from(stats.free_clusters()) {
        drop(filesystem);
        drop(disk);
        fs::remove_file(path).map_err(|e| format!("removing insufficient DOS disk: {e}"))?;
        return Err("DOS kit does not fit the selected disk size".into());
    }
    progress(0, total);
    let mut copied = CopyProgress {
        done: 0,
        total,
        callback: &mut progress,
    };
    let mut chunk = vec![0; CHUNK];
    write_tree(filesystem.root_dir(), &entries, 0, &mut chunk, &mut copied)?;
    filesystem
        .unmount()
        .map_err(|e| format!("unmounting DOS disk: {e}"))?;
    disk.flush()
        .map_err(|e| format!("flushing DOS disk: {e}"))?;
    progress(total, total);
    Ok(())
}
