use std::collections::{BTreeMap, BTreeSet};
use std::iter;

pub const SECTOR_SIZE: usize = 2048;

#[derive(Debug)]
pub struct Iso9660<'a> {
    data: &'a [u8],
    root: Directory,
}

#[derive(Clone, Copy, Debug)]
struct Directory {
    extent: u32,
    size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsoFileInfo {
    pub extent: u32,
    pub size: u32,
}

#[derive(Clone, Debug)]
struct DirRecord {
    extent: u32,
    size: u32,
    flags: u8,
    name: String,
}

#[derive(Clone, Debug)]
struct LocatedRecord {
    record: DirRecord,
    offset: usize,
}

impl<'a> Iso9660<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self, String> {
        let pvd = sector(data, 16).ok_or("ISO is too small for a primary volume descriptor")?;
        if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
            return Err("missing ISO9660 primary volume descriptor".to_string());
        }
        let root_record =
            parse_record(&pvd[156..]).ok_or("missing ISO9660 root directory record")?;
        Ok(Self {
            data,
            root: Directory {
                extent: root_record.extent,
                size: root_record.size,
            },
        })
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let record = self.find_path(path)?;
        if record.flags & 0x02 != 0 {
            return Err(format!("ISO path is a directory: {path}"));
        }
        let start = record.extent as usize * SECTOR_SIZE;
        let end = start
            .checked_add(record.size as usize)
            .ok_or_else(|| format!("ISO file is too large: {path}"))?;
        self.data
            .get(start..end)
            .map(|bytes| bytes.to_vec())
            .ok_or_else(|| format!("ISO file extent is out of range: {path}"))
    }

    pub fn file_info(&self, path: &str) -> Result<IsoFileInfo, String> {
        let record = self.find_path(path)?;
        if record.flags & 0x02 != 0 {
            return Err(format!("ISO path is a directory: {path}"));
        }
        Ok(IsoFileInfo {
            extent: record.extent,
            size: record.size,
        })
    }

    fn find_path(&self, path: &str) -> Result<DirRecord, String> {
        let mut dir = self.root;
        let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
        while let Some(part) = parts.next() {
            let record = self
                .find_in_dir(dir, part)?
                .ok_or_else(|| format!("ISO path not found: {path}"))?;
            if parts.peek().is_none() {
                return Ok(record);
            }
            if record.flags & 0x02 == 0 {
                return Err(format!("ISO path component is not a directory: {part}"));
            }
            dir = Directory {
                extent: record.extent,
                size: record.size,
            };
        }
        Err("empty ISO path".to_string())
    }

    fn find_in_dir(&self, dir: Directory, name: &str) -> Result<Option<DirRecord>, String> {
        Ok(self
            .find_in_dir_located(dir, name)?
            .map(|entry| entry.record))
    }

    fn find_in_dir_located(
        &self,
        dir: Directory,
        name: &str,
    ) -> Result<Option<LocatedRecord>, String> {
        let start = dir.extent as usize * SECTOR_SIZE;
        let end = start
            .checked_add(dir.size as usize)
            .ok_or("ISO directory is too large")?;
        let bytes = self
            .data
            .get(start..end)
            .ok_or("ISO directory extent is out of range")?;
        let wanted = normalize_name(name);
        let mut offset = 0;
        while offset < bytes.len() {
            let len = bytes[offset] as usize;
            if len == 0 {
                offset = ((offset / SECTOR_SIZE) + 1) * SECTOR_SIZE;
                continue;
            }
            let record_bytes = bytes
                .get(offset..offset + len)
                .ok_or("ISO directory record crosses directory boundary")?;
            if let Some(record) = parse_record(record_bytes) {
                if normalize_name(&record.name) == wanted {
                    return Ok(Some(LocatedRecord {
                        record,
                        offset: start + offset,
                    }));
                }
            }
            offset += len;
        }
        Ok(None)
    }
}

pub fn replace_file_in_place(data: &mut [u8], path: &str, new_data: &[u8]) -> Result<(), String> {
    let located = find_path_mut(data, path)?;
    if located.record.flags & 0x02 != 0 {
        return Err(format!("ISO path is a directory: {path}"));
    }
    let old_len = located.record.size as usize;
    if new_data.len() > old_len {
        return Err(format!(
            "replacement for {path} is too large for in-place ISO patch: new={} old={old_len}",
            new_data.len()
        ));
    }
    let start = located.record.extent as usize * SECTOR_SIZE;
    let end = start
        .checked_add(old_len)
        .ok_or_else(|| format!("ISO file is too large: {path}"))?;
    let dst = data
        .get_mut(start..end)
        .ok_or_else(|| format!("ISO file extent is out of range: {path}"))?;
    dst.fill(0);
    dst[..new_data.len()].copy_from_slice(new_data);

    let new_len = u32::try_from(new_data.len())
        .map_err(|_| format!("replacement for {path} is too large for ISO9660"))?;
    let record = data
        .get_mut(located.offset..located.offset + 34)
        .ok_or("ISO directory record disappeared during patch")?;
    record[10..14].copy_from_slice(&new_len.to_le_bytes());
    record[14..18].copy_from_slice(&new_len.to_be_bytes());
    Ok(())
}

pub fn replace_file(data: &mut Vec<u8>, path: &str, new_data: &[u8]) -> Result<(), String> {
    let located = find_path_mut(data, path)?;
    if located.record.flags & 0x02 != 0 {
        return Err(format!("ISO path is a directory: {path}"));
    }
    if new_data.len() <= located.record.size as usize {
        return replace_file_in_place(data, path, new_data);
    }

    pad_vec_to_sector(data);
    let extent = u32::try_from(data.len() / SECTOR_SIZE)
        .map_err(|_| format!("replacement for {path} would exceed ISO32 sector addressing"))?;
    data.extend_from_slice(new_data);
    pad_vec_to_sector(data);
    let new_len = u32::try_from(new_data.len())
        .map_err(|_| format!("replacement for {path} is too large for ISO9660"))?;
    update_dir_record_extent_and_size(data, located.offset, extent, new_len)?;
    update_volume_space_size(data)?;
    Ok(())
}

