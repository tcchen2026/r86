`timescale 1ns/1ps
module rf_probe_tb;
    parameter ADDR_BITS = 5;
    parameter SYNC_READ = 1;
    parameter BLOCK_RAM = 0;
    localparam DEPTH = 1 << ADDR_BITS;
    reg clk = 0;
    always #5 clk = ~clk;
    reg we = 0;
    reg [ADDR_BITS-1:0] wa = 0, ra = 0, rb = 0;
    reg [31:0] wd = 0;
    wire [31:0] qa, qb;
    reg [31:0] expected [0:DEPTH-1];
    reg [31:0] rng = 32'hbabe8632;
    reg initialized = 0;
    integer checks = 0;
    integer i, j;

`ifdef POST_MAP
    r86_rf dut (.*);
`else
    r86_rf #(.ADDR_BITS(ADDR_BITS), .SYNC_READ(SYNC_READ),
             .BLOCK_RAM(BLOCK_RAM)) dut (.*);
`endif

    task step;
        input write_enable;
        input [ADDR_BITS-1:0] write_addr;
        input [31:0] write_data;
        input [ADDR_BITS-1:0] read_a, read_b;
        reg [31:0] previous_a, previous_b;
        begin
            @(negedge clk);
            previous_a = qa;
            previous_b = qb;
            we = write_enable;
            wa = write_addr;
            wd = write_data;
            ra = read_a;
            rb = read_b;
            #1;
            if (initialized) begin
                if (SYNC_READ && (qa !== previous_a || qb !== previous_b))
                    $fatal(1, "Synchronous read changed before clock edge");
                if (!SYNC_READ && (qa !== expected[read_a] || qb !== expected[read_b]))
                    $fatal(1, "Asynchronous read did not follow address before write edge");
            end
            @(posedge clk);
            if (write_enable && write_addr[4:0] != 0)
                expected[write_addr] = write_data;
            #1;
            if (qa !== expected[read_a] || qb !== expected[read_b])
                $fatal(1, "RF mismatch check=%0d a=%0d b=%0d got=%h/%h expected=%h/%h",
                       checks, read_a, read_b, qa, qb,
                       expected[read_a], expected[read_b]);
            checks = checks + 1;
        end
    endtask

    initial begin
        for (i = 0; i < DEPTH; i = i + 1)
            expected[i] = 0;
        // Initialize through the write interface and test same-edge forwarding.
        for (i = 0; i < DEPTH; i = i + 1)
            step(1, i, 32'h12340000 ^ (i * 32'h9e3779b9), i, i);
        initialized = 1;
        // Independently exercise every pair of addresses, including both x0s.
        for (i = 0; i < DEPTH; i = i + 1)
            for (j = 0; j < DEPTH; j = j + 1)
                step(0, 0, 0, i, j);
        if (ADDR_BITS == 6) begin
            // A late write tagged for bank 0 must not hit bank 1, even when
            // the read register names match. Also exercise cross-bank reads.
            for (i = 0; i < 32; i = i + 1) begin
                step(1, i, 32'hface0000 + i, i + 32, i);
                step(1, i + 32, 32'hfeed0000 + i, i, i + 32);
            end
        end
        for (i = 0; i < 2048; i = i + 1) begin
            rng = rng ^ (rng << 13);
            rng = rng ^ (rng >> 17);
            rng = rng ^ (rng << 5);
            step(rng[31], rng[ADDR_BITS-1:0], rng,
                 rng[8 +: ADDR_BITS], rng[16 +: ADDR_BITS]);
        end
        // Check all final contents, including writes made while reading elsewhere.
        for (i = 0; i < DEPTH; i = i + 1)
            step(0, 0, 0, i, DEPTH - 1 - i);
        $display("PASS depth=%0d sync=%0d block=%0d checks=%0d",
                 DEPTH, SYNC_READ, BLOCK_RAM, checks);
        $finish;
    end
endmodule
