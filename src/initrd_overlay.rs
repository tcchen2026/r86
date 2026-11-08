use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitrdEntryKind {
    Regular,
    Directory,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitrdFile {
    pub guest_path: String,
    pub mode: u32,
    pub data: Vec<u8>,
    pub kind: InitrdEntryKind,
}

impl InitrdFile {
    pub fn from_host_spec(spec: &str) -> Result<Self, String> {
        let mut parts = spec.splitn(3, ':');
        let host_path = parts
            .next()
            .ok_or_else(|| "empty --initrd-file spec".to_string())?;
        let guest_path = parts
            .next()
            .ok_or_else(|| "--initrd-file expects HOST:GUEST[:MODE]".to_string())?;
        let mode = parts
            .next()
            .map(parse_octal_mode)
            .transpose()?
            .unwrap_or(0o644);
        let data = fs::read(host_path)
            .map_err(|err| format!("failed to read initrd overlay file {host_path}: {err}"))?;
        Self::new(guest_path, mode, data)
    }

    pub fn new(path: &str, mode: u32, data: Vec<u8>) -> Result<Self, String> {
        Self::entry(path, mode, data, InitrdEntryKind::Regular)
    }

    pub fn dir(path: &str, mode: u32) -> Result<Self, String> {
        Self::entry(path, mode, Vec::new(), InitrdEntryKind::Directory)
    }

    pub fn symlink(path: &str, target: &str) -> Result<Self, String> {
        Self::entry(
            path,
            0o777,
            target.as_bytes().to_vec(),
            InitrdEntryKind::Symlink,
        )
    }

    fn entry(path: &str, mode: u32, data: Vec<u8>, kind: InitrdEntryKind) -> Result<Self, String> {
        let guest_path = normalize_guest_path(path)?;
        Ok(Self {
            guest_path,
            mode: mode & 0o7777,
            data,
            kind,
        })
    }

    fn cpio_mode(&self) -> u32 {
        let file_type = match self.kind {
            InitrdEntryKind::Regular => 0o100000,
            InitrdEntryKind::Directory => 0o040000,
            InitrdEntryKind::Symlink => 0o120000,
        };
        file_type | self.mode
    }
}

pub fn entries_from_host_tree(spec: &str) -> Result<Vec<InitrdFile>, String> {
    let mut parts = spec.splitn(2, ':');
    let host_root = parts
        .next()
        .ok_or_else(|| "empty --initrd-tree spec".to_string())?;
    let guest_root = parts.next().unwrap_or("");
    let host_root = Path::new(host_root);
    let guest_root = normalize_optional_guest_prefix(guest_root)?;
    let mut entries = Vec::new();
    collect_tree_entries(host_root, host_root, &guest_root, &mut entries)?;
    Ok(entries)
}

pub fn append_newc_overlay(initrd: &mut Vec<u8>, files: &[InitrdFile]) {
    if files.is_empty() {
        return;
    }

    if remove_existing_newc_entries(initrd, files) {
        pad_to_4(initrd);
    } else if let Some(trailer_start) = find_newc_trailer_start(initrd) {
        initrd.truncate(trailer_start);
        pad_to_4(initrd);
    } else {
        pad_to_4(initrd);
    }
    for (index, file) in files.iter().enumerate() {
        append_newc_entry(
            initrd,
            0x8000_0001_u32.wrapping_add(index as u32),
            file.cpio_mode(),
            0,
            0,
            1,
            0,
            &file.guest_path,
            &file.data,
        );
    }
    append_newc_entry(initrd, 0, 0, 0, 0, 1, 0, "TRAILER!!!", &[]);
}

fn collect_tree_entries(
    root: &Path,
    path: &Path,
    guest_root: &str,
    entries: &mut Vec<InitrdFile>,
) -> Result<(), String> {
    let mut children = sorted_dir_entries(path)?;
    for child in children.drain(..) {
        let metadata = fs::symlink_metadata(&child)
            .map_err(|err| format!("failed to stat {}: {err}", child.display()))?;
        let guest_path = tree_guest_path(root, &child, guest_root)?;
        let mode = unix_mode(&metadata);
        if metadata.file_type().is_dir() {
            entries.push(InitrdFile::dir(&guest_path, mode)?);
            collect_tree_entries(root, &child, guest_root, entries)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&child)
                .map_err(|err| format!("failed to read symlink {}: {err}", child.display()))?;
            entries.push(InitrdFile::symlink(&guest_path, &target.to_string_lossy())?);
        } else if metadata.file_type().is_file() {
            let data = fs::read(&child).map_err(|err| {
                format!("failed to read initrd tree file {}: {err}", child.display())
            })?;
            entries.push(InitrdFile::new(&guest_path, mode, data)?);
        }
    }
    Ok(())
}

