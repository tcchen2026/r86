use alloc::vec::Vec;

#[cfg(feature = "monitor")]
mod backing;
#[cfg(feature = "monitor")]
use backing::Vram;
#[cfg(not(feature = "monitor"))]
type Vram = Vec<u8>;

use font8x8::legacy::BASIC_LEGACY;

pub const LEGACY_VGA_BASE: u64 = 0x000a_0000;
pub const LEGACY_VGA_LEN: u64 = 0x0002_0000;
pub const DEFAULT_LFB_BASE: u64 = 0xe000_0000;
pub const DEFAULT_LFB_LEN: u64 = 0x0100_0000;
pub const VRAM_SIZE: usize = DEFAULT_LFB_LEN as usize;
const VRAM_PAGES: usize = VRAM_SIZE / 4096;

const VBE_INDEX_ID: usize = 0;
const VBE_INDEX_XRES: usize = 1;
const VBE_INDEX_YRES: usize = 2;
const VBE_INDEX_BPP: usize = 3;
const VBE_INDEX_ENABLE: usize = 4;
const VBE_INDEX_BANK: usize = 5;
const VBE_INDEX_VIRT_WIDTH: usize = 6;
const VBE_INDEX_VIRT_HEIGHT: usize = 7;
const VBE_INDEX_X_OFFSET: usize = 8;
const VBE_INDEX_Y_OFFSET: usize = 9;
const VBE_INDEX_VIDEO_MEMORY_64K: usize = 10;
const VBE_REGS: usize = 11;

const VBE_ID0: u16 = 0xb0c0;
const VBE_ID5: u16 = 0xb0c5;
const VBE_ENABLED: u16 = 0x0001;
const VBE_GETCAPS: u16 = 0x0002;
const VBE_8BIT_DAC: u16 = 0x0020;
const VBE_NOCLEARMEM: u16 = 0x0080;
const VBE_MAX_XRES: u16 = 1920;
const VBE_MAX_YRES: u16 = 1600;
const VBE_MAX_BPP: u16 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderMode {
    Text,
    Chain4,
    Planar,
    Vbe,
    Blank,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VgaModeInfo {
    pub mode: RenderMode,
    pub width: u16,
    pub height: u16,
    pub bpp: u16,
    pub stride: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor_offset: u16,
    pub cells: Vec<u8>,
    pub raw_cells: Vec<[u8; 2]>,
    pub cursor: Option<(u16, u16)>,
    pub blink_enabled: bool,
}

impl TextSnapshot {
    pub fn line_bytes(&self, row: usize) -> Option<&[u8]> {
        let cols = self.cols as usize;
        self.cells
            .get(row.checked_mul(cols)?..row.checked_add(1)?.checked_mul(cols)?)
    }
}

/// Independent consumers of the frame-change flag: a frame shown on the
/// device and one sent to remote consoles must not clear each other's update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    /// The machine's own display: SDL window, LCD, terminal or browser canvas.
    Local = 0,
    /// The WebSocket console.
    Remote = 1,
}

#[derive(Clone, Debug)]
pub struct PcVga {
    vram: Vram,
    dirty_pages: Vec<u8>,
    /// One bit per viewer (see `Viewer`); any frame change sets all of them.
    frame_dirty: u8,
    misc_output: u8,
    feature_control: u8,
    seq_index: u8,
    seq: [u8; 5],
    crtc_index: u8,
    crtc: [u8; 0x19],
    gc_index: u8,
    gc: [u8; 9],
    attr_index: u8,
    attr: [u8; 0x15],
    attr_address_phase: bool,
    dac_write_index: u8,
    dac_read_index: u8,
    dac_sub_index: u8,
    dac_state: u8,
    pel_mask: u8,
    dac: [u8; 0x300],
    latch: [u8; 4],
    vbe_index: u16,
    vbe_regs: [u16; VBE_REGS],
    vbe_stride: u32,
    vbe_start_addr: u32,
    invalid_vbe_bpp: u64,
}

impl PcVga {
    pub fn new() -> Self {
        #[cfg(feature = "monitor")]
        let vram = Vram::zeroed();
        #[cfg(not(feature = "monitor"))]
        let vram = alloc::vec![0; VRAM_SIZE];
        Self::with_backing(vram, alloc::vec![0; VRAM_PAGES / 8])
    }

    /// Move the buffers into a reset device while its owning bus is replaced.
    /// The caller must discard this emptied old device after taking the result.
    pub(crate) fn take_for_reset(&mut self) -> Self {
        let mut vram = core::mem::take(&mut self.vram);
        let mut dirty_pages = core::mem::take(&mut self.dirty_pages);
        vram.fill(0);
        dirty_pages.fill(0);
        Self::with_backing(vram, dirty_pages)
    }

    fn with_backing(vram: Vram, dirty_pages: Vec<u8>) -> Self {
        let mut dev = Self {
            vram,
            dirty_pages,
            frame_dirty: u8::MAX,
            misc_output: 0x01,
            feature_control: 0,
            seq_index: 0,
            seq: [0; 5],
            crtc_index: 0,
            crtc: [0; 0x19],
            gc_index: 0,
            gc: [0; 9],
            attr_index: 0x20,
            attr: [0; 0x15],
            attr_address_phase: true,
            dac_write_index: 0,
            dac_read_index: 0,
            dac_sub_index: 0,
            dac_state: 0,
            pel_mask: 0xff,
            dac: [0; 0x300],
            latch: [0; 4],
            vbe_index: 0,
            vbe_regs: [0; VBE_REGS],
            vbe_stride: 0,
            vbe_start_addr: 0,
            invalid_vbe_bpp: 0,
        };
        dev.seq[2] = 0x0f;
        dev.seq[4] = 0x06;
        dev.gc[4] = 0;
        dev.gc[5] = 0x10;
        dev.gc[6] = 0x0e;
        dev.gc[7] = 0x0f;
        dev.gc[8] = 0xff;
        dev.crtc[0x09] = 0x0f;
        dev.crtc[0x0a] = 0x0d;
        dev.crtc[0x0b] = 0x0e;
        for i in 0..16 {
            dev.attr[i] = i as u8;
        }
        dev.attr[0x10] = 0x0c;
        dev.attr[0x12] = 0x0f;
        dev.load_default_dac();
        dev.load_default_text_font();
        dev.vbe_regs[VBE_INDEX_ID] = VBE_ID5;
        dev.vbe_regs[VBE_INDEX_VIDEO_MEMORY_64K] = (VRAM_SIZE / 0x10000) as u16;
        dev
    }

    pub fn contains_legacy(addr: u64) -> bool {
        (LEGACY_VGA_BASE..LEGACY_VGA_BASE + LEGACY_VGA_LEN).contains(&addr)
    }

    pub fn contains_lfb(addr: u64, len: u32, lfb_base: u64) -> bool {
        if len == 0 {
            return false;
        }
        let Some(end) = addr.checked_add(u64::from(len)) else {
            return false;
        };
        (lfb_base..lfb_base + DEFAULT_LFB_LEN).contains(&addr) && end <= lfb_base + DEFAULT_LFB_LEN
    }

    pub fn vram_len(&self) -> usize {
        self.vram.len()
    }

    pub fn invalid_vbe_bpp_count(&self) -> u64 {
        self.invalid_vbe_bpp
    }

    /// Read-only register evidence for a stopped guest or a missing display.
    /// Keep VRAM out of this report and allocate only on an explicit request.
    pub fn register_report(&self) -> alloc::string::String {
        alloc::format!(
            "misc={:02x} feature={:02x} seq_index={:02x} seq={:02x?}\ncrtc_index={:02x} crtc={:02x?}\ngc_index={:02x} gc={:02x?}\nattr_index={:02x} attr={:02x?} address_phase={} pel_mask={:02x}\nvbe_index={:04x} vbe={:04x?} stride={} start={:#x} invalid_bpp={}",
            self.misc_output,
            self.feature_control,
            self.seq_index,
            self.seq,
            self.crtc_index,
            &self.crtc[..0x19],
            self.gc_index,
            self.gc,
            self.attr_index,
            self.attr,
            self.attr_address_phase,
            self.pel_mask,
            self.vbe_index,
            self.vbe_regs,
            self.vbe_stride,
            self.vbe_start_addr,
            self.invalid_vbe_bpp,
        )
    }

    pub fn dirty_page(&self, page: usize) -> bool {
        if page >= VRAM_PAGES {
            return false;
        }
        (self.dirty_pages[page / 8] & (1 << (page & 7))) != 0
    }

    /// Clears the local display's change bits (see `Viewer`).
    pub fn clear_dirty(&mut self) {
        self.dirty_pages.fill(0);
        self.frame_dirty &= !(1 << Viewer::Local as u8);
    }

    /// The frame changed since the local display last cleared it.
    pub fn whole_frame_dirty(&self) -> bool {
        self.frame_dirty_for(Viewer::Local)
    }

    pub fn frame_dirty_for(&self, viewer: Viewer) -> bool {
        self.frame_dirty & (1 << viewer as u8) != 0
    }

    pub fn clear_frame_dirty_for(&mut self, viewer: Viewer) {
        self.frame_dirty &= !(1 << viewer as u8);
    }

    pub fn mode_info(&self) -> VgaModeInfo {
        if self.attr_pas_clear() {
            return VgaModeInfo {
                mode: RenderMode::Blank,
                width: 720,
                height: 400,
                bpp: 32,
                stride: 720 * 4,
            };
        }
        if self.vbe_enabled() {
            return VgaModeInfo {
                mode: RenderMode::Vbe,
                width: self.vbe_regs[VBE_INDEX_XRES],
                height: self.vbe_regs[VBE_INDEX_YRES],
                bpp: self.vbe_regs[VBE_INDEX_BPP],
                stride: self.vbe_stride,
            };
        }
        if self.chain4_enabled() {
            return VgaModeInfo {
                mode: RenderMode::Chain4,
                width: 320,
                height: 200,
                bpp: 8,
                stride: 320,
            };
        }
        if self.graphics_mode() {
            // CRTC display end is inclusive; overflow bits supply vertical
            // bits 8/9. In particular, BIOS mode 6Ah is 800x600, not 640x480.
            let height = u16::from(self.crtc[0x12])
                | (u16::from(self.crtc[7] & 0x02) << 7)
                | (u16::from(self.crtc[7] & 0x40) << 3);
            let mut stride = u32::from(self.crtc[0x13]) * 2;
            if self.crtc[0x14] & 0x40 != 0 {
                stride *= 4;
            } else if self.crtc[0x17] & 0x40 == 0 {
                stride *= 2;
            }
            return VgaModeInfo {
                mode: RenderMode::Planar,
                width: (u16::from(self.crtc[1]) + 1) * 8,
                height: height + 1,
                bpp: 4,
                stride,
            };
        }
        VgaModeInfo {
            mode: RenderMode::Text,
            width: 720,
            height: 400,
            bpp: 32,
            stride: 720 * 4,
        }
    }

