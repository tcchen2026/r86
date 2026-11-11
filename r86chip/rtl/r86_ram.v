// Memory technology boundaries for the guest tables. Storage is not reset.

// Synchronous-read 1R1W block RAM. The core never uses data read from a row
// written on the same edge, so the collision result is left to the technology.
module r86_blockram #(
    parameter integer ADDR = 10,
    parameter integer WIDTH = 40
) (
    input  wire             clk,
    input  wire [ADDR-1:0]  read_addr,
    output reg  [WIDTH-1:0] read_data,
    input  wire             write_enable,
    input  wire [ADDR-1:0]  write_addr,
    input  wire [WIDTH-1:0] write_data
);
    (* ram_style = "block", no_rw_check *) reg [WIDTH-1:0] words [0:(1<<ADDR)-1];
    always @(posedge clk) begin
        if (write_enable) words[write_addr] <= write_data;
        read_data <= words[read_addr];
    end
endmodule

// Asynchronous-read 1R1W distributed RAM.
module r86_lutram #(
    parameter integer ADDR = 6,
    parameter integer WIDTH = 36
) (
    input  wire             clk,
    input  wire [ADDR-1:0]  read_addr,
    output wire [WIDTH-1:0] read_data,
    input  wire             write_enable,
    input  wire [ADDR-1:0]  write_addr,
    input  wire [WIDTH-1:0] write_data
);
    (* ram_style = "distributed" *) reg [WIDTH-1:0] words [0:(1<<ADDR)-1];
    always @(posedge clk)
        if (write_enable) words[write_addr] <= write_data;
    assign read_data = words[read_addr];
endmodule
