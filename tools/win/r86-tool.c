/* r86-tool: guest tools for r86 Windows VMs (95 to XP).
 *
 *   r86-tool modes              list every display mode Windows offers
 *   r86-tool mode W H [BITS]    switch the desktop to W x H at BITS (16 by
 *                               default) and keep it for the next boot
 *   r86-tool version            print tool and magic instruction versions
 *   r86-tool autorun            the logon helper: absolute pointer plus a
 *                               notification-area icon (settings, pointer, exit)
 *   r86-tool mouse              run the absolute pointer helper, hidden
 *   r86-tool mouse stop         stop the running pointer helper
 *   r86-tool mouse install      start this executable at the user's logon
 *   r86-tool mouse remove       remove that Run entry and stop the helper
 *   r86-tool install            copy the helper/docs to C:\R86 and start it
 *   r86-tool uninstall          stop it, remove its all-user Run entry/files
 *   r86-tool driver display [CDROOT]  install this OS's tools-CD VGA driver
 * No arguments: C:\R86\R86-TOOL.EXE opens the settings window, inside the
 * running autorun helper when there is one, else as the helper with its icon;
 * any other copy (the tools CD) opens the installer window, whose Install
 * button reads Update once C:\R86 holds a copy. It is a GUI program, so none
 * shows a console. Commands print to a redirected output, the parent console
 * (AttachConsole, XP), or a new console that waits for Enter before closing.
 * cmd.exe does not wait for GUI programs: use `start /wait` for the exit code.
 *
 * The Display control panel hides modes smaller than 640x480, such as the
 * handheld VGA BIOS's 640x340; EnumDisplaySettings and ChangeDisplaySettings
 * do not.
 *
 * Plain C with no C runtime, importing Win32 calls present
 * since Windows 95 and NT 4. Build with MinGW:
 *
 *   i686-w64-mingw32-gcc -Os -s -nostdlib -ffreestanding \
 *       -fno-asynchronous-unwind-tables -Wall -Wextra -o r86-tool.exe \
 *       tools/win/r86-tool.c -e _start -Wl,--subsystem,windows:4.0 \
 *       -Wl,--major-os-version,4 -Wl,--minor-os-version,0 -lkernel32 -luser32 \
 *       -lgdi32 -ladvapi32
 */
#include <windows.h>
#define USE_SP_DRVINFO_DATA_V1 1
#include <setupapi.h>
#include <shlobj.h>
#include "r86-tool.h"

static HANDLE out;
static int own_console;

/* Constants mirror src/machine/magic.rs, the protocol authority. */
#define MAGIC_PORT 0x5286
#define MAGIC_VALUE 0x21363872
#define MAGIC_CPUID 0x40000000
#define MAGIC_VERSION 0
#define MAGIC_POINTER 2
#define MAGIC_REQUEST 3
#define MAGIC_REPORT 4
#define MAGIC_CAP_POINTER 2
#define MAGIC_CAP_REQUESTS 4
#define TOOL_VERSION 3

struct magic_regs { DWORD a, b, c, d, s, i; };

static int r86_present(void)
{
    DWORD before, after, a, b, c, d;
    /* 386/early 486 guests may not implement CPUID. Restore EFLAGS. */
    __asm__ volatile("pushfl; popl %0; movl %0,%1; xorl $0x200000,%1; "
                     "pushl %1; popfl; pushfl; popl %1; pushl %0; popfl"
                     : "=&r"(before), "=&r"(after) : : "cc");
    if (!((before ^ after) & 0x200000))
        return 0;
    a = MAGIC_CPUID;
    c = 0;
    __asm__ volatile("cpuid" : "+a"(a), "=b"(b), "+c"(c), "=d"(d));
    return a == MAGIC_CPUID && b == 0x72363872 && c == 0x38723638 && d == 0x36387236;
}

static struct magic_regs magic_call(DWORD command, DWORD b, DWORD s, DWORD i)
{
    struct magic_regs r = { MAGIC_VALUE, b, command, MAGIC_PORT, s, i };
    __asm__ volatile("inl %%dx,%%eax"
                     : "+a"(r.a), "+b"(r.b), "+c"(r.c), "+d"(r.d), "+S"(r.s), "+D"(r.i));
    return r;
}

/* Output for commands; the installer window and the helper never print. */
static void open_output(void)
{
    BOOL (WINAPI *attach)(DWORD);
    DWORD type;

    out = GetStdHandle(STD_OUTPUT_HANDLE);
    type = out && out != INVALID_HANDLE_VALUE ? GetFileType(out) : FILE_TYPE_UNKNOWN;
    if (type == FILE_TYPE_DISK || type == FILE_TYPE_PIPE)
        return;
    attach = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "AttachConsole");
    if (!attach || !attach((DWORD)-1)) {
        AllocConsole();
        own_console = 1;
    }
    out = CreateFileA("CONOUT$", GENERIC_WRITE, FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
}

static void say(const char *format, ...)
{
    char text[1024];
    va_list args;
    DWORD written;
    int length;

    if (!out)
        open_output();
    va_start(args, format);
    length = wvsprintfA(text, format, args);
    va_end(args);
    WriteFile(out, text, (DWORD)length, &written, NULL);
}

/* GCC may emit memset calls for structure initialisation. */
void *memset(void *to, int value, size_t count)
{
    unsigned char *p = to;
    while (count--)
        *p++ = (unsigned char)value;
    return to;
}

static const char *spaces(const char *s)
{
    while (*s == ' ' || *s == '\t')
        s++;
    return s;
}

/* Copies the next word of the command line into `word`; returns the rest. */
static const char *next_word(const char *s, char *word, int size)
{
    int n = 0, quoted;

    s = spaces(s);
    quoted = *s == '"';
    if (quoted) s++;
    for (; *s && (quoted ? *s != '"' : *s != ' ' && *s != '\t'); s++)
        if (n < size - 1)
            word[n++] = *s;
    if (quoted && *s == '"') s++;
    word[n] = 0;
    return s;
}

static const char *number(const char *s, DWORD *value)
{
    s = spaces(s);
    if (*s < '0' || *s > '9')
        return NULL;
    *value = 0;
    while (*s >= '0' && *s <= '9')
        *value = *value * 10 + (DWORD)(*s++ - '0');
    return s;
}

static int list_modes(void)
{
    DEVMODEA mode;
    DWORD i;

    for (i = 0;; i++) {
        memset(&mode, 0, sizeof mode);
        mode.dmSize = sizeof mode;
        if (!EnumDisplaySettingsA(NULL, i, &mode))
            return 0;
        say("%lu x %lu, %lu bit, %lu Hz\r\n", mode.dmPelsWidth, mode.dmPelsHeight,
            mode.dmBitsPerPel, mode.dmDisplayFrequency);
    }
}

/* Switches and saves the mode; returns the DISP_CHANGE code, with `text`. */
static LONG change_mode(DWORD width, DWORD height, DWORD bits, const char **text)
{
    DEVMODEA mode;
    LONG code;

    memset(&mode, 0, sizeof mode);
    mode.dmSize = sizeof mode;
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmBitsPerPel = bits;
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
    code = ChangeDisplaySettingsA(&mode, CDS_UPDATEREGISTRY);
    switch (code) {
    case DISP_CHANGE_SUCCESSFUL: *text = "switched"; break;
    case DISP_CHANGE_RESTART: *text = "saved; restart Windows to use it"; break;
    case DISP_CHANGE_BADMODE: *text = "Windows does not offer this mode"; break;
    case DISP_CHANGE_NOTUPDATED: *text = "switched, but not saved in the registry"; break;
    default: *text = "failed"; break;
    }
    return code;
}

static int set_mode(DWORD width, DWORD height, DWORD bits)
{
    const char *text;
    LONG code = change_mode(width, height, bits, &text);
    say("%lu x %lu, %lu bit: %s\r\n", width, height, bits, text);
    return code == DISP_CHANGE_SUCCESSFUL || code == DISP_CHANGE_RESTART ? 0 : 1;
}

/* Windows 9x offers a display driver's modes from its class key, where
 * VMDisp9x's INF lists them (MODES\BITS\WIDTH,HEIGHT). Adds a mode to every
 * VMDisp9x QEMU adapter key; returns 1 if one was missing. */
static int add_9x_mode(DWORD width, DWORD height, DWORD bits)
{
    static const char display[] = "System\\CurrentControlSet\\Services\\Class\\Display";
    char name[32], path[96], drv[32];
    HKEY root, key;
    DWORD i, size, made;
    int ours, added = 0;

    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, display, 0, KEY_READ | KEY_CREATE_SUB_KEY, &root) != ERROR_SUCCESS)
        return 0;
    for (i = 0; RegEnumKeyA(root, i, name, sizeof name) == ERROR_SUCCESS; i++) {
        wsprintfA(path, "%s\\DEFAULT", name);
        if (RegOpenKeyExA(root, path, 0, KEY_QUERY_VALUE, &key) != ERROR_SUCCESS)
            continue;
        size = sizeof drv;
        ours = RegQueryValueExA(key, "drv", NULL, NULL, (BYTE *)drv, &size) == ERROR_SUCCESS &&
               !lstrcmpiA(drv, "qemumini.drv");
        RegCloseKey(key);
        if (!ours)
            continue;
        wsprintfA(path, "%s\\MODES\\%lu\\%lu,%lu", name, bits, width, height);
        if (RegCreateKeyExA(root, path, 0, NULL, 0, KEY_WRITE, NULL, &key, &made) == ERROR_SUCCESS) {
            added |= made == REG_CREATED_NEW_KEY;
            RegCloseKey(key);
        }
    }
    RegCloseKey(root);
    return added;
}

