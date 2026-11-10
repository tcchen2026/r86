use super::*;
use alloc::{vec, vec::Vec};

fn command(s: &mut Sound, bytes: &[u8]) {
    for &b in bytes {
        s.write(0x22c, b);
    }
}
fn mixer(s: &mut Sound, reg: u8, value: u8) {
    s.write(0x224, reg);
    s.write(0x225, value);
}
fn irq(s: &mut Sound) -> u8 {
    s.write(0x224, 0x82);
    s.read(0x225)
}
fn word(s: &mut Sound, port: u16, value: u16) {
    s.write(port, value as u8);
    s.write(port, (value >> 8) as u8);
}
fn dma(s: &mut Sound, wide: bool, auto: bool, address: u32, units: u16) {
    s.write(0xd4, 0); // AT cascade
    let (mask, ff, mode, addr, count, page) = if wide {
        (0xd4, 0xd8, 0xd6, 0xc4, 0xc6, 0x8b)
    } else {
        (0xa, 0xc, 0xb, 2, 3, 0x83)
    };
    s.write(mask, 5);
    s.write(ff, 0);
    s.write(mode, 0x49 | if auto { 0x10 } else { 0 });
    word(s, addr, (address >> u32::from(wide)) as u16);
    word(s, count, units - 1);
    s.write(page, (address >> 16) as u8);
    s.write(mask, 1);
}
fn count(s: &mut Sound, wide: bool) -> u16 {
    s.write(if wide { 0xd8 } else { 0xc }, 0);
    let port = if wide { 0xc6 } else { 3 };
    u16::from_le_bytes([s.read(port), s.read(port)])
}
fn full_volume(s: &mut Sound) {
    for r in 0x30..=0x35 {
        mixer(s, r, 0xf8);
    }
}
fn tick(samples: u64, rate: u64) -> u64 {
    (samples * TICKS_PER_SECOND).div_ceil(rate)
}

#[test]
fn sound_reset_version_test_and_independent_irq_ack() {
    let mut s = Sound::default();
    s.write(0x226, 1);
    assert_eq!(s.read(0x22e) & 0x80, 0);
    s.write(0x226, 0);
    assert_ne!(s.read(0x22e) & 0x80, 0);
    assert_eq!(s.read(0x22a), 0xaa);
    command(&mut s, &[0xe1]);
    assert_eq!([s.read(0x22a), s.read(0x22a)], [4, 5]);
    command(&mut s, &[0xe4, 0x39, 0xe8]);
    assert_eq!(s.read(0x22a), 0x39);
    command(&mut s, &[0xe0, 0x39]);
    assert_eq!(s.read(0x22a), !0x39);
    command(&mut s, &[0xf2, 0xf3]);
    assert_eq!(irq(&mut s), 3);
    s.read(0x22e);
    assert_eq!(irq(&mut s), 2);
    assert!(s.irq());
    s.read(0x22f);
    assert_eq!(irq(&mut s), 0);
    assert!(!s.irq());
    command(&mut s, &[0xd1, 0xd8]);
    assert_eq!(s.read(0x22a), 0xff);
    command(&mut s, &[0xd3, 0xd8]);
    assert_eq!(s.read(0x22a), 0);
    command(&mut s, &[0xff, 0xe1]);
    assert_eq!(s.read(0x22a), 4);
}

