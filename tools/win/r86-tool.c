/* r86-tool: guest tools for r86 Windows VMs (95 to XP).
 *
 *   r86-tool modes              list every display mode Windows offers
 *   r86-tool mode W H [BITS]    switch the desktop to W x H at BITS (16 by
 *                               default) and keep it for the next boot
 *   r86-tool version            print tool and magic instruction versions
 *   r86-tool mouse              run the absolute pointer helper, hidden
 *   r86-tool mouse stop         stop the running pointer helper
 *   r86-tool mouse install      start this executable at the user's logon
 *   r86-tool mouse remove       remove that Run entry and stop the helper
 *   r86-tool install            copy the helper/docs to C:\R86 and start it
 *   r86-tool uninstall          stop it, remove its all-user Run entry/files
 *   r86-tool driver display [CDROOT]  install this OS's tools-CD VGA driver
 * No arguments runs the helper only from C:\R86\R86-TOOL.EXE; other copies
 * print usage until the installer dialog is supplied.
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
 *       tools/win/r86-tool.c -e _start -Wl,--subsystem,console:4.0 \
 *       -Wl,--major-os-version,4 -Wl,--minor-os-version,0 -lkernel32 -luser32 -ladvapi32
 */
#include <windows.h>
#define USE_SP_DRVINFO_DATA_V1 1
#include <setupapi.h>
#include "r86-tool.h"

static HANDLE out;

/* Constants mirror src/machine/magic.rs, the protocol authority. */
#define MAGIC_PORT 0x5286
#define MAGIC_VALUE 0x21363872
#define MAGIC_CPUID 0x40000000
#define MAGIC_VERSION 0
#define MAGIC_POINTER 2
#define MAGIC_CAP_POINTER 2
#define TOOL_VERSION 2

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

static struct magic_regs magic_call(DWORD command)
{
    struct magic_regs r = { MAGIC_VALUE, 0, command, MAGIC_PORT, 0, 0 };
    __asm__ volatile("inl %%dx,%%eax"
                     : "+a"(r.a), "+b"(r.b), "+c"(r.c), "+d"(r.d), "+S"(r.s), "+D"(r.i));
    return r;
}

static void say(const char *format, ...)
{
    char text[1024];
    va_list args;
    DWORD written;
    int length;

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

static int set_mode(DWORD width, DWORD height, DWORD bits)
{
    DEVMODEA mode;
    LONG code;
    const char *text;

    memset(&mode, 0, sizeof mode);
    mode.dmSize = sizeof mode;
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmBitsPerPel = bits;
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
    code = ChangeDisplaySettingsA(&mode, CDS_UPDATEREGISTRY);
    switch (code) {
    case DISP_CHANGE_SUCCESSFUL: text = "switched"; break;
    case DISP_CHANGE_RESTART: text = "saved; restart Windows to use it"; break;
    case DISP_CHANGE_BADMODE: text = "Windows does not offer this mode"; break;
    case DISP_CHANGE_NOTUPDATED: text = "switched, but not saved in the registry"; break;
    default: text = "failed"; break;
    }
    say("%lu x %lu, %lu bit: %s\r\n", width, height, bits, text);
    return code == DISP_CHANGE_SUCCESSFUL || code == DISP_CHANGE_RESTART ? 0 : 1;
}

static int version(void)
{
    struct magic_regs r;
    say("r86-tool %u\r\n", TOOL_VERSION);
    if (!r86_present()) {
        say("r86 magic instruction is not available\r\n");
        return 1;
    }
    r = magic_call(MAGIC_VERSION);
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

static int mouse(void)
{
    struct magic_regs r;
    DWORD buttons = 0, os_version, hint = 0;
    HANDLE stop, mutex, pid_map;
    DWORD *pid;
    int wheel_supported;

    FreeConsole();
    if (!r86_present())
        return 0;
    r = magic_call(MAGIC_VERSION);
    if (r.a != 1 || !(r.b & MAGIC_CAP_POINTER))
        return 0;
    os_version = GetVersion();
    nt = !(os_version & 0x80000000);
    wheel_supported = nt || LOBYTE(LOWORD(os_version)) > 4 ||
        HIBYTE(LOWORD(os_version)) >= 10;
    if (!prepare_desktop())
        return 1;
    mutex = CreateMutexA(NULL, TRUE, mutex_name);
    if (!mutex)
        return 1;
    if (GetLastError() == ERROR_ALREADY_EXISTS) {
        CloseHandle(mutex);
        return 0;
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
    /* A timed event wait has Sleep's cadence and also permits a clean stop. */
    while (WaitForSingleObject(stop, hint) == WAIT_TIMEOUT) {
        DWORD changed, flags;
        if (!input_desktop()) {
            hint = 100;
            continue; /* No poll: allow the host's PS/2 fallback lease to expire. */
        }
        r = magic_call(MAGIC_POINTER);
        if (r.a == 0xffffffff)
            break;
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
    if (buttons && input_desktop()) {
        DWORD flags = 0;
        if (buttons & 1) flags |= MOUSEEVENTF_LEFTUP;
        if (buttons & 2) flags |= MOUSEEVENTF_RIGHTUP;
        if (buttons & 4) flags |= MOUSEEVENTF_MIDDLEUP;
        mouse_event(flags, 0, 0, 0, 0);
    }
    CloseHandle(stop);
    UnmapViewOfFile(pid);
    CloseHandle(pid_map);
    ReleaseMutex(mutex);
    CloseHandle(mutex);
    return 0;
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
    char command[] = "\"C:\\R86\\R86-TOOL.EXE\"";
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
    code = startup_entry(HKEY_LOCAL_MACHINE, "r86 Tool", "\"C:\\R86\\R86-TOOL.EXE\"");
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

static int usage(void)
{
    say("Usage: r86-tool modes\r\n"
        "       r86-tool mode WIDTH HEIGHT [BITS]   (BITS defaults to 16)\r\n"
        "       r86-tool version\r\n"
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

    out = GetStdHandle(STD_OUTPUT_HANDLE);
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
    s = next_word(s, command, sizeof command);
    if (!*command && installed_image())
        status = mouse();
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
            status = mouse();
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
    ExitProcess((UINT)status);
}