/* The handheld panel's 320x170 and twice it, which VMDisp9x's INF lacks. */
static void add_panel_modes(void)
{
    static const DWORD sizes[][2] = {{320, 170}, {640, 340}}, depths[] = {8, 16, 32};
    unsigned i, j;

    for (i = 0; i < 2; i++)
        for (j = 0; j < 3; j++)
            add_9x_mode(sizes[i][0], sizes[i][1], depths[j]);
}

static int version(void)
{
    struct magic_regs r;
    say("r86-tool %u\r\n", TOOL_VERSION);
    if (!r86_present()) {
        say("r86 magic instruction is not available\r\n");
        return 1;
    }
    r = magic_call(MAGIC_VERSION, 0, 0, 0);
    say("r86 magic instruction protocol %lu, capabilities %08lx\r\n", r.a, r.b);
    return r.a == 1 ? 0 : 1;
}

/* Loaded only on NT, so missing NT desktop exports cannot prevent Win95 startup. */
static HDESK (WINAPI *open_input_desktop)(DWORD, BOOL, ACCESS_MASK);
static BOOL (WINAPI *close_desktop)(HDESK);
static BOOL (WINAPI *object_info)(HANDLE, int, PVOID, DWORD, LPDWORD);
static char own_desktop[256];
static int nt;

static int prepare_desktop(void)
{
    HDESK (WINAPI *thread_desktop)(DWORD);
    HMODULE user = GetModuleHandleA("user32.dll");
    DWORD needed;
    if (!nt)
        return 1;
    open_input_desktop = (void *)GetProcAddress(user, "OpenInputDesktop");
    close_desktop = (void *)GetProcAddress(user, "CloseDesktop");
    object_info = (void *)GetProcAddress(user, "GetUserObjectInformationA");
    thread_desktop = (void *)GetProcAddress(user, "GetThreadDesktop");
    return open_input_desktop && close_desktop && object_info && thread_desktop &&
        object_info(thread_desktop(GetCurrentThreadId()), UOI_NAME,
                    own_desktop, sizeof own_desktop, &needed);
}

static int input_desktop(void)
{
    HDESK desk;
    char name[256];
    DWORD needed;
    int same;
    if (!nt)
        return 1;
    desk = open_input_desktop(0, FALSE, DESKTOP_READOBJECTS);
    if (!desk)
        return 0;
    same = object_info(desk, UOI_NAME, name, sizeof name, &needed) &&
        !lstrcmpA(name, own_desktop);
    close_desktop(desk);
    return same;
}

static const char stop_name[] = "r86-tool.mouse.stop";
static const char mutex_name[] = "r86-tool.mouse.owner";
static const char pid_name[] = "r86-tool.mouse.pid";

static void stop_mouse(void)
{
    HANDLE event = OpenEventA(EVENT_MODIFY_STATE, FALSE, stop_name);
    if (event) {
        SetEvent(event);
        CloseHandle(event);
    }
}

static DWORD buttons;               /* held guest buttons; the polling thread's */
static volatile LONG pointer_on = 1; /* the icon menu's and settings' toggle */
static int helper_running;          /* this process is the pointer helper */

static void release_buttons(void)
{
    DWORD flags = 0;
    if (buttons & 1) flags |= MOUSEEVENTF_LEFTUP;
    if (buttons & 2) flags |= MOUSEEVENTF_RIGHTUP;
    if (buttons & 4) flags |= MOUSEEVENTF_MIDDLEUP;
    if (flags && input_desktop())
        mouse_event(flags, 0, 0, 0, 0);
    buttons = 0;
}

static int mouse_startup(int install)
{
    const char *key = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    char path[MAX_PATH], command[MAX_PATH + 16];
    HKEY run;
    LONG status;
    DWORD length;
    if (install) {
        length = GetModuleFileNameA(NULL, path, sizeof path);
        if (!length || length >= sizeof path)
            return 1;
        wsprintfA(command, "\"%s\" mouse", path);
        status = RegCreateKeyExA(HKEY_CURRENT_USER, key, 0, NULL, 0, KEY_SET_VALUE,
                                NULL, &run, NULL);
    } else {
        stop_mouse();
        status = RegOpenKeyExA(HKEY_CURRENT_USER, key, 0, KEY_SET_VALUE, &run);
        if (status == ERROR_FILE_NOT_FOUND)
            return 0;
    }
    if (status != ERROR_SUCCESS) {
        say("Run entry: error %ld\r\n", status);
        return 1;
    }
    status = install ? RegSetValueExA(run, "r86 Mouse", 0, REG_SZ,
                                   (const BYTE *)command, lstrlenA(command) + 1)
                     : RegDeleteValueA(run, "r86 Mouse");
    RegCloseKey(run);
    if (!install && status == ERROR_FILE_NOT_FOUND)
        status = ERROR_SUCCESS;
    say("r86 Mouse Run entry: %s (%ld)\r\n", install ? "install" : "remove", status);
    return status == ERROR_SUCCESS ? 0 : 1;
}

/* Dialog-facing mechanics and result contract: r86-tool.h. */
static const char install_dir[] = "C:\\R86";
static const char install_exe[] = "C:\\R86\\R86-TOOL.EXE";
static const char autorun_command[] = "\"C:\\R86\\R86-TOOL.EXE\" autorun";
static const char run_key[] = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
static const char vga_id[] = "PCI\\VEN_1234&DEV_1111";
static const char vbemp_message[] =
    "Windows 2000/XP need the VBEMP display driver. Its license allows "
    "only unmodified, non-commercial, standalone distribution, so r86 "
    "cannot include it with its Apache-2.0 tools. Download it from "
    "https://r86.run/vbemp, attach it as a CD or folder, and run "
    "r86-tool again to install it.";

static DWORD result(struct tool_result *r, DWORD code, const char *message)
{
    r->code = code;
    lstrcpynA(r->message, message, sizeof r->message);
    return code;
}

static DWORD failure(struct tool_result *r, const char *operation, DWORD code)
{
    if (!code) code = ERROR_GEN_FAILURE;
    r->code = code;
    wsprintfA(r->message, "%s: Windows error %lu", operation, code);
    return code;
}

static int join_path(char *path, const char *dir, const char *leaf)
{
    int n = lstrlenA(dir), m = lstrlenA(leaf);
    if (n + m + 2 > MAX_PATH) return 0;
    lstrcpyA(path, dir);
    if (n && path[n-1] != '\\' && path[n-1] != '/') path[n++] = '\\';
    lstrcpyA(path + n, leaf);
    return 1;
}

static int image_path(char *path)
{
    DWORD n = GetModuleFileNameA(NULL, path, MAX_PATH);
    return n && n < MAX_PATH;
}

static void parent_dir(char *path)
{
    int n = lstrlenA(path);
    while (n && path[n-1] != '\\' && path[n-1] != '/') n--;
    path[n] = 0;
}

static int installed_image(void)
{
    char path[MAX_PATH], full[MAX_PATH];
    DWORD n;
    if (!image_path(path)) return 0;
    n = GetFullPathNameA(path, sizeof full, full, NULL);
    return n && n < sizeof full && !lstrcmpiA(full, install_exe);
}

static int regular_file(const char *path)
{
    DWORD attr = GetFileAttributesA(path);
    return attr != INVALID_FILE_ATTRIBUTES && !(attr & FILE_ATTRIBUTE_DIRECTORY);
}

static DWORD startup_entry(HKEY hive, const char *name, const char *command)
{
    HKEY key;
    LONG code;
    if (command)
        code = RegCreateKeyExA(hive, run_key, 0, NULL, 0, KEY_SET_VALUE, NULL, &key, NULL);
    else
        code = RegOpenKeyExA(hive, run_key, 0, KEY_SET_VALUE, &key);
    if (!command && code == ERROR_FILE_NOT_FOUND) return ERROR_SUCCESS;
    if (code != ERROR_SUCCESS) return code;
    code = command ? RegSetValueExA(key, name, 0, REG_SZ, (const BYTE *)command,
                                   lstrlenA(command) + 1)
                   : RegDeleteValueA(key, name);
    RegCloseKey(key);
    return !command && code == ERROR_FILE_NOT_FOUND ? ERROR_SUCCESS : code;
}

DWORD r86_stop_helper(struct tool_result *r)
{
    HANDLE mutex = OpenMutexA(SYNCHRONIZE, FALSE, mutex_name);
    HANDLE mapping, process;
    const DWORD *pid;
    DWORD wait, code = mutex ? 0 : GetLastError();
    if (!mutex) {
        stop_mouse(); /* Also stops an older, event-only helper. */
        if (code != ERROR_FILE_NOT_FOUND && code != ERROR_INVALID_NAME)
            return failure(r, "Open helper mutex", code);
        return result(r, 0, "Helper is not running");
    }
    CloseHandle(mutex);
    mapping = OpenFileMappingA(FILE_MAP_READ, FALSE, pid_name);
    pid = mapping ? MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, sizeof(DWORD)) : NULL;
    if (!pid || !*pid) {
        if (pid) UnmapViewOfFile(pid);
        if (mapping) CloseHandle(mapping);
        return result(r, ERROR_BUSY, "Helper is starting or exiting; try again when it has finished");
    }
    process = OpenProcess(SYNCHRONIZE, FALSE, *pid);
    code = process ? 0 : GetLastError();
    UnmapViewOfFile(pid);
    CloseHandle(mapping);
    if (!process) return failure(r, "Open helper process", code);
    stop_mouse();
    wait = WaitForSingleObject(process, 5000);
    code = wait == WAIT_FAILED ? GetLastError() : ERROR_TIMEOUT;
    CloseHandle(process);
    if (wait != WAIT_OBJECT_0)
        return failure(r, "Wait for helper to stop", code);
    return result(r, 0, "Helper stopped");
}

