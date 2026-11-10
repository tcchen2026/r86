// C API of the r86 library for the Mac app. Implemented in
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
char *r86_library_list(const char *root);  // JSON array
char *r86_presets(void);                   // JSON array
char *r86_detect_iso(const char *path);    // preset name or ""
char *r86_library_suggest_name(const char *root, const char *base);
char *r86_library_create(const char *root, const char *name, const char *os,
                         uint32_t memory_mib, uint64_t disk_mib, const char *iso);
char *r86_library_set(const char *directory, const char *key, const char *value);
char *r86_library_delete(const char *directory);
void r86_string_free(char *s);

// Running VMs. Each owns one core thread.
R86Vm *r86_vm_start(const char *directory, const char *firmware, char **error);
void r86_vm_free(R86Vm *vm);  // turn off now, flush disks
// 0 pause, 1 resume, 2 reset, 3 power button, 4 Ctrl+Alt+Del, 5 release input
void r86_vm_command(const R86Vm *vm, uint32_t command);
void r86_vm_key(const R86Vm *vm, uint16_t mac_key_code, bool down);
void r86_vm_mouse(const R86Vm *vm, int32_t dx, int32_t dy, uint32_t buttons);
void r86_vm_change_cd(const R86Vm *vm, const char *path);  // NULL ejects
void r86_vm_set_volume(const R86Vm *vm, uint32_t percent);
void r86_vm_status(const R86Vm *vm, R86Status *status);
char *r86_vm_take_message(const R86Vm *vm);  // NULL when none
bool r86_vm_frame(const R86Vm *vm, uint64_t have_serial, uint8_t *pixels,
                  size_t capacity, R86Frame *frame);
size_t r86_vm_audio_read(const R86Vm *vm, int16_t *samples, size_t frames);
