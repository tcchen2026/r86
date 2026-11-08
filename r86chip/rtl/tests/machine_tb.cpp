// Large, zero-initialized external simulation RAM; no host x86 execution.
#include "Vr86_core.h"
#include "verilated.h"
#include "verilated_save.h"
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

static constexpr uint32_t CONFIG = 0x00800000, OUTPUT = 0x00900000, GUEST = 0x10000000;
static constexpr size_t RAM_BYTES = 512u << 20, GUEST_BYTES = 128u << 20, OUTPUT_BYTES = 1u << 20;
static void load(std::vector<uint8_t>& ram, const std::string& name, size_t address, size_t maximum) {
    std::ifstream in(name, std::ios::binary | std::ios::ate);
    if (!in) throw std::runtime_error("cannot open " + name);
    auto size = in.tellg();
    if (size < 0 || size_t(size) > maximum || address + size_t(size) > ram.size())
        throw std::runtime_error("image outside assigned RAM region: " + name);
    in.seekg(0);
    if (!in.read(reinterpret_cast<char*>(ram.data() + address), size))
        throw std::runtime_error("short read: " + name);
}
static uint32_t word(const std::vector<uint8_t>& ram, uint32_t address) {
    return uint32_t(ram[address]) | uint32_t(ram[address+1]) << 8 |
           uint32_t(ram[address+2]) << 16 | uint32_t(ram[address+3]) << 24;
}
static uint64_t hash_bytes(const uint8_t* bytes, size_t len) {
    uint64_t hash = 0xcbf29ce484222325ULL;
    for (size_t n = 0; n < len; ++n) hash = (hash ^ bytes[n]) * 0x100000001b3ULL;
    return hash;
}
static uint64_t hash_ram(const std::vector<uint8_t>& ram) { return hash_bytes(ram.data(), ram.size()); }
static uint64_t wide(const std::vector<uint8_t>& ram, uint32_t addr) {
    return uint64_t(word(ram, addr)) | uint64_t(word(ram, addr+4)) << 32;
}
int main(int argc, char** argv) try {
    if (argc != 4 && !(argc == 5 && std::string(argv[4]) == "--resume"))
        throw std::runtime_error("usage: machine_tb IMAGE_DIRECTORY CYCLE_LIMIT WALL_SECONDS [--resume]");
    const std::string dir = argv[1];
    const uint64_t limit = std::stoull(argv[2]);
    const double seconds = std::stod(argv[3]);
    if (!limit || !std::isfinite(seconds) || seconds <= 0)
        throw std::runtime_error("limits must be positive and finite");
    std::vector<uint8_t> ram(RAM_BYTES, 0);
    load(ram, dir + "/firmware.bin", 0x1000, CONFIG - 0x1000);
    load(ram, dir + "/guest.ram", GUEST, GUEST_BYTES);
    load(ram, dir + "/config.bin", CONFIG, 0x40);
    const uint64_t input_hash = hash_ram(ram);
    VerilatedContext context;
    // This model is single-threaded; avoid the default idle worker pool.
    context.threads(1);
    context.commandArgs(argc, argv);
    Vr86_core core(&context);
    core.reset = 1;
    for (int n = 0; n < 4; ++n) {
        core.clk = 0; core.eval(); core.clk = 1; core.eval();
    }
    core.reset = 0;
    uint64_t cycles = 0, retired = 0;
    uint64_t xalu_ops = 0, tlb_lookups = 0, tlb_hits = 0, tlb_fills = 0;
    uint64_t xld_ops = 0, xld_hits = 0;
    uint64_t xea_ops = 0, xea_hits = 0;
    uint32_t last_pc = 0;
    double previous_seconds = 0;
    const std::string checkpoint = dir + "/rtl-checkpoint.vlt";
    constexpr uint64_t checkpoint_magic = 0x52383643484b0004ULL;
    if (argc == 5) {
        VerilatedRestore saved;
        saved.open(checkpoint.c_str());
        if (!saved.isOpen()) throw std::runtime_error("cannot open checkpoint");
        uint64_t magic = 0, saved_inputs = 0;
        saved >> magic >> saved_inputs;
        if (magic != checkpoint_magic || saved_inputs != input_hash)
            throw std::runtime_error("checkpoint does not match these boot inputs");
        saved >> cycles >> retired >> last_pc >> previous_seconds >> xalu_ops >> tlb_lookups >> tlb_hits >> tlb_fills >> xld_ops >> xld_hits >> xea_ops >> xea_hits >> core;
        saved.read(ram.data(), ram.size());
        saved.close();
        std::cerr << "Resumed RTL checkpoint at cycle " << cycles << '\n';
    }
    size_t sent = 0;
    std::ofstream serial(dir + "/rtl-serial.log", std::ios::binary);
    const auto start = std::chrono::steady_clock::now();
    auto last_progress = start;
    auto elapsed = [&]() { return std::chrono::duration<double>(std::chrono::steady_clock::now() - start).count(); };
    auto save = [&]() {
        const std::string temporary = checkpoint + ".tmp";
        VerilatedSave saved;
        saved.open(temporary.c_str());
        if (!saved.isOpen()) throw std::runtime_error("cannot create checkpoint");
        const double total_seconds = previous_seconds + elapsed();
        saved << checkpoint_magic << input_hash << cycles << retired << last_pc << total_seconds << xalu_ops << tlb_lookups << tlb_hits << tlb_fills << xld_ops << xld_hits << xea_ops << xea_hits << core;
        saved.write(ram.data(), ram.size());
        saved.close();
        if (std::rename(temporary.c_str(), checkpoint.c_str()))
            throw std::runtime_error("cannot publish checkpoint");
    };
    auto flush = [&]() {
        const size_t end = word(ram, CONFIG + 0x50);
        if (end < sent || end > OUTPUT_BYTES) throw std::runtime_error("invalid output length");
        serial.write(reinterpret_cast<const char*>(ram.data() + OUTPUT + sent), end - sent);
        std::cout.write(reinterpret_cast<const char*>(ram.data() + OUTPUT + sent), end - sent);
        std::cout.flush(); serial.flush(); sent = end;
    };
    while (cycles < limit && !core.trap) {
        // Requests already settled after the previous rising edge. Core.scala
        // makes valid/address independent of ready/error, so one falling-edge
        // evaluation can both apply these memory responses and settle retire.
        core.i_ready = core.i_valid;
        core.i_error = core.i_addr > RAM_BYTES - 4;
        core.i_data = core.i_error ? 0 : word(ram, core.i_addr);
        core.d_ready = core.d_valid;
        core.d_error = core.d_addr > RAM_BYTES - 4;
        core.d_rdata = core.d_error ? 0 : word(ram, core.d_addr);
        core.clk = 0; core.eval();
        if (core.retire_valid) {
            ++retired; last_pc = core.retire_pc;
            unsigned opcode = core.retire_word & 127, f = (core.retire_word >> 12) & 7;
            if (opcode == 0x0b && f < 4) ++xalu_ops;
            if (opcode == 0x2b && f == 0) {
                ++tlb_lookups;
                tlb_hits += core.retire_value != 0xffffffffu;
            }
            if (opcode == 0x2b && f == 1) ++tlb_fills;
            if (opcode == 0x5b && f < 3) {
                ++xld_ops;
                xld_hits += core.d_valid && core.d_ready && !core.d_error;
            }
            if (opcode == 0x7b && f == 0) {
                ++xea_ops;
                xea_hits += core.retire_ea_hit;
            }
        }
        if (core.d_valid && core.d_ready && !core.d_error)
            for (unsigned n = 0; n < 4; ++n)
                if (core.d_wstrb & (1u << n)) ram[core.d_addr+n] = uint8_t(core.d_wdata >> (8*n));
        core.clk = 1; core.eval();
        ++cycles;
        if (cycles % 1000000 == 0) {
            flush();
            const auto now = std::chrono::steady_clock::now();
            if (std::chrono::duration<double>(now - last_progress).count() >= 5) {
                std::cerr << "RTL " << elapsed() << "s cycles=" << cycles << " rv=" << retired
                          << " x86=" << word(ram, CONFIG+0x44) << " eip=0x" << std::hex
                          << word(ram, CONFIG+0x4c) << " pc=0x" << last_pc << std::dec << '\n';
                last_progress = now;
            }
            if (elapsed() >= seconds) break;
            if (cycles % 5000000000ULL == 0) save();
        }
    }
    flush();
    const double execution_seconds = previous_seconds + elapsed();
    // Only a live run needs a resumable checkpoint. Terminal correctness tests
    // already report the full RAM hash and architectural observations below;
    // writing 512 MiB for each stopped fixture needlessly exhausts disk space.
    if (!core.trap) save();
    if (word(ram, CONFIG+28) & 4) {
        const size_t len = word(ram, CONFIG+0xb8);
        if (len > (64u << 20) || len % 64) throw std::runtime_error("invalid boundary trace length");
        std::ofstream boundaries(dir + "/rtl-boundaries.bin", std::ios::binary);
        boundaries.write(reinterpret_cast<const char*>(ram.data()+0x18000000), len);
    }
    const uint32_t status = word(ram, CONFIG+0x40);
    const uint64_t steps = uint64_t(word(ram, CONFIG+0x44)) | uint64_t(word(ram, CONFIG+0x48)) << 32;
    const std::string output(reinterpret_cast<const char*>(ram.data()+OUTPUT), sent);
    const bool boot_marker = output.find("R86_CHIP_TINYCORE_OK") != std::string::npos;
    const bool lua_marker = output.find("R86_CHIP_LUA_OK") != std::string::npos;
    const bool marker = boot_marker && (!(word(ram, CONFIG+28) & 1) || lua_marker);
    const uint64_t ram_hash = hash_ram(ram);
    const uint64_t guest_hash = hash_bytes(ram.data() + GUEST, GUEST_BYTES);
    std::ofstream report(dir + "/rtl-run.json");
    report << "{\"engine\":\"verilator-b1\",\"cycles\":" << cycles << ",\"rv_instructions\":" << retired
           << ",\"x86_steps\":" << steps << ",\"eip\":" << word(ram, CONFIG+0x4c)
           << ",\"pc\":" << last_pc << ",\"status\":" << status << ",\"marker\":" << (marker ? "true" : "false")
           << ",\"boot_marker\":" << (boot_marker ? "true" : "false")
           << ",\"lua_marker\":" << (lua_marker ? "true" : "false")
           << ",\"seconds\":" << execution_seconds << ",\"trap\":" << unsigned(core.trap)
           << ",\"trap_cause\":" << unsigned(core.trap_cause) << ",\"trap_pc\":" << core.trap_pc
           << ",\"trap_value\":" << core.trap_value << ",\"ram_fnv64\":\""
           << std::hex << std::setw(16) << std::setfill('0') << ram_hash
           << "\",\"guest_ram_fnv64\":\"" << std::setw(16) << guest_hash << "\"" << std::dec
           << ",\"jit_native_steps\":" << wide(ram, CONFIG+0x58)
           << ",\"jit_invocations\":" << wide(ram, CONFIG+0x60)
           << ",\"jit_compiled_blocks\":" << wide(ram, CONFIG+0x68)
           << ",\"jit_user_steps\":" << wide(ram, CONFIG+0x70)
           << ",\"jit_invalidations\":" << wide(ram, CONFIG+0xa8)
           << ",\"retired_native\":" << retired
           << ",\"chip_extensions\":" << word(ram, CONFIG+0xc0)
           << ",\"xalu_ops\":" << xalu_ops
           << ",\"tlb_lookups\":" << tlb_lookups
           << ",\"tlb_hits\":" << tlb_hits
           << ",\"tlb_fills\":" << tlb_fills
           << ",\"xld_ops\":" << xld_ops
           << ",\"xld_hits\":" << xld_hits
           << ",\"xea_ops\":" << xea_ops
           << ",\"xea_hits\":" << xea_hits
           << ",\"x86_completed\":" << wide(ram, CONFIG+0xf0)
           << ",\"x86_fault_attempts\":" << wide(ram, CONFIG+0xf8)
           << ",\"x86_repeat_chunks\":" << wide(ram, CONFIG+0x100)
           << ",\"x86_halted_polls\":" << wide(ram, CONFIG+0x108)
           << ",\"tsc\":" << wide(ram, CONFIG+0xb0)
           << ",\"eflags\":" << word(ram, CONFIG+0xa0) << ",\"x86_regs\":[";
    for (unsigned n = 0; n < 8; ++n) { if (n) report << ','; report << word(ram, CONFIG+0x80+n*4); }
    report << "]}\n";
    std::cerr << "RTL stopped: cycles=" << cycles << " x86=" << steps << " status=" << status
              << " trap=" << unsigned(core.trap) << " cause=" << unsigned(core.trap_cause) << '\n';
    core.final();
    return ((core.trap && core.trap_cause != 3) || (status >= 5 && status <= 7)) ? 1 : 0;
} catch (const std::exception& e) {
    std::cerr << e.what() << '\n'; return 1;
}
