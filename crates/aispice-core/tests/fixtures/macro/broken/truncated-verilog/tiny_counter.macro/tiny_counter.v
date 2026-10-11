// Behavioral model of tiny_counter. Synthetic test fixture.
module tiny_counter (
`ifdef USE_POWER_PINS
    inout VDD,
    inout VSS,
`endif
    input clk,
    input rst_n,