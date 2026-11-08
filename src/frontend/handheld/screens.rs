//! Screens for the VM library and new-VM wizard, as a stack of views. A view
//! handles keys and may push another view; a popped view hands its result to
//! the view below. The in-VM menu reuses the same stack.
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use super::ui::{
    self, ACCENT, BAD, BG, Canvas, Font, GOOD, Item, MUTED, Menu, PANEL, Rect, TEXT, TextField,
    UiKey,
};
use crate::catalog::{self, CATALOG, FreeOs, Job, Media};
use crate::guest_os::{self, OsPreset, PRESETS};
use crate::media::ImageKind;
use crate::vm_store::{self, Vm};
use crate::ws_frontend::Import;

pub struct Ctx {
    pub library: PathBuf,
    pub share: PathBuf,
    /// Where the disk image chooser starts.
    pub vhd: PathBuf,
    /// Where CD image choosers start; catalog downloads are kept here.
    pub iso: PathBuf,
    /// Largest guest memory offered, in MiB (half the host RAM).
    pub memory_limit: u32,
    /// The chord that opens the in-VM menu, for hints.
    pub menu_keys: &'static str,
    /// Mouse and menu controls, for the VM menu's help screen.
    pub help: &'static [(&'static str, &'static str)],
}

pub enum Reply {
    Path(PathBuf),
    NoFile,
    Text(String),
    Value(u64),
    Yes,
    Done(Result<Box<Vm>, String>),
}

/// Requests from the in-VM menu to the running session.
pub enum Action {
    Resume,
    CycleView,
    Display,
    CtrlAltDel,
    Cd(Option<PathBuf>),
    PowerButton,
    Reset,
    TurnOff,
    /// Start or stop remote management for the running VM.
    Remote,
}

pub enum Nav {
    Stay,
    Push(Box<dyn View>),
    Pop(Option<Reply>),
    Home,
    /// Run a VM; `true` starts it powered off under remote management.
    Start(Box<Vm>, bool),
    Exit,
    Action(Action),
}

pub trait View {
    fn draw(&mut self, c: &mut Canvas, ctx: &Ctx);
    fn key(&mut self, key: UiKey, ctx: &Ctx) -> Nav;
    /// Called when this view is on top again; `reply` is the popped view's result.
    fn reply(&mut self, _reply: Option<Reply>, _ctx: &Ctx) -> Nav {
        Nav::Stay
    }
    /// Periodic update for views showing background progress.
    fn tick(&mut self, _ctx: &Ctx) -> Nav {
        Nav::Stay
    }
    fn animated(&self) -> bool {
        false
    }
}

/// What leaves the stack: a request for the owner, or an empty stack.
pub enum Outcome {
    Start(Box<Vm>, bool),
    Exit,
    Action(Action),
    Closed,
}

pub struct Stack {
    views: Vec<Box<dyn View>>,
    /// A background tick changed the top view; the owner should redraw.
    changed: bool,
}

impl Stack {
    pub fn new(root: Box<dyn View>) -> Self {
        Self {
            views: vec![root],
            changed: false,
        }
    }

    pub fn draw(&mut self, c: &mut Canvas, ctx: &Ctx) {
        self.views.last_mut().unwrap().draw(c, ctx);
    }

    pub fn animated(&self) -> bool {
        self.views.last().unwrap().animated()
    }

    pub fn key(&mut self, key: UiKey, ctx: &Ctx) -> Option<Outcome> {
        let nav = self.views.last_mut().unwrap().key(key, ctx);
        self.apply(nav, ctx)
    }

    pub fn tick(&mut self, ctx: &Ctx) -> Option<Outcome> {
        let nav = self.views.last_mut().unwrap().tick(ctx);
        self.changed |= !matches!(nav, Nav::Stay);
        self.apply(nav, ctx)
    }

    /// Whether a tick changed the screen since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Return to the root view, which refreshes itself.
    pub fn home(&mut self, ctx: &Ctx) -> Option<Outcome> {
        self.apply(Nav::Home, ctx)
    }

    pub fn push(&mut self, view: Box<dyn View>) {
        self.views.push(view);
    }

    fn apply(&mut self, mut nav: Nav, ctx: &Ctx) -> Option<Outcome> {
        loop {
            nav = match nav {
                Nav::Stay => return None,
                Nav::Push(view) => {
                    self.views.push(view);
                    return None;
                }
                Nav::Pop(reply) => {
                    self.views.pop();
                    match self.views.last_mut() {
                        Some(view) => view.reply(reply, ctx),
                        None => return Some(Outcome::Closed),
                    }
                }
                Nav::Home => {
                    self.views.truncate(1);
                    self.views[0].reply(None, ctx)
                }
                Nav::Start(vm, remote) => return Some(Outcome::Start(vm, remote)),
                Nav::Exit => return Some(Outcome::Exit),
                Nav::Action(action) => return Some(Outcome::Action(action)),
            };
        }
    }
}

/// List screens also accept the CardputerZero launcher keys: F/X move,
/// C confirms and Z goes back, so no Fn chord is needed.
fn list_key(key: UiKey) -> UiKey {
    match key {
        UiKey::Char('f') => UiKey::Up,
        UiKey::Char('x') => UiKey::Down,
        UiKey::Char('c') | UiKey::Right => UiKey::Enter,
        UiKey::Char('z') | UiKey::Left => UiKey::Back,
        key => key,
    }
}

const LIST_HINT: &str = "F/X move  C/Enter OK  Z/Esc back";

fn list_screen(c: &mut Canvas, title: &str, note: &str, menu: &mut Menu, hint: &str) {
    c.fill(BG);
    ui::header(c, title, note);
    let area = ui::body(c);
    menu.draw(c, area);
    ui::footer(c, hint);
}

