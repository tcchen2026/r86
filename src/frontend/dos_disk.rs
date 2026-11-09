//! Plant the bundled DOS kit, `share/dos.zip`, in a new, sparse VHD using the
//! shared FAT adapter. Guest sectors and DOS dates are deterministic; the VHD
//! container has its own creation time and identity. The kit supplies FreeDOS
//! bootstrap code.

use crate::disk::{Disk, MAX_SECTORS};
use crate::fat::{self, Partition, Volume};
use fatfs::{Dir, FatType, FormatVolumeOptions};
use flate2::Crc;
use flate2::read::DeflateDecoder;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MIB: u64 = 1024 * 1024;
const PART_START: u32 = 63;
const CYLINDER: u32 = 16 * 63;
const ROOT_ENTRIES: usize = 512;
const CHUNK: usize = 64 * 1024;
const SERIAL: u32 = 0x5238_3644;

/// A stored or deflated file in the kit archive.
#[derive(Clone, Copy)]
struct Member {
    header: u64,
    deflated: bool,
    packed: u64,
    size: u32,
    crc: u32,
}

struct Entry {
    name: String,
    member: Option<Member>, // None is a directory.
    children: Vec<usize>,
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The kit as a zip archive: stored or deflated members, no ZIP64 or
/// encryption. Directory members end in '/'; folders may also be implied.
struct Kit {
    file: File,
    members: BTreeMap<String, Member>,
}

impl Kit {
    fn open(path: &Path) -> Result<Self, String> {
        let fail = |what: &dyn std::fmt::Display| format!("{}: {what}", path.display());
        let mut file = File::open(path).map_err(|e| fail(&e))?;
        let len = file.metadata().map_err(|e| fail(&e))?.len();
        let mut end = vec![0; len.min(22 + 0xffff) as usize];
        file.seek(SeekFrom::End(-(end.len() as i64)))
            .and_then(|_| file.read_exact(&mut end))
            .map_err(|e| fail(&e))?;
        let at = (0..end.len().saturating_sub(21))
            .rev()
            .find(|&i| end[i..i + 4] == *b"PK\x05\x06")
            .ok_or_else(|| fail(&"not a zip archive"))?;
        let eocd = &end[at..];
        let (count, size, offset) = (le16(eocd, 10), le32(eocd, 12), le32(eocd, 16));
        if le16(eocd, 4) != 0 || le16(eocd, 6) != 0 || count == 0xffff || offset == u32::MAX {
            return Err(fail(&"multi-part and ZIP64 archives are not supported"));
        }
        if u64::from(offset) + u64::from(size) > len {
            return Err(fail(&"truncated central directory"));
        }
        let mut directory = vec![0; size as usize];
        file.seek(SeekFrom::Start(offset.into()))
            .and_then(|_| file.read_exact(&mut directory))
            .map_err(|e| fail(&e))?;
        let mut members = BTreeMap::new();
        let mut at = 0;
        for _ in 0..count {
            let header = directory
                .get(at..at + 46)
                .filter(|h| h[..4] == *b"PK\x01\x02")
                .ok_or_else(|| fail(&"damaged central directory"))?;
            let name_end = at + 46 + usize::from(le16(header, 28));
            let name = directory
                .get(at + 46..name_end)
                .and_then(|name| std::str::from_utf8(name).ok())
                .filter(|name| name.is_ascii())
                .ok_or_else(|| fail(&"damaged or non-ASCII member name"))?
                .to_owned();
            let (flags, method) = (le16(header, 8), le16(header, 10));
            if flags & 1 != 0 || !matches!(method, 0 | 8) {
                return Err(fail(&format!(
                    "{name}: encrypted or unsupported compression"
                )));
            }
            let member = Member {
                header: le32(header, 42).into(),
                deflated: method == 8,
                packed: le32(header, 20).into(),
                size: le32(header, 24),
                crc: le32(header, 16),
            };
            at = name_end + usize::from(le16(header, 30)) + usize::from(le16(header, 32));
            if members.insert(name.clone(), member).is_some() {
                return Err(fail(&format!("{name}: duplicate member")));
            }
        }
        Ok(Self { file, members })
    }

