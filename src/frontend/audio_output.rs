//! Host consumers of the machine's 48 kHz signed stereo stream. File output
//! records guest time; SDL queue correction affects only host presentation.

use crate::devices::audio::SAMPLE_RATE;
use crate::machine::Machine;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};

pub struct AudioOutput {
    wav: Option<Wav>,
    playback: Option<Playback>,
    samples: Vec<i16>,
    dropped: u64,
    /// Playback level in percent; WAV capture is never scaled.
    volume: u8,
    /// Whether this output has the machine's mixer running.
    rendering: bool,
}

impl AudioOutput {
    pub fn new(wav: Option<&str>, playback: bool) -> Result<Self, String> {
        Ok(Self {
            wav: wav
                .map(Wav::new)
                .transpose()
                .map_err(|e| format!("audio WAV: {e}"))?,
            playback: if playback {
                Some(Playback::new()?)
            } else {
                None
            },
            samples: Vec::new(),
            dropped: 0,
            volume: 100,
            rendering: false,
        })
    }

    pub fn enabled(&self) -> bool {
        self.wav.is_some() || self.playback.is_some()
    }

    /// Sound is optional on handhelds: without an audio device the VM runs silent.
    #[cfg(unix)]
    pub fn handheld() -> Self {
        // The handheld owns SDL video through dlopen. Do not create an sdl2
        // crate Sdl owner here: its final drop calls SDL_Quit for all subsystems.
        let playback = dynamic::Playback::new()
            .inspect_err(|err| eprintln!("r86: {err}; continuing without sound"))
            .ok()
            .map(Playback::Dynamic);
        Self {
            wav: None,
            playback,
            samples: Vec::new(),
            dropped: 0,
            volume: 100,
            rendering: false,
        }
    }

    /// Set the playback level and start or stop the machine's mixer as
    /// needed. Muted playback without a WAV capture stops it, so it costs
    /// nothing. Enabling restarts the stream, so only transitions do it.
    pub fn set_volume(&mut self, machine: &mut Machine, percent: u8) {
        self.volume = percent.min(100);
        let render = self.wav.is_some() || (self.playback.is_some() && self.volume > 0);
        if render != self.rendering {
            if render {
                machine.enable_audio();
            } else {
                machine.disable_audio();
            }
            self.rendering = render;
        }
    }

    pub fn pump(&mut self, machine: &mut Machine) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        machine.drain_audio(&mut self.samples);
        let dropped = machine.audio_dropped_frames();
        if self.wav.is_some() && dropped != self.dropped {
            return Err(format!(
                "audio WAV lost {} frames; shorten the frontend execution slice",
                dropped.saturating_sub(self.dropped)
            ));
        }
        self.dropped = dropped;
        if let Some(wav) = &mut self.wav {
            wav.write(&self.samples)
                .map_err(|e| format!("audio WAV: {e}"))?;
        }
        if let Some(playback) = &mut self.playback
            && self.volume > 0
        {
            if self.volume < 100 {
                let volume = i32::from(self.volume);
                for sample in &mut self.samples {
                    *sample = (i32::from(*sample) * volume / 100) as i16;
                }
            }
            playback.write(&self.samples)?;
        }
        Ok(())
    }

    pub fn finish(&mut self, machine: &mut Machine) -> Result<(), String> {
        self.pump(machine)?;
        if let Some(wav) = &mut self.wav {
            wav.finish().map_err(|e| format!("audio WAV: {e}"))?;
        }
        Ok(())
    }
}

struct Wav {
    file: BufWriter<File>,
    bytes: u32,
}

