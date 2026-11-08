use std::io::Read;

use flate2::read::GzDecoder;

use crate::initrd_overlay::{InitrdFile, append_newc_overlay};

pub const KERNEL_LOAD_ADDR: u32 = 0x0010_0000;
pub const BOOT_PARAMS_ADDR: u32 = 0x0009_0000;
pub const CMDLINE_ADDR: u32 = 0x0002_0000;
pub const DEFAULT_INITRD_LOAD_ADDR: u32 = 0x0200_0000;
const BOOT_PARAMS_SIZE: usize = 0x1000;
const INITRD_TOP_GUARD: u32 = 16 * 1024 * 1024;

pub struct BootConfig<'a> {
    pub mem_size: u32,
    pub cmdline: &'a str,
    pub host_decompress_kernel: bool,
    pub host_decompress_initrd: bool,
    pub overlay_files: &'a [InitrdFile],
    pub dump_initrd_path: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadedBoot {
    pub entry: u32,
    pub boot_params: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub initrd_load_addr: u32,
}

pub struct DirectLinuxBoot;

impl DirectLinuxBoot {
    pub fn load(
        ram: &mut [u8],
        bz_image: &[u8],
        initrd: &[u8],
        config: &BootConfig<'_>,
    ) -> Result<LoadedBoot, String> {
        let header = BzImageHeader::parse(bz_image)?;
        let payload_storage;
        let payload = if config.host_decompress_kernel {
            payload_storage = decompress_gzip_payload(bz_image, header.payload_offset)?;
            payload_storage.as_slice()
        } else {
            bz_image
                .get(header.payload_offset..)
                .ok_or("bzImage payload offset is outside the kernel image")?
        };
        let kernel_entry = load_kernel_payload(ram, payload)?;
        let mut initrd_storage;
        let initrd_payload = if config.host_decompress_initrd || !config.overlay_files.is_empty() {
            initrd_storage = if config.host_decompress_initrd {
                decompress_gzip_stream(initrd, "initrd")?
            } else {
                initrd.to_vec()
            };
            append_newc_overlay(&mut initrd_storage, config.overlay_files);
            initrd_storage.as_slice()
        } else {
            initrd
        };
        if let Some(path) = config.dump_initrd_path {
            std::fs::write(path, initrd_payload)
                .map_err(|err| format!("failed to dump prepared initrd to {path}: {err}"))?;
        }

        let initrd_load_addr = choose_initrd_load_addr(config.mem_size, initrd_payload.len())?;

        copy_to_ram(ram, initrd_load_addr, initrd_payload, "initrd")?;

        let params_start = BOOT_PARAMS_ADDR as usize;
        let params_end = params_start + BOOT_PARAMS_SIZE;
        let params = ram
            .get_mut(params_start..params_end)
            .ok_or("boot params do not fit in guest RAM")?;
        params.fill(0);

        let copy_start = 0x1f1;
        let copy_end = core::cmp::min(0x290, bz_image.len());
        params[copy_start..copy_end].copy_from_slice(&bz_image[copy_start..copy_end]);

        put_u8(params, 0x000, 0);
        put_u8(params, 0x001, 0);
        put_u8(params, 0x006, 3);
        put_u8(params, 0x007, 80);
        put_u8(params, 0x00e, 25);
        put_u8(params, 0x00f, 1);
        put_u16(params, 0x010, 16);
        put_u32(params, 0x1e0, config.mem_size / 1024 - 1024);
        put_u8(params, 0x1e8, 2);
        put_u8(params, 0x210, 0xff);
        put_u8(params, 0x211, params[0x211] | 0x81);
        put_u16(params, 0x224, 0xe000);
        put_u32(params, 0x214, KERNEL_LOAD_ADDR);
        put_u32(params, 0x218, initrd_load_addr);
        put_u32(params, 0x21c, initrd_payload.len() as u32);
        put_u32(params, 0x228, CMDLINE_ADDR);

        write_e820(params, 0, 0, 0x000a_0000, 1);
        write_e820(
            params,
            1,
            0x0010_0000,
            config.mem_size as u64 - 0x0010_0000,
            1,
        );

        let mut cmdline = config.cmdline.as_bytes().to_vec();
        cmdline.push(0);
        copy_to_ram(ram, CMDLINE_ADDR, &cmdline, "kernel command line")?;

        Ok(LoadedBoot {
            entry: kernel_entry,
            boot_params: BOOT_PARAMS_ADDR,
            kernel_payload_len: payload.len(),
            initrd_len: initrd_payload.len(),
            initrd_load_addr,
        })
    }
}