#[test]
fn sound_dsp_register_commands_preserve_wdm_playback() {
    let mut s = Sound::default();
    s.write(0x226, 1);
    s.write(0x226, 0);
    assert_eq!(s.read(0x22a), 0xaa);

    // Windows 2000's WDM driver reads register 05h and sets low control bits.
    command(&mut s, &[0x0f, 0x05]);
    assert_ne!(s.read(0x22e) & 0x80, 0);
    assert_eq!(s.read(0x22a), 1);
    command(&mut s, &[0x0e, 0x05, 0x03, 0x0f, 0x05]);
    assert_eq!(s.read(0x22a), 3);
    assert_eq!(s.read(0x22e) & 0x80, 0);

    // ASP setup operands can themselves look like playback commands. The
    // unsupported codec commands must consume them without starting DMA.
    command(&mut s, &[0x04, 0xb6, 0x05, 0xb0, 0xd3, 0xe1]);
    assert_eq!([s.read(0x22a), s.read(0x22a)], [4, 5]);
    assert_eq!(s.next_event(), u64::MAX);

    s.enable(0);
    full_volume(&mut s);
    dma(&mut s, true, true, 0, 4);
    let ram = [0x34, 0x12, 0, 0x80, 0, 0, 0, 0];
    command(&mut s, &[0x41, 0x56, 0x22, 0xb6, 0x30, 0xff, 0xff]);
    s.advance(tick(1, 22050), &ram);
    assert_eq!(s.sb.dac, [0x1234, -32768]);
    s.advance(tick(2, 22050), &ram);
    assert_eq!(s.sb.dac, [0, 0]);
    assert_eq!(count(&mut s, true), 3); // 8237 auto-init reloaded

    // Observed stop sequence: read register 10h, write its low control bits,
    // exit auto-init, then mask DMA. It must not inject two direct-DAC values.
    command(&mut s, &[0x0f, 0x10]);
    assert_ne!(s.read(0x22e) & 0x80, 0);
    assert_eq!(s.read(0x22a), 0);
    command(&mut s, &[0x0e, 0x10, 0x02, 0xd9]);
    s.write(0xd4, 5);
    s.advance(tick(3, 22050), &ram);
    assert_eq!(s.sb.dac, [0, 0]);
    assert_eq!(count(&mut s, true), 3);
    assert_eq!(irq(&mut s), 0);
    let mut pcm = Vec::new();
    s.drain(&mut pcm);
    assert!(pcm.chunks_exact(2).any(|frame| frame == [0x1234, -32768]));
    assert_eq!(&pcm[pcm.len() - 2..], &[0, 0]);
    command(&mut s, &[0x0f, 0x10]);
    assert_eq!(s.read(0x22a), 2);
}

#[test]
fn sound_dma_single_has_exact_count_deadline_and_pcm() {
    let mut s = Sound::default();
    let ram = [0, 128, 255, 128];
    s.enable(0);
    full_volume(&mut s);
    dma(&mut s, false, false, 0, 4);
    command(&mut s, &[0x41, 0x1f, 0x40, 0xc0, 0, 3, 0]);
    let end = tick(4, 8000);
    s.advance(end - 1, &ram);
    assert_eq!(irq(&mut s), 0);
    assert_eq!(count(&mut s, false), 0);
    s.advance(end, &ram);
    assert_eq!(irq(&mut s), 1);
    assert_eq!(count(&mut s, false), 0xffff);
    assert_eq!(s.read(8) & 2, 2);
    assert_eq!(s.read(8) & 2, 0);
    let mut pcm = Vec::new();
    s.drain(&mut pcm);
    assert_eq!(pcm.len(), 48);
    for frame in 0..24 {
        let expected = match frame {
            0..=4 => 0,
            5..=10 => -32768,
            11..=16 => 0,
            17..=22 => 32512,
            _ => 0,
        };
        assert_eq!(
            &pcm[frame * 2..frame * 2 + 2],
            &[expected; 2],
            "frame {frame}"
        );
    }
}

#[test]
fn sound_autoinit_pause_continue_exit_and_dma_reload() {
    let mut s = Sound::default();
    let ram = [32, 64, 128, 192];
    dma(&mut s, false, true, 0, 4);
    command(&mut s, &[0x40, 131, 0x48, 1, 0, 0x1c]); // 8000 bytes/s, two-byte blocks
    let one = tick(1, 8000);
    s.advance(one, &ram);
    command(&mut s, &[0xd0]);
    s.advance(one + TICKS_PER_SECOND, &ram);
    assert_eq!(count(&mut s, false), 2);
    assert_eq!(irq(&mut s), 0);
    command(&mut s, &[0xd4]);
    s.advance(TICKS_PER_SECOND + tick(2, 8000), &ram);
    assert_eq!(irq(&mut s), 1);
    s.read(0x22e);
    command(&mut s, &[0xda]);
    s.advance(TICKS_PER_SECOND + tick(4, 8000), &ram);
    assert_eq!(count(&mut s, false), 3);
    assert_eq!(irq(&mut s), 1);
    s.read(0x22e);
    s.advance(3 * TICKS_PER_SECOND, &ram);
    assert_eq!(count(&mut s, false), 3);
    assert_eq!(irq(&mut s), 0);
}