fn find_path_mut(data: &[u8], path: &str) -> Result<LocatedRecord, String> {
    let pvd = sector(data, 16).ok_or("ISO is too small for a primary volume descriptor")?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return Err("missing ISO9660 primary volume descriptor".to_string());
    }
    let root_record = parse_record(&pvd[156..]).ok_or("missing ISO9660 root directory record")?;
    let mut dir = Directory {
        extent: root_record.extent,
        size: root_record.size,
    };
    let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
    while let Some(part) = parts.next() {
        let record = find_in_dir_located(data, dir, part)?
            .ok_or_else(|| format!("ISO path not found: {path}"))?;
        if parts.peek().is_none() {
            return Ok(record);
        }
        if record.record.flags & 0x02 == 0 {
            return Err(format!("ISO path component is not a directory: {part}"));
        }
        dir = Directory {
            extent: record.record.extent,
            size: record.record.size,
        };
    }
    Err("empty ISO path".to_string())
}

fn find_in_dir_located(
    data: &[u8],
    dir: Directory,
    name: &str,
) -> Result<Option<LocatedRecord>, String> {
    let start = dir.extent as usize * SECTOR_SIZE;
    let end = start
        .checked_add(dir.size as usize)
        .ok_or("ISO directory is too large")?;
    let bytes = data
        .get(start..end)
        .ok_or("ISO directory extent is out of range")?;
    let wanted = normalize_name(name);
    let mut offset = 0;
    while offset < bytes.len() {
        let len = bytes[offset] as usize;
        if len == 0 {
            offset = ((offset / SECTOR_SIZE) + 1) * SECTOR_SIZE;
            continue;
        }
        let record_bytes = bytes
            .get(offset..offset + len)
            .ok_or("ISO directory record crosses directory boundary")?;
        if let Some(record) = parse_record(record_bytes) {
            if normalize_name(&record.name) == wanted {
                return Ok(Some(LocatedRecord {
                    record,
                    offset: start + offset,
                }));
            }
        }
        offset += len;
    }
    Ok(None)
}

fn update_dir_record_extent_and_size(
    data: &mut [u8],
    offset: usize,
    extent: u32,
    size: u32,
) -> Result<(), String> {
    let record = data
        .get_mut(offset..offset + 34)
        .ok_or("ISO directory record disappeared during patch")?;
    record[2..6].copy_from_slice(&extent.to_le_bytes());
    record[6..10].copy_from_slice(&extent.to_be_bytes());
    record[10..14].copy_from_slice(&size.to_le_bytes());
    record[14..18].copy_from_slice(&size.to_be_bytes());
    Ok(())
}

fn update_volume_space_size(data: &mut [u8]) -> Result<(), String> {
    let sectors = u32::try_from(data.len() / SECTOR_SIZE)
        .map_err(|_| "ISO image would exceed ISO32 sector addressing".to_string())?;
    let mut index = 16usize;
    loop {
        let start = index
            .checked_mul(SECTOR_SIZE)
            .ok_or("ISO volume descriptor index overflow")?;
        let Some(desc) = data.get_mut(start..start + SECTOR_SIZE) else {
            return Err("ISO volume descriptor set has no terminator".to_string());
        };
        if &desc[1..6] != b"CD001" {
            return Err("invalid ISO volume descriptor while updating size".to_string());
        }
        if desc[0] == 1 || desc[0] == 2 {
            desc[80..84].copy_from_slice(&sectors.to_le_bytes());
            desc[84..88].copy_from_slice(&sectors.to_be_bytes());
        }
        if desc[0] == 255 {
            return Ok(());
        }
        index += 1;
    }
}

fn pad_vec_to_sector(data: &mut Vec<u8>) {
    let rem = data.len() % SECTOR_SIZE;
    if rem != 0 {
        data.resize(data.len() + (SECTOR_SIZE - rem), 0);
    }
}

fn sector(data: &[u8], index: usize) -> Option<&[u8]> {
    let start = index.checked_mul(SECTOR_SIZE)?;
    data.get(start..start + SECTOR_SIZE)
}

fn parse_record(bytes: &[u8]) -> Option<DirRecord> {
    let len = *bytes.first()? as usize;
    if len < 34 || bytes.len() < len {
        return None;
    }
    let name_len = bytes[32] as usize;
    if 33 + name_len > len {
        return None;
    }
    let name_bytes = &bytes[33..33 + name_len];
    let name = match name_bytes {
        [0] => ".".to_string(),
        [1] => "..".to_string(),
        _ => String::from_utf8_lossy(name_bytes).into_owned(),
    };
    Some(DirRecord {
        extent: le_u32(&bytes[2..6]),
        size: le_u32(&bytes[10..14]),
        flags: bytes[25],
        name,
    })
}