fn choose_initrd_load_addr(mem_size: u32, initrd_len: usize) -> Result<u32, String> {
    let initrd_len =
        u32::try_from(initrd_len).map_err(|_| "initrd is too large for 32-bit boot params")?;
    let guarded_top = mem_size
        .checked_sub(INITRD_TOP_GUARD)
        .ok_or("guest RAM is too small for initrd top guard")?;
    let start = guarded_top
        .checked_sub(initrd_len)
        .ok_or("initrd does not fit below guarded RAM top")?
        & !0xfff;
    if start < DEFAULT_INITRD_LOAD_ADDR {
        return Err(format!(
            "initrd does not fit without overlapping low boot memory: start={start:#x}"
        ));
    }
    Ok(start)
}

fn decompress_gzip_payload(image: &[u8], search_start: usize) -> Result<Vec<u8>, String> {
    let rel = image
        .get(search_start..)
        .ok_or("gzip search start is outside the kernel image")?
        .windows(3)
        .position(|bytes| bytes == [0x1f, 0x8b, 0x08])
        .ok_or("could not find gzip payload inside bzImage")?;
    let start = search_start + rel;
    let mut decoder = GzDecoder::new(&image[start..]);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| format!("failed to decompress gzip payload at {start:#x}: {err}"))?;
    Ok(out)
}

fn decompress_gzip_stream(bytes: &[u8], what: &str) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| format!("failed to decompress {what}: {err}"))?;
    Ok(out)
}