#[test]
fn sound_16bit_stereo_counts_words_and_preserves_channel_order() {
    let mut s = Sound::default();
    let ram = [0x34, 0x12, 0, 0x80, 0xff, 0x7f, 0, 0];
    s.enable(0);
    full_volume(&mut s);
    dma(&mut s, true, false, 0, 4);
    command(&mut s, &[0x41, 0x1f, 0x40, 0xb0, 0x30, 3, 0]);
    s.advance(tick(2, 8000) - 1, &ram);
    assert_eq!(irq(&mut s), 0);
    s.advance(tick(2, 8000), &ram);
    assert_eq!(irq(&mut s), 2);
    assert_eq!(count(&mut s, true), 0xffff);
    let mut pcm = Vec::new();
    s.drain(&mut pcm);
    assert_eq!(&pcm[10..12], &[0x1234, -32768]);
    assert_eq!(&pcm[22..24], &[32767, 0]);
}

#[test]
fn sound_dma_masks_wrap_decrement_and_independent_flipflops() {
    let mut s = Sound::default();
    let mut ram = vec![0; 0x40000];
    ram[0x1ffff] = 7;
    ram[0x10000] = 9;
    ram[0x20000] = 11;
    dma(&mut s, false, false, 0x1ffff, 2);
    let mut bytes = [0; 2];
    assert_eq!(s.dma.read_memory(1, &ram, &mut bytes), 2);
    assert_eq!(bytes, [7, 9]);
    dma(&mut s, false, true, 0x10000, 2);
    s.write(0xb, 0x79);
    assert_eq!(s.dma.read_memory(1, &ram, &mut bytes), 2);
    assert_eq!(bytes, [9, 7]);
    assert_eq!(count(&mut s, false), 1);
    s.write(0xa, 5);
    assert_eq!(s.dma.read_memory(1, &ram, &mut bytes), 0);
    s.write(0xc, 0);
    s.write(0xd8, 0);
    s.write(2, 0x34);
    s.write(0xc4, 0x78);
    s.write(2, 0x12);
    s.write(0xc4, 0x56);
    assert_eq!(
        [s.read(2), s.read(2), s.read(0xc4), s.read(0xc4)],
        [0x34, 0x12, 0x78, 0x56]
    );
    // Word DMA ignores page bit zero, increments words, and wraps at 128 KiB.
    ram[0x1fffe..0x20000].copy_from_slice(&[0x12, 0x34]);
    ram[0..2].copy_from_slice(&[0x56, 0x78]);
    dma(&mut s, true, false, 0x1fffe, 2);
    let mut words = [0; 4];
    assert_eq!(s.dma.read_memory(5, &ram, &mut words), 4);
    assert_eq!(words, [0x12, 0x34, 0x56, 0x78]);
}

#[test]
fn sound_queue_drain_and_overflow_never_change_dma_or_irq_time() {
    let mut a = Sound::default();
    let mut b = Sound::default();
    a.enable(0);
    b.enable(0);
    for s in [&mut a, &mut b] {
        dma(s, false, true, 0, 4);
        command(s, &[0x41, 0x1f, 0x40, 0xc4, 0, 3, 0]);
    }
    let ram = [32, 128, 224, 128];
    let mut sink = Vec::new();
    for i in 1..=100 {
        let time = TICKS_PER_SECOND * i / 100;
        a.advance(time, &ram);
        b.advance(time, &ram);
        a.drain(&mut sink);
    }
    assert_eq!(a.dropped_frames(), 0);
    assert!(b.dropped_frames() > 0);
    assert_eq!(count(&mut a, false), count(&mut b, false));
    assert_eq!(irq(&mut a), irq(&mut b));
    assert_eq!(a.next_event(), b.next_event());
}

