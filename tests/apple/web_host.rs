//! Linux entry for the iOS web engine. Usage:
//! cargo run --no-default-features --features apple --example web_host --
//!   VM_DIRECTORY FIRMWARE LOGS [--writable]
//! Frozen by default. stdin takes the same control/key/mouse/pointer messages
//! as the page, plus status, cd, remote, stop. Keys also accept named PC keys.
//! Commands enter Running::send, exactly as the C API does.
#![allow(dead_code)]
#[path = "../../src/frontend/disk_files.rs"]
mod disk_files;
#[path = "../../src/frontend/guest_os.rs"]
mod guest_os;
#[path = "../../src/frontend/keys.rs"]
mod keys;
#[path = "../../src/frontend/media.rs"]
mod media;
#[path = "../../src/frontend/protocol.rs"]
mod protocol;
#[path = "../../src/frontend/run_log.rs"]
mod run_log;
#[path = "../../src/frontend/runner.rs"]
mod runner;
#[path = "../../src/frontend/vm_store.rs"]
mod vm_store;
#[path = "../../src/frontend/web_host.rs"]
mod web_host;
#[path = "../../src/frontend/ws.rs"]
mod ws;
#[path = "../../src/frontend/ws_frontend.rs"]
mod ws_frontend;

use r86::devices::Ps2ButtonState;
use runner::{Command, Running, lock};
use std::io::{BufRead, Write};
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 3 || args.len() > 4 || (args.len() == 4 && args[3] != "--writable") {
        return Err("usage: web_host VM_DIRECTORY FIRMWARE LOGS [--writable]".into());
    }
    let vm = vm_store::Vm::load(PathBuf::from(&args[0]))?;
    let running = Running::start_web(
        vm,
        args[1].clone().into(),
        args[2].clone().into(),
        Some((936, 666, 16)),
        args.len() == 3,
    )?;
    println!("{}", running.web_url().unwrap());
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let fields: Vec<_> = line.split_whitespace().collect();
        let number = |i: usize| {
            fields
                .get(i)
                .and_then(|s| s.parse::<i32>().ok())
                .ok_or("Missing number")
        };
        let command = match fields.first().copied().unwrap_or("") {
            "stop" => break,
            "status" => {
                let mut status = lock(&running.shared.status);
                let frame = lock(&running.shared.frame);
                println!(
                    "native {} {} {} {} {} {}",
                    running.web_state(),
                    status.state as u32,
                    status.ips,
                    frame.width,
                    frame.height,
                    frame.serial
                );
                println!("{}", running.web_report());
                for message in status.messages.drain(..) {
                    println!("message {message}");
                }
                if let Some(path) = &status.log {
                    println!("log {}", path.display());
                }
                println!("end-status");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                continue;
            }
            "control" => match number(1)? {
                0 => Command::Pause(true),
                1 => Command::Pause(false),
                2 => Command::Reset,
                3 => Command::PowerButton,
                4 => Command::CtrlAltDel,
                5 => Command::ReleaseInput,
                _ => return Err("Unknown control".into()),
            },
            "key" => {
                let value = fields.get(1).ok_or("Missing key")?;
                let key = value
                    .parse::<u16>()
                    .ok()
                    .and_then(keys::InputKey::from_wire)
                    .or_else(|| keys::parse_named_key(value).ok())
                    .ok_or("Unknown key")?;
                Command::Key(key, number(2)? != 0)
            }
            "pointer" => Command::Pointer(
                number(1)? as u16,
                number(2)? as u16,
                number(3)? as u8,
                number(4)? as i16,
            ),
            "mouse" => {
                let buttons = number(3)?;
                Command::Mouse(
                    number(1)?,
                    number(2)?,
                    Ps2ButtonState {
                        left: buttons & 1 != 0,
                        right: buttons & 2 != 0,
                        middle: buttons & 4 != 0,
                    },
                )
            }
            "cd" => Command::Cd(None),
            "remote" => Command::Remote(true),
            _ => return Err("Unknown command".into()),
        };
        running.send(command);
    }
    // Drop joins the native owner, which stops the page and flushes/unlocks.
    Ok(())
}
