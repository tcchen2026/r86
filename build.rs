use std::env;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
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