fn load_kernel_payload(ram: &mut [u8], payload: &[u8]) -> Result<u32, String> {
    if let Some(entry) = try_load_elf32_kernel(ram, payload)? {
        return Ok(entry);
    }
    copy_to_ram(ram, KERNEL_LOAD_ADDR, payload, "kernel payload")?;
    Ok(KERNEL_LOAD_ADDR)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Elf32LoadSegment {
    offset: usize,
    vaddr: u32,
    paddr: u32,
    filesz: usize,
    memsz: usize,
}

fn try_load_elf32_kernel(ram: &mut [u8], image: &[u8]) -> Result<Option<u32>, String> {
    if !image.starts_with(b"\x7fELF") {
        return Ok(None);
    }
    if image.get(4) != Some(&1) || image.get(5) != Some(&1) {
        return Err("host-decompressed kernel ELF is not 32-bit little-endian".to_string());
    }
    let machine = read_u16(image, 18, "ELF e_machine")?;
    if machine != 3 {
        return Err(format!(
            "host-decompressed kernel ELF is not i386: e_machine={machine}"
        ));
    }
    let entry = read_u32(image, 24, "ELF e_entry")?;
    let phoff = usize::try_from(read_u32(image, 28, "ELF e_phoff")?)
        .map_err(|_| "ELF program header offset does not fit usize".to_string())?;
    let phentsize = usize::from(read_u16(image, 42, "ELF e_phentsize")?);
    let phnum = usize::from(read_u16(image, 44, "ELF e_phnum")?);
    if phentsize < 32 {
        return Err(format!(
            "ELF program header entry is too small: {phentsize}"
        ));
    }

    let mut segments = Vec::new();
    for index in 0..phnum {
        let off = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or("ELF program header index overflows")?,
            )
            .ok_or("ELF program header offset overflows")?;
        let p_type = read_u32(image, off, "ELF p_type")?;
        if p_type != 1 {
            continue;
        }
        let p_offset = usize::try_from(read_u32(image, off + 4, "ELF p_offset")?)
            .map_err(|_| "ELF segment file offset does not fit usize".to_string())?;
        let p_vaddr = read_u32(image, off + 8, "ELF p_vaddr")?;
        let p_paddr = read_u32(image, off + 12, "ELF p_paddr")?;
        let p_filesz = usize::try_from(read_u32(image, off + 16, "ELF p_filesz")?)
            .map_err(|_| "ELF segment file size does not fit usize".to_string())?;
        let p_memsz = usize::try_from(read_u32(image, off + 20, "ELF p_memsz")?)
            .map_err(|_| "ELF segment memory size does not fit usize".to_string())?;
        if p_filesz > p_memsz {
            return Err("ELF segment file size exceeds memory size".to_string());
        }
        let file_end = p_offset
            .checked_add(p_filesz)
            .ok_or("ELF segment file range overflows")?;
        if file_end > image.len() {
            return Err("ELF segment file range exceeds kernel image".to_string());
        }
        segments.push(Elf32LoadSegment {
            offset: p_offset,
            vaddr: p_vaddr,
            paddr: p_paddr,
            filesz: p_filesz,
            memsz: p_memsz,
        });
    }
    if segments.is_empty() {
        return Err("host-decompressed kernel ELF has no loadable segments".to_string());
    }

    for segment in &segments {
        zero_ram(ram, segment.paddr, segment.memsz, "ELF kernel segment")?;
    }
    for segment in &segments {
        copy_to_ram(
            ram,
            segment.paddr,
            &image[segment.offset..segment.offset + segment.filesz],
            "ELF kernel segment",
        )?;
    }

    for segment in &segments {
        if let Some(phys) = entry_in_segment(entry, segment.paddr, segment.paddr, segment.memsz) {
            return Ok(Some(phys));
        }
    }
    for segment in &segments {
        if let Some(phys) = entry_in_segment(entry, segment.vaddr, segment.paddr, segment.memsz) {
            return Ok(Some(phys));
        }
    }
    Err(format!(
        "ELF entry {entry:#x} is outside every loadable kernel segment"
    ))
}

