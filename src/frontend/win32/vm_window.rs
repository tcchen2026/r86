//! One VM in its own window: the guest screen scaled with GDI, a menu bar
//! like the web console's and a one-line status bar. A click captures the
//! mouse and Ctrl+Alt releases it; while the guest's r86 tools helper runs,
//! the pointer follows the host's 1:1 without capture.
//!
//! GDI objects: the frame goes straight from a reused buffer to the paint DC
//! (StretchDIBits); brushes and the icon are stock, system or shared. The
//! window owns one font, made for its DPI and deleted when replaced or
//! dropped.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use r86::devices::Ps2ButtonState;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_MENU, VK_RETURN,
};
use windows::Win32::UI::Input::*;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

use super::audio::Audio;
use super::keys::{self, InputKey};
use super::runner::{Command, Running, State, lock};
use super::ui::{instance, message_box, open_file, resource};
use super::vm_store::Vm;
use super::{keymap, library_changed};

/// Destroys the window once no handler holds its state (see window_proc).
pub const WM_APP_DESTROY: u32 = WM_APP + 1;
const TIMER_MS: u32 = 15;

const ID_POWER_ON: u32 = 100;
const ID_SHUT_DOWN: u32 = 101;
const ID_RESET: u32 = 102;
const ID_PAUSE: u32 = 103;
const ID_POWER_OFF: u32 = 104;
const ID_INSERT_CD: u32 = 110;
const ID_TOOLS: u32 = 111;
const ID_EJECT: u32 = 112;
const ID_CTRL_ALT_DEL: u32 = 120;
const ID_CTRL_ESC: u32 = 121;
const ID_ALT_TAB: u32 = 122;
const ID_WINDOWS_KEY: u32 = 123;
const ID_RELEASE_KEYS: u32 = 124;
const ID_FIT: u32 = 130;
const ID_ACTUAL: u32 = 131;
const ID_FULL_SCREEN: u32 = 132;

struct VmWindow {
    hwnd: HWND,
    menu: HMENU,
    directory: PathBuf,
    name: String,
    firmware: PathBuf,
    tools_iso: PathBuf,
    running: Option<Running>,
    audio: Option<Audio>,
    /// The latest guest frame as GDI's BGRX.
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    serial: u64,
    state: State,
    ips: u64,
    /// The last message from the VM or the app, shown for a few seconds.
    note: Option<(String, Instant)>,
    error: Option<String>,
    captured: bool,
    absolute: bool,
    buttons: Ps2ButtonState,
    pointer_buttons: u8,
    actual_size: bool,
    /// Window style and rectangle to restore when leaving full screen.
    windowed: Option<(isize, RECT)>,
    dpi: u32,
    /// The status font for `dpi`.
    font: HFONT,
}

impl Drop for VmWindow {
    fn drop(&mut self) {
        // SAFETY: the font is this window's own and no DC selects it now.
        let _ = unsafe { DeleteObject(self.font.into()) };
    }
}

