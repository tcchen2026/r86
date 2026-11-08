//! SDL2 screen and input, loaded with dlopen so one glibc build runs on any
//! Linux handheld whose firmware ships libSDL2 (KMSDRM on TrimUI), and needs
//! no SDL2 at build time. Two modes: a desktop window imitating the
//! CardputerZero (feature handheld-sdl), or full screen with a game
//! controller, where the guest frame is a separate GPU-scaled layer.
//!
//! Keys and controller buttons become the evdev codes the rest of the
//! frontend reads. Controller buttons are positional (SDL's A is the bottom
//! face button); the mapping suits Nintendo-labelled handhelds, whose A is
//! the right face button. The right stick is the mouse, the left stick arrows;
//! a real mouse (the simulator's, or USB) moves the guest's mouse too.
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::time::{Duration, Instant};

use super::device::{Device, KeyEvent, request_stop};
use super::keymap;
use super::ui::{Canvas, Rect};

const INIT_VIDEO: u32 = 0x20;
const INIT_GAMECONTROLLER: u32 = 0x2000;
const WINDOWPOS_CENTERED: c_int = 0x2FFF_0000;
const WINDOW_FULLSCREEN_DESKTOP: u32 = 0x1001;
const RENDERER_ACCELERATED: u32 = 2;
const PIXELFORMAT_RGB565: u32 = 0x1515_1002;
/// Bytes R, G, B, A in memory on little-endian hosts.
const PIXELFORMAT_ABGR8888: u32 = 0x1676_2004;
const TEXTUREACCESS_STREAMING: c_int = 1;

const QUIT: u32 = 0x100;
const KEYDOWN: u32 = 0x300;
const KEYUP: u32 = 0x301;
const MOUSE_MOTION: u32 = 0x400;
const MOUSE_BUTTON_DOWN: u32 = 0x401;
const MOUSE_BUTTON_UP: u32 = 0x402;
const CONTROLLER_AXIS: u32 = 0x650;
const CONTROLLER_BUTTON_DOWN: u32 = 0x651;
const CONTROLLER_BUTTON_UP: u32 = 0x652;
const CONTROLLER_ADDED: u32 = 0x653;

const AXIS_LEFT_X: u8 = 0;
const AXIS_LEFT_Y: u8 = 1;
const AXIS_RIGHT_X: u8 = 2;
const AXIS_RIGHT_Y: u8 = 3;
const AXIS_TRIGGER_LEFT: u8 = 4;
const AXIS_TRIGGER_RIGHT: u8 = 5;

