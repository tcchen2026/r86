/* Check the experiment-only mode-0 IRQ port using the reference time model.
 * Consulted: tiny386 i8254.c mode-0 terminal-count behavior. v86 pit.js agrees
 * on one-shot IRQs and rearming after count writes; it expires on elapsed >
 * count and turns a zero reload into 65535.
 * This checks edges/rearming, not a claim of clock-exact PIT emulation. */
#include "i8254.h"
#include <assert.h>
#include <stdio.h>

static uint32_t now;
static int rising, falling;
uint32_t get_uticks(void) { return now; }
static void irq(void *opaque, int number, int level)
{
    (void)opaque;
    assert(number == 0);
    if (level) ++rising; else ++falling;
}
static void count(PITState *pit, unsigned value)
{
    i8254_ioport_write(pit, 0x40, value & 255);
    i8254_ioport_write(pit, 0x40, value >> 8);
}
int main(void)
{
    PITState *pit = i8254_init(0, NULL, irq);
    i8254_ioport_write(pit, 0x43, 0x30); /* channel 0, word, mode 0 */
    count(pit, 3);
    now = 2;
    i8254_update_irq(pit);
    assert(rising == 0 && pit_get_out(pit, 0) == 0);
    now = 3;
    i8254_update_irq(pit);
    assert(rising == 1 && falling == 1 && pit_get_out(pit, 0) == 1);
    now = 100;
    i8254_update_irq(pit);
    assert(rising == 1); /* no periodic repetition */
    count(pit, 3);
    i8254_update_irq(pit);
    assert(rising == 1 && pit_get_out(pit, 0) == 0);
    now = 103;
    i8254_update_irq(pit);
    assert(rising == 2 && falling == 2);
    count(pit, 0); /* zero encodes 65536, not immediate expiry */
    now += 50000;
    i8254_update_irq(pit);
    assert(rising == 2);
    now += 5000;
    i8254_update_irq(pit);
    assert(rising == 3 && falling == 3);
    for (int mode = 2; mode <= 3; ++mode) {
        i8254_ioport_write(pit, 0x43, 0x30 | (mode << 1));
        count(pit, 3);
        int before = rising;
        /* Sample after the boundary: the unmodified periodic updater uses
         * elapsed > deadline, unlike its mode-0 output read's >= comparison. */
        now += 4;
        i8254_update_irq(pit);
        now += 4;
        i8254_update_irq(pit);
        assert(rising == before + 2 && falling == rising);
    }
    puts("PIT mode-0 deadline/rearm and periodic modes passed");
}