fn normalize_name(name: &str) -> String {
    let no_version = name.split_once(';').map_or(name, |(base, _)| base);
    no_version.trim_end_matches('.').to_ascii_lowercase()
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Recording time stamped on every record and descriptor (UTC, no zone offset).
#[derive(Clone, Copy, Debug)]
pub struct IsoTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Sectors 16..19 hold the PVD, the Joliet SVD and the set terminator.
const FIRST_DATA_SECTOR: usize = 19;
/// ECMA-119 6.8.2.1 allows 8 directory levels including the root.
const MAX_FOLDER_DEPTH: usize = 7;
/// Joliet name limit in UCS-2 units, excluding the `;1` version.
const JOLIET_NAME_MAX: usize = 64;

/// In-memory ISO 9660 level-1 + Joliet image writer.
///
/// Layout: system area, descriptors, file data in `begin_file` order (each file
/// sector-aligned, shared by both namespaces), then per namespace the directory
/// extents in path-table order and the L and M path tables, appended by `finish`.
pub struct IsoBuilder {
    image: Vec<u8>,
    label: String,
    time: IsoTime,
    /// Node 0 is the root directory.
    nodes: Vec<Node>,
    files: usize,
    /// File being written and its bytes still owed by `write`.
    current: String,
    pending: u64,
}

struct Node {
    parent: usize,
    /// Children by original name; `None` for files.
    children: Option<BTreeMap<String, usize>>,
    /// File extent and size. Directory extents are per namespace (`finish`).
    extent: u32,
    size: u32,
}

/// One namespace's root and path tables, for its volume descriptor.
struct Tree {
    root_extent: u32,
    root_size: u32,
    table_size: u32,
    l_table: u32,
    m_table: u32,
}

#[allow(clippy::len_without_is_empty)] // an image always has its descriptor sectors
impl IsoBuilder {
    /// `label` is the volume label (sanitised for the PVD: up to 32 d-characters; Joliet: up to 16 UCS-2 chars).
    pub fn new(label: &str, time: IsoTime) -> Self {
        Self {
            image: vec![0; FIRST_DATA_SECTOR * SECTOR_SIZE],
            label: label.to_string(),
            time,
            nodes: vec![Node {
                parent: 0,
                children: Some(BTreeMap::new()),
                extent: 0,
                size: 0,
            }],
            files: 0,
            current: String::new(),
            pending: 0,
        }
    }

    /// Starts a file at a '/'-separated path, creating parent directories. Exactly `size` bytes must
    /// follow through `write` before the next `begin_file`/`add_dir`/`finish`.
    pub fn begin_file(&mut self, path: &str, size: u64) -> Result<(), String> {
        self.check_complete()?;
        let parts = components(path)?;
        let (name, dirs) = parts.split_last().expect("split yields a component");
        let size32 = u32::try_from(size)
            .map_err(|_| format!("'{path}' is 4 GiB or larger, beyond the ISO 9660 file limit"))?;
        let end = self.image.len() as u64 + size;
        if end.div_ceil(SECTOR_SIZE as u64) > u64::from(u32::MAX) {
            return Err("the image would exceed the ISO 9660 size limit".to_string());
        }
        let parent = self.make_dirs(path, dirs)?;
        if self.child(parent, name).is_some() {
            return Err(format!("'{path}' already exists"));
        }
        usize::try_from(size.next_multiple_of(SECTOR_SIZE as u64))
            .ok()
            .and_then(|bytes| self.image.try_reserve(bytes).ok())
            .ok_or_else(|| format!("not enough memory for '{path}'"))?;
        let extent = (self.image.len() / SECTOR_SIZE) as u32;
        self.add_node(parent, name, None, extent, size32);
        self.files += 1;
        self.current = path.to_string();
        self.pending = size;
        Ok(())
    }

    /// Appends file data directly to the image buffer (no second copy). Errors if more than declared.
    pub fn write(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() as u64 > self.pending {
            return Err(format!(
                "more data than the declared size of '{}'",
                self.current
            ));
        }
        self.image.extend_from_slice(data);
        self.pending -= data.len() as u64;
        if self.pending == 0 {
            pad_vec_to_sector(&mut self.image);
        }
        Ok(())
    }

    /// Creates an (empty) directory and its parents; existing directories are fine.
    pub fn add_dir(&mut self, path: &str) -> Result<(), String> {
        self.check_complete()?;
        let parts = components(path)?;
        self.make_dirs(path, &parts).map(drop)
    }

    /// Bytes the image occupies so far: descriptors and sector-padded file data,
    /// including the declared size of the file being written.
    pub fn len(&self) -> u64 {
        (self.image.len() as u64 + self.pending).next_multiple_of(SECTOR_SIZE as u64)
    }

    pub fn files(&self) -> usize {
        self.files
    }

    /// Writes directories, path tables and volume descriptors and returns the image.
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        self.check_complete()?;
        let trees = [self.write_tree(false)?, self.write_tree(true)?];
        let sectors = sector_count(&self.image)?;
        for (index, tree) in trees.iter().enumerate() {
            let start = (16 + index) * SECTOR_SIZE;
            let desc = &mut self.image[start..start + SECTOR_SIZE];
            write_descriptor(desc, tree, index == 1, &self.label, self.time, sectors);
        }
        let end = &mut self.image[18 * SECTOR_SIZE..];
        end[0] = 255;
        end[1..6].copy_from_slice(b"CD001");
        end[6] = 1;
        Ok(self.image)
    }

    fn check_complete(&self) -> Result<(), String> {
        if self.pending == 0 {
            return Ok(());
        }
        Err(format!(
            "'{}' is incomplete: {} bytes missing",
            self.current, self.pending
        ))
    }

    /// Returns the directory at `dirs`, creating missing ones.
    fn make_dirs(&mut self, path: &str, dirs: &[&str]) -> Result<usize, String> {
        if dirs.len() > MAX_FOLDER_DEPTH {
            return Err(format!(
                "'{path}' is nested too deeply: ISO 9660 allows {MAX_FOLDER_DEPTH} folder levels"
            ));
        }
        let mut dir = 0;
        for name in dirs {
            dir = match self.child(dir, name) {
                Some(node) if self.nodes[node].children.is_some() => node,
                Some(_) => return Err(format!("'{path}': '{name}' is a file, not a folder")),
                None => self.add_node(dir, name, Some(BTreeMap::new()), 0, 0),
            };
        }
        Ok(dir)
    }

    fn child(&self, dir: usize, name: &str) -> Option<usize> {
        self.nodes[dir].children.as_ref()?.get(name).copied()
    }

    fn add_node(
        &mut self,
        parent: usize,
        name: &str,
        children: Option<BTreeMap<String, usize>>,
        extent: u32,
        size: u32,
    ) -> usize {
        let index = self.nodes.len();
        self.nodes.push(Node {
            parent,
            children,
            extent,
            size,
        });
        let siblings = self.nodes[parent].children.as_mut();
        siblings
            .expect("parent is a directory")
            .insert(name.to_string(), index);
        index
    }

    /// Appends one namespace's directory extents and L/M path tables.
    fn write_tree(&mut self, joliet: bool) -> Result<Tree, String> {
        let nodes = &self.nodes;
        let is_dir = |node: usize| nodes[node].children.is_some();
        let mut ident = vec![Vec::new(); nodes.len()];
        let mut kids = vec![Vec::new(); nodes.len()];
        for (dir, node) in nodes.iter().enumerate() {
            let Some(children) = &node.children else {
                continue;
            };
            for (child, name) in dir_names(children, nodes, joliet) {
                ident[child] = encode_ident(&name, is_dir(child), joliet);
                kids[dir].push(child);
            }
        }
        // Breadth-first over sorted children is the ECMA-119 6.9.1 path table
        // order: level, then parent number, then name.
        let mut order = vec![0];
        let mut next = 0;
        while let Some(&dir) = order.get(next) {
            order.extend(kids[dir].iter().copied().filter(|&child| is_dir(child)));
            next += 1;
        }
        if order.len() > usize::from(u16::MAX) {
            return Err(format!("too many folders: ISO 9660 allows {}", u16::MAX));
        }

        let mut extent = vec![0u32; nodes.len()];
        let mut size = vec![0u32; nodes.len()];
        let mut sector = self.image.len() / SECTOR_SIZE;
        for &dir in &order {
            let end = kids[dir].iter().fold(68, |off, &child| {
                let len = record_len(ident[child].len());
                place(off, len) + len
            });
            size[dir] = u32::try_from(end.next_multiple_of(SECTOR_SIZE))
                .map_err(|_| "a folder has too many entries".to_string())?;
            extent[dir] = sector as u32;
            sector += size[dir] as usize / SECTOR_SIZE;
        }
        u32::try_from(sector)
            .map_err(|_| "the image would exceed the ISO 9660 size limit".to_string())?;
        self.image.resize(sector * SECTOR_SIZE, 0);

        let time = self.time;
        for &dir in &order {
            let start = extent[dir] as usize * SECTOR_SIZE;
            let out = &mut self.image[start..start + size[dir] as usize];
            let parent = nodes[dir].parent;
            let mut off = put_record(out, extent[dir], size[dir], true, &[0], time);
            off += put_record(
                &mut out[off..],
                extent[parent],
                size[parent],
                true,
                &[1],
                time,
            );
            for &child in &kids[dir] {
                off = place(off, record_len(ident[child].len()));
                let (at, len) = if is_dir(child) {
                    (extent[child], size[child])
                } else {
                    (nodes[child].extent, nodes[child].size)
                };
                off += put_record(&mut out[off..], at, len, is_dir(child), &ident[child], time);
            }
        }

        let mut number = vec![0u16; nodes.len()];
        let mut tables = [Vec::new(), Vec::new()];
        for (index, &dir) in order.iter().enumerate() {
            number[dir] = index as u16 + 1;
            let id: &[u8] = if dir == 0 { &[0] } else { &ident[dir] };
            let parent = number[nodes[dir].parent];
            let [l, m] = &mut tables;
            path_record(l, id, extent[dir].to_le_bytes(), parent.to_le_bytes());
            path_record(m, id, extent[dir].to_be_bytes(), parent.to_be_bytes());
        }
        let mut locations = [0u32; 2];
        for (table, location) in tables.iter().zip(&mut locations) {
            *location = sector_count(&self.image)?;
            self.image.extend_from_slice(table);
            pad_vec_to_sector(&mut self.image);
        }
        Ok(Tree {
            root_extent: extent[0],
            root_size: size[0],
            table_size: tables[0].len() as u32,
            l_table: locations[0],
            m_table: locations[1],
        })
    }
}