/// SDL_Rect.
#[repr(C)]
struct SdlRect {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

impl From<Rect> for SdlRect {
    fn from(r: Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

/// SDL_DisplayMode.
#[repr(C)]
struct DisplayMode {
    format: u32,
    w: c_int,
    h: c_int,
    refresh_rate: c_int,
    driver_data: *mut c_void,
}

/// SDL_Event is a 56-byte union; fields are read at their SDL2 ABI offsets.
#[repr(C, align(8))]
struct Event([u8; 64]);

impl Event {
    fn u8(&self, at: usize) -> u8 {
        self.0[at]
    }
    fn u32(&self, at: usize) -> u32 {
        u32::from_ne_bytes(self.0[at..at + 4].try_into().unwrap())
    }
    fn i32(&self, at: usize) -> i32 {
        i32::from_ne_bytes(self.0[at..at + 4].try_into().unwrap())
    }
    fn i16(&self, at: usize) -> i16 {
        i16::from_ne_bytes(self.0[at..at + 2].try_into().unwrap())
    }
}

type Ptr = *mut c_void;

/// The SDL2 functions this frontend calls, resolved once from the library.
struct Api {
    init: unsafe extern "C" fn(u32) -> c_int,
    get_error: unsafe extern "C" fn() -> *const c_char,
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> c_int,
    desktop_mode: unsafe extern "C" fn(c_int, *mut DisplayMode) -> c_int,
    create_window: unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, c_int, u32) -> Ptr,
    create_renderer: unsafe extern "C" fn(Ptr, c_int, u32) -> Ptr,
    output_size: unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int) -> c_int,
    set_logical_size: unsafe extern "C" fn(Ptr, c_int, c_int) -> c_int,
    create_texture: unsafe extern "C" fn(Ptr, u32, c_int, c_int, c_int) -> Ptr,
    destroy_texture: unsafe extern "C" fn(Ptr),
    update_texture: unsafe extern "C" fn(Ptr, *const SdlRect, *const c_void, c_int) -> c_int,
    render_clear: unsafe extern "C" fn(Ptr) -> c_int,
    render_copy: unsafe extern "C" fn(Ptr, Ptr, *const SdlRect, *const SdlRect) -> c_int,
    render_present: unsafe extern "C" fn(Ptr),
    poll_event: unsafe extern "C" fn(*mut Event) -> c_int,
    wait_event_timeout: unsafe extern "C" fn(*mut Event, c_int) -> c_int,
    show_cursor: unsafe extern "C" fn(c_int) -> c_int,
    num_joysticks: unsafe extern "C" fn() -> c_int,
    is_game_controller: unsafe extern "C" fn(c_int) -> c_int,
    game_controller_open: unsafe extern "C" fn(c_int) -> Ptr,
}

impl Api {
    fn load() -> Result<Self, String> {
        // SAFETY: dlopen and dlsym take NUL-terminated names. The handle is
        // never closed, so every resolved function stays valid.
        unsafe {
            let lib = [c"libSDL2-2.0.so.0", c"libSDL2.so"]
                .iter()
                .map(|name| libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL))
                .find(|lib| !lib.is_null())
                .ok_or("libSDL2-2.0.so.0 is not available")?;
            macro_rules! load {
                ($($field:ident = $name:literal),* $(,)?) => {
                    Self { $($field: {
                        let symbol = libc::dlsym(lib, $name.as_ptr());
                        if symbol.is_null() {
                            return Err(format!("libSDL2 lacks {}", $name.to_string_lossy()));
                        }
                        std::mem::transmute::<*mut c_void, _>(symbol)
                    }),* }
                };
            }
            Ok(load! {
                init = c"SDL_Init",
                get_error = c"SDL_GetError",
                set_hint = c"SDL_SetHint",
                desktop_mode = c"SDL_GetDesktopDisplayMode",
                create_window = c"SDL_CreateWindow",
                create_renderer = c"SDL_CreateRenderer",
                output_size = c"SDL_GetRendererOutputSize",
                set_logical_size = c"SDL_RenderSetLogicalSize",
                create_texture = c"SDL_CreateTexture",
                destroy_texture = c"SDL_DestroyTexture",
                update_texture = c"SDL_UpdateTexture",
                render_clear = c"SDL_RenderClear",
                render_copy = c"SDL_RenderCopy",
                render_present = c"SDL_RenderPresent",
                poll_event = c"SDL_PollEvent",
                wait_event_timeout = c"SDL_WaitEventTimeout",
                show_cursor = c"SDL_ShowCursor",
                num_joysticks = c"SDL_NumJoysticks",
                is_game_controller = c"SDL_IsGameController",
                game_controller_open = c"SDL_GameControllerOpen",
            })
        }
    }

    fn error(&self, what: &str) -> String {
        // SAFETY: SDL_GetError returns a valid NUL-terminated string.
        let text = unsafe { CStr::from_ptr((self.get_error)()) };
        format!("{what}: {}", text.to_string_lossy())
    }

    fn hint(&self, name: &CStr, value: &CStr) {
        // SAFETY: both strings are NUL-terminated; SDL copies them.
        unsafe { (self.set_hint)(name.as_ptr(), value.as_ptr()) };
    }

    /// A streaming texture; `smooth` selects linear instead of nearest
    /// filtering, which SDL fixes when the texture is created.
    fn texture(
        &self,
        renderer: Ptr,
        format: u32,
        w: i32,
        h: i32,
        smooth: bool,
    ) -> Result<Ptr, String> {
        let quality = if smooth { c"linear" } else { c"nearest" };
        self.hint(c"SDL_RENDER_SCALE_QUALITY", quality);
        // SAFETY: `renderer` is live; SDL validates the other arguments.
        let texture =
            unsafe { (self.create_texture)(renderer, format, TEXTUREACCESS_STREAMING, w, h) };
        if texture.is_null() {
            return Err(self.error("SDL_CreateTexture"));
        }
        Ok(texture)
    }
}

/// A texture with the size it was created for.
struct Texture {
    raw: Ptr,
    size: (i32, i32),
}

