# Analog layout, verification and post-layout (A3): plan

Spec: `docs/superpowers/specs/2026-10-11-analog-layout-design.md`. Branch `feat/analog-layout`. Each task is test-first and ends with fmt, clippy and the crate's tests, then a commit.

## Tasks

1. **Crate scaffold.** `crates/aispice-layout` with `Tools` (KLayout and Magic paths), the run-folder helper and the argument-vector runner over `aispice_sim::backend::process`. Config fields `klayout_path`, `magic_path`.
2. **GDS reader** (`gds.rs`): records, structures, references, top cells, limits. Test GDS writer for synthetic fixtures.
3. **DRC** (`drc.rs`): switches, deck selection, lyrdb parser, layer from rule. Gated test: synthetic Metal1 spacing violation.
4. **LVS** (`lvs.rs`): CDL netlist writer from an aispice netlist, ports, the report script (`klayout/lvs_report.py`), runner, report types. Gated test on a generated cell.
5. **Render** (`render.rs`, `klayout/render.py`).
6. **Generators** (`gen/`): rule table, plan types, the drawing script (`klayout/draw.py`), MOS unit with gate strap and buses, guard rings, router, `current_mirror`, `diff_pair`, `ota5t`. Gated DRC and LVS tests for each; removed-contact and spacing-violation tests.
7. **Extraction** (`extract.rs`, `magic/extract.tcl`): Magic run, name cleanup, flat subcircuit; post-layout netlist assembly. Gated test on the generated OTA.
8. **Tools** (`aispice-tools`): `drc`, `lvs`, `extract`, `layout` (generate, render), `check_specs` `post_layout`; hints for the agent loop; system prompt line.
9. **CLI**: `aispice drc`, `aispice lvs`, `aispice extract`, `aispice layout`.
10. **Acceptance test**: 5T OTA generated, DRC and LVS clean, extracted, GBW and phase margin before and after layout.
11. **CI**: KLayout in the PDK job, DRC plumbing test.
12. **Docs**: README, SETUP (KLayout, Magic, pcell submodules), ARCHITECTURE, TOOLS.md, CHANGELOG.
13. **Checks, PR, CI green.**

## Local tool setup

The PDK's `pycell4klayout-api` and `pypreprocessor` submodules were checked out in the local IHP-Open-PDK (needed by the pcells). KLayout 0.30.4 and Magic 8.3.677 are already installed.
