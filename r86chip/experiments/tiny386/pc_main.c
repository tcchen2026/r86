/* Direct-kernel comparison using tiny386's own CPU and PC devices. The input
 * RAM image is the same pre-decompressed image supplied to r86's firmware.
 * This is a workload comparison, not a claim that the CPU models are identical. */
#include "abi.h"
#include "pc.h"
#include <string.h>

void *bigmalloc(size_t size)
{
    return size == GUEST_BYTES ? GUEST : calloc(1, size);
}
void *pcmalloc(long size) { return malloc(size); }
int load_rom(void *mem, const char *file, uword addr, int backward)
{
    (void)mem; (void)file; (void)addr; (void)backward;
    bench_abort(); /* no BIOS/reset or disks in this direct-kernel workload */
}
static void redraw(void *opaque, int x, int y, int w, int h)
{
    (void)opaque; (void)x; (void)y; (void)w; (void)h;
}
void bench_main(void)
{
    if (CONFIG[0] != 0x5238364d || CONFIG[4] != GUEST_BYTES) bench_abort();
    uint64_t limit = CONFIG[5] | ((uint64_t)CONFIG[6] << 32);
    if (limit > 0xffff0000u) bench_abort();
    PCConfig config = {0};
    config.mem_size = GUEST_BYTES;
    config.vga_mem_size = 4 << 20;
    config.width = 720;
    config.height = 480;
    config.cpu_gen = 6;
    config.fpu = 1;
    config.enable_serial = 1;
    config.bios = config.vga_bios = "";
    PC *pc = pc_new(redraw, NULL, calloc(720 * 480, 4), &config);
    bench_bind_cpu(pc->cpu);
    cpui386_reset_pm(pc->cpu, CONFIG[2]);
    cpui386_set_gpr(pc->cpu, 6, CONFIG[3]);
    while (bench_attempts < limit && !bench_marker) {
        uint32_t before = bench_cycle();
        uint32_t before_fetch = bench_fetch_faults;
        pc_step(pc);
        bench_attempts += (uint32_t)(bench_cycle() - before)
            + (bench_fetch_faults - before_fetch);
        /* Progress observation must not materialize tiny386's lazy flags. */
        WORD(0x44) = bench_attempts;
        WORD(0x48) = bench_attempts >> 32;
    }
    bench_publish_cpu(pc->cpu, bench_attempts, bench_attempts - bench_faults);
    WORD(0x40) = bench_marker ? 3 : 4;
}