/// Right-stick pointer speed at full deflection, in PS/2 counts per second.
const POINTER_SPEED: f32 = 300.0;
const STICK_DEAD_ZONE: f32 = 0.18;
const TRIGGER_PRESSED: i16 = 16384;
/// The left stick acts as arrow keys, with hysteresis.
const ARROW_PRESS: f32 = 0.5;
const ARROW_RELEASE: f32 = 0.3;
/// Held D-pad, face buttons and left stick repeat like a PC keyboard.
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(80);

pub struct Window {
    api: Api,
    renderer: Ptr,
    size: (i32, i32),
    full_screen: bool,
    ui: Option<Texture>,
    frame: Option<Texture>,
    pending: VecDeque<KeyEvent>,
    /// The repeating key and when it next repeats.
    repeat: Option<(u16, Instant)>,
    /// Left stick direction per axis: -1, 0 or 1.
    arrows: [i8; 2],
    /// Right stick deflection, -1.0..=1.0 per axis.
    stick: (f32, f32),
    triggers: [bool; 2],
    pointer_time: Instant,
    pointer_rest: (f32, f32),
    /// Relative motion of a real mouse (the simulator's, or a USB mouse).
    mouse: (i32, i32),
}

impl Window {
    /// The CardputerZero simulator window, `scale` times its 320x170 panel.
    pub fn simulator(scale: i32) -> Result<Self, String> {
        Self::open(false, (320 * scale, 170 * scale))
    }

    /// Full screen, with the UI at the largest whole multiple of 320x170
    /// that fits and controllers opened.
    pub fn full_screen() -> Result<Self, String> {
        Self::open(true, (320, 170))
    }

    fn open(full_screen: bool, mut window: (i32, i32)) -> Result<Self, String> {
        let api = Api::load()?;
        // SAFETY: plain SDL calls with valid arguments; returned handles are
        // checked and owned by this Window until the process exits.
        unsafe {
            if (api.init)(INIT_VIDEO | INIT_GAMECONTROLLER) != 0 {
                return Err(api.error("SDL_Init"));
            }
            // Size the window to the display too: without a window manager,
            // X11 does not resize a full-screen-desktop window itself.
            let mut mode = DisplayMode {
                format: 0,
                w: 0,
                h: 0,
                refresh_rate: 0,
                driver_data: std::ptr::null_mut(),
            };
            if full_screen && (api.desktop_mode)(0, &mut mode) == 0 {
                window = (mode.w, mode.h);
            }
            let title = CString::new("r86").unwrap();
            let flags = if full_screen {
                WINDOW_FULLSCREEN_DESKTOP
            } else {
                0
            };
            let win = (api.create_window)(
                title.as_ptr(),
                WINDOWPOS_CENTERED,
                WINDOWPOS_CENTERED,
                window.0,
                window.1,
                flags,
            );
            if win.is_null() {
                return Err(api.error("SDL_CreateWindow"));
            }
            let renderer = (api.create_renderer)(win, -1, RENDERER_ACCELERATED);
            if renderer.is_null() {
                return Err(api.error("SDL_CreateRenderer"));
            }
            let (mut w, mut h) = (0, 0);
            (api.output_size)(renderer, &mut w, &mut h);
            let scale = (w / 320).min(h / 170).max(1);
            let size = if full_screen {
                (api.show_cursor)(0);
                (w / scale, h / scale)
            } else {
                (320, 170)
            };
            (api.set_logical_size)(renderer, size.0, size.1);
            let mut this = Self {
                api,
                renderer,
                size,
                full_screen,
                ui: None,
                frame: None,
                pending: VecDeque::new(),
                repeat: None,
                arrows: [0; 2],
                stick: (0.0, 0.0),
                triggers: [false; 2],
                pointer_time: Instant::now(),
                pointer_rest: (0.0, 0.0),
                mouse: (0, 0),
            };
            for index in 0..(this.api.num_joysticks)() {
                this.open_controller(index);
            }
            Ok(this)
        }
    }

    fn open_controller(&mut self, index: c_int) {
        // SAFETY: SDL validates the index; an opened controller lives until exit.
        unsafe {
            if (self.api.is_game_controller)(index) != 0 {
                (self.api.game_controller_open)(index);
            }
        }
    }

