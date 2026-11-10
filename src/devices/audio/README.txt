# Sound

`devices/audio.rs` owns an AT DMA pair, Sound Blaster 16 DSP 4.05 and OPL2.
The machine exposes SB16 at 220h, initially IRQ 5, DMA 1/5. DOS software can
use `BLASTER=A220 I5 D1 H5 T6`. Windows 2000 SP4 in-box WDM playback is checked
locally; other Windows drivers and host audio devices need separate validation.

DMA address/count units are bytes on channels 0–3 and words on 5–7. Each
controller has its own byte flip-flop. Transfers respect masks, direction,
terminal count, decrement, auto-init and 64/128 KiB address wrapping. The
primary controller requires the AT cascade channel unmasked. No device
currently requests memory-to-memory, recording or floppy DMA. High page
extensions are absent; unmapped RAM reads return FFh.

The DSP supports reset, version, identification/test, direct DAC, output rate,
time constant, legacy single/auto-init/high-speed PCM, SB Pro stereo, SB16
8/16-bit signed/unsigned mono/stereo PCM, pause/continue, exit auto-init,
timed silence and software IRQ triggers. Lengths count samples, not stereo
frames. IRQ status separates 8/16-bit sources; reads at 22Eh/22Fh acknowledge
them independently. CT1745 master/DAC/FM volume and output gain affect output;
legacy volume registers alias the new controls. Speaker commands retain their
status flag but do not mute DSP 4.xx output, as Creative documents.

Unknown commands produce no reply. Known unsupported recording/ADPCM commands
consume their operands without transferring data. This is a compatibility
limit, not an implementation of those formats. DSP register read/write commands
(0Fh/0Eh) retain byte values for Windows WDM setup/stop; they do not implement
ASP/CSP processing. Mode/codec setup commands consume their operands without
starting playback. No MPU-401, PC speaker, CSP, tone filters, recording, DMA
identification or OPL3 is provided.

DMA and block IRQs advance on the machine's guest oscillator. Transfers copy
bounded chunks and retain each sample's guest timestamp. PCM uses a hold
resampler into 48 kHz signed 16-bit stereo. A bounded 16,384-frame queue drops
oldest frames if the frontend is late; queue occupancy never alters guest time,
DMA counts or IRQ scheduling. Audio writes and DMA port accesses synchronize
the devices first. Device reset retains the frontend subscription.

`Machine::enable_audio` enables output; `drain_audio` supplies interleaved
stereo samples. `frontend/audio_output.rs` provides SDL2 playback and RIFF/WAV
capture. SDL2 uses a bounded host queue and corrects drift only in presentation.
`r86-run --sdl2` and `r86-handheld` play audio. `--audio-wav PATH` captures guest
time in any CLI frontend, before host queue correction; a missed capture chunk
is an error. WAV is PCM stereo 48 kHz and limited to the RIFF 4 GiB size.

OPL2 maps 388h/389h, 228h/229h and 220h/221h. The OPL3 bank at 222h/223h
is unclaimed. OPL2 timers use guest-clock periods and maintain overflow/mask
status for AdLib detection; no AdLib IRQ is wired to the PIC. The 18 fixed
operators run at the native OPL clock / 72 (49,715.9167 Hz) and are held into
the 48 kHz mix. Features include nine melodic voices, four waveforms, ADSR,
KSL/KSR, feedback, AM/PM, five rhythm instruments and ymfm's CSM behavior.
FM port transactions charge 16 oscillator ticks (~838 ns) to the shared
machine clock. This practical ISA latency lets drivers use status-read delay
loops; it is not a cycle-exact ISA bus model. PIT, DMA and FM observe these
wait ticks together, and deterministic scheduler deadlines subtract them from
the remaining instruction budget. Realtime clock sampling never moves backward.

`opl2.rs` is adapted from Aaron Giles's BSD-3-Clause ymfm, commit
81aec25ccbb98f4873a255f7551ac4dadac59b4a: `src/ymfm_opl.h`, `ymfm_opl.cpp`,
`ymfm_fm.h`, `ymfm_fm.ipp`, and `ymfm.h`. Its full notice is at the beginning
of the Rust file. No GPL/LGPL synthesis or RIX decoder is part of r86. The
original register program `opl2_vectors.txt` is compared against 48,000 native
samples from `tests/opl2_reference.cpp`, covering melodic, rhythm and CSM paths.
This verifies fidelity to that reference, not a physical YM3812 measurement.

Focused checks: `cargo test --no-default-features --features host --lib sound_`,
the machine/no_std build, frontend builds, and
`python3 script/sound_check.py --binary PATH/r86-run --bios PATH/bios.bin
--output-dir PATH`. The original bootsector plays a known square wave and
reports `R86_SOUND_OK` from IRQ 5; the script checks the complete golden WAV.
The ignored `sound_sdl_audio_abi_and_queue` test uses SDL_AUDIODRIVER=dummy to
exercise either the linked or dynamic SDL2 ABI and host queue.

References consulted for DMA/SB16 (study, no code copied): tiny386 mainline
`sb16.c`, `i8257.c`, and `pc.c`; v86 `sb16.js` and `dma.js`; 86Box
`snd_sb_dsp.c` for unsupported commands and stereo count behavior; Creative's
[Sound Blaster Series Hardware Programming Guide](https://www.ardent-tool.com/sound/Sound_Blaster_HW_Programming_Guide_1st.pdf),
chapters 2–4 and 6. Creative's sample-count contract takes precedence over
tiny386's stereo single-cycle length workaround.

For DSP register framing/defaults, also consulted Bochs
`bochs/iodev/sound/sb16.cc` (902cc44abc1fe8fe9369b53dc3d7599abc453fb9),
86Box `src/sound/snd_sb_dsp.c` (d2bbd6bc40456d2b110f3f3447ea93504a68b4d3),
and tiny386 `sb16.c` (d47748e2b2d84597365c681e9e09c989cbce97c1). No reference
code was copied. Windows 2000 port traffic supplies the regression sequence.
