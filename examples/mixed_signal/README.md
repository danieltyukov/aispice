# Mixed signal: a Verilog-A comparator drives a Verilog counter

`ramp_counter.cir` ramps `inp` from 0 to 1 V over 10 us against 0.5 V. The comparator (`models/comparator.va`, compiled by OpenVAF) switches at 5 us; the counter (`rtl/counter.v`, co-simulated through ngspice's `d_cosim`) counts the 1 MHz clock edges while the comparator's output is high and reads 5 at the end. `aispice.toml` declares both models; the deck uses them by name and never names a compiled file.

```sh
aispice sim ramp_counter.cir -m "q0 = value_at(V(vq0), 9.9u)" -m "q2 = value_at(V(vq2), 9.9u)"
```

Needs Linux with bubblewrap or Landlock ABI 4 or later (Linux 6.7), ngspice 42 or newer, OpenVAF, Verilator, a C and C++ compiler and make (`aispice doctor` lists them). aispice runs the compiled models only in a sandboxed ngspice.