fn sorted_dir_entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut children = Vec::new();
    for entry in
        fs::read_dir(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?
    {
        let entry =
            entry.map_err(|err| format!("failed to read entry in {}: {err}", path.display()))?;
        children.push(entry.path());
    }
    children.sort();
    Ok(children)
}

fn tree_guest_path(root: &Path, path: &Path, guest_root: &str) -> Result<String, String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|err| format!("failed to relativize {}: {err}", path.display()))?;
    let rel = rel
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if guest_root.is_empty() {
        Ok(rel)
    } else {
        Ok(format!("{guest_root}/{rel}"))
    }
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_dir() {
        0o755
    } else {
        0o644
    }
}

fn remove_existing_newc_entries(initrd: &mut Vec<u8>, files: &[InitrdFile]) -> bool {
    let mut pos = 0usize;
    let mut filtered = Vec::with_capacity(initrd.len());
    while pos + 110 <= initrd.len() {
        let Some(entry) = NewcEntry::parse(initrd, pos) else {
            return false;
        };
        if entry.name == b"TRAILER!!!" {
            *initrd = filtered;
            return true;
        }
        if !files.iter().any(|file| {
            file.kind != InitrdEntryKind::Directory && file.guest_path.as_bytes() == entry.name
        }) {
            filtered.extend_from_slice(&initrd[pos..entry.next]);
        }
        pos = entry.next;
    }
    false
}

fn find_newc_trailer_start(bytes: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    while pos + 110 <= bytes.len() {
        let entry = NewcEntry::parse(bytes, pos)?;
        if entry.name == b"TRAILER!!!" {
            return Some(pos);
        }
        pos = entry.next;
    }
    None
}

struct NewcEntry<'a> {
    name: &'a [u8],
    next: usize,
}

impl<'a> NewcEntry<'a> {
    fn parse(bytes: &'a [u8], pos: usize) -> Option<Self> {
        if pos + 110 > bytes.len() || &bytes[pos..pos + 6] != b"070701" {
            return None;
        }
        let filesize = parse_hex_u32(&bytes[pos + 54..pos + 62])? as usize;
        let namesize = parse_hex_u32(&bytes[pos + 94..pos + 102])? as usize;
        if namesize == 0 {
            return None;
        }
        let name_start = pos + 110;
        let name_end = name_start.checked_add(namesize)?;
        if name_end > bytes.len() {
            return None;
        }
        let name = bytes.get(name_start..name_end - 1)?;
        let data_start = align4(name_end);
        let next = align4(data_start.checked_add(filesize)?);
        if next > bytes.len() {
            return None;
        }
        Some(Self { name, next })
    }
}

fn parse_hex_u32(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit as u32)?;
    }
    Some(value)
}

fn normalize_guest_path(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("guest initrd path must not be empty".to_string());
    }
    let path = trimmed.trim_start_matches('/');
    if path.is_empty() || path.contains('\0') {
        return Err(format!("invalid guest initrd path {trimmed:?}"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!(
            "guest initrd path must be relative and normalized: {trimmed}"
        ));
    }
    Ok(path.to_string())
}

fn normalize_optional_guest_prefix(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        Ok(String::new())
    } else {
        normalize_guest_path(trimmed)
    }
}

