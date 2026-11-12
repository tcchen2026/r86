use std::fs;

use toml::Value;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Flag,
    Number,
    Text,
    Repeated,
}

fn kind(key: &str) -> Option<Kind> {
    Some(match key {
        "no-cdrom"
        | "no-vga-bios"
        | "hdd-writable"
        | "hdd-frozen"
        | "no-jit"
        | "usb-tablet"
        | "do-not-exit-when-poweroff"
        | "host-decompress-kernel"
        | "guest-decompress-initrd"
        | "sdl2"
        | "term"
        | "sdl2-grab-mouse"
        | "minimal-ring"
        | "debug-ring"
        | "perf-counters"
        | "print-frame-hash" => Kind::Flag,
        "size-mib" | "mem-mib" | "steps" | "ws-slice-steps" | "sdl2-slice-steps" | "sdl2-scale" => {
            Kind::Number
        }
        "serial-input" | "serial-input-file" | "serial-command" | "bootlocal-command"
        | "initrd-file" | "initrd-tree" | "net-forward" => Kind::Repeated,
        "create-vhd" | "iso" | "tools-iso" | "bootsector" | "boot-linux-kernel"
        | "boot-linux-initrd" | "bios" | "vga-bios" | "hdd" | "firmware-display" | "cmdline"
        | "serial-input-after" | "expect-serial" | "dump-initrd" | "dump-prepared-iso"
        | "ws-listen" | "ws-record" | "audio-wav" | "net" | "replay" | "expect-frame-hash"
        | "cpu-engine" | "boot-order" | "clock" => Kind::Text,
        _ => return None,
    })
}

// These spellings feed the same runtime value. A CLI replacement must discard
// the configured sources before the ordinary parser reads any of their files.
fn group(key: &str) -> &str {
    match key {
        "no-cdrom" => "iso",
        "no-vga-bios" => "vga-bios",
        "serial-input-file" | "serial-command" => "serial-input",
        "initrd-tree" => "initrd-file",
        _ => key,
    }
}

struct Setting {
    key: String,
    args: Vec<String>,
}

impl Setting {
    fn value(key: &str, value: String) -> Self {
        Self {
            key: key.to_string(),
            args: vec![format!("--{key}"), value],
        }
    }

    fn flag(key: &str, enabled: bool) -> Self {
        // Keep false settings so they still replace TOML and earlier CLI flags.
        Self {
            key: key.to_string(),
            args: if enabled {
                vec![format!("--{key}")]
            } else {
                Vec::new()
            },
        }
    }
}

pub fn arguments(args: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let mut args = args;
    let mut config_path = None;
    let mut cli: Vec<Setting> = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(vec!["--help".to_string()]);
        }
        let (name, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
        let key = name
            .strip_prefix("--")
            .ok_or_else(|| format!("unknown argument {name}"))?;
        let mut value = || {
            inline
                .map(str::to_string)
                .or_else(|| args.next())
                .ok_or_else(|| format!("{name} requires a value"))
        };
        if key == "config" {
            if config_path.is_some() {
                return Err("--config may only be specified once".to_string());
            }
            config_path = Some(value()?);
            continue;
        }
        let kind = kind(key).ok_or_else(|| format!("unknown argument {name}"))?;
        let setting = if kind == Kind::Flag {
            let enabled = inline
                .unwrap_or("true")
                .parse::<bool>()
                .map_err(|_| format!("{name} expects true or false (for example {name}=false)"))?;
            Setting::flag(key, enabled)
        } else {
            Setting::value(key, value()?)
        };
        if kind != Kind::Repeated {
            cli.retain(|previous| group(&previous.key) != group(key));
        }
        cli.push(setting);
    }

    let mut merged = Vec::new();
    if let Some(path) = config_path {
        let input = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read config {path}: {err}"))?;
        let table = input
            .parse::<toml::Table>()
            .map_err(|err| format!("invalid TOML config {path}: {err}"))?;
        if table.contains_key("vga-bios") && table.contains_key("no-vga-bios") {
            return Err(format!(
                "config {path}: specify either vga-bios or no-vga-bios"
            ));
        }
        if table.contains_key("iso") && table.contains_key("no-cdrom") {
            return Err(format!("config {path}: specify either iso or no-cdrom"));
        }
        for (key, value) in table {
            let settings =
                config_setting(&key, value).map_err(|err| format!("config {path}: {err}"))?;
            if !cli.iter().any(|setting| group(&setting.key) == group(&key)) {
                merged.extend(settings.into_iter().flat_map(|setting| setting.args));
            }
        }
    }
    merged.extend(cli.into_iter().flat_map(|setting| setting.args));
    Ok(merged)
}

fn config_setting(key: &str, value: Value) -> Result<Vec<Setting>, String> {
    let kind = kind(key).ok_or_else(|| format!("unknown option {key:?}"))?;
    let expected = match kind {
        Kind::Flag => "a boolean",
        Kind::Number => "a non-negative integer",
        Kind::Text => "a string",
        Kind::Repeated => "a string or an array of strings",
    };
    let invalid = || format!("{key} expects {expected}");
    Ok(match (kind, value) {
        (Kind::Flag, Value::Boolean(value)) => vec![Setting::flag(key, value)],
        (Kind::Number, Value::Integer(value)) if value >= 0 => {
            vec![Setting::value(key, value.to_string())]
        }
        (Kind::Text | Kind::Repeated, Value::String(value)) => vec![Setting::value(key, value)],
        (Kind::Repeated, Value::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => Ok(Setting::value(key, value)),
                _ => Err(invalid()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(invalid()),
    })
}
