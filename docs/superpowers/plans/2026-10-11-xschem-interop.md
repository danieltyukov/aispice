# xschem import and export (A2): plan

Spec: `docs/superpowers/specs/2026-10-11-xschem-interop-design.md`. Branch `feat/xschem-interop`. Each task is test first, then code, then `cargo test -p <crate>` for the crate it touches; one commit per task.

1. **Compare options.** `netlist::compare::{CompareOptions, compare_with}`: ground aliases, xschem automatic names, parametric values with SPICE defaults, subcircuit bodies. `compare` unchanged. Tests beside the module.
2. **File and properties.** `xschem::file` (records, `{}` strings, embedded symbols, limits, writer) and `xschem::props` (get, tokens, quoting). Round-trip and malformed-input tests.
3. **Tcl subset.** `xschem::tcl`: words, substitution, `expr` and `ev` with `%.4g`, `if`, `return`, `sim_is_*`. Tests for IHP's and xschem's real constructs, and for refusals (unknown command, depth, size).
4. **Symbols.** `xschem::symbol`: pins from layer-5 boxes, `sim_pinnumber` order, type and format; resolver with xschem's search order; built-in definitions for the generic devices and IHP's devices.
5. **Netlister.** `xschem::netlist`: connectivity and net naming as xschem does it, format expansion (`@name`, `@pinlist`, `@@pin`, `@#n`, `@spiceprefix`, `@symname`, attributes, escapes), `tcleval` lines through the subset, code blocks and their placement, `.GLOBAL`, `device_model`, hierarchy. Fixture schematics with expected netlists.
6. **Safety scan.** `xschem::safety`. Tests with hostile fixtures.
7. **Layout of PDK devices.** `layout::circuit` maps kit subcircuits to `sg13g2\` symbols; `SymbolLibrary::from_defs` for a caller's symbol set. Tests: an inverter netlist draws with transistor symbols and round-trips; board layouts unchanged.
8. **Import.** `xschem::import`: netlist, control blocks, PDK header, policy, renames, layout, findings. Tests on fixtures.
9. **Export.** `xschem::export`: xschem-shaped symbol set, layout, `.sch` writer, per-instance self-check and format override. Tests: netlists and every template export, re-import, and compare equal.
10. **Running xschem.** `aispice_sim::xschem`: find, safe paths, private rc and home, netlist. Unit tests for the rc and path checks; a gated test runs xschem.
11. **Tools and CLI.** `aispice_tools::xschem` (conversion plus proof), `xschem_import`, `xschem_export`, `aispice xschem import|export`. Tool tests through the registry; CLI tests with `assert_cmd`.
12. **Gated tests.** `crates/aispice-tools/tests/xschem_ihp.rs`: built-ins against installed symbols; every `sg13g2_tests` bench imports and both netlist comparisons hold; templates and IHP circuits export and xschem agrees.
13. **Docs.** README, `docs/XSCHEM.md`, `docs/ARCHITECTURE.md`, CHANGELOG, `docs/TOOLS.md` regenerated.
14. **Checks and PR.** `npm ci`, `npm run check`, gated tests, push, PR, CI green.