fn megabytes(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GB", mib / 1024)
    } else {
        format!("{mib} MB")
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn message(title: &str, text: impl Into<String>) -> Nav {
    Nav::Push(Box::new(Message {
        title: title.into(),
        text: text.into(),
    }))
}

// ---------------------------------------------------------------- Home

pub struct Home {
    vms: Vec<Result<Vm, String>>,
    menu: Menu,
}

impl Home {
    pub fn new(ctx: &Ctx) -> Self {
        let mut home = Self {
            vms: Vec::new(),
            menu: Menu::new(Vec::new()),
        };
        home.refresh(ctx);
        home
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.vms = vm_store::list(&ctx.library);
        let mut items: Vec<Item> = self
            .vms
            .iter()
            .map(|vm| match vm {
                Ok(vm) => Item::new(&vm.name).detail(format!("{} MB", vm.memory)),
                Err(err) => Item::new(err).color(BAD),
            })
            .collect();
        items.push(Item::new("+ New virtual machine").color(GOOD));
        items.push(Item::new("Exit").color(MUTED));
        let selected = self.menu.selected.min(items.len() - 1);
        self.menu = Menu::new(items).select(selected);
    }
}

impl View for Home {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        let note = match self.vms.len() {
            1 => "1 VM".to_string(),
            n => format!("{n} VMs"),
        };
        list_screen(
            c,
            "r86 - x86 PCs",
            &note,
            &mut self.menu,
            "F/X move  C/Enter open",
        );
    }

    fn key(&mut self, key: UiKey, ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) || key != UiKey::Enter {
            return Nav::Stay;
        }
        let index = self.menu.selected;
        match self.vms.get(index) {
            Some(Ok(vm)) => Nav::Push(Box::new(VmMenu::new(vm.clone()))),
            Some(Err(err)) => message("Cannot read this VM", err.clone()),
            None if index == self.vms.len() => Nav::Push(Box::new(Wizard::new(ctx))),
            None => Nav::Exit,
        }
    }

    fn reply(&mut self, _reply: Option<Reply>, ctx: &Ctx) -> Nav {
        self.refresh(ctx);
        Nav::Stay
    }
}

// ---------------------------------------------------------------- One VM

struct VmMenu {
    vm: Vm,
    menu: Menu,
}

impl VmMenu {
    fn new(vm: Vm) -> Self {
        Self {
            vm,
            menu: Menu::new(vec![
                Item::new("Start").color(GOOD),
                Item::new("Remote management (Wi-Fi)"),
                Item::new("Settings"),
                Item::new("Delete").color(BAD),
            ]),
        }
    }
}

impl View for VmMenu {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        let note = self.vm.os.clone();
        list_screen(c, &self.vm.name, &note, &mut self.menu, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        match key {
            UiKey::Back => Nav::Pop(None),
            UiKey::Enter => match self.menu.selected {
                0 => Nav::Start(Box::new(self.vm.clone()), false),
                1 => Nav::Start(Box::new(self.vm.clone()), true),
                2 => Nav::Push(Box::new(Settings::new(self.vm.clone()))),
                _ => {
                    let inside = self.vm.disk.starts_with(&self.vm.directory);
                    let text = if inside {
                        format!("\"{}\" and its hard disk will be erased.", self.vm.name)
                    } else {
                        format!(
                            "\"{}\" will be removed. Its disk image {} is kept.",
                            self.vm.name,
                            file_name(&self.vm.disk)
                        )
                    };
                    Nav::Push(Box::new(Confirm::new("Delete this VM?", text, "Delete")))
                }
            },
            _ => Nav::Stay,
        }
    }

    fn reply(&mut self, reply: Option<Reply>, _ctx: &Ctx) -> Nav {
        match reply {
            Some(Reply::Yes) => match self.vm.delete() {
                Ok(()) => Nav::Home,
                Err(err) => message("Delete failed", err),
            },
            _ => {
                // Settings saved its own copy; start with what it saved.
                if let Ok(vm) = Vm::load(self.vm.directory.clone()) {
                    self.vm = vm;
                }
                Nav::Stay
            }
        }
    }
}

// ---------------------------------------------------------------- Settings

#[derive(Clone, Copy)]
enum Field {
    Os,
    Memory,
    Cd,
    Boot,
    Scale,
}

struct Settings {
    vm: Vm,
    menu: Menu,
    pending: Field,
}

impl Settings {
    const FIELDS: [Field; 5] = [
        Field::Os,
        Field::Memory,
        Field::Cd,
        Field::Boot,
        Field::Scale,
    ];

    fn new(vm: Vm) -> Self {
        let mut settings = Self {
            vm,
            menu: Menu::new(Vec::new()),
            pending: Field::Os,
        };
        settings.rebuild();
        settings
    }

    fn rebuild(&mut self) {
        let vm = &self.vm;
        let cd = match (&vm.iso, vm.cdrom) {
            (Some(iso), _) => file_name(iso),
            (None, true) => "empty".into(),
            (None, false) => "none".into(),
        };
        let boot = if vm.boot_order == "cdrom" {
            "CD first"
        } else {
            "Hard disk first"
        };
        let items = vec![
            Item::new("OS type").detail(&vm.os),
            Item::new("Memory").detail(format!("{} MB", vm.memory)),
            Item::new("CD").detail(ui::clip(&cd, 18)),
            Item::new("Boot").detail(boot),
            Item::new("Screen").detail(match vm.scale.as_str() {
                "stretch" => "Stretch",
                "integer" => "Integer scale",
                _ => "Keep aspect",
            }),
        ];
        let selected = self.menu.selected;
        self.menu = Menu::new(items).select(selected);
    }

    fn save(&mut self) -> Nav {
        self.rebuild();
        match self.vm.save() {
            Ok(()) => Nav::Stay,
            Err(err) => message("Cannot save settings", err),
        }
    }
}