    /// Passes a member's bytes to `sink` a chunk at a time, then checks its CRC.
    fn copy(
        &mut self,
        name: &str,
        member: Member,
        chunk: &mut [u8],
        mut sink: impl FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let damaged = |what: &dyn std::fmt::Display| format!("DOS kit {name}: {what}");
        let mut header = [0; 30];
        self.file
            .seek(SeekFrom::Start(member.header))
            .and_then(|_| self.file.read_exact(&mut header))
            .map_err(|e| damaged(&e))?;
        if header[..4] != *b"PK\x03\x04" {
            return Err(damaged(&"bad local header"));
        }
        let skip = i64::from(le16(&header, 26)) + i64::from(le16(&header, 28));
        self.file
            .seek(SeekFrom::Current(skip))
            .map_err(|e| damaged(&e))?;
        let packed = (&mut self.file).take(member.packed);
        let mut data: Box<dyn Read + '_> = if member.deflated {
            Box::new(DeflateDecoder::new(packed))
        } else {
            Box::new(packed)
        };
        let mut crc = Crc::new();
        let mut remaining = member.size as usize;
        while remaining != 0 {
            let len = remaining.min(chunk.len());
            data.read_exact(&mut chunk[..len])
                .map_err(|e| damaged(&e))?;
            crc.update(&chunk[..len]);
            sink(&chunk[..len])?;
            remaining -= len;
        }
        if crc.sum() != member.crc {
            return Err(damaged(&"CRC mismatch"));
        }
        Ok(())
    }
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

/// The kit folder `folder/` by upper-case DOS path; None marks a directory.
fn read_tree(kit: &Kit, folder: &str) -> Result<BTreeMap<String, Option<Member>>, String> {
    let prefix = format!("{folder}/");
    let mut tree = BTreeMap::new();
    let mut found = false;
    for (name, &member) in kit
        .members
        .range(prefix.clone()..)
        .take_while(|(name, _)| name.starts_with(&prefix))
    {
        found = true;
        let path = &name[prefix.len()..];
        let (path, directory) = path.strip_suffix('/').map_or((path, false), |p| (p, true));
        if path.is_empty() {
            continue;
        }
        let parts: Vec<_> = path.split('/').collect();
        let mut key = String::new();
        for (i, part) in parts.iter().enumerate() {
            validate_name(part)?;
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(&part.to_ascii_uppercase());
            let file = !directory && i + 1 == parts.len();
            match tree.get(&key) {
                Some(None) if !file => {}
                Some(_) => return Err(format!("{key}: duplicate DOS name")),
                None => {
                    tree.insert(key.clone(), file.then_some(member));
                }
            }
        }
    }
    if !found {
        return Err(format!("DOS kit has no {prefix} folder"));
    }
    Ok(tree)
}

fn entries(kit: &Kit, os_id: &str) -> Result<Vec<Entry>, String> {
    let mut base = read_tree(kit, "base")?;
    for (name, member) in read_tree(kit, os_id)? {
        if let Some(old) = base.get(&name) {
            if old.is_some() != member.is_some() {
                return Err(format!("{name}: overlay changes file/directory type"));
            }
        }
        base.insert(name, member);
    }
    let kernel = base
        .remove("KERNEL.SYS")
        .ok_or("DOS kit has no KERNEL.SYS")?;
    if !matches!(kernel, Some(Member { size: 1.., .. })) {
        return Err("DOS kit KERNEL.SYS must be a nonempty file".into());
    }
    let mut result = vec![Entry {
        name: String::new(),
        member: None,
        children: Vec::new(),
    }];
    let mut indices = BTreeMap::from([(String::new(), 0)]);
    for (path, member) in std::iter::once(("KERNEL.SYS".into(), kernel)).chain(base) {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", &path));
        let parent = indices[parent];
        let index = result.len();
        result[parent].children.push(index);
        result.push(Entry {
            name: name.to_owned(),
            member,
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
            let bytes = if let Some(member) = entry.member {
                u64::from(member.size)
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

fn template(kit: &mut Kit, name: &str) -> Result<[u8; 512], String> {
    let member = kit
        .members
        .get(name)
        .copied()
        .filter(|member| member.size == 512)
        .ok_or_else(|| format!("DOS kit {name}: expected a 512-byte boot template"))?;
    let mut bytes = [0; 512];
    kit.copy(name, member, &mut [0; 512], |data| {
        bytes.copy_from_slice(data);
        Ok(())
    })?;
    if bytes[510..] != [0x55, 0xaa] {
        return Err(format!("DOS kit {name}: missing boot signature"));
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
    kit: &mut Kit,
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
        let name = &entry.name;
        if let Some(member) = entry.member {
            let mut target = directory
                .create_file(name)
                .map_err(|e| format!("creating {name}: {e}"))?;
            kit.copy(name, member, chunk, |data| {
                target
                    .write_all(data)
                    .map_err(|e| format!("writing {name}: {e}"))?;
                progress.done += data.len() as u64;
                if progress.done < progress.total {
                    (progress.callback)(progress.done, progress.total);
                }
                Ok(())
            })?;
            // Report directory-entry write errors instead of relying on Drop.
            target.flush().map_err(|e| format!("closing {name}: {e}"))?;
        } else {
            let child = directory
                .create_dir(name)
                .map_err(|e| format!("creating {name}: {e}"))?;
            write_tree(kit, child, entries, index, chunk, progress)?;
        }
    }
    directory
        .finish()
        .map_err(|e| format!("closing DOS directory: {e}"))
}

/// Create an exact-size VHD from the kit's `base/` plus the selected overlay.
/// `freedos`, `win31`, and `win95` use FAT16 (3..=2047 MiB); `win98` and `winme`
/// use FAT32 (33..=131072 MiB). The kit must also fit. Generic `dos` is rejected.
///
/// Names, templates and sizes are checked before creating the destination,
/// which must not exist. Exact capacity is checked against the formatted
/// filesystem; an insufficient-capacity result removes the new disk. File data
/// is checked against its CRC while copying. That and other I/O failures can
/// leave an incomplete destination for the caller to remove. Progress counts
/// file bytes copied; completion follows unmount and a durable flush.
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
    let mut kit = Kit::open(&share.join("dos.zip"))?;
    let mbr = mbr(
        template(&mut kit, "boot/mbr.bin")?,
        partition_sectors,
        fat32,
    );
    let boot = template(
        &mut kit,
        if fat32 {
            "boot/fat32.bin"
        } else {
            "boot/fat16.bin"
        },
    )?;
    let entries = entries(&kit, os_id)?;
    if !fat32 && directory_slots(&entries[0], true) > ROOT_ENTRIES {
        return Err("FAT16 root directory exceeds 512 entries".into());
    }
    let total: u64 = entries
        .iter()
        .filter_map(|e| e.member)
        .map(|member| u64::from(member.size))
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
    write_tree(
        &mut kit,
        filesystem.root_dir(),
        &entries,
        0,
        &mut chunk,
        &mut copied,
    )?;
    filesystem
        .unmount()
        .map_err(|e| format!("unmounting DOS disk: {e}"))?;
    disk.flush()
        .map_err(|e| format!("flushing DOS disk: {e}"))?;
    progress(total, total);
    Ok(())
}
