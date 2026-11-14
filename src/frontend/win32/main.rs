//! r86 for Windows (amd64 and arm64): a library of VMs and one window per
//! running VM, drawn with GDI. bios.bin, vgabios-stdvga.bin and
//! r86_tools.iso sit beside the executable; VMs live in Documents\r86.
//! The dialogs, icon and manifest are resources (r86-win.rc, built by
//! build.rs with MinGW windres).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod audio;
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
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::Shell::{CSIDL_PERSONAL, SHGetFolderPathW, ShellExecuteW};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{HSTRING, PCWSTR, w};

    use super::guest_os::{self, PRESETS};
    use super::ui::{instance, message_box, open_file, resource, set_text, text_of};
    use super::vm_store::{self, Vm};
    use super::vm_window::{self, WM_APP_DESTROY};

    // Dialog and control IDs, as in r86-win.rc.
    const IDD_LIBRARY: u16 = 100;
    const IDD_NEW_VM: u16 = 101;
    const ID_LIST: i32 = 10;
    const ID_NEW: i32 = 11;
    const ID_DELETE: i32 = 12;
    const ID_FILES: i32 = 13;
    const ID_DETAILS: i32 = 14;
    const ID_NAME: i32 = 20;
    const ID_OS: i32 = 21;
    const ID_CD: i32 = 22;
    const ID_BROWSE: i32 = 23;
    const ID_MEMORY: i32 = 24;
    const ID_DISK: i32 = 25;
    const ID_OK: i32 = IDOK.0; // the library's Start, the New VM window's Create
    const ID_CANCEL: i32 = IDCANCEL.0;
    const WM_APP_REFRESH: u32 = WM_APP + 2;

    thread_local! {
        static LIBRARY_HWND: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
        static LIBRARY: RefCell<Option<Library>> = const { RefCell::new(None) };
        static NEW_VM: RefCell<Option<NewVm>> = const { RefCell::new(None) };
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

    /// Runs `handle` on a dialog's state. Messages that arrive while a
    /// handler runs (under a message box or the modal New VM window) are
    /// left to the dialog manager.
    fn with<T>(
        cell: &'static std::thread::LocalKey<RefCell<Option<T>>>,
        handle: impl FnOnce(&mut T) -> bool,
    ) -> isize {
        cell.with(|cell| match cell.try_borrow_mut() {
            Ok(mut state) => state.as_mut().is_some_and(handle) as isize,
            Err(_) => 0,
        })
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
                        ID_OK => self.start(),
                        ID_LIST if code == LBN_DBLCLK => self.start(),
                        ID_LIST if code == LBN_SELCHANGE => self.show_details(),
                        ID_NEW => self.new_vm(),
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
            // SAFETY: a control of this dialog.
            unsafe { GetDlgItem(Some(self.hwnd), id) }.unwrap_or_default()
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
                        "{state}\n\n{}\n{} MB memory\n{} disk\nCD: {cd}",
                        vm.os,
                        vm.memory,
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

        /// The modal New VM window; a created VM is selected and, with a CD
        /// image to install from, started.
        fn new_vm(&mut self) {
            NEW_VM.with(|cell| {
                *cell.borrow_mut() = Some(NewVm {
                    hwnd: HWND::default(),
                    root: self.root.clone(),
                    suggested: String::new(),
                })
            });
            // SAFETY: a modal dialog from this module's resources.
            let created = unsafe {
                DialogBoxParamW(
                    Some(instance()),
                    resource(IDD_NEW_VM),
                    Some(self.hwnd),
                    Some(new_vm_proc),
                    LPARAM(0),
                )
            };
            let vm = NEW_VM
                .with(|cell| cell.borrow_mut().take())
                .and(CREATED.with(|c| c.borrow_mut().take()));
            if created != 1 {
                return;
            }
            if let Some(vm) = vm {
                self.refresh(Some(vm.directory.clone()));
                if vm.iso.is_some() {
                    self.start();
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

    thread_local! {
        /// The VM the New VM window made, for Library::new_vm.
        static CREATED: RefCell<Option<Vm>> = const { RefCell::new(None) };
    }

    struct NewVm {
        hwnd: HWND,
        root: PathBuf,
        /// The name the app last suggested; replaced while unedited.
        suggested: String,
    }

    unsafe extern "system" fn new_vm_proc(hwnd: HWND, msg: u32, w: WPARAM, _l: LPARAM) -> isize {
        if msg == WM_INITDIALOG {
            set_icons(hwnd);
            return with(&NEW_VM, |dialog| {
                dialog.hwnd = hwnd;
                let os = dialog.item(ID_OS);
                for preset in PRESETS {
                    let name = HSTRING::from(preset.name);
                    // SAFETY: the item text lives across the call.
                    unsafe {
                        SendMessageW(os, CB_ADDSTRING, None, Some(LPARAM(name.as_ptr() as isize)))
                    };
                }
                dialog.choose_preset(0);
                true
            });
        }
        with(&NEW_VM, |dialog| dialog.handle(msg, w))
    }

    impl NewVm {
        fn item(&self, id: i32) -> HWND {
            // SAFETY: a control of this dialog.
            unsafe { GetDlgItem(Some(self.hwnd), id) }.unwrap_or_default()
        }

        fn handle(&mut self, msg: u32, w: WPARAM) -> bool {
            if msg != WM_COMMAND {
                return false;
            }
            let (id, code) = ((w.0 & 0xffff) as i32, (w.0 >> 16) as u32);
            match id {
                ID_OK => self.create(),
                // SAFETY: closes this modal dialog.
                ID_CANCEL => unsafe {
                    let _ = EndDialog(self.hwnd, 0);
                },
                ID_BROWSE => self.browse(),
                ID_OS if code == CBN_SELCHANGE => {
                    // SAFETY: combo box query.
                    let index =
                        unsafe { SendMessageW(self.item(ID_OS), CB_GETCURSEL, None, None) }.0;
                    self.choose_preset(index.max(0) as usize);
                }
                _ => return false,
            }
            true
        }

        /// Selects a preset: its memory and disk, and a matching name while
        /// the user has not typed one.
        fn choose_preset(&mut self, index: usize) {
            let index = index.min(PRESETS.len() - 1);
            let preset = &PRESETS[index];
            set_text(self.item(ID_MEMORY), &preset.memory_mib.to_string());
            set_text(self.item(ID_DISK), &preset.disk_mib.to_string());
            // SAFETY: combo box selection.
            unsafe { SendMessageW(self.item(ID_OS), CB_SETCURSEL, Some(WPARAM(index)), None) };
            let name = text_of(self.item(ID_NAME));
            if name.is_empty() || name == self.suggested {
                self.suggested = vm_store::suggested_name(&self.root, preset.name);
                set_text(self.item(ID_NAME), &self.suggested);
            }
        }

        fn browse(&mut self) {
            let filter = [("CD images (*.iso)", "*.iso"), ("All files", "*.*")];
            let Some(path) = open_file(self.hwnd, "Choose a CD image", &filter) else {
                return;
            };
            set_text(self.item(ID_CD), &path);
            if let Ok(Some(preset)) = guest_os::detect_iso(path.as_ref())
                && let Some(index) = PRESETS.iter().position(|p| p.name == preset.name)
            {
                self.choose_preset(index);
            }
        }

        fn create(&mut self) {
            let name = text_of(self.item(ID_NAME)).trim().to_string();
            let memory = text_of(self.item(ID_MEMORY))
                .parse::<u32>()
                .ok()
                .filter(|m| (4..=3584).contains(m));
            let disk = text_of(self.item(ID_DISK))
                .parse::<u64>()
                .ok()
                .filter(|d| (16..=131_072).contains(d));
            let cd = text_of(self.item(ID_CD)).trim().to_string();
            let (focus, problem) = if name.is_empty()
                || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
            {
                (ID_NAME, "Give the VM a name without \\ / : * ? \" < > |.")
            } else if memory.is_none() {
                (ID_MEMORY, "Memory must be 4 to 3584 MB.")
            } else if disk.is_none() {
                (ID_DISK, "The disk must be 16 MB to 128 GB.")
            } else {
                (0, "")
            };
            if focus != 0 {
                message_box(self.hwnd, problem, MB_OK | MB_ICONWARNING);
                // SAFETY: focus a control of this dialog.
                let _ = unsafe { SetFocus(Some(self.item(focus))) };
                return;
            }
            // SAFETY: combo box query.
            let index = unsafe { SendMessageW(self.item(ID_OS), CB_GETCURSEL, None, None) }
                .0
                .max(0) as usize;
            let preset = &PRESETS[index.min(PRESETS.len() - 1)];
            let mut vm = Vm::new(&self.root, name, preset);
            vm.engine = "auto".into();
            vm.memory = memory.unwrap_or(preset.memory_mib);
            vm.disk_mib = disk.unwrap_or(preset.disk_mib);
            if !cd.is_empty() {
                vm.iso = Some(cd.into());
                vm.boot_order = "cdrom".into();
            }
            let created = std::fs::create_dir_all(&self.root)
                .map_err(|err| format!("Cannot create {}: {err}", self.root.display()))
                .and_then(|()| vm.create_blank());
            match created {
                Ok(()) => {
                    CREATED.with(|c| *c.borrow_mut() = Some(vm));
                    // SAFETY: closes this modal dialog.
                    let _ = unsafe { EndDialog(self.hwnd, 1) };
                }
                Err(err) => {
                    message_box(self.hwnd, &err, MB_OK | MB_ICONERROR);
                }
            }
        }
    }

    /// The r86 icon in a dialog's title bar and the taskbar (shared icons,
    /// never destroyed).
    fn set_icons(hwnd: HWND) {
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

    fn is_window(hwnd: HWND) -> bool {
        // SAFETY: IsWindow accepts any handle.
        !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    fn vm_name(directory: &Path) -> String {
        directory
            .file_name()
            .map_or("the VM".into(), |n| n.to_string_lossy().into_owned())
    }

    fn size_text(mib: u64) -> String {
        if mib >= 1024 && mib.is_multiple_of(1024) {
            format!("{} GB", mib / 1024)
        } else {
            format!("{mib} MB")
        }
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