impl View for Settings {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        list_screen(c, "Settings", &self.vm.name, &mut self.menu, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        if key == UiKey::Back {
            return Nav::Pop(None);
        }
        if key != UiKey::Enter {
            return Nav::Stay;
        }
        self.pending = Self::FIELDS[self.menu.selected];
        match self.pending {
            Field::Os => Nav::Push(Box::new(Picker::os(guest_os::by_name(&self.vm.os), None))),
            Field::Memory => Nav::Push(Box::new(Picker::memory(
                ctx,
                self.vm.memory,
                guest_os::by_name(&self.vm.os),
            ))),
            Field::Cd => Nav::Push(Box::new(
                Browser::new("Choose a CD image", &ctx.iso, &["iso"], Some("No disc"))
                    .with_import(ImageKind::Cd, &ctx.iso),
            )),
            Field::Boot => {
                self.vm.boot_order = if self.vm.boot_order == "cdrom" {
                    "disk"
                } else {
                    "cdrom"
                }
                .into();
                self.save()
            }
            Field::Scale => {
                self.vm.scale = match self.vm.scale.as_str() {
                    "aspect" => "stretch",
                    "stretch" => "integer",
                    _ => "aspect",
                }
                .into();
                self.save()
            }
        }
    }

    fn reply(&mut self, reply: Option<Reply>, _ctx: &Ctx) -> Nav {
        match (self.pending, reply) {
            (Field::Os, Some(Reply::Value(index))) => {
                self.vm.os = PRESETS[index as usize].name.into();
            }
            (Field::Memory, Some(Reply::Value(mib))) => self.vm.memory = mib as u32,
            (Field::Cd, Some(Reply::Path(path))) => {
                self.vm.cdrom = true;
                self.vm.iso = Some(path);
            }
            (Field::Cd, Some(Reply::NoFile)) => self.vm.iso = None,
            _ => return Nav::Stay,
        }
        self.save()
    }
}

// ---------------------------------------------------------------- Wizard

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Free,
    Iso,
    Disk,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Source,
    Free,
    Os,
    Memory,
    Disk,
    Summary,
}

#[derive(Clone, Copy)]
enum Pending {
    None,
    Media,
    Name,
    Memory,
    DiskSize,
    Job,
}

pub struct Wizard {
    step: Step,
    menu: Menu,
    source: Source,
    free: Option<&'static FreeOs>,
    media: Option<PathBuf>,
    detected: Option<&'static OsPreset>,
    os: &'static OsPreset,
    memory: u32,
    disk_mib: u64,
    name: String,
    pending: Pending,
}

impl Wizard {
    fn new(_ctx: &Ctx) -> Self {
        let mut wizard = Self {
            step: Step::Source,
            menu: Menu::new(Vec::new()),
            source: Source::Free,
            free: None,
            media: None,
            detected: None,
            os: guest_os::by_name("Other"),
            memory: 128,
            disk_mib: 2048,
            name: String::new(),
            pending: Pending::None,
        };
        wizard.enter(Step::Source, None);
        wizard
    }

    fn steps(&self) -> &'static [Step] {
        match self.source {
            Source::Free => &[Step::Source, Step::Free, Step::Summary],
            Source::Iso => &[
                Step::Source,
                Step::Os,
                Step::Memory,
                Step::Disk,
                Step::Summary,
            ],
            Source::Disk => &[Step::Source, Step::Os, Step::Memory, Step::Summary],
        }
    }

    /// Windows 3.1/9x CDs install from FreeDOS planted on the new disk, so
    /// their setup runs from DOS on first boot; other CDs get a blank disk.
    fn windows_setup(&self) -> Option<&'static str> {
        let dos_setup = matches!(self.os.id, "win31" | "win95" | "win98" | "winme");
        (self.source == Source::Iso && dos_setup).then_some(self.os.id)
    }

    /// Whether this VM gets a new disk whose size the user picks.
    fn new_disk(&self) -> bool {
        match self.source {
            Source::Iso => true,
            Source::Free => self
                .free
                .is_some_and(|os| matches!(os.media, Media::FreeDos)),
            Source::Disk => false,
        }
    }

    fn choose_os(&mut self, os: &'static OsPreset, ctx: Option<&Ctx>) {
        self.os = os;
        self.memory = os.memory_mib;
        if let Some(ctx) = ctx {
            self.memory = self.memory.min(ctx.memory_limit);
        }
        self.disk_mib = os.disk_mib;
    }

    fn enter(&mut self, step: Step, ctx: Option<&Ctx>) {
        self.step = step;
        self.menu = match step {
            Step::Source => Menu::new(vec![
                Item::new("Get a free OS").detail("ReactOS, FreeDOS"),
                Item::new("Install from a CD image").detail(".iso"),
                Item::new("Use an existing disk").detail(".vhd .img"),
            ])
            .select(self.source as usize),
            Step::Free => Menu::new(
                CATALOG
                    .iter()
                    .map(|os| Item::new(os.title).detail(os.detail))
                    .collect(),
            ),
            Step::Os => {
                let items = PRESETS
                    .iter()
                    .map(|os| {
                        let detected = self.detected.is_some_and(|d| d.id == os.id);
                        Item::new(os.name).detail(if detected { "detected" } else { "" })
                    })
                    .collect();
                let index = PRESETS.iter().position(|p| p.id == self.os.id).unwrap_or(0);
                Menu::new(items).select(index)
            }
            Step::Memory => {
                let ctx = ctx.expect("memory step has context");
                Picker::memory(ctx, self.memory, self.os).menu
            }
            Step::Disk => Picker::disk(self.disk_mib, self.os).menu,
            Step::Summary => {
                let mut items = vec![
                    Item::new(match self.free {
                        Some(os) if os.download_mib > 0 => "Download and start",
                        _ => "Create and start",
                    })
                    .color(GOOD),
                    Item::new("Name").detail(ui::clip(&self.name, 18)),
                    Item::new("Memory").detail(format!("{} MB", self.memory)),
                ];
                if self.new_disk() {
                    items.push(Item::new("Disk size").detail(megabytes(self.disk_mib)));
                }
                match self.source {
                    Source::Iso => {
                        let iso = file_name(self.media.as_ref().unwrap());
                        items.push(Item::new("CD").detail(ui::clip(&iso, 18)).color(MUTED));
                    }
                    Source::Disk => {
                        let disk = file_name(self.media.as_ref().unwrap());
                        items.push(Item::new("Disk").detail(ui::clip(&disk, 18)).color(MUTED));
                    }
                    Source::Free => {
                        let os = self.free.unwrap();
                        let size = if os.download_mib > 0 {
                            format!("download {} MB", os.download_mib)
                        } else {
                            "included".into()
                        };
                        items.push(Item::new(os.title).detail(size).color(MUTED));
                    }
                }
                Menu::new(items)
            }
        };
    }

    fn back(&mut self) -> Nav {
        let steps = self.steps();
        match steps.iter().position(|s| *s == self.step) {
            Some(0) | None => Nav::Pop(None),
            Some(i) => {
                self.enter(steps[i - 1], None);
                Nav::Stay
            }
        }
    }

    fn create(&mut self, ctx: &Ctx) -> Nav {
        if let Err(err) = vm_store::validate_name(&self.name) {
            return message("Choose another name", err);
        }
        if vm_store::name_exists(&ctx.library, &self.name) {
            return message("Choose another name", "A VM with this name already exists.");
        }
        let mut vm = Vm::new(&ctx.library, self.name.clone(), self.os);
        vm.memory = self.memory;
        vm.disk_mib = self.disk_mib;
        match self.source {
            Source::Free => {
                let os = self.free.unwrap();
                self.pending = Pending::Job;
                let job = catalog::install(os, vm, ctx.share.clone(), ctx.iso.clone());
                return Nav::Push(Box::new(Progress::new(os.title, job)));
            }
            Source::Iso if let Some(os_id) = self.windows_setup() => {
                vm.iso = self.media.clone();
                self.pending = Pending::Job;
                let job = catalog::install_windows(os_id, vm, ctx.share.clone());
                return Nav::Push(Box::new(Progress::new(self.os.name, job)));
            }
            Source::Iso => {
                vm.iso = self.media.clone();
                if let Err(err) = vm.create_blank() {
                    return message("Cannot create the VM", err);
                }
            }
            Source::Disk => {
                vm.disk = self.media.clone().unwrap();
                vm.disk_mib = fs::metadata(&vm.disk).map_or(0, |m| m.len() >> 20).max(1);
                if let Err(err) = vm.create_existing() {
                    return message("Cannot create the VM", err);
                }
            }
        }
        Nav::Start(Box::new(vm), false)
    }
}

