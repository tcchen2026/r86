// Firmware on core + caches + a slow burst RAM (r86_system). Same image
// directory as machine_tb.cpp. After the core stops, dirty cache lines are
// written back (sync) so the RAM hashes are comparable with the ISS.
// RAM_LATENCY is the number of clocks before a burst request is accepted;
// each beat then takes one clock.
#include "Vr86_system.h"
#include "verilated.h"
#include <cstdint>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

static constexpr uint32_t CONFIG = 0x00800000, GUEST = 0x10000000;
static constexpr size_t RAM_BYTES = 512u << 20, GUEST_BYTES = 128u << 20;
static void load(std::vector<uint8_t>& ram, const std::string& name, size_t address) {
    std::ifstream in(name, std::ios::binary | std::ios::ate);
    if (!in) throw std::runtime_error("cannot open " + name);
    auto size = in.tellg();
    in.seekg(0);
    if (size < 0 || address + size_t(size) > ram.size() ||
        !in.read(reinterpret_cast<char*>(ram.data() + address), size))
        throw std::runtime_error("cannot load " + name);
}
static uint32_t word(const std::vector<uint8_t>& ram, uint32_t a) {
    return uint32_t(ram[a]) | uint32_t(ram[a+1]) << 8 | uint32_t(ram[a+2]) << 16 | uint32_t(ram[a+3]) << 24;
}
static uint64_t hash_bytes(const uint8_t* bytes, size_t len) {
    uint64_t hash = 0xcbf29ce484222325ULL;
    for (size_t n = 0; n < len; ++n) hash = (hash ^ bytes[n]) * 0x100000001b3ULL;
    return hash;
}
int main(int argc, char** argv) try {
    if (argc != 4) throw std::runtime_error("usage: system_machine_tb IMAGE_DIRECTORY CYCLE_LIMIT RAM_LATENCY");
    const std::string dir = argv[1];
    const uint64_t limit = std::stoull(argv[2]);
    const unsigned latency = std::stoul(argv[3]);
    std::vector<uint8_t> ram(RAM_BYTES, 0);
    load(ram, dir + "/firmware.bin", 0x1000);
    load(ram, dir + "/guest.ram", GUEST);
    load(ram, dir + "/config.bin", CONFIG);
    VerilatedContext context;
    context.threads(1);
    Vr86_system sys(&context);
    sys.io_dev_ready = 1; sys.io_dev_error = 1; sys.io_dev_rdata = 0;
    sys.io_ram_req_ready = 0; sys.io_ram_w_ready = 0; sys.io_ram_rsp_valid = 0; sys.io_sync = 0;
    sys.reset = 1;
    for (int n = 0; n < 4; ++n) { sys.clock = 0; sys.eval(); sys.clock = 1; sys.eval(); }
    sys.reset = 0;
    uint64_t cycles = 0, retired = 0, fills = 0, writebacks = 0, run_cycles = 0;
    // Line fills by region: firmware image, heap and stack, guest RAM.
    uint64_t region[3] = {0, 0, 0};
    // Clocks in which each bit of io_events was set; see Core.scala.
    uint64_t events[16] = {};
    // 0 idle, 1 waiting to accept, 2 read beats, 3 write beats, 4 write response.
    int phase = 0;
    unsigned wait = 0, beats = 0;
    uint32_t address = 0;
    bool synced = false;
    while (cycles < limit && !synced) {
        sys.clock = 0; sys.eval();
        const bool req_fire = sys.io_ram_req_valid && sys.io_ram_req_ready;
        const bool w_fire = sys.io_ram_w_valid && sys.io_ram_w_ready;
        if (req_fire) {
            address = sys.io_ram_req_bits_address;
            beats = sys.io_ram_req_bits_beats + 1u;
            phase = sys.io_ram_req_bits_write ? 3 : 2;
            ++(sys.io_ram_req_bits_write ? writebacks : fills);
            if (!sys.io_ram_req_bits_write) ++region[address < 0x02000000 ? 0 : address < GUEST ? 1 : 2];
        }
        if (w_fire) {
            if (address <= RAM_BYTES - 4)
                for (unsigned n = 0; n < 4; ++n)
                    if (sys.io_ram_w_bits_strobes & (1u << n)) ram[address+n] = uint8_t(sys.io_ram_w_bits_data >> (8*n));
            address += 4;
            if (--beats == 0) phase = 4;
        }
        if (sys.io_retire_valid) ++retired;
        for (unsigned n = 0, e = sys.io_events; e; ++n, e >>= 1) events[n] += e & 1;
        if (sys.io_synced) synced = true;
        const bool request = sys.io_ram_req_valid;
        sys.clock = 1; sys.eval();
        ++cycles;
        if (!sys.io_trap) run_cycles = cycles;
        else sys.io_sync = 1;
        sys.io_ram_req_ready = 0; sys.io_ram_w_ready = 0; sys.io_ram_rsp_valid = 0;
        if (phase == 0 && request && !req_fire) { phase = 1; wait = latency; }
        if (phase == 1) {
            if (wait) --wait; else sys.io_ram_req_ready = 1;
        } else if (phase == 2) {
            sys.io_ram_rsp_valid = 1;
            sys.io_ram_rsp_bits_error = address > RAM_BYTES - 4;
            sys.io_ram_rsp_bits_data = sys.io_ram_rsp_bits_error ? 0 : word(ram, address);
            sys.io_ram_rsp_bits_last = beats == 1;
            address += 4;
            if (--beats == 0) phase = 0;
        } else if (phase == 3) {
            sys.io_ram_w_ready = 1;
        } else if (phase == 4) {
            sys.io_ram_rsp_valid = 1; sys.io_ram_rsp_bits_error = 0; sys.io_ram_rsp_bits_last = 1;
            phase = 0;
        }
    }
    const uint64_t steps = uint64_t(word(ram, CONFIG+0x44)) | uint64_t(word(ram, CONFIG+0x48)) << 32;
    std::cout << "{\"engine\":\"verilator-system\",\"ram_latency\":" << latency << ",\"cycles\":" << run_cycles
              << ",\"retired_native\":" << retired << ",\"x86_steps\":" << steps
              << ",\"line_fills\":" << fills << ",\"fills_firmware\":" << region[0]
              << ",\"fills_heap_stack\":" << region[1] << ",\"fills_guest\":" << region[2] << ",\"write_backs\":" << writebacks
              << ",\"events\":[" << events[0];
    for (int n = 1; n < 16; ++n) std::cout << "," << events[n];
    std::cout << "],\"trap\":" << unsigned(sys.io_trap) << ",\"trap_cause\":" << unsigned(sys.io_trap_cause)
              << ",\"synced\":" << (synced ? "true" : "false") << ",\"ram_fnv64\":\"" << std::hex
              << std::setw(16) << std::setfill('0') << hash_bytes(ram.data(), ram.size())
              << "\",\"guest_ram_fnv64\":\"" << std::setw(16) << hash_bytes(ram.data() + GUEST, GUEST_BYTES)
              << "\"}" << std::dec << std::endl;
    sys.final();
    return 0;
} catch (const std::exception& e) {
    std::cerr << e.what() << '\n'; return 1;
}