    pub fn text_byte(&self, offset: usize) -> Option<u8> {
        let addr = 0xb8000u64.checked_add(offset as u64)?;
        let rel = self.legacy_offset(addr)?;
        let plane = rel & 1;
        let byte_addr = rel & !1;
        self.vram.get((byte_addr << 2) | plane).copied()
    }

    pub fn text_snapshot(&self) -> TextSnapshot {
        let cols = self.text_cols();
        let rows = self.text_rows();
        let start = self.crtc_start_addr();
        let cursor = self.crtc_cursor_addr();
        let mut cells = Vec::with_capacity(cols as usize * rows as usize);
        let mut raw_cells = Vec::with_capacity(cols as usize * rows as usize);
        for row in 0..rows {
            for col in 0..cols {
                let cell = start.wrapping_add(u16::from(row) * cols + u16::from(col));
                let offset = usize::from(cell) * 2;
                let ch = self.text_byte(offset).unwrap_or(0);
                raw_cells.push([ch, self.text_byte(offset + 1).unwrap_or(7)]);
                cells.push(if (0x20..=0x7e).contains(&ch) {
                    ch
                } else {
                    b' '
                });
            }
        }
        TextSnapshot {
            cols,
            rows,
            cursor_offset: cursor,
            cells,
            raw_cells,
            cursor: if self.crtc[0x0a] & 0x20 == 0 && cursor.wrapping_sub(start) < cols * rows {
                Some((
                    cursor.wrapping_sub(start) % cols,
                    cursor.wrapping_sub(start) / cols,
                ))
            } else {
                None
            },
            blink_enabled: self.attr[0x10] & 8 != 0,
        }
    }

    pub fn text_contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        if needle.is_empty() {
            return true;
        }

        let cols = self.text_cols();
        let rows = self.text_rows();
        let start = self.crtc_start_addr();
        let mut matched = 0usize;
        for row in 0..rows {
            if row != 0 {
                if text_match_next(needle, &mut matched, b'\n') {
                    return true;
                }
            }
            let mut line_len = cols;
            while line_len != 0 && self.text_char(start, cols, row, line_len - 1) == b' ' {
                line_len -= 1;
            }
            for col in 0..line_len {
                if text_match_next(needle, &mut matched, self.text_char(start, cols, row, col)) {
                    return true;
                }
            }
        }