fn entry_in_segment(entry: u32, base: u32, paddr: u32, len: usize) -> Option<u32> {
    let len = u32::try_from(len).ok()?;
    let offset = entry.checked_sub(base)?;
    if offset < len {
        paddr.checked_add(offset)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BzImageHeader {
    payload_offset: usize,
}

impl BzImageHeader {
    fn parse(image: &[u8]) -> Result<Self, String> {
        if image.len() < 0x400 {
            return Err("kernel image is too small for a Linux setup header".to_string());
        }
        if &image[0x202..0x206] != b"HdrS" {
            return Err("kernel image is missing the Linux HdrS signature".to_string());
        }
        let version = u16::from_le_bytes([image[0x206], image[0x207]]);
        if version < 0x0200 {
            return Err(format!(
                "unsupported old Linux boot protocol version {version:#06x}"
            ));
        }
        let setup_sects = match image[0x1f1] {
            0 => 4,
            value => value as usize,
        };
        Ok(Self {
            payload_offset: (setup_sects + 1) * 512,
        })
    }
}

fn copy_to_ram(ram: &mut [u8], addr: u32, bytes: &[u8], what: &str) -> Result<(), String> {
    let start = addr as usize;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| format!("{what} address overflows"))?;
    let dst = ram
        .get_mut(start..end)
        .ok_or_else(|| format!("{what} does not fit in guest RAM at {addr:#x}"))?;
    dst.copy_from_slice(bytes);
    Ok(())
}

fn zero_ram(ram: &mut [u8], addr: u32, len: usize, what: &str) -> Result<(), String> {
    let start = addr as usize;
    let end = start
        .checked_add(len)
        .ok_or_else(|| format!("{what} address overflows"))?;
    let dst = ram
        .get_mut(start..end)
        .ok_or_else(|| format!("{what} does not fit in guest RAM at {addr:#x}"))?;
    dst.fill(0);
    Ok(())
}

fn read_u16(bytes: &[u8], off: usize, what: &str) -> Result<u16, String> {
    let data = bytes
        .get(off..off + 2)
        .ok_or_else(|| format!("{what} is outside the buffer"))?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

fn read_u32(bytes: &[u8], off: usize, what: &str) -> Result<u32, String> {
    let data = bytes
        .get(off..off + 4)
        .ok_or_else(|| format!("{what} is outside the buffer"))?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn write_e820(params: &mut [u8], index: usize, addr: u64, size: u64, typ: u32) {
    let off = 0x2d0 + index * 20;
    put_u64(params, off, addr);
    put_u64(params, off + 8, size);
    put_u32(params, off + 16, typ);
}

fn put_u8(bytes: &mut [u8], off: usize, value: u8) {
    bytes[off] = value;
}

fn put_u16(bytes: &mut [u8], off: usize, value: u16) {
    bytes[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn tinycore_kernel() -> Vec<u8> {
        let iso = std::fs::read("refs/TinyCore-current.iso").unwrap();
        crate::iso9660::Iso9660::open(&iso)
            .unwrap()
            .read_file("boot/vmlinuz")
            .unwrap()
    }

    #[test]
    #[ignore = "requires local refs/TinyCore-current.iso"]
    fn parses_tinycore_bzimage_header() {
        let kernel = tinycore_kernel();
        let header = BzImageHeader::parse(&kernel).unwrap();
        assert_eq!(header.payload_offset, 0x4000);
    }

    #[test]
    fn lays_out_direct_boot_without_overlap() {
        let kernel = vec![0u8; 0x5000];
        let mut bz = kernel;
        bz[0x1f1] = 31;
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
        let initrd = vec![0xaa; 4096];
        let mut ram = vec![0; 64 * 1024 * 1024];
        let mem_size = ram.len() as u32;
        let loaded = DirectLinuxBoot::load(
            &mut ram,
            &bz,
            &initrd,
            &BootConfig {
                mem_size,
                cmdline: "console=ttyS0",
                host_decompress_kernel: false,
                host_decompress_initrd: false,
                overlay_files: &[],
                dump_initrd_path: None,
            },
        )
        .unwrap();
        assert_eq!(loaded.entry, KERNEL_LOAD_ADDR);
        assert!(loaded.initrd_load_addr >= DEFAULT_INITRD_LOAD_ADDR);
        assert_eq!(
            &ram[loaded.initrd_load_addr as usize..loaded.initrd_load_addr as usize + 4],
            &[0xaa; 4]
        );
        assert_eq!(
            u32::from_le_bytes([
                ram[BOOT_PARAMS_ADDR as usize + 0x228],
                ram[BOOT_PARAMS_ADDR as usize + 0x229],
                ram[BOOT_PARAMS_ADDR as usize + 0x22a],
                ram[BOOT_PARAMS_ADDR as usize + 0x22b],
            ]),
            CMDLINE_ADDR
        );
    }

    #[test]
    fn host_decompressed_kernel_loads_elf_segments_by_physical_address() {
        let mut bz = vec![0u8; 0x4000];
        bz[0x1f1] = 31;
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
        bz.extend_from_slice(&gzip_bytes(&synthetic_elf32_kernel()));
        let initrd = vec![0xaa; 16];
        let mut ram = vec![0xcc; 64 * 1024 * 1024];

        let loaded = DirectLinuxBoot::load(
            &mut ram,
            &bz,
            &initrd,
            &BootConfig {
                mem_size: 64 * 1024 * 1024,
                cmdline: "console=ttyS0",
                host_decompress_kernel: true,
                host_decompress_initrd: false,
                overlay_files: &[],
                dump_initrd_path: None,
            },
        )
        .unwrap();

        assert_eq!(loaded.entry, 0x0020_0004);
        assert_eq!(&ram[0x0020_0000..0x0020_0004], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(&ram[0x0020_0004..0x0020_0008], &[0; 4]);
        assert_eq!(loaded.kernel_payload_len, synthetic_elf32_kernel().len());
    }

    #[test]
    #[ignore = "requires local refs/TinyCore-current.iso"]
    fn host_decompressed_tinycore_kernel_does_not_jump_to_elf_header() {
        let kernel = tinycore_kernel();
        let initrd = vec![0xaa; 16];
        let mut ram = vec![0; 128 * 1024 * 1024];

        let loaded = DirectLinuxBoot::load(
            &mut ram,
            &kernel,
            &initrd,
            &BootConfig {
                mem_size: 128 * 1024 * 1024,
                cmdline: "console=ttyS0",
                host_decompress_kernel: true,
                host_decompress_initrd: false,
                overlay_files: &[],
                dump_initrd_path: None,
            },
        )
        .unwrap();

        assert_eq!(loaded.entry, 0x00c0_72d0);
        assert_ne!(
            &ram[KERNEL_LOAD_ADDR as usize..KERNEL_LOAD_ADDR as usize + 4],
            b"\x7fELF"
        );
    }

    #[test]
    fn appends_uncompressed_initrd_overlay_before_copying_to_guest_ram() {
        let kernel = vec![0u8; 0x5000];
        let mut bz = kernel;
        bz[0x1f1] = 31;
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
        let initrd = vec![0xaa; 16];
        let overlay = [InitrdFile::new("etc/r86-smoke", 0o644, b"ok\n".to_vec()).unwrap()];
        let mut ram = vec![0; 64 * 1024 * 1024];
        let mem_size = ram.len() as u32;

        let loaded = DirectLinuxBoot::load(
            &mut ram,
            &bz,
            &initrd,
            &BootConfig {
                mem_size,
                cmdline: "console=ttyS0",
                host_decompress_kernel: false,
                host_decompress_initrd: false,
                overlay_files: &overlay,
                dump_initrd_path: None,
            },
        )
        .unwrap();

        let copied = &ram[loaded.initrd_load_addr as usize
            ..loaded.initrd_load_addr as usize + loaded.initrd_len];
        assert!(copied.starts_with(&initrd));
        assert!(
            copied
                .windows(b"etc/r86-smoke".len())
                .any(|w| w == b"etc/r86-smoke")
        );
        assert!(
            copied
                .windows(b"TRAILER!!!".len())
                .any(|w| w == b"TRAILER!!!")
        );
    }

    fn gzip_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn synthetic_elf32_kernel() -> Vec<u8> {
        let mut elf = vec![0u8; 0x104];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 1;
        elf[5] = 1;
        elf[6] = 1;
        put_u16(&mut elf, 16, 2);
        put_u16(&mut elf, 18, 3);
        put_u32(&mut elf, 20, 1);
        put_u32(&mut elf, 24, 0x0020_0004);
        put_u32(&mut elf, 28, 52);
        put_u16(&mut elf, 40, 52);
        put_u16(&mut elf, 42, 32);
        put_u16(&mut elf, 44, 1);
        put_u32(&mut elf, 52, 1);
        put_u32(&mut elf, 56, 0x100);
        put_u32(&mut elf, 60, 0xc020_0000);
        put_u32(&mut elf, 64, 0x0020_0000);
        put_u32(&mut elf, 68, 4);
        put_u32(&mut elf, 72, 8);
        put_u32(&mut elf, 76, 5);
        put_u32(&mut elf, 80, 0x1000);
        elf[0x100..0x104].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        elf
    }
}
