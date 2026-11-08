//! Guest OS presets and detection from installation media or hard disks.
//! Detection only chooses defaults for a new VM; the user can pick any preset.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use r86::disk::Disk;

pub struct OsPreset {
    pub name: &'static str,
    pub id: &'static str,
    pub memory_mib: u32,
    pub disk_mib: u64,
}

const fn preset(name: &'static str, id: &'static str, memory_mib: u32, disk_mib: u64) -> OsPreset {
    OsPreset {
        name,
        id,
        memory_mib,
        disk_mib,
    }
}

pub const PRESETS: &[OsPreset] = &[
    preset("DOS", "dos", 16, 512),
    preset("Windows 3.1", "win31", 32, 512),
    preset("Windows 95", "win95", 32, 512),
    preset("Windows 98", "win98", 64, 512),
    preset("Windows ME", "winme", 64, 512),
    preset("Windows 2000", "win2000", 128, 4096),
    preset("Windows XP", "winxp", 128, 8192),
    preset("ReactOS", "reactos", 128, 4096),
    preset("Linux", "linux", 128, 4096),
    preset("Other", "other", 128, 2048),
];

/// Saved VMs store the preset name; unknown names use "Other".
pub fn by_name(name: &str) -> &'static OsPreset {
    PRESETS
        .iter()
        .find(|preset| preset.name == name)
        .unwrap_or(&PRESETS[PRESETS.len() - 1])
}

fn by_id(id: &str) -> Option<&'static OsPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

const ISO_SECTOR: u64 = 2048;

/// Classify an ISO9660 image from its volume label and root directory names.
pub fn detect_iso(path: &Path) -> io::Result<Option<&'static OsPreset>> {
    let mut file = File::open(path)?;
    let mut pvd = [0u8; ISO_SECTOR as usize];
    if read_at(&mut file, 16 * ISO_SECTOR, &mut pvd).is_err()
        || pvd[0] != 1
        || &pvd[1..6] != b"CD001"
    {
        return Ok(None);
    }
    let volume = String::from_utf8_lossy(&pvd[40..72])
        .trim()
        .to_ascii_uppercase();
    // The root directory record starts at byte 156: extent at +2, size at +10.
    let extent = u32::from_le_bytes(pvd[158..162].try_into().unwrap());
    let size = u32::from_le_bytes(pvd[166..170].try_into().unwrap());
    let root = read_directory(&mut file, extent, size)?;
    let mut names: Vec<String> = root.iter().map(|(name, ..)| name.clone()).collect();
    // Windows 3.1 setup disks are often copied to the CD as DISK1..DISKn.
    if let Some(&(_, extent, size)) = root.iter().find(|(name, ..)| name == "DISK1") {
        names.extend(
            read_directory(&mut file, extent, size)?
                .into_iter()
                .map(|(name, ..)| name),
        );
    }
    Ok(classify_iso(&volume, &names).and_then(by_id))
}

fn read_directory(file: &mut File, extent: u32, size: u32) -> io::Result<Vec<(String, u32, u32)>> {
    let mut directory = vec![0; size.min(64 * 2048) as usize];
    read_at(file, u64::from(extent) * ISO_SECTOR, &mut directory)?;
    Ok(iso_entries(&directory))
}

/// (name, extent, size) of each record, without "." and "..".
fn iso_entries(directory: &[u8]) -> Vec<(String, u32, u32)> {
    let mut names = Vec::new();
    let mut pos = 0;
    while pos < directory.len() {
        let len = usize::from(directory[pos]);
        if len == 0 {
            // Records never cross a sector; zero padding ends this sector.
            pos = (pos / ISO_SECTOR as usize + 1) * ISO_SECTOR as usize;
            continue;
        }
        let Some(record) = directory.get(pos..pos + len) else {
            break;
        };
        let name_len = usize::from(*record.get(32).unwrap_or(&0));
        if let Some(name) = record.get(33..33 + name_len)
            && name != [0]
            && name != [1]
        {
            let name = String::from_utf8_lossy(name).to_ascii_uppercase();
            let name = name.split(';').next().unwrap_or_default();
            let extent = u32::from_le_bytes(record[2..6].try_into().unwrap());
            let size = u32::from_le_bytes(record[10..14].try_into().unwrap());
            names.push((name.trim_end_matches('.').to_string(), extent, size));
        }
        pos += len;
    }
    names
}

fn classify_iso(volume: &str, names: &[String]) -> Option<&'static str> {
    let has = |name: &str| names.iter().any(|n| n == name);
    let prefix = |p: &str| names.iter().any(|n| n.starts_with(p));
    Some(if prefix("WIN51") || prefix("WIN52") {
        "winxp"
    } else if names
        .iter()
        .any(|n| n.starts_with("CDROM_") && n.ends_with(".5"))
    {
        "win2000"
    } else if has("WIN9X") {
        "winme"
    } else if has("WIN98") {
        "win98"
    } else if has("WIN95") {
        "win95"
    } else if has("SETUP.EXE")
        && [
            "WIN.CN_",
            "KRNL386.EX_",
            "GDI.EX_",
            "USER.EX_",
            "WINVER.EX_",
        ]
        .iter()
        .any(|name| has(name))
    {
        "win31"
    } else if has("REACTOS") || volume.starts_with("REACTOS") {
        "reactos"
    } else if has("FREEDOS") || volume.starts_with("FD1") || volume.contains("DOS") {
        "dos"
    } else if [
        "ISOLINUX", "SYSLINUX", "CASPER", "LIVE", "CDE", "ARCH", "BOOT",
    ]
    .iter()
    .any(|name| has(name))
    {
        "linux"
    } else {
        return None;
    })
}

