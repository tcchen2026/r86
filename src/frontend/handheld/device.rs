//! The handheld's screen and keyboard interface, and stop signals.
use std::sync::atomic::{AtomicBool, Ordering};

use super::ui::{Canvas, Rect};

/// Screen and keyboard of the handheld (or of a desktop window imitating it).
pub trait Device {
    fn size(&self) -> (i32, i32);
    fn present(&mut self, canvas: &Canvas) -> Result<(), String>;
    /// VM content without a guest layer (blank screen or remote status).
    /// An external display leaves a notice on the LCD; menus use `present`.
    fn present_vm(&mut self, canvas: &Canvas) -> Result<(), String> {
        self.present(canvas)
    }
    /// None without an LCD/HDMI pair; otherwise whether HDMI is active.
    fn hdmi(&self) -> Option<bool> {
        None
    }
    fn set_hdmi(&mut self, _enabled: bool, _vm_name: &str) -> Result<(), String> {
        Ok(())
    }
    /// Consume a display-loss notification after returning to the LCD.
    fn take_display_disconnect(&mut self) -> bool {
        false
    }
    /// Whether the guest frame is shown as its own scaled layer
    /// (present_frame) instead of being drawn into the UI canvas.
    fn guest_layer(&self) -> bool {
        false
    }
    /// Show a guest RGBA frame at 4:3 (or stretched) within `view`, a
    /// rectangle of the canvas, with `ui_area` of the canvas drawn over it.
    /// Only called when guest_layer is true.
    fn present_frame(
        &mut self,
        _rgba: &[u8],
        _size: (i32, i32),
        _stretch: bool,
        _view: Rect,
        _ui: &Canvas,
        _ui_area: Option<Rect>,
    ) -> Result<(), String> {
        unreachable!("present_frame without a guest layer")
    }
    /// Relative pointer motion since the last call (a controller stick).
    fn pointer_motion(&mut self) -> (i32, i32) {
        (0, 0)
    }
    /// Sleep until input arrives, a stop is requested, or the timeout ends.
    fn wait(&mut self, timeout_ms: i32);
    /// Append pending key events, as evdev codes, without blocking.
    fn read(&mut self, out: &mut Vec<KeyEvent>);
    /// The chord that opens the in-VM menu, for on-screen hints.
    fn menu_keys(&self) -> &'static str;
    /// Mouse and menu controls as (control, action) rows, for the VM menu's
    /// help screen.
    fn help(&self) -> &'static [(&'static str, &'static str)];
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub code: u16,
    /// 0 release, 1 press, 2 autorepeat.
    pub value: i32,
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// SIGTERM (the launcher's hold-ESC exit), SIGINT and SIGHUP request a clean
/// stop. Without SA_RESTART, a blocked poll returns at once.
pub fn install_stop_signals() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_stop as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

pub fn request_stop() {
    STOP.store(true, Ordering::Relaxed);
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}