fn parse_octal_mode(mode: &str) -> Result<u32, String> {
    u32::from_str_radix(mode.trim_start_matches("0o"), 8)
        .map_err(|_| format!("invalid octal mode {mode:?}"))
        .and_then(|value| {
            if value <= 0o7777 {
                Ok(value)
            } else {
                Err(format!("initrd file mode out of range: {mode}"))
            }
        })
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn append_newc_entry(
    out: &mut Vec<u8>,
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    name: &str,
    data: &[u8],
) {
    out.extend_from_slice(b"070701");
    write_hex_u32(out, ino);
    write_hex_u32(out, mode);
    write_hex_u32(out, uid);
    write_hex_u32(out, gid);
    write_hex_u32(out, nlink);
    write_hex_u32(out, mtime);
    write_hex_u32(out, data.len() as u32);
    write_hex_u32(out, 0);
    write_hex_u32(out, 0);
    write_hex_u32(out, 0);
    write_hex_u32(out, 0);
    write_hex_u32(out, name.len() as u32 + 1);
    write_hex_u32(out, 0);

    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad_to_4(out);
    out.extend_from_slice(data);
    pad_to_4(out);
}

fn write_hex_u32(out: &mut Vec<u8>, value: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for shift in (0..=28).rev().step_by(4) {
        out.push(HEX[((value >> shift) & 0xf) as usize]);
    }
}

fn pad_to_4(out: &mut Vec<u8>) {
    while out.len() != align4(out.len()) {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_names(cpio: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        let mut pos = 0usize;
        loop {
            if pos + 110 > cpio.len() {
                break;
            }
            assert_eq!(&cpio[pos..pos + 6], b"070701");
            let filesize = parse_hex(&cpio[pos + 54..pos + 62]) as usize;
            let namesize = parse_hex(&cpio[pos + 94..pos + 102]) as usize;
            let name_start = pos + 110;
            let name_end = name_start + namesize - 1;
            let name = String::from_utf8(cpio[name_start..name_end].to_vec()).unwrap();
            names.push(name.clone());
            let data_start = align4(name_start + namesize);
            pos = align4(data_start + filesize);
            if name == "TRAILER!!!" {
                break;
            }
        }
        names
    }

    fn parse_hex(bytes: &[u8]) -> u32 {
        let text = core::str::from_utf8(bytes).unwrap();
        u32::from_str_radix(text, 16).unwrap()
    }

    #[test]
    fn newc_overlay_appends_files_and_trailer() {
        let files = [
            InitrdFile::new("/opt/bootlocal.sh", 0o755, b"#!/bin/sh\n".to_vec()).unwrap(),
            InitrdFile::new("tmp/pi.py", 0o644, b"print(3.14)\n".to_vec()).unwrap(),
        ];
        let mut initrd = b"base".to_vec();

        append_newc_overlay(&mut initrd, &files);

        assert_eq!(
            entry_names(&initrd[4..]),
            ["opt/bootlocal.sh", "tmp/pi.py", "TRAILER!!!"]
        );
    }

    #[test]
    fn newc_overlay_replaces_existing_trailer() {
        let mut initrd = Vec::new();
        let base = [InitrdFile::new("etc/base", 0o644, b"base\n".to_vec()).unwrap()];
        let overlay = [InitrdFile::new("etc/overlay", 0o644, b"overlay\n".to_vec()).unwrap()];

        append_newc_overlay(&mut initrd, &base);
        append_newc_overlay(&mut initrd, &overlay);

        assert_eq!(
            entry_names(&initrd),
            ["etc/base", "etc/overlay", "TRAILER!!!"]
        );
    }

    #[test]
    fn newc_overlay_removes_replaced_paths_before_appending() {
        let mut initrd = Vec::new();
        let base = [InitrdFile::new("opt/bootlocal.sh", 0o755, b"old\n".to_vec()).unwrap()];
        let overlay = [InitrdFile::new("opt/bootlocal.sh", 0o755, b"new\n".to_vec()).unwrap()];

        append_newc_overlay(&mut initrd, &base);
        append_newc_overlay(&mut initrd, &overlay);

        assert_eq!(entry_names(&initrd), ["opt/bootlocal.sh", "TRAILER!!!"]);
        assert!(initrd.windows(b"new\n".len()).any(|w| w == b"new\n"));
        assert!(!initrd.windows(b"old\n".len()).any(|w| w == b"old\n"));
    }

    #[test]
    fn host_spec_normalizes_guest_path_and_octal_mode() {
        let tmp = std::env::temp_dir().join(format!("r86-initrd-overlay-{}", std::process::id()));
        fs::write(&tmp, b"hello").unwrap();
        let spec = format!("{}:/usr/local/bin/tool:0755", tmp.display());

        let file = InitrdFile::from_host_spec(&spec).unwrap();

        assert_eq!(file.guest_path, "usr/local/bin/tool");
        assert_eq!(file.mode, 0o755);
        assert_eq!(file.data, b"hello");
        let _ = fs::remove_file(tmp);
    }

    #[test]
    fn guest_paths_must_be_normalized() {
        assert!(InitrdFile::new("", 0o644, Vec::new()).is_err());
        assert!(InitrdFile::new("../x", 0o644, Vec::new()).is_err());
        assert!(InitrdFile::new("tmp//x", 0o644, Vec::new()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn host_tree_collects_dirs_files_and_symlinks_in_guest_prefix() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("r86-initrd-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/tool"), b"tool\n").unwrap();
        symlink("tool", root.join("bin/tool-link")).unwrap();
        let spec = format!("{}:/usr/local", root.display());

        let entries = entries_from_host_tree(&spec).unwrap();

        assert!(entries.iter().any(|entry| {
            entry.guest_path == "usr/local/bin" && entry.kind == InitrdEntryKind::Directory
        }));
        assert!(entries.iter().any(|entry| {
            entry.guest_path == "usr/local/bin/tool"
                && entry.kind == InitrdEntryKind::Regular
                && entry.data == b"tool\n"
        }));
        assert!(entries.iter().any(|entry| {
            entry.guest_path == "usr/local/bin/tool-link"
                && entry.kind == InitrdEntryKind::Symlink
                && entry.data == b"tool"
        }));
        let _ = fs::remove_dir_all(root);
    }
}
