//! Guest audio to the default output: waveOut, 48 kHz stereo, four 20 ms
//! buffers refilled from the runner's queue by one thread. An empty queue
//! plays silence; without an output device the VM runs silently.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    CALLBACK_EVENT, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR, WHDR_DONE,
    waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader,
    waveOutWrite,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{PCWSTR, PSTR};

use super::runner::{Shared, lock};

const RATE: u32 = 48_000;
const FRAMES: usize = 960; // 20 ms
const BUFFERS: usize = 4;

/// WAVEHDR is declared packed; aligned here so its flags can be read in place.
#[repr(C, align(8))]
struct Header(WAVEHDR);

pub struct Audio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    pub fn start(shared: Arc<Shared>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("r86 audio".into())
            .spawn(move || play(&shared, &flag))
            .ok();
        Self { stop, thread }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn play(shared: &Shared, stop: &AtomicBool) {
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let mut samples = vec![[0i16; FRAMES * 2]; BUFFERS];
    let header_size = size_of::<WAVEHDR>() as u32;
    // SAFETY: the device, the event and the headers live until the cleanup
    // below; each header points into a Vec element that outlives playback.
    unsafe {
        let Ok(event) = CreateEventW(None, false, false, PCWSTR::null()) else {
            return;
        };
        let mut out = HWAVEOUT::default();
        if waveOutOpen(
            Some(&mut out),
            WAVE_MAPPER,
            &format,
            Some(event.0 as usize),
            None,
            CALLBACK_EVENT,
        ) != 0
        {
            let _ = CloseHandle(event);
            return;
        }
        let mut headers: Vec<Header> = samples
            .iter_mut()
            .map(|buffer| {
                Header(WAVEHDR {
                    lpData: PSTR(buffer.as_mut_ptr().cast()),
                    dwBufferLength: (FRAMES * 4) as u32,
                    ..Default::default()
                })
            })
            .collect();
        for Header(header) in &mut headers {
            waveOutPrepareHeader(out, header, header_size);
            header.dwFlags |= WHDR_DONE; // free to fill
        }
        while !stop.load(Ordering::Relaxed) {
            for Header(header) in &mut headers {
                // The driver sets WHDR_DONE from its own thread; the field is
                // 4-aligned inside the aligned Header.
                if std::ptr::read_volatile(std::ptr::addr_of!(header.dwFlags)) & WHDR_DONE == 0 {
                    continue;
                }
                let buffer =
                    std::slice::from_raw_parts_mut(header.lpData.0.cast::<i16>(), FRAMES * 2);
                let mut queue = lock(&shared.audio);
                for sample in buffer.iter_mut() {
                    *sample = queue.pop_front().unwrap_or(0);
                }
                drop(queue);
                header.dwFlags &= !WHDR_DONE;
                waveOutWrite(out, header, header_size);
            }
            WaitForSingleObject(event, 50);
        }
        waveOutReset(out);
        for Header(header) in &mut headers {
            waveOutUnprepareHeader(out, header, header_size);
        }
        waveOutClose(out);
        let _ = CloseHandle(event);
    }
}