DWORD r86_start_helper(struct tool_result *r)
{
    STARTUPINFOA startup;
    PROCESS_INFORMATION process;
    char command[sizeof autorun_command];
    lstrcpyA(command, autorun_command);
    memset(&startup, 0, sizeof startup);
    startup.cb = sizeof startup;
    if (!CreateProcessA(install_exe, command, NULL, NULL, FALSE, DETACHED_PROCESS,
                        NULL, install_dir, &startup, &process))
        return failure(r, "Start installed helper", GetLastError());
    CloseHandle(process.hThread);
    CloseHandle(process.hProcess);
    return result(r, 0, "Installed helper started");
}

DWORD r86_install(struct tool_result *r)
{
    const char *names[] = {"R86-TOOL.EXE", "README.TXT", "LICENSE.TXT"};
    char image[MAX_PATH], dir[MAX_PATH], source[MAX_PATH], dest[MAX_PATH];
    DWORD code;
    unsigned i;
    if (!image_path(image)) return failure(r, "Get running image path", ERROR_FILENAME_EXCED_RANGE);
    lstrcpyA(dir, image);
    parent_dir(dir);
    for (i = 1; i < 3; i++) {
        if (!join_path(source, dir, names[i]) || !regular_file(source))
            return result(r, ERROR_FILE_NOT_FOUND, "Keep README.TXT and LICENSE.TXT beside R86-TOOL.EXE");
    }
    code = r86_stop_helper(r);
    if (code) return code;
    if (!CreateDirectoryA(install_dir, NULL) && GetLastError() != ERROR_ALREADY_EXISTS)
        return failure(r, "Create C:\\R86", GetLastError());
    if (!installed_image()) {
        for (i = 0; i < 3; i++) {
            if (!i) lstrcpyA(source, image);
            else join_path(source, dir, names[i]);
            join_path(dest, install_dir, names[i]);
            if (regular_file(dest)) SetFileAttributesA(dest, FILE_ATTRIBUTE_NORMAL);
            if (!CopyFileA(source, dest, FALSE)) return failure(r, "Copy installed helper files", GetLastError());
            if (!SetFileAttributesA(dest, FILE_ATTRIBUTE_NORMAL))
                return failure(r, "Set installed file attributes", GetLastError());
        }
    }
    code = startup_entry(HKEY_LOCAL_MACHINE, "r86 Tool", autorun_command);
    if (code) return failure(r, "Register all-user logon helper (administrator required on NT)", code);
    code = startup_entry(HKEY_CURRENT_USER, "r86 Mouse", NULL);
    if (code) return failure(r, "Remove old per-user helper entry", code);
    code = r86_start_helper(r);
    if (code) return code;
    return result(r, 0, "Installed in C:\\R86; helper started and registered for every user's logon");
}

/* Only needed when uninstall is running from the installed EXE itself.
 * Preserve all existing WININIT.INI rename/delete requests on Windows 9x. */
static DWORD delete_after_reboot(const char *path)
{
    if (!(GetVersion() & 0x80000000)) {
        BOOL (WINAPI *move)(LPCSTR, LPCSTR, DWORD);
        move = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "MoveFileExA");
        if (!move) return ERROR_CALL_NOT_IMPLEMENTED;
        return move(path, NULL, MOVEFILE_DELAY_UNTIL_REBOOT) ? 0 : GetLastError();
    } else {
        char windows[MAX_PATH], ini[MAX_PATH], *section;
        DWORD n, code;
        n = GetWindowsDirectoryA(windows, sizeof windows);
        if (!n || n >= sizeof windows || !join_path(ini, windows, "WININIT.INI"))
            return ERROR_FILENAME_EXCED_RANGE;
        /* The 9x profile API's buffer limit is 32767, not 32768. */
        section = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, 32767);
        if (!section) return ERROR_NOT_ENOUGH_MEMORY;
        n = GetPrivateProfileSectionA("Rename", section, 32767, ini);
        if (n + lstrlenA(path) + 6 >= 32766) code = ERROR_INSUFFICIENT_BUFFER;
        else {
            wsprintfA(section + n, "NUL=%s", path);
            section[n + lstrlenA(path) + 5] = 0;
            code = WritePrivateProfileSectionA("Rename", section, ini) ? 0 : GetLastError();
        }
        HeapFree(GetProcessHeap(), 0, section);
        return code;
    }
}

DWORD r86_uninstall(struct tool_result *r)
{
    const char *names[] = {"README.TXT", "LICENSE.TXT", "R86-TOOL.EXE"};
    char path[MAX_PATH];
    DWORD code;
    unsigned i;
    int reboot = 0;
    code = r86_stop_helper(r);
    if (code) return code;
    code = startup_entry(HKEY_LOCAL_MACHINE, "r86 Tool", NULL);
    if (code) return failure(r, "Remove all-user logon entry", code);
    code = startup_entry(HKEY_CURRENT_USER, "r86 Mouse", NULL);
    if (code) return failure(r, "Remove old per-user helper entry", code);
    for (i = 0; i < 3; i++) {
        join_path(path, install_dir, names[i]);
        if (!regular_file(path)) continue;
        SetFileAttributesA(path, FILE_ATTRIBUTE_NORMAL);
        if (!DeleteFileA(path)) {
            if (i != 2 || !installed_image()) return failure(r, "Delete installed file", GetLastError());
            code = delete_after_reboot(path);
            if (code) return failure(r, "Schedule installed EXE deletion", code);
            reboot = 1;
        }
    }
    /* Do not remove unrelated files if C:\R86 also contains user data. */
    RemoveDirectoryA(install_dir);
    return result(r, reboot ? ERROR_SUCCESS_REBOOT_REQUIRED : 0,
                  reboot ? "Helper and logon entry removed; restart to delete the running EXE (directory may remain)"
                         : "Helper, logon entry and installed files removed");
}

static int inf_value(const char *inf, const char *section, const char *key, const char *expected)
{
    char value[128];
    return GetPrivateProfileStringA(section, key, "", value, sizeof value, inf) &&
           !lstrcmpiA(value, expected);
}

static int nt_driver_i386(const char *path)
{
    HANDLE file = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
    IMAGE_DOS_HEADER dos;
    IMAGE_FILE_HEADER coff;
    IMAGE_OPTIONAL_HEADER32 pe;
    DWORD bytes, signature, size;
    int valid = 0;
    if (file == INVALID_HANDLE_VALUE) return 0;
    size = GetFileSize(file, NULL);
    if (size != INVALID_FILE_SIZE && ReadFile(file, &dos, sizeof dos, &bytes, NULL) &&
        bytes == sizeof dos && dos.e_magic == IMAGE_DOS_SIGNATURE && dos.e_lfanew >= (LONG)sizeof dos &&
        (DWORD)dos.e_lfanew < size && size - (DWORD)dos.e_lfanew >= sizeof signature + sizeof coff + sizeof pe &&
        SetFilePointer(file, dos.e_lfanew, NULL, FILE_BEGIN) != INVALID_SET_FILE_POINTER &&
        ReadFile(file, &signature, sizeof signature, &bytes, NULL) && bytes == sizeof signature &&
        signature == IMAGE_NT_SIGNATURE && ReadFile(file, &coff, sizeof coff, &bytes, NULL) &&
        bytes == sizeof coff && coff.Machine == IMAGE_FILE_MACHINE_I386 &&
        ReadFile(file, &pe, sizeof pe, &bytes, NULL) && bytes == sizeof pe &&
        pe.Magic == IMAGE_NT_OPTIONAL_HDR32_MAGIC && pe.Subsystem == IMAGE_SUBSYSTEM_NATIVE)
        valid = 1;
    CloseHandle(file);
    return valid;
}

/* The directory may be the extracted package root, the CD root, or the PNP
 * directory itself. Never modify the supplied INF or install an unrelated one. */
static int find_inf(const char *dir, int is_nt, char *inf)
{
    const char *nt_paths[] = {"vbemppnp.inf", "VBE20\\W2K\\PNP\\vbemppnp.inf",
                             "VBEMP\\VBE20\\W2K\\PNP\\vbemppnp.inf"};
    const char *win9x_paths[] = {"vmdisp9x.inf", "VMDISP9X\\vmdisp9x.inf"};
    unsigned i, count = is_nt ? 3 : 2;
    for (i = 0; i < count; i++)
        if (join_path(inf, dir, is_nt ? nt_paths[i] : win9x_paths[i]) && regular_file(inf)) return 1;
    return 0;
}

