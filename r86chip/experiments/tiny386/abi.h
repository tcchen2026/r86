/* The existing machine_abi.rs simulation layout, consumed by C study firmware.
 * No x86 execution is delegated to the host. All these are native addresses. */
#include <stdint.h>
#include <stddef.h>
#define CONFIG ((volatile uint32_t *)0x00800000)
#define GUEST ((char *)0x10000000)
#define GUEST_BYTES (128u << 20)
#define OUTPUT ((char *)0x00900000)
#define OUTPUT_BYTES (1u << 20)
#define WORD(offset) CONFIG[(offset) / 4]
void bench_main(void);
void bench_abort(void) __attribute__((noreturn));
void bench_publish_cpu(void *cpu, uint64_t attempts, uint64_t completed);
uint32_t bench_cycle(void);
uint64_t bench_clock_ticks(void);
void bench_bind_cpu(void *cpu);
extern uint64_t bench_attempts;
extern int bench_marker;
extern uint32_t bench_faults, bench_fetch_faults;
