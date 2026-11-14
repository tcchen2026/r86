//! "Edit this VM": the handheld's VM settings (handheld/screens.rs Settings)
//! that the Windows app uses: OS type, memory, CD, boot order and network.
//! The VM must be off; saving takes its lock.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::guest_os::{self, MEMORY_CHOICES, PRESETS};
use crate::ui::{
    instance, item, message_box, open_file, resource, send_text, set_icons, set_text, size_text,
    with,
};
use crate::vm_store::Vm;

// Dialog and control IDs, as in r86-win.rc.
const IDD_EDIT_VM: u16 = 102;
const ID_OS: i32 = 40;
const ID_MEMORY: i32 = 41;
const ID_CD: i32 = 42;
const ID_CHOOSE: i32 = 43;
const ID_EJECT: i32 = 44;
const ID_BOOT: i32 = 45;
const ID_NETWORK: i32 = 46;
const ID_DISK: i32 = 47;
const ID_OK: i32 = IDOK.0;
const ID_CANCEL: i32 = IDCANCEL.0;

struct EditVm {
    hwnd: HWND,
    vm: Vm,
    /// The memory list's values: the choices plus the VM's own size.
    memory: Vec<u32>,
    iso: Option<PathBuf>,
    saved: bool,
}

thread_local! {
    static EDIT_VM: RefCell<Option<EditVm>> = const { RefCell::new(None) };
}

/// Runs the modal dialog on a stopped VM. Returns whether it saved changes.
pub fn run(owner: HWND, vm: Vm) -> bool {
    let mut memory = MEMORY_CHOICES.to_vec();
    if !memory.contains(&vm.memory) {
        memory.push(vm.memory);
        memory.sort_unstable();
    }
    EDIT_VM.with(|cell| {
        *cell.borrow_mut() = Some(EditVm {
            hwnd: HWND::default(),
            iso: vm.iso.clone(),
            vm,
            memory,
            saved: false,
        })
    });
    // SAFETY: a modal dialog from this module's resources.
    unsafe {
        DialogBoxParamW(
            Some(instance()),
            resource(IDD_EDIT_VM),
            Some(owner),
            Some(edit_proc),
            LPARAM(0),
        )
    };
    EDIT_VM
        .with(|cell| cell.borrow_mut().take())
        .is_some_and(|dialog| dialog.saved)
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, w: WPARAM, _l: LPARAM) -> isize {
    if msg == WM_INITDIALOG {
        set_icons(hwnd);
        return with(&EDIT_VM, |dialog| {
            dialog.hwnd = hwnd;
            dialog.init();
            true
        });
    }
    with(&EDIT_VM, |dialog| dialog.handle(msg, w))
}

impl EditVm {
    fn item(&self, id: i32) -> HWND {
        item(self.hwnd, id)
    }

    fn selection(&self, id: i32) -> usize {
        // SAFETY: combo box query.
        let index = unsafe { SendMessageW(self.item(id), CB_GETCURSEL, None, None) }.0;
        index.max(0) as usize
    }

    fn select(&self, id: i32, index: usize) {
        // SAFETY: combo box selection.
        unsafe { SendMessageW(self.item(id), CB_SETCURSEL, Some(WPARAM(index)), None) };
    }

    fn init(&mut self) {
        set_text(self.hwnd, &format!("Edit {}", self.vm.name));
        let os = self.item(ID_OS);
        for preset in PRESETS {
            send_text(os, CB_ADDSTRING, preset.name);
        }
        let preset = guest_os::by_name(&self.vm.os);
        self.select(
            ID_OS,
            PRESETS.iter().position(|p| p.id == preset.id).unwrap_or(0),
        );
        self.fill_memory(self.vm.memory);
        self.show_cd();
        let boot = self.item(ID_BOOT);
        send_text(boot, CB_ADDSTRING, "Hard disk first");
        send_text(boot, CB_ADDSTRING, "CD first");
        self.select(ID_BOOT, (self.vm.boot_order == "cdrom") as usize);
        let check = if self.vm.network {
            BST_CHECKED
        } else {
            BST_UNCHECKED
        };
        // SAFETY: a check box of this dialog.
        unsafe {
            SendMessageW(
                self.item(ID_NETWORK),
                BM_SETCHECK,
                Some(WPARAM(check.0 as usize)),
                None,
            )
        };
        set_text(
            self.item(ID_DISK),
            &format!("Hard disk: {}", self.vm.disk.display()),
        );
    }

    /// Lists the memory sizes, marking the OS type's, with `current` selected.
    fn fill_memory(&self, current: u32) {
        let list = self.item(ID_MEMORY);
        let recommended = PRESETS[self.selection(ID_OS).min(PRESETS.len() - 1)].memory_mib;
        // SAFETY: combo box message.
        unsafe { SendMessageW(list, CB_RESETCONTENT, None, None) };
        for &mib in &self.memory {
            let size = size_text(u64::from(mib));
            let text = if mib == recommended {
                format!("{size} (recommended)")
            } else {
                size
            };
            send_text(list, CB_ADDSTRING, &text);
        }
        let index = self.memory.iter().position(|&m| m == current).unwrap_or(0);
        self.select(ID_MEMORY, index);
    }

    fn show_cd(&self) {
        let text = self
            .iso
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("empty".into(), |n| n.to_string_lossy().into_owned());
        set_text(self.item(ID_CD), &text);
    }

    fn handle(&mut self, msg: u32, w: WPARAM) -> bool {
        if msg != WM_COMMAND {
            return false;
        }
        let (id, code) = ((w.0 & 0xffff) as i32, (w.0 >> 16) as u32);
        match id {
            ID_OK => self.save(),
            // SAFETY: closes this modal dialog.
            ID_CANCEL => unsafe {
                let _ = EndDialog(self.hwnd, 0);
            },
            ID_OS if code == CBN_SELCHANGE => {
                let memory = self.memory[self.selection(ID_MEMORY).min(self.memory.len() - 1)];
                self.fill_memory(memory);
            }
            ID_CHOOSE => {
                let filter = [("CD images (*.iso)", "*.iso"), ("All files", "*.*")];
                if let Some(path) = open_file(self.hwnd, "Choose a CD image", &filter) {
                    self.iso = Some(path.into());
                    self.show_cd();
                }
            }
            ID_EJECT => {
                self.iso = None;
                self.show_cd();
            }
            _ => return false,
        }
        true
    }

    fn save(&mut self) {
        let os = PRESETS[self.selection(ID_OS).min(PRESETS.len() - 1)].name;
        let memory = self.memory[self.selection(ID_MEMORY).min(self.memory.len() - 1)];
        let boot = if self.selection(ID_BOOT) == 1 {
            "cdrom"
        } else {
            "disk"
        };
        // SAFETY: a check box of this dialog.
        let check = unsafe { SendMessageW(self.item(ID_NETWORK), BM_GETCHECK, None, None) }.0;
        let vm = &mut self.vm;
        vm.os = os.into();
        vm.memory = memory;
        vm.iso = self.iso.clone();
        if vm.iso.is_some() {
            vm.cdrom = true;
        }
        vm.boot_order = boot.into();
        vm.network = check == BST_CHECKED.0 as isize;
        match vm.save() {
            Ok(()) => {
                self.saved = true;
                // SAFETY: closes this modal dialog.
                let _ = unsafe { EndDialog(self.hwnd, 1) };
            }
            Err(err) => {
                let text = format!("Cannot save the settings.\n\n{err}");
                message_box(self.hwnd, &text, MB_OK | MB_ICONERROR);
            }
        }
    }
}
