//! Small helpers over the windows crate shared by the app's windows.

use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowTextW, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW, SetWindowTextW,
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
