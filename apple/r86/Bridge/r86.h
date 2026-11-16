// C API of the r86 library for the Apple apps. Implemented in
// src/frontend/apple/mod.rs (Rust, feature `apple`); keep both in step.
//
// Library calls return NULL on success or an error string. Every returned
// string is freed with r86_string_free. Strings are UTF-8.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct R86Vm R86Vm;

typedef struct {
    uint32_t state;  // 0 running, 1 paused, 2 off, 3 failed
    uint64_t instructions_per_second;
} R86Status;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint64_t serial;
} R86Frame;

// Library: VMs are folders holding vm.toml under `root`.
char *r86_library_list(const char *root);  // JSON array; engine: auto|jit|interpreter
char *r86_presets(void);                   // JSON array; diskChoices, dosSetup per preset
char *r86_memory_choices(void);            // JSON array of MiB
char *r86_detect_iso(const char *path);    // preset name or ""
char *r86_detect_disk(const char *path);   // preset name or ""
char *r86_library_suggest_name(const char *root, const char *base);
// New VMs boot from the disk first; a blank disk falls through to the CD.
char *r86_library_create(const char *root, const char *name, const char *os,
                         uint32_t memory_mib, uint64_t disk_mib, const char *iso);
// The new disk holds FreeDOS from share/dos.zip set up for `setup`: "freedos",
// or "win31" | "win95" | "win98" | "winme" to run Windows setup from `iso` on
// first boot. Blocks for seconds; call it off the main thread.
char *r86_library_create_dos(const char *root, const char *name, const char *os,
                             uint32_t memory_mib, uint64_t disk_mib, const char *iso,
                             const char *share, const char *setup);
// Around an existing disk image, used in place.
char *r86_library_create_existing(const char *root, const char *name, const char *os,
                                  uint32_t memory_mib, const char *disk);
// Transfer a stopped VM. disk_out must not exist; the caller owns its parent
// directory during the copy. *config is portable vm.toml (relative disk name,
// no ISO or firmware paths). Import moves disk into a fresh folder under root
// and returns that folder in *directory; failures keep the incoming disk.
// Both block: call off the main thread. NULL or error string; output is NULL
// on failure. Free all returned strings with r86_string_free.
char *r86_library_export(const char *directory, const char *disk_out, char **config);
char *r86_library_import(const char *root, const char *config, const char *disk,
                         char **directory);
// cpu-engine: auto|jit|interpreter (legacy interp accepted). Changes a stopped VM.
// auto selects JIT whenever available, regardless of the OS preset.
// Native builds without the JIT interpret auto and jit VMs.
// New VMs default to auto. --no-jit overrides launch without changing the file.
char *r86_library_set(const char *directory, const char *key, const char *value);
char *r86_library_delete(const char *directory);
void r86_string_free(char *s);

// Running VMs. Each owns its core or web-host thread.
// A fatal error's report goes to logs/<VM folder>.log.
// Recommended resolution for a fresh tools install, at 16 bpp. Use 0,0 for none.
R86Vm *r86_vm_start(const char *directory, const char *firmware, const char *logs,
                    uint16_t recommended_width, uint16_t recommended_height,
                    char **error);
// Web engine (iOS): the page owns CPU/RAM/devices/screen; this library
// owns disks and serves them on 127.0.0.1. Selects auto or jit with at most
// 512 MiB. NULL with *error == NULL: call r86_vm_start.
R86Vm *r86_vm_start_web(const char *directory, const char *firmware, const char *logs,
                        uint16_t recommended_width, uint16_t recommended_height,
                        char **error);
char *r86_vm_web_url(const R86Vm *vm);  // NULL for a native VM
// 0 waiting, 1 page runs the machine, 2 unsupported (probe failed or no page
// within 20 s): free the VM and call r86_vm_start. Native VMs return 0.
uint32_t r86_vm_web_state(const R86Vm *vm);
// Web VMs use the same commands/input/status/messages/logs. Frame reads report
// size/serial only and return false (NULL pixels, capacity 0 is safe); audio
// reads return 0, volume is ignored. CD change/remote report unavailable once.
void r86_vm_free(R86Vm *vm);  // turn off now, flush disks
// 0 pause, 1 resume, 2 reset, 3 power button, 4 Ctrl+Alt+Del, 5 release input
void r86_vm_command(const R86Vm *vm, uint32_t command);
void r86_vm_key(const R86Vm *vm, uint16_t mac_key_code, bool down);
// USB HID keyboard page 7 usage (UIKeyboardHIDUsage); unknown usages ignored.
void r86_vm_hid_key(const R86Vm *vm, uint16_t usage, bool down);
void r86_vm_mouse(const R86Vm *vm, int32_t dx, int32_t dy, uint32_t buttons);
// Absolute 0..65535 across the screen, for the guest's r86 tools helper while
// r86_vm_pointer_active; wheel steps, positive scrolls down.
void r86_vm_pointer(const R86Vm *vm, uint16_t x, uint16_t y, uint32_t buttons, int16_t wheel);
bool r86_vm_pointer_active(const R86Vm *vm);
void r86_vm_change_cd(const R86Vm *vm, const char *path);  // NULL ejects
void r86_vm_set_volume(const R86Vm *vm, uint32_t percent);
void r86_vm_set_remote(const R86Vm *vm, bool on);  // iOS LAN with code; macOS loopback
char *r86_vm_remote_url(const R86Vm *vm);  // includes code when needed; NULL while off
char *r86_vm_log(const R86Vm *vm);  // the fatal error report, NULL without one
void r86_vm_status(const R86Vm *vm, R86Status *status);
char *r86_vm_take_message(const R86Vm *vm);  // NULL when none
bool r86_vm_frame(const R86Vm *vm, uint64_t have_serial, uint8_t *pixels,
                  size_t capacity, R86Frame *frame);
size_t r86_vm_audio_read(const R86Vm *vm, int16_t *samples, size_t frames);