impl Wav {
    fn new(path: &str) -> io::Result<Self> {
        let mut s = Self {
            file: BufWriter::new(File::create(path)?),
            bytes: 0,
        };
        s.finish()?;
        Ok(s)
    }
    fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let added =
            u32::try_from(samples.len() * 2).map_err(|_| io::Error::other("WAV size overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(added)
            .filter(|&n| n <= u32::MAX - 36)
            .ok_or_else(|| io::Error::other("WAV reached the 4 GiB RIFF limit"))?;
        for s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        Ok(())
    }
    fn finish(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(b"RIFF")?;
        self.file.write_all(&(self.bytes + 36).to_le_bytes())?;
        self.file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x02\0")?;
        self.file.write_all(&SAMPLE_RATE.to_le_bytes())?;
        self.file.write_all(&(SAMPLE_RATE * 4).to_le_bytes())?;
        self.file.write_all(b"\x04\0\x10\0data")?;
        self.file.write_all(&self.bytes.to_le_bytes())?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.flush()
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

enum Playback {
    #[cfg(feature = "sdl2-frontend")]
    Linked(LinkedPlayback),
    #[cfg(unix)]
    Dynamic(dynamic::Playback),
}

impl Playback {
    fn new() -> Result<Self, String> {
        #[cfg(feature = "sdl2-frontend")]
        {
            return LinkedPlayback::new().map(Self::Linked);
        }
        #[cfg(all(unix, not(feature = "sdl2-frontend")))]
        {
            return dynamic::Playback::new().map(Self::Dynamic);
        }
        #[cfg(all(not(unix), not(feature = "sdl2-frontend")))]
        {
            Err("SDL2 audio was not compiled in".into())
        }
    }

    fn write(&mut self, samples: &[i16]) -> Result<(), String> {
        match self {
            #[cfg(feature = "sdl2-frontend")]
            Self::Linked(p) => p.write(samples),
            #[cfg(unix)]
            Self::Dynamic(p) => p.write(samples),
            #[cfg(all(not(unix), not(feature = "sdl2-frontend")))]
            _ => {
                let _ = samples;
                unreachable!()
            }
        }
    }
}

#[cfg(unix)]
mod dynamic {
    use super::SAMPLE_RATE;
    use std::ffi::{CStr, c_char, c_int, c_void};

    const AUDIO: u32 = 0x10;
    #[repr(C)]
    struct Spec {
        freq: c_int,
        format: u16,
        channels: u8,
        silence: u8,
        samples: u16,
        padding: u16,
        size: u32,
        callback: Option<unsafe extern "C" fn(*mut c_void, *mut u8, c_int)>,
        userdata: *mut c_void,
    }
    pub(super) struct Playback {
        lib: *mut c_void,
        device: u32,
        quit: unsafe extern "C" fn(u32),
        close: unsafe extern "C" fn(u32),
        queue: unsafe extern "C" fn(u32, *const c_void, u32) -> c_int,
        size: unsafe extern "C" fn(u32) -> u32,
        clear: unsafe extern "C" fn(u32),
        error: unsafe extern "C" fn() -> *const c_char,
        last: [i16; 2],
        initialized: bool,
    }
    impl Playback {
        pub(super) fn new() -> Result<Self, String> {
            // SAFETY: SDL2 ABI from SDL_audio.h. The owned dlopen reference
            // outlives the device and every resolved function pointer.
            unsafe {
                let lib = [c"libSDL2-2.0.so.0", c"libSDL2.so", c"libSDL2.dylib"]
                    .iter()
                    .map(|n| libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL))
                    .find(|p| !p.is_null())
                    .ok_or("SDL2 audio library is not available")?;
                macro_rules! load {
                    ($name:literal, $ty:ty) => {{
                        let p = libc::dlsym(lib, $name.as_ptr());
                        if p.is_null() {
                            libc::dlclose(lib);
                            return Err(format!("SDL2 lacks {}", $name.to_string_lossy()));
                        }
                        std::mem::transmute::<*mut c_void, $ty>(p)
                    }};
                }
                let init = load!(c"SDL_InitSubSystem", unsafe extern "C" fn(u32) -> c_int);
                let open = load!(
                    c"SDL_OpenAudioDevice",
                    unsafe extern "C" fn(
                        *const c_char,
                        c_int,
                        *const Spec,
                        *mut Spec,
                        c_int,
                    ) -> u32
                );
                let pause = load!(c"SDL_PauseAudioDevice", unsafe extern "C" fn(u32, c_int));
                let mut s = Self {
                    lib,
                    device: 0,
                    last: [0; 2],
                    initialized: false,
                    quit: load!(c"SDL_QuitSubSystem", unsafe extern "C" fn(u32)),
                    close: load!(c"SDL_CloseAudioDevice", unsafe extern "C" fn(u32)),
                    queue: load!(
                        c"SDL_QueueAudio",
                        unsafe extern "C" fn(u32, *const c_void, u32) -> c_int
                    ),
                    size: load!(c"SDL_GetQueuedAudioSize", unsafe extern "C" fn(u32) -> u32),
                    clear: load!(c"SDL_ClearQueuedAudio", unsafe extern "C" fn(u32)),
                    error: load!(c"SDL_GetError", unsafe extern "C" fn() -> *const c_char),
                };
                if init(AUDIO) != 0 {
                    return Err(s.error());
                }
                s.initialized = true;
                let wanted = Spec {
                    freq: SAMPLE_RATE as c_int,
                    format: if cfg!(target_endian = "little") {
                        0x8010
                    } else {
                        0x9010
                    },
                    channels: 2,
                    silence: 0,
                    samples: 1024,
                    padding: 0,
                    size: 0,
                    callback: None,
                    userdata: std::ptr::null_mut(),
                };
                s.device = open(std::ptr::null(), 0, &wanted, std::ptr::null_mut(), 0);
                if s.device == 0 {
                    return Err(s.error());
                }
                pause(s.device, 0);
                Ok(s)
            }
        }
        fn error(&self) -> String {
            // SAFETY: SDL_GetError returns a NUL-terminated string.
            format!(
                "SDL2 audio: {}",
                unsafe { CStr::from_ptr((self.error)()) }.to_string_lossy()
            )
        }
        pub(super) fn write(&mut self, samples: &[i16]) -> Result<(), String> {
            // SAFETY: SDL copies the complete slice before returning. No host
            // callback can touch the machine; SDL owns its internal queue.
            unsafe {
                let queued = (self.size)(self.device);
                if queued > SAMPLE_RATE * 4 / 10 {
                    (self.clear)(self.device);
                }
                if queued == 0 && samples.len() < 480 {
                    let hold: [i16; 480] = core::array::from_fn(|i| self.last[i & 1]);
                    if (self.queue)(self.device, hold.as_ptr().cast(), (hold.len() * 2) as u32) != 0
                    {
                        return Err(self.error());
                    }
                }
                if !samples.is_empty() {
                    if (self.queue)(
                        self.device,
                        samples.as_ptr().cast(),
                        (samples.len() * 2) as u32,
                    ) != 0
                    {
                        return Err(self.error());
                    }
                    self.last.copy_from_slice(&samples[samples.len() - 2..]);
                }
            }
            Ok(())
        }
    }
    impl Drop for Playback {
        fn drop(&mut self) {
            // SAFETY: device closes before its library reference is released.
            unsafe {
                if self.device != 0 {
                    (self.close)(self.device);
                }
                if self.initialized {
                    (self.quit)(AUDIO);
                }
                libc::dlclose(self.lib);
            }
        }
    }
}

#[cfg(feature = "sdl2-frontend")]
struct LinkedPlayback {
    queue: sdl2::audio::AudioQueue<i16>,
    _sdl: sdl2::Sdl,
    last: [i16; 2],
}
#[cfg(feature = "sdl2-frontend")]
impl LinkedPlayback {
    fn new() -> Result<Self, String> {
        let sdl = sdl2::init()?;
        let queue = sdl.audio()?.open_queue(
            None,
            &sdl2::audio::AudioSpecDesired {
                freq: Some(SAMPLE_RATE as i32),
                channels: Some(2),
                samples: Some(1024),
            },
        )?;
        queue.resume();
        Ok(Self {
            queue,
            _sdl: sdl,
            last: [0; 2],
        })
    }
    fn write(&mut self, samples: &[i16]) -> Result<(), String> {
        let queued = self.queue.size();
        if queued > SAMPLE_RATE * 4 / 10 {
            self.queue.clear();
        }
        if queued == 0 && samples.len() < 480 {
            let hold: [i16; 480] = core::array::from_fn(|i| self.last[i & 1]);
            self.queue.queue_audio(&hold)?;
        }
        if !samples.is_empty() {
            self.queue.queue_audio(samples)?;
            self.last.copy_from_slice(&samples[samples.len() - 2..]);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires SDL2; set SDL_AUDIODRIVER=dummy SDL_VIDEODRIVER=dummy"]
    fn sound_sdl_audio_abi_and_queue() {
        #[cfg(unix)]
        // SAFETY: the SDL2 declarations match SDL.h; this test owns and closes
        // a video-subsystem reference to model the handheld's separate owner.
        unsafe {
            let lib = libc::dlopen(c"libSDL2-2.0.so.0".as_ptr(), libc::RTLD_NOW);
            assert!(!lib.is_null());
            let init: unsafe extern "C" fn(u32) -> i32 =
                core::mem::transmute(libc::dlsym(lib, c"SDL_InitSubSystem".as_ptr()));
            let active: unsafe extern "C" fn(u32) -> u32 =
                core::mem::transmute(libc::dlsym(lib, c"SDL_WasInit".as_ptr()));
            let quit: unsafe extern "C" fn(u32) =
                core::mem::transmute(libc::dlsym(lib, c"SDL_QuitSubSystem".as_ptr()));
            assert_eq!(init(0x20), 0);
            let output = super::AudioOutput::handheld();
            assert!(output.enabled());
            drop(output);
            assert_eq!(active(0x20), 0x20, "audio must preserve the video owner");
            quit(0x20);
            libc::dlclose(lib);
        }
        let mut playback = super::Playback::new().unwrap();
        playback.write(&[100, -100, 200, -200]).unwrap();
        playback.write(&vec![0; 12000]).unwrap();
        playback.write(&[]).unwrap();
    }
}
