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

static HANDLE out;

/* Constants mirror src/machine/magic.rs, the protocol authority. */
#define MAGIC_PORT 0x5286
#define MAGIC_VALUE 0x21363872
#define MAGIC_CPUID 0x40000000
#define MAGIC_VERSION 0
#define MAGIC_POINTER 2
#define MAGIC_CAP_POINTER 2
#define TOOL_VERSION 1

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
    char text[256];
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
    int n = 0;

    for (s = spaces(s); *s && *s != ' ' && *s != '\t'; s++)
        if (n < size - 1)
            word[n++] = *s;
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
    HANDLE stop;
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
    stop = CreateEventA(NULL, TRUE, FALSE, stop_name);
    if (!stop)
        return 1;
    if (GetLastError() == ERROR_ALREADY_EXISTS) {
        CloseHandle(stop);
        return 0;
    }
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

static int usage(void)
{
    say("Usage: r86-tool modes\r\n"
        "       r86-tool mode WIDTH HEIGHT [BITS]   (BITS defaults to 16)\r\n"
        "       r86-tool version\r\n"
        "       r86-tool mouse [stop|install|remove]\r\n");
    return 2;
}

void start(void)
{
    const char *s = GetCommandLineA();
    char command[16];
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
    if (!lstrcmpiA(command, "modes"))
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
