//! File management for a powered-off VM's hard disk: the FAT drives (C:, D:
//! ...) of its first disk, lettered as DOS does. Every call mounts its drive,
//! does one operation and unmounts, so the image is consistent between calls.
//! Callers keep the guest off meanwhile: a running guest caches its FAT.
//!
//! Paths look like `C:/GAMES/PAL`; `\` also separates. Replies are JSON for the
//! remote console (see script/ws_cli.py).
use std::io::{self, Read, Seek, SeekFrom, Write};

use r86::disk::Disk;
use r86::fat::{self, Partition};

type Dir<'fs, 'disk> = fatfs::Dir<'fs, Partition<'disk>>;

// Bound fatfs path recursion and recursive deletion of corrupt directory trees.
const MAX_PATH_COMPONENTS: usize = 128;

/// `[{"letter":"C","fat":true,"bytes":N,"type":"Fat16","label":"..","total":N,"free":N}]`
pub fn drives(disk: &mut Disk) -> String {
    let mut items = Vec::new();
    for volume in fat::volumes(disk) {
        let mut fields = format!(
            "\"letter\":\"{}\",\"fat\":{},\"bytes\":{}",
            volume.letter,
            volume.fat,
            volume.sectors * 512
        );
        if volume.fat {
            let info = fat::mount(disk, &volume).and_then(|fs| {
                let stats = fs.stats().map_err(|err| err.to_string())?;
                let cluster = u64::from(stats.cluster_size());
                Ok(format!(
                    ",\"type\":\"{:?}\",\"label\":{},\"total\":{},\"free\":{}",
                    fs.fat_type(),
                    json(fs.volume_label().trim()),
                    u64::from(stats.total_clusters()) * cluster,
                    u64::from(stats.free_clusters()) * cluster,
                ))
            });
            fields += &info.unwrap_or_else(|err| format!(",\"error\":{}", json(&err)));
        }
        items.push(format!("{{{fields}}}"));
    }
    format!("[{}]", items.join(","))
}

/// `[{"name":"..","short":"8.3","dir":bool,"size":N,"date":"YYYY-MM-DD hh:mm"}]`
pub fn list(disk: &mut Disk, path: &str) -> Result<String, String> {
    on_drive(disk, path, |root, path, _| {
        let dir = if path.is_empty() {
            root.clone()
        } else {
            root.open_dir(path)?
        };
        let mut items = Vec::new();
        for entry in dir.iter() {
            let entry = entry?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let m = entry.modified();
            items.push(format!(
                "{{\"name\":{},\"short\":{},\"dir\":{},\"size\":{},\"date\":\"{:04}-{:02}-{:02} {:02}:{:02}\"}}",
                json(&name),
                json(&entry.short_file_name()),
                entry.is_dir(),
                entry.len(),
                m.date.year,
                m.date.month,
                m.date.day,
                m.time.hour,
                m.time.min,
            ));
        }
        Ok(format!("[{}]", items.join(",")))
    })
}

pub fn read(disk: &mut Disk, path: &str) -> Result<Vec<u8>, String> {
    on_drive(disk, path, |root, path, _| {
        let mut file = root.open_file(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(data)
    })
}

/// Creates or empties a file of `size` bytes to come, creating missing
/// folders; fails at once when its drive lacks the space. `append` fills it,
/// so an upload never waits in host memory.
pub fn create(disk: &mut Disk, path: &str, size: u64) -> Result<(), String> {
    let (letter, _) = split_path(path)?;
    let volume = fat::volumes(disk)
        .into_iter()
        .find(|v| v.letter == letter && v.fat)
        .ok_or_else(|| format!("No FAT drive {letter}:"))?;
    let stats = fat::mount(disk, &volume)?
        .stats()
        .map_err(|err| err.to_string())?;
    let free = u64::from(stats.free_clusters()) * u64::from(stats.cluster_size());
    if size > free {
        return Err(format!(
            "Not enough space on {letter}: {} KB free.",
            free >> 10
        ));
    }
    on_drive(disk, path, |root, _, parts| {
        let Some((name, folders)) = parts.split_last() else {
            return Err(io::Error::other("Missing file name"));
        };
        let mut dir = root.clone();
        for folder in folders {
            dir = match dir.open_dir(folder) {
                Ok(next) => next,
                Err(_) => {
                    validate_name(folder).map_err(io::Error::other)?;
                    dir.create_dir(folder)?
                }
            };
        }
        let mut file = dir.create_file(name)?;
        file.truncate()?;
        file.flush()
    })
}

/// Adds `data` to the end of a file made by `create`.
pub fn append(disk: &mut Disk, path: &str, data: &[u8]) -> Result<(), String> {
    on_drive(disk, path, |root, path, _| {
        let mut file = root.open_file(path)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(data)?;
        file.flush()
    })
}

pub fn mkdir(disk: &mut Disk, path: &str) -> Result<(), String> {
    on_drive(disk, path, |root, path, parts| {
        let name = parts
            .last()
            .ok_or_else(|| io::Error::other("Missing directory name"))?;
        validate_name(name).map_err(io::Error::other)?;
        root.create_dir(path).map(drop)
    })
}

/// Deletes a file, or a folder with everything in it.
pub fn delete(disk: &mut Disk, path: &str) -> Result<(), String> {
    on_drive(disk, path, |root, path, _| {
        if path.is_empty() {
            return Err(io::Error::other("Cannot delete a whole drive"));
        }
        remove_tree(root, path)
    })
}

/// Renames within the same folder.
pub fn rename(disk: &mut Disk, path: &str, to: &str) -> Result<(), String> {
    validate_name(to)?;
    on_drive(disk, path, |root, path, parts| {
        let parent = parts[..parts.len().saturating_sub(1)].join("/");
        let target = if parent.is_empty() {
            to.to_string()
        } else {
            format!("{parent}/{to}")
        };
        root.rename(path, root, &target)
    })
}

/// Mounts the drive named by `path` and runs `op` with its root, the path
/// inside the drive and its components.
fn on_drive<T>(
    disk: &mut Disk,
    path: &str,
    op: impl FnOnce(&Dir<'_, '_>, &str, &[String]) -> io::Result<T>,
) -> Result<T, String> {
    let (letter, parts) = split_path(path)?;
    let volume = fat::volumes(disk)
        .into_iter()
        .find(|v| v.letter == letter)
        .ok_or_else(|| format!("No drive {letter}:"))?;
    if !volume.fat {
        return Err(format!("Drive {letter}: is not a FAT drive"));
    }
    let inner = parts.join("/");
    let fs = fat::mount(disk, &volume)?;
    let root = fs.root_dir();
    let result = op(&root, &inner, &parts).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => format!("Not found: {letter}:/{inner}"),
        _ => err.to_string(),
    });
    drop(root);
    fs.unmount().map_err(|err| err.to_string())?;
    result
}

