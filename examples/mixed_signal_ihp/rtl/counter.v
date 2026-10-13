// A 4-bit counter: counts the rising clock edges at which en is high.
module counter(input clk, input en, output reg [3:0] q);
  initial q = 4'd0;
  always @(posedge clk)
    if (en) q <= q + 4'd1;
endmodule
