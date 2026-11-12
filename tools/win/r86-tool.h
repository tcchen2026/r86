#ifndef R86_TOOL_H
#define R86_TOOL_H
#include <windows.h>

/* Mechanics do not print or reboot. Code 0 succeeds; 3010 succeeds and needs
 * a reboot. Other codes are Windows errors or the distinct missing package
 * result below. The GUI can display message directly, including that result. */
#define R86_NEEDS_VBEMP 0x20000001UL
struct tool_result { DWORD code; char message[512]; };

DWORD r86_install(struct tool_result *result);
DWORD r86_uninstall(struct tool_result *result);
DWORD r86_start_helper(struct tool_result *result);
DWORD r86_stop_helper(struct tool_result *result);
/* NULL/empty directory searches CD drives. A directory is a package root or
 * the folder containing the original INF and driver files. parent owns any
 * Windows driver-copy/signature dialogs; it may be NULL for the CLI. */
DWORD r86_install_display(HWND parent, const char *directory, struct tool_result *result);
#endif
