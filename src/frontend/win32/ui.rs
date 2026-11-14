//! Small helpers over the windows crate shared by the app's windows.

use std::cell::RefCell;
use std::thread::LocalKey;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DRAW_TEXT_FORMAT, DrawTextW, HDC};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDlgItem, GetSystemMetrics, GetWindowTextW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED,
    LoadImageW, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW, SM_CXICON, SM_CXSMICON, SW_HIDE,
    SW_SHOW, SendMessageW, SetWindowTextW, ShowWindow, WM_SETICON,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

/// This executable's module handle, where its resources live.
pub fn instance() -> HINSTANCE {
    // SAFETY: the process's own module handle; never fails for NULL.
    unsafe { GetModuleHandleW(None) }
        .map(Into::into)
        .unwrap_or_default()
}

/// A resource ID as the name argument of LoadIcon, CreateDialog and others.
pub fn resource(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

pub fn message_box(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    // SAFETY: both strings live across the modal call.
    unsafe { MessageBoxW(Some(owner), &HSTRING::from(text), w!("r86"), style) }
}

pub fn text_of(hwnd: HWND) -> String {
    let mut buffer = [0u16; 1024];
    // SAFETY: the buffer is writable for its length.
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

pub fn set_text(hwnd: HWND, text: &str) {
    // SAFETY: the string lives across the call.
    let _ = unsafe { SetWindowTextW(hwnd, &HSTRING::from(text)) };
}

/// A file chosen in the standard Open dialog. `filter` pairs a description
/// with its pattern, as in "CD images", "*.iso".
pub fn open_file(owner: HWND, title: &str, filter: &[(&str, &str)]) -> Option<String> {
    let mut file = [0u16; 1024];
    let filter: Vec<u16> = filter
        .iter()
        .flat_map(|(name, pattern)| [*name, *pattern])
        .flat_map(|s| s.encode_utf16().chain([0]))
        .chain([0])
        .collect();
    let title = HSTRING::from(title);
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: `ofn` and the buffers it points to outlive the modal call.
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(String::from_utf16_lossy(&file[..len]))
}

/// Runs `handle` on a dialog's state kept in `cell`. Messages that arrive
/// while a handler runs (under a message box or a modal dialog) are left to
/// the dialog manager.
pub fn with<T>(
    cell: &'static LocalKey<RefCell<Option<T>>>,
    handle: impl FnOnce(&mut T) -> bool,
) -> isize {
    cell.with(|cell| match cell.try_borrow_mut() {
        Ok(mut state) => state.as_mut().is_some_and(handle) as isize,
        Err(_) => 0,
    })
}

/// A control of dialog `hwnd`.
pub fn item(hwnd: HWND, id: i32) -> HWND {
    // SAFETY: GetDlgItem accepts any handle and ID.
    unsafe { GetDlgItem(Some(hwnd), id) }.unwrap_or_default()
}

pub fn show(hwnd: HWND, visible: bool) {
    // SAFETY: plain window call.
    let _ = unsafe { ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE }) };
}

pub fn enable(hwnd: HWND, enabled: bool) {
    // SAFETY: plain window call.
    let _ = unsafe { EnableWindow(hwnd, enabled) };
}

/// Sends `msg` with a string argument, as LB_ADDSTRING and CB_ADDSTRING take.
pub fn send_text(hwnd: HWND, msg: u32, text: &str) -> isize {
    let text = HSTRING::from(text);
    // SAFETY: the string lives across the call.
    unsafe { SendMessageW(hwnd, msg, None, Some(LPARAM(text.as_ptr() as isize))) }.0
}

/// The r86 icon in a dialog's title bar and the taskbar (shared icons,
/// never destroyed).
pub fn set_icons(hwnd: HWND) {
    // SAFETY: shared icon handles from this module's resources.
    unsafe {
        for (kind, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let size = GetSystemMetrics(metric);
            if let Ok(icon) = LoadImageW(
                Some(instance()),
                resource(1),
                IMAGE_ICON,
                size,
                size,
                LR_SHARED,
            ) {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(kind as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
        }
    }
}

/// "512 MB" or "2 GB".
pub fn size_text(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GB", mib / 1024)
    } else {
        format!("{mib} MB")
    }
}

pub fn draw_text(dc: HDC, text: &str, rect: &mut RECT, format: DRAW_TEXT_FORMAT) {
    // Wine's DrawTextW never returns for an empty string.
    if text.is_empty() {
        return;
    }
    let mut text: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: the text and rectangle live across the call.
    unsafe { DrawTextW(dc, &mut text, rect, format) };
}
