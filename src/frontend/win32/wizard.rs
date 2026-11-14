//! The New VM wizard, step by step as the handheld's (handheld/screens.rs
//! Wizard): how to start (a CD image, FreeDOS from the kit beside the
//! executable, or an existing disk), which OS, memory, disk size, then the
//! name and "Create and start". Windows 3.1/9x/ME CDs get FreeDOS planted so
//! their setup runs from DOS. The handheld's downloads are not offered.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BACKGROUND_MODE, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    DrawFocusRect, FillRect, GetSysColor, GetSysColorBrush, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_FOCUS, ODS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::catalog::{self, CATALOG, FreeOs, Job, Media};
use crate::guest_os::{self, MEMORY_CHOICES, OsPreset, PRESETS};
use crate::ui::{
    draw_text, enable, instance, item, message_box, open_file, resource, send_text, set_icons,
    set_text, show, size_text, text_of, with,
};
use crate::vm_store::{self, Vm};

// Dialog and control IDs, as in r86-win.rc.
const IDD_NEW_VM: u16 = 101;
const ID_NAME: i32 = 20;
const ID_TITLE: i32 = 30;
const ID_STEP: i32 = 31;
const ID_LIST: i32 = 32;
const ID_NAME_LABEL: i32 = 33;
const ID_SUMMARY: i32 = 34;
const ID_HINT: i32 = 35;
const ID_BACK: i32 = 36;
const ID_NEXT: i32 = IDOK.0;
const ID_CANCEL: i32 = IDCANCEL.0;
/// Polls a FreeDOS planting job.
const TIMER_JOB: usize = 1;

/// In the order of the first step's list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Iso,
    FreeDos,
    Disk,
}

const SOURCES: [Source; 3] = [Source::Iso, Source::FreeDos, Source::Disk];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Source,
    Os,
    Memory,
    Disk,
    Summary,
}

struct Wizard {
    hwnd: HWND,
    root: PathBuf,
    /// Where dos.zip is: beside the executable.
    share: PathBuf,
    step: Step,
    source: Source,
    media: Option<PathBuf>,
    detected: Option<&'static OsPreset>,
    os: &'static OsPreset,
    memory: u32,
    disk_mib: u64,
    /// The name the wizard last suggested; replaced while unedited.
    suggested: String,
    job: Option<Job>,
    created: Option<Vm>,
}

thread_local! {
    static WIZARD: RefCell<Option<Wizard>> = const { RefCell::new(None) };
}

/// Runs the modal wizard. Returns the VM it created, to be started.
pub fn run(owner: HWND, root: &Path, share: &Path) -> Option<Vm> {
    WIZARD.with(|cell| {
        *cell.borrow_mut() = Some(Wizard {
            hwnd: HWND::default(),
            root: root.into(),
            share: share.into(),
            step: Step::Source,
            source: Source::Iso,
            media: None,
            detected: None,
            os: guest_os::by_name("Other"),
            memory: 128,
            disk_mib: 2048,
            suggested: String::new(),
            job: None,
            created: None,
        })
    });
    // SAFETY: a modal dialog from this module's resources.
    unsafe {
        DialogBoxParamW(
            Some(instance()),
            resource(IDD_NEW_VM),
            Some(owner),
            Some(wizard_proc),
            LPARAM(0),
        )
    };
    WIZARD.with(|cell| cell.borrow_mut().take())?.created
}

unsafe extern "system" fn wizard_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => {
            set_icons(hwnd);
            with(&WIZARD, |wizard| {
                wizard.hwnd = hwnd;
                wizard.enter(Step::Source);
                false // enter() set the focus
            })
        }
        // Rows 12 dialog units high; sent before WM_INITDIALOG.
        WM_MEASUREITEM => {
            let mut rect = RECT {
                bottom: 12,
                ..Default::default()
            };
            // SAFETY: lParam is the MEASUREITEMSTRUCT for this message.
            unsafe {
                let _ = MapDialogRect(hwnd, &mut rect);
                (*(l.0 as *mut MEASUREITEMSTRUCT)).itemHeight = rect.bottom as u32;
            }
            1
        }
        WM_DRAWITEM => {
            // SAFETY: lParam is the DRAWITEMSTRUCT for this message.
            draw_choice(unsafe { &*(l.0 as *const DRAWITEMSTRUCT) });
            1
        }
        _ => with(&WIZARD, |wizard| wizard.handle(msg, w)),
    }
}

