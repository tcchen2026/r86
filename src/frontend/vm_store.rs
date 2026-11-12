//! Persistent VM definitions (`<library>/<name>/vm.toml`), independent of any
//! UI. A VM owns its directory; its disk may live inside it or elsewhere.
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use r86::FirmwareDisplay;
use r86::disk::Disk;
use r86::machine::{BootOrder, DEFAULT_JIT, Machine};
use toml::{Table, Value};

use super::guest_os::OsPreset;

pub const CONFIG: &str = "vm.toml";
pub const BIOS: &str = "bios.bin";

#[derive(Clone, Debug)]
pub struct Vm {
    pub directory: PathBuf,
    pub name: String,
    pub os: String,
    pub memory: u32,
    pub disk: PathBuf,
    pub disk_mib: u64,
    pub iso: Option<PathBuf>,
    pub cdrom: bool,
    pub usb_tablet: bool,
    pub engine: String,
    pub boot_order: String,
    pub bios: Option<PathBuf>,
    pub vga_bios: Option<PathBuf>,
    /// How graphics fill the screen: "aspect", "stretch" or "integer".
    pub scale: String,
    /// Sound playback level in percent; 0 mutes.
    pub volume: u8,
    /// User-mode NAT networking (DHCP, DNS, outbound TCP).
    pub network: bool,
    pub notes: String,
}

/// The directory holding bios.bin and vgabios-stdvga.bin: beside the
/// executable, or `../share/r86` for packaged installs.
pub fn firmware_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let dir = exe.parent().ok_or("Invalid executable path")?;
    [dir.to_path_buf(), dir.join("../share/r86")]
        .into_iter()
        .find(|dir| dir.join(BIOS).is_file())
        .ok_or_else(|| {
            format!(
                "{BIOS} not found beside {} or in ../share/r86",
                exe.display()
            )
        })
}

/// VMs in a library, sorted by name. Unreadable definitions are returned as
/// errors so the UI can show them instead of hiding them.
pub fn list(root: &Path) -> Vec<Result<Vm, String>> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut vms: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(CONFIG).is_file())
        .map(|path| {
            let label = path.file_name().unwrap_or_default().to_string_lossy();
            Vm::load(path.clone()).map_err(|err| format!("{label}: {err}"))
        })
        .collect();
    vms.sort_by_key(|vm| match vm {
        Ok(vm) => vm.name.to_lowercase(),
        Err(err) => err.to_lowercase(),
    });
    vms
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim() != name
        || name.is_empty()
        || name.len() > 120
        || matches!(name, "." | "..")
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return Err("Use a name of 1–120 characters, without path separators or special filename characters.".into());
    }
    Ok(())
}

pub fn name_exists(root: &Path, name: &str) -> bool {
    // Also avoid names differing only in case, so a library can move to other filesystems.
    fs::read_dir(root).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().to_lowercase() == name.to_lowercase())
    })
}

/// `base`, or `base 2`, `base 3`, ... if that name is taken.
pub fn suggested_name(root: &Path, base: &str) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                base.to_string()
            } else {
                format!("{base} {n}")
            }
        })
        .find(|name| !name_exists(root, name))
        .unwrap()
}

pub fn lock_vm(directory: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("runtime.lock"))
        .map_err(|err| format!("Cannot open the VM lock: {err}"))?;
    file.try_lock().map_err(|err| match err {
        TryLockError::WouldBlock => {
            "This virtual machine is already running or being edited.".into()
        }
        TryLockError::Error(err) => format!("Cannot lock the virtual machine: {err}"),
    })?;
    Ok(file)
}