static DWORD display_source(const char *directory, int is_nt, char *inf, struct tool_result *r)
{
    char dir[MAX_PATH], driver[MAX_PATH];
    DWORD n, drives;
    int found = 0;
    if (directory && *directory) {
        n = GetFullPathNameA(directory, sizeof dir, dir, NULL);
        if (!n || n >= sizeof dir) return failure(r, "Driver directory path", ERROR_FILENAME_EXCED_RANGE);
        found = find_inf(dir, is_nt, inf);
    } else {
        drives = GetLogicalDrives();
        lstrcpyA(dir, "A:\\");
        for (; dir[0] <= 'Z'; dir[0]++, drives >>= 1) {
            if ((drives & 1) && GetDriveTypeA(dir) == DRIVE_CDROM && find_inf(dir, is_nt, inf)) {
                found = 1;
                break;
            }
        }
    }
    if (!found) return result(r, is_nt ? R86_NEEDS_VBEMP : ERROR_FILE_NOT_FOUND,
                             is_nt ? vbemp_message : "VMDisp9x not found; attach the r86 tools CD or select its VMDISP9X folder");
    if (!inf_value(inf, "Version", "Signature", is_nt ? "$Windows NT$" : "$CHICAGO$") ||
        !inf_value(inf, "Version", "Class", "Display") ||
        (is_nt && (!inf_value(inf, "Version", "ClassGUID", "{4D36E968-E325-11CE-BFC1-08002BE10318}") ||
                   !inf_value(inf, "Vbemp_Service_Inst", "ServiceBinary", "%12%\\Vbemp.sys"))))
        return result(r, ERROR_INVALID_DATA, "The selected INF is not the expected display driver");
    lstrcpyA(dir, inf);
    parent_dir(dir);
    join_path(driver, dir, is_nt ? "vbemp.sys" : "qemumini.drv");
    if (!regular_file(driver)) return result(r, ERROR_FILE_NOT_FOUND, "The driver binary is missing beside its INF; extract the complete original package");
    if (is_nt && !nt_driver_i386(driver))
        return result(r, ERROR_BAD_EXE_FORMAT, "VBEMP must be the original 32-bit x86 NT miniport; select VBE20\\W2K\\PNP");
    return 0;
}

/* SetupAPI is loaded only for installation. V1 driver records are required by
 * 9x. SoftGPU's windrv.c documents the sequence; this uses only r86's PCI ID. */
#define SETUP_FUNCTIONS(X) \
    X(SetupDiGetClassDevsA) X(SetupDiEnumDeviceInfo) X(SetupDiGetDeviceRegistryPropertyA) \
    X(SetupDiGetDeviceInstallParamsA) X(SetupDiSetDeviceInstallParamsA) \
    X(SetupDiBuildDriverInfoList) X(SetupDiEnumDriverInfoA) X(SetupDiGetDriverInfoDetailA) \
    X(SetupDiSetSelectedDriverA) X(SetupDiInstallDevice) X(SetupDiDestroyDeviceInfoList)
#define DECLARE_SETUP(name) static __typeof__(&name) p_##name;
SETUP_FUNCTIONS(DECLARE_SETUP)
#undef DECLARE_SETUP

