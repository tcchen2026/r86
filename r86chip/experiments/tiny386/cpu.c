/* Study-only build of the original reference CPU. build.py freezes the source
 * and applies the documented portability/counting edits to that private copy.
 * This is not an r86 backend and supplies no production x86 semantics. */
#include "abi.h"
#include <stdio.h>
FILE *freopen(const char *, const char *, FILE *);
void setlinebuf(FILE *);
#include "i386.c"

uint32_t bench_faults, bench_fetch_faults;
static CPUI386 *active_cpu;
uint64_t bench_attempts;
void bench_bind_cpu(void *cpu) { active_cpu = cpu; }

uint32_t bench_cycle(void) { return active_cpu ? (uint32_t)active_cpu->cycle : 0; }

void bench_publish_cpu(void *opaque, uint64_t attempts, uint64_t completed)
{
    CPUI386 *cpu = opaque;
    WORD(0x44) = attempts;
    WORD(0x48) = attempts >> 32;
    WORD(0x4c) = cpu->next_ip;
    for (int i = 0; i < 8; ++i) WORD(0x80 + i * 4) = REGi(i);
    WORD(0xa0) = cpu_getflags(cpu);
    WORD(0xb0) = attempts;
    WORD(0xb4) = attempts >> 32;
    WORD(0xf0) = completed;
    WORD(0xf4) = completed >> 32;
    WORD(0xf8) = bench_faults;
}

#ifndef BENCH_PC
void bench_main(void)
{
    if (CONFIG[0] != 0x5238364d || CONFIG[4] != GUEST_BYTES) bench_abort();
    uint64_t limit = CONFIG[5] | ((uint64_t)CONFIG[6] << 32);
    CPUI386 *cpu = cpui386_new(6, GUEST, GUEST_BYTES, NULL);
    active_cpu = cpu;
#ifdef I386_ENABLE_FPU
    cpui386_enable_fpu(cpu);
#endif
    cpui386_reset_pm(cpu, CONFIG[2]);
    cpui386_set_gpr(cpu, 6, CONFIG[3]);
    uint64_t attempts = 0;
    while (attempts < limit) {
        unsigned count = limit - attempts > 4096 ? 4096 : limit - attempts;
        uint32_t before = cpu->cycle;
        uint32_t before_fetch_faults = bench_fetch_faults;
        cpui386_step(cpu, count);
        attempts += (uint32_t)(cpu->cycle - before)
            + (bench_fetch_faults - before_fetch_faults);
        if (cpu->halt) bench_abort(); /* selected loops never halt */
    }
    bench_publish_cpu(cpu, attempts, attempts - bench_faults);
    WORD(0x40) = 4;
}
#endif
