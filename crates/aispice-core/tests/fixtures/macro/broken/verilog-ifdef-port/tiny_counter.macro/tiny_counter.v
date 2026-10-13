// Behavioral model of tiny_counter. Synthetic test fixture.
module tiny_counter (
`ifdef USE_POWER_PINS
    inout VDD,
    inout VSS,
`endif
    input clk,
    input rst_n,
`ifdef SYNTHESIS
    output en,
`else
    input en,
`endif
    output reg [1:0] q
);
    // A comment with input and output words in it does not declare anything.
    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) q <= 2'b00;
        else if (en) q <= q + 2'b01;
    end
endmodule