/// One row of the choice list: the choice, and after a tab its detail
/// ("detected", "recommended") right-aligned and dimmed, as on the handheld.
/// Only system brushes, which are never deleted.
fn draw_choice(item: &DRAWITEMSTRUCT) {
    let dc = item.hDC;
    let selected = item.itemState.0 & ODS_SELECTED.0 != 0;
    // SAFETY: drawing into the DC Windows lent for this message; the text
    // buffer is large enough for any row this module adds.
    unsafe {
        let background = if selected {
            COLOR_HIGHLIGHT
        } else {
            COLOR_WINDOW
        };
        FillRect(dc, &item.rcItem, GetSysColorBrush(background));
        if item.itemID != u32::MAX {
            let mut buffer = [0u16; 256];
            let len = SendMessageW(
                item.hwndItem,
                LB_GETTEXT,
                Some(WPARAM(item.itemID as usize)),
                Some(LPARAM(buffer.as_mut_ptr() as isize)),
            )
            .0;
            let text = String::from_utf16_lossy(&buffer[..len.max(0) as usize]);
            let (choice, detail) = text.split_once('\t').unwrap_or((&text, ""));
            let format = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
            let pad = (item.rcItem.bottom - item.rcItem.top) / 3;
            let mut rect = item.rcItem;
            rect.left += pad;
            rect.right -= pad;
            let (dim, normal) = if selected {
                (COLOR_HIGHLIGHTTEXT, COLOR_HIGHLIGHTTEXT)
            } else {
                (COLOR_GRAYTEXT, COLOR_WINDOWTEXT)
            };
            let mode = SetBkMode(dc, TRANSPARENT);
            let color = SetTextColor(dc, COLORREF(GetSysColor(dim)));
            draw_text(dc, detail, &mut rect.clone(), format | DT_RIGHT);
            SetTextColor(dc, COLORREF(GetSysColor(normal)));
            draw_text(dc, choice, &mut rect, format | DT_LEFT | DT_END_ELLIPSIS);
            SetTextColor(dc, color);
            SetBkMode(dc, BACKGROUND_MODE(mode as u32));
        }
        if item.itemState.0 & ODS_FOCUS.0 != 0 {
            let _ = DrawFocusRect(dc, &item.rcItem);
        }
    }
}

/// The FreeDOS entry of the kit's catalog.
fn freedos() -> &'static FreeOs {
    CATALOG
        .iter()
        .find(|os| matches!(os.media, Media::FreeDos))
        .expect("the catalog lists FreeDOS")
}

impl Wizard {
    fn item(&self, id: i32) -> HWND {
        item(self.hwnd, id)
    }

    fn handle(&mut self, msg: u32, w: WPARAM) -> bool {
        match msg {
            WM_COMMAND => {
                let (id, code) = ((w.0 & 0xffff) as i32, (w.0 >> 16) as u32);
                if self.job.is_some() {
                    return true; // planting cannot be cancelled
                }
                match id {
                    ID_NEXT => self.next(),
                    ID_LIST if code == LBN_DBLCLK => self.next(),
                    ID_BACK => self.back(),
                    // SAFETY: closes this modal dialog.
                    ID_CANCEL => unsafe {
                        let _ = EndDialog(self.hwnd, 0);
                    },
                    _ => return false,
                }
                true
            }
            WM_TIMER if w.0 == TIMER_JOB => {
                self.poll();
                true
            }
            _ => false,
        }
    }

