use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    // Packaging scripts set R86_VERSION (script/current-tree-version.sh,
    // v2026.11.18-b0dd54); other builds report Cargo's version.
    println!("cargo::rerun-if-env-changed=R86_VERSION");
    let version = env::var("R86_VERSION")
        .unwrap_or_else(|_| format!("v{}-dev", env::var("CARGO_PKG_VERSION").unwrap()));
    println!("cargo::rustc-env=R86_VERSION={version}");
    if env::var_os("CARGO_FEATURE_WIN32_FRONTEND").is_some()
        && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
    {
        windows_resources(&version);
    }
    println!("cargo::rustc-check-cfg=cfg(a64_jit)");
    if env::var_os("CARGO_FEATURE_HOST").is_some()
        && env::var_os("CARGO_FEATURE_JIT").is_some()
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
        && matches!(
            env::var("CARGO_CFG_TARGET_OS").as_deref(),
            Ok("linux" | "macos")
        )
    {
        println!("cargo::rustc-cfg=a64_jit");
    }
}

/// Compiles r86-win.exe's icon, manifest, dialogs and version
/// (src/frontend/win32/r86-win.rc) with the target's MinGW windres, or $WINDRES.
fn windows_resources(version: &str) {
    let dir = PathBuf::from("src/frontend/win32");
    for file in ["r86-win.rc", "r86-win.manifest", "r86.ico"] {
        println!("cargo::rerun-if-changed={}", dir.join(file).display());
    }
    println!("cargo::rerun-if-env-changed=WINDRES");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let windres = env::var("WINDRES").unwrap_or_else(|_| format!("{arch}-w64-mingw32-windres"));
    // FILEVERSION takes the date: v2026.11.08-... is 2026,11,8,0 (a leading
    // zero would make the resource compiler read octal).
    let numbers: Vec<String> = version
        .trim_start_matches('v')
        .split(['.', '-'])
        .take(3)
        .map(|n| n.parse::<u16>().expect("R86_VERSION starts with three numbers").to_string())
        .collect();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("r86-win-res.o");
    let status = Command::new(&windres)
        .current_dir(&dir)
        .arg(format!("-DR86_FILEVERSION={},0", numbers.join(",")))
        .arg(format!("-DR86_VERSION=\\\"{version}\\\""))
        .args(["-O", "coff", "-i", "r86-win.rc", "-o"])
        .arg(&out)
        .status()
        .unwrap_or_else(|err| panic!("{windres} (MinGW resource compiler): {err}"));
    assert!(status.success(), "{windres} failed on r86-win.rc");
    println!("cargo::rustc-link-arg-bin=r86-win={}", out.display());
}