/// Starts the VM in `directory` and opens its window.
pub fn open(directory: PathBuf, firmware: PathBuf, tools_iso: PathBuf) -> Result<HWND, String> {
    let vm = Vm::load(directory.clone())?;
    let name = vm.name.clone();
    let running = Running::start(vm, firmware.clone())?;
    let audio = Audio::start(running.shared.clone());
    let class = w!("r86Vm");
    // SAFETY: plain Win32 window creation; the class and title strings live
    // across the calls, and the window owns the boxed state until WM_DESTROY.
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance(),
            hIcon: LoadIconW(Some(instance()), resource(1)).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // fails harmlessly once registered
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(name.as_str()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance()),
            None,
        )
        .map_err(|err| format!("Cannot create the VM window: {err}"))?;
        let menu = build_menu();
        let _ = SetMenu(hwnd, Some(menu));
        let dpi = GetDpiForWindow(hwnd).max(96);
        let window = VmWindow {
            hwnd,
            menu,
            directory,
            name,
            firmware,
            tools_iso,
            running: Some(running),
            audio: Some(audio),
            pixels: Vec::new(),
            width: 0,
            height: 0,
            serial: 0,
            state: State::Running,
            ips: 0,
            note: None,
            error: None,
            captured: false,
            absolute: false,
            buttons: Ps2ButtonState::default(),
            pointer_buttons: 0,
            actual_size: false,
            windowed: None,
            dpi,
            font: status_font(dpi),
        };
        window.resize_client(window.scale(800), window.scale(600), true);
        let state = Box::into_raw(Box::new(RefCell::new(window)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        DragAcceptFiles(hwnd, true);
        SetTimer(Some(hwnd), 1, TIMER_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        Ok(hwnd)
    }
}

fn build_menu() -> HMENU {
    let items: [(&str, &[(u32, &str)]); 4] = [
        (
            "&Power",
            &[
                (ID_POWER_ON, "Power &on"),
                (ID_SHUT_DOWN, "&Shut down"),
                (ID_RESET, "&Reset"),
                (ID_PAUSE, "&Pause"),
                (0, ""),
                (ID_POWER_OFF, "Power o&ff now..."),
            ],
        ),
        (
            "&CD",
            &[
                (ID_INSERT_CD, "&Insert a CD image..."),
                (ID_TOOLS, "Install r86 &Tools"),
                (0, ""),
                (ID_EJECT, "&Eject"),
            ],
        ),
        (
            "&Keyboard",
            &[
                (ID_CTRL_ALT_DEL, "Ctrl+Alt+&Del"),
                (ID_CTRL_ESC, "Ctrl+&Esc (Start menu)"),
                (ID_ALT_TAB, "Alt+&Tab"),
                (ID_WINDOWS_KEY, "&Windows key"),
                (0, ""),
                (ID_RELEASE_KEYS, "&Release all keys"),
            ],
        ),
        (
            "&View",
            &[
                (ID_FIT, "&Fit to window"),
                (ID_ACTUAL, "&Actual size"),
                (0, ""),
                (ID_FULL_SCREEN, "F&ull screen\tCtrl+Alt+Enter"),
            ],
        ),
    ];
    // SAFETY: plain menu construction; the bar belongs to the window.
    unsafe {
        let bar = CreateMenu().unwrap_or_default();
        for (title, entries) in items {
            let popup = CreatePopupMenu().unwrap_or_default();
            for &(id, text) in entries {
                let _ = if id == 0 {
                    AppendMenuW(popup, MF_SEPARATOR, 0, None)
                } else {
                    AppendMenuW(popup, MF_STRING, id as usize, &HSTRING::from(text))
                };
            }
            let _ = AppendMenuW(bar, MF_POPUP, popup.0 as usize, &HSTRING::from(title));
        }
        bar
    }
}

/// The system's status-bar font at `dpi`; the caller deletes it.
fn status_font(dpi: u32) -> HFONT {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is writable and sized as cbSize says.
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        CreateFontIndirectW(&metrics.lfStatusFont)
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds this window's boxed state or NULL.
    let cell =
        unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<VmWindow>).as_ref() };
    let Some(cell) = cell else {
        // SAFETY: default handling before the state is attached.
        return unsafe { DefWindowProcW(hwnd, msg, w, l) };
    };
    match msg {
        // Posted, so it may arrive inside a handler's modal loop (a message
        // box): destroy only when no handler holds the state, else retry.
        WM_APP_DESTROY => {
            // SAFETY: plain window calls.
            unsafe {
                if cell.try_borrow_mut().is_ok() {
                    let _ = DestroyWindow(hwnd);
                } else {
                    let _ = PostMessageW(Some(hwnd), WM_APP_DESTROY, WPARAM(0), LPARAM(0));
                }
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            if let Ok(mut window) = cell.try_borrow_mut() {
                window.release_mouse();
                if window.windowed.is_some() {
                    // Full screen detached the menu bar, so the window does
                    // not destroy it.
                    // SAFETY: the menu is no longer attached or used.
                    let _ = unsafe { DestroyMenu(window.menu) };
                }
                drop(window);
                // SAFETY: the pointer came from Box::into_raw in open() and no
                // handler borrows it; dropping stops the core thread (which
                // flushes the disks) and the audio thread and frees the font.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(
                        cell as *const RefCell<VmWindow> as *mut RefCell<VmWindow>,
                    ));
                }
            }
            library_changed();
            return LRESULT(0);
        }
        _ => {}
    }
    if let Some(result) = modal(cell, msg, w) {
        return result;
    }
    // Other handlers never wait on the user, so a message arriving while one
    // runs is rare (sent from inside a call it makes) and gets default handling.
    let handled = match cell.try_borrow_mut() {
        Ok(mut window) => window.handle(msg, w, l),
        Err(_) => None,
    };
    // SAFETY: default handling.
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, w, l) })
}

