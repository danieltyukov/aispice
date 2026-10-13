# Mixed signal on IHP SG13G2: the StrongARM comparator drives a counter

`strongarm_counter.cir` is the `comparator` template's StrongARM latch at a 500 MHz clock, with its input 0.5 mV above or below the 0.7 V reference in each of eight cycles. Its decisions, bridged to digital and inverted, clock and enable the co-simulated counter from `rtl/counter.v`, which counts the five cycles where the input was above and reads 5 after the last one.

Needs Linux with bubblewrap or Landlock ABI 4 or later, ngspice 42 or newer, Verilator, a C and C++ compiler and make, as [the other mixed-signal example](../mixed_signal/README.md) does (OpenVAF only for the PDK's own models). Register the PDK under the name `aispice.toml` uses, then simulate:

```sh
aispice pdk add ihp-sg13g2 ~/IHP-Open-PDK/ihp-sg13g2
aispice sim strongarm_counter.cir -m "q0 = value_at(V(vq0), 16.4n)" -m "q2 = value_at(V(vq2), 16.4n)"
```
