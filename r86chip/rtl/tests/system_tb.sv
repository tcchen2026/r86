`timescale 1ns/1ps
// Core plus caches against a slow burst RAM on the RamPort. Prints the same
// R/T/C lines as core_tb, then writes dirty lines back (sync) and prints the
// FNV-64 of the RAM model as "M". +LATENCY is the clocks before a request is
// accepted; +GAPS=1 also inserts idle clocks between beats.
module system_tb;
    localparam integer BYTES = 131072;
    reg clock = 0;
    always #5 clock = !clock;
    reg reset = 1;
    wire io_ram_req_valid, io_ram_req_bits_write, io_ram_w_valid;
    wire [31:0] io_ram_req_bits_address, io_ram_w_bits_data;
    wire [7:0] io_ram_req_bits_beats;
    wire [3:0] io_ram_w_bits_strobes;
    reg io_ram_req_ready = 0, io_ram_w_ready = 0;
    reg io_ram_rsp_valid = 0, io_ram_rsp_bits_last = 0, io_ram_rsp_bits_error = 0;
    reg [31:0] io_ram_rsp_bits_data = 0;
    wire io_dev_valid;
    wire [31:0] io_dev_addr, io_dev_wdata;
    wire [3:0] io_dev_wstrb;
    // No devices in these fixtures: an access answers at once with an error.
    wire io_dev_ready = io_dev_valid;
    wire [31:0] io_dev_rdata = 0;
    wire io_dev_error = 1;
    reg io_sync = 0;
    wire io_synced, io_trap, io_retire_valid, io_retire_bank, io_retire_write;
    wire [31:0] io_trap_pc, io_trap_value, io_retire_pc, io_retire_word, io_retire_value;
    wire [3:0] io_trap_cause;
    wire [5:0] io_retire_rd;
    wire [15:0] io_events;
    r86_system dut (.*);
    reg [7:0] mem [0:BYTES-1];
    reg [1023:0] image_path;
    integer seed, latency, gaps, ram_bytes;
    reg [31:0] rng;
    integer cycles = 0, retires = 0, trapped = 0, trap_cycle = 0, requests = 0;
    integer n;
    // RAM model state: 0 idle, 1 delaying, 2 read beats, 3 write beats, 4 write response.
    integer phase = 0, wait_left = 0, beats_left = 0;
    reg [31:0] address;
    reg write, bad;
    reg [63:0] hash;
    initial begin
        if (!$value$plusargs("IMAGE=%s", image_path)) $fatal(1, "IMAGE missing");
        if (!$value$plusargs("SEED=%d", seed)) seed = 1;
        if (!$value$plusargs("LATENCY=%d", latency)) latency = 8;
        if (!$value$plusargs("GAPS=%d", gaps)) gaps = 0;
        if (!$value$plusargs("RAM_BYTES=%d", ram_bytes)) ram_bytes = BYTES;
        rng = seed;
        for (n = 0; n < BYTES; n = n + 1) mem[n] = 0;
        $readmemh(image_path, mem, 0, ram_bytes-1);
        repeat (4) @(negedge clock);
        reset = 0;
    end
    always @(posedge clock) begin
        if (!reset) begin
            cycles = cycles + 1;
            rng = rng ^ (rng << 13); rng = rng ^ (rng >> 17); rng = rng ^ (rng << 5);
            io_ram_req_ready <= 0;
            io_ram_w_ready <= 0;
            io_ram_rsp_valid <= 0;
            case (phase)
            0: if (io_ram_req_valid) begin
                wait_left = latency;
                phase = 1;
            end
            1: if (wait_left > 0) wait_left = wait_left - 1;
               else if (!io_ram_req_ready) io_ram_req_ready <= 1;
               else begin
                   // Accepted on this edge.
                   address = io_ram_req_bits_address;
                   write = io_ram_req_bits_write;
                   beats_left = io_ram_req_bits_beats + 1;
                   bad = 0;
                   requests = requests + 1;
                   if (address[1:0] != 0) $fatal(1, "unaligned RAM request");
                   phase = write ? 3 : 2;
               end
            2: if (!(gaps != 0 && rng[1:0] == 0)) begin
                io_ram_rsp_valid <= 1;
                io_ram_rsp_bits_error <= address >= ram_bytes;
                io_ram_rsp_bits_data <= address >= ram_bytes ? 32'hdeadbeef :
                    {mem[address+3], mem[address+2], mem[address+1], mem[address]};
                io_ram_rsp_bits_last <= beats_left == 1;
                address = address + 4;
                beats_left = beats_left - 1;
                if (beats_left == 0) phase = 0;
            end
            3: begin
                if (io_ram_w_valid && io_ram_w_ready) begin
                    if (address >= ram_bytes) bad = 1;
                    else for (n = 0; n < 4; n = n + 1)
                        if (io_ram_w_bits_strobes[n]) mem[address+n] = io_ram_w_bits_data[n*8 +: 8];
                    address = address + 4;
                    beats_left = beats_left - 1;
                end
                if (beats_left == 0) phase = 4;
                else io_ram_w_ready <= !(gaps != 0 && rng[1:0] == 0);
            end
            4: begin
                io_ram_rsp_valid <= 1;
                io_ram_rsp_bits_error <= bad;
                io_ram_rsp_bits_last <= 1;
                phase = 0;
            end
            endcase
            if (io_retire_valid) begin
                if (io_trap) $fatal(1, "retire after trap");
                retires = retires + 1;
                $display("R %08x %08x %01x %01x %02x %08x", io_retire_pc, io_retire_word,
                         io_retire_bank, io_retire_write, io_retire_write ? io_retire_rd : 0,
                         io_retire_write ? io_retire_value : 0);
            end
            if (io_trap) begin
                if (trapped == 0) begin
                    $display("T %08x %01x %08x", io_trap_pc, io_trap_cause, io_trap_value);
                    trap_cycle = cycles;
                end
                trapped = trapped + 1;
                if (trapped == 40) io_sync <= 1;
            end
            if (io_synced) begin
                hash = 64'hcbf29ce484222325;
                for (n = 0; n < ram_bytes; n = n + 1) hash = (hash ^ {56'b0, mem[n]}) * 64'h100000001b3;
                $display("M %016x", hash);
                $display("C %0d %0d %0d", trap_cycle, retires, requests);
                $finish;
            end
            if (cycles > 4000000) $fatal(1, "timeout");
        end
    end
endmodule