static HMODULE load_setup(struct tool_result *r)
{
    HMODULE setup = LoadLibraryA("setupapi.dll");
    if (!setup) {
        failure(r, "Load SetupAPI (Windows 95 requires its SetupAPI redistributable)", GetLastError());
        return NULL;
    }
#define LOAD_SETUP(name) p_##name = (void *)GetProcAddress(setup, #name); \
    if (!p_##name) { failure(r, #name, ERROR_PROC_NOT_FOUND); FreeLibrary(setup); return NULL; }
    SETUP_FUNCTIONS(LOAD_SETUP)
#undef LOAD_SETUP
    return setup;
}

static DWORD find_vga(HWND parent, HDEVINFO *devices, SP_DEVINFO_DATA *device, char *hardware_id)
{
    char ids[1024];
    DWORD index, i;
    *devices = p_SetupDiGetClassDevsA(NULL, NULL, parent, DIGCF_PRESENT | DIGCF_ALLCLASSES);
    if (*devices == INVALID_HANDLE_VALUE) return GetLastError();
    memset(device, 0, sizeof *device);
    device->cbSize = sizeof *device;
    for (index = 0; p_SetupDiEnumDeviceInfo(*devices, index, device); index++) {
        memset(ids, 0, sizeof ids);
        if (!p_SetupDiGetDeviceRegistryPropertyA(*devices, device, SPDRP_HARDWAREID,
                                                NULL, (BYTE *)ids, sizeof ids - 1, NULL)) continue;
        for (i = 0; i < sizeof ids - 1 && ids[i]; i += lstrlenA(ids + i) + 1) {
            unsigned j;
            for (j = 0; vga_id[j] && ids[i+j] == vga_id[j]; j++);
            if (!vga_id[j] && (!ids[i+j] || ids[i+j] == '&')) {
                if (hardware_id) lstrcpyA(hardware_id, ids + i);
                return 0;
            }
        }
    }
    return ERROR_NO_SUCH_DEVINST;
}

static DWORD display_9x(HWND parent, const char *inf, struct tool_result *r)
{
    HMODULE setup = load_setup(r);
    HDEVINFO devices = INVALID_HANDLE_VALUE;
    SP_DEVINFO_DATA device;
    SP_DEVINSTALL_PARAMS_A params;
    SP_DRVINFO_DATA_A driver;
    SP_DRVINFO_DETAIL_DATA_A detail;
    DWORD index, length, code;
    const char *operation = "Find PCI VGA (check Device Manager's PCI bus detection)";
    if (!setup) return r->code;
    code = find_vga(parent, &devices, &device, NULL);
    if (code) goto done;
    memset(&params, 0, sizeof params);
    params.cbSize = sizeof params;
    operation = "Read VGA install parameters";
    if (!p_SetupDiGetDeviceInstallParamsA(devices, &device, &params)) { code = GetLastError(); goto done; }
    params.Flags |= DI_ENUMSINGLEINF | DI_NEEDRESTART;
    params.hwndParent = parent;
    lstrcpyA(params.DriverPath, inf);
    operation = "Build VMDisp9x driver list";
    if (!p_SetupDiSetDeviceInstallParamsA(devices, &device, &params) ||
        !p_SetupDiBuildDriverInfoList(devices, &device, SPDIT_CLASSDRIVER)) { code = GetLastError(); goto done; }
    for (index = 0;; index++) {
        memset(&driver, 0, sizeof driver);
        driver.cbSize = sizeof driver;
        operation = "Find VMDisp9x Qemu section";
        if (!p_SetupDiEnumDriverInfoA(devices, &device, SPDIT_CLASSDRIVER, index, &driver)) { code = GetLastError(); goto done; }
        memset(&detail, 0, sizeof detail);
        detail.cbSize = sizeof detail;
        if (!p_SetupDiGetDriverInfoDetailA(devices, &device, &driver, &detail, sizeof detail, &length) &&
            GetLastError() != ERROR_INSUFFICIENT_BUFFER) { code = GetLastError(); goto done; }
        if (!lstrcmpiA(detail.SectionName, "Qemu")) break;
    }
    operation = "Install VMDisp9x QEMU STD VGA";
    if (!p_SetupDiSetSelectedDriverA(devices, &device, &driver) ||
        !p_SetupDiInstallDevice(devices, &device)) { code = GetLastError(); goto done; }
    code = ERROR_SUCCESS_REBOOT_REQUIRED;
    add_panel_modes();
done:
    if (devices != INVALID_HANDLE_VALUE) p_SetupDiDestroyDeviceInfoList(devices);
    FreeLibrary(setup);
    return code == ERROR_SUCCESS_REBOOT_REQUIRED ? result(r, code, "VMDisp9x installed; restart Windows before changing display modes")
                                               : failure(r, operation, code);
}

DWORD r86_install_display(HWND parent, const char *directory, struct tool_result *r)
{
    DWORD os = GetVersion(), code;
    int is_nt = !(os & 0x80000000);
    char inf[MAX_PATH];
    if (is_nt && (LOBYTE(LOWORD(os)) != 5 || HIBYTE(LOWORD(os)) > 1))
        return result(r, ERROR_NOT_SUPPORTED, "Automatic display installation supports Windows 95/98/Me and 32-bit Windows 2000/XP");
    code = display_source(directory, is_nt, inf, r);
    if (code) return code;
    if (!is_nt) return display_9x(parent, inf, r);
    else {
        BOOL (WINAPI *update)(HWND, LPCSTR, LPCSTR, DWORD, PBOOL);
        HMODULE library, setup = load_setup(r);
        HDEVINFO devices;
        SP_DEVINFO_DATA device;
        char hardware_id[1024];
        BOOL reboot = FALSE, ok;
        DEVMODEA mode;
        LONG changed;
        if (!setup) return r->code;
        code = find_vga(parent, &devices, &device, hardware_id);
        if (devices != INVALID_HANDLE_VALUE) p_SetupDiDestroyDeviceInfoList(devices);
        FreeLibrary(setup);
        if (code) return failure(r, "Find r86 PCI VGA", code);
        library = LoadLibraryA("newdev.dll");
        if (!library) return failure(r, "Load newdev.dll", GetLastError());
        update = (void *)GetProcAddress(library, "UpdateDriverForPlugAndPlayDevicesA");
        if (!update) { FreeLibrary(library); return failure(r, "UpdateDriverForPlugAndPlayDevicesA", ERROR_PROC_NOT_FOUND); }
        /* Explicit driver installation may replace the existing generic driver.
         * Passing a reboot pointer prevents newdev from rebooting the guest. */
        /* NT matches a complete HardwareID, not a vendor/device prefix. Use
         * this adapter's enumerated ID, including subsystem/revision suffixes. */
        ok = update(parent, hardware_id, inf, 1 /* INSTALLFLAG_FORCE */, &reboot);
        code = ok ? 0 : GetLastError();
        FreeLibrary(library);
        if (code) return failure(r, "Install VBEMP for PCI\\VEN_1234&DEV_1111", code);
        memset(&mode, 0, sizeof mode);
        mode.dmSize = sizeof mode;
        mode.dmBitsPerPel = 16;
        mode.dmPelsWidth = 640;
        mode.dmPelsHeight = 480;
        mode.dmFields = DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT;
        changed = ChangeDisplaySettingsA(&mode, CDS_UPDATEREGISTRY);
        if (changed != DISP_CHANGE_SUCCESSFUL && changed != DISP_CHANGE_RESTART) {
            r->code = ERROR_INVALID_DATA;
            wsprintfA(r->message, "VBEMP installed, but saving 16-bit colour failed (display code %ld). Do not use 256 colours.", changed);
            return r->code;
        }
        return result(r, reboot || changed == DISP_CHANGE_RESTART ? ERROR_SUCCESS_REBOOT_REQUIRED : 0,
                      reboot || changed == DISP_CHANGE_RESTART ? "VBEMP installed; 16-bit colour saved; restart Windows"
                                                              : "VBEMP installed; display saved at 640 x 480, 16-bit colour");
    }
}

/* Windows: plain controls on fixed windows; the mechanics above do the work
 * and supply the text. A process shows one of them in `window`: the installer
 * (the tools CD copy) or settings (the installed copy and its helper). */
enum { ID_INTRO = 100, ID_HELPER, ID_DISPLAY, ID_VBEMP, ID_CHOOSE, ID_STATUS, ID_UNINSTALL, ID_REFRESH };

static HWND window;
static HICON icon;
static char vbemp_dir[MAX_PATH];
static int display_kind; /* 0: by hand, 1: VMDisp9x, 2: VBEMP */

/* "86" on navy, drawn at run time: the build has no resource compiler. Navy
 * and white are in the 16-colour palette Windows uses before a display driver. */
static HICON r86_icon(void)
{
    HDC screen = GetDC(NULL), dc = CreateCompatibleDC(screen);
    HBITMAP colour = CreateCompatibleBitmap(screen, 16, 16), mask = CreateBitmap(16, 16, 1, 1, NULL);
    HBRUSH navy = CreateSolidBrush(RGB(0, 0, 128));
    ICONINFO info = { TRUE, 0, 0, mask, colour };
    RECT r = {0, 0, 16, 16};
    HGDIOBJ old = SelectObject(dc, mask);
    HICON made;

    PatBlt(dc, 0, 0, 16, 16, BLACKNESS); /* an all-zero mask is opaque */
    SelectObject(dc, colour);
    FillRect(dc, &r, navy);
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, RGB(255, 255, 255));
    SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
    DrawTextA(dc, "86", 2, &r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(dc, old);
    made = CreateIconIndirect(&info);
    DeleteObject(navy);
    DeleteObject(colour);
    DeleteObject(mask);
    DeleteDC(dc);
    ReleaseDC(NULL, screen);
    return made;
}

static int window_class(const char *name, WNDPROC proc)
{
    WNDCLASSA c;

    if (!icon)
        icon = r86_icon();
    memset(&c, 0, sizeof c);
    c.lpfnWndProc = proc;
    c.hInstance = GetModuleHandleA(NULL);
    c.hIcon = icon;
    c.hCursor = LoadCursorA(NULL, IDC_ARROW);
    c.hbrBackground = (HBRUSH)(COLOR_BTNFACE + 1);
    c.lpszClassName = name;
    return RegisterClassA(&c) != 0;
}

/* Window rectangle for a client area, centred in the work area with its
 * caption on screen: the handheld's 640x340 desktop must fit. */
static void centred(DWORD style, int width, int height, RECT *r)
{
    RECT area;

    SetRect(r, 0, 0, width, height);
    AdjustWindowRect(r, style, FALSE);
    SystemParametersInfoA(SPI_GETWORKAREA, 0, &area, 0);
    r->right -= r->left;
    r->bottom -= r->top;
    r->left = area.left + (area.right - area.left - r->right) / 2;
    r->top = area.top + (area.bottom - area.top - r->bottom) / 2;
    if (r->left < area.left) r->left = area.left;
    if (r->top < area.top) r->top = area.top;
}

static void dispatch(MSG *message)
{
    if (!window || !IsDialogMessageA(window, message)) {
        TranslateMessage(message);
        DispatchMessageA(message);
    }
}

static const char *install_word(void)
{
    return regular_file(install_exe) ? "Update" : "Install";
}

/* Install becomes Update, and Uninstall is offered, once C:\R86 has a copy. */
static void show_installed(void)
{
    SetDlgItemTextA(window, IDOK, install_word());
    EnableWindow(GetDlgItem(window, ID_UNINSTALL), regular_file(install_exe));
}

static HWND control(DWORD ex, const char *cls, const char *text, DWORD style, int id,
                    int x, int y, int w, int h)
{
    HWND c = CreateWindowExA(ex, cls, text, WS_CHILD | WS_VISIBLE | style, x, y, w, h,
                             window, (HMENU)(UINT_PTR)id, GetModuleHandleA(NULL), NULL);
    SendMessageA(c, WM_SETFONT, (WPARAM)GetStockObject(DEFAULT_GUI_FONT), FALSE);
    return c;
}

static const char *windows_name(void)
{
    static char name[32];
    OSVERSIONINFOA v;

    memset(&v, 0, sizeof v);
    v.dwOSVersionInfoSize = sizeof v;
    GetVersionExA(&v);
    if (v.dwPlatformId != VER_PLATFORM_WIN32_NT)
        return v.dwMinorVersion >= 90 ? "Windows Me" : v.dwMinorVersion >= 10 ? "Windows 98" : "Windows 95";
    if (v.dwMajorVersion == 5 && v.dwMinorVersion <= 1)
        return v.dwMinorVersion ? "Windows XP" : "Windows 2000";
    wsprintfA(name, "Windows NT %lu.%lu", v.dwMajorVersion, v.dwMinorVersion);
    return name;
}

/* Finds the driver again (CD drives or the chosen folder) and shows the result.
 * On 2000/XP without VBEMP the status explains why r86 does not include it. */
static void show_display(void)
{
    struct tool_result r;
    char inf[MAX_PATH], text[MAX_PATH + 16];
    HWND box = GetDlgItem(window, ID_DISPLAY);
    int found;

    if (!display_kind) {
        EnableWindow(box, FALSE);
        return;
    }
    found = !display_source(vbemp_dir[0] ? vbemp_dir : NULL, display_kind == 2, inf, &r);
    SendMessageA(box, BM_SETCHECK, found ? BST_CHECKED : BST_UNCHECKED, 0);
    if (display_kind == 2) {
        parent_dir(inf);
        wsprintfA(text, "VBEMP: %s", found ? inf : "not found");
        SetDlgItemTextA(window, ID_VBEMP, text);
    }
    wsprintfA(text, "Choose what to install, then click %s. Windows restarts once to start "
                    "the display driver.", install_word());
    SetDlgItemTextA(window, ID_STATUS, found ? text : r.message);
}

static void choose_vbemp(void)
{
    HMODULE shell = LoadLibraryA("shell32.dll");
    LPITEMIDLIST (WINAPI *browse)(LPBROWSEINFOA);
    BOOL (WINAPI *path)(LPCITEMIDLIST, LPSTR);
    HRESULT (WINAPI *get_malloc)(LPMALLOC *);
    BROWSEINFOA info;
    LPITEMIDLIST item;
    LPMALLOC allocator;
    char dir[MAX_PATH];

    if (!shell)
        return;
    browse = (void *)GetProcAddress(shell, "SHBrowseForFolderA");
    path = (void *)GetProcAddress(shell, "SHGetPathFromIDListA");
    get_malloc = (void *)GetProcAddress(shell, "SHGetMalloc");
    if (browse && path && get_malloc) {
        memset(&info, 0, sizeof info);
        info.hwndOwner = window;
        info.pszDisplayName = dir;
        info.lpszTitle = "Choose the VBEMP package folder, or its VBE20\\W2K\\PNP folder.";
        info.ulFlags = BIF_RETURNONLYFSDIRS;
        item = browse(&info);
        if (item) {
            if (path(item, dir))
                lstrcpyA(vbemp_dir, dir);
            if (SUCCEEDED(get_malloc(&allocator))) {
                allocator->lpVtbl->Free(allocator, item);
                allocator->lpVtbl->Release(allocator);
            }
        }
    }
    FreeLibrary(shell);
    show_display();
}

static void restart_windows(void)
{
    HMODULE advapi = GetModuleHandleA("advapi32.dll");
    BOOL (WINAPI *open_token)(HANDLE, DWORD, PHANDLE);
    BOOL (WINAPI *lookup)(LPCSTR, LPCSTR, PLUID);
    BOOL (WINAPI *adjust)(HANDLE, BOOL, PTOKEN_PRIVILEGES, DWORD, PTOKEN_PRIVILEGES, PDWORD);
    TOKEN_PRIVILEGES privilege;
    HANDLE token;

    /* NT needs the shutdown privilege; Windows 9x has no tokens. */
    open_token = (void *)GetProcAddress(advapi, "OpenProcessToken");
    lookup = (void *)GetProcAddress(advapi, "LookupPrivilegeValueA");
    adjust = (void *)GetProcAddress(advapi, "AdjustTokenPrivileges");
    if (!(GetVersion() & 0x80000000) && open_token && lookup && adjust &&
        open_token(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &token)) {
        privilege.PrivilegeCount = 1;
        privilege.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        if (lookup(NULL, SE_SHUTDOWN_NAME, &privilege.Privileges[0].Luid))
            adjust(token, FALSE, &privilege, 0, NULL, NULL);
        CloseHandle(token);
    }
    if (!ExitWindowsEx(EWX_REBOOT, 0))
        MessageBoxA(window, "Windows did not restart. Restart it from the Start menu.",
                    "r86 tools", MB_OK | MB_ICONEXCLAMATION);
}

static int checked(int id)
{
    return SendDlgItemMessageA(window, id, BM_GETCHECK, 0, 0) == BST_CHECKED;
}

static void report(char *text, const char *label, const struct tool_result *r, int *restart)
{
    int n = lstrlenA(text);
    wsprintfA(text + n, "%s%s: %s", n ? "\r\n\r\n" : "", label, r->message);
    *restart |= r->code == ERROR_SUCCESS_REBOOT_REQUIRED;
}

static void run(int install)
{
    static const int buttons[] = {IDOK, ID_UNINSTALL, IDCANCEL, ID_CHOOSE};
    char text[2200];
    struct tool_result r;
    HCURSOR cursor;
    int restart = 0;
    unsigned i;

    text[0] = 0;
    for (i = 0; i < 4; i++)
        EnableWindow(GetDlgItem(window, buttons[i]), FALSE);
    SetDlgItemTextA(window, ID_STATUS, !install ? "Removing..." : regular_file(install_exe) ? "Updating..." : "Installing...");
    UpdateWindow(window);
    cursor = SetCursor(LoadCursorA(NULL, IDC_WAIT));
    if (!install) {
        r86_uninstall(&r);
        report(text, "Helper", &r, &restart);
        lstrcatA(text, "\r\n\r\nThe display driver stays installed; Device Manager can change it.");
    } else {
        if (checked(ID_HELPER)) {
            r86_install(&r);
            report(text, "Helper", &r, &restart);
        }
        if (checked(ID_DISPLAY)) {
            r86_install_display(window, vbemp_dir[0] ? vbemp_dir : NULL, &r);
            report(text, "Display driver", &r, &restart);
        }
        if (!text[0])
            lstrcpyA(text, "Nothing is selected.");
    }
    SetCursor(cursor);
    SetDlgItemTextA(window, ID_STATUS, text);
    for (i = 0; i < 4; i++)
        EnableWindow(GetDlgItem(window, buttons[i]), TRUE);
    show_installed();
    if (restart && MessageBoxA(window, "Restart Windows now to finish?", "r86 tools",
                               MB_YESNO | MB_ICONQUESTION) == IDYES)
        restart_windows();
}

static LRESULT CALLBACK window_proc(HWND hwnd, UINT message, WPARAM w, LPARAM l)
{
    switch (message) {
    case WM_COMMAND:
        switch (LOWORD(w)) {
        case IDOK: run(1); return 0;
        case ID_UNINSTALL: run(0); return 0;
        case ID_CHOOSE: choose_vbemp(); return 0;
        case ID_REFRESH: show_display(); return 0;
        case IDCANCEL: DestroyWindow(hwnd); return 0;
        }
        break;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, message, w, l);
}

static int installer(void)
{
    static const char helper[] = "r86 helper: mouse integration and a taskbar icon, started at logon";
    DWORD style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    DWORD os = GetVersion();
    RECT rect;
    MSG message;
    char text[96];
    int y;

    /* Autorun starts a copy on every insertion: reuse the open window, which
     * looks for the driver on the new disc. */
    window = FindWindowA("r86-tool", NULL);
    if (window) {
        SetForegroundWindow(window);
        PostMessageA(window, WM_COMMAND, ID_REFRESH, 0);
        return 0;
    }
    if (!(os & 0x80000000))
        display_kind = LOBYTE(LOWORD(os)) == 5 && HIBYTE(LOWORD(os)) <= 1 ? 2 : 0;
    else
        display_kind = 1;
    if (!window_class("r86-tool", window_proc))
        return 1;
    centred(style, 440, 246, &rect);
    window = CreateWindowExA(0, "r86-tool", "r86 tools", style, rect.left, rect.top,
                             rect.right, rect.bottom, NULL, NULL, GetModuleHandleA(NULL), NULL);
    if (!window)
        return 1;
    wsprintfA(text, "Install the r86 tools for %s:", windows_name());
    control(0, "STATIC", text, SS_LEFT, ID_INTRO, 12, 10, 416, 16);
    control(0, "BUTTON", helper, BS_AUTOCHECKBOX | WS_TABSTOP, ID_HELPER, 12, 30, 416, 18);
    control(0, "BUTTON", display_kind == 1 ? "Display driver: VMDisp9x, for more resolutions and colours"
                       : display_kind == 2 ? "Display driver: VBEMP, used in 16-bit colour"
                                           : "Display driver: install it by hand on this Windows version",
            BS_AUTOCHECKBOX | WS_TABSTOP, ID_DISPLAY, 12, 50, 416, 18);
    y = 76;
    if (display_kind == 2) {
        control(0, "STATIC", "", SS_LEFT | SS_PATHELLIPSIS, ID_VBEMP, 30, 78, 296, 16);
        control(0, "BUTTON", "Choose folder...", BS_PUSHBUTTON | WS_TABSTOP, ID_CHOOSE, 332, 73, 96, 24);
        y = 104;
    }
    control(WS_EX_CLIENTEDGE, "EDIT", "", ES_MULTILINE | ES_READONLY | WS_VSCROLL,
            ID_STATUS, 12, y, 416, 206 - y);
    control(0, "BUTTON", "Install", BS_DEFPUSHBUTTON | WS_TABSTOP, IDOK, 188, 214, 76, 24);
    control(0, "BUTTON", "Uninstall", BS_PUSHBUTTON | WS_TABSTOP, ID_UNINSTALL, 270, 214, 76, 24);
    control(0, "BUTTON", "Close", BS_PUSHBUTTON | WS_TABSTOP, IDCANCEL, 352, 214, 76, 24);
    wsprintfA(text, "Choose what to install, then click %s.", install_word());
    SetDlgItemTextA(window, ID_STATUS, text);
    SendDlgItemMessageA(window, ID_HELPER, BM_SETCHECK, BST_CHECKED, 0);
    show_installed();
    show_display();
    if (!r86_present()) {
        SetDlgItemTextA(window, ID_STATUS, "This is not an r86 virtual machine. The r86 tools work only inside r86.");
        EnableWindow(GetDlgItem(window, IDOK), FALSE);
    }
    ShowWindow(window, SW_SHOWNORMAL);
    SetFocus(GetDlgItem(window, IsWindowEnabled(GetDlgItem(window, IDOK)) ? IDOK : IDCANCEL));
    while (GetMessageA(&message, NULL, 0, 0) > 0)
        dispatch(&message);
    return 0;
}

/* Settings window: the installed copy's window, in the helper process when
 * there is one, so the pointer toggle acts on the running helper. */
enum { ID_RESOLUTION = 200, ID_APPLY, ID_POINTER, ID_LOGON, ID_SETTINGS, ID_EXIT };
#define WM_TRAY (WM_APP + 1)

static const char settings_class[] = "r86-tool.settings";
static const char tray_class[] = "r86-tool.tray";
static int standalone; /* settings without a helper: closing it ends the process */
struct mode { DWORD width, height, bits; };
static struct mode modes[96];
static HWND tray;
static UINT taskbar_created;
static BOOL (WINAPI *notify_icon)(DWORD, PNOTIFYICONDATAA);

/* Every mode Windows offers, including those under 640x480 that the Display
 * control panel hides; one entry per size and depth, whatever the refresh. */
static int collect_modes(struct mode *list, int limit)
{
    DEVMODEA mode;
    DWORD i;
    int count = 0, j, k;

    for (i = 0; count < limit; i++) {
        memset(&mode, 0, sizeof mode);
        mode.dmSize = sizeof mode;
        if (!EnumDisplaySettingsA(NULL, i, &mode))
            break;
        /* VBEMP, the 2000/XP driver, crashes winlogon in 256 colours. */
        if (nt && mode.dmBitsPerPel == 8)
            continue;
        /* Sorted by width, height, then depth. */
        for (j = 0; j < count && (list[j].width != mode.dmPelsWidth ? list[j].width < mode.dmPelsWidth
                                  : list[j].height != mode.dmPelsHeight ? list[j].height < mode.dmPelsHeight
                                  : list[j].bits < mode.dmBitsPerPel); j++)
            ;
        if (j < count && list[j].width == mode.dmPelsWidth && list[j].height == mode.dmPelsHeight &&
            list[j].bits == mode.dmBitsPerPel)
            continue;
        for (k = count++; k > j; k--)
            list[k] = list[k - 1];
        list[j].width = mode.dmPelsWidth;
        list[j].height = mode.dmPelsHeight;
        list[j].bits = mode.dmBitsPerPel;
    }
    return count;
}

static void fill_modes(void)
{
    HWND list = GetDlgItem(window, ID_RESOLUTION);
    HDC screen = GetDC(NULL);
    DWORD width = (DWORD)GetDeviceCaps(screen, HORZRES), height = (DWORD)GetDeviceCaps(screen, VERTRES);
    DWORD bits = (DWORD)(GetDeviceCaps(screen, BITSPIXEL) * GetDeviceCaps(screen, PLANES));
    char text[48];
    int count = collect_modes(modes, sizeof modes / sizeof modes[0]), j;

    ReleaseDC(NULL, screen);
    SendMessageA(list, CB_RESETCONTENT, 0, 0);
    for (j = 0; j < count; j++) {
        if (modes[j].bits <= 8)
            wsprintfA(text, "%lu x %lu, %lu colours", modes[j].width, modes[j].height, 1UL << modes[j].bits);
        else
            wsprintfA(text, "%lu x %lu, %lu-bit colour", modes[j].width, modes[j].height, modes[j].bits);
        SendMessageA(list, CB_ADDSTRING, 0, (LPARAM)text);
        if (modes[j].width == width && modes[j].height == height && modes[j].bits == bits)
            SendMessageA(list, CB_SETCURSEL, (WPARAM)j, 0);
    }
}

static void apply_mode(void)
{
    LRESULT i = SendDlgItemMessageA(window, ID_RESOLUTION, CB_GETCURSEL, 0, 0);
    const char *result;
    char text[96];

    if (i < 0)
        return;
    change_mode(modes[i].width, modes[i].height, modes[i].bits, &result);
    wsprintfA(text, "%lu x %lu, %lu-bit: %s.", modes[i].width, modes[i].height, modes[i].bits, result);
    SetDlgItemTextA(window, ID_STATUS, text);
}

static int logon_entry(void)
{
    HKEY key;
    int found = 0;

    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, run_key, 0, KEY_QUERY_VALUE, &key) == ERROR_SUCCESS) {
        found = RegQueryValueExA(key, "r86 Tool", NULL, NULL, NULL, NULL) == ERROR_SUCCESS;
        RegCloseKey(key);
    }
    return found;
}