/// Splits an upload path, rejecting empty, `.` and `..` components.
fn components(path: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(format!(
            "invalid path '{path}': empty, '.' or '..' component"
        ));
    }
    Ok(parts)
}

/// Unique names for one directory's children in one namespace, sorted per
/// ECMA-119 9.3 (name, then extension). Names already valid in the namespace
/// claim themselves first, in original-name order.
/// - ISO 9660 level 1: other names become `STEM~N.EXT` in 8.3 d-characters
///   (`~` is the usual DOS/Windows extension to d-characters).
/// - Joliet: other names keep their sanitised form when it is free and at most
///   64 UCS-2 units; otherwise they are truncated to `stem~N.ext` within 64
///   units, keeping extensions of up to 16 characters.
///
/// Uniqueness is case-insensitive in both, as DOS and Windows look names up so.
/// `N` counts per candidate, so large collision sets stay linear.
fn dir_names(
    children: &BTreeMap<String, usize>,
    nodes: &[Node],
    joliet: bool,
) -> Vec<(usize, String)> {
    struct Want {
        node: usize,
        exact: bool,
        plain: Option<String>,
        stem: String,
        ext: String,
        stem_max: usize,
    }
    let is_dir = |node: usize| nodes[node].children.is_some();
    let wants: Vec<Want> = children
        .iter()
        .map(|(name, &node)| {
            if joliet {
                let clean: String = name.chars().map(joliet_char).collect();
                let fits = clean.chars().count() <= JOLIET_NAME_MAX;
                let (stem, ext) = split_ext(&clean, is_dir(node), 16);
                let ext_len = if ext.is_empty() {
                    0
                } else {
                    ext.chars().count() + 1
                };
                Want {
                    node,
                    exact: fits && clean == *name,
                    stem: stem.to_string(),
                    ext: ext.to_string(),
                    stem_max: JOLIET_NAME_MAX - ext_len,
                    plain: fits.then_some(clean),
                }
            } else {
                // Like Windows short names: spaces and inner dots vanish.
                let (stem, ext) = split_ext(name, is_dir(node), usize::MAX);
                let stem: String = stem
                    .chars()
                    .filter(|c| !matches!(c, ' ' | '.'))
                    .map(d_char)
                    .take(8)
                    .collect();
                let ext: String = ext
                    .chars()
                    .filter(|&c| c != ' ')
                    .map(d_char)
                    .take(3)
                    .collect();
                let plain = if ext.is_empty() {
                    stem.clone()
                } else {
                    format!("{stem}.{ext}")
                };
                let exact = plain.eq_ignore_ascii_case(name);
                Want {
                    node,
                    exact,
                    plain: exact.then_some(plain),
                    stem,
                    ext,
                    stem_max: 8,
                }
            }
        })
        .collect();
    let tilde = |want: &Want, n: u32| {
        let tail = format!("~{n}");
        let keep = want.stem_max.saturating_sub(tail.len());
        let mut name: String = want.stem.chars().take(keep).collect();
        name.push_str(&tail);
        if !want.ext.is_empty() {
            name.push('.');
            name.push_str(&want.ext);
        }
        name
    };

    let mut taken = BTreeSet::new();
    let mut names = vec![String::new(); wants.len()];
    for (want, slot) in wants.iter().zip(&mut names) {
        if let Some(plain) = want.plain.as_ref().filter(|_| want.exact)
            && taken.insert(plain.to_lowercase())
        {
            *slot = plain.clone();
        }
    }
    let mut counters = BTreeMap::new();
    for (want, slot) in wants.iter().zip(&mut names) {
        if !slot.is_empty() {
            continue;
        }
        if let Some(plain) = &want.plain
            && taken.insert(plain.to_lowercase())
        {
            *slot = plain.clone();
            continue;
        }
        let n = counters.entry(tilde(want, 1)).or_insert(1);
        *slot = loop {
            let name = tilde(want, *n);
            *n += 1;
            if taken.insert(name.to_lowercase()) {
                break name;
            }
        };
    }

    let mut out: Vec<(usize, String)> = wants.iter().map(|want| want.node).zip(names).collect();
    out.sort_by(|a, b| {
        split_ext(&a.1, is_dir(a.0), usize::MAX).cmp(&split_ext(&b.1, is_dir(b.0), usize::MAX))
    });
    out
}