fn remove_tree(root: &Dir<'_, '_>, path: &str) -> io::Result<()> {
    if path.split('/').count() > MAX_PATH_COMPONENTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Directory tree is too deep or cyclic",
        ));
    }
    if let Ok(dir) = root.open_dir(path) {
        let mut children = Vec::new();
        for entry in dir.iter() {
            let name = entry?.file_name();
            if name != "." && name != ".." {
                children.push(name);
            }
        }
        for child in children {
            remove_tree(root, &format!("{path}/{child}"))?;
        }
    }
    root.remove(path)
}

/// "C:/GAMES/PAL" -> ('C', ["GAMES", "PAL"]). No empty, "." or ".." parts.
fn split_path(path: &str) -> Result<(char, Vec<String>), String> {
    let mut chars = path.chars();
    let letter = chars
        .next()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase());
    let rest = chars.as_str().strip_prefix(':');
    let (Some(letter), Some(rest)) = (letter, rest) else {
        return Err("A path starts with a drive letter".into());
    };
    let parts: Vec<String> = rest
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    if parts.len() > MAX_PATH_COMPONENTS {
        return Err(format!("Path exceeds {MAX_PATH_COMPONENTS} components"));
    }
    if parts.iter().any(|part| part == "." || part == "..") {
        return Err("Invalid path".into());
    }
    Ok((letter, parts))
}

// fatfs 0.3.6 checks names after some mutations. Match its 255 UTF-8-byte,
// BMP-only limit and allowed characters before allocating or deleting entries.
fn validate_name(name: &str) -> Result<(), String> {
    if matches!(name, "" | "." | "..")
        || name.len() > 255
        || name
            .chars()
            .any(|c| c < ' ' || c == '\x7f' || c > '\u{ffff}' || "\"*/:<>?\\|".contains(c))
    {
        return Err("Invalid FAT file name".into());
    }
    Ok(())
}