        false
    }

    pub fn legacy_read_u8(&mut self, addr: u64) -> Option<u8> {
        let offset = self.legacy_offset(addr)?;
        if self.vbe_enabled() && self.vbe_regs[VBE_INDEX_BPP] != 4 {
            return self.vbe_bank_read(offset);
        }
        if self.chain4_enabled() {
            return self.vram.get(offset % self.vram.len()).copied();
        }
        if self.odd_even_enabled() {
            let plane = (offset & 1) as usize;
            let byte_addr = offset & !1;
            let index = byte_addr << 2;
            self.load_latch(index);
            return Some(self.latch[plane]);
        }
        let index = self.standard_plane_base(offset)?;
        self.load_latch(index);
        if (self.gc[5] & 0x08) == 0 {
            let plane = (self.gc[4] & 3) as usize;
            Some(self.latch[plane])
        } else {
            let compare = self.gc[2] & 0x0f;
            let dont_care = self.gc[7] & 0x0f;
            let mut result = 0xff;
            for plane in 0..4 {
                if (dont_care & (1 << plane)) != 0 {
                    let expected = if (compare & (1 << plane)) != 0 {
                        0xff
                    } else {
                        0
                    };
                    result &= !(self.latch[plane] ^ expected);
                }
            }
            Some(result)
        }
    }

    pub fn legacy_write_u8(&mut self, addr: u64, value: u8) -> bool {
        let Some(offset) = self.legacy_offset(addr) else {
            return false;
        };
        if self.vbe_enabled() && self.vbe_regs[VBE_INDEX_BPP] != 4 {
            return self.vbe_bank_write(offset, value);
        }
        if self.chain4_enabled() {
            let index = offset % self.vram.len();
            self.vram[index] = value;
            self.mark_dirty(index);
            return true;
        }
        if self.odd_even_enabled() {
            let plane = offset & 1;
            let byte_addr = offset & !1;
            let index = (byte_addr << 2) | plane;
            if index < self.vram.len() {
                self.vram[index] = value;
                self.mark_dirty(index);
                return true;
            }
            return false;
        }
        let Some(index) = self.standard_plane_base(offset) else {
            return false;
        };
        // A CPU read loads the four VGA latches; writes do not reload them
        // from the destination. Drivers use this to copy eight planar pixels
        // from one address to another, and to merge masked writes with a
        // previously read value (v86 vga.js agrees).
        let rotated = rotate_right_u8(value, self.gc[3] & 7);
        let write_mode = self.gc[5] & 3;
        let map_mask = self.seq[2] & 0x0f;
        let bit_mask = self.gc[8];
        for plane in 0..4 {
            if (map_mask & (1 << plane)) == 0 {
                continue;
            }
            let src = match write_mode {
                0 => {
                    if (self.gc[1] & (1 << plane)) != 0 {
                        if (self.gc[0] & (1 << plane)) != 0 {
                            0xff
                        } else {
                            0
                        }
                    } else {
                        rotated
                    }
                }
                1 => self.latch[plane],
                2 => {
                    if (value & (1 << plane)) != 0 {
                        0xff
                    } else {
                        0
                    }
                }
                3 => {
                    if (self.gc[0] & (1 << plane)) != 0 {
                        0xff
                    } else {
                        0
                    }
                }
                _ => rotated,
            };
            let mask = if write_mode == 3 {
                rotated & bit_mask
            } else {
                bit_mask
            };
            let final_byte = if write_mode == 1 {
                // Latch copy bypasses rotate, set/reset, ALU and bit mask;
                // only the sequencer's plane write mask still applies.
                self.latch[plane]
            } else {
                let logical = apply_logical_op(src, self.latch[plane], self.gc[3]);
                (logical & mask) | (self.latch[plane] & !mask)
            };
            let dst = index | plane;
            if dst < self.vram.len() {
                self.vram[dst] = final_byte;
                self.mark_dirty(dst);
            }
        }
        true
    }

    pub fn lfb_read_u8(&self, addr: u64, lfb_base: u64) -> Option<u8> {
        let offset = usize::try_from(addr.checked_sub(lfb_base)?).ok()?;
        self.vram.get(offset).copied()
    }

    pub fn lfb_write_u8(&mut self, addr: u64, lfb_base: u64, value: u8) -> bool {
        let Some(offset) = addr
            .checked_sub(lfb_base)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return false;
        };
        let Some(byte) = self.vram.get_mut(offset) else {
            return false;
        };
        *byte = value;
        self.mark_dirty(offset);
        true
    }

    /// The allocation is fixed until its owning Machine is destroyed. Software
    /// JIT proofs expire before interpretation or device/event handling; a
    /// native-only dispatch can retain one within that exclusive batch.
    /// Hardware read mappings must be invalidated on reset or PCI aperture
    /// changes before guest access resumes.
    /// Unlike legacy planar VGA, this aperture has no latch/read side effects
    /// or write masks (v86 memory.rs agrees: raw VRAM with dirty writes).
    #[cfg(any(feature = "jit", feature = "monitor"))]
    pub(crate) fn direct_lfb_page(&mut self, addr: u64, base: u64, write: bool) -> Option<*mut u8> {
        let offset = usize::try_from(addr.checked_sub(base)?).ok()?;
        let page = offset & !4095;
        if page.checked_add(4096)? > self.vram.len() {
            return None;
        }
        if write {
            self.mark_dirty_range(page, 4096);
        }
        // SAFETY: the complete page is in the fixed VRAM allocation; its owner
        // excludes reset/reallocation and device changes throughout native use.
        Some(unsafe { self.vram.as_mut_ptr().add(offset) })
    }

    pub fn lfb_write_repeat(
        &mut self,
        addr: u64,
        lfb_base: u64,
        width: u8,
        value: u32,
        units: u32,
    ) -> bool {
        let Some(len) = u32::from(width).checked_mul(units) else {
            return false;
        };
        let Some(offset) = addr
            .checked_sub(lfb_base)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return false;
        };
        let Some(end) = offset.checked_add(len as usize) else {
            return false;
        };
        if end > self.vram.len() {
            return false;
        }
        match width {
            1 => self.vram[offset..end].fill(value as u8),
            2 => {
                let bytes = (value as u16).to_le_bytes();
                for chunk in self.vram[offset..end].chunks_exact_mut(2) {
                    chunk.copy_from_slice(&bytes);
                }
            }
            4 => {
                let bytes = value.to_le_bytes();
                for chunk in self.vram[offset..end].chunks_exact_mut(4) {
                    chunk.copy_from_slice(&bytes);
                }
            }
            _ => return false,
        }
        self.mark_dirty_range(offset, len as usize);
        true
    }

    pub fn io_read_u8(&mut self, port: u16, now_step: u64) -> Option<u8> {
        match port {
            0x3ba | 0x3da => {
                self.attr_address_phase = true;
                Some(self.status_1(now_step))
            }
            0x3c0 => Some(self.attr_index),
            0x3c1 => Some(
                self.attr
                    .get((self.attr_index & 0x1f) as usize)
                    .copied()
                    .unwrap_or(0),
            ),
            0x3c2 | 0x3cc => Some(self.misc_output),
            0x3c4 => Some(self.seq_index),
            0x3c5 => Some(self.seq.get(self.seq_index as usize).copied().unwrap_or(0)),
            0x3c6 => Some(self.pel_mask),
            0x3c7 => Some(self.dac_state),
            0x3c8 => Some(self.dac_write_index),
            0x3c9 => {
                let index = ((self.dac_read_index as usize) * 3 + self.dac_sub_index as usize)
                    % self.dac.len();
                let value = self.dac[index];
                self.dac_sub_index = self.dac_sub_index.wrapping_add(1);
                if self.dac_sub_index == 3 {
                    self.dac_sub_index = 0;
                    self.dac_read_index = self.dac_read_index.wrapping_add(1);
                }
                Some(value)
            }
            0x3ce => Some(self.gc_index),
            0x3cf => Some(self.gc.get(self.gc_index as usize).copied().unwrap_or(0)),
            0x3d4 | 0x3b4 => Some(self.crtc_index),
            0x3d5 | 0x3b5 => Some(if self.crtc_index == 0x22 {
                self.latch[usize::from(self.gc[4] & 3)]
            } else {
                self.crtc
                    .get(self.crtc_index as usize)
                    .copied()
                    .unwrap_or(0)
            }),
            _ if (0x3c0..=0x3df).contains(&port) || (0x3b0..=0x3bf).contains(&port) => Some(0),
            _ => None,
        }
    }

    pub fn io_read_u16(&mut self, port: u16, _now_step: u64) -> Option<u16> {
        // tiny386 and v86 expose the VBE index/data pair only as word ports.
        // There is no high-byte alias at 1D0, nor an implicit wide VGA read.
        match port {
            0x1ce => Some(self.vbe_index),
            0x1cf => Some(self.vbe_read_selected()),
            _ => None,
        }
    }

    pub fn io_write_u8(&mut self, port: u16, value: u8, now_step: u64) -> bool {
        match port {
            0x3c0 => {
                if self.attr_address_phase {
                    if (self.attr_index ^ value) & 0x20 != 0 {
                        self.frame_dirty = u8::MAX;
                    }
                    self.attr_index = value & 0x3f;
                } else if let Some(register) = self.attr.get_mut((self.attr_index & 0x1f) as usize)
                {
                    // Only implemented VGA registers retain writes. Echoing
                    // AR16 and CR33 falsely passes the ET4000/W32 hardware
                    // probe and makes Windows install an accelerator driver.
                    // No vendor extension is advertised here (tiny386 vga.c and
                    // v86 vga.js retain neither register).
                    let mask = match self.attr_index & 0x1f {
                        0x00..=0x0f | 0x12 => 0x3f,
                        0x10 => 0xef,
                        0x13 | 0x14 => 0x0f,
                        _ => 0xff,
                    };
                    *register = value & mask;
                    self.frame_dirty = u8::MAX;
                }
                self.attr_address_phase = !self.attr_address_phase;
                true
            }
            0x3c2 => {
                self.misc_output = value;
                self.frame_dirty = u8::MAX;
                true
            }
            0x3ba | 0x3da => {
                self.feature_control = value;
                let _ = now_step;
                true
            }
            0x3c4 => {
                self.seq_index = value;
                true
            }
            0x3c5 => {
                if let Some(register) = self.seq.get_mut(self.seq_index as usize) {
                    let mask = match self.seq_index {
                        0 => 0x03,
                        1 => 0x3d,
                        2 => 0x0f,
                        3 => 0x3f,
                        _ => 0x0e,
                    };
                    *register = value & mask;
                    self.frame_dirty = u8::MAX;
                }
                true
            }
            0x3c6 => {
                self.pel_mask = value;
                self.frame_dirty = u8::MAX;
                true
            }
            0x3c7 => {
                self.dac_read_index = value;
                self.dac_sub_index = 0;
                self.dac_state = 3;
                true
            }
            0x3c8 => {
                self.dac_write_index = value;
                self.dac_sub_index = 0;
                self.dac_state = 0;
                true
            }
            0x3c9 => {
                let index = ((self.dac_write_index as usize) * 3 + self.dac_sub_index as usize)
                    % self.dac.len();
                self.dac[index] = if self.vbe_regs[VBE_INDEX_ENABLE] & VBE_8BIT_DAC != 0 {
                    value
                } else {
                    value & 0x3f
                };
                // The renderer reads these components directly, including a
                // partially written palette entry.
                self.frame_dirty = u8::MAX;
                self.dac_sub_index = self.dac_sub_index.wrapping_add(1);
                if self.dac_sub_index == 3 {
                    self.dac_sub_index = 0;
                    self.dac_write_index = self.dac_write_index.wrapping_add(1);
                }
                true
            }
            0x3ce => {
                self.gc_index = value;
                true
            }
            0x3cf => {
                if let Some(register) = self.gc.get_mut(self.gc_index as usize) {
                    let mask = match self.gc_index {
                        0..=2 | 6 | 7 => 0x0f,
                        3 => 0x1f,
                        4 => 0x03,
                        5 => 0x7b,
                        _ => 0xff,
                    };
                    *register = value & mask;
                    self.frame_dirty = u8::MAX;
                }
                true
            }
            0x3d4 | 0x3b4 => {
                self.crtc_index = value & 0x3f;
                true
            }
            0x3d5 | 0x3b5 => {
                let protected = self.crtc[0x11] & 0x80 != 0;
                if let Some(register) = self.crtc.get_mut(self.crtc_index as usize) {
                    if !protected || self.crtc_index >= 8 {
                        *register = value;
                    } else if self.crtc_index == 7 {
                        *register = (*register & !0x10) | (value & 0x10);
                    }
                    self.frame_dirty = u8::MAX;
                }
                true
            }
            _ if (0x3c0..=0x3df).contains(&port) || (0x3b0..=0x3bf).contains(&port) => true,
            _ => false,
        }
    }

    pub fn io_write_u16(&mut self, port: u16, value: u16, now_step: u64) -> bool {
        match port {
            0x1ce => {
                self.vbe_index = value;
                true
            }
            0x1cf => {
                self.vbe_write_selected(value);
                true
            }
            // VGA drivers commonly program index/data pairs with OUTW. Claim
            // both bytes before executing either; do not partially handle a
            // cycle and then ask the bus to retry it.
            0x3b0..=0x3be | 0x3c0..=0x3de => {
                self.io_write_u8(port, value as u8, now_step);
                self.io_write_u8(port + 1, (value >> 8) as u8, now_step);
                true
            }
            _ => false,
        }
    }

    pub fn render_rgba(&self, out: &mut [u8]) -> Option<VgaModeInfo> {
        let info = self.mode_info();
        let needed = info.width as usize * info.height as usize * 4;
        if out.len() < needed {
            return None;
        }
        match info.mode {
            RenderMode::Blank => out[..needed].fill(0),
            RenderMode::Text => self.render_text(out, info.width as usize, info.height as usize),
            RenderMode::Chain4 => self.render_chain4(out),
            RenderMode::Planar => self.render_planar(out, &info, self.crtc_start_addr() as usize),
            RenderMode::Vbe => self.render_vbe(out, &info),
        }
        Some(info)
    }

    fn attr_pas_clear(&self) -> bool {
        (self.attr_index & 0x20) == 0
    }

    fn vbe_enabled(&self) -> bool {
        (self.vbe_regs[VBE_INDEX_ENABLE] & VBE_ENABLED) != 0
    }

    fn chain4_enabled(&self) -> bool {
        (self.seq[4] & 0x08) != 0
    }

    fn odd_even_enabled(&self) -> bool {
        (self.seq[4] & 0x04) == 0 || (self.gc[5] & 0x10) != 0
    }

    fn graphics_mode(&self) -> bool {
        (self.attr[0x10] & 0x01) != 0 || (self.gc[6] & 0x01) != 0
    }

    fn legacy_offset(&self, addr: u64) -> Option<usize> {
        if !Self::contains_legacy(addr) {
            return None;
        }
        let raw = usize::try_from(addr - LEGACY_VGA_BASE).ok()?;
        let map = (self.gc[6] >> 2) & 3;
        match map {
            0 => Some(raw & 0x1ffff),
            1 => (raw < 0x10000).then_some(raw),
            2 => {
                if (0x10000..0x18000).contains(&raw) {
                    Some(raw - 0x10000)
                } else {
                    None
                }
            }
            _ => {
                if (0x18000..0x20000).contains(&raw) {
                    Some(raw - 0x18000)
                } else {
                    None
                }
            }
        }
    }

    fn standard_plane_base(&self, offset: usize) -> Option<usize> {
        // DISPI 4-bpp modes still use VGA planes, read latches, write masks
        // and raster operations. A bank selects 64 KiB in EACH plane, not
        // 64 KiB of packed pixels, as in Bochs' DISPI device that defines the
        // interface; SeaBIOS exposes this path as VBE mode 102h. v86 vga.js
        // does not model DISPI 4-bpp banking.
        let bank = if self.vbe_enabled() {
            usize::from(self.vbe_regs[VBE_INDEX_BANK]) * 0x10000
        } else {
            0
        };
        let index = (bank + offset) << 2;
        (index + 3 < self.vram.len()).then_some(index)
    }

    fn load_latch(&mut self, index: usize) {
        for plane in 0..4 {
            self.latch[plane] = self.vram.get(index | plane).copied().unwrap_or(0xff);
        }
    }

    fn mark_dirty(&mut self, index: usize) {
        let page = index / 4096;
        if page < VRAM_PAGES {
            self.dirty_pages[page / 8] |= 1 << (page & 7);
        }
        self.frame_dirty = u8::MAX;
    }

    fn mark_dirty_range(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len).min(self.vram.len());
        let mut page = start / 4096;
        while page <= end.saturating_sub(1) / 4096 && page < VRAM_PAGES {
            self.dirty_pages[page / 8] |= 1 << (page & 7);
            page += 1;
        }
        self.frame_dirty = u8::MAX;
    }

    fn vbe_read_selected(&self) -> u16 {
        let index = self.vbe_index as usize;
        if index >= VBE_REGS {
            return 0;
        }
        let get_caps = (self.vbe_regs[VBE_INDEX_ENABLE] & VBE_GETCAPS) != 0;
        match index {
            VBE_INDEX_XRES if get_caps => VBE_MAX_XRES,
            VBE_INDEX_YRES if get_caps => VBE_MAX_YRES,
            VBE_INDEX_BPP if get_caps => VBE_MAX_BPP,
            VBE_INDEX_BANK if get_caps => 0x1000,
            _ => self.vbe_regs[index],
        }
    }

    fn vbe_write_selected(&mut self, value: u16) {
        let index = self.vbe_index as usize;
        if index >= VBE_REGS || index == VBE_INDEX_VIDEO_MEMORY_64K {
            return;
        }
        match index {
            VBE_INDEX_ID => {
                if (VBE_ID0..=VBE_ID5).contains(&value) {
                    self.vbe_regs[index] = value;
                }
            }
            VBE_INDEX_ENABLE => self.vbe_write_enable(value),
            VBE_INDEX_BANK => self.vbe_regs[index] = value & 0x3fff,
            VBE_INDEX_XRES | VBE_INDEX_YRES | VBE_INDEX_BPP => {
                self.vbe_regs[index] = value;
                if self.vbe_enabled() {
                    // tiny386 updates derived scanout state on live writes.
                    // Reuse enable validation without a rising edge: preserve
                    // pixels/offsets while keeping size, depth and pitch valid.
                    self.vbe_write_enable(self.vbe_regs[VBE_INDEX_ENABLE]);
                }
            }
            VBE_INDEX_VIRT_WIDTH | VBE_INDEX_X_OFFSET | VBE_INDEX_Y_OFFSET => {
                self.vbe_regs[index] = value;
                if self.vbe_enabled() {
                    self.vbe_fixup();
                    self.frame_dirty = u8::MAX;
                }
            }
            _ => self.vbe_regs[index] = value,
        }
    }

    fn vbe_write_enable(&mut self, value: u16) {
        let old = self.vbe_regs[VBE_INDEX_ENABLE];
        let rising = (value & VBE_ENABLED) != 0 && (old & VBE_ENABLED) == 0;
        if (value & VBE_ENABLED) != 0 && !valid_vbe_bpp(self.vbe_regs[VBE_INDEX_BPP]) {
            self.vbe_regs[VBE_INDEX_ENABLE] = value & !VBE_ENABLED;
            self.invalid_vbe_bpp = self.invalid_vbe_bpp.wrapping_add(1);
            self.frame_dirty = u8::MAX;
            return;
        }
        self.vbe_regs[VBE_INDEX_ENABLE] = value & !VBE_NOCLEARMEM;
        if rising {
            self.vbe_regs[VBE_INDEX_X_OFFSET] = 0;
            self.vbe_regs[VBE_INDEX_Y_OFFSET] = 0;
            // A fresh mode starts with its visible pitch. Retaining a prior
            // wider mode's virtual width corrupts every row after switching.
            self.vbe_regs[VBE_INDEX_VIRT_WIDTH] = self.vbe_regs[VBE_INDEX_XRES];
            self.vbe_regs[VBE_INDEX_VIRT_HEIGHT] = self.vbe_regs[VBE_INDEX_YRES];
        }
        self.vbe_fixup();
        if rising && (value & VBE_NOCLEARMEM) == 0 {
            let len = (self.vbe_stride as usize)
                .saturating_mul(self.vbe_regs[VBE_INDEX_YRES] as usize)
                .saturating_mul(if self.vbe_regs[VBE_INDEX_BPP] == 4 {
                    4
                } else {
                    1
                })
                .min(self.vram.len());
            self.vram[..len].fill(0);
            self.mark_dirty_range(0, len);
        }
        self.frame_dirty = u8::MAX;
    }

    fn vbe_fixup(&mut self) {
        let bpp = self.vbe_regs[VBE_INDEX_BPP];
        if !valid_vbe_bpp(bpp) {
            self.vbe_stride = 0;
            self.vbe_start_addr = 0;
            return;
        }
        self.vbe_regs[VBE_INDEX_XRES] = self.vbe_regs[VBE_INDEX_XRES].clamp(8, VBE_MAX_XRES) & !7;
        self.vbe_regs[VBE_INDEX_YRES] = self.vbe_regs[VBE_INDEX_YRES].clamp(1, VBE_MAX_YRES);
        self.vbe_regs[VBE_INDEX_VIRT_WIDTH] = (self.vbe_regs[VBE_INDEX_VIRT_WIDTH] & !7)
            .clamp(self.vbe_regs[VBE_INDEX_XRES], VBE_MAX_XRES);
        if self.vbe_regs[VBE_INDEX_VIRT_HEIGHT] < self.vbe_regs[VBE_INDEX_YRES] {
            self.vbe_regs[VBE_INDEX_VIRT_HEIGHT] = self.vbe_regs[VBE_INDEX_YRES];
        }
        let (stride, x_offset) = if bpp == 4 {
            // Plane byte addresses: eight pixels in each group of four bytes.
            (
                u32::from(self.vbe_regs[VBE_INDEX_VIRT_WIDTH]) / 8,
                u32::from(self.vbe_regs[VBE_INDEX_X_OFFSET]) / 8,
            )
        } else {
            (
                u32::from(self.vbe_regs[VBE_INDEX_VIRT_WIDTH]) * bytes_per_pixel(bpp),
                u32::from(self.vbe_regs[VBE_INDEX_X_OFFSET]) * bytes_per_pixel(bpp),
            )
        };
        self.vbe_stride = stride;
        self.vbe_start_addr =
            u32::from(self.vbe_regs[VBE_INDEX_Y_OFFSET]) * self.vbe_stride + x_offset;
    }

    fn vbe_bank_read(&self, offset: usize) -> Option<u8> {
        let base = usize::from(self.vbe_regs[VBE_INDEX_BANK]) * 0x10000;
        self.vram.get(base.checked_add(offset & 0xffff)?).copied()
    }

    fn vbe_bank_write(&mut self, offset: usize, value: u8) -> bool {
        let base = usize::from(self.vbe_regs[VBE_INDEX_BANK]) * 0x10000;
        let Some(index) = base.checked_add(offset & 0xffff) else {
            return false;
        };
        let Some(byte) = self.vram.get_mut(index) else {
            return false;
        };
        *byte = value;
        self.mark_dirty(index);
        true
    }

    fn status_1(&self, now_step: u64) -> u8 {
        // Nominal 60 Hz scanout, including headless and unchanged frames. v86
        // ties retrace to rendering/polling; our browsers poll independently,
        // so use the common device clock instead. This intentionally does not
        // simulate the programmable pixel clock or every CRTC timing detail.
        const FRAME: u64 = super::TICKS_PER_SECOND / 60;
        const LINE: u64 = FRAME / 525;
        let frame_pos = now_step % FRAME;
        let retrace = frame_pos >= FRAME - super::TICKS_PER_SECOND / 1000;
        let display_disabled = retrace || frame_pos % LINE >= LINE * 4 / 5;
        (if retrace { 0x08 } else { 0 }) | (if display_disabled { 0x01 } else { 0 })
    }

    fn load_default_dac(&mut self) {
        let colors: [[u8; 3]; 16] = [
            [0, 0, 0],
            [0, 0, 42],
            [0, 42, 0],
            [0, 42, 42],
            [42, 0, 0],
            [42, 0, 42],
            [42, 21, 0],
            [42, 42, 42],
            [21, 21, 21],
            [21, 21, 63],
            [21, 63, 21],
            [21, 63, 63],
            [63, 21, 21],
            [63, 21, 63],
            [63, 63, 21],
            [63, 63, 63],
        ];
        for (index, rgb) in colors.iter().enumerate() {
            self.dac[index * 3..index * 3 + 3].copy_from_slice(rgb);
        }
        for index in 16..256 {
            let value = (index & 0x3f) as u8;
            self.dac[index * 3] = value;
            self.dac[index * 3 + 1] = value;
            self.dac[index * 3 + 2] = value;
        }
    }

    fn text_cols(&self) -> u16 {
        80
    }

    fn text_rows(&self) -> u16 {
        let char_height = self.char_height().max(1);
        (400 / char_height).clamp(1, 60) as u16
    }

    fn char_height(&self) -> usize {
        usize::from((self.crtc[0x09] & 0x1f) + 1).clamp(1, 32)
    }

    fn crtc_start_addr(&self) -> u16 {
        u16::from(self.crtc[0x0c]) << 8 | u16::from(self.crtc[0x0d])
    }

    fn crtc_cursor_addr(&self) -> u16 {
        u16::from(self.crtc[0x0e]) << 8 | u16::from(self.crtc[0x0f])
    }

    fn text_char(&self, start: u16, cols: u16, row: u16, col: u16) -> u8 {
        let cell = start.wrapping_add(row * cols + col);
        let offset = usize::from(cell) * 2;
        let ch = self.text_byte(offset).unwrap_or(0);
        if (0x20..=0x7e).contains(&ch) {
            ch
        } else {
            b' '
        }
    }

    fn render_text(&self, out: &mut [u8], width: usize, height: usize) {
        let cols = self.text_cols() as usize;
        let rows = self.text_rows() as usize;
        let char_height = self.char_height();
        let start = self.crtc_start_addr();
        let cursor = self.crtc_cursor_addr();
        out[..width * height * 4].fill(0);
        for row in 0..rows {
            for col in 0..cols {
                let cell = start.wrapping_add((row * cols + col) as u16);
                let ch = self.text_byte(usize::from(cell) * 2).unwrap_or(0);
                let attr = self.text_byte(usize::from(cell) * 2 + 1).unwrap_or(0x07);
                let fg = self.attribute_rgba(attr & 0x0f);
                let bg = self.attribute_rgba((attr >> 4) & 0x07);
                let cursor_on = cell == cursor;
                for scan in 0..char_height.min(32) {
                    let glyph = self.font_byte(ch, scan);
                    for x in 0..9 {
                        let dup9 = (0xc0..=0xdf).contains(&ch) && x == 8;
                        let bit = if x < 8 {
                            (glyph >> (7 - x)) & 1
                        } else if dup9 {
                            glyph & 1
                        } else {
                            0
                        };
                        let y = row * char_height + scan;
                        let px = col * 9 + x;
                        if y >= height || px >= width {
                            continue;
                        }
                        let color = if cursor_on && scan >= usize::from(self.crtc[0x0a] & 0x1f) {
                            fg
                        } else if bit != 0 {
                            fg
                        } else {
                            bg
                        };
                        put_rgba(out, width, px, y, color);
                    }
                }
            }
        }
    }

    fn font_byte(&self, ch: u8, scan: usize) -> u8 {
        let offset = usize::from(ch) * 32 + scan.min(31);
        self.vram.get((offset << 2) | 2).copied().unwrap_or(0)
    }

    fn load_default_text_font(&mut self) {
        for (ch, glyph) in BASIC_LEGACY.iter().enumerate() {
            for scan in 0..16usize {
                let src = glyph[scan / 2].reverse_bits();
                let offset = ch * 32 + scan;
                let index = (offset << 2) | 2;
                if let Some(byte) = self.vram.get_mut(index) {
                    *byte = src;
                }
            }
        }
    }

    fn render_chain4(&self, out: &mut [u8]) {
        let width = 320usize;
        for y in 0..200usize {
            for x in 0..width {
                let index = y * width + x;
                let color =
                    self.palette_rgba(self.vram.get(index).copied().unwrap_or(0) & self.pel_mask);
                put_rgba(out, width, x, y, color);
            }
        }
    }

    fn render_planar(&self, out: &mut [u8], info: &VgaModeInfo, start: usize) {
        let width = info.width as usize;
        for y in 0..info.height as usize {
            for byte_x in 0..width.div_ceil(8) {
                let addr = (start + y * info.stride as usize + byte_x) << 2;
                let p0 = self.vram.get(addr).copied().unwrap_or(0);
                let p1 = self.vram.get(addr | 1).copied().unwrap_or(0);
                let p2 = self.vram.get(addr | 2).copied().unwrap_or(0);
                let p3 = self.vram.get(addr | 3).copied().unwrap_or(0);
                for bit in 0..8.min(width - byte_x * 8) {
                    let mask = 0x80 >> bit;
                    let mut idx = 0u8;
                    idx |= u8::from((p0 & mask) != 0);
                    idx |= u8::from((p1 & mask) != 0) << 1;
                    idx |= u8::from((p2 & mask) != 0) << 2;
                    idx |= u8::from((p3 & mask) != 0) << 3;
                    put_rgba(
                        out,
                        width,
                        byte_x * 8 + bit,
                        y,
                        self.attribute_rgba(idx & self.attr[0x12]),
                    );
                }
            }
        }
    }

    fn render_vbe(&self, out: &mut [u8], info: &VgaModeInfo) {
        match info.bpp {
            4 => self.render_planar(out, info, self.vbe_start_addr as usize),
            8 => self.render_vbe_pixels::<8>(out, info),
            15 => self.render_vbe_pixels::<15>(out, info),
            16 => self.render_vbe_pixels::<16>(out, info),
            24 => self.render_vbe_pixels::<24>(out, info),
            32 => self.render_vbe_pixels::<32>(out, info),
            _ => {
                for pixel in
                    out[..info.width as usize * info.height as usize * 4].chunks_exact_mut(4)
                {
                    pixel.copy_from_slice(&[0, 0, 0, 0xff]);
                }
            }
        }
    }

    // Select the format once and bound each row before its pixel loop. Guest
    // offsets may put part or all of a row beyond VRAM; absent components still
    // read as zero, including a partial final pixel. Output length was checked
    // by render_rgba. No alignment or native-endian pixel layout is assumed.
    // Consulted tiny386 vga.c for packed BGR formats and pitch; retain our
    // existing 5/6-bit color expansion. v86 vga.rs agrees on the formats but
    // expands with *255/31 and *255/63, and ignores virtual-width writes.
    fn render_vbe_pixels<const BPP: u16>(&self, out: &mut [u8], info: &VgaModeInfo) {
        let width = info.width as usize;
        let height = info.height as usize;
        if width == 0 {
            return; // Live register writes can temporarily select an empty mode.
        }
        let bytes = bytes_per_pixel(BPP) as usize;
        let color = |src: &[u8]| match BPP {
            8 => self.palette_rgba(src[0] & self.pel_mask),
            15 => rgb555_to_rgba(u16::from_le_bytes([src[0], src[1]])),
            16 => rgb565_to_rgba(u16::from_le_bytes([src[0], src[1]])),
            24 | 32 => [src[2], src[1], src[0], 0xff],
            _ => unreachable!(),
        };
        for (y, row) in out[..width * height * 4]
            .chunks_exact_mut(width * 4)
            .enumerate()
        {
            let start = self.vbe_start_addr as usize + y * info.stride as usize;
            let end = (start + width * bytes).min(self.vram.len());
            let source = &self.vram[start.min(self.vram.len())..end];
            let complete = source.len() / bytes;
            let (pixels, tail) = row.split_at_mut(complete * 4);
            for (src, dst) in source.chunks_exact(bytes).zip(pixels.chunks_exact_mut(4)) {
                dst.copy_from_slice(&color(src));
            }
            let mut tail = tail.chunks_exact_mut(4);
            if let Some(pixel) = tail.next() {
                let mut partial = [0; 4];
                let remaining = &source[complete * bytes..];
                partial[..remaining.len()].copy_from_slice(remaining);
                pixel.copy_from_slice(&color(&partial));
                let missing = color(&[0; 4]);
                for pixel in tail {
                    pixel.copy_from_slice(&missing);
                }
            }
        }
    }

    /// RGBA for one of the 16 text attribute colors, through the palette and DAC.
    pub fn attribute_rgba(&self, index: u8) -> [u8; 4] {
        // VGA's 16 attribute colors select DAC entries; they are not direct
        // DAC indices. SeaBIOS maps bright white (15) to DAC entry 63. P54S
        // selects whether bits 4/5 come from this palette or Color Select.
        // v86 vga.js vga_redraw agrees.
        let palette = self.attr[usize::from(index & 15)];
        let dac = if self.attr[0x10] & 0x80 != 0 {
            (palette & 0x0f) | (self.attr[0x14] << 4)
        } else {
            (palette & 0x3f) | ((self.attr[0x14] & 0x0c) << 4)
        };
        self.palette_rgba(dac & self.pel_mask)
    }

    fn palette_rgba(&self, index: u8) -> [u8; 4] {
        let idx = usize::from(index);
        // SeaBIOS 4F08 selects this DISPI bit. Retain raw DAC components as
        // tiny386 does; the active width determines their displayed intensity.
        if self.vbe_regs[VBE_INDEX_ENABLE] & VBE_8BIT_DAC != 0 {
            return [
                self.dac[idx * 3],
                self.dac[idx * 3 + 1],
                self.dac[idx * 3 + 2],
                0xff,
            ];
        }
        [
            dac6_to_8(self.dac[idx * 3]),
            dac6_to_8(self.dac[idx * 3 + 1]),
            dac6_to_8(self.dac[idx * 3 + 2]),
            0xff,
        ]
    }
}