impl View for Wizard {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        let steps = self.steps();
        let index = steps.iter().position(|s| *s == self.step).unwrap_or(0);
        let note = format!("{}/{}", index + 1, steps.len());
        let title = match self.step {
            Step::Source => "New VM: how to start?",
            Step::Free => "Get a free OS",
            Step::Os => "Which OS is it?",
            Step::Memory => "Memory",
            Step::Disk => "Disk size (grows as used)",
            Step::Summary => "Ready",
        };
        list_screen(c, title, &note, &mut self.menu, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        if key == UiKey::Back {
            return self.back();
        }
        if key != UiKey::Enter {
            return Nav::Stay;
        }
        let selected = self.menu.selected;
        match self.step {
            Step::Source => {
                self.source = [Source::Free, Source::Iso, Source::Disk][selected];
                self.free = None;
                self.media = None;
                self.detected = None;
                if self.source == Source::Free {
                    self.enter(Step::Free, Some(ctx));
                    return Nav::Stay;
                }
                self.pending = Pending::Media;
                let (title, kind, dir, exts): (&str, _, _, &[&str]) = if self.source == Source::Iso
                {
                    ("Choose a CD image", ImageKind::Cd, &ctx.iso, &["iso"])
                } else {
                    (
                        "Choose a disk image",
                        ImageKind::Disk,
                        &ctx.vhd,
                        &["vhd", "img", "raw"],
                    )
                };
                Nav::Push(Box::new(
                    Browser::new(title, dir, exts, None).with_import(kind, dir),
                ))
            }
            Step::Free => {
                let os = &CATALOG[selected];
                self.free = Some(os);
                self.choose_os(guest_os::by_name(os.preset), Some(ctx));
                self.memory = os.memory_mib.min(ctx.memory_limit);
                self.disk_mib = os.disk_mib;
                self.name = vm_store::suggested_name(&ctx.library, os.name);
                self.enter(Step::Summary, Some(ctx));
                Nav::Stay
            }
            Step::Os => {
                self.choose_os(&PRESETS[selected], Some(ctx));
                self.enter(Step::Memory, Some(ctx));
                Nav::Stay
            }
            Step::Memory => {
                self.memory = Picker::memory(ctx, self.memory, self.os).values[selected] as u32;
                self.name = vm_store::suggested_name(&ctx.library, self.os.name);
                let next = if self.source == Source::Iso {
                    Step::Disk
                } else {
                    Step::Summary
                };
                self.enter(next, Some(ctx));
                Nav::Stay
            }
            Step::Disk => {
                self.disk_mib = Picker::disk(self.disk_mib, self.os).values[selected];
                self.enter(Step::Summary, Some(ctx));
                Nav::Stay
            }
            Step::Summary => match selected {
                0 => self.create(ctx),
                1 => {
                    self.pending = Pending::Name;
                    Nav::Push(Box::new(TextInput::new("VM name", &self.name)))
                }
                2 => {
                    self.pending = Pending::Memory;
                    Nav::Push(Box::new(Picker::memory(ctx, self.memory, self.os)))
                }
                3 if self.new_disk() => {
                    self.pending = Pending::DiskSize;
                    Nav::Push(Box::new(Picker::disk(self.disk_mib, self.os)))
                }
                _ => Nav::Stay,
            },
        }
    }

