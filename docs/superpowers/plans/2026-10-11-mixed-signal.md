# Mixed-signal, part 1: plan

Design: [docs/superpowers/specs/2026-10-11-mixed-signal-design.md](../specs/2026-10-11-mixed-signal-design.md). Branch `feat/mixed-signal`, one pull request.

Status 2026-10-12: steps 1 to 13 done; results under "As built" in the design.

## Security first

1. Netlist policy: refuse `.model` cards of type `d_cosim` and `d_process` in decks and included files, under both readings. Tests beside the existing compiled-model refusals.

## Engine (`aispice-sim`)

2. `behavioral`: source checks for Verilog-A and Verilog (sizes, includes, system tasks, DPI, `$c`, SystemC blocks), module and port extraction, the OpenVAF build and the Verilator build (argument vectors, private folder, timeouts, messages with private paths replaced), tool detection.
3. `SimJob::libraries`: compiled libraries with their SHA-256. The ngspice backend writes them into its private folder, checks the hash, loads OSDI through `.spiceinit` and co-simulation through `d_cosim` model cards it adds to both decks it runs. The other backends refuse a job that has libraries.
4. `dialect`: XSPICE `A` instances pass through for ngspice when their model is an allowlisted code model or a declared co-simulation model.
5. `budget`: the ADC front-end and LDO equations, block spec tables and `.specs` text.
6. `trim`: the per-sample code search result, reduced to coverage, needed codes and headroom.

## Tools (`aispice-tools`) and front ends

7. `aispice.toml`: `veriloga` and `[[cosim]]`.
8. Runner: build on demand, keep built libraries in memory, write the record copy under `.aispice/build/`, attach the libraries a deck uses, choose ngspice for them.
9. Tools `behavioral_models`, `budget` and `trim_check`; titles in the app; `docs/TOOLS.md` regenerated.
10. Settings `openvaf_path` and `verilator_path`; `aispice doctor` lines.

## Demo, tests, docs

11. `examples/mixed_signal/`: Verilog-A comparator, behavioural comparator, Verilog counter, deck, README.
12. Tests: policy, source checks, dialect, backend deck and `.spiceinit`, budget numbers against hand calculation, trim reduction, and the demo end to end on ngspice with OpenVAF and Verilator (skipped when they are missing). CI installs Verilator and OpenVAF on Linux so the demo runs there.
13. README, ARCHITECTURE (modules, security model), SECURITY, CHANGELOG.
14. `npm ci`, `npm run check`, desktop shell clippy; PR; CI green.
