# Macro export and analog-on-top assembly (A4 part 2): plan

Spec: `docs/superpowers/specs/2026-10-11-macro-export-design.md`. Branch `feat/macro-export`. Each task is test-first and ends with fmt, clippy and the tests of the crates it touches, then a commit. Rust builds and tests run on the offload server (`~/offload/aispice-macroexport`), never on this machine; LibreLane runs here, one job at a time.

## Tasks

1. **Edge pins** (`aispice-layout/src/generator`): `Params::pin_edge`; Metal4 pin columns at 1.2 um on the block grid, Metal3 track extensions with Via3, labels and pin shapes at the bottom edge. Unit tests: pins sit on the grid at the bottom edge, no plan spacing errors, default off leaves plans unchanged. Gated: OTA and comparator with `pin_edge` DRC and LVS clean.
2. **Package writers** (`aispice-layout/src/macro_export`): name rules (pin identifiers, corner names), pin inference from the netlist, liberty, Verilog (blackbox, wreal), SPICE with `*.PININFO`, spec table, manifest. Unit tests read each view back with `macropkg`'s parsers.
3. **LEF with Magic** (`magic/lef.tcl`, `macro_export::lef`): `lef write -hide`, read back, size and pins checked. Gated test on a generated block.
4. **Export tool** (`aispice-tools`): `macro_export` (Edit): rebuild, DRC, LVS, extraction, characterization at corners pre and post layout, views, evidence, `check_package`, one `write_files`. CLI `aispice macro export`. Gated acceptance: OTA and comparator packages pass `check_macro`; with `CHIPLEAP_MACRO_CHECK` (or `chipleap` in a private venv) they pass ChipLeap's validator too. Security: class, approval, decline, replacing only aispice packages.
5. **LEF geometry**: pin port rectangles and obstructions from `macropkg::lef` (no change to validation).
6. **Chip spec and IO library** (`aispice-layout/src/chip`): `chip.toml` parsing with limits, the `sg13g2_io` cells from the PDK's LEF, package loading through `check_package`, SPICE reduction. Unit tests with the shared fixtures and synthetic LEF.
7. **Floorplan**: die size, pad ring (orientations, corners, fillers), bond pads, seal ring box, macro placement on the grid, supply straps. Unit tests: exact side fill, corner abutment, pins inside the core.
8. **Router**: grid, terminals and access stubs, A* tree growth, retries, geometry output, a spacing and connectivity checker used by the tests. Property tests on random obstacles and terminals.
9. **Chip netlist**: macro instances in port order, pad nets as ports, standard-cell CDL, abstract macros.
10. **Assembly scripts**: `klayout/chip.py` (cells, instances, pcells, routes, labels, two outputs), `magic/fill.tcl`, `klayout/merge.py`; runners. Gated: the demo chip DRC (all decks) and LVS clean.
11. **Chip tool and CLI**: `chip` (Edit) with `plan` and `assemble`, results through `write_files`; `aispice chip assemble`. Security tests (packages outside the project, linked packages, bad names, a failing package refused).
12. **Hardened counter**: `tests/support/harden_counter.py` (LibreLane run and packaging); run once here; gated chip test with `AISPICE_DIGITAL_MACRO`.
13. **Docs**: README, ARCHITECTURE (tools, security, tests), TOOLS.md regenerated, CHANGELOG, SETUP notes.
14. **Checks, PR, CI green.**