    fn reply(&mut self, reply: Option<Reply>, ctx: &Ctx) -> Nav {
        let pending = std::mem::replace(&mut self.pending, Pending::None);
        match (pending, reply) {
            (Pending::Media, Some(Reply::Path(path))) => {
                self.detected = if self.source == Source::Iso {
                    guest_os::detect_iso(&path).ok().flatten()
                } else {
                    guest_os::detect_disk(&path).ok().flatten()
                };
                self.choose_os(
                    self.detected.unwrap_or(guest_os::by_name("Other")),
                    Some(ctx),
                );
                self.media = Some(path);
                self.enter(Step::Os, Some(ctx));
            }
            (Pending::Name, Some(Reply::Text(name))) => self.name = name,
            (Pending::Memory, Some(Reply::Value(mib))) => self.memory = mib as u32,
            (Pending::DiskSize, Some(Reply::Value(mib))) => self.disk_mib = mib,
            (Pending::Job, Some(Reply::Done(Ok(vm)))) => return Nav::Start(vm, false),
            (Pending::Job, Some(Reply::Done(Err(err)))) => {
                return message("Could not set up the VM", err);
            }
            _ => return Nav::Stay,
        }
        if self.step == Step::Summary {
            let selected = self.menu.selected;
            self.enter(Step::Summary, Some(ctx));
            self.menu.selected = selected;
        }
        Nav::Stay
    }
}

// ---------------------------------------------------------------- Picker

/// A list of values; pops `Reply::Value` with the chosen value.
pub struct Picker {
    title: String,
    menu: Menu,
    values: Vec<u64>,
}

impl Picker {
    fn new(title: &str, items: Vec<Item>, values: Vec<u64>, current: u64) -> Self {
        let index = values.iter().position(|v| *v == current).unwrap_or(0);
        Self {
            title: title.into(),
            menu: Menu::new(items).select(index),
            values,
        }
    }

    fn os(current: &OsPreset, detected: Option<&OsPreset>) -> Self {
        let items = PRESETS
            .iter()
            .map(|os| {
                let mark = detected.is_some_and(|d| d.id == os.id);
                Item::new(os.name).detail(if mark { "detected" } else { "" })
            })
            .collect();
        let index = PRESETS.iter().position(|p| p.id == current.id).unwrap_or(0);
        Self::new(
            "OS type",
            items,
            (0..PRESETS.len() as u64).collect(),
            index as u64,
        )
    }

    fn memory(ctx: &Ctx, current: u32, os: &OsPreset) -> Self {
        let values: Vec<u64> = [16, 32, 64, 96, 128, 192, 256, 384, 512]
            .into_iter()
            .filter(|mib| *mib <= ctx.memory_limit as u64)
            .collect();
        let items = values
            .iter()
            .map(|mib| {
                let note = if *mib == os.memory_mib as u64 {
                    "recommended"
                } else {
                    ""
                };
                Item::new(format!("{mib} MB")).detail(note)
            })
            .collect();
        let current = values
            .iter()
            .copied()
            .rfind(|mib| *mib <= current as u64)
            .unwrap_or(values[0]);
        Self::new("Memory", items, values, current)
    }

    fn disk(current: u64, os: &OsPreset) -> Self {
        // DOS, Windows 3.1 and the first Windows 95 use FAT16 (at most 2 GB).
        let values = if matches!(os.id, "dos" | "win31" | "win95") {
            vec![256, 512, 1024, 2047]
        } else {
            vec![512, 1024, 2048, 4096, 8192, 16384]
        };
        let items = values
            .iter()
            .map(|mib| {
                let note = if *mib == os.disk_mib {
                    "recommended"
                } else {
                    ""
                };
                Item::new(megabytes(*mib)).detail(note)
            })
            .collect();
        Self::new("Disk size (grows as used)", items, values, current)
    }
}

impl View for Picker {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        let title = self.title.clone();
        list_screen(c, &title, "", &mut self.menu, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        match key {
            UiKey::Back => Nav::Pop(None),
            UiKey::Enter => Nav::Pop(Some(Reply::Value(self.values[self.menu.selected]))),
            _ => Nav::Stay,
        }
    }
}

// ---------------------------------------------------------------- Browser

enum Entry {
    /// The `none` choice, such as "No disc".
    None,
    /// Receive the image from a browser on the same Wi-Fi.
    Import,
    Directory(PathBuf),
    File(PathBuf),
}

/// Pick a file with one of `extensions`; pops `Reply::Path` (or `NoFile`).
pub struct Browser {
    title: String,
    directory: PathBuf,
    extensions: &'static [&'static str],
    none: Option<&'static str>,
    /// What "Upload over Wi-Fi" receives, and where it is saved.
    import: Option<(ImageKind, PathBuf)>,
    entries: Vec<Entry>,
    menu: Menu,
}

impl Browser {
    pub fn new(
        title: &str,
        start: &Path,
        extensions: &'static [&'static str],
        none: Option<&'static str>,
    ) -> Self {
        let mut browser = Self {
            title: title.into(),
            directory: start.to_path_buf(),
            extensions,
            none,
            import: None,
            entries: Vec::new(),
            menu: Menu::new(Vec::new()),
        };
        browser.load();
        browser
    }

    /// Offers "Upload over Wi-Fi" first, saving a `kind` image in `dir`.
    pub fn with_import(mut self, kind: ImageKind, dir: &Path) -> Self {
        self.import = Some((kind, dir.to_path_buf()));
        self.load();
        self
    }

    fn load(&mut self) {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        if let Ok(read) = fs::read_dir(&self.directory) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    dirs.push((name, path));
                } else if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if self.extensions.contains(&ext.as_str()) {
                        let size = fs::metadata(&path).map_or(0, |m| m.len());
                        files.push((name, path, size));
                    }
                }
            }
        }
        dirs.sort_by_key(|(name, _)| name.to_lowercase());
        files.sort_by_key(|(name, ..)| name.to_lowercase());
        let mut items = Vec::new();
        self.entries.clear();
        if self.import.is_some() {
            items.push(Item::new("Upload over Wi-Fi").color(GOOD));
            self.entries.push(Entry::Import);
        }
        if let Some(label) = self.none {
            items.push(Item::new(label).color(MUTED));
            self.entries.push(Entry::None);
        }
        if let Some(parent) = self.directory.parent() {
            items.push(Item::new("..").detail("up").color(MUTED));
            self.entries.push(Entry::Directory(parent.to_path_buf()));
        }
        for (name, path) in dirs {
            items.push(Item::new(format!("{name}/")).color(ACCENT_TEXT));
            self.entries.push(Entry::Directory(path));
        }
        for (name, path, size) in files {
            items.push(Item::new(name).detail(megabytes((size + (1 << 19)) >> 20)));
            self.entries.push(Entry::File(path));
        }
        let selected = self.menu.selected;
        self.menu = Menu::new(items).select(selected);
    }
}