impl Default for PcVga {
    fn default() -> Self {
        Self::new()
    }
}

fn valid_vbe_bpp(bpp: u16) -> bool {
    matches!(bpp, 4 | 8 | 15 | 16 | 24 | 32)
}

fn bytes_per_pixel(bpp: u16) -> u32 {
    match bpp {
        15 | 16 => 2,
        24 => 3,
        32 => 4,
        _ => 1,
    }
}

fn rotate_right_u8(value: u8, count: u8) -> u8 {
    value.rotate_right(u32::from(count & 7))
}

fn apply_logical_op(src: u8, latch: u8, gc_data_rotate: u8) -> u8 {
    match (gc_data_rotate >> 3) & 3 {
        0 => src,
        1 => src & latch,
        2 => src | latch,
        _ => src ^ latch,
    }
}

fn dac6_to_8(value: u8) -> u8 {
    let value = value & 0x3f;
    (value << 2) | (value >> 4)
}

fn put_rgba(out: &mut [u8], width: usize, x: usize, y: usize, rgba: [u8; 4]) {
    let offset = (y * width + x) * 4;
    if offset + 4 <= out.len() {
        out[offset..offset + 4].copy_from_slice(&rgba);
    }
}

fn text_match_next(needle: &[u8], matched: &mut usize, byte: u8) -> bool {
    loop {
        if byte == needle[*matched] {
            *matched += 1;
            if *matched == needle.len() {
                return true;
            }
            return false;
        }
        if *matched == 0 {
            return false;
        }
        *matched = text_match_fallback(needle, *matched);
    }
}