#[test]
fn sound_pause_rate_change_and_reset_keep_the_output_timeline() {
    let mut s = Sound::default();
    s.enable(0);
    full_volume(&mut s);
    dma(&mut s, false, false, 0, 4);
    command(&mut s, &[0x40, 131, 0x14, 3, 0]);
    let ram = [32, 64, 128, 192];
    s.advance(tick(1, 8000), &ram);
    command(&mut s, &[0xd0, 0x40, 6]); // 4 kHz after continue
    let resumed = tick(1, 8000) + TICKS_PER_SECOND;
    s.advance(resumed, &ram);
    command(&mut s, &[0xd4]);
    let end = resumed + tick(3, 4000);
    s.advance(end - 1, &ram);
    assert_eq!(irq(&mut s), 0);
    s.advance(end, &ram);
    assert_eq!(irq(&mut s), 1);
    let mut before = Vec::new();
    s.drain(&mut before);
    s.advance(end + TICKS_PER_SECOND / 100, &ram);
    let pending = s.mixer.queue.len();
    s.reset(end + TICKS_PER_SECOND / 100);
    assert_eq!(s.mixer.queue.len(), pending);
    s.advance(end + TICKS_PER_SECOND / 50, &ram);
    let mut after = Vec::new();
    s.drain(&mut after);
    assert!(after.len() >= 2 * 959);
    assert_eq!(after.last(), Some(&0));
}

#[test]
fn sound_opl2_timers_masks_reset_and_port_aliases() {
    let mut s = Sound::default();
    let write = |s: &mut Sound, base, reg, value| {
        s.write(base, reg);
        s.write(base + 1, value);
    };
    assert_eq!(s.read(0x388), 6);
    write(&mut s, 0x388, 4, 0x60);
    write(&mut s, 0x388, 4, 0x80);
    write(&mut s, 0x220, 2, 255);
    write(&mut s, 0x228, 4, 0x21); // timer A on, timer B masked
    s.advance(1535, &[]);
    assert_eq!(s.read(0x388), 6);
    s.advance(1536, &[]);
    assert_eq!(s.read(0x388), 0xc6);
    assert_eq!(s.read(0x220), 0xc6);
    assert_eq!(s.read(0x228), 0xc6);
    assert!(!s.irq(), "AdLib has no ISA IRQ connection");
    write(&mut s, 0x388, 4, 0x80); // flags reset, timer keeps running
    s.advance(3072, &[]);
    assert_eq!(s.read(0x388), 0xc6);
    write(&mut s, 0x388, 4, 0x60);
    write(&mut s, 0x388, 3, 255);
    write(&mut s, 0x388, 4, 0x42);
    s.advance(3072 + 3072 - 1, &[]); // B's divide-by-16 phase was already eight samples in
    assert_eq!(s.read(0x388), 6);
    s.advance(6144, &[]);
    assert_eq!(s.read(0x388), 0xa6);
    write(&mut s, 0x388, 4, 0x60);
    s.advance(100_000, &[]);
    assert_eq!(s.read(0x388), 6);
    assert_eq!(s.read(0x389), 0xff);
    assert!(!Sound::contains(0x222));
}

#[test]
fn sound_opl2_native_pcm_matches_pinned_ymfm() {
    // 48,000 native samples: nine voices, all waveforms, feedback, KSL/KSR,
    // release/retrigger, deep AM/PM, five rhythm voices and timer-A CSM.
    // Oracle: tests/opl2_reference.cpp + ymfm 81aec25, bytes compared directly
    // before recording this FNV-1a. The vector is an original register program.
    let events: Vec<_> = include_str!("opl2_vectors.txt")
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            (
                fields.next().unwrap().parse::<u64>().unwrap(),
                u8::from_str_radix(fields.next().unwrap(), 16).unwrap(),
                u8::from_str_radix(fields.next().unwrap(), 16).unwrap(),
            )
        })
        .collect();
    let mut opl = Opl2::default();
    let mut event = 0;
    let mut hash = 0xcbf29ce484222325u64;
    for i in 0..48_000 {
        while event < events.len() && events[event].0 == i {
            opl.address(events[event].1);
            opl.write(events[event].2, i * 384);
            event += 1;
        }
        for byte in opl.advance((i + 1) * 384).to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    assert_eq!(hash, 0x2f5c58faefe37adc);
}