impl Vm {
    /// A new definition with the disk inside the VM directory.
    pub fn new(root: &Path, name: String, preset: &OsPreset) -> Self {
        let directory = root.join(&name);
        Self {
            disk: directory.join(format!("{name}.vhd")),
            directory,
            name,
            os: preset.name.into(),
            memory: preset.memory_mib,
            disk_mib: preset.disk_mib,
            iso: None,
            cdrom: true,
            usb_tablet: false,
            engine: if DEFAULT_JIT { "jit" } else { "interp" }.into(),
            boot_order: "disk".into(),
            bios: None,
            vga_bios: None,
            scale: "aspect".into(),
            volume: 100,
            network: true,
            notes: String::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        if !(16..=2048).contains(&self.memory) {
            return Err("Memory must be between 16 and 2048 MiB.".into());
        }
        if !(1..=131072).contains(&self.disk_mib) {
            return Err("Disk capacity must be between 1 MiB and 128 GiB.".into());
        }
        if !matches!(
            self.engine.as_str(),
            "auto" | "interp" | "interpreter" | "jit"
        ) {
            return Err("Unknown CPU engine.".into());
        }
        if !matches!(self.boot_order.as_str(), "disk" | "cdrom") {
            return Err("Unknown boot order.".into());
        }
        if !matches!(self.scale.as_str(), "aspect" | "stretch" | "integer") {
            return Err("Unknown screen scaling.".into());
        }
        if self.volume > 100 {
            return Err("Volume must be between 0 and 100.".into());
        }
        if self.disk.as_os_str().is_empty() {
            return Err("Choose a hard disk location.".into());
        }
        if !self.cdrom && self.iso.is_some() {
            return Err("Enable the CD-ROM drive before choosing an ISO.".into());
        }
        Ok(())
    }

    pub fn load(directory: PathBuf) -> Result<Self, String> {
        let text = fs::read_to_string(directory.join(CONFIG)).map_err(|err| err.to_string())?;
        let table: Table = text
            .parse()
            .map_err(|err| format!("Invalid VM configuration: {err}"))?;
        if table.get("version").and_then(Value::as_integer) != Some(1) {
            return Err("Unsupported VM configuration version.".into());
        }
        let text = |key: &str| -> Result<String, String> {
            table
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("Missing or invalid {key}"))
        };
        let number = |key: &str| -> Result<u64, String> {
            table
                .get(key)
                .and_then(Value::as_integer)
                .and_then(|n| n.try_into().ok())
                .ok_or_else(|| format!("Missing or invalid {key}"))
        };
        let path = |key: &str| -> Result<Option<PathBuf>, String> {
            match table.get(key) {
                None => Ok(None),
                Some(Value::String(value)) if !value.is_empty() => {
                    let p = PathBuf::from(value);
                    Ok(Some(if p.is_absolute() {
                        p
                    } else {
                        directory.join(p)
                    }))
                }
                _ => Err(format!("Invalid {key} path")),
            }
        };
        let vm = Self {
            name: text("name")?,
            os: text("os")?,
            memory: number("memory-mib")?
                .try_into()
                .map_err(|_| "Memory value is too large")?,
            disk_mib: number("disk-mib")?,
            disk: path("disk")?.ok_or("Missing disk path")?,
            iso: path("iso")?,
            bios: path("bios")?,
            vga_bios: path("vga-bios")?,
            cdrom: table
                .get("cdrom")
                .and_then(Value::as_bool)
                .ok_or("Missing cdrom setting")?,
            usb_tablet: match table.get("usb-tablet") {
                None => false,
                Some(Value::Boolean(value)) => *value,
                _ => return Err("Invalid usb-tablet setting".into()),
            },
            engine: text("cpu-engine")?,
            boot_order: text("boot-order")?,
            scale: match table.get("scale") {
                None => "aspect".into(),
                Some(_) => text("scale")?,
            },
            volume: match table.get("volume") {
                None => 100,
                Some(_) => number("volume")?.try_into().map_err(|_| "Invalid volume")?,
            },
            network: match table.get("network") {
                None => true,
                Some(value) => value.as_bool().ok_or("Invalid network setting")?,
            },
            notes: text("notes")?,
            directory,
        };
        vm.validate()?;
        Ok(vm)
    }

    pub fn save(&self) -> Result<(), String> {
        self.validate()?;
        let _lock = lock_vm(&self.directory)?;
        self.write_config()
    }