const ACCENT_TEXT: u16 = ui::rgb(140, 190, 255);

impl View for Browser {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, &self.title, "");
        let area = ui::body(c);
        let path = self.directory.display().to_string();
        let columns = (c.width / 8 - 2) as usize;
        let shown: String = if path.chars().count() > columns {
            let tail: String = path
                .chars()
                .skip(path.chars().count() - (columns - 1))
                .collect();
            format!("~{tail}")
        } else {
            path
        };
        c.text(6, area.y + 3, &shown, MUTED, Font::Small);
        // The note gets its own line below the list instead of covering it.
        let empty = !self.entries.iter().any(|e| matches!(e, Entry::File(_)));
        let list = Rect {
            y: area.y + 12,
            h: area.h - if empty { 24 } else { 12 },
            ..area
        };
        self.menu.draw(c, list);
        if empty {
            let hint = format!("No {} files here.", self.extensions.join("/"));
            c.text(8, area.y + area.h - 10, &hint, MUTED, Font::Small);
        }
        ui::footer(c, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        match key {
            UiKey::Back => Nav::Pop(None),
            UiKey::Enter => match self.entries.get(self.menu.selected) {
                Some(Entry::None) => Nav::Pop(Some(Reply::NoFile)),
                Some(Entry::Import) => {
                    let (kind, dir) = self.import.as_ref().unwrap();
                    Nav::Push(Box::new(WifiImport::new(*kind, dir)))
                }
                Some(Entry::Directory(path)) => {
                    self.directory = path.clone();
                    self.menu = Menu::new(Vec::new());
                    self.load();
                    Nav::Stay
                }
                Some(Entry::File(path)) => Nav::Pop(Some(Reply::Path(path.clone()))),
                None => Nav::Stay,
            },
            _ => Nav::Stay,
        }
    }

    fn reply(&mut self, reply: Option<Reply>, _ctx: &Ctx) -> Nav {
        match reply {
            // The received image is the choice.
            Some(Reply::Path(path)) => Nav::Pop(Some(Reply::Path(path))),
            _ => Nav::Stay,
        }
    }
}

/// Receives a disk or CD image from a browser on the same Wi-Fi, through the
/// console page in import mode; pops with the saved image.
struct WifiImport {
    kind: ImageKind,
    import: Result<Import, String>,
}

impl WifiImport {
    fn new(kind: ImageKind, dir: &Path) -> Self {
        let import = super::session::console_listener()
            .and_then(|listener| Import::new(listener, kind, dir.to_path_buf()));
        Self { kind, import }
    }
}

impl View for WifiImport {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        let what = match self.kind {
            ImageKind::Cd => "CD image",
            ImageKind::Disk => "disk image",
        };
        ui::header(c, "Upload over Wi-Fi", what);
        let import = match &self.import {
            Ok(import) => import,
            Err(err) => {
                let area = ui::body(c);
                let text = Rect {
                    x: 8,
                    y: area.y + 8,
                    w: area.w - 16,
                    h: area.h - 16,
                };
                c.paragraph(text, err, TEXT, Font::Small);
                ui::footer(c, "Esc: back");
                return;
            }
        };
        match import.url() {
            Some(url) => {
                c.text_centered(30, "On a phone or computer on the same", TEXT, Font::Small);
                c.text_centered(40, "Wi-Fi, open", TEXT, Font::Small);
                c.text_centered(58, url.trim_end_matches('/'), ACCENT, Font::Normal);
            }
            None => {
                c.text_centered(40, "This device is not on a network.", TEXT, Font::Small);
                c.text_centered(52, "Connect it to Wi-Fi.", MUTED, Font::Small);
            }
        }
        match import.progress() {
            None => c.text_centered(90, &format!("and send a {what}."), MUTED, Font::Small),
            Some((name, received, size)) => {
                c.text_centered(88, &ui::clip(name, 36), TEXT, Font::Small);
                let bar = Rect {
                    x: 40,
                    y: 102,
                    w: c.width - 80,
                    h: 6,
                };
                c.rect(bar, PANEL);
                let done = (bar.w as u64 * received / size.max(1)) as i32;
                c.rect(Rect { w: done, ..bar }, ACCENT);
                let text = format!("{} of {} MB", received >> 20, size >> 20);
                c.text_centered(116, &text, MUTED, Font::Small);
            }
        }
        ui::footer(c, "Esc: cancel");
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        // Leaving drops the server and any unfinished image.
        match list_key(key) {
            UiKey::Back => Nav::Pop(None),
            _ => Nav::Stay,
        }
    }

    fn tick(&mut self, _ctx: &Ctx) -> Nav {
        match self.import.as_mut().ok().and_then(Import::poll) {
            Some(path) => Nav::Pop(Some(Reply::Path(path))),
            None => Nav::Stay,
        }
    }

    fn animated(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------- Small views

struct TextInput {
    title: String,
    field: TextField,
}

impl TextInput {
    fn new(title: &str, text: &str) -> Self {
        Self {
            title: title.into(),
            field: TextField {
                text: text.into(),
                max: 40,
            },
        }
    }
}

impl View for TextInput {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, &self.title, "");
        let w = c.width - 24;
        self.field.draw(c, 12, 60, w);
        ui::footer(c, "Type  Enter OK  Esc cancel");
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        match key {
            UiKey::Enter => Nav::Pop(Some(Reply::Text(self.field.text.trim().to_string()))),
            UiKey::Back => Nav::Pop(None),
            key => {
                self.field.key(key);
                Nav::Stay
            }
        }
    }
}