/// Classify a raw or VHD hard disk from its partition table and boot sector.
pub fn detect_disk(path: &Path) -> Result<Option<&'static OsPreset>, String> {
    let mut disk = Disk::open(path, false).map_err(|error| error.to_string())?;
    let mut sector = [0u8; 512];
    disk.read(0, &mut sector)
        .map_err(|error| error.to_string())?;
    if sector[510..] != [0x55, 0xaa] {
        return Ok(None);
    }
    let mut partition_type = 0;
    if !is_boot_sector(&sector) {
        let entries = sector[446..510].chunks_exact(16);
        let Some(entry) = entries
            .clone()
            .find(|e| e[0] == 0x80 && e[4] != 0)
            .or_else(|| entries.clone().find(|e| e[4] != 0))
        else {
            return Ok(None);
        };
        partition_type = entry[4];
        if partition_type == 0x83 {
            return Ok(by_id("linux"));
        }
        let start = u32::from_le_bytes(entry[8..12].try_into().unwrap());
        disk.read(u64::from(start), &mut sector)
            .map_err(|error| error.to_string())?;
    }
    let fat32 = matches!(partition_type, 0x0b | 0x0c) || &sector[82..87] == b"FAT32";
    Ok(classify_boot_sector(&sector[3..11], fat32).and_then(by_id))
}

fn is_boot_sector(sector: &[u8; 512]) -> bool {
    matches!(sector[0], 0xeb | 0xe9) && u16::from_le_bytes([sector[11], sector[12]]) == 512
}

fn classify_boot_sector(oem: &[u8], fat32: bool) -> Option<&'static str> {
    let oem = String::from_utf8_lossy(oem).to_ascii_uppercase();
    Some(match oem.as_str() {
        "NTFS    " => "winxp",
        "MSWIN4.1" => "win98",
        "MSWIN4.0" => "win95",
        // Windows 9x stamps volumes it mounts with a random prefix and "IHC".
        oem if oem.ends_with("IHC") => "win98",
        "MSDOS5.0" if fat32 => "winxp",
        "MSDOS5.0" | "IBM  3.3" | "IBM  7.1" => "dos",
        oem if oem.starts_with("FRDOS") || oem.starts_with("FREEDOS") => "dos",
        oem if oem.starts_with("FRLDR") => "reactos",
        _ => return None,
    })
}

fn read_at(file: &mut File, offset: u64, data: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn install_media_markers_select_presets() {
        let cases: &[(&str, &[&str], Option<&str>)] = &[
            ("WXPFPP_EN", &["I386", "WIN51", "WIN51IP"], Some("winxp")),
            ("W2PFPP_EN", &["I386", "CDROM_NT.5"], Some("win2000")),
            ("WIN98 SE", &["WIN98", "TOOLS"], Some("win98")),
            ("WINME", &["WIN9X", "ADD-ONS"], Some("winme")),
            (
                "WIN31",
                &["SETUP.EXE", "SETUP.INF", "KRNL386.EX_"],
                Some("win31"),
            ),
            (
                "CDROM",
                &["DISK1", "DISK2", "SETUP.EXE", "WIN.CN_"],
                Some("win31"),
            ),
            ("OFFICE", &["SETUP.EXE", "SETUP.INF"], None),
            ("REACTOS", &["LOADER", "REACTOS"], Some("reactos")),
            ("FD13-LIVE", &["ISOLINUX", "PACKAGES"], Some("dos")),
            ("CORE", &["BOOT"], Some("linux")),
            ("DATA", &["MUSIC"], None),
        ];
        for (volume, list, expected) in cases {
            assert_eq!(classify_iso(volume, &names(list)), *expected, "{volume}");
        }
    }

    #[test]
    fn root_records_parse_across_sector_padding() {
        let mut directory = vec![0u8; 4096];
        for (offset, name) in [(0, &b"\0"[..]), (34, b"WIN98"), (2048, b"SETUP.EXE;1")] {
            let len = 33 + name.len() + (name.len() + 1) % 2;
            directory[offset] = len as u8;
            directory[offset + 32] = name.len() as u8;
            directory[offset + 33..offset + 33 + name.len()].copy_from_slice(name);
        }
        let names: Vec<_> = iso_entries(&directory)
            .into_iter()
            .map(|(name, ..)| name)
            .collect();
        assert_eq!(names, ["WIN98", "SETUP.EXE"]);
    }

    #[test]
    fn boot_sector_oem_ids_select_presets() {
        assert_eq!(classify_boot_sector(b"NTFS    ", false), Some("winxp"));
        assert_eq!(classify_boot_sector(b"MSWIN4.1", true), Some("win98"));
        assert_eq!(classify_boot_sector(b"2+d!$IHC", false), Some("win98"));
        assert_eq!(classify_boot_sector(b"MSDOS5.0", false), Some("dos"));
        assert_eq!(classify_boot_sector(b"MSDOS5.0", true), Some("winxp"));
        assert_eq!(classify_boot_sector(b"FRDOS4.1", false), Some("dos"));
        assert_eq!(classify_boot_sector(b"mkfs.fat", true), None);
    }
}
