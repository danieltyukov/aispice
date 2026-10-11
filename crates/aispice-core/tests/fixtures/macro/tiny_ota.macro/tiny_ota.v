// Blackbox model of tiny_ota for digital-on-top integration. Synthetic test fixture.
(* blackbox *)
module tiny_ota (
`ifdef USE_POWER_PINS
    inout  vdd,
    inout  vss,
`endif
    input  inp,
    input  inn,
    input  ibias,
    output out
);
endmodule
