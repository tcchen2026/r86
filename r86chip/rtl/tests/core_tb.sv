`timescale 1ns/1ps
module core_tb;
    localparam integer BYTES = 131072;
    reg clk = 0;
    always #5 clk = !clk;
    reg reset = 1;
    wire i_valid, d_valid;
    wire [31:0] i_addr, d_addr, d_wdata;
    wire [3:0] d_wstrb;
    reg i_ready = 0, d_ready = 0;
    reg [31:0] i_data = 0, d_rdata = 0;
    reg i_error = 0, d_error = 0;
    wire trap, retire_valid, retire_bank, retire_write, retire_ea_hit;
    wire [31:0] trap_pc, trap_value, retire_pc, retire_word, retire_value;
    wire [3:0] trap_cause;
    wire [5:0] retire_rd;
    r86_core dut (.*);
    reg [7:0] mem [0:BYTES-1];
    reg [1023:0] image_path;
    integer seed, latency, fault_pc, ram_bytes;
    reg [31:0] rng;
    integer cycles = 0, retires = 0, stores = 0;
    integer pending_redirects = 0, stalled_data = 0;
    integer trapped_cycles = 0;
    reg [67:0] held_trap;
    reg fence_pending = 0, fence_refetched = 0;
    reg [31:0] fence_pc;
    integer iw = -1, dw = -1;
    reg [31:0] held_i, held_d, held_w;
    reg [3:0] held_strb;
    integer n;
    function [31:0] read_word(input [31:0] addr);
        begin
            if (addr >= ram_bytes) read_word = 0;
            else read_word = {mem[addr+3], mem[addr+2], mem[addr+1], mem[addr]};
        end
    endfunction
    initial begin
        if (!$value$plusargs("IMAGE=%s", image_path)) $fatal(1, "IMAGE missing");
        if (!$value$plusargs("SEED=%d", seed)) seed = 1;
        if (!$value$plusargs("LATENCY=%d", latency)) latency = 7;
        if (!$value$plusargs("FAULT_PC=%h", fault_pc)) fault_pc = -1;
        // Word-aligned truncated images let ISS and RTL observe the same native
        // RAM end, including a fetch .U whose second data beat faults.
        if (!$value$plusargs("RAM_BYTES=%d", ram_bytes)) ram_bytes = BYTES;
        if (ram_bytes <= 0 || ram_bytes > BYTES || ram_bytes % 4 != 0)
            $fatal(1, "invalid RAM_BYTES");
        rng = seed;
        for (n = 0; n < BYTES; n = n + 1) mem[n] = 0;
        $readmemh(image_path, mem, 0, ram_bytes-1);
        repeat (4) @(negedge clk);
        reset = 0;
    end
    // Snapshot fetch data when a request starts, so stale prefetched instructions
    // really survive a later data write unless the frontend discards them.
    always @(negedge clk) begin
        if (!reset) begin
            rng = rng ^ (rng << 13);
            rng = rng ^ (rng >> 17);
            rng = rng ^ (rng << 5);
            i_ready = 0;
            d_ready = 0;
            if (i_valid) begin
                if (iw < 0) begin
                    held_i = i_addr;
                    i_data = read_word(i_addr);
                    i_error = i_addr >= ram_bytes || i_addr == fault_pc;
                    iw = latency == 0 ? 0 : integer'(rng % 4);
                end else if (i_addr !== held_i) $fatal(1, "fetch changed while waiting");
                if (iw == 0) i_ready = 1;
                else iw = iw - 1;
            end else if (iw >= 0) $fatal(1, "fetch withdrawn before completion");
            if (d_valid) begin
                if (dw < 0) begin
                    held_d = d_addr; held_w = d_wdata; held_strb = d_wstrb;
                    d_rdata = read_word(d_addr);
                    d_error = d_addr >= ram_bytes;
                    dw = latency == 0 ? 0 : integer'(rng % 8);
                end else if ({d_addr, d_wdata, d_wstrb} !== {held_d, held_w, held_strb})
                    $fatal(1, "data request changed while waiting");
                if (dw == 0) d_ready = 1;
                else begin dw = dw - 1; stalled_data = stalled_data + 1; end
            end else if (dw >= 0) $fatal(1, "data request withdrawn before completion");
        end
    end
    always @(posedge clk) begin
        if (!reset) begin
            cycles = cycles + 1;
            if (i_valid && i_ready) begin
                if (fence_pending && i_addr == fence_pc) fence_refetched = 1;
                iw = -1;
            end
            if (d_valid && d_ready) begin
                dw = -1;
                // Include reads (even to x0) and failed accesses: an extra read
                // can be an unwanted MMIO effect even without a GPR write.
                $display("D %08x %01x %08x", d_addr, d_wstrb,
                         d_wstrb != 0 ? d_wdata : 32'b0);
                if (!d_error && d_wstrb != 0) begin
                    stores = stores + 1;
                    $display("S %08x %01x %08x", d_addr, d_wstrb, d_wdata);
                    for (n = 0; n < 4; n = n + 1)
                        if (d_wstrb[n]) mem[d_addr+n] = d_wdata[n*8 +: 8];
                end
            end
            if (retire_valid) begin
                if (fence_pending) begin
                    if (!fence_refetched || retire_pc != fence_pc)
                        $fatal(1, "FENCE.I used a fetch completed before the fence");
                    fence_pending = 0;
                end
                if (retire_word[6:0] == 7'h0f && retire_word[14:12] == 1) begin
                    fence_pending = 1;
                    fence_refetched = 0;
                    fence_pc = retire_pc + 4;
                end
                if (i_valid && !i_ready &&
                    (retire_word[6:0] == 7'h6f || retire_word[6:0] == 7'h67 ||
                     (retire_word[6:0] == 7'h0f && retire_word[14:12] == 1) ||
                     (retire_word[6:0] == 7'h7b &&
                      (retire_word[14:12] == 4 || retire_word[14:12] == 5))))
                    pending_redirects = pending_redirects + 1;
                if ((^{retire_pc, retire_word, retire_bank, retire_write}) === 1'bx)
                    $fatal(1, "unknown retirement pc=%h word=%h bank=%b write=%b",
                           retire_pc, retire_word, retire_bank, retire_write);
                if (retire_write && (^{retire_rd, retire_value}) === 1'bx)
                    $fatal(1, "unknown register write");
                retires = retires + 1;
                $display("R %08x %08x %01x %01x %02x %08x", retire_pc, retire_word,
                         retire_bank, retire_write, retire_write ? retire_rd : 0,
                         retire_write ? retire_value : 0);
            end
            if (trap) begin
                if (retire_valid || d_valid) $fatal(1, "effect after trap");
                if (trapped_cycles == 0) begin
                    held_trap = {trap_pc, trap_cause, trap_value};
                    $display("T %08x %01x %08x", trap_pc, trap_cause, trap_value);
                end else if ({trap_pc, trap_cause, trap_value} !== held_trap)
                    $fatal(1, "trap state changed");
                trapped_cycles = trapped_cycles + 1;
                if (trapped_cycles == 16) begin
                    if (i_valid) $fatal(1, "fetch not drained after trap");
                    $display("C %0d %0d %0d %0d %0d", cycles, retires, stores, pending_redirects, stalled_data);
                    $finish;
                end
            end
            if (cycles > 2000000) $fatal(1, "timeout");
        end
    end
endmodule