static void set_logon(void)
{
    int on = checked(ID_LOGON);
    DWORD code = startup_entry(HKEY_LOCAL_MACHINE, "r86 Tool", on ? autorun_command : NULL);
    struct tool_result r;

    if (code) {
        failure(&r, on ? "Add the logon entry (administrator required on NT)" : "Remove the logon entry", code);
        SendDlgItemMessageA(window, ID_LOGON, BM_SETCHECK, on ? BST_UNCHECKED : BST_CHECKED, 0);
        SetDlgItemTextA(window, ID_STATUS, r.message);
    } else
        SetDlgItemTextA(window, ID_STATUS, on ? "r86 tools start at every user's logon."
                                              : "r86 tools no longer start at logon.");
}

static void tray_icon(DWORD action)
{
    NOTIFYICONDATAA data;

    if (!tray)
        return;
    memset(&data, 0, sizeof data);
    /* The version 1 layout: Windows 95, 98 and NT 4 shells reject larger ones. */
    data.cbSize = NOTIFYICONDATAA_V1_SIZE;
    data.hWnd = tray;
    data.uID = 1;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    lstrcpyA(data.szTip, pointer_on ? "r86 tools: the mouse follows the host pointer"
                                    : "r86 tools: absolute mouse off");
    /* At logon the taskbar may not exist yet: retry until it does. */
    if (!notify_icon(action, &data) && action == NIM_ADD)
        SetTimer(tray, 1, 2000, NULL);
}

