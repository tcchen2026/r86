## Optional Thumb-2 (Cortex-M) JIT checks

The [Thumb-2 backend](../src/x86/jit/t32/mod.rs) (feature `jit-t32`) targets
Armv8-M Mainline cores such as the Cortex-M33. With the feature, a 32-bit ARM
build uses it in place of the ARMv6K backend. The assembler and the block index
have unit tests that run on the build machine; they hold every emitter to the
encoding `llvm-mc -triple=thumbv8m.main-none-eabi` produces:

```sh
CARGO_TARGET_DIR=tmp/target-t32 cargo test --release --features jit-t32 --lib t32::
```

Block differential and whole-Machine tests run in an ARMv6 musl process under
user-mode QEMU's Cortex-A7 model, whose T32 includes every encoding used. The
ARM11 model of the ARMv6K checks cannot run this code:

```sh
CARGO_TARGET_DIR=tmp/target-t32 RUSTFLAGS='-C linker=rust-lld' \
CARGO_TARGET_ARM_UNKNOWN_LINUX_MUSLEABIHF_RUNNER='qemu-arm -cpu cortex-a7' \
  cargo test --release --target arm-unknown-linux-musleabihf --features jit-t32 \
  --lib -- t32:: arm_jit
```

The whole-boot check is the ARMv6K one above with `--features jit-t32` on the
CLI build and `qemu-arm -cpu cortex-a7`. Apart from the engine's statistics
line, the output of `--cpu-engine jit` and `--cpu-engine interp` must be
identical, including the step count in `serial expectation matched after N
steps`. The bare-metal library is compile-checked only:

```sh
CARGO_TARGET_DIR=tmp/target-t32 cargo build --release --target thumbv8m.main-none-eabihf \
  --no-default-features --features machine,jit,jit-t32 --lib
```

No Cortex-M device has run this backend. A port must define
`r86_t32_code_region` and `r86_t32_code_publish`
([arena.rs](../src/x86/jit/t32/arena.rs)). QEMU timing says nothing about a
microcontroller; compare engines by the statistics line (`invocations`,
`resolved`, `memory_exits`, `compiled_blocks`), not by seconds.