/// The commands that ask the user (message boxes, the Open dialog). They
/// borrow the state only around the dialog, so the VM keeps drawing under
/// it. Closing is always handled here: Windows' default would destroy the
/// window and leave its VM running.
fn modal(cell: &RefCell<VmWindow>, msg: u32, w: WPARAM) -> Option<LRESULT> {
    let id = (w.0 & 0xffff) as u32;
    let done = Some(LRESULT(0));
    if msg != WM_CLOSE
        && !(msg == WM_COMMAND && matches!(id, ID_POWER_ON | ID_POWER_OFF | ID_INSERT_CD))
    {
        return None;
    }
    let Ok(window) = cell.try_borrow() else {
        return done;
    };
    let (hwnd, name, on) = (window.hwnd, window.name.clone(), window.on());
    drop(window);
    match (msg, id) {
        (WM_CLOSE, _) => {
            let question = format!(
                "{name} is still running. Turn it off now? Unsaved work in it is lost.\n\n\
                 To keep it, choose No and shut it down from Power > Shut down."
            );
            if !on || message_box(hwnd, &question, MB_YESNO | MB_ICONWARNING) == IDYES {
                // SAFETY: posted; destroyed once no handler holds the state.
                let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_DESTROY, WPARAM(0), LPARAM(0)) };
            }
        }
        (_, ID_POWER_OFF) => {
            let question =
                format!("Turn {name} off now? Like pulling the plug, unsaved work in it is lost.");
            if message_box(hwnd, &question, MB_YESNO | MB_ICONWARNING) == IDYES
                && let Ok(mut window) = cell.try_borrow_mut()
            {
                window.power_down();
            }
        }
        (_, ID_INSERT_CD) => {
            let filter = [("CD images (*.iso)", "*.iso"), ("All files", "*.*")];
            if let Some(path) = open_file(hwnd, "Insert a CD image", &filter)
                && let Ok(window) = cell.try_borrow()
            {
                window.send(Command::Cd(Some(path.into())));
            }
        }
        _ => {
            let started = cell
                .try_borrow_mut()
                .map_or(Ok(()), |mut window| window.power_on());
            if let Err(err) = started {
                message_box(hwnd, &err, MB_OK | MB_ICONERROR);
            }
        }
    }
    done
}