fn text_match_fallback(needle: &[u8], matched: usize) -> usize {
    let mut len = matched - 1;
    while len != 0 {
        if needle[..len] == needle[matched - len..matched] {
            return len;
        }
        len -= 1;
    }
    0
}

fn scale5(value: u16) -> u8 {
    let value = value as u8 & 0x1f;
    (value << 3) | (value >> 2)
}

fn scale6(value: u16) -> u8 {
    let value = value as u8 & 0x3f;
    (value << 2) | (value >> 4)
}

fn rgb555_to_rgba(value: u16) -> [u8; 4] {
    [
        scale5((value >> 10) & 0x1f),
        scale5((value >> 5) & 0x1f),
        scale5(value & 0x1f),
        0xff,
    ]
}

fn rgb565_to_rgba(value: u16) -> [u8; 4] {
    [
        scale5((value >> 11) & 0x1f),
        scale6((value >> 5) & 0x3f),
        scale5(value & 0x1f),
        0xff,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_reuses_vram_and_restores_registers_fonts_and_dirty_state() {
        let mut vga = PcVga::new();
        let vram = vga.vram.as_ptr();
        let dirty = vga.dirty_pages.as_ptr();
        vga.vram.fill(0xa5);
        vga.dirty_pages.fill(0xff);
        vga.vbe_regs.fill(0xffff);
        vga.seq.fill(0xff);
        vga.dac.fill(0xff);
        vga.frame_dirty = 0;
        let mut reset = vga.take_for_reset();
        let expected = PcVga::new();
        for _ in 0..3 {
            assert_eq!(reset.vram.as_ptr(), vram);
            assert_eq!(reset.dirty_pages.as_ptr(), dirty);
            assert_eq!(reset.vram, expected.vram);
            assert_eq!(reset.dirty_pages, expected.dirty_pages);
            assert_eq!(reset.vbe_regs, expected.vbe_regs);
            assert_eq!(reset.seq, expected.seq);
            assert_eq!(reset.dac, expected.dac);
            assert_eq!(reset.frame_dirty, u8::MAX);
            reset = reset.take_for_reset();
        }
    }

    #[test]
    fn chain4_writes_are_linear_and_dirty() {
        let mut vga = PcVga::new();
        vga.io_write_u8(0x3c4, 4, 0);
        vga.io_write_u8(0x3c5, 0x0e, 0);
        vga.io_write_u8(0x3ce, 6, 0);
        vga.io_write_u8(0x3cf, 0x00, 0);
        assert!(vga.legacy_write_u8(0xa1234, 0x5a));
        assert_eq!(vga.legacy_read_u8(0xa1234), Some(0x5a));
        assert!(vga.dirty_page(0x1234 / 4096));
    }

    #[test]
    fn undefined_vendor_register_probes_do_not_echo_or_alias_vga_state() {
        let mut vga = PcVga::new();
        // A legacy SVGA probe toggles bit 4 of AR16, then complements CR33.
        // Neither register exists on this VGA. Reads must stay unchanged.
        vga.io_read_u8(0x3da, 0);
        vga.io_write_u8(0x3c0, 0x36, 0);
        let before = vga.io_read_u8(0x3c1, 0).unwrap();
        vga.io_write_u8(0x3c0, before ^ 0x10, 0);
        assert_eq!(vga.io_read_u8(0x3c1, 0), Some(before));
        assert_eq!(vga.io_read_u8(0x3c0, 0), Some(0x36));
        vga.io_write_u8(0x3d4, 0x33, 0);
        let before = vga.io_read_u8(0x3d5, 0).unwrap();
        vga.io_write_u8(0x3d5, !before, 0);
        assert_eq!(vga.io_read_u8(0x3d5, 0), Some(before));

        // Higher vendor indices must not alias a normal memory-map register.
        for (index_port, data_port, normal, vendor, value) in
            [(0x3c4, 0x3c5, 4, 0x0c, 0x06), (0x3ce, 0x3cf, 6, 0x16, 0x0e)]
        {
            vga.io_write_u8(index_port, normal, 0);
            vga.io_write_u8(data_port, value, 0);
            vga.io_write_u8(index_port, vendor, 0);
            vga.io_write_u8(data_port, 0xff, 0);
            assert_eq!(vga.io_read_u8(data_port, 0), Some(0));
            vga.io_write_u8(index_port, normal, 0);
            assert_eq!(vga.io_read_u8(data_port, 0), Some(value));
        }
        assert_ne!(vga.mode_info().mode, RenderMode::Blank);
    }

    #[test]
    fn crtc_timing_protection_keeps_only_overflow_bit_four_writable() {
        let mut vga = PcVga::new();
        vga.io_write_u16(0x3d4, 0x1200, 0);
        vga.io_write_u16(0x3d4, 0xa507, 0);
        vga.io_write_u16(0x3d4, 0x8011, 0);
        vga.io_write_u16(0x3d4, 0xff00, 0);
        assert_eq!(vga.io_read_u8(0x3d5, 0), Some(0x12));
        vga.io_write_u16(0x3d4, 0xff07, 0);
        assert_eq!(vga.io_read_u8(0x3d5, 0), Some(0xb5));
        vga.io_write_u16(0x3d4, 0x0011, 0);
        vga.io_write_u16(0x3d4, 0x5f00, 0);
        assert_eq!(vga.io_read_u8(0x3d5, 0), Some(0x5f));
    }

    #[test]
    fn odd_even_text_mapping_round_trips() {
        let mut vga = PcVga::new();
        assert!(vga.legacy_write_u8(0xb8000, b'R'));
        assert!(vga.legacy_write_u8(0xb8001, 0x1f));
        assert_eq!(vga.text_byte(0), Some(b'R'));
        assert_eq!(vga.text_byte(1), Some(0x1f));
        assert_eq!(vga.legacy_read_u8(0xb8000), Some(b'R'));
        assert_eq!(vga.legacy_read_u8(0xb8001), Some(0x1f));
    }

    #[test]
    fn text_contains_matches_trimmed_snapshot_text() {
        let mut vga = PcVga::new();
        assert!(vga.legacy_write_u8(0xb8000, b't'));
        assert!(vga.legacy_write_u8(0xb8002, b'c'));
        assert!(vga.legacy_write_u8(0xb8004, b'@'));
        assert!(vga.legacy_write_u8(0xb8006, b'b'));
        assert!(vga.legacy_write_u8(0xb8008, b'o'));
        assert!(vga.legacy_write_u8(0xb800a, b'x'));
        assert!(vga.legacy_write_u8(0xb800c, b':'));
        assert!(vga.legacy_write_u8(0xb800e, b'~'));
        assert!(vga.legacy_write_u8(0xb8010, b'$'));
        assert!(vga.text_contains("tc@box:~$"));
        assert!(vga.text_contains("tc@box:~$\n"));
        assert!(!vga.text_contains("tc@box:~$ "));
    }

    #[test]
    fn planar_write_mode_zero_honors_set_reset_and_bit_mask() {
        let mut vga = PcVga::new();
        vga.io_write_u8(0x3c4, 4, 0);
        vga.io_write_u8(0x3c5, 0x06, 0);
        vga.io_write_u8(0x3ce, 6, 0);
        vga.io_write_u8(0x3cf, 0x00, 0);
        vga.io_write_u8(0x3ce, 5, 0);
        vga.io_write_u8(0x3cf, 0x00, 0);
        vga.io_write_u8(0x3ce, 0, 0);
        vga.io_write_u8(0x3cf, 0x05, 0);
        vga.io_write_u8(0x3ce, 1, 0);
        vga.io_write_u8(0x3cf, 0x0f, 0);
        vga.io_write_u8(0x3ce, 8, 0);
        vga.io_write_u8(0x3cf, 0xf0, 0);
        assert!(vga.legacy_write_u8(0xa0000, 0xaa));
        assert_eq!(vga.vram[0], 0xf0);
        assert_eq!(vga.vram[1], 0x00);
        assert_eq!(vga.vram[2], 0xf0);
        assert_eq!(vga.vram[3], 0x00);
    }

    fn planar_probe() -> PcVga {
        let mut vga = PcVga::new();
        vga.io_write_u16(0x3c4, 0x0604, 0);
        vga.io_write_u16(0x3ce, 0x0506, 0);
        vga.io_write_u16(0x3ce, 0x0005, 0);
        vga
    }

    fn write_planes(vga: &mut PcVga, addr: u64, bytes: [u8; 4]) {
        for (plane, byte) in bytes.into_iter().enumerate() {
            vga.io_write_u16(0x3c4, (0x100 << plane) | 2, 0);
            assert!(vga.legacy_write_u8(addr, byte));
        }
        vga.io_write_u16(0x3c4, 0x0f02, 0);
    }

    fn read_planes(vga: &mut PcVga, addr: u64) -> [u8; 4] {
        core::array::from_fn(|plane| {
            vga.io_write_u16(0x3ce, ((plane as u16) << 8) | 4, 0);
            vga.legacy_read_u8(addr).unwrap()
        })
    }

    #[test]
    fn planar_latch_copy_ignores_alu_and_keeps_latches_across_writes() {
        let mut vga = planar_probe();
        let source = [0x12, 0x34, 0x56, 0x78];
        let destination = [0xa5, 0x5a, 0xc3, 0x3c];
        write_planes(&mut vga, 0xa0100, source);
        write_planes(&mut vga, 0xa0200, destination);
        write_planes(&mut vga, 0xa0300, destination);
        vga.legacy_read_u8(0xa0100); // One read loads all four planes.
        vga.io_write_u16(0x3ce, 0x0105, 0); // Write mode 1: latch copy.
        vga.io_write_u16(0x3ce, 0x1f03, 0); // XOR + rotate must be bypassed.
        vga.io_write_u16(0x3ce, 0x5a08, 0); // Bit mask must be bypassed too.
        vga.io_write_u16(0x3c4, 0x0502, 0); // Only planes 0 and 2 may change.
        assert!(vga.legacy_write_u8(0xa0200, 0xff));
        assert!(vga.legacy_write_u8(0xa0300, 0)); // No intervening read.
        for addr in [0xa0200, 0xa0300] {
            assert_eq!(read_planes(&mut vga, addr), [0x12, 0x5a, 0x56, 0x3c]);
        }
        assert_eq!(read_planes(&mut vga, 0xa0100), source);
    }

    #[test]
    fn planar_masked_writes_merge_with_last_read_not_destination() {
        for (mode, rotate, mask, value, expected) in [
            (0, 0, 0x0f, 0xa3, [0x33, 0xa3, 0x63, 0x93]),
            (2, 0, 0x0f, 0x05, [0x3f, 0xa0, 0x6f, 0x90]),
            (3, 1, 0x0f, 0xc3, [0x3c, 0xa5, 0x68, 0x97]),
            (0, 0x18, 0xf0, 0xf0, [0xcc, 0x55, 0x99, 0x66]),
        ] {
            let mut vga = planar_probe();
            write_planes(&mut vga, 0xa0100, [0x3c, 0xa5, 0x69, 0x96]);
            write_planes(&mut vga, 0xa0200, [0, 0, 0, 0]);
            vga.legacy_read_u8(0xa0100);
            vga.io_write_u16(0x3ce, 0x0a00, 0); // Mode 3 set/reset color.
            vga.io_write_u16(0x3ce, (rotate << 8) | 3, 0);
            vga.io_write_u16(0x3ce, (mode << 8) | 5, 0);
            vga.io_write_u16(0x3ce, (mask << 8) | 8, 0);
            assert!(vga.legacy_write_u8(0xa0200, value));
            assert_eq!(read_planes(&mut vga, 0xa0200), expected, "mode {mode}");
        }
    }

    #[test]
    fn read_mode_one_uses_compare_and_dont_care() {
        let mut vga = PcVga::new();
        vga.seq[4] = 0x06;
        vga.gc[6] = 0;
        vga.vram[0] = 0xff;
        vga.vram[1] = 0x00;
        vga.vram[2] = 0xff;
        vga.vram[3] = 0x00;
        vga.gc[2] = 0x05;
        vga.gc[7] = 0x0f;
        vga.gc[5] = 0x08;
        assert_eq!(vga.legacy_read_u8(0xa0000), Some(0xff));
        vga.gc[2] = 0x0f;
        assert_eq!(vga.legacy_read_u8(0xa0000), Some(0x00));
    }

    #[test]
    fn attr_flip_flop_resets_on_status_read() {
        let mut vga = PcVga::new();
        assert!(vga.io_write_u8(0x3c0, 0x12, 0));
        assert!(!vga.attr_address_phase);
        assert_eq!(vga.io_read_u8(0x3da, 0).unwrap() & 0x08, 0);
        assert!(vga.attr_address_phase);
    }

    #[test]
    fn retrace_has_sixty_frames_per_second_without_display_polling() {
        let mut vga = PcVga::new();
        let mut rising_edges = 0;
        let mut previous = false;
        for ms in 0..1000 {
            let now = ms * (16 * 1_193_182) / 1000;
            let status = vga.io_read_u8(0x3da, now).unwrap();
            assert_eq!(vga.io_read_u8(0x3da, now), Some(status));
            assert_eq!(vga.io_read_u8(0x3ba, now), Some(status));
            let retrace = status & 8 != 0;
            if retrace {
                assert_ne!(status & 1, 0, "vertical blank also disables display");
            }
            rising_edges += usize::from(retrace && !previous);
            previous = retrace;
        }
        assert_eq!(rising_edges, 60);
    }

    #[test]
    fn attribute_register_access_does_not_unblank_the_display() {
        let mut vga = PcVga::new();
        vga.io_write_u8(0x3c0, 0x12, 0);
        assert_eq!(vga.mode_info().mode, RenderMode::Blank);
        vga.clear_dirty();
        vga.io_read_u8(0x3da, 0);
        assert_eq!(vga.mode_info().mode, RenderMode::Blank);
        assert!(!vga.whole_frame_dirty());
        vga.io_write_u8(0x3c0, 0x12, 0);
        vga.io_write_u8(0x3c0, 0x0f, 0);
        assert_eq!(vga.mode_info().mode, RenderMode::Blank);
        let mut pixels = alloc::vec![0xa5; 720 * 400 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        assert!(pixels.iter().all(|&byte| byte == 0));
        vga.clear_dirty();
        vga.io_write_u8(0x3c0, 0x32, 0);
        assert_eq!(vga.mode_info().mode, RenderMode::Text);
        assert!(vga.whole_frame_dirty());
    }

    #[test]
    fn visible_palette_components_invalidate_the_frame() {
        let mut vga = PcVga::new();
        vga.io_write_u8(0x3c4, 4, 0);
        vga.io_write_u8(0x3c5, 8, 0);
        vga.io_write_u8(0x3c8, 0, 0);
        let mut pixels = alloc::vec![0; 320 * 200 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        for component in [1, 2, 3] {
            let previous = pixels[..4].to_vec();
            vga.clear_dirty();
            vga.io_write_u8(0x3c9, component, 0);
            assert!(vga.whole_frame_dirty());
            vga.render_rgba(&mut pixels).unwrap();
            assert_ne!(&pixels[..4], previous);
        }
    }

    #[test]
    fn vbe_live_format_keeps_pitch_start_and_bounds_coherent() {
        let mut vga = PcVga::new();
        write_vbe(&mut vga, VBE_INDEX_XRES, 16);
        write_vbe(&mut vga, VBE_INDEX_YRES, 2);
        write_vbe(&mut vga, VBE_INDEX_BPP, 32);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        write_vbe(&mut vga, VBE_INDEX_X_OFFSET, 1);
        write_vbe(&mut vga, VBE_INDEX_Y_OFFSET, 1);
        for (offset, pixel) in [(34, 0xf800u16), (66, 0x001f), (50, 0x07e0), (98, 0xffff)] {
            for (byte, value) in pixel.to_le_bytes().into_iter().enumerate() {
                assert!(vga.lfb_write_u8(
                    DEFAULT_LFB_BASE + offset + byte as u64,
                    DEFAULT_LFB_BASE,
                    value,
                ));
            }
        }
        // A live depth/width change must update both pitch and panning, without
        // clearing the framebuffer or resetting the programmed offsets.
        for (index, value, width, stride, first, second) in [
            (
                VBE_INDEX_BPP,
                16,
                16,
                32,
                [255, 0, 0, 255],
                [0, 0, 255, 255],
            ),
            (VBE_INDEX_XRES, 24, 24, 48, [0, 255, 0, 255], [255; 4]),
        ] {
            vga.clear_dirty();
            write_vbe(&mut vga, index, value);
            assert!(vga.whole_frame_dirty());
            let info = vga.mode_info();
            assert_eq!(
                (info.width, info.height, info.bpp, info.stride),
                (width, 2, 16, stride)
            );
            let mut pixels = alloc::vec![0; usize::from(width) * 2 * 4];
            vga.render_rgba(&mut pixels).unwrap();
            assert_eq!(&pixels[..4], &first);
            let row = usize::from(width) * 4;
            assert_eq!(&pixels[row..row + 4], &second);
        }
        for (value, width, height) in [(0, 8, 1), (u16::MAX, VBE_MAX_XRES, VBE_MAX_YRES)] {
            write_vbe(&mut vga, VBE_INDEX_XRES, value);
            write_vbe(&mut vga, VBE_INDEX_YRES, value);
            let info = vga.mode_info();
            assert_eq!((info.width, info.height), (width, height));
        }
        vga.clear_dirty();
        write_vbe(&mut vga, VBE_INDEX_BPP, 7);
        assert_eq!(vga.mode_info().mode, RenderMode::Text);
        assert!(vga.whole_frame_dirty());
    }

    #[test]
    fn vbe_eight_bit_dac_preserves_palette_readback_and_pixels() {
        let mut vga = PcVga::new();
        write_vbe(&mut vga, VBE_INDEX_XRES, 8);
        write_vbe(&mut vga, VBE_INDEX_YRES, 1);
        write_vbe(&mut vga, VBE_INDEX_BPP, 8);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        assert!(vga.lfb_write_u8(DEFAULT_LFB_BASE, DEFAULT_LFB_BASE, 0x20));
        // DISPI bit 5 is the 8-bit DAC advertised by our VBE firmware.
        for (enable, values, expected) in [
            (
                VBE_ENABLED | 0x20,
                [0x80, 0xa5, 0xff],
                [0x80, 0xa5, 0xff, 255],
            ),
            (VBE_ENABLED, [0x20, 0x2a, 0x3f], [130, 170, 255, 255]),
        ] {
            vga.clear_dirty();
            write_vbe(&mut vga, VBE_INDEX_ENABLE, enable);
            assert!(vga.whole_frame_dirty());
            vga.io_write_u8(0x3c8, 0x20, 0);
            for value in values {
                vga.io_write_u8(0x3c9, value, 0);
            }
            assert_eq!(vga.io_read_u8(0x3c8, 0), Some(0x21));
            let mut pixels = [0; 8 * 4];
            vga.render_rgba(&mut pixels).unwrap();
            assert_eq!(&pixels[..4], &expected);
            // 3C7 reports the last index access mode, not the read index.
            assert_eq!(vga.io_read_u8(0x3c7, 0), Some(0));
            vga.io_write_u8(0x3c7, 0x20, 0);
            assert_eq!(vga.io_read_u8(0x3c7, 0), Some(3));
            for value in values {
                assert_eq!(vga.io_read_u8(0x3c9, 0), Some(value));
            }
        }
    }

    #[test]
    fn dispi_getcaps_and_enable_edge_behaviour() {
        let mut vga = PcVga::new();
        vga.io_write_u16(0x1ce, VBE_INDEX_ENABLE as u16, 0);
        vga.io_write_u16(0x1cf, VBE_GETCAPS, 0);
        vga.io_write_u16(0x1ce, VBE_INDEX_XRES as u16, 0);
        assert_eq!(vga.io_read_u16(0x1cf, 0), Some(VBE_MAX_XRES));
        vga.io_write_u16(0x1ce, VBE_INDEX_BPP as u16, 0);
        vga.io_write_u16(0x1cf, 7, 0);
        vga.io_write_u16(0x1ce, VBE_INDEX_ENABLE as u16, 0);
        vga.io_write_u16(0x1cf, VBE_ENABLED, 0);
        assert_eq!(vga.invalid_vbe_bpp_count(), 1);
        assert!(!vga.vbe_enabled());
        vga.io_write_u16(0x1ce, VBE_INDEX_XRES as u16, 0);
        vga.io_write_u16(0x1cf, 641, 0);
        vga.io_write_u16(0x1ce, VBE_INDEX_YRES as u16, 0);
        vga.io_write_u16(0x1cf, 480, 0);
        vga.io_write_u16(0x1ce, VBE_INDEX_BPP as u16, 0);
        vga.io_write_u16(0x1cf, 32, 0);
        vga.io_write_u16(0x1ce, VBE_INDEX_ENABLE as u16, 0);
        vga.io_write_u16(0x1cf, VBE_ENABLED | VBE_NOCLEARMEM, 0);
        assert_eq!(vga.mode_info().width, 640);
        assert_eq!(vga.mode_info().stride, 640 * 4);
    }

    fn write_vbe(vga: &mut PcVga, index: usize, value: u16) {
        assert!(vga.io_write_u16(0x1ce, index as u16, 0));
        assert!(vga.io_write_u16(0x1cf, value, 0));
    }

    #[test]
    fn vbe_planar_banks_preserve_latches_and_scanout_pitch() {
        let mut vga = planar_probe();
        write_vbe(&mut vga, VBE_INDEX_XRES, 1024);
        write_vbe(&mut vga, VBE_INDEX_YRES, 768);
        write_vbe(&mut vga, VBE_INDEX_BPP, 4);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        assert!(vga.vbe_enabled());
        write_planes(&mut vga, 0xa0000, [0, 0, 0x80, 0]);
        write_planes(&mut vga, 0xaffff, [0, 0x80, 0, 0]);
        write_vbe(&mut vga, VBE_INDEX_BANK, 1);
        write_planes(&mut vga, 0xa0000, [0x80; 4]);
        assert_eq!(read_planes(&mut vga, 0xa0000), [0x80; 4]);
        // Copy from another bank through the read latches; plane writes must
        // not take the packed-framebuffer path just because DISPI is enabled.
        write_vbe(&mut vga, VBE_INDEX_BANK, 0);
        vga.io_write_u16(0x3ce, 0x0105, 0);
        vga.io_write_u16(0x3ce, 0x1f03, 0);
        vga.io_write_u16(0x3ce, 0x0008, 0);
        assert!(vga.legacy_write_u8(0xa0000 + 3 * 128, 0));
        assert_eq!(read_planes(&mut vga, 0xa0000), [0, 0, 0x80, 0]);
        let mut pixels = alloc::vec![0xa5; 1024 * 768 * 4 + 4];
        let info = vga.render_rgba(&mut pixels).unwrap();
        assert_eq!(
            (info.width, info.height, info.bpp, info.stride),
            (1024, 768, 4, 128)
        );
        for (x, y, color) in [
            (0, 0, [170, 0, 0, 255]),
            (0, 3, [255; 4]),
            (1016, 511, [0, 170, 0, 255]),
            (0, 512, [255; 4]),
        ] {
            let pos = (y * 1024 + x) * 4;
            assert_eq!(&pixels[pos..pos + 4], &color, "at {x},{y}");
            assert_eq!(&pixels[pos + 4..pos + 8], &[0, 0, 0, 255]);
        }
        assert_eq!(&pixels[1024 * 768 * 4..], &[0xa5; 4]);
        // A live virtual pitch/pan spans the bank boundary on the first row.
        write_vbe(&mut vga, VBE_INDEX_VIRT_WIDTH, 1032);
        write_vbe(&mut vga, VBE_INDEX_X_OFFSET, 24);
        write_vbe(&mut vga, VBE_INDEX_Y_OFFSET, 508);
        vga.render_rgba(&mut pixels).unwrap();
        assert_eq!(&pixels[..4], &[0, 170, 0, 255]);
        assert_eq!(&pixels[8 * 4..9 * 4], &[255; 4]);
        // A new mode clears all four planes and discards the old virtual pitch.
        write_vbe(&mut vga, VBE_INDEX_ENABLE, 0);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        assert_eq!(vga.mode_info().stride, 128);
        write_vbe(&mut vga, VBE_INDEX_BANK, 1);
        assert_eq!(read_planes(&mut vga, 0xa0000), [0; 4]);
    }

    #[test]
    fn planar_scanout_uses_crtc_dimensions_pitch_and_start() {
        let mut vga = planar_probe();
        for (index, value) in [
            (1, 99),
            (7, 0x40),
            (0x12, 0x57), // 800x600
            (0x13, 52),
            (0x17, 0x63), // 104 plane bytes per row
            (0x0c, 0),
            (0x0d, 16),
        ] {
            vga.io_write_u16(0x3d4, (value << 8) | index, 0);
        }
        write_planes(&mut vga, 0xa0000 + 16, [0x80, 0, 0, 0]);
        write_planes(&mut vga, 0xa0000 + 16 + 599 * 104 + 99, [0, 0, 1, 0]);
        let info = vga.mode_info();
        assert_eq!((info.width, info.height, info.stride), (800, 600, 104));
        let mut pixels = alloc::vec![0; 800 * 600 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        assert_eq!(&pixels[..4], &[0, 0, 170, 255]);
        assert_eq!(&pixels[pixels.len() - 4..], &[170, 0, 0, 255]);
    }

    #[test]
    fn vbe_pixel_formats_preserve_pitch_palette_and_output_bounds() {
        for (bpp, expected) in [
            (8, [4, 8, 12, 0xff]),
            (15, [0xff, 0, 0xff, 0xff]),
            (16, [123, 130, 0xff, 0xff]),
            (24, [99, 124, 31, 0xff]),
            (32, [99, 124, 31, 0xff]),
        ] {
            let mut vga = PcVga::new();
            write_vbe(&mut vga, VBE_INDEX_XRES, 8);
            write_vbe(&mut vga, VBE_INDEX_YRES, 2);
            write_vbe(&mut vga, VBE_INDEX_BPP, bpp);
            write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
            write_vbe(&mut vga, VBE_INDEX_VIRT_WIDTH, 16);
            write_vbe(&mut vga, VBE_INDEX_X_OFFSET, 1);
            write_vbe(&mut vga, VBE_INDEX_Y_OFFSET, 1);
            vga.pel_mask = 0x0f;
            vga.dac[15 * 3..16 * 3].copy_from_slice(&[1, 2, 3]);
            let bytes = bytes_per_pixel(bpp) as usize;
            for y in 0..2 {
                for x in 0..8 {
                    let offset = vga.vbe_start_addr as usize + y * 16 * bytes + x * bytes;
                    vga.vram[offset..offset + bytes]
                        .copy_from_slice(&[0x1f, 0x7c, 0x63, 0xcc][..bytes]);
                }
            }
            let mut pixels = [0xa5; 8 * 2 * 4 + 4];
            assert!(vga.render_rgba(&mut pixels[..63]).is_none());
            assert_eq!(pixels, [0xa5; 68]);
            vga.render_rgba(&mut pixels).unwrap();
            for pixel in pixels[..64].chunks_exact(4) {
                assert_eq!(pixel, expected, "bpp={bpp}");
            }
            assert_eq!(&pixels[64..], &[0xa5; 4]);
        }
    }

    #[test]
    fn vbe_rows_preserve_partial_pixels_at_the_end_of_vram() {
        let mut vga = PcVga::new();
        write_vbe(&mut vga, VBE_INDEX_XRES, 8);
        write_vbe(&mut vga, VBE_INDEX_YRES, 2);
        write_vbe(&mut vga, VBE_INDEX_BPP, 24);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        // Pitch 384 and x=84 select the final complete BGR pixel and the last
        // single byte in the 16 MiB aperture. The following row is all absent.
        write_vbe(&mut vga, VBE_INDEX_VIRT_WIDTH, 128);
        write_vbe(&mut vga, VBE_INDEX_Y_OFFSET, (VRAM_SIZE / 384) as u16);
        write_vbe(
            &mut vga,
            VBE_INDEX_X_OFFSET,
            ((VRAM_SIZE % 384 - 4) / 3) as u16,
        );
        assert_eq!(vga.vbe_start_addr as usize, VRAM_SIZE - 4);
        vga.vram[VRAM_SIZE - 4..].copy_from_slice(&[1, 2, 3, 4]);
        let mut pixels = [0xa5; 8 * 2 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        assert_eq!(&pixels[..8], &[3, 2, 1, 0xff, 0, 0, 4, 0xff]);
        for pixel in pixels[8..].chunks_exact(4) {
            assert_eq!(pixel, &[0, 0, 0, 0xff]);
        }
    }

    #[test]
    fn display_blank_and_unblank_redraw_without_erasing_vram() {
        let mut vga = PcVga::new();
        assert!(vga.legacy_write_u8(0xb8000, b'A'));
        let mut pixels = alloc::vec![0; 720 * 400 * 4];
        assert_eq!(vga.render_rgba(&mut pixels).unwrap().mode, RenderMode::Text);
        let original = pixels.clone();
        assert!(original.iter().any(|byte| *byte != 0));

        for enabled in [false, true] {
            vga.clear_dirty();
            vga.io_read_u8(0x3da, 0);
            vga.io_write_u8(0x3c0, if enabled { 0x20 } else { 0 }, 0);
            assert!(vga.whole_frame_dirty());
            vga.render_rgba(&mut pixels).unwrap();
            if enabled {
                assert_eq!(pixels, original);
            } else {
                assert!(pixels.iter().all(|byte| *byte == 0));
            }
            assert_eq!(vga.text_byte(0), Some(b'A'));
        }
    }

    #[test]
    fn vbe_live_pitch_and_offsets_change_display_without_enabling_again() {
        let mut vga = PcVga::new();
        write_vbe(&mut vga, VBE_INDEX_XRES, 8);
        write_vbe(&mut vga, VBE_INDEX_YRES, 2);
        write_vbe(&mut vga, VBE_INDEX_BPP, 32);
        write_vbe(&mut vga, VBE_INDEX_ENABLE, VBE_ENABLED);
        for (offset, color) in [(0, [1, 2, 3]), (32, [4, 5, 6]), (68, [7, 8, 9])] {
            for (byte, value) in color.into_iter().enumerate() {
                assert!(vga.lfb_write_u8(
                    DEFAULT_LFB_BASE + offset + byte as u64,
                    DEFAULT_LFB_BASE,
                    value,
                ));
            }
        }
        let mut pixels = alloc::vec![0xa5; 8 * 2 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        assert_eq!(&pixels[..4], &[3, 2, 1, 0xff]);
        assert_eq!(&pixels[32..36], &[6, 5, 4, 0xff]);
        for (index, value, expected) in [
            (VBE_INDEX_VIRT_WIDTH, 16, [3, 2, 1, 0xff]),
            (VBE_INDEX_X_OFFSET, 1, [0, 0, 0, 0xff]),
            (VBE_INDEX_Y_OFFSET, 1, [9, 8, 7, 0xff]),
        ] {
            vga.clear_dirty();
            write_vbe(&mut vga, index, value);
            assert!(vga.whole_frame_dirty());
            assert_eq!(vga.render_rgba(&mut pixels).unwrap().stride, 64);
            assert_eq!(&pixels[..4], &expected);
            assert_eq!(vga.lfb_read_u8(DEFAULT_LFB_BASE, DEFAULT_LFB_BASE), Some(1));
        }
    }

    #[test]
    fn vbe_mode_switch_replaces_host_pixels_and_honors_noclearmem() {
        let mut vga = PcVga::new();
        let mut pixels = alloc::vec![0xa5; 720 * 400 * 4];
        vga.render_rgba(&mut pixels).unwrap();
        for (width, height, enable) in [
            (16, 2, VBE_ENABLED | VBE_NOCLEARMEM),
            (8, 1, VBE_ENABLED | VBE_NOCLEARMEM),
            (16, 2, VBE_ENABLED),
        ] {
            write_vbe(&mut vga, VBE_INDEX_ENABLE, 0);
            for offset in 0..128 {
                assert!(vga.lfb_write_u8(DEFAULT_LFB_BASE + offset, DEFAULT_LFB_BASE, 0x55));
            }
            write_vbe(&mut vga, VBE_INDEX_XRES, width);
            write_vbe(&mut vga, VBE_INDEX_YRES, height);
            write_vbe(&mut vga, VBE_INDEX_BPP, 32);
            write_vbe(&mut vga, VBE_INDEX_ENABLE, enable);
            pixels.fill(0xa5);
            let info = vga.render_rgba(&mut pixels).unwrap();
            assert_eq!((info.width, info.height), (width, height));
            let byte = if enable & VBE_NOCLEARMEM != 0 {
                0x55
            } else {
                0
            };
            let len = width as usize * height as usize * 4;
            assert!(
                pixels[..len]
                    .chunks_exact(4)
                    .all(|p| p == [byte, byte, byte, 0xff])
            );
            assert_eq!(
                vga.lfb_read_u8(DEFAULT_LFB_BASE, DEFAULT_LFB_BASE),
                Some(byte)
            );
        }
    }

    #[test]
    fn font_plane_rendering_uses_guest_loaded_glyph() {
        let mut vga = PcVga::new();
        vga.legacy_write_u8(0xb8000, b'A');
        vga.legacy_write_u8(0xb8001, 0x0f);
        let glyph_offset = usize::from(b'A') * 32;
        vga.vram[(glyph_offset << 2) | 2] = 0x80;
        let mut out = alloc::vec![0; 720 * 400 * 4];
        let info = vga.render_rgba(&mut out).unwrap();
        assert_eq!(info.mode, RenderMode::Text);
        assert_eq!(&out[0..4], &[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(&out[4..8], &[0, 0, 0, 0xff]);
    }

    #[test]
    fn text_mode_starts_with_readable_default_font() {
        let mut vga = PcVga::new();
        vga.legacy_write_u8(0xb8000, b'A');
        vga.legacy_write_u8(0xb8001, 0x0f);

        let mut out = alloc::vec![0; 720 * 400 * 4];
        vga.render_rgba(&mut out).unwrap();

        let cell = &out[..9 * 16 * 4];
        assert!(
            cell.chunks_exact(4)
                .any(|px| px == [0xff, 0xff, 0xff, 0xff])
        );
    }
}