    /// Upload `canvas` into the UI texture, (re)creating it for its size.
    fn upload_ui(&mut self, canvas: &Canvas) -> Result<Ptr, String> {
        let size = (canvas.width, canvas.height);
        if self.ui.as_ref().is_none_or(|t| t.size != size) {
            if let Some(old) = self.ui.take() {
                // SAFETY: `old` was created by this renderer and is unused now.
                unsafe { (self.api.destroy_texture)(old.raw) };
            }
            let raw = self
                .api
                .texture(self.renderer, PIXELFORMAT_RGB565, size.0, size.1, false)?;
            self.ui = Some(Texture { raw, size });
        }
        let raw = self.ui.as_ref().unwrap().raw;
        // SAFETY: the canvas holds width*height RGB565 pixels at this pitch.
        let result = unsafe {
            (self.api.update_texture)(
                raw,
                std::ptr::null(),
                canvas.pixels.as_ptr().cast(),
                canvas.width * 2,
            )
        };
        if result != 0 {
            return Err(self.api.error("SDL_UpdateTexture"));
        }
        Ok(raw)
    }

    /// A controller key press or release; navigation and typing keys repeat.
    fn key(&mut self, code: u16, down: bool) {
        self.pending.push_back(KeyEvent {
            code,
            value: i32::from(down),
        });
        let repeats = matches!(
            code,
            keymap::UP
                | keymap::DOWN
                | keymap::LEFT
                | keymap::RIGHT
                | keymap::ENTER
                | keymap::ESC
                | keymap::SPACE
                | keymap::TAB
        );
        if down && repeats {
            self.repeat = Some((code, Instant::now() + REPEAT_DELAY));
        } else if self.repeat.is_some_and(|(held, _)| held == code) {
            self.repeat = None;
        }
    }

    fn left_stick(&mut self, axis: usize, deflection: f32) {
        let old = self.arrows[axis];
        let new = if deflection.abs() > ARROW_PRESS {
            deflection.signum() as i8
        } else if deflection.abs() < ARROW_RELEASE {
            0
        } else {
            old
        };
        if new == old {
            return;
        }
        self.arrows[axis] = new;
        let code = |dir: i8| match (axis, dir) {
            (0, -1) => keymap::LEFT,
            (0, _) => keymap::RIGHT,
            (_, -1) => keymap::UP,
            _ => keymap::DOWN,
        };
        if old != 0 {
            self.key(code(old), false);
        }
        if new != 0 {
            self.key(code(new), true);
        }
    }

    fn translate(&mut self, event: &Event) {
        match event.u32(0) {
            QUIT => request_stop(),
            kind @ (KEYDOWN | KEYUP) => {
                let value = match (kind, event.u8(13)) {
                    (KEYUP, _) => 0,
                    (_, 0) => 1,
                    _ => 2,
                };
                let scancode = event.u32(16) as i32;
                if let Some(code) = evdev_code(scancode) {
                    self.pending.push_back(KeyEvent { code, value });
                }
            }
            kind @ (CONTROLLER_BUTTON_DOWN | CONTROLLER_BUTTON_UP) => {
                if let Some(code) = button_code(event.u8(12)) {
                    self.key(code, kind == CONTROLLER_BUTTON_DOWN);
                }
            }
            CONTROLLER_AXIS => {
                let value = event.i16(16);
                let deflection = f32::from(value) / 32767.0;
                match event.u8(12) {
                    AXIS_LEFT_X => self.left_stick(0, deflection),
                    AXIS_LEFT_Y => self.left_stick(1, deflection),
                    AXIS_RIGHT_X => self.stick.0 = deflection,
                    AXIS_RIGHT_Y => self.stick.1 = deflection,
                    axis @ (AXIS_TRIGGER_LEFT | AXIS_TRIGGER_RIGHT) => {
                        let i = usize::from(axis - AXIS_TRIGGER_LEFT);
                        let pressed = value >= TRIGGER_PRESSED;
                        if pressed != self.triggers[i] {
                            self.triggers[i] = pressed;
                            let code = [keymap::LEFT_ALT, keymap::BTN_RIGHT][i];
                            self.key(code, pressed);
                        }
                    }
                    _ => {}
                }
            }
            MOUSE_MOTION => {
                self.mouse.0 += event.i32(28);
                self.mouse.1 += event.i32(32);
            }
            kind @ (MOUSE_BUTTON_DOWN | MOUSE_BUTTON_UP) => {
                let code = match event.u8(16) {
                    1 => keymap::BTN_LEFT,
                    3 => keymap::BTN_RIGHT,
                    _ => return,
                };
                self.key(code, kind == MOUSE_BUTTON_DOWN);
            }
            CONTROLLER_ADDED => self.open_controller(event.u32(8) as c_int),
            _ => {}
        }
    }