static void set_pointer(int on)
{
    pointer_on = on;
    if (window)
        SendDlgItemMessageA(window, ID_POINTER, BM_SETCHECK, on ? BST_CHECKED : BST_UNCHECKED, 0);
    tray_icon(NIM_MODIFY);
}

static LRESULT CALLBACK settings_proc(HWND hwnd, UINT message, WPARAM w, LPARAM l)
{
    switch (message) {
    case WM_COMMAND:
        switch (LOWORD(w)) {
        case ID_APPLY: apply_mode(); return 0;
        case ID_POINTER: set_pointer(checked(ID_POINTER)); return 0;
        case ID_LOGON: set_logon(); return 0;
        case IDCANCEL: DestroyWindow(hwnd); return 0;
        }
        break;
    case WM_DISPLAYCHANGE: {
        RECT client, r;
        GetClientRect(hwnd, &client);
        centred((DWORD)GetWindowLongA(hwnd, GWL_STYLE), client.right, client.bottom, &r);
        SetWindowPos(hwnd, NULL, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        fill_modes();
        return 0;
    }
    case WM_DESTROY:
        window = NULL;
        if (standalone)
            PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, message, w, l);
}

static void open_settings(void)
{
    DWORD style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    RECT r;
    char text[64];

    if (window) {
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
        return;
    }
    centred(style, 380, 176, &r);
    window = CreateWindowExA(0, settings_class, "r86 tools settings", style, r.left, r.top,
                             r.right, r.bottom, NULL, NULL, GetModuleHandleA(NULL), NULL);
    if (!window)
        return;
    control(0, "STATIC", "Screen:", SS_LEFT, ID_INTRO, 12, 14, 50, 16);
    control(0, "COMBOBOX", "", CBS_DROPDOWNLIST | WS_VSCROLL | WS_TABSTOP, ID_RESOLUTION, 64, 10, 222, 160);
    control(0, "BUTTON", "Apply", BS_PUSHBUTTON | WS_TABSTOP, ID_APPLY, 292, 9, 76, 24);
    control(0, "STATIC", "Includes the modes that the Display control panel hides.", SS_LEFT, ID_INTRO,
            64, 38, 304, 16);
    control(0, "BUTTON", helper_running ? "Mouse follows the host pointer (absolute mouse)"
                                        : "Mouse follows the host pointer (the helper is not running)",
            BS_AUTOCHECKBOX | WS_TABSTOP, ID_POINTER, 12, 62, 356, 18);
    control(0, "BUTTON", "Start r86 tools at logon, with an icon in the taskbar",
            BS_AUTOCHECKBOX | WS_TABSTOP, ID_LOGON, 12, 84, 356, 18);
    control(0, "STATIC", "", SS_LEFT, ID_STATUS, 12, 110, 356, 30);
    control(0, "BUTTON", "Close", BS_PUSHBUTTON | WS_TABSTOP, IDCANCEL, 292, 144, 76, 24);
    fill_modes();
    SendDlgItemMessageA(window, ID_POINTER, BM_SETCHECK, helper_running && pointer_on ? BST_CHECKED : BST_UNCHECKED, 0);
    EnableWindow(GetDlgItem(window, ID_POINTER), helper_running);
    SendDlgItemMessageA(window, ID_LOGON, BM_SETCHECK, logon_entry() ? BST_CHECKED : BST_UNCHECKED, 0);
    wsprintfA(text, "r86-tool %u on %s.", TOOL_VERSION, windows_name());
    SetDlgItemTextA(window, ID_STATUS, text);
    ShowWindow(window, SW_SHOWNORMAL);
    SetForegroundWindow(window);
    SetFocus(GetDlgItem(window, ID_RESOLUTION));
}

static void tray_menu(void)
{
    HMENU menu = CreatePopupMenu();
    POINT at;

    AppendMenuA(menu, MF_STRING, ID_SETTINGS, "&Settings...");
    AppendMenuA(menu, MF_STRING | (pointer_on ? MF_CHECKED : MF_UNCHECKED), ID_POINTER,
                "&Mouse follows the host pointer");
    AppendMenuA(menu, MF_SEPARATOR, 0, NULL);
    AppendMenuA(menu, MF_STRING, ID_EXIT, "E&xit");
    SetMenuDefaultItem(menu, ID_SETTINGS, FALSE);
    GetCursorPos(&at);
    /* Foreground first, and a message after, so the menu closes on an outside click. */
    SetForegroundWindow(tray);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, at.x, at.y, 0, tray, NULL);
    PostMessageA(tray, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

static LRESULT CALLBACK tray_proc(HWND hwnd, UINT message, WPARAM w, LPARAM l)
{
    if (message == WM_TRAY) {
        if (l == WM_LBUTTONUP)
            open_settings();
        else if (l == WM_RBUTTONUP)
            tray_menu();
        return 0;
    }
    if (taskbar_created && message == taskbar_created) { /* Explorer restarted */
        tray_icon(NIM_ADD);
        return 0;
    }
    switch (message) {
    case WM_TIMER:
        KillTimer(hwnd, w);
        tray_icon(NIM_ADD);
        return 0;
    case WM_COMMAND:
        switch (LOWORD(w)) {
        case ID_SETTINGS: open_settings(); return 0;
        case ID_POINTER: set_pointer(!pointer_on); return 0;
        case ID_EXIT: PostQuitMessage(0); return 0;
        }
        break;
    }
    return DefWindowProcA(hwnd, message, w, l);
}

static void make_tray(void)
{
    HMODULE shell = LoadLibraryA("shell32.dll");

    notify_icon = shell ? (void *)GetProcAddress(shell, "Shell_NotifyIconA") : NULL;
    if (!notify_icon || !window_class(tray_class, tray_proc))
        return;
    taskbar_created = RegisterWindowMessageA("TaskbarCreated");
    tray = CreateWindowExA(0, tray_class, "r86 tools", WS_POPUP, 0, 0, 0, 0, NULL, NULL,
                           GetModuleHandleA(NULL), NULL);
    tray_icon(NIM_ADD);
}

static int settings_only(void)
{
    MSG message;

    standalone = 1;
    if (!window_class(settings_class, settings_proc))
        return 1;
    open_settings();
    while (window && GetMessageA(&message, NULL, 0, 0) > 0)
        dispatch(&message);
    return 0;
}

/* The installed copy started by hand: open settings in the autorun helper. */
static int settings_in_helper(void)
{
    HWND helper = FindWindowA(tray_class, NULL);
    BOOL (WINAPI *allow)(DWORD);

    if (!helper)
        return 0;
    allow = (void *)GetProcAddress(GetModuleHandleA("user32.dll"), "AllowSetForegroundWindow");
    if (allow)
        allow(ASFW_ANY);
    PostMessageA(helper, WM_COMMAND, ID_SETTINGS, 0);
    return 1;
}

static HANDLE stop;
static DWORD ui_thread; /* gets WM_QUIT when polling ends by itself */
static int wheel_supported;
static int takes_requests; /* the host offers requests and reports */

/* Tells the host which modes Windows offers (report kinds 1 and 2). */
static void report_modes(void)
{
    struct mode list[96];
    int i, count = collect_modes(list, 96);

    magic_call(MAGIC_REPORT, 1, 0, 0);
    for (i = 0; i < count; i++)
        magic_call(MAGIC_REPORT, 2, list[i].width | list[i].height << 16, list[i].bits);
}

/* A host display request. On 9x a size VMDisp9x does not list is added to
 * its list first; Windows may only offer it after a restart. */
static void display_request(DWORD width, DWORD height, DWORD bits)
{
    const char *text;
    LONG code = change_mode(width, height, bits, &text);
    int added = 0;
    DWORD result;

    if (code == DISP_CHANGE_BADMODE && !nt && add_9x_mode(width, height, bits)) {
        added = 1;
        code = change_mode(width, height, bits, &text);
    }
    result = code == DISP_CHANGE_SUCCESSFUL ? 0 : code == DISP_CHANGE_RESTART ? 1
           : code != DISP_CHANGE_BADMODE ? 3 : added ? 4 : 2;
    magic_call(MAGIC_REPORT, 3, width | height << 16, bits | result << 16);
    report_modes();
}

static void take_requests(void)
{
    struct magic_regs r;

    for (;;) {
        r = magic_call(MAGIC_REQUEST, 0, 0, 0);
        if (!r.a || r.a == 0xffffffff)
            return;
        if (r.a == 1)
            display_request(r.b, r.c, r.d);
        /* Other kinds are newer than this tool: skipped. */
    }
}

/* Polls the host at its hinted interval on a thread of its own, so the icon
 * menu, window drags and other modal loops keep receiving the pointer. With
 * the pointer off it still takes host requests, without renewing the
 * pointer lease. */
static DWORD WINAPI poll_pointer(LPVOID unused)
{
    struct magic_regs r;
    DWORD hint = 0;

    (void)unused;
    if (takes_requests)
        report_modes();
    /* A timed event wait has Sleep's cadence and also permits a clean stop. */
    while (WaitForSingleObject(stop, hint) == WAIT_TIMEOUT) {
        DWORD changed, flags;
        hint = 100;
        if (!pointer_on) {
            release_buttons();
            if (takes_requests)
                take_requests();
            hint = 500;
            continue; /* No pointer poll: the host's PS/2 fallback takes over. */
        }
        if (!input_desktop())
            continue; /* No poll: allow the host's PS/2 fallback lease to expire. */
        r = magic_call(MAGIC_POINTER, 0, 0, 0);
        if (r.a == 0xffffffff)
            break;
        if (takes_requests && r.i)
            take_requests();
        hint = r.s;
        changed = buttons ^ r.c;
        if (!r.a && !changed)
            continue;
        flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE;
        if (changed & 1) flags |= (r.c & 1) ? MOUSEEVENTF_LEFTDOWN : MOUSEEVENTF_LEFTUP;
        if (changed & 2) flags |= (r.c & 2) ? MOUSEEVENTF_RIGHTDOWN : MOUSEEVENTF_RIGHTUP;
        if (changed & 4) flags |= (r.c & 4) ? MOUSEEVENTF_MIDDLEDOWN : MOUSEEVENTF_MIDDLEUP;
        if (wheel_supported && r.d) flags |= MOUSEEVENTF_WHEEL;
        mouse_event(flags, r.b & 0xffff, r.b >> 16,
                    wheel_supported ? (DWORD)((LONG)r.d * WHEEL_DELTA) : 0, 0);
        buttons = r.c;
    }
    release_buttons();
    if (ui_thread)
        PostThreadMessageA(ui_thread, WM_QUIT, 0, 0);
    return 0;
}

/* The pointer helper: "mouse" runs it hidden; "autorun" adds the icon; the
 * installed copy started by hand also opens settings. One per session. */
static int helper(int with_icon, int show_settings)
{
    struct magic_regs r;
    DWORD os_version = GetVersion();
    HANDLE mutex, pid_map;
    DWORD *pid;

    FreeConsole();
    if (!r86_present())
        return show_settings ? settings_only() : 0;
    r = magic_call(MAGIC_VERSION, 0, 0, 0);
    if (r.a != 1 || !(r.b & MAGIC_CAP_POINTER))
        return show_settings ? settings_only() : 0;
    takes_requests = (r.b & MAGIC_CAP_REQUESTS) != 0;
    wheel_supported = nt || LOBYTE(LOWORD(os_version)) > 4 ||
        HIBYTE(LOWORD(os_version)) >= 10;
    if (!prepare_desktop())
        return show_settings ? settings_only() : 1;
    mutex = CreateMutexA(NULL, TRUE, mutex_name);
    if (!mutex)
        return show_settings ? settings_only() : 1;
    if (GetLastError() == ERROR_ALREADY_EXISTS) {
        CloseHandle(mutex);
        return show_settings ? settings_only() : 0;
    }
    stop = CreateEventA(NULL, TRUE, FALSE, stop_name);
    if (!stop) {
        ReleaseMutex(mutex);
        CloseHandle(mutex);
        return 1;
    }
    pid_map = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, sizeof(DWORD), pid_name);
    pid = pid_map ? MapViewOfFile(pid_map, FILE_MAP_WRITE, 0, 0, sizeof(DWORD)) : NULL;
    if (!pid) {
        if (pid_map) CloseHandle(pid_map);
        CloseHandle(stop);
        ReleaseMutex(mutex);
        CloseHandle(mutex);
        return 1;
    }
    /* The installer opens this process before signalling stop, then waits for
     * process exit so Windows has released the EXE before an update/delete. */
    *pid = GetCurrentProcessId();
    helper_running = 1;
    if (!nt)
        add_panel_modes();
    if (!with_icon && !show_settings)
        poll_pointer(NULL);
    else {
        HANDLE thread;
        DWORD id;
        MSG message;

        ui_thread = GetCurrentThreadId();
        window_class(settings_class, settings_proc);
        if (with_icon)
            make_tray();
        if (show_settings)
            open_settings();
        thread = CreateThread(NULL, 0, poll_pointer, NULL, 0, &id);
        if (thread) {
            while (GetMessageA(&message, NULL, 0, 0) > 0)
                dispatch(&message);
            SetEvent(stop); /* Exit from the icon menu */
            WaitForSingleObject(thread, INFINITE);
            CloseHandle(thread);
        }
        if (window)
            DestroyWindow(window);
        if (tray) {
            tray_icon(NIM_DELETE);
            DestroyWindow(tray);
        }
    }
    CloseHandle(stop);
    UnmapViewOfFile(pid);
    CloseHandle(pid_map);
    ReleaseMutex(mutex);
    CloseHandle(mutex);
    return 0;
}

static int usage(void)
{
    say("Usage: r86-tool modes\r\n"
        "       r86-tool mode WIDTH HEIGHT [BITS]   (BITS defaults to 16)\r\n"
        "       r86-tool version\r\n"
        "       r86-tool autorun\r\n"
        "       r86-tool mouse [stop|install|remove]\r\n"
        "       r86-tool install\r\n"
        "       r86-tool uninstall\r\n"
        "       r86-tool driver display [PACKAGE_DIRECTORY]\r\n");
    return 2;
}

void start(void)
{
    const char *s = GetCommandLineA();
    char command[16];
    struct tool_result operation;
    DWORD width, height, bits = 16;
    int status;

    /* Skip the program name, which may be quoted. */
    if (*s == '"') {
        for (s++; *s && *s != '"'; s++)
            ;
        if (*s)
            s++;
    } else {
        while (*s && *s != ' ' && *s != '\t')
            s++;
    }
    nt = !(GetVersion() & 0x80000000);
    s = next_word(s, command, sizeof command);
    if (!*command)
        status = !installed_image() ? installer() : settings_in_helper() ? 0 : helper(1, 1);
    else if (!lstrcmpiA(command, "autorun"))
        status = *spaces(s) ? usage() : helper(1, 0);
    else if (!lstrcmpiA(command, "install") || !lstrcmpiA(command, "uninstall")) {
        if (*spaces(s)) status = usage();
        else {
            status = !lstrcmpiA(command, "install") ? r86_install(&operation) : r86_uninstall(&operation);
            say("%s (result %lu)\r\n", operation.message, operation.code);
        }
    } else if (!lstrcmpiA(command, "driver")) {
        char directory[MAX_PATH + 1];
        s = next_word(s, command, sizeof command);
        s = next_word(s, directory, sizeof directory);
        if (lstrcmpiA(command, "display") || *spaces(s) || lstrlenA(directory) >= MAX_PATH) status = usage();
        else {
            status = r86_install_display(NULL, directory, &operation);
            say("%s (result %lu)\r\n", operation.message, operation.code);
        }
    } else if (!lstrcmpiA(command, "modes"))
        status = list_modes();
    else if (!lstrcmpiA(command, "version"))
        status = version();
    else if (!lstrcmpiA(command, "mouse")) {
        next_word(s, command, sizeof command);
        if (!*command)
            status = helper(0, 0);
        else if (!lstrcmpiA(command, "install"))
            status = mouse_startup(1);
        else if (!lstrcmpiA(command, "remove"))
            status = mouse_startup(0);
        else if (!lstrcmpiA(command, "stop")) {
            stop_mouse();
            status = 0;
        } else
            status = usage();
    } else if (!lstrcmpiA(command, "mode") && (s = number(s, &width)) && (s = number(s, &height))) {
        number(s, &bits);
        status = set_mode(width, height, bits);
    } else
        status = usage();
    if (own_console) {
        char key[8];
        DWORD n;
        say("Press Enter to close.\r\n");
        ReadFile(CreateFileA("CONIN$", GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ, NULL,
                             OPEN_EXISTING, 0, NULL), key, sizeof key, &n, NULL);
    }
    ExitProcess((UINT)status);
}