    /// Save while the caller already holds this VM's lock.
    pub fn save_locked(&self) -> Result<(), String> {
        self.validate()?;
        self.write_config()
    }

    fn write_config(&self) -> Result<(), String> {
        let mut table = Table::new();
        for (key, value) in [
            ("name", &self.name),
            ("os", &self.os),
            ("cpu-engine", &self.engine),
            ("boot-order", &self.boot_order),
            ("scale", &self.scale),
            ("notes", &self.notes),
        ] {
            table.insert(key.into(), Value::String(value.clone()));
        }
        for (key, value) in [
            ("version", 1),
            ("memory-mib", u64::from(self.memory)),
            ("disk-mib", self.disk_mib),
            ("volume", u64::from(self.volume)),
        ] {
            table.insert(key.into(), Value::Integer(value as i64));
        }
        table.insert("cdrom".into(), Value::Boolean(self.cdrom));
        table.insert("network".into(), Value::Boolean(self.network));
        table.insert("usb-tablet".into(), Value::Boolean(self.usb_tablet));
        for (key, path) in [
            ("disk", Some(&self.disk)),
            ("iso", self.iso.as_ref()),
            ("bios", self.bios.as_ref()),
            ("vga-bios", self.vga_bios.as_ref()),
        ] {
            if let Some(path) = path {
                let stored = path.strip_prefix(&self.directory).unwrap_or(path);
                table.insert(
                    key.into(),
                    Value::String(stored.to_str().ok_or("Paths must use valid UTF-8")?.into()),
                );
            }
        }
        let temp = self.directory.join("vm.toml.new");
        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&temp)?;
            file.write_all(
                toml::to_string_pretty(&table)
                    .map_err(std::io::Error::other)?
                    .as_bytes(),
            )?;
            file.sync_all()?;
            fs::rename(&temp, self.directory.join(CONFIG))?;
            File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result.map_err(|err| format!("Cannot save virtual machine: {err}"))
    }

    /// Create the VM with a new empty dynamic VHD of `disk_mib`.
    pub fn create_blank(&self) -> Result<(), String> {
        self.create_with(|vm| {
            Disk::create_vhd(&vm.disk, vm.disk_mib * 1024 * 1024)
                .map_err(|err| format!("Cannot create disk: {err}"))
        })
    }

    /// Create the VM around an existing disk image, used in place.
    pub fn create_existing(&self) -> Result<(), String> {
        self.create_with(|vm| {
            Disk::open(&vm.disk, false)
                .map(drop)
                .map_err(|err| format!("Cannot open existing disk: {err}"))
        })
    }

    /// Reserve the VM directory, let `make_disk` create or check the disk,
    /// then save. On failure the new directory and its contents are removed.
    pub fn create_with(
        &self,
        make_disk: impl FnOnce(&Vm) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate()?;
        let root = self.directory.parent().ok_or("Invalid VM directory")?;
        fs::create_dir_all(root).map_err(|err| format!("Cannot create VM library: {err}"))?;
        if name_exists(root, &self.name) {
            return Err("That virtual machine name is already in use. Choose another name.".into());
        }
        // Atomic reservation catches another manager creating the same name.
        fs::create_dir(&self.directory)
            .map_err(|err| format!("Cannot create VM directory: {err}"))?;
        let result = make_disk(self).and_then(|()| self.save());
        if result.is_err() {
            let _ = fs::remove_dir_all(&self.directory);
        }
        result
    }

    /// Remove the VM directory, including a disk stored inside it. A disk
    /// elsewhere (an existing image the VM was created from) is kept.
    pub fn delete(&self) -> Result<(), String> {
        let _lock = lock_vm(&self.directory)?;
        fs::remove_dir_all(&self.directory)
            .map_err(|err| format!("Cannot delete virtual machine: {err}"))
    }

    /// Build a powered-on PC for this VM. Unless the VM names its own ROMs,
    /// firmware comes from `firmware`: BIOS plus the frontend's `vga_bios`
    /// (frontends may ship a VGA BIOS with extra modes for their screen).
    /// The caller holds the VM lock for the machine's lifetime and flushes
    /// its disks when done.
    pub fn machine(&self, firmware: &Path, vga_bios: &str) -> Result<Machine, String> {
        self.validate()?;
        let read = |path: &Path| {
            fs::read(path).map_err(|err| format!("Cannot read {}: {err}", path.display()))
        };
        let mut machine =
            Machine::new_firmware(self.memory as usize * 1024 * 1024, FirmwareDisplay::Vga)?;
        if self.usb_tablet {
            machine.attach_usb_tablet();
        }
        machine.set_boot_order(if self.boot_order == "cdrom" {
            BootOrder::CdromFirst
        } else {
            BootOrder::DiskFirst
        });
        machine.load_bios_rom(&read(self.bios.as_deref().unwrap_or(&firmware.join(BIOS)))?)?;
        machine.load_option_rom(
            0x000c_0000,
            &read(self.vga_bios.as_deref().unwrap_or(&firmware.join(vga_bios)))?,
        )?;
        let disk = Disk::open(&self.disk, true)
            .map_err(|err| format!("Cannot open disk {}: {err}", self.disk.display()))?;
        machine.attach_disk(disk);
        if self.cdrom {
            match &self.iso {
                Some(iso) => {
                    let file = File::open(iso)
                        .map_err(|err| format!("Cannot open CD image {}: {err}", iso.display()))?;
                    machine
                        .attach_file_cdrom(file, false)
                        .map_err(|err| err.to_string())?;
                }
                None => machine.attach_secondary_cdrom(Vec::new()),
            }
        }
        machine.set_jit_enabled(self.jit_enabled())?;
        machine.use_realtime_clock()?;
        machine.reset_bios();
        Ok(machine)
    }

    /// Resolve the saved preference without rewriting it. A host without the
    /// JIT runs every VM interpreted; VMs made on the handheld say "jit".
    pub fn jit_enabled(&self) -> bool {
        DEFAULT_JIT
            && match self.engine.as_str() {
                "jit" => true,
                "auto" => matches!(
                    super::guest_os::by_name(&self.os).id,
                    "win31" | "win95" | "win98" | "winme" | "win2000" | "winxp" | "reactos"
                ),
                _ => false,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::super::guest_os::PRESETS;
    use super::*;

    #[test]
    fn names_disks_settings_and_locks_survive_reopen_without_overwriting() {
        let root = std::env::temp_dir().join(format!("r86-vm-store-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let name = suggested_name(&root, "Windows 98");
        let mut vm = Vm::new(&root, name.clone(), &PRESETS[2]);
        vm.disk_mib = 8;
        vm.notes = "quotes \" and newlines\n中文".into();
        vm.scale = "integer".into();
        vm.volume = 25;
        vm.network = false;
        vm.usb_tablet = true;
        vm.create_blank().unwrap();
        let original = fs::read(&vm.disk).unwrap();
        assert!(vm.create_blank().is_err());
        assert_eq!(fs::read(&vm.disk).unwrap(), original);
        assert_eq!(suggested_name(&root, "Windows 98"), "Windows 98 2");
        assert!(name_exists(&root, &name.to_uppercase()));
        let reopened = Vm::load(vm.directory.clone()).unwrap();
        assert_eq!(reopened.disk, vm.disk);
        assert_eq!(reopened.notes, vm.notes);
        assert_eq!(reopened.scale, "integer");
        assert_eq!((reopened.volume, reopened.network), (25, false));
        assert!(reopened.usb_tablet);
        assert!(reopened.iso.is_none());
        let lock = lock_vm(&vm.directory).unwrap();
        assert!(vm.save().is_err());
        assert!(vm.delete().is_err());
        drop(lock);
        vm.memory = 32;
        vm.save().unwrap();
        let listed = list(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].as_ref().unwrap().memory, 32);
        vm.delete().unwrap();
        assert!(list(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_names_cannot_escape_the_library() {
        for name in [
            "",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            " spaced ",
            "line\nbreak",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }
}