fn json(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write(disk: &mut Disk, path: &str, data: &[u8]) -> Result<(), String> {
        create(disk, path, data.len() as u64)?;
        append(disk, path, data)
    }

    fn fat16_disk() -> Disk {
        let mut image = vec![0; 32 << 20];
        fatfs::format_volume(
            Cursor::new(&mut image),
            fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
        )
        .unwrap();
        Disk::memory(image, true)
    }

    fn free_clusters(disk: &mut Disk) -> u32 {
        let volume = fat::volumes(disk)[0];
        let fs = fat::mount(disk, &volume).unwrap();
        let count = fs.stats().unwrap().free_clusters();
        fs.unmount().unwrap();
        count
    }

    #[test]
    fn uploads_append_in_chunks_after_a_space_check() {
        let mut disk = fat16_disk();
        assert!(
            create(&mut disk, "C:/BIG.BIN", 64 << 20)
                .unwrap_err()
                .starts_with("Not enough space on C:")
        );
        create(&mut disk, "C:/NEW/DATA.BIN", 3000).unwrap();
        for chunk in [[1u8; 1000], [2; 1000], [3; 1000]] {
            append(&mut disk, "C:/NEW/DATA.BIN", &chunk).unwrap();
        }
        let data = read(&mut disk, "C:/NEW/DATA.BIN").unwrap();
        assert_eq!(data.len(), 3000);
        assert!(data[..1000].iter().all(|&b| b == 1) && data[2000..].iter().all(|&b| b == 3));
        // Creating again empties it for a new upload.
        create(&mut disk, "C:/NEW/DATA.BIN", 10).unwrap();
        assert!(read(&mut disk, "C:/NEW/DATA.BIN").unwrap().is_empty());
    }

    #[test]
    fn fat16_rejected_rename_preserves_file() {
        let mut disk = fat16_disk();
        let data = b"keep this data";
        write(&mut disk, "C:/ORIGINAL.TXT", data).unwrap();
        let before = free_clusters(&mut disk);
        let too_long = "名".repeat(86); // fatfs measures bytes, not UCS-2 units.
        for name in ["bad:name", too_long.as_str(), "bad😀", ".", ".."] {
            assert!(rename(&mut disk, "C:/ORIGINAL.TXT", name).is_err());
            assert_eq!(read(&mut disk, "C:/ORIGINAL.TXT").unwrap(), data);
            assert_eq!(free_clusters(&mut disk), before);
        }
        rename(&mut disk, "C:/ORIGINAL.TXT", "メモ notes.txt").unwrap();
        assert_eq!(read(&mut disk, "C:/メモ notes.txt").unwrap(), data);
    }

    #[test]
    fn fat16_rejected_mkdir_keeps_free_clusters() {
        let mut disk = fat16_disk();
        let before = free_clusters(&mut disk);
        let too_long = format!("C:/{}", "名".repeat(86));
        for path in ["C:/bad:name", too_long.as_str(), "C:/bad😀", "C:/"] {
            assert!(mkdir(&mut disk, path).is_err());
            assert_eq!(free_clusters(&mut disk), before);
        }
        assert_eq!(list(&mut disk, "C:/").unwrap(), "[]");
        // Invalid parent names must also fail before write creates folders.
        assert!(write(&mut disk, "C:/bad:name/FILE.TXT", b"data").is_err());
        assert_eq!(free_clusters(&mut disk), before);
        mkdir(&mut disk, "C:/メモ").unwrap();
        write(&mut disk, "C:/メモ/FILE.TXT", b"data").unwrap();
        delete(&mut disk, "C:/メモ").unwrap();
        assert_eq!(free_clusters(&mut disk), before);
    }

    #[test]
    fn fat16_recursive_delete_rejects_directory_cycle() {
        let mut disk = fat16_disk();
        mkdir(&mut disk, "C:/LOOP").unwrap();
        write(&mut disk, "C:/KEEP.TXT", b"keep").unwrap();
        let word = |bytes: &[u8], at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let mut sector = [0; 512];
        disk.read(0, &mut sector).unwrap();
        let sectors_per_cluster = u64::from(sector[13]);
        let root =
            u64::from(word(&sector, 14)) + u64::from(sector[16]) * u64::from(word(&sector, 22));
        let data = root + (u64::from(word(&sector, 17)) * 32).div_ceil(512);
        disk.read(root, &mut sector).unwrap();
        let entry = sector
            .chunks_exact(32)
            .find(|e| &e[..11] == b"LOOP       ")
            .unwrap();
        let cluster = word(entry, 26);
        let at = data + u64::from(cluster - 2) * sectors_per_cluster;
        disk.read(at, &mut sector).unwrap();
        // Keep the physical dot entries; add a child pointing back to this dir.
        let child = &mut sector[64..96];
        child[..11].copy_from_slice(b"LOOP       ");
        child[11] = 0x10;
        child[26..28].copy_from_slice(&cluster.to_le_bytes());
        disk.write(at, &sector).unwrap();

        let before = free_clusters(&mut disk);
        let error = delete(&mut disk, "C:/LOOP").unwrap_err();
        assert!(error.contains("too deep or cyclic"), "{error}");
        assert_eq!(free_clusters(&mut disk), before);
        assert_eq!(read(&mut disk, "C:/KEEP.TXT").unwrap(), b"keep");
        // Bound an incoming path before fatfs recursively resolves it too.
        let deep = format!("C:/{}", vec!["LOOP"; MAX_PATH_COMPONENTS + 1].join("/"));
        assert!(delete(&mut disk, &deep).unwrap_err().contains("components"));
    }

    #[test]
    fn paths_need_a_drive_and_cannot_climb() {
        let (letter, parts) = split_path("c:\\GAMES//pal/").unwrap();
        assert_eq!((letter, parts), ('C', vec!["GAMES".into(), "pal".into()]));
        assert!(split_path("GAMES/PAL").is_err());
        assert!(split_path("C:/GAMES/../X").is_err());
        assert_eq!(json("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
