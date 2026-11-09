/* r86-tool: guest tools for r86 Windows VMs (95 to XP); display modes so far.
 *
 *   r86-tool modes              list every display mode Windows offers
 *   r86-tool mode W H [BITS]    switch the desktop to W x H at BITS (16 by
 *                               default) and keep it for the next boot
 *
 * The Display control panel hides modes smaller than 640x480, such as the
 * handheld VGA BIOS's 640x340; EnumDisplaySettings and ChangeDisplaySettings
 * do not.
 *
 * Plain C with no C runtime, importing only kernel32 and user32 calls present
 * since Windows 95 and NT 4. Build with MinGW:
 *
 *   i686-w64-mingw32-gcc -Os -s -nostdlib -ffreestanding \
 *       -fno-asynchronous-unwind-tables -Wall -Wextra -o r86-tool.exe \
 *       tools/win/r86-tool.c -e _start -Wl,--subsystem,console:4.0 \
 *       -Wl,--major-os-version,4 -Wl,--minor-os-version,0 -lkernel32 -luser32
 */
#include <windows.h>

static HANDLE out;

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

static int usage(void)
{
    say("Usage: r86-tool modes\r\n"
        "       r86-tool mode WIDTH HEIGHT [BITS]   (BITS defaults to 16)\r\n");
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
    else if (!lstrcmpiA(command, "mode") && (s = number(s, &width)) && (s = number(s, &height))) {
        number(s, &bits);
        status = set_mode(width, height, bits);
    } else
        status = usage();
    ExitProcess((UINT)status);
}