    fn steps(&self) -> &'static [Step] {
        match self.source {
            Source::Iso => &[
                Step::Source,
                Step::Os,
                Step::Memory,
                Step::Disk,
                Step::Summary,
            ],
            Source::FreeDos => &[Step::Source, Step::Summary],
            Source::Disk => &[Step::Source, Step::Os, Step::Memory, Step::Summary],
        }
    }

    /// Windows 3.1/9x CDs install from FreeDOS planted on the new disk, so
    /// their setup runs from DOS on first boot; other CDs get a blank disk.
    fn windows_setup(&self) -> Option<&'static str> {
        guest_os::dos_setup(self.os).filter(|_| self.source == Source::Iso)
    }

    fn choose_os(&mut self, os: &'static OsPreset) {
        self.os = os;
        self.memory = os.memory_mib;
        self.disk_mib = os.disk_mib;
    }

    /// Suggests a name from `base` unless the user typed one.
    fn suggest(&mut self, base: &str) {
        let edit = self.item(ID_NAME);
        let name = text_of(edit);
        if name.is_empty() || name == self.suggested {
            self.suggested = vm_store::suggested_name(&self.root, base);
            set_text(edit, &self.suggested);
        }
    }

    fn enter(&mut self, step: Step) {
        self.step = step;
        let steps = self.steps();
        let index = steps.iter().position(|s| *s == step).unwrap_or(0);
        set_text(
            self.item(ID_STEP),
            &format!("{} of {}", index + 1, steps.len()),
        );
        let mark = |on: bool, text: &str| {
            if on {
                format!("\t{text}")
            } else {
                String::new()
            }
        };
        let (title, hint, choices, selected): (&str, String, Vec<String>, usize) =
            match step {
                Step::Source => (
                    "How to start?",
                    "Bring your own installation CD, or start with FreeDOS, which comes with r86."
                        .into(),
                    vec![
                        "Install from a CD image\t.iso".into(),
                        "Install FreeDOS\tincluded".into(),
                        "Use an existing disk\t.vhd .img".into(),
                    ],
                    self.source as usize,
                ),
                Step::Os => (
                    "Which OS is it?",
                    if self.detected.is_some() {
                        "r86 recognised the image. Its choice sets the memory and disk size."
                    } else {
                        "r86 did not recognise the image; choose the closest."
                    }
                    .into(),
                    PRESETS
                        .iter()
                        .map(|os| {
                            let detected = self.detected.is_some_and(|d| d.id == os.id);
                            format!("{}{}", os.name, mark(detected, "detected"))
                        })
                        .collect(),
                    PRESETS.iter().position(|p| p.id == self.os.id).unwrap_or(0),
                ),
                Step::Memory => (
                    "Memory",
                    String::new(),
                    MEMORY_CHOICES
                        .iter()
                        .map(|&mib| {
                            let recommended = mib == self.os.memory_mib;
                            let size = size_text(u64::from(mib));
                            format!("{size}{}", mark(recommended, "recommended"))
                        })
                        .collect(),
                    MEMORY_CHOICES
                        .iter()
                        .rposition(|&mib| mib <= self.memory)
                        .unwrap_or(0),
                ),
                Step::Disk => {
                    let choices = guest_os::disk_choices(self.os);
                    (
                    "Disk size (grows as used)",
                    "The disk file starts small and grows as the guest writes, up to this size."
                        .into(),
                    choices
                        .iter()
                        .map(|&mib| {
                            let recommended = mib == self.os.disk_mib;
                            format!("{}{}", size_text(mib), mark(recommended, "recommended"))
                        })
                        .collect(),
                    choices.iter().position(|&mib| mib == self.disk_mib).unwrap_or(0),
                )
                }
                Step::Summary => ("Ready", self.summary_hint().into(), Vec::new(), 0),
            };
        set_text(self.item(ID_TITLE), title);
        set_text(self.item(ID_HINT), &hint);
        let list = self.item(ID_LIST);
        // SAFETY: list box messages.
        unsafe { SendMessageW(list, LB_RESETCONTENT, None, None) };
        for choice in &choices {
            send_text(list, LB_ADDSTRING, choice);
        }
        // SAFETY: list box message.
        unsafe { SendMessageW(list, LB_SETCURSEL, Some(WPARAM(selected)), None) };
        let summary = step == Step::Summary;
        if summary {
            set_text(self.item(ID_SUMMARY), &self.summary());
        }
        show(list, !summary);
        for id in [ID_NAME_LABEL, ID_NAME, ID_SUMMARY] {
            show(self.item(id), summary);
        }
        enable(self.item(ID_BACK), index > 0);
        let next = if summary {
            "Create and &start"
        } else {
            "&Next >"
        };
        set_text(self.item(ID_NEXT), next);
        let focus = if summary { ID_NAME } else { ID_LIST };
        // SAFETY: focuses a control of this dialog.
        let _ = unsafe { SetFocus(Some(self.item(focus))) };
    }

    fn summary(&self) -> String {
        let mut lines = vec![
            format!("OS: {}", self.os.name),
            format!("Memory: {}", size_text(u64::from(self.memory))),
        ];
        if self.source != Source::Disk {
            lines.push(format!("Disk size: {}", size_text(self.disk_mib)));
        }
        let file = |path: &Option<PathBuf>| {
            path.as_ref()
                .and_then(|p| p.file_name())
                .map_or(String::new(), |n| n.to_string_lossy().into_owned())
        };
        lines.push(match self.source {
            Source::Iso => format!("CD: {}", file(&self.media)),
            Source::FreeDos => "FreeDOS: included".into(),
            Source::Disk => format!("Disk: {}", file(&self.media)),
        });
        lines.join("\n")
    }

    fn summary_hint(&self) -> &'static str {
        match self.source {
            Source::Iso if self.windows_setup().is_some() => {
                "Setup runs from DOS: r86 puts FreeDOS on the new disk, and it starts Windows Setup from the CD."
            }
            Source::Iso => "The VM starts from the CD to install.",
            Source::FreeDos => "FreeDOS is installed on the new disk.",
            Source::Disk => "The disk image is used where it is, not copied.",
        }
    }

    fn selected(&self) -> usize {
        // SAFETY: list box query.
        let index = unsafe { SendMessageW(self.item(ID_LIST), LB_GETCURSEL, None, None) }.0;
        index.max(0) as usize
    }

    fn back(&mut self) {
        let steps = self.steps();
        if let Some(i) = steps.iter().position(|s| *s == self.step)
            && i > 0
        {
            self.enter(steps[i - 1]);
        }
    }

    fn next(&mut self) {
        let selected = self.selected();
        match self.step {
            Step::Source => {
                self.source = SOURCES[selected.min(SOURCES.len() - 1)];
                self.media = None;
                self.detected = None;
                if self.source == Source::FreeDos {
                    let os = freedos();
                    self.choose_os(guest_os::by_name(os.preset));
                    self.memory = os.memory_mib;
                    self.disk_mib = os.disk_mib;
                    self.suggest(os.name);
                    self.enter(Step::Summary);
                    return;
                }
                let iso = self.source == Source::Iso;
                let path = if iso {
                    let filter = [("CD images (*.iso)", "*.iso"), ("All files", "*.*")];
                    open_file(self.hwnd, "Choose a CD image", &filter)
                } else {
                    let filter = [
                        ("Disk images (*.vhd, *.img)", "*.vhd;*.img;*.raw"),
                        ("All files", "*.*"),
                    ];
                    open_file(self.hwnd, "Choose a disk image", &filter)
                };
                let Some(path) = path.map(PathBuf::from) else {
                    return;
                };
                self.detected = if iso {
                    guest_os::detect_iso(&path).ok().flatten()
                } else {
                    guest_os::detect_disk(&path).ok().flatten()
                };
                self.choose_os(self.detected.unwrap_or(guest_os::by_name("Other")));
                self.media = Some(path);
                self.enter(Step::Os);
            }
            Step::Os => {
                self.choose_os(&PRESETS[selected.min(PRESETS.len() - 1)]);
                self.enter(Step::Memory);
            }
            Step::Memory => {
                self.memory = MEMORY_CHOICES[selected.min(MEMORY_CHOICES.len() - 1)];
                self.suggest(self.os.name);
                let next = if self.source == Source::Iso {
                    Step::Disk
                } else {
                    Step::Summary
                };
                self.enter(next);
            }
            Step::Disk => {
                let choices = guest_os::disk_choices(self.os);
                self.disk_mib = choices[selected.min(choices.len() - 1)];
                self.enter(Step::Summary);
            }
            Step::Summary => self.create(),
        }
    }

    fn create(&mut self) {
        let name = text_of(self.item(ID_NAME)).trim().to_string();
        let problem = match vm_store::validate_name(&name) {
            Err(err) => Some(err),
            Ok(()) if vm_store::name_exists(&self.root, &name) => {
                Some("A VM with this name already exists.".into())
            }
            Ok(()) => None,
        };
        if let Some(problem) = problem {
            message_box(
                self.hwnd,
                &format!("Choose another name.\n\n{problem}"),
                MB_OK | MB_ICONWARNING,
            );
            // SAFETY: focuses a control of this dialog.
            let _ = unsafe { SetFocus(Some(self.item(ID_NAME))) };
            return;
        }
        if let Err(err) = fs::create_dir_all(&self.root) {
            let text = format!("Cannot create {}: {err}", self.root.display());
            message_box(self.hwnd, &text, MB_OK | MB_ICONERROR);
            return;
        }
        let mut vm = Vm::new(&self.root, name, self.os);
        vm.engine = "auto".into();
        vm.memory = self.memory;
        vm.disk_mib = self.disk_mib;
        match self.source {
            Source::FreeDos => {
                let job = catalog::install(freedos(), vm, self.share.clone(), self.root.clone());
                self.begin(job);
            }
            Source::Iso if let Some(os_id) = self.windows_setup() => {
                vm.iso = self.media.clone();
                let job = catalog::install_windows(os_id, vm, self.share.clone());
                self.begin(job);
            }
            Source::Iso => {
                vm.iso = self.media.clone();
                let result = vm.create_blank().map(|()| vm);
                self.finish(result);
            }
            Source::Disk => {
                vm.disk = self.media.clone().unwrap_or_default();
                vm.disk_mib = fs::metadata(&vm.disk).map_or(0, |m| m.len() >> 20).max(1);
                let result = vm.create_existing().map(|()| vm);
                self.finish(result);
            }
        }
    }

    /// Shows a planting job's progress; the buttons wait for it.
    fn begin(&mut self, job: Job) {
        self.job = Some(job);
        for id in [ID_NAME, ID_BACK, ID_NEXT, ID_CANCEL] {
            enable(self.item(id), false);
        }
        // SAFETY: a timer on this dialog, killed in poll().
        unsafe { SetTimer(Some(self.hwnd), TIMER_JOB, 100, None) };
        self.poll();
    }

    fn poll(&mut self) {
        let Some(job) = &mut self.job else { return };
        let Some(result) = job.finished() else {
            let (stage, done, total) = job.status();
            let text = match total {
                0 => format!("{stage}..."),
                total => format!("{stage}... {}%", done * 100 / total),
            };
            set_text(self.item(ID_HINT), &text);
            return;
        };
        self.job = None;
        // SAFETY: the timer begin() set.
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_JOB) };
        for id in [ID_NAME, ID_NEXT, ID_CANCEL] {
            enable(self.item(id), true);
        }
        self.finish(result);
    }

    fn finish(&mut self, result: Result<Vm, String>) {
        match result {
            Ok(vm) => {
                self.created = Some(vm);
                // SAFETY: closes this modal dialog.
                let _ = unsafe { EndDialog(self.hwnd, 1) };
            }
            Err(err) => {
                let text = format!("Could not set up the VM.\n\n{err}");
                message_box(self.hwnd, &text, MB_OK | MB_ICONERROR);
                self.enter(Step::Summary);
            }
        }
    }
}