pub struct Confirm {
    title: String,
    text: String,
    menu: Menu,
}

impl Confirm {
    pub fn new(title: &str, text: String, yes: &str) -> Self {
        Self {
            title: title.into(),
            text,
            menu: Menu::new(vec![Item::new("Cancel"), Item::new(yes).color(BAD)]),
        }
    }
}

impl View for Confirm {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, &self.title, "");
        let area = ui::body(c);
        let text_area = Rect {
            x: 8,
            y: area.y + 6,
            w: area.w - 16,
            h: 60,
        };
        c.paragraph(text_area, &self.text, TEXT, Font::Small);
        let list = Rect {
            y: area.y + area.h - 40,
            h: 40,
            ..area
        };
        self.menu.draw(c, list);
        ui::footer(c, LIST_HINT);
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        match key {
            UiKey::Back => Nav::Pop(None),
            UiKey::Enter if self.menu.selected == 1 => Nav::Pop(Some(Reply::Yes)),
            UiKey::Enter => Nav::Pop(None),
            _ => Nav::Stay,
        }
    }
}

pub struct Message {
    pub title: String,
    pub text: String,
}

impl View for Message {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, &self.title, "");
        let area = ui::body(c);
        let text_area = Rect {
            x: 8,
            y: area.y + 8,
            w: area.w - 16,
            h: area.h - 16,
        };
        c.paragraph(text_area, &self.text, TEXT, Font::Small);
        ui::footer(c, "Any key: OK");
    }

    fn key(&mut self, _key: UiKey, _ctx: &Ctx) -> Nav {
        Nav::Pop(None)
    }
}

/// The device's mouse and menu controls, from the VM menu.
struct Help;

impl View for Help {
    fn draw(&mut self, c: &mut Canvas, ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, "Help", "");
        let area = ui::body(c);
        let font = Font::Normal;
        let gap = 16;
        let left = ctx
            .help
            .iter()
            .map(|row| font.width(row.0))
            .max()
            .unwrap_or(0);
        let right = ctx
            .help
            .iter()
            .map(|row| font.width(row.1))
            .max()
            .unwrap_or(0);
        let x = (c.width - left - gap - right) / 2;
        let line = font.height() + 4;
        let mut y = area.y + (area.h - ctx.help.len() as i32 * line) / 2;
        for &(control, action) in ctx.help {
            c.text(x, y, control, TEXT, font);
            c.text(x + left + gap, y, action, MUTED, font);
            y += line;
        }
        ui::footer(c, "Any key: back");
    }

    fn key(&mut self, _key: UiKey, _ctx: &Ctx) -> Nav {
        Nav::Pop(None)
    }
}

/// Shows a background job; pops `Reply::Done` when it finishes.
struct Progress {
    title: String,
    job: Job,
    started: Instant,
}

impl Progress {
    fn new(title: &str, job: Job) -> Self {
        Self {
            title: title.into(),
            job,
            started: Instant::now(),
        }
    }
}

impl View for Progress {
    fn draw(&mut self, c: &mut Canvas, _ctx: &Ctx) {
        c.fill(BG);
        ui::header(c, &self.title, "");
        let (stage, done, total) = self.job.status();
        c.text_centered(50, &stage, TEXT, Font::Normal);
        let bar = Rect {
            x: 20,
            y: 82,
            w: c.width - 40,
            h: 10,
        };
        c.rect(bar, PANEL);
        if let Some(filled) = (bar.w as u64 * done.min(total)).checked_div(total) {
            c.rect(
                Rect {
                    w: filled as i32,
                    ..bar
                },
                ACCENT,
            );
            let seconds = self.started.elapsed().as_secs().max(1);
            let line = format!(
                "{} / {} MB   {} KB/s",
                done >> 20,
                total >> 20,
                done / 1024 / seconds
            );
            c.text_centered(100, &line, MUTED, Font::Small);
        } else {
            // Indeterminate: a block sweeping across the bar.
            let t = self.started.elapsed().as_millis() as i32 / 8;
            let x = bar.x + t % (bar.w - 40);
            c.rect(Rect { x, w: 40, ..bar }, ACCENT);
        }
        ui::footer(c, "Esc: cancel");
    }

    fn key(&mut self, key: UiKey, _ctx: &Ctx) -> Nav {
        if key == UiKey::Back || key == UiKey::Char('z') {
            self.job.cancel();
        }
        Nav::Stay
    }

    fn tick(&mut self, _ctx: &Ctx) -> Nav {
        match self.job.finished() {
            Some(_) if self.job.cancelled() => Nav::Pop(None),
            Some(result) => Nav::Pop(Some(Reply::Done(result.map(Box::new)))),
            None => Nav::Stay,
        }
    }

