/* Small freestanding C runtime for reference-CPU experiments. No emulation
 * fast paths live here. Memory, diagnostics and clock work execute on RV32IM. */
#include "abi.h"
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <time.h>

static FILE console;
FILE *stdout = &console, *stderr = &console;
int errno;
static uintptr_t heap = 0x02000000;
static uint64_t idle_us;
int bench_marker;

void *malloc(unsigned long size)
{
    uintptr_t p = heap;
    if (size > 0x04000000 || p + size > 0x06000000) bench_abort();
    heap = (p + size + 15) & ~(uintptr_t)15;
    return (void *)p;
}
void free(void *p) { (void)p; }
void *memset(void *p, int value, unsigned long size)
{
    unsigned char *q = p;
    while (size--) *q++ = value;
    return p;
}
void *memcpy(void *d, const void *s, unsigned long size)
{
    unsigned char *q = d;
    const unsigned char *p = s;
    while (size--) *q++ = *p++;
    return d;
}
void *memmove(void *d, const void *s, unsigned long size)
{
    unsigned char *q = d;
    const unsigned char *p = s;
    if ((uintptr_t)q <= (uintptr_t)p) return memcpy(d, s, size);
    while (size) { --size; q[size] = p[size]; }
    return d;
}
void *calloc(size_t n, size_t size)
{
    if (n && size > (size_t)-1 / n) bench_abort();
    void *p = malloc(n * size);
    return memset(p, 0, n * size);
}
int putchar(int c)
{
    uint32_t n = WORD(0x50);
    if (n >= OUTPUT_BYTES) bench_abort();
    OUTPUT[n] = c;
    WORD(0x50) = n + 1;
    static const char marker[] = "R86_CHIP_LUA_OK";
    if (n + 1 >= sizeof(marker) - 1) {
        unsigned start = n + 2 - sizeof(marker), i = 0;
        while (i < sizeof(marker) - 1 && OUTPUT[start + i] == marker[i]) ++i;
        if (i == sizeof(marker) - 1) bench_marker = 1;
    }
    return c;
}
int fputc(int c, FILE *f) { (void)f; return putchar(c); }
int fflush(FILE *f) { (void)f; return 0; }
void perror(const char *s) { fprintf(stderr, "%s: unavailable in study firmware\n", s); }

/* No native threads or interrupts run in this bare-metal experiment. The
 * reference NIC uses byte atomics for its host receive thread; that thread is
 * absent here. Supply the same single-thread operations without requiring A. */
unsigned char atomic_load_byte(const volatile void *p, int order) __asm__("__atomic_load_1");
unsigned char atomic_load_byte(const volatile void *p, int order)
{ (void)order; return *(const volatile unsigned char *)p; }
void atomic_store_byte(volatile void *p, unsigned char value, int order) __asm__("__atomic_store_1");
void atomic_store_byte(volatile void *p, unsigned char value, int order)
{ (void)order; *(volatile unsigned char *)p = value; }
unsigned char atomic_and_byte(volatile void *p, unsigned char value, int order) __asm__("__atomic_fetch_and_1");
unsigned char atomic_and_byte(volatile void *p, unsigned char value, int order)
{ (void)order; unsigned char old = *(volatile unsigned char *)p; *(volatile unsigned char *)p = old & value; return old; }
unsigned char atomic_or_byte(volatile void *p, unsigned char value, int order) __asm__("__atomic_fetch_or_1");
unsigned char atomic_or_byte(volatile void *p, unsigned char value, int order)
{ (void)order; unsigned char old = *(volatile unsigned char *)p; *(volatile unsigned char *)p = old | value; return old; }