/// Splits a file name at its last dot when the extension has at most `max_ext`
/// characters; directories and dot-files keep the whole name as stem.
fn split_ext(name: &str, dir: bool, max_ext: usize) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if !dir && dot > 0 && name[dot + 1..].chars().count() <= max_ext => {
            (&name[..dot], &name[dot + 1..])
        }
        _ => (name, ""),
    }
}

fn d_char(c: char) -> char {
    match c {
        'A'..='Z' | '0'..='9' | '_' => c,
        'a'..='z' => c.to_ascii_uppercase(),
        _ => '_',
    }
}

/// Joliet forbids control characters and `* / : ; ? \`; UCS-2 has no room for
/// characters outside the BMP.
fn joliet_char(c: char) -> char {
    match c {
        '\0'..='\x1f' | '*' | '/' | ':' | ';' | '?' | '\\' => '_',
        c if u32::from(c) > 0xffff => '_',
        c => c,
    }
}

/// Directory record identifier: level-1 `NAME.EXT;1`, or UCS-2BE `name;1` for
/// Joliet. Directories carry no version.
fn encode_ident(name: &str, dir: bool, joliet: bool) -> Vec<u8> {
    if joliet {
        let version = if dir { "" } else { ";1" };
        let units = name.encode_utf16().chain(version.encode_utf16());
        return units.flat_map(u16::to_be_bytes).collect();
    }
    let version = match (dir, name.contains('.')) {
        (true, _) => "",
        (false, true) => ";1",
        (false, false) => ".;1",
    };
    format!("{name}{version}").into_bytes()
}

fn record_len(id_len: usize) -> usize {
    33 + id_len + (id_len + 1) % 2
}

/// Start offset for a directory record of `len` bytes; records never cross sectors.
fn place(off: usize, len: usize) -> usize {
    if off % SECTOR_SIZE + len > SECTOR_SIZE {
        off.next_multiple_of(SECTOR_SIZE)
    } else {
        off
    }
}

/// Writes an ECMA-119 9.1 directory record into zeroed `out`; returns its length.
fn put_record(out: &mut [u8], extent: u32, size: u32, dir: bool, id: &[u8], t: IsoTime) -> usize {
    let len = record_len(id.len());
    let record = &mut out[..len];
    record[0] = len as u8;
    both32(&mut record[2..10], extent);
    both32(&mut record[10..18], size);
    let year = t.year.saturating_sub(1900).min(255) as u8;
    record[18..25].copy_from_slice(&[year, t.month, t.day, t.hour, t.minute, t.second, 0]);
    record[25] = if dir { 0x02 } else { 0 };
    both16(&mut record[28..32], 1);
    record[32] = id.len() as u8;
    record[33..33 + id.len()].copy_from_slice(id);
    len
}