    fn animated(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------- In-VM menu

/// The in-VM menu. The session owns the labels and updates them in place,
/// so the selection survives changes such as cycling the view.
pub struct VmOverlay {
    labels: Rc<RefCell<OverlayLabels>>,
    menu: Menu,
    cd: bool,
}

#[derive(Default)]
pub struct OverlayLabels {
    pub name: String,
    pub view: &'static str,
    /// Present only on an LCD device with an HDMI connector.
    pub display: Option<bool>,
    pub cd: Option<String>,
    /// The console address while remote management is on.
    pub remote: Option<String>,
}

impl VmOverlay {
    pub fn new(labels: Rc<RefCell<OverlayLabels>>) -> Self {
        let mut overlay = Self {
            labels,
            menu: Menu::new(Vec::new()),
            cd: false,
        };
        overlay.rebuild();
        overlay
    }

    fn rebuild(&mut self) {
        let l = self.labels.borrow();
        let cd =
            l.cd.as_deref()
                .map_or("empty".to_string(), |c| ui::clip(c, 16));
        let selected = self.menu.selected;
        let mut items = vec![
            Item::new("Resume").color(GOOD),
            Item::new("View").detail(l.view),
        ];
        if let Some(hdmi) = l.display {
            items.push(Item::new("Display").detail(if hdmi { "HDMI" } else { "LCD" }));
        }
        items.extend([
            Item::new("Send Ctrl+Alt+Del"),
            Item::new("CD").detail(cd),
            Item::new("Remote management").detail(if l.remote.is_some() { "on" } else { "off" }),
            Item::new("Help"),
            Item::new("Power button"),
            Item::new("Reset"),
            Item::new("Turn off now").color(BAD),
        ]);
        self.menu = Menu::new(items).select(selected);
    }
}

impl View for VmOverlay {
    fn draw(&mut self, c: &mut Canvas, ctx: &Ctx) {
        self.rebuild();
        // Dim the session canvas and float the menu on top.
        c.dim(Rect {
            x: 0,
            y: 0,
            w: c.width,
            h: c.height,
        });
        let title = format!("r86: {}", self.labels.borrow().name);
        ui::header(c, &title, "paused");
        let area = ui::body(c);
        let panel = Rect {
            x: 40,
            w: c.width - 80,
            ..area
        };
        c.rect(panel, BG);
        self.menu.draw(c, panel);
        let remote = self.labels.borrow().remote.clone();
        let hint = remote.map_or_else(
            || format!("{} or Esc: resume", ctx.menu_keys),
            |url| url.trim_start_matches("http://").to_string(),
        );
        ui::footer(c, &hint);
    }

    fn key(&mut self, key: UiKey, ctx: &Ctx) -> Nav {
        let key = list_key(key);
        if self.menu.key(key) {
            return Nav::Stay;
        }
        let display = self.labels.borrow().display.is_some();
        if key == UiKey::Enter && display && self.menu.selected == 2 {
            return Nav::Action(Action::Display);
        }
        let selected = self.menu.selected - usize::from(display && self.menu.selected > 2);
        match key {
            UiKey::Back => Nav::Action(Action::Resume),
            UiKey::Enter => match selected {
                0 => Nav::Action(Action::Resume),
                1 => Nav::Action(Action::CycleView),
                2 => Nav::Action(Action::CtrlAltDel),
                3 => {
                    self.cd = true;
                    Nav::Push(Box::new(Browser::new(
                        "Insert a CD image",
                        &ctx.iso,
                        &["iso"],
                        Some("Eject"),
                    )))
                }
                4 => Nav::Action(Action::Remote),
                5 => Nav::Push(Box::new(Help)),
                6 => Nav::Action(Action::PowerButton),
                7 => Nav::Action(Action::Reset),
                _ => Nav::Push(Box::new(Confirm::new(
                    "Turn off now?",
                    "Like pulling the plug: unsaved work in the guest is lost. Prefer the power button.".into(),
                    "Turn off",
                ))),
            },
            _ => Nav::Stay,
        }
    }

    fn reply(&mut self, reply: Option<Reply>, _ctx: &Ctx) -> Nav {
        let cd = std::mem::replace(&mut self.cd, false);
        match reply {
            Some(Reply::Path(path)) if cd => Nav::Action(Action::Cd(Some(path))),
            Some(Reply::NoFile) if cd => Nav::Action(Action::Cd(None)),
            Some(Reply::Yes) => Nav::Action(Action::TurnOff),
            _ => Nav::Stay,
        }
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn optional_display_keeps_the_other_menu_actions() {
        let ctx = Ctx {
            library: PathBuf::new(),
            share: PathBuf::new(),
            vhd: PathBuf::new(),
            iso: PathBuf::new(),
            memory_limit: 256,
            menu_keys: "Fn+Esc",
            help: &[],
        };
        for display in [None, Some(false), Some(true)] {
            let labels = Rc::new(RefCell::new(OverlayLabels {
                display,
                ..OverlayLabels::default()
            }));
            let mut overlay = VmOverlay::new(labels);
            let extra = usize::from(display.is_some());
            assert_eq!(overlay.menu.items.len(), 9 + extra);
            if let Some(active) = display {
                assert_eq!(overlay.menu.items[2].label, "Display");
                assert_eq!(
                    overlay.menu.items[2].detail,
                    if active { "HDMI" } else { "LCD" }
                );
                overlay.menu.selected = 2;
                assert!(matches!(
                    overlay.key(UiKey::Enter, &ctx),
                    Nav::Action(Action::Display)
                ));
            }
            overlay.menu.selected = 2 + extra;
            assert!(matches!(
                overlay.key(UiKey::Enter, &ctx),
                Nav::Action(Action::CtrlAltDel)
            ));
            overlay.menu.selected = 3 + extra;
            assert!(matches!(overlay.key(UiKey::Enter, &ctx), Nav::Push(_)));
            assert!(matches!(
                overlay.reply(Some(Reply::NoFile), &ctx),
                Nav::Action(Action::Cd(None))
            ));
            overlay.menu.selected = 4 + extra;
            assert!(matches!(
                overlay.key(UiKey::Enter, &ctx),
                Nav::Action(Action::Remote)
            ));
            overlay.menu.selected = 5 + extra;
            assert_eq!(overlay.menu.items[overlay.menu.selected].label, "Help");
            assert!(matches!(overlay.key(UiKey::Enter, &ctx), Nav::Push(_)));
            overlay.menu.selected = 6 + extra;
            assert!(matches!(
                overlay.key(UiKey::Enter, &ctx),
                Nav::Action(Action::PowerButton)
            ));
            overlay.menu.selected = 7 + extra;
            assert!(matches!(
                overlay.key(UiKey::Enter, &ctx),
                Nav::Action(Action::Reset)
            ));
            overlay.menu.selected = 8 + extra;
            assert!(matches!(overlay.key(UiKey::Enter, &ctx), Nav::Push(_)));
            assert!(matches!(
                overlay.reply(Some(Reply::Yes), &ctx),
                Nav::Action(Action::TurnOff)
            ));
        }
    }
}