#define NANOPRINTF_USE_FIELD_WIDTH_FORMAT_SPECIFIERS 1
#define NANOPRINTF_USE_PRECISION_FORMAT_SPECIFIERS 1
#define NANOPRINTF_USE_FLOAT_FORMAT_SPECIFIERS 0
#define NANOPRINTF_USE_LARGE_FORMAT_SPECIFIERS 1
#define NANOPRINTF_USE_SMALL_FORMAT_SPECIFIERS 1
#define NANOPRINTF_USE_BINARY_FORMAT_SPECIFIERS 0
#define NANOPRINTF_USE_WRITEBACK_FORMAT_SPECIFIERS 0
#define NANOPRINTF_IMPLEMENTATION
#include "nanoprintf.h"
int vfprintf(FILE *f, const char *fmt, va_list args)
{
    (void)f;
    char text[768];
    int n = npf_vsnprintf(text, sizeof(text), fmt, args);
    for (unsigned i = 0; text[i] && i < sizeof(text) - 1; ++i) putchar(text[i]);
    return n;
}
int fprintf(FILE *f, const char *fmt, ...)
{
    va_list args;
    va_start(args, fmt);
    int result = vfprintf(f, fmt, args);
    va_end(args);
    return result;
}
int printf(const char *fmt, ...)
{
    va_list args; va_start(args, fmt);
    int n = vfprintf(stdout, fmt, args); va_end(args); return n;
}
int snprintf(char *s, size_t size, const char *fmt, ...)
{
    va_list args; va_start(args, fmt);
    int n = npf_vsnprintf(s, size, fmt, args); va_end(args); return n;
}
void bench_abort(void)
{
    WORD(0x40) = 9;
    __asm__ volatile("ebreak");
    for (;;) {}
}
void __abort(const char *where) { fprintf(stderr, "ABORT %s\n", where); bench_abort(); }
void exit(int status) { WORD(0x40) = 0x100 | (unsigned)status; bench_abort(); }
int usleep(int usec) { if (usec > 0) idle_us += (unsigned)usec; return 0; }
uint64_t bench_clock_ticks(void)
{
    /* Runs are limited below 2^32 attempts. HLT advances deterministic time,
     * but does not count as another completed x86 instruction. */
    return (uint64_t)bench_cycle() + bench_fetch_faults
        + idle_us * (16 * 1193182) / 1000000;
}
int clock_gettime(int clockid, struct timespec *ts)
{
    (void)clockid;
    /* Same 19,090,912 Hz instruction clock as r86's firmware. Device clocks
     * and RDTSC share that coordinate, with their software cost counted. */
    uint64_t ns = bench_clock_ticks() * 1000000000 / (16 * 1193182);
    ts->tv_sec = ns / 1000000000;
    ts->tv_nsec = ns % 1000000000;
    return 0;
}
uint32_t get_uticks(void)
{
    return bench_clock_ticks() * 1000000 / (16 * 1193182);
}
time_t time(time_t *p)
{
    struct timespec ts; clock_gettime(0, &ts);
    time_t value = 946684800 + ts.tv_sec;
    if (p) *p = value;
    return value;
}
struct tm *gmtime_r(const time_t *p, struct tm *t)
{
    time_t seconds = *p;
    if (seconds < 946684800) bench_abort();
    unsigned days = (seconds - 946684800) / 86400;
    t->tm_sec = seconds % 60;
    t->tm_min = seconds / 60 % 60;
    t->tm_hour = seconds / 3600 % 24;
    t->tm_wday = (days + 6) % 7;
    unsigned year = 2000;
    for (;;) {
        unsigned leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        if (days < 365 + leap) break;
        days -= 365 + leap; ++year;
    }
    t->tm_yday = days;
    static const unsigned char lengths[] = {31,28,31,30,31,30,31,31,30,31,30,31};
    unsigned month = 0;
    for (;;) {
        unsigned n = lengths[month] + (month == 1 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0));
        if (days < n) break;
        days -= n; ++month;
    }
    t->tm_year = year - 1900; t->tm_mon = month; t->tm_mday = days + 1; t->tm_isdst = 0;
    return t;
}
size_t strlen(const char *s) { const char *end = s; while (*end) ++end; return end - s; }
int strcmp(const char *a, const char *b)
{
    while (*a && *a == *b) { ++a; ++b; }
    return (unsigned char)*a - (unsigned char)*b;
}
int memcmp(const void *a, const void *b, size_t n)
{
    const unsigned char *x = a, *y = b;
    for (size_t i = 0; i < n; ++i) if (x[i] != y[i]) return x[i] - y[i];
    return 0;
}
char *strcpy(char *a, const char *b) { char *start = a; while ((*a++ = *b++)) {} return start; }
char *strdup(const char *s) { size_t n = strlen(s) + 1; char *p = malloc(n); return memcpy(p, s, n); }
FILE *fopen(const char *path, const char *mode) { (void)path; (void)mode; return NULL; }
int fclose(FILE *f) { (void)f; return 0; }
int fseek(FILE *f, off_t offset, int whence) { (void)f; (void)offset; (void)whence; bench_abort(); }
off_t ftell(FILE *f) { (void)f; bench_abort(); }
size_t fread(void *p, size_t size, size_t n, FILE *f) { (void)p; (void)size; (void)n; (void)f; bench_abort(); }
size_t fwrite(const void *p, size_t size, size_t n, FILE *f) { (void)p; (void)size; (void)n; (void)f; bench_abort(); }
