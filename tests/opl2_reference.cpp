// Native YM3812 oracle for src/devices/audio/opl2_vectors.txt (48,000 samples).
// Uses unmodified BSD-3-Clause ymfm 81aec25 through its public API.
// Build: g++ -std=c++17 -O2 -ffunction-sections -fdata-sections -Wl,--gc-sections
//   -I /path/to/ymfm/src tests/opl2_reference.cpp /path/to/ymfm/src/ymfm_opl.cpp
//   -o /tmp/opl2-reference
// Run: /tmp/opl2-reference src/devices/audio/opl2_vectors.txt /tmp/opl2.pcm
// Output: little-endian signed 16-bit mono at the native clock / 72 rate.
#include "ymfm_opl.h"
#include <cstdint>
#include <fstream>
#include <vector>

struct Interface : ymfm::ymfm_interface {
    int64_t now = 0, due[2] = {-1, -1};

    void ymfm_set_timer(uint32_t n, int32_t clocks) override {
        due[n] = clocks < 0 ? -1 : now + clocks;
    }
    void step() {
        now += 72;
        for (int i = 0; i < 2; i++) {
            if (due[i] >= 0 && now >= due[i]) {
                due[i] = -1;
                m_engine->engine_timer_expired(i);
            }
        }
    }
};

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    struct Event { int t, reg, val; };
    std::vector<Event> events;
    std::ifstream in(argv[1]);
    if (!in) return 2;
    Event e;
    while (in >> std::dec >> e.t >> std::hex >> e.reg >> e.val)
        events.push_back(e);

    Interface iface;
    ymfm::ym3812 chip(iface);
    chip.reset();
    size_t pos = 0;
    std::ofstream out(argv[2], std::ios::binary);
    if (!out) return 2;
    for (int i = 0; i < 48000; i++) {
        while (pos < events.size() && events[pos].t == i) {
            chip.write_address(events[pos].reg);
            chip.write_data(events[pos++].val);
        }
        iface.step();
        ymfm::ym3812::output_data sample;
        chip.generate(&sample);
        uint16_t value = sample.data[0];
        out.put(value);
        out.put(value >> 8);
    }
    return out ? 0 : 2;
}