fn path_record(table: &mut Vec<u8>, id: &[u8], extent: [u8; 4], parent: [u8; 2]) {
    table.extend_from_slice(&[id.len() as u8, 0]);
    table.extend_from_slice(&extent);
    table.extend_from_slice(&parent);
    table.extend_from_slice(id);
    if id.len() % 2 == 1 {
        table.push(0);
    }
}

/// Fills the primary (ISO 9660) or supplementary (Joliet, UCS-2 level 3)
/// volume descriptor (ECMA-119 8.4/8.5).
fn write_descriptor(
    desc: &mut [u8],
    tree: &Tree,
    joliet: bool,
    label: &str,
    time: IsoTime,
    sectors: u32,
) {
    let text = |field: &mut [u8], s: &str| {
        if joliet {
            put_ucs2(field, s)
        } else {
            put_ascii(field, s)
        }
    };
    let label: String = label
        .chars()
        .map(if joliet { joliet_char } else { d_char })
        .collect();
    desc[0] = if joliet { 2 } else { 1 };
    desc[1..6].copy_from_slice(b"CD001");
    desc[6] = 1;
    text(&mut desc[8..40], "");
    text(&mut desc[40..72], &label);
    both32(&mut desc[80..88], sectors);
    if joliet {
        desc[88..91].copy_from_slice(b"%/E");
    }
    both16(&mut desc[120..124], 1);
    both16(&mut desc[124..128], 1);
    both16(&mut desc[128..132], SECTOR_SIZE as u16);
    both32(&mut desc[132..140], tree.table_size);
    desc[140..144].copy_from_slice(&tree.l_table.to_le_bytes());
    desc[148..152].copy_from_slice(&tree.m_table.to_be_bytes());
    put_record(
        &mut desc[156..190],
        tree.root_extent,
        tree.root_size,
        true,
        &[0],
        time,
    );
    for field in [
        190..318,
        318..446,
        446..574,
        574..702,
        702..739,
        739..776,
        776..813,
    ] {
        text(&mut desc[field], "");
    }
    put_date(&mut desc[813..830], Some(time));
    put_date(&mut desc[830..847], Some(time));
    put_date(&mut desc[847..864], None);
    put_date(&mut desc[864..881], None);
    desc[881] = 1;
}

/// Space-padded a/d-character field.
fn put_ascii(field: &mut [u8], s: &str) {
    for (byte, c) in field.iter_mut().zip(s.bytes().chain(iter::repeat(b' '))) {
        *byte = c;
    }
}

/// UCS-2BE field padded with U+0020; an odd final byte stays zero.
fn put_ucs2(field: &mut [u8], s: &str) {
    let units = s.encode_utf16().chain(iter::repeat(0x20));
    for (pair, unit) in field.chunks_exact_mut(2).zip(units) {
        pair.copy_from_slice(&unit.to_be_bytes());
    }
}

/// ECMA-119 8.4.26.1 date; `None` is "not specified".
fn put_date(field: &mut [u8], time: Option<IsoTime>) {
    field[..16].fill(b'0');
    field[16] = 0;
    let Some(t) = time else { return };
    let parts: [u16; 7] = [
        t.year / 100,
        t.year % 100,
        t.month.into(),
        t.day.into(),
        t.hour.into(),
        t.minute.into(),
        t.second.into(),
    ];
    for (pair, value) in field.chunks_exact_mut(2).zip(parts) {
        pair[0] = b'0' + (value / 10 % 10) as u8;
        pair[1] = b'0' + (value % 10) as u8;
    }
}

fn both16(out: &mut [u8], value: u16) {
    out[..2].copy_from_slice(&value.to_le_bytes());
    out[2..4].copy_from_slice(&value.to_be_bytes());
}

fn both32(out: &mut [u8], value: u32) {
    out[..4].copy_from_slice(&value.to_le_bytes());
    out[4..8].copy_from_slice(&value.to_be_bytes());
}

