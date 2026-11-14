//! r86 for Windows (amd64 and arm64): a library of VMs and one window per
//! running VM, drawn with GDI. bios.bin, vgabios-stdvga.bin, dos.zip (the
//! DOS kit) and r86_tools.iso sit beside the executable; VMs live in
//! Documents\r86, and a fatal error's report in Documents\r86\logs
//! (run_log.rs). The dialogs, icon and manifest are resources (r86-win.rc,
//! built by build.rs with MinGW windres).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod audio;
#[cfg(windows)]
#[path = "../catalog.rs"]
#[allow(dead_code)]
mod catalog;
#[cfg(windows)]
#[path = "../disk_files.rs"]
mod disk_files;
#[cfg(windows)]
mod edit_vm;
#[cfg(windows)]
#[path = "../guest_os.rs"]
#[allow(dead_code)]
mod guest_os;
#[cfg(windows)]
mod keymap;
#[cfg(windows)]
#[path = "../keys.rs"]
#[allow(dead_code)]
mod keys;
#[cfg(windows)]
#[path = "../media.rs"]
mod media;
#[cfg(windows)]
#[path = "../protocol.rs"]
#[allow(dead_code)]
mod protocol;
#[cfg(windows)]
#[path = "../run_log.rs"]
mod run_log;
#[cfg(windows)]
#[path = "../runner.rs"]
mod runner;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
#[path = "../vm_store.rs"]
#[allow(dead_code)]
mod vm_store;
#[cfg(windows)]
mod vm_window;
#[cfg(windows)]
mod wizard;
#[cfg(windows)]
#[path = "../ws.rs"]
mod ws;
// Remote management; r86-run's server loop and debugger jobs are unused.
#[cfg(windows)]
#[path = "../ws_frontend.rs"]
#[allow(dead_code)]
mod ws_frontend;

#[cfg(windows)]
fn main() {
    library::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("r86-win is the Windows app; build it for a *-pc-windows-* target.");
    std::process::exit(2);
}

#[cfg(windows)]
use library::library_changed;

#[cfg(windows)]
mod library {
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Shell::{CSIDL_PERSONAL, SHGetFolderPathW, ShellExecuteW};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{HSTRING, PCWSTR, w};

    use super::ui::{instance, item, message_box, resource, set_icons, set_text, size_text, with};
    use super::vm_store::{self, Vm};
    use super::vm_window::{self, WM_APP_DESTROY};
    use super::{edit_vm, wizard};

    // Dialog and control IDs, as in r86-win.rc.
    const IDD_LIBRARY: u16 = 100;
    const ID_LIST: i32 = 10;
    const ID_NEW: i32 = 11;
    const ID_DELETE: i32 = 12;
    const ID_FILES: i32 = 13;
    const ID_DETAILS: i32 = 14;
    const ID_EDIT: i32 = 15;
    const ID_START: i32 = IDOK.0;
    const ID_CANCEL: i32 = IDCANCEL.0;
    const WM_APP_REFRESH: u32 = WM_APP + 2;

    thread_local! {
        static LIBRARY_HWND: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
        static LIBRARY: RefCell<Option<Library>> = const { RefCell::new(None) };
    }

    /// Asks the library to reread the VMs and window states.
    pub fn library_changed() {
        let hwnd = LIBRARY_HWND.get();
        if !hwnd.0.is_null() {
            // SAFETY: posting to a window of this thread.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0)) };
        }
    }