impl VmWindow {
    fn handle(&mut self, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TIMER => self.poll(),
            WM_PAINT => self.paint(),
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_SIZE => {
                self.clip_mouse();
                self.invalidate();
            }
            WM_DPICHANGED => self.dpi_changed((w.0 >> 16) as u32, l),
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => self.key(msg, w, l),
            WM_KILLFOCUS => {
                self.release_mouse();
                self.send(Command::ReleaseInput);
            }
            WM_INPUT => self.raw_input(l),
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP => self.mouse(msg, w, l),
            WM_MOUSEWHEEL => self.wheel(w),
            WM_INITMENUPOPUP => self.update_menu(),
            WM_COMMAND => self.command((w.0 & 0xffff) as u32),
            WM_DROPFILES => self.drop_files(HDROP(w.0 as *mut _)),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn send(&self, command: Command) {
        if let Some(running) = &self.running {
            running.send(command);
        }
    }

    fn on(&self) -> bool {
        self.running.is_some() && matches!(self.state, State::Running | State::Paused)
    }

    fn invalidate(&self) {
        // SAFETY: repaint request for this window.
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn set_note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), Instant::now()));
        self.invalidate();
    }

    fn scale(&self, value: i32) -> i32 {
        value * self.dpi as i32 / 96
    }

    fn status_height(&self) -> i32 {
        if self.windowed.is_some() {
            0
        } else {
            self.scale(22)
        }
    }

    /// Copies a new frame and status from the core thread.
    fn poll(&mut self) {
        let Some(running) = &self.running else { return };
        let shared = running.shared.clone();
        let mut changed = false;
        let mut resized = false;
        {
            let frame = lock(&shared.frame);
            if frame.serial != self.serial && frame.width > 0 {
                resized = (frame.width, frame.height) != (self.width, self.height);
                self.serial = frame.serial;
                self.width = frame.width;
                self.height = frame.height;
                self.pixels.resize(frame.rgba.len(), 0);
                for (out, rgba) in self
                    .pixels
                    .chunks_exact_mut(4)
                    .zip(frame.rgba.chunks_exact(4))
                {
                    out[0] = rgba[2];
                    out[1] = rgba[1];
                    out[2] = rgba[0];
                }
                changed = true;
            }
        }
        if resized && self.actual_size {
            self.fit_window();
        }
        let (state, ips, message) = {
            let mut status = lock(&shared.status);
            let message = status.messages.pop_back();
            status.messages.clear();
            (status.state, status.ips, message)
        };
        if let Some(message) = message {
            if state == State::Failed {
                self.error = Some(message.clone());
            }
            self.set_note(message);
        }
        if state != self.state || ips != self.ips {
            self.state = state;
            self.ips = ips;
            changed = true;
        }
        if matches!(state, State::Off | State::Failed) {
            self.power_down();
        }
        let absolute = shared.pointer.load(Ordering::Relaxed);
        if absolute != self.absolute {
            self.absolute = absolute;
            if absolute {
                self.release_mouse();
            }
            changed = true;
        }
        if self
            .note
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            self.note = None;
            changed = true;
        }
        if changed {
            self.invalidate();
        }
    }

    /// The VM stopped: free the core and audio, keep the window.
    fn power_down(&mut self) {
        self.release_mouse();
        self.audio = None;
        self.running = None;
        self.absolute = false;
        self.invalidate();
        library_changed();
    }

    fn power_on(&mut self) -> Result<(), String> {
        if self.running.is_some() {
            return Ok(());
        }
        let vm = Vm::load(self.directory.clone())?;
        let running = Running::start(vm, self.firmware.clone())?;
        self.audio = Some(Audio::start(running.shared.clone()));
        self.running = Some(running);
        self.state = State::Running;
        self.error = None;
        self.serial = 0;
        self.width = 0;
        self.invalidate();
        library_changed();
        Ok(())
    }

    /// The client area for the guest screen and the frame's place in it.
    fn layout(&self) -> (RECT, RECT) {
        let mut client = RECT::default();
        // SAFETY: plain query of this window.
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };
        let screen = RECT {
            bottom: (client.bottom - self.status_height()).max(0),
            ..client
        };
        let (sw, sh) = (screen.right - screen.left, screen.bottom - screen.top);
        if self.width == 0 || sw <= 0 || sh <= 0 {
            return (screen, screen);
        }
        let (dw, dh) = display_size(self.width, self.height);
        let (w, h) = if self.actual_size {
            (dw as i32, dh as i32)
        } else if sw as u64 * dh as u64 > sh as u64 * dw as u64 {
            ((sh as u64 * dw as u64 / dh as u64) as i32, sh)
        } else {
            (sw, (sw as u64 * dh as u64 / dw as u64) as i32)
        };
        let x = screen.left + (sw - w) / 2;
        let y = screen.top + (sh - h) / 2;
        (
            screen,
            RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
        )
    }

    fn paint(&mut self) {
        let (screen, frame) = self.layout();
        // SAFETY: drawing into this window's paint DC with buffers that live
        // across each call; only stock, system and shared objects plus the
        // window's own font, which is deselected before EndPaint.
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(self.hwnd, &mut ps);
            let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
            let old_font = SelectObject(dc, self.font.into());
            SetBkMode(dc, TRANSPARENT);
            if self.on() && self.width > 0 {
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: self.width as i32,
                        biHeight: -(self.height as i32), // top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetStretchBltMode(dc, COLORONCOLOR);
                StretchDIBits(
                    dc,
                    frame.left,
                    frame.top,
                    frame.right - frame.left,
                    frame.bottom - frame.top,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(self.pixels.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                for band in [
                    RECT {
                        bottom: frame.top,
                        ..screen
                    },
                    RECT {
                        top: frame.bottom,
                        ..screen
                    },
                    RECT {
                        top: frame.top,
                        bottom: frame.bottom,
                        right: frame.left,
                        ..screen
                    },
                    RECT {
                        top: frame.top,
                        bottom: frame.bottom,
                        left: frame.right,
                        ..screen
                    },
                ] {
                    FillRect(dc, &band, black);
                }
            } else {
                FillRect(dc, &screen, black);
                self.paint_off(dc, screen);
            }
            if self.windowed.is_none() {
                let mut client = RECT::default();
                let _ = GetClientRect(self.hwnd, &mut client);
                let bar = RECT {
                    top: screen.bottom,
                    ..client
                };
                FillRect(dc, &bar, GetSysColorBrush(COLOR_BTNFACE));
                SetTextColor(dc, COLORREF(GetSysColor(COLOR_BTNTEXT)));
                let mut text = RECT {
                    left: bar.left + self.scale(8),
                    right: bar.right - self.scale(8),
                    ..bar
                };
                draw_text(
                    dc,
                    &self.status_text(),
                    &mut text,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
            SelectObject(dc, old_font);
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    /// The r86 icon, the state and a hint, centred in the black screen.
    fn paint_off(&self, dc: HDC, screen: RECT) {
        let (title, hint) = match (&self.error, self.running.is_some()) {
            (Some(error), _) => ("Stopped".to_string(), error.clone()),
            (None, false) => (
                "Powered off".to_string(),
                "Power > Power on starts it again.".to_string(),
            ),
            (None, true) => ("Starting".to_string(), String::new()),
        };
        let icon = self.scale(64);
        let cx = (screen.left + screen.right) / 2;
        let top = (screen.top + screen.bottom) / 2 - icon;
        // SAFETY: a shared icon (never destroyed) drawn into the paint DC.
        unsafe {
            if let Ok(handle) = LoadImageW(
                Some(instance()),
                resource(1),
                IMAGE_ICON,
                icon,
                icon,
                LR_SHARED,
            ) {
                let _ = DrawIconEx(
                    dc,
                    cx - icon / 2,
                    top,
                    HICON(handle.0),
                    icon,
                    icon,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            let line = self.scale(20);
            SetTextColor(dc, COLORREF(0x00e0_e0e0));
            let mut rect = RECT {
                left: screen.left,
                right: screen.right,
                top: top + icon + self.scale(8),
                bottom: 0,
            };
            rect.bottom = rect.top + line;
            draw_text(
                dc,
                &title,
                &mut rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
            SetTextColor(dc, COLORREF(0x0090_9090));
            rect.top += line;
            rect.bottom += line;
            draw_text(
                dc,
                &hint,
                &mut rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }
    }

    fn status_text(&self) -> String {
        let state = match (self.running.is_some(), self.state) {
            (false, _) | (_, State::Off) => "Powered off".to_string(),
            (_, State::Failed) => "Stopped".to_string(),
            (_, State::Paused) => "Paused".to_string(),
            (_, State::Running) if self.ips >= 1_000_000 => {
                format!("Running, {} MIPS", self.ips / 1_000_000)
            }
            (_, State::Running) => "Running".to_string(),
        };
        let hint = if let Some((note, _)) = &self.note {
            note.as_str()
        } else if !self.on() {
            ""
        } else if self.captured {
            "Press Ctrl+Alt to release the mouse"
        } else if self.absolute {
            "The mouse follows yours (r86 tools)"
        } else {
            "Click the screen to use the mouse"
        };
        if hint.is_empty() {
            state
        } else {
            format!("{state}    {hint}")
        }
    }

    fn dpi_changed(&mut self, dpi: u32, l: LPARAM) {
        self.dpi = dpi.max(96);
        let font = status_font(self.dpi);
        // SAFETY: the old font is deselected (painting is over); `l` points
        // to the suggested window rectangle for the new DPI.
        unsafe {
            let _ = DeleteObject(std::mem::replace(&mut self.font, font).into());
            let r = &*(l.0 as *const RECT);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER,
            );
        }
        self.invalidate();
    }

    fn key(&mut self, msg: u32, w: WPARAM, l: LPARAM) {
        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        // SAFETY: keyboard state queries.
        let ctrl_alt =
            unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 && GetKeyState(VK_MENU.0 as i32) < 0 };
        if down && ctrl_alt && w.0 == VK_RETURN.0 as usize {
            self.toggle_full_screen();
            return;
        }
        if let Some(key) = keymap::key(l.0) {
            self.send(Command::Key(key, down));
        }
        if down && ctrl_alt && self.captured {
            self.release_mouse();
        }
    }

    fn capture_mouse(&mut self) {
        if self.captured || !self.on() {
            return;
        }
        self.captured = true;
        // Raw mouse input has one target per process, so only the capturing
        // window registers, until release_mouse removes it.
        raw_mouse(self.hwnd, RAWINPUTDEVICE_FLAGS(0));
        // SAFETY: cursor visibility is balanced by release_mouse.
        unsafe { ShowCursor(false) };
        self.clip_mouse();
        self.invalidate();
    }

    fn clip_mouse(&self) {
        if !self.captured {
            return;
        }
        let (_, frame) = self.layout();
        let mut corners = [
            POINT {
                x: frame.left,
                y: frame.top,
            },
            POINT {
                x: frame.right,
                y: frame.bottom,
            },
        ];
        // SAFETY: converting points of this window and clipping the cursor.
        unsafe {
            for point in &mut corners {
                let _ = ClientToScreen(self.hwnd, point);
            }
            let rect = RECT {
                left: corners[0].x,
                top: corners[0].y,
                right: corners[1].x,
                bottom: corners[1].y,
            };
            let _ = ClipCursor(Some(&rect));
        }
    }

    fn release_mouse(&mut self) {
        if !self.captured {
            return;
        }
        self.captured = false;
        self.buttons = Ps2ButtonState::default();
        self.send(Command::Mouse(0, 0, self.buttons));
        raw_mouse(HWND::default(), RIDEV_REMOVE);
        // SAFETY: undoes capture_mouse.
        unsafe {
            let _ = ClipCursor(None);
            ShowCursor(true);
        }
        self.invalidate();
    }

    fn raw_input(&mut self, l: LPARAM) {
        if !self.captured {
            return;
        }
        let mut input = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: RAWINPUT is plain data; at most `size` bytes are written.
        let read = unsafe {
            GetRawInputData(
                HRAWINPUT(l.0 as *mut _),
                RID_INPUT,
                Some((&mut input as *mut RAWINPUT).cast()),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || input.header.dwType != RIM_TYPEMOUSE.0 {
            return;
        }
        // SAFETY: a mouse record, as dwType says.
        let mouse = unsafe { input.data.mouse };
        // SAFETY: the button-flags view of the union.
        let flags = u32::from(unsafe { mouse.Anonymous.Anonymous.usButtonFlags });
        let b = &mut self.buttons;
        for (down, up, button) in [
            (
                RI_MOUSE_LEFT_BUTTON_DOWN,
                RI_MOUSE_LEFT_BUTTON_UP,
                &mut b.left,
            ),
            (
                RI_MOUSE_RIGHT_BUTTON_DOWN,
                RI_MOUSE_RIGHT_BUTTON_UP,
                &mut b.right,
            ),
            (
                RI_MOUSE_MIDDLE_BUTTON_DOWN,
                RI_MOUSE_MIDDLE_BUTTON_UP,
                &mut b.middle,
            ),
        ] {
            if flags & down != 0 {
                *button = true;
            }
            if flags & up != 0 {
                *button = false;
            }
        }
        // Remote desktops send absolute positions; only relative motion counts.
        let (dx, dy) = if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0 {
            (mouse.lLastX, mouse.lLastY)
        } else {
            (0, 0)
        };
        if dx != 0 || dy != 0 || flags != 0 {
            self.send(Command::Mouse(dx, dy, self.buttons));
        }
    }

    fn mouse(&mut self, msg: u32, w: WPARAM, l: LPARAM) {
        if !self.on() || self.captured {
            return;
        }
        if !self.absolute {
            if msg == WM_LBUTTONDOWN {
                self.capture_mouse();
            }
            return;
        }
        let keys = w.0 as u32;
        let buttons = u8::from(keys & 0x01 != 0)
            | u8::from(keys & 0x02 != 0) << 1
            | u8::from(keys & 0x10 != 0) << 2;
        // SAFETY: mouse capture keeps drags that leave the window.
        unsafe {
            if buttons != 0 && self.pointer_buttons == 0 {
                SetCapture(self.hwnd);
            } else if buttons == 0 && self.pointer_buttons != 0 {
                let _ = ReleaseCapture();
            }
        }
        self.pointer_buttons = buttons;
        let (x, y) = (
            (l.0 & 0xffff) as i16 as i32,
            (l.0 >> 16 & 0xffff) as i16 as i32,
        );
        self.pointer(x, y, buttons, 0);
    }

    fn pointer(&mut self, x: i32, y: i32, buttons: u8, wheel: i16) {
        let (_, frame) = self.layout();
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        if w < 2 || h < 2 {
            return;
        }
        let scale = |v: i32, origin: i32, size: i32| {
            ((v - origin).clamp(0, size - 1) as i64 * 65535 / (size - 1) as i64) as u16
        };
        self.send(Command::Pointer(
            scale(x, frame.left, w),
            scale(y, frame.top, h),
            buttons,
            wheel,
        ));
    }

    fn wheel(&mut self, w: WPARAM) {
        if !self.on() || !self.absolute {
            return;
        }
        let steps = -((w.0 >> 16) as u16 as i16 / WHEEL_DELTA as i16);
        if steps != 0 {
            let mut point = POINT::default();
            // SAFETY: the cursor position in this window's client coordinates.
            unsafe {
                let _ = GetCursorPos(&mut point);
                let _ = ScreenToClient(self.hwnd, &mut point);
            }
            self.pointer(point.x, point.y, self.pointer_buttons, steps);
        }
    }

    fn update_menu(&self) {
        let on = self.on();
        let enable = |id: u32, enabled: bool| {
            // SAFETY: plain menu state change.
            let _ = unsafe {
                EnableMenuItem(self.menu, id, if enabled { MF_ENABLED } else { MF_GRAYED })
            };
        };
        enable(ID_POWER_ON, self.running.is_none());
        for id in [
            ID_SHUT_DOWN,
            ID_RESET,
            ID_PAUSE,
            ID_POWER_OFF,
            ID_INSERT_CD,
            ID_EJECT,
        ] {
            enable(id, on);
        }
        enable(ID_TOOLS, on && self.tools_iso.is_file());
        for id in [
            ID_CTRL_ALT_DEL,
            ID_CTRL_ESC,
            ID_ALT_TAB,
            ID_WINDOWS_KEY,
            ID_RELEASE_KEYS,
        ] {
            enable(id, on && self.state == State::Running);
        }
        let check = |id: u32, checked: bool| {
            // SAFETY: plain menu state change.
            unsafe {
                CheckMenuItem(
                    self.menu,
                    id,
                    if checked {
                        MF_CHECKED.0
                    } else {
                        MF_UNCHECKED.0
                    },
                )
            };
        };
        check(ID_PAUSE, self.state == State::Paused);
        check(ID_FIT, !self.actual_size);
        check(ID_ACTUAL, self.actual_size);
        check(ID_FULL_SCREEN, self.windowed.is_some());
    }

    fn command(&mut self, id: u32) {
        match id {
            ID_SHUT_DOWN => self.send(Command::PowerButton),
            ID_RESET => self.send(Command::Reset),
            ID_PAUSE => self.send(Command::Pause(self.state != State::Paused)),
            ID_TOOLS => {
                self.send(Command::Cd(Some(self.tools_iso.clone())));
                self.set_note(
                    "r86 tools CD inserted: Windows opens its installer, or run R86-TOOL.EXE on it",
                );
            }
            ID_EJECT => self.send(Command::Cd(None)),
            ID_CTRL_ALT_DEL => self.send(Command::CtrlAltDel),
            ID_CTRL_ESC => self.press(&["controlleft", "escape"]),
            ID_ALT_TAB => self.press(&["altleft", "tab"]),
            ID_WINDOWS_KEY => self.press(&["metaleft"]),
            ID_RELEASE_KEYS => self.send(Command::ReleaseInput),
            ID_FIT => {
                self.actual_size = false;
                self.invalidate();
            }
            ID_ACTUAL => {
                self.actual_size = true;
                self.fit_window();
            }
            ID_FULL_SCREEN => self.toggle_full_screen(),
            _ => {}
        }
    }

    /// Presses keys in order and releases them in reverse.
    fn press(&self, names: &[&str]) {
        let keys: Vec<InputKey> = names
            .iter()
            .filter_map(|name| keys::parse_named_key(name).ok())
            .collect();
        for &key in &keys {
            self.send(Command::Key(key, true));
        }
        for &key in keys.iter().rev() {
            self.send(Command::Key(key, false));
        }
    }

    /// Sizes the window for a `width` x `height` screen area (device pixels)
    /// plus the status bar, kept on its monitor's work area; `centre` also
    /// centres it there.
    fn resize_client(&self, width: i32, height: i32, centre: bool) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height + self.status_height(),
        };
        // SAFETY: placement queries and changes of this window.
        unsafe {
            let _ = AdjustWindowRectExForDpi(
                &mut rect,
                WS_OVERLAPPEDWINDOW,
                true,
                WINDOW_EX_STYLE(0),
                self.dpi,
            );
            let mut monitor = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(
                MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            );
            let work = monitor.rcWork;
            let w = (rect.right - rect.left).min(work.right - work.left);
            let h = (rect.bottom - rect.top).min(work.bottom - work.top);
            let mut window = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut window);
            let (x, y) = if centre {
                (
                    work.left + (work.right - work.left - w) / 2,
                    work.top + (work.bottom - work.top - h) / 2,
                )
            } else {
                (
                    window.left.clamp(work.left, work.right - w),
                    window.top.clamp(work.top, work.bottom - h),
                )
            };
            let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER);
        }
    }

    /// Resizes the window so the guest screen shows at 1:1.
    fn fit_window(&self) {
        if self.width == 0 || self.windowed.is_some() {
            return;
        }
        let (w, h) = display_size(self.width, self.height);
        self.resize_client(w as i32, h as i32, false);
        self.invalidate();
    }

    fn toggle_full_screen(&mut self) {
        // SAFETY: style and placement changes of this window.
        unsafe {
            match self.windowed.take() {
                Some((style, rect)) => {
                    self.note = None;
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, style);
                    let _ = SetMenu(self.hwnd, Some(self.menu));
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_FRAMECHANGED,
                    );
                }
                None => {
                    let style = GetWindowLongPtrW(self.hwnd, GWL_STYLE);
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(self.hwnd, &mut rect);
                    let mut monitor = MONITORINFO {
                        cbSize: size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    let _ = GetMonitorInfoW(
                        MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST),
                        &mut monitor,
                    );
                    self.windowed = Some((style, rect));
                    let m = monitor.rcMonitor;
                    let _ = SetMenu(self.hwnd, None);
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        m.left,
                        m.top,
                        m.right - m.left,
                        m.bottom - m.top,
                        SWP_NOZORDER | SWP_FRAMECHANGED,
                    );
                    self.set_note("Ctrl+Alt+Enter leaves full screen");
                }
            }
        }
        self.clip_mouse();
        self.invalidate();
    }

    fn drop_files(&mut self, drop: HDROP) {
        let mut name = [0u16; 1024];
        // SAFETY: the drop handle is valid until DragFinish.
        let path = unsafe {
            let len = DragQueryFileW(drop, 0, Some(&mut name)) as usize;
            DragFinish(drop);
            String::from_utf16_lossy(&name[..len.min(name.len())])
        };
        if !self.on() {
            return;
        }
        if path.to_ascii_lowercase().ends_with(".iso") {
            self.send(Command::Cd(Some(path.into())));
        } else {
            self.set_note("Drop a CD image (.iso) to insert it");
        }
    }
}

fn raw_mouse(target: HWND, flags: RAWINPUTDEVICE_FLAGS) {
    let mouse = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: flags,
        hwndTarget: target,
    };
    // SAFETY: one plain device record.
    let _ = unsafe { RegisterRawInputDevices(&[mouse], size_of::<RAWINPUTDEVICE>() as u32) };
}

fn draw_text(dc: HDC, text: &str, rect: &mut RECT, format: DRAW_TEXT_FORMAT) {
    // Wine's DrawTextW never returns for an empty string.
    if text.is_empty() {
        return;
    }
    let mut text: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: the text and rectangle live across the call.
    unsafe { DrawTextW(dc, &mut text, rect, format) };
}

/// The guest frame's displayed size: standard VGA modes at 4:3, like a
/// monitor; other modes with square pixels.
fn display_size(width: u32, height: u32) -> (u32, u32) {
    if matches!(width, 320 | 360 | 640 | 720) && matches!(height, 200 | 350 | 400) {
        (width, width * 3 / 4)
    } else {
        (width, height)
    }
}