fn sector_count(image: &[u8]) -> Result<u32, String> {
    u32::try_from(image.len() / SECTOR_SIZE)
        .map_err(|_| "the image would exceed the ISO 9660 size limit".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires local refs/TinyCore-current.iso"]
    fn reads_tinycore_boot_assets() {
        let iso = std::fs::read("refs/TinyCore-current.iso").unwrap();
        let iso = Iso9660::open(&iso).unwrap();
        assert_eq!(iso.read_file("boot/vmlinuz").unwrap().len(), 6_087_168);
        assert_eq!(iso.read_file("boot/core.gz").unwrap().len(), 13_952_288);
    }

    #[test]
    fn replaces_smaller_file_in_place_and_updates_directory_size() {
        let mut iso = synthetic_iso();
        let before = Iso9660::open(&iso).unwrap();
        assert_eq!(before.read_file("boot/core.gz").unwrap(), b"old core image");

        replace_file_in_place(&mut iso, "boot/core.gz", b"new").unwrap();

        let after = Iso9660::open(&iso).unwrap();
        assert_eq!(
            after.file_info("boot/core.gz").unwrap(),
            IsoFileInfo {
                extent: 22,
                size: 3
            }
        );
        assert_eq!(after.read_file("boot/core.gz").unwrap(), b"new");
        let extent = 22 * SECTOR_SIZE;
        assert_eq!(&iso[extent + 3..extent + 14], &[0; 11]);
    }

    #[test]
    fn rejects_in_place_replacement_that_exceeds_original_file_size() {
        let mut iso = synthetic_iso();
        let err = replace_file_in_place(&mut iso, "boot/core.gz", b"this replacement is too big")
            .unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn growing_replacement_appends_new_extent_and_updates_volume_size() {
        let mut iso = synthetic_iso();

        replace_file(&mut iso, "boot/core.gz", b"replacement core image").unwrap();

        let patched = Iso9660::open(&iso).unwrap();
        assert_eq!(
            patched.read_file("boot/core.gz").unwrap(),
            b"replacement core image"
        );
        let info = patched.file_info("boot/core.gz").unwrap();
        assert_eq!(info.extent, 24);
        assert_eq!(info.size, 22);
        assert_eq!(
            le_u32(&iso[16 * SECTOR_SIZE + 80..16 * SECTOR_SIZE + 84]),
            25
        );
    }

    const TIME: IsoTime = IsoTime {
        year: 2026,
        month: 11,
        day: 8,
        hour: 12,
        minute: 34,
        second: 56,
    };

    fn put(builder: &mut IsoBuilder, path: &str, data: &[u8]) {
        builder.begin_file(path, data.len() as u64).unwrap();
        builder.write(data).unwrap();
    }

    fn big_file() -> Vec<u8> {
        (0..5000u32).map(|i| i as u8).collect()
    }

    fn built_iso() -> Vec<u8> {
        let mut builder = IsoBuilder::new("r86 media", TIME);
        put(&mut builder, "docs/readme.txt", b"hello");
        put(&mut builder, "docs/sub/deep/file.bin", b"deep");
        put(
            &mut builder,
            "Long file name \u{2014} \u{fc}nicode.txt",
            b"long",
        );
        put(&mut builder, "longname1.txt", b"one");
        put(&mut builder, "longname2.txt", b"two");
        put(&mut builder, &format!("{}.txt", "a".repeat(80)), b"eighty");
        put(&mut builder, "empty.dat", b"");
        builder.add_dir("nothing/here").unwrap();
        for i in 0..70 {
            put(
                &mut builder,
                &format!("many/file{i:03}.txt"),
                i.to_string().as_bytes(),
            );
        }
        let big = big_file();
        builder.begin_file("big.bin", big.len() as u64).unwrap();
        builder.write(&big[..3000]).unwrap();
        builder.write(&big[3000..]).unwrap();
        assert_eq!(builder.files(), 78);
        assert_eq!(builder.len() % SECTOR_SIZE as u64, 0);
        builder.finish().unwrap()
    }

    /// (name, extent, size) of the records in one Joliet directory, without `.`/`..`.
    fn joliet_dir(image: &[u8], extent: u32, size: u32) -> Vec<(String, u32, u32)> {
        let bytes = &image[extent as usize * SECTOR_SIZE..][..size as usize];
        let mut entries = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let len = bytes[off] as usize;
            if len == 0 {
                off = (off / SECTOR_SIZE + 1) * SECTOR_SIZE;
                continue;
            }
            let record = &bytes[off..off + len];
            let id = &record[33..33 + record[32] as usize];
            if id.len() > 1 {
                let units: Vec<u16> = id
                    .chunks(2)
                    .map(|p| u16::from_be_bytes([p[0], p[1]]))
                    .collect();
                let name = String::from_utf16(&units).unwrap();
                entries.push((name, le_u32(&record[2..]), le_u32(&record[10..])));
            }
            off += len;
        }
        entries
    }

    /// (identifier, parent number) of each record in the PVD's L or M path table.
    fn path_table(image: &[u8], big: bool) -> Vec<(String, u16)> {
        let pvd = &image[16 * SECTOR_SIZE..17 * SECTOR_SIZE];
        let location = if big {
            u32::from_be_bytes(pvd[148..152].try_into().unwrap())
        } else {
            le_u32(&pvd[140..])
        };
        let table = &image[location as usize * SECTOR_SIZE..][..le_u32(&pvd[132..]) as usize];
        let mut entries = Vec::new();
        let mut off = 0;
        while off < table.len() {
            let len = table[off] as usize;
            let parent = [table[off + 6], table[off + 7]];
            let parent = if big {
                u16::from_be_bytes(parent)
            } else {
                u16::from_le_bytes(parent)
            };
            let id = String::from_utf8_lossy(&table[off + 8..off + 8 + len]).into_owned();
            entries.push((id, parent));
            off += 8 + len + len % 2;
        }
        entries
    }

    #[test]
    fn builder_writes_readable_iso_and_joliet_trees() {
        let image = built_iso();
        assert_eq!(image, built_iso());
        let iso = Iso9660::open(&image).unwrap();
        assert_eq!(iso.read_file("docs/readme.txt").unwrap(), b"hello");
        assert_eq!(iso.read_file("docs/sub/deep/file.bin").unwrap(), b"deep");
        assert_eq!(iso.read_file("longfi~1.txt").unwrap(), b"long");
        assert_eq!(iso.read_file("longna~1.txt").unwrap(), b"one");
        assert_eq!(iso.read_file("longna~2.txt").unwrap(), b"two");
        assert_eq!(iso.read_file("aaaaaa~1.txt").unwrap(), b"eighty");
        assert_eq!(iso.read_file("empty.dat").unwrap(), b"");
        assert_eq!(iso.read_file("many/file069.txt").unwrap(), b"69");
        assert_eq!(iso.read_file("big.bin").unwrap(), big_file());
        assert!(
            iso.read_file("nothing/here")
                .unwrap_err()
                .contains("directory")
        );
        let sectors = (image.len() / SECTOR_SIZE) as u32;
        assert_eq!(le_u32(&image[16 * SECTOR_SIZE + 80..]), sectors);
        assert_eq!(le_u32(&image[17 * SECTOR_SIZE + 80..]), sectors);

        let expected: Vec<(String, u16)> = [
            ("\0", 1),
            ("DOCS", 1),
            ("MANY", 1),
            ("NOTHING", 1),
            ("SUB", 2),
            ("HERE", 4),
            ("DEEP", 5),
        ]
        .map(|(id, parent)| (id.to_string(), parent))
        .into();
        assert_eq!(path_table(&image, false), expected);
        assert_eq!(path_table(&image, true), expected);

        let svd = &image[17 * SECTOR_SIZE..18 * SECTOR_SIZE];
        assert_eq!(
            (svd[0], &svd[1..6], &svd[88..91]),
            (2, &b"CD001"[..], &b"%/E"[..])
        );
        let root = joliet_dir(&image, le_u32(&svd[158..]), le_u32(&svd[166..]));
        let has = |name: &str| root.iter().any(|entry| entry.0 == name);
        assert!(has("Long file name \u{2014} \u{fc}nicode.txt;1"));
        assert!(has(&format!("{}~1.txt;1", "a".repeat(58))));
        let many = root.iter().find(|entry| entry.0 == "many").unwrap();
        assert_eq!(many.2, 3 * SECTOR_SIZE as u32);
        assert_eq!(joliet_dir(&image, many.1, many.2).len(), 70);
    }

    #[test]
    fn builder_rejects_bad_uploads() {
        let mut builder = IsoBuilder::new("x", TIME);
        for path in ["", "a//b", "/a", "a/", "a/./b", "../a"] {
            assert!(builder.begin_file(path, 0).is_err(), "{path}");
        }
        assert!(
            builder
                .begin_file("huge", 1 << 32)
                .unwrap_err()
                .contains("4 GiB")
        );
        builder.begin_file("f", 2).unwrap();
        assert!(builder.write(b"abc").is_err());
        builder.write(b"a").unwrap();
        assert!(builder.add_dir("d").unwrap_err().contains("incomplete"));
        builder.write(b"b").unwrap();
        assert!(builder.begin_file("f", 0).unwrap_err().contains("exists"));
        assert!(builder.add_dir("f").unwrap_err().contains("not a folder"));
        assert!(
            builder
                .begin_file("f/g", 0)
                .unwrap_err()
                .contains("not a folder")
        );
        builder.add_dir("1/2/3/4/5/6/7").unwrap();
        builder.begin_file("1/2/3/4/5/6/7/f", 0).unwrap();
        assert!(
            builder
                .add_dir("1/2/3/4/5/6/7/8")
                .unwrap_err()
                .contains("too deeply")
        );
        builder.add_dir("1/2").unwrap();
        builder.begin_file("g", 1).unwrap();
        assert!(builder.finish().unwrap_err().contains("incomplete"));
    }

    fn synthetic_iso() -> Vec<u8> {
        let mut iso = vec![0; 24 * SECTOR_SIZE];
        iso[16 * SECTOR_SIZE] = 1;
        iso[16 * SECTOR_SIZE + 1..16 * SECTOR_SIZE + 6].copy_from_slice(b"CD001");
        iso[16 * SECTOR_SIZE + 6] = 1;
        iso[16 * SECTOR_SIZE + 80..16 * SECTOR_SIZE + 84].copy_from_slice(&24u32.to_le_bytes());
        iso[16 * SECTOR_SIZE + 84..16 * SECTOR_SIZE + 88].copy_from_slice(&24u32.to_be_bytes());
        let root = dir_record(20, SECTOR_SIZE as u32, 0x02, &[0]);
        iso[16 * SECTOR_SIZE + 156..16 * SECTOR_SIZE + 156 + root.len()].copy_from_slice(&root);
        iso[17 * SECTOR_SIZE] = 255;
        iso[17 * SECTOR_SIZE + 1..17 * SECTOR_SIZE + 6].copy_from_slice(b"CD001");
        iso[17 * SECTOR_SIZE + 6] = 1;

        write_dir(
            &mut iso,
            20,
            &[
                dir_record(20, SECTOR_SIZE as u32, 0x02, &[0]),
                dir_record(20, SECTOR_SIZE as u32, 0x02, &[1]),
                dir_record(21, SECTOR_SIZE as u32, 0x02, b"BOOT"),
            ],
        );
        write_dir(
            &mut iso,
            21,
            &[
                dir_record(21, SECTOR_SIZE as u32, 0x02, &[0]),
                dir_record(20, SECTOR_SIZE as u32, 0x02, &[1]),
                dir_record(22, 14, 0x00, b"CORE.GZ;1"),
            ],
        );
        iso[22 * SECTOR_SIZE..22 * SECTOR_SIZE + 14].copy_from_slice(b"old core image");
        iso
    }

    fn write_dir(iso: &mut [u8], sector_index: usize, records: &[Vec<u8>]) {
        let mut offset = sector_index * SECTOR_SIZE;
        for record in records {
            iso[offset..offset + record.len()].copy_from_slice(record);
            offset += record.len();
        }
    }

    fn dir_record(extent: u32, size: u32, flags: u8, name: &[u8]) -> Vec<u8> {
        let padding = usize::from(name.len() % 2 == 0);
        let len = 33 + name.len() + padding;
        let mut record = vec![0; len];
        record[0] = len as u8;
        record[2..6].copy_from_slice(&extent.to_le_bytes());
        record[6..10].copy_from_slice(&extent.to_be_bytes());
        record[10..14].copy_from_slice(&size.to_le_bytes());
        record[14..18].copy_from_slice(&size.to_be_bytes());
        record[25] = flags;
        record[28..30].copy_from_slice(&1u16.to_le_bytes());
        record[30..32].copy_from_slice(&1u16.to_be_bytes());
        record[32] = name.len() as u8;
        record[33..33 + name.len()].copy_from_slice(name);
        record
    }
}
