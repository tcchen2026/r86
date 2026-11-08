`timescale 1ns/1ps
module regfile_tb;
    parameter integer BANKS = 2;
    localparam integer DEPTH = BANKS * 32;
    reg clk = 0;
    always #5 clk = !clk;
    reg read_enable = 0, write_enable = 0;
    reg [5:0] read_a = 0, read_b = 0, write_addr = 0;
    reg [31:0] write_value = 0;
    wire [31:0] value_a, value_b;
    r86_regfile #(.BANKS(BANKS)) dut (.*);
    reg [31:0] expected [0:DEPTH-1];
    reg [31:0] previous_a, previous_b;
    reg initialized = 0;
    reg [31:0] rng = 32'h8632feed;
    integer checks = 0, holds = 0;
    integer i, j;

    task step(input re, input we, input [5:0] wa, ra, rb, input [31:0] wd);
        begin
            @(negedge clk);
            previous_a = value_a; previous_b = value_b;
            read_enable = re; write_enable = we; write_addr = wa;
            read_a = ra; read_b = rb; write_value = wd;
            #1;
            if (initialized && {value_a, value_b} !== {previous_a, previous_b})
                $fatal(1, "RF changed before clock edge");
            @(posedge clk);
            if (we && wa[4:0] != 0) expected[wa] = wd;
            #1;
            if (re) begin
                if (value_a !== expected[ra] || value_b !== expected[rb])
                    $fatal(1, "RF read/collision/bank mismatch at check %0d", checks);
            end else begin
                if ({value_a, value_b} !== {previous_a, previous_b})
                    $fatal(1, "RF stall did not hold operands");
                holds = holds + 1;
            end
            checks = checks + 1;
        end
    endtask

    initial begin
        for (i = 0; i < DEPTH; i = i + 1) expected[i] = 0;
        // Initializes only through the write port and requires same-edge bypass.
        for (i = 0; i < DEPTH; i = i + 1)
            step(1, 1, 6'(i), 6'(i), 6'(i), 32'h12340000 ^ (i * 32'h9e3779b9));
        initialized = 1;
        for (i = 0; i < DEPTH; i = i + 1)
            for (j = 0; j < DEPTH; j = j + 1)
                step(1, 0, 0, 6'(i), 6'(j), 0);
        if (BANKS == 2)
            for (i = 0; i < 32; i = i + 1) begin
                step(1, 1, 6'(i), 6'(i+32), 6'(i), 32'hface0000 + i);
                step(1, 1, 6'(i+32), 6'(i), 6'(i+32), 32'hfeed0000 + i);
            end
        for (i = 0; i < 2048; i = i + 1) begin
            rng = rng ^ (rng << 13); rng = rng ^ (rng >> 17); rng = rng ^ (rng << 5);
            step(rng[30], rng[31], 6'(rng[5:0] % DEPTH),
                 6'(rng[13:8] % DEPTH), 6'(rng[21:16] % DEPTH), rng);
        end
        for (i = 0; i < DEPTH; i = i + 1)
            step(1, 0, 0, 6'(i), 6'(DEPTH-1-i), 0);
        if (holds == 0) $fatal(1, "no hold checks");
        $display("PASS banks=%0d checks=%0d held=%0d", BANKS, checks, holds);
        $finish;
    end
endmodule