    fn stick_active(&self) -> bool {
        self.stick.0.abs() > STICK_DEAD_ZONE || self.stick.1.abs() > STICK_DEAD_ZONE
    }
}

/// Controller button to evdev code: D-pad arrows, right face Enter, bottom
/// Esc, left Space, top Tab, L1 Ctrl, Select the on-screen keyboard, Start
/// the r86 menu. R3 and R1 are the left mouse button, L3 the right one (as
/// PortMaster's gptokeyb; triggers are handled as axes).
fn button_code(button: u8) -> Option<u16> {
    Some(match button {
        0 => keymap::ESC,
        1 => keymap::ENTER,
        2 => keymap::SPACE,
        3 => keymap::TAB,
        4 => keymap::OSK,
        6 => keymap::MENU,
        7 => keymap::BTN_RIGHT,
        8 => keymap::BTN_LEFT,
        9 => keymap::LEFT_CTRL,
        10 => keymap::BTN_LEFT,
        11 => keymap::UP,
        12 => keymap::DOWN,
        13 => keymap::LEFT,
        14 => keymap::RIGHT,
        _ => return None,
    })
}

/// Stick deflection to a speed fraction: a dead zone, then a square curve
/// for fine control near the center.
fn stick_speed(deflection: f32) -> f32 {
    let magnitude =
        ((deflection.abs() - STICK_DEAD_ZONE) / (1.0 - STICK_DEAD_ZONE)).clamp(0.0, 1.0);
    magnitude * magnitude * deflection.signum()
}

impl Device for Window {
    fn size(&self) -> (i32, i32) {
        self.size
    }

    fn present(&mut self, canvas: &Canvas) -> Result<(), String> {
        let texture = self.upload_ui(canvas)?;
        // SAFETY: renderer and texture are live.
        unsafe {
            (self.api.render_clear)(self.renderer);
            (self.api.render_copy)(self.renderer, texture, std::ptr::null(), std::ptr::null());
            (self.api.render_present)(self.renderer);
        }
        Ok(())
    }

    fn guest_layer(&self) -> bool {
        self.full_screen
    }

    fn present_frame(
        &mut self,
        rgba: &[u8],
        size: (i32, i32),
        stretch: bool,
        view: Rect,
        ui: &Canvas,
        ui_area: Option<Rect>,
    ) -> Result<(), String> {
        if self.frame.as_ref().is_none_or(|t| t.size != size) {
            if let Some(old) = self.frame.take() {
                // SAFETY: `old` was created by this renderer and is unused now.
                unsafe { (self.api.destroy_texture)(old.raw) };
            }
            let raw =
                self.api
                    .texture(self.renderer, PIXELFORMAT_ABGR8888, size.0, size.1, true)?;
            self.frame = Some(Texture { raw, size });
        }
        let frame = self.frame.as_ref().unwrap().raw;
        // Every standard VGA mode is shown at 4:3.
        let Rect { x, y, w, h } = view;
        let area = if stretch {
            view
        } else if w * 3 > h * 4 {
            Rect {
                x: x + (w - h * 4 / 3) / 2,
                y,
                w: h * 4 / 3,
                h,
            }
        } else {
            Rect {
                x,
                y: y + (h - w * 3 / 4) / 2,
                w,
                h: w * 3 / 4,
            }
        };
        let ui_texture = match ui_area {
            Some(_) => Some(self.upload_ui(ui)?),
            None => None,
        };
        // SAFETY: `rgba` holds size.0 * size.1 RGBA pixels; handles are live.
        unsafe {
            if (self.api.update_texture)(frame, std::ptr::null(), rgba.as_ptr().cast(), size.0 * 4)
                != 0
            {
                return Err(self.api.error("SDL_UpdateTexture"));
            }
            (self.api.render_clear)(self.renderer);
            (self.api.render_copy)(self.renderer, frame, std::ptr::null(), &SdlRect::from(area));
            if let (Some(texture), Some(area)) = (ui_texture, ui_area) {
                let area = SdlRect::from(area);
                (self.api.render_copy)(self.renderer, texture, &area, &area);
            }
            (self.api.render_present)(self.renderer);
        }
        Ok(())
    }