    pub fn run() {
        // SAFETY: a modeless dialog from this module's resources, then the
        // message loop; Tab and Enter work in it through IsDialogMessage.
        unsafe {
            let Ok(hwnd) = CreateDialogParamW(
                Some(instance()),
                resource(IDD_LIBRARY),
                None,
                Some(library_proc),
                LPARAM(0),
            ) else {
                return;
            };
            let _ = ShowWindow(hwnd, SW_SHOW);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                if IsDialogMessageW(hwnd, &msg).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        LIBRARY.with(|library| library.borrow_mut().take());
    }

    struct Library {
        hwnd: HWND,
        root: PathBuf,
        firmware: PathBuf,
        tools_iso: PathBuf,
        vms: Vec<Result<Vm, String>>,
        /// VM windows by directory; closed ones are dropped on refresh.
        windows: Vec<(PathBuf, HWND)>,
    }

    unsafe extern "system" fn library_proc(hwnd: HWND, msg: u32, w: WPARAM, _l: LPARAM) -> isize {
        match msg {
            WM_INITDIALOG => {
                LIBRARY_HWND.set(hwnd);
                set_icons(hwnd);
                let exe = std::env::current_exe().unwrap_or_default();
                let firmware = exe.parent().map(PathBuf::from).unwrap_or_default();
                let mut library = Library {
                    hwnd,
                    root: documents().join("r86"),
                    tools_iso: firmware.join("r86_tools.iso"),
                    firmware,
                    vms: Vec::new(),
                    windows: Vec::new(),
                };
                library.refresh(None);
                LIBRARY.with(|cell| *cell.borrow_mut() = Some(library));
                1
            }
            // Ends the loop even when DestroyWindow runs inside a handler.
            WM_DESTROY => {
                // SAFETY: ends the message loop.
                unsafe { PostQuitMessage(0) };
                1
            }
            _ => with(&LIBRARY, |library| library.handle(msg, w)),
        }
    }

    impl Library {
        fn handle(&mut self, msg: u32, w: WPARAM) -> bool {
            match msg {
                WM_COMMAND => {
                    let (id, code) = ((w.0 & 0xffff) as i32, (w.0 >> 16) as u32);
                    match id {
                        ID_START => self.start(),
                        ID_LIST if code == LBN_DBLCLK => self.start(),
                        ID_LIST if code == LBN_SELCHANGE => self.show_details(),
                        ID_NEW => self.new_vm(),
                        ID_EDIT => self.edit(),
                        ID_DELETE => self.delete(),
                        ID_FILES => self.show_files(),
                        ID_CANCEL => {} // Esc does not quit
                        _ => return false,
                    }
                    true
                }
                WM_APP_REFRESH => {
                    self.refresh(None);
                    true
                }
                WM_CLOSE => {
                    self.quit();
                    true
                }
                _ => false,
            }
        }

        fn item(&self, id: i32) -> HWND {
            item(self.hwnd, id)
        }

        /// Rereads the library, keeping the selection (or selecting `select`).
        fn refresh(&mut self, select: Option<PathBuf>) {
            self.windows.retain(|&(_, hwnd)| is_window(hwnd));
            let selected = select.or_else(|| self.selected().map(|vm| vm.directory.clone()));
            self.vms = vm_store::list(&self.root);
            let list = self.item(ID_LIST);
            // SAFETY: list box messages; each string lives across its call.
            unsafe {
                SendMessageW(list, LB_RESETCONTENT, None, None);
                for vm in &self.vms {
                    let text = match vm {
                        Ok(vm) if self.window_of(&vm.directory).is_some() => {
                            format!("{}  (running)", vm.name)
                        }
                        Ok(vm) => vm.name.clone(),
                        Err(err) => format!("(cannot read) {err}"),
                    };
                    let text = HSTRING::from(text);
                    SendMessageW(
                        list,
                        LB_ADDSTRING,
                        None,
                        Some(LPARAM(text.as_ptr() as isize)),
                    );
                }
                let index = self
                    .vms
                    .iter()
                    .position(|vm| {
                        vm.as_ref()
                            .is_ok_and(|vm| Some(&vm.directory) == selected.as_ref())
                    })
                    .unwrap_or(0);
                SendMessageW(list, LB_SETCURSEL, Some(WPARAM(index)), None);
            }
            self.show_details();
        }

        fn selected(&self) -> Option<&Vm> {
            // SAFETY: list box query.
            let index = unsafe { SendMessageW(self.item(ID_LIST), LB_GETCURSEL, None, None) }.0;
            self.vms.get(usize::try_from(index).ok()?)?.as_ref().ok()
        }

        fn window_of(&self, directory: &Path) -> Option<HWND> {
            self.windows
                .iter()
                .find(|(d, h)| d == directory && is_window(*h))
                .map(|&(_, h)| h)
        }

        fn show_details(&self) {
            let text = match self.selected() {
                None if self.vms.is_empty() => format!(
                    "No virtual machines yet.\n\nClick New... to make one. They are kept in {}.",
                    self.root.display()
                ),
                None => String::new(),
                Some(vm) => {
                    let cd = vm
                        .iso
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map_or("none".into(), |n| n.to_string_lossy());
                    let state = if self.window_of(&vm.directory).is_some() {
                        "Running"
                    } else {
                        "Off"
                    };
                    format!(
                        "{state}\n\n{}\n{} memory\n{} disk\nCD: {cd}",
                        vm.os,
                        size_text(u64::from(vm.memory)),
                        size_text(vm.disk_mib)
                    )
                }
            };
            set_text(self.item(ID_DETAILS), &text);
        }

        fn start(&mut self) {
            let Some(vm) = self.selected() else { return };
            let directory = vm.directory.clone();
            if let Some(hwnd) = self.window_of(&directory) {
                // SAFETY: brings the VM's window forward.
                let _ = unsafe { SetForegroundWindow(hwnd) };
                return;
            }
            match vm_window::open(
                directory.clone(),
                self.firmware.clone(),
                self.tools_iso.clone(),
                self.root.join("logs"),
            ) {
                Ok(hwnd) => {
                    self.windows.push((directory, hwnd));
                    self.refresh(None);
                }
                Err(err) => {
                    let text = format!("Cannot start {}:\n\n{err}", vm_name(&directory));
                    message_box(self.hwnd, &text, MB_OK | MB_ICONERROR);
                }
            }
        }

        fn delete(&mut self) {
            let Some(vm) = self.selected() else { return };
            if self.window_of(&vm.directory).is_some() {
                let text = format!("{} is running. Turn it off first.", vm.name);
                message_box(self.hwnd, &text, MB_OK | MB_ICONINFORMATION);
                return;
            }
            let question = format!(
                "Delete {}?\n\nIts hard disk is erased with everything installed on it. This cannot be undone.",
                vm.name
            );
            if message_box(self.hwnd, &question, MB_YESNO | MB_ICONWARNING) != IDYES {
                return;
            }
            if let Err(err) = vm.delete() {
                message_box(self.hwnd, &err, MB_OK | MB_ICONERROR);
            }
            self.refresh(None);
        }

        fn show_files(&self) {
            let directory = self
                .selected()
                .map_or(self.root.clone(), |vm| vm.directory.clone());
            let _ = std::fs::create_dir_all(&directory);
            let path = HSTRING::from(directory.as_os_str());
            // SAFETY: the strings live across the call.
            unsafe {
                ShellExecuteW(
                    Some(self.hwnd),
                    w!("open"),
                    &path,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOW,
                )
            };
        }

        /// The New VM wizard; the VM it creates is selected and started.
        fn new_vm(&mut self) {
            if let Some(vm) = wizard::run(self.hwnd, &self.root, &self.firmware) {
                self.refresh(Some(vm.directory));
                self.start();
            }
        }

        /// Edit this VM: its settings, while it is off.
        fn edit(&mut self) {
            let Some(vm) = self.selected() else { return };
            if self.window_of(&vm.directory).is_some() {
                let text = format!("{} is running. Turn it off to edit it.", vm.name);
                message_box(self.hwnd, &text, MB_OK | MB_ICONINFORMATION);
                return;
            }
            // Reread, in case another r86 changed it.
            match Vm::load(vm.directory.clone()) {
                Ok(vm) => {
                    if edit_vm::run(self.hwnd, vm) {
                        self.refresh(None);
                    }
                }
                Err(err) => {
                    message_box(self.hwnd, &err, MB_OK | MB_ICONERROR);
                }
            }
        }

        /// Quits after turning off running VMs, each flushing its disks.
        fn quit(&mut self) {
            self.windows.retain(|&(_, hwnd)| is_window(hwnd));
            if !self.windows.is_empty() {
                let question = match self.windows.len() {
                    1 => "A VM is still running. Turn it off and quit? Unsaved work in it is lost."
                        .to_string(),
                    n => format!(
                        "{n} VMs are still running. Turn them off and quit? Unsaved work in them is lost."
                    ),
                };
                if message_box(self.hwnd, &question, MB_YESNO | MB_ICONWARNING) != IDYES {
                    return;
                }
                for &(_, hwnd) in &self.windows {
                    // SAFETY: synchronous, so each VM stops before the loop ends.
                    unsafe { SendMessageW(hwnd, WM_APP_DESTROY, None, None) };
                }
                self.windows.retain(|&(_, hwnd)| is_window(hwnd));
                if !self.windows.is_empty() {
                    message_box(
                        self.hwnd,
                        "Close the open message in each VM window first.",
                        MB_OK | MB_ICONINFORMATION,
                    );
                    return;
                }
            }
            // SAFETY: WM_DESTROY posts WM_QUIT.
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }

    fn is_window(hwnd: HWND) -> bool {
        // SAFETY: IsWindow accepts any handle.
        !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    fn vm_name(directory: &Path) -> String {
        directory
            .file_name()
            .map_or("the VM".into(), |n| n.to_string_lossy().into_owned())
    }

    /// The user's Documents folder, or the profile.
    fn documents() -> PathBuf {
        let mut path = [0u16; 260];
        // SAFETY: the buffer holds MAX_PATH characters as required.
        if unsafe { SHGetFolderPathW(None, CSIDL_PERSONAL as i32, None, 0, &mut path) }.is_ok() {
            let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            return PathBuf::from(String::from_utf16_lossy(&path[..len]));
        }
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default()
    }
}
