// Synchronous 2R1W technology boundary. Storage is intentionally not reset.
module r86_regfile #(
    parameter integer BANKS = 2,
    parameter RAM_STYLE = "block"
) (
    input  wire        clk,
    input  wire        read_enable,
    input  wire [5:0]  read_a,
    input  wire [5:0]  read_b,
    output reg  [31:0] value_a,
    output reg  [31:0] value_b,
    input  wire        write_enable,
    input  wire [5:0]  write_addr,
    input  wire [31:0] write_value
);
    localparam integer ADDR_BITS = BANKS == 2 ? 6 : 5;
    (* ram_style = RAM_STYLE *) reg [31:0] words [0:32*BANKS-1];
    always @(posedge clk) begin
        if (write_enable && write_addr[4:0] != 0)
            words[write_addr[ADDR_BITS-1:0]] <= write_value;
        if (read_enable) begin
            if (read_a[4:0] == 0) value_a <= 0;
            else if (write_enable && write_addr == read_a) value_a <= write_value;
            else value_a <= words[read_a[ADDR_BITS-1:0]];
            if (read_b[4:0] == 0) value_b <= 0;
            else if (write_enable && write_addr == read_b) value_b <= write_value;
            else value_b <= words[read_b[ADDR_BITS-1:0]];
        end
    end
endmodule
