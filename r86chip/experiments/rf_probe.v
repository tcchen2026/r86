// Standalone area experiment, not an implemented CPU register file.
// ADDR_BITS=6 adds a context bit above the unchanged five-bit RV register name.
// Read and write addresses include their own context tags; no third read port.
module r86_rf #(
    parameter ADDR_BITS = 5,
    parameter SYNC_READ = 1,
    parameter BLOCK_RAM = 0
) (
    input wire clk,
    input wire we,
    input wire [ADDR_BITS-1:0] wa,
    input wire [31:0] wd,
    input wire [ADDR_BITS-1:0] ra,
    input wire [ADDR_BITS-1:0] rb,
    output wire [31:0] qa,
    output wire [31:0] qb
);
    // No whole-array reset: boot software initializes any registers it needs.
    // Explicit styles expose the latency/resource tradeoff instead of relying
    // on the mapper's small-memory cost heuristic.
    (* ram_style = BLOCK_RAM ? "block" : "distributed" *)
    reg [31:0] words [0:(1 << ADDR_BITS)-1];
    wire write_live = we && (wa[4:0] != 0);

    always @(posedge clk)
        if (write_live)
            words[wa] <= wd;

    generate if (SYNC_READ) begin: sync_read
        reg [31:0] a, b;
        always @(posedge clk) begin
            a <= ra[4:0] == 0 ? 32'b0 :
                 (write_live && wa == ra ? wd : words[ra]);
            b <= rb[4:0] == 0 ? 32'b0 :
                 (write_live && wa == rb ? wd : words[rb]);
        end
        assign qa = a;
        assign qb = b;
    end else begin: async_read
        assign qa = ra[4:0] == 0 ? 32'b0 : words[ra];
        assign qb = rb[4:0] == 0 ? 32'b0 : words[rb];
    end endgenerate
endmodule