    fn wait(&mut self, timeout_ms: i32) {
        // A deflected stick moves the pointer without producing events, and a
        // held key must wake for its next repeat.
        let mut timeout = if self.stick_active() {
            timeout_ms.min(10)
        } else {
            timeout_ms
        };
        if let Some((_, at)) = self.repeat {
            let until = at.saturating_duration_since(Instant::now()).as_millis();
            timeout = timeout.min(until as i32);
        }
        let mut event = Event([0; 64]);
        // SAFETY: `event` is a writable SDL_Event-sized buffer.
        if self.pending.is_empty()
            && unsafe { (self.api.wait_event_timeout)(&mut event, timeout.max(1)) } != 0
        {
            self.translate(&event);
        }
    }

    fn read(&mut self, out: &mut Vec<KeyEvent>) {
        let mut event = Event([0; 64]);
        // SAFETY: as in wait.
        while unsafe { (self.api.poll_event)(&mut event) } != 0 {
            self.translate(&event);
        }
        let now = Instant::now();
        if let Some((code, at)) = self.repeat
            && now >= at
        {
            self.pending.push_back(KeyEvent { code, value: 2 });
            self.repeat = Some((code, now + REPEAT_INTERVAL));
        }
        out.extend(self.pending.drain(..));
    }

    fn pointer_motion(&mut self) -> (i32, i32) {
        let now = Instant::now();
        // Long gaps (a menu, a slow frame) must not turn into a jump.
        let dt = now
            .duration_since(self.pointer_time)
            .as_secs_f32()
            .min(0.05);
        self.pointer_time = now;
        let x = self.pointer_rest.0 + stick_speed(self.stick.0) * POINTER_SPEED * dt;
        let y = self.pointer_rest.1 + stick_speed(self.stick.1) * POINTER_SPEED * dt;
        self.pointer_rest = (x.fract(), y.fract());
        let (mx, my) = std::mem::take(&mut self.mouse);
        (x.trunc() as i32 + mx, y.trunc() as i32 + my)
    }

    fn menu_keys(&self) -> &'static str {
        if self.full_screen {
            "Start"
        } else {
            "Ctrl+Alt+M"
        }
    }

    fn help(&self) -> &'static [(&'static str, &'static str)] {
        if self.full_screen {
            &[
                ("Right stick", "move the mouse"),
                ("R1 or R3", "left click"),
                ("R2 or L3", "right click"),
                ("Select", "on-screen keyboard"),
                ("Start", "r86 menu"),
            ]
        } else {
            &[
                ("Ctrl+Alt+K", "on-screen keyboard"),
                ("Ctrl+Alt+M", "r86 menu"),
            ]
        }
    }
}

/// USB HID usage (SDL scancode) to Linux evdev key code.
fn evdev_code(usage: i32) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    // Usages 30..=101: 1-0, Enter, Esc, Backspace, Tab, Space, punctuation,
    // Caps Lock, F1-F12, PrintScreen..Pause, navigation, arrows, keypad.
    const MAIN: [u16; 72] = [
        2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, 27, 43, 43, 39, 40, 41, 51,
        52, 53, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104,
        111, 107, 109, 106, 105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, 72,
        73, 82, 83, 86, 127,
    ];
    const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];
    match usage {
        4..=29 => Some(LETTERS[(usage - 4) as usize]),
        30..=101 => Some(MAIN[(usage - 30) as usize]),
        224..=231 => Some(MODIFIERS[(usage - 224) as usize]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdl_scancodes_map_to_evdev_codes() {
        // (USB HID usage, evdev code): A, M, Z, 1, 0, Return, Escape, Space,
        // Slash, F1, F12, Delete, Up, Left, keypad Enter and period, LCtrl, RAlt.
        for (usage, code) in [
            (4, 30),
            (16, 50),
            (29, 44),
            (30, 2),
            (39, 11),
            (40, 28),
            (41, 1),
            (44, 57),
            (56, 53),
            (58, 59),
            (69, 88),
            (76, 111),
            (82, 103),
            (80, 105),
            (88, 96),
            (99, 83),
            (224, 29),
            (230, 100),
        ] {
            assert_eq!(evdev_code(usage), Some(code), "usage {usage}");
        }
    }

    #[test]
    fn stick_has_a_dead_zone_and_full_speed() {
        assert_eq!(stick_speed(0.1), 0.0);
        assert_eq!(stick_speed(-1.0), -1.0);
        assert!(stick_speed(0.5) > 0.0 && stick_speed(0.5) < 0.5);
    }
}
